//! Adversarial contracts for the Rev8 field-level closures: persistence,
//! validation, scoping and idempotency of presets, provider transport
//! options, MCP auth, review comments and session settings.

use std::collections::BTreeMap;

use faktor_core::id::SessionId;
use faktor_foundation::customization::McpServerDraft;
use faktor_foundation::presets::{
    FallbackCandidate, PresetDraft, PresetMode, PresetOrigin, RoleModel,
};
use faktor_foundation::providers::ConnectionDraft;
use faktor_foundation::review::{ReviewCommentDraft, ReviewCommentPatch};
use faktor_foundation::session_settings::{
    SessionSettingsService, COMPACTION_MODEL_KEY, MAX_CONSECUTIVE_FAILURES_KEY, MAX_TURNS_KEY,
};
use faktor_foundation::{Foundation, FoundationError, Scope};

fn foundation() -> (tempfile::TempDir, Foundation) {
    let dir = tempfile::tempdir().unwrap();
    let foundation = Foundation::open(dir.path()).unwrap();
    (dir, foundation)
}

fn connection(id: &str) -> ConnectionDraft {
    ConnectionDraft {
        id: id.into(),
        family: "ollama".into(),
        label: "local".into(),
        endpoint: Some("http://127.0.0.1:11434".into()),
        credential_ref: None,
        models: vec!["llama3:8b".into()],
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
    }
}

#[test]
fn provider_transport_options_are_validated_persisted_and_never_hold_proxy_credentials() {
    let (_dir, f) = foundation();
    let mut draft = connection("c1");
    draft.connect_timeout_ms = Some(1_500);
    draft.first_byte_timeout_ms = Some(2_500);
    draft.total_timeout_ms = Some(9_000);
    draft.keep_alive = Some("30m".into());
    let saved = f.providers.save(&draft, None).unwrap();
    assert_eq!(saved.connect_timeout_ms, Some(1_500));
    assert_eq!(saved.first_byte_timeout_ms, Some(2_500));
    assert_eq!(saved.total_timeout_ms, Some(9_000));
    assert_eq!(saved.keep_alive.as_deref(), Some("30m"));
    assert_eq!(saved.tls_mode, "system");

    let mut bad = connection("c2");
    bad.connect_timeout_ms = Some(1);
    assert!(matches!(
        f.providers.save(&bad, None),
        Err(FoundationError::Validation(_))
    ));
    let mut bad = connection("c3");
    bad.total_timeout_ms = Some(500);
    bad.first_byte_timeout_ms = Some(900);
    assert!(f.providers.save(&bad, None).is_err());
    let mut bad = connection("c4");
    bad.keep_alive = Some("5 minutes".into());
    assert!(f.providers.save(&bad, None).is_err());
    let mut bad = connection("c5");
    bad.proxy_url = Some("http://user:pw@proxy.example:8080".into());
    assert!(f.providers.save(&bad, None).is_err());
    let mut bad = connection("c6");
    bad.tls_mode = Some("insecure".into());
    assert!(f.providers.save(&bad, None).is_err());
    let mut bad = connection("c7");
    bad.tls_mode = Some("pinned_ca".into());
    bad.ca_cert_ref = Some("file:///etc/ca.pem".into());
    assert!(f.providers.save(&bad, None).is_err());
}

