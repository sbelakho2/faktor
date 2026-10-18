//! One durable SQLite database for the Edition 5 daemon foundation.
//!
//! Every table is created by a numbered migration; `PRAGMA user_version`
//! is the schema generation. Writers share one connection behind a mutex
//! (the foundation's write volume is settings/lifecycle scale), readers
//! open their own connection. All mutating operations run in an explicit
//! transaction so a crash can never expose a partial revision.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

use crate::FoundationError;

/// Current foundation schema generation. Bump with every migration.
pub const SCHEMA_VERSION: i64 = 3;

/// One migration per generation: index 0 upgrades 0 -> 1.
const MIGRATIONS: &[&str] = &[
    // v1 — configuration authority, secret references, provider lifecycle,
    // model call receipts, context manifests, persistent permission policy,
    // customization lifecycle (MCP/skills/hooks) and diagnostics state.
    "CREATE TABLE IF NOT EXISTS config_meta (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        revision INTEGER NOT NULL,
        runtime_revision INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL
     );
     INSERT OR IGNORE INTO config_meta(id, revision, runtime_revision, updated_ms)
        VALUES (1, 0, 0, 0);
     CREATE TABLE IF NOT EXISTS config_value (
        key TEXT NOT NULL,
        scope TEXT NOT NULL,
        value_json TEXT NOT NULL,
        revision INTEGER NOT NULL,
        PRIMARY KEY (key, scope)
     );
     CREATE TABLE IF NOT EXISTS config_history (
        revision INTEGER NOT NULL,
        key TEXT NOT NULL,
        scope TEXT NOT NULL,
        before_json TEXT,
        after_json TEXT,
        created_ms INTEGER NOT NULL,
        PRIMARY KEY (revision, key, scope)
     );
     CREATE INDEX IF NOT EXISTS idx_config_history_revision
        ON config_history(revision);
     CREATE TABLE IF NOT EXISTS secret_ref (
        handle TEXT PRIMARY KEY,
        kind TEXT NOT NULL,
        label TEXT NOT NULL,
        generation INTEGER NOT NULL,
        verified INTEGER NOT NULL,
        revoked INTEGER NOT NULL,
        created_ms INTEGER NOT NULL,
        rotated_ms INTEGER
     );
     CREATE TABLE IF NOT EXISTS provider_connection (
        id TEXT PRIMARY KEY,
        family TEXT NOT NULL,
        label TEXT NOT NULL,
        endpoint TEXT,
        credential_ref TEXT,
        models_json TEXT NOT NULL,
        allow_loopback INTEGER NOT NULL,
        api_flavor TEXT,
        created_ms INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL,
        revision INTEGER NOT NULL,
        apply_state TEXT NOT NULL,
        trust_origin TEXT NOT NULL
     );
     CREATE TABLE IF NOT EXISTS provider_health (
        connection_id TEXT PRIMARY KEY,
        state TEXT NOT NULL,
        stages_json TEXT NOT NULL,
        observed_ms INTEGER NOT NULL,
        detail TEXT
     );
     CREATE TABLE IF NOT EXISTS call_receipt (
        id INTEGER PRIMARY KEY,
        session_id INTEGER NOT NULL,
        op_id TEXT NOT NULL,
        call_id TEXT NOT NULL,
        provider TEXT NOT NULL,
        model TEXT NOT NULL,
        route TEXT NOT NULL,
        capability_revision INTEGER NOT NULL,
        parameters_json TEXT NOT NULL,
        requested_json TEXT NOT NULL,
        inherited_json TEXT NOT NULL,
        started_ms INTEGER NOT NULL,
        ended_ms INTEGER,
        status TEXT NOT NULL,
        tokens_in INTEGER,
        tokens_out INTEGER,
        error TEXT,
        UNIQUE (session_id, call_id)
     );
     CREATE INDEX IF NOT EXISTS idx_call_receipt_session
        ON call_receipt(session_id, id);
     CREATE TABLE IF NOT EXISTS context_manifest (
        session_id INTEGER NOT NULL,
        revision INTEGER NOT NULL,
        operation_id TEXT NOT NULL,
        call_id TEXT,
        model TEXT NOT NULL,
        capacity INTEGER NOT NULL,
        input_tokens INTEGER,
        output_reserve INTEGER NOT NULL,
        safety_reserve INTEGER NOT NULL,
        basis_kind TEXT NOT NULL,
        basis_method TEXT,
        basis_version TEXT,
        basis_detail TEXT,
        sources_json TEXT NOT NULL,
        created_ms INTEGER NOT NULL,
        PRIMARY KEY (session_id, revision)
     );
     CREATE TABLE IF NOT EXISTS permission_policy_meta (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        revision INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL
     );
     INSERT OR IGNORE INTO permission_policy_meta(id, revision, updated_ms)
        VALUES (1, 0, 0);
     CREATE TABLE IF NOT EXISTS permission_rule (
        id TEXT PRIMARY KEY,
        scope TEXT NOT NULL,
        tool_id TEXT NOT NULL,
        target TEXT NOT NULL,
        decision TEXT NOT NULL,
        created_ms INTEGER NOT NULL,
        expires_ms INTEGER,
        revision INTEGER NOT NULL,
        managed INTEGER NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_permission_rule_tool
        ON permission_rule(tool_id);
     CREATE TABLE IF NOT EXISTS permission_audit (
        id INTEGER PRIMARY KEY,
        decision TEXT NOT NULL,
        tool_id TEXT NOT NULL,
        target TEXT NOT NULL,
        rule_id TEXT,
        reason TEXT NOT NULL,
        revision INTEGER NOT NULL,
        decided_ms INTEGER NOT NULL,
        source TEXT NOT NULL
     );
     CREATE TABLE IF NOT EXISTS mcp_server (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        command TEXT NOT NULL,
        args_json TEXT NOT NULL,
        env_json TEXT NOT NULL,
        trust_origin TEXT NOT NULL,
        trusted INTEGER NOT NULL,
        enabled INTEGER NOT NULL,
        created_ms INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL,
        revision INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS skill_record (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        origin TEXT NOT NULL,
        source_uri TEXT NOT NULL,
        version INTEGER NOT NULL,
        content_hash TEXT NOT NULL,
        trusted INTEGER NOT NULL,
        invocation TEXT NOT NULL,
        required_capabilities_json TEXT NOT NULL,
        text TEXT NOT NULL,
        updated_ms INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS hook_record (
        id TEXT PRIMARY KEY,
        spec_json TEXT NOT NULL,
        trusted INTEGER NOT NULL,
        enabled INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL,
        revision INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS context_revision (
        session_id INTEGER PRIMARY KEY,
        revision INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS session_ui_meta (
        session_id INTEGER PRIMARY KEY,
        pinned INTEGER NOT NULL DEFAULT 0,
        archived INTEGER NOT NULL DEFAULT 0,
        updated_ms INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS ops_record (
        id TEXT PRIMARY KEY,
        session_id INTEGER NOT NULL,
        kind TEXT NOT NULL,
        state TEXT NOT NULL,
        effect_status TEXT NOT NULL,
        detail_json TEXT NOT NULL,
        created_ms INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_ops_record_session
        ON ops_record(session_id, created_ms);
     CREATE INDEX IF NOT EXISTS idx_context_manifest_call
        ON context_manifest(session_id, call_id);
     CREATE INDEX IF NOT EXISTS idx_permission_audit_ms
        ON permission_audit(decided_ms);
     CREATE INDEX IF NOT EXISTS idx_mcp_server_name
        ON mcp_server(name);
     CREATE INDEX IF NOT EXISTS idx_skill_record_name
        ON skill_record(name);",
    // v2 — Revision 8 contract layer: durable instruction/rule records and
    // captured bundles, agent presets + admission bindings, context
    // pin/exclude decisions, runtime executable trust, browser adapter
    // lifecycle/observations and the durable compaction operation ledger.
    "CREATE TABLE IF NOT EXISTS instruction_record (
        id TEXT PRIMARY KEY,
        kind TEXT NOT NULL,
        title TEXT NOT NULL,
        content TEXT NOT NULL,
        origin TEXT NOT NULL,
        scope TEXT NOT NULL,
        activation TEXT NOT NULL,
        globs_json TEXT NOT NULL,
        enabled INTEGER NOT NULL,
        locked INTEGER NOT NULL,
        version INTEGER NOT NULL,
        content_hash TEXT NOT NULL,
        created_ms INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL,
        revision INTEGER NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_instruction_record_kind
        ON instruction_record(kind, scope);
     CREATE TABLE IF NOT EXISTS instruction_bundle (
        session_id INTEGER NOT NULL,
        revision INTEGER NOT NULL,
        operation_id TEXT NOT NULL,
        bundle_hash TEXT NOT NULL,
        content TEXT NOT NULL,
        records_json TEXT NOT NULL,
        created_ms INTEGER NOT NULL,
        PRIMARY KEY (session_id, revision)
     );
     CREATE INDEX IF NOT EXISTS idx_instruction_bundle_operation
        ON instruction_bundle(session_id, operation_id);
     CREATE TABLE IF NOT EXISTS preset_record (
        id TEXT PRIMARY KEY,
        mode TEXT NOT NULL,
        name TEXT NOT NULL,
        description TEXT NOT NULL,
        origin TEXT NOT NULL,
        scope TEXT NOT NULL,
        role_instructions TEXT NOT NULL,
        model_provider TEXT,
        model_id TEXT,
        reasoning_effort TEXT,
        tool_allow_json TEXT NOT NULL,
        tool_deny_json TEXT NOT NULL,
        delegation_json TEXT NOT NULL,
        child_concurrency INTEGER NOT NULL,
        budget_share_ppm INTEGER NOT NULL,
        locked INTEGER NOT NULL,
        version INTEGER NOT NULL,
        created_ms INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL,
        revision INTEGER NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_preset_record_name
        ON preset_record(name);
     CREATE TABLE IF NOT EXISTS preset_binding (
        session_id INTEGER NOT NULL,
        submission_id TEXT NOT NULL,
        preset_id TEXT NOT NULL,
        preset_version INTEGER NOT NULL,
        envelope_json TEXT NOT NULL,
        created_ms INTEGER NOT NULL,
        PRIMARY KEY (session_id, submission_id)
     );
     CREATE TABLE IF NOT EXISTS context_pin (
        session_id INTEGER NOT NULL,
        source_id TEXT NOT NULL,
        action TEXT NOT NULL,
        updated_ms INTEGER NOT NULL,
        PRIMARY KEY (session_id, source_id)
     );
     CREATE TABLE IF NOT EXISTS runtime_trust (
        executable_path TEXT PRIMARY KEY,
        checksum TEXT NOT NULL,
        publisher TEXT,
        signature_state TEXT NOT NULL,
        confirmed_ms INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS browser_adapter (
        id TEXT PRIMARY KEY,
        mode TEXT NOT NULL,
        endpoint TEXT,
        account_scope TEXT,
        advertised INTEGER NOT NULL,
        network_scope TEXT NOT NULL,
        updated_ms INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS browser_session (
        id TEXT PRIMARY KEY,
        adapter_id TEXT NOT NULL,
        session_id INTEGER NOT NULL,
        url TEXT NOT NULL,
        host TEXT NOT NULL,
        state TEXT NOT NULL,
        created_ms INTEGER NOT NULL,
        closed_ms INTEGER
     );
     CREATE INDEX IF NOT EXISTS idx_browser_session_session
        ON browser_session(session_id);
     CREATE TABLE IF NOT EXISTS browser_artifact (
        id TEXT PRIMARY KEY,
        browser_session_id TEXT NOT NULL,
        mime TEXT NOT NULL,
        bytes INTEGER NOT NULL,
        checksum TEXT NOT NULL,
        in_model_context INTEGER NOT NULL,
        data BLOB NOT NULL,
        captured_ms INTEGER NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_browser_artifact_session
        ON browser_artifact(browser_session_id, captured_ms);
     CREATE TABLE IF NOT EXISTS browser_observation (
        id INTEGER PRIMARY KEY,
        browser_session_id TEXT NOT NULL,
        kind TEXT NOT NULL,
        severity TEXT,
        payload_json TEXT NOT NULL,
        captured_ms INTEGER NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_browser_observation_session
        ON browser_observation(browser_session_id, id);
     CREATE TABLE IF NOT EXISTS compaction_operation (
        session_id INTEGER NOT NULL,
        operation_id TEXT NOT NULL,
        state TEXT NOT NULL,
        request_json TEXT NOT NULL,
        result_json TEXT,
        created_ms INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL,
        PRIMARY KEY (session_id, operation_id)
     );",
    // v3 — closing the Rev8 field-level contracts: provider transport
    // options (P-11/P-12/P-15), preset role/fallback maps (M-05/M-06),
    // MCP auth/bounds (T-06/T-10), durable review comments (V-02),
    // idempotent change acceptance (V-03) and per-session settings (K-02,
    // B-09). Every new column is nullable/defaulted so a pre-v3 database
    // upgrades in place.
    "ALTER TABLE provider_connection ADD COLUMN connect_timeout_ms INTEGER;
     ALTER TABLE provider_connection ADD COLUMN first_byte_timeout_ms INTEGER;
     ALTER TABLE provider_connection ADD COLUMN total_timeout_ms INTEGER;
     ALTER TABLE provider_connection ADD COLUMN keep_alive TEXT;
     ALTER TABLE provider_connection ADD COLUMN proxy_url TEXT;
     ALTER TABLE provider_connection ADD COLUMN tls_mode TEXT NOT NULL DEFAULT 'system';
     ALTER TABLE provider_connection ADD COLUMN ca_cert_ref TEXT;
     ALTER TABLE preset_record ADD COLUMN role_models_json TEXT NOT NULL DEFAULT '{}';
     ALTER TABLE preset_record ADD COLUMN fallback_chain_json TEXT NOT NULL DEFAULT '[]';
     ALTER TABLE mcp_server ADD COLUMN transport TEXT NOT NULL DEFAULT 'stdio';
     ALTER TABLE mcp_server ADD COLUMN endpoint TEXT;
     ALTER TABLE mcp_server ADD COLUMN auth_state TEXT NOT NULL DEFAULT 'signed_out';
     ALTER TABLE mcp_server ADD COLUMN auth_account TEXT;
     ALTER TABLE mcp_server ADD COLUMN auth_scopes_json TEXT NOT NULL DEFAULT '[]';
     ALTER TABLE mcp_server ADD COLUMN auth_token_ref TEXT;
     ALTER TABLE mcp_server ADD COLUMN timeout_ms INTEGER;
     ALTER TABLE mcp_server ADD COLUMN output_cap_bytes INTEGER;
     ALTER TABLE mcp_server ADD COLUMN stderr_cap_bytes INTEGER;
     CREATE TABLE IF NOT EXISTS session_setting (
        session_id TEXT NOT NULL,
        key TEXT NOT NULL,
        value_json TEXT NOT NULL,
        updated_ms INTEGER NOT NULL,
        PRIMARY KEY (session_id, key)
     );
     CREATE TABLE IF NOT EXISTS review_comment (
        id TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        task_id TEXT,
        change_id TEXT,
        path TEXT NOT NULL,
        line_start INTEGER NOT NULL,
        line_end INTEGER NOT NULL,
        side TEXT NOT NULL,
        body TEXT NOT NULL,
        author TEXT NOT NULL,
        content_hash TEXT,
        resolved INTEGER NOT NULL DEFAULT 0,
        revision INTEGER NOT NULL,
        created_ms INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_review_comment_session_path
        ON review_comment(session_id, path, line_start, id);
     CREATE INDEX IF NOT EXISTS idx_review_comment_session_task
        ON review_comment(session_id, task_id, id);
     CREATE TABLE IF NOT EXISTS change_accept_receipt (
        session_id TEXT NOT NULL,
        change_set_id TEXT NOT NULL,
        request_digest TEXT NOT NULL,
        receipt_json TEXT NOT NULL,
        created_ms INTEGER NOT NULL,
        PRIMARY KEY (session_id, change_set_id)
     );",
];

/// A foundation database handle.
#[derive(Clone)]
pub struct FoundationDb {
    path: PathBuf,
    writer: Arc<Mutex<Connection>>,
}

impl FoundationDb {
    /// Open (or create) the foundation database at `path`, applying every
    /// migration. A corrupt/too-new schema is a typed refusal.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, FoundationError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                FoundationError::Store(format!(
                    "create foundation data dir {}: {e}",
                    parent.display()
                ))
            })?;
        }
        let mut conn = Connection::open(&path)
            .map_err(|e| FoundationError::Store(format!("open {}: {e}", path.display())))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| FoundationError::Store(format!("busy_timeout: {e}")))?;
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");
        let _ = conn.pragma_update(None, "foreign_keys", "ON");
        apply_migrations(&mut conn)?;
        Ok(Self {
            path,
            writer: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Open an independent reader connection.
    pub(crate) fn reader(&self) -> Result<Connection, FoundationError> {
        let conn = Connection::open(&self.path).map_err(|e| {
            FoundationError::Store(format!("open reader {}: {e}", self.path.display()))
        })?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| FoundationError::Store(format!("busy_timeout: {e}")))?;
        Ok(conn)
    }

    /// Run one writer closure under the single write lock.
    pub(crate) fn write<T>(
        &self,
        what: &str,
        f: impl FnOnce(&mut Connection) -> Result<T, FoundationError>,
    ) -> Result<T, FoundationError> {
        let mut guard = self
            .writer
            .lock()
            .map_err(|_| FoundationError::Store(format!("{what}: writer lock poisoned")))?;
        f(&mut guard)
    }

    /// Run one read closure on a dedicated reader connection.
    pub(crate) fn read<T>(
        &self,
        what: &str,
        f: impl FnOnce(&Connection) -> Result<T, FoundationError>,
    ) -> Result<T, FoundationError> {
        let conn = self.reader()?;
        f(&conn).map_err(|e| match e {
            FoundationError::Store(message) => FoundationError::Store(format!("{what}: {message}")),
            other => other,
        })
    }
}

fn apply_migrations(conn: &mut Connection) -> Result<(), FoundationError> {
    let current: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|e| FoundationError::Store(format!("read user_version: {e}")))?;
    if current > SCHEMA_VERSION {
        return Err(FoundationError::Store(format!(
            "foundation schema version {current} is newer than this daemon supports ({SCHEMA_VERSION})"
        )));
    }
    for (index, migration) in MIGRATIONS.iter().enumerate() {
        let target = index as i64 + 1;
        if current >= target {
            continue;
        }
        let tx = conn
            .transaction()
            .map_err(|e| FoundationError::Store(format!("begin migration v{target}: {e}")))?;
        tx.execute_batch(migration)
            .map_err(|e| FoundationError::Store(format!("migration v{target}: {e}")))?;
        tx.pragma_update(None, "user_version", target)
            .map_err(|e| FoundationError::Store(format!("set user_version v{target}: {e}")))?;
        tx.commit()
            .map_err(|e| FoundationError::Store(format!("commit migration v{target}: {e}")))?;
    }
    Ok(())
}

/// Milliseconds since the Unix epoch, saturating at `i64::MAX`.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_idempotent_and_newer_schema_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foundation.db");
        let db = FoundationDb::open(&path).unwrap();
        drop(db);
        let again = FoundationDb::open(&path).unwrap();
        drop(again);
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        drop(conn);
        assert!(FoundationDb::open(&path).is_err());
    }
}
