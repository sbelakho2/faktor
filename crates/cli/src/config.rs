//! Daemon configuration (faktor-plus.json). Provider keys are referenced by
//! environment variable name — the runtime never stores secrets.

use std::path::Path;
use std::sync::Arc;

use faktor_core::model::{
    BillingOrigin, MicroUsdPerMillionTokens, ModelCapabilities, PriceQuote, RoutingMode,
};
use faktor_orchestrator::runtime::task_executor::MutationMode;
use faktor_provider::catalog::{BillingOriginProvider, PricingOverrides};
use faktor_provider::egress::HttpTransport;
use faktor_provider::Provider;
use faktor_sandbox::{NetworkGate, SandboxGuarantee, SandboxPolicy};
use faktor_security::secret::SecretValue;

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

/// The additive `[completion]` section (P2 follow-up): how a contracted
/// run's ordered commit/push/PR steps execute.
///
/// Strict by construction: unknown keys are parse errors (derived
/// `deny_unknown_fields`), non-string values are type errors, and the
/// resolved [`faktor_orchestrator::runtime::completion_steps::CompletionStepsConfig`]
/// is validated (bounded remote/base branch; a bounded single-line PR
/// command/argv with known placeholders only and `{branch}` required; the
/// legacy `pr_command` string keeps every historic security check, while
/// the additive `pr_program`/`pr_args` typed argv preserves spaces per
/// element). An absent section keeps the inert defaults: push targets
/// `origin`, the PR base is `main`, and an unconfigured PR records the
/// documented `Skipped` outcome — a requested PR step is never silently
/// invented.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Default)]
pub struct CompletionCfg {
    /// The git remote the push step targets (default `origin`).
    pub remote: Option<String>,
    /// The base branch rendered into the PR template (default `main`).
    pub base_branch: Option<String>,
    /// DEPRECATED whitespace-split PR command template, e.g.
    /// `"gh pr create --head {branch} --base {base}"`. `None` (the default)
    /// means a requested PR step records `Skipped` (unless `pr_program` is
    /// configured). Mutually exclusive with `pr_program`.
    pub pr_command: Option<String>,
    /// The typed argv program (P3): executed directly through the supervisor
    /// with no shell, so paths/arguments with spaces are representable.
    /// Additive to the legacy `pr_command`.
    pub pr_program: Option<String>,
    /// The typed argv arguments; every element substitutes
    /// `{branch}`/`{base}`/`{remote}` independently. Requires `pr_program`.
    pub pr_args: Vec<String>,
}

/// Map-only strict parsing: a positional JSON array must never configure the
/// section by position, duplicates are refused and unknown keys are typed
/// errors — the same discipline the completion contract itself uses.
impl<'de> serde::Deserialize<'de> for CompletionCfg {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error as _, MapAccess, Visitor};

        struct SectionVisitor;

        impl<'de> Visitor<'de> for SectionVisitor {
            type Value = CompletionCfg;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("the [completion] section as a JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<CompletionCfg, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut out = CompletionCfg::default();
                let mut seen: u8 = 0;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "remote" => {
                            if seen & 1 != 0 {
                                return Err(A::Error::duplicate_field("remote"));
                            }
                            seen |= 1;
                            out.remote = map.next_value()?;
                        }
                        "base_branch" => {
                            if seen & 2 != 0 {
                                return Err(A::Error::duplicate_field("base_branch"));
                            }
                            seen |= 2;
                            out.base_branch = map.next_value()?;
                        }
                        "pr_command" => {
                            if seen & 4 != 0 {
                                return Err(A::Error::duplicate_field("pr_command"));
                            }
                            seen |= 4;
                            out.pr_command = map.next_value()?;
                        }
                        "pr_program" => {
                            if seen & 8 != 0 {
                                return Err(A::Error::duplicate_field("pr_program"));
                            }
                            seen |= 8;
                            out.pr_program = map.next_value()?;
                        }
                        "pr_args" => {
                            if seen & 16 != 0 {
                                return Err(A::Error::duplicate_field("pr_args"));
                            }
                            seen |= 16;
                            out.pr_args = map.next_value()?;
                        }
                        other => {
                            return Err(A::Error::unknown_field(
                                other,
                                &[
                                    "remote",
                                    "base_branch",
                                    "pr_command",
                                    "pr_program",
                                    "pr_args",
                                ],
                            ));
                        }
                    }
                }
                Ok(out)
            }
        }

        de.deserialize_map(SectionVisitor)
    }
}

impl CompletionCfg {
    /// Resolve and STRICTLY validate the completion-step execution policy.
    /// Any invalid remote/base/template is an error: a typo never half-runs.
    pub fn steps_config(
        &self,
    ) -> Result<faktor_orchestrator::runtime::completion_steps::CompletionStepsConfig, String> {
        let config = faktor_orchestrator::runtime::completion_steps::CompletionStepsConfig {
            remote: self.remote.clone().unwrap_or_else(|| "origin".to_string()),
            base_branch: self
                .base_branch
                .clone()
                .unwrap_or_else(|| "main".to_string()),
            pr_command: self.pr_command.clone(),
            pr_program: self.pr_program.clone(),
            pr_args: self.pr_args.clone(),
        };
        config.validate().map_err(|e| format!("completion: {e}"))?;
        Ok(config)
    }
}

/// The additive `[tasks]` section (P0 mutation isolation; wave-24 policy).
///
/// The section is strictly additive with `serde(default)` and an absent
/// section keeping the crate default `mutation_mode: Shadow` — every
/// MUTATING task works in a daemon-owned isolated candidate and is
/// integrated back into the user checkout with a conflict-aware CAS commit.
/// There is NO direct-owner mode any more: `mutation_mode:
/// "direct_compat"` is a strict parse error naming the removal, and so is
/// the legacy `shadow_mutation = false` value.
///
/// The pre-wave-24 boolean key `shadow_mutation` is still accepted as a
/// LEGACY alias for its historical `true` meaning only (shadow mutation on),
/// but it is an error to specify BOTH keys — the file never says two
/// different things. Unknown keys inside the section are parse errors
/// (strict on both load paths).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Default)]
pub struct TasksCfg {
    #[serde(default)]
    pub mutation_mode: MutationMode,
}

impl<'de> serde::Deserialize<'de> for TasksCfg {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct File {
            #[serde(default)]
            mutation_mode: Option<MutationMode>,
            /// Legacy pre-wave-24 alias (config_version 1 files written
            /// before the mode existed). `true` = shadow mutation on; the
            /// `false` value (direct) was REMOVED and is a strict error.
            #[serde(default)]
            shadow_mutation: Option<bool>,
        }
        let file = File::deserialize(de)?;
        match (file.mutation_mode, file.shadow_mutation) {
            (Some(_), Some(_)) => Err(D::Error::custom(
                "conflicting [tasks] keys: mutation_mode and the legacy shadow_mutation alias cannot both be present",
            )),
            (Some(m), None) => Ok(Self { mutation_mode: m }),
            (None, Some(true)) => Ok(Self {
                mutation_mode: MutationMode::Shadow,
            }),
            (None, Some(false)) => Err(D::Error::custom(
                "the legacy [tasks] shadow_mutation = false was removed: every mutating run executes in an isolated candidate (shadow mutation); there is no direct-owner mode",
            )),
            (None, None) => Ok(Self {
                mutation_mode: MutationMode::Shadow,
            }),
        }
    }
}

/// The additive `[efficiency]` section (audit 86 + the efficiency-variant
/// production flags): five independent boolean feature switches. In
/// PRODUCTION every flag defaults ON — the efficiency system is active —
/// and an explicit `false` is the documented OFF-SWITCH for that one
/// component (an absent section keeps the production defaults; a partial
/// section flips only the keys it names):
///
/// - `failure_learning`: feed the learning crate's failure prior into
///   context selection through `faktor_context`'s `FailurePrior` planner
///   seam (audit 68). OFF installs no prior (neutral omission risk);
/// - `ccr`: compressed-context representation for tool/evidence payloads.
///   OFF keeps raw tool output byte-identical (bounded excerpts only);
/// - `typed_handoff`: re-sent history rendered from durable task rows.
///   OFF falls back to the legacy transcript rendering;
/// - `semantic_context`: information-gain selection of evidence through the
///   ContextCompiler. OFF keeps the producer evidence list byte-identical
///   (neutral sparse fallback; unit contexts stay parity);
/// - `rework_routing`: rework-aware routing over durable verified-outcome
///   stats.
///
/// The section is strictly additive: unknown keys, non-boolean values,
/// duplicate keys and non-object shapes (a JSON array must never enable
/// flags by position) are parse errors on both load paths.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EfficiencyCfg {
    /// Failure-learning prior in context selection (audit 68).
    pub failure_learning: bool,
    /// Compressed Context Representation for tool/evidence payloads.
    pub ccr: bool,
    /// Typed handoff: re-sent history rendered from durable task rows.
    pub typed_handoff: bool,
    /// Semantic context: information-gain selection of evidence through the
    /// ContextCompiler (the audit's `information_gain` flag).
    pub semantic_context: bool,
    /// Rework-aware routing over durable verified-outcome stats.
    pub rework_routing: bool,
}

impl Default for EfficiencyCfg {
    /// The additive type-level default: every flag OFF. Unit/embedded
    /// callers that build `EfficiencyCfg::default()` keep the pre-efficiency
    /// behavior byte-for-byte; the PRODUCTION config paths use
    /// [`EfficiencyCfg::production_defaults`] (see [`Config::default`] and
    /// the serde default), which is what turns the efficiency system on for
    /// a daemon with an absent `[efficiency]` section.
    fn default() -> Self {
        Self {
            failure_learning: false,
            ccr: false,
            typed_handoff: false,
            semantic_context: false,
            rework_routing: false,
        }
    }
}

impl EfficiencyCfg {
    /// The PRODUCTION defaults (audit: the efficiency system is on by
    /// default): every component enabled, with the per-key `false` explicit
    /// off-switch documented on the struct.
    pub const fn production_defaults() -> Self {
        Self {
            failure_learning: true,
            ccr: true,
            typed_handoff: true,
            semantic_context: true,
            rework_routing: true,
        }
    }
}

/// Serde default for an absent `[efficiency]` section: production defaults
/// (all ON). A partial section flips only the keys it names.
fn production_efficiency() -> EfficiencyCfg {
    EfficiencyCfg::production_defaults()
}

/// The `[efficiency]` keys, in stable order (unknown-field errors list them).
const EFFICIENCY_FIELDS: &[&str] = &[
    "failure_learning",
    "ccr",
    "typed_handoff",
    "semantic_context",
    "information_gain",
    "rework_routing",
];

/// Map-only strict parsing for `[efficiency]`: unlike a derived struct with
/// all-default fields, a JSON sequence is REFUSED (serde would otherwise
/// accept `[true]` as positional field values), duplicates are refused, and
/// unknown keys are refused. Absent keys keep the `false` default.
impl<'de> serde::Deserialize<'de> for EfficiencyCfg {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error as _, MapAccess, Visitor};

        struct SectionVisitor;

        impl<'de> Visitor<'de> for SectionVisitor {
            type Value = EfficiencyCfg;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("the [efficiency] section as a JSON object of booleans")
            }

            fn visit_map<A>(self, mut map: A) -> Result<EfficiencyCfg, A::Error>
            where
                A: MapAccess<'de>,
            {
                // A PRESENT partial section starts from the production
                // defaults and flips only the keys it names: an operator who
                // disables one component never silently disables the rest.
                let mut out = EfficiencyCfg::production_defaults();
                let mut seen: u8 = 0;
                while let Some(key) = map.next_key::<String>()? {
                    let (bit, name) = match key.as_str() {
                        "failure_learning" => (1u8, "failure_learning"),
                        "ccr" => (2, "ccr"),
                        "typed_handoff" => (4, "typed_handoff"),
                        // The audit calls this switch `information_gain`;
                        // `semantic_context` remains the canonical key. Both
                        // names flip the SAME bit, so naming both is a
                        // duplicate (refused), never a silent last-wins.
                        "semantic_context" | "information_gain" => (8, "semantic_context"),
                        "rework_routing" => (16, "rework_routing"),
                        other => return Err(A::Error::unknown_field(other, EFFICIENCY_FIELDS)),
                    };
                    if seen & bit != 0 {
                        return Err(A::Error::duplicate_field(name));
                    }
                    seen |= bit;
                    let value = map.next_value::<bool>()?;
                    match bit {
                        1 => out.failure_learning = value,
                        2 => out.ccr = value,
                        4 => out.typed_handoff = value,
                        8 => out.semantic_context = value,
                        _ => out.rework_routing = value,
                    }
                }
                Ok(out)
            }
        }

        de.deserialize_map(SectionVisitor)
    }
}

/// The additive `[cloud]` section: the commercial control plane (identity,
/// organizations, RBAC, synced SCM repositories).
///
/// Strict and additive:
///
/// - `enabled` (default `false`): when false — the default and the ONLY
///   value for every pre-existing config — the daemon builds NO control
///   plane and NO SCM store, creates no database file, and every
///   `/native/identity`, `/native/orgs`, `/native/repositories` and
///   `/native/approvals` request answers a typed 409 `cloud_disabled`.
///   The local daemon is byte-identical to the pre-cloud daemon;
/// - `database` / `scm_database`: optional simple FILE NAMES (relative to
///   the daemon data dir; defaults `control-plane.db` and `scm.db`).
///   Absolute paths, separators and `..` traversal are refused: the
///   control-plane state lives inside the daemon's own data directory;
/// - `payload_dir`: the operator-staged payload directory (absolute or
///   relative to the data dir; default `payloads`). SSO client secrets and
///   the GitHub App private key/webhook secret are loaded from it under the
///   strict contract of [`crate::payload`]: plain file names only, regular
///   files only, 0600-style modes on unix, bounded and strictly parsed —
///   a referenced-but-missing/corrupt/too-permissive payload refuses
///   startup (fail closed, never a half-wired cloud surface);
/// - `[cloud.sso]`: the network OIDC adapter wiring (issuer, client id and
///   the optional operator-staged client secret plus its strict
///   `client_secret_post`/`client_secret_basic` method);
/// - `[cloud.github_app]`: the GitHub App wiring (app id, staged private
///   key and webhook secret, api base, tenant organization). When enabled
///   the daemon builds the real adapter/token source/webhook inbox/sync,
///   runs one bounded initial sync after readiness and re-syncs on webhook
///   delivery; the optional `[cloud.github_app.reconcile]` sub-section adds
///   the bounded periodic re-sync timer (disabled = webhook parity);
///   `/native/scm/webhook` dispatches into the wired inbox;
/// - unknown keys, duplicates, non-object shapes and wrong value types are
///   parse errors.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Default)]
pub struct CloudCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub scm_database: Option<String>,
    #[serde(default)]
    pub payload_dir: Option<String>,
    #[serde(default)]
    pub sso: Option<CloudSsoCfg>,
    #[serde(default)]
    pub github_app: Option<CloudGithubAppCfg>,
}

/// The `[cloud.sso]` section: the daemon-wide network OIDC adapter wiring.
/// Disabled by default; while disabled (or absent) `/native/sso/*` answers
/// a typed 409 `sso_disabled` and no adapter is built.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CloudSsoCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub issuer: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    /// The payload NAME (inside `[cloud] payload_dir`) of the confidential
    /// client's secret. Optional: absent = the public PKCE client path.
    #[serde(default)]
    pub client_secret: Option<String>,
    /// How the confidential client authenticates at the token endpoint:
    /// `client_secret_post` (the default when a secret is staged) or
    /// `client_secret_basic`. Only meaningful together with `client_secret`;
    /// a method without a secret, or an unknown method, is refused at load.
    #[serde(default)]
    pub client_secret_method: Option<String>,
    #[serde(default)]
    pub discovery_max_age_ms: Option<i64>,
    #[serde(default)]
    pub jwks_max_age_ms: Option<i64>,
    #[serde(default)]
    pub max_jwks_refetches: Option<u32>,
    /// The ID-token signing algorithms this deployment explicitly allows (the
    /// header `alg` must be in the intersection of this list, the discovery
    /// document's advertised set and the selected JWK's own constraints).
    /// Absent = the adapter default `["RS256"]`; `none` and unsupported names
    /// are refused at load, and symmetric `HS*` is honored only when listed
    /// here AND the JWK is an `oct` key with `use = "sig"`.
    #[serde(default)]
    pub allowed_algorithms: Option<Vec<String>>,
}

/// The `[cloud.github_app]` section: the real GitHub App wiring. Disabled by
/// default; while disabled no adapter/sync/webhook sink is built and the
/// daemon keeps its pre-existing SCM surface byte-identical.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CloudGithubAppCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub app_id: Option<u64>,
    /// The payload NAME of the app's PKCS#8 private key PEM.
    #[serde(default)]
    pub private_key: Option<String>,
    /// The payload NAME of the app's webhook HMAC secret.
    #[serde(default)]
    pub webhook_secret: Option<String>,
    /// The REST API base (default `https://api.github.com`; a loopback mock
    /// in tests).
    #[serde(default)]
    pub api_base: Option<String>,
    /// The tenant organization the synced rows belong to (required).
    #[serde(default)]
    pub organization: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub page_size: Option<usize>,
    #[serde(default)]
    pub max_pages: Option<usize>,
    /// The optional periodic reconcile timer. Absent (or explicitly
    /// disabled, normalized away byte-for-byte) = webhook-only parity: no
    /// timer is built and no extra sync ever runs.
    #[serde(default, deserialize_with = "deserialize_reconcile_section")]
    pub reconcile: Option<CloudGithubAppReconcileCfg>,
}

/// The `[cloud.github_app.reconcile]` section: the bounded periodic
/// installation/repository re-sync (post-readiness, in addition to the
/// webhook-driven syncs). Disabled by default; a disabled section resolves
/// byte-identically to the absent one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CloudGithubAppReconcileCfg {
    #[serde(default)]
    pub enabled: bool,
    /// Base cadence between passes (`1s..=24h`; default 5min).
    #[serde(default)]
    pub interval_ms: Option<i64>,
    /// Cadence jitter bound (`0..=60s`, also capped by the interval; default
    /// 30s). The next pass is `interval + deterministic jitter`.
    #[serde(default)]
    pub jitter_ms: Option<i64>,
    /// Failure-backoff ceiling (`interval..=24h`; default 1h). A failed pass
    /// retries after a bounded exponential backoff.
    #[serde(default)]
    pub max_backoff_ms: Option<i64>,
}

/// Disabled parity: `{enabled: false}` (or a missing section) resolves to
/// `None`, exactly like the absent key.
fn deserialize_reconcile_section<'de, D>(
    de: D,
) -> Result<Option<CloudGithubAppReconcileCfg>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = <Option<CloudGithubAppReconcileCfg> as serde::Deserialize>::deserialize(de)?;
    Ok(raw.filter(|section| section.enabled))
}

/// The `[cloud]` keys, in stable order (unknown-field errors list them).
pub const CLOUD_FIELDS: &[&str] = &[
    "enabled",
    "database",
    "scm_database",
    "payload_dir",
    "sso",
    "github_app",
];

/// The default payload directory name under the daemon data dir.
pub const DEFAULT_PAYLOAD_DIR: &str = "payloads";
/// Bound on one configured payload root.
pub const MAX_PAYLOAD_ROOT_BYTES: usize = 1024;

/// The default control-plane database file name.
pub const DEFAULT_CLOUD_DATABASE: &str = "control-plane.db";
/// The default SCM database file name.
pub const DEFAULT_SCM_DATABASE: &str = "scm.db";
/// Bound on one configured database file name.
pub const MAX_CLOUD_DATABASE_BYTES: usize = 128;

impl<'de> serde::Deserialize<'de> for CloudCfg {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error as _, MapAccess, Visitor};

        struct SectionVisitor;

        impl<'de> Visitor<'de> for SectionVisitor {
            type Value = CloudCfg;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("the [cloud] section as a JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<CloudCfg, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut out = CloudCfg::default();
                let mut seen: u8 = 0;
                while let Some(key) = map.next_key::<String>()? {
                    let (bit, name) = match key.as_str() {
                        "enabled" => (1u8, "enabled"),
                        "database" => (2, "database"),
                        "scm_database" => (4, "scm_database"),
                        "payload_dir" => (8, "payload_dir"),
                        "sso" => (16, "sso"),
                        "github_app" => (32, "github_app"),
                        other => return Err(A::Error::unknown_field(other, CLOUD_FIELDS)),
                    };
                    if seen & bit != 0 {
                        return Err(A::Error::duplicate_field(name));
                    }
                    seen |= bit;
                    match bit {
                        1 => out.enabled = map.next_value::<bool>()?,
                        2 => out.database = map.next_value::<Option<String>>()?,
                        4 => out.scm_database = map.next_value::<Option<String>>()?,
                        8 => out.payload_dir = map.next_value::<Option<String>>()?,
                        16 => out.sso = map.next_value::<Option<CloudSsoCfg>>()?,
                        _ => out.github_app = map.next_value::<Option<CloudGithubAppCfg>>()?,
                    }
                }
                // A disabled sub-section is normalized away: the resolved
                // config of `{enabled: false}` is the resolved config of the
                // absent section (disabled parity, byte-for-byte).
                if out.sso.as_ref().is_some_and(|sso| !sso.enabled) {
                    out.sso = None;
                }
                if out.github_app.as_ref().is_some_and(|app| !app.enabled) {
                    out.github_app = None;
                }
                Ok(out)
            }
        }

        de.deserialize_map(SectionVisitor)
    }
}

impl CloudCfg {
    /// Validate one configured database file name: a bounded simple file
    /// name (no directory separators, no `..`, not absolute, no control
    /// characters). The control-plane state never escapes the data dir.
    pub fn validate_database_name(kind: &str, name: &str) -> Result<(), String> {
        if name.is_empty() || name.len() > MAX_CLOUD_DATABASE_BYTES {
            return Err(format!(
                "cloud: {kind} must be 1..={MAX_CLOUD_DATABASE_BYTES} bytes"
            ));
        }
        if name.contains('/') || name.contains('\\') || name.contains("..") || name.contains(':') {
            return Err(format!(
                "cloud: {kind} {name:?} must be a plain file name (no paths, no traversal)"
            ));
        }
        if name.bytes().any(|b| b.is_ascii_control()) {
            return Err(format!(
                "cloud: {kind} {name:?} contains control characters"
            ));
        }
        if name == "." || name == ".." {
            return Err(format!("cloud: {kind} {name:?} is not a file name"));
        }
        Ok(())
    }

    /// Validate one configured payload root: bounded ASCII, no control
    /// characters, no `..` traversal. Absolute paths are allowed; relative
    /// paths live under the daemon data dir.
    pub fn validate_payload_root(raw: &str) -> Result<(), String> {
        if raw.is_empty() || raw.len() > MAX_PAYLOAD_ROOT_BYTES || !raw.is_ascii() {
            return Err(format!(
                "cloud: payload_dir must be 1..={MAX_PAYLOAD_ROOT_BYTES} ASCII bytes"
            ));
        }
        if raw.bytes().any(|b| b.is_ascii_control()) {
            return Err("cloud: payload_dir contains control characters".into());
        }
        if raw.split(['/', '\\']).any(|part| part == "..") {
            return Err(format!(
                "cloud: payload_dir {raw:?} must not contain `..` traversal"
            ));
        }
        Ok(())
    }

