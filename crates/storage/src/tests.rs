// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
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
        ..Default::default()
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

/// A checkpoint that cannot truncate (a reader pins an old frame) must not
/// be reported as an eviction: the WAL row and its bytes stay accounted, and
/// the second disk-truth scan keeps the class usage at the real size.
#[test]
fn wal_checkpoint_busy_never_fakes_reclaimed_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    std::fs::create_dir_all(&store).unwrap();
    let db_path = store.join("busy.db");
    let writer = rusqlite::Connection::open(&db_path).unwrap();
    writer
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA wal_autocheckpoint = 0;
             CREATE TABLE blob(id INTEGER PRIMARY KEY, body BLOB NOT NULL);
             INSERT INTO blob(body) VALUES (randomblob(180000));
             INSERT INTO blob(body) VALUES (randomblob(180000));",
        )
        .unwrap();
    let wal_path = store.join("busy.db-wal");
    // Pin the current snapshot, then grow the WAL past it.
    let reader = rusqlite::Connection::open(&db_path).unwrap();
    reader
        .execute_batch("BEGIN; SELECT COUNT(*) FROM blob;")
        .unwrap();
    for _ in 0..5 {
        writer
            .execute("INSERT INTO blob(body) VALUES (randomblob(180000))", [])
            .unwrap();
    }
    let wal_before = std::fs::metadata(&wal_path).unwrap().len();
    let db_before = std::fs::metadata(&db_path).unwrap().len();
    assert!(wal_before > 0);
    let quota = db_before + wal_before + 2_000_000;
    let governor = StorageGovernor::open(
        dir.path(),
        config_with(
            quota + 2_000_000,
            1_000_000,
            &[(StorageClass::SqliteWal, quota)],
        ),
    )
    .unwrap();
    let _ = governor.sqlite_maintenance().unwrap();
    let used = governor.metrics().unwrap().per_class["sqlite_wal"].bytes;
    assert!(
        used >= db_before + wal_before,
        "busy checkpoint fabricated free room: ledger {used} < real {}",
        db_before + wal_before
    );
    assert_eq!(
        governor.counters().unwrap().evictions,
        0,
        "a busy checkpoint is a refusal, never an eviction"
    );
    assert!(governor
        .eviction_index()
        .unwrap()
        .iter()
        .all(|(_, key, _, _)| !key.ends_with(".db-wal")));
    reader.execute_batch("ROLLBACK").unwrap();
    drop(reader);
    let reclaimed = governor.sqlite_maintenance().unwrap();
    let wal_after = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
    assert!(
        reclaimed > 0 || wal_after < wal_before,
        "after the reader released, the WAL must truncate"
    );
}

/// SQLite admission debt is shared by every governor instance of one ledger
/// and an in-flight tracked write is never discounted by a scan.
#[test]
fn sqlite_admission_debt_is_shared_and_inflight_safe() {
    let dir = tempfile::tempdir().unwrap();
    let first = StorageGovernor::open(dir.path(), config(100_000_000, 50_000_000, 60_000)).unwrap();
    let second = StorageGovernor::open_existing(dir.path()).unwrap();
    let admission = first.admit_sqlite_growth_tracked(10_000).unwrap();
    let admitted = first.sqlite_admitted_bytes();
    assert!(admitted > 0);
    assert_eq!(
        second.sqlite_admitted_bytes(),
        admitted,
        "a second instance must spend the same admitted room"
    );
    first.reconcile().unwrap();
    assert_eq!(
        first.sqlite_admitted_bytes(),
        admitted,
        "an in-flight write may not be discounted by a scan"
    );
    drop(admission);
    first.reconcile().unwrap();
    assert_eq!(
        first.sqlite_admitted_bytes(),
        0,
        "a completed admission reconciled from disk truth is released"
    );
    assert_eq!(second.sqlite_admitted_bytes(), 0);
}

/// Directory roots are class state, not instance state: a root registered by
/// the browser download manager survives a reopen and is scanned by a
/// governor that only knows the defaults.
#[test]
fn directory_roots_persist_across_governor_instances() {
    let data = tempfile::tempdir().unwrap();
    let custom = data.path().join("operator").join("browser-profiles");
    std::fs::create_dir_all(&custom).unwrap();
    std::fs::write(custom.join("Cookies"), vec![3u8; 4_096]).unwrap();
    {
        let governor =
            StorageGovernor::open(data.path(), config(1_000_000, 500_000, 60_000)).unwrap();
        governor
            .add_directory_root(StorageClass::BrowserStorage, custom.clone(), 0)
            .unwrap();
    }
    let reopened = StorageGovernor::open(data.path(), config(1_000_000, 500_000, 60_000)).unwrap();
    let roots = reopened
        .backend(StorageClass::BrowserStorage)
        .unwrap()
        .directory_roots()
        .expect("directory backend")
        .0
        .to_vec();
    assert!(roots.contains(&custom), "persisted roots: {roots:?}");
    reopened.reconcile().unwrap();
    let usage = reopened.metrics().unwrap().per_class["browser_storage"].clone();
    assert!(usage.bytes >= 4_096, "{usage:?}");
    assert!(usage.objects >= 1, "{usage:?}");
}

/// A stronger retention claim moves the object's PIN with it. Otherwise the
/// promoted row stays `required=1` while the pin sits under the old class:
/// unpin is a no-op and the object is starved from eviction forever.
#[test]
fn dedup_promotion_moves_the_pin_to_the_new_owner_class() {
    let dir = tempfile::tempdir().unwrap();
    let governor = StorageGovernor::open(dir.path(), config(1_000_000, 500_000, 60_000)).unwrap();
    let digest = "a".repeat(64);
    governor
        .register_object(StorageClass::MainCas, &digest, 1_000, Some(&digest))
        .unwrap();
    governor
        .pin(StorageClass::MainCas, &digest, "evidence")
        .unwrap();
    assert_eq!(
        governor.metrics().unwrap().per_class["main_cas"].pinned_bytes,
        1_000
    );
    governor
        .register_object(
            StorageClass::CompactionArchive,
            &digest,
            1_000,
            Some(&digest),
        )
        .unwrap();
    let metrics = governor.metrics().unwrap();
    assert_eq!(metrics.per_class["main_cas"].bytes, 0);
    assert_eq!(metrics.per_class["main_cas"].pinned_bytes, 0);
    assert_eq!(
        metrics.per_class["compaction_archive"].pinned_bytes, 1_000,
        "the pin must move with the promoted owner"
    );
    assert!(governor
        .is_pinned(StorageClass::CompactionArchive, &digest)
        .unwrap());
    governor
        .unpin(StorageClass::CompactionArchive, &digest)
        .unwrap();
    assert_eq!(
        governor.metrics().unwrap().per_class["compaction_archive"].pinned_bytes,
        0
    );
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

struct ErrorOnceBackend {
    class: StorageClass,
    calls: std::sync::atomic::AtomicU64,
}

impl StorageBackend for ErrorOnceBackend {
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
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            return Err(GovernorError::Backend("corrupt blob".into()));
        }
        Ok(EvictionOutcome::Evicted {
            bytes: object.bytes,
            verified: false,
            digest: None,
        })
    }
}

