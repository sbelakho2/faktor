//! Drive-start baseline engineering-contract compilation: the bounded
//! repository scanner and the production wiring around
//! [`faktor_core::contract_compile`] (audit item 5).
//!
//! The pure compiler lives in `faktor-core`; this module owns the I/O half:
//! a deterministic, bounded walk of the session's effective workspace root
//! that yields a [`RepositorySnapshot`], the asynchronous bounded findings
//! consult of the OPTIONAL system-intelligence provider, and the drive-start
//! hook that compiles the baseline contract from the task goal + that
//! snapshot and merges it durably through
//! [`SessionHandle::merge_task_contract`].
//!
//! Contract of the wiring:
//! * It runs at most once per task: only when the durable row still carries
//!   NO contract at all. A compiled contract (even an uncertainty-only one)
//!   is durable, and a deterministic re-compile would merge to a no-op.
//! * It is best-effort and never fails a turn: an unresolvable root, an
//!   unreadable store, a refused merge or a refusing/timing-out provider is
//!   logged with its typed reason (the provider one as a typed finding
//!   diagnostic) and retried on the next drive.
//! * It scans deterministically: sorted directory entries, sorted candidate
//!   paths, bounded depth/entries/reads/bytes; two scans of an unchanged
//!   tree produce byte-identical snapshots.
//! * It asks deterministically: one bounded consult of the fixed question
//!   set per drive-start compile, wall-bounded and identity-bound; findings
//!   are validated data and can only ever ADD `external:<provider>`
//!   statements to the pure compiler's output.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use faktor_core::contract_compile::{
    compile_contract, compile_contract_with_findings_checked, AcquisitionGap, AcquisitionGapKind,
    ContractCompileRequest, FindingDiagnostic, FindingDiagnosticCode, FindingsRequest,
    FindingsResponse, IntelligenceQuestion, RepositoryAcquisition, RepositoryFile,
    RepositorySnapshot, SystemIntelligenceProvider, MAX_FINDINGS, MAX_FINDING_DIAGNOSTICS,
};
use faktor_core::{CancellationToken, Deadline};
use faktor_session::{SessionHandle, Task};

use super::AgentRuntime;

/// Hard wall bound of ONE findings consult: a parked provider is dropped,
/// diagnosed and the native contract stands (never a stalled or failed turn).
const FINDINGS_CONSULT_MAX_WAIT: std::time::Duration = std::time::Duration::from_millis(2_000);

/// Longest directory depth walked.
const MAX_SCAN_DEPTH: usize = 12;
/// Hard bound on directory entries visited in one scan.
const MAX_SCAN_ENTRIES: usize = 50_000;
/// Hard bound on candidate files kept in the snapshot.
const MAX_SCAN_FILES: usize = 20_000;
/// Hard bound on source files whose CONTENT is read.
const MAX_SCAN_READ_FILES: usize = 256;
/// Hard bound on manifest files whose content is read.
const MAX_SCAN_MANIFEST_FILES: usize = 64;
/// Hard bound on ONE file's content (bytes).
const MAX_SCAN_FILE_BYTES: u64 = 256 * 1024;
/// Hard bound on the total content read (bytes).
const MAX_SCAN_TOTAL_BYTES: u64 = 8 * 1024 * 1024;
/// Hard bound on typed acquisition gaps kept in one snapshot (the last slot
/// becomes an explicit "further gaps omitted" marker when exceeded).
const MAX_SCAN_GAPS: usize = 16;

/// Directories never scanned (build output, VCS, caches, vendored deps).
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    "dist",
    "build",
    ".next",
    ".cache",
    "vendor",
    ".idea",
    ".vscode",
    ".gradle",
    ".mypy_cache",
    ".pytest_cache",
];

/// Manifest file names that classify a path as a build-system manifest.
const MANIFEST_NAMES: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "go.mod",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "CMakeLists.txt",
    "Makefile",
    "makefile",
    "GNUmakefile",
    "meson.build",
    "build.ninja",
    "BUILD",
    "BUILD.bazel",
    "MODULE.bazel",
    "WORKSPACE",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
];

/// Source/test/migration file extensions eligible for content reads.
const SOURCE_EXTENSIONS: &[&str] = &[".rs", ".sql", ".py", ".ts", ".tsx", ".js", ".go"];

struct Candidate {
    rel: String,
    size: u64,
    is_manifest: bool,
    is_source: bool,
    test_related: bool,
    token_match: bool,
}

