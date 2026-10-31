// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! Replay-horizon aging of HEAVY BODIES (audit 12, P1): the two-tier split
//! of the old "KeepForever" evidence retention.
//!
//! Two tiers, structurally separate:
//!
//! 1. **PROOF IDENTITY — permanent.** The durable evidence row (id, kind,
//!    scope, source revision, provenance, compact body, backing DIGEST,
//!    completeness, compression record, retrieval policy, timestamp) is
//!    never deleted by this module. It is enough metadata to say WHAT
//!    disappeared: the digest-only continuity record survives every sweep
//!    ([`crate::types::EvidenceEnvelope::backing_hash`]), a loss marker in
//!    the storage-governor ledger names the swept bytes, and reads of a
//!    swept digest fail loudly ("recorded but its bytes are absent"), never
//!    by substitution.
//! 2. **HEAVY BODY — replay-horizon retained.** Full logs, snapshots and
//!    bulky payloads live as CAS blobs. After
//!    [`DEFAULT_REPLAY_HORIZON_MS`] (configurable, serde-defaulted) the
//!    body becomes EVICTABLE while its proof record stays. Sweeping is
//!    explicit, accounted and marked; a healthy within-horizon body is
//!    protected by its insert-time LEASE pin and can never be removed by
//!    quota pressure.
//!
//! Lifecycle states of one heavy body (transition names used in tests and
//! reports):
//!
//! ```text
//! LeasedWithinHorizon  --age pass, beyond horizon-->  EvictableBeyondHorizon
//! EvictableBeyondHorizon --verified delete + loss marker + accounting--> Swept (digest-only)
//! EvictableBeyondHorizon --recovery pin-->  RecoveryPinned (refused; lease holds)
//! crash mid-sweep       --next pass-->      Reconciled (idempotent, exactly-once markers)
//! ```
//!
//! Crash safety is ordered around TWO durable breadcrumbs: the lease pin row
//! and the object-accounting row are removed LAST (atomically together), so
//! every crash window leaves "lease present" and the next pass re-runs the
//! idempotent remainder — loss markers dedup by (class, key, reason), so
//! each is recorded exactly once no matter where the process died. See
//! [`DurableEvidenceAuthority::age_heavy_bodies_at`] for the seam points.
//!
//! Pins (storage-governor integration): the insert-time pin with reason
//! `"evidence_backing"` is a LEASE, not a proof pin — this module releases
//! it only after the body is beyond its horizon and its bytes + accounting
//! are gone. Proof pins and recovery pins live under NAMESPACE-QUALIFIED
//! keys ([`PROOF_PIN_PREFIX`], [`RECOVERY_PIN_PREFIX`]) that are never the
//! object's own key, so pinning a proof can never mark the bulky body
//! required (audit: "pinning a proof never pins its bulky body"), while an
//! explicit recovery pin refuses aging of that body.

use std::collections::HashSet;

use faktor_cas::Cas;
use faktor_core::hash::FileHash;
use faktor_storage::ledger::Ledger;
use faktor_storage::{GovernorResult, StorageClass, StorageGovernor};
use serde::{Deserialize, Serialize};

use crate::store::{normalize_digest, DurableEvidenceAuthority, EvidenceCrashSeam};
use crate::types::{EvidenceError, EvidenceId};

/// Conservative default replay horizon: **30 days** in milliseconds.
///
/// Reason for the value: a long-lived install must keep full replay
/// material for at least a month of investigation/audit windows — far
/// beyond any normal dev iteration cycle (days) — while guaranteeing the
/// evidence CAS quota cannot be dominated *forever* by bulky old output.
/// Proof identity is permanent regardless of this horizon, so nothing
/// verifiable is lost when a body ages: only the expensive replay bytes
/// leave, marked and digest-accounted.
pub const DEFAULT_REPLAY_HORIZON_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// Hard bound of one aging/status scan: the LOWEST `AGING_SCAN_BOUND` ids
/// (ids are issued monotonically, so the lowest ids are the oldest rows —
/// exactly the ones eligible for aging). A pass over a bigger table sets
/// `scan_truncated` instead of scanning unboundedly; the operator re-runs
/// with a larger bound by letting later passes cover the next window.
pub const AGING_SCAN_BOUND: u64 = 16_384;

/// Loss-marker reason recorded in the storage-governor ledger for every
/// swept heavy body (dedup key: class + key + reason ⇒ exactly once).
pub const EVIDENCE_AGED_LOSS_REASON: &str = "evidence_backing_aged";
/// Eviction-history reason for the reclaimed accounting row.
pub const EVIDENCE_AGED_EVICTION_REASON: &str = "evidence_body_aged";

/// Namespaced governor pin key prefix for RECOVERY-CRITICAL bodies. The key
/// is never the object's own digest key, so it can be read back with the
/// public `is_pinned` probe and can never be released by lease handling.
pub const RECOVERY_PIN_PREFIX: &str = "evidence_recovery:";
/// Namespaced governor pin key prefix for PROOF-IDENTITY claims. Proof
/// permanence is structural (rows are never aged); the claim key exists so
/// an audit can see it — and, being namespaced, it can never pin the body.
pub const PROOF_PIN_PREFIX: &str = "evidence_proof:";

/// Sharer lookup bound: a digest referenced by this many rows is refused
/// conservatively (its age can no longer be proven for EVERY owner from a
/// bounded read, so nothing is deleted).
const SHARER_LOOKUP_LIMIT: usize = 1_024;
/// Maximum swept-body detail entries carried in one report (counts stay
/// exact for every sweep; the detail list is bounded).
const SWEPT_DETAIL_LIMIT: usize = 64;

/// Opportunistic write-path aging cadence: at most one automatic pass per
/// hour (mirroring the storage layer's hourly aggregation cadence). Bodies
/// only become evictable after a multi-week replay horizon, so an hourly
/// sweep is far more often than needed; the constant exists to bound the
/// work, not to tune freshness.
pub(crate) const AGING_WRITE_CADENCE_MS: i64 = 3_600_000;

/// Heavy-body aging policy. Every field is serde-defaulted, so existing
/// configs that predate this section deserialize unchanged to the safe
/// defaults (backward compatible by construction).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EvidenceAgingConfig {
    /// `true` (default): aging passes may sweep bodies beyond the horizon.
    /// `false`: [`DurableEvidenceAuthority::age_heavy_bodies`] is a fully
    /// inert no-op (nothing scanned, nothing deleted) — safe disable.
    pub enabled: bool,
    /// Replay horizon in milliseconds; a body created less than this long
    /// ago is never evictable. Must be strictly positive (validated).
    pub replay_horizon_ms: i64,
}

impl Default for EvidenceAgingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            replay_horizon_ms: DEFAULT_REPLAY_HORIZON_MS,
        }
    }
}

impl EvidenceAgingConfig {
    /// Typed refusal for nonsensical policies: a zero or negative horizon
    /// would make every body immediately evictable (or meaningless), which
    /// is never a safe coercion — the pass fails closed instead.
    pub fn validated(&self) -> Result<(), EvidenceError> {
        if self.replay_horizon_ms <= 0 {
            return Err(EvidenceError::Refused(format!(
                "evidence replay horizon must be a positive number of ms, got {}",
                self.replay_horizon_ms
            )));
        }
        Ok(())
    }
}

/// One swept heavy body, as described to an auditor: WHICH proof lost its
/// bytes (`id`), the digest-only continuity record (`digest`) and how many
/// stored bytes left the quota (`bytes`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SweptBody {
    pub id: EvidenceId,
    pub digest: String,
    pub bytes: u64,
}

/// What one [`DurableEvidenceAuthority::age_heavy_bodies_at`] pass did.
/// Counters are exact per pass; `swept_detail` is bounded to
/// [`SWEPT_DETAIL_LIMIT`] entries while `swept` counts every sweep.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceAgingReport {
    /// The pass was disabled by config: fully inert, no scan performed.
    pub disabled: bool,
    /// Evidence rows examined (bounded by [`AGING_SCAN_BOUND`]).
    pub scanned_rows: u64,
    /// The table extends past this pass's id window; re-run to cover more.
    pub scan_truncated: bool,
    /// Rows without any backing digest (proof identity only; nothing to age).
    pub no_backing_rows: u64,
    /// Rows still within the replay horizon: their bodies never evict.
    pub within_horizon_rows: u64,
    /// Bodies deleted this pass (bytes verified before deletion).
    pub swept: u64,
    /// Stored bytes freed by the sweeps of this pass.
    pub swept_bytes: u64,
    /// Bounded detail of what disappeared (id, digest, bytes).
    pub swept_detail: Vec<SweptBody>,
    /// Crash leftovers completed this pass (marker/accounting/lease).
    pub reconciled: u64,
    /// Lease pins released without accounting rows (publish-crash residue;
    /// those bytes were never charged, so no marker is owed).
    pub released_leases: u64,
    /// Bodies refused because they carry an explicit recovery pin.
    pub recovery_refused: u64,
    /// Digests kept because at least one referencing row is within horizon.
    pub shared_within_horizon: u64,
    /// Digests kept because the sharer lookup hit its bound (or a referencer
    /// vanished mid-scan): age not provable for every owner ⇒ no delete.
    pub shared_unbounded: u64,
    /// Digests whose content is already gone with nothing owed (idempotent
    /// residue of a completed sweep or a policy-dropped capture).
    pub already_absent: u64,
}

/// Read-only dry-run plan of the same scan an aging pass would run: what a
/// pass WOULD sweep, refuse or reconcile right now. Never mutates. Surfaced
/// as this crate's health/degraded view of the retention lifecycle (the
/// storage-governor doctor line already reflects the quota effects).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceAgingStatus {
    /// The installed policy (serde-defaulted).
    pub config: EvidenceAgingConfig,
    /// A storage governor is attached; without one aging refuses typed and
    /// nothing is considered evictable.
    pub governed: bool,
    pub scanned_rows: u64,
    pub scan_truncated: bool,
    pub no_backing_rows: u64,
    pub within_horizon_rows: u64,
    /// Bodies a pass would sweep right now.
    pub evictable_bodies: u64,
    /// Stored bytes a pass would free right now.
    pub evictable_bytes: u64,
    /// Crash leftovers a pass would finish (marker + accounting).
    pub pending_reconcile_bodies: u64,
    /// Publish-crash lease residues a pass would release.
    pub pending_lease_releases: u64,
    /// Recovery-pinned bodies aging would refuse.
    pub recovery_refused_bodies: u64,
    /// Digests held by a within-horizon sharer.
    pub shared_blocked_bodies: u64,
    /// Digests held by the sharer-lookup bound.
    pub shared_unbounded_bodies: u64,
    /// Digests already gone with nothing owed.
    pub already_absent_bodies: u64,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn governor_call<T>(result: GovernorResult<T>, what: &str) -> Result<T, EvidenceError> {
    result.map_err(|error| EvidenceError::Malformed(format!("{what}: {error}")))
}

