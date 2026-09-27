//! `runtime::request_tests`: out-of-line tests.

use super::*;
use crate::runtime::tests::*;
use crate::*;

/// Audit 29 regression: a 20k-message session drives a turn with a
/// bounded, budget-chosen window. The provider receives ONLY the newest
/// rows that fit the planner's load bounds (newest ~500 of the 20 000,
/// not the 2000-row message cap the old load-then-trim path would
/// materialize), the current prompt is always inside the window, and
/// after a mid-history deletion band the store still never scans the
/// old tail (the turn succeeds with the same bounded window).
#[tokio::test]
async fn turn_window_is_bounded_newest_first_over_a_20k_session() {
    use faktor_core::event::EventKind;
    use faktor_core::state::AgentState;

    // Seed a 20k-message session directly: one journal event + one
    // durable user prompt per row keeps the message-seq == event-seq
    // invariant that real turns maintain.
    let (seed_deps, _dir0) = deps(scripted_provider(vec![ScriptedResponse::End]), vec![]);
    let (manager, session) = shared_session(&seed_deps);
    let handle = manager.get_session(session).unwrap().unwrap();
    for i in 0..20_000u64 {
        let seq = handle
            .force_append_event(
                EventKind::ModelChunkReceived,
                AgentState::ReadyForNextTurn,
                None,
                None,
            )
            .unwrap();
        handle
            .put_message(
                seq.raw() as i64,
                "user",
                serde_json::json!({ "text": format!("seed-{i:08} {}", "x".repeat(32)) }),
            )
            .unwrap();
    }
    assert_eq!(handle.message_count().unwrap(), 20_000);

    // The final turn runs against a capturing provider (default small
    // model caps → the 32K budget profile, recent = 10_000 tokens).
    let captured: Arc<std::sync::Mutex<Vec<GenericAgentRequest>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let cap = captured.clone();
    let inspected = Arc::new(InspectingProvider::new(
        Arc::new(FakeProvider::with_script(
            "fake",
            ModelCapabilities {
                tools: true,
                ..Default::default()
            },
            vec![ScriptedResponse::Text("ok".into()), ScriptedResponse::End],
        )),
        move |_n, req| {
            cap.lock().unwrap().push(req.clone());
            Ok(())
        },
    ));
    let (deps1, _dir1) = deps_sharing_session(manager.clone(), inspected, vec![]);
    let runtime = AgentRuntime::new(deps1).unwrap();
    let outcome = runtime.run_turn(session, "final probe", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert!(
        !outcome.compacted,
        "the bounded window must not trip compaction"
    );

    let (max_messages, max_bytes) = AgentRuntime::history_window_bounds(&ContextBudget::default());
    // Simulate the exact greedy newest-first window over the REAL rows
    // that existed when the request was built: the assistant reply of
    // this turn is appended AFTER the provider request, so the newest
    // row at request time is the turn's own prompt row ("final probe").
    // The byte bound (per stored payload), the message cap and the
    // oversized-first rule are replicated; the provider request must
    // carry exactly that window.
    let mut rows: Vec<faktor_store::MessageRow> = Vec::new();
    let mut cursor: Option<i64> = None;
    loop {
        let page = handle.messages_before(cursor, 200).unwrap();
        if page.is_empty() {
            break;
        }
        let last = page.last().unwrap().seq;
        rows.extend(page);
        if last <= 1 {
            break;
        }
        cursor = Some(last);
    }
    let prompt_seq = rows
        .iter()
        .find(|r| {
            r.data
                .get("text")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t.contains("final probe"))
        })
        .map(|r| r.seq)
        .expect("the turn's own prompt row must be durable");
    let mut expected = 0usize;
    let mut bytes = 0u64;
    for row in rows.iter().filter(|r| r.seq <= prompt_seq) {
        if expected as u64 >= max_messages {
            break;
        }
        let row_bytes = serde_json::to_string(&row.data).unwrap().len() as u64;
        if expected > 0 && bytes.saturating_add(row_bytes) > max_bytes {
            break;
        }
        expected += 1;
        bytes += row_bytes;
    }
    assert!(
        (500..=1500).contains(&expected),
        "window {expected} must be bounded far below the 20k rows and the 2000-row cap"
    );
    {
        let requests = captured.lock().unwrap();
        assert_eq!(requests.len(), 1, "one wire request for the turn");
        let req = &requests[0];
        assert_eq!(
            req.messages.len(),
            expected,
            "the provider must receive exactly the bounded newest-first window"
        );
        // The newest rows (including the current prompt) are inside; the
        // old tail (the first seeds) is not.
        let rendered: String = req
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|c| match &c.kind {
                ContentKind::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("final probe"), "current prompt in window");
        assert!(rendered.contains("seed-00019999"), "newest seed in window");
        assert!(!rendered.contains("seed-00000000"), "oldest seed excluded");
        assert!(!rendered.contains("seed-00010000"), "old tail excluded");
    }

    // Adversarial follow-up: delete a 10k-row band mid-history (the old
    // tail becomes holes) and drive ANOTHER turn. The bounded load must
    // still succeed with the same window — it never scans the removed
    // rows. (The store-level corrupt-tail test proves the stronger
    // never-READ property at the SQL layer.)
    for seq in 5_000..=15_000i64 {
        handle.delete_message(seq).unwrap();
    }
    let captured2: Arc<std::sync::Mutex<Vec<GenericAgentRequest>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let cap2 = captured2.clone();
    let inspected2 = Arc::new(InspectingProvider::new(
        Arc::new(FakeProvider::with_script(
            "fake",
            ModelCapabilities {
                tools: true,
                ..Default::default()
            },
            vec![ScriptedResponse::Text("ok".into()), ScriptedResponse::End],
        )),
        move |_n, req| {
            cap2.lock().unwrap().push(req.clone());
            Ok(())
        },
    ));
    let (deps2, _dir2) = deps_sharing_session(manager.clone(), inspected2, vec![]);
    let runtime = AgentRuntime::new(deps2).unwrap();
    let outcome = runtime
        .run_turn(session, "probe after deletion", &[])
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    let req2 = &captured2.lock().unwrap()[0];
    assert!(
        req2.messages.len() <= expected + 8,
        "window stays bounded after holes"
    );
    let rendered2: String = req2
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|c| match &c.kind {
            ContentKind::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered2.contains("probe after deletion"));
    assert!(rendered2.contains("seed-00019999"));
    assert!(
        !rendered2.contains("seed-00007000"),
        "deleted band never resurfaces"
    );
}

