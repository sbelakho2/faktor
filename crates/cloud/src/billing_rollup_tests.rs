//! Durable billing rollup equivalence and query-plan tests.
//!
//! The v8 rollups are PROJECTIONS of the append-only ledgers. These tests
//! treat the pure folds as the oracle and try to break the projection:
//! out-of-order correction times, correction chains (a correction that names
//! another correction), ties on `occurred_at_ms`, a projection deleted under
//! a live database, settles below/above their hold, refunds, and the
//! historical 100k-event fold bound.

use std::sync::Arc;

use crate::billing::{
    fold_credits, fold_usage, CreditEntry, CreditKind, CreditLedgerError, ReconciliationState,
    SpendCategory, UsageEvent, UsageUnit,
};
use crate::billing_store::{BillingStore, CreditAppend, MemoryBillingStore};
use crate::ids::{BillingAccountId, CreditEntryId, OrganizationId, UsageEventId};
use crate::store::SqliteControlPlaneStore;

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound.max(1)
    }
}

fn org() -> OrganizationId {
    OrganizationId::try_new("org_rollup").unwrap()
}

fn account() -> BillingAccountId {
    BillingAccountId::try_new("acct_rollup").unwrap()
}

fn event(id: usize, task: u64, run: u64, at: i64) -> UsageEvent {
    let units = [
        UsageUnit::InputTokens,
        UsageUnit::OutputTokens,
        UsageUnit::CacheReadTokens,
        UsageUnit::CacheWriteTokens,
        UsageUnit::ReasoningTokens,
        UsageUnit::ProviderCostMicro,
    ];
    let unit = units[id % units.len()];
    let managed = !id.is_multiple_of(3);
    UsageEvent {
        id: UsageEventId::try_new(format!("uev_{id:016x}")).unwrap(),
        organization_id: org(),
        billing_account_id: account(),
        task_id: task,
        run_id: format!("run-{run}"),
        attempt_id: format!("attempt-{id}"),
        provider: if managed { "openai" } else { "ollama" }.into(),
        model: "m".into(),
        unit,
        quantity: if unit.is_tokens() { 1 + id as u64 } else { 1 },
        provider_cost_micro: if unit.is_tokens() { 0 } else { 11 + id as u64 },
        source_operation: "session.cost_reservation.settled".into(),
        occurred_at_ms: 1_000 + at,
        reconciliation_state: ReconciliationState::Reconciled,
        correction_of: None,
        category: if managed {
            SpendCategory::Managed
        } else {
            SpendCategory::Byok
        },
        source_key: format!("reservation:{id}:attempt-{id}:{}", unit.as_str()),
    }
}

fn correction(id: usize, base: &UsageEvent, at: i64, cost: u64) -> UsageEvent {
    let mut event = event(id, base.task_id, 0, at);
    event.unit = UsageUnit::ProviderCostMicro;
    event.quantity = 1;
    event.provider_cost_micro = cost;
    event.category = SpendCategory::Managed;
    event.correction_of = Some(base.id.clone());
    event
}

/// 250 appends with corrections that can name any prior event (including
/// another correction) and random occurred-at values (ties included). After
/// every batch, the projection must equal the pure fold over the same rows.
#[test]
fn usage_rollup_matches_the_pure_fold_for_correction_chains() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("billing.db");
    let store = Arc::new(SqliteControlPlaneStore::open(&path).unwrap());
    let mut rng = Lcg(0x5eed);
    let mut events: Vec<UsageEvent> = Vec::new();
    let mut next_id = 0usize;
    for step in 0..250usize {
        let event = if step > 3 && rng.below(100) < 30 {
            let target = rng.below(events.len() as u64) as usize;
            let base = events[target].clone();
            let at = rng.below(50) as i64;
            let cost = 1 + rng.below(5_000);
            next_id += 1;
            correction(next_id, &base, at, cost)
        } else {
            next_id += 1;
            let task = 1 + rng.below(6);
            let run = rng.below(4);
            let at = rng.below(50) as i64;
            event(next_id, task, run, at)
        };
        assert_eq!(
            store.append_usage_event(&event).unwrap(),
            crate::billing_store::UsageAppend::Appended
        );
        events.push(event);
        if step % 13 == 0 {
            let got = store.usage_fold(&org()).unwrap();
            let want = fold_usage(&org(), &events).unwrap();
            assert_eq!(got, want, "rollup drift at append {step}");
        }
    }
    let got = store.usage_fold(&org()).unwrap();
    let want = fold_usage(&org(), &events).unwrap();
    assert_eq!(got.totals, want.totals);
    assert_eq!(got.per_task, want.per_task);
    assert!(got.totals.corrected_events > 0, "corrections were folded");

    // Drop the projection under a live database: the next read must rebuild
    // it from the ledger rows (the v7-upgrade/restore path).
    {
        let conn = store.lock().unwrap();
        conn.execute("DELETE FROM billing_usage_total", []).unwrap();
        conn.execute("DELETE FROM billing_usage_task", []).unwrap();
    }
    let rebuilt = store.usage_fold(&org()).unwrap();
    assert_eq!(rebuilt, want, "lazy rebuild must reproduce the fold");
    drop(store);

    // And across a restart the projection stays exact.
    let reopened = SqliteControlPlaneStore::open(&path).unwrap();
    assert_eq!(reopened.usage_fold(&org()).unwrap(), want);
}

