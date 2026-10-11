//! `runtime::turn::drive_phases_tests`: out-of-line tests for the phase split
//! of the logical-turn loop (the extracted `AgentState` phase bodies).

use super::*;
use crate::runtime::tests::*;
use crate::*;

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
    let mut st = TurnDriveState::new(&runtime, &handle, op_id, cancel, None).unwrap();
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
