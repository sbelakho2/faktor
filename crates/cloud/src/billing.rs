//! Wave 3 commercial metering domain: the append-only usage ledger, the
//! BillingAccount / Subscription / Plan / EntitlementSnapshot / Quota /
//! Credit model, and the pure folds over both.
//!
//! Authority rules (locked by the adversarial tests in this module and in
//! `tests/billing.rs`):
//!
//! - the internal reservation/settlement ledger of a session
//!   (`faktor-session` plus `faktor-store`) is the SOURCE of truth for
//!   spend. A [`UsageEvent`] row is a PROJECTION of that ledger — never a
//!   second authority. Ingestion is idempotent per
//!   `(organization, source_key)`, so re-reading the same reservation rows
//!   appends nothing;
//! - [`UsageEvent`] rows are APPEND-ONLY: a correction is a NEW event whose
//!   `correction_of` names the corrected one; the corrected row is never
//!   mutated. The fold treats a superseded event as replaced by its
//!   correction (the last correction of a base event wins);
//! - BYOK and Faktor-managed provider expenditure are accounted separately by
//!   the mandatory `category` field (enforced on write) and the fold reports
//!   both sums independently. Only managed events ever touch the credit
//!   ledger;
//! - plans/features/limits come EXCLUSIVELY from [`BillingConfig`]. There is
//!   no price constant anywhere in this crate: a plan's numbers (token and
//!   spend limits) and the managed-provider set are operator configuration;
//! - a credit entry's `amount_micro` is bounded by
//!   [`MAX_CREDIT_AMOUNT_MICRO`] (`i64::MAX` micro-units) so its durable
//!   signed-INTEGER projection is EXACT: the JSON payload and the SQL column
//!   can never disagree, and no monetary read/aggregation/audit/ordering
//!   path ever has to clamp an amount.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::ControlPlaneError;
use crate::ids::{
    BillingAccountId, CreditEntryId, InFlightTxnId, OrganizationId, SubscriptionId, UsageEventId,
};

/// Bound on one bounded text field of a usage/credit row (provider, model,
/// unit tag, source operation).
pub const MAX_USAGE_TEXT: usize = 256;
/// Bound on one reason/rationale text.
pub const MAX_USAGE_REASON: usize = 1024;
/// Bound on one deterministic ingestion key (`reservation:<id>:<unit>`).
pub const MAX_SOURCE_KEY_BYTES: usize = 256;
/// Hard page cap for usage/credit listings.
pub const MAX_USAGE_PAGE: usize = 200;
/// Cap on the number of events one fold reads (bounded everything: a flood
/// past this bound refuses loudly instead of silently folding a prefix).
pub const MAX_FOLD_EVENTS: usize = 100_000;
/// Bound on one plan's limit table.
pub const MAX_PLAN_LIMITS: usize = 32;
/// Bound on one plan's feature set.
pub const MAX_PLAN_FEATURES: usize = 32;
/// Bound on the configured plan table.
pub const MAX_PLANS: usize = 64;

// ------------------------------------------------------------- vocabulary

/// The unit of one usage event. Token categories are recorded separately so
/// the fold reports inputs / outputs / cache / reasoning independently; the
/// provider-cost unit carries the money of one settlement (quantity 1).
pub const UNIT_INPUT_TOKENS: &str = "input_tokens";
pub const UNIT_OUTPUT_TOKENS: &str = "output_tokens";
pub const UNIT_CACHE_READ_TOKENS: &str = "cache_read_tokens";
pub const UNIT_CACHE_WRITE_TOKENS: &str = "cache_write_tokens";
pub const UNIT_REASONING_TOKENS: &str = "reasoning_tokens";
pub const UNIT_PROVIDER_COST: &str = "provider_cost_micro";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageUnit {
    InputTokens,
    OutputTokens,
    CacheReadTokens,
    CacheWriteTokens,
    ReasoningTokens,
    ProviderCostMicro,
}

impl UsageUnit {
    pub const fn as_str(self) -> &'static str {
        match self {
            UsageUnit::InputTokens => UNIT_INPUT_TOKENS,
            UsageUnit::OutputTokens => UNIT_OUTPUT_TOKENS,
            UsageUnit::CacheReadTokens => UNIT_CACHE_READ_TOKENS,
            UsageUnit::CacheWriteTokens => UNIT_CACHE_WRITE_TOKENS,
            UsageUnit::ReasoningTokens => UNIT_REASONING_TOKENS,
            UsageUnit::ProviderCostMicro => UNIT_PROVIDER_COST,
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            UNIT_INPUT_TOKENS => UsageUnit::InputTokens,
            UNIT_OUTPUT_TOKENS => UsageUnit::OutputTokens,
            UNIT_CACHE_READ_TOKENS => UsageUnit::CacheReadTokens,
            UNIT_CACHE_WRITE_TOKENS => UsageUnit::CacheWriteTokens,
            UNIT_REASONING_TOKENS => UsageUnit::ReasoningTokens,
            UNIT_PROVIDER_COST => UsageUnit::ProviderCostMicro,
            _ => return None,
        })
    }

    /// Whether the unit counts tokens (quantity x price is never computed
    /// here: prices live in provider config, not in the metering layer).
    pub const fn is_tokens(self) -> bool {
        !matches!(self, UsageUnit::ProviderCostMicro)
    }
}

/// Whose money a provider call spent: the Faktor-managed account (debits
/// credits/quota) or the organization's own provider key (BYOK — recorded,
/// never debited).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpendCategory {
    Managed,
    Byok,
}

impl SpendCategory {
    pub const fn as_str(self) -> &'static str {
        match self {
            SpendCategory::Managed => "managed",
            SpendCategory::Byok => "byok",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "managed" => SpendCategory::Managed,
            "byok" => SpendCategory::Byok,
            _ => return None,
        })
    }
}

/// The durable reconciliation state of one usage event. A correction is
/// itself an event; a base event becomes `corrected` in the FOLD when a
/// correction names it (never by mutating its row).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationState {
    Pending,
    Reconciled,
    Corrected,
}

impl ReconciliationState {
    pub const fn as_str(self) -> &'static str {
        match self {
            ReconciliationState::Pending => "pending",
            ReconciliationState::Reconciled => "reconciled",
            ReconciliationState::Corrected => "corrected",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "pending" => ReconciliationState::Pending,
            "reconciled" => ReconciliationState::Reconciled,
            "corrected" => ReconciliationState::Corrected,
            _ => return None,
        })
    }
}

// ------------------------------------------------------- task-id identity