/// Bounded deterministic repository scan for the compiler.
///
/// Acquisition is reported TRUTHFULLY: an unreadable ROOT yields
/// [`RepositoryAcquisition::Unavailable`] (never an empty complete
/// snapshot); a nested unreadable directory, a walk cap or a selected-file
/// content failure (I/O error, invalid UTF-8, oversized, budget) yields
/// [`RepositoryAcquisition::Partial`] with typed reasons. A genuinely empty
/// READABLE repository is [`RepositoryAcquisition::Complete`] with no files:
/// "verified there is nothing" and "could not read what is here" are never
/// conflated.
pub(crate) fn scan_repository_snapshot(root: &Path, tokens: &[String]) -> RepositorySnapshot {
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut stack: Vec<(PathBuf, String, usize)> = vec![(root.to_path_buf(), String::new(), 0)];
    let mut gaps: Vec<AcquisitionGap> = Vec::new();
    let mut entries_seen = 0usize;
    let mut stop = false;
    while let Some((dir, rel_dir, depth)) = stack.pop() {
        if stop {
            break;
        }
        if depth > MAX_SCAN_DEPTH {
            push_gap(
                &mut gaps,
                AcquisitionGap {
                    kind: AcquisitionGapKind::WalkTruncated,
                    path: Some(rel_dir.clone()),
                    detail: format!("directory depth exceeds {MAX_SCAN_DEPTH}"),
                    proof_relevant: false,
                },
            );
            continue;
        }
        let read = match std::fs::read_dir(&dir) {
            Ok(read) => read,
            Err(error) => {
                if rel_dir.is_empty() {
                    // The ROOT could not be read: the repository surface is
                    // UNKNOWN, never an empty repository.
                    return RepositorySnapshot {
                        files: Vec::new(),
                        acquisition: RepositoryAcquisition::Unavailable {
                            reason: format!("workspace root {root:?} could not be read: {error}"),
                        },
                    };
                }
                push_gap(
                    &mut gaps,
                    AcquisitionGap {
                        kind: AcquisitionGapKind::UnreadableDirectory,
                        path: Some(rel_dir.clone()),
                        detail: format!("directory could not be read: {error}"),
                        proof_relevant: false,
                    },
                );
                continue;
            }
        };
        let mut entries: Vec<(String, bool, u64)> = Vec::new();
        for entry in read {
            entries_seen += 1;
            if entries_seen > MAX_SCAN_ENTRIES {
                push_gap(
                    &mut gaps,
                    AcquisitionGap {
                        kind: AcquisitionGapKind::WalkTruncated,
                        path: None,
                        detail: format!(
                            "entry cap of {MAX_SCAN_ENTRIES} reached; the file list is incomplete"
                        ),
                        proof_relevant: false,
                    },
                );
                stop = true;
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    push_gap(
                        &mut gaps,
                        AcquisitionGap {
                            kind: AcquisitionGapKind::UnreadableEntry,
                            path: Some(rel_dir.clone()),
                            detail: format!("directory entry could not be read: {error}"),
                            proof_relevant: false,
                        },
                    );
                    continue;
                }
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    push_gap(
                        &mut gaps,
                        AcquisitionGap {
                            kind: AcquisitionGapKind::UnreadableEntry,
                            path: Some(join_rel(&rel_dir, &name)),
                            detail: format!("entry type could not be inspected: {error}"),
                            proof_relevant: false,
                        },
                    );
                    continue;
                }
            };
            // Symlinks are never followed: no cycles, no escaping the root.
            if file_type.is_dir() {
                if SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                entries.push((name, true, 0));
            } else if file_type.is_file() {
                let mut size = 0u64;
                match entry.metadata() {
                    Ok(meta) => size = meta.len(),
                    Err(error) => push_gap(
                        &mut gaps,
                        AcquisitionGap {
                            kind: AcquisitionGapKind::UnreadableEntry,
                            path: Some(join_rel(&rel_dir, &name)),
                            detail: format!("file metadata could not be read: {error}"),
                            proof_relevant: false,
                        },
                    ),
                }
                entries.push((name, false, size));
            }
        }
        entries.sort();
        for (name, is_dir, size) in entries.into_iter().rev() {
            let rel = join_rel(&rel_dir, &name);
            if is_dir {
                stack.push((dir.join(&name), rel, depth + 1));
                continue;
            }
            let is_manifest = MANIFEST_NAMES.contains(&name.as_str())
                || name.ends_with(".csproj")
                || name.ends_with(".sln");
            let is_source = SOURCE_EXTENSIONS
                .iter()
                .any(|extension| name.ends_with(extension));
            if !is_manifest && !is_source {
                continue;
            }
            let lower = rel.to_ascii_lowercase();
            let stem = name.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(&name);
            let test_related = lower
                .split('/')
                .any(|component| component == "tests" || component == "test")
                || stem.contains("test");
            let token_match = tokens.iter().any(|token| lower.contains(token.as_str()));
            candidates.push(Candidate {
                rel,
                size,
                is_manifest,
                is_source,
                test_related,
                token_match,
            });
        }
    }
    candidates.sort_by(|a, b| a.rel.cmp(&b.rel));
    candidates.dedup_by(|a, b| a.rel == b.rel);
    if candidates.len() > MAX_SCAN_FILES {
        push_gap(
            &mut gaps,
            AcquisitionGap {
                kind: AcquisitionGapKind::WalkTruncated,
                path: None,
                detail: format!(
                    "candidate cap of {MAX_SCAN_FILES} reached; the file list is incomplete"
                ),
                proof_relevant: false,
            },
        );
        candidates.truncate(MAX_SCAN_FILES);
    }
    let mut content: BTreeMap<String, String> = BTreeMap::new();
    let mut budget = MAX_SCAN_TOTAL_BYTES;
    for candidate in candidates.iter().filter(|candidate| candidate.is_manifest) {
        if content.len() >= MAX_SCAN_MANIFEST_FILES {
            break;
        }
        if let Err(failure) = read_content(
            &root.join(&candidate.rel),
            candidate.size,
            &mut budget,
            &mut content,
            &candidate.rel,
        ) {
            // A manifest is build-system evidence: its missing content is
            // always proof-relevant.
            push_gap(&mut gaps, content_gap(&candidate.rel, failure, true));
        }
    }
    // Content reads are SELECTIVE by design (path/test relevance first,
    // then bounded): a full-content read of a large repository is not a
    // goal of the scan, and the compiler's uncertainty rows disclose when no
    // evidence could be derived from what was read. A selected file whose
    // content FAILS to land (error, invalid UTF-8, oversized, budget) is a
    // gap, never a silent skip.
    let mut sources: Vec<&Candidate> = candidates
        .iter()
        .filter(|candidate| candidate.is_source && !candidate.is_manifest)
        .collect();
    sources.sort_by(|a, b| {
        (!a.token_match, !a.test_related, a.rel.as_str()).cmp(&(
            !b.token_match,
            !b.test_related,
            b.rel.as_str(),
        ))
    });
    let mut reads = 0usize;
    for candidate in sources {
        if reads >= MAX_SCAN_READ_FILES || budget == 0 {
            break;
        }
        match read_content(
            &root.join(&candidate.rel),
            candidate.size,
            &mut budget,
            &mut content,
            &candidate.rel,
        ) {
            Ok(()) => reads += 1,
            Err(failure) => {
                let proof_relevant = candidate.token_match || candidate.test_related;
                push_gap(
                    &mut gaps,
                    content_gap(&candidate.rel, failure, proof_relevant),
                );
            }
        }
    }
    let files: Vec<RepositoryFile> = candidates
        .into_iter()
        .map(|candidate| RepositoryFile {
            path: candidate.rel.clone(),
            content: content.remove(&candidate.rel),
        })
        .collect();
    let acquisition = if gaps.is_empty() {
        RepositoryAcquisition::Complete
    } else {
        RepositoryAcquisition::Partial { reasons: gaps }
    };
    RepositorySnapshot { files, acquisition }
}

