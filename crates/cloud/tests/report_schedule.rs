//! Adversarial tests of the durable billing-report schedule rows: strict
//! payload decode, identity/upsert semantics, terminal statuses, restart
//! persistence, the bounded retention/pruning window and memory/sqlite
//! parity.

use faktor_cloud::{
    catch_up_plan, period_from_index, period_index, BillingStore, CatchUpPlan, MemoryBillingStore,
    OrganizationId, ReportPeriodRow, ReportPeriodStatus, SqliteControlPlaneStore,
    MAX_REPORT_PERIODS_PER_ORG,
};

fn row(
    organization: &str,
    period: &str,
    status: ReportPeriodStatus,
    seen_ms: i64,
) -> ReportPeriodRow {
    ReportPeriodRow {
        organization_id: organization.to_string(),
        period: period.to_string(),
        next_cursor: None,
        status,
        attempts: 0,
        last_error: None,
        first_seen_ms: seen_ms,
        reported_at_ms: None,
        next_attempt_at_ms: seen_ms,
        skipped_at_ms: None,
    }
}

#[test]
fn status_roundtrips_are_strict() {
    for status in [
        ReportPeriodStatus::Open,
        ReportPeriodStatus::Reported,
        ReportPeriodStatus::Failed,
        ReportPeriodStatus::Skipped,
    ] {
        assert_eq!(ReportPeriodStatus::parse(status.as_str()), Some(status));
    }
    assert_eq!(ReportPeriodStatus::parse("OPEN"), None);
    assert_eq!(ReportPeriodStatus::parse(""), None);
    assert!(!ReportPeriodStatus::Open.is_terminal());
    assert!(ReportPeriodStatus::Reported.is_terminal());
    assert!(ReportPeriodStatus::Failed.is_terminal());
    assert!(ReportPeriodStatus::Skipped.is_terminal());
}

/// The bounded catch-up plan: the most recent `max_catch_up` missed periods
/// are backfilled oldest-first, everything older is the skipped range.
#[test]
fn catch_up_plan_is_bounded_and_skips_only_older_periods() {
    // Everything fits: no skip.
    let plan = catch_up_plan(0, 4, 24);
    assert_eq!(plan.backfill, vec![1, 2, 3]);
    assert_eq!(plan.skipped, None);
    // Cap 2 of 3 missed: the OLDEST is skipped, the newest two backfill.
    let plan = catch_up_plan(0, 4, 2);
    assert_eq!(plan.backfill, vec![2, 3]);
    assert_eq!(plan.skipped, Some((1, 1)));
    // Cap 0: everything is skipped (audited), nothing backfills.
    let plan = catch_up_plan(0, 4, 0);
    assert!(plan.backfill.is_empty());
    assert_eq!(plan.skipped, Some((1, 3)));
    // A huge gap never enumerates more than the cap.
    let plan = catch_up_plan(0, 1_000_000, 3);
    assert_eq!(plan.backfill, vec![999_997, 999_998, 999_999]);
    assert_eq!(plan.skipped, Some((1, 999_996)));
    // No gap (or a clock regression): empty plan.
    assert_eq!(
        catch_up_plan(5, 6, 4),
        CatchUpPlan {
            backfill: Vec::new(),
            skipped: None
        }
    );
    assert_eq!(
        catch_up_plan(9, 3, 4),
        CatchUpPlan {
            backfill: Vec::new(),
            skipped: None
        }
    );
    // Period keys/indexes round-trip (including negative buckets).
    assert_eq!(period_index("p-1"), Some(-1));
    assert_eq!(period_index(&period_from_index(42)), Some(42));
    assert_eq!(period_index("p"), None);
    assert_eq!(period_index("q7"), None);
    assert_eq!(period_index("p7x"), None);
}

