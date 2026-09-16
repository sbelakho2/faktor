//! Repository discovery that scales to real repositories without losing the
//! fail-closed completeness verdict (audit: verification must not depend on
//! a partial view of the repo).
//!
//! The previous correctness ceiling (500 files, 200 entries per directory,
//! depth 6) was too small for normal repositories: verification then
//! (correctly) refused `Passed`, but the product must actually verify such
//! repositories. Discovery is therefore bounded by RESOURCE budgets sized
//! for large repositories — never by a correctness ceiling:
//!
//! - **Git repositories** are discovered through `git ls-files -z` plus a
//!   bounded `git ls-files --others --exclude-standard -z` for untracked
//!   files, both executed through the workspace
//!   [`faktor_terminal::ProcessSupervisor`] with a per-command deadline and
//!   byte-bounded output. A successful listing is
//!   [`InventoryCompleteness::Complete`]. Exceeding the output byte budget is
//!   [`InventoryCompleteness::OutputCapped`]; a deadline or spawn failure
//!   after git was selected is [`InventoryCompleteness::GitUnavailable`]; an
//!   untracked listing longer than the configured budget is
//!   [`InventoryCompleteness::UntrackedBudgetExceeded`] WITH the excluded
//!   count. Nothing is ever silently truncated.
//! - **Non-Git** roots (git absent, not a repository, non-zero exit) use a
//!   bounded native traversal and record the typed reason in
//!   [`InventorySource`]. The traversal is bounded by RESOURCE budgets (max
//!   paths, max metadata bytes, max wall, max depth, per-directory page);
//!   exceeding any budget returns a typed non-Complete verdict.
//! - **Manifest discovery is targeted**: every directory prefix observed in
//!   the inventory is probed for the recognized manifest names
//!   ([`MANIFEST_PROBE_NAMES`]), so a manifest is never missed merely
//!   because a bounded walk stopped short of it (the frontier can be
//!   arbitrarily deep). RULE: a recognized manifest that exists on disk but
//!   is ABSENT from the inventory (gitignored, excluded by the skip set, or
//!   over a discovery budget) surfaces as
//!   [`InventoryCompleteness::ManifestExcluded`]; a content-deciding
//!   manifest (`CMakeLists.txt`/`Makefile`/`*.csproj`) larger than the
//!   derived-profile content-probe cap surfaces as
//!   [`InventoryCompleteness::ManifestOversized`], because the evidence
//!   derived from its content would silently change. Both are conservative
//!   over-approximations: the inventory drives profile derivation before
//!   changed-file mapping, so a missed manifest MAY change the derived check
//!   set, and a `Passed` verdict must never rest on it.
//!
//! The walk is deterministic (breadth-first, sorted per directory), bounded,
//! and symlink-safe: a symlink entry is never followed (so cyclic/hostile
//! links can neither loop nor hide a subtree), and its presence makes the
//! inventory non-Complete ([`InventoryCompleteness::Unreadable`] — the tree
//! could not be vouched for at that path).
//!
//! ANY value other than [`InventoryCompleteness::Complete`] prohibits a
//! `Passed` verification verdict: the caller must classify the attempt
//! `Unavailable` with the typed reason.

use std::collections::{BTreeSet, VecDeque};
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use faktor_terminal::{EnvSpec, ProcessOwner, ProcessSupervisor, SpawnConfig};

/// Default hard cap on the scanned depth (the root is depth 0), generous for
/// real repositories. A directory beyond it reports
/// [`InventoryCompleteness::DepthBudgetExceeded`].
pub const MAX_INVENTORY_DEPTH: usize = 64;
/// Default resource budget on observed directory entries (files +
/// directories) of one discovery.
pub const DEFAULT_INVENTORY_MAX_PATHS: usize = 250_000;
/// Default cumulative metadata budget of one native traversal: the sum of
/// the file sizes observed (metadata only — never file contents).
pub const DEFAULT_INVENTORY_METADATA_BYTES: u64 = 512 * 1024 * 1024;
/// Default wall budget of one whole discovery (git + traversal + probing).
pub const DEFAULT_INVENTORY_WALL: Duration = Duration::from_secs(120);
/// Absolute wall ceiling of one discovery, whatever a configured budget
/// requests: discovery itself is never unbounded.
pub const MAX_INVENTORY_WALL_CEILING: Duration = Duration::from_secs(3600);
/// Default bound on one directory's listed children.
pub const DEFAULT_INVENTORY_DIR_PAGE: usize = 64 * 1024;
/// Default byte budget of each `git ls-files` stream.
pub const DEFAULT_GIT_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
/// Default wall budget of each git command.
pub const DEFAULT_GIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Default budget of admitted untracked (non-ignored) files.
pub const DEFAULT_UNTRACKED_BUDGET: usize = 10_000;

/// Bound of the git failure reason retained on the inventory (a bounded
/// first line, never the raw stream).
const GIT_REASON_MAX_CHARS: usize = 240;
/// Bounded stderr head captured from each git command (diagnostics only).
const GIT_STDERR_CAP: usize = 16 * 1024;

/// Directories never walked and never probed (vcs metadata and
/// dependency/build trees) — the same fixed skip set the bounded
/// evidence/verification walks use. The skip set is a deliberate scope
/// exclusion, not silent truncation: a manifest below a skipped directory is
/// out of scope for this inventory and is never probed.
pub const INVENTORY_SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    ".venv",
    "dist",
];

/// The recognized project manifests probed at every directory prefix of the
/// inventory. This mirrors the marker set the project-profile derivation
/// understands; its presence decides components, languages, build systems,
/// toolchains and test frameworks — hence the completeness rule above.
pub const MANIFEST_PROBE_NAMES: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "setup.py",
    "requirements.txt",
    "Pipfile",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "CMakeLists.txt",
    "Makefile",
    "makefile",
    "GNUmakefile",
    "meson.build",
    "build.ninja",
    "MODULE.bazel",
    "WORKSPACE",
    "WORKSPACE.bazel",
    "platformio.ini",
    "west.yml",
    "west.yaml",
    "CMakePresets.json",
];

/// The certified outcome of ONE bounded manifest content probe (the
/// `package.json`-class read that decides framework/toolchain detection).
/// A content-deciding manifest that is Unreadable, Oversized or Unstable
/// must NEVER act as Absent: silently dropping its content would silently
/// drop the checks its content derives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestProbe<T> {
    /// The path does not exist (or vanished between listing and probe).
    Absent,
    /// The bounded content was read stably under the admitted generation.
    Present(T),
    /// The manifest is larger than the content-probe cap; its content was
    /// not read.
    Oversized { path: String, bytes: u64 },
    /// The manifest exists but could not be read (permission failure, I/O
    /// error, symlink/special file the probe refuses to follow).
    Unreadable { path: String, reason: String },
    /// The manifest changed identity or content between reads (or the read
    /// was admitted under a candidate generation that no longer matches):
    /// the probe cannot certify WHICH revision it saw.
    Unstable { path: String, reason: String },
}

/// One manifest probe refusal as the derived profile surfaces it: a
/// content-deciding manifest whose content could not be certified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestProbeRefusal {
    pub path: String,
    /// `oversized` | `unreadable` | `unstable`.
    pub kind: &'static str,
    pub detail: String,
}

