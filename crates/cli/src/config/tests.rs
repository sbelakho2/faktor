use super::*;

#[test]
fn tasks_section_defaults_shadow_and_rejects_the_removed_direct_mode() {
    // P0 mutation isolation: absent [tasks] keeps the PRODUCT default —
    // shadow mutation ON (`MutationMode::Shadow`); the removed
    // `direct_compat` value and the legacy `shadow_mutation = false`
    // value are STRICT parse errors naming the removal on both load
    // paths; the legacy boolean key keeps its historical `true` meaning
    // only; specifying BOTH keys (or unknown keys / bad values) is a
    // parse error.
    let cfg = Config::default();
    assert_eq!(
        cfg.tasks.mutation_mode,
        MutationMode::Shadow,
        "shadow mutation is the production default"
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.json");
    // The default round-trips through the daemon's own file shape.
    cfg.save(&path).unwrap();
    let loaded = Config::load(&path).unwrap();
    assert_eq!(loaded.tasks, cfg.tasks);
    for (body, expected) in [
        (
            r#"{"tasks": {"mutation_mode": "shadow"}}"#,
            MutationMode::Shadow,
        ),
        // Legacy alias: the pre-wave-24 boolean key keeps its historical
        // `true` meaning.
        (
            r#"{"tasks": {"shadow_mutation": true}}"#,
            MutationMode::Shadow,
        ),
    ] {
        std::fs::write(&path, body).unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.tasks.mutation_mode, expected, "{body}");
        let strict = Config::load_strict(&path).unwrap();
        assert_eq!(strict.tasks.mutation_mode, expected, "{body}");
    }
    // Partial objects keep the per-key default (Shadow).
    std::fs::write(&path, r#"{"tasks": {}}"#).unwrap();
    assert_eq!(
        Config::load(&path).unwrap().tasks.mutation_mode,
        MutationMode::Shadow
    );
    // The removed direct-owner request is a strict parse error whose
    // message NAMES the removal, on BOTH load paths.
    for body in [
        r#"{"tasks": {"mutation_mode": "direct_compat"}}"#,
        r#"{"tasks": {"shadow_mutation": false}}"#,
    ] {
        std::fs::write(&path, body).unwrap();
        let e = Config::load(&path).expect_err("the removed direct mode must fail");
        assert!(e.contains("removed"), "{body}: {e}");
        let e = Config::load_strict(&path).expect_err("strict load must fail");
        assert!(e.contains("removed"), "{body}: {e}");
    }
    for bad in [
        // A file never says two different things at once.
        r#"{"tasks": {"mutation_mode": "shadow", "shadow_mutation": true}}"#,
        r#"{"tasks": {"mutation_mode": "direct_compat", "shadow_mutation": false}}"#,
        r#"{"tasks": {"shadow_mutation": true, "bogus": 1}}"#,
        r#"{"tasks": {"shadow_mutation": "yes"}}"#,
        r#"{"tasks": {"mutation_mode": "nonsense"}}"#,
        r#"{"tasks": {"mutation_mode": "Shadow"}}"#,
        r#"{"task": {"mutation_mode": "shadow"}}"#,
    ] {
        std::fs::write(&path, bad).unwrap();
        let e = Config::load(&path).expect_err("hostile [tasks] must fail");
        assert!(
            e.contains("unknown field")
                || e.contains("invalid type")
                || e.contains("unknown variant")
                || e.contains("cannot both be present")
                || e.contains("removed"),
            "{e}"
        );
        assert!(Config::load_strict(&path).is_err());
    }
}

#[test]
fn config_roundtrip_and_defaults() {
    let cfg = Config::default();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("faktor-plus.json");
    cfg.save(&path).unwrap();
    let loaded = Config::load(&path).unwrap();
    assert_eq!(loaded.model, cfg.model);
    assert_eq!(loaded.compact_at_usage, 0.65);
    assert!(loaded.providers.is_empty());
}

/// The additive `[embeddings]` section: strict map-only parsing, the
/// strict/lenient load paths, and the registry-backed selection policy
/// (`required` refuses, `best_effort` degrades to no embedder).
#[test]
fn embeddings_section_is_strict_and_policy_gated() {
    use faktor_provider::{EmbeddingResponse, FakeProvider};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("emb.json");
    assert!(
        Config::default().embeddings.is_none(),
        "an absent section keeps no embedder"
    );
    let cfg = Config {
        embeddings: Some(EmbeddingCfg {
            provider: "embed-me".into(),
            model: "emb-1".into(),
            policy: EmbeddingPolicy::Required,
        }),
        ..Config::default()
    };
    cfg.save(&path).unwrap();
    let loaded = Config::load_strict(&path).unwrap();
    assert_eq!(
        loaded.embeddings, cfg.embeddings,
        "the selection round-trips"
    );

    // Strict parsing: unknown/duplicate keys, wrong types, missing
    // members and positional arrays are refused by BOTH load paths.
    for bad in [
        r#"{"embeddings": {"model": "m"}}"#,
        r#"{"embeddings": {"provider": "p", "model": "m", "bogus": 1}}"#,
        r#"{"embeddings": {"provider": "p", "model": "m", "policy": "sometimes"}}"#,
        r#"{"embeddings": {"provider": "p", "model": "m", "policy": true}}"#,
        r#"{"embeddings": {"provider": "p", "provider": "q", "model": "m"}}"#,
        r#"{"embeddings": ["p", "m"]}"#,
    ] {
        std::fs::write(&path, bad).unwrap();
        assert!(
            Config::load(&path).is_err(),
            "hostile [embeddings] must fail: {bad}"
        );
        assert!(Config::load_strict(&path).is_err(), "{bad}");
    }
    // Empty ids parse but are refused by semantic validation.
    std::fs::write(&path, r#"{"embeddings": {"provider": "", "model": "m"}}"#).unwrap();
    assert!(Config::load(&path).is_ok());
    assert!(Config::load_strict(&path).is_err());

    // Registry resolution.
    let retry = faktor_core::retry::RetryPolicy::default();
    let mut registry = faktor_provider::ProviderRegistry::new();
    let fake = Arc::new(
        FakeProvider::new("embed-me", ModelCapabilities::default()).with_embeddings(
            "emb-1",
            vec![Ok(EmbeddingResponse::new(vec![vec![0.5, 0.25]]).unwrap())],
        ),
    );
    registry.try_register(fake.clone()).unwrap();
    let resolved = cfg
        .semantic_embedder(&registry, &retry)
        .unwrap()
        .expect("a capable configured provider resolves");
    let vectors = resolved.try_embed(&["hello".into()]).unwrap();
    assert_eq!(vectors, vec![vec![0.5, 0.25]]);
    assert_eq!(fake.embedding_requests()[0].model, "emb-1");
    assert_eq!(
        fake.embedding_requests()[0].inputs,
        vec!["hello".to_string()]
    );

    // `required` refuses an unresolvable selection; `best_effort`
    // degrades to no embedder (honest lexical/symbol-only retrieval).
    for (provider, model) in [("nope", "emb-1"), ("embed-me", "other")] {
        let required = Config {
            embeddings: Some(EmbeddingCfg {
                provider: provider.into(),
                model: model.into(),
                policy: EmbeddingPolicy::Required,
            }),
            ..Config::default()
        };
        assert!(
            required.semantic_embedder(&registry, &retry).is_err(),
            "{provider}/{model} must refuse under required"
        );
        let best_effort = Config {
            embeddings: Some(EmbeddingCfg {
                provider: provider.into(),
                model: model.into(),
                policy: EmbeddingPolicy::BestEffort,
            }),
            ..Config::default()
        };
        assert!(
            best_effort
                .semantic_embedder(&registry, &retry)
                .unwrap()
                .is_none(),
            "{provider}/{model} must degrade under best_effort"
        );
    }
    // A registered provider with no embedding surface cannot be
    // selected even though the id resolves.
    let mut plain = faktor_provider::ProviderRegistry::new();
    plain
        .try_register(Arc::new(FakeProvider::new(
            "plain",
            ModelCapabilities::default(),
        )))
        .unwrap();
    let cfg_plain = Config {
        embeddings: Some(EmbeddingCfg {
            provider: "plain".into(),
            model: "m".into(),
            policy: EmbeddingPolicy::BestEffort,
        }),
        ..Config::default()
    };
    assert!(cfg_plain
        .semantic_embedder(&plain, &retry)
        .unwrap()
        .is_none());
}

#[test]
fn hostile_config_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.json");
    std::fs::write(&path, "{not json").unwrap();
    assert!(Config::load(&path).is_err());
    std::fs::write(&path, r#"{"providers": [{"kind": "nonsense"}]}"#).unwrap();
    assert!(Config::load(&path).is_err());
}

#[test]
fn unknown_fields_are_rejected_everywhere() {
    // Audit 39: strict configs. Unknown top-level keys and unknown keys
    // inside provider/mcp entries are parse errors, never silent noise.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("strict.json");
    for bad in [
        r#"{"model": "m", "surprise_field": true}"#,
        r#"{"providers": [{"kind": "ollama", "id": "o", "bogus": 1}]}"#,
        r#"{"providers": [{"kind": "open_ai", "id": "a", "base_url": "u", "api_key_env": null, "bogus": "x"}]}"#,
        r#"{"mcp": [{"name": "s", "command": "c", "args": [], "bogus": true}]}"#,
    ] {
        std::fs::write(&path, bad).unwrap();
        let e = Config::load(&path).expect_err("hostile config must fail");
        assert!(
            e.contains("unknown field"),
            "expected an unknown-field error, got: {e}"
        );
    }
}

#[test]
fn config_version_defaults_to_one_and_rejects_others() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.json");
    // Absent -> default 1.
    std::fs::write(&path, r#"{"model": "m"}"#).unwrap();
    Config::load(&path).expect("absent config_version defaults to 1");
    // Explicit 1 -> fine.
    std::fs::write(&path, r#"{"config_version": 1, "model": "m"}"#).unwrap();
    Config::load(&path).expect("config_version 1 is accepted");
    // Anything else -> rejected at parse, on both load paths.
    for v in [0u32, 2, 7, 999] {
        std::fs::write(&path, format!(r#"{{"config_version": {v}}}"#)).unwrap();
        let e = Config::load(&path).expect_err("unsupported version must fail");
        assert!(e.contains("config_version"), "{e}");
        assert!(Config::load_strict(&path).is_err());
    }
}

#[test]
fn validate_rejects_duplicate_provider_ids() {
    let cfg = Config {
        providers: vec![
            ProviderCfg::Ollama {
                id: "dup".into(),
                base_url: None,
                pricing: None,
                allow_loopback: true,
                quality: None,
            },
            ProviderCfg::Ollama {
                id: "other".into(),
                base_url: None,
                pricing: None,
                allow_loopback: true,
                quality: None,
            },
            ProviderCfg::OpenAi {
                id: "dup".into(),
                base_url: "http://x".into(),
                api_key_env: None,
                api: None,
                pricing: None,
                allow_loopback: true,
                quality: None,
            },
        ],
        ..Default::default()
    };
    let e = cfg.validate().expect_err("duplicates must be rejected");
    assert!(
        e.contains("dup") && !e.contains("other"),
        "the error lists the duplicate id, got: {e}"
    );
    let cfg = Config {
        providers: vec![
            cfg.providers[0].clone(),
            ProviderCfg::OpenAi {
                id: "distinct".into(),
                base_url: "http://y".into(),
                api_key_env: None,
                api: None,
                pricing: None,
                allow_loopback: true,
                quality: None,
            },
        ],
        ..Default::default()
    };
    cfg.validate().expect("distinct provider ids are fine");
}

/// The explicit loopback rule and the never-permitted address classes
/// are enforced at config load for LITERAL endpoint addresses.
#[test]
fn endpoint_address_class_requires_explicit_naming_of_non_global_literals() {
    let mk = |base: &str, allow_loopback: bool, rows: Option<Vec<String>>| Config {
        providers: vec![ProviderCfg::OpenAi {
            id: "p".into(),
            base_url: base.into(),
            api_key_env: None,
            api: None,
            allow_loopback,
            pricing: None,
            quality: None,
        }],
        sandbox: SandboxCfg {
            network: rows,
            ..Default::default()
        },
        ..Default::default()
    };
    // A loopback literal without the explicit rule or an exact sandbox
    // rule is refused at load.
    let err = mk("http://127.0.0.1:11434", false, None)
        .validate()
        .expect_err("loopback literal without any explicit naming");
    assert!(err.contains("allow_loopback"), "{err}");
    // The entry's explicit rule admits it.
    mk("http://127.0.0.1:11434", true, None)
        .validate()
        .expect("the explicit entry rule admits loopback");
    // Naming the exact destination in [sandbox] network is equally
    // explicit (the operator wrote the literal address).
    mk(
        "http://127.0.0.1:11434",
        false,
        Some(vec!["http://127.0.0.1:11434".to_string()]),
    )
    .validate()
    .expect("an exact sandbox rule admits the literal");
    let err = mk("http://[::1]:11434", false, None)
        .validate()
        .expect_err("ipv6 loopback without any explicit naming");
    assert!(err.contains("loopback"), "{err}");
    mk("http://[::1]:11434", true, None)
        .validate()
        .expect("the explicit entry rule admits ipv6 loopback");
    // Metadata / RFC1918 / link-local literals are refused without an
    // exact sandbox rule; the loopback rule never covers them.
    for base in [
        "http://169.254.169.254",
        "http://10.0.0.1",
        "http://192.168.1.1",
        "http://100.64.0.1",
    ] {
        let err = mk(base, true, None)
            .validate()
            .expect_err("special literal without an exact rule");
        let flat = err.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(flat.contains("refuses unless"), "{base}: {err}");
    }
    // A globally routable literal and a hostname need no rule (a
    // hostname is class-checked at connect time by the resolver).
    mk("https://93.184.216.34", false, None)
        .validate()
        .expect("global literal");
    mk("https://api.example.com", false, None)
        .validate()
        .expect("hostname");
}

/// Serde defaults wire the explicit loopback rule: the LOCAL Ollama
/// runtime defaults to `true` (its documented endpoint is loopback);
/// every remote vendor defaults to `false`.
#[test]
fn loopback_rule_defaults_are_typed_per_variant() {
    let ollama: ProviderCfg = serde_json::from_value(serde_json::json!({
        "kind": "ollama",
        "id": "local",
    }))
    .unwrap();
    assert!(ollama.allows_loopback(), "Ollama is the local runtime");
    let open_ai: ProviderCfg = serde_json::from_value(serde_json::json!({
        "kind": "open_ai",
        "id": "remote",
        "base_url": "https://api.example.com",
    }))
    .unwrap();
    assert!(!open_ai.allows_loopback(), "remote defaults external-only");
    let anthropic: ProviderCfg = serde_json::from_value(serde_json::json!({
        "kind": "anthropic",
        "id": "remote",
    }))
    .unwrap();
    assert!(!anthropic.allows_loopback());
    // The rule is a strict bool; a string is a config error.
    assert!(serde_json::from_value::<ProviderCfg>(serde_json::json!({
        "kind": "ollama",
        "id": "local",
        "allow_loopback": "yes",
    }))
    .is_err());
}

#[test]
fn load_strict_rejects_malformed_and_invalid_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("strict.json");
    // Malformed JSON.
    std::fs::write(&path, "{not json").unwrap();
    assert!(Config::load_strict(&path).is_err());
    // Unknown top-level field.
    std::fs::write(&path, r#"{"model": "m", "extra": 1}"#).unwrap();
    assert!(Config::load_strict(&path).is_err());
    // Unknown field inside a provider entry.
    std::fs::write(
        &path,
        r#"{"providers": [{"kind": "ollama", "id": "o", "zzz": 1}]}"#,
    )
    .unwrap();
    assert!(Config::load_strict(&path).is_err());
    // Unsupported config_version.
    std::fs::write(&path, r#"{"config_version": 3}"#).unwrap();
    assert!(Config::load_strict(&path).is_err());
    // Semantic failure: duplicate provider ids.
    std::fs::write(
        &path,
        r#"{"providers": [
            {"kind": "ollama", "id": "twice", "base_url": null},
            {"kind": "open_ai", "id": "twice", "base_url": "http://x"}
        ]}"#,
    )
    .unwrap();
    let e = Config::load_strict(&path).expect_err("duplicate ids must fail strict load");
    assert!(e.contains("twice"), "{e}");
    // A healthy explicit config still loads strictly.
    std::fs::write(
        &path,
        r#"{"config_version": 1, "model": "m", "providers": [
            {"kind": "ollama", "id": "a", "base_url": null},
            {"kind": "open_ai", "id": "b", "base_url": "http://x"}
        ]}"#,
    )
    .unwrap();
    let cfg = Config::load_strict(&path).unwrap();
    assert_eq!(cfg.model, "m");
    assert_eq!(cfg.providers.len(), 2);
}

#[test]
fn openai_api_setting_selects_family_with_documented_default() {
    use faktor_openai::OpenAiFamily;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("api.json");
    std::fs::write(
        &path,
        r#"{"config_version": 1, "model": "m", "providers": [
            {"kind": "open_ai", "id": "official-default", "base_url": "https://api.openai.com/v1"},
            {"kind": "open_ai", "id": "official-no-version", "base_url": "https://api.openai.com"},
            {"kind": "open_ai", "id": "custom-default", "base_url": "https://corp.example.com/v1"},
            {"kind": "open_ai", "id": "official-chat", "base_url": "https://api.openai.com/v1", "api": "chat"},
            {"kind": "open_ai", "id": "custom-responses", "base_url": "https://corp.example.com/v1", "api": "responses"}
        ]}"#,
    )
    .unwrap();
    let cfg = Config::load_strict(&path).unwrap();
    // Documented default: the modern Responses family ONLY for the
    // official endpoint; every custom base_url stays Chat.
    assert_eq!(
        cfg.providers[0].openai_family(),
        Some(OpenAiFamily::Responses)
    );
    assert_eq!(
        cfg.providers[1].openai_family(),
        Some(OpenAiFamily::Responses)
    );
    assert_eq!(cfg.providers[2].openai_family(), Some(OpenAiFamily::Chat));
    // Explicit `api` always wins, in both directions.
    assert_eq!(cfg.providers[3].openai_family(), Some(OpenAiFamily::Chat));
    assert_eq!(
        cfg.providers[4].openai_family(),
        Some(OpenAiFamily::Responses)
    );
    // Non-OpenAI entries select no OpenAI family.
    assert_eq!(
        ProviderCfg::Ollama {
            id: "o".into(),
            base_url: None,
            pricing: None,
            allow_loopback: true,
            quality: None,
        }
        .openai_family(),
        None
    );
    // The setting round-trips through save/load.
    cfg.save(&path).unwrap();
    let back = Config::load_strict(&path).unwrap();
    assert_eq!(back.providers[3].openai_family(), Some(OpenAiFamily::Chat));
    assert_eq!(
        back.providers[4].openai_family(),
        Some(OpenAiFamily::Responses)
    );
    // Hostile values and unknown sibling keys are strict parse errors.
    for bad in [
        r#"{"providers": [{"kind": "open_ai", "id": "x", "base_url": "https://x", "api": "auto"}]}"#,
        r#"{"providers": [{"kind": "open_ai", "id": "x", "base_url": "https://x", "api": "completions"}]}"#,
        r#"{"providers": [{"kind": "open_ai", "id": "x", "base_url": "https://x", "api": 3}]}"#,
        r#"{"providers": [{"kind": "open_ai", "id": "x", "base_url": "https://x", "api_version": "v1"}]}"#,
    ] {
        std::fs::write(&path, bad).unwrap();
        Config::load(&path).expect_err("hostile api surface must fail");
        assert!(Config::load_strict(&path).is_err());
    }
}

