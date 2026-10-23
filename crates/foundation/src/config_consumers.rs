//! Runtime-consumer truth for every configuration descriptor.
//!
//! A descriptor is only writable when a production path actually reads the
//! key. This table is the one place that answers that question, and the
//! settings API serves the same answer so a control can render
//! "stored only, no runtime consumer" instead of pretending a save applied.
//!
//! `Applied` entries name the module that consumes the key. `StoredOnly`
//! entries are deliberately kept for compatibility/import but are marked
//! read-only: a save that cannot have an effect is refused at validation.
//! `Unknown` is a fail-closed signal for a newly added descriptor that has
//! not been classified yet; the wiring matrix check turns it red.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RuntimeConsumer {
    Applied { consumer: &'static str },
    StoredOnly { reason: &'static str },
    Unknown,
}

impl RuntimeConsumer {
    pub fn is_applied(self) -> bool {
        matches!(self, RuntimeConsumer::Applied { .. })
    }

    pub fn is_stored_only(self) -> bool {
        matches!(self, RuntimeConsumer::StoredOnly { .. })
    }
}

/// Classify one descriptor key.
pub fn runtime_consumer(key: &str) -> RuntimeConsumer {
    match key {
        // Read by the daemon's context/capacity path.
        "faktor.context.ignorePatterns"
        | "faktor.context.protectedPatterns"
        | "faktor.context.outputReserve"
        | "faktor.context.safetyReserve"
        | "faktor.context.compactionMinReduction" => applied("crates/server/src/native/foundation.rs"),
        "faktor.model.contextLimit" => applied("crates/server/src/native/foundation.rs"),

        // Read at task/session admission.
        "faktor.agent.maxTurns" => applied("crates/server/src/native/task.rs"),
        "faktor.defaultProvider" | "faktor.defaultModel" => {
            applied("crates/server/src/native/session.rs")
        }
        "faktor.run.defaultProvider" | "faktor.run.defaultModel" => {
            applied("crates/server/src/native/task.rs")
        }
        "faktor.budget.maxCostMicro" | "faktor.budget.maxTokens" => {
            applied("crates/server/src/native/task.rs")
        }
        "faktor.permissions.preset" => applied("crates/server/src/permission.rs"),
        "faktor.permissions.allowedTools" | "faktor.permissions.maximumChildren" => {
            applied("crates/server/src/native/task.rs")
        }

        // Read by the browser / telemetry / sandbox projection.
        "faktor.sandbox.allowLoopback" => {
            applied("crates/server/src/native/browser_surface.rs")
        }
        "faktor.browser.enabled" => applied("crates/server/src/native/browser_surface.rs"),
        "faktor.telemetry.enabled" | "faktor.telemetry.categories" => {
            applied("crates/server/src/native/foundation_r8.rs")
        }
        "faktor.diagnostics.exportMaxBytes" => applied("crates/foundation/src/lib.rs"),

        // Read by the listing/search routes.
        "faktor.sessions.maxParallel" => applied("crates/server/src/native/foundation_ext.rs"),
        "faktor.session.titleSearch" => applied("crates/server/src/native/foundation_ext.rs"),
        "faktor.transcript.searchLimit" => applied("crates/server/src/native/foundation_ext.rs"),
        "faktor.memory.retentionDays" => applied("crates/server/src/native/longtask.rs"),

        // Restart-required settings overlaid onto the daemon config at boot.
        "faktor.retry.maxAttempts"
        | "faktor.retry.baseDelayMs"
        | "faktor.retry.maxDelayMs"
        | "faktor.retry.jitter" => applied("crates/cli/src/daemon/serve.rs"),
        "faktor.efficiency.failureLearning"
        | "faktor.efficiency.ccr"
        | "faktor.efficiency.typedHandoff"
        | "faktor.efficiency.semanticContext"
        | "faktor.efficiency.informationGain"
        | "faktor.efficiency.reworkGuard" => applied("crates/cli/src/daemon/serve.rs"),
        "faktor.compaction.model" => applied("crates/cli/src/daemon/serve.rs"),
        "faktor.retrieval.embeddingProvider" | "faktor.retrieval.embeddingModel" => {
            applied("crates/cli/src/daemon/serve.rs")
        }
        "faktor.sandbox.shell" => applied("crates/cli/src/daemon/serve.rs"),
        // The session usage projection reads this threshold on every read,
        // so a committed settings change takes effect at the next usage view.
        "faktor.budget.warnAtPercent" => applied("crates/server/src/native/usage.rs"),

        // Read-only runtime facts (never writable, served for provenance).
        "faktor.runtime.binaryPath"
        | "faktor.runtime.dataDir"
        | "faktor.runtime.installRoot" => applied("crates/foundation/src/config.rs"),

        // Honest stored-only descriptors: the value is durable and exported,
        // but this build has no runtime consumer for it.
        "faktor.agent.maxConsecutiveFailures" => stored(
            "the turn loop has a fixed loop detector; no per-session failure-count consumer exists in this build",
        ),
        "faktor.generation.temperature"
        | "faktor.generation.topP"
        | "faktor.generation.maxOutput" => stored(
            "model requests use the adapter/provider defaults; no runtime path reads these daemon settings yet",
        ),
        "faktor.retrieval.strategy" => stored(
            "retrieval strategy is fixed by the compiled index; no runtime selector reads this key",
        ),
        "faktor.permissions.childRoles" => stored(
            "child capability ceilings come from the task/preset envelope; no role-list consumer exists",
        ),
        "faktor.sandbox.network" => stored(
            "the daemon's destination allowlist is the [sandbox] config section; this UI key has no runtime consumer",
        ),
        "faktor.memory.captureMode" => stored(
            "memory capture is decided by the memory policy rows; this daemon key has no runtime consumer",
        ),
        "faktor.diagnostics.logLevel" => stored(
            "the tracing subscriber is initialized before the foundation store opens; change RUST_LOG or restart the daemon",
        ),
        "faktor.providers.credentialRef" => stored(
            "provider credentials are bound through provider connections and the vault; no runtime path reads this standalone key",
        ),

        _ => RuntimeConsumer::Unknown,
    }
}

const fn applied(consumer: &'static str) -> RuntimeConsumer {
    RuntimeConsumer::Applied { consumer }
}

const fn stored(reason: &'static str) -> RuntimeConsumer {
    RuntimeConsumer::StoredOnly { reason }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_descriptors;

    #[test]
    fn every_descriptor_is_classified_and_stored_only_is_locked() {
        for (key, descriptor) in default_descriptors() {
            let consumer = runtime_consumer(&key);
            assert_ne!(consumer, RuntimeConsumer::Unknown, "{key} is unclassified");
            let read_only_fact = key.starts_with("faktor.runtime.");
            assert_eq!(
                descriptor.writable,
                consumer.is_applied() && !read_only_fact,
                "{key} writability must match its runtime consumer"
            );
        }
    }

    #[test]
    fn stored_only_keys_name_a_reason() {
        for key in [
            "faktor.agent.maxConsecutiveFailures",
            "faktor.generation.temperature",
            "faktor.retrieval.strategy",
            "faktor.permissions.childRoles",
            "faktor.sandbox.network",
            "faktor.memory.captureMode",
            "faktor.diagnostics.logLevel",
            "faktor.providers.credentialRef",
        ] {
            match runtime_consumer(key) {
                RuntimeConsumer::StoredOnly { reason } => assert!(!reason.is_empty(), "{key}"),
                other => panic!("{key} must be stored-only, got {other:?}"),
            }
        }
    }
}
