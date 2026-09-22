//! Adversarial certification for the commerce store, jobs and service
//! (`docs/acquire.md` §11, §12, §15). Every test here tries to break an
//! invariant: crash mid-job, reopen, craft a newer/negative schema, plant a
//! secret, request stale data as current, cross an account scope, exceed
//! every bound.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use faktor_commerce::bom::BomItem;
use faktor_commerce::cache::SingleFlight;
use faktor_commerce::connector::{
    AcquisitionPath, Capability, CommerceConnector, ConnectorPolicy, Discovery, ProfileIdentity,
    QuoteCandidate,
};
use faktor_commerce::error::SourceError;
use faktor_commerce::jobs::{
    advance_job, submit_job, CommerceJobRequest, ItemOutcome, JobItemExecutor, JobItemState,
    JobLine, JobState, JobWork,
};
use faktor_commerce::offer::{
    CommercialOffer, LifecycleStatus, ObservationOrigin, OfferProvenance, PriceBreak,
    PriceVisibility, StockState,
};
use faktor_commerce::query::{
    DetailLevel, FreshnessMode, ProductRef, ProductRequest, QuoteRequest, SearchRequest, SourceSet,
};
use faktor_commerce::result::{
    artifact_digest, scan_forbidden_bytes, ArtifactRef, ArtifactStore, CompactStatus,
    COMPACT_RESULT_HARD_MAX_BYTES, MAX_ARTIFACT_BYTES,
};
use faktor_commerce::service::{CommerceSourceService, ServiceConfig, ServiceError};
use faktor_commerce::store::{
    CommerceStore, CommerceStoreError, GcPolicy, NewCacheRow, NormalizedPayload,
    SnapshotDiagnostics, COMMERCE_MIGRATIONS, COMMERCE_SCHEMA_VERSION,
};
use faktor_commerce::text::AccountScope;
use faktor_commerce::{
    CacheClass, CacheDecision, CacheIdentity, CacheTtls, ConditionalValidators, Currency,
    Freshness, Money, NonZeroQuantity, PackagingType, PricingScope, ProductIdentity, Quantity,
    SourceId, Text, VariantId, MAX_PAYLOAD_BYTES,
};

// ----------------------------------------------------------------- helpers

fn source(id: &str) -> SourceId {
    SourceId::new(id).expect("source")
}

fn account(id: &str) -> AccountScope {
    AccountScope::new(id).expect("account")
}

fn temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn db_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
    dir.path().join("commerce").join("commerce.db")
}

fn open_store(dir: &tempfile::TempDir) -> CommerceStore {
    CommerceStore::open(&db_path(dir)).expect("store open")
}

fn provenance(source: &SourceId) -> OfferProvenance {
    OfferProvenance {
        origin: ObservationOrigin::OfficialApi,
        source: source.clone(),
        extractor_version: Some(Text::<64>::new("extractor-1").expect("version")),
        connector_version: Some(Text::<64>::new("connector-3").expect("version")),
        normalization_version: Some(Text::<64>::new("normalize-2").expect("version")),
        content_digest: None,
        account_scope: None,
        locale: None,
        market: None,
        source_confidence_bp: None,
    }
}

fn identity(mpn: &str) -> ProductIdentity {
    ProductIdentity {
        manufacturer: Some(Text::<128>::new("Texas Instruments").expect("manufacturer")),
        manufacturer_part_number: Some(Text::<256>::new(mpn).expect("manufacturer_part_number")),
        source_part_number: None,
        offer_id: Some(Text::<512>::new("offer-12345").expect("offer id")),
        canonical_url: None,
        category: Some(Text::<128>::new("Power Management").expect("category")),
    }
}

fn offer(source_id: &str, mpn: &str, observed_at_ms: u64) -> CommercialOffer {
    let source = source(source_id);
    CommercialOffer {
        source: source.clone(),
        identity: identity(mpn),
        title: Text::<512>::new("TPS5430 3A step-down converter").expect("title"),
        description: Some(Text::<4096>::new("Buck converter, 5.5-36V input").expect("desc")),
        currency: Currency::USD,
        price_breaks: vec![PriceBreak {
            min_quantity: NonZeroQuantity::new(1).expect("qty"),
            max_quantity: None,
            unit_price: Money::from_micros(Currency::USD, 18_200),
            visibility: PriceVisibility::Public,
            account_scope: None,
            promotion: None,
        }],
        variants: Vec::new(),
        moq: Some(NonZeroQuantity::new(1).expect("qty")),
        order_multiple: None,
        standard_pack: None,
        stock: StockState::InStock {
            quantity: NonZeroQuantity::new(5_000).expect("qty"),
        },
        lead_time: None,
        packaging: Vec::new(),
        supplier: None,
        manufacturer: None,
        provenance: provenance(&source),
        observed_at_ms,
        price_visibility: PriceVisibility::Public,
        lifecycle: LifecycleStatus::Active,
    }
}

fn discovery(source_id: &str, mpn: &str) -> Discovery {
    let source = source(source_id);
    Discovery {
        source: source.clone(),
        identity: identity(mpn),
        title: Text::<512>::new("TPS5430 3A step-down converter").expect("title"),
        url: None,
        price_range: None,
        stock: StockState::InStock {
            quantity: NonZeroQuantity::new(5_000).expect("qty"),
        },
        supplier: None,
        packaging: None,
        moq: None,
        observed_at_ms: 1_000_000,
        provenance: provenance(&source),
    }
}

fn quote_candidate(source_id: &str, mpn: &str, quantity: u64) -> QuoteCandidate {
    let offer = offer(source_id, mpn, 1_000_000);
    let resolution = faktor_commerce::quote::price_at_quantity(
        &offer,
        faktor_commerce::quote::VariantRequest::None,
        Quantity::new(quantity).expect("quantity"),
    );
    QuoteCandidate {
        offer,
        resolution,
        freshness: Freshness::Live,
    }
}

fn cache_identity(
    class: CacheClass,
    source_id: &str,
    product: &str,
    account_scope: Option<AccountScope>,
) -> CacheIdentity {
    CacheIdentity {
        class,
        source: source(source_id),
        product: Text::<2048>::new(product).expect("product"),
        account_scope,
        locale: None,
        market: None,
        currency: None,
        quantity: None,
        packaging: None,
        variant: None,
        pricing_scope: PricingScope::Public,
    }
}

fn bom(lines: &[(&str, u64)]) -> faktor_commerce::bom::Bom {
    faktor_commerce::bom::Bom::from_pairs(lines).expect("bom")
}

