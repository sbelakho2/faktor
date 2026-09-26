//! Cheap cold-start evidence (P0-30): what the runtime serves while no
//! `Ready` index generation exists yet. The legacy fallback scanned the
//! whole workspace on the first prompt (up to 4000 files / 64 MiB / 16000
//! dirs synchronously); this provider NEVER walks the tree. Decision tree:
//!
//! ```text
//! 1. persisted OLD generation present (and <= COLD_MAX_GENERATION_LOAD_BYTES)
//!    -> serve evidence from it immediately (stale-but-cheap beats scanning;
//!       wave-11 keeps two generations, so a restart almost always has one)
//! 2. else, a git repo (.git exists):
//!    a. `git ls-files` for the tracked-file set (bounded output)
//!    b. ONE targeted, deadline-bounded ripgrep for the turn's concepts,
//!       scoped to the dirs of the directly referenced files (whole-tree
//!       only when the turn references nothing, still deadline-killed)
//!    c. results filtered to tracked paths
//! 3. always: reads of DIRECTLY referenced files (the turn's changed files
//!    + up to 32 referenced paths, head bytes only)
//! ```
//!
//! No step lists more than a few hundred directory entries and the whole
//! call runs under [`COLD_OVERALL_DEADLINE`]; when a step exceeds the
//! deadline or a tool is missing the provider DEGRADES to the remaining
//! cheaper steps (documented degrade, never the full scan). Next turn,
//! after the wave-11 service publishes a Ready generation, the runtime's
//! existing view logic serves full index evidence — nothing here caches or
//! upgrades; this is purely the pre-Ready fallback.
//!
//! Containment (audit bypass 2): the lexical sanitizer is only the FIRST
//! line. Every evidence file read resolves through ONE anchored
//! [`faktor_fs::RootedDir`] opened over the canonical workspace root,
//! component-by-component with no-follow semantics — a symlink/reparse point
//! on any component is a typed refusal — and the opened fd's identity is
//! checked against the no-follow classification before any byte is read.
//! Nothing on the evidence path re-resolves a pathname.
//!
//! Process authority (audit 14/26): cold git/rg children are NOT spawned
//! here. Every command runs through the workspace's single
//! [`faktor_terminal::ProcessSupervisor`] (typed
//! [`SupervisorColdCommandRunner`], `ProcessOwner::IndexCold` kill scope,
//! 900 ms deadline, bounded head capture, env-cleared spawn) and the
//! runtime awaits the ladder off the turn thread — this module owns no
//! private child lifecycle and no polling sleep.

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use faktor_fs::rooted::RootedEntryKind;
use faktor_fs::{ContentDigest, RootedDir};
use faktor_terminal::{EnvSpec, ProcessOwner, ProcessSupervisor, SpawnConfig};

use crate::generation::GenerationFile;
use crate::tokenize;
use crate::{WorkspaceId, WorkspaceIndex};

/// Directly referenced paths read per call (changed files + referenced).
pub const COLD_MAX_REFERENCE_PATHS: usize = 32;
/// Head bytes read per directly referenced file.
pub const COLD_MAX_READ_BYTES_PER_FILE: u64 = 64 * 1024;
/// Snippet characters kept per hit.
pub const COLD_MAX_SNIPPET_CHARS: usize = 1500;
/// Evidence hits returned per call (same bound as every other path).
pub const COLD_MAX_HITS: usize = 8;
/// A stale generation larger than this is not "cheap" to decode: skip to
/// the targeted steps (documented degrade).
pub const COLD_MAX_GENERATION_LOAD_BYTES: u64 = 16 * 1024 * 1024;
/// Hard wall-clock budget of one cold evidence call.
pub const COLD_OVERALL_DEADLINE: Duration = Duration::from_millis(1500);
/// Per external command (git/rg) budget; a killed command degrades.
const COLD_COMMAND_TIMEOUT: Duration = Duration::from_millis(900);
/// Tracked-file list cap (paths). Overflow degrades to "unknown tracked
/// set" — never a truncated set trusted as complete.
const COLD_MAX_TRACKED_PATHS: usize = 20_000;
/// Tracked-file list cap (bytes).
const COLD_MAX_TRACKED_BYTES: u64 = 512 * 1024;
/// Generation-directory entries enumerated per call. Two generations are the
/// steady state (wave-11 keeps two); the cap bounds a hostile directory, and
/// an overflowing listing is NEVER trusted as complete (the newest file may
/// be beyond the truncation).
const COLD_MAX_GENERATION_ENTRIES: usize = 256;
/// Concept cap (mirrors every other evidence path's bound).
const COLD_MAX_CONCEPTS: usize = 16;

/// Crate-wide test serialization (shared with the service seam tests):
/// the heavy cold fixtures and the seam-installing service tests must not
/// starve each other's deadlines on loaded machines.
#[cfg(test)]
pub(crate) static TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
const COLD_CONCEPT_MIN_CHARS: usize = 4;

/// One cold-evidence hit: path + snippet + score (renderer-shaped, mirrors
/// the search layer's bounded package shape).
#[derive(Debug, Clone, PartialEq)]
pub struct ColdHit {
    pub path: String,
    pub snippet: String,
    pub score: f64,
}

/// Where the cold evidence came from (observability + adversarial tests:
/// proves which decision-tree branch served, and that nothing deeper ran).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColdOrigin {
    /// A persisted OLD generation served the concepts; direct reads still
    /// rode along for the turn's own referenced files.
    StaleGeneration {
        generation: u64,
        direct_reads: usize,
    },
    /// Tracked-file git listing + targeted ripgrep served the concepts.
    TrackedSearch { tracked: bool, direct_reads: usize },
    /// No generation and no git: only the directly referenced files were
    /// read. The documented degrade end of the ladder.
    DirectReads { files: usize },
    /// Nothing at all was available (no generation, no git, no references,
    /// no concepts): an empty package, never a scan.
    None,
}

/// Work the call actually performed (proves "never a full walk").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ColdStats {
    /// Files opened and read (head bytes).
    pub files_read: usize,
    /// Directories listed.
    pub dirs_listed: usize,
    /// External commands spawned (git/rg).
    pub commands_run: usize,
    /// Steps skipped because a cheaper degrade already served enough or a
    /// deadline/tool failure cut them (documented degrade).
    pub degraded_steps: usize,
}

/// The retrieval signal of one turn, shaped for the cold provider.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ColdQuery {
    pub prompt: String,
    /// The turn's own changed files (highest-priority direct reads).
    pub changed_files: Vec<String>,
    /// Other directly referenced paths (tool reads, verification targets).
    pub referenced_paths: Vec<String>,
    /// Known failure keywords (concept signals).
    pub failures: Vec<String>,
}

/// The cold evidence package of one query.
#[derive(Debug, Clone, PartialEq)]
pub struct ColdEvidence {
    pub hits: Vec<ColdHit>,
    pub origin: ColdOrigin,
    pub stats: ColdStats,
}

/// Runner seam for git/ripgrep (tests inject failures and slow commands;
/// production uses [`SupervisorColdCommandRunner`]). `args` never contains
/// the root: commands run with the workspace root as cwd.
pub trait CommandRunner: Send + Sync {
    /// Run one program to completion; `Ok` carries the full bounded stdout
    /// (overflow, non-zero exit and deadline kills are typed `Err`s — the
    /// caller's documented degrade triggers, never partial output trusted
    /// as complete).
    fn run(&self, program: &str, args: &[String]) -> std::io::Result<String>;
}

/// The production runner (audit 14/26 — ONE process authority): every cold
/// git/rg child belongs to the workspace's single
/// [`faktor_terminal::ProcessSupervisor`] under the dedicated
/// `ProcessOwner::IndexCold` kill scope with [`COLD_COMMAND_TIMEOUT`] as
/// its hard deadline. Output is a bounded head (first
/// [`COLD_MAX_TRACKED_BYTES`] bytes of stdout, stderr capped at 4 KiB);
/// timeout kills, truncation and non-zero exits are `Err`s so the ladder
/// degrades instead of trusting partial data. The child env is cleared
/// (PATH/HOME passthrough only, `GIT_TERMINAL_PROMPT=0`): network and
/// daemon secrets never leak into a cold command.
pub struct SupervisorColdCommandRunner {
    supervisor: Arc<ProcessSupervisor>,
    cwd: PathBuf,
    workspace: WorkspaceId,
    deadline: Duration,
}

impl SupervisorColdCommandRunner {
    /// The injected daemon supervisor: [`ColdEvidenceProvider::new`]
    /// receives the daemon's ONE `Arc<ProcessSupervisor>` and installs this
    /// runner over it. No global fallback exists on this path.
    fn with_supervisor(
        supervisor: Arc<ProcessSupervisor>,
        cwd: PathBuf,
        workspace: WorkspaceId,
    ) -> Arc<dyn CommandRunner> {
        Arc::new(Self {
            supervisor,
            cwd,
            workspace,
            deadline: COLD_COMMAND_TIMEOUT,
        })
    }

