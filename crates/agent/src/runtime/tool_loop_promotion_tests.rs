//! `runtime::tool_loop_promotion_tests`: the deferred promotion signal
//! tests, split from `tool_loop_tests` by the source-size ceiling (the
//! same fixtures, oracle and isolation as the file it came from).
// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir

use super::tool_loop_tests::assert_ask_refusal_answered;
use super::*;
use crate::runtime::tests::*;
use crate::*;

// ================= deferred promotion signal (mid-turn escalation) ==========
//
// The ONE deterministic seam for a conversational turn that discovers it
// needs engineering: the reserved `promotion_request` tool signal,
// intercepted BEFORE any execution, answered with the typed
// `promotion_deferred` refusal + a durable pending marker, honored by the
// NEXT admission (see task_executor_tests::tests_promotion for the
// re-entry side: shadow/contract rows, no second agent, no duplicate
// turn/charge).

/// Write this turn's durable ask-execution row (the promotion signal's
/// only legal surface), exactly like task admission does.
fn mark_ask_turn(handle: &faktor_session::SessionHandle, op: OpId) {
    let row = faktor_protocol::TurnExecution {
        execution: faktor_protocol::ExecutionKind::Ask,
        attachments: Vec::new(),
    };
    handle
        .upsert_memory_fact(
            faktor_protocol::TURN_EXECUTION_ROW_KIND,
            &faktor_protocol::turn_execution_row_key(op.raw()),
            &row.encode().unwrap(),
        )
        .unwrap();
}

/// The session's armed pending-promotion marker, if any.
fn armed_marker(
    handle: &faktor_session::SessionHandle,
) -> Option<faktor_protocol::TurnPromotionRow> {
    handle
        .memory_fact_get(
            faktor_protocol::PROMOTION_PENDING_ROW_KIND,
            faktor_protocol::PROMOTION_PENDING_ROW_KEY,
        )
        .unwrap()
        .map(|value| faktor_protocol::TurnPromotionRow::decode(&value).unwrap())
}

/// A REGISTERED executable under the reserved promotion name — the trap:
/// the registry can never be what answers the signal. Its CANARY
/// description must never reach the model-visible bundle either (the
/// protocol-owned spec replaces it).
fn promotion_registry_trap(executions: Arc<std::sync::atomic::AtomicUsize>) -> Tool {
    Tool {
        name: faktor_protocol::PROMOTION_REQUEST_TOOL.into(),
        description: "CANARY trap: never execute the promotion signal".into(),
        input_schema: serde_json::json!({"type": "object"}),
        resource_class: faktor_core::resource::ResourceClass::Cpu,
        effects: faktor_provider::ToolEffects::CONSERVATIVE,
        capability: None,
        recovery_hint: RecoveryHint::UnknownEffect,
        path_args: vec![],
        execute: Arc::new(move |_ctx, _args| {
            let executions = executions.clone();
            Box::pin(async move {
                executions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ToolOutcome::default())
            })
        }),
    }
}

