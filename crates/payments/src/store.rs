//! The durable payment/invoice store. One SQLite database (its own
//! `payment.db`, `PRAGMA user_version = 3`) owns customers, subscriptions,
//! invoices, payments, refunds, credit notes, payment methods, local-provider
//! objects, linked provider observations, operation claims, webhook
//! idempotency/watermarks and request-bound idempotency.
//!
//! Every mutation that matters to money runs in ONE immediate transaction:
//! the effect rows, the operation claim outcome and the idempotency record
//! commit together, so a crash either leaves neither (safe to retry with the
//! provider idempotency key) or both (a retry replays the recorded response).
//! A claim that crossed the provider boundary but never committed its effect
//! is recovered as `unknown` and only resolved by a read-only provider
//! reconciliation lookup.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{de::DeserializeOwned, Serialize};

use crate::authority::{
    InvoiceReconciliationState, ProviderInvoiceObservation, ProviderObservationSource,
    ProviderPaymentObservation, ProviderRefundObservation, ReconciliationSummary,
};
use crate::error::PaymentError;
use crate::model::{
    ClaimAcquisition, CreditNote, FinancialResourceType, Invoice, InvoicePage, OperationClaim,
    OperationClaimDraft, OperationClaimState, Payment, PaymentCustomer, PaymentSubscription,
    PendingProration, Refund, StoredPaymentMethod,
};
use crate::webhook::DEFAULT_WEBHOOK_WINDOW_MS;

