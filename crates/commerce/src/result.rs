//! Result safety and the compact job-result contract.
//!
//! Two invariants of `docs/acquire.md` are enforced here rather than left to
//! review:
//!
//! - **No forbidden material ever leaves the acquisition runtime.** Raw HTML,
//!   cookies, auth headers, session tokens, CSRF tokens, API secrets and
//!   refresh tokens are refused by a shared scanner ([`scan_forbidden`]) that
//!   both the store (before persisting a payload) and the job artifact
//!   assembler (before handing bytes to CAS) run.
//! - **Large results go to a CAS artifact with a compact context result.**
//!   The compact result targets 4–12 KiB and has a hard maximum of 64 KiB
//!   ([`COMPACT_RESULT_HARD_MAX_BYTES`]); a result that would exceed the hard
//!   bound is trimmed deterministically (most important entries first) and
//!   reports `truncated: true`, never silently.

use serde::{Deserialize, Serialize};

use crate::error::SourceError;
use crate::text::Text;

/// Lower end of the compact-result target band.
pub const COMPACT_RESULT_TARGET_MIN_BYTES: usize = 4 * 1024;
/// Upper end of the compact-result target band.
pub const COMPACT_RESULT_TARGET_MAX_BYTES: usize = 12 * 1024;
/// The hard maximum of the serialized compact result.
pub const COMPACT_RESULT_HARD_MAX_BYTES: usize = 64 * 1024;
/// At most this many `important` entries are kept.
pub const MAX_IMPORTANT_ENTRIES: usize = 24;
/// Byte bound of an `important` entry key.
pub const MAX_IMPORTANT_KEY_BYTES: usize = 64;
/// Byte bound of an `important` entry value.
pub const MAX_IMPORTANT_VALUE_BYTES: usize = 512;
/// Hard bound of one CAS artifact.
pub const MAX_ARTIFACT_BYTES: usize = 8 * 1024 * 1024;

/// Raw markup that must never be persisted or handed to a model. The list is
/// ordered so the earliest byte of a hostile document is reported (a
/// `<!doctype html>` prologue is refused at byte 0, not at the later
/// `<html>` tag).
pub const MARKUP_MARKERS: &[&str] = &[
    "<!doctype",
    "<html",
    "<script",
    "</script",
    "<iframe",
    "<form",
    "javascript:",
];

/// Credential/secret material that must never be persisted or handed to a
/// model. These markers are matched anywhere (case-insensitively).
pub const SECRET_MARKERS: &[&str] = &[
    "refresh_token",
    "access_token",
    "client_secret",
    "api_key",
    "apikey",
    "x-api-key",
    "x-csrf-token",
    "csrf_token",
    "bearer ",
    "password=",
    "sessionid=",
];

/// HTTP header material. These markers only match at the start of a line
/// (after optional ASCII whitespace), so a normalized product description
/// that merely mentions the word "cookie" mid-sentence is not refused while a
/// pasted header block always is.
pub const HEADER_MARKERS: &[&str] = &[
    "set-cookie:",
    "cookie:",
    "authorization:",
    "proxy-authorization:",
    "x-auth-token:",
];

/// One refused piece of forbidden material.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("forbidden material {kind:?} at byte {at}")]
pub struct ForbiddenMaterial {
    /// The marker family that matched (`markup`, `secret`, `header`).
    pub kind: &'static str,
    /// The byte offset of the match.
    pub at: usize,
}

fn match_marker(haystack: &str, marker: &str, line_start_only: bool) -> Option<usize> {
    let mut from = 0usize;
    while let Some(offset) = haystack[from..].find(marker) {
        let at = from + offset;
        if !line_start_only {
            return Some(at);
        }
        let before = &haystack[..at];
        let line_start = match before.rfind('\n') {
            Some(newline) => before[newline + 1..].trim().is_empty(),
            None => before.trim().is_empty(),
        };
        if line_start {
            return Some(at);
        }
        from = at + marker.len();
    }
    None
}

