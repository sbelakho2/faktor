// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! HAR-05 op registry: the shared fixture plus one driver for EVERY store
//! op that carries a writer-queue or read boundary (see
//! `store_fault_matrix_tests` for the extraction/drift check and the fault
//! matrix that drives them).
//!
//! Drivers are deliberately written against seeded, deterministic fixture
//! ids so that a driver REACHES the boundary it is supposed to exercise:
//! the matrix asserts the seam fired for every write op, which turns a
//! missing fixture row into a test failure instead of a vacuous pass.

use super::*;
use faktor_core::hash::FileHash;
use faktor_core::op::ModelCallAttempt;

/// Every driver returns `StoreResult<()>`: the matrix only cares that the
/// op ran (or failed) at its boundary under a fault.
pub(crate) type Driver = fn(&Fixture) -> StoreResult<()>;

pub(crate) struct OpSpec {
    pub(crate) name: &'static str,
    pub(crate) drive: Driver,
}

pub(crate) fn ok<T>(r: StoreResult<T>) -> StoreResult<()> {
    r.map(|_| ())
}

/// Marker payload embedded in a late message row: the corrupt-page test
/// byte-flips the page that carries it.
pub(crate) const CORRUPT_MARKER: &str = "HAR05-CORRUPT-MARKER-9f3a";
pub(crate) const WS_PATH: &str = "/ws/har05";

/// Deterministic fixture rows (ids 1..=N): every phase of every fault class
/// seeds the SAME rows, so reopens of a copy see the same identities.
pub(crate) struct Fixture {
    pub(crate) _dir: Option<Arc<tempfile::TempDir>>,
    pub(crate) root: std::path::PathBuf,
    pub(crate) store: Store,
    pub(crate) ws: WorkspaceId,
    pub(crate) sid: SessionId,
    pub(crate) tid: TaskId,
    pub(crate) mid: i64,
    pub(crate) pid: i64,
    pub(crate) queue_seq: i64,
    pub(crate) wid: i64,
    pub(crate) attempt_op: u64,
    pub(crate) rec_id: VerificationRecordId,
}

fn seed_verifying_task(store: &Store, sid: SessionId, tid: TaskId) -> TaskRevision {
    let walk = |state: TaskState| TaskRow {
        task_id: tid,
        session_id: sid,
        goal: "har05".into(),
        acceptance_criteria: vec!["works".into()],
        plan: vec![],
        attachments: vec![],
        contract: Default::default(),
        max_tokens: None,
        max_turns: None,
        spent_tokens: 0,
        spent_turns: 0,
        state,
        revision: TaskRevision::new(1),
        created_ms: 1,
        updated_ms: 1,
    };
    store.upsert_task(&walk(TaskState::Pending)).unwrap();
    let mut rev = TaskRevision::new(1);
    for state in [
        TaskState::Running,
        TaskState::NeedsVerification,
        TaskState::Verifying,
    ] {
        let mut row = walk(state);
        rev = rev.checked_next().expect("task revision");
        row.revision = rev;
        store.upsert_task(&row).unwrap();
    }
    rev
}

impl Fixture {
    /// Fresh tempdir store with the deterministic baseline rows.
    pub(crate) fn seed() -> Fixture {
        let dir = tempfile::tempdir().expect("fixture tempdir");
        let root = dir.path().to_path_buf();
        let mut fx = Self::seed_in(&root);
        fx._dir = Some(Arc::new(dir));
        fx
    }

    /// Seed the deterministic baseline rows into an existing data dir (the
    /// corrupt-page test seeds first, checkpoints, closes, then copies).
    pub(crate) fn seed_in(root: &std::path::Path) -> Fixture {
        let store = Store::open(root, false).expect("open fixture store");
        let mut fx = Fixture {
            _dir: None,
            root: root.to_path_buf(),
            store,
            ws: WorkspaceId::new(1),
            sid: SessionId::new(1),
            tid: TaskId::new(1),
            mid: 1,
            pid: 1,
            queue_seq: 1,
            wid: 1,
            attempt_op: 42,
            rec_id: VerificationRecordId::new(1),
        };
        fx.populate();
        fx
    }

    /// Grow the WAL past the 64 MiB truncate threshold so
    /// `wal_checkpoint_truncate_bounded` reaches its writer boundary (the
    /// size guard is a production pre-check that would otherwise return
    /// before the queue — a vacuous pass). Called ONLY by the disk-full
    /// test: the suite keeps its disk footprint small (a storage-governor
    /// suite-mate asserts a free-space reserve that large transient files
    /// could otherwise tip over).
    pub(crate) fn pad_wal(&self) {
        let pad = "p".repeat(1024 * 1024);
        let wal_path = self.store.path().with_extension("db-wal");
        let mut pad_seq = 10_000u64;
        loop {
            let wal_size = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
            if wal_size > WAL_TRUNCATE_THRESHOLD_BYTES || pad_seq > 10_100 {
                break;
            }
            pad_seq += 1;
            self.store
                .put_message(
                    self.sid,
                    pad_seq as i64,
                    "assistant",
                    serde_json::json!({"pad": pad}),
                )
                .expect("wal padding message");
        }
    }

    /// Reopen an existing data dir (corrupt / pristine copies) with the same
    /// deterministic identities — no reseeding, the rows came with the copy.
    pub(crate) fn reopen(root: &std::path::Path) -> Fixture {
        let store = Store::open(root, false).expect("reopen fixture store");
        Fixture {
            _dir: None,
            root: root.to_path_buf(),
            store,
            ws: WorkspaceId::new(1),
            sid: SessionId::new(1),
            tid: TaskId::new(1),
            mid: 1,
            pid: 1,
            queue_seq: 1,
            wid: 1,
            attempt_op: 42,
            rec_id: VerificationRecordId::new(1),
        }
    }

