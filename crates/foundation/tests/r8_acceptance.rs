//! Revision 8 acceptance, daemon half (A01–A40).
//!
//! These tests assert the FOUNDATION-owned facts behind the acceptance
//! scenarios: configuration atomicity, capability authority, tuning unset vs
//! zero, secret non-export, loopback policy, read-only preset admission,
//! context arithmetic, MCP bounds, executable trust, immutable admission
//! binding and instruction/rule precedence. The host-UI half is exercised
//! by the IDE agents; the orchestrator/session halves live in their own
//! suites (A14/A15/A16/A17/A19/A25–A28/A32–A36/A38–A40).

use std::collections::BTreeMap;

use faktor_core::model::{CapabilityAuthority, ModelCapabilities};
use faktor_foundation::{
    compile_effective_request, tuning_descriptor, Activation, Basis, BrowserAdapterDraft,
    BrowserMode, Change, ConnectionDraft, ContextStore, DelegationPolicy, Foundation,
    FoundationError, InstructionDraft, InstructionKind, InstructionOrigin, Manifest,
    McpServerDraft, Patch, PresetDraft, PresetMode, PresetOrigin, Scope, Source,
};
use serde_json::json;

fn open() -> (tempfile::TempDir, Foundation) {
    let dir = tempfile::tempdir().unwrap();
    let foundation = Foundation::open(dir.path().join("foundation.db")).unwrap();
    (dir, foundation)
}

fn instruction(title: &str, origin: InstructionOrigin, body: &str) -> InstructionDraft {
    InstructionDraft {
        id: None,
        kind: InstructionKind::Instruction,
        title: title.into(),
        content: body.into(),
        origin,
        scope: match origin {
            InstructionOrigin::Project => Scope::Project,
            InstructionOrigin::Managed => Scope::Managed,
            _ => Scope::User,
        },
        activation: Activation::Always,
        globs: Vec::new(),
        enabled: true,
    }
}

fn preset(name: &str, mode: PresetMode) -> PresetDraft {
    PresetDraft {
        id: None,
        mode,
        name: name.into(),
        description: String::new(),
        origin: PresetOrigin::User,
        scope: Scope::User,
        role_instructions: "read carefully".into(),
        model_provider: None,
        model_id: None,
        reasoning_effort: None,
        tool_allow: Vec::new(),
        tool_deny: Vec::new(),
        delegation: DelegationPolicy::default(),
        child_concurrency: 0,
        budget_share_ppm: 500_000,
        role_models: std::collections::BTreeMap::new(),
        fallback_chain: Vec::new(),
    }
}

fn manifest(session: u64, tokens: &[u64], capacity: u64) -> Manifest {
    Manifest {
        session_id: session,
        revision: 0,
        operation_id: "op".into(),
        call_id: None,
        model: "m".into(),
        capacity,
        input: Some(tokens.iter().sum()),
        output_reserve: 4_096,
        safety_reserve: 2_048,
        basis: Basis::Estimated {
            method: "test-estimator".into(),
            version: "1".into(),
        },
        sources: tokens
            .iter()
            .enumerate()
            .map(|(index, tokens)| Source {
                id: format!("s{index}"),
                uri: format!("file:///w/{index}.rs"),
                range: Some("1-10".into()),
                version: "v1".into(),
                content_hash: format!("h{index}"),
                category: "files".into(),
                tokens: Some(*tokens),
                bytes: Some(100),
                inclusion_reason: "explicit".into(),
            })
            .collect(),
        created_ms: 0,
    }
}

#[test]
fn a01_a04_config_and_capability_authority_are_exact() {
    let (_dir, foundation) = open();
    // A37/A01 daemon half: one invalid member refuses the ENTIRE patch, so
    // neither default can advance independently.
    let before = foundation.config.snapshot().unwrap();
    let bad = foundation.config.commit(&Patch {
        expected_revision: before.revision,
        changes: vec![
            Change {
                key: "faktor.defaultProvider".into(),
                scope: Scope::User,
                value: Some(json!("fake")),
            },
            Change {
                key: "faktor.defaultModel".into(),
                scope: Scope::User,
                value: Some(json!(7)),
            },
        ],
    });
    assert!(bad.is_err());
    let after = foundation.config.snapshot().unwrap();
    assert_eq!(after.revision, before.revision);
    assert_eq!(
        after.values["faktor.defaultProvider"].value, before.values["faktor.defaultProvider"].value,
        "the provider default did not change when the model change was refused"
    );

    // A02/A03/A04: authority gates feature parameters; unset and zero differ.
    let caps = ModelCapabilities {
        tools: true,
        ..Default::default()
    };
    let descriptor = tuning_descriptor("fake", "m", &caps, CapabilityAuthority::Known, true);
    let absent = compile_effective_request(&descriptor, &BTreeMap::new(), 1).unwrap();
    assert!(absent.inherited.contains(&"temperature".to_string()));
    let zero = compile_effective_request(
        &descriptor,
        &BTreeMap::from([("temperature".to_string(), json!(0.0))]),
        1,
    )
    .unwrap();
    assert_eq!(zero.parameters["temperature"].value, json!(0.0));
    assert!(!zero.inherited.contains(&"temperature".to_string()));
    let unknown = tuning_descriptor("fake", "m", &caps, CapabilityAuthority::Unknown, true);
    let refused = compile_effective_request(
        &unknown,
        &BTreeMap::from([("topK".to_string(), json!(10))]),
        1,
    );
    assert!(
        refused.is_err(),
        "unknown authority cannot enable gated parameters"
    );
}

