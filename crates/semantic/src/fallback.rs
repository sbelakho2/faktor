//! @authority-collapse: authority-bearing
//!
//! Generic semantic fallback and constrained edit proposals (audit
//! 48-54/58/59).
//!
//! The fallback is the in-crate stub that composes the metadata seams the
//! runtime already has — index, language-server, tree-sitter, git and
//! verification adapters — **without depending on any of those crates**.
//! Each seam is a trait; downstream crates register `Arc<dyn ...>`
//! implementations and the fallback merges them. When a seam is absent the
//! fallback still answers with honest degraded/empty payloads, so ordinary
//! operation never fails solely because no semantic provider is installed.
//!
//! This module also carries the constrained-edit proposal types. A provider
//! may propose a [`ProposedPatch`]; Faktor validates it against workspace,
//! snapshot, path and source-hash rules and then applies it through its own
//! edit/FS machinery. The crate only proposes: there is no apply function
//! here, and no filesystem API is used anywhere in this crate.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use faktor_core::{CancellationToken, Clock, FileHash, SystemClock, WorkspaceId};

use crate::types::{
    AffectedRequest, AffectedSet, BoxFuture, SemanticCall, SemanticCapabilities, SemanticCheck,
    SemanticCompleteness, SemanticContextItem, SemanticContextPack, SemanticContextRequest,
    SemanticDelta, SemanticDeltaChange, SemanticDeltaRequest, SemanticEntityId, SemanticEntityRef,
    SemanticEnvelope, SemanticError, SemanticEvidenceSource, SemanticExpectation,
    SemanticExplainRequest, SemanticExplanation, SemanticFidelity, SemanticFindingProvenance,
    SemanticOp, SemanticOperationQualities, SemanticOperationQuality, SemanticOperationStatus,
    SemanticPayload, SemanticProvider, SemanticProviderDescriptor, SemanticProviderId,
    SemanticResponseCaps, SemanticSnapshot, SemanticSnapshotId, SemanticSnapshotRequest,
    SemanticSubsystem, SemanticUnavailable, SemanticVerification, SemanticVerifyRequest,
    WorkspacePath, SEMANTIC_SCHEMA_VERSION,
};

/// The fallback's provider id. Not a language and not a vendor.
pub const GENERIC_FALLBACK_ID: &str = "generic-fallback";

/// Index metadata seam: entities, revisions, lexical/symbol retrieval,
/// impacted lookups and test discovery. Implementations are synchronous
/// because the durable index is an in-memory, revision-bound snapshot; the
/// fallback checks cancellation before and after each call and bounds every
/// result.
pub trait IndexMetadata: Send + Sync {
    /// Stable source identity for provenance (`index:generation:N`).
    fn identity(&self) -> String {
        "index".to_string()
    }

    /// Current source revision the index was built from, when known.
    fn snapshot_revision(&self, _workspace: WorkspaceId) -> Option<String> {
        None
    }

    fn entity_count(&self, _workspace: WorkspaceId) -> u64 {
        0
    }

    fn context_items(
        &self,
        _workspace: WorkspaceId,
        _query: &str,
        _limit: usize,
    ) -> Result<Vec<SemanticContextItem>, SemanticError> {
        Ok(Vec::new())
    }

    /// Entities impacted by `changed`, using the index's reverse lookups.
    fn impacted_entities(
        &self,
        _workspace: WorkspaceId,
        _changed: &[SemanticEntityRef],
        _max_depth: u32,
    ) -> Result<Vec<SemanticEntityRef>, SemanticError> {
        Ok(Vec::new())
    }

    /// Declarations of `entity` known to the index.
    fn declarations(
        &self,
        _workspace: WorkspaceId,
        _entity: &SemanticEntityRef,
    ) -> Result<Vec<SemanticEntityRef>, SemanticError> {
        Ok(Vec::new())
    }

    /// Test entities discovered in the workspace (parser facts).
    fn test_entities(
        &self,
        _workspace: WorkspaceId,
    ) -> Result<Vec<SemanticEntityRef>, SemanticError> {
        Ok(Vec::new())
    }
}

/// Language-server metadata seam: real compiler semantics when an LSP server
/// is available. Every method is async, cancellable and bounded; an absent
/// server returns [`SemanticError::Unavailable`] instead of empty success.
pub trait LspMetadata: Send + Sync {
    fn identity(&self) -> String {
        "lsp".to_string()
    }

    /// Whether a server can serve this workspace right now (a cheap probe;
    /// never a spawn).
    fn available(&self, _workspace: WorkspaceId) -> bool {
        true
    }

    fn definitions<'a>(
        &'a self,
        workspace: WorkspaceId,
        entity: &'a SemanticEntityRef,
        cancellation: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<SemanticEntityRef>, SemanticError>>;

    fn references<'a>(
        &'a self,
        workspace: WorkspaceId,
        entity: &'a SemanticEntityRef,
        cancellation: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<SemanticEntityRef>, SemanticError>>;

    fn workspace_symbols<'a>(
        &'a self,
        workspace: WorkspaceId,
        query: &'a str,
        limit: usize,
        cancellation: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<SemanticEntityRef>, SemanticError>>;

    /// Incoming/outgoing call hierarchy edges for `entity`, traversed up to
    /// `max_depth` (bounded by the fallback before the call).
    fn call_hierarchy<'a>(
        &'a self,
        workspace: WorkspaceId,
        entity: &'a SemanticEntityRef,
        incoming: bool,
        max_depth: u32,
        cancellation: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<SemanticEntityRef>, SemanticError>>;
}

/// Tree-sitter metadata seam: structural parser facts for one file.
pub trait TreeSitterMetadata: Send + Sync {
    fn identity(&self) -> String {
        "tree-sitter".to_string()
    }

    fn outline(
        &self,
        _workspace: WorkspaceId,
        _path: &WorkspacePath,
    ) -> Result<Vec<SemanticEntityRef>, SemanticError> {
        Ok(Vec::new())
    }

    fn tests_in(
        &self,
        _workspace: WorkspaceId,
        _path: &WorkspacePath,
    ) -> Result<Vec<SemanticEntityRef>, SemanticError> {
        Ok(Vec::new())
    }
}

/// Git metadata seam: revision identity and entities changed between two
/// revisions.
pub trait GitMetadata: Send + Sync {
    fn identity(&self) -> String {
        "git".to_string()
    }

    /// The current `HEAD` oid, or `None` on an unborn branch.
    fn revision<'a>(
        &'a self,
        workspace: WorkspaceId,
    ) -> BoxFuture<'a, Result<Option<String>, SemanticError>>;

    fn changed_entities<'a>(
        &'a self,
        workspace: WorkspaceId,
        from_revision: &'a str,
        to_revision: &'a str,
        cancellation: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<SemanticDeltaChange>, SemanticError>>;
}

/// Verification metadata seam: discovered checks and test entities. The
/// production adapter reports DISCOVERED facts (`passed = false` unless an
/// actual execution verdict exists) — the fallback never fabricates a pass.
pub trait VerificationMetadata: Send + Sync {
    fn identity(&self) -> String {
        "verification".to_string()
    }

    fn checks(&self, _workspace: WorkspaceId) -> Result<Vec<SemanticCheck>, SemanticError> {
        Ok(Vec::new())
    }

    fn test_entities(
        &self,
        _workspace: WorkspaceId,
    ) -> Result<Vec<SemanticEntityRef>, SemanticError> {
        Ok(Vec::new())
    }
}

/// Repository relationship graph seam: the native impacted-set authority.
///
/// The graph adapter owns parsing/LSP/git/index state; this crate only
/// consumes it and preserves provenance. Implementations MUST return bounded,
/// deterministic sets and report truncation explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpactedFinding {
    pub entity: SemanticEntityRef,
    pub is_test: bool,
    /// Human-readable relation chain (e.g. `calls -> symbol:src/a.rs#f@1`).
    pub reason: String,
    /// Stable source identity for this finding (`graph`, `lsp`, `index`).
    pub provenance: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImpactMetadataResult {
    pub findings: Vec<ImpactedFinding>,
    pub truncated: bool,
    pub revision: Option<String>,
    pub degraded: bool,
}

pub trait ImpactMetadata: Send + Sync {
    fn identity(&self) -> String {
        "repository-graph".to_string()
    }

    fn affected(
        &self,
        workspace: WorkspaceId,
        changed: &[SemanticEntityRef],
        max_depth: u32,
        max_results: usize,
    ) -> Result<ImpactMetadataResult, SemanticError>;
}

/// Bounded, revision-keyed cache for deterministic seam results. The key
/// embeds the workspace, revision and operation identity, so a changed
/// revision can never be served from an older entry. Eviction is
/// deterministic (smallest key first) and the entry cap is fixed.
const MAX_REVISION_CACHE_ENTRIES: usize = 256;

#[derive(Default)]
struct RevisionCache {
    entries: BTreeMap<String, CacheValue>,
}

#[derive(Clone)]
enum CacheValue {
    Refs(Vec<SemanticEntityRef>),
    Changes(Vec<SemanticDeltaChange>),
}

impl RevisionCache {
    fn get(&self, key: &str) -> Option<CacheValue> {
        self.entries.get(key).cloned()
    }