    /// Validate the section (called by [`Config::validate`] on both load
    /// paths). A disabled section validates nothing beyond its shape.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(database) = &self.database {
            Self::validate_database_name("database", database)?;
        }
        if let Some(scm_database) = &self.scm_database {
            Self::validate_database_name("scm_database", scm_database)?;
        }
        if let Some(payload_dir) = &self.payload_dir {
            Self::validate_payload_root(payload_dir)?;
        }
        if let Some(sso) = &self.sso {
            sso.validate()?;
        }
        if let Some(github_app) = &self.github_app {
            github_app.validate()?;
        }
        Ok(())
    }

    /// The resolved operator-staged payload directory under `data_dir`
    /// (default `<data_dir>/payloads`). Validated: a hostile root is refused
    /// at resolution, never silently escaped.
    pub fn payload_root(&self, data_dir: &Path) -> Result<std::path::PathBuf, String> {
        match &self.payload_dir {
            None => Ok(data_dir.join(DEFAULT_PAYLOAD_DIR)),
            Some(raw) => {
                Self::validate_payload_root(raw)?;
                let raw_path = Path::new(raw);
                if raw_path.is_absolute() {
                    Ok(raw_path.to_path_buf())
                } else {
                    Ok(data_dir.join(raw_path))
                }
            }
        }
    }

    /// The resolved control-plane database path under `data_dir` (`None`
    /// when the section is disabled — the daemon then never creates it).
    pub fn control_plane_path(
        &self,
        data_dir: &Path,
    ) -> Result<Option<std::path::PathBuf>, String> {
        if !self.enabled {
            return Ok(None);
        }
        self.validate()?;
        let name = self
            .database
            .clone()
            .unwrap_or_else(|| DEFAULT_CLOUD_DATABASE.to_string());
        Ok(Some(data_dir.join(name)))
    }

    /// The resolved SCM database path under `data_dir` (`None` when the
    /// section is disabled).
    pub fn scm_path(&self, data_dir: &Path) -> Result<Option<std::path::PathBuf>, String> {
        if !self.enabled {
            return Ok(None);
        }
        self.validate()?;
        let name = self
            .scm_database
            .clone()
            .unwrap_or_else(|| DEFAULT_SCM_DATABASE.to_string());
        Ok(Some(data_dir.join(name)))
    }
}

impl CloudSsoCfg {
    /// Validate the section. A disabled section validates nothing beyond its
    /// shape; an enabled one requires an http(s) issuer without a trailing
    /// slash and a bounded client id, a bounded payload name for the
    /// optional client secret and the documented cache bounds.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(client_secret) = &self.client_secret {
            crate::payload::PayloadDir::validate_name(client_secret)
                .map_err(|e| format!("cloud sso: client_secret {e}"))?;
        }
        if let Some(method) = &self.client_secret_method {
            if self.client_secret.is_none() {
                return Err(
                    "cloud sso: client_secret_method requires a `client_secret` payload name"
                        .into(),
                );
            }
            let _ = self.client_auth_method()?;
            let _ = method;
        }
        if !self.enabled {
            return Ok(());
        }
        let _ = self.issuer()?;
        let _ = self.client_id()?;
        for (field, value) in [
            ("discovery_max_age_ms", self.discovery_max_age_ms),
            ("jwks_max_age_ms", self.jwks_max_age_ms),
        ] {
            if let Some(value) = value {
                if value <= 0 || value > faktor_cloud::MAX_CACHE_MAX_AGE_MS {
                    return Err(format!(
                        "cloud sso: {field} must be 1..={}",
                        faktor_cloud::MAX_CACHE_MAX_AGE_MS
                    ));
                }
            }
        }
        if let Some(refetches) = self.max_jwks_refetches {
            if refetches > faktor_cloud::MAX_JWKS_REFETCHES {
                return Err(format!(
                    "cloud sso: max_jwks_refetches must be <= {}",
                    faktor_cloud::MAX_JWKS_REFETCHES
                ));
            }
        }
        if let Some(algorithms) = &self.allowed_algorithms {
            if algorithms.is_empty() || algorithms.len() > faktor_cloud::MAX_ALLOWED_ALGORITHMS {
                return Err(format!(
                    "cloud sso: allowed_algorithms must carry 1..={} entries",
                    faktor_cloud::MAX_ALLOWED_ALGORITHMS
                ));
            }
            for algorithm in algorithms {
                if algorithm == "none" {
                    return Err(
                        "cloud sso: \"none\" is never an accepted id-token signing algorithm"
                            .into(),
                    );
                }
                if !faktor_cloud::SUPPORTED_ALGORITHMS.contains(&algorithm.as_str()) {
                    return Err(format!(
                        "cloud sso: allowed_algorithms entry {algorithm:?} is not supported \
                         (supported: {})",
                        faktor_cloud::SUPPORTED_ALGORITHMS.join(", ")
                    ));
                }
            }
        }
        Ok(())
    }

    /// The resolved allowed-algorithm policy: the configured list, or the
    /// adapter default (`["RS256"]`) when absent. Validated on the way out.
    pub fn allowed_algorithms(&self) -> Result<Vec<String>, String> {
        match &self.allowed_algorithms {
            Some(algorithms) => {
                faktor_cloud::NetworkOidcConfig::validate_allowed_algorithms(algorithms)
                    .map_err(|e| format!("cloud sso: {e}"))?;
                Ok(algorithms.clone())
            }
            None => Ok(faktor_cloud::DEFAULT_ALLOWED_ALGORITHMS
                .iter()
                .map(|alg| (*alg).to_string())
                .collect()),
        }
    }

    /// The configured issuer (required when enabled).
    pub fn issuer(&self) -> Result<String, String> {
        let raw = self
            .issuer
            .clone()
            .ok_or_else(|| "cloud sso: an enabled section requires `issuer`".to_string())?;
        if !(raw.starts_with("https://") || raw.starts_with("http://")) || raw.ends_with('/') {
            return Err("cloud sso: issuer must be an http(s) URL without a trailing slash".into());
        }
        if raw.len() > 2048 {
            return Err("cloud sso: issuer must be at most 2048 bytes".into());
        }
        Ok(raw)
    }

    /// The configured client id (required when enabled).
    pub fn client_id(&self) -> Result<String, String> {
        let raw = self
            .client_id
            .clone()
            .ok_or_else(|| "cloud sso: an enabled section requires `client_id`".to_string())?;
        if raw.trim().is_empty() || raw.len() > 256 {
            return Err("cloud sso: client_id must be 1..=256 bytes".into());
        }
        Ok(raw)
    }

    /// The strict confidential-client auth method: `client_secret_post`
    /// (the default when a secret is staged) or `client_secret_basic`.
    /// Callers reach this only when `client_secret` is configured; the
    /// method must select a confidential exchange (never `none`).
    pub fn client_auth_method(&self) -> Result<faktor_cloud::ClientAuthMethod, String> {
        let raw = self
            .client_secret_method
            .as_deref()
            .unwrap_or("client_secret_post");
        let method = faktor_cloud::ClientAuthMethod::parse(raw).ok_or_else(|| {
            format!(
                "cloud sso: client_secret_method {raw:?} must be client_secret_post \
                 or client_secret_basic"
            )
        })?;
        if !method.requires_secret() {
            return Err(
                "cloud sso: client_secret_method must select a confidential method \
                 (client_secret_post or client_secret_basic)"
                    .into(),
            );
        }
        Ok(method)
    }
}

impl CloudGithubAppCfg {
    /// Validate the section. A disabled section validates nothing beyond its
    /// shape; an enabled one requires an app id, the two staged payload
    /// NAMES and the tenant organization, and its api base must validate.
    pub fn validate(&self) -> Result<(), String> {
        for (field, name) in [
            ("private_key", self.private_key.as_deref()),
            ("webhook_secret", self.webhook_secret.as_deref()),
        ] {
            if let Some(name) = name {
                crate::payload::PayloadDir::validate_name(name)
                    .map_err(|e| format!("cloud github_app: {field} {e}"))?;
            }
        }
        if let Some(reconcile) = &self.reconcile {
            if !self.enabled {
                return Err(
                    "cloud github_app: reconcile requires an enabled github_app section".into(),
                );
            }
            reconcile.validate()?;
        }
        if !self.enabled {
            return Ok(());
        }
        if self.app_id.unwrap_or(0) == 0 {
            return Err("cloud github_app: an enabled section requires a non-zero `app_id`".into());
        }
        if self.private_key.is_none() {
            return Err(
                "cloud github_app: an enabled section requires `private_key` (a payload name)"
                    .into(),
            );
        }
        if self.webhook_secret.is_none() {
            return Err(
                "cloud github_app: an enabled section requires `webhook_secret` (a payload name)"
                    .into(),
            );
        }
        let _ = self.organization()?;
        let config = self.app_config()?;
        config
            .validate()
            .map_err(|e| format!("cloud github_app: {e}"))?;
        Ok(())
    }

    /// The tenant organization the synced rows are linked to.
    pub fn organization(&self) -> Result<String, String> {
        let raw = self.organization.clone().ok_or_else(|| {
            "cloud github_app: an enabled section requires `organization`".to_string()
        })?;
        faktor_cloud::OrganizationId::try_new(raw.clone())
            .map_err(|e| format!("cloud github_app: {e}"))?;
        Ok(raw)
    }

    /// The REST adapter configuration.
    pub fn app_config(&self) -> Result<faktor_scm::GitHubAppConfig, String> {
        let mut config = faktor_scm::GitHubAppConfig {
            api_base: self
                .api_base
                .clone()
                .unwrap_or_else(|| "https://api.github.com".to_string()),
            ..Default::default()
        };
        if let Some(user_agent) = &self.user_agent {
            config.user_agent = user_agent.clone();
        }
        if let Some(page_size) = self.page_size {
            config.page_size = page_size;
        }
        if let Some(max_pages) = self.max_pages {
            config.max_pages = max_pages;
        }
        config
            .validate()
            .map_err(|e| format!("cloud github_app: {e}"))?;
        Ok(config)
    }

    /// The optional periodic-reconcile policy (`None` = no timer; webhook
    /// parity) with every bound already validated.
    pub fn reconcile_policy(&self) -> Result<Option<faktor_scm::ReconcilePolicy>, String> {
        self.reconcile
            .as_ref()
            .map(CloudGithubAppReconcileCfg::policy)
            .transpose()
    }
}

impl CloudGithubAppReconcileCfg {
    /// The strict policy handed to the reconcile runner: configured values
    /// or documented defaults, validated against the crate's hard bounds.
    pub fn policy(&self) -> Result<faktor_scm::ReconcilePolicy, String> {
        let policy = faktor_scm::ReconcilePolicy {
            interval_ms: self
                .interval_ms
                .unwrap_or(faktor_scm::DEFAULT_RECONCILE_INTERVAL_MS),
            jitter_ms: self
                .jitter_ms
                .unwrap_or(faktor_scm::DEFAULT_RECONCILE_JITTER_MS),
            max_backoff_ms: self
                .max_backoff_ms
                .unwrap_or(faktor_scm::DEFAULT_RECONCILE_MAX_BACKOFF_MS),
        };
        policy
            .validate()
            .map_err(|e| format!("cloud github_app reconcile: {e}"))?;
        Ok(policy)
    }

    /// Validate the section (bounds only; disabled sections are normalized
    /// away before this is reached).
    pub fn validate(&self) -> Result<(), String> {
        let _ = self.policy()?;
        Ok(())
    }
}

/// The additive `[updater]` section: the signed updater/distribution
/// lifecycle (stable/beta/dev channels, ed25519 operator keys, staged
/// content-addressed installs).
///
/// Strict and additive:
///
/// - `enabled` (default `false`): when false — the default and the ONLY
///   value for every pre-existing config — the daemon builds NO updater,
///   creates no `update.db` and no install directory, and every
///   `/native/updater/*` route answers a typed 409 `updater_disabled`.
///   The local daemon is byte-identical to the pre-updater daemon;
/// - `channel` (default `stable`): one of `stable|beta|dev`; only manifests
///   the configured channel accepts can advance;
/// - `install_root` (default `install`): the install directory that holds
///   the content-addressed artifacts and the atomic `current` pointer.
///   Either an absolute path or a relative path INSIDE the data dir (no
///   `..`, no control characters, bounded);
/// - `keys` (required when enabled): the operator ed25519 allowlist, each
///   `{id, public_key}` with `public_key` = base64 of the raw 32-byte key.
///   An empty allowlist refuses every manifest (there is no implicit trust
///   anchor), so an enabled section without keys is a config error;
/// - `max_artifact_bytes` (default 256 MiB, cap 4 GiB) and `clock_skew_ms`
///   (default 5 minutes, cap 1 hour) are bounded;
/// - `allow_legacy_manifests_once` (default `false`) is the documented
///   one-time escape hatch for a signed manifest that predates the
///   `release_generation` anti-rollback counter: it is admissible only while
///   the durable per-channel high-water mark is still 0, and admitting it
///   consumes the allowance durably. Leave it off once releases carry the
///   generation.
///
/// Unknown keys, duplicates, non-object shapes and wrong value types are
/// parse errors.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Default)]
pub struct UpdaterCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub install_root: Option<String>,
    #[serde(default)]
    pub max_artifact_bytes: Option<u64>,
    #[serde(default)]
    pub clock_skew_ms: Option<i64>,
    #[serde(default)]
    pub allow_legacy_manifests_once: bool,
    #[serde(default)]
    pub keys: Vec<UpdaterKeyCfg>,
}

/// One allowlisted operator key: an identity and its raw base64 ed25519
/// public key (32 bytes).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdaterKeyCfg {
    pub id: String,
    pub public_key: String,
}

/// The `[updater]` keys, in stable order (unknown-field errors list them).
pub const UPDATER_FIELDS: &[&str] = &[
    "enabled",
    "channel",
    "install_root",
    "max_artifact_bytes",
    "clock_skew_ms",
    "allow_legacy_manifests_once",
    "keys",
];

/// The default install directory name under the daemon data dir.
pub const DEFAULT_UPDATER_INSTALL_ROOT: &str = "install";
/// The default updater database file name.
pub const DEFAULT_UPDATER_DATABASE: &str = "update.db";
/// Bound on the configured install root.
pub const MAX_UPDATER_INSTALL_ROOT_BYTES: usize = 1024;
/// Bound on the operator key allowlist.
pub const MAX_UPDATER_KEYS: usize = 8;
/// The largest `max_artifact_bytes` an operator may configure (4 GiB).
pub const MAX_UPDATER_ARTIFACT_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// The largest accepted clock skew (1 hour).
pub const MAX_UPDATER_CLOCK_SKEW_MS: i64 = 60 * 60 * 1000;

impl<'de> serde::Deserialize<'de> for UpdaterCfg {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error as _, MapAccess, Visitor};

        struct SectionVisitor;

        impl<'de> Visitor<'de> for SectionVisitor {
            type Value = UpdaterCfg;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("the [updater] section as a JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<UpdaterCfg, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut out = UpdaterCfg::default();
                let mut seen: u8 = 0;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "enabled" => {
                            if seen & 1 != 0 {
                                return Err(A::Error::duplicate_field("enabled"));
                            }
                            seen |= 1;
                            out.enabled = map.next_value::<bool>()?;
                        }
                        "channel" => {
                            if seen & 2 != 0 {
                                return Err(A::Error::duplicate_field("channel"));
                            }
                            seen |= 2;
                            out.channel = map.next_value::<Option<String>>()?;
                        }
                        "install_root" => {
                            if seen & 4 != 0 {
                                return Err(A::Error::duplicate_field("install_root"));
                            }
                            seen |= 4;
                            out.install_root = map.next_value::<Option<String>>()?;
                        }
                        "max_artifact_bytes" => {
                            if seen & 8 != 0 {
                                return Err(A::Error::duplicate_field("max_artifact_bytes"));
                            }
                            seen |= 8;
                            out.max_artifact_bytes = map.next_value::<Option<u64>>()?;
                        }
                        "clock_skew_ms" => {
                            if seen & 16 != 0 {
                                return Err(A::Error::duplicate_field("clock_skew_ms"));
                            }
                            seen |= 16;
                            out.clock_skew_ms = map.next_value::<Option<i64>>()?;
                        }
                        "keys" => {
                            if seen & 32 != 0 {
                                return Err(A::Error::duplicate_field("keys"));
                            }
                            seen |= 32;
                            out.keys = map.next_value::<Vec<UpdaterKeyCfg>>()?;
                        }
                        "allow_legacy_manifests_once" => {
                            if seen & 64 != 0 {
                                return Err(A::Error::duplicate_field(
                                    "allow_legacy_manifests_once",
                                ));
                            }
                            seen |= 64;
                            out.allow_legacy_manifests_once = map.next_value::<bool>()?;
                        }
                        other => return Err(A::Error::unknown_field(other, UPDATER_FIELDS)),
                    }
                }
                Ok(out)
            }
        }

        de.deserialize_map(SectionVisitor)
    }
}

impl UpdaterCfg {
    /// The configured channel (default `stable`).
    pub fn channel(&self) -> Result<faktor_updater::Channel, String> {
        let raw = self.channel.as_deref().unwrap_or("stable");
        faktor_updater::Channel::parse(raw)
            .ok_or_else(|| format!("updater: channel {raw:?} must be stable|beta|dev"))
    }

    /// Validate one install-root value: bounded ASCII, no control
    /// characters, no `..` traversal. Absolute paths are allowed; relative
    /// paths live under the daemon data dir.
    pub fn validate_install_root(raw: &str) -> Result<(), String> {
        if raw.is_empty() || raw.len() > MAX_UPDATER_INSTALL_ROOT_BYTES || !raw.is_ascii() {
            return Err(format!(
                "updater: install_root must be 1..={MAX_UPDATER_INSTALL_ROOT_BYTES} ASCII bytes"
            ));
        }
        if raw.bytes().any(|b| b.is_ascii_control()) {
            return Err("updater: install_root contains control characters".into());
        }
        if raw.split(['/', '\\']).any(|part| part == "..") {
            return Err(format!(
                "updater: install_root {raw:?} must not contain `..` traversal"
            ));
        }
        Ok(())
    }

    /// Validate the section (called by [`Config::validate`] on both load
    /// paths). A disabled section validates nothing beyond its shape.
    pub fn validate(&self) -> Result<(), String> {
        self.channel()?;
        if let Some(install_root) = &self.install_root {
            Self::validate_install_root(install_root)?;
        }
        if let Some(max) = self.max_artifact_bytes {
            if max == 0 || max > MAX_UPDATER_ARTIFACT_BYTES {
                return Err(format!(
                    "updater: max_artifact_bytes must be 1..={MAX_UPDATER_ARTIFACT_BYTES}"
                ));
            }
        }
        if let Some(skew) = self.clock_skew_ms {
            if !(0..=MAX_UPDATER_CLOCK_SKEW_MS).contains(&skew) {
                return Err(format!(
                    "updater: clock_skew_ms must be 0..={MAX_UPDATER_CLOCK_SKEW_MS}"
                ));
            }
        }
        if self.keys.len() > MAX_UPDATER_KEYS {
            return Err(format!(
                "updater: at most {MAX_UPDATER_KEYS} operator keys may be configured"
            ));
        }
        let mut seen: Vec<&str> = Vec::with_capacity(self.keys.len());
        for key in &self.keys {
            if seen.contains(&key.id.as_str()) {
                return Err(format!("updater: key id {:?} is configured twice", key.id));
            }
            seen.push(&key.id);
        }
        if self.enabled && self.keys.is_empty() {
            return Err(
                "updater: an enabled [updater] section requires at least one operator key \
                 (an empty allowlist refuses every manifest)"
                    .into(),
            );
        }
        // Key material is validated whenever it is present, so a bad key is
        // a startup error BEFORE an operator flips `enabled`.
        self.trusted_keys()?;
        Ok(())
    }

    /// The operator key allowlist as the updater expects it.
    pub fn trusted_keys(&self) -> Result<faktor_updater::TrustedKeys, String> {
        let mut keys = Vec::with_capacity(self.keys.len());
        for key in &self.keys {
            keys.push(
                faktor_updater::TrustedKey::from_base64(&key.id, &key.public_key)
                    .map_err(|e| format!("updater: {e}"))?,
            );
        }
        faktor_updater::TrustedKeys::new(keys).map_err(|e| format!("updater: {e}"))
    }

    /// The resolved artifact bound.
    pub fn max_artifact_bytes_resolved(&self) -> u64 {
        self.max_artifact_bytes
            .unwrap_or(faktor_updater::DEFAULT_MAX_ARTIFACT_BYTES)
    }

    /// The resolved clock skew.
    pub fn clock_skew_ms_resolved(&self) -> i64 {
        self.clock_skew_ms
            .unwrap_or(faktor_updater::DEFAULT_CLOCK_SKEW_MS)
    }

    /// The documented one-time legacy-manifest allowance (default false).
    pub fn allow_legacy_manifests_once_resolved(&self) -> bool {
        self.allow_legacy_manifests_once
    }

    /// The install root under `data_dir` (`None` when the section is
    /// disabled — the daemon then never creates it).
    pub fn install_root_path(&self, data_dir: &Path) -> Result<Option<std::path::PathBuf>, String> {
        if !self.enabled {
            return Ok(None);
        }
        self.validate()?;
        let raw = self
            .install_root
            .clone()
            .unwrap_or_else(|| DEFAULT_UPDATER_INSTALL_ROOT.to_string());
        let path = std::path::PathBuf::from(&raw);
        Ok(Some(if path.is_absolute() {
            path
        } else {
            data_dir.join(path)
        }))
    }

    /// The resolved updater database path under `data_dir` (`None` when the
    /// section is disabled).
    pub fn database_path(&self, data_dir: &Path) -> Result<Option<std::path::PathBuf>, String> {
        if !self.enabled {
            return Ok(None);
        }
        self.validate()?;
        Ok(Some(data_dir.join(DEFAULT_UPDATER_DATABASE)))
    }
}

/// The additive `[billing]` section: the Wave 3 commercial metering service
/// (usage ledger + entitlements + credits).
///
/// Strict and additive:
///
/// - `enabled` (default `false`): when false — the default and the only
///   value for every pre-existing config — the daemon builds NO billing
///   service, creates no billing database file, and every
///   `/native/entitlements`, `/native/credits/grant` and
///   `/native/usage?org=...` request answers a typed 409 `billing_disabled`.
///   The local daemon is byte-identical to the pre-billing daemon;
/// - `database`: an optional simple FILE NAME (default `billing.db`) for
///   the durable usage/credit ledger. It reuses the control-plane store's
///   migration ladder (its own `user_version` v2 tables);
/// - `organization`: the organization the local daemon's sessions meter
///   into (required when enabled: a billing service without a tenant would
///   admit nothing and account for nothing);
/// - `account` / `account_name` / `managed`: the local billing account
///   provisioned idempotently at startup. `managed = true` (default)
///   permits Faktor-managed provider spend (credits are debited); `false`
///   restricts the account to BYOK usage (recorded, never debited);
/// - `default_plan` + `plans` + `managed_providers`: the plan table is the
///   ONLY source of features/limits and the managed-provider set the ONLY
///   source of the managed/BYOK decision. Unknown plan/feature/limit names
///   are refused loudly; an enabled section with no plans (or a default
///   plan outside the table) is a startup error — the daemon never boots
///   with a silently empty entitlement surface;
/// - unknown keys, duplicates, non-object shapes and wrong value types are
///   parse errors.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Default)]
pub struct BillingCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub organization: Option<String>,
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub account_name: Option<String>,
    /// Whether the provisioned account permits Faktor-managed provider
    /// spend (default `true`; `false` = a BYOK-only account).
    #[serde(default)]
    pub managed: Option<bool>,
    #[serde(default)]
    pub default_plan: Option<String>,
    /// The plan table, keyed by plan id (each `PlanConfig.plan_id` must
    /// equal its key). Strict shapes come from `faktor_cloud::PlanConfig`.
    #[serde(default)]
    pub plans: std::collections::BTreeMap<String, faktor_cloud::PlanConfig>,
    /// Provider ids whose spend is Faktor-managed; every other provider is
    /// BYOK. Empty = all providers are BYOK.
    #[serde(default)]
    pub managed_providers: Vec<String>,
    /// The additive `[billing.report]` schedule: periodically reports the
    /// durable usage fold through the vendor adapter. Absent/disabled = no
    /// schedule rows are written and no vendor call is ever made.
    #[serde(default)]
    pub report: Option<BillingReportCfg>,
}

