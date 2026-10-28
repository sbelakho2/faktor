// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! Real billing periods: the event→period identity, the per-`(org,
//! period)` and `(org, period, task, run)` aggregates, the durable period
//! configuration and the period-rollup catch-up. Split out of
//! `billing_store.rs` to keep the source-size ceiling honest; it is a child
//! module so it shares the store's private helpers.

use super::*;

#[cfg(test)]
thread_local! {
    /// Full-period bucket loads (the old pointwise path) and targeted bucket
    /// reads. The regression test pins `full == 0` for one append at 10k
    /// buckets, so a return to O(period buckets) per event fails loudly.
    static PERIOD_FULL_BUCKET_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PERIOD_TARGETED_BUCKET_READS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(super) fn reset_period_read_counters() {
    PERIOD_FULL_BUCKET_READS.with(|count| count.set(0));
    PERIOD_TARGETED_BUCKET_READS.with(|count| count.set(0));
}

#[cfg(test)]
pub(super) fn period_read_counters() -> (usize, usize) {
    (
        PERIOD_FULL_BUCKET_READS.with(std::cell::Cell::get),
        PERIOD_TARGETED_BUCKET_READS.with(std::cell::Cell::get),
    )
}

pub(super) fn memory_event_period(state: &MemBilling, event: &UsageEvent) -> String {
    match state.period_configs.get(event.organization_id.as_str()) {
        Some((anchor, period_ms)) => {
            BillingPeriod::derive(*anchor, *period_ms, event.occurred_at_ms)
                .map(|period| period.id)
                .unwrap_or_else(|| BillingPeriod::unconfigured().id)
        }
        None => BillingPeriod::unconfigured().id,
    }
}

/// The SQL schema of the real billing PERIOD projection (migration v9):
/// one event->period identity row (append-only; corrections inherit their
/// base's period), one maintained aggregate per `(organization, period)`,
/// one per `(organization, period, task, run)`, the durable period
/// configuration written when a subscription is set, the period-rollup
/// catch-up watermark, and the settlement-outbox projection watermark the
/// admission gate drains through. The all-time rollups (v8) stay for
/// analytics; quota limits read ONLY the current period.
pub const BILLING_PERIOD_SCHEMA_V9: &str = "
     CREATE TABLE IF NOT EXISTS usage_event_period (
        event_id TEXT PRIMARY KEY,
        organization_id TEXT NOT NULL,
        period_id TEXT NOT NULL,
        period_start_ms INTEGER NOT NULL,
        period_end_ms INTEGER NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_usage_event_period_org_period
        ON usage_event_period(organization_id, period_id);
     CREATE TABLE IF NOT EXISTS billing_period_config (
        organization_id TEXT PRIMARY KEY,
        anchor_ms INTEGER NOT NULL,
        period_ms INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS billing_period_rollup_state (
        organization_id TEXT PRIMARY KEY,
        watermark INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS billing_usage_period (
        organization_id TEXT NOT NULL,
        period_id TEXT NOT NULL,
        input_tokens TEXT NOT NULL,
        output_tokens TEXT NOT NULL,
        cache_read_tokens TEXT NOT NULL,
        cache_write_tokens TEXT NOT NULL,
        reasoning_tokens TEXT NOT NULL,
        provider_cost_micro TEXT NOT NULL,
        managed_cost_micro TEXT NOT NULL,
        byok_cost_micro TEXT NOT NULL,
        events TEXT NOT NULL,
        corrected_events TEXT NOT NULL,
        PRIMARY KEY (organization_id, period_id)
     );
     CREATE TABLE IF NOT EXISTS billing_usage_period_task (
        organization_id TEXT NOT NULL,
        period_id TEXT NOT NULL,
        task_id TEXT NOT NULL,
        run_id TEXT NOT NULL,
        input_tokens TEXT NOT NULL,
        output_tokens TEXT NOT NULL,
        cache_read_tokens TEXT NOT NULL,
        cache_write_tokens TEXT NOT NULL,
        reasoning_tokens TEXT NOT NULL,
        provider_cost_micro TEXT NOT NULL,
        managed_cost_micro TEXT NOT NULL,
        byok_cost_micro TEXT NOT NULL,
        events TEXT NOT NULL,
        corrected_events TEXT NOT NULL,
        PRIMARY KEY (organization_id, period_id, task_id, run_id)
     );
     CREATE TABLE IF NOT EXISTS billing_outbox_watermark (
        organization_id TEXT PRIMARY KEY,
        last_outbox_id INTEGER NOT NULL
     );
";

// --------------------------------------------------- real billing periods

pub(super) fn read_period_config(
    conn: &Connection,
    organization: &OrganizationId,
) -> Result<Option<(i64, i64)>, BillingStoreError> {
    conn.query_row(
        "SELECT anchor_ms, period_ms FROM billing_period_config
         WHERE organization_id = ?1",
        params![organization.as_str()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
    .map_err(backend)
}

pub(super) fn derive_event_period(
    conn: &Connection,
    organization: &OrganizationId,
    at_ms: i64,
) -> Result<BillingPeriod, BillingStoreError> {
    Ok(match read_period_config(conn, organization)? {
        Some((anchor, period_ms)) => BillingPeriod::derive(anchor, period_ms, at_ms)
            .unwrap_or_else(BillingPeriod::unconfigured),
        None => BillingPeriod::unconfigured(),
    })
}

pub(super) fn read_event_period(
    conn: &Connection,
    organization: &OrganizationId,
    event_id: &str,
) -> Result<Option<BillingPeriod>, BillingStoreError> {
    conn.query_row(
        "SELECT period_id, period_start_ms, period_end_ms
         FROM usage_event_period WHERE event_id = ?1 AND organization_id = ?2",
        params![event_id, organization.as_str()],
        |r| {
            Ok(BillingPeriod {
                id: r.get(0)?,
                start_ms: r.get(1)?,
                end_ms: r.get(2)?,
            })
        },
    )
    .optional()
    .map_err(backend)
}

/// Resolve (and durably record) the period of one event. A correction
/// ALWAYS inherits its base event's period: a late correction never moves
/// usage across a period boundary.
pub(super) fn ensure_event_period_tx(
    tx: &rusqlite::Transaction<'_>,
    organization: &OrganizationId,
    event: &UsageEvent,
) -> Result<BillingPeriod, BillingStoreError> {
    if let Some(existing) = read_event_period(tx, organization, event.id.as_str())? {
        return Ok(existing);
    }
    let period = match &event.correction_of {
        Some(base_id) => {
            if let Some(base_period) = read_event_period(tx, organization, base_id.as_str())? {
                base_period
            } else if let Some(base) = usage_event_by_id(tx, organization, base_id.as_str(), None)?
            {
                let derived = derive_event_period(tx, organization, base.occurred_at_ms)?;
                tx.execute(
                    "INSERT OR IGNORE INTO usage_event_period
                        (event_id, organization_id, period_id, period_start_ms, period_end_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        base.id.as_str(),
                        organization.as_str(),
                        derived.id,
                        derived.start_ms,
                        derived.end_ms
                    ],
                )
                .map_err(backend)?;
                derived
            } else {
                // A correction may be appended BEFORE its base (the fold is
                // order-independent). Its period is then derived from its
                // own settlement time; the base, when it arrives, derives
                // the same configuration independently.
                derive_event_period(tx, organization, event.occurred_at_ms)?
            }
        }
        None => derive_event_period(tx, organization, event.occurred_at_ms)?,
    };
    tx.execute(
        "INSERT OR IGNORE INTO usage_event_period
            (event_id, organization_id, period_id, period_start_ms, period_end_ms)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            event.id.as_str(),
            organization.as_str(),
            period.id,
            period.start_ms,
            period.end_ms
        ],
    )
    .map_err(backend)?;
    Ok(period)
}

pub(super) fn read_period_projection(
    conn: &Connection,
    organization: &OrganizationId,
    period_id: &str,
) -> Result<Option<UsageTotals>, BillingStoreError> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {USAGE_TOTAL_COLUMNS} FROM billing_usage_period
             WHERE organization_id = ?1 AND period_id = ?2"
        ))
        .map_err(backend)?;
    let mut rows = stmt
        .query(params![organization.as_str(), period_id])
        .map_err(backend)?;
    match rows.next().map_err(backend)? {
        Some(row) => Ok(Some(usage_totals_from_row(row, 0)?)),
        None => Ok(None),
    }
}

pub(super) fn read_period_buckets(
    conn: &Connection,
    organization: &OrganizationId,
    period_id: &str,
) -> Result<BTreeMap<UsageBucketKey, UsageTotals>, BillingStoreError> {
    #[cfg(test)]
    PERIOD_FULL_BUCKET_READS.with(|count| count.set(count.get() + 1));
    let mut stmt = conn
        .prepare(&format!(
            "SELECT task_id, run_id, {USAGE_TOTAL_COLUMNS}
             FROM billing_usage_period_task
             WHERE organization_id = ?1 AND period_id = ?2
             ORDER BY task_id, run_id"
        ))
        .map_err(backend)?;
    let mut rows = stmt
        .query(params![organization.as_str(), period_id])
        .map_err(backend)?;
    let mut out = BTreeMap::new();
    while let Some(row) = rows.next().map_err(backend)? {
        let task_text: String = row.get(0).map_err(backend)?;
        let run_id: String = row.get(1).map_err(backend)?;
        let task_id = task_id_from_text(&task_text).ok_or_else(|| {
            BillingStoreError::Malformed(format!(
                "billing period rollup carries the non-canonical task id {task_text:?}"
            ))
        })?;
        out.insert(
            (task_id_text(task_id), run_id),
            usage_totals_from_row(row, 2)?,
        );
    }
    Ok(out)
}

/// Read ONLY the bucket one event can touch. The live pointwise path never
/// loads the period's whole bucket set (that made one metering append
/// O(period buckets), so 1M rows over many tasks became quadratic).
pub(super) fn read_period_bucket(
    conn: &Connection,
    organization: &OrganizationId,
    period_id: &str,
    key: &UsageBucketKey,
) -> Result<Option<UsageTotals>, BillingStoreError> {
    #[cfg(test)]
    PERIOD_TARGETED_BUCKET_READS.with(|count| count.set(count.get() + 1));
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {USAGE_TOTAL_COLUMNS} FROM billing_usage_period_task
             WHERE organization_id = ?1 AND period_id = ?2 AND task_id = ?3 AND run_id = ?4"
        ))
        .map_err(backend)?;
    let mut rows = stmt
        .query(params![organization.as_str(), period_id, key.0, key.1])
        .map_err(backend)?;
    match rows.next().map_err(backend)? {
        Some(row) => Ok(Some(usage_totals_from_row(row, 0)?)),
        None => Ok(None),
    }
}

