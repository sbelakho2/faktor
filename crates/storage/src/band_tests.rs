// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! Recovery-band tests (the documented three-band split over the effective
//! aggregate budget): normal admission ≤85%, emergency/critical metadata
//! (SQLite/WAL growth) ≤90%, RECOVERY-ONLY reserve 90–100%.
//!
//! Every test breaks the boundary it protects: normal admission crossing
//! into the emergency band must refuse typed, metadata growth crossing into
//! the recovery reserve must refuse typed, and recovery admissions must
//! still succeed at 100% normal pressure while the hard budget and the
//! volume free-space reserve still hold.

use std::collections::BTreeMap;

use crate::class::StorageClass;
use crate::config::StorageGovernorConfig;
use crate::error::RefusalReason;
use crate::governor::StorageGovernor;

/// One million bytes: the default bands are 850k/900k, giving each band a
/// width larger than the 16 KiB SQLite growth bound so metadata admission
/// can be probed inside a band without crossing the next one.
const TOTAL: u64 = 1_000_000;
const NORMAL_CEILING: u64 = 850_000;

fn band_config() -> StorageGovernorConfig {
    let quotas = StorageClass::ALL
        .into_iter()
        .map(|class| (class, TOTAL))
        .collect::<BTreeMap<_, _>>();
    StorageGovernorConfig {
        total_budget_bytes: TOTAL,
        watermark_permille: 1000,
        reservation_lease_ms: 60_000,
        quotas,
        // Hermetic against HOST free space: the reserve path still executes
        // (deficit math, typed FreeSpaceReserve refusals), but at the
        // clamped minimum (1% of capacity, 2 GiB floor) instead of the
        // production 5% — these tests probe PURPOSE bands, not the
        // operator's volume pressure. The production default is exercised
        // by the store/governor suites with their own configs.
        reserve_permille: 1,
        reserve_bytes: 0,
        ..Default::default()
    }
    .validated()
    .unwrap()
}

/// Charge `bytes` to `class` accounting without touching disk (the
/// governor's ledger is the durable truth the bands are enforced against).
fn fill(governor: &StorageGovernor, class: StorageClass, key: &str, bytes: u64) {
    governor
        .reserve_recovery(class, key, bytes)
        .unwrap()
        .commit(bytes, None)
        .unwrap();
}

#[test]
fn normal_admission_stays_in_the_normal_band_and_refuses_typed_at_the_default_ceiling() {
    let dir = tempfile::tempdir().unwrap();
    let governor = StorageGovernor::open(dir.path(), band_config()).unwrap();
    let capacity = governor.capacity_snapshot();
    assert!(capacity.measured, "a tempdir volume is measurable");
    // Documented defaults hold over the EFFECTIVE budget.
    let metrics = governor.metrics().unwrap();
    assert_eq!(
        metrics.normal_admission_ceiling_bytes,
        capacity.effective_budget_bytes * 850 / 1_000,
        "normal band default is 850 permille of the effective budget"
    );
    assert_eq!(
        metrics.emergency_ceiling_bytes,
        capacity.effective_budget_bytes * 900 / 1_000,
        "emergency band default is 900 permille of the effective budget"
    );
    assert_eq!(
        metrics.recovery_reserve_bytes,
        metrics.effective_budget_bytes - metrics.emergency_ceiling_bytes,
        "the recovery reserve is the band above the emergency ceiling"
    );

    // Fill to exactly the normal ceiling with NORMAL admissions.
    let step = 25_000u64;
    let mut used = 0u64;
    for index in 0..(NORMAL_CEILING / step) {
        fill(
            &governor,
            StorageClass::MainCas,
            &format!("normal-{index}"),
            step,
        );
        used += step;
    }
    assert_eq!(used, NORMAL_CEILING);
    let metrics = governor.metrics().unwrap();
    assert_eq!(metrics.admission_band, "normal");
    assert_eq!(
        governor.health().unwrap(),
        None,
        "the band is policy, not a fault"
    );

    // One byte of NORMAL admission into the EMERGENCY band must refuse
    // typed — the hard budget still has 150k left, so this can never
    // degrade into a generic budget refusal.
    let refusal = governor
        .reserve(StorageClass::MainCas, "normal-into-band", 1)
        .expect_err("normal admission must stop at the 85% band ceiling")
        .refusal()
        .cloned()
        .expect("typed refusal");
    assert_eq!(refusal.reason, RefusalReason::NormalAdmissionCeiling);
    assert_eq!(refusal.requested_bytes, 1);
    assert!(
        governor.counters().unwrap().refusals >= 1,
        "the band refusal is durable"
    );
    let degraded = governor
        .health()
        .unwrap()
        .expect("the refused band admission degrades health");
    assert!(
        degraded.code.contains("normal_admission_ceiling"),
        "the degraded health names the band refusal: {}",
        degraded.code
    );

    // The refusal left NO reservation behind (never a leaked hold).
    assert_eq!(governor.pending_reservations().unwrap(), 0);
}

