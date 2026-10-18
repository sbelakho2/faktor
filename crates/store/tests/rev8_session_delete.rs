//! Adversarial session deletion cascade: purge removes journal/ledger rows,
//! keeps a durable tombstone, and a replay returns the same audited result.

use faktor_core::event::EventKind;
use faktor_core::id::{EventSeq, SessionId};
use faktor_core::state::AgentState;
use faktor_store::Store;

#[test]
fn purge_is_transactional_idempotent_and_leaves_a_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("store"), true).unwrap();
    let workspace = store.create_workspace("/tmp/ws").unwrap();
    let session = store
        .create_session(workspace, "doomed", "fake", "m")
        .unwrap()
        .id;
    store
        .append_event(
            session,
            None,
            EventKind::PromptReceived,
            AgentState::Idle,
            1,
            Some(serde_json::json!({"text": "secret prompt"})),
        )
        .unwrap();
    assert!(!store
        .events_after(session, EventSeq::new(1))
        .unwrap()
        .is_empty());

    let tombstone = store
        .purge_session_content(session, "user_requested")
        .unwrap();
    assert_eq!(tombstone.session_id, session);
    assert!(tombstone.purged["event"].as_i64().unwrap() >= 1);
    assert!(store
        .events_after(session, EventSeq::new(1))
        .unwrap()
        .is_empty());
    // The session row remains as the closed tombstone.
    assert!(store.get_session(session).unwrap().is_some());
    let recorded = store.session_deletion(session).unwrap().unwrap();
    assert_eq!(recorded.deleted_ms, tombstone.deleted_ms);

    // Replay is the SAME tombstone, never a second delete/audit row.
    let replayed = store
        .purge_session_content(session, "ignored_second_reason")
        .unwrap();
    assert_eq!(replayed.deleted_ms, tombstone.deleted_ms);
    assert_eq!(replayed.purged, tombstone.purged);
    let missing = SessionId::new(999_999);
    assert!(store.purge_session_content(missing, "x").is_err());
}