    /// One supervised run of `program args...` in the workspace root:
    /// deadline-killed owned tree, bounded head capture, cleared env. A
    /// missing binary, a non-zero exit, a deadline kill or an overflowed
    /// capture are all `Err` — every one is a documented degrade.
    fn run_supervised(&self, program: &str, args: &[String]) -> std::io::Result<String> {
        let cfg = SpawnConfig {
            cmd: program.to_string(),
            args: args.to_vec(),
            cwd: self.cwd.clone(),
            // One authority: PATH/HOME via the allowlist; the universal
            // GIT_TERMINAL_PROMPT=0 safety default is added by resolve().
            env: EnvSpec::Allowlisted(vec!["PATH".into(), "HOME".into()]),
            owner: ProcessOwner::IndexCold {
                workspace: self.workspace,
                operation: 0,
            },
            capture: true,
            artifact_max: COLD_MAX_TRACKED_BYTES as usize,
            network_isolation: faktor_terminal::NetworkIsolation::Inherit,
        };
        let out = self
            .supervisor
            .run_sync(
                cfg,
                self.deadline,
                COLD_MAX_TRACKED_BYTES as usize,
                4 * 1024,
            )
            .map_err(|e| {
                std::io::Error::other(format!("supervised {program} spawn failed: {e}"))
            })?;
        if out.timed_out {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "supervised {program} exceeded its {:?} deadline and was killed",
                    self.deadline
                ),
            ));
        }
        if out.exit_code != Some(0) {
            let why = out.stderr_head.trim();
            return Err(std::io::Error::other(format!(
                "supervised {program} exited {:?}: {why}",
                out.exit_code
            )));
        }
        if out.stdout_truncated {
            return Err(std::io::Error::other(format!(
                "supervised {program} output exceeded the {COLD_MAX_TRACKED_BYTES}-byte capture cap"
            )));
        }
        Ok(out.stdout_head)
    }
}

impl CommandRunner for SupervisorColdCommandRunner {
    fn run(&self, program: &str, args: &[String]) -> std::io::Result<String> {
        self.run_supervised(program, args)
    }
}

/// The ONE anchored containment authority for every cold evidence read: the
/// canonical workspace root opened no-follow ONCE (symlinked/non-directory
/// roots fail typed). `None` when the root cannot be anchored — every read
/// then refuses typed; never a pathname fallback.
fn anchored_root(root: &Path) -> Option<RootedDir> {
    RootedDir::open(&root.canonicalize().ok()?).ok()
}

/// The cheap pre-Ready evidence provider (P0-30). Synchronous and bounded:
/// every step is an O(references) file read or a deadline-killed targeted
/// tool call through the one [`ProcessSupervisor`]; nothing here walks the
/// tree and nothing here owns a child. The runtime drives the ladder off
/// the turn thread (it blocks only on supervised commands that the
/// supervisor itself kills on their deadline).
pub struct ColdEvidenceProvider {
    root: PathBuf,
    workspace: WorkspaceId,
    generations_dir: PathBuf,
    overall_deadline: Duration,
    run: Arc<dyn CommandRunner>,
    /// The ONE anchored containment authority for every evidence file read:
    /// the canonical workspace root, opened no-follow once. `None` when the
    /// root cannot be anchored — every read then refuses typed; no pathname
    /// fallback exists.
    rooted: Option<RootedDir>,
}

impl std::fmt::Debug for ColdEvidenceProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ColdEvidenceProvider")
            .field("root", &self.root)
            .field("workspace", &self.workspace)
            .field("generations_dir", &self.generations_dir)
            .finish_non_exhaustive()
    }
}

impl ColdEvidenceProvider {
    /// The production constructor: every cold git/rg child is submitted to
    /// the `supervisor` injected by the daemon (the index service receives
    /// the daemon's ONE `Arc<ProcessSupervisor>` at open time) — no global
    /// process authority is ever constructed here.
    pub fn new(
        root: PathBuf,
        workspace: WorkspaceId,
        generations_dir: PathBuf,
        supervisor: Arc<ProcessSupervisor>,
    ) -> Self {
        let run = SupervisorColdCommandRunner::with_supervisor(supervisor, root.clone(), workspace);
        Self {
            rooted: anchored_root(&root),
            root,
            workspace,
            generations_dir,
            overall_deadline: COLD_OVERALL_DEADLINE,
            run,
        }
    }

    /// Test seam: inject a command runner (missing-tool and slow-command
    /// simulation) and the standard deadline.
    #[cfg(test)]
    fn with_runner(
        root: PathBuf,
        workspace: WorkspaceId,
        generations_dir: PathBuf,
        run: Arc<dyn CommandRunner>,
    ) -> Self {
        Self {
            rooted: anchored_root(&root),
            root,
            workspace,
            generations_dir,
            overall_deadline: COLD_OVERALL_DEADLINE,
            run,
        }
    }

    fn time_left(&self, started: Instant) -> bool {
        started.elapsed() < self.overall_deadline
    }

    /// The cold evidence package for one turn. Never panics, never walks
    /// the tree, never exceeds the deadline.
    pub fn evidence(&self, query: &ColdQuery) -> ColdEvidence {
        let started = Instant::now();
        let mut stats = ColdStats::default();

        // Directly referenced files, sanitized + deduped (changed first).
        let mut references: Vec<(String, PathBuf)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for raw in query
            .changed_files
            .iter()
            .chain(query.referenced_paths.iter())
            .take(COLD_MAX_REFERENCE_PATHS)
        {
            if !seen.insert(raw.clone()) {
                continue;
            }
            if let Some(rel) = sanitize_reference(raw) {
                references.push((raw.clone(), rel));
            }
        }

        let mut discovery: Vec<ColdHit> = Vec::new();
        let mut direct: Vec<ColdHit> = Vec::new();
        let mut degraded = 0usize;
        let mut gen_served: Option<u64> = None;
        let mut search_ran = false;

        // 1. Stale-but-cheap: a persisted OLD generation serves immediately
        // (its own decode is bounded; oversized files degrade to 2).
        if self.time_left(started) {
            if let Some((generation, index)) = self.load_stale_generation(&mut stats) {
                let concepts = concepts(query);
                let mut scored = search_index(&index, self.workspace, &concepts);
                scored.truncate(COLD_MAX_HITS);
                for (path, score) in scored {
                    match self.snippet_of(&path, &mut stats) {
                        // A hostile link never surfaces at all.
                        Err(ColdReadRefusal::Symlink) => continue,
                        Ok(snippet) => discovery.push(ColdHit {
                            path,
                            snippet,
                            score,
                        }),
                        Err(_) => discovery.push(ColdHit {
                            path,
                            snippet: String::new(),
                            score,
                        }),
                    }
                }
                gen_served = Some(generation);
            }
        } else {
            degraded += 1;
        }

        // 2. Direct reads: the turn's own referenced files always ride
        // along (their content IS the targeted evidence; stale generation
        // hits may add more, never replace these).
        let mut direct_reads = 0usize;
        for (raw, rel) in &references {
            if !self.time_left(started) {
                degraded += 1;
                break;
            }
            if let Ok(text) = self.snippet_of_rel(rel, &mut stats) {
                direct_reads += 1;
                direct.push(ColdHit {
                    path: raw.clone(),
                    snippet: text,
                    score: 0.99,
                });
            }
        }

        // 3. Git + targeted ripgrep ONLY when no generation served (the
        // generation branch is the restart "scan replacement"; discovery
        // stops as soon as one branch served). Missing tools and deadline
        // expiry degrade to the direct-read package — never the full scan.
        if gen_served.is_none() && self.time_left(started) {
            if self.root.join(".git").exists() || self.root.join(".git").is_file() {
                if let Some(mut hits) = self.tracked_search(query, started, &mut stats) {
                    search_ran = true;
                    discovery.append(&mut hits);
                } else {
                    degraded += 1;
                }
            } else {
                // No git: no tree listing exists in the cold ladder at all.
                degraded += 1;
            }
        } else if gen_served.is_none() {
            degraded += 1;
        }

        // Merge: discovery first (ranked), direct reads second (freshest),
        // deduped by path (direct wins the content, discovery wins the
        // score only when its rank is better), bounded.
        let mut merged: Vec<ColdHit> = Vec::new();
        let mut placed = std::collections::HashSet::new();
        for h in discovery.into_iter().chain(direct.iter().cloned()) {
            if !placed.insert(h.path.clone()) {
                continue;
            }
            merged.push(h);
        }
        merged.truncate(COLD_MAX_HITS);

        let origin = match gen_served {
            Some(generation) => ColdOrigin::StaleGeneration {
                generation,
                direct_reads,
            },
            None if search_ran => ColdOrigin::TrackedSearch {
                tracked: true,
                direct_reads,
            },
            None if !merged.is_empty() || direct_reads > 0 => ColdOrigin::DirectReads {
                files: direct_reads,
            },
            None => ColdOrigin::None,
        };
        stats.degraded_steps = degraded;
        ColdEvidence {
            hits: merged,
            origin,
            stats,
        }
    }

