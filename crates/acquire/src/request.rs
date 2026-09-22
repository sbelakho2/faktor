//! Acquisition requests, requested fields, freshness and the normalized
//! acquisition identity that keys coalescing and cache entries.
//!
//! An [`AcquisitionRequest`] is deliberately opaque about what the datum
//! *means*: it names an identity (a URL or an opaque ref), the fields the
//! caller wants, and how fresh they must be. Normalization is conservative:
//! it only lowercases the scheme and host of URL-like identities, drops the
//! fragment and default ports, and never reorders or rewrites the query, so
//! two distinct requests can never be coalesced by accident.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{invalid, AcquisitionError};

/// Bound on one identity string.
pub const MAX_IDENTITY_BYTES: usize = 2048;
/// Bound on one account scope string.
pub const MAX_ACCOUNT_SCOPE_BYTES: usize = 128;

/// One class of datum a request can ask for.
///
/// The runtime does not interpret the fields; it uses them to decide which
/// mechanisms can serve the request and which freshness class applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestedField {
    /// Stable identifiers of the datum.
    Identity,
    /// Free-form descriptive data.
    Descriptive,
    /// Availability state.
    Availability,
    /// Quantity-dependent tiers.
    QuantityTiers,
    /// Packaging options.
    Packaging,
    /// Account-scoped terms.
    AccountTerms,
    /// Provenance of an earlier observation.
    Provenance,
    /// A bulk set of related items.
    BulkSet,
}

/// Every requested field, in canonical order.
pub const ALL_REQUESTED_FIELDS: [RequestedField; 8] = [
    RequestedField::Identity,
    RequestedField::Descriptive,
    RequestedField::Availability,
    RequestedField::QuantityTiers,
    RequestedField::Packaging,
    RequestedField::AccountTerms,
    RequestedField::Provenance,
    RequestedField::BulkSet,
];

/// A canonical, deduplicated set of requested fields.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestedFields(BTreeSet<RequestedField>);

impl RequestedFields {
    /// The empty set. The planner refuses an empty set with a typed
    /// `InvalidRequest`.
    pub fn none() -> Self {
        Self(BTreeSet::new())
    }

    /// Every field.
    pub fn all() -> Self {
        Self(ALL_REQUESTED_FIELDS.into_iter().collect())
    }

    /// The canonical set of the given fields.
    pub fn of(fields: impl IntoIterator<Item = RequestedField>) -> Self {
        Self(fields.into_iter().collect())
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// How many fields are requested.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the field is requested.
    pub fn contains(&self, field: RequestedField) -> bool {
        self.0.contains(&field)
    }

    /// Iterate the requested fields in canonical order.
    pub fn iter(&self) -> impl Iterator<Item = RequestedField> + '_ {
        self.0.iter().copied()
    }
}

impl FromIterator<RequestedField> for RequestedFields {
    fn from_iter<T: IntoIterator<Item = RequestedField>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// How fresh the caller needs the datum to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestedFreshness {
    /// Serve a fresh cache entry if one exists, otherwise acquire live.
    PreferCache,
    /// Always acquire live.
    Live,
    /// Never touch the network; serve the cache even if stale (flagged), or
    /// refuse.
    CacheOnly,
}

/// One acquisition request.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AcquisitionRequest {
    /// The raw identity (URL or opaque ref); normalized for keys.
    pub identity: String,
    /// The fields the caller wants.
    pub fields: RequestedFields,
    /// How fresh the result must be.
    pub freshness: RequestedFreshness,
    /// Account scope; account-scoped data is never coalesced or cached
    /// across scopes.
    pub account_scope: Option<String>,
}

impl AcquisitionRequest {
    /// Build and validate a request.
    pub fn new(
        identity: impl Into<String>,
        fields: RequestedFields,
        freshness: RequestedFreshness,
    ) -> Result<Self, AcquisitionError> {
        let request = Self {
            identity: identity.into(),
            fields,
            freshness,
            account_scope: None,
        };
        request.validate()?;
        Ok(request)
    }

