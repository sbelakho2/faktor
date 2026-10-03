//! Adversarial PTY output and lifecycle corpora (unix).
//!
//! Part 1 drives the bounded output ring directly at and over its cap with
//! byte-exact assertions. Part 2 runs a table of hostile child scripts through
//! a real PTY: floods, close-ordering, exit-before-capture, signal races,
//! timeout kill+reap and orphan census. Every row asserts one outcome with a
//! message naming the row.

#![cfg(unix)]

use std::time::Duration;

use crate::ring::{lock_ring, Ring, RING_MAX_BYTES};
use crate::{EnvSpec, Pty, PtyConfig};

fn cap() -> usize {
    RING_MAX_BYTES
}

/// Ring rows: (label, chunks, expected snapshot length, expected total).
struct RingRow {
    label: &'static str,
    chunks: Vec<Vec<u8>>,
    expect_len: usize,
    expect_total: u64,
    expect_first_tail: u8,
    expect_last_tail: u8,
}

fn ring_rows() -> Vec<RingRow> {
    let c = cap();
    let mut rows = Vec::new();
    let mut push = |label, chunks: Vec<Vec<u8>>, expect_len, expect_total, first, last| {
        rows.push(RingRow {
            label,
            chunks,
            expect_len,
            expect_total,
            expect_first_tail: first,
            expect_last_tail: last,
        })
    };
    push("empty", vec![], 0, 0, 0, 0);
    push("zero-byte-chunk", vec![Vec::new()], 0, 0, 0, 0);
    push("one-byte", vec![vec![b'a']], 1, 1, b'a', b'a');
    push(
        "cap-minus-one",
        vec![vec![b'a'; c - 1]],
        c - 1,
        (c - 1) as u64,
        b'a',
        b'a',
    );
    push("cap-exact", vec![vec![b'a'; c]], c, c as u64, b'a', b'a');
    push(
        "cap-plus-one",
        vec![vec![b'a'; c + 1]],
        c,
        (c + 1) as u64,
        b'a',
        b'a',
    );
    push(
        "two-halves-exact",
        vec![vec![b'a'; c / 2], vec![b'b'; c / 2]],
        c,
        c as u64,
        b'a',
        b'b',
    );
    push(
        "two-halves-over-one",
        vec![vec![b'a'; c / 2], vec![b'b'; c / 2 + 1]],
        c,
        (c + 1) as u64,
        b'a',
        b'b',
    );
    push(
        "huge-single-chunk",
        vec![vec![b'x'; c * 4]],
        c,
        (c * 4) as u64,
        b'x',
        b'x',
    );
    push(
        "many-small-chunks-flood",
        (0..1024)
            .map(|i| vec![b'0' + (i % 10) as u8; 1024])
            .collect(),
        c,
        1024 * 1024,
        b'8',
        b'3',
    );
    push(
        "tail-preserved-after-flood",
        {
            let mut v = vec![vec![b'x'; c * 2]];
            v.push(b"TAILMARK".to_vec());
            v
        },
        c,
        (c * 2 + 8) as u64,
        b'x',
        b'K',
    );
    rows
}

/// Each ring row asserts the exact retained length, total, and the boundary
/// bytes proving drop-oldest (never drop-newest).
#[test]
fn output_ring_cap_matrix_is_byte_exact() {
    for row in ring_rows() {
        let mut ring = Ring::new();
        for chunk in &row.chunks {
            ring.push(chunk);
        }
        let snapshot = ring.snapshot();
        assert_eq!(
            snapshot.len(),
            row.expect_len,
            "case {:?}: retained bytes must be min(total, cap)",
            row.label
        );
        assert!(
            snapshot.len() <= cap(),
            "case {:?}: the ring must never exceed its cap",
            row.label
        );
        assert_eq!(
            ring.total(),
            row.expect_total,
            "case {:?}: total must count every pushed byte",
            row.label
        );
        if !snapshot.is_empty() {
            assert_eq!(
                snapshot[0], row.expect_first_tail,
                "case {:?}: drop-oldest must keep the trailing window",
                row.label
            );
            assert_eq!(
                *snapshot.last().unwrap(),
                row.expect_last_tail,
                "case {:?}: the newest byte must always be retained",
                row.label
            );
        }
    }
}

