//! `config::mcp`: schema domain of the daemon config.

use super::*;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpEntry {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: "default".into(),
            compaction_model: None,
            compact_at_usage: 0.65,
            instructions:
                "You are Faktor.\nAct as a careful senior engineer inside the user's repository."
                    .into(),
            providers: vec![],
            routing_mode: None,
            retry: RetryCfg::default(),
            mcp: vec![],
            verification: VerificationCfg::default(),
            sandbox: SandboxCfg::default(),
            tasks: TasksCfg::default(),
            completion: CompletionCfg::default(),
            efficiency: EfficiencyCfg::production_defaults(),
            embeddings: None,
            cloud: CloudCfg::default(),
            billing: BillingCfg::default(),
            updater: UpdaterCfg::default(),
            workers: WorkersCfg::default(),
            worker_plane: WorkerPlaneCfg::default(),
            enterprise: EnterpriseCfg::default(),
            worker_node: WorkerNodeCfg::default(),
            commerce: CommerceCfg::default(),
        }
    }
}

/// Production MCP bounds (hostile configs are rejected, never spawned).
pub const MAX_MCP_SERVERS: usize = 8;

pub const MAX_MCP_NAME_BYTES: usize = 128;

pub const MAX_MCP_COMMAND_BYTES: usize = 512;

pub const MAX_MCP_ARGS: usize = 32;

pub const MAX_MCP_ARG_BYTES: usize = 512;
