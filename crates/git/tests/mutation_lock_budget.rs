//! Regression: a second in-process mutation guard must be a BOUNDED typed
//! conflict, never an unbounded wait on the process-local lock.

use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use faktor_cas::Cas;
use faktor_core::error::ErrorKind;
use faktor_git::WorktreeManager;
use faktor_terminal::{ProcessOwner, ProcessSupervisor};

fn git(repo: &std::path::Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_same_manager_guard_is_bounded_conflict_not_a_hang() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "# repo\n").unwrap();
    git(&repo, &["add", "."]);
    git(
        &repo,
        &[
            "-c",
            "user.email=test@faktor.local",
            "-c",
            "user.name=Faktor Test",
            "commit",
            "-m",
            "init",
        ],
    );
    let cas = Arc::new(Cas::open(dir.path().join("cas")).unwrap());
    let manager = WorktreeManager::new(ProcessSupervisor::new(cas));
    let guard = manager
        .acquire_mutation_guard(&repo, ProcessOwner::Daemon)
        .await
        .unwrap();
    let started = Instant::now();
    let error = manager
        .acquire_mutation_guard_with_budget(&repo, ProcessOwner::Daemon, Duration::from_millis(100))
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Conflict, "{error:?}");
    assert!(error.message.contains("wait budget elapsed"), "{error:?}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the second acquisition must be bounded, took {:?}",
        started.elapsed()
    );
    drop(guard);
    manager
        .acquire_mutation_guard(&repo, ProcessOwner::Daemon)
        .await
        .expect("the lock is available after the holder drops it");
}