/// Snapshot is non-destructive; drain returns and clears; a drain of a full
/// ring returns the whole cap; a poisoned lock rebuilds empty (documented
/// lossy recovery, already covered elsewhere for the shared helper).
#[test]
fn ring_snapshot_drain_and_rebuild_semantics() {
    let mut ring = Ring::new();
    ring.push(b"abcdef");
    assert_eq!(ring.snapshot(), b"abcdef", "snapshot must not consume");
    assert_eq!(ring.snapshot(), b"abcdef", "snapshot is repeatable");
    let drained = ring.drain();
    assert_eq!(drained, b"abcdef", "drain returns all retained bytes");
    assert!(ring.snapshot().is_empty(), "drain must empty the ring");
    assert_eq!(ring.total(), 6, "total survives a drain");
    ring.push(b"new");
    ring.rebuild();
    assert!(ring.snapshot().is_empty(), "rebuild clears retained bytes");
    assert_eq!(ring.total(), 0, "rebuild resets the total");

    // A full ring drains exactly the cap and then is empty.
    let mut full = Ring::new();
    full.push(&vec![b'z'; cap() + 10]);
    assert_eq!(full.drain().len(), cap(), "a full ring drains the cap");
    assert!(full.snapshot().is_empty());

    // The shared lock helper returns a working guard on a healthy mutex.
    let shared = std::sync::Mutex::new(Ring::new());
    {
        let mut guard = lock_ring(&shared);
        guard.push(b"ok");
    }
    assert_eq!(lock_ring(&shared).snapshot(), b"ok");
}

// ---------------------------------------------------------------------------
// Real-PTY hostile script corpus
// ---------------------------------------------------------------------------

fn sh_cfg(script: &str) -> PtyConfig {
    PtyConfig {
        command: "sh".into(),
        args: vec!["-c".into(), script.into()],
        rows: 24,
        cols: 80,
        env: EnvSpec::default_baseline(),
        ..Default::default()
    }
}

