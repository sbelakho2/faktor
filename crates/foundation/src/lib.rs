//! `faktor-foundation` — the Edition 5 daemon foundation.
//!
//! One durable SQLite authority (under the daemon data directory) for:
//! configuration revisions and atomic patches, the machine-owned credential
//! vault, provider connection lifecycle and health, model parameter
//! descriptors and call receipts, context manifests, persistent permission
//! policy, MCP/skills/hooks lifecycle, and redacted diagnostic bundles.
//!
//! Design rules enforced here:
//! - A save is not an apply: per-field apply state is returned, and the
//!   runtime revision never advances for fields the daemon cannot
//!   acknowledge synchronously.
//! - Secret material never leaves the vault; every shared structure carries
//!   opaque references only.
//! - Every collection/input has an explicit bound; every counter is checked.
//! - Unknown configuration keys, unsupported model parameters, foreign
//!   credential handles and private-LAN endpoints fail loudly before any
//!   state is persisted.

pub mod browser;
pub mod config;
pub mod config_consumers;
pub mod context;
pub mod customization;
pub mod db;
pub mod diagnostics;
pub mod instructions;
pub mod models;
pub mod policy;
pub mod presets;
pub mod providers;
pub mod review;
pub mod runtime_trust;
pub mod session_meta;
pub mod session_settings;
pub mod vault;

use serde::{Deserialize, Serialize};

pub use browser::{
    BrowserAdapter, BrowserAdapterDraft, BrowserArtifact, BrowserMode, BrowserObservation,
    BrowserService, BrowserSession, BrowserSessionState,
};
pub use config::{
    ApplyState, ApplyTiming, Change, CommitReceipt, ConfigBundle, ConfigService, Descriptor,
    EffectiveValue, FieldError, Patch, Scope, Snapshot, ValueKind,
};
pub use context::{Basis, ContextStore, ContextUsage, Manifest, Source};
pub use customization::{
    CustomizationService, HookDraft, HookRecord, McpServerDraft, McpServerRecord, SkillDraft,
    SkillRecord,
};
pub use db::FoundationDb;
pub use diagnostics::{export as export_diagnostics, DiagnosticBundle, DiagnosticsFacts};
pub use instructions::{
    Activation, BundleConflict, BundleSource, ImportReport, InstructionBinding, InstructionBundle,
    InstructionDraft, InstructionKind, InstructionOrigin, InstructionRecord, InstructionService,
    TokenEstimate,
};
pub use models::{
    compile_effective_request, tuning_descriptor, CallReceipt, CallReceiptStore, EffectiveRequest,
    ModelTuningDescriptor, ParameterDescriptor,
};
pub use policy::{Action, Decision, PolicyService, Rule, RuleDraft, RuleScope, Verdict};
pub use presets::{
    DelegationPolicy, FallbackCandidate, PresetBinding, PresetDraft, PresetEnvelope, PresetMode,
    PresetOrigin, PresetRecord, PresetService, PresetTools, RoleModel, RoleTokenEstimate,
    ToolResolution,
};
pub use providers::{
    ConnectionDraft, HealthState, ProviderConnection, ProviderFamily, ProviderHealth,
    ProviderService,
};
pub use review::{ReviewComment, ReviewCommentDraft, ReviewCommentPatch, ReviewService};
pub use runtime_trust::{RuntimeTrustService, TrustRecord, TrustReport, TrustState};
pub use session_meta::{SessionMeta, SessionMetaPatch, SessionMetaService};
pub use session_settings::SessionSettingsService;
pub use vault::{SecretRef, SecretVault, VerificationState};

/// Typed foundation failures. HTTP adapters map these to exact statuses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FoundationError {
    #[error("store: {0}")]
    Store(String),
    #[error("corrupt: {0}")]
    Corrupt(String),
    #[error("malformed: {0}")]
    Malformed(String),
    #[error("not_found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("oversized: {0}")]
    Oversized(String),
    #[error("validation: {0:?}")]
    Validation(Vec<FieldError>),
}

impl FoundationError {
    pub fn code(&self) -> &'static str {
        match self {
            FoundationError::Store(_) => "store",
            FoundationError::Corrupt(_) => "corrupt",
            FoundationError::Malformed(_) => "malformed",
            FoundationError::NotFound(_) => "not_found",
            FoundationError::Conflict(_) => "conflict",
            FoundationError::Oversized(_) => "oversized",
            FoundationError::Validation(_) => "validation",
        }
    }

    pub fn http_status(&self) -> u16 {
        match self {
            FoundationError::Store(_) | FoundationError::Corrupt(_) => 500,
            FoundationError::Malformed(_) | FoundationError::Oversized(_) => 400,
            FoundationError::NotFound(_) => 404,
            FoundationError::Conflict(_) | FoundationError::Validation(_) => 409,
        }
    }

    pub fn field_errors(&self) -> &[FieldError] {
        match self {
            FoundationError::Validation(errors) => errors,
            _ => &[],
        }
    }
}

