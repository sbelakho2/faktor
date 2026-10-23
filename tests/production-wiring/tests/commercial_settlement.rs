//! Cross-subsystem commercial settlement scenarios through PRODUCTION
//! wiring: a real `SessionManager` (session store + reservation ledger), the
//! real cloud `EntitlementService` over a SQLite control-plane store, and the
//! real `BillingSpendProjector` the daemon's admission gate uses.
//!
//! No test here calls `/native/usage`: the quota must observe a settlement
//! because admission drains the durable outbox, not because a UI route ran.

use std::path::Path;
use std::sync::Arc;

use faktor_cloud::{
    AdmissionBoundary, AdmissionRequest, BillingAccountId, BillingConfig, EntitlementService,
    ManualClock, ObservedUsage, OrganizationId, PlanConfig, SqliteControlPlaneStore, Subscription,
    SubscriptionId, SubscriptionStatus, FEATURE_MANAGED_PROVIDERS,
    LIMIT_MAX_MANAGED_SPEND_MICRO_PER_PERIOD, LIMIT_MAX_TOKENS_PER_PERIOD,
};
use faktor_core::id::{OpId, SessionId, TaskId, TaskRevision};
use faktor_core::state::TaskState;
use faktor_server::BillingSpendProjector;
use faktor_session::{ReservationId, SessionManager};
use faktor_store::{CostReserveOutcome, Store, TaskRow};

const T0: i64 = 1_700_000_000_000;
const PERIOD_MS: i64 = 86_400_000;

struct Bridge(Arc<ManualClock>);

impl faktor_core::Clock for Bridge {
    fn now_ms(&self) -> i64 {
        use faktor_cloud::Clock as _;
        self.0.now_ms()
    }
}

struct Env {
    session: Arc<SessionManager>,
    service: Arc<EntitlementService>,
    clock: Arc<ManualClock>,
    organization: OrganizationId,
}

fn plan(limit_tokens: u64, limit_spend: u64) -> BillingConfig {
    let mut config = BillingConfig::default();
    config.plans.insert(
        "monthly".into(),
        PlanConfig {
            period_ms: Some(PERIOD_MS as u64),
            plan_id: "monthly".into(),
            features: [FEATURE_MANAGED_PROVIDERS.to_string()]
                .into_iter()
                .collect(),
            limits: [
                (LIMIT_MAX_TOKENS_PER_PERIOD.to_string(), limit_tokens),
                (
                    LIMIT_MAX_MANAGED_SPEND_MICRO_PER_PERIOD.to_string(),
                    limit_spend,
                ),
            ]
            .into_iter()
            .collect(),
        },
    );
    config.default_plan = Some("monthly".into());
    config.managed_providers.insert("managed".into());
    config
}

fn env(dir: &Path, limit_tokens: u64, limit_spend: u64) -> Env {
    let clock = Arc::new(ManualClock::new(T0));
    let session = SessionManager::open_with_clock(
        dir.join("store"),
        dir.join("cas"),
        true,
        Arc::new(Bridge(clock.clone())),
    )
    .unwrap();
    let store = Arc::new(SqliteControlPlaneStore::open(&dir.join("billing.db")).unwrap());
    let organization = OrganizationId::try_new("org_settle").unwrap();
    let service = EntitlementService::new_for_organization(
        store,
        clock.clone(),
        plan(limit_tokens, limit_spend),
        Some(organization.clone()),
    )
    .unwrap();
    service
        .ensure_account(
            &organization,
            &BillingAccountId::try_new("acct_settle").unwrap(),
            "primary",
            true,
        )
        .unwrap();
    service
        .set_subscription(&Subscription {
            id: SubscriptionId::try_new("sub_settle").unwrap(),
            organization: organization.clone(),
            plan_id: "monthly".into(),
            status: SubscriptionStatus::Active,
            started_ms: T0,
            expires_ms: None,
            updated_ms: T0,
        })
        .unwrap();
    Env {
        session,
        service,
        clock,
        organization,
    }
}

fn seed_task(store: &Store, session_id: SessionId, task_id: u64) -> TaskId {
    let task = TaskId::new(task_id);
    store
        .upsert_task(&TaskRow {
            task_id: task,
            session_id,
            goal: "g".into(),
            acceptance_criteria: vec![],
            plan: vec![],
            attachments: vec![],
            contract: Default::default(),
            max_tokens: None,
            max_turns: None,
            spent_tokens: 0,
            spent_turns: 0,
            state: TaskState::Running,
            revision: TaskRevision::new(1),
            created_ms: 1,
            updated_ms: 1,
        })
        .unwrap();
    task
}

