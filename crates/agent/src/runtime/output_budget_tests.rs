//! `runtime::output_budget_tests`: adversarial certificates for Faktor-side
//! cumulative model-output enforcement.

use super::*;
use crate::runtime::fixtures_tests::*;
use crate::runtime::tests::*;
use crate::*;
use futures::StreamExt as _;
use std::sync::atomic::{AtomicUsize, Ordering};

fn caps(max_output: usize, tools: bool) -> ModelCapabilities {
    ModelCapabilities {
        context: 200_000,
        max_output,
        tools,
        streaming: true,
        ..Default::default()
    }
}

fn counting_tool(executions: Arc<AtomicUsize>) -> Tool {
    Tool {
        name: "marker_tool".into(),
        description: "records execution".into(),
        input_schema: serde_json::json!({"type": "object"}),
        resource_class: faktor_core::resource::ResourceClass::Cpu,
        capability: None,
        recovery_hint: RecoveryHint::Idempotent,
        path_args: vec![],
        execute: Arc::new(move |_ctx, _args| {
            let executions = executions.clone();
            Box::pin(async move {
                executions.fetch_add(1, Ordering::SeqCst);
                Ok(ToolOutcome {
                    text: "ran".into(),
                    exit_code: Some(0),
                    ..Default::default()
                })
            })
        }),
    }
}

fn assistant_text(handle: &faktor_session::SessionHandle) -> String {
    let mut out = String::new();
    for m in handle.messages_before(None, 200).unwrap() {
        if m.role != "assistant" {
            continue;
        }
        for p in handle.parts_of(m.id).unwrap() {
            if p.kind == "text" {
                out.push_str(p.data.get("text").and_then(|v| v.as_str()).unwrap_or(""));
            }
        }
    }
    out
}

fn assistant_part_kinds(handle: &faktor_session::SessionHandle) -> Vec<String> {
    let mut kinds = Vec::new();
    for m in handle.messages_before(None, 200).unwrap() {
        if m.role != "assistant" {
            continue;
        }
        for p in handle.parts_of(m.id).unwrap() {
            kinds.push(p.kind);
        }
    }
    kinds
}

fn fake_with_text_chunks(
    max_output: usize,
    chunks: usize,
    chunk_chars: usize,
) -> Arc<FakeProvider> {
    let mut script: Vec<ScriptedResponse> = (0..chunks)
        .map(|i| ScriptedResponse::Text(format!("chunk{i:04}-{}", "x".repeat(chunk_chars))))
        .collect();
    script.push(ScriptedResponse::End);
    Arc::new(FakeProvider::with_script(
        "fake",
        caps(max_output, false),
        script,
    ))
}