fn bom_request(bom: faktor_commerce::bom::Bom) -> CommerceJobRequest {
    CommerceJobRequest {
        work: JobWork::Bom { bom },
        sources: SourceSet::auto(),
        freshness: FreshnessMode::PreferCache,
        detail: DetailLevel::Compact,
        account: None,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64
}

fn backup_files(dir: &tempfile::TempDir) -> Vec<std::path::PathBuf> {
    let backups = dir.path().join("commerce").join("commerce-backups");
    let mut paths: Vec<std::path::PathBuf> = match std::fs::read_dir(backups) {
        Ok(entries) => entries
            .map(|entry| entry.expect("entry").path())
            .filter(|path| path.extension().map(|ext| ext == "db").unwrap_or(false))
            .collect(),
        Err(_) => Vec::new(),
    };
    paths.sort();
    paths
}

fn backup_count(dir: &tempfile::TempDir) -> usize {
    backup_files(dir).len()
}

fn raw_conn(dir: &tempfile::TempDir) -> rusqlite::Connection {
    rusqlite::Connection::open(db_path(dir)).expect("raw open")
}

// ------------------------------------------------------------- store tests

#[test]
fn fresh_store_migrates_and_records_a_verified_restore_point() {
    let dir = temp_dir();
    let store = open_store(&dir);
    assert_eq!(
        store.schema_version().expect("version"),
        COMMERCE_SCHEMA_VERSION
    );
    assert_eq!(COMMERCE_SCHEMA_VERSION, COMMERCE_MIGRATIONS.len() as i64);
    assert!(db_path(&dir).exists());
    assert_eq!(backup_count(&dir), 1, "one pre-migration restore point");
    let backups = backup_files(&dir);
    assert!(
        backups[0]
            .file_name()
            .expect("name")
            .to_string_lossy()
            .contains("pre-migration-v0"),
        "fresh creation snapshots v0"
    );
    let check = rusqlite::Connection::open_with_flags(
        &backups[0],
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open backup");
    let verdict: String = check
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("integrity");
    assert_eq!(verdict, "ok");
    let version: i64 = check
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("backup version");
    assert_eq!(version, 0, "the restore point is the predecessor state");
    drop(store);
    drop(check);

    // Reopening at the ladder version writes no further snapshot and does
    // not re-run migrations.
    let reopened = open_store(&dir);
    assert_eq!(
        reopened.schema_version().expect("version"),
        COMMERCE_SCHEMA_VERSION
    );
    assert_eq!(backup_count(&dir), 1);
}

#[test]
fn v1_store_upgrades_with_a_restore_point_and_preserved_rows() {
    let dir = temp_dir();
    std::fs::create_dir_all(dir.path().join("commerce")).expect("commerce dir");
    {
        let conn = raw_conn(&dir);
        conn.execute_batch(COMMERCE_MIGRATIONS[0])
            .expect("v1 schema");
        conn.execute_batch("PRAGMA user_version = 1")
            .expect("cursor");
        conn.execute(
            "INSERT INTO product_identity(identity_key, normalized_mpn, manufacturer,
                created_ms, updated_ms) VALUES ('k1', 'TPS5430DDAR', 'Texas Instruments', 1, 1)",
            [],
        )
        .expect("seed identity");
        conn.execute(
            "INSERT INTO search_cache(cache_key, class, source, product, pricing_scope,
                payload_json, content_digest, observed_at_ms, hits)
             VALUES ('legacy-key', 'price', 'mouser', 'mpn:x', 'public', '{}', 'd', 5, 0)",
            [],
        )
        .expect("seed cache");
    }
    let store = open_store(&dir);
    assert_eq!(
        store.schema_version().expect("version"),
        COMMERCE_SCHEMA_VERSION
    );
    let count: i64 = {
        let conn = raw_conn(&dir);
        conn.query_row("SELECT COUNT(*) FROM product_identity", [], |row| {
            row.get(0)
        })
        .expect("count")
    };
    assert_eq!(count, 1, "the pre-migration row survived");
    let cache_count = store.cache_count().expect("cache count");
    assert_eq!(cache_count, 1);
    assert_eq!(backup_count(&dir), 1, "the upgrade wrote one restore point");
    let backup = backup_files(&dir).into_iter().next().expect("one backup");
    assert!(backup
        .file_name()
        .expect("name")
        .to_string_lossy()
        .contains("pre-migration-v1"));
    let check =
        rusqlite::Connection::open_with_flags(&backup, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open backup");
    let version: i64 = check
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("version");
    assert_eq!(version, 1, "the restore point is exactly the v1 state");
}

#[test]
fn newer_schema_is_refused_typed_without_writes() {
    let dir = temp_dir();
    drop(open_store(&dir));
    {
        let conn = raw_conn(&dir);
        conn.execute_batch(&format!(
            "PRAGMA user_version = {}",
            COMMERCE_SCHEMA_VERSION + 1
        ))
        .expect("downgrade setup");
    }
    let backups_before = backup_count(&dir);
    let error = CommerceStore::open(&db_path(&dir))
        .err()
        .expect("newer refused");
    match error {
        CommerceStoreError::UnsupportedSchema {
            found,
            maximum_supported,
        } => {
            assert_eq!(found, COMMERCE_SCHEMA_VERSION + 1);
            assert_eq!(maximum_supported, COMMERCE_SCHEMA_VERSION);
        }
        other => panic!("expected UnsupportedSchema, got {other:?}"),
    }
    let conn = raw_conn(&dir);
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("version");
    assert_eq!(version, COMMERCE_SCHEMA_VERSION + 1, "no write happened");
    assert_eq!(backup_count(&dir), backups_before, "no snapshot happened");
}

#[test]
fn negative_schema_is_refused_typed_without_writes() {
    let dir = temp_dir();
    drop(open_store(&dir));
    {
        let conn = raw_conn(&dir);
        conn.execute_batch("PRAGMA user_version = -1")
            .expect("corruption setup");
    }
    let backups_before = backup_count(&dir);
    let error = CommerceStore::open(&db_path(&dir))
        .err()
        .expect("negative refused");
    assert_eq!(error, CommerceStoreError::CorruptSchema(-1));
    assert_eq!(backup_count(&dir), backups_before);
    let conn = raw_conn(&dir);
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("version");
    assert_eq!(version, -1, "the corrupt cursor is untouched");
}

#[test]
fn concurrent_openers_serialize_and_converge() {
    let dir = temp_dir();
    std::fs::create_dir_all(dir.path().join("commerce")).expect("commerce dir");
    let path = db_path(&dir);
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let path = path.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            CommerceStore::open(&path).map(|store| store.schema_version().expect("version"))
        }));
    }
    for handle in handles {
        let version = handle.join().expect("join").expect("opener");
        assert_eq!(version, COMMERCE_SCHEMA_VERSION);
    }
    assert_eq!(backup_count(&dir), 1, "exactly one migration pass");
    let store = open_store(&dir);
    assert_eq!(
        store.schema_version().expect("version"),
        COMMERCE_SCHEMA_VERSION
    );
}

#[test]
fn cache_field_level_ttls_and_config_override() {
    let dir = temp_dir();
    let store = open_store(&dir);
    let observed = 1_000_000u64;
    let ttls = CacheTtls::default();
    let product = cache_identity(CacheClass::Product, "mouser", "offer:1", None);
    let price = cache_identity(CacheClass::Price, "mouser", "offer:1", None);
    for identity in [&product, &price] {
        store
            .cache_put(&NewCacheRow {
                identity: identity.clone(),
                payload: NormalizedPayload::from_json("{\"v\":1}".to_string()).expect("payload"),
                validators: ConditionalValidators::default(),
                observed_at_ms: observed,
            })
            .expect("put");
    }
    // 1800 s + 1 later: the price is stale, the product description is not.
    let later = observed + 1_800_001;
    let price_row = store
        .cache_get(&price, ttls.ttl_ms(CacheClass::Price), later)
        .expect("get price")
        .expect("row");
    assert!(matches!(price_row.decision, CacheDecision::Stale(_)));
    let product_row = store
        .cache_get(&product, ttls.ttl_ms(CacheClass::Product), later)
        .expect("get product")
        .expect("row");
    match product_row.decision {
        CacheDecision::Fresh(freshness) => assert!(freshness.is_current()),
        other => panic!("product must still be fresh, got {other:?}"),
    }

    // A configured TTL overrides the default per class.
    let short = CacheTtls {
        price_ms: 60_000,
        ..CacheTtls::default()
    };
    assert!(short.validate().is_ok());
    let row = store
        .cache_get(&price, short.ttl_ms(CacheClass::Price), observed + 60_001)
        .expect("get")
        .expect("row");
    assert!(matches!(row.decision, CacheDecision::Stale(_)));
    let invalid = CacheTtls {
        stock_ms: 0,
        ..CacheTtls::default()
    };
    assert!(invalid.validate().is_err());
}

#[test]
fn conditional_revalidation_extends_freshness_without_a_new_payload() {
    let dir = temp_dir();
    let store = open_store(&dir);
    let identity = cache_identity(CacheClass::Product, "mouser", "offer:9", None);
    let observed = 1_000_000u64;
    store
        .cache_put(&NewCacheRow {
            identity: identity.clone(),
            payload: NormalizedPayload::from_json("{\"title\":\"original\"}".to_string())
                .expect("payload"),
            validators: ConditionalValidators {
                etag: Some("\"v1\"".to_string()),
                last_modified_ms: Some(500),
            },
            observed_at_ms: observed,
        })
        .expect("put");
    // 2001 s later the entry is stale under the product TTL.
    let stale_now = observed + 21_600_001;
    let row = store
        .cache_get(&identity, 2_160_000, stale_now)
        .expect("get")
        .expect("row");
    assert!(matches!(row.decision, CacheDecision::Stale(_)));

    // A 304 revalidation moves the freshness origin forward, keeps the
    // payload byte-identical and writes no duplicate snapshot.
    let snapshots_before = store.offer_snapshot_count().expect("count");
    let updated = store
        .cache_revalidate(
            &identity.digest(),
            &ConditionalValidators {
                etag: Some("\"v2\"".to_string()),
                last_modified_ms: None,
            },
            stale_now,
        )
        .expect("revalidate");
    assert!(updated);
    let row = store
        .cache_get(&identity, 2_160_000, stale_now)
        .expect("get")
        .expect("row");
    match row.decision {
        CacheDecision::Fresh(freshness) => assert!(freshness.is_current()),
        other => panic!("revalidated entry must be fresh, got {other:?}"),
    }
    assert_eq!(row.payload.as_str(), "{\"title\":\"original\"}");
    assert_eq!(row.observed_at_ms, observed, "observation time unchanged");
    assert_eq!(row.etag.as_deref(), Some("\"v2\""));
    assert_eq!(
        store
            .cache_validators(&identity.digest())
            .expect("v")
            .etag
            .as_deref(),
        Some("\"v2\"")
    );
    assert_eq!(
        store.offer_snapshot_count().expect("count"),
        snapshots_before,
        "304 writes no snapshot"
    );
    assert!(!store
        .cache_revalidate("missing-key", &ConditionalValidators::default(), stale_now)
        .expect("no row"));
}