/// Scan one string for forbidden material. Case-insensitive on ASCII.
pub fn scan_forbidden(value: &str) -> Result<(), ForbiddenMaterial> {
    let lowered = value.to_ascii_lowercase();
    for marker in MARKUP_MARKERS {
        if let Some(at) = match_marker(&lowered, marker, false) {
            return Err(ForbiddenMaterial { kind: "markup", at });
        }
    }
    for marker in SECRET_MARKERS {
        if let Some(at) = match_marker(&lowered, marker, false) {
            return Err(ForbiddenMaterial { kind: "secret", at });
        }
    }
    for marker in HEADER_MARKERS {
        if let Some(at) = match_marker(&lowered, marker, true) {
            return Err(ForbiddenMaterial { kind: "header", at });
        }
    }
    Ok(())
}

/// Scan raw bytes for forbidden material (UTF-8 lossy, ASCII markers).
pub fn scan_forbidden_bytes(bytes: &[u8]) -> Result<(), ForbiddenMaterial> {
    scan_forbidden(&String::from_utf8_lossy(bytes))
}

/// Validate a BLAKE3 digest in bare or `blake3:`-prefixed 64-hex form.
pub fn validate_digest(value: &str) -> Result<(), SourceError> {
    let bare = value.strip_prefix("blake3:").unwrap_or(value);
    if bare.len() != 64 || !bare.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(SourceError::InvalidRequest);
    }
    Ok(())
}

/// The BLAKE3 digest of one artifact body, as bare lowercase hex.
pub fn artifact_digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// A reference to a CAS artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    /// The content digest (bare 64-hex or `blake3:`-prefixed).
    pub digest: String,
    /// The artifact size in bytes.
    pub bytes: u64,
}

impl ArtifactRef {
    /// Validate the digest shape and bound.
    pub fn validate(&self) -> Result<(), SourceError> {
        validate_digest(&self.digest)?;
        if self.bytes > MAX_ARTIFACT_BYTES as u64 {
            return Err(SourceError::ResponseTooLarge);
        }
        Ok(())
    }
}

/// The injected CAS seam. The real daemon wires this to `faktor-cas`; tests
/// use a fake. The commerce runtime never writes artifacts itself.
#[async_trait::async_trait]
pub trait ArtifactStore: Send + Sync {
    /// Persist one artifact and return its reference. The implementation is
    /// responsible for content addressing; callers pass the exact bytes.
    async fn put(&self, bytes: &[u8]) -> Result<ArtifactRef, SourceError>;
    /// Fetch one artifact by digest.
    async fn get(&self, digest: &str) -> Result<Option<Vec<u8>>, SourceError>;
}

/// Matched / ambiguous / unmatched accounting shared by every operation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultCounts {
    /// Lines with an exact/strong match.
    pub matched: u64,
    /// Lines with an ambiguous match (never auto-selected).
    pub ambiguous: u64,
    /// Lines with no match.
    pub unmatched: u64,
}

impl ResultCounts {
    /// Accumulate another line's outcome.
    pub fn add(&mut self, other: ResultCounts) {
        self.matched = self.matched.saturating_add(other.matched);
        self.ambiguous = self.ambiguous.saturating_add(other.ambiguous);
        self.unmatched = self.unmatched.saturating_add(other.unmatched);
    }

    /// The total line count.
    pub const fn total(&self) -> u64 {
        self.matched
            .saturating_add(self.ambiguous)
            .saturating_add(self.unmatched)
    }
}

/// The status carried by a compact result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactStatus {
    /// The job is still running.
    Running,
    /// The job finished.
    Completed,
    /// The job failed.
    Failed,
    /// The job was cancelled.
    Cancelled,
}

impl CompactStatus {
    /// The stable wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// One bounded `important` entry of a compact result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportantEntry {
    /// The entry key (bounded, validated).
    pub key: Text<{ MAX_IMPORTANT_KEY_BYTES }>,
    /// The entry value (bounded, validated, scanned).
    pub value: Text<{ MAX_IMPORTANT_VALUE_BYTES }>,
}

