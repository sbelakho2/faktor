//! Shadow mutation roots (P0-48): the strongest remaining filesystem
//! guarantee for single-agent MUTATING tasks.
//!
//! When a mutating single-item task starts, it does NOT work in the user
//! checkout: the executor begins a
//! daemon-owned SHADOW — a bounded, STABLE copy of the checkout under
//! `<data dir>/shadows/<session>/<shadow id>` (never inside the checkout) —
//! and the run's candidate is verified and landed through the SAME
//! run-base/candidate/integration architecture every orchestrated run uses:
//! a one-child instance of it. The shadow root IS the child's candidate
//! workspace; the immutable run base is recorded durably at begin
//! (`RunBaseRecord`, keyed by the shadow id) and every staged [`ChangeSet`]
//! carries the run-base/child-start/final-child snapshots exactly like an
//! orchestrated child's.
//!
//! Semantics (decided and documented here):
//! - The user checkout is the INTEGRATION TARGET, never the worktree of a
//!   shadowed drive. Landing goes through the executor's transactional
//!   `IntegrationTxnRow`/`IntegrationPathTxn` engine (record-first per-path
//!   decisions + rollback blobs, per-path CAS applies, whole-root equality).
//!   A conflict rolls every applied path back: the owner stays BYTE-IDENTICAL
//!   to its pre-landing state, never partially landed.
//! - `VerifiedComplete` is the CONSEQUENCE of a successful owner landing: the
//!   shadow-world verification the drive runs is never the permission to
//!   complete. While a MANAGED live shadow (one with a recorded run base)
//!   exists, the session completion gate refuses a proof that is not bound
//!   to a finalized integration.
//! - On integration CONFLICTS the shadow is NOT discarded: the durable
//!   shadow row moves to `IntegrationBlocked`, the integration transaction
//!   records the conflict list durably, and re-running the settlement after
//!   the user resolves the drift resumes the integration.
//! - On failure/cancel (or a discarded run) the shadow is discarded.
//! - Zero orphans: the active-shadow row is a DURABLE session fact
//!   ([`faktor_session::ShadowRow`]), so a crashed daemon's shadows are
//!   recoverable/cleanable after reopen ([`ShadowRoots::reconcile`]); a
//!   graceful daemon shutdown removes every shadow ([`ShadowRoots::shutdown`]
//!   and the service's `Drop`).
//! - Bounded everything: the base copy and the staged diff are capped
//!   (typed Oversized refusals; nothing is ever silently truncated), shadow
//!   rows live under bounded keys/values, and the git plumbing of the
//!   checkout (`.git`) is never copied.
//!
//! Git note: `faktor-git` exposes no worktree/archive-at-revision API
//! outside the repository (its only creation path, `WorktreeManager::create`,
//! adds a branch-worktree INSIDE the user repo — forbidden for shadows — and
//! needs a supervisor + async). All roots therefore use the bounded fs copy;
//! a git root (a tree with a `.git` entry) is detected and its `.git` is
//! skipped so no plumbing is ever materialized into a shadow.

#[cfg(test)]
use std::sync::Mutex;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use faktor_core::hash::FileHash;
use faktor_core::id::SessionId;
use faktor_session::{SessionManager, ShadowRow, ShadowRowState};

use crate::runtime::merge::{
    base_id_of, base_map_digest, compute_change_entries, parent_handle, put_base_map,
    put_change_set, read_base_map, read_change_set, ChangeSet, MAX_BASE_ENTRIES,
};
use crate::runtime::ExecError;

/// Default entry cap of one shadow base copy (matches the wave-13 base-map
/// cap; trees beyond it are typed Oversized refusals).
pub const SHADOW_MAX_BASE_ENTRIES: usize = MAX_BASE_ENTRIES;
/// Default total-byte cap of one shadow base copy.
pub const SHADOW_MAX_COPY_BYTES: u64 = 1024 * 1024 * 1024;
/// The daemon-owned subdirectory every shadow lives under.
pub const SHADOWS_DIR_NAME: &str = "shadows";
/// Deterministic shadow-run namespace inside the session's fact space (all
/// base-map/change-set rows of one shadow are keyed under its shadow id,
/// which is itself derived from the durable op-id sequence — generations
/// never collide).
const CHILD_ID: &str = "shadow";
/// Bounded retries of the stable shadow copy before a typed
/// [`ExecError::WorkspaceDrift`] (the SAME stable-copy contract
/// [`crate::runtime::task_executor::TaskExecutor`] applies to run bases:
/// `owner_before == owner_after == shadow_copied`).
pub const SHADOW_COPY_ATTEMPTS: usize = 3;
/// Directories the shadow copy never materializes (VCS bookkeeping is not
/// content; the root snapshot digest skips exactly these).
const SHADOW_SKIP_DIRS: &[&str] = &[".git", ".hg", ".svn"];

