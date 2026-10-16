//! Audit-tail tests split from `verification_attribution_tests.rs` to
//! stay under the source-size ceiling.

use super::*;

/// Audit item 3: verification evidence is a TYPED admission. Only a
/// VerificationOpinion model output can author review evidence; a
/// context-compression summary or an ephemeral output is refused typed,
/// and a durable row is only admitted when it carries an evidence-authoring
/// provenance tag.
#[test]
fn review_evidence_admits_only_verification_opinion_and_tagged_durable_rows() {
    let compaction = ModelOutput::new("summary", OutputTrust::ContextCompression, 41);
    let ephemeral = ModelOutput::new("title", OutputTrust::Ephemeral, 42);
    let opinion = ModelOutput::new(
        "{\"verdict\":\"pass\"}",
        OutputTrust::VerificationOpinion,
        43,
    );
    let implementation = ModelOutput::new("patch", OutputTrust::Implementation, 44);

    // Negative: compression/ephemeral/implementation can never author
    // review evidence.
    for refused in [&compaction, &ephemeral, &implementation] {
        let err = ReviewEvidence::from_review_output(refused, serde_json::json!({}))
            .expect_err("only a verification opinion may author review evidence");
        assert_eq!(err.trust, refused.trust);
        assert!(err.to_string().contains(refused.trust.provenance_tag()));
    }
    // Positive: the opinion is admitted and the provenance is stamped so a
    // durable round-trip can re-admit it.
    let admitted =
        ReviewEvidence::from_review_output(&opinion, serde_json::json!({"verdict":"pass"}))
            .expect("verification opinion is evidence");
    assert_eq!(
        admitted
            .as_value()
            .get(ReviewEvidence::OUTPUT_TRUST_KEY)
            .and_then(|v| v.as_str()),
        Some("verification_opinion")
    );
    assert_eq!(
        admitted
            .as_value()
            .get("output_call_id")
            .and_then(|v| v.as_u64()),
        Some(43)
    );
    let durable = ReviewEvidence::from_durable(admitted.clone().into_value())
        .expect("stamped durable row re-admits");
    assert_eq!(durable.as_value(), admitted.as_value());

    // Durable negatives: untagged rows and compression-tagged rows refuse.
    assert!(ReviewEvidence::from_durable(serde_json::json!({"verdict":"pass"})).is_err());
    assert!(ReviewEvidence::from_durable(serde_json::json!({
        ReviewEvidence::OUTPUT_TRUST_KEY: "context_compression",
        "verdict": "pass",
    }))
    .is_err());
    assert!(ReviewEvidence::from_durable(serde_json::json!({
        ReviewEvidence::OUTPUT_TRUST_KEY: "ephemeral",
    }))
    .is_err());

    // Deterministic-local evidence is admitted only when it does not claim
    // a reviewer attempt; a claimed reviewer needs the admitted output.
    let local = ReviewEvidence::from_deterministic_local(serde_json::json!({
        "verdict": "pass",
        "evidence": {"structured": {"review_model": {"attempted": false}}},
    }))
    .expect("local evidence without a reviewer attempt is admitted");
    assert_eq!(
        local
            .as_value()
            .get(ReviewEvidence::OUTPUT_TRUST_KEY)
            .and_then(|v| v.as_str()),
        Some(ReviewEvidence::DETERMINISTIC_LOCAL_TAG)
    );
    assert!(ReviewEvidence::from_deterministic_local(serde_json::json!({
        "verdict": "pass",
        "evidence": {"structured": {"review_model": {"attempted": true}}},
    }))
    .is_err());
    assert!(ReviewEvidence::from_durable(serde_json::json!({
        ReviewEvidence::OUTPUT_TRUST_KEY: ReviewEvidence::DETERMINISTIC_LOCAL_TAG,
        "evidence": {"structured": {"review_model": {"attempted": true}}},
    }))
    .is_err());

    // The raw-admission helper drops untrusted values (findings-only), so a
    // compaction summary can never back criterion verdicts.
    assert!(admit_review_evidence(Some(&serde_json::json!({"verdict":"pass"}))).is_none());
    assert!(admit_review_evidence(Some(admitted.as_value())).is_some());
}

/// Authority law reproducer: a paid review call must not run under a
/// FABRICATED task identity. An unreadable durable task identity is a typed
/// refusal (pre-fix it silently fell back to `TaskId::new(1)`), and the
/// provider stream never opens.
#[tokio::test]
async fn unreadable_task_identity_refuses_the_review_before_the_provider() {
    let costly = CostReportingProvider::new(vec![(None, false)]);
    let (deps, _dir) = deps_with(costly.clone(), vec![]);
    let session = new_session(&deps);
    let handle = deps.session.get_session(session).unwrap().unwrap();
    deps.session
        .store()
        .sql_execute(&format!(
            "UPDATE session SET task_id = 0 WHERE id = {}",
            session.raw()
        ))
        .unwrap();
    assert!(
        handle.task_id().is_err(),
        "the corruption must make the task identity read fail"
    );
    let cancel = CancellationToken::new();
    let outcome = run_independent_review_call(
        &deps,
        &handle,
        "{\"package\":\"x\"}",
        &["criterion".to_string()],
        None,
        &cancel,
    )
    .await;
    assert!(
        outcome.verdict.is_none(),
        "no verdict without a durable task identity"
    );
    let reason = outcome.refused.expect("a refused review");
    assert!(reason.contains("task identity unreadable"), "{reason}");
    assert_eq!(
        costly.stream_count(),
        0,
        "no paid provider call may be issued"
    );
}

