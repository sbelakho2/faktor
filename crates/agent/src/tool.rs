//! Tool definitions. Tools are stateless callables with declared metadata;
//! they never touch session persistence (Commandment 1) and every invocation
//! carries its workspace identity explicitly.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use faktor_context::compiler::ProvenanceSource;
use faktor_core::capability::Capability;
use faktor_core::error::Error;
use faktor_core::hash::FileHash;
use faktor_core::id::{OpId, SessionId, TaskId, WorkspaceId, WorktreeId};
use faktor_core::model::{ModelCapabilities, RouterPhase};
use faktor_core::resource::ResourceClass;
use faktor_core::WorkspaceIdentity;
use faktor_provider::ToolSpec;

use crate::activation::{ToolActivationSet, ToolExposure};
use crate::tool_json::{parse_tool_calls, ToolCallMode};

/// How the runtime should recover this tool after a crash (spec §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryHint {
    /// Deterministic workspace write (write_file): the tool computes a
    /// [`FilePostcondition`] at execution end (workspace id, RELATIVE path,
    /// BLAKE3 of the ACTUAL bytes as written) and the runtime records it on
    /// the run row. Crash recovery verifies the CURRENT file bytes through
    /// the workspace file service against the recorded postcondition. The
    /// runtime NEVER infers file hashes from JSON-encoded args.
    WorkspaceWrite,
    /// Reads / idempotent commands: safe to re-run after a crash. The
    /// runtime stores a [`ReplayDescriptor`] on the run row; recovery
    /// re-executes the stored invocation ONCE as a new physical attempt of
    /// the same logical operation.
    Idempotent,
    /// Commands with unknown external effects: mark `effect_status =
    /// unknown` and force verification; never blindly re-run.
    UnknownEffect,
}

/// Durable workspace-write postcondition (spec §7, v7): the expected state a
/// deterministic write tool declared for the file it wrote — `relative_path`
/// is resolved against the workspace root (canonical, traversal/symlink-safe)
/// and `expected_hash` is BLAKE3 of the bytes AS WRITTEN (never of JSON
/// encoding of the content argument).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FilePostcondition {
    pub workspace_id: WorkspaceId,
    pub worktree_id: WorktreeId,
    pub relative_path: String,
    pub expected_hash: FileHash,
}

/// Durable replay descriptor (spec §7, v7): the stored invocation crash
/// recovery may re-execute ONCE for an interrupted idempotent tool run — a
/// NEW PHYSICAL attempt of the SAME logical operation (same turn op id, same
/// tool-run row; the attempt counter lives on the row).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReplayDescriptor {
    pub tool_name: String,
    /// Canonical validated invocation arguments (a JSON object re-validated
    /// against the tool's input schema where feasible before any replay).
    pub validated_args: serde_json::Value,
    pub workspace_id: WorkspaceId,
    pub worktree_id: WorktreeId,
    pub task_id: TaskId,
    /// The logical turn this run belonged to: the replay completes the SAME
    /// turn — its identity is never re-synthesized.
    pub original_turn_op_id: OpId,
    /// The permission capability the original run was granted (replay is a
    /// recovery continuation of an already-approved call).
    pub capability: Capability,
    /// Declared recovery kind ("idempotent" today).
    pub recovery_kind: String,
}

#[derive(Clone)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub resource_class: ResourceClass,
    pub capability: Option<Capability>,
    pub recovery_hint: RecoveryHint,
    /// Which arg names of this tool are filesystem paths. The scheduler's
    /// ownership sets (reads/writes) are derived from these; the direction
    /// (read vs write) follows the tool's resource class (DiskWrite ⇒
    /// writes, anything else ⇒ reads). Empty for tools that touch no paths.
    pub path_args: Vec<String>,
    pub execute: ToolFn,
}

/// Filesystem ownership of one tool invocation, derived from the tool's
/// declared path arguments (spec §22: edits with overlapping writes
/// serialize; reads never block each other).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ownership {
    pub reads: Vec<String>,
    pub writes: Vec<String>,
}

impl Default for ToolOutcome {
    fn default() -> Self {
        Self {
            text: String::new(),
            exit_code: None,
            artifact: None,
            slice_hint: None,
            effect_status: faktor_core::op::EffectStatus::Applied,
            postcondition: None,
            provenance: ProvenanceSource::Tool,
        }
    }
}

impl Tool {
    pub fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
        }
    }

    /// Derive the reads/writes this invocation touches from the declared
    /// path args. Non-string arg values are skipped (never trusted).
    pub fn ownership(&self, args: &serde_json::Value) -> Ownership {
        let is_write = self.resource_class == ResourceClass::DiskWrite;
        let mut reads = Vec::new();
        let mut writes = Vec::new();
        for arg_name in &self.path_args {
            let Some(path) = args.get(arg_name).and_then(|v| v.as_str()) else {
                continue;
            };
            if is_write {
                writes.push(path.to_string());
            } else {
                reads.push(path.to_string());
            }
        }
        Ownership { reads, writes }
    }
}

/// Execution context: explicit identity + cancellation + artifact writer.
/// The real filesystem stack is injected per invocation by the runtime:
/// `None` means "no workspace wired" and tools must error honestly.
#[derive(Clone)]
pub struct ToolRunCtx {
    pub session_id: SessionId,
    pub op_id: OpId,
    pub identity: WorkspaceIdentity,
    pub cancellation: faktor_core::cancellation::CancellationToken,
    pub artifacts: Arc<crate::ToolArtifactSink>,
    pub tool_call_mode: ToolCallMode,
    /// Resolved workspace handle for the session (canonical root, watcher).
    pub workspace: Option<Arc<faktor_fs::WorkspaceHandle>>,
    /// Transactional edit engine for optimistic writes.
    pub edit: Option<Arc<faktor_edit::EditEngine>>,
    /// CAS-backed checkpoint store (before/after hashes for undo).
    pub snapshots: Option<Arc<faktor_snapshot::CheckpointStore>>,
    /// Capability permission engine rooted at the session workspace.
    pub sandbox: Option<Arc<faktor_sandbox::PermissionEngine>>,
    /// Process supervisor for run_command (no orphans, bounded output).
    pub supervisor: Option<Arc<faktor_terminal::ProcessSupervisor>>,
    /// Remaining op deadline in ms (0 → tool default).
    pub deadline_ms: u64,
    /// The runtime resolved this tool's permission hop to Allow before the
    /// call (the Ask decision lives in the daemon's permission requester).
    /// Tools re-check hard DENY rules; an Ask-policy verdict may proceed
    /// ONLY when this is set — a direct, permission-less invocation (tests,
    /// mis-wired registries) still refuses on Ask.
    pub permission_granted: bool,
}

/// Result of one tool invocation. `text` is bounded by the tool itself
/// (ring-buffer excerpts); big outputs go to `artifacts`.
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub text: String,
    pub exit_code: Option<i32>,
    pub artifact: Option<String>,
    pub slice_hint: Option<String>,
    pub effect_status: faktor_core::op::EffectStatus,
    /// Workspace-write tools report their expected file state here; the
    /// runtime records it on the tool-run row so crash recovery verifies the
    /// file against the REAL bytes as written (never JSON-encoded args).
    pub postcondition: Option<FilePostcondition>,
    /// Where this output came from. Ordinary tool output is
    /// [`ProvenanceSource::Tool`]; coordination-board output is
    /// [`ProvenanceSource::AgentCoordination`] — a SIBLING AGENT'S post is
    /// peer DATA and may never acquire instruction authority (a malicious
    /// board post cannot launder itself into policy by being read+archived).
    pub provenance: ProvenanceSource,
}

