//! Bounded generic-shell change attribution (P0-2 remainder).
//!
//! A generic shell (`Capability::ExecuteShell` — `run_command`) declares no
//! path args, so its mutations used to escape the change-accounting path
//! `write_file` feeds (checkpoint/snapshot rows, the turn summary's changed
//! files, verification inputs). This module closes that gap:
//!
//! 1. BEFORE the shell may execute, the runtime captures a bounded canonical
//!    workspace manifest (whole-file content hash per file, honoring the
//!    canonical manifest's VCS/temp ignore rules PLUS the runtime's build/
//!    dependency ignore rules [`SHELL_MANIFEST_SKIP_DIRS`]) and records a
//!    small durable envelope on the tool-run row (`record_tool_pre_manifest`)
//!    naming the CAS-stored manifest. The exact walk is bounded by
//!    [`SHELL_MANIFEST_MAX_ENTRIES`] / [`SHELL_MANIFEST_MAX_READ_BYTES`] /
//!    [`MAX_TREE_MANIFEST_DEPTH`]; exhaustion is a typed refusal BEFORE
//!    execution (a shell that cannot be accounted never runs).
//! 2. BEFORE execution the pre-images of regular files within
//!    [`SHELL_MANIFEST_CAPTURE_FILE_BYTES`] (up to
//!    [`SHELL_MANIFEST_CAPTURE_MAX_BYTES`] total) are stored in the CAS, so
//!    the post-execution diff can record REAL `CheckpointStore` rows for
//!    modified/deleted files (rollback needs the before bytes). Files whose
//!    pre-image could not be captured are still accounted as changes; their
//!    checkpoint is skipped with the omission typed on the envelope
//!    ([`ShellManifestCapture::omitted_blobs`]) and the review/verification
//!    path falls back to the bounded disk read.
//! 3. AFTER execution the post-manifest is captured and diffed against the
//!    pre-manifest. Added / Deleted / Modified paths (a rename is
//!    delete+add by construction) feed the SAME accounting consumers as
//!    `write_file`: checkpoint rows through the session's `CheckpointStore`,
//!    `TurnSummary::files_changed`, the durable `FileChanged` payload, the
//!    changed-file progress digests, and (via the summary) the end-of-turn
//!    verification/review inputs. The diff is capped at
//!    [`SHELL_MANIFEST_MAX_CHANGES`] with an explicit truncation reason, and
//!    the durable change lists are additionally capped at
//!    [`SHELL_CHANGE_DURABLE_MAX`] (typed, never silently shortened).
//! 4. Crash safety: the durable pre-manifest envelope is written before the
//!    shell may mutate anything. If the process dies mid-shell (or after the
//!    shell mutated but before settlement), restart reconciliation diffs the
//!    durable pre-manifest against the CURRENT tree, stores the discovered
//!    change list on the row and a `shell_change` memory fact, and the turn's
//!    completion gate refuses certification with
//!    [`ReasonCode::UnattributedChange`] — the unknown state is never read as
//!    "unchanged".

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;

use faktor_cas::Cas;
use faktor_core::hash::FileHash;
use faktor_core::id::OpId;
use faktor_core::op::EffectStatus;
use faktor_fs::rooted::WalkBudget;
use faktor_fs::tree_manifest::{
    tree_manifest_with_skip_budgeted, TreeEntryKind, TreeManifest, MAX_TREE_MANIFEST_DEPTH,
};
use faktor_fs::WorkspaceHandle;
use faktor_snapshot::FileState;

use super::*;

/// Hard bound on one shell manifest walk: trees beyond it refuse the shell
/// BEFORE it can mutate anything (documented cap; typed `Oversized`).
pub(crate) const SHELL_MANIFEST_MAX_ENTRIES: usize = 20_000;

/// Whole-operation read budget of one shell manifest walk (bytes hashed):
/// a tree whose manifest walk would read more than this refuses the shell
/// (bounded everything), never a partial manifest.
pub(crate) const SHELL_MANIFEST_MAX_READ_BYTES: u64 = 256 * 1024 * 1024;

/// Byte bound of the serialized pre-manifest CAS blob. Beyond it the
/// envelope is stored WITHOUT a manifest ref and the run is typed
/// unattributed (recovery still discovers nothing silently).
pub(crate) const SHELL_MANIFEST_MAX_SERIALIZED_BYTES: usize = 16 * 1024 * 1024;

/// Per-file pre-image capture bound: regular files at or below this size get
/// their before bytes stored in the CAS during the pre-manifest walk (so a
/// later modification/deletion can be checkpointed for rollback/review).
pub(crate) const SHELL_MANIFEST_CAPTURE_FILE_BYTES: usize = 4 * 1024 * 1024;

/// Whole-operation pre-image capture bound. Beyond it capture stops and the
/// omission is typed on the envelope (changes are still accounted).
pub(crate) const SHELL_MANIFEST_CAPTURE_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Bound of one shell diff report (`ShellChangeSet::changes`); beyond it the
/// change set carries an explicit truncation reason.
pub(crate) const SHELL_MANIFEST_MAX_CHANGES: usize = 1024;

/// Bound of the change list written to the durable run-row envelope, the
/// `FileChanged` payload and the reconciliation event (typed truncation on
/// the envelope when exceeded). 256 matches the turn ledger's changed-file
/// cap, so a bounded report is never silently smaller than the ledger.
pub(crate) const SHELL_CHANGE_DURABLE_MAX: usize = 256;