/// A stored shadow base copy: the daemon-owned work root of a shadowed
/// drive plus the generation anchors recorded at begin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shadow {
    pub session_id: SessionId,
    pub shadow_id: String,
    /// The user checkout (integration target of the executor's landing).
    pub base_root: PathBuf,
    /// The shadow's own root (daemon data dir; never inside the checkout).
    pub root: PathBuf,
}

/// Copy caps of one shadow base (bounded everything: caps are typed
/// Oversized refusals — the service never silently truncates a base copy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShadowCopyLimits {
    pub max_entries: usize,
    pub max_total_bytes: u64,
}

impl Default for ShadowCopyLimits {
    fn default() -> Self {
        Self {
            max_entries: SHADOW_MAX_BASE_ENTRIES,
            max_total_bytes: SHADOW_MAX_COPY_BYTES,
        }
    }
}

/// Deterministic test seam of the stable copy (adversarial tests): mutates
/// the owner between the `before` digest and the copy so the stability
/// contract is exercised deterministically.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowCopyDrift {
    /// Drift exactly once (the first attempt); later attempts are stable.
    Once,
    /// Drift on every attempt: the copy can never stabilize.
    Every,
}

/// The shadow service: one per daemon data dir. Begins/stages/discards
/// shadows and keeps their durable registry rows consistent. A graceful
/// shutdown (Drop or [`ShadowRoots::shutdown`]) removes every shadow dir;
/// a crash leaves rows that a reopen reconciles deterministically.
pub struct ShadowRoots {
    manager: Arc<SessionManager>,
    shadows_root: PathBuf,
    limits: ShadowCopyLimits,
    #[cfg(test)]
    copy_seam: Arc<Mutex<Option<ShadowCopyDrift>>>,
}

impl std::fmt::Debug for ShadowRoots {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShadowRoots")
            .field("shadows_root", &self.shadows_root)
            .finish_non_exhaustive()
    }
}

impl ShadowRoots {
    /// A service rooted at `<data dir>/<SHADOWS_DIR_NAME>`. The root is
    /// created on first use; `shadows_root` must never be a user checkout.
    pub fn new(manager: Arc<SessionManager>, shadows_root: PathBuf) -> Arc<Self> {
        Self::new_with_limits(manager, shadows_root, ShadowCopyLimits::default())
    }

    pub fn new_with_limits(
        manager: Arc<SessionManager>,
        shadows_root: PathBuf,
        limits: ShadowCopyLimits,
    ) -> Arc<Self> {
        if limits.max_entries == 0 || limits.max_total_bytes == 0 {
            panic!("shadow copy caps must be >= 1");
        }
        if limits.max_entries > MAX_BASE_ENTRIES {
            panic!("shadow copy entries cap exceeds the base-map cap");
        }
        Arc::new(Self {
            manager,
            shadows_root,
            limits,
            #[cfg(test)]
            copy_seam: Arc::new(Mutex::new(None)),
        })
    }

    pub fn manager(&self) -> Arc<SessionManager> {
        self.manager.clone()
    }

    pub fn shadows_root(&self) -> &Path {
        &self.shadows_root
    }

    /// The durable active-shadow row of a session (reopen-safe read).
    pub fn active_shadow(&self, session: SessionId) -> Result<Option<ShadowRow>, ExecError> {
        self.manager
            .shadow_row(session)
            .map_err(|e| ExecError::Internal(format!("shadow row read: {e}")))
    }

    // ------------------------------------------------------------ begin

