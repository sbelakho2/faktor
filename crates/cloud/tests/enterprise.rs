//! Adversarial integration tests of the enterprise plane: retention GC
//! reference protection, deletion-job crash resume, audit coverage and
//! tenancy/role gates. Every test attempts a failure mode (protected delete,
//! store error, crash between steps, duplicate retry, foreign tenant, role
//! below the matrix).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use faktor_cloud::enterprise::{
    BlobDeletion, BlobStoreError, RetentionBlobStore, RetentionReferenceOracle,
};
use faktor_cloud::{
    ArtifactId, ArtifactKind, ArtifactReference, AuditAction, DeletionScope, DeletionState,
    DeletionStep, EnterpriseService, EnterpriseStore, ManualClock, NewArtifact, NoReferences,
    OrgSettings, OrganizationId, Principal, ReferenceKind, ReferenceScanError, RetentionClass,
    Role, SqliteControlPlaneStore, SsoConfigRef, UserId,
};

const T0: i64 = 1_700_000_000_000;

fn org(id: &str) -> OrganizationId {
    OrganizationId::try_new(id).unwrap()
}

fn principal_with(role: Role) -> Principal {
    Principal::user(UserId::try_new("usr_1").unwrap(), org("org_1"), role)
}

fn digest(seed: char) -> String {
    seed.to_string().repeat(64)
}

fn artifact(
    id: &str,
    class: RetentionClass,
    digest_seed: char,
    owner: Option<&str>,
) -> NewArtifact {
    NewArtifact {
        id: ArtifactId::try_new(id).unwrap(),
        session: Some("ses_1".into()),
        task: Some(1),
        owner: owner.map(str::to_string),
        kind: match class {
            RetentionClass::AuthorityCriticalRollback => ArtifactKind::RollbackBlob,
            RetentionClass::ProviderTranscript => ArtifactKind::ProviderTranscript,
            RetentionClass::BillingRecord => ArtifactKind::BillingLedger,
            RetentionClass::VerificationEvidence => ArtifactKind::ProofArtifact,
            _ => ArtifactKind::DiagnosticBundle,
        },
        digest: digest(digest_seed),
        size: 1024,
        retention_class: class,
        // A one-minute TTL for TTL classes (the policy floor); keep-forever
        // classes carry none.
        ttl_ms: if class.default_policy().is_keep_forever() {
            None
        } else {
            Some(60_000)
        },
    }
}

#[derive(Default)]
struct FakeOracle {
    refs: Mutex<BTreeMap<String, Vec<ArtifactReference>>>,
    fail: Mutex<bool>,
    calls: AtomicUsize,
}

impl FakeOracle {
    fn protect(&self, digest_hex: &str, reference: ArtifactReference) {
        self.refs
            .lock()
            .unwrap()
            .entry(digest_hex.to_string())
            .or_default()
            .push(reference);
    }

    fn release(&self, digest_hex: &str) {
        self.refs.lock().unwrap().remove(digest_hex);
    }

    fn break_scan(&self) {
        *self.fail.lock().unwrap() = true;
    }
}

impl RetentionReferenceOracle for FakeOracle {
    fn references(
        &self,
        _organization: &OrganizationId,
        digest: &str,
    ) -> Result<Vec<ArtifactReference>, ReferenceScanError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if *self.fail.lock().unwrap() {
            return Err(ReferenceScanError::Unavailable(
                "simulated store read failure".into(),
            ));
        }
        Ok(self
            .refs
            .lock()
            .unwrap()
            .get(digest)
            .cloned()
            .unwrap_or_default())
    }
}

#[derive(Default)]
struct FakeBlobs {
    deleted: Mutex<Vec<String>>,
    refuse: Mutex<BTreeSet<String>>,
    fail: Mutex<bool>,
    calls: AtomicUsize,
}

impl FakeBlobs {
    fn refuse(&self, digest_hex: &str) {
        self.refuse.lock().unwrap().insert(digest_hex.to_string());
    }

    fn break_store(&self) {
        *self.fail.lock().unwrap() = true;
    }

    fn delete_calls(&self) -> Vec<String> {
        self.deleted.lock().unwrap().clone()
    }
}

impl RetentionBlobStore for FakeBlobs {
    fn delete_blob(&self, digest: &str) -> Result<BlobDeletion, BlobStoreError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if *self.fail.lock().unwrap() {
            return Err(BlobStoreError::Unavailable(
                "simulated blob store outage".into(),
            ));
        }
        if self.refuse.lock().unwrap().contains(digest) {
            return Ok(BlobDeletion::Refused {
                reference: ArtifactReference {
                    kind: ReferenceKind::IntegrationTxn,
                    reference: "txn-guard".into(),
                    detail: "blob store guard".into(),
                },
            });
        }
        self.deleted.lock().unwrap().push(digest.to_string());
        Ok(BlobDeletion::Deleted)
    }
}

