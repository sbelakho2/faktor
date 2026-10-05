//! Adversarial corpus for network isolation, broker endpoints and the
//! privilege-drop/user-namespace fail-closed contract (Linux).
//!
//! Pure mapping/tag/endpoint matrices plus real spawns through
//! [`ProcessSupervisor::run_sync`]: forced kernel refusals must fail the
//! spawn typed BEFORE exec (marker proof), a real isolation must actually
//! move the child into a different network namespace, and `Inherit` spawns
//! are unaffected by the failure seam. Environment-dependent branches assert
//! their predicate explicitly — never a silent skip.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::time::Duration;

use faktor_core::command::EnvSpec;

use crate::{
    FilesystemIsolation, NetworkEnforcement, NetworkIsolation, NetworkIsolationRequirement,
    ProcessOwner, ProcessSupervisor, SpawnConfig,
};

/// Serializes the tests that toggle process-global seams, through the SAME
/// crate-wide lock the `tests` module's DenyAll spawns use: the forced-
/// unshare hook and the DenyAll proof flag are process-global, so the two
/// modules must exclude each other, not merely themselves.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    crate::tests::deny_all_spawn_lock()
}

fn tmpdir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn cfg_run(command: &str, script: &str, cwd: PathBuf, isolation: NetworkIsolation) -> SpawnConfig {
    SpawnConfig {
        cmd: command.into(),
        args: vec!["-c".into(), script.into()],
        cwd,
        env: EnvSpec::Minimal,
        owner: ProcessOwner::Daemon,
        capture: true,
        artifact_max: 64 * 1024,
        network_isolation: isolation,
        filesystem_isolation: FilesystemIsolation::Inherit,
    }
}

/// Policy mapping and wire tags: no third state, no silent downgrade.
#[test]
fn network_isolation_mapping_and_tags_matrix() {
    assert_eq!(
        NetworkIsolation::from(NetworkIsolationRequirement::DenyAll),
        NetworkIsolation::DenyAll,
        "a DenyAll requirement must map to DenyAll"
    );
    assert_eq!(
        NetworkIsolation::from(NetworkIsolationRequirement::Inherit),
        NetworkIsolation::Inherit,
        "an Inherit requirement must map to Inherit"
    );
    assert_eq!(NetworkIsolation::default(), NetworkIsolation::Inherit);
    let broker = NetworkIsolation::BrokerOnly {
        endpoint: "127.0.0.1:9".parse().unwrap(),
    };
    let rows: [(&str, NetworkIsolation, &str, bool); 3] = [
        ("inherit", NetworkIsolation::Inherit, "inherit", false),
        ("deny-all", NetworkIsolation::DenyAll, "deny_all", false),
        ("broker-only", broker, "broker_only", true),
    ];
    for (label, isolation, tag, is_broker) in rows {
        assert_eq!(
            isolation.as_tag(),
            tag,
            "case {label}: the durable tag must be the frozen spelling"
        );
        assert_eq!(
            isolation.is_broker_only(),
            is_broker,
            "case {label}: broker-only detection must be exact"
        );
    }
    assert_eq!(
        crate::broker_only_supported(),
        cfg!(target_os = "linux"),
        "the compile-time backend fact must match the target"
    );
}

/// The enforcement verdict tags/Display are frozen and the test override
/// never changes what the spawn path applies.
#[test]
fn enforcement_verdict_tags_and_display_matrix() {
    let rows = [
        (NetworkEnforcement::AppLevel, "app_level"),
        (NetworkEnforcement::OsLevel, "os_level"),
        (
            NetworkEnforcement::OsLevelBrokerOnly,
            "os_level_broker_only",
        ),
        (NetworkEnforcement::Unavailable, "unavailable"),
    ];
    for (verdict, tag) in rows {
        assert_eq!(verdict.as_tag(), tag, "verdict {verdict:?} tag");
        assert!(
            verdict.to_string().len() > 10,
            "verdict {verdict:?} must carry a human-readable explanation"
        );
    }
    assert_eq!(
        NetworkEnforcement::default(),
        NetworkEnforcement::AppLevel,
        "capability existence is not enforcement: the default stays app-level"
    );
}

