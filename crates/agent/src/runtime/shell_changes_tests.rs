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
use faktor_fs::WorkspaceHandle;
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
    shell_env_with_tool(script, shell_tool())
}

fn shell_env_with_tool(script: Vec<ScriptedResponse>, tool: Tool) -> ShellEnv {
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
    let deps = deps_on_manager_with_tool(manager.clone(), script, tool);
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
    deps_on_manager_with_tool(manager, script, shell_tool())
}

fn deps_on_manager_with_tool(
    manager: Arc<SessionManager>,
    script: Vec<ScriptedResponse>,
    tool: Tool,
) -> AgentDeps {
    let cas = manager.cas();
    let mut registry = ProviderRegistry::new();
    registry
        .try_register(Arc::new(scripted_provider(script)))
        .unwrap();
    let mut tool_registry = ToolRegistry::new();
    tool_registry.register(tool);
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

/// The same tool call with `read_only: true` (P1-8).
fn read_only_call(command: &str) -> ScriptedResponse {
    ScriptedResponse::ToolCall {
        id: "shell-1".into(),
        name: "run_command".into(),
        input: serde_json::json!({ "command": command, "read_only": true }),
    }
}

/// The read-only twin of [`shell_tool`]: mirrors the production run_command
/// shape — kernel write denial for the workspace, writable build/scratch
/// roots only, fail-closed Required.
fn read_only_shell_tool() -> Tool {
    let mut tool = shell_tool();
    tool.input_schema = serde_json::json!({
        "type": "object",
        "properties": {
            "command": { "type": "string" },
            "read_only": { "type": "boolean" }
        },
        "required": ["command"]
    });
    tool.execute = Arc::new(|ctx, args| {
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
            let read_only = args
                .get("read_only")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let mut writable_roots = Vec::new();
            for rel in ["target", ".faktor-derived/build-scratch"] {
                let path = ws.root().join(rel);
                ws.create_dir_all_no_follow(std::path::Path::new(rel))?;
                writable_roots.push(path);
            }
            let filesystem_isolation = if read_only {
                faktor_terminal::FilesystemIsolation::WorkspaceReadOnly {
                    roots: vec![ws.root().to_path_buf()],
                    writable_roots,
                    required: true,
                }
            } else {
                faktor_terminal::FilesystemIsolation::Inherit
            };
            let cfg = SpawnConfig {
                cmd: "sh".into(),
                args: vec!["-c".into(), command.to_string()],
                cwd: ws.root().to_path_buf(),
                env: EnvSpec::toolchain(),
                owner: ProcessOwner::Session(ctx.session_id),
                capture: true,
                network_isolation: NetworkIsolation::Inherit,
                filesystem_isolation,
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
    });
    tool
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

/// P1-SHELL: VCS metadata and Faktor tool state live in trees the canonical
/// manifest skips, but their AUTHORITY subset must still be attributed: a
/// shell that rewrites .git/HEAD (branch), the index, refs or .faktor state
/// produces real change rows even though no manifest entry moved.
#[test]
fn authority_state_mutations_are_attributed_even_in_ignored_trees() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    std::fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
    std::fs::create_dir_all(root.join(".faktor")).unwrap();
    std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(root.join(".git/index"), b"index-v1").unwrap();
    std::fs::write(
        root.join(".git/refs/heads/main"),
        "1111111111111111111111111111111111111111\n",
    )
    .unwrap();
    std::fs::write(root.join(".faktor/state.json"), b"{\"phase\":1}").unwrap();
    let ws =
        WorkspaceHandle::open_scoped(faktor_core::id::WorkspaceId::new(1), root.clone()).unwrap();
    let before = capture_authority_state(&ws);
    assert!(before.contains_key(".git/HEAD"));
    assert!(before.contains_key(".git/index"));
    assert!(before.contains_key(".git/refs/heads/main"));
    assert!(before.contains_key(".faktor/state.json"));

    // A shell rewrites the branch, stages something (index changed), adds a
    // ref, and mutates Faktor state; also creates an ignored-tree file that
    // is NOT authority-relevant and must not appear.
    std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/evil\n").unwrap();
    std::fs::write(root.join(".git/index"), b"index-v2-longer").unwrap();
    std::fs::write(
        root.join(".git/refs/heads/evil"),
        "2222222222222222222222222222222222222222\n",
    )
    .unwrap();
    std::fs::write(root.join(".faktor/state.json"), b"{\"phase\":2}").unwrap();
    std::fs::write(root.join(".git/COMMIT_EDITMSG"), b"staged\n").unwrap();

    let after = capture_authority_state(&ws);
    let changes = diff_authority(&before, &after);
    let find = |path: &str| changes.iter().find(|change| change.path == path);
    assert_eq!(
        find(".git/HEAD").expect("HEAD changed").status,
        ShellChangeStatus::Modified
    );
    assert_eq!(
        find(".git/index").expect("index changed").status,
        ShellChangeStatus::Modified
    );
    assert_eq!(
        find(".git/refs/heads/evil").expect("new ref").status,
        ShellChangeStatus::Added
    );
    assert_eq!(
        find(".faktor/state.json").expect("state changed").status,
        ShellChangeStatus::Modified
    );
    assert_eq!(
        find(".git/COMMIT_EDITMSG").expect("appeared").status,
        ShellChangeStatus::Added
    );

    // Deletion is attributed too.
    std::fs::remove_file(root.join(".git/refs/heads/evil")).unwrap();
    let after_delete = capture_authority_state(&ws);
    let changes = diff_authority(&after, &after_delete);
    assert_eq!(
        changes
            .iter()
            .find(|change| change.path == ".git/refs/heads/evil")
            .expect("ref deleted")
            .status,
        ShellChangeStatus::Deleted
    );
}

/// The authority map skips build/dependency trees by the documented contract
/// (derived, regenerable state), and a symlinked authority file is recorded
/// as `symlink`, never followed.
#[test]
fn authority_probe_skips_derived_trees_and_never_follows_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
    std::fs::create_dir_all(root.join("target/debug")).unwrap();
    std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(root.join("node_modules/pkg/x.js"), b"ignored").unwrap();
    std::fs::write(root.join("target/debug/x"), b"ignored").unwrap();
    std::os::unix::fs::symlink(root.join(".git/HEAD"), root.join(".faktor-link")).ok();
    let ws =
        WorkspaceHandle::open_scoped(faktor_core::id::WorkspaceId::new(2), root.clone()).unwrap();
    let state = capture_authority_state(&ws);
    assert!(state.contains_key(".git/HEAD"));
    assert!(!state.keys().any(|key| key.starts_with("node_modules/")));
    assert!(!state.keys().any(|key| key.starts_with("target/")));
    // A .faktor directory entry that is a symlink is recorded as a marker,
    // never dereferenced.
    std::fs::create_dir_all(root.join(".faktor")).unwrap();
    std::fs::write(root.join(".faktor/real.json"), b"x").unwrap();
    std::fs::remove_file(root.join(".faktor-link")).ok();
    std::os::unix::fs::symlink(root.join(".git/HEAD"), root.join(".faktor/link")).unwrap();
    let state = capture_authority_state(&ws);
    assert_eq!(
        state.get(".faktor/link").map(String::as_str),
        Some("symlink")
    );
    assert!(state.contains_key(".faktor/real.json"));
}

/// (P1-SHELL) A LINKED worktree's `.git` is a file; git authority lives in
/// the resolved `gitdir` and in the shared `commondir` (refs, packed-refs).
/// Every transition — add, reset, switch, commit, update-ref — must appear in
/// the attributed change set.
#[test]
fn linked_worktree_git_authority_transitions_are_attributed() {
    use std::process::Command;
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main");
    let ws_path = dir.path().join("wt");
    std::fs::create_dir_all(&main).unwrap();
    let git = |cwd: &std::path::Path, args: &[&str]| {
        let out = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&main, &["init", "-q", "-b", "main"]);
    std::fs::write(main.join("seed.txt"), "seed").unwrap();
    git(&main, &["add", "seed.txt"]);
    git(&main, &["commit", "-q", "-m", "seed"]);
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "wt",
            ws_path.to_str().unwrap(),
        ],
    );
    assert!(
        ws_path.join(".git").is_file(),
        "linked worktree has a .git file"
    );

    let handle =
        WorkspaceHandle::open_scoped(faktor_core::id::WorkspaceId::new(11), ws_path.clone())
            .unwrap();
    let before = capture_authority_state(&handle);
    assert!(
        before.contains_key(".git"),
        "the .git pointer file is itself authority"
    );
    assert!(
        before.contains_key("gitdir/HEAD"),
        "{:?}",
        before.keys().take(6).collect::<Vec<_>>()
    );
    assert!(before.contains_key("gitdir/index"));
    assert!(
        before.keys().any(|key| key.starts_with("commondir/refs/")),
        "common refs are fingerprinted: {:?}",
        before.keys().take(8).collect::<Vec<_>>()
    );

    // git add: the worktree index (external gitdir) changes.
    std::fs::write(ws_path.join("a.txt"), "1").unwrap();
    git(&ws_path, &["add", "a.txt"]);
    let staged = capture_authority_state(&handle);
    let changes = diff_authority(&before, &staged);
    assert!(
        changes.iter().any(|c| c.path == "gitdir/index"),
        "git add must be attributed: {:?}",
        changes.iter().map(|c| &c.path).collect::<Vec<_>>()
    );

    // git reset: index changes again (back to HEAD's tree).
    git(&ws_path, &["reset", "-q"]);
    let reset = capture_authority_state(&handle);
    let changes = diff_authority(&staged, &reset);
    assert!(
        changes.iter().any(|c| c.path == "gitdir/index"),
        "{:?}",
        changes.iter().map(|c| &c.path).collect::<Vec<_>>()
    );

    // git switch -c: HEAD (gitdir) and a new ref (commondir) change.
    git(&ws_path, &["switch", "-q", "-c", "feature"]);
    let switched = capture_authority_state(&handle);
    let changes = diff_authority(&reset, &switched);
    assert!(
        changes.iter().any(|c| c.path == "gitdir/HEAD"),
        "{:?}",
        changes.iter().map(|c| &c.path).collect::<Vec<_>>()
    );
    assert!(
        changes
            .iter()
            .any(|c| c.path == "commondir/refs/heads/feature"),
        "{:?}",
        changes.iter().map(|c| &c.path).collect::<Vec<_>>()
    );

    // git commit: HEAD, index and the branch ref all move.
    std::fs::write(ws_path.join("b.txt"), "2").unwrap();
    git(&ws_path, &["add", "b.txt"]);
    git(&ws_path, &["commit", "-q", "-m", "b"]);
    let committed = capture_authority_state(&handle);
    let changes = diff_authority(&switched, &committed);
    for path in [
        "gitdir/index",
        "gitdir/COMMIT_EDITMSG",
        "commondir/refs/heads/feature",
    ] {
        assert!(
            changes.iter().any(|c| c.path == path),
            "git commit must attribute {path}: {:?}",
            changes.iter().map(|c| &c.path).collect::<Vec<_>>()
        );
    }

    // git update-ref: a brand-new common ref appears.
    git(&ws_path, &["update-ref", "refs/heads/extra", "HEAD"]);
    let refd = capture_authority_state(&handle);
    let changes = diff_authority(&committed, &refd);
    assert!(
        changes
            .iter()
            .any(|c| c.path == "commondir/refs/heads/extra"),
        "{:?}",
        changes.iter().map(|c| &c.path).collect::<Vec<_>>()
    );
}

