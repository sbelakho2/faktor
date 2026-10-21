//! Adversarial approval hardening: bounded pending queue, untrusted review
//! text, and credential refusal at ingestion.

use faktor_approval::{
    save_operator_key, unix_now_ms, ApprovalAuthority, ApprovalDraft, ApprovalError, ApprovalKind,
    DecisionOutcome, OperatorKey, Principal, PrincipalKind, RiskLevel, MAX_APPROVAL_EVIDENCE_BYTES,
    MAX_OPEN_APPROVALS,
};

fn draft(subject: String, digest: String, expires_at_ms: u64) -> ApprovalDraft {
    ApprovalDraft {
        kind: ApprovalKind::SupplyPurchaseOrder,
        subject,
        evidence: vec!["po-review".to_string()],
        proposed_change: "release the purchase order".to_string(),
        risk: RiskLevel::High,
        requester: Principal::new("supply-agent", PrincipalKind::Agent).unwrap(),
        bound_revision: "1".to_string(),
        bound_digest: digest,
        expires_at_ms,
    }
}

fn human(authority_dir: &std::path::Path) -> (ApprovalAuthority, OperatorKey) {
    let key = OperatorKey::generate();
    save_operator_key(authority_dir, "ops", &key, false).unwrap();
    faktor_approval::register_operator_key(authority_dir, "ops", &key, unix_now_ms().max(1))
        .unwrap();
    (ApprovalAuthority::open(authority_dir).unwrap(), key)
}

#[test]
fn open_approval_queue_is_bounded_and_frees_capacity_when_decided_or_expired() {
    let dir = tempfile::tempdir().unwrap();
    let (authority, key) = human(dir.path());
    let now = unix_now_ms();
    let mut requests = Vec::new();
    for index in 0..MAX_OPEN_APPROVALS {
        requests.push(
            authority
                .propose(
                    draft(
                        format!("po-{index:03}"),
                        format!("{index:064x}"),
                        now + 600_000,
                    ),
                    now,
                )
                .unwrap(),
        );
    }
    let error = authority
        .propose(
            draft(
                "po-overflow".to_string(),
                format!("{:064x}", MAX_OPEN_APPROVALS),
                now + 600_000,
            ),
            now,
        )
        .unwrap_err();
    assert!(
        matches!(error, ApprovalError::PendingLimit { open, max } if open == MAX_OPEN_APPROVALS && max == MAX_OPEN_APPROVALS),
        "{error:?}"
    );
    assert_eq!(error.code(), "approval_pending_limit");
    assert_eq!(error.to_json()["open"], MAX_OPEN_APPROVALS);

    // Deciding one frees exactly one slot.
    authority
        .decide(
            &requests[0].id,
            Principal::new("ops", PrincipalKind::HumanOperator).unwrap(),
            &key,
            DecisionOutcome::Approved,
            "reviewed".to_string(),
            now + 1,
        )
        .unwrap();
    authority
        .propose(
            draft(
                "po-after-decision".to_string(),
                format!("{:064x}", MAX_OPEN_APPROVALS),
                now + 600_000,
            ),
            now,
        )
        .unwrap();

    // Expired requests free capacity WITHOUT a ledger mutation: a long task
    // may leave an undecided backlog that ages out.
    let expired_dir = tempfile::tempdir().unwrap();
    let expired = ApprovalAuthority::open(expired_dir.path()).unwrap();
    for index in 0..MAX_OPEN_APPROVALS {
        expired
            .propose(
                draft(
                    format!("old-{index:03}"),
                    format!("{:064x}", index),
                    now + 1,
                ),
                now,
            )
            .unwrap();
    }
    expired
        .propose(
            draft(
                "fresh".to_string(),
                format!("{:064x}", MAX_OPEN_APPROVALS),
                now + 60_000,
            ),
            now + 2,
        )
        .unwrap();
}

#[test]
fn evidence_aggregate_control_characters_and_credentials_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let authority = ApprovalAuthority::open(dir.path()).unwrap();
    let now = unix_now_ms();
    let digest = "a".repeat(64);

    let mut evidence = draft("po-evidence".to_string(), digest.clone(), now + 60_000);
    evidence.evidence = (0..5).map(|_| "e".repeat(4_096)).collect();
    assert!(evidence.evidence.iter().map(String::len).sum::<usize>() > MAX_APPROVAL_EVIDENCE_BYTES);
    let error = authority.propose(evidence, now).unwrap_err();
    assert!(error.to_string().contains("aggregate bytes"), "{error}");

    let mut control = draft("po-control".to_string(), digest.clone(), now + 60_000);
    control.subject = "po-\u{1b}[2J".to_string();
    assert!(matches!(
        authority.propose(control, now).unwrap_err(),
        ApprovalError::Invalid(_)
    ));

    let mut bidi = draft("po-bidi".to_string(), digest.clone(), now + 60_000);
    bidi.proposed_change = "release \u{202E}gpj.exe".to_string();
    assert!(matches!(
        authority.propose(bidi, now).unwrap_err(),
        ApprovalError::Invalid(_)
    ));

    let secret = "AKIAIOSFODNN7EXAMPLE";
    let mut credential = draft("po-secret".to_string(), digest.clone(), now + 60_000);
    credential.proposed_change = format!("use key {secret}");
    let error = authority.propose(credential, now).unwrap_err();
    assert!(error.to_string().contains("credential"), "{error}");
    assert!(
        !error.to_string().contains(secret),
        "the refusal must never echo the credential: {error}"
    );

    let mut revision_secret = draft("po-revision".to_string(), digest.clone(), now + 60_000);
    revision_secret.bound_revision = secret.to_string();
    assert!(matches!(
        authority.propose(revision_secret, now).unwrap_err(),
        ApprovalError::Invalid(_)
    ));

    let mut requester_secret = draft("po-requester".to_string(), digest, now + 60_000);
    requester_secret.requester = Principal::new(secret, PrincipalKind::Agent).unwrap();
    let error = authority.propose(requester_secret, now).unwrap_err();
    assert!(error.to_string().contains("credential"), "{error}");
    assert!(!error.to_string().contains(secret), "{error}");
}