fn memory_service(clock: Arc<ManualClock>) -> EnterpriseService {
    let store = Arc::new(faktor_cloud::MemoryControlPlaneStore::new());
    EnterpriseService::new(store as Arc<dyn EnterpriseStore>, clock).unwrap()
}

fn txn_reference(run: &str) -> ArtifactReference {
    ArtifactReference {
        kind: ReferenceKind::IntegrationTxn,
        reference: run.into(),
        detail: "open landing transaction".into(),
    }
}

#[test]
fn gc_deletes_expired_transcript_and_protects_every_reference_kind_until_released() {
    let clock = Arc::new(ManualClock::new(T0));
    let service = memory_service(clock.clone());
    let principal = principal_with(Role::Admin);
    let oracle = FakeOracle::default();
    let blobs = FakeBlobs::default();

    let transcript = service
        .register_artifact(
            &principal,
            artifact("art_tx", RetentionClass::ProviderTranscript, 'a', None),
        )
        .unwrap();
    let rollback = service
        .register_artifact(
            &principal,
            artifact(
                "art_rb",
                RetentionClass::AuthorityCriticalRollback,
                'b',
                None,
            ),
        )
        .unwrap();
    assert_eq!(transcript.deletion_state, DeletionState::Active);
    clock.advance(120_000);
    assert!(service.mark_eligible(&principal, &transcript.id).unwrap());
    assert!(service.mark_eligible(&principal, &rollback.id).unwrap());

    // While the landing transaction is live, the rollback digest is
    // protected; the transcript (no reference) is deleted.
    oracle.protect(&rollback.digest, txn_reference("run-live"));
    let report = service.gc_pass(&principal, 50, &oracle, &blobs).unwrap();
    assert!(report.is_clean());
    assert_eq!(report.deleted, 1);
    assert_eq!(report.refused_protected, 1);
    assert_eq!(blobs.delete_calls(), vec![transcript.digest.clone()]);
    let stored = service.store().artifact(&rollback.id).unwrap().unwrap();
    assert_eq!(
        stored.deletion_state,
        DeletionState::Eligible,
        "a protected digest stays eligible, never deleted"
    );
    let transcript_state = service.store().artifact(&transcript.id).unwrap().unwrap();
    assert_eq!(transcript_state.deletion_state, DeletionState::Deleted);

    // The reference scan returned every reference kind in the matrix.
    for kind in [
        ReferenceKind::IntegrationTxn,
        ReferenceKind::VerificationRecord,
        ReferenceKind::RunBase,
    ] {
        let oracle = FakeOracle::default();
        oracle.protect(
            &digest('c'),
            ArtifactReference {
                kind,
                reference: "ref-1".into(),
                detail: "matrix".into(),
            },
        );
        let clock = Arc::new(ManualClock::new(T0));
        let service = memory_service(clock.clone());
        let p = principal_with(Role::Admin);
        let row = service
            .register_artifact(
                &p,
                artifact("art_m", RetentionClass::TerminalOutput, 'c', None),
            )
            .unwrap();
        clock.advance(120_000);
        assert!(service.mark_eligible(&p, &row.id).unwrap());
        let blobs = FakeBlobs::default();
        let report = service.gc_pass(&p, 50, &oracle, &blobs).unwrap();
        assert_eq!(
            report.refused_protected,
            1,
            "kind {} must protect the digest",
            kind.as_str()
        );
        assert!(blobs.delete_calls().is_empty());
        let event = last_audit(&service, &p, AuditAction::RetentionDeleteRefused);
        assert_eq!(event.event.correlation_id.as_deref(), Some("ref-1"));
    }

    // Once the transaction is terminal the blob may be deleted.
    oracle.release(&rollback.digest);
    let report = service.gc_pass(&principal, 50, &oracle, &blobs).unwrap();
    assert_eq!(report.deleted, 1);
    assert_eq!(blobs.delete_calls().len(), 2);
    assert_eq!(
        service
            .store()
            .artifact(&rollback.id)
            .unwrap()
            .unwrap()
            .deletion_state,
        DeletionState::Deleted
    );
}

