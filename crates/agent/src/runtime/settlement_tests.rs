//! `runtime::settlement_tests`: out-of-line tests.

use super::*;
use crate::runtime::fixtures_tests::*;
use crate::runtime::tests::*;
use crate::*;

#[tokio::test]
async fn richer_usage_chunk_settles_without_breaking_the_turn() {
    // A cache-heavy usage frame (no plain input counter) must not error
    // the stream or zero the recorded call: the turn completes cleanly.
    let (deps, _dir) = deps_with(Arc::new(CacheHeavyProvider), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime.run_turn(session, "hi", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(outcome.turns, 1);
}

#[tokio::test]
async fn change_budget_refusal_is_answered_with_typed_result() {
    // ChangeBudget edit gate: a mutating tool whose declared write paths
    // leave the budget is refused BEFORE execution and answered typed.
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "c_budget".into(),
                name: "write_file".into(),
                input: serde_json::json!({
                    "path": "src/out_of_scope.rs",
                    "content": "pub fn out() -> u32 { 0 }\n"
                }),
            },
            ScriptedResponse::End,
        ]),
        vec![counting_write_tool(executions.clone())],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    handle
        .set_change_budget(Some(&faktor_core::state::ChangeBudget {
            allowed_paths: vec!["docs".into()],
            ..Default::default()
        }))
        .unwrap();
    let outcome = runtime
        .run_turn(session, "edit outside", &[])
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_refusal_answered(
        &runtime,
        session,
        "c_budget",
        "change_budget_refused",
        "change budget refused the edit",
        &executions,
    );
}

#[tokio::test]
async fn budget_overflow_never_reaches_provider() {
    // A request that cannot be budgeted (the untrimmable static prefix
    // alone exceeds the model budget) must fail BEFORE any provider
    // contact: the provider request counter stays at zero.
    let inner = scripted_provider(vec![
        ScriptedResponse::Text("ok".into()),
        ScriptedResponse::End,
    ]);
    let wrapper = Arc::new(InspectingProvider::new(Arc::new(inner), |_n, _req| Ok(())));
    let (mut deps, _dir) = deps_with(wrapper.clone(), vec![]);
    // > 25K tokens of static instructions: even an empty history cannot
    // fit — the planner must Err(Oversized) instead of sending anything.
    deps.instructions = format!("You are Faktor.\n{}", "x".repeat(100_100));
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let err = runtime
        .run_turn(session, &"y".repeat(10_000), &[])
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Oversized);
    assert_eq!(
        wrapper.counter.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the provider must never be contacted with an unbudgeted request"
    );
}

#[tokio::test]
async fn runtime_context_limit_shrinks_the_budget_below_the_model_maximum() {
    // P0 (the /api/ps allocation reaches the REAL budget): the drive
    // loop budgets from min(capabilities.context, runtime_context_limit)
    // — a provider whose ADVERTISED maximum is 200K but whose LIVE
    // runtime window is 40K must plan its wire content under the
    // 40K-derived budget. The seeded history (~40K tokens of messages)
    // exceeds that budget, so the oldest seeds are trimmed from the
    // request — under the untouched model-maximum budget (65K+ of
    // context) nothing would have been dropped at all.
    let (seed_deps, _dir0) = deps(scripted_provider(vec![ScriptedResponse::End]), vec![]);
    let (manager, session) = shared_session(&seed_deps);
    seed_long_history(&manager, session, 8, 20_000).await;

    let main_caps = ModelCapabilities {
        tools: true,
        context: 200_000,
        ..Default::default()
    };
    let inner = Arc::new(FakeProvider::with_script(
        "fake",
        main_caps.clone(),
        vec![ScriptedResponse::Text("ok".into()), ScriptedResponse::End],
    ));
    let captured: Arc<std::sync::Mutex<Vec<GenericAgentRequest>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let cap = captured.clone();
    let inspected = Arc::new(InspectingProvider::new(inner, move |_n, req| {
        cap.lock().unwrap().push(req.clone());
        Ok(())
    }));
    let limited = Arc::new(RuntimeLimitedProvider::new(inspected, 40_000));
    let mut registry = ProviderRegistry::new();
    registry.try_register(limited).unwrap();
    let (mut final_deps, _dir) = deps_sharing_session(
        manager.clone(),
        Arc::new(FakeProvider::with_script(
            "fake",
            main_caps.clone(),
            vec![ScriptedResponse::End],
        )),
        vec![],
    );
    final_deps.providers = Arc::new(registry);
    // Compaction fires only at usage >= 1.0 — a trimmed plan lands
    // below that, so the request content is purely budget-governed
    // (no compaction interference).
    final_deps.compact_at_usage = 1.0;
    let runtime = AgentRuntime::new(final_deps).unwrap();
    let outcome = runtime
        .run_turn(session, "do the thing", &[])
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);

    // The budget the drive loop must have used: for_capabilities over
    // min(model maximum, runtime limit) — the exact effective caps the
    // runtime derives.
    let effective_caps = ModelCapabilities {
        context: main_caps.context.min(40_000),
        ..main_caps.clone()
    };
    let effective_budget = ContextBudget::for_capabilities(&effective_caps);
    let model_max_budget = ContextBudget::for_capabilities(&main_caps);
    assert!(
        effective_budget.context_max() < model_max_budget.context_max(),
        "the runtime limit must shrink the budget"
    );

    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 1, "one wire request for the turn");
    let req = &requests[0];
    // Planner-exact estimate of the captured request (text-only
    // messages, no tools): est(system) + per message 2 + Σ(est + 1).
    let est = faktor_context::Estimator;
    let total: usize = est.estimate_tokens(&req.system)
        + req
            .messages
            .iter()
            .map(|m| {
                2usize.saturating_add(
                    m.content
                        .iter()
                        .map(|p| match &p.kind {
                            ContentKind::Text { text } => {
                                est.estimate_tokens(text).saturating_add(1)
                            }
                            _ => 1,
                        })
                        .sum::<usize>(),
                )
            })
            .sum::<usize>();
    assert!(
        total <= effective_budget.context_max(),
        "the request must fit the runtime-limited budget: {total} > {}",
        effective_budget.context_max()
    );
    let all_text = request_text(req);
    assert!(
        all_text.contains("turn seed7"),
        "the newest seed must survive trimming"
    );
    assert!(
        !all_text.contains("turn seed0"),
        "the oldest seed must be trimmed under the runtime-limited budget"
    );
    assert!(total > 25_000, "real history must survive: {total}");
}

