//! `runtime::compaction`: cohesive slice of the agent runtime.

use super::*;

/// The compaction model's dedicated system contract (P0 audit, round 11):
/// the summarizer is NOT the agent — it is the Faktor context compactor
/// producing a faithful state transfer. Sending the agent instructions as
/// the system prompt let the compaction model answer the latest user message
/// instead of summarizing. The agent instructions must stay out of this
/// request entirely.
pub(crate) const COMPACTOR_SYSTEM: &str =
    "You are the Faktor context compactor. Your ONLY job is to \
produce a faithful state transfer that REPLACES the conversation below, so the agent can \
continue exactly where it stopped. The prior conversation is given as user/assistant \
messages. Write a compact but complete summary that preserves: the user's goal and current \
task; constraints and requirements; decisions made and their reasons; files changed (paths \
and what changed); unresolved errors and blockers (NEVER omit an unresolved blocker); \
results of tests and verification; observable tool effects (commands run, artifacts \
created); explicit user instructions and preferences; the current implementation state; \
and the next actions to take. NEVER invent facts, code, paths, or results that are not in \
the transcript; if the transcript is incomplete, say exactly what is missing. Prefer \
structured output (short labeled sections). Do not answer the latest user message: do not \
add advice, do not continue the task, do not write code.";

/// Fallback bound of the child-park wait (drive boundary) when the operator
/// configured an unbounded (`0`) wall-clock turn budget. A parked child is
/// re-driven by the executor on a typed timeout; it is NEVER polled forever.
pub(crate) const MAX_CHILD_PARK_WAIT: Duration = Duration::from_secs(30 * 60);

/// Fallback bound of one queue-runner wait (interrupted turn not yet
/// continuable / admission still declined) when the turn budget is `0`. The
/// durable queue head stays pending and a re-kick resumes it; the runner
/// itself never polls forever.
pub(crate) const MAX_QUEUE_WAIT: Duration = Duration::from_secs(30 * 60);

/// One bounded, guarded semantic consult of the turn (audits 54/58/77): ONLY
/// a registered provider covering [`SemanticCapabilities::AFFECTED`] is
/// consulted — the generic fallback is never asked, which is what keeps a
/// provider-less runtime byte-identical (parity). The provider's affected
/// set is rendered as provenance-tagged DATA and its entity paths become the
/// conservative risk assessment. Every failure (typed error, provider
/// crash/panic, timeout/park) degrades to Unknown risk and empty evidence —
/// never a failed or stalled turn.
pub(crate) async fn semantic_turn_consult(
    deps: &AgentDeps,
    handle: &faktor_session::SessionHandle,
    changed: &[String],
    cancel: &CancellationToken,
) -> Option<SemanticTurnState> {
    // Descriptor handshake + validated selection: an external provider is
    // only consulted once its descriptor is fetched and validated, and a
    // cooling (recently failing) provider is skipped rather than re-polled.
    let selection = deps
        .semantic
        .select_validated(
            &SemanticCapabilities::AFFECTED,
            SemanticOp::Affected,
            cancel,
        )
        .await;
    let provider = match selection {
        SemanticSelection::Provider(provider) => provider,
        // No registered provider: today's behavior exactly (parity).
        SemanticSelection::Fallback(_) => return None,
    };
    let provider_id = provider.id();
    let Ok(row) = handle.row() else {
        return Some(SemanticTurnState::unknown(provider_id.to_string()));
    };
    let workspace = row.workspace_id;
    let revision = format!("session:{}", handle.id().raw());
    let snapshot_id = SemanticSnapshotId::derive(
        workspace,
        &revision,
        &provider_id,
        provider.version(),
        SEMANTIC_SCHEMA_VERSION,
    );
    let changed_refs: Vec<SemanticEntityRef> = changed
        .iter()
        .filter_map(|path| {
            let path = WorkspacePath::parse(path).ok()?;
            let entity_id = semantic_entity_id_for(path.as_str())?;
            Some(SemanticEntityRef::new(workspace, path, entity_id))
        })
        .collect();
    // The consult is best-effort observability: an op-id allocation failure
    // degrades to the same Unknown risk as any other consult failure, never
    // a fabricated zero id and never a failed turn.
    let op_id = match deps.session.try_next_op_id() {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "op-id allocation failed; semantic consult degraded to unknown"
            );
            return Some(SemanticTurnState::unknown(provider_id.to_string()));
        }
    };
    let call = SemanticCall::new(
        op_id,
        handle.id(),
        workspace,
        deps.clock.now_ms(),
        cancel.child(),
    );
    let request = AffectedRequest {
        call,
        workspace,
        snapshot_id,
        changed: changed_refs,
        max_depth: 2,
    };
    // The registry guards cancellation, deadline and provider panics; the
    // outer wall budget bounds a parked provider (drop the future).
    let envelope = match tokio::time::timeout(
        SEMANTIC_CONSULT_MAX_WAIT,
        deps.semantic.affected(request),
    )
    .await
    {
        Ok(Ok(envelope)) => envelope,
        // Caller cancellation: the turn is ending; no data, no escalation.
        Ok(Err(err)) if err.caller_terminal() => return None,
        Ok(Err(err)) => {
            tracing::warn!(provider = %provider_id, "semantic consult failed: {err}; risk is Unknown");
            return Some(SemanticTurnState::unknown(provider_id.to_string()));
        }
        Err(_) => {
            tracing::warn!(
                provider = %provider_id,
                "semantic consult exceeded the {SEMANTIC_CONSULT_MAX_WAIT:?} wall budget; risk is Unknown"
            );
            return Some(SemanticTurnState::unknown(provider_id.to_string()));
        }
    };
    if envelope.provider_id.as_str() == GENERIC_FALLBACK_ID {
        // The registered provider failed and the registry degraded to the
        // fallback: we have no real provider evidence — Unknown, never Safe.
        tracing::warn!(provider = %provider_id, "semantic consult degraded to the generic fallback; risk is Unknown");
        return Some(SemanticTurnState::unknown(provider_id.to_string()));
    }
    let payload_text = match serde_json::to_string(&envelope.payload) {
        Ok(text) => text,
        Err(_) => return Some(SemanticTurnState::unknown(provider_id.to_string())),
    };
    let bounded = truncate(&payload_text, SEMANTIC_EVIDENCE_MAX_CHARS);
    let rendered = match envelope.render_data(&bounded) {
        Ok(block) => block,
        Err(err) => {
            tracing::warn!(provider = %provider_id, "semantic evidence refused as DATA: {err}");
            return Some(SemanticTurnState::unknown(provider_id.to_string()));
        }
    };
    let degraded = envelope.payload.degraded;
    let paths: Vec<String> = envelope
        .payload
        .affected
        .iter()
        .chain(envelope.payload.tests.iter())
        .map(|entity| entity.path.as_str().to_string())
        .collect();
    let risk = semantic_risk_from_paths(&paths, degraded);
    let level = semantic_risk_level(&risk, degraded);
    let restrictions = semantic_capability_restrictions(&risk);
    tracing::debug!(
        provider = %provider_id,
        level = ?level,
        affected = paths.len(),
        "semantic consult completed"
    );
    Some(SemanticTurnState {
        evidence: vec![Evidence {
            path: format!("semantic://{provider_id}"),
            snippet: rendered,
            // Deterministic mid-rank so provider DATA never displaces the
            // repository's own retrieved evidence.
            score: 0.5,
        }],
        level,
        risk,
        restrictions,
        provider: provider_id.to_string(),
    })
}