/// The `[billing.report]` section: the durable report schedule over the
/// report-only vendor adapter.
///
/// Strict and additive:
///
/// - `enabled` (default `false`): no schedule row is written, no vendor
///   call is made and the daemon is otherwise byte-identical;
/// - `base_url` (required when enabled): the vendor's absolute http(s) base
///   (a loopback mock in tests);
/// - `report_path` (default `/v1/usage-reports`), `user_agent`, `auth_env`:
///   the vendor request shape. `auth_env` defaults to
///   `FAKTOR_BILLING_VENDOR_TOKEN`; an EMPTY string explicitly selects the
///   unauthenticated local-mock shape (documented, never silent);
/// - `interval_ms` (default 60s, 1s..=24h): how often the maintenance loop
///   checks whether the current period is due;
/// - `period_ms` (default 24h, 1min..=366d): the reporting period bucket;
/// - `max_catch_up` (default 24, 0..=256): how many periods crossed during a
///   downtime gap are backfilled (oldest first) before the regular cadence
///   resumes; older periods beyond the cap are marked skipped-permanently
///   with a durable audit row, never silently dropped;
/// - `max_attempts` (default 5, cap 5) and `retry_base_ms` /
///   `max_backoff_ms`: the retry policy of a failed period (exponential
///   backoff with deterministic jitter).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct BillingReportCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub report_path: Option<String>,
    #[serde(default)]
    pub auth_env: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub interval_ms: Option<i64>,
    #[serde(default)]
    pub period_ms: Option<i64>,
    #[serde(default)]
    pub max_attempts: Option<u32>,
    #[serde(default)]
    pub retry_base_ms: Option<i64>,
    #[serde(default)]
    pub max_backoff_ms: Option<i64>,
    #[serde(default)]
    pub max_catch_up: Option<usize>,
}

/// The `[billing]` keys, in stable order (unknown-field errors list them).
pub const BILLING_FIELDS: &[&str] = &[
    "enabled",
    "database",
    "organization",
    "account",
    "account_name",
    "managed",
    "default_plan",
    "plans",
    "managed_providers",
    "report",
];

/// The default billing database file name (relative to the daemon data
/// dir).
pub const DEFAULT_BILLING_DATABASE: &str = "billing.db";

impl<'de> serde::Deserialize<'de> for BillingCfg {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error as _, MapAccess, Visitor};

        struct SectionVisitor;

        impl<'de> Visitor<'de> for SectionVisitor {
            type Value = BillingCfg;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("the [billing] section as a JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<BillingCfg, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut out = BillingCfg::default();
                let mut seen: u16 = 0;
                while let Some(key) = map.next_key::<String>()? {
                    let (bit, name) = match key.as_str() {
                        "enabled" => (1u16, "enabled"),
                        "database" => (2, "database"),
                        "organization" => (4, "organization"),
                        "account" => (8, "account"),
                        "account_name" => (16, "account_name"),
                        "managed" => (32, "managed"),
                        "default_plan" => (64, "default_plan"),
                        "plans" => (128, "plans"),
                        "managed_providers" => (256, "managed_providers"),
                        "report" => (512, "report"),
                        other => return Err(A::Error::unknown_field(other, BILLING_FIELDS)),
                    };
                    if seen & bit != 0 {
                        return Err(A::Error::duplicate_field(name));
                    }
                    seen |= bit;
                    match bit {
                        1 => out.enabled = map.next_value::<bool>()?,
                        2 => out.database = map.next_value::<Option<String>>()?,
                        4 => out.organization = map.next_value::<Option<String>>()?,
                        8 => out.account = map.next_value::<Option<String>>()?,
                        16 => out.account_name = map.next_value::<Option<String>>()?,
                        32 => out.managed = map.next_value::<Option<bool>>()?,
                        64 => out.default_plan = map.next_value::<Option<String>>()?,
                        128 => {
                            out.plans = map.next_value::<std::collections::BTreeMap<
                                String,
                                faktor_cloud::PlanConfig,
                            >>()?
                        }
                        256 => out.managed_providers = map.next_value::<Vec<String>>()?,
                        _ => out.report = map.next_value::<Option<BillingReportCfg>>()?,
                    }
                }
                // Disabled parity: an explicitly disabled report section
                // resolves exactly like the absent one.
                if out.report.as_ref().is_some_and(|report| !report.enabled) {
                    out.report = None;
                }
                Ok(out)
            }
        }

        de.deserialize_map(SectionVisitor)
    }
}

impl BillingCfg {
    /// The strict billing configuration handed to the service (`None` while
    /// disabled). Validated eagerly: a plan table the config cannot honor is
    /// refused at load, never at first admission.
    pub fn service_config(&self) -> Result<Option<faktor_cloud::BillingConfig>, String> {
        if !self.enabled {
            return Ok(None);
        }
        self.validate()?;
        Ok(Some(self.build_service_config()?))
    }

    /// Build (and validate) the cloud-side configuration without recursing
    /// back into [`Self::validate`].
    fn build_service_config(&self) -> Result<faktor_cloud::BillingConfig, String> {
        let config = faktor_cloud::BillingConfig {
            default_plan: self.default_plan.clone(),
            plans: self.plans.clone(),
            managed_providers: self.managed_providers.iter().cloned().collect(),
        };
        config.validate().map_err(|e| format!("billing: {e}"))?;
        Ok(config)
    }

    /// The resolved billing database path under `data_dir` (`None` when the
    /// section is disabled — the daemon then never creates it).
    pub fn billing_path(&self, data_dir: &Path) -> Result<Option<std::path::PathBuf>, String> {
        if !self.enabled {
            return Ok(None);
        }
        self.validate()?;
        let name = self
            .database
            .clone()
            .unwrap_or_else(|| DEFAULT_BILLING_DATABASE.to_string());
        Ok(Some(data_dir.join(name)))
    }

    /// The organization the daemon meters into (required when enabled).
    pub fn organization(&self) -> Result<faktor_cloud::OrganizationId, String> {
        let raw = self.organization.clone().ok_or_else(|| {
            "billing: an enabled [billing] section requires `organization`".to_string()
        })?;
        faktor_cloud::OrganizationId::try_new(raw).map_err(|e| format!("billing: {e}"))
    }

    /// The provisioned billing account id (default `local`).
    pub fn account_id(&self) -> Result<faktor_cloud::BillingAccountId, String> {
        let raw = self.account.clone().unwrap_or_else(|| "local".to_string());
        faktor_cloud::BillingAccountId::try_new(raw).map_err(|e| format!("billing: {e}"))
    }

    /// Validate the section (called by [`Config::validate`] on both load
    /// paths). A disabled section validates nothing beyond its shape.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(database) = &self.database {
            CloudCfg::validate_database_name("billing database", database)?;
        }
        if let Some(report) = &self.report {
            report.validate()?;
        }
        if self.enabled {
            let organization = self.organization()?;
            let _ = organization;
            let account = self.account_id()?;
            let _ = account;
            if let Some(name) = &self.account_name {
                if name.is_empty() || name.len() > 128 {
                    return Err("billing: account_name must be 1..=128 bytes".into());
                }
            }
            if self.plans.is_empty() {
                return Err(
                    "billing: an enabled section requires a non-empty `plans` table (the ONLY source of features/limits)"
                        .into(),
                );
            }
            self.build_service_config()?;
        }
        Ok(())
    }
}

impl BillingReportCfg {
    /// Validate the section. A disabled section validates nothing beyond its
    /// shape; an enabled one requires a vendor base URL and bounded cadence
    /// bounds.
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        let _ = self.vendor_config()?;
        let _ = self.policy()?;
        Ok(())
    }

    /// The strict vendor-adapter configuration.
    pub fn vendor_config(&self) -> Result<faktor_cloud::BillingVendorConfig, String> {
        let mut config = faktor_cloud::BillingVendorConfig {
            base_url: self
                .base_url
                .clone()
                .ok_or_else(|| {
                    "billing report: an enabled section requires `base_url`".to_string()
                })?
                .trim_end_matches('/')
                .to_string(),
            ..Default::default()
        };
        if let Some(report_path) = &self.report_path {
            config.report_path = report_path.clone();
        }
        // An explicitly EMPTY auth_env is the documented unauthenticated
        // local-mock opt-out: the vendor config keeps its default (valid)
        // env name and the runner passes no credential.
        if let Some(auth_env) = self.auth_env.as_deref().filter(|env| !env.is_empty()) {
            config.auth_env = auth_env.to_string();
        }
        if let Some(user_agent) = &self.user_agent {
            config.user_agent = user_agent.clone();
        }
        if let Some(max_attempts) = self.max_attempts {
            config.max_attempts = max_attempts;
        }
        if let Some(retry_base_ms) = self.retry_base_ms {
            config.retry_base_ms = retry_base_ms;
        }
        config
            .validate()
            .map_err(|e| format!("billing report: {e}"))?;
        Ok(config)
    }

    /// Whether the report calls carry a bearer credential resolved from
    /// `auth_env` (`false` only for the explicitly empty auth-env opt-out).
    pub fn unauthenticated(&self) -> bool {
        matches!(self.auth_env.as_deref(), Some(""))
    }

    pub fn interval_ms_resolved(&self) -> i64 {
        self.interval_ms.unwrap_or(60_000)
    }

    pub fn period_ms_resolved(&self) -> i64 {
        self.period_ms.unwrap_or(86_400_000)
    }

    pub fn max_attempts_resolved(&self) -> u32 {
        self.max_attempts
            .unwrap_or(faktor_cloud::MAX_REPORT_ATTEMPTS)
    }

    pub fn retry_base_ms_resolved(&self) -> i64 {
        self.retry_base_ms.unwrap_or(250)
    }

    pub fn max_backoff_ms_resolved(&self) -> i64 {
        self.max_backoff_ms.unwrap_or(3_600_000)
    }

    /// The schedule policy handed to the maintenance runner.
    pub fn policy(&self) -> Result<crate::billing_report::ReportPolicy, String> {
        let interval = self.interval_ms_resolved();
        if !(crate::billing_report::MIN_REPORT_INTERVAL_MS
            ..=crate::billing_report::MAX_REPORT_INTERVAL_MS)
            .contains(&interval)
        {
            return Err(format!(
                "billing report: interval_ms must be {}..={}",
                crate::billing_report::MIN_REPORT_INTERVAL_MS,
                crate::billing_report::MAX_REPORT_INTERVAL_MS
            ));
        }
        let period = self.period_ms_resolved();
        if !(crate::billing_report::MIN_REPORT_PERIOD_MS
            ..=crate::billing_report::MAX_REPORT_PERIOD_MS)
            .contains(&period)
        {
            return Err(format!(
                "billing report: period_ms must be {}..={}",
                crate::billing_report::MIN_REPORT_PERIOD_MS,
                crate::billing_report::MAX_REPORT_PERIOD_MS
            ));
        }
        let max_backoff = self.max_backoff_ms_resolved();
        if !(0..=crate::billing_report::MAX_REPORT_MAX_BACKOFF_MS).contains(&max_backoff) {
            return Err(format!(
                "billing report: max_backoff_ms must be 0..={}",
                crate::billing_report::MAX_REPORT_MAX_BACKOFF_MS
            ));
        }
        let max_catch_up = self
            .max_catch_up
            .unwrap_or(faktor_cloud::DEFAULT_REPORT_CATCH_UP);
        if max_catch_up > faktor_cloud::MAX_REPORT_CATCH_UP_PERIODS {
            return Err(format!(
                "billing report: max_catch_up must be <= {}",
                faktor_cloud::MAX_REPORT_CATCH_UP_PERIODS
            ));
        }
        Ok(crate::billing_report::ReportPolicy {
            interval_ms: interval,
            period_ms: period,
            max_attempts: self.max_attempts_resolved(),
            retry_base_ms: self.retry_base_ms_resolved(),
            max_backoff_ms: max_backoff,
            max_catch_up,
        })
    }
}

/// The additive `[workers]` section: the remote/VPC worker plane.
///
/// Strict and additive:
///
/// - `enabled` (default `false`): when false — the default and the only
///   value for every pre-existing config — the daemon builds NO worker
///   plane, creates no worker database file, every `/native/workers*` and
///   `/native/jobs/*` route answers a typed 409 `workers_disabled`, and the
///   TaskExecutor's placement seam stays DISABLED (local execution is
///   byte-identical to the pre-worker-plane daemon);
/// - `database`: an optional simple FILE NAME (default `workers.db`) for
///   the durable worker plane. The worker crate owns its OWN migration
///   ladder (`user_version` v1); the control-plane/billing `user_version`
///   is never touched;
/// - `organization`: the organization the plane leases jobs for (required
///   when enabled);
/// - `trust_domain`: the trust domain of the plane's jobs (default: the
///   organization id). Workers register into it and can only ever lease
///   jobs of their own organization AND trust domain;
/// - `os` / `arch` / `toolchains` / `network` / `region` / `min_cpu_cores`
///   / `min_memory_mb` / `gpu`: the placement REQUIREMENTS the daemon
///   demands of a worker before a task run is placed remotely. Empty
///   defaults mean "any registered worker of the trust domain".
///
/// When enabled AND at least one eligible worker is registered, a new task
/// run is placed remotely: the plane mints one immutable job generation,
/// CAS-accepts its lease and the local executor starts NOTHING (the receipt
/// names the remote job). Without an eligible worker the run executes
/// locally exactly as before.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Default)]
pub struct WorkersCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub organization: Option<String>,
    #[serde(default)]
    pub trust_domain: Option<String>,
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub arch: Option<String>,
    #[serde(default)]
    pub toolchains: Vec<String>,
    #[serde(default)]
    pub network: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub min_cpu_cores: u32,
    #[serde(default)]
    pub min_memory_mb: u64,
    #[serde(default)]
    pub gpu: bool,
}

/// The `[workers]` keys, in stable order (unknown-field errors list them).
pub const WORKERS_FIELDS: &[&str] = &[
    "enabled",
    "database",
    "organization",
    "trust_domain",
    "os",
    "arch",
    "toolchains",
    "network",
    "region",
    "min_cpu_cores",
    "min_memory_mb",
    "gpu",
];

/// The default worker-plane database file name (relative to the daemon data
/// dir).
pub const DEFAULT_WORKERS_DATABASE: &str = "workers.db";

impl<'de> serde::Deserialize<'de> for WorkersCfg {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error as _, MapAccess, Visitor};

        struct SectionVisitor;

        impl<'de> Visitor<'de> for SectionVisitor {
            type Value = WorkersCfg;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("the [workers] section as a JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<WorkersCfg, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut out = WorkersCfg::default();
                let mut seen: u16 = 0;
                while let Some(key) = map.next_key::<String>()? {
                    let (bit, name) = match key.as_str() {
                        "enabled" => (1u16, "enabled"),
                        "database" => (2, "database"),
                        "organization" => (4, "organization"),
                        "trust_domain" => (8, "trust_domain"),
                        "os" => (16, "os"),
                        "arch" => (32, "arch"),
                        "toolchains" => (64, "toolchains"),
                        "network" => (128, "network"),
                        "region" => (256, "region"),
                        "min_cpu_cores" => (512, "min_cpu_cores"),
                        "min_memory_mb" => (1024, "min_memory_mb"),
                        "gpu" => (2048, "gpu"),
                        other => return Err(A::Error::unknown_field(other, WORKERS_FIELDS)),
                    };
                    if seen & bit != 0 {
                        return Err(A::Error::duplicate_field(name));
                    }
                    seen |= bit;
                    match bit {
                        1 => out.enabled = map.next_value::<bool>()?,
                        2 => out.database = map.next_value::<Option<String>>()?,
                        4 => out.organization = map.next_value::<Option<String>>()?,
                        8 => out.trust_domain = map.next_value::<Option<String>>()?,
                        16 => out.os = map.next_value::<Option<String>>()?,
                        32 => out.arch = map.next_value::<Option<String>>()?,
                        64 => out.toolchains = map.next_value::<Vec<String>>()?,
                        128 => out.network = map.next_value::<Option<String>>()?,
                        256 => out.region = map.next_value::<Option<String>>()?,
                        512 => out.min_cpu_cores = map.next_value::<u32>()?,
                        1024 => out.min_memory_mb = map.next_value::<u64>()?,
                        _ => out.gpu = map.next_value::<bool>()?,
                    }
                }
                Ok(out)
            }
        }

        de.deserialize_map(SectionVisitor)
    }
}

impl WorkersCfg {
    /// The resolved worker-plane database path under `data_dir` (`None`
    /// when the section is disabled — the daemon then never creates it).
    pub fn workers_path(&self, data_dir: &Path) -> Result<Option<std::path::PathBuf>, String> {
        if !self.enabled {
            return Ok(None);
        }
        self.validate()?;
        let name = self
            .database
            .clone()
            .unwrap_or_else(|| DEFAULT_WORKERS_DATABASE.to_string());
        Ok(Some(data_dir.join(name)))
    }

    /// The organization the plane leases jobs for (required when enabled).
    pub fn organization(&self) -> Result<faktor_cloud::OrganizationId, String> {
        let raw = self.organization.clone().ok_or_else(|| {
            "workers: an enabled [workers] section requires `organization`".to_string()
        })?;
        faktor_cloud::OrganizationId::try_new(raw).map_err(|e| format!("workers: {e}"))
    }

    /// The plane's trust domain (default: the organization id).
    pub fn trust_domain(&self) -> Result<String, String> {
        let organization = self.organization()?;
        match &self.trust_domain {
            None => Ok(organization.as_str().to_string()),
            Some(raw) => {
                let trimmed = raw.trim();
                if trimmed.is_empty() || trimmed.len() > 128 {
                    return Err("workers: trust_domain must be 1..=128 bytes".into());
                }
                if !trimmed.bytes().all(|b| b.is_ascii_graphic()) {
                    return Err(
                        "workers: trust_domain must be printable ASCII without whitespace".into(),
                    );
                }
                Ok(trimmed.to_ascii_lowercase())
            }
        }
    }

    /// The placement requirement defaults demanded of an eligible worker.
    pub fn requirements(&self) -> Result<faktor_worker::JobRequirements, String> {
        let trust_domain = self.trust_domain()?;
        let network = match self.network.as_deref() {
            None => None,
            Some("none") => Some(faktor_worker::NetworkProfile::None),
            Some("egress_restricted") => Some(faktor_worker::NetworkProfile::EgressRestricted),
            Some("full") => Some(faktor_worker::NetworkProfile::Full),
            Some(other) => {
                return Err(format!(
                    "workers: network {other:?} must be one of none|egress_restricted|full"
                ));
            }
        };
        let mut requirements = faktor_worker::JobRequirements {
            os: self.os.clone(),
            arch: self.arch.clone(),
            toolchains: self.toolchains.clone(),
            sandbox: Vec::new(),
            network,
            min_cpu_cores: self.min_cpu_cores,
            min_memory_mb: self.min_memory_mb,
            gpu: self.gpu,
            region: self.region.clone(),
            trust_domain,
        };
        requirements
            .normalize()
            .map_err(|e| format!("workers: {e}"))?;
        Ok(requirements)
    }

    /// Validate the section (called by [`Config::validate`] on both load
    /// paths). A disabled section validates nothing beyond its shape.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(database) = &self.database {
            CloudCfg::validate_database_name("workers database", database)?;
        }
        if !self.enabled {
            return Ok(());
        }
        let _ = self.organization()?;
        let _ = self.trust_domain()?;
        let _ = self.requirements()?;
        Ok(())
    }
}

/// The additive `[worker_plane]` section: the DEPLOYMENT BOUNDARY of the
/// remote/VPC worker HTTP surface — a SECOND listener with its OWN identity,
/// separate from the loopback-oriented native listener.
///
/// Strict and additive:
///
/// - `enabled` (default `false`): while false the daemon binds no second
///   socket, the native listener stays exactly as before and the worker
///   routes keep their existing behavior (disabled parity);
/// - `bind` (default `127.0.0.1:8790`): the dedicated worker-plane socket.
///   The native listener's bind is NOT configurable (always loopback), so
///   the two listeners can never share exposure. A non-loopback bind is
///   refused at startup with a typed refusal NAMING the deployment boundary
///   unless `trusted_gateway = true` acknowledges an external
///   TLS-terminating gateway fronting the socket;
/// - `tls` (default `false`): request IN-PROCESS TLS termination. This
///   workspace compiles no inbound TLS stack (rustls exists only as a
///   transitive outbound HTTP-client dependency), so `tls = true` is a
///   typed startup refusal — never fabricated. Terminate TLS at the trusted
///   gateway and use the gateway-only mode;
/// - `trusted_gateway` (default `false`): the explicit acknowledgement
///   naming the boundary: an external, trusted gateway terminates TLS (and,
///   under `auth = "gateway_mtls"`, client mTLS) in front of this socket.
///   It is required for any non-loopback bind and recorded in the daemon's
///   startup audit line and by `faktor doctor --config`;
/// - `auth` (default `"worker_tokens"`): the plane's OWN transport
///   client-auth mode. `"gateway_mtls"` requires `trusted_gateway = true`
///   and a `bearer` (mTLS termination exists only at the gateway);
/// - `bearer`: an optional transport credential required as
///   `Authorization: Bearer <bearer>` on EVERY worker-plane request, in
///   addition to the worker registration token. The daemon password is
///   never accepted on the worker socket.
///
/// Enabling the section requires `[workers] enabled = true` (there is no
/// plane to expose otherwise); the pair is refused at config load. `Debug`
/// redacts the transport bearer.
#[derive(Clone, PartialEq, Eq, serde::Serialize, Default)]
pub struct WorkerPlaneCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub bind: Option<String>,
    #[serde(default)]
    pub tls: bool,
    #[serde(default)]
    pub trusted_gateway: bool,
    #[serde(default)]
    pub auth: Option<String>,
    #[serde(default)]
    pub bearer: Option<String>,
}

impl std::fmt::Debug for WorkerPlaneCfg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerPlaneCfg")
            .field("enabled", &self.enabled)
            .field("bind", &self.bind)
            .field("tls", &self.tls)
            .field("trusted_gateway", &self.trusted_gateway)
            .field("auth", &self.auth)
            .field("bearer", &self.bearer.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// The `[worker_plane]` keys, in stable order (unknown-field errors list
/// them).
pub const WORKER_PLANE_FIELDS: &[&str] = &[
    "enabled",
    "bind",
    "tls",
    "trusted_gateway",
    "auth",
    "bearer",
];

/// The default worker-plane bind: loopback (only an explicit operator bind
/// moves it, and only under the gateway rules).
pub const DEFAULT_WORKER_PLANE_BIND: &str = faktor_server::DEFAULT_WORKER_PLANE_BIND;

impl<'de> serde::Deserialize<'de> for WorkerPlaneCfg {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error as _, MapAccess, Visitor};

        struct SectionVisitor;

        impl<'de> Visitor<'de> for SectionVisitor {
            type Value = WorkerPlaneCfg;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("the [worker_plane] section as a JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<WorkerPlaneCfg, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut out = WorkerPlaneCfg::default();
                let mut seen: u8 = 0;
                while let Some(key) = map.next_key::<String>()? {
                    let (bit, name) = match key.as_str() {
                        "enabled" => (1u8, "enabled"),
                        "bind" => (2, "bind"),
                        "tls" => (4, "tls"),
                        "trusted_gateway" => (8, "trusted_gateway"),
                        "auth" => (16, "auth"),
                        "bearer" => (32, "bearer"),
                        other => return Err(A::Error::unknown_field(other, WORKER_PLANE_FIELDS)),
                    };
                    if seen & bit != 0 {
                        return Err(A::Error::duplicate_field(name));
                    }
                    seen |= bit;
                    match bit {
                        1 => out.enabled = map.next_value::<bool>()?,
                        2 => out.bind = map.next_value::<Option<String>>()?,
                        4 => out.tls = map.next_value::<bool>()?,
                        8 => out.trusted_gateway = map.next_value::<bool>()?,
                        16 => out.auth = map.next_value::<Option<String>>()?,
                        _ => out.bearer = map.next_value::<Option<String>>()?,
                    }
                }
                Ok(out)
            }
        }

        de.deserialize_map(SectionVisitor)
    }
}

impl WorkerPlaneCfg {
    /// The parsed bind (shape-checked on both load paths, enabled or not).
    pub fn bind(&self) -> Result<std::net::SocketAddr, String> {
        let raw = self.bind.as_deref().unwrap_or(DEFAULT_WORKER_PLANE_BIND);
        raw.parse()
            .map_err(|_| format!("worker_plane: bind {raw:?} must be a host:port socket address"))
    }

    /// The parsed transport client-auth mode.
    pub fn auth(&self) -> Result<faktor_server::WorkerPlaneAuth, String> {
        match self.auth.as_deref() {
            None | Some("worker_tokens") => Ok(faktor_server::WorkerPlaneAuth::WorkerTokens),
            Some("gateway_mtls") => Ok(faktor_server::WorkerPlaneAuth::GatewayMtls),
            Some(other) => Err(format!(
                "worker_plane: auth {other:?} must be one of worker_tokens|gateway_mtls"
            )),
        }
    }

