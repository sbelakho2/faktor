//! `runtime::retrieval`: cohesive slice of the agent runtime.

use super::*;

/// Classify one tool's output for the evidence layer. Producer-side
/// classification only (tool identity, never a provider quirk): command
/// runners produce process logs, search tools search results, test runners
/// test reports, checkers diagnostics; everything else is generic text.
pub(crate) fn tool_evidence_kind(tool: &str) -> EvidenceKind {
    let name = tool.to_ascii_lowercase();
    if name.contains("test") {
        EvidenceKind::TestReport
    } else if name.contains("search") || name.contains("grep") || name.contains("glob") {
        EvidenceKind::SearchResults
    } else if name.contains("command")
        || name.contains("shell")
        || name.contains("terminal")
        || name.contains("bash")
        || name.contains("exec")
    {
        EvidenceKind::ProcessLog
    } else if name.contains("check")
        || name.contains("lint")
        || name.contains("diagnos")
        || name.contains("compile")
    {
        EvidenceKind::DiagnosticSet
    } else if name.contains("read") || name.contains("open") {
        EvidenceKind::GenericText
    } else {
        EvidenceKind::ProcessLog
    }
}

/// Evidence budget of the index-backed first-turn evidence (audits 30/64).
/// Identical to the bounded evidence scan's caps so BOTH evidence paths
/// (durable index when Ready, bounded scan as fallback) produce the same
/// bounded package shape.
pub(crate) const INDEX_EVIDENCE_MAX_HITS: usize = 8;

/// Supplies retrieved evidence before a reasoning turn (spec §20). Default
/// implementation returns nothing; the index wires itself here.
///
/// Async since audit 14/26: retrieval must never block a turn thread. The
/// runtime awaits every provider off the turn thread under a hard wall
/// deadline, and a provider that panics, errs or simply runs long degrades
/// to an empty package — never a stalled turn, never a private child
/// lifecycle inside a provider.
/// The retrieval signal for one reasoning turn (spec §20): the current
/// prompt, the durable task state's changed files, and known failures. The
/// provider derives concepts from all of them — retrieval never depends on
/// the model deciding to search.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EvidenceQuery {
    pub prompt: String,
    pub changed_files: Vec<String>,
    pub failures: Vec<String>,
}

/// One evidence package together with the durable identity/freshness
/// metadata audit 16 requires every package to carry: the index generation,
/// the workspace content/fingerprint identity, the coverage and the typed
/// freshness. Cold packages carry honest `None` identities (no generation)
/// and `stale_while_rebuilding`/`partial` freshness.
pub(crate) struct IndexEvidencePackage {
    pub(crate) evidence: Vec<Evidence>,
    pub(crate) meta: faktor_index::EvidencePackageMeta,
}

/// Typed outcome of the cold-evidence ladder (P2). `NotHosted` is the
/// documented legacy bounded-scan degrade (no IndexService / no attach / no
/// cold provider); `Degraded` carries the TYPED failure of the off-turn
/// bridge as an explicit [`crate::EvidencePollStatus`], so a
/// panicking/unschedulable cold ladder can never collapse into an
/// indistinguishable `None` (the old `Option` wrapper's defect) and can
/// never be mistaken for an honest "no evidence" answer.
pub(crate) enum ColdEvidenceOutcome {
    /// The cold provider served a (possibly empty) package with its typed
    /// freshness metadata (audit 16).
    Served(Vec<Evidence>, faktor_index::EvidencePackageMeta),
    /// The IndexService is not hosted (or not attached): the caller's
    /// documented legacy bounded-scan degrade.
    NotHosted,
    /// The ladder ran but its off-turn bridge failed: explicit degradation.
    Degraded(crate::EvidencePollStatus),
}

/// Canonical durable shape of one [`crate::EvidencePollStatus`]: bounded
/// JSON whose `status` field is the typed discriminant. `truncated` marks
/// an encoding whose human message was dropped to respect
/// [`EVIDENCE_POLL_DURABLE_MAX_BYTES`] (the machine code and retryability
/// always survive).
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct DurableEvidencePollStatus {
    pub(crate) schema: u32,
    pub(crate) status: String,
    pub(crate) degraded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) retryable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) budget_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) abandoned: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cap: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) truncated: Option<bool>,
}

/// The stable machine discriminant of one poll status (the durable
/// encoding's `status` value; also the tracing `status` field).
pub(crate) fn evidence_poll_status_code(status: &crate::EvidencePollStatus) -> &'static str {
    match status {
        crate::EvidencePollStatus::Served => "served",
        crate::EvidencePollStatus::NoEvidence => "no_evidence",
        crate::EvidencePollStatus::RetrievalFailed { .. } => "retrieval_failed",
        crate::EvidencePollStatus::ProviderPanicked { .. } => "provider_panicked",
        crate::EvidencePollStatus::TimedOut { .. } => "timed_out",
        crate::EvidencePollStatus::NotSpawned { .. } => "not_spawned",
        crate::EvidencePollStatus::CircuitOpen { .. } => "circuit_open",
    }
}

/// Canonical, BOUNDED durable encoding of one poll status: the exact bytes
/// archived as evidence and surfaced on [`TurnOutcome::evidence_poll`].
/// Deterministic (fixed field order), so a re-poll of the same turn with the
/// same status dedupes to the SAME evidence row.
pub(crate) fn encode_evidence_poll_status(status: &crate::EvidencePollStatus) -> String {
    let mut durable = DurableEvidencePollStatus {
        schema: EVIDENCE_POLL_DURABLE_SCHEMA,
        status: evidence_poll_status_code(status).to_string(),
        degraded: status.is_degraded(),
        code: None,
        retryable: None,
        message: None,
        budget_ms: None,
        abandoned: None,
        cap: None,
        truncated: None,
    };
    match status {
        crate::EvidencePollStatus::Served | crate::EvidencePollStatus::NoEvidence => {}
        crate::EvidencePollStatus::RetrievalFailed {
            code,
            retryable,
            message,
        } => {
            durable.code = Some(bounded_durable_field(
                code,
                EVIDENCE_POLL_DURABLE_CODE_MAX_BYTES,
            ));
            durable.retryable = Some(*retryable);
            durable.message = Some(bounded_durable_field(
                message,
                EVIDENCE_POLL_DURABLE_MESSAGE_MAX_BYTES,
            ));
        }
        crate::EvidencePollStatus::ProviderPanicked { message } => {
            durable.message = Some(bounded_durable_field(
                message,
                EVIDENCE_POLL_DURABLE_MESSAGE_MAX_BYTES,
            ));
        }
        crate::EvidencePollStatus::TimedOut { budget_ms } => {
            durable.budget_ms = Some(*budget_ms);
        }
        crate::EvidencePollStatus::NotSpawned { message } => {
            durable.message = Some(bounded_durable_field(
                message,
                EVIDENCE_POLL_DURABLE_MESSAGE_MAX_BYTES,
            ));
        }
        crate::EvidencePollStatus::CircuitOpen {
            abandoned,
            cap,
            message,
        } => {
            // The circuit state is machine-readable and survives the round
            // trip: abandoned/cap are the typed counters, the message is the
            // bounded human diagnostic.
            durable.abandoned = Some(*abandoned);
            durable.cap = Some(*cap);
            durable.message = Some(bounded_durable_field(
                message,
                EVIDENCE_POLL_DURABLE_MESSAGE_MAX_BYTES,
            ));
        }
    }
    let mut encoded = serde_json::to_string(&durable).unwrap_or_default();
    if encoded.len() > EVIDENCE_POLL_DURABLE_MAX_BYTES {
        // Defensive only: the per-field caps plus JSON escaping make this
        // unreachable, but an oversized future field must never grow the
        // turn record. The typed discriminant, code and retryability
        // survive; only the human message is dropped.
        durable.message = None;
        durable.truncated = Some(true);
        encoded = serde_json::to_string(&durable).unwrap_or_default();
    }
    encoded
}

