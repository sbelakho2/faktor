//! The `full_daemon_runtime` benchmark arm: the SAME frozen corpus tasks and
//! the SAME frozen candidate outputs as every other arm, executed by the
//! REAL [`faktor_agent::AgentRuntime`] turn loop over a REAL durable session.
//!
//! What this arm isolates (`docs/certification.md` §2.8.1): the complete
//! autonomous runtime — contract-gated completion, the independent
//! completion review, the durable completion-gate/proof facts, and the
//! deterministic re-opening of work across turns. The other two arms only
//! vary the model-side prompt/context; this arm runs the model-side OUTPUTS
//! through the runtime that would gate completion in production.
//!
//! Honesty contract:
//!
//! - **No live model call, ever.** The model seam is a deterministic
//!   frozen-output provider: it serves the frozen candidate files as
//!   `write_file` tool calls and answers the runtime's independent-review
//!   call (when the runtime decides a change is risky) with a clean verdict.
//!   There is no network use on this path and no provider not registered
//!   here.
//! - **Daemon-equivalent assembly.** The runtime is built exactly like the
//!   daemon document's construction order demands for the agent stack: ONE
//!   real `SessionManager` (durable store + CAS), ONE real `ProviderRegistry`,
//!   the real `WorkspaceFileService`, the real transactional `EditEngine`,
//!   the real CAS-backed `CheckpointStore`, the real routing policy
//!   (`FixedRoutingPolicy::passthrough` — the benchmark's fixed model), the
//!   real budget authority (`NoopBudget`: the arm makes no monetary claim)
//!   and the real `VerificationService::disabled` state (this minimal
//!   manifest-less corpus derives no project checks, so the runtime
//!   HONESTLY classifies mutating turns Unverified and records that; the
//!   objective success axis remains the task's own `sh verify.sh`, executed
//!   by the harness exactly like the daemon benchmark lane does). The
//!   production CLI `build_daemon` graph is not linkable from this
//!   test-only crate; the assembly here is the daemon-faithful subset and
//!   the artifact records what actually ran.
//! - **Frozen in, frozen out.** Capture derives the candidate files from the
//!   frozen fixture plan; replay derives them from the artifact's own
//!   recorded attempts. Both drive the identical runtime path, so capture
//!   and replay must agree byte-for-byte on prompt hashes, verification
//!   outcomes, gate labels, file contents and the final workspace digest.
//!   Two scheduling artifacts are handled explicitly so this holds under
//!   load: the frozen script declares SEQUENTIAL tool execution (parallel
//!   batches settle in completion order, which would make the next wire
//!   request ordering-dependent), and the recorded request hash normalizes
//!   the runtime's VOLATILE workspace-derived sections out of the request —
//!   transient `.faktor-tmp-*` atomic-writer entries (the between-hop scans
//!   can observe a temp file mid-write; the destination write itself is an
//!   atomic rename) and the `## Retrieved evidence` block (whose refresh
//!   goes through the runtime's async cold ladder / index-readiness path and
//!   can serve the previous hop's cached package under load). Every
//!   normalized-out section is EXPLICITLY RECORDED in the attempt's
//!   `volatile` field (rule name, exact extracted bytes, frozen rationale),
//!   so the exclusion is provable evidence instead of hidden bytes, and a
//!   pinned test proves the recorded extracts are exactly the excluded
//!   bytes. Instructions, task state, the synchronously re-resolved
//!   repository map, messages and tool specs all stay under the hash
//!   byte-for-byte.
//! - **No model-quality claim.** All of this is machinery proof; real model
//!   numbers exist only in opt-in `capture_source = live` artifacts.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use faktor_agent::{
    AgentDeps, AgentRuntime, CompletionGate, NoEvidence, PermissionRequester, RecoveryHint, Tool,
    ToolOutcome, ToolRegistry,
};
use faktor_core::capability::{Capability, PermissionDecision};
use faktor_core::error::{Error, ErrorKind};
use faktor_core::hash::FileHash;
use faktor_core::id::SessionId;
use faktor_core::model::ModelCapabilities;
use faktor_core::op::FilePostcondition;
use faktor_core::resource::ResourceClass;
use faktor_core::time::SystemClock;
use faktor_core::{compile_contract, ContractCompileRequest, RepositoryFile, RepositorySnapshot};
use faktor_provider::{
    CanonicalUsage, GenericAgentRequest, Provider, ProviderChunk, ProviderRegistry, ProviderStream,
};
use faktor_session::SessionManager;

use crate::coding::{
    copy_coding_repo, fixture_candidate, fixture_plan, run_task_verification,
    validate_candidate_path, CodingCorpus, CodingTaskSpec, CODING_VERIFY_TIMEOUT,
    MAX_CANDIDATE_ATTEMPTS, MAX_CANDIDATE_FILE_BYTES,
};
use crate::frozen::{
    Arm, CostSource, FrozenAttempt, FrozenFileChange, FrozenPrompt, FrozenRun,
    FrozenRuntimeEvidence, FrozenVerification, VerifyOutcome,
};
use crate::pipeline::estimate_tokens;
use crate::{corpus_digest, io_err};

/// Provider id of the deterministic frozen-output provider.
pub const DAEMON_ARM_PROVIDER: &str = "frozen-runtime";
/// Model id of the frozen scripted plan the runtime drives.
pub const DAEMON_ARM_MODEL: &str = "frozen-plan";
/// Deterministic latency placeholder recorded per runtime attempt.
pub const DAEMON_ARM_LATENCY_MS: u64 = 1_000;
/// Deterministic `completed_at_ms` arm offset (baseline is 0, machinery is
/// 1000): the runtime arm's records carry this offset.
pub const DAEMON_ARM_TIME_OFFSET_MS: i64 = 2_000;
/// The frozen provider's deterministic final message of one turn.
const DAEMON_FINAL_TEXT: &str = "The requested change has been applied.";
/// Stable prefix of the runtime's independent-review system prompt (used to
/// route review calls to the deterministic clean verdict; a request that
/// does not carry it is a regular turn call).
const REVIEW_SYSTEM_MARKER: &str = "You are the independent completion reviewer";
/// The deterministic clean review verdict (the shape the runtime's review
/// contract asks for).
const REVIEW_CLEAN_JSON: &str = "{\"verdict\":\"clean\",\"findings\":[]}";

// ------------------------------------------------------------------ provider

/// One scripted step of a turn: the frozen candidate files as real
/// `write_file` tool calls (the runtime executes them through the real
/// workspace write path).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ScriptedTurn {
    ToolCalls(Vec<(String, serde_json::Value)>),
}

/// Per-turn provider state (set by the driver before each turn).
#[derive(Debug, Clone, Default)]
struct TurnScript {
    task_id: String,
    attempt: u32,
    responses: Vec<ScriptedTurn>,
    served: usize,
    request_hashes: Vec<String>,
    calls: u64,
    review_calls: u64,
    tokens_in: u64,
    tokens_out: u64,
    /// Every normalization exclusion applied while hashing this turn's wire
    /// requests, in provider call order (recorded in the artifact as
    /// explicit evidence, never silently dropped).
    volatile: Vec<VolatileSection>,
}