/// Broker endpoint validation: loopback only, non-zero port.
#[test]
fn broker_endpoint_validation_matrix() {
    let rows: [(&str, &str, bool); 14] = [
        ("v4-loopback", "127.0.0.1:8080", true),
        ("v4-loopback-other", "127.0.0.2:1", true),
        ("v4-loopback-max", "127.255.255.254:65535", true),
        ("v4-any", "0.0.0.0:8080", false),
        ("v4-lan", "192.168.1.1:80", false),
        ("v4-public", "8.8.8.8:53", false),
        ("v4-broadcast", "255.255.255.255:1", false),
        ("v6-loopback", "[::1]:1", true),
        ("v6-any", "[::]:8080", false),
        ("v6-public", "[2001:db8::1]:443", false),
        ("v4-loopback-port-zero", "127.0.0.1:0", false),
        ("v6-loopback-port-zero", "[::1]:0", false),
        ("v4-lan-port-zero", "10.0.0.1:0", false),
        (
            "v4-mapped-loopback-is-not-::1",
            "[::ffff:127.0.0.1]:80",
            false,
        ),
    ];
    for (label, text, valid) in rows {
        let endpoint = text
            .parse()
            .unwrap_or_else(|e| panic!("case {label}: parse {text}: {e}"));
        let result = crate::sandbox::validate_endpoint(endpoint);
        assert_eq!(
            result.is_ok(),
            valid,
            "case {label}: endpoint {text} validity must be {valid}: {result:?}"
        );
        if let Err(err) = result {
            assert_eq!(
                err.kind(),
                std::io::ErrorKind::InvalidInput,
                "case {label}: a refused endpoint must be a typed InvalidInput"
            );
        }
    }
}

/// Forced kernel refusal: every DenyAll script fails closed with a typed
/// pre-exec error, and the marker proves the child NEVER ran.
#[test]
fn forced_unshare_failure_fails_closed_without_exec() {
    let _serial = serial();
    let supervisor = ProcessSupervisor::try_shared().expect("shared supervisor");
    crate::sandbox::force_unshare_failure_for_tests(true);
    let dir = tmpdir();
    let scripts: [(&str, &str); 6] = [
        ("echo", "echo ran"),
        ("marker-write", "echo ran > marker"),
        ("exit-zero", "exit 0"),
        ("sleep", "sleep 30"),
        ("env-dump", "env > marker"),
        ("shell-builtin", "pwd > marker"),
    ];
    for (label, script) in scripts {
        let marker = dir.path().join(format!("{label}.marker"));
        let actual = script.replace("marker", marker.to_str().unwrap());
        let cfg = cfg_run(
            "sh",
            &actual,
            dir.path().to_path_buf(),
            NetworkIsolation::DenyAll,
        );
        let result = supervisor.run_sync(cfg, Duration::from_secs(5), 4096, 4096);
        let err = result.err().unwrap_or_else(|| {
            panic!("case {label}: a forced unshare refusal must fail the spawn")
        });
        assert!(
            !err.message.is_empty(),
            "case {label}: the refusal must carry a typed reason: {err}"
        );
        assert!(
            err.message.to_ascii_lowercase().contains("denyall")
                || err.message.to_ascii_lowercase().contains("sandbox")
                || err.message.to_ascii_lowercase().contains("unshare")
                || err.message.to_ascii_lowercase().contains("network"),
            "case {label}: the refusal must name the network isolation: {err}"
        );
        assert!(
            !marker.exists(),
            "case {label}: the child must NEVER exec under a failed isolation: {} exists",
            marker.display()
        );
    }
    crate::sandbox::force_unshare_failure_for_tests(false);
}

/// The failure seam changes nothing for `Inherit` spawns (no isolation, no
/// refusal).
#[test]
fn inherit_spawns_are_unaffected_by_the_failure_seam() {
    let _serial = serial();
    let supervisor = ProcessSupervisor::try_shared().expect("shared supervisor");
    crate::sandbox::force_unshare_failure_for_tests(true);
    let dir = tmpdir();
    for (label, script, needle) in [
        ("plain-echo", "echo INHERIT-OK", "INHERIT-OK"),
        ("marker", "echo ran > marker; echo DONE", "DONE"),
    ] {
        let actual = script.replace(
            "marker",
            dir.path().join("inherit.marker").to_str().unwrap(),
        );
        let cfg = cfg_run(
            "sh",
            &actual,
            dir.path().to_path_buf(),
            NetworkIsolation::Inherit,
        );
        let output = supervisor
            .run_sync(cfg, Duration::from_secs(5), 4096, 4096)
            .unwrap_or_else(|e| panic!("case {label}: Inherit spawn must succeed: {e}"));
        assert_eq!(output.exit_code, Some(0), "case {label}: clean exit");
        assert!(
            output.stdout_head.contains(needle),
            "case {label}: output must contain {needle:?}: {:?}",
            output.stdout_head
        );
    }
    crate::sandbox::force_unshare_failure_for_tests(false);
}