/// Per-path cap inside the durable `FileChanged` payload / memory fact.
pub(crate) const SHELL_CHANGE_PATH_MAX_BYTES: usize = 200;

/// Bound of the path list mirrored into the small (`MAX_FACT_VALUE_BYTES`
/// capped) `shell_change` memory fact; the full list stays on the run row.
pub(crate) const SHELL_CHANGE_FACT_MAX_PATHS: usize = 16;

/// Directory names never entered by the shell-attribution walk IN ADDITION
/// to the canonical manifest's VCS skip set ([`TREE_MANIFEST_SKIP_DIRS`]):
/// the runtime's existing build/dependency ignore rules (the search walker
/// skips `node_modules` and `target*`; here the exact names are used) plus
/// the agent's own tooling state (`.faktor`). A large ignored tree therefore
/// never touches the manifest caps.
pub(crate) const SHELL_MANIFEST_SKIP_DIRS: &[&str] = &[".faktor", "node_modules", "target"];

/// Durable memory-fact kind of one shell run's change-attribution state.
pub(crate) const SHELL_CHANGE_FACT_KIND: &str = "shell_change";

/// Stable typed tag of one unattributed shell mutation, appended to the
/// outcome text so the wire result is machine-distinguishable without
/// parsing prose.
pub(crate) const SHELL_UNATTRIBUTED_MARK: &str = "[shell-changes:unattributed]";

/// The canonical status of one manifested change (rename = delete+add).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ShellChangeStatus {
    Added,
    Deleted,
    Modified,
}

impl ShellChangeStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Deleted => "deleted",
            Self::Modified => "modified",
        }
    }
}

/// One actual change discovered by the shell manifest diff. `before_hash` /
/// `after_hash` are lowercase BLAKE3 hex; a missing side is `None`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ShellFileChange {
    pub path: String,
    pub status: ShellChangeStatus,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
}

impl ShellFileChange {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "path": self.path,
            "status": self.status.as_str(),
            "before": self.before_hash,
            "after": self.after_hash,
        })
    }
}

/// The bounded actual-change set of one shell invocation.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ShellChangeSet {
    pub changes: Vec<ShellFileChange>,
    /// Typed reason when `changes` was truncated (or the post-tree could not
    /// be proven); `None` means the diff is complete.
    pub truncated: Option<String>,
    pub before_digest: Option<String>,
    pub after_digest: Option<String>,
    /// Paths whose pre-image blob already exists in the CAS (checkpointable
    /// modifications/deletions).
    pub pre_images: BTreeMap<String, String>,
    /// Pre-images omitted by the capture bounds/errors (still accounted).
    pub omitted_pre_images: usize,
}

impl ShellChangeSet {
    /// The durable subset of the change list (bounded, `/`-normalized).
    pub(crate) fn durable_changes(&self) -> (Vec<serde_json::Value>, bool) {
        let truncated = self.truncated.is_some() || self.changes.len() > SHELL_CHANGE_DURABLE_MAX;
        let rows = self
            .changes
            .iter()
            .take(SHELL_CHANGE_DURABLE_MAX)
            .map(ShellFileChange::to_json)
            .collect();
        (rows, truncated)
    }
}

/// One pre-shell manifest capture: the canonical manifest plus the durable
/// envelope and the pre-image availability map.
#[derive(Debug, Clone)]
pub(crate) struct ShellManifestCapture {
    pub manifest: TreeManifest,
    pub digest: Option<String>,
    /// CAS hash (hex) of the serialized pre-manifest, when it fit the bound.
    pub manifest_cas: Option<String>,
    pub truncated: Option<String>,
    pub pre_images: BTreeMap<String, String>,
    pub omitted_pre_images: usize,
}

/// Build the shared whole-operation walk budget of one shell manifest walk.
pub(crate) fn shell_manifest_budget() -> WalkBudget {
    WalkBudget::new(
        SHELL_MANIFEST_MAX_ENTRIES,
        SHELL_MANIFEST_MAX_ENTRIES.saturating_add(1),
        MAX_TREE_MANIFEST_DEPTH,
        u64::MAX,
        SHELL_MANIFEST_MAX_READ_BYTES,
    )
}

/// Capture the canonical manifest of `ws`'s root under the documented shell
/// bounds and ignore rules. The error string is a typed, bounded refusal
/// reason (never a partial manifest).
pub(crate) fn capture_manifest(ws: &WorkspaceHandle) -> Result<TreeManifest, String> {
    let mut budget = shell_manifest_budget();
    tree_manifest_with_skip_budgeted(ws.root(), &mut budget, SHELL_MANIFEST_SKIP_DIRS)
        .map_err(|e| format!("shell change manifest: {e}"))
}

