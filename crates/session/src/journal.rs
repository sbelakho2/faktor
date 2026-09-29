//! Journaling rules: every event's state must be a legal `StateMachine`
//! transition from the previous state, and replay must detect corruption.

use faktor_core::event::EventKind;
use faktor_core::state::{AgentState, StateMachine};

use crate::SessionError;

/// Validate that an event of `kind` may land on `to` from `current`.
///
/// Two kinds carry documented sub-chains because the *event kind* describes a
/// step that the *state column* records as its result:
///
/// - `ToolRequested` events are recorded with state `WaitingForPermission`
///   (the machine hops `ToolRequested` then `WaitingForPermission`; both hops
///   must be legal).
/// - `Failed` events recorded with state `FailedPermanent` are legal only via
///   the documented two-step `FailedRecoverable` then force — no state's
///   `allowed_transitions` lists `FailedPermanent` by design, so entering it
///   is a deliberate, recorded escalation.
///
/// Self-transitions are legal and idempotent (replay must not fail on
/// re-emitted events).
pub(crate) fn validate_transition(
    current: AgentState,
    kind: EventKind,
    to: AgentState,
) -> Result<(), SessionError> {
    let mut m = StateMachine::new(current);
    let hop = |m: &mut StateMachine, target: AgentState| -> Result<(), SessionError> {
        m.transition(target)
            .map_err(|_| SessionError::illegal(current, to))
    };
    match kind {
        EventKind::ToolRequested => {
            if to != AgentState::WaitingForPermission {
                return Err(SessionError::Malformed(
                    "ToolRequested events must record state WaitingForPermission".into(),
                ));
            }
            hop(&mut m, AgentState::ToolRequested)?;
            hop(&mut m, AgentState::WaitingForPermission)?;
            Ok(())
        }
        EventKind::Failed if to == AgentState::FailedPermanent => {
            // Documented two-step: FailedRecoverable must be reachable first.
            hop(&mut m, AgentState::FailedRecoverable)?;
            Ok(())
        }
        _ => hop(&mut m, to),
    }
}

/// The result of replaying a session journal from durable state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayOutcome {
    /// Reconstructed machine state after the final event.
    pub state: AgentState,
    /// Sequence of the final event.
    pub last_seq: faktor_core::id::EventSeq,
    /// Number of events replayed.
    pub event_count: u64,
}

fn corruption(message: String) -> SessionError {
    SessionError::Internal(format!("journal corruption: {message}"))
}

