use faktor_approval::{
    save_operator_key, ApprovalAuthority, ApprovalCheck, ApprovalDraft, ApprovalError,
    ApprovalKind, DecisionOutcome, OperatorKey, Principal, PrincipalKind, RiskLevel,
};

fn draft(requester: &str, digest: &str, revision: &str) -> ApprovalDraft {
    ApprovalDraft {
        kind: ApprovalKind::EngineeringConstraintRelaxation,
        subject: "req-set-1".to_string(),
        evidence: vec!["cpk-study".to_string()],
        proposed_change: "hole tolerance +/-0.010 -> +/-0.025 mm".to_string(),
        risk: RiskLevel::High,
        requester: Principal::new(requester, PrincipalKind::Agent).unwrap(),
        bound_revision: revision.to_string(),
        bound_digest: digest.to_string(),
        expires_at_ms: faktor_approval::unix_now_ms() + 60_000,
    }
}

fn authority(directory: &std::path::Path) -> (ApprovalAuthority, OperatorKey) {
    let key = OperatorKey::generate();
    save_operator_key(directory, "quality-director", &key, false).unwrap();
    faktor_approval::register_operator_key(
        directory,
        "quality-director",
        &key,
        faktor_approval::unix_now_ms().max(1),
    )
    .unwrap();
    (ApprovalAuthority::open(directory).unwrap(), key)
}

#[test]
fn human_approval_is_revision_bound_and_authorizes_only_the_exact_digest() {
    let temp = tempfile::tempdir().unwrap();
    let (authority, key) = authority(temp.path());
    let digest = "a".repeat(64);
    let now = faktor_approval::unix_now_ms();
    let request = authority
        .propose(draft("agent-1", &digest, "7"), now)
        .unwrap();
    let check = ApprovalCheck::new(
        ApprovalKind::EngineeringConstraintRelaxation,
        "req-set-1",
        "7",
        digest.clone(),
    );
    assert!(matches!(
        authority.authorize(&check, now + 1).unwrap_err(),
        ApprovalError::Pending { .. }
    ));
    let decision = authority
        .decide(
            &request.id,
            Principal::new("quality-director", PrincipalKind::HumanOperator).unwrap(),
            &key,
            DecisionOutcome::Approved,
            "reviewed".to_string(),
            now + 1,
        )
        .unwrap();
    assert_eq!(decision.request_digest, request.digest().unwrap());
    let trusted: std::collections::BTreeSet<String> =
        [decision.signature.key_id.clone()].into_iter().collect();
    decision.verify(Some(&trusted)).unwrap();
    let mut forged = decision.clone();
    forged.rationale = "a different decision".to_string();
    assert!(matches!(
        forged.verify(Some(&trusted)).unwrap_err(),
        ApprovalError::SignatureMismatch(_)
    ));
    let token = authority.authorize(&check, now + 2).unwrap();
    assert_eq!(token.request_id(), request.id);
    assert_eq!(token.bound_digest(), digest);
    assert_eq!(token.bound_revision(), "7");
    assert_eq!(token.kind(), ApprovalKind::EngineeringConstraintRelaxation);

    // A later mutation (different digest) cannot reuse the approval.
    let mutated = ApprovalCheck::new(
        ApprovalKind::EngineeringConstraintRelaxation,
        "req-set-1",
        "8",
        "b".repeat(64),
    );
    assert!(matches!(
        authority.authorize(&mutated, now + 3).unwrap_err(),
        ApprovalError::Stale { .. }
    ));
}