impl ImportantEntry {
    /// Validate and construct; refuses forbidden material.
    pub fn new(key: &str, value: &str) -> Result<Self, SourceError> {
        let key = Text::<MAX_IMPORTANT_KEY_BYTES>::new(key).map_err(|_| SourceError::Store)?;
        let value =
            Text::<MAX_IMPORTANT_VALUE_BYTES>::new(value).map_err(|_| SourceError::Store)?;
        if scan_forbidden(key.as_str()).is_err() || scan_forbidden(value.as_str()).is_err() {
            return Err(SourceError::Store);
        }
        Ok(Self { key, value })
    }
}

/// The compact context result of a bulk job (`docs/acquire.md` §12).
///
/// Serialized size targets [`COMPACT_RESULT_TARGET_MIN_BYTES`] ..
/// [`COMPACT_RESULT_TARGET_MAX_BYTES`] and never exceeds
/// [`COMPACT_RESULT_HARD_MAX_BYTES`]; `truncated` reports that important
/// entries were dropped to hold the hard bound.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactResult {
    /// The job status.
    pub status: CompactStatus,
    /// The number of lines in the full artifact.
    pub lines: u64,
    /// Lines with an exact/strong match.
    pub matched: u64,
    /// Lines with an ambiguous match.
    pub ambiguous: u64,
    /// Lines with no match.
    pub unmatched: u64,
    /// The CAS artifact holding the full result, when one was written.
    pub artifact: Option<ArtifactRef>,
    /// The bounded important entries (unmatched/ambiguous lines first).
    pub important: Vec<ImportantEntry>,
    /// True when important entries were dropped to hold the hard bound.
    pub truncated: bool,
}

impl CompactResult {
    /// Build a compact result from counts, artifact and important entries.
    pub fn new(
        status: CompactStatus,
        counts: ResultCounts,
        artifact: Option<ArtifactRef>,
        important: Vec<ImportantEntry>,
    ) -> Result<Self, SourceError> {
        if let Some(artifact) = &artifact {
            artifact.validate()?;
        }
        if important.len() > MAX_IMPORTANT_ENTRIES {
            return Err(SourceError::ResponseTooLarge);
        }
        for entry in &important {
            if scan_forbidden(entry.key.as_str()).is_err()
                || scan_forbidden(entry.value.as_str()).is_err()
            {
                return Err(SourceError::Store);
            }
        }
        Ok(Self {
            status,
            lines: counts.total(),
            matched: counts.matched,
            ambiguous: counts.ambiguous,
            unmatched: counts.unmatched,
            artifact,
            important,
            truncated: false,
        })
    }

    /// Serialize the compact result under the target maximum. Trailing
    /// important entries are dropped (never reordered) until the bound
    /// holds; the returned value has `truncated` set truthfully. The result
    /// never exceeds the hard maximum, and when even an empty `important`
    /// list would exceed it the call fails typed instead of emitting an
    /// oversized context object.
    pub fn to_bounded_json(&self) -> Result<String, SourceError> {
        let mut candidate = self.clone();
        loop {
            let json = serde_json::to_string(&candidate).map_err(|_| SourceError::Store)?;
            if json.len() <= COMPACT_RESULT_TARGET_MAX_BYTES {
                return Ok(json);
            }
            if candidate.important.is_empty() {
                if json.len() <= COMPACT_RESULT_HARD_MAX_BYTES {
                    return Ok(json);
                }
                return Err(SourceError::ResponseTooLarge);
            }
            candidate.important.pop();
            candidate.truncated = true;
        }
    }

