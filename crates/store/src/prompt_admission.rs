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
//!
//! Audit P1 (durable admission ownership): every claim also carries the
//! boot-instance identity of the claiming store open (`owner_generation`), a
//! bounded `lease_deadline_ms` and the durable `reservation` (`tx-<op>`)
//! naming the accepted turn the claim is bound to. A pending row whose owner
//! is a previous boot or whose lease expired is
//! [`PromptAdmissionClaim::Stale`], never `InFlight` forever: startup
//! recovery classifies it against the durable facts and lands it as a
//! completed replay, a safe reclaim, or a typed conflict.

use std::sync::Arc;

use rusqlite::{params, OptionalExtension, TransactionBehavior};

use faktor_core::id::SessionId;

use super::{
    AdmissionResolution, AdmissionRowTuple, PendingAdmission, PendingAdmissionPage, Store,
    StoreError, StoreResult, MAX_PENDING_ADMISSION_PAGE,
};

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

/// Bound on one durable reservation id (`tx-<16 hex>`).
pub const MAX_PROMPT_ADMISSION_RESERVATION_BYTES: usize = 64;

/// The typed outcome of one prompt submission-keyed admission claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptAdmissionClaim {
    /// No row existed; one `pending` row was inserted for this caller under
    /// the caller's reservation.
    Fresh,
    /// The SAME session + key + request digest already completed: the exact
    /// stored receipt JSON (byte-for-byte what the first success stored).
    Complete(String),
    /// The same key is still a LIVE claim of THIS boot: another admission is
    /// in flight and a concurrent duplicate must retry, never execute a
    /// second prompt.
    InFlight,
    /// The same key is pending but the claim is STALE (a previous boot's
    /// owner, or a lease that expired): recovery classifies the row against
    /// the durable facts and resolves it; this is never answered as a
    /// perpetual in-flight refusal.
    Stale(PendingAdmission),
    /// The key belongs to a DIFFERENT session, completed with a different
    /// request digest, or landed as a recovery conflict: a typed conflict
    /// that must never run.
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

    /// The stale pending row of [`Self::Stale`].
    pub fn stale_row(&self) -> Option<&PendingAdmission> {
        match self {
            Self::Stale(row) => Some(row),
            _ => None,
        }
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

fn check_reservation(reservation: &str) -> StoreResult<()> {
    if reservation.is_empty() || reservation.len() > MAX_PROMPT_ADMISSION_RESERVATION_BYTES {
        return Err(StoreError::Oversized(format!(
            "prompt admission reservation must be 1..={MAX_PROMPT_ADMISSION_RESERVATION_BYTES} bytes"
        )));
    }
    Ok(())
}

fn check_receipt(receipt_json: &str) -> StoreResult<()> {
    if receipt_json.len() > MAX_PROMPT_ADMISSION_RECEIPT_BYTES {
        return Err(StoreError::Oversized(format!(
            "prompt admission receipt exceeds {MAX_PROMPT_ADMISSION_RECEIPT_BYTES} bytes"
        )));
    }
    Ok(())
}

impl Store {
    /// Claim the durable admission of ONE submission-keyed prompt. ONE
    /// writer job in an IMMEDIATE transaction; every value is prepared on
    /// the caller's thread. Absent -> INSERT `pending` owned by THIS boot
    /// with `reservation` and a fresh lease; present `complete` with the
    /// equal digest -> [`Complete`] with the stored receipt; present
    /// `complete` with a different digest (or a foreign session, or a
    /// recovery `conflict`) -> [`KeyReused`]; present `pending` -> `InFlight`
    /// when it is a live claim of this boot, otherwise [`Stale`] for the
    /// recovery pass (never a perpetual in-flight answer).
    ///
    /// [`Complete`]: PromptAdmissionClaim::Complete
    /// [`KeyReused`]: PromptAdmissionClaim::KeyReused
    /// [`Stale`]: PromptAdmissionClaim::Stale
    pub fn prompt_admission_claim(
        &self,
        session_id: SessionId,
        key: &str,
        request_digest: &str,
        reservation: &str,
        now: i64,
    ) -> StoreResult<PromptAdmissionClaim> {
        check_key(key)?;
        check_digest(request_digest)?;
        check_reservation(reservation)?;
        let key = key.to_owned();
        let request_digest = request_digest.to_owned();
        let reservation = reservation.to_owned();
        let generation = self.generation.clone();
        let session_raw = session_id.raw() as i64;
        let lease_deadline_ms = now.saturating_add(super::ADMISSION_LEASE_MS);
        let seam = Arc::clone(&self.seam);
        let corrupt_state = "prompt_admission row carries an unknown state".to_owned();
        let corrupt_receipt = "prompt_admission complete row carries no receipt".to_owned();
        let oversized_receipt =
            format!("prompt_admission receipt exceeds {MAX_PROMPT_ADMISSION_RECEIPT_BYTES} bytes");
        self.writer.execute("prompt_admission_claim", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let row: Option<AdmissionRowTuple> = tx
                .query_row(
                    "SELECT session_id, request_digest, state,
                            CASE WHEN receipt_json IS NULL THEN NULL
                                 ELSE length(CAST(receipt_json AS BLOB)) END,
                            owner_generation, lease_deadline_ms, reservation, created_ms
                     FROM prompt_admission WHERE key = ?1",
                    params![key.as_str()],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                            r.get(7)?,
                        ))
                    },
                )
                .optional()?;
            let Some((
                stored_session,
                stored_digest,
                state,
                receipt_bytes,
                owner_generation,
                lease_deadline,
                stored_reservation,
                created_ms,
            )) = row
            else {
                tx.execute(
                    "INSERT INTO prompt_admission(
                        key, session_id, request_digest, state, receipt_json, created_ms,
                        owner_generation, lease_deadline_ms, reservation)
                     VALUES (?1, ?2, ?3, 'pending', NULL, ?4, ?5, ?6, ?7)",
                    params![
                        key.as_str(),
                        session_raw,
                        request_digest.as_str(),
                        now,
                        generation.as_str(),
                        lease_deadline_ms,
                        reservation.as_str()
                    ],
                )?;
                seam.trip("prompt_admission_claim_precommit");
                tx.commit()?;
                seam.trip("prompt_admission_claim_committed");
                return Ok(PromptAdmissionClaim::Fresh);
            };
            if stored_session != session_raw {
                tx.commit()?;
                return Ok(PromptAdmissionClaim::KeyReused { stored_digest });
            }
            let outcome = match state.as_str() {
                "pending" => {
                    // DIGEST FIRST (idempotency contract): different bytes on
                    // a reused key are KeyReused even while the row is live
                    // or stale; only an identical claim is InFlight/Stale.
                    if stored_digest != request_digest {
                        PromptAdmissionClaim::KeyReused { stored_digest }
                    } else {
                        let stale = PendingAdmission {
                            key: key.clone(),
                            session_id,
                            request_digest: stored_digest,
                            reservation: stored_reservation,
                            owner_generation,
                            lease_deadline_ms: lease_deadline,
                            created_ms,
                        };
                        if stale.is_live_for(&generation, now) {
                            PromptAdmissionClaim::InFlight
                        } else {
                            PromptAdmissionClaim::Stale(stale)
                        }
                    }
                }
                "complete" => {
                    if stored_digest != request_digest {
                        PromptAdmissionClaim::KeyReused { stored_digest }
                    } else {
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
                        PromptAdmissionClaim::Complete(receipt_json)
                    }
                }
                "conflict" => PromptAdmissionClaim::KeyReused { stored_digest },
                _ => return Err(StoreError::Corrupt(vec![corrupt_state])),
            };
            seam.trip("prompt_admission_claim_precommit");
            tx.commit()?;
            seam.trip("prompt_admission_claim_committed");
            Ok(outcome)
        })
    }

    /// Complete a matching `pending` admission with the byte-exact prompt
    /// receipt JSON. The update is fenced by the claim's `reservation`: a
    /// missing row, a foreign-session row, a superseded/reclaimed claim and
    /// a row that is not pending are typed [`StoreError::Conflict`]s.
    pub fn prompt_admission_complete(
        &self,
        session_id: SessionId,
        key: &str,
        reservation: &str,
        receipt_json: &str,
    ) -> StoreResult<()> {
        check_key(key)?;
        check_reservation(reservation)?;
        check_receipt(receipt_json)?;
        let key = key.to_owned();
        let reservation = reservation.to_owned();
        let receipt_json = receipt_json.to_owned();
        let session_raw = session_id.raw() as i64;
        let seam = Arc::clone(&self.seam);
        let missing = "prompt_admission_complete: no admission row for the key".to_owned();
        let foreign =
            "prompt_admission_complete: admission row belongs to another session".to_owned();
        let not_pending = "prompt_admission_complete: admission row is not pending".to_owned();
        let superseded =
            "prompt_admission_complete: admission row was reclaimed or superseded".to_owned();
        self.writer
            .execute("prompt_admission_complete", move |conn| {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let changed = tx.execute(
                    "UPDATE prompt_admission SET state = 'complete', receipt_json = ?4
                     WHERE key = ?1 AND session_id = ?2 AND state = 'pending'
                       AND reservation IS ?3",
                    params![
                        key.as_str(),
                        session_raw,
                        reservation.as_str(),
                        receipt_json.as_str()
                    ],
                )?;
                if changed == 1 {
                    seam.trip("prompt_admission_complete_precommit");
                    tx.commit()?;
                    seam.trip("prompt_admission_complete_committed");
                    return Ok(());
                }
                let row: Option<(i64, String, Option<String>)> = tx
                    .query_row(
                        "SELECT session_id, state, reservation FROM prompt_admission WHERE key = ?1",
                        params![key.as_str()],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?;
                match row {
                    None => Err(StoreError::Conflict(missing)),
                    Some((stored_session, _, _)) if stored_session != session_raw => {
                        Err(StoreError::Conflict(foreign))
                    }
                    Some((_, _, stored_reservation))
                        if stored_reservation.as_deref() != Some(reservation.as_str()) =>
                    {
                        Err(StoreError::Conflict(superseded))
                    }
                    Some(_) => Err(StoreError::Conflict(not_pending)),
                }
            })
    }

    /// Release a STILL-PENDING admission this caller owns (a prompt that
    /// failed before acceptance), so a retry of the same key may execute.
    /// Fenced by `reservation`: a missing row, a completed row and a
    /// superseded/reclaimed claim are no-ops.
    pub fn prompt_admission_release(
        &self,
        session_id: SessionId,
        key: &str,
        reservation: &str,
    ) -> StoreResult<()> {
        check_key(key)?;
        check_reservation(reservation)?;
        let key = key.to_owned();
        let reservation = reservation.to_owned();
        let session_raw = session_id.raw() as i64;
        self.writer
            .execute("prompt_admission_release", move |conn| {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                tx.execute(
                    "DELETE FROM prompt_admission
                     WHERE key = ?1 AND session_id = ?2 AND state = 'pending'
                       AND reservation IS ?3",
                    params![key.as_str(), session_raw, reservation.as_str()],
                )?;
                tx.commit()?;
                Ok(())
            })
    }

    /// One bounded keyset page of `pending` prompt admissions (recovery
    /// scan). Ordered by key; `after_key` is the exclusive cursor.
    pub fn prompt_admission_pending_page(
        &self,
        after_key: Option<&str>,
        limit: u32,
    ) -> StoreResult<PendingAdmissionPage> {
        let limit = limit.clamp(1, MAX_PENDING_ADMISSION_PAGE) as i64;
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT key, session_id, request_digest, reservation, owner_generation,
                    lease_deadline_ms, created_ms
             FROM prompt_admission
             WHERE state = 'pending' AND (?1 IS NULL OR key > ?1)
             ORDER BY key
             LIMIT ?2",
        )?;
        let probe = limit + 1;
        let rows = stmt.query_map(params![after_key, probe], super::pending_admission_row)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        let has_more = out.len() as i64 > limit;
        if has_more {
            out.truncate(limit as usize);
        }
        Ok(PendingAdmissionPage {
            rows: out,
            has_more,
        })
    }

    /// Resolve ONE stale pending prompt admission exactly once (audit P1).
    /// Fenced by `expected_reservation`; returns `true` when the resolution
    /// landed on the row it named, `false` when the row moved on. A row is
    /// never left pending by a resolution that returned `true`.
    pub fn prompt_admission_resolve(
        &self,
        key: &str,
        expected_reservation: Option<&str>,
        resolution: AdmissionResolution,
    ) -> StoreResult<bool> {
        check_key(key)?;
        if let AdmissionResolution::Complete(receipt_json) = &resolution {
            check_receipt(receipt_json)?;
        }
        let key = key.to_owned();
        let expected_reservation = expected_reservation.map(str::to_owned);
        self.writer
            .execute("prompt_admission_resolve", move |conn| {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let changed = match resolution {
                    AdmissionResolution::Complete(receipt_json) => tx.execute(
                        "UPDATE prompt_admission SET state = 'complete', receipt_json = ?3
                     WHERE key = ?1 AND state = 'pending' AND reservation IS ?2",
                        params![key.as_str(), expected_reservation.as_deref(), receipt_json],
                    )?,
                    AdmissionResolution::Reclaim => tx.execute(
                        "DELETE FROM prompt_admission
                     WHERE key = ?1 AND state = 'pending' AND reservation IS ?2",
                        params![key.as_str(), expected_reservation.as_deref()],
                    )?,
                    AdmissionResolution::KeyReuse => tx.execute(
                        "UPDATE prompt_admission SET state = 'conflict', receipt_json = NULL
                     WHERE key = ?1 AND state = 'pending' AND reservation IS ?2",
                        params![key.as_str(), expected_reservation.as_deref()],
                    )?,
                };
                tx.commit()?;
                Ok(changed == 1)
            })
    }
}