impl<T> ManifestProbe<T> {
    /// The refusal this probe surface carries, when the content could not be
    /// certified. `Absent`/`Present` carry none.
    pub fn refusal(&self) -> Option<ManifestProbeRefusal> {
        match self {
            ManifestProbe::Absent | ManifestProbe::Present(_) => None,
            ManifestProbe::Oversized { path, bytes } => Some(ManifestProbeRefusal {
                path: path.clone(),
                kind: "oversized",
                detail: format!(
                    "recognized manifest {path:?} is {bytes} bytes, over the {}-byte content-probe cap",
                    crate::derive::MAX_PROBE_BYTES
                ),
            }),
            ManifestProbe::Unreadable { path, reason } => Some(ManifestProbeRefusal {
                path: path.clone(),
                kind: "unreadable",
                detail: format!("recognized manifest {path:?} could not be read: {reason}"),
            }),
            ManifestProbe::Unstable { path, reason } => Some(ManifestProbeRefusal {
                path: path.clone(),
                kind: "unstable",
                detail: format!(
                    "recognized manifest {path:?} changed between reads (or left its admitted \
                     candidate generation): {reason}"
                ),
            }),
        }
    }
}

/// The secure identity of one file (dev + inode on unix; unavailable
/// elsewhere). Compared around reads so a swapped file can never certify
/// content it does not own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
}

fn file_identity(path: &Path) -> Option<FileIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(path)
            .ok()
            .map(|meta| FileIdentity {
                dev: meta.dev(),
                ino: meta.ino(),
            })
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// The secure identity of the candidate generation one manifest probe is
/// admitted under: the canonical root plus its directory identity. Every
/// read is tied to it, so a probe can never certify content from a
/// different (replaced/renamed) candidate generation.
#[derive(Debug, Clone)]
pub struct CandidateGeneration {
    root: PathBuf,
    identity: Option<FileIdentity>,
}

impl CandidateGeneration {
    /// Admit a generation over `root`: canonicalized once, identity captured
    /// now. A root that does not resolve is admitted with no identity — its
    /// probes then refuse (a missing root can never certify content).
    pub fn admit(root: &Path) -> Self {
        let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let identity = file_identity(&canonical);
        Self {
            root: canonical,
            identity,
        }
    }

    /// The canonical root this generation names.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether `root` is still the admitted generation: same canonical root
    /// AND the same directory identity (a replaced root is a new
    /// generation, never the old one).
    pub fn matches(&self, root: &Path) -> bool {
        let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        if canonical != self.root {
            return false;
        }
        match (file_identity(&canonical), self.identity) {
            (Some(now), Some(admitted)) => now == admitted,
            (None, None) => true,
            _ => false,
        }
    }
}

/// The injectable content reader of [`probe_manifest_text_with`]: production
/// reads with `std::fs::read`; adversarial tests swap a reader that changes
/// content between reads. Identity checks always run against the real
/// filesystem.
type ManifestReader<'a> = &'a mut dyn FnMut(&Path) -> std::io::Result<Vec<u8>>;

/// Probe one manifest's bounded content under an admitted candidate
/// generation. A read is accepted only when the root still matches the
/// generation, the path is a plain regular file (never a symlink/special
/// file), the file identity is stable from metadata to open, and two
/// independent reads agree byte-for-byte. Everything else is a typed
/// [`ManifestProbe`] refusal — never a silent `Absent`.
pub fn probe_manifest_text(
    root: &Path,
    generation: &CandidateGeneration,
    rel: &str,
) -> ManifestProbe<String> {
    let mut read = |path: &Path| std::fs::read(path);
    probe_manifest_text_with(root, generation, rel, &mut read)
}

fn probe_manifest_text_with(
    root: &Path,
    generation: &CandidateGeneration,
    rel: &str,
    read: ManifestReader<'_>,
) -> ManifestProbe<String> {
    let cap = crate::derive::MAX_PROBE_BYTES;
    if !generation.matches(root) {
        return ManifestProbe::Unstable {
            path: rel.to_string(),
            reason: format!(
                "the probe root {} does not match the admitted candidate generation {}",
                root.display(),
                generation.root().display()
            ),
        };
    }
    let path = generation.root().join(rel);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ManifestProbe::Absent,
        Err(e) => {
            return ManifestProbe::Unreadable {
                path: rel.to_string(),
                reason: format!("metadata failed: {e}"),
            }
        }
    };
    if meta.file_type().is_symlink() {
        return ManifestProbe::Unreadable {
            path: rel.to_string(),
            reason: "the manifest is a symlink the probe refuses to follow".into(),
        };
    }
    if !meta.file_type().is_file() {
        return ManifestProbe::Unreadable {
            path: rel.to_string(),
            reason: "the manifest is not a plain regular file".into(),
        };
    }
    if meta.len() > cap {
        return ManifestProbe::Oversized {
            path: rel.to_string(),
            bytes: meta.len(),
        };
    }
    // Identity is captured from the PATH and compared against the OPENED
    // handle (the same TOCTOU guard the canonical fs state uses): a path
    // swapped under the read can never certify content.
    let opened = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(e) => {
            return ManifestProbe::Unreadable {
                path: rel.to_string(),
                reason: format!("open failed: {e}"),
            }
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let opened_meta = match opened.metadata() {
            Ok(meta) => meta,
            Err(e) => {
                return ManifestProbe::Unreadable {
                    path: rel.to_string(),
                    reason: format!("opened metadata failed: {e}"),
                }
            }
        };
        if opened_meta.dev() != meta.dev() || opened_meta.ino() != meta.ino() {
            return ManifestProbe::Unstable {
                path: rel.to_string(),
                reason: "the path changed identity between metadata and open (TOCTOU)".into(),
            };
        }
    }
    // Read #1 from the opened handle (bounded), read #2 independently: the
    // two views must be byte-identical, else the probe cannot say which
    // revision it saw.
    let mut first = Vec::new();
    if let Err(e) = opened.take(cap + 1).read_to_end(&mut first) {
        return ManifestProbe::Unreadable {
            path: rel.to_string(),
            reason: format!("read failed: {e}"),
        };
    }
    if first.len() as u64 > cap {
        return ManifestProbe::Oversized {
            path: rel.to_string(),
            bytes: first.len() as u64,
        };
    }
    let second = match read(&path) {
        Ok(bytes) => bytes,
        Err(e) => {
            return ManifestProbe::Unreadable {
                path: rel.to_string(),
                reason: format!("re-read failed: {e}"),
            }
        }
    };
    if second.len() as u64 > cap {
        return ManifestProbe::Oversized {
            path: rel.to_string(),
            bytes: second.len() as u64,
        };
    }
    if second != first {
        return ManifestProbe::Unstable {
            path: rel.to_string(),
            reason: "the content changed between two reads".into(),
        };
    }
    // The path must still resolve to the SAME identity and size after both
    // reads: a swap after the reads can never certify the bytes read.
    match std::fs::symlink_metadata(&path) {
        Ok(after) => {
            if after.len() != meta.len() {
                return ManifestProbe::Unstable {
                    path: rel.to_string(),
                    reason: "the manifest size changed across the read".into(),
                };
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if after.dev() != meta.dev() || after.ino() != meta.ino() {
                    return ManifestProbe::Unstable {
                        path: rel.to_string(),
                        reason: "the path identity changed across the read".into(),
                    };
                }
            }
        }
        Err(e) => {
            return ManifestProbe::Unstable {
                path: rel.to_string(),
                reason: format!("the path vanished across the read: {e}"),
            }
        }
    }
    ManifestProbe::Present(String::from_utf8_lossy(&first).into_owned())
}

