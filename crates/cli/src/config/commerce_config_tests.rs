use super::*;

fn parse(document: serde_json::Value) -> Result<Config, serde_json::Error> {
    let mut object = serde_json::json!({"config_version": 1});
    if let Some(section) = document.as_object() {
        for (key, value) in section {
            object[key] = value.clone();
        }
    } else {
        object = document;
    }
    serde_json::from_value(object)
}

fn parse_commerce(section: serde_json::Value) -> Result<Config, serde_json::Error> {
    parse(serde_json::json!({ "commerce": section }))
}

fn spec_example() -> serde_json::Value {
    serde_json::json!({
        "enabled": true,
        "database": "commerce.db",
        "cache": {
            "discovery_ttl_s": 1800,
            "product_ttl_s": 21600,
            "price_ttl_s": 1800,
            "stock_ttl_s": 900,
            "supplier_ttl_s": 86400
        },
        "browser": {
            "enabled": true,
            "executable": null,
            "headless": true,
            "idle_shutdown_s": 300,
            "max_browsers": 2,
            "max_pages_per_profile": 1
        },
        "connectors": {
            "1688": { "enabled": true, "profile": "procurement-cn" },
            "alibaba": { "enabled": true, "profile": "procurement-global" },
            "lcsc": { "enabled": true, "api_key_env": "FAKTOR_LCSC_KEY" },
            "mouser": { "enabled": true, "api_key_env": "FAKTOR_MOUSER_KEY" },
            "digikey": {
                "enabled": true,
                "client_id_env": "FAKTOR_DIGIKEY_CLIENT_ID",
                "client_secret_env": "FAKTOR_DIGIKEY_CLIENT_SECRET"
            }
        }
    })
}

#[test]
fn absent_and_disabled_commerce_sections_are_byte_identical_defaults() {
    let defaults = CommerceCfg::default();
    let absent = parse(serde_json::json!({})).unwrap().commerce;
    let empty = parse_commerce(serde_json::json!({})).unwrap().commerce;
    let disabled = parse_commerce(serde_json::json!({"enabled": false}))
        .unwrap()
        .commerce;
    assert_eq!(absent, defaults);
    assert_eq!(empty, defaults);
    assert_eq!(disabled, defaults);
    assert!(!defaults.enabled);
    assert_eq!(defaults.database_name(), "commerce.db");
    assert_eq!(defaults.cache.discovery_ttl_s, 1800);
    assert_eq!(defaults.cache.product_ttl_s, 21_600);
    assert_eq!(defaults.cache.price_ttl_s, 1800);
    assert_eq!(defaults.cache.stock_ttl_s, 900);
    assert_eq!(defaults.cache.supplier_ttl_s, 86_400);
    assert!(!defaults.browser.enabled);
    assert!(defaults.browser.headless);
    assert_eq!(defaults.browser.idle_shutdown_s, 300);
    assert_eq!(defaults.browser.max_browsers, 2);
    assert_eq!(defaults.browser.max_pages_per_profile, 1);
    assert!(defaults.connectors.enabled().is_empty());
    defaults.validate().unwrap();
}

#[test]
fn spec_example_parses_and_validates() {
    let cfg = parse_commerce(spec_example()).unwrap();
    let commerce = &cfg.commerce;
    assert!(commerce.enabled);
    assert_eq!(commerce.connectors.enabled().len(), 5);
    commerce.validate().unwrap();
    cfg.validate().unwrap();
    assert!(matches!(
        commerce
            .connectors
            .mouser
            .as_ref()
            .unwrap()
            .api_key_env
            .as_deref(),
        Some("FAKTOR_MOUSER_KEY")
    ));
    assert!(matches!(
        commerce
            .connectors
            .digikey
            .as_ref()
            .unwrap()
            .client_secret_env
            .as_deref(),
        Some("FAKTOR_DIGIKEY_CLIENT_SECRET")
    ));
}

#[test]
fn disabled_connector_resolves_like_the_absent_key() {
    let absent = parse_commerce(serde_json::json!({"enabled": true}))
        .unwrap()
        .commerce;
    let explicitly_off = parse_commerce(serde_json::json!({
        "enabled": true,
        "connectors": {"mouser": {"enabled": false, "api_key_env": "X"}}
    }))
    .unwrap()
    .commerce;
    assert_eq!(absent, explicitly_off);
    assert!(explicitly_off.connectors.mouser.is_none());
}

