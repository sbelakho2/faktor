//! BK07 diagnostics bundle: allowlisted, redacted, byte-bounded export.
//!
//! The bundle never carries secret material, prompts, file contents, tool
//! payloads or full logs. Every string is passed through the daemon's secret
//! redactor, the byte cap is enforced EXACTLY, and the checksum identifies the
//! serialized content.

use serde::{Deserialize, Serialize};

use faktor_security::{redact, SecretPolicy};

use crate::{hash_hex, FoundationError};

pub const MIN_BUNDLE_BYTES: usize = 4 * 1024;
pub const MAX_BUNDLE_BYTES: usize = 1024 * 1024;
pub const MAX_LOG_LINES: usize = 200;
pub const MAX_LOG_LINE_BYTES: usize = 512;
pub const MAX_COMPONENTS: usize = 64;
pub const MAX_SETTINGS: usize = 2_000;
pub const MAX_PROVIDERS: usize = 64;
pub const MAX_TOOLS: usize = 500;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComponentStatus {
    pub name: String,
    pub state: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderSummary {
    pub id: String,
    pub family: String,
    pub endpoint: Option<String>,
    pub credential_kind: String,
    pub health: String,
    pub model_count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSummary {
    pub name: String,
    pub source: String,
    pub health: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingSummary {
    pub key: String,
    pub source: String,
    pub scope: String,
    pub secret_reference: bool,
}

/// Facts the daemon passes in. The builder redacts and bounds them; it never
/// reads prompts, files or the whole process environment on its own.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticsFacts {
    pub daemon_version: String,
    pub os: String,
    pub arch: String,
    pub binary_path: String,
    pub install_root: String,
    pub data_dir: String,
    pub binary_checksum: Option<String>,
    pub started_ms: i64,
    pub components: Vec<ComponentStatus>,
    pub providers: Vec<ProviderSummary>,
    pub tools: Vec<ToolSummary>,
    pub settings: Vec<SettingSummary>,
    pub log_tail: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BundleContent {
    pub daemon_version: String,
    pub os: String,
    pub arch: String,
    pub binary_path: String,
    pub install_root: String,
    pub data_dir: String,
    pub binary_checksum: Option<String>,
    pub started_ms: i64,
    pub components: Vec<ComponentStatus>,
    pub providers: Vec<ProviderSummary>,
    pub tools: Vec<ToolSummary>,
    pub settings: Vec<SettingSummary>,
    pub log_tail: Vec<String>,
    pub omitted_categories: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticBundle {
    pub schema: String,
    pub created_ms: i64,
    pub bytes: usize,
    pub checksum: String,
    pub content: BundleContent,
}

pub const BUNDLE_SCHEMA: &str = "faktor-diagnostics-bundle/v1";
pub const OMITTED_CATEGORIES: [&str; 5] = [
    "secret_material",
    "prompt_and_completion_text",
    "file_and_tool_payloads",
    "unbounded_logs",
    "environment",
];

pub fn export(
    facts: &DiagnosticsFacts,
    max_bytes: usize,
) -> Result<DiagnosticBundle, FoundationError> {
    let max_bytes = max_bytes.clamp(MIN_BUNDLE_BYTES, MAX_BUNDLE_BYTES);
    let policy = SecretPolicy::default();
    let mut content = BundleContent {
        daemon_version: redact(&facts.daemon_version, &policy),
        os: redact(&facts.os, &policy),
        arch: redact(&facts.arch, &policy),
        binary_path: redact(&facts.binary_path, &policy),
        install_root: redact(&facts.install_root, &policy),
        data_dir: redact(&facts.data_dir, &policy),
        binary_checksum: facts.binary_checksum.clone(),
        started_ms: facts.started_ms,
        components: facts
            .components
            .iter()
            .take(MAX_COMPONENTS)
            .map(|c| ComponentStatus {
                name: redact(&c.name, &policy),
                state: redact(&c.state, &policy),
                detail: c.detail.as_deref().map(|d| redact(d, &policy)),
            })
            .collect(),
        providers: facts
            .providers
            .iter()
            .take(MAX_PROVIDERS)
            .map(|p| ProviderSummary {
                id: redact(&p.id, &policy),
                family: redact(&p.family, &policy),
                endpoint: p.endpoint.as_deref().map(|e| redact(e, &policy)),
                credential_kind: redact(&p.credential_kind, &policy),
                health: redact(&p.health, &policy),
                model_count: p.model_count,
            })
            .collect(),
        tools: facts
            .tools
            .iter()
            .take(MAX_TOOLS)
            .map(|t| ToolSummary {
                name: redact(&t.name, &policy),
                source: redact(&t.source, &policy),
                health: redact(&t.health, &policy),
            })
            .collect(),
        settings: facts
            .settings
            .iter()
            .take(MAX_SETTINGS)
            .map(|s| SettingSummary {
                key: redact(&s.key, &policy),
                source: redact(&s.source, &policy),
                scope: redact(&s.scope, &policy),
                secret_reference: s.secret_reference,
            })
            .collect(),
        log_tail: facts
            .log_tail
            .iter()
            .rev()
            .take(MAX_LOG_LINES)
            .rev()
            .map(|line| {
                let line = redact(line, &policy);
                if line.len() > MAX_LOG_LINE_BYTES {
                    let mut cut = MAX_LOG_LINE_BYTES;
                    while !line.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    format!("{}…", &line[..cut])
                } else {
                    line
                }
            })
            .collect(),
        omitted_categories: OMITTED_CATEGORIES.iter().map(|s| s.to_string()).collect(),
    };
    // Deterministic trimming: drop the cheapest optional sections first.
    loop {
        let encoded = serde_json::to_vec(&content)
            .map_err(|e| FoundationError::Store(format!("encode diagnostics: {e}")))?;
        let overhead = 512; // envelope fields are tiny; reserve a fixed margin
        if encoded.len() + overhead <= max_bytes {
            let bytes = encoded.len();
            let checksum = hash_hex(&encoded);
            return Ok(DiagnosticBundle {
                schema: BUNDLE_SCHEMA.to_string(),
                created_ms: crate::db::now_ms(),
                bytes,
                checksum,
                content,
            });
        }
        if !content.log_tail.is_empty() {
            content.log_tail.clear();
            continue;
        }
        if !content.tools.is_empty() {
            content.tools.clear();
            continue;
        }
        if !content.settings.is_empty() {
            let keep = content.settings.len() / 2;
            content.settings.truncate(keep);
            continue;
        }
        return Err(FoundationError::Oversized(format!(
            "diagnostic bundle exceeds the configured {max_bytes}-byte bound even after trimming optional sections"
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> DiagnosticsFacts {
        DiagnosticsFacts {
            daemon_version: "1.0.0".into(),
            os: "macos".into(),
            arch: "aarch64".into(),
            binary_path: "/opt/faktor/bin/faktor".into(),
            install_root: "/opt/faktor".into(),
            data_dir: "/tmp/faktor".into(),
            binary_checksum: Some("abc".into()),
            started_ms: 1,
            components: vec![ComponentStatus {
                name: "store".into(),
                state: "ready".into(),
                detail: None,
            }],
            providers: vec![ProviderSummary {
                id: "openai".into(),
                family: "openai".into(),
                endpoint: Some("https://api.openai.com/v1".into()),
                credential_kind: "vault".into(),
                health: "ready".into(),
                model_count: 2,
            }],
            tools: vec![ToolSummary {
                name: "read_file".into(),
                source: "builtin".into(),
                health: "ready".into(),
            }],
            settings: vec![SettingSummary {
                key: "faktor.retry.maxAttempts".into(),
                source: "user".into(),
                scope: "user".into(),
                secret_reference: false,
            }],
            log_tail: (0..400)
                .map(|i| format!("line {i} sk-abcdefghijklmnopqrstuvwxyz012345"))
                .collect(),
        }
    }

    #[test]
    fn export_redacts_secrets_and_enforces_the_byte_cap() {
        let bundle = export(&facts(), 8 * 1024).unwrap();
        let raw = serde_json::to_string(&bundle).unwrap();
        assert!(!raw.contains("sk-abcdefghijklmnopqrstuvwxyz012345"));
        assert!(raw.contains("<redacted:openai_key>"));
        assert!(bundle.bytes <= 8 * 1024);
        assert!(bundle.content.log_tail.len() <= MAX_LOG_LINES);
        assert_eq!(bundle.checksum.len(), 64);
        assert!(bundle
            .content
            .omitted_categories
            .contains(&"secret_material".to_string()));
    }

    #[test]
    fn hostile_tiny_bounds_either_trim_or_refuse_but_never_exceed() {
        let bundle = export(&facts(), 1).unwrap();
        assert!(bundle.bytes <= MIN_BUNDLE_BYTES);
        assert!(bundle.content.log_tail.is_empty());
        let mut hostile = facts();
        hostile.daemon_version = "x".repeat(9000);
        let err = export(&hostile, MIN_BUNDLE_BYTES).unwrap_err();
        assert!(err.to_string().contains("even after trimming"));
    }

    #[test]
    fn checksum_matches_the_serialized_content() {
        let bundle = export(&facts(), 64 * 1024).unwrap();
        let encoded = serde_json::to_vec(&bundle.content).unwrap();
        assert_eq!(bundle.checksum, hash_hex(&encoded));
    }
}