/// The resource budgets of one discovery, configurable per call. Exceeding
/// any of them is a typed non-Complete verdict, never silent truncation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryBudget {
    /// Max observed directory entries (files + directories) for a native
    /// traversal, and max file paths admitted from a git listing.
    pub max_paths: usize,
    /// Max cumulative metadata bytes (sum of observed file sizes) of a
    /// native traversal.
    pub max_metadata_bytes: u64,
    /// Max wall time of the whole discovery (git, traversal and probing).
    pub max_wall: Duration,
    /// Max scanned depth of a native traversal (the root is depth 0).
    pub max_depth: usize,
    /// Max children of one directory listed by a native traversal.
    pub max_dir_page: usize,
    /// Max bytes retained from each `git ls-files` stream.
    pub git_output_bytes: usize,
    /// Max wall time of each git command.
    pub git_timeout: Duration,
    /// Max untracked (non-ignored) files admitted from git.
    pub max_untracked: usize,
    /// Program invoked for git discovery (default `git`; tests and hosts
    /// pin an explicit path here).
    pub git_program: OsString,
}

impl Default for InventoryBudget {
    fn default() -> Self {
        Self {
            max_paths: DEFAULT_INVENTORY_MAX_PATHS,
            max_metadata_bytes: DEFAULT_INVENTORY_METADATA_BYTES,
            max_wall: DEFAULT_INVENTORY_WALL,
            max_depth: MAX_INVENTORY_DEPTH,
            max_dir_page: DEFAULT_INVENTORY_DIR_PAGE,
            git_output_bytes: DEFAULT_GIT_OUTPUT_BYTES,
            git_timeout: DEFAULT_GIT_TIMEOUT,
            max_untracked: DEFAULT_UNTRACKED_BUDGET,
            git_program: OsString::from("git"),
        }
    }
}

/// Whether a discovery certified the WHOLE repository under its budgets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InventoryCompleteness {
    /// Every entry within the budgets was read; the file list is the whole
    /// bounded view and profiling may proceed.
    Complete,
    /// A discovery stream exceeded its byte budget (git output over
    /// [`InventoryBudget::git_output_bytes`]).
    OutputCapped { limit_bytes: usize },
    /// Git was selected but could not complete (deadline, spawn or listing
    /// failure): the repository cannot be certified from a partial view.
    GitUnavailable { reason: String },
    /// More untracked files than [`InventoryBudget::max_untracked`] were
    /// observed; the count beyond the budget is surfaced, never hidden.
    UntrackedBudgetExceeded { observed: usize, budget: usize },
    /// A recognized manifest exists on disk but is absent from the
    /// inventory (gitignored, skipped, or over a budget); the derived check
    /// set could change.
    ManifestExcluded { path: String },
    /// A content-deciding manifest (`CMakeLists.txt`, `Makefile`,
    /// `*.csproj`) is larger than the derived-profile content-probe cap, so
    /// the evidence derived from its content would silently change.
    ManifestOversized { path: String, bytes: u64 },
    /// The path budget was exceeded: the traversal saw more entries than
    /// [`InventoryBudget::max_paths`].
    PathBudgetExceeded { seen: usize, max: usize },
    /// The cumulative metadata budget was exceeded.
    MetadataBudgetExceeded { seen_bytes: u64, max_bytes: u64 },
    /// The wall budget ran out mid-discovery.
    WallBudgetExceeded { max: Duration },
    /// An entry existed beyond [`InventoryBudget::max_depth`].
    DepthBudgetExceeded { max_depth: usize },
    /// A directory carried more children than
    /// [`InventoryBudget::max_dir_page`].
    DirectoryPageTruncated { limit: usize },
    /// One path could not be read as a plain file/directory entry (a
    /// permission failure, an I/O error, or a symlink the walk refuses to
    /// follow).
    Unreadable { path: String },
}

impl InventoryCompleteness {
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Complete)
    }

    /// The typed, human-readable reason of a non-Complete verdict.
    pub fn reason(&self) -> Option<String> {
        match self {
            Self::Complete => None,
            Self::OutputCapped { limit_bytes } => Some(format!(
                "repository discovery output exceeded the {limit_bytes}-byte budget; refusing to derive a check suite from a truncated listing"
            )),
            Self::GitUnavailable { reason } => Some(format!(
                "git repository discovery could not complete ({reason}); refusing to certify the repository from a partial view"
            )),
            Self::UntrackedBudgetExceeded { observed, budget } => {
                let excluded = observed.saturating_sub(*budget);
                Some(format!(
                    "repository discovery observed {observed} untracked paths, over the {budget}-path budget; {excluded} excluded paths are not in the inventory"
                ))
            }
            Self::ManifestExcluded { path } => Some(format!(
                "recognized project manifest {path:?} exists but was excluded from the inventory; the derived check set could change"
            )),
            Self::ManifestOversized { path, bytes } => Some(format!(
                "recognized project manifest {path:?} is {bytes} bytes, over the content-probe budget; check evidence derived from its content could silently change"
            )),
            Self::PathBudgetExceeded { seen, max } => Some(format!(
                "repository discovery exceeded the {max}-path budget (observed {seen}); refusing to derive a smaller check suite"
            )),
            Self::MetadataBudgetExceeded {
                seen_bytes,
                max_bytes,
            } => Some(format!(
                "repository discovery exceeded the {max_bytes}-byte metadata budget (observed {seen_bytes}); the bounded view is not the whole tree"
            )),
            Self::WallBudgetExceeded { max } => Some(format!(
                "repository discovery exceeded its {}s wall budget; the bounded view is not the whole tree",
                max.as_secs()
            )),
            Self::DepthBudgetExceeded { max_depth } => Some(format!(
                "repository inventory exceeded the depth-{max_depth} budget; a manifest or source beyond the frontier may be missing"
            )),
            Self::DirectoryPageTruncated { limit } => Some(format!(
                "repository inventory truncated a directory listing at the {limit}-entry page budget; the bounded view is not the whole tree"
            )),
            Self::Unreadable { path } => Some(format!(
                "repository inventory could not read {path:?} (unreadable path or a symlink the walk refuses to follow)"
            )),
        }
    }
}

/// How the inventory was discovered (typed provenance; a git failure is
/// never silent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InventorySource {
    /// `git ls-files` was the authority.
    Git,
    /// Git was unusable (absent / not a repository / non-zero exit): the
    /// native bounded traversal was the authority, with the typed reason.
    NativeGitFallback { reason: String },
    /// A native bounded traversal was used directly.
    Native,
}

/// The bounded discovery result: the sorted workspace-relative file paths,
/// the COMPLETENESS of the discovery that produced them, and the typed
/// source (git vs native fallback).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoInventory {
    pub files: Vec<String>,
    pub completeness: InventoryCompleteness,
    pub source: InventorySource,
}