#[cfg(test)]
mod prompt_admission_tests {
    use super::*;
    use crate::{AdmissionResolution, Store};

    const KEY: &str = "11111111-2222-3333-4444-555555555555";
    const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OTHER_DIGEST: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const RES: &str = "tx-0000000000000001";

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
            store
                .prompt_admission_claim(sid, KEY, DIGEST, RES, 1)
                .unwrap(),
            PromptAdmissionClaim::Fresh
        );
        // A pending row refuses the concurrent duplicate.
        assert_eq!(
            store
                .prompt_admission_claim(sid, KEY, DIGEST, RES, 2)
                .unwrap(),
            PromptAdmissionClaim::InFlight
        );
        let receipt =
            r#"{"op_id":"41","run_id":"prompt-0000000000000007","accepted":true,"queued":false}"#;
        store
            .prompt_admission_complete(sid, KEY, RES, receipt)
            .unwrap();
        // The replay returns the stored bytes EXACTLY and mutates nothing.
        assert_eq!(
            store
                .prompt_admission_claim(sid, KEY, DIGEST, "tx-0000000000000002", 3)
                .unwrap(),
            PromptAdmissionClaim::Complete(receipt.to_string())
        );
        assert_eq!(
            store
                .prompt_admission_claim(sid, KEY, DIGEST, "tx-0000000000000003", 4)
                .unwrap(),
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
    fn prompt_same_key_with_a_different_digest_is_key_reused_at_every_state() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        store
            .prompt_admission_claim(sid, KEY, DIGEST, RES, 1)
            .unwrap();
        let reused = PromptAdmissionClaim::KeyReused {
            stored_digest: DIGEST.to_string(),
        };
        // Pending (live) + different bytes: KeyReused, never InFlight.
        assert_eq!(
            store
                .prompt_admission_claim(sid, KEY, OTHER_DIGEST, "tx-0000000000000002", 2)
                .unwrap(),
            reused
        );
        // Pending (expired lease) + different bytes: still KeyReused, never
        // Stale. The claim landed at now=1, so the deadline is LEASE+1.
        let expired = super::super::ADMISSION_LEASE_MS + 1;
        assert_eq!(
            store
                .prompt_admission_claim(sid, KEY, OTHER_DIGEST, "tx-0000000000000003", expired)
                .unwrap(),
            reused
        );
        // Identical bytes are InFlight while live...
        assert_eq!(
            store
                .prompt_admission_claim(sid, KEY, DIGEST, "tx-0000000000000004", 2)
                .unwrap(),
            PromptAdmissionClaim::InFlight
        );
        // ...and Stale once the lease expired.
        assert!(matches!(
            store
                .prompt_admission_claim(sid, KEY, DIGEST, "tx-0000000000000005", expired)
                .unwrap(),
            PromptAdmissionClaim::Stale(_)
        ));
        store
            .prompt_admission_complete(sid, KEY, RES, "{}")
            .unwrap();
        // Complete + different bytes: KeyReused naming the stored digest.
        assert_eq!(
            store
                .prompt_admission_claim(sid, KEY, OTHER_DIGEST, "tx-0000000000000006", expired + 1)
                .unwrap(),
            reused
        );
        // Complete + identical: byte-exact replay.
        assert_eq!(
            store
                .prompt_admission_claim(sid, KEY, DIGEST, "tx-0000000000000007", expired + 1)
                .unwrap(),
            PromptAdmissionClaim::Complete("{}".to_string())
        );
    }

    #[test]
    fn prompt_release_only_deletes_a_pending_row_and_a_retry_reclaims_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        store
            .prompt_admission_claim(sid, KEY, DIGEST, RES, 1)
            .unwrap();
        // Releasing a missing/unknown key is a no-op.
        store
            .prompt_admission_release(sid, "00000000-0000-0000-0000-000000000000", RES)
            .unwrap();
        // A foreign reservation can never delete the row.
        store
            .prompt_admission_release(sid, KEY, "tx-00000000000000ff")
            .unwrap();
        assert!(matches!(
            store
                .prompt_admission_claim(sid, KEY, DIGEST, RES, 2)
                .unwrap(),
            PromptAdmissionClaim::InFlight
        ));
        store.prompt_admission_release(sid, KEY, RES).unwrap();
        assert_eq!(
            store
                .prompt_admission_claim(sid, KEY, DIGEST, "tx-0000000000000002", 2)
                .unwrap(),
            PromptAdmissionClaim::Fresh,
            "a released pre-acceptance claim may re-execute"
        );
        store
            .prompt_admission_complete(sid, KEY, "tx-0000000000000002", "receipt")
            .unwrap();
        // Release NEVER deletes a completed receipt.
        store.prompt_admission_release(sid, KEY, RES).unwrap();
        assert_eq!(
            store
                .prompt_admission_claim(sid, KEY, DIGEST, "tx-0000000000000003", 3)
                .unwrap(),
            PromptAdmissionClaim::Complete("receipt".to_string())
        );
    }

    #[test]
    fn prompt_completion_refuses_missing_completed_foreign_and_superseded_rows_typed() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        match store.prompt_admission_complete(sid, KEY, RES, "r") {
            Err(StoreError::Conflict(m)) => assert!(m.contains("no admission row"), "{m}"),
            other => panic!("missing row must be a typed conflict: {other:?}"),
        }
        store
            .prompt_admission_claim(sid, KEY, DIGEST, RES, 1)
            .unwrap();
        store.prompt_admission_complete(sid, KEY, RES, "r").unwrap();
        match store.prompt_admission_complete(sid, KEY, RES, "r2") {
            Err(StoreError::Conflict(m)) => assert!(m.contains("not pending"), "{m}"),
            other => panic!("double completion must be a typed conflict: {other:?}"),
        }
        // A superseding claim fences the stale owner out.
        let key2 = "22222222-2222-4333-8444-555555555555";
        store
            .prompt_admission_claim(sid, key2, DIGEST, RES, 1)
            .unwrap();
        assert!(store
            .prompt_admission_resolve(key2, Some(RES), AdmissionResolution::Reclaim)
            .unwrap());
        store
            .prompt_admission_claim(sid, key2, DIGEST, "tx-0000000000000009", 2)
            .unwrap();
        match store.prompt_admission_complete(sid, key2, RES, "r3") {
            Err(StoreError::Conflict(m)) => assert!(m.contains("superseded"), "{m}"),
            other => panic!("a stale reservation must be fenced typed: {other:?}"),
        }
        // A foreign session can neither complete nor release the row.
        let ws2 = store.create_workspace("/w2").unwrap();
        let other_sid = store.create_session(ws2, "other", "p", "m").unwrap().id;
        match store.prompt_admission_complete(other_sid, key2, "tx-0000000000000009", "r3") {
            Err(StoreError::Conflict(m)) => assert!(m.contains("another session"), "{m}"),
            other => panic!("foreign completion must be a typed conflict: {other:?}"),
        }
        // The foreign-session claim names the key as reused.
        assert_eq!(
            store
                .prompt_admission_claim(other_sid, key2, DIGEST, "tx-000000000000000a", 4)
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
            store.prompt_admission_claim(sid, &long_key, DIGEST, RES, 1),
            Err(StoreError::Oversized(_))
        ));
        assert!(matches!(
            store.prompt_admission_claim(sid, "", DIGEST, RES, 1),
            Err(StoreError::Oversized(_))
        ));
        let long_digest = "a".repeat(MAX_PROMPT_ADMISSION_DIGEST_BYTES + 1);
        assert!(matches!(
            store.prompt_admission_claim(sid, KEY, &long_digest, RES, 1),
            Err(StoreError::Oversized(_))
        ));
        let long_res = "r".repeat(MAX_PROMPT_ADMISSION_RESERVATION_BYTES + 1);
        assert!(matches!(
            store.prompt_admission_claim(sid, KEY, DIGEST, &long_res, 1),
            Err(StoreError::Oversized(_))
        ));
        store
            .prompt_admission_claim(sid, KEY, DIGEST, RES, 1)
            .unwrap();
        let huge_receipt = "x".repeat(MAX_PROMPT_ADMISSION_RECEIPT_BYTES + 1);
        assert!(matches!(
            store.prompt_admission_complete(sid, KEY, RES, &huge_receipt),
            Err(StoreError::Oversized(_))
        ));
        // The failed oversized writes left the pending row untouched.
        assert_eq!(
            store
                .prompt_admission_claim(sid, KEY, DIGEST, RES, 2)
                .unwrap(),
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
        // CHECK vocabulary (a hand-rolled table with the modern columns):
        // every read must refuse typed, never guess.
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
                    created_ms INTEGER NOT NULL,
                    owner_generation TEXT NOT NULL DEFAULT '',
                    lease_deadline_ms INTEGER NOT NULL DEFAULT 0,
                    reservation TEXT
                 );",
            )
            .unwrap();
        store
            .read()
            .unwrap()
            .execute(
                "INSERT INTO prompt_admission(
                    key, session_id, request_digest, state, receipt_json, created_ms,
                    owner_generation, lease_deadline_ms, reservation)
                 VALUES (?1, ?2, ?3, 'bogus', NULL, 0, 'g', 1, 'r')",
                params![KEY, sid.raw() as i64, DIGEST],
            )
            .unwrap();
        match store.prompt_admission_claim(sid, KEY, DIGEST, RES, 2) {
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
        match store.prompt_admission_claim(sid, KEY, DIGEST, RES, 3) {
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
        match store.prompt_admission_claim(sid, KEY, DIGEST, RES, 4) {
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
            let res = format!("tx-{i:016x}");
            handles.push(std::thread::spawn(move || {
                store
                    .prompt_admission_claim(sid, KEY, DIGEST, &res, i)
                    .unwrap()
            }));
        }
        let claims: Vec<PromptAdmissionClaim> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        let fresh = claims.iter().filter(|c| c.is_fresh()).count();
        let in_flight = claims.iter().filter(|c| c.is_in_flight()).count();
        assert_eq!(fresh, 1, "exactly one claimer may win the key: {claims:?}");
        assert_eq!(in_flight, claims.len() - 1, "{claims:?}");
        // And one completion turns every later duplicate into the SAME
        // byte-exact replay.
        let winner_res = claims
            .iter()
            .position(|c| c.is_fresh())
            .map(|i| format!("tx-{i:016x}"))
            .unwrap();
        store
            .prompt_admission_complete(sid, KEY, &winner_res, "receipt")
            .unwrap();
        for claim in [
            store
                .prompt_admission_claim(sid, KEY, DIGEST, "tx-00000000000000f0", 9)
                .unwrap(),
            store
                .prompt_admission_claim(sid, KEY, DIGEST, "tx-00000000000000f1", 10)
                .unwrap(),
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
            store
                .prompt_admission_claim(sid, KEY, DIGEST, RES, 1)
                .unwrap(),
            PromptAdmissionClaim::Fresh
        );
        assert_eq!(
            store
                .prompt_admission_claim(sid2, "other-key", DIGEST, "tx-0000000000000021", 1)
                .unwrap(),
            PromptAdmissionClaim::Fresh
        );
        // The SAME key on a second session is a typed KeyReused (a key never
        // aliases two sessions' logical prompts).
        assert_eq!(
            store
                .prompt_admission_claim(sid2, KEY, DIGEST, "tx-0000000000000022", 1)
                .unwrap(),
            PromptAdmissionClaim::KeyReused {
                stored_digest: DIGEST.to_string()
            }
        );
    }

    #[test]
    fn a_previous_boot_pending_prompt_is_stale_reclaimable_and_recoverable() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        store
            .prompt_admission_claim(sid, KEY, DIGEST, RES, 1)
            .unwrap();
        let previous_generation = store.generation().to_string();
        drop(store);
        let store = Store::open(dir.path(), true).unwrap();
        let claim = store
            .prompt_admission_claim(sid, KEY, DIGEST, "tx-0000000000000002", 2)
            .unwrap();
        let row = match claim {
            PromptAdmissionClaim::Stale(row) => row,
            other => panic!("a previous-generation pending row must be Stale: {other:?}"),
        };
        assert_eq!(row.owner_generation, previous_generation);
        let page = store.prompt_admission_pending_page(None, 10).unwrap();
        assert_eq!(page.rows.len(), 1);
        assert!(!page.has_more);
        let receipt =
            r#"{"op_id":"41","run_id":"tx-0000000000000041","accepted":true,"queued":false}"#;
        assert!(store
            .prompt_admission_resolve(
                KEY,
                row.reservation.as_deref(),
                AdmissionResolution::Complete(receipt.to_string())
            )
            .unwrap());
        assert_eq!(
            store
                .prompt_admission_claim(sid, KEY, DIGEST, "tx-0000000000000003", 3)
                .unwrap(),
            PromptAdmissionClaim::Complete(receipt.to_string()),
            "the recovered receipt replays byte-exactly"
        );
    }

    #[test]
    fn an_expired_prompt_lease_is_stale_even_within_one_generation() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        store
            .prompt_admission_claim(sid, KEY, DIGEST, RES, 0)
            .unwrap();
        assert_eq!(
            store
                .prompt_admission_claim(sid, KEY, DIGEST, RES, super::super::ADMISSION_LEASE_MS - 1)
                .unwrap(),
            PromptAdmissionClaim::InFlight
        );
        let claim = store
            .prompt_admission_claim(sid, KEY, DIGEST, RES, super::super::ADMISSION_LEASE_MS)
            .unwrap();
        assert!(matches!(claim, PromptAdmissionClaim::Stale(_)), "{claim:?}");
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
            version, 31,
            "v30 (admission ownership) is the migration head"
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
