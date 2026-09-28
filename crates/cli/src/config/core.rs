//! Core daemon configuration types and loading (`config/core.rs`).
//!
//! Split out of the wave-24 monolithic `config.rs`; this module owns the
//! `Config` root type, its loader/validator and the shared glob prelude the
//! sibling config modules import through `use super::*`.

use super::*;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Config {
    pub model: String,
    pub compaction_model: Option<String>,
    pub compact_at_usage: f64,
    pub instructions: String,
    pub providers: Vec<ProviderCfg>,
    /// Economic routing mode of the daemon (P0-2): `None` = Economy — every
    /// model call routes through the RouterService built from the
    /// registered providers. `Pinned { provider, model }` validates every
    /// call against the pin. Strictly additive; the file shape accepts it
    /// since config_version 1 with `serde(default)`.
    pub routing_mode: Option<RoutingMode>,
    /// MCP servers (spec §31): each entry spawns one supervised stdio
    /// server whose dynamic tools are surfaced into the agent registry.
    pub mcp: Vec<McpEntry>,
    /// The additive `[verification]` section: per-category check budgets.
    pub verification: VerificationCfg,
    /// The additive `[sandbox]` section: the daemon's network destination
    /// allowlist and the OS-level network-isolation guarantee.
    pub sandbox: SandboxCfg,
    /// The additive `[tasks]` section: native task execution policy.
    pub tasks: TasksCfg,
    /// The additive `[completion]` section (P2 completion-step execution):
    /// the strict commit/push/PR execution policy.
    pub completion: CompletionCfg,
    /// The additive `[efficiency]` section (audit 86 + the efficiency-variant
    /// production flags): five boolean feature switches, ALL default `false`.
    pub efficiency: EfficiencyCfg,
    /// The additive `[embeddings]` section: the semantic embedding provider
    /// selection (model + provider + policy) wired into the search seam.
    /// Absent = no embedder: retrieval stays lexical/symbol-only and
    /// degrades honestly, never a fabricated vector.
    pub embeddings: Option<EmbeddingCfg>,
    /// The additive `[cloud]` section: the commercial control plane
    /// (identity/org/RBAC) and the durable SCM store. Disabled by default;
    /// while disabled the local daemon is byte-identical to the pre-cloud
    /// daemon and creates no database file.
    pub cloud: CloudCfg,
    /// The additive `[billing]` section: the commercial metering service
    /// (usage ledger + entitlements + credits). Disabled by default; while
    /// disabled the local daemon is byte-identical to the pre-billing
    /// daemon and creates no billing database file.
    pub billing: BillingCfg,
    /// The additive `[updater]` section: the signed updater/distribution
    /// lifecycle. Disabled by default; while disabled the local daemon is
    /// byte-identical to the pre-updater daemon and creates no `update.db`
    /// and no install directory.
    pub updater: UpdaterCfg,
    /// The additive `[workers]` section: the remote/VPC worker plane
    /// (registration tokens, immutable job generations, leases, heartbeats,
    /// generation-checked result landing) plus the TaskExecutor placement
    /// seam. Disabled by default; while disabled the local daemon creates
    /// no worker database file, every worker route answers a typed 409
    /// `workers_disabled`, and every task run executes locally exactly as
    /// before.
    pub workers: WorkersCfg,
    /// The additive `[worker_plane]` section: the deployment boundary of the
    /// remote/VPC worker HTTP surface (a SECOND listener with its own bind,
    /// transport and auth identity, separate from the loopback-oriented
    /// native listener). Disabled by default; while disabled no second
    /// socket exists and the worker routes keep their exact native-listener
    /// behavior.
    pub worker_plane: WorkerPlaneCfg,
    /// The additive `[enterprise]` section: retention classes + guarded GC,
    /// the append-only audit ledger, deletion jobs, admin settings and the
    /// layered configuration. Disabled by default; while disabled the local
    /// daemon creates no enterprise database file and every
    /// `/native/enterprise/*` route answers a typed 409.
    pub enterprise: EnterpriseCfg,
    /// The additive `[worker_node]` section: this host as a REMOTE worker of
    /// a control plane. Disabled by default; while disabled the
    /// `faktor worker run` entry refuses before any network or filesystem
    /// effect and the daemon is byte-identical.
    pub worker_node: WorkerNodeCfg,
    /// The additive `[commerce]` section (spec §13, Faktor Acquire).
    /// Disabled by default; while disabled no `source_market` tool is
    /// registered, no commerce database is created, no connector client,
    /// browser profile or broker exists and the rest of Faktor renders
    /// byte-identical requests. Credentials are referenced by environment
    /// variable NAME only — the section never carries a value.
    pub commerce: CommerceCfg,
}

pub(crate) fn default_config_version() -> u32 {
    1
}