/// The legacy schema version.
pub const PAYMENT_SCHEMA_V1: i64 = 1;
/// The operation-claim/refund-truth schema version.
pub const PAYMENT_SCHEMA_V2: i64 = 2;
/// The current schema version (adds linked provider observations and the
/// invoice reconciliation column while preserving every earlier row).
pub const PAYMENT_SCHEMA_VERSION: i64 = 3;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS payment_customer (
    organization_id TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    provider_customer_id TEXT NOT NULL,
    payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_payment_customer_provider
    ON payment_customer(provider, provider_customer_id);

CREATE TABLE IF NOT EXISTS payment_subscription (
    id TEXT PRIMARY KEY,
    organization_id TEXT NOT NULL UNIQUE,
    provider_subscription_id TEXT NOT NULL,
    plan_id TEXT NOT NULL,
    state TEXT NOT NULL,
    payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_payment_subscription_provider
    ON payment_subscription(provider_subscription_id);

CREATE TABLE IF NOT EXISTS payment_method (
    id TEXT PRIMARY KEY,
    organization_id TEXT NOT NULL,
    is_default INTEGER NOT NULL,
    payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_payment_method_org
    ON payment_method(organization_id, is_default DESC, id);

CREATE TABLE IF NOT EXISTS payment_invoice (
    id TEXT PRIMARY KEY,
    organization_id TEXT NOT NULL,
    number TEXT NOT NULL,
    provider_invoice_id TEXT,
    status TEXT NOT NULL,
    reconciliation TEXT NOT NULL DEFAULT 'unreconciled',
    issued_ms INTEGER NOT NULL,
    total_minor INTEGER NOT NULL,
    amount_paid_minor INTEGER NOT NULL,
    payload TEXT NOT NULL,
    UNIQUE (organization_id, number)
);
CREATE INDEX IF NOT EXISTS idx_payment_invoice_org
    ON payment_invoice(organization_id, issued_ms DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_payment_invoice_provider
    ON payment_invoice(provider_invoice_id);

CREATE TABLE IF NOT EXISTS payment_invoice_line (
    invoice_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (invoice_id, seq)
);

CREATE TABLE IF NOT EXISTS payment_pending_proration (
    id TEXT PRIMARY KEY,
    organization_id TEXT NOT NULL,
    subscription_id TEXT NOT NULL,
    consumed_by TEXT,
    payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_payment_pending_org
    ON payment_pending_proration(organization_id, subscription_id, consumed_by);

CREATE TABLE IF NOT EXISTS payment_payment (
    id TEXT PRIMARY KEY,
    organization_id TEXT NOT NULL,
    invoice_id TEXT NOT NULL,
    provider_payment_id TEXT NOT NULL,
    amount_minor INTEGER NOT NULL,
    refunded_minor INTEGER NOT NULL,
    status TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_payment_payment_org
    ON payment_payment(organization_id, created_ms DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_payment_payment_invoice
    ON payment_payment(invoice_id);
CREATE INDEX IF NOT EXISTS idx_payment_payment_provider
    ON payment_payment(provider_payment_id);

CREATE TABLE IF NOT EXISTS payment_refund (
    id TEXT PRIMARY KEY,
    organization_id TEXT NOT NULL,
    payment_id TEXT NOT NULL,
    provider_refund_id TEXT NOT NULL,
    status TEXT NOT NULL,
    amount_minor INTEGER NOT NULL,
    created_ms INTEGER NOT NULL,
    payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_payment_refund_payment
    ON payment_refund(payment_id);
CREATE INDEX IF NOT EXISTS idx_payment_refund_provider
    ON payment_refund(provider_refund_id);
CREATE INDEX IF NOT EXISTS idx_payment_refund_status
    ON payment_refund(organization_id, payment_id, status);

CREATE TABLE IF NOT EXISTS payment_credit_note (
    id TEXT PRIMARY KEY,
    organization_id TEXT NOT NULL,
    number TEXT NOT NULL,
    invoice_id TEXT NOT NULL,
    payment_id TEXT NOT NULL,
    amount_minor INTEGER NOT NULL,
    created_ms INTEGER NOT NULL,
    payload TEXT NOT NULL,
    UNIQUE (organization_id, number)
);

CREATE TABLE IF NOT EXISTS payment_webhook (
    provider TEXT NOT NULL,
    event_id TEXT NOT NULL,
    organization_id TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    event_type TEXT NOT NULL,
    disposition TEXT NOT NULL,
    payload_digest TEXT NOT NULL,
    PRIMARY KEY (provider, event_id)
);
CREATE INDEX IF NOT EXISTS idx_payment_webhook_org
    ON payment_webhook(organization_id, created_ms);

CREATE TABLE IF NOT EXISTS payment_webhook_watermark (
    organization_id TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    last_created_ms INTEGER NOT NULL,
    last_event_id TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS payment_idempotency (
    organization_id TEXT NOT NULL,
    key TEXT NOT NULL,
    operation TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    response TEXT,
    created_ms INTEGER NOT NULL,
    PRIMARY KEY (organization_id, key)
);

CREATE TABLE IF NOT EXISTS payment_sequence (
    organization_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    next INTEGER NOT NULL,
    PRIMARY KEY (organization_id, kind)
);

CREATE TABLE IF NOT EXISTS payment_provider_object (
    provider TEXT NOT NULL,
    kind TEXT NOT NULL,
    organization_id TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    object_id TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    PRIMARY KEY (provider, kind, organization_id, idempotency_key)
);
CREATE INDEX IF NOT EXISTS idx_payment_provider_object_id
    ON payment_provider_object(provider, kind, object_id);

CREATE TABLE IF NOT EXISTS payment_operation_claim (
    organization_id TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    operation TEXT NOT NULL,
    resource_type TEXT NOT NULL,
    resource_id TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    provider_idempotency_key TEXT NOT NULL,
    state TEXT NOT NULL,
    context TEXT NOT NULL,
    response TEXT,
    failure_code TEXT,
    failure_message TEXT,
    created_ms INTEGER NOT NULL,
    dispatched_ms INTEGER,
    resolved_ms INTEGER,
    updated_ms INTEGER NOT NULL,
    PRIMARY KEY (organization_id, idempotency_key)
);
CREATE INDEX IF NOT EXISTS idx_payment_operation_claim_state
    ON payment_operation_claim(organization_id, state);
CREATE INDEX IF NOT EXISTS idx_payment_operation_claim_resource
    ON payment_operation_claim(organization_id, resource_type, resource_id);

CREATE TABLE IF NOT EXISTS payment_provider_invoice (
    provider TEXT NOT NULL,
    provider_invoice_id TEXT NOT NULL,
    organization_id TEXT NOT NULL,
    invoice_id TEXT NOT NULL,
    status TEXT NOT NULL,
    amount_minor INTEGER NOT NULL,
    amount_paid_minor INTEGER NOT NULL,
    amount_refunded_minor INTEGER NOT NULL,
    currency TEXT NOT NULL,
    paid_at_ms INTEGER,
    observed_ms INTEGER NOT NULL,
    source TEXT NOT NULL,
    reconciliation TEXT NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (provider, provider_invoice_id)
);
CREATE INDEX IF NOT EXISTS idx_payment_provider_invoice_invoice
    ON payment_provider_invoice(organization_id, invoice_id, observed_ms DESC, provider);
CREATE INDEX IF NOT EXISTS idx_payment_provider_invoice_reconciliation
    ON payment_provider_invoice(organization_id, reconciliation);

CREATE TABLE IF NOT EXISTS payment_provider_payment (
    provider TEXT NOT NULL,
    provider_payment_id TEXT NOT NULL,
    organization_id TEXT NOT NULL,
    invoice_id TEXT NOT NULL,
    payment_id TEXT,
    status TEXT NOT NULL,
    amount_minor INTEGER NOT NULL,
    refunded_minor INTEGER NOT NULL,
    currency TEXT NOT NULL,
    paid_at_ms INTEGER,
    observed_ms INTEGER NOT NULL,
    source TEXT NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (provider, provider_payment_id)
);
CREATE INDEX IF NOT EXISTS idx_payment_provider_payment_invoice
    ON payment_provider_payment(organization_id, invoice_id, observed_ms DESC);

CREATE TABLE IF NOT EXISTS payment_provider_refund (
    provider TEXT NOT NULL,
    provider_refund_id TEXT NOT NULL,
    organization_id TEXT NOT NULL,
    invoice_id TEXT NOT NULL,
    payment_id TEXT NOT NULL,
    status TEXT NOT NULL,
    amount_minor INTEGER NOT NULL,
    currency TEXT NOT NULL,
    observed_ms INTEGER NOT NULL,
    source TEXT NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (provider, provider_refund_id)
);
CREATE INDEX IF NOT EXISTS idx_payment_provider_refund_invoice
    ON payment_provider_refund(organization_id, invoice_id, observed_ms DESC);
";

/// Version-1 databases gain the refund truth columns and the claim table.
/// Existing refunds were always confirmed before insertion, so they backfill
/// as `succeeded`.
const MIGRATE_V2: &str = "
ALTER TABLE payment_refund ADD COLUMN provider_refund_id TEXT NOT NULL DEFAULT '';
ALTER TABLE payment_refund ADD COLUMN status TEXT NOT NULL DEFAULT 'succeeded';
CREATE INDEX IF NOT EXISTS idx_payment_refund_provider
    ON payment_refund(provider_refund_id);
CREATE INDEX IF NOT EXISTS idx_payment_refund_status
    ON payment_refund(organization_id, payment_id, status);
CREATE TABLE IF NOT EXISTS payment_operation_claim (
    organization_id TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    operation TEXT NOT NULL,
    resource_type TEXT NOT NULL,
    resource_id TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    provider_idempotency_key TEXT NOT NULL,
    state TEXT NOT NULL,
    context TEXT NOT NULL,
    response TEXT,
    failure_code TEXT,
    failure_message TEXT,
    created_ms INTEGER NOT NULL,
    dispatched_ms INTEGER,
    resolved_ms INTEGER,
    updated_ms INTEGER NOT NULL,
    PRIMARY KEY (organization_id, idempotency_key)
);
CREATE INDEX IF NOT EXISTS idx_payment_operation_claim_state
    ON payment_operation_claim(organization_id, state);
CREATE INDEX IF NOT EXISTS idx_payment_operation_claim_resource
    ON payment_operation_claim(organization_id, resource_type, resource_id);
";

/// Version-2 databases gain the linked provider observation tables and the
/// Faktor-invoice reconciliation column. Every existing invoice keeps its
/// authored totals; its observation is backfilled as `unreconciled` (or with
/// the exact provider object already stored by the local provider).
const MIGRATE_V3: &str = "
ALTER TABLE payment_invoice ADD COLUMN reconciliation TEXT NOT NULL DEFAULT 'unreconciled';
CREATE INDEX IF NOT EXISTS idx_payment_invoice_reconciliation
    ON payment_invoice(organization_id, reconciliation);

CREATE TABLE IF NOT EXISTS payment_provider_invoice (
    provider TEXT NOT NULL,
    provider_invoice_id TEXT NOT NULL,
    organization_id TEXT NOT NULL,
    invoice_id TEXT NOT NULL,
    status TEXT NOT NULL,
    amount_minor INTEGER NOT NULL,
    amount_paid_minor INTEGER NOT NULL,
    amount_refunded_minor INTEGER NOT NULL,
    currency TEXT NOT NULL,
    paid_at_ms INTEGER,
    observed_ms INTEGER NOT NULL,
    source TEXT NOT NULL,
    reconciliation TEXT NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (provider, provider_invoice_id)
);
CREATE INDEX IF NOT EXISTS idx_payment_provider_invoice_invoice
    ON payment_provider_invoice(organization_id, invoice_id, observed_ms DESC, provider);
CREATE INDEX IF NOT EXISTS idx_payment_provider_invoice_reconciliation
    ON payment_provider_invoice(organization_id, reconciliation);

CREATE TABLE IF NOT EXISTS payment_provider_payment (
    provider TEXT NOT NULL,
    provider_payment_id TEXT NOT NULL,
    organization_id TEXT NOT NULL,
    invoice_id TEXT NOT NULL,
    payment_id TEXT,
    status TEXT NOT NULL,
    amount_minor INTEGER NOT NULL,
    refunded_minor INTEGER NOT NULL,
    currency TEXT NOT NULL,
    paid_at_ms INTEGER,
    observed_ms INTEGER NOT NULL,
    source TEXT NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (provider, provider_payment_id)
);
CREATE INDEX IF NOT EXISTS idx_payment_provider_payment_invoice
    ON payment_provider_payment(organization_id, invoice_id, observed_ms DESC);

CREATE TABLE IF NOT EXISTS payment_provider_refund (
    provider TEXT NOT NULL,
    provider_refund_id TEXT NOT NULL,
    organization_id TEXT NOT NULL,
    invoice_id TEXT NOT NULL,
    payment_id TEXT NOT NULL,
    status TEXT NOT NULL,
    amount_minor INTEGER NOT NULL,
    currency TEXT NOT NULL,
    observed_ms INTEGER NOT NULL,
    source TEXT NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (provider, provider_refund_id)
);
CREATE INDEX IF NOT EXISTS idx_payment_provider_refund_invoice
    ON payment_provider_refund(organization_id, invoice_id, observed_ms DESC);
";

/// One idempotency record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdempotencyRecord {
    pub operation: String,
    pub request_hash: String,
    pub response: Option<String>,
    pub created_ms: i64,
}

/// One record to write atomically with an effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdempotencyWrite {
    pub organization_id: String,
    pub key: String,
    pub operation: String,
    pub request_hash: String,
    pub response: String,
    pub created_ms: i64,
}

/// The outcome of an idempotency lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdempotencyClaim {
    Fresh,
    Replay(Option<String>),
    Conflict(String),
}

/// One webhook row to persist with its effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookRecord {
    pub provider: String,
    pub event_id: String,
    pub organization_id: String,
    pub created_ms: i64,
    pub event_type: String,
    pub disposition: String,
    pub payload_digest: String,
}

/// Effects applied atomically with a webhook record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WebhookEffects {
    pub subscription: Option<PaymentSubscription>,
    pub invoice: Option<Invoice>,
    pub payment: Option<Payment>,
    pub refund: Option<Refund>,
    pub credit_note: Option<CreditNote>,
    pub provider_invoice: Option<ProviderInvoiceObservation>,
    pub provider_payment: Option<ProviderPaymentObservation>,
    pub provider_refund: Option<ProviderRefundObservation>,
}

/// One webhook transaction plan produced inside the same immediate
/// transaction that records the event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookPlan<T> {
    pub record: WebhookRecord,
    pub effects: WebhookEffects,
    pub advance_watermark: bool,
    pub value: T,
}

/// The outcome of applying one webhook transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebhookDecision<T> {
    /// Another delivery of this event id already committed.
    Duplicate(String),
    Applied(T),
}

/// Counts from crash recovery of operation claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClaimRecovery {
    pub unknown: usize,
    pub released: usize,
}

/// The durable store.
pub struct SqlitePaymentStore {
    conn: Mutex<Connection>,
    path: Option<PathBuf>,
}

impl SqlitePaymentStore {
    pub fn open(path: &Path) -> Result<Self, PaymentError> {
        let conn = Connection::open(path).map_err(store_err)?;
        Self::prepare(conn, Some(path.to_path_buf()))
    }

    pub fn open_in_memory() -> Result<Self, PaymentError> {
        let conn = Connection::open_in_memory().map_err(store_err)?;
        Self::prepare(conn, None)
    }

    fn prepare(conn: Connection, path: Option<PathBuf>) -> Result<Self, PaymentError> {
        conn.busy_timeout(std::time::Duration::from_millis(5_000))
            .map_err(store_err)?;
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;",
        )
        .map_err(store_err)?;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(store_err)?;
        if version > PAYMENT_SCHEMA_VERSION {
            return Err(PaymentError::Store(format!(
                "payment database schema user_version {version} is newer than this build \
                 ({PAYMENT_SCHEMA_VERSION})"
            )));
        }
        if version < PAYMENT_SCHEMA_VERSION {
            conn.execute_batch("BEGIN IMMEDIATE").map_err(store_err)?;
            let migration = (|| -> Result<(), PaymentError> {
                if version == 0 {
                    conn.execute_batch(SCHEMA).map_err(store_err)?;
                } else {
                    if version < PAYMENT_SCHEMA_V2 {
                        conn.execute_batch(MIGRATE_V2).map_err(store_err)?;
                    }
                    if version < PAYMENT_SCHEMA_VERSION {
                        conn.execute_batch(MIGRATE_V3).map_err(store_err)?;
                    }
                }
                if version < PAYMENT_SCHEMA_VERSION {
                    backfill_provider_observations(&conn)?;
                }
                Ok(())
            })();
            if let Err(e) = migration {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }
            if let Err(e) =
                conn.execute_batch(&format!("PRAGMA user_version = {PAYMENT_SCHEMA_VERSION}"))
            {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(store_err(e));
            }
            conn.execute_batch("COMMIT").map_err(store_err)?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
            path,
        })
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, PaymentError> {
        self.conn
            .lock()
            .map_err(|_| PaymentError::Store("payment store lock is poisoned".into()))
    }

    fn tx<T>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> Result<T, PaymentError>,
    ) -> Result<T, PaymentError> {
        let mut conn = self.lock()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_err)?;
        let value = f(&tx)?;
        tx.commit().map_err(store_err)?;
        Ok(value)
    }

    // ------------------------------------------------------------ customers

    pub fn put_customer(&self, customer: &PaymentCustomer) -> Result<(), PaymentError> {
        self.tx(|tx| put_customer_tx(tx, customer))
    }

    pub fn customer(&self, organization_id: &str) -> Result<Option<PaymentCustomer>, PaymentError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM payment_customer WHERE organization_id = ?1",
                params![organization_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(store_err)?;
        payload.map(|p| decode(&p)).transpose()
    }

    pub fn customer_organization(
        &self,
        provider: &str,
        provider_customer_id: &str,
    ) -> Result<Option<String>, PaymentError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT organization_id FROM payment_customer \
             WHERE provider = ?1 AND provider_customer_id = ?2",
            params![provider, provider_customer_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(store_err)
    }

    // -------------------------------------------------------- subscriptions

    pub fn put_subscription(&self, subscription: &PaymentSubscription) -> Result<(), PaymentError> {
        let conn = self.lock()?;
        put_subscription_conn(&conn, subscription)
    }

    pub fn subscription(
        &self,
        organization_id: &str,
    ) -> Result<Option<PaymentSubscription>, PaymentError> {
        let conn = self.lock()?;
        subscription_conn(&conn, organization_id)
    }

    pub fn subscription_by_id(
        &self,
        organization_id: &str,
        id: &str,
    ) -> Result<Option<PaymentSubscription>, PaymentError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM payment_subscription WHERE organization_id = ?1 AND id = ?2",
                params![organization_id, id],
                |row| row.get(0),
            )
            .optional()
            .map_err(store_err)?;
        payload.map(|p| decode(&p)).transpose()
    }

    pub fn subscription_organization(
        &self,
        provider_subscription_id: &str,
    ) -> Result<Option<String>, PaymentError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT organization_id FROM payment_subscription \
             WHERE provider_subscription_id = ?1",
            params![provider_subscription_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(store_err)
    }

    // ------------------------------------------------------ payment methods

    pub fn put_payment_method(&self, method: &StoredPaymentMethod) -> Result<(), PaymentError> {
        self.tx(|tx| put_payment_method_tx(tx, method))
    }

    pub fn payment_method(
        &self,
        organization_id: &str,
        id: &str,
    ) -> Result<Option<StoredPaymentMethod>, PaymentError> {
        let conn = self.lock()?;
        payment_method_conn(&conn, organization_id, id)
    }

    pub fn payment_methods(
        &self,
        organization_id: &str,
    ) -> Result<Vec<StoredPaymentMethod>, PaymentError> {
        let conn = self.lock()?;
        let mut statement = conn
            .prepare(
                "SELECT is_default, payload FROM payment_method WHERE organization_id = ?1 \
                 ORDER BY is_default DESC, id",
            )
            .map_err(store_err)?;
        let rows = statement
            .query_map(params![organization_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(store_err)?;
        let mut methods = Vec::new();
        for row in rows {
            let (is_default, payload) = row.map_err(store_err)?;
            let mut method: StoredPaymentMethod = decode(&payload)?;
            method.method.is_default = is_default != 0;
            methods.push(method);
        }
        Ok(methods)
    }

    /// Every raw payment-method payload (used exactly once at service open
    /// to migrate legacy plaintext token rows into vault ciphertext).
    pub fn raw_payment_methods(&self) -> Result<Vec<(String, String, String)>, PaymentError> {
        let conn = self.lock()?;
        let mut statement = conn
            .prepare("SELECT id, organization_id, payload FROM payment_method ORDER BY id")
            .map_err(store_err)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(store_err)?;
        let mut items = Vec::new();
        for row in rows {
            items.push(row.map_err(store_err)?);
        }
        Ok(items)
    }

    /// Rewrite one migrated row (the id and organization are immutable).
    pub fn rewrite_payment_method_payload(
        &self,
        id: &str,
        organization_id: &str,
        payload: &serde_json::Value,
    ) -> Result<(), PaymentError> {
        let encoded = serde_json::to_string(payload)
            .map_err(|e| PaymentError::Store(format!("payment method encoding: {e}")))?;
        let conn = self.lock()?;
        conn.execute(
            "UPDATE payment_method SET payload = ?3 WHERE id = ?1 AND organization_id = ?2",
            params![id, organization_id, encoded],
        )
        .map_err(store_err)?;
        Ok(())
    }

    pub fn remove_payment_method(
        &self,
        organization_id: &str,
        id: &str,
    ) -> Result<bool, PaymentError> {
        let conn = self.lock()?;
        let changed = conn
            .execute(
                "DELETE FROM payment_method WHERE organization_id = ?1 AND id = ?2",
                params![organization_id, id],
            )
            .map_err(store_err)?;
        Ok(changed > 0)
    }

    pub fn refund_by_id(
        &self,
        organization_id: &str,
        id: &str,
    ) -> Result<Option<Refund>, PaymentError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM payment_refund WHERE organization_id = ?1 AND id = ?2",
                params![organization_id, id],
                |row| row.get(0),
            )
            .optional()
            .map_err(store_err)?;
        payload.map(|p| decode(&p)).transpose()
    }

    pub fn refund_by_provider_id(
        &self,
        organization_id: &str,
        provider_refund_id: &str,
    ) -> Result<Option<Refund>, PaymentError> {
        let conn = self.lock()?;
        refund_by_provider_id_conn(&conn, organization_id, provider_refund_id)
    }

    pub fn refunds_for_payment(
        &self,
        organization_id: &str,
        payment_id: &str,
    ) -> Result<Vec<Refund>, PaymentError> {
        let conn = self.lock()?;
        let mut statement = conn
            .prepare(
                "SELECT payload FROM payment_refund \
                 WHERE organization_id = ?1 AND payment_id = ?2 ORDER BY created_ms, id",
            )
            .map_err(store_err)?;
        let rows = statement
            .query_map(params![organization_id, payment_id], |row| {
                row.get::<_, String>(0)
            })
            .map_err(store_err)?;
        let mut items = Vec::new();
        for row in rows {
            items.push(decode(&row.map_err(store_err)?)?);
        }
        Ok(items)
    }

    // ------------------------------------------------------------- invoices

    pub fn invoice(
        &self,
        organization_id: &str,
        id: &str,
    ) -> Result<Option<Invoice>, PaymentError> {
        let conn = self.lock()?;
        invoice_conn(&conn, organization_id, id)
    }

    pub fn invoice_by_provider_id(
        &self,
        provider_invoice_id: &str,
    ) -> Result<Option<Invoice>, PaymentError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM payment_invoice WHERE provider_invoice_id = ?1",
                params![provider_invoice_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(store_err)?;
        payload.map(|p| decode(&p)).transpose()
    }

    pub fn invoices(
        &self,
        organization_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<InvoicePage, PaymentError> {
        let conn = self.lock()?;
        let mut items: Vec<Invoice> = Vec::new();
        let mut next_cursor = None;
        let mut has_more = false;
        if let Some(cursor) = cursor {
            let Some((issued_ms, id)) = conn
                .query_row(
                    "SELECT issued_ms, id FROM payment_invoice \
                     WHERE organization_id = ?1 AND id = ?2",
                    params![organization_id, cursor],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()
                .map_err(store_err)?
            else {
                return Err(PaymentError::NotFound(format!("invoice cursor {cursor}")));
            };
            let mut statement = conn
                .prepare(
                    "SELECT payload FROM payment_invoice WHERE organization_id = ?1 AND \
                     (issued_ms < ?2 OR (issued_ms = ?2 AND id < ?3)) \
                     ORDER BY issued_ms DESC, id DESC LIMIT ?4",
                )
                .map_err(store_err)?;
            let rows = statement
                .query_map(
                    params![
                        organization_id,
                        issued_ms,
                        id,
                        checked_page_limit(limit, 1)?
                    ],
                    |row| row.get::<_, String>(0),
                )
                .map_err(store_err)?;
            for row in rows {
                items.push(decode(&row.map_err(store_err)?)?);
            }
        } else {
            let mut statement = conn
                .prepare(
                    "SELECT payload FROM payment_invoice WHERE organization_id = ?1 \
                     ORDER BY issued_ms DESC, id DESC LIMIT ?2",
                )
                .map_err(store_err)?;
            let rows = statement
                .query_map(
                    params![organization_id, checked_page_limit(limit, 1)?],
                    |row| row.get::<_, String>(0),
                )
                .map_err(store_err)?;
            for row in rows {
                items.push(decode(&row.map_err(store_err)?)?);
            }
        }
        if items.len() > limit {
            has_more = true;
            items.truncate(limit);
        }
        if has_more {
            next_cursor = items.last().map(|invoice| invoice.id.clone());
        }
        Ok(InvoicePage {
            items,
            next_cursor,
            has_more,
        })
    }

    pub fn invoice_stats(&self, organization_id: &str) -> Result<InvoiceStats, PaymentError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT COUNT(*), \
             COALESCE(SUM(CASE WHEN status IN ('open','past_due') THEN 1 ELSE 0 END), 0), \
             COALESCE(SUM(CASE WHEN status IN ('open','past_due') THEN total_minor - amount_paid_minor ELSE 0 END), 0), \
             COALESCE(SUM(CASE WHEN status = 'paid' THEN amount_paid_minor ELSE 0 END), 0) \
             FROM payment_invoice WHERE organization_id = ?1",
            params![organization_id],
            |row| {
                Ok(InvoiceStats {
                    invoice_count: row.get::<_, i64>(0)? as u64,
                    open_invoice_count: row.get::<_, i64>(1)? as u64,
                    outstanding_minor: row.get(2)?,
                    paid_minor: row.get(3)?,
                })
            },
        )
        .map_err(store_err)
    }

    /// Invoices linked to a provider invoice, ordered by issue time. Used by
    /// the manual/periodic reconciliation pass; the returned rows are Faktor
    /// truth.
    pub fn invoices_for_reconciliation(
        &self,
        organization_id: &str,
        limit: usize,
    ) -> Result<Vec<Invoice>, PaymentError> {
        let conn = self.lock()?;
        let mut statement = conn
            .prepare(
                "SELECT payload FROM payment_invoice \
                 WHERE organization_id = ?1 AND provider_invoice_id IS NOT NULL \
                 ORDER BY issued_ms, id LIMIT ?2",
            )
            .map_err(store_err)?;
        let rows = statement
            .query_map(
                params![organization_id, checked_page_limit(limit, 0)?],
                |row| row.get::<_, String>(0),
            )
            .map_err(store_err)?;
        let mut items = Vec::new();
        for row in rows {
            items.push(decode(&row.map_err(store_err)?)?);
        }
        Ok(items)
    }

    /// Exact counts of the Faktor invoices' reconciliation states.
    pub fn reconciliation_summary(
        &self,
        organization_id: &str,
    ) -> Result<ReconciliationSummary, PaymentError> {
        let conn = self.lock()?;
        let mut summary = ReconciliationSummary::default();
        let mut statement = conn
            .prepare(
                "SELECT reconciliation, COUNT(*) FROM payment_invoice \
                 WHERE organization_id = ?1 GROUP BY reconciliation",
            )
            .map_err(store_err)?;
        let rows = statement
            .query_map(params![organization_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(store_err)?;
        for row in rows {
            let (state, count) = row.map_err(store_err)?;
            let count = u64::try_from(count).unwrap_or(u64::MAX);
            match InvoiceReconciliationState::parse(&state).unwrap_or_default() {
                InvoiceReconciliationState::Unreconciled => summary.unreconciled = count,
                InvoiceReconciliationState::Matched => summary.matched = count,
                InvoiceReconciliationState::AmountMismatch => summary.amount_mismatch = count,
                InvoiceReconciliationState::CurrencyMismatch => summary.currency_mismatch = count,
                InvoiceReconciliationState::ProviderInvoiceMissing => {
                    summary.provider_invoice_missing = count;
                }
            }
        }
        Ok(summary)
    }

    /// Every linked provider-invoice observation for one Faktor invoice.
    pub fn provider_invoice_observations(
        &self,
        organization_id: &str,
        invoice_id: &str,
    ) -> Result<Vec<ProviderInvoiceObservation>, PaymentError> {
        let conn = self.lock()?;
        let mut statement = conn
            .prepare(
                "SELECT payload FROM payment_provider_invoice \
                 WHERE organization_id = ?1 AND invoice_id = ?2 \
                 ORDER BY observed_ms DESC, provider",
            )
            .map_err(store_err)?;
        let rows = statement
            .query_map(params![organization_id, invoice_id], |row| {
                row.get::<_, String>(0)
            })
            .map_err(store_err)?;
        let mut items = Vec::new();
        for row in rows {
            items.push(decode(&row.map_err(store_err)?)?);
        }
        Ok(items)
    }

    pub fn provider_invoice_observation(
        &self,
        provider: &str,
        provider_invoice_id: &str,
    ) -> Result<Option<ProviderInvoiceObservation>, PaymentError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM payment_provider_invoice \
                 WHERE provider = ?1 AND provider_invoice_id = ?2",
                params![provider, provider_invoice_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(store_err)?;
        payload.map(|payload| decode(&payload)).transpose()
    }

    pub fn provider_payment_observations(
        &self,
        organization_id: &str,
        invoice_id: &str,
    ) -> Result<Vec<ProviderPaymentObservation>, PaymentError> {
        let conn = self.lock()?;
        let mut statement = conn
            .prepare(
                "SELECT payload FROM payment_provider_payment \
                 WHERE organization_id = ?1 AND invoice_id = ?2 \
                 ORDER BY observed_ms DESC, provider",
            )
            .map_err(store_err)?;
        let rows = statement
            .query_map(params![organization_id, invoice_id], |row| {
                row.get::<_, String>(0)
            })
            .map_err(store_err)?;
        let mut items = Vec::new();
        for row in rows {
            items.push(decode(&row.map_err(store_err)?)?);
        }
        Ok(items)
    }

    pub fn provider_refund_observations(
        &self,
        organization_id: &str,
        invoice_id: &str,
    ) -> Result<Vec<ProviderRefundObservation>, PaymentError> {
        let conn = self.lock()?;
        let mut statement = conn
            .prepare(
                "SELECT payload FROM payment_provider_refund \
                 WHERE organization_id = ?1 AND invoice_id = ?2 \
                 ORDER BY observed_ms DESC, provider",
            )
            .map_err(store_err)?;
        let rows = statement
            .query_map(params![organization_id, invoice_id], |row| {
                row.get::<_, String>(0)
            })
            .map_err(store_err)?;
        let mut items = Vec::new();
        for row in rows {
            items.push(decode(&row.map_err(store_err)?)?);
        }
        Ok(items)
    }

    pub fn put_provider_invoice_observation(
        &self,
        observation: &ProviderInvoiceObservation,
    ) -> Result<(), PaymentError> {
        self.tx(|tx| upsert_provider_invoice_conn(tx, observation))
    }

    pub fn put_provider_payment_observation(
        &self,
        observation: &ProviderPaymentObservation,
    ) -> Result<(), PaymentError> {
        self.tx(|tx| upsert_provider_payment_conn(tx, observation))
    }

    pub fn put_provider_refund_observation(
        &self,
        observation: &ProviderRefundObservation,
    ) -> Result<(), PaymentError> {
        self.tx(|tx| upsert_provider_refund_conn(tx, observation))
    }

    /// Persist one reconciliation result (the Faktor invoice's reconciliation
    /// state + the linked observation) atomically. Only the reconciliation
    /// field of the invoice is written; its authored money is untouched.
    pub fn record_reconciliation(
        &self,
        invoice: &Invoice,
        observation: &ProviderInvoiceObservation,
    ) -> Result<(), PaymentError> {
        self.tx(|tx| {
            update_invoice_tx(tx, invoice)?;
            upsert_provider_invoice_conn(tx, observation)
        })
    }

    // -------------------------------------------------------------- payments

    pub fn payment(
        &self,
        organization_id: &str,
        id: &str,
    ) -> Result<Option<Payment>, PaymentError> {
        let conn = self.lock()?;
        payment_conn(&conn, organization_id, id)
    }

    pub fn payment_by_provider_id(
        &self,
        organization_id: &str,
        provider_payment_id: &str,
    ) -> Result<Option<Payment>, PaymentError> {
        let conn = self.lock()?;
        payment_by_provider_id_conn(&conn, organization_id, provider_payment_id)
    }

    pub fn payments(
        &self,
        organization_id: &str,
        limit: usize,
    ) -> Result<Vec<Payment>, PaymentError> {
        let conn = self.lock()?;
        let mut statement = conn
            .prepare(
                "SELECT payload FROM payment_payment WHERE organization_id = ?1 \
                 ORDER BY created_ms DESC, id DESC LIMIT ?2",
            )
            .map_err(store_err)?;
        let rows = statement
            .query_map(
                params![organization_id, checked_page_limit(limit, 0)?],
                |row| row.get::<_, String>(0),
            )
            .map_err(store_err)?;
        let mut items: Vec<Payment> = Vec::new();
        for row in rows {
            items.push(decode(&row.map_err(store_err)?)?);
        }
        Ok(items)
    }

    pub fn refunded_for_payment(
        &self,
        organization_id: &str,
        payment_id: &str,
    ) -> Result<i64, PaymentError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT COALESCE(SUM(amount_minor), 0) FROM payment_refund \
             WHERE organization_id = ?1 AND payment_id = ?2 AND status = 'succeeded'",
            params![organization_id, payment_id],
            |row| row.get(0),
        )
        .map_err(store_err)
    }

    /// Succeeded plus still-pending refunds: pending money is reserved so a
    /// concurrent second refund cannot exceed what was collected.
    pub fn committed_refunds_for_payment(
        &self,
        organization_id: &str,
        payment_id: &str,
    ) -> Result<i64, PaymentError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT COALESCE(SUM(amount_minor), 0) FROM payment_refund \
             WHERE organization_id = ?1 AND payment_id = ?2 \
             AND status IN ('succeeded','pending')",
            params![organization_id, payment_id],
            |row| row.get(0),
        )
        .map_err(store_err)
    }

    pub fn credit_note(
        &self,
        organization_id: &str,
        id: &str,
    ) -> Result<Option<CreditNote>, PaymentError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM payment_credit_note WHERE organization_id = ?1 AND id = ?2",
                params![organization_id, id],
                |row| row.get(0),
            )
            .optional()
            .map_err(store_err)?;
        payload.map(|p| decode(&p)).transpose()
    }

    // -------------------------------------------------------- idempotency

    pub fn read_idempotency(
        &self,
        organization_id: &str,
        key: &str,
    ) -> Result<Option<IdempotencyRecord>, PaymentError> {
        let conn = self.lock()?;
        read_idempotency_conn(&conn, organization_id, key)
    }

    /// Persist one effect and its idempotency record atomically. The effect
    /// closure receives the transaction; callers write their rows through the
    /// `*_tx` helpers below.
    pub fn record_atomic<T>(
        &self,
        write: &IdempotencyWrite,
        effect: impl FnOnce(&Transaction<'_>) -> Result<T, PaymentError>,
    ) -> Result<T, PaymentError> {
        self.tx(|tx| {
            if let Some(existing) = read_idempotency_conn(tx, &write.organization_id, &write.key)? {
                if existing.request_hash != write.request_hash
                    || existing.operation != write.operation
                {
                    return Err(PaymentError::IdempotencyConflict(format!(
                        "idempotency key {} was already used for a different request",
                        write.key
                    )));
                }
                return Err(PaymentError::Conflict(format!(
                    "idempotency key {} is already recorded",
                    write.key
                )));
            }
            if let Some(claim) = read_claim_conn(tx, &write.organization_id, &write.key)? {
                if claim.state.is_terminal() {
                    return Err(PaymentError::Conflict(format!(
                        "operation claim {} is already {}",
                        write.key,
                        claim.state.as_str()
                    )));
                }
            }
            let value = effect(tx)?;
            write_idempotency_tx(tx, write)?;
            complete_claim_tx(
                tx,
                &write.organization_id,
                &write.key,
                &write.response,
                write.created_ms,
            )?;
            Ok(value)
        })
    }

    pub fn next_sequence(&self, organization_id: &str, kind: &str) -> Result<i64, PaymentError> {
        self.tx(|tx| next_sequence_tx(tx, organization_id, kind))
    }

    // ------------------------------------------------------ operation claims

    /// Acquire the one dispatch claim for an idempotency key. The claim row
    /// and its resource binding are committed before any provider call.
    pub fn claim_operation(
        &self,
        draft: &OperationClaimDraft,
    ) -> Result<ClaimAcquisition, PaymentError> {
        self.tx(|tx| {
            if let Some(existing) =
                read_claim_conn(tx, &draft.organization_id, &draft.idempotency_key)?
            {
                if existing.operation != draft.operation
                    || existing.request_digest != draft.request_digest
                    || existing.resource_type != draft.resource_type
                    || existing.resource_id != draft.resource_id
                    || existing.provider_idempotency_key != draft.provider_idempotency_key
                {
                    return Err(PaymentError::IdempotencyConflict(format!(
                        "idempotency key {} is bound to a different operation or resource",
                        draft.idempotency_key
                    )));
                }
                return Ok(match existing.state {
                    OperationClaimState::Succeeded | OperationClaimState::Failed => {
                        ClaimAcquisition::Recorded(existing)
                    }
                    OperationClaimState::Unknown => ClaimAcquisition::Unresolved(existing),
                    OperationClaimState::Claimed | OperationClaimState::Dispatched => {
                        ClaimAcquisition::InFlight(existing)
                    }
                });
            }
            insert_claim_tx(tx, draft)?;
            Ok(ClaimAcquisition::Acquired)
        })
    }

    /// Persist the provider-boundary crossing BEFORE the provider call. Only
    /// the claimant that owns a `claimed` row may make this transition.
    pub fn mark_claim_dispatched(
        &self,
        organization_id: &str,
        key: &str,
        now_ms: i64,
    ) -> Result<OperationClaim, PaymentError> {
        self.tx(|tx| {
            let changed = tx
                .execute(
                    "UPDATE payment_operation_claim SET state = 'dispatched', \
                     dispatched_ms = ?3, updated_ms = ?3 \
                     WHERE organization_id = ?1 AND idempotency_key = ?2 AND state = 'claimed'",
                    params![organization_id, key, now_ms],
                )
                .map_err(store_err)?;
            if changed != 1 {
                return Err(PaymentError::Conflict(format!(
                    "operation claim {key} is not owned by this dispatcher"
                )));
            }
            read_claim_conn(tx, organization_id, key)?
                .ok_or_else(|| PaymentError::Store("operation claim vanished".into()))
        })
    }

    /// Record a definitive provider refusal that cannot have created money.
    pub fn fail_claim(
        &self,
        organization_id: &str,
        key: &str,
        code: &str,
        message: &str,
        now_ms: i64,
    ) -> Result<(), PaymentError> {
        self.resolve_claim_failure(organization_id, key, code, message, now_ms, false)
    }

    /// Mark a dispatched operation unknown: reconciliation, never dispatch.
    pub fn mark_claim_unknown(
        &self,
        organization_id: &str,
        key: &str,
        now_ms: i64,
    ) -> Result<(), PaymentError> {
        self.tx(|tx| {
            tx.execute(
                "UPDATE payment_operation_claim SET state = 'unknown', updated_ms = ?3 \
                 WHERE organization_id = ?1 AND idempotency_key = ?2 AND state = 'dispatched'",
                params![organization_id, key, now_ms],
            )
            .map_err(store_err)?;
            Ok(())
        })
    }

    /// A provider reconciliation proved the operation never applied.
    pub fn fail_unknown_claim(
        &self,
        organization_id: &str,
        key: &str,
        code: &str,
        message: &str,
        now_ms: i64,
    ) -> Result<(), PaymentError> {
        self.resolve_claim_failure(organization_id, key, code, message, now_ms, true)
    }

    fn resolve_claim_failure(
        &self,
        organization_id: &str,
        key: &str,
        code: &str,
        message: &str,
        now_ms: i64,
        from_unknown: bool,
    ) -> Result<(), PaymentError> {
        self.tx(|tx| {
            let allowed = if from_unknown {
                "state IN ('dispatched','unknown')"
            } else {
                "state IN ('claimed','dispatched')"
            };
            tx.execute(
                &format!(
                    "UPDATE payment_operation_claim SET state = 'failed', failure_code = ?3, \
                     failure_message = ?4, resolved_ms = ?5, updated_ms = ?5 \
                     WHERE organization_id = ?1 AND idempotency_key = ?2 AND {allowed}"
                ),
                params![organization_id, key, code, message, now_ms],
            )
            .map_err(store_err)?;
            Ok(())
        })
    }

    /// Resolve an `unknown` claim with a provider-confirmed outcome. The
    /// caller performs this only inside the same transaction that replays
    /// the recorded local effect; the store marks the claim afterwards.
    pub fn resolve_unknown_claim(
        &self,
        organization_id: &str,
        key: &str,
        response: &str,
        now_ms: i64,
    ) -> Result<(), PaymentError> {
        self.tx(|tx| {
            tx.execute(
                "UPDATE payment_operation_claim SET state = 'succeeded', response = ?3, \
                 resolved_ms = ?4, updated_ms = ?4 \
                 WHERE organization_id = ?1 AND idempotency_key = ?2 AND state = 'unknown'",
                params![organization_id, key, response, now_ms],
            )
            .map_err(store_err)?;
            Ok(())
        })
    }

    pub fn operation_claim(
        &self,
        organization_id: &str,
        key: &str,
    ) -> Result<Option<OperationClaim>, PaymentError> {
        let conn = self.lock()?;
        read_claim_conn(&conn, organization_id, key)
    }

    pub fn operation_claims(
        &self,
        organization_id: &str,
    ) -> Result<Vec<OperationClaim>, PaymentError> {
        let conn = self.lock()?;
        let mut statement = conn
            .prepare(
                "SELECT organization_id, idempotency_key, operation, resource_type, resource_id, \
                 request_digest, provider_idempotency_key, state, context, response, \
                 failure_code, failure_message, created_ms, dispatched_ms, resolved_ms, updated_ms \
                 FROM payment_operation_claim WHERE organization_id = ?1 \
                 ORDER BY created_ms, idempotency_key",
            )
            .map_err(store_err)?;
        let rows = statement
            .query_map(params![organization_id], decode_claim_row)
            .map_err(store_err)?;
        let mut claims = Vec::new();
        for row in rows {
            claims.push(row.map_err(store_err)?);
        }
        Ok(claims)
    }

    /// Startup recovery: a row that crossed the provider boundary in a dead
    /// process is unknown; a row that never dispatched had no effect and is
    /// released so the same idempotency key may be retried.
    pub fn recover_claims(&self, now_ms: i64) -> Result<ClaimRecovery, PaymentError> {
        self.tx(|tx| {
            let unknown = tx
                .execute(
                    "UPDATE payment_operation_claim SET state = 'unknown', updated_ms = ?1 \
                     WHERE state = 'dispatched'",
                    params![now_ms],
                )
                .map_err(store_err)?;
            let released = tx
                .execute(
                    "DELETE FROM payment_operation_claim WHERE state = 'claimed'",
                    [],
                )
                .map_err(store_err)?;
            Ok(ClaimRecovery { unknown, released })
        })
    }

    // ------------------------------------------------------ local provider

    pub fn provider_object(
        &self,
        provider: &str,
        kind: &str,
        organization_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<(String, serde_json::Value)>, PaymentError> {
        let conn = self.lock()?;
        provider_object_conn(&conn, provider, kind, organization_id, idempotency_key)
    }

    /// Look one local-provider object up by its provider object id. This is a
    /// read-only support path for the local adapter's invoice fetch.
    pub fn provider_object_by_object_id(
        &self,
        provider: &str,
        kind: &str,
        object_id: &str,
    ) -> Result<Option<serde_json::Value>, PaymentError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM payment_provider_object \
                 WHERE provider = ?1 AND kind = ?2 AND object_id = ?3 LIMIT 1",
                params![provider, kind, object_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(store_err)?;
        payload
            .map(|payload| {
                serde_json::from_str(&payload)
                    .map_err(|e| PaymentError::Store(format!("provider object decoding: {e}")))
            })
            .transpose()
    }

    pub fn put_provider_object(
        &self,
        provider: &str,
        kind: &str,
        organization_id: &str,
        idempotency_key: &str,
        object_id: &str,
        payload: &serde_json::Value,
    ) -> Result<(), PaymentError> {
        let encoded = serde_json::to_string(payload)
            .map_err(|e| PaymentError::Store(format!("provider object encoding: {e}")))?;
        let conn = self.lock()?;
        conn.execute(
            "INSERT OR IGNORE INTO payment_provider_object \
             (provider, kind, organization_id, idempotency_key, object_id, payload, created_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                provider,
                kind,
                organization_id,
                idempotency_key,
                object_id,
                encoded,
                now_ms()
            ],
        )
        .map_err(store_err)?;
        Ok(())
    }

    // ------------------------------------------------------------ webhook

    pub fn webhook_seen(
        &self,
        provider: &str,
        event_id: &str,
    ) -> Result<Option<String>, PaymentError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT disposition FROM payment_webhook WHERE provider = ?1 AND event_id = ?2",
            params![provider, event_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(store_err)
    }

    pub fn webhook_watermark(
        &self,
        organization_id: &str,
    ) -> Result<Option<(i64, String)>, PaymentError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT last_created_ms, last_event_id FROM payment_webhook_watermark \
             WHERE organization_id = ?1",
            params![organization_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(store_err)
    }

    /// Apply one webhook in a single immediate transaction: the duplicate
    /// check, ordering decision, state transitions, ledger changes and the
    /// idempotent event row all commit (or roll back) together.
    pub fn apply_webhook<T>(
        &self,
        provider: &str,
        event_id: &str,
        apply: impl FnOnce(&Transaction<'_>) -> Result<WebhookPlan<T>, PaymentError>,
    ) -> Result<WebhookDecision<T>, PaymentError> {
        self.tx(|tx| {
            if let Some(disposition) = tx
                .query_row(
                    "SELECT disposition FROM payment_webhook \
                     WHERE provider = ?1 AND event_id = ?2",
                    params![provider, event_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(store_err)?
            {
                return Ok(WebhookDecision::Duplicate(disposition));
            }
            let plan = apply(tx)?;
            apply_webhook_effects_tx(tx, &plan.effects)?;
            tx.execute(
                "INSERT INTO payment_webhook \
                 (provider, event_id, organization_id, created_ms, event_type, disposition, \
                  payload_digest) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    plan.record.provider,
                    plan.record.event_id,
                    plan.record.organization_id,
                    plan.record.created_ms,
                    plan.record.event_type,
                    plan.record.disposition,
                    plan.record.payload_digest
                ],
            )
            .map_err(store_err)?;
            if plan.advance_watermark {
                tx.execute(
                    "INSERT INTO payment_webhook_watermark \
                     (organization_id, provider, last_created_ms, last_event_id) \
                     VALUES (?1, ?2, ?3, ?4) \
                     ON CONFLICT(organization_id) DO UPDATE SET \
                     provider = excluded.provider, \
                     last_created_ms = excluded.last_created_ms, \
                     last_event_id = excluded.last_event_id \
                     WHERE excluded.last_created_ms >= payment_webhook_watermark.last_created_ms",
                    params![
                        plan.record.organization_id,
                        plan.record.provider,
                        plan.record.created_ms,
                        plan.record.event_id
                    ],
                )
                .map_err(store_err)?;
            }
            Ok(WebhookDecision::Applied(plan.value))
        })
    }

    /// Record a webhook and apply its effects in one transaction.
    pub fn record_webhook(
        &self,
        record: &WebhookRecord,
        effects: &WebhookEffects,
        advance_watermark: bool,
    ) -> Result<(), PaymentError> {
        self.tx(|tx| {
            apply_webhook_effects_tx(tx, effects)?;
            tx.execute(
                "INSERT INTO payment_webhook \
                 (provider, event_id, organization_id, created_ms, event_type, disposition, \
                  payload_digest) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    record.provider,
                    record.event_id,
                    record.organization_id,
                    record.created_ms,
                    record.event_type,
                    record.disposition,
                    record.payload_digest
                ],
            )
            .map_err(store_err)?;
            if advance_watermark {
                tx.execute(
                    "INSERT INTO payment_webhook_watermark \
                     (organization_id, provider, last_created_ms, last_event_id) \
                     VALUES (?1, ?2, ?3, ?4) \
                     ON CONFLICT(organization_id) DO UPDATE SET \
                     provider = excluded.provider, \
                     last_created_ms = excluded.last_created_ms, \
                     last_event_id = excluded.last_event_id \
                     WHERE excluded.last_created_ms >= payment_webhook_watermark.last_created_ms",
                    params![
                        record.organization_id,
                        record.provider,
                        record.created_ms,
                        record.event_id
                    ],
                )
                .map_err(store_err)?;
            }
            Ok(())
        })
    }

    /// Record a stale/ignored event without applying effects.
    pub fn record_webhook_only(&self, record: &WebhookRecord) -> Result<(), PaymentError> {
        self.record_webhook(record, &WebhookEffects::default(), false)
    }
}

/// Aggregated invoice figures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InvoiceStats {
    pub invoice_count: u64,
    pub open_invoice_count: u64,
    pub outstanding_minor: i64,
    pub paid_minor: i64,
}

