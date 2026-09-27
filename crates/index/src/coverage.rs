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

/// Reason spelling of a pre-coverage envelope: the generation carries no
/// coverage metadata, so its completeness is UNKNOWN — never complete.
pub const LEGACY_UNKNOWN_REASON: &str = "legacy_unknown";

/// Durable coverage of one workspace index generation: what the scan saw,
/// what it indexed, and whether the generation is complete. Persisted in the
/// generation envelope (`GenerationFile::coverage`); a missing record on a
/// pre-coverage envelope decodes as [`IndexCoverage::legacy_unknown`] —
/// completeness is UNKNOWN, never silently complete.
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
    /// the pre-coverage scan and its completeness is UNKNOWN, never a
    /// silently complete value.
    fn default() -> Self {
        Self::legacy_unknown()
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

    /// The LEGACY-UNKNOWN record: a pre-coverage envelope carries no
    /// coverage metadata, so the generation is SERVED as stale data but its
    /// misses must consult the bounded direct fallback until a rebuild
    /// replaces it. NEVER complete.
    pub fn legacy_unknown() -> Self {
        Self::partial(LEGACY_UNKNOWN_REASON)
    }

    /// True when this record is the legacy-unknown marker (see
    /// [`IndexCoverage::legacy_unknown`]).
    pub fn is_legacy_unknown(&self) -> bool {
        !self.complete && self.truncated_reason.as_deref() == Some(LEGACY_UNKNOWN_REASON)
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
    /// Legacy envelopes (no fingerprint coverage) are NEVER fully clean:
    /// their fingerprint list has UNKNOWN completeness (see
    /// [`FingerprintCoverage::legacy_unknown`]).
    fn default() -> Self {
        Self::legacy_unknown()
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

    /// A pre-coverage fingerprint: the envelope carries no shard round
    /// state, so the fingerprint is NEVER fully clean until a fresh baseline
    /// round re-establishes it (`verify: false`: the one-shot legacy list is
    /// not a trustworthy verification baseline).
    pub fn legacy_unknown() -> Self {
        Self {
            verify: false,
            truncated_reason: Some(LEGACY_UNKNOWN_REASON.to_string()),
            ..Self::round(0)
        }
    }

    /// True when this record is the legacy-unknown marker (see
    /// [`FingerprintCoverage::legacy_unknown`]).
    pub fn is_legacy_unknown(&self) -> bool {
        !self.complete && self.truncated_reason.as_deref() == Some(LEGACY_UNKNOWN_REASON)
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
    /// The package's generation predates coverage metadata: its
    /// completeness is UNKNOWN. Served as stale data with fallback on
    /// misses until a rebuild replaces it.
    LegacyUnknown,
}

impl EvidenceFreshness {
    pub fn as_str(self) -> &'static str {
        match self {
            EvidenceFreshness::Current => "current",
            EvidenceFreshness::StaleWhileRebuilding => "stale_while_rebuilding",
            EvidenceFreshness::Partial => "partial",
            EvidenceFreshness::LegacyUnknown => "legacy_unknown",
        }
    }

    /// Classify a generation from its coverage records: a pre-coverage
    /// envelope (legacy-unknown record) is [`EvidenceFreshness::LegacyUnknown`];
    /// any incomplete record is [`EvidenceFreshness::Partial`]; a complete
    /// generation with a rebuild in flight is
    /// [`EvidenceFreshness::StaleWhileRebuilding`]; otherwise
    /// [`EvidenceFreshness::Current`].
    pub fn classify(
        coverage: &IndexCoverage,
        fingerprint: &FingerprintCoverage,
        rebuilding: bool,
    ) -> Self {
        if coverage.is_legacy_unknown() || fingerprint.is_legacy_unknown() {
            EvidenceFreshness::LegacyUnknown
        } else if !coverage.complete || !fingerprint.complete {
            EvidenceFreshness::Partial
        } else if rebuilding {
            EvidenceFreshness::StaleWhileRebuilding
        } else {
            EvidenceFreshness::Current
        }
    }

    /// Wire-safe freshness for the strict IDE-panel contract (the panels
    /// reject any value outside `current`/`stale_while_rebuilding`/`partial`):
    /// the legacy-unknown state surfaces through the coverage record's
    /// `truncated_reason` ("legacy_unknown") while the freshness field stays
    /// `partial` — never `current`.
    pub fn wire(self) -> Self {
        match self {
            EvidenceFreshness::LegacyUnknown => EvidenceFreshness::Partial,
            other => other,
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
    fn legacy_defaults_are_unknown_never_complete_and_partial_helpers_are_honest() {
        // Absence of coverage metadata is UNKNOWN completeness, never a
        // silently complete generation: misses must fall back and a rebuild
        // is due.
        let legacy = IndexCoverage::default();
        assert!(!legacy.complete);
        assert!(legacy.is_legacy_unknown());
        assert_eq!(legacy.truncated_reason.as_deref(), Some("legacy_unknown"));
        assert!(legacy.needs_fallback_on_miss());
        assert_eq!(IndexCoverage::legacy_unknown(), legacy);
        let legacy_fp = FingerprintCoverage::default();
        assert!(!legacy_fp.complete);
        assert!(legacy_fp.is_legacy_unknown());
        assert!(!legacy_fp.verify, "the legacy list is not a baseline");
        assert_eq!(legacy_fp.shards_done, 0);
        assert_eq!(FingerprintCoverage::legacy_unknown(), legacy_fp);
        // The EXPLICIT complete records stay complete.
        assert!(IndexCoverage::complete().complete);
        assert!(!IndexCoverage::complete().is_legacy_unknown());
        assert!(FingerprintCoverage::complete().complete);
        assert_eq!(
            FingerprintCoverage::complete().shards_done,
            FingerprintCoverage::all_shards()
        );
        let partial = IndexCoverage::partial("batch_files");
        assert!(!partial.complete);
        assert!(!partial.is_legacy_unknown());
        assert!(partial.needs_fallback_on_miss());
        assert!(!IndexCoverage::complete().needs_fallback_on_miss());
        assert_eq!(partial.truncated_reason.as_deref(), Some("batch_files"));
        // Freshness classification: legacy -> LegacyUnknown; incomplete ->
        // Partial; complete + rebuild -> StaleWhileRebuilding; else Current.
        assert_eq!(
            EvidenceFreshness::classify(&legacy, &FingerprintCoverage::complete(), false),
            EvidenceFreshness::LegacyUnknown
        );
        assert_eq!(
            EvidenceFreshness::classify(&IndexCoverage::complete(), &legacy_fp, false),
            EvidenceFreshness::LegacyUnknown
        );
        assert_eq!(
            EvidenceFreshness::classify(&partial, &FingerprintCoverage::complete(), true),
            EvidenceFreshness::Partial
        );
        assert_eq!(
            EvidenceFreshness::classify(
                &IndexCoverage::complete(),
                &FingerprintCoverage::complete(),
                true
            ),
            EvidenceFreshness::StaleWhileRebuilding
        );
        assert_eq!(
            EvidenceFreshness::classify(
                &IndexCoverage::complete(),
                &FingerprintCoverage::complete(),
                false
            ),
            EvidenceFreshness::Current
        );
        assert_eq!(EvidenceFreshness::LegacyUnknown.as_str(), "legacy_unknown");
        // The strict IDE-panel wire contract only accepts the three
        // established values: legacy-unknown must map to partial, never
        // current.
        assert_eq!(
            EvidenceFreshness::LegacyUnknown.wire(),
            EvidenceFreshness::Partial
        );
        assert_eq!(
            EvidenceFreshness::Current.wire(),
            EvidenceFreshness::Current
        );
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
