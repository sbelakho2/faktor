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
//! `docs/specs/autonomous-engineering.md`) may ENRICH the same IR: enrichment
//! is strictly ADDITIVE, every enriched statement carries
//! `external:<provider>` provenance, and a missing, refusing or empty
//! provider leaves the native contract byte-identical (Tangerine is never
//! required).
//!
//! Statement ids replicate [`faktor_session`](../../session)'s user-authored
//! id scheme (`faktor.contract.{c1,i1,r1}.v1` + text, BLAKE3, 16 hex chars)
//! so the durable union merge dedupes a compiled statement against an
//! identical user-authored one instead of duplicating it. Proof obligation
//! ids come from [`EngineeringContract::obligation_id`].

use std::collections::BTreeSet;

use crate::contract::{
    AcceptedStrength, Constraint, ContractOrigin, EngineeringContract, EvidenceBinding, Invariant,
    ProofEvidenceKind, Requirement, RiskHypothesis, RiskSeverity,
};

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
/// Longest enrichment diagnostic kept.
const MAX_DIAGNOSTIC_BYTES: usize = 200;
/// Longest goal excerpt embedded in an uncertainty statement.
const MAX_GOAL_EXCERPT: usize = 120;
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

/// A bounded snapshot of repository state the compiler reasons over.
/// `complete` is the caller's completeness verdict: `false` means discovery
/// refused or truncated, and the compiler then records an explicit
/// uncertainty instead of silently analyzing a partial tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositorySnapshot {
    pub files: Vec<RepositoryFile>,
    pub complete: bool,
    pub incompleteness: Option<String>,
}

impl Default for RepositorySnapshot {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            complete: true,
            incompleteness: None,
        }
    }
}

impl RepositorySnapshot {
    pub fn from_files(files: Vec<RepositoryFile>) -> Self {
        Self {
            files,
            complete: true,
            incompleteness: None,
        }
    }

    pub fn from_paths(paths: impl IntoIterator<Item = String>) -> Self {
        Self::from_files(paths.into_iter().map(RepositoryFile::path_only).collect())
    }
}

/// The compiled statements' kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementKind {
    Constraint,
    Invariant,
    Risk,
    ProofObligation,
}

impl StatementKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Constraint => "constraint",
            Self::Invariant => "invariant",
            Self::Risk => "risk",
            Self::ProofObligation => "proof_obligation",
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
    /// Content identity of the snapshot the contract was compiled against.
    pub repository_digest: String,
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

/// The neutral enrichment request handed to a
/// [`SystemIntelligenceProvider`]: the native analysis results, never a
/// prescription of the answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrichmentRequest<'a> {
    pub goal: &'a str,
    pub intent_tokens: &'a [String],
    pub build_systems: &'a [String],
    pub repository_digest: &'a str,
    pub native_contract: &'a EngineeringContract,
    pub max_statements: usize,
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

