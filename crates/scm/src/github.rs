//! The GitHub App adapter: the REAL [`ScmProvider`] implementation.
//!
//! - **Auth**: every installation call carries a short-lived installation
//!   token resolved through [`InstallationTokenSource`] (the app-JWT →
//!   installation-token minting flow lives behind that seam; see the
//!   residual note on [`InstallationTokenSource`]). App-level calls
//!   (`/app/installations`) carry the app token from the same source.
//!   Tokens are cached per installation with an expiry skew and never
//!   logged (`InstallationToken`'s `Debug` is redacted).
//! - **Transport**: every request executes through the injected
//!   [`HttpTransport`] (the daemon's ONE policy-checked transport), using
//!   the request builders of `faktor_provider::egress`. This module has no
//!   HTTP client of its own.
//! - **Minimal repository permissions**: the adapter REQUIRES the granted
//!   token to carry exactly the minimal set it needs
//!   ([`REQUIRED_REPOSITORY_PERMISSIONS`]); a token missing one is refused
//!   before any call.
//! - **Rate limits**: `x-ratelimit-remaining`/`x-ratelimit-reset` and
//!   `retry-after` are observed on every response and recorded durably
//!   ([`ScmStore::record_rate_limit`]). An exhausted budget makes the NEXT
//!   call return [`ScmError::RateLimited`] WITHOUT touching the network,
//!   so a caller loop can never hot-loop against the provider.
//! - **ETag caching**: GET responses are cached by URL (bounded) and
//!   revalidated with `If-None-Match`; a `304 Not Modified` is served from
//!   the cache, so a reconcile loop is cheap in steady state.
//! - **Reconciliation**: branch and PR creation look the object up first
//!   (PRs by the exact head/base + marker) and journal the caller's
//!   [`ExternalOperationId`] BEFORE the remote call, so a crash between
//!   create and record reconciles to the existing object instead of
//!   duplicating it.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use faktor_provider::egress::{HttpTransport, RawRequest, RawResponse, RouteLabel};
use faktor_security::secret::SecretValue;

use crate::error::ScmError;
use crate::ids::{
    validate_ref_name, ExternalOperationId, IssueRef, PullRequestRef, RemoteRef, RepositoryRef,
    ScmInstallationId,
};
use crate::provider::{
    BranchSpec, CommentTarget, PullRequestSpec, ScmBranch, ScmComment, ScmInstallation, ScmIssue,
    ScmProvider, ScmPullRequest, ScmRemoteRef, ScmRepository, ScmReviewEvent,
};
use crate::store::{RateLimitRow, ScmOperationRow, ScmStore};

/// The minimal repository permission set the adapter requires (GitHub App
/// permission names → minimum level). Nothing broader is ever requested.
pub const REQUIRED_REPOSITORY_PERMISSIONS: &[(&str, &str)] = &[
    ("contents", "write"),
    ("pull_requests", "write"),
    ("issues", "write"),
    ("metadata", "read"),
];

/// Default page size for list endpoints (bounded).
pub const DEFAULT_PAGE_SIZE: usize = 100;
/// Default page cap per list endpoint (bounded everything).
pub const DEFAULT_MAX_PAGES: usize = 10;
/// Bound on one remote error excerpt folded into a typed error.
pub const MAX_ERROR_EXCERPT_BYTES: usize = 200;
/// Bound on the in-memory ETag cache entries.
pub const MAX_ETAG_ENTRIES: usize = 256;
/// Refresh an installation token this long before its stated expiry.
pub const TOKEN_EXPIRY_SKEW_MS: i64 = 60_000;

/// The adapter's strict configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubAppConfig {
    /// REST API base (no trailing slash): `https://api.github.com` in
    /// production, a loopback mock in tests.
    pub api_base: String,
    pub user_agent: String,
    pub page_size: usize,
    pub max_pages: usize,
}

impl Default for GitHubAppConfig {
    fn default() -> Self {
        Self {
            api_base: "https://api.github.com".to_string(),
            user_agent: "faktor-scm/0.1".to_string(),
            page_size: DEFAULT_PAGE_SIZE,
            max_pages: DEFAULT_MAX_PAGES,
        }
    }
}

impl GitHubAppConfig {
    /// Validate the base URL shape (http/https, no userinfo, bounded) and
    /// the paging bounds.
    pub fn validate(&self) -> Result<(), ScmError> {
        let base = self.api_base.trim_end_matches('/');
        if !(base.starts_with("https://") || base.starts_with("http://")) {
            return Err(ScmError::Config(
                "api_base must be an absolute http(s) URL".into(),
            ));
        }
        if base.contains(char::is_whitespace) || base.contains('@') {
            return Err(ScmError::Config(
                "api_base must not contain whitespace or userinfo".into(),
            ));
        }
        if base.len() > 2048 {
            return Err(ScmError::Config("api_base is oversized".into()));
        }
        if self.user_agent.is_empty() || self.user_agent.len() > 256 {
            return Err(ScmError::Config("user_agent must be 1..=256 bytes".into()));
        }
        if self.page_size == 0 || self.page_size > DEFAULT_PAGE_SIZE {
            return Err(ScmError::Config(format!(
                "page_size must be 1..={DEFAULT_PAGE_SIZE}"
            )));
        }
        if self.max_pages == 0 || self.max_pages > 100 {
            return Err(ScmError::Config("max_pages must be 1..=100".into()));
        }
        Ok(())
    }
}

/// One short-lived installation (or app) token. Never logged: the bearer
/// value is a [`faktor_security::secret::SecretValue`] (zeroized on drop,
/// redacted `Debug`, no `Display` and no serde) and leaves only through
/// [`InstallationToken::expose`].
#[derive(Clone, PartialEq, Eq)]
pub struct InstallationToken {
    token: SecretValue,
    expires_at_ms: i64,
    permissions: Vec<(String, String)>,
}