#[test]
fn emergency_metadata_grows_inside_its_band_and_stops_at_the_recovery_reserve() {
    let dir = tempfile::tempdir().unwrap();
    let governor = StorageGovernor::open(dir.path(), band_config()).unwrap();

    // Cross the normal ceiling through the RECOVERY path (proving that
    // path works too), landing usage inside the emergency band.
    fill(&governor, StorageClass::MainCas, "fill-a", 880_000);
    let metrics = governor.metrics().unwrap();
    assert_eq!(metrics.admission_band, "emergency");

    // NORMAL admission is already refused here...
    let band_refusal = governor
        .reserve(StorageClass::MainCas, "normal-refused", 1_000)
        .expect_err("usage sits above the normal ceiling")
        .refusal()
        .cloned()
        .expect("typed refusal");
    assert_eq!(band_refusal.reason, RefusalReason::NormalAdmissionCeiling);

    // ...but EMERGENCY/critical metadata (SQLite/WAL growth, bounded below)
    // is admitted: 880k + growth bound ≤ 900k emergency ceiling.
    governor
        .admit_sqlite_growth(100)
        .expect("metadata growth is admitted inside the emergency band");

    // Cross into the recovery-only reserve (still via recovery admission).
    fill(&governor, StorageClass::MainCas, "fill-b", 50_000);
    assert_eq!(
        governor.metrics().unwrap().admission_band,
        "recovery_reserve",
        "usage above the emergency ceiling is the recovery-only band"
    );

    // Metadata growth can NEVER enter the recovery-only reserve: the
    // admission is refused typed against the emergency ceiling.
    let metadata_refusal = governor
        .admit_sqlite_growth(100)
        .expect_err("ongoing ledger growth must not consume recovery bytes")
        .refusal()
        .cloned()
        .expect("typed refusal");
    assert_eq!(metadata_refusal.reason, RefusalReason::AggregateBudget);
    assert!(
        metadata_refusal.detail.contains("emergency ceiling"),
        "the refusal detail names the band: {}",
        metadata_refusal.detail
    );
}

#[test]
fn recovery_writes_succeed_at_full_normal_pressure_and_the_hard_budget_still_holds() {
    let dir = tempfile::tempdir().unwrap();
    let governor = StorageGovernor::open(dir.path(), band_config()).unwrap();
    // Normal pressure: fill to just below the normal ceiling.
    fill(
        &governor,
        StorageClass::MainCas,
        "pressure",
        NORMAL_CEILING - 10_000,
    );
    // Normal admission refuses into the reserve band...
    governor
        .reserve(StorageClass::MainCas, "normal-blocked", 100_000)
        .expect_err("normal admission can never consume the recovery reserve");

    // ...while RECOVERY admissions keep working all the way to 100% of the
    // hard budget: 840k + 160k = exactly the total.
    governor
        .reserve_recovery(StorageClass::MainCas, "recovery-1", 100_000)
        .expect("recovery admission enters the reserve band")
        .commit(100_000, None)
        .unwrap();
    governor
        .reserve_recovery(StorageClass::MainCas, "recovery-2", 60_000)
        .expect("recovery admission reaches exactly 100% of the budget")
        .commit(60_000, None)
        .unwrap();
    let metrics = governor.metrics().unwrap();
    assert_eq!(metrics.total_bytes, TOTAL, "the budget is exactly spent");
    assert_eq!(metrics.admission_band, "recovery_reserve");

    // Beyond 100%: even RECOVERY is refused typed against the hard budget.
    let refusal = governor
        .reserve_recovery(StorageClass::EvidenceCas, "recovery-over", 1)
        .expect_err("no purpose may exceed the hard aggregate budget")
        .refusal()
        .cloned()
        .expect("typed refusal");
    assert_eq!(refusal.reason, RefusalReason::AggregateBudget);
    assert_eq!(governor.pending_reservations().unwrap(), 0);

    // And a NORMAL admission stays refused at full pressure.
    governor
        .reserve(StorageClass::MainCas, "normal-at-full", 1)
        .expect_err("normal admission never reaches the recovery reserve");
}