/// Replay a session's journal, enforcing the journal's structural invariants
/// (first seq 1, first state `Idle`, gapless sequences, non-decreasing
/// timestamps) and the same transition rules the live append path uses. Any
/// violation is journal corruption and a loud error — never a silent skip.
pub(crate) fn replay(events: &[faktor_core::event::Event]) -> Result<ReplayOutcome, SessionError> {
    let first = events
        .first()
        .ok_or_else(|| corruption("session has no events; missing SessionCreated".into()))?;
    if first.seq.raw() != 1 {
        return Err(corruption(format!(
            "first event seq is {}, expected 1",
            first.seq.raw()
        )));
    }
    if first.kind != EventKind::SessionCreated {
        return Err(corruption(format!(
            "first event is {:?}, not SessionCreated",
            first.kind
        )));
    }
    if first.state != AgentState::Idle {
        return Err(corruption(format!(
            "first event state is {:?}, expected Idle",
            first.state
        )));
    }
    let mut m = StateMachine::new(first.state);
    let mut expected_seq: u64 = 1;
    let mut previous_ts = first.ts_ms;
    for (index, e) in events.iter().enumerate() {
        if e.seq.raw() != expected_seq {
            return Err(corruption(format!(
                "event seq is {}, expected {expected_seq}",
                e.seq.raw()
            )));
        }
        expected_seq = expected_seq
            .checked_add(1)
            .ok_or_else(|| corruption("journal sequence overflow".into()))?;
        if e.ts_ms < previous_ts {
            return Err(corruption(format!(
                "event {} timestamp {} is before previous timestamp {}",
                e.seq.raw(),
                e.ts_ms,
                previous_ts
            )));
        }
        previous_ts = e.ts_ms;
        if index == 0 {
            continue;
        }
        validate_transition(m.state(), e.kind, e.state)
            .map_err(|err| corruption(format!("at seq {}: {}", e.seq, err)))?;
        m.force(e.state);
    }
    let last = events.last().expect("non-empty by construction");
    Ok(ReplayOutcome {
        state: m.state(),
        last_seq: last.seq,
        event_count: events.len() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_core::event::Event;
    use faktor_core::id::{EventSeq, OpId, SessionId};

    fn ev(seq: u64, kind: EventKind, state: AgentState) -> Event {
        ev_ts(seq, kind, state, 0)
    }

    fn ev_ts(seq: u64, kind: EventKind, state: AgentState, ts_ms: i64) -> Event {
        Event::new(
            EventSeq::new(seq),
            SessionId::new(1),
            Some(OpId::new(1)),
            kind,
            state,
            ts_ms,
            None,
        )
    }

    #[test]
    fn replay_rejects_skipped_states() {
        // Preparing -> Streaming skips BuildingContext/WaitingForModel.
        let events = vec![
            ev(1, EventKind::SessionCreated, AgentState::Idle),
            ev(2, EventKind::PromptReceived, AgentState::Preparing),
            ev(3, EventKind::ModelStarted, AgentState::Streaming),
        ];
        assert!(replay(&events).is_err(), "skipped states are corruption");
    }

    #[test]
    fn replay_rejects_journal_starting_at_seq_5() {
        // A journal whose first durable row is seq 5 (missing 1..4) must never
        // replay: the gap is corruption, not a short journal.
        let events = vec![ev(5, EventKind::SessionCreated, AgentState::Idle)];
        let err = replay(&events).unwrap_err();
        assert!(
            matches!(&err, SessionError::Internal(m)
                if m.contains("first event seq is 5") && m.contains("expected 1")),
            "{err}"
        );
    }

    #[test]
    fn replay_rejects_sequence_gap() {
        // 1,3 is a missing event: self-transitions would replay it silently.
        let events = vec![
            ev(1, EventKind::SessionCreated, AgentState::Idle),
            ev(3, EventKind::PromptReceived, AgentState::Preparing),
        ];
        let err = replay(&events).unwrap_err();
        assert!(
            matches!(&err, SessionError::Internal(m)
                if m.contains("event seq is 3") && m.contains("expected 2")),
            "{err}"
        );
    }

    #[test]
    fn replay_rejects_duplicate_sequence() {
        let events = vec![
            ev(1, EventKind::SessionCreated, AgentState::Idle),
            ev(1, EventKind::SessionCreated, AgentState::Idle),
        ];
        let err = replay(&events).unwrap_err();
        assert!(
            matches!(&err, SessionError::Internal(m)
                if m.contains("event seq is 1") && m.contains("expected 2")),
            "{err}"
        );
    }

    #[test]
    fn replay_rejects_decreasing_timestamp() {
        let events = vec![
            ev_ts(1, EventKind::SessionCreated, AgentState::Idle, 1_000),
            ev_ts(2, EventKind::PromptReceived, AgentState::Preparing, 2_000),
            ev_ts(
                3,
                EventKind::ContextPrepared,
                AgentState::BuildingContext,
                1_500,
            ),
        ];
        let err = replay(&events).unwrap_err();
        assert!(
            matches!(&err, SessionError::Internal(m)
                if m.contains("timestamp 1500") && m.contains("before previous timestamp 2000")),
            "{err}"
        );
    }

    #[test]
    fn replay_rejects_non_idle_first_state() {
        let events = vec![ev(1, EventKind::SessionCreated, AgentState::Preparing)];
        let err = replay(&events).unwrap_err();
        assert!(
            matches!(&err, SessionError::Internal(m)
                if m.contains("Preparing") && m.contains("expected Idle")),
            "{err}"
        );
    }

    #[test]
    fn replay_rejects_transition_from_terminal() {
        let events = vec![
            ev(1, EventKind::SessionCreated, AgentState::Idle),
            ev(2, EventKind::TurnCompleted, AgentState::Completed),
            ev(3, EventKind::PromptReceived, AgentState::Preparing),
        ];
        assert!(replay(&events).is_err(), "terminal must be terminal");
    }

    #[test]
    fn replay_accepts_the_full_legal_chain() {
        let events = vec![
            ev(1, EventKind::SessionCreated, AgentState::Idle),
            ev(2, EventKind::PromptReceived, AgentState::Preparing),
            ev(3, EventKind::ContextPrepared, AgentState::BuildingContext),
            ev(4, EventKind::ModelStarted, AgentState::WaitingForModel),
            ev(5, EventKind::ModelChunkReceived, AgentState::Streaming),
            ev(
                6,
                EventKind::ToolRequested,
                AgentState::WaitingForPermission,
            ),
            ev(7, EventKind::ToolStarted, AgentState::ExecutingTool),
            ev(8, EventKind::ToolCompleted, AgentState::Validating),
            ev(9, EventKind::TurnCompleted, AgentState::Completed),
        ];
        let out = replay(&events).unwrap();
        assert_eq!(out.state, AgentState::Completed);
        assert_eq!(out.event_count, 9);
    }

    #[test]
    fn replay_accepts_documented_failed_permanent_escalation() {
        let events = vec![
            ev(1, EventKind::SessionCreated, AgentState::Idle),
            ev(2, EventKind::PromptReceived, AgentState::Preparing),
            ev(3, EventKind::Failed, AgentState::FailedRecoverable),
            ev(4, EventKind::Failed, AgentState::FailedPermanent),
        ];
        let out = replay(&events).unwrap();
        assert_eq!(out.state, AgentState::FailedPermanent);
    }

    #[test]
    fn validation_rejects_tool_requested_with_wrong_state() {
        let err = validate_transition(
            AgentState::Streaming,
            EventKind::ToolRequested,
            AgentState::ToolRequested,
        )
        .unwrap_err();
        assert!(matches!(err, SessionError::Malformed(_)));
    }
}