    fn populate(&mut self) {
        let store = &self.store;
        let ws = store.create_workspace(WS_PATH).expect("seed workspace");
        debug_assert_eq!(ws.raw(), 1);
        let _wid = store
            .put_worktree(ws, "/wt/har05", "main")
            .expect("seed worktree");
        let s = store
            .create_session(ws, "har05-session", "prov", "model")
            .expect("seed session");
        debug_assert_eq!(s.id.raw(), 1);
        let rev = seed_verifying_task(store, s.id, TaskId::new(1));
        let mid = store
            .put_message(
                s.id,
                1,
                "user",
                serde_json::json!({"marker": "HAR05-MARKER"}),
            )
            .expect("seed message");
        debug_assert_eq!(mid, 1);
        store
            .put_part(mid, "text", serde_json::json!({"text": "har05"}))
            .expect("seed part");
        let seq = store
            .append_ledger_entry(
                s.id,
                "goal_set",
                1,
                serde_json::json!({"goal": "har05 baseline"}),
            )
            .expect("seed ledger entry");
        store
            .put_ledger_head(s.id, serde_json::json!({"seq": seq}), seq, 1)
            .expect("seed ledger head");
        store
            .upsert_memory_fact(s.id, "memory_v2:har05", "k", "v")
            .expect("seed memory fact");
        store
            .upsert_memory_fact(s.id, "har05:legacy", "k2", "v2")
            .expect("seed legacy memory fact");
        // Two doomed facts: the delete* drivers consume these so the
        // committed baseline (>= 2 facts) survives a successful busy-phase
        // deletion.
        store
            .upsert_memory_fact(s.id, "har05:doomed1", "k", "d1")
            .expect("seed doomed fact 1");
        store
            .upsert_memory_fact(s.id, "har05:doomed2", "k", "d2")
            .expect("seed doomed fact 2");
        // Pre-v22 legacy verification facts: the one-shot importer has real
        // source rows to project (its marker is absent), so the op crosses
        // the write boundary with work to do.
        let legacy_attempt = serde_json::json!({
            "schema_ver": 2,
            "task_id": 1u64,
            "op_id": 77u64,
            "task_revision": rev.raw(),
            "workspace_root": "/wt/har05",
            "changed": ["src/legacy.rs"],
            "checks": [{"check_id": "legacy-build", "command": "cargo build", "inline": null}],
            "environment_fingerprint": null,
            "created_ms": 1i64,
        })
        .to_string();
        store
            .upsert_memory_fact(s.id, "verification_attempt", "va:1:77", &legacy_attempt)
            .expect("seed legacy attempt fact");
        let legacy_job = serde_json::json!({
            "schema_ver": 2,
            "attempt_op": 77u64,
            "task_id": 1u64,
            "task_revision": rev.raw(),
            "workspace_root": "/wt/har05",
            "check_id": "legacy-build",
            "kind": "test",
            "command": "cargo test",
            "spec_json": "{}",
            "budget_ms": 60_000u64,
            "state": "queued",
            "note": null,
            "op_id": null,
            "result_json": null,
            "environment_fingerprint": null,
            "created_ms": 1i64,
            "updated_ms": 1i64,
            "finished_ms": null,
        })
        .to_string();
        store
            .upsert_memory_fact(s.id, "verification_job", "vj:1:legacy-build", &legacy_job)
            .expect("seed legacy job fact");
        // A fifth fact so the retention sweep can delete one row and still
        // leave the committed baseline (>= 2 facts) intact.
        store
            .upsert_memory_fact(s.id, "har05:extra", "k", "x")
            .expect("seed extra fact");
        store
            .upsert_workspace_memory_fact(ws.raw(), "memory_v2:har05", "wk", "wv")
            .expect("seed workspace memory fact");
        let (pid, _ev) = store
            .insert_permission(s.id, OpId::new(1001), "fs.read")
            .expect("seed permission");
        self.pid = pid;
        let q = store
            .enqueue_prompt(
                s.id,
                OpId::new(1002),
                "har05 prompt",
                &[],
                None,
                None,
                None,
                1_700_000_000_000,
            )
            .expect("seed queue row");
        self.queue_seq = q;
        store
            .insert_checkpoint(
                s.id,
                "src/a.rs",
                true,
                "beforehash000000",
                true,
                "afterhash000000",
                Some("cas00000000000000"),
            )
            .expect("seed checkpoint");
        store
            .put_message(
                s.id,
                9001,
                "user",
                serde_json::json!({"marker": CORRUPT_MARKER}),
            )
            .expect("seed corrupt marker message");
        // Verification attempt + one queued background job.
        let attempt = VerificationAttemptRow {
            session_id: s.id,
            task_id: TaskId::new(1),
            attempt_op_id: self.attempt_op,
            task_revision: rev,
            workspace_root: "/wt/har05".into(),
            environment_fingerprint_json: None,
            created_ms: 1,
        };
        let job = VerificationJobRow {
            session_id: s.id,
            task_id: TaskId::new(1),
            attempt_op_id: self.attempt_op,
            check_id: "build".into(),
            ordinal: 0,
            task_revision: rev,
            workspace_root: "/wt/har05".into(),
            kind: "test".into(),
            command: "cargo test".into(),
            program: "cargo".into(),
            args_json: "[]".into(),
            spec_json: Some("{\"timeout_ms\":60000}".into()),
            budget_ms: 60_000,
            inline_status: None,
            state: "queued".into(),
            result_json: None,
            note: None,
            op_id: None,
            environment_fingerprint_json: None,
            created_ms: 1,
            updated_ms: 1,
            finished_ms: None,
        };
        store
            .verification_attempt_begin(&attempt, &["src/lib.rs".to_string()], &[job])
            .expect("seed verification attempt");
        let rec = VerificationRecordRow {
            id: VerificationRecordId::new(1),
            session_id: Some(s.id),
            task_id: TaskId::new(1),
            revision: rev,
            workspace_id: ws,
            worktree_id: WorktreeId::new(1),
            tree_hash: None,
            criteria: vec![],
            checks: vec![],
            changed_files: vec![],
            unrelated_changes: vec![],
            reviewer: None,
            status: VerificationStatus::Running,
            started_ms: 1,
            completed_ms: None,
        };
        self.rec_id = store
            .verification_record_put(&rec)
            .expect("seed verification record");
        store
            .evidence_insert(&EvidenceRow {
                id: 0,
                session_id: s.id,
                workspace_id: ws,
                task_id: Some(1),
                kind: "process_log".into(),
                revision: 1,
                provenance_json: r#"{"entries":["tool"]}"#.into(),
                compressibility: "aggressive".into(),
                compression_json: r#"{"algorithm":"identity"}"#.into(),
                retrieval_json: r#"{"allow_ranges":true,"allow_search":true,"max_bytes":64}"#
                    .into(),
                compact_json: r#"{"text":"har05"}"#.into(),
                backing_cas_hash: Some("cd".repeat(32)),
                completeness: "complete".into(),
                created_ms: 1,
            })
            .expect("seed evidence");
        // Two running tool runs (finish / recovery drivers target them).
        store
            .start_tool_run(
                s.id,
                OpId::new(4000),
                "shell",
                serde_json::json!({"cmd": "ls"}),
                serde_json::json!({"strategy": "verify_hash"}),
                None,
                None,
            )
            .expect("seed tool run 4000");
        store
            .start_tool_run(
                s.id,
                OpId::new(4002),
                "shell",
                serde_json::json!({"cmd": "pwd"}),
                serde_json::json!({"strategy": "verify_hash"}),
                None,
                None,
            )
            .expect("seed tool run 4002");
        // Three active turns (complete/cancel/fail drivers target them).
        for op in [4100u64, 4101, 4102] {
            store
                .start_turn_record(s.id, OpId::new(op), Some(1), Some(1), "prov", "model", None)
                .expect("seed turn record");
        }
        // Cost reservations in the states the settle/refund/dispatch drivers
        // expect (ids 1..=5; the reservation op creates its own rows).
        let raw = store.raw_conn();
        for (rid, status) in [
            (1i64, "reserved"),
            (2, "dispatched"),
            (3, "dispatched"),
            (4, "reserved"),
            (5, "reserved"),
            (6, "uncertain"),
        ] {
            raw.execute(
                "INSERT INTO cost_reservation (reservation_id, session_id, task_id, op_id,
                     predicted_micro, status, created_ms, dispatched_ms)
                 VALUES (?1, ?2, 1, ?3, 1000, ?4, 1, ?5)",
                rusqlite::params![
                    rid,
                    s.id.raw() as i64,
                    5000 + rid,
                    status,
                    if status == "dispatched" {
                        rusqlite::types::Value::Integer(1)
                    } else {
                        rusqlite::types::Value::Null
                    }
                ],
            )
            .expect("seed cost reservation");
        }
        drop(raw);
        // A provider call so the uncertain-reservation reconcile sweeps find
        // a candidate (its EXISTS join spans provider_call attempts).
        store
            .record_provider_call_attempt(
                s.id,
                &ModelCallAttempt::new(OpId::new(6100), OpId::new(6101), 0)
                    .expect("seed attempt ids"),
                Some(6),
                "prov",
                "model",
                "completed",
                Some(1),
                Some(1),
                None,
            )
            .expect("seed provider call");
        // A dedicated message the delete_message driver consumes (baseline
        // keeps >= 2 messages after it lands).
        store
            .put_message(s.id, 300, "assistant", serde_json::json!({"doomed": true}))
            .expect("seed doomed message");
        // Align the uncertain reservation with the provider attempt so the
        // reconcile sweep's EXISTS join finds its candidate.
        let raw = store.raw_conn();
        raw.execute(
            "UPDATE cost_reservation
             SET op_id = (SELECT p.op_id FROM provider_call p
                          WHERE p.session_id = ?1 ORDER BY p.id LIMIT 1),
                 attempt_op_id = (SELECT p.attempt_op_id FROM provider_call p
                                  WHERE p.session_id = ?1 ORDER BY p.id LIMIT 1)
             WHERE reservation_id IN (2, 6)",
            rusqlite::params![s.id.raw() as i64],
        )
        .expect("align uncertain reservation with provider call");
        drop(raw);
    }
}

fn cmd_event(kind: EventKind) -> CommandEvent {
    CommandEvent {
        kind,
        state: AgentState::Idle,
        op_id: None,
        ts_ms: 1,
        payload: None,
        payload_ver: 1,
    }
}

fn hot_event(kind: EventKind) -> HotWrite {
    HotWrite::AppendEvent {
        session_id: SessionId::new(1),
        op_id: None,
        kind,
        state: AgentState::Idle,
        ts_ms: 1,
        payload: None,
        payload_ver: 1,
    }
}

fn task_row(f: &Fixture) -> TaskRow {
    f.store
        .get_task(f.sid, f.tid)
        .expect("fixture task row")
        .expect("fixture task row exists")
}

fn outcome_row(run_id: &str) -> EngineeringOutcomeRow {
    EngineeringOutcomeRow {
        run_id: run_id.into(),
        session_id: 1,
        task_id: 1,
        payload: "{}".into(),
        payload_bytes: 2,
        completed_at_ms: 1_700_000_000_000,
        verified: true,
        verified_first_pass: true,
        integrated: true,
        local_used: true,
        frontier_used: false,
        wall_time_ms: 60_000,
        human_interventions: 0,
        rollback_events: 0,
        recovery_events: 0,
        repair_attempts: 0,
        risk_class: "low".into(),
        changed_files: 1,
        changed_lines: 2,
        checks_passed: 1,
        checks_required: 1,
    }
}

fn settlement_row(run_id: &str) -> EconomicSettlementRow {
    EconomicSettlementRow {
        run_id: run_id.into(),
        seq: 0,
        schema_version: 1,
        state: "complete".into(),
        reason: "verified".into(),
        model_spend_micro: Some(0),
        verification_spend_micro: Some(0),
        rework_spend_micro: Some(0),
        avoided_hosted_micro: Some(0),
        avoided_hosted_measured: true,
        steps_json: "[]".into(),
        settled_at_ms: 1_700_000_000_000,
    }
}