/// One unverifiable/corrupt object must not abort the whole eviction pass:
/// the pass records a refusal, skips it, and still frees the next object so
/// the hard class quota can be met.
#[test]
fn one_corrupt_object_does_not_abort_the_eviction_pass() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageGovernorConfig {
        total_budget_bytes: 1_000_000,
        watermark_permille: 900,
        reservation_lease_ms: 60_000,
        quotas: StorageClass::ALL
            .into_iter()
            .map(|class| {
                (
                    class,
                    if class == StorageClass::RebuildableCache {
                        10_000
                    } else {
                        200_000
                    },
                )
            })
            .collect(),
        ..Default::default()
    };
    let governor = StorageGovernor::open(dir.path(), config).unwrap();
    governor
        .register_backend(Arc::new(ErrorOnceBackend {
            class: StorageClass::RebuildableCache,
            calls: std::sync::atomic::AtomicU64::new(0),
        }))
        .unwrap();
    let now = crate::ledger::now_ms();
    governor
        .replace_inventory(
            StorageClass::RebuildableCache,
            &[
                ("corrupt".to_string(), 6_000, now - 2),
                ("healthy".to_string(), 6_000, now - 1),
            ],
        )
        .unwrap();
    let reservation = governor
        .reserve(StorageClass::RebuildableCache, "fresh", 1_000)
        .expect("the healthy candidate must make room");
    drop(reservation);
    assert!(governor.counters().unwrap().refusals >= 1);
    assert!(
        governor.metrics().unwrap().per_class["rebuildable_cache"].bytes <= 10_000,
        "the pass must continue after the corrupt object"
    );
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
        ..Default::default()
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
    eprintln!("perf: indexing 100k objects = {indexed:?}");
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
fn directory_key_is_canonical_and_survives_reconcile_and_eviction() {
    // Regression: a writer publishing into one of the SEVERAL default
    // browser-profile roots must use the backend's canonical key
    // (`<root-index>:<rel>`). Booking the bare root-relative path instead
    // indexes an object `scan` never re-reports: the row is orphaned on
    // reconcile and the bytes can never be evicted.
    let dir = tempfile::tempdir().unwrap();
    let governor =
        StorageGovernor::open(dir.path(), config(SMALL_TOTAL, SMALL_CLASS, 60_000)).unwrap();

    let roots = crate::governor::GovernorRoots::under(dir.path());
    let root = roots.browser_profiles[0].clone();
    assert!(
        roots.browser_profiles.len() > 1,
        "must be a multi-root class"
    );
    std::fs::create_dir_all(root.join("p1").join("downloads")).unwrap();

    let rel = PathBuf::from("p1").join("downloads").join("file.pdf");
    let key = governor
        .directory_key(StorageClass::BrowserStorage, &root, &rel)
        .expect("the configured root must resolve to a canonical key");
    assert_eq!(key, "0:p1/downloads/file.pdf");

    // A root this backend does not own has no canonical key.
    let foreign = dir.path().join("not-a-root");
    std::fs::create_dir_all(&foreign).unwrap();
    assert_eq!(
        governor.directory_key(StorageClass::BrowserStorage, &foreign, &rel),
        None
    );

    // Publish under the canonical key, then prove reconcile and eviction.
    let bytes = 4_096u64;
    let reservation = governor
        .reserve(StorageClass::BrowserStorage, &key, bytes)
        .unwrap();
    std::fs::write(root.join(&rel), vec![b'x'; bytes as usize]).unwrap();
    reservation.commit(bytes, None).unwrap();

    let report = governor.reconcile().unwrap();
    assert_eq!(report.removed, 0, "no row may be reconciled away");
    assert!(governor.metrics().unwrap().total_bytes >= bytes);

    assert_eq!(
        governor
            .evict_object(StorageClass::BrowserStorage, &key)
            .unwrap(),
        bytes
    );
    assert!(!root.join(&rel).exists(), "eviction removes the file");
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

fn config_with(
    total: u64,
    defaults: u64,
    overrides: &[(StorageClass, u64)],
) -> StorageGovernorConfig {
    let mut quotas = StorageClass::ALL
        .into_iter()
        .map(|class| (class, defaults))
        .collect::<BTreeMap<_, _>>();
    for (class, bytes) in overrides {
        quotas.insert(*class, *bytes);
    }
    StorageGovernorConfig {
        total_budget_bytes: total,
        watermark_permille: 900,
        reservation_lease_ms: 60_000,
        quotas,
        ..Default::default()
    }
    .validated()
    .unwrap()
}

/// Cross-instance admission must be atomic in SQLite, not only under each
/// process-local mutex: eight independent governor connections race for six
/// slots of 1 500 bytes inside a 10 000 byte budget. The old read-check-insert
/// sequence admitted all eight.
#[test]
fn concurrent_governor_instances_cannot_over_admit_the_hard_budget() {
    use std::sync::Barrier;

    let dir = tempfile::tempdir().unwrap();
    let config = config(10_000, 10_000, 60_000);
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|index| {
            let path = dir.path().to_path_buf();
            let config = config.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let governor = StorageGovernor::open(path, config).unwrap();
                barrier.wait();
                match governor.reserve(StorageClass::MainCas, &format!("race-{index}"), 1_500) {
                    Ok(reservation) => {
                        let bytes = reservation.bytes();
                        std::mem::forget(reservation);
                        bytes
                    }
                    Err(GovernorError::Refused(_)) => 0,
                    Err(error) => panic!("unexpected admission error: {error}"),
                }
            })
        })
        .collect();
    let admitted: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert!(
        admitted <= 10_000,
        "concurrent writers over-admitted the hard budget: {admitted}"
    );
    assert_eq!(admitted, 9_000, "six 1 500-byte reservations fit exactly");
    let governor = StorageGovernor::open(dir.path(), config).unwrap();
    assert_eq!(governor.pending_reservations().unwrap(), 6);
    assert_eq!(governor.metrics().unwrap().total_bytes, 0);
}

#[test]
fn dropped_reservation_is_released_immediately() {
    let fixture = Fixture::new(&[StorageClass::MainCas]);
    let governor = fixture.governor(config(100_000, 50_000, 60_000));
    let reservation = governor
        .reserve(StorageClass::MainCas, "dropped", 8_000)
        .unwrap();
    assert_eq!(governor.pending_reservations().unwrap(), 1);
    drop(reservation);
    assert_eq!(
        governor.pending_reservations().unwrap(),
        0,
        "RAII abort must delete the durable row, not leak admission forever"
    );
    assert_eq!(governor.metrics().unwrap().total_bytes, 0);
}

#[test]
fn required_object_growth_moves_the_pinned_counter() {
    let fixture = Fixture::new(&[StorageClass::MainCas]);
    let governor = fixture.governor(config(100_000, 50_000, 60_000));
    fixture.publish(&governor, StorageClass::MainCas, "pinned", 1_000);
    governor
        .pin(StorageClass::MainCas, "pinned", "proof")
        .unwrap();
    assert_eq!(
        governor.metrics().unwrap().per_class["main_cas"].pinned_bytes,
        1_000
    );
    // The same durable key is re-indexed larger (repair/refresh). Pinned
    // accounting must follow the bytes; the old upsert only moved it on the
    // required transition, silently under-counting required pressure.
    governor
        .register_object(StorageClass::MainCas, "pinned", 5_000, None)
        .unwrap();
    let metrics = governor.metrics().unwrap();
    assert_eq!(metrics.per_class["main_cas"].pinned_bytes, 5_000);
    assert_eq!(metrics.per_class["main_cas"].bytes, 5_000);
}