/// One beyond-horizon row that triggered a digest-level decision.
struct BeyondRow {
    id: EvidenceId,
    digest: String,
}

/// Counters gathered by the bounded id scan (row granularity).
#[derive(Debug, Default)]
struct ScanCounts {
    scanned_rows: u64,
    no_backing_rows: u64,
    within_horizon_rows: u64,
    truncated: bool,
}

/// The decision for ONE digest once every gate passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DigestVerdict {
    /// Content present, every referencer beyond horizon, not recovery
    /// pinned: sweep it (verify → delete → marker → accounting → lease).
    Sweep,
    /// Content gone but its accounting row survived a crash: finish the
    /// marker + accounting exactly once.
    Reconcile,
    /// Publish-crash lease pin with no content and no accounting: release
    /// the pin; no marker is owed (the bytes were never charged).
    ReleaseLease,
    /// Explicit recovery pin: refuse, count, touch nothing.
    RecoveryRefused,
    /// A referencing row is within horizon: the shared body stays.
    SharedBlocked,
    /// Sharer age unprovable (bound hit / vanished row): conservative keep.
    SharedUnbounded,
    /// Content gone and nothing owed: idempotent residue.
    NoBody,
}

/// One aging/status pass over the authority: holds the lazily opened
/// governor ledger (a second SQLite connection over the SAME WAL database
/// the governor uses — the pattern `faktor-storage`'s own history tests
/// exercise) next to the shared authority reference.
struct AgingPass<'a> {
    authority: &'a DurableEvidenceAuthority,
    ledger: Option<Ledger>,
}

/// The read-only inputs an aging dry run needs: evidence rows, the blob
/// store, and (when attached) the retention governor. The in-process
/// authority builds this view over ITS handles; the out-of-process `doctor`
/// reader builds the same view directly from the store — a status never
/// requires constructing an authority, so no CLI site can mint a parallel
/// authority over the rows (the `ONE durable evidence authority` invariant
/// stays structurally intact).
struct AgingView<'a> {
    store: &'a std::sync::Arc<faktor_store::Store>,
    cas: &'a std::sync::Arc<Cas>,
    governor: Option<&'a std::sync::Arc<StorageGovernor>>,
}

impl DurableEvidenceAuthority {
    /// The governor retention decisions run through. Absence is a TYPED
    /// refusal: never delete bytes the governor cannot account for.
    pub(crate) fn retention_governor(
        &self,
    ) -> Result<&std::sync::Arc<StorageGovernor>, EvidenceError> {
        self.cas().governor().ok_or_else(|| {
            EvidenceError::Refused(
                "evidence heavy-body retention requires an attached storage governor; refusing to act on unaccounted bytes"
                    .to_string(),
            )
        })
    }

    /// Run one heavy-body aging pass at wall-clock `now_ms`.
    ///
    /// Order of ONE sweep (each step idempotent, each seam crash-restartable):
    ///
    /// 1. gates: config validated → enabled → governor present → digest
    ///    verdict (recovery pin, sharer ages, content, accounting, lease);
    /// 2. `evidence_body_aged_pre` seam — nothing mutated yet;
    /// 3. `Cas::evict_verified` — strict verify THEN delete; a digest
    ///    mismatch is a loud typed [`EvidenceError::Malformed`] and the
    ///    bytes stay on disk;
    /// 4. `evidence_body_aged_deleted` seam — content gone, lease + accounting
    ///    still present (the crash breadcrumb);
    /// 5. loss marker (`evidence_backing_aged`, dedup ⇒ exactly once);
    /// 6. `evidence_body_aged_marked` seam;
    /// 7. accounting row + lease pin removed together (last), reclaiming the
    ///    quota exactly once.
    ///
    /// A crash anywhere re-runs this pass; every intermediate state maps to
    /// a verdict that completes the remainder without double counting.
    pub fn age_heavy_bodies_at(&self, now_ms: i64) -> Result<EvidenceAgingReport, EvidenceError> {
        let config = self.aging_config();
        config.validated()?;
        let mut report = EvidenceAgingReport {
            disabled: !config.enabled,
            ..EvidenceAgingReport::default()
        };
        if !config.enabled {
            // Fully inert: no scan, no ledger, no governor needed.
            return Ok(report);
        }
        let _governor = self.retention_governor()?;
        let (beyond, counts) = self.scan_beyond(now_ms, config.replay_horizon_ms)?;
        report.scanned_rows = counts.scanned_rows;
        report.scan_truncated = counts.truncated;
        report.no_backing_rows = counts.no_backing_rows;
        report.within_horizon_rows = counts.within_horizon_rows;

        let seam = self.crash_seam();
        let mut pass = AgingPass {
            authority: self,
            ledger: None,
        };
        let mut seen: HashSet<String> = HashSet::new();
        for row in beyond {
            if !seen.insert(row.digest.clone()) {
                continue;
            }
            let (verdict, hint) = pass.verdict(&row.digest, now_ms, config.replay_horizon_ms)?;
            match verdict {
                DigestVerdict::Sweep => {
                    pass.sweep(&row.digest, row.id, hint, seam, &mut report)?;
                }
                DigestVerdict::Reconcile => {
                    pass.record_loss_marker(&row.digest, hint)?;
                    pass.remove_accounting(&row.digest, hint)?;
                    report.reconciled += 1;
                }
                DigestVerdict::ReleaseLease => {
                    pass.release_lease(&row.digest)?;
                    report.released_leases += 1;
                }
                DigestVerdict::RecoveryRefused => report.recovery_refused += 1,
                DigestVerdict::SharedBlocked => report.shared_within_horizon += 1,
                DigestVerdict::SharedUnbounded => report.shared_unbounded += 1,
                DigestVerdict::NoBody => report.already_absent += 1,
            }
        }
        Ok(report)
    }

    /// Run one heavy-body aging pass at wall-clock now.
    pub fn age_heavy_bodies(&self) -> Result<EvidenceAgingReport, EvidenceError> {
        self.age_heavy_bodies_at(now_ms())
    }

    /// Opportunistic write-path aging cadence: run a full pass at most once
    /// per `max_age_ms`, non-blocking. A pass already running on another
    /// writer thread makes this a no-op (`Ok(false)`), so evidence inserts
    /// never queue behind a sweep — the same always-on maintenance
    /// discipline the storage governor applies in its reserve path. The
    /// caller's insert proceeds regardless; a cadence failure is the
    /// caller's to log, never a refusal of the write.
    ///
    /// Returns `Ok(true)` only when the pass actually reclaimed something
    /// (swept, reconciled or released); a run that found nothing and a
    /// skipped run both return `Ok(false)`.
    pub fn age_heavy_bodies_if_due(&self, max_age_ms: i64) -> Result<bool, EvidenceError> {
        self.age_heavy_bodies_if_due_at(now_ms(), max_age_ms)
    }

    /// The time-injectable core of [`Self::age_heavy_bodies_if_due`] (tests
    /// drive staleness and the horizon with an explicit `now`; production
    /// passes the wall clock).
    pub fn age_heavy_bodies_if_due_at(
        &self,
        now: i64,
        max_age_ms: i64,
    ) -> Result<bool, EvidenceError> {
        if max_age_ms <= 0 {
            return Err(EvidenceError::Refused(format!(
                "evidence aging cadence must be a positive number of ms, got {max_age_ms}"
            )));
        }
        let last = self
            .aging_last_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        if now.saturating_sub(last) < max_age_ms {
            return Ok(false);
        }
        let Ok(_gate) = self.aging_gate.try_lock() else {
            return Ok(false);
        };
        // Re-check under the gate: another thread may have just finished a
        // pass in the window between the staleness read and the acquire.
        let last = self
            .aging_last_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        if now.saturating_sub(last) < max_age_ms {
            return Ok(false);
        }
        let report = self.age_heavy_bodies_at(now)?;
        self.aging_last_ms
            .store(now, std::sync::atomic::Ordering::Relaxed);
        Ok(report.swept > 0 || report.reconciled > 0 || report.released_leases > 0)
    }

    /// Unix ms of the last opportunistic aging pass (`0`/`None` = never).
    pub fn aging_last_pass_ms(&self) -> Option<i64> {
        let last = self
            .aging_last_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        (last > 0).then_some(last)
    }

    /// The read-only dry-run plan of [`Self::aging_status_at`] at wall-clock
    /// now. Never mutates anything.
    pub fn aging_status(&self) -> Result<EvidenceAgingStatus, EvidenceError> {
        self.aging_status_at(now_ms())
    }

    /// What an aging pass would do at `now_ms`, without doing it. With no
    /// governor attached, aging refuses typed, so NOTHING is reported
    /// evictable (the row-scan counters are still honest).
    pub fn aging_status_at(&self, now_ms: i64) -> Result<EvidenceAgingStatus, EvidenceError> {
        self.aging_view().status_at(self.aging_config(), now_ms)
    }