    fn put(&mut self, key: String, value: CacheValue) {
        if self.entries.len() >= MAX_REVISION_CACHE_ENTRIES && !self.entries.contains_key(&key) {
            if let Some(oldest) = self.entries.keys().next().cloned() {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(key, value);
    }
}

/// The in-crate fallback provider. All seams are optional; the fallback
/// answers every operation regardless, reporting every absent subsystem as
/// typed unavailable instead of fabricating an empty success.
pub struct GenericSemanticFallback {
    index: Option<Arc<dyn IndexMetadata>>,
    lsp: Option<Arc<dyn LspMetadata>>,
    tree_sitter: Option<Arc<dyn TreeSitterMetadata>>,
    git: Option<Arc<dyn GitMetadata>>,
    verification: Option<Arc<dyn VerificationMetadata>>,
    impact: Option<Arc<dyn ImpactMetadata>>,
    clock: Arc<dyn Clock>,
    cache: Mutex<RevisionCache>,
    /// Wall-clock bound for ONE affected query's LSP enrichment phase.
    /// Exhaustion degrades the payload (typed unavailable) instead of
    /// letting a cold/hostile language server stall the impact answer.
    affected_lsp_budget: Duration,
    /// Maximum changed entities enriched through LSP per affected query.
    affected_lsp_entity_cap: usize,
    /// After a query exhausts the LSP budget, immediate repeat queries are
    /// served from the graph/index without retrying the same starved server
    /// until this instant (a short cooldown, not a permanent cache).
    lsp_backoff_until: Mutex<Option<Instant>>,
}

impl Default for GenericSemanticFallback {
    fn default() -> Self {
        Self {
            index: None,
            lsp: None,
            tree_sitter: None,
            git: None,
            verification: None,
            impact: None,
            clock: Arc::new(SystemClock),
            cache: Mutex::new(RevisionCache::default()),
            affected_lsp_budget: DEFAULT_AFFECTED_LSP_BUDGET,
            affected_lsp_entity_cap: DEFAULT_AFFECTED_LSP_ENTITY_CAP,
            lsp_backoff_until: Mutex::new(None),
        }
    }
}

/// Hard bound on changed entities one affected query accepts.
const MAX_AFFECTED_INPUTS: usize = 512;
/// Hard bound on call-hierarchy depth the fallback will traverse.
const MAX_CALL_HIERARCHY_DEPTH: u32 = 8;
/// Default wall-clock bound for the LSP enrichment phase of one affected
/// query. The impact graph and index answer inside this budget; a language
/// server that cannot is reported as a typed degradation, never awaited
/// indefinitely per query.
pub const DEFAULT_AFFECTED_LSP_BUDGET: Duration = Duration::from_millis(1_500);
/// Default maximum changed entities enriched through LSP per affected query.
/// The graph already covers the workspace transitively; LSP enrichment is a
/// bounded precision add-on, not the primary traversal.
pub const DEFAULT_AFFECTED_LSP_ENTITY_CAP: usize = 8;
/// Cooldown after an affected query exhausts the LSP budget. Immediate
/// repeats answer from the graph/index instead of re-waiting on the same
/// cold server; a revision-driven caller keeps full precision once the
/// cooldown expires.
const AFFECTED_LSP_RETRY_BACKOFF: Duration = Duration::from_secs(2);

impl GenericSemanticFallback {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_index(mut self, index: Arc<dyn IndexMetadata>) -> Self {
        self.index = Some(index);
        self
    }

    pub fn with_lsp(mut self, lsp: Arc<dyn LspMetadata>) -> Self {
        self.lsp = Some(lsp);
        self
    }

    pub fn with_tree_sitter(mut self, tree_sitter: Arc<dyn TreeSitterMetadata>) -> Self {
        self.tree_sitter = Some(tree_sitter);
        self
    }

    pub fn with_git(mut self, git: Arc<dyn GitMetadata>) -> Self {
        self.git = Some(git);
        self
    }

    pub fn with_verification(mut self, verification: Arc<dyn VerificationMetadata>) -> Self {
        self.verification = Some(verification);
        self
    }

    pub fn with_impact(mut self, impact: Arc<dyn ImpactMetadata>) -> Self {
        self.impact = Some(impact);
        self
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Bound the LSP enrichment phase of one affected query. The graph and
    /// index results are always returned; budget exhaustion only marks the
    /// payload degraded with a typed unavailable reason.
    pub fn with_affected_lsp_budget(mut self, budget: Duration) -> Self {
        self.affected_lsp_budget = budget;
        self
    }

    /// Bound how many changed entities one affected query LSP-enriches.
    pub fn with_affected_lsp_entity_cap(mut self, cap: usize) -> Self {
        self.affected_lsp_entity_cap = cap.max(1);
        self
    }

    /// The identity of every wired native subsystem. The production daemon's
    /// fallback is never the empty [`GenericSemanticFallback::default`]
    /// composition; tests assert this report directly.
    pub fn wired_backends(&self) -> crate::types::SemanticBackendReport {
        crate::types::SemanticBackendReport {
            index: self.index.as_ref().map(|seam| seam.identity()),
            lsp: self.lsp.as_ref().map(|seam| seam.identity()),
            tree_sitter: self.tree_sitter.as_ref().map(|seam| seam.identity()),
            git: self.git.as_ref().map(|seam| seam.identity()),
            verification: self.verification.as_ref().map(|seam| seam.identity()),
            impact: self.impact.as_ref().map(|seam| seam.identity()),
        }
    }

    fn provider_id(&self) -> SemanticProviderId {
        SemanticProviderId::parse(GENERIC_FALLBACK_ID)
            .expect("static generic fallback provider id is valid")
    }

    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }

    fn envelope<T: SemanticPayload>(
        &self,
        workspace: WorkspaceId,
        snapshot_id: SemanticSnapshotId,
        payload: T,
    ) -> SemanticEnvelope<T> {
        SemanticEnvelope::new(
            self.provider_id(),
            self.version(),
            workspace,
            snapshot_id,
            self.now_ms(),
            payload,
        )
    }

    fn call_live(&self, call: &SemanticCall) -> Result<(), SemanticError> {
        if call.cancellation.is_cancelled() {
            return Err(SemanticError::Cancelled {
                provider: GENERIC_FALLBACK_ID.to_string(),
            });
        }
        if call.deadline.is_some_and(|d| d.is_expired(self.now_ms())) {
            return Err(SemanticError::DeadlineExceeded {
                provider: GENERIC_FALLBACK_ID.to_string(),
            });
        }
        Ok(())
    }

    fn revision(&self, workspace: WorkspaceId, fallback: &str) -> String {
        self.index
            .as_ref()
            .and_then(|index| index.snapshot_revision(workspace))
            .unwrap_or_else(|| fallback.to_string())
    }

    fn source_id(&self) -> String {
        GENERIC_FALLBACK_ID.to_string()
    }

    fn report_absent(
        &self,
        unavailable: &mut Vec<SemanticUnavailable>,
        degraded: &mut bool,
        subsystem: SemanticSubsystem,
        error: SemanticError,
    ) -> Result<(), SemanticError> {
        if error.caller_terminal() {
            return Err(error);
        }
        *degraded = true;
        let (subsystem, reason) = match error {
            SemanticError::Unavailable { subsystem, reason } => (
                SemanticSubsystem::parse(&subsystem)
                    .unwrap_or_else(|_| subsystem_fallback(&subsystem)),
                reason,
            ),
            other => (subsystem, other.to_string()),
        };
        unavailable.push(SemanticUnavailable::new(subsystem, reason));
        Ok(())
    }

    fn push_provenance(
        provenance: &mut Vec<SemanticFindingProvenance>,
        subject: &str,
        subsystem: SemanticSubsystem,
        source: &str,
        fidelity: SemanticFidelity,
        revision: Option<&str>,
    ) {
        let entry = SemanticFindingProvenance::new(
            subject,
            subsystem,
            source,
            fidelity,
            revision.map(str::to_string),
        );
        if !provenance.iter().any(|existing| {
            existing.subject == entry.subject
                && existing.subsystem == entry.subsystem
                && existing.source == entry.source
        }) {
            provenance.push(entry);
        }
    }

    fn refs_cache_key(
        &self,
        kind: &str,
        workspace: WorkspaceId,
        revision: &str,
        entity: &SemanticEntityRef,
    ) -> String {
        format!(
            "lsp:{kind}:{}:{revision}:{}:{}",
            workspace.raw(),
            entity.path.as_str(),
            entity.entity_id.as_str()
        )
    }

    fn sort_refs(refs: &mut Vec<SemanticEntityRef>, cap: usize) {
        refs.sort();
        // The wire identity is the entity id (validation refuses duplicate
        // ids even across paths). A set filter is required: sorting by path
        // first can place equal ids non-adjacently, so `dedup_by` would miss
        // them. The smallest (path, id) wins deterministically.
        let mut seen = BTreeSet::new();
        refs.retain(|entity| seen.insert(entity.entity_id.clone()));
        refs.truncate(cap);
    }
}

fn subsystem_fallback(name: &str) -> SemanticSubsystem {
    match name {
        "index" => SemanticSubsystem::Index,
        "lsp" => SemanticSubsystem::Lsp,
        "tree_sitter" | "tree-sitter" => SemanticSubsystem::TreeSitter,
        "git" => SemanticSubsystem::Git,
        "verification" => SemanticSubsystem::Verification,
        "graph" | "impact" => SemanticSubsystem::Graph,
        _ => SemanticSubsystem::Embedding,
    }
}

impl SemanticSubsystem {
    fn parse(raw: &str) -> Result<Self, ()> {
        match raw {
            "index" => Ok(Self::Index),
            "lsp" => Ok(Self::Lsp),
            "tree_sitter" | "tree-sitter" => Ok(Self::TreeSitter),
            "git" => Ok(Self::Git),
            "verification" => Ok(Self::Verification),
            "embedding" => Ok(Self::Embedding),
            "graph" | "impact" => Ok(Self::Graph),
            _ => Err(()),
        }
    }
}

fn revision_tree_hash(workspace: WorkspaceId, revision: &str) -> FileHash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&workspace.raw().to_le_bytes());
    hasher.update(&(revision.len() as u64).to_le_bytes());
    hasher.update(revision.as_bytes());
    FileHash::from(*hasher.finalize().as_bytes())
}

impl GenericSemanticFallback {
    /// The honest per-operation quality of THIS composition, derived from
    /// the native seams that are actually wired. A composed fallback with an
    /// LSP, graph or verification seam declares a higher tier for the
    /// operations that seam serves; an absent seam never inherits a stronger
    /// claim. Lookup is O(1).
    pub fn operation_qualities(&self) -> SemanticOperationQualities {
        let mut qualities = SemanticOperationQualities::default();

        // Context: LSP workspace symbols lift the operation from the
        // lexical baseline; the pack can still contain index items, so the
        // aggregate claim stays structural rather than compiler-exact.
        let context = if self.lsp.is_some() {
            SemanticOperationQuality::native(
                SemanticEvidenceSource::Lsp,
                SemanticFidelity::Structural,
                SemanticCompleteness::Partial,
            )
        } else {
            SemanticOperationQuality::native(
                SemanticEvidenceSource::Filesystem,
                SemanticFidelity::Heuristic,
                SemanticCompleteness::Partial,
            )
        };
        qualities.set(SemanticOp::Context, context);

        // Delta is git/filesystem change identity: heuristic and its
        // coverage is genuinely unknown.
        qualities.set(
            SemanticOp::Delta,
            SemanticOperationQuality::native(
                SemanticEvidenceSource::Filesystem,
                SemanticFidelity::Heuristic,
                SemanticCompleteness::Unknown,
            ),
        );

        // Affected: the revision-bound impact graph is the strongest native
        // source, then LSP references, tree-sitter structure, or lexical.
        let affected = if self.impact.is_some() {
            // The impact graph is bound to the material revision it was
            // refreshed from, so its evidence is revision-exact.
            let mut quality = SemanticOperationQuality::native(
                SemanticEvidenceSource::Graph,
                SemanticFidelity::Structural,
                SemanticCompleteness::Partial,
            );
            quality.evidence.revision_binding = crate::intelligence::RevisionBinding::Exact;
            quality
        } else if self.lsp.is_some() {
            SemanticOperationQuality::native(
                SemanticEvidenceSource::Lsp,
                SemanticFidelity::Structural,
                SemanticCompleteness::Partial,
            )
        } else if self.tree_sitter.is_some() {
            SemanticOperationQuality::native(
                SemanticEvidenceSource::TreeSitter,
                SemanticFidelity::Structural,
                SemanticCompleteness::Partial,
            )
        } else {
            SemanticOperationQuality::native(
                SemanticEvidenceSource::Filesystem,
                SemanticFidelity::Heuristic,
                SemanticCompleteness::Unknown,
            )
        };
        qualities.set(SemanticOp::Affected, affected);

        // Verify reports DISCOVERED verification facts, never a proof:
        // authoritative proof obligations therefore stay above it.
        let verify = if self.verification.is_some() {
            SemanticOperationQuality::native(
                SemanticEvidenceSource::RuntimeHistory,
                SemanticFidelity::Structural,
                SemanticCompleteness::Unknown,
            )
        } else {
            SemanticOperationQuality::native(
                SemanticEvidenceSource::Filesystem,
                SemanticFidelity::Heuristic,
                SemanticCompleteness::Unknown,
            )
        };
        qualities.set(SemanticOp::Verify, verify);

        // Explain uses compiler definitions/references when an LSP is
        // wired; tree-sitter outlines are structural, the index lexical.
        let explain = if self.lsp.is_some() {
            SemanticOperationQuality::native(
                SemanticEvidenceSource::Lsp,
                SemanticFidelity::CompilerExact,
                SemanticCompleteness::Partial,
            )
        } else if self.tree_sitter.is_some() {
            SemanticOperationQuality::native(
                SemanticEvidenceSource::TreeSitter,
                SemanticFidelity::Structural,
                SemanticCompleteness::Partial,
            )
        } else {
            SemanticOperationQuality::native(
                SemanticEvidenceSource::Filesystem,
                SemanticFidelity::Heuristic,
                SemanticCompleteness::Partial,
            )
        };
        qualities.set(SemanticOp::Explain, explain);

        // The fallback never claims snapshot, but its honest status still
        // reports the filesystem baseline for the uncovered operation.
        qualities.set(
            SemanticOp::Snapshot,
            SemanticOperationQuality::native(
                SemanticEvidenceSource::Filesystem,
                SemanticFidelity::Heuristic,
                SemanticCompleteness::Unknown,
            ),
        );

        qualities
    }

    pub fn operation_status(&self, op: SemanticOp) -> SemanticOperationStatus {
        let descriptor = self.descriptor();
        SemanticOperationStatus::from_quality(
            self.capabilities().supports(op),
            &descriptor.declared_quality(op),
            self.source_id(),
        )
    }

