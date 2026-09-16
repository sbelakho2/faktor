//! Typed updater failures. Every refusal is a machine code; a manifest that
//! cannot be authenticated is NEVER skipped (unsigned, unknown key, expired,
//! tampered and malformed are distinct codes), and a download that does not
//! match the signed digest is refused before anything is staged.

use crate::compat::CompatibilityReport;
use crate::store::UpdateStoreError;

/// A typed refusal of one update manifest. The distinction matters: an
/// operator must be able to tell "no signature" from "wrong key" from
/// "signature valid but expired" without reading log prose.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestRefusal {
    #[error("the manifest is malformed: {0}")]
    Malformed(String),
    #[error("the manifest is UNSIGNED (no signature field); refusing to trust it")]
    Unsigned,
    #[error("signature identity {identity:?} is not on the updater key allowlist")]
    UnknownKey { identity: String },
    #[error("signature identity {identity:?} does not match its allowlisted public key")]
    KeyMismatch { identity: String },
    #[error("the manifest signature does not verify (tampered or wrong key material)")]
    Tampered,
    #[error("the manifest expired at {expires_at} (now {now_ms})")]
    Expired { expires_at: i64, now_ms: i64 },
    #[error("the manifest is not valid yet (issued_at {issued_at}, now {now_ms})")]
    NotYetValid { issued_at: i64, now_ms: i64 },
    #[error(
        "the manifest channel {found:?} is not accepted by the configured {configured:?} channel"
    )]
    ChannelMismatch { configured: String, found: String },
    #[error("the manifest carries no artifact for this host ({os}/{arch})")]
    NoArtifactForHost { os: String, arch: String },
}

impl ManifestRefusal {
    /// The stable machine code surfaced over the native wire.
    pub const fn code(&self) -> &'static str {
        match self {
            ManifestRefusal::Malformed(_) => "manifest_malformed",
            ManifestRefusal::Unsigned => "manifest_unsigned",
            ManifestRefusal::UnknownKey { .. } => "manifest_unknown_key",
            ManifestRefusal::KeyMismatch { .. } => "manifest_key_mismatch",
            ManifestRefusal::Tampered => "manifest_tampered",
            ManifestRefusal::Expired { .. } => "manifest_expired",
            ManifestRefusal::NotYetValid { .. } => "manifest_not_yet_valid",
            ManifestRefusal::ChannelMismatch { .. } => "manifest_channel_mismatch",
            ManifestRefusal::NoArtifactForHost { .. } => "manifest_no_artifact_for_host",
        }
    }
}

/// Every failure of the update lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpdateError {
    #[error("updater configuration error: {0}")]
    Config(String),
    #[error("update manifest refused: {0}")]
    Refused(#[from] ManifestRefusal),
    #[error("the running components are incompatible with this update: {0}")]
    Incompatible(CompatibilityReport),
    #[error("artifact {artifact:?} download failed: {detail}")]
    Transport { artifact: String, detail: String },
    #[error("artifact {artifact:?} exceeded the configured bound of {max_bytes} bytes")]
    ArtifactTooLarge { artifact: String, max_bytes: u64 },
    #[error("artifact {artifact:?} digest mismatch: expected {expected}, computed {actual}")]
    DigestMismatch {
        artifact: String,
        expected: String,
        actual: String,
    },
    #[error("no update operation is staged; stage one before apply")]
    NothingStaged,
    #[error("the update operation {id} is in state {state:?} and cannot {action}")]
    IllegalTransition {
        id: String,
        state: String,
        action: String,
    },
    #[error("the post-swap health probe failed: {detail}")]
    HealthFailed { detail: String },
    #[error("the staged artifact {artifact:?} is missing or corrupt: {detail}")]
    StagedArtifactUnusable { artifact: String, detail: String },
    #[error("install layout error: {0}")]
    Install(String),
    #[error("update object not found: {0}")]
    NotFound(String),
    #[error("update operation refused: {0}")]
    Conflict(String),
    #[error("the updater store is unavailable: {0}")]
    Backend(String),
}

