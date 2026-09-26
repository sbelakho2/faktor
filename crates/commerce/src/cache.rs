//! Cache identity, field-level freshness and request coalescing.
//!
//! # Cache identity includes commercial context
//!
//! A cache entry is keyed by the full commercial context of the request
//! (`docs/acquire.md` §11): source, product/offer, account scope, locale,
//! market, currency, quantity, packaging, variant, pricing scope — plus the
//! freshness class. Two requests that differ in any of those are different
//! cache entries, and an account-scoped entry is never returned to a
//! different account scope.
//!
//! # Field-level freshness
//!
//! Freshness is per class, not per row: discovery 1800 s, product 21600 s,
//! price 1800 s, stock 900 s, supplier 86400 s by default, overridable
//! through [`CacheTtls`]. A price that has gone stale does not make the
//! product description stale.
//!
//! # Coalescing
//!
//! [`SingleFlight`] coalesces concurrent acquisitions by normalized
//! acquisition identity: exactly one external request, N awaiters.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::error::SourceError;
use crate::money::Currency;
use crate::offer::Freshness;
use crate::packaging::PackagingType;
use crate::quantity::NonZeroQuantity;
use crate::text::{AccountScope, SourceId, Text, VariantId};

/// Default discovery TTL in seconds (spec §13).
pub const DEFAULT_DISCOVERY_TTL_S: u64 = 1_800;
/// Default product TTL in seconds (spec §13).
pub const DEFAULT_PRODUCT_TTL_S: u64 = 21_600;
/// Default price TTL in seconds (spec §13).
pub const DEFAULT_PRICE_TTL_S: u64 = 1_800;
/// Default stock TTL in seconds (spec §13).
pub const DEFAULT_STOCK_TTL_S: u64 = 900;
/// Default supplier TTL in seconds (spec §13).
pub const DEFAULT_SUPPLIER_TTL_S: u64 = 86_400;
/// A configured TTL must be at least one second.
pub const MIN_TTL_S: u64 = 1;
/// A configured TTL may not exceed 30 days.
pub const MAX_TTL_S: u64 = 30 * 24 * 60 * 60;
/// A stale cache entry is retained for stale-fallback for at most 7 days
/// after it expired; beyond that it is garbage-collected.
pub const MAX_STALE_RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

/// The freshness class of a cache entry / observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheClass {
    /// Discovery/search results (moderate TTL).
    Discovery,
    /// Product description/identity (slow TTL).
    Product,
    /// Prices (fast TTL).
    Price,
    /// Stock (fastest TTL).
    Stock,
    /// Supplier data (slowest TTL).
    Supplier,
}

impl CacheClass {
    /// The stable wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discovery => "discovery",
            Self::Product => "product",
            Self::Price => "price",
            Self::Stock => "stock",
            Self::Supplier => "supplier",
        }
    }

    /// Parse the wire label.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "discovery" => Some(Self::Discovery),
            "product" => Some(Self::Product),
            "price" => Some(Self::Price),
            "stock" => Some(Self::Stock),
            "supplier" => Some(Self::Supplier),
            _ => None,
        }
    }

    /// The default TTL of this class in milliseconds.
    pub const fn default_ttl_ms(self) -> u64 {
        match self {
            Self::Discovery => DEFAULT_DISCOVERY_TTL_S * 1_000,
            Self::Product => DEFAULT_PRODUCT_TTL_S * 1_000,
            Self::Price => DEFAULT_PRICE_TTL_S * 1_000,
            Self::Stock => DEFAULT_STOCK_TTL_S * 1_000,
            Self::Supplier => DEFAULT_SUPPLIER_TTL_S * 1_000,
        }
    }
}

/// The configured field-level TTLs, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheTtls {
    /// Discovery TTL.
    pub discovery_ms: u64,
    /// Product TTL.
    pub product_ms: u64,
    /// Price TTL.
    pub price_ms: u64,
    /// Stock TTL.
    pub stock_ms: u64,
    /// Supplier TTL.
    pub supplier_ms: u64,
}