#[test]
fn agents_cannot_decide_and_a_requester_cannot_self_approve() {
    let temp = tempfile::tempdir().unwrap();
    let (authority, key) = authority(temp.path());
    let now = faktor_approval::unix_now_ms();
    let digest = "c".repeat(64);
    let request = authority
        .propose(draft("agent-1", &digest, "7"), now)
        .unwrap();

    let agent = Principal::new("agent-1", PrincipalKind::Agent).unwrap();
    assert!(matches!(
        authority
            .decide(
                &request.id,
                agent,
                &key,
                DecisionOutcome::Approved,
                "self".to_string(),
                now + 1,
            )
            .unwrap_err(),
        ApprovalError::AgentApproval { .. }
    ));

    // A human requester cannot approve its own request either.
    let self_request = authority
        .propose(draft("quality-director", &digest, "7"), now)
        .unwrap();
    assert!(matches!(
        authority
            .decide(
                &self_request.id,
                Principal::new("quality-director", PrincipalKind::HumanOperator).unwrap(),
                &key,
                DecisionOutcome::Approved,
                "self".to_string(),
                now + 1,
            )
            .unwrap_err(),
        ApprovalError::SelfApproval { .. }
    ));

    // A decision signed by an unregistered human key is refused.
    let rogue = OperatorKey::generate();
    assert!(matches!(
        authority
            .decide(
                &request.id,
                Principal::new("rogue", PrincipalKind::HumanOperator).unwrap(),
                &rogue,
                DecisionOutcome::Approved,
                "rogue".to_string(),
                now + 1,
            )
            .unwrap_err(),
        ApprovalError::UntrustedOperator { .. }
    ));
}

#[test]
fn denial_is_typed_and_blocks_the_action() {
    let temp = tempfile::tempdir().unwrap();
    let (authority, key) = authority(temp.path());
    let now = faktor_approval::unix_now_ms();
    let digest = "d".repeat(64);
    let request = authority
        .propose(draft("agent-1", &digest, "7"), now)
        .unwrap();
    authority
        .decide(
            &request.id,
            Principal::new("quality-director", PrincipalKind::HumanOperator).unwrap(),
            &key,
            DecisionOutcome::Denied {
                reason: "capability study inconclusive".to_string(),
            },
            "denied".to_string(),
            now + 1,
        )
        .unwrap();
    let error = authority
        .authorize(
            &ApprovalCheck::new(
                ApprovalKind::EngineeringConstraintRelaxation,
                "req-set-1",
                "7",
                digest,
            ),
            now + 2,
        )
        .unwrap_err();
    assert!(matches!(error, ApprovalError::Denied { .. }));
    assert_eq!(error.code(), "approval_denied");
}

#[test]
fn approval_and_audit_chain_survive_restart() {
    let temp = tempfile::tempdir().unwrap();
    let now = faktor_approval::unix_now_ms();
    let digest = "e".repeat(64);
    let request = {
        let (authority, key) = authority(temp.path());
        let request = authority
            .propose(draft("agent-1", &digest, "7"), now)
            .unwrap();
        authority
            .decide(
                &request.id,
                Principal::new("quality-director", PrincipalKind::HumanOperator).unwrap(),
                &key,
                DecisionOutcome::Approved,
                "reviewed".to_string(),
                now + 1,
            )
            .unwrap();
        request
    };

    // Reopen as a fresh daemon would: no in-memory state is carried.
    let reopened = ApprovalAuthority::open(temp.path()).unwrap();
    let token = reopened
        .authorize(
            &ApprovalCheck::new(
                ApprovalKind::EngineeringConstraintRelaxation,
                "req-set-1",
                "7",
                digest,
            ),
            now + 2,
        )
        .unwrap();
    assert_eq!(token.request_id(), request.id);
    let entries = reopened.audit(Some(&request.id)).unwrap();
    assert_eq!(entries.len(), 2, "request + decision are both audited");
    assert_eq!(entries[0].seq, 1);
    assert_eq!(entries[1].seq, 2);
    assert_eq!(entries[1].prev_digest, entries[0].digest);
    assert_eq!(reopened.list().unwrap().len(), 1);
}

