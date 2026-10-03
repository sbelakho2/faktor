//! Adversarial corpus for the git mutation guard and ref/name validation.
//!
//! Pure lease-liveness matrix (pid reuse, unknown identity, malformed pids),
//! real process probes, the branch/worktree/remote validators against hostile
//! ref spellings, and the durable disk lease: contention, stale reconciliation,
//! stolen-lease release safety and a racing acquisition storm. Every row
//! asserts one outcome with a message naming the row.

use std::path::Path;
use std::time::Duration;

use faktor_core::error::ErrorKind;

use crate::guard::{
    lease_verdict, process_alive, ObserveResult, ReclaimReason, Verdict, DEFAULT_LEASE_BUDGET,
};
use crate::guard::{DiskLease, LeaseRecord, LEASE_FILE};

fn tmpdir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn expected_verdict(pid: u32, recorded_ms: i64, observed: ObserveResult) -> Verdict {
    if pid == 0 {
        return Verdict::Dead;
    }
    match observed {
        ObserveResult::InvalidPid => Verdict::Dead,
        ObserveResult::AccessDenied => Verdict::Alive,
        ObserveResult::Alive { created_ms } => {
            if recorded_ms != 0 && created_ms != 0 && created_ms != recorded_ms {
                Verdict::Reclaim(ReclaimReason::PidReused)
            } else {
                Verdict::Alive
            }
        }
    }
}

/// The full cross product of owner pid, recorded creation time and observed
/// process state: `InvalidPid` is the only proof of death, unknown identity
/// is always alive, and a differing creation time is a reclaimed generation.
#[test]
fn lease_verdict_cross_product_is_exact() {
    let pids = [0u32, 1, 4_242, u32::MAX];
    let recorded = [0i64, 1_000, i64::MAX];
    let observed = [
        ("invalid-pid", ObserveResult::InvalidPid),
        ("access-denied", ObserveResult::AccessDenied),
        (
            "alive-unknown-creation",
            ObserveResult::Alive { created_ms: 0 },
        ),
        (
            "alive-same-creation",
            ObserveResult::Alive { created_ms: 1_000 },
        ),
        (
            "alive-other-creation",
            ObserveResult::Alive { created_ms: 999 },
        ),
        (
            "alive-late-creation",
            ObserveResult::Alive {
                created_ms: i64::MAX,
            },
        ),
    ];
    let mut cases = 0usize;
    for pid in pids {
        for recorded_ms in recorded {
            for (label, observation) in observed {
                cases += 1;
                let got = lease_verdict(pid, recorded_ms, observation);
                let want = expected_verdict(pid, recorded_ms, observation);
                assert_eq!(
                    got, want,
                    "case pid={pid} recorded={recorded_ms} observed={label}: \
                     InvalidPid alone proves death; unknown is alive; a different \
                     creation time reclaims"
                );
            }
        }
    }
    assert!(
        cases >= 60,
        "the lease-verdict matrix must keep at least 60 rows, found {cases}"
    );
}

/// Unknown identity is never turned into a reclaim: the safety argument is
/// asserted directly for the shapes a hostile observer could produce.
#[test]
fn unknown_identity_never_reclaims_a_live_lease() {
    let rows: [(&str, u32, i64, ObserveResult); 8] = [
        ("access-denied", 123, 500, ObserveResult::AccessDenied),
        (
            "access-denied-no-record",
            123,
            0,
            ObserveResult::AccessDenied,
        ),
        (
            "alive-unobservable-creation",
            123,
            500,
            ObserveResult::Alive { created_ms: 0 },
        ),
        (
            "alive-own-creation-unobservable",
            123,
            0,
            ObserveResult::Alive { created_ms: 900 },
        ),
        ("pid-1-access-denied", 1, 5, ObserveResult::AccessDenied),
        (
            "pid-max-access-denied",
            u32::MAX,
            5,
            ObserveResult::AccessDenied,
        ),
        (
            "alive-negative-creation",
            123,
            -1,
            ObserveResult::Alive { created_ms: -1 },
        ),
        (
            "alive-equal-negative-generation",
            123,
            -5,
            ObserveResult::Alive { created_ms: -5 },
        ),
    ];
    for (label, pid, recorded_ms, observation) in rows {
        assert_eq!(
            lease_verdict(pid, recorded_ms, observation),
            Verdict::Alive,
            "case {label}: an unprovable identity must never reclaim the lease"
        );
    }
}