impl Default for CacheTtls {
    fn default() -> Self {
        Self {
            discovery_ms: DEFAULT_DISCOVERY_TTL_S * 1_000,
            product_ms: DEFAULT_PRODUCT_TTL_S * 1_000,
            price_ms: DEFAULT_PRICE_TTL_S * 1_000,
            stock_ms: DEFAULT_STOCK_TTL_S * 1_000,
            supplier_ms: DEFAULT_SUPPLIER_TTL_S * 1_000,
        }
    }
}

impl CacheTtls {
    /// The TTL of one class.
    pub const fn ttl_ms(&self, class: CacheClass) -> u64 {
        match class {
            CacheClass::Discovery => self.discovery_ms,
            CacheClass::Product => self.product_ms,
            CacheClass::Price => self.price_ms,
            CacheClass::Stock => self.stock_ms,
            CacheClass::Supplier => self.supplier_ms,
        }
    }

    /// Refuse a configuration outside `1 s ..= 30 days` per class.
    pub fn validate(&self) -> Result<(), CacheConfigError> {
        for (class, ttl) in [
            (CacheClass::Discovery, self.discovery_ms),
            (CacheClass::Product, self.product_ms),
            (CacheClass::Price, self.price_ms),
            (CacheClass::Stock, self.stock_ms),
            (CacheClass::Supplier, self.supplier_ms),
        ] {
            if !(MIN_TTL_S * 1_000..=MAX_TTL_S * 1_000).contains(&ttl) {
                return Err(CacheConfigError { class, ttl_ms: ttl });
            }
        }
        Ok(())
    }
}

/// A refused TTL configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "{class:?} ttl {ttl_ms} ms is outside {}..={} ms",
    MIN_TTL_S * 1_000,
    MAX_TTL_S * 1_000
)]
pub struct CacheConfigError {
    /// The offending class.
    pub class: CacheClass,
    /// The offending TTL.
    pub ttl_ms: u64,
}

/// The pricing scope a cache entry was observed under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PricingScope {
    /// Public pricing.
    #[default]
    Public,
    /// Pricing visible only to an authenticated session (no account scope
    /// required, but never public).
    Authenticated,
    /// Account pricing (requires an account scope).
    Account,
    /// Promotional pricing.
    Promotional,
    /// The pricing scope is not known.
    Unknown,
}

impl PricingScope {
    /// The stable wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Authenticated => "authenticated",
            Self::Account => "account",
            Self::Promotional => "promotional",
            Self::Unknown => "unknown",
        }
    }
}

/// The full commercial identity of a cache entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheIdentity {
    /// The freshness class.
    pub class: CacheClass,
    /// The source.
    pub source: SourceId,
    /// The product/offer key (a [`crate::query::ProductRef::identity_key`] or
    /// a normalized query).
    pub product: Text<{ crate::query::MAX_REF_BYTES }>,
    /// The account scope (account-scoped data never crosses scopes).
    pub account_scope: Option<AccountScope>,
    /// The locale.
    pub locale: Option<Text<32>>,
    /// The market.
    pub market: Option<Text<32>>,
    /// The currency.
    pub currency: Option<Currency>,
    /// The quantity the price was observed at.
    pub quantity: Option<NonZeroQuantity>,
    /// The packaging.
    pub packaging: Option<PackagingType>,
    /// The variant.
    pub variant: Option<VariantId>,
    /// The pricing scope.
    pub pricing_scope: PricingScope,
}

/// Domain separator for cache identity digests. Bump when the encoding
/// changes.
const CACHE_IDENTITY_DOMAIN: &[u8] = b"faktor-commerce.cache-identity/v1\0";

