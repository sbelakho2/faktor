//! Adversarial sandbox policy / gate tests (task category 5, sandbox leg).
//!
//! Production entry points: [`SandboxPolicy::validate`], [`NetworkGate`],
//! [`PermissionEngine::check_egress`], [`PermissionEngine::evaluate`],
//! [`SandboxPolicy::spawn_profile`] and the spawn-requirement mapping.

use super::*;
use faktor_core::command::NetworkIsolationRequirement;
use faktor_security::destination::{DestinationPolicy, RequestTarget};

fn engine_with(entries: &[&str]) -> PermissionEngine {
    let gate = NetworkGate::parse(entries.iter().copied()).expect("fixture gate parses");
    PermissionEngine::new(
        SandboxPolicy {
            network: gate,
            ..SandboxPolicy::default()
        },
        None,
    )
}

#[test]
fn policy_validation_pairs_shell_mode_with_guarantee() {
    let cases: Vec<(ShellExecutionMode, SandboxGuarantee, bool)> = vec![
        (
            ShellExecutionMode::OsIsolated,
            SandboxGuarantee::Required,
            true,
        ),
        (
            ShellExecutionMode::OsIsolated,
            SandboxGuarantee::BestEffort,
            false,
        ),
        (
            ShellExecutionMode::OsIsolated,
            SandboxGuarantee::None,
            false,
        ),
        (
            ShellExecutionMode::NetworkCapableUserGranted,
            SandboxGuarantee::BestEffort,
            true,
        ),
        (
            ShellExecutionMode::NetworkCapableUserGranted,
            SandboxGuarantee::None,
            true,
        ),
        (
            ShellExecutionMode::NetworkCapableUserGranted,
            SandboxGuarantee::Required,
            false,
        ),
    ];
    for (mode, guarantee, ok) in cases {
        let policy = SandboxPolicy {
            shell_execution: mode,
            network_guarantee: guarantee,
            ..SandboxPolicy::default()
        };
        assert_eq!(
            policy.validate().is_ok(),
            ok,
            "validate({mode:?}, {guarantee:?})"
        );
        if !ok {
            let err = policy.validate().unwrap_err();
            assert!(
                !err.is_empty(),
                "refusal for ({mode:?}, {guarantee:?}) names the conflict"
            );
        }
    }
    // The shipped default is the fail-closed pair.
    let default = SandboxPolicy::default();
    assert!(default.validate().is_ok(), "default policy is valid");
    assert_eq!(
        default.shell_execution,
        ShellExecutionMode::OsIsolated,
        "default shell mode is fail-closed"
    );
    assert_eq!(
        default.network_guarantee,
        SandboxGuarantee::Required,
        "default guarantee is Required"
    );
    assert_eq!(
        default.spawn_profile().shell,
        "os_isolated",
        "spawn profile carries the shell contract tag"
    );
    assert_eq!(
        default.spawn_profile().network,
        "required",
        "spawn profile carries the guarantee tag"
    );
    assert_eq!(
        default.spawn_profile().filesystem,
        "workspace+external:ask-ask",
        "spawn profile carries the external rule tags"
    );
    assert_eq!(
        ShellExecutionMode::OsIsolated.as_tag(),
        "os_isolated",
        "stable mode tag"
    );
    assert!(
        SandboxPolicy {
            shell_execution: ShellExecutionMode::NetworkCapableUserGranted,
            network_guarantee: SandboxGuarantee::None,
            ..SandboxPolicy::default()
        }
        .shell_execution_state()
        .strength_label()
        .contains("GRANTED BY USER"),
        "the honest strength label is surfaced"
    );
    assert!(
        NETWORK_ISOLATION_NOTE.contains("app-level only"),
        "the BestEffort limitation note is frozen"
    );
}