/// A hostile OpenAI-compatible endpoint ignores `max_tokens`, streams many
/// flushed SSE frames, then appends a tool call beyond the envelope. The
/// runtime must stop the attempt at its local ceiling, preserve the accepted
/// prefix with the explicit marker, never execute the late tool call, and
/// settle as post-dispatch uncertainty.
#[tokio::test]
async fn hostile_openai_compatible_stream_is_cut_at_the_local_ceiling() {
    let executions = Arc::new(AtomicUsize::new(0));
    let server = faktor_provider::testing::MockServer::new();
    let mut frames: Vec<Vec<u8>> = Vec::new();
    for i in 0..300 {
        let event = serde_json::json!({
            "choices": [{
                "delta": { "content": format!("chunk-{i:04}-{}", "x".repeat(160)) },
                "finish_reason": serde_json::Value::Null,
            }]
        });
        frames.push(format!("data: {event}\n\n").into_bytes());
    }
    let tool_event = serde_json::json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "late_call",
                    "type": "function",
                    "function": { "name": "marker_tool", "arguments": "{\"late\":true}" }
                }]
            },
            "finish_reason": "tool_calls",
        }]
    });
    frames.push(format!("data: {tool_event}\n\n").into_bytes());
    frames.push(b"data: [DONE]\n\n".to_vec());
    server.route(
        "POST",
        "/chat/completions",
        faktor_provider::testing::MockAction::ChunkedSse {
            status: 200,
            chunks: frames,
        },
    );
    let base = server.base_url().await;
    let model = caps(128, true);
    let inner: Arc<dyn faktor_provider::Provider> = faktor_openai::OpenAiProvider::build(
        faktor_openai::OpenAiConfig::chat(base, None).with_model("m", model),
        Arc::new(faktor_provider::egress::PolicyCheckedHttpTransport::permissive()),
    );
    let observed_chunks = Arc::new(AtomicUsize::new(0));
    let provider_cancel = Arc::new(std::sync::Mutex::new(None));
    let counted = Arc::new(CountingProvider {
        inner,
        chunks: observed_chunks.clone(),
        cancellation: provider_cancel.clone(),
    });
    let (deps, _dir) = deps_with(counted, vec![counting_tool(executions.clone())]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let ws = runtime.deps.session.create_workspace("/w").unwrap();
    let session = runtime
        .deps
        .session
        .create_session(ws, "t", "openai", "m")
        .unwrap()
        .id();
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let receipt = runtime
        .submit(session, "stream far past the cap", &[])
        .unwrap();
    let turn_token = handle
        .turn_cancellation(receipt.op_id)
        .expect("the turn cancellation token is tracked");
    let outcome = runtime.drive_receipt(&handle, receipt, None).await.unwrap();

    assert_eq!(outcome.final_state, AgentState::FailedRecoverable);
    assert_eq!(
        outcome
            .stop_reason
            .as_ref()
            .expect("typed stop reason")
            .code,
        ReasonCode::ProviderOutputLimitExceeded
    );
    let observed = observed_chunks.load(Ordering::SeqCst);
    assert!(
        observed > 0 && observed < 300,
        "the attempt must stop at the local ceiling, not drain the hostile stream (observed {observed})"
    );
    assert_eq!(
        executions.load(Ordering::SeqCst),
        0,
        "a tool call arriving beyond the envelope must never execute"
    );
    assert!(
        handle.pending_tool_runs().unwrap().is_empty(),
        "no post-envelope tool run may be pending"
    );
    let text = assistant_text(&handle);
    assert!(
        text.contains("chunk-0000"),
        "the already-accepted user-visible prefix must be preserved: {text}"
    );
    assert!(
        text.ends_with(OUTPUT_LIMIT_INCOMPLETE_MARKER),
        "the preserved prefix must carry the explicit incomplete marker: {text}"
    );
    assert!(
        !assistant_part_kinds(&handle)
            .iter()
            .any(|k| k == "tool_call"),
        "the beyond-envelope tool call must never be persisted"
    );
    let progress = runtime.progress_view(session).expect("health record");
    assert_eq!(progress["outputLimitViolations"], 1);
    assert!(progress["lastOutputLimitViolation"]
        .as_str()
        .expect("bounded detail")
        .contains("output limit exceeded"));
    assert!(
        provider_cancel
            .lock()
            .unwrap()
            .as_ref()
            .expect("request cancellation token captured")
            .is_cancelled(),
        "the provider attempt itself must be cancelled"
    );
    assert!(
        !turn_token.is_cancelled(),
        "the local attempt cancel must not mark the logical turn cancelled"
    );
    let body = server.last_request().expect("request captured").2;
    let body: serde_json::Value = serde_json::from_str(&body).expect("request JSON");
    assert_eq!(
        body["max_tokens"], 128,
        "the request carried the resolved envelope the provider ignored"
    );
}

/// Wrapper that counts every provider chunk the agent actually consumed and
/// captures the attempt's cancellation token (proves the attempt cancel is
/// real and the stream is not drained after the ceiling trips).
struct CountingProvider {
    inner: Arc<dyn faktor_provider::Provider>,
    chunks: Arc<AtomicUsize>,
    cancellation: Arc<std::sync::Mutex<Option<CancellationToken>>>,
}

impl faktor_provider::Provider for CountingProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        self.inner.capabilities(model)
    }

    fn capability_resolution(&self, model: &str) -> faktor_core::model::CapabilityResolution {
        self.inner.capability_resolution(model)
    }

    fn stream(&self, req: GenericAgentRequest) -> faktor_provider::ProviderStream {
        *self.cancellation.lock().unwrap() = Some(req.meta.cancellation.clone());
        let chunks = self.chunks.clone();
        let inner = self.inner.stream(req);
        Box::pin(inner.map(move |item| {
            chunks.fetch_add(1, Ordering::SeqCst);
            item
        }))
    }
}