#[test]
fn unknown_keys_are_startup_errors_everywhere() {
    for section in [
        serde_json::json!({"enabld": true}),
        serde_json::json!({"cache": {"discovery_ttl": 1}}),
        serde_json::json!({"browser": {"idle_shutdown": 1}}),
        serde_json::json!({"connectors": {"amazon": {"enabled": true}}}),
        serde_json::json!({"connectors": {"mouser": {"enabled": true, "key": "x"}}}),
        serde_json::json!({"connectors": {"digikey": {"enabled": true, "client_id": "x"}}}),
        serde_json::json!({"connectors": {"1688": {"enabled": true, "cookies": []}}}),
    ] {
        let error = parse_commerce(section).expect_err("unknown key must fail startup");
        assert!(
            error.to_string().contains("unknown field"),
            "expected an unknown-field error, got: {error}"
        );
    }
}

#[test]
fn credentials_are_env_var_names_only() {
    // A literal secret pasted into `*_env` fails validation.
    let cfg = parse_commerce(serde_json::json!({
        "enabled": true,
        "connectors": {"mouser": {"enabled": true, "api_key_env": "sk-live-abc123"}}
    }))
    .unwrap();
    let error = cfg
        .commerce
        .validate()
        .expect_err("a value must not be accepted");
    assert!(error.contains("environment variable NAME"), "{error}");

    let cfg = parse_commerce(serde_json::json!({
        "enabled": true,
        "connectors": {"digikey": {
            "enabled": true,
            "client_id_env": "FAKTOR_DIGIKEY_CLIENT_ID",
            "client_secret_env": "FAKTOR DIGIKEY SECRET"
        }}
    }))
    .unwrap();
    assert!(cfg.commerce.validate().is_err());

    // Missing env names and missing profiles are startup errors.
    let cfg = parse_commerce(serde_json::json!({
        "enabled": true,
        "connectors": {"lcsc": {"enabled": true}}
    }))
    .unwrap();
    assert!(cfg.commerce.validate().is_err());
    let cfg = parse_commerce(serde_json::json!({
        "enabled": true,
        "connectors": {"1688": {"enabled": true}}
    }))
    .unwrap();
    assert!(cfg.commerce.validate().is_err());

    // A valid name-shaped config validates.
    let cfg = parse_commerce(serde_json::json!({
        "enabled": true,
        "connectors": {"lcsc": {"enabled": true, "api_key_env": "FAKTOR_LCSC_KEY"}}
    }))
    .unwrap();
    cfg.commerce.validate().unwrap();
}

#[test]
fn debug_redacts_credential_shaped_fields() {
    let cfg = parse_commerce(spec_example()).unwrap();
    let debug = format!("{:?}", cfg.commerce);
    assert!(debug.contains("<redacted>"), "{debug}");
    for secret_shaped in [
        "FAKTOR_LCSC_KEY",
        "FAKTOR_MOUSER_KEY",
        "FAKTOR_DIGIKEY_CLIENT_ID",
        "FAKTOR_DIGIKEY_CLIENT_SECRET",
    ] {
        assert!(
            !debug.contains(secret_shaped),
            "Debug output must not carry {secret_shaped}: {debug}"
        );
    }
    // Profiles are operator config (not credential-shaped) and remain
    // visible for diagnosis.
    assert!(debug.contains("procurement-cn"));
    // The wire/serialized config keeps the NAMES (a name is not a value;
    // `Config::save` round-trips).
    let json = serde_json::to_value(&cfg.commerce).unwrap();
    assert_eq!(
        json["connectors"]["mouser"]["api_key_env"],
        "FAKTOR_MOUSER_KEY"
    );
}