/// The deterministic statistics of one driven turn (frozen-output evidence).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStats {
    pub task_id: String,
    pub attempt: u32,
    pub provider_calls: u64,
    pub review_calls: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// BLAKE3 over the canonical wire requests the runtime made this turn
    /// (normalized system + messages + tool specs; never the op/meta
    /// envelope). A replay must reproduce this byte-for-byte.
    pub prompt_blake3: String,
    /// Every normalization exclusion applied this turn (recorded in the
    /// artifact so the exclusion is provable, never hidden).
    pub volatile: Vec<VolatileSection>,
}

/// The deterministic frozen-output model: a pure function of the request.
pub struct FrozenOutputProvider {
    state: Mutex<TurnScript>,
}

impl FrozenOutputProvider {
    pub fn new() -> Self {
        FrozenOutputProvider {
            state: Mutex::new(TurnScript::default()),
        }
    }

    /// Arm one turn with its scripted responses.
    fn begin_turn(&self, task_id: &str, attempt: u32, responses: Vec<ScriptedTurn>) {
        let mut st = self.state.lock().expect("provider state");
        *st = TurnScript {
            task_id: task_id.to_string(),
            attempt,
            responses,
            ..Default::default()
        };
    }

    /// Close the armed turn and read its deterministic statistics.
    fn end_turn(&self) -> TurnStats {
        let mut st = self.state.lock().expect("provider state");
        let mut hasher = blake3::Hasher::new();
        hasher.update(st.task_id.as_bytes());
        hasher.update(&[0]);
        hasher.update(&st.attempt.to_le_bytes());
        for hash in &st.request_hashes {
            hasher.update(hash.as_bytes());
            hasher.update(&[0]);
        }
        let stats = TurnStats {
            task_id: st.task_id.clone(),
            attempt: st.attempt,
            provider_calls: st.calls,
            review_calls: st.review_calls,
            tokens_in: st.tokens_in,
            tokens_out: st.tokens_out,
            prompt_blake3: hasher.finalize().to_hex().to_string(),
            volatile: std::mem::take(&mut st.volatile),
        };
        *st = TurnScript::default();
        stats
    }
}

impl Default for FrozenOutputProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Rule name of the transient atomic-writer entry exclusion.
pub const VOLATILE_RULE_ATOMIC_TEMP_ENTRY: &str = "atomic_temp_writer_entry";
/// Frozen rationale of the transient atomic-writer entry exclusion.
pub const VOLATILE_RATIONALE_ATOMIC_TEMP_ENTRY: &str =
    "transient .faktor-tmp-* atomic-writer entry: the between-hop workspace scans can observe \
     a temp file while a tool is physically mid-write, so its name/content visibility is \
     scheduling-dependent, not a property of the frozen output (the destination write itself \
     is an atomic rename)";
/// Rule name of the retrieved-evidence block exclusion.
pub const VOLATILE_RULE_RETRIEVED_EVIDENCE: &str = "retrieved_evidence_block";
/// Frozen rationale of the retrieved-evidence block exclusion.
pub const VOLATILE_RATIONALE_RETRIEVED_EVIDENCE: &str =
    "## Retrieved evidence block: its refresh goes through the runtime's async cold ladder / \
     index-readiness path and can legitimately serve the previous hop's cached package under \
     load; its bytes are not a property of the frozen output";

/// TRUE when (`name`, `rationale`) is one of the two DOCUMENTED normalization
/// rules. Adding a third normalization rule requires extending this
/// vocabulary AND the pinned `volatile_evidence_records_every_excluded_byte_
/// and_only_those` test; an undocumented rule already fails artifact
/// validation (`frozen::validate_run`).
pub fn documented_volatile_rule(name: &str, rationale: &str) -> bool {
    matches!(
        (name, rationale),
        (
            VOLATILE_RULE_ATOMIC_TEMP_ENTRY,
            VOLATILE_RATIONALE_ATOMIC_TEMP_ENTRY
        ) | (
            VOLATILE_RULE_RETRIEVED_EVIDENCE,
            VOLATILE_RATIONALE_RETRIEVED_EVIDENCE
        )
    )
}

/// One normalized-out wire section: the rule name, the EXACT bytes removed
/// (so the exclusion is provable, never hidden) and the frozen rationale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolatileSection {
    pub name: &'static str,
    pub extract: String,
    pub rationale: &'static str,
}

/// The result of one wire-section normalization: the text under the hash and
/// every exclusion applied to produce it, in text order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedSystem<'a> {
    pub normalized: std::borrow::Cow<'a, str>,
    pub volatile: Vec<VolatileSection>,
}

/// Remove ONLY the two DOCUMENTED volatile regions the runtime itself
/// refreshes with its own timing before hashing a wire request:
///
/// - [`VOLATILE_RULE_ATOMIC_TEMP_ENTRY`]: each whole line containing
///   `.faktor-tmp` (exact line bytes, trailing newline included);
/// - [`VOLATILE_RULE_RETRIEVED_EVIDENCE`]: the exact `## Retrieved evidence`
///   block, from its heading line through the last line before the next
///   `## ` heading (or EOF).
///
/// The removal is a PURE byte deletion: the removed ranges are recorded
/// verbatim as [`VolatileSection`] extracts, so `normalized` plus the
/// extracts, re-inserted in order at the recorded positions, reconstruct the
/// input byte-for-byte — the recorded extracts are provably the ONLY
/// excluded bytes. Everything else (instructions, task state, the
/// synchronously re-resolved repository map, messages, tool specs) stays
/// under the hash byte-for-byte.
fn normalize_volatile_wire_sections(text: &str) -> NormalizedSystem<'_> {
    let mut volatile: Vec<VolatileSection> = Vec::new();
    let mut normalized = String::with_capacity(text.len());
    let mut offset = 0usize;
    // The active `## Retrieved evidence` block: its start offset and the
    // exact bytes accumulated so far (dropped lines of the heading + body).
    let mut evidence: Option<(usize, String)> = None;
    for line in text.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let content = line.strip_suffix('\n').unwrap_or(line);
        if let Some((_, extract)) = evidence.as_mut() {
            if content.starts_with("## ") {
                // The block ended immediately before this heading line.
                let (_, extract) = evidence.take().expect("evidence is active");
                volatile.push(VolatileSection {
                    name: VOLATILE_RULE_RETRIEVED_EVIDENCE,
                    extract,
                    rationale: VOLATILE_RATIONALE_RETRIEVED_EVIDENCE,
                });
                // Fall through: this heading line is NOT part of the block.
            } else {
                extract.push_str(line);
                continue;
            }
        }
        if content == "## Retrieved evidence" {
            evidence = Some((line_start, line.to_string()));
            continue;
        }
        if line.contains(".faktor-tmp") {
            volatile.push(VolatileSection {
                name: VOLATILE_RULE_ATOMIC_TEMP_ENTRY,
                extract: line.to_string(),
                rationale: VOLATILE_RATIONALE_ATOMIC_TEMP_ENTRY,
            });
            continue;
        }
        normalized.push_str(line);
    }
    if let Some((_, extract)) = evidence.take() {
        volatile.push(VolatileSection {
            name: VOLATILE_RULE_RETRIEVED_EVIDENCE,
            extract,
            rationale: VOLATILE_RATIONALE_RETRIEVED_EVIDENCE,
        });
    }
    if volatile.is_empty() {
        NormalizedSystem {
            normalized: std::borrow::Cow::Borrowed(text),
            volatile,
        }
    } else {
        NormalizedSystem {
            normalized: std::borrow::Cow::Owned(normalized),
            volatile,
        }
    }
}