/// Real process probes: the current process is alive with a stable marker, a
/// pid that was never assigned is dead, and pid 0 is never an owner.
#[test]
fn process_probe_matrix() {
    let me = std::process::id();
    assert!(process_alive(me), "case self: the test process is alive");
    let marker_a = crate::guard::pid_start_marker(me);
    let marker_b = crate::guard::pid_start_marker(me);
    assert_eq!(
        marker_a, marker_b,
        "case marker-stable: the start marker must not change while alive"
    );
    let dead = {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    };
    assert!(
        !process_alive(dead),
        "case reaped-child: a reaped pid must be observably gone"
    );
    let pid_max = std::fs::read_to_string("/proc/sys/kernel/pid_max")
        .ok()
        .and_then(|t| t.trim().parse::<u32>().ok())
        .unwrap_or(1 << 22);
    assert!(
        !process_alive(pid_max),
        "case pid-max: the kernel never assigns pid_max itself"
    );
    assert!(
        process_alive(me) && !process_alive(dead),
        "case mixed: liveness must distinguish self from a reaped child"
    );
}

/// Hostile branch spellings through the production validator: option-looking
/// names, revision syntax, ref separators, control characters and the length
/// bound; normal branch names still pass.
#[test]
fn branch_name_validation_corpus() {
    let invalid = [
        "",
        "-",
        "--force",
        "-D",
        "..",
        "a..b",
        "a b",
        "a\tb",
        "a\nb",
        "a\rb",
        "a\x01b",
        "a~b",
        "a^b",
        "a:b",
        "a?b",
        "a*b",
        "a[b",
        "a\\b",
        "x/",
        "refs/heads/",
    ];
    for branch in invalid {
        assert!(
            crate::validate_branch(branch).is_err(),
            "case invalid {branch:?}: must be refused typed"
        );
    }
    let long = "a".repeat(129);
    assert!(
        crate::validate_branch(&long).is_err(),
        "case 129-bytes: over-long branch names must be refused"
    );
    let valid = [
        "main",
        "master",
        "feat/x-1",
        "release/v1.2.3",
        "a_b.c",
        "user@host",
        "x+y",
        "UPPER",
        "@",
        "@{",
        "a//b",
        "a.lock",
        "a",
    ];
    for branch in valid {
        assert!(
            crate::validate_branch(branch).is_ok(),
            "case valid {branch:?}: must be accepted"
        );
    }
    assert!(
        crate::validate_branch(&"a".repeat(128)).is_ok(),
        "case 128-bytes: the boundary length is accepted"
    );
    assert!(
        crate::validate_branch("feature/-x").is_ok(),
        "case inner-dash: only a LEADING dash is option-shaped"
    );
}

