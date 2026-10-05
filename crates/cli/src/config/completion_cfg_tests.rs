//! Adversarial strictness covers for the additive `[completion]` section
//! (P2 step execution): absent => inert defaults, unknown keys / wrong
//! shapes / invalid templates are parse or validation errors, and a
//! configured template resolves to the exact orchestrator policy.
use super::*;

fn parse(json: serde_json::Value) -> Result<Config, serde_json::Error> {
    serde_json::from_value(json)
}

#[test]
fn absent_completion_section_keeps_the_inert_defaults() {
    let cfg = parse(serde_json::json!({"model": "m"})).unwrap();
    assert_eq!(cfg.completion, CompletionCfg::default());
    let steps = cfg.completion.steps_config().unwrap();
    assert_eq!(steps.remote, "origin");
    assert_eq!(steps.base_branch, "main");
    assert_eq!(steps.pr_command, None);
}

#[test]
fn configured_completion_section_resolves_to_the_validated_policy() {
    let cfg = parse(serde_json::json!({
        "model": "m",
        "completion": {
            "remote": "upstream",
            "base_branch": "develop",
            "pr_command": "gh pr create --head {branch} --base {base}"
        }
    }))
    .unwrap();
    let steps = cfg.completion.steps_config().unwrap();
    assert_eq!(steps.remote, "upstream");
    assert_eq!(steps.base_branch, "develop");
    assert_eq!(
        steps.pr_command.as_deref(),
        Some("gh pr create --head {branch} --base {base}")
    );
}

#[test]
fn completion_section_is_strict_and_hostile_values_are_refused() {
    // Unknown key anywhere in the section.
    assert!(
        parse(serde_json::json!({"completion": {"pr_command": "x {branch}", "extra": 1}})).is_err()
    );
    // Non-string member.
    assert!(parse(serde_json::json!({"completion": {"pr_command": 7}})).is_err());
    // Positional array shape is not a section.
    assert!(parse(serde_json::json!({"completion": ["origin"]})).is_err());
    // A shell-metachar / unclosed / unknown-placeholder / branch-less
    // template is refused at resolution time (never half-run).
    for (name, command) in [
        ("metachar", "gh pr create --head {branch}; rm -rf /"),
        ("unknown", "gh pr create --head {nope}"),
        ("branchless", "gh pr create --base main"),
        ("unclosed", "gh pr create --head {branch"),
    ] {
        let cfg = parse(serde_json::json!({"completion": {"pr_command": command}})).unwrap();
        let err = cfg.completion.steps_config().unwrap_err();
        assert!(err.starts_with("completion: "), "{name}: {err}");
    }
    // Hostile remote / base names are refused.
    let cfg = parse(serde_json::json!({"completion": {"remote": "a b"}})).unwrap();
    assert!(cfg.completion.steps_config().is_err());
    let cfg = parse(serde_json::json!({"completion": {"base_branch": "a..b"}})).unwrap();
    assert!(cfg.completion.steps_config().is_err());
}

#[test]
fn typed_argv_completion_section_resolves_and_is_strict() {
    // The additive typed argv: spaces in the program path and in one
    // argument stay per-element values.
    let cfg = parse(serde_json::json!({
        "model": "m",
        "completion": {
            "pr_program": "/opt/My Tools/gh",
            "pr_args": ["pr", "create", "--head", "{branch}", "--title", "my PR title"]
        }
    }))
    .unwrap();
    let steps = cfg.completion.steps_config().unwrap();
    assert_eq!(steps.pr_program.as_deref(), Some("/opt/My Tools/gh"));
    assert_eq!(steps.pr_command, None);
    assert_eq!(
        steps.pr_args,
        vec![
            "pr",
            "create",
            "--head",
            "{branch}",
            "--title",
            "my PR title"
        ]
    );

    // Strict shapes inside the section.
    assert!(parse(serde_json::json!({"completion": {"pr_program": 7}})).is_err());
    assert!(parse(serde_json::json!({"completion": {"pr_args": "nope"}})).is_err());
    assert!(parse(serde_json::json!({"completion": {"pr_extra": 1}})).is_err());
    assert!(parse(serde_json::json!({"completion": {"pr_args": [1]}})).is_err());

    // pr_args without a program, and BOTH shapes (ambiguous), refuse.
    let args_only = parse(serde_json::json!({"completion": {"pr_args": ["{branch}"]}})).unwrap();
    assert!(args_only.completion.steps_config().is_err());
    let both = parse(serde_json::json!({
        "completion": {
            "pr_command": "gh pr create --head {branch}",
            "pr_program": "/bin/gh",
            "pr_args": ["--head", "{branch}"]
        }
    }))
    .unwrap();
    assert!(both
        .completion
        .steps_config()
        .unwrap_err()
        .starts_with("completion: "));

    // Typed refusals mirror the orchestrator validator exactly.
    for (name, program, args) in [
        ("branchless", "/bin/gh", vec!["--base", "main"]),
        ("unknown", "/bin/gh", vec!["--head", "{nope}"]),
        ("control", "/bin/gh", vec!["--head", "{branch}\u{7}"]),
    ] {
        let cfg = parse(serde_json::json!({
            "completion": {"pr_program": program, "pr_args": args}
        }))
        .unwrap();
        assert!(cfg.completion.steps_config().is_err(), "{name}");
    }
}