fn put_tagged(hasher: &mut blake3::Hasher, tag: u8, value: Option<&str>) {
    hasher.update(&[tag]);
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

impl CacheIdentity {
    /// The canonical digest of this identity.
    pub fn digest(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(CACHE_IDENTITY_DOMAIN);
        put_tagged(&mut hasher, 1, Some(self.class.as_str()));
        put_tagged(&mut hasher, 2, Some(self.source.as_str()));
        put_tagged(&mut hasher, 3, Some(self.product.as_str()));
        put_tagged(
            &mut hasher,
            4,
            self.account_scope.as_ref().map(AccountScope::as_str),
        );
        put_tagged(&mut hasher, 5, self.locale.as_ref().map(Text::as_str));
        put_tagged(&mut hasher, 6, self.market.as_ref().map(Text::as_str));
        put_tagged(&mut hasher, 7, self.currency.as_ref().map(Currency::as_str));
        put_tagged(
            &mut hasher,
            8,
            self.quantity.map(|q| q.get().to_string()).as_deref(),
        );
        put_tagged(&mut hasher, 9, self.packaging.map(PackagingType::as_str));
        put_tagged(
            &mut hasher,
            10,
            self.variant.as_ref().map(VariantId::as_str),
        );
        put_tagged(&mut hasher, 11, Some(self.pricing_scope.as_str()));
        hasher.finalize().to_hex().to_string()
    }
}

/// A persisted cache entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    /// The cache key digest.
    pub cache_key: String,
    /// The freshness class.
    pub class: CacheClass,
    /// The source.
    pub source: SourceId,
    /// The account scope.
    pub account_scope: Option<AccountScope>,
    /// The normalized payload (never raw bodies).
    pub payload_json: String,
    /// The validator for conditional revalidation.
    pub etag: Option<Text<128>>,
    /// The last-modified validator.
    pub last_modified_ms: Option<u64>,
    /// When the payload was observed.
    pub observed_at_ms: u64,
    /// When the entry expires (informational; freshness is computed from
    /// `observed_at_ms` and the configured TTL).
    pub expires_at_ms: u64,
    /// When the entry was last revalidated without a new payload.
    pub revalidated_at_ms: Option<u64>,
    /// How often the entry was served.
    pub hits: u64,
}

/// Conditional validators for one cache entry (the revalidation hook).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConditionalValidators {
    /// The stored ETag.
    pub etag: Option<String>,
    /// The stored Last-Modified.
    pub last_modified_ms: Option<u64>,
}

impl ConditionalValidators {
    /// True when at least one validator exists.
    pub fn is_present(&self) -> bool {
        self.etag.is_some() || self.last_modified_ms.is_some()
    }
}

/// The freshness decision for one cache observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheDecision {
    /// Within the class TTL.
    Fresh(Freshness),
    /// Beyond the class TTL: usable only as an explicit stale fallback.
    Stale(Freshness),
    /// No observation at all.
    Miss,
}

/// Decide freshness of an observation against a class TTL. A clock skew
/// (`now_ms < observed_at_ms`) reads as age zero, never as a negative age.
pub fn decide(observed_at_ms: u64, ttl_ms: u64, now_ms: u64) -> CacheDecision {
    let age_ms = now_ms.saturating_sub(observed_at_ms);
    if age_ms <= ttl_ms {
        CacheDecision::Fresh(Freshness::FreshCache { age_ms })
    } else {
        CacheDecision::Stale(Freshness::StaleFallback { age_ms })
    }
}

/// The freshness of an optional observation.
pub fn observe(observed_at_ms: Option<u64>, ttl_ms: u64, now_ms: u64) -> Option<CacheDecision> {
    observed_at_ms.map(|observed| decide(observed, ttl_ms, now_ms))
}

/// The strongest (most current) freshness of a set of results: any stale
/// entry makes the whole answer a stale fallback.
pub fn combine_freshness(values: impl IntoIterator<Item = Freshness>) -> Option<Freshness> {
    let mut combined: Option<Freshness> = None;
    for value in values {
        combined = Some(match combined {
            None => value,
            Some(previous) => worse_of(previous, value),
        });
    }
    combined
}