/// Copy a directory tree (test fixture restoration for linked worktrees).
fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// (P1-SHELL) The same transitions through the REAL tool loop: a shell that
/// stages, branches and creates a ref must attribute the external gitdir and
/// commondir objects in the durable FileChanged accounting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn linked_worktree_transitions_reach_shell_settlement() {
    use std::process::Command;
    let script = vec![
        tool_call(
            "git add src/lib.rs && git update-ref refs/heads/extra HEAD && git switch -q -c feature",
        ),
        ScriptedResponse::Text("done".into()),
        ScriptedResponse::End,
    ];
    let env = shell_env(script);
    // Turn the harness workspace into a LINKED worktree: the root must be
    // empty for `git worktree add`, so move its contents aside and restore
    // them after the worktree exists.
    let staging = env.dir.path().join("staging");
    std::fs::rename(&env.root, &staging).unwrap();
    let main = env.dir.path().join("main");
    std::fs::create_dir_all(&main).unwrap();
    let git = |cwd: &std::path::Path, args: &[&str]| {
        let out = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&main, &["init", "-q", "-b", "main"]);
    std::fs::write(main.join("seed.txt"), "seed").unwrap();
    git(&main, &["add", "seed.txt"]);
    git(&main, &["commit", "-q", "-m", "seed"]);
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "wt",
            env.root.to_str().unwrap(),
        ],
    );
    copy_tree(&staging, &env.root);

    let manager = env.manager.clone();
    let session = env.session;
    let runtime = AgentRuntime::new(env.deps).unwrap();
    let outcome = runtime
        .run_turn(session, "git transitions through the shell", &[])
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    let handle = manager.get_session(session).unwrap().unwrap();
    let (rows, _truncated) = changed_rows(&handle);
    assert!(
        rows.iter().any(|row| row == "modified gitdir/index"),
        "staging must be attributed: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|row| row == "added commondir/refs/heads/extra"),
        "update-ref must be attributed: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|row| row == "added commondir/refs/heads/feature"),
        "branch creation must be attributed: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row == "modified gitdir/HEAD"),
        "HEAD move must be attributed: {rows:?}"
    );
}

