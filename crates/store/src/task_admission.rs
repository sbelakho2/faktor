//! Durable task-start admission (idempotency finding 1, backend half): ONE
//! durable row per client submission key, claimed BEFORE any task mutation
//! and completed with the byte-exact run receipt once the run was accepted.
//! A repeated key returns the stored receipt without a second admission; a
//! different request digest under the same key is a typed
//! [`TaskAdmissionClaim::KeyReused`] conflict; a pending row refuses a
//! concurrent duplicate with [`TaskAdmissionClaim::InFlight`].
//!
//! Audit P1 (durable admission ownership): every claim also carries the
//! boot-instance identity of the claiming store open (`owner_generation`), a
//! bounded `lease_deadline_ms` and the durable `reservation` naming the
//! accepted fact it is bound to. A pending row whose owner is a PREVIOUS
//! boot or whose lease expired is [`TaskAdmissionClaim::Stale`], never
//! `InFlight` forever: startup recovery classifies it against the durable
//! facts and lands it as a completed replay, a safe reclaim, or a typed
//! conflict. Completion/release are fenced by the reservation, so a stale
//! owner can never stamp a receipt onto a superseding claim.

use std::sync::Arc;

use rusqlite::{params, OptionalExtension, TransactionBehavior};

use faktor_core::id::SessionId;

use super::{Store, StoreError, StoreResult};

/// Bound on one submission key (the client's UUID-shaped submission id).
pub const MAX_TASK_ADMISSION_KEY_BYTES: usize = 64;

/// Bound on the canonical request digest of one task start (bare 64-hex with
/// headroom for a labelled form).
pub const MAX_TASK_ADMISSION_DIGEST_BYTES: usize = 128;

/// Bound on the stored run receipt JSON. Receipts are small (run identity,
/// mode, op id, queued flag), but the column is JSON an operator may extend;
/// the cap is enforced on write AND on read, so a hostile injected row can
/// never balloon a reader.
pub const MAX_TASK_ADMISSION_RECEIPT_BYTES: usize = 16 * 1024;

/// Bound on one durable reservation id (`tx-<16 hex>` / `run-<16 hex>`, with
/// headroom for a labelled form).
pub const MAX_TASK_ADMISSION_RESERVATION_BYTES: usize = 64;

/// Claim lifetime of one pending admission (audit P1): a pending row whose
/// deadline passed (or whose owner is a previous boot) is stale — startup
/// recovery resolves it instead of answering `InFlight` forever. The hold
/// window is one synchronous admission (submit + spawn, no drive await), so
/// five minutes is an order-of-magnitude generous bound.
pub const ADMISSION_LEASE_MS: i64 = 5 * 60 * 1000;

/// Bounded page of the pending-admission recovery scan.
pub const MAX_PENDING_ADMISSION_PAGE: u32 = 500;

/// The raw columns one admission read uses: `(session_id, request_digest,
/// state, receipt_bytes, owner_generation, lease_deadline_ms, reservation,
/// created_ms)`.
pub(crate) type AdmissionRowTuple = (
    i64,
    String,
    String,
    Option<i64>,
    String,
    i64,
    Option<String>,
    i64,
);

/// One durable `pending` admission row as recovery sees it (audit P1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAdmission {
    pub key: String,
    pub session_id: SessionId,
    pub request_digest: String,
    /// The durable accepted-fact linkage reserved at claim time
    /// (`tx-<op>` in-session, `run-<op>` orchestrated). `None` only on a
    /// pre-v30 legacy row, which recovery refuses to guess about.
    pub reservation: Option<String>,
    /// Boot-instance identity of the claimer (`''` on a legacy row).
    pub owner_generation: String,
    /// Unix-ms deadline after which the claim is stale (`0` on a legacy row).
    pub lease_deadline_ms: i64,
    pub created_ms: i64,
}

impl PendingAdmission {
    /// `true` when this row is a LIVE claim of `generation` whose lease has
    /// not expired at `now`; anything else is stale and recovery-resolvable.
    pub fn is_live_for(&self, generation: &str, now: i64) -> bool {
        self.owner_generation == generation && self.lease_deadline_ms > now
    }
}

/// One bounded page of pending admission rows (keyset cursor = last key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAdmissionPage {
    pub rows: Vec<PendingAdmission>,
    pub has_more: bool,
}

/// The recovery landing of ONE stale pending admission (audit P1). Exactly
/// one is chosen; a row is never left pending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionResolution {
    /// The accepted durable fact exists: complete the row with the
    /// reconstructed byte-exact receipt.
    Complete(String),
    /// No durable mutation of this admission exists: delete the pending row
    /// so the key is retryable (a retry may execute once).
    Reclaim,
    /// The row cannot be trusted (legacy/unlinkable or a mismatched digest):
    /// land the typed key-reuse conflict so a retry is a 409, never a
    /// possible double execution.
    KeyReuse,
}

/// The typed outcome of one submission-keyed admission claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskAdmissionClaim {
    /// No row existed; one `pending` row was inserted for this caller under
    /// the caller's reservation.
    Fresh,
    /// The SAME session + key + request digest already completed: the exact
    /// stored run receipt JSON (byte-for-byte what the first success stored).
    Complete(String),
    /// The same key is still a LIVE claim of THIS boot: another admission is
    /// in flight and a concurrent duplicate must retry, never execute a
    /// second run.
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