fn last_audit(
    service: &EnterpriseService,
    principal: &Principal,
    action: AuditAction,
) -> faktor_cloud::AuditEvent {
    let export = service
        .audit_export(principal, &principal.organization, None, 200)
        .unwrap();
    export
        .events
        .into_iter()
        .rev()
        .find(|event| event.event.action == action)
        .unwrap_or_else(|| panic!("no audit row for {}", action.as_str()))
}

#[test]
fn gc_only_touches_expired_eligible_ttl_rows_and_is_loud_on_scan_and_store_errors() {
    let clock = Arc::new(ManualClock::new(T0));
    let service = memory_service(clock.clone());
    let principal = principal_with(Role::Admin);
    let oracle = FakeOracle::default();
    let blobs = FakeBlobs::default();

    // Active (never promoted) and keep-forever rows are never deleted.
    let active = service
        .register_artifact(
            &principal,
            artifact("art_active", RetentionClass::Diagnostics, 'd', None),
        )
        .unwrap();
    let evidence = service
        .register_artifact(
            &principal,
            artifact("art_proof", RetentionClass::VerificationEvidence, 'e', None),
        )
        .unwrap();
    let report = service.gc_pass(&principal, 50, &oracle, &blobs).unwrap();
    assert_eq!(report.deleted, 0);
    assert!(blobs.delete_calls().is_empty());
    assert!(report.outcomes.iter().any(|outcome| matches!(
        outcome,
        faktor_cloud::GcOutcome::SkippedNotEligible { artifact, .. } if artifact == &active.id
    )));
    assert!(report.outcomes.iter().any(|outcome| matches!(
        outcome,
        faktor_cloud::GcOutcome::SkippedKeepForever { artifact, .. } if artifact == &evidence.id
    )));

    // A keep-forever row cannot even be promoted to Eligible.
    assert!(service.mark_eligible(&principal, &evidence.id).is_err());

    // Eligible but NOT yet expired: skipped (defense in depth — the store is
    // a seam, so an Eligible row with a future expiry must still be safe).
    let fresh = service
        .register_artifact(
            &principal,
            artifact("art_fresh", RetentionClass::TerminalOutput, 'f', None),
        )
        .unwrap();
    assert!(!service.mark_eligible(&principal, &fresh.id).unwrap());
    let mut forced = service.store().artifact(&fresh.id).unwrap().unwrap();
    forced.deletion_state = DeletionState::Eligible;
    service.store().put_artifact(&forced).unwrap();
    let report = service.gc_pass(&principal, 50, &oracle, &blobs).unwrap();
    assert!(report.outcomes.iter().any(|outcome| matches!(
        outcome,
        faktor_cloud::GcOutcome::SkippedNotExpired { artifact, .. } if artifact == &fresh.id
    )));
    assert!(blobs.delete_calls().is_empty());
    // Undo the forced eligibility: the row must not linger as a candidate
    // for the next assertions (the store is a seam operators can write).
    let mut forced = service.store().artifact(&fresh.id).unwrap().unwrap();
    forced.deletion_state = DeletionState::Active;
    service.store().put_artifact(&forced).unwrap();

    // Scan failure: the row is refused (fail closed), audited, and the blob
    // store is never asked.
    clock.advance(120_000);
    let victim = service
        .register_artifact(
            &principal,
            artifact("art_scan", RetentionClass::Diagnostics, '1', None),
        )
        .unwrap();
    clock.advance(120_000);
    assert!(service.mark_eligible(&principal, &victim.id).unwrap());
    oracle.break_scan();
    let report = service.gc_pass(&principal, 50, &oracle, &blobs).unwrap();
    assert_eq!(report.scan_failures, 1);
    assert!(!report.is_clean());
    assert!(blobs.delete_calls().is_empty());
    assert_eq!(
        service
            .store()
            .artifact(&victim.id)
            .unwrap()
            .unwrap()
            .deletion_state,
        DeletionState::Eligible,
        "a scan error never deletes"
    );
    last_audit(&service, &principal, AuditAction::RetentionScanFailed);

    // Blob store failure: the row stays eligible for the next pass.
    let oracle2 = FakeOracle::default();
    blobs.break_store();
    let report = service.gc_pass(&principal, 50, &oracle2, &blobs).unwrap();
    assert!(report.delete_failures >= 1);
    assert_eq!(
        service
            .store()
            .artifact(&victim.id)
            .unwrap()
            .unwrap()
            .deletion_state,
        DeletionState::Eligible
    );

    // Blob-store guard refusal (defense in depth beyond the oracle).
    let blobs3 = FakeBlobs::default();
    blobs3.refuse(&victim.digest);
    let report = service.gc_pass(&principal, 50, &oracle2, &blobs3).unwrap();
    assert_eq!(report.refused_protected, 1);
    assert!(report.is_clean(), "a typed refusal is not a failure");
}