/// Promotion signal on a conversational turn: typed `promotion_deferred`,
/// the durable marker armed with the bounded objective, ZERO executions
/// (the registered trap dies at interception — before permission hop,
/// scheduler and provider-tool execution), no permission event, and the
/// durable cross-turn loop window stays untouched (a control-plane answer
/// is never a task failure). The ask bundle carries EXACTLY the
/// protocol-owned signal — the registry trap's CANARY spec is replaced.
#[tokio::test]
async fn promotion_signal_on_an_ask_defers_types_refusally_and_arms_the_marker() {
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "p1".into(),
                name: faktor_protocol::PROMOTION_REQUEST_TOOL.into(),
                input: serde_json::json!({"objective": "fix the retry parser"}),
            },
            ScriptedResponse::End,
        ]),
        vec![promotion_registry_trap(executions.clone())],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let receipt = runtime
        .submit(session, "What is a mutex?", &[])
        .expect("ask admission");
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    mark_ask_turn(&handle, receipt.op_id);

    // The ask bundle offers EXACTLY the protocol-owned signal: the
    // registry's CANARY spec under the reserved name is replaced, and the
    // signal is the only non-read tool an ask may name. (The offer also
    // requires a tool-capable model — a non-tool model's request carries
    // no tools at all.)
    let caps = ModelCapabilities {
        tools: true,
        ..Default::default()
    };
    let bundle = runtime.tools_bundle_for_turn(&handle, &[], &caps);
    let names: Vec<&str> = bundle.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        vec![faktor_protocol::PROMOTION_REQUEST_TOOL],
        "{names:?}"
    );
    assert!(
        !bundle.tools[0].description.contains("CANARY"),
        "the registry trap must never be the offered spec: {}",
        bundle.tools[0].description
    );

    let op_raw = receipt.op_id.raw();
    let outcome = runtime.drive_receipt(&handle, receipt, None).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_ask_refusal_answered(
        &runtime,
        session,
        "p1",
        faktor_protocol::PROMOTION_DEFERRED_TAG,
        "promotion deferred",
        &executions,
    );
    // The durable arm: exact objective, bound-checked row, THIS turn's op.
    let marker = armed_marker(&handle).expect("the deferral must arm the marker");
    assert_eq!(marker.objective, "fix the retry parser");
    assert_eq!(marker.turn_op, op_raw);
    // No permission hop ever touched the model-visible permission surface.
    let events = handle.events_range(1, None).unwrap();
    assert!(
        !events
            .iter()
            .any(|e| e.kind == faktor_core::event::EventKind::ToolRequested),
        "the intercepted signal must never reach the permission hop"
    );
    // The control-plane answer never enters the durable cross-turn loop
    // window (failures-fold exclusion): a later turn's legitimate promotion
    // must never be punished as a repeated failing call.
    let counts = runtime
        .deps
        .session
        .store()
        .loop_signal_counts(session)
        .unwrap();
    assert!(
        counts.iter().all(|(key, _)| !key.contains("promotion")),
        "promotion outcomes must stay out of durable loop signals: {counts:?}"
    );
    assert!(
        ledger_known_failures(&handle)
            .iter()
            .all(|line| !line.contains("promotion_request")),
        "promotion outcomes must not fold into known failures"
    );
}

/// ONE promotion per turn, enforced durably: a second `promotion_request`
/// in the same turn is typed-refused with `promotion_already_requested`
/// and NEVER clobbers the first armed objective — both calls answered,
/// none executed, no permission hop.
#[tokio::test]
async fn second_promotion_in_one_turn_is_typed_refused_and_keeps_the_first_objective() {
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "first".into(),
                name: faktor_protocol::PROMOTION_REQUEST_TOOL.into(),
                input: serde_json::json!({"objective": "fix the retry parser"}),
            },
            ScriptedResponse::ToolCall {
                id: "second".into(),
                name: faktor_protocol::PROMOTION_REQUEST_TOOL.into(),
                input: serde_json::json!({"objective": "rewrite everything"}),
            },
            ScriptedResponse::End,
        ]),
        vec![promotion_registry_trap(executions.clone())],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let receipt = runtime
        .submit(session, "hmm, interesting approach", &[])
        .expect("ask admission");
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    mark_ask_turn(&handle, receipt.op_id);
    let op_raw = receipt.op_id.raw();
    let outcome = runtime.drive_receipt(&handle, receipt, None).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);

    assert_ask_refusal_answered(
        &runtime,
        session,
        "first",
        faktor_protocol::PROMOTION_DEFERRED_TAG,
        "promotion deferred",
        &executions,
    );
    assert_ask_refusal_answered(
        &runtime,
        session,
        "second",
        faktor_protocol::PROMOTION_ALREADY_REQUESTED_TAG,
        "one promotion per turn",
        &executions,
    );
    let marker = armed_marker(&handle).expect("armed by the first call");
    assert_eq!(
        marker.objective, "fix the retry parser",
        "the second attempt must never replace the armed objective"
    );
    assert_eq!(marker.turn_op, op_raw);
    let events = handle.events_range(1, None).unwrap();
    assert!(
        !events
            .iter()
            .any(|e| e.kind == faktor_core::event::EventKind::ToolRequested),
        "neither attempt may reach the permission hop"
    );
}