#[tokio::test]
async fn model_override_changes_wire_request_model() {
    // The provider records the model of every request streamed through
    // it: the per-message override must reach the wire request, and a
    // plain run_turn must keep sending the session model.
    let provider = scripted_provider(vec![
        ScriptedResponse::Text("pong".into()),
        ScriptedResponse::End,
    ]);
    let (deps, _dir) = deps(provider.clone(), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());

    let outcome = runtime
        .run_turn_with_model(session, "hi", &[], Some("m2".into()))
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(
        provider.last_request_model().as_deref(),
        Some("m2"),
        "the override must be the model on the wire request"
    );

    let outcome = runtime.run_turn(session, "hi again", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(
        provider.last_request_model().as_deref(),
        Some("m"),
        "without an override the session model must be sent"
    );
}

#[tokio::test]
async fn provider_request_cancellation_is_child_of_turn_token() {
    // The request's meta.cancellation must share the turn's lineage:
    // cancelling the turn token cancels the wire request. On the old
    // code build_request minted a fresh token and this test fails.
    let provider = scripted_provider(vec![
        ScriptedResponse::Text("ok".into()),
        ScriptedResponse::End,
    ]);
    let (deps, _dir) = deps(provider.clone(), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let receipt = handle.submit_prompt("go", &[]).unwrap();
    let turn_token = receipt.op_meta.cancellation.clone();
    runtime
        .drive_turn(&handle, receipt.op_id, turn_token.clone(), None)
        .await
        .unwrap();
    let request_cancel = provider
        .last_request_cancellation()
        .expect("a provider request was streamed");
    assert!(
        !request_cancel.is_cancelled(),
        "the request token must be live while the turn runs"
    );
    turn_token.cancel();
    assert!(
        request_cancel.is_cancelled(),
        "cancelling the turn token must cascade to the provider request"
    );
}

#[tokio::test]
async fn wire_request_contains_each_element_once() {
    // The wire request must contain every conceptual element exactly
    // once: the prompt once in messages, the tool schema once in tools,
    // and the system carries instructions/ledger — never the prompt text
    // and never the tool schema JSON.
    let inner = scripted_provider(vec![
        ScriptedResponse::Text("ok".into()),
        ScriptedResponse::End,
    ]);
    let captured: Arc<std::sync::Mutex<Vec<GenericAgentRequest>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let cap = captured.clone();
    let wrapper = InspectingProvider::new(Arc::new(inner), move |_n, req| {
        cap.lock().unwrap().push(req.clone());
        Ok(())
    });
    let (deps, _dir) = deps_with(Arc::new(wrapper), vec![echo_tool()]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    runtime.run_turn(session, "use echo", &[]).await.unwrap();

    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let req = &requests[0];
    // Prompt: exactly once, as a user message.
    let prompt_parts: Vec<&ContentPart> = req
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter(|p| matches!(&p.kind, ContentKind::Text { text } if text == "use echo"))
        .collect();
    assert_eq!(prompt_parts.len(), 1, "prompt must appear exactly once");
    assert_eq!(
        req.messages.len(),
        1,
        "only the prompt message on a fresh turn"
    );
    assert!(req.messages[0].role == Role::User);
    // System: instructions, no conversation, no tool schema.
    assert!(req.system.contains("You are a test agent."));
    assert_eq!(req.system.matches("You are a test agent.").count(), 1);
    assert!(
        !req.system.contains("use echo"),
        "history must not leak into system"
    );
    assert!(
        !req.system.contains("echo back"),
        "tool schema must not leak into system"
    );
    let tool_json = serde_json::to_string(&req.tools[0]).unwrap();
    assert!(
        !req.system.contains(&tool_json),
        "tool schema JSON in system"
    );
    // Tools: exactly once.
    assert_eq!(req.tools.len(), 1);
    assert_eq!(req.tools[0].name, "echo");
}

/// Every Windows-absolute dialect is refused by the pure host-side
/// grammar, which is compiled and asserted on every platform (darwin CI
/// included); ordinary relative names stay accepted.
#[test]
fn workspace_relative_path_grammar_rejects_windows_absolute_shapes() {
    for hostile in [
        r"C:\escape.txt",
        "C:/escape.txt",
        "c:/escape.txt",
        r"c:\escape.txt",
        "C:escape.txt",
        r"\\server\share\escape.txt",
        "//server/share/escape.txt",
        r"\escape.txt",
        "/escape.txt",
        r"\\?\C:\escape.txt",
        r"\\.\C:\escape.txt",
        "../escape.txt",
        "sub/../../escape.txt",
        "a\\..\\b.txt",
        "x\0y",
    ] {
        assert!(
            workspace_relative_path_rejection(hostile).is_some(),
            "{hostile:?} must be rejected"
        );
    }
    for safe in ["./a.txt", "a.txt", "sub/dir/a.txt", "..hidden"] {
        assert!(
            workspace_relative_path_rejection(safe).is_none(),
            "{safe:?} must stay accepted: {:?}",
            workspace_relative_path_rejection(safe)
        );
    }
}

/// (3) Typed child handoff: a child whose durable transcript holds
/// ~100k tokens of synthetic evidence contributes only a bounded
/// facts/findings/decisions/changed-files/refs render; the parent's
/// captured wire request stays within the configured handoff budget and
/// the omitted backing stays retrievable through the scoped ref.
#[tokio::test]
async fn typed_child_handoff_bounds_the_parent_request_and_keeps_backing_by_refs() {
    const BUDGET_TOKENS: usize = 512;
    // ≈100k tokens of synthetic evidence (4 bytes/token).
    let evidence = format!(
        "CHILD_EVIDENCE_BLOB_START{}CHILD_EVIDENCE_BLOB_END",
        "e".repeat(400_000)
    );
    let caps = ModelCapabilities {
        tools: true,
        streaming: true,
        context: 200_000,
        ..Default::default()
    };
    type WireCapture = (String, Vec<RequestMessage>, Vec<faktor_provider::ToolSpec>);
    let captured: Arc<std::sync::Mutex<Vec<WireCapture>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = captured.clone();
    let provider = Arc::new(InspectingProvider::new(
        Arc::new(FakeProvider::with_script(
            "fake",
            caps.clone(),
            vec![
                ScriptedResponse::Text("ok".into()),
                ScriptedResponse::End,
                ScriptedResponse::Text("ok".into()),
                ScriptedResponse::End,
            ],
        )),
        move |_i, req: &GenericAgentRequest| {
            sink.lock().unwrap().push((
                req.system.clone(),
                req.messages.clone(),
                req.tools.clone(),
            ));
            Ok(())
        },
    ));
    let (deps, _dir) = deps_with(provider, vec![]);
    let manager = deps.session.clone();
    let runtime = AgentRuntime::new(deps).unwrap();

    // The child's durable backing: a session whose transcript holds the
    // 100k-token evidence. Its scoped identity is the handoff ref.
    let child_session = new_session(runtime.deps());
    let child_handle = manager.get_session(child_session).unwrap().unwrap();
    child_handle
        .put_message(1, "assistant", serde_json::json!({ "text": evidence }))
        .unwrap();

    // The bounded handoff the parent consumes: durable facts and scoped
    // refs — never the transcript bytes.
    let mut handoff = crate::runtime::ChildHandoff::new("child-1");
    handoff.child_session = Some(child_session);
    handoff.goal = "audit the parser".into();
    handoff.outcome = "done".into();
    handoff.facts = vec!["parser has 3 lexer states".into()];
    handoff.findings = vec!["flaky test in lexer::tests".into()];
    handoff.decisions = vec!["approach: bounded handoff".into()];
    handoff.changed_files = vec!["src/parser.rs".into()];
    handoff.refs = vec![
        format!("session:{}", child_session.raw()),
        format!("session:{}/messages", child_session.raw()),
    ];
    let rendered = handoff.render_bounded(BUDGET_TOKENS);
    assert!(
        !rendered.contains("CHILD_EVIDENCE_BLOB"),
        "the child transcript must never enter the handoff render"
    );
    assert!(rendered.contains("audit the parser"));
    assert!(rendered.contains(&format!("session:{}", child_session.raw())));

    // Baseline request vs handoff request, same deps shape.
    let baseline_session = new_session(runtime.deps());
    let parent_session = new_session(runtime.deps());
    runtime.run_turn(baseline_session, "", &[]).await.unwrap();
    runtime
        .run_turn_from_handoff(parent_session, &handoff, BUDGET_TOKENS)
        .await
        .unwrap();
    let wires = captured.lock().unwrap().clone();
    assert_eq!(wires.len(), 2, "baseline + handoff parent requests");
    let baseline =
        faktor_context::measure_wire_request(&wires[0].0, &wires[0].1, &wires[0].2) as u64;
    let with_handoff =
        faktor_context::measure_wire_request(&wires[1].0, &wires[1].1, &wires[1].2) as u64;
    assert!(
        with_handoff >= baseline,
        "the bounded handoff can only add context"
    );
    assert!(
        with_handoff - baseline <= BUDGET_TOKENS as u64 + 8,
        "the parent's captured request must stay within the configured handoff budget: \
             delta {} > {BUDGET_TOKENS}",
        with_handoff - baseline
    );
    let handoff_text: String = wires[1]
        .1
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|p| match &p.kind {
            ContentKind::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(handoff_text.contains("audit the parser"));
    assert!(!handoff_text.contains("CHILD_EVIDENCE_BLOB"));

    // The omitted backing is still retrievable, scoped by child session.
    let rows = manager
        .messages_backwards_bounded(child_session, None, 4, u64::MAX)
        .await
        .unwrap();
    assert!(
        rows.iter()
            .any(|row| row.data.to_string().contains("CHILD_EVIDENCE_BLOB")),
        "the child's 100k-token evidence must stay retrievable by its scoped ref"
    );
}
