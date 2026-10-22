//! Native deterministic engineering-contract compilation from user intent +
//! repository state (audit item 5).
//!
//! Before implementation the runtime compiles a BASELINE
//! [`EngineeringContract`] for the task from two pure inputs — the user's
//! goal text and a bounded [`RepositorySnapshot`] gathered by the caller —
//! using native deterministic intelligence: manifest / build-system
//! detection, existing test-selector discovery, a text-level touched-symbol
//! graph, public API-surface detection, migration detection and changed-code
//! scale. The function is PURE: the same request always yields a
//! byte-identical contract (the caller provides all file bytes; nothing here
//! touches the filesystem, a clock or a random source), so a replayed task
//! start unions to a no-op and no claim silently drifts between runs.
//!
//! Two laws from [`crate::contract`] shape the output:
//!
//! 1. **Every generated statement carries provenance.** For each constraint,
//!    invariant, risk and proof obligation the compiler records a
//!    [`StatementProvenance`] with its origin
//!    (`source_analysis | repo_analysis | heuristic | external:<provider>`),
//!    a bounded confidence in basis points and the concrete BASIS (the
//!    manifest path, test selector, migration artifact, matched file, ...)
//!    that justifies it. The IR's own `origin` field maps onto the neutral
//!    [`ContractOrigin`] space; the compiler-level provenance preserves the
//!    finer distinction.
//!
//! 2. **No fabrication.** When no deterministic obligation can be derived the
//!    compiler does NOT emit an empty or invented contract: it records an
//!    explicit [`ContractUncertainty`] (also encoded as a durable risk
//!    hypothesis so it survives in the task row and is disclosed). Compiled
//!    proof obligations always carry a BOUND [`EvidenceBinding`] naming the
//!    exact evidence that may discharge them — the legacy
//!    [`EvidenceBinding::Category`] is never emitted.
//!
//! A [`SystemIntelligenceProvider`] (the neutral seam named by
//! `docs/specs/autonomous-engineering.md`) may be asked the fixed bounded
//! engineering QUESTION set ([`IntelligenceQuestion`]) BEFORE the pure
//! compile. The runtime performs that consult asynchronously (the provider
//! is an external service), binds every ask to an explicit operation
//! identity, and feeds the bounded [`ProviderFinding`] answers into
//! [`compile_contract_with_findings_checked`]: enrichment is strictly
//! ADDITIVE, every enriched statement carries `external:<provider>`
//! provenance, and a missing, refusing, timing-out or empty provider leaves
//! the native contract byte-identical (Tangerine is never required). The
//! checked entry enforces the `organization_contracts` value entitlement
//! before the apply; the raw apply stays private. Provider output is
//! validated DATA: findings bound to another snapshot/revision or another
//! provider, oversized findings, unknown kinds and malformed payloads are
//! dropped with typed [`FindingDiagnostic`]s and can never fail a turn.
//!
//! Statement ids replicate [`faktor_session`](../../session)'s user-authored
//! id scheme (`faktor.contract.{c1,i1,r1}.v1` + text, BLAKE3, 16 hex chars)
//! so the durable union merge dedupes a compiled statement against an
//! identical user-authored one instead of duplicating it. Proof obligation
//! ids come from [`EngineeringContract::obligation_id`].

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;

use crate::contract::{
    AcceptedStrength, Constraint, ContractOrigin, EngineeringContract, EvidenceBinding, Invariant,
    ProofEvidenceKind, Requirement, RiskHypothesis, RiskSeverity,
};
use crate::{CancellationToken, Deadline, OpId, SessionId, WorkspaceId};

/// Hard bound on repository files one compile considers (paths beyond it are
/// dropped and the snapshot is reported incomplete by the CALLER; the pure
/// compiler additionally refuses to look past this).
pub const MAX_COMPILE_FILES: usize = 20_000;
/// Hard bound on ONE accepted file content (bytes). Larger payloads are
/// ignored for content analysis (paths still count).
pub const MAX_FILE_CONTENT_BYTES: usize = 1 << 20;
/// Hard bound on the total content the compiler will process (bytes).
pub const MAX_TOTAL_CONTENT_BYTES: usize = 16 << 20;
/// Longest intent token kept (bytes).
pub const MAX_INTENT_TOKEN_BYTES: usize = 48;
/// Hard bound on intent tokens considered.
pub const MAX_INTENT_TOKENS: usize = 16;
/// Longest generated statement text (the session validator bounds contract
/// strings at 512 bytes; stay well under).
pub const MAX_STATEMENT_BYTES: usize = 400;
/// Longest accepted test selector.
pub const MAX_SELECTOR_BYTES: usize = 200;
/// Longest accepted repository-relative path.
pub const MAX_PATH_BYTES: usize = 1024;
/// Hard bound on matched (intent-relevant) files kept.
pub const MAX_MATCHED_FILES: usize = 256;
/// Hard bound on the touched-symbol graph entries kept.
pub const MAX_TOUCHED_SYMBOLS: usize = 128;
/// Hard bound on discovered test selectors kept per compile.
pub const MAX_DISCOVERED_SELECTORS: usize = 512;
/// Hard bound on compiled test proof obligations.
pub const MAX_TEST_OBLIGATIONS: usize = 24;
/// Hard bound on compiled compile-check obligations.
pub const MAX_COMPILE_OBLIGATIONS: usize = 8;
/// Hard bound on compiled constraints.
pub const MAX_CONSTRAINTS: usize = 16;
/// Hard bound on compiled invariants.
pub const MAX_INVARIANTS: usize = 16;
/// Hard bound on compiled risk hypotheses (including uncertainty rows).
pub const MAX_RISKS: usize = 16;
/// Hard bound on recorded uncertainties.
pub const MAX_UNCERTAINTIES: usize = 16;
/// Hard bound on enrichment statements applied per compile.
pub const MAX_ENRICHMENT_STATEMENTS: usize = 24;
/// Hard bound on provider findings asked for/accepted in one consult.
pub const MAX_FINDINGS: usize = MAX_ENRICHMENT_STATEMENTS;
/// Hard bound on ONE finding's provider-supplied text (UTF-8 bytes). Kept
/// below [`MAX_STATEMENT_BYTES`] so the compiler's typed `"<question>: "`
/// prefix can never push an accepted statement past the statement bound.
pub const MAX_FINDING_TEXT_BYTES: usize = 320;
/// Hard bound on one finding's provider-supplied basis (UTF-8 bytes).
pub const MAX_FINDING_BASIS_BYTES: usize = MAX_STATEMENT_BYTES;
/// Hard bound on one finding's revision label (UTF-8 bytes).
pub const MAX_FINDING_REVISION_BYTES: usize = 128;
/// Hard bound on one typed finding diagnostic's detail.
pub const MAX_FINDING_DIAGNOSTIC_BYTES: usize = 200;
/// Hard bound on typed diagnostics kept per finding batch.
pub const MAX_FINDING_DIAGNOSTICS: usize = 8;
/// Longest enrichment diagnostic kept.
const MAX_DIAGNOSTIC_BYTES: usize = 200;
/// Longest goal excerpt embedded in a neutral question query.
const MAX_QUESTION_GOAL_BYTES: usize = 160;
/// Lines scanned per file for test attributes / symbols.
const MAX_SCAN_LINES: usize = 20_000;

/// Words that never carry subsystem identity: dropped from intent tokens so
/// "Fix the refresh-token race" yields `[race, refresh, token]`.
const INTENT_STOPWORDS: &[&str] = &[
    "about",
    "add",
    "after",
    "again",
    "against",
    "all",
    "also",
    "and",
    "any",
    "are",
    "before",
    "being",
    "bug",
    "bugs",
    "can",
    "change",
    "changes",
    "could",
    "does",
    "ensure",
    "error",
    "errors",
    "fix",
    "fixed",
    "fixes",
    "fixing",
    "for",
    "from",
    "has",
    "have",
    "implement",
    "improve",
    "into",
    "issue",
    "its",
    "just",
    "make",
    "makes",
    "more",
    "new",
    "not",
    "old",
    "only",
    "other",
    "our",
    "out",
    "over",
    "problem",
    "should",
    "some",
    "support",
    "than",
    "that",
    "the",
    "their",
    "them",
    "then",
    "there",
    "these",
    "this",
    "those",
    "update",
    "updates",
    "use",
    "using",
    "via",
    "was",
    "were",
    "when",
    "where",
    "which",
    "while",
    "with",
    "without",
    "work",
    "working",
    "would",
    "your",
];

/// Tokens that ground a compatibility constraint (only with a matched public
/// API surface).
const COMPAT_TOKENS: &[&str] = &[
    "api",
    "backward",
    "breaking",
    "compat",
    "compatibility",
    "compatible",
    "public",
    "refactor",
    "rename",
    "upgrade",
];

/// Tokens that ground migration invariants/risks (only with detected
/// migration artifacts).
const MIGRATION_TOKENS: &[&str] = &["migrate", "migration", "migrations", "schema", "upgrade"];

/// Tokens that ground a concurrency/replay risk over the matched surface.
const RACE_TOKENS: &[&str] = &[
    "atomic",
    "atomicity",
    "concurrency",
    "concurrent",
    "deadlock",
    "idempotent",
    "idempotency",
    "race",
    "races",
    "replay",
    "replayed",
    "toctou",
];

/// One repository file fact: the repo-relative path (always) plus bounded
/// content for the files the caller chose to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryFile {
    pub path: String,
    pub content: Option<String>,
}

impl RepositoryFile {
    pub fn path_only(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            content: None,
        }
    }

    pub fn with_content(path: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            content: Some(content.into()),
        }
    }
}

/// The typed kind of one repository-acquisition gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquisitionGapKind {
    /// A directory in the bounded walk could not be listed; its paths (and
    /// anything under them) are unknown.
    UnreadableDirectory,
    /// A directory entry (or its metadata) could not be inspected.
    UnreadableEntry,
    /// A selected file's bytes could not be read.
    UnreadableFile,
    /// A selected file's bytes were not valid UTF-8 text.
    InvalidUtf8,
    /// A selected file exceeded the per-file content bound.
    OversizedFile,
    /// The content budget was exhausted before a selected file was read.
    ReadBudgetExhausted,
    /// The bounded walk stopped at a cap; unvisited paths may exist.
    WalkTruncated,
}

impl AcquisitionGapKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnreadableDirectory => "unreadable_directory",
            Self::UnreadableEntry => "unreadable_entry",
            Self::UnreadableFile => "unreadable_file",
            Self::InvalidUtf8 => "invalid_utf8",
            Self::OversizedFile => "oversized_file",
            Self::ReadBudgetExhausted => "read_budget_exhausted",
            Self::WalkTruncated => "walk_truncated",
        }
    }
}

/// One concrete, typed reason a snapshot is not
/// [`RepositoryAcquisition::Complete`]: WHAT could not be acquired and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquisitionGap {
    pub kind: AcquisitionGapKind,
    /// The repo-relative path (or directory) the gap concerns; `None` for
    /// whole-walk findings (caps, totals).
    pub path: Option<String>,
    /// The concrete cause, including the underlying error when one exists.
    pub detail: String,
    /// TRUE when the missing content could have carried proof-relevant
    /// evidence (a build manifest, an intent-matched source, a test file):
    /// the compiler then discloses this gap as its own uncertainty instead of
    /// burying it in the aggregate.
    pub proof_relevant: bool,
}

impl AcquisitionGap {
    /// Deterministic single-line rendering used as an uncertainty basis.
    pub fn render(&self) -> String {
        match &self.path {
            Some(path) => format!("{} {path}: {}", self.kind.as_str(), self.detail),
            None => format!("{}: {}", self.kind.as_str(), self.detail),
        }
    }
}

/// Truthful acquisition state of a [`RepositorySnapshot`]: what the caller
/// could actually READ, never conflated with what it found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepositoryAcquisition {
    /// The caller read every path it needed to inspect. An empty file list
    /// here is a GENUINELY empty (readable) repository.
    Complete,
    /// Part of the surface could not be acquired; `reasons` carries the typed
    /// per-condition explanations. Missing paths may hide evidence.
    Partial { reasons: Vec<AcquisitionGap> },
    /// The repository surface could not be acquired at all; `reason` names
    /// the typed cause. NEVER equivalent to an empty repository.
    Unavailable { reason: String },
}

impl RepositoryAcquisition {
    /// Stable label for diagnostics/telemetry.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial { .. } => "partial",
            Self::Unavailable { .. } => "unavailable",
        }
    }

    /// TRUE only for a genuinely complete acquisition.
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Complete)
    }

    /// TRUE when nothing of the repository surface could be acquired.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable { .. })
    }

    /// The typed gaps of a [`Self::Partial`] acquisition (empty otherwise).
    pub fn reasons(&self) -> &[AcquisitionGap] {
        match self {
            Self::Partial { reasons } => reasons,
            _ => &[],
        }
    }
}

/// A bounded snapshot of repository state the compiler reasons over.
/// `acquisition` is the caller's truthful acquisition verdict: a `Partial`
/// or `Unavailable` state makes the compiler record explicit uncertainties
/// instead of silently (or claiming to) analyzing a tree it could not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositorySnapshot {
    pub files: Vec<RepositoryFile>,
    pub acquisition: RepositoryAcquisition,
}

impl Default for RepositorySnapshot {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            acquisition: RepositoryAcquisition::Complete,
        }
    }
}

impl RepositorySnapshot {
    pub fn from_files(files: Vec<RepositoryFile>) -> Self {
        Self {
            files,
            acquisition: RepositoryAcquisition::Complete,
        }
    }

    pub fn from_paths(paths: impl IntoIterator<Item = String>) -> Self {
        Self::from_files(paths.into_iter().map(RepositoryFile::path_only).collect())
    }
}

/// The compiled statements' kinds. The first four are the contract IR's own
/// vector kinds; the last three preserve the finer typed kind of an
/// external finding whose disclosure is applied through the risk vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementKind {
    Constraint,
    Invariant,
    Risk,
    ProofObligation,
    /// An affected-component finding.
    Component,
    /// A hidden-dependency finding.
    Dependency,
    /// A historical failure/migration finding.
    HistoricalFailure,
}

impl StatementKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Constraint => "constraint",
            Self::Invariant => "invariant",
            Self::Risk => "risk",
            Self::ProofObligation => "proof_obligation",
            Self::Component => "component",
            Self::Dependency => "dependency",
            Self::HistoricalFailure => "historical_failure",
        }
    }
}

/// Where a compiled statement came from, at compiler granularity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatementOrigin {
    /// Derived from the user's stated intent (goal text).
    SourceAnalysis,
    /// Derived from repository facts (manifests, tests, symbols, migrations).
    RepoAnalysis,
    /// A heuristic without repository grounding.
    Heuristic,
    /// An external intelligence provider's statement.
    External { provider: String },
}

impl StatementOrigin {
    pub fn as_str(&self) -> String {
        match self {
            Self::SourceAnalysis => "source_analysis".to_string(),
            Self::RepoAnalysis => "repo_analysis".to_string(),
            Self::Heuristic => "heuristic".to_string(),
            Self::External { provider } => format!("external:{provider}"),
        }
    }

    fn contract_origin(&self) -> ContractOrigin {
        match self {
            Self::SourceAnalysis | Self::RepoAnalysis => ContractOrigin::Native,
            Self::Heuristic => ContractOrigin::Heuristic,
            Self::External { provider } => ContractOrigin::External {
                provider: provider.clone(),
            },
        }
    }
}

/// Provenance of one generated statement: origin, confidence and the concrete
/// basis that justifies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementProvenance {
    pub statement_id: String,
    pub kind: StatementKind,
    pub origin: StatementOrigin,
    pub confidence_bps: u16,
    pub basis: String,
}

/// An explicit uncertainty: what could NOT be determined and on what basis.
/// Also encoded as a durable risk hypothesis in the compiled contract so the
/// disclosure survives in the task row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractUncertainty {
    pub id: String,
    pub statement: String,
    pub origin: StatementOrigin,
    pub confidence_bps: u16,
    pub basis: String,
}

/// What one enrichment attempt did (diagnostics only; never part of the
/// contract).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrichmentOutcome {
    pub provider: String,
    pub applied: usize,
    pub skipped: usize,
    pub diagnostic: Option<String>,
}