#[test]
fn tampered_or_truncated_ledger_is_refused_not_read_as_absence() {
    let temp = tempfile::tempdir().unwrap();
    let now = faktor_approval::unix_now_ms();
    let (authority, _key) = authority(temp.path());
    authority
        .propose(draft("agent-1", &"f".repeat(64), "7"), now)
        .unwrap();
    let path = faktor_approval::ledger_path(temp.path());
    let mut text = std::fs::read_to_string(&path).unwrap();
    text = text.replacen("\"subject\":\"req-set-1\"", "\"subject\":\"forged-set\"", 1);
    std::fs::write(&path, text).unwrap();
    let error = ApprovalAuthority::open(temp.path())
        .unwrap()
        .list()
        .unwrap_err();
    assert!(matches!(error, ApprovalError::Store(_)), "{error}");
}

#[test]
fn exactly_one_decision_and_duplicate_proposals_are_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let (authority, key) = authority(temp.path());
    let now = faktor_approval::unix_now_ms();
    let digest = "1".repeat(64);
    let first_draft = draft("agent-1", &digest, "7");
    let second_draft = first_draft.clone();
    let first = authority.propose(first_draft, now).unwrap();
    let second = authority.propose(second_draft, now).unwrap();
    assert_eq!(first.id, second.id);
    authority
        .decide(
            &first.id,
            Principal::new("quality-director", PrincipalKind::HumanOperator).unwrap(),
            &key,
            DecisionOutcome::Approved,
            "ok".to_string(),
            now + 1,
        )
        .unwrap();
    assert!(matches!(
        authority
            .decide(
                &first.id,
                Principal::new("quality-director", PrincipalKind::HumanOperator).unwrap(),
                &key,
                DecisionOutcome::Denied {
                    reason: "changed mind".to_string()
                },
                "flip".to_string(),
                now + 2,
            )
            .unwrap_err(),
        ApprovalError::Conflict(_)
    ));
}

#[test]
fn racing_proposers_share_one_ledger_entry_without_corruption() {
    let temp = tempfile::tempdir().unwrap();
    let (_authority, _key) = authority(temp.path());
    let now = faktor_approval::unix_now_ms();
    let draft = draft("agent-1", &"9".repeat(64), "7");
    let mut handles = Vec::new();
    for _ in 0..8 {
        let path = temp.path().to_path_buf();
        let draft = draft.clone();
        handles.push(std::thread::spawn(move || {
            let authority = ApprovalAuthority::open(&path).unwrap();
            authority.propose(draft, now).unwrap().id
        }));
    }
    let ids: std::collections::BTreeSet<String> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(ids.len(), 1, "all identical proposals collapse to one id");
    let authority = ApprovalAuthority::open(temp.path()).unwrap();
    assert_eq!(authority.list().unwrap().len(), 1);
    assert_eq!(authority.audit(None).unwrap().len(), 1);
}

#[test]
fn expired_approval_is_refused_even_after_a_human_approved_it() {
    let temp = tempfile::tempdir().unwrap();
    let (authority, key) = authority(temp.path());
    let now = faktor_approval::unix_now_ms();
    let digest = "2".repeat(64);
    let mut draft = draft("agent-1", &digest, "7");
    draft.expires_at_ms = now + 1_500;
    let request = authority.propose(draft, now).unwrap();
    authority
        .decide(
            &request.id,
            Principal::new("quality-director", PrincipalKind::HumanOperator).unwrap(),
            &key,
            DecisionOutcome::Approved,
            "ok".to_string(),
            now + 1,
        )
        .unwrap();
    let check = ApprovalCheck::new(
        ApprovalKind::EngineeringConstraintRelaxation,
        "req-set-1",
        "7",
        digest,
    );
    let error = authority.authorize(&check, now + 5_000).unwrap_err();
    assert!(matches!(error, ApprovalError::Expired { .. }), "{error}");
}

