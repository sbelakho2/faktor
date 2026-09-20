//! End-to-end covers of the daemon-side release digest attestation.
//!
//! These spawn the REAL `faktor-cli` binary (the frozen startup-line writer)
//! and read its stdout, so they prove what the unit covers cannot: the env
//! gate is actually read from `FAKTOR_RELEASE_DIGEST`, exactly ONE attestation
//! line reaches stdout (64 lowercase hex, the running binary's own digest),
//! and it precedes the frozen `faktor server listening on ...` startup line
//! that advertises readiness. The mismatch case proves the launcher-refusal
//! path: the ACTUAL digest is still printed (never the expected one) while the
//! loud typed mismatch error goes to stderr (stdout stays a pure handshake).
//!
//! The digest is always computed on a private COPY of the binary that is then
//! spawned: cargo may relink `target/debug/faktor-cli` while this test target
//! runs, so hashing the live path could attest bytes the child never executes.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Mirror of the frozen attestation prefix the daemon writes and the
/// updater's launcher parses (byte-identical by contract).
const DIGEST_PREFIX: &str = "faktor release digest=";
/// Mirror of the frozen startup-line prefix.
const STARTUP_PREFIX: &str = "faktor server listening on http://127.0.0.1:";
/// The launcher's readiness window default is seconds; these tests boot a
/// real debug daemon on loaded CI, so allow a generous bound.
const FIRST_LINE_TIMEOUT: Duration = Duration::from_secs(180);
const NEXT_LINE_TIMEOUT: Duration = Duration::from_secs(60);

/// Copy the built CLI to `dir` and return (copy, digest of the copy). Hashing
/// the copy — the exact bytes the child executes — keeps the assertion immune
/// to cargo relinking the target path concurrently.
fn stable_cli_copy(dir: &Path) -> (PathBuf, String) {
    let source = Path::new(env!("CARGO_BIN_EXE_faktor-cli"));
    let copy = dir.join("faktor-cli-attested");
    std::fs::copy(source, &copy).expect("copy the built CLI binary");
    let digest = faktor_updater::install::file_digest(&copy).expect("the copied CLI hashes");
    (copy, digest)
}

/// A spawned daemon plus its stdout line stream; dropping it terminates the
/// child (zero orphans) and removes the data dir.
struct Daemon {
    child: Child,
    lines: mpsc::Receiver<String>,
    _data_dir: tempfile::TempDir,
}

