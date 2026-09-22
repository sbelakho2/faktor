//! Adversarial guards on the crate's own shape.
//!
//! Two invariants of `docs/acquire.md` are enforced here rather than in a
//! review comment:
//!
//! 1. exact money — no floating-point type, conversion or method may appear
//!    anywhere in the production sources;
//! 2. the dependency graph is domain-only — no model adapter, no agent
//!    reasoning runtime, no router model execution, no HTTP client, no
//!    browser.

const SOURCES: &[(&str, &str)] = &[
    ("bom.rs", include_str!("bom.rs")),
    ("cache.rs", include_str!("cache.rs")),
    ("connector.rs", include_str!("connector.rs")),
    ("error.rs", include_str!("error.rs")),
    ("identity.rs", include_str!("identity.rs")),
    ("jobs.rs", include_str!("jobs.rs")),
    ("matching.rs", include_str!("matching.rs")),
    ("money.rs", include_str!("money.rs")),
    ("offer.rs", include_str!("offer.rs")),
    ("packaging.rs", include_str!("packaging.rs")),
    ("quantity.rs", include_str!("quantity.rs")),
    ("query.rs", include_str!("query.rs")),
    ("quote.rs", include_str!("quote.rs")),
    ("ranking.rs", include_str!("ranking.rs")),
    ("reconcile.rs", include_str!("reconcile.rs")),
    ("result.rs", include_str!("result.rs")),
    ("service.rs", include_str!("service.rs")),
    ("store.rs", include_str!("store.rs")),
    ("text.rs", include_str!("text.rs")),
];

#[test]
fn production_sources_never_mention_a_floating_type() {
    let single_precision = concat!("f", "32");
    let double_precision = concat!("f", "64");
    let needles = [
        single_precision,
        double_precision,
        "powf(",
        "is_nan(",
        "is_finite(",
        "to_bits(",
        "from_bits(",
        "EPSILON",
        "NAN",
        "INFINITY",
    ];
    for (name, source) in SOURCES {
        // The ONLY permitted mention of a floating type is the serde
        // visitor signature that *rejects* floating-point input
        // (`fn visit_f64(..) -> Err`), which is a guard, not a path.
        let guarded: String = source
            .lines()
            .filter(|line| {
                let trimmed = line.trim_start();
                !trimmed.starts_with("fn visit_f64") && !trimmed.starts_with("fn visit_f32")
            })
            .collect::<Vec<_>>()
            .join("\n");
        for needle in needles {
            assert!(
                !guarded.contains(needle),
                "crates/commerce/src/{name} contains {needle:?}: the commerce domain is integer-only"
            );
        }
    }
}

#[test]
fn dependency_graph_is_domain_only() {
    let manifest = include_str!("../Cargo.toml");
    // faktor-commerce owns the commerce domain AND its durable store/jobs
    // (spec §3: "domain, matching, quotes, store, jobs"), so the embedded
    // SQLite store and tokio for async jobs are part of this crate. What
    // must never appear is a model adapter, an agent reasoning runtime, a
    // router model execution, an HTTP client or a browser.
    for forbidden in [
        "faktor-agent",
        "faktor-router",
        "faktor-provider",
        "faktor-orchestrator",
        "faktor-server",
        "openai",
        "anthropic",
        "google",
        "ollama",
        "deepseek",
        "reqwest",
        "hyper",
        "chromiumoxide",
        "headless",
    ] {
        assert!(
            !manifest.contains(forbidden),
            "crates/commerce/Cargo.toml must not depend on {forbidden:?}"
        );
    }
    for required in ["faktor-core", "serde", "thiserror"] {
        assert!(
            manifest.contains(required),
            "crates/commerce/Cargo.toml must depend on {required:?}"
        );
    }
}
