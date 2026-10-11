//! Drive-start baseline engineering-contract compilation: the bounded
//! repository scanner and the production wiring around
//! [`faktor_core::contract_compile`] (audit item 5).
//!
//! The pure compiler lives in `faktor-core`; this module owns the I/O half:
//! a deterministic, bounded walk of the session's effective workspace root
//! that yields a [`RepositorySnapshot`], and the drive-start hook that
//! compiles the baseline contract from the task goal + that snapshot and
//! merges it durably through [`SessionHandle::merge_task_contract`].
//!
//! Contract of the wiring:
//! * It runs at most once per task: only when the durable row still carries
//!   NO contract at all. A compiled contract (even an uncertainty-only one)
//!   is durable, and a deterministic re-compile would merge to a no-op.
//! * It is best-effort and never fails a turn: an unresolvable root, an
//!   unreadable store or a refused merge is logged with its typed reason and
//!   retried on the next drive.
//! * It scans deterministically: sorted directory entries, sorted candidate
//!   paths, bounded depth/entries/reads/bytes; two scans of an unchanged
//!   tree produce byte-identical snapshots.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use faktor_core::contract_compile::{
    compile_contract, ContractCompileRequest, RepositoryFile, RepositorySnapshot,
};
use faktor_session::{SessionHandle, Task};

use super::AgentRuntime;

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
/// A missing/unreadable ROOT yields an empty COMPLETE snapshot: there is no
/// repository state to analyze, and the compiler's own "no build system / no
/// matching test" uncertainties disclose that honestly (never an escalation
/// caused by a scan that had nothing to read). A PARTIAL walk (entry/depth
/// caps) yields `complete = false` with the typed reason, because missing
/// paths can hide evidence.
pub(crate) fn scan_repository_snapshot(root: &Path, tokens: &[String]) -> RepositorySnapshot {
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut stack: Vec<(PathBuf, String, usize)> = vec![(root.to_path_buf(), String::new(), 0)];
    let mut entries_seen = 0usize;
    let mut truncated: Option<String> = None;
    let mut root_read = false;
    let mut stop = false;
    while let Some((dir, rel_dir, depth)) = stack.pop() {
        if stop {
            break;
        }
        if depth > MAX_SCAN_DEPTH {
            truncated.get_or_insert(format!(
                "directory depth exceeds {MAX_SCAN_DEPTH} at {rel_dir:?}"
            ));
            continue;
        }
        let read = match std::fs::read_dir(&dir) {
            Ok(read) => read,
            Err(error) => {
                if rel_dir.is_empty() && !root_read {
                    // No repository state to analyze; parity with an empty
                    // workspace (the compiler records its own uncertainties).
                    return RepositorySnapshot::default();
                }
                truncated.get_or_insert(format!("directory {rel_dir:?} unreadable: {error}"));
                continue;
            }
        };
        root_read = true;
        let mut entries: Vec<(String, bool, u64)> = Vec::new();
        for entry in read {
            entries_seen += 1;
            if entries_seen > MAX_SCAN_ENTRIES {
                truncated.get_or_insert(format!(
                    "entry cap of {MAX_SCAN_ENTRIES} reached; the file list is incomplete"
                ));
                stop = true;
                break;
            }
            let Ok(entry) = entry else {
                truncated.get_or_insert("a directory entry could not be read".to_string());
                continue;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            // Symlinks are never followed: no cycles, no escaping the root.
            if file_type.is_dir() {
                if SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                entries.push((name, true, 0));
            } else if file_type.is_file() {
                let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
                entries.push((name, false, size));
            }
        }
        entries.sort();
        for (name, is_dir, size) in entries.into_iter().rev() {
            let rel = if rel_dir.is_empty() {
                name.clone()
            } else {
                format!("{rel_dir}/{name}")
            };
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
        truncated.get_or_insert(format!(
            "candidate cap of {MAX_SCAN_FILES} reached; the file list is incomplete"
        ));
        candidates.truncate(MAX_SCAN_FILES);
    }
    let mut content: BTreeMap<String, String> = BTreeMap::new();
    let mut budget = MAX_SCAN_TOTAL_BYTES;
    for candidate in candidates.iter().filter(|candidate| candidate.is_manifest) {
        if content.len() >= MAX_SCAN_MANIFEST_FILES {
            break;
        }
        read_content(
            &root.join(&candidate.rel),
            candidate.size,
            &mut budget,
            &mut content,
            &candidate.rel,
        );
    }
    // Content reads are SELECTIVE by design (path/test relevance first,
    // then bounded): a full-content read of a large repository is not a
    // goal of the scan, and the compiler's uncertainty rows disclose when no
    // evidence could be derived from what was read.
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
        if read_content(
            &root.join(&candidate.rel),
            candidate.size,
            &mut budget,
            &mut content,
            &candidate.rel,
        ) {
            reads += 1;
        }
    }
    let files: Vec<RepositoryFile> = candidates
        .into_iter()
        .map(|candidate| RepositoryFile {
            path: candidate.rel.clone(),
            content: content.remove(&candidate.rel),
        })
        .collect();
    RepositorySnapshot {
        files,
        complete: truncated.is_none(),
        incompleteness: truncated,
    }
}

/// Read one file's content into `content` when it fits the per-file and total
/// budgets and is valid UTF-8. Returns TRUE when content landed.
fn read_content(
    path: &Path,
    size: u64,
    budget: &mut u64,
    content: &mut BTreeMap<String, String>,
    rel: &str,
) -> bool {
    if size > MAX_SCAN_FILE_BYTES || *budget < size {
        return false;
    }
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    if bytes.len() as u64 > MAX_SCAN_FILE_BYTES {
        return false;
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return false;
    };
    if text.len() as u64 > *budget {
        return false;
    }
    *budget -= text.len() as u64;
    content.insert(rel.to_string(), text);
    true
}

impl AgentRuntime {
    /// Drive-start baseline contract compilation (audit item 5): when the
    /// durable task row carries NO contract, compile one from the goal +
    /// bounded repository state and merge it durably. Best-effort: every
    /// failure is logged with its typed reason and retried on the next
    /// drive; the turn never fails because of it.
    pub(crate) fn compile_baseline_contract(&self, handle: &SessionHandle, task: &Task) {
        let session = handle.id();
        if task.goal.trim().is_empty() {
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
        let tokens = faktor_core::contract_compile::intent_tokens(&task.goal);
        let repository = scan_repository_snapshot(&root, &tokens);
        let compiled = compile_contract(&ContractCompileRequest {
            goal: task.goal.clone(),
            repository,
        });
        if compiled.contract.is_empty() {
            return;
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
}