#[test]
fn cross_account_cache_isolation_at_the_store() {
    let dir = temp_dir();
    let store = open_store(&dir);
    let a = account("acct-a");
    let b = account("acct-b");
    let identity_a = cache_identity(CacheClass::Price, "mouser", "offer:77", Some(a.clone()));
    let identity_b = cache_identity(CacheClass::Price, "mouser", "offer:77", Some(b));
    store
        .cache_put(&NewCacheRow {
            identity: identity_a.clone(),
            payload: NormalizedPayload::from_json("{\"unit\":\"account-a\"}".to_string())
                .expect("payload"),
            validators: ConditionalValidators::default(),
            observed_at_ms: now_ms(),
        })
        .expect("put");
    // A different account scope never sees the row (different key).
    assert!(store
        .cache_get(&identity_b, 1_800_000, now_ms())
        .expect("get")
        .is_none());
    // Even a crafted key lookup under the wrong scope is refused by the
    // account guard in the WHERE clause.
    assert!(store
        .cache_get_scoped(
            &identity_a.digest(),
            CacheClass::Price,
            Some(&account("acct-b")),
            1_800_000,
            now_ms(),
        )
        .expect("scoped get")
        .is_none());
    assert!(store
        .cache_get_scoped(
            &identity_a.digest(),
            CacheClass::Price,
            None,
            1_800_000,
            now_ms(),
        )
        .expect("scoped get")
        .is_none());
    let own = store
        .cache_get(&identity_a, 1_800_000, now_ms())
        .expect("get")
        .expect("own row");
    assert_eq!(own.payload.as_str(), "{\"unit\":\"account-a\"}");
}

#[test]
fn gc_bounds_rows_bytes_and_ttl() {
    let dir = temp_dir();
    let store = open_store(&dir);
    let now = now_ms();
    for i in 0..12 {
        let identity = cache_identity(
            CacheClass::Discovery,
            "mouser",
            &format!("search:q{i}"),
            None,
        );
        // Half the rows are long expired; the rest are fresh.
        let observed = if i % 2 == 0 { now - 100_000_000 } else { now };
        store
            .cache_put(&NewCacheRow {
                identity,
                payload: NormalizedPayload::from_json(format!("{{\"i\":{i}}}")).expect("payload"),
                validators: ConditionalValidators::default(),
                observed_at_ms: observed,
            })
            .expect("put");
    }
    for i in 0..6 {
        store
            .record_offer_snapshot(
                &offer("mouser", &format!("PART{i:04}"), now),
                &Default::default(),
                &SnapshotDiagnostics::default(),
                now,
            )
            .expect("snapshot");
    }
    let policy = GcPolicy {
        max_cache_rows: 3,
        max_offer_snapshots: 2,
        max_snapshot_age_ms: 30 * 24 * 60 * 60 * 1_000,
        max_cache_age_ms: 1_000_000,
        ..GcPolicy::default()
    };
    let report = store.gc(&policy, now).expect("gc");
    assert!(report.cache_rows_removed >= 9);
    assert!(report.offer_snapshots_removed >= 4);
    assert!(report.bytes_after <= report.bytes_before);
    assert!(store.cache_count().expect("count") <= 3);
    assert!(store.offer_snapshot_count().expect("count") <= 2);
    // Expired rows are gone even when under the row cap.
    assert!(!store
        .cache_get(
            &cache_identity(CacheClass::Discovery, "mouser", "search:q0", None),
            1_800_000,
            now
        )
        .expect("get")
        .is_some());
}

#[test]
fn forbidden_payloads_are_refused_by_type() {
    // Raw markup, cookies, auth headers and secrets never reach a column.
    for hostile in [
        "<!DOCTYPE html><html><body>x</body></html>",
        "Set-Cookie: sessionid=abc; HttpOnly",
        "{\"headers\":\"Authorization: Bearer eyJhbGciOi\"}",
        "{\"refresh_token\":\"rt_secret\"}",
        "{\"api_key\":\"sk-live-123\"}",
    ] {
        match NormalizedPayload::from_json(hostile.to_string()) {
            Err(CommerceStoreError::Forbidden(forbidden)) => {
                assert!(matches!(forbidden.kind, "markup" | "secret" | "header"));
            }
            other => panic!("hostile payload accepted: {other:?}"),
        }
    }
    assert!(NormalizedPayload::from_json("x".repeat(MAX_PAYLOAD_BYTES + 1)).is_err());
    assert!(
        SnapshotDiagnostics::new(vec![("k".to_string(), "Set-Cookie: a=b".to_string())]).is_err()
    );
    assert!(SnapshotDiagnostics::new(vec![("k".to_string(), "x".repeat(300))]).is_err());

    // A normalized payload persists unchanged and nothing raw is written.
    let dir = temp_dir();
    let store = open_store(&dir);
    store
        .record_offer_snapshot(
            &offer("mouser", "TPS5430DDAR", 1_000_000),
            &Default::default(),
            &SnapshotDiagnostics::default(),
            1_000_000,
        )
        .expect("snapshot");
    let conn = raw_conn(&dir);
    let stored: String = conn
        .query_row(
            "SELECT payload_json FROM offer_snapshot LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("stored payload");
    assert!(scan_forbidden_bytes(stored.as_bytes()).is_ok());
    assert!(!stored.contains("<html"));
}

// -------------------------------------------------------------- job tests

struct ScriptedExecutor {
    calls: Mutex<Vec<String>>,
    fail_once: Mutex<Option<(String, SourceError)>>,
    state: JobItemState,
}

impl ScriptedExecutor {
    fn matched() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            fail_once: Mutex::new(None),
            state: JobItemState::Matched,
        }
    }

    fn with_state(state: JobItemState) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            fail_once: Mutex::new(None),
            state,
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls").clone()
    }
}

#[async_trait::async_trait]
impl JobItemExecutor for ScriptedExecutor {
    async fn execute(
        &self,
        _ctx: &faktor_commerce::connector::AcquireCtx,
        _job: &faktor_commerce::jobs::CommerceJob,
        line: &JobLine,
    ) -> Result<ItemOutcome, SourceError> {
        self.calls
            .lock()
            .expect("calls")
            .push(line.item_key.clone());
        let mut fail = self.fail_once.lock().expect("fail");
        if let Some((key, error)) = fail.as_ref() {
            if key == &line.item_key {
                let error = error.clone();
                *fail = None;
                return Err(error);
            }
        }
        drop(fail);
        Ok(ItemOutcome {
            state: self.state,
            label: Some(format!("line {}", line.ordinal)),
            important: if self.state == JobItemState::Matched {
                None
            } else {
                Some(
                    faktor_commerce::result::ImportantEntry::new(
                        &format!("line-{:04}", line.ordinal + 1),
                        "no match",
                    )
                    .expect("entry"),
                )
            },
            detail: NormalizedPayload::from_json(format!(
                "{{\"status\":\"{}\",\"ordinal\":{}}}",
                self.state.as_str(),
                line.ordinal
            ))
            .expect("detail"),
        })
    }
}

#[derive(Default)]
struct FakeArtifacts {
    puts: AtomicU64,
    last: Mutex<Option<Vec<u8>>>,
    fail_puts: AtomicU64,
}