#[test]
fn marketplace_connector_shape_carries_identity_and_api_names() {
    let cfg = parse_commerce(serde_json::json!({
        "enabled": true,
        "browser": {"enabled": true},
        "connectors": {
            "1688": {
                "enabled": true,
                "browser_profile": "procurement-cn",
                "account_scope": "acct-cn-1",
                "api": {
                    "app_key_env": "FAKTOR_1688_APP_KEY",
                    "app_secret_env": "FAKTOR_1688_APP_SECRET",
                    "scopes": ["discovery", "product", "price", "variants"]
                }
            },
            "alibaba": {
                "enabled": true,
                "browser_profile": "procurement-global",
                "api": {
                    "app_key_env": "FAKTOR_ALIBABA_APP_KEY",
                    "app_secret_env": "FAKTOR_ALIBABA_APP_SECRET",
                    "access_token_env": "FAKTOR_ALIBABA_TOKEN",
                    "access_token_expires_at_env": "FAKTOR_ALIBABA_TOKEN_EXPIRES_AT",
                    "scopes": ["buyer_discovery", "trade_terms"]
                }
            }
        }
    }))
    .unwrap();
    let commerce = &cfg.commerce;
    commerce.validate().unwrap();
    let entry = commerce.connectors.china1688.as_ref().expect("1688 entry");
    assert_eq!(entry.browser_profile(), Some("procurement-cn"));
    assert_eq!(entry.account_scope(), Some("acct-cn-1"));
    assert_eq!(entry.market(), None, "the marketplace shape has no market");
    let api = entry.api().expect("api");
    assert_eq!(api.app_key_env.as_deref(), Some("FAKTOR_1688_APP_KEY"));
    assert_eq!(
        api.app_secret_env.as_deref(),
        Some("FAKTOR_1688_APP_SECRET")
    );
    assert_eq!(
        api.scopes,
        vec!["discovery", "product", "price", "variants"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
    );
    let alibaba = commerce.connectors.alibaba.as_ref().expect("alibaba");
    assert_eq!(
        alibaba
            .api()
            .and_then(|api| api.access_token_expires_at_env.as_deref()),
        Some("FAKTOR_ALIBABA_TOKEN_EXPIRES_AT")
    );
}

#[test]
fn profile_shape_carries_account_scope_market_and_locale() {
    let cfg = parse_commerce(serde_json::json!({
        "enabled": true,
        "browser": {"enabled": true},
        "connectors": {
            "1688": {
                "enabled": true,
                "profile": "procurement-cn",
                "account_scope": "acct-cn-1",
                "market": "CN",
                "locale": "zh-CN"
            },
            "alibaba": {"enabled": true, "browser_profile": null, "account_scope": "acct-global"}
        }
    }))
    .unwrap();
    cfg.commerce.validate().unwrap();
    let entry = cfg.commerce.connectors.china1688.as_ref().unwrap();
    assert_eq!(entry.account_scope(), Some("acct-cn-1"));
    assert_eq!(entry.market(), Some("CN"));
    assert_eq!(entry.locale(), Some("zh-CN"));
    // The marketplace shape accepts a null browser profile (the default
    // profile) plus an account scope.
    let alibaba = cfg.commerce.connectors.alibaba.as_ref().unwrap();
    assert_eq!(alibaba.browser_profile(), None);
    assert_eq!(alibaba.account_scope(), Some("acct-global"));
    assert!(alibaba.api().is_none());
}

#[test]
fn marketplace_api_is_strict_and_never_carries_values() {
    // Unknown keys anywhere in the section (including `api`) fail.
    for connectors in [
        serde_json::json!({"1688": {"enabled": true, "api": {"key": "x"}}}),
        serde_json::json!({"1688": {"enabled": true, "profile": "p", "api": {"app_key_env": "A"}}}),
        serde_json::json!({"1688": {"enabled": true, "browser_profile": "p", "market": "CN"}}),
        serde_json::json!({"1688": {"enabled": true, "api": {"app_key_env": "A", "secret": "s"}}}),
    ] {
        let error = parse_commerce(serde_json::json!({
            "enabled": true,
            "browser": {"enabled": true},
            "connectors": connectors
        }))
        .expect_err("strict connector section");
        let text = error.to_string();
        assert!(
            text.contains("unknown field") || text.contains("mixes"),
            "{text}"
        );
    }

    // The api block requires app_key_env + app_secret_env + at least one
    // known scope; the token pair is all-or-nothing.
    for api in [
        serde_json::json!({"app_secret_env": "S", "scopes": ["discovery"]}),
        serde_json::json!({"app_key_env": "K", "scopes": ["discovery"]}),
        serde_json::json!({"app_key_env": "K", "app_secret_env": "S"}),
        serde_json::json!({"app_key_env": "K", "app_secret_env": "S", "scopes": []}),
        serde_json::json!({"app_key_env": "K", "app_secret_env": "S", "scopes": ["nope"]}),
        serde_json::json!({"app_key_env": "K", "app_secret_env": "S", "access_token_env": "T", "scopes": ["discovery"]}),
        serde_json::json!({"app_key_env": "K", "app_secret_env": "S", "access_token_expires_at_env": "E", "scopes": ["discovery"]}),
        serde_json::json!({"app_key_env": "sk-live-123", "app_secret_env": "S", "scopes": ["discovery"]}),
    ] {
        let cfg = parse_commerce(serde_json::json!({
            "enabled": true,
            "browser": {"enabled": true},
            "connectors": {"1688": {"enabled": true, "api": api.clone()}}
        }))
        .unwrap();
        assert!(
            cfg.commerce.validate().is_err(),
            "api {api} must be refused"
        );
    }

    // A browser-disabled marketplace entry with no api has no usable
    // path and must not silently validate.
    let cfg = parse_commerce(serde_json::json!({
        "enabled": true,
        "connectors": {"1688": {"enabled": true, "account_scope": "acct"}}
    }))
    .unwrap();
    assert!(cfg.commerce.validate().is_err());

    // Identity values go through the domain's bounded text type.
    for bad in [serde_json::json!(""), serde_json::json!("\u{0007}")] {
        let cfg = parse_commerce(serde_json::json!({
            "enabled": true,
            "browser": {"enabled": true},
            "connectors": {"1688": {"enabled": true, "account_scope": bad}}
        }))
        .unwrap();
        assert!(cfg.commerce.validate().is_err(), "bad identity must fail");
    }
}

#[test]
fn marketplace_api_debug_and_serialization_keep_names_only() {
    let cfg = parse_commerce(serde_json::json!({
        "enabled": true,
        "browser": {"enabled": true},
        "connectors": {"1688": {
            "enabled": true,
            "api": {
                "app_key_env": "FAKTOR_1688_APP_KEY",
                "app_secret_env": "FAKTOR_1688_APP_SECRET",
                "scopes": ["discovery", "price"]
            }
        }}
    }))
    .unwrap();
    let debug = format!("{:?}", cfg.commerce);
    assert!(debug.contains("<redacted>"), "{debug}");
    for name in ["FAKTOR_1688_APP_KEY", "FAKTOR_1688_APP_SECRET"] {
        assert!(!debug.contains(name), "Debug leaked {name}: {debug}");
    }
    assert!(debug.contains("discovery"), "scopes stay visible: {debug}");
    let json = serde_json::to_value(&cfg.commerce).unwrap();
    assert_eq!(
        json["connectors"]["1688"]["api"]["app_key_env"],
        "FAKTOR_1688_APP_KEY"
    );
    assert_eq!(
        json["connectors"]["1688"]["api"]["scopes"],
        serde_json::json!(["discovery", "price"])
    );
    // And the two shapes round-trip through save/load.
    let text = serde_json::to_string(&cfg.commerce).unwrap();
    let parsed: CommerceCfg = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed, cfg.commerce);
}