#[test]
fn audit_ledger_covers_every_listed_mutation_with_before_after_and_is_idempotent() {
    let clock = Arc::new(ManualClock::new(T0));
    let service = memory_service(clock);
    let admin = principal_with(Role::Admin);

    let mutations = [
        (AuditAction::MemberAdded, "user", "usr_2"),
        (AuditAction::MemberInvited, "invitation", "inv_1"),
        (AuditAction::MemberRemoved, "user", "usr_3"),
        (AuditAction::MemberRoleChanged, "membership", "mem_1"),
        (
            AuditAction::RepositoryInstallationAdded,
            "installation",
            "inst_1",
        ),
        (
            AuditAction::RepositoryInstallationChanged,
            "installation",
            "inst_1",
        ),
        (
            AuditAction::RepositoryInstallationRemoved,
            "installation",
            "inst_1",
        ),
        (
            AuditAction::ProviderCredentialCreated,
            "provider_credential",
            "cred_1",
        ),
        (
            AuditAction::ProviderCredentialRotated,
            "provider_credential",
            "cred_1",
        ),
        (
            AuditAction::ProviderCredentialRevoked,
            "provider_credential",
            "cred_1",
        ),
        (AuditAction::PolicyChanged, "policy", "policy_1"),
        (AuditAction::SecretCreated, "secret", "sec_1"),
        (AuditAction::SecretRotated, "secret", "sec_1"),
        (AuditAction::SecretDeleted, "secret", "sec_1"),
        (AuditAction::SecretAccessed, "secret", "sec_1"),
        (AuditAction::HumanApprovalRequested, "approval", "appr_1"),
        (AuditAction::HumanApprovalDecided, "approval", "appr_1"),
        (AuditAction::WorkerRegistered, "worker", "worker_1"),
        (AuditAction::WorkerRevoked, "worker", "worker_1"),
        (AuditAction::EntitlementChanged, "subscription", "sub_1"),
        (AuditAction::AdminSettingChanged, "org_settings", "org_1"),
        (AuditAction::PrivilegedToolGranted, "tool_grant", "tool_1"),
        (AuditAction::PrivilegedToolRevoked, "tool_grant", "tool_1"),
    ];
    for (action, kind, object) in mutations {
        let event = service
            .record_mutation(
                &admin,
                &org("org_1"),
                action,
                kind,
                object,
                Some("before"),
                Some("after"),
                Some("ses_1/run_1"),
            )
            .unwrap();
        assert_eq!(event.event.action, action);
        assert_eq!(event.event.before_ref.as_deref(), Some("before"));
        assert_eq!(event.event.after_ref.as_deref(), Some("after"));
        assert_eq!(
            event.event.correlation_id.as_deref(),
            Some("ses_1/run_1"),
            "correlation id shared with task/run ids"
        );
    }

    // Retrying the SAME logical mutation appends no second row.
    let first = service
        .record_mutation(
            &admin,
            &org("org_1"),
            AuditAction::MemberRoleChanged,
            "membership",
            "mem_1",
            Some("before"),
            Some("after"),
            Some("ses_1/run_1"),
        )
        .unwrap();
    let again = service
        .record_mutation(
            &admin,
            &org("org_1"),
            AuditAction::MemberRoleChanged,
            "membership",
            "mem_1",
            Some("before"),
            Some("after"),
            Some("ses_1/run_1"),
        )
        .unwrap();
    assert_eq!(first.seq, again.seq, "idempotent append keeps the seq");

    // Cursor export is ordered and pages.
    let page = service
        .audit_export(&admin, &org("org_1"), None, 5)
        .unwrap();
    assert_eq!(page.events.len(), 5);
    assert!(page.head_seq >= page.events.last().unwrap().seq);
    let cursor = page.next_cursor.unwrap();
    let next = service
        .audit_export(&admin, &org("org_1"), Some(cursor.parse().unwrap()), 200)
        .unwrap();
    assert!(next
        .events
        .iter()
        .all(|event| event.seq > cursor.parse::<i64>().unwrap()));
    assert!(next.events.windows(2).all(|pair| pair[0].seq < pair[1].seq));

    // Every appended row is present exactly once per logical mutation.
    let all = service
        .audit_export(&admin, &org("org_1"), None, 200)
        .unwrap();
    let keys: BTreeSet<&str> = all
        .events
        .iter()
        .map(|event| event.event_key.as_str())
        .collect();
    assert_eq!(keys.len(), all.events.len(), "the ledger never forks");

    // A member below the audit-read matrix is refused; a member may not
    // mint audit rows either.
    let member = principal_with(Role::Member);
    assert!(service
        .audit_export(&member, &org("org_1"), None, 10)
        .is_err());
    assert!(service
        .record_mutation(
            &member,
            &org("org_1"),
            AuditAction::SecretAccessed,
            "secret",
            "sec_1",
            None,
            None,
            None,
        )
        .is_err());
}