/// A class quota blocked by REQUIRED bytes must refuse, never evict another
/// class's cold objects to "make room" that the class quota cannot use.
#[test]
fn class_quota_pressure_never_evicts_another_class() {
    let fixture = Fixture::new(&[StorageClass::MainCas, StorageClass::RebuildableCache]);
    let governor = fixture.governor(config_with(
        60_000,
        20_000,
        &[
            (StorageClass::MainCas, 10_000),
            (StorageClass::RebuildableCache, 10_000),
        ],
    ));
    fixture.publish(&governor, StorageClass::MainCas, "required", 9_000);
    governor
        .pin(StorageClass::MainCas, "required", "required")
        .unwrap();
    fixture.publish(
        &governor,
        StorageClass::RebuildableCache,
        "cold-cache",
        5_000,
    );
    let refused = governor
        .reserve(StorageClass::MainCas, "over-class", 5_000)
        .expect_err("pinned bytes leave no class room");
    assert_eq!(
        refused.refusal().map(|r| r.reason.clone()),
        Some(RefusalReason::RequiredBytes)
    );
    assert_eq!(
        governor.counters().unwrap().evictions,
        0,
        "no unrelated object may be evicted against a pure class-quota deficit"
    );
    assert!(
        fixture.roots[&StorageClass::RebuildableCache]
            .join("cold-cache")
            .exists(),
        "another class's cold data must survive a class-local refusal"
    );
}

struct RecordingEvictBackend {
    class: StorageClass,
    order: Arc<std::sync::Mutex<Vec<String>>>,
}

impl StorageBackend for RecordingEvictBackend {
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
        self.order.lock().unwrap().push(object.key.clone());
        Ok(EvictionOutcome::Evicted {
            bytes: object.bytes,
            verified: false,
            digest: None,
        })
    }
}

/// LRU ordering + bounded query count at 100k objects: the oldest keys go
/// first and one admission uses keyset pages, not a full sort per object.
#[test]
fn eviction_at_100k_is_lru_ordered_and_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageGovernorConfig {
        total_budget_bytes: 1_000_000_000,
        watermark_permille: 900,
        reservation_lease_ms: 60_000,
        quotas: StorageClass::ALL
            .into_iter()
            .map(|class| {
                (
                    class,
                    if class == StorageClass::RebuildableCache {
                        99_000
                    } else {
                        500_000
                    },
                )
            })
            .collect(),
        ..Default::default()
    }
    .validated()
    .unwrap();
    let governor = StorageGovernor::open(dir.path(), config).unwrap();
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));
    governor
        .register_backend(Arc::new(RecordingEvictBackend {
            class: StorageClass::RebuildableCache,
            order: order.clone(),
        }))
        .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let objects: Vec<(String, u64, i64)> = (0..100_000)
        .map(|index| (format!("object-{index:06}"), 1u64, now + index as i64))
        .collect();
    governor
        .replace_inventory(StorageClass::RebuildableCache, &objects)
        .unwrap();
    let before = governor.eviction_query_count();
    let started = Instant::now();
    let reservation = governor
        .reserve(StorageClass::RebuildableCache, "new", 2_024)
        .unwrap();
    reservation.abort();
    let elapsed = started.elapsed();
    let queries = governor.eviction_query_count() - before;
    let evicted = order.lock().unwrap().clone();
    assert!(evicted.len() >= 2_000, "evicted {}", evicted.len());
    assert_eq!(evicted[0], "object-000000", "oldest object first");
    assert_eq!(evicted[1], "object-000001");
    assert!(
        evicted.windows(2).all(|pair| pair[0] < pair[1]),
        "eviction order must be strictly oldest-first"
    );
    eprintln!("perf: 100k-object eviction = {elapsed:?}, {queries} keyset queries");
    assert!(
        queries <= 64,
        "100k-object eviction used {queries} candidate queries"
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "evicting ~2k of 100k objects took {elapsed:?}"
    );
}

/// SQLite growth flood: admitting is bounded by durable inventory plus the
/// in-memory admitted-but-unsettled debt; a flood cannot grow the WAL past the
/// class quota between scans, and maintenance runs are rare (not per write).
#[test]
fn sqlite_wal_flood_cannot_exceed_the_class_quota_between_scans() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    std::fs::create_dir_all(&store).unwrap();
    let db_path = store.join("flood.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA wal_autocheckpoint = 0;
         CREATE TABLE blob(id INTEGER PRIMARY KEY, body BLOB NOT NULL);",
    )
    .unwrap();
    let initial_db = std::fs::metadata(&db_path).unwrap().len();
    let quota = initial_db + 1_500_000;
    let config = config_with(
        quota + 2_000_000,
        1_000_000,
        &[(StorageClass::SqliteWal, quota)],
    );
    let governor = StorageGovernor::open(dir.path(), config).unwrap();
    governor.sqlite_maintenance().unwrap();
    let wal_path = store.join("flood.db-wal");
    let body = vec![7u8; 100_000];
    let mut admitted = 0u64;
    let mut refusals = 0u64;
    for _ in 0..50 {
        match governor.admit_sqlite_growth(body.len() as u64) {
            Ok(()) => {
                admitted += body.len() as u64;
                conn.execute(
                    "INSERT INTO blob(body) VALUES (?1)",
                    rusqlite::params![body],
                )
                .unwrap();
                let wal = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
                let used = std::fs::metadata(&db_path).unwrap().len() + wal;
                assert!(
                    used <= quota,
                    "WAL flood exceeded the class quota: {used} > {quota}"
                );
            }
            Err(GovernorError::Refused(_)) => {
                refusals += 1;
            }
            Err(error) => panic!("unexpected admission failure: {error}"),
        }
    }
    assert!(refusals > 0, "a flood must eventually be refused");
    assert!(
        admitted <= quota - initial_db + 100_000,
        "admitted {admitted} bytes beyond the class quota"
    );
    assert!(
        governor.sqlite_maintenance_runs() <= 6,
        "maintenance ran {} times for 50 appends (must be pressure-driven)",
        governor.sqlite_maintenance_runs()
    );
    assert!(governor.counters().unwrap().refusals >= refusals);
    drop(conn);
}

/// Ordinary small appends take the fast path: no WAL scan, no reconcile, no
/// growth refusal, and bounded latency.
#[test]
fn small_append_admission_has_no_scan_overhead() {
    let dir = tempfile::tempdir().unwrap();
    let governor =
        StorageGovernor::open(dir.path(), config(100_000_000, 50_000_000, 60_000)).unwrap();
    let started = Instant::now();
    for _ in 0..500 {
        governor.admit_sqlite_growth(4_096).unwrap();
    }
    let elapsed = started.elapsed();
    assert_eq!(
        governor.sqlite_maintenance_runs(),
        0,
        "small appends must not trigger full WAL/database scans"
    );
    assert_eq!(governor.family_index_loads(), 0);
    assert!(
        elapsed < Duration::from_secs(5),
        "500 small admissions took {elapsed:?}"
    );
}

/// The maintenance tick re-indexes directory classes that grow outside
/// reservations (caches, profiles, agent homes) and enforces quotas.
#[test]
fn maintenance_tick_reindexes_and_evicts_ungoverned_directory_growth() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    for index in 0..5 {
        std::fs::write(cache.join(format!("entry-{index}")), vec![0u8; 10_000]).unwrap();
        filetime_set(
            &cache.join(format!("entry-{index}")),
            std::time::SystemTime::now() - Duration::from_secs(3_600 - index as u64),
        );
    }
    let config = config_with(
        500_000,
        100_000,
        &[(StorageClass::RebuildableCache, 20_000)],
    );
    let governor = StorageGovernor::open(dir.path(), config).unwrap();
    governor
        .register_backend(Arc::new(DirectoryBackend::new(
            StorageClass::RebuildableCache,
            cache.clone(),
        )))
        .unwrap();
    // A tick with zero intervals forces both reconcile and enforce.
    assert!(governor.maintain_if_due(0, 0).unwrap());
    let usage = governor.metrics().unwrap().per_class["rebuildable_cache"].clone();
    assert!(
        usage.bytes <= 20_000,
        "directory quota not enforced: {usage:?}"
    );
    let remaining = std::fs::read_dir(&cache).unwrap().count();
    assert!(remaining <= 2, "oldest cache entries were not evicted");
    assert!(!cache.join("entry-0").exists());
}