#[test]
fn spawn_requirement_mapping_is_exact() {
    let required = SandboxPolicy {
        network_guarantee: SandboxGuarantee::Required,
        ..SandboxPolicy::default()
    };
    let best_effort = SandboxPolicy {
        shell_execution: ShellExecutionMode::NetworkCapableUserGranted,
        network_guarantee: SandboxGuarantee::BestEffort,
        ..SandboxPolicy::default()
    };
    let none = SandboxPolicy {
        shell_execution: ShellExecutionMode::NetworkCapableUserGranted,
        network_guarantee: SandboxGuarantee::None,
        ..SandboxPolicy::default()
    };
    assert_eq!(
        PermissionEngine::new(required, None).spawn_network_requirement(),
        NetworkIsolationRequirement::DenyAll,
        "Required maps to DenyAll"
    );
    assert_eq!(
        PermissionEngine::new(best_effort, None).spawn_network_requirement(),
        NetworkIsolationRequirement::Inherit,
        "BestEffort maps to Inherit"
    );
    assert_eq!(
        PermissionEngine::new(none, None).spawn_network_requirement(),
        NetworkIsolationRequirement::Inherit,
        "None maps to Inherit"
    );
    assert_eq!(
        SandboxGuarantee::Required.network_requirement(),
        NetworkIsolationRequirement::DenyAll,
        "guarantee-level mapping"
    );
    assert_eq!(
        NetworkIsolationRequirement::from(SandboxGuarantee::BestEffort),
        NetworkIsolationRequirement::Inherit,
        "From conversion"
    );
}

// ------------- P1 filesystem-confinement authority (policy -> spawn) -----

/// The filesystem projection is enforcement-honest: it never says
/// `workspace` on a build without an OS confinement backend, it qualifies
/// BestEffort as non-guaranteed, and it names the external rules whenever
/// the jail is not claimed.
#[test]
fn filesystem_projection_is_enforcement_honest() {
    let locked = |guarantee| SandboxPolicy {
        read_external: Rule::Deny,
        write_external: Rule::Deny,
        filesystem_guarantee: guarantee,
        ..SandboxPolicy::default()
    };
    let backend = filesystem_backend_available();
    assert_eq!(
        backend,
        cfg!(target_os = "linux"),
        "backend is the build fact"
    );
    // Required: a guaranteed jail where the backend exists, an honest
    // application-policy-only tag (and a typed spawn refusal) elsewhere.
    assert_eq!(
        locked(FilesystemGuarantee::Required)
            .spawn_profile()
            .filesystem,
        if backend {
            "workspace"
        } else {
            "application-policy-only"
        },
        "Required must project the jail only where it can be enforced"
    );
    // BestEffort never claims a guaranteed jail.
    assert_eq!(
        locked(FilesystemGuarantee::BestEffort)
            .spawn_profile()
            .filesystem,
        if backend {
            "workspace_best_effort"
        } else {
            "application-policy-only"
        },
        "BestEffort must stay honest about the fallback"
    );
    // None claims no OS confinement at all.
    assert_eq!(
        locked(FilesystemGuarantee::None).spawn_profile().filesystem,
        "application-policy-only",
        "None must never claim a workspace jail"
    );
    // A non-jail policy names its external rules and claims nothing else.
    for (read, write, tag) in [
        (Rule::Ask, Rule::Ask, "workspace+external:ask-ask"),
        (Rule::Allow, Rule::Deny, "workspace+external:allow-deny"),
        (Rule::Deny, Rule::Allow, "workspace+external:deny-allow"),
    ] {
        let policy = SandboxPolicy {
            read_external: read,
            write_external: write,
            filesystem_guarantee: FilesystemGuarantee::Required,
            ..SandboxPolicy::default()
        };
        assert_eq!(policy.spawn_profile().filesystem, tag, "{read:?}/{write:?}");
    }
    // Determinism and serde round-trip of the projection (it is evidence).
    let policy = locked(FilesystemGuarantee::Required);
    assert_eq!(policy.spawn_profile(), policy.spawn_profile());
    let back: SpawnProfile =
        serde_json::from_value(serde_json::to_value(policy.spawn_profile()).unwrap()).unwrap();
    assert_eq!(back, policy.spawn_profile());
}

