use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::class::StorageClass;
use crate::config::StorageGovernorConfig;
use crate::directory::DirectoryBackend;
use crate::error::{GovernorError, RefusalReason};
use crate::governor::StorageGovernor;
use crate::object::{EvictionOutcome, StorageBackend, StorageObject};
use tempfile::TempDir;

const SMALL_TOTAL: u64 = 120_000;
const SMALL_CLASS: u64 = 20_000;

fn config(total: u64, class: u64, lease_ms: i64) -> StorageGovernorConfig {
    let quotas = StorageClass::ALL
        .into_iter()
        .map(|storage_class| (storage_class, class))
        .collect::<BTreeMap<_, _>>();
    StorageGovernorConfig {
        total_budget_bytes: total,
        watermark_permille: 900,
        reservation_lease_ms: lease_ms,
        quotas,
    }
    .validated()
    .unwrap()
}

struct Fixture {
    dir: TempDir,
    roots: BTreeMap<StorageClass, PathBuf>,
}

impl Fixture {
    fn new(classes: &[StorageClass]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut roots = BTreeMap::new();
        for class in classes {
            let root = dir.path().join(class.as_str());
            std::fs::create_dir_all(&root).unwrap();
            roots.insert(*class, root);
        }
        Self { dir, roots }
    }

    fn governor(&self, config: StorageGovernorConfig) -> StorageGovernor {
        let governor = StorageGovernor::open(self.dir.path(), config).unwrap();
        for (class, root) in &self.roots {
            governor
                .register_backend(Arc::new(DirectoryBackend::new(*class, root.clone())))
                .unwrap();
        }
        governor
    }

    fn publish(&self, governor: &StorageGovernor, class: StorageClass, key: &str, bytes: u64) {
        let reservation = governor.reserve(class, key, bytes).unwrap();
        let payload = vec![b'x'; bytes as usize];
        std::fs::write(self.roots[&class].join(key), &payload).unwrap();
        reservation.commit(bytes, None).unwrap();
    }
}

#[test]
fn flood_each_class_stays_within_aggregate_and_class_quotas() {
    let fixture = Fixture::new(&StorageClass::ALL);
    let governor = fixture.governor(config(SMALL_TOTAL, SMALL_CLASS, 60_000));
    let mut refusals = 0u64;
    for class in StorageClass::ALL {
        for index in 0..60u32 {
            let key = format!("object-{index}");
            match governor.reserve(class, &key, 4_000) {
                Ok(reservation) => {
                    let payload = vec![0u8; 4_000];
                    std::fs::write(fixture.roots[&class].join(&key), &payload).unwrap();
                    reservation.commit(4_000, None).unwrap();
                }
                Err(GovernorError::Refused(refusal)) => {
                    assert_eq!(refusal.class, class);
                    refusals += 1;
                }
                Err(other) => panic!("unexpected governor failure: {other}"),
            }
        }
        // One object larger than the class quota can never be admitted, no
        // matter how much other history is evicted.
        match governor.reserve(class, "oversized", SMALL_CLASS + 1) {
            Err(GovernorError::Refused(refusal)) => {
                assert_eq!(refusal.reason, RefusalReason::ClassQuota);
                refusals += 1;
            }
            other => panic!("over-quota object must be refused, got {other:?}"),
        }
    }
    let metrics = governor.metrics().unwrap();
    assert!(
        metrics.total_bytes <= SMALL_TOTAL,
        "hard aggregate budget breached: {} > {}",
        metrics.total_bytes,
        SMALL_TOTAL
    );
    for class in StorageClass::ALL {
        let usage = &metrics.per_class[class.as_str()];
        assert!(
            usage.bytes <= SMALL_CLASS,
            "class {} breached its quota: {} > {}",
            class.as_str(),
            usage.bytes,
            SMALL_CLASS
        );
    }
    assert!(metrics.evictions > 0, "flooding must evict");
    assert!(refusals > 0, "an over-quota flood must refuse");
    assert!(
        metrics.refusals >= refusals,
        "every typed refusal is durably counted"
    );
    governor.reconcile().unwrap();
    let reconciled = governor.metrics().unwrap();
    assert!(reconciled.total_bytes <= SMALL_TOTAL);
}

