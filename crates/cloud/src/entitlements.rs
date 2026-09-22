//! The entitlement service: the ONE authority that derives an
//! organization's [`EntitlementSnapshot`] from durable billing rows plus
//! the configured plan table, and the ONE admission gate the runtime
//! consults at SAFE BOUNDARIES.
//!
//! Invariants (locked by `tests/billing.rs`):
//!
//! - **Plan data is configuration.** Features, limits and the
//!   managed-provider set come from [`BillingConfig`]; this module contains
//!   no price and no default limit. A subscription naming a plan the config
//!   does not define denies admission naming the plan gap — it never falls
//!   back to a guessed plan;
//! - **No entitlement change ever interrupts an in-flight transaction.** The
//!   gate is consulted at exactly three boundaries (new task admission, new
//!   child spawn, new provider attempt BEFORE dispatch). Continuation
//!   boundaries — integration, rollback, completion — are never gated; an
//!   in-flight row freshly opened for one of them simply records that the
//!   organization has an open transaction, and [`EntitlementService::
//!   check_admission`] admits the corresponding continuation unconditionally.
//!   Expiry or quota exhaustion therefore takes effect only on the NEXT
//!   boundary, never mid-transaction;
//! - **Managed spend debits credits; BYOK spend never does.** Usage events
//!   carry their mandatory category; ingestion refuses a managed event on a
//!   BYOK-only account, and only managed provider attempts consult (and
//!   consume) the credit balance;
//! - **The reservation ledger stays the source of truth.** [`Self::ingest`]
//!   projects durable settlement rows into usage events idempotently;
//!   [`Self::fold`] aggregates the projection. Neither path can create a
//!   monetary fact the session ledger did not record.

use std::sync::Arc;

use crate::billing::{
    fold_usage, Admission, AdmissionBoundary, AdmissionRequest, BillingAccount, BillingConfig,
    CreditBalance, CreditEntry, CreditKind, CreditLedgerError, EntitlementExceeded,
    EntitlementSnapshot, InFlightKind, InFlightTxn, ReconciliationState, SpendCategory,
    Subscription, UsageEvent, UsageFold, UsageUnit, CAUSE_CREDITS, CAUSE_FEATURE_MANAGED,
    CAUSE_LEDGER_OVERFLOW, CAUSE_PLAN, CAUSE_SUBSCRIPTION_ACTIVE, FEATURE_MANAGED_PROVIDERS,
    LIMIT_MAX_ACTIVE_TASKS, LIMIT_MAX_CHILDREN_PER_TASK, LIMIT_MAX_MANAGED_SPEND_MICRO_PER_PERIOD,
    LIMIT_MAX_PROVIDER_ATTEMPTS_PER_TASK, LIMIT_MAX_TOKENS_PER_PERIOD, MAX_FOLD_EVENTS,
    MAX_SOURCE_KEY_BYTES, MAX_USAGE_TEXT, UNIT_CACHE_READ_TOKENS, UNIT_CACHE_WRITE_TOKENS,
    UNIT_INPUT_TOKENS, UNIT_OUTPUT_TOKENS, UNIT_PROVIDER_COST, UNIT_REASONING_TOKENS,
};
use crate::billing_store::{
    BillingStore, CreditAppend, StoredCreditEntry, StoredUsageEvent, UsageAppend,
};
use crate::error::ControlPlaneError;
use crate::ids::{BillingAccountId, CreditEntryId, InFlightTxnId, OrganizationId, UsageEventId};
use crate::service::{Clock, SystemClock};

/// One durable spend row projected from the session reservation/settlement
/// ledger. `faktor-session` reads these; the server converts them into this
/// shape (the cloud crate stays free of workspace dependencies).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableSpendRow {
    /// The derived organization of the run (from the trusted session owner).
    pub organization_id: String,
    pub session_id: u64,
    pub task_id: u64,
    pub reservation_id: i64,
    pub attempt_id: String,
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Never persisted per category by the settlement (reasoning is folded
    /// into the output line there); rows carry the honest zero.
    pub reasoning_tokens: u64,
    /// The amount actually folded by the settlement (`settled_cost_micro`,
    /// with the documented legacy fallback applied by the caller).
    pub provider_cost_micro: u64,
    /// The provider-reported amount, when the settlement saw one.
    pub provider_reported_micro: u64,
    /// `settled` | `uncertain` — the durable reservation state vocabulary
    /// subset the fold consumes.
    pub state: String,
    /// The settlement time (or creation time for still-uncertain rows).
    pub occurred_at_ms: i64,
    /// The source operation tag of the row (audit provenance).
    pub source_operation: String,
}

