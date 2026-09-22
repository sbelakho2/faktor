//! The durable billing seam: the [`BillingStore`] trait plus its in-memory
//! implementation and the additive SQLite implementation over the SAME
//! [`crate::store::SqliteControlPlaneStore`] database and migration ladder
//! (its migration v2 — the crate-owned next `user_version` — and its
//! migration v6, which rebuilds `usage_event.task_id` from the old lossy
//! signed INTEGER projection to the reversible fixed-width text encoding
//! [`crate::billing::task_id_text`]).
//!
//! Append-only enforcement lives here: usage events and credit entries are
//! INSERT-only (no UPDATE/DELETE surface exists), ingestion is idempotent
//! per `(organization, source_key)`, and every credit mutation runs in ONE
//! immediate transaction that re-derives the balance before the insert, so
//! two racing consumes can never jointly overdraw the account and a refund
//! can never exceed what its referenced consume/settle actually spent.
//!
//! Task identity: the `task_id` column is TEXT holding exactly
//! [`crate::billing::task_id_text`]`(id)` (16 lowercase hex digits, a
//! bijection over the full u64 domain). Writes encode through that helper
//! and task-filtered queries compare against the same encoding, so
//! `usage_events_of_task` can never alias two ids that the old
//! `task_id.min(i64::MAX as u64) as i64` projection collapsed together.
//!
//! Amount projection: `credit_entry.amount_micro` is a signed `INTEGER`
//! column whose domain is bounded by
//! [`crate::billing::MAX_CREDIT_AMOUNT_MICRO`] (`i64::MAX` micro-units).
//! Writes therefore store the amount EXACTLY (no `.min(...)` clamp, checked
//! conversion only) and every read decodes the column and the payload
//! together, refusing typed when they disagree — a legacy row written by the
//! old clamping writer is surfaced, never silently reinterpreted as its
//! clamped sentinel.

use std::collections::BTreeMap;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::billing::{
    checked_append_domain, checked_ledger_sum, fold_credits, BillingAccount, CreditBalance,
    CreditEntry, CreditKind, CreditLedgerError, InFlightTxn, Subscription, UsageEvent,
};
use crate::error::ControlPlaneError;
use crate::ids::{BillingAccountId, InFlightTxnId, OrganizationId};
use crate::store::{CloudStoreError, SqliteControlPlaneStore};