/// The guarantee -> spawn-requirement mapping is exact, and the demand is
/// workspace-shaped only where the policy claims the jail.
#[test]
fn filesystem_requirement_mapping_is_exact() {
    assert_eq!(
        FilesystemGuarantee::Required.filesystem_requirement(),
        FilesystemIsolationRequirement::Workspace { best_effort: false },
        "Required maps to a fail-closed workspace demand"
    );
    assert_eq!(
        FilesystemIsolationRequirement::from(FilesystemGuarantee::BestEffort),
        FilesystemIsolationRequirement::Workspace { best_effort: true },
        "BestEffort maps to an install-if-available workspace demand"
    );
    assert_eq!(
        FilesystemGuarantee::None.filesystem_requirement(),
        FilesystemIsolationRequirement::Inherit,
        "None maps to Inherit"
    );
    let locked = |guarantee| {
        PermissionEngine::new(
            SandboxPolicy {
                read_external: Rule::Deny,
                write_external: Rule::Deny,
                filesystem_guarantee: guarantee,
                ..SandboxPolicy::default()
            },
            None,
        )
        .spawn_filesystem_requirement()
    };
    assert_eq!(
        locked(FilesystemGuarantee::Required),
        FilesystemIsolationRequirement::Workspace { best_effort: false }
    );
    assert_eq!(
        locked(FilesystemGuarantee::BestEffort),
        FilesystemIsolationRequirement::Workspace { best_effort: true }
    );
    assert_eq!(
        locked(FilesystemGuarantee::None),
        FilesystemIsolationRequirement::Inherit,
        "None never demands confinement"
    );
    // Non-jail policies demand nothing even under a Required guarantee:
    // the profile names the external rules instead.
    for (read, write) in [(Rule::Ask, Rule::Ask), (Rule::Allow, Rule::Deny)] {
        let engine = PermissionEngine::new(
            SandboxPolicy {
                read_external: read,
                write_external: write,
                filesystem_guarantee: FilesystemGuarantee::Required,
                ..SandboxPolicy::default()
            },
            None,
        );
        assert_eq!(
            engine.spawn_filesystem_requirement(),
            FilesystemIsolationRequirement::Inherit,
            "{read:?}/{write:?}: no jail claim, no confinement demand"
        );
    }
}

/// The new guarantee field is additive on the config surface: absent means
/// Required (secure default) and the policy still round-trips.
#[test]
fn filesystem_guarantee_serde_defaults_to_required() {
    let policy = SandboxPolicy::default();
    assert_eq!(
        policy.filesystem_guarantee,
        FilesystemGuarantee::Required,
        "the secure default is Required"
    );
    let mut value = serde_json::to_value(policy).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .remove("filesystem_guarantee");
    let back: SandboxPolicy = serde_json::from_value(value).unwrap();
    assert_eq!(back.filesystem_guarantee, FilesystemGuarantee::Required);
    assert_eq!(
        serde_json::to_value(FilesystemGuarantee::BestEffort).unwrap(),
        serde_json::json!("best_effort")
    );
    assert_eq!(
        serde_json::to_value(FilesystemGuarantee::None).unwrap(),
        serde_json::json!("none")
    );
    let round = SandboxPolicy {
        filesystem_guarantee: FilesystemGuarantee::BestEffort,
        ..SandboxPolicy::default()
    };
    let back: SandboxPolicy =
        serde_json::from_value(serde_json::to_value(&round).unwrap()).unwrap();
    assert_eq!(back, round);
}

#[test]
fn default_gate_allows_only_the_frozen_provider_endpoints() {
    let engine = PermissionEngine::new(SandboxPolicy::default(), None);
    let allowed = [
        "https://api.openai.com",
        "https://api.openai.com/v1/chat/completions",
        "https://api.anthropic.com",
        "https://generativelanguage.googleapis.com",
        "https://api.deepseek.com",
        "https://API.OPENAI.COM.",
        "https://api.openai.com:8443",
        "https://api.openai.com:65535",
    ];
    for destination in allowed {
        assert!(
            engine.check_egress(destination).is_ok(),
            "default gate must allow {destination:?}"
        );
    }
    let denied = [
        "https://evil.com",
        "https://api.openai.com.evil.com",
        "https://evil-api.openai.com",
        "https://api.openai.comx",
        "http://api.openai.com",
        "http://api.anthropic.com",
        "ws://api.openai.com",
        "https://api.openai.com:0",
        "https://localhost",
        "https://127.0.0.1",
        "https://[::1]",
        "https://169.254.169.254",
        "https://metadata.google.internal",
        "https://api.deepseek.com.evil.test",
        "https://api.openai.com.evil:65535",
        "https://xn--api.openai.com",
        "https://api .openai.com",
    ];
    for destination in denied {
        let result = engine.check_egress(destination);
        assert!(
            result.is_err(),
            "default gate must deny {destination:?}, got {result:?}"
        );
    }
    // A bare host without a scheme is ambiguous: never allowed by a rule.
    for bare in ["api.openai.com", "api.openai.com:443", "127.0.0.1"] {
        assert!(
            engine.check_egress(bare).is_err(),
            "bare {bare:?} must be denied (unknown scheme never matches)"
        );
    }
}