    /// Attach an account scope (validated).
    pub fn with_account_scope(
        mut self,
        account_scope: impl Into<String>,
    ) -> Result<Self, AcquisitionError> {
        let scope = account_scope.into();
        validate_scope(&scope)?;
        self.account_scope = Some(scope);
        Ok(self)
    }

    /// Validate identity and scope bounds.
    pub fn validate(&self) -> Result<(), AcquisitionError> {
        normalize_identity(&self.identity)?;
        if let Some(scope) = &self.account_scope {
            validate_scope(scope)?;
        }
        Ok(())
    }

    /// The normalized identity.
    pub fn normalized_identity(&self) -> Result<String, AcquisitionError> {
        normalize_identity(&self.identity)
    }

    /// The key that coalescing uses: normalized identity + account scope +
    /// requested fields + freshness policy. Two callers with different
    /// freshness policies or scopes never share one external request.
    pub fn coalescing_key(&self) -> Result<AcquisitionKey, AcquisitionError> {
        Ok(AcquisitionKey {
            identity: normalize_identity(&self.identity)?,
            account_scope: self.account_scope.clone(),
            fields: self.fields.clone(),
            freshness: self.freshness,
        })
    }
}

/// The normalized key of one acquisition.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AcquisitionKey {
    /// Normalized identity.
    pub identity: String,
    /// Account scope, if any.
    pub account_scope: Option<String>,
    /// Requested fields.
    pub fields: RequestedFields,
    /// Freshness policy.
    pub freshness: RequestedFreshness,
}

impl fmt::Display for AcquisitionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.identity)?;
        if let Some(scope) = &self.account_scope {
            write!(f, " [{scope}]")?;
        }
        Ok(())
    }
}

/// Normalize one identity conservatively.
///
/// * URL-like identities (`http://` / `https://`): lowercase scheme and
///   host, drop the fragment, drop `:80`/`:443` default ports, keep the path
///   and query exactly, and refuse embedded credentials. A missing path
///   becomes `/`.
/// * Any other identity: trim and collapse internal whitespace runs; the
///   case is preserved because opaque refs may be case-sensitive.
///
/// Control characters and over-long identities are refused.
pub fn normalize_identity(raw: &str) -> Result<String, AcquisitionError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(invalid("identity is empty"));
    }
    if trimmed.len() > MAX_IDENTITY_BYTES {
        return Err(invalid(format!(
            "identity exceeds {MAX_IDENTITY_BYTES} bytes"
        )));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(invalid("identity contains control characters"));
    }
    if let Some(rest) = trimmed.get(7..).filter(|_| {
        trimmed
            .get(..7)
            .is_some_and(|head| head.eq_ignore_ascii_case("http://"))
    }) {
        normalize_url("http", rest)
    } else if let Some(rest) = trimmed.get(8..).filter(|_| {
        trimmed
            .get(..8)
            .is_some_and(|head| head.eq_ignore_ascii_case("https://"))
    }) {
        normalize_url("https", rest)
    } else {
        Ok(collapse_whitespace(trimmed))
    }
}

fn normalize_url(scheme: &str, rest: &str) -> Result<String, AcquisitionError> {
    let without_fragment = rest.split('#').next().unwrap_or(rest);
    let (authority, path_and_query) = match without_fragment.find('/') {
        Some(index) => (&without_fragment[..index], &without_fragment[index..]),
        None => (without_fragment, ""),
    };
    if authority.is_empty() {
        return Err(invalid("identity has no host"));
    }
    if authority.contains('@') {
        return Err(invalid("identity must not carry credentials"));
    }
    let host = strip_default_port(&authority.to_ascii_lowercase(), scheme);
    let mut out = String::with_capacity(scheme.len() + 3 + host.len() + path_and_query.len());
    out.push_str(scheme);
    out.push_str("://");
    out.push_str(&host);
    if path_and_query.is_empty() {
        out.push('/');
    } else {
        out.push_str(path_and_query);
    }
    if out.len() > MAX_IDENTITY_BYTES {
        return Err(invalid(format!(
            "identity exceeds {MAX_IDENTITY_BYTES} bytes"
        )));
    }
    Ok(out)
}

