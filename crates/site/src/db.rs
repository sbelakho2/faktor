use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::Connection;

use crate::error::{SiteError, SiteResult};

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY,
    email TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    password_hash TEXT NOT NULL,
    email_verified INTEGER NOT NULL DEFAULT 0,
    is_platform_admin INTEGER NOT NULL DEFAULT 0,
    disabled INTEGER NOT NULL DEFAULT 0,
    created_ms INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS organizations (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    slug TEXT NOT NULL UNIQUE,
    created_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS memberships (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    organization_id TEXT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    role TEXT NOT NULL CHECK (role IN ('owner','admin','member')),
    created_ms INTEGER NOT NULL,
    PRIMARY KEY (user_id, organization_id)
);
CREATE INDEX IF NOT EXISTS memberships_org_idx ON memberships(organization_id);
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    organization_id TEXT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    secret_hash TEXT NOT NULL UNIQUE,
    csrf TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    last_seen_ms INTEGER NOT NULL,
    expires_ms INTEGER NOT NULL,
    revoked_ms INTEGER,
    client_tag TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS sessions_user_idx ON sessions(user_id);
CREATE INDEX IF NOT EXISTS sessions_expiry_idx ON sessions(expires_ms);
CREATE TABLE IF NOT EXISTS email_events (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('verify_email','reset_password')),
    secret_hash TEXT NOT NULL UNIQUE,
    email TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    expires_ms INTEGER NOT NULL,
    used_ms INTEGER
);
CREATE INDEX IF NOT EXISTS email_events_user_idx ON email_events(user_id, kind);
CREATE TABLE IF NOT EXISTS billing_checkouts (
    id TEXT PRIMARY KEY,
    organization_id TEXT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    invoice_id TEXT NOT NULL,
    plan_id TEXT NOT NULL,
    interval TEXT NOT NULL,
    amount_minor INTEGER NOT NULL,
    currency TEXT NOT NULL,
    provider TEXT NOT NULL,
    provider_checkout_id TEXT,
    checkout_url TEXT,
    status TEXT NOT NULL DEFAULT 'open',
    idempotency_key TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL,
    UNIQUE (organization_id, idempotency_key)
);
CREATE INDEX IF NOT EXISTS billing_checkouts_org_idx ON billing_checkouts(organization_id, created_ms);
CREATE TABLE IF NOT EXISTS admin_audit (
    id TEXT PRIMARY KEY,
    actor_user_id TEXT NOT NULL,
    organization_id TEXT,
    action TEXT NOT NULL,
    target TEXT NOT NULL,
    detail TEXT NOT NULL,
    created_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS admin_audit_created_idx ON admin_audit(created_ms);
"#;

pub struct Db {
    connection: Mutex<Connection>,
    path: Option<String>,
}

impl Db {
    pub fn open(path: &Path) -> SiteResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        Self::initialize(connection, Some(path.display().to_string()))
    }

    pub fn open_in_memory() -> SiteResult<Self> {
        let connection = Connection::open_in_memory()?;
        Self::initialize(connection, None)
    }

    fn initialize(connection: Connection, path: Option<String>) -> SiteResult<Self> {
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        if path.is_some() {
            let _: String =
                connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        }
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < 1 {
            connection.execute_batch(SCHEMA_V1)?;
            connection.pragma_update(None, "user_version", 1)?;
        }
        if version > 1 {
            return Err(SiteError::internal_message(
                "site database was written by a newer schema version",
            ));
        }
        Ok(Self {
            connection: Mutex::new(connection),
            path,
        })
    }

    pub fn conn(&self) -> SiteResult<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| SiteError::internal_message("site database lock is poisoned"))
    }

    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_idempotent_and_foreign_keys_hold() {
        let db = Db::open_in_memory().unwrap();
        {
            let conn = db.conn().unwrap();
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .unwrap();
            assert_eq!(version, 1);
            let orphan = conn.execute(
                "INSERT INTO memberships (user_id, organization_id, role, created_ms) VALUES ('u','o','owner',0)",
                [],
            );
            assert!(orphan.is_err());
            let raw_pan_columns: i64 = conn
                .query_row(
                    "SELECT count(*) FROM pragma_table_info('billing_checkouts') WHERE lower(name) IN ('pan','card_number','cvc','cvv')",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(raw_pan_columns, 0);
        }
        drop(db);
        let reopened = Db::open_in_memory().unwrap();
        let conn = reopened.conn().unwrap();
        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='sessions'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1);
    }
}