#[tokio::test]
async fn provider_respecting_the_cap_is_unaffected() {
    let text = format!("a complete answer {}", "y".repeat(1_000));
    let provider = Arc::new(FakeProvider::with_script(
        "fake",
        caps(512, false),
        vec![ScriptedResponse::Text(text.clone()), ScriptedResponse::End],
    ));
    let (deps, _dir) = deps_with(provider, vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime.run_turn(session, "answer", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert!(outcome.stop_reason.is_none());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    assert_eq!(assistant_text(&handle), text);
    let progress = runtime.progress_view(session).expect("health record");
    assert_eq!(progress["outputLimitViolations"], 0);
    assert_eq!(
        progress["lastOutputLimitViolation"],
        serde_json::Value::Null
    );
}

/// A stream whose ONLY output is reasoning still has a cumulative budget:
/// reasoning tokens cannot be used to bypass the envelope.
#[tokio::test]
async fn reasoning_only_overflow_is_a_typed_refusal() {
    let provider = Arc::new(FakeProvider::with_script(
        "fake",
        caps(64, false),
        (0..8)
            .map(|i| ScriptedResponse::Reasoning(format!("think{i}-{}", "r".repeat(400))))
            .chain([ScriptedResponse::End])
            .collect(),
    ));
    let (deps, _dir) = deps_with(provider, vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime.run_turn(session, "think", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::FailedRecoverable);
    assert_eq!(
        outcome
            .stop_reason
            .as_ref()
            .expect("typed stop reason")
            .code,
        ReasonCode::ProviderOutputLimitExceeded
    );
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let kinds = assistant_part_kinds(&handle);
    assert!(
        kinds.iter().any(|k| k == "reasoning"),
        "accepted reasoning must be preserved: {kinds:?}"
    );
    assert_eq!(
        assistant_text(&handle),
        OUTPUT_LIMIT_INCOMPLETE_MARKER,
        "a reasoning-only overflow has no user text to preserve, only the marker"
    );
    assert_eq!(
        runtime.progress_view(session).unwrap()["outputLimitViolations"],
        1
    );
}

/// The durable flush boundary is not a budget boundary: this stream crosses
/// the 8 KiB segment flush (proved by multiple durable text parts) and then
/// keeps streaming; the cumulative counter still trips at the envelope.
#[tokio::test]
async fn mid_stream_flushes_cannot_reset_the_cumulative_counter() {
    let provider = fake_with_text_chunks(4_096, 40, 1_000);
    let (deps, _dir) = deps_with(provider, vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime
        .run_turn(session, "flush forever", &[])
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::FailedRecoverable);
    assert_eq!(
        outcome
            .stop_reason
            .as_ref()
            .expect("typed stop reason")
            .code,
        ReasonCode::ProviderOutputLimitExceeded
    );
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let text = assistant_text(&handle);
    assert!(
        text.len() > STREAM_FLUSH_BYTES,
        "the accepted prefix must have crossed a durable flush segment: {} bytes",
        text.len()
    );
    assert!(
        text.ends_with(OUTPUT_LIMIT_INCOMPLETE_MARKER),
        "the flushed prefix must still be marked incomplete"
    );
    let text_parts = {
        let mut parts = 0;
        for m in handle.messages_before(None, 200).unwrap() {
            if m.role != "assistant" {
                continue;
            }
            for p in handle.parts_of(m.id).unwrap() {
                if p.kind == "text" {
                    parts += 1;
                }
            }
        }
        parts
    };
    assert!(
        text_parts >= 2,
        "at least one mid-stream durable flush must precede the marker (parts={text_parts})"
    );
}

/// A hostile tool-call payload beyond the envelope is rejected BEFORE any
/// durable tool-call part exists, so it can never enter the tool batch.
#[tokio::test]
async fn tool_payload_beyond_the_envelope_is_never_persisted_or_executed() {
    let executions = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(FakeProvider::with_script(
        "fake",
        caps(64, true),
        vec![
            ScriptedResponse::Text("accepted prefix".into()),
            ScriptedResponse::ToolCall {
                id: "late".into(),
                name: "marker_tool".into(),
                input: serde_json::json!({ "blob": "z".repeat(200_000) }),
            },
            ScriptedResponse::End,
        ],
    ));
    let (deps, _dir) = deps_with(provider, vec![counting_tool(executions.clone())]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime
        .run_turn(session, "call the tool", &[])
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::FailedRecoverable);
    assert_eq!(
        outcome
            .stop_reason
            .as_ref()
            .expect("typed stop reason")
            .code,
        ReasonCode::ProviderOutputLimitExceeded
    );
    assert_eq!(
        executions.load(Ordering::SeqCst),
        0,
        "the oversized tool call must never execute"
    );
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    assert!(
        !assistant_part_kinds(&handle)
            .iter()
            .any(|k| k == "tool_call"),
        "the oversized payload must never be persisted as a tool-call part"
    );
    assert!(handle.pending_tool_runs().unwrap().is_empty());
    let text = assistant_text(&handle);
    assert!(text.contains("accepted prefix"));
    assert!(text.ends_with(OUTPUT_LIMIT_INCOMPLETE_MARKER));
}

/// Settlement truth on a local ceiling breach: the request DID dispatch, so
/// the attempt becomes UNCERTAIN (never refunded, never settled at a partial
/// usage), and the commercial hold stays durable for reconciliation.
#[tokio::test]
async fn output_overflow_settles_conservatively_as_post_dispatch_uncertainty() {
    let recorder = DebitRecorder::new(true);
    let budget = DebitBudget::new(Some(9));
    let fake = fake_with_text_chunks(64, 40, 400);
    let captured_token: Arc<std::sync::Mutex<Option<CancellationToken>>> =
        Arc::new(std::sync::Mutex::new(None));
    let hook_token = captured_token.clone();
    let hook_recorder = recorder.clone();
    let inspected = Arc::new(InspectingProvider::new(fake, move |_n, req| {
        hook_recorder.push("stream");
        *hook_token.lock().unwrap() = Some(req.meta.cancellation.clone());
        Ok(())
    }));
    let (mut deps, _dir) = deps_with(inspected.clone(), vec![]);
    deps.budgets = budget.clone();
    let runtime = AgentRuntime::new(deps).unwrap();
    runtime
        .set_provider_debits(Some(recorder.clone() as Arc<dyn ProviderAttemptDebits>))
        .expect("install debit authority");
    let (manager, session) = shared_session(runtime.deps());
    let _ = manager;
    let outcome = runtime.run_turn(session, "overflow", &[]).await.unwrap();

    assert_eq!(outcome.final_state, AgentState::FailedRecoverable);
    assert_eq!(
        outcome
            .stop_reason
            .as_ref()
            .expect("typed stop reason")
            .code,
        ReasonCode::ProviderOutputLimitExceeded
    );
    assert_eq!(
        inspected.counter.load(Ordering::SeqCst),
        1,
        "a local ceiling breach is terminal, never a retry"
    );
    assert_eq!(
        budget.uncertain.load(Ordering::SeqCst),
        1,
        "dispatch occurred: the reservation must become UNCERTAIN"
    );
    assert_eq!(
        budget.refunds.load(Ordering::SeqCst),
        0,
        "a dispatched attempt is never refunded"
    );
    assert_eq!(
        budget.settles.load(Ordering::SeqCst),
        0,
        "a truncated stream is not a settlement basis"
    );
    assert!(recorder.settles.lock().unwrap().is_empty());
    assert!(recorder.refunds.lock().unwrap().is_empty());
    assert!(recorder.events().iter().any(|e| e == "stream"));
    assert!(
        captured_token
            .lock()
            .unwrap()
            .as_ref()
            .expect("request cancellation token captured")
            .is_cancelled(),
        "the provider attempt must be cancelled"
    );
}

/// Cancellation race: a user Stop that lands before the next (huge) chunk is
/// processed wins over the output-budget path — the turn is Cancelled, not a
/// provider output-limit failure, and no health violation is recorded.
#[tokio::test]
async fn turn_cancel_wins_over_a_simultaneous_overflow_chunk() {
    let fed = FedProvider::new();
    let (deps, _dir, _clock) = deps_with_skew(fed.clone());
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let receipt = runtime.submit(session, "work", &[]).unwrap();
    let turn_token = handle
        .turn_cancellation(receipt.op_id)
        .expect("the turn cancellation token is tracked");
    let agent = runtime.clone();
    let handle2 = runtime.deps.session.get_session(session).unwrap().unwrap();
    let drive = tokio::spawn(async move { agent.drive_receipt(&handle2, receipt, None).await });
    wait_fed_sender(&fed).await;
    let sender = fed.sender();
    sender
        .send(ProviderChunk::Text {
            text: "partial".into(),
        })
        .expect("feed a first chunk");
    turn_token.cancel();
    sender
        .send(ProviderChunk::Text {
            text: "x".repeat(200_000),
        })
        .expect("feed the overflowing chunk");
    let outcome = tokio::time::timeout(Duration::from_secs(30), drive)
        .await
        .expect("the cancelled turn must terminate")
        .unwrap()
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::Cancelled);
    assert_eq!(
        outcome.stop_reason.as_ref().expect("cancel reason").code,
        ReasonCode::Cancelled,
        "cancellation wins; it must not become an output-limit refusal: {outcome:?}"
    );
    let progress = runtime.progress_view(session).expect("health record");
    assert_eq!(
        progress["outputLimitViolations"], 0,
        "the overflow chunk was never admitted after the cancel"
    );
}
