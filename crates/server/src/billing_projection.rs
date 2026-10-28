//! Durable settlement-outbox projection (commercial metering).
//!
//! The session store writes one [`faktor_store::BillingOutboxRow`] in the
//! SAME transaction that settles a reservation. This module drains those
//! rows incrementally into the organization's cloud usage ledger:
//!
//! - the projector's cursor is the cloud store's durable
//!   `billing_outbox_watermark`, so a restart resumes exactly where the last
//!   successful projection stopped;
//! - projection is idempotent per `(organization, source_key)` (the
//!   reservation identity), so a crash between the ledger append and the
//!   watermark advance replays without double counting;
//! - admission calls [`BillingSpendProjector::drain`] before evaluating the
//!   gate: either the outbox is fully drained (the quota sees every settled
//!   row) or the drain refuses typed — it never admits against a partial
//!   projection;
//! - the cost is O(new outbox rows), never a scan of every session/task/
//!   reservation. The legacy one-time backfill (pre-outbox rows) is a
//!   separate bounded UI-triggered pass and is NOT on the admission path.

use std::sync::Arc;

use faktor_cloud::{
    BillingAccountId, ControlPlaneError, DurableSpendRow, EntitlementService, OrganizationId,
    SpendCategory,
};
use faktor_session::SessionManager;

/// Bound on one outbox projection page.
pub const MAX_OUTBOX_PAGE: usize = 512;
/// Bound on one admission-time drain. A backlog past this refuses admission
/// (fail closed) instead of evaluating the quota against a prefix.
pub const MAX_ADMISSION_OUTBOX_ROWS: usize = 10_000;
/// Bound on one UI-triggered catch-up pass.
pub const MAX_CATCHUP_OUTBOX_ROWS: usize = 10_000;

/// A typed projection failure. Every variant refuses; none is treated as an
/// empty projection (which would under-report commercial spend).
#[derive(Debug, thiserror::Error)]
pub enum ProjectionError {
    #[error("settlement outbox read failed: {0}")]
    Outbox(String),
    #[error("usage projection failed: {0}")]
    Billing(#[from] ControlPlaneError),
    #[error(
        "settlement outbox backlog exceeds the bounded projection window \
         ({projected} projected, more remain); refusing to evaluate quota against a prefix"
    )]
    Lag { projected: usize },
}

/// The outcome of one bounded drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProjectionReport {
    /// New usage events appended (duplicates from a crash replay are not
    /// counted here).
    pub appended: usize,
    /// Outbox rows consumed by this drain.
    pub rows: usize,
    /// True when a bounded window was exhausted and more rows remain.
    pub pending: bool,
}

/// Drains the session settlement outbox into the cloud usage ledger.
pub struct BillingSpendProjector {
    service: Arc<EntitlementService>,
    organization: OrganizationId,
    session: Arc<SessionManager>,
}

impl std::fmt::Debug for BillingSpendProjector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BillingSpendProjector")
            .field("organization", &self.organization)
            .finish_non_exhaustive()
    }
}

impl BillingSpendProjector {
    pub fn new(
        service: Arc<EntitlementService>,
        organization: OrganizationId,
        session: Arc<SessionManager>,
    ) -> Self {
        Self {
            service,
            organization,
            session,
        }
    }

    pub fn organization(&self) -> &OrganizationId {
        &self.organization
    }