/// The canonical durable/storage encoding of a [`UsageEvent::task_id`]:
/// fixed-width, zero-padded, lowercase 16-hex-digit text
/// (`format!("{task_id:016x}")`).
///
/// Why the encoding exists (P1 identity-integrity): SQLite `INTEGER` is
/// signed 64-bit, and the original storage path wrote
/// `task_id.min(i64::MAX as u64) as i64`. Every id in
/// `[i64::MAX, u64::MAX]` therefore collapsed onto the single indexed value
/// `i64::MAX`: `0x7fff_ffff_ffff_ffff`, `0x8000_0000_0000_0000` and
/// `0xffff_ffff_ffff_ffff` all became the same row key, while the JSON
/// payload still reported the original id. A task-filtered query for one
/// high id could then return another high id's rows — two distinct tasks
/// aliased onto one ledger slice. `{:016x}` is a bijection over the WHOLE
/// u64 domain (no clamping, no sign, no float), compares as ordinary text
/// (no unsigned ordering is required since the column is used only for
/// equality filters), and keeps the `(organization_id, task_id, event_seq)`
/// index applicable.
///
/// Invariant: every write to and filter over the stored `task_id` column
/// goes through this pair ([`task_id_text`] / [`task_id_from_text`]); no SQL
/// path may compare a raw numeric task id against the column.
pub fn task_id_text(task_id: u64) -> String {
    format!("{task_id:016x}")
}

/// The exact inverse of [`task_id_text`]: `Some` only for 16 lowercase hex
/// digits (the SQL CHECK constraint enforces the same shape); anything else
/// is `None`, never a silently truncated id.
pub fn task_id_from_text(raw: &str) -> Option<u64> {
    if raw.len() != 16 || !raw.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    u64::from_str_radix(raw, 16).ok()
}

// ------------------------------------------------------------ usage event

/// One append-only usage ledger row: the exact projection of one durable
/// reservation/settlement fact (or one manual correction of it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageEvent {
    pub id: UsageEventId,
    pub organization_id: OrganizationId,
    pub billing_account_id: BillingAccountId,
    pub task_id: u64,
    pub run_id: String,
    pub attempt_id: String,
    pub provider: String,
    pub model: String,
    pub unit: UsageUnit,
    pub quantity: u64,
    /// The provider cost of this event in microUSD. Enforced on write: it is
    /// non-zero only on [`UsageUnit::ProviderCostMicro`] rows, so token rows
    /// can never smuggle money into the aggregate.
    pub provider_cost_micro: u64,
    pub source_operation: String,
    pub occurred_at_ms: i64,
    pub reconciliation_state: ReconciliationState,
    /// `Some(base)` makes this event a CORRECTION of a base event (the base
    /// row is never mutated; a correction never corrects another correction).
    pub correction_of: Option<UsageEventId>,
    pub category: SpendCategory,
    /// The deterministic ingestion key of the source fact
    /// (`reservation:<id>:<unit>`). Unique per organization: re-ingesting
    /// the same durable rows appends nothing.
    pub source_key: String,
}

fn bounded(kind: &str, value: &str, max: usize) -> Result<(), ControlPlaneError> {
    if value.is_empty() || value.len() > max {
        return Err(ControlPlaneError::Malformed(format!(
            "{kind} must be 1..={max} bytes"
        )));
    }
    if value.bytes().any(|b| b.is_ascii_control()) {
        return Err(ControlPlaneError::Malformed(format!(
            "{kind} must not contain control characters"
        )));
    }
    Ok(())
}

impl UsageEvent {
    /// The write-time shape rules. A hostile event is refused before any
    /// credit/quota side effect (category enforcement is enforced here).
    pub fn validate(&self) -> Result<(), ControlPlaneError> {
        bounded("run id", &self.run_id, MAX_USAGE_TEXT)?;
        bounded("attempt id", &self.attempt_id, MAX_USAGE_TEXT)?;
        bounded("provider", &self.provider, MAX_USAGE_TEXT)?;
        bounded("model", &self.model, MAX_USAGE_TEXT)?;
        bounded("source operation", &self.source_operation, MAX_USAGE_TEXT)?;
        bounded("source key", &self.source_key, MAX_SOURCE_KEY_BYTES)?;
        if self.task_id == 0 {
            return Err(ControlPlaneError::Malformed(
                "a usage event must carry a non-zero task id".into(),
            ));
        }
        match self.unit {
            UsageUnit::ProviderCostMicro => {
                if self.quantity != 1 {
                    return Err(ControlPlaneError::Malformed(
                        "a provider-cost event carries quantity 1".into(),
                    ));
                }
                if self.provider_cost_micro == 0 {
                    return Err(ControlPlaneError::Malformed(
                        "a provider-cost event must carry a non-zero cost".into(),
                    ));
                }
                if self.correction_of.is_none()
                    && self.reconciliation_state == ReconciliationState::Corrected
                {
                    return Err(ControlPlaneError::Malformed(
                        "a non-correction event can never be corrected on write; a correction is a new event"
                            .into(),
                    ));
                }
            }
            _ => {
                if self.provider_cost_micro != 0 {
                    return Err(ControlPlaneError::Malformed(
                        "a token event must carry provider_cost 0 (money rides the provider-cost unit)"
                            .into(),
                    ));
                }
                if self.quantity == 0 {
                    return Err(ControlPlaneError::Malformed(
                        "a token event must carry a non-zero quantity".into(),
                    ));
                }
                if self.reconciliation_state == ReconciliationState::Corrected {
                    return Err(ControlPlaneError::Malformed(
                        "a token event is never written corrected; corrections are provider-cost events"
                            .into(),
                    ));
                }
            }
        }
        if self.correction_of.as_ref() == Some(&self.id) {
            return Err(ControlPlaneError::Malformed(
                "an event can never correct itself".into(),
            ));
        }
        if self.correction_of.is_some() && self.reconciliation_state == ReconciliationState::Pending
        {
            return Err(ControlPlaneError::Malformed(
                "a correction is never pending: it is the reconciled/corrected result".into(),
            ));
        }
        Ok(())
    }

    /// The current durable state of a base event given every event of the
    /// organization: the LATEST correction naming it (by `occurred_at_ms`,
    /// then id) decides; a superseded base event reports `Corrected`.
    pub fn effective_state(event: &UsageEvent, all: &[UsageEvent]) -> ReconciliationState {
        let mut latest: Option<&UsageEvent> = None;
        for candidate in all {
            if candidate.correction_of.as_ref() != Some(&event.id) {
                continue;
            }
            let newer = match latest {
                None => true,
                Some(current) => {
                    (candidate.occurred_at_ms, candidate.id.as_str())
                        > (current.occurred_at_ms, current.id.as_str())
                }
            };
            if newer {
                latest = Some(candidate);
            }
        }
        match latest {
            Some(correction) => correction.reconciliation_state,
            None => event.reconciliation_state,
        }
    }
}

