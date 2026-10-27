//! Boot-time overlay of durable settings onto the daemon file config.
//!
//! The settings UI edits the foundation store, while the agent/retry/sandbox
//! authorities are constructed from the file config. Restart-required
//! descriptors are therefore applied HERE, after the foundation opens and
//! before the daemon graph is built: a saved value has a real consumer, and
//! a descriptor that still has no consumer is locked read-only in
//! `faktor_foundation::config_consumers`.
//!
//! Only EXPLICIT values (source != `default`) override the file config; a
//! descriptor default must never silently replace an operator's config file.

use faktor_foundation::config::Snapshot;

use crate::config;

fn explicit<'a>(snapshot: &'a Snapshot, key: &str) -> Option<&'a serde_json::Value> {
    snapshot
        .values
        .get(key)
        .filter(|value| value.source != "default")
        .map(|value| &value.value)
}

fn explicit_u64(snapshot: &Snapshot, key: &str) -> Option<u64> {
    explicit(snapshot, key).and_then(serde_json::Value::as_u64)
}

fn explicit_f64(snapshot: &Snapshot, key: &str) -> Option<f64> {
    explicit(snapshot, key).and_then(serde_json::Value::as_f64)
}

fn explicit_bool(snapshot: &Snapshot, key: &str) -> Option<bool> {
    explicit(snapshot, key).and_then(serde_json::Value::as_bool)
}