/// The full compile report: the contract and its provenance/uncertainty
/// evidence. The durable task row stores [`Self::contract`]; the rest is
/// disclosure for the caller (logs, doctor, tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledContract {
    pub contract: EngineeringContract,
    pub provenance: Vec<StatementProvenance>,
    pub uncertainties: Vec<ContractUncertainty>,
    /// The normalized intent tokens the compile matched on.
    pub intent_tokens: Vec<String>,
    /// Detected build systems (sorted, stable names: `cargo`, `node`, ...).
    pub build_systems: Vec<String>,
    /// Matched (intent-relevant) repository paths.
    pub touched_files: Vec<String>,
    /// The text-level touched-symbol graph (`fn foo in src/x.rs`, ...).
    pub touched_symbols: Vec<String>,
    /// Content identity of the snapshot the contract was compiled against
    /// (files AND truthful acquisition state).
    pub repository_digest: String,
    /// The snapshot's truthful acquisition state; `Partial`/`Unavailable`
    /// contracts carry explicit uncertainties and never claim full analysis.
    pub repository_acquisition: RepositoryAcquisition,
    /// Per-provider enrichment outcomes (empty for native-only compiles).
    pub enrichments: Vec<EnrichmentOutcome>,
}

impl CompiledContract {
    /// TRUE when the compile recorded at least one explicit uncertainty.
    pub fn has_uncertainty(&self) -> bool {
        !self.uncertainties.is_empty()
    }
}

/// One compile request: the user's goal plus the bounded repository snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractCompileRequest {
    pub goal: String,
    pub repository: RepositorySnapshot,
}

/// One bounded engineering question the runtime asks a
/// [`SystemIntelligenceProvider`] before the pure compile
/// (`docs/specs/autonomous-engineering.md` § "System intelligence").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum IntelligenceQuestion {
    /// Which components/architecture does the objective actually touch?
    AffectedComponents,
    /// Which system invariants must survive the change?
    Invariants,
    /// Which hidden dependencies connect the change surface to the rest?
    HiddenDependencies,
    /// How far does the change propagate (blast radius)?
    BlastRadius,
    /// Which historical failures/migrations are relevant to this objective?
    HistoricalFailures,
}

impl IntelligenceQuestion {
    /// The fixed, bounded question set: every consult asks exactly these.
    pub const ALL: [IntelligenceQuestion; 5] = [
        IntelligenceQuestion::AffectedComponents,
        IntelligenceQuestion::Invariants,
        IntelligenceQuestion::HiddenDependencies,
        IntelligenceQuestion::BlastRadius,
        IntelligenceQuestion::HistoricalFailures,
    ];

    /// Stable wire/diagnostic name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AffectedComponents => "affected_components",
            Self::Invariants => "invariants",
            Self::HiddenDependencies => "hidden_dependencies",
            Self::BlastRadius => "blast_radius",
            Self::HistoricalFailures => "historical_failures",
        }
    }

    /// Short human-readable label used as the statement prefix.
    pub const fn label(self) -> &'static str {
        match self {
            Self::AffectedComponents => "affected component",
            Self::Invariants => "invariant that must survive",
            Self::HiddenDependencies => "hidden dependency",
            Self::BlastRadius => "blast radius",
            Self::HistoricalFailures => "historical failure",
        }
    }

    /// The finding kind an answer to this question must carry.
    pub const fn expected_kind(self) -> FindingKind {
        match self {
            Self::AffectedComponents => FindingKind::Component,
            Self::Invariants => FindingKind::Invariant,
            Self::HiddenDependencies => FindingKind::Dependency,
            Self::BlastRadius => FindingKind::Risk,
            Self::HistoricalFailures => FindingKind::HistoricalFailure,
        }
    }

    /// The bounded neutral question text a retrieval-style provider is
    /// asked. Deterministic: the same goal renders byte-identically.
    pub fn query(self, goal: &str) -> String {
        let goal = bounded(goal, MAX_QUESTION_GOAL_BYTES);
        match self {
            Self::AffectedComponents => format!(
                "objective: {goal}; question affected_components: which components and architecture does this objective directly affect?"
            ),
            Self::Invariants => format!(
                "objective: {goal}; question invariants: which system invariants must survive this change?"
            ),
            Self::HiddenDependencies => format!(
                "objective: {goal}; question hidden_dependencies: which hidden dependencies connect this change to the rest of the system?"
            ),
            Self::BlastRadius => format!(
                "objective: {goal}; question blast_radius: how far does this change propagate and what can it break?"
            ),
            Self::HistoricalFailures => format!(
                "objective: {goal}; question historical_failures: which historical failures or migrations are relevant to this objective?"
            ),
        }
    }
}

/// The typed kind of one provider finding answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FindingKind {
    /// An affected component / architecture surface.
    Component,
    /// A system invariant that must survive.
    Invariant,
    /// A hidden dependency.
    Dependency,
    /// A blast-radius risk hypothesis.
    Risk,
    /// A relevant historical failure/migration.
    HistoricalFailure,
}

impl FindingKind {
    /// Stable wire/diagnostic name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Component => "component",
            Self::Invariant => "invariant",
            Self::Dependency => "dependency",
            Self::Risk => "risk",
            Self::HistoricalFailure => "historical_failure",
        }
    }
}

/// One bounded finding a provider returns: DATA, never a prescription. The
/// runtime validates provider identity, snapshot/revision binding, kind,
/// confidence and bounds before it can ever reach the contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderFinding {
    /// The question this finding answers.
    pub question: IntelligenceQuestion,
    /// The answer kind (must be [`IntelligenceQuestion::expected_kind`]).
    pub kind: FindingKind,
    /// The bounded finding text.
    pub text: String,
    /// The concrete basis that justifies it.
    pub basis: String,
    /// Confidence in basis points.
    pub confidence_bps: u16,
    /// Identity of the repository snapshot the finding was derived from: it
    /// must equal the consult's [`FindingsRequest::repository_digest`].
    pub repository_digest: String,
    /// The provider that stands behind the claim: it must equal the id of
    /// the provider the runtime asked.
    pub provider: String,
    /// The provider's source revision label, when it declares one
    /// (diagnostics only; the DIGEST above is the binding).
    pub revision: Option<String>,
    /// Risk severity for risk-shaped findings (defaults at the compiler).
    pub severity: Option<RiskSeverity>,
}

/// Why one prospective finding was refused. Every variant is a stable typed
/// code; the compiler records them as diagnostics and drops the finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindingRejection {
    /// The finding names a provider other than the one that was asked.
    ProviderMismatch { expected: String, actual: String },
    /// The finding is bound to another snapshot/revision than the consult.
    DigestMismatch { expected: String, actual: String },
    /// The finding kind does not answer the question it claims.
    KindMismatch {
        question: IntelligenceQuestion,
        kind: FindingKind,
    },
    /// The finding's text is empty.
    EmptyText,
    /// The finding's text exceeds [`MAX_FINDING_TEXT_BYTES`].
    TextTooLong { bytes: usize, max: usize },
    /// The finding's basis is empty.
    EmptyBasis,
    /// The finding's basis exceeds [`MAX_FINDING_BASIS_BYTES`].
    BasisTooLong { bytes: usize, max: usize },
    /// The confidence is outside `0..=10_000` bps.
    ConfidenceOutOfRange { bps: u16 },
    /// The revision label exceeds [`MAX_FINDING_REVISION_BYTES`].
    RevisionTooLong { bytes: usize, max: usize },
}

impl FindingRejection {
    /// Stable typed diagnostic code.
    pub const fn code(&self) -> FindingDiagnosticCode {
        match self {
            Self::ProviderMismatch { .. } => FindingDiagnosticCode::ProviderMismatch,
            Self::DigestMismatch { .. } => FindingDiagnosticCode::SnapshotMismatch,
            Self::KindMismatch { .. } => FindingDiagnosticCode::InvalidFinding,
            Self::EmptyText | Self::EmptyBasis => FindingDiagnosticCode::InvalidFinding,
            Self::TextTooLong { .. } | Self::BasisTooLong { .. } | Self::RevisionTooLong { .. } => {
                FindingDiagnosticCode::Oversized
            }
            Self::ConfidenceOutOfRange { .. } => FindingDiagnosticCode::InvalidFinding,
        }
    }

    /// Bounded human-readable detail.
    pub fn detail(&self) -> String {
        bounded(
            &match self {
                Self::ProviderMismatch { expected, actual } => {
                    format!("finding claims provider {actual:?}, expected {expected:?}")
                }
                Self::DigestMismatch { expected, actual } => {
                    format!("finding is bound to snapshot {actual:?}, expected {expected:?}")
                }
                Self::KindMismatch { question, kind } => format!(
                    "question {} cannot be answered by a {} finding",
                    question.as_str(),
                    kind.as_str()
                ),
                Self::EmptyText => "finding text is empty".to_string(),
                Self::TextTooLong { bytes, max } => {
                    format!("finding text of {bytes} bytes exceeds the {max}-byte bound")
                }
                Self::EmptyBasis => "finding basis is empty".to_string(),
                Self::BasisTooLong { bytes, max } => {
                    format!("finding basis of {bytes} bytes exceeds the {max}-byte bound")
                }
                Self::ConfidenceOutOfRange { bps } => {
                    format!("finding confidence {bps} bps is outside 0..=10000")
                }
                Self::RevisionTooLong { bytes, max } => {
                    format!("finding revision of {bytes} bytes exceeds the {max}-byte bound")
                }
            },
            MAX_FINDING_DIAGNOSTIC_BYTES,
        )
    }
}

/// The typed class of one finding diagnostic (stable codes: tests and
/// operators branch on these, never on prose).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FindingDiagnosticCode {
    /// The whole consult was refused by the provider.
    ProviderRefused,
    /// The consult exceeded the runtime's wall bound.
    ProviderTimedOut,
    /// The finding names another provider than the one asked.
    ProviderMismatch,
    /// The finding is bound to another snapshot/revision.
    SnapshotMismatch,
    /// The provider response carried invalid provenance/workspace identity.
    InvalidProvenance,
    /// No workspace content revision could be established for the questions.
    RevisionUnavailable,
    /// A bounded payload was exceeded.
    Oversized,
    /// The finding is otherwise malformed.
    InvalidFinding,
    /// No provider could serve the consult (absent/cooling/incompatible).
    ProviderUnavailable,
    /// The statement/entry cap was reached.
    CapReached,
}

impl FindingDiagnosticCode {
    /// Stable code string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderRefused => "provider_refused",
            Self::ProviderTimedOut => "provider_timed_out",
            Self::ProviderMismatch => "provider_mismatch",
            Self::SnapshotMismatch => "snapshot_mismatch",
            Self::InvalidProvenance => "invalid_provenance",
            Self::RevisionUnavailable => "revision_unavailable",
            Self::Oversized => "oversized",
            Self::InvalidFinding => "invalid_finding",
            Self::ProviderUnavailable => "provider_unavailable",
            Self::CapReached => "cap_reached",
        }
    }
}

/// One typed finding diagnostic: what was dropped and why. Diagnostics are
/// disclosure only — they never alter the native contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindingDiagnostic {
    pub provider: String,
    pub code: FindingDiagnosticCode,
    pub detail: String,
}

impl FindingDiagnostic {
    pub fn new(
        provider: impl Into<String>,
        code: FindingDiagnosticCode,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            code,
            detail: bounded(&detail.into(), MAX_FINDING_DIAGNOSTIC_BYTES),
        }
    }
}

/// The bounded answer to one findings consult: findings plus the typed
/// diagnostics of everything dropped along the way.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FindingsResponse {
    pub findings: Vec<ProviderFinding>,
    pub diagnostics: Vec<FindingDiagnostic>,
}

/// The explicit async-operation envelope of one findings consult
/// (commandment 2: `operation_id, session_id, started_ms, deadline,
/// cancellation_token` are carried, never implicit), plus the neutral
/// engineering inputs, never a prescription of the answer.
#[derive(Debug, Clone)]
pub struct FindingsRequest<'a> {
    pub operation_id: OpId,
    pub session_id: SessionId,
    pub workspace: WorkspaceId,
    pub started_ms: i64,
    pub deadline: Option<Deadline>,
    pub cancellation: CancellationToken,
    /// The workspace content revision the questions are bound to, when the
    /// runtime could establish one. `None` means no content identity exists:
    /// a provider must not answer (identity law — no synthetic revision).
    pub source_revision: Option<&'a str>,
    pub goal: &'a str,
    pub intent_tokens: &'a [String],
    pub build_systems: &'a [String],
    pub repository_digest: &'a str,
    pub native_contract: &'a EngineeringContract,
    /// The fixed question set; always [`IntelligenceQuestion::ALL`].
    pub questions: &'a [IntelligenceQuestion],
    /// Hard bound on findings the provider may return.
    pub max_findings: usize,
}

/// Boxed future returned by the findings seam. Standard library only: this
/// crate carries no async runtime.
pub type IntelligenceFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The neutral system-intelligence seam (`docs/specs/autonomous-engineering.md`).
/// The runtime ASKS bounded questions before the pure compile; the provider
/// never touches the compiler and is never required: an absent provider, a
/// refusal or a timeout leaves the native contract byte-identical.
pub trait SystemIntelligenceProvider: Send + Sync {
    /// Stable provider id used as the `external:<provider>` provenance tag.
    fn provider_id(&self) -> &str;

    /// Answer the fixed bounded question set. `Err` is diagnosed but never
    /// alters the contract. The default answers nothing.
    fn findings<'a>(
        &'a self,
        request: &'a FindingsRequest<'a>,
    ) -> IntelligenceFuture<'a, Result<FindingsResponse, String>> {
        let _ = request;
        Box::pin(async { Ok(FindingsResponse::default()) })
    }
}

/// What an enrichment provider may add. Obligations MUST carry a bound
/// (non-[`EvidenceBinding::Category`]) binding; hostile or malformed
/// statements are skipped and counted, never emitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnrichmentKind {
    Constraint,
    Invariant,
    Risk,
    ProofObligation,
    /// An affected-component finding (applied as an external risk
    /// disclosure unless the provider supplied a stronger kind).
    Component,
    /// A hidden-dependency finding.
    Dependency,
    /// A historical failure/migration finding.
    HistoricalFailure,
}

/// One enrichment statement proposed by an external provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrichmentStatement {
    pub kind: EnrichmentKind,
    pub text: String,
    pub confidence_bps: u16,
    pub basis: String,
    /// Proof obligations: the evidence family that may discharge the claim.
    pub evidence: Option<ProofEvidenceKind>,
    /// Proof obligations: exactly what may discharge the claim.
    pub binding: Option<EvidenceBinding>,
    /// Risks: severity (defaults to Medium when absent).
    pub severity: Option<RiskSeverity>,
    /// Proof obligations: TRUE for a disclosure-only (Advisory) claim.
    pub advisory: bool,
}

/// Normalize a goal into deterministic intent tokens: lowercase ASCII
/// alphanumeric words, 3..=[`MAX_INTENT_TOKEN_BYTES`] bytes, stopwords
/// dropped, deduplicated, sorted, capped at [`MAX_INTENT_TOKENS`].
pub fn intent_tokens(goal: &str) -> Vec<String> {
    let mut tokens: BTreeSet<String> = BTreeSet::new();
    for raw in goal.split(|c: char| !c.is_ascii_alphanumeric()) {
        let token = raw.to_ascii_lowercase();
        if token.len() < 3 || token.len() > MAX_INTENT_TOKEN_BYTES {
            continue;
        }
        if INTENT_STOPWORDS.contains(&token.as_str()) {
            continue;
        }
        tokens.insert(token);
        if tokens.len() >= MAX_INTENT_TOKENS {
            break;
        }
    }
    tokens.into_iter().collect()
}

/// Compile the native baseline contract (no external findings).
pub fn compile_contract(request: &ContractCompileRequest) -> CompiledContract {
    let intent = intent_tokens(&request.goal);
    let files = normalize_files(&request.repository.files);
    let build_systems = detect_build_systems(&files);
    let selectors = discover_test_selectors(&files);
    NativeCompile::run(request, &intent, &files, &build_systems, &selectors)
}