#[test]
fn audit_rows_live_in_their_own_table_and_never_mix_with_proof_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cp.db");
    let store: Arc<dyn EnterpriseStore> = Arc::new(SqliteControlPlaneStore::open(&path).unwrap());
    let service = EnterpriseService::new(store, Arc::new(ManualClock::new(T0))).unwrap();
    let admin = principal_with(Role::Admin);
    service
        .record_mutation(
            &admin,
            &org("org_1"),
            AuditAction::SecretAccessed,
            "secret",
            "sec_1",
            None,
            None,
            None,
        )
        .unwrap();

    // The enterprise audit rows live in `ent_audit_event`; the engineering
    // proof tables (verification_record, ledger_entry, ...) never appear in
    // this database at all, so a proof query can never return an audit row.
    let conn = rusqlite::Connection::open(&path).unwrap();
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap();
    let tables: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(tables.iter().any(|name| name == "ent_audit_event"));
    for proof_table in [
        "verification_record",
        "ledger_entry",
        "proof",
        "evidence",
        "session",
    ] {
        assert!(
            !tables.iter().any(|name| name == proof_table),
            "the enterprise database must not carry the engineering proof table {proof_table}"
        );
    }
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM ent_audit_event", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    // There is no update/delete surface on the ledger: the only writes are
    // INSERTs from the store impl (compile-time), and a duplicate append is
    // a no-op, not an update.
    assert_eq!(
        conn.execute(
            "INSERT OR IGNORE INTO ent_audit_event
                (organization_id, event_key, action, object, occurred_at_ms, payload)
             VALUES ('org_1', 'k', 'member_added', 'x', 1, '{}')",
            [],
        )
        .unwrap(),
        1
    );
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM ent_audit_event", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);
}