#[test]
fn adding_a_directory_root_merges_instead_of_replacing_governance() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first");
    let second = dir.path().join("second");
    std::fs::create_dir_all(&first).unwrap();
    std::fs::create_dir_all(&second).unwrap();
    std::fs::write(first.join("a"), b"a").unwrap();
    std::fs::write(second.join("b"), b"b").unwrap();
    let governor = StorageGovernor::open(dir.path(), config(1_000_000, 500_000, 60_000)).unwrap();
    // Replace the default updater backend with one explicit root...
    governor
        .register_backend(Arc::new(DirectoryBackend::new(
            StorageClass::UpdateRemnant,
            first.clone(),
        )))
        .unwrap();
    // ...then add the operator-configured root: both must stay governed.
    governor
        .add_directory_root(StorageClass::UpdateRemnant, second.clone(), 0)
        .unwrap();
    let scanned = governor
        .backend(StorageClass::UpdateRemnant)
        .unwrap()
        .scan(&governor)
        .unwrap();
    let keys: Vec<_> = scanned.into_iter().map(|object| object.key).collect();
    assert_eq!(keys, vec!["0:a", "1:b"]);
}

/// A publish that finishes after its lease expired must be adopted into
/// accounting (the bytes are durable), not dropped as "vanished"; the old
/// commit returned Corrupt and left the published blob uncounted until a scan.
#[test]
fn commit_after_lease_expiry_adopts_published_bytes() {
    let fixture = Fixture::new(&[StorageClass::MainCas]);
    let governor = fixture.governor(config(100_000, 50_000, 40));
    let reservation = governor
        .reserve(StorageClass::MainCas, "slow-publish", 6_000)
        .unwrap();
    std::thread::sleep(Duration::from_millis(60));
    // Another writer's decision expires the stale lease.
    governor.reconcile().unwrap();
    assert_eq!(governor.pending_reservations().unwrap(), 0);
    reservation.commit(6_000, None).unwrap();
    let metrics = governor.metrics().unwrap();
    assert_eq!(metrics.total_bytes, 6_000);
    assert_eq!(metrics.per_class["main_cas"].objects, 1);
    assert!(governor.health().unwrap().is_none());
}

/// A reservation left by a CRASHED PROCESS is swept on the next open instead
/// of blocking the budget until its lease expires. Governor instances inside
/// one process share the boot id and keep each other's live reservations.
#[test]
fn restart_sweeps_reservations_from_a_previous_process() {
    let dir = tempfile::tempdir().unwrap();
    let path = StorageGovernor::database_path(dir.path());
    let mut ledger = crate::ledger::Ledger::open(&path).unwrap();
    let now = crate::ledger::now_ms();
    ledger
        .insert_reservation(
            "crashed-publish",
            StorageClass::MainCas,
            "key",
            5_000,
            "dead-process",
            now,
            now + 60_000,
        )
        .unwrap();
    // A DIFFERENT live process sharing the data dir must keep its in-flight
    // reservation: liveness (not merely a different boot id) decides.
    let live_owner = format!("{}-still-running", std::process::id());
    ledger
        .insert_reservation(
            "live-publish",
            StorageClass::MainCas,
            "live-key",
            1_000,
            &live_owner,
            now,
            now + 60_000,
        )
        .unwrap();
    assert_eq!(ledger.pending_count().unwrap(), 2);
    drop(ledger);
    let governor = StorageGovernor::open(dir.path(), config(100_000, 50_000, 60_000)).unwrap();
    assert_eq!(
        governor.pending_reservations().unwrap(),
        1,
        "a dead process's reservation is swept; a live process's survives"
    );
    assert_eq!(governor.metrics().unwrap().total_bytes, 0);
}

/// Watermark eviction has hysteresis: crossing the watermark evicts a batch
/// with headroom instead of one object per subsequent write (thrash).
#[test]
fn watermark_eviction_uses_hysteresis_not_per_write_thrash() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageGovernorConfig {
        total_budget_bytes: 200_000,
        watermark_permille: 900,
        reservation_lease_ms: 60_000,
        quotas: StorageClass::ALL
            .into_iter()
            .map(|class| {
                (
                    class,
                    if class == StorageClass::RebuildableCache {
                        200_000
                    } else {
                        50_000
                    },
                )
            })
            .collect(),
        ..Default::default()
    }
    .validated()
    .unwrap();
    let governor = StorageGovernor::open(dir.path(), config).unwrap();
    governor
        .register_backend(Arc::new(FakeEvictBackend {
            class: StorageClass::RebuildableCache,
        }))
        .unwrap();
    let now = crate::ledger::now_ms();
    let inventory: Vec<(String, u64, i64)> = (0..10)
        .map(|index| (format!("old-{index}"), 10_000u64, now + index as i64))
        .collect();
    governor
        .replace_inventory(StorageClass::RebuildableCache, &inventory)
        .unwrap();
    for index in 0..10 {
        let key = format!("new-{index}");
        governor
            .reserve(StorageClass::RebuildableCache, &key, 10_000)
            .unwrap()
            .commit(10_000, None)
            .unwrap();
    }
    let evictions = governor.counters().unwrap().evictions;
    let evicted_bytes: u64 = governor
        .eviction_index()
        .unwrap()
        .iter()
        .map(|(_, _, bytes, _)| *bytes)
        .sum();
    assert!(
        evictions <= 4,
        "watermark thrash: {evictions} evictions for 10 writes above the watermark"
    );
    assert!(
        evicted_bytes >= 15_000,
        "watermark must evict a batch with headroom, freed only {evicted_bytes} bytes"
    );
}

/// Deterministic simulated backing volume. Paths are matched by longest
/// mounted prefix so a data dir and every root beneath it share one volume
/// unless a test mounts a separate one.
struct SimFs {
    id: u64,
    capacity_bytes: u64,
    available_bytes: std::sync::atomic::AtomicU64,
}

struct SimProbe {
    roots: std::sync::Mutex<Vec<(PathBuf, Arc<SimFs>)>>,
}

