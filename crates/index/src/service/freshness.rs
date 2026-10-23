//! Content-freshness fast path and bounded exact recovery (P1).
//!
//! The fast path hashes exactly the files a watcher event named. Periodic and
//! recovery passes use the shared `faktor-freshness` records: size/mtime
//! shortlist suspects for an incremental round, while an EXACT round BLAKE3
//! hashes every visited material file over rotating shards. A shortlist round
//! never certifies clean; it transitions into an exact recovery round.

use std::collections::BTreeMap;
use std::path::Path;

use crate::coverage::{fingerprint_shard, FINGERPRINT_SHARDS};
use faktor_freshness::{Freshness, Observation};

use super::*;

/// Fast watcher-event path bounds: touched paths are hashed directly, but a
/// storm selects the bounded exact recovery sweep instead of unbounded work.
pub(super) const MAX_FAST_TOUCHED_PATHS: usize = 1_024;
const MAX_FAST_TOUCHED_BYTES: u64 = 8 * 1024 * 1024;

/// Outcome of one bounded fingerprint round pass (audit 6, interleaved):
/// ONE physical walk collects the entries of every shard the current round
/// still owes.
pub(super) struct FingerprintRoundPass {
    /// Entries per shard id (only incomplete shards have buckets; every
    /// bucket is sorted by path after the walk).
    pub(super) entries: Vec<Vec<FingerprintEntry>>,
    /// True when the walk exhausted the tree: every remaining shard was
    /// visited to exhaustion by THIS pass.
    pub(super) walk_completed: bool,
    /// The shared continuation cursor (resume exactly after the last
    /// consumed entry; the stopping entry is re-visited).
    pub(super) cursor: ScanCursor,
    pub(super) reason: Option<String>,
    /// The cancellation token fired during the pass (audit 20): nothing is
    /// merged and the build is abandoned, never published.
    pub(super) cancelled: bool,
    /// Files actually BLAKE3-hashed by this pass (metadata shortlists hash
    /// only suspects; an exact recovery pass hashes every visited file).
    pub(super) hashed_files: usize,
    /// Bytes hashed by this pass (bounded by the configured budget).
    pub(super) hashed_bytes: u64,
}

/// Fast watcher-event path: BLAKE3-hash exactly the touched files and
/// compare their content digests against the published fingerprint.
/// Clean touched paths are dropped without a rebuild (an identical
/// rewrite is a cheap no-op); a proven difference marks the generation
/// dirty; anything unprovable (oversized/unreadable/overflow/cancel)
/// schedules the bounded rotating exact sweep instead of a metadata
/// verdict.
pub(super) fn verify_touched(
    svc: &IndexService,
    workspace: WorkspaceId,
    cancel: Option<&CancellationToken>,
) -> Result<(), IndexError> {
    let (root, stored, touched, overflow, ready) = {
        let mut live = lock_recover(&svc.inner.live);
        let Some(l) = live.get_mut(&workspace) else {
            return Ok(());
        };
        let ready = matches!(l.state, WorkspaceIndexState::Ready { .. }) && l.content.is_some();
        let touched = std::mem::take(&mut l.touched);
        let overflow = std::mem::take(&mut l.touch_overflow);
        if !ready || (touched.is_empty() && !overflow) {
            // Not ready yet: keep the paths for the next pass instead of
            // dropping an event the build never saw.
            if !ready {
                l.touched = touched;
                l.touch_overflow |= overflow;
            }
            return Ok(());
        }
        (
            l.root.clone(),
            l.last_fingerprint.clone().unwrap_or_default(),
            touched,
            overflow,
            ready,
        )
    };
    if !ready {
        return Ok(());
    }
    let Some(root) = root else {
        return Ok(());
    };
    let stored: BTreeMap<&str, &FingerprintEntry> = stored
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let mut dirty = false;
    let mut recovery = overflow;
    let mut hashed_bytes = 0u64;
    for rel in &touched {
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            recovery = true;
            break;
        }
        let previous = stored.get(rel.as_str()).copied();
        match faktor_freshness::observe_path(&root, rel, SCAN_MAX_FILE_BYTES, cancel) {
            Observation::Hashed { record, bytes } => {
                hashed_bytes = hashed_bytes.saturating_add(bytes);
                match previous {
                    None => dirty = true,
                    Some(previous) => match faktor_freshness::classify(previous, &record) {
                        Freshness::Clean => {}
                        Freshness::Dirty(_) => dirty = true,
                        Freshness::Unverified => recovery = true,
                    },
                }
            }
            Observation::Missing => {
                // A path that is still absent was not a material file;
                // a stored path that disappeared is a deletion.
                if previous.is_some() {
                    dirty = true;
                }
            }
            Observation::NotRegular => {
                if previous.is_some() {
                    dirty = true;
                }
            }
            Observation::TooLarge { .. } | Observation::Unreadable { .. } => {
                recovery = true;
            }
            Observation::Cancelled => {
                recovery = true;
                break;
            }
        }
        if hashed_bytes > MAX_FAST_TOUCHED_BYTES {
            recovery = true;
            break;
        }
    }
    let mut live = lock_recover(&svc.inner.live);
    let Some(l) = live.get_mut(&workspace) else {
        return Ok(());
    };
    if dirty {
        l.dirty_now = true;
        l.pending = true;
    } else if recovery {
        l.recovery_pending = true;
        l.pending = true;
    } else if l.touched.is_empty() && !l.touch_overflow {
        // Every event path was proven byte-identical: nothing is owed.
        if !l.recovery_pending {
            l.pending = false;
        }
    }
    Ok(())
}