#[tokio::test]
async fn built_openai_provider_targets_the_selected_family_endpoint() {
    // The BUILD path (not just the pure selector) must construct the
    // family the config declares: the recorded request URL proves which
    // endpoint the adapter will speak (responses default for the
    // official endpoint, chat default for custom ones, explicit wins).
    use faktor_core::cancellation::CancellationToken;
    use faktor_core::id::{OpId, SessionId};
    use faktor_provider::egress::MockHttpTransport;
    use faktor_provider::{ContentPart, GenericAgentRequest, RequestMessage, RequestMeta, Role};
    use futures::StreamExt as _;

    let make_cfg = |base_url: &str, api: Option<OpenAiApi>| ProviderCfg::OpenAi {
        id: "probe".into(),
        base_url: base_url.into(),
        api_key_env: None,
        api,
        pricing: None,
        allow_loopback: true,
        quality: None,
    };
    let request = || GenericAgentRequest {
        model: "m".into(),
        system: String::new(),
        messages: vec![RequestMessage {
            role: Role::User,
            content: vec![ContentPart::text("hi")],
        }],
        tools: vec![],
        max_output: None,
        reasoning: None,
        stream: true,
        meta: RequestMeta {
            operation_id: OpId::new(1),
            session_id: SessionId::new(1),
            provider: "probe".into(),
            attempt: 0,
            deadline_ms: 0,
            cancellation: CancellationToken::new(),
        },
    };
    for (cfg, expected_path) in [
        (
            make_cfg("https://api.openai.com/v1", None),
            "https://api.openai.com/v1/responses",
        ),
        (
            make_cfg("https://corp.example.com/v1", None),
            "https://corp.example.com/v1/chat/completions",
        ),
        (
            make_cfg("https://api.openai.com/v1", Some(OpenAiApi::Chat)),
            "https://api.openai.com/v1/chat/completions",
        ),
        (
            make_cfg("https://corp.example.com/v1", Some(OpenAiApi::Responses)),
            "https://corp.example.com/v1/responses",
        ),
    ] {
        let mock = Arc::new(MockHttpTransport::new(200, "data: [DONE]\n\n"));
        let transport: Arc<dyn HttpTransport> = mock.clone();
        let provider = cfg
            .build(transport)
            .unwrap_or_else(|e| panic!("{cfg:?} build: {e}"));
        let mut stream = provider.stream(request());
        while stream.next().await.is_some() {}
        assert_eq!(
            mock.requests(),
            vec![("POST".to_string(), expected_path.to_string())],
            "{cfg:?}"
        );
    }
}