#[test]
fn credentials_and_host_encoding_tricks_never_bypass_a_gate() {
    let engine = engine_with(&["https://allowed.example.com:443"]);
    let unparseable = [
        "https://user:pass@allowed.example.com/",
        "https://user@allowed.example.com/",
        "https://allowed.example.com@evil.com/",
        "https://allowed.example.com%2f@evil.com/",
        "https://allowed.example.com#@evil.com",
        "https://allowed.example.com?@evil.com",
        "https://allowed.example.com\\@evil.com",
        "https://allowed.example.com%00.evil.com",
        "https://allowed.example.com%2eevil.com",
        "https://allowed.example.com\\evil.com",
        "https://allowed.example.com:443@evil.com",
        "https://[allowed.example.com]",
        "https://allowed.example.com.:443x",
    ];
    for destination in unparseable {
        match engine.check_egress(destination) {
            Err(EgressError::Unparseable(_)) => {}
            other => {
                panic!("host/credential trick {destination:?} must be unparseable, got {other:?}")
            }
        }
    }
    // Canonical-equal spellings still allow; lookalikes do not.
    let allowed = [
        "https://allowed.example.com",
        "https://ALLOWED.EXAMPLE.COM",
        "https://allowed.example.com.",
        "https://allowed.example.com:443",
        "https://allowed.example.com/some/path?x=1#frag",
    ];
    for destination in allowed {
        assert!(
            engine.check_egress(destination).is_ok(),
            "canonical spelling {destination:?} must allow"
        );
    }
    let denied = [
        "https://allowed.example.com.evil.com",
        "https://evil-allowed.example.com",
        "https://allowed.example.com.evil",
        "https://allowed.example.com:8443",
        "http://allowed.example.com",
        "https://allowed.example.com:0",
    ];
    for destination in denied {
        assert!(
            engine.check_egress(destination).is_err(),
            "lookalike/port/scheme {destination:?} must deny"
        );
    }
}

#[test]
fn explicit_deny_and_allow_all_gates_have_exact_semantics() {
    let deny = PermissionEngine::new(
        SandboxPolicy {
            network: NetworkGate::deny_all(),
            ..SandboxPolicy::default()
        },
        None,
    );
    for destination in [
        "https://api.openai.com",
        "https://example.com",
        "http://127.0.0.1:8080",
        "https://[::1]",
        "ws://localhost",
    ] {
        assert!(
            deny.check_egress(destination).is_err(),
            "deny_all must refuse {destination:?}"
        );
    }
    let allow = PermissionEngine::new(
        SandboxPolicy {
            network: NetworkGate::allow_all(),
            ..SandboxPolicy::default()
        },
        None,
    );
    for destination in [
        "https://api.openai.com",
        "https://example.com",
        "http://127.0.0.1:8080",
        "https://[::1]",
        "ws://localhost:1234",
    ] {
        assert!(
            allow.check_egress(destination).is_ok(),
            "allow_all must permit parseable {destination:?}"
        );
    }
    // Even default-allow never permits an UNPARSEABLE destination.
    for destination in ["", "ftp://x.test", "not a host", "https://user:pw@x.test/"] {
        match allow.check_egress(destination) {
            Err(EgressError::Unparseable(_)) => {}
            other => panic!("allow_all must still refuse unparseable {destination:?}: {other:?}"),
        }
    }
    // The gate exposes the installed policy shape.
    assert!(
        allow.network_gate().installed().is_none(),
        "allow_all = None"
    );
    assert!(deny.network_gate().installed().is_some(), "deny_all = Some");
    assert!(
        deny.network_gate().installed().unwrap().is_empty(),
        "deny_all installs the empty policy"
    );
}