#[test]
fn budget_refusal_is_enforced_before_publish() {
    let fixture = Fixture::new(&[StorageClass::MainCas]);
    let governor = fixture.governor(config(10_000, 1_000, 60_000));
    let error = governor
        .reserve(StorageClass::MainCas, "oversized", 1_001)
        .unwrap_err();
    let refusal = error.refusal().expect("typed refusal");
    assert_eq!(refusal.reason, RefusalReason::ClassQuota);
    assert_eq!(refusal.requested_bytes, 1_001);
    assert_eq!(governor.counters().unwrap().refusals, 1);
    assert_eq!(governor.metrics().unwrap().total_bytes, 0);
    assert!(governor.health().unwrap().is_some(), "budget cannot be met");
}

#[test]
fn budget_refusal_guard_is_pinned_against_mutation() {
    let fixture = Fixture::new(&[StorageClass::MainCas]);
    let governor = fixture.governor(config(10_000, 1_000, 60_000));
    // Fill the class quota with REQUIRED bytes: no eviction can make room,
    // so an object that alone fits the quota must still be refused. A mutant
    // that drops the admission comparison would admit it and publish a
    // reservation/object past the hard budget.
    fixture.publish(&governor, StorageClass::MainCas, "required", 1_000);
    governor
        .pin(StorageClass::MainCas, "required", "required")
        .unwrap();
    let refused = governor.reserve(StorageClass::MainCas, "over-admission", 500);
    let refusal = refused
        .expect_err("pinned bytes leave no admissible room")
        .refusal()
        .cloned()
        .expect("typed refusal");
    assert_eq!(refusal.reason, RefusalReason::RequiredBytes);
    assert_eq!(governor.pending_reservations().unwrap(), 0);
    assert_eq!(governor.metrics().unwrap().total_bytes, 1_000);
}

#[test]
fn pinned_and_required_objects_are_never_evicted() {
    let fixture = Fixture::new(&[StorageClass::EvidenceCas]);
    let governor = fixture.governor(config(40_000, 10_000, 60_000));
    fixture.publish(&governor, StorageClass::EvidenceCas, "required", 4_000);
    governor
        .pin(StorageClass::EvidenceCas, "required", "evidence")
        .unwrap();
    for index in 0..20 {
        if let Ok(reservation) =
            governor.reserve(StorageClass::EvidenceCas, &format!("filler-{index}"), 1_500)
        {
            let payload = vec![0u8; 1_500];
            std::fs::write(
                fixture.roots[&StorageClass::EvidenceCas].join(format!("filler-{index}")),
                &payload,
            )
            .unwrap();
            reservation.commit(1_500, None).unwrap();
        }
    }
    assert_eq!(
        std::fs::read(fixture.roots[&StorageClass::EvidenceCas].join("required")).unwrap(),
        vec![b'x'; 4_000],
        "required evidence backing must survive every eviction pass"
    );
    assert!(governor
        .is_pinned(StorageClass::EvidenceCas, "required")
        .unwrap());
    let metrics = governor.metrics().unwrap();
    assert!(metrics.per_class["evidence_cas"].pinned_bytes >= 4_000);
}

#[test]
fn restart_reconciles_accounting_from_disk() {
    let fixture = Fixture::new(&[StorageClass::MainCas]);
    let config = config(1_000_000, 500_000, 60_000);
    {
        let governor = fixture.governor(config.clone());
        fixture.publish(&governor, StorageClass::MainCas, "indexed", 2_000);
    }
    // A crash can leave an unindexed blob; reconciliation counts the disk,
    // not the stale ledger.
    std::fs::write(
        fixture.roots[&StorageClass::MainCas].join("orphan"),
        vec![0u8; 7_000],
    )
    .unwrap();
    let governor = fixture.governor(config);
    let report = governor.reconcile().unwrap();
    assert_eq!(report.scanned, 2);
    assert_eq!(report.total_bytes, 9_000);
    let usage = governor.metrics().unwrap();
    assert_eq!(usage.per_class["main_cas"].bytes, 9_000);
    assert_eq!(usage.per_class["main_cas"].objects, 2);
}

#[test]
fn crash_safe_reservation_survives_reopen_and_then_expires() {
    let fixture = Fixture::new(&[StorageClass::MainCas]);
    let config = config(1_000_000, 500_000, 50);
    {
        let governor = fixture.governor(config.clone());
        let reservation = governor
            .reserve(StorageClass::MainCas, "crashed-publish", 5_000)
            .unwrap();
        assert_eq!(governor.pending_reservations().unwrap(), 1);
        // Simulate a process death between reserve and publish: the RAII
        // abort must NOT run.
        std::mem::forget(reservation);
    }
    let governor = fixture.governor(config);
    assert_eq!(
        governor.pending_reservations().unwrap(),
        1,
        "the durable reservation row outlives the crashed process"
    );
    std::thread::sleep(Duration::from_millis(70));
    let report = governor.reconcile().unwrap();
    assert!(
        report.expired_reservations >= 1,
        "a stale reservation is expired, not counted forever"
    );
    assert_eq!(governor.pending_reservations().unwrap(), 0);
    assert_eq!(governor.metrics().unwrap().total_bytes, 0);
}

