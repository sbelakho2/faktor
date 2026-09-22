//! Provenance of one acquisition: origin, observation time, content digest.
//!
//! Every acquired payload carries its provenance so the layer above can
//! reconcile observations without a model (`docs/acquire.md` §8). The digest
//! is BLAKE3 over the exact response bytes as received, so two observations
//! of identical content are provably identical and a `304 Not Modified`
//! keeps the original digest instead of inventing a new one.

use serde::{Deserialize, Serialize};

use crate::mechanism::AcquisitionMechanism;

/// Where one observation came from.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AcquisitionProvenance {
    /// The mechanism that produced it.
    pub origin: AcquisitionMechanism,
    /// When it was observed (Unix milliseconds).
    pub observed_at_ms: u64,
    /// BLAKE3 hex digest of the exact response bytes.
    pub content_digest: String,
    /// Whether the request was conditional (`If-None-Match` /
    /// `If-Modified-Since`).
    pub conditional: bool,
    /// The final URL after redirects, when the origin has one.
    pub final_url: Option<String>,
}

impl AcquisitionProvenance {
    /// Build provenance for `bytes`.
    pub fn observed(
        origin: AcquisitionMechanism,
        observed_at_ms: u64,
        bytes: &[u8],
        conditional: bool,
        final_url: Option<String>,
    ) -> Self {
        Self {
            origin,
            observed_at_ms,
            content_digest: content_digest(bytes),
            conditional,
            final_url,
        }
    }
}

/// The BLAKE3 hex digest of a byte slice.
pub fn content_digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_stable_and_content_addressed() {
        let a = content_digest(b"{\"a\":1}");
        let b = content_digest(b"{\"a\":1}");
        let c = content_digest(b"{\"a\":2}");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    #[test]
    fn provenance_records_origin_time_and_digest() {
        let provenance = AcquisitionProvenance::observed(
            AcquisitionMechanism::DirectHttp,
            42,
            b"body",
            true,
            Some("https://h/p".into()),
        );
        assert_eq!(provenance.origin, AcquisitionMechanism::DirectHttp);
        assert_eq!(provenance.observed_at_ms, 42);
        assert!(provenance.conditional);
        assert_eq!(provenance.content_digest, content_digest(b"body"));
    }
}
