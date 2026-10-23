//! The durable payment/invoice store. One SQLite database (its own
//! `payment.db`, `PRAGMA user_version = 1`) owns customers, subscriptions,
//! invoices, payments, refunds, credit notes, payment methods, local-provider
//! objects, webhook idempotency/watermarks and request-bound idempotency.
//!
//! Every mutation that matters to money runs in ONE immediate transaction:
//! the effect rows and the idempotency record commit together, so a crash
//! either leaves neither (safe to retry with the provider idempotency key)
//! or both (a retry replays the recorded response).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{de::DeserializeOwned, Serialize};

use crate::error::PaymentError;
use crate::model::{
    CreditNote, Invoice, InvoicePage, Payment, PaymentCustomer, PaymentMethod, PaymentSubscription,
    PendingProration, Refund,
};
use crate::webhook::DEFAULT_WEBHOOK_WINDOW_MS;

/// The single schema version.
pub const PAYMENT_SCHEMA_V1: i64 = 1;

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
    amount_minor INTEGER NOT NULL,
    created_ms INTEGER NOT NULL,
    payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_payment_refund_payment
    ON payment_refund(payment_id);

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
        if version > PAYMENT_SCHEMA_V1 {
            return Err(PaymentError::Store(format!(
                "payment database schema user_version {version} is newer than this build \
                 ({PAYMENT_SCHEMA_V1})"
            )));
        }
        if version < PAYMENT_SCHEMA_V1 {
            conn.execute_batch("BEGIN IMMEDIATE").map_err(store_err)?;
            if let Err(e) = conn.execute_batch(SCHEMA) {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(store_err(e));
            }
            if let Err(e) =
                conn.execute_batch(&format!("PRAGMA user_version = {PAYMENT_SCHEMA_V1}"))
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
        let tx = conn.transaction().map_err(store_err)?;
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

    pub fn put_payment_method(&self, method: &PaymentMethod) -> Result<(), PaymentError> {
        self.tx(|tx| put_payment_method_tx(tx, method))
    }

    pub fn payment_method(
        &self,
        organization_id: &str,
        id: &str,
    ) -> Result<Option<PaymentMethod>, PaymentError> {
        let conn = self.lock()?;
        payment_method_conn(&conn, organization_id, id)
    }

    pub fn payment_methods(
        &self,
        organization_id: &str,
    ) -> Result<Vec<PaymentMethod>, PaymentError> {
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
            let mut method: PaymentMethod = decode(&payload)?;
            method.is_default = is_default != 0;
            methods.push(method);
        }
        Ok(methods)
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

    // -------------------------------------------------------------- payments

    pub fn payment(
        &self,
        organization_id: &str,
        id: &str,
    ) -> Result<Option<Payment>, PaymentError> {
        let conn = self.lock()?;
        payment_conn(&conn, organization_id, id)
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
             WHERE organization_id = ?1 AND payment_id = ?2",
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
            let value = effect(tx)?;
            write_idempotency_tx(tx, write)?;
            Ok(value)
        })
    }

    pub fn next_sequence(&self, organization_id: &str, kind: &str) -> Result<i64, PaymentError> {
        self.tx(|tx| next_sequence_tx(tx, organization_id, kind))
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

    /// Record a webhook and apply its effects in one transaction.
    pub fn record_webhook(
        &self,
        record: &WebhookRecord,
        effects: &WebhookEffects,
        advance_watermark: bool,
    ) -> Result<(), PaymentError> {
        self.tx(|tx| {
            if let Some(subscription) = &effects.subscription {
                put_subscription_conn(tx, subscription)?;
            }
            if let Some(invoice) = &effects.invoice {
                update_invoice_conn(tx, invoice)?;
            }
            if let Some(payment) = &effects.payment {
                insert_payment_conn(tx, payment)?;
            }
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
) -> Result<Option<PaymentMethod>, PaymentError> {
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
            let mut method: PaymentMethod = decode(&payload)?;
            method.is_default = is_default != 0;
            Ok(Some(method))
        }
    }
}

pub(crate) fn put_payment_method_tx(
    tx: &Connection,
    method: &PaymentMethod,
) -> Result<(), PaymentError> {
    if method.is_default {
        tx.execute(
            "UPDATE payment_method SET is_default = 0 WHERE organization_id = ?1",
            params![method.organization_id],
        )
        .map_err(store_err)?;
    }
    tx.execute(
        "INSERT OR REPLACE INTO payment_method (id, organization_id, is_default, payload) \
         VALUES (?1, ?2, ?3, ?4)",
        params![
            method.id,
            method.organization_id,
            i64::from(method.is_default),
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
         (id, organization_id, number, provider_invoice_id, status, issued_ms, total_minor, \
          amount_paid_minor, payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            invoice.id,
            invoice.organization_id,
            invoice.number,
            invoice.provider_invoice_id,
            invoice.status.as_str(),
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
         total_minor = ?4, amount_paid_minor = ?5, payload = ?6 WHERE id = ?1",
        params![
            invoice.id,
            invoice.provider_invoice_id,
            invoice.status.as_str(),
            invoice.total_minor,
            invoice.amount_paid_minor,
            encode(invoice)?
        ],
    )
    .map_err(store_err)?;
    Ok(())
}

fn update_invoice_conn(conn: &Connection, invoice: &Invoice) -> Result<(), PaymentError> {
    update_invoice_tx(conn, invoice)
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

fn insert_payment_conn(conn: &Connection, payment: &Payment) -> Result<(), PaymentError> {
    insert_payment_tx(conn, payment)
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
         (id, organization_id, payment_id, amount_minor, created_ms, payload) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            refund.id,
            refund.organization_id,
            refund.payment_id,
            refund.amount_minor,
            refund.created_ms,
            encode(refund)?
        ],
    )
    .map_err(store_err)?;
    Ok(())
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
        let method = PaymentMethod {
            id: "pm_1".into(),
            organization_id: "org_1".into(),
            provider: "local".into(),
            token: "tok".into(),
            brand: "visa".into(),
            last4: "4242".into(),
            exp_month: 12,
            exp_year: 2030,
            is_default: true,
            created_ms: 1,
        };
        store.put_payment_method(&method).unwrap();
        assert_eq!(store.payment_methods("org_1").unwrap().len(), 1);
        assert!(store.payment_methods("org_2").unwrap().is_empty());
    }
}