// ------------------------------------------------------------ tx helpers

pub(crate) fn put_customer_tx(
    tx: &Connection,
    customer: &PaymentCustomer,
) -> Result<(), PaymentError> {
    tx.execute(
        "INSERT OR REPLACE INTO payment_customer \
         (organization_id, provider, provider_customer_id, payload) VALUES (?1, ?2, ?3, ?4)",
        params![
            customer.organization_id,
            customer.provider,
            customer.provider_customer_id,
            encode(customer)?
        ],
    )
    .map_err(store_err)?;
    Ok(())
}

fn put_subscription_conn(
    conn: &Connection,
    subscription: &PaymentSubscription,
) -> Result<(), PaymentError> {
    conn.execute(
        "INSERT OR REPLACE INTO payment_subscription \
         (id, organization_id, provider_subscription_id, plan_id, state, payload) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            subscription.id,
            subscription.organization_id,
            subscription.provider_subscription_id,
            subscription.plan_id,
            subscription.state.as_str(),
            encode(subscription)?
        ],
    )
    .map_err(store_err)?;
    Ok(())
}

pub(crate) fn put_subscription_tx(
    tx: &Connection,
    subscription: &PaymentSubscription,
) -> Result<(), PaymentError> {
    put_subscription_conn(tx, subscription)
}

