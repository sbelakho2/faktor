//! Adversarial tests for generic-shell change attribution (P0-2 remainder):
//! shell-created/deleted/renamed files must reach the SAME accounting and
//! proof inputs `write_file` feeds, an allowed shell under a change budget
//! must still record its changes, and a crash after shell mutation must be
//! discovered by manifest reconciliation on restart — never read as
//! "unchanged".

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::runtime::tests::{scripted_provider, test_resolver, AlwaysAllow, ScriptedResponse};

use faktor_core::capability::Capability;
use faktor_core::id::OpId;
use faktor_core::resource::ResourceClass;
use faktor_core::time::{Deadline, SystemClock};
use faktor_core::WorkspaceIdentity;
use faktor_session::SessionManager;
use faktor_terminal::{EnvSpec, NetworkIsolation, ProcessOwner, ProcessSupervisor, SpawnConfig};

/// The test shell tool: a real `/bin/sh -c` through the production process
/// supervisor, with the exact shape `run_command` registers (shell
/// capability, terminal resource class, unknown external effects).
fn shell_tool() -> Tool {
    Tool {
        name: "run_command".into(),
        description: "test shell".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"]
        }),
        resource_class: ResourceClass::Terminal,
        capability: Some(Capability::ExecuteShell {
            command: String::new(),
        }),
        recovery_hint: RecoveryHint::UnknownEffect,
        path_args: vec![],
        execute: Arc::new(|ctx, args| {
            Box::pin(async move {
                let ws = ctx
                    .workspace
                    .clone()
                    .ok_or_else(|| Error::internal("no workspace wired"))?;
                let supervisor = ctx
                    .supervisor
                    .clone()
                    .ok_or_else(|| Error::internal("no supervisor wired"))?;
                let command = args
                    .get("command")
                    .and_then(|c| c.as_str())
                    .ok_or_else(|| Error::malformed("run_command requires command"))?;
                let cfg = SpawnConfig {
                    cmd: "sh".into(),
                    args: vec!["-c".into(), command.to_string()],
                    cwd: ws.root().to_path_buf(),
                    env: EnvSpec::toolchain(),
                    owner: ProcessOwner::Session(ctx.session_id),
                    capture: true,
                    network_isolation: NetworkIsolation::Inherit,
                    ..Default::default()
                };
                let out = supervisor
                    .run(cfg, Duration::from_secs(30), ctx.cancellation.clone())
                    .await?;
                Ok(ToolOutcome {
                    text: out.excerpt,
                    exit_code: out.exit_code,
                    artifact: out.artifact,
                    slice_hint: out.slice_hint,
                    effect_status: EffectStatus::Unknown,
                    postcondition: None,
                    provenance: faktor_context::compiler::ProvenanceSource::Tool,
                })
            })
        }),
    }
}

struct ShellEnv {
    deps: AgentDeps,
    dir: tempfile::TempDir,
    root: PathBuf,
    manager: Arc<SessionManager>,
    session: SessionId,
    cas: Arc<faktor_cas::Cas>,
    snapshots: Arc<faktor_snapshot::CheckpointStore>,
}

/// One real workspace + session + a runtime wired with the shell tool, the
/// process supervisor and the CAS-backed checkpoint store.
fn shell_env(script: Vec<ScriptedResponse>) -> ShellEnv {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() -> u32 { 1 }\n").unwrap();
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let ws_id = manager.create_workspace(root.to_str().unwrap()).unwrap();
    let session = manager
        .create_session(ws_id, "shell attribution", "fake", "m")
        .unwrap()
        .id();
    let cas = manager.cas();
    let snapshots = Arc::new(faktor_snapshot::CheckpointStore::new(
        cas.clone(),
        manager.store(),
    ));
    let deps = deps_on_manager(manager.clone(), script);
    ShellEnv {
        deps,
        dir,
        root,
        manager,
        session,
        cas,
        snapshots,
    }
}