/// Boundary race (audit): the active turn ends exactly as the runner's
/// wait budget expires. A settle-path kick that arrives while the runner
/// still holds the gate must ARM it for one more bounded pass instead of
/// being dropped — the durable head is claimed by exactly one runner and
/// completes exactly once, with no second runner and no lost kick.
#[tokio::test]
async fn queue_runner_kick_in_budget_boundary_is_armed_and_drains_exactly_once() {
    let base = Arc::new(scripted_provider(vec![
        ScriptedResponse::Text("A answer".into()),
        ScriptedResponse::End,
    ]));
    let provider = Arc::new(InspectingProvider::new(base, |_, _| Ok(())));
    let (deps, _dir) = deps_with(provider.clone(), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    // A short wall budget: the runner's first bounded pass expires while
    // the active turn is still mid-flight.
    runtime.set_turn_budget_ms(250);
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    // A is the active logical turn (submitted, not driven); B queues.
    let receipt_a = runtime.submit(session, "A prompt", &[]).unwrap();
    assert!(!receipt_a.queued);
    let _receipt_b = runtime.submit(session, "B prompt", &[]).unwrap();
    assert_eq!(handle.queued_prompt_count().unwrap(), 1);

    // runner1 owns the gate and waits on the mid-turn session.
    let runner1 = runtime.clone();
    let t1 = tokio::spawn(async move { runner1.run_session_queue(session).await });
    wait_for_runner_passes(&runtime, session, 1).await;

    // The settle-path kick arrives while runner1 is still gated: it must
    // ARM the live runner, never start a second one and never be lost.
    let kicker = runtime.clone();
    kicker.run_session_queue(session).await;
    assert!(
        !t1.is_finished(),
        "the kick must arm the live runner, not replace it"
    );

    // The first bounded pass expires (A still mid-turn); ONLY the armed
    // second pass keeps the SAME runner task alive. An unarmed runner
    // would have exited here and left B pending with no runner.
    wait_for_runner_passes(&runtime, session, 2).await;
    assert!(
        !t1.is_finished(),
        "the armed pass must keep the same runner alive (kick not lost)"
    );

    // A ends exactly at the boundary while runner1 is inside its armed
    // pass; the pass admits B and drives it exactly once.
    let outcome = runtime
        .drive_receipt(&handle, receipt_a, None)
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    t1.await.unwrap();

    assert_eq!(handle.queued_prompt_count().unwrap(), 0, "B drained");
    let counts = handle.queue_status_counts().unwrap();
    assert_eq!(
        counts.get("done").and_then(|v| v.as_i64()),
        Some(1),
        "B's durable row completed exactly once: {counts}"
    );
    assert_eq!(
        provider.counter.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "A and B each streamed exactly one provider call"
    );
    let events = handle.events_range(1, None).unwrap();
    let prompts = events
        .iter()
        .filter(|e| e.kind == faktor_core::event::EventKind::PromptReceived)
        .count();
    assert_eq!(prompts, 2, "one PromptReceived per prompt");
    let turns = events
        .iter()
        .filter(|e| e.kind == faktor_core::event::EventKind::TurnCompleted)
        .count();
    assert_eq!(turns, 2, "exactly two logical turns completed");
}

#[tokio::test]
async fn exhausted_token_budget_blocks_verified_complete() {
    // Adversarial: a task whose durable spend already exceeds max_tokens
    // must NEVER reach VerifiedComplete — the gate refuses at the same
    // genuine end that would otherwise claim completion.
    let (manager, session, _dir) = verified_shared_env();
    let h = manager.get_session(session).unwrap().unwrap();
    // Durable spend first (provider-call rows are the crash-safe source).
    h.record_provider_call(
        OpId::new(4242),
        "fake",
        "m",
        "completed",
        Some(500),
        Some(200),
        None,
    )
    .unwrap();
    assert_eq!(h.spent_tokens().unwrap(), 700);
    // Pre-create the task row with a small token budget: the runtime
    // preserves caller-set caps on an existing row.
    let now = h.now_ms();
    h.create_task(Task {
        task_id: h.task_id().unwrap(),
        session_id: session,
        goal: "gating task".into(),
        acceptance_criteria: vec![],
        plan: vec![],
        attachments: Vec::new(),
        budget: faktor_session::TaskBudget {
            max_tokens: Some(10),
            max_turns: None,
            spent_tokens: 700,
            spent_turns: 0,
        },
        state: TaskState::Running,
        created_ms: now,
        updated_ms: now,
    })
    .unwrap();
    // The turn's checks PASS; only the budget refuses the gate.
    let (turn_deps, _d) = verified_turn_deps(
        &manager,
        vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/a.rs", "content": "pub fn a() -> u32 {\n    let base: u32 = 10;\n    let step: u32 = 41;\n    base.saturating_add(step).saturating_add(1)\n}\n"}),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        fake_ok(),
        0.65,
    );
    let runtime = AgentRuntime::new(turn_deps).unwrap();
    let outcome = runtime
        .run_turn(session, "write src/a.rs", &[])
        .await
        .unwrap();
    assert_eq!(outcome.acceptance, Some(faktor_verify::Acceptance::Pass));
    match outcome.completion {
        Some(CompletionGate::BlockedVerification { reasons }) => {
            assert!(
                reasons.iter().any(|r| {
                    r.code == ReasonCode::SpendOverBudget
                        && r.detail.contains("budget")
                        && r.detail.contains("700")
                }),
                "the refusal must name the exhausted budget: {reasons:?}"
            );
        }
        other => panic!("an exhausted budget must block completion, got {other:?}"),
    }
    let h = manager.get_session(session).unwrap().unwrap();
    let tasks = h.list_tasks().unwrap();
    assert_eq!(
        tasks.len(),
        1,
        "the pre-created row is updated, not replaced"
    );
    assert_eq!(
        tasks[0].state,
        TaskState::Blocked,
        "row never claims completion"
    );
    assert_eq!(
        tasks[0].budget.max_tokens,
        Some(10),
        "caller budget caps survive"
    );
    let facts = h.memory_facts().unwrap();
    assert!(
        facts
            .iter()
            .any(|(k, key, v)| k == "task_state" && key == "state" && v == "blocked"),
        "the durable gate rows must agree with the refused gate: {facts:?}"
    );
}

#[tokio::test]
async fn usage_settlement_persists_reported_cost_and_route_json_on_the_reservation() {
    // P0-6/12 (e): the usage settlement of a ROUTED turn persists the
    // provider-reported cost AND the routing decision JSON onto the
    // reservation row (the durable chain: route decision -> reservation
    // -> settlement).
    let costly = CostReportingProvider::new(vec![(Some(777), false)]);
    let mut registry = ProviderRegistry::new();
    let dyn_p: Arc<dyn faktor_provider::Provider> = costly.clone();
    registry.try_register(dyn_p).unwrap();
    let (mut adeps, _dir) = deps_with(costly.clone(), vec![]);
    adeps.providers = Arc::new(registry);
    let ledger = faktor_session::DurableBudgetLedger::new(adeps.session.clone());
    let budgets: Arc<dyn faktor_session::BudgetAuthority> = ledger.clone();
    adeps.budgets = budgets;
    let candidate = faktor_core::model::ModelDescriptor {
        provider: "fake".into(),
        model: "m".into(),
        context: 512_000,
        max_output: 16_000,
        tools: true,
        parallel_tools: true,
        reasoning: false,
        thinking: false,
        vision: false,
        structured_output: false,
        embeddings: false,
        streaming: true,
        // P0-1/wave-B: the descriptor's economics is the router's
        // per-token ESTIMATE surface ($1/Mtok in AND out); the
        // route-time price capture is cut from the catalog state
        // ($1/M per-million-token quote) so settlement is priced at
        // the frozen exact quote — 40 x 1 + 9 x 1 == 49 microUSD,
        // never a fabricated 1-micro-per-token fallback.
        economics: faktor_core::model::ModelEconomics {
            input_price_per_mtok: faktor_core::model::MicroUsdPerToken::from_dollars_per_million(1),
            output_price_per_mtok: faktor_core::model::MicroUsdPerToken::from_dollars_per_million(
                1,
            ),
            coding_reliability: 90,
            tool_reliability: 90,
            ..Default::default()
        },
        source: faktor_core::model::ModelSource::ProviderCatalog,
    };
    adeps.routing = crate::EconomicRoutingPolicy::new(
        Arc::new(faktor_router::RouterService::with_pricing(
            vec![candidate.clone()],
            std::collections::HashMap::from([(
                (candidate.provider.clone(), candidate.model.clone()),
                faktor_core::model::PricingSnapshot::exact(
                    faktor_core::model::PriceQuote {
                        input:
                            faktor_core::model::MicroUsdPerMillionTokens::from_dollars_per_million(
                                1,
                            ),
                        output:
                            faktor_core::model::MicroUsdPerMillionTokens::from_dollars_per_million(
                                1,
                            ),
                        cache_read: faktor_core::model::MicroUsdPerMillionTokens::ZERO,
                        cache_write: faktor_core::model::MicroUsdPerMillionTokens::ZERO,
                    },
                    1,
                    "fake".to_string(),
                ),
            )]),
        )),
        crate::RoutingMode::Economy,
    );
    let runtime = AgentRuntime::new(adeps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let now = handle.now_ms();
    handle
        .create_task(faktor_session::Task {
            task_id: handle.task_id().unwrap(),
            session_id: session,
            goal: "routed".into(),
            acceptance_criteria: vec![],
            plan: vec![],
            attachments: Vec::new(),
            budget: faktor_session::TaskBudget::default(),
            state: faktor_core::state::TaskState::Pending,
            created_ms: now,
            updated_ms: now,
        })
        .unwrap();
    let outcome = runtime.run_turn(session, "hi", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    let task_id = handle.task_id().unwrap();
    let view = ledger
        .session_budget_view(session, task_id)
        .expect("durable budget view");
    assert_eq!(
        view.spent_cost_micro, 777,
        "the provider-reported cost is authoritative at settlement"
    );
    let rows = ledger.reservations_of(session, task_id, 10).unwrap();
    assert_eq!(rows.len(), 1, "one reservation per paid call");
    let row = &rows[0];
    assert_eq!(row.status, "settled");
    assert_eq!(
        row.provider_reported_micro,
        Some(777),
        "the provider-reported cost rides the reservation row"
    );
    assert_eq!(
            row.provider_cost_micro,
            Some(49),
            "the locally calculated cost (40 in + 9 out at the frozen 1 microUSD/token lines) is recorded too"
        );
    assert_eq!(
        row.pricing_snapshot.as_ref().map(|s| s.authority),
        Some(faktor_core::model::PriceAuthority::Exact),
        "the routed decision's price capture rides the reservation"
    );
    assert_eq!(
        row.pricing_snapshot
            .as_ref()
            .and_then(|s| s.quote)
            .map(|q| (q.input.0, q.output.0)),
        Some((1_000_000, 1_000_000)),
        "the frozen per-million-token lines are exactly the catalog quote of the chosen candidate"
    );
    assert!(
        row.dispatched_ms.is_some(),
        "the dispatch marker was written before the stream"
    );
    let json = row
        .route_decision_json
        .as_deref()
        .expect("route json recorded");
    assert!(
        json.contains("\"provider\":\"fake\"") && json.contains("\"model\":\"m\""),
        "the routing decision JSON rides the reservation: {json}"
    );
}

#[tokio::test]
async fn canonical_cache_split_settles_exactly_without_double_billing() {
    // Audit Phase-1 item C regression: the old runtime priced openai's
    // `tokens_in` — a total ALREADY containing the 600 cached tokens —
    // AND added the 600-token cache_read line on top (1350 micro). The
    // canonical frame arrives split (400 uncached + 600 cache reads) and
    // must price exactly 400@input + 600@cache_read + 50@output = 750
    // micro at the frozen $1/$0.5/$1 lines.
    let provider: Arc<dyn faktor_provider::Provider> =
        Arc::new(CacheSplitProvider { reported: None });
    let (runtime, session, ledger) = routed_settlement_runtime(provider).await;
    let outcome = runtime.run_turn(session, "hi", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    let task_id = runtime
        .deps
        .session
        .get_session(session)
        .unwrap()
        .unwrap()
        .task_id()
        .unwrap();
    let view = ledger
        .session_budget_view(session, task_id)
        .expect("durable budget view");
    assert_eq!(
        view.spent_cost_micro, 750,
        "400@1 + 600@0.5 + 50@1 = 750 micro — never 1350 (cache double bill) \
             and never 1000 (cached tokens repriced at the input line)"
    );
    let rows = ledger.reservations_of(session, task_id, 10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "settled");
    assert_eq!(rows[0].provider_cost_micro, Some(750));
    assert_eq!(rows[0].provider_reported_micro, None);
    drop(runtime);
}

#[tokio::test]
async fn hard_budget_denies_the_request_before_the_provider() {
    // P0-6/12: a request whose estimate exceeds the task's DURABLE cost
    // cap never reaches the provider; the reservation refusal is a typed
    // BudgetExceeded stop on the turn and no tool ever ran.
    let counted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let tool = {
        let counted = counted.clone();
        Tool {
            name: "t".into(),
            description: "d".into(),
            input_schema: serde_json::json!({}),
            resource_class: faktor_core::resource::ResourceClass::Cpu,
            capability: None,
            recovery_hint: RecoveryHint::Idempotent,
            path_args: vec![],
            execute: Arc::new(move |_ctx, _a: serde_json::Value| {
                let c = counted.clone();
                Box::pin(async move {
                    c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(ToolOutcome {
                        text: "x".into(),
                        exit_code: Some(0),
                        ..Default::default()
                    })
                })
            }),
        }
    };
    let (mut adeps, _dir) = deps_with(
        Arc::new(scripted_provider(vec![ScriptedResponse::End])),
        vec![tool],
    );
    // The real DURABLE ledger replaces the old in-memory budget_micro:
    // a tiny cap of 1 micro denies the very first reservation.
    let ledger = faktor_session::DurableBudgetLedger::new(adeps.session.clone());
    let budgets: Arc<dyn faktor_session::BudgetAuthority> = ledger.clone();
    adeps.budgets = budgets;
    let runtime = AgentRuntime::new(adeps).unwrap();
    let session = new_session(runtime.deps());
    // The task row exists before the drive (the drive's restore heals,
    // never recreates) and carries the money cap.
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let now = handle.now_ms();
    handle
        .create_task(faktor_session::Task {
            task_id: handle.task_id().unwrap(),
            session_id: session,
            goal: "budgeted".into(),
            acceptance_criteria: vec![],
            plan: vec![],
            attachments: Vec::new(),
            budget: faktor_session::TaskBudget::default(),
            state: faktor_core::state::TaskState::Pending,
            created_ms: now,
            updated_ms: now,
        })
        .unwrap();
    ledger
        .set_task_max_cost(session, handle.task_id().unwrap(), Some(1))
        .unwrap();
    let outcome = runtime.run_turn(session, "hi", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::FailedRecoverable);
    assert_eq!(
        outcome.stop_reason.as_ref().map(|r| r.code),
        Some(ReasonCode::BudgetExceeded),
        "the typed stop reason carries budget_exceeded"
    );
    assert_eq!(
        counted.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no tool ran"
    );
}

// ---- typed durable ledger integration (audits 27 / 71-72) ----

#[tokio::test]
async fn typed_ledger_records_goal_criteria_verify_and_blocker_entries() {
    // Real drives must append typed ledger entries at the genuine
    // decision points: goal, criteria, plan steps, verify runs, the
    // failed-verification blocker and the turn end.
    let failing = fake(|cmd: &str| Err(format!("check failed: {cmd}")));
    let (deps, _dir, root) = verified_rust_env(
        vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/a.rs", "content": "pub fn g() {}"}),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        Some(failing),
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = session_in_workspace(runtime.deps(), &root);
    let outcome = runtime
        .run_turn(session, "write src/a.rs", &[])
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let title = handle.title().unwrap();
    let view = handle.ledger_view().unwrap();
    assert_eq!(
        view.head.goal, title,
        "GoalSet seeded from the session title"
    );
    assert!(
        view.head.criteria.iter().any(|c| c.contains("cargo check")),
        "CriteriaSet carries the derived checks: {:?}",
        view.head.criteria
    );
    assert!(
        view.head
            .open_blockers
            .iter()
            .any(|r| r.contains("rust_check")),
        "BlockerOpened carries the failed check reason: {:?}",
        view.head.open_blockers
    );
    assert_eq!(
        view.head.last_verify.as_ref().map(|v| v.outcome.as_str()),
        Some("failed"),
        "VerifyRun records the failed end-of-turn verification"
    );
    // Entry-level evidence.
    let mut seen_goal = false;
    let mut seen_criteria = false;
    let mut seen_blocker = false;
    let mut seen_verify = false;
    let mut seen_turn = false;
    let mut seen_plan_step = false;
    let mut cursor = None;
    loop {
        let page = handle.ledger_entries_page(cursor, 200).unwrap();
        for e in &page.entries {
            match &e.payload {
                faktor_session::LedgerPayload::GoalSet { .. } => seen_goal = true,
                faktor_session::LedgerPayload::CriteriaSet { .. } => seen_criteria = true,
                faktor_session::LedgerPayload::BlockerOpened { .. } => seen_blocker = true,
                faktor_session::LedgerPayload::VerifyRun { .. } => seen_verify = true,
                faktor_session::LedgerPayload::TurnCompleted { .. } => seen_turn = true,
                faktor_session::LedgerPayload::PlanStepAdded { .. } => seen_plan_step = true,
                _ => {}
            }
        }
        if !page.has_more {
            break;
        }
        cursor = page.entries.last().map(|e| e.seq);
    }
    assert!(seen_goal && seen_criteria && seen_blocker && seen_verify && seen_turn);
    assert!(
        seen_plan_step,
        "the durable plan append must mirror each step as PlanStepAdded"
    );
    // A second failing drive must not double-open the same blocker.
    let outcome = runtime
        .run_turn(session, "write src/a.rs again", &[])
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    let view = handle.ledger_view().unwrap();
    assert_eq!(
        view.head.open_blockers.len(),
        1,
        "the same open reason is never re-opened"
    );
}

#[tokio::test]
async fn typed_ledger_records_routing_decisions_from_the_router_service() {
    // The effective provider/model decision of the RouterService is a
    // durable routing-history entry per turn.
    let (mut adeps, _dir) = deps_with(
        Arc::new(FakeProvider::with_script(
            "paid",
            ModelCapabilities {
                streaming: true,
                tools: true,
                context: 512_000,
                ..Default::default()
            },
            vec![ScriptedResponse::Text("ok".into()), ScriptedResponse::End],
        )),
        vec![],
    );
    let mut registry = ProviderRegistry::new();
    if let Some(f) = adeps.providers.get("paid") {
        registry.try_register(f).unwrap();
    }
    registry
        .try_register(Arc::new(FakeProvider::with_script(
            "cheap",
            ModelCapabilities {
                streaming: true,
                tools: true,
                context: 512_000,
                ..Default::default()
            },
            vec![
                ScriptedResponse::Text("cheap".into()),
                ScriptedResponse::End,
            ],
        )))
        .unwrap();
    let expensive = faktor_core::model::ModelEconomics {
        input_price_per_mtok: faktor_core::model::MicroUsdPerToken::from(15),
        output_price_per_mtok: faktor_core::model::MicroUsdPerToken::from(60),
        coding_reliability: 95,
        tool_reliability: 95,
        ..Default::default()
    };
    let cheap_e = faktor_core::model::ModelEconomics {
        input_price_per_mtok: faktor_core::model::MicroUsdPerToken::from(1),
        output_price_per_mtok: faktor_core::model::MicroUsdPerToken::from(3),
        coding_reliability: 82,
        tool_reliability: 82,
        ..Default::default()
    };
    let mk = |provider: &str, economics: faktor_core::model::ModelEconomics| {
        faktor_core::model::ModelDescriptor {
            provider: provider.into(),
            model: "m".into(),
            context: 512_000,
            max_output: 16_000,
            tools: true,
            parallel_tools: true,
            reasoning: true,
            thinking: true,
            vision: false,
            structured_output: true,
            embeddings: false,
            streaming: true,
            economics,
            source: faktor_core::model::ModelSource::ProviderCatalog,
        }
    };
    adeps.providers = Arc::new(registry);
    adeps.routing = crate::EconomicRoutingPolicy::new(
        Arc::new(faktor_router::RouterService::new(vec![
            mk("paid", expensive),
            mk("cheap", cheap_e),
        ])),
        crate::RoutingMode::Economy,
    );
    let runtime = AgentRuntime::new(adeps).unwrap();
    let ws = runtime.deps().session.create_workspace("/w").unwrap();
    let session = runtime
        .deps()
        .session
        .create_session(ws, "router", "paid", "m")
        .unwrap()
        .id();
    let outcome = runtime.run_turn(session, "hi", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let view = handle.ledger_view().unwrap();
    assert_eq!(view.head.routing_count, 1, "one routed logical turn");
    let routed = view.head.routing_tail.last().unwrap();
    assert_eq!(routed.provider, "cheap", "router's cheapest candidate wins");
    assert_eq!(routed.model, "m");
    assert!(
        !routed.reasoning.is_empty(),
        "router reasoning rides the entry"
    );
    assert!(routed.cost_micro > 0);
    assert_eq!(
        view.head.goal, "router",
        "the routed turn still seeds GoalSet from the session title"
    );
}

#[tokio::test]
async fn change_budget_refuses_out_of_scope_edits_and_clears_to_parity() {
    // Audits 57/105: with a durable ChangeBudget the edit gate refuses a
    // mutating tool whose declared write paths leave `allowed_paths`
    // BEFORE anything executes; the denial is durable and typed. Clearing
    // the budget restores today's behavior for the identical run.
    let (manager, session, dir) = verified_shared_env();
    let h = manager.get_session(session).unwrap().unwrap();
    h.set_change_budget(Some(&faktor_core::state::ChangeBudget {
        allowed_paths: vec!["docs".into()],
        ..Default::default()
    }))
    .unwrap();
    let ok = fake_ok();
    let (turn_deps, _d) = verified_turn_deps(
        &manager,
        vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({
                    "path": "src/allowed.rs",
                    "content": "pub fn allowed() -> u32 {\n    let base: u32 = 41;\n    let step: u32 = 1;\n    base.saturating_add(step).saturating_mul(2)\n}\n",
                }),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        ok.clone(),
        0.65,
    );
    let runtime = AgentRuntime::new(turn_deps).unwrap();
    let o1 = runtime
        .run_turn(session, "write outside", &[])
        .await
        .unwrap();
    drop(runtime);
    assert!(
        !dir.path().join("ws/src/allowed.rs").exists(),
        "the out-of-budget write must be refused BEFORE execution"
    );
    assert_ne!(o1.completion, Some(CompletionGate::VerifiedComplete));
    let h = manager.get_session(session).unwrap().unwrap();
    assert!(h.pending_tool_runs().unwrap().is_empty());
    let events = h.events_range(1, None).unwrap();
    assert!(
        !events
            .iter()
            .any(|e| e.kind == faktor_core::event::EventKind::ToolStarted),
        "no run may start for an out-of-budget edit"
    );
    let denial = events
        .iter()
        .find(|e| e.kind == faktor_core::event::EventKind::PermissionDenied)
        .expect("the change-budget edit gate journals a PermissionDenied");
    let payload = denial.payload.as_ref().expect("denial carries a payload");
    assert_eq!(payload["tool"], "write_file");
    assert!(
        payload["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("change budget refused the edit"),
        "{payload}"
    );

    // Parity: with NO budget the IDENTICAL run executes and the verified
    // gate lands exactly as an unbudgeted session would (fresh
    // environment: the refused turn's session may be mid-recovery).
    let (manager2, session2, dir2) = verified_shared_env();
    let (turn_deps2, _d2) = verified_turn_deps(
        &manager2,
        vec![
            ScriptedResponse::ToolCall {
                id: "c2".into(),
                name: "write_file".into(),
                input: serde_json::json!({
                    "path": "src/allowed.rs",
                    "content": "pub fn allowed() -> u32 {\n    let base: u32 = 41;\n    let step: u32 = 1;\n    base.saturating_add(step).saturating_mul(2)\n}\n",
                }),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        ok,
        0.65,
    );
    let runtime2 = AgentRuntime::new(turn_deps2).unwrap();
    let o2 = runtime2
        .run_turn(session2, "write outside", &[])
        .await
        .unwrap();
    drop(runtime2);
    assert_eq!(
        o2.completion,
        Some(CompletionGate::VerifiedComplete),
        "absent budget = today's behavior"
    );
    assert!(
        dir2.path().join("ws/src/allowed.rs").exists(),
        "the unbudgeted run's write executes"
    );
}

#[tokio::test]
async fn managed_attempt_debits_exactly_once_with_the_hold_visible_pre_dispatch() {
    let recorder = DebitRecorder::new(true);
    let budget = DebitBudget::new(Some(260));
    let (outcome, _runtime) = run_turn_with_debits(&recorder, budget.clone()).await;
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    // Exactly one debit attempt, and its hold was opened BEFORE the
    // provider request was streamed: begin < stream < settle.
    assert_eq!(recorder.begins.load(std::sync::atomic::Ordering::SeqCst), 1);
    let events = recorder.events();
    let begin_at = events
        .iter()
        .position(|e| e.starts_with("begin:"))
        .expect("a begin event");
    let stream_at = events
        .iter()
        .position(|e| e == "stream")
        .expect("a stream event");
    let settle_at = events
        .iter()
        .position(|e| e.starts_with("settle:"))
        .expect("a settle event");
    assert!(
        begin_at < stream_at && stream_at < settle_at,
        "ordering must be begin < stream < settle: {events:?}"
    );
    assert_eq!(
        recorder.settles.lock().unwrap().clone(),
        vec![260],
        "the hold settles at the reservation ledger's actual"
    );
    assert!(recorder.refunds.lock().unwrap().is_empty());
    assert_eq!(budget.refunds.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(budget.settles.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn refused_debit_refuses_the_attempt_before_dispatch() {
    let recorder = DebitRecorder::refusing("insufficient credits");
    let budget = DebitBudget::new(Some(260));
    let (mut deps, _dir) = deps_with(debit_aware_provider(&recorder), vec![]);
    deps.budgets = budget.clone();
    let runtime = AgentRuntime::new(deps).unwrap();
    runtime
        .set_provider_debits(Some(recorder.clone() as Arc<dyn ProviderAttemptDebits>))
        .expect("install debit authority");
    let (manager, session) = shared_session(runtime.deps());
    let _ = manager;
    let outcome = runtime
        .run_turn(session, "do the thing", &[])
        .await
        .expect("turn");
    assert_eq!(outcome.final_state, AgentState::FailedRecoverable);
    let events = recorder.events();
    assert!(
        !events.iter().any(|e| e == "stream"),
        "a refused debit must never dispatch: {events:?}"
    );
    assert!(recorder.refunds.lock().unwrap().is_empty());
    // The budget reservation was released (pre-dispatch refund).
    assert_eq!(budget.refunds.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn byok_attempt_never_debits_and_disabled_daemon_is_parity() {
    // BYOK: the authority is consulted (config-derived classification)
    // but the attempt records usage only — no hold, no settle, no refund.
    let byok = DebitRecorder::new(false);
    let budget = DebitBudget::new(Some(260));
    let (outcome, _runtime) = run_turn_with_debits(&byok, budget).await;
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(byok.begins.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(byok.settles.lock().unwrap().is_empty());
    assert!(byok.refunds.lock().unwrap().is_empty());
    let events = byok.events();
    assert_eq!(
        events.len(),
        2,
        "BYOK: classification + stream only: {events:?}"
    );
    assert!(events[0].starts_with("begin:") && events[1] == "stream");

    // Billing disabled (`None` authority): the provider path is the
    // pre-billing path exactly — no debit call exists at all.
    let (mut deps, _dir) = deps_with(
        Arc::new(FakeProvider::with_script(
            "fake",
            ModelCapabilities {
                tools: true,
                context: 200_000,
                ..Default::default()
            },
            vec![ScriptedResponse::Text("ok".into()), ScriptedResponse::End],
        )),
        vec![],
    );
    deps.budgets = DebitBudget::new(Some(260));
    let runtime = AgentRuntime::new(deps).unwrap();
    let (manager, session) = shared_session(runtime.deps());
    let _ = manager;
    let outcome = runtime
        .run_turn(session, "do the thing", &[])
        .await
        .expect("turn");
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
}

#[tokio::test]
async fn poisoned_debit_authority_refuses_typed_before_dispatch_and_never_reads_as_disabled() {
    // P0 monetary fail-open: `provider_debits()` used to map a poisoned
    // authority lock to `None`, which means "billing deliberately
    // disabled" — a Faktor-managed provider request would then dispatch
    // with no commercial debit authority (a free managed call after a
    // poisoning). Retrieval and the setter must surface the poison typed,
    // and the attempt must be refused BEFORE any provider dispatch.
    let recorder = DebitRecorder::new(true);
    let budget = DebitBudget::new(Some(260));
    let (mut deps, _dir) = deps_with(debit_aware_provider(&recorder), vec![]);
    deps.budgets = budget.clone();
    let runtime = AgentRuntime::new(deps).unwrap();
    runtime
        .set_provider_debits(Some(recorder.clone() as Arc<dyn ProviderAttemptDebits>))
        .expect("install debit authority");

    // The only way to poison a std::sync::Mutex: panic while it is held.
    let poisoner = runtime.clone();
    let _ = std::thread::spawn(move || {
        let _guard = poisoner.provider_debits.lock().unwrap();
        panic!("poison the provider debit authority slot");
    })
    .join();

    // Retrieval is fallible: a poisoned lock is `Unavailable`, NEVER a
    // silent `None`.
    match runtime.provider_debits() {
        Err(DebitError::Unavailable { reason }) => {
            assert!(reason.contains("poisoned"), "{reason}");
        }
        Err(other) => panic!("poisoned authority must be Unavailable, got {other:?}"),
        Ok(_) => panic!("a poisoned authority must never read as an authority slot"),
    }

    // The per-attempt debit machine is refused typed before any dispatch.
    let attempt = ModelCallAttempt::new(OpId::new(7), OpId::new(8), 0).unwrap();
    let attempt_err = runtime
        .attempt_debits(SessionId::new(1), TaskId::new(1), "fake", "m", attempt, 10)
        .expect_err("a poisoned authority must refuse the debit machine");
    assert!(
        matches!(attempt_err, DebitError::Unavailable { .. }),
        "{attempt_err:?}"
    );

    // The SETTER is typed too: a poisoned slot is never a silent ignore.
    let setter_err = runtime
        .set_provider_debits(None)
        .expect_err("a poisoned authority slot must refuse the setter");
    assert!(
        matches!(setter_err, DebitError::Unavailable { .. }),
        "{setter_err:?}"
    );

    // End to end: a managed turn with a poisoned authority is refused
    // pre-dispatch. The provider is invoked ZERO times and the budget
    // reservation is released.
    let (manager, session) = shared_session(runtime.deps());
    let _ = manager;
    let outcome = runtime
        .run_turn(session, "do the thing", &[])
        .await
        .expect("the refusal is a classified turn end, not a hard error");
    assert_eq!(outcome.final_state, AgentState::FailedRecoverable);
    let detail = outcome
        .stop_reason
        .as_ref()
        .expect("a typed refusal reason")
        .detail
        .clone();
    assert!(
        detail.contains("provider debit authority lock poisoned"),
        "{detail}"
    );
    let events = recorder.events();
    assert!(
        !events.iter().any(|event| event == "stream"),
        "a poisoned authority must never dispatch: {events:?}"
    );
    assert_eq!(
        recorder.begins.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no debit hold exists without the authority"
    );
    assert_eq!(
        budget.refunds.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the pre-dispatch refusal releases the reservation"
    );

    // Billing deliberately disabled (`None` on a CLEAN slot) still takes
    // the documented pre-billing path byte-for-byte.
    let (mut clean_deps, _dir2) = deps_with(
        Arc::new(FakeProvider::with_script(
            "fake",
            ModelCapabilities {
                tools: true,
                context: 200_000,
                ..Default::default()
            },
            vec![ScriptedResponse::Text("ok".into()), ScriptedResponse::End],
        )),
        vec![],
    );
    clean_deps.budgets = DebitBudget::new(Some(260));
    let clean_runtime = AgentRuntime::new(clean_deps).unwrap();
    clean_runtime
        .set_provider_debits(None)
        .expect("a clean slot accepts None");
    let (manager, session) = shared_session(clean_runtime.deps());
    let _ = manager;
    let outcome = clean_runtime
        .run_turn(session, "do the thing", &[])
        .await
        .expect("turn");
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
}

/// F6 (adversarial): marker files are created with `create_new` and a
/// pid+random name tail — an existing marker is NEVER overwritten, even
/// when the base name is identical (two processes sharing a store root).
#[test]
fn marker_files_never_overwrite_on_name_collision() {
    let dir = tempfile::tempdir().unwrap();
    let first = write_marker_file(dir.path(), "dw-base", b"one").unwrap();
    let second = write_marker_file(dir.path(), "dw-base", b"two").unwrap();
    assert_ne!(
        first, second,
        "a colliding marker name must never overwrite the existing file"
    );
    assert_eq!(std::fs::read(&first).unwrap(), b"one");
    assert_eq!(std::fs::read(&second).unwrap(), b"two");
    // The production name tail varies per call even at the same
    // millisecond (pid + RandomState-seeded tag).
    assert_ne!(marker_random_tag(42, 1), marker_random_tag(42, 1));
}

/// F11 (adversarial): markers are handled PER STATE — terminal states are
/// consumed, unknown states and corrupt files are surfaced and retained,
/// pending markers replay — and the directory is bounded by consuming
/// the oldest terminal markers while pending work is NEVER deleted.
#[tokio::test]
async fn marker_states_are_handled_and_the_directory_is_bounded() {
    let (deps, _dir) = deps(scripted_provider(vec![]), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let manager = runtime.deps().session.clone();
    let session = new_session(runtime.deps());
    let handle = manager.get_session(session).unwrap().unwrap();
    let root = manager.store().root().to_path_buf();
    let marker_dir = root.join(DURABLE_WRITE_MARKER_DIR);
    std::fs::create_dir_all(&marker_dir).unwrap();

    // Corrupt file: retained (never silently deleted or replayed).
    std::fs::write(marker_dir.join("dw-0-corrupt.json"), b"{not json").unwrap();
    // Unknown status: retained, never replayed.
    write_raw_marker(
        &marker_dir,
        "dw-1-unknown.json",
        &serde_json::json!({
            "status": "weird",
            "session": session.raw(),
            "at_ms": 1,
            "intent": {"write": "reset_loop_signals"},
        }),
    );
    // Terminal status: consumed, never replayed.
    write_raw_marker(
        &marker_dir,
        "dw-2-applied.json",
        &serde_json::json!({
            "status": "applied",
            "session": session.raw(),
            "at_ms": 2,
            "intent": {"write": "reset_loop_signals"},
        }),
    );
    runtime.replay_durable_write_failures(&handle);
    let names: Vec<String> = marker_files(&root)
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert!(
        names.iter().any(|n| n.contains("corrupt")),
        "a corrupt marker is retained: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("unknown")),
        "an unknown status is retained: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("applied")),
        "a terminal marker is consumed: {names:?}"
    );

    // Directory bound: with the bound overridden to 2, GC consumes the
    // oldest terminal marker, keeps every pending marker (durable work),
    // and is loud afterwards.
    DURABLE_WRITE_MARKER_DIR_MAX_OVERRIDE.store(2, std::sync::atomic::Ordering::Relaxed);
    for i in 0..3 {
        write_raw_marker(
            &marker_dir,
            &format!("dw-pending-{i}.json"),
            &serde_json::json!({
                "status": "pending",
                "session": session.raw(),
                "at_ms": 10 + i,
                "attempts": 0,
                "site": "test",
                "intent": {"write": "reset_loop_signals"},
            }),
        );
    }
    gc_durable_write_markers(&marker_dir);
    DURABLE_WRITE_MARKER_DIR_MAX_OVERRIDE.store(0, std::sync::atomic::Ordering::Relaxed);
    let files = marker_files(&root);
    let pending_left = files
        .iter()
        .filter(|p| p.file_name().unwrap().to_string_lossy().contains("pending"))
        .count();
    assert_eq!(
        pending_left, 3,
        "pending markers are never deleted by the directory GC: {:?}",
        files
    );
}

#[tokio::test]
async fn corrupt_task_ledger_row_is_marked_and_reconstructed_on_reopen() {
    // Adversarial (externally corrupted durable row): an undecodable
    // legacy `task_ledger` row must NEVER fall back to a silent default.
    // The decode failure surfaces typed, a durable retry-on-next-open
    // marker (plus the CrashDetected audit event) records it, and the
    // next open reconstructs the row from the typed durable authorities.
    let (deps, dir) = deps(scripted_provider(vec![ScriptedResponse::End]), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let manager = runtime.deps().session.clone();
    let session = new_session(runtime.deps());
    let h = manager.get_session(session).unwrap().unwrap();
    // Durable authority to reconstruct from: the typed ledger head goal.
    h.ledger_goal_set("rebuilt from the typed ledger").unwrap();
    // The adversary replaces the legacy blob with a JSON array (decodes
    // as `TaskLedger` never).
    h.put_task_ledger(serde_json::json!(["not", "a", "ledger"]))
        .unwrap();
    let err = runtime
        .load_ledger(&h)
        .expect_err("a corrupt ledger row is a surfaced failure, never a silent default");
    assert_eq!(err.kind, faktor_core::ErrorKind::Malformed, "{err:?}");
    let root = manager.store().root().to_path_buf();
    assert_eq!(
        marker_sites(&root),
        vec![DW_SITE_LEDGER_REBUILD.to_string()],
        "exactly the lost ledger decode is compensated"
    );
    let marker = marker_json(&marker_files(&root)[0]);
    assert_eq!(marker["status"], "pending");
    assert_eq!(marker["intent"]["write"], "rebuild_task_ledger");
    assert_eq!(marker["session"], serde_json::json!(session.raw()));
    let audit = h.events_range(1, Some(512)).unwrap();
    assert!(
        audit.iter().any(|e| {
            e.kind == faktor_core::event::EventKind::CrashDetected
                && e.payload
                    .as_ref()
                    .and_then(|p| p.get("durable_write_failure"))
                    .and_then(|f| f.get("site"))
                    .and_then(|s| s.as_str())
                    == Some(DW_SITE_LEDGER_REBUILD)
        }),
        "the durable audit event must name the corrupt ledger site: {audit:?}"
    );
    // The raw corrupt blob is still what the store holds until replay
    // (the failure was NOT papered over with a defaulted ledger).
    assert!(serde_json::from_value::<TaskLedger>(h.get_task_ledger().unwrap().unwrap()).is_err());
    drop(runtime);
    drop(manager);
    let manager2 = reopen_manager(&dir);
    let (deps2, _d2) = deps_sharing_session(
        manager2.clone(),
        Arc::new(scripted_provider(vec![ScriptedResponse::End])),
        vec![],
    );
    AgentRuntime::new(deps2).unwrap().recover().unwrap();
    let h2 = manager2.get_session(session).unwrap().unwrap();
    let ledger: TaskLedger = serde_json::from_value(h2.get_task_ledger().unwrap().unwrap())
        .expect("recovery must reconstruct a decodable ledger row");
    assert_eq!(ledger.goal, "rebuilt from the typed ledger");
    assert!(
        marker_files(manager2.store().root()).is_empty(),
        "the consumed marker is removed after reconstruction"
    );
}

/// Audit item 3 negative coverage: a context-compression summary or an
/// ephemeral title can NEVER write a completion fact or immutable task
/// fact — the writer refuses the trust class typed and NOTHING reaches the
/// store — while an Implementation output keeps its existing authority.
#[tokio::test]
async fn compression_and_ephemeral_outputs_cannot_write_completion_facts() {
    let (deps_, _dir) = deps(scripted_provider(vec![ScriptedResponse::End]), vec![]);
    let runtime = AgentRuntime::new(deps_).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let compaction = ModelOutput::new("summary", OutputTrust::ContextCompression, 31);
    let ephemeral = ModelOutput::new("title", OutputTrust::Ephemeral, 32);

    for (i, output) in [&compaction, &ephemeral].iter().enumerate() {
        let propagated = runtime
            .guarded_upsert_memory_fact(
                &handle,
                FactSource::Model(output),
                "task_state",
                &format!("refused_{i}"),
                "{}",
                "test.trust_gate",
            )
            .unwrap_err();
        assert_eq!(propagated.kind, faktor_core::error::ErrorKind::Permission);
        runtime.dw_note_upsert_memory_fact(
            &handle,
            FactSource::Model(output),
            "task_state",
            &format!("refused_note_{i}"),
            "{}",
            "test.trust_gate",
        );
    }
    let facts = handle.memory_facts().unwrap();
    assert!(
        !facts.iter().any(|(k, key, _)| k == "task_state"
            && (key.starts_with("refused_") || key.starts_with("refused_note_"))),
        "a refused trust class must leave NO durable row: {facts:?}"
    );

    // Existing behavior is unchanged: an Implementation output may author a
    // completion fact, and a durable projection still writes.
    let implementation = ModelOutput::new("code", OutputTrust::Implementation, 33);
    runtime
        .guarded_upsert_memory_fact(
            &handle,
            FactSource::Model(&implementation),
            "task_state",
            "written",
            "{\"ok\":true}",
            "test.trust_gate",
        )
        .unwrap();
    runtime
        .guarded_upsert_memory_fact(
            &handle,
            FactSource::Durable,
            "task_state",
            "durable",
            "{}",
            "test.trust_gate",
        )
        .unwrap();
    let facts = handle.memory_facts().unwrap();
    assert!(facts
        .iter()
        .any(|(k, key, v)| k == "task_state" && key == "written" && v == "{\"ok\":true}"));
    assert!(facts
        .iter()
        .any(|(k, key, _)| k == "task_state" && key == "durable"));

    // Provenance is the ONLY durable write a compression output may make:
    // the compression author is admitted there, while an implementation
    // output (not a compression artifact) is refused and writes nothing.
    runtime.dw_note_upsert_provenance_fact(
        &handle,
        FactSource::Model(&implementation),
        "compaction",
        "provenance_refused",
        "{}",
        "test.trust_gate",
    );
    let facts = handle.memory_facts().unwrap();
    assert!(
        !facts
            .iter()
            .any(|(k, key, _)| k == "compaction" && key == "provenance_refused"),
        "a non-compression author must not write provenance: {facts:?}"
    );
    runtime.dw_note_upsert_provenance_fact(
        &handle,
        FactSource::Model(&compaction),
        "compaction",
        "provenance_admitted",
        "{\"provenance\":\"context_compression\"}",
        "test.trust_gate",
    );
    let facts = handle.memory_facts().unwrap();
    assert!(facts
        .iter()
        .any(|(k, key, _)| k == "compaction" && key == "provenance_admitted"));
}