fn subscription_conn(
    conn: &Connection,
    organization_id: &str,
) -> Result<Option<PaymentSubscription>, PaymentError> {
    let payload: Option<String> = conn
        .query_row(
            "SELECT payload FROM payment_subscription WHERE organization_id = ?1",
            params![organization_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(store_err)?;
    payload.map(|p| decode(&p)).transpose()
}

fn payment_method_conn(
    conn: &Connection,
    organization_id: &str,
    id: &str,
) -> Result<Option<StoredPaymentMethod>, PaymentError> {
    let payload: Option<(i64, String)> = conn
        .query_row(
            "SELECT is_default, payload FROM payment_method \
             WHERE organization_id = ?1 AND id = ?2",
            params![organization_id, id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(store_err)?;
    match payload {
        None => Ok(None),
        Some((is_default, payload)) => {
            let mut method: StoredPaymentMethod = decode(&payload)?;
            method.method.is_default = is_default != 0;
            Ok(Some(method))
        }
    }
}

pub(crate) fn put_payment_method_tx(
    tx: &Connection,
    method: &StoredPaymentMethod,
) -> Result<(), PaymentError> {
    if method.method.is_default {
        tx.execute(
            "UPDATE payment_method SET is_default = 0 WHERE organization_id = ?1",
            params![method.method.organization_id],
        )
        .map_err(store_err)?;
    }
    tx.execute(
        "INSERT OR REPLACE INTO payment_method (id, organization_id, is_default, payload) \
         VALUES (?1, ?2, ?3, ?4)",
        params![
            method.method.id,
            method.method.organization_id,
            i64::from(method.method.is_default),
            encode(method)?
        ],
    )
    .map_err(store_err)?;
    Ok(())
}

pub(crate) fn insert_invoice_tx(
    tx: &Connection,
    invoice: &Invoice,
    lines: &[crate::model::InvoiceLine],
) -> Result<(), PaymentError> {
    tx.execute(
        "INSERT INTO payment_invoice \
         (id, organization_id, number, provider_invoice_id, status, reconciliation, issued_ms, \
          total_minor, amount_paid_minor, payload) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            invoice.id,
            invoice.organization_id,
            invoice.number,
            invoice.provider_invoice_id,
            invoice.status.as_str(),
            invoice.reconciliation.as_str(),
            invoice.issued_ms,
            invoice.total_minor,
            invoice.amount_paid_minor,
            encode(invoice)?
        ],
    )
    .map_err(store_err)?;
    for (seq, line) in lines.iter().enumerate() {
        tx.execute(
            "INSERT INTO payment_invoice_line (invoice_id, seq, payload) VALUES (?1, ?2, ?3)",
            params![invoice.id, seq as i64, encode(line)?],
        )
        .map_err(store_err)?;
    }
    Ok(())
}

pub(crate) fn update_invoice_tx(tx: &Connection, invoice: &Invoice) -> Result<(), PaymentError> {
    tx.execute(
        "UPDATE payment_invoice SET provider_invoice_id = ?2, status = ?3, \
         reconciliation = ?4, total_minor = ?5, amount_paid_minor = ?6, payload = ?7 \
         WHERE id = ?1",
        params![
            invoice.id,
            invoice.provider_invoice_id,
            invoice.status.as_str(),
            invoice.reconciliation.as_str(),
            invoice.total_minor,
            invoice.amount_paid_minor,
            encode(invoice)?
        ],
    )
    .map_err(store_err)?;
    Ok(())
}

#[allow(dead_code)]
fn update_invoice_conn(conn: &Connection, invoice: &Invoice) -> Result<(), PaymentError> {
    update_invoice_tx(conn, invoice)
}

/// Upsert the latest linked provider-invoice observation. The primary key is
/// `(provider, provider_invoice_id)`; the row is an observation, never an
/// invoice and never a total authority.
pub(crate) fn upsert_provider_invoice_conn(
    conn: &Connection,
    observation: &ProviderInvoiceObservation,
) -> Result<(), PaymentError> {
    conn.execute(
        "INSERT OR REPLACE INTO payment_provider_invoice \
         (provider, provider_invoice_id, organization_id, invoice_id, status, amount_minor, \
          amount_paid_minor, amount_refunded_minor, currency, paid_at_ms, observed_ms, source, \
          reconciliation, payload) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            observation.provider,
            observation.provider_invoice_id,
            observation.organization_id,
            observation.invoice_id,
            observation.status,
            observation.amount_minor,
            observation.amount_paid_minor,
            observation.amount_refunded_minor,
            observation.currency,
            observation.paid_at_ms,
            observation.observed_ms,
            observation.source.as_str(),
            observation.reconciliation.as_str(),
            encode(observation)?
        ],
    )
    .map_err(store_err)?;
    Ok(())
}

pub(crate) fn upsert_provider_payment_conn(
    conn: &Connection,
    observation: &ProviderPaymentObservation,
) -> Result<(), PaymentError> {
    conn.execute(
        "INSERT OR REPLACE INTO payment_provider_payment \
         (provider, provider_payment_id, organization_id, invoice_id, payment_id, status, \
          amount_minor, refunded_minor, currency, paid_at_ms, observed_ms, source, payload) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            observation.provider,
            observation.provider_payment_id,
            observation.organization_id,
            observation.invoice_id,
            observation.payment_id,
            observation.status,
            observation.amount_minor,
            observation.refunded_minor,
            observation.currency,
            observation.paid_at_ms,
            observation.observed_ms,
            observation.source.as_str(),
            encode(observation)?
        ],
    )
    .map_err(store_err)?;
    Ok(())
}

pub(crate) fn upsert_provider_refund_conn(
    conn: &Connection,
    observation: &ProviderRefundObservation,
) -> Result<(), PaymentError> {
    conn.execute(
        "INSERT OR REPLACE INTO payment_provider_refund \
         (provider, provider_refund_id, organization_id, invoice_id, payment_id, status, \
          amount_minor, currency, observed_ms, source, payload) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            observation.provider,
            observation.provider_refund_id,
            observation.organization_id,
            observation.invoice_id,
            observation.payment_id,
            observation.status,
            observation.amount_minor,
            observation.currency,
            observation.observed_ms,
            observation.source.as_str(),
            encode(observation)?
        ],
    )
    .map_err(store_err)?;
    Ok(())
}