#[test]
fn nested_enabled_sections_require_commerce_enabled() {
    let cfg = parse_commerce(serde_json::json!({
        "enabled": false,
        "connectors": {"mouser": {"enabled": true, "api_key_env": "FAKTOR_MOUSER_KEY"}}
    }))
    .unwrap();
    assert!(cfg.commerce.validate().is_err());

    let cfg = parse_commerce(serde_json::json!({
        "enabled": false,
        "browser": {"enabled": true}
    }))
    .unwrap();
    assert!(cfg.commerce.validate().is_err());
}

#[test]
fn hostile_bounds_are_refused() {
    for section in [
        serde_json::json!({"enabled": true, "cache": {"stock_ttl_s": 0}}),
        serde_json::json!({"enabled": true, "cache": {"stock_ttl_s": 99_999_999_999_u64}}),
        serde_json::json!({"enabled": true, "browser": {"max_browsers": 0}}),
        serde_json::json!({"enabled": true, "browser": {"max_pages_per_profile": 9}}),
        serde_json::json!({"enabled": true, "browser": {"idle_shutdown_s": 0}}),
        serde_json::json!({"enabled": true, "database": "../escape.db"}),
        serde_json::json!({"enabled": true, "database": "other.db"}),
        serde_json::json!({"enabled": true, "connectors": {"1688": {"enabled": true, "profile": "../x"}}}),
    ] {
        let cfg = parse_commerce(section.clone()).unwrap();
        assert!(
            cfg.commerce.validate().is_err(),
            "{section} must be refused at validation"
        );
    }
}