    /// The read-only inputs an aging dry run needs: evidence rows, the blob
    /// store, and (when attached) the retention governor. The in-process
    /// authority builds this view over ITS handles; the out-of-process
    /// `doctor` reader builds the same view directly from the store — a
    /// status never requires constructing an authority, so no CLI site can
    /// mint a parallel authority over the rows (the `ONE durable evidence
    /// authority` invariant stays structurally intact).
    fn aging_view(&self) -> AgingView<'_> {
        AgingView {
            store: self.store(),
            cas: self.cas(),
            governor: self.cas().governor(),
        }
    }

    /// Out-of-process READ-ONLY aging status for `faktor doctor`: the same
    /// dry-run plan as [`Self::aging_status`], computed from the store
    /// handle WITHOUT constructing an authority (no mutation capability, no
    /// id/backing identity state — the daemon's `DaemonGraph::evidence`
    /// remains the only authority over these rows). The caller must only
    /// invoke this when the backing CAS directory already exists (the
    /// doctor checks first; this fn never creates it deliberately — the
    /// `open_in_class` fallback keeps a pre-existing directory intact).
    pub fn read_only_aging_status(
        store: std::sync::Arc<faktor_store::Store>,
    ) -> Result<EvidenceAgingStatus, EvidenceError> {
        let root = store.root().join("evidence-cas");
        // NO silent fresh-handle fallback on this read path: an unopenable
        // backing CAS is a loud typed error (doctor prints UNREADABLE and
        // counts an issue) — the reviewed `for_store` fallback exists for
        // the daemon's authority, never for out-of-process reads.
        let cas = std::sync::Arc::new(
            Cas::open_in_class(root, faktor_storage::StorageClass::EvidenceCas).map_err(
                |error| {
                    EvidenceError::Malformed(format!("evidence backing CAS unreadable: {error}"))
                },
            )?,
        );
        let config = EvidenceAgingConfig::default();
        AgingView {
            store: &store,
            cas: &cas,
            governor: cas.governor(),
        }
        .status_at(config, now_ms())
    }

    /// Mark this evidence's HEAVY BODY recovery-critical: aging refuses to
    /// sweep it (counted, untouched) until [`Self::unpin_backing_for_recovery`]
    /// releases the claim. The pin lives under [`RECOVERY_PIN_PREFIX`], never
    /// on the object key, so lease handling and accounting stay the only
    /// owners of the object key itself.
    pub fn pin_backing_for_recovery(&self, id: EvidenceId) -> Result<(), EvidenceError> {
        let digest = self.body_digest(id)?;
        let key = format!("{RECOVERY_PIN_PREFIX}{digest}");
        governor_call(
            self.retention_governor()?.pin(
                StorageClass::EvidenceCas,
                &key,
                "evidence_recovery_critical",
            ),
            "evidence recovery pin",
        )
    }

    /// Release a recovery claim so the body ages normally again.
    pub fn unpin_backing_for_recovery(&self, id: EvidenceId) -> Result<(), EvidenceError> {
        let digest = self.body_digest(id)?;
        let key = format!("{RECOVERY_PIN_PREFIX}{digest}");
        governor_call(
            self.retention_governor()?
                .unpin(StorageClass::EvidenceCas, &key),
            "evidence recovery unpin",
        )
    }

    /// Is this body explicitly recovery-critical right now?
    pub fn is_body_recovery_pinned(&self, id: EvidenceId) -> Result<bool, EvidenceError> {
        let digest = self.body_digest(id)?;
        let key = format!("{RECOVERY_PIN_PREFIX}{digest}");
        governor_call(
            self.retention_governor()?
                .is_pinned(StorageClass::EvidenceCas, &key),
            "evidence recovery pin probe",
        )
    }

    /// Record a durable PROOF-IDENTITY claim for this evidence.
    ///
    /// The proof is the row itself — permanent by construction; this claim
    /// exists so an audit can read it back ([`Self::is_proof_pinned`]) — and
    /// because it lives under [`PROOF_PIN_PREFIX`] (never the object key)
    /// it can NEVER pin the bulky body: pinning a proof does not extend the
    /// body's life (audit 12). Refuses typed for an unknown id or without a
    /// governor to carry the claim; the row stays permanent either way.
    pub fn pin_proof_identity(&self, id: EvidenceId) -> Result<(), EvidenceError> {
        self.require_row(id)?;
        let key = format!("{PROOF_PIN_PREFIX}{}", id.0);
        governor_call(
            self.retention_governor()?.pin(
                StorageClass::EvidenceCas,
                &key,
                "evidence_proof_identity",
            ),
            "evidence proof pin",
        )
    }

    /// Is a proof-identity claim recorded for this evidence?
    pub fn is_proof_pinned(&self, id: EvidenceId) -> Result<bool, EvidenceError> {
        let key = format!("{PROOF_PIN_PREFIX}{}", id.0);
        governor_call(
            self.retention_governor()?
                .is_pinned(StorageClass::EvidenceCas, &key),
            "evidence proof pin probe",
        )
    }

    /// The digest-only body address of one row (typed errors for unknown
    /// ids, rows without a body, and malformed digests).
    fn body_digest(&self, id: EvidenceId) -> Result<String, EvidenceError> {
        let row = self.require_row(id)?;
        let raw = row.backing_cas_hash.ok_or_else(|| {
            EvidenceError::Refused(format!(
                "evidence {id} has no heavy body (no backing digest); there is nothing to pin"
            ))
        })?;
        normalize_digest(&raw)
    }

    fn require_row(&self, id: EvidenceId) -> Result<faktor_store::EvidenceRow, EvidenceError> {
        self.store()
            .evidence_get(id.0)?
            .ok_or_else(|| EvidenceError::Malformed(format!("unknown evidence id {id}")))
    }

    /// The bounded oldest-first scan: rows `1..=min(high_water,
    /// AGING_SCAN_BOUND)`. Digest normalization happens for EVERY backing
    /// row (a malformed durable digest is loud typed corruption, never a
    /// skipped row), and a row counts as within-horizon only when its
    /// `created_ms + horizon` still exceeds `now_ms` (saturating: a
    /// future-dated row is conservatively young).
    fn scan_beyond(
        &self,
        now_ms: i64,
        horizon_ms: i64,
    ) -> Result<(Vec<BeyondRow>, ScanCounts), EvidenceError> {
        self.aging_view().scan_beyond(now_ms, horizon_ms)
    }
}

impl<'a> AgingView<'a> {
    fn governor(&self) -> Result<&'a std::sync::Arc<StorageGovernor>, EvidenceError> {
        self.governor.ok_or_else(|| {
            EvidenceError::Refused(
                "evidence heavy-body retention requires an attached storage governor; refusing to act on unaccounted bytes"
                    .to_string(),
            )
        })
    }

    /// The bounded oldest-first scan: ids `1..=min(high_water,
    /// AGING_SCAN_BOUND)` (below `max_id`); a row past the bound marks
    /// `truncated` instead of extending the scan unboundedly. A row counts
    /// as within-horizon only when its `created_ms + horizon` still exceeds
    /// `now_ms` (saturating: a future-dated row is conservatively young).
    fn scan_beyond(
        &self,
        now_ms: i64,
        horizon_ms: i64,
    ) -> Result<(Vec<BeyondRow>, ScanCounts), EvidenceError> {
        let high_water = self.store.evidence_high_water()?;
        let max_id = high_water.min(AGING_SCAN_BOUND);
        let mut counts = ScanCounts {
            truncated: high_water > AGING_SCAN_BOUND,
            ..ScanCounts::default()
        };
        let mut beyond = Vec::new();
        for id in 1..=max_id {
            let Some(row) = self.store.evidence_get(id)? else {
                continue;
            };
            counts.scanned_rows += 1;
            let Some(raw) = row.backing_cas_hash.as_deref() else {
                counts.no_backing_rows += 1;
                continue;
            };
            let digest = normalize_digest(raw)?;
            if row.created_ms.saturating_add(horizon_ms) > now_ms {
                counts.within_horizon_rows += 1;
                continue;
            }
            beyond.push(BeyondRow {
                id: EvidenceId(id),
                digest,
            });
        }
        Ok((beyond, counts))
    }

    /// The dry-run plan itself: identical verdict logic to a real pass
    /// (one implementation), never mutating anything.
    fn status_at(
        &self,
        config: EvidenceAgingConfig,
        now_ms: i64,
    ) -> Result<EvidenceAgingStatus, EvidenceError> {
        config.validated()?;
        let governed = self.governor.is_some();
        let (beyond, counts) = self.scan_beyond(now_ms, config.replay_horizon_ms)?;
        let mut status = EvidenceAgingStatus {
            config,
            governed,
            scanned_rows: counts.scanned_rows,
            scan_truncated: counts.truncated,
            no_backing_rows: counts.no_backing_rows,
            within_horizon_rows: counts.within_horizon_rows,
            ..EvidenceAgingStatus::default()
        };
        if !governed {
            return Ok(status);
        }
        let mut ledger: Option<Ledger> = None;
        let mut seen: HashSet<String> = HashSet::new();
        for row in beyond {
            if !seen.insert(row.digest.clone()) {
                continue;
            }
            let (verdict, hint) =
                self.verdict(&mut ledger, &row.digest, now_ms, config.replay_horizon_ms)?;
            match verdict {
                DigestVerdict::Sweep => {
                    status.evictable_bodies += 1;
                    status.evictable_bytes += hint;
                }
                DigestVerdict::Reconcile => status.pending_reconcile_bodies += 1,
                DigestVerdict::ReleaseLease => status.pending_lease_releases += 1,
                DigestVerdict::RecoveryRefused => status.recovery_refused_bodies += 1,
                DigestVerdict::SharedBlocked => status.shared_blocked_bodies += 1,
                DigestVerdict::SharedUnbounded => status.shared_unbounded_bodies += 1,
                DigestVerdict::NoBody => status.already_absent_bodies += 1,
            }
        }
        Ok(status)
    }

    /// Gate order (fail-safe first): recovery pin → sharer ages → content →
    /// accounting → lease. Returns the verdict plus a byte hint (stored size
    /// for `Sweep`, accounting-row bytes for `Reconcile`, else 0). The ONE
    /// verdict implementation: the real aging pass and the read-only dry-run
    /// status share it, so a status can never disagree with a pass.
    fn verdict(
        &self,
        ledger: &mut Option<Ledger>,
        digest_hex: &str,
        now_ms: i64,
        horizon_ms: i64,
    ) -> Result<(DigestVerdict, u64), EvidenceError> {
        let class = StorageClass::EvidenceCas;
        let governor = self.governor()?;
        let recovery_key = format!("{RECOVERY_PIN_PREFIX}{digest_hex}");
        if governor_call(
            governor.is_pinned(class, &recovery_key),
            "evidence recovery pin probe",
        )? {
            return Ok((DigestVerdict::RecoveryRefused, 0));
        }
        let lease = governor_call(
            governor.is_pinned(class, digest_hex),
            "evidence lease pin probe",
        )?;

        // EVERY referencer must be beyond the horizon: a deduplicated blob
        // may be shared with a younger proof row, and an aged body must
        // never silently strip a within-horizon referencer.
        let sharers = self
            .store
            .evidence_ids_by_backing(digest_hex, SHARER_LOOKUP_LIMIT)?;
        if sharers.len() >= SHARER_LOOKUP_LIMIT {
            return Ok((DigestVerdict::SharedUnbounded, 0));
        }
        for id in sharers {
            let Some(row) = self.store.evidence_get(id)? else {
                // Referencer vanished mid-scan: age is not provable for
                // every owner — conservative keep, never a delete.
                return Ok((DigestVerdict::SharedUnbounded, 0));
            };
            if row.created_ms.saturating_add(horizon_ms) > now_ms {
                return Ok((DigestVerdict::SharedBlocked, 0));
            }
        }

        let hash = FileHash::from_hex(digest_hex).ok_or_else(|| {
            EvidenceError::Malformed(format!("{digest_hex:?} is not a BLAKE3 file hash"))
        })?;
        if self.cas.has(hash) {
            let bytes = self.cas.stored_size(hash).map_err(|error| {
                EvidenceError::Malformed(format!("evidence heavy body {digest_hex}: {error}"))
            })?;
            return Ok((DigestVerdict::Sweep, bytes));
        }
        // Content absent: accounting row (crash leftover) → lease (publish
        // residue) → nothing owed.
        let object_row = governor_call(
            ledger_for(governor, ledger)?.object_row(class, digest_hex),
            "evidence aging object probe",
        )?;
        if let Some(row) = object_row {
            return Ok((DigestVerdict::Reconcile, row.bytes));
        }
        if lease {
            return Ok((DigestVerdict::ReleaseLease, 0));
        }
        Ok((DigestVerdict::NoBody, 0))
    }
}