#[test]
fn parts_based_checks_match_the_text_gate() {
    let engine = engine_with(&["https://allowed.example.com:8443"]);
    // (scheme, host, port, is_ipv4, ip, allowed)
    type EgressFixture = (
        &'static str,
        &'static str,
        Option<u16>,
        bool,
        Option<[u8; 4]>,
        bool,
    );
    let cases: Vec<EgressFixture> = vec![
        (
            "https",
            "allowed.example.com",
            Some(8443),
            false,
            None,
            true,
        ),
        (
            "HTTPS",
            "ALLOWED.EXAMPLE.COM",
            Some(8443),
            false,
            None,
            true,
        ),
        ("https", "allowed.example.com", None, false, None, false),
        (
            "http",
            "allowed.example.com",
            Some(8443),
            false,
            None,
            false,
        ),
        (
            "https",
            "allowed.example.com",
            Some(443),
            false,
            None,
            false,
        ),
        (
            "https",
            "allowed.example.com.evil",
            Some(8443),
            false,
            None,
            false,
        ),
        (
            "https",
            "user@allowed.example.com",
            Some(8443),
            false,
            None,
            false,
        ),
        ("https", "allowed.example.com", Some(0), false, None, false),
        (
            "https",
            "127.000.0.01",
            Some(8443),
            true,
            Some([127, 0, 0, 1]),
            false,
        ),
        ("wss", "allowed.example.com", Some(8443), false, None, false),
    ];
    for (scheme, host, port, is_ipv4, ip, allowed) in cases {
        let result = engine.check_egress_parts(scheme, host, port, is_ipv4, ip);
        assert_eq!(
            result.is_ok(),
            allowed,
            "check_egress_parts({scheme:?}, {host:?}, {port:?}, {is_ipv4}, {ip:?})"
        );
    }
    assert!(
        matches!(
            engine.check_egress_parts("ftp", "allowed.example.com", None, false, None),
            Err(EgressError::Unparseable(_))
        ),
        "unsupported scheme is a typed unparseable refusal"
    );
    assert!(
        matches!(
            engine.check_egress_parts("https", "", None, false, None),
            Err(EgressError::Unparseable(_))
        ),
        "empty host is unparseable"
    );
}

#[test]
fn network_capability_evaluation_maps_to_the_gate() {
    let engine = engine_with(&["https://allowed.example.com"]);
    let capability = |destination: &str| Capability::Network {
        destination: destination.to_string(),
    };
    assert_eq!(
        engine.evaluate(&capability("https://allowed.example.com")),
        PermissionDecision::Allow,
        "allowed destination evaluates Allow"
    );
    assert_eq!(
        engine.evaluate(&capability("https://evil.example.com")),
        PermissionDecision::Deny,
        "denied destination evaluates Deny"
    );
    assert_eq!(
        engine.evaluate(&capability("not a destination")),
        PermissionDecision::Deny,
        "unparseable destination evaluates Deny (never Ask/Allow)"
    );
    assert_eq!(
        engine.evaluate(&capability("")),
        PermissionDecision::Deny,
        "empty destination evaluates Deny"
    );
    // The parts API feeds the same parsed-target check.
    let target = RequestTarget::parse("https://allowed.example.com/x").unwrap();
    assert_eq!(
        engine
            .network_gate()
            .check(&target)
            .map_err(|e| format!("{e}")),
        Ok(()),
        "parsed target checks Allowed"
    );
}

#[test]
fn network_gate_parse_is_strict() {
    let bad: Vec<Vec<&str>> = vec![
        vec![""],
        vec!["https://"],
        vec!["https://example.com:0"],
        vec!["https://example.com:abc"],
        vec!["ftps://example.com"],
        vec!["example.com/path"],
        vec!["*.com"],
        vec!["exa mple.com"],
        vec!["https://example.com", "https://example.com"],
        vec!["127.0.0.1", "127.000.000.001"],
    ];
    for entries in bad {
        assert!(
            NetworkGate::parse(entries.iter().copied()).is_err(),
            "gate must refuse bad entries {entries:?}"
        );
    }
    assert!(
        NetworkGate::parse(std::iter::empty::<&str>()).is_ok(),
        "an empty allowlist is legal (default-deny)"
    );
    let empty = NetworkGate::parse(std::iter::empty::<&str>()).unwrap();
    assert!(
        empty.installed().is_some_and(|p| p.is_empty()),
        "empty parse installs the empty (deny-all) policy"
    );
    // from_policy installs the caller's parsed policy verbatim.
    let policy = DestinationPolicy::parse_lines(["example.com"]).unwrap();
    let gate = NetworkGate::from_policy(policy.clone());
    assert_eq!(gate.installed(), Some(&policy), "from_policy installs it");
}

