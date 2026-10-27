//! The in-memory [`BillingStore`] implementation (tests and embedded
//! hosts). Split out of `billing_store.rs` to keep the source-size ceiling
//! honest; it is a child module so it shares the parent's private helpers
//! and the period projection exactly.

use super::*;

#[derive(Default)]
pub(super) struct MemBilling {
    pub(super) accounts: BTreeMap<String, BillingAccount>,
    pub(super) subscriptions: BTreeMap<String, Subscription>,
    pub(super) usage: BTreeMap<String, StoredUsageEvent>,
    pub(super) usage_order: BTreeMap<String, Vec<String>>,
    /// Append-only lookup indexes. The old append scanned every usage row
    /// twice (source-key replay + correction base), so ingesting 1M rows was
    /// quadratic in comparisons and task pages scanned all organizations.
    pub(super) usage_source: BTreeMap<(String, String), String>,
    pub(super) usage_task: BTreeMap<(String, u64), Vec<String>>,
    pub(super) usage_corrections: BTreeMap<(String, String), String>,
    pub(super) period_configs: BTreeMap<String, (i64, i64)>,
    pub(super) outbox_watermarks: BTreeMap<String, i64>,
    pub(super) credits: BTreeMap<String, StoredCreditEntry>,
    pub(super) credit_order: BTreeMap<String, Vec<String>>,
    /// Incremental credit indexes/projection: the append path must not fold
    /// every prior entry (that was O(n^2) over an organization's ledger).
    pub(super) credit_idempotency: BTreeMap<(String, String), String>,
    pub(super) credit_settles: BTreeMap<(String, String), String>,
    pub(super) credit_refunds: BTreeMap<(String, String), u64>,
    pub(super) credit_balances: BTreeMap<String, CreditBalance>,
    pub(super) in_flight: BTreeMap<String, InFlightTxn>,
    pub(super) report_periods: BTreeMap<String, ReportPeriodRow>,
    pub(super) mutations: BTreeMap<String, (String, Option<String>, i64)>,
}

/// In-memory [`BillingStore`] (tests and embedded hosts). Enforces exactly
/// the same append-only/idempotency/balance rules as the SQLite store.
#[derive(Default)]
pub struct MemoryBillingStore {
    inner: Mutex<MemBilling>,
}

impl MemoryBillingStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, MemBilling>, BillingStoreError> {
        self.inner
            .lock()
            .map_err(|_| BillingStoreError::Backend("in-memory billing lock is poisoned".into()))
    }
}

