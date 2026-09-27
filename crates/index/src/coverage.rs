//! Index coverage, batch continuation cursors and evidence freshness
//! (audits 5/6/16).
//!
//! # Why coverage is durable
//!
//! A repository scan can no longer be a one-shot bounded prefix walk: total
//! caps (`4k files / 16k dirs / 64 MiB`) silently produced a generation that
//! looked complete while a suffix of the tree was never visited — a symbol in
//! file 4,500 was reported "not found" forever. Instead a generation is built
//! in bounded PER-GENERATION BATCHES, each batch persists its continuation
//! cursor and [`IndexCoverage`] inside the generation envelope, and indexing
//! continues (asynchronously, through the reconciliation worker) until the
//! generation is complete.
//!
//! # Fingerprint coverage
//!
//! The stat-only fingerprint walk is capped too, but a capped fingerprint may
//! NEVER report the workspace "fully clean". Its coverage is persisted
//! ([`FingerprintCoverage`]) and subsequent scans rotate through path shards
//! so no suffix of the tree is perpetually ignored; while coverage is
//! incomplete a "no differences seen" result keeps the workspace pending and
//! schedules the next shard immediately.
//!
//! # Evidence freshness
//!
//! Every evidence package built from a generation carries its
//! [`EvidencePackageMeta`]: the generation number, the content/fingerprint
//! identity, the coverage, and a typed [`EvidenceFreshness`]
//! (`current` / `stale_while_rebuilding` / `partial`). High-risk edit/review
//! consumers require the evidence's file identity to equal the edit
//! authority's expected hash before relying on it.

use serde::{Deserialize, Serialize};

/// Number of fingerprint shards. Files are assigned to a shard by a stable
/// hash of their workspace-relative path; a verification round walks shard by
/// shard and only completes when every shard was visited to exhaustion.
pub const FINGERPRINT_SHARDS: u32 = 8;

/// Durable coverage of one workspace index generation: what the scan saw,
/// what it indexed, and whether the generation is complete. Persisted in the
/// generation envelope (`GenerationFile::coverage`); a missing record on a
/// legacy envelope decodes as COMPLETE (those builds were one-shot full
/// builds under the old caps and are grandfathered honestly as complete).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexCoverage {
    /// Regular files the batch walker observed (indexed or skipped as
    /// binary/oversized).
    pub files_seen: u64,
    /// Files actually inserted into the index.
    pub files_indexed: u64,
    /// Bytes read and indexed (never includes skipped files).
    pub bytes_indexed: u64,
    /// True exactly when the batch cursor exhausted the tree.
    pub complete: bool,
    /// Why the generation is incomplete (`batch_files`, `batch_bytes`,
    /// `batch_dirs`, ...); `None` for a complete generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<String>,
}

impl Default for IndexCoverage {
    /// The LEGACY default: an envelope with no coverage record was built by
    /// the pre-coverage one-shot scan and is treated as complete.
    fn default() -> Self {
        Self::complete()
    }
}

impl IndexCoverage {
    /// A complete, empty coverage record (legacy / freshly finished).
    pub fn complete() -> Self {
        Self {
            files_seen: 0,
            files_indexed: 0,
            bytes_indexed: 0,
            complete: true,
            truncated_reason: None,
        }
    }

    /// An empty, incomplete coverage record for a fresh generation build.
    pub fn empty() -> Self {
        Self {
            files_seen: 0,
            files_indexed: 0,
            bytes_indexed: 0,
            complete: false,
            truncated_reason: Some("build_started".to_string()),
        }
    }

    /// An honest partial record with a typed reason (e.g. a cold fallback
    /// package).
    pub fn partial(reason: impl Into<String>) -> Self {
        Self {
            files_seen: 0,
            files_indexed: 0,
            bytes_indexed: 0,
            complete: false,
            truncated_reason: Some(reason.into()),
        }
    }

    /// Retarget the record at a full scan that has not started.
    pub fn reset_for_build(&mut self) {
        *self = Self::empty();
    }

    /// A retrieval MISS under an incomplete generation must consult the
    /// bounded direct filesystem/Git fallback before claiming "no match".
    pub fn needs_fallback_on_miss(&self) -> bool {
        !self.complete
    }
}