#[test]
fn archive_eviction_writes_a_durable_loss_marker() {
    let fixture = Fixture::new(&[StorageClass::CompactionArchive]);
    let governor = fixture.governor(config(20_000, 10_000, 60_000));
    for index in 0..3 {
        fixture.publish(
            &governor,
            StorageClass::CompactionArchive,
            &format!("chunk-{index}"),
            4_000,
        );
    }
    let reservation = governor
        .reserve(StorageClass::CompactionArchive, "chunk-new", 8_000)
        .unwrap();
    std::fs::write(
        fixture.roots[&StorageClass::CompactionArchive].join("chunk-new"),
        vec![0u8; 8_000],
    )
    .unwrap();
    reservation.commit(8_000, None).unwrap();
    let markers = governor
        .loss_markers(Some(StorageClass::CompactionArchive))
        .unwrap();
    assert!(
        !markers.is_empty(),
        "evicting an archive object must leave a durable loss marker"
    );
    assert_eq!(markers[0].2, "archive_evicted");
    assert!(markers[0].1 > 0);
    assert!(governor.metrics().unwrap().loss_markers >= 1);
}

#[test]
fn archive_loss_marker_guard_is_pinned_against_mutation() {
    let fixture = Fixture::new(&[StorageClass::CompactionArchive]);
    let governor = fixture.governor(config(10_000, 8_000, 60_000));
    fixture.publish(&governor, StorageClass::CompactionArchive, "old", 3_000);
    fixture.publish(&governor, StorageClass::CompactionArchive, "old-2", 3_000);
    let reservation = governor
        .reserve(StorageClass::CompactionArchive, "new", 5_000)
        .unwrap();
    std::fs::write(
        fixture.roots[&StorageClass::CompactionArchive].join("new"),
        vec![0u8; 5_000],
    )
    .unwrap();
    reservation.commit(5_000, None).unwrap();
    let evictions = governor.eviction_index().unwrap();
    assert!(
        evictions
            .iter()
            .any(|(class, _, _, _)| class == "compaction_archive"),
        "the eviction index names the evicted archive object"
    );
    assert!(!governor.loss_markers(None).unwrap().is_empty());
}

#[test]
fn sqlite_wal_is_checkpointed_truncated_and_growth_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    std::fs::create_dir_all(&store).unwrap();
    let db_path = store.join("faktor-plus.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA wal_autocheckpoint = 0;
         CREATE TABLE blob(id INTEGER PRIMARY KEY, body BLOB NOT NULL);",
    )
    .unwrap();
    let body = vec![0u8; 300_000];
    for _ in 0..8 {
        conn.execute(
            "INSERT INTO blob(body) VALUES (?1)",
            rusqlite::params![body],
        )
        .unwrap();
    }
    let wal = store.join("faktor-plus.db-wal");
    let before = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
    assert!(before > 0, "the probe must create a real WAL");
    let current =
        std::fs::metadata(&db_path).unwrap().len() + std::fs::metadata(&wal).unwrap().len();
    let config = config(64 * 1024 * 1024, current + 400_000, 60_000);
    let governor = StorageGovernor::open(dir.path(), config).unwrap();
    let reclaimed = governor.sqlite_maintenance().unwrap();
    let after = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
    assert!(
        after < before || reclaimed > 0,
        "a WAL above the class quota must be checkpoint-truncated (before {before}, after {after})"
    );
    // Growth larger than the remaining class room is refused BEFORE the
    // append: the typed refusal is durable and health is degraded.
    let error = governor.admit_sqlite_growth(10_000_000).unwrap_err();
    let refusal = error.refusal().expect("typed growth refusal");
    assert_eq!(refusal.class, StorageClass::SqliteWal);
    assert!(governor.counters().unwrap().refusals >= 1);
    assert!(governor
        .health()
        .unwrap()
        .is_some_and(|health| health.class == Some(StorageClass::SqliteWal)));
    drop(conn);
}

struct FakeEvictBackend {
    class: StorageClass,
}

impl StorageBackend for FakeEvictBackend {
    fn class(&self) -> StorageClass {
        self.class
    }