/// Credit rollup: grants, holds, a below-hold settle, an above-hold settle,
/// a cancelled pending hold and partial refunds. Rebuilt from scratch too.
#[test]
fn credit_rollup_matches_the_pure_fold_and_rebuilds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("billing.db");
    let store = Arc::new(SqliteControlPlaneStore::open(&path).unwrap());
    let mut entries: Vec<CreditEntry> = Vec::new();
    let mut append = |entry: CreditEntry| {
        assert_eq!(
            store.append_credit_entry(&entry).unwrap(),
            CreditAppend::Appended
        );
        entries.push(entry);
    };
    for i in 0..3u64 {
        append(CreditEntry {
            id: CreditEntryId::try_new(format!("crd_grant_{i}")).unwrap(),
            organization: org(),
            billing_account_id: account(),
            kind: CreditKind::Grant,
            amount_micro: 10_000,
            reference: None,
            usage_event_id: None,
            reason: "grant".into(),
            occurred_at_ms: i as i64,
            idempotency_key: None,
        });
    }
    let consume = |id: &str, amount: u64| CreditEntry {
        id: CreditEntryId::try_new(id).unwrap(),
        organization: org(),
        billing_account_id: account(),
        kind: CreditKind::Consume,
        amount_micro: amount,
        reference: None,
        usage_event_id: None,
        reason: "hold".into(),
        occurred_at_ms: 10,
        idempotency_key: None,
    };
    let held_below = CreditEntryId::try_new("crd_held_below").unwrap();
    let held_above = CreditEntryId::try_new("crd_held_above").unwrap();
    let cancelled = CreditEntryId::try_new("crd_cancelled").unwrap();
    append(consume(held_below.as_str(), 1_000));
    append(consume(held_above.as_str(), 2_500));
    append(consume(cancelled.as_str(), 700));
    let settle = |id: &str, reference: &CreditEntryId, actual: u64| CreditEntry {
        id: CreditEntryId::try_new(id).unwrap(),
        organization: org(),
        billing_account_id: account(),
        kind: CreditKind::Settle,
        amount_micro: actual,
        reference: Some(reference.clone()),
        usage_event_id: None,
        reason: "settle".into(),
        occurred_at_ms: 20,
        idempotency_key: None,
    };
    append(settle("crd_settle_low", &held_below, 900));
    append(settle("crd_settle_high", &held_above, 2_600));
    let refund = |id: &str, reference: &CreditEntryId, amount: u64| CreditEntry {
        id: CreditEntryId::try_new(id).unwrap(),
        organization: org(),
        billing_account_id: account(),
        kind: CreditKind::Refund,
        amount_micro: amount,
        reference: Some(reference.clone()),
        usage_event_id: None,
        reason: "refund".into(),
        occurred_at_ms: 30,
        idempotency_key: None,
    };
    append(refund("crd_cancel", &cancelled, 700));
    append(refund("crd_refund_1", &held_below, 100));
    append(refund("crd_refund_2", &held_below, 800));
    let want = fold_credits(&entries).unwrap();
    assert_eq!(store.credit_balance(&org()).unwrap(), want);
    assert_eq!(want.pending_consumes, 0);
    assert_eq!(want.held_micro, 0);

    // A second settle and an over-refund are still refused typed, and the
    // rollup balance is untouched by the refusals.
    assert!(store
        .append_credit_entry(&settle("crd_settle_again", &held_below, 900))
        .is_err());
    assert!(store
        .append_credit_entry(&refund("crd_over_refund", &held_below, 1))
        .is_err());
    assert_eq!(store.credit_balance(&org()).unwrap(), want);

    {
        let conn = store.lock().unwrap();
        conn.execute("DELETE FROM billing_credit_rollup", [])
            .unwrap();
    }
    assert_eq!(
        store.credit_balance(&org()).unwrap(),
        want,
        "lazy credit rebuild must reproduce the fold"
    );
    drop(store);
    let reopened = SqliteControlPlaneStore::open(&path).unwrap();
    assert_eq!(reopened.credit_balance(&org()).unwrap(), want);
}