#[test]
fn deletion_job_resumes_after_crash_and_retains_billing_records_by_policy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cp.db");
    let owner = principal_with(Role::Owner);
    let oracle = NoReferences;
    let blobs = FakeBlobs::default();

    // ---- first service instance: start + freeze, then "crash".
    let (job_id, ordinary_id, billing_id) = {
        let store: Arc<dyn EnterpriseStore> =
            Arc::new(SqliteControlPlaneStore::open(&path).unwrap());
        let service = EnterpriseService::new(store, Arc::new(ManualClock::new(T0))).unwrap();
        let ordinary = service
            .register_artifact(
                &owner,
                artifact("art_ord", RetentionClass::Diagnostics, '2', None),
            )
            .unwrap();
        let billing = service
            .register_artifact(
                &owner,
                artifact("art_bill", RetentionClass::BillingRecord, '3', None),
            )
            .unwrap();
        let job = service
            .start_deletion(&owner, DeletionScope::Organization)
            .unwrap();
        assert_eq!(job.next_step, DeletionStep::FreezeAdmissions);
        let job = service
            .advance_deletion(&owner, &job.id, &oracle, &blobs)
            .unwrap();
        assert_eq!(job.frozen_at_ms, Some(T0));
        assert_eq!(job.next_step, DeletionStep::ExportManifest);
        assert!(service.admissions_frozen(&org("org_1")).unwrap());
        (job.id.clone(), ordinary.id, billing.id)
    };

    // ---- second instance over the SAME database resumes at the next step.
    let store: Arc<dyn EnterpriseStore> = Arc::new(SqliteControlPlaneStore::open(&path).unwrap());
    let service = EnterpriseService::new(store, Arc::new(ManualClock::new(T0 + 1000))).unwrap();
    assert!(
        service.admissions_frozen(&org("org_1")).unwrap(),
        "the freeze survives a restart"
    );
    // A new registration is refused while frozen.
    assert!(service
        .register_artifact(
            &owner,
            artifact("art_new", RetentionClass::Diagnostics, '4', None),
        )
        .is_err());

    let mut job = service.deletion_job(&owner, &job_id).unwrap();
    assert_eq!(job.next_step, DeletionStep::ExportManifest);
    for _ in 0..3 {
        job = service
            .advance_deletion(&owner, &job.id, &oracle, &blobs)
            .unwrap();
        if job.state == faktor_cloud::DeletionJobState::Completed {
            break;
        }
    }
    assert_eq!(job.state, faktor_cloud::DeletionJobState::Completed);
    let manifest = job.manifest.clone().unwrap();
    assert_eq!(manifest.entries.len(), 1);
    assert_eq!(manifest.retained.len(), 1, "billing records are retained");
    assert_eq!(manifest.retained[0].id, billing_id);
    assert_eq!(
        service
            .store()
            .artifact(&ordinary_id)
            .unwrap()
            .unwrap()
            .deletion_state,
        DeletionState::Deleted
    );
    assert_eq!(
        service
            .store()
            .artifact(&billing_id)
            .unwrap()
            .unwrap()
            .deletion_state,
        DeletionState::Retained,
        "billing retention is a durable state, not a silent skip"
    );
    let tombstone = service
        .tombstone(&owner, &format!("org:{}", "org_1"))
        .unwrap()
        .expect("tombstone");
    assert_eq!(
        tombstone.manifest_digest.as_deref(),
        Some(manifest.digest.as_str())
    );
    assert_eq!(tombstone.deleted_artifacts, 1);
    assert_eq!(tombstone.retained_artifacts, 1);

    // Reconciliation audit rows exist exactly once per job step; a re-run of
    // the completed job appends nothing.
    let export = service
        .audit_export(&owner, &org("org_1"), None, 200)
        .unwrap();
    let count = |action: AuditAction| {
        export
            .events
            .iter()
            .filter(|event| event.event.action == action)
            .count()
    };
    assert_eq!(count(AuditAction::DeletionJobStarted), 1);
    assert_eq!(count(AuditAction::DeletionAdmissionsFrozen), 1);
    assert_eq!(count(AuditAction::DeletionManifestExported), 1);
    assert_eq!(count(AuditAction::DeletionTombstoned), 1);
    assert!(count(AuditAction::DeletionClassReconciled) >= 1);
    let before = export.head_seq;
    let replayed = service
        .advance_deletion(&owner, &job_id, &oracle, &blobs)
        .unwrap();
    assert_eq!(replayed.state, faktor_cloud::DeletionJobState::Completed);
    let after = service
        .audit_export(&owner, &org("org_1"), None, 200)
        .unwrap()
        .head_seq;
    assert_eq!(before, after, "a completed job re-run appends no rows");
}

#[test]
fn account_scoped_deletion_distinguishes_ordinary_data_from_billing_retained_records() {
    let clock = Arc::new(ManualClock::new(T0));
    let service = memory_service(clock);
    let owner = principal_with(Role::Owner);
    let oracle = NoReferences;
    let blobs = FakeBlobs::default();

    let mine = service
        .register_artifact(
            &owner,
            artifact("art_mine", RetentionClass::UserArtifact, '5', Some("usr_1")),
        )
        .unwrap();
    let mine_billing = service
        .register_artifact(
            &owner,
            artifact(
                "art_mine_bill",
                RetentionClass::BillingRecord,
                '6',
                Some("usr_1"),
            ),
        )
        .unwrap();
    let other = service
        .register_artifact(
            &owner,
            artifact(
                "art_other",
                RetentionClass::UserArtifact,
                '7',
                Some("usr_2"),
            ),
        )
        .unwrap();

    let job = service
        .start_deletion(
            &owner,
            DeletionScope::Account {
                user: UserId::try_new("usr_1").unwrap(),
            },
        )
        .unwrap();
    let mut job = job;
    for _ in 0..4 {
        job = service
            .advance_deletion(&owner, &job.id, &oracle, &blobs)
            .unwrap();
    }
    assert_eq!(job.state, faktor_cloud::DeletionJobState::Completed);
    let manifest = job.manifest.unwrap();
    assert_eq!(
        manifest.entries.len(),
        1,
        "only the account's ordinary data"
    );
    assert_eq!(manifest.entries[0].id, mine.id);
    assert_eq!(manifest.retained.len(), 1);
    assert_eq!(manifest.retained[0].id, mine_billing.id);
    assert_eq!(
        service
            .store()
            .artifact(&other.id)
            .unwrap()
            .unwrap()
            .deletion_state,
        DeletionState::Active,
        "another account's data is untouched"
    );
}

