//! Adversarial guardian / control-pipe corpus (unix).
//!
//! Pure EOF-decision matrix ([`decide_on_eof`] over synthetic and real
//! identities), probe boundaries, the exact exit-code contract, and real
//! guardian lifecycles: deliberate release, dead-child release, malformed
//! records, repeated start/stop, forced FIFO control channel and the
//! CLOEXEC-at-creation seam. Every row asserts one exact outcome with its own
//! message.

#![cfg(unix)]

use std::time::Duration;

use super::*;

/// The largest pid the kernel can never assign (Linux `/proc/sys/kernel/
/// pid_max`; pid_max itself is out of range). Falls back to a high constant.
fn never_assigned_pid() -> u32 {
    std::fs::read_to_string("/proc/sys/kernel/pid_max")
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        .unwrap_or(1 << 22)
}

fn pid_gone(pid: u32) -> bool {
    !group_exists(pid) && process_start_time(pid).is_none()
}

/// Spawn a child that is its own process-group leader (`setsid`-equivalent
/// via `process_group(0)`), so the guardian's group kill has a real target.
fn spawn_own_group_child(script: &str) -> std::process::Child {
    use std::os::unix::process::CommandExt as _;
    std::process::Command::new("sh")
        .args(["-c", script])
        .process_group(0)
        .spawn()
        .expect("fixture child spawns")
}