#[test]
fn routing_mode_parses_economy_default_and_pinned_and_rejects_hostile() {
    // Absent -> None (the daemon treats None as Economy).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.json");
    std::fs::write(&path, r#"{"model": "m"}"#).unwrap();
    let cfg = Config::load(&path).unwrap();
    assert_eq!(cfg.routing_mode, None);
    // Economy string.
    std::fs::write(&path, r#"{"routing_mode": "economy"}"#).unwrap();
    assert_eq!(
        Config::load(&path).unwrap().routing_mode,
        Some(RoutingMode::Economy)
    );
    // Pinned object.
    std::fs::write(
        &path,
        r#"{"routing_mode": {"pinned": {"provider": "deepseek", "model": "deepseek-chat"}}}"#,
    )
    .unwrap();
    let cfg = Config::load(&path).unwrap();
    assert_eq!(
        cfg.routing_mode,
        Some(RoutingMode::Pinned {
            provider: "deepseek".into(),
            model: "deepseek-chat".into(),
        })
    );
    // Round-trip through save/load (the daemon's own default file).
    cfg.save(&path).unwrap();
    assert_eq!(Config::load(&path).unwrap().routing_mode, cfg.routing_mode);
    // Hostile shapes are rejected: the old "auto" sentinel, a pinned
    // object missing the model, and wrong-typed values.
    for bad in [
        r#"{"routing_mode": "auto"}"#,
        r#"{"routing_mode": {"pinned": {"provider": "p"}}}"#,
        r#"{"routing_mode": 42}"#,
        r#"{"routing_mode": {"mode": "economy"}}"#,
    ] {
        std::fs::write(&path, bad).unwrap();
        assert!(
            Config::load(&path).is_err(),
            "hostile routing_mode must be rejected: {bad}"
        );
    }
}

#[test]
fn pricing_section_parses_roundtrips_and_applies_to_the_custom_endpoint_only() {
    // The `pricing` section rides the provider entry it names: an
    // exact table prices EVERY model of THAT endpoint (UserOverride,
    // epoch bumped); a second endpoint without a section keeps its
    // Unknown adapter rows — overrides never leak across ids.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.json");
    std::fs::write(
        &path,
        r#"{"config_version": 1, "model": "m", "providers": [
            {"kind": "open_ai", "id": "corp-proxy", "base_url": "https://corp.example.com/v1",
             "pricing": {"input_micro_usd_per_million_tokens": 2000000,
                         "output_micro_usd_per_million_tokens": 8000000}},
            {"kind": "open_ai", "id": "dev-proxy", "base_url": "https://dev.example.com/v1"}
        ]}"#,
    )
    .unwrap();
    let cfg = Config::load(&path).unwrap();
    assert_eq!(cfg.providers.len(), 2);
    let p = &cfg.providers[0];
    assert_eq!(p.id(), "corp-proxy");
    let pricing = p.pricing().expect("pricing section parsed");
    assert_eq!(pricing.input_micro_usd_per_million_tokens, Some(2_000_000));
    assert_eq!(pricing.output_micro_usd_per_million_tokens, Some(8_000_000));
    assert_eq!(pricing.cache_read_micro_usd_per_million_tokens, None);
    assert_eq!(pricing.pricing_ceiling_micro_usd_per_million_tokens, None);
    // Strict load accepts the healthy config (semantic validation too).
    let strict = Config::load_strict(&path).unwrap();
    assert_eq!(strict.providers[0].pricing(), p.pricing());
    // The config file round-trips through save/load.
    cfg.save(&path).unwrap();
    assert_eq!(
        Config::load(&path).unwrap().providers[0].pricing(),
        p.pricing()
    );
    // Apply: the configured endpoint's rows become Known/UserOverride
    // with the pricing epoch bumped; the other endpoint stays Unknown.
    let mut registry = faktor_provider::ProviderRegistry::new();
    for provider in &cfg.providers {
        registry
            .try_register(provider.build(open_transport()).unwrap())
            .unwrap();
    }
    let corp = registry.get("corp-proxy").unwrap().catalog_entry("default");
    match &corp.pricing {
        faktor_provider::catalog::PricingState::Known(snap) => {
            assert_eq!(snap.authority, faktor_core::model::PriceAuthority::Exact);
            let q = snap.quote.expect("exact override quotes");
            assert_eq!(
                q.input,
                faktor_core::model::MicroUsdPerMillionTokens(2_000_000)
            );
            assert_eq!(
                q.output,
                faktor_core::model::MicroUsdPerMillionTokens(8_000_000)
            );
            assert_eq!(
                q.cache_read,
                faktor_core::model::MicroUsdPerMillionTokens(0)
            );
            assert_eq!(
                q.cache_write,
                faktor_core::model::MicroUsdPerMillionTokens(0)
            );
            // The exact quote never truncates and never reads free.
            assert_eq!(snap.settle_cost(1_000_000, 0, 0, 0), Some(2_000_000));
        }
        other => panic!("override must price the row Known, got {other:?}"),
    }
    assert_eq!(
        corp.provenance,
        faktor_provider::catalog::Provenance::UserOverride
    );
    assert_eq!(
        corp.source_epoch,
        faktor_provider::catalog::CATALOG_FIRST_EPOCH + 1,
        "the override increments the pricing epoch"
    );
    let dev = registry.get("dev-proxy").unwrap().catalog_entry("default");
    assert_eq!(
        dev.pricing,
        faktor_provider::catalog::PricingState::Unknown,
        "an endpoint without a pricing section keeps its Unknown adapter rows"
    );
}

