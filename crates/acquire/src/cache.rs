//! Field-level cache policy and conditional HTTP validators.
//!
//! Freshness is a property of the *field*, not of the response: descriptive
//! data ages slowly, availability ages fast. [`FreshnessClassTtl`] documents
//! the classes and their default TTLs; [`CacheState`] records one entry per
//! (identity, account scope, field) and answers coverage questions without
//! mutating anything.
//!
//! Conditional HTTP is represented by [`ConditionalValidators`] and rendered
//! into `If-None-Match` / `If-Modified-Since` headers. A `304 Not Modified`
//! response means *no parse and no duplicate snapshot* — the caller keeps the
//! stored entry and only its observation timestamp moves (see
//! [`crate::http`]).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::request::RequestedField;

/// How quickly a field goes stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldFreshnessClass {
    /// Slow-moving data (identity, descriptive data, packaging).
    Slow,
    /// Moderately moving data (account-scoped terms, provenance).
    Moderate,
    /// Fast-moving data (availability, quantity tiers, bulk sets).
    Fast,
}

/// The documented default TTL per freshness class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreshnessClassTtl {
    /// TTL for [`FieldFreshnessClass::Slow`].
    pub slow_ms: u64,
    /// TTL for [`FieldFreshnessClass::Moderate`].
    pub moderate_ms: u64,
    /// TTL for [`FieldFreshnessClass::Fast`].
    pub fast_ms: u64,
}

impl Default for FreshnessClassTtl {
    fn default() -> Self {
        Self {
            slow_ms: 86_400_000,
            moderate_ms: 1_800_000,
            fast_ms: 900_000,
        }
    }
}

impl FreshnessClassTtl {
    /// The TTL of one class.
    pub const fn ttl_ms(&self, class: FieldFreshnessClass) -> u64 {
        match class {
            FieldFreshnessClass::Slow => self.slow_ms,
            FieldFreshnessClass::Moderate => self.moderate_ms,
            FieldFreshnessClass::Fast => self.fast_ms,
        }
    }
}

/// The documented class of each requested field.
pub const fn freshness_class(field: RequestedField) -> FieldFreshnessClass {
    match field {
        RequestedField::Identity | RequestedField::Descriptive | RequestedField::Packaging => {
            FieldFreshnessClass::Slow
        }
        RequestedField::AccountTerms | RequestedField::Provenance => FieldFreshnessClass::Moderate,
        RequestedField::Availability | RequestedField::QuantityTiers | RequestedField::BulkSet => {
            FieldFreshnessClass::Fast
        }
    }
}

/// One cached field observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheEntry {
    /// When the observation was made.
    pub observed_at_ms: u64,
    /// The field's freshness class at observation time.
    pub class: FieldFreshnessClass,
    /// The `ETag` validator, when the response carried one.
    pub etag: Option<String>,
    /// The `Last-Modified` validator, when the response carried one.
    pub last_modified: Option<String>,
    /// The BLAKE3 hex digest of the observed content.
    pub content_digest: Option<String>,
    /// The observed content, when the caller chose to store it.
    pub content: Option<serde_json::Value>,
}

impl CacheEntry {
    /// The age of the observation at `now_ms`.
    pub fn age_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.observed_at_ms)
    }

    /// Whether the entry is fresh under `ttl` at `now_ms`.
    pub fn is_fresh(&self, ttl: &FreshnessClassTtl, now_ms: u64) -> bool {
        self.age_ms(now_ms) <= ttl.ttl_ms(self.class)
    }

    /// The validators this entry can send conditionally.
    pub fn validators(&self) -> ConditionalValidators {
        ConditionalValidators {
            etag: self.etag.clone(),
            last_modified: self.last_modified.clone(),
        }
    }
}

/// One cache identity: identity + account scope + field.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CacheKey {
    /// Normalized identity.
    pub identity: String,
    /// Account scope, if any.
    pub account_scope: Option<String>,
    /// The field.
    pub field: RequestedField,
}

impl CacheKey {
    /// Build one key.
    pub fn new(
        identity: impl Into<String>,
        account_scope: Option<String>,
        field: RequestedField,
    ) -> Self {
        Self {
            identity: identity.into(),
            account_scope,
            field,
        }
    }
}