    /// The resolved worker-plane bind/transport configuration (`None` while
    /// the section is disabled — the daemon then binds no second socket).
    /// Every boundary refusal is the server crate's typed error, rendered
    /// with its stable machine code so startup refusals are unmistakable.
    pub fn resolve(&self) -> Result<Option<faktor_server::WorkerPlaneBindConfig>, String> {
        let bind = self.bind()?;
        let auth = self.auth()?;
        if !self.enabled {
            return Ok(None);
        }
        let config = faktor_server::WorkerPlaneBindConfig {
            bind,
            transport: if self.tls {
                faktor_server::WorkerPlaneTransport::Tls
            } else {
                faktor_server::WorkerPlaneTransport::Plaintext
            },
            trusted_gateway: self.trusted_gateway,
            auth,
            bearer: self.bearer.clone(),
        };
        config
            .validate()
            .map_err(|refusal| format!("worker_plane: [{}] {refusal}", refusal.code()))?;
        Ok(Some(config))
    }

    /// Validate the section (called by [`Config::validate`] on both load
    /// paths). Shape errors (bind/auth) are always errors; the deployment
    /// boundary rules apply when the section is enabled.
    pub fn validate(&self) -> Result<(), String> {
        let _ = self.resolve()?;
        Ok(())
    }
}

/// The additive `[worker_node]` section: THIS host acting as a remote worker
/// node of a control plane (the client half of the `[workers]` plane).
///
/// Strict and additive:
///
/// - `enabled` (default `false`): while disabled `faktor worker run` refuses
///   typed before any network or filesystem effect — the disabled-parity
///   state of every pre-existing config;
/// - `worker_id`, `token` XOR `token_payload`, `control_plane_url`: the
///   registration identity, credential and daemon base URL (required when
///   enabled; the token never rides the config log line). `token_payload`
///   names a payload inside `payload_dir` (the strict operator-staged
///   contract: regular file, 0600-style on unix, bounded, non-empty); a
///   missing/corrupt/too-permissive token payload refuses before any
///   network or filesystem effect beyond the payload read itself
///   (`token_file` is accepted as a legacy alias and treated as a payload
///   NAME — absolute paths are refused);
/// - `trust_domain` (required) and the capability advertisement (`os`,
///   `arch`, `toolchains`, `sandbox`, `network`, `region`, `cpu_cores`,
///   `memory_mb`, `gpu`): the advertisement the plane reconciles. Only jobs
///   whose required toolchains/sandbox/network profile the worker advertises
///   are ever claimed;
/// - `claim_deadline_ms` / `claim_interval_ms` / `heartbeat_interval_ms`:
///   the bounded long-poll/heartbeat cadence (clamped by the worker crate);
/// - `payload_dir`: the local directory the job payloads are staged in
///   (`<payload_dir>/<digest>`) AND the directory `token_payload` is
///   resolved from (default `<data_dir>/worker_payloads`); a missing job
///   payload is fail-closed (`PayloadUnavailable`: nothing executes,
///   nothing submits);
/// - `discard_workspace_on_success` (default `true`): bounded disk;
/// - `iterations`: the bounded loop budget of one `worker run` invocation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct WorkerNodeCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub worker_id: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
    /// The payload NAME (inside `payload_dir`) holding the registration
    /// token. `token_file` is accepted as a legacy alias.
    #[serde(default, alias = "token_file")]
    pub token_payload: Option<String>,
    #[serde(default)]
    pub control_plane_url: Option<String>,
    #[serde(default)]
    pub trust_domain: Option<String>,
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub arch: Option<String>,
    #[serde(default)]
    pub toolchains: Vec<String>,
    #[serde(default)]
    pub sandbox: Vec<String>,
    #[serde(default)]
    pub network: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub cpu_cores: u32,
    #[serde(default)]
    pub memory_mb: u64,
    #[serde(default)]
    pub gpu: bool,
    #[serde(default)]
    pub claim_deadline_ms: Option<i64>,
    #[serde(default)]
    pub claim_interval_ms: Option<i64>,
    #[serde(default)]
    pub heartbeat_interval_ms: Option<i64>,
    #[serde(default)]
    pub payload_dir: Option<String>,
    #[serde(default)]
    pub discard_workspace_on_success: Option<bool>,
    #[serde(default)]
    pub iterations: Option<u32>,
}

impl WorkerNodeCfg {
    /// Validate the section (called by [`Config::validate`] on both load
    /// paths). A disabled section validates nothing beyond its shape.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(payload_dir) = &self.payload_dir {
            WorkerNodeCfg::validate_payload_dir(payload_dir)?;
        }
        if let Some(name) = &self.token_payload {
            crate::payload::PayloadDir::validate_name(name)
                .map_err(|e| format!("worker_node: token_payload {e}"))?;
        }
        if !self.enabled {
            return Ok(());
        }
        let _ = self.worker_id()?;
        let _ = self.control_plane_url()?;
        let _ = self.trust_domain()?;
        match (&self.token, &self.token_payload) {
            (Some(_), Some(_)) => return Err(
                "worker_node: `token` and `token_payload` are mutually exclusive; set exactly one"
                    .into(),
            ),
            (None, None) => return Err(
                "worker_node: an enabled [worker_node] section requires `token` or `token_payload`"
                    .into(),
            ),
            _ => {}
        }
        let _ = self.capabilities()?;
        if let Some(deadline) = self.claim_deadline_ms {
            if !(0..=faktor_worker::MAX_CLAIM_DEADLINE_MS).contains(&deadline) {
                return Err(format!(
                    "worker_node: claim_deadline_ms must be 0..={}",
                    faktor_worker::MAX_CLAIM_DEADLINE_MS
                ));
            }
        }
        if let Some(interval) = self.claim_interval_ms {
            if interval < faktor_worker::MIN_CLAIM_INTERVAL_MS {
                return Err(format!(
                    "worker_node: claim_interval_ms must be >= {}",
                    faktor_worker::MIN_CLAIM_INTERVAL_MS
                ));
            }
        }
        if let Some(iterations) = self.iterations {
            if iterations == 0 || iterations > faktor_worker::MAX_LOOP_ITERATIONS {
                return Err(format!(
                    "worker_node: iterations must be 1..={}",
                    faktor_worker::MAX_LOOP_ITERATIONS
                ));
            }
        }
        Ok(())
    }

    pub fn worker_id(&self) -> Result<faktor_worker::WorkerId, String> {
        let raw = self.worker_id.clone().ok_or_else(|| {
            "worker_node: an enabled [worker_node] section requires `worker_id`".to_string()
        })?;
        faktor_worker::WorkerId::try_new(raw).map_err(|e| format!("worker_node: {e}"))
    }

    /// Validate one configured payload directory: bounded ASCII, no control
    /// characters, no `..` traversal (absolute paths allowed; relative paths
    /// live under the data dir).
    pub fn validate_payload_dir(raw: &str) -> Result<(), String> {
        if raw.is_empty() || raw.len() > MAX_PAYLOAD_ROOT_BYTES || !raw.is_ascii() {
            return Err(format!(
                "worker_node: payload_dir must be 1..={MAX_PAYLOAD_ROOT_BYTES} ASCII bytes"
            ));
        }
        if raw.bytes().any(|b| b.is_ascii_control()) {
            return Err("worker_node: payload_dir contains control characters".into());
        }
        if raw.split(['/', '\\']).any(|part| part == "..") {
            return Err(format!(
                "worker_node: payload_dir {raw:?} must not contain `..` traversal"
            ));
        }
        Ok(())
    }

    /// The resolved staged payload directory (default
    /// `<data_dir>/worker_payloads`): job payloads AND the registration
    /// token payload resolve under it.
    pub fn payload_root(&self, data_dir: &Path) -> Result<std::path::PathBuf, String> {
        match &self.payload_dir {
            None => Ok(data_dir.join("worker_payloads")),
            Some(raw) => {
                Self::validate_payload_dir(raw)?;
                let raw_path = Path::new(raw);
                if raw_path.is_absolute() {
                    Ok(raw_path.to_path_buf())
                } else {
                    Ok(data_dir.join(raw_path))
                }
            }
        }
    }

    /// The daemon base URL (http(s), no trailing slash).
    pub fn control_plane_url(&self) -> Result<String, String> {
        let raw = self.control_plane_url.clone().ok_or_else(|| {
            "worker_node: an enabled [worker_node] section requires `control_plane_url`".to_string()
        })?;
        let raw = raw.trim_end_matches('/').to_string();
        if !(raw.starts_with("https://") || raw.starts_with("http://")) || raw.len() > 2048 {
            return Err("worker_node: control_plane_url must be an http(s) URL".into());
        }
        Ok(raw)
    }

    pub fn trust_domain(&self) -> Result<String, String> {
        let raw = self.trust_domain.clone().ok_or_else(|| {
            "worker_node: an enabled [worker_node] section requires `trust_domain`".to_string()
        })?;
        let raw = raw.trim().to_ascii_lowercase();
        if raw.is_empty() || raw.len() > 128 || !raw.bytes().all(|b| b.is_ascii_graphic()) {
            return Err("worker_node: trust_domain must be 1..=128 printable ASCII bytes".into());
        }
        Ok(raw)
    }

    /// The versioned capability advertisement this node registers.
    pub fn capabilities(&self) -> Result<faktor_worker::WorkerCapabilities, String> {
        let network = match self.network.as_deref() {
            None | Some("none") => faktor_worker::NetworkProfile::None,
            Some("egress_restricted") => faktor_worker::NetworkProfile::EgressRestricted,
            Some("full") => faktor_worker::NetworkProfile::Full,
            Some(other) => {
                return Err(format!(
                    "worker_node: network {other:?} must be one of none|egress_restricted|full"
                ));
            }
        };
        let cpu_cores = if self.cpu_cores > 0 {
            self.cpu_cores
        } else {
            std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(1)
        };
        let memory_mb = if self.memory_mb > 0 {
            self.memory_mb
        } else {
            4_096
        };
        let sandbox = self
            .sandbox
            .iter()
            .map(|tag| faktor_worker::SandboxCapability::try_new(tag.clone()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("worker_node: {e}"))?;
        let mut capabilities = faktor_worker::WorkerCapabilities {
            os: self
                .os
                .clone()
                .unwrap_or_else(|| std::env::consts::OS.to_string()),
            arch: self
                .arch
                .clone()
                .unwrap_or_else(|| std::env::consts::ARCH.to_string()),
            toolchains: self.toolchains.clone(),
            sandbox,
            network,
            cpu_cores,
            memory_mb,
            gpu: self.gpu.then(|| faktor_worker::GpuCapability {
                model: "unknown".into(),
                count: 1,
                memory_mb: 0,
            }),
            region: self.region.clone().unwrap_or_else(|| "local".to_string()),
            trust_domain: self.trust_domain()?,
            protocol_version: faktor_worker::WORKER_PROTOCOL_VERSION,
        };
        capabilities
            .normalize()
            .map_err(|e| format!("worker_node: {e}"))?;
        Ok(capabilities)
    }
}

/// The additive `[enterprise]` section: the enterprise retention/audit/
/// admin plane (retention classes + guarded GC, the append-only audit
/// ledger, deletion jobs, admin settings and the effective-config
/// attestation). Disabled by default: no database is created, every
/// `/native/enterprise/*` route answers a typed 409 `enterprise_disabled`,
/// and the daemon is otherwise byte-identical.
///
/// Strict by construction (`deny_unknown_fields`: unknown keys, duplicates
/// and wrong value types are parse errors on both load paths):
///
/// - `enabled` (default `false`);
/// - `database`: an optional simple FILE NAME relative to the data dir
///   (default `enterprise.db`); paths/traversal are refused;
/// - `organization`: the tenant the local operator administers (required
///   when enabled; also the principal used by the `faktor enterprise`
///   local-parity subcommands);
/// - `[enterprise.policy]`: the LOCAL organization policy layer of the
///   layered configuration (allowed sets; intersect-only);
/// - `[enterprise.preferences]`: the LOCAL user preference layer (chosen
///   values; refused when outside the policy).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct EnterpriseCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub organization: Option<String>,
    #[serde(default)]
    pub policy: EnterprisePolicyCfg,
    #[serde(default)]
    pub preferences: EnterprisePreferenceCfg,
}

/// The `[enterprise.policy]` keys: allowed sets per configuration key
/// (intersect-only policy semantics; an empty list is refused by the
/// resolver, never silently accepted).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct EnterprisePolicyCfg {
    #[serde(default)]
    pub network: Option<Vec<String>>,
    #[serde(default)]
    pub providers: Option<Vec<String>>,
    #[serde(default)]
    pub models: Option<Vec<String>>,
    #[serde(default)]
    pub tool_grants: Option<Vec<String>>,
    #[serde(default)]
    pub retention: Option<Vec<String>>,
}

/// The `[enterprise.preferences]` keys: the chosen value per configuration
/// key (a preference outside the effective policy is refused).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct EnterprisePreferenceCfg {
    #[serde(default)]
    pub network: Option<String>,
    #[serde(default)]
    pub providers: Option<String>,
    #[serde(default)]
    pub models: Option<String>,
    #[serde(default)]
    pub tool_grants: Option<String>,
    #[serde(default)]
    pub retention: Option<String>,
}

/// The default enterprise-plane database file name (relative to the daemon
/// data dir).
pub const DEFAULT_ENTERPRISE_DATABASE: &str = "enterprise.db";

impl EnterpriseCfg {
    /// The resolved database path (`None` while disabled).
    pub fn enterprise_path(&self, data_dir: &Path) -> Result<Option<std::path::PathBuf>, String> {
        if !self.enabled {
            return Ok(None);
        }
        let name = self
            .database
            .clone()
            .unwrap_or_else(|| DEFAULT_ENTERPRISE_DATABASE.to_string());
        CloudCfg::validate_database_name("enterprise database", &name)?;
        Ok(Some(data_dir.join(name)))
    }

    /// The tenant the local operator administers.
    pub fn organization(&self) -> Result<Option<faktor_cloud::OrganizationId>, String> {
        if !self.enabled {
            return Ok(None);
        }
        let raw = self
            .organization
            .as_deref()
            .ok_or("enterprise: an enabled [enterprise] section requires `organization`")?;
        let trimmed = raw.trim();
        faktor_cloud::OrganizationId::try_new(trimmed)
            .map(Some)
            .map_err(|e| format!("enterprise: organization: {e}"))
    }

    /// The ordered local layers of the layered configuration: the
    /// organization POLICY layer (optional) then the user PREFERENCE layer
    /// (optional). Semantics and ceilings are enforced by
    /// [`faktor_cloud::resolve_layers`], the ONE resolver both this local
    /// parity path and the server route use.
    pub fn layers(&self) -> Result<Vec<faktor_cloud::ConfigLayer>, String> {
        use faktor_cloud::{ConfigKey, ConfigLayer, ConfigScope, LayerSemantics, LayerValue};
        let mut layers = Vec::new();
        let policy_values: Vec<(ConfigKey, LayerValue)> = [
            (ConfigKey::Network, self.policy.network.as_ref()),
            (ConfigKey::Providers, self.policy.providers.as_ref()),
            (ConfigKey::Models, self.policy.models.as_ref()),
            (ConfigKey::ToolGrants, self.policy.tool_grants.as_ref()),
            (ConfigKey::Retention, self.policy.retention.as_ref()),
        ]
        .into_iter()
        .filter_map(|(key, values)| values.map(|values| (key, LayerValue::Policy(values.clone()))))
        .collect();
        if !policy_values.is_empty() {
            layers.push(ConfigLayer {
                scope: ConfigScope::Organization,
                semantics: LayerSemantics::Policy,
                scope_ref: self.organization.clone(),
                revision: 1,
                values: policy_values.into_iter().collect(),
            });
        }
        let preference_values: Vec<(ConfigKey, LayerValue)> = [
            (ConfigKey::Network, self.preferences.network.as_ref()),
            (ConfigKey::Providers, self.preferences.providers.as_ref()),
            (ConfigKey::Models, self.preferences.models.as_ref()),
            (ConfigKey::ToolGrants, self.preferences.tool_grants.as_ref()),
            (ConfigKey::Retention, self.preferences.retention.as_ref()),
        ]
        .into_iter()
        .filter_map(|(key, value)| value.map(|value| (key, LayerValue::Preference(value.clone()))))
        .collect();
        if !preference_values.is_empty() {
            layers.push(ConfigLayer {
                scope: ConfigScope::User,
                semantics: LayerSemantics::Preference,
                scope_ref: self.organization.clone(),
                revision: 1,
                values: preference_values.into_iter().collect(),
            });
        }
        for layer in &layers {
            layer.validate().map_err(|e| format!("enterprise: {e}"))?;
        }
        Ok(layers)
    }

    /// Validate the section (called by [`Config::validate`] on both load
    /// paths). A disabled section validates nothing beyond its shape.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(database) = &self.database {
            CloudCfg::validate_database_name("enterprise database", database)?;
        }
        if !self.enabled {
            return Ok(());
        }
        let _ = self.organization()?;
        let _ = self.layers()?;
        Ok(())
    }
}

/// The additive `[embeddings]` section: ONE selected semantic embedding
/// provider. Strict by construction (map-only parsing: unknown keys,
/// duplicate keys, non-object shapes and wrong value types are parse
/// errors) and strictly additive (an absent section keeps no embedder):///
/// - `provider` names a REGISTERED provider instance id;
/// - `model` names the embedding model that instance serves;
/// - `policy` decides what an unresolvable selection means:
///   `best_effort` (default) degrades to no embedder with a warning, while
///   `required` refuses startup — the daemon never boots claiming semantic
///   retrieval it cannot honor.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EmbeddingCfg {
    pub provider: String,
    pub model: String,
    pub policy: EmbeddingPolicy,
}

/// Selection strictness of the `[embeddings]` section. See [`EmbeddingCfg`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingPolicy {
    /// An unresolvable selection degrades to no embedder (honest lexical/
    /// symbol-only retrieval) with a warning.
    #[default]
    BestEffort,
    /// An unresolvable selection fails daemon startup.
    Required,
}

impl<'de> serde::Deserialize<'de> for EmbeddingPolicy {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        let value = String::deserialize(de)?;
        match value.as_str() {
            "best_effort" => Ok(Self::BestEffort),
            "required" => Ok(Self::Required),
            other => Err(D::Error::custom(format!(
                "unknown embedding policy {other:?}; expected \"best_effort\" or \"required\""
            ))),
        }
    }
}

/// The `[embeddings]` keys, in stable order (unknown-field errors list
/// them).
pub const EMBEDDING_FIELDS: &[&str] = &["provider", "model", "policy"];

/// Strict bound of the configured embedding provider/model ids: long enough
/// for real ids, short enough to stay journal-safe.
pub const MAX_EMBEDDING_ID_BYTES: usize = 256;

impl<'de> serde::Deserialize<'de> for EmbeddingCfg {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error as _, MapAccess, Visitor};

        struct SectionVisitor;

        impl<'de> Visitor<'de> for SectionVisitor {
            type Value = EmbeddingCfg;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("the [embeddings] section as a JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<EmbeddingCfg, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut provider: Option<String> = None;
                let mut model: Option<String> = None;
                let mut policy: Option<EmbeddingPolicy> = None;
                let mut seen: u8 = 0;
                while let Some(key) = map.next_key::<String>()? {
                    let (bit, name) = match key.as_str() {
                        "provider" => (1u8, "provider"),
                        "model" => (2, "model"),
                        "policy" => (4, "policy"),
                        other => return Err(A::Error::unknown_field(other, EMBEDDING_FIELDS)),
                    };
                    if seen & bit != 0 {
                        return Err(A::Error::duplicate_field(name));
                    }
                    seen |= bit;
                    match bit {
                        1 => provider = Some(map.next_value()?),
                        2 => model = Some(map.next_value()?),
                        _ => policy = Some(map.next_value()?),
                    }
                }
                Ok(EmbeddingCfg {
                    provider: provider.ok_or_else(|| A::Error::missing_field("provider"))?,
                    model: model.ok_or_else(|| A::Error::missing_field("model"))?,
                    policy: policy.unwrap_or_default(),
                })
            }
        }

        de.deserialize_map(SectionVisitor)
    }
}

impl EmbeddingCfg {
    /// Semantic validation shared by both load paths: ids are non-empty and
    /// bounded. A registry-resolvable check happens at daemon build, where
    /// the provider registry exists.
    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [("provider", &self.provider), ("model", &self.model)] {
            if value.trim().is_empty() {
                return Err(format!("embeddings: {name} is empty"));
            }
            if value.len() > MAX_EMBEDDING_ID_BYTES {
                return Err(format!(
                    "embeddings: {name} exceeds {MAX_EMBEDDING_ID_BYTES} bytes"
                ));
            }
        }
        Ok(())
    }
}

/// The additive `[verification]` section (daemon verification policy).
/// Strictly additive with `serde(default)`: an absent section (or absent
/// keys inside it) keep the crate defaults (quick ≤ 60 s, unit ≤ 600 s
/// inline, full in background). `quick_max_s: 0` disables the verification
/// service entirely (fail closed — mutating turns classify Unverified).
/// Unknown keys inside the section are parse errors (strict both on the
/// lenient and the strict load path).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationCfg {
    #[serde(default = "default_quick_max_s")]
    pub quick_max_s: u64,
    #[serde(default = "default_unit_max_s")]
    pub unit_max_s: u64,
    #[serde(default = "default_full_as_background")]
    pub full_as_background: bool,
}

fn default_quick_max_s() -> u64 {
    60
}
fn default_unit_max_s() -> u64 {
    600
}
fn default_full_as_background() -> bool {
    true
}

impl Default for VerificationCfg {
    fn default() -> Self {
        Self {
            quick_max_s: default_quick_max_s(),
            unit_max_s: default_unit_max_s(),
            full_as_background: default_full_as_background(),
        }
    }
}

/// The additive `[sandbox]` section (daemon sandbox policy overrides).
/// `network` rows are parsed destination-allowlist rules in the security
/// crate's rule syntax (e.g. `http://127.0.0.1:8080`); `None` keeps the
/// sandbox crate's frozen default provider-endpoint allowlist, while an
/// explicit list — even an empty one (deny-all) — replaces it. The
/// `network_guarantee` (`none` default, `best_effort`, `required`) declares
/// what the policy requires of OS-level network isolation for shell
/// commands. Unknown keys inside the section are parse errors.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SandboxCfg {
    #[serde(default)]
    pub network: Option<Vec<String>>,
    #[serde(default)]
    pub network_guarantee: SandboxGuarantee,
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

fn default_config_version() -> u32 {
    1
}

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

impl VerificationCfg {
    /// The verification policy this section resolves to. `None` when
    /// `quick_max_s` is 0: the verification service is DISABLED (fail
    /// closed — mutating turns classify Unverified, never silently
    /// complete). Sane values map onto the crate policy whose per-category
    /// budgets gate every check (`budget_for`).
    pub fn policy(&self) -> Option<faktor_verify::exec::VerificationPolicy> {
        if self.quick_max_s == 0 {
            return None;
        }
        Some(faktor_verify::exec::VerificationPolicy {
            quick_max: std::time::Duration::from_secs(self.quick_max_s),
            unit_max: std::time::Duration::from_secs(self.unit_max_s),
            full_as_background: self.full_as_background,
            min_inline: std::time::Duration::from_secs(5),
        })
    }
}

impl Config {
    /// The daemon sandbox policy this config resolves to (pure mapping,
    /// used by the daemon build and by strict config validation). The
    /// section overrides the sandbox crate's defaults: an explicit
    /// `network` row list replaces the network gate (parsed strictly — one
    /// unparseable rule fails the whole policy), and the configured
    /// guarantee rides into `SandboxPolicy::network_guarantee`.
    pub fn sandbox_policy(&self) -> Result<SandboxPolicy, String> {
        let mut policy = SandboxPolicy::default();
        if let Some(rows) = &self.sandbox.network {
            policy.network =
                NetworkGate::parse(rows).map_err(|e| format!("network rule error: {e}"))?;
        }
        policy.network_guarantee = self.sandbox.network_guarantee;
        Ok(policy)
    }
}