/// The historical `MAX_FOLD_EVENTS` (100k) refusal was a hard scale ceiling:
/// an organization past it could not be admitted at all. The projection
/// crosses the bound and a correction still reconciles exactly.
#[test]
fn usage_rollup_crosses_the_old_100k_fold_bound() {
    const COUNT: usize = 100_001;
    let store = SqliteControlPlaneStore::open_in_memory().unwrap();
    let mut events = Vec::with_capacity(COUNT);
    for i in 0..COUNT {
        let mut event = event(i + 1, 1 + (i as u64 % 100), 0, i as i64 % 1_000);
        // Keep the payload compact for the 100k-row seed.
        event.attempt_id = format!("a{i}");
        event.source_key = format!("reservation:{i}:a{i}:input_tokens");
        events.push(event);
    }
    {
        let mut conn = store.lock().unwrap();
        let tx = conn.transaction().unwrap();
        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO usage_event
                        (id, organization_id, event_seq, source_key, billing_account_id, task_id,
                         run_id, attempt_id, category, correction_of, occurred_at_ms,
                         reconciliation_state, payload)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10, 'reconciled', ?11)",
                )
                .unwrap();
            for (index, event) in events.iter().enumerate() {
                stmt.execute(rusqlite::params![
                    event.id.as_str(),
                    event.organization_id.as_str(),
                    (index + 1) as i64,
                    event.source_key,
                    event.billing_account_id.as_str(),
                    crate::billing::task_id_text(event.task_id),
                    event.run_id,
                    event.attempt_id,
                    event.category.as_str(),
                    event.occurred_at_ms,
                    serde_json::to_string(event).unwrap(),
                ])
                .unwrap();
            }
        }
        tx.execute(
            "INSERT INTO usage_event_seq (organization_id, next_seq) VALUES (?1, ?2)",
            rusqlite::params![org().as_str(), COUNT as i64],
        )
        .unwrap();
        tx.commit().unwrap();
    }
    let fold = store.usage_fold(&org()).unwrap();
    assert_eq!(fold.totals.events, COUNT as u64);
    let want = fold_usage(&org(), &events).unwrap();
    assert_eq!(fold.totals, want.totals, "100k totals are exact");
    assert_eq!(fold.per_task, want.per_task, "100k buckets are exact");

    // The rollup watermark is caught up: one more append is O(1) and exact.
    let base = events[0].clone();
    let extra = correction(COUNT + 10, &base, 999, 4_242);
    assert_eq!(
        store.append_usage_event(&extra).unwrap(),
        crate::billing_store::UsageAppend::Appended
    );
    events.push(extra);
    let after = store.usage_fold(&org()).unwrap();
    let want = fold_usage(&org(), &events).unwrap();
    assert_eq!(after.totals, want.totals);
    assert_eq!(after.totals.corrected_events, 2);
    assert_eq!(after.totals.events, COUNT as u64);
}

