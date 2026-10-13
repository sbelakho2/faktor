//! Soak tests (spec §38): synthetic long-running sessions — no state loss,
//! no runaway memory, no compaction loop, no duplicate effects. The 12-hour
//! scale is CI-gated (`#[ignore]`); the fast variant runs in seconds and
//! exercises the same invariants.

#![cfg_attr(
    not(test),
    allow(dead_code, unused_imports, unused_variables, unused_mut)
)] // test-harness crate: the lib view exists only for clippy
use std::sync::Arc;
use std::time::Duration;

use faktor_agent::{
    AgentDeps, AgentRuntime, NoEvidence, PermissionRequester, Tool, ToolOutcome, ToolRegistry,
};
use faktor_core::capability::PermissionDecision;
use faktor_core::id::SessionId;
use faktor_core::model::ModelCapabilities;
use faktor_core::state::AgentState;
use faktor_core::time::SystemClock;
use faktor_provider::{FakeProvider, ProviderRegistry, ScriptedResponse};
use faktor_session::SessionManager;
use tempfile::tempdir;

struct AlwaysAllow;
impl PermissionRequester for AlwaysAllow {
    fn request(
        &self,
        _s: SessionId,
        _p: &faktor_session::ops::PermissionRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = faktor_core::Result<PermissionDecision>> + Send>,
    > {
        Box::pin(async { Ok(PermissionDecision::Allow) })
    }
}

fn agent_for(session: Arc<SessionManager>, tools: bool, compact_at: f64) -> Arc<AgentRuntime> {
    let mut registry = ProviderRegistry::new();
    registry
        .try_register(Arc::new(FakeProvider::with_script(
            "fake",
            ModelCapabilities {
                tools: true,
                ..Default::default()
            },
            vec![ScriptedResponse::Text("ok".into()), ScriptedResponse::End],
        )))
        .unwrap();
    let mut tool_registry = ToolRegistry::new();
    if tools {
        tool_registry.register(Tool {
            name: "echo".into(),
            description: "d".into(),
            input_schema: serde_json::json!({}),
            resource_class: faktor_core::resource::ResourceClass::Cpu,
            capability: None,
            recovery_hint: faktor_agent::RecoveryHint::Idempotent,
            path_args: vec![],
            execute: Arc::new(|_ctx, args| {
                Box::pin(async move {
                    Ok(ToolOutcome {
                        text: format!("echo: {args}"),
                        exit_code: Some(0),
                        ..Default::default()
                    })
                })
            }),
        });
    }
    AgentRuntime::new(AgentDeps {
        session,
        providers: Arc::new(registry),
        chunk_sink: None,
        permission_requester: Arc::new(AlwaysAllow),
        evidence: Arc::new(NoEvidence),
        tools: Arc::new(tool_registry),
        cas: None,
        workspaces: faktor_fs::WorkspaceFileService::new(),
        edit: None,
        snapshots: None,
        sandbox: None,
        supervisor: None,
        verification: faktor_agent::VerificationService::disabled(),
        hooks: None,
        instructions_resolver: faktor_instructions::no_roots_resolver(),
        routing: faktor_agent::FixedRoutingPolicy::passthrough(),
        budgets: Arc::new(faktor_session::NoopBudget),
        model: "m".into(),
        compaction_model: None,
        compact_at_usage: compact_at,
        instructions: "i".into(),
        clock: Arc::new(SystemClock),
        tool_call_mode: faktor_agent::ToolCallMode::Native,
        tool_deadline_ms: 2000,
        retry_policy: faktor_core::retry::RetryPolicy::default(),
        semantic: faktor_agent::fallback_semantic_registry(),
        context_prior: None,
        secret_registry: None,
        efficiency: Default::default(),
    })
    .unwrap()
}

/// Fast soak: 500 turns with tool calls, 3 daemon restarts, compaction
/// pressure — invariants: no state loss, no double effects, no loop.
#[tokio::test]
async fn fast_soak_500_turns_3_restarts_no_loss() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let session = SessionManager::open(root.join("store"), root.join("cas"), true).unwrap();
    let ws = session.create_workspace("/w").unwrap();
    let row = session.create_session(ws, "soak", "fake", "m").unwrap();

