//! Durable ordinary-prompt admission (idempotency finding 1, backend half):
//! ONE durable row per client submission key of the plain prompt path,
//! claimed BEFORE the `PromptReceived` journal append and any queue/message
//! mutation and completed with the byte-exact serialized prompt receipt once
//! the prompt was accepted. A repeated key returns the stored receipt
//! without a second prompt; a different body digest under the same key is a
//! typed [`PromptAdmissionClaim::KeyReused`] conflict; a pending row refuses
//! a concurrent duplicate with [`PromptAdmissionClaim::InFlight`].
//!
//! This table is SEPARATE from `task_admission` (task-start semantics are
//! untouched): a prompt key never aliases a task start and vice versa.

use rusqlite::{params, OptionalExtension, TransactionBehavior};

use faktor_core::id::SessionId;

use super::{Store, StoreError, StoreResult};

/// Bound on one prompt submission key (the client's UUID-shaped id).
pub const MAX_PROMPT_ADMISSION_KEY_BYTES: usize = 64;

/// Bound on the canonical request digest of one prompt (bare 64-hex with
/// headroom for a labelled form).
pub const MAX_PROMPT_ADMISSION_DIGEST_BYTES: usize = 128;

/// Bound on the stored prompt receipt JSON. Receipts are small (op id, run
/// id, accepted/queued flags), but the column is JSON an operator may
/// extend; the cap is enforced on write AND on read, so a hostile injected
/// row can never balloon a reader.
pub const MAX_PROMPT_ADMISSION_RECEIPT_BYTES: usize = 16 * 1024;

/// The typed outcome of one prompt submission-keyed admission claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptAdmissionClaim {
    /// No row existed; one `pending` row was inserted for this caller.
    Fresh,
    /// The SAME session + key + request digest already completed: the exact
    /// stored receipt JSON (byte-for-byte what the first success stored).
    Complete(String),
    /// The same key is still pending: another admission is in flight and a
    /// concurrent duplicate must retry, never execute a second prompt.
    InFlight,
    /// The key belongs to a DIFFERENT session, or completed with a different
    /// request digest: a typed conflict that must never run.
    KeyReused { stored_digest: String },
}

impl PromptAdmissionClaim {
    /// `true` for [`Self::Fresh`].
    pub fn is_fresh(&self) -> bool {
        matches!(self, Self::Fresh)
    }

    /// The byte-exact stored receipt JSON of [`Self::Complete`].
    pub fn complete_receipt(&self) -> Option<&str> {
        match self {
            Self::Complete(receipt_json) => Some(receipt_json),
            _ => None,
        }
    }

    /// `true` for [`Self::InFlight`].
    pub fn is_in_flight(&self) -> bool {
        matches!(self, Self::InFlight)
    }

    /// The stored request digest of [`Self::KeyReused`].
    pub fn key_reused_digest(&self) -> Option<&str> {
        match self {
            Self::KeyReused { stored_digest } => Some(stored_digest),
            _ => None,
        }
    }
}

fn check_key(key: &str) -> StoreResult<()> {
    if key.is_empty() || key.len() > MAX_PROMPT_ADMISSION_KEY_BYTES {
        return Err(StoreError::Oversized(format!(
            "prompt admission key must be 1..={MAX_PROMPT_ADMISSION_KEY_BYTES} bytes"
        )));
    }
    Ok(())
}

fn check_digest(request_digest: &str) -> StoreResult<()> {
    if request_digest.is_empty() || request_digest.len() > MAX_PROMPT_ADMISSION_DIGEST_BYTES {
        return Err(StoreError::Oversized(format!(
            "prompt admission request digest must be 1..={MAX_PROMPT_ADMISSION_DIGEST_BYTES} bytes"
        )));
    }
    Ok(())
}