// ---------------------------------------------------------- billing model

/// One organization billing account. `managed` enabled means Faktor-managed
/// provider spend is permitted for this account (and debits credits); a
/// BYOK-dedicated account (`managed == false`) refuses managed usage writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BillingAccount {
    pub id: BillingAccountId,
    pub organization: OrganizationId,
    pub name: String,
    pub managed: bool,
    pub created_ms: i64,
    pub disabled: bool,
}

impl BillingAccount {
    pub fn validate(&self) -> Result<(), ControlPlaneError> {
        bounded("billing account name", &self.name, MAX_USAGE_TEXT)
    }

    /// Enforce the category against the account on write: a BYOK-only
    /// account can never record a managed event (nothing else would ever
    /// debit the organization).
    pub fn permits(&self, category: SpendCategory) -> bool {
        !self.disabled && (self.managed || category == SpendCategory::Byok)
    }
}

/// The subscription lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionStatus {
    Active,
    Expired,
    Canceled,
}

impl SubscriptionStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            SubscriptionStatus::Active => "active",
            SubscriptionStatus::Expired => "expired",
            SubscriptionStatus::Canceled => "canceled",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "active" => SubscriptionStatus::Active,
            "expired" => SubscriptionStatus::Expired,
            "canceled" => SubscriptionStatus::Canceled,
            _ => return None,
        })
    }
}

/// One organization subscription. Expiry is derived from the durable
/// `expires_ms` (a stale `Active` row past its expiry is INACTIVE), never
/// from a background sweeper that could interrupt an in-flight transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subscription {
    pub id: SubscriptionId,
    pub organization: OrganizationId,
    pub plan_id: String,
    pub status: SubscriptionStatus,
    pub started_ms: i64,
    pub expires_ms: Option<i64>,
    pub updated_ms: i64,
}

impl Subscription {
    pub fn is_active_at(&self, now_ms: i64) -> bool {
        self.status == SubscriptionStatus::Active
            && self.expires_ms.map(|e| now_ms <= e).unwrap_or(true)
    }

    pub fn validate(&self) -> Result<(), ControlPlaneError> {
        bounded("plan id", &self.plan_id, MAX_USAGE_TEXT)
    }
}

/// Legal plan feature tags.
pub const FEATURE_MANAGED_PROVIDERS: &str = "managed_providers";
pub const FEATURE_BYOK: &str = "byok";
pub const FEATURE_CREDITS: &str = "credits";
/// Every legal feature tag (strict: an unknown tag is a config error).
pub const ALL_FEATURES: &[&str] = &[FEATURE_MANAGED_PROVIDERS, FEATURE_BYOK, FEATURE_CREDITS];

/// Legal plan limit names.
pub const LIMIT_MAX_ACTIVE_TASKS: &str = "max_active_tasks";
pub const LIMIT_MAX_CHILDREN_PER_TASK: &str = "max_children_per_task";
pub const LIMIT_MAX_PROVIDER_ATTEMPTS_PER_TASK: &str = "max_provider_attempts_per_task";
pub const LIMIT_MAX_MANAGED_SPEND_MICRO_PER_PERIOD: &str = "max_managed_spend_micro_per_period";
pub const LIMIT_MAX_TOKENS_PER_PERIOD: &str = "max_tokens_per_period";
pub const LIMIT_MIN_CREDIT_BALANCE_MICRO: &str = "min_credit_balance_micro";
/// Every legal limit name (strict: an unknown name is a config error).
pub const ALL_LIMITS: &[&str] = &[
    LIMIT_MAX_ACTIVE_TASKS,
    LIMIT_MAX_CHILDREN_PER_TASK,
    LIMIT_MAX_PROVIDER_ATTEMPTS_PER_TASK,
    LIMIT_MAX_MANAGED_SPEND_MICRO_PER_PERIOD,
    LIMIT_MAX_TOKENS_PER_PERIOD,
    LIMIT_MIN_CREDIT_BALANCE_MICRO,
];

/// One plan, CONFIG-PROVIDED: features and integer limits only. There is no
/// price field anywhere — money limits are operator numbers, prices are
/// provider configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanConfig {
    pub plan_id: String,
    #[serde(default)]
    pub features: BTreeSet<String>,
    #[serde(default)]
    pub limits: BTreeMap<String, u64>,
}

impl PlanConfig {
    pub fn validate(&self) -> Result<(), ControlPlaneError> {
        bounded("plan id", &self.plan_id, MAX_USAGE_TEXT)?;
        if self.features.len() > MAX_PLAN_FEATURES {
            return Err(ControlPlaneError::Malformed(format!(
                "plan {} carries more than {MAX_PLAN_FEATURES} features",
                self.plan_id
            )));
        }
        for feature in &self.features {
            if !ALL_FEATURES.contains(&feature.as_str()) {
                return Err(ControlPlaneError::Malformed(format!(
                    "plan {} names the unknown feature {feature:?}",
                    self.plan_id
                )));
            }
        }
        if self.limits.len() > MAX_PLAN_LIMITS {
            return Err(ControlPlaneError::Malformed(format!(
                "plan {} carries more than {MAX_PLAN_LIMITS} limits",
                self.plan_id
            )));
        }
        for limit in self.limits.keys() {
            if !ALL_LIMITS.contains(&limit.as_str()) {
                return Err(ControlPlaneError::Malformed(format!(
                    "plan {} names the unknown limit {limit:?}",
                    self.plan_id
                )));
            }
        }
        Ok(())
    }
}

/// The strict billing/plans configuration (`[billing]`). Plans are the ONLY
/// source of features/limits; the managed-provider set is the ONLY source of
/// the managed/BYOK decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct BillingConfig {
    #[serde(default)]
    pub default_plan: Option<String>,
    #[serde(default)]
    pub plans: BTreeMap<String, PlanConfig>,
    #[serde(default)]
    pub managed_providers: BTreeSet<String>,
}

impl BillingConfig {
    /// Strict validation: every table key equals its plan's `plan_id`, the
    /// default plan exists, no unknown feature/limit tag, bounded sizes.
    pub fn validate(&self) -> Result<(), ControlPlaneError> {
        if self.plans.len() > MAX_PLANS {
            return Err(ControlPlaneError::Malformed(format!(
                "billing config carries more than {MAX_PLANS} plans"
            )));
        }
        for (key, plan) in &self.plans {
            plan.validate()?;
            if key != &plan.plan_id {
                return Err(ControlPlaneError::Malformed(format!(
                    "plan table key {key:?} does not match plan_id {:?}",
                    plan.plan_id
                )));
            }
        }
        if let Some(default) = &self.default_plan {
            bounded("default plan", default, MAX_USAGE_TEXT)?;
            if !self.plans.contains_key(default) {
                return Err(ControlPlaneError::Malformed(format!(
                    "default plan {default:?} is not defined in the plan table"
                )));
            }
        }
        for provider in &self.managed_providers {
            bounded("managed provider", provider, MAX_USAGE_TEXT)?;
        }
        Ok(())
    }