fn wait_group_dead(pid: u32, label: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while group_exists(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "case {label}: process group {pid} must be gone within the bound"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The pure EOF decision matrix: malformed records are refused, live
/// verified records kill, recycled records refuse, gone groups do nothing.
#[test]
fn decide_on_eof_matrix_is_exact() {
    let self_pid = std::process::id();
    let dead = never_assigned_pid();
    let live = ProcessIdentity::capture(self_pid, self_pid);
    assert_eq!(
        live.verify(),
        IdentityVerdict::Match,
        "the live self identity must verify as Match on a probe-capable platform"
    );

    struct Row {
        label: &'static str,
        identity: ProcessIdentity,
        expect: GuardianAction,
    }
    let rows = vec![
        Row {
            label: "pid-zero",
            identity: ProcessIdentity {
                pid: 0,
                pgid: 0,
                start_time: None,
            },
            expect: GuardianAction::RefusedMalformed,
        },
        Row {
            label: "pid-zero-pgid-live",
            identity: ProcessIdentity {
                pid: 0,
                pgid: self_pid,
                start_time: None,
            },
            expect: GuardianAction::RefusedMalformed,
        },
        Row {
            label: "pgid-zero",
            identity: ProcessIdentity {
                pid: self_pid,
                pgid: 0,
                start_time: None,
            },
            expect: GuardianAction::RefusedMalformed,
        },
        Row {
            label: "pgid-not-leader",
            identity: ProcessIdentity {
                pid: self_pid,
                pgid: self_pid - 1,
                start_time: None,
            },
            expect: GuardianAction::RefusedMalformed,
        },
        Row {
            label: "pgid-beyond-pid-range",
            identity: ProcessIdentity {
                pid: i32::MAX as u32 + 1,
                pgid: i32::MAX as u32 + 1,
                start_time: None,
            },
            expect: GuardianAction::RefusedMalformed,
        },
        Row {
            label: "pid-u32-max",
            identity: ProcessIdentity {
                pid: u32::MAX,
                pgid: u32::MAX,
                start_time: Some(1),
            },
            expect: GuardianAction::RefusedMalformed,
        },
        Row {
            label: "live-without-marker",
            identity: ProcessIdentity {
                pid: self_pid,
                pgid: self_pid,
                start_time: None,
            },
            expect: GuardianAction::RefusedUnverifiable,
        },
        Row {
            label: "live-marker-mismatch",
            identity: ProcessIdentity {
                pid: self_pid,
                pgid: self_pid,
                start_time: Some(live.start_time.unwrap_or(0).wrapping_add(1)),
            },
            expect: GuardianAction::RefusedRecycled,
        },
        Row {
            label: "gone-pid",
            identity: ProcessIdentity {
                pid: dead,
                pgid: dead,
                start_time: Some(0),
            },
            expect: GuardianAction::NothingToDo,
        },
        Row {
            label: "gone-pid-no-marker",
            identity: ProcessIdentity {
                pid: dead,
                pgid: dead,
                start_time: None,
            },
            expect: GuardianAction::NothingToDo,
        },
        Row {
            label: "live-marker-exact",
            identity: live,
            expect: GuardianAction::Kill,
        },
    ];
    for row in rows {
        assert_eq!(
            decide_on_eof(&row.identity),
            row.expect,
            "case {:?}: EOF decision for {:?}",
            row.label,
            row.identity
        );
    }
}

/// Unknown-detail guard: `AccessDenied`-equivalent unverifiable records must
/// never authorize a kill.
#[test]
fn unverifiable_identities_never_authorize_a_kill() {
    for start_time in [None, Some(0)] {
        let identity = ProcessIdentity {
            pid: std::process::id(),
            pgid: std::process::id(),
            start_time,
        };
        let action = decide_on_eof(&identity);
        if start_time.is_none() {
            assert_eq!(
                action,
                GuardianAction::RefusedUnverifiable,
                "a marker-less live record must refuse, never kill"
            );
        } else {
            assert_eq!(
                action,
                GuardianAction::RefusedRecycled,
                "a mismatched marker must refuse as recycled, never kill"
            );
        }
    }
}

/// The exit-code contract is frozen and total.
#[test]
fn guardian_action_exit_codes_are_frozen() {
    let rows = [
        (GuardianAction::Kill, GUARDIAN_EXIT_KILLED, 10),
        (GuardianAction::NothingToDo, GUARDIAN_EXIT_NOTHING_TO_DO, 0),
        (
            GuardianAction::RefusedRecycled,
            GUARDIAN_EXIT_REFUSED_RECYCLED,
            11,
        ),
        (
            GuardianAction::RefusedUnverifiable,
            GUARDIAN_EXIT_REFUSED_UNVERIFIABLE,
            12,
        ),
        (
            GuardianAction::RefusedMalformed,
            GUARDIAN_EXIT_REFUSED_MALFORMED,
            13,
        ),
    ];
    for (action, constant, expected) in rows {
        assert_eq!(
            action.exit_code(),
            expected,
            "case {action:?}: exit code must equal {expected}"
        );
        assert_eq!(
            constant, expected,
            "case {action:?}: the public constant must be frozen"
        );
    }
}

/// Probe boundaries: pid 0 and never-assigned pids have no start time and no
/// group; a live own-group child has both while it lives and neither after it
/// is reaped.
#[test]
fn process_probe_boundaries_are_exact() {
    assert_eq!(process_start_time(0), None, "pid 0 has no start time");
    assert!(!group_exists(0), "pgid 0 is never a group");
    assert_eq!(
        process_start_time(u32::MAX),
        None,
        "an out-of-range pid has no start time"
    );
    assert!(
        !group_exists(u32::MAX),
        "an out-of-range pgid must never be signalled"
    );
    assert!(
        !group_exists(i32::MAX as u32 + 1),
        "a pgid beyond pid_t must never be signalled"
    );
    let dead = never_assigned_pid();
    assert_eq!(
        process_start_time(dead),
        None,
        "pid_max itself is never assigned"
    );
    assert!(!group_exists(dead), "pid_max has no group");
    assert!(
        pid_gone(dead),
        "the never-assigned pid must be provably gone"
    );

    let mut child = spawn_own_group_child("sleep 30");
    let pid = child.id();
    assert!(
        group_exists(pid),
        "a live own-group child must have a live group"
    );
    assert!(
        process_start_time(pid).is_some(),
        "a live child must carry a start-time marker"
    );
    let identity = ProcessIdentity::capture(pid, pid);
    assert_eq!(
        identity.verify(),
        IdentityVerdict::Match,
        "the live child identity must verify as Match"
    );
    let _ = child.kill();
    let _ = child.wait();
    wait_group_dead(pid, "probe-lifecycle");
    assert_eq!(
        process_start_time(pid),
        None,
        "a reaped child has no start-time marker"
    );
    assert!(!group_exists(pid), "a reaped child's group is gone");
}

/// Deliberate release against a live verified group kills it; release against
/// an already-reaped child does nothing; malformed records are refused and
/// never signal; double release is idempotent.
#[test]
fn guardian_release_matrix_is_exact() {
    let _serial = crate::test_serial();

    // Row: live child -> Kill(10).
    {
        let mut child = spawn_own_group_child("sleep 30");
        let pid = child.id();
        let identity = ProcessIdentity::capture(pid, pid);
        let mut guardian = GuardianHandle::spawn(identity).expect("guardian spawns");
        assert!(!guardian.is_released(), "a fresh guardian is not released");
        assert!(guardian.pid() > 0, "the guardian has a pid");
        assert_eq!(
            guardian.release(),
            Some(GUARDIAN_EXIT_KILLED),
            "case live-release: the guardian must kill the live verified group"
        );
        let _ = child.wait();
        wait_group_dead(pid, "live-release");
        assert!(
            guardian.is_released(),
            "release must close the control pipe"
        );
        assert_eq!(guardian.release(), None, "double release must be a no-op");
    }

    // Row: already-reaped child -> NothingToDo(0), no signal.
    {
        let mut child = spawn_own_group_child("true");
        let pid = child.id();
        let identity = ProcessIdentity::capture(pid, pid);
        let mut guardian = GuardianHandle::spawn(identity).expect("guardian spawns");
        let _ = child.wait();
        wait_group_dead(pid, "reaped-release");
        assert_eq!(
            guardian.release(),
            Some(GUARDIAN_EXIT_NOTHING_TO_DO),
            "case reaped-release: a provably-gone group must exit cleanly"
        );
    }

    // Row: malformed pid-0 record -> RefusedMalformed(13), no signal.
    {
        let mut guardian = GuardianHandle::spawn(ProcessIdentity {
            pid: 0,
            pgid: 0,
            start_time: None,
        })
        .expect("guardian spawns");
        assert_eq!(
            guardian.release(),
            Some(GUARDIAN_EXIT_REFUSED_MALFORMED),
            "case malformed-release: a pid-0 record must be refused, never signalled"
        );
    }

    // Row: Drop releases the guardian and kills a live group.
    {
        let mut child = spawn_own_group_child("sleep 30");
        let pid = child.id();
        let identity = ProcessIdentity::capture(pid, pid);
        {
            let _guardian = GuardianHandle::spawn(identity).expect("guardian spawns");
        }
        let _ = child.wait();
        wait_group_dead(pid, "drop-release");
    }
}

/// Repeated start/stop: every round reaps its guardian with the exact kill
/// code and leaves no group member behind (a start/stop storm must not leak
/// guardians or children).
#[test]
fn repeated_guardian_start_stop_leaves_no_orphans() {
    let _serial = crate::test_serial();
    for round in 0..10u32 {
        let mut child = spawn_own_group_child("sleep 30");
        let pid = child.id();
        let identity = ProcessIdentity::capture(pid, pid);
        let mut guardian = GuardianHandle::spawn(identity)
            .unwrap_or_else(|e| panic!("round {round}: guardian spawn failed: {e}"));
        assert!(
            !guardian.is_released(),
            "round {round}: a fresh guardian must not be released"
        );
        assert_eq!(
            guardian.release(),
            Some(GUARDIAN_EXIT_KILLED),
            "round {round}: the guardian must kill the live verified group"
        );
        let _ = child.wait();
        wait_group_dead(pid, &format!("round-{round}"));
        assert!(
            pid_gone(guardian.pid().max(pid)),
            "round {round}: no guardian or child may survive"
        );
    }
}

/// The forced-FIFO control channel (the macOS path on Linux CI) still kills a
/// live group and still exits cleanly for a dead record.
#[test]
fn forced_fifo_control_channel_release_matrix() {
    let _serial = crate::test_serial();
    struct Unforce;
    impl Drop for Unforce {
        fn drop(&mut self) {
            force_fifo_control_pipe_for_tests(false);
        }
    }
    force_fifo_control_pipe_for_tests(true);
    let _unforce = Unforce;

    // FIFO + live child -> Kill(10).
    let mut child = spawn_own_group_child("sleep 30");
    let pid = child.id();
    let identity = ProcessIdentity::capture(pid, pid);
    let mut guardian = GuardianHandle::spawn(identity).expect("fifo guardian spawns");
    assert_eq!(
        guardian.release(),
        Some(GUARDIAN_EXIT_KILLED),
        "case fifo-live: the FIFO control channel must still kill the group"
    );
    let _ = child.wait();
    wait_group_dead(pid, "fifo-live");

    // FIFO + dead record -> NothingToDo(0).
    let dead = never_assigned_pid();
    let mut dead_guardian = GuardianHandle::spawn(ProcessIdentity {
        pid: dead,
        pgid: dead,
        start_time: Some(0),
    })
    .expect("fifo guardian spawns");
    assert_eq!(
        dead_guardian.release(),
        Some(GUARDIAN_EXIT_NOTHING_TO_DO),
        "case fifo-dead: a dead record on the FIFO channel exits cleanly"
    );
}

/// The control-pipe seam proves FD_CLOEXEC is set on BOTH ends at the instant
/// they exist (read from `/proc/self/fdinfo`) and that the seam fires once per
/// creation with exactly the pair.
#[test]
#[cfg(target_os = "linux")]
fn control_pipe_seam_observes_cloexec_on_both_ends() {
    let _serial = crate::test_serial();
    use std::sync::{Arc, Mutex};

    let observations: Arc<Mutex<Vec<(i32, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let observations = observations.clone();
        install_control_pipe_seam(Box::new(move |fds: &[std::os::fd::RawFd; 2]| {
            let mut guard = observations.lock().unwrap();
            for fd in fds {
                let cloexec = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}"))
                    .ok()
                    .and_then(|text| {
                        text.lines()
                            .find_map(|line| line.strip_prefix("flags:"))
                            .and_then(|value| u32::from_str_radix(value.trim(), 8).ok())
                    })
                    .is_some_and(|flags| flags & 0o2000000 != 0);
                guard.push((*fd, cloexec));
            }
        }));
    }

    let dead = never_assigned_pid();
    let mut guardian = GuardianHandle::spawn(ProcessIdentity {
        pid: dead,
        pgid: dead,
        start_time: Some(0),
    })
    .expect("guardian spawns under the seam");
    clear_control_pipe_seam();
    let _ = guardian.release();

    let observed = observations.lock().unwrap().clone();
    assert_eq!(
        observed.len(),
        2,
        "the seam must fire exactly once per creation with the fd pair: {observed:?}"
    );
    for (fd, cloexec) in &observed {
        assert!(
            *cloexec,
            "fd {fd} must carry FD_CLOEXEC at the instant it exists (no fork/exec window)"
        );
    }
}
