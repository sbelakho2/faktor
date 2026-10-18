//! Browser adapter lifecycle and observations (Rev8 S20, X-06..X-09).
//!
//! The daemon owns the durable identity/scope/lifecycle records and the
//! bounded observation/artifact store. It does NOT pretend to be a browser
//! driver: only an adapter registered by the host (with an explicit
//! `advertised` flag) may open a session, and only adapter events advance a
//! session to `connected`. With no adapter, connect is a typed
//! `browser_adapter_unavailable` refusal, never a mocked preview.
//!
//! Every capture is bounded and content-addressed; screenshots enter model
//! context only when the caller explicitly sets `inModelContext`.

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::db::now_ms;
use crate::{hash_hex, page_limit_i64, FoundationDb, FoundationError};

pub const MAX_ADAPTERS: usize = 16;
pub const MAX_BROWSER_SESSIONS: usize = 64;
pub const MAX_OBSERVATIONS_PER_SESSION: usize = 2_000;
pub const MAX_CAPTURE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_OBSERVATION_BYTES: usize = 64 * 1024;
pub const MAX_URL_BYTES: usize = 4_096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserMode {
    Local,
    Remote,
}

impl BrowserMode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "local" => Some(Self::Local),
            "remote" => Some(Self::Remote),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserSessionState {
    Requested,
    Connected,
    Closed,
    Failed,
}

impl BrowserSessionState {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "requested" => Some(Self::Requested),
            "connected" => Some(Self::Connected),
            "closed" => Some(Self::Closed),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Connected => "connected",
            Self::Closed => "closed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserAdapterDraft {
    pub id: String,
    pub mode: BrowserMode,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub account_scope: Option<String>,
    pub advertised: bool,
    #[serde(default = "default_network_scope")]
    pub network_scope: String,
}