#[test]
fn pricing_ceiling_parses_and_composites_unknown_rows_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.json");
    std::fs::write(
        &path,
        r#"{"providers": [
            {"kind": "open_ai", "id": "gw", "base_url": "https://gw.example.com/v1",
             "pricing": {"pricing_ceiling_micro_usd_per_million_tokens": 42000000}}
        ]}"#,
    )
    .unwrap();
    let cfg = Config::load_strict(&path).unwrap();
    let provider = cfg.providers[0].build(open_transport()).unwrap();
    let entry = provider.catalog_entry("whatever-model");
    assert_eq!(
        entry.provenance,
        faktor_provider::catalog::Provenance::Composite
    );
    assert_eq!(entry.source_epoch, 2);
    match &entry.pricing {
        faktor_provider::catalog::PricingState::ConservativeCeiling(snap) => {
            assert_eq!(
                snap.authority,
                faktor_core::model::PriceAuthority::ConservativeCeiling
            );
            let q = snap.quote.expect("ceiling quotes");
            for line in [q.input, q.output, q.cache_read, q.cache_write] {
                assert_eq!(
                    line,
                    faktor_core::model::MicroUsdPerMillionTokens(42_000_000)
                );
            }
        }
        other => panic!("ceiling must produce ConservativeCeiling, got {other:?}"),
    }
    // Known rows of the SAME endpoint keep their price under a ceiling
    // (covered by the graph test) — here only the epoch bump is
    // asserted for the Unknown row above.
    let _ = provider.known_models();
}

#[test]
fn hostile_pricing_override_values_are_typed_errors_everywhere() {
    // 0 input, absurd magnitudes, partial tables, zero/absurd ceilings,
    // tables on local runtimes, and unknown keys inside `pricing` are
    // all refused — on the parse/validate path AND at adapter build.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("h.json");
    let cases: Vec<(&str, &str)> = vec![
        // Zero input price = the local-free marker on a remote endpoint.
        (
            "zero input",
            r#""pricing": {"input_micro_usd_per_million_tokens": 0,
                           "output_micro_usd_per_million_tokens": 8000000}"#,
        ),
        // Partial tables would silently price the missing side at 0.
        (
            "partial table",
            r#""pricing": {"input_micro_usd_per_million_tokens": 2000000}"#,
        ),
        // Absurd magnitudes beyond the cap.
        (
            "absurd price",
            r#""pricing": {"input_micro_usd_per_million_tokens": 1000000000001,
                           "output_micro_usd_per_million_tokens": 8000000}"#,
        ),
        (
            "absurd ceiling",
            r#""pricing": {"pricing_ceiling_micro_usd_per_million_tokens":
                           18446744073709551615}"#,
        ),
        // A zero ceiling prices unknown models as free — refused.
        (
            "zero ceiling",
            r#""pricing": {"pricing_ceiling_micro_usd_per_million_tokens": 0}"#,
        ),
    ];
    for (label, pricing_json) in cases {
        let text = format!(
            r#"{{"providers": [{{"kind": "open_ai", "id": "p", "base_url": "http://x", {pricing_json}}}]}}"#
        );
        std::fs::write(&path, &text).unwrap();
        // Parse succeeds (lenient); semantic validation refuses.
        let cfg = Config::load(&path).unwrap_or_else(|e| panic!("{label}: parse: {e}"));
        let e = cfg
            .validate()
            .expect_err(&format!("{label}: validate must refuse"));
        assert!(!e.is_empty(), "{label}");
        // Adapter build refuses too (the runtime gate).
        let err = match cfg.providers[0].build(open_transport()) {
            Ok(_) => panic!("{label}: build must refuse"),
            Err(e) => e,
        };
        assert!(!err.is_empty(), "{label}");
        // And the strict file load path refuses.
        std::fs::write(&path, &text).unwrap();
        let strict_err =
            Config::load_strict(&path).expect_err(&format!("{label}: strict load must refuse"));
        assert!(!strict_err.is_empty(), "{label}");
    }
    // A pricing table under a LOCAL (ollama) provider is refused:
    // overrides apply to custom REMOTE endpoints only.
    let ollama = ProviderCfg::Ollama {
        id: "ollama".into(),
        base_url: None,
        pricing: Some(ProviderPricingCfg {
            input_micro_usd_per_million_tokens: Some(15_000_000),
            output_micro_usd_per_million_tokens: Some(60_000_000),
            ..Default::default()
        }),
        allow_loopback: true,
        quality: None,
    };
    let e = ollama
        .validate_pricing()
        .expect_err("ollama pricing refused");
    assert!(e.contains("local"), "{e}");
    // Unknown keys inside the pricing section are parse errors.
    std::fs::write(
        &path,
        r#"{"providers": [{"kind": "open_ai", "id": "p", "base_url": "http://x",
             "pricing": {"input_micro_usd_per_million_tokens": 2000000, "bogus": 1}}]}"#,
    )
    .unwrap();
    let e = Config::load(&path).expect_err("unknown pricing key must fail");
    assert!(e.contains("unknown field"), "{e}");
    // A hostile section on an unknown kind is refused at parse like any
    // unknown provider kind.
    std::fs::write(
        &path,
        r#"{"providers": [{"kind": "open_ai", "id": "p", "base_url": "http://x",
             "pricing": "expensive"}]}"#,
    )
    .unwrap();
    assert!(Config::load(&path).is_err());
}