#[async_trait::async_trait]
impl ArtifactStore for FakeArtifacts {
    async fn put(&self, bytes: &[u8]) -> Result<ArtifactRef, SourceError> {
        if self.fail_puts.load(Ordering::SeqCst) > 0 {
            return Err(SourceError::Store);
        }
        self.puts.fetch_add(1, Ordering::SeqCst);
        *self.last.lock().expect("last") = Some(bytes.to_vec());
        Ok(ArtifactRef {
            digest: artifact_digest(bytes),
            bytes: bytes.len() as u64,
        })
    }

    async fn get(&self, digest: &str) -> Result<Option<Vec<u8>>, SourceError> {
        let last = self.last.lock().expect("last");
        match last.as_ref() {
            Some(bytes) if artifact_digest(bytes) == digest => Ok(Some(bytes.clone())),
            _ => Ok(None),
        }
    }
}

fn test_ctx() -> faktor_commerce::connector::AcquireCtx {
    faktor_commerce::connector::AcquireCtx::new()
}

#[test]
fn job_digest_identity_attach_and_new_after_terminal() {
    let dir = temp_dir();
    let store = open_store(&dir);
    let request = bom_request(bom(&[("TPS5430DDAR", 100), ("STM32F407VGT6", 50)]));
    let enabled = vec![source("mouser"), source("lcsc")];
    let profile = ProfileIdentity::new(source("1688"), "procurement-cn").expect("profile");
    let digest = faktor_commerce::jobs::job_digest(&request, &enabled, None);
    assert_eq!(digest.len(), 64);
    // Enabled sources and profile identity are part of the identity.
    assert_ne!(
        digest,
        faktor_commerce::jobs::job_digest(&request, &enabled, Some(&profile))
    );
    assert_ne!(
        digest,
        faktor_commerce::jobs::job_digest(&request, &[source("mouser")], None)
    );
    let mut live = request.clone();
    live.freshness = FreshnessMode::Live;
    assert_ne!(
        digest,
        faktor_commerce::jobs::job_digest(&live, &enabled, None)
    );
    let mut other = request.clone();
    other.account = Some(account("acct-a"));
    assert_ne!(
        digest,
        faktor_commerce::jobs::job_digest(&other, &enabled, None)
    );

    let (job, attached) =
        submit_job(&store, request.clone(), &enabled, None, 1_000).expect("submit");
    assert!(!attached);
    assert_eq!(job.state, JobState::Queued);
    assert_eq!(store.job_item_count(&job.id).expect("items"), 2);
    let (second, attached) =
        submit_job(&store, request.clone(), &enabled, None, 1_001).expect("submit");
    assert!(attached, "identical active request attaches");
    assert_eq!(second.id, job.id);
    assert_eq!(store.job_count(None).expect("jobs"), 1);

    // Drive it to completion; afterwards an identical request is a NEW job.
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let artifacts = FakeArtifacts::default();
    let executor = ScriptedExecutor::matched();
    let outcome = runtime
        .block_on(advance_job(
            &store,
            &test_ctx(),
            &job.id,
            &executor,
            &artifacts,
            2_000,
        ))
        .expect("advance");
    assert_eq!(outcome.job.state, JobState::Completed);
    assert_eq!(outcome.compact.matched, 2);
    let (third, attached) = submit_job(&store, request, &enabled, None, 2_001).expect("submit");
    assert!(!attached, "a terminal job does not absorb a new request");
    assert_ne!(third.id, job.id);
    assert_eq!(store.job_count(None).expect("jobs"), 2);
}

#[test]
fn job_restart_recovery_does_not_repeat_settled_lines() {
    let dir = temp_dir();
    let store = open_store(&dir);
    let lines = [
        ("PART-0000", 10u64),
        ("PART-0001", 10),
        ("PART-0002", 10),
        ("PART-0003", 10),
        ("PART-0004", 10),
    ];
    let request = bom_request(bom(&lines));
    let (job, _) = submit_job(&store, request, &[], None, 1_000).expect("submit");

    // First pass: the first two lines settle, then the acquisition "crashes"
    // (a transient failure) on line 3. The durable state must show 2 settled
    // lines and 3 pending.
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let artifacts = FakeArtifacts::default();
    let first = ScriptedExecutor::matched();
    let crash_key = BomItem::new("PART-0002", 10).expect("line").key();
    *first.fail_once.lock().expect("fail") = Some((crash_key.clone(), SourceError::NetworkTimeout));
    let outcome = runtime
        .block_on(advance_job(
            &store,
            &test_ctx(),
            &job.id,
            &first,
            &artifacts,
            2_000,
        ))
        .expect("first pass");
    assert_eq!(outcome.job.state, JobState::Running);
    assert_eq!(outcome.compact.status, CompactStatus::Running);
    assert_eq!(artifacts.puts.load(Ordering::SeqCst), 0);
    assert_eq!(
        first.calls(),
        vec![
            BomItem::new("PART-0000", 10).expect("line").key(),
            BomItem::new("PART-0001", 10).expect("line").key(),
            crash_key.clone(),
        ]
    );
    let settled: Vec<JobItemState> = store
        .job_items(&job.id)
        .expect("items")
        .into_iter()
        .map(|item| item.state)
        .collect();
    assert_eq!(
        settled,
        vec![
            JobItemState::Matched,
            JobItemState::Matched,
            JobItemState::Pending,
            JobItemState::Pending,
            JobItemState::Pending,
        ]
    );

    // Crash: drop the store and reopen the same database.
    drop(store);
    let store = open_store(&dir);
    let pending = store.pending_jobs().expect("pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].job_id, job.id);

    // Second pass continues deterministically: settled lines are NOT
    // re-acquired; only the pending lines run (the in-flight line is
    // retried, the documented at-least-once boundary).
    let second = ScriptedExecutor::matched();
    let outcome = runtime
        .block_on(advance_job(
            &store,
            &test_ctx(),
            &job.id,
            &second,
            &artifacts,
            3_000,
        ))
        .expect("second pass");
    assert_eq!(outcome.job.state, JobState::Completed);
    assert_eq!(outcome.compact.matched, 5);
    assert_eq!(outcome.compact.lines, 5);
    assert_eq!(
        second.calls(),
        vec![
            crash_key,
            BomItem::new("PART-0003", 10).expect("line").key(),
            BomItem::new("PART-0004", 10).expect("line").key(),
        ],
        "no settled line was re-executed"
    );
    assert_eq!(artifacts.puts.load(Ordering::SeqCst), 1);
    assert!(store.pending_jobs().expect("pending").is_empty());
    // Replaying the finished job is a no-op (idempotent re-entry).
    let replay = runtime
        .block_on(advance_job(
            &store,
            &test_ctx(),
            &job.id,
            &ScriptedExecutor::matched(),
            &artifacts,
            4_000,
        ))
        .expect("replay");
    assert_eq!(replay.job.state, JobState::Completed);
    assert_eq!(
        artifacts.puts.load(Ordering::SeqCst),
        1,
        "no duplicate artifact"
    );
}

#[test]
fn bom_job_artifact_and_compact_contract() {
    let dir = temp_dir();
    let store = open_store(&dir);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let artifacts = FakeArtifacts::default();

    // 500 lines: the full result goes to the CAS artifact; the compact
    // result holds the hard bound.
    let lines: Vec<(String, u64)> = (0..500).map(|i| (format!("PART-{i:04}"), 100)).collect();
    let pairs: Vec<(&str, u64)> = lines.iter().map(|(q, n)| (q.as_str(), *n)).collect();
    let request = bom_request(bom(&pairs));
    let (job, _) = submit_job(&store, request, &[], None, 1_000).expect("submit");
    let outcome = runtime
        .block_on(advance_job(
            &store,
            &test_ctx(),
            &job.id,
            &ScriptedExecutor::matched(),
            &artifacts,
            2_000,
        ))
        .expect("advance");
    assert_eq!(outcome.compact.lines, 500);
    assert_eq!(outcome.compact.matched, 500);
    assert_eq!(outcome.compact.status, CompactStatus::Completed);
    let artifact = outcome.compact.artifact.as_ref().expect("artifact");
    artifact.validate().expect("digest and bound");
    assert!(artifact.bytes as usize <= MAX_ARTIFACT_BYTES);
    let bytes = artifacts.last.lock().expect("last").clone().expect("bytes");
    assert_eq!(artifact_digest(&bytes), artifact.digest);
    let compact_json = serde_json::to_string(&outcome.compact).expect("json");
    assert!(compact_json.len() <= COMPACT_RESULT_HARD_MAX_BYTES);
    assert!(scan_forbidden_bytes(&bytes).is_ok());

    // 300 ambiguous lines: important entries are capped and trimming is
    // reported honestly.
    let lines: Vec<(String, u64)> = (0..300).map(|i| (format!("AMB-{i:04}"), 1)).collect();
    let pairs: Vec<(&str, u64)> = lines.iter().map(|(q, n)| (q.as_str(), *n)).collect();
    let request = bom_request(bom(&pairs));
    let (job, _) = submit_job(&store, request, &[], None, 1_000).expect("submit");
    let outcome = runtime
        .block_on(advance_job(
            &store,
            &test_ctx(),
            &job.id,
            &ScriptedExecutor::with_state(JobItemState::Ambiguous),
            &artifacts,
            2_000,
        ))
        .expect("advance");
    assert_eq!(outcome.compact.ambiguous, 300);
    assert!(outcome.compact.important.len() <= 24);
    let compact_json = serde_json::to_string(&outcome.compact).expect("json");
    assert!(compact_json.len() <= COMPACT_RESULT_HARD_MAX_BYTES);
}

#[test]
fn planted_secret_never_reaches_an_artifact() {
    // The type-level door: a planted secret cannot be constructed as a
    // normalized payload at all.
    let error =
        NormalizedPayload::from_json("{\"description\":\"refresh_token: rt_abc123\"}".to_string())
            .expect_err("secret refused");
    assert!(matches!(error, CommerceStoreError::Forbidden(_)));

    // End to end: a hostile connector observation is refused before any
    // cache row or artifact byte exists.
    let dir = temp_dir();
    let store = open_store(&dir);
    let request = bom_request(bom(&[("SECRETIVE", 5)]));
    let (job, _) = submit_job(&store, request, &[], None, 1_000).expect("submit");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let artifacts = FakeArtifacts::default();
    let executor = SecretExecutor;
    let outcome = runtime
        .block_on(advance_job(
            &store,
            &test_ctx(),
            &job.id,
            &executor,
            &artifacts,
            2_000,
        ))
        .expect("advance");
    assert_eq!(outcome.job.state, JobState::Completed);
    assert_eq!(outcome.compact.unmatched, 1);
    assert_eq!(store.cache_count().expect("cache"), 0);
    let compact_json = serde_json::to_string(&outcome.compact).expect("json");
    assert!(!compact_json.contains("refresh_token"));
    let artifact_bytes = artifacts.last.lock().expect("last").clone();
    if let Some(bytes) = artifact_bytes {
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains("refresh_token"));
        assert!(!text.contains("rt_abc123"));
        assert!(scan_forbidden_bytes(&bytes).is_ok());
    }
}