    let mut turns_done = 0u64;
    let per_round = 500 / 3;
    let remainder = 500 - per_round * 2;
    for round in 0..3 {
        let agent = agent_for(session.clone(), true, 0.02);
        let count = if round < 2 { per_round } else { remainder };
        for i in 0..count {
            let script = vec![
                ScriptedResponse::ToolCall {
                    id: format!("c{i}"),
                    name: "echo".into(),
                    input: serde_json::json!({"round": round, "i": i}),
                },
                ScriptedResponse::Text(format!("round {round} turn {i} {}", "x".repeat(300))),
                ScriptedResponse::End,
            ];
            let mut registry = ProviderRegistry::new();
            registry
                .try_register(Arc::new(FakeProvider::with_script(
                    "fake",
                    ModelCapabilities {
                        tools: true,
                        ..Default::default()
                    },
                    script,
                )))
                .unwrap();
            let mut deps = agent_deps(session.clone());
            deps.compact_at_usage = 0.02;
            deps.providers = Arc::new(registry);
            deps.tools = Arc::new(with_echo_tools());
            let turn_agent = AgentRuntime::new(deps).unwrap();
            let outcome = turn_agent
                .run_turn(row.id(), &format!("turn {i}"), &[])
                .await
                .unwrap();
            assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
            turns_done += 1;
        }
        drop(agent);
        // DAEMON RESTART (round boundary): recovery finds nothing pending
        // and the session is coherent.
        let session2 = SessionManager::open(root.join("store"), root.join("cas"), true).unwrap();
        let reports = session2.recover_all_sessions().unwrap();
        let pending: usize = reports.iter().map(|r| r.crashed_ops.len()).sum();
        assert_eq!(
            pending, 0,
            "no unfinished ops at restart {round}: {reports:?}"
        );
        let handle = session2.get_session(row.id()).unwrap().unwrap();
        let page = handle.messages_page(None, 5).unwrap();
        assert!(page.messages.len() <= 5);
        assert!(page.has_more);
        drop(session2);
    }

    // Final integrity: journal gapless, no corruption, counts coherent.
    let session_final = SessionManager::open(root.join("store"), root.join("cas"), true).unwrap();
    let handle = session_final.get_session(row.id()).unwrap().unwrap();
    let replay = handle.replay_journal().unwrap();
    assert_eq!(
        replay.event_count,
        replay.last_seq.raw(),
        "journal must be gapless"
    );
    assert_eq!(turns_done, 500);
    // No duplicate effects: every tool ran exactly once per turn (pending
    // runs are all resolved).
    assert!(handle.pending_tool_runs().unwrap().is_empty());
}

fn with_echo_tools() -> ToolRegistry {
    let mut tool_registry = ToolRegistry::new();
    tool_registry.register(Tool {
        name: "echo".into(),
        description: "d".into(),
        input_schema: serde_json::json!({}),
        resource_class: faktor_core::resource::ResourceClass::Cpu,
        capability: None,
        recovery_hint: faktor_agent::RecoveryHint::Idempotent,
        path_args: vec![],
        execute: Arc::new(|_ctx, args| {
            Box::pin(async move {
                Ok(ToolOutcome {
                    text: format!("echo: {args}"),
                    exit_code: Some(0),
                    ..Default::default()
                })
            })
        }),
    });
    tool_registry
}

fn agent_deps(session: Arc<SessionManager>) -> AgentDeps {
    let mut registry = ProviderRegistry::new();
    registry
        .try_register(Arc::new(FakeProvider::with_script(
            "fake",
            ModelCapabilities {
                tools: true,
                ..Default::default()
            },
            vec![ScriptedResponse::End],
        )))
        .unwrap();
    AgentDeps {
        session,
        providers: Arc::new(registry),
        chunk_sink: None,
        permission_requester: Arc::new(AlwaysAllow),
        evidence: Arc::new(NoEvidence),
        tools: Arc::new(ToolRegistry::new()),
        cas: None,
        workspaces: faktor_fs::WorkspaceFileService::new(),
        edit: None,
        snapshots: None,
        sandbox: None,
        supervisor: None,
        verification: faktor_agent::VerificationService::disabled(),
        hooks: None,
        instructions_resolver: faktor_instructions::no_roots_resolver(),
        routing: faktor_agent::FixedRoutingPolicy::passthrough(),
        budgets: Arc::new(faktor_session::NoopBudget),
        model: "m".into(),
        compaction_model: None,
        compact_at_usage: 0.65,
        instructions: "i".into(),
        clock: Arc::new(SystemClock),
        tool_call_mode: faktor_agent::ToolCallMode::Native,
        tool_deadline_ms: 2000,
        retry_policy: faktor_core::retry::RetryPolicy::default(),
        semantic: faktor_agent::fallback_semantic_registry(),
        context_prior: None,
        secret_registry: None,
        efficiency: Default::default(),
    }
}