impl Summarizer for LedgerSummarizer {
    fn summarize<'a>(
        &'a self,
        _residue: &'a [faktor_context::RecentTurn],
        durable_facts: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>> {
        // The weak deterministic summarizer echoes ONLY the durable facts
        // render (never the goal/criteria: those never enter summarizer
        // input — audit 70).
        Box::pin(async move { durable_facts.to_string() })
    }
}

/// Default bound on one compaction-model summary stream (spec §9): a
/// summarizer that does not finish cleanly inside the bound is treated as
/// failed and its partial text is discarded. Per-instance injectable so the
/// timeout path is testable without waiting 90s.
pub(crate) const DEFAULT_SUMMARY_TIMEOUT: Duration = Duration::from_secs(90);

/// The attempt-keyed identity of ONE budgeted compaction summarizer call.
#[derive(Clone)]
pub(crate) struct CompactCallTrace {
    pub(crate) attempt: ModelCallAttempt,
    pub(crate) reservation: Option<faktor_session::ReservationId>,
    pub(crate) provider: String,
    pub(crate) model: String,
}

/// The reservation's attempt machine a budgeted provider stream marks
/// dispatched immediately before its request is sent (P0-2 + attempt-
/// accounting audit; see
/// [`faktor_session::BudgetAuthority::mark_dispatched`]). The machine is
/// SHARED with the caller (the compaction site decides the terminal state
/// — settle / UNCERTAIN / refund — from the machine's guarded state after
/// the opaque `Summarizer` run, so a post-dispatch refund can never be
/// issued and a never-dispatched failure can never go UNCERTAIN).
#[derive(Clone)]
pub(crate) struct BudgetDispatchMarker {
    pub(crate) machine: Arc<tokio::sync::Mutex<crate::AttemptAccounting>>,
}

