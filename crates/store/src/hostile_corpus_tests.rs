//! Hostile-corpus certification for the durable store (SQLite).
//!
//! Every case goes through the production `Store` APIs (no re-implemented
//! validation in the test) and is individually asserted with its case number
//! and input. Categories:
//! 1. hostile SQL/identifier/parameter inputs into every public write/read;
//! 2. migration matrix: per-version fixtures, torn cursors, malformed rows;
//! 3. journal/event semantics: gaps, reorders, unknowns, overflow, cross-session;
//! 4. admission/CAS: task+prompt admission, index CAS, interleavings;
//! 5. attachment/artifact identity;
//! 6. billing/evidence money and identity edges.
//! 7. signed-domain cast boundaries across every paged read.

use std::path::Path;
use std::sync::Arc;

use faktor_core::attachment::AttachmentId;
use faktor_core::event::EventKind;
use faktor_core::hash::FileHash;
use faktor_core::id::{EventSeq, OpId, SessionId, TaskId, WorkspaceId};
use faktor_core::state::{AgentState, SessionLifecycle, TaskState};

use crate::{
    CommandEvent, EvidenceRow, IndexStateRow, JournalDepth, PromptAdmissionClaim,
    SessionTransition, Store, StoreError, TaskAdmissionClaim, TaskRow,
    MAX_PROMPT_ADMISSION_DIGEST_BYTES, MAX_PROMPT_ADMISSION_KEY_BYTES,
    MAX_PROMPT_ADMISSION_RECEIPT_BYTES, MAX_TASK_ADMISSION_DIGEST_BYTES,
    MAX_TASK_ADMISSION_KEY_BYTES, MAX_TASK_ADMISSION_RECEIPT_BYTES,
};

fn tmp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path(), true).expect("store opens");
    (dir, store)
}

fn ws_sid(store: &Store) -> (WorkspaceId, SessionId) {
    let ws = store.create_workspace("/hostile").unwrap();
    let sid = store.create_session(ws, "t", "p", "m").unwrap().id;
    (ws, sid)
}

fn hostile_strings() -> Vec<String> {
    let mut out: Vec<String> = vec![
        String::new(),
        "'".into(),
        "\"".into(),
        "`".into(),
        "''".into(),
        "\"\"".into(),
        "'; DROP TABLE session; --".into(),
        "1 OR 1=1".into(),
        "1; SELECT * FROM sqlite_master".into(),
        "-- comment".into(),
        "/* comment */".into(),
        "a'b".into(),
        "a\"b".into(),
        "\\".into(),
        "a\\b".into(),
        "\u{0}".into(),
        "nul\u{0}mid".into(),
        "日本語".into(),
        "Ünïcødé".into(),
        "𝒜𝒷𝒸".into(),
        "ﬁle".into(),
        "ａｂｃ".into(),
        "е".into(),
        "ΑΒΓ".into(),
        "line\nbreak".into(),
        "tab\there".into(),
        "cr\rhere".into(),
        " leading".into(),
        "trailing ".into(),
        "SELECT".into(),
        "INSERT INTO".into(),
        "PRAGMA table_info(session)".into(),
        ")))".into(),
        "{{".into(),
        "%%s".into(),
        "?".into(),
        ":named".into(),
        "@named".into(),
        "$named".into(),
        "NULL".into(),
        "0xdeadbeef".into(),
        "-9223372036854775808".into(),
        "18446744073709551615".into(),
        "\u{202e}evil\u{202c}".into(),
        "a\u{7f}b".into(),
    ];
    out.push("k".repeat(4096));
    out.push("'".repeat(2048));
    out.push("q".repeat(256));
    out.push("é".repeat(2048));
    out.push("x".repeat(4096));
    out
}

fn plant(store: &Store, sql: &str, values: &[rusqlite::types::Value]) {
    let conn = store.read().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
    let outcome = conn.execute(sql, rusqlite::params_from_iter(values.iter()));
    let _ = conn.execute_batch("PRAGMA foreign_keys = ON");
    outcome.expect("planted row");
}

fn task_row(sid: SessionId, task: u64) -> TaskRow {
    TaskRow {
        task_id: TaskId::new(task),
        session_id: sid,
        goal: "g".into(),
        acceptance_criteria: vec!["c".into()],
        plan: vec!["p".into()],
        attachments: vec![],
        max_tokens: Some(1_000_000),
        max_turns: Some(100),
        spent_tokens: 0,
        spent_turns: 0,
        state: TaskState::Running,
        revision: faktor_core::id::TaskRevision::new(1),
        created_ms: 1,
        updated_ms: 1,
    }
}

// =====================================================================
// Category 1: hostile SQL / identifier / parameter inputs
// =====================================================================

#[test]
fn store_hostile_workspace_roots_roundtrip() {
    let (_d, store) = tmp_store();
    let mut case = 0usize;
    for raw in hostile_strings() {
        case += 1;
        let ws = store
            .create_workspace(&raw)
            .unwrap_or_else(|e| panic!("hostile-sql case #{case} (root {raw:?}): {e}"));
        let back = store.workspace_root(ws).unwrap_or_else(|e| {
            panic!("hostile-sql case #{case} (root {raw:?}): read failed: {e}")
        });
        assert_eq!(
            back.as_deref(),
            Some(raw.as_str()),
            "hostile-sql case #{case} (root {raw:?}): parameter binding must be byte-exact, never SQL"
        );
    }
    assert_eq!(case, 50, "workspace hostile corpus size drifted");
}

#[test]
fn store_hostile_session_identity_roundtrip() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let corpus = hostile_strings();
    let mut case = 0usize;
    for (i, raw) in corpus.iter().enumerate() {
        case += 1;
        let provider = &corpus[(i + 1) % corpus.len()];
        let model = &corpus[(i + 2) % corpus.len()];
        let row = store
            .create_session(ws, raw, provider, model)
            .unwrap_or_else(|e| panic!("hostile-sql case #{case} (title {raw:?}): {e}"));
        let back = store
            .get_session(row.id)
            .unwrap()
            .unwrap_or_else(|| panic!("hostile-sql case #{case}: session vanished"));
        assert_eq!(
            back.title, *raw,
            "hostile-sql case #{case} (title {raw:?}): title must roundtrip byte-exact"
        );
        assert_eq!(
            back.provider, *provider,
            "hostile-sql case #{case} (provider {provider:?}): provider must roundtrip"
        );
        assert_eq!(
            back.model, *model,
            "hostile-sql case #{case} (model {model:?}): model must roundtrip"
        );
        assert_eq!(
            back.state,
            AgentState::Idle,
            "hostile-sql case #{case}: seeded state must be Idle"
        );
        assert_eq!(
            back.lifecycle,
            SessionLifecycle::Open,
            "hostile-sql case #{case}: seeded lifecycle must be Open"
        );
    }
    assert_eq!(case, 50, "session hostile corpus size drifted");
}

#[test]
fn store_hostile_message_and_part_parameters_roundtrip() {
    let (_d, store) = tmp_store();
    let (_, sid) = ws_sid(&store);
    let corpus = hostile_strings();
    let mut case = 0usize;
    for (i, raw) in corpus.iter().enumerate() {
        case += 1;
        store
            .put_message(
                sid,
                i as i64 + 1,
                raw,
                serde_json::json!({ "text": raw, "i": i }),
            )
            .unwrap_or_else(|e| panic!("hostile-sql case #{case} (role {raw:?}): {e}"));
    }
    let messages = store.messages_before(sid, None, u64::MAX).unwrap();
    assert_eq!(
        messages.len(),
        corpus.len(),
        "hostile-sql: every hostile message row must persist"
    );
    for (i, raw) in corpus.iter().enumerate() {
        case += 1;
        let row = messages
            .iter()
            .find(|m| m.seq == i as i64 + 1)
            .unwrap_or_else(|| panic!("hostile-sql case #{case}: message {i} missing"));
        assert_eq!(
            row.role, *raw,
            "hostile-sql case #{case} (role {raw:?}): role must roundtrip"
        );
        assert_eq!(
            row.data,
            serde_json::json!({ "text": raw, "i": i }),
            "hostile-sql case #{case} (data {raw:?}): JSON payload must roundtrip structurally"
        );
    }
    for (i, raw) in corpus.iter().enumerate() {
        case += 1;
        let message_id = store
            .put_message(sid, 10_000 + i as i64, "u", serde_json::json!({}))
            .unwrap();
        store
            .put_part(message_id, raw, serde_json::json!({ "kind": raw }))
            .unwrap_or_else(|e| panic!("hostile-sql case #{case} (part kind {raw:?}): {e}"));
        let parts = store.parts_of(message_id).unwrap();
        assert_eq!(
            parts.len(),
            1,
            "hostile-sql case #{case} (part kind {raw:?}): one part row"
        );
        assert_eq!(
            parts[0].kind, *raw,
            "hostile-sql case #{case} (part kind {raw:?}): kind must roundtrip"
        );
        assert_eq!(
            parts[0].data,
            serde_json::json!({ "kind": raw }),
            "hostile-sql case #{case} (part kind {raw:?}): data must roundtrip"
        );
    }
    assert_eq!(case, 150, "message/part hostile corpus size drifted");
}

#[test]
fn store_hostile_memory_facts_and_artifacts_roundtrip() {
    let (_d, store) = tmp_store();
    let corpus = hostile_strings();
    let mut case = 0usize;
    for (i, raw) in corpus.iter().enumerate() {
        case += 1;
        let ws = store.create_workspace(&format!("/w{i}")).unwrap();
        let sid = store.create_session(ws, "t", "p", "m").unwrap().id;
        store
            .upsert_memory_fact(sid, raw, raw, raw)
            .unwrap_or_else(|e| panic!("hostile-sql case #{case} (fact {raw:?}): {e}"));
        let facts = store.memory_facts(sid).unwrap();
        assert_eq!(
            facts,
            vec![(raw.clone(), raw.clone(), raw.clone())],
            "hostile-sql case #{case} (fact {raw:?}): kind/key/value must roundtrip byte-exact"
        );
        case += 1;
        let hash = format!("cas-{i}-{raw}");
        store
            .put_artifact(sid, raw, &hash, raw, i as i64)
            .unwrap_or_else(|e| panic!("hostile-sql case #{case} (artifact {raw:?}): {e}"));
        let (summary, kind) = store
            .artifact_for(sid, &hash)
            .unwrap()
            .unwrap_or_else(|| panic!("hostile-sql case #{case}: artifact vanished"));
        assert_eq!(
            (summary.as_str(), kind.as_str()),
            (raw.as_str(), raw.as_str()),
            "hostile-sql case #{case} (artifact {raw:?}): summary/kind must roundtrip"
        );
        case += 1;
        let state_json = format!("{{\"raw\": {}}}", serde_json::to_string(raw).unwrap());
        store
            .index_state_put(ws, &state_json, i as i64, raw)
            .unwrap_or_else(|e| panic!("hostile-sql case #{case} (index {raw:?}): {e}"));
        let row: IndexStateRow = store.index_state_get(ws).unwrap().unwrap();
        assert_eq!(
            row.state_json, state_json,
            "hostile-sql case #{case} (index {raw:?}): state JSON must roundtrip"
        );
    }
    assert_eq!(
        case, 150,
        "memory/artifact/index hostile corpus size drifted"
    );
}

