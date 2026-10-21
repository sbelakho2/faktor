//! Independent long-run token/context audit against a real local Ollama
//! runtime through a transparent logging proxy. Manual measurement, not a
//! certification: `FAKTOR_AUDIT_LIVE=1 cargo run --release -p
//! faktor-tests-longrun --example indep_longrun_audit`.
//!
//! The harness drives the production `AgentRuntime` (real session store,
//! real compactor, real wire planner) and prints one JSON object with the
//! durable-side facts. Wire-level token counts come from the proxy log.

use std::pin::Pin;
use std::sync::Arc;

use faktor_agent::{AgentDeps, AgentRuntime, NoEvidence, PermissionRequester, ToolRegistry};
use faktor_core::capability::PermissionDecision;
use faktor_core::model::{CapabilityResolution, ModelCapabilities};
use faktor_core::time::SystemClock;
use faktor_ollama::{OllamaConfig, OllamaProvider};
use faktor_orchestrator::runtime::OrchestratorRuntime;
use faktor_provider::egress::PolicyCheckedHttpTransport;
use faktor_provider::{
    GenericAgentRequest, Provider, ProviderChunk, ProviderRegistry, ProviderStream,
};
use faktor_session::child::{ChildIdentity, ChildRuntimeBlockerRow};
use faktor_session::{SessionHandle, SessionManager};
use futures::StreamExt;
use serde_json::json;

const MARKS: [&str; 8] = [
    "FACT-ALPHA-01",
    "FACT-BRAVO-02",
    "FACT-CHARLIE-03",
    "FACT-DELTA-04",
    "FACT-ECHO-05",
    "FACT-FOXTROT-06",
    "FACT-GOLF-07",
    "FACT-HOTEL-08",
];

