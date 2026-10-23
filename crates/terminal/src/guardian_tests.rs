//! Daemon-death guardian oracles (Linux + macOS): zero orphans on SIGKILL.

use super::*;

// ============ daemon-death guardian (Linux + macOS): zero orphans on SIGKILL =====

/// Bounded generic wait used by the guardian rows.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wait_until<F: FnMut() -> bool>(what: &str, bound: Duration, mut cond: F) {
    let deadline = std::time::Instant::now() + bound;
    while !cond() {
        assert!(std::time::Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Live process ids whose full command line matches `marker` (`pgrep -f`).
/// The unique marker is the identity proof: a recycled or unrelated pid can
/// never carry it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn marker_processes(marker: &str) -> Vec<u32> {
    let out = std::process::Command::new("pgrep")
        .args(["-f", "--", marker])
        .output()
        .expect("pgrep");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

/// Read one pid written by a spawned fixture (bounded).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_pid_file(path: &std::path::Path) -> u32 {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse() {
                return pid;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no pid in {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Spawn a detached marked tree: `sh` backgrounds a `sleep 300` (reporting
/// its pid), then waits. Returns (handle, descendant pid, guardian pid).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn spawn_marked_wait_tree(
    sup: &ProcessSupervisor,
    dir: &std::path::Path,
    marker: &str,
) -> (ChildHandle, u32, u32) {
    let pidfile = dir.join(format!("descendant-{marker}.pid"));
    let script = format!(
        "sleep 300 & echo $! > '{}'; wait # {marker}",
        pidfile.display()
    );
    let handle = sup.spawn(sh(&script)).expect("spawn marked tree");
    let descendant = read_pid_file(&pidfile);
    let guardian = sup
        .guardian_pid_for_tests(handle.id)
        .expect("every supervised child must own a guardian");
    (handle, descendant, guardian)
}

/// ADVERSARIAL (a): with the owner alive, dropping the guardian's control
/// pipe — exactly what a daemon SIGKILL does to the last write end — must
/// SIGTERM→SIGKILL the whole recorded group, and the guardian must then
/// exit on its own (no guardian leak).
#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn guardian_owner_death_kills_the_whole_group() {
    let (dir, sup) = supervisor();
    let marker = format!("faktor-guardian-owner-death-{}", std::process::id());
    let (handle, descendant, guardian) = spawn_marked_wait_tree(&sup, dir.path(), &marker);
    assert!(
        !pid_is_gone(handle.pid),
        "leader must be alive before release"
    );
    assert!(!pid_is_gone(descendant), "descendant must be alive");
    assert!(!pid_is_gone(guardian), "guardian must be alive");
    assert!(
        !marker_processes(&marker).is_empty(),
        "the marker proves the shell is alive"
    );

    assert!(
        sup.release_guardian_for_tests(handle.id),
        "the row owns a live guardian"
    );
    wait_until("owner-death group kill", Duration::from_secs(10), || {
        group_gone(handle.pid)
            && pid_is_gone(handle.pid)
            && pid_is_gone(descendant)
            && pid_is_gone(guardian)
            && marker_processes(&marker).is_empty()
    });
    for _ in 0..200 {
        if !sup.reap().is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(sup.registered(), 0, "the row is collectible");
}

/// ADVERSARIAL (a, post-reap gap): the direct child exits first while its
/// background descendant survives in the group; the row's guarded signal
/// paths correctly refuse after the reap, so only the identity-verified
/// guardian can end the descendant when the owner dies.
#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn guardian_owner_death_kills_descendants_of_a_consumed_leader() {
    let (dir, sup) = supervisor();
    let marker = format!("faktor-guardian-post-reap-{}", std::process::id());
    let pidfile = dir.path().join("post-reap.pid");
    let script = format!(
        "sleep 300 & echo $! > '{}'; true # {marker}",
        pidfile.display()
    );
    let handle = sup.spawn(sh(&script)).expect("spawn leader+descendant");
    let descendant = read_pid_file(&pidfile);
    let guardian = sup.guardian_pid_for_tests(handle.id).unwrap();
    wait_until(
        "the single reaper consumes the leader",
        Duration::from_secs(10),
        || {
            sup.reap_state(handle.id)
                .map(|reap| reap.reaped())
                .unwrap_or(true)
        },
    );
    assert!(pid_is_gone(handle.pid), "the leader was consumed");
    assert!(
        !pid_is_gone(descendant),
        "the descendant still holds the group after the reap"
    );

    assert!(sup.release_guardian_for_tests(handle.id));
    wait_until("post-reap group kill", Duration::from_secs(10), || {
        group_gone(handle.pid)
            && pid_is_gone(descendant)
            && pid_is_gone(guardian)
            && marker_processes(&marker).is_empty()
    });
    let _ = sup.reap();
}

/// ADVERSARIAL (b): the normal kill route must leave neither a descendant
/// nor a guardian behind once the row is collected.
#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn normal_kill_route_leaves_no_guardian_or_descendant() {
    let (dir, sup) = supervisor();
    let marker = format!("faktor-guardian-normal-kill-{}", std::process::id());
    let (handle, descendant, guardian) = spawn_marked_wait_tree(&sup, dir.path(), &marker);
    sup.kill(handle.id, 500).expect("normal kill");
    wait_until(
        "normal kill group extinction",
        Duration::from_secs(10),
        || group_gone(handle.pid) && pid_is_gone(descendant),
    );
    let mut collected = false;
    for _ in 0..200 {
        if sup.reap().iter().any(|r| r.id == handle.id) {
            collected = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(collected, "the killed row is collectible");
    wait_until("guardian release exit", Duration::from_secs(10), || {
        pid_is_gone(guardian) && marker_processes(&marker).is_empty()
    });
    assert_eq!(sup.registered(), 0);
}

/// ADVERSARIAL (c): a forged (recycled) identity record is refused and the
/// addressed pid is never signalled; a structurally impossible record is
/// refused before any fork. The unique marker proves the exact target
/// stayed alive.
#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn guardian_refuses_forged_identity_without_killing_the_target() {
    use std::os::unix::process::CommandExt;
    let marker = format!("faktor-guardian-forged-{}", std::process::id());
    let script = format!("sleep 300 & wait # {marker}");
    let mut decoy = std::process::Command::new("/bin/sh")
        .args(["-c", &script])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn decoy");
    let pid = decoy.id();
    wait_until("decoy visible", Duration::from_secs(5), || {
        marker_processes(&marker).contains(&pid)
    });
    let live = guardian::ProcessIdentity::capture(pid, pid);
    assert!(live.start_time.is_some(), "marker captured while alive");
    let forged = guardian::ProcessIdentity {
        start_time: live.start_time.map(|t| t.wrapping_add(1)),
        ..live
    };
    let mut guardian = guardian::GuardianHandle::spawn_with_identity(forged).unwrap();
    let guardian_pid = guardian.pid();
    assert_eq!(
        guardian.release(),
        Some(guardian::GUARDIAN_EXIT_REFUSED_RECYCLED),
        "a recycled record must refuse"
    );
    assert!(pid_is_gone(guardian_pid), "the refusing guardian exits");
    assert!(
        marker_processes(&marker).contains(&pid),
        "a forged record must never signal the addressed pid"
    );
    // Structurally impossible records are refused before any fork.
    for malformed in [
        guardian::ProcessIdentity {
            pid,
            pgid: 0,
            start_time: live.start_time,
        },
        guardian::ProcessIdentity {
            pid: 0,
            pgid: 0,
            start_time: None,
        },
        guardian::ProcessIdentity {
            pid,
            pgid: pid + 1,
            start_time: live.start_time,
        },
    ] {
        assert!(
            guardian::GuardianHandle::spawn_with_identity(malformed).is_err(),
            "malformed identity must be refused: {malformed:?}"
        );
    }
    let _ = decoy.kill();
    let _ = decoy.wait();
    wait_until("decoy cleanup", Duration::from_secs(5), || {
        marker_processes(&marker).is_empty()
    });
}

/// ADVERSARIAL (c, scoping): releasing one row's guardian kills exactly its
/// own group — a sibling supervised tree, marker included, stays alive.
#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn guardian_kill_is_scoped_to_its_own_group() {
    let (dir, sup) = supervisor();
    let marker_a = format!("faktor-guardian-scope-a-{}", std::process::id());
    let marker_b = format!("faktor-guardian-scope-b-{}", std::process::id());
    let (a, a_descendant, a_guardian) = spawn_marked_wait_tree(&sup, dir.path(), &marker_a);
    let (b, b_descendant, _b_guardian) = spawn_marked_wait_tree(&sup, dir.path(), &marker_b);
    assert!(sup.release_guardian_for_tests(a.id));
    wait_until("scoped kill of A", Duration::from_secs(10), || {
        group_gone(a.pid)
            && pid_is_gone(a_descendant)
            && pid_is_gone(a_guardian)
            && marker_processes(&marker_a).is_empty()
    });
    assert!(
        !pid_is_gone(b.pid) && !pid_is_gone(b_descendant),
        "killing A's group must never touch B's tree"
    );
    assert!(
        marker_processes(&marker_b).contains(&b.pid),
        "B's marker must survive A's guardian"
    );
    sup.kill(b.id, 500).expect("cleanup B");
    let _ = sup.reap();
}

/// ADVERSARIAL (d): a rapid spawn/kill storm leaves no zombie (every child
/// pid is reaped), no live guardian, and no marker process.
#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn rapid_spawn_kill_storm_leaves_no_zombies_or_guardians() {
    let (_dir, sup) = supervisor();
    let marker = format!("faktor-guardian-storm-{}", std::process::id());
    let mut guardians = Vec::new();
    for round in 0..16u32 {
        let handle = sup
            .spawn(sh(&format!("sleep 300 # {marker}")))
            .unwrap_or_else(|e| panic!("round {round}: spawn: {e}"));
        let guardian = sup
            .guardian_pid_for_tests(handle.id)
            .unwrap_or_else(|| panic!("round {round}: no guardian"));
        guardians.push(guardian);
        sup.kill(handle.id, 0)
            .unwrap_or_else(|e| panic!("round {round}: kill: {e}"));
        wait_until(
            &format!("round {round}: row collectible"),
            Duration::from_secs(5),
            || sup.reap().iter().any(|r| r.id == handle.id),
        );
        wait_until(
            &format!("round {round}: child pid extinct (no zombie)"),
            Duration::from_secs(5),
            || pid_is_gone(handle.pid),
        );
    }
    for guardian in guardians {
        wait_until("storm guardian extinction", Duration::from_secs(10), || {
            pid_is_gone(guardian)
        });
    }
    assert_eq!(sup.registered(), 0, "registry fully drained");
    wait_until("no marker processes", Duration::from_secs(10), || {
        marker_processes(&marker).is_empty()
    });
}