#[test]
fn recovery_band_clamps_keep_the_three_bands_ordered_and_nonzero() {
    // A hostile configuration (inverted or zeroed bands) is pulled into the
    // documented safe range: bands can never vanish or let normal admission
    // reach 100%.
    let mut config = band_config();
    config.normal_admission_ceiling_permille = 1;
    config.emergency_ceiling_permille = 0;
    let validated = config.validated().unwrap();
    assert!(
        validated.normal_admission_ceiling_permille
            >= crate::config::MIN_NORMAL_ADMISSION_CEILING_PERMILLE,
        "the normal band is clamped, never zeroed"
    );
    assert!(
        validated.emergency_ceiling_permille > validated.normal_admission_ceiling_permille,
        "emergency band must stay strictly above the normal band"
    );
    assert!(
        validated.emergency_ceiling_permille <= 1000,
        "the emergency band never exceeds the whole budget"
    );

    let mut config = band_config();
    config.normal_admission_ceiling_permille = 5000;
    config.emergency_ceiling_permille = 5000;
    let validated = config.validated().unwrap();
    assert!(
        validated.normal_admission_ceiling_permille <= 950,
        "normal admission can never be configured up to the full budget"
    );
    assert!(
        validated.emergency_ceiling_permille > validated.normal_admission_ceiling_permille,
        "a recovery reserve always remains above the emergency band"
    );

    // And the same guards hold through a serialized config round-trip
    // (a persisted config is re-validated on every open).
    let mut config = band_config();
    config.normal_admission_ceiling_permille = 950;
    config.emergency_ceiling_permille = 951;
    let validated = config.validated().unwrap();
    let raw = serde_json::to_string(&validated).unwrap();
    let reloaded: StorageGovernorConfig = serde_json::from_str(&raw).unwrap();
    assert_eq!(reloaded, validated, "config round-trip is lossless");
    let dir = tempfile::tempdir().unwrap();
    assert!(StorageGovernor::open(dir.path(), reloaded).is_ok());
}

// ---------------------------------------------------------------------
// Purpose-aware SQLite admission (audit item 14 follow-up).
//
// The store writer's SQLite/WAL growth used to be admitted ONLY in the
// emergency band, so crash-reconciliation materialization could not commit
// once aggregate usage was above the 90% emergency ceiling. These tests pin
// the purpose-aware contract: an explicit RECOVERY purpose reaches the
// 90-100% reserve with the same accounting/refusal semantics, ordinary
// (emergency/normal) purposes still stop at their own ceilings, and the
// hard budget still refuses beyond 100%.
// ---------------------------------------------------------------------