/// Apply one credit entry to the in-memory balance projection. This is the
/// exact delta of `fold_credits`; the append path keeps it incrementally so a
/// 1M-entry ledger is never folded per append.
fn apply_memory_credit_entry(
    balance: &mut CreditBalance,
    entry: &CreditEntry,
    target_hold: Option<u64>,
    settled: bool,
    refunded_before: u64,
) -> Result<(), BillingStoreError> {
    match entry.kind {
        CreditKind::Grant => add_credit_delta(balance, "granted_micro", entry.amount_micro),
        CreditKind::Consume => {
            add_credit_delta(balance, "consumed_micro", entry.amount_micro)?;
            add_credit_delta(balance, "held_micro", entry.amount_micro)?;
            add_credit_delta(balance, "pending_consumes", 1)
        }
        CreditKind::Settle => {
            let Some(held) = target_hold else {
                return Ok(());
            };
            balance.held_micro = balance.held_micro.checked_sub(held).ok_or_else(|| {
                BillingStoreError::Ledger(CreditLedgerError::Corrupt {
                    detail: format!(
                        "credit rollup held_micro is below the settle hold {held} of {}",
                        entry.reference.as_ref().map(|r| r.as_str()).unwrap_or("")
                    ),
                })
            })?;
            balance.pending_consumes =
                balance.pending_consumes.checked_sub(1).ok_or_else(|| {
                    BillingStoreError::Ledger(CreditLedgerError::Corrupt {
                        detail: format!(
                            "credit rollup pending_consumes is zero at the settle of {}",
                            entry.reference.as_ref().map(|r| r.as_str()).unwrap_or("")
                        ),
                    })
                })?;
            if entry.amount_micro >= held {
                add_credit_delta(
                    balance,
                    "consumed_micro",
                    entry.amount_micro.saturating_sub(held),
                )
            } else {
                let reduction = held - entry.amount_micro;
                balance.consumed_micro =
                    balance
                        .consumed_micro
                        .checked_sub(reduction)
                        .ok_or_else(|| {
                            BillingStoreError::Ledger(CreditLedgerError::Corrupt {
                                detail: format!(
                                    "credit rollup consumed_micro is below the settle reduction \
                                     {reduction} of {}",
                                    entry.reference.as_ref().map(|r| r.as_str()).unwrap_or("")
                                ),
                            })
                        })?;
                Ok(())
            }
        }
        CreditKind::Refund => {
            add_credit_delta(balance, "refunded_micro", entry.amount_micro)?;
            if settled {
                return Ok(());
            }
            // A pending consume is only fully released (append validated the
            // exact remaining hold), so the refund releases that hold.
            let released = entry
                .amount_micro
                .checked_add(refunded_before)
                .ok_or_else(|| {
                    BillingStoreError::Ledger(CreditLedgerError::Corrupt {
                        detail: "credit refund release overflows u64".into(),
                    })
                })?;
            if let Some(held) = target_hold {
                if released > held {
                    return Err(BillingStoreError::Ledger(CreditLedgerError::Corrupt {
                        detail: format!(
                            "pending consume {} has {released} microUSD released against its \
                             {held} microUSD hold",
                            entry.reference.as_ref().map(|r| r.as_str()).unwrap_or("")
                        ),
                    }));
                }
            }
            balance.held_micro = balance
                .held_micro
                .checked_sub(entry.amount_micro)
                .ok_or_else(|| {
                    BillingStoreError::Ledger(CreditLedgerError::Corrupt {
                        detail: format!(
                            "credit rollup held_micro is below the released {} of consume {}",
                            entry.amount_micro,
                            entry.reference.as_ref().map(|r| r.as_str()).unwrap_or("")
                        ),
                    })
                })?;
            balance.pending_consumes =
                balance.pending_consumes.checked_sub(1).ok_or_else(|| {
                    BillingStoreError::Ledger(CreditLedgerError::Corrupt {
                        detail: format!(
                            "credit rollup pending_consumes is zero at the release of {}",
                            entry.reference.as_ref().map(|r| r.as_str()).unwrap_or("")
                        ),
                    })
                })?;
            Ok(())
        }
    }
}