/// Compile the native baseline contract and apply the bounded findings the
/// runtime already asked the provider for. The function stays PURE: findings
/// are DATA, every one is re-validated against `request`'s own identity
/// (defense in depth), and an empty/malformed/refusing answer leaves the
/// native contract byte-identical to [`compile_contract`].
///
/// PRIVATE on purpose: this is the raw apply path. External callers reach it
/// ONLY through [`compile_contract_with_findings_checked`], which enforces
/// the `organization_contracts` value entitlement first — an embedder must
/// not be able to bypass the org gate by calling the pure apply directly.
/// In-crate unit tests exercise the raw path directly.
fn compile_contract_with_findings(
    request: &ContractCompileRequest,
    provider_id: &str,
    response: &FindingsResponse,
) -> CompiledContract {
    let mut compiled = compile_contract(request);
    let Some(provider_id) = sanitized_provider_id(provider_id) else {
        compiled.enrichments.push(EnrichmentOutcome {
            provider: provider_id.to_string(),
            applied: 0,
            skipped: 0,
            diagnostic: Some("provider id is empty after sanitization; findings refused".into()),
        });
        return compiled;
    };
    let conversion = findings_to_statements(
        &provider_id,
        &compiled.repository_digest,
        MAX_ENRICHMENT_STATEMENTS,
        &response.findings,
    );
    let origin = StatementOrigin::External {
        provider: provider_id.clone(),
    };
    let mut outcome = EnrichmentOutcome {
        provider: provider_id,
        applied: 0,
        skipped: 0,
        diagnostic: None,
    };
    let mut skip_reasons: Vec<String> = Vec::new();
    for diagnostic in &response.diagnostics {
        push_skip_reason(&mut skip_reasons, diagnostic.code.as_str());
    }
    for diagnostic in &conversion.diagnostics {
        push_skip_reason(&mut skip_reasons, diagnostic.code.as_str());
    }
    outcome.skipped = conversion.rejected;
    for statement in conversion.statements {
        match apply_enrichment_statement(&mut compiled, &origin, statement) {
            Ok(()) => outcome.applied += 1,
            Err(reason) => {
                outcome.skipped += 1;
                push_skip_reason(&mut skip_reasons, &reason);
            }
        }
    }
    if !skip_reasons.is_empty() {
        outcome.diagnostic = Some(bounded(
            &format!("dropped findings: {}", skip_reasons.join("; ")),
            MAX_DIAGNOSTIC_BYTES,
        ));
    }
    compiled.enrichments.push(outcome);
    compiled
}

/// The ONLY public entry of the findings-enrichment path: the
/// `organization_contracts` value entitlement is an ENFORCED parameter, not
/// a convention. A refusal is the typed
/// [`crate::entitlement::ValueGateRefusal`] naming the entitlement, its
/// hook, the cause and (when resolved) the plan — the caller keeps the
/// native contract ([`compile_contract`] output) standing. With a granting
/// gate the output is byte-identical to the raw apply path.
pub fn compile_contract_with_findings_checked(
    request: &ContractCompileRequest,
    provider_id: &str,
    response: &FindingsResponse,
    gate: &crate::entitlement::ValueGate,
) -> Result<CompiledContract, crate::entitlement::ValueGateRefusal> {
    gate.check(crate::entitlement::ValueEntitlement::OrganizationContracts)?;
    Ok(compile_contract_with_findings(
        request,
        provider_id,
        response,
    ))
}

struct NativeCompile<'a> {
    request: &'a ContractCompileRequest,
    intent: &'a [String],
    files: Vec<&'a RepositoryFile>,
    build_systems: &'a [String],
    selectors: &'a [DiscoveredTest],
    contract: EngineeringContract,
    provenance: Vec<StatementProvenance>,
    uncertainties: Vec<ContractUncertainty>,
    touched_files: Vec<String>,
    touched_symbols: Vec<String>,
    total_lines: u64,
    /// Cargo component roots (`Cargo.toml` parent dirs) of the snapshot,
    /// built ONCE per compile: the touched-file loops used to rebuild this
    /// set inside every `cargo_component_root` call (O(touched × files)).
    cargo_roots: BTreeSet<&'a str>,
    /// Path -> index into `files`, built once (first occurrence wins):
    /// `compile_constraints` used a linear `files.iter().find` per touched
    /// path (O(touched × files)).
    file_index: std::collections::HashMap<&'a str, usize>,
}

// Test-only per-thread count of Cargo-root set builds per compile (evidence
// for the hoisting test): production builds carry no counter.
#[cfg(test)]
thread_local! {
    pub(crate) static CARGO_ROOT_SET_BUILDS: std::cell::Cell<u64> =
        const { std::cell::Cell::new(0) };
}

impl<'a> NativeCompile<'a> {
    fn run(
        request: &'a ContractCompileRequest,
        intent: &'a [String],
        files: &'a [&'a RepositoryFile],
        build_systems: &'a [String],
        selectors: &'a [DiscoveredTest],
    ) -> CompiledContract {
        let cargo_roots: BTreeSet<&str> = files
            .iter()
            .filter_map(|file| {
                file.path
                    .strip_suffix("Cargo.toml")
                    .map(|dir| dir.trim_end_matches('/'))
            })
            .collect();
        #[cfg(test)]
        CARGO_ROOT_SET_BUILDS.with(|counter| counter.set(counter.get() + 1));
        let mut file_index: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::with_capacity(files.len());
        for (index, file) in files.iter().enumerate() {
            file_index.entry(file.path.as_str()).or_insert(index);
        }
        let mut state = Self {
            request,
            intent,
            files: files.to_vec(),
            build_systems,
            selectors,
            contract: EngineeringContract::default(),
            provenance: Vec::new(),
            uncertainties: Vec::new(),
            touched_files: Vec::new(),
            touched_symbols: Vec::new(),
            total_lines: 0,
            cargo_roots,
            file_index,
        };
        state.collect_touched_surface();
        state.compile_test_obligations();
        state.compile_compile_obligations();
        state.compile_constraints();
        state.compile_migration_statements();
        state.compile_risks();
        state.record_uncertainties();
        let repository_digest = repository_digest(&request.repository);
        CompiledContract {
            contract: state.contract,
            provenance: state.provenance,
            uncertainties: state.uncertainties,
            intent_tokens: intent.to_vec(),
            build_systems: build_systems.to_vec(),
            touched_files: state.touched_files,
            touched_symbols: state.touched_symbols,
            repository_digest,
            repository_acquisition: request.repository.acquisition.clone(),
            enrichments: Vec::new(),
        }
    }

    fn push_provenance(
        &mut self,
        statement_id: &str,
        kind: StatementKind,
        origin: StatementOrigin,
        confidence_bps: u16,
        basis: String,
    ) {
        self.provenance.push(StatementProvenance {
            statement_id: statement_id.to_string(),
            kind,
            origin,
            confidence_bps: confidence_bps.min(10_000),
            basis: bounded(&basis, MAX_STATEMENT_BYTES),
        });
    }

    fn collect_touched_surface(&mut self) {
        let mut matched: Vec<String> = Vec::new();
        let mut symbols: Vec<String> = Vec::new();
        let mut content_budget = MAX_TOTAL_CONTENT_BYTES;
        for file in &self.files {
            let lower_path = file.path.to_ascii_lowercase();
            let path_matched = matches_tokens(&lower_path, self.intent);
            let mut content_matched = false;
            if let Some(content) = &file.content {
                if content.len() <= MAX_FILE_CONTENT_BYTES && content_budget >= content.len() {
                    content_budget -= content.len();
                    let lower = content.to_ascii_lowercase();
                    content_matched = matches_tokens(&lower, self.intent);
                    self.total_lines = self
                        .total_lines
                        .saturating_add(content.lines().count() as u64);
                    if path_matched || content_matched {
                        for symbol in rust_symbols(file.path.as_str(), content) {
                            if symbols.len() < MAX_TOUCHED_SYMBOLS {
                                symbols.push(symbol);
                            }
                        }
                    }
                }
            }
            if (path_matched || content_matched)
                && matched.len() < MAX_MATCHED_FILES
                && file.path.ends_with(".rs")
            {
                matched.push(file.path.clone());
            }
        }
        matched.sort();
        symbols.sort();
        self.touched_files = matched;
        self.touched_symbols = symbols;
    }

    fn compile_test_obligations(&mut self) {
        // Tests in the SAME package as a touched file are obligations even
        // when the user's wording does not repeat the test name. The old
        // selector/path token match dropped exactly the tests that protect a
        // rename or a bug fix ("fix add" vs `tests::add_returns_sum`), so the
        // contract reached the model with no executable proof at all.
        let touched_roots: BTreeSet<String> = self
            .touched_files
            .iter()
            .map(|path| cargo_component_root(path, &self.cargo_roots))
            .collect();
        let mut candidates: Vec<&DiscoveredTest> = self
            .selectors
            .iter()
            .filter(|test| {
                self.intent.is_empty()
                    || matches_tokens(&test.selector.to_ascii_lowercase(), self.intent)
                    || matches_tokens(&test.path.to_ascii_lowercase(), self.intent)
                    || touched_roots
                        .iter()
                        .any(|root| test.path.starts_with(root.as_str()))
                    || self
                        .touched_symbols
                        .iter()
                        .any(|symbol| test.selector.contains(symbol))
            })
            .collect();
        // A vague goal that matched no path must not compile a contract with
        // zero proof: the repository's bounded test surface is the honest
        // fallback (truncated, deterministic).
        if candidates.is_empty() {
            candidates = self.selectors.iter().take(4).collect();
        }
        candidates.sort_by(|a, b| {
            a.selector
                .cmp(&b.selector)
                .then_with(|| a.path.cmp(&b.path))
        });
        for test in candidates.into_iter().take(MAX_TEST_OBLIGATIONS) {
            let claim = bounded(
                &format!("existing test `{}` passes after the change", test.selector),
                MAX_STATEMENT_BYTES,
            );
            let selector_matched = matches_tokens(&test.selector.to_ascii_lowercase(), self.intent);
            let binding = selector_binding(test);
            if binding.validate().is_err() {
                continue;
            }
            let obligation = EngineeringContract::obligation_bound(
                &claim,
                ProofEvidenceKind::TestExecution,
                ContractOrigin::Native,
                Requirement::Required,
                binding,
                AcceptedStrength::Supported,
            );
            let id = obligation.id.clone();
            self.contract.proof_obligations.push(obligation);
            self.push_provenance(
                &id,
                StatementKind::ProofObligation,
                StatementOrigin::RepoAnalysis,
                if selector_matched { 7_500 } else { 6_000 },
                format!(
                    "discovered test {}{} in {}",
                    test.selector,
                    if selector_matched {
                        " (matches the objective)"
                    } else {
                        " (in a matched subsystem)"
                    },
                    test.path
                ),
            );
        }
    }

    fn compile_compile_obligations(&mut self) {
        if !self.build_systems.iter().any(|system| system == "cargo") {
            return;
        }
        let mut roots: BTreeSet<String> = BTreeSet::new();
        for path in &self.touched_files {
            if !path.ends_with(".rs") {
                continue;
            }
            roots.insert(cargo_component_root(path, &self.cargo_roots));
        }
        // A goal that named no path still executes in a cargo workspace: the
        // compile proof is the minimum honest contract.
        if roots.is_empty() {
            roots.insert(cargo_component_root("src/lib.rs", &self.cargo_roots));
        }
        for root in roots.into_iter().take(MAX_COMPILE_OBLIGATIONS) {
            let label = component_label(&root);
            let claim = bounded(
                &format!("the cargo component at {label} still compiles"),
                MAX_STATEMENT_BYTES,
            );
            let check_id = component_check_id(&root, "rust_check");
            let obligation = EngineeringContract::obligation_bound(
                &claim,
                ProofEvidenceKind::Compile,
                ContractOrigin::Native,
                Requirement::Advisory,
                EvidenceBinding::CheckIds(vec![check_id.clone()]),
                AcceptedStrength::for_requirement(Requirement::Advisory),
            );
            let id = obligation.id.clone();
            self.contract.proof_obligations.push(obligation);
            self.push_provenance(
                &id,
                StatementKind::ProofObligation,
                StatementOrigin::RepoAnalysis,
                6_000,
                format!("cargo manifest detected for {label}; native check id {check_id}"),
            );
        }
    }

    fn compile_constraints(&mut self) {
        if !self
            .intent
            .iter()
            .any(|token| COMPAT_TOKENS.contains(&token.as_str()))
        {
            return;
        }
        // A rename that explicitly forbids the old name is a BREAKING
        // change: compiling "the public API stays source-compatible" from
        // the word "rename" creates a contradiction the completion gate can
        // never discharge (observed: a correct rename blocked at
        // VerifiedComplete). A breaking rename gets the call-site/test
        // obligations, not a compatibility claim.
        let goal = self.request.goal.to_ascii_lowercase();
        let removes_old_surface = goal.contains("rename")
            && (goal.contains("no occurrence")
                || goal.contains("no longer")
                || goal.contains("must not remain")
                || goal.contains("remove")
                || goal.contains("delete"));
        if removes_old_surface {
            return;
        }
        let mut by_component: std::collections::BTreeMap<String, (usize, usize, Vec<String>)> =
            std::collections::BTreeMap::new();
        for path in &self.touched_files {
            let Some(&file_index) = self.file_index.get(path.as_str()) else {
                continue;
            };
            let Some(content) = self.files[file_index].content.as_ref() else {
                continue;
            };
            let public_items = public_item_count(content);
            if public_items == 0 {
                continue;
            }
            let root = cargo_component_root(path, &self.cargo_roots);
            let entry = by_component.entry(root).or_insert((0, 0, Vec::new()));
            entry.0 += public_items;
            entry.1 += 1;
            if entry.2.len() < 3 {
                entry.2.push(path.clone());
            }
        }
        for (root, (public_items, matched_files, examples)) in by_component {
            if self.contract.constraints.len() >= MAX_CONSTRAINTS {
                break;
            }
            let label = component_label(&root);
            let statement = bounded(
                &format!(
                    "the public API surface of {label} stays source-compatible ({public_items} public items across {matched_files} matched files)"
                ),
                MAX_STATEMENT_BYTES,
            );
            let id = entry_id("c1", &statement);
            self.contract.constraints.push(Constraint {
                id: id.clone(),
                statement,
                origin: ContractOrigin::Native,
            });
            self.push_provenance(
                &id,
                StatementKind::Constraint,
                StatementOrigin::RepoAnalysis,
                6_500,
                format!("public items detected in {}", examples.join(", ")),
            );
        }
    }

    fn compile_migration_statements(&mut self) {
        let migrations = detect_migration_files(&self.files);
        if migrations.is_empty() {
            return;
        }
        let intent_names_migration = self
            .intent
            .iter()
            .any(|token| MIGRATION_TOKENS.contains(&token.as_str()));
        if !intent_names_migration {
            return;
        }
        let example = migrations.first().cloned().unwrap_or_default();
        if self.contract.invariants.len() < MAX_INVARIANTS {
            let statement = bounded(
                &format!(
                    "existing data remains readable and consistent across the migration ({} migration artifact(s) detected)",
                    migrations.len()
                ),
                MAX_STATEMENT_BYTES,
            );
            let id = entry_id("i1", &statement);
            self.contract.invariants.push(Invariant {
                id: id.clone(),
                statement,
                origin: ContractOrigin::Native,
            });
            self.push_provenance(
                &id,
                StatementKind::Invariant,
                StatementOrigin::RepoAnalysis,
                6_500,
                format!(
                    "migration artifacts detected: {} (+{} more)",
                    example,
                    migrations.len().saturating_sub(1)
                ),
            );
        }
        if self.contract.risks.len() < MAX_RISKS {
            let description = bounded(
                &format!(
                    "the migration over {} detected migration artifact(s) may corrupt or strand existing data",
                    migrations.len()
                ),
                MAX_STATEMENT_BYTES,
            );
            let id = entry_id("r1", &description);
            self.contract.risks.push(RiskHypothesis {
                id: id.clone(),
                description,
                severity: RiskSeverity::High,
                origin: ContractOrigin::Native,
            });
            self.push_provenance(
                &id,
                StatementKind::Risk,
                StatementOrigin::RepoAnalysis,
                6_000,
                format!("migration artifacts detected: {}", example),
            );
        }
    }

    fn compile_risks(&mut self) {
        let names_race = self
            .intent
            .iter()
            .any(|token| RACE_TOKENS.contains(&token.as_str()));
        if names_race && !self.touched_files.is_empty() && self.contract.risks.len() < MAX_RISKS {
            let example = self.touched_files.first().cloned().unwrap_or_default();
            let description = bounded(
                &format!(
                    "the matched change surface ({} file(s)) may contain a race under concurrent or replayed execution",
                    self.touched_files.len()
                ),
                MAX_STATEMENT_BYTES,
            );
            let id = entry_id("r1", &description);
            self.contract.risks.push(RiskHypothesis {
                id: id.clone(),
                description,
                severity: RiskSeverity::High,
                origin: ContractOrigin::Native,
            });
            self.push_provenance(
                &id,
                StatementKind::Risk,
                StatementOrigin::RepoAnalysis,
                6_000,
                format!("objective names concurrency/replay; matched files start at {example}"),
            );
        }
        if (self.touched_files.len() >= 8 || self.total_lines >= 500)
            && self.contract.risks.len() < MAX_RISKS
        {
            let description = bounded(
                &format!(
                    "the change surface spans {} file(s) and {} lines; partial edits may leave inconsistent state",
                    self.touched_files.len(),
                    self.total_lines
                ),
                MAX_STATEMENT_BYTES,
            );
            let id = entry_id("r1", &description);
            self.contract.risks.push(RiskHypothesis {
                id: id.clone(),
                description,
                severity: RiskSeverity::Medium,
                origin: ContractOrigin::Native,
            });
            self.push_provenance(
                &id,
                StatementKind::Risk,
                StatementOrigin::RepoAnalysis,
                5_500,
                "changed-code scale estimate from the bounded snapshot".to_string(),
            );
        }
    }

    fn record_uncertainties(&mut self) {
        let mut reasons: Vec<(StatementOrigin, u16, String, String, RiskSeverity)> = Vec::new();
        let has_obligations = !self.contract.proof_obligations.is_empty();
        if !has_obligations {
            let reason = if self.request.repository.acquisition.is_unavailable() {
                "the repository state was unavailable"
            } else if self.build_systems.is_empty() {
                "no build system was detected in the acquired snapshot"
            } else if self.selectors.is_empty() {
                "no existing test was discovered"
            } else {
                "no existing test matches the objective"
            };
            reasons.push((
                StatementOrigin::Heuristic,
                3_000,
                format!(
                    "no deterministic proof obligation could be derived from the repository: {reason}"
                ),
                format!(
                    "scanned {} file(s), detected build system(s) {}, discovered {} test selector(s), matched {} of them",
                    self.files.len(),
                    if self.build_systems.is_empty() {
                        "none".to_string()
                    } else {
                        self.build_systems.join(", ")
                    },
                    self.selectors.len(),
                    self.contract
                        .proof_obligations
                        .iter()
                        .filter(|o| o.evidence == ProofEvidenceKind::TestExecution)
                        .count()
                ),
                RiskSeverity::Medium,
            ));
        }
        if self.build_systems.is_empty() {
            let (statement, basis) = match &self.request.repository.acquisition {
                RepositoryAcquisition::Complete => (
                    "no build system was detected; automated verification may be unavailable for this objective".to_string(),
                    "manifest detection over the bounded snapshot found no known build-system manifest".to_string(),
                ),
                RepositoryAcquisition::Partial { .. } => (
                    "no build system was detected in the partially acquired repository state; unread paths may contain manifests".to_string(),
                    "manifest detection over the partially acquired snapshot found no known build-system manifest".to_string(),
                ),
                RepositoryAcquisition::Unavailable { reason } => (
                    "no build system could be detected because the repository state was unavailable".to_string(),
                    format!("repository acquisition is Unavailable: {reason}"),
                ),
            };
            reasons.push((
                StatementOrigin::RepoAnalysis,
                3_500,
                statement,
                basis,
                RiskSeverity::Medium,
            ));
        }
        // Intent-vs-repository mismatches (source analysis): the goal names a
        // concern the bounded snapshot could not corroborate. Disclosed,
        // never silently dropped.
        let detects_migration = self
            .intent
            .iter()
            .any(|token| MIGRATION_TOKENS.contains(&token.as_str()));
        if detects_migration && detect_migration_files(&self.files).is_empty() {
            let (statement, basis, severity) = match &self.request.repository.acquisition {
                RepositoryAcquisition::Complete => (
                    "the objective names a migration, but no migration artifact was detected in the bounded snapshot".to_string(),
                    "goal tokens name migration/schema; path-based migration detection found nothing".to_string(),
                    RiskSeverity::Medium,
                ),
                RepositoryAcquisition::Partial { .. } => (
                    "the objective names a migration, but no migration artifact was found in the partially acquired repository state".to_string(),
                    "goal tokens name migration/schema; path-based migration detection found nothing in the partially acquired snapshot".to_string(),
                    RiskSeverity::Medium,
                ),
                RepositoryAcquisition::Unavailable { reason } => (
                    "the objective names a migration, but no migration artifact could be verified because the repository state was unavailable".to_string(),
                    format!("goal tokens name migration/schema; no repository content could be acquired: {reason}"),
                    RiskSeverity::Unknown,
                ),
            };
            reasons.push((
                StatementOrigin::SourceAnalysis,
                3_000,
                statement,
                basis,
                severity,
            ));
        }
        if self
            .intent
            .iter()
            .any(|token| RACE_TOKENS.contains(&token.as_str()))
            && self.touched_files.is_empty()
        {
            let (statement, basis, severity) = match &self.request.repository.acquisition {
                RepositoryAcquisition::Complete => (
                    "the objective names concurrency or replay, but no matching code was found in the bounded snapshot".to_string(),
                    "goal tokens name concurrency/replay; the matched-file set is empty".to_string(),
                    RiskSeverity::Medium,
                ),
                RepositoryAcquisition::Partial { .. } => (
                    "the objective names concurrency or replay, but no matching code was found in the partially acquired repository state".to_string(),
                    "goal tokens name concurrency/replay; the matched-file set is empty over the partially acquired snapshot".to_string(),
                    RiskSeverity::Medium,
                ),
                RepositoryAcquisition::Unavailable { reason } => (
                    "the objective names concurrency or replay, but no matching code could be verified because the repository state was unavailable".to_string(),
                    format!("goal tokens name concurrency/replay; no repository content could be acquired: {reason}"),
                    RiskSeverity::Unknown,
                ),
            };
            reasons.push((
                StatementOrigin::SourceAnalysis,
                3_000,
                statement,
                basis,
                severity,
            ));
        }
        match &self.request.repository.acquisition {
            RepositoryAcquisition::Complete => {}
            RepositoryAcquisition::Unavailable { reason } => {
                reasons.push((
                    StatementOrigin::RepoAnalysis,
                    3_000,
                    "repository state is unavailable; no repository content could be analyzed for this objective"
                        .to_string(),
                    format!("repository acquisition is Unavailable: {reason}"),
                    RiskSeverity::Unknown,
                ));
            }
            RepositoryAcquisition::Partial { reasons: gaps } => {
                // The state disclosure is emitted FIRST so the uncertainty
                // cap can never silently drop it; proof-relevant gaps follow
                // as their own bounded rows.
                reasons.push((
                    StatementOrigin::RepoAnalysis,
                    3_000,
                    "repository analysis is partial; deterministic coverage may be missing"
                        .to_string(),
                    format!(
                        "repository acquisition is Partial; acquisition gaps: {}",
                        gaps.iter()
                            .map(AcquisitionGap::render)
                            .collect::<Vec<String>>()
                            .join("; ")
                    ),
                    RiskSeverity::Unknown,
                ));
                for gap in gaps.iter().filter(|gap| gap.proof_relevant) {
                    reasons.push((
                        StatementOrigin::RepoAnalysis,
                        3_000,
                        format!(
                            "repository analysis is partial: proof-relevant content could not be acquired ({})",
                            gap.kind.as_str()
                        ),
                        gap.render(),
                        RiskSeverity::Unknown,
                    ));
                }
            }
        }
        for (origin, confidence_bps, statement, basis, severity) in reasons {
            if self.uncertainties.len() >= MAX_UNCERTAINTIES {
                break;
            }
            let statement = bounded(&statement, MAX_STATEMENT_BYTES);
            let id = entry_id("r1", &statement);
            self.uncertainties.push(ContractUncertainty {
                id: id.clone(),
                statement: statement.clone(),
                origin: origin.clone(),
                confidence_bps,
                basis: bounded(&basis, MAX_STATEMENT_BYTES),
            });
            if self.contract.risks.len() < MAX_RISKS {
                self.contract.risks.push(RiskHypothesis {
                    id: id.clone(),
                    description: statement,
                    severity,
                    origin: origin.contract_origin(),
                });
                self.push_provenance(&id, StatementKind::Risk, origin, confidence_bps, basis);
            }
        }
    }
}