    /// Begin a shadow of the session's workspace: a fresh daemon-owned
    /// bounded STABLE copy of `base_root` (the user checkout) plus the
    /// durable active-shadow row, the base manifest (the CAS anchors of
    /// every staged change) and the immutable `RunBaseRecord` (the exact
    /// generation every staged change set binds). The stable-copy contract
    /// matches the executor's run bases: `owner_before == owner_after ==
    /// copied` for the shadow tree, with [`SHADOW_COPY_ATTEMPTS`] bounded
    /// retries and a typed [`ExecError::WorkspaceDrift`] when the owner
    /// keeps moving. Refuses while the session already carries a LIVE
    /// shadow (crash residue or a drive in flight — resume/discard first).
    ///
    /// Copy order is deterministic and zero-orphan-safe: directory, stable
    /// bounded copy (`.git` plumbing skipped; symlink escapes, unreadable
    /// files and trees beyond the caps fail LOUDLY with typed errors),
    /// durable base manifest, durable run base, durable active row. A
    /// failure at any point removes the partial directory; a crash between
    /// steps leaves a row-less directory (removed by
    /// [`ShadowRoots::reconcile`]) or orphan manifest/run-base rows
    /// (harmless: the shadow id is never reused).
    pub fn begin_shadow(&self, session: SessionId, base_root: &Path) -> Result<Shadow, ExecError> {
        let handle = parent_handle(&self.manager, session)?;
        let session_row = handle
            .row()
            .map_err(|e| ExecError::Internal(format!("session row read: {e}")))?;
        let shadows_root = self.ensure_shadows_root()?;
        if let Some(existing) = self
            .manager
            .shadow_row(session)
            .map_err(|e| ExecError::Internal(format!("shadow row read: {e}")))?
        {
            if existing.state.is_live() {
                return Err(ExecError::Conflict(format!(
                    "session {session} has a live shadow {} (root {}); resume its integration or discard() it before beginning a new one",
                    existing.shadow_id,
                    existing.root
                )));
            }
            // A retired shadow row: its directory must be gone already; a
            // residue is removed below with the fresh directory.
        }
        let base = base_root.canonicalize().map_err(|e| {
            ExecError::NotFound(format!("shadow base {}: {e}", base_root.display()))
        })?;
        if !base.is_dir() {
            return Err(ExecError::NotFound(format!(
                "shadow base {} is not a directory",
                base.display()
            )));
        }
        if base.starts_with(&shadows_root) || base == shadows_root {
            return Err(ExecError::Conflict(format!(
                "shadow base {} lives inside the daemon shadow root {}; a user checkout can never be a shadow",
                base.display(),
                shadows_root.display()
            )));
        }
        let shadow_id = format!("sh-{:016x}", self.manager.next_op_id().raw());
        let dir = shadows_root
            .join(session.raw().to_string())
            .join(&shadow_id);
        let digest = |root: &Path| -> Result<String, ExecError> {
            super::task_executor::root_manifest_digest(root)
        };
        // Stable copy (bounded retries): the copy is accepted only when the
        // owner digest was identical before and after the copy AND the
        // copied tree digests to the same value; otherwise the whole
        // directory is rebuilt and the owner re-read.
        let mut detail = String::new();
        let mut accepted: Option<(String, Vec<faktor_fs::SnapshotEntry>)> = None;
        for attempt in 1..=SHADOW_COPY_ATTEMPTS {
            if dir.exists() {
                // Crash residue/previous attempt: the daemon-owned dir is
                // removed wholesale (never inside the user checkout).
                std::fs::remove_dir_all(&dir).map_err(|e| {
                    ExecError::Internal(format!("shadow residue removal {}: {e}", dir.display()))
                })?;
            }
            std::fs::create_dir_all(&dir)
                .map_err(|e| ExecError::Internal(format!("shadow dir {}: {e}", dir.display())))?;
            let before = digest(&base)?;
            self.check_copy_seam(&base, attempt);
            // The copy is CANONICAL-manifest faithful (literal symlinks,
            // executable bits preserved): the stable-copy equality below is
            // an equality of the ONE tree identity, not of a re-shaped
            // materialization. Shadow policy stays stricter than the general
            // manifest: a link whose resolved target leaves the checkout is
            // still refused loudly before anything is copied.
            reject_escaping_links(&base, self.limits.max_entries).inspect_err(|_| {
                let _ = std::fs::remove_dir_all(&dir);
            })?;
            if let Err(e) = faktor_fs::tree_manifest::copy_tree_manifest(
                &base,
                &dir,
                self.limits.max_entries,
                self.limits.max_total_bytes,
                SHADOW_SKIP_DIRS,
            ) {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(shadow_copy_error(
                    &format!("shadow base copy of {}", base.display()),
                    e,
                ));
            }
            let after = digest(&base)?;
            let copied = digest(&dir)?;
            if before == after && after == copied {
                let snapshot = faktor_fs::snapshot_tree(&dir, MAX_BASE_ENTRIES)
                    .map_err(|e| ExecError::from_fs("shadow tree snapshot", &dir, e))?;
                accepted = Some((copied, snapshot));
                break;
            }
            detail = format!("attempt {attempt}: before={before} after={after} copied={copied}");
        }
        let Some((copied, manifest)) = accepted else {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(ExecError::WorkspaceDrift(format!(
                "shadow base {} did not stabilize during the stable copy after {SHADOW_COPY_ATTEMPTS} attempts ({detail}); refusing to shadow a drifting checkout",
                base.display()
            )));
        };
        let manifest_rows: Vec<(PathBuf, FileHash)> =
            manifest.iter().map(|e| (e.path.clone(), e.hash)).collect();
        // Durable base manifest FIRST (crash before later rows leaves a
        // row-less dir removed by reconcile), then the immutable run base,
        // then the durable active row.
        if let Err(e) = put_base_map(
            &self.manager,
            session,
            &shadow_id,
            CHILD_ID,
            "base",
            &manifest_rows,
        ) {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
        let record = faktor_session::ledger::RunBaseRecord {
            run_id: shadow_id.clone(),
            workspace_id: session_row.workspace_id.raw(),
            worktree_id: session_row.worktree_id.raw(),
            snapshot_hash: copied,
            manifest_digest: base_map_digest(&manifest_rows),
            root: dir.to_string_lossy().into_owned(),
            created_ms: handle.now_ms(),
        };
        if let Err(e) = handle.ledger_run_base_set(&record).map(|_| ()) {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(ExecError::Internal(format!("shadow run base write: {e}")));
        }
        let total: u64 = manifest.iter().map(|e| e.size).sum();
        let row = ShadowRow {
            session_id: session.raw(),
            shadow_id: shadow_id.clone(),
            base_root: base.to_string_lossy().into_owned(),
            root: dir.to_string_lossy().into_owned(),
            state: ShadowRowState::Active,
            base_entries: manifest.len() as u64,
            base_bytes: total,
            created_ms: self.manager.now_ms(),
        };
        if let Err(e) = self
            .manager
            .put_shadow_row(session, &row)
            .map_err(|e| ExecError::Internal(format!("shadow row write: {e}")))
        {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
        Ok(Shadow {
            session_id: session,
            shadow_id,
            base_root: base,
            root: dir,
        })
    }

    // ------------------------------------------------------------ staging

    /// Compute and durably store the change set of the session's shadow:
    /// every file whose current shadow content differs from the base
    /// manifest (with the base digest as the CAS anchor of the user path),
    /// plus recorded deletions. The set carries the SAME generation anchors
    /// as an orchestrated child's: `run_base_snapshot` (the immutable run
    /// base recorded at begin — a shadow without one refuses to stage),
    /// `child_start_snapshot` (digest of the begin map) and
    /// `final_child_snapshot` (digest of the shadow tree now). Entry count
    /// beyond [`crate::runtime::merge::MAX_CHANGES`] is a typed Oversized
    /// error and NOTHING is stored (the integration never silently
    /// truncates). Deterministic and idempotent for an unchanged shadow.
    pub fn stage_change_set(&self, session: SessionId) -> Result<ChangeSet, ExecError> {
        let row = self.live_row(session)?;
        let shadow = Shadow {
            session_id: session,
            shadow_id: row.shadow_id.clone(),
            base_root: PathBuf::from(&row.base_root),
            root: PathBuf::from(&row.root),
        };
        let handle = parent_handle(&self.manager, session)?;
        let Some(run_base) = handle
            .ledger_run_base_get(&shadow.shadow_id)
            .map_err(|e| ExecError::Internal(format!("shadow run base read: {e}")))?
        else {
            return Err(ExecError::InvalidState(format!(
                "shadow {} has no durable run base (a legacy or crashed begin); discard() it and begin_shadow() again",
                shadow.shadow_id
            )));
        };
        let base_map = self.base_manifest(&shadow)?;
        let now_snap = faktor_fs::snapshot_tree(&shadow.root, MAX_BASE_ENTRIES)
            .map_err(|e| ExecError::from_fs("shadow tree snapshot", &shadow.root, e))?;
        let now: Vec<(PathBuf, FileHash)> =
            now_snap.iter().map(|e| (e.path.clone(), e.hash)).collect();
        let files = compute_change_entries(CHILD_ID, &base_map, &now, &base_map)?;
        let cs = ChangeSet {
            child_id: CHILD_ID.to_string(),
            base_id: base_id_of(&shadow.shadow_id),
            run_base_snapshot: Some(run_base.snapshot_hash),
            child_start_snapshot: Some(base_map_digest(&base_map)),
            final_child_snapshot: Some(base_map_digest(&now)),
            files,
            created_ms: self.manager.now_ms(),
        };
        put_change_set(&self.manager, session, &shadow.shadow_id, &cs)?;
        Ok(cs)
    }

    /// Present the session's staged change set (staging it first when
    /// absent). A stored set bound to a DIFFERENT run base than the shadow's
    /// durable record is stale and is re-staged — a stale generation can
    /// never be presented as the current candidate.
    pub fn present_change_set(&self, session: SessionId) -> Result<ChangeSet, ExecError> {
        let row = self.live_row(session)?;
        let handle = parent_handle(&self.manager, session)?;
        let run_base = handle
            .ledger_run_base_get(&row.shadow_id)
            .map_err(|e| ExecError::Internal(format!("shadow run base read: {e}")))?
            .ok_or_else(|| {
                ExecError::InvalidState(format!(
                    "shadow {} has no durable run base; discard() it and begin_shadow() again",
                    row.shadow_id
                ))
            })?;
        let cs_id = format!("{}-cs", base_id_of(&row.shadow_id));
        match read_change_set(&self.manager, session, &row.shadow_id, CHILD_ID, &cs_id) {
            Ok(cs) if cs.run_base_snapshot.as_deref() == Some(run_base.snapshot_hash.as_str()) => {
                Ok(cs)
            }
            Ok(_) => self.stage_change_set(session),
            Err(ExecError::NotFound(_)) => self.stage_change_set(session),
            Err(e) => Err(e),
        }
    }

    // -------------------------------------------------------- integration

    /// Mark the shadow INTEGRATED (record-first: the durable row reaches
    /// `Integrated` BEFORE the directory is removed, so a reader or a crash
    /// can never see filesystem cleanup with the row still live). The
    /// caller has already landed the verified candidate into the owner
    /// through the transactional integration engine; this only retires the
    /// shadow's own envelope.
    pub fn mark_integrated(&self, session: SessionId) -> Result<(), ExecError> {
        let Some(row) = self
            .manager
            .shadow_row(session)
            .map_err(|e| ExecError::Internal(format!("shadow row read: {e}")))?
        else {
            return Err(ExecError::NotFound(format!(
                "session {session} has no shadow row to integrate"
            )));
        };
        self.mark_state(session, ShadowRowState::Integrated)?;
        let dir = PathBuf::from(&row.root);
        if dir.exists() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        Ok(())
    }

    /// Mark the shadow `IntegrationBlocked`: the landing transaction refused
    /// (conflict or drift); the shadow directory is RETAINED so a later
    /// settlement can resume after the user resolves the drift.
    pub fn mark_integration_blocked(&self, session: SessionId) -> Result<(), ExecError> {
        self.mark_state(session, ShadowRowState::IntegrationBlocked)
    }

    // ------------------------------------------------------------ discard

    /// Remove the session's shadow directory and durably mark the row
    /// `Discarded` (the row itself stays: a tombstone, never silently
    /// dropped). Idempotent for a missing directory.
    pub fn discard(&self, session: SessionId) -> Result<(), ExecError> {
        let Some(row) = self
            .manager
            .shadow_row(session)
            .map_err(|e| ExecError::Internal(format!("shadow row read: {e}")))?
        else {
            return Err(ExecError::NotFound(format!(
                "session {session} has no shadow row to discard"
            )));
        };
        let dir = PathBuf::from(&row.root);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|e| {
                ExecError::Internal(format!("shadow removal {}: {e}", dir.display()))
            })?;
        }
        self.mark_state(session, ShadowRowState::Discarded)
    }

