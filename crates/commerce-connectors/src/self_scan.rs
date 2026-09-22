//! Adversarial guards on this crate's own shape (spec §2, §16).
//!
//! Three invariants are enforced here instead of in a review comment:
//!
//! 1. **exact money** — no floating-point type or operation may appear
//!    anywhere in the production sources;
//! 2. **no self-created client, no browser, no model crate** — the
//!    dependency graph contains no HTTP client, no Chromium driver, no
//!    reasoning runtime and no model adapter;
//! 3. **no process spawning** — connectors never launch anything; the
//!    browser and the transport are injected seams.

const SOURCES: &[(&str, &str)] = &[
    ("lib.rs", include_str!("lib.rs")),
    ("browser.rs", include_str!("browser.rs")),
    ("config.rs", include_str!("config.rs")),
    ("context.rs", include_str!("context.rs")),
    ("contract.rs", include_str!("contract.rs")),
    ("http.rs", include_str!("http.rs")),
    ("normalize.rs", include_str!("normalize.rs")),
    ("quota.rs", include_str!("quota.rs")),
    ("secrets.rs", include_str!("secrets.rs")),
    ("testing.rs", include_str!("testing.rs")),
    ("mouser/mod.rs", include_str!("mouser/mod.rs")),
    ("mouser/normalize.rs", include_str!("mouser/normalize.rs")),
    ("digikey/mod.rs", include_str!("digikey/mod.rs")),
    ("digikey/normalize.rs", include_str!("digikey/normalize.rs")),
    ("lcsc/mod.rs", include_str!("lcsc/mod.rs")),
    ("lcsc/normalize.rs", include_str!("lcsc/normalize.rs")),
    ("alibaba/mod.rs", include_str!("alibaba/mod.rs")),
    ("alibaba/normalize.rs", include_str!("alibaba/normalize.rs")),
    ("china1688/mod.rs", include_str!("china1688/mod.rs")),
    (
        "china1688/normalize.rs",
        include_str!("china1688/normalize.rs"),
    ),
];

/// Remove comments, string literals and raw strings so mentions inside
/// documentation or fixtures cannot mask (or fake) a violation.
fn strip_comments_and_literals(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut index = 0usize;
    while index < chars.len() {
        let current = chars[index];
        if current == '/' && index + 1 < chars.len() && chars[index + 1] == '/' {
            while index < chars.len() && chars[index] != '\n' {
                index += 1;
            }
        } else if current == '/' && index + 1 < chars.len() && chars[index + 1] == '*' {
            index += 2;
            let mut depth = 1usize;
            while index < chars.len() && depth > 0 {
                if chars[index] == '/' && index + 1 < chars.len() && chars[index + 1] == '*' {
                    depth += 1;
                    index += 2;
                } else if chars[index] == '*' && index + 1 < chars.len() && chars[index + 1] == '/'
                {
                    depth -= 1;
                    index += 2;
                } else {
                    index += 1;
                }
            }
        } else if current == '"' {
            index += 1;
            while index < chars.len() {
                if chars[index] == '\\' {
                    index += 2;
                } else if chars[index] == '"' {
                    index += 1;
                    break;
                } else {
                    index += 1;
                }
            }
        } else if current == 'r' && index + 1 < chars.len() && chars[index + 1] == '"' {
            index += 1;
            let mut hashes = 0usize;
            while index < chars.len() && chars[index] == '#' {
                hashes += 1;
                index += 1;
            }
            if index < chars.len() && chars[index] == '"' {
                index += 1;
                loop {
                    if index >= chars.len() {
                        break;
                    }
                    if chars[index] == '"' {
                        let mut matched = true;
                        for offset in 1..=hashes {
                            if index + offset >= chars.len() || chars[index + offset] != '#' {
                                matched = false;
                                break;
                            }
                        }
                        if matched {
                            index += 1 + hashes;
                            break;
                        }
                    }
                    index += 1;
                }
            }
        } else {
            out.push(current);
            index += 1;
        }
    }
    out
}

/// True when `needle` appears as a standalone identifier token.
fn contains_token(haystack: &str, needle: &str) -> bool {
    let boundary = |character: Option<char>| {
        character
            .map(|character| !(character.is_alphanumeric() || character == '_'))
            .unwrap_or(true)
    };
    let mut start = 0usize;
    while let Some(offset) = haystack[start..].find(needle) {
        let absolute = start + offset;
        let before = haystack[..absolute].chars().last();
        let after = haystack[absolute + needle.len()..].chars().next();
        if boundary(before) && boundary(after) {
            return true;
        }
        start = absolute + needle.len();
    }
    false
}

#[test]
fn production_sources_never_mention_a_floating_type() {
    let single_precision = concat!("f", "32");
    let double_precision = concat!("f", "64");
    for (name, source) in SOURCES {
        let stripped = strip_comments_and_literals(source);
        for needle in [single_precision, double_precision] {
            assert!(
                !contains_token(&stripped, needle),
                "crates/commerce-connectors/src/{name} mentions {needle:?}: the connector layer is integer-only"
            );
        }
    }
}

#[test]
fn dependency_graph_has_no_http_client_browser_or_model_crate() {
    let manifest = include_str!("../Cargo.toml");
    // Only the production dependency section counts; the dev-only test
    // runtime is not part of the shipped graph.
    let dependencies = manifest
        .split("[dev-dependencies]")
        .next()
        .unwrap_or(manifest);
    for forbidden in [
        "reqwest",
        "hyper",
        "ureq",
        "isahc",
        "curl",
        "chromiumoxide",
        "headless_chrome",
        "fantoccini",
        "tokio",
        "faktor-agent",
        "faktor-router",
        "faktor-provider",
        "openai",
        "anthropic",
        "google",
        "ollama",
        "deepseek",
    ] {
        assert!(
            !dependencies.contains(forbidden),
            "faktor-commerce-connectors must not depend on {forbidden:?}"
        );
    }
    for required in ["faktor-commerce", "faktor-security", "async-trait"] {
        assert!(
            dependencies.contains(required),
            "faktor-commerce-connectors must depend on {required:?}"
        );
    }
}

#[test]
fn connectors_never_spawn_a_process() {
    for (name, source) in SOURCES {
        let stripped = strip_comments_and_literals(source);
        for needle in ["Command::new", "std::process", "process::Command"] {
            assert!(
                !stripped.contains(needle),
                "crates/commerce-connectors/src/{name} spawns or references a process: connectors only use injected seams"
            );
        }
    }
}