impl Config {
    /// Validate the configured MCP surface: bounded entries with sane
    /// names/commands; empty names and oversized anything are malformed.
    pub fn mcp_servers(&self) -> Result<Vec<McpEntry>, String> {
        if self.mcp.len() > MAX_MCP_SERVERS {
            return Err(format!(
                "mcp: {} servers exceed the cap of {MAX_MCP_SERVERS}",
                self.mcp.len()
            ));
        }
        let mut names = std::collections::HashSet::new();
        for e in &self.mcp {
            if e.name.is_empty() || e.name.len() > MAX_MCP_NAME_BYTES {
                return Err(format!("mcp: name {:?} is empty or oversized", e.name));
            }
            if e.command.is_empty() || e.command.len() > MAX_MCP_COMMAND_BYTES {
                return Err(format!(
                    "mcp: command for {:?} is empty or oversized",
                    e.name
                ));
            }
            if e.args.len() > MAX_MCP_ARGS {
                return Err(format!("mcp: {:?} has too many args", e.name));
            }
            for a in &e.args {
                if a.len() > MAX_MCP_ARG_BYTES {
                    return Err(format!("mcp: {:?} has an oversized arg", e.name));
                }
            }
            if !names.insert(e.name.clone()) {
                return Err(format!("mcp: duplicate server name {:?}", e.name));
            }
        }
        Ok(self.mcp.clone())
    }
}

impl Config {
    /// The daemon sandbox policy this config resolves to (pure mapping,
    /// used by the daemon build and by strict config validation). The
    /// section overrides the sandbox crate's defaults: an explicit
    /// `network` row list replaces the network gate (parsed strictly — one
    /// unparseable rule fails the whole policy), and the configured
    /// guarantee rides into `SandboxPolicy::network_guarantee`. This policy
    /// is the AGENT shell-tool trust class: an unset `shell` keeps the
    /// crate's secure `os_isolated` default (which requires `required`), so
    /// nothing here can widen the agent shell tool. The interactive
    /// terminal class is resolved separately by the terminal authority and
    /// never feeds back into this policy.
    pub fn sandbox_policy(&self) -> Result<SandboxPolicy, String> {
        let mut policy = SandboxPolicy::default();
        if let Some(rows) = &self.sandbox.network {
            policy.network =
                NetworkGate::parse(rows).map_err(|e| format!("network rule error: {e}"))?;
        }
        policy.network_guarantee = self.sandbox.network_guarantee;
        policy.shell_execution = self
            .sandbox
            .shell
            .unwrap_or(faktor_sandbox::ShellExecutionMode::OsIsolated);
        policy.validate()?;
        Ok(policy)
    }
}