pub type ToolFn = Arc<
    dyn Fn(
            ToolRunCtx,
            serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = Result<ToolOutcome, Error>> + Send>>
        + Send
        + Sync,
>;

// --------------------------------------------------------------------------
// Tool bundles per phase (audit 47): tool definitions are prompt tokens, so
// the model must never see one ever-growing bundle. A phase selects the
// relevant subset of the registry; `Implement` keeps the whole registered set
// (the historical wire shape), every other phase is a strict subset.
// --------------------------------------------------------------------------

/// Stable identity of a tool bundle (`phase:<slug>` today): addressable in
/// telemetry and tests without depending on the bundle's contents.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct ToolBundleId(String);

impl ToolBundleId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ToolBundleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ToolBundleId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

/// Maximum model-visible specs a semantic-provider capability may map to:
/// Faktor's semantic retrieval runs automatically (the model does not need a
/// definition per semantic operation), so the surface collapses to at most
/// one or two specs.
pub const SEMANTIC_BUNDLE_MAX_SPECS: usize = 2;

/// The tool definitions that ride ONE model request for ONE router phase.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolBundle {
    pub id: ToolBundleId,
    pub phase: RouterPhase,
    pub tools: Vec<ToolSpec>,
}

/// The name of the semantic escape-hatch spec Faktor exposes when the
/// semantic-provider capability is present (see [`ToolBundle::semantic_specs`]).
pub const SEMANTIC_QUERY_TOOL: &str = "semantic_query";

impl ToolBundle {
    /// Select the phase bundle over `registry` for `capabilities` with an
    /// EMPTY lazy activation set: exactly the historical behavior (a lazy
    /// tool contributes nothing until it is activated).
    ///
    /// Selection is by registry metadata (`ResourceClass`) and declared
    /// capabilities only — never provider names. A semantic-provider
    /// capability (`ModelCapabilities::embeddings`) maps to AT MOST
    /// [`SEMANTIC_BUNDLE_MAX_SPECS`] specs and is offered only to the phases
    /// whose work is retrieval; the raw per-operation tools are never dumped.
    pub fn for_phase(
        phase: RouterPhase,
        registry: &ToolRegistry,
        capabilities: &ModelCapabilities,
    ) -> Self {
        Self::for_phase_with_activation(phase, registry, capabilities, &ToolActivationSet::new())
    }

    /// [`ToolBundle::for_phase`] under an explicit activation set: a tool
    /// registered [`ToolExposure::Lazy`] is included only when its name is
    /// active AND its phase mask contains `phase`. `Normal` tools are
    /// unaffected — the historical class policy still decides them.
    pub fn for_phase_with_activation(
        phase: RouterPhase,
        registry: &ToolRegistry,
        capabilities: &ModelCapabilities,
        activation: &ToolActivationSet,
    ) -> Self {
        let mut tools: Vec<ToolSpec> = registry
            .iter()
            .filter(|tool| registry.exposure_allows(phase, tool, activation))
            .map(Tool::spec)
            .collect();
        if phase_exposes_semantic(phase) {
            for spec in Self::semantic_specs(capabilities) {
                if !tools.iter().any(|t| t.name == spec.name) {
                    tools.push(spec);
                }
            }
        }
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools.dedup_by(|a, b| a.name == b.name);
        Self {
            id: ToolBundleId::new(format!("phase:{}", phase_slug(phase))),
            phase,
            tools,
        }
    }

    /// The model-visible semantic surface: one [`SEMANTIC_QUERY_TOOL`] spec
    /// when the capabilities carry the semantic (embedding) provider, empty
    /// otherwise. The count is capped at [`SEMANTIC_BUNDLE_MAX_SPECS`] by
    /// construction — Faktor uses the semantic provider automatically instead
    /// of dumping dozens of model-visible tool definitions.
    pub fn semantic_specs(capabilities: &ModelCapabilities) -> Vec<ToolSpec> {
        if !capabilities.embeddings {
            return Vec::new();
        }
        let specs = vec![ToolSpec {
            name: SEMANTIC_QUERY_TOOL.into(),
            description: "Semantic (embedding-backed) workspace query served by Faktor's \
                          semantic provider: returns ranked paths and snippets. Faktor runs \
                          semantic retrieval itself; this is the model's explicit escape hatch."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer" }
                },
                "required": ["query"]
            }),
        }];
        debug_assert!(specs.len() <= SEMANTIC_BUNDLE_MAX_SPECS);
        specs
    }

    /// BLAKE3 over the canonical serialization of this bundle's tool NAMES
    /// and input SCHEMAS: tools sorted by name, every JSON object's keys
    /// sorted recursively, each field length-framed. Descriptions are prose
    /// (never schema), so they do not move the hash. The hash is
    /// byte-identical for the same phase + capabilities and changes exactly
    /// when the schema changes.
    pub fn bundle_hash(&self) -> String {
        let mut tools: Vec<&ToolSpec> = self.tools.iter().collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"faktor.tool-bundle.v1\0");
        for tool in tools {
            let name = tool.name.as_bytes();
            hasher.update(&(name.len() as u64).to_le_bytes());
            hasher.update(name);
            let schema = serde_json::to_vec(&canonical_json(&tool.input_schema))
                .expect("canonical JSON serializes");
            hasher.update(&(schema.len() as u64).to_le_bytes());
            hasher.update(&schema);
        }
        hasher.finalize().to_hex().to_string()
    }

    /// The bundle's tool names (already name-sorted by construction).
    pub fn tool_names(&self) -> Vec<&str> {
        self.tools.iter().map(|t| t.name.as_str()).collect()
    }
}

/// Recursively re-key every JSON object in sorted key order so serialization
/// is canonical regardless of map backend / insertion order.
fn canonical_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            let mut out = serde_json::Map::with_capacity(map.len());
            for key in keys {
                out.insert(key.clone(), canonical_json(&map[key]));
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonical_json).collect())
        }
        other => other.clone(),
    }
}

/// Registry-metadata phase policy: which tools a phase may show the model.
/// `Implement` keeps the full registered set (including MCP tools) — today's
/// wire behavior, now the explicit `implement` bundle.
fn phase_allows(phase: RouterPhase, tool: &Tool) -> bool {
    // Coordination tools are cross-cutting: a child may need to read or
    // post in any tool-bearing phase (a question during exploration is the
    // point). They are still hidden from the model-only phases.
    if tool.name == BOARD_POST_TOOL || tool.name == BOARD_READ_TOOL {
        return !matches!(
            phase,
            RouterPhase::Compact | RouterPhase::Title | RouterPhase::Embed
        );
    }
    match phase {
        RouterPhase::Implement => true,
        // Exploration / planning / summary: read-only surface. Summarize must
        // never see a filesystem write tool.
        RouterPhase::Plan
        | RouterPhase::Explore
        | RouterPhase::Retrieve
        | RouterPhase::Summarize => tool.resource_class == ResourceClass::DiskRead,
        // Review and test analysis inspect evidence and re-run checks; no
        // mutation tools.
        RouterPhase::Review | RouterPhase::TestAnalysis => matches!(
            tool.resource_class,
            ResourceClass::DiskRead | ResourceClass::Terminal
        ),
        // Debugging may inspect, edit and re-run checks.
        RouterPhase::Debug => matches!(
            tool.resource_class,
            ResourceClass::DiskRead | ResourceClass::DiskWrite | ResourceClass::Terminal
        ),
        // Model-only phases: no tools at all.
        RouterPhase::Compact | RouterPhase::Title | RouterPhase::Embed => false,
    }
}

/// Phases whose work is retrieval and therefore carry the compact semantic
/// surface when the semantic-provider capability is present.
fn phase_exposes_semantic(phase: RouterPhase) -> bool {
    matches!(
        phase,
        RouterPhase::Plan | RouterPhase::Explore | RouterPhase::Retrieve
    )
}

/// Whether a phase may carry tools at all. `Compact`/`Title`/`Embed` are
/// model-only: no tool (lazy or not) is ever exposed there.
fn phase_is_tool_bearing(phase: RouterPhase) -> bool {
    !matches!(
        phase,
        RouterPhase::Compact | RouterPhase::Title | RouterPhase::Embed
    )
}