impl SimProbe {
    fn new() -> Self {
        Self {
            roots: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn mount(&self, root: &Path, id: u64, capacity_bytes: u64, available_bytes: u64) -> Arc<SimFs> {
        let fs = Arc::new(SimFs {
            id,
            capacity_bytes,
            available_bytes: std::sync::atomic::AtomicU64::new(available_bytes),
        });
        self.roots
            .lock()
            .unwrap()
            .push((root.to_path_buf(), fs.clone()));
        fs
    }

    fn release(&self, fs: &SimFs, bytes: u64) {
        fs.available_bytes
            .fetch_add(bytes, std::sync::atomic::Ordering::AcqRel);
    }

    /// Simulate bytes MATERIALIZING on the volume (a publish): measured free
    /// space drops exactly like a real write, unlike `release` (eviction).
    fn consume(&self, fs: &SimFs, bytes: u64) {
        let _ = fs.available_bytes.fetch_update(
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
            |current| Some(current.saturating_sub(bytes)),
        );
    }

    fn available(&self, fs: &SimFs) -> u64 {
        fs.available_bytes
            .load(std::sync::atomic::Ordering::Acquire)
    }
}

impl crate::capacity::CapacityProbe for SimProbe {
    fn space(&self, path: &Path) -> Option<crate::capacity::FilesystemSpace> {
        let roots = self.roots.lock().unwrap();
        roots
            .iter()
            .filter(|(root, _)| path.starts_with(root))
            .max_by_key(|(root, _)| root.components().count())
            .or_else(|| roots.first())
            .map(|(_, fs)| crate::capacity::FilesystemSpace {
                filesystem_id: fs.id,
                capacity_bytes: fs.capacity_bytes,
                available_bytes: fs
                    .available_bytes
                    .load(std::sync::atomic::Ordering::Acquire),
            })
    }
}

struct CapacityEvictBackend {
    class: StorageClass,
    probe: Arc<SimProbe>,
    fs: Arc<SimFs>,
}

impl StorageBackend for CapacityEvictBackend {
    fn class(&self) -> StorageClass {
        self.class
    }

    fn scan(
        &self,
        _governor: &StorageGovernor,
    ) -> Result<Vec<crate::object::StorageObject>, GovernorError> {
        Ok(Vec::new())
    }

    fn evict(
        &self,
        _governor: &StorageGovernor,
        object: &crate::object::StorageObject,
    ) -> Result<EvictionOutcome, GovernorError> {
        self.probe.release(&self.fs, object.bytes);
        Ok(EvictionOutcome::Evicted {
            bytes: object.bytes,
            verified: false,
            digest: None,
        })
    }
}

fn capacity_config(total: u64) -> StorageGovernorConfig {
    let quotas = StorageClass::ALL
        .into_iter()
        .map(|class| (class, total))
        .collect::<BTreeMap<_, _>>();
    StorageGovernorConfig {
        total_budget_bytes: total,
        quotas,
        ..Default::default()
    }
    .validated()
    .unwrap()
}

const GIB: u64 = 1024 * 1024 * 1024;

/// Mutation witness for the capacity clamp: a 32 GiB volume at 90% use must
/// yield a budget near 5% of capacity (floored by the installation
/// allowance), NOT the operator cap. Dropping the capacity cap (or the
/// capacity-minus-reserve clamp) makes this assertion fail.
#[test]
fn capacity_relative_budget_shrinks_on_a_nearly_full_32gb_disk() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Arc::new(SimProbe::new());
    probe.mount(dir.path(), 1, 32 * GIB, 32 * GIB / 10);
    let governor = StorageGovernor::open_with_capacity_probe(
        dir.path(),
        capacity_config(u64::MAX),
        probe.clone(),
    )
    .unwrap();

    let capacity = governor.capacity_snapshot();
    assert!(capacity.measured);
    assert_eq!(capacity.capacity_bytes, 32 * GIB);
    assert_eq!(capacity.available_bytes, 32 * GIB / 10);
    assert_eq!(capacity.reserve_bytes, 2 * GIB);
    // 5% = 1.6 GiB, raised to the 2 GiB installation allowance.
    assert_eq!(capacity.capacity_cap_bytes, 2 * GIB);
    assert_eq!(capacity.effective_budget_bytes, 2 * GIB);
    assert!(
        capacity.effective_budget_bytes < 32 * GIB / 8,
        "the fixed/unclamped budget would be 32 GiB, not a small capacity budget"
    );
    assert_eq!(capacity.headroom_bytes, 32 * GIB / 10 - 2 * GIB);
}

/// Mutation witness for reserve enforcement: the nominal budget (3.2 GiB)
/// fits the request, but admitting it would leave less than the 3.2 GiB
/// reserve from 6 GiB free. Dropping the reserve check admits it.
#[test]
fn free_space_reserve_is_enforced_even_when_the_nominal_budget_fits() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Arc::new(SimProbe::new());
    probe.mount(dir.path(), 2, 64 * GIB, 6 * GIB);
    let governor = StorageGovernor::open_with_capacity_probe(
        dir.path(),
        capacity_config(u64::MAX),
        probe.clone(),
    )
    .unwrap();

    let capacity = governor.capacity_snapshot();
    assert_eq!(capacity.reserve_bytes, 64 * GIB * 50 / 1000);
    assert_eq!(capacity.headroom_bytes, 6 * GIB - capacity.reserve_bytes);

    let refusal = governor
        .reserve(StorageClass::MainCas, "reserve-breaker", 3 * GIB)
        .expect_err("the reserve must win over nominal room")
        .refusal()
        .cloned()
        .expect("typed refusal");
    assert_eq!(refusal.reason, RefusalReason::FreeSpaceReserve);
    assert_eq!(refusal.available_bytes, capacity.headroom_bytes);
    assert_eq!(
        governor.metrics().unwrap().reserve_bytes,
        capacity.reserve_bytes
    );
}

/// Disk headroom is authoritative: before refusing, the governor reclaims a
/// noncritical cache object, which grows measured free space enough to admit
/// the write above the reserve.
#[test]
fn reserve_pressure_evicts_noncritical_classes_before_refusing() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Arc::new(SimProbe::new());
    let fs = probe.mount(dir.path(), 3, 64 * GIB, 6 * GIB);
    let governor = StorageGovernor::open_with_capacity_probe(
        dir.path(),
        capacity_config(u64::MAX),
        probe.clone(),
    )
    .unwrap();
    governor
        .register_backend(Arc::new(CapacityEvictBackend {
            class: StorageClass::RebuildableCache,
            probe: probe.clone(),
            fs: fs.clone(),
        }))
        .unwrap();
    let now = crate::ledger::now_ms();
    governor
        .replace_inventory(
            StorageClass::RebuildableCache,
            &[("cold-cache".to_string(), GIB / 2, now - 10)],
        )
        .unwrap();

    let reservation = governor
        .reserve(StorageClass::MainCas, "headroom-payload", 3 * GIB)
        .expect("one cache eviction restores the reserve and the headroom");
    reservation.abort();

    assert_eq!(governor.counters().unwrap().evictions, 1);
    assert_eq!(probe.available(&fs), 6 * GIB + GIB / 2);
    assert!(
        governor.capacity_snapshot().headroom_bytes >= 3 * GIB,
        "the measured headroom must actually cover the admission"
    );
}

/// The periodic tick reclaims noncritical storage when free space has fallen
/// into the reserve, even when every class is under its nominal quota.
#[test]
fn enforce_reclaims_noncritical_storage_when_free_space_enters_reserve() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Arc::new(SimProbe::new());
    let fs = probe.mount(dir.path(), 4, 64 * GIB, 2 * GIB);
    let governor = StorageGovernor::open_with_capacity_probe(
        dir.path(),
        capacity_config(u64::MAX),
        probe.clone(),
    )
    .unwrap();
    governor
        .register_backend(Arc::new(CapacityEvictBackend {
            class: StorageClass::RebuildableCache,
            probe: probe.clone(),
            fs: fs.clone(),
        }))
        .unwrap();
    let now = crate::ledger::now_ms();
    governor
        .replace_inventory(
            StorageClass::RebuildableCache,
            &[
                ("cache-a".to_string(), GIB, now - 10),
                ("cache-b".to_string(), GIB, now - 20),
            ],
        )
        .unwrap();

    let freed = governor.enforce().unwrap();
    assert!(freed >= 64 * GIB * 50 / 1000 - 2 * GIB);
    assert!(probe.available(&fs) >= 64 * GIB * 50 / 1000);
    assert!(
        governor.health().unwrap().is_none(),
        "once the reserve is whole the governor is healthy again"
    );
}

