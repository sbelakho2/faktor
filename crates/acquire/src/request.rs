//! Acquisition requests, requested fields, freshness and the normalized
//! acquisition identity that keys coalescing and cache entries.
//!
//! An [`AcquisitionRequest`] is deliberately opaque about what the datum
//! *means*: it names an identity (a URL or an opaque ref), the fields the
//! caller wants, and how fresh they must be. Normalization is conservative:
//! URL-like identities canonicalize their host through the ONE shared
//! destination authority (`faktor_security::destination::canonicalize_request_host`)
//! so identity keys agree with the egress destination gate, lowercase the
//! scheme, drop the fragment and default ports, validate non-default ports
//! and never reorder or rewrite the query, so two distinct requests can
//! never be coalesced by accident.

use std::collections::BTreeSet;
use std::fmt;

use faktor_security::destination::canonicalize_request_host;
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
/// * URL-like identities (`http://` / `https://`): the host is canonicalized
///   by the ONE shared destination authority
///   ([`canonicalize_request_host`], `faktor-security::destination`), so an
///   identity key and the egress destination gate can never disagree about
///   which host a spelling names (lowercase, trailing-dot strip, UTS-46
///   IDN/punycode, IPv4/IPv6 literal canonicalization, strict label
///   grammar); the scheme is lowercased, the fragment is dropped, the
///   default port (`:80`/`:443`) is stripped and a non-default port is
///   validated (`1..=65535`); the path and query are kept exactly; embedded
///   credentials are refused. A missing path becomes `/`.
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
    let (host_raw, port_raw) = split_authority(authority)?;
    let host = canonicalize_request_host(host_raw, false, None).map_err(|reason| {
        invalid(format!(
            "identity host {host_raw:?} is not a canonical destination host: {reason}"
        ))
    })?;
    let port = canonical_port(port_raw, scheme)?;
    let mut out = String::with_capacity(scheme.len() + 3 + host.len() + 8 + path_and_query.len());
    out.push_str(scheme);
    out.push_str("://");
    if host.contains(':') {
        // Unbracketed canonical IPv6 text is re-bracketed for the URL form.
        out.push('[');
        out.push_str(&host);
        out.push(']');
    } else {
        out.push_str(&host);
    }
    if let Some(port) = port {
        out.push(':');
        out.push_str(&port.to_string());
    }
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

/// Split a URL authority into (host text, optional port text): bracketed
/// IPv6 hosts are de-bracketed, a single `:` splits host and port, and an
/// unbracketed multi-`:` authority is treated as an IPv6 literal (the shared
/// canonicalizer then validates or refuses it).
fn split_authority(authority: &str) -> Result<(&str, Option<&str>), AcquisitionError> {
    if let Some(rest) = authority.strip_prefix('[') {
        let Some(end) = rest.find(']') else {
            return Err(invalid("identity has an unterminated IPv6 host"));
        };
        let host = &rest[..end];
        let after = &rest[end + 1..];
        if after.is_empty() {
            return Ok((host, None));
        }
        let Some(port) = after.strip_prefix(':') else {
            return Err(invalid("identity has a malformed host:port"));
        };
        return Ok((host, Some(port)));
    }
    if authority.matches(':').count() == 1 {
        let (host, port) = authority.split_once(':').expect("one colon present");
        return Ok((host, Some(port)));
    }
    Ok((authority, None))
}

/// Canonicalize one optional port: digits only, `1..=65535`; the scheme's
/// default port (`http` 80 / `https` 443) is dropped so both spellings key
/// identically.
fn canonical_port(raw: Option<&str>, scheme: &str) -> Result<Option<u16>, AcquisitionError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let port: u16 = raw
        .parse()
        .ok()
        .filter(|_| !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()))
        .filter(|port| *port != 0)
        .ok_or_else(|| invalid("identity port must be a number 1..=65535"))?;
    let default = if scheme == "http" { 80 } else { 443 };
    Ok((port != default).then_some(port))
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

    /// Audit bypass 5: identity canonicalization must be the SAME authority
    /// the egress destination gate matches against, or one side accepts a
    /// spelling the other treats as a different host. The normalized host is
    /// exactly `canonicalize_request_host`'s output (and a fixed point of
    /// it), so identity keys agree with destination authority keys.
    #[test]
    fn identity_host_canonicalization_is_the_shared_destination_authority() {
        for (raw, host) in [
            ("https://EXAMPLE.com./x", "EXAMPLE.com."),
            ("https://exämple.com/x", "exämple.com"),
            ("http://127.000.0.01/x", "127.000.0.01"),
            ("https://[0:0:0:0:0:0:0:1]/x", "0:0:0:0:0:0:0:1"),
            ("https://Example.COM:8443/x", "Example.COM"),
            ("http://localhost:8080/x", "localhost"),
        ] {
            let canonical = canonicalize_request_host(host, false, None).unwrap();
            let normalized = normalize_identity(raw).unwrap();
            let host_in_url = if canonical.contains(':') {
                format!("[{canonical}]")
            } else {
                canonical.clone()
            };
            assert!(
                normalized.contains(&format!("://{host_in_url}")),
                "{raw} normalized to {normalized}, but the destination authority says {canonical:?}"
            );
            // The shared authority's output is itself canonical: both sides
            // are fixed points of the same function.
            assert_eq!(
                canonicalize_request_host(&canonical, false, None).unwrap(),
                canonical,
                "destination canonicalization must be idempotent"
            );
        }
        // Equivalent spellings produce the SAME coalescing key, so the
        // identity cannot be used to smuggle a second name for one host past
        // coalescing/caching.
        let a = AcquisitionRequest::new(
            "https://EXAMPLE.com./p",
            RequestedFields::of([RequestedField::Identity]),
            RequestedFreshness::Live,
        )
        .unwrap();
        let b = AcquisitionRequest::new(
            "https://example.com/p",
            RequestedFields::of([RequestedField::Identity]),
            RequestedFreshness::Live,
        )
        .unwrap();
        assert_eq!(
            a.coalescing_key().unwrap(),
            b.coalescing_key().unwrap(),
            "equivalent host spellings must coalesce identically"
        );
    }

    #[test]
    fn non_canonical_destination_hosts_and_ports_are_refused() {
        for hostile in [
            "https://my_host/x",
            "https://-bad.com/x",
            "https://bad-.com/x",
            "https://h:0/x",
            "https://h:99999/x",
            "https://h:abc/x",
            "https://[::1/x",
        ] {
            assert!(
                normalize_identity(hostile).is_err(),
                "{hostile} must be refused: the destination gate would refuse it"
            );
        }
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