fn phase_slug(phase: RouterPhase) -> &'static str {
    match phase {
        RouterPhase::Plan => "plan",
        RouterPhase::Explore => "explore",
        RouterPhase::Retrieve => "retrieve",
        RouterPhase::Implement => "implement",
        RouterPhase::Review => "review",
        RouterPhase::TestAnalysis => "test_analysis",
        RouterPhase::Debug => "debug",
        RouterPhase::Compact => "compact",
        RouterPhase::Summarize => "summarize",
        RouterPhase::Title => "title",
        RouterPhase::Embed => "embed",
    }
}

/// Tool registry: the agent asks the registry, tools are wired by the CLI.
///
/// `tools` carries every registered tool (lazy or not) so execution and
/// ownership lookups are unchanged; `exposure` carries the per-name
/// [`ToolExposure`] that decides MODEL VISIBILITY. A name registered before
/// exposure tracking (or absent from the map) is `Normal` by construction —
/// [`ToolRegistry::register`] always inserts [`ToolExposure::Normal`].
#[derive(Default)]
pub struct ToolRegistry {
    tools: std::collections::HashMap<String, Arc<Tool>>,
    exposure: std::collections::HashMap<String, ToolExposure>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a `Normal` tool: exposed exactly as before (the per-phase
    /// class policy decides). Unchanged meaning.
    pub fn register(&mut self, tool: Tool) {
        self.register_with_exposure(tool, ToolExposure::Normal);
    }

    /// Register a tool with an explicit exposure. A [`ToolExposure::Lazy`]
    /// tool is invisible to the model (zero schema bytes/tokens) until its
    /// activation set contains its name; [`ToolExposure::Normal`] here is an
    /// alias of [`ToolRegistry::register`].
    pub fn register_lazy(&mut self, tool: Tool, exposure: ToolExposure) {
        self.register_with_exposure(tool, exposure);
    }

    fn register_with_exposure(&mut self, tool: Tool, exposure: ToolExposure) {
        let name = tool.name.clone();
        self.tools.insert(name.clone(), Arc::new(tool));
        self.exposure.insert(name, exposure);
    }

    pub fn get(&self, name: &str) -> Option<Arc<Tool>> {
        self.tools.get(name).cloned()
    }

    /// The declared exposure of `name` (`None` for an unregistered name).
    pub fn exposure(&self, name: &str) -> Option<&ToolExposure> {
        self.exposure.get(name)
    }

    /// Whether `name` is a registered lazy tool (invisible until activated).
    pub fn is_lazy(&self, name: &str) -> bool {
        self.exposure.get(name).is_some_and(ToolExposure::is_lazy)
    }

    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.tools.keys().cloned().collect();
        v.sort();
        v
    }

    /// Read-only iteration over the registered tools (map order; callers
    /// that need determinism sort).
    pub fn iter(&self) -> impl Iterator<Item = &Tool> {
        self.tools.values().map(|t| t.as_ref())
    }

    /// The tool bundle `phase` exposes over this registry with an EMPTY lazy
    /// activation set: byte-identical to the historical behavior (audit 47).
    pub fn bundle_for_phase(
        &self,
        phase: RouterPhase,
        capabilities: &ModelCapabilities,
    ) -> ToolBundle {
        self.bundle_for_phase_with_activation(phase, capabilities, &ToolActivationSet::new())
    }

    /// The tool bundle `phase` exposes under `activation`. Only `Lazy`
    /// exposure consults the activation set; `Normal` tools follow the
    /// historical class policy byte-for-byte.
    pub fn bundle_for_phase_with_activation(
        &self,
        phase: RouterPhase,
        capabilities: &ModelCapabilities,
        activation: &ToolActivationSet,
    ) -> ToolBundle {
        ToolBundle::for_phase_with_activation(phase, self, capabilities, activation)
    }

    /// Registry-metadata visibility of one tool. `Normal` tools use the
    /// historical class policy; a `Lazy` tool is visible only while active
    /// AND inside its phase mask. The mask IS the lazy tool's phase policy
    /// for tool-bearing phases — a `Network`-class acquisition tool must be
    /// able to ride `Plan`/`Explore`/`Retrieve`, which the class policy
    /// alone refuses — while the model-only phases (`Compact`, `Title`,
    /// `Embed`) stay a hard floor no lazy mask can breach.
    fn exposure_allows(
        &self,
        phase: RouterPhase,
        tool: &Tool,
        activation: &ToolActivationSet,
    ) -> bool {
        match self.exposure.get(&tool.name) {
            Some(ToolExposure::Lazy { phases, .. }) => {
                phase_is_tool_bearing(phase)
                    && activation.is_active(&tool.name)
                    && phases.contains(phase)
            }
            _ => phase_allows(phase, tool),
        }
    }

    /// Fold one user text into `activation`: every registered lazy tool's
    /// deterministic triggers (whole-token signals, first-party product
    /// URLs, the explicit `/source on|off` flag) are evaluated over the text.
    /// No classifier, embedding or model call. Activation is sticky; only an
    /// explicit `off` flag deactivates. Names are visited in sorted order so
    /// the result is independent of registration order.
    pub fn observe_activation(&self, text: &str, activation: &mut ToolActivationSet) {
        let mut names: Vec<&String> = self.exposure.keys().collect();
        names.sort();
        for name in names {
            let Some(ToolExposure::Lazy { triggers, .. }) = self.exposure.get(name) else {
                continue;
            };
            activation.observe(name, triggers, text);
        }
    }

    /// [`ToolRegistry::observe_activation`] on a copy of `prior`.
    pub fn activation_for_text(&self, prior: &ToolActivationSet, text: &str) -> ToolActivationSet {
        let mut next = prior.clone();
        self.observe_activation(text, &mut next);
        next
    }

    /// Every registered spec, lazy or not (the historical meaning: this is
    /// NOT a model-visible bundle; use `bundle_for_phase*` for that).
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut v: Vec<ToolSpec> = self.tools.values().map(|t| t.spec()).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// Convenience for tools that need to parse their own args with bounds.
pub fn bound_args(args: &serde_json::Value, max_bytes: usize) -> Result<serde_json::Value, Error> {
    let s = serde_json::to_vec(args)
        .map_err(|e| Error::malformed(format!("args not serializable: {e}")))?;
    if s.len() > max_bytes {
        return Err(Error::oversized(format!(
            "tool args {} bytes exceed bound {max_bytes}",
            s.len()
        )));
    }
    Ok(args.clone())
}

// --------------------------------------------------------------------------
// Coordination-board tools (additive): the model-visible surface of the
// durable parent/descendant agent board. The tools themselves stay
// persistence-free (Commandment 1): the daemon injects a
// [`BoardToolGateway`] over its durable board authority, the tool only
// shapes bounded arguments and a bounded JSON result. Every bound and
// scope check (family scoping, terminal-child refusal, reset CAS) lives in
// the session layer behind the gateway.
// --------------------------------------------------------------------------

/// Model-visible name of the board post tool.
pub const BOARD_POST_TOOL: &str = "board_post";
/// Model-visible name of the board read tool.
pub const BOARD_READ_TOOL: &str = "board_read";
/// Hard bound on one board tool's result text handed to the model.
pub const BOARD_TOOL_TEXT_MAX: usize = 64 * 1024;
/// Max `limit` one `board_read` may request (mirrors the session board page).
pub const BOARD_TOOL_MAX_LIMIT: usize = 100;
/// Max refs one `board_post` may carry (mirrors the session board bound).
pub const BOARD_TOOL_MAX_REFS: usize = 32;
/// The provenance class of BOTH board tools' output. Board content is
/// agent-coordination DATA: archived through the evidence authority it stays
/// non-authoritative by construction, so a sibling's "ignore user policy"
/// post can never instruct the reading agent.
pub const BOARD_TOOL_PROVENANCE: ProvenanceSource = ProvenanceSource::AgentCoordination;

/// The durable coordination-board authority injected into the board tools.
/// Implementations resolve the calling session and delegate to the session
/// board API; errors stay typed ([`Error`]).
pub trait BoardToolGateway: Send + Sync {
    /// Post to the calling session's run-family board; returns the created
    /// post as JSON.
    fn board_post(
        &self,
        session: SessionId,
        subject: &str,
        body: &str,
        refs: &[String],
    ) -> Result<serde_json::Value, Error>;