/// The shell-attribution deps over an EXISTING manager (its CAS/store are
/// reused verbatim, so a reopened manager sees the same durable CAS blobs).
fn deps_on_manager(manager: Arc<SessionManager>, script: Vec<ScriptedResponse>) -> AgentDeps {
    let cas = manager.cas();
    let mut registry = ProviderRegistry::new();
    registry
        .try_register(Arc::new(scripted_provider(script)))
        .unwrap();
    let mut tool_registry = ToolRegistry::new();
    tool_registry.register(shell_tool());
    AgentDeps {
        session: manager.clone(),
        providers: Arc::new(registry),
        chunk_sink: None,
        permission_requester: Arc::new(AlwaysAllow),
        evidence: Arc::new(NoEvidence),
        tools: Arc::new(tool_registry),
        cas: Some(cas.clone()),
        workspaces: faktor_fs::WorkspaceFileService::new(),
        edit: None,
        snapshots: Some(Arc::new(faktor_snapshot::CheckpointStore::new(
            cas.clone(),
            manager.store(),
        ))),
        sandbox: None,
        supervisor: Some(ProcessSupervisor::new(cas)),
        verification: crate::VerificationService::disabled(),
        hooks: None,
        instructions_resolver: test_resolver(&manager),
        routing: crate::FixedRoutingPolicy::passthrough(),
        budgets: Arc::new(faktor_session::NoopBudget),
        model: "m".into(),
        compaction_model: None,
        compact_at_usage: 0.65,
        instructions: "test".into(),
        clock: Arc::new(SystemClock),
        tool_call_mode: ToolCallMode::Native,
        tool_deadline_ms: 30_000,
        retry_policy: faktor_core::retry::RetryPolicy::default(),
        semantic: crate::fallback_semantic_registry(),
        context_prior: None,
        secret_registry: None,
        efficiency: Default::default(),
    }
}

fn tool_call(command: &str) -> ScriptedResponse {
    ScriptedResponse::ToolCall {
        id: "shell-1".into(),
        name: "run_command".into(),
        input: serde_json::json!({ "command": command }),
    }
}

/// Every `FileChanged` payload's `changes` list of one session, as canonical
/// `status path` lines (sorted), plus its typed truncation reason when set.
fn changed_rows(handle: &faktor_session::SessionHandle) -> (Vec<String>, Option<String>) {
    let mut rows = Vec::new();
    let mut truncated = None;
    for event in handle.events_range(1, None).unwrap() {
        if event.kind != faktor_core::event::EventKind::FileChanged {
            continue;
        }
        let Some(payload) = &event.payload else {
            continue;
        };
        if let Some(reason) = payload.get("changes_truncated").and_then(|v| v.as_str()) {
            truncated = Some(reason.to_string());
        }
        if let Some(changes) = payload.get("changes").and_then(|v| v.as_array()) {
            for change in changes {
                let status = change.get("status").and_then(|v| v.as_str()).unwrap_or("?");
                let path = change.get("path").and_then(|v| v.as_str()).unwrap_or("?");
                rows.push(format!("{status} {path}"));
            }
        }
    }
    rows.sort();
    (rows, truncated)
}