/// The result of one ingestion batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IngestReport {
    pub appended: usize,
    pub duplicates: usize,
}

/// The window one usage page covers (cursor is the durable `event_seq`).
#[derive(Debug, Clone, PartialEq)]
pub struct UsagePage {
    pub items: Vec<StoredUsageEvent>,
    pub next_cursor: Option<String>,
}

/// The entitlement service over one [`BillingStore`].
pub struct EntitlementService {
    store: Arc<dyn BillingStore>,
    clock: Arc<dyn Clock>,
    config: BillingConfig,
}

impl std::fmt::Debug for EntitlementService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EntitlementService")
            .field("plans", &self.config.plans.len())
            .finish_non_exhaustive()
    }
}

impl EntitlementService {
    /// Build the service. The configuration is validated eagerly: a strict
    /// config error is refused at construction, never at first admission.
    pub fn new(
        store: Arc<dyn BillingStore>,
        clock: Arc<dyn Clock>,
        config: BillingConfig,
    ) -> Result<Arc<Self>, ControlPlaneError> {
        config.validate()?;
        Ok(Arc::new(Self {
            store,
            clock,
            config,
        }))
    }

    /// Build with the system clock (production hosts).
    pub fn with_system_clock(
        store: Arc<dyn BillingStore>,
        config: BillingConfig,
    ) -> Result<Arc<Self>, ControlPlaneError> {
        Self::new(store, Arc::new(SystemClock), config)
    }

    pub fn config(&self) -> &BillingConfig {
        &self.config
    }