/// Lazily open (and cache) a connection to the governor's WAL ledger for
/// read/record operations shared by the aging pass and the status view.
fn ledger_for<'l>(
    governor: &StorageGovernor,
    cache: &'l mut Option<Ledger>,
) -> Result<&'l mut Ledger, EvidenceError> {
    if cache.is_none() {
        let path = StorageGovernor::database_path(governor.data_dir());
        *cache = Some(governor_call(
            Ledger::open(&path),
            "evidence aging governor ledger",
        )?);
    }
    Ok(cache.as_mut().expect("ledger set above"))
}

impl<'a> AgingPass<'a> {
    fn governor(&self) -> Result<&std::sync::Arc<StorageGovernor>, EvidenceError> {
        self.authority.retention_governor()
    }

    /// Lazily open the pass's own connection to the governor's WAL ledger.
    fn ledger(&mut self) -> Result<&mut Ledger, EvidenceError> {
        if self.ledger.is_none() {
            let path = {
                let governor = self.governor()?;
                StorageGovernor::database_path(governor.data_dir())
            };
            self.ledger = Some(governor_call(
                Ledger::open(&path),
                "evidence aging governor ledger",
            )?);
        }
        Ok(self.ledger.as_mut().expect("ledger set above"))
    }

    /// Gate order (fail-safe first): recovery pin → sharer ages → content →
    /// accounting → lease — the SINGLE implementation lives on
    /// [`AgingView::verdict`] (shared with the read-only status path); this
    /// wrapper hands the pass's cached governor ledger to it.
    fn verdict(
        &mut self,
        digest_hex: &str,
        now_ms: i64,
        horizon_ms: i64,
    ) -> Result<(DigestVerdict, u64), EvidenceError> {
        AgingView {
            store: self.authority.store(),
            cas: self.authority.cas(),
            governor: self.authority.cas().governor(),
        }
        .verdict(&mut self.ledger, digest_hex, now_ms, horizon_ms)
    }

    fn sweep(
        &mut self,
        digest_hex: &str,
        trigger: EvidenceId,
        hint: u64,
        seam: &EvidenceCrashSeam,
        report: &mut EvidenceAgingReport,
    ) -> Result<(), EvidenceError> {
        seam.trip("evidence_body_aged_pre");
        let hash = FileHash::from_hex(digest_hex).ok_or_else(|| {
            EvidenceError::Malformed(format!("{digest_hex:?} is not a BLAKE3 file hash"))
        })?;
        let (freed, _verified) = self.authority.cas().evict_verified(hash).map_err(|error| {
            EvidenceError::Malformed(format!(
                "evidence heavy body {digest_hex} failed pre-aging verification: {error}"
            ))
        })?;
        seam.trip("evidence_body_aged_deleted");
        // `freed == 0` means the blob raced to absent after the verdict; the
        // pre-measured size is still the honest accounting of what left.
        let accounted = if freed > 0 { freed } else { hint };
        self.record_loss_marker(digest_hex, accounted)?;
        seam.trip("evidence_body_aged_marked");
        self.remove_accounting(digest_hex, accounted)?;
        if freed > 0 {
            report.swept += 1;
            report.swept_bytes += freed;
            if report.swept_detail.len() < SWEPT_DETAIL_LIMIT {
                report.swept_detail.push(SweptBody {
                    id: trigger,
                    digest: digest_hex.to_string(),
                    bytes: freed,
                });
            }
        } else {
            report.reconciled += 1;
        }
        Ok(())
    }

    /// The durable, exactly-once loss marker: key + digest + bytes + reason,
    /// deduped by the ledger itself so a crash-retry never double counts.
    fn record_loss_marker(&mut self, digest_hex: &str, bytes: u64) -> Result<(), EvidenceError> {
        governor_call(
            self.ledger()?.record_loss_marker(
                StorageClass::EvidenceCas,
                digest_hex,
                Some(digest_hex),
                bytes,
                EVIDENCE_AGED_LOSS_REASON,
            ),
            "evidence aging loss marker",
        )
    }

    /// Reclaim quota accounting exactly once (eviction-history row included)
    /// and make sure the lease pin is gone: `remove_object_and_record_eviction`
    /// drops the object row TOGETHER with its pin; a lease pin without an
    /// object row (publish-crash residue) is released explicitly.
    fn remove_accounting(
        &mut self,
        digest_hex: &str,
        freed_bytes: u64,
    ) -> Result<(), EvidenceError> {
        let class = StorageClass::EvidenceCas;
        governor_call(
            self.ledger()?.remove_object_and_record_eviction(
                class,
                digest_hex,
                Some(digest_hex),
                freed_bytes,
                true,
                EVIDENCE_AGED_EVICTION_REASON,
            ),
            "evidence aging accounting removal",
        )?;
        if governor_call(
            self.governor()?.is_pinned(class, digest_hex),
            "evidence lease pin probe",
        )? {
            governor_call(
                self.governor()?.unpin(class, digest_hex),
                "evidence lease release",
            )?;
        }
        Ok(())
    }

    fn release_lease(&mut self, digest_hex: &str) -> Result<(), EvidenceError> {
        governor_call(
            self.governor()?
                .unpin(StorageClass::EvidenceCas, digest_hex),
            "evidence lease release",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{EvidenceAccessContext, EvidenceCrashArm};
    use crate::types::{
        BackingCompleteness, CompactRepresentation, Compressibility, CompressionRecord,
        EvidenceEnvelope, EvidenceKind, ProvenanceSet, ProvenanceSource, RetrievalPolicy,
    };
    use faktor_cas::Cas;
    use faktor_core::{SessionId, WorkspaceId};
    use faktor_storage::StorageGovernorConfig;
    use faktor_store::EvidenceRow;
    use std::sync::Arc;

    struct Fixture {
        _dir: tempfile::TempDir,
        store: Arc<faktor_store::Store>,
        cas: Arc<Cas>,
        governor: Arc<StorageGovernor>,
        authority: DurableEvidenceAuthority,
        session: SessionId,
        workspace: WorkspaceId,
    }

    fn governed_with(quota: u64, config: EvidenceAgingConfig) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(faktor_store::Store::open(dir.path().join("db"), true).unwrap());
        let config_governor = StorageGovernorConfig {
            total_budget_bytes: 1_000_000,
            watermark_permille: 900,
            reservation_lease_ms: 60_000,
            // Hermetic against HOST free space (band_config precedent): the
            // volume-reserve path still executes at the clamped minimum
            // instead of the production 5% — these fixtures test evidence
            // retention, not the operator's disk pressure.
            reserve_permille: 1,
            reserve_bytes: 0,
            quotas: StorageClass::ALL
                .into_iter()
                .map(|class| {
                    (
                        class,
                        if class == StorageClass::EvidenceCas {
                            quota
                        } else {
                            100_000
                        },
                    )
                })
                .collect(),
            ..Default::default()
        }
        .validated()
        .unwrap();
        let governor = Arc::new(StorageGovernor::open(dir.path(), config_governor).unwrap());
        let mut cas =
            Cas::open_in_class(dir.path().join("evidence-cas"), StorageClass::EvidenceCas).unwrap();
        cas.attach_governor(governor.clone(), StorageClass::EvidenceCas);
        let authority = DurableEvidenceAuthority::new(store.clone(), Arc::new(cas), 4096)
            .with_aging_config(config);
        let workspace = store.create_workspace("/w").unwrap();
        let session = store.create_session(workspace, "t", "p", "m").unwrap().id;
        Fixture {
            _dir: dir,
            store,
            cas: {
                // Re-derive the shared handle from the authority so fixture
                // assertions observe the SAME CAS allocation.
                authority.cas().clone()
            },
            governor,
            authority,
            session,
            workspace,
        }
    }

    fn governed(quota: u64) -> Fixture {
        governed_with(quota, EvidenceAgingConfig::default())
    }

    /// A store whose CAS has NO governor attached: every retention action
    /// must fail closed (typed) instead of deleting unaccounted bytes. The
    /// governor database itself exists (so ledger probes stay honest), but
    /// the CAS never learned about it.
    fn bare() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(faktor_store::Store::open(dir.path().join("db"), true).unwrap());
        let cas =
            Cas::open_in_class(dir.path().join("evidence-cas"), StorageClass::EvidenceCas).unwrap();
        let config = StorageGovernorConfig {
            quotas: StorageClass::ALL
                .into_iter()
                .map(|class| (class, 100_000))
                .collect(),
            ..Default::default()
        }
        .validated()
        .unwrap();
        let governor = Arc::new(StorageGovernor::open(dir.path(), config).unwrap());
        let authority = DurableEvidenceAuthority::new(store.clone(), Arc::new(cas), 4096);
        assert!(
            authority.cas().governor().is_none(),
            "fixture must be ungoverned (no discoverable governor in a temp dir)"
        );
        let workspace = store.create_workspace("/w").unwrap();
        let session = store.create_session(workspace, "t", "p", "m").unwrap().id;
        Fixture {
            _dir: dir,
            store,
            cas: authority.cas().clone(),
            governor,
            authority,
            session,
            workspace,
        }
    }