/// The worse (less current) of two freshness values; equal severity keeps
/// the larger age.
fn worse_of(left: Freshness, right: Freshness) -> Freshness {
    match (left, right) {
        (Freshness::StaleFallback { age_ms: left }, Freshness::StaleFallback { age_ms: right }) => {
            Freshness::StaleFallback {
                age_ms: left.max(right),
            }
        }
        (stale @ Freshness::StaleFallback { .. }, _) => stale,
        (_, stale @ Freshness::StaleFallback { .. }) => stale,
        (Freshness::FreshCache { age_ms: left }, Freshness::FreshCache { age_ms: right }) => {
            Freshness::FreshCache {
                age_ms: left.max(right),
            }
        }
        (fresh @ Freshness::FreshCache { .. }, Freshness::Live)
        | (Freshness::Live, fresh @ Freshness::FreshCache { .. }) => fresh,
        (Freshness::Live, Freshness::Live) => Freshness::Live,
    }
}

struct Flight<V> {
    slot: Mutex<Option<Result<V, SourceError>>>,
    ready: tokio::sync::watch::Sender<u64>,
}

impl<V> Flight<V> {
    fn new() -> Self {
        let (ready, _receiver) = tokio::sync::watch::channel(0u64);
        Self {
            slot: Mutex::new(None),
            ready,
        }
    }

    fn complete(&self, result: Result<V, SourceError>) {
        if let Ok(mut slot) = self.slot.lock() {
            *slot = Some(result);
        }
        let _ = self.ready.send(1);
    }

    fn is_complete(&self) -> bool {
        self.slot.lock().map(|slot| slot.is_some()).unwrap_or(false)
    }
}

impl<V: Clone> Flight<V> {
    fn take(&self) -> Option<Result<V, SourceError>> {
        self.slot.lock().ok().and_then(|slot| slot.clone())
    }
}

/// Removes the in-flight entry when the leader finishes or panics.
struct LeaderGuard<K: Eq + Hash, V> {
    flights: Arc<Mutex<HashMap<K, Arc<Flight<V>>>>>,
    key: K,
    flight: Arc<Flight<V>>,
}

impl<K: Eq + Hash, V> Drop for LeaderGuard<K, V> {
    fn drop(&mut self) {
        if !self.flight.is_complete() {
            // The leader panicked: waiters must not hang. Deliver a typed
            // failure and let a later call retry the acquisition.
            self.flight.complete(Err(SourceError::Store));
        }
        if let Ok(mut flights) = self.flights.lock() {
            flights.remove(&self.key);
        }
    }
}

/// Concurrent-call coalescing by normalized acquisition identity: exactly
/// one leader runs the operation, every waiter receives a clone of the same
/// result (including a failure).
pub struct SingleFlight<K, V> {
    flights: Arc<Mutex<HashMap<K, Arc<Flight<V>>>>>,
    runs: AtomicU64,
}

impl<K, V> Default for SingleFlight<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> SingleFlight<K, V> {
    /// An empty coalescer.
    pub fn new() -> Self {
        Self {
            flights: Arc::new(Mutex::new(HashMap::new())),
            runs: AtomicU64::new(0),
        }
    }

    /// How many operations actually ran (one per coalesced group).
    pub fn runs(&self) -> u64 {
        self.runs.load(Ordering::Relaxed)
    }

    /// How many groups are in flight right now.
    pub fn in_flight(&self) -> usize {
        self.flights.lock().map(|f| f.len()).unwrap_or(0)
    }
}