/// The SQL schema of the billing domain (migration v2 of the control-plane
/// ladder). Append-only by construction: no UPDATE/DELETE statement names
/// `usage_event` or `credit_entry` outside the store's own read paths.
///
/// `usage_event.task_id` is created as `INTEGER` here for historical shape
/// only: migration [`BILLING_TASK_ID_TEXT_SCHEMA_V6`] rebuilds it as the
/// reversible fixed-width TEXT encoding before any query runs, fresh
/// databases included (see [`crate::billing::task_id_text`]).
pub const BILLING_SCHEMA_V2: &str = "
     CREATE TABLE IF NOT EXISTS billing_account (
        id TEXT PRIMARY KEY,
        organization_id TEXT NOT NULL,
        payload TEXT NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_billing_account_org
        ON billing_account(organization_id, id);
     CREATE TABLE IF NOT EXISTS billing_subscription (
        id TEXT PRIMARY KEY,
        organization_id TEXT NOT NULL UNIQUE,
        payload TEXT NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_billing_subscription_org
        ON billing_subscription(organization_id);
     CREATE TABLE IF NOT EXISTS usage_event (
        id TEXT PRIMARY KEY,
        organization_id TEXT NOT NULL,
        event_seq INTEGER NOT NULL,
        source_key TEXT NOT NULL,
        billing_account_id TEXT NOT NULL,
        task_id INTEGER NOT NULL,
        run_id TEXT NOT NULL,
        attempt_id TEXT NOT NULL,
        category TEXT NOT NULL,
        correction_of TEXT,
        occurred_at_ms INTEGER NOT NULL,
        reconciliation_state TEXT NOT NULL,
        payload TEXT NOT NULL,
        UNIQUE (organization_id, source_key)
     );
     CREATE INDEX IF NOT EXISTS idx_usage_event_org_seq
        ON usage_event(organization_id, event_seq);
     CREATE INDEX IF NOT EXISTS idx_usage_event_org_task
        ON usage_event(organization_id, task_id, event_seq);
     CREATE INDEX IF NOT EXISTS idx_usage_event_correction
        ON usage_event(organization_id, correction_of);
     CREATE TABLE IF NOT EXISTS usage_event_seq (
        organization_id TEXT PRIMARY KEY,
        next_seq INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS credit_entry (
        id TEXT PRIMARY KEY,
        organization_id TEXT NOT NULL,
        entry_seq INTEGER NOT NULL,
        billing_account_id TEXT NOT NULL,
        kind TEXT NOT NULL,
        reference TEXT,
        usage_event_id TEXT,
        idempotency_key TEXT,
        amount_micro INTEGER NOT NULL,
        occurred_at_ms INTEGER NOT NULL,
        payload TEXT NOT NULL,
        UNIQUE (organization_id, idempotency_key)
     );
     CREATE INDEX IF NOT EXISTS idx_credit_entry_org_seq
        ON credit_entry(organization_id, entry_seq);
     CREATE INDEX IF NOT EXISTS idx_credit_entry_org_ref
        ON credit_entry(organization_id, reference);
     CREATE TABLE IF NOT EXISTS billing_credit_seq (
        organization_id TEXT PRIMARY KEY,
        next_seq INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS billing_in_flight (
        id TEXT PRIMARY KEY,
        organization_id TEXT NOT NULL,
        kind TEXT NOT NULL,
        reference TEXT NOT NULL,
        started_ms INTEGER NOT NULL,
        ended_ms INTEGER,
        payload TEXT NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_billing_in_flight_org
        ON billing_in_flight(organization_id, ended_ms);
";

/// The SQL half of migration v6 of the control-plane ladder: rebuild
/// `usage_event.task_id` from the lossy signed INTEGER projection to the
/// reversible fixed-width TEXT encoding ([`crate::billing::task_id_text`]).
///
/// The old projection was `task_id.min(i64::MAX as u64) as i64`: every id
/// `>= i64::MAX` aliased onto the same indexed value, so a task-filtered
/// query could return another task's ledger rows. The rebuild cannot be
/// completed in SQL alone: SQLite JSON functions expose an integer above
/// `i64::MAX` only as an inexact REAL, and the legacy payload is the sole
/// authority for such an id. This SQL therefore only STAGES the rebuild —
/// every row gets a syntactically valid placeholder and its legacy INTEGER
/// value is carried in `usage_event_v6_task_id_carry` — and
/// [`finalize_task_id_text_migration`] resolves each row's exact id in Rust
/// inside the SAME migration transaction, then drops the carry table. The
/// placeholder is never visible to a committed database; if any row cannot
/// be recovered exactly the whole migration transaction rolls back and the
/// pre-v6 database (and its rows) stay untouched.
pub const BILLING_TASK_ID_TEXT_SCHEMA_V6: &str = "
     CREATE TABLE usage_event_v6 (
        id TEXT PRIMARY KEY,
        organization_id TEXT NOT NULL,
        event_seq INTEGER NOT NULL,
        source_key TEXT NOT NULL,
        billing_account_id TEXT NOT NULL,
        task_id TEXT NOT NULL
            CONSTRAINT usage_event_task_id_text CHECK (
                length(task_id) = 16
                AND task_id NOT GLOB '*[^0-9a-f]*'
            ),
        run_id TEXT NOT NULL,
        attempt_id TEXT NOT NULL,
        category TEXT NOT NULL,
        correction_of TEXT,
        occurred_at_ms INTEGER NOT NULL,
        reconciliation_state TEXT NOT NULL,
        payload TEXT NOT NULL,
        UNIQUE (organization_id, source_key)
     );
     CREATE TABLE usage_event_v6_task_id_carry (
        event_id TEXT PRIMARY KEY,
        legacy_task_id INTEGER NOT NULL
     );
     INSERT INTO usage_event_v6
        (id, organization_id, event_seq, source_key, billing_account_id, task_id,
         run_id, attempt_id, category, correction_of, occurred_at_ms,
         reconciliation_state, payload)
     SELECT id, organization_id, event_seq, source_key, billing_account_id,
            '0000000000000000',
            run_id, attempt_id, category, correction_of, occurred_at_ms,
            reconciliation_state, payload
     FROM usage_event;
     INSERT INTO usage_event_v6_task_id_carry (event_id, legacy_task_id)
     SELECT id, task_id FROM usage_event;
     DROP TABLE usage_event;
     ALTER TABLE usage_event_v6 RENAME TO usage_event;
     CREATE INDEX IF NOT EXISTS idx_usage_event_org_seq
        ON usage_event(organization_id, event_seq);
     CREATE INDEX IF NOT EXISTS idx_usage_event_org_task
        ON usage_event(organization_id, task_id, event_seq);
     CREATE INDEX IF NOT EXISTS idx_usage_event_correction
        ON usage_event(organization_id, correction_of);
";

/// The staging table [`BILLING_TASK_ID_TEXT_SCHEMA_V6`] carries legacy
/// INTEGER task ids in until [`finalize_task_id_text_migration`] resolves
/// them; its presence marks the rebuild as not yet finalized.
const TASK_ID_TEXT_CARRY_TABLE: &str = "usage_event_v6_task_id_carry";
/// Page size of the v6 finalizer (bounded everything: the migration never
/// holds an unbounded row set in memory).
const TASK_ID_TEXT_FINALIZE_PAGE: i64 = 256;

/// Resolve the staged v6 rebuild exactly. For every carried row: read the
/// original id from the JSON payload (the authority the read paths already
/// return), verify it against the carried legacy projection, write the
/// reversible [`crate::billing::task_id_text`] encoding, and finally drop
/// the carry table. Runs inside the migration transaction, so a refusal
/// leaves the database exactly at v5.
///
/// Refuses typed (never guesses) when a row cannot be recovered exactly:
///
/// - the legacy stored value is not a non-zero id;
/// - the payload is unreadable or carries no exact u64 `task_id` (ids above
///   `i64::MAX` cannot be parsed out of JSON by SQL, which is why this runs
///   in Rust);
/// - the payload id is 0 (never a legal event) or the two sources
///   disagree. A carried value below `i64::MAX` was stored exactly, so a
///   disagreement is corruption. A carried `i64::MAX` is the one
///   intentionally lossy case (every original id `>= i64::MAX` clamped onto
///   it), so it is accepted when the payload id is `>= i64::MAX` and
///   refused when the payload id is lower.
///
/// Idempotent: a no-op when the carry table is absent (already finalized or
/// never staged), so it is safe to call on every open.
pub(crate) fn finalize_task_id_text_migration(conn: &Connection) -> Result<(), BillingStoreError> {
    let staged: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![TASK_ID_TEXT_CARRY_TABLE],
            |r| r.get(0),
        )
        .optional()
        .map_err(backend)?;
    if staged.is_none() {
        return Ok(());
    }
    let mut after = String::new();
    loop {
        let mut stmt = conn
            .prepare(
                "SELECT carry.event_id, carry.legacy_task_id, event.payload
                 FROM usage_event_v6_task_id_carry carry
                 JOIN usage_event event ON event.id = carry.event_id
                 WHERE carry.event_id > ?1
                 ORDER BY carry.event_id LIMIT ?2",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(params![after, TASK_ID_TEXT_FINALIZE_PAGE], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        if rows.is_empty() {
            break;
        }
        after = rows.last().map(|row| row.0.clone()).unwrap_or_default();
        for (event_id, legacy_task_id, payload) in rows {
            let task_id = resolve_legacy_task_id(&event_id, legacy_task_id, &payload)?;
            conn.execute(
                "UPDATE usage_event SET task_id = ?1 WHERE id = ?2",
                params![crate::billing::task_id_text(task_id), event_id],
            )
            .map_err(backend)?;
        }
    }
    conn.execute("DROP TABLE usage_event_v6_task_id_carry", [])
        .map_err(backend)?;
    Ok(())
}

/// One legacy row's exact recovery (see [`finalize_task_id_text_migration`]
/// for the refusal rules). Extracted so every refusal names the event id.
fn resolve_legacy_task_id(
    event_id: &str,
    legacy_task_id: i64,
    payload: &str,
) -> Result<u64, BillingStoreError> {
    let refuse = |reason: String| {
        BillingStoreError::Malformed(format!(
            "usage_event {event_id}: {reason}; refusing to guess the task identity \
             (operator verification required)"
        ))
    };
    if legacy_task_id < 1 {
        return Err(refuse(format!(
            "legacy stored task id {legacy_task_id} is not a valid non-zero id"
        )));
    }
    let payload: serde_json::Value = serde_json::from_str(payload).map_err(|e| {
        refuse(format!(
            "payload is unreadable ({e}), so the exact task id cannot be recovered"
        ))
    })?;
    let task_id = payload
        .get("task_id")
        .and_then(|value| value.as_u64())
        .ok_or_else(|| refuse("payload carries no exact u64 task_id".to_string()))?;
    if task_id == 0 {
        return Err(refuse("payload task id is 0".to_string()));
    }
    if legacy_task_id < i64::MAX {
        if task_id != legacy_task_id as u64 {
            return Err(refuse(format!(
                "payload task id {task_id} disagrees with the exact stored id {legacy_task_id}"
            )));
        }
    } else if task_id < i64::MAX as u64 {
        return Err(refuse(format!(
            "the stored id is the clamped sentinel i64::MAX but payload task id {task_id} is lower, \
             so the original id cannot be recovered unambiguously"
        )));
    }
    Ok(task_id)
}

/// One stored usage event with its immutable durable order (`event_seq`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredUsageEvent {
    pub event_seq: i64,
    pub event: UsageEvent,
}

/// The SQL schema of the durable billing-report schedule (migration v4 of
/// the control-plane ladder). Append-only in spirit: rows advance through
/// their open -> reported/failed/skipped states, and a terminal period is
/// never re-opened (the runner never re-opens it), so a period can never be
/// double-reported even across a crash or restart.
pub const BILLING_REPORT_SCHEMA_V4: &str = "
     CREATE TABLE IF NOT EXISTS billing_report_period (
        organization_id TEXT NOT NULL,
        period TEXT NOT NULL,
        status TEXT NOT NULL,
        next_attempt_at_ms INTEGER NOT NULL,
        first_seen_ms INTEGER NOT NULL,
        payload TEXT NOT NULL,
        PRIMARY KEY (organization_id, period)
     );
     CREATE INDEX IF NOT EXISTS idx_billing_report_period_due
        ON billing_report_period(organization_id, status, next_attempt_at_ms);
";

/// The status of one reporting period.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportPeriodStatus {
    /// Open: not reported yet (possibly mid-report with a continuation
    /// cursor, or waiting for its retry backoff).
    Open,
    /// Reported to completion; terminal (never double-reported).
    Reported,
    /// Terminally failed (a final vendor refusal or exhausted attempts);
    /// never retried.
    Failed,
    /// Permanently skipped: the schedule was down long enough that this
    /// period fell outside the bounded catch-up window. The row IS the
    /// durable audit record (`last_error` names the skipped range, and
    /// `skipped_at_ms` records when the decision was made) — a missed
    /// period is never silently ignored.
    Skipped,
}

impl ReportPeriodStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            ReportPeriodStatus::Open => "open",
            ReportPeriodStatus::Reported => "reported",
            ReportPeriodStatus::Failed => "failed",
            ReportPeriodStatus::Skipped => "skipped",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "open" => Some(ReportPeriodStatus::Open),
            "reported" => Some(ReportPeriodStatus::Reported),
            "failed" => Some(ReportPeriodStatus::Failed),
            "skipped" => Some(ReportPeriodStatus::Skipped),
            _ => None,
        }
    }

    /// Whether the status is terminal (a terminal period is never
    /// re-opened or re-sent).
    pub const fn is_terminal(self) -> bool {
        !matches!(self, ReportPeriodStatus::Open)
    }
}