/// The pure conversion of one bounded finding batch: the accepted
/// enrichment statements plus one typed diagnostic per refused finding.
/// Deterministic in input order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FindingsConversion {
    pub statements: Vec<EnrichmentStatement>,
    pub diagnostics: Vec<FindingDiagnostic>,
    /// Findings refused (by validation or the bound).
    pub rejected: usize,
}

/// Validate + convert a bounded batch of findings against the consult's
/// identity. Pure and total: hostile input yields typed diagnostics, never
/// an error and never a statement the compiler must skip.
pub fn findings_to_statements(
    expected_provider: &str,
    expected_digest: &str,
    max_findings: usize,
    findings: &[ProviderFinding],
) -> FindingsConversion {
    let mut out = FindingsConversion::default();
    let mut cap_diagnosed = false;
    for (index, finding) in findings.iter().enumerate() {
        if index >= max_findings {
            out.rejected += 1;
            if !cap_diagnosed && out.diagnostics.len() < MAX_FINDING_DIAGNOSTICS {
                cap_diagnosed = true;
                out.diagnostics.push(FindingDiagnostic::new(
                    expected_provider,
                    FindingDiagnosticCode::CapReached,
                    format!("more than {max_findings} findings returned; the excess was dropped"),
                ));
            }
            continue;
        }
        match finding_to_statement(finding, expected_provider, expected_digest) {
            Ok(statement) => out.statements.push(statement),
            Err(rejection) => {
                out.rejected += 1;
                if out.diagnostics.len() < MAX_FINDING_DIAGNOSTICS {
                    out.diagnostics.push(FindingDiagnostic::new(
                        expected_provider,
                        rejection.code(),
                        format!("{}: {}", finding.kind.as_str(), rejection.detail()),
                    ));
                }
            }
        }
    }
    out
}

/// Validate ONE finding against the consult's identity and convert it to an
/// immutable bounded enrichment statement. Every refusal is typed.
pub fn finding_to_statement(
    finding: &ProviderFinding,
    expected_provider: &str,
    expected_digest: &str,
) -> Result<EnrichmentStatement, FindingRejection> {
    if finding.provider != expected_provider {
        return Err(FindingRejection::ProviderMismatch {
            expected: expected_provider.to_string(),
            actual: finding.provider.clone(),
        });
    }
    if finding.repository_digest != expected_digest {
        return Err(FindingRejection::DigestMismatch {
            expected: expected_digest.to_string(),
            actual: finding.repository_digest.clone(),
        });
    }
    if finding.kind != finding.question.expected_kind() {
        return Err(FindingRejection::KindMismatch {
            question: finding.question,
            kind: finding.kind,
        });
    }
    if finding.text.trim().is_empty() {
        return Err(FindingRejection::EmptyText);
    }
    if finding.text.len() > MAX_FINDING_TEXT_BYTES {
        return Err(FindingRejection::TextTooLong {
            bytes: finding.text.len(),
            max: MAX_FINDING_TEXT_BYTES,
        });
    }
    if finding.basis.trim().is_empty() {
        return Err(FindingRejection::EmptyBasis);
    }
    if finding.basis.len() > MAX_FINDING_BASIS_BYTES {
        return Err(FindingRejection::BasisTooLong {
            bytes: finding.basis.len(),
            max: MAX_FINDING_BASIS_BYTES,
        });
    }
    if finding.confidence_bps > 10_000 {
        return Err(FindingRejection::ConfidenceOutOfRange {
            bps: finding.confidence_bps,
        });
    }
    if let Some(revision) = &finding.revision {
        if revision.len() > MAX_FINDING_REVISION_BYTES {
            return Err(FindingRejection::RevisionTooLong {
                bytes: revision.len(),
                max: MAX_FINDING_REVISION_BYTES,
            });
        }
    }
    let (kind, severity) = match finding.kind {
        FindingKind::Component => (
            EnrichmentKind::Component,
            finding.severity.or(Some(RiskSeverity::Low)),
        ),
        FindingKind::Invariant => (EnrichmentKind::Invariant, finding.severity),
        FindingKind::Dependency => (EnrichmentKind::Dependency, finding.severity),
        FindingKind::Risk => (EnrichmentKind::Risk, finding.severity),
        FindingKind::HistoricalFailure => (EnrichmentKind::HistoricalFailure, finding.severity),
    };
    let text = bounded(
        &format!("{}: {}", finding.question.label(), finding.text.trim()),
        MAX_STATEMENT_BYTES,
    );
    Ok(EnrichmentStatement {
        kind,
        text,
        confidence_bps: finding.confidence_bps,
        basis: bounded(&finding.basis, MAX_FINDING_BASIS_BYTES),
        evidence: None,
        binding: None,
        severity,
        advisory: false,
    })
}

fn push_skip_reason(reasons: &mut Vec<String>, reason: &str) {
    if reasons.len() < 3 && !reasons.iter().any(|seen| seen == reason) {
        reasons.push(reason.to_string());
    }
}

/// Apply one external risk-vector statement (a risk, or a typed finding kind
/// disclosed through the risk vector) with bounded caps and provenance.
fn push_enriched_risk(
    compiled: &mut CompiledContract,
    origin: &StatementOrigin,
    kind: StatementKind,
    text: String,
    basis: String,
    statement: &EnrichmentStatement,
) -> Result<(), String> {
    if compiled.contract.risks.len() >= MAX_RISKS {
        return Err("risk cap reached".into());
    }
    let id = entry_id("r1", &text);
    if compiled.contract.risks.iter().any(|r| r.id == id) {
        return Err("duplicate risk id".into());
    }
    compiled.contract.risks.push(RiskHypothesis {
        id: id.clone(),
        description: text,
        severity: statement.severity.unwrap_or(RiskSeverity::Medium),
        origin: origin.contract_origin(),
    });
    compiled.provenance.push(StatementProvenance {
        statement_id: id,
        kind,
        origin: origin.clone(),
        confidence_bps: statement.confidence_bps.min(10_000),
        basis,
    });
    Ok(())
}

fn apply_enrichment_statement(
    compiled: &mut CompiledContract,
    origin: &StatementOrigin,
    statement: EnrichmentStatement,
) -> Result<(), String> {
    if statement.text.trim().is_empty() {
        return Err("empty statement text".into());
    }
    if statement.text.len() > MAX_STATEMENT_BYTES {
        return Err("statement text exceeds the size bound".into());
    }
    let text = statement.text.trim().to_string();
    let basis = if statement.basis.trim().is_empty() {
        format!("external provider {} supplied no basis", origin.as_str())
    } else {
        bounded(&statement.basis, MAX_STATEMENT_BYTES)
    };
    match statement.kind {
        EnrichmentKind::Constraint => {
            if compiled.contract.constraints.len() >= MAX_CONSTRAINTS {
                return Err("constraint cap reached".into());
            }
            let id = entry_id("c1", &text);
            if compiled.contract.constraints.iter().any(|c| c.id == id) {
                return Err("duplicate constraint id".into());
            }
            compiled.contract.constraints.push(Constraint {
                id: id.clone(),
                statement: text,
                origin: origin.contract_origin(),
            });
            compiled.provenance.push(StatementProvenance {
                statement_id: id,
                kind: StatementKind::Constraint,
                origin: origin.clone(),
                confidence_bps: statement.confidence_bps.min(10_000),
                basis,
            });
            Ok(())
        }
        EnrichmentKind::Invariant => {
            if compiled.contract.invariants.len() >= MAX_INVARIANTS {
                return Err("invariant cap reached".into());
            }
            let id = entry_id("i1", &text);
            if compiled.contract.invariants.iter().any(|i| i.id == id) {
                return Err("duplicate invariant id".into());
            }
            compiled.contract.invariants.push(Invariant {
                id: id.clone(),
                statement: text,
                origin: origin.contract_origin(),
            });
            compiled.provenance.push(StatementProvenance {
                statement_id: id,
                kind: StatementKind::Invariant,
                origin: origin.clone(),
                confidence_bps: statement.confidence_bps.min(10_000),
                basis,
            });
            Ok(())
        }
        EnrichmentKind::Risk => push_enriched_risk(
            compiled,
            origin,
            StatementKind::Risk,
            text,
            basis,
            &statement,
        ),
        // Finding kinds without a dedicated IR vector apply as external risk
        // DISCLOSURES (never as constraints/invariants: a component/dependency
        // finding is data, and the finer typed kind survives in provenance).
        EnrichmentKind::Component
        | EnrichmentKind::Dependency
        | EnrichmentKind::HistoricalFailure => {
            let statement_kind = match statement.kind {
                EnrichmentKind::Component => StatementKind::Component,
                EnrichmentKind::Dependency => StatementKind::Dependency,
                EnrichmentKind::HistoricalFailure => StatementKind::HistoricalFailure,
                _ => unreachable!("arm matched above"),
            };
            push_enriched_risk(compiled, origin, statement_kind, text, basis, &statement)
        }
        EnrichmentKind::ProofObligation => {
            if compiled.contract.proof_obligations.len()
                >= MAX_TEST_OBLIGATIONS + MAX_COMPILE_OBLIGATIONS + MAX_ENRICHMENT_STATEMENTS
            {
                return Err("proof obligation cap reached".into());
            }
            let evidence = statement
                .evidence
                .ok_or_else(|| "obligation without an evidence kind".to_string())?;
            let binding = statement
                .binding
                .ok_or_else(|| "obligation without an evidence binding".to_string())?;
            if !binding.is_bound() {
                return Err("obligation carries the legacy Category binding".into());
            }
            binding.validate()?;
            let requirement = if statement.advisory {
                Requirement::Advisory
            } else {
                Requirement::Required
            };
            let obligation = EngineeringContract::obligation_bound(
                &text,
                evidence,
                origin.contract_origin(),
                requirement,
                binding,
                AcceptedStrength::for_requirement(requirement),
            );
            let id = obligation.id.clone();
            if compiled
                .contract
                .proof_obligations
                .iter()
                .any(|o| o.id == id)
            {
                return Err("duplicate obligation id".into());
            }
            compiled.contract.proof_obligations.push(obligation);
            compiled.provenance.push(StatementProvenance {
                statement_id: id,
                kind: StatementKind::ProofObligation,
                origin: origin.clone(),
                confidence_bps: statement.confidence_bps.min(10_000),
                basis,
            });
            Ok(())
        }
    }
}