/// Capture the pre-shell manifest and, when a CAS is wired, store its
/// serialized form plus the bounded pre-image blobs of regular files. The
/// manifest itself is a complete walk or a typed refusal (never partial).
pub(crate) fn capture_pre_shell(
    ws: &WorkspaceHandle,
    cas: Option<&Cas>,
) -> Result<ShellManifestCapture, String> {
    let manifest = capture_manifest(ws)?;
    let digest = match manifest.digest() {
        Ok(digest) => Some(digest),
        Err(e) => Some(format!("unprovable-tree-digest: {e}")),
    };
    let mut truncated: Option<String> = match &digest {
        Some(d) if d.starts_with("unprovable-tree-digest") => Some(d.clone()),
        _ => None,
    };
    let mut manifest_cas = None;
    if let Some(cas) = cas {
        match serde_json::to_vec(&manifest) {
            Ok(bytes) if bytes.len() <= SHELL_MANIFEST_MAX_SERIALIZED_BYTES => {
                match cas.put(&bytes) {
                    Ok(hash) => manifest_cas = Some(hash.to_hex()),
                    Err(e) => {
                        truncated = Some(format!(
                            "pre-manifest could not be stored durably: {e}; \
                             crash reconciliation unavailable"
                        ));
                    }
                }
            }
            Ok(bytes) => {
                truncated = Some(format!(
                    "serialized pre-manifest of {} bytes exceeds the {} byte bound",
                    bytes.len(),
                    SHELL_MANIFEST_MAX_SERIALIZED_BYTES
                ));
            }
            Err(e) => {
                truncated = Some(format!("pre-manifest serialization failed: {e}"));
            }
        }
    } else {
        truncated = Some("no content store wired for the pre-manifest".into());
    }
    // Bounded pre-image capture: regular files within the per-file cap, up
    // to the whole-operation cap. Hashes are re-checked against the manifest
    // entry, so a torn read can never make an unaccountable pre-image.
    let mut pre_images = BTreeMap::new();
    let mut omitted_pre_images = 0usize;
    let mut captured_bytes = 0u64;
    if let Some(cas) = cas {
        for entry in manifest.entries() {
            if entry.kind != TreeEntryKind::Regular {
                continue;
            }
            if captured_bytes >= SHELL_MANIFEST_CAPTURE_MAX_BYTES {
                omitted_pre_images += 1;
                continue;
            }
            let path = std::path::Path::new(&entry.normalized_path);
            let Ok(data) = ws.read(path, SHELL_MANIFEST_CAPTURE_FILE_BYTES) else {
                omitted_pre_images += 1;
                continue;
            };
            let Some(hash) = data.full_hash() else {
                // Over the per-file cap: no pre-image (still manifested).
                omitted_pre_images += 1;
                continue;
            };
            if hash.to_hex() != entry.payload_digest || cas.put(&data.bytes).is_err() {
                omitted_pre_images += 1;
                continue;
            }
            captured_bytes = captured_bytes.saturating_add(data.bytes.len() as u64);
            pre_images.insert(entry.normalized_path.clone(), hash.to_hex());
        }
    } else {
        omitted_pre_images = manifest.entries().len();
        truncated.get_or_insert_with(|| "no content store wired for pre-images".into());
    }
    Ok(ShellManifestCapture {
        manifest,
        digest,
        manifest_cas,
        truncated,
        pre_images,
        omitted_pre_images,
    })
}

impl ShellManifestCapture {
    /// The durable pre-shell row envelope (`state:"captured"`).
    pub(crate) fn envelope(&self, turn_op: OpId) -> serde_json::Value {
        serde_json::json!({
            "schema": 1,
            "state": "captured",
            "turn_op": turn_op.raw(),
            "cas": self.manifest_cas,
            "digest": self.digest,
            "entries": self.manifest.entries().len(),
            "omitted_pre_images": self.omitted_pre_images,
            "truncated": self.truncated,
        })
    }

    /// The small `shell_change` memory fact of the captured state.
    pub(crate) fn fact_value(
        &self,
        state: &str,
        turn_op: OpId,
        tool_op: OpId,
        paths: &[String],
        total: usize,
    ) -> serde_json::Value {
        let paths: Vec<String> = paths
            .iter()
            .take(SHELL_CHANGE_FACT_MAX_PATHS)
            .map(|p| truncate(p, SHELL_CHANGE_PATH_MAX_BYTES))
            .collect();
        serde_json::json!({
            "state": state,
            "turn_op": turn_op.raw(),
            "tool_op": tool_op.raw(),
            "changed": total,
            "truncated": self.truncated,
            "paths": paths,
        })
    }
}

/// Diff two canonical manifests into the bounded actual change set. Both
/// inputs are sorted by path; the merge is linear and a rename surfaces as
/// delete+add with no fabricated pairing.
pub(crate) fn diff_manifests(
    before: &TreeManifest,
    after: &TreeManifest,
    pre_images: BTreeMap<String, String>,
    omitted_pre_images: usize,
    before_digest: Option<String>,
    after_digest: Option<String>,
) -> ShellChangeSet {
    let mut changes: Vec<ShellFileChange> = Vec::new();
    let mut truncated = None;
    let mut i = 0usize;
    let mut j = 0usize;
    let before_entries = before.entries();
    let after_entries = after.entries();
    while i < before_entries.len() || j < after_entries.len() {
        if changes.len() > SHELL_MANIFEST_MAX_CHANGES {
            truncated = Some(format!(
                "more than {} changed paths (report truncated at the bound)",
                SHELL_MANIFEST_MAX_CHANGES
            ));
            break;
        }
        let b = before_entries.get(i);
        let a = after_entries.get(j);
        let change = match (b, a) {
            (Some(b), Some(a)) => match b.normalized_path.cmp(&a.normalized_path) {
                std::cmp::Ordering::Less => {
                    i += 1;
                    Some(ShellFileChange {
                        path: b.normalized_path.clone(),
                        status: ShellChangeStatus::Deleted,
                        before_hash: Some(b.payload_digest.clone()),
                        after_hash: None,
                    })
                }
                std::cmp::Ordering::Greater => {
                    j += 1;
                    Some(ShellFileChange {
                        path: a.normalized_path.clone(),
                        status: ShellChangeStatus::Added,
                        before_hash: None,
                        after_hash: Some(a.payload_digest.clone()),
                    })
                }
                std::cmp::Ordering::Equal => {
                    i += 1;
                    j += 1;
                    (b.kind != a.kind || b.mode != a.mode || b.payload_digest != a.payload_digest)
                        .then(|| ShellFileChange {
                            path: a.normalized_path.clone(),
                            status: ShellChangeStatus::Modified,
                            before_hash: Some(b.payload_digest.clone()),
                            after_hash: Some(a.payload_digest.clone()),
                        })
                }
            },
            (Some(b), None) => {
                i += 1;
                Some(ShellFileChange {
                    path: b.normalized_path.clone(),
                    status: ShellChangeStatus::Deleted,
                    before_hash: Some(b.payload_digest.clone()),
                    after_hash: None,
                })
            }
            (None, Some(a)) => {
                j += 1;
                Some(ShellFileChange {
                    path: a.normalized_path.clone(),
                    status: ShellChangeStatus::Added,
                    before_hash: None,
                    after_hash: Some(a.payload_digest.clone()),
                })
            }
            (None, None) => None,
        };
        if let Some(change) = change {
            changes.push(change);
        }
    }
    ShellChangeSet {
        changes,
        truncated,
        before_digest,
        after_digest,
        pre_images,
        omitted_pre_images,
    }
}