/// One durable report period row: the schedule's unit of exactly-once
/// reporting. `next_cursor` is the vendor continuation cursor INSIDE the
/// period (recorded before the next page is sent, so a crash replays the
/// same page key and the vendor de-duplicates).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportPeriodRow {
    pub organization_id: String,
    pub period: String,
    #[serde(default)]
    pub next_cursor: Option<String>,
    pub status: ReportPeriodStatus,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub last_error: Option<String>,
    pub first_seen_ms: i64,
    #[serde(default)]
    pub reported_at_ms: Option<i64>,
    pub next_attempt_at_ms: i64,
    /// When a [`ReportPeriodStatus::Skipped`] audit decision was made
    /// (never set for any other status).
    #[serde(default)]
    pub skipped_at_ms: Option<i64>,
}

/// Bound on the retained report periods of ONE organization (oldest rows
/// are pruned on insert; the schedule is bounded over an unbounded lifetime).
pub const MAX_REPORT_PERIODS_PER_ORG: usize = 512;

/// One stored credit entry with its immutable durable order (`entry_seq`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredCreditEntry {
    pub entry_seq: i64,
    pub entry: CreditEntry,
}

/// The outcome of one idempotent usage append.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageAppend {
    /// The event is durable (a new row).
    Appended,
    /// The `(organization, source_key)` fact was already recorded: nothing
    /// was written and nothing is double-counted.
    Duplicate,
}