impl<K, V> SingleFlight<K, V>
where
    K: Eq + Hash + Clone + Send,
    V: Clone + Send,
{
    /// Run `operation` under `key`, coalescing concurrent callers.
    pub async fn run<F, Fut>(&self, key: K, operation: F) -> Result<V, SourceError>
    where
        F: FnOnce() -> Fut + Send,
        Fut: std::future::Future<Output = Result<V, SourceError>> + Send,
    {
        let (flight, leader) = {
            let mut flights = match self.flights.lock() {
                Ok(flights) => flights,
                Err(_) => return Err(SourceError::Store),
            };
            match flights.get(&key) {
                Some(existing) => (existing.clone(), false),
                None => {
                    let flight = Arc::new(Flight::new());
                    flights.insert(key.clone(), flight.clone());
                    (flight, true)
                }
            }
        };

        if leader {
            self.runs.fetch_add(1, Ordering::Relaxed);
            let guard = LeaderGuard {
                flights: self.flights.clone(),
                key: key.clone(),
                flight: flight.clone(),
            };
            let result = operation().await;
            flight.complete(result.clone());
            drop(guard);
            return result;
        }

        let mut ready = flight.ready.subscribe();
        loop {
            if let Some(result) = flight.take() {
                return result;
            }
            if ready.changed().await.is_err() {
                // The leader vanished without completing: fail typed rather
                // than hanging, and let the next call retry.
                return Err(SourceError::Store);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::MAX_REF_BYTES;

    fn identity() -> CacheIdentity {
        CacheIdentity {
            class: CacheClass::Price,
            source: SourceId::new("mouser").expect("source"),
            product: Text::<MAX_REF_BYTES>::new("offer:mouser:123").expect("product"),
            account_scope: None,
            locale: None,
            market: None,
            currency: Some(Currency::CNY),
            quantity: Some(NonZeroQuantity::new(100).expect("qty")),
            packaging: Some(PackagingType::TapeAndReel),
            variant: None,
            pricing_scope: PricingScope::Public,
        }
    }

    #[test]
    fn default_ttls_match_the_spec() {
        let ttls = CacheTtls::default();
        assert_eq!(ttls.discovery_ms, 1_800_000);
        assert_eq!(ttls.product_ms, 21_600_000);
        assert_eq!(ttls.price_ms, 1_800_000);
        assert_eq!(ttls.stock_ms, 900_000);
        assert_eq!(ttls.supplier_ms, 86_400_000);
        assert!(ttls.validate().is_ok());
        assert_eq!(ttls.ttl_ms(CacheClass::Stock), 900_000);
    }

    #[test]
    fn ttl_configuration_is_bounded() {
        let zero = CacheTtls {
            stock_ms: 0,
            ..CacheTtls::default()
        };
        assert!(zero.validate().is_err());
        let over = CacheTtls {
            stock_ms: MAX_TTL_S * 1_000 + 1,
            ..CacheTtls::default()
        };
        assert!(over.validate().is_err());
        let max = CacheTtls {
            stock_ms: MAX_TTL_S * 1_000,
            ..CacheTtls::default()
        };
        assert!(max.validate().is_ok());
    }

    #[test]
    fn cache_identity_includes_every_commercial_dimension() {
        let base = identity();
        let digest = base.digest();
        assert_eq!(digest.len(), 64);

        let mut changed = base.clone();
        changed.account_scope = Some(AccountScope::new("acct-a").expect("scope"));
        assert_ne!(digest, changed.digest(), "account scope must be in the key");

        let mut changed = base.clone();
        changed.quantity = Some(NonZeroQuantity::new(101).expect("qty"));
        assert_ne!(digest, changed.digest(), "quantity must be in the key");

        let mut changed = base.clone();
        changed.packaging = Some(PackagingType::Tray);
        assert_ne!(digest, changed.digest(), "packaging must be in the key");

        let mut changed = base.clone();
        changed.variant = Some(VariantId::new("reel-5000").expect("variant"));
        assert_ne!(digest, changed.digest(), "variant must be in the key");

        let mut changed = base.clone();
        changed.market = Some(Text::<32>::new("cn").expect("market"));
        assert_ne!(digest, changed.digest(), "market must be in the key");

        let mut changed = base.clone();
        changed.locale = Some(Text::<32>::new("zh-CN").expect("locale"));
        assert_ne!(digest, changed.digest(), "locale must be in the key");

        let mut changed = base.clone();
        changed.currency = Some(Currency::USD);
        assert_ne!(digest, changed.digest(), "currency must be in the key");

        let mut changed = base.clone();
        changed.pricing_scope = PricingScope::Account;
        assert_ne!(digest, changed.digest(), "pricing scope must be in the key");

        let mut changed = base.clone();
        changed.class = CacheClass::Stock;
        assert_ne!(
            digest,
            changed.digest(),
            "freshness class must be in the key"
        );

        assert_eq!(base.digest(), identity().digest());
    }

    #[test]
    fn freshness_is_field_level_and_never_negative() {
        let ttls = CacheTtls::default();
        let observed = 1_000_000u64;
        assert_eq!(
            decide(
                observed,
                ttls.ttl_ms(CacheClass::Price),
                observed + 1_799_000
            ),
            CacheDecision::Fresh(Freshness::FreshCache { age_ms: 1_799_000 })
        );
        assert_eq!(
            decide(
                observed,
                ttls.ttl_ms(CacheClass::Price),
                observed + 1_800_001
            ),
            CacheDecision::Stale(Freshness::StaleFallback { age_ms: 1_800_001 })
        );
        // A product observation is still fresh while its price is stale.
        assert_eq!(
            decide(
                observed,
                ttls.ttl_ms(CacheClass::Product),
                observed + 1_800_001
            ),
            CacheDecision::Fresh(Freshness::FreshCache { age_ms: 1_800_001 })
        );
        // Clock skew reads as age zero.
        assert_eq!(
            decide(observed, 1_000, observed.saturating_sub(5_000)),
            CacheDecision::Fresh(Freshness::FreshCache { age_ms: 0 })
        );
    }

    #[test]
    fn combined_freshness_is_never_better_than_the_worst_input() {
        assert_eq!(
            combine_freshness([
                Freshness::Live,
                Freshness::FreshCache { age_ms: 10 },
                Freshness::StaleFallback { age_ms: 99_000 },
            ]),
            Some(Freshness::StaleFallback { age_ms: 99_000 })
        );
        assert_eq!(
            combine_freshness([Freshness::Live, Freshness::FreshCache { age_ms: 10 }]),
            Some(Freshness::FreshCache { age_ms: 10 })
        );
        assert_eq!(combine_freshness([]), None);
    }

    #[tokio::test(start_paused = true)]
    async fn single_flight_coalesces_and_retries_after_completion() {
        let flight: Arc<SingleFlight<String, u64>> = Arc::new(SingleFlight::new());
        let calls = Arc::new(AtomicU64::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let flight = flight.clone();
            let calls = calls.clone();
            handles.push(tokio::spawn(async move {
                flight
                    .run("key".to_string(), || {
                        let calls = calls.clone();
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                            Ok(7u64)
                        }
                    })
                    .await
            }));
        }
        for handle in handles {
            assert_eq!(handle.await.expect("join"), Ok(7));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one external request");
        assert_eq!(flight.runs(), 1);
        assert_eq!(flight.in_flight(), 0);

        let result = flight.run("key".to_string(), || async { Ok(9u64) }).await;
        assert_eq!(result, Ok(9));
        assert_eq!(flight.runs(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn single_flight_shares_failures() {
        let flight: Arc<SingleFlight<String, u64>> = Arc::new(SingleFlight::new());
        let calls = Arc::new(AtomicU64::new(0));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let flight = flight.clone();
            let calls = calls.clone();
            handles.push(tokio::spawn(async move {
                flight
                    .run("key".to_string(), || {
                        let calls = calls.clone();
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                            Err(SourceError::NetworkTimeout)
                        }
                    })
                    .await
            }));
        }
        for handle in handles {
            assert_eq!(
                handle.await.expect("join"),
                Err(SourceError::NetworkTimeout)
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