/// Reconstruct the typed status from its durable encoding (reopen/diag
/// path). `None` for an unknown schema, an unknown discriminant or a
/// malformed payload — never a guess and never a panic (hostile bytes are
/// just not decodable).
#[cfg(test)]
pub(crate) fn decode_evidence_poll_status(encoded: &str) -> Option<crate::EvidencePollStatus> {
    let durable: DurableEvidencePollStatus = serde_json::from_str(encoded).ok()?;
    if durable.schema != EVIDENCE_POLL_DURABLE_SCHEMA {
        return None;
    }
    Some(match durable.status.as_str() {
        "served" => crate::EvidencePollStatus::Served,
        "no_evidence" => crate::EvidencePollStatus::NoEvidence,
        "retrieval_failed" => crate::EvidencePollStatus::RetrievalFailed {
            code: durable.code?,
            retryable: durable.retryable?,
            message: durable.message.unwrap_or_default(),
        },
        "provider_panicked" => crate::EvidencePollStatus::ProviderPanicked {
            message: durable.message.unwrap_or_default(),
        },
        "timed_out" => crate::EvidencePollStatus::TimedOut {
            budget_ms: durable.budget_ms?,
        },
        "not_spawned" => crate::EvidencePollStatus::NotSpawned {
            message: durable.message.unwrap_or_default(),
        },
        "circuit_open" => crate::EvidencePollStatus::CircuitOpen {
            abandoned: durable.abandoned?,
            cap: durable.cap?,
            message: durable.message.unwrap_or_default(),
        },
        _ => return None,
    })
}

/// Emit the runtime's OWN structured diagnostic of one advisory poll
/// outcome. Loud by contract: the lib.rs wrapper logs only for its own
/// frozen call shape, and any other caller owns its diagnostic — a degraded
/// poll is NEVER silent.
pub(crate) fn log_evidence_poll_outcome(status: &crate::EvidencePollStatus, budget: Duration) {
    let budget_ms = budget.as_millis().min(u64::MAX as u128) as u64;
    match status {
        crate::EvidencePollStatus::Served | crate::EvidencePollStatus::NoEvidence => {}
        crate::EvidencePollStatus::RetrievalFailed {
            code,
            retryable,
            message,
        } => tracing::error!(
            target: "faktor_agent::evidence",
            status = "retrieval_failed",
            provider_code = %code,
            retryable,
            budget_ms,
            "advisory evidence poll failed: {} (advisory contract: the turn continues with an empty package; the typed failure is archived durably, never conflated with no-evidence)",
            message
        ),
        crate::EvidencePollStatus::ProviderPanicked { message } => tracing::error!(
            target: "faktor_agent::evidence",
            status = "provider_panicked",
            budget_ms,
            "advisory evidence poll panicked: {message} (advisory contract: the turn continues with an empty package, never silently)"
        ),
        crate::EvidencePollStatus::TimedOut {
            budget_ms: missed_ms,
        } => tracing::warn!(
            target: "faktor_agent::evidence",
            status = "timed_out",
            budget_ms = *missed_ms,
            "advisory evidence poll missed its wall budget (advisory contract: the turn continues with an empty package)"
        ),
        crate::EvidencePollStatus::NotSpawned { message } => tracing::error!(
            target: "faktor_agent::evidence",
            status = "not_spawned",
            budget_ms,
            "advisory evidence poll could not spawn its detached thread: {message} (advisory contract: the turn continues with an empty package)"
        ),
        crate::EvidencePollStatus::CircuitOpen {
            abandoned,
            cap,
            message,
        } => tracing::error!(
            target: "faktor_agent::evidence",
            status = "circuit_open",
            abandoned,
            cap,
            budget_ms,
            "advisory evidence poll refused: the evidence executor circuit is OPEN ({abandoned}/{cap} abandoned worker threads): {message} (advisory contract: the turn continues with an empty package; the typed circuit state is archived durably)"
        ),
    }
}

pub trait EvidenceProvider: Send + Sync {
    fn evidence_for(
        &self,
        session: SessionId,
        query: EvidenceQuery,
    ) -> futures::future::BoxFuture<'_, faktor_core::Result<Vec<Evidence>>>;

    /// Forget one workspace's cached state (idle-unload, spec §21): the
    /// session ended, its index/scan state is dropped. Default: nothing.
    fn forget(&self, _workspace: WorkspaceId) {}

    /// The CONFIGURED semantic embedder of this evidence provider, when the
    /// daemon wired one (the `[embeddings]` config selection resolved
    /// against the provider registry). The agent's index-backed evidence
    /// assembly reads the embedder from THIS seam instead of hardcoding
    /// `None`, so a configured embedder fuses semantically on both evidence
    /// paths. Default `None`: lexical/symbol-only retrieval — an honest
    /// degradation, never a fabricated vector.
    fn embedder(&self) -> Option<Arc<dyn faktor_search::Embedder>> {
        None
    }

    /// The embedder's stable `(model_id, revision)` when known: the index
    /// persists vectors keyed by it so unchanged chunks are never
    /// re-embedded. Default `None` keeps persistence off (honest).
    fn embedding_model(&self) -> Option<(String, String)> {
        self.embedder().and_then(|e| e.identity())
    }

    /// The index-build embedding source (the CLI's adapter over the same
    /// configured embedder) plus its identity. Default `None`: builds stay
    /// lexical/symbol-only even when a query-time embedder exists.
    fn index_embedding(
        &self,
    ) -> Option<(
        Arc<dyn faktor_index::EmbeddingSource>,
        faktor_index::EmbeddingModel,
    )> {
        None
    }
}

pub struct NoEvidence;

impl EvidenceProvider for NoEvidence {
    fn evidence_for(
        &self,
        _session: SessionId,
        _query: EvidenceQuery,
    ) -> futures::future::BoxFuture<'_, faktor_core::Result<Vec<Evidence>>> {
        Box::pin(async { Ok(vec![]) })
    }
}

/// One logical turn's semantic consult result: provider DATA rendered as
/// evidence plus the conservative risk it implies. Only constructed when a
/// REGISTERED provider covers the consulted operation; a provider that
/// fails/crashes/parks yields [`SemanticTurnState::unknown`] (never Safe).
#[derive(Debug, Clone)]
pub struct SemanticTurnState {
    /// `[evidence:data]`-tagged provider blocks appended to the turn's
    /// evidence (audit 49: provider output is DATA).
    pub evidence: Vec<Evidence>,
    /// Conservative risk level of the provider's assessment: Unknown when
    /// the provider failed or degraded (never Safe).
    pub level: RiskLevel,
    /// The full provider risk assessment behind `level`.
    pub risk: SemanticRisk,
    /// The capability classes the provider's DATA restricts at the tool
    /// gate: an intersection result the gate can only shrink.
    pub restrictions: faktor_core::CapabilitySet,
    /// Validated provider id (diagnostics only).
    pub provider: String,
}

impl SemanticTurnState {
    pub(crate) fn unknown(provider: String) -> Self {
        Self {
            evidence: Vec::new(),
            level: RiskLevel::Unknown,
            risk: SemanticRisk::unknown(),
            restrictions: faktor_core::CapabilitySet::ALL,
            provider,
        }
    }
}

/// Outcome of attaching a workspace for first-turn index evidence. The
/// distinction matters: a BROKEN ATTACH (hostile root, unknown workspace,
/// poisoned service) is a diagnosable failure, while NO READY GENERATION is
/// the documented "worker still building" degrade — both keep the bounded
/// evidence scan, but only one is a warning worth surfacing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IndexEvidenceAttach {
    Ready,
    NoReadyGeneration,
    Broken(String),
}

/// Attach `workspace` to the index service and classify the outcome. Pure
/// observation: attaching only kicks the reconciliation worker and never
/// waits for a build.
pub(crate) fn index_attach_for_evidence(
    service: &faktor_index::IndexService,
    workspace: WorkspaceId,
) -> IndexEvidenceAttach {
    if let Err(e) = service.attach(workspace) {
        return IndexEvidenceAttach::Broken(e.to_string());
    }
    if service.view(workspace).is_none() {
        return IndexEvidenceAttach::NoReadyGeneration;
    }
    IndexEvidenceAttach::Ready
}

/// States meaning "an operation is in flight" (mirror of the session
/// layer's `is_op_active`; the runtime may not reach into its internals).
pub(crate) fn state_is_op_active(s: AgentState) -> bool {
    matches!(
        s,
        AgentState::Preparing
            | AgentState::BuildingContext
            | AgentState::WaitingForModel
            | AgentState::Streaming
            | AgentState::ToolRequested
            | AgentState::WaitingForPermission
            | AgentState::ExecutingTool
            | AgentState::Validating
            | AgentState::UpdatingMemory
    )
}

/// Hash of one retrieved evidence set (path + bounded snippet per row).
/// Insertion order never leaks: rows sort by path first.
pub(crate) fn evidence_set_hash(evidence: &[Evidence]) -> u64 {
    let mut rows: Vec<(&str, &str)> = evidence
        .iter()
        .map(|e| (e.path.as_str(), e.snippet.as_str()))
        .collect();
    rows.sort();
    let mut parts: Vec<Vec<u8>> = Vec::with_capacity(rows.len() * 2);
    for (path, snippet) in rows {
        parts.push(path.as_bytes().to_vec());
        parts.push(snippet.chars().take(200).collect::<String>().into_bytes());
    }
    let refs: Vec<&[u8]> = parts.iter().map(|p| p.as_slice()).collect();
    evidence_fold_hash(&refs)
}

