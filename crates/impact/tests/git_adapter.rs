// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
#![cfg(feature = "git")]

use std::path::PathBuf;
use std::sync::Arc;

use faktor_core::id::{SessionId, WorkspaceId};
use faktor_impact::git::GitAdapter;
use faktor_impact::{ImpactProcessOwner, Revision};
use faktor_terminal::{ProcessOwner, ProcessSupervisor};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn private_supervisor() -> (tempfile::TempDir, Arc<ProcessSupervisor>) {
    let dir = tempfile::tempdir().unwrap();
    let supervisor = ProcessSupervisor::new(Arc::new(faktor_cas::Cas::new(dir.path().join("cas"))));
    (dir, supervisor)
}

#[tokio::test]
async fn git_adapter_binds_revision_and_changed_paths() {
    let root = repo_root();
    let (_cas, supervisor) = private_supervisor();
    let adapter = GitAdapter::new(supervisor, ImpactProcessOwner::Daemon);
    let revision = adapter.revision(&root).await.unwrap();
    let head = Revision::read_head(&root);
    assert_eq!(revision.commit, head.commit);
    assert!(revision
        .commit
        .as_deref()
        .is_some_and(|commit| commit.len() == 40 && commit.chars().all(|c| c.is_ascii_hexdigit())));
    assert!(revision.tree.is_some());
    let changed = adapter
        .changed_files(&root, Some("HEAD~1"), false)
        .await
        .unwrap();
    assert!(changed.windows(2).all(|pair| pair[0] < pair[1]));
    for path in &changed {
        assert!(!path.starts_with('/'));
        assert!(!path.contains(".."));
    }
}

/// Adversarial ownership probe: every git child spawned by the adapter is
/// owned by the REAL workspace, never by a fabricated user session. Ending
/// user session 1 must therefore not reap them, while workspace teardown is
/// the only workspace-scoped kill. Completed git children are reaped, so the
/// probe also proves the adapter leaves no live orphan.
#[tokio::test]
async fn git_children_are_workspace_owned_and_immune_to_session_one() {
    let root = repo_root();
    let (_cas, supervisor) = private_supervisor();
    let workspace = WorkspaceId::new(4242);
    let adapter = GitAdapter::new(supervisor.clone(), ImpactProcessOwner::Workspace(workspace));
    adapter.revision(&root).await.unwrap();

    let git_spawns: Vec<_> = supervisor
        .recent_spawns()
        .into_iter()
        .filter(|spawn| spawn.argv.starts_with("git "))
        .collect();
    assert!(
        !git_spawns.is_empty(),
        "the adapter must run real supervised git children"
    );
    let workspace_owner = format!("{:?}", ProcessOwner::Workspace(workspace));
    for spawn in &git_spawns {
        assert_eq!(
            spawn.owner, workspace_owner,
            "git child must belong to the real workspace, never a session: {spawn:?}"
        );
        assert!(
            !spawn.owner.contains("Session"),
            "no git child may be session-owned: {spawn:?}"
        );
    }
    assert!(
        supervisor
            .kill_all_for(ProcessOwner::Session(SessionId::new(1)))
            .is_empty(),
        "ending user session 1 must never kill workspace-owned git children"
    );
    assert!(
        supervisor.alive().is_empty(),
        "the adapter must not leave a live git orphan"
    );
}