// ---- end-to-end daemon SIGKILL: a re-exec'd helper is the daemon -------

#[cfg(any(target_os = "linux", target_os = "macos"))]
const SIM_REPORT_ENV: &str = "FAKTOR_TERMINAL_SIM_REPORT_FILE";
#[cfg(any(target_os = "linux", target_os = "macos"))]
const SIM_MARKER_ENV: &str = "FAKTOR_TERMINAL_SIM_MARKER";
#[cfg(any(target_os = "linux", target_os = "macos"))]
const SIM_LEADER_EXITS_ENV: &str = "FAKTOR_TERMINAL_SIM_LEADER_EXITS";
#[cfg(any(target_os = "linux", target_os = "macos"))]
const SIM_DIR_ENV: &str = "FAKTOR_TERMINAL_SIM_DIR";
#[cfg(any(target_os = "linux", target_os = "macos"))]
const SIM_OWNER_ENV: &str = "FAKTOR_TERMINAL_SIM_OWNER";

/// `pgrep -af <marker>` output: the live-tree evidence a test can print
/// under `--nocapture` and the exact probe the assertions use.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn pgrep_dump(marker: &str) -> String {
    let out = std::process::Command::new("pgrep")
        .args(["-af", "--", marker])
        .output()
        .expect("pgrep");
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