    /// Head snippet of a discovery path string (generation or rg output):
    /// the lexical sanitizer runs first, then the anchored rooted read.
    fn snippet_of(&self, rel: &str, stats: &mut ColdStats) -> Result<String, ColdReadRefusal> {
        let rel = sanitize_reference(rel).ok_or(ColdReadRefusal::RejectedPath)?;
        self.snippet_of_rel(&rel, stats)
    }

    /// Head snippet of one already-sanitized workspace-relative path through
    /// the ONE anchored authority; never a pathname reopen.
    fn snippet_of_rel(&self, rel: &Path, stats: &mut ColdStats) -> Result<String, ColdReadRefusal> {
        let rooted = self.rooted.as_ref().ok_or(ColdReadRefusal::Unavailable)?;
        read_head_rooted(rooted, rel, stats)
    }

    /// Test seam: the typed refusal of one anchored evidence read (the
    /// public package shape stays Option-like).
    #[cfg(test)]
    fn read_for_test(&self, rel: &Path) -> Result<String, ColdReadRefusal> {
        let rooted = self.rooted.as_ref().ok_or(ColdReadRefusal::Unavailable)?;
        read_head_rooted(rooted, rel, &mut ColdStats::default())
    }

    /// The newest persisted generation whose file is cheap to decode, read
    /// through ONE no-follow [`RootedDir`] anchored on the generation
    /// directory: the enumeration and the read share that single handle, so
    /// a symlink/reparse entry — including one swapped in between the
    /// enumeration and the read — is refused typed and never followed. The
    /// stale-but-cheap semantics are unchanged; every refusal (missing or
    /// symlinked directory, link entry, oversized file) is the documented
    /// degrade.
    fn load_stale_generation(&self, stats: &mut ColdStats) -> Option<(u64, WorkspaceIndex)> {
        let dir = self.generations_dir.join(self.workspace.raw().to_string());
        stats.dirs_listed += 1;
        // ONE anchored authority for the whole call. A missing, symlinked or
        // otherwise unopenable generation directory refuses typed here.
        let rooted = RootedDir::open(&dir).ok()?;
        let listing = rooted
            .list_entries(Path::new(""), COLD_MAX_GENERATION_ENTRIES)
            .ok()?;
        if listing.overflowed {
            // The listing is truncated: the newest generation may be beyond
            // the cut, so the truncated set is never trusted as complete.
            return None;
        }
        let mut newest: Option<(u64, OsString)> = None;
        for entry in listing.entries {
            if entry.kind != RootedEntryKind::File {
                continue;
            }
            let Some(g) = generation_of_name(&entry.name.to_string_lossy()) else {
                continue;
            };
            if newest.as_ref().map(|(ng, _)| g > *ng).unwrap_or(true) {
                newest = Some((g, entry.name));
            }
        }
        let (generation, name) = newest?;
        read_generation_file(&rooted, &name, generation, self.workspace, stats).ok()
    }

    /// Git tracked set + ONE targeted ripgrep for the turn's concepts,
    /// scoped to the referenced files' dirs. Missing tools or deadlines
    /// return `None` (the direct-read package is the documented degrade).
    fn tracked_search(
        &self,
        query: &ColdQuery,
        started: Instant,
        stats: &mut ColdStats,
    ) -> Option<Vec<ColdHit>> {
        stats.commands_run += 1;
        let out = self
            .run
            .run("git", &["ls-files".to_string(), "-z".to_string()])
            .ok()?;
        let mut set = std::collections::HashSet::new();
        for piece in out.split('\0') {
            if piece.is_empty() {
                continue;
            }
            if set.len() >= COLD_MAX_TRACKED_PATHS {
                // Overflow: the truncated set is NOT trusted as complete —
                // degrade to direct reads.
                return None;
            }
            set.insert(piece.to_string());
        }
        let tracked: Option<std::collections::HashSet<String>> = Some(set);
        let concepts = concepts(query);
        if concepts.is_empty() {
            return None;
        }
        if !self.time_left(started) {
            return None;
        }
        // Scope: the dirs of the directly referenced files; with none, the
        // tree root (deadline-killed on hostile giants — never a full walk
        // that hangs the turn).
        let mut scope: Vec<String> = Vec::new();
        let mut scope_set = std::collections::HashSet::new();
        for raw in query
            .changed_files
            .iter()
            .chain(query.referenced_paths.iter())
        {
            let dir = Path::new(raw)
                .parent()
                .map(|d| d.to_string_lossy().into_owned())
                .unwrap_or_default();
            let dir = if dir.is_empty() { "." } else { &dir };
            if scope_set.insert(dir.to_string()) {
                scope.push(dir.to_string());
            }
        }
        if scope.is_empty() {
            scope.push(".".into());
        }
        stats.commands_run += 1;
        let mut args = vec!["-l".into(), "-m".into(), "3".into(), "-F".into()];
        for c in concepts.iter().take(COLD_MAX_CONCEPTS) {
            args.push("-e".into());
            args.push(c.clone());
        }
        args.extend(scope.iter().cloned());
        let out = self.run.run("rg", &args).ok()?;
        let mut hits: Vec<ColdHit> = Vec::new();
        for line in out.lines().take(COLD_MAX_HITS * 4) {
            let path = line.to_string();
            if tracked.as_ref().is_some_and(|t| !t.contains(&path)) {
                continue; // untracked noise never rides cold evidence
            }
            match self.snippet_of(&path, stats) {
                // Lexically hostile and symlinked paths surface nothing.
                Err(ColdReadRefusal::RejectedPath | ColdReadRefusal::Symlink) => continue,
                Ok(snippet) => hits.push(ColdHit {
                    snippet,
                    path,
                    score: 0.9,
                }),
                Err(_) => hits.push(ColdHit {
                    snippet: String::new(),
                    path,
                    score: 0.9,
                }),
            }
        }
        if hits.is_empty() {
            return None;
        }
        Some(hits)
    }
}

/// Typed refusal of one cold evidence read. The public package is
/// Option-shaped (a refused reference contributes no hit), but the reason is
/// explicit and adversarially testable — never a silent pathname read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColdReadRefusal {
    /// The lexical sanitizer rejected the reference shape.
    RejectedPath,
    /// The workspace root could not be anchored at provider construction.
    Unavailable,
    /// No such entry under the anchored root.
    Missing,
    /// A symlink/reparse entry on any component (the no-follow walk refuses
    /// links), or an open that met a link swapped in after classification.
    Symlink,
    /// A directory or special file (FIFO/device/socket): never read.
    NotRegularFile,
    /// A zero-length file (parity with the historical snippet policy).
    Empty,
    /// Larger than [`COLD_MAX_READ_BYTES_PER_FILE`].
    Oversized,
    /// A NUL byte among the first 8192 bytes (binary sniff).
    Binary,
    /// Any other anchored-read failure.
    Io,
}

/// Map a rooted-authority error to the typed cold refusal: the anchored walk
/// reports a symlink/non-directory component as `Permission`.
fn refusal_of(e: &faktor_core::error::Error) -> ColdReadRefusal {
    match e.kind {
        faktor_core::error::ErrorKind::NotFound => ColdReadRefusal::Missing,
        faktor_core::error::ErrorKind::Permission => ColdReadRefusal::Symlink,
        faktor_core::error::ErrorKind::Oversized => ColdReadRefusal::Oversized,
        _ => ColdReadRefusal::Io,
    }
}

/// Parse the `gen-<u64>.json` generation-file name.
fn generation_of_name(name: &str) -> Option<u64> {
    name.strip_prefix("gen-")
        .and_then(|n| n.strip_suffix(".json"))
        .and_then(|n| n.parse::<u64>().ok())
}