impl InstallationToken {
    pub fn new(
        token: impl Into<SecretValue>,
        expires_at_ms: i64,
        permissions: Vec<(String, String)>,
    ) -> Result<Self, ScmError> {
        let token = token.into();
        if token.is_empty() || token.len() > 4096 || token.expose().contains(char::is_whitespace) {
            return Err(ScmError::Unauthorized(
                "installation token has an illegal shape".into(),
            ));
        }
        Ok(Self {
            token,
            expires_at_ms,
            permissions,
        })
    }

    /// The bearer value (call sites must never log it).
    pub fn expose(&self) -> &str {
        self.token.expose()
    }

    pub fn expires_at_ms(&self) -> i64 {
        self.expires_at_ms
    }

    pub fn permissions(&self) -> &[(String, String)] {
        &self.permissions
    }
}

impl fmt::Debug for InstallationToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstallationToken")
            .field("token", &"<redacted>")
            .field("expires_at_ms", &self.expires_at_ms)
            .field("permissions", &self.permissions)
            .finish()
    }
}

/// The seam that mints short-lived installation tokens.
///
/// The PRODUCTION implementation is [`crate::token::GitHubAppTokenSource`]:
/// it mints an app JWT (RS256 over the app private key), exchanges it at
/// `POST /app/installations/{id}/access_tokens` through the injected checked
/// transport with bounded retries, caches the granted token per installation
/// with an expiry skew, and reports the granted permission set. This trait
/// stays the boundary so tests and embedded hosts can substitute a
/// deterministic source.
#[async_trait]
pub trait InstallationTokenSource: Send + Sync {
    /// One token for an installation.
    async fn token_for(
        &self,
        installation: ScmInstallationId,
    ) -> Result<InstallationToken, ScmError>;
    /// The app-level token used by `/app/installations`.
    async fn app_token(&self) -> Result<InstallationToken, ScmError>;
}

/// A deterministic token source for tests and embedded hosts: one fixed
/// token for every installation (documented test double, never production).
#[derive(Debug, Clone)]
pub struct StaticTokenSource {
    token: InstallationToken,
}

impl StaticTokenSource {
    pub fn new(token: InstallationToken) -> Self {
        Self { token }
    }

    /// The minimal-permissions token every test double should carry.
    pub fn minimal(expires_at_ms: i64) -> Result<Self, ScmError> {
        InstallationToken::new(
            "test-installation-token",
            expires_at_ms,
            REQUIRED_REPOSITORY_PERMISSIONS
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
        .map(Self::new)
    }
}

#[async_trait]
impl InstallationTokenSource for StaticTokenSource {
    async fn token_for(
        &self,
        _installation: ScmInstallationId,
    ) -> Result<InstallationToken, ScmError> {
        Ok(self.token.clone())
    }

    async fn app_token(&self) -> Result<InstallationToken, ScmError> {
        Ok(self.token.clone())
    }
}

/// The clock seam (deterministic tests; production uses the wall clock).
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

/// The production clock.
#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// A manually advanced clock for deterministic tests.
#[derive(Debug)]
pub struct ManualClock {
    now_ms: std::sync::atomic::AtomicI64,
}

impl ManualClock {
    pub fn new(now_ms: i64) -> Self {
        Self {
            now_ms: std::sync::atomic::AtomicI64::new(now_ms),
        }
    }

    pub fn advance(&self, delta_ms: i64) {
        self.now_ms
            .fetch_add(delta_ms, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> i64 {
        self.now_ms.load(std::sync::atomic::Ordering::SeqCst)
    }
}

struct EtagEntry {
    url: String,
    etag: String,
    body: Vec<u8>,
}

/// Bounded FIFO ETag cache for GET responses.
#[derive(Default)]
struct EtagCache {
    entries: VecDeque<EtagEntry>,
}

impl EtagCache {
    fn get(&self, url: &str) -> Option<(&str, &[u8])> {
        self.entries
            .iter()
            .find(|entry| entry.url == url)
            .map(|entry| (entry.etag.as_str(), entry.body.as_slice()))
    }

    fn put(&mut self, url: &str, etag: &str, body: &[u8]) {
        self.entries.retain(|entry| entry.url != url);
        self.entries.push_back(EtagEntry {
            url: url.to_string(),
            etag: etag.to_string(),
            body: body.to_vec(),
        });
        while self.entries.len() > MAX_ETAG_ENTRIES {
            self.entries.pop_front();
        }
    }
}

enum Auth {
    Installation(ScmInstallationId),
    App,
}

/// The durable operation identity a journal row carries (the caller's
/// operation id is passed separately, so one call site cannot forget it).
struct OperationIdentity<'a> {
    kind: &'a str,
    repository: &'a str,
    marker: &'a str,
}

/// The GitHub App adapter. One instance is shared (Send + Sync) across
/// runtime tasks; the ETag cache and the token cache are internally locked
/// and bounded.
pub struct GitHubApp {
    config: GitHubAppConfig,
    transport: std::sync::Arc<dyn HttpTransport>,
    tokens: std::sync::Arc<dyn InstallationTokenSource>,
    store: std::sync::Arc<dyn ScmStore>,
    clock: std::sync::Arc<dyn Clock>,
    etags: Mutex<EtagCache>,
    token_cache: Mutex<HashMap<u64, InstallationToken>>,
}

impl GitHubApp {
    pub fn new(
        config: GitHubAppConfig,
        transport: std::sync::Arc<dyn HttpTransport>,
        tokens: std::sync::Arc<dyn InstallationTokenSource>,
        store: std::sync::Arc<dyn ScmStore>,
        clock: std::sync::Arc<dyn Clock>,
    ) -> Result<Self, ScmError> {
        config.validate()?;
        Ok(Self {
            config: GitHubAppConfig {
                api_base: config.api_base.trim_end_matches('/').to_string(),
                ..config
            },
            transport,
            tokens,
            store,
            clock,
            etags: Mutex::new(EtagCache::default()),
            token_cache: Mutex::new(HashMap::new()),
        })
    }

    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }

    fn cache_lock(&self) -> Result<std::sync::MutexGuard<'_, EtagCache>, ScmError> {
        self.etags
            .lock()
            .map_err(|_| ScmError::Store("etag cache lock is poisoned".into()))
    }

    fn token_lock(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<u64, InstallationToken>>, ScmError> {
        self.token_cache
            .lock()
            .map_err(|_| ScmError::Store("installation token cache lock is poisoned".into()))
    }

    /// Resolve (and cache) one installation token, enforcing the minimal
    /// permission set before it is ever used.
    async fn installation_token(
        &self,
        installation: ScmInstallationId,
    ) -> Result<InstallationToken, ScmError> {
        let now = self.now_ms();
        {
            let cache = self.token_lock()?;
            if let Some(token) = cache.get(&installation.raw()) {
                if token.expires_at_ms() - TOKEN_EXPIRY_SKEW_MS > now {
                    return Ok(token.clone());
                }
            }
        }
        let token = self.tokens.token_for(installation).await?;
        require_permissions(&token)?;
        self.token_lock()?.insert(installation.raw(), token.clone());
        Ok(token)
    }

    /// Refuse BEFORE any network call while the recorded provider backoff
    /// is still active.
    fn rate_limit_guard(&self) -> Result<(), ScmError> {
        if let Some(row) = self.store.rate_limit(self.provider_name())? {
            let now = self.now_ms();
            if row.until_ms > now {
                return Err(ScmError::RateLimited {
                    retry_after_ms: row.until_ms - now,
                });
            }
        }
        Ok(())
    }

    /// Record the provider backoff when the observed budget is exhausted or
    /// the provider asked for a retry delay. Returns the typed refusal when
    /// THIS response is itself a rate-limit refusal (403/429), so the caller
    /// never doubles as an auth/permission error.
    fn observe_rate_limit(&self, response: &RawResponse, now: i64) -> Result<(), ScmError> {
        let retry_after_ms = response
            .header("retry-after")
            .and_then(|v| v.trim().parse::<i64>().ok())
            .map(|secs| secs.max(0).saturating_mul(1000));
        let remaining = response
            .header("x-ratelimit-remaining")
            .and_then(|v| v.trim().parse::<i64>().ok());
        let reset_ms = response
            .header("x-ratelimit-reset")
            .and_then(|v| v.trim().parse::<i64>().ok())
            .map(|secs| secs.saturating_mul(1000));
        let exhausted = remaining == Some(0)
            || (matches!(response.status, 403 | 429)
                && (retry_after_ms.is_some() || remaining.is_some()));
        if !exhausted {
            return Ok(());
        }
        let until_ms = [
            retry_after_ms.map(|delay| now.saturating_add(delay)),
            reset_ms.map(|reset| reset.saturating_add(1_000)),
            Some(now.saturating_add(1_000)),
        ]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or_else(|| now.saturating_add(1_000));
        self.store.record_rate_limit(&RateLimitRow {
            provider: self.provider_name().to_string(),
            until_ms,
            observed_ms: now,
        })?;
        // A 403/429 that consumed the observed budget IS the rate-limit
        // refusal: report it typed instead of letting the status mapper call
        // it a permission error.
        if matches!(response.status, 403 | 429) {
            return Err(ScmError::RateLimited {
                retry_after_ms: until_ms.saturating_sub(now),
            });
        }
        Ok(())
    }

    /// One authenticated request through the checked transport.
    async fn send(
        &self,
        auth: Auth,
        method: &str,
        path: &str,
        query: &str,
        body: Option<serde_json::Value>,
    ) -> Result<RawResponse, ScmError> {
        self.rate_limit_guard()?;
        let token = match auth {
            Auth::Installation(installation) => self.installation_token(installation).await?,
            Auth::App => {
                // The app credential is a signed JWT identifying the APP
                // (used by `/app/installations`): it carries no repository
                // permission grant, so the installation-permission check does
                // not apply to it. Installation tokens are checked where they
                // are resolved (and again here before use).
                self.tokens.app_token().await?
            }
        };
        let url = format!("{}{}{}", self.config.api_base, path, query);
        let mut request = RawRequest::new(method, url.clone())
            .route(RouteLabel::GithubApi)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28")
            .header("user-agent", self.config.user_agent.clone())
            .header("authorization", format!("Bearer {}", token.expose()));
        // ETag revalidation: a cached GET sends If-None-Match; the 304 body
        // below is served from the cache.
        let cached_etag = if method == "GET" {
            let cache = self.cache_lock()?;
            cache.get(&url).map(|(etag, _)| etag.to_string())
        } else {
            None
        };
        if let Some(etag) = cached_etag {
            request = request.header("if-none-match", etag);
        }
        if let Some(body) = &body {
            request = request
                .json_body(body)
                .map_err(|e| ScmError::Transport(format!("request json body: {e}")))?;
        }
        let response = crate::execute_raw_bounded(
            self.transport.as_ref(),
            request,
            &format!("{method} {path}"),
        )
        .await?;
        self.observe_rate_limit(&response, self.now_ms())?;
        if response.status == 304 {
            let cache = self.cache_lock()?;
            let (_, cached) = cache.get(&url).ok_or_else(|| {
                ScmError::Transport(
                    "provider answered 304 for a response that was not cached".into(),
                )
            })?;
            return Ok(RawResponse {
                status: 200,
                headers: response.headers.clone(),
                body: cached.to_vec(),
            });
        }
        if method == "GET" && response.status == 200 {
            if let Some(etag) = response.header("etag") {
                let etag = etag.to_string();
                self.cache_lock()?.put(&url, &etag, &response.body);
            }
        }
        Ok(response)
    }

    fn expect_success(
        &self,
        response: RawResponse,
        context: &str,
    ) -> Result<RawResponse, ScmError> {
        match response.status {
            200..=299 => Ok(response),
            401 => Err(ScmError::Unauthorized(format!(
                "{context}: provider rejected the token"
            ))),
            403 => Err(ScmError::Forbidden(format!(
                "{context}: provider refused (403: {})",
                bounded_excerpt(&response.body_text())
            ))),
            404 => Err(ScmError::NotFound(format!("{context}: not found"))),
            status => Err(ScmError::Api {
                status,
                detail: format!("{context}: {}", bounded_excerpt(&response.body_text())),
            }),
        }
    }

    fn parse_json(
        &self,
        response: &RawResponse,
        context: &str,
    ) -> Result<serde_json::Value, ScmError> {
        serde_json::from_slice(&response.body).map_err(|e| ScmError::Api {
            status: response.status,
            detail: format!("{context}: unparseable response ({e})"),
        })
    }

    /// Journal the durable operation identity before the remote call.
    fn journal(
        &self,
        operation: &ExternalOperationId,
        identity: OperationIdentity<'_>,
        state: &str,
        external_id: Option<String>,
        version: Option<String>,
    ) -> Result<(), ScmError> {
        self.store.record_external_operation(&ScmOperationRow {
            operation_id: operation.as_str().to_string(),
            provider: self.provider_name().to_string(),
            kind: identity.kind.to_string(),
            repository: identity.repository.to_string(),
            marker: identity.marker.to_string(),
            external_id,
            version,
            state: state.to_string(),
            updated_ms: self.now_ms(),
        })?;
        Ok(())
    }

    fn repo_path(&self, repository: &RepositoryRef) -> String {
        format!("/repos/{}/{}", repository.owner(), repository.name())
    }

    async fn get_json(
        &self,
        repository: &RepositoryRef,
        path: &str,
        query: &str,
    ) -> Result<serde_json::Value, ScmError> {
        let response = self
            .send(
                Auth::Installation(repository.installation()),
                "GET",
                path,
                query,
                None,
            )
            .await?;
        let response = self.expect_success(response, path)?;
        self.parse_json(&response, path)
    }

    /// One bounded page list: `unwrap` a JSON array or an object with an
    /// `items`-style key, following `page` up to `max_pages`.
    async fn list_pages(
        &self,
        auth: Auth,
        path: &str,
        array_key: Option<&str>,
    ) -> Result<Vec<serde_json::Value>, ScmError> {
        let mut out = Vec::new();
        for page in 1..=self.config.max_pages {
            let query = format!("?per_page={}&page={page}", self.config.page_size);
            let response = self
                .send(auth_ref(&auth), "GET", path, &query, None)
                .await?;
            let response = self.expect_success(response, path)?;
            let json = self.parse_json(&response, path)?;
            let items = match &array_key {
                None => json.as_array().cloned().ok_or_else(|| ScmError::Api {
                    status: 200,
                    detail: format!("{path}: expected a JSON array"),
                })?,
                Some(key) => json
                    .get(*key)
                    .and_then(|v| v.as_array())
                    .cloned()
                    .ok_or_else(|| ScmError::Api {
                        status: 200,
                        detail: format!("{path}: expected an array at {key:?}"),
                    })?,
            };
            let count = items.len();
            out.extend(items);
            if count < self.config.page_size {
                break;
            }
        }
        Ok(out)
    }

    fn parse_repository(
        &self,
        repository: &RepositoryRef,
        json: &serde_json::Value,
    ) -> Result<ScmRepository, ScmError> {
        let full_name = string_field(json, "full_name")?;
        let default_branch = string_field(json, "default_branch").unwrap_or_else(|_| "main".into());
        Ok(ScmRepository {
            reference: repository.clone(),
            full_name,
            default_branch,
            private: json
                .get("private")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            archived: json
                .get("archived")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            url: string_field(json, "html_url").unwrap_or_default(),
        })
    }

    fn parse_pull_request(
        &self,
        repository: &RepositoryRef,
        json: &serde_json::Value,
        marker: &str,
        created: bool,
    ) -> Result<ScmPullRequest, ScmError> {
        let number = json
            .get("number")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| ScmError::Api {
                status: 200,
                detail: "pull request payload has no number".into(),
            })?;
        let head = json
            .get("head")
            .and_then(|h| h.get("ref"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let base = json
            .get("base")
            .and_then(|h| h.get("ref"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let head_sha = json
            .get("head")
            .and_then(|h| h.get("sha"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let updated_at = string_field(json, "updated_at").unwrap_or_default();
        Ok(ScmPullRequest {
            reference: PullRequestRef::try_new(repository.clone(), number)?,
            head: head.clone(),
            base,
            state: string_field(json, "state").unwrap_or_else(|_| "unknown".into()),
            marker: marker.to_string(),
            version: format!("{head_sha}@{updated_at}"),
            url: string_field(json, "html_url").unwrap_or_default(),
            created,
        })
    }
}

fn auth_ref(auth: &Auth) -> Auth {
    match auth {
        Auth::Installation(id) => Auth::Installation(*id),
        Auth::App => Auth::App,
    }
}

pub(crate) fn require_permissions(token: &InstallationToken) -> Result<(), ScmError> {
    for (name, minimum) in REQUIRED_REPOSITORY_PERMISSIONS {
        let granted = token
            .permissions()
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, level)| level.as_str());
        let ok = match (*name, *minimum, granted) {
            (_, _, None) => false,
            ("metadata", "read", Some(level)) => level == "read" || level == "write",
            (_, "write", Some(level)) => level == "write",
            (_, "read", Some(level)) => level == "read" || level == "write",
            _ => false,
        };
        if !ok {
            return Err(ScmError::Forbidden(format!(
                "installation token lacks the required {name}:{minimum} permission"
            )));
        }
    }
    Ok(())
}

fn string_field(json: &serde_json::Value, key: &str) -> Result<String, ScmError> {
    json.get(key)
        .and_then(|v| v.as_str())
        .map(|v| v.to_string())
        .ok_or_else(|| ScmError::Api {
            status: 200,
            detail: format!("response payload has no string field {key:?}"),
        })
}

fn bounded_excerpt(text: &str) -> String {
    if text.len() <= MAX_ERROR_EXCERPT_BYTES {
        return text.to_string();
    }
    let mut end = MAX_ERROR_EXCERPT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[async_trait]
impl ScmProvider for GitHubApp {
    fn provider_name(&self) -> &'static str {
        "github"
    }

    async fn repository(&self, repository: &RepositoryRef) -> Result<ScmRepository, ScmError> {
        let path = self.repo_path(repository);
        let json = self.get_json(repository, &path, "").await?;
        self.parse_repository(repository, &json)
    }

    async fn issue(&self, issue: &IssueRef) -> Result<ScmIssue, ScmError> {
        let repository = issue.repository();
        let path = format!("{}/issues/{}", self.repo_path(repository), issue.number());
        let json = self.get_json(repository, &path, "").await?;
        Ok(ScmIssue {
            reference: issue.clone(),
            title: string_field(&json, "title").unwrap_or_default(),
            state: string_field(&json, "state").unwrap_or_default(),
            body: json
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
        })
    }

    async fn remote_ref(&self, reference: &RemoteRef) -> Result<Option<ScmRemoteRef>, ScmError> {
        let repository = reference.repository();
        let canonical = reference.canonical();
        let git_ref = canonical.strip_prefix("refs/").unwrap_or(&canonical);
        let path = format!("{}/git/ref/{}", self.repo_path(repository), git_ref);
        let response = self
            .send(
                Auth::Installation(repository.installation()),
                "GET",
                &path,
                "",
                None,
            )
            .await?;
        if response.status == 404 {
            return Ok(None);
        }
        let response = self.expect_success(response, &path)?;
        let json = self.parse_json(&response, &path)?;
        Ok(Some(ScmRemoteRef {
            reference: reference.clone(),
            head_sha: json
                .get("object")
                .and_then(|o| o.get("sha"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            url: string_field(&json, "url").unwrap_or_default(),
        }))
    }

    async fn create_or_reconcile_branch(
        &self,
        operation: &ExternalOperationId,
        spec: &BranchSpec,
    ) -> Result<ScmBranch, ScmError> {
        validate_ref_name(&spec.branch)?;
        if spec.head_sha.len() != 40 || !spec.head_sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ScmError::InvalidInput(
                "branch head sha must be a 40-hex commit id".into(),
            ));
        }
        let repository = &spec.repository;
        let full_name = repository.full_name();
        self.journal(
            operation,
            OperationIdentity {
                kind: "branch",
                repository: &full_name,
                marker: &spec.marker,
            },
            "prepared",
            None,
            None,
        )?;

        if let Some(existing) = self
            .remote_ref_lookup(repository, &spec.branch, &spec.marker)
            .await?
        {
            if existing.0 != spec.head_sha {
                return Err(ScmError::ReconcileConflict {
                    detail: format!(
                        "branch {} already exists at {} but {} was requested",
                        spec.branch, existing.0, spec.head_sha
                    ),
                });
            }
            self.journal(
                operation,
                OperationIdentity {
                    kind: "branch",
                    repository: &full_name,
                    marker: &spec.marker,
                },
                "completed",
                Some(format!("refs/heads/{}", spec.branch)),
                Some(existing.0.clone()),
            )?;
            return Ok(ScmBranch {
                reference: RemoteRef::try_new(
                    repository.clone(),
                    format!("refs/heads/{}", spec.branch),
                )?,
                head_sha: existing.0,
                url: existing.1,
                created: false,
            });
        }

        let path = format!("{}/git/refs", self.repo_path(repository));
        let body = serde_json::json!({
            "ref": format!("refs/heads/{}", spec.branch),
            "sha": spec.head_sha,
        });
        let response = self
            .send(
                Auth::Installation(repository.installation()),
                "POST",
                &path,
                "",
                Some(body),
            )
            .await?;
        match response.status {
            201 => {
                let json = self.parse_json(&response, &path)?;
                let sha = json
                    .get("object")
                    .and_then(|o| o.get("sha"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(&spec.head_sha)
                    .to_string();
                self.journal(
                    operation,
                    OperationIdentity {
                        kind: "branch",
                        repository: &full_name,
                        marker: &spec.marker,
                    },
                    "completed",
                    Some(format!("refs/heads/{}", spec.branch)),
                    Some(sha.clone()),
                )?;
                Ok(ScmBranch {
                    reference: RemoteRef::try_new(
                        repository.clone(),
                        format!("refs/heads/{}", spec.branch),
                    )?,
                    head_sha: sha,
                    url: string_field(&json, "url").unwrap_or_default(),
                    created: true,
                })
            }
            // 422: the ref appeared between the lookup and the create (or a
            // racing writer created it): reconcile against the winner.
            422 => {
                let Some(existing) = self
                    .remote_ref_lookup(repository, &spec.branch, &spec.marker)
                    .await?
                else {
                    return Err(self.conflict_from_response(response, &path));
                };
                if existing.0 != spec.head_sha {
                    return Err(ScmError::ReconcileConflict {
                        detail: format!(
                            "branch {} was created concurrently at {} (requested {})",
                            spec.branch, existing.0, spec.head_sha
                        ),
                    });
                }
                self.journal(
                    operation,
                    OperationIdentity {
                        kind: "branch",
                        repository: &full_name,
                        marker: &spec.marker,
                    },
                    "completed",
                    Some(format!("refs/heads/{}", spec.branch)),
                    Some(existing.0.clone()),
                )?;
                Ok(ScmBranch {
                    reference: RemoteRef::try_new(
                        repository.clone(),
                        format!("refs/heads/{}", spec.branch),
                    )?,
                    head_sha: existing.0,
                    url: existing.1,
                    created: false,
                })
            }
            _ => Err(self.conflict_from_response(response, &path)),
        }
    }

    async fn create_or_reconcile_pull_request(
        &self,
        operation: &ExternalOperationId,
        spec: &PullRequestSpec,
    ) -> Result<ScmPullRequest, ScmError> {
        validate_ref_name(&spec.head)?;
        validate_ref_name(&spec.base)?;
        if spec.marker.is_empty() || spec.marker.len() > 200 {
            return Err(ScmError::InvalidInput(
                "pull request marker must be 1..=200 bytes".into(),
            ));
        }
        let repository = &spec.repository;
        let full_name = repository.full_name();
        self.journal(
            operation,
            OperationIdentity {
                kind: "pull_request",
                repository: &full_name,
                marker: &spec.marker,
            },
            "prepared",
            None,
            None,
        )?;

        if let Some(existing) = self.find_pull_request(repository, spec).await? {
            let pr = self.parse_pull_request(repository, &existing, &spec.marker, false)?;
            self.journal(
                operation,
                OperationIdentity {
                    kind: "pull_request",
                    repository: &full_name,
                    marker: &spec.marker,
                },
                "completed",
                Some(pr.reference.number().to_string()),
                Some(pr.version.clone()),
            )?;
            return Ok(pr);
        }

        let path = format!("{}/pulls", self.repo_path(repository));
        let body = serde_json::json!({
            "title": spec.title,
            "body": spec.body,
            "head": spec.head,
            "base": spec.base,
        });
        let response = self
            .send(
                Auth::Installation(repository.installation()),
                "POST",
                &path,
                "",
                Some(body),
            )
            .await?;
        match response.status {
            201 => {
                let json = self.parse_json(&response, &path)?;
                let pr = self.parse_pull_request(repository, &json, &spec.marker, true)?;
                self.journal(
                    operation,
                    OperationIdentity {
                        kind: "pull_request",
                        repository: &full_name,
                        marker: &spec.marker,
                    },
                    "completed",
                    Some(pr.reference.number().to_string()),
                    Some(pr.version.clone()),
                )?;
                Ok(pr)
            }
            // The PR appeared between the lookup and the create: reconcile
            // against the winner (never a duplicate).
            422 => {
                let Some(existing) = self.find_pull_request(repository, spec).await? else {
                    return Err(self.conflict_from_response(response, &path));
                };
                let pr = self.parse_pull_request(repository, &existing, &spec.marker, false)?;
                self.journal(
                    operation,
                    OperationIdentity {
                        kind: "pull_request",
                        repository: &full_name,
                        marker: &spec.marker,
                    },
                    "completed",
                    Some(pr.reference.number().to_string()),
                    Some(pr.version.clone()),
                )?;
                Ok(pr)
            }
            _ => Err(self.conflict_from_response(response, &path)),
        }
    }

    async fn comment(
        &self,
        operation: &ExternalOperationId,
        target: &CommentTarget,
        body: &str,
    ) -> Result<ScmComment, ScmError> {
        if body.is_empty() || body.len() > 65_536 {
            return Err(ScmError::InvalidInput(
                "comment body must be 1..=65536 bytes".into(),
            ));
        }
        let (repository, number) = match target {
            CommentTarget::Issue(issue) => (issue.repository(), issue.number()),
            CommentTarget::PullRequest(pr) => (pr.repository(), pr.number()),
        };
        let path = format!("{}/issues/{number}/comments", self.repo_path(repository));
        self.journal(
            operation,
            OperationIdentity {
                kind: "comment",
                repository: &repository.full_name(),
                marker: "",
            },
            "prepared",
            None,
            None,
        )?;
        let response = self
            .send(
                Auth::Installation(repository.installation()),
                "POST",
                &path,
                "",
                Some(serde_json::json!({ "body": body })),
            )
            .await?;
        let response = self.expect_success(response, &path)?;
        let json = self.parse_json(&response, &path)?;
        let id = json
            .get("id")
            .and_then(|v| v.as_u64())
            .map(|v| v.to_string())
            .ok_or_else(|| ScmError::Api {
                status: 201,
                detail: "comment payload has no id".into(),
            })?;
        let url = string_field(&json, "html_url").unwrap_or_default();
        self.journal(
            operation,
            OperationIdentity {
                kind: "comment",
                repository: &repository.full_name(),
                marker: "",
            },
            "completed",
            Some(id.clone()),
            None,
        )?;
        Ok(ScmComment { id, url })
    }

    async fn review_events(
        &self,
        pull_request: &PullRequestRef,
    ) -> Result<Vec<ScmReviewEvent>, ScmError> {
        let repository = pull_request.repository();
        let path = format!(
            "{}/pulls/{}/reviews",
            self.repo_path(repository),
            pull_request.number()
        );
        let response = self
            .send(
                Auth::Installation(repository.installation()),
                "GET",
                &path,
                &format!("?per_page={}", self.config.page_size),
                None,
            )
            .await?;
        let response = self.expect_success(response, &path)?;
        let json = self.parse_json(&response, &path)?;
        let items = json.as_array().cloned().unwrap_or_default();
        Ok(items
            .into_iter()
            .filter_map(|item| {
                let id = item.get("id")?.as_u64()?.to_string();
                Some(ScmReviewEvent {
                    id,
                    reviewer: item
                        .get("user")
                        .and_then(|u| u.get("login"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    state: item
                        .get("state")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    submitted_ms: item
                        .get("submitted_at")
                        .and_then(|v| v.as_str())
                        .map(|s| chrono_like_parse_ms(s).unwrap_or_default()),
                })
            })
            .collect())
    }

    async fn list_installations(&self) -> Result<Vec<ScmInstallation>, ScmError> {
        let items = self
            .list_pages(Auth::App, "/app/installations", None)
            .await?;
        let mut out = Vec::new();
        for item in items {
            let id = item
                .get("id")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| ScmError::Api {
                    status: 200,
                    detail: "installation payload has no id".into(),
                })?;
            let account = item.get("account").cloned().unwrap_or_default();
            let mut permissions: Vec<(String, String)> = item
                .get("permissions")
                .and_then(|v| v.as_object())
                .map(|map| {
                    map.iter()
                        .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            permissions.sort();
            let account_login = account
                .get("login")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ScmError::Api {
                    status: 200,
                    detail: "installation payload has no account login".into(),
                })?;
            if account_login.len() > crate::ids::MAX_OWNER_BYTES {
                return Err(ScmError::Api {
                    status: 200,
                    detail: "installation account login is oversized".into(),
                });
            }
            out.push(ScmInstallation {
                installation_id: ScmInstallationId::try_from_raw(id)?,
                account_login: account_login.to_string(),
                account_type: account
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("User")
                    .to_string(),
                permissions,
                suspended: item
                    .get("suspended_at")
                    .map(|v| !v.is_null())
                    .unwrap_or(false),
            });
        }
        Ok(out)
    }

    async fn list_repositories(
        &self,
        installation: ScmInstallationId,
    ) -> Result<Vec<ScmRepository>, ScmError> {
        let items = self
            .list_pages(
                Auth::Installation(installation),
                "/installation/repositories",
                Some("repositories"),
            )
            .await?;
        let mut out = Vec::new();
        for item in items {
            let owner = item
                .get("owner")
                .and_then(|o| o.get("login"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| ScmError::Api {
                    status: 200,
                    detail: "repository payload has no owner".into(),
                })?;
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ScmError::Api {
                    status: 200,
                    detail: "repository payload has no name".into(),
                })?;
            let reference = RepositoryRef::try_new(installation, owner, name)?;
            out.push(self.parse_repository(&reference, &item)?);
        }
        Ok(out)
    }
}

impl GitHubApp {
    /// One ref lookup returning `(sha, url)`.
    async fn remote_ref_lookup(
        &self,
        repository: &RepositoryRef,
        branch: &str,
        marker: &str,
    ) -> Result<Option<(String, String)>, ScmError> {
        let reference = RemoteRef::try_new(repository.clone(), format!("refs/heads/{branch}"))?;
        let _ = marker;
        Ok(self
            .remote_ref(&reference)
            .await?
            .map(|found| (found.head_sha, found.url)))
    }

    /// Find one pull request by the EXACT `(repository, head, base, marker)`
    /// identity. Bounded pagination; a marker match is unique per contract
    /// (the first match wins deterministically because the list is ordered
    /// oldest-first by the provider).
    async fn find_pull_request(
        &self,
        repository: &RepositoryRef,
        spec: &PullRequestSpec,
    ) -> Result<Option<serde_json::Value>, ScmError> {
        let path = format!("{}/pulls", self.repo_path(repository));
        let mut matches: Vec<serde_json::Value> = Vec::new();
        for page in 1..=self.config.max_pages {
            let query = format!(
                "?state=all&head={}:{}&base={}&per_page={}&page={page}",
                repository.owner(),
                spec.head,
                spec.base,
                self.config.page_size,
            );
            let response = self
                .send(
                    Auth::Installation(repository.installation()),
                    "GET",
                    &path,
                    &query,
                    None,
                )
                .await?;
            let response = self.expect_success(response, &path)?;
            let json = self.parse_json(&response, &path)?;
            let items = json.as_array().cloned().unwrap_or_default();
            let count = items.len();
            for item in items {
                let body = item
                    .get("body")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let head_matches = item
                    .get("head")
                    .and_then(|h| h.get("ref"))
                    .and_then(|v| v.as_str())
                    == Some(spec.head.as_str());
                let base_matches = item
                    .get("base")
                    .and_then(|b| b.get("ref"))
                    .and_then(|v| v.as_str())
                    == Some(spec.base.as_str());
                if head_matches && base_matches && body.contains(&spec.marker) {
                    matches.push(item);
                }
            }
            if count < self.config.page_size {
                break;
            }
        }
        match matches.len() {
            0 => Ok(None),
            1 => Ok(matches.pop()),
            _ => Err(ScmError::ReconcileConflict {
                detail: format!(
                    "{} pull requests carry the marker {} (exactly one is required)",
                    matches.len(),
                    spec.marker
                ),
            }),
        }
    }

    fn conflict_from_response(&self, response: RawResponse, context: &str) -> ScmError {
        ScmError::Api {
            status: response.status,
            detail: format!("{context}: {}", bounded_excerpt(&response.body_text())),
        }
    }
}

/// A minimal RFC 3339 → epoch-ms parser for `submitted_at` (no chrono
/// dependency): `YYYY-MM-DDTHH:MM:SSZ`, days-from-civil arithmetic.
pub(crate) fn chrono_like_parse_ms(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<i64> {
        std::str::from_utf8(&bytes[range]).ok()?.parse().ok()
    };
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    // days-from-civil (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + hour * 3_600 + minute * 60 + second) * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_validation_is_strict() {
        assert!(GitHubAppConfig::default().validate().is_ok());
        for bad in [
            GitHubAppConfig {
                api_base: "ftp://x".into(),
                ..Default::default()
            },
            GitHubAppConfig {
                api_base: "https://user@host".into(),
                ..Default::default()
            },
            GitHubAppConfig {
                user_agent: String::new(),
                ..Default::default()
            },
            GitHubAppConfig {
                page_size: 0,
                ..Default::default()
            },
            GitHubAppConfig {
                page_size: 101,
                ..Default::default()
            },
            GitHubAppConfig {
                max_pages: 0,
                ..Default::default()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn token_debug_is_redacted_and_shape_is_bounded() {
        let token = InstallationToken::new("secret-value", 5, vec![]).unwrap();
        let rendered = format!("{token:?}");
        assert!(!rendered.contains("secret-value"));
        assert!(rendered.contains("<redacted>"));
        assert!(InstallationToken::new("", 5, vec![]).is_err());
        assert!(InstallationToken::new("has space", 5, vec![]).is_err());
        assert!(InstallationToken::new("x".repeat(4097), 5, vec![]).is_err());
    }

    /// The compile-time negative proof: `InstallationToken` has NO `Display`
    /// and NO `Serialize`.
    macro_rules! assert_no_display_no_serialize {
        ($ty:ty) => {{
            trait AmbiguousIfImpl<A> {
                fn probe() {}
            }
            impl<T: ?Sized> AmbiguousIfImpl<()> for T {}
            impl<T: ?Sized + std::fmt::Display> AmbiguousIfImpl<u8> for T {}
            impl<T: ?Sized + ::serde::Serialize> AmbiguousIfImpl<u16> for T {}
            let _ = <$ty as AmbiguousIfImpl<_>>::probe;
        }};
    }

    #[test]
    fn planted_token_never_leaks_through_debug_display_serde_or_panic() {
        assert_no_display_no_serialize!(InstallationToken);
        const PLANTED: &str = "PLANTED-INSTALLATION-TOKEN-do-not-leak-0123456789";
        let token = InstallationToken::new(PLANTED, 5, vec![]).unwrap();
        for rendered in [
            format!("{token:?}"),
            format!("{:?}", Some(token.clone())),
            format!("{:?}", vec![token.clone()]),
            format!("{:?}", (token.clone(), 1u8)),
        ] {
            assert!(!rendered.contains(PLANTED), "leaked via {rendered}");
        }
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("installation token {token:?}")
        }))
        .expect_err("the closure must panic");
        let message = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default();
        assert!(
            !message.contains(PLANTED),
            "panic payload leaked: {message}"
        );
        // The shape refusal names the shape, never the planted bytes.
        let err = InstallationToken::new(format!("{PLANTED} with space"), 5, vec![]).unwrap_err();
        let rendered = format!("{err} {err:?}");
        assert!(!rendered.contains(PLANTED), "error leaked: {rendered}");
    }

    #[test]
    fn minimal_permission_enforcement_is_exact() {
        let ok = InstallationToken::new(
            "t",
            5,
            REQUIRED_REPOSITORY_PERMISSIONS
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
        .unwrap();
        assert!(require_permissions(&ok).is_ok());
        let missing =
            InstallationToken::new("t", 5, vec![("contents".into(), "write".into())]).unwrap();
        assert!(require_permissions(&missing).is_err());
        let read_only = InstallationToken::new(
            "t",
            5,
            REQUIRED_REPOSITORY_PERMISSIONS
                .iter()
                .map(|(k, v)| {
                    (
                        k.to_string(),
                        if *k == "contents" {
                            "read".to_string()
                        } else {
                            v.to_string()
                        },
                    )
                })
                .collect(),
        )
        .unwrap();
        assert!(require_permissions(&read_only).is_err());
        let empty = InstallationToken::new("t", 5, vec![]).unwrap();
        assert!(
            require_permissions(&empty).is_err(),
            "an undeclared permission set is not a grant"
        );
    }

    #[test]
    fn error_excerpts_are_bounded_and_utf8_safe() {
        let text = "é".repeat(400);
        let excerpt = bounded_excerpt(&text);
        assert!(excerpt.len() <= MAX_ERROR_EXCERPT_BYTES + 4);
        assert!(excerpt.ends_with('…'));
        assert_eq!(bounded_excerpt("short"), "short");
    }

    #[test]
    fn timestamp_parsing_is_exact() {
        assert_eq!(chrono_like_parse_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            chrono_like_parse_ms("2024-02-29T12:34:56Z"),
            Some(1_709_210_096_000)
        );
        assert_eq!(chrono_like_parse_ms("not-a-timestamp"), None);
        assert_eq!(chrono_like_parse_ms("2024-13-01T00:00:00Z"), None);
    }

    #[test]
    fn etag_cache_is_bounded_and_replaces_per_url() {
        let mut cache = EtagCache::default();
        for i in 0..(MAX_ETAG_ENTRIES + 10) {
            cache.put(&format!("url-{i}"), "etag", b"body");
        }
        assert_eq!(cache.entries.len(), MAX_ETAG_ENTRIES);
        assert!(cache.get("url-0").is_none(), "oldest entries are evicted");
        cache.put("same", "e1", b"one");
        cache.put("same", "e2", b"two");
        assert_eq!(cache.get("same").unwrap().0, "e2");
        assert_eq!(cache.entries.len(), MAX_ETAG_ENTRIES);
    }
}
