//! The durable commerce store: `<data-dir>/commerce/commerce.db` (spec §11).
//!
//! A separate SQLite database, never the session DB. WAL, `synchronous =
//! FULL`, and a strict migration ladder over `PRAGMA user_version` with the
//! repo's storage discipline:
//!
//! * the version read, the pre-migration restore point and every migration
//!   statement run inside one `BEGIN IMMEDIATE` transaction, so a second
//!   concurrent opener blocks, re-reads the advanced cursor and skips — it
//!   can never snapshot post-migration content and label it pre-migration;
//! * a database written by a newer binary is refused typed (downgrade is a
//!   decision, not an accident);
//! * a negative `user_version` is durable corruption and is refused typed
//!   before any snapshot or migration;
//! * every schema transition is preceded by a VERIFIED restore point (the
//!   SQLite backup API onto a sibling file, then `integrity_check` on the
//!   copy); if it cannot be written and verified, the migration is refused.
//!
//! # What is persisted
//!
//! Normalized domain data plus provenance, connector/extractor/normalization
//! versions, a content digest and small diagnostics. Raw HTML, response
//! bodies, screenshots, cookies and auth headers are refused by TYPE:
//! every payload goes through [`NormalizedPayload`], which enforces a byte
//! bound and runs the shared forbidden-material scanner ([`crate::result`])
//! before the value can reach SQL. There is no API that accepts raw bytes.
//!
//! # Cache identity
//!
//! Cache rows carry the full [`CacheIdentity`] (class, source, product,
//! account scope, locale, market, currency, quantity, packaging, variant,
//! pricing scope). Reads pass the expected account scope to the query
//! (`account_scope IS ?`), so an account-scoped row can never be returned
//! to a different account scope even if a key is crafted.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::cache::{
    CacheClass, CacheDecision, CacheIdentity, ConditionalValidators, MAX_STALE_RETENTION_MS,
};
use crate::error::SourceError;
use crate::identity::ProductIdentity;
use crate::money::Currency;
use crate::offer::{
    CommercialOffer, ObservationOrigin, PriceBreak, PriceVisibility, StockState, VariantOffer,
};
use crate::packaging::PackagingType;
use crate::result::{scan_forbidden, ForbiddenMaterial};
use crate::text::{AccountScope, SourceId, Text, VariantId};

/// Hard bound of one normalized payload persisted anywhere in the store.
pub const MAX_PAYLOAD_BYTES: usize = 256 * 1024;
/// Hard bound of the bounded diagnostics attached to one snapshot.
pub const MAX_DIAGNOSTICS: usize = 16;
/// Hard bound of one diagnostic key.
pub const MAX_DIAGNOSTIC_KEY_BYTES: usize = 64;
/// Hard bound of one diagnostic value.
pub const MAX_DIAGNOSTIC_VALUE_BYTES: usize = 256;
/// Hard bound of one stored `ETag`.
pub const MAX_ETAG_BYTES: usize = 128;

/// The `application_id` this store writes, so an unrelated SQLite database is
/// never mistaken for a commerce store.
pub const COMMERCE_APPLICATION_ID: i64 = 0x434F_4D4D; // "COMM"

/// Domain separator for payload content digests.
const CONTENT_DIGEST_DOMAIN: &[u8] = b"faktor-commerce.content-digest/v1\0";
/// Domain separator for product identity keys.
const IDENTITY_KEY_DOMAIN: &[u8] = b"faktor-commerce.identity-key/v1\0";

/// A typed store failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommerceStoreError {
    /// The backend failed.
    #[error("commerce store backend: {0}")]
    Backend(String),
    /// The database is not a commerce store (foreign `application_id`).
    #[error("not a commerce store: application_id {found:#x}")]
    NotACommerceStore {
        /// The observed application id.
        found: i64,
    },
    /// Durable corruption: a negative schema version cannot have been
    /// produced by this ladder.
    #[error("commerce store schema user_version {0} is corrupt (negative): refusing to snapshot or migrate it")]
    CorruptSchema(i64),
    /// A database written by a newer binary than this one.
    #[error(
        "commerce store schema v{found} is newer than this binary's ladder v{maximum_supported}: \
         downgrade refused (run the newer binary or restore the pre-migration restore point)"
    )]
    UnsupportedSchema {
        /// The version found on disk.
        found: i64,
        /// The newest version this binary knows.
        maximum_supported: i64,
    },
    /// The payload carries raw markup, secrets or headers and must never be
    /// persisted.
    #[error("commerce payload refused: {0}")]
    Forbidden(ForbiddenMaterial),
    /// The payload exceeds the byte bound.
    #[error("commerce payload exceeds {limit} bytes (got {actual})")]
    TooLarge {
        /// The bound.
        limit: usize,
        /// The actual size.
        actual: usize,
    },
    /// Malformed durable row.
    #[error("commerce store row is malformed: {0}")]
    Malformed(String),
    /// An illegal job state transition.
    #[error("job {job_id} cannot move from {from} to {to}")]
    InvalidTransition {
        /// The job.
        job_id: String,
        /// The current state.
        from: &'static str,
        /// The requested state.
        to: &'static str,
    },
    /// The store lock is poisoned.
    #[error("commerce store lock is poisoned")]
    Poisoned,
}

impl From<CommerceStoreError> for SourceError {
    fn from(error: CommerceStoreError) -> Self {
        tracing::warn!("commerce store failure: {error}");
        SourceError::Store
    }
}

impl From<rusqlite::Error> for CommerceStoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Backend(error.to_string())
    }
}

impl From<std::io::Error> for CommerceStoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Backend(format!("io: {error}"))
    }
}

/// A validated, bounded, normalized payload. The ONLY way bytes reach a
/// `payload_json` column: construction enforces the size bound and refuses
/// raw markup, secret material and HTTP header blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedPayload(String);

impl NormalizedPayload {
    /// Validate a JSON string for persistence. Raw HTML/bodies, cookies and
    /// auth headers are refused; credential values echoed into extracted
    /// text are scrubbed with the shared `faktor-security` utility.
    pub fn from_json(json: impl Into<String>) -> Result<Self, CommerceStoreError> {
        let json = json.into();
        if json.len() > MAX_PAYLOAD_BYTES {
            return Err(CommerceStoreError::TooLarge {
                limit: MAX_PAYLOAD_BYTES,
                actual: json.len(),
            });
        }
        if let Err(forbidden) = scan_forbidden(&json) {
            return Err(CommerceStoreError::Forbidden(forbidden));
        }
        // Extracted text can echo a credential value; scrub it before the
        // payload can reach a cache row, snapshot or CAS artifact. The
        // marker scan above still refuses the marker strings it owns, while
        // the scrub replaces credential values matched by the shared
        // `faktor-security` patterns.
        let scrubbed = crate::result::scrub_secrets(&json);
        if scrubbed.len() > MAX_PAYLOAD_BYTES {
            // A replacement can be marginally longer than its match; the
            // bound still holds.
            return Err(CommerceStoreError::TooLarge {
                limit: MAX_PAYLOAD_BYTES,
                actual: scrubbed.len(),
            });
        }
        Ok(Self(scrubbed))
    }

    /// Serialize a normalized domain value and validate it. The domain
    /// types cannot carry raw bodies by construction; the scan is defense in
    /// depth against a hostile field value.
    pub fn from_serializable<T: Serialize>(value: &T) -> Result<Self, CommerceStoreError> {
        let json = serde_json::to_string(value)
            .map_err(|e| CommerceStoreError::Malformed(format!("serialize payload: {e}")))?;
        Self::from_json(json)
    }

    /// The payload JSON.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parse back into a domain value. The stored JSON is scrubbed again on
    /// read (idempotent for payloads written since the scrub boundary): a
    /// legacy or hostile row can never hand a credential back to a caller.
    pub fn parse<T: DeserializeOwned>(&self) -> Result<T, CommerceStoreError> {
        let scrubbed = crate::result::scrub_secrets(&self.0);
        serde_json::from_str(&scrubbed)
            .map_err(|e| CommerceStoreError::Malformed(format!("parse payload: {e}")))
    }

    /// `BLAKE3` over the domain separator and the exact payload bytes.
    pub fn content_digest(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(CONTENT_DIGEST_DOMAIN);
        hasher.update(self.0.as_bytes());
        hasher.finalize().to_hex().to_string()
    }
}

impl Serialize for NormalizedPayload {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for NormalizedPayload {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        NormalizedPayload::from_json(raw).map_err(serde::de::Error::custom)
    }
}

/// Bounded, validated snapshot diagnostics (operator-facing, never raw
/// bodies).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SnapshotDiagnostics(Vec<(String, String)>);

impl SnapshotDiagnostics {
    /// Validate and construct.
    pub fn new(entries: Vec<(String, String)>) -> Result<Self, CommerceStoreError> {
        if entries.len() > MAX_DIAGNOSTICS {
            return Err(CommerceStoreError::TooLarge {
                limit: MAX_DIAGNOSTICS,
                actual: entries.len(),
            });
        }
        for (key, value) in &entries {
            if key.is_empty()
                || key.len() > MAX_DIAGNOSTIC_KEY_BYTES
                || value.len() > MAX_DIAGNOSTIC_VALUE_BYTES
            {
                return Err(CommerceStoreError::Malformed(
                    "diagnostic entry is out of bounds".to_string(),
                ));
            }
            if scan_forbidden(key).is_err() || scan_forbidden(value).is_err() {
                return Err(CommerceStoreError::Forbidden(ForbiddenMaterial {
                    kind: "diagnostic",
                    at: 0,
                }));
            }
        }
        Ok(Self(entries))
    }

    /// The entries.
    pub fn entries(&self) -> &[(String, String)] {
        &self.0
    }
}

/// The provenance/version tuple persisted with a snapshot (spec §11).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotVersions {
    /// The extractor version.
    pub extractor_version: Option<String>,
    /// The connector version.
    pub connector_version: Option<String>,
    /// The normalization version.
    pub normalization_version: Option<String>,
}