#[test]
fn effective_config_digest_is_attributable_in_audit_rows() {
    let clock = Arc::new(ManualClock::new(T0));
    let service = memory_service(clock);
    let admin = principal_with(Role::Admin);
    use faktor_cloud::{ConfigKey, ConfigLayer, ConfigScope, LayerSemantics, LayerValue};

    let layers = vec![ConfigLayer {
        scope: ConfigScope::Task,
        semantics: LayerSemantics::Policy,
        scope_ref: Some("ses_1/run_1".into()),
        revision: 7,
        values: BTreeMap::from([(
            ConfigKey::ToolGrants,
            LayerValue::Policy(vec!["shell".into()]),
        )]),
    }];
    let effective = faktor_cloud::resolve_layers(&layers).unwrap();
    let attestation = effective.attestation();
    assert_eq!(attestation.digest, effective.digest);

    // The digest is recordable with the run's correlation id; resolving the
    // same layers again is byte-identical.
    let event = service
        .record_mutation(
            &admin,
            &org("org_1"),
            AuditAction::PolicyChanged,
            "effective_config",
            "ses_1/run_1",
            None,
            Some(&attestation.digest),
            Some("ses_1/run_1"),
        )
        .unwrap();
    assert_eq!(
        event.event.after_ref.as_deref(),
        Some(attestation.digest.as_str())
    );
    assert_eq!(
        faktor_cloud::resolve_layers(&layers).unwrap().digest,
        attestation.digest
    );

    // Changing the layer revision alone changes the attributable digest.
    let mut changed = layers.clone();
    changed[0].revision = 8;
    assert_ne!(
        faktor_cloud::resolve_layers(&changed).unwrap().digest,
        attestation.digest
    );
}

#[test]
fn durable_config_layers_are_policy_audited_and_foreign_writes_are_refused() {
    let clock = Arc::new(ManualClock::new(T0));
    let service = memory_service(clock);
    let admin = principal_with(Role::Admin);
    use faktor_cloud::{ConfigKey, ConfigLayer, ConfigScope, LayerSemantics, LayerValue};

    let layer = ConfigLayer {
        scope: ConfigScope::Organization,
        semantics: LayerSemantics::Policy,
        scope_ref: Some("org_1".into()),
        revision: 1,
        values: BTreeMap::from([(ConfigKey::Network, LayerValue::Policy(vec!["none".into()]))]),
    };
    let stored = service.put_config_layer(&admin, layer.clone()).unwrap();
    assert_eq!(stored.id, "organization:org_1");
    assert_eq!(service.config_layers(&admin).unwrap().len(), 1);

    // The upsert is audited as a policy change with before/after digests.
    let event = last_audit(&service, &admin, AuditAction::PolicyChanged);
    assert_eq!(event.event.object, "organization:org_1");
    assert!(event.event.before_ref.is_none());
    assert!(event.event.after_ref.is_some());

    // A widened update is still a policy change row with BOTH refs (the
    // intersection semantics live in the resolver, not in the store).
    let mut widened = layer.clone();
    widened.values.insert(
        ConfigKey::Network,
        LayerValue::Policy(vec!["none".into(), "provider".into()]),
    );
    service.put_config_layer(&admin, widened).unwrap();
    let events = service
        .audit_export(&admin, &org("org_1"), None, 50)
        .unwrap()
        .events;
    let policy_rows: Vec<_> = events
        .iter()
        .filter(|event| event.event.action == AuditAction::PolicyChanged)
        .collect();
    assert_eq!(policy_rows.len(), 2);
    assert!(policy_rows[1].event.before_ref.is_some());
    assert!(policy_rows[1].event.after_ref.is_some());

    // A member cannot write layers; removal is audited with a before ref.
    let member = principal_with(Role::Member);
    assert!(service.put_config_layer(&member, layer.clone()).is_err());
    assert!(service
        .remove_config_layer(&admin, "organization:org_1")
        .unwrap());
    assert!(service.config_layers(&admin).unwrap().is_empty());
    let event = last_audit(&service, &admin, AuditAction::PolicyChanged);
    assert!(event.event.before_ref.is_some());
    assert!(event.event.after_ref.is_none());
    assert!(!service
        .remove_config_layer(&admin, "organization:org_1")
        .unwrap());
}