#[test]
fn preset_roles_and_fallback_are_ordered_atomic_and_revision_fenced() {
    let (_dir, f) = foundation();
    let draft = PresetDraft {
        id: Some("p1".into()),
        mode: PresetMode::Code,
        name: "Coder".into(),
        description: String::new(),
        origin: PresetOrigin::User,
        scope: Scope::User,
        role_instructions: "code".into(),
        model_provider: Some("ollama".into()),
        model_id: Some("llama3:8b".into()),
        reasoning_effort: None,
        tool_allow: Vec::new(),
        tool_deny: Vec::new(),
        delegation: Default::default(),
        child_concurrency: 0,
        budget_share_ppm: 500_000,
        role_models: BTreeMap::new(),
        fallback_chain: Vec::new(),
    };
    let record = f.presets.upsert(&draft, Some(0)).unwrap();
    let mut roles = BTreeMap::new();
    roles.insert(
        "coder".to_string(),
        RoleModel {
            provider: "ollama".into(),
            model: "qwen2.5-coder:7b".into(),
            reasoning_effort: Some("high".into()),
        },
    );
    let updated = f
        .presets
        .set_role_models("p1", roles.clone(), Some(record.revision))
        .unwrap();
    assert_eq!(updated.role_models, roles);
    // A stale revision is a typed conflict, never a partial write.
    assert!(matches!(
        f.presets
            .set_role_models("p1", roles.clone(), Some(record.revision)),
        Err(FoundationError::Conflict(_))
    ));
    let mut unknown = BTreeMap::new();
    unknown.insert(
        "ceo".to_string(),
        RoleModel {
            provider: "ollama".into(),
            model: "x".into(),
            reasoning_effort: None,
        },
    );
    assert!(f.presets.set_role_models("p1", unknown, None).is_err());

    let chain = vec![
        FallbackCandidate {
            provider: "ollama".into(),
            model: "a".into(),
            reason: Some("first".into()),
            quality_floor_ppm: Some(800_000),
        },
        FallbackCandidate {
            provider: "ollama".into(),
            model: "b".into(),
            reason: None,
            quality_floor_ppm: None,
        },
    ];
    let updated = f
        .presets
        .set_fallback_chain("p1", chain.clone(), Some(updated.revision))
        .unwrap();
    assert_eq!(updated.fallback_chain, chain);
    let duplicate = vec![chain[0].clone(), chain[0].clone()];
    assert!(f.presets.set_fallback_chain("p1", duplicate, None).is_err());
    let envelope = f.presets.envelope_with("p1", &[]).unwrap();
    assert_eq!(envelope.role_models, roles);
    assert_eq!(envelope.fallback_chain.len(), 2);
}

fn remote_mcp() -> McpServerDraft {
    McpServerDraft {
        id: Some("m1".into()),
        name: "Remote".into(),
        command: String::new(),
        args: vec![],
        env: vec![],
        trust_origin: None,
        trusted: Some(true),
        enabled: Some(true),
        transport: Some("streamable_http".into()),
        endpoint: Some("https://mcp.example.com/rpc".into()),
        timeout_ms: Some(5_000),
        output_cap_bytes: Some(1_048_576),
        stderr_cap_bytes: Some(65_536),
    }
}

#[test]
fn mcp_auth_is_vault_backed_remote_only_and_revocable() {
    let (_dir, f) = foundation();
    let record = f
        .customization
        .save_mcp_server(&remote_mcp(), None)
        .unwrap();
    assert_eq!(record.transport, "streamable_http");
    assert_eq!(record.auth_state, "signed_out");
    assert_eq!(record.timeout_ms, Some(5_000));
    let secret = f
        .vault
        .store("mcp_oauth", "token", b"sk-live-token")
        .unwrap();
    let authorized = f
        .customization
        .set_mcp_auth(
            "m1",
            Some(secret.handle.clone()),
            Some("acct@example.com".into()),
            vec!["tools.read".into()],
            Some(record.revision),
        )
        .unwrap();
    assert_eq!(authorized.auth_state, "authorized");
    assert_eq!(authorized.auth_account.as_deref(), Some("acct@example.com"));
    assert!(authorized.auth_token_ref.is_some());
    assert!(!serde_json::to_string(&authorized)
        .unwrap()
        .contains("sk-live-token"));
    // A foreign/non-vault reference is refused.
    assert!(f
        .customization
        .set_mcp_auth("m1", Some("sk-live-token".into()), None, vec![], None)
        .is_err());
    let revoked = f
        .customization
        .set_mcp_auth("m1", None, None, vec![], None)
        .unwrap();
    assert_eq!(revoked.auth_state, "signed_out");
    assert!(revoked.auth_token_ref.is_none());
    // Stdio servers have no OAuth binding.
    let mut stdio = remote_mcp();
    stdio.id = Some("m2".into());
    stdio.transport = Some("stdio".into());
    stdio.endpoint = None;
    stdio.command = "npx".into();
    f.customization.save_mcp_server(&stdio, None).unwrap();
    assert!(f
        .customization
        .set_mcp_auth("m2", None, None, vec![], None)
        .is_err());
    // Bounds are validated before persistence.
    let mut over = remote_mcp();
    over.id = Some("m3".into());
    over.output_cap_bytes = Some(1);
    assert!(f.customization.save_mcp_server(&over, None).is_err());
}