/// The credit projection crosses a 100k-entry ledger (grants + pending
/// holds) and then appends in O(1), exactly: the historical balance read
/// folded and JSON-decoded every entry on every entitlement check.
#[test]
fn credit_rollup_crosses_a_100k_entry_ledger() {
    const PAIRS: usize = 50_000;
    let store = SqliteControlPlaneStore::open_in_memory().unwrap();
    {
        let mut conn = store.lock().unwrap();
        let tx = conn.transaction().unwrap();
        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO credit_entry
                        (id, organization_id, entry_seq, billing_account_id, kind, reference,
                         usage_event_id, idempotency_key, amount_micro, occurred_at_ms, payload)
                     VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, NULL, ?6, ?7, ?8)",
                )
                .unwrap();
            for pair in 0..PAIRS {
                for (offset, kind, amount) in [
                    (0usize, CreditKind::Grant, 1_000u64),
                    (1, CreditKind::Consume, 500),
                ] {
                    let seq = pair * 2 + offset;
                    let id = format!("crd_bulk_{seq:016x}");
                    let entry = CreditEntry {
                        id: CreditEntryId::try_new(&id).unwrap(),
                        organization: org(),
                        billing_account_id: account(),
                        kind,
                        amount_micro: amount,
                        reference: None,
                        usage_event_id: None,
                        reason: "bulk".into(),
                        occurred_at_ms: seq as i64,
                        idempotency_key: None,
                    };
                    stmt.execute(rusqlite::params![
                        id,
                        org().as_str(),
                        (seq + 1) as i64,
                        account().as_str(),
                        kind.as_str(),
                        amount as i64,
                        entry.occurred_at_ms,
                        serde_json::to_string(&entry).unwrap(),
                    ])
                    .unwrap();
                }
            }
        }
        tx.execute(
            "INSERT INTO billing_credit_seq (organization_id, next_seq) VALUES (?1, ?2)",
            rusqlite::params![org().as_str(), (PAIRS * 2) as i64],
        )
        .unwrap();
        tx.commit().unwrap();
    }
    let balance = store.credit_balance(&org()).unwrap();
    assert_eq!(balance.granted_micro, PAIRS as u64 * 1_000);
    assert_eq!(balance.consumed_micro, PAIRS as u64 * 500);
    assert_eq!(balance.held_micro, PAIRS as u64 * 500);
    assert_eq!(balance.pending_consumes, PAIRS as u64);
    assert_eq!(
        balance.balance_micro().unwrap(),
        PAIRS as u64 * 500,
        "exact free balance over 100k rows"
    );

    // A fresh grant is a point read + one row update, not another 100k fold.
    let start = std::time::Instant::now();
    store
        .append_credit_entry(&CreditEntry {
            id: CreditEntryId::try_new("crd_after_bulk").unwrap(),
            organization: org(),
            billing_account_id: account(),
            kind: CreditKind::Grant,
            amount_micro: 1_000,
            reference: None,
            usage_event_id: None,
            reason: "after-bulk".into(),
            occurred_at_ms: 0,
            idempotency_key: None,
        })
        .unwrap();
    assert_eq!(
        store
            .credit_balance(&org())
            .unwrap()
            .balance_micro()
            .unwrap(),
        PAIRS as u64 * 500 + 1_000
    );
    assert!(
        start.elapsed() < std::time::Duration::from_millis(500),
        "one append over a 100k ledger took {:?}",
        start.elapsed()
    );
}

/// A usage append whose checked aggregate would leave `u64` is refused
/// transactionally BEFORE it can poison the projection: the ledger stays
/// readable and exact at its previous boundary. (The historical path wrote
/// the row and then made EVERY subsequent fold/admission refuse forever.)
#[test]
fn sqlite_usage_append_refuses_an_overflow_without_poisoning_the_ledger() {
    let store = SqliteControlPlaneStore::open_in_memory().unwrap();
    let max = i64::MAX as u64;
    let mut base = event(1, 7, 0, 10);
    base.unit = UsageUnit::ProviderCostMicro;
    base.quantity = 1;
    base.provider_cost_micro = max;
    assert_eq!(
        store.append_usage_event(&base).unwrap(),
        crate::billing_store::UsageAppend::Appended
    );
    let mut second = event(2, 7, 0, 11);
    second.unit = UsageUnit::ProviderCostMicro;
    second.quantity = 1;
    second.provider_cost_micro = max;
    assert_eq!(
        store.append_usage_event(&second).unwrap(),
        crate::billing_store::UsageAppend::Appended
    );
    let boundary = store.usage_fold(&org()).unwrap();
    assert_eq!(boundary.totals.provider_cost_micro, u64::MAX - 1);
    let mut third = event(3, 7, 0, 12);
    third.unit = UsageUnit::ProviderCostMicro;
    third.quantity = 1;
    third.provider_cost_micro = max;
    match store.append_usage_event(&third).unwrap_err() {
        crate::billing_store::BillingStoreError::Ledger(CreditLedgerError::Overflow {
            field: "provider_cost_micro",
            ..
        }) => {}
        other => panic!("expected the typed append-domain refusal, got {other:?}"),
    }
    // The refused append wrote nothing: the same exact boundary is readable.
    assert_eq!(store.usage_fold(&org()).unwrap(), boundary);
    let page = store.usage_events(&org(), 0, 10).unwrap();
    assert_eq!(page.len(), 2, "the overflowing event was not written");
}