fn claim_credit(
    state: &mut MemBilling,
    entry: &CreditEntry,
) -> Result<CreditAppend, BillingStoreError> {
    let org = entry.organization.as_str().to_string();
    if let Some(key) = &entry.idempotency_key {
        if let Some(existing_id) = state
            .credit_idempotency
            .get(&(org.clone(), key.as_str().to_string()))
        {
            let existing = state
                .credits
                .get(existing_id)
                .expect("credit index and rows are written together");
            if existing.entry.kind == entry.kind
                && existing.entry.amount_micro == entry.amount_micro
                && existing.entry.reference == entry.reference
                && existing.entry.billing_account_id == entry.billing_account_id
                && existing.entry.usage_event_id == entry.usage_event_id
            {
                // A record-before-call replay must only replay an OPEN hold.
                // Once the consume was settled or refunded, returning
                // `Duplicate` would hand the caller a hold token for money
                // that is already closed and let it dispatch a second,
                // unpaid provider call.
                if existing.entry.kind == CreditKind::Consume {
                    let closed = existing.entry.id.as_str().to_string();
                    if state
                        .credit_settles
                        .contains_key(&(org.clone(), closed.clone()))
                        || state
                            .credit_refunds
                            .contains_key(&(org.clone(), closed.clone()))
                    {
                        return Err(CreditAppendRefusal::ConsumeAlreadyClosed {
                            reference: existing.entry.id.as_str().to_string(),
                        }
                        .into());
                    }
                }
                return Ok(CreditAppend::Duplicate);
            }
            return Err(CreditAppendRefusal::IdempotencyConflict(key.clone()).into());
        }
    }
    if state.credits.contains_key(entry.id.as_str()) {
        // Primary-key parity with SQLite: a reused entry id would overwrite
        // the prior row while leaving its sequence/index entries behind.
        return Err(BillingStoreError::Malformed(format!(
            "credit entry id {} is already recorded; ids are immutable",
            entry.id
        )));
    }
    let mut balance = state.credit_balances.get(&org).copied().unwrap_or_default();
    // Delta inputs for the incremental balance update below (the validation
    // branch already resolved them; never re-scan the ledger).
    let mut applied_target_hold: Option<u64> = None;
    let mut applied_settled = false;
    let mut applied_refunded_before: u64 = 0;
    match entry.kind {
        CreditKind::Grant => {}
        CreditKind::Consume => {
            let available = balance.balance_micro()?;
            if entry.amount_micro > available {
                return Err(CreditAppendRefusal::InsufficientCredits {
                    available,
                    requested: entry.amount_micro,
                }
                .into());
            }
        }
        CreditKind::Settle | CreditKind::Refund => {
            let reference = entry
                .reference
                .as_ref()
                .expect("validated: settle/refund carry a reference");
            let ref_key = reference.as_str().to_string();
            let target = state
                .credits
                .get(reference.as_str())
                .filter(|row| row.entry.organization == entry.organization)
                .map(|row| &row.entry);
            let Some(target) = target else {
                return Err(
                    CreditAppendRefusal::UnknownReference(reference.as_str().to_string()).into(),
                );
            };
            if target.kind != CreditKind::Consume {
                return Err(CreditAppendRefusal::NotAConsume {
                    reference: reference.as_str().to_string(),
                    kind: target.kind.as_str().to_string(),
                }
                .into());
            }
            if target.billing_account_id != entry.billing_account_id {
                return Err(CreditAppendRefusal::ReferenceAccountMismatch {
                    reference: reference.as_str().to_string(),
                    expected: target.billing_account_id.as_str().to_string(),
                    actual: entry.billing_account_id.as_str().to_string(),
                }
                .into());
            }
            let settle = state
                .credit_settles
                .get(&(org.clone(), ref_key.clone()))
                .and_then(|id| state.credits.get(id))
                .map(|row| &row.entry);
            applied_target_hold = Some(target.amount_micro);
            applied_settled = settle.is_some();
            if entry.kind == CreditKind::Settle {
                if settle.is_some() {
                    return Err(CreditAppendRefusal::AlreadySettled {
                        reference: reference.as_str().to_string(),
                    }
                    .into());
                }
                // A released hold is never settled: refunding a pending
                // consume cancels it, and charging the actual after that
                // would let the organization spend credits it already got
                // back (a double-spend/credit-minting hole).
                if state
                    .credit_refunds
                    .contains_key(&(org.clone(), ref_key.clone()))
                {
                    return Err(CreditAppendRefusal::SettleAfterRefund {
                        reference: reference.as_str().to_string(),
                    }
                    .into());
                }
                // Only the DELTA above the pending hold must be covered. The
                // hold is ALREADY inside `balance_micro()` (an unsettled
                // consume counts as consumed at its held amount), so adding
                // `target.amount_micro` back would double-count it and let a
                // settle spend free + hold.
                let delta = entry.amount_micro.saturating_sub(target.amount_micro);
                let available = balance.balance_micro()?;
                if delta > available {
                    return Err(CreditAppendRefusal::InsufficientCredits {
                        available,
                        requested: delta,
                    }
                    .into());
                }
            } else {
                // Refund exactness: what the consume actually spent (its
                // settle amount when settled, else the pending hold) minus
                // everything already refunded for it. Checked: a ledger whose
                // refunds exceed the spend is refused typed, never masked to
                // a zero refundable amount.
                let spent = settle
                    .map(|s| s.amount_micro)
                    .unwrap_or(target.amount_micro);
                let refunded = state
                    .credit_refunds
                    .get(&(org.clone(), ref_key.clone()))
                    .copied()
                    .unwrap_or(0);
                applied_refunded_before = refunded;
                let refundable =
                    spent
                        .checked_sub(refunded)
                        .ok_or_else(|| CreditLedgerError::Corrupt {
                            detail: format!(
                            "consume {} has {refunded} microUSD refunded against {spent} microUSD \
                             spent",
                            reference.as_str()
                        ),
                        })?;
                if settle.is_none() {
                    // A pending consume is cancelled only by releasing its
                    // FULL remaining hold: a partial refund would leave a
                    // half-released hold that a later settle could charge
                    // again (free managed spend).
                    if entry.amount_micro != refundable {
                        return Err(CreditAppendRefusal::PendingHoldRefundNotFull {
                            reference: reference.as_str().to_string(),
                            refundable,
                            requested: entry.amount_micro,
                        }
                        .into());
                    }
                } else if entry.amount_micro > refundable {
                    return Err(CreditAppendRefusal::RefundExceedsConsumed {
                        reference: reference.as_str().to_string(),
                        refundable,
                        requested: entry.amount_micro,
                    }
                    .into());
                }
            }
        }
    }
    // The entry's own contribution must not leave the u64 domain: refused
    // typed before the write (nothing is ever written by a refusal).
    checked_append_domain(entry, &balance)?;
    apply_memory_credit_entry(
        &mut balance,
        entry,
        applied_target_hold,
        applied_settled,
        applied_refunded_before,
    )?;
    let seq = state.credit_order.get(&org).map(|v| v.len()).unwrap_or(0) as i64 + 1;
    let id = entry.id.as_str().to_string();
    state.credits.insert(
        id.clone(),
        StoredCreditEntry {
            entry_seq: seq,
            entry: entry.clone(),
        },
    );
    state
        .credit_order
        .entry(org.clone())
        .or_default()
        .push(id.clone());
    if let Some(key) = &entry.idempotency_key {
        state
            .credit_idempotency
            .insert((org.clone(), key.as_str().to_string()), id.clone());
    }
    if let Some(reference) = &entry.reference {
        let ref_key = reference.as_str().to_string();
        match entry.kind {
            CreditKind::Settle => {
                state
                    .credit_settles
                    .insert((org.clone(), ref_key), id.clone());
            }
            CreditKind::Refund => {
                let released = state
                    .credit_refunds
                    .get(&(org.clone(), ref_key.clone()))
                    .copied()
                    .unwrap_or(0);
                state.credit_refunds.insert(
                    (org.clone(), ref_key),
                    checked_ledger_sum("refunded_micro", released, entry.amount_micro)?,
                );
            }
            CreditKind::Grant | CreditKind::Consume => {}
        }
    }
    state.credit_balances.insert(org, balance);
    Ok(CreditAppend::Appended)
}