/// Bounded acquisition-gap reporting: once the cap is reached the last slot
/// becomes an explicit "further gaps omitted" marker instead of silently
/// dropping reasons.
fn push_gap(gaps: &mut Vec<AcquisitionGap>, gap: AcquisitionGap) {
    if gaps.len() < MAX_SCAN_GAPS {
        gaps.push(gap);
        return;
    }
    gaps[MAX_SCAN_GAPS - 1] = AcquisitionGap {
        kind: AcquisitionGapKind::WalkTruncated,
        path: None,
        detail: format!(
            "further acquisition gaps omitted at the {MAX_SCAN_GAPS}-gap reporting cap"
        ),
        proof_relevant: false,
    };
}

fn join_rel(rel_dir: &str, name: &str) -> String {
    if rel_dir.is_empty() {
        name.to_string()
    } else {
        format!("{rel_dir}/{name}")
    }
}

/// Why a selected file's content is missing from the snapshot.
enum ContentFailure {
    TooLarge,
    BudgetExhausted,
    Io(String),
    InvalidUtf8,
}

/// Turn one selected-file content failure into a typed acquisition gap.
fn content_gap(rel: &str, failure: ContentFailure, proof_relevant: bool) -> AcquisitionGap {
    let (kind, detail) = match failure {
        ContentFailure::TooLarge => (
            AcquisitionGapKind::OversizedFile,
            format!(
                "selected file exceeds the {MAX_SCAN_FILE_BYTES}-byte per-file content bound and was not analyzed"
            ),
        ),
        ContentFailure::BudgetExhausted => (
            AcquisitionGapKind::ReadBudgetExhausted,
            format!(
                "the {MAX_SCAN_TOTAL_BYTES}-byte total content budget was exhausted before this selected file was read"
            ),
        ),
        ContentFailure::Io(error) => (
            AcquisitionGapKind::UnreadableFile,
            format!("selected file could not be read: {error}"),
        ),
        ContentFailure::InvalidUtf8 => (
            AcquisitionGapKind::InvalidUtf8,
            "selected file is not valid UTF-8 text and was not analyzed".to_string(),
        ),
    };
    AcquisitionGap {
        kind,
        path: Some(rel.to_string()),
        detail,
        proof_relevant,
    }
}