/// The wire family of an `open_ai` provider entry.
///
/// - `chat` (default for CUSTOM endpoints): `POST /chat/completions` — the
///   classic shape every OpenAI-compatible server implements.
/// - `responses` (default for the OFFICIAL `api.openai.com` endpoint): the
///   native Responses API (`POST /responses`).
///
/// The default is chosen from the configured `base_url` when `api` is
/// absent; an explicit `api` always wins (including `api = "chat"` on the
/// official endpoint, for deployments/proxies that only speak Chat
/// Completions). Any other value is a strict parse error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiApi {
    Chat,
    Responses,
}

/// The typed default of [`ProviderCfg::Ollama::allow_loopback`]: the local
/// runtime's own documented endpoint is loopback.
fn default_ollama_allow_loopback() -> bool {
    true
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderCfg {
    Ollama {
        id: String,
        base_url: Option<String>,
        /// Explicit loopback address-class rule for this LOCAL runtime: its
        /// documented default endpoint is `http://127.0.0.1:11434`, so the
        /// typed default is `true`. Set `false` to refuse loopback for this
        /// entry. Never a global exception: every other special address
        /// class (private, link-local/metadata, CGNAT, documentation, ...)
        /// stays refused for every provider.
        #[serde(default = "default_ollama_allow_loopback")]
        allow_loopback: bool,
        #[serde(default)]
        pricing: Option<ProviderPricingCfg>,
    },
    OpenAi {
        id: String,
        base_url: String,
        api_key_env: Option<String>,
        /// Wire family override. Absent = the documented endpoint default:
        /// the official OpenAI endpoint prefers the modern Responses API
        /// while every custom `base_url` stays on Chat Completions (custom
        /// compatible servers rarely implement `/responses`). Explicit
        /// `chat`/`responses` always wins; any other value fails to parse.
        #[serde(default)]
        api: Option<OpenAiApi>,
        /// Explicit loopback address-class rule (default `false`: a remote
        /// endpoint whose host resolves onto loopback is a rebinding
        /// signal). `true` is the deliberate opt-in for a local
        /// OpenAI-compatible proxy/runtime. Only loopback can be opted
        /// into; no other special class is ever allowed.
        #[serde(default)]
        allow_loopback: bool,
        #[serde(default)]
        pricing: Option<ProviderPricingCfg>,
    },
    Anthropic {
        id: String,
        api_key_env: Option<String>,
        /// Explicit loopback address-class rule (default `false`: a remote
        /// endpoint whose host resolves onto loopback is a rebinding
        /// signal). `true` is the deliberate opt-in for a local
        /// OpenAI-compatible proxy/runtime. Only loopback can be opted
        /// into; no other special class is ever allowed.
        #[serde(default)]
        allow_loopback: bool,
        #[serde(default)]
        pricing: Option<ProviderPricingCfg>,
    },
    Google {
        id: String,
        api_key_env: Option<String>,
        /// Explicit loopback address-class rule (default `false`: a remote
        /// endpoint whose host resolves onto loopback is a rebinding
        /// signal). `true` is the deliberate opt-in for a local
        /// OpenAI-compatible proxy/runtime. Only loopback can be opted
        /// into; no other special class is ever allowed.
        #[serde(default)]
        allow_loopback: bool,
        #[serde(default)]
        pricing: Option<ProviderPricingCfg>,
    },
    DeepSeek {
        id: String,
        profile: String,
        base_url: Option<String>,
        api_key_env: Option<String>,
        /// Explicit loopback address-class rule (default `false`: a remote
        /// endpoint whose host resolves onto loopback is a rebinding
        /// signal). `true` is the deliberate opt-in for a local
        /// OpenAI-compatible proxy/runtime. Only loopback can be opted
        /// into; no other special class is ever allowed.
        #[serde(default)]
        allow_loopback: bool,
        #[serde(default)]
        pricing: Option<ProviderPricingCfg>,
    },
    Gateway {
        id: String,
        base_url: String,
        api_key_env: Option<String>,
        /// Explicit loopback address-class rule (default `false`: a remote
        /// endpoint whose host resolves onto loopback is a rebinding
        /// signal). `true` is the deliberate opt-in for a local
        /// OpenAI-compatible proxy/runtime. Only loopback can be opted
        /// into; no other special class is ever allowed.
        #[serde(default)]
        allow_loopback: bool,
        #[serde(default)]
        pricing: Option<ProviderPricingCfg>,
    },
}

/// The additive per-provider `pricing` override section (audit P0-1 /
/// wave-B item A): prices are microUSD PER MILLION TOKENS — the exact unit
/// providers publish — so sub-$1/M list prices ($0.50/M = 500_000 microUSD
/// per million tokens) are representable and can never truncate to a free
/// lie. Example: `{"pricing": {"input_micro_usd_per_million_tokens":
/// 2_500_000, "output_micro_usd_per_million_tokens": 10_000_000}}` ($2.50/M
/// in, $10/M out) inside ONE `providers[]` entry, scoped to that entry's
/// `id`. This is the money surface that makes REAL production economics
/// reach the routing graph — without it, remote (OpenAI-compatible/gateway/
/// deepseek/anthropic/google) models are catalog-priced
/// [`PricingState::Unknown`] (unless the built-in table documents them) and
/// the Economy candidate set EXCLUDES the Unknown ones (no fake zero
/// prices, no 1-microUSD fallback).
///
/// Two independent knobs:
///
/// - exact prices: `input_micro_usd_per_million_tokens` + `output_micro_usd_per_million_tokens`
///   (both REQUIRED together, each >= 1 microUSD per million tokens), plus optional
///   `cache_read_*`/`cache_write_*` (0 = the endpoint publishes no cache price). They price
///   EVERY model the endpoint serves at the declared per-million microUSD values
///   (state `Known`, provenance `UserOverride`). Intended for custom OpenAI-compatible
///   endpoints whose real prices the operator knows; overrides NEVER apply to a local
///   runtime (Ollama rows are `LocalZero` and stay zero).
/// - `pricing_ceiling_micro_usd_per_million_tokens`: a CONSERVATIVE budget bound that prices
///   ONLY models the adapter itself leaves Unknown, at the ceiling on all four price lines
///   (state `ConservativeCeiling`, provenance `Composite` — a ceiling, never a measured
///   price). Known-priced and LocalZero models are untouched.
///
/// Both knobs bump the catalog row's `source_epoch` so settlement can tell
/// the price generation changed. Unknown keys inside `pricing` are parse
/// errors; hostile values (0 input, absurd magnitudes, a table on a local
/// provider) are typed validation errors.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ProviderPricingCfg {
    /// Exact override, microUSD per MILLION tokens.
    pub input_micro_usd_per_million_tokens: Option<u64>,
    pub output_micro_usd_per_million_tokens: Option<u64>,
    pub cache_read_micro_usd_per_million_tokens: Option<u64>,
    pub cache_write_micro_usd_per_million_tokens: Option<u64>,
    /// Conservative ceiling, microUSD per million tokens, applied to
    /// Unknown-priced models only.
    pub pricing_ceiling_micro_usd_per_million_tokens: Option<u64>,
}

/// Ceiling and exact-price magnitude cap (microUSD per million tokens).
/// 1_000_000_000_000 microUSD/million tokens = $1M per million tokens —
/// beyond any production model; anything larger is a hostile/absurd config
/// value.
pub const MAX_PRICING_MICRO_USD_PER_MILLION_TOKENS: u64 = 1_000_000_000_000;

impl ProviderPricingCfg {
    /// True when the section configures anything at all.
    pub fn is_empty(&self) -> bool {
        self == &ProviderPricingCfg::default()
    }

    /// Typed validation of the override surface. Rules:
    ///
    /// - a pricing section under a LOCAL provider (kind `ollama`) is
    ///   refused (Ollama rows are measured LocalZero; a config table must
    ///   never make a local runtime look paid, nor is it honored);
    /// - exact input/output prices are REQUIRED as a pair and each must
    ///   be >= 1 microUSD — `0` is the local-free marker, and a remote
    ///   endpoint must never silently read as free — and <= the magnitude
    ///   cap;
    /// - cache prices are optional, 0 allowed (= no cache price), capped;
    /// - the ceiling must be >= 1 microUSD and <= the cap (a zero ceiling
    ///   would price unknown models as free — the exact lie this audit
    ///   removes).
    pub fn validate(&self, kind: &str) -> Result<(), String> {
        if self.is_empty() {
            return Ok(());
        }
        if kind == "ollama" {
            return Err(
                "pricing overrides on a local (ollama) provider are refused: \
                 ollama rows are measured local-zero cost and a pricing table would only \
                 fabricate a paid profile (id-scoped override tables apply to custom \
                 OpenAI-compatible REMOTE endpoints only)"
                    .to_string(),
            );
        }
        let exact_present = self.input_micro_usd_per_million_tokens.is_some()
            || self.output_micro_usd_per_million_tokens.is_some()
            || self.cache_read_micro_usd_per_million_tokens.is_some()
            || self.cache_write_micro_usd_per_million_tokens.is_some();
        if exact_present {
            let (Some(input), Some(output)) = (
                self.input_micro_usd_per_million_tokens,
                self.output_micro_usd_per_million_tokens,
            ) else {
                return Err(
                    "pricing override table must set BOTH input_micro_usd_per_million_tokens \
                     and output_micro_usd_per_million_tokens (a partial table would silently \
                     price the missing side at 0 microUSD)"
                        .to_string(),
                );
            };
            if input == 0 || output == 0 {
                return Err(
                    "pricing override input/output prices of 0 are refused: 0 microUSD is the \
                     LOCAL-free marker and a remote endpoint must never silently read as free"
                        .to_string(),
                );
            }
            for (name, v) in [
                ("input_micro_usd_per_million_tokens", input),
                ("output_micro_usd_per_million_tokens", output),
                (
                    "cache_read_micro_usd_per_million_tokens",
                    self.cache_read_micro_usd_per_million_tokens.unwrap_or(0),
                ),
                (
                    "cache_write_micro_usd_per_million_tokens",
                    self.cache_write_micro_usd_per_million_tokens.unwrap_or(0),
                ),
            ] {
                if v > MAX_PRICING_MICRO_USD_PER_MILLION_TOKENS {
                    return Err(format!(
                        "{name} = {v} exceeds the magnitude cap of \
                         {MAX_PRICING_MICRO_USD_PER_MILLION_TOKENS} microUSD per million tokens"
                    ));
                }
            }
        }
        if let Some(c) = self.pricing_ceiling_micro_usd_per_million_tokens {
            if c == 0 {
                return Err(
                    "pricing_ceiling_micro_usd_per_million_tokens of 0 is refused: a zero \
                     ceiling would price unknown models as free"
                        .to_string(),
                );
            }
            if c > MAX_PRICING_MICRO_USD_PER_MILLION_TOKENS {
                return Err(format!(
                    "pricing_ceiling_micro_usd_per_million_tokens = {c} exceeds the magnitude \
                     cap of {MAX_PRICING_MICRO_USD_PER_MILLION_TOKENS} microUSD per million \
                     tokens"
                ));
            }
        }
        Ok(())
    }

    /// Map the parsed config onto the provider crate's override policy
    /// (per-million-token quotes; exact = a real [`PriceQuote`], ceiling =
    /// the conservative per-million bound).
    pub(crate) fn to_overrides(&self) -> PricingOverrides {
        let exact = match (
            self.input_micro_usd_per_million_tokens,
            self.output_micro_usd_per_million_tokens,
        ) {
            (Some(input), Some(output)) => Some(PriceQuote {
                input: MicroUsdPerMillionTokens(input),
                output: MicroUsdPerMillionTokens(output),
                cache_read: MicroUsdPerMillionTokens(
                    self.cache_read_micro_usd_per_million_tokens.unwrap_or(0),
                ),
                cache_write: MicroUsdPerMillionTokens(
                    self.cache_write_micro_usd_per_million_tokens.unwrap_or(0),
                ),
            }),
            _ => None,
        };
        PricingOverrides {
            exact,
            ceiling_micro_usd_per_million_tokens: self
                .pricing_ceiling_micro_usd_per_million_tokens
                .map(MicroUsdPerMillionTokens),
        }
    }
}

/// The canonical official OpenAI API base URL (the ONLY OpenAI `base_url`
/// that resolves to [`BillingOrigin::OfficialOpenAi`]).
pub const OPENAI_OFFICIAL_BASE_URL: &str = "https://api.openai.com/v1";

/// The canonical official DeepSeek API base URL (the ONLY DeepSeek
/// `base_url` — besides the absent default — that resolves to
/// [`BillingOrigin::OfficialDeepSeek`]).
pub const DEEPSEEK_OFFICIAL_BASE_URL: &str = "https://api.deepseek.com";

/// Strict endpoint identity: exact string apart from surrounding
/// whitespace and a trailing slash.
fn same_endpoint(configured: &str, canonical: &str) -> bool {
    configured.trim().trim_end_matches('/') == canonical.trim_end_matches('/')
}

impl ProviderCfg {
    pub fn id(&self) -> &str {
        match self {
            ProviderCfg::Ollama { id, .. }
            | ProviderCfg::OpenAi { id, .. }
            | ProviderCfg::Anthropic { id, .. }
            | ProviderCfg::Google { id, .. }
            | ProviderCfg::DeepSeek { id, .. }
            | ProviderCfg::Gateway { id, .. } => id,
        }
    }

    /// The explicit loopback address-class rule this entry wires into its
    /// egress transport: `true` ONLY when this entry's typed config expects
    /// loopback (the local Ollama runtime by default, or a local
    /// OpenAI-compatible proxy that opted in). Everything else stays
    /// external-only; no other special address class is configurable.
    pub fn allows_loopback(&self) -> bool {
        match self {
            ProviderCfg::Ollama { allow_loopback, .. }
            | ProviderCfg::OpenAi { allow_loopback, .. }
            | ProviderCfg::Anthropic { allow_loopback, .. }
            | ProviderCfg::Google { allow_loopback, .. }
            | ProviderCfg::DeepSeek { allow_loopback, .. }
            | ProviderCfg::Gateway { allow_loopback, .. } => *allow_loopback,
        }
    }

    /// The configured endpoint URL of this entry, when it has one (the
    /// config-load destination-class validation inspects literal IPs here).
    fn configured_base_url(&self) -> Option<&str> {
        match self {
            ProviderCfg::Ollama { base_url, .. } => base_url.as_deref(),
            ProviderCfg::OpenAi { base_url, .. } => Some(base_url.as_str()),
            ProviderCfg::Anthropic { .. } | ProviderCfg::Google { .. } => None,
            ProviderCfg::DeepSeek { base_url, .. } => base_url.as_deref(),
            ProviderCfg::Gateway { base_url, .. } => Some(base_url.as_str()),
        }
    }

    /// The transport family of this entry (used to gate the pricing
    /// override surface: local runtimes refuse override tables).
    fn kind(&self) -> &'static str {
        match self {
            ProviderCfg::Ollama { .. } => "ollama",
            ProviderCfg::OpenAi { .. } => "open_ai",
            ProviderCfg::Anthropic { .. } => "anthropic",
            ProviderCfg::Google { .. } => "google",
            ProviderCfg::DeepSeek { .. } => "deepseek",
            ProviderCfg::Gateway { .. } => "gateway",
        }
    }

    /// The OpenAI wire family this entry selects (`None` for non-`open_ai`
    /// entries). An explicit `api` always wins; absent, the OFFICIAL OpenAI
    /// endpoint defaults to the native Responses family (the modern default
    /// for api.openai.com) while any custom `base_url` stays on Chat
    /// Completions (compatible servers rarely implement `/responses`).
    pub fn openai_family(&self) -> Option<faktor_openai::OpenAiFamily> {
        match self {
            ProviderCfg::OpenAi { base_url, api, .. } => Some(match api {
                Some(OpenAiApi::Chat) => faktor_openai::OpenAiFamily::Chat,
                Some(OpenAiApi::Responses) => faktor_openai::OpenAiFamily::Responses,
                None => {
                    if same_endpoint(base_url, OPENAI_OFFICIAL_BASE_URL)
                        || same_endpoint(base_url, "https://api.openai.com")
                    {
                        faktor_openai::OpenAiFamily::Responses
                    } else {
                        faktor_openai::OpenAiFamily::Chat
                    }
                }
            }),
            _ => None,
        }
    }

    /// The configured endpoint's STRICT billing origin (billing-origin
    /// audit): official canonical endpoints resolve to their official
    /// origin, any other `base_url` is a [`BillingOrigin::CustomEndpoint`]
    /// (whatever transport family it speaks), `kind = gateway` and the
    /// deepseek gateway/openrouter profiles are [`BillingOrigin::Gateway`],
    /// and Ollama is [`BillingOrigin::Local`]. The origin is decided by the
    /// ENDPOINT CONFIG ONLY — the instance id (and the adapter's transport
    /// family) never changes it.
    pub fn billing_origin(&self) -> BillingOrigin {
        match self {
            ProviderCfg::Ollama { .. } => BillingOrigin::Local,
            ProviderCfg::OpenAi { base_url, .. } => {
                if same_endpoint(base_url, OPENAI_OFFICIAL_BASE_URL)
                    || same_endpoint(base_url, "https://api.openai.com")
                {
                    BillingOrigin::OfficialOpenAi
                } else {
                    BillingOrigin::CustomEndpoint
                }
            }
            ProviderCfg::Anthropic { .. } => BillingOrigin::OfficialAnthropic,
            ProviderCfg::Google { .. } => BillingOrigin::OfficialGoogle,
            ProviderCfg::DeepSeek {
                profile, base_url, ..
            } => match profile.as_str() {
                "direct" => match base_url.as_deref() {
                    None => BillingOrigin::OfficialDeepSeek,
                    Some(b)
                        if same_endpoint(b, DEEPSEEK_OFFICIAL_BASE_URL)
                            || same_endpoint(b, "https://api.deepseek.com/v1") =>
                    {
                        BillingOrigin::OfficialDeepSeek
                    }
                    Some(_) => BillingOrigin::CustomEndpoint,
                },
                // Both gateway-shaped profiles bill through an aggregator:
                // never the official DeepSeek list price.
                "gateway" | "openrouter" => BillingOrigin::Gateway,
                _ => BillingOrigin::CustomEndpoint,
            },
            ProviderCfg::Gateway { .. } => BillingOrigin::Gateway,
        }
    }

    /// The pricing override section of this entry, when configured.
    pub fn pricing(&self) -> Option<&ProviderPricingCfg> {
        match self {
            ProviderCfg::Ollama { pricing, .. }
            | ProviderCfg::OpenAi { pricing, .. }
            | ProviderCfg::Anthropic { pricing, .. }
            | ProviderCfg::Google { pricing, .. }
            | ProviderCfg::DeepSeek { pricing, .. }
            | ProviderCfg::Gateway { pricing, .. } => pricing.as_ref(),
        }
    }

    /// Validate this entry's pricing override surface (typed errors for
    /// hostile values: 0 input, absurd magnitudes, tables on local
    /// runtimes). Called by the strict config validation and by the
    /// adapter build (the runtime gate — a provider whose pricing config
    /// cannot be honored is never registered).
    pub fn validate_pricing(&self) -> Result<(), String> {
        match self.pricing() {
            Some(p) => p.validate(self.kind()),
            None => Ok(()),
        }
    }

    /// Config-load destination-class validation for a LITERAL base_url:
    /// a non-global literal endpoint is reachable only when the operator
    /// explicitly named it — either as an exact `[sandbox] network` rule
    /// for that destination or, for loopback, through this entry's
    /// `allow_loopback` rule (which also covers a NAME base_url that
    /// resolves onto loopback at connect time). Every other non-global
    /// literal is a typed startup error instead of a confusing runtime
    /// denial. Hostname base URLs cannot be judged without DNS and are
    /// enforced at connect time by the central resolver.
    fn validate_endpoint_address_class(
        &self,
        sandbox: Option<&faktor_security::destination::DestinationPolicy>,
    ) -> Result<(), String> {
        let Some(raw) = self.configured_base_url() else {
            return Ok(());
        };
        // Best-effort authority extraction: URL shape validation happens
        // where the adapter is built; this method only classifies a LITERAL
        // host, and anything that does not reduce to one falls through.
        let Some((scheme, rest)) = raw.split_once("://") else {
            return Ok(());
        };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        let (host, port_text) = if let Some(bracketed) = authority.strip_prefix('[') {
            match bracketed.split_once(']') {
                Some((host, tail)) => (host, tail.strip_prefix(':')),
                None => return Ok(()),
            }
        } else {
            match authority.split_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (authority, None),
            }
        };
        let Ok(ip) = host.parse::<std::net::IpAddr>() else {
            return Ok(());
        };
        let port: Option<u16> = port_text.and_then(|p| p.parse().ok());
        let class = faktor_security::network::classify_ip(ip);
        if class == faktor_security::network::AddressClass::Global {
            return Ok(());
        }
        // An exact allowlist entry for this destination is the operator
        // explicitly naming the literal, so it admits the destination.
        let (is_ipv4, octets) = match ip {
            std::net::IpAddr::V4(v4) => (true, Some(v4.octets())),
            std::net::IpAddr::V6(_) => (false, None),
        };
        // No installed allowlist (allow-all gate) admits every destination,
        // so a literal is admitted too.
        let Some(sandbox) = sandbox else {
            return Ok(());
        };
        if let Ok(target) = faktor_security::destination::RequestTarget::from_parts(
            Some(scheme),
            host,
            port,
            is_ipv4,
            octets,
        ) {
            if matches!(
                target.check_against(sandbox),
                faktor_security::destination::Decision::Allowed
            ) {
                return Ok(());
            }
        }
        if class == faktor_security::network::AddressClass::Loopback && self.allows_loopback() {
            return Ok(());
        }
        Err(format!(
            "base_url is a {class} address, which the egress address policy refuses unless \
             this entry sets `\"allow_loopback\": true` (loopback only) or the [sandbox] \
             network section allowlists that exact destination"
        ))
    }

    /// The configured key read from its env var (never stored in the file;
    /// the runtime never logs or persists the value). `None` when the entry
    /// carries no key env or the env var is unset. Wrapped in
    /// [`SecretValue`] (redacted `Debug`, zeroized on drop, explicit
    /// `expose()`); exposed for the daemon's outbound secret registry,
    /// which registers the SAME values the adapter builds from.
    pub(crate) fn key(&self) -> Option<SecretValue> {
        let env = match self {
            ProviderCfg::Ollama { .. } => return None,
            ProviderCfg::OpenAi { api_key_env, .. }
            | ProviderCfg::Anthropic { api_key_env, .. }
            | ProviderCfg::Google { api_key_env, .. }
            | ProviderCfg::DeepSeek { api_key_env, .. }
            | ProviderCfg::Gateway { api_key_env, .. } => api_key_env,
        };
        env.as_ref()
            .and_then(|name| std::env::var(name).ok())
            .map(SecretValue::new)
    }

    /// Build the adapter for this config entry over an explicit egress
    /// transport (the daemon passes the policy-checked transport built from
    /// its SandboxPolicy network gate + outbound secret scan; tests pass a
    /// default-allow one). Every provider is wrapped with its CONFIGURED
    /// instance id so the registry resolves by id (two OpenAI-compatible
    /// endpoints never overwrite each other; the adapter's family id stays
    /// for capability queries).
    /// Concrete Ollama provider when this entry configures one (the daemon
    /// warm-up keeps the concrete Arc so live probing reaches the SAME
    /// instance the registry serves).
    pub fn build_ollama(
        &self,
        transport: Arc<dyn HttpTransport>,
    ) -> Option<Arc<faktor_ollama::OllamaProvider>> {
        match self {
            ProviderCfg::Ollama { base_url, .. } => {
                let cfg = faktor_ollama::OllamaConfig::new(base_url.clone());
                Some(faktor_ollama::OllamaProvider::new(cfg, transport))
            }
            _ => None,
        }
    }

    pub fn build(&self, transport: Arc<dyn HttpTransport>) -> Result<Arc<dyn Provider>, String> {
        let instance = self.id();
        let provider: Arc<dyn Provider> = match self {
            ProviderCfg::Ollama { base_url, .. } => {
                let cfg = faktor_ollama::OllamaConfig::new(base_url.clone());
                faktor_ollama::OllamaProvider::new(cfg, transport.clone())
            }
            ProviderCfg::OpenAi { base_url, .. } => {
                let mut cfg = faktor_openai::OpenAiConfig::chat(base_url, self.key());
                cfg.family = self
                    .openai_family()
                    .expect("open_ai entries always select an OpenAI family");
                faktor_openai::OpenAiProvider::build(cfg, transport.clone())
            }
            ProviderCfg::Anthropic { .. } => {
                let cfg = faktor_anthropic::AnthropicConfig::new(self.key());
                faktor_anthropic::AnthropicProvider::build(cfg, transport.clone())
            }
            ProviderCfg::Google { .. } => {
                let cfg = faktor_google::GoogleConfig::new(self.key());
                faktor_google::GoogleProvider::build(cfg, transport.clone())
            }
            ProviderCfg::DeepSeek {
                profile, base_url, ..
            } => {
                let cfg = match profile.as_str() {
                    // The direct profile honors a configured base_url (a
                    // DeepSeek-compatible local/proxy endpoint): default is
                    // the native api.deepseek.com.
                    "direct" => {
                        let mut c = faktor_deepseek::DeepSeekConfig::direct(self.key());
                        if let Some(b) = base_url.clone() {
                            c.profile =
                                faktor_deepseek::DeepSeekProfile::Compatible { base_url: b };
                        }
                        c
                    }
                    // A gateway is BYO endpoint: there is no implicit
                    // third-party default, so a missing base_url is a loud
                    // typed refusal instead of a hardcoded vendor.
                    "gateway" => {
                        let Some(base_url) = base_url.clone() else {
                            return Err(
                                "deepseek profile \"gateway\" requires an explicit base_url \
                                 (no implicit gateway endpoint is assumed)"
                                    .into(),
                            );
                        };
                        faktor_deepseek::DeepSeekConfig {
                            profile: faktor_deepseek::DeepSeekProfile::Gateway { base_url },
                            api_key: self.key(),
                            model_overrides: Default::default(),
                        }
                    }
                    "openrouter" => faktor_deepseek::DeepSeekConfig {
                        profile: faktor_deepseek::DeepSeekProfile::OpenRouter,
                        api_key: self.key(),
                        model_overrides: Default::default(),
                    },
                    "compatible" => faktor_deepseek::DeepSeekConfig::compatible(
                        base_url
                            .clone()
                            .unwrap_or_else(|| "http://127.0.0.1:8000".into()),
                        self.key(),
                    ),
                    "local" => faktor_deepseek::DeepSeekConfig {
                        profile: faktor_deepseek::DeepSeekProfile::LocalDerivative {
                            base_url: base_url
                                .clone()
                                .unwrap_or_else(|| "http://127.0.0.1:8000".into()),
                        },
                        api_key: self.key(),
                        model_overrides: Default::default(),
                    },
                    other => {
                        return Err(format!("unknown deepseek profile {other:?}"));
                    }
                };
                faktor_deepseek::build(cfg, transport.clone())
            }
            ProviderCfg::Gateway { base_url, .. } => {
                let cfg = faktor_gateway::GatewayConfig {
                    id: "gateway".into(),
                    base_url: base_url.clone(),
                    api_key: self.key(),
                    extra_headers: faktor_provider::config::ExtraHeaders::empty(),
                    route_prefixes: vec![],
                    default_caps: ModelCapabilities::default(),
                };
                faktor_gateway::build(cfg, transport.clone())
            }
        };
        // Billing-origin audit: EVERY configured endpoint is wrapped with
        // its STRICTLY-resolved billing origin (official canonical endpoint
        // vs custom base_url vs gateway vs local), so a custom
        // OpenAI-compatible endpoint can never inherit official list prices
        // through its transport family id. A configured `pricing` section
        // then applies (exact prices -> UserOverride rows, ceiling ->
        // Composite rows for Unknown-priced models only; both bump the
        // pricing epoch). Hostile values are refused HERE so a provider
        // whose pricing cannot be honored never registers — and the
        // local-runtime (ollama) gate also holds on the raw `build` path
        // (the daemon's warm-up path builds ollama separately, where
        // `Config::validate`/`load_strict` refuse such a config loudly).
        let overrides = match self.pricing() {
            Some(pricing) => {
                pricing.validate(self.kind())?;
                pricing.to_overrides()
            }
            None => PricingOverrides::default(),
        };
        Ok(BillingOriginProvider::wrap(
            provider,
            instance,
            self.billing_origin(),
            overrides,
        ))
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

// --------------------------------------------------------------------------
// [commerce] — Faktor Acquire (docs/acquire.md §13, build step 17)
// --------------------------------------------------------------------------

/// The connector ids the section knows; any other name is a startup error.
pub const COMMERCE_CONNECTOR_IDS: &[&str] = &["1688", "alibaba", "lcsc", "mouser", "digikey"];
/// The default commerce database file name (spec §13).
pub const DEFAULT_COMMERCE_DATABASE: &str = "commerce.db";
/// Bound on one configured database file name.
pub const MAX_COMMERCE_DATABASE_BYTES: usize = 128;
/// Bound on one configured environment-variable NAME (never a value).
pub const MAX_COMMERCE_ENV_NAME_BYTES: usize = 128;
/// Bound on one configured browser profile name.
pub const MAX_COMMERCE_PROFILE_BYTES: usize = 64;
/// Bound on one configured browser executable path.
pub const MAX_COMMERCE_EXECUTABLE_BYTES: usize = 4096;
/// Upper bound on any configured cache TTL (365 days).
pub const MAX_COMMERCE_TTL_S: u64 = 31_536_000;

/// One environment-variable NAME (`[A-Za-z_][A-Za-z0-9_]*`). Values are
/// never accepted: an operator who pastes a secret into `*_env` fails
/// startup instead of silently shipping the value.
fn validate_env_name(field: &str, name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_COMMERCE_ENV_NAME_BYTES {
        return Err(format!(
            "commerce: {field} must be 1..={MAX_COMMERCE_ENV_NAME_BYTES} bytes"
        ));
    }
    let mut bytes = name.bytes();
    let first = bytes.next().unwrap_or(0);
    if !(first.is_ascii_alphabetic() || first == b'_')
        || !bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(format!(
            "commerce: {field} {name:?} must be an environment variable NAME \
             ([A-Za-z_][A-Za-z0-9_]*), never a credential value"
        ));
    }
    Ok(())
}