impl BillingStore for MemoryBillingStore {
    fn put_billing_account(&self, account: &BillingAccount) -> Result<(), BillingStoreError> {
        let mut state = self.lock()?;
        // An account id is globally unique. Silently replacing another
        // organization's row would hand that tenant's ledger identity to a
        // different organization (a cross-tenant aliasing hole).
        if let Some(existing) = state.accounts.get(account.id.as_str()) {
            if existing.organization != account.organization {
                return Err(BillingStoreError::Malformed(format!(
                    "billing account {} is already owned by another organization; refusing to \
                     reassign it",
                    account.id
                )));
            }
        }
        state
            .accounts
            .insert(account.id.as_str().to_string(), account.clone());
        Ok(())
    }

    fn billing_account(
        &self,
        organization: &OrganizationId,
        id: &BillingAccountId,
    ) -> Result<Option<BillingAccount>, BillingStoreError> {
        Ok(self
            .lock()?
            .accounts
            .get(id.as_str())
            .filter(|a| a.organization == *organization)
            .cloned())
    }

    fn billing_accounts(
        &self,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<BillingAccount>, BillingStoreError> {
        Ok(self
            .lock()?
            .accounts
            .values()
            .filter(|a| a.organization == *organization)
            .filter(|a| after.map(|c| a.id.as_str() > c).unwrap_or(true))
            .take(limit)
            .cloned()
            .collect())
    }

    fn put_subscription(&self, subscription: &Subscription) -> Result<(), BillingStoreError> {
        // A trialing row without an expiry would be an unbounded trial in
        // `is_active_at`; the store seam refuses the shape instead of
        // relying on every caller having gone through the service.
        subscription
            .validate()
            .map_err(|error| BillingStoreError::Malformed(error.to_string()))?;
        self.lock()?.subscriptions.insert(
            subscription.organization.as_str().to_string(),
            subscription.clone(),
        );
        Ok(())
    }

    fn subscription(
        &self,
        organization: &OrganizationId,
    ) -> Result<Option<Subscription>, BillingStoreError> {
        let row = self
            .lock()?
            .subscriptions
            .get(organization.as_str())
            .cloned();
        if let Some(subscription) = &row {
            subscription
                .validate()
                .map_err(|error| BillingStoreError::Malformed(error.to_string()))?;
        }
        Ok(row)
    }

    fn append_usage_event(&self, event: &UsageEvent) -> Result<UsageAppend, BillingStoreError> {
        // Public seam: enforce the event shape so a malformed event is a
        // typed refusal, never a row the folds cannot interpret.
        event
            .validate()
            .map_err(|error| BillingStoreError::Malformed(error.to_string()))?;
        let mut state = self.lock()?;
        let org = event.organization_id.as_str().to_string();
        if let Some(existing_id) = state
            .usage_source
            .get(&(org.clone(), event.source_key.clone()))
        {
            let existing = state
                .usage
                .get(existing_id)
                .expect("usage index and rows are written together");
            // A replay of the same durable fact is idempotent; the SAME
            // source key carrying a different fact is a typed collision (a
            // silent duplicate would drop real usage/spend).
            if existing.event.same_fact(event) {
                return Ok(UsageAppend::Duplicate);
            }
            return Err(BillingStoreError::Malformed(format!(
                "usage source key {:?} already records a different fact; refusing a silent \
                 duplicate",
                event.source_key
            )));
        }
        if state.usage.contains_key(event.id.as_str()) {
            // The durable store's primary key refuses this; overwriting here
            // would silently detach the old (organization, source_key) fact
            // from its row and corrupt the replay index.
            return Err(BillingStoreError::Malformed(format!(
                "usage event id {} is already recorded; a different source key cannot reuse it",
                event.id
            )));
        }
        let period_id = match &event.correction_of {
            Some(base) => state
                .usage
                .get(base.as_str())
                .filter(|row| row.event.organization_id == event.organization_id)
                .map(|row| row.period_id.clone())
                .unwrap_or_else(|| period::memory_event_period(&state, event)),
            None => period::memory_event_period(&state, event),
        };
        let seq = state.usage_order.get(&org).map(|v| v.len()).unwrap_or(0) as i64 + 1;
        let id = event.id.as_str().to_string();
        state.usage.insert(
            id.clone(),
            StoredUsageEvent {
                event_seq: seq,
                event: event.clone(),
                period_id,
            },
        );
        state
            .usage_order
            .entry(org.clone())
            .or_default()
            .push(id.clone());
        state
            .usage_source
            .insert((org.clone(), event.source_key.clone()), id.clone());
        state
            .usage_task
            .entry((org.clone(), event.task_id))
            .or_default()
            .push(id.clone());
        if let Some(base) = &event.correction_of {
            state
                .usage_corrections
                .insert((org, base.as_str().to_string()), id);
        }
        Ok(UsageAppend::Appended)
    }

    fn usage_event(
        &self,
        organization: &OrganizationId,
        id: &str,
    ) -> Result<Option<StoredUsageEvent>, BillingStoreError> {
        Ok(self
            .lock()?
            .usage
            .get(id)
            .filter(|row| row.event.organization_id == *organization)
            .cloned())
    }

    fn usage_events(
        &self,
        organization: &OrganizationId,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<StoredUsageEvent>, BillingStoreError> {
        let state = self.lock()?;
        let org = organization.as_str();
        let mut rows: Vec<StoredUsageEvent> = state
            .usage_order
            .get(org)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| state.usage.get(id).cloned())
                    .filter(|row| row.event_seq > after_seq)
                    .take(limit)
                    .collect()
            })
            .unwrap_or_default();
        rows.sort_by_key(|row| row.event_seq);
        Ok(rows)
    }

    fn usage_events_of_task(
        &self,
        organization: &OrganizationId,
        task_id: u64,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<StoredUsageEvent>, BillingStoreError> {
        let state = self.lock()?;
        let mut rows: Vec<StoredUsageEvent> = state
            .usage_task
            .get(&(organization.as_str().to_string(), task_id))
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| state.usage.get(id).cloned())
                    .filter(|row| row.event_seq > after_seq)
                    .collect()
            })
            .unwrap_or_default();
        rows.sort_by_key(|row| row.event_seq);
        rows.truncate(limit);
        Ok(rows)
    }

    fn usage_fold(&self, organization: &OrganizationId) -> Result<UsageFold, BillingStoreError> {
        let state = self.lock()?;
        // The pure fold is the in-memory authority: the memory store keeps
        // the same append-only rows (no rollup projection to drift).
        let mut rows: Vec<&StoredUsageEvent> = state
            .usage
            .values()
            .filter(|row| row.event.organization_id == *organization)
            .collect();
        rows.sort_by_key(|row| row.event_seq);
        let events: Vec<UsageEvent> = rows.into_iter().map(|row| row.event.clone()).collect();
        drop(state);
        Ok(fold_usage(organization, &events)?)
    }

    fn usage_totals(
        &self,
        organization: &OrganizationId,
    ) -> Result<UsageTotals, BillingStoreError> {
        Ok(self.usage_fold(organization)?.totals)
    }

    fn usage_fold_in_period(
        &self,
        organization: &OrganizationId,
        period_id: &str,
    ) -> Result<UsageFold, BillingStoreError> {
        let state = self.lock()?;
        let mut rows: Vec<&StoredUsageEvent> = state
            .usage
            .values()
            .filter(|row| row.event.organization_id == *organization && row.period_id == period_id)
            .collect();
        rows.sort_by_key(|row| row.event_seq);
        let events: Vec<UsageEvent> = rows.into_iter().map(|row| row.event.clone()).collect();
        drop(state);
        Ok(fold_usage(organization, &events)?)
    }

    fn usage_totals_in_period(
        &self,
        organization: &OrganizationId,
        period_id: &str,
    ) -> Result<UsageTotals, BillingStoreError> {
        Ok(self.usage_fold_in_period(organization, period_id)?.totals)
    }

    fn put_period_config(
        &self,
        organization: &OrganizationId,
        anchor_ms: i64,
        period_ms: i64,
    ) -> Result<(), BillingStoreError> {
        if anchor_ms < 0 || period_ms <= 0 || period_ms > faktor_core::MAX_BILLING_PERIOD_MS {
            return Err(BillingStoreError::Malformed(
                "period configuration is outside the legal domain".into(),
            ));
        }
        self.lock()?
            .period_configs
            .insert(organization.as_str().to_string(), (anchor_ms, period_ms));
        Ok(())
    }

    fn period_config(
        &self,
        organization: &OrganizationId,
    ) -> Result<Option<(i64, i64)>, BillingStoreError> {
        Ok(self
            .lock()?
            .period_configs
            .get(organization.as_str())
            .copied())
    }

    fn outbox_watermark(&self, organization: &OrganizationId) -> Result<i64, BillingStoreError> {
        Ok(self
            .lock()?
            .outbox_watermarks
            .get(organization.as_str())
            .copied()
            .unwrap_or(0))
    }

    fn advance_outbox_watermark(
        &self,
        organization: &OrganizationId,
        last_outbox_id: i64,
    ) -> Result<(), BillingStoreError> {
        let mut state = self.lock()?;
        let entry = state
            .outbox_watermarks
            .entry(organization.as_str().to_string())
            .or_insert(0);
        *entry = (*entry).max(last_outbox_id);
        Ok(())
    }

    fn correction_exists(
        &self,
        organization: &OrganizationId,
        base: &str,
    ) -> Result<bool, BillingStoreError> {
        Ok(self
            .lock()?
            .usage_corrections
            .contains_key(&(organization.as_str().to_string(), base.to_string())))
    }

    fn append_credit_entry(&self, entry: &CreditEntry) -> Result<CreditAppend, BillingStoreError> {
        validate_credit_amount(entry)?;
        // The store is a public seam: enforce the entry SHAPE too (a settle
        // or refund without a reference, an oversized reason/key, a grant
        // that references another entry). Without this the balance guard
        // would `expect` on a malformed settle and panic the daemon instead
        // of returning a typed refusal.
        entry
            .validate()
            .map_err(|error| BillingStoreError::Malformed(error.to_string()))?;
        let mut state = self.lock()?;
        claim_credit(&mut state, entry)
    }

    fn credit_entries(
        &self,
        organization: &OrganizationId,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<StoredCreditEntry>, BillingStoreError> {
        let state = self.lock()?;
        let mut rows: Vec<StoredCreditEntry> = state
            .credit_order
            .get(organization.as_str())
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| state.credits.get(id).cloned())
                    .filter(|row| row.entry_seq > after_seq)
                    .take(limit)
                    .collect()
            })
            .unwrap_or_default();
        rows.sort_by_key(|row| row.entry_seq);
        Ok(rows)
    }

    fn credit_entry_by_idempotency_key(
        &self,
        organization: &OrganizationId,
        key: &str,
    ) -> Result<Option<StoredCreditEntry>, BillingStoreError> {
        let state = self.lock()?;
        Ok(state
            .credit_idempotency
            .get(&(organization.as_str().to_string(), key.to_string()))
            .and_then(|id| state.credits.get(id))
            .cloned())
    }

    fn credit_balance(
        &self,
        organization: &OrganizationId,
    ) -> Result<CreditBalance, BillingStoreError> {
        Ok(self
            .lock()?
            .credit_balances
            .get(organization.as_str())
            .copied()
            .unwrap_or_default())
    }

    fn begin_in_flight(&self, txn: &InFlightTxn) -> Result<(), BillingStoreError> {
        let mut state = self.lock()?;
        if let Some(existing) = state.in_flight.get(txn.id.as_str()) {
            if existing.organization != txn.organization {
                return Err(BillingStoreError::Malformed(format!(
                    "in-flight transaction {} is already owned by another organization; \
                     refusing a cross-tenant overwrite",
                    txn.id
                )));
            }
        }
        state
            .in_flight
            .insert(txn.id.as_str().to_string(), txn.clone());
        Ok(())
    }

    fn end_in_flight(
        &self,
        organization: &OrganizationId,
        id: &InFlightTxnId,
        at_ms: i64,
    ) -> Result<bool, BillingStoreError> {
        let mut state = self.lock()?;
        let Some(row) = state.in_flight.get_mut(id.as_str()) else {
            return Ok(false);
        };
        if row.organization != *organization {
            return Ok(false);
        }
        // Close exactly once: a second close changes nothing and reports
        // `false` (the same contract the SQLite store's guarded UPDATE has).
        if row.ended_ms.is_some() {
            return Ok(false);
        }
        row.ended_ms = Some(at_ms);
        Ok(true)
    }

    fn in_flight(
        &self,
        organization: &OrganizationId,
    ) -> Result<Vec<InFlightTxn>, BillingStoreError> {
        Ok(self
            .lock()?
            .in_flight
            .values()
            .filter(|row| row.organization == *organization)
            .cloned()
            .collect())
    }

    fn claim_billing_mutation(
        &self,
        organization: &OrganizationId,
        key: &str,
        request: &str,
        at_ms: i64,
    ) -> Result<BillingMutationClaim, BillingStoreError> {
        let mut state = self.lock()?;
        let map_key = report_key(organization.as_str(), key);
        if let Some((existing, response, _)) = state.mutations.get(&map_key) {
            if existing == request {
                return Ok(BillingMutationClaim::Replay {
                    response: response.clone(),
                });
            }
            return Ok(BillingMutationClaim::Conflict {
                request: existing.clone(),
            });
        }
        state
            .mutations
            .insert(map_key, (request.to_string(), None, at_ms));
        // Bounded memory: keep the newest keys for this organization.
        let prefix = format!("{}\0", organization.as_str());
        let mut keys: Vec<(String, i64)> = state
            .mutations
            .iter()
            .filter(|(key, _)| key.starts_with(&prefix))
            .map(|(key, (_, _, at))| (key.clone(), *at))
            .collect();
        if keys.len() > MAX_BILLING_MUTATIONS_PER_ORG {
            keys.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
            let drop = keys.len() - MAX_BILLING_MUTATIONS_PER_ORG;
            for (key, _) in keys.into_iter().take(drop) {
                state.mutations.remove(&key);
            }
        }
        Ok(BillingMutationClaim::Fresh)
    }

    fn billing_mutation_response(
        &self,
        organization: &OrganizationId,
        key: &str,
    ) -> Result<Option<String>, BillingStoreError> {
        Ok(self
            .lock()?
            .mutations
            .get(&report_key(organization.as_str(), key))
            .and_then(|(_, response, _)| response.clone()))
    }

    fn finish_billing_mutation(
        &self,
        organization: &OrganizationId,
        key: &str,
        response: &str,
    ) -> Result<(), BillingStoreError> {
        let mut state = self.lock()?;
        let Some(row) = state
            .mutations
            .get_mut(&report_key(organization.as_str(), key))
        else {
            return Err(BillingStoreError::Malformed(
                "an idempotency key was finished before it was claimed".into(),
            ));
        };
        row.1 = Some(response.to_string());
        Ok(())
    }

    fn put_report_period(&self, row: &ReportPeriodRow) -> Result<(), BillingStoreError> {
        let mut state = self.lock()?;
        let key = report_key(&row.organization_id, &row.period);
        state.report_periods.insert(key, row.clone());
        prune_report_periods(
            &mut state.report_periods,
            row.organization_id.as_str(),
            MAX_REPORT_PERIODS_PER_ORG,
        );
        Ok(())
    }

    fn report_period(
        &self,
        organization: &OrganizationId,
        period: &str,
    ) -> Result<Option<ReportPeriodRow>, BillingStoreError> {
        Ok(self
            .lock()?
            .report_periods
            .get(&report_key(organization.as_str(), period))
            .cloned())
    }

    fn report_periods(
        &self,
        organization: &OrganizationId,
        limit: usize,
    ) -> Result<Vec<ReportPeriodRow>, BillingStoreError> {
        let state = self.lock()?;
        let mut rows: Vec<ReportPeriodRow> = state
            .report_periods
            .values()
            .filter(|row| row.organization_id == organization.as_str())
            .cloned()
            .collect();
        rows.sort_by(|a, b| {
            b.first_seen_ms
                .cmp(&a.first_seen_ms)
                .then_with(|| b.period.cmp(&a.period))
        });
        rows.truncate(limit);
        Ok(rows)
    }
}