/// The projection and its correction lookup must stay index-backed: a scan
/// or temp sort here would reintroduce O(ledger) work under load.
#[test]
fn billing_rollup_queries_are_index_backed() {
    let store = SqliteControlPlaneStore::open_in_memory().unwrap();
    let conn = store.lock().unwrap();
    let cases: &[(&str, &[&dyn rusqlite::types::ToSql])] = &[
        (
            "SELECT input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                    reasoning_tokens, provider_cost_micro, managed_cost_micro, byok_cost_micro,
                    events, corrected_events, watermark
             FROM billing_usage_total WHERE organization_id = ?1",
            &[&"org" as &dyn rusqlite::types::ToSql],
        ),
        (
            "SELECT task_id, run_id FROM billing_usage_task
             WHERE organization_id = ?1 ORDER BY task_id, run_id",
            &[&"org" as &dyn rusqlite::types::ToSql],
        ),
        (
            "SELECT granted_micro, consumed_micro, refunded_micro, held_micro,
                    pending_consumes, watermark
             FROM billing_credit_rollup WHERE organization_id = ?1",
            &[&"org" as &dyn rusqlite::types::ToSql],
        ),
        (
            "SELECT payload FROM usage_event INDEXED BY idx_usage_event_correction_winner
             WHERE organization_id = ?1 AND correction_of = ?2 AND event_seq < ?3
             ORDER BY occurred_at_ms DESC, id DESC LIMIT 1",
            &[
                &"org" as &dyn rusqlite::types::ToSql,
                &"uev" as &dyn rusqlite::types::ToSql,
                &0i64 as &dyn rusqlite::types::ToSql,
            ],
        ),
        (
            "SELECT 1 FROM usage_event INDEXED BY idx_usage_event_correction_winner
             WHERE organization_id = ?1 AND correction_of = ?2 AND event_seq < ?3 LIMIT 1",
            &[
                &"org" as &dyn rusqlite::types::ToSql,
                &"uev" as &dyn rusqlite::types::ToSql,
                &0i64 as &dyn rusqlite::types::ToSql,
            ],
        ),
    ];
    for (sql, values) in cases {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let mut rows = stmt.query(*values).unwrap();
        let mut details = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            details.push(row.get::<_, String>(3).unwrap());
        }
        assert!(!details.is_empty(), "no plan for {sql}");
        for detail in &details {
            assert!(
                detail.contains("SEARCH ") && detail.contains("USING "),
                "billing rollup query is not index-backed ({sql}): {details:?}"
            );
            assert!(
                !detail.contains("SCAN ") && !detail.contains("TEMP B-TREE"),
                "billing rollup query regressed to a scan/sort ({sql}): {details:?}"
            );
        }
    }
}

/// The in-memory twin must keep serving the same pure fold (no projection to
/// drift), so a host that chooses it sees identical behavior.
#[test]
fn memory_store_fold_matches_sqlite_rollup() {
    let memory = MemoryBillingStore::new();
    let mut events = Vec::new();
    for i in 0..40usize {
        let event = event(i + 1, 1 + i as u64 % 3, 0, i as i64);
        memory.append_usage_event(&event).unwrap();
        events.push(event);
    }
    let want = fold_usage(&org(), &events).unwrap();
    assert_eq!(memory.usage_fold(&org()).unwrap(), want);
    assert_eq!(memory.usage_totals(&org()).unwrap(), want.totals);
}