/// The neutral system-intelligence seam (`docs/specs/autonomous-engineering.md`).
/// A provider may enrich the SAME IR additively; it is never required, and
/// an absent/refusing provider cannot alter the native contract.
pub trait SystemIntelligenceProvider {
    /// Stable provider id used as the `external:<provider>` provenance tag.
    fn provider_id(&self) -> &str;
    /// Bounded enrichment. `Err` is diagnosed but never alters the contract.
    fn enrich(&self, request: &EnrichmentRequest<'_>) -> Result<Vec<EnrichmentStatement>, String>;
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

/// Compile the native baseline contract (no external enrichment).
pub fn compile_contract(request: &ContractCompileRequest) -> CompiledContract {
    compile_contract_with(request, None)
}

/// Compile the native baseline contract and, when a provider is configured,
/// let it ENRICH the same IR additively. With `None` (or an empty/refusing
/// provider) the contract is byte-identical to [`compile_contract`].
pub fn compile_contract_with(
    request: &ContractCompileRequest,
    provider: Option<&dyn SystemIntelligenceProvider>,
) -> CompiledContract {
    let intent = intent_tokens(&request.goal);
    let files = normalize_files(&request.repository.files);
    let build_systems = detect_build_systems(&files);
    let selectors = discover_test_selectors(&files);
    let mut compiled = NativeCompile::run(request, &intent, &files, &build_systems, &selectors);
    if let Some(provider) = provider {
        apply_enrichment(&mut compiled, request, provider);
    }
    compiled
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
}

impl<'a> NativeCompile<'a> {
    fn run(
        request: &'a ContractCompileRequest,
        intent: &'a [String],
        files: &'a [&'a RepositoryFile],
        build_systems: &'a [String],
        selectors: &'a [DiscoveredTest],
    ) -> CompiledContract {
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
        };
        state.collect_touched_surface();
        state.compile_test_obligations();
        state.compile_compile_obligations();
        state.compile_constraints();
        state.compile_migration_statements();
        state.compile_risks();
        state.record_uncertainties();
        let repository_digest = repository_digest(&request.repository.files);
        CompiledContract {
            contract: state.contract,
            provenance: state.provenance,
            uncertainties: state.uncertainties,
            intent_tokens: intent.to_vec(),
            build_systems: build_systems.to_vec(),
            touched_files: state.touched_files,
            touched_symbols: state.touched_symbols,
            repository_digest,
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
        let mut candidates: Vec<&DiscoveredTest> = self
            .selectors
            .iter()
            .filter(|test| {
                self.intent.is_empty()
                    || matches_tokens(&test.selector.to_ascii_lowercase(), self.intent)
                    || matches_tokens(&test.path.to_ascii_lowercase(), self.intent)
            })
            .collect();
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
            roots.insert(cargo_component_root(path, &self.files));
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
        let mut by_component: std::collections::BTreeMap<String, (usize, usize, Vec<String>)> =
            std::collections::BTreeMap::new();
        for path in &self.touched_files {
            let Some(file) = self.files.iter().find(|f| f.path == *path) else {
                continue;
            };
            let Some(content) = &file.content else {
                continue;
            };
            let public_items = public_item_count(content);
            if public_items == 0 {
                continue;
            }
            let root = cargo_component_root(path, &self.files);
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
            let reason = if self.build_systems.is_empty() {
                "no build system was detected"
            } else if self.selectors.is_empty() {
                "no existing test was discovered"
            } else {
                "no existing test matches the objective"
            };
            reasons.push((
                StatementOrigin::Heuristic,
                3_000,
                format!(
                    "no deterministic proof obligation could be derived from the repository for objective {:?}: {reason}",
                    goal_excerpt(&self.request.goal)
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
            reasons.push((
                StatementOrigin::RepoAnalysis,
                3_500,
                "no build system was detected; automated verification may be unavailable for this objective".to_string(),
                "manifest detection over the bounded snapshot found no known build-system manifest".to_string(),
                RiskSeverity::Medium,
            ));
        }
        // Intent-vs-repository mismatches (source analysis): the goal names a
        // concern the bounded snapshot could not corroborate. Disclosed,
        // never silently dropped.
        if self
            .intent
            .iter()
            .any(|token| MIGRATION_TOKENS.contains(&token.as_str()))
            && detect_migration_files(&self.files).is_empty()
        {
            reasons.push((
                StatementOrigin::SourceAnalysis,
                3_000,
                "the objective names a migration, but no migration artifact was detected in the bounded snapshot".to_string(),
                "goal tokens name migration/schema; path-based migration detection found nothing".to_string(),
                RiskSeverity::Medium,
            ));
        }
        if self
            .intent
            .iter()
            .any(|token| RACE_TOKENS.contains(&token.as_str()))
            && self.touched_files.is_empty()
        {
            reasons.push((
                StatementOrigin::SourceAnalysis,
                3_000,
                "the objective names concurrency or replay, but no matching code was found in the bounded snapshot".to_string(),
                "goal tokens name concurrency/replay; the matched-file set is empty".to_string(),
                RiskSeverity::Medium,
            ));
        }
        if !self.request.repository.complete {
            reasons.push((
                StatementOrigin::RepoAnalysis,
                3_000,
                "repository analysis was truncated; deterministic coverage may be missing"
                    .to_string(),
                self.request
                    .repository
                    .incompleteness
                    .clone()
                    .unwrap_or_else(|| "the caller reported an incomplete snapshot".to_string()),
                RiskSeverity::Unknown,
            ));
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

/// Apply one provider's enrichment: validate every statement, append the
/// valid ones with `external:<provider>` provenance, and diagnose the rest.
fn apply_enrichment(
    compiled: &mut CompiledContract,
    request: &ContractCompileRequest,
    provider: &dyn SystemIntelligenceProvider,
) {
    let Some(provider_id) = sanitized_provider_id(provider.provider_id()) else {
        compiled.enrichments.push(EnrichmentOutcome {
            provider: provider.provider_id().to_string(),
            applied: 0,
            skipped: 0,
            diagnostic: Some("provider id is empty after sanitization; enrichment refused".into()),
        });
        return;
    };
    let origin = StatementOrigin::External {
        provider: provider_id.clone(),
    };
    let max_statements = MAX_ENRICHMENT_STATEMENTS;
    let enrichment_request = EnrichmentRequest {
        goal: &request.goal,
        intent_tokens: &compiled.intent_tokens,
        build_systems: &compiled.build_systems,
        repository_digest: &compiled.repository_digest,
        native_contract: &compiled.contract,
        max_statements,
    };
    let mut outcome = EnrichmentOutcome {
        provider: provider_id,
        applied: 0,
        skipped: 0,
        diagnostic: None,
    };
    let mut skip_reasons: Vec<String> = Vec::new();
    match provider.enrich(&enrichment_request) {
        Ok(statements) => {
            for statement in statements {
                if outcome.applied + outcome.skipped >= max_statements {
                    outcome.skipped += 1;
                    push_skip_reason(&mut skip_reasons, "statement cap reached");
                    continue;
                }
                match apply_enrichment_statement(compiled, &origin, statement) {
                    Ok(()) => outcome.applied += 1,
                    Err(reason) => {
                        outcome.skipped += 1;
                        push_skip_reason(&mut skip_reasons, &reason);
                    }
                }
            }
        }
        Err(error) => {
            outcome.diagnostic = Some(bounded(
                &format!("provider refused: {error}"),
                MAX_DIAGNOSTIC_BYTES,
            ));
        }
    }
    if !skip_reasons.is_empty() && outcome.diagnostic.is_none() {
        outcome.diagnostic = Some(bounded(
            &format!("skipped statements: {}", skip_reasons.join("; ")),
            MAX_DIAGNOSTIC_BYTES,
        ));
    }
    compiled.enrichments.push(outcome);
}

fn push_skip_reason(reasons: &mut Vec<String>, reason: &str) {
    if reasons.len() < 3 && !reasons.iter().any(|seen| seen == reason) {
        reasons.push(reason.to_string());
    }
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
        EnrichmentKind::Risk => {
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
                kind: StatementKind::Risk,
                origin: origin.clone(),
                confidence_bps: statement.confidence_bps.min(10_000),
                basis,
            });
            Ok(())
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

/// One discovered test selector.
struct DiscoveredTest {
    selector: String,
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
        if !file.path.ends_with(".rs") {
            continue;
        }
        let Some(content) = &file.content else {
            continue;
        };
        if content.len() > MAX_FILE_CONTENT_BYTES {
            continue;
        }
        let Some(module) = rust_module_path(&file.path) else {
            continue;
        };
        for selector in extract_test_selectors(&module, content) {
            if selector.len() <= MAX_SELECTOR_BYTES {
                out.push(DiscoveredTest {
                    selector,
                    path: file.path.clone(),
                });
            }
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

/// The cargo test selector binding for one discovered test: the exact
/// selector, plus the runtime's derived test filter (the sanitized file stem)
/// WHEN the selector provably contains it — `cargo test <filter>` runs every
/// test whose name contains the filter, so the coarse filter genuinely
/// exercises the claimed test (and the native runtime derives exactly that
/// filter for the changed file).
fn selector_binding(test: &DiscoveredTest) -> EvidenceBinding {
    let mut targets = vec![test.selector.clone()];
    if let Some(filter) = rust_test_filter(&test.path) {
        if filter != test.selector && test.selector.contains(&filter) {
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

/// The module path of one Rust file (`None` when the layout is unknown):
/// * top-level integration targets `tests/<name>.rs` (and `<pkg>/tests/...`)
///   yield `<name>`;
/// * `src/...` files yield the module path below `src/` (dropping
///   `lib.rs` / `main.rs` / `mod.rs`), empty for the crate root;
/// * anything else is unknown and yields no selectors.
fn rust_module_path(path: &str) -> Option<Vec<String>> {
    let stem = path.strip_suffix(".rs")?;
    let tests_rest = if let Some(rest) = stem.strip_prefix("tests/") {
        Some(rest)
    } else {
        stem.rfind("/tests/").map(|index| &stem[index + 7..])
    };
    if let Some(rest) = tests_rest {
        if rest.contains('/') || rest.is_empty() {
            return None;
        }
        return is_ident(rest).then(|| vec![rest.to_string()]);
    }
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

/// Extract the test selectors of one Rust file: `module::inline_mods::fn`.
fn extract_test_selectors(module: &[String], content: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut mods: Vec<(String, i32)> = Vec::new();
    let mut depth: i32 = 0;
    let mut pending_test_attr = false;
    for line in content.lines().take(MAX_SCAN_LINES) {
        let trimmed = line.trim_start();
        if is_test_attribute(trimmed) {
            pending_test_attr = true;
        } else if trimmed.starts_with("#[") {
            // Attribute stacks (`#[test]` + `#[should_panic]`) keep pending.
        } else if pending_test_attr {
            if let Some(name) = fn_name_of(trimmed) {
                let mut parts = module.to_vec();
                parts.extend(mods.iter().map(|(name, _)| name.clone()));
                parts.push(name);
                out.push(parts.join("::"));
            }
            if !trimmed.is_empty() && !trimmed.starts_with("//") {
                pending_test_attr = false;
            }
        }
        if let Some(name) = mod_open_name(trimmed) {
            mods.push((name, depth + 1));
        }
        depth += brace_delta(line);
        while let Some((_, open_depth)) = mods.last() {
            if *open_depth > depth {
                mods.pop();
            } else {
                break;
            }
        }
    }
    out.sort();
    out.dedup();
    out
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

fn brace_delta(line: &str) -> i32 {
    let mut delta = 0i32;
    for c in line.chars() {
        match c {
            '{' => delta += 1,
            '}' => delta -= 1,
            _ => {}
        }
    }
    delta
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
/// among the snapshot paths (`""` = the repository root).
fn cargo_component_root(path: &str, files: &[&RepositoryFile]) -> String {
    let manifests: BTreeSet<&str> = files
        .iter()
        .filter_map(|file| {
            file.path
                .strip_suffix("Cargo.toml")
                .map(|dir| dir.trim_end_matches('/'))
        })
        .collect();
    let mut dir = match path.rfind('/') {
        Some(index) => &path[..index],
        None => "",
    };
    loop {
        if manifests.contains(dir) {
            return dir.to_string();
        }
        match dir.rfind('/') {
            Some(index) => dir = &dir[..index],
            None => {
                if dir.is_empty() || manifests.contains("") {
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
/// against.
fn repository_digest(files: &[RepositoryFile]) -> String {
    let mut sorted: Vec<&RepositoryFile> = files.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"faktor.contract.compile.repo.v1\0");
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
    format!("repo1:{}", hasher.finalize().to_hex())
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

fn goal_excerpt(goal: &str) -> String {
    bounded(goal.trim(), MAX_GOAL_EXCERPT)
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
        assert!(compiled.contract.proof_obligations.is_empty());
        assert!(
            !compiled.contract.is_empty(),
            "an uncertainty is never an empty contract"
        );
        assert!(compiled.has_uncertainty());
        assert!(compiled.uncertainties.iter().any(|uncertainty| uncertainty
            .statement
            .contains("no deterministic proof obligation")));
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
    }

    struct NoopProvider;
    impl SystemIntelligenceProvider for NoopProvider {
        fn provider_id(&self) -> &str {
            "tangerine"
        }
        fn enrich(
            &self,
            _request: &EnrichmentRequest<'_>,
        ) -> Result<Vec<EnrichmentStatement>, String> {
            Ok(Vec::new())
        }
    }

    struct FailingProvider;
    impl SystemIntelligenceProvider for FailingProvider {
        fn provider_id(&self) -> &str {
            "tangerine"
        }
        fn enrich(
            &self,
            _request: &EnrichmentRequest<'_>,
        ) -> Result<Vec<EnrichmentStatement>, String> {
            Err("provider unavailable".into())
        }
    }

    struct AddingProvider;
    impl SystemIntelligenceProvider for AddingProvider {
        fn provider_id(&self) -> &str {
            "tangerine"
        }
        fn enrich(
            &self,
            _request: &EnrichmentRequest<'_>,
        ) -> Result<Vec<EnrichmentStatement>, String> {
            Ok(vec![
                EnrichmentStatement {
                    kind: EnrichmentKind::ProofObligation,
                    text: "the refresh endpoint rejects replayed tokens".into(),
                    confidence_bps: 9_500,
                    basis: "provider architecture analysis".into(),
                    evidence: Some(ProofEvidenceKind::TestExecution),
                    binding: Some(EvidenceBinding::TestSelector(
                        "auth::refresh::tests::replay_is_rejected".into(),
                    )),
                    severity: None,
                    advisory: false,
                },
                EnrichmentStatement {
                    kind: EnrichmentKind::Risk,
                    text: "token rotation may desynchronize legacy clients".into(),
                    confidence_bps: 8_000,
                    basis: "provider architecture analysis".into(),
                    evidence: None,
                    binding: None,
                    severity: Some(RiskSeverity::High),
                    advisory: false,
                },
                EnrichmentStatement {
                    kind: EnrichmentKind::Constraint,
                    text: "existing API clients cannot break".into(),
                    confidence_bps: 8_500,
                    basis: "provider contract analysis".into(),
                    evidence: None,
                    binding: None,
                    severity: None,
                    advisory: false,
                },
            ])
        }
    }

    #[test]
    fn enrichment_is_additive_with_external_provenance_and_no_provider_equivalence() {
        let native = compile_contract(&request(REFRESH_GOAL, refresh_snapshot()));
        let noop = compile_contract_with(
            &request(REFRESH_GOAL, refresh_snapshot()),
            Some(&NoopProvider),
        );
        assert_eq!(
            native.contract, noop.contract,
            "an empty provider changes nothing"
        );
        let failing = compile_contract_with(
            &request(REFRESH_GOAL, refresh_snapshot()),
            Some(&FailingProvider),
        );
        assert_eq!(
            native.contract, failing.contract,
            "a refusing provider never alters the contract"
        );
        assert!(failing.enrichments[0]
            .diagnostic
            .as_deref()
            .unwrap()
            .contains("provider unavailable"));
        let enriched = compile_contract_with(
            &request(REFRESH_GOAL, refresh_snapshot()),
            Some(&AddingProvider),
        );
        // Native statements are preserved byte-for-byte.
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
        // The external statements carry external:<provider> provenance.
        let external = enriched
            .contract
            .proof_obligations
            .iter()
            .find(|o| o.claim == "the refresh endpoint rejects replayed tokens")
            .expect("the enriched obligation");
        assert_eq!(
            external.origin,
            ContractOrigin::External {
                provider: "tangerine".into()
            }
        );
        let entry = enriched
            .provenance
            .iter()
            .find(|entry| entry.statement_id == external.id)
            .unwrap();
        assert_eq!(entry.origin.as_str(), "external:tangerine");
        assert!(enriched
            .contract
            .risks
            .iter()
            .any(|risk| risk.description == "token rotation may desynchronize legacy clients"));
        assert!(enriched
            .contract
            .constraints
            .iter()
            .any(|constraint| constraint.statement == "existing API clients cannot break"));
        assert_eq!(enriched.enrichments[0].applied, 3);
        assert_eq!(enriched.enrichments[0].skipped, 0);
    }

    struct HostileProvider;
    impl SystemIntelligenceProvider for HostileProvider {
        fn provider_id(&self) -> &str {
            "hostile provider"
        }
        fn enrich(
            &self,
            _request: &EnrichmentRequest<'_>,
        ) -> Result<Vec<EnrichmentStatement>, String> {
            let mut statements = vec![
                EnrichmentStatement {
                    kind: EnrichmentKind::ProofObligation,
                    text: "category-bound fabrication".into(),
                    confidence_bps: 10_000,
                    basis: String::new(),
                    evidence: Some(ProofEvidenceKind::Compile),
                    binding: Some(EvidenceBinding::Category),
                    severity: None,
                    advisory: false,
                },
                EnrichmentStatement {
                    kind: EnrichmentKind::ProofObligation,
                    text: "bindingless fabrication".into(),
                    confidence_bps: 10_000,
                    basis: String::new(),
                    evidence: Some(ProofEvidenceKind::Compile),
                    binding: None,
                    severity: None,
                    advisory: false,
                },
                EnrichmentStatement {
                    kind: EnrichmentKind::Constraint,
                    text: "   ".into(),
                    confidence_bps: 10_000,
                    basis: String::new(),
                    evidence: None,
                    binding: None,
                    severity: None,
                    advisory: false,
                },
                EnrichmentStatement {
                    kind: EnrichmentKind::Risk,
                    text: "x".repeat(MAX_STATEMENT_BYTES + 1),
                    confidence_bps: 10_000,
                    basis: String::new(),
                    evidence: None,
                    binding: None,
                    severity: None,
                    advisory: false,
                },
            ];
            for index in 0..40 {
                statements.push(EnrichmentStatement {
                    kind: EnrichmentKind::Risk,
                    text: format!("hostile risk {index}"),
                    confidence_bps: 10_000,
                    basis: "hostile".into(),
                    evidence: None,
                    binding: None,
                    severity: Some(RiskSeverity::High),
                    advisory: false,
                });
            }
            Ok(statements)
        }
    }

    #[test]
    fn hostile_enrichment_is_skipped_never_emitted() {
        let compiled = compile_contract_with(
            &request(REFRESH_GOAL, refresh_snapshot()),
            Some(&HostileProvider),
        );
        let outcome = &compiled.enrichments[0];
        assert_eq!(outcome.provider, "hostile_provider");
        assert!(outcome.applied <= MAX_ENRICHMENT_STATEMENTS);
        assert!(outcome.skipped >= 24);
        for obligation in &compiled.contract.proof_obligations {
            assert!(
                obligation.evidence_binding.is_bound(),
                "a hostile provider must never smuggle a Category binding"
            );
            assert!(!obligation.claim.contains("fabrication"));
        }
        assert!(compiled.contract.risks.len() <= MAX_RISKS);
        assert!(!compiled
            .contract
            .constraints
            .iter()
            .any(|constraint| constraint.statement.trim().is_empty()));
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
    fn incomplete_repository_records_an_unknown_severity_uncertainty() {
        let mut repository = refresh_snapshot();
        repository.complete = false;
        repository.incompleteness = Some("entry cap reached".into());
        let compiled = compile_contract(&request(REFRESH_GOAL, repository));
        let truncated = compiled
            .uncertainties
            .iter()
            .find(|uncertainty| uncertainty.statement.contains("truncated"))
            .expect("the truncation uncertainty");
        let risk = compiled
            .contract
            .risks
            .iter()
            .find(|risk| risk.id == truncated.id)
            .expect("the uncertainty is durable");
        assert_eq!(risk.severity, RiskSeverity::Unknown);
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
}