/// Deterministic bounded token claim of one producer evidence slice (the
/// generic estimator's chars/3 floor, saturating): the volatile allocation
/// only needs a bounded demand, never an exact count.
pub(crate) fn estimate_evidence_tokens(evidence: &[Evidence]) -> u32 {
    evidence.iter().fold(0u32, |acc, row| {
        acc.saturating_add(u32::try_from(row.snippet.len() / 3 + 1).unwrap_or(u32::MAX))
    })
}

/// Evidence JSON list caps (the review value rides the durable
/// verification record, whose reviewer-JSON bound is 16 KiB; the caps keep
/// the evidence comfortably below it).
pub(crate) const REVIEW_EVIDENCE_MAX_FILES: usize = 24;

/// Typed refusal of the high-risk evidence-hash gate (audit 16). Every
/// variant is a REFUSAL to rely on the evidence, never a silent degrade:
/// the caller records it as a blocking reason, so the change cannot
/// complete on drifted bytes; the next turn refreshes against the new state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EditEvidenceRefusal {
    /// The durable expectation is not a well-formed hash.
    MalformedExpectation { path: String },
    /// The file's CURRENT bytes differ from the edit service's expectation.
    Drifted {
        path: String,
        expected: String,
        actual: String,
    },
    /// The edit expected the file deleted, but it exists (or cannot be
    /// proven absent).
    UnexpectedlyPresent { path: String },
    /// The file could not be read for the check (not proof of absence).
    Unreadable { path: String, reason: String },
    /// The checkpoint/expectation STORE could not be read: the recorded
    /// expectations are unknown, never "there were none" (audit P0/P1 —
    /// anti-drift evidence fails closed).
    ExpectationsUnavailable { reason: String },
}

impl std::fmt::Display for EditEvidenceRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "high-risk evidence refused (refresh required): ")?;
        match self {
            EditEvidenceRefusal::MalformedExpectation { path } => {
                write!(f, "{path} carries a malformed durable edit hash")
            }
            EditEvidenceRefusal::Drifted {
                path,
                expected,
                actual,
            } => write!(
                f,
                "{path} currently hashes to {actual} but the edit service expects {expected}"
            ),
            EditEvidenceRefusal::UnexpectedlyPresent { path } => write!(
                f,
                "the edit service expects {path} deleted, but it still exists"
            ),
            EditEvidenceRefusal::Unreadable { path, reason } => {
                write!(f, "{path} cannot be read for the edit-hash check: {reason}")
            }
            EditEvidenceRefusal::ExpectationsUnavailable { reason } => write!(
                f,
                "the recorded edit expectations are unreadable ({reason}); \
                 failing closed instead of reviewing without them"
            ),
        }
    }
}

/// Audit 16 high-risk evidence gate: before a high-risk edit/review relies
/// on per-file evidence, every changed file's CURRENT bytes must hash to the
/// edit/checkpoint authority's newest recorded expected hash for that path.
/// A file that moved under the evidence, an unreadable file and an
/// expected-but-present deletion are TYPED refusals — the caller must
/// refresh, never review stale bytes. A path with NO durable expectation has
/// nothing to drift FROM: the review fetch binds the evidence to the current
/// bytes (the refresh branch), so it is not a refusal.
pub(crate) fn require_edit_evidence_hashes(
    deps: &AgentDeps,
    handle: &faktor_session::SessionHandle,
    ws: &faktor_fs::WorkspaceHandle,
    changed: &[String],
) -> Result<(), EditEvidenceRefusal> {
    require_edit_evidence_hashes_for_store(deps.snapshots.as_deref(), handle, ws, changed)
}

/// The store-parameterized core of [`require_edit_evidence_hashes`] so the
/// fail-closed behavior is directly testable (audit P0/P1).
pub(crate) fn require_edit_evidence_hashes_for_store(
    snapshots: Option<&faktor_snapshot::CheckpointStore>,
    handle: &faktor_session::SessionHandle,
    ws: &faktor_fs::WorkspaceHandle,
    changed: &[String],
) -> Result<(), EditEvidenceRefusal> {
    // Audit P0/P1: a checkpoint-STORE read failure is NOT "no prior
    // expectations" — the anti-drift gate would otherwise lose exactly the
    // evidence it exists to enforce. Only an absent store is the empty case.
    let rows = match snapshots {
        None => Vec::new(),
        Some(snapshots) => match snapshots.checkpoints(handle.id()) {
            Ok(rows) => rows,
            Err(e) => {
                return Err(EditEvidenceRefusal::ExpectationsUnavailable {
                    reason: e.to_string(),
                })
            }
        },
    };
    for path in changed
        .iter()
        .take(faktor_verify::review::REVIEW_MAX_CHANGED_FILES)
    {
        let latest = rows
            .iter()
            .filter(|r| &r.path == path)
            .max_by_key(|r| r.sequence);
        let Some(row) = latest else {
            // Refresh: no durable edit to compare against; the caller's
            // evidence is bound to the bytes it just read.
            continue;
        };
        if !row.after_exists {
            // The expectation is a deletion: any readable content violates it.
            if ws
                .hash_file_streaming(std::path::Path::new(path), None)
                .is_ok()
            {
                return Err(EditEvidenceRefusal::UnexpectedlyPresent { path: path.clone() });
            }
            continue;
        }
        let Some(expected) = file_hash_from_row(&row.after_hash) else {
            return Err(EditEvidenceRefusal::MalformedExpectation { path: path.clone() });
        };
        match ws.hash_file_streaming(std::path::Path::new(path), None) {
            Ok((_, actual)) if actual == expected => {}
            Ok((_, actual)) => {
                return Err(EditEvidenceRefusal::Drifted {
                    path: path.clone(),
                    expected: expected.to_hex(),
                    actual: actual.to_hex(),
                });
            }
            Err(e) => {
                return Err(EditEvidenceRefusal::Unreadable {
                    path: path.clone(),
                    reason: e.to_string(),
                });
            }
        }
    }
    Ok(())
}

/// BLAKE3 (length-prefixed) digest of the ordered changed-file evidence
/// (path + digest rows, sorted): the candidate's changed-files digest.
pub(crate) fn changed_files_fold(files: &[FileStateEvidence]) -> String {
    let mut rows: Vec<(&str, &str)> = files
        .iter()
        .map(|f| (f.path.as_str(), f.digest_hex.as_str()))
        .collect();
    rows.sort();
    let mut hasher = blake3::Hasher::new();
    for (path, digest) in rows {
        for part in [path.as_bytes(), digest.as_bytes()] {
            hasher.update(&(part.len() as u64).to_le_bytes());
            hasher.update(part);
        }
    }
    hasher.finalize().to_hex().to_string()
}

impl AgentRuntime {
    /// Concepts from the retrieval signal (spec §20): the prompt's own
    /// words first, then basename tokens of the changed files (so edited
    /// files rank for follow-up), then failure keywords. Bounded, deduped —
    /// mirrors `faktor_cli::evidence::RepoEvidence::concepts` so the
    /// index-backed path and the bounded-scan fallback agree.
    pub(crate) fn evidence_concepts(query: &EvidenceQuery) -> Vec<String> {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        let push_tokens = |text: &str, out: &mut Vec<String>, seen: &mut HashSet<String>| {
            for tok in faktor_index::tokenize(text).into_iter().take(512) {
                if tok.len() < INDEX_EVIDENCE_CONCEPT_MIN_CHARS || !seen.insert(tok.clone()) {
                    continue;
                }
                out.push(tok);
                if out.len() >= INDEX_EVIDENCE_CONCEPT_MAX {
                    return;
                }
            }
        };
        push_tokens(&query.prompt, &mut out, &mut seen);
        if out.len() < INDEX_EVIDENCE_CONCEPT_MAX {
            for f in query.changed_files.iter().take(16) {
                let base = f.rsplit('/').next().unwrap_or(f.as_str());
                push_tokens(base, &mut out, &mut seen);
                if out.len() >= INDEX_EVIDENCE_CONCEPT_MAX {
                    break;
                }
            }
        }
        if out.len() < INDEX_EVIDENCE_CONCEPT_MAX {
            push_tokens(&query.failures.join(" "), &mut out, &mut seen);
        }
        out
    }