/// ([perf]) Admission must stay O(1) at one million durable usage events:
/// the cold read repairs the projection ONCE (bounded by the ledger), then
/// every admission reads the caught-up rollup row instead of scanning the
/// ledger. The historical full-scan snapshot made each admission O(events).
#[test]
#[ignore = "[perf] admission over a 1M-event caught-up usage rollup"]
fn billing_admission_is_constant_at_one_million_events() {
    use crate::billing::{
        Admission, AdmissionBoundary, AdmissionRequest, BillingConfig, PlanConfig, Subscription,
        SubscriptionStatus, LIMIT_MAX_ACTIVE_TASKS, LIMIT_MAX_MANAGED_SPEND_MICRO_PER_PERIOD,
    };
    use crate::entitlements::EntitlementService;
    use crate::ids::SubscriptionId;
    use crate::service::ManualClock;
    use std::time::{Duration, Instant};

    const COUNT: usize = 1_000_000;
    let store = SqliteControlPlaneStore::open_in_memory().unwrap();
    {
        let mut conn = store.lock().unwrap();
        let tx = conn.transaction().unwrap();
        // One recursive INSERT builds the real 1M-row ledger (valid payloads:
        // the cold admission parses every row when it repairs the projection).
        tx.execute(
            "WITH RECURSIVE seq(n) AS (
                 SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < ?1
             )
             INSERT INTO usage_event
                (id, organization_id, event_seq, source_key, billing_account_id, task_id,
                 run_id, attempt_id, category, correction_of, occurred_at_ms,
                 reconciliation_state, payload)
             SELECT printf('uev_%016x', n),
                    ?2,
                    n,
                    printf('reservation:%d:input_tokens', n),
                    ?3,
                    printf('%016x', 1),
                    'run-1',
                    printf('attempt-%d', n),
                    'managed',
                    NULL,
                    0,
                    'reconciled',
                    json_object(
                        'id', printf('uev_%016x', n),
                        'organization_id', ?2,
                        'billing_account_id', ?3,
                        'task_id', 1,
                        'run_id', 'run-1',
                        'attempt_id', printf('attempt-%d', n),
                        'provider', 'openai',
                        'model', 'm',
                        'unit', 'input_tokens',
                        'quantity', 1,
                        'provider_cost_micro', '0',
                        'source_operation', 'session.cost_settle',
                        'occurred_at_ms', 0,
                        'reconciliation_state', 'reconciled',
                        'correction_of', NULL,
                        'category', 'managed',
                        'source_key', printf('reservation:%d:input_tokens', n)
                    )
             FROM seq",
            rusqlite::params![COUNT as i64, org().as_str(), account().as_str()],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO usage_event_seq (organization_id, next_seq) VALUES (?1, ?2)",
            rusqlite::params![org().as_str(), COUNT as i64],
        )
        .unwrap();
        tx.commit().unwrap();
    }

    let mut config = BillingConfig::default();
    config.plans.insert(
        "pro".into(),
        PlanConfig {
            period_ms: None,
            plan_id: "pro".into(),
            features: Default::default(),
            limits: [
                (LIMIT_MAX_ACTIVE_TASKS.to_string(), 100),
                (
                    LIMIT_MAX_MANAGED_SPEND_MICRO_PER_PERIOD.to_string(),
                    u64::MAX / 2,
                ),
            ]
            .into_iter()
            .collect(),
        },
    );
    config.default_plan = Some("pro".into());
    let service = EntitlementService::new_for_organization(
        Arc::new(store) as Arc<dyn BillingStore>,
        Arc::new(ManualClock::new(1_000)),
        config,
        Some(org()),
    )
    .unwrap();
    service
        .ensure_account(&org(), &account(), "rollup", true)
        .unwrap();
    service
        .set_subscription(&Subscription {
            id: SubscriptionId::try_new("sub_perf").unwrap(),
            organization: org(),
            plan_id: "pro".into(),
            status: SubscriptionStatus::Active,
            started_ms: 0,
            expires_ms: None,
            updated_ms: 0,
        })
        .unwrap();
    let request = AdmissionRequest::boundary(AdmissionBoundary::NewTask);

    // The cold read repairs the projection from all 1M rows exactly once.
    let cold_start = Instant::now();
    assert_eq!(
        service.check_admission(&org(), &request).unwrap(),
        Admission::Admitted
    );
    let cold = cold_start.elapsed();
    let totals = service.store().usage_totals(&org()).unwrap();
    assert_eq!(totals.events, COUNT as u64, "the cold read saw every row");
    assert_eq!(totals.input_tokens, COUNT as u64);

    // Every subsequent admission reads the caught-up rollup row: constant
    // work, no ledger scan. 200 samples keep the probe's own cost bounded.
    let mut samples = Vec::with_capacity(200);
    for _ in 0..200 {
        let start = Instant::now();
        assert_eq!(
            service.check_admission(&org(), &request).unwrap(),
            Admission::Admitted
        );
        samples.push(start.elapsed());
    }
    samples.sort_unstable();
    let p50 = samples[100];
    let p95 = samples[190];
    eprintln!("[perf] admission events={COUNT}: cold_repair={cold:?} p50={p50:?} p95={p95:?}");
    assert!(
        p95 < Duration::from_millis(50),
        "admission at 1M events is not constant: p95={p95:?}"
    );
    assert!(
        cold > p95.saturating_mul(10),
        "the cold repair must dominate the caught-up read: cold={cold:?} p95={p95:?}"
    );
}