fn default_network_scope() -> String {
    "egress_policy".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserAdapter {
    pub id: String,
    pub mode: BrowserMode,
    pub endpoint: Option<String>,
    pub account_scope: Option<String>,
    pub advertised: bool,
    pub network_scope: String,
    pub updated_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserSession {
    pub id: String,
    pub adapter_id: String,
    pub session_id: u64,
    pub url: String,
    pub host: String,
    pub state: BrowserSessionState,
    pub created_ms: i64,
    pub closed_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserObservation {
    pub id: i64,
    pub browser_session_id: String,
    pub kind: String,
    pub severity: Option<String>,
    pub payload: serde_json::Value,
    pub captured_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserArtifact {
    pub id: String,
    pub browser_session_id: String,
    pub mime: String,
    pub bytes: usize,
    pub checksum: String,
    pub in_model_context: bool,
    pub captured_ms: i64,
}

fn validate_adapter(draft: &BrowserAdapterDraft) -> Result<(), FoundationError> {
    if draft.id.is_empty()
        || draft.id.len() > 128
        || !draft
            .id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    {
        return Err(FoundationError::Malformed(
            "adapter id must be 1..=128 bytes of ASCII [A-Za-z0-9._-]".into(),
        ));
    }
    if let Some(endpoint) = &draft.endpoint {
        url::Url::parse(endpoint).map_err(|e| {
            FoundationError::Malformed(format!("adapter endpoint is not a URL: {e}"))
        })?;
    }
    if let Some(account) = &draft.account_scope {
        if account.len() > 256 || account.contains('\u{0}') {
            return Err(FoundationError::Malformed(
                "account scope is too long".into(),
            ));
        }
    }
    if draft.network_scope != "egress_policy" && draft.network_scope != "loopback_only" {
        return Err(FoundationError::Malformed(
            "networkScope must be egress_policy or loopback_only".into(),
        ));
    }
    Ok(())
}

fn validate_url(raw: &str) -> Result<(String, String), FoundationError> {
    if raw.is_empty() || raw.len() > MAX_URL_BYTES {
        return Err(FoundationError::Malformed(format!(
            "url must be 1..={MAX_URL_BYTES} bytes"
        )));
    }
    let parsed = url::Url::parse(raw)
        .map_err(|e| FoundationError::Malformed(format!("url does not parse: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(FoundationError::Malformed(
            "only http(s) URLs can be requested; file/data/javascript are refused".into(),
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(FoundationError::Malformed(
            "userinfo in a preview URL is refused".into(),
        ));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| FoundationError::Malformed("url has no host".into()))?
        .to_string();
    Ok((raw.to_string(), host))
}

pub struct BrowserService {
    db: FoundationDb,
}

impl BrowserService {
    pub fn new(db: FoundationDb) -> Self {
        Self { db }
    }

    /// The host wiring registers adapters. There is deliberately no HTTP
    /// route that turns `advertised` on: a repository cannot advertise a
    /// browser the daemon did not build.
    pub fn upsert_adapter(
        &self,
        draft: &BrowserAdapterDraft,
    ) -> Result<BrowserAdapter, FoundationError> {
        validate_adapter(draft)?;
        let now = now_ms();
        self.db.write("browser adapter", |conn| {
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM browser_adapter", [], |r| r.get(0))
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let exists: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM browser_adapter WHERE id = ?1",
                    params![draft.id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if exists.is_none() && count as usize >= MAX_ADAPTERS {
                return Err(FoundationError::Oversized(format!(
                    "at most {MAX_ADAPTERS} browser adapters"
                )));
            }
            conn.execute(
                "INSERT INTO browser_adapter(id, mode, endpoint, account_scope, advertised, network_scope, updated_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id) DO UPDATE SET
                   mode = excluded.mode, endpoint = excluded.endpoint,
                   account_scope = excluded.account_scope,
                   advertised = excluded.advertised,
                   network_scope = excluded.network_scope,
                   updated_ms = excluded.updated_ms",
                params![
                    draft.id,
                    draft.mode.as_str(),
                    draft.endpoint,
                    draft.account_scope,
                    if draft.advertised { 1 } else { 0 },
                    draft.network_scope,
                    now
                ],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            read_adapter(conn, &draft.id)
        })
    }

    pub fn adapters(&self) -> Result<Vec<BrowserAdapter>, FoundationError> {
        self.db.read("browser adapters", |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, mode, endpoint, account_scope, advertised, network_scope, updated_ms
                     FROM browser_adapter ORDER BY id ASC LIMIT ?1",
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let rows = stmt
                .query_map(params![MAX_ADAPTERS as i64], map_adapter)
                .map_err(|e| FoundationError::Store(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(rows)
        })
    }

    /// Request one browser session. Without an ADVERTISED adapter this is a
    /// typed refusal: no preview is mocked.
    pub fn request_session(
        &self,
        adapter_id: &str,
        session_id: u64,
        url: &str,
    ) -> Result<BrowserSession, FoundationError> {
        let (url, host) = validate_url(url)?;
        let adapter = self
            .db
            .read("browser adapter get", |conn| {
                conn.query_row(
                    "SELECT id, mode, endpoint, account_scope, advertised, network_scope, updated_ms
                     FROM browser_adapter WHERE id = ?1",
                    params![adapter_id],
                    map_adapter,
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))
            })?
            .ok_or_else(|| FoundationError::NotFound(format!("browser adapter {adapter_id}")))?;
        if !adapter.advertised {
            return Err(FoundationError::Conflict(format!(
                "browser adapter {adapter_id} is not advertised by this daemon; browser controls stay unavailable"
            )));
        }
        let now = now_ms();
        let id = format!(
            "browser-session-{}",
            &hash_hex(format!("{adapter_id}:{session_id}:{url}:{now}").as_bytes())[..24]
        );
        self.db.write("browser session", |conn| {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM browser_session WHERE session_id = ?1 AND state IN ('requested','connected')",
                    params![session_id as i64],
                    |r| r.get(0),
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if count as usize >= MAX_BROWSER_SESSIONS {
                return Err(FoundationError::Oversized(format!(
                    "at most {MAX_BROWSER_SESSIONS} live browser sessions per session"
                )));
            }
            conn.execute(
                "INSERT INTO browser_session(id, adapter_id, session_id, url, host, state, created_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'requested', ?6)",
                params![id, adapter_id, session_id as i64, url, host, now],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            read_session(conn, &id)
        })
    }

    /// Adapter-owned transition. Not reachable over HTTP: only the wired
    /// adapter may report that a browser actually connected/failed.
    pub fn record_state(
        &self,
        id: &str,
        state: BrowserSessionState,
    ) -> Result<BrowserSession, FoundationError> {
        if !matches!(
            state,
            BrowserSessionState::Connected
                | BrowserSessionState::Failed
                | BrowserSessionState::Closed
        ) {
            return Err(FoundationError::Malformed(
                "recordState accepts connected, failed or closed".into(),
            ));
        }
        self.db.write("browser state", |conn| {
            let closed = if state == BrowserSessionState::Closed {
                Some(now_ms())
            } else {
                None
            };
            let changed = conn
                .execute(
                    "UPDATE browser_session SET state = ?2, closed_ms = COALESCE(?3, closed_ms)
                     WHERE id = ?1",
                    params![id, state.as_str(), closed],
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if changed == 0 {
                return Err(FoundationError::NotFound(format!("browser session {id}")));
            }
            read_session(conn, id)
        })
    }

    pub fn sessions(&self, session_id: u64) -> Result<Vec<BrowserSession>, FoundationError> {
        self.db.read("browser sessions", |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, adapter_id, session_id, url, host, state, created_ms, closed_ms
                     FROM browser_session WHERE session_id = ?1 ORDER BY created_ms DESC LIMIT ?2",
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let rows = stmt
                .query_map(
                    params![session_id as i64, MAX_BROWSER_SESSIONS as i64],
                    map_session,
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(rows)
        })
    }

    pub fn append_observation(
        &self,
        browser_session_id: &str,
        kind: &str,
        severity: Option<&str>,
        payload: &serde_json::Value,
    ) -> Result<BrowserObservation, FoundationError> {
        if kind.is_empty() || kind.len() > 64 || kind.contains('\u{0}') {
            return Err(FoundationError::Malformed(
                "invalid observation kind".into(),
            ));
        }
        let payload_json = serde_json::to_string(payload)
            .map_err(|e| FoundationError::Store(format!("encode observation: {e}")))?;
        if payload_json.len() > MAX_OBSERVATION_BYTES {
            return Err(FoundationError::Oversized(format!(
                "an observation is bounded to {MAX_OBSERVATION_BYTES} bytes"
            )));
        }
        let now = now_ms();
        self.db.write("browser observation", |conn| {
            let session: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM browser_session WHERE id = ?1",
                    params![browser_session_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if session.is_none() {
                return Err(FoundationError::NotFound(format!(
                    "browser session {browser_session_id}"
                )));
            }
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM browser_observation WHERE browser_session_id = ?1",
                    params![browser_session_id],
                    |r| r.get(0),
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if count as usize >= MAX_OBSERVATIONS_PER_SESSION {
                return Err(FoundationError::Oversized(format!(
                    "at most {MAX_OBSERVATIONS_PER_SESSION} observations per browser session"
                )));
            }
            conn.execute(
                "INSERT INTO browser_observation(browser_session_id, kind, severity, payload_json, captured_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![browser_session_id, kind, severity, payload_json, now],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            let id = conn.last_insert_rowid();
            read_observation(conn, id)
        })
    }

    pub fn observations(
        &self,
        browser_session_id: &str,
        severity: Option<&str>,
        after: Option<i64>,
        limit: usize,
    ) -> Result<Vec<BrowserObservation>, FoundationError> {
        let limit = page_limit_i64(limit.clamp(1, 500), "browser observations")?;
        self.db.read("browser observations", |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, browser_session_id, kind, severity, payload_json, captured_ms
                     FROM browser_observation
                     WHERE browser_session_id = ?1 AND id > ?2
                       AND (?3 IS NULL OR severity = ?3)
                     ORDER BY id ASC LIMIT ?4",
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let rows = stmt
                .query_map(
                    params![browser_session_id, after.unwrap_or(0), severity, limit],
                    map_observation,
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(rows)
        })
    }

    pub fn store_screenshot(
        &self,
        browser_session_id: &str,
        mime: &str,
        bytes: &[u8],
        in_model_context: bool,
    ) -> Result<BrowserArtifact, FoundationError> {
        if mime != "image/png" && mime != "image/jpeg" && mime != "image/webp" {
            return Err(FoundationError::Malformed(
                "screenshot mime must be image/png, image/jpeg or image/webp".into(),
            ));
        }
        if bytes.is_empty() || bytes.len() > MAX_CAPTURE_BYTES {
            return Err(FoundationError::Oversized(format!(
                "a screenshot must be 1..={MAX_CAPTURE_BYTES} bytes"
            )));
        }
        let checksum = hash_hex(bytes);
        let id = format!("artifact-{}", &checksum[..24]);
        let now = now_ms();
        self.db.write("browser artifact", |conn| {
            let session: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM browser_session WHERE id = ?1",
                    params![browser_session_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if session.is_none() {
                return Err(FoundationError::NotFound(format!(
                    "browser session {browser_session_id}"
                )));
            }
            let existing_checksum: Option<String> = conn
                .query_row(
                    "SELECT checksum FROM browser_artifact WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if let Some(existing) = existing_checksum {
                if existing == checksum {
                    return read_artifact(conn, &id);
                }
                return Err(FoundationError::Conflict(
                    "artifact id collided with different bytes".into(),
                ));
            }
            conn.execute(
                "INSERT INTO browser_artifact(id, browser_session_id, mime, bytes, checksum, in_model_context, data, captured_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    id,
                    browser_session_id,
                    mime,
                    bytes.len() as i64,
                    checksum,
                    if in_model_context { 1 } else { 0 },
                    bytes,
                    now
                ],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            read_artifact(conn, &id)
        })
    }

    pub fn artifact_bytes(
        &self,
        id: &str,
    ) -> Result<Option<(BrowserArtifact, Vec<u8>)>, FoundationError> {
        self.db.read("browser artifact bytes", |conn| {
            conn.query_row(
                "SELECT id, browser_session_id, mime, bytes, checksum, in_model_context, data, captured_ms
                 FROM browser_artifact WHERE id = ?1",
                params![id],
                |row| {
                    let artifact = map_artifact_meta(row, 0, 1, 2, 3, 4, 5, 7)?;
                    let data: Vec<u8> = row.get(6)?;
                    Ok((artifact, data))
                },
            )
            .optional()
            .map_err(|e| FoundationError::Store(e.to_string()))
        })
    }
}

fn read_adapter(conn: &rusqlite::Connection, id: &str) -> Result<BrowserAdapter, FoundationError> {
    conn.query_row(
        "SELECT id, mode, endpoint, account_scope, advertised, network_scope, updated_ms
         FROM browser_adapter WHERE id = ?1",
        params![id],
        map_adapter,
    )
    .map_err(|e| FoundationError::Store(e.to_string()))
}

fn map_adapter(row: &rusqlite::Row<'_>) -> rusqlite::Result<BrowserAdapter> {
    let mode: String = row.get(1)?;
    let mode = BrowserMode::parse(&mode).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unknown browser mode",
            )),
        )
    })?;
    Ok(BrowserAdapter {
        id: row.get(0)?,
        mode,
        endpoint: row.get(2)?,
        account_scope: row.get(3)?,
        advertised: row.get::<_, i64>(4)? != 0,
        network_scope: row.get(5)?,
        updated_ms: row.get(6)?,
    })
}

fn read_session(conn: &rusqlite::Connection, id: &str) -> Result<BrowserSession, FoundationError> {
    conn.query_row(
        "SELECT id, adapter_id, session_id, url, host, state, created_ms, closed_ms
         FROM browser_session WHERE id = ?1",
        params![id],
        map_session,
    )
    .map_err(|e| FoundationError::Store(e.to_string()))
}

fn map_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<BrowserSession> {
    let state: String = row.get(5)?;
    let state = BrowserSessionState::parse(&state).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            5,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unknown browser session state",
            )),
        )
    })?;
    Ok(BrowserSession {
        id: row.get(0)?,
        adapter_id: row.get(1)?,
        session_id: u64::try_from(row.get::<_, i64>(2)?).unwrap_or(0),
        url: row.get(3)?,
        host: row.get(4)?,
        state,
        created_ms: row.get(6)?,
        closed_ms: row.get(7)?,
    })
}

fn read_observation(
    conn: &rusqlite::Connection,
    id: i64,
) -> Result<BrowserObservation, FoundationError> {
    conn.query_row(
        "SELECT id, browser_session_id, kind, severity, payload_json, captured_ms
         FROM browser_observation WHERE id = ?1",
        params![id],
        map_observation,
    )
    .map_err(|e| FoundationError::Store(e.to_string()))
}

fn map_observation(row: &rusqlite::Row<'_>) -> rusqlite::Result<BrowserObservation> {
    let raw: String = row.get(4)?;
    let payload: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        )
    })?;
    Ok(BrowserObservation {
        id: row.get(0)?,
        browser_session_id: row.get(1)?,
        kind: row.get(2)?,
        severity: row.get(3)?,
        payload,
        captured_ms: row.get(5)?,
    })
}