    // ------------------------------------------------------------ lifecycle

    /// Reopen recovery (crash residue, deterministic): every durable shadow
    /// row of every session is checked against the filesystem and the
    /// session table.
    /// - a LIVE row whose directory is gone → the shadow cannot integrate;
    ///   marked `Discarded` (deterministic: resume is impossible, discard
    ///   is the only path — a fresh shadow is begun by the next run);
    /// - a live row of a CLOSED session → the drive can never resume;
    ///   directory removed, row marked `Discarded`;
    /// - a row marked Integrated/Discarded whose directory still exists →
    ///   removed;
    /// - daemon-owned shadow directories without any row (a crash between
    ///   the copy and the row write) → removed.
    ///
    /// Returns a human-readable list of every action taken (empty = already
    /// consistent). Read/write access to rows goes through the session
    /// registry; nothing outside the daemon's shadow root is ever touched.
    pub fn reconcile(&self) -> Result<Vec<String>, ExecError> {
        let mut actions: Vec<String> = Vec::new();
        let Ok(sessions) = self.manager.list_sessions(None) else {
            return Err(ExecError::Internal("session list failed".into()));
        };
        let mut rowed_dirs: Vec<PathBuf> = Vec::new();
        for handle in sessions {
            let session = handle.id();
            let row = self
                .manager
                .shadow_row(session)
                .map_err(|e| ExecError::Internal(format!("shadow row read: {e}")))?;
            let Ok(srow) = handle.row() else {
                continue;
            };
            let session_terminal = srow.lifecycle.is_terminal();
            let Some(row) = row else {
                continue;
            };
            let dir = PathBuf::from(&row.root);
            rowed_dirs.push(dir.clone());
            if row.state.is_live() && !dir.is_dir() {
                // Shadow directory gone: integration is impossible.
                self.mark_state(session, ShadowRowState::Discarded)?;
                actions.push(format!(
                    "session {session}: shadow {} directory gone; marked discarded",
                    row.shadow_id
                ));
                continue;
            }
            if row.state.is_live() && session_terminal {
                // Record-first, like integration: the durable terminal state
                // precedes filesystem cleanup so no observer can catch a
                // live row with its directory already gone.
                self.mark_state(session, ShadowRowState::Discarded)?;
                if dir.is_dir() {
                    let _ = std::fs::remove_dir_all(&dir);
                }
                actions.push(format!(
                    "session {session}: closed with a live shadow {}; discarded",
                    row.shadow_id
                ));
                continue;
            }
            if !row.state.is_live() && dir.is_dir() {
                let _ = std::fs::remove_dir_all(&dir);
                actions.push(format!(
                    "session {session}: retired shadow {} directory removed",
                    row.shadow_id
                ));
            }
        }
        // Row-less daemon-owned shadow directories (crash between the copy
        // and the row write) are removed. The scan root is canonicalized so
        // its entries compare equal to the canonical row roots.
        let scan_root = self
            .shadows_root
            .canonicalize()
            .unwrap_or_else(|_| self.shadows_root.clone());
        if let Ok(entries) = std::fs::read_dir(&scan_root) {
            for e in entries.flatten() {
                let session_dir = e.path();
                let Ok(entries2) = std::fs::read_dir(&session_dir) else {
                    continue;
                };
                for e2 in entries2.flatten() {
                    let dir = e2.path();
                    if dir.is_dir() && !rowed_dirs.contains(&dir) {
                        let _ = std::fs::remove_dir_all(&dir);
                        actions.push(format!(
                            "row-less shadow directory {} removed",
                            dir.display()
                        ));
                    }
                }
            }
        }
        Ok(actions)
    }