/// The typed refusal of one credit append.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CreditAppendRefusal {
    #[error(
        "insufficient credits: {available} microUSD available, {requested} microUSD requested \
         (nothing was written)"
    )]
    InsufficientCredits { available: u64, requested: u64 },
    #[error("credit reference {0} does not exist")]
    UnknownReference(String),
    #[error("credit reference {reference} is a {kind}, not a consume")]
    NotAConsume { reference: String, kind: String },
    #[error("consume {reference} was already settled")]
    AlreadySettled { reference: String },
    #[error(
        "refund refused: {requested} microUSD exceeds the {refundable} microUSD left of consume \
         {reference} (nothing was written)"
    )]
    RefundExceedsConsumed {
        reference: String,
        refundable: u64,
        requested: u64,
    },
    #[error("the idempotency key {0:?} was already used for a different credit entry")]
    IdempotencyConflict(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BillingStoreError {
    #[error("billing store backend unavailable: {0}")]
    Backend(String),
    #[error("billing store refused a malformed row: {0}")]
    Malformed(String),
    #[error(transparent)]
    Credit(#[from] CreditAppendRefusal),
    /// A monetary aggregate left the `u64` domain or the ledger violates its
    /// own invariants: typed, never saturated (a saturated balance would hide
    /// debt from every guard).
    #[error(transparent)]
    Ledger(#[from] CreditLedgerError),
}

impl From<CloudStoreError> for BillingStoreError {
    fn from(e: CloudStoreError) -> Self {
        match e {
            CloudStoreError::Backend(m) => BillingStoreError::Backend(m),
            CloudStoreError::Malformed(m) => BillingStoreError::Malformed(m),
            CloudStoreError::Conflict(m) => BillingStoreError::Malformed(m),
        }
    }
}

impl From<BillingStoreError> for ControlPlaneError {
    fn from(e: BillingStoreError) -> Self {
        match e {
            BillingStoreError::Backend(m) => ControlPlaneError::Backend(m),
            BillingStoreError::Malformed(m) => ControlPlaneError::Malformed(m),
            BillingStoreError::Credit(CreditAppendRefusal::InsufficientCredits {
                available,
                requested,
            }) => ControlPlaneError::Conflict(format!(
                "insufficient credits: {available} microUSD available, {requested} requested"
            )),
            BillingStoreError::Credit(other) => ControlPlaneError::Conflict(other.to_string()),
            // A typed ledger refusal (overflow / corrupt invariant) stays
            // TYPED at the control-plane boundary: it is never retryable and
            // never flattened into a generic backend failure.
            BillingStoreError::Ledger(e) => ControlPlaneError::Ledger(e),
        }
    }
}

/// The durable billing seam. Object-safe: the entitlement service holds one
/// `Arc<dyn BillingStore>`.
pub trait BillingStore: Send + Sync {
    fn put_billing_account(&self, account: &BillingAccount) -> Result<(), BillingStoreError>;
    fn billing_account(
        &self,
        organization: &OrganizationId,
        id: &BillingAccountId,
    ) -> Result<Option<BillingAccount>, BillingStoreError>;
    fn billing_accounts(
        &self,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<BillingAccount>, BillingStoreError>;

    fn put_subscription(&self, subscription: &Subscription) -> Result<(), BillingStoreError>;
    fn subscription(
        &self,
        organization: &OrganizationId,
    ) -> Result<Option<Subscription>, BillingStoreError>;

    /// Append one usage event (idempotent per `(organization, source_key)`).
    fn append_usage_event(&self, event: &UsageEvent) -> Result<UsageAppend, BillingStoreError>;
    fn usage_event(
        &self,
        organization: &OrganizationId,
        id: &str,
    ) -> Result<Option<StoredUsageEvent>, BillingStoreError>;
    /// One ascending page of stored events (cursor = `event_seq`).
    fn usage_events(
        &self,
        organization: &OrganizationId,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<StoredUsageEvent>, BillingStoreError>;
    /// Every event of one organization with `task_id`, ascending (bounded
    /// by `limit`). Both implementations match the EXACT id: SQLite compares
    /// the canonical [`crate::billing::task_id_text`] encoding of the
    /// column, the memory store the raw `u64` — no projection may alias two
    /// ids.
    fn usage_events_of_task(
        &self,
        organization: &OrganizationId,
        task_id: u64,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<StoredUsageEvent>, BillingStoreError>;
    /// Whether a correction already names `base` (used to refuse double
    /// corrections; the fold itself tolerates chains).
    fn correction_exists(
        &self,
        organization: &OrganizationId,
        base: &str,
    ) -> Result<bool, BillingStoreError>;

    /// Append one credit entry under the store's balance/refund guards.
    fn append_credit_entry(&self, entry: &CreditEntry) -> Result<CreditAppend, BillingStoreError>;
    fn credit_entries(
        &self,
        organization: &OrganizationId,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<StoredCreditEntry>, BillingStoreError>;
    /// One recorded credit entry by its idempotency key (the DURABLE
    /// identity of a record-before-call consume), `None` when no entry of
    /// this organization claimed the key. Additive read used by the
    /// agent-side debit adapter to settle/refund the hold it opened (the
    /// service's append returns the key's outcome, not the entry id).
    fn credit_entry_by_idempotency_key(
        &self,
        organization: &OrganizationId,
        key: &str,
    ) -> Result<Option<StoredCreditEntry>, BillingStoreError>;
    fn credit_balance(
        &self,
        organization: &OrganizationId,
    ) -> Result<CreditBalance, BillingStoreError>;

    fn begin_in_flight(&self, txn: &InFlightTxn) -> Result<(), BillingStoreError>;
    fn end_in_flight(
        &self,
        organization: &OrganizationId,
        id: &InFlightTxnId,
        at_ms: i64,
    ) -> Result<bool, BillingStoreError>;
    fn in_flight(
        &self,
        organization: &OrganizationId,
    ) -> Result<Vec<InFlightTxn>, BillingStoreError>;

    /// Upsert one report period row (the record-before-send step: it is
    /// durable before any vendor call). Insertion prunes the organization's
    /// oldest rows beyond [`MAX_REPORT_PERIODS_PER_ORG`].
    fn put_report_period(&self, row: &ReportPeriodRow) -> Result<(), BillingStoreError>;
    /// One report period row (idempotency/restart resolution).
    fn report_period(
        &self,
        organization: &OrganizationId,
        period: &str,
    ) -> Result<Option<ReportPeriodRow>, BillingStoreError>;
    /// One bounded page of the organization's report rows, newest first.
    fn report_periods(
        &self,
        organization: &OrganizationId,
        limit: usize,
    ) -> Result<Vec<ReportPeriodRow>, BillingStoreError>;
}

/// The outcome of one credit append.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreditAppend {
    Appended,
    /// The event's idempotency key was already claimed: the recorded entry
    /// is replayed and nothing is double-debited.
    Duplicate,
}

/// Enforce the credit-amount domain bound at the persistence boundary
/// (defense in depth: the service validates too, but no store write may ever
/// project an out-of-domain amount into the ledger). The refusal is typed and
/// names the field and the limit; both backends refuse identically.
fn validate_credit_amount(entry: &CreditEntry) -> Result<(), BillingStoreError> {
    crate::billing::validate_credit_amount_micro(entry.amount_micro)
        .map_err(|e| BillingStoreError::Malformed(e.to_string()))
}

// ------------------------------------------------------------- in-memory

#[derive(Default)]
struct MemBilling {
    accounts: BTreeMap<String, BillingAccount>,
    subscriptions: BTreeMap<String, Subscription>,
    usage: BTreeMap<String, StoredUsageEvent>,
    usage_order: BTreeMap<String, Vec<String>>,
    credits: BTreeMap<String, StoredCreditEntry>,
    credit_order: BTreeMap<String, Vec<String>>,
    in_flight: BTreeMap<String, InFlightTxn>,
    report_periods: BTreeMap<String, ReportPeriodRow>,
}

/// In-memory [`BillingStore`] (tests and embedded hosts). Enforces exactly
/// the same append-only/idempotency/balance rules as the SQLite store.
#[derive(Default)]
pub struct MemoryBillingStore {
    inner: Mutex<MemBilling>,
}

impl MemoryBillingStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, MemBilling>, BillingStoreError> {
        self.inner
            .lock()
            .map_err(|_| BillingStoreError::Backend("in-memory billing lock is poisoned".into()))
    }
}

fn claim_credit(
    state: &mut MemBilling,
    entry: &CreditEntry,
) -> Result<CreditAppend, BillingStoreError> {
    let org = entry.organization.as_str().to_string();
    if let Some(key) = &entry.idempotency_key {
        if let Some(existing) = state.credits.values().find(|row| {
            row.entry.organization == entry.organization
                && row.entry.idempotency_key.as_deref() == Some(key.as_str())
        }) {
            if existing.entry.kind == entry.kind
                && existing.entry.amount_micro == entry.amount_micro
                && existing.entry.reference == entry.reference
                && existing.entry.billing_account_id == entry.billing_account_id
                && existing.entry.usage_event_id == entry.usage_event_id
            {
                return Ok(CreditAppend::Duplicate);
            }
            return Err(CreditAppendRefusal::IdempotencyConflict(key.clone()).into());
        }
    }
    let entries: Vec<CreditEntry> = match state.credit_order.get(&org) {
        Some(ids) => ids
            .iter()
            .filter_map(|id| state.credits.get(id).map(|row| row.entry.clone()))
            .collect(),
        None => Vec::new(),
    };
    let balance = fold_credits(&entries)?;
    match entry.kind {
        CreditKind::Grant => {}
        CreditKind::Consume => {
            let available = balance.balance_micro()?;
            if entry.amount_micro > available {
                return Err(CreditAppendRefusal::InsufficientCredits {
                    available,
                    requested: entry.amount_micro,
                }
                .into());
            }
        }
        CreditKind::Settle | CreditKind::Refund => {
            let reference = entry
                .reference
                .as_ref()
                .expect("validated: settle/refund carry a reference");
            let target = entries.iter().find(|row| &row.id == reference);
            let Some(target) = target else {
                return Err(
                    CreditAppendRefusal::UnknownReference(reference.as_str().to_string()).into(),
                );
            };
            if target.kind != CreditKind::Consume {
                return Err(CreditAppendRefusal::NotAConsume {
                    reference: reference.as_str().to_string(),
                    kind: target.kind.as_str().to_string(),
                }
                .into());
            }
            let settle = entries.iter().find(|row| {
                row.kind == CreditKind::Settle && row.reference.as_ref() == Some(reference)
            });
            if entry.kind == CreditKind::Settle {
                if settle.is_some() {
                    return Err(CreditAppendRefusal::AlreadySettled {
                        reference: reference.as_str().to_string(),
                    }
                    .into());
                }
                // Only the DELTA above the pending hold must be covered. The
                // hold is ALREADY inside `balance_micro()` (an unsettled
                // consume counts as consumed at its held amount), so adding
                // `target.amount_micro` back would double-count it and let a
                // settle spend free + hold.
                let delta = entry.amount_micro.saturating_sub(target.amount_micro);
                let available = balance.balance_micro()?;
                if delta > available {
                    return Err(CreditAppendRefusal::InsufficientCredits {
                        available,
                        requested: delta,
                    }
                    .into());
                }
            } else {
                // Refund exactness: what the consume actually spent (its
                // settle amount when settled, else the pending hold) minus
                // everything already refunded for it. Checked: a ledger whose
                // refunds exceed the spend is refused typed, never masked to
                // a zero refundable amount.
                let spent = settle
                    .map(|s| s.amount_micro)
                    .unwrap_or(target.amount_micro);
                let mut refunded: u64 = 0;
                for row in entries.iter().filter(|row| {
                    row.kind == CreditKind::Refund && row.reference.as_ref() == Some(reference)
                }) {
                    refunded = checked_ledger_sum("refunded_micro", refunded, row.amount_micro)?;
                }
                let refundable =
                    spent
                        .checked_sub(refunded)
                        .ok_or_else(|| CreditLedgerError::Corrupt {
                            detail: format!(
                            "consume {} has {refunded} microUSD refunded against {spent} microUSD \
                             spent",
                            reference.as_str()
                        ),
                        })?;
                if entry.amount_micro > refundable {
                    return Err(CreditAppendRefusal::RefundExceedsConsumed {
                        reference: reference.as_str().to_string(),
                        refundable,
                        requested: entry.amount_micro,
                    }
                    .into());
                }
            }
        }
    }
    // The entry's own contribution must not leave the u64 domain: refused
    // typed before the write (nothing is ever written by a refusal).
    checked_append_domain(entry, &balance)?;
    let seq = state.credit_order.get(&org).map(|v| v.len()).unwrap_or(0) as i64 + 1;
    state.credits.insert(
        entry.id.as_str().to_string(),
        StoredCreditEntry {
            entry_seq: seq,
            entry: entry.clone(),
        },
    );
    state
        .credit_order
        .entry(org)
        .or_default()
        .push(entry.id.as_str().to_string());
    Ok(CreditAppend::Appended)
}

impl BillingStore for MemoryBillingStore {
    fn put_billing_account(&self, account: &BillingAccount) -> Result<(), BillingStoreError> {
        self.lock()?
            .accounts
            .insert(account.id.as_str().to_string(), account.clone());
        Ok(())
    }

    fn billing_account(
        &self,
        organization: &OrganizationId,
        id: &BillingAccountId,
    ) -> Result<Option<BillingAccount>, BillingStoreError> {
        Ok(self
            .lock()?
            .accounts
            .get(id.as_str())
            .filter(|a| a.organization == *organization)
            .cloned())
    }

    fn billing_accounts(
        &self,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<BillingAccount>, BillingStoreError> {
        Ok(self
            .lock()?
            .accounts
            .values()
            .filter(|a| a.organization == *organization)
            .filter(|a| after.map(|c| a.id.as_str() > c).unwrap_or(true))
            .take(limit)
            .cloned()
            .collect())
    }

    fn put_subscription(&self, subscription: &Subscription) -> Result<(), BillingStoreError> {
        self.lock()?.subscriptions.insert(
            subscription.organization.as_str().to_string(),
            subscription.clone(),
        );
        Ok(())
    }

    fn subscription(
        &self,
        organization: &OrganizationId,
    ) -> Result<Option<Subscription>, BillingStoreError> {
        Ok(self
            .lock()?
            .subscriptions
            .get(organization.as_str())
            .cloned())
    }

    fn append_usage_event(&self, event: &UsageEvent) -> Result<UsageAppend, BillingStoreError> {
        let mut state = self.lock()?;
        let duplicate = state.usage.values().any(|row| {
            row.event.organization_id == event.organization_id
                && row.event.source_key == event.source_key
        });
        if duplicate {
            return Ok(UsageAppend::Duplicate);
        }
        let org = event.organization_id.as_str().to_string();
        let seq = state.usage_order.get(&org).map(|v| v.len()).unwrap_or(0) as i64 + 1;
        state.usage.insert(
            event.id.as_str().to_string(),
            StoredUsageEvent {
                event_seq: seq,
                event: event.clone(),
            },
        );
        state
            .usage_order
            .entry(org)
            .or_default()
            .push(event.id.as_str().to_string());
        Ok(UsageAppend::Appended)
    }

    fn usage_event(
        &self,
        organization: &OrganizationId,
        id: &str,
    ) -> Result<Option<StoredUsageEvent>, BillingStoreError> {
        Ok(self
            .lock()?
            .usage
            .get(id)
            .filter(|row| row.event.organization_id == *organization)
            .cloned())
    }

    fn usage_events(
        &self,
        organization: &OrganizationId,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<StoredUsageEvent>, BillingStoreError> {
        let state = self.lock()?;
        let org = organization.as_str();
        let mut rows: Vec<StoredUsageEvent> = state
            .usage_order
            .get(org)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| state.usage.get(id).cloned())
                    .filter(|row| row.event_seq > after_seq)
                    .take(limit)
                    .collect()
            })
            .unwrap_or_default();
        rows.sort_by_key(|row| row.event_seq);
        Ok(rows)
    }

    fn usage_events_of_task(
        &self,
        organization: &OrganizationId,
        task_id: u64,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<StoredUsageEvent>, BillingStoreError> {
        let state = self.lock()?;
        let mut rows: Vec<StoredUsageEvent> = state
            .usage
            .values()
            .filter(|row| {
                row.event.organization_id == *organization && row.event.task_id == task_id
            })
            .filter(|row| row.event_seq > after_seq)
            .cloned()
            .collect();
        rows.sort_by_key(|row| row.event_seq);
        rows.truncate(limit);
        Ok(rows)
    }

    fn correction_exists(
        &self,
        organization: &OrganizationId,
        base: &str,
    ) -> Result<bool, BillingStoreError> {
        Ok(self.lock()?.usage.values().any(|row| {
            row.event.organization_id == *organization
                && row.event.correction_of.as_ref().map(|c| c.as_str()) == Some(base)
        }))
    }

    fn append_credit_entry(&self, entry: &CreditEntry) -> Result<CreditAppend, BillingStoreError> {
        validate_credit_amount(entry)?;
        let mut state = self.lock()?;
        claim_credit(&mut state, entry)
    }

    fn credit_entries(
        &self,
        organization: &OrganizationId,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<StoredCreditEntry>, BillingStoreError> {
        let state = self.lock()?;
        let mut rows: Vec<StoredCreditEntry> = state
            .credit_order
            .get(organization.as_str())
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| state.credits.get(id).cloned())
                    .filter(|row| row.entry_seq > after_seq)
                    .take(limit)
                    .collect()
            })
            .unwrap_or_default();
        rows.sort_by_key(|row| row.entry_seq);
        Ok(rows)
    }

    fn credit_entry_by_idempotency_key(
        &self,
        organization: &OrganizationId,
        key: &str,
    ) -> Result<Option<StoredCreditEntry>, BillingStoreError> {
        let state = self.lock()?;
        Ok(state
            .credits
            .values()
            .find(|row| {
                row.entry.organization == *organization
                    && row.entry.idempotency_key.as_deref() == Some(key)
            })
            .cloned())
    }

    fn credit_balance(
        &self,
        organization: &OrganizationId,
    ) -> Result<CreditBalance, BillingStoreError> {
        let state = self.lock()?;
        let entries: Vec<CreditEntry> = state
            .credit_order
            .get(organization.as_str())
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| state.credits.get(id).map(|row| row.entry.clone()))
                    .collect()
            })
            .unwrap_or_default();
        Ok(fold_credits(&entries)?)
    }

    fn begin_in_flight(&self, txn: &InFlightTxn) -> Result<(), BillingStoreError> {
        self.lock()?
            .in_flight
            .insert(txn.id.as_str().to_string(), txn.clone());
        Ok(())
    }

    fn end_in_flight(
        &self,
        organization: &OrganizationId,
        id: &InFlightTxnId,
        at_ms: i64,
    ) -> Result<bool, BillingStoreError> {
        let mut state = self.lock()?;
        let Some(row) = state.in_flight.get_mut(id.as_str()) else {
            return Ok(false);
        };
        if row.organization != *organization {
            return Ok(false);
        }
        // Close exactly once: a second close changes nothing and reports
        // `false` (the same contract the SQLite store's guarded UPDATE has).
        if row.ended_ms.is_some() {
            return Ok(false);
        }
        row.ended_ms = Some(at_ms);
        Ok(true)
    }

    fn in_flight(
        &self,
        organization: &OrganizationId,
    ) -> Result<Vec<InFlightTxn>, BillingStoreError> {
        Ok(self
            .lock()?
            .in_flight
            .values()
            .filter(|row| row.organization == *organization)
            .cloned()
            .collect())
    }

    fn put_report_period(&self, row: &ReportPeriodRow) -> Result<(), BillingStoreError> {
        let mut state = self.lock()?;
        let key = report_key(&row.organization_id, &row.period);
        state.report_periods.insert(key, row.clone());
        prune_report_periods(
            &mut state.report_periods,
            row.organization_id.as_str(),
            MAX_REPORT_PERIODS_PER_ORG,
        );
        Ok(())
    }

    fn report_period(
        &self,
        organization: &OrganizationId,
        period: &str,
    ) -> Result<Option<ReportPeriodRow>, BillingStoreError> {
        Ok(self
            .lock()?
            .report_periods
            .get(&report_key(organization.as_str(), period))
            .cloned())
    }

    fn report_periods(
        &self,
        organization: &OrganizationId,
        limit: usize,
    ) -> Result<Vec<ReportPeriodRow>, BillingStoreError> {
        let state = self.lock()?;
        let mut rows: Vec<ReportPeriodRow> = state
            .report_periods
            .values()
            .filter(|row| row.organization_id == organization.as_str())
            .cloned()
            .collect();
        rows.sort_by(|a, b| {
            b.first_seen_ms
                .cmp(&a.first_seen_ms)
                .then_with(|| b.period.cmp(&a.period))
        });
        rows.truncate(limit);
        Ok(rows)
    }
}