/// When no noncritical bytes can be reclaimed, the reserve is still never
/// breached: the admission is refused typed and health degrades.
#[test]
fn unpreservable_reserve_refuses_typed_and_degrades() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Arc::new(SimProbe::new());
    probe.mount(dir.path(), 5, 64 * GIB, GIB);
    let governor = StorageGovernor::open_with_capacity_probe(
        dir.path(),
        capacity_config(u64::MAX),
        probe.clone(),
    )
    .unwrap();

    let refusal = governor
        .reserve(StorageClass::MainCas, "no-room", GIB)
        .expect_err("nothing can be evicted")
        .refusal()
        .cloned()
        .expect("typed refusal");
    assert_eq!(refusal.reason, RefusalReason::FreeSpaceReserve);
    let health = governor.health().unwrap().expect("typed degraded health");
    assert!(
        health.code.contains("free_space_reserve"),
        "{}",
        health.code
    );
}

#[test]
fn shared_filesystem_roots_are_counted_once() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Arc::new(SimProbe::new());
    probe.mount(dir.path(), 7, 100 * GIB, 50 * GIB);
    let first = dir.path().join("browser-a");
    let second = dir.path().join("browser-b");
    std::fs::create_dir_all(&first).unwrap();
    std::fs::create_dir_all(&second).unwrap();
    let governor = StorageGovernor::open_with_capacity_probe(
        dir.path(),
        capacity_config(u64::MAX),
        probe.clone(),
    )
    .unwrap();
    governor
        .register_backend(Arc::new(DirectoryBackend::with_roots(
            StorageClass::BrowserStorage,
            vec![first, second],
        )))
        .unwrap();

    let capacity = governor.capacity_snapshot();
    assert!(capacity.measured);
    assert_eq!(
        capacity.filesystems.len(),
        1,
        "three roots on one volume are one filesystem"
    );
    assert_eq!(capacity.capacity_bytes, 100 * GIB);
    assert_eq!(capacity.available_bytes, 50 * GIB);
    assert_eq!(governor.metrics().unwrap().filesystems.len(), 1);
}

#[test]
fn operator_cap_lower_than_capacity_wins() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Arc::new(SimProbe::new());
    probe.mount(dir.path(), 8, 1024 * GIB, 900 * GIB);
    let user_cap = 100 * 1024 * 1024;
    let governor = StorageGovernor::open_with_capacity_probe(
        dir.path(),
        capacity_config(user_cap),
        probe.clone(),
    )
    .unwrap();

    let capacity = governor.capacity_snapshot();
    assert_eq!(capacity.effective_budget_bytes, user_cap);
    assert!(capacity.capacity_cap_bytes > user_cap);
    let refusal = governor
        .reserve(StorageClass::MainCas, "over-cap", user_cap + 1)
        .expect_err("the operator cap is the smaller constraint")
        .refusal()
        .cloned()
        .expect("typed refusal");
    assert_eq!(refusal.reason, RefusalReason::ClassQuota);
}

#[test]
fn capacity_configuration_is_clamped_to_safe_bounds() {
    use crate::config::{
        MAX_CAPACITY_PERMILLE, MAX_INSTALL_ALLOWANCE_BYTES, MAX_RESERVE_BYTES,
        MAX_RESERVE_PERMILLE, MIN_CAPACITY_PERMILLE, MIN_INSTALL_ALLOWANCE_BYTES,
        MIN_RESERVE_BYTES, MIN_RESERVE_PERMILLE,
    };

    let low = StorageGovernorConfig {
        capacity_permille: 0,
        reserve_permille: 0,
        reserve_bytes: 0,
        min_install_allowance_bytes: 0,
        ..Default::default()
    }
    .validated()
    .unwrap();
    assert_eq!(low.capacity_permille, MIN_CAPACITY_PERMILLE);
    assert_eq!(low.reserve_permille, MIN_RESERVE_PERMILLE);
    assert_eq!(low.reserve_bytes, MIN_RESERVE_BYTES);
    assert_eq!(low.min_install_allowance_bytes, MIN_INSTALL_ALLOWANCE_BYTES);

    let high = StorageGovernorConfig {
        capacity_permille: u16::MAX,
        reserve_permille: u16::MAX,
        reserve_bytes: u64::MAX,
        min_install_allowance_bytes: u64::MAX,
        ..Default::default()
    }
    .validated()
    .unwrap();
    assert_eq!(high.capacity_permille, MAX_CAPACITY_PERMILLE);
    assert_eq!(high.reserve_permille, MAX_RESERVE_PERMILLE);
    assert_eq!(high.reserve_bytes, MAX_RESERVE_BYTES);
    assert_eq!(
        high.min_install_allowance_bytes,
        MAX_INSTALL_ALLOWANCE_BYTES
    );
}

// ---------------------------------------------------------------------------
// Audit item 18: capacity-governor counterexamples (adversarial acceptance).
// ---------------------------------------------------------------------------

/// Probe seam for acceptance test (a): fails every measurement on demand so
/// the "no volume can be measured" state is reproducible, and recovers to a
/// fixed healthy volume so admission-recovery can be asserted.
struct ToggleProbe {
    fail: std::sync::atomic::AtomicBool,
}

impl ToggleProbe {
    fn failing() -> Self {
        Self {
            fail: std::sync::atomic::AtomicBool::new(true),
        }
    }
}

impl crate::capacity::CapacityProbe for ToggleProbe {
    fn space(&self, _path: &Path) -> Option<crate::capacity::FilesystemSpace> {
        if self.fail.load(std::sync::atomic::Ordering::Acquire) {
            return None;
        }
        Some(crate::capacity::FilesystemSpace {
            filesystem_id: 7,
            capacity_bytes: 64 * GIB,
            available_bytes: 40 * GIB,
        })
    }
}

/// Counterexample 1 — a failed capacity probe must NEVER widen the effective
/// admission budget to the configured operator cap (default u64::MAX). New
/// material storage is refused typed `capacity_unmeasured`, the exact
/// degraded state is visible on every health surface, and admission resumes
/// only when a probe succeeds again.
#[test]
fn failed_capacity_probe_refuses_new_storage_typed_until_measurement_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Arc::new(ToggleProbe::failing());
    let governor = StorageGovernor::open_with_capacity_probe(
        dir.path(),
        capacity_config(u64::MAX),
        probe.clone(),
    )
    .unwrap();

    // (a) reservation path: typed refusal, not an unbounded admission.
    let refusal = governor
        .reserve(StorageClass::MainCas, "unmeasured", 1_000)
        .expect_err("no measurement, no new material storage")
        .refusal()
        .cloned()
        .expect("typed refusal");
    assert_eq!(refusal.reason, RefusalReason::CapacityUnmeasured);
    assert_eq!(refusal.requested_bytes, 1_000);
    assert_eq!(governor.pending_reservations().unwrap(), 0);
    assert_eq!(governor.counters().unwrap().refusals, 1);

    // SQLite growth path: the same typed refusal before any append.
    let growth = governor
        .admit_sqlite_growth(4_096)
        .expect_err("unmeasured volume refuses WAL growth too")
        .refusal()
        .cloned()
        .expect("typed refusal");
    assert_eq!(growth.reason, RefusalReason::CapacityUnmeasured);
    assert!(governor.counters().unwrap().refusals >= 2);

    // The exact degraded state is surfaced, not a silent fallback.
    let health = governor
        .health()
        .unwrap()
        .expect("measurement failure is a visible degraded state");
    assert_eq!(health.code, "capacity_unmeasured", "{}", health.code);
    let metrics = governor.metrics().unwrap();
    assert!(!metrics.capacity_measured);
    assert!(
        metrics
            .degraded
            .as_ref()
            .is_some_and(|degraded| degraded.code == "capacity_unmeasured"),
        "{:?}",
        metrics.degraded
    );

    // Recovery: one successful probe reopens admission and clears health.
    probe
        .fail
        .store(false, std::sync::atomic::Ordering::Release);
    governor
        .reserve(StorageClass::MainCas, "recovered", 1_000)
        .expect("admission resumes when measurement succeeds")
        .abort();
    assert!(
        governor.health().unwrap().is_none(),
        "measurement recovery must clear the capacity_unmeasured degradation"
    );
    assert!(governor.metrics().unwrap().capacity_measured);
}