/// Read one file's content into `content` when it fits the per-file and total
/// budgets and is valid UTF-8. An `Err` names WHY the selected content is
/// missing so the caller can mark the snapshot partial.
fn read_content(
    path: &Path,
    size: u64,
    budget: &mut u64,
    content: &mut BTreeMap<String, String>,
    rel: &str,
) -> Result<(), ContentFailure> {
    if size > MAX_SCAN_FILE_BYTES {
        return Err(ContentFailure::TooLarge);
    }
    if *budget < size {
        return Err(ContentFailure::BudgetExhausted);
    }
    let bytes = std::fs::read(path).map_err(|error| ContentFailure::Io(error.to_string()))?;
    if bytes.len() as u64 > MAX_SCAN_FILE_BYTES {
        return Err(ContentFailure::TooLarge);
    }
    let text = String::from_utf8(bytes).map_err(|_| ContentFailure::InvalidUtf8)?;
    if text.len() as u64 > *budget {
        return Err(ContentFailure::BudgetExhausted);
    }
    *budget -= text.len() as u64;
    content.insert(rel.to_string(), text);
    Ok(())
}

impl AgentRuntime {
    /// Drive-start baseline contract compilation (audit item 5): when the
    /// durable task row carries NO contract, compile one from the goal +
    /// bounded repository state and merge it durably. Best-effort: every
    /// failure is logged with its typed reason and retried on the next
    /// drive; the turn never fails because of it.
    ///
    /// BEFORE the compile the runtime asks the OPTIONAL system-intelligence
    /// provider the fixed bounded engineering questions (asynchronous, wall
    /// bounded, cancellation/deadline carrying): the answers are validated
    /// DATA converted into `external:<provider>` enrichment statements and
    /// fed into the PURE [`compile_contract_with_findings_checked`] entry
    /// (the raw apply is private; the checked wrapper enforces the
    /// `organization_contracts` entitlement). A refusal,
    /// timeout, oversized or invalid answer is dropped with a typed
    /// diagnostic and the native contract stands byte-identically.
    pub(crate) async fn compile_baseline_contract(&self, handle: &SessionHandle, task: &Task) {
        let session = handle.id();
        // Defensive boundary: the contract is a durable, cacheable-prefix
        // artifact, so it is compiled from the REDACTED semantic goal even
        // when a legacy task row still carries a raw prompt.
        let goal = crate::runtime::durable_goal_text(&task.goal);
        if goal.trim().is_empty() {
            return;
        }
        let root = match self.deps.session.resolve_workspace_root(session) {
            Ok(Some(root)) => root,
            Ok(None) => {
                tracing::debug!(
                    session = %session,
                    "baseline contract compilation skipped: no workspace root"
                );
                return;
            }
            Err(error) => {
                tracing::warn!(
                    session = %session,
                    "baseline contract compilation skipped: workspace root unreadable: {error}"
                );
                return;
            }
        };
        let tokens = faktor_core::contract_compile::intent_tokens(&goal);
        let repository = scan_repository_snapshot(&root, &tokens);
        let request = ContractCompileRequest {
            goal: goal.clone(),
            repository,
        };
        // The native compile establishes the neutral identity (intent
        // tokens, build systems, repository digest, native contract) the
        // questions are built from; it is pure and bounded.
        let native = compile_contract(&request);
        let provider = self.system_intelligence_provider();
        let compiled = match &provider {
            Some(provider) => {
                // The organization-contract enrichment BOUNDARY: consulting
                // the org's system-intelligence provider (and applying its
                // findings) is the paid `organization_contracts`
                // augmentation. The gate is consulted BEFORE the provider
                // side effect; a denial is a typed refusal logged loudly and
                // the native contract stands (the local product path).
                match self
                    .value_gate()
                    .check(faktor_core::entitlement::ValueEntitlement::OrganizationContracts)
                {
                    Ok(()) => {
                        let response = self
                            .ask_system_intelligence(
                                handle,
                                &root,
                                &request,
                                &native,
                                provider.as_ref(),
                            )
                            .await;
                        // The ONE public apply entry: the org-contract
                        // entitlement is an ENFORCED parameter (the check
                        // above gates the provider side effect; this one
                        // gates the apply itself, defense in depth).
                        match compile_contract_with_findings_checked(
                            &request,
                            provider.provider_id(),
                            &response,
                            &self.value_gate(),
                        ) {
                            Ok(compiled) => compiled,
                            Err(refusal) => {
                                tracing::warn!(
                                    session = %session,
                                    task_id = task.task_id.raw(),
                                    entitlement = %refusal.entitlement,
                                    hook = %refusal.hook,
                                    cause = %refusal.cause,
                                    "organization contract enrichment refused typed at the apply boundary; the native contract stands"
                                );
                                native
                            }
                        }
                    }
                    Err(refusal) => {
                        tracing::warn!(
                            session = %session,
                            task_id = task.task_id.raw(),
                            entitlement = %refusal.entitlement,
                            hook = %refusal.hook,
                            cause = %refusal.cause,
                            plan = ?refusal.plan_id,
                            "organization contract enrichment refused typed; the native contract stands"
                        );
                        native
                    }
                }
            }
            // No provider installed: the exact native contract bytes.
            None => native,
        };
        for outcome in &compiled.enrichments {
            match &outcome.diagnostic {
                Some(diagnostic) => tracing::warn!(
                    session = %session,
                    task_id = task.task_id.raw(),
                    provider = %outcome.provider,
                    applied = outcome.applied,
                    skipped = outcome.skipped,
                    diagnostic = %diagnostic,
                    "system-intelligence findings left the native contract untouched"
                ),
                None if outcome.applied > 0 => tracing::info!(
                    session = %session,
                    task_id = task.task_id.raw(),
                    provider = %outcome.provider,
                    applied = outcome.applied,
                    skipped = outcome.skipped,
                    "native baseline engineering contract additively enriched with findings"
                ),
                None => {}
            }
        }
        // Fresh-context design readiness without another agent/model call:
        // inspect only the compiled contract + bounded compiler facts. The
        // audit is ADVISORY and EPHEMERAL: its findings are never applied to
        // the durable contract, so they create no risk rows, artifacts,
        // evidence, model/Tangerine calls or second source of truth. Tangerine
        // has already enriched through the single existing neutral call; Kiwi
        // remains downstream independent verification of neutral claims only.
        // A calm/empty contract can produce neither findings nor focus
        // terms: return BEFORE the audit so the ordinary no-signal compile
        // path performs zero readiness work.
        if compiled.contract.is_empty() {
            return;
        }
        let readiness = faktor_core::audit_compiled_design(&compiled);
        if !readiness.findings.is_empty() {
            tracing::info!(
                session = %session,
                task_id = task.task_id.raw(),
                findings = readiness.findings.len(),
                focus_terms = readiness.focus_terms.len(),
                "fresh-context design readiness reported advisory findings (not persisted)"
            );
        }
        match handle.merge_task_contract(task.task_id, &compiled.contract) {
            Ok(_) => tracing::info!(
                session = %session,
                task_id = task.task_id.raw(),
                obligations = compiled.contract.proof_obligations.len(),
                constraints = compiled.contract.constraints.len(),
                invariants = compiled.contract.invariants.len(),
                risks = compiled.contract.risks.len(),
                uncertainties = compiled.uncertainties.len(),
                build_systems = %compiled.build_systems.join(","),
                acquisition = compiled.repository_acquisition.label(),
                repository = %compiled.repository_digest,
                "baseline engineering contract compiled from intent + repository state"
            ),
            Err(error) => tracing::warn!(
                session = %session,
                task_id = task.task_id.raw(),
                "baseline engineering contract could not be merged: {error}"
            ),
        }
    }