#[test]
fn long_task_many_approvals_stays_bounded_and_leaves_no_debris() {
    let dir = tempfile::tempdir().unwrap();
    let (authority, key) = human(dir.path());
    let now = unix_now_ms();
    let started = std::time::Instant::now();
    for index in 0..200u32 {
        let request = authority
            .propose(
                draft(
                    format!("po-long-{index:03}"),
                    format!("{index:064x}"),
                    now + 600_000,
                ),
                now,
            )
            .unwrap();
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
    }
    let elapsed = started.elapsed();
    let views = authority.list().unwrap();
    assert_eq!(views.len(), 200);
    assert!(views.iter().all(|view| view.decision.is_some()));
    assert!(
        elapsed < std::time::Duration::from_secs(60),
        "200 propose/decide cycles took {elapsed:?}; the bounded queue must stay linear-ish"
    );
    // No temporary or per-request files: the ledger is the only durable
    // queue, and every write is append-only.
    let approval_dir = dir.path().join(faktor_approval::OPERATOR_DIRECTORY);
    let allowed = [
        "ledger.jsonl",
        ".ledger.lock",
        "ledger.head",
        "operators.json",
        "operators",
        "ops.key",
    ];
    for entry in std::fs::read_dir(&approval_dir).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().to_string();
        assert!(
            allowed.contains(&name.as_str()),
            "unexpected approval debris: {name}"
        );
    }
    eprintln!(
        "APPROVAL_LONG_TASK cycles=200 elapsed_ms={}",
        elapsed.as_millis()
    );
}

fn tool_ctx() -> faktor_agent::ToolRunCtx {
    use faktor_agent::ToolCallMode;
    use faktor_core::cancellation::CancellationToken;
    use faktor_core::id::{OpId, SessionId, TaskId, WorkspaceId, WorktreeId};
    use faktor_core::WorkspaceIdentity;
    faktor_agent::ToolRunCtx {
        session_id: SessionId::new(1),
        op_id: OpId::new(1),
        identity: WorkspaceIdentity::new(WorkspaceId::new(1), WorktreeId::new(1), TaskId::new(1)),
        cancellation: CancellationToken::new(),
        artifacts: std::sync::Arc::new(faktor_agent::ToolArtifactSink::Null),
        tool_call_mode: ToolCallMode::Native,
        workspace: None,
        edit: None,
        snapshots: None,
        sandbox: None,
        supervisor: None,
        deadline_ms: 60_000,
        permission_granted: true,
    }
}

/// The daemon-facing tool reports the bounded queue as a TYPED result
/// (`approval_pending_limit`, exit 0), not as a malformed call: a model can
/// distinguish "full, wait/decide" from "bad arguments" and never retries a
/// consequential action blindly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approval_tool_reports_a_typed_pending_limit_at_capacity() {
    use faktor_tests_production_wiring::harness::{self, Config};
    let directory = tempfile::tempdir().unwrap();
    let graph = harness::build_production_graph(&directory.path().join("data"), Config::default())
        .expect("production daemon graph");
    let tool = harness::daemon_tool(&graph, "approval_request");
    for index in 0..MAX_OPEN_APPROVALS {
        let outcome = (tool.execute)(
            tool_ctx(),
            serde_json::json!({
                "kind": "supply_purchase_order",
                "subject": format!("po-{index:03}"),
                "evidence": ["po-review"],
                "proposed_change": "release the purchase order",
                "risk": "high",
                "requester_id": "supply-agent",
                "bound_revision": "1",
                "bound_digest": format!("{index:064x}"),
                "expires_in_ms": 60_000,
            }),
        )
        .await
        .expect("proposal is a normal tool result");
        let value: serde_json::Value = serde_json::from_str(&outcome.text).unwrap();
        assert_eq!(value["status"], "approval_pending");
    }
    let outcome = (tool.execute)(
        tool_ctx(),
        serde_json::json!({
            "kind": "supply_purchase_order",
            "subject": "po-overflow",
            "evidence": ["po-review"],
            "proposed_change": "release the purchase order",
            "risk": "high",
            "requester_id": "supply-agent",
            "bound_revision": "1",
            "bound_digest": format!("{:064x}", MAX_OPEN_APPROVALS),
            "expires_in_ms": 60_000,
        }),
    )
    .await
    .expect("the cap is a typed outcome, not a transport error");
    let value: serde_json::Value = serde_json::from_str(&outcome.text).unwrap();
    assert_eq!(value["status"], "approval_pending_limit", "{value}");
    assert_eq!(value["code"], "approval_pending_limit");
    assert_eq!(value["open"], MAX_OPEN_APPROVALS);
    assert_eq!(value["max"], MAX_OPEN_APPROVALS);
    assert_eq!(outcome.exit_code, Some(0));
}