/// One resumed directory frame of the deterministic batch walk: the walk
/// consumes a directory's entries in sorted-name order and records the name
/// of the last entry it consumed. `dir` is the workspace-relative directory
/// path (`""` = the root).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScanFrame {
    pub dir: String,
    pub after: String,
}

/// Durable continuation cursor of the CONTENT walk: the outer-to-inner stack
/// of directory frames the batch stopped in. Parsing is total (an empty
/// cursor restarts from the root); an entry whose name sorts at or before a
/// frame's `after` was already consumed by this generation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScanCursor {
    pub frames: Vec<ScanFrame>,
}

impl ScanCursor {
    pub fn is_root(&self) -> bool {
        self.frames.is_empty()
    }
}

/// Durable coverage of the stat-only fingerprint scan of one generation.
///
/// A verification ROUND is complete only when every shard was visited to
/// exhaustion; while a round is incomplete the workspace can never be
/// reported "fully clean". `round_start` rotates across rounds so a
/// repeatedly capped scan never begins at the same shard forever.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FingerprintCoverage {
    /// Files currently held in the persisted fingerprint (for diagnostics).
    pub scanned: u64,
    /// True exactly when the current round visited every shard to
    /// exhaustion.
    pub complete: bool,
    /// Shard the current round is working on.
    pub shard: u32,
    /// Shard the NEXT round starts at (rotation: a capped round must not
    /// perpetually ignore the same suffix).
    pub round_start: u32,
    /// Ordinal of the CURRENT round. The generation envelope tags its
    /// in-progress "next" map with this epoch, so a resumed round continues
    /// its own map and a fresh round starts from an empty one.
    #[serde(default)]
    pub epoch: u64,
    /// Bitmask of shards finished in the CURRENT round.
    pub shards_done: u64,
    /// True when this round VERIFIES against an existing baseline: any
    /// difference supersedes the generation. False while the generation's
    /// FIRST fingerprint baseline is still being established (entries are
    /// being recorded, not compared).
    #[serde(default = "verify_default")]
    pub verify: bool,
    /// Continuation cursor PER SHARD (index = shard id). A single round
    /// cursor would advance past files belonging to other shards and then
    /// "complete" those shards with zero entries, silently losing their
    /// files — exactly the suffix-ignored failure this audit removes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cursors: Vec<ScanCursor>,
    /// Why the fingerprint is partial (`fingerprint_files`,
    /// `fingerprint_dirs`, ...); `None` for complete coverage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<String>,
}

/// Legacy/default verification flag: an existing complete fingerprint is a
/// verified baseline.
fn verify_default() -> bool {
    true
}

impl Default for FingerprintCoverage {
    /// Legacy envelopes (no fingerprint coverage) are complete: their
    /// fingerprint list was one bounded full walk under the old caps.
    fn default() -> Self {
        Self::complete()
    }
}

impl FingerprintCoverage {
    /// A complete, empty fingerprint coverage (legacy / finished round).
    pub fn complete() -> Self {
        Self {
            scanned: 0,
            complete: true,
            shard: 0,
            round_start: 0,
            epoch: 0,
            shards_done: (1u64 << FINGERPRINT_SHARDS) - 1,
            verify: true,
            cursors: Vec::new(),
            truncated_reason: None,
        }
    }

    /// The generation's FIRST round: the fingerprint baseline is being
    /// recorded, so scanned entries are never "changes".
    pub fn baseline() -> Self {
        Self {
            verify: false,
            ..Self::round(0)
        }
    }

    /// A fresh, incomplete VERIFICATION round starting at `round_start`:
    /// entries are compared against the existing baseline and any difference
    /// supersedes the generation.
    pub fn round(round_start: u32) -> Self {
        Self {
            scanned: 0,
            complete: false,
            shard: round_start % FINGERPRINT_SHARDS,
            round_start: round_start % FINGERPRINT_SHARDS,
            epoch: 0,
            shards_done: 0,
            verify: true,
            cursors: Vec::new(),
            truncated_reason: Some("fingerprint_round_started".to_string()),
        }
    }