/// Checked conversion of one already-bounded page limit into SQLite's
/// signed range; an out-of-range limit is a typed oversized refusal, never a
/// wrapping cast.
pub(crate) fn page_limit_i64(limit: usize, what: &str) -> Result<i64, FoundationError> {
    i64::try_from(limit)
        .map_err(|_| FoundationError::Oversized(format!("{what} page exceeds SQLite signed range")))
}

/// True when `text` carries a recognizable secret from the daemon's default
/// secret-pattern set (`sk-…`, `ghp_…`, AWS/Slack/Google keys, PEM private
/// keys). Credential-adjacent metadata (labels, provider endpoints/proxies)
/// is REFUSED when this returns true: metadata is served, exported and
/// logged, so a secret pasted there would otherwise live outside the vault.
pub(crate) fn contains_secret_material(text: &str) -> bool {
    !faktor_security::scan_secrets(text, &faktor_security::SecretPolicy::default()).is_empty()
}

/// Lowercase hex blake3 digest.
pub fn hash_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// The bundle of foundation services over one database.
#[derive(Clone)]
pub struct Foundation {
    pub db: FoundationDb,
    pub config: std::sync::Arc<ConfigService>,
    pub browser: std::sync::Arc<BrowserService>,
    pub vault: std::sync::Arc<SecretVault>,
    pub providers: std::sync::Arc<ProviderService>,
    pub receipts: std::sync::Arc<CallReceiptStore>,
    pub context: std::sync::Arc<ContextStore>,
    pub policy: std::sync::Arc<PolicyService>,
    pub customization: std::sync::Arc<CustomizationService>,
    pub instructions: std::sync::Arc<InstructionService>,
    pub presets: std::sync::Arc<PresetService>,
    pub runtime_trust: std::sync::Arc<RuntimeTrustService>,
    pub session_meta: std::sync::Arc<SessionMetaService>,
    pub review: std::sync::Arc<ReviewService>,
    pub session_settings: std::sync::Arc<SessionSettingsService>,
}

impl Foundation {
    /// Open the foundation database at `<data_dir>/foundation.db` and build
    /// every service over the SAME handle. The default diagnostic bound is
    /// read from the configuration snapshot at call time.
    pub fn open(data_dir: impl AsRef<std::path::Path>) -> Result<Self, FoundationError> {
        let db = FoundationDb::open(data_dir.as_ref().join("foundation.db"))?;
        let vault = std::sync::Arc::new(SecretVault::new(db.clone(), data_dir.as_ref()));
        let config = std::sync::Arc::new(ConfigService::new(db.clone()));
        let providers = std::sync::Arc::new(ProviderService::new(db.clone(), (*vault).clone()));
        Ok(Self {
            browser: std::sync::Arc::new(BrowserService::new(db.clone())),
            config,
            vault,
            providers,
            receipts: std::sync::Arc::new(CallReceiptStore::new(db.clone())),
            context: std::sync::Arc::new(ContextStore::new(db.clone())),
            policy: std::sync::Arc::new(PolicyService::new(db.clone())),
            customization: std::sync::Arc::new(CustomizationService::new(db.clone())),
            instructions: std::sync::Arc::new(InstructionService::new(db.clone())),
            presets: std::sync::Arc::new(PresetService::new(db.clone())),
            runtime_trust: std::sync::Arc::new(RuntimeTrustService::new(db.clone())),
            session_meta: std::sync::Arc::new(SessionMetaService::new(db.clone())),
            review: std::sync::Arc::new(ReviewService::new(db.clone())),
            session_settings: std::sync::Arc::new(SessionSettingsService::new(db.clone())),
            db,
        })
    }

    /// Effective diagnostics byte cap (clamped by the exporter).
    pub fn diagnostics_cap(&self) -> usize {
        let configured = self
            .config
            .u64_value("faktor.diagnostics.exportMaxBytes")
            .ok()
            .flatten()
            .unwrap_or(262_144) as usize;
        configured.clamp(diagnostics::MIN_BUNDLE_BYTES, diagnostics::MAX_BUNDLE_BYTES)
    }
}