    fn scan(&self, _governor: &StorageGovernor) -> Result<Vec<StorageObject>, GovernorError> {
        Ok(Vec::new())
    }

    fn evict(
        &self,
        _governor: &StorageGovernor,
        object: &StorageObject,
    ) -> Result<EvictionOutcome, GovernorError> {
        Ok(EvictionOutcome::Evicted {
            bytes: object.bytes,
            verified: false,
            digest: None,
        })
    }
}

#[test]
fn eviction_at_100k_objects_uses_bounded_keyset_queries_not_quadratic_scans() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageGovernorConfig {
        total_budget_bytes: 1_000_000_000,
        watermark_permille: 900,
        reservation_lease_ms: 60_000,
        quotas: StorageClass::ALL
            .into_iter()
            .map(|class| {
                let quota = if class == StorageClass::RebuildableCache {
                    99_000
                } else {
                    500_000
                };
                (class, quota)
            })
            .collect(),
    };
    let governor = StorageGovernor::open(dir.path(), config).unwrap();
    governor
        .register_backend(Arc::new(FakeEvictBackend {
            class: StorageClass::RebuildableCache,
        }))
        .unwrap();
    let start = Instant::now();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    // 100k one-byte objects, 1k over the class quota. Reserving 1 KiB
    // evicts ~2k of them through keyset pages of 64 (~32 queries), never
    // one full scan/sort per object.
    let objects: Vec<(String, u64, i64)> = (0..100_000)
        .map(|index| (format!("object-{index:06}"), 1u64, now + index as i64))
        .collect();
    governor
        .replace_inventory(StorageClass::RebuildableCache, &objects)
        .unwrap();
    let indexed = start.elapsed();
    let before = governor.eviction_query_count();
    let reservation = governor
        .reserve(StorageClass::RebuildableCache, "new-object", 1_024)
        .unwrap();
    reservation.abort();
    let queries = governor.eviction_query_count() - before;
    assert!(
        queries <= 64,
        "eviction used {queries} queries for 1 024 objects; must stay bounded"
    );
    assert!(
        indexed < Duration::from_secs(20),
        "indexing 100k objects took {indexed:?}"
    );
    assert!(queries > 0);
}

#[test]
fn commit_may_not_exceed_its_own_admission_and_maintenance_is_durable() {
    let fixture = Fixture::new(&[StorageClass::MainCas]);
    let governor = fixture.governor(config(100_000, 20_000, 60_000));
    assert_eq!(governor.last_maintenance_ms().unwrap(), None);
    assert_eq!(governor.reconcile().unwrap().total_bytes, 0);
    assert!(
        governor.last_maintenance_ms().unwrap().is_some(),
        "reconcile records a durable last-maintenance stamp"
    );
    assert!(governor.metrics().unwrap().last_maintenance_ms.is_some());
    let reservation = governor
        .reserve(StorageClass::MainCas, "over-commit", 1_000)
        .unwrap();
    let error = reservation.commit(1_001, None).unwrap_err();
    assert!(error.to_string().contains("may never exceed"), "{error}");
    assert_eq!(governor.metrics().unwrap().total_bytes, 0);
    assert_eq!(
        governor.pending_reservations().unwrap(),
        0,
        "the invalid commit aborts its reservation"
    );
    governor
        .reserve(StorageClass::MainCas, "bounded", 1_000)
        .unwrap()
        .commit(1_000, None)
        .unwrap();
    assert_eq!(governor.metrics().unwrap().total_bytes, 1_000);
}

#[test]
fn update_remnants_are_cleaned_but_fresh_staging_is_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let staging = dir.path().join("install").join("staging").join("op-1");
    std::fs::create_dir_all(&staging).unwrap();
    let stale = staging.join("old-artifact");
    std::fs::write(&stale, vec![0u8; 4_096]).unwrap();
    let old = std::time::SystemTime::now() - Duration::from_secs(2 * 60 * 60);
    filetime_set(&stale, old);
    let fresh = staging.join("fresh-artifact");
    std::fs::write(&fresh, vec![0u8; 2_048]).unwrap();
    let config = config(1_000_000, 500_000, 60_000);
    let governor = StorageGovernor::open(dir.path(), config).unwrap();
    let removed = governor.cleanup_update_remnants(60 * 60 * 1000).unwrap();
    assert_eq!(removed, 4_096);
    assert!(!stale.exists());
    assert!(fresh.exists(), "an in-flight staging file is preserved");
}

fn filetime_set(path: &Path, time: std::time::SystemTime) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(time).unwrap();
}
