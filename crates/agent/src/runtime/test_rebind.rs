//! Durable evidence gathering for deterministic test-selector rebinding.
//!
//! The pure rebinding authority lives in `faktor_verify::rebind`; this slice
//! only gathers its REAL inputs from the candidate workspace:
//! * candidate/base file contents through the checkpoint CAS (the same
//!   durable rows a crash-resumed settlement reads), with base `git show`
//!   fallback for a rename source that has no checkpoint row;
//! * git rename detection over the candidate worktree (`git diff
//!   --find-renames --name-status HEAD`), the rename EDGE authority.
//!
//! Neither input is fabricated: a missing side is absent and the mapping
//! stays unresolved rather than guessing.

use std::path::Path;
use std::sync::Arc;

use faktor_git::WorktreeManager;
use faktor_terminal::{ProcessOwner, ProcessSupervisor};
use faktor_verify::rebind::{TestFileDelta, TestRebindEvidence, TestRenameEdge};

use super::*;

impl AgentRuntime {
    /// Gather the rebinding evidence for one candidate snapshot. Bounded:
    /// only changed Rust files (plus rename-edge endpoints), at most the
    /// review fetch bound, each side bounded by the review side bound.
    pub(crate) async fn test_rebind_evidence(
        &self,
        handle: &faktor_session::SessionHandle,
        ws: &faktor_fs::WorkspaceHandle,
        changed: &[String],
        root: &Path,
    ) -> TestRebindEvidence {
        let mut renames: Vec<TestRenameEdge> = Vec::new();
        if let Some(supervisor) = &self.deps.supervisor {
            if let Ok(edges) = git_rename_edges(supervisor.clone(), root, handle.id()).await {
                for (from, to) in edges {
                    if from.ends_with(".rs") || to.ends_with(".rs") {
                        renames.push(TestRenameEdge {
                            from: from.clone(),
                            to: to.clone(),
                            basis: "git_rename".to_string(),
                        });
                    }
                }
            }
        }
        let mut paths: Vec<String> = changed
            .iter()
            .filter(|path| path.ends_with(".rs"))
            .cloned()
            .collect();
        for edge in &renames {
            if edge.from.ends_with(".rs") {
                paths.push(edge.from.clone());
            }
            if edge.to.ends_with(".rs") {
                paths.push(edge.to.clone());
            }
        }
        paths.sort();
        paths.dedup();
        paths.truncate(faktor_verify::review::REVIEW_MAX_CHANGED_FILES);
        if paths.is_empty() {
            return TestRebindEvidence {
                files: Vec::new(),
                renames,
            };
        }
        let (fetched, _) = review_fetch_changed_files(&self.deps, handle, ws, &paths);
        let mut files = Vec::new();
        for file in fetched {
            let before = file
                .before_bytes
                .as_deref()
                .and_then(|bytes| std::str::from_utf8(bytes).ok())
                .map(str::to_string);
            let mut before = before;
            let mut after = file
                .after_bytes
                .as_deref()
                .and_then(|bytes| std::str::from_utf8(bytes).ok())
                .map(str::to_string);
            if after.is_none() {
                after = ws_read_text(ws, &file.path);
            }
            if before.is_none() && file.change.before_exists {
                if let Some(supervisor) = &self.deps.supervisor {
                    before = git_show_text(supervisor.clone(), root, handle.id(), &file.path).await;
                }
            }
            if before.is_some() || after.is_some() {
                files.push(TestFileDelta {
                    path: file.path,
                    before,
                    after,
                });
            }
        }
        TestRebindEvidence { files, renames }
    }
}

fn ws_read_text(ws: &faktor_fs::WorkspaceHandle, path: &str) -> Option<String> {
    let data = ws.read(Path::new(path), REVIEW_SIDE_BOUND).ok()?;
    if data.bytes.len() > REVIEW_SIDE_BOUND {
        return None;
    }
    std::str::from_utf8(&data.bytes).ok().map(str::to_string)
}

async fn git_show_text(
    supervisor: Arc<ProcessSupervisor>,
    root: &Path,
    session: SessionId,
    path: &str,
) -> Option<String> {
    let manager = WorktreeManager::new(supervisor);
    let spec = format!("HEAD:{path}");
    manager
        .git_read(
            root,
            &["show", spec.as_str()],
            ProcessOwner::Session(session),
        )
        .await
        .ok()
}

async fn git_rename_edges(
    supervisor: Arc<ProcessSupervisor>,
    root: &Path,
    session: SessionId,
) -> Result<Vec<(String, String)>, ()> {
    let manager = WorktreeManager::new(supervisor);
    let output = manager
        .git_read(
            root,
            &[
                "diff",
                "--find-renames=1%",
                "--name-status",
                "-z",
                "HEAD",
                "--",
            ],
            ProcessOwner::Session(session),
        )
        .await
        .map_err(|_| ())?;
    Ok(parse_rename_status(&output))
}

fn parse_rename_status(output: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut tokens = output.split('\0');
    while let Some(status) = tokens.next() {
        let status = status.trim_matches('\n');
        if status.is_empty() {
            continue;
        }
        let kind = status.as_bytes().first().copied().unwrap_or(b' ');
        match kind {
            b'R' => {
                let Some(from) = tokens.next() else { break };
                let Some(to) = tokens.next() else { break };
                if !from.is_empty() && !to.is_empty() {
                    out.push((from.to_string(), to.to_string()));
                }
            }
            b'C' => {
                let _ = tokens.next();
                let _ = tokens.next();
            }
            b'A' | b'D' | b'M' | b'T' | b'U' | b'X' | b'B' => {
                let _ = tokens.next();
            }
            _ => {}
        }
    }
    out
}