/// The stable memory-fact key of one shell run's attribution state.
pub(crate) fn fact_key(turn_op: OpId, tool_op: OpId) -> String {
    format!("{}:{}", turn_op.raw(), tool_op.raw())
}

/// Read the current shell-change facts of one turn: `(all_paths, unresolved)`
/// where `unresolved` is true for any fact state that never proved the actual
/// tree (`captured` / `discovered` / `unattributed`).
pub(crate) fn shell_change_facts(
    handle: &faktor_session::SessionHandle,
    turn_op: OpId,
) -> (Vec<String>, bool) {
    let Ok(facts) = handle.memory_facts() else {
        // An unreadable fact table is never read as "no shell changes": the
        // caller refuses certification (conservative).
        return (Vec::new(), true);
    };
    let mut paths = Vec::new();
    let mut unresolved = false;
    for (kind, _key, value) in facts {
        if kind != SHELL_CHANGE_FACT_KIND {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&value) else {
            unresolved = true;
            continue;
        };
        if value.get("turn_op").and_then(|v| v.as_u64()) != Some(turn_op.raw()) {
            continue;
        }
        match value.get("state").and_then(|v| v.as_str()) {
            Some("reconciled") => {}
            Some(_) => unresolved = true,
            None => unresolved = true,
        }
        if let Some(list) = value.get("paths").and_then(|v| v.as_array()) {
            for path in list {
                if let Some(path) = path.as_str() {
                    paths.push(path.to_string());
                }
            }
        }
    }
    (paths, unresolved)
}

/// Refuse a shell BEFORE execution when the pre-manifest cannot be captured:
/// an unaccountable shell never runs. The typed reason rides the outcome.
pub(crate) fn refused_shell_outcome(reason: &str) -> ToolOutcome {
    ToolOutcome {
        text: format!("{SHELL_UNATTRIBUTED_MARK} shell refused before execution: {reason}"),
        exit_code: Some(1),
        effect_status: EffectStatus::Unknown,
        ..Default::default()
    }
}

impl AgentRuntime {
    /// Run one generic shell with bounded pre/post manifest attribution. The
    /// pre-manifest envelope is durable BEFORE the shell may execute; the
    /// post-manifest diff is stashed in `shell_changes` for settlement to
    /// feed checkpoints/summary/proof inputs.
    pub(crate) async fn execute_shell_with_manifest(
        self: &Arc<Self>,
        handle: &faktor_session::SessionHandle,
        tool: Arc<Tool>,
        ctx: ToolRunCtx,
        args: serde_json::Value,
        turn_op: OpId,
        shell_changes: &Arc<std::sync::Mutex<HashMap<OpId, ShellChangeSet>>>,
    ) -> faktor_core::Result<ToolOutcome> {
        let Some(ws) = ctx.workspace.clone() else {
            // No workspace: the tool errors honestly, nothing can be
            // attributed and nothing can have been mutated through it.
            return (tool.execute)(ctx, args).await;
        };
        let op = ctx.op_id;
        let cas = self.deps.cas.as_deref();
        let capture = match capture_pre_shell(&ws, cas) {
            Ok(capture) => capture,
            Err(reason) => {
                tracing::warn!(
                    session = %handle.id(),
                    op = %op,
                    "generic shell refused: bounded change accounting is unavailable: {reason}"
                );
                return Ok(refused_shell_outcome(&reason));
            }
        };
        let envelope = capture.envelope(turn_op);
        if let Err(e) = handle.record_tool_pre_manifest(op, &envelope) {
            return Ok(refused_shell_outcome(&format!(
                "the durable pre-manifest could not be recorded: {e}"
            )));
        }
        let captured_fact = capture.fact_value("captured", turn_op, op, &[], 0);
        let _ = handle.upsert_memory_fact(
            SHELL_CHANGE_FACT_KIND,
            &fact_key(turn_op, op),
            &captured_fact.to_string(),
        );
        let outcome = (tool.execute)(ctx, args).await?;
        match capture_manifest(&ws) {
            Ok(after) => {
                let set = diff_manifests(
                    &capture.manifest,
                    &after,
                    capture.pre_images.clone(),
                    capture.omitted_pre_images,
                    capture.digest.clone(),
                    after.digest().ok(),
                );
                shell_changes.lock().unwrap().insert(op, set);
            }
            Err(reason) => {
                // The shell ran, the post tree cannot be proven: keep the
                // outcome but land the typed unattributed state so the turn's
                // completion gate refuses certification.
                let _ = handle.upsert_memory_fact(
                    SHELL_CHANGE_FACT_KIND,
                    &fact_key(turn_op, op),
                    &capture
                        .fact_value("unattributed", turn_op, op, &[], 0)
                        .to_string(),
                );
                let envelope = serde_json::json!({
                    "schema": 1,
                    "state": "unattributed",
                    "turn_op": turn_op.raw(),
                    "cas": capture.manifest_cas,
                    "digest": capture.digest,
                    "entries": capture.manifest.entries().len(),
                    "omitted_pre_images": capture.omitted_pre_images,
                    "truncated": reason,
                });
                let _ = handle.record_tool_pre_manifest(op, &envelope);
                tracing::error!(
                    session = %handle.id(),
                    op = %op,
                    "shell mutated the workspace but its post-manifest is unavailable: {reason}"
                );
            }
        }
        Ok(outcome)
    }