/// Compaction cannot loop even under pathological summarizer pressure:
/// repeated compactions converge to a bounded context.
#[tokio::test]
async fn compaction_converges_under_pressure() {
    let idx = faktor_context::Compactor::deterministic_only();
    let mut history: Vec<faktor_context::RecentTurn> = (0..500)
        .map(|i| faktor_context::RecentTurn {
            role: "assistant".into(),
            text: format!("turn {i} {}", "z".repeat(500)),
        })
        .collect();
    let ledger = faktor_context::TaskLedger {
        goal: "soak".into(),
        ..Default::default()
    };
    let target = 20_000usize;
    let mut steps = 0usize;
    loop {
        let before: usize = history.iter().map(|t| t.text.len()).sum::<usize>() / 4;
        let req = faktor_context::CompactionRequest::new(before, target);
        let plan = idx.compact(&history, &ledger, &req).await;
        assert!(plan.accepted, "deterministic pruning must always accept");
        assert!(plan.after_tokens <= req.hard_cap(), "hard invariant");
        if idx.would_compact_again(&plan, &req) {
            history = plan.kept_recent.clone();
            steps += 1;
            assert!(steps < 20, "convergence bounded");
            continue;
        }
        break;
    }
    // Convergence is the invariant; the number of rounds depends on the
    // budget math (the liar-summarizer death-spiral is covered in
    // faktor-context's unit tests).
    let _ = steps;
}

/// The 12-hour scale soak (spec §38): 10k ops, restarts, UI reconnects.
/// CI-gated.
#[tokio::test]
#[ignore = "[soak] 12-hour synthetic session — run explicitly"]
async fn soak_12h_scale() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let session = SessionManager::open(root.join("store"), root.join("cas"), true).unwrap();
    let ws = session.create_workspace("/w").unwrap();
    let row = session.create_session(ws, "soak-12h", "fake", "m").unwrap();

    for i in 0..10_000u64 {
        let agent = agent_for(session.clone(), i % 10 == 0, 0.01);
        let outcome = agent
            .run_turn(row.id(), &format!("op {i}"), &[])
            .await
            .unwrap();
        assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
        if i % 3_000 == 0 {
            // Daemon restart + UI reconnect.
            let session2 =
                SessionManager::open(root.join("store"), root.join("cas"), true).unwrap();
            let reports = session2.recover_all_sessions().unwrap();
            let pending: usize = reports.iter().map(|r| r.crashed_ops.len()).sum();
            assert_eq!(pending, 0);
            let handle = session2.get_session(row.id()).unwrap().unwrap();
            let _ = handle.latest_messages_page(100).unwrap();
            drop(session2);
        }
        // Memory bound: message count grows but pages stay small.
        if i % 1_000 == 0 {
            let handle = session.get_session(row.id()).unwrap().unwrap();
            assert!(handle.messages_page(None, 100).unwrap().messages.len() <= 100);
        }
    }
    let _ = Duration::from_secs(1);
}