    /// Drain up to `max_rows` outbox rows. Returns `pending = true` when the
    /// bound was reached with rows still queued (the caller decides whether
    /// that is a UI catch-up continuation or an admission refusal).
    pub fn drain(&self, max_rows: usize) -> Result<ProjectionReport, ProjectionError> {
        // The local settlement rows belong to the configured billing
        // organization. A foreign organization still reads its own durable
        // ledger, but it can never claim the local sessions' spend.
        if let Some(configured) = self.service.billing_organization() {
            if configured != &self.organization {
                return Ok(ProjectionReport::default());
            }
        }
        let Some(account) = self
            .service
            .default_account(&self.organization)
            .map_err(ProjectionError::Billing)?
        else {
            // Nothing can be attributed. If rows are waiting, refusing is
            // the only non-underreporting answer; with an empty outbox the
            // zero projection is exact.
            let watermark = self
                .service
                .store()
                .outbox_watermark(&self.organization)
                .map_err(|e| ProjectionError::Outbox(e.to_string()))?;
            if !self
                .session
                .store()
                .billing_outbox_after(watermark, 1)
                .map_err(|e| ProjectionError::Outbox(e.to_string()))?
                .is_empty()
            {
                return Err(ProjectionError::Outbox(
                    "settlement outbox has pending rows but no billing account is provisioned"
                        .into(),
                ));
            }
            return Ok(ProjectionReport::default());
        };
        // Keep the session store's period stamp aligned with the durable
        // subscription/plan (a plan change updates it on the next drain).
        if let Some((anchor, period_ms)) = self
            .service
            .period_config(&self.organization)
            .map_err(ProjectionError::Billing)?
        {
            let store = self.session.store();
            let current = store
                .billing_period_config()
                .map_err(|e| ProjectionError::Outbox(e.to_string()))?;
            if current != Some((anchor, period_ms)) {
                store
                    .set_billing_period_config(anchor, period_ms)
                    .map_err(|e| ProjectionError::Outbox(e.to_string()))?;
            }
        }
        let mut report = ProjectionReport::default();
        let mut watermark = self
            .service
            .store()
            .outbox_watermark(&self.organization)
            .map_err(|e| ProjectionError::Outbox(e.to_string()))?;
        while report.rows < max_rows {
            let page = (max_rows - report.rows).min(MAX_OUTBOX_PAGE);
            let rows = self
                .session
                .store()
                .billing_outbox_after(watermark, page)
                .map_err(|e| ProjectionError::Outbox(e.to_string()))?;
            if rows.is_empty() {
                return Ok(report);
            }
            for row in rows {
                let attributed = !row.provider.is_empty();
                let projected = DurableSpendRow {
                    organization_id: self.organization.as_str().to_string(),
                    session_id: row.session_id.raw(),
                    task_id: row.task_id.raw(),
                    reservation_id: row.reservation_id,
                    attempt_id: row.attempt_id.clone(),
                    provider: if attributed {
                        row.provider.clone()
                    } else {
                        faktor_session::UNATTRIBUTED_PROVENANCE.to_string()
                    },
                    model: if row.model.is_empty() {
                        faktor_session::UNATTRIBUTED_PROVENANCE.to_string()
                    } else {
                        row.model.clone()
                    },
                    input_tokens: row.uncached_input_tokens,
                    output_tokens: row.output_tokens,
                    cache_read_tokens: row.cache_read_tokens,
                    cache_write_tokens: row.cache_write_tokens,
                    reasoning_tokens: row.reasoning_tokens,
                    provider_cost_micro: row.provider_cost_micro,
                    price_authority: row.price_authority,
                    provider_reported_micro: row.provider_reported_micro.unwrap_or(0),
                    state: row.state.clone(),
                    occurred_at_ms: row.occurred_at_ms,
                    source_operation: row.source_operation.clone(),
                };
                // An unattributed settlement (numeric/legacy finalize) is
                // classified conservatively as managed: undercounting quota
                // would admit spend the account did not fund.
                let category = if attributed {
                    self.service.config().category_of(&row.provider)
                } else {
                    SpendCategory::Managed
                };
                let ingest = self.service.ingest_spend_row(
                    &self.organization,
                    &account,
                    &projected,
                    category,
                )?;
                report.appended += ingest.appended;
                report.rows += 1;
                watermark = row.outbox_id;
                // Advance after each row: a crash replays at most one row,
                // and the source-key idempotency makes that replay a no-op.
                self.service
                    .store()
                    .advance_outbox_watermark(&self.organization, watermark)
                    .map_err(|e| ProjectionError::Outbox(e.to_string()))?;
            }
        }
        report.pending = !self
            .session
            .store()
            .billing_outbox_after(watermark, 1)
            .map_err(|e| ProjectionError::Outbox(e.to_string()))?
            .is_empty();
        Ok(report)
    }

    /// Drain with the admission bound: a backlog past it is a typed refusal.
    pub fn drain_for_admission(&self) -> Result<ProjectionReport, ProjectionError> {
        let report = self.drain(MAX_ADMISSION_OUTBOX_ROWS)?;
        if report.pending {
            return Err(ProjectionError::Lag {
                projected: report.rows,
            });
        }
        Ok(report)
    }

    /// The number of outbox rows not yet projected (for diagnostics/UI).
    pub fn pending_rows(&self) -> Result<usize, ProjectionError> {
        let watermark = self
            .service
            .store()
            .outbox_watermark(&self.organization)
            .map_err(|e| ProjectionError::Outbox(e.to_string()))?;
        Ok(self
            .session
            .store()
            .billing_outbox_after(watermark, MAX_ADMISSION_OUTBOX_ROWS + 1)
            .map_err(|e| ProjectionError::Outbox(e.to_string()))?
            .len())
    }

    /// The account id the projection attributes rows to (the service's
    /// default account), if provisioned.
    pub fn account(&self) -> Result<Option<BillingAccountId>, ProjectionError> {
        self.service
            .default_account(&self.organization)
            .map_err(ProjectionError::Billing)
    }
}