/// An executor that tries to plant a secret and is refused typed.
struct SecretExecutor;

#[async_trait::async_trait]
impl JobItemExecutor for SecretExecutor {
    async fn execute(
        &self,
        _ctx: &faktor_commerce::connector::AcquireCtx,
        _job: &faktor_commerce::jobs::CommerceJob,
        line: &JobLine,
    ) -> Result<ItemOutcome, SourceError> {
        let _ = line;
        let detail =
            NormalizedPayload::from_json("{\"note\":\"refresh_token: rt_abc123\"}".to_string())
                .map_err(|_| SourceError::Store)?;
        Ok(ItemOutcome {
            state: JobItemState::Matched,
            label: None,
            important: None,
            detail,
        })
    }
}

#[test]
fn schema_has_every_table_index_wal_and_pragmas() {
    let dir = temp_dir();
    let store = open_store(&dir);
    let conn = raw_conn(&dir);
    let journal: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("journal mode");
    assert_eq!(journal.to_ascii_lowercase(), "wal");
    let synchronous: i64 = conn
        .query_row("PRAGMA synchronous", [], |row| row.get(0))
        .expect("synchronous");
    assert_eq!(synchronous, 2, "synchronous = FULL");

    let mut tables: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .expect("prepare");
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query");
        rows.map(|row| row.expect("row")).collect()
    };
    tables.sort();
    for required in [
        "product_identity",
        "source_product",
        "offer_snapshot",
        "variant_snapshot",
        "price_break",
        "stock_snapshot",
        "supplier",
        "supplier_snapshot",
        "search_cache",
        "connector_state",
        "browser_profile_state",
        "egress_state",
        "job",
        "job_item",
        "challenge",
    ] {
        assert!(
            tables.iter().any(|name| name == required),
            "missing table {required}"
        );
    }

    let mut indexes: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'index' ORDER BY name")
            .expect("prepare");
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query");
        rows.map(|row| row.expect("row")).collect()
    };
    indexes.sort();
    for required in [
        "idx_product_identity_mpn",
        "idx_product_identity_manufacturer",
        "idx_source_product_sku",
        "idx_search_cache_expiry",
        "idx_job_digest",
        "idx_offer_snapshot_observed",
        "idx_stock_snapshot_observed",
    ] {
        assert!(
            indexes.iter().any(|name| name == required),
            "missing index {required}"
        );
    }
    drop(conn);
    assert!(store.size_bytes().expect("size") > 0);
}

#[test]
fn snapshot_persists_normalized_data_provenance_versions_and_diagnostics() {
    let dir = temp_dir();
    let store = open_store(&dir);
    let mut offer = offer("mouser", "TPS5430DDAR", 1_000_000);
    offer.variants = vec![faktor_commerce::VariantOffer {
        variant_id: faktor_commerce::VariantId::new("reel-5000").expect("variant"),
        attributes: vec![faktor_commerce::VariantAttribute {
            name: Text::<64>::new("packaging").expect("name"),
            value: Text::<256>::new("reel").expect("value"),
        }],
        packaging: Some(PackagingType::TapeAndReel),
        moq: None,
        order_multiple: None,
        standard_pack: None,
        stock: StockState::InStock {
            quantity: NonZeroQuantity::new(5_000).expect("qty"),
        },
        price_breaks: vec![PriceBreak {
            min_quantity: NonZeroQuantity::new(1).expect("qty"),
            max_quantity: None,
            unit_price: Money::from_micros(Currency::USD, 17_500),
            visibility: PriceVisibility::Public,
            account_scope: None,
            promotion: None,
        }],
        lead_time: None,
    }];
    offer.packaging = vec![faktor_commerce::PackagingOption {
        packaging: PackagingType::TapeAndReel,
        label: None,
        moq: None,
        order_multiple: None,
        standard_pack: None,
        stock: StockState::InStock {
            quantity: NonZeroQuantity::new(5_000).expect("qty"),
        },
        price_breaks: vec![PriceBreak {
            min_quantity: NonZeroQuantity::new(1).expect("qty"),
            max_quantity: None,
            unit_price: Money::from_micros(Currency::USD, 17_400),
            visibility: PriceVisibility::Public,
            account_scope: None,
            promotion: None,
        }],
        lead_time: None,
    }];
    offer.supplier = Some(faktor_commerce::Supplier {
        id: Some(Text::<128>::new("sup-1").expect("id")),
        name: Text::<256>::new("Mouser Electronics").expect("name"),
        country: Some(Text::<64>::new("US").expect("country")),
        url: None,
        account_scope: None,
    });
    let diagnostics =
        SnapshotDiagnostics::new(vec![("origin".to_string(), "official_api".to_string())])
            .expect("diagnostics");
    store
        .record_offer_snapshot(
            &offer,
            &faktor_commerce::SnapshotVersions {
                extractor_version: Some("extractor-9".to_string()),
                connector_version: Some("connector-4".to_string()),
                normalization_version: Some("normalization-7".to_string()),
            },
            &diagnostics,
            1_000_000,
        )
        .expect("snapshot");

    let conn = raw_conn(&dir);
    let (digest, origin, connector_version, normalization_version, diagnostics_json): (
        String,
        String,
        Option<String>,
        Option<String>,
        String,
    ) = conn
        .query_row(
            "SELECT content_digest, origin, connector_version, normalization_version, diagnostics_json
             FROM offer_snapshot LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .expect("snapshot row");
    assert_eq!(digest.len(), 64);
    assert_eq!(origin, "official_api");
    assert_eq!(connector_version.as_deref(), Some("connector-4"));
    assert_eq!(normalization_version.as_deref(), Some("normalization-7"));
    assert!(diagnostics_json.contains("origin"));
    for (table, expected) in [
        ("variant_snapshot", 1i64),
        ("price_break", 3),
        ("stock_snapshot", 1),
        ("supplier", 1),
        ("supplier_snapshot", 1),
        ("product_identity", 1),
        ("source_product", 1),
    ] {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count");
        assert_eq!(count, expected, "unexpected row count in {table}");
    }
    // Normalized data parses back exactly.
    let snapshot = store
        .latest_offer_snapshot(&source("mouser"), "offer-12345")
        .expect("latest")
        .expect("row");
    let restored: CommercialOffer = snapshot.payload.parse().expect("parse");
    assert_eq!(restored, offer);
}

#[test]
fn durable_connector_profile_egress_and_challenge_state_round_trips() {
    let dir = temp_dir();
    let store = open_store(&dir);
    let now = now_ms();
    store
        .save_connector_state(&faktor_commerce::ConnectorStateRow {
            source: source("mouser"),
            health: faktor_commerce::ConnectorHealth::CoolingDown {
                until_ms: now + 60_000,
            },
            last_error: Some("network_timeout".to_string()),
            api_open_until_ms: Some(now + 60_000),
            browser_open_until_ms: None,
            failures: 3,
            updated_ms: now,
        })
        .expect("connector state");
    let state = store
        .connector_state(&source("mouser"))
        .expect("read")
        .expect("row");
    assert_eq!(state.failures, 3);
    assert!(matches!(
        state.health,
        faktor_commerce::ConnectorHealth::CoolingDown { .. }
    ));

    store
        .save_browser_profile_state(&faktor_commerce::BrowserProfileStateRow {
            source: source("1688"),
            profile: Text::<64>::new("procurement-cn").expect("profile"),
            account_scope: Some(account("acct-a")),
            egress: Some(Text::<64>::new("egress-cn").expect("egress")),
            verification: Some(Text::<256>::new("captcha pending").expect("verification")),
            last_used_ms: Some(now),
            updated_ms: now,
        })
        .expect("profile state");
    let profiles = store
        .browser_profile_states(&source("1688"))
        .expect("profiles");
    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0].profile.as_str(), "procurement-cn");

    store
        .save_egress_state(&faktor_commerce::EgressStateRow {
            egress: Text::<64>::new("egress-cn").expect("egress"),
            healthy: false,
            note: Some(Text::<256>::new("broker unreachable").expect("note")),
            last_used_ms: Some(now),
            updated_ms: now,
        })
        .expect("egress state");
    let egress = store.egress_state("egress-cn").expect("read").expect("row");
    assert!(!egress.healthy);

    let challenge = store
        .record_challenge(
            &source("1688"),
            Some("procurement-cn"),
            "captcha",
            "challenge-1",
            now,
        )
        .expect("challenge");
    assert!(challenge > 0);
    assert_eq!(store.open_challenges().expect("open").len(), 1);
    assert_eq!(
        store
            .resolve_challenges(&source("1688"), Some("procurement-cn"), now)
            .expect("resolve"),
        1
    );
    assert!(store.open_challenges().expect("open").is_empty());

    // A hostile challenge label is refused (bounded text).
    assert!(store
        .record_challenge(&source("1688"), None, "", "k", now)
        .is_err());
}