/// The simulated daemon, re-exec'd by [`daemon_sigkill_kills_the_supervised_tree`]
/// through `--exact`. Without the report env it is an inert pass in the
/// ordinary suite; with it, it supervises a marked `sh` tree, reports the
/// pids, and holds the guardian control pipe until the parent SIGKILLs it.
#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn simulated_terminal_daemon_helper() {
    let Ok(report) = std::env::var(SIM_REPORT_ENV) else {
        return;
    };
    let marker = std::env::var(SIM_MARKER_ENV).expect("sim marker");
    // The PARENT owns the directory (the helper is SIGKILLed, so its own
    // TempDir Drop would never run): scratch files must not leak.
    let dir = std::path::PathBuf::from(std::env::var(SIM_DIR_ENV).expect("sim dir"));
    let cas = Arc::new(faktor_cas::Cas::open(dir.join("cas")).unwrap());
    let sup = ProcessSupervisor::new(cas);
    let pidfile = dir.join("descendant.pid");
    // The background leaf carries the marker IN ITS OWN ARGV (`tail -f
    // <marker-named file>`), so `pgrep -f <marker>` proves the descendant
    // itself died, not only the shell that named it.
    let keep = dir.join(format!("{marker}.keep"));
    std::fs::write(&keep, b"keep").expect("marker keep file");
    // Default shape: the shell waits on the background descendant. With
    // `SIM_LEADER_EXITS` the shell exits immediately, so the descendant
    // survives AFTER the direct child was already reaped — the post-reap
    // gap only the guardian can close.
    let tail = if std::env::var(SIM_LEADER_EXITS_ENV).as_deref() == Ok("1") {
        "true"
    } else {
        "wait"
    };
    let script = format!(
        "tail -f '{}' & echo $! > '{}'; {tail} # {marker}",
        keep.display(),
        pidfile.display()
    );
    let mut cfg = sh(&script);
    let browser_owned = std::env::var(SIM_OWNER_ENV).as_deref() == Ok("browser");
    if browser_owned {
        cfg.owner = ProcessOwner::Browser {
            source: "sim-source".into(),
            profile: "sim-profile".into(),
        };
    }
    // Browser children are launched through the DETACHED pipes entry point
    // (chromium stdin/stdout/stderr), not the capture-oriented `spawn`; both
    // entry points must own the same guardian.
    let (_detached, pid, id) = if browser_owned {
        let spawned = sup
            .spawn_detached_with_pipes(cfg)
            .expect("sim detached spawn");
        let pid = spawned.child_pid;
        let id = sup
            .alive()
            .into_iter()
            .find(|handle| handle.pid == pid)
            .expect("detached row registered")
            .id;
        (Some(spawned), pid, id)
    } else {
        let handle = sup.spawn(cfg).expect("sim spawn");
        (None, handle.pid, handle.id)
    };
    let descendant = read_pid_file(&pidfile);
    if tail == "true" {
        // Prove the leader was consumed by its reaper before the daemon
        // dies: the guardian's Gone-but-group-alive path is what must act.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !sup.reap_state(id).map(|reap| reap.reaped()).unwrap_or(true) {
            assert!(
                std::time::Instant::now() < deadline,
                "leader must be reaped"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let guardian = sup
        .guardian_pid_for_tests(id)
        .expect("guardian forked for the sim child");
    std::fs::write(&report, format!("{pid} {guardian} {descendant} {marker}\n"))
        .expect("write sim report");
    // Hold the control pipe until the parent SIGKILLs this process: only
    // process death closes the last write end (no Drop, no release).
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

/// Discover the libtest name of the simulated daemon helper (the module
/// path is an implementation detail; `--list` is the authority).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn simulated_daemon_helper_name() -> String {
    let out = std::process::Command::new(std::env::current_exe().expect("current exe"))
        .arg("--list")
        .output()
        .expect("test binary --list");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|line| {
            let name = line.strip_suffix(": test")?.trim();
            name.ends_with("simulated_terminal_daemon_helper")
                .then(|| name.to_string())
        })
        .expect("helper test must be present in --list")
}