fn explicit_string(snapshot: &Snapshot, key: &str) -> Option<String> {
    explicit(snapshot, key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Overlay every restart-required, consumer-backed descriptor onto `config`.
pub(crate) fn apply_saved_runtime_settings(
    config: config::Config,
    foundation: &faktor_foundation::Foundation,
) -> Result<config::Config, String> {
    let snapshot = foundation
        .config
        .snapshot()
        .map_err(|error| format!("reading durable settings: {error}"))?;
    let mut config = config;

    if let Some(value) = explicit_u64(&snapshot, "faktor.retry.maxAttempts") {
        config.retry.max_attempts = (value as u32).clamp(1, 8);
    }
    if let Some(value) = explicit_u64(&snapshot, "faktor.retry.baseDelayMs") {
        config.retry.base_delay_ms = value;
    }
    if let Some(value) = explicit_u64(&snapshot, "faktor.retry.maxDelayMs") {
        config.retry.max_delay_ms = value.max(config.retry.base_delay_ms).max(1);
    }
    if let Some(value) = explicit_f64(&snapshot, "faktor.retry.jitter") {
        config.retry.jitter = value.clamp(0.0, 1.0);
    }

    if let Some(value) = explicit_bool(&snapshot, "faktor.efficiency.failureLearning") {
        config.efficiency.failure_learning = value;
    }
    if let Some(value) = explicit_bool(&snapshot, "faktor.efficiency.ccr") {
        config.efficiency.ccr = value;
    }
    if let Some(value) = explicit_bool(&snapshot, "faktor.efficiency.typedHandoff") {
        config.efficiency.typed_handoff = value;
    }
    // The descriptor calls this switch "information gain"; the agent flag is
    // `semantic_context` (the same capability). The explicit semantic-context
    // key wins when both are set.
    if let Some(value) = explicit_bool(&snapshot, "faktor.efficiency.informationGain") {
        config.efficiency.semantic_context = value;
    }
    if let Some(value) = explicit_bool(&snapshot, "faktor.efficiency.semanticContext") {
        config.efficiency.semantic_context = value;
    }
    if let Some(value) = explicit_bool(&snapshot, "faktor.efficiency.reworkGuard") {
        config.efficiency.rework_routing = value;
    }

    if explicit(&snapshot, "faktor.compaction.model").is_some() {
        config.compaction_model = explicit_string(&snapshot, "faktor.compaction.model");
    }
    if let Some(value) = explicit_f64(&snapshot, "faktor.context.compactionMinReduction") {
        config.compaction_min_reduction = value.clamp(0.0, 0.999_999);
    }

    let embedding_provider = explicit_string(&snapshot, "faktor.retrieval.embeddingProvider");
    let embedding_model = explicit_string(&snapshot, "faktor.retrieval.embeddingModel");
    match (embedding_provider, embedding_model) {
        (Some(provider), Some(model)) => {
            let policy = config
                .embeddings
                .as_ref()
                .map(|embedding| embedding.policy)
                .unwrap_or_default();
            config.embeddings = Some(config::EmbeddingCfg {
                provider,
                model,
                policy,
            });
        }
        (Some(_), None) | (None, Some(_)) => tracing::warn!(
            "faktor.retrieval.embeddingProvider and embeddingModel must be set together; keeping the file-config embedder"
        ),
        (None, None) => {}
    }

    if let Some(shell) = explicit_string(&snapshot, "faktor.sandbox.shell") {
        match shell.as_str() {
            "os_isolated" => {
                config.sandbox.shell = Some(faktor_sandbox::ShellExecutionMode::OsIsolated);
                config.sandbox.network_guarantee = faktor_sandbox::SandboxGuarantee::Required;
            }
            "network_capable_user_granted" => {
                config.sandbox.shell =
                    Some(faktor_sandbox::ShellExecutionMode::NetworkCapableUserGranted);
                config.sandbox.network_guarantee = faktor_sandbox::SandboxGuarantee::BestEffort;
            }
            other => {
                return Err(format!(
                    "faktor.sandbox.shell has unsupported value {other:?}; expected os_isolated or network_capable_user_granted"
                ))
            }
        }
    }

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_foundation::config::{Change, Patch, Scope};

    fn commit(foundation: &faktor_foundation::Foundation, key: &str, value: serde_json::Value) {
        let snapshot = foundation.config.snapshot().unwrap();
        foundation
            .config
            .commit(&Patch {
                expected_revision: snapshot.revision,
                changes: vec![Change {
                    key: key.into(),
                    scope: Scope::User,
                    value: Some(value),
                }],
            })
            .unwrap();
    }

    fn foundation() -> (tempfile::TempDir, faktor_foundation::Foundation) {
        let dir = tempfile::tempdir().unwrap();
        let foundation =
            faktor_foundation::Foundation::open(dir.path().join("foundation.db")).unwrap();
        (dir, foundation)
    }

    #[test]
    fn explicit_restart_settings_override_file_config() {
        let (_dir, foundation) = foundation();
        commit(
            &foundation,
            "faktor.retry.maxAttempts",
            serde_json::json!(5),
        );
        commit(&foundation, "faktor.retry.jitter", serde_json::json!(0.4));
        commit(
            &foundation,
            "faktor.efficiency.ccr",
            serde_json::json!(false),
        );
        commit(
            &foundation,
            "faktor.compaction.model",
            serde_json::json!("fake/compact"),
        );
        commit(
            &foundation,
            "faktor.context.compactionMinReduction",
            serde_json::json!(0.4),
        );
        // The embedder is a cross-validated pair: one commit sets both.
        let snapshot = foundation.config.snapshot().unwrap();
        foundation
            .config
            .commit(&Patch {
                expected_revision: snapshot.revision,
                changes: vec![
                    Change {
                        key: "faktor.retrieval.embeddingProvider".into(),
                        scope: Scope::User,
                        value: Some(serde_json::json!("fake")),
                    },
                    Change {
                        key: "faktor.retrieval.embeddingModel".into(),
                        scope: Scope::User,
                        value: Some(serde_json::json!("embed")),
                    },
                ],
            })
            .unwrap();
        commit(
            &foundation,
            "faktor.sandbox.shell",
            serde_json::json!("network_capable_user_granted"),
        );
        let applied = apply_saved_runtime_settings(config::Config::default(), &foundation).unwrap();
        assert_eq!(applied.retry.max_attempts, 5);
        assert!((applied.retry.jitter - 0.4).abs() < f64::EPSILON);
        assert!(!applied.efficiency.ccr);
        assert_eq!(applied.compaction_model.as_deref(), Some("fake/compact"));
        assert!((applied.compaction_min_reduction - 0.4).abs() < f64::EPSILON);
        assert_eq!(applied.embeddings.unwrap().model, "embed");
        assert_eq!(
            applied.sandbox.shell,
            Some(faktor_sandbox::ShellExecutionMode::NetworkCapableUserGranted)
        );
        assert_eq!(
            applied.sandbox.network_guarantee,
            faktor_sandbox::SandboxGuarantee::BestEffort
        );
    }

    #[test]
    fn descriptor_defaults_never_override_the_file_config() {
        let (_dir, foundation) = foundation();
        let mut file = config::Config::default();
        file.retry.max_attempts = 7;
        file.efficiency.ccr = false;
        file.compaction_model = Some("file/model".into());
        let applied = apply_saved_runtime_settings(file, &foundation).unwrap();
        assert_eq!(applied.retry.max_attempts, 7);
        assert!(!applied.efficiency.ccr);
        assert_eq!(applied.compaction_model.as_deref(), Some("file/model"));
    }

    #[test]
    fn a_half_configured_embedder_is_refused_before_it_can_be_stored() {
        let (_dir, foundation) = foundation();
        let snapshot = foundation.config.snapshot().unwrap();
        let refused = foundation.config.commit(&Patch {
            expected_revision: snapshot.revision,
            changes: vec![Change {
                key: "faktor.retrieval.embeddingProvider".into(),
                scope: Scope::User,
                value: Some(serde_json::json!("fake")),
            }],
        });
        assert!(refused.is_err(), "a provider without a model must not save");
    }
}