impl Default for RepoInventory {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            completeness: InventoryCompleteness::Complete,
            source: InventorySource::Native,
        }
    }
}

impl RepoInventory {
    pub fn is_complete(&self) -> bool {
        self.completeness.is_complete()
    }

    /// `None` when the discovery was Complete; otherwise the typed reason a
    /// verification verdict must surface instead of `Passed`.
    pub fn refusal_reason(&self) -> Option<String> {
        self.completeness.reason()
    }
}

fn posix_rel(parts: &[String]) -> String {
    parts.join("/")
}

fn join_rel(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// True when a non-final path component is a skipped directory (the skip
/// policy applies to directories, never to a file that merely shares the
/// name).
fn under_skip_dir(path: &str) -> bool {
    let mut segments = path.split('/').peekable();
    while let Some(segment) = segments.next() {
        if segments.peek().is_none() {
            return false;
        }
        if INVENTORY_SKIP_DIRS.contains(&segment) {
            return true;
        }
    }
    false
}

/// Whether a manifest's CONTENT decides check families. A bounded content
/// probe whose marker is missed silently (the manifest is oversized) can
/// therefore change the derived check set, so such a manifest is surfaced.
fn content_decides_checks(name: &str) -> bool {
    matches!(
        name,
        "CMakeLists.txt" | "Makefile" | "makefile" | "GNUmakefile"
    ) || name.ends_with(".csproj")
}

/// Discover a repository's bounded file inventory with the default resource
/// budgets. Deterministic (git order aside: sorted), symlink-safe, and
/// honest about every budget: the first non-Complete condition encountered
/// in discovery order is reported (discovery order is stable, so the verdict
/// is stable).
pub fn discover_repo_inventory(root: &Path) -> RepoInventory {
    discover_repo_inventory_with_budget(root, &InventoryBudget::default())
}

/// [`discover_repo_inventory`] under explicit resource budgets (adversarial
/// tests and callers with tighter scope). Git is attempted first; a git
/// failure falls back to the native traversal with the typed reason, while
/// git LIMIT hits (output cap, untracked cap, deadline) are typed
/// non-Complete verdicts — a limit hit is never silently papered over by a
/// fallback that would implicitly claim git-equivalence.
pub fn discover_repo_inventory_with_budget(root: &Path, budget: &InventoryBudget) -> RepoInventory {
    let wall = budget.max_wall.min(MAX_INVENTORY_WALL_CEILING);
    let deadline = Instant::now() + wall;
    if budget.max_wall.is_zero() {
        // No discovery method can run within the budget: typed refusal
        // before any process or directory is touched.
        return RepoInventory {
            files: Vec::new(),
            completeness: InventoryCompleteness::WallBudgetExceeded {
                max: budget.max_wall,
            },
            source: InventorySource::Native,
        };
    }
    let (mut files, mut completeness, source) = match try_git_inventory(root, budget, deadline) {
        GitAttempt::Complete(files) => {
            (files, InventoryCompleteness::Complete, InventorySource::Git)
        }
        GitAttempt::Incomplete(inventory) => return inventory,
        GitAttempt::Fallback { reason } => {
            let native = native_inventory(root, budget, deadline);
            (
                native.files,
                native.completeness,
                InventorySource::NativeGitFallback { reason },
            )
        }
    };
    if completeness.is_complete() {
        files.sort();
        files.dedup();
        if let Some(probe) = probe_manifests(root, &files, budget, deadline) {
            completeness = probe;
        }
    }
    RepoInventory {
        files,
        completeness,
        source,
    }
}

enum GitAttempt {
    Complete(Vec<String>),
    Incomplete(RepoInventory),
    Fallback { reason: String },
}

/// The narrowed result of one supervised `git` invocation.
enum GitRun {
    Output(String),
    /// Output exceeded the byte budget: the listing is partial.
    Capped,
    /// The command hit its deadline (the remaining discovery wall budget,
    /// capped by the per-command git timeout).
    TimedOut {
        after: Duration,
    },
    /// Spawn failure or non-zero exit (e.g. "not a git repository").
    Failed {
        reason: String,
    },
}

/// The child environment of a git discovery: the toolchain allowlist
/// (PATH/HOME/...) plus `GIT_CEILING_DIRECTORIES` pinned to the parent of
/// the root, so git never ascends into an enclosing repository and lists
/// paths outside the verification root. The root itself is still checked.
fn git_env(root: &Path) -> EnvSpec {
    let mut entries: Vec<(OsString, OsString)> = faktor_core::command::TOOLCHAIN_ENV_ALLOWLIST
        .iter()
        .map(|name| (OsString::from(*name), OsString::new()))
        .collect();
    if let Some(parent) = root
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        entries.push((
            OsString::from("GIT_CEILING_DIRECTORIES"),
            parent.as_os_str().to_os_string(),
        ));
    }
    EnvSpec::Explicit(entries)
}

fn bounded_git_reason(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    line.chars().take(GIT_REASON_MAX_CHARS).collect()
}

fn run_git(root: &Path, budget: &InventoryBudget, deadline: Instant, args: &[&str]) -> GitRun {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let timeout = budget.git_timeout.min(remaining);
    if timeout.is_zero() {
        return GitRun::TimedOut {
            after: Duration::ZERO,
        };
    }
    let cfg = SpawnConfig {
        cmd: budget.git_program.to_string_lossy().into_owned(),
        args: args.iter().map(|arg| (*arg).to_string()).collect(),
        cwd: root.to_path_buf(),
        env: git_env(root),
        owner: ProcessOwner::Daemon,
        capture: true,
        artifact_max: budget.git_output_bytes.max(4096),
        network_isolation: faktor_terminal::NetworkIsolation::Inherit,
    };
    match ProcessSupervisor::shared().run_sync(
        cfg,
        timeout,
        budget.git_output_bytes,
        GIT_STDERR_CAP,
    ) {
        Ok(out) if out.timed_out => GitRun::TimedOut { after: timeout },
        Ok(out) if out.exit_code != Some(0) => {
            let reason = bounded_git_reason(&out.stderr_head);
            GitRun::Failed {
                reason: if reason.is_empty() {
                    format!("git exited with {:?}", out.exit_code)
                } else {
                    reason
                },
            }
        }
        Ok(out) if out.stdout_truncated => GitRun::Capped,
        Ok(out) => GitRun::Output(out.stdout_head),
        Err(e) => GitRun::Failed {
            reason: format!("git could not be spawned: {e}"),
        },
    }
}

fn parse_z(text: &str) -> Vec<String> {
    text.split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect()
}

fn git_incomplete(completeness: InventoryCompleteness) -> GitAttempt {
    GitAttempt::Incomplete(RepoInventory {
        files: Vec::new(),
        completeness,
        source: InventorySource::Git,
    })
}

