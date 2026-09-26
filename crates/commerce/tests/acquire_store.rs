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
    advance_job, bom_line_key, job_digest, submit_job, CommerceJobRequest, CommercePrincipal,
    ItemOutcome, JobItemExecutor, JobItemState, JobLine, JobState, JobWork, MAX_JOB_ITEM_ATTEMPTS,
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
    CommerceStore, CommerceStoreError, GcPolicy, JobItemRow, NewCacheRow, NormalizedPayload,
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

fn principal() -> CommercePrincipal {
    principal_for(1, 1, None)
}

fn principal_for(workspace: u64, session: u64, scope: Option<&str>) -> CommercePrincipal {
    CommercePrincipal::new(
        faktor_core::WorkspaceId::new(workspace),
        faktor_core::SessionId::new(session),
        scope.map(account),
    )
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

/// The stored `(pricing_scope, account_scope)` of the newest cache row of
/// `class` — the observation authority a write actually recorded.
fn last_cache_scope(dir: &tempfile::TempDir, class: &str) -> (String, Option<String>) {
    let conn = raw_conn(dir);
    conn.query_row(
        "SELECT pricing_scope, account_scope FROM search_cache WHERE class = ?1
         ORDER BY rowid DESC LIMIT 1",
        rusqlite::params![class],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
    )
    .expect("a cache row was written")
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
    let digest = faktor_commerce::jobs::job_digest(&request, &enabled, None, &principal());
    assert_eq!(digest.len(), 64);
    // Enabled sources and profile identity are part of the identity.
    assert_ne!(
        digest,
        faktor_commerce::jobs::job_digest(&request, &enabled, Some(&profile), &principal())
    );
    assert_ne!(
        digest,
        faktor_commerce::jobs::job_digest(&request, &[source("mouser")], None, &principal())
    );
    let mut live = request.clone();
    live.freshness = FreshnessMode::Live;
    assert_ne!(
        digest,
        faktor_commerce::jobs::job_digest(&live, &enabled, None, &principal())
    );
    let mut other = request.clone();
    other.account = Some(account("acct-a"));
    assert_ne!(
        digest,
        faktor_commerce::jobs::job_digest(&other, &enabled, None, &principal())
    );
    // The OWNER is part of the identity: another session or another account
    // can never mint the same digest (and therefore never attach) for the
    // same request.
    assert_ne!(
        digest,
        faktor_commerce::jobs::job_digest(&request, &enabled, None, &principal_for(1, 2, None))
    );
    assert_ne!(
        digest,
        faktor_commerce::jobs::job_digest(
            &request,
            &enabled,
            None,
            &principal_for(1, 1, Some("acct-a"))
        )
    );

    let (job, attached) =
        submit_job(&store, request.clone(), &enabled, None, &principal(), 1_000).expect("submit");
    assert!(!attached);
    assert_eq!(job.state, JobState::Queued);
    assert_eq!(store.job_item_count(&job.id).expect("items"), 2);
    let (second, attached) =
        submit_job(&store, request.clone(), &enabled, None, &principal(), 1_001).expect("submit");
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
    let (third, attached) =
        submit_job(&store, request, &enabled, None, &principal(), 2_001).expect("submit");
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
    let (job, _) = submit_job(&store, request, &[], None, &principal(), 1_000).expect("submit");

    // First pass: the first two lines settle, then the acquisition "crashes"
    // (a transient failure) on line 3. The durable state must show 2 settled
    // lines and 3 pending.
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let artifacts = FakeArtifacts::default();
    let first = ScriptedExecutor::matched();
    let crash_key = bom_line_key(2, &BomItem::new("PART-0002", 10).expect("line"));
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
            bom_line_key(0, &BomItem::new("PART-0000", 10).expect("line")),
            bom_line_key(1, &BomItem::new("PART-0001", 10).expect("line")),
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
            bom_line_key(3, &BomItem::new("PART-0003", 10).expect("line")),
            bom_line_key(4, &BomItem::new("PART-0004", 10).expect("line")),
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
fn job_item_attempts_round_trip_and_honor_the_caller_count() {
    let dir = temp_dir();
    let store = open_store(&dir);
    let request = bom_request(bom(&[("PART-A", 10)]));
    let (job, _) = submit_job(&store, request, &[], None, &principal(), 1_000).expect("submit");
    let key = bom_line_key(0, &BomItem::new("PART-A", 10).expect("line"));
    let row = |attempts: u64, label: &str| JobItemRow {
        job_id: job.id.clone(),
        item_key: key.clone(),
        ordinal: 0,
        state: JobItemState::Pending,
        attempts,
        result: None,
        error_label: Some(label.to_string()),
        updated_ms: 1_001,
    };
    store
        .upsert_job_item(&row(7, "network_timeout"))
        .expect("upsert");
    let item = store
        .job_items(&job.id)
        .expect("items")
        .into_iter()
        .find(|item| item.item_key == key)
        .expect("item");
    assert_eq!(item.attempts, 7, "the caller's attempt count is persisted");
    // A second upsert persists the caller's NEW count verbatim: the store
    // never force-increments or resets it.
    store
        .upsert_job_item(&row(3, "rate_limited"))
        .expect("upsert");
    let item = store
        .job_items(&job.id)
        .expect("items")
        .into_iter()
        .find(|item| item.item_key == key)
        .expect("item");
    assert_eq!(item.attempts, 3);
    assert_eq!(item.error_label.as_deref(), Some("rate_limited"));
}

#[test]
fn transient_failures_are_capped_and_never_requeued_forever() {
    let dir = temp_dir();
    let store = open_store(&dir);
    let request = bom_request(bom(&[("FLAKY", 1)]));
    let (job, _) = submit_job(&store, request, &[], None, &principal(), 1_000).expect("submit");
    let key = bom_line_key(0, &BomItem::new("FLAKY", 1).expect("line"));
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let artifacts = FakeArtifacts::default();
    let executor = ScriptedExecutor::matched();

    // Each pass consumes one attempt and returns a running outcome, until
    // the documented per-line bound is reached.
    for round in 1..=MAX_JOB_ITEM_ATTEMPTS {
        *executor.fail_once.lock().expect("fail") =
            Some((key.clone(), SourceError::NetworkTimeout));
        let outcome = runtime
            .block_on(advance_job(
                &store,
                &test_ctx(),
                &job.id,
                &executor,
                &artifacts,
                2_000 + round,
            ))
            .expect("advance");
        if round < MAX_JOB_ITEM_ATTEMPTS {
            assert_eq!(outcome.job.state, JobState::Running, "round {round}");
            assert_eq!(outcome.compact.status, CompactStatus::Running);
        } else {
            assert_eq!(outcome.job.state, JobState::Completed);
            assert_eq!(outcome.compact.status, CompactStatus::Completed);
            assert_eq!(outcome.compact.unmatched, 1);
        }
    }
    assert_eq!(
        executor.calls().len() as u64,
        MAX_JOB_ITEM_ATTEMPTS,
        "a permanently failing line executes at most the documented bound"
    );
    let item = store
        .job_items(&job.id)
        .expect("items")
        .pop()
        .expect("item");
    assert_eq!(item.state, JobItemState::Failed);
    assert_eq!(item.attempts, MAX_JOB_ITEM_ATTEMPTS);
    assert_eq!(
        item.error_label.as_deref(),
        Some("network_timeout"),
        "the LAST typed error is the durable diagnostic"
    );
    let detail = item.result.expect("failed payload").as_str().to_string();
    assert!(detail.contains("network_timeout"));
    let parsed: faktor_commerce::jobs::JobItemResult =
        serde_json::from_str(&detail).expect("failed result payload");
    assert_eq!(parsed.state, JobItemState::Failed);
    assert!(parsed
        .detail
        .as_str()
        .contains(&format!("\"attempts\":{MAX_JOB_ITEM_ATTEMPTS}")));
    // A re-entry after the terminal state consumes no further work.
    let outcome = runtime
        .block_on(advance_job(
            &store,
            &test_ctx(),
            &job.id,
            &executor,
            &artifacts,
            9_000,
        ))
        .expect("replay");
    assert_eq!(outcome.job.state, JobState::Completed);
    assert_eq!(executor.calls().len() as u64, MAX_JOB_ITEM_ATTEMPTS);
    assert_eq!(artifacts.puts.load(Ordering::SeqCst), 1);
}

#[test]
fn terminal_job_without_compact_json_reports_the_true_state() {
    let dir = temp_dir();
    let store = open_store(&dir);
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let artifacts = FakeArtifacts::default();
    let mut job_ids = Vec::new();
    for (terminal, expected) in [
        (JobState::Failed, CompactStatus::Failed),
        (JobState::Cancelled, CompactStatus::Cancelled),
    ] {
        let request = bom_request(bom(&[(&format!("T{terminal:?}"), 1)]));
        let (job, _) = submit_job(&store, request, &[], None, &principal(), 1_000).expect("submit");
        store
            .update_job_state(&job.id, JobState::Running, 1_001)
            .expect("running");
        store
            .update_job_state(&job.id, terminal, 1_002)
            .expect("terminal");
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
        assert_eq!(outcome.job.state, terminal);
        assert_eq!(
            outcome.compact.status, expected,
            "a terminal job is never reported as running"
        );
        assert!(outcome.compact.artifact.is_none());
        assert!(
            outcome
                .compact
                .important
                .iter()
                .any(|entry| entry.key.as_str() == "job-error"),
            "the refusal carries a typed diagnostic"
        );
        job_ids.push(job.id);
    }

    // The same true state is reported by `job_status`.
    let service = CommerceSourceService::open(
        dir.path(),
        ServiceConfig::default(),
        Arc::new(FakeArtifacts::default()),
    )
    .expect("service");
    let failed = service
        .job_status(&principal(), &job_ids[0])
        .expect("failed status");
    assert_eq!(failed.state, JobState::Failed);
    assert_eq!(failed.compact.status, CompactStatus::Failed);
    let cancelled = service
        .job_status(&principal(), &job_ids[1])
        .expect("cancelled status");
    assert_eq!(cancelled.state, JobState::Cancelled);
    assert_eq!(cancelled.compact.status, CompactStatus::Cancelled);
}

#[test]
fn duplicate_bom_lines_each_execute_and_count() {
    let dir = temp_dir();
    let store = open_store(&dir);
    // Three identically-normalizing lines requested; each is a real line
    // (the user asked to buy it three times).
    let request = bom_request(bom(&[("DUP", 10), ("DUP", 10), ("dup", 10), ("OTHER", 10)]));
    let (job, _) = submit_job(&store, request, &[], None, &principal(), 1_000).expect("submit");
    assert_eq!(
        store.job_item_count(&job.id).expect("items"),
        4,
        "duplicate BOM lines never collapse into one durable item"
    );
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
    assert_eq!(outcome.compact.lines, 4);
    assert_eq!(outcome.compact.matched, 4);
    let calls = executor.calls();
    assert_eq!(calls.len(), 4, "every line executed");
    let distinct: std::collections::BTreeSet<&String> = calls.iter().collect();
    assert_eq!(distinct.len(), 4, "each line has its own stable key");
    let bytes = artifacts.last.lock().expect("last").clone().expect("bytes");
    let text = String::from_utf8_lossy(&bytes).to_string();
    assert_eq!(
        text.matches("\"ordinal\":").count(),
        4,
        "the artifact carries one entry per requested line"
    );
}

#[test]
fn job_digest_is_source_order_insensitive_and_punctuation_exact() {
    let mut request = bom_request(bom(&[("TPS5430DDAR", 100)]));
    request.sources = SourceSet::named(vec![source("mouser"), source("lcsc")]).expect("sources");
    let mut permuted = request.clone();
    permuted.sources = SourceSet::named(vec![source("lcsc"), source("mouser")]).expect("sources");
    assert_eq!(
        job_digest(&request, &[], None, &principal()),
        job_digest(&permuted, &[], None, &principal()),
        "a permuted source set is the same request"
    );

    let dir = temp_dir();
    let store = open_store(&dir);
    let (job, _) = submit_job(&store, request, &[], None, &principal(), 1_000).expect("submit");
    let (attached_job, attached) =
        submit_job(&store, permuted, &[], None, &principal(), 1_001).expect("submit");
    assert!(attached, "the permuted request attaches to the same job");
    assert_eq!(attached_job.id, job.id);

    // Punctuation that is part of one part number must not collide with the
    // stripped spelling: the digest (and therefore the job and cache
    // identity) separates them.
    let mut with_hash = bom_request(bom(&[("x", 1)]));
    with_hash.work = JobWork::Product {
        reference: ProductRef::parse("AB#123", None).expect("ref"),
    };
    let mut plain = with_hash.clone();
    plain.work = JobWork::Product {
        reference: ProductRef::parse("AB123", None).expect("ref"),
    };
    assert_ne!(
        job_digest(&with_hash, &[], None, &principal()),
        job_digest(&plain, &[], None, &principal())
    );

    // ...and a single-line BOM is no exception.
    let hash_bom = bom_request(bom(&[("AB#123", 1)]));
    let plain_bom = bom_request(bom(&[("AB123", 1)]));
    assert_ne!(
        job_digest(&hash_bom, &[], None, &principal()),
        job_digest(&plain_bom, &[], None, &principal())
    );
    let (hash_job, hash_attached) =
        submit_job(&store, hash_bom, &[], None, &principal(), 2_000).expect("submit");
    assert!(!hash_attached);
    let (plain_job, plain_attached) =
        submit_job(&store, plain_bom, &[], None, &principal(), 2_001).expect("submit");
    assert!(
        !plain_attached,
        "punctuation-distinct BOMs are distinct jobs"
    );
    assert_ne!(hash_job.id, plain_job.id);
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
    let (job, _) = submit_job(&store, request, &[], None, &principal(), 1_000).expect("submit");
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
    let (job, _) = submit_job(&store, request, &[], None, &principal(), 1_000).expect("submit");
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
    let (job, _) = submit_job(&store, request, &[], None, &principal(), 1_000).expect("submit");
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
    quote_requests: Mutex<Vec<QuoteRequest>>,
    seen_mechanisms: Mutex<Vec<Option<faktor_commerce::connector::AcquisitionMechanism>>>,
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
            quote_requests: Mutex::new(Vec::new()),
            seen_mechanisms: Mutex::new(Vec::new()),
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

    fn mechanisms(&self) -> Vec<Option<faktor_commerce::connector::AcquisitionMechanism>> {
        self.seen_mechanisms.lock().expect("mechanisms").clone()
    }

    fn record_mechanism(&self, ctx: &faktor_commerce::connector::AcquireCtx) {
        self.seen_mechanisms
            .lock()
            .expect("mechanisms")
            .push(ctx.mechanism());
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
        self.record_mechanism(_ctx);
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
        self.record_mechanism(_ctx);
        let behavior = self.behavior.lock().expect("behavior");
        if behavior.fail {
            return Err(SourceError::ApiUnavailable);
        }
        behavior.product.clone().ok_or(SourceError::ProductNotFound)
    }

    async fn quote(
        &self,
        _ctx: &faktor_commerce::connector::AcquireCtx,
        req: QuoteRequest,
    ) -> Result<Vec<QuoteCandidate>, SourceError> {
        self.quote_calls.fetch_add(1, Ordering::SeqCst);
        self.record_mechanism(_ctx);
        self.quote_requests
            .lock()
            .expect("quote requests")
            .push(req.clone());
        let behavior = self.behavior.lock().expect("behavior");
        if behavior.fail {
            return Err(SourceError::ApiUnavailable);
        }
        if !behavior.candidates.is_empty() {
            return Ok(behavior.candidates.clone());
        }
        // No scripted candidates: resolve from the product offer using the
        // request's variant/packaging (so a pass-through defect changes the
        // status the service observes).
        let Some(offer) = &behavior.product else {
            return Err(SourceError::ProductNotFound);
        };
        let quantity =
            Quantity::new(req.quantity.get()).map_err(|_| SourceError::InvalidRequest)?;
        let variant = req
            .variant
            .as_ref()
            .map(|variant| faktor_commerce::quote::VariantRequest::Id(variant.as_str()))
            .unwrap_or(faktor_commerce::quote::VariantRequest::None);
        let pricing = faktor_commerce::quote::PricingContext {
            variant,
            packaging: req.packaging,
            ..faktor_commerce::quote::PricingContext::default()
        };
        let resolution = faktor_commerce::quote::resolve_quote(offer, &pricing, quantity);
        Ok(vec![QuoteCandidate {
            offer: offer.clone(),
            resolution,
            freshness: Freshness::Live,
        }])
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
                    &principal(),
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
async fn quote_job_passes_the_requested_variant_and_packaging_through() {
    let dir = temp_dir();
    let mut offer = offer("mouser", "TPS5430DDAR", 1_000_000);
    offer.price_breaks = Vec::new();
    offer.variants = vec![faktor_commerce::VariantOffer {
        variant_id: VariantId::new("v1").expect("variant"),
        attributes: Vec::new(),
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
    let connector = FakeConnector::new(
        "mouser",
        Behavior {
            discoveries: Vec::new(),
            product: Some(offer),
            candidates: Vec::new(),
            fail: false,
            delay_ms: 0,
        },
    );
    let (service, _artifacts) = enabled_service(&dir, connector.clone());
    let request = CommerceJobRequest {
        work: JobWork::Quote {
            reference: ProductRef::parse("TPS5430DDAR", None).expect("ref"),
            quantity: NonZeroQuantity::new(100).expect("qty"),
            packaging: Some(PackagingType::TapeAndReel),
            variant: Some(VariantId::new("v1").expect("variant")),
        },
        sources: SourceSet::auto(),
        freshness: FreshnessMode::Live,
        detail: DetailLevel::Compact,
        account: None,
    };
    let outcome = service
        .submit_and_advance(&principal(), &test_ctx(), request)
        .await
        .expect("quote job");
    assert_eq!(outcome.job.state, JobState::Completed);
    assert_eq!(
        outcome.compact.matched, 1,
        "the requested variant/packaging resolves instead of VariantAmbiguous"
    );
    let requests = connector.quote_requests.lock().expect("requests");
    let recorded = requests
        .last()
        .expect("a quote request reached the connector");
    assert_eq!(recorded.variant.as_ref().map(|v| v.as_str()), Some("v1"));
    assert_eq!(recorded.packaging, Some(PackagingType::TapeAndReel));
    assert_eq!(recorded.quantity.get(), 100);
}

/// The observation's own visibility decides the cache scope: an
/// authenticated offer observed through an ANONYMOUS ctx is stored under
/// `Authenticated`, never `Public`; an account-scoped offer under `Account`
/// with its own account scope; a public offer under `Public`.
#[tokio::test(start_paused = true)]
async fn observed_price_visibility_decides_the_cache_scope_never_the_ctx() {
    let dir = temp_dir();

    // 1. Authenticated observation + anonymous ctx => Authenticated.
    let mut authenticated = offer("mouser", "TPS5430DDAR", 1_000_000);
    authenticated.price_visibility = PriceVisibility::Authenticated;
    for price_break in &mut authenticated.price_breaks {
        price_break.visibility = PriceVisibility::Authenticated;
    }
    let connector = FakeConnector::new(
        "mouser",
        Behavior {
            discoveries: Vec::new(),
            product: Some(authenticated),
            candidates: Vec::new(),
            fail: false,
            delay_ms: 0,
        },
    );
    let (service, _artifacts) = enabled_service(&dir, connector.clone());
    let ctx = test_ctx();
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
        .await
        .expect("authenticated product");
    let (scope, account) = last_cache_scope(&dir, "product");
    assert_eq!(scope, "authenticated", "authenticated is never weakened");
    assert_ne!(scope, "public");
    assert_eq!(account, None);
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
        .await
        .expect("authenticated quote");
    let (scope, account) = last_cache_scope(&dir, "price");
    assert_eq!(scope, "authenticated");
    assert_ne!(scope, "public");
    assert_eq!(account, None);
    // The anonymous lookup scope never admits the authenticated row.
    connector.set_fail(true);
    assert_eq!(
        service
            .quote(
                &ctx,
                QuoteRequest::new(
                    ProductRef::parse("TPS5430DDAR", None).expect("ref"),
                    100,
                    None,
                    None,
                    None,
                    FreshnessMode::PreferCache,
                )
                .expect("request"),
            )
            .await
            .expect_err("anonymous ctx must not read the authenticated row"),
        ServiceError::Source(SourceError::ApiUnavailable)
    );
}

#[tokio::test(start_paused = true)]
async fn account_specific_observations_are_keyed_by_their_own_account_scope() {
    let dir = temp_dir();
    let observed_account = account("acct-b");
    let mut scoped = offer("mouser", "TPS5430DDAR", 1_000_000);
    scoped.price_visibility = PriceVisibility::AccountSpecific;
    for price_break in &mut scoped.price_breaks {
        price_break.visibility = PriceVisibility::AccountSpecific;
        price_break.account_scope = Some(observed_account.clone());
    }
    let connector = FakeConnector::new(
        "mouser",
        Behavior {
            discoveries: Vec::new(),
            product: Some(scoped),
            candidates: Vec::new(),
            fail: false,
            delay_ms: 0,
        },
    );
    let (service, _artifacts) = enabled_service(&dir, connector.clone());
    let anonymous = test_ctx();
    service
        .quote(
            &anonymous,
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
        .await
        .expect("account-specific quote with an anonymous ctx");
    let (scope, stored) = last_cache_scope(&dir, "price");
    assert_eq!(scope, "account");
    assert_ne!(scope, "public");
    assert_eq!(
        stored.as_deref(),
        Some(observed_account.as_str()),
        "the observation's own account keys the row"
    );
    // A public lookup (an anonymous ctx) can never serve the account row.
    connector.set_fail(true);
    assert_eq!(
        service
            .quote(
                &anonymous,
                QuoteRequest::new(
                    ProductRef::parse("TPS5430DDAR", None).expect("ref"),
                    100,
                    None,
                    None,
                    None,
                    FreshnessMode::PreferCache,
                )
                .expect("request"),
            )
            .await
            .expect_err("account data must not cross into an anonymous lookup"),
        ServiceError::Source(SourceError::ApiUnavailable)
    );
}

#[tokio::test(start_paused = true)]
async fn public_observation_is_public_and_an_account_specific_offer_without_a_scope_is_refused() {
    let dir = temp_dir();
    // A plain public offer stays Public under an anonymous ctx.
    let connector = FakeConnector::healthy("mouser");
    let (service, _artifacts) = enabled_service(&dir, connector.clone());
    let ctx = test_ctx();
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
        .await
        .expect("public product");
    let (scope, account) = last_cache_scope(&dir, "product");
    assert_eq!(scope, "public");
    assert_eq!(account, None);

    // An account-specific observation with no account scope to key it by is
    // a typed refusal, never a silent widening or an unkeyed row.
    let dir = temp_dir();
    let mut unkeyed = offer("mouser", "TPS5430DDAR", 1_000_000);
    unkeyed.price_visibility = PriceVisibility::AccountSpecific;
    let connector = FakeConnector::new(
        "mouser",
        Behavior {
            discoveries: Vec::new(),
            product: Some(unkeyed),
            candidates: Vec::new(),
            fail: false,
            delay_ms: 0,
        },
    );
    let (service, _artifacts) = enabled_service(&dir, connector);
    let error = service
        .product(
            &ctx,
            ProductRequest::new(
                ProductRef::parse("TPS5430DDAR", None).expect("ref"),
                FreshnessMode::Live,
                DetailLevel::Compact,
            )
            .expect("request"),
        )
        .await
        .expect_err("typed refusal");
    assert_eq!(error, ServiceError::Source(SourceError::InvalidRequest));
    assert_eq!(
        service
            .store()
            .expect("store")
            .cache_count()
            .expect("count"),
        0,
        "nothing was cached unkeyed"
    );
}

/// BOM lines carry their own selections into the deterministic job: the
/// executor must thread the line's variant/packaging into the quote request
/// (through the real quote engine), and duplicate queries with different
/// selections remain distinct lines.
#[tokio::test(start_paused = true)]
async fn bom_job_threads_per_line_selections_through_the_quote_engine() {
    let dir = temp_dir();
    let mut offer = offer("mouser", "TPS5430DDAR", 1_000_000);
    offer.price_breaks = Vec::new();
    offer.variants = vec![faktor_commerce::VariantOffer {
        variant_id: VariantId::new("v1").expect("variant"),
        attributes: Vec::new(),
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
    let connector = FakeConnector::new(
        "mouser",
        Behavior {
            discoveries: Vec::new(),
            product: Some(offer),
            candidates: Vec::new(),
            fail: false,
            delay_ms: 0,
        },
    );
    let (service, _artifacts) = enabled_service(&dir, connector.clone());
    let selected = BomItem::new("TPS5430DDAR", 100)
        .expect("line")
        .with_variant(VariantId::new("v1").expect("variant"))
        .with_packaging(PackagingType::TapeAndReel);
    let unselected = BomItem::new("TPS5430DDAR", 100).expect("line");
    assert_ne!(
        selected.key(),
        unselected.key(),
        "duplicate queries with different selections are distinct lines"
    );
    let bom = faktor_commerce::bom::Bom::new(vec![selected, unselected]).expect("bom");
    let request = CommerceJobRequest {
        work: JobWork::Bom { bom },
        sources: SourceSet::named(vec![source("mouser")]).expect("sources"),
        freshness: FreshnessMode::Live,
        detail: DetailLevel::Compact,
        account: None,
    };
    let outcome = service
        .submit_and_advance(&principal(), &test_ctx(), request)
        .await
        .expect("bom job");
    assert_eq!(outcome.job.state, JobState::Completed);
    assert_eq!(outcome.compact.lines, 2, "two distinct durable lines");
    assert_eq!(
        outcome.compact.matched, 1,
        "the selected variant line resolves through the quote engine"
    );
    assert_eq!(
        outcome.compact.ambiguous, 1,
        "the unselected duplicate is ambiguous, never silently resolved"
    );
    let requests = connector.quote_requests.lock().expect("requests");
    assert_eq!(requests.len(), 2, "one quote per line, in request order");
    assert_eq!(requests[0].variant.as_ref().map(|v| v.as_str()), Some("v1"));
    assert_eq!(requests[0].packaging, Some(PackagingType::TapeAndReel));
    assert_eq!(requests[0].quantity.get(), 100);
    assert_eq!(requests[1].variant.as_ref().map(|v| v.as_str()), None);
    assert_eq!(requests[1].packaging, None);
    drop(requests);
    // The durable job request carries the selections (a restart resumes the
    // exact same lines).
    let row = service
        .store()
        .expect("store")
        .job(&outcome.job.id)
        .expect("read")
        .expect("row");
    let durable = row.request.as_str();
    assert!(durable.contains("v1"), "{durable}");
    assert!(durable.contains("tape_and_reel"), "{durable}");
}

const PLANTED_KEY: &str = "ghp_0123456789abcdefghijklmnopqrstuvwx";

#[tokio::test(start_paused = true)]
async fn extracted_secrets_are_scrubbed_before_results_and_artifacts() {
    // The shared scrub utility replaces the credential value...
    let scrubbed = faktor_commerce::scrub_secrets(&format!("token {PLANTED_KEY} end"));
    assert_eq!(scrubbed, "token <redacted:github_token> end");
    // ...and the normalized-payload door both scrubs values and keeps
    // refusing the forbidden marker strings.
    let payload = NormalizedPayload::from_json(format!("{{\"description\":\"{PLANTED_KEY}\"}}"))
        .expect("payload");
    assert!(!payload.as_str().contains(PLANTED_KEY));
    assert!(payload.as_str().contains("<redacted:github_token>"));
    assert!(NormalizedPayload::from_json(
        "{\"description\":\"refresh_token: rt_abc\"}".to_string()
    )
    .is_err());

    // A connector echoing the key in a title/description: the search,
    // product and quote results that leave the service are scrubbed.
    let dir = temp_dir();
    let mut tainted_offer = offer("mouser", "TPS5430DDAR", 1_000_000);
    tainted_offer.title =
        Text::<512>::new(&format!("TPS5430 converter {PLANTED_KEY}")).expect("title");
    tainted_offer.description =
        Some(Text::<4096>::new(&format!("credentials {PLANTED_KEY}")).expect("desc"));
    let mut tainted_discovery = discovery("mouser", "TPS5430DDAR");
    tainted_discovery.title =
        Text::<512>::new(&format!("TPS5430 converter {PLANTED_KEY}")).expect("title");
    let candidate = quote_candidate("mouser", "TPS5430DDAR", 100);
    let mut candidate = QuoteCandidate {
        offer: tainted_offer.clone(),
        ..candidate
    };
    candidate.resolution = faktor_commerce::quote::price_at_quantity(
        &candidate.offer,
        faktor_commerce::quote::VariantRequest::None,
        Quantity::new(100).expect("qty"),
    );
    let connector = FakeConnector::new(
        "mouser",
        Behavior {
            discoveries: vec![tainted_discovery],
            product: Some(tainted_offer),
            candidates: vec![candidate],
            fail: false,
            delay_ms: 0,
        },
    );
    let (service, artifacts) = enabled_service(&dir, connector);
    let live = test_ctx().with_freshness(FreshnessMode::Live);
    let search = service
        .search(&live, search_request_mode("TPS5430", FreshnessMode::Live))
        .await
        .expect("search");
    let title = search.discoveries[0].title.as_str();
    assert!(
        !title.contains(PLANTED_KEY),
        "search title scrubbed: {title}"
    );
    assert!(title.contains("<redacted:github_token>"));
    let product = service
        .product(
            &live,
            ProductRequest::new(
                ProductRef::parse("TPS5430DDAR", None).expect("ref"),
                FreshnessMode::Live,
                DetailLevel::Compact,
            )
            .expect("request"),
        )
        .await
        .expect("product");
    assert!(!product.offer.title.as_str().contains(PLANTED_KEY));
    assert!(!product
        .offer
        .description
        .as_ref()
        .expect("description")
        .as_str()
        .contains(PLANTED_KEY));
    let quote = service
        .quote(
            &live,
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
        .await
        .expect("quote");
    assert!(!quote.candidates[0]
        .offer
        .title
        .as_str()
        .contains(PLANTED_KEY));

    // A job whose executor echoes the key in its detail payload: the CAS
    // artifact never carries it.
    let request = bom_request(bom(&[("KEYECHO", 1)]));
    let (job, _) =
        submit_job(&store_of(&service), request, &[], None, &principal(), 1_000).expect("submit");
    let outcome = advance_job(
        &store_of(&service),
        &test_ctx(),
        &job.id,
        &KeyEchoExecutor,
        artifacts.as_ref(),
        2_000,
    )
    .await
    .expect("advance");
    assert_eq!(outcome.job.state, JobState::Completed);
    assert_eq!(outcome.compact.matched, 1);
    let bytes = artifacts.last.lock().expect("last").clone().expect("bytes");
    let text = String::from_utf8_lossy(&bytes).to_string();
    assert!(!text.contains(PLANTED_KEY), "artifact scrubbed: {text}");
    assert!(text.contains("<redacted:github_token>"));
    assert!(scan_forbidden_bytes(&bytes).is_ok());
}

fn store_of(service: &CommerceSourceService) -> Arc<CommerceStore> {
    service.store().expect("store").clone()
}

/// An executor that tries to echo a planted credential in its detail payload.
struct KeyEchoExecutor;

#[async_trait::async_trait]
impl JobItemExecutor for KeyEchoExecutor {
    async fn execute(
        &self,
        _ctx: &faktor_commerce::connector::AcquireCtx,
        _job: &faktor_commerce::jobs::CommerceJob,
        _line: &JobLine,
    ) -> Result<ItemOutcome, SourceError> {
        Ok(ItemOutcome {
            state: JobItemState::Matched,
            label: None,
            important: None,
            detail: NormalizedPayload::from_json(format!("{{\"note\":\"{PLANTED_KEY}\"}}"))
                .map_err(|_| SourceError::Store)?,
        })
    }
}

#[tokio::test(start_paused = true)]
async fn service_bom_job_flows_through_connectors_and_artifacts() {
    let dir = temp_dir();
    let connector = FakeConnector::healthy("mouser");
    let (service, artifacts) = enabled_service(&dir, connector.clone());
    let ctx = test_ctx();
    let outcome = service
        .bom(
            &principal(),
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
    let status = service
        .job_status(&principal(), &outcome.job.id)
        .expect("status");
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
            &principal(),
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
        let (job, _) = submit_job(&store, request, &[], None, &principal(), 1_000).expect("submit");
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
    let status = service.job_status(&principal(), &job_id).expect("status");
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

// ------------------------------------------------- job ownership tests

#[test]
fn active_job_dedup_never_cross_attaches_owners() {
    let dir = temp_dir();
    let store = open_store(&dir);
    let request = bom_request(bom(&[("TPS5430DDAR", 100)]));
    let enabled: Vec<SourceId> = Vec::new();
    let owner = principal_for(1, 1, None);
    let other_session = principal_for(1, 2, None);
    let other_workspace = principal_for(2, 1, None);
    let other_account = principal_for(1, 1, Some("acct-a"));

    let (job, attached) =
        submit_job(&store, request.clone(), &enabled, None, &owner, 1_000).expect("submit");
    assert!(!attached);
    let (again, attached) =
        submit_job(&store, request.clone(), &enabled, None, &owner, 1_001).expect("resubmit");
    assert!(attached, "the same owner attaches to its own active job");
    assert_eq!(again.id, job.id);

    for (label, other) in [
        ("another session", &other_session),
        ("another workspace", &other_workspace),
        ("another account", &other_account),
    ] {
        let (foreign, attached) =
            submit_job(&store, request.clone(), &enabled, None, other, 2_000).expect("submit");
        assert!(
            !attached,
            "{label} must never attach to another owner's active job"
        );
        assert_ne!(foreign.id, job.id);
        let row = store.job(&foreign.id).expect("read").expect("row");
        assert_eq!(
            row.owner_key.as_deref(),
            Some(other.owner_key().as_str()),
            "{label} owns its own durable row"
        );
    }
    let row = store.job(&job.id).expect("read").expect("row");
    assert_eq!(row.owner_key.as_deref(), Some(owner.owner_key().as_str()));
    assert_eq!(store.job_count(None).expect("jobs"), 4);
}

#[test]
fn job_status_is_scoped_to_the_owning_principal() {
    let dir = temp_dir();
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let (service, _artifacts) = enabled_service(&dir, FakeConnector::healthy("mouser"));
    let owner = principal();
    let outcome = runtime
        .block_on(service.bom(
            &owner,
            &test_ctx(),
            SourceSet::auto(),
            FreshnessMode::PreferCache,
            DetailLevel::Compact,
            bom(&[("TPS5430DDAR", 100)]),
        ))
        .expect("bom");
    let status = service
        .job_status(&owner, &outcome.job.id)
        .expect("the owner reads its own job");
    assert_eq!(status.state, JobState::Completed);
    assert_eq!(status.job_id, outcome.job.id);

    for (label, foreign) in [
        ("different session, same account", principal_for(1, 2, None)),
        ("different workspace", principal_for(2, 1, None)),
        ("different account", principal_for(1, 1, Some("acct-a"))),
    ] {
        assert_eq!(
            service.job_status(&foreign, &outcome.job.id).unwrap_err(),
            ServiceError::Source(SourceError::ProductNotFound),
            "{label} must see a foreign job as missing, never forbidden"
        );
    }
    // A foreign id and a missing id are indistinguishable.
    assert_eq!(
        service.job_status(&owner, "job_00000000000000000000000000000000"),
        Err(ServiceError::Source(SourceError::ProductNotFound))
    );
}

#[test]
fn anonymous_and_account_scoped_principals_are_isolated() {
    let dir = temp_dir();
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let (service, _artifacts) = enabled_service(&dir, FakeConnector::healthy("mouser"));
    let anonymous = principal_for(1, 1, None);
    let scoped = principal_for(1, 1, Some("acct-a"));
    let scoped_ctx = test_ctx().with_account(account("acct-a"));

    let anonymous_job = runtime
        .block_on(service.bom(
            &anonymous,
            &test_ctx(),
            SourceSet::auto(),
            FreshnessMode::PreferCache,
            DetailLevel::Compact,
            bom(&[("TPS5430DDAR", 100)]),
        ))
        .expect("anonymous bom");
    let scoped_job = runtime
        .block_on(service.bom(
            &scoped,
            &scoped_ctx,
            SourceSet::auto(),
            FreshnessMode::PreferCache,
            DetailLevel::Compact,
            bom(&[("TPS5430DDAR", 100)]),
        ))
        .expect("scoped bom");
    assert_ne!(
        anonymous_job.job.id, scoped_job.job.id,
        "the account scope is part of the job identity"
    );
    service
        .job_status(&anonymous, &anonymous_job.job.id)
        .expect("anonymous owner reads");
    service
        .job_status(&scoped, &scoped_job.job.id)
        .expect("scoped owner reads");
    for (reader, job) in [
        (&scoped, &anonymous_job.job.id),
        (&anonymous, &scoped_job.job.id),
    ] {
        assert_eq!(
            service.job_status(reader, job).unwrap_err(),
            ServiceError::Source(SourceError::ProductNotFound)
        );
    }
    assert_eq!(
        service
            .job_status(&principal_for(1, 1, Some("acct-b")), &scoped_job.job.id)
            .unwrap_err(),
        ServiceError::Source(SourceError::ProductNotFound)
    );

    // A request whose account scope disagrees with the principal's is
    // refused typed: a job never executes under a mismatched identity.
    let mut mismatched = bom_request(bom(&[("TPS5430DDAR", 100)]));
    mismatched.account = None;
    assert_eq!(
        runtime
            .block_on(service.submit_and_advance(&scoped, &scoped_ctx, mismatched))
            .expect_err("mismatched account scope"),
        ServiceError::Source(SourceError::InvalidRequest)
    );
}

#[test]
fn legacy_ownerless_jobs_are_typed_migrated_and_refused() {
    let dir = temp_dir();
    std::fs::create_dir_all(dir.path().join("commerce")).expect("commerce dir");
    let request = bom_request(bom(&[("LEGACY", 1)]));
    let request_json = serde_json::to_string(&request).expect("request json");
    let active_id = "job_legacy_active";
    let terminal_id = "job_legacy_terminal";
    {
        // Build the exact v2 predecessor state: jobs existed, ownership did
        // not.
        let conn = raw_conn(&dir);
        conn.execute_batch(COMMERCE_MIGRATIONS[0])
            .expect("v1 schema");
        conn.execute_batch(COMMERCE_MIGRATIONS[1])
            .expect("v2 schema");
        conn.execute_batch("PRAGMA user_version = 2")
            .expect("cursor");
        for (id, state) in [(active_id, "queued"), (terminal_id, "completed")] {
            conn.execute(
                "INSERT INTO job(job_id, digest, kind, state, request_json, created_ms,
                    updated_ms, matched, ambiguous, unmatched)
                 VALUES (?1, 'legacy-digest', 'bom', ?2, ?3, 10, 10, 0, 0, 0)",
                rusqlite::params![id, state, request_json],
            )
            .expect("seed legacy job");
        }
        conn.execute(
            "INSERT INTO job_item(job_id, item_key, ordinal, state, attempts, updated_ms)
             VALUES (?1, 'legacy-item', 0, 'pending', 0, 10)",
            rusqlite::params![active_id],
        )
        .expect("seed legacy item");
    }

    let store = open_store(&dir);
    assert_eq!(
        store.schema_version().expect("version"),
        COMMERCE_SCHEMA_VERSION
    );
    // Active legacy work is terminalized by the migration with a typed
    // diagnostic: it can never resume without an owner.
    let active = store.job(active_id).expect("read").expect("row");
    assert_eq!(active.owner_key, None);
    assert_eq!(active.state, JobState::Cancelled);
    assert_eq!(active.finished_ms, Some(10));
    assert_eq!(
        active.last_error.as_deref(),
        Some("owner_scope_missing"),
        "the migration leaves a typed reason"
    );
    // Terminal legacy rows keep their state but stay ownerless.
    let terminal = store.job(terminal_id).expect("read").expect("row");
    assert_eq!(terminal.owner_key, None);
    assert_eq!(terminal.state, JobState::Completed);
    assert!(store.pending_jobs().expect("pending").is_empty());

    // No principal can read an ownerless row; recovery refuses it typed.
    let service = CommerceSourceService::open(
        dir.path(),
        ServiceConfig::default(),
        Arc::new(FakeArtifacts::default()),
    )
    .expect("service");
    for id in [active_id, terminal_id] {
        assert_eq!(
            service.job_status(&principal(), id).unwrap_err(),
            ServiceError::Source(SourceError::ProductNotFound),
            "a legacy ownerless row is never globally readable"
        );
    }
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    for id in [active_id, terminal_id] {
        assert_eq!(
            runtime
                .block_on(advance_job(
                    &store,
                    &test_ctx(),
                    id,
                    &ScriptedExecutor::matched(),
                    &FakeArtifacts::default(),
                    20,
                ))
                .expect_err("ownerless job refused"),
            SourceError::Store
        );
    }
    // The owner index exists and legacy rows still carry no owner.
    let conn = raw_conn(&dir);
    let owners: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM job WHERE owner_key IS NULL",
            [],
            |row| row.get(0),
        )
        .expect("owner count");
    assert_eq!(owners, 2);
    let index: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'index' AND name = 'idx_job_owner_digest'",
            [],
            |row| row.get(0),
        )
        .expect("index");
    assert_eq!(index, 1);
}

// ------------------------------------------- planner-authority certificates

/// A spy planner: counts `plan()` calls and delegates to the real planner.
struct CountingPlanner {
    calls: AtomicU64,
    inner: faktor_acquire::AcquisitionPlanner,
}

impl faktor_acquire::AcquisitionPlanning for CountingPlanner {
    fn plan(
        &self,
        state: &faktor_acquire::RuntimeAcquisitionState,
    ) -> faktor_acquire::AcquisitionPlan {
        self.calls.fetch_add(1, Ordering::SeqCst);
        faktor_acquire::RuntimeAcquisitionState::plan(state, &self.inner)
    }
}

/// A scripted planner: returns the decision it is told to, so tests can prove
/// the service executes exactly what the planner decided.
struct ScriptedPlanner {
    decision: Mutex<faktor_acquire::PlanDecision>,
}

impl ScriptedPlanner {
    fn new(decision: faktor_acquire::PlanDecision) -> Self {
        Self {
            decision: Mutex::new(decision),
        }
    }

    fn set(&self, decision: faktor_acquire::PlanDecision) {
        *self.decision.lock().expect("decision") = decision;
    }
}

impl faktor_acquire::AcquisitionPlanning for ScriptedPlanner {
    fn plan(
        &self,
        _state: &faktor_acquire::RuntimeAcquisitionState,
    ) -> faktor_acquire::AcquisitionPlan {
        faktor_acquire::AcquisitionPlan {
            decision: self.decision.lock().expect("decision").clone(),
            cache: Default::default(),
            fields: Vec::new(),
            notes: Vec::new(),
        }
    }
}

fn acquire_decision(
    mechanism: faktor_acquire::AcquisitionMechanism,
) -> faktor_acquire::PlanDecision {
    faktor_acquire::PlanDecision::Acquire {
        mechanism,
        conditional: None,
        substituted: false,
        degraded: false,
        coverage: faktor_acquire::CapabilityLevel::Full,
    }
}

fn service_with_planner(
    dir: &tempfile::TempDir,
    connector: Arc<dyn CommerceConnector>,
    planner: Arc<dyn faktor_acquire::AcquisitionPlanning>,
) -> Arc<CommerceSourceService> {
    let artifacts = Arc::new(FakeArtifacts::default());
    let service = CommerceSourceService::open_with_planner(
        dir.path(),
        ServiceConfig::default(),
        artifacts,
        planner,
    )
    .expect("service");
    service
        .register(connector, ConnectorPolicy::default())
        .expect("register");
    service
}

#[tokio::test]
async fn production_path_invokes_the_planner() {
    let dir = temp_dir();
    let connector = FakeConnector::healthy("mouser");
    let spy = Arc::new(CountingPlanner {
        calls: AtomicU64::new(0),
        inner: faktor_acquire::AcquisitionPlanner::default(),
    });
    let service = service_with_planner(&dir, connector.clone(), spy.clone());
    let outcome = service
        .search(&test_ctx(), search_request("TPS5430"))
        .await
        .expect("search");
    assert_eq!(outcome.discoveries.len(), 1);
    assert_eq!(
        spy.calls.load(Ordering::SeqCst),
        1,
        "the production path must call plan() once per consulted source"
    );
    assert_eq!(connector.calls().0, 1);
}

#[tokio::test]
async fn api_quota_exhaustion_with_fallback_disabled_refuses_without_browsing() {
    let dir = temp_dir();
    let connector = FakeConnector::new(
        "1688",
        Behavior {
            discoveries: vec![discovery("1688", "TPS5430DDAR")],
            product: None,
            candidates: Vec::new(),
            fail: false,
            delay_ms: 0,
        },
    );
    // The connector advertises an API and a browser mechanism: the ordinary
    // escape hatch the service must NOT take on its own.
    let browser_capable = Arc::new(BrowserCapableFake {
        inner: connector.clone(),
    });
    let service = service_with_planner(
        &dir,
        browser_capable,
        Arc::new(faktor_acquire::AcquisitionPlanner::default()),
    );
    let mut ctx = test_ctx();
    ctx.quota = Some(faktor_commerce::connector::QuotaState {
        source: source("1688"),
        remaining: Some(0),
        reset_ms: Some(now_ms() + 600_000),
    });
    let error = service
        .search(&ctx, search_request("TPS5430"))
        .await
        .expect_err("exhausted API quota must refuse");
    match error {
        ServiceError::Source(SourceError::QuotaExhausted { reset_ms }) => {
            assert!(reset_ms > now_ms());
        }
        other => panic!("expected QuotaExhausted, got {other:?}"),
    }
    assert_eq!(
        connector.calls(),
        (0, 0, 0),
        "fallback-disabled quota exhaustion must not silently browse"
    );
}

/// A thin wrapper delegating to the fake connector but advertising the
/// browser mechanism too (an ordinary API-first connector).
struct BrowserCapableFake {
    inner: Arc<FakeConnector>,
}

#[async_trait::async_trait]
impl CommerceConnector for BrowserCapableFake {
    fn source(&self) -> SourceId {
        self.inner.source()
    }

    fn capabilities(&self) -> faktor_commerce::connector::ConnectorCapabilities {
        faktor_commerce::connector::ConnectorCapabilities {
            mechanisms: vec![
                faktor_commerce::connector::AcquisitionMechanism::OfficialApi,
                faktor_commerce::connector::AcquisitionMechanism::BrowserNetwork,
            ],
            ..self.inner.capabilities()
        }
    }

    async fn discover(
        &self,
        ctx: &faktor_commerce::connector::AcquireCtx,
        req: SearchRequest,
    ) -> Result<Vec<Discovery>, SourceError> {
        self.inner.discover(ctx, req).await
    }

    async fn product(
        &self,
        ctx: &faktor_commerce::connector::AcquireCtx,
        req: ProductRequest,
    ) -> Result<CommercialOffer, SourceError> {
        self.inner.product(ctx, req).await
    }

    async fn quote(
        &self,
        ctx: &faktor_commerce::connector::AcquireCtx,
        req: QuoteRequest,
    ) -> Result<Vec<QuoteCandidate>, SourceError> {
        self.inner.quote(ctx, req).await
    }
}

#[tokio::test]
async fn connector_receives_and_executes_the_planner_chosen_mechanism() {
    let dir = temp_dir();
    let connector = FakeConnector::healthy("mouser");
    let scripted = Arc::new(ScriptedPlanner::new(acquire_decision(
        faktor_acquire::AcquisitionMechanism::Dom,
    )));
    let service = service_with_planner(&dir, connector.clone(), scripted);
    let outcome = service
        .search(&test_ctx(), search_request("TPS5430"))
        .await
        .expect("search");
    assert_eq!(outcome.discoveries.len(), 1);
    assert_eq!(
        connector.mechanisms(),
        vec![Some(faktor_commerce::connector::AcquisitionMechanism::Dom)],
        "the connector must receive exactly the planned mechanism"
    );
}

#[tokio::test]
async fn breaker_state_tracks_the_mechanism_actually_used() {
    let dir = temp_dir();
    let connector = FakeConnector::new(
        "mouser",
        Behavior {
            discoveries: vec![discovery("mouser", "TPS5430DDAR")],
            product: None,
            candidates: Vec::new(),
            fail: true,
            delay_ms: 0,
        },
    );
    let scripted = Arc::new(ScriptedPlanner::new(acquire_decision(
        faktor_acquire::AcquisitionMechanism::OfficialApi,
    )));
    let browser_capable = Arc::new(BrowserCapableFake {
        inner: connector.clone(),
    });
    let service = service_with_planner(&dir, browser_capable, scripted.clone());
    let now = now_ms();

    // Three API-mechanism failures open the API breaker and nothing else.
    for index in 0..3 {
        let _ = service
            .search(&test_ctx(), search_request(&format!("api-failure-{index}")))
            .await;
    }
    let (api_open, browser_open) = service.breaker_state(&source("mouser"), now);
    assert!(api_open.is_some(), "the api breaker opened");
    assert!(browser_open.is_none(), "the browser path was never used");

    // A browser-mechanism success must close only the browser path: if the
    // service reported it as API health, the API breaker would close too.
    scripted.set(acquire_decision(
        faktor_acquire::AcquisitionMechanism::BrowserNetwork,
    ));
    connector.set_fail(false);
    service
        .search(&test_ctx(), search_request("browser-success"))
        .await
        .expect("browser success");
    let (api_open, browser_open) = service.breaker_state(&source("mouser"), now);
    assert!(
        api_open.is_some(),
        "a browser success must not be reported as API health"
    );
    assert!(browser_open.is_none());
    let mechanisms = connector.mechanisms();
    assert_eq!(mechanisms.len(), 4);
    assert!(mechanisms[..3]
        .iter()
        .all(|m| *m == Some(faktor_commerce::connector::AcquisitionMechanism::OfficialApi)));
    assert_eq!(
        mechanisms[3],
        Some(faktor_commerce::connector::AcquisitionMechanism::BrowserNetwork)
    );

    // Browser failures move only the browser path.
    connector.set_fail(true);
    for index in 0..3 {
        let _ = service
            .search(
                &test_ctx(),
                search_request(&format!("browser-failure-{index}")),
            )
            .await;
    }
    let (api_open, browser_open) = service.breaker_state(&source("mouser"), now);
    assert!(api_open.is_some(), "the api breaker is independent");
    assert!(browser_open.is_some(), "three browser failures open it");
}

#[tokio::test]
async fn cache_serve_decision_skips_connectors_entirely() {
    let dir = temp_dir();
    let connector = FakeConnector::healthy("mouser");
    let spy = Arc::new(CountingPlanner {
        calls: AtomicU64::new(0),
        inner: faktor_acquire::AcquisitionPlanner::default(),
    });
    let service = service_with_planner(&dir, connector.clone(), spy.clone());
    let live_ctx = test_ctx().with_freshness(FreshnessMode::Live);
    service
        .search(
            &live_ctx,
            search_request_mode("TPS5430", FreshnessMode::Live),
        )
        .await
        .expect("live");
    assert_eq!(connector.calls().0, 1);
    assert_eq!(spy.calls.load(Ordering::SeqCst), 1);

    let outcome = service
        .search(&test_ctx(), search_request("TPS5430"))
        .await
        .expect("cache");
    assert!(matches!(
        outcome.freshness,
        Some(Freshness::FreshCache { .. })
    ));
    assert_eq!(
        connector.calls().0,
        1,
        "a ServeFromCache decision must skip the connector entirely"
    );
    assert_eq!(
        spy.calls.load(Ordering::SeqCst),
        2,
        "the planner made the cache decision"
    );
}