/// P1-8: a read-only shell runs under KERNEL write denial. A workspace write
/// fails at the syscall (the file never appears), a build-root write
/// succeeds, and the execution generates NO whole-workspace change
/// accounting because it has no write authority.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_shell_has_kernel_write_denial_and_no_change_manifest() {
    let script = vec![
        read_only_call(
            "echo bad > src/blocked.rs; echo ok > target/scratch.txt; echo bad > root-escape.txt; true",
        ),
        ScriptedResponse::Text("done".into()),
        ScriptedResponse::End,
    ];
    let env = shell_env_with_tool(script, read_only_shell_tool());
    let manager = env.manager.clone();
    let session = env.session;
    let root = env.root.clone();
    let runtime = AgentRuntime::new(env.deps).unwrap();
    let outcome = runtime
        .run_turn(session, "read-only verification shell", &[])
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert!(
        !root.join("src/blocked.rs").exists(),
        "the kernel must deny the workspace write (file must never exist)"
    );
    assert!(
        !root.join("root-escape.txt").exists(),
        "a workspace-root write is denied too"
    );
    assert!(
        root.join("target/scratch.txt").exists(),
        "the explicit build root stays writable"
    );
    let handle = manager.get_session(session).unwrap().unwrap();
    let (rows, _truncated) = changed_rows(&handle);
    assert!(
        rows.is_empty(),
        "a read-only shell executes with NO change accounting: {rows:?}"
    );
}