#[test]
fn committed_head_detects_tail_truncation_of_the_ledger() {
    let temp = tempfile::tempdir().unwrap();
    let (authority, _key) = authority(temp.path());
    let now = faktor_approval::unix_now_ms();
    authority
        .propose(draft("agent-1", &"3".repeat(64), "7"), now)
        .unwrap();
    authority
        .propose(draft("agent-1", &"4".repeat(64), "8"), now + 1)
        .unwrap();
    let ledger = faktor_approval::ledger_path(temp.path());
    let text = std::fs::read_to_string(&ledger).unwrap();
    let first = text.lines().next().expect("first entry");
    std::fs::write(&ledger, format!("{first}\n")).unwrap();
    let error = authority.authorize(
        &ApprovalCheck::new(
            ApprovalKind::EngineeringConstraintRelaxation,
            "req-set-1",
            "7",
            "3".repeat(64),
        ),
        now + 2,
    );
    assert!(
        matches!(error, Err(ApprovalError::Store(_))),
        "a tail-truncated ledger must be refused, not read as a shorter history: {error:?}"
    );
    assert!(temp.path().join("approval/ledger.head").exists());
}

#[test]
fn status_and_list_refuse_decisions_whose_operator_is_no_longer_registered() {
    let temp = tempfile::tempdir().unwrap();
    let (authority, key) = authority(temp.path());
    let now = faktor_approval::unix_now_ms();
    let digest = "5".repeat(64);
    let request = authority
        .propose(draft("agent-1", &digest, "7"), now)
        .unwrap();
    authority
        .decide(
            &request.id,
            Principal::new("quality-director", PrincipalKind::HumanOperator).unwrap(),
            &key,
            DecisionOutcome::Approved,
            "reviewed".to_string(),
            now + 1,
        )
        .unwrap();
    let registry = temp
        .path()
        .join(faktor_approval::OPERATOR_DIRECTORY)
        .join(faktor_approval::OPERATOR_REGISTRY_FILE);
    std::fs::remove_file(&registry).unwrap();
    let status = authority.status(&request.id);
    assert!(
        matches!(status, Err(ApprovalError::UntrustedOperator { .. })),
        "a decision from a revoked operator must not be reported as approved: {status:?}"
    );
    assert!(matches!(
        authority.list(),
        Err(ApprovalError::UntrustedOperator { .. })
    ));
    assert!(matches!(
        authority.authorize(
            &ApprovalCheck::new(
                ApprovalKind::EngineeringConstraintRelaxation,
                "req-set-1",
                "7",
                digest,
            ),
            now + 2,
        ),
        Err(ApprovalError::UntrustedOperator { .. })
    ));
}

#[cfg(unix)]
#[test]
fn operator_seed_is_owner_only_and_never_serialized_into_the_ledger() {
    use std::os::unix::fs::MetadataExt as _;
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().unwrap();
    let key = OperatorKey::from_seed([7u8; 32]);
    let path = faktor_approval::save_operator_key(temp.path(), "ops", &key, false).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        faktor_approval::load_operator_key(&path).is_err(),
        "a group/world-readable seed must be refused"
    );
    let debug = format!("{key:?}");
    let seed_hex = "07".repeat(32);
    assert!(!debug.contains(&seed_hex), "Debug must not leak the seed");
    let authority = ApprovalAuthority::open(temp.path()).unwrap();
    let now = faktor_approval::unix_now_ms();
    let request = authority
        .propose(draft("agent-1", &"6".repeat(64), "7"), now)
        .unwrap();
    let registry = temp
        .path()
        .join(faktor_approval::OPERATOR_DIRECTORY)
        .join(faktor_approval::OPERATOR_REGISTRY_FILE);
    std::fs::remove_file(&registry).ok();
    faktor_approval::register_operator_key(temp.path(), "ops", &key, now).unwrap();
    authority
        .decide(
            &request.id,
            Principal::new("ops", PrincipalKind::HumanOperator).unwrap(),
            &key,
            DecisionOutcome::Approved,
            "reviewed".to_string(),
            now + 1,
        )
        .unwrap();
    let ledger = std::fs::read_to_string(faktor_approval::ledger_path(temp.path())).unwrap();
    assert!(!ledger.contains(&seed_hex));
}