/// SIGKILL the re-exec'd daemon, then assert the whole supervised tree and
/// the marker are gone within the bound.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_daemon_sigkill_case(marker: &str, leader_exits: bool, browser_owned: bool) {
    let dir = tempdir().unwrap();
    let report = dir.path().join("sim.report");
    let sim_dir = dir.path().join("sim");
    std::fs::create_dir(&sim_dir).unwrap();
    let mut daemon = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("--nocapture")
        .arg(simulated_daemon_helper_name())
        .env(SIM_REPORT_ENV, &report)
        .env(SIM_MARKER_ENV, marker)
        .env(SIM_LEADER_EXITS_ENV, if leader_exits { "1" } else { "0" })
        .env(SIM_DIR_ENV, &sim_dir)
        .env(
            SIM_OWNER_ENV,
            if browser_owned { "browser" } else { "daemon" },
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("re-exec the simulated daemon");
    wait_until("simulated daemon report", Duration::from_secs(30), || {
        std::fs::read_to_string(&report)
            .map(|text| text.split_whitespace().count() == 4)
            .unwrap_or(false)
    });
    let text = std::fs::read_to_string(&report).unwrap();
    let fields: Vec<&str> = text.split_whitespace().collect();
    let leader: u32 = fields[0].parse().unwrap();
    let guardian: u32 = fields[1].parse().unwrap();
    let descendant: u32 = fields[2].parse().unwrap();
    assert_eq!(fields[3], marker);
    // Proof taken BEFORE the crash: the daemon-held write end and the
    // descendant are signallable right now.
    if leader_exits {
        assert!(
            pid_is_gone(leader),
            "the direct child was already consumed before the daemon SIGKILL"
        );
    } else {
        assert!(!pid_is_gone(leader), "leader must be alive at crash time");
    }
    assert!(
        !pid_is_gone(descendant),
        "descendant must be alive at crash time"
    );
    assert!(
        !pid_is_gone(guardian),
        "guardian must be alive at crash time"
    );
    eprintln!("[pgrep before daemon SIGKILL] {}", pgrep_dump(marker));
    // Simulated daemon SIGKILL: no Drop, no deliberate release — kernel
    // death closes the write end and arms the child's PDEATHSIG.
    daemon.kill().expect("SIGKILL the simulated daemon");
    let _ = daemon.wait();
    wait_until(
        "the whole supervised tree to die after the daemon SIGKILL",
        Duration::from_secs(10),
        || {
            pid_is_gone(leader)
                && pid_is_gone(descendant)
                && pid_is_gone(guardian)
                && pgrep_dump(marker).is_empty()
        },
    );
    eprintln!(
        "[pgrep after daemon SIGKILL] {:?} (empty = no orphan)",
        pgrep_dump(marker)
    );
}