/// Settle one reservation directly through the store's settlement
/// transaction (the same transaction the budget ledger uses).
fn settle(
    store: &Store,
    session: SessionId,
    task: TaskId,
    op: u64,
    tokens: u64,
    cost_micro: u64,
    at_ms: i64,
) -> ReservationId {
    let CostReserveOutcome::Granted(reservation) = store
        .cost_reserve_priced(
            session,
            task,
            OpId::new(op),
            cost_micro,
            at_ms,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .unwrap()
    else {
        panic!("reservation refused")
    };
    store.cost_mark_dispatched(reservation, at_ms).unwrap();
    store
        .cost_settle_usage_with_context(
            reservation,
            tokens,
            0,
            0,
            0,
            Some(cost_micro),
            None,
            at_ms,
            "managed",
            "model",
        )
        .unwrap();
    ReservationId::new(reservation)
}

fn projector(env: &Env) -> BillingSpendProjector {
    BillingSpendProjector::new(
        env.service.clone(),
        env.organization.clone(),
        env.session.clone(),
    )
}

fn task_admission() -> AdmissionRequest {
    AdmissionRequest {
        boundary: AdmissionBoundary::NewTask,
        task_id: Some(1),
        provider: Some("managed".into()),
        observed: ObservedUsage::default(),
    }
}

#[test]
fn settle_60k_tokens_is_visible_to_the_next_admission_and_resets_at_rollover() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(dir.path(), 50_000, 10_000_000);
    let ws = env.session.create_workspace("/w").unwrap();
    let session = env.session.create_session(ws, "t", "managed", "m").unwrap();
    let task = seed_task(&env.session.store(), session.id(), 1);
    let store = env.session.store();

    // Zero usage: the current period admits.
    assert!(env
        .service
        .check_admission(&env.organization, &task_admission())
        .is_ok());
    settle(&store, session.id(), task, 1, 60_000, 60_000, T0 + 10);

    // No /native/usage call: the admission-grade drain is the only reader.
    let report = projector(&env).drain_for_admission().unwrap();
    assert_eq!(report.rows, 1);
    assert_eq!(report.appended, 2, "one token event + one cost event");
    let denied = env
        .service
        .check_admission(&env.organization, &task_admission())
        .unwrap_err();
    assert_eq!(denied.limit, LIMIT_MAX_TOKENS_PER_PERIOD);
    assert_eq!(denied.observed, 60_000);

    // The all-time rollup keeps the fact; the current period resets at the
    // deterministic boundary.
    env.clock.advance(PERIOD_MS);
    let snapshot = env.service.entitlement_snapshot(&env.organization).unwrap();
    assert_eq!(snapshot.period_total_tokens, 0);
    assert_eq!(snapshot.total_tokens, 60_000);
    assert!(env
        .service
        .check_admission(&env.organization, &task_admission())
        .is_ok());
}

#[test]
fn a_task_with_501_reservations_projects_successfully() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(dir.path(), 1_000_000, 100_000_000);
    let ws = env.session.create_workspace("/w").unwrap();
    let session = env.session.create_session(ws, "t", "managed", "m").unwrap();
    let task = seed_task(&env.session.store(), session.id(), 1);
    let store = env.session.store();
    for index in 0..501u64 {
        settle(
            &store,
            session.id(),
            task,
            index + 1,
            1,
            1,
            T0 + index as i64,
        );
    }
    let report = projector(&env).drain_for_admission().unwrap();
    assert_eq!(report.rows, 501);
    assert_eq!(report.appended, 1_002);
    assert!(!report.pending);
    let snapshot = env.service.entitlement_snapshot(&env.organization).unwrap();
    assert_eq!(snapshot.period_total_tokens, 501);
    assert_eq!(snapshot.period_managed_spend_micro, 501);
}