#[test]
fn a06_a07_secrets_never_export_and_lan_endpoints_are_refused() {
    let (dir, foundation) = open();
    let secret = b"sk-live-must-not-leak".to_vec();
    let reference = foundation
        .vault
        .store("provider", "openai", &secret)
        .unwrap();
    assert_eq!(foundation.vault.reveal(&reference.handle).unwrap(), secret);
    let export = foundation.config.export().unwrap();
    let encoded = serde_json::to_string(&export).unwrap();
    assert!(!encoded.contains("sk-live-must-not-leak"));
    assert!(!encoded.contains(&reference.handle));

    // A07: private LAN and metadata endpoints are refused before persistence.
    for endpoint in [
        "http://192.168.1.5:8080",
        "http://10.0.0.1",
        "http://169.254.169.254/latest",
    ] {
        let draft = ConnectionDraft {
            id: "lan".into(),
            family: "openai".into(),
            label: "LAN".into(),
            endpoint: Some(endpoint.into()),
            credential_ref: None,
            models: Vec::new(),
            allow_loopback: Some(true),
            api_flavor: None,
            trust_origin: None,
            connect_timeout_ms: None,
            first_byte_timeout_ms: None,
            total_timeout_ms: None,
            keep_alive: None,
            proxy_url: None,
            tls_mode: None,
            ca_cert_ref: None,
        };
        assert!(
            foundation.providers.save(&draft, None).is_err(),
            "{endpoint} must be refused"
        );
    }
    // A08: loopback needs the explicit flag (the flag is the authority, not
    // the host spelling).
    let mut loopback = ConnectionDraft {
        id: "local".into(),
        family: "openai".into(),
        label: "Local".into(),
        endpoint: Some("http://127.0.0.1:11434".into()),
        credential_ref: None,
        models: Vec::new(),
        allow_loopback: None,
        api_flavor: None,
        trust_origin: None,
        connect_timeout_ms: None,
        first_byte_timeout_ms: None,
        total_timeout_ms: None,
        keep_alive: None,
        proxy_url: None,
        tls_mode: None,
        ca_cert_ref: None,
    };
    assert!(foundation.providers.save(&loopback, None).is_err());
    loopback.allow_loopback = Some(true);
    assert!(foundation.providers.save(&loopback, None).is_ok());
    let _ = dir;
}

#[test]
fn a09_a10_presets_enforce_read_only_and_never_certify_unknown_tools() {
    let (_dir, foundation) = open();
    let mut ask = preset("Ask", PresetMode::Ask);
    ask.id = Some("ask".into());
    ask.tool_allow = vec!["read".into(), "mcp:future".into()];
    ask.tool_deny = vec!["run".into()];
    foundation.presets.upsert(&ask, None).unwrap();
    let envelope = foundation
        .presets
        .envelope_with("ask", &["read".into(), "run".into()])
        .unwrap();
    assert!(!envelope.admission_mutation_allowed);
    assert!(envelope
        .tools
        .resolved
        .iter()
        .any(|tool| tool.name == "mcp:future"
            && !tool.offered
            && tool.reason.as_deref() == Some("tool is not registered with this daemon")));
    assert!(envelope
        .tools
        .resolved
        .iter()
        .any(|tool| tool.name == "run" && tool.denied));
    let refusal = foundation
        .presets
        .admission_refusal(&envelope, true)
        .expect("Ask refuses mutation");
    assert!(matches!(refusal, FoundationError::Validation(_)));
    assert!(foundation
        .presets
        .admission_refusal(&envelope, false)
        .is_none());

    // A10: preset text/roles never grant a capability the policy doesn't.
    let mut code = preset("Code", PresetMode::Code);
    code.id = Some("code".into());
    code.tool_allow = vec!["run".into()];
    foundation.presets.upsert(&code, None).unwrap();
    let code_env = foundation
        .presets
        .envelope_with("code", &["run".into()])
        .unwrap();
    assert_eq!(
        code_env.tools.dispatch_enforcement,
        "admission_and_permission_policy"
    );
}

