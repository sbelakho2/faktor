//! Independent final-wave probe for the approval authority's cross-process
//! guarantees, exercised with REAL child processes (never an in-process fake):
//!
//! * two concurrent writers append one valid hash chain (OS advisory lock on
//!   an open descriptor serializes them);
//! * a crashed lock holder is released by the kernel immediately (no stale
//!   timeout, no deletable lock file);
//! * a wall-clock observation persisted by one process keeps an approval
//!   expired for a second process whose wall clock moved backwards.

use std::fs::{OpenOptions, TryLockError};
use std::io::{BufRead as _, BufReader, Write as _};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use faktor_approval::{
    save_operator_key, ApprovalAuthority, ApprovalCheck, ApprovalDraft, ApprovalError,
    ApprovalKind, ApprovalLedger, DecisionOutcome, OperatorKey, Principal, PrincipalKind,
    RiskLevel,
};

const CHILD_ENV: &str = "FAKTOR_APPROVAL_WAVE_CHILD";
const CHILD_MODE: &str = "FAKTOR_APPROVAL_WAVE_MODE";
const CHILD_DIR: &str = "FAKTOR_APPROVAL_WAVE_DIR";
const CHILD_TEST: &str = "child_process_entry";
const PROPOSALS: usize = 25;

fn authority(dir: &std::path::Path) -> (ApprovalAuthority, OperatorKey) {
    let key = OperatorKey::generate();
    save_operator_key(dir, "quality-director", &key, false).unwrap();
    faktor_approval::register_operator_key(
        dir,
        "quality-director",
        &key,
        faktor_approval::unix_now_ms().max(1),
    )
    .unwrap();
    (ApprovalAuthority::open(dir).unwrap(), key)
}

fn draft(subject: &str, digest: &str) -> ApprovalDraft {
    ApprovalDraft {
        kind: ApprovalKind::EngineeringConstraintRelaxation,
        subject: subject.to_string(),
        evidence: vec!["wave-probe".to_string()],
        proposed_change: "independent process probe".to_string(),
        risk: RiskLevel::High,
        requester: Principal::new("wave-agent", PrincipalKind::Agent).unwrap(),
        bound_revision: "9".to_string(),
        bound_digest: digest.to_string(),
        expires_at_ms: faktor_approval::unix_now_ms() + 120_000,
    }
}

fn spawn_child(dir: &std::path::Path, mode: &str) -> (Child, BufReader<ChildStdout>) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", CHILD_TEST, "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, "1")
        .env(CHILD_MODE, mode)
        .env(CHILD_DIR, dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn approval child");
    let stdout = child.stdout.take().expect("child stdout");
    (child, BufReader::new(stdout))
}

fn wait_line(reader: &mut BufReader<ChildStdout>, needle: &str) {
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line).expect("read child output");
        assert!(read != 0, "child exited before {needle:?}");
        if line.contains(needle) {
            return;
        }
    }
}

/// Child-process entry point. It is a no-op in a normal test run.
#[test]
fn child_process_entry() {
    let Some(dir) = std::env::var_os(CHILD_DIR) else {
        return;
    };
    assert!(
        std::env::var_os(CHILD_ENV).is_some(),
        "the child entry point is only reachable through its env gate"
    );
    let dir = std::path::PathBuf::from(dir);
    let mode = std::env::var(CHILD_MODE).expect("child mode");
    match mode.as_str() {
        "propose" => {
            let authority = ApprovalAuthority::open(&dir).unwrap();
            let now = faktor_approval::unix_now_ms();
            for index in 0..PROPOSALS {
                let subject = format!("wave-process-{}-{index}", std::process::id());
                let digest = format!(
                    "{:064x}",
                    (std::process::id() as u128) * 1000 + index as u128
                );
                authority
                    .propose(draft(&subject, &digest), now + index as u64)
                    .expect("child proposal must serialize, not fail");
            }
        }
        "hold-lock" => {
            std::fs::create_dir_all(dir.join("approval")).unwrap();
            let file = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(dir.join("approval").join(".ledger.lock"))
                .unwrap();
            file.lock().expect("child acquires the raw ledger lock");
            println!("LOCKED");
            std::io::stdout().flush().unwrap();
            let mut ignored = String::new();
            let _ = std::io::stdin().read_line(&mut ignored);
        }
        "expire" => {
            let authority = ApprovalAuthority::open(&dir).unwrap();
            let check = ApprovalCheck::new(
                ApprovalKind::EngineeringConstraintRelaxation,
                "wave-clock-subject",
                "9",
                "e".repeat(64),
            );
            let now = faktor_approval::unix_now_ms();
            let error = authority
                .authorize(&check, now + 120_000)
                .expect_err("the far-future observation expires the request");
            assert!(matches!(error, ApprovalError::Expired { .. }), "{error:?}");
        }
        other => panic!("unknown child mode {other:?}"),
    }
}