    /// Newest-first read of the calling session's board; returns the page
    /// as JSON.
    fn board_read(
        &self,
        session: SessionId,
        since_revision: Option<u64>,
        limit: usize,
        exclude_self: bool,
    ) -> Result<serde_json::Value, Error>;
}

/// Bounded JSON text of one board tool result (never an unbounded dump).
fn board_tool_text(value: &serde_json::Value) -> Result<String, Error> {
    let text = serde_json::to_string(value)
        .map_err(|e| Error::internal(format!("board result serialization: {e}")))?;
    if text.len() <= BOARD_TOOL_TEXT_MAX {
        return Ok(text);
    }
    let mut cut = BOARD_TOOL_TEXT_MAX;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    Ok(format!(
        "{}\n[board output truncated at {BOARD_TOOL_TEXT_MAX} bytes]",
        &text[..cut]
    ))
}

/// The `board_post(subject, body, refs)` tool: bounded schema, typed
/// argument errors, and the durable posting authority injected through the
/// gateway (a terminal child is refused there, before any write).
pub fn board_post_tool(gateway: Arc<dyn BoardToolGateway>) -> Tool {
    Tool {
        name: BOARD_POST_TOOL.into(),
        description: "Post a bounded note to the run-family agent coordination board \
                      (subject, body, optional evidence/path/artifact refs). The board is \
                      append-only and scoped to this run family."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "subject": { "type": "string", "minLength": 1, "maxLength": 512 },
                "body": { "type": "string", "minLength": 1, "maxLength": 16384 },
                "refs": {
                    "type": "array",
                    "maxItems": BOARD_TOOL_MAX_REFS,
                    "items": { "type": "string", "minLength": 1, "maxLength": 1024 }
                }
            },
            "required": ["subject", "body"],
            "additionalProperties": false
        }),
        // Coordination is in-process durable bookkeeping, not disk work;
        // Cpu keeps it out of the disk-ownership sets.
        resource_class: ResourceClass::Cpu,
        capability: None,
        // A post is a durable append: blind re-execution after a crash
        // would duplicate it, so recovery never silently replays it.
        recovery_hint: RecoveryHint::UnknownEffect,
        path_args: vec![],
        execute: Arc::new(move |ctx, args| {
            let gateway = gateway.clone();
            Box::pin(async move {
                let subject = args
                    .get("subject")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| Error::malformed("board_post requires a string subject"))?;
                let body = args
                    .get("body")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| Error::malformed("board_post requires a string body"))?;
                let refs = match args.get("refs") {
                    None | Some(serde_json::Value::Null) => Vec::new(),
                    Some(serde_json::Value::Array(items)) => {
                        if items.len() > BOARD_TOOL_MAX_REFS {
                            return Err(Error::oversized(format!(
                                "board_post carries more than {BOARD_TOOL_MAX_REFS} refs"
                            )));
                        }
                        let mut refs = Vec::with_capacity(items.len());
                        for item in items {
                            let Some(s) = item.as_str() else {
                                return Err(Error::malformed("board_post refs must be strings"));
                            };
                            refs.push(s.to_string());
                        }
                        refs
                    }
                    Some(_) => {
                        return Err(Error::malformed("board_post refs must be an array"));
                    }
                };
                let value = gateway.board_post(ctx.session_id, subject, body, &refs)?;
                Ok(ToolOutcome {
                    text: board_tool_text(&value)?,
                    exit_code: Some(0),
                    provenance: BOARD_TOOL_PROVENANCE,
                    ..Default::default()
                })
            })
        }),
    }
}

/// The `board_read(since_revision, limit, exclude_self)` tool: one bounded
/// newest-first page of the calling session's run-family board.
pub fn board_read_tool(gateway: Arc<dyn BoardToolGateway>) -> Tool {
    Tool {
        name: BOARD_READ_TOOL.into(),
        description: "Read the newest-first coordination board of this run family \
                      (bounded page). Posts hidden by a board reset are never returned."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "since_revision": { "type": "integer", "minimum": 0 },
                "limit": { "type": "integer", "minimum": 1, "maximum": BOARD_TOOL_MAX_LIMIT },
                "exclude_self": { "type": "boolean" }
            },
            "additionalProperties": false
        }),
        resource_class: ResourceClass::DiskRead,
        capability: None,
        recovery_hint: RecoveryHint::Idempotent,
        path_args: vec![],
        execute: Arc::new(move |ctx, args| {
            let gateway = gateway.clone();
            Box::pin(async move {
                let since_revision = match args.get("since_revision") {
                    None | Some(serde_json::Value::Null) => None,
                    Some(v) => Some(v.as_u64().ok_or_else(|| {
                        Error::malformed("board_read since_revision must be a non-negative integer")
                    })?),
                };
                let limit = match args.get("limit") {
                    None | Some(serde_json::Value::Null) => 20,
                    Some(v) => {
                        let raw = v.as_u64().ok_or_else(|| {
                            Error::malformed("board_read limit must be an integer")
                        })?;
                        if raw == 0 || raw > BOARD_TOOL_MAX_LIMIT as u64 {
                            return Err(Error::oversized(format!(
                                "board_read limit must be 1..={BOARD_TOOL_MAX_LIMIT}"
                            )));
                        }
                        raw as usize
                    }
                };
                let exclude_self = match args.get("exclude_self") {
                    None | Some(serde_json::Value::Null) => false,
                    Some(v) => v.as_bool().ok_or_else(|| {
                        Error::malformed("board_read exclude_self must be a boolean")
                    })?,
                };
                let value =
                    gateway.board_read(ctx.session_id, since_revision, limit, exclude_self)?;
                Ok(ToolOutcome {
                    text: board_tool_text(&value)?,
                    exit_code: Some(0),
                    // A sibling's post is coordination DATA, never policy:
                    // the archived evidence must carry AgentCoordination.
                    provenance: BOARD_TOOL_PROVENANCE,
                    ..Default::default()
                })
            })
        }),
    }
}

