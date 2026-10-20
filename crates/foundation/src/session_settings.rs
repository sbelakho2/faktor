//! Per-session selectable settings (Rev8 K-02/B-09) and idempotent change
//! acceptance receipts (V-03). Settings are validated at this boundary;
//! consumers read the exact persisted value (never a guess).

use rusqlite::{params, OptionalExtension};
use serde_json::Value;

use crate::db::now_ms;
use crate::{FoundationDb, FoundationError};

pub const COMPACTION_MODEL_KEY: &str = "compactionModel";
pub const MAX_TURNS_KEY: &str = "maxTurns";
pub const MAX_CONSECUTIVE_FAILURES_KEY: &str = "maxConsecutiveFailures";
/// Per-session context-window override in tokens (F046). It can only LOWER
/// the effective budget below the model/runtime limit, never raise it.
pub const CONTEXT_WINDOW_KEY: &str = "contextWindow";
pub const MAX_SESSION_SETTINGS: usize = 32;
/// Bounds of the per-session context-window override (tokens).
pub const MIN_CONTEXT_WINDOW_TOKENS: u64 = 256;
pub const MAX_CONTEXT_WINDOW_TOKENS: u64 = 10_000_000;
pub const MAX_SETTING_BYTES: usize = 1_024;

pub struct SessionSettingsService {
    db: FoundationDb,
}

impl SessionSettingsService {
    pub fn new(db: FoundationDb) -> Self {
        Self { db }
    }