/// One browser profile name (`[a-z0-9_-]{1,64}`, the browser authority's
/// grammar), validated here so a hostile profile never reaches a path join.
fn validate_profile_name(field: &str, name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_COMMERCE_PROFILE_BYTES {
        return Err(format!(
            "commerce: {field} must be 1..={MAX_COMMERCE_PROFILE_BYTES} bytes"
        ));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return Err(format!("commerce: {field} {name:?} must match [a-z0-9_-]+"));
    }
    Ok(())
}

/// Normalize a present-but-disabled connector subsection away: the resolved
/// config of `{enabled: false}` is byte-identically the absent key
/// (disabled parity).
trait ConnectorEnabled {
    fn connector_enabled(&self) -> bool;
}

fn deserialize_enabled_connector<'de, D, T>(de: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de> + ConnectorEnabled,
{
    let raw = <Option<T> as serde::Deserialize>::deserialize(de)?;
    Ok(raw.filter(ConnectorEnabled::connector_enabled))
}

/// `commerce.connectors.<source>` for the API-key connectors (LCSC,
/// Mouser): the key is referenced by environment-variable name only.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommerceApiConnectorCfg {
    #[serde(default)]
    pub enabled: bool,
    /// The environment variable name holding the API key.
    #[serde(default)]
    pub api_key_env: Option<String>,
}

impl ConnectorEnabled for CommerceApiConnectorCfg {
    fn connector_enabled(&self) -> bool {
        self.enabled
    }
}

impl std::fmt::Debug for CommerceApiConnectorCfg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommerceApiConnectorCfg")
            .field("enabled", &self.enabled)
            // Env-var NAMES are operator config, but Debug output is copied
            // into logs/traces; anything credential-shaped stays redacted.
            .field(
                "api_key_env",
                &self.api_key_env.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// `commerce.connectors.digikey`: the OAuth client id + secret are
/// referenced by environment-variable names only.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommerceDigikeyConnectorCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub client_id_env: Option<String>,
    #[serde(default)]
    pub client_secret_env: Option<String>,
}

impl ConnectorEnabled for CommerceDigikeyConnectorCfg {
    fn connector_enabled(&self) -> bool {
        self.enabled
    }
}

impl std::fmt::Debug for CommerceDigikeyConnectorCfg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommerceDigikeyConnectorCfg")
            .field("enabled", &self.enabled)
            .field(
                "client_id_env",
                &self.client_id_env.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "client_secret_env",
                &self.client_secret_env.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// `commerce.connectors.1688` / `commerce.connectors.alibaba`: browser
/// profile names. The credential lives inside the browser profile, never in
/// this config.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct CommerceProfileConnectorCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub profile: Option<String>,
}

impl ConnectorEnabled for CommerceProfileConnectorCfg {
    fn connector_enabled(&self) -> bool {
        self.enabled
    }
}

/// The `commerce.connectors` map: exactly the five known source ids; any
/// other key is a startup error. A present-but-disabled connector resolves
/// to `None` (disabled parity).
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct CommerceConnectorsCfg {
    #[serde(rename = "1688", deserialize_with = "deserialize_enabled_connector")]
    pub china1688: Option<CommerceProfileConnectorCfg>,
    #[serde(deserialize_with = "deserialize_enabled_connector")]
    pub alibaba: Option<CommerceProfileConnectorCfg>,
    #[serde(deserialize_with = "deserialize_enabled_connector")]
    pub lcsc: Option<CommerceApiConnectorCfg>,
    #[serde(deserialize_with = "deserialize_enabled_connector")]
    pub mouser: Option<CommerceApiConnectorCfg>,
    #[serde(deserialize_with = "deserialize_enabled_connector")]
    pub digikey: Option<CommerceDigikeyConnectorCfg>,
}

impl CommerceConnectorsCfg {
    /// Every enabled connector as `(source id, credential requirement)`,
    /// sorted by source id.
    pub fn enabled(&self) -> Vec<(&'static str, ConnectorCredential<'_>)> {
        let mut rows: Vec<(&'static str, ConnectorCredential<'_>)> = Vec::new();
        if let Some(cfg) = &self.china1688 {
            rows.push(("1688", ConnectorCredential::Profile(cfg.profile.as_deref())));
        }
        if let Some(cfg) = &self.alibaba {
            rows.push((
                "alibaba",
                ConnectorCredential::Profile(cfg.profile.as_deref()),
            ));
        }
        if let Some(cfg) = &self.lcsc {
            rows.push((
                "lcsc",
                ConnectorCredential::ApiKey(cfg.api_key_env.as_deref()),
            ));
        }
        if let Some(cfg) = &self.mouser {
            rows.push((
                "mouser",
                ConnectorCredential::ApiKey(cfg.api_key_env.as_deref()),
            ));
        }
        if let Some(cfg) = &self.digikey {
            rows.push((
                "digikey",
                ConnectorCredential::OAuthPair(
                    cfg.client_id_env.as_deref(),
                    cfg.client_secret_env.as_deref(),
                ),
            ));
        }
        rows
    }
}

/// The credential requirement of one enabled connector: env-var NAMES (or a
/// profile), never values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorCredential<'a> {
    /// A profile-based browser connector.
    Profile(Option<&'a str>),
    /// A single API key env var name.
    ApiKey(Option<&'a str>),
    /// An OAuth client id + secret env var name pair.
    OAuthPair(Option<&'a str>, Option<&'a str>),
}

/// The `commerce.cache` field-level TTLs (seconds; spec §13).
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct CommerceCacheCfg {
    #[serde(default = "default_commerce_discovery_ttl_s")]
    pub discovery_ttl_s: u64,
    #[serde(default = "default_commerce_product_ttl_s")]
    pub product_ttl_s: u64,
    #[serde(default = "default_commerce_price_ttl_s")]
    pub price_ttl_s: u64,
    #[serde(default = "default_commerce_stock_ttl_s")]
    pub stock_ttl_s: u64,
    #[serde(default = "default_commerce_supplier_ttl_s")]
    pub supplier_ttl_s: u64,
}

fn default_commerce_discovery_ttl_s() -> u64 {
    1800
}
fn default_commerce_product_ttl_s() -> u64 {
    21_600
}
fn default_commerce_price_ttl_s() -> u64 {
    1800
}
fn default_commerce_stock_ttl_s() -> u64 {
    900
}
fn default_commerce_supplier_ttl_s() -> u64 {
    86_400
}

impl Default for CommerceCacheCfg {
    fn default() -> Self {
        Self {
            discovery_ttl_s: default_commerce_discovery_ttl_s(),
            product_ttl_s: default_commerce_product_ttl_s(),
            price_ttl_s: default_commerce_price_ttl_s(),
            stock_ttl_s: default_commerce_stock_ttl_s(),
            supplier_ttl_s: default_commerce_supplier_ttl_s(),
        }
    }
}

impl CommerceCacheCfg {
    fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("discovery_ttl_s", self.discovery_ttl_s),
            ("product_ttl_s", self.product_ttl_s),
            ("price_ttl_s", self.price_ttl_s),
            ("stock_ttl_s", self.stock_ttl_s),
            ("supplier_ttl_s", self.supplier_ttl_s),
        ] {
            if value == 0 || value > MAX_COMMERCE_TTL_S {
                return Err(format!(
                    "commerce cache: {name} must be 1..={MAX_COMMERCE_TTL_S} seconds"
                ));
            }
        }
        Ok(())
    }
}

/// The `commerce.browser` block (spec §13): lazy Chromium startup, headed
/// only for interactive login, idle shutdown and hard page/browser bounds.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct CommerceBrowserCfg {
    /// Master browser switch. Default `false`: enabling it is an explicit
    /// operator decision (browser acquisition is the last acquisition
    /// path).
    #[serde(default)]
    pub enabled: bool,
    /// Explicit Chromium executable path; absent = resolve from PATH.
    #[serde(default)]
    pub executable: Option<String>,
    /// Headless acquisition. Interactive `commerce login` always overrides
    /// this to a headed window.
    #[serde(default = "default_commerce_browser_headless")]
    pub headless: bool,
    /// Kill an unused browser after this many seconds.
    #[serde(default = "default_commerce_browser_idle_s")]
    pub idle_shutdown_s: u64,
    /// Maximum simultaneously live browser children.
    #[serde(default = "default_commerce_browser_max")]
    pub max_browsers: usize,
    /// Maximum simultaneously open pages per profile.
    #[serde(default = "default_commerce_browser_pages")]
    pub max_pages_per_profile: usize,
}

fn default_commerce_browser_headless() -> bool {
    true
}
fn default_commerce_browser_idle_s() -> u64 {
    300
}
fn default_commerce_browser_max() -> usize {
    2
}
fn default_commerce_browser_pages() -> usize {
    1
}

impl Default for CommerceBrowserCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            executable: None,
            headless: default_commerce_browser_headless(),
            idle_shutdown_s: default_commerce_browser_idle_s(),
            max_browsers: default_commerce_browser_max(),
            max_pages_per_profile: default_commerce_browser_pages(),
        }
    }
}

impl CommerceBrowserCfg {
    fn validate(&self) -> Result<(), String> {
        if !(1..=86_400).contains(&self.idle_shutdown_s) {
            return Err("commerce browser: idle_shutdown_s must be 1..=86400".into());
        }
        if !(1..=16).contains(&self.max_browsers) {
            return Err("commerce browser: max_browsers must be 1..=16".into());
        }
        if !(1..=8).contains(&self.max_pages_per_profile) {
            return Err("commerce browser: max_pages_per_profile must be 1..=8".into());
        }
        if let Some(executable) = &self.executable {
            if executable.is_empty() || executable.len() > MAX_COMMERCE_EXECUTABLE_BYTES {
                return Err(format!(
                    "commerce browser: executable must be 1..={MAX_COMMERCE_EXECUTABLE_BYTES} bytes"
                ));
            }
            if executable.bytes().any(|b| b.is_ascii_control()) {
                return Err("commerce browser: executable contains control characters".into());
            }
        }
        Ok(())
    }
}

/// The additive `[commerce]` section (docs/acquire.md §13).
///
/// Strict by construction: unknown keys anywhere in the section are startup
/// errors (the derived `deny_unknown_fields` on every nested block plus the
/// fixed connector-id map), non-matching value types are type errors, and
/// [`CommerceCfg::validate`] enforces the semantic bounds. Credentials are
/// referenced by environment-variable NAME only; the section never carries
/// a value and `Debug` redacts every credential-shaped field.
///
/// Disabled parity (spec §13): absent, `{}` and `{enabled: false}` resolve
/// byte-identically — no commerce tool registered, no database created, no
/// connector client, browser profile or broker state, no network request
/// and no schema tokens.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct CommerceCfg {
    /// Whether Faktor Acquire is enabled at all.
    pub enabled: bool,
    /// The commerce database file name. Only the default `commerce.db` is
    /// supported by the commerce store today; a different name is refused at
    /// validation instead of being silently ignored.
    pub database: Option<String>,
    /// Field-level cache TTLs.
    pub cache: CommerceCacheCfg,
    /// Browser acquisition bounds.
    pub browser: CommerceBrowserCfg,
    /// The per-source connector configuration.
    pub connectors: CommerceConnectorsCfg,
}

impl Default for CommerceCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            database: Some(DEFAULT_COMMERCE_DATABASE.to_string()),
            cache: CommerceCacheCfg::default(),
            browser: CommerceBrowserCfg::default(),
            connectors: CommerceConnectorsCfg::default(),
        }
    }
}

impl CommerceCfg {
    /// The resolved database file name (default when absent).
    pub fn database_name(&self) -> &str {
        self.database
            .as_deref()
            .unwrap_or(DEFAULT_COMMERCE_DATABASE)
    }