/// UI disconnect/reconnect under an active agent: the agent continues.
#[tokio::test]
async fn ui_disconnect_agent_continues() {
    let dir = tempdir().unwrap();
    let session =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let ws = session.create_workspace("/w").unwrap();
    let row = session.create_session(ws, "ui", "fake", "m").unwrap();
    let agent = agent_for(session.clone(), true, 0.65);
    // Simulate the UI vanishing: submit the prompt and let the agent run to
    // completion (the HTTP layer would spawn this; here it is direct).
    let outcome = agent.run_turn(row.id(), "run", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    // Reconnect: the session state reproduces from the journal.
    let handle = session.get_session(row.id()).unwrap().unwrap();
    let state = handle.state().unwrap();
    assert_eq!(state, AgentState::ReadyForNextTurn);
}

// ---------------------------------------------------------------------------
// Convergence churn: the workload the soak driver samples for QUIESCENT
// CONVERGENCE (never "no crash"). It drives the REAL runtime/store at a paced,
// bounded rate for a driver-set real wall-clock target, emits bounded runtime
// gauges (queues, background tasks, journal/index latency) as JSONL for the
// driver's checker, and performs real daemon restarts + reconnects.
// ---------------------------------------------------------------------------

/// Bounded metrics emitter for `scripts/certification/soak-convergence.py`.
/// Every line is one sample; emits are O(1) appends (never buffered in RAM).
struct ConvergenceEmitter {
    file: Option<std::fs::File>,
    started: std::time::Instant,
}

impl ConvergenceEmitter {
    fn new() -> Self {
        let file = std::env::var("FAKTOR_SOAK_METRICS_FILE")
            .ok()
            .and_then(|path| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .ok()
            });
        Self {
            file,
            started: std::time::Instant::now(),
        }
    }

    fn emit(&mut self, value: serde_json::Value) {
        if let Some(file) = self.file.as_mut() {
            use std::io::Write;
            let _ = writeln!(file, "{value}");
            let _ = file.flush();
        }
    }

    fn gauge(
        &mut self,
        turn: u64,
        writer_queue: u64,
        reader_queue: u64,
        background_tasks: u64,
        journal_us: u64,
        index_us: u64,
    ) {
        self.gauge_marked(
            turn,
            writer_queue,
            reader_queue,
            background_tasks,
            journal_us,
            index_us,
            false,
        );
    }

    /// [`Self::gauge`] with the `quiescent` marker: the checker requires
    /// EVERY marked sample to be zero (the convergence invariant), while
    /// unmarked churn samples are allowed to be busy. Marked samples are
    /// only emitted while the drain condition is actually OBSERVED zero
    /// (see the observable quiescence wait in [`soak_convergence_churn`]),
    /// so the marker is evidence, never a claim.
    #[allow(clippy::too_many_arguments)] // one flat metric row; a param struct would hide the wire shape
    fn gauge_marked(
        &mut self,
        turn: u64,
        writer_queue: u64,
        reader_queue: u64,
        background_tasks: u64,
        journal_us: u64,
        index_us: u64,
        quiescent: bool,
    ) {
        self.emit(serde_json::json!({
            "t": self.started.elapsed().as_millis() as u64,
            "turn": turn,
            "writer_queue": writer_queue,
            "reader_queue": reader_queue,
            "background_tasks": background_tasks,
            "journal_us": journal_us,
            "index_us": index_us,
            "quiescent": quiescent,
        }));
    }

    fn reconnect(&mut self, ok: bool, count: u64) {
        self.emit(serde_json::json!({
            "t": self.started.elapsed().as_millis() as u64,
            "event": "reconnect",
            "ok": ok,
            "reconnects": count,
        }));
    }
}

fn bounded_p95(window: &std::collections::VecDeque<u128>) -> u64 {
    if window.is_empty() {
        return 0;
    }
    let mut values: Vec<u128> = window.iter().copied().collect();
    values.sort_unstable();
    let index = (((values.len() - 1) as f64) * 0.95).round() as usize;
    values[index.min(values.len() - 1)].min(u64::MAX as u128) as u64
}

fn push_bounded(window: &mut std::collections::VecDeque<u128>, value: u128) {
    if window.len() >= 128 {
        window.pop_front();
    }
    window.push_back(value);
}