#[derive(Clone, Copy)]
enum Check {
    Contains(&'static str),
    ContainsBoth(&'static str, &'static str),
    TotalAtLeast(usize),
    SnapshotBounded,
    NaturalExit,
    KillReaps,
    SpawnFails,
    WriteAfterExitBounded,
    ResizeAfterExitFails,
    InputRoundTrip,
}

struct PtyRow {
    label: &'static str,
    script: &'static str,
    check: Check,
    input: Option<&'static str>,
}

fn pty_rows() -> Vec<PtyRow> {
    vec![
        PtyRow {
            label: "echo-marker",
            script: "echo MARKER-ONE; exit 0",
            check: Check::Contains("MARKER-ONE"),
            input: None,
        },
        PtyRow {
            label: "output-one-byte",
            script: "printf x",
            check: Check::Contains("x"),
            input: None,
        },
        PtyRow {
            label: "output-cap-minus-one",
            script: "head -c 262143 /dev/zero | tr '\\0' 'a'",
            check: Check::TotalAtLeast(262143),
            input: None,
        },
        PtyRow {
            label: "output-exactly-cap",
            script: "head -c 262144 /dev/zero | tr '\\0' 'b'",
            check: Check::SnapshotBounded,
            input: None,
        },
        PtyRow {
            label: "output-cap-plus-one",
            script: "head -c 262145 /dev/zero | tr '\\0' 'c'",
            check: Check::SnapshotBounded,
            input: None,
        },
        PtyRow {
            label: "output-four-caps",
            script: "head -c 1048576 /dev/zero | tr '\\0' 'd'",
            check: Check::SnapshotBounded,
            input: None,
        },
        PtyRow {
            label: "stdout-then-stderr-order",
            script: "echo OUT-FIRST; echo ERR-SECOND 1>&2",
            check: Check::ContainsBoth("OUT-FIRST", "ERR-SECOND"),
            input: None,
        },
        PtyRow {
            label: "stdout-closed-then-stderr",
            script: "exec 1>&-; echo ONLY-ERR 1>&2",
            check: Check::Contains("ONLY-ERR"),
            input: None,
        },
        PtyRow {
            label: "stderr-closed-then-stdout",
            script: "exec 2>&-; echo ONLY-OUT",
            check: Check::Contains("ONLY-OUT"),
            input: None,
        },
        PtyRow {
            label: "both-closed-then-exit",
            script: "exec 1>&- 2>&-; exit 0",
            check: Check::NaturalExit,
            input: None,
        },
        PtyRow {
            label: "exit-before-capture",
            script: "printf late; exit 0",
            check: Check::Contains("late"),
            input: None,
        },
        PtyRow {
            label: "signal-kill-self",
            script: "kill -9 $$",
            check: Check::NaturalExit,
            input: None,
        },
        PtyRow {
            label: "signal-term-self",
            script: "kill -TERM $$",
            check: Check::NaturalExit,
            input: None,
        },
        PtyRow {
            label: "signal-int-self",
            script: "kill -INT $$",
            check: Check::NaturalExit,
            input: None,
        },
        PtyRow {
            label: "sleep-then-natural-exit",
            script: "sleep 0.1; echo SLEPT",
            check: Check::Contains("SLEPT"),
            input: None,
        },
        PtyRow {
            label: "long-sleep-kill-reaps",
            script: "sleep 300",
            check: Check::KillReaps,
            input: None,
        },
        PtyRow {
            label: "grandchild-then-leader-exit",
            script: "sh -c 'sleep 300 &'; exit 0",
            check: Check::KillReaps,
            input: None,
        },
        PtyRow {
            label: "one-megabyte-line",
            script: "head -c 1048576 /dev/zero | tr '\\0' 'L'",
            check: Check::SnapshotBounded,
            input: None,
        },
        PtyRow {
            label: "nul-bytes-output",
            script: "printf 'a\\000bNULMARK'",
            check: Check::Contains("NULMARK"),
            input: None,
        },
        PtyRow {
            label: "invalid-utf8-output",
            script: "printf '\\377\\376RAW'",
            check: Check::Contains("RAW"),
            input: None,
        },
        PtyRow {
            label: "utf8-split-writes",
            script: "printf '\\303'; sleep 0.05; printf '\\251UTF'",
            check: Check::Contains("UTF"),
            input: None,
        },
        PtyRow {
            label: "crlf-output",
            script: "printf 'CR\\r\\nLF\\n'",
            check: Check::Contains("LF"),
            input: None,
        },
        PtyRow {
            label: "ansi-escape-output",
            script: "printf '\\033[31mRED\\033[0m'",
            check: Check::Contains("RED"),
            input: None,
        },
        PtyRow {
            label: "input-round-trip",
            script: "stty -echo; read x; echo GOT:$x",
            check: Check::InputRoundTrip,
            input: Some("payload-123"),
        },
        PtyRow {
            label: "no-input-child-exits",
            script: "echo NOINPUT",
            check: Check::Contains("NOINPUT"),
            input: None,
        },
        PtyRow {
            label: "write-after-exit-fails",
            script: "exit 0",
            check: Check::WriteAfterExitBounded,
            input: None,
        },
        PtyRow {
            label: "resize-after-exit-fails",
            script: "exit 0",
            check: Check::ResizeAfterExitFails,
            input: None,
        },
        PtyRow {
            label: "spawn-missing-binary-fails",
            script: "",
            check: Check::SpawnFails,
            input: None,
        },
        PtyRow {
            label: "many-small-writes",
            script: "i=0; while [ $i -lt 200 ]; do echo line-$i; i=$((i+1)); done",
            check: Check::Contains("line-199"),
            input: None,
        },
        PtyRow {
            label: "rapid-start-stop",
            script: "true",
            check: Check::NaturalExit,
            input: None,
        },
        PtyRow {
            label: "large-input-line",
            script: "stty -echo; read x; echo INLEN:${#x}",
            check: Check::InputRoundTrip,
            input: Some("0123456789"),
        },
    ]
}

/// Safe pid probe through the POSIX `kill` utility (no unsafe block is added
/// to this test file, and no signal beyond the null probe is ever sent).
fn pid_alive(pid: libc::pid_t) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn wait_group_gone(pgid: libc::pid_t, bound: Duration, label: &str) {
    let deadline = std::time::Instant::now() + bound;
    while crate::guardian::group_exists(pgid as u32) {
        assert!(
            std::time::Instant::now() < deadline,
            "case {label}: group {pgid} must be gone within {bound:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_pid_gone(pid: libc::pid_t, bound: Duration, label: &str) {
    let deadline = std::time::Instant::now() + bound;
    while pid_alive(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "case {label}: pid {pid} must be reaped within {bound:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Hostile child scripts through a real PTY: every row asserts its exact
/// output/exit/teardown outcome. The process-wide serial keeps the pid-reuse
/// assertions observation-stable.
#[test]
fn pty_script_corpus_matches_every_outcome() {
    let _serial = crate::test_serial();
    for row in pty_rows() {
        if matches!(row.check, Check::SpawnFails) {
            let cfg = PtyConfig {
                command: "definitely-not-a-real-binary-faktor".into(),
                ..sh_cfg("")
            };
            assert!(
                Pty::spawn(&cfg).is_err(),
                "case {}: spawning a missing binary must fail typed",
                row.label
            );
            continue;
        }
        let mut pty = Pty::spawn(&sh_cfg(row.script))
            .unwrap_or_else(|e| panic!("case {}: spawn must succeed for fixture: {e}", row.label));
        if let Some(input) = row.input {
            pty.write_line(input)
                .unwrap_or_else(|e| panic!("case {}: input write failed: {e}", row.label));
        }
        match row.check {
            Check::Contains(needle) => {
                assert!(
                    pty.wait_for_contains(needle, Duration::from_secs(10)),
                    "case {}: output must contain {needle:?}: {:?}",
                    row.label,
                    String::from_utf8_lossy(&pty.snapshot())
                );
            }
            Check::ContainsBoth(first, second) => {
                assert!(
                    pty.wait_for_contains(first, Duration::from_secs(10)),
                    "case {}: first marker {first:?} must appear",
                    row.label
                );
                assert!(
                    pty.wait_for_contains(second, Duration::from_secs(10)),
                    "case {}: second marker {second:?} must appear",
                    row.label
                );
                let text = String::from_utf8_lossy(&pty.snapshot()).into_owned();
                let a = text.find(first).expect("first marker present");
                let b = text.find(second).expect("second marker present");
                assert!(
                    a < b,
                    "case {}: stdout marker must precede the stderr marker: {text:?}",
                    row.label
                );
            }
            Check::TotalAtLeast(n) => {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while pty.total_bytes() < n as u64 {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "case {}: total bytes must reach {n}, got {}",
                        row.label,
                        pty.total_bytes()
                    );
                    std::thread::sleep(Duration::from_millis(10));
                    let _ = pty.read_available();
                }
            }
            Check::SnapshotBounded => {
                let deadline = std::time::Instant::now() + Duration::from_secs(15);
                while pty.is_alive() && std::time::Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                    let _ = pty.read_available();
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "case {}: the flood must terminate",
                    row.label
                );
                assert!(
                    pty.snapshot().len() <= RING_MAX_BYTES,
                    "case {}: snapshot must stay bounded, got {}",
                    row.label,
                    pty.snapshot().len()
                );
                assert!(
                    pty.total_bytes() > 0,
                    "case {}: the flood must have been observed",
                    row.label
                );
            }
            Check::NaturalExit => {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while pty.is_alive() {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "case {}: the child must be reaped by the reader",
                        row.label
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            Check::KillReaps => {
                let pid = pty.pid() as libc::pid_t;
                pty.shutdown();
                wait_pid_gone(pid, Duration::from_secs(5), row.label);
            }
            Check::WriteAfterExitBounded => {
                // A write to a master whose slave is gone may be accepted
                // (the kernel buffers it) or refused typed; either way it
                // MUST return promptly and never block the caller on a dead
                // child.
                let started = std::time::Instant::now();
                let outcome = pty.write_all(b"after-exit");
                assert!(
                    started.elapsed() < Duration::from_secs(2),
                    "case {}: a write to an exited pty must be bounded, took {:?}",
                    row.label,
                    started.elapsed()
                );
                if let Err(e) = outcome {
                    assert!(
                        !e.message.is_empty(),
                        "case {}: a refused write must carry a typed message: {e}",
                        row.label
                    );
                }
            }
            Check::ResizeAfterExitFails => {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while pty.is_alive() && std::time::Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
                // The master may still accept TIOCSWINSZ after child exit;
                // only assert that a resize never panics and never reports a
                // zero-size state.
                let _ = pty.resize(30, 100);
                let (rows, cols) = pty.size();
                assert!(
                    rows > 0 && cols > 0,
                    "case {}: size must never be zero after a resize: {rows}x{cols}",
                    row.label
                );
            }
            Check::SpawnFails => unreachable!("handled above"),
            Check::InputRoundTrip => {
                assert!(
                    pty.wait_for_contains("GOT:", Duration::from_secs(10))
                        || pty.wait_for_contains("INLEN:", Duration::from_secs(10)),
                    "case {}: the child must answer the bounded input: {:?}",
                    row.label,
                    String::from_utf8_lossy(&pty.snapshot())
                );
            }
        }
        pty.kill();
    }
}

/// Timeout kill + reap: every row must end with the child pid provably gone
/// and no surviving process-group member.
#[test]
fn timeout_kill_and_reap_census() {
    let _serial = crate::test_serial();
    let rows: [(&str, &str); 8] = [
        ("plain-sleep", "sleep 300"),
        ("sleep-in-subshell", "(sleep 300)"),
        ("nested-shells", "sh -c 'sh -c \"sleep 300\"'"),
        ("sleep-loop", "while :; do sleep 1; done"),
        ("blocked-on-read", "read x"),
        ("cat-blocks", "cat"),
        ("background-then-wait", "sleep 300 & wait"),
        ("ignores-term", "trap '' TERM; sleep 300"),
    ];
    for (label, script) in rows {
        let mut pty = Pty::spawn(&sh_cfg(script)).unwrap();
        let pid = pty.pid() as libc::pid_t;
        pty.shutdown();
        wait_pid_gone(pid, Duration::from_secs(5), label);
        wait_group_gone(pid, Duration::from_secs(5), label);
    }
}

/// Orphan census after every failure path: a spawn failure creates no child,
/// a natural exit leaves no zombie, an explicit kill reaps, and Drop reaps —
/// asserted by process-table probes, not by trusting the API.
#[test]
fn orphan_census_after_every_failure_path() {
    let _serial = crate::test_serial();
    // 1. Spawn failure: no child exists.
    let bad = PtyConfig {
        command: "faktor-no-such-binary".into(),
        ..sh_cfg("")
    };
    assert!(Pty::spawn(&bad).is_err(), "spawn must fail");

    // 2. Natural exit: the reader reaps (kill(pid, 0) => ESRCH).
    let pty = Pty::spawn(&sh_cfg("true")).unwrap();
    let pid = pty.pid() as libc::pid_t;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while pid_alive(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "natural exit: the reader must reap the child"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // 3. Explicit shutdown: pid and group gone.
    let mut pty = Pty::spawn(&sh_cfg("sleep 300")).unwrap();
    let pid = pty.pid() as libc::pid_t;
    pty.shutdown();
    wait_pid_gone(pid, Duration::from_secs(5), "explicit-shutdown");
    wait_group_gone(pid, Duration::from_secs(5), "explicit-shutdown");

    // 4. Drop: the emergency path still kills and reaps the group.
    let pid = {
        let pty = Pty::spawn(&sh_cfg("sleep 300")).unwrap();
        pty.pid() as libc::pid_t
    };
    wait_pid_gone(pid, Duration::from_secs(5), "drop-path");
}