    /// Apply one settled shell change set to the SAME accounting path
    /// `write_file` feeds: per-path checkpoint rows (when the pre-image and
    /// the after bytes are available), the turn summary's changed files, the
    /// durable row/fact envelopes and the per-path progress digest. Returns
    /// the number of checkpoint rows recorded.
    pub(crate) fn settle_shell_changes(
        &self,
        handle: &faktor_session::SessionHandle,
        op_id: OpId,
        turn_op: OpId,
        set: &ShellChangeSet,
        turn_summary: &mut faktor_context::ledger::TurnSummary,
        ws: &WorkspaceHandle,
    ) -> usize {
        let mut checkpointed = 0usize;
        if let Some(snapshots) = &self.deps.snapshots {
            for change in &set.changes {
                let before = match &change.before_hash {
                    Some(raw) => match FileHash::from_hex(raw) {
                        Some(hash) => FileState::existing(hash),
                        None => FileState::missing(),
                    },
                    None => FileState::missing(),
                };
                let after = match &change.after_hash {
                    Some(raw) => match FileHash::from_hex(raw) {
                        Some(hash) => FileState::existing(hash),
                        None => FileState::missing(),
                    },
                    None => FileState::missing(),
                };
                if before == after {
                    continue;
                }
                // The after bytes are read from the current tree and their
                // hash must equal the post-manifest entry; a torn read is a
                // refused checkpoint, never a wrong one.
                let after_content = if after.exists {
                    match ws.read(
                        std::path::Path::new(&change.path),
                        SHELL_MANIFEST_CAPTURE_FILE_BYTES,
                    ) {
                        Ok(data) => match data.full_hash() {
                            Some(hash) if Some(hash.to_hex()) == change.after_hash => {
                                Some(data.bytes)
                            }
                            _ => None,
                        },
                        Err(_) => None,
                    }
                } else {
                    None
                };
                if after.exists && after_content.is_none() {
                    continue;
                }
                // before_content stays None: modifications/deletions require
                // the pre-image blob captured before the shell ran (the
                // store verifies it exists); a missing one fails the whole
                // record loudly, so the checkpoint is skipped, never faked.
                match snapshots.record_change(
                    handle.id(),
                    &change.path,
                    before,
                    None,
                    after,
                    after_content.as_deref(),
                ) {
                    Ok(_) => checkpointed += 1,
                    Err(e) => {
                        tracing::warn!(
                            session = %handle.id(),
                            op = %op_id,
                            path = %change.path,
                            "shell change checkpoint skipped (pre-image or after bytes unavailable): {e}"
                        );
                    }
                }
            }
        }
        for change in &set.changes {
            let path = truncate(&change.path, 300);
            if !turn_summary.files_changed.contains(&path) {
                turn_summary.files_changed.push(path);
            }
        }
        let (rows, list_truncated) = set.durable_changes();
        let envelope = serde_json::json!({
            "schema": 1,
            "state": "reconciled",
            "turn_op": turn_op.raw(),
            "tool_op": op_id.raw(),
            "changes": rows,
            "changed": set.changes.len(),
            "truncated": set.truncated.clone().or_else(|| list_truncated.then(|| {
                format!("change list truncated at {SHELL_CHANGE_DURABLE_MAX} durable rows")
            })),
            "before_digest": set.before_digest,
            "after_digest": set.after_digest,
            "omitted_pre_images": set.omitted_pre_images,
        });
        let _ = handle.record_tool_pre_manifest(op_id, &envelope);
        let paths: Vec<String> = set
            .changes
            .iter()
            .take(SHELL_CHANGE_FACT_MAX_PATHS)
            .map(|c| truncate(&c.path, SHELL_CHANGE_PATH_MAX_BYTES))
            .collect();
        let fact = serde_json::json!({
            "state": "reconciled",
            "turn_op": turn_op.raw(),
            "tool_op": op_id.raw(),
            "changed": set.changes.len(),
            "truncated": set.truncated,
            "paths": paths,
        });
        let _ = handle.upsert_memory_fact(
            SHELL_CHANGE_FACT_KIND,
            &fact_key(turn_op, op_id),
            &fact.to_string(),
        );
        for change in &set.changes {
            if let Some(raw) = &change.after_hash {
                if let Some(hash) = FileHash::from_hex(raw) {
                    self.progress_repo_digest(handle.id(), &change.path, hash);
                }
            }
        }
        tracing::info!(
            session = %handle.id(),
            op = %op_id,
            changed = set.changes.len(),
            checkpointed,
            truncated = set.truncated.is_some(),
            "generic shell changes attributed"
        );
        checkpointed
    }
}