/// The additive `[cloud]` section: disabled by default, byte-identical
/// to an absent section while disabled, strictly parsed, and its
/// databases resolve INSIDE the data dir with options off-by-default.
#[test]
fn cloud_section_is_disabled_by_default_and_strictly_parsed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cloud.json");

    // Absent section == explicit disabled section: same resolved
    // databases (none) and the same serialized config.
    let absent = Config::default();
    assert!(!absent.cloud.enabled);
    assert_eq!(absent.cloud.control_plane_path(dir.path()).unwrap(), None);
    assert_eq!(absent.cloud.scm_path(dir.path()).unwrap(), None);
    std::fs::write(&path, r#"{"model": "m"}"#).unwrap();
    let parsed_absent = Config::load_strict(&path).unwrap();
    std::fs::write(&path, r#"{"model": "m", "cloud": {"enabled": false}}"#).unwrap();
    let parsed_disabled = Config::load_strict(&path).unwrap();
    assert_eq!(
        serde_json::to_value(&parsed_absent).unwrap(),
        serde_json::to_value(&parsed_disabled).unwrap(),
        "a disabled [cloud] section must serialize exactly like an absent one"
    );
    assert_eq!(
        parsed_disabled
            .cloud
            .control_plane_path(dir.path())
            .unwrap(),
        None
    );
    assert_eq!(parsed_disabled.cloud.scm_path(dir.path()).unwrap(), None);

    // Enabled: the default file names resolve under the data dir.
    std::fs::write(&path, r#"{"cloud": {"enabled": true}}"#).unwrap();
    let enabled = Config::load_strict(&path).unwrap();
    assert_eq!(
        enabled.cloud.control_plane_path(dir.path()).unwrap(),
        Some(dir.path().join("control-plane.db"))
    );
    assert_eq!(
        enabled.cloud.scm_path(dir.path()).unwrap(),
        Some(dir.path().join("scm.db"))
    );
    // Explicit simple file names resolve too.
    std::fs::write(
        &path,
        r#"{"cloud": {"enabled": true, "database": "cp.db", "scm_database": "repos.db"}}"#,
    )
    .unwrap();
    let named = Config::load_strict(&path).unwrap();
    assert_eq!(
        named.cloud.control_plane_path(dir.path()).unwrap(),
        Some(dir.path().join("cp.db"))
    );
    assert_eq!(
        named.cloud.scm_path(dir.path()).unwrap(),
        Some(dir.path().join("repos.db"))
    );

    // Strict parsing: unknown/duplicate keys, wrong types, positional
    // arrays and hostile paths are refused by BOTH load paths.
    for bad in [
        r#"{"cloud": {"enabled": "yes"}}"#,
        r#"{"cloud": {"enabled": true, "bogus": 1}}"#,
        r#"{"cloud": {"enabled": true, "enabled": false}}"#,
        r#"{"cloud": {"database": 1}}"#,
        r#"{"cloud": true}"#,
        r#"{"cloud": ["enabled"]}"#,
    ] {
        std::fs::write(&path, bad).unwrap();
        assert!(
            Config::load(&path).is_err(),
            "hostile [cloud] must fail: {bad}"
        );
        assert!(Config::load_strict(&path).is_err(), "{bad}");
    }
    for hostile in [
        r#"{"cloud": {"enabled": true, "database": "../escape.db"}}"#,
        r#"{"cloud": {"enabled": true, "database": "/etc/passwd"}}"#,
        r#"{"cloud": {"enabled": true, "database": "sub/dir.db"}}"#,
        r#"{"cloud": {"enabled": true, "database": "a\u{5c}b"}}"#,
        r#"{"cloud": {"enabled": true, "scm_database": ".."}}"#,
    ] {
        std::fs::write(&path, hostile).unwrap();
        assert!(
            Config::load_strict(&path).is_err(),
            "hostile cloud path must fail: {hostile}"
        );
    }
    // A disabled section with a hostile name is still refused (the file
    // never says two different things).
    std::fs::write(
        &path,
        r#"{"cloud": {"enabled": false, "database": "../x"}}"#,
    )
    .unwrap();
    assert!(Config::load_strict(&path).is_err());
}

/// The additive payload/SSO/GitHub-App cloud surface: absent sections
/// stay inert, hostile shapes are refused on both load paths, an enabled
/// SSO or GitHub App section requires the cloud section, and the
/// payload root resolves under the data dir with traversal refused.
#[test]
fn cloud_payload_sso_and_github_app_sections_are_strict_and_additive() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cloud-extra.json");

    let absent = Config::default();
    assert!(absent.cloud.sso.is_none());
    assert!(absent.cloud.github_app.is_none());
    assert_eq!(
        absent.cloud.payload_root(dir.path()).unwrap(),
        dir.path().join("payloads"),
        "the default payload root lives under the data dir"
    );

    for bad in [
        r#"{"cloud": {"payload_dir": 1}}"#,
        r#"{"cloud": {"sso": true}}"#,
        r#"{"cloud": {"sso": {"enabled": true, "bogus": 1}}}"#,
        r#"{"cloud": {"sso": {"enabled": true, "enabled": false}}}"#,
        r#"{"cloud": {"github_app": {"app_id": "x"}}}"#,
        r#"{"cloud": {"github_app": {"enabled": true, "hostile": 1}}}"#,
    ] {
        std::fs::write(&path, bad).unwrap();
        assert!(Config::load(&path).is_err(), "hostile shape: {bad}");
        assert!(Config::load_strict(&path).is_err(), "{bad}");
    }
    for hostile_root in [
        r#"{"cloud": {"payload_dir": "../escape"}}"#,
        r#"{"cloud": {"payload_dir": "a/../b"}}"#,
        r#"{"cloud": {"payload_dir": "a\u{5c}b"}}"#,
    ] {
        std::fs::write(&path, hostile_root).unwrap();
        assert!(Config::load_strict(&path).is_err(), "{hostile_root}");
    }

    // An enabled SSO section requires issuer and client id; the payload
    // name of the optional secret is validated whenever present.
    std::fs::write(
        &path,
        r#"{"cloud": {"enabled": true, "sso": {"enabled": true}}}"#,
    )
    .unwrap();
    assert!(Config::load_strict(&path).is_err());
    std::fs::write(
        &path,
        r#"{"cloud": {"enabled": true, "sso": {"enabled": true, "issuer": "https://idp.example/", "client_id": "c"}}}"#,
    )
    .unwrap();
    assert!(Config::load_strict(&path).is_err(), "trailing slash");
    std::fs::write(
        &path,
        r#"{"cloud": {"enabled": true, "sso": {"enabled": true, "issuer": "https://idp.example", "client_id": "c", "client_secret": "../x"}}}"#,
    )
    .unwrap();
    assert!(Config::load_strict(&path).is_err(), "traversal name");
    std::fs::write(
        &path,
        r#"{"cloud": {"enabled": true, "sso": {"enabled": true, "issuer": "https://idp.example", "client_id": "client-1", "client_secret": "idp.secret", "discovery_max_age_ms": 1000, "jwks_max_age_ms": 1000, "max_jwks_refetches": 1}}}"#,
    )
    .unwrap();
    let sso = Config::load_strict(&path).unwrap();
    assert_eq!(
        sso.cloud.sso.as_ref().unwrap().issuer().unwrap(),
        "https://idp.example"
    );
    assert_eq!(
        sso.cloud.payload_root(dir.path()).unwrap(),
        dir.path().join("payloads")
    );

    // The SSO signing-algorithm policy is additive and strict: the
    // default is RS256-only, `none`/unsupported/empty lists and
    // non-list shapes are refused at load.
    std::fs::write(
        &path,
        r#"{"cloud": {"enabled": true, "sso": {"enabled": true, "issuer": "https://idp.example", "client_id": "c", "allowed_algorithms": ["RS256", "HS256"]}}}"#,
    )
    .unwrap();
    let sso = Config::load_strict(&path).unwrap();
    assert_eq!(
        sso.cloud
            .sso
            .as_ref()
            .unwrap()
            .allowed_algorithms()
            .unwrap(),
        vec!["RS256".to_string(), "HS256".to_string()]
    );
    for bad_alg_list in [
        r#"{"cloud": {"enabled": true, "sso": {"enabled": true, "issuer": "https://idp.example", "client_id": "c", "allowed_algorithms": ["none"]}}}"#,
        r#"{"cloud": {"enabled": true, "sso": {"enabled": true, "issuer": "https://idp.example", "client_id": "c", "allowed_algorithms": []}}}"#,
        r#"{"cloud": {"enabled": true, "sso": {"enabled": true, "issuer": "https://idp.example", "client_id": "c", "allowed_algorithms": ["RS512"]}}}"#,
        r#"{"cloud": {"enabled": true, "sso": {"enabled": true, "issuer": "https://idp.example", "client_id": "c", "allowed_algorithms": "RS256"}}}"#,
    ] {
        std::fs::write(&path, bad_alg_list).unwrap();
        assert!(Config::load_strict(&path).is_err(), "{bad_alg_list}");
    }

    // An enabled GitHub App section requires app id, both staged payload
    // names and the tenant organization.
    for bad_app in [
        r#"{"cloud": {"enabled": true, "github_app": {"enabled": true}}}"#,
        r#"{"cloud": {"enabled": true, "github_app": {"enabled": true, "app_id": 7, "private_key": "k.pem"}}}"#,
        r#"{"cloud": {"enabled": true, "github_app": {"enabled": true, "app_id": 7, "private_key": "k.pem", "webhook_secret": "s"}}}"#,
        r#"{"cloud": {"enabled": true, "github_app": {"enabled": true, "app_id": 7, "private_key": "../k.pem", "webhook_secret": "s", "organization": "org_x"}}}"#,
    ] {
        std::fs::write(&path, bad_app).unwrap();
        assert!(Config::load_strict(&path).is_err(), "{bad_app}");
    }
    std::fs::write(
        &path,
        r#"{"cloud": {"enabled": true, "payload_dir": "/srv/faktor/payloads", "github_app": {"enabled": true, "app_id": 7, "private_key": "app.pem", "webhook_secret": "hook.secret", "api_base": "http://127.0.0.1:9/", "organization": "org_local"}}}"#,
    )
    .unwrap();
    let app = Config::load_strict(&path).unwrap();
    let github = app.cloud.github_app.as_ref().unwrap();
    assert_eq!(github.organization().unwrap(), "org_local");
    assert_eq!(
        github.app_config().unwrap().api_base,
        "http://127.0.0.1:9/",
        "the adapter trims the trailing slash itself"
    );
    assert_eq!(
        app.cloud.payload_root(dir.path()).unwrap(),
        std::path::PathBuf::from("/srv/faktor/payloads"),
        "an absolute payload root is honored"
    );

    // Both new sections require [cloud] enabled (an orphan section is a
    // startup refusal, never a silently inert wiring).
    std::fs::write(
        &path,
        r#"{"cloud": {"sso": {"enabled": true, "issuer": "https://idp.example", "client_id": "c"}}}"#,
    )
    .unwrap();
    assert!(Config::load_strict(&path).is_err());
    std::fs::write(
        &path,
        r#"{"cloud": {"github_app": {"enabled": true, "app_id": 7, "private_key": "k.pem", "webhook_secret": "s", "organization": "org"}}}"#,
    )
    .unwrap();
    assert!(Config::load_strict(&path).is_err());
}

/// The additive `[billing.report]` schedule: disabled by default with
/// byte-identical serialization to an absent section, strict parsing,
/// bounded cadence and the billing/enabled pairing.
#[test]
fn billing_report_section_is_disabled_by_default_strict_and_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("report.json");
    let absent = Config::default();
    assert!(absent.billing.report.is_none());
    std::fs::write(&path, r#"{"model": "m"}"#).unwrap();
    let parsed_absent = Config::load_strict(&path).unwrap();
    std::fs::write(
        &path,
        r#"{"model": "m", "billing": {"report": {"enabled": false}}}"#,
    )
    .unwrap();
    let parsed_disabled = Config::load_strict(&path).unwrap();
    assert_eq!(
        serde_json::to_value(&parsed_absent).unwrap(),
        serde_json::to_value(&parsed_disabled).unwrap(),
        "a disabled report section serializes like an absent one"
    );

    for bad in [
        r#"{"billing": {"report": true}}"#,
        r#"{"billing": {"report": {"enabled": true, "bogus": 1}}}"#,
        r#"{"billing": {"report": {"interval_ms": "soon"}}}"#,
    ] {
        std::fs::write(&path, bad).unwrap();
        assert!(Config::load(&path).is_err(), "{bad}");
    }
    // Enabled report over a disabled billing section is refused.
    std::fs::write(
        &path,
        r#"{"billing": {"enabled": false, "report": {"enabled": true, "base_url": "http://127.0.0.1:9"}}}"#,
    )
    .unwrap();
    assert!(Config::load_strict(&path).is_err());
    // Enabled report requires a base_url.
    std::fs::write(
        &path,
        r#"{"cloud": {"enabled": true}, "billing": {"enabled": true, "organization": "org", "plans": {"pro": {"plan_id": "pro"}}, "report": {"enabled": true}}}"#,
    )
    .unwrap();
    assert!(Config::load_strict(&path).is_err());
    // Bounds: interval/period/backoff are all refused outside their range.
    for (key, value) in [
        ("interval_ms", "10"),
        ("interval_ms", "999999999"),
        ("period_ms", "10"),
        ("period_ms", "99999999999999"),
        ("max_backoff_ms", "-1"),
        ("max_backoff_ms", "999999999"),
        ("max_catch_up", "999999"),
    ] {
        std::fs::write(
            &path,
            format!(
                r#"{{"cloud": {{"enabled": true}}, "billing": {{"enabled": true, "organization": "org", "plans": {{"pro": {{"plan_id": "pro"}}}}, "report": {{"enabled": true, "base_url": "http://127.0.0.1:9", "{key}": {value}}}}}}}"#
            ),
        )
        .unwrap();
        assert!(Config::load_strict(&path).is_err(), "{key}={value}");
    }
    // A complete report section resolves the strict policy and vendor
    // config; an explicitly empty auth_env selects the unauthenticated
    // local-mock shape.
    std::fs::write(
        &path,
        r#"{"cloud": {"enabled": true}, "billing": {"enabled": true, "organization": "org", "plans": {"pro": {"plan_id": "pro"}}, "report": {"enabled": true, "base_url": "http://127.0.0.1:9/", "auth_env": "", "interval_ms": 5000, "period_ms": 60000, "max_attempts": 2, "retry_base_ms": 0, "max_backoff_ms": 1000, "max_catch_up": 3}}}"#,
    )
    .unwrap();
    let cfg = Config::load_strict(&path).unwrap();
    let report = cfg.billing.report.as_ref().unwrap();
    assert!(report.unauthenticated());
    let policy = report.policy().unwrap();
    assert_eq!(policy.interval_ms, 5000);
    assert_eq!(policy.period_ms, 60000);
    assert_eq!(policy.max_attempts, 2);
    assert_eq!(policy.retry_base_ms, 0);
    assert_eq!(policy.max_backoff_ms, 1000);
    assert_eq!(policy.max_catch_up, 3);
    assert_eq!(
        report.vendor_config().unwrap().base_url,
        "http://127.0.0.1:9"
    );
    // The default catch-up window is the documented 24-period bound.
    std::fs::write(
        &path,
        r#"{"cloud": {"enabled": true}, "billing": {"enabled": true, "organization": "org", "plans": {"pro": {"plan_id": "pro"}}, "report": {"enabled": true, "base_url": "http://127.0.0.1:9/"}}}"#,
    )
    .unwrap();
    let cfg = Config::load_strict(&path).unwrap();
    assert_eq!(
        cfg.billing
            .report
            .as_ref()
            .unwrap()
            .policy()
            .unwrap()
            .max_catch_up,
        faktor_cloud::DEFAULT_REPORT_CATCH_UP
    );
}

/// `[worker_node]` payload staging: the payload dir and the token
/// payload name are validated even while disabled; the resolved root
/// stays inside its configured base and traversal is refused.
#[test]
fn worker_node_payload_staging_is_validated_and_resolved() {
    let dir = tempfile::tempdir().unwrap();
    let mut node = WorkerNodeCfg::default();
    assert_eq!(
        node.payload_root(dir.path()).unwrap(),
        dir.path().join("worker_payloads")
    );
    node.payload_dir = Some("../escape".into());
    assert!(node.validate().is_err(), "traversal is refused");
    node.payload_dir = Some("staging/payloads".into());
    assert_eq!(
        node.payload_root(dir.path()).unwrap(),
        dir.path().join("staging/payloads")
    );
    assert!(node
        .payload_root(dir.path())
        .unwrap()
        .starts_with(dir.path()));
    node.payload_dir = Some("with\u{7}control".into());
    assert!(node.validate().is_err());
    node.payload_dir = Some("/var/lib/faktor/payloads".into());
    assert_eq!(
        node.payload_root(dir.path()).unwrap(),
        std::path::PathBuf::from("/var/lib/faktor/payloads")
    );
    node.token_payload = Some("worker.token".into());
    assert!(node.validate().is_ok());
    node.token_payload = Some("../worker.token".into());
    assert!(node.validate().is_err());
}

/// The additive `[updater]` section: disabled by default with BYTE-IDENTICAL
/// serialization to an absent section, strict parsing on both load paths,
/// bounded values, a mandatory non-empty operator allowlist when enabled,
/// and no filesystem resolution while disabled.
#[test]
fn updater_section_is_disabled_by_default_and_strictly_parsed() {
    const KEY: &str = "PMIf08ao62O4xMR4upvk5ymt++8EcWRtWHWZGLa4TKo=";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("updater.json");

    // Absent == explicit disabled: no store, no install root, and
    // identical serialization (the disabled daemon is byte-identical).
    let absent = Config::default();
    assert!(!absent.updater.enabled);
    assert_eq!(absent.updater.database_path(dir.path()).unwrap(), None);
    assert_eq!(absent.updater.install_root_path(dir.path()).unwrap(), None);
    std::fs::write(&path, r#"{"model": "m"}"#).unwrap();
    let parsed_absent = Config::load_strict(&path).unwrap();
    std::fs::write(&path, r#"{"model": "m", "updater": {"enabled": false}}"#).unwrap();
    let parsed_disabled = Config::load_strict(&path).unwrap();
    assert_eq!(
        serde_json::to_value(&parsed_absent).unwrap(),
        serde_json::to_value(&parsed_disabled).unwrap(),
        "a disabled [updater] section must serialize exactly like an absent one"
    );
    assert_eq!(
        parsed_disabled.updater.database_path(dir.path()).unwrap(),
        None
    );
    assert_eq!(
        parsed_disabled
            .updater
            .install_root_path(dir.path())
            .unwrap(),
        None
    );

    // Enabled: keys are mandatory, the default paths resolve under the
    // data dir, and the channel parses.
    let enabled_json = format!(
        r#"{{"updater": {{"enabled": true, "channel": "beta", "keys": [{{"id": "op", "public_key": "{KEY}"}}]}}}}"#
    );
    std::fs::write(&path, &enabled_json).unwrap();
    let enabled = Config::load_strict(&path).unwrap();
    assert!(enabled.updater.enabled);
    assert!(
        !enabled.updater.allow_legacy_manifests_once_resolved(),
        "the one-time legacy allowance is off by default"
    );
    std::fs::write(
        &path,
        format!(
            r#"{{"updater": {{"enabled": true, "allow_legacy_manifests_once": true, "keys": [{{"id": "op", "public_key": "{KEY}"}}]}}}}"#
        ),
    )
    .unwrap();
    assert!(
        Config::load_strict(&path)
            .unwrap()
            .updater
            .allow_legacy_manifests_once_resolved(),
        "the documented one-time legacy allowance round-trips"
    );
    assert_eq!(
        enabled.updater.channel().unwrap(),
        faktor_updater::Channel::Beta
    );
    assert_eq!(
        enabled.updater.database_path(dir.path()).unwrap(),
        Some(dir.path().join("update.db"))
    );
    assert_eq!(
        enabled.updater.install_root_path(dir.path()).unwrap(),
        Some(dir.path().join("install"))
    );
    assert_eq!(enabled.updater.trusted_keys().unwrap().len(), 1);
    // Relative install roots resolve under the data dir; absolute ones
    // are honored as written.
    std::fs::write(
        &path,
        format!(
            r#"{{"updater": {{"enabled": true, "install_root": "releases/current", "keys": [{{"id": "op", "public_key": "{KEY}"}}]}}}}"#
        ),
    )
    .unwrap();
    assert_eq!(
        Config::load_strict(&path)
            .unwrap()
            .updater
            .install_root_path(dir.path())
            .unwrap(),
        Some(dir.path().join("releases").join("current"))
    );
    std::fs::write(
        &path,
        format!(
            r#"{{"updater": {{"enabled": true, "install_root": "/opt/faktor", "keys": [{{"id": "op", "public_key": "{KEY}"}}]}}}}"#
        ),
    )
    .unwrap();
    assert_eq!(
        Config::load_strict(&path)
            .unwrap()
            .updater
            .install_root_path(dir.path())
            .unwrap(),
        Some(std::path::PathBuf::from("/opt/faktor"))
    );

    // Shape errors are refused by BOTH load paths (unknown/duplicate
    // keys, wrong types, positional arrays, malformed key material).
    for bad in [
        r#"{"updater": {"enabled": "yes"}}"#,
        r#"{"updater": {"enabled": true, "bogus": 1}}"#,
        r#"{"updater": {"enabled": true, "enabled": false}}"#,
        r#"{"updater": {"channel": 1}}"#,
        r#"{"updater": {"allow_legacy_manifests_once": "yes"}}"#,
        r#"{"updater": {"allow_legacy_manifests_once": true, "allow_legacy_manifests_once": false}}"#,
        r#"{"updater": true}"#,
        r#"{"updater": ["enabled"]}"#,
        // Unknown key field / missing key field.
        format!(
            r#"{{"updater": {{"keys": [{{"id": "op", "public_key": "{KEY}", "extra": 1}}]}}}}"#
        )
        .as_str(),
        r#"{"updater": {"keys": [{"id": "op"}]}}"#,
    ] {
        std::fs::write(&path, bad).unwrap();
        assert!(
            Config::load(&path).is_err(),
            "hostile [updater] shape must fail: {bad}"
        );
        assert!(Config::load_strict(&path).is_err(), "{bad}");
    }
    // Semantic errors are refused by the strict path (the daemon never
    // boots on a section it cannot honor): enabled without an
    // allowlist, unknown channels, duplicate identities and hostile
    // bounds.
    for bad in [
        r#"{"updater": {"enabled": true}}"#,
        r#"{"updater": {"enabled": true, "keys": []}}"#,
        r#"{"updater": {"channel": "nightly"}}"#,
        // Not base64 / wrong raw length. (A 32-byte string that is not a
        // curve point is covered by the keys.rs unit test; dalek
        // accepts reduced non-canonical encodings, so no fixed byte
        // pattern can be asserted here.)
        r#"{"updater": {"keys": [{"id": "op", "public_key": "!!!"}]}}"#,
        r#"{"updater": {"keys": [{"id": "op", "public_key": "AAAA"}]}}"#,
        format!(
            r#"{{"updater": {{"keys": [{{"id": "op", "public_key": "{KEY}"}}, {{"id": "op", "public_key": "{KEY}"}}]}}}}"#
        )
        .as_str(),
        r#"{"updater": {"max_artifact_bytes": 0}}"#,
        r#"{"updater": {"max_artifact_bytes": 99999999999999}}"#,
        r#"{"updater": {"clock_skew_ms": -1}}"#,
        r#"{"updater": {"clock_skew_ms": 99999999}}"#,
    ] {
        std::fs::write(&path, bad).unwrap();
        assert!(
            Config::load_strict(&path).is_err(),
            "hostile [updater] value must fail: {bad}"
        );
    }
    // Hostile install roots are refused even while disabled (the file
    // never says two different things).
    for hostile in [
        r#"{"updater": {"enabled": true, "install_root": "../escape", "keys": [{"id": "op", "public_key": "PMIf08ao62O4xMR4upvk5ymt++8EcWRtWHWZGLa4TKo="}]}}"#,
        r#"{"updater": {"enabled": false, "install_root": "a/../../b"}}"#,
        r#"{"updater": {"enabled": false, "install_root": ""}}"#,
    ] {
        std::fs::write(&path, hostile).unwrap();
        assert!(
            Config::load_strict(&path).is_err(),
            "hostile updater install root must fail: {hostile}"
        );
    }
    // More than the allowlist cap.
    let many: Vec<String> = (0..(MAX_UPDATER_KEYS + 1))
        .map(|i| format!(r#"{{"id": "op-{i}", "public_key": "{KEY}"}}"#))
        .collect();
    std::fs::write(
        &path,
        format!(r#"{{"updater": {{"keys": [{}]}}}}"#, many.join(",")),
    )
    .unwrap();
    assert!(Config::load_strict(&path).is_err());
}

/// The additive `[billing]` section: disabled by default (byte-identical
/// to an absent section), strictly parsed, plans/limits config-provided
/// only, and refused when enabled without the `[cloud]` principal.
#[test]
fn billing_section_is_disabled_by_default_strict_and_plan_configured() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("billing.json");

    // Absent section == explicit disabled section.
    let absent = Config::default();
    assert!(!absent.billing.enabled);
    assert_eq!(absent.billing.billing_path(dir.path()).unwrap(), None);
    assert_eq!(absent.billing.service_config().unwrap(), None);
    std::fs::write(&path, r#"{"model": "m"}"#).unwrap();
    let parsed_absent = Config::load_strict(&path).unwrap();
    std::fs::write(&path, r#"{"model": "m", "billing": {"enabled": false}}"#).unwrap();
    let parsed_disabled = Config::load_strict(&path).unwrap();
    assert_eq!(
        serde_json::to_value(&parsed_absent).unwrap(),
        serde_json::to_value(&parsed_disabled).unwrap(),
        "a disabled [billing] section must serialize exactly like an absent one"
    );
    assert_eq!(
        parsed_disabled.billing.billing_path(dir.path()).unwrap(),
        None
    );
    // Even disabled, a hostile database name is refused.
    std::fs::write(
        &path,
        r#"{"billing": {"enabled": false, "database": "../x.db"}}"#,
    )
    .unwrap();
    assert!(Config::load_strict(&path).is_err());

    // Enabled requires the cloud section (the principal/tenant surface).
    std::fs::write(
        &path,
        r#"{"billing": {"enabled": true, "organization": "org_a", "plans": {"pro": {"plan_id": "pro"}}}}"#,
    )
    .unwrap();
    assert!(
        Config::load_strict(&path).is_err(),
        "billing without cloud has no organization principal"
    );

    // Enabled with cloud: the plan table is the ONLY source of features
    // and limits (no defaults, no prices in code).
    std::fs::write(
        &path,
        r#"{
            "cloud": {"enabled": true},
            "billing": {
                "enabled": true,
                "organization": "org_a",
                "account": "acct_local",
                "managed_providers": ["managed-provider"],
                "default_plan": "pro",
                "plans": {
                    "pro": {
                        "plan_id": "pro",
                        "features": ["managed_providers", "byok"],
                        "limits": {"max_active_tasks": 2, "max_managed_spend_micro_per_period": 1000000}
                    }
                }
            }
        }"#,
    )
    .unwrap();
    let enabled = Config::load_strict(&path).unwrap();
    assert_eq!(
        enabled.billing.billing_path(dir.path()).unwrap(),
        Some(dir.path().join("billing.db")),
        "the default billing database resolves inside the data dir"
    );
    let service_config = enabled.billing.service_config().unwrap().unwrap();
    assert_eq!(
        service_config.category_of("managed-provider"),
        faktor_cloud::SpendCategory::Managed
    );
    assert_eq!(
        service_config.category_of("other"),
        faktor_cloud::SpendCategory::Byok
    );
    assert_eq!(
        service_config.plan("pro").and_then(|plan| plan
            .limits
            .get(faktor_cloud::LIMIT_MAX_ACTIVE_TASKS)
            .copied()),
        Some(2)
    );
    assert_eq!(enabled.billing.organization().unwrap().as_str(), "org_a");

    // Strict parsing: unknown/duplicate keys, wrong types, unknown
    // features/limits and an empty plan table are refused.
    for bad in [
        r#"{"billing": true}"#,
        r#"{"billing": {"enabled": "yes"}}"#,
        r#"{"billing": {"enabled": true, "bogus": 1}}"#,
        r#"{"billing": {"enabled": true, "enabled": false}}"#,
        r#"{"billing": {"database": 1}}"#,
        r#"{"billing": {"plans": []}}"#,
    ] {
        std::fs::write(&path, bad).unwrap();
        assert!(
            Config::load(&path).is_err() && Config::load_strict(&path).is_err(),
            "hostile [billing] must fail: {bad}"
        );
    }
    for bad in [
        // Unknown feature tag.
        r#"{"cloud": {"enabled": true}, "billing": {"enabled": true, "organization": "org_a", "plans": {"pro": {"plan_id": "pro", "features": ["gold"]}}}}"#,
        // Unknown limit name.
        r#"{"cloud": {"enabled": true}, "billing": {"enabled": true, "organization": "org_a", "plans": {"pro": {"plan_id": "pro", "limits": {"price": 1}}}}}"#,
        // Plan key != plan_id.
        r#"{"cloud": {"enabled": true}, "billing": {"enabled": true, "organization": "org_a", "plans": {"pro": {"plan_id": "team"}}}}"#,
        // Default plan outside the table.
        r#"{"cloud": {"enabled": true}, "billing": {"enabled": true, "organization": "org_a", "default_plan": "nope", "plans": {"pro": {"plan_id": "pro"}}}}"#,
        // Empty plan table while enabled.
        r#"{"cloud": {"enabled": true}, "billing": {"enabled": true, "organization": "org_a", "plans": {}}}"#,
        // Missing organization while enabled.
        r#"{"cloud": {"enabled": true}, "billing": {"enabled": true, "plans": {"pro": {"plan_id": "pro"}}}}"#,
        // A price-shaped plan field is NOT part of the contract.
        r#"{"cloud": {"enabled": true}, "billing": {"enabled": true, "organization": "org_a", "plans": {"pro": {"plan_id": "pro", "price_per_token": 1}}}}"#,
    ] {
        std::fs::write(&path, bad).unwrap();
        assert!(
            Config::load_strict(&path).is_err(),
            "the config must refuse: {bad}"
        );
    }
}

#[test]
fn workers_section_is_additive_strict_and_disabled_by_default() {
    // Absent = disabled, no database, no placement seam.
    let cfg = Config::default();
    assert!(!cfg.workers.enabled);
    assert!(cfg
        .workers
        .workers_path(std::path::Path::new("/tmp"))
        .unwrap()
        .is_none());
    // Strict shape: unknown keys and duplicates are parse errors.
    for bad in [
        r#"{"model": "m", "workers": {"enabled": false, "hostile": 1}}"#,
        r#"{"model": "m", "workers": {"enabled": true, "enabled": true}}"#,
        r#"{"model": "m", "workers": {"enabled": "yes"}}"#,
        r#"{"model": "m", "workers": {"min_cpu_cores": -1}}"#,
    ] {
        assert!(
            serde_json::from_str::<Config>(bad).is_err(),
            "the config must refuse: {bad}"
        );
    }
    // A disabled section still validates its database name shape, but
    // creates no file.
    let disabled: Config = serde_json::from_str(
        r#"{"model": "m", "workers": {"enabled": false, "database": "wp.db"}}"#,
    )
    .unwrap();
    assert!(disabled
        .workers
        .workers_path(std::path::Path::new("/tmp"))
        .unwrap()
        .is_none());
    let traversal: Config =
        serde_json::from_str(r#"{"model": "m", "workers": {"database": "../escape.db"}}"#).unwrap();
    assert!(traversal.validate().is_err(), "path traversal is refused");
}

#[test]
fn worker_plane_section_is_strict_and_enforces_the_deployment_boundary() {
    // Absent/disabled = no second socket, nothing to resolve.
    let cfg = Config::default();
    assert!(!cfg.worker_plane.enabled);
    assert!(cfg.worker_plane.resolve().unwrap().is_none());
    // Strict shape: unknown keys, duplicates and wrong types are parse
    // errors.
    for bad in [
        r#"{"model": "m", "worker_plane": {"enabled": false, "hostile": 1}}"#,
        r#"{"model": "m", "worker_plane": {"enabled": true, "enabled": true}}"#,
        r#"{"model": "m", "worker_plane": {"enabled": "yes"}}"#,
        r#"{"model": "m", "worker_plane": {"tls": "yes"}}"#,
    ] {
        assert!(
            serde_json::from_str::<Config>(bad).is_err(),
            "the config must refuse: {bad}"
        );
    }
    // Bind/auth shapes are errors even while the section is disabled.
    for bad in [
        r#"{"model": "m", "worker_plane": {"bind": "not-a-socket"}}"#,
        r#"{"model": "m", "worker_plane": {"bind": "127.0.0.1:99999"}}"#,
        r#"{"model": "m", "worker_plane": {"auth": "hostile"}}"#,
    ] {
        let cfg: Config = serde_json::from_str(bad).unwrap();
        assert!(cfg.validate().is_err(), "the config must refuse: {bad}");
    }
    // An enabled boundary without the [workers] plane never boots.
    let cfg: Config =
        serde_json::from_str(r#"{"model": "m", "worker_plane": {"enabled": true}}"#).unwrap();
    let error = cfg.validate().unwrap_err();
    assert!(error.contains("requires [workers] enabled"), "{error}");

    let base = |extra: &str| {
        format!(
            r#"{{"model": "m", "cloud": {{"enabled": true}}, "workers": {{"enabled": true, "organization": "org_local"}}, "worker_plane": {{"enabled": true{extra}}}}}"#
        )
    };

    // The default bind is loopback: allowed with no acknowledgement.
    let cfg: Config = serde_json::from_str(&base("")).unwrap();
    cfg.validate().unwrap();
    let resolved = cfg.worker_plane.resolve().unwrap().unwrap();
    assert!(resolved.bind.ip().is_loopback());
    assert_eq!(resolved.bind.port(), 8790);
    assert!(!resolved.trusted_gateway);

    // A non-loopback bind without TLS or the gateway acknowledgement is
    // the typed startup refusal NAMING the deployment boundary.
    let cfg: Config = serde_json::from_str(&base(r#", "bind": "0.0.0.0:8790""#)).unwrap();
    let error = cfg.validate().unwrap_err();
    assert!(
        error.contains("worker-plane deployment boundary"),
        "the refusal names the boundary: {error}"
    );
    assert!(
        error.contains("worker_plane_boundary_refused"),
        "the refusal carries its stable code: {error}"
    );

    // Gateway mode admits non-loopback and the acknowledgement is
    // visible in the resolved exposure/audit line.
    let cfg: Config = serde_json::from_str(&base(
        r#", "bind": "0.0.0.0:8790", "trusted_gateway": true"#,
    ))
    .unwrap();
    cfg.validate().unwrap();
    let exposure = cfg
        .worker_plane
        .resolve()
        .unwrap()
        .unwrap()
        .validate()
        .unwrap();
    assert!(exposure.beyond_loopback && exposure.trusted_gateway);
    assert!(exposure.audit_line().contains("trusted_gateway=true"));

    // In-process TLS is refused typed: this build has no inbound TLS
    // stack, so the gateway-only mode is the honest path.
    let cfg: Config = serde_json::from_str(&base(r#", "tls": true"#)).unwrap();
    let error = cfg.validate().unwrap_err();
    assert!(error.contains("no inbound TLS stack"), "{error}");
    assert!(
        error.contains("worker-plane deployment boundary"),
        "{error}"
    );

    // gateway_mtls requires the acknowledgement AND the gateway bearer.
    let cfg: Config = serde_json::from_str(&base(r#", "auth": "gateway_mtls""#)).unwrap();
    assert!(cfg.validate().unwrap_err().contains("gateway_mtls"));
    let cfg: Config = serde_json::from_str(&base(
        r#", "auth": "gateway_mtls", "trusted_gateway": true"#,
    ))
    .unwrap();
    assert!(cfg.validate().unwrap_err().contains("bearer"));
    let cfg: Config = serde_json::from_str(&base(
        r#", "auth": "gateway_mtls", "trusted_gateway": true, "bearer": "gw-secret""#,
    ))
    .unwrap();
    cfg.validate().unwrap();
    let resolved = cfg.worker_plane.resolve().unwrap().unwrap();
    assert_eq!(resolved.auth, faktor_server::WorkerPlaneAuth::GatewayMtls);
    assert_eq!(
        resolved.bearer.as_ref().map(SecretValue::expose),
        Some("gw-secret")
    );
}

/// The `[worker_plane]` transport bearer is a credential: Debug, the
/// saved-config JSON projection and every boundary refusal must never
/// carry the planted bytes, while the resolved config still uses it for
/// the constant-time header check.
#[test]
fn worker_plane_bearer_is_wrapped_and_never_rendered() {
    const PLANTED: &str = "PLANTED-WORKER-PLANE-BEARER-do-not-leak-0123456789";
    let json = format!(
        r#"{{"model": "m", "cloud": {{"enabled": true}}, "workers": {{"enabled": true, "organization": "org_local"}}, "worker_plane": {{"enabled": true, "auth": "gateway_mtls", "trusted_gateway": true, "bearer": "{PLANTED}"}}}}"#
    );
    let cfg: Config = serde_json::from_str(&json).unwrap();
    cfg.validate().unwrap();
    // The parsed field really is the wrapped planted secret.
    let resolved = cfg.worker_plane.resolve().unwrap().unwrap();
    assert_eq!(
        resolved.bearer.as_ref().map(SecretValue::expose),
        Some(PLANTED)
    );
    // Debug of the section and the resolved bind config: redacted.
    for rendered in [
        format!("{:?}", cfg.worker_plane),
        format!("{:?}", resolved),
        format!("{:?}", Some(&cfg.worker_plane)),
    ] {
        assert!(!rendered.contains(PLANTED), "leaked: {rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
    }
    // The saved-config projection never writes the bearer back.
    let saved = serde_json::to_string(&cfg.worker_plane).unwrap();
    assert!(!saved.contains(PLANTED), "saved config leaked: {saved}");
    assert!(!saved.contains("PLANTED"), "{saved}");
    // A refusal on a malformed (oversized) bearer names the shape, never
    // the value.
    let oversized_value = PLANTED.repeat(20);
    let oversized = format!(
        r#"{{"model": "m", "cloud": {{"enabled": true}}, "workers": {{"enabled": true, "organization": "org_local"}}, "worker_plane": {{"enabled": true, "trusted_gateway": true, "bearer": "{oversized_value}"}}}}"#
    );
    let cfg: Config = serde_json::from_str(&oversized).unwrap();
    let err = cfg.validate().unwrap_err();
    assert!(!err.contains(PLANTED), "refusal leaked: {err}");
    assert!(
        err.contains("printable ASCII"),
        "the refusal names the shape: {err}"
    );
}

#[test]
fn worker_node_section_is_additive_strict_and_disabled_by_default() {
    // Absent = disabled: the entry refuses before any effect.
    let cfg = Config::default();
    assert!(!cfg.worker_node.enabled);
    assert!(cfg.worker_node.validate().is_ok());
    // Strict shape: unknown keys, duplicates and wrong types are parse
    // errors; a disabled section imposes nothing else.
    for bad in [
        r#"{"model": "m", "worker_node": {"enabled": false, "hostile": 1}}"#,
        r#"{"model": "m", "worker_node": {"enabled": true, "enabled": true}}"#,
        r#"{"model": "m", "worker_node": {"enabled": "yes"}}"#,
        r#"{"model": "m", "worker_node": {"cpu_cores": -1}}"#,
    ] {
        assert!(
            serde_json::from_str::<Config>(bad).is_err(),
            "the config must refuse: {bad}"
        );
    }
    // An enabled section requires identity, credential, base URL and
    // trust domain; token XOR token_file is enforced at validation.
    let mut node = WorkerNodeCfg {
        enabled: true,
        worker_id: Some("wrk_local".into()),
        control_plane_url: Some("http://127.0.0.1:8787/".into()),
        trust_domain: Some("org_local".into()),
        ..Default::default()
    };
    assert!(
        node.validate().is_err(),
        "an enabled node without a credential is refused"
    );
    node.token = Some("wkr_abc".into());
    node.validate().expect("a complete node validates");
    assert_eq!(
        node.control_plane_url().unwrap(),
        "http://127.0.0.1:8787",
        "the trailing slash is normalized away"
    );
    node.token_payload = Some("/tmp/token".into());
    assert!(
        node.validate().is_err(),
        "an absolute path is not a payload name (and token XOR token_payload is exclusive)"
    );
    node.token = None;
    assert!(
        node.validate().is_err(),
        "token_payload must be a plain payload name"
    );
    node.token_payload = Some("worker.token".into());
    node.validate().expect("a staged payload name validates");
    // The advertisement is normalized and protocol-pinned.
    let capabilities = node.capabilities().unwrap();
    assert_eq!(capabilities.trust_domain, "org_local");
    assert_eq!(
        capabilities.protocol_version,
        faktor_worker::WORKER_PROTOCOL_VERSION
    );
    assert!(
        capabilities.toolchains.is_empty(),
        "an empty advertisement claims no toolchain"
    );
}

#[test]
fn workers_enabled_requires_cloud_org_and_maps_requirements() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = WorkersCfg {
        enabled: true,
        organization: Some("org_local".into()),
        ..Default::default()
    };
    assert_eq!(
        w.trust_domain().unwrap(),
        "org_local",
        "default = organization"
    );
    w.trust_domain = Some("Eu-1".into());
    assert_eq!(w.trust_domain().unwrap(), "eu-1", "normalized");
    w.toolchains = vec!["Rust".into(), "rust".into(), "Node".into()];
    w.network = Some("full".into());
    w.min_cpu_cores = 4;
    let req = w.requirements().unwrap();
    assert_eq!(req.toolchains, vec!["node", "rust"], "normalized + deduped");
    assert_eq!(req.network, Some(faktor_worker::NetworkProfile::Full));
    assert_eq!(req.trust_domain, "eu-1");
    w.network = Some("hostile".into());
    assert!(w.requirements().is_err(), "unknown network profile refused");
    w.network = None;
    assert!(w.workers_path(dir.path()).unwrap().is_some());
    // An enabled section without [cloud] never boots.
    let cfg: Config = serde_json::from_str(
        r#"{"model": "m", "workers": {"enabled": true, "organization": "org_local"}}"#,
    )
    .unwrap();
    assert!(cfg.validate().is_err());
    // With [cloud] enabled the pair validates.
    let cfg: Config = serde_json::from_str(
        r#"{"model": "m", "cloud": {"enabled": true}, "workers": {"enabled": true, "organization": "org_local"}}"#,
    )
    .unwrap();
    cfg.validate().unwrap();
}

#[test]
fn enterprise_section_is_additive_strict_and_disabled_by_default() {
    // Absent = disabled, no database path, no layers.
    let cfg = Config::default();
    assert!(!cfg.enterprise.enabled);
    assert!(cfg
        .enterprise
        .enterprise_path(std::path::Path::new("/tmp"))
        .unwrap()
        .is_none());
    assert!(cfg.enterprise.organization().unwrap().is_none());
    assert!(cfg.enterprise.layers().unwrap().is_empty());

    // Strict shape: unknown keys, duplicates and wrong types are parse
    // errors on both load paths.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.json");
    for bad in [
        r#"{"model": "m", "enterprise": {"enabled": false, "hostile": 1}}"#,
        r#"{"model": "m", "enterprise": {"enabled": true, "enabled": true}}"#,
        r#"{"model": "m", "enterprise": {"enabled": "yes"}}"#,
        r#"{"model": "m", "enterprise": {"policy": {"hostile": ["x"]}}}"#,
        r#"{"model": "m", "enterprise": {"policy": {"network": "none"}}}"#,
        r#"{"model": "m", "enterprise": {"preferences": {"network": ["none"]}}}"#,
    ] {
        std::fs::write(&path, bad).unwrap();
        assert!(
            Config::load(&path).is_err(),
            "the config must refuse: {bad}"
        );
        assert!(
            Config::load_strict(&path).is_err(),
            "the strict load must refuse: {bad}"
        );
    }

    // Path traversal in the database name is refused even while
    // disabled; an enabled section without [cloud] never boots.
    let cfg: Config =
        serde_json::from_str(r#"{"model": "m", "enterprise": {"database": "../e.db"}}"#).unwrap();
    assert!(cfg.validate().is_err());
    let cfg: Config = serde_json::from_str(
        r#"{"model": "m", "enterprise": {"enabled": true, "organization": "org_local"}}"#,
    )
    .unwrap();
    assert!(cfg.validate().is_err(), "enterprise requires [cloud]");
    let cfg: Config = serde_json::from_str(
        r#"{"model": "m", "cloud": {"enabled": true}, "enterprise": {"enabled": true, "organization": "org_local"}}"#,
    )
    .unwrap();
    cfg.validate().unwrap();
    let resolved = cfg.enterprise.enterprise_path(dir.path()).unwrap().unwrap();
    assert!(resolved.ends_with("enterprise.db"));
}

#[test]
fn enterprise_layers_are_policy_vs_preference_and_loosening_is_refused() {
    // A preference outside the configured policy is refused by the ONE
    // resolver (the same one the server route uses).
    let cfg: Config = serde_json::from_str(
        r#"{
            "model": "m",
            "cloud": {"enabled": true},
            "enterprise": {
                "enabled": true,
                "organization": "org_local",
                "policy": {"network": ["none"]},
                "preferences": {"network": "provider"}
            }
        }"#,
    )
    .unwrap();
    assert!(
        cfg.validate().is_err(),
        "a preference outside policy must refuse AT LOAD (no enterprise DB created)"
    );
    let layers = cfg.enterprise.layers().unwrap();
    let error = faktor_cloud::resolve_layers(&layers).unwrap_err();
    assert!(
        matches!(
            error,
            faktor_cloud::LayeredConfigError::PreferenceRefusedByPolicy { .. }
        ),
        "{error}"
    );

    // A satisfied preference resolves with an attributable digest; the
    // digest changes when any layer changes.
    let cfg: Config = serde_json::from_str(
        r#"{
            "model": "m",
            "cloud": {"enabled": true},
            "enterprise": {
                "enabled": true,
                "organization": "org_local",
                "policy": {"network": ["none", "provider"], "providers": ["anthropic"]},
                "preferences": {"network": "none", "providers": "anthropic"}
            }
        }"#,
    )
    .unwrap();
    let effective = faktor_cloud::resolve_layers(&cfg.enterprise.layers().unwrap()).unwrap();
    assert!(effective.policy_allows(faktor_cloud::ConfigKey::Network, "none"));
    let digest = effective.digest.clone();
    let mut changed = cfg.enterprise.clone();
    changed.policy.providers = Some(vec!["anthropic".into(), "openai".into()]);
    let changed = faktor_cloud::resolve_layers(&changed.layers().unwrap()).unwrap();
    assert_ne!(changed.digest, digest);
}