    pub fn intelligence_capability(
        &self,
        op: SemanticOp,
    ) -> crate::intelligence::IntelligenceCapability {
        let descriptor = self.descriptor();
        crate::intelligence::IntelligenceCapability::from_quality(
            op,
            &descriptor.declared_quality(op),
            self.capabilities().supports(op),
            self.source_id(),
        )
    }

    /// Every semantic operation's capability, in [`SemanticOp::ALL`] order.
    pub fn intelligence_capabilities(&self) -> Vec<crate::intelligence::IntelligenceCapability> {
        SemanticOp::ALL
            .iter()
            .map(|op| self.intelligence_capability(*op))
            .collect()
    }
}

impl SemanticProvider for GenericSemanticFallback {
    fn id(&self) -> SemanticProviderId {
        self.provider_id()
    }

    fn version(&self) -> u32 {
        1
    }

    fn capabilities(&self) -> SemanticCapabilities {
        // TRUTHFUL (audit Tangerine-4): the generic fallback serves bounded
        // native context/delta/affected/explain data, and can serve verify
        // ONLY when a verification seam is wired (discovered checks are
        // reported, never a fabricated pass). It never claims snapshot (no
        // compiler database) or constrained edits.
        let mut caps = SemanticCapabilities::CONTEXT
            .union(SemanticCapabilities::DELTA)
            .union(SemanticCapabilities::AFFECTED)
            .union(SemanticCapabilities::EXPLAIN);
        if self.verification.is_some() {
            caps = caps.union(SemanticCapabilities::VERIFY);
        }
        caps
    }

    fn fidelity(&self) -> SemanticFidelity {
        SemanticFidelity::Heuristic
    }

    fn completeness(&self) -> SemanticCompleteness {
        SemanticCompleteness::Partial
    }

    /// The fallback's honest self-description. The per-operation quality
    /// profile is derived from the wired seams, so LSP-backed operations
    /// report higher than heuristic ones and an absent seam never inherits a
    /// stronger claim. The provider-wide scalar fields remain the honest
    /// aggregate/legacy view; authoritative selection reads `operations`.
    /// Snapshot and constrained edits stay uncovered.
    fn descriptor(&self) -> SemanticProviderDescriptor {
        let mut descriptor = SemanticProviderDescriptor::new(
            self.provider_id(),
            self.version(),
            SEMANTIC_SCHEMA_VERSION,
            self.capabilities(),
        );
        descriptor.fidelity = Some(SemanticFidelity::Heuristic);
        descriptor.completeness = Some(SemanticCompleteness::Partial);
        descriptor.tier = Some(if self.impact.is_some() {
            crate::intelligence::IntelligenceTier::GRAPH.raw()
        } else if self.verification.is_some() {
            crate::intelligence::IntelligenceTier::RUNTIME.raw()
        } else if self.lsp.is_some() {
            crate::intelligence::IntelligenceTier::COMPILER.raw()
        } else if self.index.is_some() || self.tree_sitter.is_some() {
            crate::intelligence::IntelligenceTier::SYNTAX.raw()
        } else {
            crate::intelligence::IntelligenceTier::FILESYSTEM.raw()
        });
        descriptor.evidence = crate::intelligence::native_evidence_dimensions();
        descriptor.operations = self.operation_qualities();
        descriptor
    }