#[test]
fn ceiling_applies_to_gateway_instances_too() {
    // The gateway family is a custom endpoint: its Unknown rows are
    // composite-priced by a ceiling exactly like open_ai endpoints.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("g.json");
    std::fs::write(
        &path,
        r#"{"providers": [
            {"kind": "gateway", "id": "agg-gw", "base_url": "https://gateway.example.com",
             "pricing": {"pricing_ceiling_micro_usd_per_million_tokens": 60000000}}
        ]}"#,
    )
    .unwrap();
    let cfg = Config::load_strict(&path).unwrap();
    let provider = cfg.providers[0].build(open_transport()).unwrap();
    let entry = provider.catalog_entry("default");
    assert_eq!(
        entry.provenance,
        faktor_provider::catalog::Provenance::Composite
    );
}

#[test]
fn keys_read_from_env_not_file() {
    std::env::set_var("FAKTOR_TEST_KEY", "secret-value");
    let cfg = ProviderCfg::OpenAi {
        id: "t".into(),
        base_url: "http://x".into(),
        api_key_env: Some("FAKTOR_TEST_KEY".into()),
        api: None,
        pricing: None,
        allow_loopback: true,
        quality: None,
    };
    assert_eq!(
        cfg.key().as_ref().map(SecretValue::expose),
        Some("secret-value")
    );
    std::env::remove_var("FAKTOR_TEST_KEY");
    assert_eq!(
        cfg.key(),
        None,
        "missing env = no key, never a stored secret"
    );
}

#[test]
fn provider_ids_are_stable() {
    let cfg = ProviderCfg::Ollama {
        id: "ollama".into(),
        base_url: None,
        pricing: None,
        allow_loopback: true,
        quality: None,
    };
    assert_eq!(cfg.id(), "ollama");
}

#[test]
fn built_providers_register_under_configured_instance_ids() {
    // Two OpenAI-compatible endpoints with distinct configured ids:
    // both must register and resolve by their ids (the old registry
    // keyed the adapter family id "openai", so the second overwrote
    // the first and custom ids never looked up).
    let mut registry = faktor_provider::ProviderRegistry::new();
    for id in ["corp-proxy", "dev-proxy"] {
        let cfg = ProviderCfg::OpenAi {
            id: id.into(),
            base_url: format!("https://{id}.example.com/v1"),
            api_key_env: None,
            api: None,
            pricing: None,
            allow_loopback: true,
            quality: None,
        };
        registry
            .try_register(cfg.build(open_transport()).unwrap())
            .unwrap();
    }
    assert_eq!(registry.ids(), vec!["corp-proxy", "dev-proxy"]);
    assert!(registry.get("corp-proxy").is_some());
    assert!(registry.get("dev-proxy").is_some());
    assert!(
        registry.get("openai").is_none(),
        "family id must not resolve"
    );
}

#[test]
fn deepseek_profiles_build_including_gateway_and_direct_base() {
    // The DeepSeek matrix (spec §11): every profile string in the
    // config builds a provider — including "gateway" (previously an
    // unparseable arm) and "direct" with a custom base_url.
    let mut registry = faktor_provider::ProviderRegistry::new();
    for (profile, base) in [
        ("direct", None),
        ("direct", Some("http://127.0.0.1:9000")),
        ("gateway", Some("https://gw.example.com")),
        ("openrouter", None),
        ("compatible", Some("http://127.0.0.1:8000")),
        ("local", Some("http://127.0.0.1:8000")),
    ] {
        let cfg = ProviderCfg::DeepSeek {
            id: format!("ds-{profile}-{}", base.is_some()),
            profile: profile.into(),
            base_url: base.map(|b| b.to_string()),
            api_key_env: None,
            pricing: None,
            allow_loopback: true,
            quality: None,
        };
        let provider = cfg
            .build(open_transport())
            .unwrap_or_else(|e| panic!("{profile:?} build: {e}"));
        registry.try_register(provider).unwrap();
    }
    assert!(registry.get("ds-gateway-true").is_some());
    assert!(registry.get("ds-direct-true").is_some());
    assert!(registry.get("ds-direct-false").is_some());
    // Unknown profiles stay loud.
    let cfg = ProviderCfg::DeepSeek {
        id: "x".into(),
        profile: "bogus".into(),
        base_url: None,
        api_key_env: None,
        pricing: None,
        allow_loopback: true,
        quality: None,
    };
    assert!(cfg.build(open_transport()).is_err());
    // A gateway without an explicit endpoint is refused: the config
    // never silently assumes a third-party gateway URL.
    let cfg = ProviderCfg::DeepSeek {
        id: "gw-missing".into(),
        profile: "gateway".into(),
        base_url: None,
        api_key_env: None,
        pricing: None,
        allow_loopback: true,
        quality: None,
    };
    let err = cfg
        .build(open_transport())
        .err()
        .expect("gateway without base_url must be refused");
    assert!(err.contains("base_url"), "{err}");
}