    /// Graceful daemon shutdown: remove every shadow directory of every
    /// live row and mark the rows `Discarded`. Best effort (a shutdown must
    /// never fail the process); [`ShadowRoots::reconcile`] at the next
    /// daemon start handles whatever a crash left behind. Also invoked by
    /// the service's `Drop`.
    pub fn shutdown(&self) {
        let Ok(sessions) = self.manager.list_sessions(None) else {
            return;
        };
        for handle in sessions {
            let session = handle.id();
            let Ok(Some(row)) = self.manager.shadow_row(session) else {
                continue;
            };
            if row.state.is_live() {
                let dir = PathBuf::from(&row.root);
                if dir.exists() {
                    let _ = std::fs::remove_dir_all(&dir);
                }
                let mut retired = row;
                retired.state = ShadowRowState::Discarded;
                let _ = self.manager.put_shadow_row(session, &retired);
            }
        }
    }

    // ------------------------------------------------------------ internals

    fn ensure_shadows_root(&self) -> Result<PathBuf, ExecError> {
        std::fs::create_dir_all(&self.shadows_root).map_err(|e| {
            ExecError::Internal(format!("shadow root {}: {e}", self.shadows_root.display()))
        })?;
        // Canonical: the "base inside the shadow root" escape check compares
        // canonical paths — a `/var` vs `/private/var` spelling mismatch
        // would defeat starts_with.
        self.shadows_root
            .canonicalize()
            .map_err(|e| ExecError::Internal(format!("shadow root canonical: {e}")))
    }

