//! The generic REST billing-vendor adapter: reports the DURABLE usage
//! aggregate of one organization + billing period to an operator-configured
//! vendor endpoint through the daemon's ONE checked transport.
//!
//! Invariants:
//!
//! - **The local ledger stays the source of truth.** The adapter is
//!   REPORT-ONLY: it reads the entitlement service's durable fold and never
//!   appends a usage/credit row. A vendor outage delays reporting; it never
//!   changes what the local ledger recorded;
//! - **Idempotent per report.** Every report carries an `Idempotency-Key`
//!   derived from `(organization, period, cursor)`, so a retry (or a crash
//!   between the vendor's write and the caller's acknowledgement) replays
//!   the SAME key and the vendor de-duplicates it;
//! - **Cursor semantics.** The vendor may answer `next_cursor` to continue a
//!   period's report; [`BillingVendorAdapter::report_period_to_completion`]
//!   follows cursors within a hard page bound, each page under its own key;
//! - **Retry semantics.** Transport failures, 429 (honoring `retry-after`)
//!   and 5xx are retried within the configured attempt bound; every other
//!   4xx refusal is final. Exhausted attempts surface typed — never a silent
//!   drop;
//! - **Credentials** come from the configured auth environment variable
//!   (`auth_env`), read at construction; the token never appears in `Debug`
//!   output or error text.

use std::sync::Arc;
use std::time::Duration;

use faktor_provider::egress::{
    execute_raw, EgressError, HttpTransport, RawRequest, RawResponse, ResponseBudget, RouteLabel,
    MAX_RAW_RESPONSE_BYTES,
};

use crate::billing::UsageTotals;
use crate::entitlements::EntitlementService;
use crate::error::ControlPlaneError;
use crate::ids::OrganizationId;

/// Bound on the report attempts of ONE page (1 = no retry).
pub const MAX_REPORT_ATTEMPTS: u32 = 5;
/// Bound on one period/cursor text field.
pub const MAX_REPORT_TEXT_BYTES: usize = 128;
/// Hard page bound of [`BillingVendorAdapter::report_period_to_completion`].
pub const MAX_REPORT_PAGES: usize = 16;
/// Hard ceiling on one retry delay.
pub const MAX_RETRY_DELAY_MS: i64 = 30_000;
/// Bound on the per-task rows attached to one report.
pub const MAX_REPORT_TASKS: usize = 512;
/// The default vendor report path.
pub const DEFAULT_REPORT_PATH: &str = "/v1/usage-reports";

/// Documented wall-clock bound for ONE vendor report attempt. The shared
/// egress client only bounds connect, so a vendor that accepts and then
/// stalls would otherwise pin the reporting loop forever. Retries are
/// bounded by the configured attempt count, so one page cannot exceed
/// `max_attempts × (VENDOR_HTTP_TIMEOUT_MS + backoff)`.
pub const VENDOR_HTTP_TIMEOUT_MS: u64 = 30_000;

/// Execute one report attempt under [`VENDOR_HTTP_TIMEOUT_MS`]. A policy
/// denial stays the final `Forbidden`; a breach is a retryable `Backend`
/// naming the bound.
async fn execute_raw_bounded(
    transport: &dyn HttpTransport,
    request: RawRequest,
) -> Result<RawResponse, ControlPlaneError> {
    // Every vendor-page read passes an explicit response budget: head/idle/
    // total all equal the documented attempt bound, and the body is capped
    // by the seam's materialization bound.
    let budget = ResponseBudget::for_timeout(
        Duration::from_millis(VENDOR_HTTP_TIMEOUT_MS),
        MAX_RAW_RESPONSE_BYTES as u64,
    );
    match tokio::time::timeout(
        Duration::from_millis(VENDOR_HTTP_TIMEOUT_MS),
        execute_raw(transport, request, &budget),
    )
    .await
    {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(EgressError::Denied { url, .. })) => Err(ControlPlaneError::Forbidden(format!(
            "billing vendor egress to {url} denied by policy"
        ))),
        Ok(Err(e)) => Err(ControlPlaneError::Backend(format!(
            "billing vendor transport: {e}"
        ))),
        Err(_) => Err(ControlPlaneError::Backend(format!(
            "billing vendor report exceeded the {VENDOR_HTTP_TIMEOUT_MS} ms network bound"
        ))),
    }
}

/// The adapter's strict configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillingVendorConfig {
    /// Absolute http(s) base URL (no trailing slash).
    pub base_url: String,
    /// The report endpoint path (must start with `/`).
    pub report_path: String,
    /// Name of the environment variable carrying the bearer credential.
    /// The VALUE is never stored on the config.
    pub auth_env: String,
    pub user_agent: String,
    pub max_attempts: u32,
    pub retry_base_ms: i64,
}