fn try_git_inventory(root: &Path, budget: &InventoryBudget, deadline: Instant) -> GitAttempt {
    if !root.is_dir() {
        return GitAttempt::Fallback {
            reason: "repository root is not a readable directory".to_string(),
        };
    }
    let tracked = match run_git(root, budget, deadline, &["ls-files", "-z"]) {
        GitRun::Output(text) => parse_z(&text),
        GitRun::Capped => {
            return git_incomplete(InventoryCompleteness::OutputCapped {
                limit_bytes: budget.git_output_bytes,
            })
        }
        GitRun::TimedOut { after } => {
            return git_incomplete(InventoryCompleteness::GitUnavailable {
                reason: format!("git ls-files exceeded its {}ms deadline", after.as_millis()),
            })
        }
        GitRun::Failed { reason } => return GitAttempt::Fallback { reason },
    };
    if tracked.len() > budget.max_paths {
        return git_incomplete(InventoryCompleteness::PathBudgetExceeded {
            seen: tracked.len(),
            max: budget.max_paths,
        });
    }
    let untracked = match run_git(
        root,
        budget,
        deadline,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    ) {
        GitRun::Output(text) => parse_z(&text),
        GitRun::Capped => {
            return git_incomplete(InventoryCompleteness::OutputCapped {
                limit_bytes: budget.git_output_bytes,
            })
        }
        GitRun::TimedOut { after } => {
            return git_incomplete(InventoryCompleteness::GitUnavailable {
                reason: format!(
                    "git ls-files --others exceeded its {}ms deadline",
                    after.as_millis()
                ),
            })
        }
        GitRun::Failed { reason } => {
            return git_incomplete(InventoryCompleteness::GitUnavailable { reason })
        }
    };
    if untracked.len() > budget.max_untracked {
        return git_incomplete(InventoryCompleteness::UntrackedBudgetExceeded {
            observed: untracked.len(),
            budget: budget.max_untracked,
        });
    }
    if tracked.len() + untracked.len() > budget.max_paths {
        return git_incomplete(InventoryCompleteness::PathBudgetExceeded {
            seen: tracked.len() + untracked.len(),
            max: budget.max_paths,
        });
    }
    let mut files: Vec<String> = tracked
        .into_iter()
        .chain(untracked)
        .filter(|path| !under_skip_dir(path))
        .collect();
    files.sort();
    files.dedup();
    GitAttempt::Complete(files)
}

/// The bounded native traversal: breadth-first, sorted per directory,
/// symlinks never followed. Every budget hit is a typed verdict.
fn native_inventory(root: &Path, budget: &InventoryBudget, deadline: Instant) -> RepoInventory {
    let mut files: Vec<String> = Vec::new();
    let mut completeness = InventoryCompleteness::Complete;
    let mut paths_seen: usize = 0;
    let mut bytes_seen: u64 = 0;
    let mut queue: VecDeque<(usize, Vec<String>)> = VecDeque::new();
    queue.push_back((0, Vec::new()));
    'walk: while let Some((depth, dir_parts)) = queue.pop_front() {
        if Instant::now() >= deadline {
            completeness = InventoryCompleteness::WallBudgetExceeded {
                max: budget.max_wall,
            };
            break 'walk;
        }
        let abs = if dir_parts.is_empty() {
            root.to_path_buf()
        } else {
            root.join(posix_rel(&dir_parts))
        };
        let entries = match std::fs::read_dir(&abs) {
            Ok(entries) => entries,
            Err(_) => {
                completeness = InventoryCompleteness::Unreadable {
                    path: posix_rel(&dir_parts),
                };
                break 'walk;
            }
        };
        let mut names: Vec<(String, std::fs::FileType)> = Vec::new();
        for entry in entries {
            paths_seen += 1;
            if paths_seen > budget.max_paths {
                completeness = InventoryCompleteness::PathBudgetExceeded {
                    seen: paths_seen,
                    max: budget.max_paths,
                };
                break 'walk;
            }
            if names.len() >= budget.max_dir_page {
                completeness = InventoryCompleteness::DirectoryPageTruncated {
                    limit: budget.max_dir_page,
                };
                break 'walk;
            }
            if paths_seen.is_multiple_of(4096) && Instant::now() >= deadline {
                completeness = InventoryCompleteness::WallBudgetExceeded {
                    max: budget.max_wall,
                };
                break 'walk;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    completeness = InventoryCompleteness::Unreadable {
                        path: posix_rel(&dir_parts),
                    };
                    break 'walk;
                }
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            // symlink_metadata-based classification: a symlink stays a
            // symlink (never followed), so cyclic links cannot loop and a
            // linked subtree can never hide from the completeness verdict.
            let meta = match std::fs::symlink_metadata(entry.path()) {
                Ok(meta) => meta,
                Err(_) => {
                    let mut path = dir_parts.clone();
                    path.push(name);
                    completeness = InventoryCompleteness::Unreadable {
                        path: posix_rel(&path),
                    };
                    break 'walk;
                }
            };
            if meta.file_type().is_file() {
                bytes_seen = bytes_seen.saturating_add(meta.len());
                if bytes_seen > budget.max_metadata_bytes {
                    completeness = InventoryCompleteness::MetadataBudgetExceeded {
                        seen_bytes: bytes_seen,
                        max_bytes: budget.max_metadata_bytes,
                    };
                    break 'walk;
                }
            }
            names.push((name, meta.file_type()));
        }
        names.sort_by(|(a, _), (b, _)| a.cmp(b));
        for (name, file_type) in names {
            let mut parts = dir_parts.clone();
            parts.push(name.clone());
            if file_type.is_symlink() {
                completeness = InventoryCompleteness::Unreadable {
                    path: posix_rel(&parts),
                };
                break 'walk;
            }
            if file_type.is_dir() {
                if INVENTORY_SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                if depth + 1 > budget.max_depth {
                    completeness = InventoryCompleteness::DepthBudgetExceeded {
                        max_depth: budget.max_depth,
                    };
                    break 'walk;
                }
                queue.push_back((depth + 1, parts));
            } else {
                files.push(posix_rel(&parts));
            }
        }
    }
    files.sort();
    files.dedup();
    RepoInventory {
        files,
        completeness,
        source: InventorySource::Native,
    }
}