#[test]
fn recovery_sqlite_admission_enters_the_recovery_band_and_ordinary_metadata_still_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let governor = StorageGovernor::open(dir.path(), band_config()).unwrap();

    // 95% pressure: inside the recovery-only reserve, above the emergency
    // ceiling (900k).
    fill(
        &governor,
        StorageClass::EvidenceCas,
        "sqlite-band-pressure",
        TOTAL * 95 / 100,
    );
    assert_eq!(
        governor.metrics().unwrap().admission_band,
        "recovery_reserve"
    );

    // Ordinary metadata growth keeps the historical emergency band: typed
    // refusal naming the band, with no recovery powers.
    let emergency = governor
        .admit_sqlite_growth(100)
        .expect_err("emergency metadata must never consume the recovery reserve")
        .refusal()
        .cloned()
        .expect("typed refusal");
    assert_eq!(emergency.reason, RefusalReason::AggregateBudget);
    assert!(
        emergency.detail.contains("emergency ceiling"),
        "ordinary refusal names the emergency band: {}",
        emergency.detail
    );

    // The explicit recovery path admits into the reserve.
    assert_eq!(governor.sqlite_admitted_bytes(), 0);
    let admission = governor
        .admit_sqlite_recovery_growth_tracked(100)
        .expect("recovery-class SQLite growth is admitted in the 90-100% band");
    let charged = governor.sqlite_admitted_bytes();
    assert!(charged > 0, "the recovery admission is tracked as debt");
    // Explicit accounting release: dropping the guard clears the in-flight
    // half; the next durable scan reconciles the completed half to disk
    // truth. Nothing was written, so no usage may remain charged.
    drop(admission);
    assert_eq!(governor.pending_reservations().unwrap(), 0);
    governor.sqlite_maintenance().unwrap();
    assert_eq!(
        governor.sqlite_admitted_bytes(),
        0,
        "dropped-before-commit admission is never charged as durable usage"
    );
    let sqlite_bytes = governor
        .metrics()
        .unwrap()
        .per_class
        .get("sqlite_wal")
        .map(|row| row.bytes)
        .unwrap_or(0);
    assert_eq!(
        sqlite_bytes, 0,
        "an admission with no committed row must not become usage"
    );

    // Beyond 100% even RECOVERY refuses typed: no unbounded bypass.
    let remaining = TOTAL - governor.metrics().unwrap().total_bytes;
    if remaining > 0 {
        fill(
            &governor,
            StorageClass::EvidenceCas,
            "sqlite-band-full",
            remaining,
        );
    }
    assert_eq!(governor.metrics().unwrap().total_bytes, TOTAL);
    let over = governor
        .admit_sqlite_recovery_growth_tracked(100)
        .expect_err("no purpose may exceed the hard aggregate budget")
        .refusal()
        .cloned()
        .expect("typed refusal");
    assert_eq!(over.reason, RefusalReason::AggregateBudget);
    assert!(
        over.detail.contains("recovery ceiling"),
        "the recovery refusal names its band: {}",
        over.detail
    );
}

#[test]
fn sqlite_purpose_is_per_call_and_a_concurrent_caller_cannot_flip_it() {
    use crate::governor::AdmissionPurpose;
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let governor = Arc::new(StorageGovernor::open(dir.path(), band_config()).unwrap());
    fill(
        &governor,
        StorageClass::EvidenceCas,
        "concurrent-pressure",
        TOTAL * 95 / 100,
    );

    // Interleave ordinary (emergency/normal) and recovery admissions from
    // many threads. The purpose is an explicit argument of EACH call, so the
    // outcome is determined by the call itself, never by another thread's
    // choice or a shared mode.
    let mut handles = Vec::new();
    let mut recovery_admitted = 0usize;
    for index in 0..8 {
        let governor = Arc::clone(&governor);
        handles.push(std::thread::spawn(move || {
            if index % 2 == 0 {
                (
                    true,
                    governor
                        .admit_sqlite_growth_tracked_for(AdmissionPurpose::EmergencyMetadata, 100)
                        .map(|_| ())
                        .map_err(|e| format!("{e:?}")),
                )
            } else {
                (
                    false,
                    governor
                        .admit_sqlite_recovery_growth_tracked(100)
                        .map(|_| ())
                        .map_err(|e| format!("{e:?}")),
                )
            }
        }));
    }
    for handle in handles {
        let (ordinary, outcome) = handle.join().unwrap();
        if ordinary {
            assert!(
                outcome.is_err(),
                "an ordinary SQLite call must refuse at recovery-band pressure"
            );
        } else {
            // Every recovery outcome is either an admission or a TYPED
            // refusal (never a panic, never silent). Under a 4-way storm at
            // the band edge the room check conservatively counts a sibling
            // thread's pre-marked in-flight bound alongside admitted debt,
            // so an individual call can transiently refuse even though its
            // purpose band allows it — safe-by-conservatism (never
            // over-admits); the recovery sweep retries idempotently. The
            // exact per-call semantics are pinned sequentially below.
            match &outcome {
                Ok(()) => {}
                Err(detail) => assert!(
                    detail.contains("recovery ceiling"),
                    "a recovery refusal must name the recovery band: {detail}"
                ),
            }
        }
        if outcome.is_ok() {
            recovery_admitted += 1;
        }
    }
    assert!(
        recovery_admitted >= 1,
        "the recovery band must admit under recovery-band pressure (band difference exists)"
    );
    assert_eq!(governor.pending_reservations().unwrap(), 0);

    // The same call sequence reversed stays honest: recovery still admits,
    // ordinary still refuses (no leaked "last purpose" state).
    assert!(governor.admit_sqlite_recovery_growth_tracked(100).is_ok());
    assert!(governor.admit_sqlite_growth(100).is_err());
}