#[test]
fn billing_origin_matrix_is_strict_and_instance_scoped() {
    use faktor_core::model::PriceAuthority;
    use faktor_provider::catalog::PricingState;

    // Origin resolution reads the ENDPOINT CONFIG only: the canonical
    // official URLs resolve official, any other base_url is a custom
    // endpoint (whatever transport family it speaks), gateway kinds are
    // gateways, ollama is local.
    let official_openai = ProviderCfg::OpenAi {
        id: "a".into(),
        base_url: OPENAI_OFFICIAL_BASE_URL.into(),
        api_key_env: None,
        api: None,
        pricing: None,
        allow_loopback: true,
        quality: None,
    };
    let official_openai_slash = ProviderCfg::OpenAi {
        id: "a2".into(),
        base_url: format!("{OPENAI_OFFICIAL_BASE_URL}/"),
        api_key_env: None,
        api: None,
        pricing: None,
        allow_loopback: true,
        quality: None,
    };
    let custom_openai = ProviderCfg::OpenAi {
        id: "corp-proxy".into(),
        base_url: "https://corp.example.com/v1".into(),
        api_key_env: None,
        api: None,
        pricing: None,
        allow_loopback: true,
        quality: None,
    };
    assert_eq!(
        official_openai.billing_origin(),
        BillingOrigin::OfficialOpenAi
    );
    assert_eq!(
        official_openai_slash.billing_origin(),
        BillingOrigin::OfficialOpenAi,
        "a trailing slash is the same canonical endpoint"
    );
    assert_eq!(
        custom_openai.billing_origin(),
        BillingOrigin::CustomEndpoint
    );
    assert_eq!(
        ProviderCfg::Anthropic {
            id: "anthropic".into(),
            api_key_env: None,
            pricing: None,
            allow_loopback: true,
            quality: None,
        }
        .billing_origin(),
        BillingOrigin::OfficialAnthropic
    );
    assert_eq!(
        ProviderCfg::Google {
            id: "google".into(),
            api_key_env: None,
            pricing: None,
            allow_loopback: true,
            quality: None,
        }
        .billing_origin(),
        BillingOrigin::OfficialGoogle
    );
    let deepseek = |profile: &str, base: Option<&str>| ProviderCfg::DeepSeek {
        id: format!("ds-{profile}"),
        profile: profile.into(),
        base_url: base.map(str::to_string),
        api_key_env: None,
        pricing: None,
        allow_loopback: true,
        quality: None,
    };
    assert_eq!(
        deepseek("direct", None).billing_origin(),
        BillingOrigin::OfficialDeepSeek
    );
    assert_eq!(
        deepseek("direct", Some(DEEPSEEK_OFFICIAL_BASE_URL)).billing_origin(),
        BillingOrigin::OfficialDeepSeek
    );
    assert_eq!(
        deepseek("direct", Some("https://corp.example.com/v1")).billing_origin(),
        BillingOrigin::CustomEndpoint
    );
    assert_eq!(
        deepseek("gateway", None).billing_origin(),
        BillingOrigin::Gateway
    );
    assert_eq!(
        deepseek("openrouter", None).billing_origin(),
        BillingOrigin::Gateway
    );
    assert_eq!(
        ProviderCfg::Gateway {
            id: "gw".into(),
            base_url: "https://gateway.example.com".into(),
            api_key_env: None,
            pricing: None,
            allow_loopback: true,
            quality: None,
        }
        .billing_origin(),
        BillingOrigin::Gateway
    );
    assert_eq!(
        ProviderCfg::Ollama {
            id: "ollama".into(),
            base_url: None,
            pricing: None,
            allow_loopback: true,
            quality: None,
        }
        .billing_origin(),
        BillingOrigin::Local
    );
    // The instance id NEVER changes the origin: two entries differing
    // only in id resolve identically.
    let same_custom_other_id = ProviderCfg::OpenAi {
        id: "b".into(),
        base_url: "https://corp.example.com/v1".into(),
        api_key_env: None,
        api: None,
        pricing: None,
        allow_loopback: true,
        quality: None,
    };
    assert_ne!(custom_openai.id(), same_custom_other_id.id());
    assert_eq!(
        custom_openai.billing_origin(),
        same_custom_other_id.billing_origin()
    );

    // Built rows follow the origin, not the wire family: official
    // OpenAI gpt-4o is Exact at $2.50/M; the custom OpenAI-compatible
    // endpoint is Unknown; DeepSeek mirrors both.
    let official = official_openai.build(open_transport()).unwrap();
    let e = official.catalog_entry("gpt-4o");
    assert_eq!(e.pricing.authority(), PriceAuthority::Exact);
    assert_eq!(
        e.pricing.quote().unwrap().input,
        MicroUsdPerMillionTokens(2_500_000)
    );
    let official_other_id = ProviderCfg::OpenAi {
        id: "b".into(),
        base_url: OPENAI_OFFICIAL_BASE_URL.into(),
        api_key_env: None,
        api: None,
        pricing: None,
        allow_loopback: true,
        quality: None,
    }
    .build(open_transport())
    .unwrap();
    let e2 = official_other_id.catalog_entry("gpt-4o");
    assert_ne!(e.provider, e2.provider, "instance ids differ");
    assert_eq!(
        e.pricing, e2.pricing,
        "the instance id must not change the resolved price"
    );
    let custom = custom_openai.build(open_transport()).unwrap();
    let e = custom.catalog_entry("gpt-4o");
    assert_eq!(e.provider, "corp-proxy");
    assert_eq!(e.pricing, PricingState::Unknown);
    assert_eq!(e.pricing_snapshot().settle_cost(1_000_000, 0, 0, 0), None);
    // DeepSeek V4-era facts are not documented as exact: the official
    // endpoint resolves a CONSERVATIVE CEILING (never read as a list
    // price), and changing the origin never lets a model inherit
    // another origin's catalog.
    let official_ds = deepseek("direct", None).build(open_transport()).unwrap();
    let e = official_ds.catalog_entry("deepseek-chat");
    assert_eq!(
        e.pricing.authority(),
        PriceAuthority::ConservativeCeiling,
        "the V4-era row is a bound, not an exact claim"
    );
    assert_eq!(
        e.pricing.quote().unwrap().input,
        MicroUsdPerMillionTokens(560_000)
    );
    assert_eq!(
        official_ds.catalog_entry("gpt-4o").pricing,
        PricingState::Unknown,
        "a DeepSeek endpoint never inherits the OpenAI catalog"
    );
    assert_eq!(
        official.catalog_entry("deepseek-chat").pricing,
        PricingState::Unknown,
        "and an OpenAI endpoint never inherits DeepSeek's row"
    );
    // Retired Anthropic IDs never resolve to their last-known price.
    let official_anthropic = ProviderCfg::Anthropic {
        id: "anthropic".into(),
        api_key_env: None,
        pricing: None,
        allow_loopback: true,
        quality: None,
    }
    .build(open_transport())
    .unwrap();
    assert_eq!(
        official_anthropic.catalog_entry("claude-haiku-3.5").pricing,
        PricingState::Unknown
    );
    let custom_ds = deepseek("direct", Some("https://corp.example.com/v1"))
        .build(open_transport())
        .unwrap();
    assert_eq!(
        custom_ds.catalog_entry("deepseek-chat").pricing,
        PricingState::Unknown
    );
}

#[test]
fn mcp_config_validation_bounds_and_duplicates() {
    // Spec §31 hostile configs are rejected, never spawned.
    let mut cfg = Config::default();
    assert!(cfg.mcp_servers().unwrap().is_empty());
    cfg.mcp.push(McpEntry {
        name: "server".into(),
        command: "python3".into(),
        args: vec!["-m".into(), "srv".into()],
    });
    assert_eq!(cfg.mcp_servers().unwrap().len(), 1);
    // Duplicate names.
    cfg.mcp.push(McpEntry {
        name: "server".into(),
        command: "python3".into(),
        args: vec![],
    });
    assert!(cfg.mcp_servers().is_err(), "duplicate names rejected");
    cfg.mcp.pop();
    // Empty names/commands and oversized entries.
    for bad in [
        McpEntry {
            name: String::new(),
            command: "x".into(),
            args: vec![],
        },
        McpEntry {
            name: "n".into(),
            command: String::new(),
            args: vec![],
        },
        McpEntry {
            name: "x".repeat(200),
            command: "c".into(),
            args: vec![],
        },
        McpEntry {
            name: "n".into(),
            command: "c".into(),
            args: vec!["a".repeat(600)],
        },
        McpEntry {
            name: "n".into(),
            command: "c".into(),
            args: vec!["a".into(); MAX_MCP_ARGS + 1],
        },
    ] {
        cfg.mcp.push(bad);
        assert!(cfg.mcp_servers().is_err(), "hostile entry rejected");
        cfg.mcp.pop();
    }
    // Too many servers.
    cfg.mcp = (0..MAX_MCP_SERVERS + 1)
        .map(|i| McpEntry {
            name: format!("s{i}"),
            command: "c".into(),
            args: vec![],
        })
        .collect();
    assert!(cfg.mcp_servers().is_err(), "server count capped");
    // Round-trips through the file config loader.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("faktor-plus.json");
    std::fs::write(
        &path,
        r#"{"mcp": [{"name": "fixture", "command": "python3", "args": ["mock.py"]}]}"#,
    )
    .unwrap();
    let loaded = Config::load(&path).unwrap();
    assert_eq!(loaded.mcp.len(), 1);
    assert_eq!(loaded.mcp[0].name, "fixture");
}