/// Read a CAS-stored pre-manifest blob back under the documented bound.
pub(crate) fn load_pre_manifest(cas: &Cas, hash_hex: &str) -> Result<TreeManifest, String> {
    let hash = FileHash::from_hex(hash_hex)
        .ok_or_else(|| format!("pre-manifest CAS hash {hash_hex:?} is not valid hex"))?;
    let bytes = cas
        .get_bounded(hash, SHELL_MANIFEST_MAX_SERIALIZED_BYTES)
        .map_err(|e| format!("pre-manifest CAS read failed: {e}"))?
        .ok_or_else(|| "pre-manifest CAS blob is missing".to_string())?;
    serde_json::from_slice(&bytes).map_err(|e| format!("pre-manifest decode failed: {e}"))
}

/// Crash reconciliation of ONE shell run row: diff the durable pre-manifest
/// against the current tree, persist the discovered change list on the row
/// and the small unresolved fact, and return the discovered set. Returns
/// `None` when the row carries no usable captured pre-manifest (the caller
/// still refuses certification through the row's unattributed state).
pub(crate) fn reconcile_shell_row(
    handle: &faktor_session::SessionHandle,
    row: &ToolRunRow,
    ws: &WorkspaceHandle,
    cas: Option<&Cas>,
) -> Option<ShellChangeSet> {
    let envelope = row.pre_manifest.as_ref()?;
    if envelope.get("state").and_then(|v| v.as_str()) != Some("captured") {
        return None;
    }
    let turn_op = envelope
        .get("turn_op")
        .and_then(|v| v.as_u64())
        .map(OpId::new)?;
    let pre_images_available = envelope
        .get("omitted_pre_images")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let result = (|| -> Result<(ShellChangeSet, usize), String> {
        let cas = cas.ok_or_else(|| "no content store wired".to_string())?;
        let cas_hash = envelope
            .get("cas")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                "the durable pre-manifest was never stored (captured without CAS)".to_string()
            })?;
        let before = load_pre_manifest(cas, cas_hash)?;
        let after = capture_manifest(ws)?;
        let entries = before.entries().len();
        let set = diff_manifests(
            &before,
            &after,
            BTreeMap::new(),
            pre_images_available as usize,
            envelope
                .get("digest")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            after.digest().ok(),
        );
        Ok((set, entries))
    })();
    let (set, before_entries, failure) = match result {
        Ok((set, entries)) => (set, entries, None),
        Err(reason) => (
            ShellChangeSet::default(),
            0,
            Some(format!("crash reconciliation failed: {reason}")),
        ),
    };
    let (rows, list_truncated) = set.durable_changes();
    let truncated = failure.clone().or_else(|| {
        set.truncated.clone().or_else(|| {
            list_truncated.then(|| {
                format!("change list truncated at {SHELL_CHANGE_DURABLE_MAX} durable rows")
            })
        })
    });
    let state = if failure.is_some() {
        "unattributed"
    } else {
        "discovered"
    };
    let envelope = serde_json::json!({
        "schema": 1,
        "state": state,
        "turn_op": turn_op.raw(),
        "tool_op": row.op_id.raw(),
        "cas": row
            .pre_manifest
            .as_ref()
            .and_then(|e| e.get("cas"))
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "digest": row
            .pre_manifest
            .as_ref()
            .and_then(|e| e.get("digest"))
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "entries": before_entries,
        "changes": rows,
        "changed": set.changes.len(),
        "truncated": truncated,
        "before_digest": set.before_digest,
        "after_digest": set.after_digest,
        "discovered_at_restart": true,
    });
    let _ = handle.record_tool_pre_manifest(row.op_id, &envelope);
    let paths: Vec<String> = set
        .changes
        .iter()
        .take(SHELL_CHANGE_FACT_MAX_PATHS)
        .map(|c| truncate(&c.path, SHELL_CHANGE_PATH_MAX_BYTES))
        .collect();
    let fact = serde_json::json!({
        "state": state,
        "turn_op": turn_op.raw(),
        "tool_op": row.op_id.raw(),
        "changed": set.changes.len(),
        "truncated": truncated,
        "paths": paths,
    });
    let _ = handle.upsert_memory_fact(
        SHELL_CHANGE_FACT_KIND,
        &fact_key(turn_op, row.op_id),
        &fact.to_string(),
    );
    tracing::error!(
        session = %handle.id(),
        op = %row.op_id,
        state,
        changed = set.changes.len(),
        "crashed generic shell reconciled against the durable pre-manifest; \
         the discovered tree can never be certified without verification"
    );
    Some(set)
}