/// DOCUMENTED allowed point: the signal is accepted at ANY point of its
/// conversational turn (deferral changes no mid-turn authority — there is
/// deliberately no pre-first-tool restriction). A read tool executes in
/// the SAME batch, the turn CONTINUES, and the promotion still arms; the
/// registry trap still never runs.
#[tokio::test]
async fn promotion_signal_after_an_executed_tool_is_accepted_per_the_documented_rule() {
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "r".into(),
                name: "echo".into(),
                input: serde_json::json!({"text": "hi"}),
            },
            ScriptedResponse::ToolCall {
                id: "p".into(),
                name: faktor_protocol::PROMOTION_REQUEST_TOOL.into(),
                input: serde_json::json!({"objective": "extract the retry policy"}),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ]),
        vec![echo_tool(), promotion_registry_trap(executions.clone())],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let receipt = runtime
        .submit(session, "explain the current retry flow", &[])
        .expect("ask admission");
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    mark_ask_turn(&handle, receipt.op_id);
    let op_raw = receipt.op_id.raw();
    let outcome = runtime.drive_receipt(&handle, receipt, None).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);

    // The read tool EXECUTED (the turn continued past the batch) …
    let (echo_excerpt, echo_exit) = tool_result_for(&handle, "r").expect("echo answered");
    assert_eq!(echo_exit, Some(0), "{echo_excerpt}");
    // … and the promotion signal in the same/any later batch still defers
    // and arms: allowed at any point of the conversational turn. (The
    // shared ask-refusal helper cannot be used here: it pins the ABSENCE
    // of every ToolStarted event, and this turn legitimately started the
    // echo run.)
    let (promo_excerpt, promo_exit) =
        tool_result_for(&handle, "p").expect("the signal is answered");
    assert_eq!(promo_exit, Some(1), "a denial is an error result");
    assert!(
        promo_excerpt.contains(&format!(
            "tool call denied ({})",
            faktor_protocol::PROMOTION_DEFERRED_TAG
        )),
        "typed deferral tag missing: {promo_excerpt}"
    );
    assert!(
        promo_excerpt.contains("promotion deferred"),
        "the deferral reason must reach the model: {promo_excerpt}"
    );
    assert!(
        dangling_tool_calls(&handle).is_empty(),
        "the signal must be answered, never dangle"
    );
    assert_eq!(
        executions.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the registered trap never executes, even beside an executed tool"
    );
    let marker = armed_marker(&handle).expect("armed after the executed tool");
    assert_eq!(marker.objective, "extract the retry policy");
    assert_eq!(marker.turn_op, op_raw);
}

/// Engineering-turn isolation: without the durable ask-execution row the
/// reserved name is STRIPPED from the bundle, a stale call is typed-refused
/// `not_in_active_bundle`, and NOTHING arms — the promotion surface exists
/// only on conversational turns (the trap's CANARY spec never rides the
/// wire either).
#[tokio::test]
async fn promotion_signal_on_an_engineering_turn_never_arms() {
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "t".into(),
                name: faktor_protocol::PROMOTION_REQUEST_TOOL.into(),
                input: serde_json::json!({"objective": "promote an engineering turn"}),
            },
            ScriptedResponse::End,
        ]),
        vec![promotion_registry_trap(executions.clone())],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let receipt = runtime
        .submit(session, "implement the retry policy", &[])
        .expect("admission");
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    // NO ask-execution row: the documented task default.
    let outcome = runtime.drive_receipt(&handle, receipt, None).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);

    assert_ask_refusal_answered(
        &runtime,
        session,
        "t",
        "not_in_active_bundle",
        "not part of the active tool bundle",
        &executions,
    );
    assert!(
        armed_marker(&handle).is_none(),
        "an engineering turn must never arm a promotion"
    );
    assert_eq!(
        executions.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the trap must never execute on an engineering turn either"
    );
}