    pub fn plan(&self, plan_id: &str) -> Option<&PlanConfig> {
        self.plans.get(plan_id)
    }

    /// The category of one provider's spend under this configuration.
    pub fn category_of(&self, provider: &str) -> SpendCategory {
        if self.managed_providers.contains(provider) {
            SpendCategory::Managed
        } else {
            SpendCategory::Byok
        }
    }
}

// -------------------------------------------------------------- snapshot

/// The free/held credit picture of one organization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct CreditBalance {
    pub granted_micro: u64,
    /// Every consume's EFFECTIVE debit: a settled consume at its settled
    /// actual, a still-pending consume at its held estimate.
    pub consumed_micro: u64,
    pub refunded_micro: u64,
    /// Pending consumes (record-before-call rows not yet settled/refunded)
    /// still HOLD their amount: they are never silently released.
    pub held_micro: u64,
    pub pending_consumes: u64,
}

impl CreditBalance {
    /// The free balance: grants + refunds minus the effective debits (a
    /// pending consume's hold is already part of its debit, so it is never
    /// double counted).
    pub fn balance_micro(&self) -> u64 {
        self.granted_micro
            .saturating_add(self.refunded_micro)
            .saturating_sub(self.consumed_micro)
    }
}

/// The durable view of one in-flight integration/rollback/completion
/// transaction: while one is open, entitlement changes can never interrupt
/// it (the gate is only consulted at admission boundaries and always admits
/// a continuation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InFlightTxn {
    pub id: InFlightTxnId,
    pub organization: OrganizationId,
    pub kind: InFlightKind,
    pub reference: String,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InFlightKind {
    Integration,
    Rollback,
    Completion,
}

impl InFlightKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            InFlightKind::Integration => "integration",
            InFlightKind::Rollback => "rollback",
            InFlightKind::Completion => "completion",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "integration" => InFlightKind::Integration,
            "rollback" => InFlightKind::Rollback,
            "completion" => InFlightKind::Completion,
            _ => return None,
        })
    }
}

/// The effective entitlement picture of one organization, DERIVED (never
/// stored): subscription + plan config + credit balance + folded usage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EntitlementSnapshot {
    pub organization_id: OrganizationId,
    pub billing_account_id: Option<BillingAccountId>,
    pub plan_id: Option<String>,
    /// False when the subscription names a plan the config does not define:
    /// admission then denies naming the plan gap (never a silent default).
    pub plan_found: bool,
    pub subscription_status: Option<SubscriptionStatus>,
    pub subscription_expires_ms: Option<i64>,
    pub subscription_active: bool,
    pub features: BTreeSet<String>,
    pub limits: BTreeMap<String, u64>,
    pub credits: CreditBalance,
    /// Folded managed vs BYOK spend (projection of the reservation ledger).
    pub managed_spend_micro: u64,
    pub byok_spend_micro: u64,
    pub total_tokens: u64,
    pub in_flight: Vec<InFlightTxn>,
    pub now_ms: i64,
}

impl EntitlementSnapshot {
    pub fn limit(&self, name: &str) -> Option<u64> {
        self.limits.get(name).copied()
    }

    pub fn has_feature(&self, feature: &str) -> bool {
        self.features.contains(feature)
    }
}

/// One admission boundary. Only the first three are GATED; the continuation
/// boundaries always admit (an entitlement change never interrupts an
/// in-flight integration/rollback/completion transaction).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionBoundary {
    NewTask,
    NewChildSpawn,
    NewProviderAttempt,
    IntegrationContinuation,
    RollbackContinuation,
    CompletionContinuation,
}

impl AdmissionBoundary {
    pub const fn as_str(self) -> &'static str {
        match self {
            AdmissionBoundary::NewTask => "new_task",
            AdmissionBoundary::NewChildSpawn => "new_child_spawn",
            AdmissionBoundary::NewProviderAttempt => "new_provider_attempt",
            AdmissionBoundary::IntegrationContinuation => "integration_continuation",
            AdmissionBoundary::RollbackContinuation => "rollback_continuation",
            AdmissionBoundary::CompletionContinuation => "completion_continuation",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "new_task" => AdmissionBoundary::NewTask,
            "new_child_spawn" => AdmissionBoundary::NewChildSpawn,
            "new_provider_attempt" => AdmissionBoundary::NewProviderAttempt,
            "integration_continuation" => AdmissionBoundary::IntegrationContinuation,
            "rollback_continuation" => AdmissionBoundary::RollbackContinuation,
            "completion_continuation" => AdmissionBoundary::CompletionContinuation,
            _ => return None,
        })
    }

    /// Gated boundaries consult the entitlement snapshot; continuations are
    /// never gated (the in-flight invariant).
    pub const fn is_gated(self) -> bool {
        matches!(
            self,
            AdmissionBoundary::NewTask
                | AdmissionBoundary::NewChildSpawn
                | AdmissionBoundary::NewProviderAttempt
        )
    }
}

impl std::fmt::Display for AdmissionBoundary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The observed counters of one admission request. Counters are supplied by
/// the caller (the executor knows its live child/attempt counts); the spend
/// and token counters are ALWAYS derived from the folded usage ledger by the
/// service, never trusted from the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedUsage {
    #[serde(default)]
    pub active_tasks: u64,
    #[serde(default)]
    pub children_of_task: u64,
    #[serde(default)]
    pub provider_attempts_of_task: u64,
    /// The estimated managed cost of the pending attempt (checked against
    /// the remaining credit balance and the managed-spend limit).
    #[serde(default)]
    pub estimated_provider_cost_micro: u64,
}

/// One admission request: the boundary plus the observed counters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionRequest {
    pub boundary: AdmissionBoundary,
    pub task_id: Option<u64>,
    pub provider: Option<String>,
    #[serde(default)]
    pub observed: ObservedUsage,
}

impl AdmissionRequest {
    pub fn boundary(boundary: AdmissionBoundary) -> Self {
        Self {
            boundary,
            task_id: None,
            provider: None,
            observed: ObservedUsage::default(),
        }
    }