pub(crate) fn apply_webhook_effects_tx(
    tx: &Connection,
    effects: &WebhookEffects,
) -> Result<(), PaymentError> {
    if let Some(subscription) = &effects.subscription {
        put_subscription_tx(tx, subscription)?;
    }
    if let Some(invoice) = &effects.invoice {
        update_invoice_tx(tx, invoice)?;
    }
    if let Some(payment) = &effects.payment {
        insert_payment_tx(tx, payment)?;
    }
    if let Some(refund) = &effects.refund {
        put_refund_tx(tx, refund)?;
    }
    if let Some(credit_note) = &effects.credit_note {
        tx.execute(
            "INSERT INTO payment_credit_note \
             (id, organization_id, number, invoice_id, payment_id, amount_minor, created_ms, \
              payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                credit_note.id,
                credit_note.organization_id,
                credit_note.number,
                credit_note.invoice_id,
                credit_note.payment_id,
                credit_note.amount_minor,
                credit_note.created_ms,
                encode(credit_note)?
            ],
        )
        .map_err(store_err)?;
    }
    if let Some(observation) = &effects.provider_invoice {
        upsert_provider_invoice_conn(tx, observation)?;
    }
    if let Some(observation) = &effects.provider_payment {
        upsert_provider_payment_conn(tx, observation)?;
    }
    if let Some(observation) = &effects.provider_refund {
        upsert_provider_refund_conn(tx, observation)?;
    }
    Ok(())
}