impl SnapshotVersions {
    fn validate_text(&self) -> Result<(), CommerceStoreError> {
        for value in [
            &self.extractor_version,
            &self.connector_version,
            &self.normalization_version,
        ]
        .into_iter()
        .flatten()
        {
            if value.len() > 64 {
                return Err(CommerceStoreError::Malformed(
                    "version label exceeds 64 bytes".to_string(),
                ));
            }
        }
        Ok(())
    }
}

/// A stored offer snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct OfferSnapshotRow {
    /// The row id.
    pub snapshot_id: i64,
    /// The product identity row.
    pub identity_id: i64,
    /// The source.
    pub source: SourceId,
    /// The source offer id, when known.
    pub source_offer_id: Option<String>,
    /// The content digest of the normalized payload.
    pub content_digest: String,
    /// The extraction origin.
    pub origin: ObservationOrigin,
    /// When the observation was made.
    pub observed_at_ms: u64,
    /// The validated normalized payload.
    pub payload: NormalizedPayload,
}

/// A stored identity row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityRow {
    /// The row id.
    pub identity_id: i64,
    /// The stable identity key digest.
    pub identity_key: String,
    /// The canonical normalized MPN, when one exists.
    pub normalized_mpn: Option<String>,
    /// The manufacturer.
    pub manufacturer: Option<String>,
}

/// One cache row read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedRow {
    /// The cache key.
    pub cache_key: String,
    /// The class.
    pub class: CacheClass,
    /// The account scope the row belongs to.
    pub account_scope: Option<AccountScope>,
    /// The validated payload.
    pub payload: NormalizedPayload,
    /// The stored ETag.
    pub etag: Option<String>,
    /// The stored Last-Modified.
    pub last_modified_ms: Option<u64>,
    /// When the payload was observed.
    pub observed_at_ms: u64,
    /// When the row was last revalidated without a new payload.
    pub revalidated_at_ms: Option<u64>,
    /// The freshness decision against the class TTL.
    pub decision: CacheDecision,
    /// How often the row was served.
    pub hits: u64,
}

/// A new cache row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCacheRow {
    /// The full commercial identity.
    pub identity: CacheIdentity,
    /// The validated payload.
    pub payload: NormalizedPayload,
    /// The conditional validators observed with the payload.
    pub validators: ConditionalValidators,
    /// When the payload was observed.
    pub observed_at_ms: u64,
}

/// Durable connector state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorStateRow {
    /// The source.
    pub source: SourceId,
    /// The planner-facing health.
    pub health: crate::error::ConnectorHealth,
    /// The last typed error label.
    pub last_error: Option<String>,
    /// When the API breaker closes.
    pub api_open_until_ms: Option<u64>,
    /// When the browser breaker closes.
    pub browser_open_until_ms: Option<u64>,
    /// Consecutive failures on the API path.
    pub failures: u64,
    /// When the row was written.
    pub updated_ms: u64,
}

/// Durable browser profile state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserProfileStateRow {
    /// The source.
    pub source: SourceId,
    /// The profile name.
    pub profile: Text<64>,
    /// The account the profile is logged into, when known.
    pub account_scope: Option<AccountScope>,
    /// The egress identity.
    pub egress: Option<Text<64>>,
    /// A pending verification, when one is open.
    pub verification: Option<Text<256>>,
    /// Last use.
    pub last_used_ms: Option<u64>,
    /// When the row was written.
    pub updated_ms: u64,
}

/// Durable egress state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressStateRow {
    /// The egress identity.
    pub egress: Text<64>,
    /// Whether the egress is healthy.
    pub healthy: bool,
    /// A short operator note.
    pub note: Option<Text<256>>,
    /// Last use.
    pub last_used_ms: Option<u64>,
    /// When the row was written.
    pub updated_ms: u64,
}

/// One recorded verification challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChallengeRow {
    /// The row id.
    pub challenge_id: i64,
    /// The source.
    pub source: SourceId,
    /// The profile, when known.
    pub profile: Option<String>,
    /// The challenge kind label.
    pub kind: String,
    /// The stable challenge key.
    pub challenge_key: String,
    /// `open` or `resolved`.
    pub state: String,
    /// When the challenge was detected.
    pub detected_ms: u64,
    /// When it was resolved, if it was.
    pub resolved_ms: Option<u64>,
}

/// A durable job row.
#[derive(Debug, Clone, PartialEq)]
pub struct JobRow {
    /// The job id.
    pub job_id: String,
    /// The request digest.
    pub digest: String,
    /// The work kind label (`search`/`product`/`quote`/`bom`).
    pub kind: String,
    /// The job state.
    pub state: crate::jobs::JobState,
    /// The validated request payload.
    pub request: NormalizedPayload,
    /// Creation time.
    pub created_ms: u64,
    /// Last update.
    pub updated_ms: u64,
    /// When execution started.
    pub started_ms: Option<u64>,
    /// When the job reached a terminal state.
    pub finished_ms: Option<u64>,
    /// Matched line count.
    pub matched: u64,
    /// Ambiguous line count.
    pub ambiguous: u64,
    /// Unmatched line count.
    pub unmatched: u64,
    /// The artifact digest, once written.
    pub artifact_digest: Option<String>,
    /// The artifact size, once written.
    pub artifact_bytes: Option<u64>,
    /// The stored compact result, once assembled.
    pub compact_json: Option<String>,
    /// The last typed failure label.
    pub last_error: Option<String>,
}

/// A durable job item row.
#[derive(Debug, Clone, PartialEq)]
pub struct JobItemRow {
    /// The owning job.
    pub job_id: String,
    /// The stable item key.
    pub item_key: String,
    /// The line ordinal.
    pub ordinal: u32,
    /// The item state.
    pub state: crate::jobs::JobItemState,
    /// How many times execution was attempted.
    pub attempts: u64,
    /// The validated per-item result payload.
    pub result: Option<NormalizedPayload>,
    /// The last typed failure label.
    pub error_label: Option<String>,
    /// Last update.
    pub updated_ms: u64,
}

/// GC bounds (spec §11: bounded rows/bytes, field-level TTLs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcPolicy {
    /// Maximum cache rows.
    pub max_cache_rows: u64,
    /// Maximum offer snapshot rows.
    pub max_offer_snapshots: u64,
    /// Maximum stock snapshot rows.
    pub max_stock_snapshots: u64,
    /// Maximum job rows (terminal jobs; active jobs are never collected).
    pub max_jobs: u64,
    /// Maximum age of a snapshot (any class) before collection.
    pub max_snapshot_age_ms: u64,
    /// Maximum cache age beyond the class TTL + stale retention.
    pub max_cache_age_ms: u64,
    /// Soft maximum of the database file in bytes.
    pub max_total_bytes: u64,
}

impl Default for GcPolicy {
    fn default() -> Self {
        Self {
            max_cache_rows: 100_000,
            max_offer_snapshots: 50_000,
            max_stock_snapshots: 200_000,
            max_jobs: 10_000,
            max_snapshot_age_ms: 30 * 24 * 60 * 60 * 1_000,
            max_cache_age_ms: MAX_STALE_RETENTION_MS + crate::cache::MAX_TTL_S * 1_000,
            max_total_bytes: 512 * 1024 * 1024,
        }
    }
}

/// What one GC pass removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Cache rows removed.
    pub cache_rows_removed: u64,
    /// Offer snapshot rows removed.
    pub offer_snapshots_removed: u64,
    /// Stock snapshot rows removed.
    pub stock_snapshots_removed: u64,
    /// Terminal job rows removed.
    pub jobs_removed: u64,
    /// Database size before the pass.
    pub bytes_before: u64,
    /// Database size after the pass.
    pub bytes_after: u64,
}

