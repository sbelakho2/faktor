//! Loopback integration tests for the Kiwi external verification provider:
//! request shape, counterexample ingestion, hostile/malformed/oversized
//! response refusals, deadlines/watchdog, auth headers, and dispatch.
//!
//! No network beyond loopback: every server binds `127.0.0.1:0` and the
//! checked egress transport is allowlisted for exactly that origin.

mod support;

use std::sync::Arc;

use faktor_core::{
    CancellationToken, ClaimState, Constraint, ContractOrigin, EngineeringContract, EvidenceRef,
    Invariant, OpId, ProofEvidenceKind, RiskHypothesis, RiskSeverity, SessionId, WorkspaceId,
};
use faktor_kiwi::{
    job_id_for, KiwiJob, KiwiProviderConfig, KiwiVerificationProvider, KIWI_SCHEMA_VERSION,
};
use faktor_provider::egress::{HttpTransport, PolicyCheckedHttpTransport};
use faktor_security::destination::DestinationPolicy;
use faktor_verify::{
    now_epoch_ms, ClaimRequest, VerificationError, VerificationOp, VerificationOutcome,
    VerificationProvider, VerificationRegistry, VerificationRequest,
};
use support::{FakeKiwi, Reply};

fn transport_for(server: &FakeKiwi) -> Arc<dyn HttpTransport> {
    Arc::new(PolicyCheckedHttpTransport::with_policy_for_tests(
        DestinationPolicy::parse_lines([&format!("http://127.0.0.1:{}", server.port())]).unwrap(),
    ))
}

fn provider(server: &FakeKiwi, timeout_ms: u64) -> Arc<KiwiVerificationProvider> {
    KiwiProviderConfig {
        endpoint: server.endpoint(),
        auth_env: None,
        timeout_ms,
        operations: None,
    }
    .build(transport_for(server))
    .expect("kiwi provider builds")
}

fn request(op: VerificationOp) -> VerificationRequest {
    VerificationRequest::new(
        OpId::new(7),
        SessionId::new(3),
        WorkspaceId::new(1),
        now_epoch_ms(),
        CancellationToken::new(),
        op,
    )
}

fn claim_request() -> VerificationRequest {
    request(VerificationOp::VerifyClaim).with_claim("refresh tokens cannot be replayed")
}

/// A valid wire reply body for one job.
fn valid_reply(job: &KiwiJob, claim_state: ClaimState) -> serde_json::Value {
    let mut claim = serde_json::json!({
        "claim_id": job.proof_obligations[0].id,
        "state": claim_state,
        "evidence": [],
    });
    if claim_state == ClaimState::Disproven {
        claim["counterexample"] = serde_json::json!("replay succeeded");
    }
    if matches!(claim_state, ClaimState::Supported | ClaimState::KnownTrue) {
        // A positive verdict carries evidence (the neutral evidence-adequacy
        // law); known_true must additionally be proof-grade.
        claim["evidence"] = serde_json::json!([{
            "kind": "review",
            "source": "kiwi.attack",
            "detail": "attack suite clean",
        }]);
        if claim_state == ClaimState::KnownTrue {
            claim["evidence_strength"] = serde_json::json!("proof");
        }
    }
    serde_json::json!({
        "v": KIWI_SCHEMA_VERSION,
        "op": job.op.as_str(),
        "job_id": job.job_id,
        "outcome": "passed",
        "summary": "verified",
        "claims": [claim],
        "evidence": [],
    })
}

fn http_reply(body: serde_json::Value) -> Reply {
    Reply::Http {
        status: 200,
        body: body.to_string(),
    }
}