    pub fn validate(&self) -> Result<(), ControlPlaneError> {
        if let Some(task_id) = self.task_id {
            if task_id == 0 {
                return Err(ControlPlaneError::Malformed(
                    "an admission request task id cannot be 0".into(),
                ));
            }
        }
        if let Some(provider) = &self.provider {
            bounded("provider", provider, MAX_USAGE_TEXT)?;
        }
        Ok(())
    }
}

/// The typed refusal of one admission: ALWAYS names the exact limit (or the
/// subscription/plan state) that refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "entitlement exceeded at {boundary}: limit {limit} (value {limit_value:?}, observed {observed}); \
     nothing was admitted (in-flight transactions are never interrupted)"
)]
pub struct EntitlementExceeded {
    pub boundary: AdmissionBoundary,
    /// The exact limit name (`max_active_tasks`, ...) or the state cause
    /// (`subscription_active`, `plan`, `credits`).
    pub limit: String,
    pub limit_value: Option<u64>,
    pub observed: u64,
}

impl EntitlementExceeded {
    pub fn of(boundary: AdmissionBoundary, limit: &str) -> Self {
        Self {
            boundary,
            limit: limit.to_string(),
            limit_value: None,
            observed: 0,
        }
    }

    pub fn with_value(mut self, limit_value: u64, observed: u64) -> Self {
        self.limit_value = Some(limit_value);
        self.observed = observed;
        self
    }
}

/// The cause tags used by refusals that are not plan limits.
pub const CAUSE_SUBSCRIPTION_ACTIVE: &str = "subscription_active";
pub const CAUSE_PLAN: &str = "plan";
pub const CAUSE_CREDITS: &str = "credits";
pub const CAUSE_FEATURE_MANAGED: &str = "feature_managed_providers";

/// The outcome of one admission decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// A new task/child/attempt was admitted under the snapshot.
    Admitted,
    /// A continuation of an in-flight transaction: always admitted.
    InFlightContinuation,
}

// ----------------------------------------------------------------- credit

/// The maximum legal `amount_micro` of one credit ledger entry: `i64::MAX`
/// micro-units (~9.22e18 micro-units, ~9.22e12 USD in the ledger's microUSD
/// unit).
///
/// This is a DOMAIN invariant, not a formatting choice. The durable SQLite
/// projection of [`CreditEntry::amount_micro`] is a signed 64-bit `INTEGER`
/// column, and a monetary ledger must never persist a value it cannot read
/// back exactly. The old writer persisted
/// `amount_micro.min(i64::MAX as u64) as i64`: `i64::MAX`, `i64::MAX + 1`
/// and `u64::MAX` all stored the SAME row value while the JSON payload kept
/// the true amount, so the column and the payload could disagree and any SQL
/// aggregation/audit/balance/reconciliation/ordering over the column would
/// silently use the clamped number. Bounding the domain at `i64::MAX` makes
/// the projection exact for every legal value (no clamp, no lossy cast), so
/// payload and column are bit-identical by construction.
pub const MAX_CREDIT_AMOUNT_MICRO: u64 = i64::MAX as u64;

/// The one write-time amount rule of the credit ledger (the domain bound is
/// documented on [`MAX_CREDIT_AMOUNT_MICRO`]): zero is refused (every entry
/// moves a non-zero amount) and anything above the bound is refused TYPED,
/// naming the field and the limit — never clamped, never truncated.
pub fn validate_credit_amount_micro(amount: u64) -> Result<(), ControlPlaneError> {
    if amount == 0 {
        return Err(ControlPlaneError::Malformed(
            "a credit entry must carry a non-zero amount".into(),
        ));
    }
    if amount > MAX_CREDIT_AMOUNT_MICRO {
        return Err(ControlPlaneError::Malformed(format!(
            "credit entry amount_micro {amount} exceeds the maximum {MAX_CREDIT_AMOUNT_MICRO} \
             micro-units (i64::MAX): the durable ledger column is a signed 64-bit INTEGER and a \
             larger amount cannot be stored without loss"
        )));
    }
    Ok(())
}

/// The credit ledger entry kind. Every entry is a NEW append-only row; a
/// settle/refund never mutates the consume it refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CreditKind {
    /// Increase the balance (operator/admin grant).
    Grant,
    /// Debit at admission time, BEFORE the provider call (record-before-call).
    Consume,
    /// Settle one pending consume at its actual spend (exactly once).
    Settle,
    /// Refund a consume/its settled actual (a credit EVENT, append-only).
    Refund,
}

impl CreditKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            CreditKind::Grant => "grant",
            CreditKind::Consume => "consume",
            CreditKind::Settle => "settle",
            CreditKind::Refund => "refund",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "grant" => CreditKind::Grant,
            "consume" => CreditKind::Consume,
            "settle" => CreditKind::Settle,
            "refund" => CreditKind::Refund,
            _ => return None,
        })
    }
}

/// One durable credit ledger row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreditEntry {
    pub id: CreditEntryId,
    pub organization: OrganizationId,
    pub billing_account_id: BillingAccountId,
    pub kind: CreditKind,
    pub amount_micro: u64,
    /// The consume this settle/refund names (grants/consumes carry the
    /// source reference instead).
    pub reference: Option<CreditEntryId>,
    /// The usage event this entry accounts for (managed consumes).
    pub usage_event_id: Option<UsageEventId>,
    pub reason: String,
    pub occurred_at_ms: i64,
    /// Optional caller idempotency key (unique per organization when set).
    pub idempotency_key: Option<String>,
}

impl CreditEntry {
    pub fn validate(&self) -> Result<(), ControlPlaneError> {
        bounded("credit reason", &self.reason, MAX_USAGE_REASON)?;
        // The amount domain bound (zero rules included) is enforced HERE and
        // at every store append boundary; a larger amount is refused typed,
        // never clamped into the durable column.
        validate_credit_amount_micro(self.amount_micro)?;
        match self.kind {
            CreditKind::Grant => {
                if self.reference.is_some() {
                    return Err(ControlPlaneError::Malformed(
                        "a grant never references another credit entry".into(),
                    ));
                }
            }
            CreditKind::Consume => {
                if self.reference.is_some() {
                    return Err(ControlPlaneError::Malformed(
                        "a consume never references another credit entry".into(),
                    ));
                }
            }
            CreditKind::Settle | CreditKind::Refund => {
                if self.reference.is_none() {
                    return Err(ControlPlaneError::Malformed(format!(
                        "a {} must name the consume it applies to",
                        self.kind.as_str()
                    )));
                }
            }
        }
        if let Some(key) = &self.idempotency_key {
            bounded(
                "idempotency key",
                key,
                crate::service::MAX_IDEMPOTENCY_KEY_BYTES,
            )?;
        }
        Ok(())
    }