pub(super) fn write_period_total(
    conn: &Connection,
    organization: &OrganizationId,
    period_id: &str,
    totals: &UsageTotals,
) -> Result<(), BillingStoreError> {
    conn.execute(
        &format!(
            "INSERT INTO billing_usage_period
                (organization_id, period_id, {USAGE_TOTAL_COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(organization_id, period_id) DO UPDATE SET
                input_tokens = excluded.input_tokens,
                output_tokens = excluded.output_tokens,
                cache_read_tokens = excluded.cache_read_tokens,
                cache_write_tokens = excluded.cache_write_tokens,
                reasoning_tokens = excluded.reasoning_tokens,
                provider_cost_micro = excluded.provider_cost_micro,
                managed_cost_micro = excluded.managed_cost_micro,
                byok_cost_micro = excluded.byok_cost_micro,
                events = excluded.events,
                corrected_events = excluded.corrected_events,
                unpriced_calls = excluded.unpriced_calls"
        ),
        params![
            organization.as_str(),
            period_id,
            totals.input_tokens.to_string(),
            totals.output_tokens.to_string(),
            totals.cache_read_tokens.to_string(),
            totals.cache_write_tokens.to_string(),
            totals.reasoning_tokens.to_string(),
            totals.provider_cost_micro.to_string(),
            totals.managed_cost_micro.to_string(),
            totals.byok_cost_micro.to_string(),
            totals.events.to_string(),
            totals.corrected_events.to_string(),
            totals.unpriced_calls.to_string(),
        ],
    )
    .map_err(backend)?;
    Ok(())
}

pub(super) fn write_period_bucket(
    conn: &Connection,
    organization: &OrganizationId,
    period_id: &str,
    key: &UsageBucketKey,
    totals: &UsageTotals,
) -> Result<(), BillingStoreError> {
    conn.execute(
        &format!(
            "INSERT INTO billing_usage_period_task
                (organization_id, period_id, task_id, run_id, {USAGE_TOTAL_COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
             ON CONFLICT(organization_id, period_id, task_id, run_id) DO UPDATE SET
                input_tokens = excluded.input_tokens,
                output_tokens = excluded.output_tokens,
                cache_read_tokens = excluded.cache_read_tokens,
                cache_write_tokens = excluded.cache_write_tokens,
                reasoning_tokens = excluded.reasoning_tokens,
                provider_cost_micro = excluded.provider_cost_micro,
                managed_cost_micro = excluded.managed_cost_micro,
                byok_cost_micro = excluded.byok_cost_micro,
                events = excluded.events,
                corrected_events = excluded.corrected_events,
                unpriced_calls = excluded.unpriced_calls"
        ),
        params![
            organization.as_str(),
            period_id,
            key.0,
            key.1,
            totals.input_tokens.to_string(),
            totals.output_tokens.to_string(),
            totals.cache_read_tokens.to_string(),
            totals.cache_write_tokens.to_string(),
            totals.reasoning_tokens.to_string(),
            totals.provider_cost_micro.to_string(),
            totals.managed_cost_micro.to_string(),
            totals.byok_cost_micro.to_string(),
            totals.events.to_string(),
            totals.corrected_events.to_string(),
            totals.unpriced_calls.to_string(),
        ],
    )
    .map_err(backend)?;
    Ok(())
}

pub(super) fn period_rollup_watermark(
    conn: &Connection,
    organization: &OrganizationId,
) -> Result<i64, BillingStoreError> {
    Ok(conn
        .query_row(
            "SELECT watermark FROM billing_period_rollup_state WHERE organization_id = ?1",
            params![organization.as_str()],
            |r| r.get(0),
        )
        .optional()
        .map_err(backend)?
        .unwrap_or(0))
}

pub(super) fn apply_period_event_pointwise(
    conn: &Connection,
    organization: &OrganizationId,
    event: &UsageEvent,
    seq: i64,
    period: &BillingPeriod,
) -> Result<(), BillingStoreError> {
    let mut totals = read_period_projection(conn, organization, &period.id)?.unwrap_or_default();
    // Only the buckets this event can touch are read/written: its own, plus
    // a correction's prior winner and target. Reading the whole period here
    // made every append O(tasks in the period), i.e. quadratic at scale.
    let mut buckets = BTreeMap::new();
    let event_key = (task_id_text(event.task_id), event.run_id.clone());
    if let Some(values) = read_period_bucket(conn, organization, &period.id, &event_key)? {
        buckets.insert(event_key.clone(), values);
    }
    if let Some(base_id) = &event.correction_of {
        let period_scope = Some(period.id.as_str());
        let winner = correction_winner(conn, organization, base_id.as_str(), seq, period_scope)?;
        if let Some(winner) = &winner {
            let key = (task_id_text(winner.task_id), winner.run_id.clone());
            if let Some(values) = read_period_bucket(conn, organization, &period.id, &key)? {
                buckets.insert(key, values);
            }
        }
        if let Some(target) = usage_event_by_id(conn, organization, base_id.as_str(), period_scope)?
        {
            let key = (task_id_text(target.task_id), target.run_id.clone());
            if let Some(values) = read_period_bucket(conn, organization, &period.id, &key)? {
                buckets.insert(key, values);
            }
        }
    }
    let mut dirty = std::collections::BTreeSet::new();
    apply_usage_event(
        conn,
        organization,
        event,
        seq,
        Some(&period.id),
        &mut totals,
        &mut buckets,
        &mut dirty,
    )?;
    write_period_total(conn, organization, &period.id, &totals)?;
    for key in dirty {
        let totals = buckets.get(&key).cloned().unwrap_or_default();
        write_period_bucket(conn, organization, &period.id, &key, &totals)?;
    }
    Ok(())
}

/// Catch the per-period projections up to the ledger head. Two passes: first
/// every event gains its durable period identity, then each event is applied
/// to its period aggregate (the second pass can then resolve corrections and
/// their targets with the exact same logic as the all-time rollup).
pub(super) fn ensure_period_rollup_tx(
    tx: &rusqlite::Transaction<'_>,
    organization: &OrganizationId,
) -> Result<(), BillingStoreError> {
    let target = usage_next_seq(tx, organization)?;
    let watermark = period_rollup_watermark(tx, organization)?;
    if watermark >= target {
        return Ok(());
    }
    let mut cursor = watermark;
    loop {
        let page: Vec<(i64, String)> = {
            let mut stmt = tx
                .prepare(
                    "SELECT event_seq, payload FROM usage_event
                     WHERE organization_id = ?1 AND event_seq > ?2
                     ORDER BY event_seq LIMIT 512",
                )
                .map_err(backend)?;
            let mapped = stmt
                .query_map(params![organization.as_str(), cursor], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(backend)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(backend)?;
            mapped
        };
        if page.is_empty() {
            break;
        }
        for (_, payload) in &page {
            let event: UsageEvent = parse(payload)?;
            if event.organization_id != *organization {
                return Err(BillingStoreError::Ledger(CreditLedgerError::Corrupt {
                    detail: format!(
                        "usage event {} in organization {} names organization {}",
                        event.id, organization, event.organization_id
                    ),
                }));
            }
            ensure_event_period_tx(tx, organization, &event)?;
        }
        cursor = page.last().map(|(seq, _)| *seq).unwrap_or(cursor);
    }
    let mut periods: BTreeMap<String, (UsageTotals, BTreeMap<UsageBucketKey, UsageTotals>)> =
        BTreeMap::new();
    let mut cursor = watermark;
    loop {
        let page: Vec<(i64, String)> = {
            let mut stmt = tx
                .prepare(
                    "SELECT event_seq, payload FROM usage_event
                     WHERE organization_id = ?1 AND event_seq > ?2
                     ORDER BY event_seq LIMIT 512",
                )
                .map_err(backend)?;
            let mapped = stmt
                .query_map(params![organization.as_str(), cursor], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(backend)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(backend)?;
            mapped
        };
        if page.is_empty() {
            break;
        }
        for (seq, payload) in page {
            let event: UsageEvent = parse(&payload)?;
            let period =
                read_event_period(tx, organization, event.id.as_str())?.ok_or_else(|| {
                    BillingStoreError::Malformed(format!(
                        "usage event {} has no durable period identity",
                        event.id
                    ))
                })?;
            if !periods.contains_key(&period.id) {
                let totals =
                    read_period_projection(tx, organization, &period.id)?.unwrap_or_default();
                let buckets = read_period_buckets(tx, organization, &period.id)?;
                periods.insert(period.id.clone(), (totals, buckets));
            }
            let (totals, buckets) = periods
                .get_mut(&period.id)
                .expect("period entry was just inserted");
            let mut dirty = std::collections::BTreeSet::new();
            apply_usage_event(
                tx,
                organization,
                &event,
                seq,
                Some(&period.id),
                totals,
                buckets,
                &mut dirty,
            )?;
            cursor = seq;
        }
    }
    for (period_id, (totals, buckets)) in &periods {
        write_period_total(tx, organization, period_id, totals)?;
        for (key, totals) in buckets {
            write_period_bucket(tx, organization, period_id, key, totals)?;
        }
    }
    tx.execute(
        "INSERT INTO billing_period_rollup_state (organization_id, watermark)
         VALUES (?1, ?2)
         ON CONFLICT(organization_id) DO UPDATE SET watermark = excluded.watermark",
        params![organization.as_str(), cursor.max(watermark)],
    )
    .map_err(backend)?;
    Ok(())
}

pub(super) fn ensure_period_rollup(
    conn: &mut Connection,
    organization: &OrganizationId,
) -> Result<(), BillingStoreError> {
    let target = usage_next_seq(conn, organization)?;
    if period_rollup_watermark(conn, organization)? >= target {
        return Ok(());
    }
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(backend)?;
    ensure_period_rollup_tx(&tx, organization)?;
    tx.commit().map_err(backend)?;
    Ok(())
}

pub(super) fn read_period_fold(
    conn: &Connection,
    organization: &OrganizationId,
    period_id: &str,
) -> Result<UsageFold, BillingStoreError> {
    let totals = read_period_projection(conn, organization, period_id)?.unwrap_or_default();
    let mut per_task = Vec::new();
    for ((task_text, run_id), totals) in read_period_buckets(conn, organization, period_id)? {
        let task_id = task_id_from_text(&task_text).ok_or_else(|| {
            BillingStoreError::Malformed(format!(
                "billing period task bucket carries the non-canonical task id {task_text:?}"
            ))
        })?;
        per_task.push(TaskUsage {
            task_id,
            run_id,
            totals,
        });
    }
    Ok(UsageFold {
        organization_id: organization.clone(),
        totals,
        per_task,
        next_cursor: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SqliteControlPlaneStore;
    use crate::{ReconciliationState, UsageEventId};

    fn probe_event() -> UsageEvent {
        UsageEvent {
            id: UsageEventId::try_new("uev_period_probe").unwrap(),
            organization_id: OrganizationId::try_new("org_period").unwrap(),
            billing_account_id: BillingAccountId::try_new("acct_period").unwrap(),
            task_id: 9_999,
            run_id: "run_probe".into(),
            attempt_id: "attempt_probe".into(),
            provider: "openai".into(),
            model: "m".into(),
            unit: UsageUnit::ProviderCostMicro,
            quantity: 1,
            provider_cost_micro: 7,
            source_operation: "probe".into(),
            occurred_at_ms: 1,
            reconciliation_state: ReconciliationState::Reconciled,
            correction_of: None,
            category: SpendCategory::Managed,
            source_key: "probe:period:1".into(),
        }
    }

    #[test]
    fn pointwise_append_reads_only_touched_period_buckets() {
        let store = SqliteControlPlaneStore::open_in_memory().unwrap();
        let event = probe_event();
        let organization = event.organization_id.clone();
        let period = BillingPeriod::unconfigured();
        {
            let conn = store.raw_conn_for_test();
            for index in 0..10_000u64 {
                conn.execute(
                    "INSERT INTO billing_usage_period_task (
                        organization_id, period_id, task_id, run_id, input_tokens,
                        output_tokens, cache_read_tokens, cache_write_tokens,
                        reasoning_tokens, provider_cost_micro, managed_cost_micro,
                        byok_cost_micro, events, corrected_events
                     ) VALUES (?1, ?2, ?3, 'run', '0','0','0','0','0','0','0','0','1','0')",
                    rusqlite::params![organization.as_str(), period.id, index.to_string()],
                )
                .unwrap();
            }
        }
        reset_period_read_counters();
        {
            let conn = store.raw_conn_for_test();
            apply_period_event_pointwise(&conn, &organization, &event, 1, &period).unwrap();
        }
        let (full, targeted) = period_read_counters();
        assert_eq!(
            full, 0,
            "one append must never load the whole period's 10k buckets"
        );
        assert!(
            targeted <= 3,
            "one uncorrected append reads at most its own bucket, got {targeted}"
        );
        let conn = store.raw_conn_for_test();
        let touched: String = conn
            .query_row(
                "SELECT provider_cost_micro FROM billing_usage_period_task
                 WHERE organization_id = ?1 AND period_id = ?2 AND task_id = ?3 AND run_id = ?4",
                rusqlite::params![
                    organization.as_str(),
                    period.id,
                    task_id_text(event.task_id),
                    event.run_id
                ],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(touched, "7");
    }
}