/// [soak] convergence churn (driver-targeted real wall clock).
///
/// `scripts/soak.sh --mode churn|realtime` runs this test with
/// `FAKTOR_SOAK_TARGET_SECONDS` (the real target), `FAKTOR_SOAK_DATA_DIR`
/// (persistent sampled data dir) and `FAKTOR_SOAK_METRICS_FILE` (the runtime
/// gauge stream). The paced rate keeps durable growth linear and bounded
/// while still exercising WAL, journal and CAS churn; a successful run ends
/// quiescent (drain to zero OBSERVED, then held stable for a window derived
/// from this run's own measured turn cost — see the quiescence wait below).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "[soak] convergence churn (driver-targeted real wall clock) — run via scripts/soak.sh"]
async fn soak_convergence_churn() {
    let target_seconds = std::env::var("FAKTOR_SOAK_TARGET_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(20)
        .clamp(1, 25 * 60 * 60);
    let root = std::env::var("FAKTOR_SOAK_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::temp_dir().join(format!("faktor-soak-convergence-{}", std::process::id()))
        });
    std::fs::create_dir_all(&root).unwrap();
    let store_dir = root.join("store");
    let cas_dir = root.join("cas");

    // Publish this process as the convergence subject: the driver's sampler
    // follows the subject pidfile, so compile/build processes are never
    // mistaken for workload state.
    if let Ok(pidfile) = std::env::var("FAKTOR_SOAK_SUBJECT_PIDFILE") {
        let _ = std::fs::write(pidfile, format!("{}\n", std::process::id()));
    }

    let mut manager = SessionManager::open(&store_dir, &cas_dir, true).unwrap();
    let ws = manager.create_workspace("/soak-convergence").unwrap();
    let mut session_id = manager
        .create_session(ws, "convergence-churn", "fake", "m")
        .unwrap()
        .id();

    // BOUNDED CALIBRATION CONTROL (same workload shape, reduced fixed size).
    // `scripts/soak.sh` invokes this same test through the same bounded
    // harness path with `FAKTOR_SOAK_CALIBRATION_TURNS` set, measures the
    // harness wall it observed, and derives the campaign's load-aware wall
    // budget from this monotonic cost over the documented product nominal
    // (see the soak budget derivation in docs/certification.md §2.15). In
    // calibration mode the wall target does not apply and no gauge/reconnect
    // evidence is emitted: the campaign round owns that evidence.
    if let Some(calibration_turns) = std::env::var("FAKTOR_SOAK_CALIBRATION_TURNS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|turns| *turns > 0)
    {
        let started = std::time::Instant::now();
        for turn in 0..calibration_turns {
            let agent = agent_for(manager.clone(), true, 1.0);
            let outcome = agent
                .run_turn(session_id, &format!("calibration turn {turn}"), &[])
                .await
                .unwrap();
            assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
            drop(agent);
            let handle = manager.get_session(session_id).unwrap().unwrap();
            let _ = handle.latest_messages_page(100).unwrap();
            let _ = handle.messages_page(None, 50).unwrap();
        }
        let elapsed_ms = started.elapsed().as_millis();
        // libtest captures test stderr, so the harness reads the measurement
        // from the env-pointed file; the stderr line stays for manual runs.
        if let Ok(path) = std::env::var("FAKTOR_SOAK_CALIBRATION_OUT") {
            let _ = std::fs::write(
                path,
                format!(
                    "{{\"schema\":\"faktor-soak-calibration/v1\",\"turns\":{calibration_turns},\"elapsed_ms\":{elapsed_ms}}}\n"
                ),
            );
        }
        eprintln!("soak calibration: turns={calibration_turns} elapsed_ms={elapsed_ms}");
        return;
    }

    let mut emitter = ConvergenceEmitter::new();
    let started = std::time::Instant::now();
    let target = Duration::from_secs(target_seconds);
    // Bounded churn rate: the real gate is wall-clock duration, not maximal
    // disk growth. Long targets pace slower so a 12-24h run stays linear.
    let pace = if target_seconds > 3600 {
        Duration::from_millis(200)
    } else if target_seconds > 300 {
        Duration::from_millis(50)
    } else {
        Duration::from_millis(5)
    };
    let gauge_every = if target_seconds > 600 {
        Duration::from_secs(10)
    } else {
        Duration::from_secs(1)
    };
    let reconnect_every = if target_seconds > 600 {
        Duration::from_secs(120)
    } else {
        Duration::from_secs(15)
    };
    let mut turns = 0u64;
    let mut reconnects = 0u64;
    let mut last_gauge = std::time::Instant::now();
    let mut last_reconnect = std::time::Instant::now();
    let mut journal_window: std::collections::VecDeque<u128> = std::collections::VecDeque::new();
    let mut index_window: std::collections::VecDeque<u128> = std::collections::VecDeque::new();
    // Same-run control for the quiescence wait below: the p95 cost of a whole
    // churn turn in THIS run (runtime turn + the durable reads the workload
    // repeats), the workload shape whose residual writer work the final drain
    // must retire. A loaded/emulated runner stretches this measurement with
    // the run, so the derived wait windows scale with it.
    let mut turn_window: std::collections::VecDeque<u128> = std::collections::VecDeque::new();

    // One real restart/reconnect before the first gauge so even a short run
    // records a reconnect event (the checker requires at least one).
    manager = SessionManager::open(&store_dir, &cas_dir, true).unwrap();
    {
        let reports = manager.recover_all_sessions().unwrap();
        let ok = reports.iter().all(|report| report.crashed_ops.is_empty());
        reconnects += 1;
        emitter.reconnect(ok, reconnects);
    }
    if manager.get_session(session_id).unwrap().is_none() {
        session_id = manager
            .create_session(ws, "convergence-churn", "fake", "m")
            .unwrap()
            .id();
    }

    while started.elapsed() < target {
        let turn_started = std::time::Instant::now();
        let agent = agent_for(manager.clone(), true, 1.0);
        let outcome = agent
            .run_turn(session_id, &format!("convergence turn {turns}"), &[])
            .await
            .unwrap();
        assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
        drop(agent);
        turns += 1;

        {
            let handle = manager.get_session(session_id).unwrap().unwrap();
            // Bounded durable-journal tail read (the UI's latest-page path,
            // not a whole-journal replay): a latency that must not trend
            // upward under churn.
            let journal_start = std::time::Instant::now();
            let _ = handle.latest_messages_page(100).unwrap();
            push_bounded(&mut journal_window, journal_start.elapsed().as_micros());
            // Index-backed pagination from the conversation head.
            let index_start = std::time::Instant::now();
            let _ = handle.messages_page(None, 50).unwrap();
            push_bounded(&mut index_window, index_start.elapsed().as_micros());
        }
        // Observed cost of the complete turn unit this run actually sustained.
        push_bounded(&mut turn_window, turn_started.elapsed().as_millis().max(1));

        tokio::time::sleep(pace).await;

        // Rotate the churn session so journal replay and durable growth stay
        // bounded (a deleted session is terminal; its rows remain auditable).
        if turns.is_multiple_of(100) {
            let _ = manager.clone().delete_session(session_id);
            session_id = manager
                .create_session(ws, "convergence-churn", "fake", "m")
                .unwrap()
                .id();
        }

        // Warm the bounded latency windows before the first gauge: a p95 over
        // a handful of startup measurements trends spuriously.
        if last_gauge.elapsed() >= gauge_every && journal_window.len() >= 32 {
            last_gauge = std::time::Instant::now();
            let telemetry = manager.store().writer_telemetry();
            let queued = {
                let counts = manager.store().queue_status_counts(session_id).unwrap();
                ["pending", "claimed", "running"]
                    .iter()
                    .map(|key| {
                        counts
                            .get(key)
                            .and_then(|value| value.as_u64())
                            .unwrap_or(0)
                    })
                    .sum::<u64>()
            };
            let active = manager.store().all_active_turns().unwrap().len() as u64;
            let tool_rows = manager.store().all_running_tool_rows().unwrap().len() as u64;
            emitter.gauge(
                turns,
                telemetry.pending_depth as u64,
                queued,
                active + tool_rows,
                bounded_p95(&journal_window),
                bounded_p95(&index_window),
            );
        }

        if last_reconnect.elapsed() >= reconnect_every {
            last_reconnect = std::time::Instant::now();
            // Daemon restart: drop the manager (all handles closed above),
            // reopen the SAME data dir, recover, and verify no crashed ops.
            manager = SessionManager::open(&store_dir, &cas_dir, true).unwrap();
            let reports = manager.recover_all_sessions().unwrap();
            let ok = reports.iter().all(|report| report.crashed_ops.is_empty());
            reconnects += 1;
            emitter.reconnect(ok, reconnects);
            if manager.get_session(session_id).unwrap().is_none() {
                session_id = manager
                    .create_session(ws, "convergence-churn", "fake", "m")
                    .unwrap()
                    .id();
            }
        }
    }

    // Quiescent convergence: background work must settle and the writer
    // queue must drain to zero after the churn stops. The drain is an
    // OBSERVABLE condition, so it is waited for, never judged by a fixed
    // wall window: an emulated/loaded runner stretches every turn, and a
    // constant settle window misreads a slow-but-draining writer queue as
    // non-convergence (run 67: the fixed 30 s window expired with the queue
    // at depth 1 after a 1690 s campaign).
    //
    //   * CONTROL (same run): the p95 churn-turn cost measured above — the
    //     cost unit whose residual work the drain must retire.
    //   * OBSERVATION: poll until active turns, running tool rows and the
    //     writer pending depth are ALL zero, then require them to stay zero
    //     for a documented stable window of 10 x control, clamped to
    //     [2 s, 60 s]. Marked `quiescent` gauges carry the observed values,
    //     so the checker's every-marked-sample-zero rule keeps its teeth.
    //   * TEETH: the whole wait is capped at 60 x control, clamped to
    //     [120 s, 900 s]; a genuinely non-draining queue never reaches the
    //     stable window and fails loudly with the last observed state, and
    //     the quiesce property is asserted exactly (== 0) where observed.
    let control_ms = bounded_p95(&turn_window).max(1);
    let stable_ms = control_ms.saturating_mul(10).clamp(2_000, 60_000);
    let settle_cap_ms = control_ms.saturating_mul(60).clamp(120_000, 900_000);
    let poll_ms = (control_ms / 2).clamp(250, 2_000);
    let deadline = std::time::Instant::now() + Duration::from_millis(settle_cap_ms);
    let mut zero_since: Option<std::time::Instant> = None;
    let mut last_mark: Option<std::time::Instant> = None;
    loop {
        let now = std::time::Instant::now();
        let pending = manager.store().writer_telemetry().pending_depth;
        let active = manager.store().all_active_turns().unwrap().len();
        let tools = manager.store().all_running_tool_rows().unwrap().len();
        if pending == 0 && active == 0 && tools == 0 {
            let since = *zero_since.get_or_insert(now);
            if last_mark.is_none_or(|last| now.duration_since(last) >= Duration::from_secs(1)) {
                emitter.gauge_marked(
                    turns,
                    pending as u64,
                    active as u64,
                    tools as u64,
                    bounded_p95(&journal_window),
                    bounded_p95(&index_window),
                    true,
                );
                last_mark = Some(now);
            }
            if now.duration_since(since) >= Duration::from_millis(stable_ms) {
                break;
            }
        } else {
            zero_since = None;
        }
        assert!(
            now < deadline,
            "writer queue did not quiesce to zero within {settle_cap_ms} ms \
             (control={control_ms} ms turn p95, stable window={stable_ms} ms): \
             active_turns={active} running_tools={tools} pending_depth={pending}"
        );
        tokio::time::sleep(Duration::from_millis(poll_ms)).await;
    }
    // The quiesce property, asserted exactly where it was observed.
    let pending = manager.store().writer_telemetry().pending_depth;
    let active = manager.store().all_active_turns().unwrap().len();
    let tools = manager.store().all_running_tool_rows().unwrap().len();
    assert_eq!(
        pending, 0,
        "writer queue must quiesce to zero (observed drain + stable window)"
    );
    assert_eq!(
        active, 0,
        "no active turns may remain after quiescence was observed"
    );
    assert_eq!(
        tools, 0,
        "no running tool rows may remain after quiescence was observed"
    );
    emitter.gauge_marked(
        turns,
        0,
        0,
        0,
        bounded_p95(&journal_window),
        bounded_p95(&index_window),
        true,
    );
    drop(manager);
    // A quiet window lets the driver's bounded samplers capture the
    // post-quiescence process/data-dir state before this process exits. It
    // scales with the same-run control (an emulated runner's sampler ticks
    // are slower too) and stays bounded.
    let quiet_ms = control_ms.saturating_mul(4).clamp(3_000, 20_000);
    tokio::time::sleep(Duration::from_millis(quiet_ms)).await;
    eprintln!(
        "soak convergence churn: target={target_seconds}s elapsed={:.1}s turns={turns} reconnects={reconnects} control_ms={control_ms} stable_ms={stable_ms} settle_cap_ms={settle_cap_ms} quiet_ms={quiet_ms}",
        started.elapsed().as_secs_f64()
    );
}