    /// [`FingerprintCoverage::round`] with an explicit epoch (the envelope's
    /// in-progress map is tagged with it).
    pub fn round_at_epoch(round_start: u32, epoch: u64) -> Self {
        Self {
            epoch,
            ..Self::round(round_start)
        }
    }

    /// The continuation cursor of one shard (empty = start of that shard).
    pub fn cursor_for(&self, shard: u32) -> ScanCursor {
        self.cursors
            .get(shard as usize % FINGERPRINT_SHARDS as usize)
            .cloned()
            .unwrap_or_default()
    }

    /// Persist the continuation cursor of one shard.
    pub fn set_cursor(&mut self, shard: u32, cursor: ScanCursor) {
        let idx = shard as usize % FINGERPRINT_SHARDS as usize;
        if self.cursors.len() <= idx {
            self.cursors.resize(idx + 1, ScanCursor::default());
        }
        self.cursors[idx] = cursor;
    }

    /// The all-shards-done mask of the current round.
    pub fn all_shards() -> u64 {
        (1u64 << FINGERPRINT_SHARDS) - 1
    }

    /// Mark a shard exhausted and advance (completing the round when every
    /// shard is done, rotating `round_start` for the next round).
    pub fn finish_shard(&mut self, shard: u32) {
        let shard = shard % FINGERPRINT_SHARDS;
        self.shards_done |= 1u64 << shard;
        self.set_cursor(shard, ScanCursor::default());
        if self.shards_done == Self::all_shards() {
            self.complete = true;
            self.truncated_reason = None;
            self.cursors.clear();
            // A completed round advances the epoch: the envelope's finished
            // "next" map becomes the baseline and a new round starts empty.
            self.epoch = self.epoch.wrapping_add(1);
            self.round_start = self.round_start.wrapping_add(1) % FINGERPRINT_SHARDS;
            self.shard = self.round_start;
        } else {
            self.shard = (shard + 1) % FINGERPRINT_SHARDS;
        }
    }

    /// True when `shard` was already finished in the current round.
    pub fn shard_done(&self, shard: u32) -> bool {
        self.shards_done & (1u64 << (shard % FINGERPRINT_SHARDS)) != 0
    }

    pub fn needs_round(&self) -> bool {
        self.complete
    }
}

/// Stable shard assignment of one workspace-relative path.
pub fn fingerprint_shard(path: &str) -> u32 {
    let hash = blake3::hash(path.as_bytes());
    u32::from(hash.as_bytes()[0]) % FINGERPRINT_SHARDS
}

/// Typed freshness of one evidence package (audit 16).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceFreshness {
    /// The package was built from a complete, at-rest published generation.
    Current,
    /// A published generation exists but a rebuild/continuation is in
    /// flight; the package reflects the newest published snapshot.
    StaleWhileRebuilding,
    /// The package's generation (or fingerprint) coverage is incomplete.
    Partial,
}

impl EvidenceFreshness {
    pub fn as_str(self) -> &'static str {
        match self {
            EvidenceFreshness::Current => "current",
            EvidenceFreshness::StaleWhileRebuilding => "stale_while_rebuilding",
            EvidenceFreshness::Partial => "partial",
        }
    }
}

/// Identity + freshness metadata every evidence package carries (audit 16).
/// `content_identity` digests the generation's content + fingerprint;
/// `fingerprint_identity` digests only the fingerprint. `None` identities are
/// honest ("no generation identity available", e.g. the cold direct-read
/// fallback), never a fabricated digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidencePackageMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint_identity: Option<String>,
    pub freshness: EvidenceFreshness,
    pub coverage: IndexCoverage,
    pub fingerprint: FingerprintCoverage,
}

impl EvidencePackageMeta {
    /// Metadata of a cold (non-generation) package: no generation identity,
    /// coverage honestly incomplete, freshness `stale_while_rebuilding` for
    /// a persisted old generation and `partial` for direct reads.
    pub fn cold(generation: Option<u64>, freshness: EvidenceFreshness) -> Self {
        Self {
            generation,
            content_identity: None,
            fingerprint_identity: None,
            freshness,
            coverage: IndexCoverage::partial("cold_evidence"),
            fingerprint: FingerprintCoverage::round(0),
        }
    }
}