impl Store {
    /// Claim the durable admission of ONE submission-keyed prompt. ONE
    /// writer job in an IMMEDIATE transaction; every value is prepared on
    /// the caller's thread. Absent -> INSERT `pending` and [`Fresh`]; present
    /// `complete` with the equal digest -> [`Complete`] with the stored
    /// receipt; present `complete` with a different digest (or a foreign
    /// session) -> [`KeyReused`]; present `pending` -> [`InFlight`].
    pub fn prompt_admission_claim(
        &self,
        session_id: SessionId,
        key: &str,
        request_digest: &str,
        now: i64,
    ) -> StoreResult<PromptAdmissionClaim> {
        check_key(key)?;
        check_digest(request_digest)?;
        let key = key.to_owned();
        let request_digest = request_digest.to_owned();
        let session_raw = session_id.raw() as i64;
        let corrupt_state = "prompt_admission row carries an unknown state".to_owned();
        let corrupt_receipt = "prompt_admission complete row carries no receipt".to_owned();
        let oversized_receipt =
            format!("prompt_admission receipt exceeds {MAX_PROMPT_ADMISSION_RECEIPT_BYTES} bytes");
        self.writer.execute("prompt_admission_claim", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let row: Option<(i64, String, String, Option<i64>)> = tx
                .query_row(
                    "SELECT session_id, request_digest, state,
                            CASE WHEN receipt_json IS NULL THEN NULL
                                 ELSE length(CAST(receipt_json AS BLOB)) END
                     FROM prompt_admission WHERE key = ?1",
                    params![key.as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            let Some((stored_session, stored_digest, state, receipt_bytes)) = row else {
                tx.execute(
                    "INSERT INTO prompt_admission(
                        key, session_id, request_digest, state, receipt_json, created_ms)
                     VALUES (?1, ?2, ?3, 'pending', NULL, ?4)",
                    params![key.as_str(), session_raw, request_digest.as_str(), now],
                )?;
                tx.commit()?;
                return Ok(PromptAdmissionClaim::Fresh);
            };
            if stored_session != session_raw {
                tx.commit()?;
                return Ok(PromptAdmissionClaim::KeyReused { stored_digest });
            }
            match state.as_str() {
                "pending" => {
                    tx.commit()?;
                    Ok(PromptAdmissionClaim::InFlight)
                }
                "complete" => {
                    if stored_digest != request_digest {
                        tx.commit()?;
                        return Ok(PromptAdmissionClaim::KeyReused { stored_digest });
                    }
                    let Some(receipt_bytes) = receipt_bytes else {
                        return Err(StoreError::Corrupt(vec![corrupt_receipt]));
                    };
                    if receipt_bytes > MAX_PROMPT_ADMISSION_RECEIPT_BYTES as i64 {
                        return Err(StoreError::Oversized(oversized_receipt));
                    }
                    let receipt_json: String = tx.query_row(
                        "SELECT receipt_json FROM prompt_admission WHERE key = ?1",
                        params![key.as_str()],
                        |r| r.get(0),
                    )?;
                    tx.commit()?;
                    Ok(PromptAdmissionClaim::Complete(receipt_json))
                }
                _ => Err(StoreError::Corrupt(vec![corrupt_state])),
            }
        })
    }

    /// Complete a matching `pending` admission with the byte-exact prompt
    /// receipt JSON. A missing row, a foreign-session row and a row that is
    /// not pending are typed [`StoreError::Conflict`]s: completion is the
    /// acceptance marker, never a blind upsert.
    pub fn prompt_admission_complete(
        &self,
        session_id: SessionId,
        key: &str,
        receipt_json: &str,
    ) -> StoreResult<()> {
        check_key(key)?;
        if receipt_json.len() > MAX_PROMPT_ADMISSION_RECEIPT_BYTES {
            return Err(StoreError::Oversized(format!(
                "prompt admission receipt exceeds {MAX_PROMPT_ADMISSION_RECEIPT_BYTES} bytes"
            )));
        }
        let key = key.to_owned();
        let receipt_json = receipt_json.to_owned();
        let session_raw = session_id.raw() as i64;
        let missing = "prompt_admission_complete: no admission row for the key".to_owned();
        let foreign =
            "prompt_admission_complete: admission row belongs to another session".to_owned();
        let not_pending = "prompt_admission_complete: admission row is not pending".to_owned();
        self.writer
            .execute("prompt_admission_complete", move |conn| {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let changed = tx.execute(
                    "UPDATE prompt_admission SET state = 'complete', receipt_json = ?3
                     WHERE key = ?1 AND session_id = ?2 AND state = 'pending'",
                    params![key.as_str(), session_raw, receipt_json.as_str()],
                )?;
                if changed == 1 {
                    tx.commit()?;
                    return Ok(());
                }
                let row: Option<(i64, String)> = tx
                    .query_row(
                        "SELECT session_id, state FROM prompt_admission WHERE key = ?1",
                        params![key.as_str()],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                match row {
                    None => Err(StoreError::Conflict(missing)),
                    Some((stored_session, _)) if stored_session != session_raw => {
                        Err(StoreError::Conflict(foreign))
                    }
                    Some(_) => Err(StoreError::Conflict(not_pending)),
                }
            })
    }

    /// Release a STILL-PENDING admission (a prompt that failed before
    /// acceptance), so a retry of the same key may execute. A missing row
    /// and a completed row are no-ops: a completed receipt is never deleted
    /// by a release racing its own acceptance.
    pub fn prompt_admission_release(&self, session_id: SessionId, key: &str) -> StoreResult<()> {
        check_key(key)?;
        let key = key.to_owned();
        let session_raw = session_id.raw() as i64;
        self.writer
            .execute("prompt_admission_release", move |conn| {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                tx.execute(
                    "DELETE FROM prompt_admission
                     WHERE key = ?1 AND session_id = ?2 AND state = 'pending'",
                    params![key.as_str(), session_raw],
                )?;
                tx.commit()?;
                Ok(())
            })
    }
}

#[cfg(test)]
mod prompt_admission_tests {
    use super::*;
    use crate::Store;