/// Strict-DTO refusals: every hostile payload shape is typed
/// `promotion_invalid`, NOTHING arms, and a PRE-ARMED marker from another
/// turn is never clobbered by a malformed attempt.
#[tokio::test]
async fn promotion_signal_strict_dto_refuses_hostile_payloads_without_clobbering() {
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let oversize = "x".repeat(faktor_protocol::MAX_PROMOTION_OBJECTIVE_BYTES + 1);
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "m1".into(),
                name: faktor_protocol::PROMOTION_REQUEST_TOOL.into(),
                input: serde_json::json!({}),
            },
            ScriptedResponse::ToolCall {
                id: "m2".into(),
                name: faktor_protocol::PROMOTION_REQUEST_TOOL.into(),
                input: serde_json::json!({"objective": 7}),
            },
            ScriptedResponse::ToolCall {
                id: "m3".into(),
                name: faktor_protocol::PROMOTION_REQUEST_TOOL.into(),
                input: serde_json::json!({"objective": "fine", "scope": "workspace"}),
            },
            ScriptedResponse::ToolCall {
                id: "m4".into(),
                name: faktor_protocol::PROMOTION_REQUEST_TOOL.into(),
                input: serde_json::json!({"objective": oversize}),
            },
            ScriptedResponse::End,
        ]),
        vec![promotion_registry_trap(executions.clone())],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let receipt = runtime
        .submit(session, "so about that", &[])
        .expect("ask admission");
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    mark_ask_turn(&handle, receipt.op_id);
    // A marker armed by an EARLIER turn is present; the hostile attempts
    // must leave it byte-identical.
    let prior =
        faktor_protocol::TurnPromotionRow::new("keep me", receipt.op_id.raw().wrapping_add(7), 1)
            .unwrap();
    handle
        .upsert_memory_fact(
            faktor_protocol::PROMOTION_PENDING_ROW_KIND,
            faktor_protocol::PROMOTION_PENDING_ROW_KEY,
            &prior.encode().unwrap(),
        )
        .unwrap();

    let outcome = runtime.drive_receipt(&handle, receipt, None).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    for call in ["m1", "m2", "m3", "m4"] {
        assert_ask_refusal_answered(
            &runtime,
            session,
            call,
            faktor_protocol::PROMOTION_INVALID_TAG,
            "promotion request refused",
            &executions,
        );
    }
    assert_eq!(
        armed_marker(&handle).expect("the prior arm survives"),
        prior,
        "a malformed promotion must never arm or clobber the marker"
    );
    assert_eq!(
        executions.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "hostile payloads must never reach an executable"
    );
}

/// A CORRUPT pending marker never bricks the signal: the fresh valid arm
/// overwrites it (loud log) and the turn gets a real deferred result.
#[tokio::test]
async fn corrupt_pending_marker_is_healed_by_a_fresh_valid_arm() {
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "p".into(),
                name: faktor_protocol::PROMOTION_REQUEST_TOOL.into(),
                input: serde_json::json!({"objective": "repair the marker"}),
            },
            ScriptedResponse::End,
        ]),
        vec![promotion_registry_trap(executions.clone())],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let receipt = runtime.submit(session, "hm", &[]).expect("ask admission");
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    mark_ask_turn(&handle, receipt.op_id);
    let op_raw = receipt.op_id.raw();
    handle
        .upsert_memory_fact(
            faktor_protocol::PROMOTION_PENDING_ROW_KIND,
            faktor_protocol::PROMOTION_PENDING_ROW_KEY,
            "{not json",
        )
        .unwrap();

    let outcome = runtime.drive_receipt(&handle, receipt, None).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_ask_refusal_answered(
        &runtime,
        session,
        "p",
        faktor_protocol::PROMOTION_DEFERRED_TAG,
        "promotion deferred",
        &executions,
    );
    let marker = armed_marker(&handle).expect("healed by the fresh arm");
    assert_eq!(marker.objective, "repair the marker");
    assert_eq!(marker.turn_op, op_raw);
}

