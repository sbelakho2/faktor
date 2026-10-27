//! @authority-collapse: authority-bearing
//!
//! `runtime::compaction`: cohesive slice of the agent runtime.

use super::*;

/// The compaction model's dedicated system contract (P0 audit, round 11):
/// the summarizer is NOT the agent — it is the Faktor context compactor
/// producing a faithful state transfer. Sending the agent instructions as
/// the system prompt let the compaction model answer the latest user message
/// instead of summarizing. The agent instructions must stay out of this
/// request entirely.
pub(crate) const COMPACTOR_SYSTEM: &str =
    "You are the Faktor context compactor. Produce a faithful state transfer (STATE \
TRANSFER) that replaces the conversation below. Output ONLY the schema, no preamble, no \
reasoning, no advice, no code:\n\
STATE: <one line, <=200 chars>\n\
DECISIONS:\n- <step>: <choice> -> <reason>\n\
FILES:\n- <path> :: <change>\n\
CHECKS:\n- <name>: pass|fail <summary>\n\
BLOCKERS:\n- <unresolved errors and blockers, NEVER omit one>\n\
NEXT:\n- <next action>\n\
NOTES:\n- F:<stable-id> <fact>\n\
Rules: the task goal, acceptance criteria and durable facts already ride the wire \
VERBATIM outside your output; never restate them. Emit each fact ONCE with its stable id \
and never repeat a line. Prefer ids/paths/deltas over prose. Omit greetings, thoughts, \
acknowledgements. NEVER invent facts, code, paths or results. Keep the WHOLE output under \
900 tokens and <=8 items per section. If the transcript is incomplete, say exactly what \
is missing in STATE.";

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
/// a registered provider covering
/// [`faktor_semantic::SemanticCapabilities::AFFECTED`] at the authoritative
/// quality floor is
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
    // Descriptor handshake + THE authoritative quality gate: an external
    // provider is only consulted once its per-operation affected quality
    // satisfies the authoritative floor (structural / partial), and a
    // cooling (recently failing) provider is skipped rather than re-polled.
    let selection = deps
        .semantic
        .select_quality(
            faktor_semantic::SemanticQualityRequirement::authoritative(SemanticOp::Affected),
            cancel,
        )
        .await;
    let provider = match selection {
        Ok(faktor_semantic::SemanticQualitySelection::Provider { provider, .. }) => provider,
        // No registered provider at the required quality: parity with the
        // provider-less runtime (no consult, risk stays Unknown).
        Ok(faktor_semantic::SemanticQualitySelection::Fallback { .. }) | Err(_) => return None,
    };
    let provider_id = provider.id();
    let Ok(row) = handle.row() else {
        return Some(SemanticTurnState::unknown(provider_id.to_string()));
    };
    let workspace = row.workspace_id;
    // The live revision is the workspace CONTENT identity (audit
    // P1-SEMANTIC), resolved through the session's shadow-aware root — the
    // session id is an internal handle with no relation to program content.
    let workspace_root = deps
        .session
        .resolve_workspace_root(handle.id())
        .ok()
        .flatten();
    // Identity law (audit P1): without a CONTENT revision there is no
    // snapshot identity to derive — degrade to Unknown instead of deriving
    // one from a synthetic marker.
    let Some(revision) = crate::semantic_tool::semantic_source_revision(workspace_root.as_deref())
    else {
        tracing::warn!(
            session = %handle.id(),
            "workspace content identity unavailable; semantic consult degraded to unknown"
        );
        return Some(SemanticTurnState::unknown(provider_id.to_string()));
    };
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
            Some(SemanticEntityRef::heuristic(workspace, path, entity_id))
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
    )
    // The view identity above was derived from THIS provider (audit
    // P1-SEMANTIC): pin dispatch so a fail-over provider can never answer a
    // request whose identity describes the selected one.
    .pinned_to(provider_id.clone());
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

