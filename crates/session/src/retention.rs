//! The durable reference scanner behind the retention plane.
//!
//! [`LiveReferenceScanner`] walks the durable session store (ledger rows and
//! verification records) and collects the digests that MUST NOT be deleted
//! because recoverable state still references them:
//!
//! - an OPEN landing transaction ([`crate::ledger::IntegrationTxnRow`] in a
//!   non-terminal phase): its per-path rollback blobs, its base/candidate
//!   payload hashes and its snapshots;
//! - an OPEN durable edit transaction (`edit_txn_prepared` without a
//!   terminal row): its staged base content digests;
//! - a recorded run base: its snapshot and manifest digests;
//! - a durable verification record: its tree hash, every changed-file
//!   digest and every digest of its candidate-proof reference.
//!
//! The scan is explicitly bounded (sessions, ledger rows per session,
//! verification records, total references): exceeding a bound is a typed
//! [`RetentionScanError::BoundExceeded`] refusal, never a silent truncation.
//! Store read failures and corrupt rows are loud too
//! ([`RetentionScanError::Unavailable`] / [`RetentionScanError::Malformed`]):
//! the deletion side fails closed on all of them.
//!
//! [`LiveReferenceSet`] is the resulting guard; it implements the
//! `faktor-cas` [`RetentionGuard`], so the CAS deletion primitive enforces
//! the same invariant at the storage boundary.

use std::collections::BTreeMap;

use faktor_cas::retention::{
    ProtectedReference, ProtectionKind, RetentionGuard, RetentionScanError,
};
use faktor_core::hash::FileHash;
use faktor_fs::entry_state::EntryState;
use faktor_store::Store;

use crate::ledger::{decode_ledger_entry_row, LedgerPayload};
use crate::SessionError;

/// Hard scan bounds. Exceeding any of them refuses the whole scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanLimits {
    pub max_sessions: usize,
    pub max_ledger_rows_per_session: u64,
    pub max_verification_records: usize,
    pub max_references: usize,
}

/// The production scan bounds: bounded everything, sized for a daemon tree.
pub const DEFAULT_SCAN_LIMITS: ScanLimits = ScanLimits {
    max_sessions: 100_000,
    max_ledger_rows_per_session: 5_000_000,
    max_verification_records: 1_000_000,
    max_references: 5_000_000,
};

/// One ledger page size of the scan (paging is fundamental).
const LEDGER_PAGE: u64 = 1000;

/// The set of digests protected by recoverable durable state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveReferenceSet {
    by_digest: BTreeMap<String, ProtectedReference>,
}

impl LiveReferenceSet {
    pub fn len(&self) -> usize {
        self.by_digest.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_digest.is_empty()
    }

    /// The protection record of one digest, if any.
    pub fn reference(&self, digest_hex: &str) -> Option<&ProtectedReference> {
        self.by_digest.get(digest_hex)
    }

    /// Every protected digest, ascending (tests/diagnostics).
    pub fn digests(&self) -> impl Iterator<Item = &str> {
        self.by_digest.keys().map(String::as_str)
    }

    fn insert(&mut self, digest: &str, reference: ProtectedReference) -> bool {
        let Some(hex) = normalize_digest(digest) else {
            return false;
        };
        if self.by_digest.contains_key(&hex) {
            return false;
        }
        self.by_digest.insert(hex, reference);
        true
    }
}

impl RetentionGuard for LiveReferenceSet {
    fn protect(&self, digest_hex: &str) -> Result<Option<ProtectedReference>, RetentionScanError> {
        Ok(normalize_digest(digest_hex).and_then(|hex| self.by_digest.get(&hex).cloned()))
    }
}

/// Normalize one digest text to bare lowercase 64-hex (`None` for anything
/// that is not a canonical digest — such values cannot be blob addresses).
fn normalize_digest(value: &str) -> Option<String> {
    let hex = value.strip_prefix("blake3:").unwrap_or(value);
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    Some(hex.to_string())
}

/// Walks the durable store and builds the protected-digest set.
pub struct LiveReferenceScanner<'a> {
    store: &'a Store,
    limits: ScanLimits,
}

impl<'a> LiveReferenceScanner<'a> {
    pub fn new(store: &'a Store) -> Self {
        Self {
            store,
            limits: DEFAULT_SCAN_LIMITS,
        }
    }

    pub fn with_limits(store: &'a Store, limits: ScanLimits) -> Self {
        Self { store, limits }
    }