    /// Strict validation: database name, TTL bounds, browser bounds and the
    /// enabled connectors' credential requirements. A nested enabled section
    /// under `enabled: false` is refused (the daemon never boots a
    /// half-configured commerce surface).
    pub fn validate(&self) -> Result<(), String> {
        let database = self.database_name();
        if database.is_empty()
            || database.len() > MAX_COMMERCE_DATABASE_BYTES
            || database.contains('/')
            || database.contains('\\')
            || database.contains("..")
            || database.contains(':')
            || database.bytes().any(|b| b.is_ascii_control())
        {
            return Err(format!(
                "commerce: database {database:?} must be a plain file name (no paths, no traversal)"
            ));
        }
        if database != DEFAULT_COMMERCE_DATABASE {
            return Err(format!(
                "commerce: database {database:?} is not supported; the commerce store owns \
                 {DEFAULT_COMMERCE_DATABASE:?}"
            ));
        }
        self.cache.validate()?;
        self.browser.validate()?;
        let enabled = self.connectors.enabled();
        if !enabled.is_empty() && !self.enabled {
            return Err("commerce: an enabled connector requires [commerce] enabled = true".into());
        }
        if self.browser.enabled && !self.enabled {
            return Err(
                "commerce: an enabled browser block requires [commerce] enabled = true".into(),
            );
        }
        for (source, credential) in enabled {
            match credential {
                ConnectorCredential::Profile(profile) => {
                    let Some(profile) = profile else {
                        return Err(format!(
                            "commerce connectors: {source} requires a browser profile name"
                        ));
                    };
                    validate_profile_name(&format!("connectors.{source}.profile"), profile)?;
                }
                ConnectorCredential::ApiKey(api_key_env) => {
                    let Some(api_key_env) = api_key_env else {
                        return Err(format!(
                            "commerce connectors: {source} requires api_key_env to name an \
                             environment variable"
                        ));
                    };
                    validate_env_name(&format!("connectors.{source}.api_key_env"), api_key_env)?;
                }
                ConnectorCredential::OAuthPair(client_id, client_secret) => {
                    let (Some(client_id), Some(client_secret)) = (client_id, client_secret) else {
                        return Err(format!(
                            "commerce connectors: {source} requires client_id_env and \
                             client_secret_env to name environment variables"
                        ));
                    };
                    validate_env_name(&format!("connectors.{source}.client_id_env"), client_id)?;
                    validate_env_name(
                        &format!("connectors.{source}.client_secret_env"),
                        client_secret,
                    )?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
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
                },
                ProviderCfg::Ollama {
                    id: "other".into(),
                    base_url: None,
                    pricing: None,
                    allow_loopback: true,
                },
                ProviderCfg::OpenAi {
                    id: "dup".into(),
                    base_url: "http://x".into(),
                    api_key_env: None,
                    api: None,
                    pricing: None,
                    allow_loopback: true,
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
        use faktor_provider::{
            ContentPart, GenericAgentRequest, RequestMessage, RequestMeta, Role,
        };
        use futures::StreamExt as _;

        let make_cfg = |base_url: &str, api: Option<OpenAiApi>| ProviderCfg::OpenAi {
            id: "probe".into(),
            base_url: base_url.into(),
            api_key_env: None,
            api,
            pricing: None,
            allow_loopback: true,
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
        std::env::set_var("KP_TEST_KEY", "secret-value");
        let cfg = ProviderCfg::OpenAi {
            id: "t".into(),
            base_url: "http://x".into(),
            api_key_env: Some("KP_TEST_KEY".into()),
            api: None,
            pricing: None,
            allow_loopback: true,
        };
        assert_eq!(
            cfg.key().as_ref().map(SecretValue::expose),
            Some("secret-value")
        );
        std::env::remove_var("KP_TEST_KEY");
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
        };
        let official_openai_slash = ProviderCfg::OpenAi {
            id: "a2".into(),
            base_url: format!("{OPENAI_OFFICIAL_BASE_URL}/"),
            api_key_env: None,
            api: None,
            pricing: None,
            allow_loopback: true,
        };
        let custom_openai = ProviderCfg::OpenAi {
            id: "corp-proxy".into(),
            base_url: "https://corp.example.com/v1".into(),
            api_key_env: None,
            api: None,
            pricing: None,
            allow_loopback: true,
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
        use faktor_verify::exec::{
            budget_for, BudgetDecision, CheckCategory, CheckKind, CheckSpec,
        };
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
        use faktor_sandbox::{SandboxGuarantee, SandboxPolicy};
        // Absent section: crate defaults (frozen gate, guarantee None).
        let cfg = Config::default();
        let policy = cfg.sandbox_policy().unwrap();
        assert_eq!(policy.network_guarantee, SandboxGuarantee::None);
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
        // best_effort parses; a hostile guarantee value is a parse error;
        // unknown keys inside [sandbox] are rejected.
        for (text, expect) in [
            (
                r#"{"sandbox": {"network_guarantee": "best_effort"}}"#,
                SandboxGuarantee::BestEffort,
            ),
            (
                r#"{"sandbox": {"network_guarantee": "none"}}"#,
                SandboxGuarantee::None,
            ),
        ] {
            std::fs::write(&path, text).unwrap();
            let cfg = Config::load(&path).unwrap();
            assert_eq!(cfg.sandbox_policy().unwrap().network_guarantee, expect);
        }
        for bad in [
            r#"{"sandbox": {"network_guarantee": "mandatory"}}"#,
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
}

#[cfg(test)]
mod completion_cfg_tests {
    //! Adversarial strictness covers for the additive `[completion]` section
    //! (P2 step execution): absent => inert defaults, unknown keys / wrong
    //! shapes / invalid templates are parse or validation errors, and a
    //! configured template resolves to the exact orchestrator policy.
    use super::*;

    fn parse(json: serde_json::Value) -> Result<Config, serde_json::Error> {
        serde_json::from_value(json)
    }

    #[test]
    fn absent_completion_section_keeps_the_inert_defaults() {
        let cfg = parse(serde_json::json!({"model": "m"})).unwrap();
        assert_eq!(cfg.completion, CompletionCfg::default());
        let steps = cfg.completion.steps_config().unwrap();
        assert_eq!(steps.remote, "origin");
        assert_eq!(steps.base_branch, "main");
        assert_eq!(steps.pr_command, None);
    }

    #[test]
    fn configured_completion_section_resolves_to_the_validated_policy() {
        let cfg = parse(serde_json::json!({
            "model": "m",
            "completion": {
                "remote": "upstream",
                "base_branch": "develop",
                "pr_command": "gh pr create --head {branch} --base {base}"
            }
        }))
        .unwrap();
        let steps = cfg.completion.steps_config().unwrap();
        assert_eq!(steps.remote, "upstream");
        assert_eq!(steps.base_branch, "develop");
        assert_eq!(
            steps.pr_command.as_deref(),
            Some("gh pr create --head {branch} --base {base}")
        );
    }

    #[test]
    fn completion_section_is_strict_and_hostile_values_are_refused() {
        // Unknown key anywhere in the section.
        assert!(
            parse(serde_json::json!({"completion": {"pr_command": "x {branch}", "extra": 1}}))
                .is_err()
        );
        // Non-string member.
        assert!(parse(serde_json::json!({"completion": {"pr_command": 7}})).is_err());
        // Positional array shape is not a section.
        assert!(parse(serde_json::json!({"completion": ["origin"]})).is_err());
        // A shell-metachar / unclosed / unknown-placeholder / branch-less
        // template is refused at resolution time (never half-run).
        for (name, command) in [
            ("metachar", "gh pr create --head {branch}; rm -rf /"),
            ("unknown", "gh pr create --head {nope}"),
            ("branchless", "gh pr create --base main"),
            ("unclosed", "gh pr create --head {branch"),
        ] {
            let cfg = parse(serde_json::json!({"completion": {"pr_command": command}})).unwrap();
            let err = cfg.completion.steps_config().unwrap_err();
            assert!(err.starts_with("completion: "), "{name}: {err}");
        }
        // Hostile remote / base names are refused.
        let cfg = parse(serde_json::json!({"completion": {"remote": "a b"}})).unwrap();
        assert!(cfg.completion.steps_config().is_err());
        let cfg = parse(serde_json::json!({"completion": {"base_branch": "a..b"}})).unwrap();
        assert!(cfg.completion.steps_config().is_err());
    }

    #[test]
    fn typed_argv_completion_section_resolves_and_is_strict() {
        // The additive typed argv: spaces in the program path and in one
        // argument stay per-element values.
        let cfg = parse(serde_json::json!({
            "model": "m",
            "completion": {
                "pr_program": "/opt/My Tools/gh",
                "pr_args": ["pr", "create", "--head", "{branch}", "--title", "my PR title"]
            }
        }))
        .unwrap();
        let steps = cfg.completion.steps_config().unwrap();
        assert_eq!(steps.pr_program.as_deref(), Some("/opt/My Tools/gh"));
        assert_eq!(steps.pr_command, None);
        assert_eq!(
            steps.pr_args,
            vec![
                "pr",
                "create",
                "--head",
                "{branch}",
                "--title",
                "my PR title"
            ]
        );

        // Strict shapes inside the section.
        assert!(parse(serde_json::json!({"completion": {"pr_program": 7}})).is_err());
        assert!(parse(serde_json::json!({"completion": {"pr_args": "nope"}})).is_err());
        assert!(parse(serde_json::json!({"completion": {"pr_extra": 1}})).is_err());
        assert!(parse(serde_json::json!({"completion": {"pr_args": [1]}})).is_err());

        // pr_args without a program, and BOTH shapes (ambiguous), refuse.
        let args_only =
            parse(serde_json::json!({"completion": {"pr_args": ["{branch}"]}})).unwrap();
        assert!(args_only.completion.steps_config().is_err());
        let both = parse(serde_json::json!({
            "completion": {
                "pr_command": "gh pr create --head {branch}",
                "pr_program": "/bin/gh",
                "pr_args": ["--head", "{branch}"]
            }
        }))
        .unwrap();
        assert!(both
            .completion
            .steps_config()
            .unwrap_err()
            .starts_with("completion: "));

        // Typed refusals mirror the orchestrator validator exactly.
        for (name, program, args) in [
            ("branchless", "/bin/gh", vec!["--base", "main"]),
            ("unknown", "/bin/gh", vec!["--head", "{nope}"]),
            ("control", "/bin/gh", vec!["--head", "{branch}\u{7}"]),
        ] {
            let cfg = parse(serde_json::json!({
                "completion": {"pr_program": program, "pr_args": args}
            }))
            .unwrap();
            assert!(cfg.completion.steps_config().is_err(), "{name}");
        }
    }

    /// The additive `[cloud]` section: disabled by default, byte-identical
    /// to an absent section while disabled, strictly parsed, and its
    /// databases resolve INSIDE the data dir with options off-by-default.
    #[test]
    fn cloud_section_is_disabled_by_default_and_strictly_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud.json");

        // Absent section == explicit disabled section: same resolved
        // databases (none) and the same serialized config.
        let absent = Config::default();
        assert!(!absent.cloud.enabled);
        assert_eq!(absent.cloud.control_plane_path(dir.path()).unwrap(), None);
        assert_eq!(absent.cloud.scm_path(dir.path()).unwrap(), None);
        std::fs::write(&path, r#"{"model": "m"}"#).unwrap();
        let parsed_absent = Config::load_strict(&path).unwrap();
        std::fs::write(&path, r#"{"model": "m", "cloud": {"enabled": false}}"#).unwrap();
        let parsed_disabled = Config::load_strict(&path).unwrap();
        assert_eq!(
            serde_json::to_value(&parsed_absent).unwrap(),
            serde_json::to_value(&parsed_disabled).unwrap(),
            "a disabled [cloud] section must serialize exactly like an absent one"
        );
        assert_eq!(
            parsed_disabled
                .cloud
                .control_plane_path(dir.path())
                .unwrap(),
            None
        );
        assert_eq!(parsed_disabled.cloud.scm_path(dir.path()).unwrap(), None);

        // Enabled: the default file names resolve under the data dir.
        std::fs::write(&path, r#"{"cloud": {"enabled": true}}"#).unwrap();
        let enabled = Config::load_strict(&path).unwrap();
        assert_eq!(
            enabled.cloud.control_plane_path(dir.path()).unwrap(),
            Some(dir.path().join("control-plane.db"))
        );
        assert_eq!(
            enabled.cloud.scm_path(dir.path()).unwrap(),
            Some(dir.path().join("scm.db"))
        );
        // Explicit simple file names resolve too.
        std::fs::write(
            &path,
            r#"{"cloud": {"enabled": true, "database": "cp.db", "scm_database": "repos.db"}}"#,
        )
        .unwrap();
        let named = Config::load_strict(&path).unwrap();
        assert_eq!(
            named.cloud.control_plane_path(dir.path()).unwrap(),
            Some(dir.path().join("cp.db"))
        );
        assert_eq!(
            named.cloud.scm_path(dir.path()).unwrap(),
            Some(dir.path().join("repos.db"))
        );

        // Strict parsing: unknown/duplicate keys, wrong types, positional
        // arrays and hostile paths are refused by BOTH load paths.
        for bad in [
            r#"{"cloud": {"enabled": "yes"}}"#,
            r#"{"cloud": {"enabled": true, "bogus": 1}}"#,
            r#"{"cloud": {"enabled": true, "enabled": false}}"#,
            r#"{"cloud": {"database": 1}}"#,
            r#"{"cloud": true}"#,
            r#"{"cloud": ["enabled"]}"#,
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(
                Config::load(&path).is_err(),
                "hostile [cloud] must fail: {bad}"
            );
            assert!(Config::load_strict(&path).is_err(), "{bad}");
        }
        for hostile in [
            r#"{"cloud": {"enabled": true, "database": "../escape.db"}}"#,
            r#"{"cloud": {"enabled": true, "database": "/etc/passwd"}}"#,
            r#"{"cloud": {"enabled": true, "database": "sub/dir.db"}}"#,
            r#"{"cloud": {"enabled": true, "database": "a\u{5c}b"}}"#,
            r#"{"cloud": {"enabled": true, "scm_database": ".."}}"#,
        ] {
            std::fs::write(&path, hostile).unwrap();
            assert!(
                Config::load_strict(&path).is_err(),
                "hostile cloud path must fail: {hostile}"
            );
        }
        // A disabled section with a hostile name is still refused (the file
        // never says two different things).
        std::fs::write(
            &path,
            r#"{"cloud": {"enabled": false, "database": "../x"}}"#,
        )
        .unwrap();
        assert!(Config::load_strict(&path).is_err());
    }

    /// The additive payload/SSO/GitHub-App cloud surface: absent sections
    /// stay inert, hostile shapes are refused on both load paths, an enabled
    /// SSO or GitHub App section requires the cloud section, and the
    /// payload root resolves under the data dir with traversal refused.
    #[test]
    fn cloud_payload_sso_and_github_app_sections_are_strict_and_additive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-extra.json");

        let absent = Config::default();
        assert!(absent.cloud.sso.is_none());
        assert!(absent.cloud.github_app.is_none());
        assert_eq!(
            absent.cloud.payload_root(dir.path()).unwrap(),
            dir.path().join("payloads"),
            "the default payload root lives under the data dir"
        );

        for bad in [
            r#"{"cloud": {"payload_dir": 1}}"#,
            r#"{"cloud": {"sso": true}}"#,
            r#"{"cloud": {"sso": {"enabled": true, "bogus": 1}}}"#,
            r#"{"cloud": {"sso": {"enabled": true, "enabled": false}}}"#,
            r#"{"cloud": {"github_app": {"app_id": "x"}}}"#,
            r#"{"cloud": {"github_app": {"enabled": true, "hostile": 1}}}"#,
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(Config::load(&path).is_err(), "hostile shape: {bad}");
            assert!(Config::load_strict(&path).is_err(), "{bad}");
        }
        for hostile_root in [
            r#"{"cloud": {"payload_dir": "../escape"}}"#,
            r#"{"cloud": {"payload_dir": "a/../b"}}"#,
            r#"{"cloud": {"payload_dir": "a\u{5c}b"}}"#,
        ] {
            std::fs::write(&path, hostile_root).unwrap();
            assert!(Config::load_strict(&path).is_err(), "{hostile_root}");
        }

        // An enabled SSO section requires issuer and client id; the payload
        // name of the optional secret is validated whenever present.
        std::fs::write(
            &path,
            r#"{"cloud": {"enabled": true, "sso": {"enabled": true}}}"#,
        )
        .unwrap();
        assert!(Config::load_strict(&path).is_err());
        std::fs::write(
            &path,
            r#"{"cloud": {"enabled": true, "sso": {"enabled": true, "issuer": "https://idp.example/", "client_id": "c"}}}"#,
        )
        .unwrap();
        assert!(Config::load_strict(&path).is_err(), "trailing slash");
        std::fs::write(
            &path,
            r#"{"cloud": {"enabled": true, "sso": {"enabled": true, "issuer": "https://idp.example", "client_id": "c", "client_secret": "../x"}}}"#,
        )
        .unwrap();
        assert!(Config::load_strict(&path).is_err(), "traversal name");
        std::fs::write(
            &path,
            r#"{"cloud": {"enabled": true, "sso": {"enabled": true, "issuer": "https://idp.example", "client_id": "client-1", "client_secret": "idp.secret", "discovery_max_age_ms": 1000, "jwks_max_age_ms": 1000, "max_jwks_refetches": 1}}}"#,
        )
        .unwrap();
        let sso = Config::load_strict(&path).unwrap();
        assert_eq!(
            sso.cloud.sso.as_ref().unwrap().issuer().unwrap(),
            "https://idp.example"
        );
        assert_eq!(
            sso.cloud.payload_root(dir.path()).unwrap(),
            dir.path().join("payloads")
        );

        // The SSO signing-algorithm policy is additive and strict: the
        // default is RS256-only, `none`/unsupported/empty lists and
        // non-list shapes are refused at load.
        std::fs::write(
            &path,
            r#"{"cloud": {"enabled": true, "sso": {"enabled": true, "issuer": "https://idp.example", "client_id": "c", "allowed_algorithms": ["RS256", "HS256"]}}}"#,
        )
        .unwrap();
        let sso = Config::load_strict(&path).unwrap();
        assert_eq!(
            sso.cloud
                .sso
                .as_ref()
                .unwrap()
                .allowed_algorithms()
                .unwrap(),
            vec!["RS256".to_string(), "HS256".to_string()]
        );
        for bad_alg_list in [
            r#"{"cloud": {"enabled": true, "sso": {"enabled": true, "issuer": "https://idp.example", "client_id": "c", "allowed_algorithms": ["none"]}}}"#,
            r#"{"cloud": {"enabled": true, "sso": {"enabled": true, "issuer": "https://idp.example", "client_id": "c", "allowed_algorithms": []}}}"#,
            r#"{"cloud": {"enabled": true, "sso": {"enabled": true, "issuer": "https://idp.example", "client_id": "c", "allowed_algorithms": ["RS512"]}}}"#,
            r#"{"cloud": {"enabled": true, "sso": {"enabled": true, "issuer": "https://idp.example", "client_id": "c", "allowed_algorithms": "RS256"}}}"#,
        ] {
            std::fs::write(&path, bad_alg_list).unwrap();
            assert!(Config::load_strict(&path).is_err(), "{bad_alg_list}");
        }

        // An enabled GitHub App section requires app id, both staged payload
        // names and the tenant organization.
        for bad_app in [
            r#"{"cloud": {"enabled": true, "github_app": {"enabled": true}}}"#,
            r#"{"cloud": {"enabled": true, "github_app": {"enabled": true, "app_id": 7, "private_key": "k.pem"}}}"#,
            r#"{"cloud": {"enabled": true, "github_app": {"enabled": true, "app_id": 7, "private_key": "k.pem", "webhook_secret": "s"}}}"#,
            r#"{"cloud": {"enabled": true, "github_app": {"enabled": true, "app_id": 7, "private_key": "../k.pem", "webhook_secret": "s", "organization": "org_x"}}}"#,
        ] {
            std::fs::write(&path, bad_app).unwrap();
            assert!(Config::load_strict(&path).is_err(), "{bad_app}");
        }
        std::fs::write(
            &path,
            r#"{"cloud": {"enabled": true, "payload_dir": "/srv/faktor/payloads", "github_app": {"enabled": true, "app_id": 7, "private_key": "app.pem", "webhook_secret": "hook.secret", "api_base": "http://127.0.0.1:9/", "organization": "org_local"}}}"#,
        )
        .unwrap();
        let app = Config::load_strict(&path).unwrap();
        let github = app.cloud.github_app.as_ref().unwrap();
        assert_eq!(github.organization().unwrap(), "org_local");
        assert_eq!(
            github.app_config().unwrap().api_base,
            "http://127.0.0.1:9/",
            "the adapter trims the trailing slash itself"
        );
        assert_eq!(
            app.cloud.payload_root(dir.path()).unwrap(),
            std::path::PathBuf::from("/srv/faktor/payloads"),
            "an absolute payload root is honored"
        );

        // Both new sections require [cloud] enabled (an orphan section is a
        // startup refusal, never a silently inert wiring).
        std::fs::write(
            &path,
            r#"{"cloud": {"sso": {"enabled": true, "issuer": "https://idp.example", "client_id": "c"}}}"#,
        )
        .unwrap();
        assert!(Config::load_strict(&path).is_err());
        std::fs::write(
            &path,
            r#"{"cloud": {"github_app": {"enabled": true, "app_id": 7, "private_key": "k.pem", "webhook_secret": "s", "organization": "org"}}}"#,
        )
        .unwrap();
        assert!(Config::load_strict(&path).is_err());
    }

    /// The additive `[billing.report]` schedule: disabled by default with
    /// byte-identical serialization to an absent section, strict parsing,
    /// bounded cadence and the billing/enabled pairing.
    #[test]
    fn billing_report_section_is_disabled_by_default_strict_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.json");
        let absent = Config::default();
        assert!(absent.billing.report.is_none());
        std::fs::write(&path, r#"{"model": "m"}"#).unwrap();
        let parsed_absent = Config::load_strict(&path).unwrap();
        std::fs::write(
            &path,
            r#"{"model": "m", "billing": {"report": {"enabled": false}}}"#,
        )
        .unwrap();
        let parsed_disabled = Config::load_strict(&path).unwrap();
        assert_eq!(
            serde_json::to_value(&parsed_absent).unwrap(),
            serde_json::to_value(&parsed_disabled).unwrap(),
            "a disabled report section serializes like an absent one"
        );

        for bad in [
            r#"{"billing": {"report": true}}"#,
            r#"{"billing": {"report": {"enabled": true, "bogus": 1}}}"#,
            r#"{"billing": {"report": {"interval_ms": "soon"}}}"#,
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(Config::load(&path).is_err(), "{bad}");
        }
        // Enabled report over a disabled billing section is refused.
        std::fs::write(
            &path,
            r#"{"billing": {"enabled": false, "report": {"enabled": true, "base_url": "http://127.0.0.1:9"}}}"#,
        )
        .unwrap();
        assert!(Config::load_strict(&path).is_err());
        // Enabled report requires a base_url.
        std::fs::write(
            &path,
            r#"{"cloud": {"enabled": true}, "billing": {"enabled": true, "organization": "org", "plans": {"pro": {"plan_id": "pro"}}, "report": {"enabled": true}}}"#,
        )
        .unwrap();
        assert!(Config::load_strict(&path).is_err());
        // Bounds: interval/period/backoff are all refused outside their range.
        for (key, value) in [
            ("interval_ms", "10"),
            ("interval_ms", "999999999"),
            ("period_ms", "10"),
            ("period_ms", "99999999999999"),
            ("max_backoff_ms", "-1"),
            ("max_backoff_ms", "999999999"),
            ("max_catch_up", "999999"),
        ] {
            std::fs::write(
                &path,
                format!(
                    r#"{{"cloud": {{"enabled": true}}, "billing": {{"enabled": true, "organization": "org", "plans": {{"pro": {{"plan_id": "pro"}}}}, "report": {{"enabled": true, "base_url": "http://127.0.0.1:9", "{key}": {value}}}}}}}"#
                ),
            )
            .unwrap();
            assert!(Config::load_strict(&path).is_err(), "{key}={value}");
        }
        // A complete report section resolves the strict policy and vendor
        // config; an explicitly empty auth_env selects the unauthenticated
        // local-mock shape.
        std::fs::write(
            &path,
            r#"{"cloud": {"enabled": true}, "billing": {"enabled": true, "organization": "org", "plans": {"pro": {"plan_id": "pro"}}, "report": {"enabled": true, "base_url": "http://127.0.0.1:9/", "auth_env": "", "interval_ms": 5000, "period_ms": 60000, "max_attempts": 2, "retry_base_ms": 0, "max_backoff_ms": 1000, "max_catch_up": 3}}}"#,
        )
        .unwrap();
        let cfg = Config::load_strict(&path).unwrap();
        let report = cfg.billing.report.as_ref().unwrap();
        assert!(report.unauthenticated());
        let policy = report.policy().unwrap();
        assert_eq!(policy.interval_ms, 5000);
        assert_eq!(policy.period_ms, 60000);
        assert_eq!(policy.max_attempts, 2);
        assert_eq!(policy.retry_base_ms, 0);
        assert_eq!(policy.max_backoff_ms, 1000);
        assert_eq!(policy.max_catch_up, 3);
        assert_eq!(
            report.vendor_config().unwrap().base_url,
            "http://127.0.0.1:9"
        );
        // The default catch-up window is the documented 24-period bound.
        std::fs::write(
            &path,
            r#"{"cloud": {"enabled": true}, "billing": {"enabled": true, "organization": "org", "plans": {"pro": {"plan_id": "pro"}}, "report": {"enabled": true, "base_url": "http://127.0.0.1:9/"}}}"#,
        )
        .unwrap();
        let cfg = Config::load_strict(&path).unwrap();
        assert_eq!(
            cfg.billing
                .report
                .as_ref()
                .unwrap()
                .policy()
                .unwrap()
                .max_catch_up,
            faktor_cloud::DEFAULT_REPORT_CATCH_UP
        );
    }

    /// `[worker_node]` payload staging: the payload dir and the token
    /// payload name are validated even while disabled; the resolved root
    /// stays inside its configured base and traversal is refused.
    #[test]
    fn worker_node_payload_staging_is_validated_and_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let mut node = WorkerNodeCfg::default();
        assert_eq!(
            node.payload_root(dir.path()).unwrap(),
            dir.path().join("worker_payloads")
        );
        node.payload_dir = Some("../escape".into());
        assert!(node.validate().is_err(), "traversal is refused");
        node.payload_dir = Some("staging/payloads".into());
        assert_eq!(
            node.payload_root(dir.path()).unwrap(),
            dir.path().join("staging/payloads")
        );
        assert!(node
            .payload_root(dir.path())
            .unwrap()
            .starts_with(dir.path()));
        node.payload_dir = Some("with\u{7}control".into());
        assert!(node.validate().is_err());
        node.payload_dir = Some("/var/lib/faktor/payloads".into());
        assert_eq!(
            node.payload_root(dir.path()).unwrap(),
            std::path::PathBuf::from("/var/lib/faktor/payloads")
        );
        node.token_payload = Some("worker.token".into());
        assert!(node.validate().is_ok());
        node.token_payload = Some("../worker.token".into());
        assert!(node.validate().is_err());
    }

    /// The additive `[updater]` section: disabled by default with BYTE-IDENTICAL
    /// serialization to an absent section, strict parsing on both load paths,
    /// bounded values, a mandatory non-empty operator allowlist when enabled,
    /// and no filesystem resolution while disabled.
    #[test]
    fn updater_section_is_disabled_by_default_and_strictly_parsed() {
        const KEY: &str = "PMIf08ao62O4xMR4upvk5ymt++8EcWRtWHWZGLa4TKo=";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("updater.json");

        // Absent == explicit disabled: no store, no install root, and
        // identical serialization (the disabled daemon is byte-identical).
        let absent = Config::default();
        assert!(!absent.updater.enabled);
        assert_eq!(absent.updater.database_path(dir.path()).unwrap(), None);
        assert_eq!(absent.updater.install_root_path(dir.path()).unwrap(), None);
        std::fs::write(&path, r#"{"model": "m"}"#).unwrap();
        let parsed_absent = Config::load_strict(&path).unwrap();
        std::fs::write(&path, r#"{"model": "m", "updater": {"enabled": false}}"#).unwrap();
        let parsed_disabled = Config::load_strict(&path).unwrap();
        assert_eq!(
            serde_json::to_value(&parsed_absent).unwrap(),
            serde_json::to_value(&parsed_disabled).unwrap(),
            "a disabled [updater] section must serialize exactly like an absent one"
        );
        assert_eq!(
            parsed_disabled.updater.database_path(dir.path()).unwrap(),
            None
        );
        assert_eq!(
            parsed_disabled
                .updater
                .install_root_path(dir.path())
                .unwrap(),
            None
        );

        // Enabled: keys are mandatory, the default paths resolve under the
        // data dir, and the channel parses.
        let enabled_json = format!(
            r#"{{"updater": {{"enabled": true, "channel": "beta", "keys": [{{"id": "op", "public_key": "{KEY}"}}]}}}}"#
        );
        std::fs::write(&path, &enabled_json).unwrap();
        let enabled = Config::load_strict(&path).unwrap();
        assert!(enabled.updater.enabled);
        assert!(
            !enabled.updater.allow_legacy_manifests_once_resolved(),
            "the one-time legacy allowance is off by default"
        );
        std::fs::write(
            &path,
            format!(
                r#"{{"updater": {{"enabled": true, "allow_legacy_manifests_once": true, "keys": [{{"id": "op", "public_key": "{KEY}"}}]}}}}"#
            ),
        )
        .unwrap();
        assert!(
            Config::load_strict(&path)
                .unwrap()
                .updater
                .allow_legacy_manifests_once_resolved(),
            "the documented one-time legacy allowance round-trips"
        );
        assert_eq!(
            enabled.updater.channel().unwrap(),
            faktor_updater::Channel::Beta
        );
        assert_eq!(
            enabled.updater.database_path(dir.path()).unwrap(),
            Some(dir.path().join("update.db"))
        );
        assert_eq!(
            enabled.updater.install_root_path(dir.path()).unwrap(),
            Some(dir.path().join("install"))
        );
        assert_eq!(enabled.updater.trusted_keys().unwrap().len(), 1);
        // Relative install roots resolve under the data dir; absolute ones
        // are honored as written.
        std::fs::write(
            &path,
            format!(
                r#"{{"updater": {{"enabled": true, "install_root": "releases/current", "keys": [{{"id": "op", "public_key": "{KEY}"}}]}}}}"#
            ),
        )
        .unwrap();
        assert_eq!(
            Config::load_strict(&path)
                .unwrap()
                .updater
                .install_root_path(dir.path())
                .unwrap(),
            Some(dir.path().join("releases").join("current"))
        );
        std::fs::write(
            &path,
            format!(
                r#"{{"updater": {{"enabled": true, "install_root": "/opt/faktor", "keys": [{{"id": "op", "public_key": "{KEY}"}}]}}}}"#
            ),
        )
        .unwrap();
        assert_eq!(
            Config::load_strict(&path)
                .unwrap()
                .updater
                .install_root_path(dir.path())
                .unwrap(),
            Some(std::path::PathBuf::from("/opt/faktor"))
        );

        // Shape errors are refused by BOTH load paths (unknown/duplicate
        // keys, wrong types, positional arrays, malformed key material).
        for bad in [
            r#"{"updater": {"enabled": "yes"}}"#,
            r#"{"updater": {"enabled": true, "bogus": 1}}"#,
            r#"{"updater": {"enabled": true, "enabled": false}}"#,
            r#"{"updater": {"channel": 1}}"#,
            r#"{"updater": {"allow_legacy_manifests_once": "yes"}}"#,
            r#"{"updater": {"allow_legacy_manifests_once": true, "allow_legacy_manifests_once": false}}"#,
            r#"{"updater": true}"#,
            r#"{"updater": ["enabled"]}"#,
            // Unknown key field / missing key field.
            format!(
                r#"{{"updater": {{"keys": [{{"id": "op", "public_key": "{KEY}", "extra": 1}}]}}}}"#
            )
            .as_str(),
            r#"{"updater": {"keys": [{"id": "op"}]}}"#,
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(
                Config::load(&path).is_err(),
                "hostile [updater] shape must fail: {bad}"
            );
            assert!(Config::load_strict(&path).is_err(), "{bad}");
        }
        // Semantic errors are refused by the strict path (the daemon never
        // boots on a section it cannot honor): enabled without an
        // allowlist, unknown channels, duplicate identities and hostile
        // bounds.
        for bad in [
            r#"{"updater": {"enabled": true}}"#,
            r#"{"updater": {"enabled": true, "keys": []}}"#,
            r#"{"updater": {"channel": "nightly"}}"#,
            // Not base64 / wrong raw length. (A 32-byte string that is not a
            // curve point is covered by the keys.rs unit test; dalek
            // accepts reduced non-canonical encodings, so no fixed byte
            // pattern can be asserted here.)
            r#"{"updater": {"keys": [{"id": "op", "public_key": "!!!"}]}}"#,
            r#"{"updater": {"keys": [{"id": "op", "public_key": "AAAA"}]}}"#,
            format!(
                r#"{{"updater": {{"keys": [{{"id": "op", "public_key": "{KEY}"}}, {{"id": "op", "public_key": "{KEY}"}}]}}}}"#
            )
            .as_str(),
            r#"{"updater": {"max_artifact_bytes": 0}}"#,
            r#"{"updater": {"max_artifact_bytes": 99999999999999}}"#,
            r#"{"updater": {"clock_skew_ms": -1}}"#,
            r#"{"updater": {"clock_skew_ms": 99999999}}"#,
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(
                Config::load_strict(&path).is_err(),
                "hostile [updater] value must fail: {bad}"
            );
        }
        // Hostile install roots are refused even while disabled (the file
        // never says two different things).
        for hostile in [
            r#"{"updater": {"enabled": true, "install_root": "../escape", "keys": [{"id": "op", "public_key": "PMIf08ao62O4xMR4upvk5ymt++8EcWRtWHWZGLa4TKo="}]}}"#,
            r#"{"updater": {"enabled": false, "install_root": "a/../../b"}}"#,
            r#"{"updater": {"enabled": false, "install_root": ""}}"#,
        ] {
            std::fs::write(&path, hostile).unwrap();
            assert!(
                Config::load_strict(&path).is_err(),
                "hostile updater install root must fail: {hostile}"
            );
        }
        // More than the allowlist cap.
        let many: Vec<String> = (0..(MAX_UPDATER_KEYS + 1))
            .map(|i| format!(r#"{{"id": "op-{i}", "public_key": "{KEY}"}}"#))
            .collect();
        std::fs::write(
            &path,
            format!(r#"{{"updater": {{"keys": [{}]}}}}"#, many.join(",")),
        )
        .unwrap();
        assert!(Config::load_strict(&path).is_err());
    }

    /// The additive `[billing]` section: disabled by default (byte-identical
    /// to an absent section), strictly parsed, plans/limits config-provided
    /// only, and refused when enabled without the `[cloud]` principal.
    #[test]
    fn billing_section_is_disabled_by_default_strict_and_plan_configured() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("billing.json");

        // Absent section == explicit disabled section.
        let absent = Config::default();
        assert!(!absent.billing.enabled);
        assert_eq!(absent.billing.billing_path(dir.path()).unwrap(), None);
        assert_eq!(absent.billing.service_config().unwrap(), None);
        std::fs::write(&path, r#"{"model": "m"}"#).unwrap();
        let parsed_absent = Config::load_strict(&path).unwrap();
        std::fs::write(&path, r#"{"model": "m", "billing": {"enabled": false}}"#).unwrap();
        let parsed_disabled = Config::load_strict(&path).unwrap();
        assert_eq!(
            serde_json::to_value(&parsed_absent).unwrap(),
            serde_json::to_value(&parsed_disabled).unwrap(),
            "a disabled [billing] section must serialize exactly like an absent one"
        );
        assert_eq!(
            parsed_disabled.billing.billing_path(dir.path()).unwrap(),
            None
        );
        // Even disabled, a hostile database name is refused.
        std::fs::write(
            &path,
            r#"{"billing": {"enabled": false, "database": "../x.db"}}"#,
        )
        .unwrap();
        assert!(Config::load_strict(&path).is_err());

        // Enabled requires the cloud section (the principal/tenant surface).
        std::fs::write(
            &path,
            r#"{"billing": {"enabled": true, "organization": "org_a", "plans": {"pro": {"plan_id": "pro"}}}}"#,
        )
        .unwrap();
        assert!(
            Config::load_strict(&path).is_err(),
            "billing without cloud has no organization principal"
        );

        // Enabled with cloud: the plan table is the ONLY source of features
        // and limits (no defaults, no prices in code).
        std::fs::write(
            &path,
            r#"{
                "cloud": {"enabled": true},
                "billing": {
                    "enabled": true,
                    "organization": "org_a",
                    "account": "acct_local",
                    "managed_providers": ["managed-provider"],
                    "default_plan": "pro",
                    "plans": {
                        "pro": {
                            "plan_id": "pro",
                            "features": ["managed_providers", "byok"],
                            "limits": {"max_active_tasks": 2, "max_managed_spend_micro_per_period": 1000000}
                        }
                    }
                }
            }"#,
        )
        .unwrap();
        let enabled = Config::load_strict(&path).unwrap();
        assert_eq!(
            enabled.billing.billing_path(dir.path()).unwrap(),
            Some(dir.path().join("billing.db")),
            "the default billing database resolves inside the data dir"
        );
        let service_config = enabled.billing.service_config().unwrap().unwrap();
        assert_eq!(
            service_config.category_of("managed-provider"),
            faktor_cloud::SpendCategory::Managed
        );
        assert_eq!(
            service_config.category_of("other"),
            faktor_cloud::SpendCategory::Byok
        );
        assert_eq!(
            service_config.plan("pro").and_then(|plan| plan
                .limits
                .get(faktor_cloud::LIMIT_MAX_ACTIVE_TASKS)
                .copied()),
            Some(2)
        );
        assert_eq!(enabled.billing.organization().unwrap().as_str(), "org_a");

        // Strict parsing: unknown/duplicate keys, wrong types, unknown
        // features/limits and an empty plan table are refused.
        for bad in [
            r#"{"billing": true}"#,
            r#"{"billing": {"enabled": "yes"}}"#,
            r#"{"billing": {"enabled": true, "bogus": 1}}"#,
            r#"{"billing": {"enabled": true, "enabled": false}}"#,
            r#"{"billing": {"database": 1}}"#,
            r#"{"billing": {"plans": []}}"#,
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(
                Config::load(&path).is_err() && Config::load_strict(&path).is_err(),
                "hostile [billing] must fail: {bad}"
            );
        }
        for bad in [
            // Unknown feature tag.
            r#"{"cloud": {"enabled": true}, "billing": {"enabled": true, "organization": "org_a", "plans": {"pro": {"plan_id": "pro", "features": ["gold"]}}}}"#,
            // Unknown limit name.
            r#"{"cloud": {"enabled": true}, "billing": {"enabled": true, "organization": "org_a", "plans": {"pro": {"plan_id": "pro", "limits": {"price": 1}}}}}"#,
            // Plan key != plan_id.
            r#"{"cloud": {"enabled": true}, "billing": {"enabled": true, "organization": "org_a", "plans": {"pro": {"plan_id": "team"}}}}"#,
            // Default plan outside the table.
            r#"{"cloud": {"enabled": true}, "billing": {"enabled": true, "organization": "org_a", "default_plan": "nope", "plans": {"pro": {"plan_id": "pro"}}}}"#,
            // Empty plan table while enabled.
            r#"{"cloud": {"enabled": true}, "billing": {"enabled": true, "organization": "org_a", "plans": {}}}"#,
            // Missing organization while enabled.
            r#"{"cloud": {"enabled": true}, "billing": {"enabled": true, "plans": {"pro": {"plan_id": "pro"}}}}"#,
            // A price-shaped plan field is NOT part of the contract.
            r#"{"cloud": {"enabled": true}, "billing": {"enabled": true, "organization": "org_a", "plans": {"pro": {"plan_id": "pro", "price_per_token": 1}}}}"#,
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(
                Config::load_strict(&path).is_err(),
                "the config must refuse: {bad}"
            );
        }
    }

    #[test]
    fn workers_section_is_additive_strict_and_disabled_by_default() {
        // Absent = disabled, no database, no placement seam.
        let cfg = Config::default();
        assert!(!cfg.workers.enabled);
        assert!(cfg
            .workers
            .workers_path(std::path::Path::new("/tmp"))
            .unwrap()
            .is_none());
        // Strict shape: unknown keys and duplicates are parse errors.
        for bad in [
            r#"{"model": "m", "workers": {"enabled": false, "hostile": 1}}"#,
            r#"{"model": "m", "workers": {"enabled": true, "enabled": true}}"#,
            r#"{"model": "m", "workers": {"enabled": "yes"}}"#,
            r#"{"model": "m", "workers": {"min_cpu_cores": -1}}"#,
        ] {
            assert!(
                serde_json::from_str::<Config>(bad).is_err(),
                "the config must refuse: {bad}"
            );
        }
        // A disabled section still validates its database name shape, but
        // creates no file.
        let disabled: Config = serde_json::from_str(
            r#"{"model": "m", "workers": {"enabled": false, "database": "wp.db"}}"#,
        )
        .unwrap();
        assert!(disabled
            .workers
            .workers_path(std::path::Path::new("/tmp"))
            .unwrap()
            .is_none());
        let traversal: Config =
            serde_json::from_str(r#"{"model": "m", "workers": {"database": "../escape.db"}}"#)
                .unwrap();
        assert!(traversal.validate().is_err(), "path traversal is refused");
    }

    #[test]
    fn worker_plane_section_is_strict_and_enforces_the_deployment_boundary() {
        // Absent/disabled = no second socket, nothing to resolve.
        let cfg = Config::default();
        assert!(!cfg.worker_plane.enabled);
        assert!(cfg.worker_plane.resolve().unwrap().is_none());
        // Strict shape: unknown keys, duplicates and wrong types are parse
        // errors.
        for bad in [
            r#"{"model": "m", "worker_plane": {"enabled": false, "hostile": 1}}"#,
            r#"{"model": "m", "worker_plane": {"enabled": true, "enabled": true}}"#,
            r#"{"model": "m", "worker_plane": {"enabled": "yes"}}"#,
            r#"{"model": "m", "worker_plane": {"tls": "yes"}}"#,
        ] {
            assert!(
                serde_json::from_str::<Config>(bad).is_err(),
                "the config must refuse: {bad}"
            );
        }
        // Bind/auth shapes are errors even while the section is disabled.
        for bad in [
            r#"{"model": "m", "worker_plane": {"bind": "not-a-socket"}}"#,
            r#"{"model": "m", "worker_plane": {"bind": "127.0.0.1:99999"}}"#,
            r#"{"model": "m", "worker_plane": {"auth": "hostile"}}"#,
        ] {
            let cfg: Config = serde_json::from_str(bad).unwrap();
            assert!(cfg.validate().is_err(), "the config must refuse: {bad}");
        }
        // An enabled boundary without the [workers] plane never boots.
        let cfg: Config =
            serde_json::from_str(r#"{"model": "m", "worker_plane": {"enabled": true}}"#).unwrap();
        let error = cfg.validate().unwrap_err();
        assert!(error.contains("requires [workers] enabled"), "{error}");

        let base = |extra: &str| {
            format!(
                r#"{{"model": "m", "cloud": {{"enabled": true}}, "workers": {{"enabled": true, "organization": "org_local"}}, "worker_plane": {{"enabled": true{extra}}}}}"#
            )
        };

        // The default bind is loopback: allowed with no acknowledgement.
        let cfg: Config = serde_json::from_str(&base("")).unwrap();
        cfg.validate().unwrap();
        let resolved = cfg.worker_plane.resolve().unwrap().unwrap();
        assert!(resolved.bind.ip().is_loopback());
        assert_eq!(resolved.bind.port(), 8790);
        assert!(!resolved.trusted_gateway);

        // A non-loopback bind without TLS or the gateway acknowledgement is
        // the typed startup refusal NAMING the deployment boundary.
        let cfg: Config = serde_json::from_str(&base(r#", "bind": "0.0.0.0:8790""#)).unwrap();
        let error = cfg.validate().unwrap_err();
        assert!(
            error.contains("worker-plane deployment boundary"),
            "the refusal names the boundary: {error}"
        );
        assert!(
            error.contains("worker_plane_boundary_refused"),
            "the refusal carries its stable code: {error}"
        );

        // Gateway mode admits non-loopback and the acknowledgement is
        // visible in the resolved exposure/audit line.
        let cfg: Config = serde_json::from_str(&base(
            r#", "bind": "0.0.0.0:8790", "trusted_gateway": true"#,
        ))
        .unwrap();
        cfg.validate().unwrap();
        let exposure = cfg
            .worker_plane
            .resolve()
            .unwrap()
            .unwrap()
            .validate()
            .unwrap();
        assert!(exposure.beyond_loopback && exposure.trusted_gateway);
        assert!(exposure.audit_line().contains("trusted_gateway=true"));

        // In-process TLS is refused typed: this build has no inbound TLS
        // stack, so the gateway-only mode is the honest path.
        let cfg: Config = serde_json::from_str(&base(r#", "tls": true"#)).unwrap();
        let error = cfg.validate().unwrap_err();
        assert!(error.contains("no inbound TLS stack"), "{error}");
        assert!(
            error.contains("worker-plane deployment boundary"),
            "{error}"
        );

        // gateway_mtls requires the acknowledgement AND the gateway bearer.
        let cfg: Config = serde_json::from_str(&base(r#", "auth": "gateway_mtls""#)).unwrap();
        assert!(cfg.validate().unwrap_err().contains("gateway_mtls"));
        let cfg: Config = serde_json::from_str(&base(
            r#", "auth": "gateway_mtls", "trusted_gateway": true"#,
        ))
        .unwrap();
        assert!(cfg.validate().unwrap_err().contains("bearer"));
        let cfg: Config = serde_json::from_str(&base(
            r#", "auth": "gateway_mtls", "trusted_gateway": true, "bearer": "gw-secret""#,
        ))
        .unwrap();
        cfg.validate().unwrap();
        let resolved = cfg.worker_plane.resolve().unwrap().unwrap();
        assert_eq!(resolved.auth, faktor_server::WorkerPlaneAuth::GatewayMtls);
        assert_eq!(resolved.bearer.as_deref(), Some("gw-secret"));
    }

    #[test]
    fn worker_node_section_is_additive_strict_and_disabled_by_default() {
        // Absent = disabled: the entry refuses before any effect.
        let cfg = Config::default();
        assert!(!cfg.worker_node.enabled);
        assert!(cfg.worker_node.validate().is_ok());
        // Strict shape: unknown keys, duplicates and wrong types are parse
        // errors; a disabled section imposes nothing else.
        for bad in [
            r#"{"model": "m", "worker_node": {"enabled": false, "hostile": 1}}"#,
            r#"{"model": "m", "worker_node": {"enabled": true, "enabled": true}}"#,
            r#"{"model": "m", "worker_node": {"enabled": "yes"}}"#,
            r#"{"model": "m", "worker_node": {"cpu_cores": -1}}"#,
        ] {
            assert!(
                serde_json::from_str::<Config>(bad).is_err(),
                "the config must refuse: {bad}"
            );
        }
        // An enabled section requires identity, credential, base URL and
        // trust domain; token XOR token_file is enforced at validation.
        let mut node = WorkerNodeCfg {
            enabled: true,
            worker_id: Some("wrk_local".into()),
            control_plane_url: Some("http://127.0.0.1:8787/".into()),
            trust_domain: Some("org_local".into()),
            ..Default::default()
        };
        assert!(
            node.validate().is_err(),
            "an enabled node without a credential is refused"
        );
        node.token = Some("wkr_abc".into());
        node.validate().expect("a complete node validates");
        assert_eq!(
            node.control_plane_url().unwrap(),
            "http://127.0.0.1:8787",
            "the trailing slash is normalized away"
        );
        node.token_payload = Some("/tmp/token".into());
        assert!(
            node.validate().is_err(),
            "an absolute path is not a payload name (and token XOR token_payload is exclusive)"
        );
        node.token = None;
        assert!(
            node.validate().is_err(),
            "token_payload must be a plain payload name"
        );
        node.token_payload = Some("worker.token".into());
        node.validate().expect("a staged payload name validates");
        // The advertisement is normalized and protocol-pinned.
        let capabilities = node.capabilities().unwrap();
        assert_eq!(capabilities.trust_domain, "org_local");
        assert_eq!(
            capabilities.protocol_version,
            faktor_worker::WORKER_PROTOCOL_VERSION
        );
        assert!(
            capabilities.toolchains.is_empty(),
            "an empty advertisement claims no toolchain"
        );
    }

    #[test]
    fn workers_enabled_requires_cloud_org_and_maps_requirements() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = WorkersCfg {
            enabled: true,
            organization: Some("org_local".into()),
            ..Default::default()
        };
        assert_eq!(
            w.trust_domain().unwrap(),
            "org_local",
            "default = organization"
        );
        w.trust_domain = Some("Eu-1".into());
        assert_eq!(w.trust_domain().unwrap(), "eu-1", "normalized");
        w.toolchains = vec!["Rust".into(), "rust".into(), "Node".into()];
        w.network = Some("full".into());
        w.min_cpu_cores = 4;
        let req = w.requirements().unwrap();
        assert_eq!(req.toolchains, vec!["node", "rust"], "normalized + deduped");
        assert_eq!(req.network, Some(faktor_worker::NetworkProfile::Full));
        assert_eq!(req.trust_domain, "eu-1");
        w.network = Some("hostile".into());
        assert!(w.requirements().is_err(), "unknown network profile refused");
        w.network = None;
        assert!(w.workers_path(dir.path()).unwrap().is_some());
        // An enabled section without [cloud] never boots.
        let cfg: Config = serde_json::from_str(
            r#"{"model": "m", "workers": {"enabled": true, "organization": "org_local"}}"#,
        )
        .unwrap();
        assert!(cfg.validate().is_err());
        // With [cloud] enabled the pair validates.
        let cfg: Config = serde_json::from_str(
            r#"{"model": "m", "cloud": {"enabled": true}, "workers": {"enabled": true, "organization": "org_local"}}"#,
        )
        .unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn enterprise_section_is_additive_strict_and_disabled_by_default() {
        // Absent = disabled, no database path, no layers.
        let cfg = Config::default();
        assert!(!cfg.enterprise.enabled);
        assert!(cfg
            .enterprise
            .enterprise_path(std::path::Path::new("/tmp"))
            .unwrap()
            .is_none());
        assert!(cfg.enterprise.organization().unwrap().is_none());
        assert!(cfg.enterprise.layers().unwrap().is_empty());

        // Strict shape: unknown keys, duplicates and wrong types are parse
        // errors on both load paths.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e.json");
        for bad in [
            r#"{"model": "m", "enterprise": {"enabled": false, "hostile": 1}}"#,
            r#"{"model": "m", "enterprise": {"enabled": true, "enabled": true}}"#,
            r#"{"model": "m", "enterprise": {"enabled": "yes"}}"#,
            r#"{"model": "m", "enterprise": {"policy": {"hostile": ["x"]}}}"#,
            r#"{"model": "m", "enterprise": {"policy": {"network": "none"}}}"#,
            r#"{"model": "m", "enterprise": {"preferences": {"network": ["none"]}}}"#,
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(
                Config::load(&path).is_err(),
                "the config must refuse: {bad}"
            );
            assert!(
                Config::load_strict(&path).is_err(),
                "the strict load must refuse: {bad}"
            );
        }

        // Path traversal in the database name is refused even while
        // disabled; an enabled section without [cloud] never boots.
        let cfg: Config =
            serde_json::from_str(r#"{"model": "m", "enterprise": {"database": "../e.db"}}"#)
                .unwrap();
        assert!(cfg.validate().is_err());
        let cfg: Config = serde_json::from_str(
            r#"{"model": "m", "enterprise": {"enabled": true, "organization": "org_local"}}"#,
        )
        .unwrap();
        assert!(cfg.validate().is_err(), "enterprise requires [cloud]");
        let cfg: Config = serde_json::from_str(
            r#"{"model": "m", "cloud": {"enabled": true}, "enterprise": {"enabled": true, "organization": "org_local"}}"#,
        )
        .unwrap();
        cfg.validate().unwrap();
        let resolved = cfg.enterprise.enterprise_path(dir.path()).unwrap().unwrap();
        assert!(resolved.ends_with("enterprise.db"));
    }

    #[test]
    fn enterprise_layers_are_policy_vs_preference_and_loosening_is_refused() {
        // A preference outside the configured policy is refused by the ONE
        // resolver (the same one the server route uses).
        let cfg: Config = serde_json::from_str(
            r#"{
                "model": "m",
                "cloud": {"enabled": true},
                "enterprise": {
                    "enabled": true,
                    "organization": "org_local",
                    "policy": {"network": ["none"]},
                    "preferences": {"network": "provider"}
                }
            }"#,
        )
        .unwrap();
        cfg.validate().unwrap();
        let layers = cfg.enterprise.layers().unwrap();
        let error = faktor_cloud::resolve_layers(&layers).unwrap_err();
        assert!(
            matches!(
                error,
                faktor_cloud::LayeredConfigError::PreferenceRefusedByPolicy { .. }
            ),
            "{error}"
        );

        // A satisfied preference resolves with an attributable digest; the
        // digest changes when any layer changes.
        let cfg: Config = serde_json::from_str(
            r#"{
                "model": "m",
                "cloud": {"enabled": true},
                "enterprise": {
                    "enabled": true,
                    "organization": "org_local",
                    "policy": {"network": ["none", "provider"], "providers": ["anthropic"]},
                    "preferences": {"network": "none", "providers": "anthropic"}
                }
            }"#,
        )
        .unwrap();
        let effective = faktor_cloud::resolve_layers(&cfg.enterprise.layers().unwrap()).unwrap();
        assert!(effective.policy_allows(faktor_cloud::ConfigKey::Network, "none"));
        let digest = effective.digest.clone();
        let mut changed = cfg.enterprise.clone();
        changed.policy.providers = Some(vec!["anthropic".into(), "openai".into()]);
        let changed = faktor_cloud::resolve_layers(&changed.layers().unwrap()).unwrap();
        assert_ne!(changed.digest, digest);
    }
}

/// `[commerce]` config tests (docs/acquire.md §13): strictness, disabled
/// parity, env-var-name-only credentials and Debug redaction.
#[cfg(test)]
mod commerce_config_tests {
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
}