impl TaskAdmissionClaim {
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
    if key.is_empty() || key.len() > MAX_TASK_ADMISSION_KEY_BYTES {
        return Err(StoreError::Oversized(format!(
            "task admission key must be 1..={MAX_TASK_ADMISSION_KEY_BYTES} bytes"
        )));
    }
    Ok(())
}

fn check_digest(request_digest: &str) -> StoreResult<()> {
    if request_digest.is_empty() || request_digest.len() > MAX_TASK_ADMISSION_DIGEST_BYTES {
        return Err(StoreError::Oversized(format!(
            "task admission request digest must be 1..={MAX_TASK_ADMISSION_DIGEST_BYTES} bytes"
        )));
    }
    Ok(())
}

fn check_reservation(reservation: &str) -> StoreResult<()> {
    if reservation.is_empty() || reservation.len() > MAX_TASK_ADMISSION_RESERVATION_BYTES {
        return Err(StoreError::Oversized(format!(
            "task admission reservation must be 1..={MAX_TASK_ADMISSION_RESERVATION_BYTES} bytes"
        )));
    }
    Ok(())
}

fn check_receipt(receipt_json: &str) -> StoreResult<()> {
    if receipt_json.len() > MAX_TASK_ADMISSION_RECEIPT_BYTES {
        return Err(StoreError::Oversized(format!(
            "task admission receipt exceeds {MAX_TASK_ADMISSION_RECEIPT_BYTES} bytes"
        )));
    }
    Ok(())
}