// ------------------------------------------------------------ repository facts

/// Normalize a snapshot: drop unusable paths, sort by path (content-bearing
/// entries first on duplicates), deduplicate, cap at [`MAX_COMPILE_FILES`].
/// Deterministic regardless of caller ordering.
fn normalize_files(files: &[RepositoryFile]) -> Vec<&RepositoryFile> {
    let mut out: Vec<&RepositoryFile> = files
        .iter()
        .filter(|file| {
            !file.path.is_empty()
                && file.path.len() <= MAX_PATH_BYTES
                && !file.path.starts_with('/')
                && !file.path.contains("..")
        })
        .collect();
    out.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| a.content.is_none().cmp(&b.content.is_none()))
    });
    out.dedup_by(|a, b| a.path == b.path);
    out.truncate(MAX_COMPILE_FILES);
    out
}

/// Detect known build systems from manifest paths (sorted, stable names).
fn detect_build_systems(files: &[&RepositoryFile]) -> Vec<String> {
    let mut systems: BTreeSet<&'static str> = BTreeSet::new();
    for file in files {
        let Some(name) = file.path.rsplit('/').next() else {
            continue;
        };
        let system = match name {
            "Cargo.toml" => Some("cargo"),
            "package.json" => Some("node"),
            "go.mod" => Some("go"),
            "pyproject.toml" | "setup.py" | "setup.cfg" => Some("python"),
            "CMakeLists.txt" => Some("cmake"),
            "Makefile" | "makefile" | "GNUmakefile" => Some("make"),
            "meson.build" => Some("meson"),
            "build.ninja" => Some("ninja"),
            "BUILD" | "BUILD.bazel" | "MODULE.bazel" | "WORKSPACE" => Some("bazel"),
            "pom.xml" => Some("maven"),
            "build.gradle" | "build.gradle.kts" => Some("gradle"),
            _ if name.ends_with(".csproj") || name.ends_with(".sln") => Some("dotnet"),
            _ => None,
        };
        if let Some(system) = system {
            systems.insert(system);
        }
    }
    systems.into_iter().map(str::to_string).collect()
}

/// Discover the cargo test selectors authored inline in ONE Rust file, using
/// the exact path layout rules the contract compiler applies (`tests/<name>`
/// integration targets and `src/**` module trees). This is the shared
/// authority for the compiler's test obligations and the verifier's derived
/// test checks: a selector the contract can demand must be executable.
pub fn rust_test_selectors_for_path(path: &str, content: &str) -> Vec<String> {
    if !path.ends_with(".rs") || content.len() > MAX_FILE_CONTENT_BYTES {
        return Vec::new();
    }
    let Some(module) = rust_module_path(path) else {
        return Vec::new();
    };
    extract_test_selectors(&module, content)
        .into_iter()
        .filter(|selector| selector.len() <= MAX_SELECTOR_BYTES)
        .collect()
}

/// One discovered Rust test declaration. `selector` is the semantic
/// module-qualified identity recorded on the contract; `executable` is the
/// libtest filter name that actually runs it (`cargo test --test <target>
/// <executable>` for integration targets, `cargo test <selector>` for unit
/// tests); `body` is the bounded function body used by proof rebinding to
/// prove a symbol rename; `attributes` are the declaration's attribute lines
/// (`#[test]`, `#[should_panic]`, `#[ignore]`, ...) because they change the
/// executable meaning of an otherwise identical body. `target` is the
/// integration test target (file stem) when the declaration lives in
/// `tests/<name>.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustTestDeclaration {
    pub selector: String,
    pub executable: String,
    pub target: Option<String>,
    pub body: String,
    /// TRUE when `body` is the COMPLETE brace-balanced function body. An
    /// unparseable/truncated body is never proof-bearing (the adapters fail
    /// closed); an empty but complete body is legal.
    pub body_complete: bool,
    pub attributes: Vec<String>,
}

/// Discover the Rust test declarations authored in ONE file. This is the
/// shared authority for the contract compiler, the verifier's derived test
/// checks and proof rebinding: a selector the contract demands and a
/// selector the verifier executes can never drift.
pub fn rust_test_declarations_for_path(path: &str, content: &str) -> Vec<RustTestDeclaration> {
    if !path.ends_with(".rs") || content.len() > MAX_FILE_CONTENT_BYTES {
        return Vec::new();
    }
    let Some(module) = rust_module_path(path) else {
        return Vec::new();
    };
    let target = rust_integration_target(path);
    let mut out = Vec::new();
    for declaration in extract_test_declarations(&module, content) {
        if declaration.selector.len() > MAX_SELECTOR_BYTES {
            continue;
        }
        let executable = match &target {
            Some(target) => declaration
                .selector
                .strip_prefix(&format!("{target}::"))
                .unwrap_or(&declaration.selector)
                .to_string(),
            None => declaration.selector.clone(),
        };
        if executable.is_empty() || executable.len() > MAX_SELECTOR_BYTES {
            continue;
        }
        out.push(RustTestDeclaration {
            selector: declaration.selector,
            executable,
            target: target.clone(),
            body: declaration.body,
            body_complete: declaration.body_complete,
            attributes: declaration.attributes,
        });
    }
    out.sort_by(|a, b| {
        a.selector
            .cmp(&b.selector)
            .then_with(|| a.executable.cmp(&b.executable))
    });
    out.dedup_by(|a, b| a.selector == b.selector && a.executable == b.executable);
    out
}

/// One discovered test selector.
struct DiscoveredTest {
    selector: String,
    executable: String,
    path: String,
}

/// Discover existing cargo test selectors from Rust sources: top-level
/// `tests/<name>.rs` integration targets and `src/**/*.rs` module trees,
/// with inline `mod <name> { ... }` nesting and the
/// `#[test]`/`#[tokio::test]`/`#[async_std::test]`/`#[futures_test::test]`
/// attributes. Sorted; capped at [`MAX_DISCOVERED_SELECTORS`]. This is a
/// text-level (tier 1) analysis — the basis and confidence say so.
fn discover_test_selectors(files: &[&RepositoryFile]) -> Vec<DiscoveredTest> {
    let mut out: Vec<DiscoveredTest> = Vec::new();
    for file in files {
        let Some(content) = &file.content else {
            continue;
        };
        for declaration in rust_test_declarations_for_path(&file.path, content) {
            out.push(DiscoveredTest {
                selector: declaration.selector,
                executable: declaration.executable,
                path: file.path.clone(),
            });
        }
    }
    out.sort_by(|a, b| {
        a.selector
            .cmp(&b.selector)
            .then_with(|| a.path.cmp(&b.path))
    });
    out.dedup_by(|a, b| a.selector == b.selector && a.path == b.path);
    out.truncate(MAX_DISCOVERED_SELECTORS);
    out
}

/// The cargo test selector binding for one discovered test: the semantic
/// selector, the EXECUTABLE libtest filter that actually runs it, and the
/// runtime's derived file-stem filter only when that filter is genuinely a
/// substring of the executable test name (`cargo test <filter>` only runs a
/// test whose NAME contains the filter; a file-stem filter on an integration
/// test whose function name differs would run zero tests and must never
/// discharge the claim).
fn selector_binding(test: &DiscoveredTest) -> EvidenceBinding {
    let mut targets = vec![test.selector.clone()];
    if test.executable != test.selector {
        targets.push(test.executable.clone());
    }
    if let Some(filter) = rust_test_filter(&test.path) {
        if filter != test.selector
            && test.executable.contains(&filter)
            && !targets.contains(&filter)
        {
            targets.push(filter);
        }
    }
    targets.dedup();
    if targets.len() == 1 {
        EvidenceBinding::TestSelector(targets.remove(0))
    } else {
        EvidenceBinding::TestSelectors(targets)
    }
}

/// Mirror of the runtime's derived Rust test filter: the file stem, only when
/// it is a clean single token (hostile stems yield no filter).
fn rust_test_filter(path: &str) -> Option<String> {
    let name = path.rsplit('/').next()?;
    let stem = name.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(name);
    let clean = !stem.is_empty()
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    clean.then(|| stem.to_string())
}

/// The integration test target (file stem) of a top-level
/// `tests/<name>.rs` (or `<pkg>/tests/<name>.rs`) file. Nested test files
/// are not cargo targets.
fn rust_integration_target(path: &str) -> Option<String> {
    let stem = path.strip_suffix(".rs")?;
    let rest = if let Some(rest) = stem.strip_prefix("tests/") {
        rest
    } else {
        let index = stem.rfind("/tests/")?;
        &stem[index + 7..]
    };
    if rest.contains('/') || rest.is_empty() {
        return None;
    }
    is_ident(rest).then(|| rest.to_string())
}

/// The module path of one Rust file (`None` when the layout is unknown):
/// * top-level integration targets `tests/<name>.rs` (and `<pkg>/tests/...`)
///   yield `<name>`;
/// * `src/...` files yield the module path below `src/` (dropping
///   `lib.rs` / `main.rs` / `mod.rs`), empty for the crate root;
/// * anything else is unknown and yields no selectors.
fn rust_module_path(path: &str) -> Option<Vec<String>> {
    if let Some(target) = rust_integration_target(path) {
        return Some(vec![target]);
    }
    let stem = path.strip_suffix(".rs")?;
    let below_src = if let Some(index) = stem.rfind("/src/") {
        &stem[index + 5..]
    } else {
        stem.strip_prefix("src/")?
    };
    let mut segments: Vec<String> = below_src
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect();
    if let Some("lib" | "main" | "mod") = segments.last().map(String::as_str) {
        segments.pop();
    }
    if segments.iter().any(|segment| !is_ident(segment)) {
        return None;
    }
    Some(segments)
}

fn is_ident(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// One test declaration extracted from a single source file.
struct ExtractedTest {
    selector: String,
    body: String,
    body_complete: bool,
    attributes: Vec<String>,
}

/// Extract the test declarations of one Rust file: `module::inline_mods::fn`
/// plus the bounded body and the attribute stack used to prove a symbol
/// rename without ever dropping execution semantics (`#[should_panic]`,
/// `#[ignore]`).
fn extract_test_declarations(module: &[String], content: &str) -> Vec<ExtractedTest> {
    let lines: Vec<&str> = content.lines().take(MAX_SCAN_LINES).collect();
    let mut out: Vec<ExtractedTest> = Vec::new();
    let mut mods: Vec<(String, i32)> = Vec::new();
    let mut depth: i32 = 0;
    let mut scan_state = crate::rust_lex::ScanState::default();
    let mut pending_test_attr = false;
    let mut attr_block: Vec<String> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        // Comment/string text is not a declaration: a commented-out
        // `#[test] fn ...` must never become a proof-bearing declaration.
        let code_line = scan_state.at_code_boundary();
        if code_line {
            if trimmed.starts_with("#[") {
                if is_test_attribute(trimmed) {
                    pending_test_attr = true;
                }
                attr_block.push(trimmed.to_string());
            } else if pending_test_attr {
                if let Some(name) = fn_name_of(trimmed) {
                    let mut parts = module.to_vec();
                    parts.extend(mods.iter().map(|(name, _)| name.clone()));
                    parts.push(name);
                    let (body, body_complete) = crate::rust_lex::test_body(&lines, index);
                    out.push(ExtractedTest {
                        selector: parts.join("::"),
                        body,
                        body_complete,
                        attributes: attr_block.clone(),
                    });
                }
                if !trimmed.is_empty() && !trimmed.starts_with("//") {
                    pending_test_attr = false;
                    attr_block.clear();
                }
            } else if !trimmed.is_empty() && !trimmed.starts_with("//") {
                attr_block.clear();
            }
            if let Some(name) = mod_open_name(trimmed) {
                mods.push((name, depth + 1));
            }
        }
        depth += crate::rust_lex::brace_delta(&mut scan_state, line);
        while let Some((_, open_depth)) = mods.last() {
            if *open_depth > depth {
                mods.pop();
            } else {
                break;
            }
        }
    }
    out.sort_by(|a, b| a.selector.cmp(&b.selector));
    out.dedup_by(|a, b| a.selector == b.selector);
    out
}

/// Extract only the test selectors of one Rust file (the semantic identity
/// used by the contract compiler and the historical public API).
fn extract_test_selectors(module: &[String], content: &str) -> Vec<String> {
    extract_test_declarations(module, content)
        .into_iter()
        .map(|test| test.selector)
        .collect()
}

fn is_test_attribute(trimmed: &str) -> bool {
    trimmed.starts_with("#[test]")
        || trimmed.starts_with("#[tokio::test")
        || trimmed.starts_with("#[async_std::test")
        || trimmed.starts_with("#[futures_test::test")
}

/// `mod name {` (optionally `pub`/`pub(crate)`) → `Some(name)`.
fn mod_open_name(trimmed: &str) -> Option<String> {
    let rest = strip_visibility(trimmed);
    let rest = rest.strip_prefix("mod ")?;
    let (name, tail) = split_ident(rest)?;
    let tail = tail.trim_start();
    let braces = tail.find('{')?;
    if tail[..braces].trim().is_empty() {
        Some(name)
    } else {
        None
    }
}

/// The fn name of a `fn name(...)` / `async fn` / `pub fn` line.
fn fn_name_of(trimmed: &str) -> Option<String> {
    let mut rest = strip_visibility(trimmed);
    loop {
        if let Some(r) = rest.strip_prefix("async ") {
            rest = r.trim_start();
            continue;
        }
        if let Some(r) = rest.strip_prefix("unsafe ") {
            rest = r.trim_start();
            continue;
        }
        if let Some(r) = rest.strip_prefix("const ") {
            rest = r.trim_start();
            continue;
        }
        break;
    }
    let rest = rest.strip_prefix("fn ")?;
    let (name, _) = split_ident(rest)?;
    Some(name)
}

fn strip_visibility(trimmed: &str) -> &str {
    let rest = trimmed.trim_start();
    if let Some(rest) = rest.strip_prefix("pub ") {
        return rest.trim_start();
    }
    if let Some(rest) = rest.strip_prefix("pub(crate) ") {
        return rest.trim_start();
    }
    if let Some(rest) = rest.strip_prefix("pub(super) ") {
        return rest.trim_start();
    }
    if let Some(rest) = rest.strip_prefix("pub(in ") {
        if let Some(end) = rest.find(") ") {
            return rest[end + 2..].trim_start();
        }
    }
    rest
}

fn split_ident(rest: &str) -> Option<(String, &str)> {
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    let name = &rest[..end];
    if !is_ident(name) {
        return None;
    }
    // The identifier must END at ASCII syntax: a non-ASCII continuation
    // (`fn résumé`) is not a Rust identifier and yields no symbol.
    match rest[end..].chars().next() {
        None => {}
        Some(next) if " \t(<{=:;,)".contains(next) => {}
        Some(_) => return None,
    }
    Some((name.to_string(), &rest[end..]))
}