/// What the cache holds for a set of requested fields.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheCoverage {
    /// Fields with a fresh entry.
    pub fresh: Vec<RequestedField>,
    /// Fields with a stale entry.
    pub stale: Vec<RequestedField>,
    /// Fields with no entry.
    pub missing: Vec<RequestedField>,
}

impl CacheCoverage {
    /// Whether every field is fresh.
    pub fn all_fresh(&self) -> bool {
        self.stale.is_empty() && self.missing.is_empty()
    }

    /// Whether every field has an entry (fresh or stale).
    pub fn all_present(&self) -> bool {
        self.missing.is_empty()
    }
}

/// A field-level cache.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheState {
    entries: BTreeMap<CacheKey, CacheEntry>,
}

impl CacheState {
    /// An empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Store (or replace) one entry.
    pub fn insert(&mut self, key: CacheKey, entry: CacheEntry) {
        self.entries.insert(key, entry);
    }

    /// The entry for one key.
    pub fn get(&self, key: &CacheKey) -> Option<&CacheEntry> {
        self.entries.get(key)
    }

    /// The entry for one (identity, scope, field).
    pub fn entry(
        &self,
        identity: &str,
        account_scope: Option<&str>,
        field: RequestedField,
    ) -> Option<&CacheEntry> {
        self.entries.get(&CacheKey::new(
            identity,
            account_scope.map(str::to_string),
            field,
        ))
    }

    /// How many entries the cache holds.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Classify a set of fields against the cache at `now_ms`.
    pub fn coverage(
        &self,
        identity: &str,
        account_scope: Option<&str>,
        fields: impl IntoIterator<Item = RequestedField>,
        ttl: &FreshnessClassTtl,
        now_ms: u64,
    ) -> CacheCoverage {
        let mut coverage = CacheCoverage::default();
        for field in fields {
            match self.entry(identity, account_scope, field) {
                Some(entry) if entry.is_fresh(ttl, now_ms) => coverage.fresh.push(field),
                Some(_) => coverage.stale.push(field),
                None => coverage.missing.push(field),
            }
        }
        coverage
    }

    /// The first validator available among the given fields, preferring
    /// `ETag` over `Last-Modified`, in the caller's field order. This is the
    /// deterministic merge used for conditional requests.
    pub fn conditional_validators(
        &self,
        identity: &str,
        account_scope: Option<&str>,
        fields: impl IntoIterator<Item = RequestedField>,
    ) -> Option<ConditionalValidators> {
        let mut merged = ConditionalValidators::default();
        for field in fields {
            let Some(entry) = self.entry(identity, account_scope, field) else {
                continue;
            };
            if merged.etag.is_none() {
                merged.etag = entry.etag.clone();
            }
            if merged.last_modified.is_none() {
                merged.last_modified = entry.last_modified.clone();
            }
        }
        if merged.is_empty() {
            None
        } else {
            Some(merged)
        }
    }
}

/// HTTP conditional-request validators.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConditionalValidators {
    /// `ETag` value, sent as `If-None-Match`.
    pub etag: Option<String>,
    /// `Last-Modified` value, sent as `If-Modified-Since`.
    pub last_modified: Option<String>,
}

impl ConditionalValidators {
    /// Whether neither validator is present.
    pub fn is_empty(&self) -> bool {
        self.etag.is_none() && self.last_modified.is_none()
    }

    /// The conditional headers in a fixed order: `If-None-Match` first, then
    /// `If-Modified-Since`.
    pub fn headers(&self) -> Vec<(String, String)> {
        let mut headers = Vec::new();
        if let Some(etag) = &self.etag {
            headers.push(("if-none-match".to_string(), etag.clone()));
        }
        if let Some(last_modified) = &self.last_modified {
            headers.push(("if-modified-since".to_string(), last_modified.clone()));
        }
        headers
    }