impl Config {
    /// Parse a config file. Lenient in the sense that it only parses (the
    /// strict `deny_unknown_fields`/`config_version` layer is inside
    /// deserialization); it does NOT run semantic validation. Default-only
    /// paths never call this.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let cfg: Config = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        Ok(cfg)
    }

    /// Strict load for EXPLICIT --config paths: any parse error, unknown
    /// field, unsupported `config_version`, or semantic validation failure
    /// (duplicate provider ids, hostile MCP bounds) fails startup — the
    /// daemon never boots on a config it cannot fully honor.
    pub fn load_strict(path: &Path) -> Result<Self, String> {
        let cfg = Self::load(path)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Semantic validation: duplicate provider ids are rejected (the error
    /// lists every duplicate), the MCP surface must satisfy its own
    /// hostile-config bounds, the sandbox section's destination rows must
    /// all parse (a rule that cannot parse is a config error, never
    /// silently permissive), and every provider's `pricing` override
    /// section must validate (typed errors: 0 prices, absurd magnitudes,
    /// override tables on local runtimes).
    pub fn validate(&self) -> Result<(), String> {
        self.mcp_servers()?;
        let sandbox = self
            .sandbox_policy()
            .map_err(|e| format!("sandbox config: {e}"))?;
        let mut seen = std::collections::HashSet::new();
        let mut dupes: Vec<String> = Vec::new();
        for p in &self.providers {
            let id = p.id().to_string();
            if !seen.insert(id.clone()) {
                dupes.push(id);
            }
        }
        dupes.sort();
        dupes.dedup();
        if !dupes.is_empty() {
            return Err(format!("duplicate provider id(s): {}", dupes.join(", ")));
        }
        for p in &self.providers {
            p.validate_quality()
                .map_err(|e| format!("provider {}: {e}", p.id()))?;
            p.validate_pricing()
                .map_err(|e| format!("provider {}: {e}", p.id()))?;
            p.validate_endpoint_address_class(sandbox.network.installed())
                .map_err(|e| format!("provider {}: {e}", p.id()))?;
        }
        if let Some(embeddings) = &self.embeddings {
            embeddings.validate()?;
        }
        self.cloud.validate()?;
        self.billing.validate()?;
        self.updater.validate()?;
        self.workers.validate()?;
        self.worker_plane.validate()?;
        self.enterprise.validate()?;
        self.worker_node.validate()?;
        self.commerce.validate()?;
        // The billing routes derive their tenant from the control-plane
        // principal, so an enabled billing section without the cloud section
        // could never authorize an organization-scoped read. The pair is
        // refused at load — the daemon never boots with an unauthenticated
        // billing surface.
        if self.billing.enabled && !self.cloud.enabled {
            return Err(
                "billing: an enabled [billing] section requires [cloud] enabled (the control plane supplies the organization principal)"
                    .into(),
            );
        }
        // The operator half of the worker surface (token mint, listing,
        // revocation) authorizes through a control-plane principal, so an
        // enabled worker plane without the cloud section could never mint a
        // registration token — refuse the pair at load instead of booting a
        // plane no operator can provision.
        if self.workers.enabled && !self.cloud.enabled {
            return Err(
                "workers: an enabled [workers] section requires [cloud] enabled (the control plane supplies the organization principal)"
                    .into(),
            );
        }
        // The worker-plane listener exposes the worker credential/protocol
        // routes; without the [workers] plane there is nothing to expose (and
        // every route would answer workers_disabled), so the pair is refused
        // at load.
        if self.worker_plane.enabled && !self.workers.enabled {
            return Err(
                "worker_plane: an enabled [worker_plane] section requires [workers] enabled (there is no worker plane to expose)"
                    .into(),
            );
        }
        // The enterprise routes derive their tenant from the control-plane
        // principal (and the audit ledger names principals), so an enabled
        // enterprise section without the cloud section could never
        // authorize an organization-scoped operation. Refuse the pair at
        // load instead of booting an unauthenticated admin surface.
        if self.enterprise.enabled && !self.cloud.enabled {
            return Err(
                "enterprise: an enabled [enterprise] section requires [cloud] enabled (the control plane supplies the organization principal)"
                    .into(),
            );
        }
        // The SSO routes resolve their organization's SSO reference from the
        // enterprise settings over the control plane; the GitHub App sync
        // writes organization-scoped SCM rows and its webhook route serves
        // the wired inbox. Both sections therefore require [cloud] enabled.
        if self.cloud.sso.as_ref().is_some_and(|sso| sso.enabled) && !self.cloud.enabled {
            return Err(
                "cloud sso: an enabled [cloud.sso] section requires [cloud] enabled".into(),
            );
        }
        if self
            .cloud
            .github_app
            .as_ref()
            .is_some_and(|app| app.enabled)
            && !self.cloud.enabled
        {
            return Err(
                "cloud github_app: an enabled [cloud.github_app] section requires [cloud] enabled"
                    .into(),
            );
        }
        // The report schedule writes durable report rows through the billing
        // store and reads the entitlement fold; it can only exist over an
        // enabled billing section.
        if self
            .billing
            .report
            .as_ref()
            .is_some_and(|report| report.enabled)
            && !self.billing.enabled
        {
            return Err(
                "billing report: an enabled [billing.report] section requires [billing] enabled"
                    .into(),
            );
        }
        Ok(())
    }

    /// Resolve the configured semantic embedder against the daemon's
    /// provider registry (the runtime half of `[embeddings]`). `Ok(None)`
    /// means "no semantic embedder": retrieval stays lexical/symbol-only —
    /// an honest degradation, never a fabricated vector. A `required`
    /// policy turns an unresolvable selection into an error (startup
    /// refusal); `best_effort` degrades to `None` with a warning.
    ///
    /// The embedder calls the provider SYNCHRONOUSLY through
    /// [`faktor_provider::Provider::embed`]; retries follow `retry` (the
    /// daemon's configured policy) and input batching is internal.
    pub fn semantic_embedder(
        &self,
        providers: &faktor_provider::ProviderRegistry,
        retry: &faktor_core::retry::RetryPolicy,
    ) -> Result<Option<Arc<dyn faktor_search::Embedder>>, String> {
        let Some(cfg) = &self.embeddings else {
            return Ok(None);
        };
        cfg.validate()?;
        let missing = |reason: String| -> Result<Option<Arc<dyn faktor_search::Embedder>>, String> {
            match cfg.policy {
                EmbeddingPolicy::Required => Err(format!(
                    "embeddings: provider {:?} model {:?} is required but {reason}",
                    cfg.provider, cfg.model
                )),
                EmbeddingPolicy::BestEffort => {
                    tracing::warn!(
                        "semantic embeddings disabled: provider {:?} model {:?} {reason}; retrieval stays lexical/symbol-only",
                        cfg.provider,
                        cfg.model
                    );
                    Ok(None)
                }
            }
        };
        let Some(provider) = providers.get(cfg.provider.as_str()) else {
            return missing("is not a registered provider".to_string());
        };
        if !provider.supports_embeddings(&cfg.model) {
            return missing("does not advertise embedding support for the model".to_string());
        }
        Ok(Some(Arc::new(crate::embeddings::ProviderEmbedder::new(
            provider,
            cfg.model.clone(),
            *retry,
        ))))
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, text).map_err(|e| e.to_string())
    }
}
