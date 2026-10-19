#![cfg(feature = "git")]

use std::path::PathBuf;

use faktor_impact::git::GitAdapter;
use faktor_impact::Revision;
use faktor_terminal::ProcessSupervisor;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

#[tokio::test]
async fn git_adapter_binds_revision_and_changed_paths() {
    let root = repo_root();
    let adapter = GitAdapter::new(ProcessSupervisor::try_shared().unwrap());
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
