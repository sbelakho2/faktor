//! Git adapter (feature `git`): revision identity and changed-file sets.
//!
//! All commands run through `faktor-git`'s read path, which owns the child
//! process through the daemon's ONE [`ProcessSupervisor`](faktor_terminal::ProcessSupervisor).
//! No production code in this crate constructs `std::process::Command`.

use std::path::Path;
use std::sync::Arc;

use faktor_git::WorktreeManager;
use faktor_terminal::{ProcessOwner, ProcessSupervisor};

use crate::model::{validate_rel_path, GraphError, Revision};
use crate::ImpactProcessOwner;

pub struct GitAdapter {
    manager: Arc<WorktreeManager>,
    owner: ProcessOwner,
}

impl GitAdapter {
    /// The caller supplies the daemon/CLI supervisor; this crate never
    /// constructs one (the static-authority scan enforces the one owner).
    /// The owner is a REAL workspace or the standalone daemon/system host:
    /// git children are semantic shared resources, so a user session must
    /// never own (and be able to kill) them.
    pub fn new(supervisor: Arc<ProcessSupervisor>, owner: ImpactProcessOwner) -> Self {
        Self {
            manager: Arc::new(WorktreeManager::new(supervisor)),
            owner: owner.process_owner(),
        }
    }

    pub async fn revision(&self, root: &Path) -> Result<Revision, GraphError> {
        let commit = self
            .read(root, &["rev-parse", "--verify", "--quiet", "HEAD"])
            .await?;
        let commit = commit.trim().to_string();
        if commit.is_empty() {
            return Ok(Revision::default());
        }
        let tree = self
            .read(root, &["rev-parse", "HEAD^{tree}"])
            .await
            .map(|value| value.trim().to_string())
            .ok();
        let dirty = self
            .read(root, &["status", "--porcelain"])
            .await
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);
        Ok(Revision {
            commit: Some(commit),
            tree,
            dirty,
        })
    }

    /// Changed paths between `base...HEAD` (or HEAD~1..HEAD), optionally
    /// including the unstaged/staged/untracked working tree. Rename
    /// detection is disabled: a rename selects both paths.
    pub async fn changed_files(
        &self,
        root: &Path,
        base: Option<&str>,
        worktree: bool,
    ) -> Result<Vec<String>, GraphError> {
        let range = match base {
            Some(base) => format!("{base}...HEAD"),
            None => "HEAD~1..HEAD".to_string(),
        };
        let mut paths = Vec::new();
        if let Ok(output) = self
            .read(
                root,
                &[
                    "diff",
                    "--name-only",
                    "--no-renames",
                    "--diff-filter=ACMRD",
                    "-z",
                    &range,
                ],
            )
            .await
        {
            paths.extend(parse_nul(&output));
        }
        if worktree {
            for args in [
                vec![
                    "diff",
                    "--name-only",
                    "--no-renames",
                    "--diff-filter=ACMRD",
                    "-z",
                ],
                vec![
                    "diff",
                    "--cached",
                    "--name-only",
                    "--no-renames",
                    "--diff-filter=ACMRD",
                    "-z",
                ],
                vec!["ls-files", "--others", "--exclude-standard", "-z"],
            ] {
                if let Ok(output) = self.read(root, &args).await {
                    paths.extend(parse_nul(&output));
                }
            }
        }
        let mut normalized = Vec::new();
        for raw in paths {
            let raw = raw.replace('\\', "/");
            if let Ok(path) = validate_rel_path(&raw) {
                normalized.push(path);
            }
        }
        normalized.sort();
        normalized.dedup();
        Ok(normalized)
    }

    async fn read(&self, root: &Path, args: &[&str]) -> Result<String, GraphError> {
        self.manager
            .git_read(root, args, self.owner.clone())
            .await
            .map_err(|e| GraphError::Malformed(format!("git {}: {e}", args.join(" "))))
    }
}

fn parse_nul(output: &str) -> Vec<String> {
    output
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect()
}