    /// One bounded, loud scan over every session's durable ledger and every
    /// verification record.
    pub fn scan(&self) -> Result<LiveReferenceSet, RetentionScanError> {
        let mut set = LiveReferenceSet::default();
        let sessions = self
            .store
            .session_ids()
            .map_err(|error| RetentionScanError::Unavailable(error.to_string()))?;
        if sessions.len() > self.limits.max_sessions {
            return Err(RetentionScanError::BoundExceeded(format!(
                "{} sessions exceed the scan bound {}",
                sessions.len(),
                self.limits.max_sessions
            )));
        }
        let mut references_seen = 0usize;
        let mut verification_records = 0usize;
        for session in sessions {
            let mut open_edits: BTreeMap<u64, (String, Vec<String>)> = BTreeMap::new();
            // Latest landing transaction per run: an earlier open phase is
            // superseded by the newest row (exactly the recovery read
            // contract), so a terminal row releases the rollback material.
            let mut latest_txns: BTreeMap<String, crate::ledger::IntegrationTxnRow> =
                BTreeMap::new();
            let mut cursor: Option<i64> = None;
            let mut rows_read: u64 = 0;
            loop {
                let page = self
                    .store
                    .ledger_entries(session, cursor, LEDGER_PAGE)
                    .map_err(|error| RetentionScanError::Unavailable(error.to_string()))?;
                if page.is_empty() {
                    break;
                }
                rows_read += page.len() as u64;
                if rows_read > self.limits.max_ledger_rows_per_session {
                    return Err(RetentionScanError::BoundExceeded(format!(
                        "session {} exceeds the {} row ledger scan bound",
                        session.raw(),
                        self.limits.max_ledger_rows_per_session
                    )));
                }
                for row in &page {
                    cursor = Some(row.seq);
                    let entry = decode_ledger_entry_row(row).map_err(|error| match error {
                        SessionError::Malformed(detail) => RetentionScanError::Malformed(format!(
                            "ledger row {} of session {}: {detail}",
                            row.seq,
                            session.raw()
                        )),
                        other => RetentionScanError::Unavailable(other.to_string()),
                    })?;
                    match entry.payload {
                        LedgerPayload::IntegrationTxnRecorded { row } => {
                            latest_txns.insert(row.run_id.clone(), row);
                        }
                        LedgerPayload::RunBaseRecorded { record } => {
                            for digest in [
                                record.snapshot_hash.as_str(),
                                record.manifest_digest.as_str(),
                            ] {
                                references_seen += usize::from(set.insert(
                                    digest,
                                    ProtectedReference {
                                        kind: ProtectionKind::RunBase,
                                        reference: record.run_id.clone(),
                                        detail: "immutable run base".into(),
                                    },
                                ));
                            }
                        }
                        LedgerPayload::EditTxnPrepared {
                            txn_id,
                            session: prepared_session,
                            files,
                            strategy,
                        } => {
                            open_edits.insert(
                                txn_id,
                                (
                                    format!("{prepared_session}:{strategy}"),
                                    files.into_iter().map(|file| file.base_digest).collect(),
                                ),
                            );
                        }
                        LedgerPayload::EditTxnCommitted { txn_id, .. }
                        | LedgerPayload::EditTxnRolledBack { txn_id, .. } => {
                            open_edits.remove(&txn_id);
                        }
                        _ => {}
                    }
                }
            }
            for row in latest_txns.values() {
                if row.phase.is_terminal() {
                    continue;
                }
                let detail = format!("open landing transaction ({})", row.phase.as_tag());
                for path in &row.paths {
                    if let Some(rollback) = &path.rollback_blob {
                        references_seen += usize::from(set.insert(
                            rollback,
                            ProtectedReference {
                                kind: ProtectionKind::IntegrationTxn,
                                reference: row.run_id.clone(),
                                detail: detail.clone(),
                            },
                        ));
                    }
                    for state in [&path.base_state, &path.candidate_state] {
                        if let EntryState::Regular { payload, .. } = state {
                            references_seen += usize::from(set.insert(
                                &payload.to_hex(),
                                ProtectedReference {
                                    kind: ProtectionKind::IntegrationTxn,
                                    reference: row.run_id.clone(),
                                    detail: detail.clone(),
                                },
                            ));
                        }
                    }
                }
                for snapshot in [
                    row.run_base_snapshot.as_str(),
                    row.verified_candidate_snapshot.as_str(),
                ] {
                    references_seen += usize::from(set.insert(
                        snapshot,
                        ProtectedReference {
                            kind: ProtectionKind::IntegrationTxn,
                            reference: row.run_id.clone(),
                            detail: detail.clone(),
                        },
                    ));
                }
            }
            if references_seen > self.limits.max_references {
                return Err(RetentionScanError::BoundExceeded(format!(
                    "{references_seen} references exceed the scan bound {}",
                    self.limits.max_references
                )));
            }
            for (txn_id, (label, digests)) in open_edits {
                for digest in digests {
                    references_seen += usize::from(set.insert(
                        &digest,
                        ProtectedReference {
                            kind: ProtectionKind::OpenEditTxn,
                            reference: format!("edit_txn:{txn_id}"),
                            detail: format!("open edit transaction ({label})"),
                        },
                    ));
                }
            }
            if references_seen > self.limits.max_references {
                return Err(RetentionScanError::BoundExceeded(format!(
                    "{references_seen} references exceed the scan bound {}",
                    self.limits.max_references
                )));
            }

            // Verification records of every task of the session.
            let tasks = self
                .store
                .list_tasks(session)
                .map_err(|error| RetentionScanError::Unavailable(error.to_string()))?;
            for task in tasks {
                let records = self
                    .store
                    .verification_record_list_by_task_with_evidence(task.task_id)
                    .map_err(|error| RetentionScanError::Unavailable(error.to_string()))?;
                for (record, _fingerprint_json, candidate_json) in records {
                    verification_records += 1;
                    if verification_records > self.limits.max_verification_records {
                        return Err(RetentionScanError::BoundExceeded(format!(
                            "{verification_records} verification records exceed the scan bound {}",
                            self.limits.max_verification_records
                        )));
                    }
                    let reference_id = format!("record:{}", record.id.raw());
                    let detail = "durable verification record".to_string();
                    let mut protect = |digest: &str, set: &mut LiveReferenceSet| {
                        references_seen += usize::from(set.insert(
                            digest,
                            ProtectedReference {
                                kind: ProtectionKind::VerificationRecord,
                                reference: reference_id.clone(),
                                detail: detail.clone(),
                            },
                        ));
                    };
                    if let Some(tree_hash) = &record.tree_hash {
                        protect(tree_hash, &mut set);
                    }
                    for file in &record.changed_files {
                        protect(&file.digest_hex, &mut set);
                    }
                    if let Some(candidate_json) = &candidate_json {
                        let candidate: faktor_core::state::CandidateProofRef =
                            serde_json::from_str(candidate_json).map_err(|error| {
                                RetentionScanError::Malformed(format!(
                                    "verification record {} candidate proof ref: {error}",
                                    record.id.raw()
                                ))
                            })?;
                        for digest in [
                            Some(candidate.base_manifest_hash.as_str()),
                            Some(candidate.candidate_manifest_hash.as_str()),
                            Some(candidate.accounting_snapshot_digest.as_str()),
                            candidate.run_base_snapshot.as_deref(),
                            candidate.candidate_snapshot.as_deref(),
                            candidate.sources_digest.as_deref(),
                            candidate.changed_files_digest.as_deref(),
                        ]
                        .into_iter()
                        .flatten()
                        {
                            protect(digest, &mut set);
                        }
                    }
                    if references_seen > self.limits.max_references {
                        return Err(RetentionScanError::BoundExceeded(format!(
                            "{references_seen} references exceed the scan bound {}",
                            self.limits.max_references
                        )));
                    }
                }
            }
        }
        Ok(set)
    }
}