/// The failure fallback returned when the streaming summarizer produced no
/// summary (provider error, deadline, cancellation, non-streaming model).
/// An EMPTY string would be a data-loss hole: the compactor's token
/// estimate of "" is 0, which always passes its hard cap, so a wiped
/// history would be "accepted" as an LLM summary. Instead the unsummarizable
/// transcript is echoed verbatim — every character is real, nothing is
/// invented — and repeated 3× so its estimate (chars/4) provably exceeds
/// the compactor's hard cap (at most 3/4 of the byte-based `before` figure;
/// chars ≤ bytes, so 3×(chars/4) + prefixes > 3/4×before holds for ANY
/// UTF-8 input, multibyte included). The compactor therefore REJECTS it and
/// deterministic pruning takes over — the documented degradation path.
pub(crate) fn summarize_failure_fallback(history: &[RecentTurn]) -> String {
    const FALLBACK_COPIES: usize = 3;
    let mut out = String::new();
    for _ in 0..FALLBACK_COPIES {
        for turn in history {
            out.push_str(&format!("{}: {}\n", turn.role, turn.text));
        }
    }
    out
}

impl Summarizer for StreamingSummarizer {
    fn summarize<'a>(
        &'a self,
        residue: &'a [faktor_context::RecentTurn],
        _durable_facts: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>> {
        // The streaming model summarizes the POST-EXTRACTION residue only.
        // The durable facts (and the verbatim goal/criteria) are never part
        // of its transcript — a lossy model cannot rewrite them (audit 70).
        Box::pin(async move {
            self.run(residue)
                .await
                .unwrap_or_else(|| summarize_failure_fallback(residue))
        })
    }
}

pub(crate) fn tool_mode_tag(mode: ToolCallMode) -> &'static str {
    match mode {
        ToolCallMode::Native => "native",
        ToolCallMode::NativeWithRepair => "native_with_repair",
        ToolCallMode::StructuredFallback => "structured_fallback",
    }
}

/// True when the turn made genuine progress: nothing failed, files were
/// applied, or tests passed. Text-only turns count (no failures).
pub(crate) fn turn_made_progress(summary: &faktor_context::ledger::TurnSummary) -> bool {
    if summary.failures.is_empty() {
        return true;
    }
    if !summary.files_changed.is_empty() {
        return true;
    }
    if !summary.tests_run.is_empty() && summary.tests_failed.is_empty() {
        return true;
    }
    false
}

/// Fold one completed tool call into the logical-turn summary with REAL
/// data (audit: only TurnSummary::default() was recorded; the tool NAME was
/// even journaled as a changed file).
pub(crate) fn collect_tool_summary(
    summary: &mut faktor_context::ledger::TurnSummary,
    name: &str,
    input: &serde_json::Value,
    outcome: &ToolOutcome,
) {
    // Step description: name + the primary path/command argument.
    let path = input
        .get("path")
        .or_else(|| input.get("file"))
        .or_else(|| input.get("filename"))
        .and_then(|p| p.as_str());
    let command = input.get("command").and_then(|c| c.as_str());
    let step = match (path, command) {
        (Some(p), _) if !p.is_empty() => format!("{name} ({p})"),
        (_, Some(c)) if !c.is_empty() => format!("{name}: {}", truncate(c, 120)),
        _ => name.to_string(),
    };
    if !step.is_empty() {
        summary.steps_completed.push(truncate(&step, 200));
    }
    // Changed files: a write tool's target (from its input) when the tool
    // completed — real paths, never the tool name.
    if outcome.effect_status == faktor_core::op::EffectStatus::Applied
        || outcome.exit_code == Some(0)
    {
        if let Some(p) = path.filter(|p| !p.is_empty()) {
            let p = truncate(p, 300);
            if !summary.files_changed.contains(&p) {
                summary.files_changed.push(p);
            }
        }
    }
    // Failures: non-zero exit or errored effect.
    if outcome.exit_code.is_some_and(|c| c != 0)
        || outcome.effect_status == faktor_core::op::EffectStatus::Failed
    {
        let msg = truncate(&outcome.text, 300);
        let failure = if msg.is_empty() {
            format!("{name} failed (exit {})", outcome.exit_code.unwrap_or(-1))
        } else {
            format!("{name}: {msg}")
        };
        summary.failures.push(truncate(&failure, 400));
    }
    // Tests: test-running commands recorded as run/failed with their real
    // exit status.
    if name == "run_command" || name == "run_command_on_files" {
        if let Some(c) = command {
            if looks_like_test_command(c) {
                let cmd = truncate(c, 200);
                if !summary.tests_run.contains(&cmd) {
                    summary.tests_run.push(cmd.clone());
                }
                if outcome.exit_code.is_some_and(|e| e != 0) && !summary.tests_failed.contains(&cmd)
                {
                    summary.tests_failed.push(cmd);
                }
            }
        }
    }
}