/// Read and validate ONE generation file through the anchored handle:
/// no-follow classification first ([`RootedDir::entry_meta`] — a
/// symlink/reparse entry, directory or special file refuses TYPED before any
/// open, so a hostile link is never followed and its target bytes are never
/// read), then a bounded [`RootedDir::read`] whose full-file digest and
/// classified size are the post-open identity net: a swap that changed the
/// file between the classification and the open reads as a `Slice` digest or
/// a size mismatch and is refused, never decoded as someone else's file.
fn read_generation_file(
    rooted: &RootedDir,
    name: &OsStr,
    generation: u64,
    workspace: WorkspaceId,
    stats: &mut ColdStats,
) -> Result<(u64, WorkspaceIndex), ColdReadRefusal> {
    let rel = Path::new(name);
    let meta = rooted
        .entry_meta(rel)
        .map_err(|e| refusal_of(&e))?
        .ok_or(ColdReadRefusal::Missing)?;
    match meta.kind {
        RootedEntryKind::File => {}
        RootedEntryKind::Symlink => return Err(ColdReadRefusal::Symlink),
        RootedEntryKind::Directory | RootedEntryKind::Other => {
            return Err(ColdReadRefusal::NotRegularFile)
        }
    }
    if meta.size > COLD_MAX_GENERATION_LOAD_BYTES {
        // Too big to be cheap: degrade (documented), and never read it.
        return Err(ColdReadRefusal::Oversized);
    }
    stats.files_read += 1;
    let data = rooted
        .read(rel, COLD_MAX_GENERATION_LOAD_BYTES as usize)
        .map_err(|e| refusal_of(&e))?;
    // Post-open identity net: trust only the opened fd's own verified read —
    // a file that grew past the bound (Slice digest) or changed size after
    // the classification is refused, never partially decoded.
    let ContentDigest::Full(_) = data.digest else {
        return Err(ColdReadRefusal::Oversized);
    };
    if data.size as u64 != meta.size {
        return Err(ColdReadRefusal::Io);
    }
    let file = GenerationFile::from_bytes(&data.bytes).map_err(|_| ColdReadRefusal::Io)?;
    if file.workspace != workspace.raw() || file.generation != generation {
        // Hostile/mismatched fixture: never serve someone else's tree.
        return Err(ColdReadRefusal::Io);
    }
    let index = file.materialize().map_err(|_| ColdReadRefusal::Io)?;
    Ok((generation, index))
}

/// Head read of one file through the anchored root (bounded, text-only),
/// refusing every hostile shape typed. The containment ladder:
///
/// 1. the lexical sanitizer already ran (caller);
/// 2. no-follow classification via [`RootedDir::entry_meta`]: a symlink /
///    reparse point / directory / special file is refused BEFORE any open;
/// 3. the no-follow anchored walk opens the final component (`O_NOFOLLOW`
///    on unix, reparse-refusing relative open on Windows);
/// 4. post-open identity net: the OPENED fd's metadata must still be the
///    regular file of the classified size — a swap between classification
///    and open is refused — and the read never exceeds that size.
fn read_head_rooted(
    rooted: &RootedDir,
    rel: &Path,
    stats: &mut ColdStats,
) -> Result<String, ColdReadRefusal> {
    let meta = rooted
        .entry_meta(rel)
        .map_err(|e| refusal_of(&e))?
        .ok_or(ColdReadRefusal::Missing)?;
    match meta.kind {
        RootedEntryKind::File => {}
        RootedEntryKind::Symlink => return Err(ColdReadRefusal::Symlink),
        RootedEntryKind::Directory | RootedEntryKind::Other => {
            return Err(ColdReadRefusal::NotRegularFile)
        }
    }
    if meta.size == 0 {
        return Err(ColdReadRefusal::Empty);
    }
    if meta.size > COLD_MAX_READ_BYTES_PER_FILE {
        return Err(ColdReadRefusal::Oversized);
    }
    stats.files_read += 1;
    let mut file = rooted.open_read(rel).map_err(|e| refusal_of(&e))?;
    // Post-open identity net: trust only the fd, never a pathname.
    let opened = file.metadata().map_err(|_| ColdReadRefusal::Io)?;
    if !opened.is_file() || opened.len() != meta.size {
        return Err(ColdReadRefusal::NotRegularFile);
    }
    // Bounded read: at most the classified size, even when the file grows
    // after the fstat (a hostile growth never inflates the allocation).
    let mut bytes = vec![0u8; meta.size as usize];
    let mut filled = 0usize;
    while filled < bytes.len() {
        match file.read(&mut bytes[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(ColdReadRefusal::Io),
        }
    }
    bytes.truncate(filled);
    if bytes.iter().take(8192).any(|b| *b == 0) {
        return Err(ColdReadRefusal::Binary);
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut out: String = text.chars().take(COLD_MAX_SNIPPET_CHARS).collect();
    if text.chars().count() > COLD_MAX_SNIPPET_CHARS {
        out.push('…');
    }
    Ok(out)
}

/// Lexical FIRST LINE ONLY: reject hostile reference shapes (absolute paths,
/// parent traversal, NUL bytes, empty/overlength strings) and normalize the
/// rest to workspace-relative components. This is NOT the containment
/// authority: containment is decided by the anchored [`RootedDir`] read,
/// which refuses links on every component and never reopens a pathname.
fn sanitize_reference(raw: &str) -> Option<PathBuf> {
    if raw.is_empty() || raw.len() > 4096 || raw.contains('\0') {
        return None;
    }
    let p = Path::new(raw);
    if p.is_absolute() {
        return None;
    }
    let mut rel = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
            Component::CurDir => {}
            Component::Normal(name) => rel.push(name),
        }
    }
    if rel.as_os_str().is_empty() {
        return None;
    }
    Some(rel)
}

/// Concepts from the retrieval signal (bounded, deduped — mirrors every
/// other evidence path).
fn concepts(query: &ColdQuery) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let push = |text: &str, out: &mut Vec<String>, seen: &mut std::collections::HashSet<String>| {
        for tok in tokenize(text).into_iter().take(512) {
            if tok.len() < COLD_CONCEPT_MIN_CHARS || !seen.insert(tok.clone()) {
                continue;
            }
            out.push(tok);
            if out.len() >= COLD_MAX_CONCEPTS {
                return;
            }
        }
    };
    push(&query.prompt, &mut out, &mut seen);
    if out.len() < COLD_MAX_CONCEPTS {
        for f in query.changed_files.iter().take(16) {
            let base = f.rsplit('/').next().unwrap_or(f.as_str());
            push(base, &mut out, &mut seen);
            if out.len() >= COLD_MAX_CONCEPTS {
                break;
            }
        }
    }
    if out.len() < COLD_MAX_CONCEPTS {
        push(&query.failures.join(" "), &mut out, &mut seen);
    }
    out
}