fn report_key(organization: &str, period: &str) -> String {
    format!("{organization}\0{period}")
}

/// Prune one organization's report rows to the newest `bound` (by first
/// sighting, then period): the schedule stays bounded over an unbounded
/// lifetime. Terminal rows are pruned first so an open period or a skipped
/// audit row is never silently dropped while an old terminal row survives.
fn prune_report_periods(
    rows: &mut BTreeMap<String, ReportPeriodRow>,
    organization: &str,
    bound: usize,
) {
    let mut mine: Vec<(String, bool, i64, String)> = rows
        .iter()
        .filter(|(_, row)| row.organization_id == organization)
        .map(|(key, row)| {
            (
                key.clone(),
                row.status == ReportPeriodStatus::Open || row.status == ReportPeriodStatus::Skipped,
                row.first_seen_ms,
                row.period.clone(),
            )
        })
        .collect();
    if mine.len() <= bound {
        return;
    }
    // Oldest first, terminal before open within the same age (an open
    // period is the more valuable row to keep).
    mine.sort_by(|a, b| {
        a.2.cmp(&b.2)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.3.cmp(&b.3))
    });
    let excess = mine.len().saturating_sub(bound);
    for (key, _, _, _) in mine.into_iter().take(excess) {
        rows.remove(&key);
    }
}