struct AlwaysAllow;
impl PermissionRequester for AlwaysAllow {
    fn request(
        &self,
        _session: faktor_core::id::SessionId,
        _permission: &faktor_session::ops::PermissionRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = faktor_core::Result<PermissionDecision>> + Send>>
    {
        Box::pin(async { Ok(PermissionDecision::Allow) })
    }
}

/// Counts calls/usage from the canonical frames (cross-check of the proxy).
struct CountingProvider {
    inner: Arc<dyn Provider>,
    caps: ModelCapabilities,
    main_in: Arc<std::sync::atomic::AtomicU64>,
    main_cached: Arc<std::sync::atomic::AtomicU64>,
    main_out: Arc<std::sync::atomic::AtomicU64>,
    compact_in: Arc<std::sync::atomic::AtomicU64>,
    compact_out: Arc<std::sync::atomic::AtomicU64>,
    main_calls: Arc<std::sync::atomic::AtomicU64>,
    compact_calls: Arc<std::sync::atomic::AtomicU64>,
}

impl Provider for CountingProvider {
    fn id(&self) -> &str {
        "ollama"
    }
    fn capabilities(&self, _model: &str) -> ModelCapabilities {
        self.caps.clone()
    }
    fn capability_resolution(&self, _model: &str) -> CapabilityResolution {
        CapabilityResolution::known(self.caps.clone())
    }
    fn resolved_model(&self, model: &str) -> String {
        self.inner.resolved_model(model)
    }
    fn stream(&self, req: GenericAgentRequest) -> ProviderStream {
        let compaction = req
            .system
            .starts_with("You are the Faktor context compactor");
        let counters = if compaction {
            self.compact_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (
                self.compact_in.clone(),
                Arc::new(std::sync::atomic::AtomicU64::new(0)),
                self.compact_out.clone(),
            )
        } else {
            self.main_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (
                self.main_in.clone(),
                self.main_cached.clone(),
                self.main_out.clone(),
            )
        };
        let stream = self.inner.stream(req);
        Box::pin(stream.map(move |item| {
            if let Ok(ProviderChunk::Usage(usage)) = &item {
                counters.0.fetch_add(
                    usage.uncached_input_tokens + usage.cache_write_tokens,
                    std::sync::atomic::Ordering::Relaxed,
                );
                counters.1.fetch_add(
                    usage.cache_read_tokens,
                    std::sync::atomic::Ordering::Relaxed,
                );
                counters
                    .2
                    .fetch_add(usage.output_tokens, std::sync::atomic::Ordering::Relaxed);
            }
            item
        }))
    }
}

fn session_handle(manager: &Arc<SessionManager>, id: faktor_core::id::SessionId) -> SessionHandle {
    manager.get_session(id).unwrap().unwrap()
}

fn deps(manager: Arc<SessionManager>, registry: Arc<ProviderRegistry>, model: &str) -> AgentDeps {
    AgentDeps {
        session: manager,
        providers: registry,
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
        model: model.to_string(),
        compaction_model: Some(model.to_string()),
        compact_at_usage: std::env::var("AUDIT_COMPACT_AT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.25),
        instructions: "You are a measurement agent. Follow the reply protocol exactly.".to_string(),
        clock: Arc::new(SystemClock),
        tool_call_mode: faktor_agent::ToolCallMode::Native,
        tool_deadline_ms: 2000,
        retry_policy: faktor_core::retry::RetryPolicy::default(),
        semantic: faktor_agent::fallback_semantic_registry(),
        context_prior: None,
        secret_registry: None,
        receipt_sink: None,
        efficiency: faktor_agent::EfficiencyFlags {
            failure_learning: true,
            ccr: true,
            typed_handoff: true,
            semantic_context: true,
            rework_routing: true,
        },
    }
}

fn provider(
    model: &str,
    base: &str,
    caps: ModelCapabilities,
) -> (Arc<ProviderRegistry>, Arc<CountingProvider>) {
    let policy = faktor_security::destination::DestinationPolicy::parse_lines([base.to_string()])
        .expect("loopback destination policy");
    let transport = PolicyCheckedHttpTransport::try_with_policy(policy).expect("egress transport");
    let ollama: Arc<dyn Provider> = OllamaProvider::new(
        OllamaConfig::new(Some(base.to_string())),
        Arc::new(transport),
    );
    let counting = Arc::new(CountingProvider {
        inner: ollama,
        caps,
        main_in: Arc::new(Default::default()),
        main_cached: Arc::new(Default::default()),
        main_out: Arc::new(Default::default()),
        compact_in: Arc::new(Default::default()),
        compact_out: Arc::new(Default::default()),
        main_calls: Arc::new(Default::default()),
        compact_calls: Arc::new(Default::default()),
    });
    let mut registry = ProviderRegistry::new();
    registry.try_register(counting.clone()).unwrap();
    let _ = model;
    (Arc::new(registry), counting)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    if std::env::var("FAKTOR_AUDIT_LIVE").as_deref() != Ok("1") {
        println!(
            "{}",
            json!({"skipped": "set FAKTOR_AUDIT_LIVE=1 to run the real-Ollama audit"})
        );
        return;
    }
    let turns: usize = std::env::var("AUDIT_TURNS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(340);
    let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "llama3.2:1b".to_string());
    let base =
        std::env::var("OLLAMA_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:19434".to_string());
    let dir = tempfile::tempdir().expect("temp dir");
    let store_dir = dir.path().join("store");
    let cas_dir = dir.path().join("cas");
    let workspace_dir = dir.path().join("ws");
    std::fs::create_dir_all(&workspace_dir).unwrap();
    let manager = SessionManager::open(&store_dir, &cas_dir, true).expect("session store");
    let mut caps = ModelCapabilities::small_local();
    caps.max_output = std::env::var("AUDIT_MAX_OUTPUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256);
    let (registry, counting) = provider(&model, &base, caps.clone());
    let runtime =
        AgentRuntime::new(deps(manager.clone(), registry.clone(), &model)).expect("runtime");
    let ws = manager
        .create_workspace(workspace_dir.to_str().unwrap())
        .unwrap();
    let parent = manager
        .create_session(
            ws,
            &format!(
                "Keep the audit ledger. Retain these facts exactly: {}",
                MARKS.join(", ")
            ),
            "ollama",
            &model,
        )
        .unwrap()
        .id();
    let parent_handle = session_handle(&manager, parent);
    parent_handle
        .put_task_ledger(json!({
            "goal": format!("Keep the audit ledger. Retain these facts exactly: {}", MARKS.join(", ")),
            "constraints": ["goal must stay verbatim", "durable facts appear exactly once"],
            "decisions": [format!("DURABLE-DECISION-{}", MARKS[0])],
            "known_failures": [],
            "changed_files": ["audit/ledger.md"],
            "tests_run": ["ledger-retention"],
            "tests_failed": [],
            "open_steps": ["keep appending turn confirmations"],
            "completed_steps": [],
            "user_preferences": []
        }))
        .unwrap();
    parent_handle
        .ledger_decision(
            "retention",
            "durable facts are never re-summarized",
            "compaction",
        )
        .unwrap();

    // Warm the model outside the measured window.
    runtime
        .run_turn(parent, "Reply with ACK 0", &[])
        .await
        .expect("warm turn");
    counting
        .main_in
        .swap(0, std::sync::atomic::Ordering::Relaxed);
    counting
        .main_cached
        .swap(0, std::sync::atomic::Ordering::Relaxed);
    counting
        .main_out
        .swap(0, std::sync::atomic::Ordering::Relaxed);
    counting
        .main_calls
        .swap(0, std::sync::atomic::Ordering::Relaxed);

    let started = std::time::Instant::now();
    let mut completed = 0usize;
    let mut ack_ok = 0usize;
    let mut turn_failures = 0usize;
    let mut turn_errors = Vec::new();
    for turn in 1..=turns {
        let prompt = format!(
            "AUDIT-TURN-{turn:04}: continue the audit ledger; append one receipt line. \
             Work note {} Reply exactly: ACK {turn}",
            "receipt-detail ".repeat(12)
        );
        match runtime.run_turn(parent, &prompt, &[]).await {
            Ok(outcome)
                if outcome.final_state == faktor_core::state::AgentState::ReadyForNextTurn =>
            {
                completed += 1;
            }
            Ok(outcome) => {
                // A recoverable failure is a failed attempt, not the end of
                // the session: record it and submit the next user turn on
                // the recovered session (bounded consecutive failures).
                turn_failures += 1;
                turn_errors.push(format!(
                    "turn {turn}: state {:?} stop {:?}",
                    outcome.final_state, outcome.stop_reason
                ));
                if turn_failures >= 5 {
                    break;
                }
                continue;
            }
            Err(error) => {
                turn_failures += 1;
                turn_errors.push(format!("turn {turn}: {error}"));
                if turn_failures >= 5 {
                    break;
                }
                continue;
            }
        }
        let last = session_handle(&manager, parent)
            .messages_page(None, 1)
            .ok()
            .and_then(|page| page.messages.into_iter().next())
            .and_then(|message| serde_json::to_string(&message).ok())
            .unwrap_or_default()
            .to_uppercase();
        if last.contains(&format!("ACK {turn}")) {
            ack_ok += 1;
        }
        turn_failures = 0;
        if turn % 25 == 0 {
            eprintln!(
                "[audit] turn {turn}/{turns} elapsed {:?}",
                started.elapsed()
            );
        }
    }

    // ---- child isolation + bounded typed handoffs ---------------------
    let orchestrator = OrchestratorRuntime::new(manager.clone(), runtime.clone());
    let child_errors: Vec<String> = Vec::new();
    let mut handoffs = Vec::new();
    let mut child_turns = 0usize;
    for (index, mark) in MARKS.iter().enumerate().take(4) {
        let child_mark = format!("CHILD-ONLY-{index}-{}", mark.replace("FACT-", ""));
        let child = manager
            .create_session(
                ws,
                &format!("child {index}: isolated shard {child_mark}"),
                "ollama",
                &model,
            )
            .unwrap()
            .id();
        let handle = session_handle(&manager, child);
        let identity = ChildIdentity {
            parent_session_id: parent,
            workspace_id: ws.raw(),
            worktree_id: 1,
            item_id: format!("item-{index}"),
            task_goal: format!("audit shard {child_mark}"),
            operation_id: 0,
            ownership: faktor_session::child::ChildOwnership::ReadOnlyShared,
            model: model.clone(),
            require_semantic_delta: false,
            permissions: faktor_core::CapabilitySet::ALL,
            created_ms: 1,
        };
        handle.orchestrator_child_identity_put(&identity).unwrap();
        handle
            .orchestrator_child_runtime_put(&ChildRuntimeBlockerRow {
                child_id: format!("child-{index}"),
                state: "done".into(),
                blocker: None,
                updated_ms: 0,
            })
            .unwrap();
        if runtime
            .run_turn(
                child,
                &format!("Report one sentence about {child_mark}."),
                &[],
            )
            .await
            .is_ok()
        {
            child_turns += 1;
        }
        // Shared facts deliberately repeated across children plus one
        // child-unique decision/fact/memory row each.
        handle
            .ledger_decision("shard", "SHARED-HANDOFF-FACT", "common")
            .unwrap();
        handle
            .ledger_decision(
                "shard",
                &format!("UNIQUE-DECISION-{index}"),
                &format!("child-{index}"),
            )
            .unwrap();
        handle
            .upsert_memory_fact("fact", "shared", "SHARED-MEMORY-FACT")
            .unwrap();
        handle
            .upsert_memory_fact(
                "fact",
                &format!("unique-{index}"),
                &format!("UNIQUE-MEMORY-{index}"),
            )
            .unwrap();
        handle
            .put_task_ledger(json!({
                "goal": format!("audit shard {child_mark}"),
                "constraints": [],
                "decisions": [],
                "known_failures": [format!("UNIQUE-FINDING-{index}")],
                "changed_files": [format!("audit/child-{index}.md")],
                "tests_run": [],
                "tests_failed": [],
                "open_steps": [],
                "completed_steps": [],
                "user_preferences": []
            }))
            .unwrap();
        match orchestrator.child_handoff(&format!("child-{index}"), child, 2048) {
            Ok(handoff) => handoffs.push(handoff),
            Err(error) => eprintln!("[audit] handoff {index} refused: {error:?}"),
        }
    }
    let rendered = faktor_agent::runtime::ParentTurnInput {
        user_message: Some(
            "AUDIT-HANDOFF-TURN: consume the child handoffs; reply ACK handoff".into(),
        ),
        internal: handoffs
            .iter()
            .map(
                |handoff| faktor_agent::runtime::InternalContextItem::ChildHandoff {
                    provenance: "orchestrator:child-handoff".into(),
                    handoff: handoff.clone(),
                },
            )
            .collect(),
    }
    .render_internal_bounded(4 * 2048)
    .expect("bounded internal render")
    .unwrap_or_default();
    let handoff_section_tokens = faktor_context::Estimator.estimate_tokens(&rendered);
    let handoff_bytes = rendered.len();
    let shared_occurrences = rendered.matches("SHARED-HANDOFF-FACT").count();
    let shared_memory_occurrences = rendered.matches("SHARED-MEMORY-FACT").count();
    let mut duplicate_handoff_lines = 0usize;
    let mut seen = std::collections::BTreeSet::new();
    for line in rendered.lines().filter(|line| line.starts_with("- ")) {
        if !seen.insert(line) {
            duplicate_handoff_lines += 1;
        }
    }
    let handoff_outcome = runtime
        .run_turn_input(
            parent,
            faktor_agent::runtime::ParentTurnInput {
                user_message: Some(
                    "AUDIT-HANDOFF-TURN: consume the child handoffs; reply ACK handoff".into(),
                ),
                internal: handoffs
                    .iter()
                    .map(
                        |handoff| faktor_agent::runtime::InternalContextItem::ChildHandoff {
                            provenance: "orchestrator:child-handoff".into(),
                            handoff: handoff.clone(),
                        },
                    )
                    .collect(),
            },
            4 * 2048,
        )
        .await;

    // ---- durable state + restart digest -------------------------------
    let handle = session_handle(&manager, parent);
    let events = handle.events_range(1, None).unwrap();
    let compacted = events
        .iter()
        .filter(|event| event.kind == faktor_core::event::EventKind::ContextCompacted)
        .count();
    let rejected = events
        .iter()
        .filter(|event| event.kind == faktor_core::event::EventKind::CompactRejected)
        .count();
    let checkpoint_before = handle.context_checkpoint().unwrap();
    let events_before = events.len();
    let messages_before = handle.messages_page(None, 10_000).unwrap().messages.len();
    let head_decisions = handle
        .ledger_ensure_head()
        .map(|head| {
            head.decisions
                .iter()
                .map(|d| format!("{}: {}", d.step, d.choice))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let before_digest = checkpoint_before
        .as_ref()
        .map(|checkpoint| {
            format!(
                "{}:{}:{}",
                checkpoint.through_seq,
                checkpoint.digest,
                blake3::hash(&checkpoint.payload).to_hex()
            )
        })
        .unwrap_or_default();
    let checkpoint_turns: Vec<faktor_context::RecentTurn> = checkpoint_before
        .as_ref()
        .and_then(|checkpoint| serde_json::from_slice(&checkpoint.payload).ok())
        .unwrap_or_default();
    let summary_turns = checkpoint_turns
        .iter()
        .filter(|turn| turn.role == "summary")
        .count();
    let digest_turns = checkpoint_turns
        .iter()
        .filter(|t| t.text.starts_with("[Earlier context archived:"))
        .count();
    drop(orchestrator);
    drop(runtime);
    drop(parent_handle);
    drop(handle);
    drop(manager);

    let reopened = SessionManager::open(&store_dir, &cas_dir, true).expect("reopen");
    let reopened_handle = session_handle(&reopened, parent);
    let checkpoint_after = reopened_handle.context_checkpoint().unwrap();
    let after_digest = checkpoint_after
        .as_ref()
        .map(|checkpoint| {
            format!(
                "{}:{}:{}",
                checkpoint.through_seq,
                checkpoint.digest,
                blake3::hash(&checkpoint.payload).to_hex()
            )
        })
        .unwrap_or_default();
    let events_after = reopened_handle.events_range(1, None).unwrap().len();
    let messages_after = reopened_handle
        .messages_page(None, 10_000)
        .unwrap()
        .messages
        .len();
    let (registry2, counting2) = provider(&model, &base, caps);
    let runtime2 =
        AgentRuntime::new(deps(reopened.clone(), registry2, &model)).expect("restart runtime");
    let restart_turn = runtime2
        .run_turn(parent, "AUDIT-RESTART-TURN: reply exactly ACK restart", &[])
        .await;

    let main_in = counting.main_in.load(std::sync::atomic::Ordering::Relaxed);
    let main_cached = counting
        .main_cached
        .load(std::sync::atomic::Ordering::Relaxed);
    let main_out = counting.main_out.load(std::sync::atomic::Ordering::Relaxed);
    let compact_in = counting
        .compact_in
        .load(std::sync::atomic::Ordering::Relaxed);
    let compact_out = counting
        .compact_out
        .load(std::sync::atomic::Ordering::Relaxed);
    println!(
        "{}",
        json!({
            "turns_requested": turns,
            "turns_completed": completed,
            "turn_errors": turn_errors,
            "turn_failures": turn_failures,
            "ack_ok": ack_ok,
            "elapsed_ms": started.elapsed().as_millis() as u64,
            "compactions": compacted,
            "compact_rejected": rejected,
            "main_calls": counting.main_calls.load(std::sync::atomic::Ordering::Relaxed),
            "compact_calls": counting.compact_calls.load(std::sync::atomic::Ordering::Relaxed),
            "main_uncached_input": main_in,
            "main_cached_input": main_cached,
            "main_output": main_out,
            "compact_uncached_input": compact_in,
            "compact_output": compact_out,
            "child_turns": child_turns,
            "child_errors": child_errors,
            "handoff_outcome": handoff_outcome.as_ref().ok().map(|o| format!("{:?}", o.final_state)),
            "handoff_section_tokens": handoff_section_tokens,
            "handoff_bytes": handoff_bytes,
            "handoff_shared_occurrences": shared_occurrences,
            "handoff_shared_memory_occurrences": shared_memory_occurrences,
            "handoff_duplicate_lines": duplicate_handoff_lines,
            "checkpoint_summary_turns": summary_turns,
            "checkpoint_digest_turns": digest_turns,
            "checkpoint_before": before_digest,
            "checkpoint_after": after_digest,
            "checkpoint_equal": before_digest == after_digest,
            "events_before": events_before,
            "events_after": events_after,
            "messages_before": messages_before,
            "head_decisions": head_decisions,
            "messages_after": messages_after,
            "restart_turn": restart_turn.as_ref().ok().map(|o| format!("{:?}", o.final_state)),
            "restart_calls": counting2.main_calls.load(std::sync::atomic::Ordering::Relaxed),
            "store_dir": store_dir.to_string_lossy(),
        })
    );
}