/// The failure fallback of the streaming summarizer. An EMPTY string is
/// the typed "no summary" signal: the compactor rejects it and deterministic
/// pruning takes over. The old verbatim 3x transcript echo relied on the
/// compactor NOT clamping; once summaries are hard-budgeted, a transcript
/// echo would be clamped and wrongly accepted as a ~0% LLM summary.
pub(crate) fn summarize_failure_fallback(_history: &[RecentTurn]) -> String {
    String::new()
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
    // Only DiskWrite/Terminal tools can change the tree. A read-only tool's
    // path argument is NOT a changed file: treating it as one made every read
    // look like an edit, triggered required checks (and child verification
    // failure) for files the turn never touched.
    mutating: bool,
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
    if mutating
        && (outcome.effect_status == faktor_core::op::EffectStatus::Applied
            || outcome.exit_code == Some(0))
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

/// True only when the archive manifest exists AND declares a non-empty,
/// internally consistent chunk set whose chunk blobs are all still present.
/// `Cas::has(manifest)` alone is not "retrievable": the governor may evict a
/// chunk while the small manifest survives (per-object eviction), and a
/// truncated/corrupt manifest is not a pointer either. The manifest is
/// bounded (one small JSON row per chunk), so this check is a single bounded
/// read plus one stat per referenced chunk.
pub(crate) fn archive_manifest_is_retrievable(
    cas: &faktor_cas::Cas,
    manifest: faktor_core::hash::FileHash,
) -> bool {
    const MANIFEST_MAX_BYTES: usize = 256 * 1024;
    let Ok(Some(bytes)) = cas.get_bounded(manifest, MANIFEST_MAX_BYTES) else {
        return false;
    };
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    if parsed.get("version").and_then(serde_json::Value::as_u64) != Some(1) {
        return false;
    }
    let Some(chunks) = parsed.get("chunks").and_then(serde_json::Value::as_array) else {
        return false;
    };
    if chunks.is_empty() {
        return false;
    }
    let mut total = 0u64;
    for chunk in chunks {
        let Some(hex) = chunk.get("hash").and_then(serde_json::Value::as_str) else {
            return false;
        };
        let Some(hash) = faktor_core::hash::FileHash::from_hex(hex) else {
            return false;
        };
        if !cas.has(hash) {
            return false;
        }
        let Some(size) = chunk.get("size").and_then(serde_json::Value::as_u64) else {
            return false;
        };
        total = total.saturating_add(size);
    }
    parsed
        .get("total_bytes")
        .and_then(serde_json::Value::as_u64)
        .is_none_or(|declared| declared == total)
}

/// The compactor's digest turn is the ONE wire carrier of a compaction
/// archive pointer. Other `artifact://` refs (tool-log artifacts) are single
/// blobs with different semantics and must never be interpreted as a
/// compaction manifest.
fn is_archive_digest_turn(turn: &RecentTurn) -> bool {
    turn.role == "assistant" && turn.text.starts_with("[Earlier context archived:")
}

/// Replace a carried archive artifact ref that is no longer fully
/// retrievable (the storage governor evicted the archive class or one of its
/// chunks) with an explicit "archive evicted" marker. Without this the next
/// compaction keeps claiming that evicted context is retrievable at
/// `artifact://<hash>`, and the model spends turns trying to recover it. The
/// durable transcript rows remain the fallback authority; only the wire text
/// changes. Scans every digest turn (the pointer is normally first, but the
/// marker must never be missed because of plan ordering). Returns the number
/// of refs marked.
pub fn mark_evicted_compaction_archives(
    cas: Option<&faktor_cas::Cas>,
    plan: &mut faktor_context::compactor::CompactionPlan,
) -> usize {
    let Some(cas) = cas else {
        return 0;
    };
    let mut marked = 0usize;
    for turn in &mut plan.kept_recent {
        // Fast path: only a compaction digest carries a manifest pointer;
        // other artifact refs (tool logs) are not manifest-backed.
        if !is_archive_digest_turn(turn) || !turn.text.contains("artifact://") {
            continue;
        }
        let text = std::mem::take(&mut turn.text);
        let mut out = String::with_capacity(text.len());
        let mut rest = text.as_str();
        let mut turn_marked = 0usize;
        while let Some(index) = rest.find("artifact://") {
            out.push_str(&rest[..index]);
            let tail = &rest[index + "artifact://".len()..];
            let digest_len = tail
                .bytes()
                .take_while(|byte| byte.is_ascii_hexdigit())
                .count()
                .min(64);
            let digest = &tail[..digest_len];
            match faktor_core::hash::FileHash::from_hex(digest) {
                Some(hash) if !archive_manifest_is_retrievable(cas, hash) => {
                    out.push_str("[archive evicted: ");
                    out.push_str(digest);
                    out.push(']');
                    turn_marked += 1;
                    rest = &tail[digest_len..];
                }
                _ => {
                    out.push_str("artifact://");
                    rest = tail;
                }
            }
        }
        out.push_str(rest);
        if turn_marked > 0 {
            turn.text = out;
            marked += turn_marked;
        } else {
            // Byte-identical when nothing moved (no allocation retained).
            turn.text = text;
        }
    }
    if marked > 0 {
        tracing::warn!(
            refs = marked,
            "compaction archive was evicted; the wire now names the loss instead of a dead artifact ref"
        );
    }
    marked
}

/// Replace the compactor's archive placeholder with the TRUTH for this turn:
/// the real manifest artifact ref when the archive was published, or an
/// explicit "archive unavailable" marker when the CAS is absent/refusing.
/// The former left the literal `<artifact://hash>` placeholder on the wire,
/// a dangling artifact reference that invited the model to spend turns
/// re-deriving context that was never retrievable. The evicted rows remain in
/// the durable transcript either way; only the wire marker changes.
///
/// An archive is published ONLY when the plan actually carries a wire
/// pointer for it. When the digest marker could not fit under the hard cap,
/// storing the chunks would create an unreachable artifact (pure storage
/// growth under pressure), so the archive is skipped and the durable
/// transcript remains the authority.
pub fn attach_compaction_archive(
    cas: Option<&faktor_cas::Cas>,
    plan: &mut faktor_context::compactor::CompactionPlan,
) -> Option<faktor_core::hash::FileHash> {
    if plan.archive_chunks.is_empty() {
        return None;
    }
    let archived_bytes: usize = plan.archive_chunks.iter().map(String::len).sum();
    if !plan
        .kept_recent
        .iter()
        .any(|turn| is_archive_digest_turn(turn) && turn.text.contains("<artifact://hash>"))
    {
        tracing::warn!(
            chunks = plan.archive_chunks.len(),
            bytes = archived_bytes,
            "compaction archive not published: the plan carries no wire archive pointer (the marker did not fit the hard cap)"
        );
        return None;
    }
    let Some(cas) = cas else {
        annotate_archive_unavailable(plan, archived_bytes);
        tracing::warn!(
            chunks = plan.archive_chunks.len(),
            bytes = archived_bytes,
            "compaction archive skipped: no content store; the wire carries an explicit unavailable marker"
        );
        return None;
    };
    match store_compaction_archive(cas, &plan.archive_chunks) {
        Some(hash) => {
            let marker = format!("artifact://{hash}");
            for turn in &mut plan.kept_recent {
                if is_archive_digest_turn(turn) && turn.text.contains("<artifact://hash>") {
                    turn.text = turn.text.replace("<artifact://hash>", &marker);
                }
            }
            Some(hash)
        }
        None => {
            annotate_archive_unavailable(plan, archived_bytes);
            tracing::warn!(
                chunks = plan.archive_chunks.len(),
                bytes = archived_bytes,
                "compaction archive publish failed; the wire carries an explicit unavailable marker"
            );
            None
        }
    }
}

fn annotate_archive_unavailable(
    plan: &mut faktor_context::compactor::CompactionPlan,
    archived_bytes: usize,
) {
    let marker = format!(
        "[archive unavailable: {archived_bytes} bytes omitted from the wire; the original rows remain in the durable transcript]"
    );
    for turn in &mut plan.kept_recent {
        if is_archive_digest_turn(turn) && turn.text.contains("<artifact://hash>") {
            turn.text = turn.text.replace("<artifact://hash>", &marker);
        }
    }
}

/// Store the eviction archive (ordered chunks + JSON manifest) in the main
/// CAS under the `CompactionArchive` class. Returns the manifest digest, or
/// `None` when nothing could be published (best-effort: the turn continues
/// with the inline digest exactly as before).
pub(crate) fn store_compaction_archive(
    cas: &faktor_cas::Cas,
    chunks: &[String],
) -> Option<faktor_core::hash::FileHash> {
    let mut chunk_entries: Vec<serde_json::Value> = Vec::new();
    let mut total_bytes = 0usize;
    for (index, chunk) in chunks.iter().enumerate() {
        // Each chunk is stored whole (chunks are already bounded by the
        // compactor; a pathological single-turn chunk may exceed the bound
        // and still stores — never truncated).
        let Ok(hash) = cas.put_bounded_in_class(
            faktor_cas::CasStorageClass::CompactionArchive,
            "compaction-archive-chunk",
            chunk.as_bytes(),
            chunk.len(),
        ) else {
            return None;
        };
        total_bytes = total_bytes.saturating_add(chunk.len());
        chunk_entries.push(serde_json::json!({
            "index": index,
            "size": chunk.len(),
            "hash": hash.to_string(),
        }));
    }
    if chunk_entries.is_empty() {
        return None;
    }
    let manifest = serde_json::json!({
        "version": 1,
        "chunks": chunk_entries,
        "total_bytes": total_bytes,
    });
    let bytes = serde_json::to_vec(&manifest).ok()?;
    cas.put_in_class(
        faktor_cas::CasStorageClass::CompactionArchive,
        "compaction-archive-manifest",
        &bytes,
    )
    .ok()
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

    /// The effective configured compaction model: the session setting wins,
    /// then the daemon config. Empty AND the literal `inherit` mean "not
    /// configured": both fall through to routing/deterministic pruning.
    ///
    /// `inherit` is not a model. A durable/legacy row carrying it must never
    /// become a bogus provider call (which also wastes the one compaction
    /// slot); the daemon's own settings route clears the fact, but recovery
    /// deletes nothing and older writers may have mirrored the literal.
    pub(crate) fn configured_compaction_model<'a>(
        session: Option<&'a str>,
        daemon: Option<&'a str>,
    ) -> Option<&'a str> {
        session
            .map(str::trim)
            .filter(|value| !value.is_empty() && *value != "inherit")
            .or_else(|| {
                daemon
                    .map(str::trim)
                    .filter(|value| !value.is_empty() && *value != "inherit")
            })
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

    /// Compatibility entry point for callers/tests that do not own a
    /// durable history head (no checkpoint is persisted).
    #[allow(dead_code)] // test/compatibility entry; production uses the head-carrying twin
    pub(crate) async fn try_compact(
        &self,
        handle: &faktor_session::SessionHandle,
        recent: &[RecentTurn],
        ledger: &TaskLedger,
        budget: &ContextBudget,
        cancel: &CancellationToken,
    ) -> faktor_core::Result<Option<CompactionPlan>> {
        self.try_compact_with_head(handle, recent, ledger, budget, cancel, None)
            .await
    }

    /// Select and budget the compaction summarizer for this turn: an
    /// explicit compaction_model wins; otherwise the Compact phase is
    /// routed (passthrough => deterministic pruning). Extracted from the
    /// compaction orchestrator so its function-length ceiling stays honest;
    /// the selection and refusal semantics are unchanged.
    async fn select_compaction_summarizer(
        &self,
        handle: &faktor_session::SessionHandle,
        before: usize,
        cancel: &CancellationToken,
        compaction_output_slot: Arc<std::sync::Mutex<Option<ModelOutput>>>,
        compaction_usage_slot: Arc<std::sync::Mutex<Option<CanonicalUsage>>>,
    ) -> faktor_core::Result<BudgetedSummarizer> {
        // The per-session compaction model (K-02, `/native/session/{id}/
        // settings`) wins over the daemon config, exactly like the session
        // context-window setting: the daemon mirrors the accepted
        // `provider/model` into the session fact space because the agent
        // runtime cannot read the foundation store. An empty/absent fact is
        // `inherit` (the daemon config, then router passthrough).
        let session_model = handle
            .memory_fact_get("session_settings", "compactionModel")
            .ok()
            .flatten()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        // `inherit` is the ABSENCE of a model, never a model id: a stored
        // literal (legacy/hostile row) must fall through to the daemon
        // config exactly like an empty/absent fact, not consume a provider
        // call with a guaranteed-unroutable name.
        let configured_model = Self::configured_compaction_model(
            session_model.as_deref(),
            self.deps.compaction_model.as_deref(),
        );
        Ok(if let Some(model) = configured_model {
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
                    usage_slot: compaction_usage_slot.clone(),
                }),
                Err(e) => {
                    tracing::warn!(
                        "compaction model {model:?} unresolvable: {e}; using the ledger summarizer"
                    );
                    None
                }
            };
            let (summarizer, reservation, machine, trace) = self
                .budgeted_summarizer(
                    handle,
                    built,
                    before,
                    None,
                    faktor_core::model::ExecutionClass::Unknown,
                )
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
                            usage_slot: compaction_usage_slot.clone(),
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
                    // decision's price capture on its reservation; P1: the
                    // decision's execution class rides it too.
                    self.budgeted_summarizer(
                        handle,
                        built,
                        before,
                        d.pricing_snapshot,
                        d.execution_class,
                    )
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
        })
    }

    pub(crate) async fn try_compact_with_head(
        &self,
        handle: &faktor_session::SessionHandle,
        recent: &[RecentTurn],
        ledger: &TaskLedger,
        budget: &ContextBudget,
        // The LOGICAL TURN's token reaches the summarizer stream: a Stop
        // must cancel compaction (no orphan request after the turn; audit 11).
        cancel: &CancellationToken,
        // Durable high-water mark of the window `recent` was loaded from.
        head_seq: Option<u64>,
    ) -> faktor_core::Result<Option<CompactionPlan>> {
        // Strip the PREVIOUS compaction's synthesized blocks before the
        // next compaction: the identity/durable-facts turns are regenerated
        // from the durable projection below, and carrying the old copies
        // would duplicate them on every cycle. The prior eviction digest is
        // KEPT: the compactor carries it forward verbatim when this cycle
        // evicts nothing, so repeated compactions never orphan the durable
        // archive. Prior `summary` turns stay too: the newest is carried
        // verbatim and never re-summarized.
        let input: Vec<RecentTurn> = recent
            .iter()
            .filter(|t| t.role != "system")
            .cloned()
            .collect();
        let recent = input.as_slice();
        // SAME token unit as the compactor's `after_tokens`: the previous
        // `bytes / 4` underestimated the estimator by ~25%, so the recorded
        // reduction ratio could pass while the exact wire shrank by less.
        let before = faktor_context::estimate_history_tokens(recent);
        if before == 0 {
            return Ok(None);
        }
        // A tiny history cannot need compaction: the wire plan is dominated
        // by the system prompt/tool bundle, which compaction does not touch,
        // and a fresh session's first turn used to be failed by a summary
        // that could only GROW the 3-token history. Below this floor the
        // honest behavior is "no compaction" (an oversized call still fails
        // with a typed context error from the provider).
        // The deterministic/summary floors are ~65 tokens; a history below
        // this can never be reduced by any summary, so compaction is skipped
        // instead of being attempted (and rejected as a grow).
        const MIN_COMPACTABLE_HISTORY_TOKENS: usize = 128;
        if before < MIN_COMPACTABLE_HISTORY_TOKENS {
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
        let compaction_usage_slot: Arc<std::sync::Mutex<Option<CanonicalUsage>>> =
            Arc::new(std::sync::Mutex::new(None));
        let (summarizer, _reservation, machine, trace): BudgetedSummarizer = self
            .select_compaction_summarizer(
                handle,
                before,
                cancel,
                compaction_output_slot.clone(),
                compaction_usage_slot.clone(),
            )
            .await?;
        let compactor: Compactor = match summarizer {
            Some(s) => Compactor::new(Some(s)),
            // No model may compact (routing passthrough): deterministic
            // pruning IS the state transfer. The old ledger "summarizer"
            // echoed the durable facts that the synthesized projection
            // already rides verbatim, so every accepted cycle duplicated
            // them on the wire.
            None => Compactor::deterministic_only(),
        };
        // Evidence extracted from huge historical tool output is archived
        // through the daemon's DURABLE authority at the live session scope:
        // the `evidence://<id>` rendered on the wire must still resolve after
        // this compactor is dropped and the daemon restarts. A failed durable
        // write keeps the raw output inline (never a dangling ref).
        let workspace_id = handle.row()?.workspace_id.raw();
        let task_id = handle
            .task_id() // allowed-task-id-read: read-only evidence archive scope; compaction never writes a task row
            .ok()
            .map(|task| task.raw())
            .filter(|task| *task > 0);
        let compactor = compactor
            .with_evidence_scope(handle.id().raw(), workspace_id, task_id)
            .map_err(|error| Error::internal(format!("compaction evidence scope: {error}")))?
            .with_durable_evidence(self.evidence_authority.clone());
        // Audit 70/71: compaction consumes a READ-ONLY projection built from
        // the durable rows — goal/criteria/plan/state from the typed Task
        // row, decisions + child progress from the typed session ledger,
        // checks from the VerificationRecord rows. The transcript can lie;
        // these rows cannot. (The projection is not written back: compaction
        // has no ledger write authority.)
        //
        // `[efficiency].typed_handoff` is the production switch of exactly
        // this rendering path: ON (the production default) renders the
        // re-sent history from the durable task rows below; OFF falls back
        // to the legacy transcript-derived fold (`from_task_ledger`), which
        // is the byte-identical pre-flag rendering.
        let projection = if self.deps.efficiency.typed_handoff {
            Self::compaction_projection(handle, ledger)?
        } else {
            faktor_context::ledger::TaskContextProjection::from_task_ledger(ledger)
        };
        // ONE configured invariant: the same minimum reduction drives the
        // request's hard cap AND the durable acceptance policy. A daemon
        // setting (or a host-installed seam) can no longer be silently
        // relaxed to the 25% default on the proactive path.
        let min_reduction = self.compaction_min_reduction();
        let mut request = CompactionRequest::new(before, target);
        request.min_reduction_ratio = min_reduction;
        let mut plan = compactor
            .compact_projected(recent, &projection, ledger, &request)
            .await;
        // Reservation terminal state moves through the attempt machine
        // exactly once: accepted => settle; dispatched-but-unsettled =>
        // UNCERTAIN (may have billed, never a refund); never dispatched =>
        // refund. The provenance identity survives after `trace` is consumed.
        let compaction_identity: Option<(String, String)> = trace
            .as_ref()
            .map(|t| (t.provider.clone(), t.model.clone()));
        if let (Some(trace), Some(machine)) = (trace, machine) {
            let settled = plan.accepted
                && matches!(
                    plan.strategy,
                    faktor_context::CompactionStrategy::LlmSummary
                );
            // The provider's measured exchange, when the wire reported one.
            // A zero frame is not evidence (it is the malformed no-usage
            // shape): fall back to the plan's conservative transcript
            // figures for BOTH the money settlement and the durable row.
            let measured = compaction_usage_slot
                .lock()
                .ok()
                .and_then(|slot| slot.clone())
                .filter(|usage| recorded_input_total(usage) != 0 || usage.output_tokens != 0);
            let (settle_in, settle_cache_read, settle_cache_write, settle_out) = measured
                .as_ref()
                .map(|usage| {
                    (
                        usage.uncached_input_tokens,
                        usage.cache_read_tokens,
                        usage.cache_write_tokens,
                        usage.output_tokens,
                    )
                })
                .unwrap_or((plan.before_tokens as u64, 0, 0, plan.after_tokens as u64));
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
                            settle_in,
                            settle_cache_read,
                            settle_cache_write,
                            settle_out,
                            measured.as_ref().and_then(|u| {
                                u.reported_cost
                                    .as_ref()
                                    .filter(|cost| cost.is_usd())
                                    .map(|cost| cost.micro_usd)
                            }),
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
                    // basis the reservation settled at: the provider's
                    // measured usage frame when the wire reports one, else
                    // the plan's conservative transcript figures.
                    if let Err(row_err) = handle.record_provider_call_attempt(
                        trace.attempt,
                        trace.reservation,
                        &trace.provider,
                        &trace.model,
                        "completed",
                        Some(
                            settle_in
                                .saturating_add(settle_cache_read)
                                .saturating_add(settle_cache_write),
                        ),
                        Some(settle_out),
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
        // A cancellation during the stream discards the plan; the bounded
        // summarizer token means no compaction task outlives the turn.
        if cancel.is_cancelled() {
            tracing::warn!(
                session = %handle.id(),
                "turn cancelled during compaction; the plan is discarded (no checkpoint, no event)"
            );
            return Ok(None);
        }
        let accepted = plan.accepted;
        if plan.after_tokens > plan.before_tokens {
            // A grow is NOT a recordable compaction (`record_compaction`
            // rightly refuses it as malformed); the plan is already rejected,
            // so skip the record and the turn instead of turning a no-op
            // compaction into a failed turn.
            tracing::warn!(
                session = %handle.id(),
                before = plan.before_tokens,
                after = plan.after_tokens,
                "compaction summary would grow the context; skipping compaction"
            );
            return Ok(None);
        }
        let strategy = match plan.strategy {
            faktor_context::CompactionStrategy::LlmSummary => "llm_summary",
            faktor_context::CompactionStrategy::DeterministicPruning => "deterministic",
            faktor_context::CompactionStrategy::Rejected => "rejected",
        };
        // Durable replacement context (restart parity): the accepted plan's
        // EXACT wire turns, its compaction row and its ContextCompacted event
        // commit in ONE transaction. The previous two-step (checkpoint write
        // then compaction record) could crash between the two and leave a
        // replacement context with no compaction authority; a crash at any
        // seam now reopens on exactly the old world or exactly the new one.
        if accepted {
            // Durable archiving belongs to an ACCEPTED plan only. Rejected
            // plans have no checkpoint that can ever point at their chunks;
            // publishing them would add unreachable artifacts on every
            // rejected compaction under storage pressure (and a sentinel
            // threshold can reject many times in a row). Runs BEFORE the
            // durable checkpoint is written so the checkpoint persists the
            // SAME final marker text the in-memory history carries.
            //
            // Evicted turns arrive as ORDERED chunks (oldest first, each
            // bounded): each chunk is written to the CAS, then a small JSON
            // manifest {version:1, chunks:[{index,size,hash}], total_bytes}
            // is written and its content address replaces the digest
            // placeholder — the digest rides the wire with the archive
            // behind ONE artifact ref. Best-effort: an unwritable CAS leaves
            // an explicit unavailable marker (never breaks the turn).
            mark_evicted_compaction_archives(self.deps.cas.as_deref(), &mut plan);
            attach_compaction_archive(self.deps.cas.as_deref(), &mut plan);
            let Some(through_seq) = head_seq else {
                // No durable source window was supplied (direct callers and
                // unit tests pass a synthetic history). There is nothing to
                // anchor a replacement context to, so journal the compaction
                // authority and leave the durable transcript untouched; the
                // next drive re-compacts from durable rows.
                tracing::warn!(
                    session = %handle.id(),
                    "compaction had no source high-water seq; durable checkpoint skipped"
                );
                handle.record_compaction(
                    plan.before_tokens as i64,
                    plan.after_tokens as i64,
                    plan.target_tokens as i64,
                    strategy,
                    &faktor_session::compaction::CompactionPolicy {
                        min_reduction_ratio: min_reduction,
                    },
                )?;
                return Ok(Some(plan));
            };
            let payload = serde_json::to_vec(&plan.kept_recent)?;
            let digest = faktor_context::history_digest(&plan.kept_recent);
            let record = handle.commit_compaction_with_checkpoint(
                plan.before_tokens as i64,
                plan.after_tokens as i64,
                plan.target_tokens as i64,
                plan.accepted,
                strategy,
                &faktor_session::compaction::CompactionPolicy {
                    min_reduction_ratio: min_reduction,
                },
                through_seq,
                &digest,
                &payload,
            )?;
            if !record.accepted {
                // The durable authority refused the numeric plan (target or
                // minimum reduction): CompactRejected is journaled, no
                // checkpoint was written, and the turn must not claim an
                // applied compaction.
                return Ok(None);
            }
        } else {
            // The compactor's verdict is authoritative AND the SAME
            // configured minimum is the durable acceptance policy. Using
            // the 25% default or numbers alone here could journal
            // `ContextCompacted` for a plan the compactor rejected (a
            // tiny digest-only `after`, or a 40% configured minimum with a
            // 30% reduction): the journal would contradict the turn.
            handle.record_compaction_rejected(
                plan.before_tokens as i64,
                plan.after_tokens as i64,
                plan.target_tokens as i64,
                strategy,
                &faktor_session::compaction::CompactionPolicy {
                    min_reduction_ratio: min_reduction,
                },
            )?;
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
        self.persist_compaction_ledger(handle, &plan)?;
        Ok(Some(plan))
    }

    /// Persist the accepted plan's typed ledger and prune the typed entry
    /// stream below the never-FIFO watermark in one durable step.
    fn persist_compaction_ledger(
        &self,
        handle: &faktor_session::SessionHandle,
        plan: &CompactionPlan,
    ) -> faktor_core::Result<()> {
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
        Ok(())
    }
    /// Build the READ-ONLY durable projection a compaction summarizes against
    /// (audits 70/71): goal/criteria/plan/state from the typed Task row,
    /// decisions + child progress from the typed session ledger, checks from
    /// the VerificationRecord rows. The transcript can lie; these rows cannot.
    /// (The projection is not written back: compaction has no ledger write
    /// authority.)
    fn compaction_projection(
        handle: &faktor_session::SessionHandle,
        ledger: &TaskLedger,
    ) -> faktor_core::Result<faktor_context::ledger::TaskContextProjection> {
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
        Ok(TaskContextProjection::from_durable_rows(DurableTaskRows {
            task_state,
            goal,
            criteria,
            plan_steps,
            decisions,
            checks,
            children,
            known_failures: ledger.known_failures.clone(),
            changed_files: ledger.changed_files.clone(),
        }))
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
        execution_class: faktor_core::model::ExecutionClass,
    ) -> faktor_core::Result<BudgetedSummarizer> {
        let Some(mut s) = built else {
            return Ok((None, None, None, None));
        };
        let task_id = handle.task_id()?;
        // Transcript to exchange + the schema's hard generation bound +
        // the compactor system prompt. The old 4096 output guess reserved
        // 4x the tokens this call can ever produce.
        let predicted = (before as u64)
            .saturating_add(faktor_context::compactor::SUMMARY_MAX_OUTPUT_TOKENS as u64)
            .saturating_add(1024);
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
                execution_class,
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
            s.provider.id(),
            s.model.clone(),
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

#[cfg(test)]
mod collect_tool_summary_tests {
    use super::*;

    fn applied() -> ToolOutcome {
        ToolOutcome {
            text: "ok".into(),
            exit_code: Some(0),
            effect_status: faktor_core::op::EffectStatus::Applied,
            ..Default::default()
        }
    }

    /// Regression: a completed `read_file` (DiskRead) added its path to
    /// `TurnSummary::files_changed`. The projection then listed read files as
    /// changed, end-of-turn verification ran required checks for untouched
    /// files, and a read-only child was settled Failed despite a completed
    /// turn. Reads are NOT edits.
    #[test]
    fn read_only_tools_never_contribute_changed_files() {
        let mut summary = faktor_context::ledger::TurnSummary::default();
        collect_tool_summary(
            &mut summary,
            "read_file",
            &serde_json::json!({"path": "README.md"}),
            &applied(),
            false,
        );
        assert!(summary.files_changed.is_empty(), "{summary:?}");
        assert_eq!(summary.steps_completed, vec!["read_file (README.md)"]);
    }

    #[test]
    fn write_and_terminal_tools_still_contribute_changed_files() {
        let mut summary = faktor_context::ledger::TurnSummary::default();
        collect_tool_summary(
            &mut summary,
            "write_file",
            &serde_json::json!({"path": "src/lib.rs"}),
            &applied(),
            true,
        );
        assert_eq!(summary.files_changed, vec!["src/lib.rs"]);
        collect_tool_summary(
            &mut summary,
            "apply_patch",
            &serde_json::json!({"path": "a.rs"}),
            &applied(),
            true,
        );
        assert_eq!(summary.files_changed, vec!["src/lib.rs", "a.rs"]);
    }

    #[test]
    fn failed_mutating_tool_does_not_claim_a_change() {
        let mut summary = faktor_context::ledger::TurnSummary::default();
        collect_tool_summary(
            &mut summary,
            "write_file",
            &serde_json::json!({"path": "src/lib.rs"}),
            &ToolOutcome {
                text: "denied".into(),
                exit_code: Some(1),
                effect_status: faktor_core::op::EffectStatus::Failed,
                ..Default::default()
            },
            true,
        );
        assert!(summary.files_changed.is_empty());
        assert_eq!(summary.failures.len(), 1);
    }
}