// ---------------------------------------------------------------- sqlite

fn backend(e: rusqlite::Error) -> BillingStoreError {
    BillingStoreError::Backend(e.to_string())
}

fn parse<T: for<'de> Deserialize<'de>>(payload: &str) -> Result<T, BillingStoreError> {
    serde_json::from_str(payload)
        .map_err(|e| BillingStoreError::Malformed(format!("billing row payload: {e}")))
}

fn encode<T: Serialize>(value: &T) -> Result<String, BillingStoreError> {
    serde_json::to_string(value)
        .map_err(|e| BillingStoreError::Malformed(format!("billing row encode: {e}")))
}

/// One matched `credit_entry` idempotency row: (id, kind, stored
/// `amount_micro` column, reference, billing_account_id, usage_event_id,
/// payload). The payload is carried so the identity comparison uses the
/// EXACT amount authority, never the signed column projection.
type CreditEntryKeyRow = (
    String,
    String,
    i64,
    Option<String>,
    String,
    Option<String>,
    String,
);

impl SqliteControlPlaneStore {
    /// Run one credit append inside an IMMEDIATE transaction: the balance is
    /// re-derived from the durable rows and the guards applied before the
    /// insert, so concurrent writers can never overdraw or over-refund.
    fn credit_append_tx(
        conn: &mut Connection,
        entry: &CreditEntry,
    ) -> Result<CreditAppend, BillingStoreError> {
        // The amount domain bound is enforced BEFORE the transaction opens:
        // an out-of-domain amount is refused typed and writes nothing.
        validate_credit_amount(entry)?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(backend)?;
        if let Some(key) = &entry.idempotency_key {
            let existing: Option<CreditEntryKeyRow> = tx
                .query_row(
                    "SELECT id, kind, amount_micro, reference, billing_account_id, usage_event_id,
                            payload
                     FROM credit_entry
                     WHERE organization_id = ?1 AND idempotency_key = ?2",
                    params![entry.organization.as_str(), key],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                        ))
                    },
                )
                .optional()
                .map_err(backend)?;
            if let Some((id, kind, amount, reference, account, usage_event, payload)) = existing {
                // The payload amount is the authority (and a legacy clamped
                // row is refused typed here too, before any comparison).
                let stored = decode_credit_row(&id, amount, &payload)?;
                let same = kind == entry.kind.as_str()
                    && stored.amount_micro == entry.amount_micro
                    && reference.as_deref() == entry.reference.as_ref().map(|r| r.as_str())
                    && account == entry.billing_account_id.as_str()
                    && usage_event.as_deref() == entry.usage_event_id.as_ref().map(|u| u.as_str());
                if same {
                    tx.commit().map_err(backend)?;
                    return Ok(CreditAppend::Duplicate);
                }
                return Err(BillingStoreError::Credit(
                    CreditAppendRefusal::IdempotencyConflict(key.clone()),
                ));
            }
        }
        let entries = read_credit_entries(&tx, &entry.organization)?;
        let balance = fold_credits(&entries)?;
        match entry.kind {
            CreditKind::Grant => {}
            CreditKind::Consume => {
                let available = balance.balance_micro()?;
                if entry.amount_micro > available {
                    return Err(BillingStoreError::Credit(
                        CreditAppendRefusal::InsufficientCredits {
                            available,
                            requested: entry.amount_micro,
                        },
                    ));
                }
            }
            CreditKind::Settle | CreditKind::Refund => {
                let reference = entry
                    .reference
                    .as_ref()
                    .expect("validated: settle/refund carry a reference");
                let target = entries.iter().find(|row| &row.id == reference);
                let Some(target) = target else {
                    return Err(BillingStoreError::Credit(
                        CreditAppendRefusal::UnknownReference(reference.as_str().to_string()),
                    ));
                };
                if target.kind != CreditKind::Consume {
                    return Err(BillingStoreError::Credit(
                        CreditAppendRefusal::NotAConsume {
                            reference: reference.as_str().to_string(),
                            kind: target.kind.as_str().to_string(),
                        },
                    ));
                }
                let settle = entries.iter().find(|row| {
                    row.kind == CreditKind::Settle && row.reference.as_ref() == Some(reference)
                });
                if entry.kind == CreditKind::Settle {
                    if settle.is_some() {
                        return Err(BillingStoreError::Credit(
                            CreditAppendRefusal::AlreadySettled {
                                reference: reference.as_str().to_string(),
                            },
                        ));
                    }
                    let delta = entry.amount_micro.saturating_sub(target.amount_micro);
                    // The pending hold is ALREADY inside `balance_micro()`
                    // (an unsettled consume counts as consumed at its held
                    // amount), so only the delta above it must be covered.
                    // Adding `target.amount_micro` back would double-count
                    // the hold and let a settle spend free + hold.
                    let available = balance.balance_micro()?;
                    if delta > available {
                        return Err(BillingStoreError::Credit(
                            CreditAppendRefusal::InsufficientCredits {
                                available,
                                requested: delta,
                            },
                        ));
                    }
                } else {
                    let spent = settle
                        .map(|s| s.amount_micro)
                        .unwrap_or(target.amount_micro);
                    // Checked: a ledger whose refunds exceed the spend is
                    // refused typed, never masked to a zero refundable
                    // amount.
                    let mut refunded: u64 = 0;
                    for row in entries.iter().filter(|row| {
                        row.kind == CreditKind::Refund && row.reference.as_ref() == Some(reference)
                    }) {
                        refunded =
                            checked_ledger_sum("refunded_micro", refunded, row.amount_micro)?;
                    }
                    let refundable =
                        spent
                            .checked_sub(refunded)
                            .ok_or_else(|| CreditLedgerError::Corrupt {
                                detail: format!(
                                    "consume {} has {refunded} microUSD refunded against {spent} \
                                 microUSD spent",
                                    reference.as_str()
                                ),
                            })?;
                    if entry.amount_micro > refundable {
                        return Err(BillingStoreError::Credit(
                            CreditAppendRefusal::RefundExceedsConsumed {
                                reference: reference.as_str().to_string(),
                                refundable,
                                requested: entry.amount_micro,
                            },
                        ));
                    }
                }
            }
        }
        // The entry's own contribution must not leave the u64 domain: refused
        // typed before the write (the transaction rolls back, nothing is
        // written).
        checked_append_domain(entry, &balance)?;
        let seq: i64 = tx
            .query_row(
                "INSERT INTO billing_credit_seq (organization_id, next_seq) VALUES (?1, 1)
                 ON CONFLICT(organization_id) DO UPDATE SET next_seq = next_seq + 1
                 RETURNING next_seq",
                params![entry.organization.as_str()],
                |r| r.get(0),
            )
            .map_err(backend)?;
        // The domain bound (validated above) makes the signed projection
        // EXACT. The conversion is still checked, never `as`: no future path
        // can silently reintroduce a clamp into the monetary column.
        let amount_i64 = i64::try_from(entry.amount_micro).map_err(|_| {
            BillingStoreError::Malformed(format!(
                "credit entry {} amount_micro {} exceeds the durable i64::MAX limit {}",
                entry.id.as_str(),
                entry.amount_micro,
                crate::billing::MAX_CREDIT_AMOUNT_MICRO
            ))
        })?;
        tx.execute(
            "INSERT INTO credit_entry
                (id, organization_id, entry_seq, billing_account_id, kind, reference,
                 usage_event_id, idempotency_key, amount_micro, occurred_at_ms, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                entry.id.as_str(),
                entry.organization.as_str(),
                seq,
                entry.billing_account_id.as_str(),
                entry.kind.as_str(),
                entry.reference.as_ref().map(|r| r.as_str()),
                entry.usage_event_id.as_ref().map(|u| u.as_str()),
                entry.idempotency_key.as_deref(),
                amount_i64,
                entry.occurred_at_ms,
                encode(entry)?,
            ],
        )
        .map_err(backend)?;
        tx.commit().map_err(backend)?;
        Ok(CreditAppend::Appended)
    }
}