#[test]
fn row_payloads_are_strictly_decoded() {
    let good = serde_json::json!({
        "organization_id": "org_a",
        "period": "p1",
        "status": "open",
        "first_seen_ms": 1,
        "next_attempt_at_ms": 2,
    });
    let parsed: ReportPeriodRow = serde_json::from_value(good).unwrap();
    assert_eq!(parsed.status, ReportPeriodStatus::Open);
    assert_eq!(parsed.next_cursor, None);
    assert_eq!(parsed.skipped_at_ms, None);
    // A skipped audit row carries its decision instant and range.
    let skipped: ReportPeriodRow = serde_json::from_value(serde_json::json!({
        "organization_id": "org_a",
        "period": "p1",
        "status": "skipped",
        "first_seen_ms": 1,
        "next_attempt_at_ms": 2,
        "skipped_at_ms": 3,
        "last_error": "skipped 2 missed periods p1..p2: catch-up cap exceeded",
    }))
    .unwrap();
    assert_eq!(skipped.status, ReportPeriodStatus::Skipped);
    assert_eq!(skipped.skipped_at_ms, Some(3));
    for bad in [
        // Unknown field.
        serde_json::json!({"organization_id": "o", "period": "p", "status": "open",
                           "first_seen_ms": 1, "next_attempt_at_ms": 2, "extra": 1}),
        // Unknown status.
        serde_json::json!({"organization_id": "o", "period": "p", "status": "maybe",
                           "first_seen_ms": 1, "next_attempt_at_ms": 2}),
        // Missing required field.
        serde_json::json!({"organization_id": "o", "status": "open",
                           "first_seen_ms": 1, "next_attempt_at_ms": 2}),
    ] {
        assert!(serde_json::from_value::<ReportPeriodRow>(bad).is_err());
    }
}

#[test]
fn memory_and_sqlite_report_rows_have_identical_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let memory: Vec<Box<dyn BillingStore>> = vec![Box::new(MemoryBillingStore::new())];
    let sqlite = SqliteControlPlaneStore::open(&dir.path().join("cp.db")).unwrap();
    let stores: Vec<&dyn BillingStore> = vec![memory[0].as_ref(), &sqlite];
    let organization = OrganizationId::try_new("org_a").unwrap();
    let other = OrganizationId::try_new("org_b").unwrap();
    for store in &stores {
        assert_eq!(store.report_period(&organization, "p1").unwrap(), None);
        let mut open = row("org_a", "p1", ReportPeriodStatus::Open, 10);
        open.next_cursor = Some("c1".into());
        store.put_report_period(&open).unwrap();
        let stored = store.report_period(&organization, "p1").unwrap().unwrap();
        assert_eq!(stored, open);
        // A continuation upsert keeps identity and advances the cursor.
        open.next_cursor = Some("c2".into());
        open.attempts = 3;
        store.put_report_period(&open).unwrap();
        assert_eq!(
            store.report_period(&organization, "p1").unwrap().unwrap(),
            open
        );
        // Terminal transition is durable and idempotent to re-put.
        open.status = ReportPeriodStatus::Reported;
        open.reported_at_ms = Some(99);
        store.put_report_period(&open).unwrap();
        store.put_report_period(&open).unwrap();
        assert_eq!(
            store
                .report_period(&organization, "p1")
                .unwrap()
                .unwrap()
                .status,
            ReportPeriodStatus::Reported
        );
        // Tenant isolation: another organization sees nothing.
        store
            .put_report_period(&row("org_b", "p1", ReportPeriodStatus::Open, 11))
            .unwrap();
        assert_eq!(store.report_periods(&organization, 10).unwrap().len(), 1);
        assert_eq!(store.report_periods(&other, 10).unwrap().len(), 1);
        assert_eq!(
            store.report_periods(&other, 10).unwrap()[0].organization_id,
            "org_b"
        );
    }
}