#[test]
#[ignore = "[perf] approval ledger verify, decide and authorize"]
fn perf_approval_ledger_verify_decide_authorize() {
    let temp = tempfile::tempdir().unwrap();
    let (authority, key) = authority(temp.path());
    let now = faktor_approval::unix_now_ms();
    let count: usize = 200;

    let started = std::time::Instant::now();
    let mut requests = Vec::with_capacity(count);
    for index in 0..count {
        let digest = format!("{index:064x}");
        requests.push(
            authority
                .propose(
                    draft("agent-1", &digest, &format!("rev-{index}")),
                    now + index as u64,
                )
                .unwrap(),
        );
    }
    let propose = started.elapsed();

    let started = std::time::Instant::now();
    for (index, request) in requests.iter().enumerate() {
        authority
            .decide(
                &request.id,
                Principal::new("quality-director", PrincipalKind::HumanOperator).unwrap(),
                &key,
                DecisionOutcome::Approved,
                "reviewed".to_string(),
                now + 10_000 + index as u64,
            )
            .unwrap();
    }
    let decide = started.elapsed();

    let last = requests.last().unwrap();
    let check = ApprovalCheck::new(
        ApprovalKind::EngineeringConstraintRelaxation,
        last.subject.clone(),
        last.bound_revision.clone(),
        last.bound_digest.clone(),
    );
    let started = std::time::Instant::now();
    authority.authorize(&check, now + 20_000).unwrap();
    let authorize = started.elapsed();

    let started = std::time::Instant::now();
    let entries = faktor_approval::ApprovalLedger::load(temp.path())
        .unwrap()
        .entries
        .len();
    let verify = started.elapsed();
    assert_eq!(entries, count * 2);
    eprintln!(
        "perf approval ledger: {count} proposals in {propose:?} ({:?}/op), {count} decisions in {decide:?} ({:?}/op), authorize {authorize:?}, full chain verify {verify:?} ({} entries)",
        propose / count as u32,
        decide / count as u32,
        entries
    );
    assert!(authorize < std::time::Duration::from_millis(500));
}

#[test]
fn replayed_or_reassigned_decisions_fail_signature_and_operator_binding() {
    let temp = tempfile::tempdir().unwrap();
    let (authority, key) = authority(temp.path());
    let now = faktor_approval::unix_now_ms();
    let digest = "8".repeat(64);
    let request = authority
        .propose(draft("agent-1", &digest, "7"), now)
        .unwrap();
    let decision = authority
        .decide(
            &request.id,
            Principal::new("quality-director", PrincipalKind::HumanOperator).unwrap(),
            &key,
            DecisionOutcome::Approved,
            "reviewed".to_string(),
            now + 1,
        )
        .unwrap();
    let trusted: std::collections::BTreeSet<String> =
        [decision.signature.key_id.clone()].into_iter().collect();

    let mut replayed = decision.clone();
    replayed.request_id = format!("apr-{}", "9".repeat(32));
    replayed.request_digest = "a".repeat(64);
    assert!(matches!(
        replayed.verify(Some(&trusted)),
        Err(ApprovalError::SignatureMismatch(_))
    ));

    // A different registered human key cannot sign in the first human's name.
    let other = OperatorKey::generate();
    faktor_approval::save_operator_key(temp.path(), "other-human", &other, false).unwrap();
    faktor_approval::register_operator_key(temp.path(), "other-human", &other, now).unwrap();
    let error = authority
        .decide(
            &request.id,
            Principal::new("quality-director", PrincipalKind::HumanOperator).unwrap(),
            &other,
            DecisionOutcome::Approved,
            "impersonation".to_string(),
            now + 2,
        )
        .unwrap_err();
    assert!(
        matches!(error, ApprovalError::UntrustedOperator { .. }),
        "{error}"
    );

    // An approval for another subject cannot authorize this one.
    let wrong_subject = ApprovalCheck::new(
        ApprovalKind::EngineeringConstraintRelaxation,
        "other-set",
        "7",
        digest,
    );
    assert!(matches!(
        authority.authorize(&wrong_subject, now + 3),
        Err(ApprovalError::Stale { .. } | ApprovalError::ApprovalRequired { .. })
    ));
}
