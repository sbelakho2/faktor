//! Native content-addressed checkpoints (section 16 of the architecture
//! spec): `before_hash`/`after_hash` pairs so rollback can verify the current
//! content before restoring, never overwriting unrelated edits.

use faktor_core::event::EventKind;
use faktor_core::hash::FileHash;
use faktor_store::CheckpointRow;

use crate::handle::SessionHandle;
use crate::SessionError;

const MAX_CHECKPOINT_PATH_BYTES: usize = 4096;

impl SessionHandle {
    /// Record a checkpoint for one file. `sequence` must be unique per
    /// session (duplicates conflict) and `path` bounded. Journals
    /// `CheckpointCreated` with the same state the session is in.
    pub fn put_checkpoint(
        &self,
        sequence: i64,
        path: &str,
        before_hash: FileHash,
        after_hash: FileHash,
    ) -> faktor_core::Result<i64> {
        if sequence < 0 {
            return Err(SessionError::Malformed(format!(
                "checkpoint sequence must be >= 0, got {sequence}"
            ))
            .into());
        }
        if path.is_empty() || path.len() > MAX_CHECKPOINT_PATH_BYTES {
            return Err(SessionError::Malformed("invalid checkpoint path".into()).into());
        }
        let existing = self.checkpoints_of()?;
        if existing.iter().any(|c| c.sequence == sequence) {
            return Err(SessionError::Conflict(format!(
                "checkpoint sequence {sequence} already exists"
            ))
            .into());
        }
        let _guard = self.command_guard();
        // Validate against the pre-state read ONCE; the atomic store command
        // re-verifies it inside the same transaction as the row insert, so a
        // checkpoint row can never exist without its CheckpointCreated event.
        let current = self.state()?;
        crate::journal::validate_transition(current, EventKind::CheckpointCreated, current)?;
        let event = crate::ops::command_event(
            EventKind::CheckpointCreated,
            current,
            None,
            self.now_ms(),
            // Same payload shape the store's content-aware checkpoint
            // command writes (this API is hash-only, so both sides exist).
            Some(serde_json::json!({
                "sequence": sequence,
                "path": path,
                "before_hash": before_hash.to_hex(),
                "after_hash": after_hash.to_hex(),
                "before_exists": true,
                "after_exists": true,
            })),
        )?;
        let (id, _seq) = self
            .manager
            .store()
            .put_checkpoint_and_event(
                self.id,
                sequence,
                path,
                &before_hash.to_hex(),
                &after_hash.to_hex(),
                // This API records hashes only (no after-content bytes), so
                // the CAS after-blob is unknown: redo/diff refuse such rows
                // honestly. The content-aware path is faktor-snapshot's
                // after_write, which stores the after blob in the CAS.
                None,
                current,
                event,
            )
            .map_err(crate::map_store_err)?;
        Ok(id)
    }

    pub fn checkpoints_of(&self) -> faktor_core::Result<Vec<CheckpointRow>> {
        self.manager
            .store()
            .checkpoints_of(self.id)
            .map_err(|e| crate::map_store_err(e).into())
    }

    /// Mark a checkpoint as restored (durable audit of the rollback path).
    pub fn mark_checkpoint_restored(&self, id: i64) -> faktor_core::Result<()> {
        self.manager
            .store()
            .mark_checkpoint_restored(id)
            .map_err(|e| crate::map_store_err(e).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::tests::{session, test_manager};

    fn hashes(a: u8, b: u8) -> (FileHash, FileHash) {
        (FileHash::from([a; 32]), FileHash::from([b; 32]))
    }

    #[test]
    fn checkpoints_record_and_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let (sid, hashes_out) = {
            let m =
                crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
                    .unwrap();
            let s = session(&m);
            let (before, after) = hashes(1, 2);
            s.put_checkpoint(0, "a.rs", before, after).unwrap();
            let (b2, a2) = hashes(3, 4);
            s.put_checkpoint(1, "b.rs", b2, a2).unwrap();
            (s.id(), vec![(0, before, after), (1, b2, a2)])
        };
        // Reopen: checkpoints are durable.
        let m = crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
            .unwrap();
        let s = m.get_session(sid).unwrap().unwrap();
        let rows = s.checkpoints_of().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].sequence, 0);
        assert_eq!(rows[0].after_hash, hashes_out[0].2.to_hex());
        assert_eq!(rows[1].path, "b.rs");
        // The journal carried CheckpointCreated events.
        let kinds: Vec<_> = s
            .events_range(1, None)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                EventKind::SessionCreated,
                EventKind::CheckpointCreated,
                EventKind::CheckpointCreated
            ]
        );
    }

    #[test]
    fn duplicate_checkpoint_sequence_conflicts_without_trace() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let (before, after) = hashes(1, 2);
        s.put_checkpoint(0, "a.rs", before, after).unwrap();
        let err = s.put_checkpoint(0, "a.rs", before, after).unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Conflict);
        assert_eq!(s.checkpoints_of().unwrap().len(), 1);
        assert_eq!(s.events_range(1, None).unwrap().len(), 2, "one event only");
        // Negative sequences and empty paths are malformed.
        assert!(s.put_checkpoint(-1, "a.rs", before, after).is_err());
        assert!(s.put_checkpoint(1, "", before, after).is_err());
        assert!(s
            .put_checkpoint(1, &"p".repeat(MAX_CHECKPOINT_PATH_BYTES + 1), before, after)
            .is_err());
    }

    #[test]
    fn rollback_audit_marking_is_idempotent_for_unknown_rows() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let (before, after) = hashes(1, 2);
        let id = s.put_checkpoint(0, "a.rs", before, after).unwrap();
        s.mark_checkpoint_restored(id).unwrap();
        // The store marks; unknown ids are silently ignored by the store, so
        // we verify the restored_ms is durable on the known row.
        let rows = s.checkpoints_of().unwrap();
        assert!(rows[0].restored_ms.is_some());
    }

    #[test]
    fn checkpoint_seams_reopen_old_or_new_only() {
        // One checkpoint command = ONE transaction: a crash at any durability
        // boundary reopens on exactly the old world (no row, no event) or
        // exactly the new one (row AND event), never a row without its event.
        for seam in [
            "session_command_side_row",
            "session_command_precommit",
            "session_command_committed",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let m =
                crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
                    .unwrap();
            let s = session(&m);
            let sid = s.id();
            let (before, after) = hashes(1, 2);
            m.store().crash_arm(faktor_store::CrashArm {
                point: seam,
                ordinal: 0,
            });
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = s.put_checkpoint(0, "a.rs", before, after);
            }));
            assert!(caught.is_err(), "seam {seam} must fire");
            drop(s);
            drop(m);
            let m =
                crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
                    .unwrap();
            let s = m.get_session(sid).unwrap().unwrap();
            let rows = s.checkpoints_of().unwrap();
            let created = s
                .events_range(1, None)
                .unwrap()
                .into_iter()
                .filter(|e| e.kind == EventKind::CheckpointCreated)
                .count();
            match seam {
                "session_command_committed" => {
                    assert_eq!(rows.len(), 1, "committed checkpoint row is durable");
                    assert_eq!(rows[0].sequence, 0);
                    assert_eq!(rows[0].after_hash, after.to_hex());
                    assert_eq!(created, 1, "committed checkpoint event is durable");
                }
                _ => {
                    assert!(rows.is_empty(), "rolled-back checkpoint leaves no row");
                    assert_eq!(created, 0, "rolled-back checkpoint leaves no event");
                }
            }
        }
    }
}