/// Apply one pass to the durable round state: fold the pass's per-shard
/// buckets, finish every owed shard when the walk exhausted the tree, or keep
/// the SHARED STOP cursor when it was capped. Returns `true` when a
/// verification round observed a difference (the caller supersedes the
/// generation).
///
/// An EXACT round hashes every visited file and can complete clean. A
/// SHORTLIST round hashes only metadata suspects, so finishing it is never
/// clean proof: on a clean finish it transitions into an exact recovery
/// round instead of setting `complete`.
pub(super) fn apply_fingerprint_round_pass(
    fp_cov: &mut FingerprintCoverage,
    fp_stored: &mut BTreeMap<String, FingerprintEntry>,
    fp_next: &mut BTreeMap<String, FingerprintEntry>,
    pass: FingerprintRoundPass,
) -> bool {
    let exact = fp_cov.exact;
    let mut dirty = false;
    for bucket in pass.entries {
        for entry in bucket {
            match fp_stored.get(&entry.path) {
                Some(previous) => match faktor_freshness::classify(previous, &entry) {
                    Freshness::Clean => {}
                    Freshness::Dirty(_) => {
                        if fp_cov.verify {
                            dirty = true;
                        }
                    }
                    // An unhashed observation is never proof of unchanged
                    // bytes; it can only be unverified.
                    Freshness::Unverified => {
                        if fp_cov.verify {
                            dirty = true;
                        }
                    }
                },
                None => {
                    // A baseline round RECORDS the fingerprint; only a
                    // verification round can call a difference a change.
                    if fp_cov.verify {
                        dirty = true;
                    }
                }
            }
            fp_next.insert(entry.path.clone(), entry);
        }
    }
    if pass.walk_completed {
        // A verification exact round must have seen every stored entry or
        // the difference (a deletion/rename) supersedes the generation.
        if exact && fp_cov.verify && fp_stored.keys().ne(fp_next.keys()) {
            dirty = true;
        }
        // The walk exhausted the tree in ONE pass: every shard the round
        // still owed was visited to exhaustion.
        for shard in 0..FINGERPRINT_SHARDS {
            if !fp_cov.shard_done(shard) {
                fp_cov.finish_shard(shard);
            }
        }
        if fp_cov.complete {
            if exact {
                // A baseline round MERGES the observed map into the
                // accumulation it has been publishing all along (nothing may
                // be lost). A verification round replaces it only when clean.
                if fp_cov.verify {
                    if !dirty {
                        *fp_stored = std::mem::take(fp_next);
                    }
                } else {
                    for (path, entry) in std::mem::take(fp_next) {
                        fp_stored.insert(path, entry);
                    }
                }
            } else if !dirty {
                // Shortlist completeness is NOT clean: rotate immediately
                // into the exact BLAKE3 recovery round.
                let start = fp_cov.round_start;
                let epoch = fp_cov.epoch;
                *fp_cov = FingerprintCoverage::round_at_epoch(start, epoch);
                fp_next.clear();
            }
        }
    } else {
        // A capped walk keeps its STOP position: the stopping entry was not
        // consumed and no incomplete shard was advanced past a file of its
        // own.
        for shard in 0..FINGERPRINT_SHARDS {
            if !fp_cov.shard_done(shard) {
                fp_cov.set_cursor(shard, pass.cursor.clone());
            }
        }
        fp_cov.truncated_reason = Some(
            pass.reason
                .unwrap_or_else(|| "fingerprint_batch".to_string()),
        );
    }
    dirty
}