#[test]
fn store_hostile_ledger_entry_type_and_schema_bounds() {
    let (_d, store) = tmp_store();
    let (_, sid) = ws_sid(&store);
    let mut case = 0usize;
    for len in [1usize, 2, 63, 64] {
        case += 1;
        let entry_type = "e".repeat(len);
        let seq = store
            .append_ledger_entry(sid, &entry_type, 1, serde_json::json!({"n": len}))
            .unwrap_or_else(|e| panic!("hostile-sql case #{case} (entry_type len {len}): {e}"));
        assert!(seq >= 1, "hostile-sql case #{case}: seq must be positive");
    }
    for len in [65usize, 256, 4096] {
        case += 1;
        match store.append_ledger_entry(sid, &"e".repeat(len), 1, serde_json::json!({})) {
            Err(StoreError::Migration(m)) => assert!(
                m.contains("1..=64"),
                "hostile-sql case #{case} (entry_type len {len}): wrong refusal {m}"
            ),
            other => panic!("hostile-sql case #{case} (entry_type len {len}): expected typed refusal, got {other:?}"),
        }
    }
    case += 1;
    match store.append_ledger_entry(sid, "", 1, serde_json::json!({})) {
        Err(StoreError::Migration(_)) => {}
        other => {
            panic!("hostile-sql case #{case}: empty entry_type must be refused, got {other:?}")
        }
    }
    for bad_ver in [0i64, -1, i64::MIN] {
        case += 1;
        match store.append_ledger_entry(sid, "ok", bad_ver, serde_json::json!({})) {
            Err(StoreError::Migration(m)) => assert!(
                m.contains("schema_ver"),
                "hostile-sql case #{case} (schema_ver {bad_ver}): wrong refusal {m}"
            ),
            other => panic!("hostile-sql case #{case} (schema_ver {bad_ver}): expected typed refusal, got {other:?}"),
        }
    }
    case += 1;
    let hostile_type = "'; DROP TABLE ledger_entry; --";
    store
        .append_ledger_entry(sid, hostile_type, 1, serde_json::json!({"x": 1}))
        .unwrap();
    let rows = store.ledger_entries(sid, None, u64::MAX).unwrap();
    assert!(
        rows.iter().any(|r| r.entry_type == hostile_type),
        "hostile-sql case #{case}: SQL-shaped entry_type must be stored as data"
    );
    assert_eq!(
        store.ledger_max_seq(sid).unwrap(),
        rows.last().unwrap().seq,
        "hostile-sql case #{case}: ledger head after hostile entry_type"
    );
    assert_eq!(case, 12, "COUNT");
}

// =====================================================================
// Category 2: migration matrix
// =====================================================================

fn apply_prefix(root: &Path, count: usize) {
    std::fs::create_dir_all(root).unwrap();
    let conn = rusqlite::Connection::open(root.join("faktor-plus.db")).unwrap();
    for sql in &crate::MIGRATIONS[..count] {
        conn.execute_batch(sql)
            .unwrap_or_else(|e| panic!("fixture prefix {count} failed: {e}"));
    }
    conn.execute_batch(&format!("PRAGMA user_version = {count}"))
        .unwrap();
}

#[test]
fn store_per_version_fixture_roundtrip_matrix() {
    let mut case = 0usize;
    let head = crate::MIGRATIONS.len();
    for version in 0..=head {
        case += 1;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("fixture");
        apply_prefix(&root, version);
        let store = Store::open(&root, true).unwrap_or_else(|e| {
            panic!("migration-matrix case #{case} (fixture v{version}): reopen failed: {e}")
        });
        let w = store.create_workspace("/w").unwrap();
        let s = store.create_session(w, "t", "p", "m").unwrap();
        assert_eq!(
            s.state,
            AgentState::Idle,
            "migration-matrix case #{case} (fixture v{version}): session state after migration chain"
        );
        store
            .put_message(s.id, 1, "user", serde_json::json!({"v": version}))
            .unwrap_or_else(|e| {
                panic!("migration-matrix case #{case} (fixture v{version}): write after migration: {e}")
            });
        let back = store.messages_before(s.id, None, 10).unwrap();
        assert_eq!(
            back.len(),
            1,
            "migration-matrix case #{case} (fixture v{version}): message survives"
        );
        let cursor: i64 = store
            .raw_conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            cursor, head as i64,
            "migration-matrix case #{case} (fixture v{version}): every chain must reach the head"
        );
    }
    assert_eq!(case, 30, "per-version fixture matrix size drifted");
}

#[test]
fn store_torn_migration_cursor_never_half_migrates_silently() {
    let head = crate::MIGRATIONS.len();
    let mut case = 0usize;
    for applied in 1..=head {
        case += 1;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("torn");
        apply_prefix(&root, applied);
        let conn = rusqlite::Connection::open(root.join("faktor-plus.db")).unwrap();
        conn.execute_batch(&format!("PRAGMA user_version = {}", applied - 1))
            .unwrap();
        drop(conn);
        match Store::open(&root, true) {
            Ok(store) => {
                let cursor: i64 = store
                    .raw_conn()
                    .query_row("PRAGMA user_version", [], |r| r.get(0))
                    .unwrap();
                assert_eq!(
                    cursor, head as i64,
                    "torn-migration case #{case} (schema v{applied}, cursor v{}): a successful reopen must complete the chain",
                    applied - 1
                );
                let w = store.create_workspace("/w").unwrap_or_else(|e| {
                    panic!("torn-migration case #{case}: functional write failed: {e}")
                });
                store
                    .create_session(w, "t", "p", "m")
                    .unwrap_or_else(|e| panic!("torn-migration case #{case}: session: {e}"));
            }
            Err(StoreError::Migration(_) | StoreError::Sqlite(_) | StoreError::Corrupt(_)) => {}
            Err(other) => panic!(
                "torn-migration case #{case} (schema v{applied}, cursor v{}): untrusted typed failure class {other:?}",
                applied - 1
            ),
        }
    }
    assert_eq!(case, 29, "torn-migration matrix size drifted");
}