/// Targeted manifest discovery: probe every directory prefix observed in
/// the inventory for the recognized manifest names, and surface the first
/// completeness violation in deterministic order (root first, sorted dirs,
/// fixed name order). The rule is documented at the module level.
fn probe_manifests(
    root: &Path,
    files: &[String],
    budget: &InventoryBudget,
    deadline: Instant,
) -> Option<InventoryCompleteness> {
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    dirs.insert(String::new());
    for file in files {
        let mut start = 0usize;
        while let Some(pos) = file[start..].find('/') {
            let end = start + pos;
            dirs.insert(file[..end].to_string());
            start = end + 1;
        }
    }
    for dir in &dirs {
        if Instant::now() >= deadline {
            return Some(InventoryCompleteness::WallBudgetExceeded {
                max: budget.max_wall,
            });
        }
        for name in MANIFEST_PROBE_NAMES {
            let rel = join_rel(dir, name);
            // A case-variant of the same manifest name (Makefile/makefile)
            // already present in the inventory covers this probe: on a
            // case-insensitive filesystem the probe would otherwise find
            // the present file under the other spelling and report a false
            // exclusion. Only one variant is ever read by profile
            // derivation, so skipping cannot hide a check-affecting
            // manifest.
            let case_variant_present = MANIFEST_PROBE_NAMES.iter().any(|other| {
                !other.eq(name)
                    && other.eq_ignore_ascii_case(name)
                    && files.binary_search(&join_rel(dir, other)).is_ok()
            });
            if files.binary_search(&rel).is_ok() {
                if content_decides_checks(name) {
                    if let Ok(meta) = std::fs::symlink_metadata(root.join(&rel)) {
                        if meta.file_type().is_file() && meta.len() > crate::derive::MAX_PROBE_BYTES
                        {
                            return Some(InventoryCompleteness::ManifestOversized {
                                path: rel,
                                bytes: meta.len(),
                            });
                        }
                    }
                }
                continue;
            }
            if case_variant_present {
                continue;
            }
            match std::fs::symlink_metadata(root.join(&rel)) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Some(InventoryCompleteness::Unreadable { path: rel });
                }
                Ok(meta) if meta.file_type().is_file() => {
                    return Some(InventoryCompleteness::ManifestExcluded { path: rel });
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Some(InventoryCompleteness::Unreadable { path: rel }),
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn write(root: &Path, rel: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"x").unwrap();
    }

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn init_git(dir: &Path) {
        git(dir, &["init", "-q"]);
        git(dir, &["add", "-A"]);
    }

    fn deep_dir(depth: usize) -> String {
        (0..depth)
            .map(|i| format!("d{i:02}"))
            .collect::<Vec<_>>()
            .join("/")
    }

    // ---------------------------------------------------------- native basics

    #[test]
    fn complete_small_repo_is_complete_and_sorted() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/lib.rs");
        write(dir.path(), "Cargo.toml");
        write(dir.path(), "src/deep/mod.rs");
        let inv = discover_repo_inventory(dir.path());
        assert_eq!(inv.completeness, InventoryCompleteness::Complete);
        assert_eq!(
            inv.files,
            vec![
                "Cargo.toml".to_string(),
                "src/deep/mod.rs".to_string(),
                "src/lib.rs".to_string()
            ]
        );
        assert!(inv.refusal_reason().is_none());
        assert!(
            matches!(&inv.source, InventorySource::NativeGitFallback { reason } if !reason.is_empty()),
            "a non-repository root must record its typed fallback: {inv:?}"
        );
    }

    #[test]
    fn skip_dirs_are_not_walked_and_never_poison_completeness() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".git/objects/x");
        write(dir.path(), "target/debug/x");
        write(dir.path(), "src/lib.rs");
        let inv = discover_repo_inventory(dir.path());
        assert_eq!(inv.completeness, InventoryCompleteness::Complete);
        assert_eq!(inv.files, vec!["src/lib.rs".to_string()]);
    }

    #[test]
    fn unreadable_root_is_reported_not_empty_complete() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let inv = discover_repo_inventory(&missing);
        assert_eq!(
            inv.completeness,
            InventoryCompleteness::Unreadable {
                path: String::new()
            }
        );
        assert!(inv.files.is_empty());
        assert!(inv.refusal_reason().is_some());
    }

    // ------------------------------------------------------------- git path

    #[test]
    fn git_repo_with_five_thousand_tracked_files_and_deep_manifests_is_complete() {
        if !git_available() {
            eprintln!("skipping git discovery test: git is not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let deep = deep_dir(12);
        write(dir.path(), &format!("{deep}/Cargo.toml"));
        write(dir.path(), &format!("{deep}/package.json"));
        write(dir.path(), &format!("{deep}/src/lib.rs"));
        for i in 0..5000 {
            write(dir.path(), &format!("src/g{:02}/f{i:05}.txt", i % 50));
        }
        init_git(dir.path());

        let inv = discover_repo_inventory(dir.path());
        assert_eq!(inv.source, InventorySource::Git, "{inv:?}");
        assert_eq!(inv.completeness, InventoryCompleteness::Complete);
        assert_eq!(inv.files.len(), 5003, "5k sources + 2 manifests + lib.rs");
        assert!(
            inv.files.contains(&format!("{deep}/Cargo.toml")),
            "a depth-12 manifest beyond the old depth-6 frontier must be discovered"
        );

        // The derived check set is the right one for the deep component.
        let profile = crate::derive::detect_project_profile(dir.path(), &inv.files);
        let changed = vec![PathBuf::from(format!("{deep}/src/lib.rs"))];
        let specs = crate::derive::derive_checks(&profile, &changed).unwrap();
        let cargo = specs
            .iter()
            .find(|spec| spec.id.ends_with("rust_check"))
            .expect("the deep Cargo component must derive a cargo check");
        assert_eq!(cargo.cwd_rel, PathBuf::from(&deep));
        assert!(specs.iter().any(|spec| spec.program == "cargo"));
    }

    #[test]
    fn outer_repository_never_leaks_into_an_inner_root() {
        if !git_available() {
            eprintln!("skipping git discovery test: git is not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "sub/inner.rs");
        write(dir.path(), "outer.rs");
        init_git(dir.path());
        // `dir` is a repository; `dir/sub` is inside it but is not its own
        // repository. Discovery rooted at `sub` must not list the outer
        // repository's tracked paths (the ceiling env stops the ascent).
        let inner = dir.path().join("sub");
        let inv = discover_repo_inventory(&inner);
        assert!(
            matches!(&inv.source, InventorySource::NativeGitFallback { .. }),
            "an enclosing repository must not be probed through: {inv:?}"
        );
        assert_eq!(inv.completeness, InventoryCompleteness::Complete);
        assert_eq!(inv.files, vec!["inner.rs".to_string()]);
    }

    #[test]
    fn untracked_budget_caps_surface_the_excluded_count() {
        if !git_available() {
            eprintln!("skipping git discovery test: git is not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "tracked.txt");
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["add", "tracked.txt"]);
        for i in 0..20 {
            write(dir.path(), &format!("u{i:02}.txt"));
        }
        let budget = InventoryBudget {
            max_untracked: 5,
            ..Default::default()
        };
        let inv = discover_repo_inventory_with_budget(dir.path(), &budget);
        assert_eq!(
            inv.completeness,
            InventoryCompleteness::UntrackedBudgetExceeded {
                observed: 20,
                budget: 5
            },
            "{inv:?}"
        );
        let reason = inv.refusal_reason().unwrap();
        assert!(
            reason.contains("20 untracked") && reason.contains("15 excluded"),
            "the exclusion count must be surfaced: {reason}"
        );
        assert!(!inv.is_complete());
    }

    #[test]
    fn git_output_cap_is_typed_and_never_silent() {
        if !git_available() {
            eprintln!("skipping git discovery test: git is not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        for i in 0..64 {
            write(dir.path(), &format!("f{i:02}.txt"));
        }
        init_git(dir.path());
        let budget = InventoryBudget {
            git_output_bytes: 16,
            ..Default::default()
        };
        let inv = discover_repo_inventory_with_budget(dir.path(), &budget);
        assert_eq!(
            inv.completeness,
            InventoryCompleteness::OutputCapped { limit_bytes: 16 },
            "{inv:?}"
        );
        assert!(inv.refusal_reason().unwrap().contains("16-byte budget"));
    }

    #[test]
    fn absent_git_falls_back_typed_to_native() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/lib.rs");
        let budget = InventoryBudget {
            git_program: OsString::from("faktor-no-such-git-binary"),
            ..Default::default()
        };
        let inv = discover_repo_inventory_with_budget(dir.path(), &budget);
        assert!(
            matches!(&inv.source, InventorySource::NativeGitFallback { .. }),
            "{inv:?}"
        );
        assert_eq!(inv.completeness, InventoryCompleteness::Complete);
        assert_eq!(inv.files, vec!["src/lib.rs".to_string()]);
    }

    #[test]
    fn ignored_manifest_is_typed_incompleteness() {
        if !git_available() {
            eprintln!("skipping git discovery test: git is not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/lib.rs");
        fs::write(dir.path().join(".gitignore"), b"Cargo.toml\n").unwrap();
        write(dir.path(), "Cargo.toml");
        init_git(dir.path());
        let inv = discover_repo_inventory(dir.path());
        assert_eq!(inv.source, InventorySource::Git);
        assert_eq!(
            inv.completeness,
            InventoryCompleteness::ManifestExcluded {
                path: "Cargo.toml".to_string()
            },
            "{inv:?}"
        );
        assert!(inv
            .refusal_reason()
            .unwrap()
            .contains("derived check set could change"));
    }

    // ------------------------------------------------------- resource budgets

    #[test]
    fn path_budget_exceeded_is_typed_and_prohibits_passed() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..100 {
            write(dir.path(), &format!("d{}/f{i:03}.rs", i % 10));
        }
        let budget = InventoryBudget {
            max_paths: 10,
            ..Default::default()
        };
        let inv = discover_repo_inventory_with_budget(dir.path(), &budget);
        assert!(
            matches!(
                inv.completeness,
                InventoryCompleteness::PathBudgetExceeded { max: 10, .. }
            ),
            "{inv:?}"
        );
        assert!(!inv.is_complete());
        assert!(inv.refusal_reason().unwrap().contains("10-path budget"));
    }

    #[test]
    fn metadata_budget_exceeded_is_typed() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "big.bin");
        let budget = InventoryBudget {
            max_metadata_bytes: 0,
            ..Default::default()
        };
        let inv = discover_repo_inventory_with_budget(dir.path(), &budget);
        assert!(
            matches!(
                inv.completeness,
                InventoryCompleteness::MetadataBudgetExceeded { max_bytes: 0, .. }
            ),
            "{inv:?}"
        );
        assert!(!inv.is_complete());
    }

    #[test]
    fn wall_budget_exceeded_is_typed() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/lib.rs");
        let budget = InventoryBudget {
            git_program: OsString::from("faktor-no-such-git-binary"),
            max_wall: Duration::ZERO,
            ..Default::default()
        };
        let inv = discover_repo_inventory_with_budget(dir.path(), &budget);
        assert_eq!(
            inv.completeness,
            InventoryCompleteness::WallBudgetExceeded {
                max: Duration::ZERO
            },
            "{inv:?}"
        );
        assert!(!inv.is_complete());
    }

    #[test]
    fn directory_page_budget_truncation_is_typed() {
        let dir = tempfile::tempdir().unwrap();
        let wide = dir.path().join("wide");
        fs::create_dir_all(&wide).unwrap();
        for i in 0..5 {
            fs::write(wide.join(format!("c{i}")), b"x").unwrap();
        }
        let budget = InventoryBudget {
            max_dir_page: 2,
            ..Default::default()
        };
        let inv = discover_repo_inventory_with_budget(dir.path(), &budget);
        assert_eq!(
            inv.completeness,
            InventoryCompleteness::DirectoryPageTruncated { limit: 2 },
            "{inv:?}"
        );
        assert!(inv
            .refusal_reason()
            .unwrap()
            .contains("2-entry page budget"));
    }

    #[test]
    fn ten_thousand_entry_directory_stays_bounded_and_complete() {
        let dir = tempfile::tempdir().unwrap();
        let wide = dir.path().join("wide");
        fs::create_dir_all(&wide).unwrap();
        for i in 0..10_000 {
            fs::write(wide.join(format!("c{i:05}")), b"x").unwrap();
        }
        let inv = discover_repo_inventory(dir.path());
        assert_eq!(inv.completeness, InventoryCompleteness::Complete, "{inv:?}");
        assert_eq!(inv.files.len(), 10_000);
    }

    #[test]
    fn manifest_beyond_the_depth_budget_is_a_typed_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let mut deep = dir.path().to_path_buf();
        for i in 0..=MAX_INVENTORY_DEPTH {
            deep = deep.join(format!("d{i:02}"));
        }
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("Cargo.toml"), b"x").unwrap();
        let inv = discover_repo_inventory(dir.path());
        assert_eq!(
            inv.completeness,
            InventoryCompleteness::DepthBudgetExceeded {
                max_depth: MAX_INVENTORY_DEPTH
            },
            "{inv:?}"
        );
        assert!(inv.refusal_reason().unwrap().contains("depth"));
    }

    #[test]
    fn manifest_at_depth_twelve_is_discovered() {
        let dir = tempfile::tempdir().unwrap();
        let deep = deep_dir(12);
        write(dir.path(), &format!("{deep}/Cargo.toml"));
        write(dir.path(), &format!("{deep}/src/lib.rs"));
        let inv = discover_repo_inventory(dir.path());
        assert_eq!(inv.completeness, InventoryCompleteness::Complete, "{inv:?}");
        assert!(
            inv.files.contains(&format!("{deep}/Cargo.toml")),
            "a depth-12 manifest is well within the generous budgets: {inv:?}"
        );
    }

    #[test]
    fn large_native_tree_of_fifty_thousand_files_with_deep_manifest_is_complete() {
        let dir = tempfile::tempdir().unwrap();
        let deep = deep_dir(12);
        write(dir.path(), &format!("{deep}/CMakeLists.txt"));
        write(dir.path(), &format!("{deep}/src/main.c"));
        for d in 0..100 {
            let sub = dir.path().join(format!("src/g{d:03}"));
            fs::create_dir_all(&sub).unwrap();
            for f in 0..500 {
                fs::write(sub.join(format!("f{f:03}.rs")), b"// x\n").unwrap();
            }
        }
        let inv = discover_repo_inventory(dir.path());
        assert_eq!(
            inv.completeness,
            InventoryCompleteness::Complete,
            "{:?}",
            inv.completeness
        );
        assert!(inv.files.len() >= 50_002, "{} files", inv.files.len());
        assert!(inv.files.contains(&format!("{deep}/CMakeLists.txt")));

        let profile = crate::derive::detect_project_profile(dir.path(), &inv.files);
        let changed = vec![PathBuf::from(format!("{deep}/src/main.c"))];
        let specs = crate::derive::derive_checks(&profile, &changed).unwrap();
        assert!(
            specs
                .iter()
                .any(|spec| spec.id.ends_with("cmake_configure")),
            "the deep CMake component must derive its configure check: {specs:?}"
        );
    }

    // --------------------------------------------------- manifest completeness

    #[test]
    fn makefile_case_variant_is_not_a_false_exclusion() {
        // On a case-insensitive filesystem (`Makefile` present) the probe of
        // the lowercase spelling must not report a phantom exclusion.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Makefile");
        write(dir.path(), "src/main.c");
        let inv = discover_repo_inventory(dir.path());
        assert_eq!(inv.completeness, InventoryCompleteness::Complete, "{inv:?}");
    }

    #[test]
    fn oversized_content_deciding_manifest_is_typed_incompleteness() {
        let dir = tempfile::tempdir().unwrap();
        let oversized = vec![b'x'; (crate::derive::MAX_PROBE_BYTES + 1) as usize];
        fs::write(dir.path().join("Makefile"), &oversized).unwrap();
        write(dir.path(), "src/main.c");
        let inv = discover_repo_inventory(dir.path());
        assert!(
            matches!(
                &inv.completeness,
                InventoryCompleteness::ManifestOversized { path, bytes }
                    if path == "Makefile" && *bytes > crate::derive::MAX_PROBE_BYTES
            ),
            "{inv:?}"
        );
        assert!(!inv.is_complete());
        assert!(inv
            .refusal_reason()
            .unwrap()
            .contains("content-probe budget"));
    }

    #[test]
    fn oversized_non_deciding_manifest_stays_advisory_complete() {
        let dir = tempfile::tempdir().unwrap();
        let oversized = vec![b'x'; (crate::derive::MAX_PROBE_BYTES + 1) as usize];
        fs::write(dir.path().join("package.json"), &oversized).unwrap();
        write(dir.path(), "src/app.ts");
        let inv = discover_repo_inventory(dir.path());
        assert_eq!(
            inv.completeness,
            InventoryCompleteness::Complete,
            "package.json content is advisory only: {inv:?}"
        );
    }

    // -------------------------------------------------------------- hostile

    #[test]
    fn unreadable_subdirectory_is_reported_and_never_silently_skipped() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/lib.rs");
        let locked = dir.path().join("locked");
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join("hidden.rs"), b"x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
            let readable = std::fs::read_dir(&locked).is_ok();
            let inv = discover_repo_inventory(dir.path());
            // Root (CI containers) can read through mode 0; the test then
            // has no unreadable directory to assert and says so loudly.
            if readable {
                fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
                eprintln!("skipping unreadable-dir assertion: running with permission bypass");
                return;
            }
            assert!(
                matches!(
                    &inv.completeness,
                    InventoryCompleteness::Unreadable { path } if path == "locked"
                ),
                "{inv:?}"
            );
            assert!(inv.refusal_reason().unwrap().contains("locked"));
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    // ------------------------------------------------- manifest probe (generation)

    #[test]
    fn manifest_probe_present_absent_and_symlink_are_typed() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            br#"{"devDependencies":{"jest":"30"}}"#,
        )
        .unwrap();
        let generation = CandidateGeneration::admit(dir.path());
        assert!(matches!(
            probe_manifest_text(dir.path(), &generation, "package.json"),
            ManifestProbe::Present(text) if text.contains("jest")
        ));
        // A genuinely missing manifest stays Absent (no refusal).
        assert_eq!(
            probe_manifest_text(dir.path(), &generation, "missing.json"),
            ManifestProbe::Absent
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                dir.path().join("package.json"),
                dir.path().join("link.json"),
            )
            .unwrap();
            let probe = probe_manifest_text(dir.path(), &generation, "link.json");
            assert!(
                matches!(probe, ManifestProbe::Unreadable { .. }),
                "{probe:?}"
            );
            assert!(probe.refusal().is_some());
        }
    }

    #[test]
    fn manifest_probe_oversized_and_special_files_are_refusals_never_absent() {
        let dir = tempfile::tempdir().unwrap();
        let oversized = vec![b'x'; (crate::derive::MAX_PROBE_BYTES + 1) as usize];
        fs::write(dir.path().join("package.json"), &oversized).unwrap();
        let generation = CandidateGeneration::admit(dir.path());
        let probe = probe_manifest_text(dir.path(), &generation, "package.json");
        match &probe {
            ManifestProbe::Oversized { bytes, .. } => {
                assert_eq!(*bytes, crate::derive::MAX_PROBE_BYTES + 1);
            }
            other => panic!("oversized manifest must be a typed refusal: {other:?}"),
        }
        assert_eq!(probe.refusal().map(|r| r.kind), Some("oversized"));

        // A directory named package.json is a special file: unreadable.
        fs::remove_file(dir.path().join("package.json")).unwrap();
        fs::create_dir(dir.path().join("package.json")).unwrap();
        let probe = probe_manifest_text(dir.path(), &generation, "package.json");
        assert!(
            matches!(probe, ManifestProbe::Unreadable { .. }),
            "{probe:?}"
        );
        assert_eq!(probe.refusal().map(|r| r.kind), Some("unreadable"));
    }

    #[test]
    fn manifest_probe_unstable_when_content_changes_between_reads() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("package.json"), br#"{"jest":true}"#).unwrap();
        let generation = CandidateGeneration::admit(dir.path());
        let mut calls = 0usize;
        let mut reader = |_path: &Path| {
            calls += 1;
            Ok(br#"{"vitest":true}"#.to_vec())
        };
        let probe = probe_manifest_text_with(dir.path(), &generation, "package.json", &mut reader);
        assert_eq!(calls, 1, "the probe re-reads the content exactly once");
        match &probe {
            ManifestProbe::Unstable { reason, .. } => {
                assert!(reason.contains("changed between two reads"), "{reason}");
            }
            other => panic!("changing content must be a typed refusal: {other:?}"),
        }
        assert_eq!(probe.refusal().map(|r| r.kind), Some("unstable"));
    }

    #[cfg(unix)]
    #[test]
    fn manifest_probe_generation_mismatch_refuses() {
        let base = tempfile::tempdir().unwrap();
        let a = base.path().join("gen-a");
        let b = base.path().join("gen-b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("package.json"), br#"{"jest":true}"#).unwrap();
        fs::write(b.join("package.json"), br#"{"jest":true}"#).unwrap();
        let generation = CandidateGeneration::admit(&a);
        // A generation admitted on A never certifies content under B.
        let probe = probe_manifest_text(&b, &generation, "package.json");
        assert!(matches!(probe, ManifestProbe::Unstable { .. }), "{probe:?}");
        // Replacing the admitted root at the SAME path (new identity) is a
        // new generation: the old admission refuses.
        fs::remove_dir_all(&a).unwrap();
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("package.json"), br#"{"jest":true}"#).unwrap();
        let probe = probe_manifest_text(&a, &generation, "package.json");
        match &probe {
            ManifestProbe::Unstable { reason, .. } => {
                assert!(reason.contains("does not match"), "{reason}");
            }
            other => panic!("a replaced generation must refuse: {other:?}"),
        }
    }

    #[test]
    fn cyclic_and_hostile_symlinks_terminate_and_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/lib.rs");
        let loop_dir = dir.path().join("loops");
        fs::create_dir_all(&loop_dir).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&loop_dir, loop_dir.join("self")).unwrap();
            std::os::unix::fs::symlink(dir.path(), loop_dir.join("up")).unwrap();
            // A dangling symlink is still a symlink: refused, never followed.
            std::os::unix::fs::symlink("/nonexistent/faktor", loop_dir.join("dangling")).unwrap();
            let inv = discover_repo_inventory(dir.path());
            assert!(
                matches!(
                    &inv.completeness,
                    InventoryCompleteness::Unreadable { path } if path.starts_with("loops")
                ),
                "a symlink must make the inventory non-Complete: {inv:?}"
            );
        }
    }
}