#[test]
fn rebuild_attempt_verification_matches_the_scan_on_hostile_row_sets() {
    // The per-check row lookup is a map now; the semantics must stay exactly
    // the old first-match scan: duplicates keep the FIRST row, missing /
    // cancelled / unavailable rows carry no verdict, and inline outcomes are
    // authoritative without a row.
    use faktor_session::{
        VerificationAttempt, VerificationAttemptCheck, VerificationInlineStatus, VerificationJob,
        VerificationJobState,
    };

    let check =
        |check_id: &str, inline: Option<VerificationInlineStatus>| VerificationAttemptCheck {
            check_id: check_id.to_string(),
            command: format!("ctest {check_id}"),
            inline,
        };
    let attempt = VerificationAttempt {
        task_id: 1,
        op_id: 7,
        task_revision: 3,
        workspace_root: "/tmp/rebuild-root".to_string(),
        changed: vec!["src/a.c".to_string()],
        checks: vec![
            check("inline-ok", Some(VerificationInlineStatus::Passed)),
            check("inline-bad", Some(VerificationInlineStatus::Failed)),
            check(
                "inline-unknown",
                Some(VerificationInlineStatus::Unavailable),
            ),
            check("bg-passed", None),
            check("bg-cancelled", None),
            check("bg-missing", None),
        ],
        environment_fingerprint: None,
        created_ms: 1,
    };
    let spec = |id: &str| {
        // The REAL durable spec shape (OsString fields ride serde's enum
        // representation), produced by the same serializer the runtime uses.
        serde_json::to_string(&faktor_verify::exec::CheckSpec {
            id: id.to_string(),
            kind: faktor_verify::exec::CheckKind::Test,
            category: faktor_verify::exec::CheckCategory::Quick,
            program: std::ffi::OsString::from("ctest"),
            args: vec![std::ffi::OsString::from("-R"), std::ffi::OsString::from(id)],
            cwd_rel: std::path::PathBuf::from("."),
            affects: Vec::new(),
            required: true,
        })
        .unwrap()
    };
    let outcome = |status: faktor_verify::exec::CheckRunStatus| {
        serde_json::to_string(&faktor_verify::exec::CheckOutcome {
            status,
            exit: Some(0),
            started_ms: 1,
            finished_ms: 2,
            summary: None,
            truncated: false,
        })
        .unwrap()
    };
    let row = |check_id: &str,
               state: VerificationJobState,
               result_json: Option<String>|
     -> VerificationJob {
        VerificationJob {
            id: format!("vj:1:7:{check_id}"),
            task_id: 1,
            task_revision: 3,
            workspace_root: "/tmp/rebuild-root".to_string(),
            check_id: check_id.to_string(),
            kind: "test".to_string(),
            command: format!("ctest {check_id}"),
            spec_json: spec(check_id),
            budget_ms: 600_000,
            attempt_op: 7,
            ordinal: 0,
            state,
            inline: None,
            note: None,
            op_id: None,
            result_json,
            environment_fingerprint: None,
            created_ms: 1,
            updated_ms: 2,
            finished_ms: None,
        }
    };
    let rows = vec![
        row(
            "bg-passed",
            VerificationJobState::Passed,
            Some(outcome(faktor_verify::exec::CheckRunStatus::Passed)),
        ),
        // A hostile second row for the same check after a terminal one: the
        // FIRST row must win (old `.find` semantics), never the last.
        row(
            "bg-passed",
            VerificationJobState::Failed,
            Some(outcome(faktor_verify::exec::CheckRunStatus::Failed)),
        ),
        row("bg-cancelled", VerificationJobState::Cancelled, None),
    ];
    let rebuild = rebuild_attempt_verification(&attempt, &rows);
    assert_eq!(
        rebuild
            .mirrors
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "inline-ok",
            "inline-bad",
            "inline-unknown",
            "bg-passed",
            "bg-cancelled",
            "bg-missing"
        ],
        "mirror order is the attempt's derivation order"
    );
    assert_eq!(
        rebuild.results,
        vec![
            ("inline-ok".to_string(), true),
            ("inline-bad".to_string(), false),
            ("bg-passed".to_string(), true),
        ],
        "inline outcomes stay authoritative; the first bg row wins"
    );
    assert_eq!(
        rebuild.unavailable,
        vec![
            (
                "inline-unknown".to_string(),
                "ctest inline-unknown".to_string()
            ),
            ("bg-cancelled".to_string(), "ctest bg-cancelled".to_string()),
            ("bg-missing".to_string(), "ctest bg-missing".to_string()),
        ],
        "cancelled and missing rows carry no verdict"
    );
    assert_eq!(rebuild.executed.len(), 1);
    assert_eq!(rebuild.executed[0].id, "bg-passed");
    assert_eq!(rebuild.executed[0].program, "ctest");
    assert_eq!(rebuild.executed[0].args, vec!["-R", "bg-passed"]);
    assert!(rebuild.executed[0].passed);

    // An empty row set is all-unavailable, never a silent pass.
    let empty = rebuild_attempt_verification(&attempt, &[]);
    assert!(empty.results.len() == 2 && empty.executed.is_empty());
    assert_eq!(empty.unavailable.len(), 4);
}