impl Default for BillingVendorConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            report_path: DEFAULT_REPORT_PATH.to_string(),
            auth_env: "FAKTOR_BILLING_VENDOR_TOKEN".to_string(),
            user_agent: "faktor-cloud/0.1".to_string(),
            max_attempts: MAX_REPORT_ATTEMPTS,
            retry_base_ms: 250,
        }
    }
}

impl BillingVendorConfig {
    /// Strict validation (URL shape, bounded texts, bounded attempts).
    pub fn validate(&self) -> Result<(), ControlPlaneError> {
        let base = self.base_url.trim_end_matches('/');
        if !(base.starts_with("https://") || base.starts_with("http://")) {
            return Err(ControlPlaneError::Config(
                "billing vendor base_url must be an absolute http(s) URL".into(),
            ));
        }
        if base.contains(char::is_whitespace) || base.contains('@') {
            return Err(ControlPlaneError::Config(
                "billing vendor base_url must not contain whitespace or userinfo".into(),
            ));
        }
        if !self.report_path.starts_with('/') || self.report_path.len() > MAX_REPORT_TEXT_BYTES {
            return Err(ControlPlaneError::Config(
                "billing vendor report_path must start with '/' and be bounded".into(),
            ));
        }
        if self.auth_env.is_empty() || self.auth_env.len() > MAX_REPORT_TEXT_BYTES {
            return Err(ControlPlaneError::Config(
                "billing vendor auth_env must name a bounded environment variable".into(),
            ));
        }
        if self.user_agent.is_empty() || self.user_agent.len() > 256 {
            return Err(ControlPlaneError::Config(
                "billing vendor user_agent must be 1..=256 bytes".into(),
            ));
        }
        if self.max_attempts == 0 || self.max_attempts > MAX_REPORT_ATTEMPTS {
            return Err(ControlPlaneError::Config(format!(
                "billing vendor max_attempts must be 1..={MAX_REPORT_ATTEMPTS}"
            )));
        }
        if self.retry_base_ms < 0 || self.retry_base_ms > MAX_RETRY_DELAY_MS {
            return Err(ControlPlaneError::Config(format!(
                "billing vendor retry_base_ms must be 0..={MAX_RETRY_DELAY_MS}"
            )));
        }
        Ok(())
    }

    fn endpoint(&self) -> String {
        format!(
            "{}{}",
            self.base_url.trim_end_matches('/'),
            self.report_path
        )
    }
}

/// One report outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportOutcome {
    pub organization: OrganizationId,
    pub period: String,
    /// The cursor this page reported (`None` = the period head).
    pub cursor: Option<String>,
    /// The vendor's continuation cursor, when it asked for another page.
    pub next_cursor: Option<String>,
    /// The ledger totals this report carried.
    pub totals: UsageTotals,
    /// Attempts spent on this page (1 = no retry).
    pub attempts: u32,
    /// The vendor reported this exact report as an idempotent replay.
    pub idempotent_replay: bool,
}

/// The report-only REST adapter over one [`EntitlementService`].
pub struct BillingVendorAdapter {
    service: Arc<EntitlementService>,
    config: BillingVendorConfig,
    transport: Arc<dyn HttpTransport>,
    auth: Option<String>,
}

impl std::fmt::Debug for BillingVendorAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BillingVendorAdapter")
            .field("base_url", &self.config.base_url)
            .field("auth_env", &self.config.auth_env)
            .field("authenticated", &self.auth.is_some())
            .finish_non_exhaustive()
    }
}

impl BillingVendorAdapter {
    /// Build the adapter with an explicit bearer credential (`None` = no
    /// `Authorization` header; tests and unauthenticated local mocks).
    pub fn new(
        service: Arc<EntitlementService>,
        config: BillingVendorConfig,
        transport: Arc<dyn HttpTransport>,
        auth: Option<String>,
    ) -> Result<Self, ControlPlaneError> {
        config.validate()?;
        if let Some(auth) = &auth {
            if auth.is_empty() || auth.len() > 4096 || auth.contains(char::is_whitespace) {
                return Err(ControlPlaneError::Config(
                    "billing vendor credential has an illegal shape".into(),
                ));
            }
        }
        Ok(Self {
            service,
            config,
            transport,
            auth,
        })
    }