fn read_artifact(
    conn: &rusqlite::Connection,
    id: &str,
) -> Result<BrowserArtifact, FoundationError> {
    conn.query_row(
        "SELECT id, browser_session_id, mime, bytes, checksum, in_model_context, data, captured_ms
         FROM browser_artifact WHERE id = ?1",
        params![id],
        |row| map_artifact_meta(row, 0, 1, 2, 3, 4, 5, 7),
    )
    .map_err(|e| FoundationError::Store(e.to_string()))
}

#[allow(clippy::too_many_arguments)]
fn map_artifact_meta(
    row: &rusqlite::Row<'_>,
    id: usize,
    session: usize,
    mime: usize,
    bytes: usize,
    checksum: usize,
    in_model: usize,
    captured: usize,
) -> rusqlite::Result<BrowserArtifact> {
    Ok(BrowserArtifact {
        id: row.get(id)?,
        browser_session_id: row.get(session)?,
        mime: row.get(mime)?,
        bytes: usize::try_from(row.get::<_, i64>(bytes)?).unwrap_or(0),
        checksum: row.get(checksum)?,
        in_model_context: row.get::<_, i64>(in_model)? != 0,
        captured_ms: row.get(captured)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> (tempfile::TempDir, BrowserService) {
        let dir = tempfile::tempdir().unwrap();
        let db = FoundationDb::open(dir.path().join("foundation.db")).unwrap();
        (dir, BrowserService::new(db))
    }

    fn adapter(advertised: bool) -> BrowserAdapterDraft {
        BrowserAdapterDraft {
            id: "chromium".into(),
            mode: BrowserMode::Local,
            endpoint: Some("http://127.0.0.1:9222".into()),
            account_scope: Some("daemon".into()),
            advertised,
            network_scope: "loopback_only".into(),
        }
    }

    #[test]
    fn unadvertised_adapters_refuse_and_connected_is_never_inferred() {
        let (_dir, svc) = service();
        assert!(matches!(
            svc.request_session("chromium", 1, "https://example.com"),
            Err(FoundationError::NotFound(_))
        ));
        svc.upsert_adapter(&adapter(false)).unwrap();
        assert!(matches!(
            svc.request_session("chromium", 1, "https://example.com"),
            Err(FoundationError::Conflict(_))
        ));
        assert!(svc.sessions(1).unwrap().is_empty());
        svc.upsert_adapter(&adapter(true)).unwrap();
        let session = svc
            .request_session("chromium", 1, "https://example.com/app")
            .unwrap();
        assert_eq!(session.state, BrowserSessionState::Requested);
        assert_eq!(session.host, "example.com");
        // Only the adapter may advance the state.
        let connected = svc
            .record_state(&session.id, BrowserSessionState::Connected)
            .unwrap();
        assert_eq!(connected.state, BrowserSessionState::Connected);
    }

    #[test]
    fn hostile_urls_observations_and_captures_are_bounded() {
        let (_dir, svc) = service();
        svc.upsert_adapter(&adapter(true)).unwrap();
        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://user:pass@example.com/",
            "data:text/html,x",
        ] {
            assert!(
                svc.request_session("chromium", 1, url).is_err(),
                "{url} must be refused"
            );
        }
        let session = svc
            .request_session("chromium", 1, "https://example.com")
            .unwrap();
        let obs = svc
            .append_observation(
                &session.id,
                "console",
                Some("error"),
                &serde_json::json!({"message": "boom"}),
            )
            .unwrap();
        assert_eq!(obs.kind, "console");
        let errors = svc
            .observations(&session.id, Some("error"), None, 10)
            .unwrap();
        assert_eq!(errors.len(), 1);
        assert!(svc
            .observations(&session.id, Some("info"), None, 10)
            .unwrap()
            .is_empty());
        let big = serde_json::json!({"x": "a".repeat(MAX_OBSERVATION_BYTES + 1)});
        assert!(svc
            .append_observation(&session.id, "network", None, &big)
            .is_err());
        let png = b"\x89PNG\r\n\x1a\n";
        let artifact = svc
            .store_screenshot(&session.id, "image/png", png, true)
            .unwrap();
        let (meta, bytes) = svc.artifact_bytes(&artifact.id).unwrap().unwrap();
        assert_eq!(bytes, png);
        assert!(meta.in_model_context);
        assert_eq!(meta.checksum, hash_hex(png));
        assert!(svc
            .store_screenshot(
                &session.id,
                "image/png",
                &vec![0u8; MAX_CAPTURE_BYTES + 1],
                false
            )
            .is_err());
        assert!(svc
            .store_screenshot(&session.id, "text/html", b"x", false)
            .is_err());
    }
}