    /// The durable task facts one compile is generated from (audit: Task +
    /// active WorkItem + criteria + failures + verification state). Rows are
    /// the durable ones: the typed task row's acceptance criteria and state,
    /// the session ledger's goal/failures/changed files. A missing row is
    /// neutral (empty criteria/failures) — never a synthetic success; an
    /// UNREADABLE identity/ledger is a typed error (authority law), never
    /// fabricated facts.
    pub(crate) fn task_facts_for(
        &self,
        handle: &faktor_session::SessionHandle,
        ledger: &TaskLedger,
        task_id: TaskId,
    ) -> faktor_core::Result<TaskFacts> {
        // Authority law: an unreadable session identity or task ledger is a
        // TYPED error, never fabricated facts (guessed workspace 1 / empty
        // tasks); the compiler caller keeps producer evidence on error.
        let workspace_id = handle.identity()?.workspace_id;
        let tasks = handle.list_tasks()?;
        let task = tasks
            .iter()
            .find(|t| t.task_id == task_id)
            .or_else(|| tasks.first());
        // Typed criteria (audits 56/57/105): the durable criterion's stable
        // content id, requirement, origin, explicit evidence edge and
        // semantic snapshot ride the compiler facts — never raw strings.
        let criteria: Vec<CriterionFact> = task
            .map(|t| t.criteria())
            .unwrap_or_default()
            .into_iter()
            .map(|c| CriterionFact {
                id: c.id.to_string(),
                text: c.text,
                requirement: c.requirement,
                origin: c.origin,
                evidence_source: c.evidence_source.map(EvidenceId),
                semantic_snapshot: c.semantic_snapshot,
            })
            .collect();
        let verification_state = match task.map(|t| t.state) {
            Some(TaskState::VerifiedComplete) => VerificationState::Passed,
            Some(TaskState::NeedsVerification) | Some(TaskState::Verifying) => {
                VerificationState::Pending
            }
            Some(TaskState::Failed) => VerificationState::Failed,
            _ => VerificationState::Unknown,
        };
        let mut failures = ledger.known_failures.clone();
        for test in &ledger.tests_failed {
            if !failures.contains(test) {
                failures.push(test.clone());
            }
        }
        let active_work_item = ledger.open_steps.first().map(|step| WorkItem {
            id: "step:0".to_string(),
            title: step.clone(),
            state: "open".to_string(),
            paths: ledger.changed_files.clone(),
            criteria: Vec::new(),
        });
        Ok(TaskFacts {
            session_id: handle.id(),
            workspace_id,
            task_id: Some(task_id.raw()),
            goal: ledger.goal.clone(),
            active_work_item,
            criteria,
            failures,
            verification_state,
            owned_paths: ledger.changed_files.clone(),
            changed_files: ledger.changed_files.clone(),
            // The current semantic snapshot is unknown at fact-build time
            // (the per-turn consult runs later): None is an honest
            // absence — never a positive mismatch — so no evidence edge is
            // falsely marked stale.
            semantic_snapshot: None,
        })
    }

    /// Archive this turn's PRODUCER evidence (index/cold retrieval, semantic
    /// DATA, learning corpus) into the durable evidence authority. Producers
    /// normalize/compress through the evidence layer; failures are logged
    /// and skipped — a producer can never fail the turn.
    pub(crate) fn archive_turn_producers(
        &self,
        handle: &faktor_session::SessionHandle,
        task_id: TaskId,
        repo: &[Evidence],
        semantic: &[Evidence],
        learning: &[Evidence],
    ) {
        // Never archive under a GUESSED workspace: an unreadable session
        // identity skips the producer archive with a typed warn (the archive
        // is auxiliary; the turn is unaffected).
        let workspace_id = match handle.identity() {
            Ok(identity) => identity.workspace_id,
            Err(e) => {
                tracing::warn!(
                    session = %handle.id(),
                    error = %e,
                    "evidence archive skipped: session identity unreadable"
                );
                return;
            }
        };
        let sources: [(&[Evidence], EvidenceKind, ProvenanceSource); 3] = [
            (repo, EvidenceKind::FileMap, ProvenanceSource::Repository),
            (
                semantic,
                EvidenceKind::SemanticContext,
                ProvenanceSource::SemanticProvider,
            ),
            (
                learning,
                EvidenceKind::StructuredRows,
                ProvenanceSource::Verification,
            ),
        ];
        for (entries, kind, provenance) in sources {
            for entry in entries {
                if entry.snippet.is_empty() {
                    continue;
                }
                if let Err(err) = self.evidence_authority.archive_text(
                    handle.id(),
                    workspace_id,
                    Some(task_id.raw()),
                    kind,
                    Some(entry.path.as_str()),
                    provenance,
                    &entry.snippet,
                    faktor_context::compactor::EVIDENCE_COMPACT_BODY_MAX_BYTES,
                ) {
                    tracing::debug!(
                        session = %handle.id(),
                        path = %entry.path,
                        "producer evidence archive skipped: {err}"
                    );
                }
            }
        }
    }

    /// Archive this turn's typed advisory evidence-poll status into the SAME
    /// durable evidence authority as the producers (schema v21): one
    /// LOSSLESS [`EvidenceKind::GenericText`] envelope whose body is the
    /// canonical bounded encoding
    /// ([`encode_evidence_poll_status`]), so reopening the store
    /// reconstructs the EXACT typed status — "retrieval failed" and "no
    /// evidence found" can never collapse into the same durable fact. The
    /// source revision is per logical turn (`evidence-poll:<turn_op>`), so
    /// an identical re-poll of that turn dedupes; the provenance is
    /// [`ProvenanceSource::Verification`] (runtime machinery output, never
    /// instruction authority). Failures are logged and skipped like every
    /// producer: durable diagnostics can never fail the turn.
    pub(crate) fn archive_turn_evidence_poll(
        &self,
        handle: &faktor_session::SessionHandle,
        task_id: TaskId,
        turn_op: OpId,
        status: &crate::EvidencePollStatus,
    ) {
        // Authority law: an unreadable session identity is never a fabricated
        // workspace (the sibling `task_facts_for` refuses typed). The
        // diagnostic archive is SKIPPED loudly instead of being filed under a
        // guessed workspace id.
        let Ok(identity) = handle.identity() else {
            tracing::warn!(
                session = %handle.id(),
                turn_op = %turn_op,
                "evidence-poll status archive skipped: session identity unreadable"
            );
            return;
        };
        let workspace_id = identity.workspace_id;
        let revision = format!("evidence-poll:{turn_op}");
        let encoded = encode_evidence_poll_status(status);
        if let Err(err) = self.evidence_authority.archive_text(
            handle.id(),
            workspace_id,
            Some(task_id.raw()),
            EvidenceKind::GenericText,
            Some(revision.as_str()),
            ProvenanceSource::Verification,
            &encoded,
            faktor_context::compactor::EVIDENCE_COMPACT_BODY_MAX_BYTES,
        ) {
            tracing::warn!(
                session = %handle.id(),
                turn_op = %turn_op,
                "evidence-poll status archive skipped: {err}"
            );
        }
    }

