//! Independent final-wave billing probe: the quota feed is the durable
//! settlement outbox, not `/native/usage`.
//!
//! Every scenario uses a real `SessionManager` (reservation ledger + outbox),
//! a real SQLite control-plane store, the real `EntitlementService`, and the
//! real admission-grade `BillingSpendProjector`. No generation of the usage
//! route is involved.

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
const TOKEN_LIMIT: u64 = 60_000;

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

fn config() -> BillingConfig {
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
                (LIMIT_MAX_TOKENS_PER_PERIOD.to_string(), TOKEN_LIMIT),
                (
                    LIMIT_MAX_MANAGED_SPEND_MICRO_PER_PERIOD.to_string(),
                    100_000_000,
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

fn env(dir: &Path) -> Env {
    let clock = Arc::new(ManualClock::new(T0));
    let session = SessionManager::open_with_clock(
        dir.join("store"),
        dir.join("cas"),
        true,
        Arc::new(Bridge(clock.clone())),
    )
    .unwrap();
    let store = Arc::new(SqliteControlPlaneStore::open(&dir.join("billing.db")).unwrap());
    let organization = OrganizationId::try_new("org_wave").unwrap();
    let service = EntitlementService::new_for_organization(
        store,
        clock.clone(),
        config(),
        Some(organization.clone()),
    )
    .unwrap();
    service
        .ensure_account(
            &organization,
            &BillingAccountId::try_new("acct_wave").unwrap(),
            "primary",
            true,
        )
        .unwrap();
    service
        .set_subscription(&Subscription {
            id: SubscriptionId::try_new("sub_wave").unwrap(),
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
        panic!("reservation refused");
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
            "probe-model",
        )
        .unwrap();
    ReservationId::new(reservation)
}

fn admission() -> AdmissionRequest {
    AdmissionRequest {
        boundary: AdmissionBoundary::NewTask,
        task_id: Some(1),
        provider: Some("managed".into()),
        observed: ObservedUsage::default(),
    }
}

#[test]
fn a_settlement_is_visible_to_the_next_admission_without_a_usage_read() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(dir.path());
    let ws = env.session.create_workspace("/w").unwrap();
    let session = env.session.create_session(ws, "t", "managed", "m").unwrap();
    let task = seed_task(&env.session.store(), session.id(), 1);

    // Nothing has used the route: the current period admits.
    assert!(env
        .service
        .check_admission(&env.organization, &admission())
        .is_ok());
    settle(
        &env.session.store(),
        session.id(),
        task,
        1,
        TOKEN_LIMIT,
        TOKEN_LIMIT,
        T0 + 10,
    );

    let projector = BillingSpendProjector::new(
        env.service.clone(),
        env.organization.clone(),
        env.session.clone(),
    );
    let report = projector.drain_for_admission().unwrap();
    assert_eq!(report.rows, 1);
    assert_eq!(report.appended, 2);
    assert!(!report.pending);

    let denied = env
        .service
        .check_admission(&env.organization, &admission())
        .unwrap_err();
    assert_eq!(denied.limit, LIMIT_MAX_TOKENS_PER_PERIOD);
    assert_eq!(denied.observed, TOKEN_LIMIT);
}

#[test]
fn a_period_rollover_resets_the_quota_while_all_time_usage_persists() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(dir.path());
    let ws = env.session.create_workspace("/w").unwrap();
    let session = env.session.create_session(ws, "t", "managed", "m").unwrap();
    let task = seed_task(&env.session.store(), session.id(), 1);
    settle(
        &env.session.store(),
        session.id(),
        task,
        1,
        TOKEN_LIMIT,
        TOKEN_LIMIT,
        T0 + 10,
    );
    let projector = BillingSpendProjector::new(
        env.service.clone(),
        env.organization.clone(),
        env.session.clone(),
    );
    projector.drain_for_admission().unwrap();
    let first = env.service.entitlement_snapshot(&env.organization).unwrap();
    assert_eq!(first.period_total_tokens, TOKEN_LIMIT);
    assert_eq!(first.total_tokens, TOKEN_LIMIT);

    // Skip one whole period: the current period has no usage, the all-time
    // fold still has the first settlement.
    env.clock.advance(2 * PERIOD_MS);
    let rolled = env.service.entitlement_snapshot(&env.organization).unwrap();
    assert_eq!(rolled.period_total_tokens, 0);
    assert_eq!(rolled.total_tokens, TOKEN_LIMIT);
    assert!(env
        .service
        .check_admission(&env.organization, &admission())
        .is_ok());

    // New period usage is the only numerator; the all-time fold accumulates.
    settle(
        &env.session.store(),
        session.id(),
        task,
        2,
        7_000,
        7_000,
        T0 + 2 * PERIOD_MS + 5,
    );
    projector.drain_for_admission().unwrap();
    let second = env.service.entitlement_snapshot(&env.organization).unwrap();
    assert_eq!(second.period_total_tokens, 7_000);
    assert_eq!(second.total_tokens, TOKEN_LIMIT + 7_000);
    assert!(env
        .service
        .check_admission(&env.organization, &admission())
        .is_ok());
}

#[test]
fn a_task_with_613_reservations_projects_every_row() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(dir.path());
    let ws = env.session.create_workspace("/w").unwrap();
    let session = env.session.create_session(ws, "t", "managed", "m").unwrap();
    let task = seed_task(&env.session.store(), session.id(), 1);
    let store = env.session.store();
    let reservations = 613u64;
    for index in 0..reservations {
        settle(
            &store,
            session.id(),
            task,
            index + 1,
            2,
            3,
            T0 + index as i64,
        );
    }
    let projector = BillingSpendProjector::new(
        env.service.clone(),
        env.organization.clone(),
        env.session.clone(),
    );
    let report = projector.drain_for_admission().unwrap();
    assert_eq!(report.rows as u64, reservations);
    assert_eq!(report.appended as u64, reservations * 2);
    assert!(!report.pending);
    let snapshot = env.service.entitlement_snapshot(&env.organization).unwrap();
    assert_eq!(snapshot.period_total_tokens, reservations * 2);
    assert_eq!(snapshot.period_managed_spend_micro, reservations * 3);
    assert_eq!(snapshot.total_tokens, reservations * 2);
}