    fn live_row(&self, session: SessionId) -> Result<ShadowRow, ExecError> {
        let Some(row) = self
            .manager
            .shadow_row(session)
            .map_err(|e| ExecError::Internal(format!("shadow row read: {e}")))?
        else {
            return Err(ExecError::NotFound(format!(
                "session {session} has no shadow; begin_shadow() first"
            )));
        };
        if !row.state.is_live() {
            return Err(ExecError::InvalidState(format!(
                "shadow {} of session {session} is {:?}; only Active/IntegrationBlocked shadows stage",
                row.shadow_id, row.state
            )));
        }
        Ok(row)
    }

    fn mark_state(&self, session: SessionId, state: ShadowRowState) -> Result<(), ExecError> {
        let Some(mut row) = self
            .manager
            .shadow_row(session)
            .map_err(|e| ExecError::Internal(format!("shadow row read: {e}")))?
        else {
            return Err(ExecError::NotFound(format!(
                "session {session} has no shadow row"
            )));
        };
        row.state = state;
        self.manager
            .put_shadow_row(session, &row)
            .map_err(|e| ExecError::Internal(format!("shadow row write: {e}")))
    }

    /// The durable base manifest of one shadow (the CAS anchors of the user
    /// checkout at begin). Missing = a crashed begin: refuse loudly — the
    /// only deterministic paths are discard (then begin again) or, when the
    /// manifest simply never got written, retrying begin after discard.
    fn base_manifest(&self, shadow: &Shadow) -> Result<Vec<(PathBuf, FileHash)>, ExecError> {
        match read_base_map(&self.manager, shadow.session_id, &shadow.shadow_id, CHILD_ID, "base")
        {
            Ok(Some(map)) => Ok(map),
            Ok(None) => Err(ExecError::InvalidState(format!(
                "shadow {} has no durable base manifest (crashed begin); discard() it and begin_shadow() again",
                shadow.shadow_id
            ))),
            Err(e) => Err(e),
        }
    }