    /// True when the serialized result sits inside the 4–12 KiB target band.
    pub fn is_within_target(&self) -> bool {
        match serde_json::to_string(self) {
            Ok(json) => {
                json.len() >= COMPACT_RESULT_TARGET_MIN_BYTES
                    && json.len() <= COMPACT_RESULT_TARGET_MAX_BYTES
            }
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(matched: u64, ambiguous: u64, unmatched: u64) -> ResultCounts {
        ResultCounts {
            matched,
            ambiguous,
            unmatched,
        }
    }

    #[test]
    fn scanner_refuses_markup_secrets_and_headers_but_not_prose() {
        assert_eq!(
            scan_forbidden("<!DOCTYPE html><html><body>x</body>"),
            Err(ForbiddenMaterial {
                kind: "markup",
                at: 0
            })
        );
        assert!(scan_forbidden("Set-Cookie: session=abc; HttpOnly").is_err());
        assert!(scan_forbidden("  Authorization: Bearer eyJhbGciOi").is_err());
        assert!(scan_forbidden("{\"refresh_token\":\"rt_123\"}").is_err());
        assert!(scan_forbidden("{\"client_secret\":\"shh\"}").is_err());
        assert!(scan_forbidden("x-api-key: 12345").is_err());
        assert!(scan_forbidden("a description mentioning a cookie jar").is_ok());
        assert!(scan_forbidden("Mouser Electronics - Electronic Components").is_ok());
    }

    #[test]
    fn important_entries_reject_forbidden_material() {
        assert!(ImportantEntry::new("line-1", "Set-Cookie: a=b").is_err());
        assert!(ImportantEntry::new("line-1", "TPS5430DDAR matched at ¥18.20").is_ok());
        assert!(ImportantEntry::new("", "value").is_err());
        assert!(ImportantEntry::new("k", &"x".repeat(MAX_IMPORTANT_VALUE_BYTES + 1)).is_err());
    }

    #[test]
    fn compact_result_holds_the_hard_bound_by_trimming() {
        let important: Vec<ImportantEntry> = (0..MAX_IMPORTANT_ENTRIES)
            .map(|i| {
                ImportantEntry::new(
                    &format!("line-{i:03}"),
                    &"y".repeat(MAX_IMPORTANT_VALUE_BYTES),
                )
                .expect("bounded entry")
            })
            .collect();
        let result = CompactResult::new(
            CompactStatus::Completed,
            counts(499, 1, 0),
            Some(ArtifactRef {
                digest: artifact_digest(b"artifact"),
                bytes: 1024,
            }),
            important,
        )
        .expect("compact result");
        let json = result.to_bounded_json().expect("bounded");
        assert!(json.len() <= COMPACT_RESULT_HARD_MAX_BYTES);
        let parsed: CompactResult = serde_json::from_str(&json).expect("round trip");
        assert!(parsed.truncated, "trimming is reported");
        assert!(parsed.important.len() < MAX_IMPORTANT_ENTRIES);
        assert_eq!(parsed.matched, 499);
        assert_eq!(parsed.ambiguous, 1);
        assert_eq!(parsed.unmatched, 0);
    }

    #[test]
    fn a_normal_compact_result_sits_in_the_target_band() {
        let important: Vec<ImportantEntry> = (0..20)
            .map(|i| {
                ImportantEntry::new(
                    &format!("line-{i:04}"),
                    &format!(
                        "ambiguous: three candidates for PART-{i:04} {}",
                        "x".repeat(180)
                    ),
                )
                .expect("bounded entry")
            })
            .collect();
        let result = CompactResult::new(
            CompactStatus::Completed,
            counts(1_000, 20, 2),
            Some(ArtifactRef {
                digest: artifact_digest(b"artifact"),
                bytes: 4096,
            }),
            important,
        )
        .expect("compact result");
        assert!(
            result.is_within_target(),
            "{} bytes is outside the 4-12 KiB band",
            serde_json::to_string(&result).unwrap().len()
        );
        assert!(!result.truncated, "a band-sized result is not trimmed");
        assert_eq!(result.lines, 1_022);
    }

    #[test]
    fn digest_validation_is_strict() {
        assert!(validate_digest(&artifact_digest(b"x")).is_ok());
        assert!(validate_digest(&format!("blake3:{}", artifact_digest(b"x"))).is_ok());
        assert!(validate_digest("").is_err());
        assert!(validate_digest(&"z".repeat(64)).is_err());
        assert!(validate_digest(&"a".repeat(63)).is_err());
    }
}