/// P1-8: read-only shell ownership is reads-only, so a path-constrained
/// ChangeBudget never refuses it and writers do not serialize behind it.
#[test]
fn read_only_shell_takes_no_write_ownership() {
    let (reads, writes) = ownership_sets_for(
        Some(&Capability::ExecuteShell {
            command: String::new(),
        }),
        crate::tool::Ownership {
            reads: Vec::new(),
            writes: Vec::new(),
        },
        None,
        true,
    );
    assert_eq!(reads.entries(), &["**".to_string()]);
    assert!(
        writes.is_empty(),
        "no write ownership for a read-only shell"
    );
    let (_reads, writes) = ownership_sets_for(
        Some(&Capability::ExecuteShell {
            command: String::new(),
        }),
        crate::tool::Ownership::default(),
        None,
        false,
    );
    assert_eq!(writes.entries(), &["**".to_string()]);
}

/// P0: the read-only tool's writable-root provisioning goes through the
/// ROOTED authority. A hostile `.faktor-derived` symlink to an outside dir
/// must produce a typed preparation refusal BEFORE the process exists, and
/// the daemon must never mkdir outside the workspace.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_shell_refuses_hostile_derived_symlink_before_exec() {
    let script = vec![
        read_only_call("echo pwned > target/pwned.txt"),
        ScriptedResponse::Text("done".into()),
        ScriptedResponse::End,
    ];
    let env = shell_env_with_tool(script, read_only_shell_tool());
    let manager = env.manager.clone();
    let session = env.session;
    let root = env.root.clone();
    let outside = env.dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, root.join(".faktor-derived")).unwrap();
    let runtime = AgentRuntime::new(env.deps).unwrap();
    let _outcome = runtime
        .run_turn(session, "hostile derived symlink", &[])
        .await
        .unwrap();
    assert!(
        !outside.join("build-scratch").exists(),
        "the daemon must never create the scratch root outside the workspace"
    );
    assert!(
        !root.join("target/pwned.txt").exists(),
        "the command must never run after a failed preparation"
    );
    let handle = manager.get_session(session).unwrap().unwrap();
    let (rows, _truncated) = changed_rows(&handle);
    assert!(
        rows.is_empty(),
        "no execution, no change accounting: {rows:?}"
    );
}