fn invoice_conn(
    conn: &Connection,
    organization_id: &str,
    id: &str,
) -> Result<Option<Invoice>, PaymentError> {
    let payload: Option<String> = conn
        .query_row(
            "SELECT payload FROM payment_invoice WHERE organization_id = ?1 AND id = ?2",
            params![organization_id, id],
            |row| row.get(0),
        )
        .optional()
        .map_err(store_err)?;
    payload.map(|p| decode(&p)).transpose()
}

pub(crate) fn insert_pending_proration_tx(
    tx: &Connection,
    pending: &PendingProration,
) -> Result<(), PaymentError> {
    tx.execute(
        "INSERT INTO payment_pending_proration \
         (id, organization_id, subscription_id, consumed_by, payload) \
         VALUES (?1, ?2, ?3, NULL, ?4)",
        params![
            pending.id,
            pending.organization_id,
            pending.subscription_id,
            encode(pending)?
        ],
    )
    .map_err(store_err)?;
    Ok(())
}

pub fn pending_prorations(
    store: &SqlitePaymentStore,
    organization_id: &str,
    subscription_id: &str,
) -> Result<Vec<PendingProration>, PaymentError> {
    let conn = store.lock()?;
    let mut statement = conn
        .prepare(
            "SELECT payload FROM payment_pending_proration \
             WHERE organization_id = ?1 AND subscription_id = ?2 AND consumed_by IS NULL \
             ORDER BY id",
        )
        .map_err(store_err)?;
    let rows = statement
        .query_map(params![organization_id, subscription_id], |row| {
            row.get::<_, String>(0)
        })
        .map_err(store_err)?;
    let mut items = Vec::new();
    for row in rows {
        items.push(decode(&row.map_err(store_err)?)?);
    }
    Ok(items)
}

pub(crate) fn consume_proration_tx(
    tx: &Connection,
    id: &str,
    invoice_id: &str,
) -> Result<(), PaymentError> {
    tx.execute(
        "UPDATE payment_pending_proration SET consumed_by = ?2 WHERE id = ?1",
        params![id, invoice_id],
    )
    .map_err(store_err)?;
    Ok(())
}

pub(crate) fn insert_payment_tx(tx: &Connection, payment: &Payment) -> Result<(), PaymentError> {
    tx.execute(
        "INSERT OR REPLACE INTO payment_payment \
         (id, organization_id, invoice_id, provider_payment_id, amount_minor, refunded_minor, \
          status, created_ms, payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            payment.id,
            payment.organization_id,
            payment.invoice_id,
            payment.provider_payment_id,
            payment.amount_minor,
            payment.refunded_minor,
            payment.status.as_str(),
            payment.created_ms,
            encode(payment)?
        ],
    )
    .map_err(store_err)?;
    Ok(())
}

#[allow(dead_code)]
fn insert_payment_conn(conn: &Connection, payment: &Payment) -> Result<(), PaymentError> {
    insert_payment_tx(conn, payment)
}

#[allow(dead_code)]
fn insert_refund_conn(conn: &Connection, refund: &Refund) -> Result<(), PaymentError> {
    insert_refund_tx(conn, refund)
}

#[allow(dead_code)]
fn insert_credit_note_conn(conn: &Connection, note: &CreditNote) -> Result<(), PaymentError> {
    insert_credit_note_tx(conn, note)
}

pub(crate) fn invoice_by_provider_id_tx(
    conn: &Connection,
    provider_invoice_id: &str,
) -> Result<Option<Invoice>, PaymentError> {
    let payload: Option<String> = conn
        .query_row(
            "SELECT payload FROM payment_invoice WHERE provider_invoice_id = ?1",
            params![provider_invoice_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(store_err)?;
    payload.map(|p| decode(&p)).transpose()
}

pub(crate) fn subscription_by_provider_id_tx(
    conn: &Connection,
    provider_subscription_id: &str,
) -> Result<Option<PaymentSubscription>, PaymentError> {
    let payload: Option<String> = conn
        .query_row(
            "SELECT payload FROM payment_subscription WHERE provider_subscription_id = ?1",
            params![provider_subscription_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(store_err)?;
    payload.map(|p| decode(&p)).transpose()
}

pub(crate) fn customer_tx(
    conn: &Connection,
    organization_id: &str,
) -> Result<Option<PaymentCustomer>, PaymentError> {
    let payload: Option<String> = conn
        .query_row(
            "SELECT payload FROM payment_customer WHERE organization_id = ?1",
            params![organization_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(store_err)?;
    payload.map(|p| decode(&p)).transpose()
}

pub(crate) fn subscription_by_id_tx(
    conn: &Connection,
    organization_id: &str,
    id: &str,
) -> Result<Option<PaymentSubscription>, PaymentError> {
    let payload: Option<String> = conn
        .query_row(
            "SELECT payload FROM payment_subscription WHERE organization_id = ?1 AND id = ?2",
            params![organization_id, id],
            |row| row.get(0),
        )
        .optional()
        .map_err(store_err)?;
    payload.map(|p| decode(&p)).transpose()
}

#[allow(dead_code)]
pub(crate) fn customer_organization_tx(
    conn: &Connection,
    provider: &str,
    provider_customer_id: &str,
) -> Result<Option<String>, PaymentError> {
    conn.query_row(
        "SELECT organization_id FROM payment_customer \
         WHERE provider = ?1 AND provider_customer_id = ?2",
        params![provider, provider_customer_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(store_err)
}

pub(crate) fn payment_by_provider_id_tx(
    conn: &Connection,
    organization_id: &str,
    provider_payment_id: &str,
) -> Result<Option<Payment>, PaymentError> {
    payment_by_provider_id_conn(conn, organization_id, provider_payment_id)
}

fn payment_by_provider_id_conn(
    conn: &Connection,
    organization_id: &str,
    provider_payment_id: &str,
) -> Result<Option<Payment>, PaymentError> {
    let payload: Option<String> = conn
        .query_row(
            "SELECT payload FROM payment_payment \
             WHERE organization_id = ?1 AND provider_payment_id = ?2",
            params![organization_id, provider_payment_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(store_err)?;
    payload.map(|p| decode(&p)).transpose()
}

pub(crate) fn payment_tx(
    conn: &Connection,
    organization_id: &str,
    id: &str,
) -> Result<Option<Payment>, PaymentError> {
    payment_conn(conn, organization_id, id)
}

pub(crate) fn invoice_tx(
    conn: &Connection,
    organization_id: &str,
    id: &str,
) -> Result<Option<Invoice>, PaymentError> {
    invoice_conn(conn, organization_id, id)
}

pub(crate) fn watermark_tx(
    conn: &Connection,
    organization_id: &str,
) -> Result<Option<(i64, String)>, PaymentError> {
    conn.query_row(
        "SELECT last_created_ms, last_event_id FROM payment_webhook_watermark \
         WHERE organization_id = ?1",
        params![organization_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .map_err(store_err)
}

fn payment_conn(
    conn: &Connection,
    organization_id: &str,
    id: &str,
) -> Result<Option<Payment>, PaymentError> {
    let payload: Option<String> = conn
        .query_row(
            "SELECT payload FROM payment_payment WHERE organization_id = ?1 AND id = ?2",
            params![organization_id, id],
            |row| row.get(0),
        )
        .optional()
        .map_err(store_err)?;
    payload.map(|p| decode(&p)).transpose()
}

pub(crate) fn insert_refund_tx(tx: &Connection, refund: &Refund) -> Result<(), PaymentError> {
    tx.execute(
        "INSERT INTO payment_refund \
         (id, organization_id, payment_id, provider_refund_id, status, amount_minor, created_ms, \
          payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            refund.id,
            refund.organization_id,
            refund.payment_id,
            refund.provider_refund_id,
            refund.status.as_str(),
            refund.amount_minor,
            refund.created_ms,
            encode(refund)?
        ],
    )
    .map_err(store_err)?;
    Ok(())
}

pub(crate) fn put_refund_tx(tx: &Connection, refund: &Refund) -> Result<(), PaymentError> {
    tx.execute(
        "INSERT OR REPLACE INTO payment_refund \
         (id, organization_id, payment_id, provider_refund_id, status, amount_minor, created_ms, \
          payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            refund.id,
            refund.organization_id,
            refund.payment_id,
            refund.provider_refund_id,
            refund.status.as_str(),
            refund.amount_minor,
            refund.created_ms,
            encode(refund)?
        ],
    )
    .map_err(store_err)?;
    Ok(())
}

pub(crate) fn refund_by_provider_id_conn(
    conn: &Connection,
    organization_id: &str,
    provider_refund_id: &str,
) -> Result<Option<Refund>, PaymentError> {
    let payload: Option<String> = conn
        .query_row(
            "SELECT payload FROM payment_refund \
             WHERE organization_id = ?1 AND provider_refund_id = ?2",
            params![organization_id, provider_refund_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(store_err)?;
    payload.map(|p| decode(&p)).transpose()
}

pub(crate) fn refund_by_id_tx(
    conn: &Connection,
    organization_id: &str,
    id: &str,
) -> Result<Option<Refund>, PaymentError> {
    let payload: Option<String> = conn
        .query_row(
            "SELECT payload FROM payment_refund WHERE organization_id = ?1 AND id = ?2",
            params![organization_id, id],
            |row| row.get(0),
        )
        .optional()
        .map_err(store_err)?;
    payload.map(|p| decode(&p)).transpose()
}

pub(crate) fn insert_credit_note_tx(
    tx: &Connection,
    note: &CreditNote,
) -> Result<(), PaymentError> {
    tx.execute(
        "INSERT INTO payment_credit_note \
         (id, organization_id, number, invoice_id, payment_id, amount_minor, created_ms, payload) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            note.id,
            note.organization_id,
            note.number,
            note.invoice_id,
            note.payment_id,
            note.amount_minor,
            note.created_ms,
            encode(note)?
        ],
    )
    .map_err(store_err)?;
    Ok(())
}

fn insert_claim_tx(tx: &Connection, draft: &OperationClaimDraft) -> Result<(), PaymentError> {
    tx.execute(
        "INSERT INTO payment_operation_claim \
         (organization_id, idempotency_key, operation, resource_type, resource_id, \
          request_digest, provider_idempotency_key, state, context, response, failure_code, \
          failure_message, created_ms, dispatched_ms, resolved_ms, updated_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'claimed', ?8, NULL, NULL, NULL, ?9, NULL, NULL, ?9)",
        params![
            draft.organization_id,
            draft.idempotency_key,
            draft.operation,
            draft.resource_type.as_str(),
            draft.resource_id,
            draft.request_digest,
            draft.provider_idempotency_key,
            draft.context,
            draft.created_ms
        ],
    )
    .map_err(|e| {
        if is_constraint_violation(&e) {
            PaymentError::Conflict(format!(
                "operation claim {} was acquired concurrently",
                draft.idempotency_key
            ))
        } else {
            store_err(e)
        }
    })?;
    Ok(())
}

fn complete_claim_tx(
    tx: &Connection,
    organization_id: &str,
    key: &str,
    response: &str,
    now_ms: i64,
) -> Result<(), PaymentError> {
    tx.execute(
        "UPDATE payment_operation_claim SET state = 'succeeded', response = ?3, \
         resolved_ms = ?4, updated_ms = ?4 \
         WHERE organization_id = ?1 AND idempotency_key = ?2 \
         AND state IN ('claimed','dispatched','unknown')",
        params![organization_id, key, response, now_ms],
    )
    .map_err(store_err)?;
    Ok(())
}

fn read_claim_conn(
    conn: &Connection,
    organization_id: &str,
    key: &str,
) -> Result<Option<OperationClaim>, PaymentError> {
    conn.query_row(
        "SELECT organization_id, idempotency_key, operation, resource_type, resource_id, \
         request_digest, provider_idempotency_key, state, context, response, failure_code, \
         failure_message, created_ms, dispatched_ms, resolved_ms, updated_ms \
         FROM payment_operation_claim WHERE organization_id = ?1 AND idempotency_key = ?2",
        params![organization_id, key],
        decode_claim_row,
    )
    .optional()
    .map_err(store_err)
}

fn decode_claim_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<OperationClaim> {
    let resource_type: String = row.get(3)?;
    let state: String = row.get(7)?;
    let resource_type = FinancialResourceType::parse(&resource_type).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unknown operation claim resource type",
            )),
        )
    })?;
    let state = OperationClaimState::parse(&state).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            7,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unknown operation claim state",
            )),
        )
    })?;
    Ok(OperationClaim {
        organization_id: row.get(0)?,
        idempotency_key: row.get(1)?,
        operation: row.get(2)?,
        resource_type,
        resource_id: row.get(4)?,
        request_digest: row.get(5)?,
        provider_idempotency_key: row.get(6)?,
        state,
        context: row.get(8)?,
        response: row.get(9)?,
        failure_code: row.get(10)?,
        failure_message: row.get(11)?,
        created_ms: row.get(12)?,
        dispatched_ms: row.get(13)?,
        resolved_ms: row.get(14)?,
        updated_ms: row.get(15)?,
    })
}

fn is_constraint_violation(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::ConstraintViolation,
                ..
            },
            _
        )
    )
}

