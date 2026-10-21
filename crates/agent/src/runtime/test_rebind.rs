//! Durable evidence gathering for deterministic test-selector rebinding.
//!
//! The pure rebinding authority lives in `faktor_verify::rebind`; this slice
//! only gathers its REAL inputs from the candidate workspace:
//! * candidate/base file contents through the checkpoint CAS (the same
//!   durable rows a crash-resumed settlement reads), with base `git show`
//!   fallback for a rename source that has no checkpoint row;
//! * git rename detection over the candidate worktree (`git diff
//!   --find-renames=50% --name-status HEAD`), the rename EDGE authority.
//!
//! A rename edge proves only the path relation. Semantic preservation is
//! proven separately by the declaration adapters; a weak (1%) similarity
//! edge is never authority for a proof migration. Neither input is
//! fabricated: a missing side is absent and the mapping stays unresolved
//! rather than guessing.

use std::path::Path;
use std::sync::Arc;

use faktor_git::WorktreeManager;
use faktor_terminal::{ProcessOwner, ProcessSupervisor};
use faktor_verify::rebind::{TestFileDelta, TestRebindEvidence, TestRenameEdge};

use super::*;

impl AgentRuntime {
    /// Gather the rebinding evidence for one candidate snapshot. Bounded:
    /// only changed test-bearing files (plus rename-edge endpoints), at most
    /// the review fetch bound, each side bounded by the review side bound.
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
                    if faktor_verify::rebind::supports_test_path(&from)
                        || faktor_verify::rebind::supports_test_path(&to)
                    {
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
            .filter(|path| faktor_verify::rebind::supports_test_path(path))
            .cloned()
            .collect();
        for edge in &renames {
            if faktor_verify::rebind::supports_test_path(&edge.from) {
                paths.push(edge.from.clone());
            }
            if faktor_verify::rebind::supports_test_path(&edge.to) {
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
                "--find-renames=50%",
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
                // Defense in depth: the command already asks for >=50%
                // detection, but an unexpected low-similarity score is never
                // proof-authoritative even if git emits one.
                let score = status
                    .get(1..)
                    .and_then(|score| score.parse::<u32>().ok())
                    .unwrap_or(100);
                if score >= 50 && !from.is_empty() && !to.is_empty() {
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

#[cfg(test)]
mod parser_tests {
    use super::parse_rename_status;

    #[test]
    fn rename_status_requires_a_strong_similarity_score() {
        let output = "R100\0tests/old.rs\0tests/new.rs\0R001\0tests/weak.rs\0tests/weak_new.rs\0";
        assert_eq!(
            parse_rename_status(output),
            vec![("tests/old.rs".to_string(), "tests/new.rs".to_string())],
            "a 1% pairing must never become proof-authoritative rename evidence"
        );
    }

    #[test]
    fn rename_status_skips_copies_adds_deletes_and_malformed_tails() {
        let output =
            "C100\0tests/a.rs\0tests/b.rs\0A\0tests/new.rs\0D\0tests/gone.rs\0R100\0tests/from.rs\0";
        assert!(parse_rename_status(output).is_empty());
    }
}