/// Canonical wire-request hash of one runtime provider call (everything the
/// model would see; never the transient op/meta envelope). The messages and
/// tool specs are canonicalized (object keys sorted recursively) BEFORE
/// hashing: `serde_json`'s process-wide `preserve_order` feature flips
/// `Value` object key order (insertion vs sorted), and workspace-wide builds
/// enable it through other members' feature unification — so a hash over the
/// raw serialization would silently depend on the BUILD CONFIGURATION, not
/// on the model-visible content (the committed fixture was blessed with the
/// feature off, so workspace `cargo test --workspace` runs saw a phantom
/// "runtime prompt drifted" red). Key order is not model-visible semantics;
/// every other byte stays under the hash.
fn request_blake3(normalized: &NormalizedSystem<'_>, req: &GenericAgentRequest) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(normalized.normalized.as_bytes());
    hasher.update(&[0]);
    hasher.update(&canonical_json_bytes(&req.messages));
    hasher.update(&[0]);
    hasher.update(&canonical_json_bytes(&req.tools));
    hasher.finalize().to_hex().to_string()
}

/// Serialize through a recursively key-sorted `Value`, so the bytes are
/// identical whether or not the `serde_json/preserve_order` feature is
/// unified into the build.
fn canonical_json_bytes<T: serde::Serialize>(value: &T) -> Vec<u8> {
    let mut value = serde_json::to_value(value).unwrap_or(serde_json::Value::Null);
    sort_json_keys(&mut value);
    serde_json::to_vec(&value).unwrap_or_default()
}

/// Re-materialize every object's entries in sorted key order (re-insertion
/// order is the iteration order under both `serde_json` map backends).
fn sort_json_keys(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(String, serde_json::Value)> =
                std::mem::take(map).into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            for (key, mut child) in entries {
                sort_json_keys(&mut child);
                map.insert(key, child);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                sort_json_keys(item);
            }
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => {}
    }
}

/// Deterministic request token estimate: the NORMALIZED system + message
/// text + tool specs (same normalization as the recorded request hash, so
/// the accounting never depends on the volatile workspace-scan sections).
fn request_tokens_est(normalized: &NormalizedSystem<'_>, req: &GenericAgentRequest) -> u64 {
    let mut total = estimate_tokens(normalized.normalized.as_ref());
    for message in &req.messages {
        for part in &message.content {
            if let Ok(value) = serde_json::to_value(part) {
                let text = value
                    .get("text")
                    .and_then(|t| t.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| value.to_string());
                total = total.saturating_add(estimate_tokens(&text));
            }
        }
    }
    for tool in &req.tools {
        total = total.saturating_add(estimate_tokens(&tool.name));
        total = total.saturating_add(estimate_tokens(&tool.description));
    }
    total.max(1)
}

impl Provider for FrozenOutputProvider {
    fn id(&self) -> &str {
        DAEMON_ARM_PROVIDER
    }

    fn capabilities(&self, _model: &str) -> ModelCapabilities {
        ModelCapabilities {
            tools: true,
            // The frozen script serves ONE response per call and replays must
            // reproduce the wire request byte-for-byte: declaring sequential
            // tool execution keeps multi-file tool results in submit order
            // (parallel batches would settle in completion order and make the
            // second wire request scheduling-dependent).
            parallel_tools: false,
            streaming: true,
            json_schema: true,
            ..Default::default()
        }
    }

    fn stream(&self, req: GenericAgentRequest) -> ProviderStream {
        let mut chunks: Vec<Result<ProviderChunk, faktor_provider::ProviderError>> = Vec::new();
        let mut st = self.state.lock().expect("provider state");
        st.calls += 1;
        let normalized = normalize_volatile_wire_sections(&req.system);
        st.request_hashes.push(request_blake3(&normalized, &req));
        st.volatile.extend(normalized.volatile.iter().cloned());
        let input_est = request_tokens_est(&normalized, &req);
        let is_review = req.system.starts_with(REVIEW_SYSTEM_MARKER);
        let (text_out, tool_calls) = if is_review {
            st.review_calls += 1;
            (Some(REVIEW_CLEAN_JSON.to_string()), Vec::new())
        } else {
            match st.responses.get(st.served).cloned() {
                Some(ScriptedTurn::ToolCalls(calls)) => {
                    st.served += 1;
                    (None, calls)
                }
                None => (Some(DAEMON_FINAL_TEXT.to_string()), Vec::new()),
            }
        };
        let output_est = match &text_out {
            Some(text) => estimate_tokens(text),
            None => tool_calls
                .iter()
                .map(|(name, input)| {
                    estimate_tokens(name).saturating_add(estimate_tokens(&input.to_string()))
                })
                .sum::<u64>()
                .max(1),
        };
        st.tokens_in = st.tokens_in.saturating_add(input_est);
        st.tokens_out = st.tokens_out.saturating_add(output_est);
        drop(st);
        for (index, (name, input)) in tool_calls.into_iter().enumerate() {
            chunks.push(Ok(ProviderChunk::ToolCall {
                id: format!("frozen-call-{index}"),
                name,
                input,
                complete: true,
            }));
        }
        if let Some(text) = text_out {
            chunks.push(Ok(ProviderChunk::Text { text }));
        }
        chunks.push(Ok(ProviderChunk::Usage(CanonicalUsage {
            uncached_input_tokens: input_est,
            output_tokens: output_est,
            ..Default::default()
        })));
        chunks.push(Ok(ProviderChunk::Done));
        Box::pin(futures::stream::iter(chunks))
    }
}

// --------------------------------------------------------------------- tools

struct AlwaysAllow;