/// THE end-to-end fault test: the daemon process is SIGKILLed while a
/// supervised `sh` waits on a background `sleep`. The direct child dies via
/// PDEATHSIG; the guardian — seeing the pipe EOF that SIGKILL produces —
/// must take the descendant, and both the guardian and the marker must
/// vanish within the bound.
#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn daemon_sigkill_kills_the_supervised_tree() {
    let marker = format!("faktor-terminal-sim-daemon-{}", std::process::id());
    run_daemon_sigkill_case(&marker, false, false);
}

/// THE exact observed fault (leader already consumed): `sh -c
/// 'sleep 554 &'` exits, its background `sleep` survives the reap, then the
/// daemon is SIGKILLed. Only the guardian's verified Gone-but-group-alive
/// kill can end the descendant.
#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn daemon_sigkill_kills_descendants_of_an_exited_supervisor_leader() {
    let marker = format!("faktor-terminal-sim-reaped-{}", std::process::id());
    run_daemon_sigkill_case(&marker, true, false);
}

/// A BROWSER-owned long-lived child (the supervisor's Chromium shape) must be
/// reaped by the same guardian on daemon SIGKILL: the browser path is not a
/// second, unguarded process owner.
#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn daemon_sigkill_reaps_browser_owned_child() {
    let marker = format!("faktor-terminal-sim-browser-{}", std::process::id());
    run_daemon_sigkill_case(&marker, false, true);
}

/// Normal FORCE-exit shutdown: `kill_all_browsers` reaps exactly the
/// browser-owned tree through its registry row; a sibling session tree is
/// untouched, and a second call is a harmless no-op (no double kill).
#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn kill_all_browsers_is_scoped_and_idempotent() {
    let (dir, sup) = supervisor();
    let browser_marker = format!("faktor-browser-reap-b-{}", std::process::id());
    let session_marker = format!("faktor-browser-reap-s-{}", std::process::id());
    let mut browser_cfg = sh(&format!(
        "sleep 300 & echo $! > \"{}\"; wait # {browser_marker}",
        dir.path().join("browser.pid").display()
    ));
    browser_cfg.owner = ProcessOwner::Browser {
        source: "src".into(),
        profile: "profile".into(),
    };
    let browser = sup
        .spawn_detached_with_pipes(browser_cfg)
        .expect("browser tree");
    let browser_pid = browser.child_pid;
    let browser_id = sup
        .alive()
        .into_iter()
        .find(|handle| handle.pid == browser_pid)
        .expect("browser row registered")
        .id;
    let browser_descendant = read_pid_file(&dir.path().join("browser.pid"));
    let session = sup
        .spawn(sh(&format!("sleep 300 & wait # {session_marker}")))
        .expect("session tree");
    assert_eq!(sup.kill_all_browsers(), vec![browser_id]);
    wait_until("browser tree reaped", Duration::from_secs(10), || {
        group_gone(browser_pid)
            && pid_is_gone(browser_descendant)
            && marker_processes(&browser_marker).is_empty()
    });
    assert!(
        !pid_is_gone(session.pid),
        "the session-owned tree must survive a browser-only reap"
    );
    assert!(
        marker_processes(&session_marker).contains(&session.pid),
        "the session marker must still be alive"
    );
    assert!(
        sup.kill_all_browsers().is_empty(),
        "a second FORCE-exit reap must not double-kill or invent rows"
    );
    sup.kill(session.id, 500).expect("session cleanup");
    let _ = sup.reap();
}