#[test]
fn two_real_processes_append_one_valid_chain() {
    let dir = tempfile::tempdir().unwrap();
    let (_authority, _key) = authority(dir.path());
    let mut first = spawn_child(dir.path(), "propose");
    let mut second = spawn_child(dir.path(), "propose");
    let status_first = first.0.wait().unwrap();
    let status_second = second.0.wait().unwrap();
    assert!(status_first.success(), "first child failed");
    assert!(status_second.success(), "second child failed");

    let ledger = ApprovalLedger::load(dir.path()).expect("the serialized ledger is a valid chain");
    assert_eq!(
        ledger.entries.len(),
        PROPOSALS * 2,
        "no concurrent append may be lost or duplicated"
    );
    assert_eq!(ledger.requests.len(), PROPOSALS * 2);
    for (index, entry) in ledger.entries.iter().enumerate() {
        assert_eq!(entry.seq, index as u64 + 1, "sequence stays contiguous");
    }
}

#[test]
fn a_crashed_raw_lock_holder_is_released_without_a_stale_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let (authority, _key) = authority(dir.path());
    let (mut child, mut reader) = spawn_child(dir.path(), "hold-lock");
    wait_line(&mut reader, "LOCKED");
    let probe = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.path().join("approval").join(".ledger.lock"))
        .unwrap();
    assert!(
        matches!(probe.try_lock(), Err(TryLockError::WouldBlock)),
        "the live child must hold the OS lock"
    );
    drop(probe);
    child.kill().unwrap();
    child.wait().unwrap();

    let started = Instant::now();
    authority
        .propose(
            draft("wave-crash", &"c".repeat(64)),
            faktor_approval::unix_now_ms(),
        )
        .expect("the kernel releases a dead holder's lock immediately");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "crash release must not wait for a stale-lock heuristic"
    );
}

#[test]
fn a_backwards_wall_clock_cannot_revive_an_expiry_observed_by_another_process() {
    let dir = tempfile::tempdir().unwrap();
    let (authority, key) = authority(dir.path());
    let now = faktor_approval::unix_now_ms();
    let digest = "e".repeat(64);
    let request = authority
        .propose(
            ApprovalDraft {
                kind: ApprovalKind::EngineeringConstraintRelaxation,
                subject: "wave-clock-subject".to_string(),
                evidence: vec!["wave-clock".to_string()],
                proposed_change: "clock probe".to_string(),
                risk: RiskLevel::High,
                requester: Principal::new("wave-agent", PrincipalKind::Agent).unwrap(),
                bound_revision: "9".to_string(),
                bound_digest: digest.clone(),
                expires_at_ms: now + 60_000,
            },
            now,
        )
        .unwrap();
    authority
        .decide(
            &request.id,
            Principal::new("quality-director", PrincipalKind::HumanOperator).unwrap(),
            &key,
            DecisionOutcome::Approved,
            "reviewed".to_string(),
            now + 1,
        )
        .unwrap();

    // The child observes a wall clock beyond the expiry and durably persists
    // the high-water mark. The parent then presents a clock one hour earlier.
    let (mut child, _reader) = spawn_child(dir.path(), "expire");
    let status = child.wait().unwrap();
    assert!(status.success(), "clock child failed");

    let check = ApprovalCheck::new(
        ApprovalKind::EngineeringConstraintRelaxation,
        "wave-clock-subject",
        "9",
        digest,
    );
    assert!(
        matches!(
            authority.authorize(&check, now.saturating_sub(3_600_000)),
            Err(ApprovalError::Expired { .. })
        ),
        "a backwards jump must not extend an approval the other process saw expired"
    );
}