impl PermissionRequester for AlwaysAllow {
    fn request(
        &self,
        _session: SessionId,
        _permission: &faktor_session::ops::PermissionRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = faktor_core::Result<PermissionDecision>> + Send>,
    > {
        Box::pin(async { Ok(PermissionDecision::Allow) })
    }
}

/// The real `write_file` execution path (the SAME machinery the production
/// tool drives: workspace-relative containment, optimistic base hash,
/// transactional `EditEngine` replace, CAS-backed before/after
/// checkpoints). Content identical to the current file is the documented
/// "unchanged" no-op. `verify.sh` is structurally unreachable through
/// [`validate_candidate_path`].
fn write_file_tool() -> Tool {
    Tool {
        name: "write_file".into(),
        description: "Write a file inside the session workspace (transactional, checkpointed)."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        }),
        resource_class: ResourceClass::DiskWrite,
        capability: Some(Capability::WriteWorkspace { path: ".".into() }),
        recovery_hint: RecoveryHint::WorkspaceWrite,
        path_args: vec!["path".into()],
        execute: Arc::new(|ctx, args| {
            Box::pin(async move {
                let ws = ctx
                    .workspace
                    .clone()
                    .ok_or_else(|| Error::permission("write_file requires a workspace"))?;
                let edit = ctx.edit.clone().ok_or_else(|| {
                    Error::permission("write_file requires the edit engine (none wired)")
                })?;
                let snapshots = ctx.snapshots.clone().ok_or_else(|| {
                    Error::permission("write_file requires the checkpoint store (none wired)")
                })?;
                let path = args
                    .get("path")
                    .and_then(|p| p.as_str())
                    .ok_or_else(|| Error::malformed("write_file requires path"))?;
                let content = args
                    .get("content")
                    .and_then(|c| c.as_str())
                    .ok_or_else(|| Error::malformed("write_file requires content"))?;
                validate_candidate_path(path)
                    .map_err(|e| Error::permission(format!("write_file path refused: {e}")))?;
                if content.len() > MAX_CANDIDATE_FILE_BYTES {
                    return Err(Error::oversized(format!(
                        "write_file content exceeds {MAX_CANDIDATE_FILE_BYTES} bytes"
                    )));
                }
                let rel = Path::new(path);
                // Containment: the anchored resolution validates an EXISTING
                // path. A path whose parent chain does not exist yet (the
                // candidate adds `lib/log.sh`) is the create case; the
                // harness walks the EXISTING prefix itself and rejects any
                // symlink component, then creates the missing directories
                // explicitly (the production atomic writer only creates the
                // destination's parent after resolving it).
                let create_case = match ws.resolve(rel) {
                    Ok(_) => false,
                    Err(e) => match new_path_within(ws.root(), rel) {
                        Ok(create) => create,
                        Err(refusal) => {
                            return Err(Error::permission(format!(
                                "write_file escape: {refusal} (resolve said: {e})"
                            )));
                        }
                    },
                };
                if create_case {
                    let target = ws.root().join(rel);
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| {
                            Error::internal(format!("mkdir {}: {e}", parent.display()))
                        })?;
                    }
                }
                let postcondition = |after: FileHash| FilePostcondition {
                    workspace_id: ctx.identity.workspace_id,
                    worktree_id: ctx.identity.worktree_id,
                    relative_path: path.to_string(),
                    expected_hash: after,
                };
                let current = if create_case {
                    None
                } else {
                    match ws.read(rel, MAX_CANDIDATE_FILE_BYTES) {
                        Ok(data) => Some(data),
                        Err(e) if e.kind == ErrorKind::NotFound => None,
                        Err(e) => return Err(e),
                    }
                };
                if let Some(cur) = &current {
                    if cur.bytes == content.as_bytes() {
                        let hash = cur.full_hash().ok_or_else(|| {
                            Error::oversized(format!(
                                "{path} exceeds the {MAX_CANDIDATE_FILE_BYTES} byte write bound"
                            ))
                        })?;
                        return Ok(ToolOutcome {
                            text: format!("{path} unchanged ({} bytes)", content.len()),
                            exit_code: Some(0),
                            postcondition: Some(postcondition(hash)),
                            ..Default::default()
                        });
                    }
                    if cur.full_hash().is_none() {
                        return Err(Error::oversized(format!(
                            "{path} exceeds the {MAX_CANDIDATE_FILE_BYTES} byte write bound"
                        )));
                    }
                }
                let current_hash: Option<FileHash> = current.as_ref().and_then(|c| c.full_hash());
                if current.is_none() && content.is_empty() {
                    let hash = ws
                        .write_atomic(rel, b"")
                        .map_err(|e| Error::internal(format!("write {path}: {e}")))?;
                    return Ok(ToolOutcome {
                        text: format!("created {path} (0 bytes)"),
                        exit_code: Some(0),
                        postcondition: Some(postcondition(hash)),
                        ..Default::default()
                    });
                }
                let before_bytes: &[u8] =
                    current.as_ref().map(|c| c.bytes.as_slice()).unwrap_or(b"");
                let before = snapshots
                    .before_write(ctx.session_id, path, before_bytes)
                    .map_err(|e| Error::internal(format!("checkpoint {path}: {e}")))?;
                let after = match current_hash {
                    Some(expected) => {
                        let cur = current.as_ref().expect("hash implies a read");
                        let req = faktor_edit::EditRequest {
                            path: path.to_string(),
                            expected_hash: expected,
                            ops: vec![faktor_edit::EditOp::Range {
                                start: 0,
                                end: cur.bytes.len(),
                                replacement: content.to_string(),
                            }],
                        };
                        edit.apply(
                            &ws,
                            &ctx.identity,
                            &req,
                            faktor_edit::RepairMode::AllowModelRepair,
                        )
                        .map_err(|e| Error::internal(format!("edit {path}: {e}")))?
                        .new_hash
                    }
                    None => ws
                        .write_atomic(rel, content.as_bytes())
                        .map_err(|e| Error::internal(format!("write {path}: {e}")))?,
                };
                let sequence = snapshots
                    .checkpoints(ctx.session_id)
                    .map_err(|e| Error::internal(format!("checkpoints {path}: {e}")))?
                    .len() as i64
                    + 1;
                snapshots
                    .after_write(
                        ctx.session_id,
                        path,
                        before,
                        after,
                        sequence,
                        content.as_bytes(),
                    )
                    .map_err(|e| Error::internal(format!("checkpoint {path}: {e}")))?;
                Ok(ToolOutcome {
                    text: format!("wrote {path} ({} bytes)", content.len()),
                    exit_code: Some(0),
                    postcondition: Some(postcondition(after)),
                    ..Default::default()
                })
            })
        }),
    }
}

/// Whether `rel` (already lexically validated by
/// [`validate_candidate_path`]) is a NOT-YET-EXISTING path whose existing
/// prefix stays inside `root` without traversing a symlink. `Ok(true)` = the
/// safe create case; `Ok(false)` = the whole path exists on disk; `Err` = a
/// symlink (or a non-normal component) in the existing prefix.
fn new_path_within(root: &Path, rel: &Path) -> Result<bool, String> {
    use std::path::Component;
    let mut cur = root.to_path_buf();
    for component in rel.components() {
        match component {
            Component::Normal(name) => {
                cur.push(name);
                match std::fs::symlink_metadata(&cur) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        return Err(format!("symlink in path: {}", rel.display()));
                    }
                    Ok(_) => {}
                    Err(_) => return Ok(true),
                }
            }
            _ => return Err(format!("non-normal component in path: {}", rel.display())),
        }
    }
    Ok(false)
}

// ------------------------------------------------------------- agent assembly