/// Counterexample 2 — free bytes from DISTINCT filesystems are pooled by the
/// aggregate snapshot; a nearly-full destination volume A must keep its own
/// reserve even when an empty, larger volume B pools into the totals. The
/// write is targeted at A explicitly (and, through conservative candidate
/// resolution, through plain `reserve` as well).
#[test]
fn nearly_full_destination_volume_is_not_masked_by_an_empty_other_volume() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Arc::new(SimProbe::new());
    let reserve_a = 64 * GIB * 50 / 1000; // 5% of A, above the 2 GiB floor
    let a_free = reserve_a + GIB / 4;
    let fs_a = probe.mount(dir.path(), 21, 64 * GIB, a_free);
    let other = dir.path().join("other-volume");
    std::fs::create_dir_all(&other).unwrap();
    let fs_b = probe.mount(&other, 22, 500 * GIB, 400 * GIB);
    let governor = StorageGovernor::open_with_capacity_probe(
        dir.path(),
        capacity_config(u64::MAX),
        probe.clone(),
    )
    .unwrap();
    // A CAS-style second volume joins the measured snapshot (the pooling
    // surface the audit flagged).
    governor.register_capacity_root(&other);

    // Mutation witness: the pooled aggregate would happily admit 10 GiB.
    let capacity = governor.capacity_snapshot();
    assert!(capacity.measured);
    assert_eq!(capacity.filesystems.len(), 2, "two distinct volumes pooled");
    assert!(
        capacity.headroom_bytes > 10 * GIB,
        "aggregate headroom {} would mask A's shortfall",
        capacity.headroom_bytes
    );

    // Write TARGETING A: A's own reserve wins over the pooled headroom.
    let target_a = dir.path().join("target-a");
    let refusal = governor
        .reserve_into(StorageClass::MainCas, "target-a", GIB, &target_a)
        .expect_err("A's reserve must be preserved despite B's emptiness")
        .refusal()
        .cloned()
        .expect("typed refusal");
    assert_eq!(refusal.reason, RefusalReason::FreeSpaceReserve);
    assert_eq!(
        refusal.available_bytes,
        GIB / 4,
        "available must be A's own headroom, never B's"
    );
    assert_eq!(
        probe.available(&fs_a),
        a_free,
        "no eviction may touch A while refusing; its free space is unchanged"
    );

    // Plain `reserve` (conservative candidates A+B) refuses on A as well.
    let plain = governor
        .reserve(StorageClass::MainCas, "candidates", GIB)
        .expect_err("every candidate volume must keep its reserve")
        .refusal()
        .cloned()
        .expect("typed refusal");
    assert_eq!(plain.reason, RefusalReason::FreeSpaceReserve);
    assert_eq!(plain.available_bytes, GIB / 4);

    // The SAME size targeting healthy volume B is admitted (no over-refusal
    // from A's pressure).
    governor
        .reserve_into(StorageClass::MainCas, "target-b", GIB, other.join("blob"))
        .expect("volume B has room for its own reserve and the write")
        .abort();
    assert_eq!(probe.available(&fs_b), 400 * GIB);
}

/// Counterexample 3 (sequential) — reservations granted but not yet
/// materialized are subtracted from the destination volume: the second of
/// two pending writes refuses even though the AGGREGATE budget and class
/// quota happily fit both (their combined reservations fit the quota; only
/// the volume cannot hold them).
#[test]
fn unmaterialized_reservations_block_a_second_write_on_the_destination_volume() {
    let dir = tempfile::tempdir().unwrap();
    let probe = Arc::new(SimProbe::new());
    let reserve = 64 * GIB * 50 / 1000;
    let write = GIB;
    // The volume holds its reserve plus 1.5 writes: exactly ONE write fits.
    probe.mount(dir.path(), 11, 64 * GIB, reserve + write + write / 2);
    let governor = StorageGovernor::open_with_capacity_probe(
        dir.path(),
        capacity_config(100 * GIB),
        probe.clone(),
    )
    .unwrap();

    let first = governor
        .reserve(StorageClass::MainCas, "first", write)
        .expect("the first write fits above the reserve");
    assert_eq!(governor.pending_reservations().unwrap(), 1);

    // Aggregate room still exists (quota 3.2 GiB+ sees 1 of 100 GiB used);
    // the refusal must come from the destination volume's charge.
    let refusal = governor
        .reserve(StorageClass::MainCas, "second", write)
        .expect_err("the unmaterialized first write already spent the room")
        .refusal()
        .cloned()
        .expect("typed refusal");
    assert_eq!(refusal.reason, RefusalReason::FreeSpaceReserve);
    assert_eq!(
        refusal.available_bytes,
        write + write / 2,
        "available is the volume headroom above its reserve"
    );
    assert_eq!(
        governor.pending_reservations().unwrap(),
        1,
        "only the first reservation exists"
    );

    // Releasing the unmaterialized write returns exactly its bytes.
    drop(first);
    assert_eq!(governor.pending_reservations().unwrap(), 0);
    governor
        .reserve(StorageClass::MainCas, "third", write)
        .expect("a released reservation restores the volume room")
        .abort();
}

/// Counterexample 3 (concurrent) — two synchronized writers race for the
/// same destination volume before either materializes. The free-space rule
/// is evaluated INSIDE the admission transaction, so the second writer
/// cannot pass on the first's stale free reading: exactly one admits, and
/// the total reservation fits the volume.
#[test]
fn two_synchronized_reservations_cannot_both_pass_one_free_space_reading() {
    use std::sync::Barrier;

    let dir = tempfile::tempdir().unwrap();
    let probe = Arc::new(SimProbe::new());
    let reserve = 64 * GIB * 50 / 1000;
    let write = GIB;
    // Room for the reserve plus 1.5 writes: two reservations cannot fit.
    probe.mount(dir.path(), 12, 64 * GIB, reserve + write + write / 2);
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|index| {
            let path = dir.path().to_path_buf();
            let config = capacity_config(100 * GIB);
            let probe = probe.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let governor =
                    StorageGovernor::open_with_capacity_probe(path, config, probe).unwrap();
                barrier.wait();
                match governor.reserve(StorageClass::MainCas, &format!("race-{index}"), write) {
                    Ok(reservation) => {
                        let bytes = reservation.bytes();
                        std::mem::forget(reservation);
                        bytes
                    }
                    Err(GovernorError::Refused(refusal)) => {
                        assert_eq!(refusal.reason, RefusalReason::FreeSpaceReserve);
                        0
                    }
                    Err(error) => panic!("unexpected admission failure: {error}"),
                }
            })
        })
        .collect();
    let admitted: u64 = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .sum();
    assert_eq!(
        admitted, write,
        "exactly one of two synchronized writers may admit (admitted {admitted})"
    );
    assert_eq!(
        probe.available(&fs_of(&probe)),
        reserve + write + write / 2,
        "neither writer materialized: measured free space must be untouched"
    );
}