    pub fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }

    fn new_id(prefix: &str) -> String {
        format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
    }

    // ------------------------------------------------------------ snapshot

    /// Derive the effective entitlement picture of one organization. Every
    /// field is a pure function of durable rows + the configured plan table.
    pub fn entitlement_snapshot(
        &self,
        organization: &OrganizationId,
    ) -> Result<EntitlementSnapshot, ControlPlaneError> {
        let now = self.now_ms();
        let accounts = self.store.billing_accounts(organization, None, 1)?;
        let account = accounts.into_iter().next();
        let subscription = self.store.subscription(organization)?;
        // Plan resolution: the subscription's plan when one exists, else the
        // configured default plan. A named-but-undefined plan is reported
        // (plan_found false) and denies at admission — never a silent guess.
        let plan_id = subscription
            .as_ref()
            .map(|s| s.plan_id.clone())
            .or_else(|| self.config.default_plan.clone());
        let plan = plan_id.as_deref().and_then(|id| self.config.plan(id));
        let credits = self.store.credit_balance(organization)?;
        let events = self.scan_events(organization)?;
        // The fold is CHECKED: an aggregate that leaves u64 propagates as a
        // typed ledger failure (see `ControlPlaneError::Ledger`) instead of
        // saturating the snapshot's spend/token totals.
        let fold = fold_usage(organization, &events)?;
        let total_tokens = fold.totals.total_tokens()?;
        let in_flight = self
            .store
            .in_flight(organization)?
            .into_iter()
            .filter(|txn| txn.ended_ms.is_none())
            .collect();
        Ok(EntitlementSnapshot {
            organization_id: organization.clone(),
            billing_account_id: account.as_ref().map(|a| a.id.clone()),
            plan_id: plan_id.clone(),
            plan_found: plan.is_some(),
            subscription_status: subscription.as_ref().map(|s| s.status),
            subscription_expires_ms: subscription.as_ref().and_then(|s| s.expires_ms),
            subscription_active: subscription
                .as_ref()
                .map(|s| s.is_active_at(now))
                .unwrap_or(false),
            features: plan.map(|p| p.features.clone()).unwrap_or_default(),
            limits: plan.map(|p| p.limits.clone()).unwrap_or_default(),
            credits,
            managed_spend_micro: fold.totals.managed_cost_micro,
            byok_spend_micro: fold.totals.byok_cost_micro,
            total_tokens,
            in_flight,
            now_ms: now,
        })
    }

    // ----------------------------------------------------------- admission

    /// The ONE admission gate: consult it at the three gated boundaries,
    /// never mid-transaction. Continuations always admit. Every refusal is a
    /// typed [`EntitlementExceeded`] naming the exact limit (or state cause).
    pub fn check_admission(
        &self,
        organization: &OrganizationId,
        request: &AdmissionRequest,
    ) -> Result<Admission, EntitlementExceeded> {
        request
            .validate()
            .map_err(|e| EntitlementExceeded::of(request.boundary, &format!("malformed:{e}")))?;
        if !request.boundary.is_gated() {
            // The in-flight invariant: an entitlement change (expiry, quota
            // exhaustion, credit depletion) NEVER interrupts an in-progress
            // integration/rollback/completion transaction.
            return Ok(Admission::InFlightContinuation);
        }
        let snapshot = match self.entitlement_snapshot(organization) {
            Ok(snapshot) => snapshot,
            // FAIL CLOSED on an authoritative ledger aggregate that left the
            // u64 domain: an overflowed projection cannot be proven below any
            // configured quota, so the gate refuses naming the exact
            // overflowed field (CAUSE_LEDGER_OVERFLOW) — it is NEVER
            // saturated into an admission.
            Err(ControlPlaneError::Ledger(CreditLedgerError::Overflow { field, left, .. })) => {
                return Err(EntitlementExceeded::ledger_overflow(
                    request.boundary,
                    field,
                    left,
                ))
            }
            Err(ControlPlaneError::Ledger(_)) => {
                // Any other ledger failure (a corrupt invariant, never
                // produced by the pure fold) refuses the same way: fail
                // closed naming the ledger cause.
                return Err(EntitlementExceeded::of(
                    request.boundary,
                    CAUSE_LEDGER_OVERFLOW,
                ));
            }
            Err(e) => {
                return Err(EntitlementExceeded::of(
                    request.boundary,
                    &format!("snapshot:{e}"),
                ))
            }
        };
        self.evaluate(&snapshot, request)
    }

    fn evaluate(
        &self,
        snapshot: &EntitlementSnapshot,
        request: &AdmissionRequest,
    ) -> Result<Admission, EntitlementExceeded> {
        let boundary = request.boundary;
        // 1. Plan must resolve (configuration authority, never a default).
        let Some(plan_id) = &snapshot.plan_id else {
            return Err(EntitlementExceeded::of(boundary, CAUSE_PLAN));
        };
        let _ = plan_id;
        if !snapshot.plan_found {
            return Err(EntitlementExceeded::of(boundary, CAUSE_PLAN));
        }
        // 2. The subscription must be active (derived expiry, never a
        //    sweeper): an expired/canceled subscription denies NEW admissions.
        if !snapshot.subscription_active {
            return Err(EntitlementExceeded::of(boundary, CAUSE_SUBSCRIPTION_ACTIVE));
        }
        let observed = request.observed;
        // 3. Per-boundary observed counters against the configured limits.
        match boundary {
            AdmissionBoundary::NewTask => {
                if let Some(limit) = snapshot.limit(LIMIT_MAX_ACTIVE_TASKS) {
                    if observed.active_tasks >= limit {
                        return Err(EntitlementExceeded::of(boundary, LIMIT_MAX_ACTIVE_TASKS)
                            .with_value(limit, observed.active_tasks));
                    }
                }
            }
            AdmissionBoundary::NewChildSpawn => {
                if let Some(limit) = snapshot.limit(LIMIT_MAX_CHILDREN_PER_TASK) {
                    if observed.children_of_task >= limit {
                        return Err(
                            EntitlementExceeded::of(boundary, LIMIT_MAX_CHILDREN_PER_TASK)
                                .with_value(limit, observed.children_of_task),
                        );
                    }
                }
            }
            AdmissionBoundary::NewProviderAttempt => {
                if let Some(limit) = snapshot.limit(LIMIT_MAX_PROVIDER_ATTEMPTS_PER_TASK) {
                    if observed.provider_attempts_of_task >= limit {
                        return Err(EntitlementExceeded::of(
                            boundary,
                            LIMIT_MAX_PROVIDER_ATTEMPTS_PER_TASK,
                        )
                        .with_value(limit, observed.provider_attempts_of_task));
                    }
                }
                // A managed attempt additionally needs the feature, the
                // remaining managed-spend quota and the credit balance. The
                // managed/BYOK decision is CONFIG-derived (the operator's
                // managed-provider set), never a provider-name check here.
                let managed = request
                    .provider
                    .as_deref()
                    .map(|provider| self.config.category_of(provider))
                    .unwrap_or(SpendCategory::Managed);
                if managed == SpendCategory::Managed
                    && snapshot.has_feature(FEATURE_MANAGED_PROVIDERS)
                    && snapshot.plan_found
                {
                    let estimate = observed.estimated_provider_cost_micro;
                    if let Some(limit) = snapshot.limit(LIMIT_MAX_MANAGED_SPEND_MICRO_PER_PERIOD) {
                        // Checked: a projection that leaves u64 cannot be
                        // below the limit, so it fails closed naming the
                        // quota — never saturates into an admission.
                        let projected = match snapshot.managed_spend_micro.checked_add(estimate) {
                            Some(projected) => projected,
                            None => {
                                return Err(EntitlementExceeded::of(
                                    boundary,
                                    LIMIT_MAX_MANAGED_SPEND_MICRO_PER_PERIOD,
                                )
                                .with_value(limit, snapshot.managed_spend_micro));
                            }
                        };
                        if projected > limit {
                            return Err(EntitlementExceeded::of(
                                boundary,
                                LIMIT_MAX_MANAGED_SPEND_MICRO_PER_PERIOD,
                            )
                            .with_value(limit, snapshot.managed_spend_micro));
                        }
                    }
                    // The free balance is checked (never saturated): a
                    // corrupt ledger fails closed naming the credits cause.
                    let free = snapshot
                        .credits
                        .balance_micro()
                        .map_err(|_| EntitlementExceeded::of(boundary, CAUSE_CREDITS))?;
                    if estimate > free {
                        return Err(EntitlementExceeded::of(boundary, CAUSE_CREDITS)
                            .with_value(free, estimate));
                    }
                }
                if managed == SpendCategory::Managed
                    && !snapshot.has_feature(FEATURE_MANAGED_PROVIDERS)
                {
                    return Err(EntitlementExceeded::of(boundary, CAUSE_FEATURE_MANAGED));
                }
            }
            _ => {}
        }
        // 4. Token quota (all gated boundaries consume the same period
        //    aggregate).
        if let Some(limit) = snapshot.limit(LIMIT_MAX_TOKENS_PER_PERIOD) {
            if snapshot.total_tokens >= limit {
                return Err(
                    EntitlementExceeded::of(boundary, LIMIT_MAX_TOKENS_PER_PERIOD)
                        .with_value(limit, snapshot.total_tokens),
                );
            }
        }
        Ok(Admission::Admitted)
    }

    // -------------------------------------------------------- in-flight registry

    /// Open one in-flight integration/rollback/completion transaction. From
    /// this moment until [`Self::finish_in_flight`] the transaction is
    /// protected: no gate decision can interrupt it (the continuation
    /// boundaries are never gated at all).
    pub fn begin_in_flight(
        &self,
        organization: &OrganizationId,
        kind: InFlightKind,
        reference: &str,
    ) -> Result<InFlightTxn, ControlPlaneError> {
        if reference.is_empty() || reference.len() > MAX_USAGE_TEXT {
            return Err(ControlPlaneError::Malformed(
                "in-flight reference must be 1..=256 bytes".into(),
            ));
        }
        let txn = InFlightTxn {
            id: InFlightTxnId::try_new(Self::new_id("ifx"))?,
            organization: organization.clone(),
            kind,
            reference: reference.to_string(),
            started_ms: self.now_ms(),
            ended_ms: None,
        };
        self.store.begin_in_flight(&txn)?;
        Ok(txn)
    }

    /// Close one in-flight transaction exactly once. Idempotent: a second
    /// close reports `false` and changes nothing.
    pub fn finish_in_flight(
        &self,
        organization: &OrganizationId,
        id: &InFlightTxnId,
    ) -> Result<bool, ControlPlaneError> {
        Ok(self.store.end_in_flight(organization, id, self.now_ms())?)
    }

    // ------------------------------------------------------------- accounting

    /// One billing account of an organization (the first, by id). Billing
    /// reads are always organization-scoped: a foreign id resolves `None`.
    pub fn billing_account_of(
        &self,
        organization: &OrganizationId,
    ) -> Result<Option<BillingAccount>, ControlPlaneError> {
        Ok(self
            .store
            .billing_accounts(organization, None, 1)?
            .into_iter()
            .next())
    }

    /// Ensure one billing account exists for an organization (operator
    /// provisioning at daemon startup; idempotent by the account id). The
    /// account row is the ONLY durable place the managed/BYOK permission of
    /// an organization lives.
    pub fn ensure_account(
        &self,
        organization: &OrganizationId,
        account: &BillingAccountId,
        name: &str,
        managed: bool,
    ) -> Result<BillingAccount, ControlPlaneError> {
        if let Some(existing) = self.store.billing_account(organization, account)? {
            return Ok(existing);
        }
        let row = BillingAccount {
            id: account.clone(),
            organization: organization.clone(),
            name: name.to_string(),
            managed,
            created_ms: self.now_ms(),
            disabled: false,
        };
        row.validate()?;
        self.store.put_billing_account(&row)?;
        Ok(row)
    }

    /// Provision (or replace) one organization's subscription row
    /// (operator/admin path; the row's expiry is derived at read time, so a
    /// stale `Active` status past `expires_ms` is already inactive).
    pub fn set_subscription(&self, subscription: &Subscription) -> Result<(), ControlPlaneError> {
        subscription.validate()?;
        self.store.put_subscription(subscription)?;
        Ok(())
    }

    /// One stored usage event by id, organization-scoped (a foreign id
    /// resolves `None`; a corrupt row is a typed malformed).
    pub fn usage_event_of(
        &self,
        organization: &OrganizationId,
        id: &str,
    ) -> Result<Option<StoredUsageEvent>, ControlPlaneError> {
        Ok(self.store.usage_event(organization, id)?)
    }

    /// One ascending page of the credit ledger (cursor = `entry_seq`).
    pub fn credit_entries_page(
        &self,
        organization: &OrganizationId,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<StoredCreditEntry>, ControlPlaneError> {
        Ok(self.store.credit_entries(organization, after_seq, limit)?)
    }

    /// The raw durable seam (operator tooling and tests: read-only paths
    /// stay organization-scoped through the store's own signatures).
    pub fn store(&self) -> &Arc<dyn BillingStore> {
        &self.store
    }

    /// Record one usage event. The category is ENFORCED on write against the
    /// account: a BYOK-only account refuses managed events; a disabled or
    /// unknown account refuses everything (nothing is written).
    pub fn record_usage(&self, event: &UsageEvent) -> Result<UsageAppend, ControlPlaneError> {
        event.validate()?;
        let account = self
            .store
            .billing_account(&event.organization_id, &event.billing_account_id)?
            .ok_or_else(|| {
                ControlPlaneError::NotFound(format!(
                    "billing account {} of organization {}",
                    event.billing_account_id, event.organization_id
                ))
            })?;
        if !account.permits(event.category) {
            return Err(ControlPlaneError::Conflict(format!(
                "billing account {} does not permit {} usage (managed: {})",
                account.id,
                event.category.as_str(),
                account.managed
            )));
        }
        Ok(self.store.append_usage_event(event)?)
    }

    /// Record one CORRECTION: a NEW event naming the corrected base. The
    /// base row is never mutated; a base already corrected is refused
    /// (corrections never fork) and a correction of a foreign/missing event
    /// is refused (tenant isolation).
    pub fn correct_usage(&self, correction: &UsageEvent) -> Result<UsageAppend, ControlPlaneError> {
        let base_id = correction.correction_of.as_ref().ok_or_else(|| {
            ControlPlaneError::Malformed(
                "a correction must carry correction_of naming the base event".into(),
            )
        })?;
        if correction.reconciliation_state == ReconciliationState::Pending {
            return Err(ControlPlaneError::Malformed(
                "a correction is never pending".into(),
            ));
        }
        correction.validate()?;
        let base = self
            .store
            .usage_event(&correction.organization_id, base_id.as_str())?
            .ok_or_else(|| ControlPlaneError::NotFound(format!("usage event {base_id}")))?;
        if base.event.correction_of.is_some() {
            return Err(ControlPlaneError::Conflict(
                "a correction never corrects another correction".into(),
            ));
        }
        if self
            .store
            .correction_exists(&correction.organization_id, base_id.as_str())?
        {
            return Err(ControlPlaneError::Conflict(format!(
                "usage event {base_id} was already corrected (corrections are append-only, not forks)"
            )));
        }
        self.record_usage(correction)
    }

    /// Project one durable spend row into the usage ledger. Deterministic and
    /// idempotent: the source key is derived from the reservation identity,
    /// so re-ingesting the same durable rows appends nothing. Managed rows
    /// record a `managed` category (they are the ones that may debit
    /// credits); BYOK rows never debit.
    pub fn ingest_spend_row(
        &self,
        organization: &OrganizationId,
        account: &BillingAccountId,
        row: &DurableSpendRow,
        provider_category: SpendCategory,
    ) -> Result<IngestReport, ControlPlaneError> {
        if row.reservation_id <= 0 {
            return Err(ControlPlaneError::Malformed(
                "a durable spend row must carry a positive reservation id".into(),
            ));
        }
        if row.provider.len() > MAX_USAGE_TEXT
            || row.model.len() > MAX_USAGE_TEXT
            || row.attempt_id.len() > MAX_USAGE_TEXT
            || row.source_operation.len() > MAX_USAGE_TEXT
        {
            return Err(ControlPlaneError::Malformed(
                "durable spend row text field is oversized".into(),
            ));
        }
        let run_id = row.session_id.to_string();
        let mut report = IngestReport::default();
        let mut events: Vec<UsageEvent> = Vec::new();
        let tokens: [(UsageUnit, u64); 5] = [
            (UsageUnit::InputTokens, row.input_tokens),
            (UsageUnit::OutputTokens, row.output_tokens),
            (UsageUnit::CacheReadTokens, row.cache_read_tokens),
            (UsageUnit::CacheWriteTokens, row.cache_write_tokens),
            (UsageUnit::ReasoningTokens, row.reasoning_tokens),
        ];
        for (unit, quantity) in tokens {
            if quantity == 0 {
                continue;
            }
            events.push(self.projected_event(
                organization,
                account,
                row,
                &run_id,
                unit,
                quantity,
                0,
                provider_category,
            )?);
        }
        if row.provider_cost_micro > 0 {
            events.push(self.projected_event(
                organization,
                account,
                row,
                &run_id,
                UsageUnit::ProviderCostMicro,
                1,
                row.provider_cost_micro,
                provider_category,
            )?);
        }
        for event in &events {
            match self.record_usage(event)? {
                UsageAppend::Appended => report.appended += 1,
                UsageAppend::Duplicate => report.duplicates += 1,
            }
        }
        Ok(report)
    }

    #[allow(clippy::too_many_arguments)]
    fn projected_event(
        &self,
        organization: &OrganizationId,
        account: &BillingAccountId,
        row: &DurableSpendRow,
        run_id: &str,
        unit: UsageUnit,
        quantity: u64,
        provider_cost_micro: u64,
        category: SpendCategory,
    ) -> Result<UsageEvent, ControlPlaneError> {
        let unit_key = match unit {
            UsageUnit::InputTokens => UNIT_INPUT_TOKENS,
            UsageUnit::OutputTokens => UNIT_OUTPUT_TOKENS,
            UsageUnit::CacheReadTokens => UNIT_CACHE_READ_TOKENS,
            UsageUnit::CacheWriteTokens => UNIT_CACHE_WRITE_TOKENS,
            UsageUnit::ReasoningTokens => UNIT_REASONING_TOKENS,
            UsageUnit::ProviderCostMicro => UNIT_PROVIDER_COST,
        };
        let source_key = format!(
            "reservation:{}:{}:{}",
            row.reservation_id, row.attempt_id, unit_key
        );
        if source_key.len() > MAX_SOURCE_KEY_BYTES {
            return Err(ControlPlaneError::Malformed(
                "the derived ingestion source key is oversized".into(),
            ));
        }
        Ok(UsageEvent {
            id: UsageEventId::try_new(Self::new_id("uev"))?,
            organization_id: organization.clone(),
            billing_account_id: account.clone(),
            task_id: row.task_id,
            run_id: run_id.to_string(),
            attempt_id: row.attempt_id.clone(),
            provider: row.provider.clone(),
            model: row.model.clone(),
            unit,
            quantity,
            provider_cost_micro,
            source_operation: row.source_operation.clone(),
            occurred_at_ms: row.occurred_at_ms,
            // A settled durable row is reconciled by construction; the
            // uncertain state never fabricates money (it carries the
            // conservative estimate the ledger recorded).
            reconciliation_state: ReconciliationState::Reconciled,
            correction_of: None,
            category,
            source_key,
        })
    }

    /// Fold one organization's usage ledger: org totals plus per-task rows
    /// (inputs/outputs/cache/reasoning tokens, provider cost, managed vs
    /// BYOK spend). The scan is bounded; a flood past the bound refuses
    /// loudly instead of folding a prefix. Every aggregate is checked: a
    /// total that leaves the `u64` domain propagates the typed
    /// [`ControlPlaneError::Ledger`] (never a saturated `u64::MAX`).
    pub fn fold(&self, organization: &OrganizationId) -> Result<UsageFold, ControlPlaneError> {
        let events = self.scan_events(organization)?;
        Ok(fold_usage(organization, &events)?)
    }

    /// The durable fold total of one organization: managed spend (used by
    /// the admission gate) without materializing the whole page structure.
    pub fn managed_spend_micro(
        &self,
        organization: &OrganizationId,
    ) -> Result<u64, ControlPlaneError> {
        Ok(self.fold(organization)?.totals.managed_cost_micro)
    }

    fn scan_events(
        &self,
        organization: &OrganizationId,
    ) -> Result<Vec<UsageEvent>, ControlPlaneError> {
        let mut events = Vec::new();
        let mut cursor = 0i64;
        loop {
            let page =
                self.store
                    .usage_events(organization, cursor, crate::billing::MAX_USAGE_PAGE)?;
            if page.is_empty() {
                break;
            }
            let last = page.last().map(|row| row.event_seq).unwrap_or(cursor);
            for row in page {
                events.push(row.event);
            }
            if events.len() > MAX_FOLD_EVENTS {
                return Err(ControlPlaneError::Conflict(format!(
                    "usage fold of organization {organization} exceeds the {MAX_FOLD_EVENTS}-event bound; refusing to fold a prefix"
                )));
            }
            cursor = last;
        }
        Ok(events)
    }

    // --------------------------------------------------------------- credits

    /// Grant credits (admin-gated at the route). The grant is a new
    /// append-only entry; the idempotency key (when supplied) is enforced by
    /// the store.
    pub fn grant_credits(
        &self,
        organization: &OrganizationId,
        account: &BillingAccountId,
        amount_micro: u64,
        reason: &str,
        idempotency_key: Option<&str>,
    ) -> Result<CreditAppend, ControlPlaneError> {
        self.append_credit(
            organization,
            account,
            CreditKind::Grant,
            amount_micro,
            None,
            None,
            reason,
            idempotency_key,
        )
    }

    /// Debit credits BEFORE a managed provider call (record-before-call): a
    /// pending consume that holds the estimate until [`Self::settle_consume`]
    /// closes it at the actual. A crash between the two leaves the hold in
    /// place — never a silently free call. A replayed idempotency key returns
    /// the `Duplicate` outcome (nothing double-debited).
    pub fn consume_before_call(
        &self,
        organization: &OrganizationId,
        account: &BillingAccountId,
        estimate_micro: u64,
        usage_event: Option<&UsageEventId>,
        reason: &str,
        idempotency_key: Option<&str>,
    ) -> Result<CreditAppend, ControlPlaneError> {
        self.append_credit(
            organization,
            account,
            CreditKind::Consume,
            estimate_micro,
            None,
            usage_event,
            reason,
            idempotency_key,
        )
    }

    /// The DURABLE id of the credit entry recorded under `idempotency_key`
    /// (`None` when no entry of this organization claimed it). Additive
    /// follow-up for record-before-call callers: [`Self::consume_before_call`]
    /// reports the append outcome, and the agent-side debit adapter names the
    /// returned id in [`Self::settle_consume`]/[`Self::refund_consume`] to
    /// close exactly the hold its own key opened.
    pub fn credit_entry_id_by_idempotency_key(
        &self,
        organization: &OrganizationId,
        idempotency_key: &str,
    ) -> Result<Option<CreditEntryId>, ControlPlaneError> {
        crate::service::ControlPlane::validate_idempotency_key(idempotency_key)?;
        Ok(self
            .store
            .credit_entry_by_idempotency_key(organization, idempotency_key)?
            .map(|row| row.entry.id))
    }

    /// Settle one pending consume at its actual spend. Exactly once: a
    /// second settle is a typed conflict. The delta above the hold must be
    /// covered by the remaining balance (or the settle refuses, leaving the
    /// hold in place).
    pub fn settle_consume(
        &self,
        organization: &OrganizationId,
        account: &BillingAccountId,
        consume: &CreditEntryId,
        actual_micro: u64,
        reason: &str,
    ) -> Result<CreditAppend, ControlPlaneError> {
        self.append_credit(
            organization,
            account,
            CreditKind::Settle,
            actual_micro,
            Some(consume),
            None,
            reason,
            None,
        )
    }

    /// Refund unused credits of a consume/its settled actual. A refund is a
    /// NEW credit event; exactness (never more than what was actually
    /// spent/held minus prior refunds) is enforced by the store.
    pub fn refund_consume(
        &self,
        organization: &OrganizationId,
        account: &BillingAccountId,
        consume: &CreditEntryId,
        amount_micro: u64,
        reason: &str,
    ) -> Result<CreditAppend, ControlPlaneError> {
        self.append_credit(
            organization,
            account,
            CreditKind::Refund,
            amount_micro,
            Some(consume),
            None,
            reason,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn append_credit(
        &self,
        organization: &OrganizationId,
        account: &BillingAccountId,
        kind: CreditKind,
        amount_micro: u64,
        reference: Option<&CreditEntryId>,
        usage_event: Option<&UsageEventId>,
        reason: &str,
        idempotency_key: Option<&str>,
    ) -> Result<CreditAppend, ControlPlaneError> {
        if let Some(key) = idempotency_key {
            crate::service::ControlPlane::validate_idempotency_key(key)?;
        }
        let entry = CreditEntry {
            id: CreditEntryId::try_new(Self::new_id("crd"))?,
            organization: organization.clone(),
            billing_account_id: account.clone(),
            kind,
            amount_micro,
            reference: reference.cloned(),
            usage_event_id: usage_event.cloned(),
            reason: reason.to_string(),
            occurred_at_ms: self.now_ms(),
            idempotency_key: idempotency_key.map(|k| k.to_string()),
        };
        entry.validate()?;
        Ok(self.store.append_credit_entry(&entry)?)
    }

    /// One ascending page of the usage ledger (strict cursor = `event_seq`).
    pub fn usage_page(
        &self,
        organization: &OrganizationId,
        since: Option<&str>,
        limit: usize,
    ) -> Result<UsagePage, ControlPlaneError> {
        let cursor = parse_cursor(since)?;
        let mut items = self
            .store
            .usage_events(organization, cursor, limit.saturating_add(1))?;
        let has_more = items.len() > limit;
        items.truncate(limit);
        let next_cursor = if has_more {
            items.last().map(|row| row.event_seq.to_string())
        } else {
            None
        };
        Ok(UsagePage { items, next_cursor })
    }

    /// The durable credit balance of one organization.
    pub fn credit_balance(
        &self,
        organization: &OrganizationId,
    ) -> Result<CreditBalance, ControlPlaneError> {
        Ok(self.store.credit_balance(organization)?)
    }

    /// The account the gate/ingest default to when the caller names none.
    pub fn default_account(
        &self,
        organization: &OrganizationId,
    ) -> Result<Option<BillingAccountId>, ControlPlaneError> {
        Ok(self
            .billing_account_of(organization)?
            .map(|account| account.id))
    }
}

/// The outcome of one credit append through the service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreditAppendOutcome {
    /// A new durable entry with this id.
    Appended(CreditEntryId),
    /// The idempotency key replayed the recorded entry; nothing was written.
    Duplicate(CreditEntryId),
}

fn parse_cursor(since: Option<&str>) -> Result<i64, ControlPlaneError> {
    match since {
        None => Ok(0),
        Some(raw) => raw.parse::<i64>().map_err(|_| {
            ControlPlaneError::Malformed("cursor must be a non-negative integer".into())
        }),
    }
    .and_then(|value| {
        if value < 0 {
            Err(ControlPlaneError::Malformed(
                "cursor must be a non-negative integer".into(),
            ))
        } else {
            Ok(value)
        }
    })
}