/// Score one materialized index against the concepts: token frequency +
/// symbol presence, deterministic, bounded.
fn search_index(
    index: &WorkspaceIndex,
    ws: WorkspaceId,
    concepts: &[String],
) -> Vec<(String, f64)> {
    let mut scores: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for c in concepts {
        for hit in index.files_for_token(ws, c, 4) {
            *scores.entry(hit.path).or_insert(0.0) += f64::from(hit.freq).min(8.0);
        }
        for (path, sym) in index.symbol_lookup(ws, c, 4) {
            if sym.name.eq_ignore_ascii_case(c) {
                *scores.entry(path).or_insert(0.0) += 16.0;
            } else {
                *scores.entry(path).or_insert(0.0) += 4.0;
            }
        }
    }
    let mut ranked: Vec<(String, f64)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn ws_of(raw: u64) -> WorkspaceId {
        WorkspaceId::new(raw)
    }

    fn write(root: &Path, rel: &str, bytes: &[u8]) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    fn gen_dir(root: &Path, ws: WorkspaceId) -> PathBuf {
        root.join("generations").join(ws.raw().to_string())
    }

    fn publish_generation(
        data_root: &Path,
        ws: WorkspaceId,
        generation: u64,
        index: &WorkspaceIndex,
    ) {
        let dir = gen_dir(data_root, ws);
        std::fs::create_dir_all(&dir).unwrap();
        let env = GenerationFile::capture(ws.raw(), generation, index, vec![]);
        let bytes = env.to_bytes().unwrap();
        let mut f = std::fs::File::create(dir.join(format!("gen-{generation}.json"))).unwrap();
        f.write_all(&bytes).unwrap();
    }

    /// Every tool is missing in this fixture (the documented degrade
    /// trigger: `NotFound`).
    struct NeverRunner;
    impl CommandRunner for NeverRunner {
        fn run(&self, program: &str, _a: &[String]) -> std::io::Result<String> {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{program} intentionally absent in this fixture"),
            ))
        }
    }

    fn never_runner() -> Arc<dyn CommandRunner> {
        Arc::new(NeverRunner)
    }

    /// git works, rg is missing (the ls-files arm of the degrade ladder).
    struct GitOkRgMissing;
    impl CommandRunner for GitOkRgMissing {
        fn run(&self, program: &str, _a: &[String]) -> std::io::Result<String> {
            if program == "git" {
                Ok("src/app.rs\0".into())
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "rg missing",
                ))
            }
        }
    }

    /// The production runner over an INJECTED supervisor.
    fn supervised_runner(
        sup: &Arc<ProcessSupervisor>,
        cwd: PathBuf,
        workspace: WorkspaceId,
    ) -> Arc<dyn CommandRunner> {
        SupervisorColdCommandRunner::with_supervisor(sup.clone(), cwd, workspace)
    }

    /// One fixture command through the SAME supervisor authority (audit
    /// 14/26: cold.rs owns no private child lifecycle — not even fixture
    /// setup): env-cleared spawn with PATH/HOME passthrough and
    /// `GIT_TERMINAL_PROMPT=0`, bounded heads, 60 s fixture deadline.
    /// `Ok(true)` when the command exited 0 inside the deadline.
    fn supervised_fixture(
        sup: &ProcessSupervisor,
        cwd: &Path,
        program: &str,
        args: &[&str],
    ) -> bool {
        let cfg = SpawnConfig {
            cmd: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: cwd.to_path_buf(),
            env: EnvSpec::Allowlisted(vec!["PATH".into(), "HOME".into()]),
            owner: ProcessOwner::Daemon,
            ..Default::default()
        };
        sup.run_sync(cfg, Duration::from_secs(60), 64 * 1024, 64 * 1024)
            .is_ok_and(|o| o.exit_code == Some(0) && !o.timed_out)
    }

    /// The kill scope every supervised cold command of `ws` runs under.
    fn cold_owner(ws: WorkspaceId) -> ProcessOwner {
        ProcessOwner::IndexCold {
            workspace: ws,
            operation: 0,
        }
    }

    fn provider(root: PathBuf, ws: WorkspaceId, data_root: &Path) -> ColdEvidenceProvider {
        ColdEvidenceProvider::with_runner(
            root.clone(),
            ws,
            data_root.join("generations"),
            never_runner(),
        )
    }

    fn query(prompt: &str, changed: &[&str]) -> ColdQuery {
        ColdQuery {
            prompt: prompt.into(),
            changed_files: changed.iter().map(|s| s.to_string()).collect(),
            referenced_paths: vec![],
            failures: vec![],
        }
    }

    /// (a) A 200k-file NON-git tree: the cold path must complete in <50 ms
    /// and return ONLY the targeted bits (the changed-file read). The old
    /// fallback would have walked 200k files — nothing here lists a repo
    /// directory or spawns a command, so a full walk is structurally
    /// impossible, and the time bound proves it empirically.
    #[test]
    fn giant_non_git_tree_is_never_walked_and_returns_only_targeted_bits() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("giant");
        std::fs::create_dir_all(&root).unwrap();
        // 200 dirs x 1000 files = 200k files (hostile synthetic tree; the
        // files are empty — the fixture's cost is the ENTRY count, which is
        // what a walk would pay).
        for d in 0..200u32 {
            let sub = root.join(format!("d{d:03}"));
            std::fs::create_dir_all(&sub).unwrap();
            for f in 0..1000u32 {
                std::fs::File::create(sub.join(format!("f{f:04}.rs"))).unwrap();
            }
        }
        write(
            &root,
            "d007/f0001.rs",
            b"pub fn hot_symbol_x() -> i64 { 42 }\n",
        );
        let ws = ws_of(9);
        let provider = provider(root.clone(), ws, dir.path());
        let started = std::time::Instant::now();
        let evidence = provider.evidence(&query("inspect hot_symbol_x", &["d007/f0001.rs"]));
        let elapsed = started.elapsed();
        assert!(
            elapsed.as_millis() < 50,
            "cold path over a 200k-file tree took {elapsed:?}"
        );
        // Only the targeted bit came back: the changed file itself.
        assert!(
            evidence.hits.iter().any(|h| h.path == "d007/f0001.rs"),
            "{:?}",
            evidence.hits
        );
        assert!(
            evidence.hits.len() <= 1,
            "only the directly referenced file may surface: {:?}",
            evidence.hits
        );
        assert!(matches!(
            evidence.origin,
            ColdOrigin::DirectReads { files: 1 }
        ));
        assert_eq!(evidence.stats.files_read, 1);
        assert_eq!(evidence.stats.commands_run, 0);
        assert!(
            evidence.stats.dirs_listed <= 1,
            "only the (absent) generation dir may be probed, never repo dirs"
        );
        // The old fallback would have surfaced filler content from a full
        // walk; none of it can be here.
        assert!(!evidence.hits.iter().any(|h| h.path.contains("filler")));
    }

    /// (b) A persisted OLD generation serves the first prompt WITHOUT any
    /// scan: the repo tree is EMPTY, the generation file alone carries the
    /// content, and the provider never touches a repo file.
    #[test]
    fn stale_generation_serves_without_any_scan() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let ws = ws_of(11);
        let mut index = WorkspaceIndex::new();
        index
            .index_file(
                ws,
                std::path::Path::new("src/lib.rs"),
                b"pub fn balance_account() -> i64 { 42 }\n",
                1000,
            )
            .unwrap();
        publish_generation(dir.path(), ws, 1, &index);
        let provider = provider(root.clone(), ws, dir.path());
        let evidence = provider.evidence(&query("fix balance_account", &[]));
        assert!(matches!(
            evidence.origin,
            ColdOrigin::StaleGeneration {
                generation: 1,
                direct_reads: 0
            }
        ));
        assert!(
            evidence.hits.iter().any(|h| h.path == "src/lib.rs"),
            "{:?}",
            evidence.hits
        );
        assert_eq!(evidence.stats.commands_run, 0);
        // The generation file itself is the only repo-side read: no file
        // under the tree was scanned (the tree is empty anyway).
        assert!(evidence.stats.files_read <= 2, "{:?}", evidence.stats);
    }

    /// (c) Partial-now / upgrade-next, unit half 1 + half 2 in one fixture:
    /// turn 1 (no Ready generation) serves only the targeted changed-file
    /// reads; after the real IndexService publishes a Ready generation,
    /// the view serves FULL index evidence (the provider is never consulted
    /// again on the runtime path — the ordering guarantee is tested at the
    /// service level with the runtime's own decision tree).
    #[test]
    fn partial_now_upgrade_next_serves_different_packages() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        write(&root, "src/a.rs", b"pub fn alpha() -> i64 { 1 }\n");
        write(&root, "src/b.rs", b"pub fn beta() -> i64 { 2 }\n");
        let ws = ws_of(13);
        let provider = provider(root.clone(), ws, dir.path());
        // Turn 1: nothing indexed yet — only the changed file is read.
        let first = provider.evidence(&query("alpha", &["src/a.rs"]));
        assert!(matches!(first.origin, ColdOrigin::DirectReads { files: 1 }));
        assert!(
            first.hits.iter().any(|h| h.path == "src/a.rs"),
            "{:?}",
            first.hits
        );
        // The service publishes gen 1 (full index over BOTH files)...
        let mut index = WorkspaceIndex::new();
        index
            .index_file(
                ws,
                std::path::Path::new("src/a.rs"),
                b"pub fn alpha() -> i64 { 1 }\n",
                1,
            )
            .unwrap();
        index
            .index_file(
                ws,
                std::path::Path::new("src/b.rs"),
                b"pub fn beta() -> i64 { 2 }\n",
                2,
            )
            .unwrap();
        publish_generation(dir.path(), ws, 1, &index);
        // ...and the next turn serves the FULL package from the generation.
        let second = provider.evidence(&query("beta", &[]));
        assert!(matches!(
            second.origin,
            ColdOrigin::StaleGeneration { generation: 1, .. }
        ));
        assert!(
            second.hits.iter().any(|h| h.path == "src/b.rs"),
            "{:?}",
            second.hits
        );
    }

    /// (d) Hostile referenced paths (parent traversal, absolute, empty)
    /// are rejected: nothing outside the root is ever read and the call
    /// never panics; safe siblings still serve.
    #[test]
    fn hostile_referenced_paths_are_rejected() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        write(&root, "ok.rs", b"pub fn safe() {}\n");
        let ws = ws_of(15);
        let provider = provider(root.clone(), ws, dir.path());
        // Hostile refs next to a safe one: only the safe one reads.
        let mut q = query("inspect", &["../escape.rs"]);
        q.referenced_paths = vec![
            "/etc/passwd".into(),
            "a/../../b.rs".into(),
            "".into(),
            "ok.rs".into(),
        ];
        let evidence = provider.evidence(&q);
        assert!(
            evidence.hits.iter().all(|h| h.path == "ok.rs"),
            "{:?}",
            evidence.hits
        );
        assert_eq!(evidence.stats.files_read, 1);
        // All-hostile: empty package, no reads, no panic.
        let q = query("inspect", &["/etc/hosts", "../x.rs", "d/../../e.rs"]);
        let evidence = provider.evidence(&q);
        assert_eq!(evidence.stats.files_read, 0);
        assert!(evidence.hits.is_empty());
        assert!(matches!(evidence.origin, ColdOrigin::None));
    }

    /// (e) ripgrep/git missing: the targeted read-only fallback serves the
    /// directly referenced files (documented degrade) and the full scan is
    /// structurally impossible (0 commands can ever run here).
    #[test]
    fn missing_tools_degrade_to_reads_only_and_never_scan() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        write(&root, "src/app.rs", b"pub fn payments() {}\n");
        // A .git marker exists, but every tool is missing in this fixture.
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let ws = ws_of(17);
        let provider = provider(root.clone(), ws, dir.path());
        let evidence = provider.evidence(&query("payments", &["src/app.rs"]));
        assert!(
            matches!(evidence.origin, ColdOrigin::DirectReads { files: 1 }),
            "{:?}",
            evidence.origin
        );
        // git was ATTEMPTED (a .git marker exists) and failed: the attempt
        // is counted, the degrade ladder served reads only — no scan.
        assert_eq!(evidence.stats.commands_run, 1);
        assert_eq!(evidence.stats.files_read, 1);
        assert!(
            evidence.hits.iter().any(|h| h.path == "src/app.rs"),
            "{:?}",
            evidence.hits
        );
        assert_eq!(evidence.stats.degraded_steps, 1, "documented degrade");
    }

    /// Git present + git works + rg missing: ls-files runs, rg degrades,
    /// the direct reads still serve and no full scan ever happens.
    #[test]
    fn git_ok_rg_missing_degrades_to_reads() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        write(&root, "src/app.rs", b"pub fn payments() {}\n");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let ws = ws_of(19);
        let provider = ColdEvidenceProvider::with_runner(
            root.clone(),
            ws,
            dir.path().join("generations"),
            Arc::new(GitOkRgMissing),
        );
        let evidence = provider.evidence(&query("payments", &["src/app.rs"]));
        assert!(
            matches!(evidence.origin, ColdOrigin::DirectReads { files: 1 }),
            "{:?}",
            evidence.origin
        );
        assert_eq!(
            evidence.stats.commands_run, 2,
            "git attempted, rg attempted"
        );
        assert_eq!(evidence.stats.files_read, 1);
    }

    /// Behavior on a REAL small git repo (git + ripgrep available): the
    /// tracked file list gates the targeted ripgrep scope, and a file the
    /// turn references in a subdir surfaces through the tracked search.
    /// Skipped silently when git or rg is not installed (CI minimal
    /// images); the unit seams above lock the degrade ladder regardless.
    /// Fixture setup (tool probes, git init/add) runs through the SAME
    /// supervisor authority — never a private child here.
    #[test]
    fn real_small_git_repo_serves_tracked_search() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        write(
            &root,
            "src/app.rs",
            b"pub fn balance_account() -> i64 { 42 }\n",
        );
        write(&root, "src/other.rs", b"pub fn unrelated() {}\n");
        let sup = ProcessSupervisor::try_shared().expect("standalone supervisor");
        if !supervised_fixture(&sup, &root, "git", &["--version"])
            || !supervised_fixture(&sup, &root, "rg", &["--version"])
        {
            eprintln!("skipping real-git fixture: git/rg not installed");
            return;
        }
        assert!(
            supervised_fixture(&sup, &root, "git", &["init", "-q"]),
            "git init failed"
        );
        assert!(
            supervised_fixture(&sup, &root, "git", &["add", "."]),
            "git add failed"
        );
        let ws = ws_of(21);
        let provider = ColdEvidenceProvider::new(
            root.clone(),
            ws,
            dir.path().join("generations"),
            sup.clone(),
        );
        let evidence = provider.evidence(&ColdQuery {
            prompt: "continue".into(),
            changed_files: vec!["src/app.rs".into()],
            ..Default::default()
        });
        // Concept from the changed-file basename (app) matches app.rs via
        // tracked search; the direct read also surfaces the file itself.
        assert!(
            evidence.hits.iter().any(|h| h.path.ends_with("app.rs")),
            "{:?}",
            evidence.hits
        );
        assert!(evidence.stats.commands_run >= 2, "{:?}", evidence.stats);
        assert!(evidence.hits.len() <= COLD_MAX_HITS, "bounded package");
        // The real supervised children all exited: nothing live under the
        // cold kill scope of this workspace.
        let killed = sup.kill_all_for(cold_owner(ws));
        assert!(
            killed.is_empty(),
            "live cold children after a clean run: {killed:?}"
        );
    }

    /// (a) A real supervised rg over a 200k-file GIT tree: the ladder must
    /// return within the deadline or degrade to the documented read-only
    /// fallback — it never hangs (per-command 900 ms kill deadline) and
    /// never leaves a process (supervisor accounting after the call is
    /// empty under the cold kill scope).
    #[test]
    fn supervised_rg_over_200k_file_tree_is_bounded_and_leaves_no_child() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        // 200 dirs x 1000 files = 200k files. Only 8 dirs are TRACKED (the
        // 512 KiB tracked-set cap must not overflow) while rg's scope is
        // the whole tree: the real rg child must chew through 200k files
        // inside its 900 ms supervisor deadline or be killed by it.
        for d in 0..200u32 {
            let sub = root.join(format!("d{d:03}"));
            std::fs::create_dir_all(&sub).unwrap();
            for f in 0..1000u32 {
                std::fs::File::create(sub.join(format!("f{f:04}.rs"))).unwrap();
            }
        }
        write(
            &root,
            "d003/f0001.rs",
            b"pub fn balance_account() -> i64 { 42 }\n",
        );
        let sup = ProcessSupervisor::try_shared().expect("standalone supervisor");
        if !supervised_fixture(&sup, &root, "git", &["--version"])
            || !supervised_fixture(&sup, &root, "rg", &["--version"])
        {
            eprintln!("skipping 200k-file fixture: git/rg not installed");
            return;
        }
        assert!(supervised_fixture(&sup, &root, "git", &["init", "-q"]));
        let tracked_dirs: Vec<String> = (0..8u32).map(|d| format!("d{d:03}")).collect();
        let mut add_args = vec!["add"];
        add_args.extend(tracked_dirs.iter().map(|s| s.as_str()));
        assert!(
            supervised_fixture(&sup, &root, "git", &add_args),
            "fixture git add of the tracked subset failed"
        );
        let ws = ws_of(23);
        let provider = ColdEvidenceProvider::with_runner(
            root.clone(),
            ws,
            dir.path().join("generations"),
            supervised_runner(&sup, root.clone(), ws),
        );
        // No references: the rg scope is the tree root and the prompt is a
        // concept signal. Either rg beats its 900 ms deadline (hits from
        // d003/f0001.rs, gated by the tracked set) or the supervisor kills
        // it and the direct-read ladder serves — never a hang, never a
        // live child afterwards.
        let started = std::time::Instant::now();
        let evidence = provider.evidence(&ColdQuery {
            prompt: "inspect balance_account".into(),
            ..Default::default()
        });
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(4000),
            "cold evidence over a 200k-file tree took {elapsed:?}"
        );
        assert_eq!(
            evidence.stats.commands_run, 2,
            "git ls-files and rg were both attempted: {:?}",
            evidence.stats
        );
        assert!(evidence.hits.len() <= COLD_MAX_HITS, "bounded package");
        assert!(
            evidence.stats.degraded_steps <= 1,
            "only the rg step may degrade: {:?}",
            evidence.stats
        );
        // Supervisor accounting: every cold child of this workspace exited
        // (a killed rg is reaped by the supervisor's own waiter).
        let killed = sup.kill_all_for(cold_owner(ws));
        assert!(
            killed.is_empty(),
            "live cold children after the call: {killed:?}"
        );
        assert!(
            !sup.alive().iter().any(|c| c.owner == cold_owner(ws)),
            "cold kill scope still owns a live child after the call"
        );
    }

    /// (b) A deliberately slow rg (a real supervised child sleeping far
    /// past the 900 ms deadline) is killed by the supervisor deadline, no
    /// orphan survives (unix process-group probe), and the fallback
    /// evidence is returned.
    #[cfg(unix)]
    #[test]
    fn slow_rg_is_killed_by_the_supervisor_deadline_and_leaves_no_orphan() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        write(&root, "src/app.rs", b"pub fn payments() {}\n");
        let sup = ProcessSupervisor::try_shared().expect("standalone supervisor");
        if !supervised_fixture(&sup, &root, "git", &["--version"]) {
            eprintln!("skipping slow-rg fixture: git not installed");
            return;
        }
        assert!(supervised_fixture(&sup, &root, "git", &["init", "-q"]));
        assert!(supervised_fixture(&sup, &root, "git", &["add", "."]));
        let ws = ws_of(25);
        // The seam mirrors the PRODUCTION supervised runner for git; the rg
        // arm spawns a REAL child (`sleep 30`) under the same IndexCold
        // owner and 900 ms deadline, so only the supervisor's deadline kill
        // can end the call — and it must reap the whole owned tree.
        struct SlowRgSeam {
            git: SupervisorColdCommandRunner,
        }
        impl CommandRunner for SlowRgSeam {
            fn run(&self, program: &str, args: &[String]) -> std::io::Result<String> {
                if program == "git" {
                    self.git.run(program, args)
                } else {
                    self.git.run_supervised("sleep", &["30".to_string()])
                }
            }
        }
        let runner = Arc::new(SlowRgSeam {
            git: SupervisorColdCommandRunner {
                supervisor: sup.clone(),
                cwd: root.clone(),
                workspace: ws,
                deadline: COLD_COMMAND_TIMEOUT,
            },
        });
        let provider = ColdEvidenceProvider::with_runner(
            root.clone(),
            ws,
            dir.path().join("generations"),
            runner,
        );
        let started = std::time::Instant::now();
        let evidence = provider.evidence(&query("payments", &["src/app.rs"]));
        let elapsed = started.elapsed();
        // The rg arm was killed around its 900 ms deadline; the ladder then
        // served the direct read of the changed file.
        assert!(
            elapsed >= Duration::from_millis(800) && elapsed < Duration::from_secs(5),
            "slow rg ended {elapsed:?} after the call started"
        );
        assert!(
            matches!(evidence.origin, ColdOrigin::DirectReads { files: 1 }),
            "{:?}",
            evidence.origin
        );
        assert_eq!(evidence.stats.commands_run, 2, "{:?}", evidence.stats);
        assert_eq!(evidence.stats.degraded_steps, 1, "documented degrade");
        assert!(
            evidence.hits.iter().any(|h| h.path == "src/app.rs"),
            "{:?}",
            evidence.hits
        );
        // The supervisor reaped the killed tree: nothing live under the
        // cold kill scope, and the sleep's timeline row shows an exit.
        let killed = sup.kill_all_for(cold_owner(ws));
        assert!(killed.is_empty(), "orphaned cold children: {killed:?}");
        assert!(
            !sup.alive().iter().any(|c| c.owner == cold_owner(ws)),
            "cold kill scope still owns a live child after the deadline kill"
        );
        let sleeps: Vec<_> = sup
            .recent_spawns()
            .into_iter()
            .filter(|t| t.argv == "sleep 30")
            .collect();
        assert!(!sleeps.is_empty(), "the slow rg arm never spawned");
        assert!(
            sleeps.iter().all(|t| t.exited_ms.is_some()),
            "the supervisor must reap the killed sleep: {sleeps:?}"
        );
        // Unix group probe: no member of the killed child's process group
        // survives (kill -0 against the group id fails).
        #[cfg(unix)]
        {
            let pid = sleeps[0].pid;
            // A SIGKILLed process group can linger for a scheduling quantum
            // while its members are reaped. The bounded wait (owned by the
            // terminal process-lifecycle authority, see
            // `faktor_terminal::wait_for_group_gone`) observes extinction
            // instead of racing the probe right after the kill: the invariant
            // (no survivor) is unchanged, only the sampling is.
            assert!(
                faktor_terminal::wait_for_group_gone(pid, Duration::from_secs(10)),
                "orphan process group {pid} still alive 10s after the deadline kill"
            );
        }
    }

    /// (d) Source scan (audit 14/26): cold.rs owns NO private subprocess
    /// lifecycle — no std Command, no polling sleep, no hand-rolled wait
    /// or kill anywhere in this file (production or tests).
    #[test]
    fn cold_source_never_owns_a_private_child_lifecycle() {
        let src =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/cold.rs")).unwrap();
        let banned = [
            "std::process::".to_string() + "Command",
            "try_".to_string() + "wait",
            "thread::".to_string() + "sleep",
            ".kill".to_string() + "()",
        ];
        for token in banned {
            assert!(
                !src.contains(&token),
                "cold.rs must not contain a private child lifecycle ({token})"
            );
        }
    }

    /// Containment 1 (audit bypass 2): a committed symlink under the root
    /// pointing at an outside secret is refused TYPED by the anchored read
    /// (never via a pathname stat/open) and its target bytes never surface;
    /// honest files still serve snippets.
    #[cfg(unix)]
    #[test]
    fn symlink_reference_never_reads_outside_and_refuses_typed() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        write(&root, "honest.rs", b"pub fn honest() -> i64 { 7 }\n");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.rs"), b"TOP_SECRET_MARKER_9911\n").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.rs"), root.join("link.rs")).unwrap();
        let ws = ws_of(31);
        let provider = provider(root.clone(), ws, dir.path());
        assert_eq!(
            provider.read_for_test(Path::new("link.rs")).unwrap_err(),
            ColdReadRefusal::Symlink,
            "the anchored read owns the containment decision (typed refusal)"
        );
        let mut q = query("marker", &["link.rs"]);
        q.referenced_paths = vec!["honest.rs".into()];
        let evidence = provider.evidence(&q);
        assert!(
            evidence.hits.iter().all(|h| h.path != "link.rs"),
            "a symlink never surfaces: {:?}",
            evidence.hits
        );
        assert!(
            evidence
                .hits
                .iter()
                .all(|h| !h.snippet.contains("TOP_SECRET_MARKER_9911")),
            "the link target's bytes must never reach the package: {:?}",
            evidence.hits
        );
        assert!(
            evidence
                .hits
                .iter()
                .any(|h| h.path == "honest.rs" && h.snippet.contains("honest")),
            "honest files still serve snippets: {:?}",
            evidence.hits
        );
        assert_eq!(
            evidence.stats.files_read, 1,
            "only the honest file was read"
        );
    }

    /// Containment 2: a symlink swapped in AFTER the enumeration named the
    /// path (the rg result arrives, then the hostile swap) is refused at
    /// read time — the lexical reference is byte-identical to the honest
    /// path, yet the anchored walk owns the decision.
    #[cfg(unix)]
    #[test]
    fn symlink_swapped_between_enumeration_and_read_is_refused() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        write(&root, "src/app.rs", b"pub fn payments() {}\n");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.rs"), b"SWAP_SECRET_MARKER_2288\n").unwrap();
        struct SwapOnRg {
            root: PathBuf,
            outside: PathBuf,
        }
        impl CommandRunner for SwapOnRg {
            fn run(&self, program: &str, _args: &[String]) -> std::io::Result<String> {
                if program == "git" {
                    return Ok("src/app.rs\0".into());
                }
                // The enumeration names src/app.rs and the hostile swap
                // lands before the ladder's read of that path.
                let live = self.root.join("src/app.rs");
                std::fs::remove_file(&live).unwrap();
                std::os::unix::fs::symlink(self.outside.join("secret.rs"), &live).unwrap();
                Ok("src/app.rs\n".into())
            }
        }
        let ws = ws_of(33);
        let provider = ColdEvidenceProvider::with_runner(
            root.clone(),
            ws,
            dir.path().join("generations"),
            Arc::new(SwapOnRg {
                root: root.clone(),
                outside,
            }),
        );
        let evidence = provider.evidence(&ColdQuery {
            prompt: "payments".into(),
            ..Default::default()
        });
        assert_eq!(evidence.stats.commands_run, 2, "{:?}", evidence.stats);
        assert!(
            evidence.hits.iter().all(|h| h.path != "src/app.rs"),
            "the swapped link never surfaces: {:?}",
            evidence.hits
        );
        assert!(
            evidence
                .hits
                .iter()
                .all(|h| !h.snippet.contains("SWAP_SECRET_MARKER_2288")),
            "the swapped link's target bytes never reach the package: {:?}",
            evidence.hits
        );
        assert_eq!(evidence.stats.files_read, 0);
    }

    /// Containment 3: a nested symlinked directory (root/sub -> outside) is
    /// refused — the anchored walk rejects the link at the component level
    /// even though the final entry under it is a real file.
    #[cfg(unix)]
    #[test]
    fn nested_symlinked_directory_is_refused() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        write(&root, "ok.rs", b"pub fn ok() {}\n");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.rs"), b"NESTED_SECRET_MARKER_7733\n").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("sub")).unwrap();
        let ws = ws_of(35);
        let provider = provider(root.clone(), ws, dir.path());
        assert_eq!(
            provider
                .read_for_test(Path::new("sub/secret.rs"))
                .unwrap_err(),
            ColdReadRefusal::Symlink
        );
        let evidence = provider.evidence(&query("nested", &["sub/secret.rs", "ok.rs"]));
        assert!(
            evidence.hits.iter().all(|h| h.path != "sub/secret.rs"),
            "{:?}",
            evidence.hits
        );
        assert!(
            evidence
                .hits
                .iter()
                .all(|h| !h.snippet.contains("NESTED_SECRET_MARKER_7733")),
            "{:?}",
            evidence.hits
        );
        assert!(
            evidence.hits.iter().any(|h| h.path == "ok.rs"),
            "{:?}",
            evidence.hits
        );
        assert_eq!(evidence.stats.files_read, 1);
    }

    /// Containment 4: bounds stay typed through the anchored read —
    /// oversized, empty and binary files refuse with their own variants; a
    /// long honest file still yields a snippet capped at
    /// [`COLD_MAX_SNIPPET_CHARS`].
    #[test]
    fn anchored_read_bounds_are_typed() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let big = vec![b'a'; COLD_MAX_READ_BYTES_PER_FILE as usize + 1];
        write(&root, "big.rs", &big);
        write(&root, "bin.rs", b"ok\x00binary");
        std::fs::write(root.join("empty.rs"), b"").unwrap();
        write(
            &root,
            "long.rs",
            "a".repeat(COLD_MAX_SNIPPET_CHARS + 10).as_bytes(),
        );
        let ws = ws_of(37);
        let provider = provider(root.clone(), ws, dir.path());
        assert_eq!(
            provider.read_for_test(Path::new("big.rs")).unwrap_err(),
            ColdReadRefusal::Oversized
        );
        assert_eq!(
            provider.read_for_test(Path::new("bin.rs")).unwrap_err(),
            ColdReadRefusal::Binary
        );
        assert_eq!(
            provider.read_for_test(Path::new("empty.rs")).unwrap_err(),
            ColdReadRefusal::Empty
        );
        assert_eq!(
            provider.read_for_test(Path::new("missing.rs")).unwrap_err(),
            ColdReadRefusal::Missing
        );
        let long = provider.read_for_test(Path::new("long.rs")).unwrap();
        assert_eq!(long.chars().count(), COLD_MAX_SNIPPET_CHARS + 1);
        assert!(long.ends_with('…'));
    }

    /// Containment 5 (source scan): the evidence path never reopens a
    /// pathname — no pathname-based open, no unbounded read-to-end anywhere
    /// in cold.rs; the containment authority is the anchored `RootedDir`
    /// (walk + post-open identity net).
    #[test]
    fn cold_evidence_path_never_reopens_by_pathname() {
        let src =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/cold.rs")).unwrap();
        let banned = [
            "std::fs::File::".to_string() + "open",
            "read_to_".to_string() + "end",
        ];
        for token in banned {
            assert!(
                !src.contains(&token),
                "cold evidence reads must resolve through the anchored RootedDir, never by pathname ({token})"
            );
        }
        assert!(src.contains("entry_meta") && src.contains("open_read"));
        // The generation read is anchored too (audit residual): the
        // production region must not read the generations directory by
        // pathname anywhere — no read_dir, no raw read, no raw metadata.
        let production = &src[..src.find("mod tests {").expect("test module boundary")];
        for token in [
            "std::fs::read_".to_string() + "dir",
            "std::fs::read(".to_string(),
            "std::fs::metadata".to_string(),
        ] {
            assert!(
                !production.contains(&token),
                "generation reads must resolve through the anchored RootedDir ({token})"
            );
        }
    }

    /// The generation read is anchored: a symlinked `gen-*.json` entry is
    /// refused TYPED before any open (never followed), its outside target
    /// bytes never surface, and the provider degrades instead of serving.
    #[cfg(unix)]
    #[test]
    fn symlinked_generation_file_is_refused_typed_and_never_read() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let ws = ws_of(41);
        let gdir = gen_dir(dir.path(), ws);
        std::fs::create_dir_all(&gdir).unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(
            outside.join("gen-1.json"),
            b"OUTSIDE_GENERATION_MARKER_5522",
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.join("gen-1.json"), gdir.join("gen-1.json")).unwrap();

        let rooted = RootedDir::open(&gdir).unwrap();
        let mut stats = ColdStats::default();
        assert_eq!(
            read_generation_file(&rooted, OsStr::new("gen-1.json"), 1, ws, &mut stats).unwrap_err(),
            ColdReadRefusal::Symlink,
            "the anchored classification owns the refusal"
        );
        assert_eq!(
            stats.files_read, 0,
            "the link target is never opened or read"
        );

        let provider = provider(root, ws, dir.path());
        let evidence = provider.evidence(&query("fix balance_account", &[]));
        assert!(
            !matches!(evidence.origin, ColdOrigin::StaleGeneration { .. }),
            "a symlinked generation never serves: {:?}",
            evidence.origin
        );
        assert!(evidence
            .hits
            .iter()
            .all(|h| !h.snippet.contains("OUTSIDE_GENERATION_MARKER_5522")));
    }

    /// An honest generation still opens through the same anchored read.
    #[test]
    fn anchored_generation_read_serves_honest_files() {
        let dir = TempDir::new().unwrap();
        let ws = ws_of(43);
        let mut index = WorkspaceIndex::new();
        index
            .index_file(
                ws,
                Path::new("src/lib.rs"),
                b"pub fn anchored_ok() -> i64 { 7 }\n",
                1,
            )
            .unwrap();
        publish_generation(dir.path(), ws, 1, &index);
        let gdir = gen_dir(dir.path(), ws);
        let rooted = RootedDir::open(&gdir).unwrap();
        let mut stats = ColdStats::default();
        let (generation, loaded) =
            read_generation_file(&rooted, OsStr::new("gen-1.json"), 1, ws, &mut stats).unwrap();
        assert_eq!(generation, 1);
        assert!(
            !loaded.files_for_token(ws, "anchored", 4).is_empty(),
            "the honest generation's index must materialize"
        );
        assert_eq!(stats.files_read, 1);
    }

    /// A generation file larger than the cheap-decode bound is refused
    /// before any byte is read (bounded; no allocation, no partial decode).
    #[test]
    fn oversized_generation_is_refused_before_any_read() {
        let dir = TempDir::new().unwrap();
        let ws = ws_of(45);
        let gdir = gen_dir(dir.path(), ws);
        std::fs::create_dir_all(&gdir).unwrap();
        let big = std::fs::File::create(gdir.join("gen-1.json")).unwrap();
        big.set_len(COLD_MAX_GENERATION_LOAD_BYTES + 1).unwrap();
        drop(big);
        let rooted = RootedDir::open(&gdir).unwrap();
        let mut stats = ColdStats::default();
        assert_eq!(
            read_generation_file(&rooted, OsStr::new("gen-1.json"), 1, ws, &mut stats).unwrap_err(),
            ColdReadRefusal::Oversized
        );
        assert_eq!(stats.files_read, 0, "an oversized file is never read");

        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let provider = provider(root, ws, dir.path());
        let evidence = provider.evidence(&query("balance", &[]));
        assert!(
            !matches!(evidence.origin, ColdOrigin::StaleGeneration { .. }),
            "an oversized generation degrades instead of serving: {:?}",
            evidence.origin
        );
    }

    /// A swap between the enumeration and the read cannot redirect the read:
    /// the entry enumerated as a regular file is replaced by an outside
    /// symlink before the anchored read, the no-follow walk refuses it typed,
    /// and the outside marker never reaches the package.
    #[cfg(unix)]
    #[test]
    fn generation_swapped_between_enumeration_and_read_is_refused() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let ws = ws_of(47);
        let mut index = WorkspaceIndex::new();
        index
            .index_file(ws, Path::new("src/lib.rs"), b"pub fn swap_anchor() {}\n", 1)
            .unwrap();
        publish_generation(dir.path(), ws, 2, &index);
        let gdir = gen_dir(dir.path(), ws);
        let rooted = RootedDir::open(&gdir).unwrap();
        // Enumeration through the SAME anchored handle sees an honest file…
        let listing = rooted
            .list_entries(Path::new(""), COLD_MAX_GENERATION_ENTRIES)
            .unwrap();
        assert!(listing
            .entries
            .iter()
            .any(|e| e.name.to_string_lossy() == "gen-2.json" && e.kind == RootedEntryKind::File));
        // …then the hostile swap lands before the read.
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("gen-2.json"), b"SWAP_GENERATION_MARKER_8844").unwrap();
        std::fs::remove_file(gdir.join("gen-2.json")).unwrap();
        std::os::unix::fs::symlink(outside.join("gen-2.json"), gdir.join("gen-2.json")).unwrap();
        let mut stats = ColdStats::default();
        assert_eq!(
            read_generation_file(&rooted, OsStr::new("gen-2.json"), 2, ws, &mut stats).unwrap_err(),
            ColdReadRefusal::Symlink,
            "the swapped entry is refused at read time"
        );
        assert_eq!(stats.files_read, 0, "the swapped link is never opened");

        let provider = provider(root, ws, dir.path());
        let evidence = provider.evidence(&query("swap_anchor", &[]));
        assert!(
            !matches!(evidence.origin, ColdOrigin::StaleGeneration { .. }),
            "{:?}",
            evidence.origin
        );
        assert!(evidence
            .hits
            .iter()
            .all(|h| !h.snippet.contains("SWAP_GENERATION_MARKER_8844")));
    }
}