#[test]
fn replayed_outbox_projection_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(dir.path(), 1_000_000, 100_000_000);
    let ws = env.session.create_workspace("/w").unwrap();
    let session = env.session.create_session(ws, "t", "managed", "m").unwrap();
    let task = seed_task(&env.session.store(), session.id(), 1);
    let store = env.session.store();
    let reservation = settle(&store, session.id(), task, 1, 1_000, 1_000, T0 + 1);

    let first = projector(&env).drain_for_admission().unwrap();
    assert_eq!(first.appended, 2);
    // Replay the SAME outbox fact into the ledger (the crash-after-append-
    // before-watermark window): every event is a duplicate, nothing doubles.
    let row = store.billing_outbox_after(0, 1).unwrap().remove(0);
    let replay = faktor_cloud::DurableSpendRow {
        organization_id: env.organization.as_str().to_string(),
        session_id: row.session_id.raw(),
        task_id: row.task_id.raw(),
        reservation_id: row.reservation_id,
        attempt_id: row.attempt_id.clone(),
        provider: row.provider.clone(),
        model: row.model.clone(),
        input_tokens: row.uncached_input_tokens,
        output_tokens: row.output_tokens,
        cache_read_tokens: row.cache_read_tokens,
        cache_write_tokens: row.cache_write_tokens,
        reasoning_tokens: row.reasoning_tokens,
        provider_cost_micro: row.provider_cost_micro,
        provider_reported_micro: row.provider_reported_micro.unwrap_or(0),
        state: row.state.clone(),
        occurred_at_ms: row.occurred_at_ms,
        source_operation: row.source_operation.clone(),
    };
    let account = env
        .service
        .default_account(&env.organization)
        .unwrap()
        .unwrap();
    let replayed = env
        .service
        .ingest_spend_row(
            &env.organization,
            &account,
            &replay,
            faktor_cloud::SpendCategory::Managed,
        )
        .unwrap();
    assert_eq!(replayed.appended, 0, "the replay is idempotent");
    let snapshot = env.service.entitlement_snapshot(&env.organization).unwrap();
    assert_eq!(snapshot.period_total_tokens, 1_000);
    assert_eq!(snapshot.period_managed_spend_micro, 1_000);

    // A duplicated legacy outbox insert (same reservation idempotency key)
    // is a no-op too.
    assert!(!store
        .billing_outbox_record(faktor_store::BillingOutboxFact {
            reservation_id: reservation.raw(),
            session_id: session.id(),
            task_id: task,
            attempt_id: "replay".into(),
            provider: "managed".into(),
            model: "model".into(),
            uncached_input_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: 0,
            provider_cost_micro: 0,
            provider_reported_micro: None,
            occurred_at_ms: T0 + 1,
            source_operation: "legacy_backfill".into(),
        })
        .unwrap());
}

#[test]
fn crash_between_settle_and_projection_resumes_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(dir.path(), 1_000_000, 100_000_000);
    let ws = env.session.create_workspace("/w").unwrap();
    let session = env.session.create_session(ws, "t", "managed", "m").unwrap();
    let task = seed_task(&env.session.store(), session.id(), 1);
    let store = env.session.store();
    settle(&store, session.id(), task, 1, 1_000, 1_000, T0 + 1);
    // Simulate the crash: drop every in-process authority before projecting.
    drop(store);
    drop(env);

    // Restart over the SAME durable directories (session store/outbox and
    // the cloud billing DB/watermark).
    let clock = Arc::new(ManualClock::new(T0));
    let session = SessionManager::open_with_clock(
        dir.path().join("store"),
        dir.path().join("cas"),
        true,
        Arc::new(Bridge(clock.clone())),
    )
    .unwrap();
    let store = Arc::new(SqliteControlPlaneStore::open(&dir.path().join("billing.db")).unwrap());
    let organization = OrganizationId::try_new("org_settle").unwrap();
    let service = EntitlementService::new_for_organization(
        store,
        clock,
        plan(1_000_000, 100_000_000),
        Some(organization.clone()),
    )
    .unwrap();
    let projector =
        BillingSpendProjector::new(service.clone(), organization.clone(), session.clone());
    let first = projector.drain_for_admission().unwrap();
    assert_eq!(first.rows, 1);
    assert_eq!(first.appended, 2);
    let second = projector.drain_for_admission().unwrap();
    assert_eq!(second.rows, 0, "resumed exactly once");
    let snapshot = service.entitlement_snapshot(&organization).unwrap();
    assert_eq!(snapshot.period_total_tokens, 1_000);
}