    /// A settle/refund must always reference a CONSUME (never a grant or
    /// another settle/refund): enforced by the store at append time.
    pub fn requires_consume_reference(&self) -> bool {
        matches!(self.kind, CreditKind::Settle | CreditKind::Refund)
    }
}

/// Fold one organization's credit entries into its balance. Settled consumes
/// count at their SETTLED actual; a still-pending consume holds its full
/// amount (never silently released). Refunds add back exactly what they
/// carry (exactness is enforced at append time against the referenced
/// consume/settle).
pub fn fold_credits(entries: &[CreditEntry]) -> CreditBalance {
    let mut balance = CreditBalance::default();
    // Settle amounts by consume id.
    let mut settled: BTreeMap<&str, u64> = BTreeMap::new();
    for entry in entries {
        if entry.kind == CreditKind::Settle {
            if let Some(reference) = &entry.reference {
                settled.insert(reference.as_str(), entry.amount_micro);
            }
        }
    }
    for entry in entries {
        match entry.kind {
            CreditKind::Grant => {
                balance.granted_micro = balance.granted_micro.saturating_add(entry.amount_micro);
            }
            CreditKind::Consume => {
                let effective = settled
                    .get(entry.id.as_str())
                    .copied()
                    .unwrap_or(entry.amount_micro);
                balance.consumed_micro = balance.consumed_micro.saturating_add(effective);
                if !settled.contains_key(entry.id.as_str()) {
                    balance.held_micro = balance.held_micro.saturating_add(entry.amount_micro);
                    balance.pending_consumes += 1;
                }
            }
            CreditKind::Settle => {}
            CreditKind::Refund => {
                balance.refunded_micro = balance.refunded_micro.saturating_add(entry.amount_micro);
            }
        }
    }
    balance
}

// ------------------------------------------------------------------- fold

/// One aggregate bucket (org totals and per-task rows share this shape).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    pub provider_cost_micro: u64,
    pub managed_cost_micro: u64,
    pub byok_cost_micro: u64,
    pub events: u64,
    /// Events whose row was superseded by a correction (they contribute
    /// nothing; the correction's values are folded instead).
    pub corrected_events: u64,
}

/// One per-task aggregate of an organization's usage fold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskUsage {
    pub task_id: u64,
    pub run_id: String,
    pub totals: UsageTotals,
}

/// The per-organization usage fold: org totals plus per-task rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UsageFold {
    pub organization_id: OrganizationId,
    pub totals: UsageTotals,
    pub per_task: Vec<TaskUsage>,
    /// Cursor for the next page when the fold hit the scan bound.
    pub next_cursor: Option<String>,
}

fn add_event(totals: &mut UsageTotals, event: &UsageEvent) {
    totals.events += 1;
    match event.unit {
        UsageUnit::InputTokens => {
            totals.input_tokens = totals.input_tokens.saturating_add(event.quantity)
        }
        UsageUnit::OutputTokens => {
            totals.output_tokens = totals.output_tokens.saturating_add(event.quantity)
        }
        UsageUnit::CacheReadTokens => {
            totals.cache_read_tokens = totals.cache_read_tokens.saturating_add(event.quantity)
        }
        UsageUnit::CacheWriteTokens => {
            totals.cache_write_tokens = totals.cache_write_tokens.saturating_add(event.quantity)
        }
        UsageUnit::ReasoningTokens => {
            totals.reasoning_tokens = totals.reasoning_tokens.saturating_add(event.quantity)
        }
        UsageUnit::ProviderCostMicro => {
            totals.provider_cost_micro = totals
                .provider_cost_micro
                .saturating_add(event.provider_cost_micro);
            match event.category {
                SpendCategory::Managed => {
                    totals.managed_cost_micro = totals
                        .managed_cost_micro
                        .saturating_add(event.provider_cost_micro)
                }
                SpendCategory::Byok => {
                    totals.byok_cost_micro = totals
                        .byok_cost_micro
                        .saturating_add(event.provider_cost_micro)
                }
            }
        }
    }
}