/// Refuse a completion gate over an unresolved shell attribution with the
/// typed [`ReasonCode::UnattributedChange`] reason. Keeps an existing
/// non-passing gate's reasons and adds this one.
pub(crate) fn refuse_unattributed_gate(
    gate: Option<CompletionGate>,
    detail: &str,
) -> Option<CompletionGate> {
    let reason = faktor_core::state::OutcomeReason::new(
        faktor_core::state::ReasonCode::UnattributedChange,
        detail.to_string(),
    );
    match gate {
        Some(CompletionGate::VerifiedComplete) | Some(CompletionGate::Unverified) => {
            Some(CompletionGate::BlockedVerification {
                reasons: vec![reason],
            })
        }
        Some(CompletionGate::BlockedVerification { mut reasons })
        | Some(CompletionGate::FailedVerification { mut reasons }) => {
            reasons.push(reason);
            Some(CompletionGate::BlockedVerification { reasons })
        }
        // A pending background attempt certifies nothing yet: it stands and
        // re-checks at settlement.
        Some(gate @ CompletionGate::VerificationPending) => Some(gate),
        None => Some(CompletionGate::BlockedVerification {
            reasons: vec![reason],
        }),
    }
}

/// Mark one shell run unattributed WITHOUT a diff (the workspace/CAS needed
/// to prove the tree is unavailable): the durable fact forces the turn's
/// completion gate to refuse certification rather than assume no changes.
pub(crate) fn mark_shell_unattributed(
    handle: &faktor_session::SessionHandle,
    row: &ToolRunRow,
    turn_op: OpId,
    reason: &str,
) {
    let envelope = serde_json::json!({
        "schema": 1,
        "state": "unattributed",
        "turn_op": turn_op.raw(),
        "tool_op": row.op_id.raw(),
        "truncated": reason,
        "discovered_at_restart": true,
    });
    let _ = handle.record_tool_pre_manifest(row.op_id, &envelope);
    let fact = serde_json::json!({
        "state": "unattributed",
        "turn_op": turn_op.raw(),
        "tool_op": row.op_id.raw(),
        "changed": 0,
        "truncated": reason,
        "paths": [],
    });
    let _ = handle.upsert_memory_fact(
        SHELL_CHANGE_FACT_KIND,
        &fact_key(turn_op, row.op_id),
        &fact.to_string(),
    );
}

impl AgentRuntime {
    /// Crash-recovery reconciliation of every captured-but-unsettled shell
    /// run of one session (bounded newest-first scan). Runs once per recovery
    /// sweep, after the live-driver guard: a captured envelope means the
    /// shell may have mutated the tree, so the durable pre-manifest is diffed
    /// against the CURRENT tree and the discovered set is recorded. Returns
    /// true when anything was reconciled (the caller marks the report
    /// applied).
    pub(crate) fn reconcile_captured_shell_runs(
        &self,
        handle: &faktor_session::SessionHandle,
    ) -> bool {
        let Ok(rows) = handle.tool_runs_with_pre_manifest(64) else {
            return false;
        };
        let mut any = false;
        let workspace = (|| {
            let row = handle.row().ok()?;
            let root = self
                .deps
                .session
                .resolve_workspace_root(handle.id())
                .ok()
                .flatten()?;
            self.deps.workspaces.open(row.workspace_id, root).ok()
        })();
        for row in rows {
            let Some(envelope) = &row.pre_manifest else {
                continue;
            };
            if envelope.get("state").and_then(|v| v.as_str()) != Some("captured") {
                continue;
            }
            let Some(turn_op) = envelope
                .get("turn_op")
                .and_then(|v| v.as_u64())
                .map(OpId::new)
            else {
                mark_shell_unattributed(
                    handle,
                    &row,
                    OpId::new(1),
                    "captured shell envelope carries no turn identity",
                );
                any = true;
                continue;
            };
            match &workspace {
                Some(ws) => {
                    reconcile_shell_row(handle, &row, ws, self.deps.cas.as_deref());
                }
                None => {
                    mark_shell_unattributed(
                        handle,
                        &row,
                        turn_op,
                        "the session workspace could not be opened for crash reconciliation",
                    );
                }
            }
            any = true;
        }
        any
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_core::id::WorkspaceId;

    fn workspace(root: &std::path::Path) -> WorkspaceHandle {
        WorkspaceHandle::open_scoped(WorkspaceId::new(1), root.to_path_buf()).unwrap()
    }

    fn write(root: &std::path::Path, rel: &str, bytes: &[u8]) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    /// A rename is represented as delete+add by construction, and added/
    /// deleted/modified entries carry the real hashes of both states.
    #[test]
    fn manifest_diff_represents_add_delete_modify_and_rename() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        write(&root, "keep.txt", b"keep");
        write(&root, "gone.txt", b"gone");
        write(&root, "changed.txt", b"one");
        write(&root, "old_name.txt", b"moved");
        let before = capture_manifest(&workspace(&root)).unwrap();

        std::fs::remove_file(root.join("gone.txt")).unwrap();
        std::fs::write(root.join("changed.txt"), b"two").unwrap();
        write(&root, "created.txt", b"new");
        std::fs::rename(root.join("old_name.txt"), root.join("new_name.txt")).unwrap();

        let after = capture_manifest(&workspace(&root)).unwrap();
        let set = diff_manifests(
            &before,
            &after,
            BTreeMap::new(),
            0,
            before.digest().ok(),
            after.digest().ok(),
        );
        let mut rows: Vec<String> = set
            .changes
            .iter()
            .map(|c| format!("{} {}", c.status.as_str(), c.path))
            .collect();
        rows.sort();
        assert_eq!(
            rows,
            vec![
                "added created.txt",
                "added new_name.txt",
                "deleted gone.txt",
                "deleted old_name.txt",
                "modified changed.txt",
            ],
            "rename must be delete+add; every actual change must appear"
        );
        assert!(set.truncated.is_none(), "no truncation on a small diff");
        for change in &set.changes {
            match change.status {
                ShellChangeStatus::Added => assert!(change.before_hash.is_none()),
                ShellChangeStatus::Deleted => assert!(change.after_hash.is_none()),
                ShellChangeStatus::Modified => {
                    assert!(change.before_hash.is_some() && change.after_hash.is_some());
                }
            }
        }
    }

