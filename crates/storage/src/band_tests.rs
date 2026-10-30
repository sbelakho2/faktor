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