/// A convenience digest check reused by tests and hosts.
pub fn is_canonical_digest(value: &str) -> bool {
    normalize_digest(value).is_some()
}

/// The hash type accepted by the CAS guard (kept public so hosts can convert
/// without re-implementing the parse).
pub fn parse_blob_hash(value: &str) -> Option<FileHash> {
    FileHash::from_hex(&normalize_digest(value)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::tests::{session, test_manager};
    use crate::ledger::{
        EditTxnLedgerFile, IntegrationPathTxn, IntegrationPathTxnState, IntegrationTxnPhase,
        IntegrationTxnRow, RunBaseRecord,
    };
    use crate::task::{Task, TaskBudget};
    use faktor_core::id::TaskId;
    use faktor_core::state::{FileStateEvidence, TaskState, VerificationStatus};
    use faktor_fs::tree_manifest::CanonicalMode;

    fn digest(seed: u8) -> String {
        format!("{:02x}", seed).repeat(32)
    }

    fn regular(payload_hex: &str) -> EntryState {
        EntryState::Regular {
            mode: CanonicalMode::RegularFile,
            payload: FileHash::from_hex(payload_hex).unwrap(),
        }
    }

    fn open_txn(run: &str, rollback_hex: &str, phase: IntegrationTxnPhase) -> IntegrationTxnRow {
        let snapshot = digest(9);
        IntegrationTxnRow {
            run_id: run.into(),
            task_id: 1,
            owner_root: "/owner".into(),
            candidate_root: "/candidate".into(),
            run_base_snapshot: snapshot.clone(),
            verified_candidate_snapshot: snapshot,
            sources_digest: digest(8),
            phase,
            paths: vec![IntegrationPathTxn {
                path: "src/lib.rs".into(),
                base_state: regular(rollback_hex),
                candidate_state: EntryState::Absent,
                rollback_blob: Some(rollback_hex.to_string()),
                rollback_link_target: None,
                canonical: true,
                state: IntegrationPathTxnState::Pending,
            }],
            path_count: 1,
            applied_count: 0,
            conflicts: Vec::new(),
            at_ms: 1,
        }
    }

    #[test]
    fn live_txn_rollback_blob_is_protected_and_a_terminal_phase_releases_it() {
        let (_dir, manager) = test_manager();
        let handle = session(&manager);
        let blob = digest(7);
        let base_snapshot = digest(6);
        handle
            .ledger_integration_txn_set(&open_txn("run-live", &blob, IntegrationTxnPhase::Prepared))
            .unwrap();
        handle
            .ledger_run_base_set(&RunBaseRecord {
                run_id: "run-base".into(),
                workspace_id: 1,
                worktree_id: 1,
                snapshot_hash: base_snapshot.clone(),
                manifest_digest: digest(5),
                root: "/base".into(),
                created_ms: 1,
            })
            .unwrap();

        let guard = LiveReferenceScanner::new(manager.store().as_ref())
            .scan()
            .unwrap();
        assert!(guard.reference(&blob).is_some(), "rollback blob protected");
        assert_eq!(
            guard.reference(&blob).unwrap().kind,
            ProtectionKind::IntegrationTxn
        );
        assert_eq!(guard.reference(&blob).unwrap().reference, "run-live");
        assert!(
            guard.reference(&base_snapshot).is_some(),
            "run base protected"
        );
        assert_eq!(
            guard.reference(&base_snapshot).unwrap().kind,
            ProtectionKind::RunBase
        );
        // The guardian accepts the same digest through the CAS guard.
        assert!(
            faktor_cas::retention::RetentionGuard::protect(&guard, &blob)
                .unwrap()
                .is_some()
        );

        // A terminal phase releases the rollback material.
        handle
            .ledger_integration_txn_set(&open_txn("run-live", &blob, IntegrationTxnPhase::Landed))
            .unwrap();
        let guard = LiveReferenceScanner::new(manager.store().as_ref())
            .scan()
            .unwrap();
        assert!(guard.reference(&blob).is_none(), "landed txn releases");
        assert!(
            guard.reference(&base_snapshot).is_some(),
            "run base stays protected"
        );
    }

    #[test]
    fn open_edit_txn_and_verification_records_protect_their_digests() {
        let (_dir, manager) = test_manager();
        let handle = session(&manager);
        let staged = digest(4);
        handle
            .ledger_edit_txn_prepared(
                11,
                "ses",
                &[EditTxnLedgerFile {
                    path: "src/lib.rs".into(),
                    base_digest: staged.clone(),
                    base_bytes_len: 3,
                }],
                "roll_forward",
            )
            .unwrap();
        let guard = LiveReferenceScanner::new(manager.store().as_ref())
            .scan()
            .unwrap();
        assert!(guard.reference(&staged).is_some(), "open edit txn protects");
        assert_eq!(
            guard.reference(&staged).unwrap().kind,
            ProtectionKind::OpenEditTxn
        );

        // A terminal row releases it.
        handle
            .ledger_edit_txn_committed(11, &["src/lib.rs".into()], &[], &[])
            .unwrap();
        let guard = LiveReferenceScanner::new(manager.store().as_ref())
            .scan()
            .unwrap();
        assert!(
            guard.reference(&staged).is_none(),
            "committed edit releases"
        );

        // Verification record: tree hash + changed file digest protected.
        let task = handle
            .create_task(Task {
                task_id: TaskId::new(1),
                session_id: handle.id(),
                goal: "g".into(),
                acceptance_criteria: vec!["a".into()],
                plan: Vec::new(),
                attachments: Vec::new(),
                budget: TaskBudget::default(),
                state: TaskState::Running,
                created_ms: 1,
                updated_ms: 1,
            })
            .unwrap();
        let tree = digest(3);
        let changed = digest(2);
        handle
            .create_verification_record(
                task.task_id,
                Some(tree.clone()),
                Vec::new(),
                Vec::new(),
                vec![FileStateEvidence {
                    path: "src/lib.rs".into(),
                    digest_hex: changed.clone(),
                    size: 3,
                }],
                Vec::new(),
                None,
                VerificationStatus::Running,
                1,
            )
            .unwrap();
        let guard = LiveReferenceScanner::new(manager.store().as_ref())
            .scan()
            .unwrap();
        assert!(guard.reference(&tree).is_some(), "tree hash protected");
        assert!(
            guard.reference(&changed).is_some(),
            "changed file protected"
        );
        assert_eq!(
            guard.reference(&tree).unwrap().kind,
            ProtectionKind::VerificationRecord
        );
    }

    #[test]
    fn scan_bounds_are_loud_and_never_silently_truncate() {
        let (_dir, manager) = test_manager();
        let handle = session(&manager);
        handle
            .ledger_integration_txn_set(&open_txn(
                "run-1",
                &digest(1),
                IntegrationTxnPhase::Prepared,
            ))
            .unwrap();
        let store = manager.store();
        let scanner = LiveReferenceScanner::with_limits(
            store.as_ref(),
            ScanLimits {
                max_sessions: 0,
                ..DEFAULT_SCAN_LIMITS
            },
        );
        assert!(matches!(
            scanner.scan().unwrap_err(),
            RetentionScanError::BoundExceeded(_)
        ));
        let store = manager.store();
        let scanner = LiveReferenceScanner::with_limits(
            store.as_ref(),
            ScanLimits {
                max_ledger_rows_per_session: 0,
                ..DEFAULT_SCAN_LIMITS
            },
        );
        assert!(matches!(
            scanner.scan().unwrap_err(),
            RetentionScanError::BoundExceeded(_)
        ));
        let store = manager.store();
        let scanner = LiveReferenceScanner::with_limits(
            store.as_ref(),
            ScanLimits {
                max_references: 0,
                ..DEFAULT_SCAN_LIMITS
            },
        );
        assert!(matches!(
            scanner.scan().unwrap_err(),
            RetentionScanError::BoundExceeded(_)
        ));
    }

    #[test]
    fn a_corrupt_durable_row_makes_the_scan_loud() {
        let (_dir, manager) = test_manager();
        let handle = session(&manager);
        handle
            .ledger_integration_txn_set(&open_txn(
                "run-x",
                &digest(1),
                IntegrationTxnPhase::Prepared,
            ))
            .unwrap();
        // A malformed row of a REGISTERED kind (the store is a seam; this is
        // what on-disk corruption or a foreign writer looks like).
        manager
            .store()
            .append_ledger_entry(
                handle.id(),
                "integration_txn",
                1,
                serde_json::json!({"nonsense": true}),
            )
            .unwrap();
        let error = LiveReferenceScanner::new(manager.store().as_ref())
            .scan()
            .unwrap_err();
        assert!(
            matches!(error, RetentionScanError::Malformed(_)),
            "a corrupt row refuses the scan, got {error:?}"
        );

        // An UNKNOWN entry kind is malformed too (never silently skipped).
        manager
            .store()
            .append_ledger_entry(handle.id(), "future_kind", 1, serde_json::json!({}))
            .unwrap();
        assert!(matches!(
            LiveReferenceScanner::new(manager.store().as_ref())
                .scan()
                .unwrap_err(),
            RetentionScanError::Malformed(_)
        ));
    }

    #[test]
    fn guard_answers_are_deterministic_and_digest_normalized() {
        let guard = LiveReferenceSet::default();
        assert!(RetentionGuard::protect(&guard, &digest(1))
            .unwrap()
            .is_none());
        assert!(!is_canonical_digest("ZZZ"));
        assert!(is_canonical_digest(&format!("blake3:{}", digest(1))));
        assert!(parse_blob_hash(&digest(1)).is_some());
        assert_eq!(guard.len(), 0);
        assert!(guard.is_empty());
    }
}