    fn snapshot(
        &self,
        request: SemanticSnapshotRequest,
    ) -> BoxFuture<'_, Result<SemanticEnvelope<SemanticSnapshot>, SemanticError>> {
        Box::pin(async move {
            let workspace = request.workspace;
            let fallback_revision = request.source_revision.clone();
            self.call_live(&request.call)?;
            let mut unavailable = Vec::new();
            let index_revision = match &self.index {
                Some(index) => match index.snapshot_revision(workspace) {
                    Some(revision) => Some(revision),
                    None => {
                        unavailable.push(SemanticUnavailable::new(
                            SemanticSubsystem::Index,
                            "index has no published revision for this workspace",
                        ));
                        None
                    }
                },
                None => {
                    unavailable.push(SemanticUnavailable::new(
                        SemanticSubsystem::Index,
                        "no index adapter wired",
                    ));
                    None
                }
            };
            // Git identity is the SOURCE revision when available; the index
            // label is content-address metadata, not a commit.
            let git_revision = match &self.git {
                Some(git) => match git.revision(workspace).await {
                    Ok(revision) => revision,
                    Err(error) => {
                        if error.caller_terminal() {
                            return Err(error);
                        }
                        if let SemanticError::Unavailable { reason, .. } = error {
                            unavailable
                                .push(SemanticUnavailable::new(SemanticSubsystem::Git, reason));
                        } else {
                            unavailable.push(SemanticUnavailable::new(
                                SemanticSubsystem::Git,
                                error.to_string(),
                            ));
                        }
                        None
                    }
                },
                None => {
                    unavailable.push(SemanticUnavailable::new(
                        SemanticSubsystem::Git,
                        "no git adapter wired",
                    ));
                    None
                }
            };
            if git_revision.is_none() && self.git.is_some() {
                unavailable.push(SemanticUnavailable::new(
                    SemanticSubsystem::Git,
                    "workspace has no commit identity (unborn branch)",
                ));
            }
            let revision = git_revision
                .or_else(|| index_revision.clone())
                .unwrap_or(fallback_revision);
            let entity_count = self
                .index
                .as_ref()
                .map_or(0, |index| index.entity_count(workspace));
            let snapshot_id = SemanticSnapshotId::derive(
                workspace,
                &revision,
                &self.provider_id(),
                self.version(),
                SemanticEnvelope::<SemanticSnapshot>::SCHEMA_VERSION,
            );
            let payload = SemanticSnapshot {
                program_manifest: None,
                source_revision: revision.clone(),
                // When the index published a generation, this is its
                // BLAKE3 content identity (a real content digest over the
                // indexed snapshot). Otherwise it is explicitly derived
                // from the source revision and is NOT a compiler database
                // identity.
                tree_hash: index_revision
                    .as_deref()
                    .and_then(|label| label.rsplit(':').next())
                    .and_then(FileHash::from_hex)
                    .unwrap_or_else(|| revision_tree_hash(workspace, &revision)),
                entity_count,
                created_ms: self.now_ms(),
                unavailable,
            };
            Ok(self.envelope(workspace, snapshot_id, payload))
        })
    }

    fn context(
        &self,
        request: SemanticContextRequest,
    ) -> BoxFuture<'_, Result<SemanticEnvelope<SemanticContextPack>, SemanticError>> {
        Box::pin(async move {
            self.call_live(&request.call)?;
            request.validate()?;
            let workspace = request.workspace;
            let revision = self.revision(workspace, &request.source_revision);
            let max_items = request.max_items.min(crate::types::MAX_ENTITY_REFS);
            let max_bytes = request
                .max_bytes
                .min(crate::types::SEMANTIC_CONTEXT_MAX_BYTES);
            let mut unavailable = Vec::new();
            let mut degraded = false;
            let mut merged: BTreeMap<(String, String), SemanticContextItem> = BTreeMap::new();

            if let Some(index) = &self.index {
                match index.context_items(workspace, &request.query, max_items) {
                    Ok(items) => {
                        for item in items {
                            merge_context_item(&mut merged, item);
                        }
                    }
                    Err(error) => self.report_absent(
                        &mut unavailable,
                        &mut degraded,
                        SemanticSubsystem::Index,
                        error,
                    )?,
                }
            } else {
                degraded = true;
                unavailable.push(SemanticUnavailable::new(
                    SemanticSubsystem::Index,
                    "no index adapter wired",
                ));
            }

            if merged.len() < max_items {
                if let Some(lsp) = &self.lsp {
                    if lsp.available(workspace) {
                        let remaining = max_items.saturating_sub(merged.len());
                        let result = lsp
                            .workspace_symbols(
                                workspace,
                                &request.query,
                                remaining,
                                request.call.cancellation.clone(),
                            )
                            .await;
                        match result {
                            Ok(refs) => {
                                for entity in refs {
                                    let subject = entity.entity_id.as_str().to_string();
                                    let excerpt = format!(
                                        "workspace symbol {} at {}",
                                        entity.entity_id, entity.path
                                    );
                                    let mut provenance = Vec::new();
                                    Self::push_provenance(
                                        &mut provenance,
                                        &subject,
                                        SemanticSubsystem::Lsp,
                                        &lsp.identity(),
                                        SemanticFidelity::CompilerExact,
                                        Some(&revision),
                                    );
                                    merge_context_item(
                                        &mut merged,
                                        SemanticContextItem {
                                            entity,
                                            relevance_bps: 7_000,
                                            excerpt,
                                            provenance,
                                        },
                                    );
                                }
                            }
                            Err(error) => self.report_absent(
                                &mut unavailable,
                                &mut degraded,
                                SemanticSubsystem::Lsp,
                                error,
                            )?,
                        }
                    } else {
                        degraded = true;
                        unavailable.push(SemanticUnavailable::new(
                            SemanticSubsystem::Lsp,
                            "no language server is available for this workspace",
                        ));
                    }
                } else {
                    degraded = true;
                    unavailable.push(SemanticUnavailable::new(
                        SemanticSubsystem::Lsp,
                        "no LSP adapter wired",
                    ));
                }
            }

            let mut items: Vec<SemanticContextItem> = merged.into_values().collect();
            items.sort_by(|a, b| {
                b.relevance_bps
                    .cmp(&a.relevance_bps)
                    .then_with(|| a.entity.path.cmp(&b.entity.path))
                    .then_with(|| a.entity.entity_id.cmp(&b.entity.entity_id))
            });
            let mut kept = Vec::new();
            let mut total_bytes = 0usize;
            let mut truncated = false;
            let mut seen_ids: BTreeSet<SemanticEntityId> = BTreeSet::new();
            for item in items {
                if kept.len() >= max_items {
                    truncated = true;
                    break;
                }
                if !seen_ids.insert(item.entity.entity_id.clone()) {
                    continue;
                }
                let item_bytes = item.excerpt.len();
                if total_bytes.saturating_add(item_bytes) > max_bytes {
                    truncated = true;
                    break;
                }
                total_bytes += item_bytes;
                kept.push(item);
            }
            let payload = SemanticContextPack {
                items: kept,
                truncated,
                total_bytes,
                degraded,
                unavailable,
            };
            Ok(self.envelope(workspace, request.snapshot_id, payload))
        })
    }

    fn delta(
        &self,
        request: SemanticDeltaRequest,
    ) -> BoxFuture<'_, Result<SemanticEnvelope<SemanticDelta>, SemanticError>> {
        Box::pin(async move {
            self.call_live(&request.call)?;
            let workspace = request.workspace;
            let mut unavailable = Vec::new();
            let mut degraded = false;
            let changes = match &self.git {
                Some(git) => {
                    let key = format!(
                        "git:changed:{}:{}:{}",
                        workspace.raw(),
                        request.from_source_revision,
                        request.to_source_revision
                    );
                    match self.cache_get(&key) {
                        Some(CacheValue::Changes(changes)) => changes,
                        _ => match git
                            .changed_entities(
                                workspace,
                                &request.from_source_revision,
                                &request.to_source_revision,
                                request.call.cancellation.clone(),
                            )
                            .await
                        {
                            Ok(mut changes) => {
                                for change in &mut changes {
                                    if change.provenance.is_empty() {
                                        let subject = change.entity.entity_id.as_str().to_string();
                                        Self::push_provenance(
                                            &mut change.provenance,
                                            &subject,
                                            SemanticSubsystem::Git,
                                            &git.identity(),
                                            SemanticFidelity::Heuristic,
                                            Some(&request.to_source_revision),
                                        );
                                    }
                                }
                                self.cache_put(key, CacheValue::Changes(changes.clone()));
                                changes
                            }
                            Err(error) => {
                                self.report_absent(
                                    &mut unavailable,
                                    &mut degraded,
                                    SemanticSubsystem::Git,
                                    error,
                                )?;
                                Vec::new()
                            }
                        },
                    }
                }
                None => {
                    degraded = true;
                    unavailable.push(SemanticUnavailable::new(
                        SemanticSubsystem::Git,
                        "no git adapter wired",
                    ));
                    Vec::new()
                }
            };
            let mut changes = changes;
            changes.sort_by(|a, b| {
                a.entity
                    .path
                    .cmp(&b.entity.path)
                    .then_with(|| a.entity.entity_id.cmp(&b.entity.entity_id))
            });
            let mut seen_changes = BTreeSet::new();
            changes.retain(|change| seen_changes.insert(change.entity.entity_id.clone()));
            changes.truncate(crate::types::MAX_ENTITY_REFS);
            let to_snapshot = SemanticSnapshotId::derive(
                workspace,
                &request.to_source_revision,
                &self.provider_id(),
                self.version(),
                SemanticEnvelope::<SemanticDelta>::SCHEMA_VERSION,
            );
            let payload = SemanticDelta {
                workspace,
                from_snapshot: request.from_snapshot,
                to_snapshot,
                changes,
                degraded,
                unavailable,
            };
            Ok(self.envelope(workspace, to_snapshot, payload))
        })
    }

    fn affected(
        &self,
        request: AffectedRequest,
    ) -> BoxFuture<'_, Result<SemanticEnvelope<AffectedSet>, SemanticError>> {
        Box::pin(async move {
            self.call_live(&request.call)?;
            let workspace = request.workspace;
            let mut revision = self.revision(workspace, "unknown");
            let max_depth = request.max_depth.min(MAX_CALL_HIERARCHY_DEPTH);
            let changed: Vec<SemanticEntityRef> = request
                .changed
                .iter()
                .take(MAX_AFFECTED_INPUTS)
                .cloned()
                .collect();
            let mut unavailable = Vec::new();
            let mut degraded = false;
            let mut affected: BTreeMap<SemanticEntityRef, Vec<SemanticFindingProvenance>> =
                BTreeMap::new();
            let mut tests: BTreeMap<SemanticEntityRef, Vec<SemanticFindingProvenance>> =
                BTreeMap::new();
            let mut impact_tests: Vec<(SemanticEntityRef, Vec<SemanticFindingProvenance>)> =
                Vec::new();

            // The repository graph is the primary impact authority. It is
            // synchronous and in-memory (the adapter owns parsing/refresh);
            // every finding carries its relation chain and provenance.
            if let Some(impact) = &self.impact {
                match impact.affected(
                    workspace,
                    &changed,
                    max_depth.max(1),
                    crate::types::MAX_ENTITY_REFS,
                ) {
                    Ok(result) => {
                        if let Some(source_revision) = result.revision {
                            revision = source_revision;
                        }
                        degraded |= result.degraded;
                        if result.truncated {
                            unavailable.push(SemanticUnavailable::new(
                                SemanticSubsystem::Graph,
                                "impact result reached the configured entity bound; the caller must widen conservatively",
                            ));
                        }
                        for finding in result.findings {
                            let subject = finding.entity.entity_id.as_str().to_string();
                            let mut provenance = Vec::new();
                            Self::push_provenance(
                                &mut provenance,
                                &subject,
                                SemanticSubsystem::Graph,
                                &format!("{}:{}", impact.identity(), finding.provenance),
                                SemanticFidelity::Structural,
                                Some(&revision),
                            );
                            if finding.is_test {
                                impact_tests.push((finding.entity, provenance));
                            } else {
                                merge_finding(&mut affected, finding.entity, provenance);
                            }
                        }
                    }
                    Err(error) => self.report_absent(
                        &mut unavailable,
                        &mut degraded,
                        SemanticSubsystem::Graph,
                        error,
                    )?,
                }
            } else {
                degraded = true;
                unavailable.push(SemanticUnavailable::new(
                    SemanticSubsystem::Graph,
                    "no repository impact graph wired",
                ));
            }

            if let Some(index) = &self.index {
                match index.impacted_entities(workspace, &changed, max_depth) {
                    Ok(refs) => {
                        for entity in refs {
                            let subject = entity.entity_id.as_str().to_string();
                            let mut provenance = Vec::new();
                            Self::push_provenance(
                                &mut provenance,
                                &subject,
                                SemanticSubsystem::Index,
                                &index.identity(),
                                SemanticFidelity::Heuristic,
                                Some(&revision),
                            );
                            merge_finding(&mut affected, entity, provenance);
                        }
                    }
                    Err(error) => self.report_absent(
                        &mut unavailable,
                        &mut degraded,
                        SemanticSubsystem::Index,
                        error,
                    )?,
                }
            } else {
                degraded = true;
                unavailable.push(SemanticUnavailable::new(
                    SemanticSubsystem::Index,
                    "no index adapter wired",
                ));
            }

            if let Some(lsp) = &self.lsp {
                if lsp.available(workspace) {
                    // The impact graph is the primary authority; LSP is a
                    // bounded precision add-on. Cap the entities AND the
                    // wall-clock phase so a cold or hostile language server
                    // can never stretch one affected query per entity.
                    let cap = self.affected_lsp_entity_cap.min(changed.len());
                    let cooling_down = self
                        .lsp_backoff_until
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .is_some_and(|until| Instant::now() < until);
                    let started = Instant::now();
                    let mut enriched = 0usize;
                    let mut budget_exhausted = cooling_down;
                    for entity in changed.iter().take(if cooling_down { 0 } else { cap }) {
                        let remaining = self.affected_lsp_budget.saturating_sub(started.elapsed());
                        if remaining.is_zero() {
                            budget_exhausted = true;
                            break;
                        }
                        match tokio::time::timeout(
                            remaining,
                            self.lsp_references(
                                workspace,
                                &revision,
                                entity,
                                lsp.as_ref(),
                                request.call.cancellation.clone(),
                            ),
                        )
                        .await
                        {
                            Ok(Ok(refs)) => {
                                for found in refs {
                                    let subject = found.entity_id.as_str().to_string();
                                    let mut provenance = Vec::new();
                                    Self::push_provenance(
                                        &mut provenance,
                                        &subject,
                                        SemanticSubsystem::Lsp,
                                        &lsp.identity(),
                                        SemanticFidelity::CompilerExact,
                                        Some(&revision),
                                    );
                                    merge_finding(&mut affected, found, provenance);
                                }
                            }
                            Ok(Err(error)) => self.report_absent(
                                &mut unavailable,
                                &mut degraded,
                                SemanticSubsystem::Lsp,
                                error,
                            )?,
                            Err(_) => {
                                budget_exhausted = true;
                                break;
                            }
                        }
                        if max_depth > 1 {
                            let remaining =
                                self.affected_lsp_budget.saturating_sub(started.elapsed());
                            if remaining.is_zero() {
                                budget_exhausted = true;
                                break;
                            }
                            match tokio::time::timeout(
                                remaining,
                                self.lsp_calls(
                                    workspace,
                                    &revision,
                                    entity,
                                    lsp.as_ref(),
                                    max_depth,
                                    request.call.cancellation.clone(),
                                ),
                            )
                            .await
                            {
                                Ok(Ok(refs)) => {
                                    for found in refs {
                                        let subject = found.entity_id.as_str().to_string();
                                        let mut provenance = Vec::new();
                                        Self::push_provenance(
                                            &mut provenance,
                                            &subject,
                                            SemanticSubsystem::Lsp,
                                            &lsp.identity(),
                                            SemanticFidelity::CompilerExact,
                                            Some(&revision),
                                        );
                                        merge_finding(&mut affected, found, provenance);
                                    }
                                }
                                Ok(Err(error)) => self.report_absent(
                                    &mut unavailable,
                                    &mut degraded,
                                    SemanticSubsystem::Lsp,
                                    error,
                                )?,
                                Err(_) => {
                                    budget_exhausted = true;
                                    break;
                                }
                            }
                        }
                        enriched += 1;
                    }
                    if budget_exhausted || cap < changed.len() {
                        degraded = true;
                        unavailable.push(SemanticUnavailable::new(
                            SemanticSubsystem::Lsp,
                            format!(
                                "affected LSP enrichment bounded to {enriched}/{} changed entities (cap {} entities, {}ms budget); graph/index results are complete",
                                changed.len(),
                                cap,
                                self.affected_lsp_budget.as_millis()
                            ),
                        ));
                        if budget_exhausted {
                            *self
                                .lsp_backoff_until
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                                Some(Instant::now() + AFFECTED_LSP_RETRY_BACKOFF);
                        }
                    } else {
                        *self
                            .lsp_backoff_until
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
                    }
                } else {
                    degraded = true;
                    unavailable.push(SemanticUnavailable::new(
                        SemanticSubsystem::Lsp,
                        "no language server is available for this workspace",
                    ));
                }
            } else {
                degraded = true;
                unavailable.push(SemanticUnavailable::new(
                    SemanticSubsystem::Lsp,
                    "no LSP adapter wired",
                ));
            }

            if let Some(tree_sitter) = &self.tree_sitter {
                for entity in &changed {
                    match tree_sitter.outline(workspace, &entity.path) {
                        Ok(refs) => {
                            for found in refs {
                                let subject = found.entity_id.as_str().to_string();
                                let mut provenance = Vec::new();
                                Self::push_provenance(
                                    &mut provenance,
                                    &subject,
                                    SemanticSubsystem::TreeSitter,
                                    &tree_sitter.identity(),
                                    SemanticFidelity::Structural,
                                    Some(&revision),
                                );
                                merge_finding(&mut affected, found, provenance);
                            }
                        }
                        Err(error) => self.report_absent(
                            &mut unavailable,
                            &mut degraded,
                            SemanticSubsystem::TreeSitter,
                            error,
                        )?,
                    }
                    if let Ok(refs) = tree_sitter.tests_in(workspace, &entity.path) {
                        for found in refs {
                            let subject = found.entity_id.as_str().to_string();
                            let mut provenance = Vec::new();
                            Self::push_provenance(
                                &mut provenance,
                                &subject,
                                SemanticSubsystem::TreeSitter,
                                &tree_sitter.identity(),
                                SemanticFidelity::Structural,
                                Some(&revision),
                            );
                            merge_finding(&mut tests, found, provenance);
                        }
                    }
                }
            }

            if let Some(index) = &self.index {
                if let Ok(refs) = index.test_entities(workspace) {
                    for found in refs {
                        let subject = found.entity_id.as_str().to_string();
                        let mut provenance = Vec::new();
                        Self::push_provenance(
                            &mut provenance,
                            &subject,
                            SemanticSubsystem::Index,
                            &index.identity(),
                            SemanticFidelity::Heuristic,
                            Some(&revision),
                        );
                        merge_finding(&mut tests, found, provenance);
                    }
                }
            }
            if tests.is_empty() {
                if let Some(verification) = &self.verification {
                    if let Ok(refs) = verification.test_entities(workspace) {
                        for found in refs {
                            let subject = found.entity_id.as_str().to_string();
                            let mut provenance = Vec::new();
                            Self::push_provenance(
                                &mut provenance,
                                &subject,
                                SemanticSubsystem::Verification,
                                &verification.identity(),
                                SemanticFidelity::Structural,
                                Some(&revision),
                            );
                            merge_finding(&mut tests, found, provenance);
                        }
                    }
                }
            }

            let affected_entities: Vec<SemanticEntityRef> = affected
                .keys()
                .take(crate::types::MAX_ENTITY_REFS)
                .cloned()
                .collect();
            let mut affected_entities = affected_entities;
            Self::sort_refs(&mut affected_entities, crate::types::MAX_ENTITY_REFS);
            let affected_ids: BTreeSet<SemanticEntityId> = affected_entities
                .iter()
                .map(|entity| entity.entity_id.clone())
                .collect();
            let affected_paths: BTreeSet<String> = affected_entities
                .iter()
                .map(|e| e.path.as_str().to_string())
                .collect();
            let mut test_entities: Vec<SemanticEntityRef> = Vec::new();
            let mut test_provenance: Vec<SemanticFindingProvenance> = Vec::new();
            for (entity, entries) in tests {
                if (affected_paths.contains(entity.path.as_str())
                    || changed.iter().any(|c| c.path == entity.path))
                    && test_entities.len() < crate::types::MAX_ENTITY_REFS
                {
                    test_provenance.extend(entries);
                    test_entities.push(entity);
                }
            }
            for (entity, entries) in impact_tests {
                if test_entities.len() >= crate::types::MAX_ENTITY_REFS {
                    break;
                }
                test_provenance.extend(entries);
                test_entities.push(entity);
            }
            Self::sort_refs(&mut test_entities, crate::types::MAX_ENTITY_REFS);
            test_entities.retain(|entity| !affected_ids.contains(&entity.entity_id));
            let mut provenance: Vec<SemanticFindingProvenance> = affected
                .into_values()
                .flatten()
                .take(crate::types::MAX_ENTITY_REFS)
                .collect();
            provenance.extend(test_provenance);
            let payload = AffectedSet {
                affected: affected_entities,
                tests: test_entities,
                degraded,
                provenance,
                unavailable,
            };
            Ok(self.envelope(workspace, request.snapshot_id, payload))
        })
    }

    fn verify(
        &self,
        request: SemanticVerifyRequest,
    ) -> BoxFuture<'_, Result<SemanticEnvelope<SemanticVerification>, SemanticError>> {
        Box::pin(async move {
            self.call_live(&request.call)?;
            let workspace = request.workspace;
            let revision = self.revision(workspace, &request.snapshot_id.to_hex());
            let mut unavailable = Vec::new();
            let mut degraded = false;
            let checks = match &self.verification {
                Some(verification) => match verification.checks(workspace) {
                    Ok(mut checks) => {
                        for check in &mut checks {
                            if check.provenance.is_empty() {
                                Self::push_provenance(
                                    &mut check.provenance,
                                    &check.name,
                                    SemanticSubsystem::Verification,
                                    &verification.identity(),
                                    SemanticFidelity::Structural,
                                    Some(&revision),
                                );
                            }
                        }
                        checks
                    }
                    Err(error) => {
                        self.report_absent(
                            &mut unavailable,
                            &mut degraded,
                            SemanticSubsystem::Verification,
                            error,
                        )?;
                        Vec::new()
                    }
                },
                None => {
                    degraded = true;
                    unavailable.push(SemanticUnavailable::new(
                        SemanticSubsystem::Verification,
                        "no verification adapter wired",
                    ));
                    Vec::new()
                }
            };
            // No checks means we do not know: report the gap, never a fake
            // pass. A discovered-but-unexecuted check reports `passed=false`
            // from its adapter, so it can never become a fabricated pass.
            let passed = !checks.is_empty() && checks.iter().all(|check| check.passed);
            degraded |= checks.is_empty();
            let payload = SemanticVerification {
                passed,
                degraded,
                checks,
                unavailable,
            };
            Ok(self.envelope(workspace, request.snapshot_id, payload))
        })
    }

    fn explain(
        &self,
        request: SemanticExplainRequest,
    ) -> BoxFuture<'_, Result<SemanticEnvelope<SemanticExplanation>, SemanticError>> {
        Box::pin(async move {
            self.call_live(&request.call)?;
            let workspace = request.workspace;
            let revision = self.revision(workspace, &request.snapshot_id.to_hex());
            let mut unavailable = Vec::new();
            let mut degraded = false;
            let mut definitions: BTreeSet<SemanticEntityRef> = BTreeSet::new();
            let mut references: BTreeSet<SemanticEntityRef> = BTreeSet::new();
            let mut provenance = Vec::new();

            if let Some(index) = &self.index {
                match index.declarations(workspace, &request.entity) {
                    Ok(refs) => {
                        for entity in refs {
                            let subject = entity.entity_id.as_str().to_string();
                            Self::push_provenance(
                                &mut provenance,
                                &subject,
                                SemanticSubsystem::Index,
                                &index.identity(),
                                SemanticFidelity::Heuristic,
                                Some(&revision),
                            );
                            definitions.insert(entity);
                        }
                    }
                    Err(error) => self.report_absent(
                        &mut unavailable,
                        &mut degraded,
                        SemanticSubsystem::Index,
                        error,
                    )?,
                }
            } else {
                degraded = true;
                unavailable.push(SemanticUnavailable::new(
                    SemanticSubsystem::Index,
                    "no index adapter wired",
                ));
            }
            if let Some(lsp) = &self.lsp {
                if lsp.available(workspace) {
                    match self
                        .lsp_definitions(
                            workspace,
                            &revision,
                            &request.entity,
                            lsp.as_ref(),
                            request.call.cancellation.clone(),
                        )
                        .await
                    {
                        Ok(refs) => {
                            for entity in refs {
                                let subject = entity.entity_id.as_str().to_string();
                                Self::push_provenance(
                                    &mut provenance,
                                    &subject,
                                    SemanticSubsystem::Lsp,
                                    &lsp.identity(),
                                    SemanticFidelity::CompilerExact,
                                    Some(&revision),
                                );
                                definitions.insert(entity);
                            }
                        }
                        Err(error) => self.report_absent(
                            &mut unavailable,
                            &mut degraded,
                            SemanticSubsystem::Lsp,
                            error,
                        )?,
                    }
                    match self
                        .lsp_references(
                            workspace,
                            &revision,
                            &request.entity,
                            lsp.as_ref(),
                            request.call.cancellation.clone(),
                        )
                        .await
                    {
                        Ok(refs) => {
                            for entity in refs {
                                let subject = entity.entity_id.as_str().to_string();
                                Self::push_provenance(
                                    &mut provenance,
                                    &subject,
                                    SemanticSubsystem::Lsp,
                                    &lsp.identity(),
                                    SemanticFidelity::CompilerExact,
                                    Some(&revision),
                                );
                                references.insert(entity);
                            }
                        }
                        Err(error) => self.report_absent(
                            &mut unavailable,
                            &mut degraded,
                            SemanticSubsystem::Lsp,
                            error,
                        )?,
                    }
                } else {
                    degraded = true;
                    unavailable.push(SemanticUnavailable::new(
                        SemanticSubsystem::Lsp,
                        "no language server is available for this workspace",
                    ));
                }
            } else {
                degraded = true;
                unavailable.push(SemanticUnavailable::new(
                    SemanticSubsystem::Lsp,
                    "no LSP adapter wired",
                ));
            }
            if let Some(tree_sitter) = &self.tree_sitter {
                if let Ok(refs) = tree_sitter.outline(workspace, &request.entity.path) {
                    for entity in refs {
                        let subject = entity.entity_id.as_str().to_string();
                        Self::push_provenance(
                            &mut provenance,
                            &subject,
                            SemanticSubsystem::TreeSitter,
                            &tree_sitter.identity(),
                            SemanticFidelity::Structural,
                            Some(&revision),
                        );
                        definitions.insert(entity);
                    }
                }
            } else {
                degraded = true;
                unavailable.push(SemanticUnavailable::new(
                    SemanticSubsystem::TreeSitter,
                    "no tree-sitter adapter wired",
                ));
            }

            let definitions: Vec<_> = definitions
                .into_iter()
                .take(crate::types::MAX_ENTITY_REFS)
                .collect();
            let references: Vec<_> = references
                .into_iter()
                .take(crate::types::MAX_ENTITY_REFS)
                .collect();
            let mut citations = definitions.clone();
            for reference in &references {
                if !citations.contains(reference) {
                    citations.push(reference.clone());
                }
            }
            citations.sort();
            let mut seen_citations = BTreeSet::new();
            citations.retain(|citation| {
                citation.entity_id != request.entity.entity_id
                    && seen_citations.insert(citation.entity_id.clone())
            });
            // The payload's own `entity` is validated together with the
            // citations: never repeat its id in the citation list.
            citations.truncate(crate::types::MAX_ENTITY_REFS);
            let text = if definitions.is_empty() && references.is_empty() {
                format!(
                    "entity {} ({}): no native definition or reference evidence was found \
                     from {}; no explanation fabricated{}",
                    request.entity.entity_id,
                    request.entity.path,
                    self.source_id(),
                    unavailable_suffix(&unavailable),
                )
            } else {
                format!(
                    "entity {} ({}): {} native definition(s) and {} native reference(s) from \
                     {}{}",
                    request.entity.entity_id,
                    request.entity.path,
                    definitions.len(),
                    references.len(),
                    self.source_id(),
                    unavailable_suffix(&unavailable),
                )
            };
            Self::push_provenance(
                &mut provenance,
                request.entity.entity_id.as_str(),
                if self.lsp.is_some() {
                    SemanticSubsystem::Lsp
                } else {
                    SemanticSubsystem::Index
                },
                &self.source_id(),
                SemanticFidelity::Heuristic,
                Some(&revision),
            );
            let _ = degraded;
            let payload = SemanticExplanation {
                entity: request.entity,
                text,
                citations,
                provenance,
                unavailable,
            };
            Ok(self.envelope(workspace, request.snapshot_id, payload))
        })
    }
}