/// Requirement 3(a): a shell that creates, deletes and renames 100 files has
/// EVERY actual change in the change accounting/proof inputs — byte-compared
/// against the canonical list.
#[tokio::test]
async fn shell_creates_deletes_renames_reach_change_accounting() {
    // 10 modifies, 20 deletes + 10 rename sources, 100 creations + 10 rename
    // destinations, 4 pre-existing keepers.
    let script = vec![
        tool_call(
            "mkdir -p created; i=0; while [ $i -lt 100 ]; do printf 'n%s' \"$i\" > \"created/c$i.txt\"; i=$((i+1)); done; \
             rm -rf rm_target; mkdir -p moved; i=0; while [ $i -lt 10 ]; do mv \"renamed/r$i.txt\" \"moved/r$i.txt\"; i=$((i+1)); done; \
             i=0; while [ $i -lt 10 ]; do printf 'more' >> \"mod/f$i.txt\"; i=$((i+1)); done",
        ),
        ScriptedResponse::Text("done".into()),
        ScriptedResponse::End,
    ];
    let env = shell_env(script);
    let root = env.root.clone();
    std::fs::create_dir_all(root.join("rm_target")).unwrap();
    std::fs::create_dir_all(root.join("renamed")).unwrap();
    std::fs::create_dir_all(root.join("mod")).unwrap();
    for i in 0..20 {
        std::fs::write(root.join(format!("rm_target/d{i:03}.txt")), b"gone").unwrap();
    }
    for i in 0..10 {
        std::fs::write(root.join(format!("renamed/r{i}.txt")), b"move").unwrap();
        std::fs::write(root.join(format!("mod/f{i}.txt")), b"one").unwrap();
    }
    let manager = env.manager.clone();
    let session = env.session;
    let snapshots = env.snapshots.clone();
    let runtime = AgentRuntime::new(env.deps).unwrap();
    let outcome = runtime
        .run_turn(session, "mutate through the shell", &[])
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    let handle = manager.get_session(session).unwrap().unwrap();

    let mut expected: Vec<String> = Vec::new();
    for i in 0..100 {
        expected.push(format!("added created/c{i}.txt"));
    }
    for i in 0..10 {
        expected.push(format!("added moved/r{i}.txt"));
        expected.push(format!("deleted renamed/r{i}.txt"));
        expected.push(format!("modified mod/f{i}.txt"));
    }
    for i in 0..20 {
        expected.push(format!("deleted rm_target/d{i:03}.txt"));
    }
    expected.sort();
    assert_eq!(
        expected.len(),
        150,
        "100 creates + 20 deletes + 10 renames (20) + 10 modifies"
    );

    let (rows, truncated) = changed_rows(&handle);
    assert_eq!(
        rows, expected,
        "every actual shell change must appear in the durable FileChanged accounting"
    );
    assert!(
        truncated.is_none(),
        "150 changes fit under the durable bound"
    );

    // The turn summary (the verification/proof input) carries the same
    // changed paths: the durable ledger is its exact mirror.
    let ledger = handle.get_task_ledger().unwrap().unwrap();
    let mut ledger_paths = ledger
        .get("changed_files")
        .and_then(|v| v.as_array())
        .map(|rows| {
            rows.iter()
                .filter_map(|p| p.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    ledger_paths.sort();
    let mut expected_paths: Vec<String> = expected
        .iter()
        .map(|line| line.split_once(' ').unwrap().1.to_string())
        .collect();
    expected_paths.sort();
    assert_eq!(
        ledger_paths, expected_paths,
        "the turn ledger's changed files must byte-equal the actual change set"
    );

    // Snapshot/checkpoint: every actual change has a real before/after row
    // (creations needs no pre-image; modifications/deletions use the bounded
    // pre-images captured before the shell ran).
    let checkpoints = snapshots.checkpoints(session).unwrap();
    assert_eq!(
        checkpoints.len(),
        expected_paths.len(),
        "every actual change must land a checkpoint row"
    );
}

/// Requirement 3(b): a shell permitted by an (unrestricted) change budget
/// still records its changes; the pre-exec budget gate never silently drops
/// attribution for allowed runs.
#[tokio::test]
async fn budget_allowed_shell_still_records_changes() {
    let script = vec![
        tool_call("printf a > allowed_a.txt; printf b > allowed_b.txt; printf c > allowed_c.txt"),
        ScriptedResponse::Text("done".into()),
        ScriptedResponse::End,
    ];
    let env = shell_env(script);
    let manager = env.manager.clone();
    let session = env.session;
    // The budget EXISTS but restricts nothing: empty allowed_paths = allow
    // every declared path (the shell's `**` observe token passes).
    let handle = manager.get_session(session).unwrap().unwrap();
    let budget = faktor_core::state::ChangeBudget::default();
    handle.set_change_budget(Some(&budget)).unwrap();
    let runtime = AgentRuntime::new(env.deps).unwrap();
    let outcome = runtime.run_turn(session, "mutate", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert!(
        !handle
            .events_range(1, None)
            .unwrap()
            .iter()
            .any(|e| e.kind == faktor_core::event::EventKind::PermissionDenied),
        "an allowed shell must not be refused"
    );
    let (rows, _) = changed_rows(&handle);
    assert_eq!(
        rows,
        vec![
            "added allowed_a.txt".to_string(),
            "added allowed_b.txt".to_string(),
            "added allowed_c.txt".to_string(),
        ]
    );
}

/// Requirement 3(c): crash after the shell mutated the tree but before
/// settlement. Restart reconciliation discovers the actual changed tree from
/// the durable pre-manifest and the crashed turn is never certified as
/// unchanged.
#[tokio::test]
async fn crash_after_shell_mutation_is_discovered_on_restart() {
    let env = shell_env(vec![]);
    let dir = env.dir;
    let root = env.root.clone();
    let manager = env.manager.clone();
    let session = env.session;
    let cas = env.cas.clone();
    std::fs::write(root.join("crash_gone.txt"), b"gone").unwrap();
    std::fs::write(root.join("crash_mod.txt"), b"one").unwrap();
    let runtime = AgentRuntime::new(env.deps).unwrap();
    let handle = manager.get_session(session).unwrap().unwrap();
    let receipt = handle.submit_prompt("crash mid-shell", &[]).unwrap();
    let turn_op = receipt.op_id;
    // Drive the machine to ExecutingTool exactly like the live batch does.
    handle
        .append_event(
            faktor_core::event::EventKind::ContextPrepared,
            AgentState::BuildingContext,
            Some(turn_op),
            None,
        )
        .unwrap();
    handle
        .append_event(
            faktor_core::event::EventKind::ModelStarted,
            AgentState::WaitingForModel,
            Some(turn_op),
            None,
        )
        .unwrap();
    handle
        .append_event(
            faktor_core::event::EventKind::ModelChunkReceived,
            AgentState::Streaming,
            Some(turn_op),
            None,
        )
        .unwrap();
    let perm = handle
        .request_permission(
            turn_op,
            &Capability::ExecuteShell {
                command: String::new(),
            },
        )
        .unwrap();
    handle
        .resolve_permission(perm.id, PermissionDecision::Allow)
        .unwrap();
    let op_id = manager.try_next_op_id().unwrap();
    let meta = OpMeta::new(
        op_id,
        session,
        Deadline::at(manager.now_ms() + 60_000),
        faktor_core::retry::RetryPolicy::default(),
        CancellationToken::new(),
        RecoveryStrategy::MarkUnknown,
        manager.now_ms(),
    );
    let args = serde_json::json!({
        "command": "printf new > crash_new.txt; rm crash_gone.txt; printf two >> crash_mod.txt"
    });
    handle
        .start_tool_run(meta, "run_command", args.clone())
        .unwrap();

    // Execute through the REAL shell-attribution path (pre-manifest durable
    // BEFORE the shell runs), then simulate the crash: no settlement, no
    // Finish, no Postcondition — the process simply dies.
    let row = handle.row().unwrap();
    let identity = WorkspaceIdentity::new(row.workspace_id, row.worktree_id, row.task_id);
    let ws_handle = runtime
        .deps()
        .workspaces
        .open(row.workspace_id, root.clone())
        .unwrap();
    let tool = runtime.deps().tools.get("run_command").unwrap();
    let ctx = ToolRunCtx {
        session_id: session,
        permission_granted: true,
        op_id,
        identity,
        cancellation: CancellationToken::new(),
        artifacts: Arc::new(ToolArtifactSink::Null),
        tool_call_mode: ToolCallMode::Native,
        workspace: Some(Arc::new(ws_handle)),
        edit: None,
        snapshots: runtime.deps().snapshots.clone(),
        sandbox: None,
        supervisor: runtime.deps().supervisor.clone(),
        deadline_ms: 0,
    };
    let shell_changes: Arc<std::sync::Mutex<HashMap<OpId, ShellChangeSet>>> =
        Arc::new(std::sync::Mutex::new(HashMap::new()));
    runtime
        .execute_shell_with_manifest(&handle, tool, ctx, args, turn_op, &shell_changes)
        .await
        .unwrap();
    // The mutation really happened and the row is still RUNNING: the crash
    // window.
    assert!(root.join("crash_new.txt").exists());
    assert!(!root.join("crash_gone.txt").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("crash_mod.txt")).unwrap(),
        "onetwo"
    );
    let pre = handle.tool_runs_with_pre_manifest(8).unwrap();
    assert_eq!(pre.len(), 1);
    assert_eq!(pre[0].pre_manifest.as_ref().unwrap()["state"], "captured");

    // The process dies: drop every in-process owner and reopen the durable
    // world.
    drop(shell_changes);
    drop(runtime);
    drop(handle);
    let manager2 =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let handle2 = manager2.get_session(session).unwrap().unwrap();
    let runtime2 = AgentRuntime::new(deps_on_manager(manager2.clone(), vec![])).unwrap();
    runtime2.recover().unwrap();

    // Restart discovered the ACTUAL changed tree from the durable
    // pre-manifest — never "unchanged".
    let rows = handle2.tool_runs_with_pre_manifest(8).unwrap();
    let row = rows
        .iter()
        .find(|row| row.op_id == op_id)
        .expect("the shell row survives the crash");
    let envelope = row.pre_manifest.as_ref().unwrap();
    assert_eq!(envelope["state"], "discovered");
    assert_eq!(envelope["discovered_at_restart"], true);
    let mut discovered: Vec<String> = envelope["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|change| {
            format!(
                "{} {}",
                change["status"].as_str().unwrap(),
                change["path"].as_str().unwrap()
            )
        })
        .collect();
    discovered.sort();
    assert_eq!(
        discovered,
        vec![
            "added crash_new.txt".to_string(),
            "deleted crash_gone.txt".to_string(),
            "modified crash_mod.txt".to_string(),
        ]
    );

    // The small durable fact marks the state unresolved, and the completion
    // gate refuses certification with the typed reason. The crashed turn can
    // never report success.
    let (paths, unresolved) = shell_change_facts(&handle2, turn_op);
    assert!(unresolved, "the crashed shell state stays unresolved");
    assert_eq!(paths.len(), 3);
    assert_eq!(handle2.state().unwrap(), AgentState::FailedRecoverable);
    let refused = refuse_unattributed_gate(
        Some(CompletionGate::VerifiedComplete),
        "crash-after-shell reconciliation",
    );
    match refused {
        Some(CompletionGate::BlockedVerification { reasons }) => {
            assert_eq!(
                reasons[0].code,
                faktor_core::state::ReasonCode::UnattributedChange
            );
        }
        other => panic!("expected a typed blocked gate, got {other:?}"),
    }
    // The durable pre-manifest CAS blob survives the crash and still decodes
    // to the canonical before-tree, so a later verifier can reconstruct the
    // exact pre-shell state.
    let cas_hash = envelope["cas"].as_str().expect("pre-manifest CAS ref");
    let before_tree = load_pre_manifest(&cas, cas_hash).unwrap();
    let mut before_paths: Vec<&str> = before_tree
        .entries()
        .iter()
        .map(|e| e.normalized_path.as_str())
        .collect();
    before_paths.sort();
    assert!(before_paths.contains(&"crash_gone.txt"));
    assert!(before_paths.contains(&"crash_mod.txt"));
    assert!(!before_paths.contains(&"crash_new.txt"));
}

/// The typed gate downgrade keeps existing reasons and never fabricates a
/// pass.
#[test]
fn unattributed_gate_downgrade_is_typed_and_additive() {
    let existing = CompletionGate::BlockedVerification {
        reasons: vec![OutcomeReason::new(
            ReasonCode::ReviewBlocked,
            "review says no",
        )],
    };
    match refuse_unattributed_gate(Some(existing), "no pre-manifest") {
        Some(CompletionGate::BlockedVerification { reasons }) => {
            assert_eq!(reasons.len(), 2);
            assert_eq!(reasons[1].code, ReasonCode::UnattributedChange);
        }
        other => panic!("expected blocked, got {other:?}"),
    }
    match refuse_unattributed_gate(None, "no pre-manifest") {
        Some(CompletionGate::BlockedVerification { reasons }) => {
            assert_eq!(reasons[0].code, ReasonCode::UnattributedChange);
        }
        other => panic!("expected blocked, got {other:?}"),
    }
    assert_eq!(
        refuse_unattributed_gate(Some(CompletionGate::VerificationPending), "x"),
        Some(CompletionGate::VerificationPending)
    );
}

/// The durable `shell_change` facts are the crash-resume input to
/// `finish_logical_turn`: a reconciled turn merges the ACTUAL discovered
/// paths; a captured/discovered/unattributed fact can never read as "no
/// changes".
#[test]
fn shell_change_facts_distinguish_settled_from_unresolved() {
    let env = shell_env(vec![]);
    let manager = env.manager;
    let session = env.session;
    drop(env.deps);
    let handle = manager.get_session(session).unwrap().unwrap();
    let turn = OpId::new(41);
    let tool = OpId::new(42);

    // Unresolved: no paths and a hard refusal signal.
    handle
        .upsert_memory_fact(
            SHELL_CHANGE_FACT_KIND,
            &fact_key(turn, tool),
            &serde_json::json!({
                "state": "captured",
                "turn_op": turn.raw(),
                "tool_op": tool.raw(),
                "changed": 0,
                "paths": [],
            })
            .to_string(),
        )
        .unwrap();
    let (paths, unresolved) = shell_change_facts(&handle, turn);
    assert!(paths.is_empty());
    assert!(unresolved);

    // A different turn's unresolved fact never leaks into this turn.
    let (_, other_unresolved) = shell_change_facts(&handle, OpId::new(99));
    assert!(!other_unresolved);

    // Reconciled: the actual paths merge and the refusal signal clears.
    handle
        .upsert_memory_fact(
            SHELL_CHANGE_FACT_KIND,
            &fact_key(turn, tool),
            &serde_json::json!({
                "state": "reconciled",
                "turn_op": turn.raw(),
                "tool_op": tool.raw(),
                "changed": 2,
                "paths": ["src/a.rs", "src/b.rs"],
            })
            .to_string(),
        )
        .unwrap();
    let (paths, unresolved) = shell_change_facts(&handle, turn);
    assert!(!unresolved);
    assert_eq!(paths, vec!["src/a.rs".to_string(), "src/b.rs".to_string()]);
}