    fn env(f: &Fixture, compact_body: &str) -> EvidenceEnvelope {
        EvidenceEnvelope::new(
            EvidenceId(0),
            EvidenceKind::ProcessLog,
            f.session,
            f.workspace,
            Some(7),
            Some("rev-1".to_string()),
            ProvenanceSet::new([ProvenanceSource::Tool]),
            Compressibility::Aggressive,
            CompactRepresentation {
                grammar: "log-v1".to_string(),
                body: compact_body.to_string(),
            },
            None,
            BackingCompleteness::Complete,
            CompressionRecord::identity(17),
            RetrievalPolicy::new(true, true, 64),
        )
        .unwrap()
    }

    fn insert(f: &Fixture, compact_body: &str, payload: &[u8]) -> EvidenceEnvelope {
        f.authority
            .insert_new(&env(f, compact_body), Some(payload))
            .unwrap()
    }

    fn crafted_row(
        id: u64,
        env: &EvidenceEnvelope,
        created_ms: i64,
        digest_hex: &str,
    ) -> EvidenceRow {
        EvidenceRow {
            id,
            session_id: env.session_id,
            workspace_id: env.workspace_id,
            task_id: env.task_id,
            kind: serde_json::to_string(&env.kind)
                .unwrap()
                .trim_matches('"')
                .to_string(),
            revision: 1,
            provenance_json: serde_json::to_string(&env.provenance).unwrap(),
            compressibility: serde_json::to_string(&env.compressibility)
                .unwrap()
                .trim_matches('"')
                .to_string(),
            compression_json: serde_json::to_string(&env.compression).unwrap(),
            retrieval_json: serde_json::to_string(&env.retrieval).unwrap(),
            compact_json: serde_json::json!({
                "grammar": env.compact.grammar,
                "body": env.compact.body,
                "source_revision": env.source_revision,
            })
            .to_string(),
            backing_cas_hash: Some(digest_hex.to_string()),
            completeness: serde_json::to_string(&env.backing_completeness)
                .unwrap()
                .trim_matches('"')
                .to_string(),
            created_ms,
        }
    }

    /// Deterministic incompressible noise: adversarial bodies must not
    /// vanish under CAS compression, so quota math sees real bytes.
    fn payload(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed ^ 0x9E37_79B9_7F4A_7C15;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    fn hex(bytes: &[u8; 32]) -> String {
        FileHash::from(*bytes).to_hex()
    }

    fn ctx(f: &Fixture) -> EvidenceAccessContext {
        EvidenceAccessContext::new(f.session.raw(), f.workspace.raw(), Some(7))
    }

    fn stored_bytes(f: &Fixture, digest_hex: &str) -> u64 {
        let hash = FileHash::from_hex(digest_hex).unwrap();
        std::fs::metadata(f.cas.root().join(hash.cas_path()))
            .unwrap()
            .len()
    }

    fn usage(f: &Fixture) -> u64 {
        f.governor
            .metrics()
            .unwrap()
            .per_class
            .get("evidence_cas")
            .map(|u| u.bytes)
            .unwrap_or(0)
    }

    fn markers(f: &Fixture) -> Vec<(String, u64, String)> {
        f.governor
            .loss_markers(Some(StorageClass::EvidenceCas))
            .unwrap()
    }

    /// A `now` safely past any row's default replay horizon.
    fn far_future() -> i64 {
        now_ms() + DEFAULT_REPLAY_HORIZON_MS + 60_000
    }

    // ------------------------------------------------------------------
    // Beyond horizon: the body leaves, the proof identity survives whole.
    // ------------------------------------------------------------------
    #[test]
    fn beyond_horizon_sweeps_the_body_while_the_proof_identity_survives_intact() {
        let f = governed(100_000);
        let raw = payload(1, 2048);
        let mut e = env(&f, "proof body");
        e.compression = CompressionRecord::identity(raw.len() as u64);
        let stored = f.authority.insert_new(&e, Some(&raw)).unwrap();
        let digest = hex(&stored.backing_hash.unwrap());
        let bytes_before = stored_bytes(&f, &digest);
        let before = f.authority.get_scoped(stored.id, &ctx(&f)).unwrap();

        let report = f.authority.age_heavy_bodies_at(far_future()).unwrap();
        assert!(!report.disabled, "default config is enabled");
        assert_eq!(report.swept, 1, "one heavy body crossed the horizon");
        assert_eq!(report.swept_bytes, bytes_before);
        assert_eq!(
            report.swept_detail,
            vec![SweptBody {
                id: stored.id,
                digest: digest.clone(),
                bytes: bytes_before,
            }],
            "the report must say exactly what disappeared"
        );

        // PROOF IDENTITY: every permanent field byte-identical, including
        // the digest-only continuity record.
        let after = f.authority.get_scoped(stored.id, &ctx(&f)).unwrap();
        assert!(after.backing.is_none(), "body is gone by design");
        assert_eq!(
            after.envelope, before.envelope,
            "proof identity must survive the sweep byte-for-byte"
        );
        assert_eq!(
            after.envelope.backing_hash,
            Some(FileHash::from_hex(&digest).unwrap().bytes()),
            "the digest-only continuity record must survive"
        );
        assert_eq!(
            after.envelope.compression.original_bytes,
            raw.len() as u64,
            "enough metadata to say what disappeared (original size)"
        );

        // Loud typed absence: the known digest never substitutes content.
        match f.authority.read_backing_by_digest(&digest, &ctx(&f)) {
            Err(EvidenceError::Malformed(m)) => assert!(m.contains("absent"), "{m}"),
            other => panic!("a swept digest must fail loudly, got {other:?}"),
        }
        assert!(!f.cas.has(FileHash::from_hex(&digest).unwrap()));

        // Explicit, accounted, marked: the loss marker names bytes + reason.
        let markers = markers(&f);
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].0, digest);
        assert_eq!(markers[0].1, bytes_before);
        assert_eq!(markers[0].2, EVIDENCE_AGED_LOSS_REASON);
        assert!(f.governor.counters().unwrap().loss_markers >= 1);