impl GenericSemanticFallback {
    fn cache_get(&self, key: &str) -> Option<CacheValue> {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(key)
    }

    fn cache_put(&self, key: String, value: CacheValue) {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .put(key, value);
    }

    async fn lsp_definitions(
        &self,
        workspace: WorkspaceId,
        revision: &str,
        entity: &SemanticEntityRef,
        lsp: &dyn LspMetadata,
        cancellation: CancellationToken,
    ) -> Result<Vec<SemanticEntityRef>, SemanticError> {
        let key = self.refs_cache_key("def", workspace, revision, entity);
        if let Some(CacheValue::Refs(refs)) = self.cache_get(&key) {
            return Ok(refs);
        }
        let mut refs = lsp.definitions(workspace, entity, cancellation).await?;
        Self::sort_refs(&mut refs, crate::types::MAX_ENTITY_REFS);
        self.cache_put(key, CacheValue::Refs(refs.clone()));
        Ok(refs)
    }

    async fn lsp_references(
        &self,
        workspace: WorkspaceId,
        revision: &str,
        entity: &SemanticEntityRef,
        lsp: &dyn LspMetadata,
        cancellation: CancellationToken,
    ) -> Result<Vec<SemanticEntityRef>, SemanticError> {
        let key = self.refs_cache_key("ref", workspace, revision, entity);
        if let Some(CacheValue::Refs(refs)) = self.cache_get(&key) {
            return Ok(refs);
        }
        let mut refs = lsp.references(workspace, entity, cancellation).await?;
        Self::sort_refs(&mut refs, crate::types::MAX_ENTITY_REFS);
        self.cache_put(key, CacheValue::Refs(refs.clone()));
        Ok(refs)
    }

    async fn lsp_calls(
        &self,
        workspace: WorkspaceId,
        revision: &str,
        entity: &SemanticEntityRef,
        lsp: &dyn LspMetadata,
        max_depth: u32,
        cancellation: CancellationToken,
    ) -> Result<Vec<SemanticEntityRef>, SemanticError> {
        let key = format!(
            "lsp:calls:{}:{revision}:{}:{}:{max_depth}",
            workspace.raw(),
            entity.path.as_str(),
            entity.entity_id.as_str()
        );
        if let Some(CacheValue::Refs(refs)) = self.cache_get(&key) {
            return Ok(refs);
        }
        let mut refs = lsp
            .call_hierarchy(workspace, entity, true, max_depth, cancellation)
            .await?;
        Self::sort_refs(&mut refs, crate::types::MAX_ENTITY_REFS);
        self.cache_put(key, CacheValue::Refs(refs.clone()));
        Ok(refs)
    }
}