/// The migration ladder. v1 is the whole commerce schema; v2 adds the
/// connector/profile/egress/challenge state tables. Public so certification
/// suites can build exact historical states.
pub const COMMERCE_MIGRATIONS: &[&str] = &[
    // v1 — identities, snapshots, cache, jobs.
    "CREATE TABLE IF NOT EXISTS product_identity (
        identity_id INTEGER PRIMARY KEY AUTOINCREMENT,
        identity_key TEXT NOT NULL UNIQUE,
        normalized_mpn TEXT,
        manufacturer TEXT,
        manufacturer_part_number TEXT,
        source_part_number TEXT,
        offer_id TEXT,
        canonical_url TEXT,
        category TEXT,
        created_ms INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_product_identity_mpn ON product_identity(normalized_mpn);
     CREATE INDEX IF NOT EXISTS idx_product_identity_manufacturer ON product_identity(manufacturer);
     CREATE INDEX IF NOT EXISTS idx_product_identity_offer_id ON product_identity(offer_id);
     CREATE TABLE IF NOT EXISTS source_product (
        source_product_id INTEGER PRIMARY KEY AUTOINCREMENT,
        identity_id INTEGER NOT NULL REFERENCES product_identity(identity_id) ON DELETE CASCADE,
        source TEXT NOT NULL,
        lookup_key TEXT NOT NULL,
        sku TEXT,
        offer_id TEXT,
        canonical_url TEXT,
        first_seen_ms INTEGER NOT NULL,
        last_seen_ms INTEGER NOT NULL,
        UNIQUE (source, lookup_key)
     );
     CREATE INDEX IF NOT EXISTS idx_source_product_sku ON source_product(source, sku);
     CREATE INDEX IF NOT EXISTS idx_source_product_offer ON source_product(source, offer_id);
     CREATE TABLE IF NOT EXISTS offer_snapshot (
        offer_snapshot_id INTEGER PRIMARY KEY AUTOINCREMENT,
        identity_id INTEGER NOT NULL REFERENCES product_identity(identity_id) ON DELETE CASCADE,
        source TEXT NOT NULL,
        source_offer_id TEXT,
        content_digest TEXT NOT NULL,
        origin TEXT NOT NULL,
        extractor_version TEXT,
        connector_version TEXT,
        normalization_version TEXT,
        diagnostics_json TEXT NOT NULL,
        payload_json TEXT NOT NULL,
        observed_at_ms INTEGER NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_offer_snapshot_offer ON offer_snapshot(source, source_offer_id);
     CREATE INDEX IF NOT EXISTS idx_offer_snapshot_observed ON offer_snapshot(observed_at_ms);
     CREATE INDEX IF NOT EXISTS idx_offer_snapshot_identity ON offer_snapshot(identity_id, observed_at_ms);
     CREATE TABLE IF NOT EXISTS variant_snapshot (
        variant_snapshot_id INTEGER PRIMARY KEY AUTOINCREMENT,
        offer_snapshot_id INTEGER NOT NULL REFERENCES offer_snapshot(offer_snapshot_id) ON DELETE CASCADE,
        variant_id TEXT NOT NULL,
        attributes_json TEXT NOT NULL,
        packaging TEXT,
        stock_state TEXT NOT NULL,
        stock_quantity INTEGER,
        content_digest TEXT NOT NULL,
        observed_at_ms INTEGER NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_variant_snapshot_offer ON variant_snapshot(offer_snapshot_id);
     CREATE TABLE IF NOT EXISTS price_break (
        price_break_id INTEGER PRIMARY KEY AUTOINCREMENT,
        offer_snapshot_id INTEGER NOT NULL REFERENCES offer_snapshot(offer_snapshot_id) ON DELETE CASCADE,
        scope TEXT NOT NULL,
        scope_key TEXT,
        min_quantity INTEGER NOT NULL,
        max_quantity INTEGER,
        unit_price_micros INTEGER NOT NULL,
        currency TEXT NOT NULL,
        visibility TEXT NOT NULL,
        account_scope TEXT,
        promotion_json TEXT,
        observed_at_ms INTEGER NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_price_break_snapshot ON price_break(offer_snapshot_id);
     CREATE TABLE IF NOT EXISTS stock_snapshot (
        stock_snapshot_id INTEGER PRIMARY KEY AUTOINCREMENT,
        identity_id INTEGER NOT NULL REFERENCES product_identity(identity_id) ON DELETE CASCADE,
        source TEXT NOT NULL,
        variant_id TEXT,
        state TEXT NOT NULL,
        quantity INTEGER,
        lead_time_min_days INTEGER,
        lead_time_max_days INTEGER,
        observed_at_ms INTEGER NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_stock_snapshot_observed ON stock_snapshot(observed_at_ms);
     CREATE INDEX IF NOT EXISTS idx_stock_snapshot_identity ON stock_snapshot(identity_id, observed_at_ms);
     CREATE TABLE IF NOT EXISTS supplier (
        supplier_id INTEGER PRIMARY KEY AUTOINCREMENT,
        source TEXT NOT NULL,
        supplier_key TEXT NOT NULL,
        name TEXT NOT NULL,
        country TEXT,
        url TEXT,
        account_scope TEXT,
        updated_ms INTEGER NOT NULL,
        UNIQUE (source, supplier_key)
     );
     CREATE TABLE IF NOT EXISTS supplier_snapshot (
        supplier_snapshot_id INTEGER PRIMARY KEY AUTOINCREMENT,
        supplier_id INTEGER NOT NULL REFERENCES supplier(supplier_id) ON DELETE CASCADE,
        content_digest TEXT NOT NULL,
        payload_json TEXT NOT NULL,
        observed_at_ms INTEGER NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_supplier_snapshot_observed ON supplier_snapshot(observed_at_ms);
     CREATE TABLE IF NOT EXISTS search_cache (
        cache_key TEXT PRIMARY KEY,
        class TEXT NOT NULL,
        source TEXT NOT NULL,
        product TEXT NOT NULL,
        account_scope TEXT,
        locale TEXT,
        market TEXT,
        currency TEXT,
        quantity INTEGER,
        packaging TEXT,
        variant TEXT,
        pricing_scope TEXT NOT NULL,
        payload_json TEXT NOT NULL,
        content_digest TEXT NOT NULL,
        etag TEXT,
        last_modified_ms INTEGER,
        observed_at_ms INTEGER NOT NULL,
        revalidated_at_ms INTEGER,
        hits INTEGER NOT NULL DEFAULT 0
     );
     CREATE INDEX IF NOT EXISTS idx_search_cache_expiry ON search_cache(class, observed_at_ms);
     CREATE INDEX IF NOT EXISTS idx_search_cache_source ON search_cache(source, class);
     CREATE TABLE IF NOT EXISTS job (
        job_id TEXT PRIMARY KEY,
        digest TEXT NOT NULL,
        kind TEXT NOT NULL,
        state TEXT NOT NULL,
        request_json TEXT NOT NULL,
        created_ms INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL,
        started_ms INTEGER,
        finished_ms INTEGER,
        matched INTEGER NOT NULL DEFAULT 0,
        ambiguous INTEGER NOT NULL DEFAULT 0,
        unmatched INTEGER NOT NULL DEFAULT 0,
        artifact_digest TEXT,
        artifact_bytes INTEGER,
        compact_json TEXT,
        last_error TEXT
     );
     CREATE INDEX IF NOT EXISTS idx_job_digest ON job(digest, state);
     CREATE INDEX IF NOT EXISTS idx_job_state ON job(state, updated_ms);
     CREATE TABLE IF NOT EXISTS job_item (
        job_id TEXT NOT NULL REFERENCES job(job_id) ON DELETE CASCADE,
        item_key TEXT NOT NULL,
        ordinal INTEGER NOT NULL,
        state TEXT NOT NULL,
        attempts INTEGER NOT NULL DEFAULT 0,
        result_json TEXT,
        error_label TEXT,
        updated_ms INTEGER NOT NULL,
        PRIMARY KEY (job_id, item_key)
     );
     CREATE INDEX IF NOT EXISTS idx_job_item_state ON job_item(job_id, state, ordinal);",
    // v2 — connector/profile/egress/challenge state.
    "CREATE TABLE IF NOT EXISTS connector_state (
        source TEXT PRIMARY KEY,
        health_json TEXT NOT NULL,
        last_error TEXT,
        api_open_until_ms INTEGER,
        browser_open_until_ms INTEGER,
        failures INTEGER NOT NULL DEFAULT 0,
        updated_ms INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS browser_profile_state (
        source TEXT NOT NULL,
        profile TEXT NOT NULL,
        account_scope TEXT,
        egress TEXT,
        verification TEXT,
        last_used_ms INTEGER,
        updated_ms INTEGER NOT NULL,
        PRIMARY KEY (source, profile)
     );
     CREATE TABLE IF NOT EXISTS egress_state (
        egress TEXT PRIMARY KEY,
        healthy INTEGER NOT NULL DEFAULT 1,
        note TEXT,
        last_used_ms INTEGER,
        updated_ms INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS challenge (
        challenge_id INTEGER PRIMARY KEY AUTOINCREMENT,
        source TEXT NOT NULL,
        profile TEXT,
        kind TEXT NOT NULL,
        challenge_key TEXT NOT NULL,
        state TEXT NOT NULL,
        detected_ms INTEGER NOT NULL,
        resolved_ms INTEGER,
        UNIQUE (source, profile, challenge_key)
     );
     CREATE INDEX IF NOT EXISTS idx_challenge_open ON challenge(state, detected_ms);",
];

/// The newest schema version this binary knows.
pub const COMMERCE_SCHEMA_VERSION: i64 = COMMERCE_MIGRATIONS.len() as i64;

/// The durable commerce store. One writer connection behind a mutex; every
/// multi-row transition is one transaction.
pub struct CommerceStore {
    conn: Mutex<Connection>,
    path: Option<PathBuf>,
}

impl CommerceStore {
    /// Open (creating) and migrate the store at `path`.
    pub fn open(path: &Path) -> Result<Self, CommerceStoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open(path)?;
        configure(&conn)?;
        reject_foreign_database(&conn)?;
        migrate(&mut conn, Some(path))?;
        Ok(Self {
            conn: Mutex::new(conn),
            path: Some(path.to_path_buf()),
        })
    }

    /// Open an in-memory store (tests, ephemeral hosts). No backup files and
    /// no filesystem side effects.
    pub fn open_in_memory() -> Result<Self, CommerceStoreError> {
        let mut conn = Connection::open_in_memory()?;
        configure(&conn)?;
        migrate(&mut conn, None)?;
        Ok(Self {
            conn: Mutex::new(conn),
            path: None,
        })
    }

    /// The database path (`None` for an in-memory store).
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The schema version on disk.
    pub fn schema_version(&self) -> Result<i64, CommerceStoreError> {
        let conn = self.lock()?;
        Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    /// The database size in bytes (main file + WAL).
    pub fn size_bytes(&self) -> Result<u64, CommerceStoreError> {
        let conn = self.lock()?;
        let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        Ok((page_count.max(0) as u64).saturating_mul(page_size.max(0) as u64))
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, CommerceStoreError> {
        self.conn.lock().map_err(|_| CommerceStoreError::Poisoned)
    }

    // ------------------------------------------------------------ identities

    /// The stable identity key digest of a product identity.
    pub fn identity_key(identity: &ProductIdentity) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(IDENTITY_KEY_DOMAIN);
        for value in [
            identity.manufacturer.as_ref().map(|v| v.as_str()),
            identity
                .manufacturer_part_number
                .as_ref()
                .map(|v| v.as_str()),
            identity.source_part_number.as_ref().map(|v| v.as_str()),
            identity.offer_id.as_ref().map(|v| v.as_str()),
            identity.canonical_url.as_ref().map(|v| v.as_str()),
            identity.category.as_ref().map(|v| v.as_str()),
        ] {
            match value {
                None => {
                    hasher.update(&[0]);
                }
                Some(value) => {
                    hasher.update(&[1]);
                    hasher.update(&(value.len() as u64).to_le_bytes());
                    hasher.update(value.as_bytes());
                }
            }
        }
        hasher.finalize().to_hex().to_string()
    }

    /// Upsert one product identity; returns its row id.
    pub fn upsert_identity(
        &self,
        identity: &ProductIdentity,
        now_ms: u64,
    ) -> Result<i64, CommerceStoreError> {
        identity
            .validate()
            .map_err(|e| CommerceStoreError::Malformed(format!("identity: {e}")))?;
        let key = Self::identity_key(identity);
        let normalized_mpn = identity
            .normalized_mpn()
            .map_err(|e| CommerceStoreError::Malformed(format!("mpn: {e}")))?
            .map(|mpn| mpn.canonical().to_string());
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<i64> = tx
            .query_row(
                "SELECT identity_id FROM product_identity WHERE identity_key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?;
        let id = match existing {
            Some(id) => {
                tx.execute(
                    "UPDATE product_identity SET updated_ms = ?1 WHERE identity_id = ?2",
                    params![now_ms as i64, id],
                )?;
                id
            }
            None => {
                tx.execute(
                    "INSERT INTO product_identity(identity_key, normalized_mpn, manufacturer,
                        manufacturer_part_number, source_part_number, offer_id, canonical_url,
                        category, created_ms, updated_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
                    params![
                        key,
                        normalized_mpn,
                        identity.manufacturer.as_ref().map(|v| v.as_str()),
                        identity
                            .manufacturer_part_number
                            .as_ref()
                            .map(|v| v.as_str()),
                        identity.source_part_number.as_ref().map(|v| v.as_str()),
                        identity.offer_id.as_ref().map(|v| v.as_str()),
                        identity.canonical_url.as_ref().map(|v| v.as_str()),
                        identity.category.as_ref().map(|v| v.as_str()),
                        now_ms as i64,
                    ],
                )?;
                tx.last_insert_rowid()
            }
        };
        tx.commit()?;
        Ok(id)
    }

    /// Find an identity by normalized MPN and manufacturer (the index the
    /// spec asks for). The manufacturer must match exactly when given.
    pub fn find_identity_by_mpn(
        &self,
        normalized_mpn: &str,
        manufacturer: Option<&str>,
    ) -> Result<Option<IdentityRow>, CommerceStoreError> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT identity_id, identity_key, normalized_mpn, manufacturer
                 FROM product_identity
                 WHERE normalized_mpn = ?1 AND (?2 IS NULL OR manufacturer = ?2)
                 ORDER BY identity_id ASC LIMIT 1",
                params![normalized_mpn, manufacturer],
                |r| {
                    Ok(IdentityRow {
                        identity_id: r.get(0)?,
                        identity_key: r.get(1)?,
                        normalized_mpn: r.get(2)?,
                        manufacturer: r.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    // ------------------------------------------------------------- snapshots

    /// Persist one normalized offer snapshot with provenance, versions,
    /// content digest and small diagnostics; writes the identity, the
    /// source-product mapping, variants, price breaks, stock and supplier in
    /// one transaction.
    pub fn record_offer_snapshot(
        &self,
        offer: &CommercialOffer,
        versions: &SnapshotVersions,
        diagnostics: &SnapshotDiagnostics,
        now_ms: u64,
    ) -> Result<i64, CommerceStoreError> {
        offer
            .validate()
            .map_err(|e| CommerceStoreError::Malformed(format!("offer: {e}")))?;
        versions.validate_text()?;
        let payload = NormalizedPayload::from_serializable(offer)?;
        let digest = payload.content_digest();
        let diagnostics_json = serde_json::to_string(diagnostics.entries())
            .map_err(|e| CommerceStoreError::Malformed(format!("diagnostics: {e}")))?;
        let identity_id = self.upsert_identity(&offer.identity, now_ms)?;
        let source = offer.source.as_str().to_string();
        let source_offer_id = offer
            .identity
            .offer_id
            .as_ref()
            .map(|v| v.as_str().to_string());
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO offer_snapshot(identity_id, source, source_offer_id, content_digest,
                origin, extractor_version, connector_version, normalization_version,
                diagnostics_json, payload_json, observed_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                identity_id,
                source,
                source_offer_id,
                digest,
                offer.provenance.origin.as_str(),
                versions.extractor_version.as_deref().or(offer
                    .provenance
                    .extractor_version
                    .as_ref()
                    .map(|v| v.as_str())),
                versions.connector_version.as_deref().or(offer
                    .provenance
                    .connector_version
                    .as_ref()
                    .map(|v| v.as_str())),
                versions.normalization_version.as_deref().or(offer
                    .provenance
                    .normalization_version
                    .as_ref()
                    .map(|v| v.as_str())),
                diagnostics_json,
                payload.as_str(),
                now_ms as i64,
            ],
        )?;
        let snapshot_id = tx.last_insert_rowid();
        if let Some(supplier) = &offer.supplier {
            self.upsert_supplier_tx(&tx, &source, supplier, now_ms)?;
        }
        for variant in &offer.variants {
            insert_variant_tx(&tx, snapshot_id, variant, offer.currency, now_ms)?;
        }
        for price_break in &offer.price_breaks {
            insert_price_break_tx(&tx, snapshot_id, "offer", None, price_break, now_ms)?;
        }
        for option in &offer.packaging {
            for price_break in &option.price_breaks {
                insert_price_break_tx(
                    &tx,
                    snapshot_id,
                    "packaging",
                    Some(option.packaging.as_str()),
                    price_break,
                    now_ms,
                )?;
            }
        }
        insert_stock_tx(&tx, identity_id, &source, None, offer.stock, now_ms)?;
        let source_product_key = offer
            .identity
            .source_part_number
            .as_ref()
            .map(|sku| format!("sku:{}", sku.as_str()))
            .or_else(|| {
                offer
                    .identity
                    .offer_id
                    .as_ref()
                    .map(|offer_id| format!("offer:{}", offer_id.as_str()))
            })
            .or_else(|| {
                offer
                    .identity
                    .canonical_url
                    .as_ref()
                    .map(|url| format!("url:{}", url.as_str()))
            });
        if let Some(lookup_key) = source_product_key {
            tx.execute(
                "INSERT INTO source_product(identity_id, source, lookup_key, sku, offer_id,
                    canonical_url, first_seen_ms, last_seen_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
                 ON CONFLICT(source, lookup_key) DO UPDATE SET
                    identity_id = excluded.identity_id,
                    last_seen_ms = excluded.last_seen_ms",
                params![
                    identity_id,
                    source,
                    lookup_key,
                    offer
                        .identity
                        .source_part_number
                        .as_ref()
                        .map(|v| v.as_str()),
                    offer.identity.offer_id.as_ref().map(|v| v.as_str()),
                    offer.identity.canonical_url.as_ref().map(|v| v.as_str()),
                    now_ms as i64,
                ],
            )?;
        }
        tx.commit()?;
        Ok(snapshot_id)
    }

    fn upsert_supplier_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        source: &str,
        supplier: &crate::offer::Supplier,
        now_ms: u64,
    ) -> Result<i64, CommerceStoreError> {
        let supplier_key = supplier
            .id
            .as_ref()
            .map(|id| format!("id:{}", id.as_str()))
            .unwrap_or_else(|| format!("name:{}", supplier.name.as_str()));
        tx.execute(
            "INSERT INTO supplier(source, supplier_key, name, country, url, account_scope, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(source, supplier_key) DO UPDATE SET
                name = excluded.name,
                country = excluded.country,
                url = excluded.url,
                account_scope = excluded.account_scope,
                updated_ms = excluded.updated_ms",
            params![
                source,
                supplier_key,
                supplier.name.as_str(),
                supplier.country.as_ref().map(|v| v.as_str()),
                supplier.url.as_ref().map(|v| v.as_str()),
                supplier.account_scope.as_ref().map(|v| v.as_str()),
                now_ms as i64,
            ],
        )?;
        let supplier_id = tx.last_insert_rowid();
        let payload = NormalizedPayload::from_serializable(supplier)?;
        let digest = payload.content_digest();
        tx.execute(
            "INSERT INTO supplier_snapshot(supplier_id, content_digest, payload_json, observed_at_ms)
             VALUES (?1, ?2, ?3, ?4)",
            params![supplier_id, digest, payload.as_str(), now_ms as i64],
        )?;
        Ok(supplier_id)
    }

    /// The newest offer snapshot for a source offer id, if any.
    pub fn latest_offer_snapshot(
        &self,
        source: &SourceId,
        source_offer_id: &str,
    ) -> Result<Option<OfferSnapshotRow>, CommerceStoreError> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT offer_snapshot_id, identity_id, source, source_offer_id, content_digest,
                        origin, observed_at_ms, payload_json
                 FROM offer_snapshot
                 WHERE source = ?1 AND source_offer_id = ?2
                 ORDER BY observed_at_ms DESC, offer_snapshot_id DESC LIMIT 1",
                params![source.as_str(), source_offer_id],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, i64>(6)?,
                        r.get::<_, String>(7)?,
                    ))
                },
            )
            .optional()?;
        row.map(decode_offer_snapshot).transpose()
    }

    /// The newest offer snapshot for an identity row.
    pub fn latest_offer_snapshot_for_identity(
        &self,
        identity_id: i64,
    ) -> Result<Option<OfferSnapshotRow>, CommerceStoreError> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT offer_snapshot_id, identity_id, source, source_offer_id, content_digest,
                        origin, observed_at_ms, payload_json
                 FROM offer_snapshot
                 WHERE identity_id = ?1
                 ORDER BY observed_at_ms DESC, offer_snapshot_id DESC LIMIT 1",
                params![identity_id],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, i64>(6)?,
                        r.get::<_, String>(7)?,
                    ))
                },
            )
            .optional()?;
        row.map(decode_offer_snapshot).transpose()
    }

    /// Count snapshot rows (test/doctor probe).
    pub fn offer_snapshot_count(&self) -> Result<u64, CommerceStoreError> {
        let conn = self.lock()?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM offer_snapshot", [], |r| r.get(0))?;
        Ok(count.max(0) as u64)
    }

    // ----------------------------------------------------------------- cache

    /// Insert or replace one cache row.
    pub fn cache_put(&self, row: &NewCacheRow) -> Result<(), CommerceStoreError> {
        let identity = &row.identity;
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO search_cache(cache_key, class, source, product, account_scope, locale,
                market, currency, quantity, packaging, variant, pricing_scope, payload_json,
                content_digest, etag, last_modified_ms, observed_at_ms, revalidated_at_ms, hits)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, NULL, 0)
             ON CONFLICT(cache_key) DO UPDATE SET
                class = excluded.class,
                source = excluded.source,
                product = excluded.product,
                account_scope = excluded.account_scope,
                locale = excluded.locale,
                market = excluded.market,
                currency = excluded.currency,
                quantity = excluded.quantity,
                packaging = excluded.packaging,
                variant = excluded.variant,
                pricing_scope = excluded.pricing_scope,
                payload_json = excluded.payload_json,
                content_digest = excluded.content_digest,
                etag = excluded.etag,
                last_modified_ms = excluded.last_modified_ms,
                observed_at_ms = excluded.observed_at_ms,
                revalidated_at_ms = NULL,
                hits = 0",
            params![
                identity.digest(),
                identity.class.as_str(),
                identity.source.as_str(),
                identity.product.as_str(),
                identity.account_scope.as_ref().map(|v| v.as_str()),
                identity.locale.as_ref().map(|v| v.as_str()),
                identity.market.as_ref().map(|v| v.as_str()),
                identity.currency.as_ref().map(Currency::as_str),
                identity.quantity.map(|q| q.get() as i64),
                identity.packaging.map(PackagingType::as_str),
                identity.variant.as_ref().map(VariantId::as_str),
                identity.pricing_scope.as_str(),
                row.payload.as_str(),
                row.payload.content_digest(),
                row.validators.etag.as_deref(),
                row.validators.last_modified_ms.map(|v| v as i64),
                row.observed_at_ms as i64,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Read one cache row for an explicit key and account scope. The account
    /// scope is part of the WHERE clause (`account_scope IS ?`), so a row is
    /// never returned to a different scope even if two identities collide on
    /// a key. Served rows bump `hits`.
    pub fn cache_get_scoped(
        &self,
        cache_key: &str,
        class: CacheClass,
        account_scope: Option<&AccountScope>,
        ttl_ms: u64,
        now_ms: u64,
    ) -> Result<Option<CachedRow>, CommerceStoreError> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT cache_key, class, account_scope, payload_json, etag, last_modified_ms,
                        observed_at_ms, revalidated_at_ms, hits
                 FROM search_cache
                 WHERE cache_key = ?1 AND class = ?2 AND account_scope IS ?3",
                params![
                    cache_key,
                    class.as_str(),
                    account_scope.map(AccountScope::as_str)
                ],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, Option<i64>>(5)?,
                        r.get::<_, i64>(6)?,
                        r.get::<_, Option<i64>>(7)?,
                        r.get::<_, i64>(8)?,
                    ))
                },
            )
            .optional()?;
        let Some(raw) = row else {
            return Ok(None);
        };
        let class = CacheClass::parse(&raw.1)
            .ok_or_else(|| CommerceStoreError::Malformed(format!("cache class {}", raw.1)))?;
        let payload = NormalizedPayload::from_json(raw.3)?;
        let observed_at_ms = raw.6.max(0) as u64;
        let effective = raw.7.map(|v| v.max(0) as u64).unwrap_or(observed_at_ms);
        let decision = crate::cache::decide(effective, ttl_ms, now_ms);
        conn.execute(
            "UPDATE search_cache SET hits = hits + 1 WHERE cache_key = ?1",
            params![cache_key],
        )?;
        Ok(Some(CachedRow {
            cache_key: raw.0,
            class,
            account_scope: parse_account_scope(raw.2)?,
            payload,
            etag: raw.4,
            last_modified_ms: raw.5.map(|v| v.max(0) as u64),
            observed_at_ms,
            revalidated_at_ms: raw.7.map(|v| v.max(0) as u64),
            decision,
            hits: raw.8.max(0) as u64 + 1,
        }))
    }

    /// Read one cache row for a full identity.
    pub fn cache_get(
        &self,
        identity: &CacheIdentity,
        ttl_ms: u64,
        now_ms: u64,
    ) -> Result<Option<CachedRow>, CommerceStoreError> {
        self.cache_get_scoped(
            &identity.digest(),
            identity.class,
            identity.account_scope.as_ref(),
            ttl_ms,
            now_ms,
        )
    }

    /// The conditional validators stored for a cache key.
    pub fn cache_validators(
        &self,
        cache_key: &str,
    ) -> Result<ConditionalValidators, CommerceStoreError> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT etag, last_modified_ms FROM search_cache WHERE cache_key = ?1",
                params![cache_key],
                |r| {
                    Ok(ConditionalValidators {
                        etag: r.get(0)?,
                        last_modified_ms: r.get::<_, Option<i64>>(1)?.map(|v| v.max(0) as u64),
                    })
                },
            )
            .optional()?;
        Ok(row.unwrap_or_default())
    }

    /// Conditional revalidation (304): record the validators and move the
    /// freshness origin forward WITHOUT touching the payload or writing a
    /// duplicate snapshot. Returns false when no row exists.
    pub fn cache_revalidate(
        &self,
        cache_key: &str,
        validators: &ConditionalValidators,
        now_ms: u64,
    ) -> Result<bool, CommerceStoreError> {
        let conn = self.lock()?;
        let changed = conn.execute(
            "UPDATE search_cache
             SET revalidated_at_ms = ?1,
                 etag = COALESCE(?2, etag),
                 last_modified_ms = COALESCE(?3, last_modified_ms)
             WHERE cache_key = ?4",
            params![
                now_ms as i64,
                validators.etag,
                validators.last_modified_ms.map(|v| v as i64),
                cache_key,
            ],
        )?;
        Ok(changed > 0)
    }

    /// Remove one cache row.
    pub fn cache_remove(&self, cache_key: &str) -> Result<bool, CommerceStoreError> {
        let conn = self.lock()?;
        Ok(conn.execute(
            "DELETE FROM search_cache WHERE cache_key = ?1",
            params![cache_key],
        )? > 0)
    }

    /// Drop every cache row; returns how many were removed.
    pub fn clear_cache(&self) -> Result<u64, CommerceStoreError> {
        let conn = self.lock()?;
        Ok(conn.execute("DELETE FROM search_cache", [])? as u64)
    }

    /// Cache row count (test/doctor probe).
    pub fn cache_count(&self) -> Result<u64, CommerceStoreError> {
        let conn = self.lock()?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM search_cache", [], |r| r.get(0))?;
        Ok(count.max(0) as u64)
    }

    // --------------------------------------------------- connector/egress/etc.

    /// Persist connector state.
    pub fn save_connector_state(&self, row: &ConnectorStateRow) -> Result<(), CommerceStoreError> {
        let health_json = serde_json::to_string(&row.health)
            .map_err(|e| CommerceStoreError::Malformed(format!("health: {e}")))?;
        NormalizedPayload::from_json(health_json.clone())?;
        if let Some(error) = &row.last_error {
            NormalizedPayload::from_json(error.clone())?;
        }
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO connector_state(source, health_json, last_error, api_open_until_ms,
                browser_open_until_ms, failures, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(source) DO UPDATE SET
                health_json = excluded.health_json,
                last_error = excluded.last_error,
                api_open_until_ms = excluded.api_open_until_ms,
                browser_open_until_ms = excluded.browser_open_until_ms,
                failures = excluded.failures,
                updated_ms = excluded.updated_ms",
            params![
                row.source.as_str(),
                health_json,
                row.last_error,
                row.api_open_until_ms.map(|v| v as i64),
                row.browser_open_until_ms.map(|v| v as i64),
                row.failures as i64,
                row.updated_ms as i64,
            ],
        )?;
        Ok(())
    }

    /// Read connector state.
    pub fn connector_state(
        &self,
        source: &SourceId,
    ) -> Result<Option<ConnectorStateRow>, CommerceStoreError> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT source, health_json, last_error, api_open_until_ms, browser_open_until_ms,
                        failures, updated_ms
                 FROM connector_state WHERE source = ?1",
                params![source.as_str()],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, i64>(6)?,
                    ))
                },
            )
            .optional()?;
        let Some(raw) = row else { return Ok(None) };
        let health = serde_json::from_str(&raw.1)
            .map_err(|e| CommerceStoreError::Malformed(format!("connector health: {e}")))?;
        Ok(Some(ConnectorStateRow {
            source: SourceId::new(&raw.0)
                .map_err(|e| CommerceStoreError::Malformed(format!("source: {e}")))?,
            health,
            last_error: raw.2,
            api_open_until_ms: raw.3.map(|v| v.max(0) as u64),
            browser_open_until_ms: raw.4.map(|v| v.max(0) as u64),
            failures: raw.5.max(0) as u64,
            updated_ms: raw.6.max(0) as u64,
        }))
    }

    /// Persist browser profile state.
    pub fn save_browser_profile_state(
        &self,
        row: &BrowserProfileStateRow,
    ) -> Result<(), CommerceStoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO browser_profile_state(source, profile, account_scope, egress,
                verification, last_used_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(source, profile) DO UPDATE SET
                account_scope = excluded.account_scope,
                egress = excluded.egress,
                verification = excluded.verification,
                last_used_ms = excluded.last_used_ms,
                updated_ms = excluded.updated_ms",
            params![
                row.source.as_str(),
                row.profile.as_str(),
                row.account_scope.as_ref().map(|v| v.as_str()),
                row.egress.as_ref().map(|v| v.as_str()),
                row.verification.as_ref().map(|v| v.as_str()),
                row.last_used_ms.map(|v| v as i64),
                row.updated_ms as i64,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Every browser profile state row for one source.
    pub fn browser_profile_states(
        &self,
        source: &SourceId,
    ) -> Result<Vec<BrowserProfileStateRow>, CommerceStoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT source, profile, account_scope, egress, verification, last_used_ms, updated_ms
             FROM browser_profile_state WHERE source = ?1 ORDER BY profile ASC",
        )?;
        let rows = stmt.query_map(params![source.as_str()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let raw = row?;
            out.push(BrowserProfileStateRow {
                source: SourceId::new(&raw.0)
                    .map_err(|e| CommerceStoreError::Malformed(format!("source: {e}")))?,
                profile: Text::<64>::new(&raw.1)
                    .map_err(|e| CommerceStoreError::Malformed(format!("profile: {e}")))?,
                account_scope: parse_account_scope(raw.2)?,
                egress: raw
                    .3
                    .map(|value| {
                        Text::<64>::new(&value)
                            .map_err(|e| CommerceStoreError::Malformed(format!("egress: {e}")))
                    })
                    .transpose()?,
                verification: raw
                    .4
                    .map(|value| {
                        Text::<256>::new(&value).map_err(|e| {
                            CommerceStoreError::Malformed(format!("verification: {e}"))
                        })
                    })
                    .transpose()?,
                last_used_ms: raw.5.map(|v| v.max(0) as u64),
                updated_ms: raw.6.max(0) as u64,
            });
        }
        Ok(out)
    }

    /// Persist egress state.
    pub fn save_egress_state(&self, row: &EgressStateRow) -> Result<(), CommerceStoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO egress_state(egress, healthy, note, last_used_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(egress) DO UPDATE SET
                healthy = excluded.healthy,
                note = excluded.note,
                last_used_ms = excluded.last_used_ms,
                updated_ms = excluded.updated_ms",
            params![
                row.egress.as_str(),
                row.healthy as i64,
                row.note.as_ref().map(|v| v.as_str()),
                row.last_used_ms.map(|v| v as i64),
                row.updated_ms as i64,
            ],
        )?;
        Ok(())
    }

    /// Read egress state.
    pub fn egress_state(&self, egress: &str) -> Result<Option<EgressStateRow>, CommerceStoreError> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT egress, healthy, note, last_used_ms, updated_ms
                 FROM egress_state WHERE egress = ?1",
                params![egress],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some(raw) = row else { return Ok(None) };
        Ok(Some(EgressStateRow {
            egress: Text::<64>::new(&raw.0)
                .map_err(|e| CommerceStoreError::Malformed(format!("egress: {e}")))?,
            healthy: raw.1 != 0,
            note: raw
                .2
                .map(|value| {
                    Text::<256>::new(&value)
                        .map_err(|e| CommerceStoreError::Malformed(format!("note: {e}")))
                })
                .transpose()?,
            last_used_ms: raw.3.map(|v| v.max(0) as u64),
            updated_ms: raw.4.max(0) as u64,
        }))
    }

    /// Record (or refresh) one verification challenge.
    pub fn record_challenge(
        &self,
        source: &SourceId,
        profile: Option<&str>,
        kind: &str,
        challenge_key: &str,
        now_ms: u64,
    ) -> Result<i64, CommerceStoreError> {
        Text::<256>::new(kind)
            .map_err(|e| CommerceStoreError::Malformed(format!("challenge kind: {e}")))?;
        Text::<256>::new(challenge_key)
            .map_err(|e| CommerceStoreError::Malformed(format!("challenge key: {e}")))?;
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO challenge(source, profile, kind, challenge_key, state, detected_ms)
             VALUES (?1, ?2, ?3, ?4, 'open', ?5)
             ON CONFLICT(source, profile, challenge_key) DO UPDATE SET
                kind = excluded.kind,
                state = 'open',
                detected_ms = excluded.detected_ms,
                resolved_ms = NULL",
            params![source.as_str(), profile, kind, challenge_key, now_ms as i64,],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Resolve every open challenge for a source/profile.
    pub fn resolve_challenges(
        &self,
        source: &SourceId,
        profile: Option<&str>,
        now_ms: u64,
    ) -> Result<u64, CommerceStoreError> {
        let conn = self.lock()?;
        Ok(conn.execute(
            "UPDATE challenge SET state = 'resolved', resolved_ms = ?1
             WHERE source = ?2 AND profile IS ?3 AND state = 'open'",
            params![now_ms as i64, source.as_str(), profile],
        )? as u64)
    }

    /// Every open challenge, oldest first.
    pub fn open_challenges(&self) -> Result<Vec<ChallengeRow>, CommerceStoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT challenge_id, source, profile, kind, challenge_key, state, detected_ms, resolved_ms
             FROM challenge WHERE state = 'open' ORDER BY detected_ms ASC, challenge_id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, Option<i64>>(7)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let raw = row?;
            out.push(ChallengeRow {
                challenge_id: raw.0,
                source: SourceId::new(&raw.1)
                    .map_err(|e| CommerceStoreError::Malformed(format!("source: {e}")))?,
                profile: raw.2,
                kind: raw.3,
                challenge_key: raw.4,
                state: raw.5,
                detected_ms: raw.6.max(0) as u64,
                resolved_ms: raw.7.map(|v| v.max(0) as u64),
            });
        }
        Ok(out)
    }

    // ------------------------------------------------------------------ jobs

    /// Insert one job row. Returns false when the job id already exists
    /// (idempotent replay).
    pub fn insert_job(
        &self,
        job: &crate::jobs::CommerceJob,
        now_ms: u64,
    ) -> Result<bool, CommerceStoreError> {
        let request = NormalizedPayload::from_serializable(&job.request)?;
        let conn = self.lock()?;
        let changed = conn.execute(
            "INSERT OR IGNORE INTO job(job_id, digest, kind, state, request_json, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![
                job.id,
                job.digest,
                job.request.work.kind(),
                job.state.as_str(),
                request.as_str(),
                now_ms as i64,
            ],
        )?;
        Ok(changed > 0)
    }

    /// The active (queued/running) job with this digest, if any.
    pub fn find_active_job(
        &self,
        digest: &str,
    ) -> Result<Option<crate::jobs::CommerceJob>, CommerceStoreError> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT job_id, digest, state, request_json
                 FROM job WHERE digest = ?1 AND state IN ('queued', 'running')
                 ORDER BY created_ms DESC, job_id DESC LIMIT 1",
                params![digest],
                decode_job_tuple,
            )
            .optional()?;
        row.map(decode_job).transpose()
    }

    /// One job by id.
    pub fn job(&self, job_id: &str) -> Result<Option<JobRow>, CommerceStoreError> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT job_id, digest, kind, state, request_json, created_ms, updated_ms,
                        started_ms, finished_ms, matched, ambiguous, unmatched, artifact_digest,
                        artifact_bytes, compact_json, last_error
                 FROM job WHERE job_id = ?1",
                params![job_id],
                decode_job_row,
            )
            .optional()?;
        Ok(row)
    }

    /// Jobs that are not terminal (`queued` or `running`), oldest first —
    /// the restart-recovery work list.
    pub fn pending_jobs(&self) -> Result<Vec<JobRow>, CommerceStoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT job_id, digest, kind, state, request_json, created_ms, updated_ms,
                    started_ms, finished_ms, matched, ambiguous, unmatched, artifact_digest,
                    artifact_bytes, compact_json, last_error
             FROM job WHERE state IN ('queued', 'running')
             ORDER BY created_ms ASC, job_id ASC",
        )?;
        let rows = stmt.query_map([], decode_job_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Move a job between states, enforcing the legal transition table.
    pub fn update_job_state(
        &self,
        job_id: &str,
        to: crate::jobs::JobState,
        now_ms: u64,
    ) -> Result<(), CommerceStoreError> {
        let conn = self.lock()?;
        let current: Option<String> = conn
            .query_row(
                "SELECT state FROM job WHERE job_id = ?1",
                params![job_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(current) = current else {
            return Err(CommerceStoreError::Malformed(format!(
                "job {job_id} does not exist"
            )));
        };
        let from = crate::jobs::JobState::parse(&current)
            .ok_or_else(|| CommerceStoreError::Malformed(format!("job state {current}")))?;
        if !from.can_transition_to(to) {
            return Err(CommerceStoreError::InvalidTransition {
                job_id: job_id.to_string(),
                from: from.as_str(),
                to: to.as_str(),
            });
        }
        let started = (to == crate::jobs::JobState::Running).then_some(now_ms as i64);
        conn.execute(
            "UPDATE job SET state = ?1, updated_ms = ?2,
                started_ms = COALESCE(started_ms, ?3),
                finished_ms = CASE WHEN ?1 IN ('completed', 'failed', 'cancelled')
                                   THEN ?2 ELSE finished_ms END
             WHERE job_id = ?4",
            params![to.as_str(), now_ms as i64, started, job_id],
        )?;
        Ok(())
    }

    /// Insert or update one job item with the CALLER's attempt count: the
    /// durable `attempts` column is exactly `item.attempts` (the caller owns
    /// the per-line retry budget; the store never force-increments it), so a
    /// recovered item resumes with the count it earned.
    pub fn upsert_job_item(&self, item: &JobItemRow) -> Result<(), CommerceStoreError> {
        let result_json = item
            .result
            .as_ref()
            .map(|payload| payload.as_str().to_string());
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO job_item(job_id, item_key, ordinal, state, attempts, result_json,
                error_label, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(job_id, item_key) DO UPDATE SET
                ordinal = excluded.ordinal,
                state = excluded.state,
                attempts = excluded.attempts,
                result_json = excluded.result_json,
                error_label = excluded.error_label,
                updated_ms = excluded.updated_ms",
            params![
                item.job_id,
                item.item_key,
                item.ordinal as i64,
                item.state.as_str(),
                item.attempts as i64,
                result_json,
                item.error_label,
                item.updated_ms as i64,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Every item of a job in ordinal order.
    pub fn job_items(&self, job_id: &str) -> Result<Vec<JobItemRow>, CommerceStoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT job_id, item_key, ordinal, state, attempts, result_json, error_label, updated_ms
             FROM job_item WHERE job_id = ?1 ORDER BY ordinal ASC, item_key ASC",
        )?;
        let rows = stmt.query_map(params![job_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, i64>(7)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let raw = row?;
            out.push(JobItemRow {
                job_id: raw.0,
                item_key: raw.1,
                ordinal: raw.2.max(0) as u32,
                state: crate::jobs::JobItemState::parse(&raw.3).ok_or_else(|| {
                    CommerceStoreError::Malformed(format!("item state {}", raw.3))
                })?,
                attempts: raw.4.max(0) as u64,
                result: raw.5.map(NormalizedPayload::from_json).transpose()?,
                error_label: raw.6,
                updated_ms: raw.7.max(0) as u64,
            });
        }
        Ok(out)
    }

    /// Count job item rows (test probe).
    pub fn job_item_count(&self, job_id: &str) -> Result<u64, CommerceStoreError> {
        let conn = self.lock()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM job_item WHERE job_id = ?1",
            params![job_id],
            |r| r.get(0),
        )?;
        Ok(count.max(0) as u64)
    }

    /// Persist the terminal job state with counts, artifact and compact
    /// result.
    #[allow(clippy::too_many_arguments)]
    pub fn finish_job(
        &self,
        job_id: &str,
        state: crate::jobs::JobState,
        counts: crate::result::ResultCounts,
        artifact: Option<&crate::result::ArtifactRef>,
        compact_json: &str,
        last_error: Option<&str>,
        now_ms: u64,
    ) -> Result<(), CommerceStoreError> {
        if !state.is_terminal() {
            return Err(CommerceStoreError::Malformed(format!(
                "finish_job requires a terminal state, got {}",
                state.as_str()
            )));
        }
        NormalizedPayload::from_json(compact_json.to_string())?;
        let conn = self.lock()?;
        let updated = conn.execute(
            "UPDATE job SET state = ?1, updated_ms = ?2, finished_ms = ?2,
                matched = ?3, ambiguous = ?4, unmatched = ?5,
                artifact_digest = ?6, artifact_bytes = ?7, compact_json = ?8, last_error = ?9
             WHERE job_id = ?10",
            params![
                state.as_str(),
                now_ms as i64,
                counts.matched as i64,
                counts.ambiguous as i64,
                counts.unmatched as i64,
                artifact.map(|a| a.digest.clone()),
                artifact.map(|a| a.bytes as i64),
                compact_json,
                last_error,
                job_id,
            ],
        )?;
        if updated == 0 {
            return Err(CommerceStoreError::Malformed(format!(
                "job {job_id} does not exist"
            )));
        }
        Ok(())
    }

    /// The stored compact result JSON, if the job produced one.
    pub fn stored_compact(&self, job_id: &str) -> Result<Option<String>, CommerceStoreError> {
        let conn = self.lock()?;
        Ok(conn
            .query_row(
                "SELECT compact_json FROM job WHERE job_id = ?1",
                params![job_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Count jobs by state (test probe).
    pub fn job_count(
        &self,
        state: Option<crate::jobs::JobState>,
    ) -> Result<u64, CommerceStoreError> {
        let conn = self.lock()?;
        let count: i64 = match state {
            Some(state) => conn.query_row(
                "SELECT COUNT(*) FROM job WHERE state = ?1",
                params![state.as_str()],
                |r| r.get(0),
            )?,
            None => conn.query_row("SELECT COUNT(*) FROM job", [], |r| r.get(0))?,
        };
        Ok(count.max(0) as u64)
    }

    // -------------------------------------------------------------------- gc

    /// One bounded retention pass (spec §11): delete expired snapshots and
    /// cache rows, then enforce row caps and the byte cap. Never touches
    /// active jobs.
    pub fn gc(&self, policy: &GcPolicy, now_ms: u64) -> Result<GcReport, CommerceStoreError> {
        let bytes_before = self.size_bytes()?;
        let mut report = GcReport {
            bytes_before,
            ..GcReport::default()
        };
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let snapshot_cutoff = now_ms.saturating_sub(policy.max_snapshot_age_ms) as i64;
        report.offer_snapshots_removed = tx.execute(
            "DELETE FROM offer_snapshot WHERE observed_at_ms < ?1",
            params![snapshot_cutoff],
        )? as u64;
        report.stock_snapshots_removed = tx.execute(
            "DELETE FROM stock_snapshot WHERE observed_at_ms < ?1",
            params![snapshot_cutoff],
        )? as u64;
        let cache_cutoff = now_ms.saturating_sub(policy.max_cache_age_ms) as i64;
        report.cache_rows_removed = tx.execute(
            "DELETE FROM search_cache WHERE COALESCE(revalidated_at_ms, observed_at_ms) < ?1",
            params![cache_cutoff],
        )? as u64;
        report.jobs_removed = tx.execute(
            "DELETE FROM job WHERE state IN ('completed', 'failed', 'cancelled')
                 AND updated_ms < ?1",
            params![snapshot_cutoff],
        )? as u64;
        tx.commit()?;
        drop(conn);

        report.cache_rows_removed +=
            self.enforce_row_cap("search_cache", "cache_key", policy.max_cache_rows)?;
        report.offer_snapshots_removed += self.enforce_row_cap(
            "offer_snapshot",
            "offer_snapshot_id",
            policy.max_offer_snapshots,
        )?;
        report.stock_snapshots_removed += self.enforce_row_cap(
            "stock_snapshot",
            "stock_snapshot_id",
            policy.max_stock_snapshots,
        )?;
        report.jobs_removed += self.enforce_row_cap("job", "job_id", policy.max_jobs)?;

        report.bytes_after = self.size_bytes()?;
        if report.bytes_after > policy.max_total_bytes && report.bytes_after < bytes_before {
            // Deletions happened; return free pages to the filesystem so the
            // byte bound is real. VACUUM is bounded to one pass per gc call.
            let conn = self.lock()?;
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
            drop(conn);
            report.bytes_after = self.size_bytes()?;
        }
        Ok(report)
    }

    fn enforce_row_cap(
        &self,
        table: &'static str,
        key_column: &'static str,
        cap: u64,
    ) -> Result<u64, CommerceStoreError> {
        let conn = self.lock()?;
        let mut removed = 0u64;
        loop {
            let count: i64 =
                conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
            if count.max(0) as u64 <= cap {
                return Ok(removed);
            }
            let excess = (count.max(0) as u64) - cap;
            let batch = excess.min(1_000);
            let deleted = conn.execute(
                &format!(
                    "DELETE FROM {table} WHERE {key_column} IN (
                        SELECT {key_column} FROM {table} ORDER BY rowid ASC LIMIT ?1
                     )"
                ),
                params![batch as i64],
            )?;
            removed += deleted as u64;
            if deleted == 0 {
                return Ok(removed);
            }
        }
    }
}

fn decode_job_tuple(r: &rusqlite::Row<'_>) -> rusqlite::Result<(String, String, String, String)> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
}

fn decode_job(
    raw: (String, String, String, String),
) -> Result<crate::jobs::CommerceJob, CommerceStoreError> {
    let state = crate::jobs::JobState::parse(&raw.2)
        .ok_or_else(|| CommerceStoreError::Malformed(format!("job state {}", raw.2)))?;
    let request = serde_json::from_str(&raw.3)
        .map_err(|e| CommerceStoreError::Malformed(format!("job request: {e}")))?;
    Ok(crate::jobs::CommerceJob {
        id: raw.0,
        digest: raw.1,
        state,
        request,
    })
}

fn decode_job_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<JobRow> {
    fn malformed(at: usize, reason: impl std::fmt::Display) -> rusqlite::Error {
        rusqlite::Error::InvalidColumnType(at, reason.to_string(), rusqlite::types::Type::Text)
    }
    let state_raw: String = r.get(3)?;
    let state = crate::jobs::JobState::parse(&state_raw)
        .ok_or_else(|| malformed(3, format!("job state {state_raw}")))?;
    let request_json: String = r.get(4)?;
    let request = NormalizedPayload::from_json(request_json)
        .map_err(|e| malformed(4, format!("request: {e}")))?;
    Ok(JobRow {
        job_id: r.get(0)?,
        digest: r.get(1)?,
        kind: r.get(2)?,
        state,
        request,
        created_ms: r.get::<_, i64>(5)?.max(0) as u64,
        updated_ms: r.get::<_, i64>(6)?.max(0) as u64,
        started_ms: r.get::<_, Option<i64>>(7)?.map(|v| v.max(0) as u64),
        finished_ms: r.get::<_, Option<i64>>(8)?.map(|v| v.max(0) as u64),
        matched: r.get::<_, i64>(9)?.max(0) as u64,
        ambiguous: r.get::<_, i64>(10)?.max(0) as u64,
        unmatched: r.get::<_, i64>(11)?.max(0) as u64,
        artifact_digest: r.get(12)?,
        artifact_bytes: r.get::<_, Option<i64>>(13)?.map(|v| v.max(0) as u64),
        compact_json: r.get(14)?,
        last_error: r.get(15)?,
    })
}

fn decode_offer_snapshot(
    raw: (
        i64,
        i64,
        String,
        Option<String>,
        String,
        String,
        i64,
        String,
    ),
) -> Result<OfferSnapshotRow, CommerceStoreError> {
    Ok(OfferSnapshotRow {
        snapshot_id: raw.0,
        identity_id: raw.1,
        source: SourceId::new(&raw.2)
            .map_err(|e| CommerceStoreError::Malformed(format!("source: {e}")))?,
        source_offer_id: raw.3,
        content_digest: raw.4,
        origin: parse_origin(&raw.5)?,
        observed_at_ms: raw.6.max(0) as u64,
        payload: NormalizedPayload::from_json(raw.7)?,
    })
}

fn parse_origin(raw: &str) -> Result<ObservationOrigin, CommerceStoreError> {
    serde_json::from_value(serde_json::Value::String(raw.to_string()))
        .map_err(|e| CommerceStoreError::Malformed(format!("origin {raw}: {e}")))
}

fn parse_account_scope(raw: Option<String>) -> Result<Option<AccountScope>, CommerceStoreError> {
    raw.map(|value| {
        AccountScope::new(&value)
            .map_err(|e| CommerceStoreError::Malformed(format!("account scope: {e}")))
    })
    .transpose()
}

fn insert_variant_tx(
    tx: &rusqlite::Transaction<'_>,
    snapshot_id: i64,
    variant: &VariantOffer,
    _currency: Currency,
    now_ms: u64,
) -> Result<(), CommerceStoreError> {
    let attributes_json = serde_json::to_string(&variant.attributes)
        .map_err(|e| CommerceStoreError::Malformed(format!("attributes: {e}")))?;
    let payload = NormalizedPayload::from_serializable(&(&variant.variant_id, variant.stock))?;
    let (state, quantity) = stock_columns(variant.stock);
    tx.execute(
        "INSERT INTO variant_snapshot(offer_snapshot_id, variant_id, attributes_json, packaging,
            stock_state, stock_quantity, content_digest, observed_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            snapshot_id,
            variant.variant_id.as_str(),
            attributes_json,
            variant.packaging.map(PackagingType::as_str),
            state,
            quantity,
            payload.content_digest(),
            now_ms as i64,
        ],
    )?;
    for price_break in &variant.price_breaks {
        insert_price_break_tx(
            tx,
            snapshot_id,
            "variant",
            Some(variant.variant_id.as_str()),
            price_break,
            now_ms,
        )?;
    }
    Ok(())
}

fn insert_price_break_tx(
    tx: &rusqlite::Transaction<'_>,
    snapshot_id: i64,
    scope: &str,
    scope_key: Option<&str>,
    price_break: &PriceBreak,
    now_ms: u64,
) -> Result<(), CommerceStoreError> {
    let promotion_json = match &price_break.promotion {
        Some(promotion) => {
            let json = serde_json::to_string(promotion)
                .map_err(|e| CommerceStoreError::Malformed(format!("promotion: {e}")))?;
            Some(NormalizedPayload::from_json(json)?.as_str().to_string())
        }
        None => None,
    };
    tx.execute(
        "INSERT INTO price_break(offer_snapshot_id, scope, scope_key, min_quantity, max_quantity,
            unit_price_micros, currency, visibility, account_scope, promotion_json, observed_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            snapshot_id,
            scope,
            scope_key,
            price_break.min_quantity.get() as i64,
            price_break.max_quantity.map(|q| q.get() as i64),
            price_break.unit_price.micros,
            price_break.unit_price.currency.as_str(),
            visibility_label(price_break.visibility),
            price_break.account_scope.as_ref().map(|v| v.as_str()),
            promotion_json,
            now_ms as i64,
        ],
    )?;
    Ok(())
}

fn insert_stock_tx(
    tx: &rusqlite::Transaction<'_>,
    identity_id: i64,
    source: &str,
    variant_id: Option<&str>,
    stock: StockState,
    now_ms: u64,
) -> Result<(), CommerceStoreError> {
    let (state, quantity) = stock_columns(stock);
    let (min_days, max_days) = match stock {
        StockState::Backorder {
            lead_time: Some(lead),
            ..
        } => (Some(lead.min_days as i64), Some(lead.max_days as i64)),
        _ => (None, None),
    };
    tx.execute(
        "INSERT INTO stock_snapshot(identity_id, source, variant_id, state, quantity,
            lead_time_min_days, lead_time_max_days, observed_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            identity_id,
            source,
            variant_id,
            state,
            quantity,
            min_days,
            max_days,
            now_ms as i64,
        ],
    )?;
    Ok(())
}

fn stock_columns(stock: StockState) -> (&'static str, Option<i64>) {
    match stock {
        StockState::InStock { quantity } => ("in_stock", Some(quantity.get() as i64)),
        StockState::OutOfStock => ("out_of_stock", None),
        StockState::Backorder { quantity, .. } => {
            ("backorder", quantity.map(|quantity| quantity.get() as i64))
        }
        StockState::Unknown => ("unknown", None),
    }
}

fn visibility_label(visibility: PriceVisibility) -> &'static str {
    match visibility {
        PriceVisibility::Public => "public",
        PriceVisibility::Authenticated => "authenticated",
        PriceVisibility::AccountSpecific => "account_specific",
        PriceVisibility::Promotional => "promotional",
        PriceVisibility::InquiryRequired => "inquiry_required",
        PriceVisibility::Unknown => "unknown",
    }
}

fn configure(conn: &Connection) -> Result<(), CommerceStoreError> {
    conn.busy_timeout(Duration::from_millis(5_000))?;
    conn.execute_batch(
        "PRAGMA synchronous = FULL;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;",
    )?;
    // Entering WAL requires a brief exclusive lock; a concurrent opener can
    // win it first and make this pragma return SQLITE_BUSY even though a
    // busy timeout is set (SQLite does not route the journal-mode change
    // through the busy handler). Retry bounded — the mode converges as soon
    // as one opener has committed it.
    for attempt in 0..80 {
        match conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get::<_, String>(0)) {
            Ok(mode) if mode.eq_ignore_ascii_case("wal") || mode.eq_ignore_ascii_case("memory") => {
                return Ok(())
            }
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(error, _))
                if matches!(
                    error.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) => {}
            Err(error) => return Err(error.into()),
        }
        if attempt == 79 {
            return Err(CommerceStoreError::Backend(
                "could not enter WAL mode: database is locked".to_string(),
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(())
}

fn reject_foreign_database(conn: &Connection) -> Result<(), CommerceStoreError> {
    let app_id: i64 = conn.query_row("PRAGMA application_id", [], |r| r.get(0))?;
    if app_id != 0 && app_id != COMMERCE_APPLICATION_ID {
        return Err(CommerceStoreError::NotACommerceStore { found: app_id });
    }
    Ok(())
}

/// Apply the commerce schema ladder. The version read, the pre-migration
/// restore point and every migration statement run inside ONE `BEGIN
/// IMMEDIATE` transaction, so a second concurrent opener blocks on the write
/// lock, re-reads the advanced version inside its own transaction and skips.
fn migrate(conn: &mut Connection, db_path: Option<&Path>) -> Result<(), CommerceStoreError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let started: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let ladder = COMMERCE_SCHEMA_VERSION;
    if started < 0 {
        return Err(CommerceStoreError::CorruptSchema(started));
    }
    if started > ladder {
        return Err(CommerceStoreError::UnsupportedSchema {
            found: started,
            maximum_supported: ladder,
        });
    }
    if started == ladder {
        tx.commit()?;
        return Ok(());
    }
    if started == 0 {
        tx.execute_batch(&format!(
            "PRAGMA application_id = {COMMERCE_APPLICATION_ID}"
        ))?;
    }
    let mut version = started;
    if let Some(path) = db_path {
        // A verified restore point of the ACTUAL predecessor state must
        // exist before any schema transition. The snapshot runs on a
        // separate read-only connection (the backup API cannot run on a
        // connection holding a write transaction; the BEGIN IMMEDIATE lock
        // makes every reader see exactly this pre-migration state).
        let _backup = verified_pre_migration_backup(path, version)?;
    }
    for (i, sql) in COMMERCE_MIGRATIONS.iter().enumerate() {
        let target = (i + 1) as i64;
        if version >= target {
            continue;
        }
        tx.execute_batch(sql).map_err(|e| {
            CommerceStoreError::Backend(format!("commerce migration v{target}: {e}"))
        })?;
        tx.execute_batch(&format!("PRAGMA user_version = {target}"))
            .map_err(|e| {
                CommerceStoreError::Backend(format!("commerce migration v{target} cursor: {e}"))
            })?;
        version = target;
    }
    tx.commit()?;
    Ok(())
}

/// Write and independently verify a pre-migration restore point. Refuses the
/// migration when the copy cannot be produced or does not pass
/// `integrity_check`.
fn verified_pre_migration_backup(
    db_path: &Path,
    version: i64,
) -> Result<PathBuf, CommerceStoreError> {
    let parent = db_path.parent().unwrap_or_else(|| Path::new("."));
    let backup_dir = parent.join("commerce-backups");
    std::fs::create_dir_all(&backup_dir)?;
    let name = db_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "commerce.db".to_string());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dest = backup_dir.join(format!("{name}.pre-migration-v{version}-{stamp}.db"));
    {
        let src = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut dst = Connection::open(&dest)?;
        {
            let backup = rusqlite::backup::Backup::new(&src, &mut dst)?;
            backup
                .run_to_completion(64, Duration::from_millis(2), None)
                .map_err(|e| {
                    CommerceStoreError::Backend(format!("pre-migration restore point failed: {e}"))
                })?;
        }
    }
    let check = Connection::open_with_flags(&dest, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let verdict: String = check.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if verdict != "ok" {
        return Err(CommerceStoreError::Backend(format!(
            "pre-migration restore point at {} failed integrity_check: {verdict}",
            dest.display()
        )));
    }
    drop(check);
    // The copy inherits WAL mode; opening it above may have created sidecar
    // files. A restore point is exactly one self-contained `.db` file.
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{suffix}", dest.display()));
        if sidecar.exists() {
            let _ = std::fs::remove_file(&sidecar);
        }
    }
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::CanonicalUrl;

    fn identity(url: &str) -> ProductIdentity {
        ProductIdentity {
            manufacturer: None,
            manufacturer_part_number: Some(Text::new("STM32F407VGT6").expect("mpn")),
            source_part_number: None,
            offer_id: None,
            canonical_url: Some(CanonicalUrl::parse(url).expect("url")),
            category: None,
        }
    }

    /// The persisted identity digest must see the fragment-free canonical
    /// form: fragments are never sent and never identify a product.
    #[test]
    fn identity_digest_key_ignores_fragments() {
        let one = CommerceStore::identity_key(&identity("https://x.test/product#one"));
        let two = CommerceStore::identity_key(&identity("https://x.test/product#two"));
        let plain = CommerceStore::identity_key(&identity("https://x.test/product"));
        assert_eq!(one, two);
        assert_eq!(one, plain);
        assert_ne!(
            plain,
            CommerceStore::identity_key(&identity("https://x.test/product?v=1")),
            "query differences are real identity differences"
        );
        assert_ne!(
            plain,
            CommerceStore::identity_key(&identity("https://x.test/other")),
            "path differences are real identity differences"
        );
    }
}