// ---------------------------------------------------------- service tests

struct FakeConnector {
    source: SourceId,
    behavior: Mutex<Behavior>,
    discover_calls: AtomicU64,
    product_calls: AtomicU64,
    quote_calls: AtomicU64,
}

struct Behavior {
    discoveries: Vec<Discovery>,
    product: Option<CommercialOffer>,
    candidates: Vec<QuoteCandidate>,
    fail: bool,
    delay_ms: u64,
}

impl FakeConnector {
    fn new(source_id: &str, behavior: Behavior) -> Arc<Self> {
        Arc::new(Self {
            source: source(source_id),
            behavior: Mutex::new(behavior),
            discover_calls: AtomicU64::new(0),
            product_calls: AtomicU64::new(0),
            quote_calls: AtomicU64::new(0),
        })
    }

    fn healthy(source_id: &str) -> Arc<Self> {
        Self::new(
            source_id,
            Behavior {
                discoveries: vec![discovery(source_id, "TPS5430DDAR")],
                product: Some(offer(source_id, "TPS5430DDAR", 1_000_000)),
                candidates: vec![quote_candidate(source_id, "TPS5430DDAR", 100)],
                fail: false,
                delay_ms: 0,
            },
        )
    }

    fn set_fail(&self, fail: bool) {
        self.behavior.lock().expect("behavior").fail = fail;
    }

    fn calls(&self) -> (u64, u64, u64) {
        (
            self.discover_calls.load(Ordering::SeqCst),
            self.product_calls.load(Ordering::SeqCst),
            self.quote_calls.load(Ordering::SeqCst),
        )
    }
}

#[async_trait::async_trait]
impl CommerceConnector for FakeConnector {
    fn source(&self) -> SourceId {
        self.source.clone()
    }

    fn capabilities(&self) -> faktor_commerce::connector::ConnectorCapabilities {
        faktor_commerce::connector::ConnectorCapabilities {
            discovery: true,
            exact_product: true,
            quantity_pricing: true,
            stock: true,
            packaging: true,
            account_pricing: true,
            supplier_data: true,
            bulk: true,
            mechanisms: vec![faktor_commerce::connector::AcquisitionMechanism::OfficialApi],
        }
    }

    async fn discover(
        &self,
        _ctx: &faktor_commerce::connector::AcquireCtx,
        _req: SearchRequest,
    ) -> Result<Vec<Discovery>, SourceError> {
        self.discover_calls.fetch_add(1, Ordering::SeqCst);
        let (delay_ms, fail, discoveries) = {
            let behavior = self.behavior.lock().expect("behavior");
            (
                behavior.delay_ms,
                behavior.fail,
                behavior.discoveries.clone(),
            )
        };
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
        if fail {
            return Err(SourceError::ApiUnavailable);
        }
        Ok(discoveries)
    }

    async fn product(
        &self,
        _ctx: &faktor_commerce::connector::AcquireCtx,
        _req: ProductRequest,
    ) -> Result<CommercialOffer, SourceError> {
        self.product_calls.fetch_add(1, Ordering::SeqCst);
        let behavior = self.behavior.lock().expect("behavior");
        if behavior.fail {
            return Err(SourceError::ApiUnavailable);
        }
        behavior.product.clone().ok_or(SourceError::ProductNotFound)
    }

    async fn quote(
        &self,
        _ctx: &faktor_commerce::connector::AcquireCtx,
        _req: QuoteRequest,
    ) -> Result<Vec<QuoteCandidate>, SourceError> {
        self.quote_calls.fetch_add(1, Ordering::SeqCst);
        let behavior = self.behavior.lock().expect("behavior");
        if behavior.fail {
            return Err(SourceError::ApiUnavailable);
        }
        Ok(behavior.candidates.clone())
    }
}

fn search_request(query: &str) -> SearchRequest {
    search_request_mode(query, FreshnessMode::PreferCache)
}

fn search_request_mode(query: &str, mode: FreshnessMode) -> SearchRequest {
    SearchRequest::new(query, SourceSet::auto(), 10, mode, DetailLevel::Compact)
        .expect("search request")
}

fn enabled_service(
    dir: &tempfile::TempDir,
    connector: Arc<FakeConnector>,
) -> (Arc<CommerceSourceService>, Arc<FakeArtifacts>) {
    let artifacts = Arc::new(FakeArtifacts::default());
    let service =
        CommerceSourceService::open(dir.path(), ServiceConfig::default(), artifacts.clone())
            .expect("service");
    service
        .register(connector, ConnectorPolicy::default())
        .expect("register");
    (service, artifacts)
}