/// P1: every path the read-only mode may write is DERIVED, non-authority
/// state under every authority that looks at it: the shell manifest skips
/// `.faktor-derived`, and the authority scanner (which tracks `.faktor/**`)
/// never reports it.
#[test]
fn read_only_writable_roots_are_derived_in_every_authority() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    std::fs::create_dir_all(&root).unwrap();
    let handle =
        WorkspaceHandle::open_scoped(faktor_core::id::WorkspaceId::new(401), root.clone()).unwrap();
    std::fs::create_dir_all(handle.root().join(".faktor-derived/build-scratch/nested")).unwrap();
    std::fs::write(
        handle
            .root()
            .join(".faktor-derived/build-scratch/nested/out.o"),
        b"derived",
    )
    .unwrap();
    std::fs::create_dir_all(handle.root().join(".faktor")).unwrap();
    std::fs::write(handle.root().join(".faktor/state.json"), b"authority").unwrap();
    let capture = capture_pre_shell(&handle, None).unwrap();
    assert!(
        capture
            .manifest
            .entries()
            .iter()
            .all(|entry| !entry.normalized_path.starts_with(".faktor-derived")),
        "derived build scratch must never enter the shell manifest"
    );
    let authority = capture_authority_state(&handle);
    assert!(
        authority
            .keys()
            .all(|key| !key.starts_with(".faktor-derived")),
        "derived build scratch must never be authority state: {:?}",
        authority.keys().collect::<Vec<_>>()
    );
    assert!(
        authority.keys().any(|key| key.starts_with(".faktor/")),
        "the real .faktor state stays authority: {:?}",
        authority.keys().collect::<Vec<_>>()
    );
    drop(dir);
}

/// P1: `.git` is workspace-controlled text. Hostile gitdir/commondir claims
/// must be refused WITHOUT any authority keys, and the obvious host paths
/// (`/etc`, `/home`, arbitrary `..` traversals) are refused LEXICALLY before
/// any stat/open of the claimed location.
#[test]
fn hostile_git_pointers_are_refused_without_authority_keys() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    std::fs::create_dir_all(&root).unwrap();
    let handle =
        WorkspaceHandle::open_scoped(faktor_core::id::WorkspaceId::new(402), root.clone()).unwrap();

    // The obvious hostile claims: refused lexically (no reads possible).
    for raw in ["/etc", "/home", "../../../outside-etc", "/var/lib"] {
        std::fs::write(root.join(".git"), format!("gitdir: {raw}\n")).unwrap();
        let state = capture_authority_state(&handle);
        assert!(
            state.is_empty(),
            "hostile gitdir {raw:?} must be refused with zero authority keys, got {:?}",
            state.keys().collect::<Vec<_>>()
        );
    }

    // Shape-valid but unbound: a foreign `worktrees/<name>` whose reciprocal
    // `gitdir` file does not point back at THIS workspace is refused.
    let foreign = dir.path().join("foreign/worktrees/evil");
    std::fs::create_dir_all(&foreign).unwrap();
    std::fs::write(foreign.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(foreign.join("gitdir"), "/somewhere/else/.git\n").unwrap();
    std::fs::write(
        root.join(".git"),
        format!("gitdir: {}\n", foreign.display()),
    )
    .unwrap();
    let state = capture_authority_state(&handle);
    assert!(
        state.is_empty(),
        "an unbound external gitdir must never be fingerprinted: {:?}",
        state.keys().collect::<Vec<_>>()
    );

    // Bound gitdir but a commondir traversal outside the git layout: the
    // lexical ancestor guard refuses before any stat of `/home`.
    let bound = dir.path().join("common/worktrees/evil");
    std::fs::create_dir_all(&bound).unwrap();
    std::fs::write(bound.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(bound.join("index"), b"i").unwrap();
    std::fs::write(
        bound.join("gitdir"),
        format!("{}\n", root.join(".git").display()),
    )
    .unwrap();
    std::fs::write(bound.join("commondir"), "/home\n").unwrap();
    std::fs::write(root.join(".git"), format!("gitdir: {}\n", bound.display())).unwrap();
    let state = capture_authority_state(&handle);
    assert!(
        state.is_empty(),
        "a commondir traversal outside the git layout must be refused: {:?}",
        state.keys().collect::<Vec<_>>()
    );

    // The same fixture with the REAL common dir relation (../..) is bound and
    // fingerprinted: the guards only reject foreign layouts.
    std::fs::write(bound.join("commondir"), "../..\n").unwrap();
    let state = capture_authority_state(&handle);
    assert!(
        state.contains_key("gitdir/HEAD"),
        "a genuinely bound worktree layout is fingerprinted: {:?}",
        state.keys().collect::<Vec<_>>()
    );
}