/// The text-level symbol graph of one Rust source: `fn`/`struct`/`enum`/
/// `trait`/`impl`/`type`/`const`/`static`/`mod` declarations as
/// `"<kind> <name> in <path>:<line>"`.
fn rust_symbols(path: &str, content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (index, line) in content.lines().take(MAX_SCAN_LINES).enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        let rest = strip_visibility(trimmed);
        let (kind, rest) = if let Some(rest) = rest.strip_prefix("async fn ") {
            ("fn", rest)
        } else if let Some(rest) = rest.strip_prefix("fn ") {
            ("fn", rest)
        } else if let Some(rest) = rest.strip_prefix("struct ") {
            ("struct", rest)
        } else if let Some(rest) = rest.strip_prefix("enum ") {
            ("enum", rest)
        } else if let Some(rest) = rest.strip_prefix("trait ") {
            ("trait", rest)
        } else if let Some(rest) = rest.strip_prefix("type ") {
            ("type", rest)
        } else if let Some(rest) = rest.strip_prefix("const ") {
            ("const", rest)
        } else if let Some(rest) = rest.strip_prefix("static ") {
            ("static", rest)
        } else if let Some(rest) = rest.strip_prefix("mod ") {
            ("mod", rest)
        } else {
            continue;
        };
        if let Some((name, _)) = split_ident(rest) {
            out.push(format!("{kind} {name} in {path}:{}", index + 1));
        }
    }
    out
}

/// Count lines that declare a public item (`pub ` at the trimmed line start,
/// after comments/attributes) — the public API-surface density.
fn public_item_count(content: &str) -> usize {
    content
        .lines()
        .take(MAX_SCAN_LINES)
        .filter(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("pub ")
                && !trimmed.starts_with("pub use ")
                && !trimmed.starts_with("pub mod ")
        })
        .count()
}

/// The nearest ancestor directory of `path` that contains a `Cargo.toml`
/// among the snapshot paths (`""` = the repository root). `cargo_roots` is
/// the prebuilt set of manifest parent dirs (see
/// [`NativeCompile::cargo_roots`]).
fn cargo_component_root(path: &str, cargo_roots: &BTreeSet<&str>) -> String {
    let mut dir = match path.rfind('/') {
        Some(index) => &path[..index],
        None => "",
    };
    loop {
        if cargo_roots.contains(dir) {
            return dir.to_string();
        }
        match dir.rfind('/') {
            Some(index) => dir = &dir[..index],
            None => {
                if dir.is_empty() || cargo_roots.contains("") {
                    return String::new();
                }
                dir = "";
            }
        }
    }
}

fn component_label(root: &str) -> String {
    if root.is_empty() {
        "the repository root".to_string()
    } else {
        format!("`{root}`")
    }
}

/// The runtime's derived check id for one component (mirrors
/// `faktor_verify`'s `component_id`: sanitized root + `:` + base id, no
/// prefix for the repository root).
fn component_check_id(root: &str, base: &str) -> String {
    if root.is_empty() {
        return base.to_string();
    }
    let sanitized: String = root
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{sanitized}:{base}")
}

/// Detected migration artifacts (path-based, sorted): a migration directory
/// component, a `.sql` file or a `migration*` file name.
fn detect_migration_files(files: &[&RepositoryFile]) -> Vec<String> {
    let mut out: Vec<String> = files
        .iter()
        .filter(|file| {
            let lower = file.path.to_ascii_lowercase();
            let name = lower.rsplit('/').next().unwrap_or(lower.as_str());
            lower.split('/').any(|component| {
                matches!(
                    component,
                    "migrations" | "migration" | "migrate" | "alembic" | "flyway"
                )
            }) || name.ends_with(".sql")
                || name.starts_with("migration")
        })
        .map(|file| file.path.clone())
        .collect();
    out.sort();
    out.dedup();
    out.truncate(64);
    out
}

fn matches_tokens(haystack_lower: &str, tokens: &[String]) -> bool {
    tokens
        .iter()
        .any(|token| haystack_lower.contains(token.as_str()))
}

/// Deterministic content identity of the snapshot the contract was compiled
/// against, including its truthful acquisition state: a partially readable
/// tree is never byte-identical to a fully readable one with the same files.
fn repository_digest(repository: &RepositorySnapshot) -> String {
    let mut sorted: Vec<&RepositoryFile> = repository.files.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"faktor.contract.compile.repo.v2\0");
    match &repository.acquisition {
        RepositoryAcquisition::Complete => {
            hasher.update(b"complete\0");
        }
        RepositoryAcquisition::Partial { reasons } => {
            hasher.update(b"partial\0");
            for gap in reasons {
                hasher.update(gap.kind.as_str().as_bytes());
                hasher.update(b"\0");
                hasher.update(gap.path.as_deref().unwrap_or("").as_bytes());
                hasher.update(b"\0");
                hasher.update(gap.detail.as_bytes());
                hasher.update(b"\0");
                hasher.update(if gap.proof_relevant {
                    b"proof\0"
                } else {
                    b"context\0"
                });
            }
        }
        RepositoryAcquisition::Unavailable { reason } => {
            hasher.update(b"unavailable\0");
            hasher.update(reason.as_bytes());
            hasher.update(b"\0");
        }
    }
    for file in sorted {
        hasher.update(file.path.as_bytes());
        hasher.update(b"\0");
        match &file.content {
            Some(content) => {
                hasher.update(blake3::hash(content.as_bytes()).as_bytes());
            }
            None => {
                hasher.update(b"path-only");
            }
        }
        hasher.update(b"\n");
    }
    format!("repo2:{}", hasher.finalize().to_hex())
}

/// The deterministic statement id, replicating the session's user-authored
/// scheme (`faktor.contract.<kind>.v1` domain + text, BLAKE3, 16 hex) so the
/// durable union merge dedupes identical compiled and user statements.
fn entry_id(kind: &str, text: &str) -> String {
    let digest = blake3::hash(format!("faktor.contract.{kind}.v1\0{text}").as_bytes());
    format!("{kind}:{}", &digest.to_hex()[..16])
}

fn sanitized_provider_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let sanitized: String = trimmed
        .chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    (!sanitized.is_empty()).then_some(sanitized)
}