#[test]
fn disabled_service_creates_nothing_and_calls_nothing() {
    let dir = temp_dir();
    // Construction with a disabled config touches neither the directory nor
    // any connector/client state.
    let artifacts = Arc::new(FakeArtifacts::default());
    let service =
        CommerceSourceService::open(dir.path(), ServiceConfig::disabled(), artifacts.clone())
            .expect("disabled service");
    assert!(!service.is_enabled());
    assert!(service.store().is_none());
    assert!(service.data_dir().is_none());
    assert_eq!(
        std::fs::read_dir(dir.path()).expect("read dir").count(),
        0,
        "disabled construction creates no file or directory"
    );

    let connector = FakeConnector::healthy("mouser");
    service
        .register(connector.clone(), ConnectorPolicy::default())
        .expect("register on disabled service is inert");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let ctx = test_ctx();
    let outcomes = runtime.block_on(async {
        (
            service.search(&ctx, search_request("TPS5430")).await,
            service
                .product(
                    &ctx,
                    ProductRequest::new(
                        ProductRef::parse("TPS5430DDAR", None).expect("ref"),
                        FreshnessMode::Live,
                        DetailLevel::Compact,
                    )
                    .expect("request"),
                )
                .await,
            service
                .quote(
                    &ctx,
                    QuoteRequest::new(
                        ProductRef::parse("TPS5430DDAR", None).expect("ref"),
                        100,
                        None,
                        None,
                        None,
                        FreshnessMode::Live,
                    )
                    .expect("request"),
                )
                .await,
            service
                .bom(
                    &ctx,
                    SourceSet::auto(),
                    FreshnessMode::PreferCache,
                    DetailLevel::Compact,
                    bom(&[("TPS5430DDAR", 100)]),
                )
                .await,
        )
    });
    assert_eq!(outcomes.0, Err(ServiceError::Disabled));
    assert_eq!(outcomes.1, Err(ServiceError::Disabled));
    assert_eq!(outcomes.2, Err(ServiceError::Disabled));
    assert_eq!(outcomes.3, Err(ServiceError::Disabled));
    assert_eq!(connector.calls(), (0, 0, 0), "no network path was touched");
    assert_eq!(artifacts.puts.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read_dir(dir.path()).expect("read dir").count(), 0);

    // The bare disabled constructor is equally inert.
    let bare = CommerceSourceService::disabled();
    assert!(runtime
        .block_on(bare.search(&ctx, search_request("x")))
        .is_err());
    assert_eq!(std::fs::read_dir(dir.path()).expect("read dir").count(), 0);
}

#[tokio::test(start_paused = true)]
async fn service_deadline_propagates() {
    let dir = temp_dir();
    let connector = FakeConnector::new(
        "mouser",
        Behavior {
            discoveries: vec![discovery("mouser", "TPS5430DDAR")],
            product: None,
            candidates: Vec::new(),
            fail: false,
            delay_ms: 10_000,
        },
    );
    let (service, _artifacts) = enabled_service(&dir, connector.clone());
    let ctx = test_ctx().with_deadline(Duration::from_millis(100));
    let error = service
        .search(&ctx, search_request("TPS5430"))
        .await
        .expect_err("deadline");
    assert_eq!(error, ServiceError::Source(SourceError::Deadline));
    assert_eq!(connector.calls().0, 1, "the call started once");

    // A cancelled context surfaces Cancelled, not a hang.
    let cancelled = test_ctx();
    cancelled.cancel.cancel();
    let error = service
        .search(&cancelled, search_request("TPS5430"))
        .await
        .expect_err("cancelled");
    assert_eq!(error, ServiceError::Source(SourceError::Cancelled));
}

#[tokio::test(start_paused = true)]
async fn service_coalesces_identical_concurrent_requests() {
    let dir = temp_dir();
    let connector = FakeConnector::new(
        "mouser",
        Behavior {
            discoveries: vec![discovery("mouser", "TPS5430DDAR")],
            product: None,
            candidates: Vec::new(),
            fail: false,
            delay_ms: 50,
        },
    );
    let (service, _artifacts) = enabled_service(&dir, connector.clone());
    let mut handles = Vec::new();
    for _ in 0..6 {
        let service = service.clone();
        handles.push(tokio::spawn(async move {
            let ctx = test_ctx().with_freshness(FreshnessMode::Live);
            service
                .search(&ctx, search_request_mode("TPS5430", FreshnessMode::Live))
                .await
        }));
    }
    for handle in handles {
        let outcome = handle.await.expect("join").expect("search");
        assert_eq!(outcome.discoveries.len(), 1);
    }
    assert_eq!(
        connector.calls().0,
        1,
        "one external request for N awaiters"
    );
}

#[tokio::test(start_paused = true)]
async fn service_freshness_semantics_prefer_cache_live_cache_only() {
    let dir = temp_dir();
    let connector = FakeConnector::healthy("mouser");
    let (service, _artifacts) = enabled_service(&dir, connector.clone());
    let store = service.store().expect("store").clone();
    let now = now_ms();

    // 1. Live acquisition populates the discovery cache.
    let live_ctx = test_ctx().with_freshness(FreshnessMode::Live);
    let live_request = search_request_mode("TPS5430", FreshnessMode::Live);
    let outcome = service
        .search(&live_ctx, live_request.clone())
        .await
        .expect("live");
    assert_eq!(outcome.freshness, Some(Freshness::Live));
    assert_eq!(connector.calls().0, 1);

    // 2. PreferCache answers from the fresh row without a second call.
    let cache_ctx = test_ctx();
    let outcome = service
        .search(&cache_ctx, search_request("TPS5430"))
        .await
        .expect("cached");
    assert!(matches!(
        outcome.freshness,
        Some(Freshness::FreshCache { .. })
    ));
    assert_eq!(connector.calls().0, 1, "cache hit made no request");

    // 3. Live forces a fresh call even with a warm cache.
    let _ = service
        .search(&live_ctx, live_request.clone())
        .await
        .expect("forced live");
    assert_eq!(connector.calls().0, 2);

    // 4. An expired row is never presented as current. Writing an old row
    // directly simulates the TTL elapsing.
    let identity = cache_identity(CacheClass::Discovery, "mouser", "search:tps5430", None);
    let stale_observed = now.saturating_sub(100_000_000);
    store
        .cache_put(&NewCacheRow {
            identity: identity.clone(),
            payload: NormalizedPayload::from_serializable(&vec![discovery(
                "mouser",
                "TPS5430DDAR",
            )])
            .expect("payload"),
            validators: ConditionalValidators::default(),
            observed_at_ms: stale_observed,
        })
        .expect("put stale");
    let calls_before = connector.calls().0;
    connector.set_fail(true);
    let error = service
        .search(&live_ctx, live_request)
        .await
        .expect_err("live never falls back");
    assert_eq!(error, ServiceError::Source(SourceError::ApiUnavailable));
    let fallback = service
        .search(&cache_ctx, search_request("TPS5430"))
        .await
        .expect("prefer_cache falls back");
    match fallback.freshness {
        Some(freshness) => {
            assert!(freshness.is_stale(), "stale is labeled stale");
            assert!(!freshness.is_current(), "stale is never current");
        }
        None => panic!("expected a stale fallback freshness"),
    }
    assert!(connector.calls().0 > calls_before);

    // 5. cache_only never touches the network; a miss is typed.
    let cache_only = test_ctx().with_freshness(FreshnessMode::CacheOnly);
    let outcome = service
        .search(
            &cache_only,
            search_request_mode("TPS5430", FreshnessMode::CacheOnly),
        )
        .await
        .expect("cache_only stale fallback");
    assert!(outcome.freshness.expect("freshness").is_stale());
    let calls = connector.calls().0;
    let error = service
        .search(
            &cache_only,
            search_request_mode("never seen part", FreshnessMode::CacheOnly),
        )
        .await
        .expect_err("cache miss");
    assert!(matches!(error, ServiceError::CacheMiss { .. }));
    assert_eq!(connector.calls().0, calls, "cache_only made no request");
}

#[tokio::test(start_paused = true)]
async fn service_account_scope_isolation() {
    let dir = temp_dir();
    let connector = FakeConnector::healthy("mouser");
    let (service, _artifacts) = enabled_service(&dir, connector.clone());
    let account_a = account("acct-a");
    let account_b = account("acct-b");
    let request = QuoteRequest::new(
        ProductRef::parse("TPS5430DDAR", None).expect("ref"),
        100,
        None,
        None,
        None,
        FreshnessMode::PreferCache,
    )
    .expect("request");

    // Account A acquires live and caches its account price.
    let ctx_a = test_ctx().with_account(account_a.clone());
    let outcome = service
        .quote(&ctx_a, request.clone())
        .await
        .expect("quote a");
    assert_eq!(outcome.account, Some(account_a.clone()));
    assert_eq!(outcome.freshness, Freshness::Live);
    assert_eq!(connector.calls().2, 1);

    // Account A is served from its own cache...
    let outcome = service
        .quote(&ctx_a, request.clone())
        .await
        .expect("cached a");
    assert!(matches!(outcome.freshness, Freshness::FreshCache { .. }));
    assert_eq!(connector.calls().2, 1);

    // ...while account B is NOT: the connector now fails, and B's cache
    // lookup must miss rather than leak A's account price.
    connector.set_fail(true);
    let ctx_b = test_ctx().with_account(account_b);
    let error = service
        .quote(&ctx_b, request)
        .await
        .expect_err("account B must not see account A's price");
    assert_eq!(error, ServiceError::Source(SourceError::ApiUnavailable));
}