    /// Test-only stable-copy drift seam: mutates the owner between the
    /// `before` digest and the copy. Production compiles to a no-op.
    #[cfg(test)]
    fn check_copy_seam(&self, owner: &Path, attempt: usize) {
        let mut guard = self.copy_seam.lock().expect("copy seam poisoned");
        let mode = *guard;
        match mode {
            Some(ShadowCopyDrift::Once) if attempt == 1 => {
                *guard = None;
                let _ = std::fs::write(
                    owner.join("shadow-copy-drift.txt"),
                    format!("drift on attempt {attempt}"),
                );
            }
            Some(ShadowCopyDrift::Every) => {
                let _ = std::fs::write(
                    owner.join("shadow-copy-drift.txt"),
                    format!("drift on attempt {attempt}"),
                );
            }
            _ => {}
        }
    }

    /// Test-only: arm the deterministic stable-copy drift seam of THIS
    /// service instance.
    #[cfg(test)]
    pub fn arm_copy_drift(&self, mode: ShadowCopyDrift) {
        *self.copy_seam.lock().expect("copy seam poisoned") = Some(mode);
    }

    #[cfg(not(test))]
    fn check_copy_seam(&self, _owner: &Path, _attempt: usize) {}
}

/// Map a canonical tree-manifest refusal of the shadow copy onto the typed
/// orchestrator error space: an oversize stays `Oversized` (a refused cap,
/// never corruption), an unavailable base stays `NotFound`, and every other
/// refusal (special file, malformed name, i/o) is a typed workspace drift —
/// the shadow is never begun from a tree the manifest cannot prove.
fn shadow_copy_error(what: &str, e: faktor_fs::tree_manifest::TreeManifestError) -> ExecError {
    use faktor_fs::tree_manifest::TreeManifestError as E;
    match e {
        E::Oversized(message) => ExecError::Oversized(format!("{what}: {message}")),
        E::RootUnavailable(message) => ExecError::NotFound(format!("{what}: {message}")),
        other => ExecError::WorkspaceDrift(format!("{what}: {other}")),
    }
}