    pub fn get(&self, session_id: u64, key: &str) -> Result<Option<Value>, FoundationError> {
        self.db.read("session setting get", |conn| {
            let raw: Option<String> = conn
                .query_row(
                    "SELECT value_json FROM session_setting WHERE session_id = ?1 AND key = ?2",
                    params![session_id.to_string(), key],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            match raw {
                Some(raw) => serde_json::from_str(&raw)
                    .map(Some)
                    .map_err(|e| FoundationError::Corrupt(format!("session setting {key}: {e}"))),
                None => Ok(None),
            }
        })
    }

    pub fn set(&self, session_id: u64, key: &str, value: &Value) -> Result<Value, FoundationError> {
        validate_setting(key, value)?;
        let encoded = serde_json::to_string(value)
            .map_err(|e| FoundationError::Store(format!("encode session setting: {e}")))?;
        if encoded.len() > MAX_SETTING_BYTES {
            return Err(FoundationError::Oversized(format!(
                "session setting must fit {MAX_SETTING_BYTES} bytes"
            )));
        }
        self.db.write("session setting set", |conn| {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM session_setting WHERE session_id = ?1",
                    params![session_id.to_string()],
                    |r| r.get(0),
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let exists: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM session_setting WHERE session_id = ?1 AND key = ?2",
                    params![session_id.to_string(), key],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if exists.is_none() && count as usize >= MAX_SESSION_SETTINGS {
                return Err(FoundationError::Oversized(format!(
                    "at most {MAX_SESSION_SETTINGS} session settings"
                )));
            }
            conn.execute(
                "INSERT INTO session_setting(session_id, key, value_json, updated_ms)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(session_id, key) DO UPDATE SET
                   value_json = excluded.value_json, updated_ms = excluded.updated_ms",
                params![session_id.to_string(), key, encoded, now_ms()],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(value.clone())
        })
    }

    pub fn clear(&self, session_id: u64, key: &str) -> Result<bool, FoundationError> {
        self.db.write("session setting clear", |conn| {
            let changed = conn
                .execute(
                    "DELETE FROM session_setting WHERE session_id = ?1 AND key = ?2",
                    params![session_id.to_string(), key],
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(changed > 0)
        })
    }

    /// The idempotent change-acceptance receipt of one `(session,
    /// changeSetId)`. A persisted receipt is returned byte-for-byte to every
    /// replay; a different request digest under the same identity is a typed
    /// conflict at the caller's boundary.
    pub fn change_receipt(
        &self,
        session_id: u64,
        change_set_id: &str,
    ) -> Result<Option<(String, Value)>, FoundationError> {
        self.db.read("change receipt get", |conn| {
            let row: Option<(String, String)> = conn
                .query_row(
                    "SELECT request_digest, receipt_json FROM change_accept_receipt
                     WHERE session_id = ?1 AND change_set_id = ?2",
                    params![session_id.to_string(), change_set_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            match row {
                Some((digest, raw)) => serde_json::from_str(&raw)
                    .map(|value| Some((digest, value)))
                    .map_err(|e| FoundationError::Corrupt(format!("change receipt: {e}"))),
                None => Ok(None),
            }
        })
    }

    pub fn put_change_receipt(
        &self,
        session_id: u64,
        change_set_id: &str,
        request_digest: &str,
        receipt: &Value,
    ) -> Result<(), FoundationError> {
        if change_set_id.is_empty() || change_set_id.len() > 256 {
            return Err(FoundationError::Malformed(
                "changeSetId must be 1..=256 bytes".into(),
            ));
        }
        if request_digest.is_empty() || request_digest.len() > 128 {
            return Err(FoundationError::Malformed(
                "requestDigest must be 1..=128 bytes".into(),
            ));
        }
        let raw = serde_json::to_string(receipt)
            .map_err(|e| FoundationError::Store(format!("encode change receipt: {e}")))?;
        self.db.write("change receipt put", |conn| {
            conn.execute(
                "INSERT INTO change_accept_receipt(
                   session_id, change_set_id, request_digest, receipt_json, created_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(session_id, change_set_id) DO NOTHING",
                params![
                    session_id.to_string(),
                    change_set_id,
                    request_digest,
                    raw,
                    now_ms()
                ],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(())
        })
    }
}

fn validate_setting(key: &str, value: &Value) -> Result<(), FoundationError> {
    match key {
        COMPACTION_MODEL_KEY => {
            let model = value.as_str().ok_or_else(|| {
                FoundationError::Malformed("compactionModel must be a string".into())
            })?;
            if model.is_empty() || model.len() > 256 || model.chars().any(char::is_control) {
                return Err(FoundationError::Malformed(
                    "compactionModel must be 1..=256 non-control bytes".into(),
                ));
            }
        }
        MAX_TURNS_KEY => {
            let turns = value
                .as_u64()
                .ok_or_else(|| FoundationError::Malformed("maxTurns must be an integer".into()))?;
            if !(1..=1_000_000).contains(&turns) {
                return Err(FoundationError::Malformed(
                    "maxTurns must be 1..=1000000".into(),
                ));
            }
        }
        MAX_CONSECUTIVE_FAILURES_KEY => {
            let failures = value.as_u64().ok_or_else(|| {
                FoundationError::Malformed("maxConsecutiveFailures must be an integer".into())
            })?;
            if !(2..=1_000).contains(&failures) {
                return Err(FoundationError::Malformed(
                    "maxConsecutiveFailures must be 2..=1000".into(),
                ));
            }
        }
        CONTEXT_WINDOW_KEY => {
            let tokens = value.as_u64().ok_or_else(|| {
                FoundationError::Malformed("contextWindow must be an integer".into())
            })?;
            if !(MIN_CONTEXT_WINDOW_TOKENS..=MAX_CONTEXT_WINDOW_TOKENS).contains(&tokens) {
                return Err(FoundationError::Malformed(format!(
                    "contextWindow must be {MIN_CONTEXT_WINDOW_TOKENS}..={MAX_CONTEXT_WINDOW_TOKENS} tokens"
                )));
            }
        }
        _ => {
            return Err(FoundationError::Malformed(format!(
                "unknown session setting {key:?}"
            )))
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn change_receipt_is_idempotent_durable_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foundation.db");
        let receipt = json!({"accepted": ["a.txt"], "digest": "abc"});
        {
            let service = SessionSettingsService::new(FoundationDb::open(&path).unwrap());
            service
                .put_change_receipt(7, "change-set-1", "digest-1", &receipt)
                .unwrap();
            assert_eq!(
                service.change_receipt(7, "change-set-1").unwrap(),
                Some(("digest-1".to_string(), receipt.clone()))
            );
            // The SQL insert is ON CONFLICT DO NOTHING: a replay keeps the
            // first durable receipt; the caller compares request_digest and
            // returns a typed conflict instead of a second acceptance.
            service
                .put_change_receipt(7, "change-set-1", "digest-2", &json!({"accepted": []}))
                .unwrap();
            assert_eq!(
                service.change_receipt(7, "change-set-1").unwrap(),
                Some(("digest-1".to_string(), receipt.clone()))
            );
        }
        // Reopen: the receipt is durable, not process memory.
        let reopened = SessionSettingsService::new(FoundationDb::open(&path).unwrap());
        assert_eq!(
            reopened.change_receipt(7, "change-set-1").unwrap(),
            Some(("digest-1".to_string(), receipt))
        );
        assert!(reopened
            .change_receipt(8, "change-set-1")
            .unwrap()
            .is_none());

        for (change_set, digest) in [
            ("", "d"),
            ("x".repeat(257).as_str(), "d"),
            ("c", ""),
            ("c", "x".repeat(129).as_str()),
        ] {
            let error = reopened
                .put_change_receipt(7, change_set, digest, &json!({}))
                .unwrap_err();
            assert!(matches!(error, FoundationError::Malformed(_)), "{error:?}");
        }
    }

    #[test]
    fn session_settings_validate_and_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foundation.db");
        {
            let service = SessionSettingsService::new(FoundationDb::open(&path).unwrap());
            service.set(3, MAX_TURNS_KEY, &json!(12)).unwrap();
            service
                .set(3, COMPACTION_MODEL_KEY, &json!("fake/m"))
                .unwrap();
            assert!(service.set(3, MAX_TURNS_KEY, &json!(0)).is_err());
            assert!(service
                .set(
                    3,
                    COMPACTION_MODEL_KEY,
                    &json!("x".repeat(MAX_SETTING_BYTES + 1))
                )
                .is_err());
        }
        let reopened = SessionSettingsService::new(FoundationDb::open(&path).unwrap());
        assert_eq!(reopened.get(3, MAX_TURNS_KEY).unwrap(), Some(json!(12)));
        assert_eq!(
            reopened.get(3, COMPACTION_MODEL_KEY).unwrap(),
            Some(json!("fake/m"))
        );
        assert!(reopened.clear(3, MAX_TURNS_KEY).unwrap());
        assert!(!reopened.clear(3, MAX_TURNS_KEY).unwrap());
    }
}