impl AgentRuntime {
    /// The production context compiler: `Some` only when the
    /// `semantic_context` (information-gain) flag is on. `None` keeps the
    /// caller on the producer evidence byte-for-byte (documented off-switch
    /// / unit parity). The compiler runs over the SAME durable authority and
    /// the installed failure-learning prior (the prior is consulted only
    /// when `failure_learning` is on too).
    pub(crate) fn context_compiler(&self) -> Option<ContextCompiler> {
        if !self.deps.efficiency.semantic_context {
            return None;
        }
        let learning = if self.deps.efficiency.failure_learning {
            self.deps.context_prior.clone()
        } else {
            None
        };
        Some(ContextCompiler::new(
            Some(self.evidence_authority.clone()),
            learning,
        ))
    }

    /// Resolve the configured compaction model ("model" uses the session's
    /// provider; "provider/model" names another registered provider).
    pub(crate) fn resolve_compaction_model(
        &self,
        handle: &faktor_session::SessionHandle,
        spec: &str,
    ) -> faktor_core::Result<(Arc<dyn faktor_provider::Provider>, String)> {
        let provider_id = match spec.split_once('/') {
            Some((p, _)) => p.to_string(),
            None => handle.provider()?,
        };
        let model = match spec.split_once('/') {
            Some((_, m)) => m.to_string(),
            None => spec.to_string(),
        };
        if model.is_empty() || model.len() > 256 || provider_id.len() > 256 {
            return Err(Error::malformed("invalid compaction model spec"));
        }
        let provider = self
            .deps
            .providers
            .get(&provider_id)
            .ok_or_else(|| Error::not_found(format!("compaction provider {provider_id}")))?;
        Ok((provider, model))
    }