pub(crate) fn next_sequence_tx(
    tx: &Connection,
    organization_id: &str,
    kind: &str,
) -> Result<i64, PaymentError> {
    let current: Option<i64> = tx
        .query_row(
            "SELECT next FROM payment_sequence WHERE organization_id = ?1 AND kind = ?2",
            params![organization_id, kind],
            |row| row.get(0),
        )
        .optional()
        .map_err(store_err)?;
    let next = current.unwrap_or(1);
    tx.execute(
        "INSERT INTO payment_sequence (organization_id, kind, next) VALUES (?1, ?2, ?3) \
         ON CONFLICT(organization_id, kind) DO UPDATE SET next = excluded.next",
        params![organization_id, kind, next + 1],
    )
    .map_err(store_err)?;
    Ok(next)
}

fn read_idempotency_conn(
    conn: &Connection,
    organization_id: &str,
    key: &str,
) -> Result<Option<IdempotencyRecord>, PaymentError> {
    conn.query_row(
        "SELECT operation, request_hash, response, created_ms FROM payment_idempotency \
         WHERE organization_id = ?1 AND key = ?2",
        params![organization_id, key],
        |row| {
            Ok(IdempotencyRecord {
                operation: row.get(0)?,
                request_hash: row.get(1)?,
                response: row.get(2)?,
                created_ms: row.get(3)?,
            })
        },
    )
    .optional()
    .map_err(store_err)
}

pub(crate) fn write_idempotency_tx(
    tx: &Connection,
    write: &IdempotencyWrite,
) -> Result<(), PaymentError> {
    tx.execute(
        "INSERT INTO payment_idempotency \
         (organization_id, key, operation, request_hash, response, created_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            write.organization_id,
            write.key,
            write.operation,
            write.request_hash,
            write.response,
            write.created_ms
        ],
    )
    .map_err(|e| {
        if matches!(
            e,
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ErrorCode::ConstraintViolation,
                    ..
                },
                _
            )
        ) {
            PaymentError::IdempotencyConflict(format!(
                "idempotency key {} was recorded concurrently",
                write.key
            ))
        } else {
            store_err(e)
        }
    })?;
    Ok(())
}

fn provider_object_conn(
    conn: &Connection,
    provider: &str,
    kind: &str,
    organization_id: &str,
    idempotency_key: &str,
) -> Result<Option<(String, serde_json::Value)>, PaymentError> {
    conn.query_row(
        "SELECT object_id, payload FROM payment_provider_object \
         WHERE provider = ?1 AND kind = ?2 AND organization_id = ?3 AND idempotency_key = ?4",
        params![provider, kind, organization_id, idempotency_key],
        |row| {
            let id: String = row.get(0)?;
            let payload: String = row.get(1)?;
            let value = serde_json::from_str(&payload).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            Ok((id, value))
        },
    )
    .optional()
    .map_err(store_err)
}

// ------------------------------------------------------------- migration

/// Backfill linked provider observations for every pre-observation invoice.
/// The Faktor invoice payload is decoded and its authored totals are copied
/// into the observation as the initial "unreconciled" baseline; a local
/// provider object that already exists is then linked with its exact
/// reported amount/currency so a pre-existing divergence surfaces instead of
/// being silently normalized.
fn backfill_provider_observations(conn: &Connection) -> Result<(), PaymentError> {
    let mut statement = conn
        .prepare(
            "SELECT organization_id, payload FROM payment_invoice \
             WHERE provider_invoice_id IS NOT NULL",
        )
        .map_err(store_err)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(store_err)?;
    let mut invoices = Vec::new();
    for row in rows {
        let (_, payload) = row.map_err(store_err)?;
        invoices.push(decode::<Invoice>(&payload)?);
    }
    drop(statement);

    for invoice in &invoices {
        let provider = provider_for_organization(conn, &invoice.organization_id)?;
        let observation = ProviderInvoiceObservation {
            provider,
            provider_invoice_id: invoice.provider_invoice_id.clone().unwrap_or_default(),
            organization_id: invoice.organization_id.clone(),
            invoice_id: invoice.id.clone(),
            status: invoice.status.as_str().to_string(),
            amount_minor: invoice.total_minor,
            amount_paid_minor: invoice.amount_paid_minor,
            amount_refunded_minor: invoice.amount_refunded_minor,
            currency: invoice.currency.clone(),
            paid_at_ms: invoice.paid_ms,
            observed_ms: invoice.created_ms,
            source: ProviderObservationSource::Migration,
            reconciliation: InvoiceReconciliationState::Unreconciled,
        };
        upsert_provider_invoice_conn(conn, &observation)?;
    }

    let mut objects = conn
        .prepare(
            "SELECT provider, organization_id, payload FROM payment_provider_object \
             WHERE kind IN ('invoice','invoice_by_id')",
        )
        .map_err(store_err)?;
    let rows = objects
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(store_err)?;
    let mut observed = Vec::new();
    for row in rows {
        let (provider, organization_id, payload) = row.map_err(store_err)?;
        let value: serde_json::Value = serde_json::from_str(&payload)
            .map_err(|e| PaymentError::Store(format!("migration provider object decoding: {e}")))?;
        let object = value.get("object").cloned().unwrap_or(value);
        let Some(provider_invoice_id) = object.get("id").and_then(|id| id.as_str()) else {
            continue;
        };
        let Some(invoice) = invoices
            .iter()
            .find(|invoice| invoice.provider_invoice_id.as_deref() == Some(provider_invoice_id))
        else {
            continue;
        };
        let amount_minor = object
            .get("amount_minor")
            .and_then(|amount| amount.as_i64())
            .unwrap_or(invoice.total_minor);
        let currency = object
            .get("currency")
            .and_then(|currency| currency.as_str())
            .unwrap_or(&invoice.currency)
            .to_string();
        let status = object
            .get("status")
            .and_then(|status| status.as_str())
            .unwrap_or("unknown")
            .to_string();
        let reconciliation = if currency != invoice.currency {
            InvoiceReconciliationState::CurrencyMismatch
        } else if amount_minor != invoice.total_minor {
            InvoiceReconciliationState::AmountMismatch
        } else {
            InvoiceReconciliationState::Matched
        };
        observed.push(ProviderInvoiceObservation {
            provider,
            provider_invoice_id: provider_invoice_id.to_string(),
            organization_id: organization_id.clone(),
            invoice_id: invoice.id.clone(),
            amount_paid_minor: if status == "paid" { amount_minor } else { 0 },
            status,
            amount_minor,
            amount_refunded_minor: 0,
            currency,
            paid_at_ms: None,
            observed_ms: invoice.created_ms,
            source: ProviderObservationSource::Migration,
            reconciliation,
        });
    }
    drop(objects);
    for observation in &observed {
        upsert_provider_invoice_conn(conn, observation)?;
    }
    // Reflect the linked observation's typed state on the Faktor invoice
    // itself (payload + indexed column). The authored money is untouched.
    for invoice in &mut invoices {
        if let Some(observation) = observed
            .iter()
            .find(|observation| observation.invoice_id == invoice.id)
        {
            invoice.reconciliation = observation.reconciliation;
            update_invoice_tx(conn, invoice)?;
        }
    }
    Ok(())
}

