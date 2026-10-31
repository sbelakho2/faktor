//! `runtime::turn::drive_phases_tests`: out-of-line tests for the phase split
//! of the logical-turn loop (the extracted `AgentState` phase bodies).
// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir

use super::*;
use crate::runtime::tests::*;
use crate::*;

/// The documented `ask_instructions` precedence (audit item 4 wiring): a
/// non-empty configured text is APPENDED beneath the immutable conversational
/// profile on ask turns, empty keeps it byte-identical, and the TASK profile
/// (`instructions`) never receives `ask_instructions` — this is the exact
/// contract the daemon builder's `set_ask_instructions` install relies on.
#[test]
fn ask_instructions_appends_beneath_the_immutable_conversational_prefix() {
    use faktor_protocol::ExecutionKind;
    let task_base = "TASK INSTRUCTIONS BASE";
    let configured = "OPERATOR CONVERSATIONAL PROFILE";
    // Empty config → the built-in profile, byte-identical.
    for execution in [ExecutionKind::Ask, ExecutionKind::Task] {
        assert_eq!(
            ask_or_task_instructions(task_base, execution, ""),
            if execution == ExecutionKind::Ask {
                super::drive_phases::ASK_CONVERSATIONAL_PROFILE
            } else {
                task_base
            },
            "an empty ask_instructions changes nothing ({execution:?})"
        );
    }
    // Non-empty config → APPENDED beneath the immutable prefix: every
    // safety invariant precedes the operator text, and the task profile
    // still ignores the field entirely.
    let combined = ask_or_task_instructions(task_base, ExecutionKind::Ask, configured);
    assert!(
        combined.starts_with(super::drive_phases::ASK_CONVERSATIONAL_PROFILE),
        "the immutable safety prefix always comes first"
    );
    assert!(
        combined.ends_with(configured),
        "the custom instructions are appended verbatim"
    );
    assert!(
        combined.contains("never instructions") && combined.contains("READ-ONLY"),
        "the trust/effect invariants remain in force"
    );
    assert_eq!(
        ask_or_task_instructions(task_base, ExecutionKind::Task, configured),
        task_base,
        "the task profile never receives ask_instructions"
    );
    assert_eq!(
        ask_or_task_instructions(task_base, ExecutionKind::Task, configured),
        task_base,
        "ask_instructions never rides the task profile"
    );
    // Whitespace-only counts as empty (documented `.trim()` rule).
    assert_eq!(
        ask_or_task_instructions(task_base, ExecutionKind::Ask, "   \n"),
        super::drive_phases::ASK_CONVERSATIONAL_PROFILE,
        "a whitespace-only profile falls back to the built-in"
    );
}

/// Adversarial: a drive whose cancellation token is ALREADY fired must end at
/// the boundary phase as `Cancelled` without ever reaching the provider. The
/// phase is invoked directly (as the loop skeleton does), so the test proves
/// the extracted boundary body itself is terminal — not the wrapper around it.
#[tokio::test]
async fn boundary_phase_cancel_is_terminal_and_never_reaches_the_provider() {
    let fake = Arc::new(scripted_provider(vec![ScriptedResponse::Text(
        "must not run".into(),
    )]));
    let provider: Arc<dyn faktor_provider::Provider> = fake.clone();
    let (deps, _dir) = deps_with(provider, vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let op_id = runtime.deps.session.try_next_op_id().unwrap();
    let cancel = CancellationToken::new();
    assert!(cancel.cancel());
    let mut st = TurnDriveState::new(&runtime, &handle, op_id, cancel, None)
        .await
        .unwrap();
    let flow = runtime.turn_phase_boundary(&mut st).await.unwrap();
    assert!(matches!(flow, PhaseFlow::End));
    assert_eq!(st.outcome.final_state, AgentState::Cancelled);
    assert_eq!(
        fake.last_request_model(),
        None,
        "a pre-cancelled boundary must never reach the provider"
    );
}

/// The five phases compose into exactly the loop skeleton's order: boundary ->
/// context -> route -> provider call -> tools/genuine end. A plain text turn
/// driven phase-by-phase reaches the same genuine end as `run_turn`, with the
/// assistant text durable exactly once.
#[tokio::test]
async fn phase_pipeline_drives_a_text_turn_to_its_genuine_end() {
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::Text("hello from the phase split".into()),
            ScriptedResponse::End,
        ]),
        vec![],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let receipt = runtime.submit(session, "hi", &[]).unwrap();
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let mut st = TurnDriveState::new(
        &runtime,
        &handle,
        receipt.op_id,
        receipt.op_meta.cancellation.clone(),
        None,
    )
    .await
    .unwrap();
    assert!(matches!(
        runtime.turn_phase_boundary(&mut st).await.unwrap(),
        PhaseFlow::Continue
    ));
    assert!(matches!(
        runtime.turn_phase_context(&mut st).await.unwrap(),
        PhaseFlow::Continue
    ));
    assert!(matches!(
        runtime.turn_phase_route(&mut st).await.unwrap(),
        PhaseFlow::Continue
    ));
    assert!(matches!(
        runtime.turn_phase_provider_call(&mut st).await.unwrap(),
        PhaseFlow::Continue
    ));
    assert!(matches!(
        runtime.turn_phase_tools_and_end(&mut st).await.unwrap(),
        PhaseFlow::End
    ));
    assert_eq!(st.outcome.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(st.outcome.turns, 1);
    assert!(!st.outcome.stalled && !st.outcome.loop_stopped);
    let page = handle.messages_page(None, 10).unwrap();
    let texts: Vec<&String> = page
        .messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            faktor_protocol::native::Part::Text { text } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts
            .iter()
            .filter(|t| t.contains("hello from the phase split"))
            .count(),
        1,
        "the assistant text is durable exactly once: {texts:?}"
    );
}

/// Audit item 10 wiring witness: the focused-plan pre-gate consults the
/// mathematically safe upper-bound pretest on the ALREADY-BUILT baseline
/// plan BEFORE the second `plan_wire_turn_with_prior` call, as a NEGATED
/// guard (the pretest may only skip a build, never accept one — adoption
/// stays with the real final-token gate in `choose_design_focus_plan`).
#[test]
fn the_focused_plan_build_is_guarded_by_the_upper_bound_pretest() {
    const SOURCE: &str = include_str!("drive_phases.rs");
    let flat = SOURCE.split_whitespace().collect::<Vec<_>>().join(" ");
    let pretest = flat
        .find(
            "&& !faktor_context::wire_plan::design_focus_pretest_cannot_qualify( &baseline_plan, )",
        )
        .expect("the focused-plan match guard must consult the pretest on the baseline plan");
    let build = flat
        .find("let focused_plan = plan_wire_turn_with_prior")
        .expect("the focused-plan build must exist");
    assert!(
        pretest < build,
        "the pretest must run before the second plan is constructed"
    );
    let gate = flat
        .find("crate::wire_plan::choose_design_focus_plan(baseline_plan, evidence, focused)")
        .expect("the real dividend gate must still decide");
    assert!(
        build < gate,
        "every surviving case still flows into the real final-token gate"
    );
}