fn bounded(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::ClaimState;

    const REFRESH_GOAL: &str = "Fix the refresh-token race";

    fn refresh_snapshot() -> RepositorySnapshot {
        RepositorySnapshot::from_files(vec![
            RepositoryFile::with_content(
                "Cargo.toml",
                "[package]\nname = \"auth\"\nversion = \"0.1.0\"\n",
            ),
            RepositoryFile::with_content(
                "src/auth/refresh.rs",
                r#"
use std::sync::Mutex;

pub struct RefreshStore {
    lock: Mutex<u64>,
}

pub fn refresh(store: &RefreshStore) -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_is_rejected() {
        assert!(true);
    }

    #[tokio::test]
    async fn concurrent_refresh_yields_one_token() {}
}
"#,
            ),
            RepositoryFile::with_content(
                "tests/refresh_tokens.rs",
                r#"
#[test]
fn replayed_refresh_token_fails() {}
"#,
            ),
        ])
    }

    fn request(goal: &str, repository: RepositorySnapshot) -> ContractCompileRequest {
        ContractCompileRequest {
            goal: goal.to_string(),
            repository,
        }
    }

    fn bindings_of(contract: &EngineeringContract) -> Vec<String> {
        contract
            .proof_obligations
            .iter()
            .flat_map(|obligation| match &obligation.evidence_binding {
                EvidenceBinding::TestSelector(selector) => vec![selector.clone()],
                EvidenceBinding::TestSelectors(selectors) => selectors.clone(),
                _ => Vec::new(),
            })
            .collect()
    }

    #[test]
    fn intent_tokens_drop_stopwords_and_are_deterministic() {
        assert_eq!(
            intent_tokens(REFRESH_GOAL),
            vec![
                "race".to_string(),
                "refresh".to_string(),
                "token".to_string()
            ]
        );
        assert_eq!(intent_tokens("   "), Vec::<String>::new());
        // Deterministic order regardless of word order in the goal.
        assert_eq!(
            intent_tokens("refresh race token"),
            intent_tokens("token race refresh")
        );
    }

    #[test]
    fn refresh_token_goal_compiles_non_empty_bound_obligations() {
        let compiled = compile_contract(&request(REFRESH_GOAL, refresh_snapshot()));
        assert!(!compiled.contract.is_empty());
        assert!(!compiled.contract.proof_obligations.is_empty());
        let selectors = bindings_of(&compiled.contract);
        assert!(
            selectors.contains(&"auth::refresh::tests::replay_is_rejected".to_string()),
            "the discovered inline test must be a binding target: {selectors:?}"
        );
        assert!(
            selectors.contains(&"refresh_tokens::replayed_refresh_token_fails".to_string()),
            "the discovered integration test must be a binding target: {selectors:?}"
        );
        for obligation in &compiled.contract.proof_obligations {
            assert!(
                obligation.evidence_binding.is_bound(),
                "a compiled obligation may never carry the legacy Category binding"
            );
            obligation.evidence_binding.validate().unwrap();
            if obligation.evidence == ProofEvidenceKind::TestExecution {
                assert_eq!(obligation.requirement, Requirement::Required);
                assert_eq!(obligation.accepted_strength, AcceptedStrength::Supported);
                assert_eq!(obligation.state, ClaimState::Unknown);
                assert_eq!(obligation.origin, ContractOrigin::Native);
            }
        }
        // A discovered test obligation is bound to the exact selector (plus
        // the runtime's derived filter when the selector provably contains
        // it).
        let replay = compiled
            .contract
            .proof_obligations
            .iter()
            .find(|o| o.claim.contains("auth::refresh::tests::replay_is_rejected"))
            .expect("the replay test obligation");
        match &replay.evidence_binding {
            EvidenceBinding::TestSelectors(targets) => {
                assert!(targets.contains(&"auth::refresh::tests::replay_is_rejected".to_string()));
                assert!(targets.contains(&"refresh".to_string()));
            }
            other => panic!("expected a test-selector binding, got {other:?}"),
        }
        // The grounded race risk is derived from the matched surface.
        assert!(compiled
            .contract
            .risks
            .iter()
            .any(|risk| risk.description.contains("race")));
        assert_eq!(compiled.build_systems, vec!["cargo".to_string()]);
        assert!(compiled
            .touched_files
            .contains(&"src/auth/refresh.rs".to_string()));
    }

    #[test]
    fn empty_repository_records_uncertainty_instead_of_fabrication() {
        let compiled = compile_contract(&request(REFRESH_GOAL, RepositorySnapshot::default()));
        assert_eq!(
            compiled.repository_acquisition,
            RepositoryAcquisition::Complete,
            "a genuinely empty (readable) repository is complete"
        );
        assert!(compiled.contract.proof_obligations.is_empty());
        assert!(
            !compiled.contract.is_empty(),
            "an uncertainty is never an empty contract"
        );
        assert!(compiled.has_uncertainty());
        assert!(compiled.uncertainties.iter().any(|uncertainty| uncertainty
            .statement
            .contains("no deterministic proof obligation")));
        // The derived statement names the objective conceptually; it must
        // not repeat the goal text (the rendered contract already carries it
        // once as GOAL, and the system head must never duplicate the prompt).
        assert!(
            !compiled
                .uncertainties
                .iter()
                .any(|uncertainty| uncertainty.statement.contains(REFRESH_GOAL)),
            "derived uncertainty must not echo the objective: {:?}",
            compiled.uncertainties
        );
        assert!(
            !compiled.uncertainties.iter().any(|uncertainty| uncertainty
                .statement
                .contains("repository analysis is partial")
                || uncertainty
                    .statement
                    .contains("repository state is unavailable")),
            "an honest empty repository claims neither partiality nor unavailability: {:?}",
            compiled.uncertainties
        );
        // Every uncertainty is durable as a risk hypothesis of the contract
        // (Medium disclosure, never a fabricated obligation).
        for uncertainty in &compiled.uncertainties {
            assert!(compiled
                .contract
                .risks
                .iter()
                .any(|risk| risk.id == uncertainty.id));
        }
        assert!(compiled.contract.constraints.is_empty());
        assert!(compiled.contract.invariants.is_empty());
        // All three compiler origins are real: the source-analysis mismatch
        // (concurrency named, no code matched), the repo-analysis gap (no
        // build system) and the heuristic conclusion (nothing derivable).
        let origins: std::collections::BTreeSet<String> = compiled
            .uncertainties
            .iter()
            .map(|uncertainty| uncertainty.origin.as_str())
            .collect();
        assert!(origins.contains("source_analysis"), "{origins:?}");
        assert!(origins.contains("repo_analysis"), "{origins:?}");
        assert!(origins.contains("heuristic"), "{origins:?}");
    }

    #[test]
    fn every_generated_statement_carries_provenance() {
        let compiled = compile_contract(&request(REFRESH_GOAL, refresh_snapshot()));
        let mut ids: Vec<String> = Vec::new();
        ids.extend(compiled.contract.constraints.iter().map(|c| c.id.clone()));
        ids.extend(compiled.contract.invariants.iter().map(|i| i.id.clone()));
        ids.extend(compiled.contract.risks.iter().map(|r| r.id.clone()));
        ids.extend(
            compiled
                .contract
                .proof_obligations
                .iter()
                .map(|o| o.id.clone()),
        );
        assert!(!ids.is_empty());
        for id in &ids {
            let entries: Vec<&StatementProvenance> = compiled
                .provenance
                .iter()
                .filter(|entry| entry.statement_id == *id)
                .collect();
            assert_eq!(entries.len(), 1, "statement {id} must carry provenance");
            assert!(entries[0].confidence_bps <= 10_000);
            assert!(!entries[0].basis.is_empty());
            assert!(matches!(
                entries[0].origin,
                StatementOrigin::SourceAnalysis
                    | StatementOrigin::RepoAnalysis
                    | StatementOrigin::Heuristic
            ));
        }
        assert_eq!(compiled.provenance.len(), ids.len());
    }

    #[test]
    fn compile_is_deterministic_and_input_order_independent() {
        let a = compile_contract(&request(REFRESH_GOAL, refresh_snapshot()));
        let b = compile_contract(&request(REFRESH_GOAL, refresh_snapshot()));
        assert_eq!(a, b);
        assert_eq!(
            serde_json::to_vec(&a.contract).unwrap(),
            serde_json::to_vec(&b.contract).unwrap(),
            "same inputs must yield a byte-identical contract"
        );
        // Shuffled file order is the same snapshot.
        let mut shuffled = refresh_snapshot();
        shuffled.files.reverse();
        let c = compile_contract(&request(REFRESH_GOAL, shuffled));
        assert_eq!(a, c);
        assert_eq!(
            serde_json::to_vec(&a.contract).unwrap(),
            serde_json::to_vec(&c.contract).unwrap()
        );
        // Changed content changes the identity digest (never the stable ids).
        let mut mutated = refresh_snapshot();
        for file in &mut mutated.files {
            if file.path == "src/auth/refresh.rs" {
                file.content = Some(file.content.clone().unwrap() + "\n// changed\n");
            }
        }
        let d = compile_contract(&request(REFRESH_GOAL, mutated));
        assert_ne!(a.repository_digest, d.repository_digest);
        assert_eq!(
            a.contract, d.contract,
            "prose changes do not re-id the contract"
        );
        // Acquisition gaps compile deterministically, and a partially
        // readable tree is never byte-identical to the fully readable one.
        let mut partial = refresh_snapshot();
        partial.acquisition = RepositoryAcquisition::Partial {
            reasons: vec![AcquisitionGap {
                kind: AcquisitionGapKind::UnreadableFile,
                path: Some("src/auth/refresh.rs".into()),
                detail: "selected file could not be read: permission denied".into(),
                proof_relevant: true,
            }],
        };
        let e = compile_contract(&request(REFRESH_GOAL, partial.clone()));
        let f = compile_contract(&request(REFRESH_GOAL, partial));
        assert_eq!(e, f, "acquisition state compiles deterministically");
        assert_ne!(
            a.repository_digest, e.repository_digest,
            "acquisition state is part of the snapshot identity"
        );
    }

    /// One finding against the request's own snapshot identity.
    fn finding(
        question: IntelligenceQuestion,
        text: &str,
        digest: &str,
        provider: &str,
    ) -> ProviderFinding {
        ProviderFinding {
            question,
            kind: question.expected_kind(),
            text: text.to_string(),
            basis: "provider engineering analysis".to_string(),
            confidence_bps: 8_000,
            repository_digest: digest.to_string(),
            provider: provider.to_string(),
            revision: Some("rev-1".to_string()),
            severity: None,
        }
    }

    #[test]
    fn finding_validation_rejects_identity_bounds_and_kind_mismatches_typed() {
        let digest = "repo2:test";
        let good = finding(
            IntelligenceQuestion::AffectedComponents,
            "auth.refresh owns token rotation",
            digest,
            "tangerine",
        );
        assert!(finding_to_statement(&good, "tangerine", digest).is_ok());

        let mut wrong_provider = good.clone();
        wrong_provider.provider = "other".into();
        let rejection = finding_to_statement(&wrong_provider, "tangerine", digest).unwrap_err();
        assert_eq!(rejection.code(), FindingDiagnosticCode::ProviderMismatch);

        let mut stale = good.clone();
        stale.repository_digest = "repo2:stale".into();
        let rejection = finding_to_statement(&stale, "tangerine", digest).unwrap_err();
        assert_eq!(
            rejection.code(),
            FindingDiagnosticCode::SnapshotMismatch,
            "a stale snapshot/revision binding is refused typed"
        );

        let mut wrong_kind = good.clone();
        wrong_kind.kind = FindingKind::Risk;
        let rejection = finding_to_statement(&wrong_kind, "tangerine", digest).unwrap_err();
        assert!(matches!(rejection, FindingRejection::KindMismatch { .. }));

        let mut oversized = good.clone();
        oversized.text = "x".repeat(MAX_FINDING_TEXT_BYTES + 1);
        assert_eq!(
            finding_to_statement(&oversized, "tangerine", digest)
                .unwrap_err()
                .code(),
            FindingDiagnosticCode::Oversized
        );
        let mut empty = good.clone();
        empty.text = "   ".into();
        assert_eq!(
            finding_to_statement(&empty, "tangerine", digest)
                .unwrap_err()
                .code(),
            FindingDiagnosticCode::InvalidFinding
        );
        let mut baseless = good.clone();
        baseless.basis = String::new();
        assert!(finding_to_statement(&baseless, "tangerine", digest).is_err());
        let mut overconfident = good.clone();
        overconfident.confidence_bps = 10_001;
        assert_eq!(
            finding_to_statement(&overconfident, "tangerine", digest)
                .unwrap_err()
                .code(),
            FindingDiagnosticCode::InvalidFinding
        );

        // A batch beyond the cap is bounded: one typed CapReached diagnostic,
        // every excess finding counted as rejected, never an error.
        let many: Vec<ProviderFinding> = (0..(MAX_FINDINGS + 5))
            .map(|i| {
                finding(
                    IntelligenceQuestion::BlastRadius,
                    &format!("risk {i}"),
                    digest,
                    "tangerine",
                )
            })
            .collect();
        let conversion = findings_to_statements("tangerine", digest, MAX_FINDINGS, &many);
        assert_eq!(conversion.statements.len(), MAX_FINDINGS);
        assert_eq!(conversion.rejected, 5);
        assert!(conversion
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == FindingDiagnosticCode::CapReached));
    }

    #[test]
    fn findings_are_additive_with_external_provenance_and_no_findings_equivalence() {
        let native_request = request(REFRESH_GOAL, refresh_snapshot());
        let native = compile_contract(&native_request);
        assert!(native.enrichments.is_empty());

        // No findings: byte-identical to the native compile.
        let empty = compile_contract_with_findings(
            &native_request,
            "tangerine",
            &FindingsResponse::default(),
        );
        assert_eq!(
            serde_json::to_vec(&empty.contract).unwrap(),
            serde_json::to_vec(&native.contract).unwrap(),
            "no findings must compile byte-identically to the native path"
        );
        assert!(empty
            .provenance
            .iter()
            .all(|entry| !matches!(entry.origin, StatementOrigin::External { .. })));

        // A consult refusal is diagnosed and never alters the contract.
        let refused = FindingsResponse {
            findings: Vec::new(),
            diagnostics: vec![FindingDiagnostic::new(
                "tangerine",
                FindingDiagnosticCode::ProviderRefused,
                "provider unavailable",
            )],
        };
        let refused_compile =
            compile_contract_with_findings(&native_request, "tangerine", &refused);
        assert_eq!(refused_compile.contract, native.contract);
        let outcome = &refused_compile.enrichments[0];
        assert_eq!(outcome.applied, 0);
        assert!(outcome
            .diagnostic
            .as_deref()
            .unwrap()
            .contains("provider_refused"));

        // Real findings answer the questions: the contract gains
        // provenance-tagged statements, NOT capability descriptions.
        let digest = native.repository_digest.clone();
        let response = FindingsResponse {
            findings: vec![
                finding(
                    IntelligenceQuestion::AffectedComponents,
                    "auth.refresh owns token rotation",
                    &digest,
                    "tangerine",
                ),
                finding(
                    IntelligenceQuestion::Invariants,
                    "issued tokens stay usable until expiry",
                    &digest,
                    "tangerine",
                ),
                finding(
                    IntelligenceQuestion::HiddenDependencies,
                    "billing depends on token introspection",
                    &digest,
                    "tangerine",
                ),
                finding(
                    IntelligenceQuestion::BlastRadius,
                    "session middleware breaks if the refresh shape changes",
                    &digest,
                    "tangerine",
                ),
                finding(
                    IntelligenceQuestion::HistoricalFailures,
                    "a prior migration double-issued refresh tokens",
                    &digest,
                    "tangerine",
                ),
            ],
            diagnostics: Vec::new(),
        };
        let enriched = compile_contract_with_findings(&native_request, "tangerine", &response);
        // Native statements are preserved verbatim.
        for obligation in &native.contract.proof_obligations {
            assert!(enriched
                .contract
                .proof_obligations
                .iter()
                .any(|o| o == obligation));
        }
        for risk in &native.contract.risks {
            assert!(enriched.contract.risks.iter().any(|r| r == risk));
        }
        // Component/dependency/blast-radius/historical-failure answers are
        // external risk hypotheses; the invariant answer becomes an
        // external invariant. Every entry names its provider.
        for (label, text) in [
            ("affected component", "auth.refresh owns token rotation"),
            (
                r#"hidden dependency"#,
                "billing depends on token introspection",
            ),
            (
                "blast radius",
                "session middleware breaks if the refresh shape changes",
            ),
            (
                "historical failure",
                "a prior migration double-issued refresh tokens",
            ),
        ] {
            let risk = enriched
                .contract
                .risks
                .iter()
                .find(|risk| risk.description.contains(text))
                .unwrap_or_else(|| panic!("missing external risk for {label}"));
            assert!(
                risk.description.starts_with(&format!("{label}: ")),
                "typed question label must survive: {}",
                risk.description
            );
            assert_eq!(
                risk.origin,
                ContractOrigin::External {
                    provider: "tangerine".into()
                }
            );
        }
        let invariant = enriched
            .contract
            .invariants
            .iter()
            .find(|invariant| invariant.statement.contains("issued tokens stay usable"))
            .expect("the invariant finding lands as an external invariant");
        assert_eq!(
            invariant.origin,
            ContractOrigin::External {
                provider: "tangerine".into()
            }
        );
        // The finer typed kinds survive in provenance.
        let external_kinds: Vec<StatementKind> = enriched
            .provenance
            .iter()
            .filter(|entry| {
                matches!(
                    entry.origin,
                    StatementOrigin::External { ref provider } if provider == "tangerine"
                )
            })
            .map(|entry| entry.kind)
            .collect();
        for expected in [
            StatementKind::Component,
            StatementKind::Invariant,
            StatementKind::Dependency,
            StatementKind::Risk,
            StatementKind::HistoricalFailure,
        ] {
            assert!(
                external_kinds.contains(&expected),
                "missing typed provenance kind {expected:?}: {external_kinds:?}"
            );
        }
        for entry in &enriched.provenance {
            if matches!(entry.origin, StatementOrigin::External { .. }) {
                assert!(entry.confidence_bps <= 10_000);
                assert!(!entry.basis.is_empty());
            }
        }
        assert_eq!(enriched.enrichments[0].applied, 5);
        assert_eq!(enriched.enrichments[0].skipped, 0);
        assert!(enriched.enrichments[0].diagnostic.is_none());
    }

    #[test]
    fn hostile_findings_are_bounded_and_never_emitted() {
        let native_request = request(REFRESH_GOAL, refresh_snapshot());
        let digest = compile_contract(&native_request).repository_digest;
        let mut findings: Vec<ProviderFinding> = vec![
            {
                let mut oversized = finding(
                    IntelligenceQuestion::AffectedComponents,
                    "oversized",
                    &digest,
                    "hostile provider",
                );
                oversized.text = "x".repeat(MAX_FINDING_TEXT_BYTES + 1);
                oversized
            },
            {
                let mut stale = finding(
                    IntelligenceQuestion::BlastRadius,
                    "stale snapshot fabrication",
                    "repo2:other",
                    "hostile provider",
                );
                stale.revision = Some("y".repeat(MAX_FINDING_REVISION_BYTES + 1));
                stale
            },
            {
                let mut wrong_provider = finding(
                    IntelligenceQuestion::BlastRadius,
                    "provider laundering",
                    &digest,
                    "somebody-else",
                );
                wrong_provider.provider = "somebody-else".into();
                wrong_provider
            },
            {
                let mut empty = finding(
                    IntelligenceQuestion::Invariants,
                    "   ",
                    &digest,
                    "hostile provider",
                );
                empty.basis = String::new();
                empty
            },
        ];
        for index in 0..(MAX_FINDINGS + 16) {
            findings.push(finding(
                IntelligenceQuestion::BlastRadius,
                &format!("hostile risk {index}"),
                &digest,
                "hostile provider",
            ));
        }
        let response = FindingsResponse {
            findings,
            diagnostics: vec![FindingDiagnostic::new(
                "hostile provider",
                FindingDiagnosticCode::Oversized,
                "a provider response payload exceeded the bound",
            )],
        };
        let compiled =
            compile_contract_with_findings(&native_request, "hostile provider", &response);
        let outcome = &compiled.enrichments[0];
        assert_eq!(outcome.provider, "hostile_provider");
        assert!(outcome.applied <= MAX_ENRICHMENT_STATEMENTS);
        assert!(outcome.applied <= MAX_RISKS);
        assert!(outcome.skipped >= 16, "the cap verdict is counted");
        assert!(outcome.diagnostic.is_some());
        assert!(compiled.contract.risks.len() <= MAX_RISKS);
        assert!(!compiled
            .contract
            .risks
            .iter()
            .any(|risk| risk.description.contains("fabrication")));
        assert!(!compiled
            .contract
            .invariants
            .iter()
            .any(|invariant| invariant.statement.trim().is_empty()));
        // Every external entry is bounded and typed; none came from the
        // stale/provider-mismatched fabrications.
        for risk in &compiled.contract.risks {
            if matches!(risk.origin, ContractOrigin::External { .. }) {
                assert!(risk.description.len() <= MAX_STATEMENT_BYTES);
                assert!(risk.description.starts_with("blast radius: "));
            }
        }
    }

    #[test]
    fn migration_artifacts_ground_invariants_and_risks_only_with_intent() {
        let snapshot = || {
            RepositorySnapshot::from_files(vec![
                RepositoryFile::with_content("Cargo.toml", "[package]\nname = \"auth\"\n"),
                RepositoryFile::with_content("migrations/0001_init.sql", "CREATE TABLE tokens;\n"),
                RepositoryFile::with_content(
                    "src/auth/refresh.rs",
                    "pub fn refresh() -> u64 { 0 }\n",
                ),
            ])
        };
        let compiled = compile_contract(&request(
            "migrate refresh tokens to a rotating schema",
            snapshot(),
        ));
        assert_eq!(compiled.contract.invariants.len(), 1);
        assert!(compiled.contract.invariants[0]
            .statement
            .contains("existing data remains readable"));
        let migration_risk = compiled
            .contract
            .risks
            .iter()
            .find(|risk| risk.description.contains("migration"))
            .expect("a migration risk");
        assert_eq!(migration_risk.severity, RiskSeverity::High);
        let provenance = compiled
            .provenance
            .iter()
            .find(|entry| entry.statement_id == migration_risk.id)
            .unwrap();
        assert_eq!(provenance.origin, StatementOrigin::RepoAnalysis);
        assert!(provenance.basis.contains("migrations/0001_init.sql"));
        // The same repository without migration intent derives no invariant.
        let calm = compile_contract(&request("refresh the token cache", snapshot()));
        assert!(calm.contract.invariants.is_empty());
        // No artifacts, intent notwithstanding: no fabricated migration claim
        // — the mismatch is disclosed as an explicit uncertainty instead.
        let artifactless = RepositorySnapshot::from_files(vec![RepositoryFile::with_content(
            "Cargo.toml",
            "[package]\nname = \"auth\"\n",
        )]);
        let uncorroborated = compile_contract(&request("migrate the schema", artifactless));
        assert!(uncorroborated.contract.invariants.is_empty());
        assert!(!uncorroborated
            .contract
            .risks
            .iter()
            .any(|risk| risk.description.contains("may corrupt or strand")));
        let mismatch = uncorroborated
            .uncertainties
            .iter()
            .find(|uncertainty| uncertainty.statement.contains("no migration artifact"))
            .expect("the intent-vs-repository mismatch is disclosed");
        assert_eq!(mismatch.origin, StatementOrigin::SourceAnalysis);
    }

    #[test]
    fn public_api_constraint_requires_explicit_compatibility_intent() {
        let snapshot = || {
            RepositorySnapshot::from_files(vec![
                RepositoryFile::with_content("Cargo.toml", "[package]\nname = \"parser\"\n"),
                RepositoryFile::with_content(
                    "src/lib.rs",
                    "// parser api entry point\npub fn parse() -> u64 { 1 }\n",
                ),
            ])
        };
        let compat = compile_contract(&request("refactor the parser api", snapshot()));
        assert_eq!(compat.contract.constraints.len(), 1);
        assert!(compat.contract.constraints[0]
            .statement
            .contains("source-compatible"));
        let entry = compat
            .provenance
            .iter()
            .find(|entry| entry.statement_id == compat.contract.constraints[0].id)
            .unwrap();
        assert_eq!(entry.origin, StatementOrigin::RepoAnalysis);
        // A fix that merely touches the same public file fabricates nothing.
        let fix = compile_contract(&request("fix the parser panic", snapshot()));
        assert!(fix.contract.constraints.is_empty());
    }

    #[test]
    fn partial_repository_discloses_state_gaps_and_never_claims_full_analysis() {
        let mut repository = refresh_snapshot();
        repository.acquisition = RepositoryAcquisition::Partial {
            reasons: vec![AcquisitionGap {
                kind: AcquisitionGapKind::InvalidUtf8,
                path: Some("src/auth/refresh.rs".into()),
                detail: "selected file is not valid UTF-8 text".into(),
                proof_relevant: true,
            }],
        };
        let compiled = compile_contract(&request(REFRESH_GOAL, repository.clone()));
        assert_eq!(compiled.repository_acquisition, repository.acquisition);
        assert!(
            compiled.has_uncertainty(),
            "a partial snapshot must never compile to a silently full contract"
        );
        let partial = compiled
            .uncertainties
            .iter()
            .find(|uncertainty| uncertainty.statement.contains("partial"))
            .expect("the partial-acquisition uncertainty");
        assert_eq!(partial.origin, StatementOrigin::RepoAnalysis);
        assert!(
            partial.basis.contains("refresh.rs"),
            "the aggregate basis names the gap: {}",
            partial.basis
        );
        let risk = compiled
            .contract
            .risks
            .iter()
            .find(|risk| risk.id == partial.id)
            .expect("the uncertainty is durable");
        assert_eq!(risk.severity, RiskSeverity::Unknown);
        // The readable files still ground obligations, but the proof-relevant
        // gap is disclosed as its own row: the two are never silently
        // conflated into a trustworthy full analysis.
        assert!(!compiled.contract.proof_obligations.is_empty());
        assert!(compiled.uncertainties.iter().any(|uncertainty| uncertainty
            .statement
            .contains("proof-relevant")
            && uncertainty.basis.contains("refresh.rs")));
    }

    #[test]
    fn unavailable_repository_is_never_analyzed_as_an_empty_repo() {
        let compiled = compile_contract(&request(
            REFRESH_GOAL,
            RepositorySnapshot {
                files: Vec::new(),
                acquisition: RepositoryAcquisition::Unavailable {
                    reason: "workspace root unreadable: permission denied".into(),
                },
            },
        ));
        assert!(compiled.repository_acquisition.is_unavailable());
        assert!(compiled.contract.proof_obligations.is_empty());
        assert!(!compiled.contract.is_empty());
        let unavailable = compiled
            .uncertainties
            .iter()
            .find(|uncertainty| {
                uncertainty
                    .statement
                    .contains("repository state is unavailable")
            })
            .expect("the unavailable-acquisition uncertainty");
        assert_eq!(unavailable.origin, StatementOrigin::RepoAnalysis);
        assert!(unavailable.basis.contains("permission denied"));
        // The heuristic conclusion names the unavailable state, not an empty
        // construction result.
        assert!(compiled.uncertainties.iter().any(|uncertainty| uncertainty
            .statement
            .contains("no deterministic proof obligation")
            && uncertainty
                .statement
                .contains("the repository state was unavailable")));
        // No row may claim the repository was analyzed: the build-system
        // disclosure names the unavailable state, not an empty scan result.
        let build = compiled
            .uncertainties
            .iter()
            .find(|uncertainty| uncertainty.statement.contains("build system"))
            .expect("the build-system uncertainty");
        assert!(
            build.statement.contains("unavailable"),
            "{}",
            build.statement
        );
        assert!(
            !compiled.uncertainties.iter().any(|uncertainty| {
                uncertainty
                    .statement
                    .contains("no migration artifact was detected")
                    || uncertainty.statement.contains("no matching code was found")
            }),
            "an unavailable repository cannot claim a search found nothing: {:?}",
            compiled.uncertainties
        );
    }

    #[test]
    fn selectors_and_statements_stay_bounded_under_hostile_volume() {
        let mut files = vec![RepositoryFile::with_content(
            "Cargo.toml",
            "[package]\nname = \"widget\"\n",
        )];
        for index in 0..100 {
            files.push(RepositoryFile::with_content(
                format!("src/widget_{index}.rs"),
                format!(
                    "#[cfg(test)]\nmod tests {{\n    #[test]\n    fn widget_case_{index}() {{}}\n}}\n"
                ),
            ));
        }
        let compiled = compile_contract(&request(
            "fix the widget bug",
            RepositorySnapshot::from_files(files),
        ));
        assert!(
            compiled.contract.proof_obligations.len()
                <= MAX_TEST_OBLIGATIONS + MAX_COMPILE_OBLIGATIONS
        );
        let test_obligations = compiled
            .contract
            .proof_obligations
            .iter()
            .filter(|obligation| obligation.evidence == ProofEvidenceKind::TestExecution)
            .count();
        assert!(test_obligations <= MAX_TEST_OBLIGATIONS);
        for obligation in &compiled.contract.proof_obligations {
            assert!(obligation.claim.len() <= MAX_STATEMENT_BYTES);
            obligation.evidence_binding.validate().unwrap();
            if let EvidenceBinding::TestSelectors(targets) = &obligation.evidence_binding {
                assert!(targets.len() <= 64);
            }
        }
        assert!(compiled.contract.risks.len() <= MAX_RISKS);
        assert!(compiled.touched_symbols.len() <= MAX_TOUCHED_SYMBOLS);
    }

    #[test]
    fn hostile_paths_goals_and_content_never_panic_and_stay_deterministic() {
        let mut files = vec![
            RepositoryFile::path_only("../etc/passwd"),
            RepositoryFile::path_only("/absolute/path.rs"),
            RepositoryFile::path_only(""),
            RepositoryFile::path_only("x".repeat(MAX_PATH_BYTES + 1)),
            RepositoryFile::with_content("src/1bad/x.rs", "#[test]\nfn t() {}\n"),
            RepositoryFile::with_content("src/huge.rs", "a".repeat(MAX_FILE_CONTENT_BYTES + 1)),
            RepositoryFile::with_content(
                "src/refresh.rs",
                "#[test]\nfn r\u{00e9}sum\u{00e9}() {}\n",
            ),
        ];
        files.push(RepositoryFile::with_content(
            "Cargo.toml",
            "[package]\nname=\"a\"\n",
        ));
        let goal = "fix the ðŸ¦€ refresh token race \0\u{fffd}";
        let a = compile_contract(&request(
            goal,
            RepositorySnapshot::from_files(files.clone()),
        ));
        files.reverse();
        let b = compile_contract(&request(goal, RepositorySnapshot::from_files(files)));
        assert_eq!(a, b);
        for obligation in &a.contract.proof_obligations {
            assert!(obligation.evidence_binding.is_bound());
        }
        assert!(a
            .touched_files
            .iter()
            .all(|path| !path.contains("..") && !path.starts_with('/')));
    }

    #[test]
    fn integration_selectors_expose_the_executable_test_name() {
        let content = r#"
#[test]
fn totals_add_up() {
    assert_eq!(1 + 1, 2);
}
"#;
        let declarations = rust_test_declarations_for_path("tests/cart.rs", content);
        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0].selector, "cart::totals_add_up");
        assert_eq!(declarations[0].executable, "totals_add_up");
        assert_eq!(declarations[0].target.as_deref(), Some("cart"));
        assert!(!declarations[0].body.is_empty());
        // A unit test's executable name IS its semantic selector.
        let unit = r#"