/// Decode one `credit_entry` row and verify its durable amount projection.
///
/// Invariant (P1 ledger-consistency): the stored `amount_micro` column is
/// the EXACT signed projection of the payload's `amount_micro`, whose domain
/// is bounded by [`crate::billing::MAX_CREDIT_AMOUNT_MICRO`] (`i64::MAX`), so
/// column and payload are bit-identical for every row the current writer
/// produces. A row written by the old clamping writer can still hold the
/// sentinel `i64::MAX` while the payload carries the true (out-of-domain)
/// amount. Such a row is REFUSED typed here, naming the row id and both
/// values — never silently reinterpreted as the clamped `i64::MAX`, and
/// never folded into a balance/audit/aggregation number.
///
/// No in-place migration rewrites these rows: the true amount is outside the
/// bounded domain, so there is no exact in-domain value to migrate to and
/// any rewrite would be a guess. The refusal names the row so an operator
/// can verify the ledger; this is the documented decision (the v6 task-id
/// precedent: refuse rather than guess).
fn decode_credit_row(
    id: &str,
    stored_amount: i64,
    payload: &str,
) -> Result<CreditEntry, BillingStoreError> {
    let entry: CreditEntry = parse(payload)?;
    if u64::try_from(stored_amount).ok() != Some(entry.amount_micro) {
        return Err(BillingStoreError::Malformed(format!(
            "credit_entry {id}: stored amount_micro {stored_amount} disagrees with the payload \
             amount_micro {} (legacy clamped row written before the i64::MAX domain bound); \
             refusing to reinterpret the ledger amount (operator verification required)",
            entry.amount_micro
        )));
    }
    Ok(entry)
}