impl Store {
    /// Claim the durable admission of ONE submission-keyed task start. ONE
    /// writer job in an IMMEDIATE transaction; every value is prepared on
    /// the caller's thread. Absent -> INSERT `pending` owned by THIS boot
    /// with `reservation` and a fresh lease; present `complete` with the
    /// equal digest -> [`Complete`] with the stored receipt; present
    /// `complete` with a different digest (or a foreign session, or a
    /// recovery `conflict`) -> [`KeyReused`]; present `pending` -> `InFlight`
    /// when it is a live claim of this boot, otherwise [`Stale`] for the
    /// recovery pass (never a perpetual in-flight answer).
    ///
    /// [`Complete`]: TaskAdmissionClaim::Complete
    /// [`KeyReused`]: TaskAdmissionClaim::KeyReused
    /// [`Stale`]: TaskAdmissionClaim::Stale
    pub fn task_admission_claim(
        &self,
        session_id: SessionId,
        key: &str,
        request_digest: &str,
        reservation: &str,
        now: i64,
    ) -> StoreResult<TaskAdmissionClaim> {
        check_key(key)?;
        check_digest(request_digest)?;
        check_reservation(reservation)?;
        let key = key.to_owned();
        let request_digest = request_digest.to_owned();
        let reservation = reservation.to_owned();
        let generation = self.generation.clone();
        let session_raw = session_id.raw() as i64;
        let lease_deadline_ms = now.saturating_add(ADMISSION_LEASE_MS);
        let seam = Arc::clone(&self.seam);
        let corrupt_state = "task_admission row carries an unknown state".to_owned();
        let corrupt_receipt = "task_admission complete row carries no receipt".to_owned();
        let oversized_receipt =
            format!("task_admission receipt exceeds {MAX_TASK_ADMISSION_RECEIPT_BYTES} bytes");
        self.writer.execute("task_admission_claim", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let row: Option<AdmissionRowTuple> = tx
                .query_row(
                    "SELECT session_id, request_digest, state,
                            CASE WHEN receipt_json IS NULL THEN NULL
                                 ELSE length(CAST(receipt_json AS BLOB)) END,
                            owner_generation, lease_deadline_ms, reservation, created_ms
                     FROM task_admission WHERE key = ?1",
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
                    "INSERT INTO task_admission(
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
                seam.trip("task_admission_claim_precommit");
                tx.commit()?;
                seam.trip("task_admission_claim_committed");
                return Ok(TaskAdmissionClaim::Fresh);
            };
            if stored_session != session_raw {
                tx.commit()?;
                return Ok(TaskAdmissionClaim::KeyReused { stored_digest });
            }
            let outcome = match state.as_str() {
                "pending" => {
                    // DIGEST FIRST (idempotency contract): a key reused with
                    // different bytes is the typed KeyReused regardless of
                    // lease state; only a byte-identical claim may answer
                    // InFlight (live) or Stale (recovery-reclaimable).
                    if stored_digest != request_digest {
                        TaskAdmissionClaim::KeyReused { stored_digest }
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
                            TaskAdmissionClaim::InFlight
                        } else {
                            TaskAdmissionClaim::Stale(stale)
                        }
                    }
                }
                "complete" => {
                    if stored_digest != request_digest {
                        TaskAdmissionClaim::KeyReused { stored_digest }
                    } else {
                        let Some(receipt_bytes) = receipt_bytes else {
                            return Err(StoreError::Corrupt(vec![corrupt_receipt]));
                        };
                        if receipt_bytes > MAX_TASK_ADMISSION_RECEIPT_BYTES as i64 {
                            return Err(StoreError::Oversized(oversized_receipt));
                        }
                        let receipt_json: String = tx.query_row(
                            "SELECT receipt_json FROM task_admission WHERE key = ?1",
                            params![key.as_str()],
                            |r| r.get(0),
                        )?;
                        TaskAdmissionClaim::Complete(receipt_json)
                    }
                }
                "conflict" => TaskAdmissionClaim::KeyReused { stored_digest },
                _ => return Err(StoreError::Corrupt(vec![corrupt_state])),
            };
            seam.trip("task_admission_claim_precommit");
            tx.commit()?;
            seam.trip("task_admission_claim_committed");
            Ok(outcome)
        })
    }

    /// READ-ONLY status of one submission key, made for the executor's
    /// pre-run short circuit: `None` when no row exists; otherwise the typed
    /// outcome a [`Self::task_admission_claim`] with the same
    /// session/digest would answer — `Complete` with the stored receipt,
    /// `InFlight` for a live claim, `Stale` for a claim recovery must
    /// resolve, or `KeyReused` — WITHOUT inserting or mutating anything. A
    /// row released between the length probe and the receipt read reads as
    /// absent.
    pub fn task_admission_peek(
        &self,
        session_id: SessionId,
        key: &str,
        request_digest: &str,
        now: i64,
    ) -> StoreResult<Option<TaskAdmissionClaim>> {
        check_key(key)?;
        check_digest(request_digest)?;
        let session_raw = session_id.raw() as i64;
        let corrupt_state = "task_admission row carries an unknown state".to_owned();
        let corrupt_receipt = "task_admission complete row carries no receipt".to_owned();
        let oversized_receipt =
            format!("task_admission receipt exceeds {MAX_TASK_ADMISSION_RECEIPT_BYTES} bytes");
        let conn = self.read()?;
        let row: Option<AdmissionRowTuple> = conn
            .query_row(
                "SELECT session_id, request_digest, state,
                        CASE WHEN receipt_json IS NULL THEN NULL
                             ELSE length(CAST(receipt_json AS BLOB)) END,
                        owner_generation, lease_deadline_ms, reservation, created_ms
                 FROM task_admission WHERE key = ?1",
                params![key],
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
            return Ok(None);
        };
        if stored_session != session_raw {
            return Ok(Some(TaskAdmissionClaim::KeyReused { stored_digest }));
        }
        match state.as_str() {
            "pending" => {
                // Same digest-first contract as `task_admission_claim`.
                if stored_digest != request_digest {
                    return Ok(Some(TaskAdmissionClaim::KeyReused { stored_digest }));
                }
                let stale = PendingAdmission {
                    key: key.to_owned(),
                    session_id,
                    request_digest: stored_digest,
                    reservation: stored_reservation,
                    owner_generation,
                    lease_deadline_ms: lease_deadline,
                    created_ms,
                };
                if stale.is_live_for(self.generation(), now) {
                    Ok(Some(TaskAdmissionClaim::InFlight))
                } else {
                    Ok(Some(TaskAdmissionClaim::Stale(stale)))
                }
            }
            "complete" => {
                if stored_digest != request_digest {
                    return Ok(Some(TaskAdmissionClaim::KeyReused { stored_digest }));
                }
                let Some(receipt_bytes) = receipt_bytes else {
                    return Err(StoreError::Corrupt(vec![corrupt_receipt]));
                };
                if receipt_bytes > MAX_TASK_ADMISSION_RECEIPT_BYTES as i64 {
                    return Err(StoreError::Oversized(oversized_receipt));
                }
                let receipt_json: Option<String> = conn
                    .query_row(
                        "SELECT receipt_json FROM task_admission WHERE key = ?1",
                        params![key],
                        |r| r.get(0),
                    )
                    .optional()?;
                match receipt_json {
                    Some(receipt_json) => Ok(Some(TaskAdmissionClaim::Complete(receipt_json))),
                    None => Ok(None),
                }
            }
            "conflict" => Ok(Some(TaskAdmissionClaim::KeyReused { stored_digest })),
            _ => Err(StoreError::Corrupt(vec![corrupt_state])),
        }
    }

    /// Complete a matching `pending` admission with the byte-exact run
    /// receipt JSON. The update is fenced by the claim's `reservation`: a
    /// missing row, a foreign-session row, a superseded/reclaimed claim and
    /// a row that is not pending are typed [`StoreError::Conflict`]s —
    /// completion is the acceptance marker, never a blind upsert.
    pub fn task_admission_complete(
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
        let missing = "task_admission_complete: no admission row for the key".to_owned();
        let foreign =
            "task_admission_complete: admission row belongs to another session".to_owned();
        let not_pending = "task_admission_complete: admission row is not pending".to_owned();
        let superseded =
            "task_admission_complete: admission row was reclaimed or superseded".to_owned();
        self.writer.execute("task_admission_complete", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let changed = tx.execute(
                "UPDATE task_admission SET state = 'complete', receipt_json = ?4
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
                seam.trip("task_admission_complete_precommit");
                tx.commit()?;
                seam.trip("task_admission_complete_committed");
                return Ok(());
            }
            let row: Option<(i64, String, Option<String>)> = tx
                .query_row(
                    "SELECT session_id, state, reservation FROM task_admission WHERE key = ?1",
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

    /// Release a STILL-PENDING admission this caller owns (a start that
    /// failed before acceptance), so a retry of the same key may execute.
    /// Fenced by `reservation`: a missing row, a completed row and a
    /// superseded/reclaimed claim are no-ops — a completed receipt is never
    /// deleted by a release racing its own acceptance, and a stale owner can
    /// never delete a superseding claim.
    pub fn task_admission_release(
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
        self.writer.execute("task_admission_release", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute(
                "DELETE FROM task_admission
                 WHERE key = ?1 AND session_id = ?2 AND state = 'pending'
                   AND reservation IS ?3",
                params![key.as_str(), session_raw, reservation.as_str()],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    /// One bounded keyset page of `pending` task admissions (recovery scan).
    /// Ordered by key; `after_key` is the exclusive cursor from the previous
    /// page.
    pub fn task_admission_pending_page(
        &self,
        after_key: Option<&str>,
        limit: u32,
    ) -> StoreResult<PendingAdmissionPage> {
        let limit = limit.clamp(1, MAX_PENDING_ADMISSION_PAGE) as i64;
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT key, session_id, request_digest, reservation, owner_generation,
                    lease_deadline_ms, created_ms
             FROM task_admission
             WHERE state = 'pending' AND (?1 IS NULL OR key > ?1)
             ORDER BY key
             LIMIT ?2",
        )?;
        let probe = limit + 1;
        let rows = stmt.query_map(params![after_key, probe], pending_admission_row)?;
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

    /// Resolve ONE stale pending task admission exactly once (audit P1).
    /// Fenced by `expected_reservation` so a resolution based on an older
    /// read can never stomp a superseding claim. Returns `true` when the
    /// resolution landed on the row it named; `false` when the row moved on
    /// (the caller re-reads; nothing is guessed). A row is never left
    /// pending by a resolution that returned `true`.
    pub fn task_admission_resolve(
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
        self.writer.execute("task_admission_resolve", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let changed = match resolution {
                AdmissionResolution::Complete(receipt_json) => tx.execute(
                    "UPDATE task_admission SET state = 'complete', receipt_json = ?3
                     WHERE key = ?1 AND state = 'pending' AND reservation IS ?2",
                    params![key.as_str(), expected_reservation.as_deref(), receipt_json],
                )?,
                AdmissionResolution::Reclaim => tx.execute(
                    "DELETE FROM task_admission
                     WHERE key = ?1 AND state = 'pending' AND reservation IS ?2",
                    params![key.as_str(), expected_reservation.as_deref()],
                )?,
                AdmissionResolution::KeyReuse => tx.execute(
                    "UPDATE task_admission SET state = 'conflict', receipt_json = NULL
                     WHERE key = ?1 AND state = 'pending' AND reservation IS ?2",
                    params![key.as_str(), expected_reservation.as_deref()],
                )?,
            };
            tx.commit()?;
            Ok(changed == 1)
        })
    }
}

/// Decode one pending-admission row (typed corrupt on impossible ids).
pub(crate) fn pending_admission_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<PendingAdmission> {
    let session_raw: i64 = r.get(1)?;
    let session = u64::try_from(session_raw)
        .ok()
        .filter(|v| *v > 0)
        .and_then(|v| SessionId::try_from(v).ok());
    let key: String = r.get(0)?;
    match session {
        Some(session_id) => Ok(PendingAdmission {
            key,
            session_id,
            request_digest: r.get(2)?,
            reservation: r.get(3)?,
            owner_generation: r.get(4)?,
            lease_deadline_ms: r.get(5)?,
            created_ms: r.get(6)?,
        }),
        None => Err(rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Integer,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("pending admission {key:?} carries an impossible session id {session_raw}"),
            )),
        )),
    }
}

#[cfg(test)]
mod task_admission_tests {
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

    fn raw_count(store: &Store, table: &str) -> i64 {
        store
            .read()
            .unwrap()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    fn raw_state(store: &Store, table: &str, key: &str) -> (String, Option<String>) {
        store
            .read()
            .unwrap()
            .query_row(
                &format!("SELECT state, receipt_json FROM {table} WHERE key = ?1"),
                params![key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }

    #[test]
    fn claim_fresh_then_complete_replays_the_exact_receipt_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        assert_eq!(
            store
                .task_admission_claim(sid, KEY, DIGEST, RES, 1)
                .unwrap(),
            TaskAdmissionClaim::Fresh
        );
        // A pending row refuses the concurrent duplicate.
        assert_eq!(
            store
                .task_admission_claim(sid, KEY, DIGEST, RES, 2)
                .unwrap(),
            TaskAdmissionClaim::InFlight
        );
        let receipt =
            r#"{"run_id":"tx-0000000000000007","mode":"in_session","op_id":7,"queued":false}"#;
        store
            .task_admission_complete(sid, KEY, RES, receipt)
            .unwrap();
        // The replay returns the stored bytes EXACTLY and mutates nothing.
        assert_eq!(
            store
                .task_admission_claim(sid, KEY, DIGEST, "tx-0000000000000002", 3)
                .unwrap(),
            TaskAdmissionClaim::Complete(receipt.to_string())
        );
        assert_eq!(
            store
                .task_admission_claim(sid, KEY, DIGEST, "tx-0000000000000003", 4)
                .unwrap(),
            TaskAdmissionClaim::Complete(receipt.to_string())
        );
    }

    #[test]
    fn same_key_with_a_different_digest_is_key_reused_in_every_pending_state() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        store
            .task_admission_claim(sid, KEY, DIGEST, RES, 1)
            .unwrap();
        let reused = TaskAdmissionClaim::KeyReused {
            stored_digest: DIGEST.to_string(),
        };
        // Pending (live) + different bytes: KeyReused, never InFlight.
        assert_eq!(
            store
                .task_admission_claim(sid, KEY, OTHER_DIGEST, "tx-0000000000000002", 2)
                .unwrap(),
            reused
        );
        // Pending (expired lease) + different bytes: still KeyReused.
        assert_eq!(
            store
                .task_admission_claim(
                    sid,
                    KEY,
                    OTHER_DIGEST,
                    "tx-0000000000000003",
                    ADMISSION_LEASE_MS
                )
                .unwrap(),
            reused
        );
        // Identical bytes remain InFlight while live.
        assert_eq!(
            store
                .task_admission_claim(sid, KEY, DIGEST, "tx-0000000000000004", 2)
                .unwrap(),
            TaskAdmissionClaim::InFlight
        );
        store.task_admission_complete(sid, KEY, RES, "{}").unwrap();
        let claim = store
            .task_admission_claim(sid, KEY, OTHER_DIGEST, "tx-0000000000000005", 3)
            .unwrap();
        assert_eq!(
            claim,
            TaskAdmissionClaim::KeyReused {
                stored_digest: DIGEST.to_string()
            },
            "a completed key with a different digest names the stored digest"
        );
    }

    #[test]
    fn release_only_deletes_a_pending_row_and_a_retry_reclaims_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        store
            .task_admission_claim(sid, KEY, DIGEST, RES, 1)
            .unwrap();
        // Releasing a missing/unknown key is a no-op.
        store
            .task_admission_release(sid, "00000000-0000-0000-0000-000000000000", RES)
            .unwrap();
        // A foreign reservation can never delete the row.
        store
            .task_admission_release(sid, KEY, "tx-00000000000000ff")
            .unwrap();
        assert!(matches!(
            store
                .task_admission_claim(sid, KEY, DIGEST, RES, 2)
                .unwrap(),
            TaskAdmissionClaim::InFlight
        ));
        store.task_admission_release(sid, KEY, RES).unwrap();
        assert_eq!(
            store
                .task_admission_claim(sid, KEY, DIGEST, "tx-0000000000000002", 2)
                .unwrap(),
            TaskAdmissionClaim::Fresh,
            "a released pre-acceptance claim may re-execute"
        );
        store
            .task_admission_complete(sid, KEY, "tx-0000000000000002", "receipt")
            .unwrap();
        // Release NEVER deletes a completed receipt.
        store.task_admission_release(sid, KEY, RES).unwrap();
        assert_eq!(
            store
                .task_admission_claim(sid, KEY, DIGEST, "tx-0000000000000003", 3)
                .unwrap(),
            TaskAdmissionClaim::Complete("receipt".to_string())
        );
    }

    #[test]
    fn completion_refuses_missing_completed_foreign_and_superseded_rows_typed() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        match store.task_admission_complete(sid, KEY, RES, "r") {
            Err(StoreError::Conflict(m)) => assert!(m.contains("no admission row"), "{m}"),
            other => panic!("missing row must be a typed conflict: {other:?}"),
        }
        store
            .task_admission_claim(sid, KEY, DIGEST, RES, 1)
            .unwrap();
        store.task_admission_complete(sid, KEY, RES, "r").unwrap();
        match store.task_admission_complete(sid, KEY, RES, "r2") {
            Err(StoreError::Conflict(m)) => assert!(m.contains("not pending"), "{m}"),
            other => panic!("double completion must be a typed conflict: {other:?}"),
        }
        // A superseding claim fences the stale owner out.
        let key2 = "22222222-2222-4333-8444-555555555555";
        let fresh_res = "tx-0000000000000009";
        store
            .task_admission_claim(sid, key2, DIGEST, RES, 1)
            .unwrap();
        assert!(store
            .task_admission_resolve(key2, Some(RES), AdmissionResolution::Reclaim)
            .unwrap());
        store
            .task_admission_claim(sid, key2, DIGEST, fresh_res, 2)
            .unwrap();
        match store.task_admission_complete(sid, key2, RES, "r3") {
            Err(StoreError::Conflict(m)) => assert!(m.contains("superseded"), "{m}"),
            other => panic!("a stale reservation must be fenced typed: {other:?}"),
        }
        // A foreign session can neither complete nor release the row.
        let ws2 = store.create_workspace("/w2").unwrap();
        let other_sid = store.create_session(ws2, "other", "p", "m").unwrap().id;
        match store.task_admission_complete(other_sid, key2, fresh_res, "r3") {
            Err(StoreError::Conflict(m)) => assert!(m.contains("another session"), "{m}"),
            other => panic!("foreign completion must be a typed conflict: {other:?}"),
        }
        // The foreign-session claim names the key as reused.
        assert_eq!(
            store
                .task_admission_claim(other_sid, key2, DIGEST, "tx-000000000000000a", 4)
                .unwrap(),
            TaskAdmissionClaim::KeyReused {
                stored_digest: DIGEST.to_string()
            }
        );
    }

    #[test]
    fn oversized_keys_digests_reservations_and_receipts_are_typed_and_write_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        let long_key = "a".repeat(MAX_TASK_ADMISSION_KEY_BYTES + 1);
        assert!(matches!(
            store.task_admission_claim(sid, &long_key, DIGEST, RES, 1),
            Err(StoreError::Oversized(_))
        ));
        assert!(matches!(
            store.task_admission_claim(sid, "", DIGEST, RES, 1),
            Err(StoreError::Oversized(_))
        ));
        let long_digest = "a".repeat(MAX_TASK_ADMISSION_DIGEST_BYTES + 1);
        assert!(matches!(
            store.task_admission_claim(sid, KEY, &long_digest, RES, 1),
            Err(StoreError::Oversized(_))
        ));
        let long_res = "r".repeat(MAX_TASK_ADMISSION_RESERVATION_BYTES + 1);
        assert!(matches!(
            store.task_admission_claim(sid, KEY, DIGEST, &long_res, 1),
            Err(StoreError::Oversized(_))
        ));
        assert!(matches!(
            store.task_admission_claim(sid, KEY, DIGEST, "", 1),
            Err(StoreError::Oversized(_))
        ));
        store
            .task_admission_claim(sid, KEY, DIGEST, RES, 1)
            .unwrap();
        let huge_receipt = "x".repeat(MAX_TASK_ADMISSION_RECEIPT_BYTES + 1);
        assert!(matches!(
            store.task_admission_complete(sid, KEY, RES, &huge_receipt),
            Err(StoreError::Oversized(_))
        ));
        // The failed oversized writes left the pending row untouched.
        assert_eq!(
            store
                .task_admission_claim(sid, KEY, DIGEST, RES, 2)
                .unwrap(),
            TaskAdmissionClaim::InFlight
        );
        assert_eq!(raw_count(&store, "task_admission"), 1);
    }

    #[test]
    fn corrupt_injected_rows_are_loud_never_guessed() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        // Simulate a hostile/tampered database whose table lacks the fresh
        // CHECK vocabulary (a hand-rolled table with the modern columns):
        // every read must refuse typed, never guess.
        store
            .read()
            .unwrap()
            .execute_batch(
                "DROP TABLE task_admission;
                 CREATE TABLE task_admission (
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
                "INSERT INTO task_admission(
                    key, session_id, request_digest, state, receipt_json, created_ms,
                    owner_generation, lease_deadline_ms, reservation)
                 VALUES (?1, ?2, ?3, 'bogus', NULL, 0, 'g', 1, 'r')",
                params![KEY, sid.raw() as i64, DIGEST],
            )
            .unwrap();
        match store.task_admission_claim(sid, KEY, DIGEST, RES, 2) {
            Err(StoreError::Corrupt(_)) => {}
            other => panic!("unknown state must be loud corruption: {other:?}"),
        }
        // A complete row without a receipt is corruption, never an empty
        // receipt replay.
        store
            .read()
            .unwrap()
            .execute(
                "UPDATE task_admission SET state = 'complete', receipt_json = NULL WHERE key = ?1",
                params![KEY],
            )
            .unwrap();
        match store.task_admission_claim(sid, KEY, DIGEST, RES, 3) {
            Err(StoreError::Corrupt(_)) => {}
            other => panic!("a receipt-less complete row must be corruption: {other:?}"),
        }
        // An oversized injected receipt refuses typed on read.
        let huge = "x".repeat(MAX_TASK_ADMISSION_RECEIPT_BYTES + 1);
        store
            .read()
            .unwrap()
            .execute(
                "UPDATE task_admission SET state = 'complete', receipt_json = ?2 WHERE key = ?1",
                params![KEY, huge],
            )
            .unwrap();
        match store.task_admission_claim(sid, KEY, DIGEST, RES, 4) {
            Err(StoreError::Oversized(_)) => {}
            other => panic!("an oversized receipt must refuse typed: {other:?}"),
        }
    }

    #[test]
    fn peek_reports_the_claim_outcome_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        // Absent: nothing to replay and nothing written.
        assert_eq!(
            store.task_admission_peek(sid, KEY, DIGEST, 0).unwrap(),
            None
        );
        assert_eq!(
            raw_count(&store, "task_admission"),
            0,
            "a peek never inserts"
        );
        store
            .task_admission_claim(sid, KEY, DIGEST, RES, 1)
            .unwrap();
        assert_eq!(
            store.task_admission_peek(sid, KEY, DIGEST, 2).unwrap(),
            Some(TaskAdmissionClaim::InFlight)
        );
        store
            .task_admission_complete(sid, KEY, RES, "receipt")
            .unwrap();
        assert_eq!(
            store.task_admission_peek(sid, KEY, DIGEST, 3).unwrap(),
            Some(TaskAdmissionClaim::Complete("receipt".to_string()))
        );
        assert_eq!(
            store
                .task_admission_peek(sid, KEY, OTHER_DIGEST, 3)
                .unwrap(),
            Some(TaskAdmissionClaim::KeyReused {
                stored_digest: DIGEST.to_string()
            })
        );
        let ws2 = store.create_workspace("/w2").unwrap();
        let other_sid = store.create_session(ws2, "other", "p", "m").unwrap().id;
        assert_eq!(
            store
                .task_admission_peek(other_sid, KEY, DIGEST, 3)
                .unwrap(),
            Some(TaskAdmissionClaim::KeyReused {
                stored_digest: DIGEST.to_string()
            })
        );
    }

    #[test]
    fn pending_key_digest_mismatch_is_key_reused_not_in_flight_or_stale() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        store
            .task_admission_claim(sid, KEY, DIGEST, RES, 0)
            .unwrap();
        let reused = TaskAdmissionClaim::KeyReused {
            stored_digest: DIGEST.to_string(),
        };
        // Live + different bytes: KeyReused on BOTH entry points.
        assert_eq!(
            store
                .task_admission_claim(sid, KEY, OTHER_DIGEST, "tx-0000000000000009", 1)
                .unwrap(),
            reused
        );
        assert_eq!(
            store
                .task_admission_peek(sid, KEY, OTHER_DIGEST, 1)
                .unwrap(),
            Some(reused.clone())
        );
        // Live + identical: InFlight on both.
        assert_eq!(
            store
                .task_admission_claim(sid, KEY, DIGEST, "tx-000000000000000a", 1)
                .unwrap(),
            TaskAdmissionClaim::InFlight
        );
        assert_eq!(
            store.task_admission_peek(sid, KEY, DIGEST, 1).unwrap(),
            Some(TaskAdmissionClaim::InFlight)
        );
        // Stale (lease deadline reached) + identical: Stale on both.
        let expired = ADMISSION_LEASE_MS;
        assert!(matches!(
            store
                .task_admission_claim(sid, KEY, DIGEST, "tx-000000000000000b", expired)
                .unwrap(),
            TaskAdmissionClaim::Stale(_)
        ));
        assert!(matches!(
            store
                .task_admission_peek(sid, KEY, DIGEST, expired)
                .unwrap(),
            Some(TaskAdmissionClaim::Stale(_))
        ));
        // Stale + different bytes: still KeyReused, never Stale.
        assert_eq!(
            store
                .task_admission_claim(sid, KEY, OTHER_DIGEST, "tx-000000000000000c", expired)
                .unwrap(),
            reused
        );
        assert_eq!(
            store
                .task_admission_peek(sid, KEY, OTHER_DIGEST, expired)
                .unwrap(),
            Some(reused.clone())
        );
        // Complete + same: byte-exact replay; + different: KeyReused.
        store
            .task_admission_complete(sid, KEY, RES, "receipt")
            .unwrap();
        assert_eq!(
            store
                .task_admission_claim(sid, KEY, DIGEST, "tx-000000000000000d", expired + 1)
                .unwrap(),
            TaskAdmissionClaim::Complete("receipt".to_string())
        );
        assert_eq!(
            store
                .task_admission_claim(sid, KEY, OTHER_DIGEST, "tx-000000000000000e", expired + 1)
                .unwrap(),
            reused
        );
    }

    #[test]
    fn concurrent_claimers_see_exactly_one_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        let store = std::sync::Arc::new(store);
        let mut handles = Vec::new();
        for i in 0..8 {
            let store = store.clone();
            let res = format!("tx-{i:016x}");
            handles.push(std::thread::spawn(move || {
                store
                    .task_admission_claim(sid, KEY, DIGEST, &res, i)
                    .unwrap()
            }));
        }
        let claims: Vec<TaskAdmissionClaim> =
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
            .task_admission_complete(sid, KEY, &winner_res, "receipt")
            .unwrap();
        for claim in [
            store
                .task_admission_claim(sid, KEY, DIGEST, "tx-00000000000000f0", 9)
                .unwrap(),
            store
                .task_admission_claim(sid, KEY, DIGEST, "tx-00000000000000f1", 10)
                .unwrap(),
        ] {
            assert_eq!(claim, TaskAdmissionClaim::Complete("receipt".to_string()));
        }
    }

    #[test]
    fn session_admissions_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        let ws2 = store.create_workspace("/w2").unwrap();
        let sid2 = store.create_session(ws2, "t2", "p", "m").unwrap().id;
        assert_eq!(
            store
                .task_admission_claim(sid, KEY, DIGEST, RES, 1)
                .unwrap(),
            TaskAdmissionClaim::Fresh
        );
        assert_eq!(
            store
                .task_admission_claim(sid2, "other-key", DIGEST, "tx-0000000000000021", 1)
                .unwrap(),
            TaskAdmissionClaim::Fresh
        );
        // The SAME key on a second session is a typed KeyReused (a key never
        // aliases two sessions' logical starts).
        assert_eq!(
            store
                .task_admission_claim(sid2, KEY, DIGEST, "tx-0000000000000022", 1)
                .unwrap(),
            TaskAdmissionClaim::KeyReused {
                stored_digest: DIGEST.to_string()
            }
        );
    }

    #[test]
    fn a_previous_boot_pending_row_is_stale_reclaimable_and_never_perpetual_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        assert_eq!(
            store
                .task_admission_claim(sid, KEY, DIGEST, RES, 1)
                .unwrap(),
            TaskAdmissionClaim::Fresh
        );
        let previous_generation = store.generation().to_string();
        drop(store);
        // Reopening mints a NEW boot generation: the surviving pending row is
        // a dead claim, never InFlight to this boot.
        let store = Store::open(dir.path(), true).unwrap();
        assert_ne!(store.generation(), previous_generation);
        let claim = store
            .task_admission_claim(sid, KEY, DIGEST, "tx-0000000000000002", 2)
            .unwrap();
        let row = match claim {
            TaskAdmissionClaim::Stale(row) => row,
            other => panic!("a previous-generation pending row must be Stale: {other:?}"),
        };
        assert_eq!(row.reservation.as_deref(), Some(RES));
        assert_eq!(row.owner_generation, previous_generation);
        // The bounded recovery scan surfaces it; no durable fact exists, so
        // the safe landing is reclaim.
        let page = store.task_admission_pending_page(None, 10).unwrap();
        assert_eq!(page.rows, vec![row.clone()]);
        assert!(!page.has_more);
        assert!(store
            .task_admission_resolve(
                KEY,
                row.reservation.as_deref(),
                AdmissionResolution::Reclaim
            )
            .unwrap());
        assert_eq!(
            store
                .task_admission_claim(sid, KEY, DIGEST, "tx-0000000000000002", 3)
                .unwrap(),
            TaskAdmissionClaim::Fresh,
            "a reclaimed key retries as a single fresh execution"
        );
    }

    #[test]
    fn an_expired_lease_is_stale_even_within_one_generation() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        store
            .task_admission_claim(sid, KEY, DIGEST, RES, 0)
            .unwrap();
        // Just inside the lease: live.
        assert_eq!(
            store
                .task_admission_claim(sid, KEY, DIGEST, RES, ADMISSION_LEASE_MS - 1)
                .unwrap(),
            TaskAdmissionClaim::InFlight
        );
        // At/after the deadline: stale, never InFlight forever.
        let claim = store
            .task_admission_claim(sid, KEY, DIGEST, RES, ADMISSION_LEASE_MS)
            .unwrap();
        assert!(matches!(claim, TaskAdmissionClaim::Stale(_)), "{claim:?}");
    }

    #[test]
    fn recovery_completion_lands_the_byte_exact_receipt_and_replays() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        store
            .task_admission_claim(sid, KEY, DIGEST, RES, 1)
            .unwrap();
        let receipt =
            r#"{"run_id":"tx-0000000000000007","mode":"in_session","op_id":7,"queued":false}"#;
        assert!(store
            .task_admission_resolve(
                KEY,
                Some(RES),
                AdmissionResolution::Complete(receipt.to_string())
            )
            .unwrap());
        assert_eq!(
            store
                .task_admission_claim(sid, KEY, DIGEST, "tx-0000000000000002", 2)
                .unwrap(),
            TaskAdmissionClaim::Complete(receipt.to_string())
        );
        // A mismatched-digest retry is the typed conflict.
        assert_eq!(
            store
                .task_admission_claim(sid, KEY, OTHER_DIGEST, "tx-0000000000000003", 3)
                .unwrap(),
            TaskAdmissionClaim::KeyReused {
                stored_digest: DIGEST.to_string()
            }
        );
    }

    #[test]
    fn recovery_key_reuse_lands_conflict_and_never_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        // A legacy (pre-v30) pending row has no reservation: recovery never
        // guesses and lands the typed conflict.
        store
            .read()
            .unwrap()
            .execute(
                "INSERT INTO task_admission(
                    key, session_id, request_digest, state, receipt_json, created_ms,
                    owner_generation, lease_deadline_ms, reservation)
                 VALUES (?1, ?2, ?3, 'pending', NULL, 0, '', 0, NULL)",
                params![KEY, sid.raw() as i64, DIGEST],
            )
            .unwrap();
        let page = store.task_admission_pending_page(None, 10).unwrap();
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].reservation, None);
        assert!(store
            .task_admission_resolve(KEY, None, AdmissionResolution::KeyReuse)
            .unwrap());
        assert_eq!(
            store
                .task_admission_claim(sid, KEY, DIGEST, "tx-0000000000000002", 1)
                .unwrap(),
            TaskAdmissionClaim::KeyReused {
                stored_digest: DIGEST.to_string()
            },
            "an unlinkable legacy claim is a typed conflict, never a fresh run"
        );
        assert_eq!(raw_state(&store, "task_admission", KEY).0, "conflict");
    }

    #[test]
    fn resolve_is_fenced_by_the_reservation_it_read() {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = store_with_session(&dir);
        store
            .task_admission_claim(sid, KEY, DIGEST, RES, 1)
            .unwrap();
        // A resolution computed from an older read can never stomp a
        // superseding claim.
        assert!(!store
            .task_admission_resolve(
                KEY,
                Some("tx-00000000000000ff"),
                AdmissionResolution::Reclaim
            )
            .unwrap());
        assert_eq!(
            store
                .task_admission_claim(sid, KEY, DIGEST, RES, 2)
                .unwrap(),
            TaskAdmissionClaim::InFlight,
            "the original claim survived a stale resolution"
        );
    }

    #[test]
    fn fresh_database_carries_the_admission_table_and_head() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let conn = store.raw_conn();
        let present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='task_admission'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(present, 1, "fresh schema must carry task_admission");
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            version, 31,
            "v30 (admission ownership) is the migration head"
        );
        // The CHECK vocabulary refuses an unknown state at the SQL level and
        // admits the typed conflict landing.
        let pid = SessionId::new(1);
        conn.execute(
            "INSERT INTO task_admission(key, session_id, request_digest, state, receipt_json, created_ms)
             VALUES ('k', ?1, 'd', 'bogus', NULL, 0)",
            params![pid.raw() as i64],
        )
        .unwrap_err();
        conn.execute(
            "INSERT INTO task_admission(key, session_id, request_digest, state, receipt_json, created_ms)
             VALUES ('k', ?1, 'd', 'conflict', NULL, 0)",
            params![pid.raw() as i64],
        )
        .unwrap();
    }
}