/// Refuse a checkout carrying a symlink that a LITERAL manifest-faithful
/// copy could not keep inside the daemon-owned shadow: an absolute target
/// (it would point back at the user checkout) or a relative target that
/// climbs above `base`. A relative in-root link is copied literally and
/// resolves inside the shadow exactly as it did in the checkout, so it is
/// accepted. (The canonical manifest itself hashes link targets literally
/// and never follows them; this pre-check only keeps `begin_shadow`'s
/// historic no-escape contract with a manifest-faithful copy.)
fn reject_escaping_links(base: &Path, max_entries: usize) -> Result<(), ExecError> {
    use std::path::Component;
    let manifest = faktor_fs::tree_manifest::tree_manifest(base, max_entries).map_err(|e| {
        shadow_copy_error(&format!("shadow base manifest of {}", base.display()), e)
    })?;
    for entry in manifest.entries() {
        if entry.kind != faktor_fs::tree_manifest::TreeEntryKind::Symlink {
            continue;
        }
        let path = base.join(&entry.normalized_path);
        let target = std::fs::read_link(&path).map_err(|e| {
            ExecError::WorkspaceDrift(format!(
                "symlink {:?} cannot be read literally: {e}",
                entry.normalized_path
            ))
        })?;
        let refuse = |detail: String| {
            ExecError::WorkspaceDrift(format!(
                "symlink escape rejected: {:?} -> {detail}",
                entry.normalized_path
            ))
        };
        if target.is_absolute() {
            return Err(refuse(format!(
                "{} (an absolute target cannot stay inside the shadow copy)",
                target.display()
            )));
        }
        // Lexically resolve the relative target against the link's directory;
        // any climb above the base root is an escape.
        let mut depth = entry.normalized_path.matches('/').count();
        for component in target.components() {
            match component {
                Component::Normal(_) => depth += 1,
                Component::CurDir => {}
                Component::ParentDir => {
                    if depth == 0 {
                        return Err(refuse(format!(
                            "{} (climbs above the checkout root)",
                            target.display()
                        )));
                    }
                    depth -= 1;
                }
                _ => {
                    return Err(refuse(format!("{} (malformed)", target.display())));
                }
            }
        }
        // A relative link that cannot be resolved even in the checkout would
        // make the literal shadow copy unusable (and `snapshot_tree` refuses
        // it later anyway): refuse it here, before anything is written.
        if std::fs::canonicalize(&path).is_err() {
            return Err(refuse(format!("{} (broken link)", target.display())));
        }
    }
    Ok(())
}

impl Drop for ShadowRoots {
    fn drop(&mut self) {
        // Zero orphans on graceful teardown: remove every shadow of every
        // live row. Best effort by design (Drop cannot fail); reconcile()
        // on the next daemon start is the deterministic recovery of a
        // crash that skipped this.
        self.shutdown();
    }
}

#[cfg(test)]
#[path = "shadow_tests.rs"]
mod shadow_tests;
