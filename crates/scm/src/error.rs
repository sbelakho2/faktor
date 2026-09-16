//! Typed SCM failures. Every variant carries a stable machine code so a
//! refused call is traceable without parsing prose, and no variant ever
//! carries a token, secret or unbounded remote body.

use crate::store::ScmStoreError;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScmError {
    #[error("scm configuration error: {0}")]
    Config(String),
    #[error("scm input refused: {0}")]
    InvalidInput(String),
    #[error("scm object not found: {0}")]
    NotFound(String),
    #[error("scm authentication refused: {0}")]
    Unauthorized(String),
    #[error("scm permission refused: {0}")]
    Forbidden(String),
    /// The provider rate limit is exhausted; `retry_after_ms` is the
    /// remaining backoff the caller must observe (no request was sent).
    #[error("scm provider rate limited; retry after {retry_after_ms} ms")]
    RateLimited { retry_after_ms: i64 },
    /// The remote object exists but does not match the recorded identity
    /// (different head sha, stale version, marker mismatch). Never repaired
    /// silently.
    #[error("scm reconciliation conflict: {detail}")]
    ReconcileConflict { detail: String },
    #[error("scm remote call failed: {0}")]
    Transport(String),
    #[error("scm provider refused ({status}): {detail}")]
    Api { status: u16, detail: String },
    #[error("scm store unavailable: {0}")]
    Store(String),
}

impl ScmError {
    /// The stable machine code (safe for durable details and logs).
    pub const fn code(&self) -> &'static str {
        match self {
            ScmError::Config(_) => "scm_config",
            ScmError::InvalidInput(_) => "scm_invalid_input",
            ScmError::NotFound(_) => "scm_not_found",
            ScmError::Unauthorized(_) => "scm_unauthorized",
            ScmError::Forbidden(_) => "scm_forbidden",
            ScmError::RateLimited { .. } => "scm_rate_limited",
            ScmError::ReconcileConflict { .. } => "scm_reconcile_conflict",
            ScmError::Transport(_) => "scm_transport",
            ScmError::Api { .. } => "scm_api",
            ScmError::Store(_) => "scm_store",
        }
    }

    /// Whether the caller may retry the same call unchanged.
    pub const fn retryable(&self) -> bool {
        matches!(
            self,
            ScmError::RateLimited { .. } | ScmError::Transport(_) | ScmError::Store(_)
        )
    }
}

impl From<ScmStoreError> for ScmError {
    fn from(e: ScmStoreError) -> Self {
        ScmError::Store(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_retryability_are_stable() {
        assert_eq!(ScmError::Config("x".into()).code(), "scm_config");
        assert_eq!(
            ScmError::RateLimited { retry_after_ms: 5 }.code(),
            "scm_rate_limited"
        );
        assert!(ScmError::RateLimited { retry_after_ms: 5 }.retryable());
        assert!(ScmError::Transport("io".into()).retryable());
        assert!(!ScmError::Forbidden("no".into()).retryable());
        assert!(!ScmError::ReconcileConflict { detail: "x".into() }.retryable());
        assert!(!ScmError::Api {
            status: 422,
            detail: "x".into()
        }
        .retryable());
    }
}