#[test]
fn report_rows_survive_a_restart_and_prune_to_the_bound() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cp.db");
    let organization = OrganizationId::try_new("org_a").unwrap();
    {
        let store = SqliteControlPlaneStore::open(&path).unwrap();
        store
            .put_report_period(&row("org_a", "p1", ReportPeriodStatus::Reported, 1))
            .unwrap();
        // Fill beyond the bound: the oldest terminal rows are pruned and an
        // open period is retained preferentially.
        let bound = MAX_REPORT_PERIODS_PER_ORG;
        for index in 0..bound {
            store
                .put_report_period(&row(
                    "org_a",
                    &format!("fill{index:04}"),
                    ReportPeriodStatus::Reported,
                    100 + index as i64,
                ))
                .unwrap();
        }
        let open_seen = 100 + bound as i64 + 1;
        store
            .put_report_period(&row(
                "org_a",
                "open-period",
                ReportPeriodStatus::Open,
                open_seen,
            ))
            .unwrap();
    }
    let store = SqliteControlPlaneStore::open(&path).unwrap();
    let rows = store.report_periods(&organization, 10_000).unwrap();
    assert_eq!(rows.len(), MAX_REPORT_PERIODS_PER_ORG);
    assert!(
        rows.iter().any(|r| r.period == "open-period"),
        "the open period survives pruning"
    );
    assert!(
        !rows.iter().any(|r| r.period == "p1"),
        "the oldest terminal row is pruned first"
    );
    // Newest-first listing.
    assert_eq!(rows[0].period, "open-period");

    // The in-memory store prunes with the same bound and the same
    // open-preferring rule.
    let memory = MemoryBillingStore::new();
    memory
        .put_report_period(&row("org_a", "p1", ReportPeriodStatus::Reported, 1))
        .unwrap();
    for index in 0..MAX_REPORT_PERIODS_PER_ORG {
        memory
            .put_report_period(&row(
                "org_a",
                &format!("fill{index:04}"),
                ReportPeriodStatus::Reported,
                100 + index as i64,
            ))
            .unwrap();
    }
    memory
        .put_report_period(&row(
            "org_a",
            "open-period",
            ReportPeriodStatus::Open,
            100 + MAX_REPORT_PERIODS_PER_ORG as i64 + 1,
        ))
        .unwrap();
    let rows = memory.report_periods(&organization, 10_000).unwrap();
    assert_eq!(rows.len(), MAX_REPORT_PERIODS_PER_ORG);
    assert!(rows.iter().any(|r| r.period == "open-period"));
    assert!(!rows.iter().any(|r| r.period == "p1"));
}

/// Open periods and skipped audit rows are retained preferentially when the
/// bounded schedule prunes: a same-age terminal (reported) row is evicted
/// first, so a skip decision can never be silently lost to retention.
#[test]
fn open_and_skipped_audit_rows_survive_pruning_preferentially() {
    let organization = OrganizationId::try_new("org_a").unwrap();
    let bound = MAX_REPORT_PERIODS_PER_ORG;
    let build = |store: &dyn BillingStore| {
        for index in 0..(bound - 2) {
            store
                .put_report_period(&row(
                    "org_a",
                    &format!("r{index:04}"),
                    ReportPeriodStatus::Reported,
                    100,
                ))
                .unwrap();
        }
        store
            .put_report_period(&row(
                "org_a",
                "open-same-age",
                ReportPeriodStatus::Open,
                100,
            ))
            .unwrap();
        store
            .put_report_period(&row(
                "org_a",
                "skipped-same-age",
                ReportPeriodStatus::Skipped,
                100,
            ))
            .unwrap();
        store
            .put_report_period(&row(
                "org_a",
                "reported-extra",
                ReportPeriodStatus::Reported,
                100,
            ))
            .unwrap();
    };
    let check = |store: &dyn BillingStore| {
        let rows = store.report_periods(&organization, 10_000).unwrap();
        assert_eq!(rows.len(), bound);
        assert!(
            rows.iter().any(|row| row.period == "open-same-age"),
            "an open period survives"
        );
        assert!(
            rows.iter().any(|row| row.period == "skipped-same-age"),
            "a skipped audit row survives"
        );
        assert!(
            !rows.iter().any(|row| row.period == "r0000"),
            "a reported row is evicted first"
        );
    };
    let memory = MemoryBillingStore::new();
    build(&memory);
    check(&memory);
    let dir = tempfile::tempdir().unwrap();
    let sqlite = SqliteControlPlaneStore::open(&dir.path().join("cp.db")).unwrap();
    build(&sqlite);
    check(&sqlite);
}