    const KEY: &str = "11111111-2222-3333-4444-555555555555";
    const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OTHER_DIGEST: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn store_with_session(dir: &tempfile::TempDir) -> (Store, SessionId) {
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let sid = store.create_session(ws, "t", "p", "m").unwrap().id;
        (store, sid)
    }

    #[test]
    fn prompt_claim_fresh_then_complete_replays_the_exact_receipt_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        assert_eq!(
            store.prompt_admission_claim(sid, KEY, DIGEST, 1).unwrap(),
            PromptAdmissionClaim::Fresh
        );
        // A pending row refuses the concurrent duplicate.
        assert_eq!(
            store.prompt_admission_claim(sid, KEY, DIGEST, 2).unwrap(),
            PromptAdmissionClaim::InFlight
        );
        let receipt =
            r#"{"op_id":"41","run_id":"prompt-0000000000000007","accepted":true,"queued":false}"#;
        store.prompt_admission_complete(sid, KEY, receipt).unwrap();
        // The replay returns the stored bytes EXACTLY and mutates nothing.
        assert_eq!(
            store.prompt_admission_claim(sid, KEY, DIGEST, 3).unwrap(),
            PromptAdmissionClaim::Complete(receipt.to_string())
        );
        assert_eq!(
            store.prompt_admission_claim(sid, KEY, DIGEST, 4).unwrap(),
            PromptAdmissionClaim::Complete(receipt.to_string())
        );
        let count: i64 = store
            .read()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM prompt_admission", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "a replay never inserts a second admission row");
    }