#[test]
fn workspace_paths_are_confined() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("inside.txt"), "x").unwrap();
    std::fs::write(root.join("sub/nested.txt"), "x").unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("outside.txt"), "x").unwrap();
    let outside_dir = outside.path().canonicalize().unwrap();

    let engine = PermissionEngine::new(SandboxPolicy::default(), Some(root.clone()));
    let inside = [
        Path::new("inside.txt"),
        Path::new("sub/nested.txt"),
        Path::new("."),
        Path::new("missing.txt"),
        Path::new("sub/missing.txt"),
    ];
    for path in inside {
        assert!(
            engine.is_within_workspace(path),
            "path {path:?} must be inside the workspace"
        );
    }
    let escapes = [
        Path::new("../outside.txt"),
        Path::new("sub/../../outside.txt"),
        Path::new("sub/../.."),
        Path::new("/etc/passwd"),
        Path::new("/"),
    ];
    for path in escapes {
        assert!(
            !engine.is_within_workspace(path),
            "path {path:?} must be outside the workspace"
        );
    }
    // Absolute paths under the root are inside.
    assert!(
        engine.is_within_workspace(&root.join("inside.txt")),
        "absolute inside path"
    );
    assert!(
        !engine.is_within_workspace(&outside_dir.join("outside.txt")),
        "absolute outside path"
    );

    // A dangling symlink escape is never inside.
    #[cfg(unix)]
    {
        let link = root.join("escape");
        std::os::unix::fs::symlink(&outside_dir, &link).unwrap();
        assert!(
            !engine.is_within_workspace(Path::new("escape/outside.txt")),
            "symlinked directory escape must be refused"
        );
        let loop_link = root.join("loop");
        std::os::unix::fs::symlink(&loop_link, &loop_link).unwrap();
        assert!(
            !engine.is_within_workspace(Path::new("loop")),
            "symlink loop must be refused"
        );
    }
}

#[test]
fn capability_evaluation_is_policy_exact() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("inside.txt"), "x").unwrap();
    let outside = tempfile::tempdir().unwrap();
    let outside_file = outside.path().canonicalize().unwrap().join("o.txt");
    std::fs::write(&outside_file, "x").unwrap();

    let engine = PermissionEngine::new(SandboxPolicy::default(), Some(root.clone()));
    assert_eq!(
        engine.evaluate(&Capability::ReadWorkspace {
            path: PathBuf::from("inside.txt")
        }),
        PermissionDecision::Allow,
        "workspace read uses the workspace rule"
    );
    assert_eq!(
        engine.evaluate(&Capability::WriteWorkspace {
            path: PathBuf::from("inside.txt")
        }),
        PermissionDecision::Allow,
        "workspace write uses the workspace rule"
    );
    // Escaping paths fall through to the external rule (Ask by default).
    assert_eq!(
        engine.evaluate(&Capability::ReadWorkspace {
            path: outside_file.clone()
        }),
        PermissionDecision::Ask,
        "escaping read evaluates the external rule"
    );
    assert_eq!(
        engine.evaluate(&Capability::WriteExternal {
            path: PathBuf::from("inside.txt")
        }),
        PermissionDecision::Allow,
        "an external-typed path inside the workspace uses the workspace rule"
    );
    // Explicit rule overrides.
    let deny_write = PermissionEngine::new(
        SandboxPolicy {
            write_workspace: Rule::Deny,
            read_external: Rule::Deny,
            ..SandboxPolicy::default()
        },
        Some(root.clone()),
    );
    assert_eq!(
        deny_write.evaluate(&Capability::WriteWorkspace {
            path: PathBuf::from("inside.txt")
        }),
        PermissionDecision::Deny,
        "deny write rule"
    );
    assert_eq!(
        deny_write.evaluate(&Capability::ReadExternal {
            path: outside_file.clone()
        }),
        PermissionDecision::Deny,
        "deny external read rule"
    );
    assert_eq!(
        deny_write.evaluate(&Capability::ExecuteShell {
            command: "sh".into()
        }),
        PermissionDecision::Ask,
        "default shell rule is Ask"
    );
    assert_eq!(
        deny_write.evaluate(&Capability::Mcp {
            server: "mcp:test".into()
        }),
        PermissionDecision::Allow,
        "default MCP rule is Allow"
    );
    assert_eq!(
        deny_write.evaluate(&Capability::Git {
            operation: "status".into()
        }),
        PermissionDecision::Allow,
        "default git rule is Allow"
    );
    // No workspace root: EVERY path is external, never accidentally inside.
    let rootless = PermissionEngine::new(SandboxPolicy::default(), None);
    assert!(
        !rootless.is_within_workspace(Path::new("inside.txt")),
        "no root means nothing is inside"
    );
    assert_eq!(
        rootless.evaluate(&Capability::ReadWorkspace {
            path: PathBuf::from("inside.txt")
        }),
        PermissionDecision::Ask,
        "rootless workspace read falls to the external rule"
    );
}