    /// Extract validators from a response header list.
    pub fn from_headers<'a>(
        headers: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> ConditionalValidators {
        let mut validators = ConditionalValidators::default();
        for (name, value) in headers {
            if name.eq_ignore_ascii_case("etag") && validators.etag.is_none() {
                validators.etag = Some(value.to_string());
            }
            if name.eq_ignore_ascii_case("last-modified") && validators.last_modified.is_none() {
                validators.last_modified = Some(value.to_string());
            }
        }
        validators
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(observed_at_ms: u64, class: FieldFreshnessClass) -> CacheEntry {
        CacheEntry {
            observed_at_ms,
            class,
            etag: None,
            last_modified: None,
            content_digest: None,
            content: None,
        }
    }

    #[test]
    fn freshness_is_per_field_class() {
        let ttl = FreshnessClassTtl::default();
        let fast = entry(0, FieldFreshnessClass::Fast);
        let slow = entry(0, FieldFreshnessClass::Slow);
        assert!(!fast.is_fresh(&ttl, ttl.fast_ms + 1));
        assert!(fast.is_fresh(&ttl, ttl.fast_ms));
        assert!(slow.is_fresh(&ttl, ttl.fast_ms + 1));
        assert!(slow.is_fresh(&ttl, ttl.slow_ms));
        assert!(!slow.is_fresh(&ttl, ttl.slow_ms + 1));
    }

    #[test]
    fn coverage_separates_fresh_stale_and_missing() {
        let mut cache = CacheState::new();
        let ttl = FreshnessClassTtl::default();
        cache.insert(
            CacheKey::new("https://h/p", None, RequestedField::Identity),
            entry(0, FieldFreshnessClass::Slow),
        );
        cache.insert(
            CacheKey::new("https://h/p", None, RequestedField::Availability),
            entry(0, FieldFreshnessClass::Fast),
        );
        let coverage = cache.coverage(
            "https://h/p",
            None,
            [RequestedField::Identity, RequestedField::Availability],
            &ttl,
            ttl.fast_ms + 1,
        );
        assert_eq!(coverage.fresh, vec![RequestedField::Identity]);
        assert_eq!(coverage.stale, vec![RequestedField::Availability]);
        assert!(coverage.missing.is_empty());
        assert!(!coverage.all_fresh());
        assert!(coverage.all_present());

        // Account scopes never share entries.
        let scoped = cache.coverage(
            "https://h/p",
            Some("acct"),
            [RequestedField::Identity],
            &ttl,
            0,
        );
        assert_eq!(scoped.missing, vec![RequestedField::Identity]);
    }

    #[test]
    fn conditional_headers_are_rendered_and_merged_deterministically() {
        let mut cache = CacheState::new();
        let mut first = entry(0, FieldFreshnessClass::Slow);
        first.last_modified = Some("Wed, 21 Oct 2015 07:28:00 GMT".into());
        let mut second = entry(0, FieldFreshnessClass::Slow);
        second.etag = Some("\"abc\"".into());
        cache.insert(
            CacheKey::new("https://h/p", None, RequestedField::Identity),
            first,
        );
        cache.insert(
            CacheKey::new("https://h/p", None, RequestedField::Descriptive),
            second,
        );
        let validators = cache
            .conditional_validators(
                "https://h/p",
                None,
                [RequestedField::Identity, RequestedField::Descriptive],
            )
            .unwrap();
        assert_eq!(validators.etag.as_deref(), Some("\"abc\""));
        assert_eq!(
            validators.last_modified.as_deref(),
            Some("Wed, 21 Oct 2015 07:28:00 GMT")
        );
        assert_eq!(
            validators.headers(),
            vec![
                ("if-none-match".to_string(), "\"abc\"".to_string()),
                (
                    "if-modified-since".to_string(),
                    "Wed, 21 Oct 2015 07:28:00 GMT".to_string()
                ),
            ]
        );
        assert!(cache
            .conditional_validators("https://h/p", None, [RequestedField::BulkSet])
            .is_none());
    }

    #[test]
    fn response_validators_are_extracted_case_insensitively() {
        let validators = ConditionalValidators::from_headers([
            ("ETag", "\"v1\""),
            ("etag", "\"v2\""),
            ("Last-Modified", "now"),
        ]);
        assert_eq!(validators.etag.as_deref(), Some("\"v1\""));
        assert_eq!(validators.last_modified.as_deref(), Some("now"));
        assert!(!validators.is_empty());
    }
}
