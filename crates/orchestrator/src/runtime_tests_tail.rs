//! `runtime_tests_tail`: the run-mirror poison-recovery test, split out of
//! `runtime_tests` by the source-size ceiling (every fixture arrives via
//! `use super::*`, exactly like the admission/perf children).
// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir

use super::*;

/// The run-mirror lock is a projection of the durable rows: a poisoned guard
/// (a panicking writer) must be recovered by the next read path instead of
/// wedging child execution, and the poison flag must be cleared.
#[test]
fn poisoned_exec_mirror_recovers_and_serves_reads() {
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(dir.path(), vec![], 0);
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = env.orchestrator.exec.lock().unwrap();
        panic!("poison the exec mirror (test seam)");
    }));
    assert!(poisoned.is_err(), "the poisoner must unwind");
    assert!(env.orchestrator.exec.is_poisoned());
    // OWNERSHIP projection => reconciled recovery: the read path serves durable truth.
    let err = env
        .orchestrator
        .locate_child("missing-child")
        .expect_err("an unknown child is a typed NotFound, never a panic");
    assert!(matches!(err, ExecError::NotFound(_)), "{err:?}");
    assert!(!env.orchestrator.exec.is_poisoned());
}