/// Fold one organization's usage events: superseded base events are skipped
/// (their correction contributes), tokens are summed per category and money
/// is summed per category (managed vs BYOK) independently. Corrections of
/// one base form an append-only chain: the LATEST correction wins, older
/// corrections contribute nothing (they are superseded, never deleted).
pub fn fold_usage(organization: &OrganizationId, events: &[UsageEvent]) -> UsageFold {
    // The winning correction of each base event id: latest by
    // (occurred_at_ms, id) — deterministic and stable across read order.
    let mut winning: BTreeMap<&str, &str> = BTreeMap::new();
    for event in events {
        let Some(base) = &event.correction_of else {
            continue;
        };
        let key = base.as_str();
        let candidate = (event.occurred_at_ms, event.id.as_str());
        let replace = match winning.get(key) {
            None => true,
            Some(current) => *current < candidate.1,
        };
        if replace {
            winning.insert(key, candidate.1);
        }
    }
    let mut superseded: BTreeSet<&str> = BTreeSet::new();
    for event in events {
        if let Some(base) = &event.correction_of {
            if winning.get(base.as_str()) == Some(&event.id.as_str()) {
                // The winning correction supersedes its base row.
                superseded.insert(base.as_str());
            } else {
                // An older correction of the same base is itself superseded.
                superseded.insert(event.id.as_str());
            }
        }
    }
    let mut totals = UsageTotals::default();
    let mut per_task: BTreeMap<(u64, String), UsageTotals> = BTreeMap::new();
    for event in events {
        if superseded.contains(event.id.as_str()) {
            totals.corrected_events += 1;
            per_task
                .entry((event.task_id, event.run_id.clone()))
                .or_default()
                .corrected_events += 1;
            continue;
        }
        if event.correction_of.is_some() {
            totals.corrected_events += 1;
            per_task
                .entry((event.task_id, event.run_id.clone()))
                .or_default()
                .corrected_events += 1;
        }
        add_event(&mut totals, event);
        add_event(
            per_task
                .entry((event.task_id, event.run_id.clone()))
                .or_default(),
            event,
        );
    }
    UsageFold {
        organization_id: organization.clone(),
        totals,
        per_task: per_task
            .into_iter()
            .map(|((task_id, run_id), totals)| TaskUsage {
                task_id,
                run_id,
                totals,
            })
            .collect(),
        next_cursor: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::UsageEventId;

    fn org() -> OrganizationId {
        OrganizationId::try_new("org_1").unwrap()
    }

    fn account() -> BillingAccountId {
        BillingAccountId::try_new("acct_1").unwrap()
    }

    fn token_event(
        id: &str,
        unit: UsageUnit,
        quantity: u64,
        category: SpendCategory,
    ) -> UsageEvent {
        UsageEvent {
            id: UsageEventId::try_new(id).unwrap(),
            organization_id: org(),
            billing_account_id: account(),
            task_id: 7,
            run_id: "1".into(),
            attempt_id: "11".into(),
            provider: "openai".into(),
            model: "m".into(),
            unit,
            quantity,
            provider_cost_micro: 0,
            source_operation: "session.cost_settle".into(),
            occurred_at_ms: 10,
            reconciliation_state: ReconciliationState::Reconciled,
            correction_of: None,
            category,
            source_key: format!("reservation:1:{id}"),
        }
    }

    fn cost_event(id: &str, cost: u64, category: SpendCategory) -> UsageEvent {
        UsageEvent {
            id: UsageEventId::try_new(id).unwrap(),
            organization_id: org(),
            billing_account_id: account(),
            task_id: 7,
            run_id: "1".into(),
            attempt_id: "11".into(),
            provider: "openai".into(),
            model: "m".into(),
            unit: UsageUnit::ProviderCostMicro,
            quantity: 1,
            provider_cost_micro: cost,
            source_operation: "session.cost_settle".into(),
            occurred_at_ms: 10,
            reconciliation_state: ReconciliationState::Reconciled,
            correction_of: None,
            category,
            source_key: format!("reservation:1:cost:{id}"),
        }
    }

    #[test]
    fn write_rules_refuse_money_on_token_rows_and_pending_corrections() {
        // Every refusal is the typed Malformed error naming the exact rule —
        // never a generic failure and never a silent write.
        fn refuse(event: &UsageEvent, needle: &str) {
            match event.validate() {
                Err(ControlPlaneError::Malformed(msg)) => {
                    assert!(msg.contains(needle), "refusal {msg:?} must name {needle:?}")
                }
                other => panic!("expected Malformed containing {needle:?}, got {other:?}"),
            }
        }
        let mut event = token_event("e1", UsageUnit::InputTokens, 5, SpendCategory::Byok);
        assert_eq!(event.validate(), Ok(()), "the base event is valid");
        event.provider_cost_micro = 9;
        refuse(&event, "token event must carry provider_cost 0");
        let mut event = token_event("e2", UsageUnit::InputTokens, 5, SpendCategory::Byok);
        event.quantity = 0;
        refuse(&event, "non-zero quantity");
        let mut event = cost_event("e3", 10, SpendCategory::Managed);
        event.quantity = 2;
        refuse(&event, "quantity 1");
        let mut event = cost_event("e4", 10, SpendCategory::Managed);
        event.correction_of = Some(UsageEventId::try_new("e0").unwrap());
        event.reconciliation_state = ReconciliationState::Pending;
        refuse(&event, "a correction is never pending");
        let mut event = cost_event("e5", 10, SpendCategory::Managed);
        event.reconciliation_state = ReconciliationState::Corrected;
        refuse(&event, "non-correction event can never be corrected");
        let mut event = cost_event("e6", 10, SpendCategory::Managed);
        event.correction_of = Some(UsageEventId::try_new("e6").unwrap());
        refuse(&event, "never correct itself");
    }

    #[test]
    fn corrections_are_new_events_and_supersede_the_base_in_the_fold() {
        let base = cost_event("e1", 100, SpendCategory::Managed);
        let mut correction = cost_event("e2", 60, SpendCategory::Managed);
        correction.correction_of = Some(base.id.clone());
        correction.reconciliation_state = ReconciliationState::Reconciled;
        let events = vec![base.clone(), correction.clone()];
        assert_eq!(
            UsageEvent::effective_state(&base, &events),
            ReconciliationState::Reconciled
        );
        let fold = fold_usage(&org(), &events);
        assert_eq!(fold.totals.provider_cost_micro, 60);
        assert_eq!(fold.totals.managed_cost_micro, 60);
        assert_eq!(
            fold.totals.events, 1,
            "the superseded base contributes nothing"
        );
        assert_eq!(fold.totals.corrected_events, 2, "base + correction tracked");
    }

    #[test]
    fn managed_and_byok_spend_are_accounted_separately() {
        let events = vec![
            cost_event("e1", 100, SpendCategory::Managed),
            cost_event("e2", 30, SpendCategory::Byok),
            token_event("e3", UsageUnit::InputTokens, 12, SpendCategory::Byok),
            token_event("e4", UsageUnit::ReasoningTokens, 4, SpendCategory::Managed),
        ];
        let fold = fold_usage(&org(), &events);
        assert_eq!(fold.totals.provider_cost_micro, 130);
        assert_eq!(fold.totals.managed_cost_micro, 100);
        assert_eq!(fold.totals.byok_cost_micro, 30);
        assert_eq!(fold.totals.input_tokens, 12);
        assert_eq!(fold.totals.reasoning_tokens, 4);
        assert_eq!(fold.per_task.len(), 1);
        assert_eq!(fold.per_task[0].task_id, 7);
    }

    #[test]
    fn credit_fold_counts_pending_consume_as_held_and_settle_at_actual() {
        let entry =
            |id: &str, kind: CreditKind, amount: u64, reference: Option<&str>| CreditEntry {
                id: CreditEntryId::try_new(id).unwrap(),
                organization: org(),
                billing_account_id: account(),
                kind,
                amount_micro: amount,
                reference: reference.map(|r| CreditEntryId::try_new(r).unwrap()),
                usage_event_id: None,
                reason: "test".into(),
                occurred_at_ms: 1,
                idempotency_key: None,
            };
        let entries = vec![
            entry("c1", CreditKind::Grant, 1000, None),
            entry("c2", CreditKind::Consume, 400, None),
        ];
        let balance = fold_credits(&entries);
        assert_eq!(balance.held_micro, 400);
        assert_eq!(balance.balance_micro(), 600);
        let mut settled = entries.clone();
        settled.push(entry("c3", CreditKind::Settle, 250, Some("c2")));
        let balance = fold_credits(&settled);
        assert_eq!(balance.held_micro, 0);
        assert_eq!(balance.consumed_micro, 250);
        assert_eq!(balance.balance_micro(), 750);
        settled.push(entry("c4", CreditKind::Refund, 50, Some("c2")));
        let balance = fold_credits(&settled);
        assert_eq!(balance.balance_micro(), 800);
    }

    /// The credit amount domain: `i64::MAX` is the largest legal amount and
    /// is accepted unchanged; `i64::MAX + 1` and `u64::MAX` are refused typed
    /// naming the field and the limit (never clamped); zero stays refused
    /// with its exact message.
    #[test]
    fn credit_amount_domain_is_bounded_at_i64_max_and_refuses_larger_values() {
        let entry = |amount: u64| CreditEntry {
            id: CreditEntryId::try_new("crd_1").unwrap(),
            organization: org(),
            billing_account_id: account(),
            kind: CreditKind::Grant,
            amount_micro: amount,
            reference: None,
            usage_event_id: None,
            reason: "test".into(),
            occurred_at_ms: 1,
            idempotency_key: None,
        };
        assert_eq!(MAX_CREDIT_AMOUNT_MICRO, i64::MAX as u64);
        assert_eq!(entry(1).validate(), Ok(()));
        assert_eq!(entry(i64::MAX as u64).validate(), Ok(()));
        for over in [i64::MAX as u64 + 1, u64::MAX] {
            match entry(over).validate() {
                Err(ControlPlaneError::Malformed(msg)) => {
                    assert!(msg.contains("amount_micro"), "{msg}");
                    assert!(
                        msg.contains(&MAX_CREDIT_AMOUNT_MICRO.to_string()),
                        "the refusal names the limit: {msg}"
                    );
                    assert!(
                        msg.contains(&over.to_string()),
                        "the refusal names the value: {msg}"
                    );
                }
                other => panic!("amount {over} must be refused typed, got {other:?}"),
            }
        }
        match entry(0).validate() {
            Err(ControlPlaneError::Malformed(msg)) => {
                assert!(
                    msg.contains("non-zero amount"),
                    "zero rules unchanged: {msg}"
                )
            }
            other => panic!("zero must stay refused, got {other:?}"),
        }
    }

    #[test]
    fn plan_config_is_strict_and_there_are_no_price_defaults() {
        let mut config = BillingConfig::default();
        config.plans.insert(
            "pro".into(),
            PlanConfig {
                plan_id: "pro".into(),
                features: [FEATURE_MANAGED_PROVIDERS.to_string()]
                    .into_iter()
                    .collect(),
                limits: [(LIMIT_MAX_ACTIVE_TASKS.to_string(), 3)]
                    .into_iter()
                    .collect(),
            },
        );
        config.default_plan = Some("pro".into());
        config.managed_providers.insert("openai".into());
        assert!(config.validate().is_ok());
        assert_eq!(config.category_of("openai"), SpendCategory::Managed);
        assert_eq!(config.category_of("anthropic"), SpendCategory::Byok);

        let mut bad = config.clone();
        bad.plans
            .get_mut("pro")
            .unwrap()
            .limits
            .insert("price".into(), 1);
        assert!(bad.validate().is_err(), "unknown limit names are refused");
        let mut bad = config.clone();
        bad.plans
            .get_mut("pro")
            .unwrap()
            .features
            .insert("gold".into());
        assert!(bad.validate().is_err(), "unknown features are refused");
        let mut bad = config.clone();
        bad.plans.insert(
            "team".into(),
            PlanConfig {
                plan_id: "pro".into(),
                features: BTreeSet::new(),
                limits: BTreeMap::new(),
            },
        );
        assert!(bad.validate().is_err(), "table key must equal plan_id");
        let mut bad = config.clone();
        bad.default_plan = Some("missing".into());
        assert!(bad.validate().is_err());
        assert!(
            serde_json::from_str::<BillingConfig>(r#"{"plans":{},"defaultPlan":"x"}"#).is_err(),
            "unknown fields are refused on the config surface"
        );
    }

    #[test]
    fn boundary_tags_roundtrip_and_only_admissions_are_gated() {
        for boundary in [
            AdmissionBoundary::NewTask,
            AdmissionBoundary::NewChildSpawn,
            AdmissionBoundary::NewProviderAttempt,
            AdmissionBoundary::IntegrationContinuation,
            AdmissionBoundary::RollbackContinuation,
            AdmissionBoundary::CompletionContinuation,
        ] {
            assert_eq!(AdmissionBoundary::parse(boundary.as_str()), Some(boundary));
        }
        assert!(AdmissionBoundary::NewTask.is_gated());
        assert!(AdmissionBoundary::NewProviderAttempt.is_gated());
        assert!(!AdmissionBoundary::IntegrationContinuation.is_gated());
        assert!(!AdmissionBoundary::RollbackContinuation.is_gated());
        assert!(!AdmissionBoundary::CompletionContinuation.is_gated());
    }

    /// The task-id storage encoding is a bijection over the FULL u64 domain:
    /// every boundary value (including the three the old
    /// `min(i64::MAX as u64) as i64` projection aliased together) maps to a
    /// distinct canonical 16-hex text and back exactly.
    #[test]
    fn task_id_text_is_a_reversible_full_u64_bijection() {
        let ids = [
            0u64,
            1,
            0x00ff_ffff_ffff_ffff,
            i64::MAX as u64,
            i64::MAX as u64 + 1,
            u64::MAX - 1,
            u64::MAX,
        ];
        let mut seen = BTreeSet::new();
        for id in ids {
            let text = task_id_text(id);
            assert_eq!(text.len(), 16, "{id:#x} must encode to 16 digits");
            assert!(
                text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
                "{text:?} must be lowercase hex"
            );
            assert_eq!(task_id_from_text(&text), Some(id), "{text:?} round-trips");
            assert!(seen.insert(text), "distinct ids must not alias");
        }
        assert_eq!(task_id_text(i64::MAX as u64), "7fffffffffffffff");
        assert_eq!(task_id_text(i64::MAX as u64 + 1), "8000000000000000");
        assert_eq!(task_id_text(u64::MAX), "ffffffffffffffff");
        for bad in [
            "",
            "0",
            "7FFFFFFFFFFFFFFF",
            "7fffffffffffffff0",
            "7ffffffffffffffg",
            " 7fffffffffffffff",
        ] {
            assert_eq!(task_id_from_text(bad), None, "{bad:?} is not canonical");
        }
    }

    /// Zero stays rejected (a usage event never carries task id 0); every
    /// non-zero value of the full u64 domain — the aliased boundaries
    /// included — passes validation unchanged.
    #[test]
    fn validator_rejects_zero_task_id_and_accepts_every_boundary() {
        let mut event = token_event("e1", UsageUnit::InputTokens, 1, SpendCategory::Byok);
        event.task_id = 0;
        match event.validate() {
            Err(ControlPlaneError::Malformed(msg)) => {
                assert!(msg.contains("non-zero task id"), "{msg}")
            }
            other => panic!("expected Malformed non-zero task id, got {other:?}"),
        }
        for id in [1, 7, i64::MAX as u64, i64::MAX as u64 + 1, u64::MAX] {
            event.task_id = id;
            assert_eq!(event.validate(), Ok(()), "task id {id} is legal");
        }
    }
}