/// Worktree names and remote names: traversal, separators and whitespace are
/// refused; ordinary tokens pass.
#[test]
fn worktree_and_remote_name_corpora() {
    let bad_names = ["", "..", "a..b", "a/b", "a b", "a\tb", "name/../x"];
    for name in bad_names {
        assert!(
            crate::validate_name(name).is_err(),
            "case worktree-invalid {name:?}: must be refused"
        );
    }
    let long = "n".repeat(65);
    assert!(
        crate::validate_name(&long).is_err(),
        "case worktree-65: over-long names must be refused"
    );
    for name in ["wt-1", "a_b.c", "UPPER", "caf\u{e9}", "a\\b"] {
        assert!(
            crate::validate_name(name).is_ok(),
            "case worktree-valid {name:?}: must be accepted"
        );
    }
    assert!(
        crate::validate_name(&"n".repeat(64)).is_ok(),
        "case worktree-64: the boundary length is accepted"
    );

    let bad_remotes = [
        "",
        "-",
        "--upload-pack=x",
        "a..b",
        "a/b",
        "a\\b",
        "a b",
        "a\tb",
        "a~b",
        "a^b",
        "a:b",
        "a?b",
        "a*b",
        "a[b",
    ];
    for remote in bad_remotes {
        assert!(
            crate::validate_remote(remote).is_err(),
            "case remote-invalid {remote:?}: must be refused"
        );
    }
    for remote in ["origin", "upstream", "my-remote_1", "UPPER"] {
        assert!(
            crate::validate_remote(remote).is_ok(),
            "case remote-valid {remote:?}: must be accepted"
        );
    }
    assert!(
        crate::validate_remote(&"r".repeat(129)).is_err(),
        "case remote-129: over-long remote names must be refused"
    );
}

fn lease_at(
    dir: &Path,
    purpose: &str,
    budget: Duration,
) -> Result<DiskLease, faktor_core::error::Error> {
    DiskLease::acquire(dir, purpose, budget)
}

/// Durable disk lease: contention is a typed conflict naming the holder,
/// release is token-scoped, and a dead owner is reconciled immediately.
#[test]
fn disk_lease_contention_and_release_matrix() {
    // Missing common dir is refused typed.
    let dir = tmpdir();
    let missing = dir.path().join("no-git-dir");
    assert!(
        lease_at(&missing, "x", Duration::from_millis(10)).is_err(),
        "case missing-dir: a missing common dir must be refused"
    );

    // Acquire, observe our identity, and prove contention is typed.
    let dir = tmpdir();
    let lease = lease_at(dir.path(), "primary", DEFAULT_LEASE_BUDGET).expect("first acquire");
    assert_eq!(
        lease.record().pid,
        std::process::id(),
        "case identity: the lease records our pid"
    );
    assert!(
        lease.path().ends_with(LEASE_FILE),
        "case path: the lease lives at the documented file name"
    );
    let blocked = lease_at(dir.path(), "contender", Duration::from_millis(80))
        .expect_err("a live holder must block a contender");
    assert_eq!(
        blocked.kind,
        ErrorKind::Conflict,
        "case contention: a held lease is a typed conflict: {blocked}"
    );
    assert!(
        blocked.message.contains("held by pid"),
        "case contention-names-holder: {blocked}"
    );
    drop(lease);
    assert!(
        !dir.path().join(LEASE_FILE).exists(),
        "case release: dropping the holder removes the lease file"
    );

    // A stale record with pid 0 (never an owner) is reconciled immediately.
    let dir = tmpdir();
    let stale = LeaseRecord {
        pid: 0,
        pid_start_marker: String::new(),
        pid_created_ms: 0,
        owner: "dead-owner-token".into(),
        started_ms: 1,
        purpose: "crashed".into(),
    };
    std::fs::write(
        dir.path().join(LEASE_FILE),
        serde_json::to_vec(&stale).unwrap(),
    )
    .unwrap();
    let acquired = lease_at(dir.path(), "steal", Duration::from_millis(200))
        .expect("a pid-0 record must be reconcilable");
    assert_eq!(
        acquired.record().purpose,
        "steal",
        "case stale-steal: the new owner wins the lease"
    );

    // A record naming a reaped child is stale and reconcilable.
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead_pid = child.id();
    child.wait().unwrap();
    let stale = LeaseRecord {
        pid: dead_pid,
        pid_start_marker: String::new(),
        pid_created_ms: 0,
        owner: "reaped-owner".into(),
        started_ms: 1,
        purpose: "crashed".into(),
    };
    std::fs::write(
        dir.path().join(LEASE_FILE),
        serde_json::to_vec(&stale).unwrap(),
    )
    .unwrap();
    let acquired = lease_at(dir.path(), "steal-reaped", Duration::from_millis(200))
        .expect("a reaped owner must be reconcilable");
    assert_eq!(acquired.record().purpose, "steal-reaped");

    // Purpose text is bounded to 200 characters.
    let dir = tmpdir();
    let long_purpose = "p".repeat(500);
    let lease = lease_at(dir.path(), &long_purpose, Duration::from_millis(50)).unwrap();
    assert_eq!(
        lease.record().purpose.chars().count(),
        200,
        "case purpose-bound: diagnostic text must be truncated to 200 chars"
    );
}

