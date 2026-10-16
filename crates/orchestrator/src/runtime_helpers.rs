//! `runtime::runtime_helpers`: the run-boundary validation helpers split
//! out of `runtime.rs` mechanically (source-size ceiling). Every name here
//! is re-exported by the `runtime` module, so all existing
//! `super::…` / `crate::runtime::…` paths resolve exactly as before.

use super::*;

/// All durable facts of one session (bounded page scan).
pub(crate) fn parent_facts(
    handle: &faktor_session::SessionHandle,
) -> faktor_core::Result<Vec<(String, String, String)>> {
    let mut out = Vec::new();
    let mut after: Option<(i64, String, String)> = None;
    for _ in 0..64 {
        let page = handle.memory_facts_page(after.as_ref(), 200)?;
        out.extend(page.facts);
        match page.cursor {
            Some(c) => after = Some(c),
            None => break,
        }
    }
    Ok(out)
}

/// The child session's durable Task-row token cap — the ONE budget
/// authority the registry projection derives from (None when no task row or
/// an unlimited cap).
pub(crate) fn child_task_budget_cap(manager: &Arc<SessionManager>, session_id: u64) -> Option<u64> {
    // Decoded raw id backstop: zero derives nothing, never a panic.
    let session_id = SessionId::try_from(session_id).ok()?;
    let handle = manager.get_session(session_id).ok().flatten()?;
    let task_id = handle.task_id().ok()?;
    handle
        .get_task(task_id)
        .ok()
        .flatten()
        .and_then(|t| t.budget.max_tokens)
}

/// The child session's durable execution phase (Planning when no drive-state
/// row exists).
pub(crate) fn child_execution_phase(
    manager: &Arc<SessionManager>,
    session_id: u64,
) -> ExecutionPhase {
    // Decoded raw id backstop: zero derives the default, never a panic.
    SessionId::try_from(session_id)
        .ok()
        .and_then(|id| manager.get_session(id).ok().flatten())
        .and_then(|h| h.orchestrator_drive_state_get().ok())
        .map(|ds| ds.execution_phase)
        .unwrap_or_default()
}

pub(crate) fn validate_config(config: &ExecConfig) -> Result<(), ExecError> {
    config.ceilings.validate().map_err(ExecError::InvalidPlan)?;
    if config.run_id.is_empty()
        || config.run_id.len() > MAX_RUN_ID_CHARS
        || !config.run_id.is_ascii()
        || config.run_id.contains('/')
    {
        return Err(ExecError::Oversized(format!(
            "run id must be 1..={MAX_RUN_ID_CHARS} ASCII characters without '/'"
        )));
    }
    if config.default_model.is_empty() {
        return Err(ExecError::InvalidPlan("default_model is empty".into()));
    }
    if config.provider.is_empty() {
        return Err(ExecError::InvalidPlan("provider is empty".into()));
    }
    Ok(())
}

pub(crate) fn validate_specs(
    plan: &crate::TaskPlan,
    specs: &[ChildSpec],
) -> Result<HashMap<String, ChildSpec>, ExecError> {
    let mut map = HashMap::new();
    for s in specs {
        if !plan.work_items.iter().any(|w| w.id == s.item_id) {
            return Err(ExecError::NotFound(format!(
                "spec names unknown work item {:?}",
                s.item_id
            )));
        }
        if map.insert(s.item_id.clone(), s.clone()).is_some() {
            return Err(ExecError::Conflict(format!(
                "duplicate spec for work item {}",
                s.item_id
            )));
        }
        if s.model
            .as_ref()
            .is_some_and(|m| m.is_empty() || m.len() > 128)
        {
            return Err(ExecError::InvalidPlan(
                "child model must be 1..=128 characters".into(),
            ));
        }
        validate_attachment_files(&s.files).map_err(|e| {
            ExecError::InvalidPlan(format!("attached files of work item {}: {e}", s.item_id))
        })?;
        validate_attachment_ids(&s.attachments).map_err(|e| {
            ExecError::InvalidPlan(format!(
                "binary attachments of work item {}: {e}",
                s.item_id
            ))
        })?;
    }
    Ok(map)
}

/// The ONE attachment-file validation rule of a run (shared by the request
/// boundary, the durable spec decode and the server DTO): bounded by the
/// SAME constants the single-session prompt submission enforces
/// ([`faktor_session::MAX_FILES_PER_PROMPT`],
/// [`faktor_session::MAX_FILE_PATH_BYTES`]) plus the typed hostile-path
/// refusal (empty/control-character paths and absolute or `..` traversal
/// components). Returns a typed [`ExecError::Oversized`]/[`ExecError::Malformed`]
/// — never a truncation or a silent drop.
pub fn validate_attachment_files(files: &[String]) -> Result<(), ExecError> {
    if files.len() > faktor_session::MAX_FILES_PER_PROMPT {
        return Err(ExecError::Oversized(format!(
            "{} files exceed MAX_FILES_PER_PROMPT ({})",
            files.len(),
            faktor_session::MAX_FILES_PER_PROMPT
        )));
    }
    for f in files {
        if f.len() > faktor_session::MAX_FILE_PATH_BYTES {
            return Err(ExecError::Oversized(format!(
                "file path of {} bytes exceeds MAX_FILE_PATH_BYTES ({})",
                f.len(),
                faktor_session::MAX_FILE_PATH_BYTES
            )));
        }
        if f.trim().is_empty() {
            return Err(ExecError::Malformed(
                "an attached file path is empty or whitespace-only".into(),
            ));
        }
        if f.chars().any(|c| c.is_control()) {
            return Err(ExecError::Malformed(format!(
                "attached file path {f:?} carries control characters"
            )));
        }
        let path = std::path::Path::new(f);
        let drive_prefixed = f.len() >= 2
            && f.as_bytes()[0].is_ascii_alphabetic()
            && f.as_bytes()[1] == b':'
            && f.as_bytes()
                .get(2)
                .is_some_and(|b| *b == b'/' || *b == b'\\');
        // `Path::is_absolute` is false on Windows for a leading `/` (no
        // prefix), so a Unix-absolute hostile path slipped through there;
        // any leading separator is root-relative and rejected everywhere.
        let root_relative = f.starts_with('/') || f.starts_with('\\');
        if path.is_absolute() || drive_prefixed || root_relative {
            return Err(ExecError::Malformed(format!(
                "attached file path {f:?} is absolute; attached files are workspace-relative"
            )));
        }
        if f.split(['/', '\\']).any(|segment| segment == "..") {
            return Err(ExecError::Malformed(format!(
                "attached file path {f:?} traverses outside the workspace ('..')"
            )));
        }
    }
    Ok(())
}

