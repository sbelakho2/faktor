//! BK01 configuration authority: typed descriptors, one versioned effective
//! snapshot, transactional atomic patches against an expected revision,
//! per-field apply state and linear-history rollback.
//!
//! Unknown keys fail; scopes are allowlisted per descriptor; secret slots
//! accept only opaque references. Nothing is persisted until the WHOLE patch
//! validates, and the revision comparison happens AGAIN inside the write
//! transaction so a lost update can never overwrite another editor's change.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::{now_ms, FoundationDb};
use crate::{hash_hex, FoundationError};

/// Maximum settings returned by one search page (bounded search).
pub const MAX_SEARCH_LIMIT: usize = 200;
/// Maximum changes in one atomic patch.
pub const MAX_PATCH_CHANGES: usize = 2_000;
/// Maximum serialized size of one setting value.
pub const MAX_SETTING_VALUE_BYTES: usize = 256 * 1024;
/// Maximum serialized size of one imported bundle.
pub const MAX_IMPORT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    User,
    Project,
    Machine,
    Managed,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Project => "project",
            Scope::Machine => "machine",
            Scope::Managed => "managed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "user" => Scope::User,
            "project" => Scope::Project,
            "machine" => Scope::Machine,
            "managed" => Scope::Managed,
            _ => return None,
        })
    }
}

/// When a saved value becomes live. The service never advances
/// `runtime_revision` for a timing it cannot acknowledge synchronously.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyTiming {
    Effective,
    NextCall,
    NextTask,
    NewSession,
    RestartRequired,
}

impl ApplyTiming {
    pub fn as_str(self) -> &'static str {
        match self {
            ApplyTiming::Effective => "effective",
            ApplyTiming::NextCall => "next_call",
            ApplyTiming::NextTask => "next_task",
            ApplyTiming::NewSession => "new_session",
            ApplyTiming::RestartRequired => "restart_required",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "effective" => ApplyTiming::Effective,
            "next_call" => ApplyTiming::NextCall,
            "next_task" => ApplyTiming::NextTask,
            "new_session" => ApplyTiming::NewSession,
            "restart_required" => ApplyTiming::RestartRequired,
            _ => return None,
        })
    }
}

/// Per-field apply state served by a commit receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ApplyState {
    Effective,
    NextCall,
    NextTask,
    NewSession,
    RestartRequired,
    Failed { message: String },
}

impl ApplyState {
    pub fn for_timing(timing: ApplyTiming) -> Self {
        match timing {
            ApplyTiming::Effective => ApplyState::Effective,
            ApplyTiming::NextCall => ApplyState::NextCall,
            ApplyTiming::NextTask => ApplyState::NextTask,
            ApplyTiming::NewSession => ApplyState::NewSession,
            ApplyTiming::RestartRequired => ApplyState::RestartRequired,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ApplyState::Effective => "effective",
            ApplyState::NextCall => "next_call",
            ApplyState::NextTask => "next_task",
            ApplyState::NewSession => "new_session",
            ApplyState::RestartRequired => "restart_required",
            ApplyState::Failed { .. } => "failed",
        }
    }
}

/// Validation/type shape of one setting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ValueKind {
    Bool,
    Integer {
        min: i64,
        max: i64,
    },
    Number {
        min: f64,
        max: f64,
    },
    Text {
        max_bytes: usize,
    },
    Enum {
        choices: Vec<String>,
    },
    StringList {
        max_items: usize,
        max_item_bytes: usize,
    },
    SecretReference,
    Json,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Descriptor {
    pub key: String,
    pub label: String,
    pub description: String,
    pub category: String,
    pub kind: ValueKind,
    pub default: Value,
    pub allowed_scopes: Vec<Scope>,
    pub writable: bool,
    pub apply_timing: ApplyTiming,
    pub secret_reference_only: bool,
    /// Keys that conflict with this one (alias migration): a patch may not
    /// set both members of one group.
    #[serde(default)]
    pub alias_group: Option<String>,
}

impl Descriptor {
    fn new(
        key: &str,
        label: &str,
        description: &str,
        category: &str,
        kind: ValueKind,
        default: Value,
        timing: ApplyTiming,
    ) -> Self {
        Self {
            key: key.to_string(),
            label: label.to_string(),
            description: description.to_string(),
            category: category.to_string(),
            kind,
            default,
            allowed_scopes: vec![Scope::User, Scope::Project, Scope::Machine],
            writable: true,
            apply_timing: timing,
            secret_reference_only: false,
            alias_group: None,
        }
    }

    fn secret(mut self) -> Self {
        self.secret_reference_only = true;
        self.kind = ValueKind::SecretReference;
        self
    }

    fn locked(mut self) -> Self {
        self.writable = false;
        self.allowed_scopes = Vec::new();
        self
    }

    fn alias(mut self, group: &str) -> Self {
        self.alias_group = Some(group.to_string());
        self
    }
}

/// Rev8 selectable descriptors (C-08/B-09/K-02). Kept as one focused helper
/// so the default descriptor table stays under the function-size ceiling.
fn rev8_descriptors() -> Vec<Descriptor> {
    let mut out = Vec::new();
    let mut add = |descriptor: Descriptor| out.push(descriptor);
    add(Descriptor::new(
        "faktor.context.ignorePatterns",
        "Context ignore patterns",
        "Search/retrieval exclusions (workspace-relative globs). These are NOT security denies; an authorized shell can still read a matching file.",
        "context",
        ValueKind::StringList {
            max_items: 128,
            max_item_bytes: 512,
        },
        Value::Array(Vec::new()),
        ApplyTiming::NextTask,
    ));
    add(Descriptor::new(
        "faktor.context.protectedPatterns",
        "Context protected patterns",
        "Workspace-relative globs that ignore rules can never exclude (security-relevant files). A protected match wins over an ignore match.",
        "context",
        ValueKind::StringList {
            max_items: 128,
            max_item_bytes: 512,
        },
        Value::Array(Vec::new()),
        ApplyTiming::NextTask,
    ));
    add(Descriptor::new(
        "faktor.agent.maxTurns",
        "Maximum turns",
        "Served per-task turn bound applied at task admission. Unset keeps the daemon default bound.",
        "agent",
        ValueKind::Integer {
            min: 1,
            max: 1_000_000,
        },
        Value::Null,
        ApplyTiming::NextTask,
    ));
    add(Descriptor::new(
        "faktor.agent.maxConsecutiveFailures",
        "Maximum consecutive failures",
        "Served loop guard for consecutive no-progress iterations (unset keeps the daemon default bound).",
        "agent",
        ValueKind::Integer {
            min: 2,
            max: 1_000,
        },
        Value::Null,
        ApplyTiming::NextTask,
    ));
    add(Descriptor::new(
        "faktor.compaction.model",
        "Compaction model",
        "Served provider/model used for compaction summaries; unset inherits the session model. Applied at the next daemon restart (the compaction authority is built at boot).",
        "context",
        ValueKind::Text { max_bytes: 256 },
        Value::String(String::new()),
        ApplyTiming::RestartRequired,
    ));
    out
}