/// Enablement stays the LAST word over the reserved signal: a deny-listed
/// promotion is dropped from the offered bundle and a stale call lands on
/// the ordinary typed membership refusal with NOTHING armed.
#[tokio::test]
async fn disabled_promotion_signal_is_refused_by_enablement_and_never_arms() {
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "p".into(),
                name: faktor_protocol::PROMOTION_REQUEST_TOOL.into(),
                input: serde_json::json!({"objective": "sneak past enablement"}),
            },
            ScriptedResponse::End,
        ]),
        vec![promotion_registry_trap(executions.clone())],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let receipt = runtime
        .submit(session, "What is a mutex?", &[])
        .expect("ask admission");
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    mark_ask_turn(&handle, receipt.op_id);
    handle
        .upsert_memory_fact(
            TOOL_ENABLEMENT_FACT_KIND,
            TOOL_ENABLEMENT_FACT_KEY,
            &serde_json::json!({"allow": ["read_file"], "deny": []}).to_string(),
        )
        .unwrap();
    let bundle = runtime.tools_bundle_for_turn(
        &handle,
        &[],
        &ModelCapabilities {
            tools: true,
            ..Default::default()
        },
    );
    assert!(
        bundle.tools.is_empty(),
        "enablement must drop the reserved signal: {:?}",
        bundle.tools
    );

    let outcome = runtime.drive_receipt(&handle, receipt, None).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_ask_refusal_answered(
        &runtime,
        session,
        "p",
        "not_in_active_bundle",
        "not part of the active tool bundle",
        &executions,
    );
    assert!(
        armed_marker(&handle).is_none(),
        "enablement denial must never arm a promotion"
    );
}

/// The ask predicate's ONE named exception is exactly the reserved signal:
/// it is the only NON-read-only spec an ask may carry (it declares
/// `external_mutation` honestly), and every other mutating tool stays
/// refused — the exception cannot generalize.
#[test]
fn the_promotion_signal_is_the_only_non_read_tool_an_ask_may_name() {
    let signal: faktor_provider::ToolSpec = serde_json::from_value(serde_json::json!({
        "name": faktor_protocol::PROMOTION_REQUEST_TOOL,
        "description": "d",
        "input_schema": { "type": "object" }
        // no `effects`: decodes CONSERVATIVE (all-mutating) — the name
        // exception is the ONLY reason an ask may carry it.
    }))
    .unwrap();
    assert!(
        signal.effects == faktor_provider::ToolEffects::CONSERVATIVE,
        "fixture sanity: the signal spec is non-read-only"
    );
    assert!(
        crate::runtime::tool_loop::ask_may_run(&signal),
        "the reserved signal must be the ask bundle's only non-read tool"
    );
    let other: faktor_provider::ToolSpec = serde_json::from_value(serde_json::json!({
        "name": "webhook_deliver",
        "description": "d",
        "input_schema": { "type": "object" }
    }))
    .unwrap();
    assert!(
        !crate::runtime::tool_loop::ask_may_run(&other),
        "no other non-read tool may ride the ask bundle through the exception"
    );
}

/// The denial tags are STABLE machine vocabulary and promotion outcomes
/// are classified as control-plane signals (excluded from the failures
/// fold) while the membership refusal keeps its ordinary failure class.
#[test]
fn promotion_denial_tags_are_stable_and_classified_as_control_plane_signals() {
    use crate::runtime::tool_loop::ToolDenialKind;
    assert_eq!(
        ToolDenialKind::PromotionDeferred.tag(),
        faktor_protocol::PROMOTION_DEFERRED_TAG
    );
    assert_eq!(
        ToolDenialKind::PromotionAlreadyRequested.tag(),
        faktor_protocol::PROMOTION_ALREADY_REQUESTED_TAG
    );
    assert_eq!(
        ToolDenialKind::PromotionInvalid.tag(),
        faktor_protocol::PROMOTION_INVALID_TAG
    );
    assert_eq!(
        ToolDenialKind::PromotionNotApplicable.tag(),
        "promotion_not_applicable"
    );
    for kind in [
        ToolDenialKind::PromotionDeferred,
        ToolDenialKind::PromotionAlreadyRequested,
        ToolDenialKind::PromotionInvalid,
        ToolDenialKind::PromotionNotApplicable,
    ] {
        assert!(kind.is_promotion_signal(), "{:?}", kind);
    }
    for kind in [
        ToolDenialKind::NotInActiveBundle,
        ToolDenialKind::UnknownTool,
        ToolDenialKind::PermissionDenied,
    ] {
        assert!(!kind.is_promotion_signal(), "{:?}", kind);
    }
}