impl Daemon {
    fn spawn(binary: &Path, exported_digest: Option<&str>, stderr: Stdio) -> Self {
        let data_dir = tempfile::tempdir().expect("a temp data dir");
        let mut command = Command::new(binary);
        command
            .arg("serve")
            .arg("--port")
            .arg("0")
            .arg("--data-dir")
            .arg(data_dir.path())
            .stdout(Stdio::piped())
            .stderr(stderr)
            // The ambient environment must not decide the handshake: the
            // test either exports a digest explicitly or the bootstrap vars
            // are absent (the direct/unix start).
            .env_remove(faktor_updater::RELEASE_DIGEST_ENV)
            .env_remove(faktor_updater::RELEASE_ID_ENV)
            .env_remove(faktor_updater::LAUNCHER_ROOT_ENV);
        if let Some(digest) = exported_digest {
            command.env(faktor_updater::RELEASE_DIGEST_ENV, digest);
        }
        let mut child = command.spawn().expect("spawn the real faktor daemon");
        let stdout = child.stdout.take().expect("piped stdout");
        let (sender, lines) = mpsc::channel();
        // One bounded reader: complete lines only, and a killed child ends
        // the stream with EOF so no reader thread outlives its process.
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if sender.send(line).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });
        Daemon {
            child,
            lines,
            _data_dir: data_dir,
        }
    }

    fn next_line(&self, timeout: Duration) -> Option<String> {
        self.lines.recv_timeout(timeout).ok()
    }

    /// Wait (bounded) for the loud typed log to reach the captured stderr
    /// file; the message may land before this test starts reading.
    fn wait_for_stderr(path: &Path, marker: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            if text.contains(marker) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the typed {marker} error never reached stderr; saw: {text}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn is_lower_hex_64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Direct/unix start (`FAKTOR_RELEASE_DIGEST` unset): stdout is EXACTLY the
/// frozen startup line — no attestation may appear.
#[test]
fn an_unset_export_keeps_stdout_at_the_frozen_startup_line() {
    let binary = Path::new(env!("CARGO_BIN_EXE_faktor-cli"));
    let daemon = Daemon::spawn(binary, None, Stdio::null());
    let first = daemon
        .next_line(FIRST_LINE_TIMEOUT)
        .expect("the daemon must reach the startup line");
    assert!(
        first.starts_with(STARTUP_PREFIX),
        "env unset => the startup line is the first and only stdout line: {first:?}"
    );
    assert!(
        !first.starts_with(DIGEST_PREFIX),
        "no attestation without an export: {first:?}"
    );
    assert!(
        daemon.next_line(Duration::from_millis(750)).is_none(),
        "nothing else may follow the frozen startup line"
    );
}

/// Launched start (`FAKTOR_RELEASE_DIGEST` set to the binary's digest):
/// EXACTLY one attestation line, 64 lowercase hex, equal to the running
/// binary's digest (the same `file_digest`/`self_digest` chain the health
/// report uses), and it arrives BEFORE the startup line that advertises
/// readiness.
#[test]
fn an_exported_digest_is_attested_once_before_the_startup_line() {
    let bindir = tempfile::tempdir().expect("a temp bin dir");
    let (binary, digest) = stable_cli_copy(bindir.path());
    assert!(is_lower_hex_64(&digest), "sha256 hex: {digest}");
    let daemon = Daemon::spawn(&binary, Some(&digest), Stdio::null());
    let attestation = daemon
        .next_line(FIRST_LINE_TIMEOUT)
        .expect("the daemon must attest before readiness");
    let attested = attestation
        .strip_prefix(DIGEST_PREFIX)
        .unwrap_or_else(|| panic!("the first stdout line is the attestation: {attestation:?}"));
    assert_eq!(attested, digest, "the attested digest is the actual binary");
    assert!(
        is_lower_hex_64(attested),
        "the line carries 64 lowercase hex: {attestation:?}"
    );
    let startup = daemon
        .next_line(NEXT_LINE_TIMEOUT)
        .expect("the attestation does not replace the startup line");
    let port = startup
        .strip_prefix(STARTUP_PREFIX)
        .unwrap_or_else(|| panic!("the startup line follows the attestation: {startup:?}"));
    assert!(
        port.parse::<u16>().is_ok_and(|port| port > 0),
        "a usable port: {startup:?}"
    );
    assert!(
        daemon.next_line(Duration::from_millis(750)).is_none(),
        "exactly one attestation line, never a duplicate"
    );
}

/// Launcher-refusal path: the expected export differs. The daemon still
/// prints the ACTUAL digest (never the expected one, no false attestation)
/// and logs the loud typed mismatch to stderr; the startup line still
/// follows (the child's job is to be honest, the launcher's to refuse).
#[test]
fn a_mismatched_export_still_attests_the_actual_digest_and_logs_typed() {
    let bindir = tempfile::tempdir().expect("a temp bin dir");
    let (binary, digest) = stable_cli_copy(bindir.path());
    let expected = "0".repeat(64);
    assert_ne!(
        digest, expected,
        "the fixture must differ from the real hash"
    );
    let stderr_dir = tempfile::tempdir().expect("a temp stderr dir");
    let stderr_path = stderr_dir.path().join("daemon.stderr");
    let stderr_file = std::fs::File::create(&stderr_path).expect("capture stderr");
    let daemon = Daemon::spawn(&binary, Some(&expected), Stdio::from(stderr_file));
    let attestation = daemon
        .next_line(FIRST_LINE_TIMEOUT)
        .expect("the mismatched daemon still attests before readiness");
    assert_eq!(
        attestation,
        format!("{DIGEST_PREFIX}{digest}"),
        "the ACTUAL digest is printed, never the expected value"
    );
    assert!(!attestation.contains(&expected));
    let startup = daemon
        .next_line(NEXT_LINE_TIMEOUT)
        .expect("the mismatched daemon still reaches readiness");
    assert!(startup.starts_with(STARTUP_PREFIX), "{startup:?}");
    Daemon::wait_for_stderr(&stderr_path, "faktor.release_digest.mismatch");
}