#[tokio::test(start_paused = true)]
async fn service_freshness_never_labels_a_miss_as_data() {
    let dir = temp_dir();
    let connector = FakeConnector::healthy("mouser");
    let (service, _artifacts) = enabled_service(&dir, connector);
    // cache_only with an empty cache is a typed miss for every operation.
    let ctx = test_ctx().with_freshness(FreshnessMode::CacheOnly);
    assert!(matches!(
        service
            .search(
                &ctx,
                search_request_mode("nothing cached", FreshnessMode::CacheOnly),
            )
            .await
            .expect_err("miss"),
        ServiceError::CacheMiss {
            class: CacheClass::Discovery
        }
    ));
    assert!(matches!(
        service
            .product(
                &ctx,
                ProductRequest::new(
                    ProductRef::parse("TPS5430DDAR", None).expect("ref"),
                    FreshnessMode::CacheOnly,
                    DetailLevel::Compact,
                )
                .expect("request"),
            )
            .await
            .expect_err("miss"),
        ServiceError::CacheMiss {
            class: CacheClass::Product
        }
    ));
    assert!(matches!(
        service
            .quote(
                &ctx,
                QuoteRequest::new(
                    ProductRef::parse("TPS5430DDAR", None).expect("ref"),
                    100,
                    None,
                    None,
                    None,
                    FreshnessMode::CacheOnly,
                )
                .expect("request"),
            )
            .await
            .expect_err("miss"),
        ServiceError::CacheMiss {
            class: CacheClass::Price
        }
    ));
}

#[tokio::test(start_paused = true)]
async fn service_bom_job_flows_through_connectors_and_artifacts() {
    let dir = temp_dir();
    let connector = FakeConnector::healthy("mouser");
    let (service, artifacts) = enabled_service(&dir, connector.clone());
    let ctx = test_ctx();
    let outcome = service
        .bom(
            &ctx,
            SourceSet::auto(),
            FreshnessMode::PreferCache,
            DetailLevel::Compact,
            bom(&[("TPS5430DDAR", 100), ("TPS5430DDAR", 500)]),
        )
        .await
        .expect("bom");
    assert_eq!(outcome.job.state, JobState::Completed);
    assert_eq!(outcome.compact.lines, 2);
    assert_eq!(outcome.compact.matched, 2);
    assert!(outcome.compact.artifact.is_some());
    assert_eq!(artifacts.puts.load(Ordering::SeqCst), 1);

    // The deterministic `job` operation returns the stored compact result.
    let status = service.job_status(&outcome.job.id).expect("status");
    assert_eq!(status.state, JobState::Completed);
    assert_eq!(status.compact.matched, 2);
    assert_eq!(status.compact.lines, 2);

    // Coalescing at the line level: the second identical quote came from
    // the price cache, so the connector was called once per distinct
    // (reference, quantity).
    assert_eq!(connector.calls().2, 2);
}

#[tokio::test(start_paused = true)]
async fn service_rejects_out_of_bounds_requests() {
    let dir = temp_dir();
    let connector = FakeConnector::healthy("mouser");
    let (service, _artifacts) = enabled_service(&dir, connector.clone());
    let ctx = test_ctx();

    // A limit outside 1..=50 is refused by per-operation validation.
    let mut invalid = search_request("TPS5430");
    invalid.limit = 51;
    assert_eq!(
        service.search(&ctx, invalid).await.expect_err("limit"),
        ServiceError::Source(SourceError::InvalidRequest)
    );

    // A BOM with 501 lines cannot be built through `Bom::new`, but hostile
    // input can still be deserialized; validation must catch it.
    let items: Vec<serde_json::Value> = (0..501)
        .map(|i| serde_json::json!({"q": format!("PART-{i:04}"), "qty": 1}))
        .collect();
    let oversized: faktor_commerce::bom::Bom =
        serde_json::from_value(serde_json::Value::Array(items)).expect("hostile bom");
    let error = service
        .bom(
            &ctx,
            SourceSet::auto(),
            FreshnessMode::PreferCache,
            DetailLevel::Compact,
            oversized,
        )
        .await
        .expect_err("oversized bom");
    assert_eq!(error, ServiceError::Source(SourceError::InvalidRequest));

    // The typed field bounds hold at construction.
    assert!(Text::<512>::new(&"q".repeat(513)).is_err());
    assert!(faktor_commerce::query::SearchRequest::new(
        &"q".repeat(513),
        SourceSet::auto(),
        10,
        FreshnessMode::Live,
        DetailLevel::Compact
    )
    .is_err());
    assert!(BomItem::new("part", 1_000_000_001).is_err());
    assert!(BomItem::new("part", 0).is_err());
    assert_eq!(connector.calls().0, 0, "nothing reached a connector");
    let _ = PackagingType::CutTape;
    let _ = VariantId::new("reel-5000").expect("variant");
    let _ = SingleFlight::<String, u64>::new();
    let _ = Capability::Discovery;
    let _ = AcquisitionPath::Api;
}

#[test]
fn service_restart_resumes_pending_jobs() {
    let dir = temp_dir();
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let artifacts = Arc::new(FakeArtifacts::default());
    let job_id = {
        let service =
            CommerceSourceService::open(dir.path(), ServiceConfig::default(), artifacts.clone())
                .expect("service");
        let connector = FakeConnector::healthy("mouser");
        service
            .register(connector, ConnectorPolicy::default())
            .expect("register");
        // Enqueue a job without advancing it: it is queued when the daemon
        // process dies.
        let request = bom_request(bom(&[("TPS5430DDAR", 100)]));
        let store = service.store().expect("store").clone();
        let (job, _) = submit_job(&store, request, &[], None, 1_000).expect("submit");
        job.id
    };
    // "Restart": a fresh service over the same data dir resumes the job.
    let service =
        CommerceSourceService::open(dir.path(), ServiceConfig::default(), artifacts.clone())
            .expect("service restart");
    service
        .register(FakeConnector::healthy("mouser"), ConnectorPolicy::default())
        .expect("register");
    let outcomes = runtime
        .block_on(service.resume_pending(&test_ctx()))
        .expect("resume");
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].job.id, job_id);
    assert_eq!(outcomes[0].job.state, JobState::Completed);
    assert_eq!(outcomes[0].compact.matched, 1);
    let status = service.job_status(&job_id).expect("status");
    assert_eq!(status.state, JobState::Completed);
    assert!(runtime
        .block_on(service.resume_pending(&test_ctx()))
        .expect("idempotent resume")
        .is_empty());
    assert_eq!(artifacts.puts.load(Ordering::SeqCst), 1);
}

#[test]
fn service_gc_and_clear_cache_are_admin_safe() {
    let dir = temp_dir();
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let artifacts = Arc::new(FakeArtifacts::default());
    let service = CommerceSourceService::open(dir.path(), ServiceConfig::default(), artifacts)
        .expect("service");
    service
        .register(FakeConnector::healthy("mouser"), ConnectorPolicy::default())
        .expect("register");
    let ctx = test_ctx();
    runtime
        .block_on(service.search(&ctx, search_request("TPS5430")))
        .expect("search");
    assert!(
        service
            .store()
            .expect("store")
            .cache_count()
            .expect("count")
            > 0
    );
    let removed = service.clear_cache().expect("clear");
    assert!(removed > 0);
    assert_eq!(
        service
            .store()
            .expect("store")
            .cache_count()
            .expect("count"),
        0
    );
    let report = service.gc(now_ms()).expect("gc");
    assert!(report.bytes_after <= report.bytes_before);

    // Disabled admin operations are typed refusals.
    let disabled = CommerceSourceService::disabled();
    assert_eq!(disabled.gc(now_ms()), Err(ServiceError::Disabled));
    assert_eq!(disabled.clear_cache(), Err(ServiceError::Disabled));
}