/// The daemon-equivalent agent environment: ONE durable store + CAS, ONE
/// provider registry (the frozen-output provider only), ONE workspace
/// service / edit engine / checkpoint store, and the real `AgentRuntime`.
pub struct DaemonArmEnv {
    pub manager: Arc<SessionManager>,
    pub runtime: Arc<AgentRuntime>,
    pub provider: Arc<FrozenOutputProvider>,
    pub tokio: tokio::runtime::Runtime,
}

impl DaemonArmEnv {
    /// Build the environment under `root` (its store/CAS live there; the
    /// workspace copies live under `scratch`).
    pub fn open(root: &Path) -> Result<Self, String> {
        fs::create_dir_all(root).map_err(|e| io_err("create daemon-arm root", e))?;
        let manager = SessionManager::open(root.join("store"), root.join("cas"), true)
            .map_err(|e| format!("daemon-arm session manager: {e}"))?;
        let provider = Arc::new(FrozenOutputProvider::new());
        let mut registry = ProviderRegistry::new();
        registry
            .try_register(provider.clone() as Arc<dyn Provider>)
            .map_err(|e| format!("daemon-arm provider registry: {e}"))?;
        let workspaces = faktor_fs::WorkspaceFileService::new();
        let cas = manager.cas().clone();
        let store = manager.store().clone();
        let snapshots = Arc::new(faktor_snapshot::CheckpointStore::new(cas, store));
        let edit = Arc::new(faktor_edit::EditEngine::new(workspaces.clone()));
        let mut tools = ToolRegistry::new();
        tools.register(write_file_tool());
        let tokio = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| format!("daemon-arm tokio runtime: {e}"))?;
        let runtime = AgentRuntime::new(AgentDeps {
            session: manager.clone(),
            chunk_sink: None,
            providers: Arc::new(registry),
            permission_requester: Arc::new(AlwaysAllow),
            evidence: Arc::new(NoEvidence),
            tools: Arc::new(tools),
            cas: Some(manager.cas().clone()),
            workspaces,
            edit: Some(edit),
            snapshots: Some(snapshots),
            sandbox: None,
            supervisor: None,
            // The corpus derives no project checks (manifest-less repos); a
            // disabled verifier is the EXPLICIT honest state — mutating
            // turns classify Unverified and the harness's own `sh verify.sh`
            // run provides the objective axis.
            verification: faktor_agent::VerificationService::disabled(),
            model: DAEMON_ARM_MODEL.to_string(),
            compaction_model: None,
            compact_at_usage: 0.65,
            instructions: "You are a coding agent working in a small repository. Apply the \
                           requested change with write_file and report completion."
                .to_string(),
            hooks: None,
            instructions_resolver: faktor_instructions::no_roots_resolver(),
            // The benchmark's fixed model: no economic routing under test.
            routing: faktor_agent::FixedRoutingPolicy::passthrough(),
            budgets: Arc::new(faktor_session::NoopBudget),
            clock: Arc::new(SystemClock),
            tool_call_mode: faktor_agent::ToolCallMode::Native,
            tool_deadline_ms: 10_000,
            retry_policy: faktor_core::retry::RetryPolicy::default(),
            semantic: faktor_agent::fallback_semantic_registry(),
            context_prior: None,
            efficiency: Default::default(),
            secret_registry: None,
        })
        .map_err(|e| format!("daemon-arm agent runtime: {e}"))?;
        Ok(DaemonArmEnv {
            manager,
            runtime,
            provider,
            tokio,
        })
    }
}

// ------------------------------------------------------------------ the drive

/// The deterministic outcome of driving ONE task through the runtime.
pub struct RuntimeTaskDrive {
    pub attempts: Vec<FrozenAttempt>,
    pub verified: bool,
    pub first_pass: bool,
    pub final_tree_blake3: String,
}

fn repair_prompt(goal: &str, failure: Option<&str>) -> String {
    match failure {
        Some(excerpt) => format!(
            "{goal}\n\nPREVIOUS ATTEMPT FAILED VERIFICATION. Failure output:\n{}\nFix the problem and apply the complete corrected files again.",
            excerpt.trim()
        ),
        None => format!("{goal}\n\nPREVIOUS ATTEMPT DID NOT PASS VERIFICATION. Try again."),
    }
}

fn gate_label(gate: Option<&CompletionGate>) -> String {
    match gate {
        None => "none",
        Some(CompletionGate::VerifiedComplete) => "verified_complete",
        Some(CompletionGate::Unverified) => "unverified",
        Some(CompletionGate::FailedVerification { .. }) => "failed_verification",
        Some(CompletionGate::BlockedVerification { .. }) => "blocked_verification",
        Some(CompletionGate::VerificationPending) => "verification_pending",
    }
    .to_string()
}

/// One intended candidate: every file the frozen output asks to write.
type IntendedFiles = BTreeMap<String, String>;

/// The frozen candidate files of one capture attempt, from the fixture plan
/// (the last plan entry repeats beyond its length, exactly like the other
/// arms' scripted model).
pub fn plan_candidate(task: &CodingTaskSpec, attempt: u32) -> IntendedFiles {
    let plan = fixture_plan(task);
    let index = (attempt as usize).min(plan.len() - 1);
    fixture_candidate(task, plan[index]).files
}

/// The frozen candidate files of one RECORDED attempt (replay source).
pub fn recorded_candidate(attempt: &FrozenAttempt) -> IntendedFiles {
    attempt
        .files
        .iter()
        .map(|f| (f.path.clone(), f.content.clone()))
        .collect()
}