#[test]
fn a11_a12_a13_context_null_arithmetic_and_source_identity() {
    let (_dir, foundation) = open();
    let mut unknown = manifest(1, &[100], 32_768);
    unknown.sources[0].tokens = None;
    unknown.input = None;
    let usage = unknown.usage();
    assert_eq!(usage.input, None, "unknown is not zero");
    assert_eq!(usage.headroom, None);
    assert!(!usage.measured && usage.estimated);

    let known = manifest(2, &[18_400, 8_224], 32_768);
    let usage = known.usage();
    assert_eq!(usage.input, Some(26_624));
    assert_eq!(usage.categories[0].tokens, 26_624);

    let mut duplicate = manifest(3, &[10, 20], 32_768);
    duplicate.sources[1].id = duplicate.sources[0].id.clone();
    assert!(duplicate.validate().is_err());
    let store = ContextStore::new(foundation.db.clone());
    assert!(store.capture(duplicate).is_err());
}

#[test]
fn a21_a23_mcp_bounds_and_exact_path_trust() {
    let (dir, foundation) = open();
    let mut mcp = McpServerDraft {
        id: Some("srv".into()),
        name: "docs".into(),
        command: "node".into(),
        args: vec!["server.js".into()],
        env: Vec::new(),
        trust_origin: Some("user".into()),
        trusted: Some(true),
        enabled: Some(true),
        transport: None,
        endpoint: None,
        timeout_ms: None,
        output_cap_bytes: None,
        stderr_cap_bytes: None,
    };
    mcp.args = (0..33).map(|i| format!("a{i}")).collect();
    assert!(matches!(
        foundation.customization.save_mcp_server(&mcp, None),
        Err(FoundationError::Validation(errors))
            if errors.iter().any(|e| e.key == "args" && e.code == "oversized")
    ));
    mcp.args = vec!["server.js".into()];
    foundation
        .customization
        .save_mcp_server(&mcp, None)
        .unwrap();

    let exe = dir.path().join("daemon");
    std::fs::write(&exe, b"v1").unwrap();
    let path = exe.to_string_lossy().to_string();
    let record = foundation
        .runtime_trust
        .confirm(&path, Some("release"))
        .unwrap();
    assert_eq!(record.signature_state, "unverified");
    assert_eq!(
        foundation.runtime_trust.check(&path).state,
        faktor_foundation::TrustState::Match
    );
    std::fs::write(&exe, b"tampered").unwrap();
    assert_eq!(
        foundation.runtime_trust.check(&path).state,
        faktor_foundation::TrustState::Mismatch
    );
}

#[test]
fn a29_a37_bindings_and_instruction_precedence_are_immutable() {
    let (_dir, foundation) = open();
    let mut ask = preset("Ask", PresetMode::Ask);
    ask.id = Some("ask".into());
    foundation.presets.upsert(&ask, None).unwrap();
    let envelope = foundation.presets.envelope_with("ask", &[]).unwrap();
    let first = foundation.presets.bind(7, "sub", &envelope).unwrap();
    assert_eq!(foundation.presets.bind(7, "sub", &envelope).unwrap(), first);
    let mut code = preset("Code", PresetMode::Code);
    code.id = Some("code".into());
    foundation.presets.upsert(&code, None).unwrap();
    let code_env = foundation.presets.envelope_with("code", &[]).unwrap();
    assert!(matches!(
        foundation.presets.bind(7, "sub", &code_env),
        Err(FoundationError::Conflict(_))
    ));

    foundation
        .instructions
        .upsert(&instruction("Style", InstructionOrigin::User, "user"), None)
        .unwrap();
    foundation
        .instructions
        .upsert(
            &instruction("Style", InstructionOrigin::Project, "project"),
            None,
        )
        .unwrap();
    let bundle = foundation.instructions.preview("", &[], &[]).unwrap();
    assert!(bundle.content.contains("project"));
    assert!(!bundle.content.contains("user"));
    assert_eq!(bundle.conflicts.len(), 1);
    let binding = foundation.instructions.bind(7, "op", &bundle).unwrap();
    assert_eq!(binding.revision, 1);
    assert_eq!(
        foundation
            .instructions
            .bind(7, "op", &bundle)
            .unwrap()
            .revision,
        1
    );
}

#[test]
fn browser_adapters_cannot_be_advertised_over_the_wire_shape() {
    let (_dir, foundation) = open();
    assert!(foundation.browser.adapters().unwrap().is_empty());
    // The service API requires an explicit advertised flag; an unadvertised
    // adapter still refuses session requests.
    foundation
        .browser
        .upsert_adapter(&BrowserAdapterDraft {
            id: "b".into(),
            mode: BrowserMode::Local,
            endpoint: None,
            account_scope: None,
            advertised: false,
            network_scope: "loopback_only".into(),
        })
        .unwrap();
    assert!(foundation
        .browser
        .request_session("b", 1, "https://example.com")
        .is_err());
}