/// Explicit default-allow transport for construction-level unit tests
/// (the daemon always passes the policy-checked transport built from
/// its SandboxPolicy; these tests never exercise egress).
fn open_transport() -> Arc<dyn HttpTransport> {
    Arc::new(faktor_provider::egress::PolicyCheckedHttpTransport::permissive())
}

#[test]
fn verification_section_defaults_partial_objects_and_zero_disables() {
    // Absent section -> crate defaults (60/600/background).
    let cfg = Config::default();
    assert_eq!(cfg.verification.quick_max_s, 60);
    assert_eq!(cfg.verification.unit_max_s, 600);
    assert!(cfg.verification.full_as_background);
    let policy = cfg.verification.policy().expect("defaults stay enabled");
    use faktor_verify::exec::{budget_for, BudgetDecision, CheckCategory, CheckKind, CheckSpec};
    let spec = CheckSpec::new(
        "q",
        CheckKind::Compile,
        CheckCategory::Quick,
        "cargo",
        ["check"],
        true,
    );
    // Budget probe: the derived per-check budget is the configured cap.
    assert_eq!(
        budget_for(spec.category, &policy, None),
        BudgetDecision::RunInline(std::time::Duration::from_secs(60))
    );
    assert_eq!(
        budget_for(CheckCategory::Unit, &policy, None),
        BudgetDecision::RunInline(std::time::Duration::from_secs(600))
    );
    assert_eq!(
        budget_for(CheckCategory::Full, &policy, None),
        BudgetDecision::RunAsTaskOwnedOperation,
        "full checks go background by default"
    );
    // Partial objects fill per-key defaults (60/600/true), never 0.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.json");
    std::fs::write(
        &path,
        r#"{"verification": {"full_as_background": false, "unit_max_s": 120}}"#,
    )
    .unwrap();
    let cfg = Config::load(&path).unwrap();
    assert_eq!(
        cfg.verification.quick_max_s, 60,
        "partial keeps quick default"
    );
    assert_eq!(cfg.verification.unit_max_s, 120);
    assert!(!cfg.verification.full_as_background);
    let policy = cfg.verification.policy().unwrap();
    assert_eq!(
        budget_for(CheckCategory::Quick, &policy, None),
        BudgetDecision::RunInline(std::time::Duration::from_secs(60))
    );
    assert_eq!(
        budget_for(CheckCategory::Full, &policy, None),
        BudgetDecision::RunInline(std::time::Duration::from_secs(120)),
        "full_as_background: false keeps full checks inline under unit_max"
    );
    // quick_max_s = 0 disables the service: the mapping yields None
    // (fail closed), on the parse path AND the daemon mapping path.
    std::fs::write(
        &path,
        r#"{"verification": {"quick_max_s": 0, "unit_max_s": 0}}"#,
    )
    .unwrap();
    let cfg = Config::load(&path).unwrap();
    assert_eq!(cfg.verification.policy(), None, "quick 0 -> disabled");
}