fn provider_for_organization(
    conn: &Connection,
    organization_id: &str,
) -> Result<String, PaymentError> {
    conn.query_row(
        "SELECT provider FROM payment_customer WHERE organization_id = ?1",
        params![organization_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(store_err)
    .map(|provider| provider.unwrap_or_else(|| "local".to_string()))
}

// ------------------------------------------------------------- encoding

pub(crate) fn encode<T: Serialize>(value: &T) -> Result<String, PaymentError> {
    serde_json::to_string(value)
        .map_err(|e| PaymentError::Store(format!("payment row encoding: {e}")))
}

fn decode<T: DeserializeOwned>(payload: &str) -> Result<T, PaymentError> {
    serde_json::from_str(payload)
        .map_err(|e| PaymentError::Store(format!("payment row decoding: {e}")))
}

fn store_err(e: rusqlite::Error) -> PaymentError {
    PaymentError::Store(format!("payment store: {e}"))
}

/// Page limits arrive as `usize` but bind into SQLite's signed range: a
/// limit above `i64::MAX` is refused typed instead of wrapping negative
/// (SQLite reads a negative LIMIT as unbounded). `extra` is the deliberate
/// `+1` probe used to detect whether another page exists.
fn checked_page_limit(limit: usize, extra: usize) -> Result<i64, PaymentError> {
    let value = limit
        .checked_add(extra)
        .ok_or_else(|| PaymentError::Malformed(format!("page limit {limit} overflows usize")))?;
    i64::try_from(value).map_err(|_| {
        PaymentError::Malformed(format!("page limit {value} exceeds SQLite signed range"))
    })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The default webhook window is re-exported for the service.
pub const WEBHOOK_WINDOW_MS: i64 = DEFAULT_WEBHOOK_WINDOW_MS;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{InvoiceStatus, PaymentStatus, SubscriptionState};

    fn subscription() -> PaymentSubscription {
        PaymentSubscription {
            id: "sub_1".into(),
            organization_id: "org_1".into(),
            customer_id: "cus_1".into(),
            provider_subscription_id: "provider_sub_1".into(),
            provider_item_id: None,
            provider_product_id: None,
            provider_price_id: None,
            plan_id: "pro".into(),
            interval: crate::catalog::BillingInterval::Monthly,
            quantity: 2,
            state: SubscriptionState::Active,
            currency: "USD".into(),
            current_period_start_ms: 0,
            current_period_end_ms: 100,
            trial_end_ms: None,
            cancel_at_period_end: false,
            canceled_ms: None,
            dunning_attempts: 0,
            grace_end_ms: None,
            created_ms: 0,
            updated_ms: 0,
        }
    }

    fn invoice(status: InvoiceStatus) -> Invoice {
        Invoice {
            id: "inv_1".into(),
            organization_id: "org_1".into(),
            number: "INV-000001".into(),
            subscription_id: Some("sub_1".into()),
            provider_invoice_id: Some("provider_inv_1".into()),
            provider_idempotency_key: None,
            reconciliation: Default::default(),
            currency: "USD".into(),
            status,
            lines: vec![],
            subtotal_minor: 1_000,
            discount_minor: 0,
            credits_minor: 0,
            tax_minor: 0,
            tax_jurisdiction: None,
            tax_inclusive: false,
            total_minor: 1_000,
            amount_paid_minor: 0,
            amount_refunded_minor: 0,
            period_start_ms: None,
            period_end_ms: None,
            issued_ms: 1,
            due_ms: 2,
            paid_ms: None,
            voided_ms: None,
            created_ms: 1,
        }
    }

    #[test]
    fn store_roundtrips_tenant_scoped_rows_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payment.db");
        {
            let store = SqlitePaymentStore::open(&path).unwrap();
            store.put_subscription(&subscription()).unwrap();
            let write = IdempotencyWrite {
                organization_id: "org_1".into(),
                key: "k".into(),
                operation: "test".into(),
                request_hash: "hash".into(),
                response: "{\"ok\":true}".into(),
                created_ms: 1,
            };
            store
                .record_atomic(&write, |tx| {
                    insert_invoice_tx(tx, &invoice(InvoiceStatus::Open), &[])
                })
                .unwrap();
            assert_eq!(
                store
                    .invoice("org_1", "inv_1")
                    .unwrap()
                    .unwrap()
                    .total_minor,
                1_000
            );
            assert!(store.invoice("org_2", "inv_1").unwrap().is_none());
            assert_eq!(
                store.invoice_stats("org_1").unwrap().outstanding_minor,
                1_000
            );
        }
        let store = SqlitePaymentStore::open(&path).unwrap();
        let record = store.read_idempotency("org_1", "k").unwrap().unwrap();
        assert_eq!(record.response.as_deref(), Some("{\"ok\":true}"));
        assert!(store.read_idempotency("org_2", "k").unwrap().is_none());
        assert!(store.subscription("org_1").unwrap().is_some());
    }

    #[test]
    fn idempotency_race_or_reuse_is_typed() {
        let store = SqlitePaymentStore::open_in_memory().unwrap();
        let write = IdempotencyWrite {
            organization_id: "org_1".into(),
            key: "same".into(),
            operation: "op".into(),
            request_hash: "a".into(),
            response: "{}".into(),
            created_ms: 1,
        };
        store.record_atomic(&write, |_| Ok(())).unwrap();
        let err = store.record_atomic(&write, |_| Ok(())).unwrap_err();
        assert!(matches!(err, PaymentError::Conflict(_)));
        let changed = IdempotencyWrite {
            request_hash: "b".into(),
            ..write
        };
        assert!(matches!(
            store.record_atomic(&changed, |_| Ok(())),
            Err(PaymentError::IdempotencyConflict(_))
        ));
    }

    #[test]
    fn refunds_payments_and_defaults_are_tenant_scoped() {
        let store = SqlitePaymentStore::open_in_memory().unwrap();
        let payment = Payment {
            id: "pay_1".into(),
            organization_id: "org_1".into(),
            invoice_id: "inv_1".into(),
            provider: "local".into(),
            provider_payment_id: "provider_pay_1".into(),
            payment_method_id: None,
            amount_minor: 1_000,
            refunded_minor: 0,
            currency: "USD".into(),
            status: PaymentStatus::Succeeded,
            created_ms: 1,
        };
        store
            .record_atomic(
                &IdempotencyWrite {
                    organization_id: "org_1".into(),
                    key: "pay".into(),
                    operation: "pay".into(),
                    request_hash: "h".into(),
                    response: "{}".into(),
                    created_ms: 1,
                },
                |tx| insert_payment_tx(tx, &payment),
            )
            .unwrap();
        assert!(store.payment("org_1", "pay_1").unwrap().is_some());
        assert!(store.payment("org_2", "pay_1").unwrap().is_none());
        let method = StoredPaymentMethod {
            method: crate::model::PaymentMethod {
                id: "pm_1".into(),
                organization_id: "org_1".into(),
                provider: "local".into(),
                brand: "visa".into(),
                last4: "4242".into(),
                exp_month: 12,
                exp_year: 2030,
                is_default: true,
                created_ms: 1,
            },
            token_ciphertext: "v1:sealed".into(),
        };
        store.put_payment_method(&method).unwrap();
        assert_eq!(store.payment_methods("org_1").unwrap().len(), 1);
        assert!(store.payment_methods("org_2").unwrap().is_empty());
    }

    #[test]
    fn v2_dual_rows_migrate_to_linked_observations_without_touching_totals() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payment.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE payment_customer (
                    organization_id TEXT PRIMARY KEY,
                    provider TEXT NOT NULL,
                    provider_customer_id TEXT NOT NULL,
                    payload TEXT NOT NULL
                );
                CREATE TABLE payment_invoice (
                    id TEXT PRIMARY KEY,
                    organization_id TEXT NOT NULL,
                    number TEXT NOT NULL,
                    provider_invoice_id TEXT,
                    status TEXT NOT NULL,
                    issued_ms INTEGER NOT NULL,
                    total_minor INTEGER NOT NULL,
                    amount_paid_minor INTEGER NOT NULL,
                    payload TEXT NOT NULL,
                    UNIQUE (organization_id, number)
                );
                CREATE TABLE payment_provider_object (
                    provider TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    organization_id TEXT NOT NULL,
                    idempotency_key TEXT NOT NULL,
                    object_id TEXT NOT NULL,
                    payload TEXT NOT NULL,
                    created_ms INTEGER NOT NULL,
                    PRIMARY KEY (provider, kind, organization_id, idempotency_key)
                );
                PRAGMA user_version = 2;",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO payment_customer VALUES ('org_1','local','cus_1','{}')",
                [],
            )
            .unwrap();
            let payload = |id: &str, total: i64, provider: &str| {
                serde_json::json!({
                    "id": id,
                    "organization_id": "org_1",
                    "number": format!("INV-{id}"),
                    "subscription_id": null,
                    "provider_invoice_id": provider,
                    "currency": "USD",
                    "status": "open",
                    "lines": [],
                    "subtotal_minor": total,
                    "discount_minor": 0,
                    "credits_minor": 0,
                    "tax_minor": 0,
                    "tax_jurisdiction": null,
                    "tax_inclusive": false,
                    "total_minor": total,
                    "amount_paid_minor": 0,
                    "amount_refunded_minor": 0,
                    "period_start_ms": null,
                    "period_end_ms": null,
                    "issued_ms": 1,
                    "due_ms": 2,
                    "paid_ms": null,
                    "voided_ms": null,
                    "created_ms": 1
                })
                .to_string()
            };
            for (id, total, provider) in [
                ("inv_match", 1_234_i64, "prov_match"),
                ("inv_conflict", 2_000_i64, "prov_conflict"),
            ] {
                conn.execute(
                    "INSERT INTO payment_invoice VALUES (?1,'org_1',?2,?3,'open',1,?4,0,?5)",
                    params![
                        id,
                        format!("INV-{id}"),
                        provider,
                        total,
                        payload(id, total, provider)
                    ],
                )
                .unwrap();
            }
            for (provider_id, amount) in [("prov_match", 1_234_i64), ("prov_conflict", 1_999_i64)] {
                conn.execute(
                    "INSERT INTO payment_provider_object VALUES ('local','invoice','org_1',?1,?2,?3,1)",
                    params![
                        provider_id,
                        provider_id,
                        serde_json::json!({"object": {
                            "id": provider_id,
                            "status": "open",
                            "amount_minor": amount,
                            "currency": "USD"
                        }})
                        .to_string()
                    ],
                )
                .unwrap();
            }
        }

        let store = SqlitePaymentStore::open(&path).unwrap();
        let matched = store.invoice("org_1", "inv_match").unwrap().unwrap();
        assert_eq!(matched.total_minor, 1_234);
        assert_eq!(matched.reconciliation, InvoiceReconciliationState::Matched);
        assert_eq!(
            store
                .provider_invoice_observation("local", "prov_match")
                .unwrap()
                .unwrap()
                .invoice_id,
            "inv_match"
        );

        let conflict = store.invoice("org_1", "inv_conflict").unwrap().unwrap();
        assert_eq!(conflict.total_minor, 2_000, "Faktor total is unchanged");
        assert_eq!(
            conflict.reconciliation,
            InvoiceReconciliationState::AmountMismatch
        );
        let observation = store
            .provider_invoice_observation("local", "prov_conflict")
            .unwrap()
            .unwrap();
        assert_eq!(observation.amount_minor, 1_999);
        assert_eq!(
            observation.reconciliation,
            InvoiceReconciliationState::AmountMismatch
        );
    }
}