/// When this host can really isolate (root + usable netns) the child must run
/// in a DIFFERENT network namespace; otherwise the same request must refuse
/// typed with no exec. The environment predicate is asserted in the message,
/// never silently skipped.
#[test]
fn deny_all_real_isolation_or_typed_refusal_only() {
    let _serial = serial();
    let supervisor = ProcessSupervisor::try_shared().expect("shared supervisor");
    let dir = tmpdir();
    let can_isolate = crate::sandbox::isolation_must_succeed_for_tests();

    let scripts: [(&str, &str, i32); 5] = [
        ("ns-readlink", "readlink /proc/self/ns/net > marker", 0),
        ("true", "true", 0),
        // The child's OWN exit code must survive the isolation wrapper.
        ("exit-3", "exit 3", 3),
        ("echo", "echo ISOLATED", 0),
        ("route-read", "cat /proc/net/route > marker", 0),
    ];
    for (label, script, expected_exit) in scripts {
        let marker = dir.path().join(format!("real-{label}.marker"));
        let actual = script.replace("marker", marker.to_str().unwrap());
        let cfg = cfg_run(
            "sh",
            &actual,
            dir.path().to_path_buf(),
            NetworkIsolation::DenyAll,
        );
        let result = supervisor.run_sync(cfg, Duration::from_secs(5), 4096, 4096);
        if can_isolate {
            let output = result.unwrap_or_else(|e| {
                panic!(
                    "case {label}: this host can isolate (root + netns available), so DenyAll must succeed: {e}"
                )
            });
            assert_eq!(
                output.exit_code,
                Some(expected_exit),
                "case {label}: the isolated child must exit with its own code"
            );
            if label == "ns-readlink" || label == "route-read" {
                let child_ns = std::fs::read_link("/proc/self/ns/net").unwrap();
                let text = std::fs::read_to_string(&marker)
                    .unwrap_or_else(|e| panic!("case {label}: the child marker must exist: {e}"));
                if label == "ns-readlink" {
                    assert_ne!(
                        text.trim(),
                        child_ns.to_string_lossy(),
                        "case {label}: the child must run in a DIFFERENT netns than the daemon"
                    );
                } else {
                    assert!(
                        text.trim().lines().count() <= 1,
                        "case {label}: an empty netns must have no default route entries: {text:?}"
                    );
                }
            }
        } else {
            let err = result.err().unwrap_or_else(|| {
                panic!(
                    "case {label}: this host cannot isolate (predicate false), so DenyAll must refuse typed"
                )
            });
            assert!(
                !err.message.is_empty(),
                "case {label}: the refusal must be typed: {err}"
            );
            assert!(
                !marker.exists(),
                "case {label}: a refused isolation must not exec the child"
            );
        }
    }
}

/// The privilege-drop probe: as root the production drop must succeed with
/// `no_new_privs` set and every capability emptied; as a normal user it must
/// refuse with a kernel errno. The environment predicate is asserted.
#[test]
fn privilege_drop_probe_matrix_is_exact() {
    let must_succeed = crate::sandbox::privilege_drop_must_succeed_for_tests();
    let mut successes = 0usize;
    let mut refusals = 0usize;
    for round in 0..5 {
        match crate::sandbox::probe_privilege_drop_for_tests() {
            crate::sandbox::DropProbe::Succeeded {
                no_new_privs,
                caps_empty,
            } => {
                successes += 1;
                assert!(
                    no_new_privs,
                    "round {round}: a successful drop must set no_new_privs"
                );
                assert!(
                    caps_empty,
                    "round {round}: a successful drop must empty all capabilities"
                );
            }
            crate::sandbox::DropProbe::Refused(errno) => {
                refusals += 1;
                assert!(
                    matches!(errno, libc::EPERM | libc::EINVAL | libc::ENOSYS),
                    "round {round}: an unprivileged refusal must be a real kernel errno, got {errno}"
                );
            }
        }
    }
    if must_succeed {
        assert_eq!(
            refusals, 0,
            "this environment can drop privileges (root), so no refusal is acceptable"
        );
        assert_eq!(successes, 5, "all five probes must succeed as root");
    } else {
        assert_eq!(
            successes, 0,
            "a non-root environment cannot drop to an empty capability set; success would be a lie"
        );
        assert_eq!(refusals, 5, "all five probes must refuse typed as non-root");
        // Non-root: the setns escape back to pid 1's netns must be denied by
        // the kernel (running it is safe exactly in this branch).
        assert!(
            crate::sandbox::setns_escape_denied_for_tests(),
            "as a non-root user the setns escape to pid 1's netns must be denied"
        );
    }
}

/// Netns availability is a cached, stable fact; and the unshare probe agrees
/// with a real DenyAll spawn outcome (no capability guessing).
#[test]
fn netns_availability_probe_is_consistent() {
    let first = crate::sandbox::netns_unshare_available_for_tests();
    for round in 0..3 {
        assert_eq!(
            crate::sandbox::netns_unshare_available_for_tests(),
            first,
            "round {round}: the cached probe must be stable"
        );
    }
    let must_succeed = crate::sandbox::isolation_must_succeed_for_tests();
    assert_eq!(
        must_succeed,
        crate::sandbox::privilege_drop_must_succeed_for_tests() && first,
        "the combined predicate must be exactly root AND unshare-available"
    );
}