    /// A mode-only change and a symlink target change are real changes
    /// (canonical identity includes kind+mode).
    #[test]
    #[cfg(unix)]
    fn manifest_diff_sees_mode_and_symlink_target_changes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        write(&root, "tool.sh", b"#!/bin/sh\n");
        write(&root, "target.txt", b"t");
        std::os::unix::fs::symlink("target.txt", root.join("link")).unwrap();
        let before = capture_manifest(&workspace(&root)).unwrap();

        std::fs::set_permissions(root.join("tool.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::remove_file(root.join("link")).unwrap();
        std::os::unix::fs::symlink("other.txt", root.join("link")).unwrap();

        let after = capture_manifest(&workspace(&root)).unwrap();
        let set = diff_manifests(&before, &after, BTreeMap::new(), 0, None, None);
        let mut rows: Vec<String> = set
            .changes
            .iter()
            .map(|c| format!("{} {}", c.status.as_str(), c.path))
            .collect();
        rows.sort();
        assert_eq!(rows, vec!["modified link", "modified tool.sh"]);
    }

    /// The report is bounded with a typed truncation reason, never silently
    /// shortened.
    #[test]
    fn manifest_diff_is_bounded_and_typed_on_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..(SHELL_MANIFEST_MAX_CHANGES + 8) {
            write(&root, &format!("f{i:05}.txt"), b"x");
        }
        let before = capture_manifest(&workspace(&root)).unwrap();
        for i in 0..(SHELL_MANIFEST_MAX_CHANGES + 8) {
            std::fs::remove_file(root.join(format!("f{i:05}.txt"))).unwrap();
        }
        let after = capture_manifest(&workspace(&root)).unwrap();
        let set = diff_manifests(&before, &after, BTreeMap::new(), 0, None, None);
        assert!(
            set.truncated.is_some(),
            "a diff beyond the cap must carry a typed truncation reason"
        );
        assert!(
            set.changes.len() > SHELL_MANIFEST_MAX_CHANGES,
            "the loop detects the overflow at the first entry past the cap"
        );
        let (rows, durable_truncated) = set.durable_changes();
        assert_eq!(rows.len(), SHELL_CHANGE_DURABLE_MAX);
        assert!(durable_truncated);
    }

    /// A large ignored tree is NEVER entered: the documented caps bound the
    /// visited tree, so build/dependency output cannot blow them.
    #[test]
    fn ignored_trees_do_not_touch_the_manifest_caps() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        write(&root, "keep.txt", b"keep");
        for i in 0..2_000 {
            write(&root, &format!("target/obj{i:05}.o"), b"ignored");
            write(&root, &format!("node_modules/pkg{i}.js"), b"ignored");
        }
        // One visible file plus the two skipped directory entries: a
        // 4-entry budget proves the 4k ignored files were never charged.
        let mut budget = WalkBudget::new(4, 5, MAX_TREE_MANIFEST_DEPTH, u64::MAX, u64::MAX);
        let manifest =
            tree_manifest_with_skip_budgeted(&root, &mut budget, SHELL_MANIFEST_SKIP_DIRS)
                .expect("ignored trees must not exhaust the manifest budget");
        let paths: Vec<&str> = manifest
            .entries()
            .iter()
            .map(|e| e.normalized_path.as_str())
            .collect();
        assert_eq!(paths, vec!["keep.txt"]);
    }

    /// The pre-shell capture stores the pre-image blobs in the CAS so a
    /// later modification/deletion can be checkpointed.
    #[test]
    fn pre_shell_capture_stores_bounded_pre_images() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        write(&root, "a.txt", b"alpha");
        write(
            &root,
            "big.bin",
            &vec![7u8; SHELL_MANIFEST_CAPTURE_FILE_BYTES + 1],
        );
        let cas = Cas::open(dir.path().join("cas")).unwrap();
        let capture = capture_pre_shell(&workspace(&root), Some(&cas)).unwrap();
        assert_eq!(capture.pre_images.len(), 1);
        assert!(capture.pre_images.contains_key("a.txt"));
        assert!(capture.omitted_pre_images >= 1, "oversize file omitted");
        let hash = capture.pre_images.get("a.txt").unwrap();
        let bytes = cas
            .get_bounded(FileHash::from_hex(hash).unwrap(), 64)
            .unwrap()
            .unwrap();
        assert_eq!(bytes, b"alpha");
        // The envelope is a small durable note; the manifest itself is in
        // the CAS and round-trips byte-identically.
        let envelope = capture.envelope(OpId::new(9));
        assert_eq!(envelope["state"], "captured");
        assert_eq!(envelope["turn_op"], 9);
        let loaded = load_pre_manifest(&cas, capture.manifest_cas.as_deref().unwrap()).unwrap();
        assert_eq!(loaded, capture.manifest);
    }
}