fn merge_context_item(
    merged: &mut BTreeMap<(String, String), SemanticContextItem>,
    item: SemanticContextItem,
) {
    let key = (
        item.entity.path.as_str().to_string(),
        item.entity.entity_id.as_str().to_string(),
    );
    match merged.get_mut(&key) {
        Some(existing) => {
            existing.relevance_bps = existing.relevance_bps.max(item.relevance_bps);
            for entry in item.provenance {
                if !existing.provenance.iter().any(|current| {
                    current.subject == entry.subject
                        && current.subsystem == entry.subsystem
                        && current.source == entry.source
                }) {
                    existing.provenance.push(entry);
                }
            }
        }
        None => {
            merged.insert(key, item);
        }
    }
}

fn merge_finding(
    map: &mut BTreeMap<SemanticEntityRef, Vec<SemanticFindingProvenance>>,
    entity: SemanticEntityRef,
    provenance: Vec<SemanticFindingProvenance>,
) {
    let entry = map.entry(entity).or_default();
    for item in provenance {
        if !entry.iter().any(|current| {
            current.subject == item.subject
                && current.subsystem == item.subsystem
                && current.source == item.source
        }) {
            entry.push(item);
        }
    }
}

fn unavailable_suffix(unavailable: &[SemanticUnavailable]) -> String {
    if unavailable.is_empty() {
        return String::new();
    }
    let reasons: Vec<String> = unavailable
        .iter()
        .take(4)
        .map(|entry| format!("{}: {}", entry.subsystem.as_str(), entry.reason))
        .collect();
    format!("; unavailable: {}", reasons.join("; "))
}

/// A request for a constrained semantic edit proposal.
#[derive(Debug, Clone)]
pub struct SemanticEditRequest {
    pub call: SemanticCall,
    pub workspace: WorkspaceId,
    pub snapshot_id: SemanticSnapshotId,
    pub entity: SemanticEntityRef,
    pub intent: String,
}

/// One proposed file replacement. The path type already refuses absolute
/// paths and `..`; validation re-checks defense in depth.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SemanticFileEdit {
    pub path: WorkspacePath,
    pub replacement: String,
}

/// A constrained edit proposal. The provider proposes; Faktor validates and
/// applies through its own machinery. There is deliberately no `apply` here.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProposedPatch {
    pub workspace: WorkspaceId,
    pub snapshot_id: SemanticSnapshotId,
    pub file_edits: Vec<SemanticFileEdit>,
    /// Expected current content hash per workspace-relative path.
    pub expected_source_hashes: BTreeMap<String, FileHash>,
    pub semantic_delta: SemanticDelta,
}

/// Read-only view of current source hashes, supplied by the Faktor-side
/// applier. This crate never reads files itself.
pub trait SourceHashView {
    fn current_hash(&self, path: &WorkspacePath) -> Option<FileHash>;
}

