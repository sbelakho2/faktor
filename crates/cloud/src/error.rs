//! Typed control-plane failures. Stable machine codes; no variant carries a
//! secret, and `NotFound` for a foreign tenant is indistinguishable from
//! `NotFound` for a nonexistent row (no existence leak).

use crate::store::CloudStoreError;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ControlPlaneError {
    #[error("control-plane configuration error: {0}")]
    Config(String),
    #[error("control-plane input refused: {0}")]
    Malformed(String),
    #[error("control-plane object not found: {0}")]
    NotFound(String),
    #[error("control-plane authentication refused: {0}")]
    Unauthorized(String),
    #[error("control-plane permission refused: {0}")]
    Forbidden(String),
    #[error("control-plane conflict: {0}")]
    Conflict(String),
    #[error("control-plane store unavailable: {0}")]
    Backend(String),
}

impl ControlPlaneError {
    pub const fn code(&self) -> &'static str {
        match self {
            ControlPlaneError::Config(_) => "control_plane_config",
            ControlPlaneError::Malformed(_) => "malformed",
            ControlPlaneError::NotFound(_) => "not_found",
            ControlPlaneError::Unauthorized(_) => "unauthorized",
            ControlPlaneError::Forbidden(_) => "permission_denied",
            ControlPlaneError::Conflict(_) => "conflict",
            ControlPlaneError::Backend(_) => "internal",
        }
    }

    pub const fn http_status(&self) -> u16 {
        match self {
            ControlPlaneError::Config(_) | ControlPlaneError::Backend(_) => 500,
            ControlPlaneError::Malformed(_) => 400,
            ControlPlaneError::NotFound(_) => 404,
            ControlPlaneError::Unauthorized(_) => 401,
            ControlPlaneError::Forbidden(_) => 403,
            ControlPlaneError::Conflict(_) => 409,
        }
    }

    pub const fn retryable(&self) -> bool {
        matches!(self, ControlPlaneError::Backend(_))
    }
}

impl From<crate::rbac::Denied> for ControlPlaneError {
    fn from(denied: crate::rbac::Denied) -> Self {
        match denied {
            crate::rbac::Denied::NotFound { message } => ControlPlaneError::NotFound(message),
            crate::rbac::Denied::Forbidden { message } => ControlPlaneError::Forbidden(message),
            crate::rbac::Denied::Malformed { message } => ControlPlaneError::Malformed(message),
        }
    }
}

impl From<CloudStoreError> for ControlPlaneError {
    fn from(e: CloudStoreError) -> Self {
        match e {
            CloudStoreError::Conflict(message) => ControlPlaneError::Conflict(message),
            other => ControlPlaneError::Backend(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_and_code_mapping_is_stable() {
        assert_eq!(ControlPlaneError::NotFound("x".into()).http_status(), 404);
        assert_eq!(ControlPlaneError::NotFound("x".into()).code(), "not_found");
        assert_eq!(ControlPlaneError::Forbidden("x".into()).http_status(), 403);
        assert_eq!(
            ControlPlaneError::Unauthorized("x".into()).http_status(),
            401
        );
        assert_eq!(ControlPlaneError::Conflict("x".into()).http_status(), 409);
        assert_eq!(ControlPlaneError::Malformed("x".into()).http_status(), 400);
        assert!(ControlPlaneError::Backend("x".into()).retryable());
        assert!(!ControlPlaneError::Conflict("x".into()).retryable());
    }
}