/// Parse tool-call text extracted from a stream (StructuredFallback path).
pub fn parse_tool_text(text: &str, mode: ToolCallMode) -> Vec<crate::tool_json::ParsedToolCall> {
    parse_tool_calls(text, mode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_core::error::ErrorKind;

    #[test]
    fn registry_roundtrip_and_unknown() {
        let mut r = ToolRegistry::new();
        r.register(Tool {
            name: "read_file".into(),
            description: "d".into(),
            input_schema: serde_json::json!({"type": "object"}),
            resource_class: ResourceClass::DiskRead,
            capability: None,
            recovery_hint: RecoveryHint::Idempotent,
            path_args: vec!["path".into()],
            execute: Arc::new(|_ctx, args| {
                Box::pin(async move {
                    Ok(ToolOutcome {
                        text: format!("{args:?}"),
                        ..Default::default()
                    })
                })
            }),
        });
        assert_eq!(r.names(), vec!["read_file"]);
        assert!(r.get("read_file").is_some());
        assert!(r.get("write_file").is_none());
        assert_eq!(r.specs().len(), 1);
    }

    #[tokio::test]
    async fn tool_gets_explicit_identity_and_cancellation() {
        let mut r = ToolRegistry::new();
        let seen = Arc::new(std::sync::Mutex::new(None));
        let seen2 = seen.clone();
        r.register(Tool {
            name: "identity_probe".into(),
            description: "d".into(),
            input_schema: serde_json::json!({}),
            resource_class: ResourceClass::Cpu,
            capability: None,
            recovery_hint: RecoveryHint::Idempotent,
            path_args: vec![],
            execute: Arc::new(move |ctx, args| {
                let seen = seen2.clone();
                Box::pin(async move {
                    *seen.lock().unwrap() = Some((ctx.session_id, ctx.identity.workspace_id, args));
                    Ok(ToolOutcome::default())
                })
            }),
        });
        let token = faktor_core::cancellation::CancellationToken::new();
        let ctx = ToolRunCtx {
            session_id: SessionId::new(5),
            op_id: OpId::new(6),
            identity: WorkspaceIdentity::new(
                faktor_core::WorkspaceId::new(1),
                faktor_core::WorktreeId::new(2),
                faktor_core::TaskId::new(3),
            ),
            cancellation: token.clone(),
            artifacts: Arc::new(crate::ToolArtifactSink::Null),
            tool_call_mode: ToolCallMode::Native,
            workspace: None,
            edit: None,
            snapshots: None,
            sandbox: None,
            supervisor: None,
            deadline_ms: 0,
            permission_granted: false,
        };
        let tool = r.get("identity_probe").unwrap();
        (tool.execute)(ctx, serde_json::json!({"path": "/x"}))
            .await
            .unwrap();
        let got = seen.lock().unwrap().take().unwrap();
        assert_eq!(got.0, SessionId::new(5));
        assert_eq!(got.1, faktor_core::WorkspaceId::new(1));
        assert_eq!(got.2["path"], "/x");
        // Cancellation token arrives intact and functional.
        assert!(!token.is_cancelled());
    }

    #[test]
    fn bound_args_rejects_oversized() {
        let big = serde_json::json!({"payload": "x".repeat(10_000)});
        assert!(bound_args(&big, 100).is_err());
        assert!(bound_args(&big, 100_000).is_ok());
    }

    #[test]
    fn recovery_hints_are_explicit() {
        assert!(matches!(
            RecoveryHint::WorkspaceWrite,
            RecoveryHint::WorkspaceWrite
        ));
        assert!(matches!(RecoveryHint::Idempotent, RecoveryHint::Idempotent));
        assert!(matches!(
            RecoveryHint::UnknownEffect,
            RecoveryHint::UnknownEffect
        ));
    }

    #[test]
    fn ownership_derived_from_path_args_and_resource_class() {
        let read = Tool {
            name: "read_file".into(),
            description: "d".into(),
            input_schema: serde_json::json!({}),
            resource_class: ResourceClass::DiskRead,
            capability: None,
            recovery_hint: RecoveryHint::Idempotent,
            path_args: vec!["path".into()],
            execute: Arc::new(|_ctx, _args| Box::pin(async move { Ok(ToolOutcome::default()) })),
        };
        let write = Tool {
            path_args: vec!["path".into()],
            resource_class: ResourceClass::DiskWrite,
            ..read.clone()
        };
        let no_paths = Tool {
            path_args: vec![],
            ..read.clone()
        };

        let o = read.ownership(&serde_json::json!({"path": "src/a.rs"}));
        assert_eq!(o.reads, vec!["src/a.rs"]);
        assert!(o.writes.is_empty());

        let o = write.ownership(&serde_json::json!({"path": "src/a.rs"}));
        assert!(o.reads.is_empty());
        assert_eq!(o.writes, vec!["src/a.rs"]);

        assert_eq!(
            read.ownership(&serde_json::json!({"path": 42})),
            Ownership::default(),
            "non-string path args are never trusted"
        );
        assert_eq!(
            no_paths.ownership(&serde_json::json!({"path": "x"})),
            Ownership::default(),
            "tools with no declared path args own nothing"
        );
    }

    // ---- audit 47: tool bundles per phase -------------------------------

    fn bundle_tool(name: &str, class: ResourceClass, schema: serde_json::Value) -> Tool {
        Tool {
            name: name.into(),
            description: format!("{name} test description"),
            input_schema: schema,
            resource_class: class,
            capability: None,
            recovery_hint: RecoveryHint::Idempotent,
            path_args: vec![],
            execute: Arc::new(|_ctx, _args| Box::pin(async move { Ok(ToolOutcome::default()) })),
        }
    }

    /// Register the standard builtin-shaped tools in the given order (order
    /// deliberately parameterized: the bundle must not depend on it).
    fn phase_registry(order: &[&str]) -> ToolRegistry {
        let mut r = ToolRegistry::new();
        for name in order {
            let (class, schema) = match *name {
                "read_file" => (
                    ResourceClass::DiskRead,
                    serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
                ),
                "search" => (
                    ResourceClass::DiskRead,
                    serde_json::json!({"type": "object", "properties": {"pattern": {"type": "string"}}}),
                ),
                "write_file" => (
                    ResourceClass::DiskWrite,
                    serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}}}),
                ),
                "edit_file" => (
                    ResourceClass::DiskWrite,
                    serde_json::json!({"type": "object", "properties": {"edits": {"type": "array"}}}),
                ),
                "run_command" => (
                    ResourceClass::Terminal,
                    serde_json::json!({"type": "object", "properties": {"command": {"type": "string"}}}),
                ),
                other => (
                    ResourceClass::Mcp,
                    serde_json::json!({"type": "object", "properties": {"name": {"const": other}}}),
                ),
            };
            r.register(bundle_tool(name, class, schema));
        }
        r
    }

    fn caps_semantic() -> ModelCapabilities {
        ModelCapabilities {
            embeddings: true,
            ..Default::default()
        }
    }

    #[test]
    fn summarize_bundle_has_no_write_tool() {
        let registry = phase_registry(&[
            "read_file",
            "search",
            "write_file",
            "edit_file",
            "run_command",
        ]);
        let bundle = registry.bundle_for_phase(RouterPhase::Summarize, &caps_semantic());
        let names = bundle.tool_names();
        assert!(!names.contains(&"write_file"));
        assert!(!names.contains(&"edit_file"));
        for spec in &bundle.tools {
            let tool = registry
                .get(&spec.name)
                .expect("phase bundle specs come from the registry");
            assert_ne!(
                tool.resource_class,
                ResourceClass::DiskWrite,
                "summarize must never expose a filesystem write tool"
            );
        }
        assert_eq!(bundle.id.as_str(), "phase:summarize");
    }

    #[test]
    fn implement_bundle_contains_required_edit_tool() {
        let registry = phase_registry(&[
            "read_file",
            "write_file",
            "edit_file",
            "search",
            "run_command",
            "mcp_database",
        ]);
        let bundle = registry.bundle_for_phase(RouterPhase::Implement, &caps_semantic());
        let names = bundle.tool_names();
        assert!(names.contains(&"edit_file"), "implement must expose edit");
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"search"));
        assert!(
            names.contains(&"run_command"),
            "implement must expose checks"
        );
        // Full registered set preserved (historical wire shape), name-sorted.
        assert_eq!(bundle.tools.len(), registry.len());
        assert_eq!(
            names,
            vec![
                "edit_file",
                "mcp_database",
                "read_file",
                "run_command",
                "search",
                "write_file"
            ]
        );
        assert_eq!(bundle.id.as_str(), "phase:implement");
    }

    #[test]
    fn same_phase_same_capabilities_produces_byte_identical_bundle() {
        let a = phase_registry(&["read_file", "search", "write_file"])
            .bundle_for_phase(RouterPhase::Explore, &caps_semantic());
        let b = phase_registry(&["write_file", "search", "read_file"])
            .bundle_for_phase(RouterPhase::Explore, &caps_semantic());
        assert_eq!(a.bundle_hash(), b.bundle_hash());
        assert_eq!(
            serde_json::to_vec(&a).unwrap(),
            serde_json::to_vec(&b).unwrap(),
            "same phase + capabilities is byte-identical regardless of registration order"
        );
    }

    #[test]
    fn tool_bundle_hash_changes_only_when_schema_changes() {
        let registry = phase_registry(&["read_file", "search"]);
        let base = registry.bundle_for_phase(RouterPhase::Explore, &caps_semantic());

        let mut prose_changed = base.clone();
        prose_changed.tools[0].description = "entirely different prose".into();
        assert_eq!(
            base.bundle_hash(),
            prose_changed.bundle_hash(),
            "descriptions are prose, not schema"
        );

        let mut key_order = base.clone();
        key_order.tools[0].input_schema = serde_json::json!({
            "properties": {"path": {"type": "string"}},
            "type": "object"
        });
        assert_eq!(
            base.bundle_hash(),
            key_order.bundle_hash(),
            "canonical serialization sorts object keys"
        );

        let mut schema_changed = base.clone();
        schema_changed.tools[0].input_schema = serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "start_line": {"type": "integer"}
            }
        });
        assert_ne!(base.bundle_hash(), schema_changed.bundle_hash());

        let mut tool_added = base.clone();
        tool_added.tools.push(
            bundle_tool(
                "run_command",
                ResourceClass::Terminal,
                serde_json::json!({"type": "object"}),
            )
            .spec(),
        );
        assert_ne!(base.bundle_hash(), tool_added.bundle_hash());

        let mut tool_removed = base.clone();
        tool_removed.tools.pop();
        assert_ne!(base.bundle_hash(), tool_removed.bundle_hash());
    }

    #[test]
    fn explore_and_review_bundles_are_relevant_subsets() {
        let registry = phase_registry(&[
            "read_file",
            "search",
            "write_file",
            "edit_file",
            "run_command",
            "mcp_database",
        ]);

        let explore = registry.bundle_for_phase(RouterPhase::Explore, &caps_semantic());
        let names = explore.tool_names();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"search"));
        assert!(names.contains(&"semantic_query"));
        assert!(!names.contains(&"write_file"));
        assert!(!names.contains(&"edit_file"));
        assert!(!names.contains(&"run_command"));
        assert!(!names.contains(&"mcp_database"));

        let review = registry.bundle_for_phase(RouterPhase::Review, &caps_semantic());
        let names = review.tool_names();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"search"));
        assert!(names.contains(&"run_command"), "review re-runs checks");
        assert!(!names.contains(&"write_file"));
        assert!(!names.contains(&"edit_file"));

        let debug = registry.bundle_for_phase(RouterPhase::Debug, &caps_semantic());
        let names = debug.tool_names();
        assert!(names.contains(&"edit_file"));
        assert!(names.contains(&"run_command"));
    }

    #[test]
    fn semantic_capability_maps_to_at_most_two_specs() {
        assert!(ToolBundle::semantic_specs(&ModelCapabilities::default()).is_empty());
        let specs = ToolBundle::semantic_specs(&caps_semantic());
        assert!(!specs.is_empty());
        assert!(specs.len() <= SEMANTIC_BUNDLE_MAX_SPECS);

        // A registry carrying many raw semantic MCP tools must still surface
        // only the compact semantic escape hatch on retrieval phases.
        let registry = phase_registry(&[
            "read_file",
            "search",
            "mcp_semantic_a",
            "mcp_semantic_b",
            "mcp_semantic_c",
        ]);
        let bundle = registry.bundle_for_phase(RouterPhase::Retrieve, &caps_semantic());
        let semantic_surface = bundle
            .tool_names()
            .into_iter()
            .filter(|n| n.starts_with("semantic") || n.starts_with("mcp_semantic"))
            .count();
        assert!(semantic_surface <= SEMANTIC_BUNDLE_MAX_SPECS);
        assert!(bundle.tool_names().contains(&"semantic_query"));
    }

    #[test]
    fn model_only_phases_expose_no_tools() {
        let registry = phase_registry(&["read_file", "write_file", "run_command", "mcp_database"]);
        for phase in [RouterPhase::Compact, RouterPhase::Title, RouterPhase::Embed] {
            let bundle = registry.bundle_for_phase(phase, &caps_semantic());
            assert!(bundle.tools.is_empty(), "{phase:?} exposes no tools");
        }
    }

    // ---- coordination-board tools --------------------------------------

    type PostCall = (SessionId, String, String, Vec<String>);
    type ReadCall = (SessionId, Option<u64>, usize, bool);

    #[derive(Default)]
    struct FakeBoard {
        posts: std::sync::Mutex<Vec<PostCall>>,
        reads: std::sync::Mutex<Vec<ReadCall>>,
        fail: std::sync::Mutex<Option<Error>>,
    }

    impl FakeBoard {
        fn post_calls(&self) -> Vec<PostCall> {
            self.posts.lock().unwrap().clone()
        }

        fn read_calls(&self) -> Vec<ReadCall> {
            self.reads.lock().unwrap().clone()
        }
    }

    impl BoardToolGateway for FakeBoard {
        fn board_post(
            &self,
            session: SessionId,
            subject: &str,
            body: &str,
            refs: &[String],
        ) -> Result<serde_json::Value, Error> {
            if let Some(e) = self.fail.lock().unwrap().take() {
                return Err(e);
            }
            self.posts.lock().unwrap().push((
                session,
                subject.to_string(),
                body.to_string(),
                refs.to_vec(),
            ));
            Ok(serde_json::json!({"id": 1, "revision": 1}))
        }

        fn board_read(
            &self,
            session: SessionId,
            since_revision: Option<u64>,
            limit: usize,
            exclude_self: bool,
        ) -> Result<serde_json::Value, Error> {
            if let Some(e) = self.fail.lock().unwrap().take() {
                return Err(e);
            }
            self.reads
                .lock()
                .unwrap()
                .push((session, since_revision, limit, exclude_self));
            Ok(serde_json::json!({"posts": [], "has_more": false}))
        }
    }

    fn board_ctx() -> ToolRunCtx {
        ToolRunCtx {
            session_id: SessionId::new(7),
            op_id: OpId::new(8),
            identity: WorkspaceIdentity::new(
                faktor_core::WorkspaceId::new(1),
                faktor_core::WorktreeId::new(2),
                faktor_core::TaskId::new(3),
            ),
            cancellation: faktor_core::cancellation::CancellationToken::new(),
            artifacts: Arc::new(crate::ToolArtifactSink::Null),
            tool_call_mode: ToolCallMode::Native,
            workspace: None,
            edit: None,
            snapshots: None,
            sandbox: None,
            supervisor: None,
            deadline_ms: 0,
            permission_granted: false,
        }
    }

    #[tokio::test]
    async fn board_tools_forward_bounded_args_and_typed_errors() {
        let fake = Arc::new(FakeBoard::default());
        let post = board_post_tool(fake.clone());
        let read = board_read_tool(fake.clone());

        let out = (post.execute)(
            board_ctx(),
            serde_json::json!({"subject": "s", "body": "b", "refs": ["a.txt", "ev/1"]}),
        )
        .await
        .unwrap();
        assert!(out.text.contains("\"revision\":1"));
        let calls = fake.post_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, SessionId::new(7));
        assert_eq!(calls[0].3, vec!["a.txt".to_string(), "ev/1".to_string()]);

        let out = (read.execute)(
            board_ctx(),
            serde_json::json!({"since_revision": 4, "limit": 5, "exclude_self": true}),
        )
        .await
        .unwrap();
        assert!(out.text.contains("has_more"));
        let calls = fake.read_calls();
        assert_eq!(calls[0], (SessionId::new(7), Some(4), 5, true));

        // Typed argument refusals: no gateway call, no write.
        for (tool, args) in [
            (post.clone(), serde_json::json!({"body": "b"})),
            (
                post.clone(),
                serde_json::json!({"subject": "s", "body": "b", "refs": "not-array"}),
            ),
            (
                post.clone(),
                serde_json::json!({"subject": "s", "body": "b", "refs": [1]}),
            ),
            (read.clone(), serde_json::json!({"since_revision": "4"})),
            (read.clone(), serde_json::json!({"limit": 0})),
            (read.clone(), serde_json::json!({"limit": 101})),
            (read.clone(), serde_json::json!({"exclude_self": "yes"})),
        ] {
            let err = (tool.execute)(board_ctx(), args).await.unwrap_err();
            assert!(
                matches!(err.kind, ErrorKind::Malformed | ErrorKind::Oversized),
                "{err:?}"
            );
        }
        assert!(fake.post_calls().len() == 1, "no rejected write ran");
        assert!(fake.read_calls().len() == 1);

        // A gateway failure stays typed (terminal-child refusal).
        *fake.fail.lock().unwrap() = Some(Error::permission("terminal child"));
        let err = (post.execute)(
            board_ctx(),
            serde_json::json!({"subject": "s", "body": "b"}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Permission);
    }

    #[test]
    fn board_tool_schemas_are_bounded_and_cross_phase_visible() {
        let registry_tools = [
            board_post_tool(Arc::new(FakeBoard::default())),
            board_read_tool(Arc::new(FakeBoard::default())),
        ];
        // Schema bounds: the model-visible definitions carry the hard caps.
        let post_schema = &registry_tools[0].input_schema;
        assert_eq!(post_schema["properties"]["subject"]["maxLength"], 512);
        assert_eq!(post_schema["properties"]["body"]["maxLength"], 16384);
        assert_eq!(
            post_schema["properties"]["refs"]["items"]["maxLength"],
            1024
        );
        let read_schema = &registry_tools[1].input_schema;
        assert_eq!(read_schema["properties"]["limit"]["maximum"], 100);

        let mut registry = phase_registry(&["read_file", "write_file", "run_command"]);
        for tool in registry_tools {
            registry.register(tool);
        }
        for phase in [
            RouterPhase::Implement,
            RouterPhase::Explore,
            RouterPhase::Plan,
            RouterPhase::Summarize,
            RouterPhase::Review,
            RouterPhase::Debug,
        ] {
            let bundle = registry.bundle_for_phase(phase, &ModelCapabilities::default());
            let names = bundle.tool_names();
            assert!(names.contains(&"board_post"), "{phase:?}");
            assert!(names.contains(&"board_read"), "{phase:?}");
        }
        for phase in [RouterPhase::Compact, RouterPhase::Title, RouterPhase::Embed] {
            let bundle = registry.bundle_for_phase(phase, &ModelCapabilities::default());
            let names = bundle.tool_names();
            assert!(!names.contains(&"board_post"), "{phase:?}");
            assert!(!names.contains(&"board_read"), "{phase:?}");
        }
    }

    // ---- lazy tool exposure (docs/acquire.md §2/§4) ----------------------

    use crate::activation::{
        acquire_source_phases, acquire_source_triggers, evaluate_triggers, PhaseMask,
        TriggerVerdict,
    };

    /// The future `source_market` shape: a `Network`-class tool with the
    /// normative §4 mask and trigger vocabulary (the acquisition runtime
    /// itself is a later build step; the class and schema are real).
    fn source_market_tool() -> Tool {
        bundle_tool(
            "source_market",
            ResourceClass::Network,
            serde_json::json!({
                "type": "object",
                "properties": {"op": {"enum": ["search", "product", "quote", "bom", "job"]}},
                "required": ["op"],
                "additionalProperties": false
            }),
        )
    }

    fn lazy_registry() -> ToolRegistry {
        let mut registry = phase_registry(&[
            "read_file",
            "search",
            "write_file",
            "edit_file",
            "run_command",
            "mcp_database",
        ]);
        registry.register_lazy(
            source_market_tool(),
            ToolExposure::lazy(acquire_source_phases(), acquire_source_triggers()),
        );
        registry
    }

    /// Every phase whose bundle exposes `source_market` under `activation`.
    fn exposing_phases(
        registry: &ToolRegistry,
        activation: &ToolActivationSet,
    ) -> Vec<RouterPhase> {
        RouterPhase::ALL
            .into_iter()
            .filter(|phase| {
                registry
                    .bundle_for_phase_with_activation(
                        *phase,
                        &ModelCapabilities::default(),
                        activation,
                    )
                    .tool_names()
                    .contains(&"source_market")
            })
            .collect()
    }

    fn active_source_market() -> ToolActivationSet {
        let mut activation = ToolActivationSet::new();
        activation.activate("source_market");
        activation
    }

    #[test]
    fn register_still_means_normal_and_unregistered_names_have_no_exposure() {
        let mut registry = ToolRegistry::new();
        registry.register(bundle_tool(
            "read_file",
            ResourceClass::DiskRead,
            serde_json::json!({"type": "object"}),
        ));
        assert_eq!(registry.exposure("read_file"), Some(&ToolExposure::Normal));
        assert!(!registry.is_lazy("read_file"));
        assert_eq!(registry.exposure("never_registered"), None);
        assert!(!registry.is_lazy("never_registered"));
        assert!(registry
            .bundle_for_phase(RouterPhase::Implement, &ModelCapabilities::default())
            .tool_names()
            .contains(&"read_file"));
    }

    #[test]
    fn legacy_bundle_is_byte_identical_to_the_empty_activation_bundle() {
        let registry = lazy_registry();
        let empty = ToolActivationSet::new();
        for caps in [ModelCapabilities::default(), caps_semantic()] {
            for phase in RouterPhase::ALL {
                let legacy = registry.bundle_for_phase(phase, &caps);
                let explicit = registry.bundle_for_phase_with_activation(phase, &caps, &empty);
                assert_eq!(
                    serde_json::to_vec(&legacy).unwrap(),
                    serde_json::to_vec(&explicit).unwrap(),
                    "{phase:?}: bundle_for_phase must be byte-identical to an empty activation"
                );
                assert_eq!(legacy.bundle_hash(), explicit.bundle_hash());
                assert!(
                    !legacy.tool_names().contains(&"source_market"),
                    "{phase:?}: an inactive lazy tool must be invisible"
                );
            }
        }
    }

    #[test]
    fn lazy_tool_is_invisible_until_activated_and_mask_gated() {
        let registry = lazy_registry();
        let empty = ToolActivationSet::new();
        assert!(exposing_phases(&registry, &empty).is_empty());

        let active = active_source_market();
        assert_eq!(
            exposing_phases(&registry, &active),
            vec![
                RouterPhase::Plan,
                RouterPhase::Explore,
                RouterPhase::Retrieve,
                RouterPhase::Implement,
                RouterPhase::Debug,
            ],
            "spec §4: Plan/Explore/Retrieve/Implement yes, Debug optional (chosen yes), \
             Review/TestAnalysis/Summarize/Compact/Title/Embed no"
        );
        for phase in [
            RouterPhase::Review,
            RouterPhase::TestAnalysis,
            RouterPhase::Summarize,
        ] {
            assert!(
                !exposing_phases(&registry, &active).contains(&phase),
                "{phase:?} must not expose an acquisition tool"
            );
        }
    }

    #[test]
    fn a_lazy_mask_can_never_breach_the_model_only_phases() {
        let mut registry = ToolRegistry::new();
        registry.register_lazy(
            source_market_tool(),
            ToolExposure::lazy(PhaseMask::ALL, acquire_source_triggers()),
        );
        let active = active_source_market();
        let phases = exposing_phases(&registry, &active);
        assert!(phases.contains(&RouterPhase::Implement));
        for phase in [RouterPhase::Compact, RouterPhase::Title, RouterPhase::Embed] {
            assert!(
                !phases.contains(&phase),
                "the model-only phase {phase:?} is a hard floor"
            );
        }
    }

    #[test]
    fn lazy_mask_is_authoritative_over_the_class_policy_for_tool_phases() {
        // A Network tool is refused by the class policy in Plan/Explore/
        // Retrieve; the lazy mask is what lets the activated tool ride them.
        let registry = lazy_registry();
        let caps = ModelCapabilities::default();
        let active = active_source_market();
        for phase in [
            RouterPhase::Plan,
            RouterPhase::Explore,
            RouterPhase::Retrieve,
        ] {
            assert!(
                registry
                    .bundle_for_phase_with_activation(phase, &caps, &active)
                    .tool_names()
                    .contains(&"source_market"),
                "{phase:?} must expose the activated Network tool"
            );
        }
        // And a lazy DiskWrite tool with an Explore-only mask is exposed in
        // Explore (mask authoritative) while a NORMAL DiskWrite tool is not.
        let mut write_registry = ToolRegistry::new();
        let write = bundle_tool(
            "lazy_writer",
            ResourceClass::DiskWrite,
            serde_json::json!({"type": "object"}),
        );
        write_registry.register_lazy(
            write.clone(),
            ToolExposure::lazy(PhaseMask::of([RouterPhase::Explore]), vec![]),
        );
        write_registry.register(bundle_tool(
            "normal_writer",
            ResourceClass::DiskWrite,
            serde_json::json!({"type": "object"}),
        ));
        let mut activation = ToolActivationSet::new();
        activation.activate("lazy_writer");
        let explore = write_registry.bundle_for_phase_with_activation(
            RouterPhase::Explore,
            &caps,
            &activation,
        );
        let names = explore.tool_names();
        assert!(names.contains(&"lazy_writer"));
        assert!(!names.contains(&"normal_writer"));
        let implement = write_registry.bundle_for_phase_with_activation(
            RouterPhase::Implement,
            &caps,
            &activation,
        );
        assert!(!implement.tool_names().contains(&"lazy_writer"));
    }

    #[test]
    fn activation_folds_ordinary_supplier_and_strong_prompts_deterministically() {
        let registry = lazy_registry();
        let empty = ToolActivationSet::new();

        // An unrelated coding turn: no activation, no lazy schema anywhere.
        let ordinary =
            registry.activation_for_text(&empty, "fix the failing parser test in src/parser.rs");
        assert!(ordinary.is_empty());
        assert!(exposing_phases(&registry, &ordinary).is_empty());

        // A Rust `supplier` trait discussion: still nothing (weak signal).
        let supplier_trait =
            registry.activation_for_text(&empty, "the supplier trait needs a lifetime parameter");
        assert!(
            supplier_trait.is_empty(),
            "a supplier trait discussion must never expose an acquisition tool"
        );

        // Strong signals activate; `/source off` deactivates.
        for text in [
            "check 1688 for this connector",
            "please source this part",
            "quote this BOM",
            "see https://detail.1688.com/offer/1.html",
            "/source on",
        ] {
            let folded = registry.activation_for_text(&empty, text);
            assert!(
                folded.is_active("source_market"),
                "{text:?} must activate source_market"
            );
        }
        let active = registry.activation_for_text(&empty, "Mouser quote");
        let off = registry.activation_for_text(&active, "/source off");
        assert!(!off.is_active("source_market"));

        // Deterministic: identical text + prior, identical set, regardless
        // of registration order.
        let a = registry.activation_for_text(&empty, "buy 5,000 LCSC reels");
        let b = registry.activation_for_text(&empty, "buy 5,000 LCSC reels");
        assert_eq!(a, b);
        let mut reversed = ToolRegistry::new();
        for name in [
            "mcp_database",
            "run_command",
            "edit_file",
            "write_file",
            "search",
            "read_file",
        ] {
            let (class, schema) = (
                if name == "run_command" {
                    ResourceClass::Terminal
                } else if name == "write_file" || name == "edit_file" {
                    ResourceClass::DiskWrite
                } else if name == "read_file" || name == "search" {
                    ResourceClass::DiskRead
                } else {
                    ResourceClass::Mcp
                },
                serde_json::json!({"type": "object"}),
            );
            reversed.register(bundle_tool(name, class, schema));
        }
        reversed.register_lazy(
            source_market_tool(),
            ToolExposure::lazy(acquire_source_phases(), acquire_source_triggers()),
        );
        assert_eq!(
            reversed.activation_for_text(&empty, "buy 5,000 LCSC reels"),
            a,
            "activation is independent of registration order"
        );
    }

    #[test]
    fn re_registration_replaces_the_exposure() {
        let caps = ModelCapabilities::default();
        let mut registry = ToolRegistry::new();
        registry.register_lazy(
            source_market_tool(),
            ToolExposure::lazy(acquire_source_phases(), acquire_source_triggers()),
        );
        assert!(registry.is_lazy("source_market"));
        // A later normal registration of the same name wins.
        registry.register(source_market_tool());
        assert_eq!(
            registry.exposure("source_market"),
            Some(&ToolExposure::Normal)
        );
        assert!(registry
            .bundle_for_phase(RouterPhase::Implement, &caps)
            .tool_names()
            .contains(&"source_market"));

        // ... and the reverse: a lazy re-registration hides it again.
        let mut registry = ToolRegistry::new();
        registry.register(source_market_tool());
        assert!(registry
            .bundle_for_phase(RouterPhase::Implement, &caps)
            .tool_names()
            .contains(&"source_market"));
        registry.register_lazy(
            source_market_tool(),
            ToolExposure::lazy(acquire_source_phases(), acquire_source_triggers()),
        );
        assert!(registry.is_lazy("source_market"));
        assert!(!registry
            .bundle_for_phase(RouterPhase::Implement, &caps)
            .tool_names()
            .contains(&"source_market"));

        // `register_lazy(.., Normal)` is an alias of `register`.
        let mut registry = ToolRegistry::new();
        registry.register_lazy(source_market_tool(), ToolExposure::Normal);
        assert_eq!(
            registry.exposure("source_market"),
            Some(&ToolExposure::Normal)
        );
    }

    #[test]
    fn activation_does_not_touch_the_semantic_surface_or_normal_tools() {
        let registry = lazy_registry();
        let active = active_source_market();
        for phase in [
            RouterPhase::Plan,
            RouterPhase::Explore,
            RouterPhase::Retrieve,
        ] {
            let bundle =
                registry.bundle_for_phase_with_activation(phase, &caps_semantic(), &active);
            let names = bundle.tool_names();
            assert!(names.contains(&"semantic_query"));
            assert!(names.contains(&"source_market"));
            // The class policy still governs the normal tools.
            assert!(!names.contains(&"write_file"));
        }
        let summarize = registry.bundle_for_phase_with_activation(
            RouterPhase::Summarize,
            &caps_semantic(),
            &active,
        );
        assert!(!summarize.tool_names().contains(&"source_market"));
    }

    #[tokio::test]
    async fn lazy_tool_execution_and_ownership_are_unaffected_by_activation() {
        let mut registry = ToolRegistry::new();
        let mut lazy = bundle_tool(
            "lazy_probe",
            ResourceClass::Network,
            serde_json::json!({"type": "object"}),
        );
        lazy.path_args = vec!["path".into()];
        registry.register_lazy(
            lazy,
            ToolExposure::lazy(acquire_source_phases(), acquire_source_triggers()),
        );
        // Lookup and ownership are registry metadata, not model visibility.
        let tool = registry.get("lazy_probe").expect("registered");
        assert_eq!(
            tool.ownership(&serde_json::json!({"path": "src/a.rs"}))
                .reads,
            vec!["src/a.rs"]
        );
        let out = (tool.execute)(board_ctx(), serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(out.text, "");
    }

    #[test]
    fn evaluate_triggers_is_exposed_with_the_normative_vocabulary() {
        let triggers = acquire_source_triggers();
        assert_eq!(
            evaluate_triggers(&triggers, "the supplier trait"),
            TriggerVerdict::None
        );
        assert_eq!(
            evaluate_triggers(&triggers, "find manufacturers"),
            TriggerVerdict::Activate
        );
        assert_eq!(
            evaluate_triggers(&triggers, "/source off"),
            TriggerVerdict::Deactivate
        );
    }
}