    /// The bounded asynchronous findings consult: build the explicit
    /// operation envelope, ask the provider the fixed question set under a
    /// wall bound, and reduce every outcome (answers, refusal, timeout,
    /// oversized/invalid findings) to a bounded [`FindingsResponse`]. Total:
    /// no path here fails the drive, and no provider failure can alter the
    /// native contract.
    async fn ask_system_intelligence(
        &self,
        handle: &SessionHandle,
        root: &Path,
        request: &ContractCompileRequest,
        native: &faktor_core::contract_compile::CompiledContract,
        provider: &dyn SystemIntelligenceProvider,
    ) -> FindingsResponse {
        let provider_id = provider.provider_id();
        let workspace = match handle.row() {
            Ok(row) => row.workspace_id,
            Err(error) => {
                return refused(
                    provider_id,
                    FindingDiagnosticCode::ProviderUnavailable,
                    format!("session workspace identity unavailable: {error}"),
                );
            }
        };
        let operation_id = match self.deps.session.try_next_op_id() {
            Ok(id) => id,
            Err(error) => {
                return refused(
                    provider_id,
                    FindingDiagnosticCode::ProviderUnavailable,
                    format!("operation id allocation failed: {error}"),
                );
            }
        };
        let started_ms = self.deps.clock.now_ms();
        let revision = crate::semantic_tool::semantic_source_revision(Some(root));
        let findings_request = FindingsRequest {
            operation_id,
            session_id: handle.id(),
            workspace,
            started_ms,
            deadline: Some(Deadline::at(
                started_ms.saturating_add(FINDINGS_CONSULT_MAX_WAIT.as_millis() as i64),
            )),
            cancellation: CancellationToken::new(),
            source_revision: revision.as_deref(),
            goal: &request.goal,
            intent_tokens: &native.intent_tokens,
            build_systems: &native.build_systems,
            repository_digest: &native.repository_digest,
            native_contract: &native.contract,
            questions: &IntelligenceQuestion::ALL,
            max_findings: MAX_FINDINGS,
        };
        match tokio::time::timeout(
            FINDINGS_CONSULT_MAX_WAIT,
            provider.findings(&findings_request),
        )
        .await
        {
            Ok(Ok(mut response)) => {
                if response.findings.len() > MAX_FINDINGS {
                    let excess = response.findings.len() - MAX_FINDINGS;
                    response.findings.truncate(MAX_FINDINGS);
                    if response.diagnostics.len() < MAX_FINDING_DIAGNOSTICS {
                        response.diagnostics.push(FindingDiagnostic::new(
                            provider_id,
                            FindingDiagnosticCode::CapReached,
                            format!(
                                "{excess} findings beyond the {MAX_FINDINGS} bound were dropped"
                            ),
                        ));
                    }
                }
                response.diagnostics.truncate(MAX_FINDING_DIAGNOSTICS);
                response
            }
            Ok(Err(refusal)) => refused(
                provider_id,
                FindingDiagnosticCode::ProviderRefused,
                format!("provider refused: {refusal}"),
            ),
            Err(_) => refused(
                provider_id,
                FindingDiagnosticCode::ProviderTimedOut,
                format!(
                    "consult exceeded the {}ms wall budget; the native contract stands",
                    FINDINGS_CONSULT_MAX_WAIT.as_millis()
                ),
            ),
        }
    }
}

/// One-finding-diagnostic response for a consult that produced no findings.
fn refused(provider: &str, code: FindingDiagnosticCode, detail: String) -> FindingsResponse {
    FindingsResponse {
        findings: Vec::new(),
        diagnostics: vec![FindingDiagnostic::new(provider, code, detail)],
    }
}