    pub(crate) async fn try_compact(
        &self,
        handle: &faktor_session::SessionHandle,
        recent: &[RecentTurn],
        ledger: &TaskLedger,
        budget: &ContextBudget,
        // The LOGICAL TURN's cancellation token: a user Stop during
        // compaction must reach the compaction model's stream (P0 audit,
        // round 11 — the summary request used to mint an orphan token and
        // ran up to the full 90s after the turn was cancelled).
        cancel: &CancellationToken,
    ) -> faktor_core::Result<Option<CompactionPlan>> {
        let before = recent.iter().map(|t| t.text.len()).sum::<usize>() / 4;
        if before == 0 {
            return Ok(None);
        }
        let target = budget.context_max();
        // Compaction-model selection (P0-2 phase mapping): an EXPLICIT
        // compaction_model config ("model" or "provider/model") is honored
        // verbatim (spec §36 — explicit config wins over routing). Without
        // one, the ECONOMIC policy is consulted for the Compact phase: in
        // Economy mode the router picks the cheapest compaction-capable
        // model; in Pinned mode the pin is validated for compaction; the
        // passthrough pin (empty decision) keeps the deterministic ledger
        // summarizer (today's no-model default). Routing failures follow the
        // fail-closed matrix: only RouterUnavailable may degrade to the
        // ledger summarizer (warned); every other refusal is a typed error
        // on the turn — compaction never silently substitutes a model.
        // Audit item 3: the accepted summarizer text is captured here as a
        // typed context-compression output so the provenance row below can
        // name its trust class + durable call id.
        let compaction_output_slot: Arc<std::sync::Mutex<Option<ModelOutput>>> =
            Arc::new(std::sync::Mutex::new(None));
        let (summarizer, _reservation, machine, trace): BudgetedSummarizer = if let Some(model) =
            self.deps.compaction_model.as_deref()
        {
            let built = match self.resolve_compaction_model(handle, model) {
                Ok((provider, model_name)) => Some(StreamingSummarizer {
                    provider,
                    model: model_name,
                    // The summarizer runs under the compactor contract —
                    // NEVER the agent instructions (P0 audit round 11).
                    op_id: self.deps.session.try_next_op_id()?,
                    session_id: handle.id(),
                    cancellation: cancel.child(),
                    summary_timeout: DEFAULT_SUMMARY_TIMEOUT,
                    budget_marker: None,
                    output_slot: compaction_output_slot.clone(),
                }),
                Err(e) => {
                    tracing::warn!(
                        "compaction model {model:?} unresolvable: {e}; using the ledger summarizer"
                    );
                    None
                }
            };
            let (summarizer, reservation, machine, trace) = self
                .budgeted_summarizer(handle, built, before, None)
                .await?;
            (summarizer, reservation, machine, trace)
        } else {
            // No explicit compaction model: route the Compact phase on the
            // REAL summarizer dimensions (the transcript to exchange = the
            // pre-compaction history estimate, the summary output cap).
            let intent = crate::ModelCallIntent::compact();
            let req = intent.route_request(before.max(4096) as u64, 4096, 0);
            match self.deps.routing.route(&req) {
                Ok(d) if d.provider.is_empty() && d.model.is_empty() => (None, None, None, None),
                Ok(d) => {
                    let built = match self.deps.providers.get(&d.provider) {
                        Some(p) => Some(StreamingSummarizer {
                            provider: p,
                            model: d.model.clone(),
                            op_id: self.deps.session.try_next_op_id()?,
                            session_id: handle.id(),
                            cancellation: cancel.child(),
                            summary_timeout: DEFAULT_SUMMARY_TIMEOUT,
                            budget_marker: None,
                            output_slot: compaction_output_slot.clone(),
                        }),
                        None => {
                            return Err(Error::new(
                                    ErrorKind::Internal,
                                    format!(
                                        "routing chose compaction provider {:?} which is not registered",
                                        d.provider
                                    ),
                                ));
                        }
                    };
                    // P0-1: the routed compaction call freezes the route
                    // decision's price capture on its reservation.
                    self.budgeted_summarizer(handle, built, before, d.pricing_snapshot)
                        .await?
                }
                Err(f) => {
                    // Fail closed: no model may compact, and the turn must
                    // not silently degrade past a typed denial — there is
                    // no RouterUnavailable fallback to the ledger
                    // summarizer anymore.
                    return Err(Error::new(
                        ErrorKind::Conflict,
                        format!("routing refused the compaction call: {f:?}"),
                    ));
                }
            }
        };
        let compactor: Compactor = match summarizer {
            Some(s) => Compactor::new(Some(s)),
            None => Compactor::new(Some(Arc::new(LedgerSummarizer))),
        };
        // Audit 70/71: compaction consumes a READ-ONLY projection built from
        // the durable rows — goal/criteria/plan/state from the typed Task
        // row, decisions + child progress from the typed session ledger,
        // checks from the VerificationRecord rows. The transcript can lie;
        // these rows cannot. (The projection is not written back: compaction
        // has no ledger write authority.)
        let projection = {
            use faktor_context::ledger::{
                DurableTaskRows, ProjectedCheck, ProjectedChild, ProjectedDecision,
                TaskContextProjection,
            };
            let task_id = handle.task_id()?;
            let durable_task = handle.get_task(task_id)?;
            let head = handle.ledger_ensure_head()?;
            let records = handle
                .list_verification_records(task_id)
                .map_err(faktor_core::Error::from)?;
            let checks: Vec<ProjectedCheck> = match records.last() {
                Some(record) => record
                    .checks
                    .iter()
                    .map(|c| ProjectedCheck {
                        name: c.check.clone(),
                        status: format!("{:?}", c.status).to_lowercase(),
                        summary: c.summary.clone().unwrap_or_default(),
                    })
                    .collect(),
                None => head
                    .last_verify
                    .as_ref()
                    .map(|v| {
                        v.checks
                            .iter()
                            .map(|c| ProjectedCheck {
                                name: c.id.clone(),
                                status: if c.passed { "passed" } else { "failed" }.to_string(),
                                summary: String::new(),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            };
            let decisions = head
                .decisions
                .iter()
                .map(|d| ProjectedDecision {
                    step: d.step.clone(),
                    choice: d.choice.clone(),
                    rationale: d.rationale.clone(),
                })
                .collect();
            let children = head
                .children
                .iter()
                .map(|c| ProjectedChild {
                    purpose: c.purpose.clone(),
                    outcome: c.outcome.clone(),
                })
                .collect();
            let goal = durable_task
                .as_ref()
                .map(|t| t.goal.clone())
                .filter(|g| !g.is_empty())
                .or_else(|| (!head.goal.is_empty()).then(|| head.goal.clone()))
                .unwrap_or_else(|| ledger.goal.clone());
            let criteria = durable_task
                .as_ref()
                .map(|t| t.acceptance_criteria.clone())
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| {
                    if !head.criteria.is_empty() {
                        head.criteria.clone()
                    } else {
                        ledger.constraints.clone()
                    }
                });
            let plan_steps = durable_task
                .as_ref()
                .map(|t| t.plan.clone())
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| {
                    if !head.plan_steps.is_empty() {
                        head.plan_steps.iter().map(|s| s.text.clone()).collect()
                    } else {
                        ledger.open_steps.clone()
                    }
                });
            let task_state = durable_task
                .as_ref()
                .map(|t| format!("{:?}", t.state))
                .unwrap_or_else(|| "unknown".to_string());
            TaskContextProjection::from_durable_rows(DurableTaskRows {
                task_state,
                goal,
                criteria,
                plan_steps,
                decisions,
                checks,
                children,
                known_failures: ledger.known_failures.clone(),
                changed_files: ledger.changed_files.clone(),
            })
        };
        let mut plan = compactor
            .compact_projected(
                recent,
                &projection,
                ledger,
                &CompactionRequest::new(before, target),
            )
            .await;
        // The compaction reservation's terminal state runs through its
        // attempt machine (attempt-accounting audit): money moves exactly
        // once per reservation and only through the machine's guarded
        // transitions, which know whether the summarizer's stream truly
        // dispatched:
        //   - accepted LLM summary  => settle the exchanged transcript;
        //   - dispatched but NOT settled (stream failure / timeout /
        //     cancellation / rejected summary) => UNCERTAIN: the provider
        //     may have billed, so the reserved amount keeps consuming until
        //     a reconcile or the task-end finalize — never a refund;
        //   - never dispatched (summarizer never ran / not stream-capable)
        //     => refund the prediction.
        // The provenance identity survives the attempt-machine block (which
        // consumes `trace`): the real summarizer pair this call routed.
        let compaction_identity: Option<(String, String)> = trace
            .as_ref()
            .map(|t| (t.provider.clone(), t.model.clone()));
        if let (Some(trace), Some(machine)) = (trace, machine) {
            let settled = plan.accepted
                && matches!(
                    plan.strategy,
                    faktor_context::CompactionStrategy::LlmSummary
                );
            let mut acct = machine.lock().await;
            match (settled, acct.dispatched()) {
                (true, true) => {
                    // The exchanged transcript — input + output — settles at
                    // the reservation's frozen route-time price capture
                    // (compaction usage frames are not surfaced through the
                    // Summarizer contract); unpriced reservations close as a
                    // documented Unknown spend, never a fabricated 1-micro
                    // actual.
                    if let Err(settle_err) = acct
                        .settle_usage(
                            plan.before_tokens as u64,
                            0,
                            0,
                            plan.after_tokens as u64,
                            None,
                            None,
                        )
                        .await
                    {
                        // The exchange happened but the ledger refused the
                        // close (e.g. unknown price under a hard cap):
                        // leave NO dangling dispatched row — the machine
                        // marks the attempt UNCERTAIN and the turn fails.
                        if let Err(uncertain_err) = acct
                            .fail_after_dispatch("compaction_settle_refused", None)
                            .await
                        {
                            tracing::error!(
                                session = %handle.id(),
                                "cannot mark the unsettled compaction attempt uncertain: {uncertain_err}"
                            );
                        }
                        return Err(settle_err.into());
                    }
                    // The settled compaction exchange's attempt-keyed
                    // provider-call row (attempt accounting, schema v18):
                    // THIS physical attempt with the canonical exchange
                    // basis the reservation settled at — the exchanged
                    // transcript in/out (compaction usage frames never
                    // surface, so the plan's honest transcript numbers ARE
                    // the canonical usage of this call).
                    if let Err(row_err) = handle.record_provider_call_attempt(
                        trace.attempt,
                        trace.reservation,
                        &trace.provider,
                        &trace.model,
                        "completed",
                        Some(plan.before_tokens as u64),
                        Some(plan.after_tokens as u64),
                        None,
                    ) {
                        tracing::error!(
                            session = %handle.id(),
                            "compaction provider-call completion row failed: {row_err}"
                        );
                    }
                    // Telemetry outcome entry (P0-28): the settled Compact
                    // call — success=true with the actual summarizer
                    // provider/model. No verified sample at this site.
                    self.deps.routing.record_call_outcome(&SettledCallOutcome {
                        provider: trace.provider.clone(),
                        model: trace.model.clone(),
                        phase: RouterPhase::Compact,
                        success: true,
                        retried: false,
                        rate_limited: false,
                        latency_ms: 0,
                        verified: None,
                    });
                }
                (false, true) => {
                    // Dispatched but not settled: the summarizer may have
                    // billed — mark the attempt UNCERTAIN with the reason so
                    // the task-end finalize charges it (best-effort; a
                    // refusal leaves the row for recovery).
                    if let Err(e) = acct
                        .fail_after_dispatch("compaction_not_accepted_after_dispatch", None)
                        .await
                    {
                        tracing::error!(
                            session = %handle.id(),
                            "cannot mark the dispatched compaction attempt uncertain: {e}"
                        );
                    }
                    // The failed compaction attempt's attempt-keyed row:
                    // this physical attempt's failure with its own identity
                    // (never a legacy logical-op merged row).
                    if let Err(row_err) = handle.record_provider_call_attempt(
                        trace.attempt,
                        trace.reservation,
                        &trace.provider,
                        &trace.model,
                        "failed",
                        None,
                        None,
                        Some("compaction summary not accepted after dispatch"),
                    ) {
                        tracing::error!(
                            session = %handle.id(),
                            "compaction provider-call failure row failed: {row_err}"
                        );
                    }
                    // Telemetry failure signal (P0-28): the dispatched
                    // compaction call did not resolve.
                    self.deps.routing.record_call_outcome(&SettledCallOutcome {
                        provider: trace.provider.clone(),
                        model: trace.model.clone(),
                        phase: RouterPhase::Compact,
                        success: false,
                        retried: false,
                        rate_limited: false,
                        latency_ms: 0,
                        verified: None,
                    });
                }
                (false, false) => {
                    // Never dispatched: the provider was provably never
                    // contacted — release the prediction.
                    if let Err(e) = acct.fail_before_dispatch().await {
                        tracing::error!(session = %handle.id(), "compaction budget refund failed: {e}");
                    }
                }
                (true, false) => {
                    // An accepted LLM summary from a summarizer that never
                    // dispatched is an internal contradiction (summaries are
                    // only accepted from a real stream run): refuse loudly,
                    // never guess a money move.
                    return Err(Error::new(
                        ErrorKind::Internal,
                        "compaction reservation settled without a dispatch marker",
                    ));
                }
            }
        }
        let accepted = plan.accepted;
        handle.record_compaction_defaults(
            plan.before_tokens as i64,
            plan.after_tokens as i64,
            plan.target_tokens as i64,
            match plan.strategy {
                faktor_context::CompactionStrategy::LlmSummary => "llm_summary",
                faktor_context::CompactionStrategy::DeterministicPruning => "deterministic",
                faktor_context::CompactionStrategy::Rejected => "rejected",
            },
        )?;
        if !accepted {
            // CompactRejected is journaled by record_compaction.
            return Ok(None);
        }
        // Audit item 3: an accepted LLM summary records its durable
        // PROVENANCE — the context-compression trust class, the physical
        // call id, the transcript deltas and the routed pair. The row is
        // namespaced `compaction`/`provenance`: it is NOT a task fact, the
        // completion/task-fact writers refuse this trust class typed, and
        // nothing in this path touches the task_state/criteria/verification
        // facts (compaction never rewrites durable authority).
        if matches!(
            plan.strategy,
            faktor_context::CompactionStrategy::LlmSummary
        ) {
            let output = compaction_output_slot
                .lock()
                .ok()
                .and_then(|slot| slot.clone());
            if let Some(output) = output {
                if let Err(refusal) = self.record_compaction_provenance(
                    handle,
                    &output,
                    &plan,
                    compaction_identity
                        .as_ref()
                        .map(|(p, m)| (p.as_str(), m.as_str())),
                ) {
                    tracing::error!(
                        session = %handle.id(),
                        "compaction provenance refused by the trust gate: {refusal}"
                    );
                }
            } else {
                tracing::warn!(
                    session = %handle.id(),
                    "accepted LLM summary carried no captured model output; provenance not recorded"
                );
            }
        }
        handle.put_task_ledger(serde_json::to_value(&plan.ledger)?)?;
        // Typed ledger watermark compaction (audit 27): an accepted
        // compaction also prunes the typed entry stream below the
        // never-FIFO-evict durability watermark. The head checkpoint is
        // rewritten in the same transaction as the deletions; an entry that
        // fails its schema decode REFUSES the compaction loudly (nothing is
        // ever silently deleted). Errors fail the turn: a corrupt typed
        // ledger must never compact "successfully" around corruption.
        {
            let report = handle.compact_typed_ledger()?;
            tracing::debug!(
                session = %handle.id(),
                deleted = report.deleted,
                kept = report.kept,
                "typed ledger compaction below the durability watermark"
            );
        }
        // Durable archiving (P0: no more 1 MiB cap losing history): evicted
        // turns arrive as ORDERED chunks (oldest first, each bounded) — each
        // chunk is written to the CAS, then a small JSON manifest
        // {version:1, chunks:[{index,size,hash}], total_bytes} is written
        // and its content address replaces the digest placeholder — the
        // digest rides the wire with the archive behind ONE artifact ref.
        // Best-effort: an unwritable CAS leaves the digest text without a
        // hash (never breaks the turn).
        if !plan.archive_chunks.is_empty() {
            if let Some(cas) = &self.deps.cas {
                let mut chunk_entries: Vec<serde_json::Value> = Vec::new();
                let mut total_bytes = 0usize;
                for (index, chunk) in plan.archive_chunks.iter().enumerate() {
                    // Each chunk is stored whole (chunks are already bounded
                    // by the compactor; a pathological single-turn chunk may
                    // exceed the bound and still stores — never truncated).
                    let Ok(hash) = cas.put_bounded(chunk.as_bytes(), chunk.len()) else {
                        chunk_entries.clear();
                        break;
                    };
                    total_bytes = total_bytes.saturating_add(chunk.len());
                    chunk_entries.push(serde_json::json!({
                        "index": index,
                        "size": chunk.len(),
                        "hash": hash.to_string(),
                    }));
                }
                let manifest = serde_json::json!({
                    "version": 1,
                    "chunks": chunk_entries,
                    "total_bytes": total_bytes,
                });
                if !chunk_entries.is_empty() {
                    if let Ok(bytes) = serde_json::to_vec(&manifest) {
                        if let Ok(hash) = cas.put(&bytes) {
                            let marker = format!("artifact://{hash}");
                            if let Some(first) = plan.kept_recent.first_mut() {
                                first.text = first.text.replace("<artifact://hash>", &marker);
                            }
                        }
                    }
                }
            }
        }
        Ok(Some(plan))
    }

    /// Record the durable provenance of one ACCEPTED context-compression
    /// output (audit item 3). Only a
    /// [`crate::OutputTrust::ContextCompression`] output may author a
    /// provenance row — any other class is a typed [`TrustRefusal`]. The row
    /// is namespaced `compaction`/`provenance` and is the ONLY durable write
    /// a compression output can make: completion/immutable-task-fact writers
    /// refuse this class typed, so provenance can never replace durable
    /// facts.
    pub(crate) fn record_compaction_provenance(
        &self,
        handle: &faktor_session::SessionHandle,
        output: &ModelOutput,
        plan: &CompactionPlan,
        identity: Option<(&str, &str)>,
    ) -> Result<(), TrustRefusal> {
        if output.trust != OutputTrust::ContextCompression {
            return Err(TrustRefusal {
                trust: output.trust,
                required: "a context-compression output (only compression authors provenance)",
            });
        }
        let strategy = match plan.strategy {
            faktor_context::CompactionStrategy::LlmSummary => "llm_summary",
            faktor_context::CompactionStrategy::DeterministicPruning => "deterministic",
            faktor_context::CompactionStrategy::Rejected => "rejected",
        };
        let value = serde_json::json!({
            "provenance": output.trust.provenance_tag(),
            "call_id": output.call_id,
            "strategy": strategy,
            "before_tokens": plan.before_tokens,
            "after_tokens": plan.after_tokens,
            "target_tokens": plan.target_tokens,
            "provider": identity.map(|(p, _)| p),
            "model": identity.map(|(_, m)| m),
        })
        .to_string();
        self.dw_note_upsert_provenance_fact(
            handle,
            FactSource::Model(output),
            "compaction",
            "provenance",
            &value,
            DW_SITE_COMPACTION_PROVENANCE,
        );
        Ok(())
    }

    /// Reserve the budget of one compaction summarizer BEFORE it streams and
    /// return it paired with its reservation (the reservation settles only
    /// when the LLM summary is accepted — see [`AgentRuntime::try_compact`] —
    /// and refunds otherwise). Prediction = the transcript to exchange plus
    /// the summary output; `pricing_snapshot` is the route-time price
    /// capture of the summarizer model when routing priced the call (None =
    /// the explicit `compaction_model` config path, whose session side is
    /// unpriced — settlement then closes the accepted summary as an honest
    /// Unknown spend instead of a fabricated 1-micro-per-token actual).
    /// A budget denial fails the turn (fail closed); the weak ledger
    /// summarizer is the RouterUnavailable-only degradation, never a
    /// budget-workaround.
    pub(crate) async fn budgeted_summarizer(
        &self,
        handle: &faktor_session::SessionHandle,
        built: Option<StreamingSummarizer>,
        before: usize,
        pricing_snapshot: Option<PricingSnapshot>,
    ) -> faktor_core::Result<BudgetedSummarizer> {
        let Some(mut s) = built else {
            return Ok((None, None, None, None));
        };
        let task_id = handle.task_id()?;
        let predicted = (before as u64).saturating_add(4096).saturating_add(1024);
        // Attempt identity (attempt-accounting audit): the compaction call
        // is one logical op (the summarizer's) with ONE physical attempt —
        // minted BEFORE the reservation so the reservation and the
        // attempt-keyed provider-call rows share the same attempt id.
        let attempt_identity =
            ModelCallAttempt::new(s.op_id, self.deps.session.try_next_op_id()?, 0).ok_or_else(
                || {
                    Error::new(
                        ErrorKind::Internal,
                        "the compaction attempt op id collided with the summarizer op id",
                    )
                },
            )?;
        let reservation = match self
            .deps
            .budgets
            .reserve_attempt(
                handle.id(),
                task_id,
                attempt_identity,
                predicted,
                pricing_snapshot,
            )
            .await
        {
            Ok(r) => r,
            Err(SessionBudgetError::BudgetExceeded { .. }) => {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    "budget exceeded: cannot afford the compaction call",
                ));
            }
            Err(e) => return Err(e.into()),
        };
        // The attempt machine of the compaction reservation (attempt-
        // accounting audit): the summarizer marks it dispatched immediately
        // before its provider request is sent (see [`StreamingSummarizer`])
        // and the caller settles / marks UNCERTAIN / refunds through the
        // SAME machine — a post-dispatch refund is refused by its guards.
        let machine = Arc::new(tokio::sync::Mutex::new(crate::AttemptAccounting::new(
            self.deps.budgets.clone(),
            handle.id(),
            Some(reservation),
        )));
        s.budget_marker = Some(BudgetDispatchMarker {
            machine: machine.clone(),
        });
        let trace = CompactCallTrace {
            attempt: attempt_identity,
            reservation: reservation_link(reservation),
            provider: s.provider.id().to_string(),
            model: s.model.clone(),
        };
        Ok((
            Some(Arc::new(s)),
            Some(reservation),
            Some(machine),
            Some(trace),
        ))
    }
}