#[test]
fn settings_writes_are_role_gated_ceiling_bounded_and_audited() {
    let clock = Arc::new(ManualClock::new(T0));
    let service = memory_service(clock);
    let admin = principal_with(Role::Admin);

    // Above the class ceiling: refused.
    let mut too_long = OrgSettings::empty(org("org_1"), T0);
    too_long.retention_overrides.insert(
        RetentionClass::ProviderTranscript,
        365 * 24 * 60 * 60 * 1000,
    );
    assert!(service.set_settings(&admin, too_long).is_err());

    // Keep-forever class: refused.
    let mut keep_forever = OrgSettings::empty(org("org_1"), T0);
    keep_forever
        .retention_overrides
        .insert(RetentionClass::VerificationEvidence, 60_000);
    assert!(service.set_settings(&admin, keep_forever).is_err());

    // Within the ceiling: accepted, revisioned, audited with before/after.
    let mut ok = OrgSettings::empty(org("org_1"), T0);
    ok.retention_overrides
        .insert(RetentionClass::ProviderTranscript, 60_000);
    ok.allowed_providers = vec!["anthropic".into()];
    ok.sso = Some(SsoConfigRef {
        issuer: "https://idp.example".into(),
        client_id: "faktor".into(),
        membership_claim: "groups".into(),
        group_role_map: BTreeMap::from([("admins".to_string(), Role::Admin)]),
        client_secret_ref: Some("sso-client-secret".into()),
        enabled: true,
    });
    let saved = service.set_settings(&admin, ok).unwrap();
    assert_eq!(saved.revision, 1);
    let again = service
        .set_settings(
            &admin,
            OrgSettings {
                organization: org("org_1"),
                revision: 0,
                allowed_providers: vec!["anthropic".into()],
                allowed_models: Vec::new(),
                retention_overrides: BTreeMap::new(),
                sso: None,
                updated_at_ms: T0,
            },
        )
        .unwrap();
    assert_eq!(again.revision, 2, "each write is a revision");
    let events = service
        .audit_export(&admin, &org("org_1"), None, 50)
        .unwrap();
    let setting_events: Vec<_> = events
        .events
        .iter()
        .filter(|event| event.event.action == AuditAction::AdminSettingChanged)
        .collect();
    assert_eq!(setting_events.len(), 2);
    assert!(setting_events[0].event.before_ref.is_none());
    assert!(setting_events[0].event.after_ref.is_some());
    assert!(setting_events[1].event.before_ref.is_some());
    assert!(setting_events[1].event.after_ref.is_some());

    // Role gates: member cannot write settings; viewer may read.
    let member = principal_with(Role::Member);
    assert!(service
        .set_settings(&member, OrgSettings::empty(org("org_1"), T0))
        .is_err());
    let viewer = principal_with(Role::Viewer);
    assert!(service.settings(&viewer, &org("org_1")).is_ok());
}

#[test]
fn foreign_organization_access_is_indistinguishable_from_missing() {
    let clock = Arc::new(ManualClock::new(T0));
    let service = memory_service(clock);
    let admin = principal_with(Role::Admin);
    let foreign = org("org_2");

    assert!(service.audit_export(&admin, &foreign, None, 10).is_err());
    assert!(service.settings(&admin, &foreign).is_err());
    assert!(service.artifacts(&admin, &foreign, None, 10).is_err());
    // A foreign deletion job is a not-found even for an owner of the
    // caller's own organization (role gate first, tenancy second).
    let local_owner = Principal::user(UserId::try_new("usr_9").unwrap(), org("org_1"), Role::Owner);
    let foreign_owner =
        Principal::user(UserId::try_new("usr_9").unwrap(), org("org_2"), Role::Owner);
    let job = service
        .start_deletion(&foreign_owner, DeletionScope::Organization)
        .unwrap();
    let denied = service.deletion_job(&local_owner, &job.id).unwrap_err();
    assert_eq!(denied.code(), "not_found");
}

#[test]
fn malformed_artifact_rows_are_refused_before_any_durable_write() {
    let clock = Arc::new(ManualClock::new(T0));
    let service = memory_service(clock);
    let admin = principal_with(Role::Admin);
    // A digest that is not canonical is refused.
    let mut malformed = artifact("art_bad", RetentionClass::Diagnostics, '8', None);
    malformed.digest = "not-a-digest".into();
    assert!(service.register_artifact(&admin, malformed).is_err());
    // An override above the class ceiling is refused.
    let mut over_ceiling = artifact("art_bad2", RetentionClass::TerminalOutput, '9', None);
    over_ceiling.ttl_ms = Some(365 * 24 * 60 * 60 * 1000);
    assert!(service.register_artifact(&admin, over_ceiling).is_err());
    // A viewer cannot register at all.
    let viewer = principal_with(Role::Viewer);
    assert!(service
        .register_artifact(
            &viewer,
            artifact("art_v", RetentionClass::Diagnostics, 'a', None)
        )
        .is_err());
    assert!(service
        .store()
        .artifacts(&org("org_1"), None, 10)
        .unwrap()
        .is_empty());
}