#[test]
fn verification_section_unknown_fields_fail_everywhere() {
    // Strictness stays for EXPLICIT configs: an unknown key inside
    // [verification] is a parse error on both load paths.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.json");
    for bad in [
        r#"{"verification": {"quick_max_s": 30, "bogus": 1}}"#,
        r#"{"verification": {"quick_max_s": "fast"}}"#,
    ] {
        std::fs::write(&path, bad).unwrap();
        let e = Config::load(&path).expect_err("hostile [verification] must fail");
        assert!(
            e.contains("unknown field") || e.contains("invalid type"),
            "{e}"
        );
        assert!(Config::load_strict(&path).is_err());
    }
    // The section itself deserializes strict too (a nested object under
    // the wrong name is still an unknown top-level key).
    std::fs::write(&path, r#"{"verif": {"quick_max_s": 30}}"#).unwrap();
    assert!(Config::load(&path).is_err());
}

#[test]
fn sandbox_section_maps_guarantees_and_rows_strictly() {
    use faktor_sandbox::{SandboxGuarantee, SandboxPolicy, ShellExecutionMode};
    // Absent section: crate defaults (frozen gate, secure guarantee
    // Required + OS-isolated shell).
    let cfg = Config::default();
    let policy = cfg.sandbox_policy().unwrap();
    assert_eq!(policy.network_guarantee, SandboxGuarantee::Required);
    assert_eq!(policy.shell_execution, ShellExecutionMode::OsIsolated);
    assert!(policy.network.installed().is_some(), "frozen allowlist");
    assert_eq!(
        policy,
        SandboxPolicy::default(),
        "absent sandbox section == sandbox defaults"
    );
    // Explicit rows replace the gate; an empty list denies everything.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.json");
    std::fs::write(
        &path,
        r#"{"sandbox": {"network": ["http://127.0.0.1:8765"], "network_guarantee": "required"}}"#,
    )
    .unwrap();
    let cfg = Config::load(&path).unwrap();
    let policy = cfg.sandbox_policy().unwrap();
    assert_eq!(
        policy.network_guarantee,
        SandboxGuarantee::Required,
        "required parses through the sandbox serde field"
    );
    assert!(policy.network.installed().is_some());
    // Defaults round-trip through the file shape.
    cfg.save(&path).unwrap();
    let loaded = Config::load(&path).unwrap();
    assert_eq!(loaded.sandbox_policy().unwrap(), policy);
    // A non-Required guarantee WITHOUT the explicit user grant is a
    // typed policy error: full functionality is opt-in, never a silent
    // default, and the error names the exact key to set.
    std::fs::write(&path, r#"{"sandbox": {"network_guarantee": "none"}}"#).unwrap();
    let cfg = Config::load(&path).unwrap();
    let e = cfg.sandbox_policy().expect_err("implicit grant refused");
    assert!(e.contains("network_capable_user_granted"), "{e}");
    // best_effort/none parse WITH the explicit network-capable shell
    // grant; a hostile guarantee value is a parse error; unknown keys
    // inside [sandbox] are rejected.
    for (text, expect) in [
        (
            r#"{"sandbox": {"network_guarantee": "best_effort", "shell": "network_capable_user_granted"}}"#,
            SandboxGuarantee::BestEffort,
        ),
        (
            r#"{"sandbox": {"network_guarantee": "none", "shell": "network_capable_user_granted"}}"#,
            SandboxGuarantee::None,
        ),
    ] {
        std::fs::write(&path, text).unwrap();
        let cfg = Config::load(&path).unwrap();
        let policy = cfg.sandbox_policy().unwrap();
        assert_eq!(policy.network_guarantee, expect);
        assert_eq!(
            policy.shell_execution,
            ShellExecutionMode::NetworkCapableUserGranted
        );
        assert_eq!(policy.spawn_profile().shell, "network_capable_user_granted");
    }
    // The explicit grant is disjoint from OS isolation: Required +
    // network_capable_user_granted is refused typed.
    std::fs::write(
        &path,
        r#"{"sandbox": {"shell": "network_capable_user_granted"}}"#,
    )
    .unwrap();
    let cfg = Config::load(&path).unwrap();
    assert!(cfg.sandbox_policy().is_err(), "contradictory pairing");
    for bad in [
        r#"{"sandbox": {"network_guarantee": "mandatory"}}"#,
        r#"{"sandbox": {"shell": "network_capable"}}"#,
        r#"{"sandbox": {"network": ["http://127.0.0.1:1"], "bogus": true}}"#,
    ] {
        std::fs::write(&path, bad).unwrap();
        assert!(
            Config::load(&path).is_err(),
            "hostile [sandbox] must be rejected: {bad}"
        );
    }
    // A rule that cannot parse is a semantic (validate/strict-load and
    // daemon-policy) error — never silently permissive.
    std::fs::write(&path, r#"{"sandbox": {"network": ["not a url"]}}"#).unwrap();
    let cfg = Config::load(&path).unwrap();
    let e = cfg.sandbox_policy().expect_err("unparseable rule fails");
    assert!(!e.is_empty());
    assert!(Config::load_strict(&path).is_err());
}

/// The `[sandbox] shell` setting is an OPTION over two trust classes:
/// UNSET (`None`) is distinguishable from an explicit `os_isolated`, so
/// the agent shell-tool class keeps its secure default when nothing is
/// configured while the interactive-terminal class can select its own
/// user-initiated default (resolved by the terminal authority; the
/// server crate unit-tests that constructor). Parse strictness and the
/// unset round-trip are pinned here.
#[test]
fn sandbox_shell_option_distinguishes_unset_from_explicit_os_isolation() {
    use faktor_sandbox::{SandboxGuarantee, SandboxPolicy, ShellExecutionMode};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.json");

    // Unset: `None`; the agent policy is the crate's secure default.
    let cfg = Config::default();
    assert_eq!(cfg.sandbox.shell, None, "an absent shell key is UNSET");
    assert_eq!(cfg.sandbox_policy().unwrap(), SandboxPolicy::default());

    // Explicit os_isolated: `Some(OsIsolated)` with the required
    // guarantee — the same agent contract, but an EXPLICIT choice the
    // terminal class can tell apart from the unset default.
    std::fs::write(&path, r#"{"sandbox": {"shell": "os_isolated"}}"#).unwrap();
    let cfg = Config::load(&path).unwrap();
    assert_eq!(cfg.sandbox.shell, Some(ShellExecutionMode::OsIsolated));
    assert_eq!(cfg.sandbox.network_guarantee, SandboxGuarantee::Required);
    assert_eq!(
        cfg.sandbox_policy().unwrap().shell_execution,
        ShellExecutionMode::OsIsolated
    );

    // Explicit grant: `Some(...)` with a non-required guarantee.
    std::fs::write(
        &path,
        r#"{"sandbox": {"network_guarantee": "none", "shell": "network_capable_user_granted"}}"#,
    )
    .unwrap();
    let cfg = Config::load(&path).unwrap();
    assert_eq!(
        cfg.sandbox.shell,
        Some(ShellExecutionMode::NetworkCapableUserGranted)
    );
    assert_eq!(cfg.sandbox.network_guarantee, SandboxGuarantee::None);
    assert_eq!(
        cfg.sandbox_policy().unwrap().shell_execution,
        ShellExecutionMode::NetworkCapableUserGranted
    );

    // The unset state round-trips (`null`), never silently becoming an
    // explicit os_isolated.
    let cfg = Config::default();
    cfg.save(&path).unwrap();
    let reloaded = Config::load(&path).unwrap();
    assert_eq!(reloaded.sandbox.shell, None);
    assert_eq!(
        cfg.sandbox_policy().unwrap(),
        reloaded.sandbox_policy().unwrap()
    );
}

#[test]
fn efficiency_section_defaults_on_parses_strictly_and_roundtrips() {
    // Absent section: every flag ON (the production efficiency system).
    // `EfficiencyCfg::default()` stays the additive all-off semantics for
    // unit/embedded callers; the Config paths use production_defaults().
    let cfg = Config::default();
    assert_eq!(cfg.efficiency, EfficiencyCfg::production_defaults());
    assert!(
        cfg.efficiency.failure_learning
            && cfg.efficiency.ccr
            && cfg.efficiency.typed_handoff
            && cfg.efficiency.semantic_context
            && cfg.efficiency.rework_routing,
        "every [efficiency] flag defaults ON in production"
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.json");
    // The documented off-switches: an explicit `false` per component.
    std::fs::write(
        &path,
        r#"{"efficiency": {"failure_learning": false, "ccr": false, "typed_handoff": false,
             "semantic_context": false, "rework_routing": false}}"#,
    )
    .unwrap();
    let all_off = Config::load_strict(&path).unwrap();
    assert_eq!(
        all_off.efficiency,
        EfficiencyCfg {
            failure_learning: false,
            ccr: false,
            typed_handoff: false,
            semantic_context: false,
            rework_routing: false,
        }
    );
    // Partial objects flip only the named key; the others keep ON.
    std::fs::write(&path, r#"{"efficiency": {"ccr": false}}"#).unwrap();
    let partial = Config::load(&path).unwrap();
    assert!(!partial.efficiency.ccr);
    assert!(partial.efficiency.failure_learning);
    assert!(partial.efficiency.typed_handoff);
    assert!(partial.efficiency.semantic_context);
    assert!(partial.efficiency.rework_routing);
    // Round-trip through the daemon's own file shape.
    all_off.save(&path).unwrap();
    assert_eq!(Config::load(&path).unwrap().efficiency, all_off.efficiency);
    // Hostile shapes: unknown keys, non-boolean values, duplicate keys,
    // and non-object containers (a positional array must never enable
    // flags) all fail on both load paths.
    for bad in [
        r#"{"efficiency": {"ccr": true, "bogus": 1}}"#,
        r#"{"efficiency": {"ccr": "yes"}}"#,
        r#"{"efficiency": {"ccr": 1}}"#,
        r#"{"efficiency": {"failure_learning": null}}"#,
        r#"{"efficiency": {"ccr": true, "ccr": false}}"#,
        r#"{"efficiency": []}"#,
        r#"{"efficiency": [true, true, true, true, true]}"#,
        r#"{"efficiency": true}"#,
        r#"{"efficency": {"ccr": true}}"#,
    ] {
        std::fs::write(&path, bad).unwrap();
        let e = Config::load(&path).expect_err("hostile [efficiency] must fail");
        assert!(
            e.contains("unknown field")
                || e.contains("invalid type")
                || e.contains("duplicate field"),
            "{bad}: {e}"
        );
        assert!(Config::load_strict(&path).is_err(), "{bad}");
    }
}

/// The reachable half of the `failure_learning` flag: the parsed flag is
/// exactly what decides whether a failure prior is handed to the context
/// planner, and the planner honors it. The production wiring now exists:
/// `crates/cli/src/main.rs` `daemon_context_prior` builds the real
/// `LearningService`-backed adapter only when the flag is on,
/// `efficiency_flags` mirrors the whole section onto
/// `faktor_agent::EfficiencyFlags`, and the runtime consumes the gate at
/// the `plan_wire_turn_with_prior` call site.
#[test]
fn failure_learning_flag_gates_the_planner_prior_hook() {
    use faktor_context::planner::{plan_context, plan_context_with_prior, ContextPlanRequest};
    use faktor_context::{CandidateKind, ContextCandidate, FailurePrior};

    struct BoostA;
    impl FailurePrior for BoostA {
        fn omission_risk(&self, candidate: &ContextCandidate) -> f64 {
            if candidate.id == "a" {
                2.0
            } else {
                1.0
            }
        }
    }
    let candidate = |id: &str, utility: f64| ContextCandidate {
        id: id.into(),
        kind: CandidateKind::FileNote,
        bytes: 10,
        estimate_tokens: 10,
        utility,
        ..ContextCandidate::default()
    };
    let request = || ContextPlanRequest {
        index_evidence: vec![candidate("a", 0.5), candidate("b", 0.6)],
        token_budget: 10,
        ..ContextPlanRequest::default()
    };

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.json");
    std::fs::write(&path, r#"{"efficiency": {"failure_learning": false}}"#).unwrap();
    let off = Config::load(&path).unwrap();
    std::fs::write(&path, r#"{"efficiency": {"failure_learning": true}}"#).unwrap();
    let on = Config::load(&path).unwrap();
    assert!(!off.efficiency.failure_learning);
    assert!(on.efficiency.failure_learning);

    let prior = BoostA;
    let planned = |enabled: bool| {
        if enabled {
            plan_context_with_prior(request(), Some(&prior))
        } else {
            plan_context(request())
        }
    };
    let baseline = planned(off.efficiency.failure_learning);
    let boosted = planned(on.efficiency.failure_learning);
    assert!(
        baseline.selected.iter().any(|c| c.id == "b"),
        "flag off: the baseline selector keeps b"
    );
    assert!(
        boosted.selected.iter().any(|c| c.id == "a"),
        "flag on: the prior boosts a into the window"
    );
    assert_ne!(
        baseline, boosted,
        "the parsed flag must decide planner construction"
    );
}