impl UpdateError {
    /// The stable machine code surfaced over the native wire.
    pub const fn code(&self) -> &'static str {
        match self {
            UpdateError::Config(_) => "updater_config",
            UpdateError::Refused(refusal) => refusal.code(),
            UpdateError::Incompatible(_) => "incompatible",
            UpdateError::Transport { .. } => "transport",
            UpdateError::ArtifactTooLarge { .. } => "artifact_too_large",
            UpdateError::DigestMismatch { .. } => "digest_mismatch",
            UpdateError::NothingStaged => "nothing_staged",
            UpdateError::IllegalTransition { .. } => "illegal_transition",
            UpdateError::HealthFailed { .. } => "health_failed",
            UpdateError::StagedArtifactUnusable { .. } => "staged_artifact_unusable",
            UpdateError::Install(_) => "install_error",
            UpdateError::NotFound(_) => "not_found",
            UpdateError::Conflict(_) => "conflict",
            UpdateError::Backend(_) => "internal",
        }
    }

    pub const fn http_status(&self) -> u16 {
        match self {
            UpdateError::Config(_) | UpdateError::Backend(_) => 500,
            UpdateError::Refused(ManifestRefusal::Malformed(_)) => 400,
            UpdateError::Refused(_) => 409,
            UpdateError::Incompatible(_) => 409,
            UpdateError::Transport { .. } => 502,
            UpdateError::ArtifactTooLarge { .. } => 413,
            UpdateError::DigestMismatch { .. } => 409,
            UpdateError::NothingStaged => 409,
            UpdateError::IllegalTransition { .. } => 409,
            UpdateError::HealthFailed { .. } => 500,
            UpdateError::StagedArtifactUnusable { .. } => 409,
            UpdateError::Install(_) => 500,
            UpdateError::NotFound(_) => 404,
            UpdateError::Conflict(_) => 409,
        }
    }

    pub const fn retryable(&self) -> bool {
        matches!(
            self,
            UpdateError::Transport { .. } | UpdateError::Backend(_)
        )
    }
}

impl From<UpdateStoreError> for UpdateError {
    fn from(e: UpdateStoreError) -> Self {
        UpdateError::Backend(e.to_string())
    }
}

impl From<faktor_core::Error> for UpdateError {
    fn from(e: faktor_core::Error) -> Self {
        UpdateError::Install(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusal_codes_are_distinct_and_stable() {
        let codes = [
            ManifestRefusal::Malformed("x".into()).code(),
            ManifestRefusal::Unsigned.code(),
            ManifestRefusal::UnknownKey {
                identity: "k".into(),
            }
            .code(),
            ManifestRefusal::KeyMismatch {
                identity: "k".into(),
            }
            .code(),
            ManifestRefusal::Tampered.code(),
            ManifestRefusal::Expired {
                expires_at: 0,
                now_ms: 1,
            }
            .code(),
            ManifestRefusal::NotYetValid {
                issued_at: 5,
                now_ms: 1,
            }
            .code(),
            ManifestRefusal::ChannelMismatch {
                configured: "stable".into(),
                found: "dev".into(),
            }
            .code(),
            ManifestRefusal::NoArtifactForHost {
                os: "darwin".into(),
                arch: "arm64".into(),
            }
            .code(),
        ];
        let mut seen = codes.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), codes.len(), "codes must be unique: {codes:?}");
        assert!(codes.iter().all(|c| c.starts_with("manifest_")));
    }

    #[test]
    fn status_mapping_is_honest() {
        assert_eq!(
            UpdateError::Refused(ManifestRefusal::Unsigned).http_status(),
            409
        );
        assert_eq!(
            UpdateError::Refused(ManifestRefusal::Malformed("m".into())).http_status(),
            400
        );
        assert_eq!(
            UpdateError::DigestMismatch {
                artifact: "a".into(),
                expected: "0".repeat(64),
                actual: "1".repeat(64),
            }
            .http_status(),
            409
        );
        assert!(UpdateError::Transport {
            artifact: "a".into(),
            detail: "d".into()
        }
        .retryable());
        assert!(!UpdateError::NothingStaged.retryable());
    }
}