/// Drive ONE task through the real runtime: fresh workspace copy, one
/// durable session, one turn per attempt (repair prompts carry the previous
/// real `verify.sh` failure excerpt), the objective `sh verify.sh` run by
/// the harness after every turn. Fails loudly on any workspace drift between
/// what the frozen output asked the runtime to write and what the runtime
/// actually left on disk.
pub fn drive_task(
    env: &DaemonArmEnv,
    spec: &CodingTaskSpec,
    workspace_root: &Path,
    candidates: &dyn Fn(u32) -> IntendedFiles,
) -> Result<RuntimeTaskDrive, String> {
    if workspace_root.exists() {
        fs::remove_dir_all(workspace_root).map_err(|e| io_err("clear runtime workspace", e))?;
    }
    copy_coding_repo(spec, workspace_root).map_err(|e| io_err("copy coding repo", e))?;
    let session = env
        .manager
        .create_session(
            env.manager
                .create_workspace(
                    workspace_root
                        .to_str()
                        .ok_or_else(|| "workspace path is not UTF-8".to_string())?,
                )
                .map_err(|e| format!("{}: create workspace: {e}", spec.id))?,
            &format!("bench-daemon-{}", spec.id),
            DAEMON_ARM_PROVIDER,
            DAEMON_ARM_MODEL,
        )
        .map_err(|e| format!("{}: create session: {e}", spec.id))?
        .id();
    let mut current_state = spec.repo_files.clone();
    let mut attempts: Vec<FrozenAttempt> = Vec::new();
    let mut verified = false;
    let mut first_pass = false;
    let mut previous_failure: Option<String> = None;
    for attempt in 0..MAX_CANDIDATE_ATTEMPTS {
        let intended = candidates(attempt);
        let tool_calls: Vec<(String, serde_json::Value)> = intended
            .iter()
            .map(|(path, content)| {
                (
                    "write_file".to_string(),
                    serde_json::json!({ "path": path, "content": content }),
                )
            })
            .collect();
        env.provider
            .begin_turn(&spec.id, attempt, vec![ScriptedTurn::ToolCalls(tool_calls)]);
        let prompt = if attempt == 0 {
            spec.goal.clone()
        } else {
            repair_prompt(&spec.goal, previous_failure.as_deref())
        };
        let outcome = env
            .tokio
            .block_on(env.runtime.run_turn(session, &prompt, &[]))
            .map_err(|e| format!("{}: runtime turn {attempt}: {e}", spec.id))?;
        let stats = env.provider.end_turn();
        let changes = collect_changes(spec, workspace_root, &current_state, &intended)?;
        let mut diff_text = String::new();
        let mut added_total = 0u64;
        let mut removed_total = 0u64;
        let mut files_applied = 0u64;
        for (path, content) in &intended {
            let before = current_state.get(path).cloned().unwrap_or_default();
            if before == *content {
                continue;
            }
            files_applied += 1;
            let (diff, added, removed) = crate::frozen::unified_diff(path, &before, content)
                .map_err(|e| format!("{}: unified diff for {path}: {e}", spec.id))?;
            diff_text.push_str(&diff);
            added_total += added;
            removed_total += removed;
        }
        let verification = run_task_verification(workspace_root, CODING_VERIFY_TIMEOUT);
        let passed = verification.passed();
        let review_verdict = outcome
            .review
            .as_ref()
            .and_then(|r| r.get("verdict"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let volatile: Vec<crate::frozen::FrozenVolatileSection> = stats
            .volatile
            .iter()
            .map(|section| crate::frozen::FrozenVolatileSection {
                name: section.name.to_string(),
                extract: section.extract.clone(),
                rationale: section.rationale.to_string(),
            })
            .collect();
        attempts.push(FrozenAttempt {
            index: attempt,
            provider: DAEMON_ARM_PROVIDER.into(),
            model: DAEMON_ARM_MODEL.into(),
            local: true,
            tokens_in: stats.tokens_in,
            tokens_out: stats.tokens_out,
            spend_micro: 0,
            cost_source: CostSource::Fixture,
            latency_ms: DAEMON_ARM_LATENCY_MS,
            raw_response: None,
            parse_error: None,
            files: changes,
            diff_unified: diff_text,
            added_lines: added_total,
            removed_lines: removed_total,
            verification: FrozenVerification {
                outcome: VerifyOutcome::from(verification.result.clone()),
                detail: None,
                millis: 0,
            },
            prompt_blake3: stats.prompt_blake3,
            prompt_tokens_est: stats.tokens_in,
            volatile,
            runtime: Some(FrozenRuntimeEvidence {
                completion_gate: gate_label(outcome.completion.as_ref()),
                review_verdict,
                derived_checks: outcome.verification.len() as u64,
                provider_calls: stats.provider_calls,
                review_calls: stats.review_calls,
                files_applied,
            }),
        });
        // Every intended file landed exactly (the runtime never drifts from
        // the frozen output; a drift is a hard error, not a measurement).
        for (path, content) in &intended {
            current_state.insert(path.clone(), content.clone());
        }
        if passed {
            verified = true;
            first_pass = attempt == 0;
            break;
        }
        previous_failure = verification.failure_excerpt_for(workspace_root);
    }
    let final_tree_blake3 = corpus_digest(workspace_root)?;
    Ok(RuntimeTaskDrive {
        attempts,
        verified,
        first_pass,
        final_tree_blake3,
    })
}

/// Record every intended file (before/after hashes + content) and verify the
/// runtime left exactly the intended bytes on disk.
fn collect_changes(
    spec: &CodingTaskSpec,
    workspace: &Path,
    current_state: &BTreeMap<String, String>,
    intended: &IntendedFiles,
) -> Result<Vec<FrozenFileChange>, String> {
    let mut changes = Vec::with_capacity(intended.len());
    for (path, content) in intended {
        validate_candidate_path(path)
            .map_err(|e| format!("{}: runtime candidate path: {e}", spec.id))?;
        let on_disk =
            fs::read(workspace.join(path)).map_err(|e| format!("{}: read {path}: {e}", spec.id))?;
        if on_disk != content.as_bytes() {
            return Err(format!(
                "{}: runtime workspace drifted from the frozen output at {path}",
                spec.id
            ));
        }
        let before = current_state.get(path);
        changes.push(FrozenFileChange {
            path: path.clone(),
            before_blake3: before.map(|c| blake3::hash(c.as_bytes()).to_hex().to_string()),
            after_blake3: blake3::hash(content.as_bytes()).to_hex().to_string(),
            content: content.clone(),
        });
    }
    Ok(changes)
}

// ----------------------------------------------------------- arm entry points

/// Run the `full_daemon_runtime` arm over the frozen corpus in frozen order,
/// from the frozen fixture plan (capture mode).
pub fn run_runtime_arm(corpus: &CodingCorpus, scratch: &Path) -> Result<Vec<FrozenRun>, String> {
    let env = DaemonArmEnv::open(&scratch.join("daemon-env"))?;
    let mut runs = Vec::with_capacity(corpus.tasks.len());
    for task in &corpus.tasks {
        if !crate::coding::runtime_available(task.runtime) {
            return Err(format!(
                "{}: runtime {} is not available on this host",
                task.id,
                task.runtime.as_str()
            ));
        }
        let workspace = scratch
            .join("daemon-work")
            .join(format!("{}-{}", task.order, task.id));
        let drive = drive_task(&env, task, &workspace, &|attempt| {
            plan_candidate(task, attempt)
        })?;
        runs.push(build_run(task, drive)?);
    }
    Ok(runs)
}

/// Re-drive ONE recorded `full_daemon_runtime` run from the artifact's own
/// frozen outputs, refuse every disagreement with the record, and return the
/// re-driven workspace root (used to rebuild the durable outcome record).
pub fn redrive_runtime_run(
    env: &DaemonArmEnv,
    capture_id: &str,
    task: &CodingTaskSpec,
    run: &FrozenRun,
    scratch: &Path,
) -> Result<std::path::PathBuf, String> {
    if !run.prompt.arm.is_runtime() {
        return Err(format!("{}: not a runtime run", task.id));
    }
    let workspace = scratch
        .join("replay-daemon-work")
        .join(capture_id)
        .join(format!("{}-{}", task.order, task.id));
    let recorded: Vec<&FrozenAttempt> = run.attempts.iter().collect();
    let drive = drive_task(env, task, &workspace, &|attempt| {
        let index = (attempt as usize).min(recorded.len() - 1);
        recorded_candidate(recorded[index])
    })?;
    if drive.attempts.len() != run.attempts.len() {
        return Err(format!(
            "{}: runtime replay produced {} attempt(s), the artifact recorded {}",
            task.id,
            drive.attempts.len(),
            run.attempts.len()
        ));
    }
    for (replayed, recorded) in drive.attempts.iter().zip(run.attempts.iter()) {
        if replayed.prompt_blake3 != recorded.prompt_blake3 {
            return Err(format!(
                "{}: attempt {}: runtime prompt drifted (recorded {}, replayed {})",
                task.id, recorded.index, recorded.prompt_blake3, replayed.prompt_blake3
            ));
        }
        if replayed.files != recorded.files
            || replayed.diff_unified != recorded.diff_unified
            || replayed.added_lines != recorded.added_lines
            || replayed.removed_lines != recorded.removed_lines
        {
            return Err(format!(
                "{}: attempt {}: frozen file changes drifted",
                task.id, recorded.index
            ));
        }
        if replayed.verification.outcome != recorded.verification.outcome {
            return Err(format!(
                "{}: attempt {}: verification outcome drifted (recorded {:?}, re-ran {:?})",
                task.id,
                recorded.index,
                recorded.verification.outcome,
                replayed.verification.outcome
            ));
        }
        if replayed.runtime != recorded.runtime {
            return Err(format!(
                "{}: attempt {}: runtime gate/review evidence drifted",
                task.id, recorded.index
            ));
        }
        // `volatile` is deliberately NOT compared: it records the
        // scheduling-dependent bytes normalization excluded at CAPTURE time
        // (that is exactly why the rules exist), while the identity check
        // above is the normalized `prompt_blake3`; the rule vocabulary is
        // pinned by artifact validation and the normalization unit test.
        if replayed.tokens_in != recorded.tokens_in
            || replayed.tokens_out != recorded.tokens_out
            || replayed.provider != recorded.provider
            || replayed.model != recorded.model
        {
            return Err(format!(
                "{}: attempt {}: runtime accounting drifted",
                task.id, recorded.index
            ));
        }
    }
    if drive.verified != run.verified || drive.first_pass != run.first_pass {
        return Err(format!(
            "{}: runtime verified/first-pass flags drifted",
            task.id
        ));
    }
    if drive.final_tree_blake3 != run.final_tree_blake3 {
        return Err(format!("{}: runtime final workspace drifted", task.id));
    }
    Ok(workspace)
}

fn build_run(task: &CodingTaskSpec, drive: RuntimeTaskDrive) -> Result<FrozenRun, String> {
    let contract_digest = daemon_contract_digest(task);
    let wall_time_ms = drive.attempts.len() as u64 * DAEMON_ARM_LATENCY_MS;
    Ok(FrozenRun {
        task_id: task.id.clone(),
        order: task.order,
        category: task.category,
        language: task.language,
        runtime: task.runtime,
        prompt: FrozenPrompt {
            arm: Arm::FullDaemonRuntime,
            machinery: true,
            contract_digest: Some(contract_digest),
            intelligence: None,
        },
        attempts: drive.attempts,
        verified: drive.verified,
        first_pass: drive.first_pass,
        final_tree_blake3: drive.final_tree_blake3,
        wall_time_ms,
        completed_at_ms: crate::fixture::FIXTURE_EPOCH_MS
            + i64::from(task.order) * 60_000
            + DAEMON_ARM_TIME_OFFSET_MS,
    })
}

/// The compiled-contract digest of one task (the SAME pure compilation the
/// machinery arm renders and the runtime evaluates at drive start; carried
/// as frozen-corpus evidence of the runtime arm's contract input).
fn daemon_contract_digest(task: &CodingTaskSpec) -> String {
    let files: Vec<RepositoryFile> = task
        .repo_files
        .iter()
        .map(|(path, content)| RepositoryFile::with_content(path.as_str(), content.as_str()))
        .collect();
    let compiled = compile_contract(&ContractCompileRequest {
        goal: task.goal.clone(),
        repository: RepositorySnapshot::from_files(files),
    });
    crate::pipeline::contract_digest_of(&compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_core::id::OpId;

    fn request(system: &str) -> GenericAgentRequest {
        GenericAgentRequest {
            model: DAEMON_ARM_MODEL.into(),
            system: system.into(),
            messages: Vec::new(),
            tools: Vec::new(),
            max_output: Some(1024),
            reasoning: None,
            stream: true,
            meta: faktor_provider::RequestMeta {
                operation_id: OpId::new(7),
                session_id: SessionId::new(3),
                provider: DAEMON_ARM_PROVIDER.into(),
                attempt: 0,
                deadline_ms: 5_000,
                cancellation: faktor_core::CancellationToken::new(),
            },
        }
    }

    fn collect(provider: &FrozenOutputProvider, req: GenericAgentRequest) -> Vec<ProviderChunk> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("tokio runtime");
        runtime.block_on(async {
            use futures::StreamExt;
            provider
                .stream(req)
                .collect::<Vec<Result<ProviderChunk, _>>>()
                .await
                .into_iter()
                .filter_map(Result::ok)
                .collect()
        })
    }

    #[test]
    fn review_requests_get_the_deterministic_clean_verdict() {
        let provider = FrozenOutputProvider::new();
        provider.begin_turn("t", 0, Vec::new());
        let chunks = collect(
            &provider,
            request(&format!("{REVIEW_SYSTEM_MARKER}\nReview the change.")),
        );
        let text: String = chunks
            .iter()
            .filter_map(|c| match c {
                ProviderChunk::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, REVIEW_CLEAN_JSON);
        assert!(chunks.iter().any(|c| matches!(c, ProviderChunk::Done)));
        let stats = provider.end_turn();
        assert_eq!(stats.review_calls, 1, "the call is classified as a review");
        assert_eq!(stats.provider_calls, 1);
    }

    #[test]
    fn scripted_turns_serve_the_frozen_tool_calls_then_a_final_text() {
        let provider = FrozenOutputProvider::new();
        let calls = vec![(
            "write_file".to_string(),
            serde_json::json!({ "path": "a.sh", "content": "echo a\n" }),
        )];
        provider.begin_turn("t", 0, vec![ScriptedTurn::ToolCalls(calls)]);
        let first = collect(&provider, request("you are a coding agent"));
        assert!(first.iter().any(|c| matches!(
            c,
            ProviderChunk::ToolCall { name, complete: true, .. } if name == "write_file"
        )));
        let second = collect(&provider, request("you are a coding agent"));
        assert!(second.iter().any(|c| matches!(
            c,
            ProviderChunk::Text { text } if text == DAEMON_FINAL_TEXT
        )));
        let stats = provider.end_turn();
        assert_eq!(stats.provider_calls, 2);
        assert_eq!(stats.review_calls, 0);
        assert!(!stats.prompt_blake3.is_empty());
        assert!(stats.tokens_in > 0 && stats.tokens_out > 0);
    }

    /// The recorded extracts are EXACTLY the excluded bytes — no more, no
    /// less: `normalized` plus the extracts, re-inserted in order, rebuilds
    /// the input byte-for-byte, and both the extraction and the normalized
    /// output are PINNED. A new (or widened) normalization rule changes one
    /// of the pinned assertions and fails here unless the documented rule
    /// vocabulary, this test and `frozen::validate_run` are deliberately
    /// extended together.
    #[test]
    fn volatile_evidence_records_every_excluded_byte_and_only_those() {
        let text = "instructions\n\
## Repository map\n\
a.sh\n\
lib/.log.sh.faktor-tmp-1-ab\n\
b.sh\n\
\n\
## Retrieved evidence\n\
\n\
### a.sh\n\
token `x` × 1\n\
pid 12345\n\
\n\
## Task progress\n\
STEPS\n\
## Current errors\n\
none\n";
        let out = normalize_volatile_wire_sections(text);
        // (1) Completeness: every byte missing from `normalized` is part of
        //     the next recorded extract, in order; no extract is unused and
        //     `normalized` adds nothing.
        assert_excluded_bytes_are_exactly_the_extracts(text, &out.normalized, &out.volatile);
        // (2) The extraction is pinned exactly (a widened region or a new
        //     rule fails): one temp line and one evidence block, in text
        //     order, byte-for-byte.
        assert_eq!(out.volatile.len(), 2);
        assert_eq!(out.volatile[0].name, VOLATILE_RULE_ATOMIC_TEMP_ENTRY);
        assert_eq!(out.volatile[0].extract, "lib/.log.sh.faktor-tmp-1-ab\n");
        assert_eq!(
            out.volatile[0].rationale,
            VOLATILE_RATIONALE_ATOMIC_TEMP_ENTRY
        );
        assert_eq!(out.volatile[1].name, VOLATILE_RULE_RETRIEVED_EVIDENCE);
        assert_eq!(
            out.volatile[1].extract,
            "## Retrieved evidence\n\n### a.sh\ntoken `x` × 1\npid 12345\n\n"
        );
        assert_eq!(
            out.volatile[1].rationale,
            VOLATILE_RATIONALE_RETRIEVED_EVIDENCE
        );
        // (3) The normalized text is pinned byte-for-byte: only the documented
        //     regions are gone; `## Task progress`, `## Current errors` and
        //     the timestamp-free decoys all stay under the hash.
        assert_eq!(
            out.normalized.as_ref(),
            "instructions\n## Repository map\na.sh\nb.sh\n\n## Task progress\nSTEPS\n## Current errors\nnone\n"
        );
        // (4) The document rules are the ONLY documented vocabulary.
        for section in &out.volatile {
            assert!(
                documented_volatile_rule(section.name, section.rationale),
                "recorded rule {} must be documented",
                section.name
            );
        }

        // An evidence block at EOF: exactly its own bytes, nothing else.
        let at_eof = "head\n## Retrieved evidence\nbody\n";
        let out = normalize_volatile_wire_sections(at_eof);
        assert_excluded_bytes_are_exactly_the_extracts(at_eof, &out.normalized, &out.volatile);
        assert_eq!(out.normalized.as_ref(), "head\n");
        assert_eq!(out.volatile.len(), 1);
        assert_eq!(out.volatile[0].extract, "## Retrieved evidence\nbody\n");

        // A tmp-only text: the whole line only; a line that merely mentions
        // the marker word in DATA is the documented line rule (dropped), so
        // decoys that must survive contain no marker at all.
        let with_temp = "line one\nlib/.log.sh.faktor-tmp-1-ab\ntail\n";
        let out = normalize_volatile_wire_sections(with_temp);
        assert_excluded_bytes_are_exactly_the_extracts(with_temp, &out.normalized, &out.volatile);
        assert_eq!(out.normalized.as_ref(), "line one\ntail\n");
        assert_eq!(out.volatile.len(), 1);

        // Untouched text: borrowed, zero exclusions (no normalization fired).
        let untouched = "plain system text\n";
        let out = normalize_volatile_wire_sections(untouched);
        assert!(matches!(out.normalized, std::borrow::Cow::Borrowed(_)));
        assert!(out.volatile.is_empty());
        assert_eq!(out.normalized.as_ref(), untouched);
    }

    /// Decomposition proof for "the recorded extracts are the ONLY excluded
    /// bytes": search the lockstep positions with memoized backtracking
    /// (kept-byte steps and whole-extract steps) for a path that consumes
    /// the original and the normalized text together with EVERY recorded
    /// extract, in order. A byte dropped without a recorded extract, or a
    /// recorded extract that does not match the original bytes at its
    /// position, has no path and fails here.
    fn assert_excluded_bytes_are_exactly_the_extracts(
        original: &str,
        normalized: &str,
        volatile: &[VolatileSection],
    ) {
        let orig = original.as_bytes();
        let norm = normalized.as_bytes();
        let extracts: Vec<&[u8]> = volatile
            .iter()
            .map(|section| {
                assert!(
                    !section.extract.is_empty(),
                    "recorded extracts are never empty"
                );
                section.extract.as_bytes()
            })
            .collect();
        let mut seen = std::collections::HashSet::new();
        let mut stack = vec![(0usize, 0usize, 0usize)];
        while let Some(state) = stack.pop() {
            let (i, j, k) = state;
            if i == orig.len() && j == norm.len() && k == extracts.len() {
                return;
            }
            if !seen.insert(state) {
                continue;
            }
            if i < orig.len() && j < norm.len() && orig[i] == norm[j] {
                stack.push((i + 1, j + 1, k));
            }
            if k < extracts.len() {
                let extract = extracts[k];
                if orig[i..].starts_with(extract) {
                    stack.push((i + extract.len(), j, k + 1));
                }
            }
        }
        panic!(
            "the recorded extracts are not exactly the excluded bytes \
             (original {} bytes, normalized {} bytes, {} recorded sections)",
            orig.len(),
            norm.len(),
            extracts.len()
        );
    }

    /// The wire hash must be a property of the model-visible CONTENT, never
    /// of the process-wide `serde_json/preserve_order` feature unification
    /// (workspace builds enable it through other members, flipping `Value`
    /// object key order). `canonical_json_bytes` sorts every object key, so
    /// logically identical payloads built with different literal orders hash
    /// identically — and a regression to the raw `to_vec` serialization
    /// fails this witness in ANY build configuration.
    #[test]
    fn wire_json_is_canonical_across_map_orderings() {
        let a = serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"},
            },
            "required": ["path", "content"],
        });
        let reordered = serde_json::json!({
            "required": ["path", "content"],
            "properties": {
                "content": {"type": "string"},
                "path": {"type": "string"},
            },
            "type": "object",
        });
        assert_eq!(canonical_json_bytes(&a), canonical_json_bytes(&reordered));
        assert_eq!(
            String::from_utf8(canonical_json_bytes(&a)).unwrap(),
            r#"{"properties":{"content":{"type":"string"},"path":{"type":"string"}},"required":["path","content"],"type":"object"}"#
        );
        // Arrays keep their element order; only object keys canonicalize.
        let arr = serde_json::json!([{"b": 1, "a": 2}, {"d": 3, "c": 4}]);
        assert_eq!(
            String::from_utf8(canonical_json_bytes(&arr)).unwrap(),
            r#"[{"a":2,"b":1},{"c":4,"d":3}]"#
        );
    }
}