/// The op registry: one driver per boundary-carrying `impl Store` method.
/// `store_fault_matrix_tests::store_op_registry_covers_every_boundary_method`
/// fails when this table and the source scanner disagree in either
/// direction.
pub(crate) const OPS: &[OpSpec] = &[
    // ----------------------------------------------------------- sessions
    OpSpec {
        name: "adopt_session_identity",
        drive: |f| {
            ok(f.store
                .adopt_session_identity(f.sid, WorktreeId::new(1), f.tid))
        },
    },
    OpSpec {
        name: "create_session",
        drive: |f| ok(f.store.create_session(f.ws, "har05-extra", "prov", "model")),
    },
    OpSpec {
        name: "create_workspace",
        drive: |f| ok(f.store.create_workspace("/ws/har05-extra")),
    },
    OpSpec {
        name: "insert_checkpoint",
        drive: |f| {
            ok(f.store.insert_checkpoint(
                f.sid,
                "src/b.rs",
                true,
                "beforehash11111",
                true,
                "afterhash11111",
                Some("cas111111111111111"),
            ))
        },
    },
    OpSpec {
        name: "insert_checkpoint_and_event",
        drive: |f| {
            ok(f.store.insert_checkpoint_and_event(
                f.sid,
                "src/c.rs",
                true,
                "beforehash22222",
                true,
                "afterhash22222",
                None,
                AgentState::Idle,
            ))
        },
    },
    OpSpec {
        name: "insert_permission",
        drive: |f| {
            ok(f.store
                .insert_permission(f.sid, OpId::new(1101), "fs.write"))
        },
    },
    OpSpec {
        name: "insert_permission_and_event",
        drive: |f| {
            ok(f.store.insert_permission_and_event(
                f.sid,
                OpId::new(1102),
                "execute_shell",
                AgentState::Idle,
                cmd_event(EventKind::PermissionDenied),
            ))
        },
    },
    OpSpec {
        name: "put_checkpoint_and_event",
        drive: |f| {
            ok(f.store.put_checkpoint_and_event(
                f.sid,
                1,
                "src/d.rs",
                "beforehash33333",
                "afterhash33333",
                None,
                AgentState::Idle,
                cmd_event(EventKind::CheckpointCreated),
            ))
        },
    },
    OpSpec {
        name: "put_worktree",
        drive: |f| ok(f.store.put_worktree(f.ws, "/wt/har05-extra", "feature")),
    },
    OpSpec {
        name: "record_compaction_and_event",
        drive: |f| {
            ok(f.store.record_compaction_and_event(
                f.sid,
                1000,
                400,
                800,
                true,
                "auto",
                AgentState::Idle,
                cmd_event(EventKind::ContextCompacted),
            ))
        },
    },
    OpSpec {
        name: "record_compaction_with_checkpoint_and_event",
        drive: |f| {
            ok(f.store.record_compaction_with_checkpoint_and_event(
                f.sid,
                1100,
                420,
                840,
                true,
                "auto",
                AgentState::Idle,
                cmd_event(EventKind::ContextCompacted),
                "auto",
                &[("src/e.rs".into(), "h444444444444444".into())],
            ))
        },
    },
    OpSpec {
        name: "remove_worktree",
        drive: |f| ok(f.store.remove_worktree("/wt/har05")),
    },
    OpSpec {
        name: "resolve_permission_and_event",
        drive: |f| {
            ok(f.store.resolve_permission_and_event(
                f.pid,
                f.sid,
                "expired",
                AgentState::Idle,
                cmd_event(EventKind::PermissionExpired),
            ))
        },
    },
    OpSpec {
        name: "set_lifecycle_if",
        drive: |f| {
            ok(f.store
                .set_lifecycle_if(f.sid, SessionLifecycle::Open, SessionLifecycle::Suspended))
        },
    },
    OpSpec {
        name: "set_session_state",
        drive: |f| ok(f.store.set_session_state(f.sid, AgentState::Preparing)),
    },
    OpSpec {
        name: "update_session_title",
        drive: |f| ok(f.store.update_session_title(f.sid, "har05 renamed")),
    },
    // -------------------------------------------------------- transitions
    OpSpec {
        name: "transition_session",
        drive: |f| {
            ok(f.store.transition_session(
                f.sid,
                None,
                SessionTransition {
                    expected_lifecycle: None,
                    new_lifecycle: None,
                    expected_state: None,
                    new_state: AgentState::Preparing,
                    event_kind: EventKind::PhaseChanged,
                    event_payload: None,
                    event_payload_ver: 1,
                },
            ))
        },
    },
    // ------------------------------------------------------------- ledger
    OpSpec {
        name: "append_ledger_entry",
        drive: |f| {
            ok(f.store.append_ledger_entry(
                f.sid,
                "blocker_opened",
                1,
                serde_json::json!({"kind": "har05"}),
            ))
        },
    },
    OpSpec {
        name: "compact_ledger",
        drive: |f| {
            ok(f.store
                .compact_ledger(f.sid, 0, &[0], serde_json::json!({"seq": 1}), 1, 1))
        },
    },
    OpSpec {
        name: "evidence_insert",
        drive: |f| {
            ok(f.store.evidence_insert(&EvidenceRow {
                id: 0,
                session_id: f.sid,
                workspace_id: f.ws,
                task_id: None,
                kind: "log".into(),
                revision: 1,
                provenance_json: r#"{"entries":[]}"#.into(),
                compressibility: "none".into(),
                compression_json: r#"{"algorithm":"identity"}"#.into(),
                retrieval_json: r#"{"allow_ranges":false,"allow_search":true,"max_bytes":64}"#
                    .into(),
                compact_json: r#"{"text":"x"}"#.into(),
                backing_cas_hash: None,
                completeness: "complete".into(),
                created_ms: 2,
            }))
        },
    },
    OpSpec {
        name: "put_ledger_head",
        drive: |f| {
            ok(f.store
                .put_ledger_head(f.sid, serde_json::json!({"seq": 1}), 1, 1))
        },
    },
    OpSpec {
        name: "upsert_memory_fact",
        drive: |f| {
            ok(f.store
                .upsert_memory_fact(f.sid, "har05:runtime", "rk", "rv"))
        },
    },
    OpSpec {
        name: "replace_memory_facts_kind",
        drive: |f| {
            ok(f.store.replace_memory_facts_kind(
                f.sid,
                "har05:replaced",
                &[
                    ("a".to_string(), "1".to_string()),
                    ("b".to_string(), "2".to_string()),
                ],
            ))
        },
    },
    OpSpec {
        name: "upsert_memory_facts_if_unchanged",
        drive: |f| {
            ok(f.store.upsert_memory_facts_if_unchanged(
                f.sid,
                "har05:legacy",
                "k2",
                Some("v2"),
                &[("har05:legacy", "k2", "v2b")],
            ))
        },
    },
    OpSpec {
        name: "put_task_ledger",
        drive: |f| {
            ok(f.store
                .put_task_ledger(f.sid, serde_json::json!({"tasks": []})))
        },
    },
    OpSpec {
        name: "purge_session_content",
        drive: |f| ok(f.store.purge_session_content(f.sid, "har05 test")),
    },
    OpSpec {
        name: "put_instruction_binding",
        drive: |f| {
            ok(f.store.put_instruction_binding(InstructionBindingRow {
                session_id: f.sid,
                instruction_epoch: "har05-epoch".into(),
                bundle_hash: "bh".repeat(32),
                turn_op_id: Some(1),
                captured_ms: 1,
            }))
        },
    },
    OpSpec {
        name: "alloc_op_ids",
        drive: |f| ok(f.store.alloc_op_ids(f.sid, 4)),
    },
    // ---------------------------------------------------------------- tasks
    OpSpec {
        name: "upsert_task",
        drive: |f| {
            let mut row = task_row(f);
            row.updated_ms = 7;
            ok(f.store.upsert_task(&row))
        },
    },
    OpSpec {
        name: "seed_task_attachments_txn",
        drive: |f| {
            let attachment = AttachmentId {
                digest: FileHash::from([0x11; 32]),
                mime: "text/plain".into(),
                filename: None,
                size: 3,
            };
            let current = task_row(f);
            let expectation = PreparedTaskExpectation::prepare(current.clone())?;
            let mut seeded = current;
            seeded.revision = seeded.revision.checked_next().expect("revision step");
            seeded.attachments = vec![attachment.clone()];
            let prepared = PreparedTaskWrite::prepare(seeded)?;
            let prepared_ref = PreparedAttachmentRef::prepare(&attachment)?;
            ok(f.store.seed_task_attachments_txn(
                f.sid,
                Some(expectation),
                &[prepared_ref],
                prepared,
            ))
        },
    },
    OpSpec {
        name: "task_complete_verified",
        drive: |f| {
            let row = task_row(f);
            let outer =
                f.store
                    .task_complete_verified(f.sid, f.tid, row.revision, f.rec_id, 999)?;
            let _ = outer;
            Ok(())
        },
    },
    // -------------------------------------------------------- verification
    OpSpec {
        name: "verification_attempt_begin",
        drive: |f| {
            let rev = f
                .store
                .get_task(f.sid, f.tid)?
                .map(|r| r.revision)
                .unwrap_or(TaskRevision::new(1));
            let attempt = VerificationAttemptRow {
                session_id: f.sid,
                task_id: f.tid,
                attempt_op_id: 43,
                task_revision: rev,
                workspace_root: "/wt/har05".into(),
                environment_fingerprint_json: None,
                created_ms: 5,
            };
            let job = VerificationJobRow {
                session_id: f.sid,
                task_id: f.tid,
                attempt_op_id: 43,
                check_id: "lint".into(),
                ordinal: 0,
                task_revision: rev,
                workspace_root: "/wt/har05".into(),
                kind: "lint".into(),
                command: "cargo clippy".into(),
                program: "cargo".into(),
                args_json: "[]".into(),
                spec_json: Some("{}".into()),
                budget_ms: 1,
                inline_status: None,
                state: "queued".into(),
                result_json: None,
                note: None,
                op_id: None,
                environment_fingerprint_json: None,
                created_ms: 5,
                updated_ms: 5,
                finished_ms: None,
            };
            ok(f.store
                .verification_attempt_begin(&attempt, &["src/x.rs".to_string()], &[job]))
        },
    },
    OpSpec {
        name: "verification_record_put_with_evidence",
        drive: |f| {
            let rev = f
                .store
                .get_task(f.sid, f.tid)?
                .map(|r| r.revision)
                .unwrap_or(TaskRevision::new(1));
            let rec = VerificationRecordRow {
                id: VerificationRecordId::new(1),
                session_id: Some(f.sid),
                task_id: f.tid,
                revision: rev,
                workspace_id: f.ws,
                worktree_id: WorktreeId::new(1),
                tree_hash: None,
                criteria: vec![],
                checks: vec![],
                changed_files: vec![],
                unrelated_changes: vec![],
                reviewer: None,
                status: VerificationStatus::Running,
                started_ms: 9,
                completed_ms: None,
            };
            ok(f.store
                .verification_record_put_with_evidence(&rec, Some("{}"), None))
        },
    },
    OpSpec {
        name: "verification_record_finalize",
        drive: |f| {
            ok(f.store.verification_record_finalize(
                f.sid,
                f.rec_id,
                VerificationStatus::Passed,
                99,
            ))
        },
    },
    OpSpec {
        name: "verification_job_claim",
        drive: |f| {
            ok(f.store
                .verification_job_claim(f.sid, f.tid, f.attempt_op, "build", 4000, 1))
        },
    },
    OpSpec {
        name: "verification_job_resolve",
        drive: |f| {
            ok(f.store.verification_job_resolve(
                f.sid,
                f.tid,
                f.attempt_op,
                "build",
                "failed",
                None,
                Some("{}"),
                4001,
            ))
        },
    },
    OpSpec {
        name: "import_legacy_verification_facts",
        drive: |f| ok(f.store.import_legacy_verification_facts()),
    },
    // ---------------------------------------------------------- provider/turn
    OpSpec {
        name: "record_provider_call_attempt",
        drive: |f| {
            ok(f.store.record_provider_call_attempt(
                f.sid,
                &ModelCallAttempt::new(OpId::new(6001), OpId::new(6002), 0)
                    .expect("distinct attempt ids"),
                None,
                "prov",
                "model",
                "ok",
                Some(10),
                Some(20),
                None,
            ))
        },
    },
    OpSpec {
        name: "start_turn_record",
        drive: |f| {
            ok(f.store.start_turn_record(
                f.sid,
                OpId::new(4103),
                Some(1),
                None,
                "prov",
                "model",
                None,
            ))
        },
    },
    // ------------------------------------------------------------ tool runs
    OpSpec {
        name: "start_tool_run",
        drive: |f| {
            ok(f.store.start_tool_run(
                f.sid,
                OpId::new(4003),
                "shell",
                serde_json::json!({"cmd": "echo"}),
                serde_json::json!({"strategy": "verify_hash"}),
                None,
                None,
            ))
        },
    },
    OpSpec {
        name: "start_tool_run_and_event",
        drive: |f| {
            ok(f.store.start_tool_run_and_event(
                f.sid,
                OpId::new(4004),
                "shell",
                serde_json::json!({"cmd": "echo"}),
                serde_json::json!({"strategy": "verify_hash"}),
                None,
                None,
                AgentState::Idle,
                cmd_event(EventKind::ToolStarted),
            ))
        },
    },
    OpSpec {
        name: "finish_tool_run",
        drive: |f| {
            ok(f.store
                .finish_tool_run(f.sid, OpId::new(4000), "succeeded", "clean"))
        },
    },
    OpSpec {
        name: "finish_recovered_tool_run_and_event",
        drive: |f| {
            ok(f.store.finish_recovered_tool_run_and_event(
                f.sid,
                OpId::new(4005),
                "succeeded",
                "clean",
                EventKind::ToolCompleted,
                AgentState::Idle,
                None,
            ))
        },
    },
    // ---------------------------------------------------------------- billing
    OpSpec {
        name: "cost_reserve_attempt",
        drive: |f| {
            ok(f.store.cost_reserve_attempt(
                f.sid,
                f.tid,
                &ModelCallAttempt::new(OpId::new(6005), OpId::new(6006), 0)
                    .expect("distinct attempt ids"),
                500,
                1_700_000_000_000,
                None,
                ExecutionClass::HostedEconomy,
            ))
        },
    },
    OpSpec {
        name: "cost_mark_dispatched",
        drive: |f| ok(f.store.cost_mark_dispatched(1, 1_700_000_000_001)),
    },
    OpSpec {
        name: "cost_settle",
        drive: |f| {
            ok(f.store
                .cost_settle(1, 500, Some(100), Some(90), None, 1_700_000_000_002))
        },
    },
    OpSpec {
        name: "cost_settle_usage_with_context",
        drive: |f| {
            ok(f.store.cost_settle_usage_with_context(
                3,
                10,
                5,
                0,
                7,
                Some(11),
                None,
                1_700_000_000_004,
                "prov",
                "model",
            ))
        },
    },
    OpSpec {
        name: "cost_refund",
        drive: |f| ok(f.store.cost_refund(4, 1_700_000_000_006)),
    },
    OpSpec {
        name: "cost_recover_open_reservations",
        drive: |f| ok(f.store.cost_recover_open_reservations(1_700_000_000_008)),
    },
    OpSpec {
        name: "billing_outbox_record",
        drive: |f| {
            ok(f.store.billing_outbox_record(BillingOutboxFact {
                reservation_id: 2,
                session_id: f.sid,
                task_id: f.tid,
                attempt_id: "attempt-har05".into(),
                provider: "prov".into(),
                model: "model".into(),
                uncached_input_tokens: 10,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: 5,
                reasoning_tokens: 0,
                provider_cost_micro: 7,
                provider_reported_micro: Some(7),
                occurred_at_ms: 1_700_000_000_009,
                source_operation: "cost_settle".into(),
            }))
        },
    },
    OpSpec {
        name: "set_billing_period_config",
        drive: |f| {
            ok(f.store
                .set_billing_period_config(1_700_000_000_000, 86_400_000))
        },
    },
    OpSpec {
        name: "model_outcome_stats_append",
        drive: |f| {
            ok(f.store.model_outcome_stats_append(
                "prov",
                "model",
                RouterPhase::Implement,
                TaskClass::Easy,
                RiskBucket::Low,
                ModelOutcomeSample {
                    verified_success: true,
                    rework_cost_micro: 0,
                    rework_turns: 0,
                },
            ))
        },
    },
    OpSpec {
        name: "engineering_outcome_append",
        drive: |f| {
            ok(f.store.engineering_outcome_append(
                &outcome_row("har05-run-1"),
                &[],
                &settlement_row("har05-run-1"),
            ))
        },
    },
    OpSpec {
        name: "economic_settlement_append",
        drive: |f| {
            ok(f.store
                .economic_settlement_append(&settlement_row("har05-run-2")))
        },
    },
    OpSpec {
        name: "terminal_run_facts_upsert",
        drive: |f| ok(f.store.terminal_run_facts_upsert("har05-run-1", "{}")),
    },
    // ------------------------------------------------------------- queue
    OpSpec {
        name: "enqueue_prompt",
        drive: |f| {
            ok(f.store.enqueue_prompt(
                f.sid,
                OpId::new(1103),
                "har05 second prompt",
                &[],
                None,
                None,
                None,
                1_700_000_000_010,
            ))
        },
    },
    OpSpec {
        name: "edit_queued_prompt",
        drive: |f| {
            ok(f.store
                .edit_queued_prompt(f.sid, OpId::new(1002), "har05 edited", &[]))
        },
    },
    OpSpec {
        name: "mark_queue_status",
        drive: |f| ok(f.store.mark_queue_status(f.sid, f.queue_seq, "claimed")),
    },
    OpSpec {
        name: "admit_queue_head",
        drive: |f| ok(f.store.admit_queue_head(f.sid, &["idle"], "preparing")),
    },
    OpSpec {
        name: "recover_claimed_queue_rows",
        drive: |f| ok(f.store.recover_claimed_queue_rows(f.sid)),
    },
    OpSpec {
        name: "bump_loop_signal",
        drive: |f| {
            ok(f.store
                .bump_loop_signal(f.sid, "har05", 3, 1_700_000_000_011))
        },
    },
    // ------------------------------------------------------------- messages
    OpSpec {
        name: "put_message",
        drive: |f| {
            ok(f.store.put_message(
                f.sid,
                200,
                "assistant",
                serde_json::json!({"text": "har05 reply"}),
            ))
        },
    },
    OpSpec {
        name: "put_part",
        drive: |f| {
            ok(f.store
                .put_part(f.mid, "text", serde_json::json!({"text": "extra"})))
        },
    },
    OpSpec {
        name: "batch_hot_writes",
        drive: |f| {
            ok(f.store.batch_hot_writes(&[
                hot_event(EventKind::ModelStarted),
                HotWrite::PutMessage {
                    session_id: f.sid,
                    seq: 201,
                    role: "assistant".into(),
                    data: serde_json::json!({"text": "hot"}),
                },
                HotWrite::PutPart {
                    message_id: f.mid,
                    kind: "text".into(),
                    data: serde_json::json!({"text": "hot part"}),
                },
            ]))
        },
    },
    // -------------------------------------------------------- child runtime
    OpSpec {
        name: "child_runtime_put",
        drive: |f| {
            ok(f.store.child_runtime_put(&ChildRuntimeRow {
                session_id: f.sid,
                child_id: "har05-child".into(),
                state: "running".into(),
                blocker_kind: None,
                blocker_reason: None,
                blocker_dependency: None,
                blocker_resolution: None,
                last_progress_ms: None,
                updated_ms: 1,
            }))
        },
    },
    // -------------------------------------------------------- admissions
    OpSpec {
        name: "task_admission_claim",
        drive: |f| {
            ok(f.store.task_admission_claim(
                f.sid,
                "adkey-1",
                "digest-1",
                "reservation-1",
                1_700_000_000_016,
            ))
        },
    },
    OpSpec {
        name: "task_admission_resolve",
        drive: |f| {
            ok(f.store.task_admission_resolve(
                "adkey-1",
                Some("reservation-1"),
                AdmissionResolution::Reclaim,
            ))
        },
    },
    OpSpec {
        name: "prompt_admission_claim",
        drive: |f| {
            ok(f.store.prompt_admission_claim(
                f.sid,
                "pk-1",
                "pdigest-1",
                "preset-1",
                1_700_000_000_018,
            ))
        },
    },
    OpSpec {
        name: "prompt_admission_resolve",
        drive: |f| {
            ok(f.store.prompt_admission_resolve(
                "pk-1",
                Some("preset-1"),
                AdmissionResolution::Reclaim,
            ))
        },
    },
    OpSpec {
        name: "billing_outbox_backfill_done",
        drive: |f| ok(f.store.billing_outbox_backfill_done()),
    },
    // ---------------------------------------------------- longtask/compaction
    OpSpec {
        name: "insert_compaction_operation",
        drive: |f| {
            ok(f.store.insert_compaction_operation(CompactionOperationRow {
                session_id: f.sid,
                operation_id: "har05-op".into(),
                state: "pending".into(),
                phase: "queued".into(),
                progress_ppm: 0,
                expected_revision: None,
                manifest_revision: None,
                before_tokens: None,
                after_tokens: None,
                target_tokens: 1000,
                min_reduction_ppm: 300_000,
                strategy: None,
                result_json: None,
                error_code: None,
                error_message: None,
                created_ms: 1,
                updated_ms: 1,
            }))
        },
    },
    OpSpec {
        name: "update_compaction_operation",
        drive: |f| {
            ok(f.store.update_compaction_operation(
                f.sid,
                "har05-op",
                CompactionOperationUpdate {
                    state: "running",
                    phase: "scanning",
                    progress_ppm: 100_000,
                    before_tokens: None,
                    after_tokens: None,
                    target_tokens: None,
                    manifest_revision: None,
                    strategy: None,
                    result_json: None,
                    error_code: None,
                    error_message: None,
                },
            ))
        },
    },
    OpSpec {
        name: "memory_policy_cas",
        drive: |f| {
            ok(f.store
                .memory_policy_cas("session", f.sid.raw() as i64, 0, "{\"har05\":true}", 3))
        },
    },
    OpSpec {
        name: "index_state_put",
        drive: |f| {
            ok(f.store
                .index_state_put(f.ws, "{\"state\":\"ready\",\"generation\":1}", 1, "har05"))
        },
    },
    OpSpec {
        name: "index_state_cas",
        drive: |f| {
            ok(f.store.index_state_cas(
                f.ws,
                "{\"state\":\"ready\",\"generation\":0}",
                0,
                "{\"state\":\"ready\",\"generation\":2}",
                2,
                "har05",
            ))
        },
    },
    // ------------------------------------------------------------ maintenance
    OpSpec {
        name: "integrity_check",
        drive: |f| ok(f.store.integrity_check()),
    },
    OpSpec {
        name: "quick_integrity_check",
        drive: |f| ok(f.store.quick_integrity_check()),
    },
    OpSpec {
        name: "journal_consistency_issues",
        drive: |f| ok(f.store.journal_consistency_issues()),
    },
    OpSpec {
        name: "diagnostics_with",
        drive: |f| ok(f.store.diagnostics_with(crate::connection::check_integrity)),
    },
    OpSpec {
        name: "sql_execute",
        drive: |f| ok(f.store.sql_execute("SELECT 1")),
    },
    OpSpec {
        name: "wal_checkpoint_passive",
        drive: |f| ok(f.store.wal_checkpoint_passive()),
    },
    OpSpec {
        name: "wal_checkpoint_truncate_bounded",
        drive: |f| ok(f.store.wal_checkpoint_truncate_bounded()),
    },
    // ------------------------------------------------------------------ reads
    OpSpec {
        name: "session_usage_aggregates",
        drive: |f| ok(f.store.session_usage_aggregates(&[f.sid])),
    },
    OpSpec {
        name: "usage_facts_by_session",
        drive: |f| ok(f.store.usage_facts_by_session(&[f.sid])),
    },
    OpSpec {
        name: "events_range",
        drive: |f| ok(f.store.events_range(f.sid, 1, None)),
    },
    OpSpec {
        name: "message_count",
        drive: |f| ok(f.store.message_count(f.sid)),
    },
    OpSpec {
        name: "messages_before_page",
        drive: |f| {
            ok(f.store
                .messages_before_page(f.sid, Some(9_000_000), Some(10)))
        },
    },
    OpSpec {
        name: "messages_backwards_bounded",
        drive: |f| {
            ok(f.store
                .messages_backwards_bounded(f.sid, Some(9_000_000), 10, 4096))
        },
    },
    OpSpec {
        name: "messages_window_with_parts",
        drive: |f| {
            ok(f.store
                .messages_window_with_parts(f.sid, None, None, 10, 4096))
        },
    },
    OpSpec {
        name: "parts_for_messages",
        drive: |f| ok(f.store.parts_for_messages(&[f.mid])),
    },
    OpSpec {
        name: "search_messages",
        drive: |f| ok(f.store.search_messages(f.sid, "marker", 10)),
    },
    OpSpec {
        name: "provider_call_prefix_rows_limited",
        drive: |f| ok(f.store.provider_call_prefix_rows_limited(f.sid, Some(10))),
    },
    OpSpec {
        name: "ledger_entries_by_type_page",
        drive: |f| {
            ok(f.store.ledger_entries_by_type_page(
                f.sid,
                &["goal_set", "blocker_opened"],
                Some(0),
                Some(10),
            ))
        },
    },
    OpSpec {
        name: "ledger_head",
        drive: |f| ok(f.store.ledger_head(f.sid)),
    },
    OpSpec {
        name: "journal_session_problems",
        drive: |f| ok(f.store.journal_session_problems(f.sid, JournalDepth::Deep)),
    },
    OpSpec {
        name: "memory_fact_by_key",
        drive: |f| ok(f.store.memory_fact_by_key(f.sid, "k")),
    },
    OpSpec {
        name: "memory_facts",
        drive: |f| ok(f.store.memory_facts(f.sid)),
    },
    OpSpec {
        name: "memory_facts_of_kinds",
        drive: |f| {
            ok(f.store
                .memory_facts_of_kinds(f.sid, &["memory_v2:har05"], 10))
        },
    },
    OpSpec {
        name: "memory_facts_by_kind_page",
        drive: |f| {
            ok(f.store
                .memory_facts_by_kind_page(f.sid, "memory_v2:har05", None, 10))
        },
    },
    OpSpec {
        name: "checkpoints_of",
        drive: |f| ok(f.store.checkpoints_of(f.sid)),
    },
    OpSpec {
        name: "checkpoints_newest",
        drive: |f| ok(f.store.checkpoints_newest(f.sid, 10)),
    },
    OpSpec {
        name: "evidence_get",
        drive: |f| ok(f.store.evidence_get(1)),
    },
    OpSpec {
        name: "evidence_list_by_scope",
        drive: |f| ok(f.store.evidence_list_by_scope(f.sid, f.ws, 10)),
    },
    OpSpec {
        name: "cas_hash_references",
        drive: |f| ok(f.store.cas_hash_references()),
    },
    OpSpec {
        name: "attachment",
        drive: |f| ok(f.store.attachment(f.sid, FileHash::from([0x33; 32]))),
    },
    OpSpec {
        name: "attachment_ref",
        drive: |f| ok(f.store.attachment_ref(f.sid, 1)),
    },
    OpSpec {
        name: "attachments_by_digest",
        drive: |f| {
            ok(f.store
                .attachments_by_digest(f.sid, FileHash::from([0x33; 32])))
        },
    },
    OpSpec {
        name: "billing_outbox_after",
        drive: |f| ok(f.store.billing_outbox_after(0, 10)),
    },
    OpSpec {
        name: "billing_period_config",
        drive: |f| ok(f.store.billing_period_config()),
    },
    OpSpec {
        name: "provider_call_task_rows",
        drive: |f| ok(f.store.provider_call_task_rows(f.sid, f.tid, 10)),
    },
    OpSpec {
        name: "pending_tool_runs",
        drive: |f| ok(f.store.pending_tool_runs(f.sid)),
    },
    OpSpec {
        name: "active_turn_record",
        drive: |f| ok(f.store.active_turn_record(f.sid)),
    },
    OpSpec {
        name: "queue_head",
        drive: |f| ok(f.store.queue_head(f.sid)),
    },
    OpSpec {
        name: "index_state_log",
        drive: |f| ok(f.store.index_state_log(f.ws, 10)),
    },
    OpSpec {
        name: "session_ids",
        drive: |f| ok::<Vec<SessionId>>(Ok(vec![f.sid])),
    },
    OpSpec {
        name: "list_sessions_page_at",
        drive: |f| ok(f.store.list_sessions_page_at(Some(f.ws), None, 10, None)),
    },
    OpSpec {
        name: "task_usage_scope",
        drive: |f| ok(f.store.task_usage_scope(&[f.sid], 10)),
    },
    OpSpec {
        name: "get_task",
        drive: |f| ok(f.store.get_task(f.sid, f.tid)),
    },
    OpSpec {
        name: "list_tasks",
        drive: |f| ok(f.store.list_tasks(f.sid)),
    },
    OpSpec {
        name: "verification_attempt_get",
        drive: |f| ok(f.store.verification_attempt_get(f.sid, f.tid, f.attempt_op)),
    },
    OpSpec {
        name: "verification_record_get",
        drive: |f| ok(f.store.verification_record_get(f.sid, f.rec_id)),
    },
    OpSpec {
        name: "verification_record_head_by_tasks",
        drive: |f| {
            ok(f.store
                .verification_record_head_by_tasks(f.sid, &[f.tid], 5))
        },
    },
    OpSpec {
        name: "verification_jobs_for_attempt",
        drive: |f| {
            ok(f.store
                .verification_jobs_for_attempt(f.sid, f.tid, f.attempt_op))
        },
    },
    OpSpec {
        name: "verification_jobs_open",
        drive: |f| ok(f.store.verification_jobs_open(f.sid, f.tid)),
    },
    OpSpec {
        name: "engineering_outcome_page",
        drive: |f| ok(f.store.engineering_outcome_page(None, 10)),
    },
    OpSpec {
        name: "engineering_outcome_count",
        drive: |f| ok(f.store.engineering_outcome_count()),
    },
    OpSpec {
        name: "economic_settlement_latest_for_runs",
        drive: |f| {
            ok(f.store
                .economic_settlement_latest_for_runs(&["har05-run-1".into()]))
        },
    },
    OpSpec {
        name: "compaction_operation",
        drive: |f| ok(f.store.compaction_operation(f.sid, "har05-op")),
    },
    // --------------------------------------------------- missing batch (130)
    OpSpec {
        name: "active_turn_ownership_invariants",
        drive: |f| ok(f.store.active_turn_ownership_invariants()),
    },
    OpSpec {
        name: "all_active_turns",
        drive: |f| ok(f.store.all_active_turns()),
    },
    OpSpec {
        name: "all_pending_permissions",
        drive: |f| ok(f.store.all_pending_permissions()),
    },
    OpSpec {
        name: "all_running_tool_rows",
        drive: |f| ok(f.store.all_running_tool_rows()),
    },
    OpSpec {
        name: "artifact_for",
        drive: |f| ok(f.store.artifact_for(f.sid, "aa".repeat(32).as_str())),
    },
    OpSpec {
        name: "attachment_refs_for_digest",
        drive: |f| {
            ok(f.store
                .attachment_refs_for_digest(f.sid, FileHash::from([0x44; 32])))
        },
    },
    OpSpec {
        name: "attachment_row",
        drive: |f| {
            ok(f.store.attachment_row(
                f.sid,
                &AttachmentId {
                    digest: FileHash::from([0x55; 32]),
                    mime: "text/plain".into(),
                    filename: None,
                    size: 1,
                },
            ))
        },
    },
    OpSpec {
        name: "bump_tool_run_attempt",
        drive: |f| ok(f.store.bump_tool_run_attempt(f.sid, OpId::new(4002))),
    },
    OpSpec {
        name: "cancel_queued_ops",
        drive: |f| ok(f.store.cancel_queued_ops(f.sid, &[OpId::new(1002)])),
    },
    OpSpec {
        name: "checkpoint_by_id",
        drive: |f| ok(f.store.checkpoint_by_id(f.sid, 1)),
    },
    OpSpec {
        name: "child_runtime_delete",
        drive: |f| ok(f.store.child_runtime_delete(f.sid)),
    },
    OpSpec {
        name: "child_runtime_get",
        drive: |f| ok(f.store.child_runtime_get(f.sid)),
    },
    OpSpec {
        name: "clear_checkpoint_restored",
        drive: |f| ok(f.store.clear_checkpoint_restored(1)),
    },
    OpSpec {
        name: "compare_and_swap_memory_fact",
        drive: |f| {
            ok(f.store.compare_and_swap_memory_fact(
                f.sid,
                "memory_v2:har05",
                "k",
                blake3::hash(b"v").to_hex().as_str(),
                "v2",
            ))
        },
    },
    OpSpec {
        name: "compare_and_swap_workspace_memory_fact",
        drive: |f| {
            ok(f.store.compare_and_swap_workspace_memory_fact(
                f.ws.raw(),
                "memory_v2:har05",
                "wk",
                blake3::hash(b"wv").to_hex().as_str(),
                "wv2",
            ))
        },
    },
    OpSpec {
        name: "cost_finalize_uncertain",
        drive: |f| {
            ok(f.store
                .cost_finalize_uncertain(f.sid, f.tid, 1_700_000_000_020))
        },
    },
    OpSpec {
        name: "cost_mark_uncertain",
        drive: |f| {
            ok(f.store
                .cost_mark_uncertain(2, "provider-5xx", None, 1_700_000_000_021))
        },
    },
    OpSpec {
        name: "cost_reconcile_uncertain",
        drive: |f| {
            ok(f.store
                .cost_reconcile_uncertain(f.sid, f.tid, 1_700_000_000_022))
        },
    },
    OpSpec {
        name: "cost_reservation_invariants",
        drive: |f| ok(f.store.cost_reservation_invariants()),
    },
    OpSpec {
        name: "cost_reservation_state",
        drive: |f| ok(f.store.cost_reservation_state(1)),
    },
    OpSpec {
        name: "cost_reservations_of",
        drive: |f| ok(f.store.cost_reservations_of(f.sid, f.tid, 10)),
    },
    OpSpec {
        name: "cost_reserve_inner",
        drive: |f| {
            ok(f.store.cost_reserve_inner(
                f.sid,
                f.tid,
                OpId::new(6102),
                250,
                1_700_000_000_023,
                None,
                ExecutionClass::Local,
            ))
        },
    },
    OpSpec {
        name: "cost_task_cap_set",
        drive: |f| ok(f.store.cost_task_cap_set(f.sid, f.tid, Some(1_000_000))),
    },
    OpSpec {
        name: "cost_task_row",
        drive: |f| ok(f.store.cost_task_row(f.sid, f.tid)),
    },
    OpSpec {
        name: "delete_memory_fact",
        drive: |f| ok(f.store.delete_memory_fact(f.sid, "har05:doomed1", "k")),
    },
    OpSpec {
        name: "delete_memory_fact_cas",
        drive: |f| {
            ok(f.store
                .delete_memory_fact_cas(f.sid, "har05:doomed2", "k", None))
        },
    },
    OpSpec {
        name: "delete_memory_facts_before",
        drive: |f| {
            ok(f.store
                .delete_memory_facts_before(f.sid, 9_999_999_999_999, 1))
        },
    },
    OpSpec {
        name: "delete_message",
        drive: |f| ok(f.store.delete_message(f.sid, 300)),
    },
    OpSpec {
        name: "delete_workspace_memory_fact",
        drive: |f| {
            ok(f.store
                .delete_workspace_memory_fact(f.ws.raw(), "har05:absent-kind", "k"))
        },
    },
    OpSpec {
        name: "economic_settlement_latest",
        drive: |f| ok(f.store.economic_settlement_latest("har05-run-1")),
    },
    OpSpec {
        name: "engineering_outcome_get",
        drive: |f| ok(f.store.engineering_outcome_get("har05-run-1")),
    },
    OpSpec {
        name: "engineering_outcome_human_interventions",
        drive: |f| ok(f.store.engineering_outcome_human_interventions(f.sid)),
    },
    OpSpec {
        name: "engineering_outcome_metrics_snapshot",
        drive: |f| ok(f.store.engineering_outcome_metrics_snapshot(10)),
    },
    OpSpec {
        name: "engineering_outcome_page_with_settlements",
        drive: |f| ok(f.store.engineering_outcome_page_with_settlements(None, 10)),
    },
    OpSpec {
        name: "engineering_outcome_recovery_events",
        drive: |f| ok(f.store.engineering_outcome_recovery_events(f.sid)),
    },
    OpSpec {
        name: "events_versioned_range",
        drive: |f| ok(f.store.events_versioned_range(f.sid, 1, Some(10))),
    },
    OpSpec {
        name: "evidence_high_water",
        drive: |f| ok(f.store.evidence_high_water()),
    },
    OpSpec {
        name: "evidence_ids_by_backing",
        drive: |f| {
            ok(f.store
                .evidence_ids_by_backing("cd".repeat(32).as_str(), 10))
        },
    },
    OpSpec {
        name: "evidence_list_by_scope_newest",
        drive: |f| ok(f.store.evidence_list_by_scope_newest(f.sid, f.ws, None, 10)),
    },
    OpSpec {
        name: "expired_pending_permissions",
        drive: |f| ok(f.store.expired_pending_permissions(f.sid, i64::MAX)),
    },
    OpSpec {
        name: "finish_tool_run_and_event",
        drive: |f| {
            ok(f.store.finish_tool_run_and_event(
                f.sid,
                OpId::new(4006),
                "succeeded",
                "clean",
                AgentState::Idle,
                cmd_event(EventKind::ToolCompleted),
            ))
        },
    },
    OpSpec {
        name: "get_memory_fact",
        drive: |f| ok(f.store.get_memory_fact(f.sid, "memory_v2:har05", "k")),
    },
    OpSpec {
        name: "get_session",
        drive: |f| ok(f.store.get_session(f.sid)),
    },
    OpSpec {
        name: "get_task_ledger",
        drive: |f| ok(f.store.get_task_ledger(f.sid)),
    },
    OpSpec {
        name: "index_state_get",
        drive: |f| ok(f.store.index_state_get(f.ws)),
    },
    OpSpec {
        name: "inherit_attachment_refs",
        drive: |f| {
            ok(f.store.inherit_attachment_refs(
                f.sid,
                &[AttachmentId {
                    digest: FileHash::from([0x66; 32]),
                    mime: "text/plain".into(),
                    filename: None,
                    size: 2,
                }],
            ))
        },
    },
    OpSpec {
        name: "insert_memory_fact_if_absent",
        drive: |f| {
            ok(f.store
                .insert_memory_fact_if_absent(f.sid, "har05:absent", "a", "1"))
        },
    },
    OpSpec {
        name: "insert_workspace_memory_fact_if_absent",
        drive: |f| {
            ok(f.store
                .insert_workspace_memory_fact_if_absent(f.ws.raw(), "har05:absent", "a", "1"))
        },
    },
    OpSpec {
        name: "instruction_binding_get",
        drive: |f| ok(f.store.instruction_binding_get(f.sid)),
    },
    OpSpec {
        name: "last_event_seq",
        drive: |f| ok(f.store.last_event_seq(f.sid)),
    },
    OpSpec {
        name: "latest_checkpoint",
        drive: |f| ok(f.store.latest_checkpoint(f.sid)),
    },
    OpSpec {
        name: "latest_compaction_operation",
        drive: |f| ok(f.store.latest_compaction_operation(f.sid)),
    },
    OpSpec {
        name: "latest_turn_record",
        drive: |f| ok(f.store.latest_turn_record(f.sid)),
    },
    OpSpec {
        name: "ledger_entries_by_type_desc_page",
        drive: |f| {
            ok(f.store.ledger_entries_by_type_desc_page(
                f.sid,
                &["goal_set", "blocker_opened"],
                Some(9_000_000),
                Some(10),
            ))
        },
    },
    OpSpec {
        name: "ledger_entries_desc_page",
        drive: |f| {
            ok(f.store
                .ledger_entries_desc_page(f.sid, Some(9_000_000), Some(10)))
        },
    },
    OpSpec {
        name: "ledger_entries_page",
        drive: |f| ok(f.store.ledger_entries_page(f.sid, None, Some(10))),
    },
    OpSpec {
        name: "ledger_max_seq",
        drive: |f| ok(f.store.ledger_max_seq(f.sid)),
    },
    OpSpec {
        name: "list_attachments",
        drive: |f| ok(f.store.list_attachments(f.sid, 10)),
    },
    OpSpec {
        name: "list_sessions",
        drive: |f| ok(f.store.list_sessions(Some(f.ws))),
    },
    OpSpec {
        name: "loop_signal_counts",
        drive: |f| ok(f.store.loop_signal_counts(f.sid)),
    },
    OpSpec {
        name: "mark_billing_outbox_backfill_done",
        drive: |f| ok(f.store.mark_billing_outbox_backfill_done()),
    },
    OpSpec {
        name: "mark_checkpoint_restored",
        drive: |f| ok(f.store.mark_checkpoint_restored(1)),
    },
    OpSpec {
        name: "memory_fact_count",
        drive: |f| ok(f.store.memory_fact_count(f.sid)),
    },
    OpSpec {
        name: "memory_fact_count_v2",
        drive: |f| ok(f.store.memory_fact_count_v2(f.sid)),
    },
    OpSpec {
        name: "memory_fact_get",
        drive: |f| ok(f.store.memory_fact_get(f.sid, "memory_v2:har05", "k")),
    },
    OpSpec {
        name: "memory_fact_key_prefix_exists",
        drive: |f| {
            ok(f.store
                .memory_fact_key_prefix_exists(f.sid, "memory_v2:har05", "k"))
        },
    },
    OpSpec {
        name: "memory_fact_rows_of_kinds",
        drive: |f| ok(f.store.memory_fact_rows_of_kinds(&["memory_v2:har05"])),
    },
    OpSpec {
        name: "memory_facts_by_kind_key_ordered",
        drive: |f| {
            ok(f.store
                .memory_facts_by_kind_key_ordered(f.sid, "memory_v2:har05", 10))
        },
    },
    OpSpec {
        name: "memory_facts_by_kind_key_prefix",
        drive: |f| {
            ok(f.store
                .memory_facts_by_kind_key_prefix(f.sid, "memory_v2:har05", "k", None, 10))
        },
    },
    OpSpec {
        name: "memory_policy_get",
        drive: |f| ok(f.store.memory_policy_get("session", f.sid.raw() as i64)),
    },
    OpSpec {
        name: "message_created_ms",
        drive: |f| ok(f.store.message_created_ms(f.sid, 1)),
    },
    OpSpec {
        name: "model_outcome_stats_get",
        drive: |f| {
            ok(f.store.model_outcome_stats_get(
                "prov",
                "model",
                RouterPhase::Implement,
                TaskClass::Easy,
                RiskBucket::Low,
            ))
        },
    },
    OpSpec {
        name: "model_outcome_stats_phase",
        drive: |f| {
            ok(f.store
                .model_outcome_stats_phase("prov", "model", RouterPhase::Implement))
        },
    },
    OpSpec {
        name: "op_id_seq_high_water",
        drive: |f| ok(f.store.op_id_seq_high_water()),
    },
    OpSpec {
        name: "parts_of",
        drive: |f| ok(f.store.parts_of(f.mid)),
    },
    OpSpec {
        name: "pending_permission",
        drive: |f| ok(f.store.pending_permission(f.pid)),
    },
    OpSpec {
        name: "permission_decision",
        drive: |f| ok(f.store.permission_decision(f.pid)),
    },
    OpSpec {
        name: "prompt_admission_complete",
        drive: |f| {
            ok(f.store
                .prompt_admission_complete(f.sid, "pk-1", "preset-1", "{}"))
        },
    },
    OpSpec {
        name: "prompt_admission_pending_page",
        drive: |f| ok(f.store.prompt_admission_pending_page(None, 10)),
    },
    OpSpec {
        name: "prompt_admission_release",
        drive: |f| ok(f.store.prompt_admission_release(f.sid, "pk-1", "preset-1")),
    },
    OpSpec {
        name: "prune_compaction_operations_keep_newest",
        drive: |f| ok(f.store.prune_compaction_operations_keep_newest(f.sid, 1)),
    },
    OpSpec {
        name: "prune_memory_facts_kind_keep_newest",
        drive: |f| {
            ok(f.store
                .prune_memory_facts_kind_keep_newest(f.sid, "memory_v2:har05", 1))
        },
    },
    OpSpec {
        name: "put_artifact",
        drive: |f| {
            ok(f.store
                .put_artifact(f.sid, "dag", "ab".repeat(32).as_str(), "har05 artifact", 5))
        },
    },
    OpSpec {
        name: "put_attachment_ref",
        drive: |f| {
            ok(f.store.put_attachment_ref(
                f.sid,
                &AttachmentId {
                    digest: FileHash::from([0x77; 32]),
                    mime: "text/plain".into(),
                    filename: None,
                    size: 7,
                },
            ))
        },
    },
    OpSpec {
        name: "put_checkpoint",
        drive: |f| {
            ok(f.store.put_checkpoint(
                f.sid,
                50,
                "src/e.rs",
                "beforehash55555",
                "afterhash55555",
                None,
            ))
        },
    },
    OpSpec {
        name: "queue_op_ids",
        drive: |f| ok(f.store.queue_op_ids(f.sid)),
    },
    OpSpec {
        name: "queue_row_by_op",
        drive: |f| ok(f.store.queue_row_by_op(f.sid, OpId::new(1002))),
    },
    OpSpec {
        name: "queue_rows",
        drive: |f| ok(f.store.queue_rows(f.sid, 10)),
    },
    OpSpec {
        name: "queue_status_counts",
        drive: |f| ok(f.store.queue_status_counts(f.sid)),
    },
    OpSpec {
        name: "record_compaction",
        drive: |f| {
            ok(f.store
                .record_compaction(f.sid, 1000, 400, 800, true, "auto"))
        },
    },
    OpSpec {
        name: "record_tool_postcondition",
        drive: |f| {
            ok(f.store.record_tool_postcondition(
                f.sid,
                OpId::new(4002),
                &serde_json::json!({"hash": "ok"}),
            ))
        },
    },
    OpSpec {
        name: "record_tool_pre_manifest",
        drive: |f| {
            ok(f.store.record_tool_pre_manifest(
                f.sid,
                OpId::new(4002),
                &serde_json::json!({"files": 1}),
            ))
        },
    },
    OpSpec {
        name: "request_compaction_cancel",
        drive: |f| ok(f.store.request_compaction_cancel(f.sid, "har05-op")),
    },
    OpSpec {
        name: "reset_loop_signals",
        drive: |f| ok(f.store.reset_loop_signals(f.sid)),
    },
    OpSpec {
        name: "resolve_permission",
        drive: |f| ok(f.store.resolve_permission(f.pid, f.sid, "granted")),
    },
    OpSpec {
        name: "search_session_titles",
        drive: |f| ok(f.store.search_session_titles("har05", 10)),
    },
    OpSpec {
        name: "session_applied_tool_runs",
        drive: |f| ok(f.store.session_applied_tool_runs(f.sid, 10)),
    },
    OpSpec {
        name: "session_completed_tool_runs",
        drive: |f| ok(f.store.session_completed_tool_runs(f.sid, 10)),
    },
    OpSpec {
        name: "session_deletion",
        drive: |f| ok(f.store.session_deletion(f.sid)),
    },
    OpSpec {
        name: "session_listing_snapshot",
        drive: |f| ok(f.store.session_listing_snapshot()),
    },
    OpSpec {
        name: "session_stored_prefix_stability",
        drive: |f| ok(f.store.session_stored_prefix_stability(f.sid)),
    },
    OpSpec {
        name: "session_usage_tokens",
        drive: |f| ok(f.store.session_usage_tokens(f.sid)),
    },
    OpSpec {
        name: "sessions_with_pending_queues",
        drive: |f| ok(f.store.sessions_with_pending_queues()),
    },
    OpSpec {
        name: "set_session_lifecycle",
        drive: |f| {
            ok(f.store
                .set_session_lifecycle(f.sid, SessionLifecycle::Suspended))
        },
    },
    OpSpec {
        name: "set_tool_run_effect",
        drive: |f| {
            ok(f.store
                .set_tool_run_effect(f.sid, OpId::new(4002), "applied"))
        },
    },
    OpSpec {
        name: "set_turn_record_envelope",
        drive: |f| {
            ok(f.store.set_turn_record_envelope(
                f.sid,
                OpId::new(4100),
                "prov",
                "model",
                None,
                None,
            ))
        },
    },
    OpSpec {
        name: "task_admission_complete",
        drive: |f| {
            ok(f.store
                .task_admission_complete(f.sid, "adkey-1", "reservation-1", "{}"))
        },
    },
    OpSpec {
        name: "task_admission_peek",
        drive: |f| ok(f.store.task_admission_peek(f.sid, "adkey-9", "digest-9", 1)),
    },
    OpSpec {
        name: "task_admission_pending_page",
        drive: |f| ok(f.store.task_admission_pending_page(None, 10)),
    },
    OpSpec {
        name: "task_admission_release",
        drive: |f| {
            ok(f.store
                .task_admission_release(f.sid, "adkey-1", "reservation-1"))
        },
    },
    OpSpec {
        name: "terminal_run_facts_get",
        drive: |f| ok(f.store.terminal_run_facts_get("har05-run-1")),
    },
    OpSpec {
        name: "tool_runs_with_pre_manifest",
        drive: |f| ok(f.store.tool_runs_with_pre_manifest(f.sid, 10)),
    },
    OpSpec {
        name: "turn_completed_count",
        drive: |f| ok(f.store.turn_completed_count(f.sid)),
    },
    OpSpec {
        name: "turn_record_of",
        drive: |f| ok(f.store.turn_record_of(f.sid, OpId::new(4100))),
    },
    OpSpec {
        name: "turn_records_of",
        drive: |f| ok(f.store.turn_records_of(f.sid)),
    },
    OpSpec {
        name: "upsert_memory_fact_cas",
        drive: |f| {
            ok(f.store
                .upsert_memory_fact_cas(f.sid, "har05:cas2", "k", "v", None))
        },
    },
    OpSpec {
        name: "upsert_memory_facts",
        drive: |f| {
            ok(f.store
                .upsert_memory_facts(f.sid, &[("har05:b", "x", "1"), ("har05:b", "y", "2")]))
        },
    },
    OpSpec {
        name: "upsert_workspace_memory_fact",
        drive: |f| {
            ok(f.store
                .upsert_workspace_memory_fact(f.ws.raw(), "har05:runtime", "rk", "rv"))
        },
    },
    OpSpec {
        name: "verification_attempt_cancel",
        drive: |f| {
            ok(f.store.verification_attempt_cancel(
                f.sid,
                f.tid,
                f.attempt_op,
                "harness cancel",
                100,
            ))
        },
    },
    OpSpec {
        name: "verification_attempt_current",
        drive: |f| ok(f.store.verification_attempt_current(f.sid, f.tid)),
    },
    OpSpec {
        name: "verification_jobs_requeue_running",
        drive: |f| ok(f.store.verification_jobs_requeue_running(f.sid, 100)),
    },
    OpSpec {
        name: "verification_record_get_with_evidence",
        drive: |f| {
            ok(f.store
                .verification_record_get_with_evidence(f.sid, f.rec_id))
        },
    },
    OpSpec {
        name: "verification_record_invariants",
        drive: |f| ok(f.store.verification_record_invariants()),
    },
    OpSpec {
        name: "verification_record_list_by_task",
        drive: |f| ok(f.store.verification_record_list_by_task(f.sid, f.tid)),
    },
    OpSpec {
        name: "verification_record_list_by_task_with_evidence",
        drive: |f| {
            ok(f.store
                .verification_record_list_by_task_with_evidence(f.sid, f.tid))
        },
    },
    OpSpec {
        name: "verification_record_lookup",
        drive: |f| ok(f.store.verification_record_lookup(f.sid, f.rec_id)),
    },
    OpSpec {
        name: "workspace_memory_fact_by_key",
        drive: |f| ok(f.store.workspace_memory_fact_by_key(f.ws.raw(), "wk")),
    },
    OpSpec {
        name: "workspace_memory_fact_count",
        drive: |f| ok(f.store.workspace_memory_fact_count(f.ws.raw())),
    },
    OpSpec {
        name: "workspace_memory_facts_by_kind_page",
        drive: |f| {
            ok(f.store
                .workspace_memory_facts_by_kind_page(f.ws.raw(), "memory_v2:har05", None, 10))
        },
    },
    OpSpec {
        name: "workspace_memory_facts_fresh",
        drive: |f| ok(f.store.workspace_memory_facts_fresh(f.ws.raw(), 10)),
    },
    OpSpec {
        name: "worktrees_of",
        drive: |f| ok(f.store.worktrees_of(f.ws)),
    },
    OpSpec {
        name: "workspace_root",
        drive: |f| ok(f.store.workspace_root(f.ws)),
    },
];