/// The built-in descriptor table. Every key a settings screen may show is
/// declared here; unknown keys are a loud validation error.
pub fn default_descriptors() -> BTreeMap<String, Descriptor> {
    let mut m = BTreeMap::new();
    let mut add = |d: Descriptor| {
        m.insert(d.key.clone(), d);
    };
    add(Descriptor::new(
        "faktor.defaultProvider",
        "Default provider",
        "Served provider instance used for new conversations.",
        "models",
        ValueKind::Text { max_bytes: 256 },
        Value::String(String::new()),
        ApplyTiming::NextTask,
    ));
    add(Descriptor::new(
        "faktor.defaultModel",
        "Default model",
        "Model used for new conversations; validated together with the provider.",
        "models",
        ValueKind::Text { max_bytes: 256 },
        Value::String(String::new()),
        ApplyTiming::NextTask,
    ));
    add(Descriptor::new(
        "faktor.run.defaultProvider",
        "Run default provider",
        "Provider used when a task start does not name one.",
        "models",
        ValueKind::Text { max_bytes: 256 },
        Value::String(String::new()),
        ApplyTiming::NextTask,
    ));
    add(Descriptor::new(
        "faktor.run.defaultModel",
        "Run default model",
        "Model used when a task start does not name one.",
        "models",
        ValueKind::Text { max_bytes: 256 },
        Value::String(String::new()),
        ApplyTiming::NextTask,
    ));
    add(Descriptor::new(
        "faktor.model.contextLimit",
        "Context limit override",
        "Positive context-window override bounded by the model capability.",
        "models",
        ValueKind::Integer {
            min: 1_024,
            max: 10_000_000,
        },
        Value::Null,
        ApplyTiming::NextTask,
    ));
    add(Descriptor::new(
        "faktor.generation.temperature",
        "Temperature",
        "Sampling temperature; unset inherits the adapter default (unset is not zero).",
        "generation",
        ValueKind::Number { min: 0.0, max: 2.0 },
        Value::Null,
        ApplyTiming::NextCall,
    ));
    add(Descriptor::new(
        "faktor.generation.topP",
        "Top-p",
        "Nucleus sampling probability.",
        "generation",
        ValueKind::Number { min: 0.0, max: 1.0 },
        Value::Null,
        ApplyTiming::NextCall,
    ));
    add(Descriptor::new(
        "faktor.generation.maxOutput",
        "Maximum output",
        "Positive output-token limit bounded per model.",
        "generation",
        ValueKind::Integer {
            min: 1,
            max: 1_000_000,
        },
        Value::Null,
        ApplyTiming::NextCall,
    ));
    add(Descriptor::new(
        "faktor.budget.maxCostMicro",
        "Cost cap (microUSD)",
        "Exact non-negative integer microUSD cap; unset means no cap.",
        "budgets",
        ValueKind::Integer {
            min: 0,
            max: i64::MAX,
        },
        Value::Null,
        ApplyTiming::NextTask,
    ));
    add(Descriptor::new(
        "faktor.budget.maxTokens",
        "Token cap",
        "Exact non-negative integer token cap; unset means no cap.",
        "budgets",
        ValueKind::Integer {
            min: 0,
            max: i64::MAX,
        },
        Value::Null,
        ApplyTiming::NextTask,
    ));
    add(Descriptor::new(
        "faktor.budget.warnAtPercent",
        "Budget warning threshold",
        "Warning threshold in whole percent of the captured cap.",
        "budgets",
        ValueKind::Integer { min: 1, max: 100 },
        Value::Number(80.into()),
        ApplyTiming::NextTask,
    ));
    add(Descriptor::new(
        "faktor.retry.maxAttempts",
        "Retry attempts",
        "Total attempts, including the first (1..=8).",
        "reliability",
        ValueKind::Integer { min: 1, max: 8 },
        Value::Number(3.into()),
        ApplyTiming::RestartRequired,
    ));
    add(Descriptor::new(
        "faktor.retry.baseDelayMs",
        "Retry base delay",
        "Base backoff delay in milliseconds.",
        "reliability",
        ValueKind::Integer {
            min: 0,
            max: 300_000,
        },
        Value::Number(500.into()),
        ApplyTiming::RestartRequired,
    ));
    add(Descriptor::new(
        "faktor.retry.maxDelayMs",
        "Retry maximum delay",
        "Maximum backoff delay in milliseconds.",
        "reliability",
        ValueKind::Integer {
            min: 0,
            max: 3_600_000,
        },
        Value::Number(30_000.into()),
        ApplyTiming::RestartRequired,
    ));
    add(Descriptor::new(
        "faktor.retry.jitter",
        "Retry jitter",
        "Jitter fraction in [0, 1].",
        "reliability",
        ValueKind::Number { min: 0.0, max: 1.0 },
        Value::Number(serde_json::Number::from_f64(0.2).expect("finite")),
        ApplyTiming::RestartRequired,
    ));
    add(Descriptor::new(
        "faktor.efficiency.failureLearning",
        "Failure learning",
        "Learn failure priors from verified outcomes.",
        "efficiency",
        ValueKind::Bool,
        Value::Bool(true),
        ApplyTiming::RestartRequired,
    ));
    add(Descriptor::new(
        "faktor.efficiency.ccr",
        "Context compression",
        "Compress retried model calls with cached context.",
        "efficiency",
        ValueKind::Bool,
        Value::Bool(true),
        ApplyTiming::RestartRequired,
    ));
    add(Descriptor::new(
        "faktor.efficiency.typedHandoff",
        "Typed handoff",
        "Use typed child handoff envelopes.",
        "efficiency",
        ValueKind::Bool,
        Value::Bool(true),
        ApplyTiming::RestartRequired,
    ));
    add(Descriptor::new(
        "faktor.efficiency.semanticContext",
        "Semantic context",
        "Use semantic retrieval when an embedder is served; lexical/symbol fallback otherwise.",
        "efficiency",
        ValueKind::Bool,
        Value::Bool(true),
        ApplyTiming::RestartRequired,
    )
    .alias("semantic-context"));
    add(Descriptor::new(
        "faktor.efficiency.informationGain",
        "Information gain",
        "Deprecated alias of semantic context; a patch may not set both.",
        "efficiency",
        ValueKind::Bool,
        Value::Bool(true),
        ApplyTiming::RestartRequired,
    )
    .alias("semantic-context"));
    add(Descriptor::new(
        "faktor.efficiency.reworkGuard",
        "Rework guard",
        "Detect and refuse repeated rework loops.",
        "efficiency",
        ValueKind::Bool,
        Value::Bool(true),
        ApplyTiming::RestartRequired,
    ));
    add(Descriptor::new(
        "faktor.retrieval.strategy",
        "Retrieval strategy",
        "lexical | symbol | semantic | hybrid (semantic requires a served embedder).",
        "efficiency",
        ValueKind::Enum {
            choices: ["lexical", "symbol", "semantic", "hybrid"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        },
        Value::String("hybrid".into()),
        ApplyTiming::RestartRequired,
    ));
    add(Descriptor::new(
        "faktor.retrieval.embeddingProvider",
        "Embedding provider",
        "Served provider instance used for embeddings; unset keeps lexical/symbol retrieval.",
        "efficiency",
        ValueKind::Text { max_bytes: 256 },
        Value::String(String::new()),
        ApplyTiming::RestartRequired,
    ));
    add(Descriptor::new(
        "faktor.retrieval.embeddingModel",
        "Embedding model",
        "Served embedding model; unset keeps lexical/symbol retrieval.",
        "efficiency",
        ValueKind::Text { max_bytes: 256 },
        Value::String(String::new()),
        ApplyTiming::RestartRequired,
    ));
    add(Descriptor::new(
        "faktor.context.compactionMinReduction",
        "Compaction minimum reduction",
        "Fraction in (0, 1); a compaction below it is rejected.",
        "context",
        ValueKind::Number {
            min: 0.01,
            max: 0.99,
        },
        Value::Number(serde_json::Number::from_f64(0.25).expect("finite")),
        ApplyTiming::NextTask,
    ));
    add(Descriptor::new(
        "faktor.context.outputReserve",
        "Output reserve",
        "Tokens reserved for output; never counted as consumed input.",
        "context",
        ValueKind::Integer {
            min: 0,
            max: 1_000_000,
        },
        Value::Number(4_096.into()),
        ApplyTiming::NextTask,
    ));
    add(Descriptor::new(
        "faktor.context.safetyReserve",
        "Safety reserve",
        "Tokens reserved for safety; never counted as consumed input.",
        "context",
        ValueKind::Integer {
            min: 0,
            max: 1_000_000,
        },
        Value::Number(2_048.into()),
        ApplyTiming::NextTask,
    ));
    for descriptor in rev8_descriptors() {
        add(descriptor);
    }
    add(Descriptor::new(
        "faktor.permissions.preset",
        "Permission preset",
        "ask | plan | code | full_auto (managed denies always win).",
        "permissions",
        ValueKind::Enum {
            choices: ["ask", "plan", "code", "full_auto"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        },
        Value::String("ask".into()),
        ApplyTiming::NextTask,
    ));
    add(Descriptor::new(
        "faktor.permissions.allowedTools",
        "Allowed tools",
        "Bounded tool-name allowlist for custom presets.",
        "permissions",
        ValueKind::StringList {
            max_items: 256,
            max_item_bytes: 128,
        },
        Value::Array(Vec::new()),
        ApplyTiming::NextTask,
    ));
    add(Descriptor::new(
        "faktor.permissions.childRoles",
        "Child roles",
        "Bounded roles a custom preset may delegate.",
        "permissions",
        ValueKind::StringList {
            max_items: 64,
            max_item_bytes: 128,
        },
        Value::Array(Vec::new()),
        ApplyTiming::NextTask,
    ));
    add(Descriptor::new(
        "faktor.permissions.maximumChildren",
        "Maximum children",
        "Per-task child concurrency bound.",
        "permissions",
        ValueKind::Integer { min: 0, max: 64 },
        Value::Number(4.into()),
        ApplyTiming::NextTask,
    ));
    add(Descriptor::new(
        "faktor.sandbox.shell",
        "Shell mode",
        "Configured shell execution mode (requires restart to change).",
        "sandbox",
        ValueKind::Text { max_bytes: 128 },
        Value::String("os_isolated".into()),
        ApplyTiming::RestartRequired,
    ));
    add(Descriptor::new(
        "faktor.sandbox.network",
        "Network mode",
        "Configured network policy (requires restart to change).",
        "sandbox",
        ValueKind::Text { max_bytes: 128 },
        Value::String("deny".into()),
        ApplyTiming::RestartRequired,
    ));
    add(Descriptor::new(
        "faktor.sandbox.allowLoopback",
        "Allow loopback",
        "Explicit loopback opt-in for a configured local endpoint; never a LAN bypass.",
        "sandbox",
        ValueKind::Bool,
        Value::Bool(false),
        ApplyTiming::RestartRequired,
    ));
    add(Descriptor::new(
        "faktor.sessions.maxParallel",
        "Maximum parallel sessions",
        "Bounded number of listed parallel conversations; a larger listing is truncated with truncated:true.",
        "sessions",
        ValueKind::Integer { min: 1, max: 1_000 },
        Value::Number(16.into()),
        ApplyTiming::Effective,
    ));
    add(Descriptor::new(
        "faktor.memory.captureMode",
        "Memory capture",
        "off | manual | auto — bounded memory capture policy.",
        "memory",
        ValueKind::Enum {
            choices: ["off", "manual", "auto"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        },
        Value::String("manual".into()),
        ApplyTiming::RestartRequired,
    ));
    add(Descriptor::new(
        "faktor.memory.retentionDays",
        "Memory retention",
        "Retention window in days; 0 means retain until explicit deletion.",
        "memory",
        ValueKind::Integer { min: 0, max: 3_650 },
        Value::Number(0.into()),
        ApplyTiming::NextTask,
    ));
    add(Descriptor::new(
        "faktor.diagnostics.logLevel",
        "Log level",
        "error | warn | info | debug | trace.",
        "diagnostics",
        ValueKind::Enum {
            choices: ["error", "warn", "info", "debug", "trace"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        },
        Value::String("info".into()),
        ApplyTiming::RestartRequired,
    ));
    add(Descriptor::new(
        "faktor.diagnostics.exportMaxBytes",
        "Diagnostic export bound",
        "Byte cap of one redacted diagnostic bundle (4 KiB..1 MiB).",
        "diagnostics",
        ValueKind::Integer {
            min: 4_096,
            max: 1_048_576,
        },
        Value::Number(262_144.into()),
        ApplyTiming::Effective,
    ));
    add(Descriptor::new(
        "faktor.browser.enabled",
        "Browser adapter",
        "Enable the browser adapter surface where one is advertised.",
        "browser",
        ValueKind::Bool,
        Value::Bool(false),
        ApplyTiming::Effective,
    ));
    add(Descriptor::new(
        "faktor.telemetry.enabled",
        "Telemetry",
        "Explicit opt-in diagnostic telemetry; never carries secrets or source payloads.",
        "privacy",
        ValueKind::Bool,
        Value::Bool(false),
        ApplyTiming::Effective,
    ));
    add(Descriptor::new(
        "faktor.telemetry.categories",
        "Telemetry categories",
        "Explicit category allowlist for telemetry.",
        "privacy",
        ValueKind::StringList {
            max_items: 32,
            max_item_bytes: 64,
        },
        Value::Array(Vec::new()),
        ApplyTiming::Effective,
    ));
    add(Descriptor::new(
        "faktor.session.titleSearch",
        "Title search",
        "Search durable session titles in History.",
        "sessions",
        ValueKind::Bool,
        Value::Bool(true),
        ApplyTiming::Effective,
    ));
    add(Descriptor::new(
        "faktor.transcript.searchLimit",
        "Transcript search page",
        "Maximum transcript search results per page.",
        "sessions",
        ValueKind::Integer { min: 1, max: 200 },
        Value::Number(50.into()),
        ApplyTiming::Effective,
    ));
    add(Descriptor::new(
        "faktor.providers.credentialRef",
        "Provider credential",
        "Opaque vault reference for a provider connection (write-only secret binding).",
        "providers",
        ValueKind::SecretReference,
        Value::Null,
        ApplyTiming::RestartRequired,
    )
    .secret());
    add_runtime_descriptors(&mut m);
    // A descriptor with no runtime consumer is a stored compatibility value,
    // never a writable control: the settings UI must not offer a save that
    // cannot have an effect. The consumer table is the one authority (and is
    // checked against the tree by the wiring matrix gate).
    for descriptor in m.values_mut() {
        if !crate::config_consumers::runtime_consumer(&descriptor.key).is_applied() {
            descriptor.writable = false;
        }
    }
    m
}

fn add_runtime_descriptors(m: &mut BTreeMap<String, Descriptor>) {
    let mut add = |d: Descriptor| {
        m.insert(d.key.clone(), d);
    };
    // Read-only runtime facts surfaced by the same snapshot (never writable;
    // the runtime identifies them, a settings patch cannot fake them).
    add(Descriptor::new(
        "faktor.runtime.binaryPath",
        "Runtime path",
        "The running daemon executable path (read-only).",
        "runtime",
        ValueKind::Text { max_bytes: 4096 },
        Value::String(String::new()),
        ApplyTiming::Effective,
    )
    .locked());
    add(Descriptor::new(
        "faktor.runtime.dataDir",
        "Data directory",
        "The daemon data directory (read-only).",
        "runtime",
        ValueKind::Text { max_bytes: 4096 },
        Value::String(String::new()),
        ApplyTiming::Effective,
    )
    .locked());
    add(Descriptor::new(
        "faktor.runtime.installRoot",
        "Install root",
        "The verified install root (read-only).",
        "runtime",
        ValueKind::Text { max_bytes: 4096 },
        Value::String(String::new()),
        ApplyTiming::Effective,
    )
    .locked());
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OriginValue {
    pub scope: Scope,
    pub value: Value,
    /// Exactly one origin wins for one key.
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectiveValue {
    pub key: String,
    pub value: Value,
    pub scope: Scope,
    /// `default` | `user` | `project` | `machine` | `managed`
    pub source: String,
    pub locked: bool,
    pub apply_timing: ApplyTiming,
    /// Every scope that sets this key, highest precedence first. The active
    /// origin is the winner; the others are shadowed (never silently lost).
    #[serde(default)]
    pub origins: Vec<OriginValue>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingConflict {
    pub key: String,
    pub winner: Scope,
    pub shadowed: Vec<Scope>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub revision: u64,
    pub runtime_revision: u64,
    pub values: BTreeMap<String, EffectiveValue>,
    /// Keys set by more than one scope (S60/S64 conflict display).
    #[serde(default)]
    pub conflicts: Vec<SettingConflict>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub key: String,
    pub scope: Scope,
    /// `None` removes the override (the default/inherited value wins).
    pub value: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Patch {
    pub expected_revision: u64,
    pub changes: Vec<Change>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldError {
    pub key: String,
    pub scope: Option<Scope>,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommitReceipt {
    pub saved_revision: u64,
    pub runtime_revision: u64,
    pub changed: usize,
    pub field_states: BTreeMap<String, ApplyState>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RollbackReceipt {
    pub saved_revision: u64,
    pub runtime_revision: u64,
    pub rolled_back_to: u64,
    pub changed: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigBundle {
    pub schema: String,
    pub revision: u64,
    pub values: Vec<BundleValue>,
    pub omitted_secrets: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BundleValue {
    pub key: String,
    pub scope: Scope,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub key: String,
    pub label: String,
    pub category: String,
    pub description: String,
    pub value: Value,
    pub source: String,
    pub scope: Scope,
    pub locked: bool,
    pub apply_timing: ApplyTiming,
    pub matched: String,
}

pub const BUNDLE_SCHEMA: &str = "faktor-config-bundle/v1";

pub struct ConfigService {
    db: FoundationDb,
    descriptors: BTreeMap<String, Descriptor>,
    /// Last computed effective snapshot. Every read path (`/native/config/
    /// snapshot`, descriptors, search, export, runtime `value()` reads)
    /// used to re-read every `config_value` row and re-parse each JSON
    /// value; configuration changes are rare, so one cached snapshot keyed
    /// by the service's own write generation is exact. Every mutator below
    /// invalidates it after a successful write.
    snapshot_cache: std::sync::Mutex<Option<(u64, Snapshot)>>,
    /// Generation fence for the snapshot cache: a read that started before
    /// a concurrent commit must not publish its (pre-commit) snapshot into
    /// the fresh generation.
    snapshot_generation: std::sync::atomic::AtomicU64,
}

impl ConfigService {
    pub fn new(db: FoundationDb) -> Self {
        Self {
            db,
            descriptors: default_descriptors(),
            snapshot_cache: std::sync::Mutex::new(None),
            snapshot_generation: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Test/benchmark constructor with an explicit descriptor table.
    pub fn with_descriptors(db: FoundationDb, descriptors: BTreeMap<String, Descriptor>) -> Self {
        Self {
            db,
            descriptors,
            snapshot_cache: std::sync::Mutex::new(None),
            snapshot_generation: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn descriptors(&self) -> &BTreeMap<String, Descriptor> {
        &self.descriptors
    }

    /// Insert/refresh a non-persistent descriptor (provider connection fields
    /// are declared by the lifecycle service through this seam).
    pub fn upsert_descriptor(&mut self, descriptor: Descriptor) {
        self.descriptors.insert(descriptor.key.clone(), descriptor);
        self.invalidate_snapshot();
    }

    fn invalidate_snapshot(&self) {
        self.snapshot_generation
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        *self
            .snapshot_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    pub fn snapshot(&self) -> Result<Snapshot, FoundationError> {
        use std::sync::atomic::Ordering;
        let generation = self.snapshot_generation.load(Ordering::Acquire);
        if let Some((cached_generation, cached)) = self
            .snapshot_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            if *cached_generation == generation {
                return Ok(cached.clone());
            }
        }
        let snapshot = self.snapshot_uncached()?;
        let mut cache = self
            .snapshot_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Only publish when no writer invalidated while this read ran.
        if self.snapshot_generation.load(Ordering::Acquire) == generation {
            *cache = Some((generation, snapshot.clone()));
        }
        Ok(snapshot)
    }

    fn snapshot_uncached(&self) -> Result<Snapshot, FoundationError> {
        let (revision, runtime_revision, rows) = self.db.read("config snapshot", |conn| {
            let (revision, runtime_revision): (i64, i64) = conn
                .query_row(
                    "SELECT revision, runtime_revision FROM config_meta WHERE id = 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let mut stmt = conn
                .prepare("SELECT key, scope, value_json FROM config_value")
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })
                .map_err(|e| FoundationError::Store(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok((revision.max(0) as u64, runtime_revision.max(0) as u64, rows))
        })?;
        let mut stored: BTreeMap<String, Vec<(Scope, Value)>> = BTreeMap::new();
        for (key, scope, raw) in rows {
            let Some(scope) = Scope::parse(&scope) else {
                return Err(FoundationError::Corrupt(format!(
                    "config row {key:?} carries unknown scope {scope:?}"
                )));
            };
            let value: Value = serde_json::from_str(&raw).map_err(|e| {
                FoundationError::Corrupt(format!("config row {key:?} value is not JSON: {e}"))
            })?;
            stored.entry(key).or_default().push((scope, value));
        }
        // Deterministic precedence: managed > machine > project > user.
        // Scope's Ord is declaration order, so sorting descending picks the
        // winner exactly once; the stored row order is irrelevant.
        for origins in stored.values_mut() {
            origins.sort_by_key(|(scope, _)| std::cmp::Reverse(*scope));
        }
        let mut values = BTreeMap::new();
        let mut conflicts = Vec::new();
        for (key, descriptor) in &self.descriptors {
            let (scope, value, source, origins) = match stored.get(key) {
                Some(origins) if !origins.is_empty() => {
                    let winner = &origins[0];
                    if origins.len() > 1 {
                        conflicts.push(SettingConflict {
                            key: key.clone(),
                            winner: winner.0,
                            shadowed: origins[1..].iter().map(|(scope, _)| *scope).collect(),
                            reason: "scope precedence (managed > machine > project > user)"
                                .to_string(),
                        });
                    }
                    (
                        winner.0,
                        winner.1.clone(),
                        winner.0.as_str().to_string(),
                        origins
                            .iter()
                            .enumerate()
                            .map(|(index, (scope, value))| OriginValue {
                                scope: *scope,
                                value: value.clone(),
                                active: index == 0,
                            })
                            .collect(),
                    )
                }
                _ => (
                    Scope::User,
                    descriptor.default.clone(),
                    "default".to_string(),
                    Vec::new(),
                ),
            };
            values.insert(
                key.clone(),
                EffectiveValue {
                    key: key.clone(),
                    value,
                    scope,
                    source,
                    locked: !descriptor.writable,
                    apply_timing: descriptor.apply_timing,
                    origins,
                },
            );
        }
        Ok(Snapshot {
            revision,
            runtime_revision,
            values,
            conflicts,
        })
    }

    /// The typed value of one key (default included; `None` for an unknown
    /// key). Used by runtime seams that read a single setting.
    pub fn value(&self, key: &str) -> Result<Option<Value>, FoundationError> {
        let snap = self.snapshot()?;
        Ok(snap.values.get(key).map(|v| v.value.clone()))
    }

    pub fn string_value(&self, key: &str) -> Result<Option<String>, FoundationError> {
        Ok(self
            .value(key)?
            .and_then(|v| v.as_str().map(str::to_string))
            .filter(|s| !s.is_empty()))
    }

    pub fn u64_value(&self, key: &str) -> Result<Option<u64>, FoundationError> {
        Ok(self.value(key)?.and_then(|v| v.as_u64()))
    }

    /// Validate a complete patch WITHOUT persisting. Returns every field
    /// error (never just the first).
    pub fn validate(&self, patch: &Patch) -> Result<(), Vec<FieldError>> {
        let snapshot = self.snapshot().map_err(|e| {
            vec![FieldError {
                key: "configuration".into(),
                scope: None,
                code: "store".into(),
                message: e.to_string(),
            }]
        })?;
        self.validate_against(patch, &snapshot)
    }

    fn validate_against(&self, patch: &Patch, snapshot: &Snapshot) -> Result<(), Vec<FieldError>> {
        let mut errors = Vec::new();
        if patch.changes.len() > MAX_PATCH_CHANGES {
            errors.push(FieldError {
                key: "*".into(),
                scope: None,
                code: "oversized".into(),
                message: format!("a patch carries at most {MAX_PATCH_CHANGES} changes"),
            });
        }
        if patch.expected_revision != snapshot.revision {
            errors.push(FieldError {
                key: "revision".into(),
                scope: None,
                code: "conflict".into(),
                message: format!(
                    "expected revision {} but the current revision is {}; refresh and rebase the draft",
                    patch.expected_revision, snapshot.revision
                ),
            });
        }
        let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
        let mut aliases: BTreeMap<&str, &str> = BTreeMap::new();
        for change in &patch.changes {
            let Some(descriptor) = self.descriptors.get(&change.key) else {
                errors.push(FieldError {
                    key: change.key.clone(),
                    scope: Some(change.scope),
                    code: "unknown_key".into(),
                    message: "Unknown setting".into(),
                });
                continue;
            };
            if !seen.insert((change.key.clone(), change.scope.as_str().to_string())) {
                errors.push(FieldError {
                    key: change.key.clone(),
                    scope: Some(change.scope),
                    code: "duplicate".into(),
                    message: "Duplicate key and scope in one patch".into(),
                });
            }
            if !descriptor.writable {
                let message = match crate::config_consumers::runtime_consumer(&descriptor.key) {
                    crate::config_consumers::RuntimeConsumer::StoredOnly { reason } => {
                        format!(
                            "{} is stored only in this build and cannot be applied: {reason}",
                            descriptor.key
                        )
                    }
                    crate::config_consumers::RuntimeConsumer::Unknown => format!(
                        "{} has no classified runtime consumer; refusing a write that could not apply",
                        descriptor.key
                    ),
                    crate::config_consumers::RuntimeConsumer::Applied { .. } => {
                        format!("{} is read-only", descriptor.key)
                    }
                };
                errors.push(FieldError {
                    key: change.key.clone(),
                    scope: Some(change.scope),
                    code: "read_only".into(),
                    message,
                });
            }
            if !descriptor.allowed_scopes.contains(&change.scope) {
                errors.push(FieldError {
                    key: change.key.clone(),
                    scope: Some(change.scope),
                    code: "scope".into(),
                    message: format!(
                        "scope {} is not allowed for {}",
                        change.scope.as_str(),
                        descriptor.key
                    ),
                });
            }
            if let Some(group) = &descriptor.alias_group {
                if let Some(previous) = aliases.insert(group, &change.key) {
                    errors.push(FieldError {
                        key: change.key.clone(),
                        scope: Some(change.scope),
                        code: "alias_conflict".into(),
                        message: format!(
                            "{} and {} are aliases; set exactly one",
                            previous, change.key
                        ),
                    });
                }
            }
            if let Some(value) = &change.value {
                if let Err(message) = validate_value(descriptor, value) {
                    errors.push(FieldError {
                        key: change.key.clone(),
                        scope: Some(change.scope),
                        code: if descriptor.secret_reference_only {
                            "secret_reference"
                        } else {
                            "invalid_value"
                        }
                        .into(),
                        message,
                    });
                    continue;
                }
                if descriptor.secret_reference_only {
                    if let Some(raw) = value.as_str() {
                        if !valid_secret_reference(raw) {
                            errors.push(FieldError {
                                key: change.key.clone(),
                                scope: Some(change.scope),
                                code: "secret_reference".into(),
                                message: "Use an opaque secret reference (vault://... or env:NAME)"
                                    .into(),
                            });
                        }
                    } else if !value.is_null() {
                        errors.push(FieldError {
                            key: change.key.clone(),
                            scope: Some(change.scope),
                            code: "secret_reference".into(),
                            message: "Use an opaque secret reference".into(),
                        });
                    }
                }
            }
        }
        errors.extend(self.cross_validate(snapshot, patch));
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    fn cross_validate(&self, snapshot: &Snapshot, patch: &Patch) -> Vec<FieldError> {
        let mut merged: BTreeMap<String, Value> = snapshot
            .values
            .iter()
            .map(|(k, v)| (k.clone(), v.value.clone()))
            .collect();
        for change in &patch.changes {
            match &change.value {
                Some(value) => {
                    merged.insert(change.key.clone(), value.clone());
                }
                None => {
                    merged.remove(&change.key);
                }
            }
        }
        let mut errors = Vec::new();
        let base = merged
            .get("faktor.retry.baseDelayMs")
            .and_then(Value::as_u64)
            .unwrap_or(500);
        let max = merged
            .get("faktor.retry.maxDelayMs")
            .and_then(Value::as_u64)
            .unwrap_or(30_000);
        if base > max {
            errors.push(FieldError {
                key: "faktor.retry.baseDelayMs".into(),
                scope: None,
                code: "cross_field".into(),
                message: "base delay must be <= maximum delay".into(),
            });
        }
        let output_reserve = merged
            .get("faktor.context.outputReserve")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let safety_reserve = merged
            .get("faktor.context.safetyReserve")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if output_reserve.saturating_add(safety_reserve) > 16_000_000 {
            errors.push(FieldError {
                key: "faktor.context.safetyReserve".into(),
                scope: None,
                code: "cross_field".into(),
                message: "output + safety reserves exceed the bounded capacity domain".into(),
            });
        }
        let embedding = |key: &str| {
            merged
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .is_some_and(|value| !value.is_empty())
        };
        let provider_set = embedding("faktor.retrieval.embeddingProvider");
        let model_set = embedding("faktor.retrieval.embeddingModel");
        if provider_set != model_set {
            errors.push(FieldError {
                key: if provider_set {
                    "faktor.retrieval.embeddingProvider".into()
                } else {
                    "faktor.retrieval.embeddingModel".into()
                },
                scope: None,
                code: "cross_field".into(),
                message: "embedding provider and model must be set together (or both cleared)"
                    .into(),
            });
        }
        errors
    }

    /// Commit ONE validated patch: compare the revision inside the
    /// transaction, write all changes plus their linear-history rows or
    /// nothing.
    pub fn commit(&self, patch: &Patch) -> Result<CommitReceipt, FoundationError> {
        let snapshot = self.snapshot()?;
        if let Err(errors) = self.validate_against(patch, &snapshot) {
            return Err(FoundationError::Validation(errors));
        }
        let result = self.db.write("config commit", |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| FoundationError::Store(format!("begin commit: {e}")))?;
            let (current, runtime_revision): (i64, i64) = tx
                .query_row(
                    "SELECT revision, runtime_revision FROM config_meta WHERE id = 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if current.max(0) as u64 != patch.expected_revision {
                return Err(FoundationError::Conflict(format!(
                    "revision changed concurrently: expected {} but found {}",
                    patch.expected_revision, current
                )));
            }
            // Re-run the FULL cross-validation against the transaction's
            // values (a concurrent writer could have changed dependencies
            // between the pre-check and this transaction).
            let live_snapshot = snapshot_from_tx(&tx, &self.descriptors)?;
            if let Err(errors) = self.validate_against(patch, &live_snapshot) {
                return Err(FoundationError::Validation(errors));
            }
            let saved = current.max(0) as u64 + 1;
            let now = now_ms();
            let mut states = BTreeMap::new();
            let mut all_effective = true;
            for change in &patch.changes {
                let descriptor = self
                    .descriptors
                    .get(&change.key)
                    .expect("validation checked every key");
                let before: Option<String> = tx
                    .query_row(
                        "SELECT value_json FROM config_value WHERE key = ?1 AND scope = ?2",
                        params![change.key, change.scope.as_str()],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(|e| FoundationError::Store(e.to_string()))?;
                match &change.value {
                    Some(value) => {
                        let raw = serde_json::to_string(value)
                            .map_err(|e| FoundationError::Store(format!("encode value: {e}")))?;
                        tx.execute(
                            "INSERT INTO config_value(key, scope, value_json, revision)
                             VALUES (?1, ?2, ?3, ?4)
                             ON CONFLICT(key, scope) DO UPDATE SET
                               value_json = excluded.value_json,
                               revision = excluded.revision",
                            params![change.key, change.scope.as_str(), raw, saved as i64],
                        )
                        .map_err(|e| FoundationError::Store(e.to_string()))?;
                        tx.execute(
                            "INSERT OR REPLACE INTO config_history
                               (revision, key, scope, before_json, after_json, created_ms)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                            params![
                                saved as i64,
                                change.key,
                                change.scope.as_str(),
                                before,
                                raw,
                                now
                            ],
                        )
                        .map_err(|e| FoundationError::Store(e.to_string()))?;
                    }
                    None => {
                        tx.execute(
                            "DELETE FROM config_value WHERE key = ?1 AND scope = ?2",
                            params![change.key, change.scope.as_str()],
                        )
                        .map_err(|e| FoundationError::Store(e.to_string()))?;
                        tx.execute(
                            "INSERT OR REPLACE INTO config_history
                               (revision, key, scope, before_json, after_json, created_ms)
                             VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
                            params![saved as i64, change.key, change.scope.as_str(), before, now],
                        )
                        .map_err(|e| FoundationError::Store(e.to_string()))?;
                    }
                }
                if descriptor.apply_timing != ApplyTiming::Effective {
                    all_effective = false;
                }
                states.insert(
                    change.key.clone(),
                    ApplyState::for_timing(descriptor.apply_timing),
                );
            }
            let next_runtime = if all_effective {
                saved as i64
            } else {
                runtime_revision
            };
            tx.execute(
                "UPDATE config_meta SET revision = ?1, runtime_revision = ?2, updated_ms = ?3
                 WHERE id = 1",
                params![saved as i64, next_runtime, now],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            tx.commit()
                .map_err(|e| FoundationError::Store(format!("commit: {e}")))?;
            Ok(CommitReceipt {
                saved_revision: saved,
                runtime_revision: next_runtime.max(0) as u64,
                changed: patch.changes.len(),
                field_states: states,
            })
        });
        if result.is_ok() {
            self.invalidate_snapshot();
        }
        result
    }

    /// Advance the runtime revision after the daemon observed the saved
    /// values (explicit acknowledgement; never implied by a save).
    pub fn acknowledge_runtime(&self, expected_revision: u64) -> Result<u64, FoundationError> {
        let result = self.db.write("config acknowledge", |conn| {
            let current: i64 = conn
                .query_row(
                    "SELECT revision FROM config_meta WHERE id = 1",
                    [],
                    |r| r.get(0),
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if current.max(0) as u64 != expected_revision {
                return Err(FoundationError::Conflict(format!(
                    "runtime acknowledgement expected revision {expected_revision} but found {current}"
                )));
            }
            conn.execute(
                "UPDATE config_meta SET runtime_revision = revision, updated_ms = ?1 WHERE id = 1",
                params![now_ms()],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(expected_revision)
        });
        if result.is_ok() {
            self.invalidate_snapshot();
        }
        result
    }

    /// Roll a linear history back to `target_revision`, writing a NEW
    /// revision whose values equal the target state. The replay is bounded by
    /// the number of changed keys (never a full snapshot per step).
    pub fn rollback(
        &self,
        expected_revision: u64,
        target_revision: u64,
    ) -> Result<RollbackReceipt, FoundationError> {
        if target_revision > expected_revision {
            return Err(FoundationError::Malformed(
                "cannot roll forward: target revision is newer than the current revision".into(),
            ));
        }
        let result = self.db.write("config rollback", |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| FoundationError::Store(format!("begin rollback: {e}")))?;
            let (current, runtime_revision): (i64, i64) = tx
                .query_row(
                    "SELECT revision, runtime_revision FROM config_meta WHERE id = 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if current.max(0) as u64 != expected_revision {
                return Err(FoundationError::Conflict(format!(
                    "revision changed concurrently: expected {expected_revision} but found {current}"
                )));
            }
            let saved = current.max(0) as u64 + 1;
            let now = now_ms();
            let mut changed = 0usize;
            let mut stmt = tx
                .prepare(
                    "SELECT key, scope, before_json, after_json FROM config_history
                     WHERE revision > ?1 AND revision <= ?2
                     ORDER BY revision DESC, key ASC, scope ASC",
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let rows = stmt
                .query_map(
                    params![target_revision as i64, current],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, Option<String>>(3)?,
                        ))
                    },
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            drop(stmt);
            for (key, scope, before, after) in rows {
                // Each key history row describes before -> after; to replay
                // backwards we restore `before`. Later duplicates are handled
                // by the per-key current value check so an already-restored
                // key is not rewritten.
                let live: Option<String> = tx
                    .query_row(
                        "SELECT value_json FROM config_value WHERE key = ?1 AND scope = ?2",
                        params![&key, &scope],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(|e| FoundationError::Store(e.to_string()))?;
                if live == before {
                    continue;
                }
                changed += 1;
                match &before {
                    Some(raw) => {
                        let value: Value = serde_json::from_str(raw).map_err(|e| {
                            FoundationError::Corrupt(format!(
                                "history row {key:?} before value is not JSON: {e}"
                            ))
                        })?;
                        if let Some(descriptor) = self.descriptors.get(&key) {
                            validate_value(descriptor, &value)
                                .map_err(FoundationError::Corrupt)?;
                        }
                        tx.execute(
                            "INSERT INTO config_value(key, scope, value_json, revision)
                             VALUES (?1, ?2, ?3, ?4)
                             ON CONFLICT(key, scope) DO UPDATE SET
                               value_json = excluded.value_json,
                               revision = excluded.revision",
                            params![&key, &scope, raw, saved as i64],
                        )
                        .map_err(|e| FoundationError::Store(e.to_string()))?;
                    }
                    None => {
                        tx.execute(
                            "DELETE FROM config_value WHERE key = ?1 AND scope = ?2",
                            params![&key, &scope],
                        )
                        .map_err(|e| FoundationError::Store(e.to_string()))?;
                    }
                }
                tx.execute(
                    "INSERT OR REPLACE INTO config_history
                       (revision, key, scope, before_json, after_json, created_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![saved as i64, &key, &scope, live, after, now],
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            }
            tx.execute(
                "UPDATE config_meta SET revision = ?1, runtime_revision = ?2, updated_ms = ?3
                 WHERE id = 1",
                params![saved as i64, runtime_revision, now],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            tx.commit()
                .map_err(|e| FoundationError::Store(format!("commit rollback: {e}")))?;
            Ok(RollbackReceipt {
                saved_revision: saved,
                runtime_revision: runtime_revision.max(0) as u64,
                rolled_back_to: target_revision,
                changed,
            })
        });
        if result.is_ok() {
            self.invalidate_snapshot();
        }
        result
    }

    /// Bounded effective-settings search with provenance. One pass over the
    /// descriptor table; no cross product and no per-row snapshot reload.
    pub fn search(
        &self,
        query: &str,
        category: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>, FoundationError> {
        let limit = limit.clamp(1, MAX_SEARCH_LIMIT);
        let snapshot = self.snapshot()?;
        let needle = query.trim().to_ascii_lowercase();
        let mut hits = Vec::new();
        for (key, descriptor) in &self.descriptors {
            if let Some(category) = category {
                if descriptor.category != category {
                    continue;
                }
            }
            let matched = if needle.is_empty() {
                Some("all".to_string())
            } else if key.to_ascii_lowercase().contains(&needle) {
                Some("key".to_string())
            } else if descriptor.label.to_ascii_lowercase().contains(&needle) {
                Some("label".to_string())
            } else if descriptor
                .description
                .to_ascii_lowercase()
                .contains(&needle)
            {
                Some("description".to_string())
            } else if descriptor.category.to_ascii_lowercase().contains(&needle) {
                Some("category".to_string())
            } else {
                None
            };
            let Some(matched) = matched else { continue };
            let effective = snapshot.values.get(key);
            hits.push(SearchHit {
                key: key.clone(),
                label: descriptor.label.clone(),
                category: descriptor.category.clone(),
                description: descriptor.description.clone(),
                value: effective.map(|v| v.value.clone()).unwrap_or(Value::Null),
                source: effective
                    .map(|v| v.source.clone())
                    .unwrap_or_else(|| "default".into()),
                scope: effective.map(|v| v.scope).unwrap_or(Scope::User),
                locked: !descriptor.writable,
                apply_timing: descriptor.apply_timing,
                matched,
            });
            if hits.len() >= limit {
                break;
            }
        }
        Ok(hits)
    }

    /// Export a redacted, schema-validated bundle. Secret slots export only
    /// their opaque reference; secret MATERIAL never enters this structure.
    pub fn export(&self) -> Result<ConfigBundle, FoundationError> {
        let snapshot = self.snapshot()?;
        let mut values = Vec::new();
        let mut omitted = Vec::new();
        for (key, effective) in &snapshot.values {
            let Some(descriptor) = self.descriptors.get(key) else {
                continue;
            };
            if descriptor.secret_reference_only {
                if !effective.value.is_null() {
                    omitted.push(key.clone());
                }
                continue;
            }
            if effective.source == "default" {
                continue;
            }
            values.push(BundleValue {
                key: key.clone(),
                scope: effective.scope,
                value: effective.value.clone(),
            });
        }
        Ok(ConfigBundle {
            schema: BUNDLE_SCHEMA.to_string(),
            revision: snapshot.revision,
            values,
            omitted_secrets: omitted,
        })
    }

    /// Import a bundle as ONE atomic patch against `expected_revision`
    /// (defaults to the bundle's revision when called with `None`).
    pub fn import(
        &self,
        bundle: &ConfigBundle,
        expected_revision: Option<u64>,
    ) -> Result<CommitReceipt, FoundationError> {
        if bundle.schema != BUNDLE_SCHEMA {
            return Err(FoundationError::Malformed(format!(
                "unknown configuration bundle schema {:?}",
                bundle.schema
            )));
        }
        let encoded = serde_json::to_vec(bundle)
            .map_err(|e| FoundationError::Store(format!("encode bundle: {e}")))?;
        if encoded.len() > MAX_IMPORT_BYTES {
            return Err(FoundationError::Oversized(format!(
                "configuration bundle is {} bytes, over {MAX_IMPORT_BYTES}",
                encoded.len()
            )));
        }
        let snapshot = self.snapshot()?;
        let changes: Vec<Change> = bundle
            .values
            .iter()
            .map(|v| Change {
                key: v.key.clone(),
                scope: v.scope,
                value: Some(v.value.clone()),
            })
            .collect();
        let patch = Patch {
            expected_revision: expected_revision.unwrap_or(bundle.revision),
            changes,
        };
        // Import is an explicit user action: accept the current revision when
        // the imported bundle carries an older/newer revision unless the
        // caller pinned one.
        let patch = if expected_revision.is_none() && bundle.revision != snapshot.revision {
            Patch {
                expected_revision: snapshot.revision,
                ..patch
            }
        } else {
            patch
        };
        self.commit(&patch)
    }
}

fn snapshot_from_tx(
    tx: &rusqlite::Transaction<'_>,
    descriptors: &BTreeMap<String, Descriptor>,
) -> Result<Snapshot, FoundationError> {
    let (revision, runtime_revision): (i64, i64) = tx
        .query_row(
            "SELECT revision, runtime_revision FROM config_meta WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|e| FoundationError::Store(e.to_string()))?;
    let mut stmt = tx
        .prepare("SELECT key, scope, value_json FROM config_value")
        .map_err(|e| FoundationError::Store(e.to_string()))?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .map_err(|e| FoundationError::Store(e.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| FoundationError::Store(e.to_string()))?;
    drop(stmt);
    let mut stored: BTreeMap<String, Vec<(Scope, Value)>> = BTreeMap::new();
    for (key, scope, raw) in rows {
        let scope = Scope::parse(&scope).ok_or_else(|| {
            FoundationError::Corrupt(format!(
                "config row {key:?} carries unknown scope {scope:?}"
            ))
        })?;
        let value = serde_json::from_str(&raw).map_err(|e| {
            FoundationError::Corrupt(format!("config row {key:?} value is not JSON: {e}"))
        })?;
        stored.entry(key).or_default().push((scope, value));
    }
    for origins in stored.values_mut() {
        origins.sort_by_key(|(scope, _)| std::cmp::Reverse(*scope));
    }
    let mut values = BTreeMap::new();
    let mut conflicts = Vec::new();
    for (key, descriptor) in descriptors {
        let (scope, value, source, origins) = match stored.get(key) {
            Some(origins) if !origins.is_empty() => {
                let winner = &origins[0];
                if origins.len() > 1 {
                    conflicts.push(SettingConflict {
                        key: key.clone(),
                        winner: winner.0,
                        shadowed: origins[1..].iter().map(|(scope, _)| *scope).collect(),
                        reason: "scope precedence (managed > machine > project > user)".to_string(),
                    });
                }
                (
                    winner.0,
                    winner.1.clone(),
                    winner.0.as_str().to_string(),
                    origins
                        .iter()
                        .enumerate()
                        .map(|(index, (scope, value))| OriginValue {
                            scope: *scope,
                            value: value.clone(),
                            active: index == 0,
                        })
                        .collect(),
                )
            }
            _ => (
                Scope::User,
                descriptor.default.clone(),
                "default".to_string(),
                Vec::new(),
            ),
        };
        values.insert(
            key.clone(),
            EffectiveValue {
                key: key.clone(),
                value,
                scope,
                source,
                locked: !descriptor.writable,
                apply_timing: descriptor.apply_timing,
                origins,
            },
        );
    }
    Ok(Snapshot {
        revision: revision.max(0) as u64,
        runtime_revision: runtime_revision.max(0) as u64,
        values,
        conflicts,
    })
}

fn validate_value(descriptor: &Descriptor, value: &Value) -> Result<(), String> {
    let encoded = serde_json::to_vec(value)
        .map_err(|e| format!("{}: value is not serializable: {e}", descriptor.key))?;
    if encoded.len() > MAX_SETTING_VALUE_BYTES {
        return Err(format!(
            "{}: value is {} bytes, over {MAX_SETTING_VALUE_BYTES}",
            descriptor.key,
            encoded.len()
        ));
    }
    match &descriptor.kind {
        ValueKind::Bool => value
            .is_boolean()
            .then_some(())
            .ok_or_else(|| format!("{}: expected a boolean", descriptor.key)),
        ValueKind::Integer { min, max } => {
            let Some(n) = value.as_i64() else {
                return Err(format!("{}: expected an integer", descriptor.key));
            };
            if n < *min || n > *max {
                return Err(format!(
                    "{}: {n} is outside the served range {min}..={max}",
                    descriptor.key
                ));
            }
            Ok(())
        }
        ValueKind::Number { min, max } => {
            let Some(n) = value.as_f64() else {
                return Err(format!("{}: expected a number", descriptor.key));
            };
            if !n.is_finite() || n < *min || n > *max {
                return Err(format!(
                    "{}: {n} is outside the served range {min}..={max}",
                    descriptor.key
                ));
            }
            Ok(())
        }
        ValueKind::Text { max_bytes } => {
            let Some(s) = value.as_str() else {
                return Err(format!("{}: expected a string", descriptor.key));
            };
            if s.len() > *max_bytes {
                return Err(format!(
                    "{}: string is {} bytes, over {max_bytes}",
                    descriptor.key,
                    s.len()
                ));
            }
            Ok(())
        }
        ValueKind::Enum { choices } => {
            let Some(s) = value.as_str() else {
                return Err(format!("{}: expected a string choice", descriptor.key));
            };
            if !choices.iter().any(|c| c == s) {
                return Err(format!(
                    "{}: {s:?} is not one of {}",
                    descriptor.key,
                    choices.join(", ")
                ));
            }
            Ok(())
        }
        ValueKind::StringList {
            max_items,
            max_item_bytes,
        } => {
            let Some(items) = value.as_array() else {
                return Err(format!("{}: expected a list of strings", descriptor.key));
            };
            if items.len() > *max_items {
                return Err(format!(
                    "{}: {} items, over {max_items}",
                    descriptor.key,
                    items.len()
                ));
            }
            for item in items {
                let Some(s) = item.as_str() else {
                    return Err(format!("{}: list entries must be strings", descriptor.key));
                };
                if s.len() > *max_item_bytes {
                    return Err(format!(
                        "{}: list entry is {} bytes, over {max_item_bytes}",
                        descriptor.key,
                        s.len()
                    ));
                }
            }
            Ok(())
        }
        ValueKind::SecretReference => {
            if value.is_null() {
                return Ok(());
            }
            let Some(s) = value.as_str() else {
                return Err(format!(
                    "{}: expected an opaque secret reference",
                    descriptor.key
                ));
            };
            if !valid_secret_reference(s) {
                return Err(format!(
                    "{}: expected vault://... or env:NAME, got a non-reference value",
                    descriptor.key
                ));
            }
            Ok(())
        }
        ValueKind::Json => Ok(()),
    }
}

/// A secret reference is an opaque handle, never a secret: `vault://<kind>/
/// <id>` or `env:<NAME>`. A raw API-key-shaped string is refused here.
pub fn valid_secret_reference(raw: &str) -> bool {
    if let Some(rest) = raw.strip_prefix("vault://") {
        let mut parts = rest.split('/');
        let (Some(kind), Some(id), None) = (parts.next(), parts.next(), parts.next()) else {
            return false;
        };
        return !kind.is_empty()
            && kind.len() <= 64
            && !id.is_empty()
            && id.len() <= 128
            && kind
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    }
    if let Some(name) = raw.strip_prefix("env:") {
        return !name.is_empty()
            && name.len() <= 128
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    }
    false
}

/// Stable digest helper for receipts and bundles.
pub fn digest_bytes(bytes: &[u8]) -> String {
    hash_hex(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> (tempfile::TempDir, ConfigService) {
        let dir = tempfile::tempdir().unwrap();
        let db = FoundationDb::open(dir.path().join("f.db")).unwrap();
        (dir, ConfigService::new(db))
    }

    fn patch(rev: u64, changes: Vec<Change>) -> Patch {
        Patch {
            expected_revision: rev,
            changes,
        }
    }

    #[test]
    fn snapshot_precedence_is_deterministic_and_conflicts_are_visible() {
        let (_dir, svc) = service();
        let key = "faktor.retry.maxAttempts";
        let mut rev = svc.snapshot().unwrap().revision;
        for (scope, value) in [(Scope::User, 2), (Scope::Project, 3), (Scope::Machine, 4)] {
            svc.commit(&patch(
                rev,
                vec![Change {
                    key: key.into(),
                    scope,
                    value: Some(Value::from(value)),
                }],
            ))
            .unwrap();
            rev = svc.snapshot().unwrap().revision;
        }
        let snapshot = svc.snapshot().unwrap();
        let effective = &snapshot.values[key];
        assert_eq!(effective.scope, Scope::Machine);
        assert_eq!(effective.value, Value::from(4));
        assert_eq!(effective.source, "machine");
        assert_eq!(
            effective
                .origins
                .iter()
                .map(|o| (o.scope, o.active))
                .collect::<Vec<_>>(),
            vec![
                (Scope::Machine, true),
                (Scope::Project, false),
                (Scope::User, false),
            ]
        );
        let conflict = snapshot
            .conflicts
            .iter()
            .find(|c| c.key == key)
            .expect("conflict row");
        assert_eq!(conflict.winner, Scope::Machine);
        assert_eq!(conflict.shadowed, vec![Scope::Project, Scope::User]);
    }

    #[test]
    fn unknown_keys_duplicates_scopes_and_secret_values_are_rejected_completely() {
        let (_dir, svc) = service();
        let rev = svc.snapshot().unwrap().revision;
        let errs = svc
            .validate(&patch(
                rev,
                vec![
                    Change {
                        key: "nope".into(),
                        scope: Scope::User,
                        value: Some(Value::Bool(true)),
                    },
                    Change {
                        key: "faktor.retry.maxAttempts".into(),
                        scope: Scope::User,
                        value: Some(Value::from(99)),
                    },
                    Change {
                        key: "faktor.providers.credentialRef".into(),
                        scope: Scope::User,
                        value: Some(Value::String("sk-live-raw-key".into())),
                    },
                    Change {
                        key: "faktor.retry.jitter".into(),
                        scope: Scope::Managed,
                        value: Some(Value::from(0.1)),
                    },
                ],
            ))
            .unwrap_err();
        assert!(errs.iter().any(|e| e.code == "unknown_key"));
        assert!(errs.iter().any(|e| e.code == "invalid_value"));
        assert!(errs.iter().any(|e| e.code == "secret_reference"));
        assert!(errs.iter().any(|e| e.code == "scope"));
        assert_eq!(svc.snapshot().unwrap().revision, rev, "nothing persisted");
    }

    #[test]
    fn atomic_commit_revision_conflict_and_field_states_are_exact() {
        let (_dir, svc) = service();
        let rev = svc.snapshot().unwrap().revision;
        let receipt = svc
            .commit(&patch(
                rev,
                vec![
                    Change {
                        key: "faktor.retry.maxAttempts".into(),
                        scope: Scope::User,
                        value: Some(Value::from(5)),
                    },
                    Change {
                        key: "faktor.diagnostics.exportMaxBytes".into(),
                        scope: Scope::User,
                        value: Some(Value::from(65_536)),
                    },
                ],
            ))
            .unwrap();
        assert_eq!(receipt.saved_revision, rev + 1);
        assert_eq!(
            receipt.runtime_revision, rev,
            "a non-effective field blocks ack"
        );
        assert_eq!(
            receipt.field_states["faktor.retry.maxAttempts"],
            ApplyState::RestartRequired
        );
        assert_eq!(
            receipt.field_states["faktor.diagnostics.exportMaxBytes"],
            ApplyState::Effective
        );
        let stale = svc
            .commit(&patch(
                rev,
                vec![Change {
                    key: "faktor.retry.maxAttempts".into(),
                    scope: Scope::User,
                    value: Some(Value::from(4)),
                }],
            ))
            .unwrap_err();
        assert!(matches!(stale, FoundationError::Validation(_)));
        assert_eq!(
            svc.snapshot().unwrap().values["faktor.retry.maxAttempts"].value,
            Value::from(5),
            "the stale write persisted nothing"
        );
    }

    #[test]
    fn rollback_replays_linear_history_and_forgets_overrides() {
        let (_dir, svc) = service();
        let r0 = svc.snapshot().unwrap().revision;
        svc.commit(&patch(
            r0,
            vec![Change {
                key: "faktor.diagnostics.exportMaxBytes".into(),
                scope: Scope::User,
                value: Some(Value::from(65_536)),
            }],
        ))
        .unwrap();
        let r1 = svc.snapshot().unwrap().revision;
        svc.commit(&patch(
            r1,
            vec![Change {
                key: "faktor.diagnostics.exportMaxBytes".into(),
                scope: Scope::User,
                value: Some(Value::from(131_072)),
            }],
        ))
        .unwrap();
        let r2 = svc.snapshot().unwrap().revision;
        let receipt = svc.rollback(r2, r0).unwrap();
        assert_eq!(receipt.rolled_back_to, r0);
        let snap = svc.snapshot().unwrap();
        assert_eq!(snap.revision, r2 + 1);
        assert_eq!(
            snap.values["faktor.diagnostics.exportMaxBytes"].source, "default",
            "the override is removed, not replaced with 0"
        );
    }

    #[test]
    fn search_is_bounded_and_reports_the_winning_source() {
        let (_dir, svc) = service();
        let rev = svc.snapshot().unwrap().revision;
        svc.commit(&patch(
            rev,
            vec![Change {
                key: "faktor.budget.maxCostMicro".into(),
                scope: Scope::Project,
                value: Some(Value::from(5_000_000u64)),
            }],
        ))
        .unwrap();
        let hits = svc.search("budget", None, 100).unwrap();
        assert!(!hits.is_empty());
        let hit = hits
            .iter()
            .find(|h| h.key == "faktor.budget.maxCostMicro")
            .unwrap();
        assert_eq!(hit.value, Value::from(5_000_000u64));
        assert_eq!(hit.source, "project");
        assert_eq!(hit.scope, Scope::Project);
        assert!(svc.search("", None, 3).unwrap().len() <= 3);
    }

    #[test]
    fn export_never_contains_secret_material_and_import_is_one_patch() {
        let secret = || {
            Descriptor::new(
                "provider.test.credential",
                "Provider credential",
                "Opaque vault reference for the test provider connection.",
                "providers",
                ValueKind::SecretReference,
                Value::Null,
                ApplyTiming::RestartRequired,
            )
            .secret()
        };
        let (_dir, mut svc) = service();
        svc.upsert_descriptor(secret());
        let rev = svc.snapshot().unwrap().revision;
        svc.commit(&patch(
            rev,
            vec![
                Change {
                    key: "provider.test.credential".into(),
                    scope: Scope::User,
                    value: Some(Value::String("vault://provider/abc".into())),
                },
                Change {
                    key: "faktor.budget.maxTokens".into(),
                    scope: Scope::User,
                    value: Some(Value::from(10_000)),
                },
            ],
        ))
        .unwrap();
        let bundle = svc.export().unwrap();
        let raw = serde_json::to_string(&bundle).unwrap();
        assert!(!raw.contains("vault://provider/abc"));
        assert!(bundle
            .omitted_secrets
            .contains(&"provider.test.credential".into()));
        let (_target_dir, mut target) = service();
        target.upsert_descriptor(secret());
        let receipt = target.import(&bundle, None).unwrap();
        assert!(receipt.saved_revision > 0);
        assert_eq!(
            target.snapshot().unwrap().values["faktor.budget.maxTokens"].value,
            Value::from(10_000)
        );
    }

    #[test]
    fn ten_thousand_descriptors_load_patch_and_search_stay_sub_linear_per_key() {
        let dir = tempfile::tempdir().unwrap();
        let db = FoundationDb::open(dir.path().join("f.db")).unwrap();
        let mut descriptors = default_descriptors();
        for i in 0..10_000 {
            let key = format!("bench.setting.{i:05}");
            descriptors.insert(
                key.clone(),
                Descriptor::new(
                    &key,
                    &format!("Benchmark setting {i}"),
                    "Synthetic setting for the bounded configuration benchmark.",
                    "bench",
                    ValueKind::Integer {
                        min: 0,
                        max: 1_000_000,
                    },
                    Value::from(0),
                    ApplyTiming::NextTask,
                ),
            );
        }
        let svc = ConfigService::with_descriptors(db, descriptors);
        let started = std::time::Instant::now();
        let snapshot = svc.snapshot().unwrap();
        let load = started.elapsed();
        assert_eq!(snapshot.values.len(), 10_000 + default_descriptors().len());
        let changes: Vec<Change> = (0..500)
            .map(|i| Change {
                key: format!("bench.setting.{i:05}"),
                scope: Scope::User,
                value: Some(Value::from(i as i64)),
            })
            .collect();
        let started = std::time::Instant::now();
        let receipt = svc
            .commit(&Patch {
                expected_revision: snapshot.revision,
                changes,
            })
            .unwrap();
        let commit = started.elapsed();
        let started = std::time::Instant::now();
        let hits = svc.search("benchmark setting 9999", None, 50).unwrap();
        let search = started.elapsed();
        assert_eq!(receipt.changed, 500);
        assert_eq!(hits.len(), 1);
        eprintln!("config 10k: load {load:?}, 500-key patch {commit:?}, search {search:?}");
        assert!(load < std::time::Duration::from_secs(2));
        assert!(commit < std::time::Duration::from_secs(2));
        assert!(search < std::time::Duration::from_secs(1));
    }

    /// The snapshot cache is exact: repeat reads are identical until a
    /// successful commit invalidates, and a reader racing commits never
    /// regresses to an older revision (the generation fence keeps a
    /// pre-commit read from publishing into the fresh generation).
    #[test]
    fn snapshot_cache_is_fenced_against_concurrent_commits() {
        let (_dir, svc) = service();
        let key = "faktor.retry.maxAttempts";
        let first = svc.snapshot().unwrap();
        assert_eq!(svc.snapshot().unwrap(), first, "cached read is exact");
        svc.commit(&patch(
            first.revision,
            vec![Change {
                key: key.into(),
                scope: Scope::User,
                value: Some(Value::from(7)),
            }],
        ))
        .unwrap();
        let after = svc.snapshot().unwrap();
        assert!(after.revision > first.revision);
        assert_eq!(after.values[key].value, Value::from(7));

        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    let mut last = svc.snapshot().unwrap().revision;
                    for _ in 0..200 {
                        let now = svc.snapshot().unwrap().revision;
                        assert!(
                            now >= last,
                            "a snapshot revision regressed {last} -> {now} (stale cache published)"
                        );
                        last = now;
                    }
                });
            }
            scope.spawn(|| {
                for i in 0..40u64 {
                    let rev = svc.snapshot().unwrap().revision;
                    svc.commit(&patch(
                        rev,
                        vec![Change {
                            key: key.into(),
                            scope: Scope::User,
                            value: Some(Value::from(2 + (i % 3))),
                        }],
                    ))
                    .unwrap();
                }
            });
        });
    }
}