/// Helper for the concurrent volume test: the single mounted SimFs.
/// (SimProbe only ever mounts one volume in that test.)
fn fs_of(probe: &SimProbe) -> std::sync::Arc<SimFs> {
    let roots = probe.roots.lock().unwrap();
    roots[0].1.clone()
}

/// Counterexample 3 + crash reconciliation — boundary states of one write:
/// crash at RESERVE (row, no bytes), at PUBLISH (row + materialized bytes,
/// no commit), and at COMMIT (atomic tx: row gone, bytes indexed). Across
/// every boundary the accounting must stay conservative (never over-admit)
/// and materialized bytes must be counted EXACTLY ONCE — never in both
/// measured free space and a live reservation/usage double charge.
#[test]
fn crash_at_reserve_publish_or_commit_reconciles_conservatively_without_double_charging() {
    let write = GIB;
    let reserve = 64 * GIB * 50 / 1000;
    let dead = "dead-process"; // pid parse fails -> treated as a dead owner

    // ---- boundary 1: crash between reserve and publish -----------------
    {
        let dir = tempfile::tempdir().unwrap();
        let probe = Arc::new(SimProbe::new());
        // Room for the reserve plus 1.5 writes; nothing was ever written.
        probe.mount(dir.path(), 31, 64 * GIB, reserve + write + write / 2);
        let governor = StorageGovernor::open_with_capacity_probe(
            dir.path(),
            capacity_config(100 * GIB),
            probe.clone(),
        )
        .unwrap();
        // The dead process's durable reservation row (no volume attribution:
        // legacy rows charge every volume conservatively).
        {
            let mut ledger =
                crate::ledger::Ledger::open(&StorageGovernor::database_path(dir.path())).unwrap();
            let now = crate::ledger::now_ms();
            ledger
                .insert_reservation(
                    "crashed-reserve",
                    StorageClass::MainCas,
                    "crashed",
                    write,
                    dead,
                    now,
                    now + 60_000,
                )
                .unwrap();
        }
        // While the row is live its bytes are charged: never over-admit.
        let refusal = governor
            .reserve_into(
                StorageClass::MainCas,
                "over-crash",
                write,
                dir.path().join("w"),
            )
            .expect_err("the crashed reservation still holds its bytes")
            .refusal()
            .cloned()
            .expect("typed refusal");
        assert_eq!(refusal.reason, RefusalReason::FreeSpaceReserve);
        assert_eq!(
            probe.available(&fs_of(&probe)),
            reserve + write + write / 2,
            "refusal must not evict or fabricate free space"
        );

        // Restart sweeps the dead owner (crash seam): the row's bytes never
        // existed on disk, so admission room returns exactly once.
        let governor = StorageGovernor::open_with_capacity_probe(
            dir.path(),
            capacity_config(100 * GIB),
            probe.clone(),
        )
        .unwrap();
        assert_eq!(governor.pending_reservations().unwrap(), 0);
        governor
            .reserve_into(
                StorageClass::MainCas,
                "after-sweep",
                write,
                dir.path().join("w"),
            )
            .expect("the swept reservation restores room exactly once")
            .abort();
        // ...but the volume still holds only ONE write: never over-admit.
        governor
            .reserve_into(
                StorageClass::MainCas,
                "still-bounded",
                write + write,
                dir.path().join("w2"),
            )
            .expect_err("recovery must not widen the volume beyond its room");
    }

    // ---- boundary 2: crash between publish and commit ------------------
    {
        let dir = tempfile::tempdir().unwrap();
        let probe = Arc::new(SimProbe::new());
        // Room for the reserve plus 2.5 writes.
        probe.mount(dir.path(), 32, 64 * GIB, reserve + 2 * write + write / 2);
        let governor = StorageGovernor::open_with_capacity_probe(
            dir.path(),
            capacity_config(100 * GIB),
            probe.clone(),
        )
        .unwrap();
        {
            let mut ledger =
                crate::ledger::Ledger::open(&StorageGovernor::database_path(dir.path())).unwrap();
            let now = crate::ledger::now_ms();
            ledger
                .insert_reservation(
                    "crashed-publish",
                    StorageClass::MainCas,
                    "published",
                    write,
                    dead,
                    now,
                    now + 60_000,
                )
                .unwrap();
        }
        // The bytes materialized before the crash: measured free dropped.
        std::fs::write(dir.path().join("published-blob"), b"partial").unwrap();
        probe.consume(&fs_of(&probe), write);

        // Pre-sweep the row AND the consumed bytes are charged — conservative
        // over-subtraction, never over-admission.
        governor
            .reserve_into(
                StorageClass::MainCas,
                "pre-sweep",
                write,
                dir.path().join("w"),
            )
            .expect_err("live row + materialized bytes must not admit a third charge");

        // Restart sweeps the dead row: the materialized bytes are then
        // counted exactly ONCE — through measured free space, not twice.
        let governor = StorageGovernor::open_with_capacity_probe(
            dir.path(),
            capacity_config(100 * GIB),
            probe.clone(),
        )
        .unwrap();
        assert_eq!(governor.pending_reservations().unwrap(), 0);
        governor
            .reserve_into(
                StorageClass::MainCas,
                "one-more",
                write,
                dir.path().join("w"),
            )
            .expect(
                "no double charge: one write fits once the crashed row is swept (free space \
                 still carries the materialized bytes)",
            )
            .abort();
        governor
            .reserve_into(
                StorageClass::MainCas,
                "two-more",
                write + write,
                dir.path().join("w2"),
            )
            .expect_err("never over-admit past the volume's remaining room");
    }

    // ---- boundary 3: commit (atomic tx: row deleted + bytes indexed) ---
    {
        let dir = tempfile::tempdir().unwrap();
        let probe = Arc::new(SimProbe::new());
        probe.mount(dir.path(), 33, 64 * GIB, reserve + 2 * write + write / 2);
        let governor = StorageGovernor::open_with_capacity_probe(
            dir.path(),
            capacity_config(100 * GIB),
            probe.clone(),
        )
        .unwrap();
        // A commit crash can only land on one side of one transaction: the
        // row is deleted and the object indexed atomically. Reproduce the
        // committed side of that transaction with the same disk state as
        // boundary 2's post-sweep state.
        let reservation = governor
            .reserve_into(
                StorageClass::MainCas,
                "committing",
                write,
                dir.path().join("c"),
            )
            .unwrap();
        std::fs::write(dir.path().join("committed-blob"), b"done").unwrap();
        probe.consume(&fs_of(&probe), write);
        reservation.commit(write, None).unwrap();
        assert_eq!(governor.pending_reservations().unwrap(), 0);
        assert_eq!(governor.metrics().unwrap().total_bytes, write);

        // Byte-identical admission outcomes to boundary 2's post-sweep
        // state: usage accounting (quota side) adds no second charge against
        // measured free space.
        governor
            .reserve_into(
                StorageClass::MainCas,
                "one-more",
                write,
                dir.path().join("w"),
            )
            .expect("committed bytes are charged once — through free space")
            .abort();
        governor
            .reserve_into(
                StorageClass::MainCas,
                "two-more",
                write + write,
                dir.path().join("w2"),
            )
            .expect_err("never over-admit past the volume's remaining room");
    }
}