/// Release must never delete a lease that was stolen and re-acquired: the
/// owner token check is what makes release safe after a stale reconcile.
#[test]
fn release_never_deletes_a_stolen_lease() {
    let dir = tmpdir();
    let lease = lease_at(dir.path(), "original", Duration::from_millis(50)).unwrap();
    // Simulate another process reconciling and re-acquiring the same path.
    let thief = LeaseRecord {
        pid: std::process::id(),
        pid_start_marker: String::new(),
        pid_created_ms: 0,
        owner: "thief-token".into(),
        started_ms: 2,
        purpose: "thief".into(),
    };
    std::fs::write(
        dir.path().join(LEASE_FILE),
        serde_json::to_vec(&thief).unwrap(),
    )
    .unwrap();
    drop(lease);
    let raw = std::fs::read(dir.path().join(LEASE_FILE))
        .expect("case stolen: the thief's lease must survive the old owner's release");
    let surviving: LeaseRecord = serde_json::from_slice(&raw).unwrap();
    assert_eq!(
        surviving.owner, "thief-token",
        "case stolen-owner: the file must still carry the thief's token"
    );
    assert_eq!(surviving.purpose, "thief");
}

/// A racing acquisition storm: every successful owner observes the lease file
/// carrying its OWN token while held (mutual exclusion), all contenders
/// eventually acquire, and the file is gone afterwards.
#[test]
fn racing_lease_acquisitions_are_mutually_exclusive() {
    let dir = tmpdir();
    let root = dir.path().to_path_buf();
    let contenders = 6usize;
    let mut handles = Vec::new();
    for i in 0..contenders {
        let root = root.clone();
        handles.push(std::thread::spawn(move || {
            let purpose = format!("contender-{i}");
            let lease = DiskLease::acquire(&root, &purpose, Duration::from_secs(5))
                .unwrap_or_else(|e| panic!("contender {i}: acquire failed: {e}"));
            // While we hold it, the file MUST carry our own token.
            let raw = std::fs::read(root.join(LEASE_FILE)).unwrap();
            let on_disk: LeaseRecord = serde_json::from_slice(&raw).unwrap();
            assert_eq!(
                on_disk.owner,
                lease.record().owner,
                "contender {i}: another owner's token must never be observable while held"
            );
            assert_eq!(on_disk.pid, std::process::id());
            std::thread::sleep(Duration::from_millis(30));
            drop(lease);
        }));
    }
    for (i, handle) in handles.into_iter().enumerate() {
        handle
            .join()
            .unwrap_or_else(|_| panic!("contender {i} panicked"));
    }
    assert!(
        !root.join(LEASE_FILE).exists(),
        "the storm must leave no lease residue"
    );
}

/// A fresh malformed lease (crashed writer residue) is NOT reconcilable
/// before its TTL: a contender times out with a typed conflict and the
/// residue is left byte-identical for forensic recovery.
#[test]
fn fresh_malformed_lease_is_not_reconciled() {
    let dir = tmpdir();
    let path = dir.path().join(LEASE_FILE);
    let garbage = b"{ this is not a lease record".to_vec();
    std::fs::write(&path, &garbage).unwrap();
    let err = lease_at(dir.path(), "contender", Duration::from_millis(120))
        .expect_err("a fresh malformed lease must block a contender");
    assert_eq!(
        err.kind,
        ErrorKind::Conflict,
        "case malformed: the refusal is a typed conflict: {err}"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        garbage,
        "case malformed-residue: the residue must be left intact"
    );
}