/// The ONE durable binary-attachment validation rule of a run (shared by
/// the request boundary, the durable spec decode and the server DTO): the
/// set is bounded by [`faktor_session::MAX_ATTACHMENTS_PER_TASK`] and every
/// id is structurally validated (hostile mime/filename/size are typed
/// refusals). Durable-row existence is resolved by the session layer at
/// admission ([`faktor_session::SessionHandle::resolve_attachments`]) and
/// image delivery is validated against the chosen model's capabilities at
/// the server DTO; a child inherits the verified rows at spawn.
pub fn validate_attachment_ids(
    ids: &[faktor_core::attachment::AttachmentId],
) -> Result<(), ExecError> {
    use faktor_core::attachment::MAX_ATTACHMENTS_PER_TASK;
    if ids.len() > MAX_ATTACHMENTS_PER_TASK {
        return Err(ExecError::Oversized(format!(
            "{} binary attachments exceed MAX_ATTACHMENTS_PER_TASK ({MAX_ATTACHMENTS_PER_TASK})",
            ids.len()
        )));
    }
    for id in ids {
        id.validate()
            .map_err(|e| ExecError::Malformed(format!("binary attachment {}: {e}", id.digest)))?;
    }
    Ok(())
}

/// (audits 7/8/21/22) The typed-policy half of the per-item ownership
/// compile: a child whose effective ownership grants NO write authority
/// (every read-only item) must never carry WriteWorkspace in its policy —
/// even when the policy claims it and the parent could grant it. A
/// semantic-entity item's writes are provider-scoped entities: its policy
/// may not claim FILE write capability either (that would grant shared-
/// worktree file writes its ownership never authorized).
pub(crate) fn check_item_policies(
    specs: &HashMap<String, ChildSpec>,
    effective: &HashMap<String, OwnershipSpec>,
) -> Result<(), ExecError> {
    for spec in specs.values() {
        let Some(eff) = effective.get(&spec.item_id) else {
            continue;
        };
        let write_claimed = spec
            .task_caps
            .iter()
            .chain(spec.child_caps.iter())
            .any(|g| g.cap == crate::caps::LatticeCap::WriteWorkspace);
        if !write_claimed {
            continue;
        }
        if !eff.allows_writes() {
            return Err(ExecError::InvalidPlan(format!(
                "read-only work item {:?} requests WriteWorkspace in its policy; read-only \
                 items can never receive write capability",
                spec.item_id
            )));
        }
        if matches!(eff, OwnershipSpec::SemanticEntities { .. }) {
            return Err(ExecError::InvalidPlan(format!(
                "semantic-entity work item {:?} requests file-level WriteWorkspace in its \
                 policy; semantic-entity writes are provider-scoped and never touch the \
                 shared worktree",
                spec.item_id
            )));
        }
    }
    Ok(())
}

/// (audits 7/8/21/22) The canonicalized half of the item disjointness
/// check: the compile already rejected lexically overlapping path sets;
/// here every mutating item's path set is resolved against the REAL owner
/// root so fs-equivalent spellings (`src` vs `src/../src`) collide before
/// any spawn — not at a later live-overlap refusal.
pub(crate) fn check_plan_disjointness_canonical(
    plan: &crate::TaskPlan,
    effective: &HashMap<String, OwnershipSpec>,
    owner: &OwnerContext,
) -> Result<(), ExecError> {
    let mut path_items: Vec<(String, &Vec<String>)> = plan
        .work_items
        .iter()
        .filter(|w| w.kind.is_mutating())
        .filter_map(|w| match effective.get(&w.id) {
            Some(OwnershipSpec::Paths { paths }) => Some((w.id.clone(), paths)),
            _ => None,
        })
        .collect();
    path_items.sort_by(|a, b| a.0.cmp(&b.0));
    for i in 0..path_items.len() {
        for j in (i + 1)..path_items.len() {
            let (a_id, a_paths) = &path_items[i];
            let (b_id, b_paths) = &path_items[j];
            let mine = SchOwnershipSet::new((*a_paths).clone()).canonicalized(&owner.root);
            let theirs = SchOwnershipSet::new((*b_paths).clone()).canonicalized(&owner.root);
            if mine.overlaps(&theirs) {
                return Err(ExecError::OverlappingExclusiveOwnership(format!(
                    "work items {a_id:?} and {b_id:?} have overlapping normalized write \
                     ownership; mutating items of one plan must be disjoint before spawn"
                )));
            }
        }
    }
    Ok(())
}