fn strip_default_port(authority: &str, scheme: &str) -> String {
    let default = if scheme == "http" { ":80" } else { ":443" };
    match authority.strip_suffix(default) {
        Some(host) if !host.is_empty() => host.to_string(),
        _ => authority.to_string(),
    }
}

fn collapse_whitespace(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn validate_scope(scope: &str) -> Result<(), AcquisitionError> {
    if scope.is_empty() {
        return Err(invalid("account scope is empty"));
    }
    if scope.len() > MAX_ACCOUNT_SCOPE_BYTES {
        return Err(invalid(format!(
            "account scope exceeds {MAX_ACCOUNT_SCOPE_BYTES} bytes"
        )));
    }
    if scope.chars().any(char::is_control) {
        return Err(invalid("account scope contains control characters"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_normalization_is_conservative() {
        assert_eq!(
            normalize_identity(" HTTPS://Example.COM:443/a/B?Q=1#frag ").unwrap(),
            "https://example.com/a/B?Q=1"
        );
        assert_eq!(
            normalize_identity("http://example.com:80").unwrap(),
            "http://example.com/"
        );
        assert_eq!(
            normalize_identity("https://example.com:8443/x").unwrap(),
            "https://example.com:8443/x"
        );
        // Query order and case are preserved: distinct queries stay distinct.
        assert_ne!(
            normalize_identity("https://h/?a=1&b=2").unwrap(),
            normalize_identity("https://h/?b=2&a=1").unwrap()
        );
    }

    #[test]
    fn opaque_identities_collapse_whitespace_but_keep_case() {
        assert_eq!(normalize_identity("  Ref:AB 12 ").unwrap(), "Ref:AB 12");
        assert_eq!(normalize_identity("ref:ab").unwrap(), "ref:ab");
    }

    #[test]
    fn hostile_identities_are_refused() {
        assert!(normalize_identity("").is_err());
        assert!(normalize_identity("   ").is_err());
        assert!(normalize_identity("https://user:pass@h/x").is_err());
        assert!(normalize_identity("https://").is_err());
        assert!(normalize_identity("a\u{0}b").is_err());
        assert!(normalize_identity(&"x".repeat(MAX_IDENTITY_BYTES + 1)).is_err());
    }

    #[test]
    fn coalescing_key_separates_freshness_scope_and_fields() {
        let base = AcquisitionRequest::new(
            "https://h/p",
            RequestedFields::of([RequestedField::Identity]),
            RequestedFreshness::Live,
        )
        .unwrap();
        let mut other = base.clone();
        other.freshness = RequestedFreshness::CacheOnly;
        assert_ne!(
            base.coalescing_key().unwrap(),
            other.coalescing_key().unwrap()
        );

        let scoped = base.clone().with_account_scope("acct-1").unwrap();
        assert_ne!(
            base.coalescing_key().unwrap(),
            scoped.coalescing_key().unwrap()
        );

        let mut wider = base.clone();
        wider.fields = RequestedFields::all();
        assert_ne!(
            base.coalescing_key().unwrap(),
            wider.coalescing_key().unwrap()
        );

        // Field order does not matter: the set is canonical.
        let a = AcquisitionRequest::new(
            "https://h/p",
            RequestedFields::of([RequestedField::Availability, RequestedField::Identity]),
            RequestedFreshness::Live,
        )
        .unwrap();
        let b = AcquisitionRequest::new(
            "https://h/p",
            RequestedFields::of([RequestedField::Identity, RequestedField::Availability]),
            RequestedFreshness::Live,
        )
        .unwrap();
        assert_eq!(a.coalescing_key().unwrap(), b.coalescing_key().unwrap());
    }

    #[test]
    fn empty_field_set_is_constructible_but_visible() {
        let request = AcquisitionRequest::new(
            "https://h/p",
            RequestedFields::none(),
            RequestedFreshness::Live,
        )
        .unwrap();
        assert!(request.fields.is_empty());
    }
}