/// One bounded INTERLEAVED fingerprint pass (audit 6, revised): ONE physical
/// walk over the round's shared cursor collects the entries of EVERY shard
/// the round still owes, so a full round enumerates the tree once instead of
/// once per shard.
///
/// EXACT rounds BLAKE3-hash every visited file (the recovery authority).
/// SHORTLIST rounds use size/mtime/identity only to select suspects and hash
/// those; they never certify unchanged bytes. Both are bounded by files,
/// directories and hashed bytes, and a capped pass is never reported clean.
/// `pace`/`cancel` are the slow-filesystem seam and the caller's token.
#[allow(clippy::too_many_arguments)]
pub(super) fn fingerprint_shared_pass(
    root: &Path,
    fp_cov: &FingerprintCoverage,
    baseline: &BTreeMap<String, FingerprintEntry>,
    max_files: usize,
    max_dirs: usize,
    max_bytes: u64,
    pace: Option<Duration>,
    cancel: Option<&CancellationToken>,
) -> Result<FingerprintRoundPass, String> {
    let mut entries: Vec<Vec<FingerprintEntry>> =
        (0..FINGERPRINT_SHARDS).map(|_| Vec::new()).collect();
    let mut total = 0usize;
    let mut hashed_files = 0usize;
    let mut hashed_bytes = 0u64;
    let mut cancelled = false;
    let cursor = fp_cov.shared_round_cursor();
    let walked = walk_batch(root, &cursor, max_dirs, pace, cancel, |rel, path| {
        let shard = fingerprint_shard(rel);
        if fp_cov.shard_done(shard) {
            return WalkControl::Continue;
        }
        if total >= max_files {
            return WalkControl::Stop("fingerprint_files");
        }
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            cancelled = true;
            return WalkControl::Stop("cancelled");
        }
        let Ok(meta) = fs::symlink_metadata(path) else {
            return WalkControl::Continue;
        };
        if !meta.file_type().is_file() || meta.len() > SCAN_MAX_FILE_BYTES {
            return WalkControl::Continue;
        }
        total += 1;
        let current = FingerprintEntry::from_metadata(rel, &meta);
        if !fp_cov.exact && !faktor_freshness::needs_hash(baseline.get(rel), &current) {
            // Metadata shortlist: equal size/mtime/identity selects nothing
            // to hash. This is NOT proof of unchanged bytes; the round will
            // transition into the exact recovery sweep.
            return WalkControl::Continue;
        }
        if hashed_bytes.saturating_add(meta.len()) > max_bytes && hashed_bytes > 0 {
            return WalkControl::Stop("fingerprint_bytes");
        }
        match faktor_freshness::observe_path(root, rel, SCAN_MAX_FILE_BYTES, cancel) {
            Observation::Hashed { record, bytes } => {
                hashed_files += 1;
                hashed_bytes = hashed_bytes.saturating_add(bytes);
                entries[shard as usize].push(record);
                WalkControl::Continue
            }
            Observation::Missing | Observation::NotRegular | Observation::TooLarge { .. } => {
                // Not material (vanished, replaced by a non-file, or beyond
                // the content cap): omission from an exact round is caught by
                // the stored-vs-observed path comparison.
                WalkControl::Continue
            }
            Observation::Unreadable { reason } => {
                // The content scan also skips an unreadable file, so it is
                // not a material input. Omission from an exact round is
                // caught by the stored-vs-observed path comparison; a later
                // readable sweep re-admits it.
                tracing::warn!(path = rel, reason = %reason, "fingerprint read skipped");
                WalkControl::Continue
            }
            Observation::Cancelled => {
                cancelled = true;
                WalkControl::Stop("cancelled")
            }
        }
    })?;
    if cancelled {
        for bucket in &mut entries {
            bucket.clear();
        }
    }
    for bucket in &mut entries {
        bucket.sort();
    }
    Ok(FingerprintRoundPass {
        entries,
        walk_completed: walked.complete,
        cursor: walked.cursor,
        reason: walked.reason,
        cancelled: walked.cancelled || cancelled,
        hashed_files,
        hashed_bytes,
    })
}