        // The proof row is still listed; the quota is actually free.
        assert_eq!(
            f.authority.list_scoped(&ctx(&f), 100).unwrap(),
            vec![stored.id]
        );
        assert_eq!(usage(&f), 0);
    }

    #[test]
    fn within_horizon_bodies_and_quota_pressure_never_evict_a_leased_body() {
        // Tight quota (1000) mirroring the admission-refusal path: the lease
        // pin must survive a failed over-quota admission AND an explicit
        // enforce(), until the horizon itself passes.
        let f = governed(1_000);
        let raw = payload(7, 40);
        let stored = insert(&f, "young body", &raw);
        let digest = hex(&stored.backing_hash.unwrap());
        let bytes = stored_bytes(&f, &digest);

        // Over-quota admission is refused and must not strip the lease.
        let refused = f
            .authority
            .insert_new(&env(&f, "too big"), Some(&payload(8, 2000)));
        assert!(refused.is_err(), "over-quota body must be refused");
        assert!(f.governor.counters().unwrap().refusals >= 1);

        let now = now_ms();
        let report = f.authority.age_heavy_bodies_at(now).unwrap();
        assert_eq!(report.swept, 0, "nothing inside the horizon evicts");
        assert_eq!(report.within_horizon_rows, 1);

        // Explicit enforcement can never take a leased body either.
        let _ = f.governor.enforce().unwrap();
        assert!(f.cas.has(FileHash::from_hex(&digest).unwrap()));
        assert_eq!(usage(&f), bytes);
        assert!(f
            .governor
            .is_pinned(StorageClass::EvidenceCas, &digest)
            .unwrap());

        // One millisecond before the boundary: still inside. Exactly AT
        // `created + horizon`: evictable. The boundary is strict `<=`.
        let row = f.store.evidence_get(stored.id.0).unwrap().unwrap();
        let report = f
            .authority
            .age_heavy_bodies_at(row.created_ms + DEFAULT_REPLAY_HORIZON_MS - 1)
            .unwrap();
        assert_eq!(report.swept, 0);
        assert_eq!(report.within_horizon_rows, 1);
        let report = f
            .authority
            .age_heavy_bodies_at(row.created_ms + DEFAULT_REPLAY_HORIZON_MS)
            .unwrap();
        assert_eq!(
            report.swept, 1,
            "the boundary itself makes the body evictable"
        );
        assert_eq!(usage(&f), 0);
    }

    // ------------------------------------------------------------------
    // Pins: recovery refuses aging; a proof pin never pins the body.
    // ------------------------------------------------------------------
    #[test]
    fn recovery_pinned_bodies_refuse_aging_until_released() {
        let f = governed(100_000);
        let raw = payload(2, 1024);
        let stored = insert(&f, "recovery body", &raw);
        let digest = hex(&stored.backing_hash.unwrap());
        let bytes = stored_bytes(&f, &digest);

        f.authority.pin_backing_for_recovery(stored.id).unwrap();
        assert!(f.authority.is_body_recovery_pinned(stored.id).unwrap());

        let report = f.authority.age_heavy_bodies_at(far_future()).unwrap();
        assert_eq!(report.swept, 0, "a recovery-critical body never ages out");
        assert_eq!(report.recovery_refused, 1);
        assert!(f.cas.has(FileHash::from_hex(&digest).unwrap()));
        assert!(f
            .governor
            .is_pinned(StorageClass::EvidenceCas, &digest)
            .unwrap());
        assert!(markers(&f).is_empty(), "a refusal is not a loss");
        assert_eq!(usage(&f), bytes);
        let alive = f.authority.get_scoped(stored.id, &ctx(&f)).unwrap();
        assert_eq!(alive.backing.as_deref(), Some(&raw[..]));

        // Explicit release restores the normal lifecycle.
        f.authority.unpin_backing_for_recovery(stored.id).unwrap();
        assert!(!f.authority.is_body_recovery_pinned(stored.id).unwrap());
        let report = f.authority.age_heavy_bodies_at(far_future()).unwrap();
        assert_eq!(report.swept, 1);
        assert!(!f.cas.has(FileHash::from_hex(&digest).unwrap()));
        assert_eq!(usage(&f), 0);
    }

    #[test]
    fn pinning_a_proof_never_pins_its_bulky_body() {
        let f = governed(100_000);
        let raw = payload(3, 1024);
        let stored = insert(&f, "proof with bulky body", &raw);
        let digest = hex(&stored.backing_hash.unwrap());

        f.authority.pin_proof_identity(stored.id).unwrap();
        assert!(f.authority.is_proof_pinned(stored.id).unwrap());
        assert!(
            !f.authority.is_body_recovery_pinned(stored.id).unwrap(),
            "a proof pin must not create a body pin"
        );

        // The body ages out anyway: proof permanence is structural (the row
        // survives), it never extends the heavy body's life.
        let report = f.authority.age_heavy_bodies_at(far_future()).unwrap();
        assert_eq!(report.swept, 1, "a proof pin must not block aging");
        assert!(!f.cas.has(FileHash::from_hex(&digest).unwrap()));
        let after = f.authority.get_scoped(stored.id, &ctx(&f)).unwrap();
        assert!(after.backing.is_none());
        assert_eq!(
            after.envelope.backing_hash,
            Some(FileHash::from_hex(&digest).unwrap().bytes())
        );

        // The durable proof claim itself outlives the sweep (namespaced
        // key, object key fully released).
        assert!(f.authority.is_proof_pinned(stored.id).unwrap());
        assert!(!f
            .governor
            .is_pinned(StorageClass::EvidenceCas, &digest)
            .unwrap());

        // Unknown ids are typed, never a fabricated claim.
        let err = f
            .authority
            .pin_proof_identity(EvidenceId(4242))
            .unwrap_err();
        assert!(matches!(err, EvidenceError::Malformed(_)), "{err:?}");
        assert!(err.to_string().contains("4242"), "{err}");
    }

    // ------------------------------------------------------------------
    // Degradation: a digest mismatch fails typed, bytes untouched.
    // ------------------------------------------------------------------
    #[test]
    fn degraded_body_digest_mismatch_fails_typed_and_keeps_the_bytes() {
        let f = governed(100_000);
        let raw = payload(4, 2048);
        let stored = insert(&f, "degradable body", &raw);
        let digest = hex(&stored.backing_hash.unwrap());
        let bytes = stored_bytes(&f, &digest);
        let blob = f
            .cas
            .root()
            .join(FileHash::from_hex(&digest).unwrap().cas_path());
        std::fs::write(&blob, b"not a zstd frame at all").unwrap();

        let err = f.authority.age_heavy_bodies_at(far_future()).unwrap_err();
        assert!(matches!(err, EvidenceError::Malformed(_)), "{err:?}");
        assert!(err.to_string().contains("verification"), "{err}");
        assert!(err.to_string().contains(&digest), "{err}");

        // The unverifiable bytes are NEVER destroyed, the lease and the
        // accounting survive for a human/heal path, and no marker lies
        // about a loss that did not happen.
        assert!(blob.exists(), "eviction must not destroy unverified bytes");
        assert!(f
            .governor
            .is_pinned(StorageClass::EvidenceCas, &digest)
            .unwrap());
        assert_eq!(usage(&f), bytes);
        assert!(markers(&f).is_empty());
        assert!(f.store.evidence_get(stored.id.0).unwrap().is_some());
        // Reading the degraded record through the store is loud too.
        assert!(matches!(
            f.authority.get_scoped(stored.id, &ctx(&f)),
            Err(EvidenceError::Malformed(_))
        ));
    }

    // ------------------------------------------------------------------
    // Quota end-to-end: refusal before aging, freed + admitted after.
    // ------------------------------------------------------------------
    #[test]
    fn aging_frees_the_evidence_cas_quota_and_admits_new_evidence() {
        let quota = 6_000u64;
        let f = governed(quota);
        let raw_a = payload(11, 3000);
        let a = insert(&f, "bulky old evidence", &raw_a);
        let digest_a = hex(&a.backing_hash.unwrap());
        let stored_a = stored_bytes(&f, &digest_a);
        assert_eq!(usage(&f), stored_a, "the old body occupies its quota");
        let water = f.store.evidence_high_water().unwrap();

        // Admission refusal while the quota is dominated by the old body.
        let raw_b = payload(22, 4000);
        let refused = f
            .authority
            .insert_new(&env(&f, "new evidence"), Some(&raw_b));
        assert!(refused.is_err(), "quota-full admission must refuse");
        assert_eq!(
            f.store.evidence_high_water().unwrap(),
            water,
            "no row for a refused body"
        );
        assert!(f.governor.counters().unwrap().refusals >= 1);

        // Age the old body past its horizon: quota math, exactly.
        let report = f.authority.age_heavy_bodies_at(far_future()).unwrap();
        assert_eq!(report.swept, 1);
        assert_eq!(report.swept_bytes, stored_a);
        assert_eq!(
            usage(&f),
            0,
            "usage_after == usage_before - swept_bytes ({} - {})",
            stored_a,
            report.swept_bytes
        );

        // The same new evidence is admitted now.
        let b = f
            .authority
            .insert_new(&env(&f, "new evidence"), Some(&raw_b))
            .unwrap();
        let digest_b = hex(&b.backing_hash.unwrap());
        let stored_b = stored_bytes(&f, &digest_b);
        assert!(stored_b <= quota, "admitted body fits the quota");
        assert_eq!(usage(&f), stored_b);

        // The sweep is fully accounted: loss marker + verified eviction row.
        let markers = markers(&f);
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].0, digest_a);
        assert_eq!(markers[0].1, stored_a);
        assert_eq!(markers[0].2, EVIDENCE_AGED_LOSS_REASON);
        assert!(f.governor.counters().unwrap().loss_markers >= 1);
        assert!(
            f.governor
                .eviction_index()
                .unwrap()
                .iter()
                .any(|(class, key, bytes, verified)| {
                    class == "evidence_cas" && key == &digest_a && *bytes == stored_a && *verified
                }),
            "the reclaimed accounting row names the swept digest, bytes and verification"
        );

        // The new (within-horizon) body is protected like any other lease.
        let _ = f.governor.enforce().unwrap();
        let alive = f.authority.get_scoped(b.id, &ctx(&f)).unwrap();
        assert_eq!(alive.backing.as_deref(), Some(&raw_b[..]));
    }

    // ------------------------------------------------------------------
    // Crash seams: every mid-aging crash reconciles exactly once.
    // ------------------------------------------------------------------
    #[test]
    fn crash_before_the_first_aging_mutation_is_a_clean_retry() {
        let f = governed(100_000);
        let raw = payload(31, 1024);
        let stored = insert(&f, "pre-crash body", &raw);
        let digest = hex(&stored.backing_hash.unwrap());
        let bytes = stored_bytes(&f, &digest);

        f.authority.crash_arm(EvidenceCrashArm {
            point: "evidence_body_aged_pre",
            ordinal: 0,
        });
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            f.authority.age_heavy_bodies_at(far_future())
        }));
        assert!(crashed.is_err(), "the armed seam must fire");
        // Nothing mutated before the first boundary.
        assert!(f.cas.has(FileHash::from_hex(&digest).unwrap()));
        assert!(f
            .governor
            .is_pinned(StorageClass::EvidenceCas, &digest)
            .unwrap());
        assert!(markers(&f).is_empty());
        assert_eq!(usage(&f), bytes);

        // The seam is one-shot: a retry completes the pass.
        let report = f.authority.age_heavy_bodies_at(far_future()).unwrap();
        assert_eq!(report.swept, 1);
        assert_eq!(report.swept_bytes, bytes);
        assert_eq!(markers(&f).len(), 1);
        assert_eq!(usage(&f), 0);
    }

    #[test]
    fn crash_mid_aging_reconciles_at_every_boundary_exactly_once() {
        let f = governed(100_000);
        let raw_a = payload(32, 1024);
        let a = insert(&f, "body A", &raw_a);
        let digest_a = hex(&a.backing_hash.unwrap());
        let bytes_a = stored_bytes(&f, &digest_a);
        let envelope_a = f.authority.get_scoped(a.id, &ctx(&f)).unwrap().envelope;

        // Crash AFTER the bytes left, BEFORE the loss marker.
        f.authority.crash_arm(EvidenceCrashArm {
            point: "evidence_body_aged_deleted",
            ordinal: 0,
        });
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            f.authority.age_heavy_bodies_at(far_future())
        }));
        assert!(crashed.is_err());
        assert!(
            !f.cas.has(FileHash::from_hex(&digest_a).unwrap()),
            "bytes left"
        );
        assert!(markers(&f).is_empty(), "marker not yet written");
        assert_eq!(usage(&f), bytes_a, "accounting row is the breadcrumb");
        assert!(f
            .governor
            .is_pinned(StorageClass::EvidenceCas, &digest_a)
            .unwrap());
        assert!(
            f.store.evidence_get(a.id.0).unwrap().is_some(),
            "proof row survives"
        );

        // Crash AFTER the marker, BEFORE the accounting removal.
        let raw_b = payload(34, 1024);
        let b = insert(&f, "body B", &raw_b);
        let digest_b = hex(&b.backing_hash.unwrap());
        let bytes_b = stored_bytes(&f, &digest_b);
        let envelope_b = f.authority.get_scoped(b.id, &ctx(&f)).unwrap().envelope;
        f.authority.crash_arm(EvidenceCrashArm {
            point: "evidence_body_aged_marked",
            ordinal: 0,
        });
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            f.authority.age_heavy_bodies_at(far_future())
        }));
        assert!(crashed.is_err());
        // Pass one reconciled A fully; B stopped after its marker.
        assert!(!f.cas.has(FileHash::from_hex(&digest_b).unwrap()));
        let marker_rows = markers(&f);
        assert_eq!(
            marker_rows.len(),
            2,
            "A reconciled, B crashed after its marker"
        );
        assert!(marker_rows.iter().any(|(key, _, _)| key == &digest_a));
        assert!(marker_rows.iter().any(|(key, _, _)| key == &digest_b));
        assert_eq!(usage(&f), bytes_b, "B's accounting survived its crash");
        assert!(f
            .governor
            .is_pinned(StorageClass::EvidenceCas, &digest_b)
            .unwrap());

        // Reconciling pass: everything completes, nothing double counts.
        let report = f.authority.age_heavy_bodies_at(far_future()).unwrap();
        assert_eq!(report.swept, 0, "no bytes remain to delete");
        assert_eq!(report.reconciled, 1, "B's remainder completes");
        assert_eq!(report.already_absent, 1, "A is idempotent residue");
        assert_eq!(usage(&f), 0, "quota reclaimed exactly once");
        let marker_rows = markers(&f);
        assert_eq!(
            marker_rows.len(),
            2,
            "one loss marker per body, never doubled"
        );
        assert_eq!(f.governor.counters().unwrap().loss_markers, 2);
        assert!(!f
            .governor
            .is_pinned(StorageClass::EvidenceCas, &digest_a)
            .unwrap());
        assert!(!f
            .governor
            .is_pinned(StorageClass::EvidenceCas, &digest_b)
            .unwrap());

        // Proof identity intact for both, digest-only continuity preserved.
        let after_a = f.authority.get_scoped(a.id, &ctx(&f)).unwrap();
        assert!(after_a.backing.is_none());
        assert_eq!(after_a.envelope, envelope_a);
        let after_b = f.authority.get_scoped(b.id, &ctx(&f)).unwrap();
        assert!(after_b.backing.is_none());
        assert_eq!(after_b.envelope, envelope_b);

        // Third pass: fully inert.
        let report = f.authority.age_heavy_bodies_at(far_future()).unwrap();
        assert_eq!(report.swept, 0);
        assert_eq!(report.reconciled, 0);
        assert_eq!(report.already_absent, 2);
        assert_eq!(f.governor.counters().unwrap().loss_markers, 2);
    }

    // ------------------------------------------------------------------
    // Shared (deduplicated) digests: a body is only swept when EVERY
    // referencer is beyond the horizon — and then exactly once.
    // ------------------------------------------------------------------
    #[test]
    fn shared_digest_waits_for_every_referencer_then_sweeps_once() {
        let f = governed(100_000);
        let raw = payload(33, 1024);
        let digest = FileHash::from(blake3::hash(&raw).into()).to_hex();
        let old = insert(&f, "shared old", &raw);
        let old_row = f.store.evidence_get(old.id.0).unwrap().unwrap();
        let horizon = DEFAULT_REPLAY_HORIZON_MS;
        let bytes = stored_bytes(&f, &digest);

        // A second referencer of the SAME digest, young at the first eval.
        let young_id = f
            .store
            .evidence_insert(&crafted_row(
                0,
                &env(&f, "shared young"),
                old_row.created_ms + 1_000 * horizon,
                &digest,
            ))
            .unwrap();

        // Old referencer beyond horizon, young referencer within: nothing
        // may be deleted from under the young proof.
        let report = f
            .authority
            .age_heavy_bodies_at(old_row.created_ms + 2 * horizon)
            .unwrap();
        assert_eq!(
            report.swept, 0,
            "a within-horizon referencer blocks the sweep"
        );
        assert_eq!(report.shared_within_horizon, 1);
        assert_eq!(report.within_horizon_rows, 1);
        assert!(f.cas.has(FileHash::from_hex(&digest).unwrap()));
        assert!(markers(&f).is_empty());
        assert_eq!(usage(&f), bytes);

        // Both referencers beyond horizon: ONE sweep, one marker, both
        // proof rows intact (dedup shares the body; aging shares the loss).
        let report = f
            .authority
            .age_heavy_bodies_at(old_row.created_ms + 1_002 * horizon)
            .unwrap();
        assert_eq!(report.swept, 1, "the shared digest is swept exactly once");
        assert_eq!(report.swept_bytes, bytes);
        assert_eq!(markers(&f).len(), 1);
        assert_eq!(usage(&f), 0);
        assert!(!f.cas.has(FileHash::from_hex(&digest).unwrap()));

        let after_old = f.authority.get_scoped(old.id, &ctx(&f)).unwrap();
        assert!(after_old.backing.is_none());
        assert_eq!(
            after_old.envelope.backing_hash,
            Some(FileHash::from_hex(&digest).unwrap().bytes())
        );
        let after_young = f
            .authority
            .get_scoped(EvidenceId(young_id), &ctx(&f))
            .unwrap();
        assert!(after_young.backing.is_none());
        assert_eq!(after_young.envelope.compact.body, "shared young");
        assert_eq!(
            after_young.envelope.backing_hash,
            Some(FileHash::from_hex(&digest).unwrap().bytes())
        );

        // Idempotence at the same evaluation point.
        let report = f
            .authority
            .age_heavy_bodies_at(old_row.created_ms + 1_002 * horizon)
            .unwrap();
        assert_eq!(report.swept, 0);
        assert_eq!(report.reconciled, 0);
        assert_eq!(report.already_absent, 1);
        assert_eq!(markers(&f).len(), 1);
    }

    // ------------------------------------------------------------------
    // Config: serde defaults are backward compatible; invalid refuses;
    // disabled is fully inert.
    // ------------------------------------------------------------------
    #[test]
    fn aging_config_serde_defaults_are_backward_compatible_and_invalid_horizons_refuse() {
        let default = EvidenceAgingConfig::default();
        assert!(
            default.enabled,
            "safe default: aging ON with a real horizon"
        );
        assert_eq!(default.replay_horizon_ms, DEFAULT_REPLAY_HORIZON_MS);

        // A config written BEFORE this feature (no section, no keys) must
        // deserialize to exactly the safe defaults.
        let legacy: EvidenceAgingConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy, default, "missing keys must take the safe defaults");
        let partial: EvidenceAgingConfig =
            serde_json::from_str(r#"{"replay_horizon_ms": 42}"#).unwrap();
        assert!(partial.enabled, "omitted `enabled` defaults to ON");
        assert_eq!(partial.replay_horizon_ms, 42);
        let round: EvidenceAgingConfig =
            serde_json::from_str(&serde_json::to_string(&partial).unwrap()).unwrap();
        assert_eq!(round, partial);

        for bad in [0, -1, i64::MIN] {
            let cfg = EvidenceAgingConfig {
                replay_horizon_ms: bad,
                ..EvidenceAgingConfig::default()
            };
            let err = cfg.validated().unwrap_err();
            assert!(matches!(err, EvidenceError::Refused(_)), "{err:?}");
            assert!(err.to_string().contains("horizon"), "{err}");
        }

        // A nonsensical installed config is surfaced typed by BOTH entry
        // points — never silently coerced, never half-applied.
        let f = governed_with(
            100_000,
            EvidenceAgingConfig {
                replay_horizon_ms: 0,
                ..EvidenceAgingConfig::default()
            },
        );
        let raw = payload(41, 64);
        let stored = insert(&f, "untouchable", &raw);
        let digest = hex(&stored.backing_hash.unwrap());
        let err = f.authority.age_heavy_bodies_at(far_future()).unwrap_err();
        assert!(matches!(err, EvidenceError::Refused(_)), "{err:?}");
        let err = f.authority.aging_status_at(far_future()).unwrap_err();
        assert!(matches!(err, EvidenceError::Refused(_)), "{err:?}");
        assert!(
            f.cas.has(FileHash::from_hex(&digest).unwrap()),
            "nothing touched"
        );
        assert!(markers(&f).is_empty());
    }

    #[test]
    fn disabled_aging_is_a_fully_inert_noop() {
        let f = governed_with(
            100_000,
            EvidenceAgingConfig {
                enabled: false,
                replay_horizon_ms: 1,
            },
        );
        let raw = payload(42, 512);
        let stored = insert(&f, "protected by disable", &raw);
        let digest = hex(&stored.backing_hash.unwrap());
        let bytes = stored_bytes(&f, &digest);

        let report = f.authority.age_heavy_bodies_at(far_future()).unwrap();
        assert!(report.disabled);
        assert_eq!(report.swept, 0);
        assert_eq!(report.scanned_rows, 0, "a disabled pass does not even scan");
        assert!(f.cas.has(FileHash::from_hex(&digest).unwrap()));
        assert!(f
            .governor
            .is_pinned(StorageClass::EvidenceCas, &digest)
            .unwrap());
        assert!(markers(&f).is_empty());
        assert_eq!(usage(&f), bytes);

        // The read-only plan still shows what an enabled pass WOULD do —
        // visibility without action.
        let status = f.authority.aging_status_at(far_future()).unwrap();
        assert!(!status.config.enabled);
        assert_eq!(
            status.evictable_bodies, 1,
            "plan is informative while disabled"
        );
        assert_eq!(status.evictable_bytes, bytes);
        assert!(
            f.cas.has(FileHash::from_hex(&digest).unwrap()),
            "the plan never mutates"
        );
    }

    // ------------------------------------------------------------------
    // Fail closed without a governor; hostile durable state is typed.
    // ------------------------------------------------------------------
    #[test]
    fn aging_without_a_governor_fails_closed() {
        let f = bare();
        let raw = payload(51, 256);
        let stored = insert(&f, "ungoverned body", &raw);
        let digest = hex(&stored.backing_hash.unwrap());

        let err = f.authority.age_heavy_bodies_at(far_future()).unwrap_err();
        assert!(matches!(err, EvidenceError::Refused(_)), "{err:?}");
        assert!(err.to_string().contains("governor"), "{err}");
        assert!(
            f.cas.has(FileHash::from_hex(&digest).unwrap()),
            "bytes must never leave an unaccounted store"
        );
        assert!(markers(&f).is_empty(), "no loss ever happened here");
        assert_eq!(usage(&f), 0, "the ungoverned CAS never charged the ledger");
        assert!(f.store.evidence_get(stored.id.0).unwrap().is_some());

        // Status stays honest: governed=false ⇒ nothing evictable.
        let status = f.authority.aging_status_at(far_future()).unwrap();
        assert!(!status.governed);
        assert_eq!(status.evictable_bodies, 0);
        assert_eq!(status.scanned_rows, 1);

        // Claims need the governor ledger too — typed, not invented.
        let err = f.authority.pin_proof_identity(stored.id).unwrap_err();
        assert!(matches!(err, EvidenceError::Refused(_)), "{err:?}");
        let err = f.authority.pin_backing_for_recovery(stored.id).unwrap_err();
        assert!(matches!(err, EvidenceError::Refused(_)), "{err:?}");
    }

    #[test]
    fn hostile_backing_digest_fails_typed_before_any_mutation() {
        let f = governed(100_000);
        let raw = payload(61, 128);
        let h = f.cas.put(&raw).unwrap();
        let hostile = "zz".repeat(32);
        f.store
            .evidence_insert(&crafted_row(0, &env(&f, "hostile row"), 1, &hostile))
            .unwrap();

        let err = f.authority.age_heavy_bodies_at(far_future()).unwrap_err();
        assert!(matches!(err, EvidenceError::Malformed(_)), "{err:?}");
        assert!(err.to_string().contains("backing digest"), "{err}");

        // Nothing was mutated on the way to the typed failure.
        assert!(markers(&f).is_empty());
        assert!(f.cas.has(h));
        assert_eq!(f.store.evidence_high_water().unwrap(), 1);
    }

    #[test]
    fn aging_scan_is_bounded_and_reports_truncation() {
        let f = governed(100_000);
        let raw = payload(71, 256);
        let h = f.cas.put(&raw).unwrap();
        // One ancient row JUST outside the bounded window.
        let beyond_window = AGING_SCAN_BOUND + 1;
        f.store
            .evidence_insert(&crafted_row(
                beyond_window,
                &env(&f, "outside the window"),
                1,
                &h.to_hex(),
            ))
            .unwrap();
        assert_eq!(f.store.evidence_high_water().unwrap(), beyond_window);

        let report = f.authority.age_heavy_bodies_at(far_future()).unwrap();
        assert!(
            report.scan_truncated,
            "the window is smaller than the table"
        );
        assert_eq!(
            report.scanned_rows, 0,
            "only ids 1..=AGING_SCAN_BOUND are scanned"
        );
        assert_eq!(report.swept, 0);
        assert!(
            f.cas.has(h),
            "rows outside the bounded window are never touched"
        );
        let status = f.authority.aging_status_at(far_future()).unwrap();
        assert!(status.scan_truncated, "the plan reports the same bound");
    }

    // ------------------------------------------------------------------
    // Status: a faithful, non-mutating plan that the pass then obeys.
    // ------------------------------------------------------------------
    #[test]
    fn aging_status_is_a_faithful_dry_run_plan() {
        let f = governed(100_000);
        let raw = payload(81, 512);
        let h = f.cas.put(&raw).unwrap();
        let digest = h.to_hex();
        let bytes = stored_bytes(&f, &digest);
        // Order matters under the write-path cadence: the authority insert
        // stamps (and may run) the opportunistic pass, so the FRESH body
        // goes in first while no ancient row exists. The crafted ancient
        // rows below use the store-level insert, which the cadence does not
        // hook — production sweep-on-due stays intact, and this dry-run
        // fixture keeps its "ancient rows still present" premise.
        let fresh_payload = payload(82, 200);
        let fresh = insert(&f, "fresh body", &fresh_payload);
        let fresh_bytes = stored_bytes(&f, &hex(&fresh.backing_hash.unwrap()));
        let rid1 = f
            .store
            .evidence_insert(&crafted_row(0, &env(&f, "ancient 1"), 1, &digest))
            .unwrap();
        let rid2 = f
            .store
            .evidence_insert(&crafted_row(0, &env(&f, "ancient 2"), 1, &digest))
            .unwrap();
        let now = now_ms();

        let status = f.authority.aging_status_at(now).unwrap();
        assert_eq!(status.config, f.authority.aging_config());
        assert!(status.governed);
        assert_eq!(status.scanned_rows, 3);
        assert_eq!(
            status.within_horizon_rows, 1,
            "the fresh row is inside its horizon"
        );
        assert_eq!(status.evictable_bodies, 1, "one digest would be swept");
        assert_eq!(status.evictable_bytes, bytes);
        assert_eq!(status.recovery_refused_bodies, 0);
        assert_eq!(status.shared_blocked_bodies, 0);

        // The dry run really is dry.
        assert!(f.cas.has(h));
        assert!(markers(&f).is_empty());
        assert_eq!(usage(&f), bytes + fresh_bytes);

        // A recovery pin flips the plan without mutating anything.
        f.authority
            .pin_backing_for_recovery(EvidenceId(rid1))
            .unwrap();
        let status = f.authority.aging_status_at(now).unwrap();
        assert_eq!(status.evictable_bodies, 0);
        assert_eq!(status.recovery_refused_bodies, 1);

        // The real pass obeys the plan exactly.
        let report = f.authority.age_heavy_bodies_at(now).unwrap();
        assert_eq!(report.swept, 0);
        assert_eq!(report.recovery_refused, 1);
        assert!(f.cas.has(h));
        assert!(markers(&f).is_empty());

        // Release → plan and pass agree again.
        f.authority
            .unpin_backing_for_recovery(EvidenceId(rid1))
            .unwrap();
        let status = f.authority.aging_status_at(now).unwrap();
        assert_eq!(status.evictable_bodies, 1);
        assert_eq!(status.evictable_bytes, bytes);
        let report = f.authority.age_heavy_bodies_at(now).unwrap();
        assert_eq!(report.swept, 1);
        assert_eq!(
            usage(&f),
            fresh_bytes,
            "only the fresh body's quota remains"
        );

        // Post-sweep: the plan shows zero evictable, residue classified,
        // and BOTH ancient proof rows are still intact and visible.
        let status = f.authority.aging_status_at(now).unwrap();
        assert_eq!(status.evictable_bodies, 0);
        assert_eq!(status.already_absent_bodies, 1);
        assert_eq!(
            status.within_horizon_rows, 1,
            "the fresh row still never evicts"
        );
        assert!(f
            .authority
            .get_scoped(EvidenceId(rid1), &ctx(&f))
            .unwrap()
            .backing
            .is_none());
        assert!(f
            .authority
            .get_scoped(EvidenceId(rid2), &ctx(&f))
            .unwrap()
            .backing
            .is_none());
    }

    // ------------------------------------------------------------------
    // Write-path cadence (production wiring, audit item 12): opportunistic,
    // bounded, non-blocking, and inert when disabled.
    // ------------------------------------------------------------------
    #[test]
    fn write_path_insert_stamps_the_cadence_and_skips_within_the_interval() {
        let f = governed(100_000);
        assert_eq!(
            f.authority.aging_last_pass_ms(),
            None,
            "a fresh authority never aged"
        );
        let raw = payload(71, 512);
        let a = insert(&f, "fresh A", &raw);
        let stamped = f
            .authority
            .aging_last_pass_ms()
            .expect("the first insert stamps the cadence");
        // The second insert lands within the hour: it is a plain write.
        let b = insert(&f, "fresh B", &raw);
        assert_eq!(
            f.authority.aging_last_pass_ms().unwrap(),
            stamped,
            "a within-interval insert must not re-run the pass"
        );
        // Nothing beyond the horizon existed, so nothing was touched.
        assert!(f
            .cas
            .has(FileHash::from_hex(&hex(&a.backing_hash.unwrap())).unwrap()));
        assert!(f
            .cas
            .has(FileHash::from_hex(&hex(&b.backing_hash.unwrap())).unwrap()));
        assert!(markers(&f).is_empty());
        assert!(usage(&f) > 0);
    }

    #[test]
    fn if_due_sweeps_a_stale_body_exactly_once_and_then_quietens() {
        let f = governed(100_000);
        let raw = payload(72, 2048);
        let body = insert(&f, "stale body", &raw);
        let digest = hex(&body.backing_hash.unwrap());
        let bytes = stored_bytes(&f, &digest);
        assert!(f.cas.has(FileHash::from_hex(&digest).unwrap()));
        assert_eq!(usage(&f), bytes, "the body is accounted before aging");

        // A due pass (authority never stamped; clock beyond the horizon)
        // reclaims the body through the SAME sweep contract as an explicit
        // pass: bytes gone, loss marker written, quota freed.
        f.authority
            .aging_last_ms
            .store(0, std::sync::atomic::Ordering::Relaxed);
        let ran = f
            .authority
            .age_heavy_bodies_if_due_at(far_future(), 1)
            .unwrap();
        assert!(ran, "the due pass reclaimed something");
        assert!(!f.cas.has(FileHash::from_hex(&digest).unwrap()));
        assert_eq!(markers(&f).len(), 1, "exactly-once loss marker");
        assert_eq!(usage(&f), 0, "quota reclaimed");
        // The proof row itself is permanent.
        let kept = f.authority.get_scoped(body.id, &ctx(&f)).unwrap();
        assert!(kept.backing.is_none(), "digest-only continuity");
        assert_eq!(kept.envelope.backing_hash.as_ref().map(hex), Some(digest));

        // Immediately after: within the interval → skipped, NOT a second sweep.
        let again = f
            .authority
            .age_heavy_bodies_if_due_at(far_future(), 1)
            .unwrap();
        assert!(!again, "stamped by the first pass; no re-run");
        assert_eq!(markers(&f).len(), 1, "marker count pinned");
    }

    #[test]
    fn if_due_is_non_blocking_and_refuses_a_nonsensical_cadence() {
        let f = governed(100_000);
        // Held gate: a concurrent pass makes this a skip, never a queue.
        let gate = f.authority.aging_gate.clone();
        let held = gate.lock().unwrap();
        let ran = f
            .authority
            .age_heavy_bodies_if_due(AGING_WRITE_CADENCE_MS)
            .unwrap();
        assert!(!ran, "a held gate must skip, not block");
        assert_eq!(
            f.authority.aging_last_pass_ms(),
            None,
            "a skipped call must not stamp"
        );
        drop(held);
        // Free gate: the first due pass runs (and stamps) even with nothing
        // to sweep.
        let _ = f
            .authority
            .age_heavy_bodies_if_due(AGING_WRITE_CADENCE_MS)
            .unwrap();
        assert!(f.authority.aging_last_pass_ms().is_some());
        // Cadence <= 0 is a typed refusal, never a silent clamp.
        match f.authority.age_heavy_bodies_if_due(0) {
            Err(EvidenceError::Refused(msg)) => assert!(msg.contains("cadence"), "{msg}"),
            other => panic!("expected a typed cadence refusal, got {other:?}"),
        }
    }

    #[test]
    fn disabled_config_makes_the_write_path_pass_inert() {
        let f = governed_with(
            100_000,
            EvidenceAgingConfig {
                enabled: false,
                ..EvidenceAgingConfig::default()
            },
        );
        let raw = payload(73, 1024);
        let body = insert(&f, "aged but disabled", &raw);
        let digest = hex(&body.backing_hash.unwrap());
        assert!(f.cas.has(FileHash::from_hex(&digest).unwrap()));

        // A fully due pass under the disabled config: no scan, no sweep, no
        // marker — but the cadence still stamps so later writes skip cheaply.
        let ran = f
            .authority
            .age_heavy_bodies_if_due_at(far_future(), 1)
            .unwrap();
        assert!(!ran, "disabled aging reclaims nothing");
        assert!(f.cas.has(FileHash::from_hex(&digest).unwrap()));
        assert!(markers(&f).is_empty());
        assert_eq!(usage(&f), stored_bytes(&f, &digest));
        assert!(
            f.authority.aging_last_pass_ms().is_some(),
            "the inert pass still stamps the cadence"
        );
    }
}
