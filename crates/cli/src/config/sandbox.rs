//! `config::sandbox`: schema domain of the daemon config.

use super::*;

/// The additive `[sandbox]` section (daemon sandbox policy overrides).
/// `network` rows are parsed destination-allowlist rules in the security
/// crate's rule syntax (e.g. `http://127.0.0.1:8080`); `None` keeps the
/// sandbox crate's frozen default provider-endpoint allowlist, while an
/// explicit list — even an empty one (deny-all) — replaces it. The
/// `network_guarantee` (`required` default, `best_effort`, `none`) declares
/// what the policy requires of OS-level network isolation for shell
/// commands, and `shell` is the OPTIONAL explicit shell-execution contract
/// over TWO distinct trust classes:
///
/// - **Unset (`None`, the default)**: the agent-generated shell tool
///   (`run_command` and every supervised agent shell spawn) keeps the
///   sandbox crate's SECURE default — `os_isolated` + `required`: isolate or
///   refuse typed, never an unenforced shell. INTERACTIVE session terminals
///   (IDE user-initiated, not agent-generated) get the user-initiated
///   default instead: `network_capable_user_granted` with NO isolation
///   claim, so the terminal child inherits the daemon namespace and the
///   durable profile records the grant honestly
///   (`network_isolation=inherit`), never a fabricated OS-isolation claim.
///   The agent class never inherits this grant.
/// - `"os_isolated"`: the operator EXPLICITLY demands OS-level isolation
///   for BOTH classes (requires `network_guarantee = "required"`): agent
///   shells and interactive terminals are confined identically and refused
///   typed where no OS backend exists. The unset default is distinguishable
///   from this explicit choice, so selecting it is what turns terminal
///   refusal on.
/// - `"network_capable_user_granted"`: the explicit network-capable grant
///   for both classes (requires a non-`required` `network_guarantee`).
///
/// Choosing `network_guarantee` other than `required` REQUIRES setting
/// `shell = "network_capable_user_granted"`: full functionality is an
/// explicit grant, never a silent default, and the two modes are never
/// presented as equivalent strength. Unknown keys inside the section are
/// parse errors.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SandboxCfg {
    #[serde(default)]
    pub network: Option<Vec<String>>,
    #[serde(default)]
    pub network_guarantee: SandboxGuarantee,
    #[serde(default)]
    pub shell: Option<faktor_sandbox::ShellExecutionMode>,
}

/// The config FILE shape: `Config` plus `config_version` (default 1 when
/// the key is absent). Deserialization is STRICT: unknown fields anywhere
/// are rejected (a typo'd key fails startup instead of silently changing
/// behavior), and any `config_version` other than 1 is a parse error.
impl<'de> serde::Deserialize<'de> for Config {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct File {
            #[serde(default = "default_config_version")]
            config_version: u32,
            #[serde(default)]
            model: String,
            #[serde(default)]
            compaction_model: Option<String>,
            #[serde(default)]
            compact_at_usage: f64,
            #[serde(default)]
            instructions: String,
            #[serde(default)]
            providers: Vec<ProviderCfg>,
            /// `"economy"` (the default when absent), or
            /// `{"pinned": {"provider": "…", "model": "…"}}`. Anything else
            /// is a parse error (hostile configs are rejected, never
            /// half-honored).
            #[serde(default)]
            routing_mode: Option<RoutingMode>,
            #[serde(default)]
            mcp: Vec<McpEntry>,
            #[serde(default)]
            verification: VerificationCfg,
            #[serde(default)]
            sandbox: SandboxCfg,
            #[serde(default)]
            tasks: TasksCfg,
            #[serde(default)]
            completion: CompletionCfg,
            #[serde(default = "production_efficiency")]
            efficiency: EfficiencyCfg,
            #[serde(default)]
            embeddings: Option<EmbeddingCfg>,
            #[serde(default)]
            cloud: CloudCfg,
            #[serde(default)]
            billing: BillingCfg,
            #[serde(default)]
            updater: UpdaterCfg,
            #[serde(default)]
            workers: WorkersCfg,
            #[serde(default)]
            worker_plane: WorkerPlaneCfg,
            #[serde(default)]
            enterprise: EnterpriseCfg,
            #[serde(default)]
            worker_node: WorkerNodeCfg,
            #[serde(default)]
            commerce: CommerceCfg,
        }
        let file = File::deserialize(de)?;
        if file.config_version != 1 {
            return Err(D::Error::custom(format!(
                "unsupported config_version {}; this build accepts only config_version 1",
                file.config_version
            )));
        }
        Ok(Self {
            model: file.model,
            compaction_model: file.compaction_model,
            compact_at_usage: file.compact_at_usage,
            instructions: file.instructions,
            providers: file.providers,
            routing_mode: file.routing_mode,
            mcp: file.mcp,
            verification: file.verification,
            sandbox: file.sandbox,
            tasks: file.tasks,
            completion: file.completion,
            efficiency: file.efficiency,
            embeddings: file.embeddings,
            cloud: file.cloud,
            billing: file.billing,
            updater: file.updater,
            workers: file.workers,
            worker_plane: file.worker_plane,
            enterprise: file.enterprise,
            worker_node: file.worker_node,
            commerce: file.commerce,
        })
    }
}