    #[test]
    fn prompt_same_key_with_a_different_digest_is_key_reused_after_completion() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        store.prompt_admission_claim(sid, KEY, DIGEST, 1).unwrap();
        // Pending: a different digest is still the SAME in-flight logical
        // prompt (the pending winner owns the key).
        assert_eq!(
            store
                .prompt_admission_claim(sid, KEY, OTHER_DIGEST, 2)
                .unwrap(),
            PromptAdmissionClaim::InFlight
        );
        store.prompt_admission_complete(sid, KEY, "{}").unwrap();
        let claim = store
            .prompt_admission_claim(sid, KEY, OTHER_DIGEST, 3)
            .unwrap();
        assert_eq!(
            claim,
            PromptAdmissionClaim::KeyReused {
                stored_digest: DIGEST.to_string()
            },
            "a completed key with a different digest names the stored digest"
        );
    }

    #[test]
    fn prompt_release_only_deletes_a_pending_row_and_a_retry_reclaims_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        store.prompt_admission_claim(sid, KEY, DIGEST, 1).unwrap();
        // Releasing a missing/unknown key is a no-op.
        store
            .prompt_admission_release(sid, "00000000-0000-0000-0000-000000000000")
            .unwrap();
        store.prompt_admission_release(sid, KEY).unwrap();
        assert_eq!(
            store.prompt_admission_claim(sid, KEY, DIGEST, 2).unwrap(),
            PromptAdmissionClaim::Fresh,
            "a released pre-acceptance claim may re-execute"
        );
        store
            .prompt_admission_complete(sid, KEY, "receipt")
            .unwrap();
        // Release NEVER deletes a completed receipt.
        store.prompt_admission_release(sid, KEY).unwrap();
        assert_eq!(
            store.prompt_admission_claim(sid, KEY, DIGEST, 3).unwrap(),
            PromptAdmissionClaim::Complete("receipt".to_string())
        );
    }

    #[test]
    fn prompt_completion_refuses_missing_completed_and_foreign_rows_typed() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        match store.prompt_admission_complete(sid, KEY, "r") {
            Err(StoreError::Conflict(m)) => assert!(m.contains("no admission row"), "{m}"),
            other => panic!("missing row must be a typed conflict: {other:?}"),
        }
        store.prompt_admission_claim(sid, KEY, DIGEST, 1).unwrap();
        store.prompt_admission_complete(sid, KEY, "r").unwrap();
        match store.prompt_admission_complete(sid, KEY, "r2") {
            Err(StoreError::Conflict(m)) => assert!(m.contains("not pending"), "{m}"),
            other => panic!("double completion must be a typed conflict: {other:?}"),
        }
        // A foreign session can neither complete nor release the row.
        let ws2 = store.create_workspace("/w2").unwrap();
        let other_sid = store.create_session(ws2, "other", "p", "m").unwrap().id;
        match store.prompt_admission_complete(other_sid, KEY, "r3") {
            Err(StoreError::Conflict(m)) => assert!(m.contains("another session"), "{m}"),
            other => panic!("foreign completion must be a typed conflict: {other:?}"),
        }
        // The foreign-session claim names the key as reused.
        assert_eq!(
            store
                .prompt_admission_claim(other_sid, KEY, DIGEST, 4)
                .unwrap(),
            PromptAdmissionClaim::KeyReused {
                stored_digest: DIGEST.to_string()
            }
        );
    }

    #[test]
    fn prompt_oversized_keys_digests_and_receipts_are_typed_and_write_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        let long_key = "a".repeat(MAX_PROMPT_ADMISSION_KEY_BYTES + 1);
        assert!(matches!(
            store.prompt_admission_claim(sid, &long_key, DIGEST, 1),
            Err(StoreError::Oversized(_))
        ));
        assert!(matches!(
            store.prompt_admission_claim(sid, "", DIGEST, 1),
            Err(StoreError::Oversized(_))
        ));
        let long_digest = "a".repeat(MAX_PROMPT_ADMISSION_DIGEST_BYTES + 1);
        assert!(matches!(
            store.prompt_admission_claim(sid, KEY, &long_digest, 1),
            Err(StoreError::Oversized(_))
        ));
        store.prompt_admission_claim(sid, KEY, DIGEST, 1).unwrap();
        let huge_receipt = "x".repeat(MAX_PROMPT_ADMISSION_RECEIPT_BYTES + 1);
        assert!(matches!(
            store.prompt_admission_complete(sid, KEY, &huge_receipt),
            Err(StoreError::Oversized(_))
        ));
        // The failed oversized writes left the pending row untouched.
        assert_eq!(
            store.prompt_admission_claim(sid, KEY, DIGEST, 2).unwrap(),
            PromptAdmissionClaim::InFlight
        );
        let count: i64 = store
            .read()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM prompt_admission", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "refused writes never insert extra rows");
    }

    #[test]
    fn prompt_corrupt_injected_rows_are_loud_never_guessed() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        // Simulate a hostile/tampered database whose table lacks the fresh
        // CHECK vocabulary (a hand-rolled table): every read must refuse
        // typed, never guess.
        store
            .read()
            .unwrap()
            .execute_batch(
                "DROP TABLE prompt_admission;
                 CREATE TABLE prompt_admission (
                    key TEXT PRIMARY KEY,
                    session_id INTEGER NOT NULL,
                    request_digest TEXT NOT NULL,
                    state TEXT NOT NULL,
                    receipt_json TEXT,
                    created_ms INTEGER NOT NULL
                 );",
            )
            .unwrap();
        store
            .read()
            .unwrap()
            .execute(
                "INSERT INTO prompt_admission(
                    key, session_id, request_digest, state, receipt_json, created_ms)
                 VALUES (?1, ?2, ?3, 'bogus', NULL, 0)",
                params![KEY, sid.raw() as i64, DIGEST],
            )
            .unwrap();
        match store.prompt_admission_claim(sid, KEY, DIGEST, 2) {
            Err(StoreError::Corrupt(_)) => {}
            other => panic!("unknown state must be loud corruption: {other:?}"),
        }
        // A complete row without a receipt is corruption, never an empty
        // receipt replay.
        store
            .read()
            .unwrap()
            .execute(
                "UPDATE prompt_admission SET state = 'complete', receipt_json = NULL WHERE key = ?1",
                params![KEY],
            )
            .unwrap();
        match store.prompt_admission_claim(sid, KEY, DIGEST, 3) {
            Err(StoreError::Corrupt(_)) => {}
            other => panic!("a receipt-less complete row must be corruption: {other:?}"),
        }
        // An oversized injected receipt refuses typed on read.
        let huge = "x".repeat(MAX_PROMPT_ADMISSION_RECEIPT_BYTES + 1);
        store
            .read()
            .unwrap()
            .execute(
                "UPDATE prompt_admission SET state = 'complete', receipt_json = ?2 WHERE key = ?1",
                params![KEY, huge],
            )
            .unwrap();
        match store.prompt_admission_claim(sid, KEY, DIGEST, 4) {
            Err(StoreError::Oversized(_)) => {}
            other => panic!("an oversized receipt must refuse typed: {other:?}"),
        }
    }

    #[test]
    fn concurrent_prompt_claimers_see_exactly_one_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        let store = std::sync::Arc::new(store);
        let mut handles = Vec::new();
        for i in 0..8 {
            let store = store.clone();
            handles.push(std::thread::spawn(move || {
                store.prompt_admission_claim(sid, KEY, DIGEST, i).unwrap()
            }));
        }
        let claims: Vec<PromptAdmissionClaim> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        let fresh = claims
            .iter()
            .filter(|c| **c == PromptAdmissionClaim::Fresh)
            .count();
        let in_flight = claims
            .iter()
            .filter(|c| **c == PromptAdmissionClaim::InFlight)
            .count();
        assert_eq!(fresh, 1, "exactly one claimer may win the key: {claims:?}");
        assert_eq!(in_flight, claims.len() - 1, "{claims:?}");
        // And one completion turns every later duplicate into the SAME
        // byte-exact replay.
        store
            .prompt_admission_complete(sid, KEY, "receipt")
            .unwrap();
        for claim in [
            store.prompt_admission_claim(sid, KEY, DIGEST, 9).unwrap(),
            store.prompt_admission_claim(sid, KEY, DIGEST, 10).unwrap(),
        ] {
            assert_eq!(claim, PromptAdmissionClaim::Complete("receipt".to_string()));
        }
    }

    #[test]
    fn prompt_admissions_are_independent_per_session() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        let ws2 = store.create_workspace("/w2").unwrap();
        let sid2 = store.create_session(ws2, "t2", "p", "m").unwrap().id;
        assert_eq!(
            store.prompt_admission_claim(sid, KEY, DIGEST, 1).unwrap(),
            PromptAdmissionClaim::Fresh
        );
        assert_eq!(
            store
                .prompt_admission_claim(sid2, "other-key", DIGEST, 1)
                .unwrap(),
            PromptAdmissionClaim::Fresh
        );
        // The SAME key on a second session is a typed KeyReused (a key never
        // aliases two sessions' logical prompts).
        assert_eq!(
            store.prompt_admission_claim(sid2, KEY, DIGEST, 1).unwrap(),
            PromptAdmissionClaim::KeyReused {
                stored_digest: DIGEST.to_string()
            }
        );
    }

    #[test]
    fn fresh_database_carries_the_prompt_admission_table_and_head() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let conn = store.raw_conn();
        let present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='prompt_admission'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(present, 1, "fresh schema must carry prompt_admission");
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            version, 29,
            "v28 (per-session artifact identity) is the migration head"
        );
        // The CHECK vocabulary refuses an unknown state at the SQL level.
        let pid = SessionId::new(1);
        conn.execute(
            "INSERT INTO prompt_admission(key, session_id, request_digest, state, receipt_json, created_ms)
             VALUES ('k', ?1, 'd', 'bogus', NULL, 0)",
            params![pid.raw() as i64],
        )
        .unwrap_err();
    }
}