#[cfg(test)]
mod tests {
    #[test]
    fn adds_works() { assert_eq!(1 + 1, 2); }
}
"#;
        let declarations = rust_test_declarations_for_path("src/lib.rs", unit);
        assert_eq!(declarations[0].selector, "tests::adds_works");
        assert_eq!(declarations[0].executable, "tests::adds_works");
        assert!(declarations[0].target.is_none());
    }

    #[test]
    fn declarations_capture_the_execution_changing_attribute_stack() {
        let content = r#"
#[test]
#[should_panic(expected = "boom")]
#[ignore = "manual"]
fn totals_add_up() {
    assert_eq!(1 + 1, 2);
}
"#;
        let declarations = rust_test_declarations_for_path("tests/cart.rs", content);
        assert_eq!(declarations.len(), 1);
        assert_eq!(
            declarations[0].attributes,
            vec![
                "#[test]".to_string(),
                "#[should_panic(expected = \"boom\")]".to_string(),
                "#[ignore = \"manual\"]".to_string(),
            ]
        );
        // A stack with the attributes in a different order is still the same
        // executable declaration.
        let reordered = r#"
#[should_panic(expected = "boom")]
#[test]
#[ignore = "manual"]
fn totals_add_up() {
    assert_eq!(1 + 1, 2);
}
"#;
        let reordered = rust_test_declarations_for_path("tests/cart.rs", reordered);
        let mut left = declarations[0].attributes.clone();
        let mut right = reordered[0].attributes.clone();
        left.sort();
        right.sort();
        assert_eq!(left, right);
    }

    #[test]
    fn integration_obligation_binding_never_uses_a_vacuous_stem_filter() {
        let files = [RepositoryFile::with_content(
            "tests/cart.rs",
            "#[test]\nfn totals_add_up() { assert_eq!(1 + 1, 2); }\n",
        )];
        let refs: Vec<&RepositoryFile> = files.iter().collect();
        let discovered = discover_test_selectors(&refs);
        assert_eq!(discovered.len(), 1);
        let binding = selector_binding(&discovered[0]);
        match binding {
            EvidenceBinding::TestSelectors(targets) => {
                assert!(targets.contains(&"cart::totals_add_up".to_string()));
                assert!(
                    targets.contains(&"totals_add_up".to_string()),
                    "the executable test name must be a binding target: {targets:?}"
                );
                assert!(
                    !targets.contains(&"cart".to_string()),
                    "a file-stem filter that runs zero tests must never discharge proof: {targets:?}"
                );
            }
            other => panic!("expected a test-selector binding, got {other:?}"),
        }
    }

    #[test]
    fn discovered_selectors_are_precise_and_conservative() {
        let files = refresh_snapshot();
        let refs: Vec<&RepositoryFile> = files.files.iter().collect();
        let selectors: Vec<String> = discover_test_selectors(&refs)
            .into_iter()
            .map(|test| test.selector)
            .collect();
        assert_eq!(
            selectors,
            vec![
                "auth::refresh::tests::concurrent_refresh_yields_one_token".to_string(),
                "auth::refresh::tests::replay_is_rejected".to_string(),
                "refresh_tokens::replayed_refresh_token_fails".to_string(),
            ]
        );
        // A nested integration file is not a cargo target.
        assert!(rust_module_path("tests/auth/refresh.rs").is_none());
        assert!(rust_module_path("src/lib.rs").unwrap().is_empty());
        assert_eq!(
            rust_module_path("crates/a/src/auth/mod.rs").unwrap(),
            vec!["auth".to_string()]
        );
        assert_eq!(
            rust_module_path("crates/a/tests/refresh.rs").unwrap(),
            vec!["refresh".to_string()]
        );
        assert!(rust_module_path("scripts/build.rs").is_none());
    }

    #[test]
    fn statement_ids_are_stable_and_domain_separated() {
        let a = entry_id("r1", "same text");
        let b = entry_id("r1", "same text");
        let c = entry_id("c1", "same text");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("r1:"));
        assert_eq!(a.len(), 3 + 16);
        assert!(a[3..].chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    #[test]
    fn cargo_root_lookup_is_hoisted_once_per_compile() {
        // A full-size snapshot (the MAX_COMPILE_FILES cap) with 64 touched
        // Rust files across 64 Cargo components: the old code rebuilt the
        // Cargo-root set inside EVERY cargo_component_root call — once per
        // touched file in the obligation pass and again in the constraints
        // pass — i.e. >= 64 full 20k-file scans per compile. The
        // instrumented counter proves the hoisted build runs exactly once,
        // so old/new work-unit ratio is >= the touched-file count.
        let mut files: Vec<RepositoryFile> = Vec::with_capacity(MAX_COMPILE_FILES);
        files.push(RepositoryFile::with_content(
            "Cargo.toml",
            "[package]\nname = \"root\"\n",
        ));
        for i in 0..64 {
            files.push(RepositoryFile::with_content(
                format!("pkg{i:03}/Cargo.toml"),
                format!("[package]\nname = \"pkg{i:03}\"\n"),
            ));
            files.push(RepositoryFile::with_content(
                format!("pkg{i:03}/src/refresh.rs"),
                "pub fn refresh() {}\n",
            ));
        }
        while files.len() < MAX_COMPILE_FILES {
            files.push(RepositoryFile::path_only(format!(
                "src/vendor/f{:05}.bin",
                files.len()
            )));
        }
        CARGO_ROOT_SET_BUILDS.with(|counter| counter.set(0));
        let compiled = compile_contract(&request(
            "Fix the refresh race and refactor the public surface",
            RepositorySnapshot::from_files(files),
        ));
        let builds = CARGO_ROOT_SET_BUILDS.with(std::cell::Cell::get);
        let touched_rs = compiled
            .touched_files
            .iter()
            .filter(|path| path.ends_with(".rs"))
            .count();
        assert_eq!(
            touched_rs, 64,
            "every per-component refresh.rs must be matched: {:?}",
            compiled.touched_files
        );
        assert_eq!(
            builds, 1,
            "the Cargo-root set must be built exactly once per compile \
             ({touched_rs} touched files would have rebuilt it at least {touched_rs} times)"
        );
        assert!(
            compiled.build_systems.iter().any(|s| s == "cargo"),
            "the cargo path must actually run: {:?}",
            compiled.build_systems
        );
        assert!(
            compiled
                .contract
                .proof_obligations
                .iter()
                .any(|o| o.evidence_binding.is_bound()),
            "the cargo manifest obligations must still be derived"
        );
    }

    #[test]
    fn cargo_root_lookup_takes_the_deepest_manifest_and_tolerates_duplicates() {
        // Adversarial snapshot: nested manifests (a, a/b, a/b/c), a duplicate
        // path with/without content, and a touched path OUTSIDE the
        // snapshot. The deepest ancestor manifest must own the constraint,
        // and the duplicate/absent paths must not panic or misroot.
        let files = vec![
            RepositoryFile::with_content("Cargo.toml", "[package]\nname = \"root\"\n"),
            RepositoryFile::with_content("a/Cargo.toml", "[package]\nname = \"a\"\n"),
            RepositoryFile::with_content("a/b/Cargo.toml", "[package]\nname = \"ab\"\n"),
            RepositoryFile::with_content("a/b/c/Cargo.toml", "[package]\nname = \"abc\"\n"),
            RepositoryFile::with_content("a/b/src/refresh.rs", "pub fn refresh() {}\n"),
            RepositoryFile::path_only("a/b/src/refresh.rs"),
            RepositoryFile::with_content("a/b/src/noa.rs", "pub fn rename_a() {}\n"),
        ];
        let compiled = compile_contract(&request(
            "Rename the public api in refresh",
            RepositorySnapshot::from_files(files),
        ));
        let deepest = |root: &str| {
            compiled
                .contract
                .proof_obligations
                .iter()
                .any(|o| match &o.evidence_binding {
                    EvidenceBinding::CheckIds(ids) => {
                        ids.iter().any(|id| id == &format!("{root}:rust_check"))
                    }
                    _ => false,
                })
        };
        assert!(
            deepest("a/b"),
            "the deepest manifest (a/b) owns its tree: {:?}",
            compiled
                .contract
                .proof_obligations
                .iter()
                .map(|o| o.evidence_binding.clone())
                .collect::<Vec<_>>()
        );
        // The duplicate path (content-bearing first, path-only second) never
        // double-counts or panics: the compile stays deterministic under any
        // input order (the duplicate pair keeps its relative order — the
        // digest is order-sensitive only for genuinely unordered input).
        let again = compile_contract(&request(
            "Rename the public api in refresh",
            RepositorySnapshot::from_files(vec![
                RepositoryFile::with_content("a/b/src/noa.rs", "pub fn rename_a() {}\n"),
                RepositoryFile::with_content("a/b/src/refresh.rs", "pub fn refresh() {}\n"),
                RepositoryFile::path_only("a/b/src/refresh.rs"),
                RepositoryFile::with_content("a/b/c/Cargo.toml", "[package]\nname = \"abc\"\n"),
                RepositoryFile::with_content("a/b/Cargo.toml", "[package]\nname = \"ab\"\n"),
                RepositoryFile::with_content("a/Cargo.toml", "[package]\nname = \"a\"\n"),
                RepositoryFile::with_content("Cargo.toml", "[package]\nname = \"root\"\n"),
            ]),
        ));
        assert_eq!(compiled, again, "input ordering must not change output");
        // The duplicate pair never doubles the constraint counts.
        assert_eq!(compiled.contract.constraints.len(), 1, "{compiled:?}");
    }

    #[test]
    fn an_explicit_breaking_rename_does_not_compile_a_compatibility_constraint() {
        let compiled = compile_contract(&request(
            "Rename the public helper compute_total to total_cents. No occurrence of the old name may remain.",
            RepositorySnapshot::from_files(vec![
                RepositoryFile::with_content(
                    "Cargo.toml",
                    "[package]\nname = \"invoicing\"\n",
                ),
                RepositoryFile::with_content(
                    "src/money.rs",
                    "pub fn compute_total(cents: &[i64]) -> i64 { cents.iter().sum() }\n",
                ),
            ]),
        ));
        assert!(
            compiled
                .contract
                .constraints
                .iter()
                .all(|constraint| !constraint.statement.contains("source-compatible")),
            "a breaking rename must not demand source compatibility: {:?}",
            compiled.contract.constraints
        );
    }

    #[test]
    fn same_component_tests_are_obligations_even_when_the_goal_does_not_name_them() {
        // The user says "fix the off by one"; the test is named
        // `boundary_case` and lives in tests/boundary.rs. The old
        // intent-token filter dropped it, so the contract reached the model
        // with no executable proof.
        let compiled = compile_contract(&request(
            "fix the off by one",
            RepositorySnapshot::from_files(vec![
                RepositoryFile::with_content(
                    "Cargo.toml",
                    "[package]\nname = \"boundary\"\n",
                ),
                RepositoryFile::with_content(
                    "src/lib.rs",
                    "pub fn clamp_index(index: usize, len: usize) -> usize { index.min(len) }\n",
                ),
                RepositoryFile::with_content(
                    "tests/boundary.rs",
                    "use boundary::clamp_index;\n\n#[test]\nfn boundary_case() {\n    assert_eq!(clamp_index(3, 3), 3);\n}\n",
                ),
            ]),
        ));
        let claims: Vec<&str> = compiled
            .contract
            .proof_obligations
            .iter()
            .map(|obligation| obligation.claim.as_str())
            .collect();
        assert!(
            claims.iter().any(|claim| claim.contains("boundary_case")),
            "the same-package test must be a proof obligation: {claims:?}"
        );
        assert!(
            claims.iter().any(|claim| claim.contains("still compiles")),
            "a cargo workspace with no intent-matched path must still require a compile: {claims:?}"
        );
    }
}