#[test]
fn store_malformed_durable_rows_are_typed_never_guessed() {
    let (_d, store) = tmp_store();
    let (ws, sid) = ws_sid(&store);
    let mut case = 0usize;

    macro_rules! typed_case {
        ($desc:expr, $plant:expr, $probe:expr, $pattern:pat) => {{
            case += 1;
            let store = &store;
            ($plant)(store);
            match ($probe)(store) {
                $pattern => {}
                _ => panic!(
                    "malformed-row case #{case} ({}): expected {}",
                    $desc,
                    stringify!($pattern),
                ),
            }
        }};
    }

    let message_id = store
        .put_message(sid, 1, "user", serde_json::json!({"text": "ok"}))
        .unwrap();
    store
        .put_part(message_id, "text", serde_json::json!({"t": "ok"}))
        .unwrap();
    store
        .append_event(
            sid,
            None,
            EventKind::PromptReceived,
            AgentState::Idle,
            1,
            None,
        )
        .unwrap();
    store
        .put_checkpoint(sid, 1, "a.rs", "b", "a", None)
        .unwrap();
    store.upsert_memory_fact(sid, "k", "key", "value").unwrap();
    store
        .put_ledger_head(sid, serde_json::json!({"n": 1}), 1, 1)
        .unwrap();
    store
        .append_ledger_entry(sid, "entry", 1, serde_json::json!({"n": 1}))
        .unwrap();
    store.upsert_task(&task_row(sid, 1)).unwrap();
    store
        .insert_permission(sid, OpId::new(1), "fs.write")
        .unwrap();
    store
        .enqueue_prompt(sid, OpId::new(2), "p", &[], None, None, None, 1)
        .unwrap();
    store
        .record_provider_call_with_prefix(
            sid,
            OpId::new(3),
            "p",
            "m",
            "ok",
            Some(1),
            Some(1),
            None,
            Some([7u8; 32]),
            Some(9),
            Some(0.5),
        )
        .unwrap();
    let evidence = EvidenceRow {
        id: 0,
        session_id: sid,
        workspace_id: ws,
        task_id: Some(1),
        kind: "k".into(),
        revision: 1,
        provenance_json: "{\"p\":1}".into(),
        compressibility: "low".into(),
        compression_json: "{}".into(),
        retrieval_json: "{}".into(),
        compact_json: "{}".into(),
        backing_cas_hash: Some("cas".into()),
        completeness: "complete".into(),
        created_ms: 1,
    };
    let evidence_id = store.evidence_insert(&evidence).unwrap();
    store
        .put_attachment(
            sid,
            &AttachmentId::new(FileHash::from([3u8; 32]), "text/plain", None, 3).unwrap(),
        )
        .unwrap();
    store.put_artifact(sid, "k", "cas-1", "s", 1).unwrap();
    store
        .index_state_put(ws, "{\"seed\":true}", 0, "seed")
        .unwrap();

    typed_case!(
        "session.state bogus",
        |s: &Store| plant(
            s,
            "UPDATE session SET state = '\"bogus\"' WHERE id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.get_session(SessionId::new(sid.raw())),
        Err(StoreError::Corrupt(_))
    );
    typed_case!(
        "session.state broken json",
        |s: &Store| plant(
            s,
            "UPDATE session SET state = 'broken{' WHERE id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.get_session(sid),
        Err(StoreError::Corrupt(_))
    );
    typed_case!(
        "session.lifecycle bogus",
        |s: &Store| plant(
            s,
            "UPDATE session SET lifecycle = '\"bogus\"' WHERE id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.get_session(sid),
        Err(StoreError::Corrupt(_))
    );
    typed_case!(
        "session.lifecycle broken json",
        |s: &Store| plant(
            s,
            "UPDATE session SET lifecycle = 'broken{' WHERE id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.get_session(sid),
        Err(StoreError::Corrupt(_))
    );
    typed_case!(
        "session.workspace_id zero",
        |s: &Store| plant(
            s,
            "UPDATE session SET workspace_id = 0 WHERE id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.get_session(sid),
        Err(StoreError::Corrupt(_))
    );
    typed_case!(
        "session.workspace_id negative",
        |s: &Store| plant(
            s,
            "UPDATE session SET workspace_id = -1 WHERE id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.get_session(sid),
        Err(StoreError::Corrupt(_))
    );
    typed_case!(
        "event.kind unknown",
        |s: &Store| plant(
            s,
            "UPDATE event SET kind = 'bogus' WHERE session_id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.events_range(sid, 1, None),
        Err(StoreError::Corrupt(_))
    );
    typed_case!(
        "event.state unknown",
        |s: &Store| plant(
            s,
            "UPDATE event SET state = '\"bogus\"' WHERE session_id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.events_range(sid, 1, None),
        Err(StoreError::Corrupt(_))
    );
    typed_case!(
        "event.payload broken json",
        |s: &Store| plant(
            s,
            "UPDATE event SET payload = 'broken{' WHERE session_id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.events_range(sid, 1, None),
        Err(StoreError::Corrupt(_))
    );
    typed_case!(
        "event.op_id zero",
        |s: &Store| plant(
            s,
            "UPDATE event SET op_id = 0 WHERE session_id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.events_range(sid, 1, None),
        Err(StoreError::Corrupt(_))
    );
    case += 1;
    let zero_seq = store.read().unwrap().execute(
        "UPDATE event SET seq = 0 WHERE session_id = ?1 AND seq = 1",
        rusqlite::params![sid.raw() as i64],
    );
    assert!(
        zero_seq.is_err(),
        "malformed-row case #{case} (event.seq zero): the schema must refuse a zero sequence at write time"
    );
    case += 1;
    let negative_seq = store.read().unwrap().execute(
        "UPDATE event SET seq = -7 WHERE session_id = ?1 AND seq = 1",
        rusqlite::params![sid.raw() as i64],
    );
    assert!(
        negative_seq.is_err(),
        "malformed-row case #{case} (event.seq negative): the schema must refuse a negative sequence at write time"
    );
    typed_case!(
        "message.data broken json",
        |s: &Store| plant(
            s,
            "UPDATE message SET data = 'broken{' WHERE session_id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.messages_before(sid, None, 10),
        Err(StoreError::Corrupt(_))
    );
    typed_case!(
        "message.session_id zero is invisible to owner-scoped reads",
        |s: &Store| plant(
            s,
            "UPDATE message SET session_id = 0 WHERE session_id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.messages_before(sid, None, 10),
        Ok(_)
    );
    typed_case!(
        "part.data blob",
        |s: &Store| plant(
            s,
            "UPDATE part SET data = x'FF' WHERE message_id = ?1",
            &[message_id.into()]
        ),
        |s: &Store| s.parts_of(message_id),
        Err(_)
    );
    typed_case!(
        "checkpoint.before_hash undecodable blob",
        |s: &Store| plant(
            s,
            "UPDATE checkpoint SET before_hash = x'FF' WHERE session_id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.checkpoints_of(sid),
        Err(_)
    );
    typed_case!(
        "ledger_head.head_json broken",
        |s: &Store| plant(
            s,
            "UPDATE ledger_head SET head_json = 'broken{' WHERE session_id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.ledger_head(sid),
        Err(StoreError::Corrupt(_))
    );
    typed_case!(
        "ledger_entry.payload broken",
        |s: &Store| plant(
            s,
            "UPDATE ledger_entry SET payload = 'broken{' WHERE session_id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.ledger_entries(sid, None, u64::MAX),
        Err(StoreError::Corrupt(_))
    );
    typed_case!(
        "task.state bogus",
        |s: &Store| plant(
            s,
            "UPDATE task SET state = '\"bogus\"' WHERE session_id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.get_task(sid, TaskId::new(1)),
        Err(StoreError::Corrupt(_))
    );
    typed_case!(
        "task.criteria broken",
        |s: &Store| plant(
            s,
            "UPDATE task SET acceptance_criteria = 'broken{' WHERE session_id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.get_task(sid, TaskId::new(1)),
        Err(StoreError::Corrupt(_))
    );
    typed_case!(
        "task.revision zero",
        |s: &Store| plant(
            s,
            "UPDATE task SET revision = 0 WHERE session_id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.get_task(sid, TaskId::new(1)),
        Err(StoreError::Corrupt(_))
    );
    typed_case!(
        "task.plan broken",
        |s: &Store| plant(
            s,
            "UPDATE task SET plan = 'broken{' WHERE session_id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.get_task(sid, TaskId::new(1)),
        Err(StoreError::Corrupt(_))
    );
    typed_case!(
        "provider_call prefix hash wrong length",
        |s: &Store| plant(
            s,
            "UPDATE provider_call SET prompt_prefix_hash = x'00' WHERE session_id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.provider_call_prefix_rows(sid),
        Err(StoreError::Malformed(_))
    );
    typed_case!(
        "provider_call prompt_tokens negative",
        |s: &Store| plant(
            s,
            "UPDATE provider_call SET prompt_tokens = -1 WHERE session_id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.provider_call_prefix_rows(sid),
        Err(StoreError::Malformed(_))
    );
    typed_case!(
        "provider_call stability out of range",
        |s: &Store| plant(
            s,
            "UPDATE provider_call SET prefix_stability = 2.0 WHERE session_id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.provider_call_prefix_rows(sid),
        Err(StoreError::Malformed(_))
    );
    typed_case!(
        "prompt_queue.files broken",
        |s: &Store| plant(
            s,
            "UPDATE prompt_queue SET files = 'broken{' WHERE session_id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.queue_head(sid),
        Err(StoreError::Corrupt(_))
    );
    typed_case!(
        "prompt_queue.op_id zero",
        |s: &Store| plant(
            s,
            "UPDATE prompt_queue SET op_id = 0 WHERE session_id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.queue_head(sid),
        Err(StoreError::Corrupt(_))
    );
    typed_case!(
        "evidence.revision zero",
        |s: &Store| plant(
            s,
            "UPDATE evidence SET revision = 0 WHERE id = ?1",
            &[(evidence_id as i64).into()]
        ),
        |s: &Store| s.evidence_get(evidence_id),
        Err(StoreError::Corrupt(_))
    );
    typed_case!(
        "evidence.id negative",
        |s: &Store| plant(
            s,
            "UPDATE evidence SET id = -3 WHERE id = ?1",
            &[(evidence_id as i64).into()]
        ),
        |s: &Store| s.evidence_ids_by_backing("cas", 10),
        Err(StoreError::Corrupt(_))
    );
    typed_case!(
        "attachment.digest not hex",
        |s: &Store| plant(
            s,
            "UPDATE attachment SET digest = 'zz' WHERE session_id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.list_attachments(sid, 10),
        Err(_)
    );
    typed_case!(
        "attachment.size negative",
        |s: &Store| plant(
            s,
            "UPDATE attachment SET size = -1 WHERE session_id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.list_attachments(sid, 10),
        Err(_)
    );
    typed_case!(
        "artifact.summary undecodable blob",
        |s: &Store| plant(
            s,
            "UPDATE artifact SET summary = x'FF' WHERE session_id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.artifact_for(sid, "cas-1"),
        Err(StoreError::Corrupt(_))
    );
    typed_case!(
        "artifact.kind undecodable blob",
        |s: &Store| plant(
            s,
            "UPDATE artifact SET kind = x'FE' WHERE session_id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.artifact_for(sid, "cas-1"),
        Err(StoreError::Corrupt(_))
    );
    typed_case!(
        "permission.op_id zero",
        |s: &Store| plant(
            s,
            "UPDATE permission SET op_id = 0 WHERE session_id = ?1",
            &[(sid.raw() as i64).into()]
        ),
        |s: &Store| s.expired_pending_permissions(sid, i64::MAX),
        Err(StoreError::Corrupt(_))
    );
    case += 1;
    let bogus_status = store.read().unwrap().execute(
        "INSERT INTO cost_reservation(session_id, task_id, op_id, predicted_micro, status, created_ms)
         VALUES (?1, 1, 1, 5, 'bogus', 1)",
        rusqlite::params![sid.raw() as i64],
    );
    assert!(
        bogus_status.is_err(),
        "malformed-row case #{case} (cost_reservation.status bogus): the closed status vocabulary must be enforced at the schema"
    );
    typed_case!(
        "index_state generation extreme is opaque, never normalized",
        |s: &Store| plant(
            s,
            "UPDATE index_state SET generation = -9223372036854775808 WHERE workspace_id = ?1",
            &[(ws.raw() as i64).into()]
        ),
        |s: &Store| s.index_state_get(ws),
        Ok(Some(_))
    );
    typed_case!(
        "evidence.backing hash hostile is data",
        |s: &Store| plant(
            s,
            "UPDATE evidence SET backing_cas_hash = 'zz' WHERE id = ?1",
            &[(evidence_id as i64).into()]
        ),
        |s: &Store| s.evidence_ids_by_backing("zz", 10),
        Ok(_)
    );
    assert_eq!(case, 37, "COUNT");
}

// =====================================================================
// Category 3: journal / event semantics
// =====================================================================

#[test]
fn store_journal_sequence_semantics() {
    let (_d, store) = tmp_store();
    let (_, sid) = ws_sid(&store);
    let (_, sid2) = ws_sid(&store);
    let mut case = 0usize;
    for n in 1..=20u64 {
        case += 1;
        let seq = store
            .append_event(
                sid,
                None,
                EventKind::PhaseChanged,
                AgentState::Preparing,
                n as i64,
                Some(serde_json::json!({"n": n})),
            )
            .unwrap();
        assert_eq!(
            seq.raw(),
            n + 1,
            "journal case #{case} (append {n}): gapless sequence after the seed event"
        );
    }
    let events = store.events_range(sid, 1, None).unwrap();
    assert_eq!(
        events.iter().map(|e| e.seq.raw()).collect::<Vec<_>>(),
        (1..=21).collect::<Vec<_>>(),
        "journal: contiguous 1..=21 with no gap"
    );
    for i in 0..5u64 {
        case += 1;
        let seq = store
            .append_event(
                sid2,
                None,
                EventKind::PhaseChanged,
                AgentState::Preparing,
                i as i64,
                None,
            )
            .unwrap();
        assert_eq!(
            seq.raw(),
            i + 2,
            "journal case #{case}: second session has its own sequence (after its own seed)"
        );
    }
    case += 1;
    assert!(
        store
            .events_range(sid, 1, None)
            .unwrap()
            .iter()
            .all(|e| e.session_id == sid),
        "journal case #{case}: cross-session isolation on read"
    );
    case += 1;
    assert_eq!(
        store.last_event_seq(sid).unwrap(),
        Some(EventSeq::new(21)),
        "journal case #{case}: per-session last seq"
    );
    case += 1;
    assert_eq!(
        store.last_event_seq(sid2).unwrap(),
        Some(EventSeq::new(6)),
        "journal case #{case}: second session last seq"
    );
    case += 1;
    assert!(
        store
            .journal_session_problems(sid, JournalDepth::Deep)
            .unwrap()
            .is_empty(),
        "journal case #{case}: a clean appended journal has no problems"
    );
    assert_eq!(case, 29, "COUNT");
}

#[test]
fn store_journal_gap_reorder_and_first_event_corruption() {
    let (_d, store) = tmp_store();
    let (_, sid) = ws_sid(&store);
    let mut case = 0usize;

    case += 1;
    plant(
        &store,
        "INSERT INTO event(seq, session_id, op_id, kind, state, ts_ms, payload, payload_ver)
         VALUES (100, ?1, NULL, 'phase_changed', '\"preparing\"', 5, NULL, 1)",
        &[(sid.raw() as i64).into()],
    );
    let problems = store
        .journal_session_problems(sid, JournalDepth::Deep)
        .unwrap();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("gap") || p.contains("sequence")),
        "journal case #{case}: planted seq gap must be reported: {problems:?}"
    );
    case += 1;
    let next = store
        .append_event(
            sid,
            None,
            EventKind::PhaseChanged,
            AgentState::Preparing,
            6,
            None,
        )
        .unwrap();
    assert_eq!(
        next.raw(),
        101,
        "journal case #{case}: append after a gap continues from MAX(seq), never re-fills the hole"
    );
    case += 1;
    assert_eq!(
        store.last_event_seq(sid).unwrap(),
        Some(EventSeq::new(101)),
        "journal case #{case}: MAX(seq) after the gap"
    );

    let dir = tempfile::tempdir().unwrap();
    let store2 = Store::open(dir.path(), true).unwrap();
    let ws2 = store2.create_workspace("/w").unwrap();
    let sid2 = store2.create_session(ws2, "t", "p", "m").unwrap().id;
    case += 1;
    plant(
        &store2,
        "UPDATE event SET kind = 'tool_completed', state = '\"executing_tool\"' WHERE session_id = ?1 AND seq = 1",
        &[(sid2.raw() as i64).into()],
    );
    let problems = store2
        .journal_session_problems(sid2, JournalDepth::Deep)
        .unwrap();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("session_created") || p.contains("first")),
        "journal case #{case}: a wrong first event must be reported: {problems:?}"
    );

    let dir = tempfile::tempdir().unwrap();
    let store3 = Store::open(dir.path(), true).unwrap();
    let ws3 = store3.create_workspace("/w").unwrap();
    let sid3 = store3.create_session(ws3, "t", "p", "m").unwrap().id;
    store3
        .append_event(
            sid3,
            None,
            EventKind::PhaseChanged,
            AgentState::Preparing,
            5,
            None,
        )
        .unwrap();
    case += 1;
    plant(
        &store3,
        "UPDATE event SET ts_ms = 0 WHERE session_id = ?1 AND seq = 2",
        &[(sid3.raw() as i64).into()],
    );
    let problems = store3
        .journal_session_problems(sid3, JournalDepth::Deep)
        .unwrap();
    assert!(
        !problems.is_empty(),
        "journal case #{case}: a decremented timestamp must be reported"
    );
    case += 1;
    assert!(
        store3
            .journal_consistency_issues()
            .unwrap()
            .iter()
            .any(|p| p.contains(&sid3.to_string()) || p.contains("timestamp")),
        "journal case #{case}: whole-store consistency sweep must surface the torn session"
    );
    assert_eq!(case, 6, "COUNT");
}

#[test]
fn store_journal_timestamp_monotonicity_on_append() {
    let (_d, store) = tmp_store();
    let (_, sid) = ws_sid(&store);
    let mut case = 0usize;
    let stamps: [i64; 8] = [10_000, 10_001, 9_999, 0, -5, 10_002, i64::MIN, i64::MAX];
    let mut prev = store.events_range(sid, 1, None).unwrap()[0].ts_ms;
    for now in &stamps {
        case += 1;
        let seq = store
            .append_event(
                sid,
                None,
                EventKind::PhaseChanged,
                AgentState::Preparing,
                *now,
                None,
            )
            .unwrap();
        let stored = store
            .events_range(sid, seq.raw(), Some(1))
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .ts_ms;
        let expected = prev.max(*now);
        assert_eq!(
            stored, expected,
            "journal-ts case #{case} (stamp {now}, prev {prev}): stored time must clamp backwards clocks"
        );
        assert!(
            stored >= prev,
            "journal-ts case #{case} (stamp {now}): never decreases"
        );
        prev = stored;
    }
    assert_eq!(case, 8, "COUNT");
}

#[test]
fn store_journal_overflow_and_versioned_reads() {
    let (_d, store) = tmp_store();
    let (_, sid) = ws_sid(&store);
    let mut case = 0usize;

    case += 1;
    plant(
        &store,
        "UPDATE event SET seq = 9223372036854775807 WHERE session_id = ?1 AND seq = 1",
        &[(sid.raw() as i64).into()],
    );
    match store.append_event(
        sid,
        None,
        EventKind::PhaseChanged,
        AgentState::Preparing,
        1,
        None,
    ) {
        Err(StoreError::Corrupt(messages)) => assert!(
            messages.iter().any(|m| m.contains("overflow")),
            "journal case #{case}: overflow must name itself: {messages:?}"
        ),
        other => panic!("journal case #{case}: expected typed overflow, got {other:?}"),
    }

    let dir = tempfile::tempdir().unwrap();
    let store2 = Store::open(dir.path(), true).unwrap();
    let ws2 = store2.create_workspace("/w").unwrap();
    let sid2 = store2.create_session(ws2, "t", "p", "m").unwrap().id;
    case += 1;
    store2
        .append_event_v(
            sid2,
            None,
            EventKind::PhaseChanged,
            AgentState::Preparing,
            1,
            None,
            999,
        )
        .unwrap();
    let rows = store2.events_versioned_range(sid2, 1, None).unwrap();
    assert_eq!(
        rows[1].1, 999,
        "journal case #{case}: an unknown payload version is preserved for the typed reader"
    );
    let plain = store2.events_range(sid2, 1, None).unwrap();
    assert_eq!(
        plain.len(),
        2,
        "journal case #{case}: the unversioned read must not silently drop versioned rows"
    );
    case += 1;
    assert_eq!(
        store2
            .events_after(sid2, EventSeq::new(u64::MAX))
            .unwrap()
            .len(),
        0,
        "journal case #{case}: a u64::MAX cursor can never wrap into the whole journal"
    );
    case += 1;
    assert_eq!(
        store2
            .events_after(sid2, EventSeq::new(i64::MAX as u64))
            .unwrap()
            .len(),
        0,
        "journal case #{case}: an i64::MAX cursor is empty"
    );
    case += 1;
    assert_eq!(
        store2.events_after(sid2, EventSeq::new(1)).unwrap().len(),
        1,
        "journal case #{case}: after=1 returns the single later event"
    );
    case += 1;
    assert_eq!(
        store2.events_range(sid2, u64::MAX, None).unwrap().len(),
        0,
        "journal case #{case}: from_seq above i64::MAX is empty, never negative"
    );
    assert_eq!(case, 6, "COUNT");
}

#[test]
fn store_transition_session_expectations_and_replay() {
    let (_d, store) = tmp_store();
    let (_, sid) = ws_sid(&store);
    let mut case = 0usize;
    let transition = |state: AgentState| SessionTransition {
        expected_lifecycle: Some(SessionLifecycle::Open),
        new_lifecycle: None,
        expected_state: Some(AgentState::Idle),
        new_state: state,
        event_kind: EventKind::PhaseChanged,
        event_payload: None,
        event_payload_ver: 1,
    };
    case += 1;
    let seq = store
        .transition_session(sid, None, transition(AgentState::Preparing))
        .unwrap();
    assert_eq!(
        seq.raw(),
        2,
        "transition case #{case}: transition journals seq 2"
    );
    case += 1;
    match store.transition_session(sid, None, transition(AgentState::Preparing)) {
        Err(StoreError::Conflict(m)) => assert!(
            m.contains("expected"),
            "transition case #{case}: stale expectation must name the mismatch: {m}"
        ),
        other => panic!("transition case #{case}: expected typed conflict, got {other:?}"),
    }
    case += 1;
    match store.transition_session(
        SessionId::new(9999),
        None,
        transition(AgentState::Preparing),
    ) {
        Err(StoreError::Conflict(m)) => assert!(
            m.contains("does not exist"),
            "transition case #{case}: missing session conflict text: {m}"
        ),
        other => panic!("transition case #{case}: expected typed conflict, got {other:?}"),
    }
    case += 1;
    let seq = store
        .transition_session(
            sid,
            None,
            SessionTransition {
                expected_lifecycle: Some(SessionLifecycle::Open),
                new_lifecycle: Some(SessionLifecycle::Closing),
                expected_state: Some(AgentState::Preparing),
                new_state: AgentState::Completed,
                event_kind: EventKind::SessionEnded,
                event_payload: Some(serde_json::json!({"end": true})),
                event_payload_ver: 1,
            },
        )
        .unwrap();
    assert_eq!(
        seq.raw(),
        3,
        "transition case #{case}: lifecycle transition journals seq 3"
    );
    let row = store.get_session(sid).unwrap().unwrap();
    assert_eq!(
        row.state,
        AgentState::Completed,
        "transition case #{case}: state landed"
    );
    assert_eq!(
        row.lifecycle,
        SessionLifecycle::Closing,
        "transition case #{case}: lifecycle landed"
    );
    case += 1;
    match store.transition_session(
        sid,
        None,
        SessionTransition {
            expected_lifecycle: Some(SessionLifecycle::Open),
            new_lifecycle: None,
            expected_state: None,
            new_state: AgentState::Idle,
            event_kind: EventKind::PhaseChanged,
            event_payload: None,
            event_payload_ver: 1,
        },
    ) {
        Err(StoreError::Conflict(m)) => assert!(
            m.contains("lifecycle"),
            "transition case #{case}: lifecycle mismatch must be typed: {m}"
        ),
        other => {
            panic!("transition case #{case}: expected typed lifecycle conflict, got {other:?}")
        }
    }
    case += 1;
    assert!(
        store
            .journal_session_problems(sid, JournalDepth::Deep)
            .unwrap()
            .is_empty(),
        "transition case #{case}: the atomic transitions leave a consistent journal"
    );
    assert_eq!(case, 6, "COUNT");
}

#[test]
fn store_permission_expiry_is_durable_and_idempotent() {
    let (_d, store) = tmp_store();
    let (_, sid) = ws_sid(&store);
    let mut case = 0usize;
    let (p1, expires) = store
        .insert_permission(sid, OpId::new(11), "fs.write")
        .unwrap();
    let (p2, _) = store
        .insert_permission(sid, OpId::new(12), "shell.exec")
        .unwrap();
    assert!(expires > 0);
    case += 1;
    assert_eq!(
        store
            .pending_permission(p1)
            .unwrap()
            .map(|(s, _, c)| (s, c)),
        Some((sid, "fs.write".to_string())),
        "permission case #{case}: unexpired row is pending"
    );
    case += 1;
    assert_eq!(
        store
            .expired_pending_permissions(sid, expires + 5_000)
            .unwrap()
            .len(),
        2,
        "permission case #{case}: every passed deadline is listed (expires_ms <= now)"
    );
    let event = CommandEvent {
        kind: EventKind::PermissionExpired,
        state: AgentState::ReadyForNextTurn,
        op_id: None,
        ts_ms: expires + 1,
        payload: Some(serde_json::json!({"expired": []})),
        payload_ver: 1,
    };
    case += 1;
    let resolution = store
        .expire_pending_permissions_for_session(
            sid,
            expires + 5_000,
            AgentState::Idle,
            event.clone(),
        )
        .unwrap();
    assert_eq!(
        resolution.expired,
        vec![(p1, OpId::new(11)), (p2, OpId::new(12))],
        "permission case #{case}: both pending rows terminalize with their op ids"
    );
    assert!(
        resolution.event_seq.is_some(),
        "permission case #{case}: one event journaled"
    );
    case += 1;
    assert_eq!(
        store.permission_decision(p1).unwrap().as_deref(),
        Some("expired"),
        "permission case #{case}: decision is terminal `expired`, never a fake deny"
    );
    case += 1;
    let again = store
        .expire_pending_permissions_for_session(
            sid,
            expires + 5_001,
            AgentState::ReadyForNextTurn,
            event,
        )
        .unwrap();
    assert!(
        again.is_empty() && again.event_seq.is_none(),
        "permission case #{case}: a second sweep is byte-for-byte invisible"
    );
    case += 1;
    match store.resolve_permission(p1, sid, "allow") {
        Err(StoreError::Conflict(m)) => assert!(
            m.contains("not pending"),
            "permission case #{case}: resolving an expired row is a typed conflict: {m}"
        ),
        other => panic!("permission case #{case}: expected typed conflict, got {other:?}"),
    }
    case += 1;
    match store.expire_pending_permissions_for_session(
        sid,
        expires + 5_002,
        AgentState::Idle,
        CommandEvent {
            kind: EventKind::PermissionGranted,
            state: AgentState::Idle,
            op_id: None,
            ts_ms: 0,
            payload: None,
            payload_ver: 1,
        },
    ) {
        Err(StoreError::Migration(m)) => assert!(
            m.contains("PermissionExpired"),
            "permission case #{case}: a wrong event kind must be refused typed: {m}"
        ),
        other => panic!("permission case #{case}: expected typed misuse refusal, got {other:?}"),
    }
    assert_eq!(case, 7, "COUNT");
}

// =====================================================================
// Category 4: admission / CAS
// =====================================================================

fn admission_fixture() -> (tempfile::TempDir, Store, SessionId) {
    let (d, store) = tmp_store();
    let (_, sid) = ws_sid(&store);
    (d, store, sid)
}

#[test]
fn store_task_admission_claim_matrix() {
    let (_d, store, sid) = admission_fixture();
    let ws2 = store.create_workspace("/w2").unwrap();
    let sid2 = store.create_session(ws2, "t", "p", "m").unwrap().id;
    let mut case = 0usize;
    let keys = [
        "k1",
        "11111111-2222-3333-4444-555555555555",
        "a'b",
        "\"quoted\"",
        "k\u{0}null",
        "日本語key",
        "k with spaces",
        "k/../../etc",
        "k; DROP TABLE task_admission; --",
        &"K".repeat(MAX_TASK_ADMISSION_KEY_BYTES),
    ];
    let digests = [
        "d1",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        &"D".repeat(MAX_TASK_ADMISSION_DIGEST_BYTES),
    ];
    for (i, key) in keys.iter().enumerate() {
        case += 1;
        let digest = digests[i % digests.len()];
        assert_eq!(
            store.task_admission_claim(sid, key, digest, 1).unwrap(),
            TaskAdmissionClaim::Fresh,
            "admission case #{case} (key {key:?}): first claim is Fresh"
        );
        case += 1;
        assert_eq!(
            store.task_admission_claim(sid, key, digest, 2).unwrap(),
            TaskAdmissionClaim::InFlight,
            "admission case #{case} (key {key:?}): duplicate while pending is InFlight"
        );
        case += 1;
        assert_eq!(
            store.task_admission_peek(sid, key, digest).unwrap(),
            Some(TaskAdmissionClaim::InFlight),
            "admission case #{case} (key {key:?}): peek mirrors the claim without writing"
        );
        let receipt = format!("{{\"key\":{}}}", serde_json::to_string(key).unwrap());
        case += 1;
        store.task_admission_complete(sid, key, &receipt).unwrap();
        assert_eq!(
            store.task_admission_claim(sid, key, digest, 3).unwrap(),
            TaskAdmissionClaim::Complete(receipt.clone()),
            "admission case #{case} (key {key:?}): completed replay returns the byte-exact receipt"
        );
        case += 1;
        assert_eq!(
            store
                .task_admission_claim(sid, key, digests[(i + 1) % digests.len()], 4)
                .unwrap(),
            TaskAdmissionClaim::KeyReused {
                stored_digest: digest.to_string()
            },
            "admission case #{case} (key {key:?}): a different digest names the stored digest"
        );
        case += 1;
        assert_eq!(
            store.task_admission_claim(sid2, key, digest, 5).unwrap(),
            TaskAdmissionClaim::KeyReused {
                stored_digest: digest.to_string()
            },
            "admission case #{case} (key {key:?}): a foreign session can never reuse the key"
        );
        case += 1;
        store.task_admission_release(sid2, key).unwrap();
        assert_eq!(
            store.task_admission_claim(sid, key, digest, 6).unwrap(),
            TaskAdmissionClaim::Complete(receipt),
            "admission case #{case} (key {key:?}): release is a no-op on a completed row"
        );
    }
    assert_eq!(case, 70, "task admission matrix size drifted");
}

#[test]
fn store_task_admission_bounds_and_corruption() {
    let (_d, store, sid) = admission_fixture();
    let mut case = 0usize;
    let too_long_key = "k".repeat(MAX_TASK_ADMISSION_KEY_BYTES + 1);
    case += 1;
    assert!(matches!(
        store.task_admission_claim(sid, &too_long_key, "d", 1),
        Err(StoreError::Oversized(_))
    ));
    case += 1;
    assert!(matches!(
        store.task_admission_claim(sid, "", "d", 1),
        Err(StoreError::Oversized(_))
    ));
    let too_long_digest = "d".repeat(MAX_TASK_ADMISSION_DIGEST_BYTES + 1);
    case += 1;
    assert!(matches!(
        store.task_admission_claim(sid, "k", &too_long_digest, 1),
        Err(StoreError::Oversized(_))
    ));
    case += 1;
    assert!(matches!(
        store.task_admission_claim(sid, "k", "", 1),
        Err(StoreError::Oversized(_))
    ));
    store.task_admission_claim(sid, "k", "d", 1).unwrap();
    let huge = "x".repeat(MAX_TASK_ADMISSION_RECEIPT_BYTES + 1);
    case += 1;
    assert!(matches!(
        store.task_admission_complete(sid, "k", &huge),
        Err(StoreError::Oversized(_))
    ));
    case += 1;
    assert_eq!(
        store.task_admission_claim(sid, "k", "d", 2).unwrap(),
        TaskAdmissionClaim::InFlight,
        "a refused oversized receipt never mutates the pending row"
    );
    case += 1;
    match store.task_admission_complete(sid, "missing", "r") {
        Err(StoreError::Conflict(m)) => assert!(m.contains("no admission row"), "{m}"),
        other => panic!("admission case #{case}: {other:?}"),
    }
    case += 1;
    store.task_admission_complete(sid, "k", "r").unwrap();
    match store.task_admission_complete(sid, "k", "r2") {
        Err(StoreError::Conflict(m)) => assert!(m.contains("not pending"), "{m}"),
        other => panic!("admission case #{case}: {other:?}"),
    }
    case += 1;
    store
        .read()
        .unwrap()
        .execute_batch(
            "DROP TABLE task_admission;
             CREATE TABLE task_admission (
                key TEXT PRIMARY KEY,
                session_id INTEGER NOT NULL,
                request_digest TEXT NOT NULL,
                state TEXT NOT NULL,
                receipt_json TEXT,
                created_ms INTEGER NOT NULL
             );",
        )
        .unwrap();
    plant(
        &store,
        "INSERT INTO task_admission(key, session_id, request_digest, state, receipt_json, created_ms)
         VALUES ('k', ?1, 'd', 'bogus', NULL, 0)",
        &[(sid.raw() as i64).into()],
    );
    match store.task_admission_claim(sid, "k", "d", 3) {
        Err(StoreError::Corrupt(_)) => {}
        other => panic!("admission case #{case}: unknown state must be loud corruption: {other:?}"),
    }
    case += 1;
    plant(
        &store,
        "UPDATE task_admission SET state = 'complete', receipt_json = NULL WHERE key = 'k'",
        &[],
    );
    match store.task_admission_claim(sid, "k", "d", 4) {
        Err(StoreError::Corrupt(_)) => {}
        other => {
            panic!("admission case #{case}: receipt-less complete must be corruption: {other:?}")
        }
    }
    case += 1;
    plant(
        &store,
        "UPDATE task_admission SET receipt_json = ?1 WHERE key = 'k'",
        &[rusqlite::types::Value::Text(huge)],
    );
    match store.task_admission_peek(sid, "k", "d") {
        Err(StoreError::Oversized(_)) => {}
        other => {
            panic!("admission case #{case}: oversized injected receipt must refuse: {other:?}")
        }
    }
    assert_eq!(case, 11, "COUNT");
}

#[test]
fn store_prompt_admission_claim_matrix() {
    let (_d, store, sid) = admission_fixture();
    let ws2 = store.create_workspace("/w2").unwrap();
    let sid2 = store.create_session(ws2, "t", "p", "m").unwrap().id;
    let mut case = 0usize;
    let keys = [
        "p1",
        "quote'key",
        "double\"key",
        "nul\u{0}key",
        "日本語",
        " spaced ",
        "p/../x",
        &"P".repeat(MAX_PROMPT_ADMISSION_KEY_BYTES),
    ];
    for (i, key) in keys.iter().enumerate() {
        case += 1;
        let digest = format!("digest-{i}");
        assert_eq!(
            store.prompt_admission_claim(sid, key, &digest, 1).unwrap(),
            PromptAdmissionClaim::Fresh,
            "prompt-admission case #{case} (key {key:?}): Fresh"
        );
        case += 1;
        assert_eq!(
            store.prompt_admission_claim(sid, key, &digest, 2).unwrap(),
            PromptAdmissionClaim::InFlight,
            "prompt-admission case #{case} (key {key:?}): InFlight"
        );
        let receipt = format!("{{\"r\":{i}}}");
        case += 1;
        store.prompt_admission_complete(sid, key, &receipt).unwrap();
        assert_eq!(
            store.prompt_admission_claim(sid, key, &digest, 3).unwrap(),
            PromptAdmissionClaim::Complete(receipt),
            "prompt-admission case #{case} (key {key:?}): byte-exact replay"
        );
        case += 1;
        assert_eq!(
            store.prompt_admission_claim(sid2, key, &digest, 4).unwrap(),
            PromptAdmissionClaim::KeyReused {
                stored_digest: digest.clone()
            },
            "prompt-admission case #{case} (key {key:?}): cross-session reuse"
        );
    }
    case += 1;
    let too_long = "k".repeat(MAX_PROMPT_ADMISSION_KEY_BYTES + 1);
    assert!(matches!(
        store.prompt_admission_claim(sid, &too_long, "d", 1),
        Err(StoreError::Oversized(_))
    ));
    case += 1;
    let too_long_digest = "d".repeat(MAX_PROMPT_ADMISSION_DIGEST_BYTES + 1);
    assert!(matches!(
        store.prompt_admission_claim(sid, "k", &too_long_digest, 1),
        Err(StoreError::Oversized(_))
    ));
    case += 1;
    let huge = "x".repeat(MAX_PROMPT_ADMISSION_RECEIPT_BYTES + 1);
    store
        .prompt_admission_claim(sid, "pending", "d", 1)
        .unwrap();
    assert!(matches!(
        store.prompt_admission_complete(sid, "pending", &huge),
        Err(StoreError::Oversized(_))
    ));
    case += 1;
    assert_eq!(
        store
            .prompt_admission_claim(sid, "pending", "d", 2)
            .unwrap(),
        PromptAdmissionClaim::InFlight,
        "a refused oversized prompt receipt leaves the pending row untouched"
    );
    case += 1;
    store
        .read()
        .unwrap()
        .execute_batch(
            "DROP TABLE prompt_admission;
             CREATE TABLE prompt_admission (
                key TEXT PRIMARY KEY,
                session_id INTEGER NOT NULL,
                request_digest TEXT NOT NULL,
                state TEXT NOT NULL,
                receipt_json TEXT,
                created_ms INTEGER NOT NULL
             );",
        )
        .unwrap();
    plant(
        &store,
        "INSERT INTO prompt_admission(key, session_id, request_digest, state, receipt_json, created_ms)
         VALUES ('pending', ?1, 'd', 'bogus', NULL, 0)",
        &[(sid.raw() as i64).into()],
    );
    match store.prompt_admission_claim(sid, "pending", "d", 3) {
        Err(StoreError::Corrupt(_)) => {}
        other => panic!("prompt-admission case #{case}: {other:?}"),
    }
    assert_eq!(case, 37, "prompt admission matrix size drifted");
}

#[test]
fn store_admission_concurrent_claimers_and_independence() {
    let (_d, store, sid) = admission_fixture();
    let store = Arc::new(store);
    let mut case = 0usize;
    for round in 0..4 {
        case += 1;
        let key = format!("concurrent-{round}");
        let mut handles = Vec::new();
        for t in 0..8 {
            let store = Arc::clone(&store);
            let key = key.clone();
            handles.push(std::thread::spawn(move || {
                store.task_admission_claim(sid, &key, "d", t).unwrap()
            }));
        }
        let claims: Vec<TaskAdmissionClaim> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        let fresh = claims.iter().filter(|c| c.is_fresh()).count();
        assert_eq!(
            fresh, 1,
            "admission-concurrency case #{case}: exactly one claimer wins: {claims:?}"
        );
        assert_eq!(
            claims.len() - fresh,
            claims.iter().filter(|c| c.is_in_flight()).count(),
            "admission-concurrency case #{case}: every loser is InFlight"
        );
    }
    for i in 0..8u64 {
        case += 1;
        assert_eq!(
            store
                .prompt_admission_claim(sid, &format!("p{i}"), &format!("d{i}"), i as i64)
                .unwrap(),
            PromptAdmissionClaim::Fresh,
            "admission-independence case #{case}: distinct keys are independent"
        );
    }
    assert_eq!(case, 12, "COUNT");
}

#[test]
fn store_index_state_cas_interleavings() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let mut case = 0usize;
    assert!(store.index_state_get(ws).unwrap().is_none());
    case += 1;
    store.index_state_put(ws, "{\"s\":0}", 0, "seed").unwrap();
    assert_eq!(
        store.index_state_get(ws).unwrap().unwrap().generation,
        0,
        "index-cas case #{case}: seed row generation"
    );
    for generation in 0..20i64 {
        case += 1;
        let expected = format!("{{\"s\":{generation}}}");
        let next = format!("{{\"s\":{}}}", generation + 1);
        assert!(
            store
                .index_state_cas(ws, &expected, generation, &next, generation + 1, "build")
                .unwrap(),
            "index-cas case #{case} (generation {generation}): exact expectation must win"
        );
        case += 1;
        assert!(
            !store
                .index_state_cas(ws, &expected, generation, &next, generation + 1, "stale")
                .unwrap(),
            "index-cas case #{case} (generation {generation}): a stale expectation must lose"
        );
    }
    case += 1;
    let row = store.index_state_get(ws).unwrap().unwrap();
    assert_eq!(row.generation, 20, "index-cas case #{case}: winner landed");
    case += 1;
    let log = store.index_state_log(ws, 1_000).unwrap();
    assert_eq!(
        log.len(),
        21,
        "index-cas case #{case}: exactly one journal row per accepted write"
    );
    case += 1;
    assert!(
        store
            .index_state_put(WorkspaceId::new(999), "{}", 0, "x")
            .is_err(),
        "index-cas case #{case}: a put on an unknown workspace violates the FK loudly"
    );
    case += 1;
    assert!(!store
        .index_state_cas(WorkspaceId::new(999), "{}", 0, "{}", 1, "x")
        .unwrap());
    // Hostile CAS payloads (quotes/SQL) are data.
    case += 1;
    store
        .index_state_put(ws, "'; DROP TABLE index_state; --", 99, "q'uote")
        .unwrap();
    let row = store.index_state_get(ws).unwrap().unwrap();
    assert_eq!(
        row.generation, 99,
        "index-cas case #{case}: hostile state JSON stored verbatim"
    );
    case += 1;
    assert_eq!(
        store.index_state_log(ws, 1).unwrap()[0].kind,
        "q'uote",
        "index-cas case #{case}: hostile kind stored verbatim"
    );
    // Concurrent CAS: exactly one winner.
    case += 1;
    store.index_state_put(ws, "{\"c\":0}", 0, "seed").unwrap();
    let store = Arc::new(store);
    let mut handles = Vec::new();
    for _ in 0..8 {
        let store = Arc::clone(&store);
        handles.push(std::thread::spawn(move || {
            store
                .index_state_cas(ws, "{\"c\":0}", 0, "{\"c\":1}", 1, "race")
                .unwrap()
        }));
    }
    let winners = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|w| *w)
        .count();
    assert_eq!(
        winners, 1,
        "index-cas case #{case}: exactly one concurrent CAS winner"
    );
    assert_eq!(case, 48, "COUNT");
}

// =====================================================================
// Category 5: attachment / artifact identity
// =====================================================================

#[test]
fn store_attachment_identity_bytes_vs_metadata() {
    let (_d, store) = tmp_store();
    let (_, sid) = ws_sid(&store);
    let ws2 = store.create_workspace("/w2").unwrap();
    let sid2 = store.create_session(ws2, "t", "p", "m").unwrap().id;
    let d1 = FileHash::from([0x11; 32]);
    let d2 = FileHash::from([0x22; 32]);
    let base = AttachmentId::new(d1, "text/plain", None, 3).unwrap();
    let mut case = 0usize;

    case += 1;
    let ref0 = store.put_attachment_ref(sid, &base).unwrap();
    let ref0_again = store.put_attachment_ref(sid, &base).unwrap();
    assert_eq!(
        ref0.id, ref0_again.id,
        "attachment-identity case #{case}: an exact identity re-put must be idempotent"
    );
    case += 1;
    assert_eq!(
        store.put_attachment(sid, &base).unwrap(),
        base,
        "attachment-identity case #{case}: put_attachment returns the canonical identity"
    );

    let variants: [(&str, Option<&str>, u64); 6] = [
        ("image/png", Some("a.png"), 3),
        ("image/png", Some("b.png"), 3),
        ("text/plain", Some("a.txt"), 3),
        ("text/plain", None, 4),
        ("application/pdf", Some("c.pdf"), 3),
        ("application/octet-stream", None, 0),
    ];
    let mut ids = std::collections::HashSet::new();
    ids.insert(ref0.id);
    for (mime, filename, size) in variants {
        case += 1;
        let id = AttachmentId::new(d1, mime, filename, size).unwrap();
        let stored_ref = store.put_attachment_ref(sid, &id).unwrap_or_else(|e| {
            panic!("attachment-identity case #{case} ({mime}/{filename:?}/{size}): {e}")
        });
        assert_eq!(
            stored_ref.attachment(),
            id,
            "attachment-identity case #{case} ({mime}/{filename:?}/{size}): same bytes with different metadata are a distinct identity"
        );
        assert!(
            ids.insert(stored_ref.id),
            "attachment-identity case #{case} ({mime}/{filename:?}/{size}): each metadata variant owns a distinct row"
        );
        case += 1;
        assert_eq!(
            store.attachment_row(sid, &id).unwrap(),
            Some(id.clone()),
            "attachment-identity case #{case}: exact (digest, mime, filename, size) row lookup"
        );
    }
    case += 1;
    assert_eq!(
        store.attachments_by_digest(sid, d1).unwrap().len(),
        ids.len(),
        "attachment-identity case #{case}: one metadata variant can never replace another"
    );
    case += 1;
    assert_eq!(
        store.attachment(sid, d1).unwrap(),
        Some(base.clone()),
        "attachment-identity case #{case}: digest-only lookup returns the first row"
    );

    let other_digest_meta = AttachmentId::new(d2, "text/plain", None, 3).unwrap();
    case += 1;
    let d2_ref = store.put_attachment_ref(sid, &other_digest_meta).unwrap();
    assert_ne!(
        d2_ref.id, ref0.id,
        "attachment-identity case #{case}: different bytes with identical metadata are distinct rows"
    );
    case += 1;
    assert_eq!(
        store.attachments_by_digest(sid, d2).unwrap(),
        vec![other_digest_meta.clone()],
        "attachment-identity case #{case}: digest scoping"
    );

    case += 1;
    store.put_attachment(sid2, &base).unwrap();
    assert_eq!(
        store.attachments_by_digest(sid2, d1).unwrap().len(),
        1,
        "attachment-identity case #{case}: the same blob in another session is a separate durable row"
    );
    case += 1;
    assert_eq!(
        store.attachment_ref(sid2, ref0.id).unwrap(),
        None,
        "attachment-identity case #{case}: a ref id is session-scoped; cross-session lookup is absent, never aliased"
    );
    case += 1;
    assert_eq!(
        store.attachment_refs_for_digest(sid2, d1).unwrap().len(),
        1,
        "attachment-identity case #{case}: digest refs are scoped to the asking session"
    );

    let concurrent =
        AttachmentId::new(FileHash::from([0x33; 32]), "image/gif", Some("c.gif"), 7).unwrap();
    let store = Arc::new(store);
    let mut handles = Vec::new();
    for _ in 0..8 {
        let store = Arc::clone(&store);
        let id = concurrent.clone();
        handles.push(std::thread::spawn(move || {
            store.put_attachment_ref(sid, &id).unwrap()
        }));
    }
    let results: Vec<faktor_core::attachment::AttachmentRef> =
        handles.into_iter().map(|h| h.join().unwrap()).collect();
    case += 1;
    assert!(
        results.iter().all(|r| r.id == results[0].id),
        "attachment-identity case #{case}: concurrent identical puts converge on one row"
    );
    case += 1;
    assert_eq!(
        store
            .attachments_by_digest(sid, concurrent.digest)
            .unwrap()
            .len(),
        1,
        "attachment-identity case #{case}: concurrent puts never duplicate the identity"
    );
    assert_eq!(case, 23, "COUNT");
}

#[test]
fn store_attachment_corrupt_columns_bounds_and_receipts() {
    let (_d, store) = tmp_store();
    let (_, sid) = ws_sid(&store);
    let mut case = 0usize;

    case += 1;
    assert!(matches!(
        AttachmentId::new(FileHash::from([1u8; 32]), "text/plain", None, u64::MAX),
        Err(e) if e.kind == faktor_core::error::ErrorKind::Oversized
    ));
    case += 1;
    assert!(store.attachment_ref(sid, 0).unwrap().is_none());
    case += 1;
    assert!(store
        .attachment_ref(sid, i64::MAX as u64 + 1)
        .unwrap()
        .is_none());
    case += 1;
    assert!(store.attachment_ref(sid, u64::MAX).unwrap().is_none());
    case += 1;
    assert!(store.list_attachments(sid, 0).unwrap().is_empty());
    case += 1;
    assert!(matches!(
        store.list_attachments(sid, usize::MAX),
        Err(StoreError::Oversized(_))
    ));

    let digest = FileHash::from([4u8; 32]);
    for i in 0..(crate::MAX_ATTACHMENT_REFS_PER_BLOB + 1) {
        let id = AttachmentId::new(digest, "text/plain", Some(&format!("f{i}")), i as u64).unwrap();
        case += 1;
        store
            .put_attachment(sid, &id)
            .unwrap_or_else(|e| panic!("attachment-bounds case #{case} (ref {i}): {e}"));
    }
    case += 1;
    match store.attachment_refs_for_digest(sid, digest) {
        Err(StoreError::Oversized(m)) => assert!(
            m.contains("references"),
            "attachment-bounds case #{case}: the refusal names the ref bound: {m}"
        ),
        other => panic!("attachment-bounds case #{case}: {other:?}"),
    }
    case += 1;
    assert_eq!(
        store.attachments_by_digest(sid, digest).unwrap().len(),
        crate::MAX_ATTACHMENT_REFS_PER_BLOB + 1,
        "attachment-bounds case #{case}: the unscoped identity list still sees every row"
    );

    case += 1;
    plant(
        &store,
        "UPDATE attachment SET digest = 'zz' WHERE session_id = ?1 AND id = (SELECT MIN(id) FROM attachment WHERE session_id = ?1)",
        &[(sid.raw() as i64).into()],
    );
    assert!(
        store.list_attachments(sid, 1_000).is_err(),
        "attachment-bounds case #{case}: a non-hex digest is typed corruption, never skipped"
    );
    case += 1;
    plant(
        &store,
        "UPDATE attachment SET size = -1 WHERE session_id = ?1 AND id = (SELECT MIN(id) FROM attachment WHERE session_id = ?1)",
        &[(sid.raw() as i64).into()],
    );
    assert!(
        store.list_attachments(sid, 1_000).is_err(),
        "attachment-bounds case #{case}: a negative size is typed corruption"
    );
    case += 1;
    plant(
        &store,
        "UPDATE attachment SET filename = x'FF' WHERE session_id = ?1 AND id = (SELECT MIN(id) FROM attachment WHERE session_id = ?1)",
        &[(sid.raw() as i64).into()],
    );
    assert!(
        store.list_attachments(sid, 1_000).is_err(),
        "attachment-bounds case #{case}: an undecodable filename blob is typed corruption"
    );
    assert_eq!(case, 76, "COUNT");
}

#[test]
fn store_artifact_identity_matrix() {
    let (_d, store) = tmp_store();
    let (_, sid) = ws_sid(&store);
    let ws2 = store.create_workspace("/w2").unwrap();
    let sid2 = store.create_session(ws2, "t", "p", "m").unwrap().id;
    let mut case = 0usize;

    case += 1;
    let a1 = store
        .put_artifact(sid, "diff", "cas-abc", "first summary", 10)
        .unwrap();
    let a2 = store
        .put_artifact(sid, "note", "cas-abc", "second summary", 999)
        .unwrap();
    assert_eq!(
        a1, a2,
        "artifact-identity case #{case}: (session, cas_hash) is the identity; metadata cannot fork it"
    );
    case += 1;
    assert_eq!(
        store.artifact_for(sid, "cas-abc").unwrap(),
        Some(("first summary".to_string(), "diff".to_string())),
        "artifact-identity case #{case}: the first durable row owns the metadata"
    );
    case += 1;
    assert_eq!(store.artifact_for(sid, "cas-missing").unwrap(), None);
    case += 1;
    assert_eq!(
        store.artifact_for(sid2, "cas-abc").unwrap(),
        None,
        "artifact-identity case #{case}: another session cannot read this artifact"
    );
    case += 1;
    let b = store
        .put_artifact(sid2, "diff", "cas-abc", "other session", 10)
        .unwrap();
    assert_ne!(
        a1, b,
        "artifact-identity case #{case}: the same cas_hash in another session is a separate row"
    );

    let refs = store.cas_hash_references().unwrap();
    case += 1;
    assert_eq!(
        refs.iter()
            .filter(|r| r.source == "artifact" && r.hash == "cas-abc")
            .count(),
        2,
        "artifact-identity case #{case}: CAS references see one row per session"
    );
    case += 1;
    assert!(
        store
            .cas_hash_references()
            .unwrap()
            .iter()
            .all(|r| r.row_id > 0),
        "artifact-identity case #{case}: every CAS reference carries a positive row id"
    );

    for i in 0..8 {
        case += 1;
        let kind = format!("k{i}");
        let cas = format!("cas-{i}");
        store
            .put_artifact(sid, &kind, &cas, &"s".repeat(4096), i64::MIN)
            .unwrap();
        assert_eq!(
            store.artifact_for(sid, &cas).unwrap().unwrap().1,
            kind,
            "artifact-identity case #{case} (cas {cas}): 4 KiB metadata and i64::MIN size stay opaque data"
        );
    }
    case += 1;
    assert_eq!(
        store
            .put_artifact(sid, "dupe", "cas-abc", "ignored", 1)
            .unwrap(),
        a1,
        "artifact-identity case #{case}: repeated put returns the owning row id"
    );
    assert_eq!(case, 16, "COUNT");
}

// =====================================================================
// Category 6: billing / evidence money and identity
// =====================================================================

#[test]
fn store_cost_money_i64_boundaries_and_caps() {
    let (_d, store) = tmp_store();
    let (_, sid) = ws_sid(&store);
    let hostile_task = TaskId::new(7);
    let clean_task = TaskId::new(9);
    store
        .upsert_task(&task_row(sid, hostile_task.raw()))
        .unwrap();
    store.upsert_task(&task_row(sid, clean_task.raw())).unwrap();
    let mut case = 0usize;

    let predicted = [
        0u64,
        1,
        i64::MAX as u64 - 1,
        i64::MAX as u64,
        i64::MAX as u64 + 1,
        u64::MAX,
    ];
    for (i, amount) in predicted.iter().enumerate() {
        case += 1;
        match store
            .cost_reserve_priced(
                sid,
                hostile_task,
                OpId::new(i as u64 + 1),
                *amount,
                i as i64,
                None,
            )
            .unwrap_or_else(|e| panic!("money case #{case} (predicted {amount}): {e}"))
        {
            crate::CostReserveOutcome::Granted(id) => {
                let row = store
                    .cost_reservations_of(sid, hostile_task, 1_000)
                    .unwrap()
                    .into_iter()
                    .find(|r| r.reservation_id == id)
                    .unwrap();
                assert_eq!(
                    row.predicted_micro,
                    (*amount).min(i64::MAX as u64),
                    "money case #{case} (predicted {amount}): durable micro-units clamp into the signed domain"
                );
                assert_eq!(
                    row.status, "reserved",
                    "money case #{case}: a grant starts reserved"
                );
            }
            other => {
                panic!("money case #{case} (predicted {amount}): expected Granted, got {other:?}")
            }
        }
    }

    case += 1;
    store.cost_task_cap_set(sid, hostile_task, Some(1)).unwrap();
    assert_eq!(
        store
            .cost_task_row(sid, hostile_task)
            .unwrap()
            .unwrap()
            .max_cost_micro,
        Some(1),
        "money case #{case}: cap stored exactly"
    );
    case += 1;
    match store
        .cost_reserve_priced(sid, hostile_task, OpId::new(80), 1, 0, None)
        .unwrap_or_else(|e| {
            panic!(
                "money case #{case}: a task holding near-i64::MAX open predictions must refuse \
                 with Exceeded, never fail with a raw SQLite integer overflow: {e}"
            )
        }) {
        crate::CostReserveOutcome::Exceeded { free } => assert_eq!(
            free, 0,
            "money case #{case}: saturated free balance clamps to zero"
        ),
        other => panic!("money case #{case}: expected Exceeded, got {other:?}"),
    }
    case += 1;
    assert_eq!(
        store
            .cost_reservations_of(sid, hostile_task, 1_000)
            .unwrap()
            .len(),
        predicted.len(),
        "money case #{case}: a refused reservation writes NOTHING"
    );

    case += 1;
    store.cost_task_cap_set(sid, clean_task, Some(1)).unwrap();
    match store
        .cost_reserve_priced(sid, clean_task, OpId::new(81), 1, 0, None)
        .unwrap()
    {
        crate::CostReserveOutcome::Granted(_) => {}
        other => {
            panic!("money case #{case}: exactly-at-cap reservation must be granted, got {other:?}")
        }
    }
    case += 1;
    match store
        .cost_reserve_priced(sid, clean_task, OpId::new(82), 1, 0, None)
        .unwrap()
    {
        crate::CostReserveOutcome::Exceeded { free } => assert_eq!(
            free, 0,
            "money case #{case}: the free amount never underflows the signed domain"
        ),
        other => panic!("money case #{case}: expected Exceeded, got {other:?}"),
    }
    case += 1;
    assert_eq!(
        store
            .cost_reservations_of(sid, clean_task, 1_000)
            .unwrap()
            .len(),
        1,
        "money case #{case}: a refused reservation writes NOTHING"
    );

    case += 1;
    store.cost_task_cap_set(sid, clean_task, None).unwrap();
    assert_eq!(
        store
            .cost_task_row(sid, clean_task)
            .unwrap()
            .unwrap()
            .max_cost_micro,
        None,
        "money case #{case}: None clears the cap (unlimited)"
    );
    case += 1;
    store
        .cost_task_cap_set(sid, clean_task, Some(u64::MAX))
        .unwrap();
    assert_eq!(
        store
            .cost_task_row(sid, clean_task)
            .unwrap()
            .unwrap()
            .max_cost_micro,
        Some(i64::MAX as u64),
        "money case #{case}: u64::MAX cap clamps into the signed domain"
    );
    case += 1;
    store.cost_task_cap_set(sid, clean_task, Some(0)).unwrap();
    assert_eq!(
        store
            .cost_task_row(sid, clean_task)
            .unwrap()
            .unwrap()
            .max_cost_micro,
        Some(0),
        "money case #{case}: cap 0 is durable (treated as unlimited by the reserve path)"
    );
    case += 1;
    assert!(matches!(
        store
            .cost_reserve_priced(sid, clean_task, OpId::new(90), u64::MAX, 0, None)
            .unwrap(),
        crate::CostReserveOutcome::Granted(_)
    ));
    case += 1;
    match store.cost_task_cap_set(sid, TaskId::new(999), Some(5)) {
        Err(StoreError::Conflict(m)) => assert!(
            m.contains("no row"),
            "money case #{case}: cap set on a missing task is a typed conflict: {m}"
        ),
        other => panic!("money case #{case}: {other:?}"),
    }

    case += 1;
    plant(
        &store,
        "UPDATE task SET spent_cost_micro = -1 WHERE session_id = ?1 AND task_id = ?2",
        &[(sid.raw() as i64).into(), (clean_task.raw() as i64).into()],
    );
    assert_eq!(
        store.cost_task_row(sid, clean_task).unwrap().unwrap().spent_cost_micro,
        u64::MAX,
        "money case #{case}: a negative spent column is loud saturation, never a wrapped small number"
    );
    case += 1;
    plant(
        &store,
        "UPDATE task SET max_cost_micro = -1 WHERE session_id = ?1 AND task_id = ?2",
        &[(sid.raw() as i64).into(), (clean_task.raw() as i64).into()],
    );
    assert_eq!(
        store
            .cost_task_row(sid, clean_task)
            .unwrap()
            .unwrap()
            .max_cost_micro,
        Some(0),
        "money case #{case}: a negative cap clamps to zero"
    );
    assert_eq!(case, 19, "COUNT");
}

#[test]
fn store_cost_reservation_state_machine_edges() {
    let (_d, store) = tmp_store();
    let (_, sid) = ws_sid(&store);
    let task = TaskId::new(3);
    store.upsert_task(&task_row(sid, task.raw())).unwrap();
    let mut case = 0usize;

    case += 1;
    assert_eq!(
        store.cost_refund(999_999, 0).unwrap(),
        crate::RefundOutcome::Missing,
        "reservation case #{case}: refund of an unknown id is Missing"
    );
    case += 1;
    assert_eq!(
        store
            .cost_settle_usage(999_999, 0, 0, 0, 0, None, None, 0)
            .unwrap(),
        crate::CostSettleOutcome::Missing,
        "reservation case #{case}: settle of an unknown id is Missing"
    );

    let id = match store
        .cost_reserve_priced(sid, task, OpId::new(1), 100, 0, None)
        .unwrap()
    {
        crate::CostReserveOutcome::Granted(id) => id,
        other => panic!("reservation setup failed: {other:?}"),
    };
    case += 1;
    assert_eq!(
        store.cost_refund(id, 5).unwrap(),
        crate::RefundOutcome::Applied,
        "reservation case #{case}: a pre-dispatch reservation refunds"
    );
    case += 1;
    match store.cost_refund(id, 6).unwrap() {
        crate::RefundOutcome::Blocked {
            current,
            dispatched_ms,
        } => {
            assert_eq!(
                current, "refunded",
                "reservation case #{case}: current state named"
            );
            assert_eq!(
                dispatched_ms, None,
                "reservation case #{case}: never dispatched"
            );
        }
        other => panic!("reservation case #{case}: {other:?}"),
    }
    case += 1;
    match store
        .cost_settle_usage(id, 1, 0, 0, 1, Some(3), None, 7)
        .unwrap()
    {
        crate::CostSettleOutcome::NotOpen { current } => assert_eq!(
            current, "refunded",
            "reservation case #{case}: settling a refunded reservation is typed, not silent"
        ),
        other => panic!("reservation case #{case}: {other:?}"),
    }

    let id2 = match store
        .cost_reserve_priced(sid, task, OpId::new(2), 50, 0, None)
        .unwrap()
    {
        crate::CostReserveOutcome::Granted(id) => id,
        other => panic!("setup: {other:?}"),
    };
    case += 1;
    assert_eq!(
        store.cost_mark_dispatched(id2, 11).unwrap(),
        crate::CostReservationState::Applied,
        "reservation case #{case}: dispatch marker applies to reserved"
    );
    case += 1;
    assert_eq!(
        store.cost_mark_dispatched(id2, 12).unwrap(),
        crate::CostReservationState::Applied,
        "reservation case #{case}: a repeated dispatch marker is idempotent"
    );
    case += 1;
    match store.cost_refund(id2, 13).unwrap() {
        crate::RefundOutcome::Blocked { dispatched_ms, .. } => assert_eq!(
            dispatched_ms,
            Some(12),
            "reservation case #{case}: post-dispatch refund is blocked and names the latest marker"
        ),
        other => panic!("reservation case #{case}: {other:?}"),
    }
    case += 1;
    match store
        .cost_settle_usage(id2, 0, 0, 0, 0, Some(0), None, 14)
        .unwrap()
    {
        crate::CostSettleOutcome::Applied { actual_micro } => {
            assert!(
                actual_micro <= i64::MAX as u64,
                "reservation case #{case}: settled cost stays in the signed domain"
            );
        }
        crate::CostSettleOutcome::AppliedUnknown => {}
        other => panic!("reservation case #{case}: {other:?}"),
    }
    case += 1;
    assert!(matches!(
        store
            .cost_settle_usage(id2, 0, 0, 0, 0, None, None, 15)
            .unwrap(),
        crate::CostSettleOutcome::NotOpen { .. }
    ));

    let id3 = match store
        .cost_reserve_priced(sid, task, OpId::new(3), 10, 0, None)
        .unwrap()
    {
        crate::CostReserveOutcome::Granted(id) => id,
        other => panic!("setup: {other:?}"),
    };
    case += 1;
    assert_eq!(
        store
            .cost_mark_uncertain(id3, "wire_ambiguous", Some("req-1"), 20)
            .unwrap(),
        crate::CostReservationState::NotOpen {
            current: "reserved".into()
        },
        "reservation case #{case}: a never-dispatched reservation is not uncertain"
    );
    case += 1;
    assert_eq!(
        store.cost_mark_dispatched(id3, 20).unwrap(),
        crate::CostReservationState::Applied,
        "reservation case #{case}: dispatch before ambiguity"
    );
    case += 1;
    assert_eq!(
        store
            .cost_mark_uncertain(id3, "wire_ambiguous", Some("req-1"), 21)
            .unwrap(),
        crate::CostReservationState::Applied,
        "reservation case #{case}: dispatched -> uncertain applies"
    );
    case += 1;
    assert!(
        matches!(
            store.cost_mark_uncertain(id3, "again", None, 21).unwrap(),
            crate::CostReservationState::NotOpen { .. }
        ),
        "reservation case #{case}: a second uncertain mark is NotOpen"
    );
    case += 1;
    let report = store.cost_reconcile_uncertain(sid, task, 22).unwrap();
    assert!(
        report.settled == 0 && report.closed_unknown == 0 && report.left_uncertain == 0,
        "reservation case #{case}: without a completed provider call there is no reconcile candidate: {report:?}"
    );
    case += 1;
    let finalized = store.cost_finalize_uncertain(sid, task, 23).unwrap();
    assert!(
        finalized.settled >= 1,
        "reservation case #{case}: finalize conservatively closes the uncertain reservation at its estimate: {finalized:?}"
    );
    case += 1;
    assert_eq!(
        store.cost_mark_dispatched(id3, 24).unwrap(),
        crate::CostReservationState::NotOpen {
            current: "settled".into()
        },
        "reservation case #{case}: a finalized (settled) reservation is terminal for dispatch"
    );
    assert_eq!(case, 17, "COUNT");
}

fn evidence_fixture(
    sid: SessionId,
    ws: WorkspaceId,
    id: u64,
    kind: &str,
    revision: i64,
) -> EvidenceRow {
    EvidenceRow {
        id,
        session_id: sid,
        workspace_id: ws,
        task_id: None,
        kind: kind.into(),
        revision,
        provenance_json: "{\"p\":1}".into(),
        compressibility: "low".into(),
        compression_json: "{}".into(),
        retrieval_json: "{}".into(),
        compact_json: "{}".into(),
        backing_cas_hash: Some(format!("cas-{kind}")),
        completeness: "complete".into(),
        created_ms: revision,
    }
}

#[test]
fn store_evidence_identity_and_domain_edges() {
    let (_d, store) = tmp_store();
    let (ws, sid) = ws_sid(&store);
    let ws2 = store.create_workspace("/w2").unwrap();
    let sid2 = store.create_session(ws2, "t", "p", "m").unwrap().id;
    let mut case = 0usize;

    case += 1;
    let auto = store
        .evidence_insert(&evidence_fixture(sid, ws, 0, "auto", 1))
        .unwrap();
    assert!(
        auto >= 1,
        "evidence case #{case}: autogenerated id is positive"
    );
    case += 1;
    assert_eq!(
        store.evidence_get(auto).unwrap().unwrap().kind,
        "auto",
        "evidence case #{case}: read-back"
    );
    case += 1;
    assert!(store.evidence_get(0).unwrap().is_none());
    case += 1;
    assert!(matches!(
        store.evidence_get(u64::MAX),
        Err(StoreError::Malformed(_))
    ));
    case += 1;
    assert!(matches!(
        store.evidence_insert(&evidence_fixture(sid, ws, 0, "bad", 0)),
        Err(StoreError::Malformed(_))
    ));
    case += 1;
    assert!(matches!(
        store.evidence_insert(&evidence_fixture(sid, ws, u64::MAX, "huge", 1)),
        Err(StoreError::Malformed(_))
    ));
    case += 1;
    store
        .evidence_insert(&evidence_fixture(sid, ws, 4_000, "explicit", 1))
        .unwrap();
    match store.evidence_insert(&evidence_fixture(sid, ws, 4_000, "dupe", 2)) {
        Err(StoreError::Conflict(m)) => assert!(
            m.contains("already exists"),
            "evidence case #{case}: explicit id collision must refuse to overwrite: {m}"
        ),
        other => panic!("evidence case #{case}: {other:?}"),
    }

    let mut previous_high_water = store.evidence_high_water().unwrap();
    for i in 0..12u64 {
        case += 1;
        store
            .evidence_insert(&evidence_fixture(
                sid,
                ws,
                0,
                &format!("k{i}"),
                (i + 1) as i64,
            ))
            .unwrap();
        let high_water = store.evidence_high_water().unwrap();
        assert!(
            high_water > previous_high_water,
            "evidence case #{case}: high water advances monotonically ({previous_high_water} -> {high_water})"
        );
        previous_high_water = high_water;
    }
    case += 1;
    assert_eq!(
        store.evidence_high_water().unwrap(),
        4_012,
        "evidence case #{case}: high water equals MAX(id) after 12 rows follow the explicit id 4000"
    );

    case += 1;
    assert!(
        store
            .evidence_list_by_scope(sid2, ws2, 100)
            .unwrap()
            .is_empty(),
        "evidence case #{case}: a session/workspace scope cannot read foreign rows"
    );
    case += 1;
    assert_eq!(
        store.evidence_list_by_scope(sid, ws, 100).unwrap().len(),
        14,
        "evidence case #{case}: scope listing sees exactly its own rows"
    );
    case += 1;
    assert!(store.evidence_list_by_scope(sid, ws, 0).unwrap().is_empty());

    let newest = store
        .evidence_list_by_scope_newest(sid, ws, None, 100)
        .unwrap();
    case += 1;
    assert!(
        newest.windows(2).all(|w| w[0].id > w[1].id),
        "evidence case #{case}: newest-first ordering is strictly descending by id"
    );
    case += 1;
    let before_first = store
        .evidence_list_by_scope_newest(sid, ws, Some(newest[0].id), 100)
        .unwrap();
    assert!(
        before_first.first().map(|r| r.id) == newest.get(1).map(|r| r.id),
        "evidence case #{case}: before cursor is exclusive"
    );
    case += 1;
    assert!(
        store
            .evidence_list_by_scope_newest(sid, ws, Some(1), 100)
            .unwrap()
            .is_empty(),
        "evidence case #{case}: before=1 can never yield a row (ids start at 1)"
    );
    case += 1;
    assert!(matches!(
        store.evidence_list_by_scope_newest(sid, ws, Some(u64::MAX), 100),
        Err(StoreError::Malformed(_))
    ));

    let backing = store.evidence_ids_by_backing("cas-auto", 100).unwrap();
    case += 1;
    assert_eq!(
        backing.len(),
        1,
        "evidence case #{case}: backing index finds the row"
    );
    case += 1;
    assert!(store
        .evidence_ids_by_backing("no-such", 100)
        .unwrap()
        .is_empty());
    case += 1;
    assert_eq!(
        store.evidence_ids_by_backing("cas-auto", 100).unwrap(),
        vec![auto],
        "evidence case #{case}: backing index maps the hash to its evidence ids"
    );
    assert_eq!(case, 30, "COUNT");
}

// =====================================================================
// Category 7: signed-domain casts across every paged read
// =====================================================================

#[test]
fn store_paged_reads_signed_domain_boundaries() {
    let (_d, store) = tmp_store();
    let (ws, sid) = ws_sid(&store);
    let task = TaskId::new(5);
    store.upsert_task(&task_row(sid, task.raw())).unwrap();
    for i in 1..=6i64 {
        store
            .put_message(sid, i, "user", serde_json::json!({"i": i}))
            .unwrap();
        store
            .append_event(
                sid,
                None,
                EventKind::PhaseChanged,
                AgentState::Preparing,
                i,
                None,
            )
            .unwrap();
        store
            .append_ledger_entry(sid, "e", 1, serde_json::json!({"i": i}))
            .unwrap();
        store
            .upsert_memory_fact(sid, "k", &format!("key{i}"), "v")
            .unwrap();
        store
            .evidence_insert(&evidence_fixture(sid, ws, 0, &format!("ev{i}"), i))
            .unwrap();
    }
    store.index_state_put(ws, "{}", 0, "seed").unwrap();
    store
        .cost_reserve_priced(sid, task, OpId::new(1), 5, 0, None)
        .unwrap();
    store
        .record_provider_call_with_prefix(
            sid,
            OpId::new(2),
            "p",
            "m",
            "ok",
            Some(1),
            Some(1),
            None,
            Some([1u8; 32]),
            Some(1),
            None,
        )
        .unwrap();

    let limits_u64: [u64; 6] = [
        0,
        1,
        i64::MAX as u64 - 1,
        i64::MAX as u64,
        i64::MAX as u64 + 1,
        u64::MAX,
    ];
    let mut case = 0usize;

    for limit in limits_u64 {
        case += 1;
        let outcome = store.messages_before_page(sid, None, Some(limit));
        if limit > i64::MAX as u64 {
            assert!(
                matches!(outcome, Err(StoreError::Oversized(_))),
                "signed-cast case #{case} (messages_before_page limit {limit}): above the signed domain must refuse typed, never bind an unbounded LIMIT"
            );
            continue;
        }
        let rows = outcome.unwrap_or_else(|e| {
            panic!("signed-cast case #{case} (messages_before_page limit {limit}): {e}")
        });
        assert!(
            rows.len() <= 6,
            "signed-cast case #{case} (messages_before_page limit {limit}): page grows with the limit but never past the data"
        );
        if limit == 0 {
            assert!(
                rows.is_empty(),
                "signed-cast case #{case} (limit 0): an explicit zero page is empty"
            );
        }
    }
    for limit in limits_u64 {
        case += 1;
        let rows = store
            .messages_backwards_bounded(sid, None, limit, u64::MAX)
            .unwrap_or_else(|e| {
                panic!("signed-cast case #{case} (messages_backwards_bounded max {limit}): {e}")
            });
        assert!(
            rows.len() <= 6,
            "signed-cast case #{case} (messages_backwards_bounded max {limit}): bounded page"
        );
        if limit == 0 {
            assert!(
                rows.is_empty(),
                "signed-cast case #{case}: zero messages is empty"
            );
        }
    }
    for before in limits_u64 {
        case += 1;
        let rows = store
            .messages_backwards_bounded(sid, Some(before), u64::MAX, u64::MAX)
            .unwrap_or_else(|e| panic!("signed-cast case #{case} (before {before}): {e}"));
        assert!(
            rows.iter().all(|r| (r.seq as u64) < before),
            "signed-cast case #{case} (before {before}): every row is strictly older than the cursor"
        );
    }
    for limit in limits_u64 {
        case += 1;
        let outcome = store.events_range(sid, 1, Some(limit));
        if limit > i64::MAX as u64 {
            assert!(
                matches!(outcome, Err(StoreError::Oversized(_))),
                "signed-cast case #{case} (events_range limit {limit}): above the signed domain refuses typed"
            );
            continue;
        }
        let rows = outcome.unwrap_or_else(|e| {
            panic!("signed-cast case #{case} (events_range limit {limit}): {e}")
        });
        assert!(
            rows.len() <= 7,
            "signed-cast case #{case}: event page bounded by the journal"
        );
    }
    for from in limits_u64 {
        case += 1;
        let rows = store
            .events_range(sid, from, None)
            .unwrap_or_else(|e| panic!("signed-cast case #{case} (events_range from {from}): {e}"));
        assert!(
            rows.iter().all(|r| r.seq.raw() >= from),
            "signed-cast case #{case} (from {from}): rows respect the lower bound"
        );
        if from > 7 {
            assert!(
                rows.is_empty(),
                "signed-cast case #{case} (from {from}): above MAX(seq) is empty"
            );
        }
    }
    let after_values: [i64; 6] = [i64::MIN, -1, 0, 1, i64::MAX - 1, i64::MAX];
    for after in after_values {
        case += 1;
        let rows = store
            .ledger_entries_page(sid, Some(after), None)
            .unwrap_or_else(|e| panic!("signed-cast case #{case} (ledger after {after}): {e}"));
        assert!(
            rows.iter().all(|r| r.seq > after),
            "signed-cast case #{case} (ledger after {after}): strictly after the cursor"
        );
    }
    for limit in limits_u64 {
        case += 1;
        let outcome = store.ledger_entries_page(sid, None, Some(limit));
        if limit > i64::MAX as u64 {
            assert!(
                matches!(outcome, Err(StoreError::Oversized(_))),
                "signed-cast case #{case} (ledger limit {limit}): above the signed domain refuses typed"
            );
            continue;
        }
        let rows = outcome
            .unwrap_or_else(|e| panic!("signed-cast case #{case} (ledger limit {limit}): {e}"));
        assert!(
            rows.len() <= 6,
            "signed-cast case #{case} (ledger limit {limit}): bounded"
        );
        if limit == 0 {
            assert!(
                rows.is_empty(),
                "signed-cast case #{case}: zero ledger page"
            );
        }
    }
    for before in after_values {
        case += 1;
        let rows = store
            .ledger_entries_desc_page(sid, Some(before), None)
            .unwrap_or_else(|e| {
                panic!("signed-cast case #{case} (ledger desc before {before}): {e}")
            });
        assert!(
            rows.iter().all(|r| r.seq < before),
            "signed-cast case #{case} (ledger desc before {before}): strictly before the cursor"
        );
    }
    for limit in limits_u64 {
        case += 1;
        let (rows, _more) = store
            .memory_facts_page(sid, None, limit)
            .unwrap_or_else(|e| panic!("signed-cast case #{case} (facts limit {limit}): {e}"));
        assert!(
            rows.len() <= 6,
            "signed-cast case #{case} (facts limit {limit}): bounded"
        );
        if limit == 0 {
            assert!(rows.is_empty(), "signed-cast case #{case}: zero facts page");
        }
    }
    for limit in [0usize, 1, 10_000, usize::MAX] {
        case += 1;
        let rows = store
            .evidence_list_by_scope(sid, ws, limit)
            .unwrap_or_else(|e| panic!("signed-cast case #{case} (evidence limit {limit}): {e}"));
        assert!(
            rows.len() <= 6,
            "signed-cast case #{case} (evidence limit {limit}): bounded"
        );
        if limit == 0 {
            assert!(
                rows.is_empty(),
                "signed-cast case #{case}: zero evidence page"
            );
        }
    }
    for before in limits_u64 {
        case += 1;
        let outcome = store.evidence_list_by_scope_newest(sid, ws, Some(before), 100);
        if before > i64::MAX as u64 {
            assert!(
                matches!(outcome, Err(StoreError::Malformed(_))),
                "signed-cast case #{case} (evidence before {before}): a cursor above the signed domain names no row and must refuse typed"
            );
            continue;
        }
        let rows = outcome
            .unwrap_or_else(|e| panic!("signed-cast case #{case} (evidence before {before}): {e}"));
        assert!(
            rows.iter().all(|r| r.id < before),
            "signed-cast case #{case} (evidence before {before}): strictly before the cursor"
        );
    }
    let reservation_limits: [i64; 6] = [i64::MIN, -1, 0, 1, i64::MAX - 1, i64::MAX];
    for limit in reservation_limits {
        case += 1;
        let rows = store
            .cost_reservations_of(sid, task, limit)
            .unwrap_or_else(|e| {
                panic!("signed-cast case #{case} (reservations limit {limit}): {e}")
            });
        assert!(
            rows.len() <= 1,
            "signed-cast case #{case} (reservations limit {limit}): a negative/zero limit is clamped, never unbounded"
        );
        if limit <= 0 {
            assert!(
                rows.is_empty(),
                "signed-cast case #{case} (reservations limit {limit}): clamped to empty"
            );
        }
    }
    let log_limits: [i64; 5] = [i64::MIN, -1, 0, 1, i64::MAX];
    for limit in log_limits {
        case += 1;
        let rows = store
            .index_state_log(ws, limit)
            .unwrap_or_else(|e| panic!("signed-cast case #{case} (index log limit {limit}): {e}"));
        assert!(
            rows.len() <= 1,
            "signed-cast case #{case} (index log limit {limit}): bounded"
        );
        if limit <= 0 {
            assert!(
                rows.is_empty(),
                "signed-cast case #{case} (index log limit {limit}): empty"
            );
        }
    }
    for limit in reservation_limits {
        case += 1;
        let (rows, _more) = store
            .provider_call_task_rows(sid, task, limit)
            .unwrap_or_else(|e| {
                panic!("signed-cast case #{case} (provider calls limit {limit}): {e}")
            });
        assert!(
            rows.len() <= 1,
            "signed-cast case #{case} (provider calls limit {limit}): clamped to the signed domain"
        );
        if limit <= 0 {
            assert!(
                rows.is_empty(),
                "signed-cast case #{case} (provider calls limit {limit}): a non-positive limit yields no rows"
            );
        }
    }
    assert_eq!(case, 81, "signed-cast case count drifted");
}