fn comment(_session: u64, path: &str) -> ReviewCommentDraft {
    ReviewCommentDraft {
        id: None,
        task_id: None,
        change_id: Some("cs-1".into()),
        path: path.into(),
        line_start: 3,
        line_end: 4,
        side: "new".into(),
        body: "consider a guard".into(),
        author: Some("reviewer".into()),
        content_hash: None,
        resolved: Some(false),
    }
}

#[test]
fn review_comments_are_session_scoped_revisioned_and_reject_hostile_paths() {
    let (_dir, f) = foundation();
    let created = f.review.create(7, &comment(7, "src/lib.rs")).unwrap();
    assert_eq!(created.session_id, 7);
    assert_eq!(
        f.review
            .list(7, None, None, 50)
            .unwrap()
            .iter()
            .map(|c| c.id.clone())
            .collect::<Vec<_>>(),
        vec![created.id.clone()]
    );
    assert!(f.review.list(8, None, None, 50).unwrap().is_empty());
    assert!(f.review.get(8, &created.id).unwrap().is_none());
    assert!(f.review.create(7, &comment(7, "../escape")).is_err());
    assert!(f.review.create(7, &comment(7, "/abs")).is_err());
    let patched = f
        .review
        .update(
            7,
            &created.id,
            Some(created.revision),
            &ReviewCommentPatch {
                body: None,
                resolved: Some(true),
            },
        )
        .unwrap();
    assert!(patched.resolved);
    assert!(f
        .review
        .update(
            7,
            &created.id,
            Some(created.revision),
            &ReviewCommentPatch {
                body: None,
                resolved: Some(false),
            },
        )
        .is_err());
    assert!(f
        .review
        .update(
            8,
            &created.id,
            None,
            &ReviewCommentPatch {
                body: None,
                resolved: Some(true)
            }
        )
        .is_err());
    f.review
        .delete(7, &created.id, Some(patched.revision))
        .unwrap();
    assert!(f.review.get(7, &created.id).unwrap().is_none());
    // Replay of the deleted id is a typed conflict, not a silent recreate.
    assert!(matches!(
        f.review.delete(7, &created.id, None),
        Err(FoundationError::NotFound(_))
    ));
}

#[test]
fn session_settings_are_bounded_and_change_receipts_are_durable() {
    let (_dir, f) = foundation();
    let session = SessionId::new(42);
    let settings = SessionSettingsService::new(f.db.clone());
    assert!(settings
        .set(42, COMPACTION_MODEL_KEY, &serde_json::json!("x"))
        .is_ok());
    assert!(settings
        .set(42, MAX_TURNS_KEY, &serde_json::json!(0))
        .is_err());
    assert!(settings
        .set(42, MAX_CONSECUTIVE_FAILURES_KEY, &serde_json::json!(1))
        .is_err());
    assert!(settings.set(42, "unknown", &serde_json::json!(1)).is_err());
    settings
        .set(42, MAX_TURNS_KEY, &serde_json::json!(25))
        .unwrap();
    assert_eq!(
        settings.get(42, MAX_TURNS_KEY).unwrap(),
        Some(serde_json::json!(25))
    );
    assert_eq!(settings.get(43, MAX_TURNS_KEY).unwrap(), None);

    let receipt = serde_json::json!({"accepted": true, "changeSetId": "cs-1"});
    settings
        .put_change_receipt(42, "cs-1", "digest-a", &receipt)
        .unwrap();
    // DO NOTHING preserves the first durable receipt (idempotent replay).
    settings
        .put_change_receipt(
            42,
            "cs-1",
            "digest-a",
            &serde_json::json!({"accepted": false}),
        )
        .unwrap();
    let (digest, stored) = settings.change_receipt(42, "cs-1").unwrap().unwrap();
    assert_eq!(digest, "digest-a");
    assert_eq!(stored, receipt);
    assert!(settings
        .change_receipt(session.raw(), "missing")
        .unwrap()
        .is_none());
}