    /// Build the adapter with the credential resolved from the configured
    /// environment variable. A missing/empty variable is a typed
    /// configuration refusal — never a silently unauthenticated report.
    pub fn from_env(
        service: Arc<EntitlementService>,
        config: BillingVendorConfig,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self, ControlPlaneError> {
        config.validate()?;
        let auth = std::env::var(&config.auth_env).map_err(|_| {
            ControlPlaneError::Config(format!(
                "billing vendor credential environment variable {} is not set",
                config.auth_env
            ))
        })?;
        if auth.trim().is_empty() {
            return Err(ControlPlaneError::Config(format!(
                "billing vendor credential environment variable {} is empty",
                config.auth_env
            )));
        }
        Self::new(service, config, transport, Some(auth))
    }

    /// The deterministic idempotency key of one report page.
    pub fn idempotency_key(
        organization: &OrganizationId,
        period: &str,
        cursor: Option<&str>,
    ) -> String {
        format!(
            "faktor-usage-report:v1:{}:{}:{}",
            organization.as_str(),
            period,
            cursor.unwrap_or("head")
        )
    }

    fn retry_delay_ms(&self, attempt: u32, retry_after_ms: Option<i64>) -> i64 {
        if let Some(retry_after) = retry_after_ms {
            return retry_after.clamp(0, MAX_RETRY_DELAY_MS);
        }
        self.config
            .retry_base_ms
            .saturating_mul(1i64 << attempt.min(6))
            .clamp(0, MAX_RETRY_DELAY_MS)
    }

    /// Report ONE page: the durable aggregate of `(organization, period)`
    /// under the given vendor cursor. Idempotent per page key.
    pub async fn report_period(
        &self,
        organization: &OrganizationId,
        period: &str,
        cursor: Option<&str>,
    ) -> Result<ReportOutcome, ControlPlaneError> {
        validate_report_text("period", period)?;
        if let Some(cursor) = cursor {
            validate_report_text("cursor", cursor)?;
        }
        // The ledger is the source of truth: the aggregate is the durable
        // fold, computed HERE and never persisted by the adapter.
        let fold = self.service.fold(organization)?;
        let totals = fold.totals.clone();
        let tasks: Vec<serde_json::Value> = fold
            .per_task
            .iter()
            .take(MAX_REPORT_TASKS)
            .map(|task| {
                serde_json::json!({
                    "task_id": task.task_id,
                    "run_id": task.run_id,
                    "totals": task.totals,
                })
            })
            .collect();
        let body = serde_json::json!({
            "organization": organization.as_str(),
            "period": period,
            "cursor": cursor,
            "aggregate": totals,
            "tasks": tasks,
            "ledger_next_cursor": fold.next_cursor,
        });
        let body = serde_json::to_vec(&body).map_err(|e| {
            ControlPlaneError::Malformed(format!("billing vendor report body: {e}"))
        })?;
        let url = self.config.endpoint();
        let key = Self::idempotency_key(organization, period, cursor);
        let mut attempt = 0u32;
        loop {
            let last_attempt = attempt + 1 >= self.config.max_attempts;
            let mut request = RawRequest::new("POST", url.clone())
                .route(RouteLabel::BillingVendorReport)
                .header("accept", "application/json")
                .header("content-type", "application/json")
                .header("user-agent", self.config.user_agent.clone())
                .header("idempotency-key", key.clone());
            if let Some(auth) = &self.auth {
                request = request.header("authorization", format!("Bearer {auth}"));
            }
            let request = request.bytes_body(body.clone());
            let (error, retry_after_ms): (ControlPlaneError, Option<i64>) =
                match execute_raw_bounded(self.transport.as_ref(), request).await {
                    Ok(response) => match self.classify(response, &key)? {
                        Ok(page) => {
                            return Ok(ReportOutcome {
                                organization: organization.clone(),
                                period: period.to_string(),
                                cursor: cursor.map(str::to_string),
                                next_cursor: page.next_cursor,
                                totals,
                                attempts: attempt + 1,
                                idempotent_replay: page.idempotent_replay,
                            });
                        }
                        Err(retry) => (retry.error, retry.retry_after_ms),
                    },
                    // Policy denials map to the final `Forbidden`; transport
                    // failures and bound breaches are retryable `Backend`
                    // errors within the attempt bound.
                    Err(error) => (error, None),
                };
            if !report_retryable(&error) || last_attempt {
                return Err(error);
            }
            let delay = self.retry_delay_ms(attempt, retry_after_ms);
            attempt += 1;
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay as u64)).await;
            }
        }
    }

    /// Follow the vendor's `next_cursor` chain within [`MAX_REPORT_PAGES`].
    /// Each page has its own idempotency key; re-running the whole call after
    /// a crash replays the recorded keys in order (the vendor de-duplicates
    /// every already-accepted page).
    pub async fn report_period_to_completion(
        &self,
        organization: &OrganizationId,
        period: &str,
    ) -> Result<(Vec<ReportOutcome>, Option<String>), ControlPlaneError> {
        let mut pages = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_REPORT_PAGES {
            let outcome = self
                .report_period(organization, period, cursor.as_deref())
                .await?;
            let next = outcome.next_cursor.clone();
            pages.push(outcome);
            match next {
                Some(next) => cursor = Some(next),
                None => return Ok((pages, None)),
            }
        }
        Err(ControlPlaneError::Conflict(format!(
            "billing vendor asked for more than {MAX_REPORT_PAGES} report pages of period {period}; refusing an unbounded report loop"
        )))
    }

    /// One vendor response → the page continuation, or the typed refusal
    /// with its retry metadata.
    fn classify(
        &self,
        response: RawResponse,
        key: &str,
    ) -> Result<Result<VendorPage, VendorRetry>, ControlPlaneError> {
        let status = response.status;
        let excerpt = bounded_excerpt(&response.body_text());
        match status {
            200..=299 => self.parse_page(&response).map(Ok),
            401 => Err(ControlPlaneError::Unauthorized(
                "billing vendor rejected the credential (401)".to_string(),
            )),
            403 => Err(ControlPlaneError::Forbidden(format!(
                "billing vendor refused (403: {excerpt})"
            ))),
            404 => Err(ControlPlaneError::NotFound(
                "billing vendor report endpoint not found (404)".into(),
            )),
            409 => Err(ControlPlaneError::Conflict(format!(
                "billing vendor idempotency conflict for key {key}: {excerpt}"
            ))),
            429 => {
                let retry_after_ms = response
                    .header("retry-after")
                    .and_then(|v| v.trim().parse::<i64>().ok())
                    .map(|secs| secs.max(0).saturating_mul(1000));
                Ok(Err(VendorRetry {
                    error: ControlPlaneError::Backend(format!(
                        "billing vendor rate limited (429): {excerpt}"
                    )),
                    retry_after_ms,
                }))
            }
            500..=599 => Ok(Err(VendorRetry {
                error: ControlPlaneError::Backend(format!(
                    "billing vendor failed ({status}): {excerpt}"
                )),
                retry_after_ms: None,
            })),
            other => Err(ControlPlaneError::Malformed(format!(
                "billing vendor refused ({other}): {excerpt}"
            ))),
        }
    }

    fn parse_page(&self, response: &RawResponse) -> Result<VendorPage, ControlPlaneError> {
        if response.body.is_empty() {
            return Ok(VendorPage::default());
        }
        let json: serde_json::Value = serde_json::from_slice(&response.body).map_err(|e| {
            ControlPlaneError::Malformed(format!(
                "billing vendor response is unparseable (status {}): {e}",
                response.status
            ))
        })?;
        let next_cursor = match json.get("next_cursor") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(cursor)) => {
                validate_report_text("next_cursor", cursor)?;
                Some(cursor.clone())
            }
            Some(other) => {
                return Err(ControlPlaneError::Malformed(format!(
                    "billing vendor next_cursor has an illegal shape: {other}"
                )));
            }
        };
        let idempotent_replay = json
            .get("idempotent_replay")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        Ok(VendorPage {
            next_cursor,
            idempotent_replay,
        })
    }
}

#[derive(Default)]
struct VendorPage {
    next_cursor: Option<String>,
    idempotent_replay: bool,
}

struct VendorRetry {
    error: ControlPlaneError,
    retry_after_ms: Option<i64>,
}

/// Whether one report failure may be retried: a transport failure, a
/// rate-limited answer and a 5xx are transient; every other refusal
/// (including every other 4xx and the adapter's own config/malformed
/// failures) is final.
fn report_retryable(error: &ControlPlaneError) -> bool {
    matches!(error, ControlPlaneError::Backend(_))
}

fn validate_report_text(field: &str, value: &str) -> Result<(), ControlPlaneError> {
    if value.is_empty() || value.len() > MAX_REPORT_TEXT_BYTES {
        return Err(ControlPlaneError::Malformed(format!(
            "billing vendor {field} must be 1..={MAX_REPORT_TEXT_BYTES} bytes"
        )));
    }
    if !value.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(ControlPlaneError::Malformed(format!(
            "billing vendor {field} must be printable ASCII without whitespace"
        )));
    }
    Ok(())
}

fn bounded_excerpt(text: &str) -> String {
    const MAX: usize = 200;
    if text.len() <= MAX {
        return text.to_string();
    }
    let mut end = MAX;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}