#[tokio::test]
async fn submission_carries_objective_acceptance_and_contract_without_expected_results() {
    let server = FakeKiwi::start();
    let provider = provider(&server, 5_000);
    let req = claim_request();

    let contract = EngineeringContract {
        constraints: vec![Constraint {
            id: "c1".into(),
            statement: "existing sessions remain valid".into(),
            origin: ContractOrigin::User,
        }],
        invariants: vec![Invariant {
            id: "i1".into(),
            statement: "revoked sessions cannot authenticate".into(),
            origin: ContractOrigin::Policy,
        }],
        risks: vec![RiskHypothesis {
            id: "r1".into(),
            description: "race conditions".into(),
            severity: RiskSeverity::High,
            origin: ContractOrigin::Heuristic,
        }],
        proof_obligations: Vec::new(),
    };
    let job = KiwiJob::for_contract(
        &req,
        "migrate authentication to rotating refresh tokens",
        vec!["existing sessions remain valid".to_string()],
        &contract,
    )
    .unwrap();
    let claim_id = job.proof_obligations[0].id.clone();
    server.push(http_reply(serde_json::json!({
        "v": KIWI_SCHEMA_VERSION,
        "op": "verify_claim",
        "job_id": job.job_id,
        "outcome": "passed",
        "summary": "no counterexample found",
        "claims": [{"claim_id": claim_id, "state": "supported", "evidence": [
            {"kind": "review", "source": "kiwi", "detail": "attack suite clean"}
        ]}],
        "evidence": [],
    })));

    let response = provider.submit(req.clone(), job).await.unwrap();
    assert_eq!(response.outcome, VerificationOutcome::Passed);
    assert_eq!(response.provider.as_str(), "kiwi");
    assert_eq!(response.exit_code, None);

    // The request reached the endpoint as a typed, bounded frame.
    let recorded = server.requests();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].method, "POST");
    assert_eq!(recorded[0].path, "/verify");
    assert_eq!(recorded[0].header("content-type"), Some("application/json"));
    let frame: serde_json::Value = serde_json::from_str(&recorded[0].body).unwrap();
    assert_eq!(frame["v"], KIWI_SCHEMA_VERSION);
    assert_eq!(frame["op"], "verify_claim");
    let job = &frame["job"];
    assert_eq!(job["workspace_id"], 1);
    assert_eq!(job["session_id"], 3);
    assert_eq!(
        job["objective"],
        "migrate authentication to rotating refresh tokens"
    );
    assert_eq!(
        job["acceptance_criteria"][0],
        "existing sessions remain valid"
    );
    assert_eq!(
        job["constraints"][0]["statement"],
        "existing sessions remain valid"
    );
    assert_eq!(job["constraints"][0]["origin"], "user");
    assert_eq!(
        job["invariants"][0]["statement"],
        "revoked sessions cannot authenticate"
    );
    assert_eq!(job["risks"][0]["description"], "race conditions");
    assert_eq!(job["risks"][0]["severity"], "high");
    let obligations = job["proof_obligations"].as_array().unwrap();
    assert!(!obligations.is_empty());
    // The submitted obligation carries required behavior, no prior verdict.
    assert_eq!(obligations[0]["state"], "unknown");
    assert_eq!(obligations[0]["confidence_bps"], serde_json::Value::Null);
    assert!(obligations[0]["evidence_refs"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(job["claim"], "refresh tokens cannot be replayed");
    assert_eq!(
        job["max_output_bytes"],
        faktor_verify::DEFAULT_MAX_OUTPUT_BYTES as u64
    );
    // No wire field prescribes an expected result.
    let text = &recorded[0].body;
    for forbidden in ["expected", "expecte", "expectation"] {
        assert!(
            !text.contains(forbidden),
            "submission prescribes {forbidden:?}"
        );
    }
}

#[tokio::test]
async fn counterexample_ingestion_maps_into_the_response() {
    let server = FakeKiwi::start();
    let provider = provider(&server, 5_000);
    let req = claim_request();
    let job = KiwiJob::for_request(&req).unwrap();
    let claim_id = job.proof_obligations[0].id.clone();
    server.push(http_reply(serde_json::json!({
        "v": KIWI_SCHEMA_VERSION,
        "op": "verify_claim",
        "job_id": job.job_id,
        "outcome": "failed",
        "summary": "counterexample found",
        "claims": [{
            "claim_id": claim_id,
            "state": "disproven",
            "evidence": [{"kind": "test_execution", "source": "kiwi.attack", "detail": "replay test"}],
            "counterexample": "replay succeeded for token t-9",
        }],
        "evidence": [{"kind": "runtime_trace", "source": "kiwi.trace", "detail": "trace artifact"}],
    })));

    let response = provider.verify(req).await.unwrap();
    assert_eq!(response.outcome, VerificationOutcome::Failed);
    assert!(
        response.evidence.iter().any(|artifact| artifact
            .detail
            .contains("replay succeeded for token t-9")
            && artifact.kind == ProofEvidenceKind::Review),
        "the counterexample must be ingested as evidence: {:?}",
        response.evidence
    );
    assert!(
        response
            .evidence
            .iter()
            .any(|artifact| artifact.detail == "replay test"),
        "per-claim evidence must be ingested: {:?}",
        response.evidence
    );
    assert!(
        response
            .evidence
            .iter()
            .any(|artifact| artifact.detail == "trace artifact"),
        "aggregate evidence must be ingested: {:?}",
        response.evidence
    );
}

#[tokio::test]
async fn hostile_and_malformed_responses_are_refused_typed() {
    type Predicate = fn(&VerificationError) -> bool;
    let server = FakeKiwi::start();
    let provider = provider(&server, 5_000);
    let req = claim_request();
    let job = KiwiJob::for_request(&req).unwrap();

    let cases: Vec<(&str, serde_json::Value, Predicate)> = vec![
        (
            "wrong schema version",
            {
                let mut body = valid_reply(&job, ClaimState::Supported);
                body["v"] = serde_json::json!(KIWI_SCHEMA_VERSION + 1);
                body
            },
            |e| matches!(e, VerificationError::Malformed(_)),
        ),
        (
            "wrong op echo",
            {
                let mut body = valid_reply(&job, ClaimState::Supported);
                body["op"] = serde_json::json!("delta");
                body
            },
            |e| matches!(e, VerificationError::UnknownOp(_)),
        ),
        (
            "wrong job id",
            {
                let mut body = valid_reply(&job, ClaimState::Supported);
                body["job_id"] = serde_json::json!("v1:other");
                body
            },
            |e| matches!(e, VerificationError::Malformed(_)),
        ),
        (
            "claim the job never submitted",
            {
                let mut body = valid_reply(&job, ClaimState::Supported);
                body["claims"][0]["claim_id"] = serde_json::json!("po1:not-asked");
                body
            },
            |e| matches!(e, VerificationError::Malformed(_)),
        ),
        (
            "duplicate claim ids",
            {
                let mut body = valid_reply(&job, ClaimState::Supported);
                let claim = body["claims"][0].clone();
                body["claims"].as_array_mut().unwrap().push(claim);
                body
            },
            |e| matches!(e, VerificationError::Malformed(_)),
        ),
        (
            "disproof without any ground",
            {
                let mut body = valid_reply(&job, ClaimState::Disproven);
                body["claims"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("counterexample");
                body
            },
            |e| matches!(e, VerificationError::Malformed(_)),
        ),
        (
            "oversized summary",
            {
                let mut body = valid_reply(&job, ClaimState::Supported);
                body["summary"] = serde_json::json!("x".repeat(64 * 1024));
                body
            },
            |e| matches!(e, VerificationError::Malformed(_)),
        ),
        (
            "counterexample oversized",
            {
                let mut body = valid_reply(&job, ClaimState::Disproven);
                body["claims"][0]["counterexample"] = serde_json::json!("x".repeat(64 * 1024));
                body
            },
            |e| matches!(e, VerificationError::Malformed(_)),
        ),
        (
            "too many claims",
            {
                let mut body = valid_reply(&job, ClaimState::Supported);
                let claims = body["claims"].as_array_mut().unwrap();
                let mut many: Vec<serde_json::Value> = Vec::new();
                for _ in 0..faktor_kiwi::MAX_KIWI_CLAIMS + 1 {
                    many.push(claims[0].clone());
                }
                *claims = many;
                body
            },
            |e| matches!(e, VerificationError::Malformed(_)),
        ),
    ];

    for (name, body, predicate) in cases {
        server.push(http_reply(body));
        let err = provider
            .verify(req.clone())
            .await
            .expect_err("hostile response must be refused");
        assert!(predicate(&err), "case {name:?}: unexpected {err:?}");
    }

    // Malformed JSON is refused before any field is trusted.
    server.push(Reply::Http {
        status: 200,
        body: "{not json".to_string(),
    });
    let err = provider.verify(req.clone()).await.unwrap_err();
    assert!(matches!(err, VerificationError::Malformed(_)), "{err:?}");

    // Unknown op names in the wire are typed UnknownOp.
    let mut body = valid_reply(&job, ClaimState::Supported);
    body["op"] = serde_json::json!("melted");
    server.push(http_reply(body));
    let err = provider.verify(req.clone()).await.unwrap_err();
    assert!(matches!(err, VerificationError::UnknownOp(_)), "{err:?}");

    // HTTP auth failures are Refused; server faults are recoverable.
    server.push(Reply::Http {
        status: 401,
        body: "{}".to_string(),
    });
    let err = provider.verify(req.clone()).await.unwrap_err();
    assert!(matches!(err, VerificationError::Refused(_)), "{err:?}");
    server.push(Reply::Http {
        status: 503,
        body: "{}".to_string(),
    });
    let err = provider.verify(req.clone()).await.unwrap_err();
    assert!(
        matches!(err, VerificationError::Unavailable(_)),
        "a 5xx is a recoverable provider fault: {err:?}"
    );
}

#[tokio::test]
async fn oversized_response_bodies_are_refused_before_parsing() {
    let server = FakeKiwi::start();
    let provider = provider(&server, 5_000);
    let req = claim_request().with_max_output_bytes(4096);
    // 2 MiB of body against a 4 KiB output bound + 64 KiB slack: refused by
    // the budget-aware reader, never buffered past the bound.
    let body = "x".repeat(2 * 1024 * 1024);
    let mut raw = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    raw.extend_from_slice(body.as_bytes());
    server.push(Reply::Raw(raw));
    let err = provider
        .verify(req)
        .await
        .expect_err("oversized bodies must be refused");
    assert!(
        matches!(err, VerificationError::OutputTooLarge { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn caller_deadline_is_terminal_and_provider_watchdog_is_recoverable() {
    // The caller's deadline wins when it is tighter: terminal Timeout.
    let server = FakeKiwi::start();
    let deadline_provider = provider(&server, 60_000);
    server.push(Reply::Hang(std::time::Duration::from_secs(2)));
    let now = now_epoch_ms();
    let mut req = claim_request();
    req.created_ms = now;
    req.deadline_ms = Some(now + 200);
    let job = KiwiJob::for_request(&req).unwrap();
    let err = deadline_provider.submit(req, job).await.unwrap_err();
    assert!(
        matches!(err, VerificationError::Timeout(_)),
        "the caller deadline is terminal: {err:?}"
    );
    assert!(err.caller_terminal());
    assert!(err.to_string().contains("deadline"), "{err}");

    // The provider's own watchdog with no caller deadline is a recoverable
    // fault so dispatch may fail over.
    let server = FakeKiwi::start();
    let watchdog_provider = provider(&server, 200);
    server.push(Reply::Hang(std::time::Duration::from_secs(2)));
    let req = claim_request();
    let job = KiwiJob::for_request(&req).unwrap();
    let err = watchdog_provider.submit(req, job).await.unwrap_err();
    assert!(
        matches!(err, VerificationError::Unavailable(_)),
        "the provider watchdog is recoverable: {err:?}"
    );
    assert!(!err.caller_terminal());
    assert!(err.to_string().contains("watchdog"), "{err}");
}

#[tokio::test]
async fn bearer_auth_env_is_read_at_call_time_and_absence_is_unavailable() {
    let server = FakeKiwi::start();
    let var = "FAKTOR_KIWI_LOOPBACK_TEST_TOKEN";
    std::env::remove_var(var);
    let provider = KiwiProviderConfig {
        endpoint: server.endpoint(),
        auth_env: Some(var.to_string()),
        timeout_ms: 5_000,
        operations: None,
    }
    .build(transport_for(&server))
    .unwrap();
    let req = claim_request();
    let job = KiwiJob::for_request(&req).unwrap();
    server.push(http_reply(valid_reply(&job, ClaimState::Supported)));

    // Absent auth env: recoverable unavailable, no request sent.
    let err = provider.submit(req.clone(), job.clone()).await.unwrap_err();
    assert!(matches!(err, VerificationError::Unavailable(_)), "{err:?}");
    assert_eq!(server.requests().len(), 0);

    // Present: the bearer header is attached, never the token in config.
    std::env::set_var(var, "test-secret");
    server.push(http_reply(valid_reply(&job, ClaimState::Supported)));
    provider.submit(req, job).await.unwrap();
    std::env::remove_var(var);
    let recorded = server.requests();
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0].header("authorization"),
        Some("Bearer test-secret")
    );
}

#[tokio::test]
async fn require_provider_dispatch_reaches_the_configured_kiwi() {
    let server = FakeKiwi::start();
    let provider = provider(&server, 5_000);
    let mut registry = VerificationRegistry::new();
    registry.register(provider);
    assert!(registry.has_external_provider());
    assert!(registry.has_external_provider_for(VerificationOp::VerifyClaim));

    let req = claim_request().requiring_provider();
    let job = KiwiJob::for_request(&req).unwrap();
    server.push(http_reply(valid_reply(&job, ClaimState::Supported)));
    let response = registry.verify(req).await.unwrap();
    assert_eq!(response.provider.as_str(), "kiwi");
    assert_eq!(response.outcome, VerificationOutcome::Passed);
}

#[tokio::test]
async fn a_registry_without_a_kiwi_provider_refuses_a_require_provider_claim() {
    // Unconfigured => no external provider: the native baseline (absent
    // here, so the registry is empty) can never silently substitute.
    let registry = VerificationRegistry::new();
    assert!(!registry.has_external_provider());
    let err = registry
        .verify(claim_request().requiring_provider())
        .await
        .expect_err("an unconfigured external demand must refuse typed");
    assert!(matches!(err, VerificationError::Unavailable(_)), "{err:?}");
}

#[allow(dead_code)]
fn evidence_ref_bridge_is_available() {
    // The frozen `EvidenceRef` bridge exists for a later runtime wave.
    let artifact =
        faktor_verify::ArtifactRef::new(ProofEvidenceKind::Review, "kiwi", "counterexample")
            .unwrap();
    let _: EvidenceRef = artifact.to_evidence_ref();
}

// ------------------------------------------------- claim-batch round trip

/// The end-to-end round trip through the registry: A -> disproven,
/// B -> supported, C -> unknown each land on the RIGHT obligation, keyed by
/// the obligation id Faktor submitted — never a Kiwi-private id.
#[tokio::test]
async fn claim_round_trip_preserves_obligation_identity() {
    use faktor_core::contract::{AcceptedStrength, EvidenceBinding, Requirement};

    let server = FakeKiwi::start();
    let provider = provider(&server, 5_000);
    let mut registry = VerificationRegistry::new();
    registry.register(provider);

    let claim = |id: &str, contract: &str| ClaimRequest {
        obligation_id: id.to_string(),
        claim: format!("claim {id} holds"),
        evidence_kind: ProofEvidenceKind::ContractCheck,
        requirement: Requirement::Required,
        binding: EvidenceBinding::ContractIds(vec![contract.to_string()]),
        accepted_strength: AcceptedStrength::Supported,
    };
    let claims = vec![
        claim("po1:alpha", "c-alpha"),
        claim("po1:beta", "c-beta"),
        claim("po1:gamma", "c-gamma"),
    ];
    let batch = request(VerificationOp::CheckContracts);
    server.push(http_reply(serde_json::json!({
        "v": KIWI_SCHEMA_VERSION,
        "op": "check_contracts",
        "job_id": job_id_for(&batch),
        "outcome": "failed",
        "summary": "one counterexample, one support, one unknown",
        "claims": [
            {"claim_id": "po1:alpha", "state": "disproven",
             "evidence": [], "counterexample": "contract c-alpha was violated"},
            {"claim_id": "po1:beta", "state": "supported",
             "evidence": [{"kind": "contract_check", "source": "kiwi",
                           "detail": "contract c-beta observed"}], "counterexample": null},
            {"claim_id": "po1:gamma", "state": "unknown", "evidence": []}
        ],
        "evidence": [],
    })));
    let results = registry
        .verify_claims(batch.clone(), claims.clone())
        .await
        .expect("claim batch round trip");
    assert_eq!(results.len(), 3, "{results:?}");
    let by_id = |id: &str| {
        results
            .iter()
            .find(|result| result.obligation_id == id)
            .unwrap_or_else(|| panic!("no result for {id}: {results:?}"))
    };
    assert_eq!(by_id("po1:alpha").state, ClaimState::Disproven);
    assert_eq!(
        by_id("po1:alpha").counterexample,
        vec!["contract c-alpha was violated".to_string()]
    );
    assert_eq!(by_id("po1:beta").state, ClaimState::Supported);
    assert!(by_id("po1:beta")
        .evidence
        .iter()
        .any(|artifact| artifact.detail.contains("c-beta")));
    assert_eq!(by_id("po1:gamma").state, ClaimState::Unknown);
    assert!(by_id("po1:gamma").evidence.is_empty());
    for result in &results {
        assert_eq!(result.provider.as_str(), "kiwi");
    }

    // The submitted frame carried exactly the obligation ids, verbatim.
    let recorded = server.requests();
    let frame: serde_json::Value = serde_json::from_str(&recorded[0].body).unwrap();
    let ids: Vec<&str> = frame["job"]["proof_obligations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|obligation| obligation["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["po1:alpha", "po1:beta", "po1:gamma"]);
    assert_eq!(
        frame["job"]["acceptance_criteria"][0],
        "claim po1:alpha holds"
    );
    // The submitted obligation carries required behavior, no prior verdict.
    assert_eq!(frame["job"]["proof_obligations"][0]["state"], "unknown");
    assert_eq!(
        frame["job"]["proof_obligations"][0]["confidence_bps"],
        serde_json::Value::Null
    );

    // A reply that silently omits a required obligation is refused typed.
    server.push(http_reply(serde_json::json!({
        "v": KIWI_SCHEMA_VERSION,
        "op": "check_contracts",
        "job_id": job_id_for(&batch),
        "outcome": "passed",
        "summary": "supposedly complete",
        "claims": [
            {"claim_id": "po1:alpha", "state": "supported",
             "evidence": [{"kind": "contract_check", "source": "kiwi", "detail": "a"}]},
            {"claim_id": "po1:beta", "state": "supported",
             "evidence": [{"kind": "contract_check", "source": "kiwi", "detail": "b"}]}
        ],
        "evidence": [],
    })));
    let err = registry
        .verify_claims(batch, claims)
        .await
        .expect_err("the omitted required claim must be refused typed");
    match err {
        VerificationError::Malformed(detail) => {
            assert!(detail.contains("po1:gamma"), "{detail}");
        }
        other => panic!("expected typed Malformed, got {other:?}"),
    }
}

/// A positive verdict without evidence is refused by the neutral import even
/// when the provider claims a whole batch is fine.
#[tokio::test]
async fn unevidenced_positive_verdicts_are_refused_typed() {
    use faktor_core::contract::{AcceptedStrength, EvidenceBinding, Requirement};

    let server = FakeKiwi::start();
    let provider = provider(&server, 5_000);
    let mut registry = VerificationRegistry::new();
    registry.register(provider);
    let claims = vec![ClaimRequest {
        obligation_id: "po1:naked".to_string(),
        claim: "something holds".to_string(),
        evidence_kind: ProofEvidenceKind::ContractCheck,
        requirement: Requirement::Required,
        binding: EvidenceBinding::ContractIds(vec!["c1".to_string()]),
        accepted_strength: AcceptedStrength::Supported,
    }];
    let batch = request(VerificationOp::CheckContracts);
    server.push(http_reply(serde_json::json!({
        "v": KIWI_SCHEMA_VERSION,
        "op": "check_contracts",
        "job_id": job_id_for(&batch),
        "outcome": "passed",
        "summary": "supported without any ground",
        "claims": [{"claim_id": "po1:naked", "state": "supported", "evidence": []}],
        "evidence": [],
    })));
    let err = registry
        .verify_claims(batch, claims)
        .await
        .expect_err("a supported verdict without evidence is refused");
    assert!(matches!(err, VerificationError::Malformed(_)), "{err:?}");
}

/// Capability negotiation: an operation outside the negotiated subset is
/// refused typed before any request is framed, and the registry reports no
/// capable provider instead of silently substituting.
#[tokio::test]
async fn operations_outside_the_negotiated_subset_are_refused_typed() {
    let server = FakeKiwi::start();
    let provider = provider(&server, 5_000);
    assert!(
        !VerificationProvider::capabilities(provider.as_ref()).supports(VerificationOp::RunTests),
        "the conservative default never claims the native-owned test op"
    );
    let err = provider
        .verify(request(VerificationOp::RunTests))
        .await
        .expect_err("RunTests is outside the negotiated default");
    assert!(matches!(err, VerificationError::Unavailable(_)), "{err:?}");
    assert!(
        server.requests().is_empty(),
        "an unsupported operation must never reach the network"
    );

    // An explicit negotiation can serve it; then the request is framed.
    let server = FakeKiwi::start();
    let provider = KiwiProviderConfig {
        endpoint: server.endpoint(),
        auth_env: None,
        timeout_ms: 5_000,
        operations: Some(vec![VerificationOp::RunTests]),
    }
    .build(transport_for(&server))
    .unwrap();
    assert!(
        VerificationProvider::capabilities(provider.as_ref()).supports(VerificationOp::RunTests)
    );
    assert!(!VerificationProvider::capabilities(provider.as_ref())
        .supports(VerificationOp::VerifyClaim));
    let mut registry = VerificationRegistry::new();
    registry.register(provider);
    let req = request(VerificationOp::RunTests);
    let job = KiwiJob::for_request(&req).unwrap();
    server.push(http_reply(valid_reply(&job, ClaimState::Supported)));
    let response = registry.verify(req).await.unwrap();
    assert_eq!(response.provider.as_str(), "kiwi");
}