fn read_credit_entries(
    conn: &Connection,
    organization: &OrganizationId,
) -> Result<Vec<CreditEntry>, BillingStoreError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, amount_micro, payload FROM credit_entry
             WHERE organization_id = ?1 ORDER BY entry_seq",
        )
        .map_err(backend)?;
    let rows = stmt
        .query_map(params![organization.as_str()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .map_err(backend)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(backend)?;
    let mut out = Vec::with_capacity(rows.len());
    for (id, amount, payload) in rows {
        out.push(decode_credit_row(&id, amount, &payload)?);
    }
    Ok(out)
}

impl BillingStore for SqliteControlPlaneStore {
    fn put_billing_account(&self, account: &BillingAccount) -> Result<(), BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM billing_account WHERE organization_id = ?1 AND id = ?2",
                params![account.organization.as_str(), account.id.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        match existing {
            Some(_) => {
                conn.execute(
                    "UPDATE billing_account SET payload = ?1 WHERE id = ?2",
                    params![encode(account)?, account.id.as_str()],
                )
                .map_err(backend)?;
            }
            None => {
                conn.execute(
                    "INSERT INTO billing_account (id, organization_id, payload) VALUES (?1, ?2, ?3)",
                    params![account.id.as_str(), account.organization.as_str(), encode(account)?],
                )
                .map_err(backend)?;
            }
        }
        Ok(())
    }

    fn billing_account(
        &self,
        organization: &OrganizationId,
        id: &BillingAccountId,
    ) -> Result<Option<BillingAccount>, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM billing_account WHERE organization_id = ?1 AND id = ?2",
                params![organization.as_str(), id.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        payload.map(|p| parse(&p)).transpose()
    }

    fn billing_accounts(
        &self,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<BillingAccount>, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT payload FROM billing_account
                 WHERE organization_id = ?1 AND (?2 IS NULL OR id > ?2)
                 ORDER BY id LIMIT ?3",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(params![organization.as_str(), after, limit as i64], |r| {
                r.get::<_, String>(0)
            })
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        rows.iter().map(|p| parse(p)).collect()
    }

    fn put_subscription(&self, subscription: &Subscription) -> Result<(), BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        conn.execute(
            "INSERT INTO billing_subscription (id, organization_id, payload) VALUES (?1, ?2, ?3)
             ON CONFLICT(organization_id) DO UPDATE SET
                id = excluded.id, payload = excluded.payload",
            params![
                subscription.id.as_str(),
                subscription.organization.as_str(),
                encode(subscription)?,
            ],
        )
        .map_err(backend)?;
        Ok(())
    }

    fn subscription(
        &self,
        organization: &OrganizationId,
    ) -> Result<Option<Subscription>, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM billing_subscription WHERE organization_id = ?1",
                params![organization.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        payload.map(|p| parse(&p)).transpose()
    }

    fn append_usage_event(&self, event: &UsageEvent) -> Result<UsageAppend, BillingStoreError> {
        let mut conn = self.lock_billing_conn()?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(backend)?;
        let duplicate: Option<String> = tx
            .query_row(
                "SELECT id FROM usage_event WHERE organization_id = ?1 AND source_key = ?2",
                params![event.organization_id.as_str(), event.source_key],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        if duplicate.is_some() {
            tx.commit().map_err(backend)?;
            return Ok(UsageAppend::Duplicate);
        }
        let seq: i64 = tx
            .query_row(
                "INSERT INTO usage_event_seq (organization_id, next_seq) VALUES (?1, 1)
                 ON CONFLICT(organization_id) DO UPDATE SET next_seq = next_seq + 1
                 RETURNING next_seq",
                params![event.organization_id.as_str()],
                |r| r.get(0),
            )
            .map_err(backend)?;
        tx.execute(
            "INSERT INTO usage_event
                (id, organization_id, event_seq, source_key, billing_account_id, task_id,
                 run_id, attempt_id, category, correction_of, occurred_at_ms,
                 reconciliation_state, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                event.id.as_str(),
                event.organization_id.as_str(),
                seq,
                event.source_key,
                event.billing_account_id.as_str(),
                crate::billing::task_id_text(event.task_id),
                event.run_id,
                event.attempt_id,
                event.category.as_str(),
                event.correction_of.as_ref().map(|c| c.as_str()),
                event.occurred_at_ms,
                event.reconciliation_state.as_str(),
                encode(event)?,
            ],
        )
        .map_err(backend)?;
        tx.commit().map_err(backend)?;
        Ok(UsageAppend::Appended)
    }

    fn usage_event(
        &self,
        organization: &OrganizationId,
        id: &str,
    ) -> Result<Option<StoredUsageEvent>, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        let row: Option<(i64, String)> = conn
            .query_row(
                "SELECT event_seq, payload FROM usage_event
                 WHERE organization_id = ?1 AND id = ?2",
                params![organization.as_str(), id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(backend)?;
        match row {
            Some((event_seq, payload)) => Ok(Some(StoredUsageEvent {
                event_seq,
                event: parse(&payload)?,
            })),
            None => Ok(None),
        }
    }

    fn usage_events(
        &self,
        organization: &OrganizationId,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<StoredUsageEvent>, BillingStoreError> {
        self.usage_page(organization, None, after_seq, limit)
    }

    fn usage_events_of_task(
        &self,
        organization: &OrganizationId,
        task_id: u64,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<StoredUsageEvent>, BillingStoreError> {
        self.usage_page(organization, Some(task_id), after_seq, limit)
    }

    fn correction_exists(
        &self,
        organization: &OrganizationId,
        base: &str,
    ) -> Result<bool, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_event
                 WHERE organization_id = ?1 AND correction_of = ?2",
                params![organization.as_str(), base],
                |r| r.get(0),
            )
            .map_err(backend)?;
        Ok(count > 0)
    }

    fn append_credit_entry(&self, entry: &CreditEntry) -> Result<CreditAppend, BillingStoreError> {
        let mut conn = self.lock_billing_conn()?;
        Self::credit_append_tx(&mut conn, entry)
    }

    fn credit_entries(
        &self,
        organization: &OrganizationId,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<StoredCreditEntry>, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT entry_seq, id, amount_micro, payload FROM credit_entry
                 WHERE organization_id = ?1 AND entry_seq > ?2
                 ORDER BY entry_seq LIMIT ?3",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(
                params![organization.as_str(), after_seq, limit as i64],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                },
            )
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        let mut out = Vec::with_capacity(rows.len());
        for (entry_seq, id, amount, payload) in rows {
            out.push(StoredCreditEntry {
                entry_seq,
                entry: decode_credit_row(&id, amount, &payload)?,
            });
        }
        Ok(out)
    }

    fn credit_entry_by_idempotency_key(
        &self,
        organization: &OrganizationId,
        key: &str,
    ) -> Result<Option<StoredCreditEntry>, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        let row = conn
            .query_row(
                "SELECT entry_seq, id, amount_micro, payload FROM credit_entry
                 WHERE organization_id = ?1 AND idempotency_key = ?2",
                params![organization.as_str(), key],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(backend)?;
        match row {
            Some((entry_seq, id, amount, payload)) => Ok(Some(StoredCreditEntry {
                entry_seq,
                entry: decode_credit_row(&id, amount, &payload)?,
            })),
            None => Ok(None),
        }
    }

    fn credit_balance(
        &self,
        organization: &OrganizationId,
    ) -> Result<CreditBalance, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        Ok(fold_credits(&read_credit_entries(&conn, organization)?)?)
    }

    fn begin_in_flight(&self, txn: &InFlightTxn) -> Result<(), BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        conn.execute(
            "INSERT INTO billing_in_flight (id, organization_id, kind, reference, started_ms, ended_ms, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET payload = excluded.payload",
            params![
                txn.id.as_str(),
                txn.organization.as_str(),
                txn.kind.as_str(),
                txn.reference,
                txn.started_ms,
                txn.ended_ms,
                encode(txn)?,
            ],
        )
        .map_err(backend)?;
        Ok(())
    }

    fn end_in_flight(
        &self,
        organization: &OrganizationId,
        id: &InFlightTxnId,
        at_ms: i64,
    ) -> Result<bool, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        let updated = conn
            .execute(
                "UPDATE billing_in_flight SET ended_ms = ?1
                 WHERE id = ?2 AND organization_id = ?3 AND ended_ms IS NULL",
                params![at_ms, id.as_str(), organization.as_str()],
            )
            .map_err(backend)?;
        Ok(updated == 1)
    }

    fn in_flight(
        &self,
        organization: &OrganizationId,
    ) -> Result<Vec<InFlightTxn>, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT payload FROM billing_in_flight
                 WHERE organization_id = ?1 ORDER BY started_ms, id",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(params![organization.as_str()], |r| r.get::<_, String>(0))
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        rows.iter().map(|p| parse(p)).collect()
    }

    fn put_report_period(&self, row: &ReportPeriodRow) -> Result<(), BillingStoreError> {
        let payload = encode(row)?;
        let mut conn = self.lock_billing_conn()?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(backend)?;
        tx.execute(
            "INSERT INTO billing_report_period
                (organization_id, period, status, next_attempt_at_ms, first_seen_ms, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(organization_id, period) DO UPDATE SET
                status = excluded.status,
                next_attempt_at_ms = excluded.next_attempt_at_ms,
                first_seen_ms = excluded.first_seen_ms,
                payload = excluded.payload",
            params![
                row.organization_id,
                row.period,
                row.status.as_str(),
                row.next_attempt_at_ms,
                row.first_seen_ms,
                payload,
            ],
        )
        .map_err(backend)?;
        // Bounded schedule: prune the organization's oldest rows beyond the
        // bound (reported/failed rows first at the same age; open periods
        // and skipped audit rows are retained preferentially).
        tx.execute(
            "DELETE FROM billing_report_period WHERE organization_id = ?1 AND period NOT IN (
                 SELECT period FROM billing_report_period WHERE organization_id = ?1
                 ORDER BY (status IN ('open', 'skipped')) DESC, first_seen_ms DESC, period DESC
                 LIMIT ?2)",
            params![row.organization_id, MAX_REPORT_PERIODS_PER_ORG as i64],
        )
        .map_err(backend)?;
        tx.commit().map_err(backend)?;
        Ok(())
    }

    fn report_period(
        &self,
        organization: &OrganizationId,
        period: &str,
    ) -> Result<Option<ReportPeriodRow>, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM billing_report_period
                 WHERE organization_id = ?1 AND period = ?2",
                params![organization.as_str(), period],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        payload.map(|p| parse(&p)).transpose()
    }

    fn report_periods(
        &self,
        organization: &OrganizationId,
        limit: usize,
    ) -> Result<Vec<ReportPeriodRow>, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT payload FROM billing_report_period
                 WHERE organization_id = ?1
                 ORDER BY first_seen_ms DESC, period DESC LIMIT ?2",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(params![organization.as_str(), limit as i64], |r| {
                r.get::<_, String>(0)
            })
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        rows.iter().map(|p| parse(p)).collect()
    }
}

impl SqliteControlPlaneStore {
    fn lock_billing_conn(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, Connection>, BillingStoreError> {
        self.lock().map_err(BillingStoreError::from)
    }

    fn usage_page(
        &self,
        organization: &OrganizationId,
        task_id: Option<u64>,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<StoredUsageEvent>, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        // The task filter compares the reversible text encoding
        // (`crate::billing::task_id_text`), NEVER a numeric projection: the
        // old `task_id.min(i64::MAX as u64) as i64` comparison aliased every
        // id >= i64::MAX onto one value, so a filtered page could return
        // another task's rows. Two statements (rather than an
        // `? IS NULL OR task_id = ?` predicate) keep the
        // `(organization_id, task_id, event_seq)` index applicable to the
        // filtered page.
        let rows: Vec<(i64, String)> = match task_id {
            Some(task_id) => {
                let mut stmt = conn
                    .prepare(
                        "SELECT event_seq, payload FROM usage_event
                         WHERE organization_id = ?1 AND task_id = ?2 AND event_seq > ?3
                         ORDER BY event_seq LIMIT ?4",
                    )
                    .map_err(backend)?;
                let mapped = stmt
                    .query_map(
                        params![
                            organization.as_str(),
                            crate::billing::task_id_text(task_id),
                            after_seq,
                            limit as i64
                        ],
                        usage_row,
                    )
                    .map_err(backend)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(backend)?;
                mapped
            }
            None => {
                let mut stmt = conn
                    .prepare(
                        "SELECT event_seq, payload FROM usage_event
                         WHERE organization_id = ?1 AND event_seq > ?2
                         ORDER BY event_seq LIMIT ?3",
                    )
                    .map_err(backend)?;
                let mapped = stmt
                    .query_map(
                        params![organization.as_str(), after_seq, limit as i64],
                        usage_row,
                    )
                    .map_err(backend)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(backend)?;
                mapped
            }
        };
        rows.into_iter()
            .map(|(event_seq, payload)| {
                Ok(StoredUsageEvent {
                    event_seq,
                    event: parse(&payload)?,
                })
            })
            .collect()
    }
}

/// Decode one `(event_seq, payload)` usage row.
fn usage_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, String)> {
    Ok((r.get(0)?, r.get(1)?))
}