    /// Compile the turn's evidence through THE durable authority and the
    /// information-gain selector. Returns:
    ///
    /// - `Ok(None)` when the flag is off, nothing matched, or the compiler
    ///   selected nothing: the caller keeps the producer evidence
    ///   byte-for-byte (neutral parity);
    /// - `Ok(Some(selected))` with exactly the compiled evidence (required
    ///   needs covered, redundant S/M/L variants dropped);
    /// - `Err(CompilerError)` for a retrieval failure or an envelope
    ///   overflow: the caller logs it and keeps the producer evidence (a
    ///   required-evidence overflow is never answered by silently dropping
    ///   content).
    ///
    /// `volatile` carries the competing history/evidence/handoff/tool-note
    /// token claims of this turn: the compiler allocates the evidence
    /// envelope from them by marginal information (required evidence is
    /// hard-reserved), within its explicit memory/runtime ceilings — never a
    /// fixed third of the context.
    pub(crate) fn compile_turn_evidence(
        &self,
        handle: &faktor_session::SessionHandle,
        task_id: TaskId,
        ledger: &TaskLedger,
        budget: &ContextBudget,
        volatile: VolatileClaims,
    ) -> Result<Option<Vec<Evidence>>, CompilerError> {
        let Some(compiler) = self.context_compiler() else {
            return Ok(None);
        };
        let facts = self
            .task_facts_for(handle, ledger, task_id)
            .map_err(|e| CompilerError::Selection(format!("task facts unreadable: {e}")))?;
        let input = CompilerInput::new(
            facts,
            u32::try_from(budget.context_max()).unwrap_or(u32::MAX),
        )
        .with_volatile(volatile);
        let compiled = compiler.compile(&input)?;
        if compiled.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            compiled
                .selected
                .iter()
                .map(|item| Evidence {
                    path: item.path.clone(),
                    snippet: item.body.clone(),
                    score: if item.required { 1.0 } else { item.score },
                })
                .collect(),
        ))
    }

    /// Durable learning hook dispatch (audits 65-67/92), gated by
    /// [`EfficiencyFlags::failure_learning`] exactly like the planner prior:
    ///
    /// - a `FailedVerification` gate records the failed attempt as an
    ///   UNVERIFIED episode (attempted action + failure fingerprint +
    ///   environment, built from the durable per-attempt verification
    ///   record). The miner never turns it into a learning — "failed alone"
    ///   mints nothing;
    /// - a `VerifiedComplete` gate mines ONLY when a prior unverified
    ///   episode exists: the recovered episode keeps the failed identity,
    ///   adds the recovery chain from the PASSED record's changed files and
    ///   the durable verification record id, and `mine_and_store` persists
    ///   the learning into the same durable ledger corpus.
    ///
    /// Flag off is a strict no-op (no learning rows, byte-parity durable
    /// state). Errors are loud: a corrupt learning row refuses the hook
    /// instead of being silently dropped.
    pub(crate) fn record_learning_for_gate(
        &self,
        handle: &faktor_session::SessionHandle,
        gate: Option<&CompletionGate>,
    ) -> Result<(), faktor_learning::LearningError> {
        if !self.deps.efficiency.failure_learning {
            return Ok(());
        }
        match gate {
            Some(CompletionGate::FailedVerification { .. }) => self.record_failed_attempt(handle),
            Some(CompletionGate::VerifiedComplete) => self.mine_verified_recovery(handle),
            _ => Ok(()),
        }
    }

    /// The newest durable verification record of `handle`'s task with the
    /// given status — the succeeded/failed attempt the learning hook reads
    /// its evidence from.
    pub(crate) fn latest_attempt_record(
        handle: &faktor_session::SessionHandle,
        status: VerificationStatus,
    ) -> Result<Option<faktor_session::VerificationRecord>, faktor_learning::LearningError> {
        // P2-VERIFY: a store read failure is an ERROR, not "no record" — the
        // learning hooks must not silently skip on a broken ledger.
        let task_id = handle.task_id().map_err(|e| {
            faktor_learning::LearningError::Store(format!("task id unreadable: {e}"))
        })?;
        let records = handle.list_verification_records(task_id).map_err(|e| {
            faktor_learning::LearningError::Store(format!("verification records unreadable: {e}"))
        })?;
        Ok(records
            .into_iter()
            .filter(|record| record.status == status)
            .max_by_key(|record| record.record_id.raw()))
    }

    /// Durable learning-corpus DATA for the turn's evidence (audits
    /// 65-69/82): with `failure_learning` on, the session's durable
    /// `learning_record` ledger rows are read through
    /// [`faktor_learning::SessionLearningStore`] and every learning whose
    /// failure fingerprint matches one of the recent durable FAILED
    /// verification attempts contributes ONE metadata-only evidence item
    /// addressed by `learning:<failure-digest>`. The wire planner maps that
    /// path onto the candidate's `omission_keys`, which the production
    /// failure prior resolves against the same durable corpus — closing the
    /// loop from mined verification failures to context selection.
    ///
    /// Bounded and safe: at most [`LEARNING_EVIDENCE_MAX`] items drawn from
    /// at most [`LEARNING_FAILURE_SCAN`] recent failed records; the body
    /// carries digests, ids and numbers only (never advice text or
    /// instructions) wrapped in the DATA markers; a corpus read error
    /// (hostile/corrupt ledger row) is LOUD but non-fatal — no learning
    /// evidence this turn, never a failed turn; flag off or an empty corpus
    /// emits nothing (byte parity).
    pub(crate) fn learning_corpus_evidence(
        &self,
        handle: &faktor_session::SessionHandle,
    ) -> Vec<Evidence> {
        use faktor_learning::LearningStore as _;

        if !self.deps.efficiency.failure_learning {
            return Vec::new();
        }
        let task_id = match handle.task_id() {
            Ok(task_id) => task_id,
            Err(error) => {
                // Auxiliary corpus: a task-id read failure is logged typed,
                // never silently equal to "no learning evidence".
                tracing::warn!(
                    session = %handle.id(),
                    %error,
                    "learning corpus evidence skipped: task id unreadable"
                );
                return Vec::new();
            }
        };
        let records = match handle.list_verification_records(task_id) {
            Ok(records) => records,
            Err(error) => {
                tracing::warn!(
                    session = %handle.id(),
                    %error,
                    "learning evidence: failed-attempt records unreadable; no learning evidence (neutral)"
                );
                return Vec::new();
            }
        };
        // The turn's current failure fingerprints: the most recent durable
        // FAILED verification attempts, reconstructed with the SAME
        // descriptor shape the mining hook uses, so they match the mined
        // learning's stored failure identity exactly.
        let mut failed: Vec<&faktor_session::VerificationRecord> = records
            .iter()
            .filter(|record| record.status == VerificationStatus::Failed)
            .collect();
        failed.sort_by_key(|record| std::cmp::Reverse(record.record_id.raw()));
        failed.truncate(LEARNING_FAILURE_SCAN);
        let current: Vec<FileHash> = failed
            .iter()
            .filter_map(|record| Self::record_failure_fingerprint(record))
            .map(|failure| failure.digest())
            .collect();
        if current.is_empty() {
            return Vec::new();
        }
        let corpus = match faktor_learning::SessionLearningStore::open(
            handle.clone(),
            faktor_learning::DEFAULT_MEMORY_CAPACITY,
        ) {
            Ok(corpus) => corpus,
            Err(error) => {
                tracing::error!(
                    session = %handle.id(),
                    %error,
                    "learning evidence: durable corpus unreadable (hostile/corrupt row?); no learning evidence this turn (neutral)"
                );
                return Vec::new();
            }
        };
        let mut out = Vec::with_capacity(LEARNING_EVIDENCE_MAX);
        for learning in corpus.all() {
            let failure = learning.pattern.failure.digest();
            if !current.contains(&failure) {
                continue;
            }
            out.push(Evidence {
                path: format!("learning:{}", failure.to_hex()),
                snippet: Self::render_learning_evidence(learning),
                // Deterministic mid-rank, mirroring semantic provider DATA:
                // learning metadata never displaces the repository's own
                // retrieved evidence; the prior protects it up to 2x where
                // the corpus has a matching entry.
                score: 0.5,
            });
            if out.len() >= LEARNING_EVIDENCE_MAX {
                break;
            }
        }
        out
    }

    /// One metadata-only DATA block for a matched learning. The body is
    /// built from hex digests, ids and numbers (plus the crate-owned
    /// invalidation tag) only: no advice text, no project key — nothing a
    /// hostile ledger row could launder into a marker or an instruction.
    pub(crate) fn render_learning_evidence(learning: &faktor_learning::ProjectLearning) -> String {
        let pattern = learning.pattern_digest().to_hex();
        let failure = learning.pattern.failure.digest().to_hex();
        let body = format!(
            "learning pattern={pattern} failure={failure} workspace={} confidence_ppm={} samples={} invalidation={}",
            learning.pattern.project.workspace_id.raw(),
            learning.confidence_ppm,
            learning.sample_count,
            learning.invalidation.reason(),
        );
        format!("{LEARNING_EVIDENCE_DATA_MARKER}\n{body}\n{LEARNING_EVIDENCE_DATA_END}")
    }

    /// Build one failure episode from DURABLE data only: the per-attempt
    /// verification record's environment fingerprint, its first changed
    /// file (the attempted action) and its failed check (the failure
    /// fingerprint); the episode id is the record id (durable, unique).
    /// `None` when the record carries no changed file or no check — there
    /// is no honest action/failure identity to learn from.
    pub(crate) fn learning_episode_from_record(
        &self,
        handle: &faktor_session::SessionHandle,
        record: &faktor_session::VerificationRecord,
    ) -> Option<faktor_learning::FailureEpisode> {
        use faktor_learning::{
            ActionDescriptor, ActionFingerprint, EnvironmentFingerprint, EpisodeId,
            FailureDescriptor, FailureEpisode, FailureFingerprint, ProjectScope, TaskClass,
        };

        let changed = record.changed_files.first()?;
        let failed_check = record
            .checks
            .iter()
            .find(|check| check.status == VerificationStatus::Failed)
            .or_else(|| record.checks.first())?;
        // Project identity: workspace id from the durable record, project
        // key from the session's durable workspace root when resolvable
        // (honest fallback, never a guess at a private repo identity).
        let project_key = match self.deps.session.resolve_workspace_root(handle.id()) {
            Ok(root) => root,
            Err(e) => {
                // Typed warn: the hint degrades to no project key, never a
                // guessed identity, and the failure is visible.
                tracing::warn!(
                    error = %e,
                    "project key hint skipped: workspace root unreadable"
                );
                None
            }
        }
        .and_then(|root| {
            root.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .filter(|key| !key.is_empty())
        .unwrap_or_else(|| "workspace".to_string());
        let scope = ProjectScope::new(record.workspace_id, &project_key).ok()?;
        let (platform, toolchain, source_hash) = match &record.environment_fingerprint {
            Some(fingerprint) => (
                fingerprint.platform.clone(),
                fingerprint
                    .toolchain_versions
                    .first()
                    .map(|tool| format!("{}@{}", tool.tool, tool.version))
                    .unwrap_or_else(|| "unknown".to_string()),
                fingerprint
                    .base_tree_hash
                    .as_deref()
                    .and_then(FileHash::from_hex),
            ),
            None => (
                std::env::consts::OS.to_string(),
                "unknown".to_string(),
                None,
            ),
        };
        let environment =
            EnvironmentFingerprint::new(scope, &platform, &toolchain, source_hash).ok()?;
        let attempted_action = ActionFingerprint::of(
            &ActionDescriptor::new(
                "edit",
                &changed.path,
                None,
                "verification attempt changed the file",
            )
            .ok()?,
        );
        let failure = FailureFingerprint::of(
            &FailureDescriptor::new(
                "verification_failure",
                Some(&failed_check.check),
                failed_check
                    .summary
                    .as_deref()
                    .unwrap_or(failed_check.check.as_str()),
            )
            .ok()?,
        );
        Some(FailureEpisode::new(
            EpisodeId::new(record.record_id.raw()),
            TaskClass::new("coding").ok()?,
            environment,
            attempted_action,
            failure,
        ))
    }

    /// The recovery chain of a PASSED attempt: one action fingerprint per
    /// changed file (bounded). `None` when nothing changed — no recovery
    /// chain means no mineable episode.
    pub(crate) fn learning_recovery_actions(
        record: &faktor_session::VerificationRecord,
    ) -> Option<Vec<faktor_learning::ActionFingerprint>> {
        use faktor_learning::{ActionDescriptor, ActionFingerprint};
        let mut actions = Vec::new();
        for file in &record.changed_files {
            if actions.len() >= faktor_learning::episode::MAX_RECOVERY_ACTIONS {
                break;
            }
            actions.push(ActionFingerprint::of(
                &ActionDescriptor::new(
                    "edit",
                    &file.path,
                    None,
                    "recovery attempt changed the file",
                )
                .ok()?,
            ));
        }
        (!actions.is_empty()).then_some(actions)
    }

    /// Mine the verified recovery into the session's durable learning
    /// corpus, but ONLY when a failed attempt preceded it. The recovered
    /// episode reuses the durable FAILED identity, adds the PASSED attempt's
    /// recovery chain and its durable verification record id. Mining runs
    /// FIRST (a crash before the recovered episode row leaves the pending
    /// failure durable, so a later verified end re-mines idempotently), then
    /// the recovered episode is appended.
    pub(crate) fn mine_verified_recovery(
        &self,
        handle: &faktor_session::SessionHandle,
    ) -> Result<(), faktor_learning::LearningError> {
        let Some(passed) = Self::latest_attempt_record(handle, VerificationStatus::Passed)? else {
            return Ok(());
        };
        let Some(recovery_actions) = Self::learning_recovery_actions(&passed) else {
            return Ok(());
        };
        let store = faktor_learning::SessionLearningStore::open(
            handle.clone(),
            faktor_learning::DEFAULT_MEMORY_CAPACITY,
        )?;
        let Some(pending) = store.latest_pending().cloned() else {
            // No failed attempt preceded this success: nothing is mined.
            return Ok(());
        };
        let recovered = pending
            .with_recovery_actions(recovery_actions)?
            .verified(passed.record_id);
        let mut service = faktor_learning::LearningService::new(store);
        service.mine_and_store(std::slice::from_ref(&recovered))?;
        let mut store = service.into_store();
        store.record_episode(&recovered)?;
        // Consume the pending failures this recovery resolved: a later
        // verified completion must never re-pair them into new samples.
        store.consume_pending_episodes()?;
        Ok(())
    }

    /// The durable IndexService for this runtime, created lazily ONCE from
    /// the session store + workspace registry (data root next to the store
    /// file). `None` when hosting fails — the bounded evidence scan is then
    /// always used (never a broken first prompt).
    pub(crate) fn index_service(&self) -> Option<std::sync::Arc<faktor_index::IndexService>> {
        let once = self.index_service.get_or_init(|| {
            let store = self.deps.session.store();
            let data_root = store
                .path()
                .parent()
                .map(|p| p.join("index_data"))
                .unwrap_or_else(|| std::path::PathBuf::from("index_data"));
            // DI: the daemon's injected supervisor roots every cold git/rg
            // child of this index host. Standalone hosts without a
            // supervisor fall back to the typed standalone constructor.
            match match self.deps.supervisor.clone() {
                Some(supervisor) => faktor_index::IndexService::open_with_supervisor(
                    store.clone(),
                    data_root,
                    self.deps.workspaces.clone(),
                    supervisor,
                ),
                None => faktor_index::IndexService::open(
                    store.clone(),
                    data_root,
                    self.deps.workspaces.clone(),
                ),
            } {
                Ok(svc) => {
                    // Embedding persistence wiring (no re-embed of unchanged
                    // chunks): the configured embedder + its stable identity
                    // reach the index build through the evidence seam. No
                    // configured embedder => lexical/symbol-only builds.
                    if let Some((source, model)) = self.deps.evidence.index_embedding() {
                        svc.set_embedding_source(Some(source), model);
                    }
                    tracing::info!("repository IndexService hosted");
                    Some(svc)
                }
                Err(e) => {
                    tracing::warn!("repository IndexService unavailable: {e}");
                    None
                }
            }
        });
        once.clone()
    }

    /// Read-only health snapshot of the lazily-hosted index worker:
    /// `Some(status)` exactly when the service was ever opened, `None` when
    /// it was never requested. Never opens the service as a side effect and
    /// never starts the worker (dead-generation reconciliation inside
    /// [`faktor_index::IndexService::worker_status`] only records an already
    /// terminal state).
    pub fn index_service_worker_status(&self) -> Option<faktor_index::WorkerStatus> {
        self.index_service
            .get()
            .and_then(|service| service.as_ref())
            .map(|service| service.worker_status())
    }

    /// Read-only coverage diagnostics of the lazily-hosted index service
    /// (audits 5/6): `Some(snapshot)` exactly when the service was ever
    /// opened AND the workspace is attached. Never opens the service and
    /// never starts a build as a side effect; `None` is the honest "no
    /// hosted index to report" (never a fabricated complete coverage).
    pub fn index_coverage_snapshot(
        &self,
        workspace: faktor_core::WorkspaceId,
    ) -> Option<faktor_index::IndexCoverageSnapshot> {
        self.index_service
            .get()
            .and_then(|service| service.as_ref())
            .and_then(|service| service.coverage_snapshot(workspace))
    }

    /// First-turn evidence swap (audits 30/64): `Some(evidence)` only when
    /// the session's workspace resolves AND the IndexService has a Ready
    /// generation for it. Attaching the workspace kicks the background
    /// reconciliation worker (resume/initial build) but NEVER waits for a
    /// build; `Ok(None)` keeps the bounded evidence scan in charge until a
    /// Ready generation exists — the fallback scan is retired per workspace
    /// only then. A configured embedder's typed failure is NOT flattened: it
    /// propagates out of the fused search and fails the turn explicitly.
    pub(crate) fn index_evidence_if_ready(
        &self,
        handle: &faktor_session::SessionHandle,
        query: &EvidenceQuery,
    ) -> faktor_core::Result<Option<IndexEvidencePackage>> {
        // Authority law: a session-row read failure propagates typed; only a
        // genuinely absent index service is `Ok(None)`.
        let ws = handle.row()?.workspace_id;
        let Some(service) = self.index_service() else {
            return Ok(None);
        };
        match index_attach_for_evidence(&service, ws) {
            IndexEvidenceAttach::Ready => {}
            IndexEvidenceAttach::NoReadyGeneration => {
                // Attached, no published generation yet: the documented
                // bounded-scan degrade until the worker publishes one.
                tracing::debug!(
                    workspace = ws.raw(),
                    "index has no Ready generation yet; serving the bounded evidence scan"
                );
                return Ok(None);
            }
            IndexEvidenceAttach::Broken(reason) => {
                // BROKEN ATTACH is not "not ready": the service could not
                // attach the workspace at all (hostile/unwritable root,
                // unknown workspace, poisoned service state). Logged typed so
                // the degrade is diagnosable; the bounded scan still serves.
                tracing::warn!(
                    workspace = ws.raw(),
                    error = %reason,
                    "index attach FAILED (distinct from no-ready-generation); serving the \
                     bounded evidence scan for this workspace: {reason}"
                );
                return Ok(None);
            }
        }
        let Some(view) = service.view(ws) else {
            // The generation retired between the attach check and the view
            // read: the fallback scan serves.
            tracing::debug!(
                workspace = ws.raw(),
                "index generation retired after attach; serving the bounded evidence scan"
            );
            return Ok(None);
        };
        let meta = view.evidence_package_meta();
        if query.prompt.len() > INDEX_EVIDENCE_MAX_PROMPT_BYTES {
            return Ok(Some(IndexEvidencePackage {
                evidence: Vec::new(),
                meta,
            }));
        }
        let concepts = Self::evidence_concepts(query);
        if concepts.is_empty() {
            return Ok(Some(IndexEvidencePackage {
                evidence: Vec::new(),
                meta,
            }));
        }
        // The CONFIGURED embedder (resolved from `[embeddings]` and exposed
        // by the evidence provider) fuses the semantic leg; `None` keeps
        // lexical/symbol-only retrieval (explicit `Disabled` semantics).
        // A configured-but-failing embedder propagates its typed error —
        // retrieval NEVER silently substitutes lexical-only evidence.
        let search = faktor_search::SearchService::new(view.index(), self.deps.evidence.embedder());
        let hits = search.evidence_package(ws, &concepts, INDEX_EVIDENCE_MAX_HITS)?;
        if hits.is_empty() && view.miss_needs_fallback() {
            // Audit 5: a retrieval MISS under an INCOMPLETE generation is not
            // "no match" — the bounded direct filesystem/Git ladder must run
            // before the turn claims nothing exists. The package metadata
            // (partial coverage) rides the degrade.
            tracing::info!(
                workspace = ws.raw(),
                freshness = meta.freshness.as_str(),
                indexed = meta.coverage.files_indexed,
                "index miss under incomplete coverage; serving the bounded direct fallback"
            );
            return Ok(None);
        }
        Ok(Some(IndexEvidencePackage {
            evidence: hits
                .into_iter()
                .enumerate()
                .map(|(i, h)| Evidence {
                    path: h.path,
                    snippet: h.snippet,
                    score: 1.0 / (1.0 + i as f64),
                })
                .collect(),
            meta,
        }))
    }

    /// Run the synchronous cold ladder off the turn thread and map its TYPED
    /// failure into an explicit [`crate::EvidencePollStatus`] degradation
    /// (P2): never `None`, never conflated with "no evidence". A panic is
    /// `ProviderPanicked`; an unschedulable bridge is the typed `NotSpawned`
    /// refusal. This is the ONLY bridge the production cold path uses.
    pub(crate) async fn cold_ladder_off_turn<F>(
        f: F,
    ) -> Result<faktor_index::cold::ColdEvidence, crate::EvidencePollStatus>
    where
        F: FnOnce() -> faktor_index::cold::ColdEvidence + Send + 'static,
    {
        match crate::run_off_turn_thread_outcome(f).await {
            Ok(package) => Ok(package),
            Err(failure) => Err(crate::evidence_status_from_off_turn_failure(failure)),
        }
    }

    /// Cheap cold evidence while no Ready generation exists (P0-30): the
    /// IndexService's `ColdEvidenceProvider` serves a persisted OLD
    /// generation when one exists, else targeted reads of the turn's own
    /// referenced files (+ deadline-bounded targeted search in git repos).
    /// The provider itself is synchronous and internally bounded: every
    /// git/rg child belongs to the ONE process supervisor with a 900 ms
    /// kill deadline (audit 14/26), and this wrapper runs the whole ladder
    /// off the turn thread (the TYPED bridge above) so evidence assembly
    /// never occupies a turn thread (async turn latency).
    /// `Served(..)` (possibly empty) when the IndexService is hosted;
    /// `NotHosted` only when hosting failed — the caller's legacy scan
    /// degrade; `Degraded(status)` when the ladder ran but its off-turn
    /// bridge failed — an explicit, typed degradation, never "no evidence".
    pub(crate) async fn cold_evidence_if_unready(
        &self,
        handle: &faktor_session::SessionHandle,
        query: &EvidenceQuery,
    ) -> ColdEvidenceOutcome {
        let ws = match handle.row() {
            Ok(row) => row.workspace_id,
            Err(e) => {
                // Typed warn: the documented legacy scan degrade takes over;
                // a store failure is never silently equal to "not hosted".
                tracing::warn!(
                    error = %e,
                    "cold evidence skipped: session row unreadable; falling back to the bounded scan"
                );
                return ColdEvidenceOutcome::NotHosted;
            }
        };
        let Some(service) = self.index_service() else {
            return ColdEvidenceOutcome::NotHosted;
        };
        if service.attach(ws).is_err() {
            return ColdEvidenceOutcome::NotHosted;
        }
        let Some(provider) = service.cold_provider(ws) else {
            return ColdEvidenceOutcome::NotHosted;
        };
        let cold_query = faktor_index::cold::ColdQuery {
            prompt: query.prompt.clone(),
            changed_files: query.changed_files.clone(),
            referenced_paths: Vec::new(),
            failures: query.failures.clone(),
        };
        let package = match Self::cold_ladder_off_turn(move || provider.evidence(&cold_query)).await
        {
            Ok(package) => package,
            Err(status) => return ColdEvidenceOutcome::Degraded(status),
        };
        // The provider's origin/stats carry the degrade ladder for
        // observability; evidence mapping keeps the renderer's shape
        // (scores finite in [0,1]: the wire planner clamps again). The
        // typed freshness metadata is derived BEFORE the hits move.
        let meta = package.package_meta();
        let mut out: Vec<Evidence> = package
            .hits
            .into_iter()
            .map(|h| Evidence {
                path: h.path,
                snippet: h.snippet,
                // NaN scores normalize to 0.0 (clamp() would propagate NaN).
                score: if h.score.is_nan() {
                    0.0
                } else {
                    h.score.clamp(0.0, 1.0)
                },
            })
            .collect();
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.path.cmp(&b.path))
        });
        out.truncate(INDEX_EVIDENCE_MAX_HITS);
        ColdEvidenceOutcome::Served(out, meta)
    }

    /// Bounded repository knowledge for the context (spec §8 class 3 +
    /// §26): a small deterministic file map + the workspace AGENTS.md rules.
    /// Empty when the session has no resolvable workspace — never an error.
    pub(crate) fn repo_knowledge(
        &self,
        handle: &faktor_session::SessionHandle,
    ) -> (String, String) {
        const MAX_ENTRIES: usize = 500;
        const MAX_DEPTH: usize = 6;
        const MAX_RULES_BYTES: usize = 8192;
        const SKIP: &[&str] = &[".git", "target", "node_modules", ".venv", "dist", ".hg"];
        let row = match handle.row() {
            Ok(r) => r,
            Err(_) => return (String::new(), String::new()),
        };
        // P0-48 root re-pointing: the repo map + rules come from the
        // session's EFFECTIVE root — the live shadow root while a shadowed
        // drive runs (the turn must read the world it mutates), else the
        // stored workspace root byte-identically.
        let root = match self.deps.session.resolve_workspace_root(handle.id()) {
            Ok(Some(r)) => r,
            _ => return (String::new(), String::new()),
        };
        let ws = match self.deps.workspaces.open(row.workspace_id, root) {
            Ok(w) => w,
            Err(_) => return (String::new(), String::new()),
        };
        // Project rules: AGENTS.md at the canonical root, bounded.
        let mut rules = ws
            .read_default(std::path::Path::new("AGENTS.md"))
            .ok()
            .map(|d| String::from_utf8_lossy(&d.bytes).into_owned())
            .unwrap_or_default();
        rules.truncate(MAX_RULES_BYTES);
        // Provenance guard (audit round 16): AGENTS.md is Repository data
        // and can NEVER acquire instruction authority in this runtime.
        // Content that tries to override the system prompt is not
        // instructions — it is hostile data — so the whole AGENTS.md rules
        // block is dropped from the prompt (documented), with a warn. The
        // override scan is bounded (first 256 KiB) and case/whitespace
        // insensitive.
        if !rules.is_empty() && faktor_security::contains_instruction_override(&rules) {
            tracing::warn!(
                "dropping AGENTS.md rules: instruction-override phrasing detected \
                 (repository content is data, not instruction authority)"
            );
            rules.clear();
        }
        // Lazy instructions (audit, P0-32): rules activate by scope/keyword
        // from the session's DURABLE workspace root; each active rule is
        // appended with its reason so provenance rides the context. Total
        // stays bounded by the same MAX_RULES_BYTES cap. A hostile tree
        // (oversized authority rules) is a surfaced warn — repository rules
        // are never silently truncated into the prompt.
        let prompt_hint = handle.title().unwrap_or_default();
        match self
            .deps
            .instructions_resolver
            .resolve(row.workspace_id.raw(), None)
        {
            Ok(loaded) => {
                for instr in loaded.active_for(&prompt_hint, &[]).iter().take(16) {
                    // Same provenance guard per appended rule: a repository-
                    // sourced rule that overrides is data, never appended.
                    if faktor_security::contains_instruction_override(&instr.content) {
                        tracing::warn!(
                            path = %instr.path,
                            "dropping instruction rule: instruction-override phrasing detected \
                             (repository content is data, not instruction authority)"
                        );
                        continue;
                    }
                    let head = instr.content.lines().next().unwrap_or("").to_string();
                    rules.push_str(&format!(
                        "\n## {0} ({1}, loaded: {1})\n{2}\n",
                        instr.path,
                        instr.reason_loaded,
                        head.chars().take(200).collect::<String>()
                    ));
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "workspace instructions resolve failed; repository rules skipped this turn"
                );
            }
        }
        rules.truncate(MAX_RULES_BYTES);
        // Deterministic bounded walk (sorted per dir, depth-capped).
        let mut entries: Vec<String> = Vec::new();
        let mut stack: Vec<(usize, String)> = vec![(0, String::new())];
        while let Some((depth, rel)) = stack.pop() {
            if depth > MAX_DEPTH || entries.len() >= MAX_ENTRIES {
                break;
            }
            let path = std::path::Path::new(&rel);
            let Ok(list) = ws.list(path, 200) else {
                continue;
            };
            for meta in list {
                if entries.len() >= MAX_ENTRIES {
                    break;
                }
                let name = meta
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let child = if rel.is_empty() {
                    name.clone()
                } else {
                    format!("{rel}/{name}")
                };
                if meta.path.is_dir() {
                    if !SKIP.contains(&name.as_str()) {
                        stack.push((depth + 1, child));
                    }
                } else if name != "AGENTS.md" {
                    entries.push(child);
                }
            }
        }
        entries.sort();
        let map = entries
            .iter()
            .take(MAX_ENTRIES)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        (rules, map)
    }

    /// Audit 61-64: bounded typed-memory DATA block for the cacheable head.
    /// V2 rows render through the bounded newest-first walker ("when
    /// available"); legacy rows stay on the compat path in `faktor-memory`
    /// and are not force-fed here. Memory is DATA: the block carries the
    /// explicit provenance banner and can never gain instruction authority.
    /// Best-effort — a read failure yields no block, never a failed turn.
    pub(crate) fn memory_data_block(&self, handle: &faktor_session::SessionHandle) -> String {
        const MEMORY_RENDER_BUDGET_BYTES: usize = 4096;
        let repository =
            faktor_memory::StoreRepository::new(self.deps.session.store(), handle.id());
        let query = faktor_memory::MemoryQuery::typed_for_session(handle.id());
        faktor_memory::render_for_context(&repository, &query, MEMORY_RENDER_BUDGET_BYTES)
            .map(|render| render.text)
            .unwrap_or_default()
    }

    /// Load the session's durable activation set with the typed scan verdict
    /// ([`ToolActivationScan`]): `Found` reports the durable flag (missing =
    /// fresh session, empty set), `Absent` proves it does not exist inside
    /// the bound, `BoundExhausted` means the bound ran out with older pages
    /// still present and the safe inactive default applies — LOUDLY.
    pub(crate) fn load_tool_activation(
        &self,
        handle: &faktor_session::SessionHandle,
    ) -> ToolActivationLoad {
        let mut cursor: Option<(i64, String, String)> = None;
        for _ in 0..TOOL_ACTIVATION_MAX_PAGES {
            let page = match handle.memory_facts_page(cursor.as_ref(), TOOL_ACTIVATION_PAGE) {
                Ok(page) => page,
                Err(error) => {
                    tracing::warn!(
                        session = %handle.id(),
                        "tool activation read failed: {error}"
                    );
                    return ToolActivationLoad {
                        set: ToolActivationSet::new(),
                        scan: ToolActivationScan::ReadFailed,
                    };
                }
            };
            for (kind, key, value) in &page.facts {
                if kind == TOOL_ACTIVATION_FACT_KIND && key == TOOL_ACTIVATION_FACT_KEY {
                    // Authority law: a corrupt durable activation value is an
                    // UNREADABLE read, never an empty (inactive) set: the
                    // typed `ReadFailed` scan is the refusal, exactly like a
                    // failed store read above.
                    let Ok(names) = serde_json::from_str::<Vec<String>>(value) else {
                        tracing::warn!(
                            session = %handle.id(),
                            "tool activation fact is corrupt; treating the read as failed"
                        );
                        return ToolActivationLoad {
                            set: ToolActivationSet::new(),
                            scan: ToolActivationScan::ReadFailed,
                        };
                    };
                    let mut set = ToolActivationSet::new();
                    for name in names {
                        set.activate(name);
                    }
                    return ToolActivationLoad {
                        set,
                        scan: ToolActivationScan::Found,
                    };
                }
            }
            match page.cursor {
                Some(next) if page.has_more => cursor = Some(next),
                // The walk reached the end of the fact table: the absence is
                // decisive, the session is genuinely fresh (or explicitly
                // deactivated) and the empty set is the truth.
                _ => {
                    return ToolActivationLoad {
                        set: ToolActivationSet::new(),
                        scan: ToolActivationScan::Absent,
                    }
                }
            }
        }
        // Bound exhausted and the last page still had older rows: the scan
        // cannot prove the set. Keep the safe inactive default but make the
        // degradation LOUD (a durable activation older than the window must
        // never be silently reverted to inactive) and expose the same typed
        // event to tests.
        tracing::error!(
            session = %handle.id(),
            pages = TOOL_ACTIVATION_MAX_PAGES,
            page_size = TOOL_ACTIVATION_PAGE,
            "tool activation scan bound exhausted without a decisive fact; falling back to the \
             safe inactive default (an activation fact older than the {}x{} newest-first window \
             is not applied this turn)",
            TOOL_ACTIVATION_MAX_PAGES,
            TOOL_ACTIVATION_PAGE,
        );
        #[cfg(test)]
        activation_scan_diagnostics_tests::record(
            self.deps.session.store().root(),
            activation_scan_diagnostics_tests::Diagnostic::BoundExhausted {
                pages: TOOL_ACTIVATION_MAX_PAGES,
                page_size: TOOL_ACTIVATION_PAGE,
            },
        );
        ToolActivationLoad {
            set: ToolActivationSet::new(),
            scan: ToolActivationScan::BoundExhausted,
        }
    }

    /// Persist the session's activation set. A failure is logged and the
    /// in-memory set still governs this turn (never a hard turn failure).
    pub(crate) fn store_tool_activation(
        &self,
        handle: &faktor_session::SessionHandle,
        activation: &ToolActivationSet,
    ) {
        let value = serde_json::to_string(&activation.names()).unwrap_or_else(|_| "[]".into());
        if let Err(error) =
            handle.upsert_memory_fact(TOOL_ACTIVATION_FACT_KIND, TOOL_ACTIVATION_FACT_KEY, &value)
        {
            tracing::warn!(
                session = %handle.id(),
                "tool activation write failed: {error}"
            );
        }
    }

    /// The newest user-visible text of the loaded history window. The
    /// deterministic detector only reads text parts of the newest user
    /// message; older messages are covered by the durable activation flag.
    pub(crate) fn newest_user_text(history: &[RequestMessage]) -> String {
        for message in history.iter().rev() {
            if message.role != Role::User {
                continue;
            }
            let mut text = String::new();
            for part in &message.content {
                if let ContentKind::Text { text: part_text } = &part.kind {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(part_text);
                }
            }
            return text;
        }
        String::new()
    }
}