impl ProposedPatch {
    /// Validate a proposal BEFORE any apply stage: workspace/snapshot
    /// identity, path safety and uniqueness, complete expected-hash coverage,
    /// freshness against the applier's hash view, delta refs inside the
    /// workspace, and payload bounds.
    pub fn validate(
        &self,
        expected_workspace: WorkspaceId,
        expected_snapshot: SemanticSnapshotId,
        view: &dyn SourceHashView,
        caps: &SemanticResponseCaps,
    ) -> Result<(), SemanticError> {
        if self.workspace != expected_workspace {
            return Err(SemanticError::WorkspaceMismatch {
                expected: expected_workspace,
                actual: self.workspace,
            });
        }
        if self.snapshot_id != expected_snapshot {
            return Err(SemanticError::SnapshotMismatch {
                expected: expected_snapshot,
                actual: self.snapshot_id,
            });
        }
        if self.semantic_delta.workspace != expected_workspace {
            return Err(SemanticError::WorkspaceMismatch {
                expected: expected_workspace,
                actual: self.semantic_delta.workspace,
            });
        }
        let expectation = SemanticExpectation::new(expected_workspace, expected_snapshot);
        if self.file_edits.is_empty() {
            return Err(SemanticError::Refused(
                "proposed patch carries no file edits".to_string(),
            ));
        }

        let mut seen_paths = BTreeSet::new();
        let mut total_bytes = 0usize;
        for edit in &self.file_edits {
            edit.path.ensure_safe()?;
            if !seen_paths.insert(edit.path.as_str()) {
                return Err(SemanticError::DuplicateEntity(edit.path.to_string()));
            }
            let Some(expected) = self.expected_source_hashes.get(edit.path.as_str()) else {
                return Err(SemanticError::Malformed(format!(
                    "no expected source hash for edit path {}",
                    edit.path
                )));
            };
            let actual = view.current_hash(&edit.path);
            if actual.as_ref() != Some(expected) {
                return Err(SemanticError::StaleHash {
                    path: edit.path.to_string(),
                    expected: *expected,
                    actual,
                });
            }
            total_bytes = total_bytes.saturating_add(edit.replacement.len());
        }

        // Hash-map keys are attacker-controlled strings: they parse through
        // the same path grammar, so `../../outside` is refused here too.
        for key in self.expected_source_hashes.keys() {
            let path = WorkspacePath::parse(key)?;
            if !seen_paths.contains(path.as_str()) {
                return Err(SemanticError::Malformed(format!(
                    "expected hash for {key:?} has no matching file edit"
                )));
            }
        }

        // Delta refs must live in the expected workspace and be duplicate
        // free, exactly like any other semantic payload.
        let mut seen_ids = BTreeSet::new();
        for change in &self.semantic_delta.changes {
            change.entity.validate_for(expectation.workspace)?;
            if !seen_ids.insert(change.entity.entity_id.clone()) {
                return Err(SemanticError::DuplicateEntity(
                    change.entity.entity_id.to_string(),
                ));
            }
        }

        total_bytes = total_bytes.saturating_add(self.semantic_delta.byte_len()?);
        if total_bytes > caps.max_payload_bytes {
            return Err(SemanticError::Oversized {
                max: caps.max_payload_bytes,
                actual: total_bytes,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{block_on, call, entity, snapshot};
    use crate::types::{
        SemanticContextRequest, SemanticDeltaKind, SemanticEntityId, WorkspacePath,
    };
    use std::collections::BTreeMap;

    /// Timed executor for the LSP-budget tests: `tokio::time::timeout` and
    /// the slow seam need a time driver, unlike the first-poll-only default
    /// futures.
    fn block_on_timed<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("current-thread runtime")
            .block_on(future)
    }

    struct TestIndex {
        revision: String,
        items: Vec<SemanticContextItem>,
    }

    impl IndexMetadata for TestIndex {
        fn snapshot_revision(&self, _workspace: WorkspaceId) -> Option<String> {
            Some(self.revision.clone())
        }

        fn entity_count(&self, _workspace: WorkspaceId) -> u64 {
            self.items.len() as u64
        }

        fn context_items(
            &self,
            _workspace: WorkspaceId,
            _query: &str,
            limit: usize,
        ) -> Result<Vec<SemanticContextItem>, SemanticError> {
            Ok(self
                .items
                .iter()
                .take(limit.saturating_add(10))
                .cloned()
                .collect())
        }
    }

    struct MapHashes(BTreeMap<String, FileHash>);

    impl SourceHashView for MapHashes {
        fn current_hash(&self, path: &WorkspacePath) -> Option<FileHash> {
            self.0.get(path.as_str()).copied()
        }
    }

    fn context_request() -> SemanticContextRequest {
        SemanticContextRequest {
            call: call(),
            workspace: WorkspaceId::new(1),
            source_revision: "rev-1".to_string(),
            snapshot_id: snapshot(WorkspaceId::new(1), "rev-1"),
            query: "q".to_string(),
            max_items: 8,
            max_bytes: 4096,
        }
    }

    fn item(path: &str, id: &str, excerpt: &str) -> SemanticContextItem {
        SemanticContextItem {
            entity: entity(path, id),
            relevance_bps: 9000,
            excerpt: excerpt.to_string(),
            provenance: Vec::new(),
        }
    }

    fn hash(byte: u8) -> FileHash {
        FileHash::from([byte; 32])
    }

    fn proposal(edit_path: &str, expected: Option<FileHash>) -> ProposedPatch {
        let file_edits = match WorkspacePath::parse(edit_path) {
            Ok(path) => vec![SemanticFileEdit {
                path,
                replacement: "fn main() {}".to_string(),
            }],
            Err(_) => Vec::new(),
        };
        let mut expected_source_hashes = BTreeMap::new();
        if let Some(hash) = expected {
            expected_source_hashes.insert(edit_path.to_string(), hash);
        }
        let workspace = WorkspaceId::new(1);
        ProposedPatch {
            workspace,
            snapshot_id: snapshot(workspace, "rev-1"),
            file_edits,
            expected_source_hashes,
            semantic_delta: SemanticDelta {
                workspace,
                from_snapshot: snapshot(workspace, "rev-1"),
                to_snapshot: snapshot(workspace, "rev-2"),
                changes: Vec::new(),
                degraded: true,
                unavailable: Vec::new(),
            },
        }
    }

    #[test]
    fn context_without_seams_is_degraded_empty_but_ok() {
        let fallback = GenericSemanticFallback::default();
        let envelope = block_on(fallback.context(context_request())).unwrap();
        assert!(envelope.payload.degraded);
        assert!(envelope.payload.items.is_empty());
        assert!(!envelope.payload.truncated);
        // Provenance is provider DATA, never instruction authority.
        assert!(envelope.assert_data_only().is_ok());
    }

    #[test]
    fn index_seam_feeds_bounded_context() {
        let index = Arc::new(TestIndex {
            revision: "rev-1".to_string(),
            items: vec![
                item("src/a.rs", "a", "aaaa"),
                item("src/b.rs", "b", "bbbb"),
                item("src/c.rs", "c", "cccc"),
            ],
        });
        let fallback = GenericSemanticFallback::default().with_index(index);
        let mut request = context_request();
        request.max_items = 2;
        let envelope = block_on(fallback.context(request)).unwrap();
        assert!(!envelope.payload.degraded);
        assert_eq!(envelope.payload.items.len(), 2);
        assert!(envelope.payload.truncated);

        // Byte bound truncates before the item that would exceed it.
        let index = Arc::new(TestIndex {
            revision: "rev-1".to_string(),
            items: vec![item("src/a.rs", "a", "aaaa"), item("src/b.rs", "b", "bbbb")],
        });
        let fallback = GenericSemanticFallback::default().with_index(index);
        let mut request = context_request();
        request.max_bytes = 6;
        let envelope = block_on(fallback.context(request)).unwrap();
        assert_eq!(envelope.payload.items.len(), 1);
        assert!(envelope.payload.truncated);

        // Snapshot picks up the index revision and entity count.
        let snapshot_env = block_on(fallback.snapshot(SemanticSnapshotRequest {
            call: call(),
            workspace: WorkspaceId::new(1),
            source_revision: "ignored".to_string(),
        }))
        .unwrap();
        assert_eq!(snapshot_env.payload.source_revision, "rev-1");
        assert_eq!(snapshot_env.payload.entity_count, 2);
    }

    #[test]
    fn verification_without_metadata_reports_gap_not_pass() {
        let fallback = GenericSemanticFallback::default();
        let envelope = block_on(fallback.verify(SemanticVerifyRequest {
            call: call(),
            workspace: WorkspaceId::new(1),
            snapshot_id: snapshot(WorkspaceId::new(1), "rev-1"),
            entity: entity("src/lib.rs", "lib"),
            claim: "compiles".to_string(),
        }))
        .unwrap();
        assert!(!envelope.payload.passed);
        assert!(envelope.payload.degraded);
        assert!(envelope.payload.checks.is_empty());
    }

    #[test]
    fn proposal_validation_rejects_traversal_and_stale_hashes() {
        let caps = SemanticResponseCaps::default();
        let workspace = WorkspaceId::new(1);
        let snapshot_id = snapshot(workspace, "rev-1");

        // Path traversal is refused at the type boundary.
        match WorkspacePath::parse("../../outside") {
            Err(SemanticError::PathTraversal(_)) => {}
            other => panic!("expected PathTraversal, got {other:?}"),
        }
        // And again when a hash-map key carries the traversal string.
        let mut hostile = proposal("src/lib.rs", Some(hash(1)));
        hostile
            .expected_source_hashes
            .insert("../../outside".to_string(), hash(1));
        let view = MapHashes(BTreeMap::from([("src/lib.rs".to_string(), hash(1))]));
        match hostile.validate(workspace, snapshot_id, &view, &caps) {
            Err(SemanticError::PathTraversal(_)) => {}
            other => panic!("expected PathTraversal from hash key, got {other:?}"),
        }

        // Stale hash: view holds different content.
        let stale = proposal("src/lib.rs", Some(hash(1)));
        let view = MapHashes(BTreeMap::from([("src/lib.rs".to_string(), hash(2))]));
        match stale.validate(workspace, snapshot_id, &view, &caps) {
            Err(SemanticError::StaleHash {
                path,
                actual: Some(actual),
                ..
            }) => {
                assert_eq!(path, "src/lib.rs");
                assert_eq!(actual, hash(2));
            }
            other => panic!("expected StaleHash, got {other:?}"),
        }

        // Missing file: also stale, with actual None.
        let missing = proposal("src/lib.rs", Some(hash(1)));
        let view = MapHashes(BTreeMap::new());
        match missing.validate(workspace, snapshot_id, &view, &caps) {
            Err(SemanticError::StaleHash { actual: None, .. }) => {}
            other => panic!("expected StaleHash(missing), got {other:?}"),
        }

        // Missing expected hash for an edit is malformed, never applied.
        let no_hash = proposal("src/lib.rs", None);
        let view = MapHashes(BTreeMap::from([("src/lib.rs".to_string(), hash(1))]));
        assert!(matches!(
            no_hash.validate(workspace, snapshot_id, &view, &caps),
            Err(SemanticError::Malformed(_))
        ));
    }

    #[test]
    fn proposal_validation_accepts_fresh_hash_and_rejects_bad_identity() {
        let caps = SemanticResponseCaps::default();
        let workspace = WorkspaceId::new(1);
        let snapshot_id = snapshot(workspace, "rev-1");
        let patch = proposal("src/lib.rs", Some(hash(7)));
        let view = MapHashes(BTreeMap::from([("src/lib.rs".to_string(), hash(7))]));
        assert!(patch.validate(workspace, snapshot_id, &view, &caps).is_ok());

        // Wrong workspace / stale snapshot are typed and precede hash checks.
        let view = MapHashes(BTreeMap::from([("src/lib.rs".to_string(), hash(7))]));
        assert!(matches!(
            patch.validate(WorkspaceId::new(2), snapshot_id, &view, &caps),
            Err(SemanticError::WorkspaceMismatch { .. })
        ));
        assert!(matches!(
            patch.validate(workspace, snapshot(workspace, "rev-old"), &view, &caps),
            Err(SemanticError::SnapshotMismatch { .. })
        ));

        // Delta refs outside the workspace are refused.
        let mut outside = patch.clone();
        outside.semantic_delta.changes.push(SemanticDeltaChange {
            entity: SemanticEntityRef::new(
                WorkspaceId::new(9),
                WorkspacePath::parse("src/lib.rs").unwrap(),
                SemanticEntityId::parse("ghost").unwrap(),
            ),
            kind: SemanticDeltaKind::Modified,
            old_hash: None,
            new_hash: None,
            provenance: Vec::new(),
        });
        assert!(matches!(
            outside.validate(workspace, snapshot_id, &view, &caps),
            Err(SemanticError::InvalidEntityRef(_))
        ));

        // Duplicate delta entity ids are refused.
        let mut duplicate = patch;
        duplicate.semantic_delta.changes.push(SemanticDeltaChange {
            entity: entity("src/lib.rs", "dup"),
            kind: SemanticDeltaKind::Added,
            old_hash: None,
            new_hash: None,
            provenance: Vec::new(),
        });
        duplicate.semantic_delta.changes.push(SemanticDeltaChange {
            entity: entity("src/other.rs", "dup"),
            kind: SemanticDeltaKind::Removed,
            old_hash: None,
            new_hash: None,
            provenance: Vec::new(),
        });
        assert!(matches!(
            duplicate.validate(workspace, snapshot_id, &view, &caps),
            Err(SemanticError::DuplicateEntity(_))
        ));
    }

    #[test]
    fn proposal_validation_rejects_oversize_payload() {
        let workspace = WorkspaceId::new(1);
        let snapshot_id = snapshot(workspace, "rev-1");
        let mut patch = proposal("src/lib.rs", Some(hash(1)));
        patch.file_edits[0].replacement = "x".repeat(4096);
        let view = MapHashes(BTreeMap::from([("src/lib.rs".to_string(), hash(1))]));
        let caps = SemanticResponseCaps::new(1, 64, 16);
        assert!(matches!(
            patch.validate(workspace, snapshot_id, &view, &caps),
            Err(SemanticError::Oversized { max: 64, .. })
        ));
    }

    struct FakeTreeSitter;

    impl TreeSitterMetadata for FakeTreeSitter {
        fn identity(&self) -> String {
            "tree-sitter:test".to_string()
        }
    }

    struct FakeVerification;

    impl VerificationMetadata for FakeVerification {
        fn identity(&self) -> String {
            "verification:test".to_string()
        }
    }

    struct FakeImpact;

    impl ImpactMetadata for FakeImpact {
        fn identity(&self) -> String {
            "test-graph".to_string()
        }

        fn affected(
            &self,
            _workspace: WorkspaceId,
            _changed: &[SemanticEntityRef],
            _max_depth: u32,
            _max_results: usize,
        ) -> Result<ImpactMetadataResult, SemanticError> {
            Ok(ImpactMetadataResult {
                findings: vec![
                    ImpactedFinding {
                        entity: entity("src/caller.rs", "caller"),
                        is_test: false,
                        reason: "calls -> symbol:src/a.rs#a@1".to_string(),
                        provenance: "calls".to_string(),
                    },
                    ImpactedFinding {
                        entity: entity("src/caller.rs", "checks"),
                        is_test: true,
                        reason: "covers -> symbol:src/a.rs#a@1".to_string(),
                        provenance: "covers".to_string(),
                    },
                ],
                truncated: false,
                revision: Some("abc123".to_string()),
                degraded: false,
            })
        }
    }

    /// The native adapter declares REAL per-operation quality from the
    /// wired sources: filesystem/lexical (tier 0), tree-sitter (1), LSP (2),
    /// graph impact (3), runtime/verification (4). LSP-backed operations
    /// report higher than heuristic ones; an absent seam never inherits a
    /// stronger claim.
    #[test]
    fn native_operation_quality_tracks_wired_evidence_sources() {
        use crate::types::SemanticEvidenceSource;

        let bare = GenericSemanticFallback::default();
        let descriptor = bare.descriptor();
        let context = descriptor.operation_quality(SemanticOp::Context).unwrap();
        assert_eq!(context.provenance, SemanticEvidenceSource::Filesystem);
        assert_eq!(context.tier, Some(0));
        assert_eq!(context.fidelity, SemanticFidelity::Heuristic);
        let affected = descriptor.operation_quality(SemanticOp::Affected).unwrap();
        assert_eq!(affected.fidelity, SemanticFidelity::Heuristic);
        assert_eq!(affected.completeness, SemanticCompleteness::Unknown);

        let with_lsp =
            GenericSemanticFallback::default().with_lsp(Arc::new(CountingLsp::default()));
        let descriptor = with_lsp.descriptor();
        let context = descriptor.operation_quality(SemanticOp::Context).unwrap();
        assert_eq!(context.provenance, SemanticEvidenceSource::Lsp);
        assert_eq!(context.tier, Some(2));
        assert_eq!(context.fidelity, SemanticFidelity::Structural);
        assert_eq!(context.completeness, SemanticCompleteness::Partial);
        let explain = descriptor.operation_quality(SemanticOp::Explain).unwrap();
        assert_eq!(explain.fidelity, SemanticFidelity::CompilerExact);
        assert_eq!(explain.tier, Some(2));

        let with_graph = GenericSemanticFallback::default().with_impact(Arc::new(FakeImpact));
        let graph_descriptor = with_graph.descriptor();
        let affected = graph_descriptor
            .operation_quality(SemanticOp::Affected)
            .unwrap();
        assert_eq!(affected.provenance, SemanticEvidenceSource::Graph);
        assert_eq!(affected.tier, Some(3));
        assert_eq!(affected.fidelity, SemanticFidelity::Structural);
        assert_eq!(affected.completeness, SemanticCompleteness::Partial);
        assert_eq!(
            affected.evidence.revision_binding,
            crate::intelligence::RevisionBinding::Exact,
            "the native graph is revision-bound, not an inferred guess"
        );

        let with_syntax =
            GenericSemanticFallback::default().with_tree_sitter(Arc::new(FakeTreeSitter));
        let syntax_descriptor = with_syntax.descriptor();
        let explain = syntax_descriptor
            .operation_quality(SemanticOp::Explain)
            .unwrap();
        assert_eq!(explain.provenance, SemanticEvidenceSource::TreeSitter);
        assert_eq!(explain.tier, Some(1));
        assert_eq!(explain.fidelity, SemanticFidelity::Structural);

        let with_verify =
            GenericSemanticFallback::default().with_verification(Arc::new(FakeVerification));
        let descriptor = with_verify.descriptor();
        let verify = descriptor.operation_quality(SemanticOp::Verify).unwrap();
        assert_eq!(verify.provenance, SemanticEvidenceSource::RuntimeHistory);
        assert_eq!(verify.tier, Some(4));
        assert_eq!(verify.fidelity, SemanticFidelity::Structural);
        // Discovered checks are never a proof: the authoritative verify
        // requirement stays above the native declaration.
        assert!(with_verify.capabilities().supports(SemanticOp::Verify));
        let status = with_verify.operation_status(SemanticOp::Verify);
        let declared = SemanticOperationQuality::new(
            status.fidelity,
            status.completeness,
            None,
            crate::intelligence::EvidenceDimensions::UNKNOWN,
            status.provenance,
        );
        assert!(
            !crate::quality::SemanticQualityRequirement::authoritative(SemanticOp::Verify)
                .accepts(&declared)
        );
    }

    struct TruncatedImpact;

    impl ImpactMetadata for TruncatedImpact {
        fn affected(
            &self,
            _workspace: WorkspaceId,
            _changed: &[SemanticEntityRef],
            _max_depth: u32,
            _max_results: usize,
        ) -> Result<ImpactMetadataResult, SemanticError> {
            Ok(ImpactMetadataResult {
                findings: Vec::new(),
                truncated: true,
                revision: None,
                degraded: false,
            })
        }
    }

    #[test]
    fn impact_graph_findings_are_merged_with_provenance_and_tests() {
        let fallback = GenericSemanticFallback::default().with_impact(Arc::new(FakeImpact));
        let workspace = WorkspaceId::new(1);
        let request = AffectedRequest {
            call: call(),
            workspace,
            snapshot_id: snapshot(workspace, "rev-1"),
            changed: vec![entity("src/a.rs", "a")],
            max_depth: 2,
        };
        let envelope = block_on(fallback.affected(request)).unwrap();
        assert!(envelope
            .payload
            .affected
            .iter()
            .any(|entity| entity.entity_id.as_str() == "caller"));
        assert!(envelope
            .payload
            .tests
            .iter()
            .any(|entity| entity.entity_id.as_str() == "checks"));
        assert!(envelope.payload.provenance.iter().any(|entry| {
            entry.subsystem == SemanticSubsystem::Graph
                && entry.source == "test-graph:calls"
                && entry.revision.as_deref() == Some("abc123")
        }));
        assert!(!envelope
            .payload
            .unavailable
            .iter()
            .any(|entry| entry.subsystem == SemanticSubsystem::Graph));
    }

    #[test]
    fn impact_graph_truncation_is_typed_not_silent() {
        let fallback = GenericSemanticFallback::default().with_impact(Arc::new(TruncatedImpact));
        let workspace = WorkspaceId::new(1);
        let request = AffectedRequest {
            call: call(),
            workspace,
            snapshot_id: snapshot(workspace, "rev-1"),
            changed: vec![entity("src/a.rs", "a")],
            max_depth: 1,
        };
        let envelope = block_on(fallback.affected(request)).unwrap();
        assert!(envelope.payload.unavailable.iter().any(|entry| {
            entry.subsystem == SemanticSubsystem::Graph && entry.reason.contains("bound")
        }));
    }

    #[test]
    fn explain_without_seams_is_typed_unavailable_and_never_fabricates() {
        let workspace = WorkspaceId::new(1);
        let envelope = block_on(GenericSemanticFallback::default().explain(
            crate::types::SemanticExplainRequest {
                call: call(),
                workspace,
                snapshot_id: snapshot(workspace, "rev-1"),
                entity: entity("src/a.rs", "a"),
                question: "why?".to_string(),
            },
        ))
        .unwrap();
        assert!(envelope.payload.citations.is_empty());
        assert!(envelope.payload.text.contains("no explanation fabricated"));
        for subsystem in [
            SemanticSubsystem::Index,
            SemanticSubsystem::Lsp,
            SemanticSubsystem::TreeSitter,
        ] {
            assert!(
                envelope
                    .payload
                    .unavailable
                    .iter()
                    .any(|entry| entry.subsystem == subsystem),
                "absent {subsystem:?} must be typed unavailable"
            );
        }
    }

    struct CountingLsp {
        references: std::sync::atomic::AtomicUsize,
        symbols: std::sync::atomic::AtomicUsize,
    }

    impl Default for CountingLsp {
        fn default() -> Self {
            Self {
                references: std::sync::atomic::AtomicUsize::new(0),
                symbols: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    impl LspMetadata for CountingLsp {
        fn identity(&self) -> String {
            "lsp:test".to_string()
        }

        fn definitions<'a>(
            &'a self,
            _workspace: WorkspaceId,
            _entity: &'a SemanticEntityRef,
            _cancellation: CancellationToken,
        ) -> BoxFuture<'a, Result<Vec<SemanticEntityRef>, SemanticError>> {
            Box::pin(async { Ok(vec![entity("src/def.rs", "def")]) })
        }

        fn references<'a>(
            &'a self,
            _workspace: WorkspaceId,
            _entity: &'a SemanticEntityRef,
            _cancellation: CancellationToken,
        ) -> BoxFuture<'a, Result<Vec<SemanticEntityRef>, SemanticError>> {
            self.references
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Ok(vec![entity("src/use.rs", "user")]) })
        }

        fn workspace_symbols<'a>(
            &'a self,
            _workspace: WorkspaceId,
            _query: &'a str,
            _limit: usize,
            _cancellation: CancellationToken,
        ) -> BoxFuture<'a, Result<Vec<SemanticEntityRef>, SemanticError>> {
            self.symbols
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Ok(vec![entity("src/sym.rs", "symbol")]) })
        }

        fn call_hierarchy<'a>(
            &'a self,
            _workspace: WorkspaceId,
            _entity: &'a SemanticEntityRef,
            _incoming: bool,
            _max_depth: u32,
            _cancellation: CancellationToken,
        ) -> BoxFuture<'a, Result<Vec<SemanticEntityRef>, SemanticError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    struct SlowLsp {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl Default for SlowLsp {
        fn default() -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    impl LspMetadata for SlowLsp {
        fn identity(&self) -> String {
            "lsp:slow".to_string()
        }

        fn definitions<'a>(
            &'a self,
            _workspace: WorkspaceId,
            _entity: &'a SemanticEntityRef,
            _cancellation: CancellationToken,
        ) -> BoxFuture<'a, Result<Vec<SemanticEntityRef>, SemanticError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn references<'a>(
            &'a self,
            _workspace: WorkspaceId,
            _entity: &'a SemanticEntityRef,
            _cancellation: CancellationToken,
        ) -> BoxFuture<'a, Result<Vec<SemanticEntityRef>, SemanticError>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(250)).await;
                Ok(vec![entity("src/use.rs", "user")])
            })
        }

        fn workspace_symbols<'a>(
            &'a self,
            _workspace: WorkspaceId,
            _query: &'a str,
            _limit: usize,
            _cancellation: CancellationToken,
        ) -> BoxFuture<'a, Result<Vec<SemanticEntityRef>, SemanticError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn call_hierarchy<'a>(
            &'a self,
            _workspace: WorkspaceId,
            _entity: &'a SemanticEntityRef,
            _incoming: bool,
            _max_depth: u32,
            _cancellation: CancellationToken,
        ) -> BoxFuture<'a, Result<Vec<SemanticEntityRef>, SemanticError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    struct MutableIndex {
        revision: Arc<Mutex<String>>,
    }

    impl IndexMetadata for MutableIndex {
        fn identity(&self) -> String {
            "index:test".to_string()
        }

        fn snapshot_revision(&self, _workspace: WorkspaceId) -> Option<String> {
            Some(
                self.revision
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone(),
            )
        }
    }

    fn affected_request() -> AffectedRequest {
        let workspace = WorkspaceId::new(1);
        AffectedRequest {
            call: call(),
            workspace,
            snapshot_id: snapshot(workspace, "rev-1"),
            changed: vec![entity("src/a.rs", "a")],
            max_depth: 1,
        }
    }

    #[test]
    fn lsp_findings_carry_provenance_and_absent_index_is_typed_unavailable() {
        let fallback =
            GenericSemanticFallback::default().with_lsp(Arc::new(CountingLsp::default()));
        let envelope = block_on(fallback.context(context_request())).unwrap();
        assert!(!envelope.payload.items.is_empty());
        assert!(envelope.payload.items.iter().all(|item| item
            .provenance
            .iter()
            .any(|entry| entry.subsystem == SemanticSubsystem::Lsp
                && entry.fidelity == SemanticFidelity::CompilerExact)));
        assert!(envelope
            .payload
            .unavailable
            .iter()
            .any(|entry| entry.subsystem == SemanticSubsystem::Index));
    }

    #[test]
    fn lsp_cache_is_revision_keyed() {
        let revision = Arc::new(Mutex::new("rev-1".to_string()));
        let lsp = Arc::new(CountingLsp::default());
        let fallback = GenericSemanticFallback::default()
            .with_index(Arc::new(MutableIndex {
                revision: revision.clone(),
            }))
            .with_lsp(lsp.clone());
        let first = block_on_timed(fallback.affected(affected_request())).unwrap();
        assert!(!first.payload.affected.is_empty());
        assert_eq!(lsp.references.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Same revision: served from the revision-keyed cache.
        let second = block_on_timed(fallback.affected(affected_request())).unwrap();
        assert_eq!(lsp.references.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(first.payload.affected, second.payload.affected);
        // A new revision must NOT reuse the old entry.
        *revision
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = "rev-2".to_string();
        let third = block_on_timed(fallback.affected(affected_request())).unwrap();
        assert_eq!(lsp.references.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(first.payload.affected, third.payload.affected);
    }

    #[test]
    fn affected_lsp_budget_exhaustion_degrades_without_losing_graph_results() {
        let slow = Arc::new(SlowLsp::default());
        let fallback = GenericSemanticFallback::default()
            .with_impact(Arc::new(FakeImpact))
            .with_lsp(slow.clone())
            .with_affected_lsp_budget(Duration::from_millis(20))
            .with_affected_lsp_entity_cap(8);
        let workspace = WorkspaceId::new(1);
        let request = AffectedRequest {
            call: call(),
            workspace,
            snapshot_id: snapshot(workspace, "rev-1"),
            changed: vec![
                entity("src/a.rs", "a"),
                entity("src/b.rs", "b"),
                entity("src/c.rs", "c"),
            ],
            max_depth: 2,
        };
        let envelope = block_on_timed(fallback.affected(request)).unwrap();
        assert!(
            envelope
                .payload
                .affected
                .iter()
                .any(|entity| entity.entity_id.as_str() == "caller"),
            "graph results survive a starved LSP phase"
        );
        assert!(envelope
            .payload
            .tests
            .iter()
            .any(|entity| entity.entity_id.as_str() == "checks"));
        assert!(envelope.payload.degraded);
        let reason = envelope
            .payload
            .unavailable
            .iter()
            .find(|entry| entry.subsystem == SemanticSubsystem::Lsp)
            .map(|entry| entry.reason.clone())
            .expect("budget exhaustion is a typed LSP unavailable reason");
        assert!(reason.contains("bounded"), "unexpected reason: {reason}");
        assert_eq!(
            slow.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the budget stops after the first entity, never one query per entity"
        );
        // An immediate repeat query is served from graph/index during the
        // cooldown: it never re-waits on the same starved server.
        let repeat = block_on_timed(fallback.affected(AffectedRequest {
            call: call(),
            workspace,
            snapshot_id: snapshot(workspace, "rev-1"),
            changed: vec![entity("src/a.rs", "a")],
            max_depth: 2,
        }))
        .unwrap();
        assert_eq!(
            slow.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the cooldown prevents a per-query LSP retry storm"
        );
        assert!(repeat.payload.degraded);
        assert!(repeat
            .payload
            .affected
            .iter()
            .any(|entity| entity.entity_id.as_str() == "caller"));
    }

    #[test]
    fn affected_lsp_entity_cap_is_typed_and_limits_enrichment() {
        let lsp = Arc::new(CountingLsp::default());
        let fallback = GenericSemanticFallback::default()
            .with_impact(Arc::new(FakeImpact))
            .with_lsp(lsp.clone())
            .with_affected_lsp_budget(Duration::from_secs(5))
            .with_affected_lsp_entity_cap(1);
        let workspace = WorkspaceId::new(1);
        let request = AffectedRequest {
            call: call(),
            workspace,
            snapshot_id: snapshot(workspace, "rev-1"),
            changed: vec![entity("src/a.rs", "a"), entity("src/b.rs", "b")],
            max_depth: 1,
        };
        let envelope = block_on_timed(fallback.affected(request)).unwrap();
        assert_eq!(lsp.references.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(envelope.payload.degraded);
        assert!(envelope
            .payload
            .unavailable
            .iter()
            .any(
                |entry| entry.subsystem == SemanticSubsystem::Lsp && entry.reason.contains("cap 1")
            ));
        assert!(envelope
            .payload
            .affected
            .iter()
            .any(|entity| entity.entity_id.as_str() == "caller"));
    }

    #[test]
    fn cancellation_is_typed_and_precedes_any_seam_call() {
        let lsp = Arc::new(CountingLsp::default());
        let fallback = GenericSemanticFallback::default().with_lsp(lsp.clone());
        let request = context_request();
        request.call.cancellation.cancel();
        let error = block_on(fallback.context(request)).unwrap_err();
        assert!(matches!(error, SemanticError::Cancelled { .. }));
        assert_eq!(
            lsp.symbols.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a cancelled call must never reach a subsystem"
        );
    }

    #[test]
    fn wired_backend_report_is_never_the_empty_default() {
        let report = GenericSemanticFallback::default()
            .with_index(Arc::new(TestIndex {
                revision: "rev-1".to_string(),
                items: Vec::new(),
            }))
            .with_lsp(Arc::new(CountingLsp::default()))
            .wired_backends();
        assert_eq!(report.index.as_deref(), Some("index"));
        assert_eq!(report.lsp.as_deref(), Some("lsp:test"));
        assert!(report.is_wired());
        assert_ne!(report, GenericSemanticFallback::default().wired_backends());
    }

    #[test]
    fn edit_request_carries_intent_and_identity() {
        let request = SemanticEditRequest {
            call: call(),
            workspace: WorkspaceId::new(1),
            snapshot_id: snapshot(WorkspaceId::new(1), "rev-1"),
            entity: entity("src/lib.rs", "lib"),
            intent: "rename".to_string(),
        };
        assert_eq!(request.intent, "rename");
        assert_eq!(request.workspace, WorkspaceId::new(1));
        assert_eq!(request.entity.entity_id.as_str(), "lib");
    }
}