/// Diagnostics snapshot of one workspace's index coverage (the `/native`
/// coverage route and both IDE panels consume this).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexCoverageSnapshot {
    pub workspace: u64,
    /// Stable machine spelling of the durable index state
    /// (`not_started`/`building`/`ready`/`dirty`/`failed`).
    pub state: String,
    /// The row generation (build target while building).
    pub generation: u64,
    /// The published generation, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_generation: Option<u64>,
    pub coverage: IndexCoverage,
    pub fingerprint: FingerprintCoverage,
    pub freshness: EvidenceFreshness,
    /// True while indexed content can be served to readers.
    pub serving: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_defaults_are_complete_and_partial_helpers_are_honest() {
        assert!(IndexCoverage::default().complete);
        assert_eq!(IndexCoverage::default().truncated_reason, None);
        assert!(FingerprintCoverage::default().complete);
        assert_eq!(
            FingerprintCoverage::default().shards_done,
            FingerprintCoverage::all_shards()
        );
        let partial = IndexCoverage::partial("batch_files");
        assert!(!partial.complete);
        assert!(partial.needs_fallback_on_miss());
        assert!(!IndexCoverage::complete().needs_fallback_on_miss());
        assert_eq!(partial.truncated_reason.as_deref(), Some("batch_files"));
    }

    #[test]
    fn fingerprint_round_rotates_and_never_completes_without_every_shard() {
        assert!(FingerprintCoverage::complete().verify);
        assert!(!FingerprintCoverage::baseline().verify);
        assert!(FingerprintCoverage::round(0).verify);
        let encoded = serde_json::to_value(FingerprintCoverage::baseline()).unwrap();
        let mut legacy = encoded.as_object().unwrap().clone();
        legacy.remove("verify");
        let decoded: FingerprintCoverage =
            serde_json::from_value(serde_json::Value::Object(legacy)).unwrap();
        assert!(
            decoded.verify,
            "a missing verify flag means a baseline existed"
        );
        let mut cov = FingerprintCoverage::round(3);
        assert_eq!(cov.shard, 3);
        assert!(!cov.complete);
        // Visiting shards out of order still completes only when every
        // shard is done.
        for shard in [3u32, 4, 5, 6, 7, 0, 1] {
            cov.finish_shard(shard);
            assert!(!cov.complete, "missing shard {shard}");
        }
        cov.finish_shard(2);
        assert!(cov.complete);
        assert_eq!(cov.truncated_reason, None);
        // The next round starts at the rotated position, never at a fixed
        // shard: a repeatedly capped scan cannot ignore a suffix forever.
        assert_eq!(cov.round_start, 4);
        assert_eq!(cov.shard, 4);
        let next = FingerprintCoverage::round(cov.round_start);
        assert_eq!(next.shard, 4);
        assert_eq!(next.shards_done, 0);
    }

    #[test]
    fn shard_assignment_is_stable_and_in_range() {
        for path in ["a.rs", "src/lib.rs", "deep/nested/dir/file.py", "é.rs"] {
            let s = fingerprint_shard(path);
            assert!(s < FINGERPRINT_SHARDS, "{path} -> {s}");
            assert_eq!(s, fingerprint_shard(path));
        }
    }

    #[test]
    fn evidence_package_meta_roundtrips_and_cold_is_honest() {
        let meta = EvidencePackageMeta {
            generation: Some(7),
            content_identity: Some("abc".into()),
            fingerprint_identity: Some("def".into()),
            freshness: EvidenceFreshness::Partial,
            coverage: IndexCoverage::partial("batch_files"),
            fingerprint: FingerprintCoverage::round(2),
        };
        let json = serde_json::to_string(&meta).unwrap();
        assert_eq!(
            serde_json::from_str::<EvidencePackageMeta>(&json).unwrap(),
            meta
        );
        assert_eq!(meta.freshness.as_str(), "partial");
        let cold = EvidencePackageMeta::cold(Some(3), EvidenceFreshness::StaleWhileRebuilding);
        assert!(cold.content_identity.is_none());
        assert!(!cold.coverage.complete);
        // A hostile/absent required field is a typed decode error, never a
        // silent default.
        assert!(serde_json::from_str::<EvidencePackageMeta>(r#"{"freshness":"current"}"#).is_err());
    }
}
