//! Bounded, validated text, identity and URL newtypes.
//!
//! Every string that enters the commerce domain from an external source is
//! hostile input until proven otherwise. The types in this module validate
//! at the boundary (serde deserialization and constructors):
//!
//! - byte length is bounded per field,
//! - control characters are rejected (a site label can never smuggle a
//!   terminal escape or a NUL into logs),
//! - Unicode bidirectional control characters are rejected (no spoofed
//!   part numbers via RTL overrides),
//! - [`CanonicalUrl`] is produced by the same trusted URL parser used at
//!   transport boundaries (the `url` crate), never a second parser, with
//!   the host canonicalized through the shared destination authority:
//!   only absolute `http`/`https` URLs, no userinfo (credentials can never
//!   hide in a canonical URL), UTS-46 punycode/IDN hosts, canonical
//!   IPv4/IPv6, default ports removed, empty path normalized to `/`, no
//!   fragment (the fragment is stripped during canonicalization: it is
//!   never sent in a request and never participates in identity), and a
//!   bounded length.

use faktor_security::destination::canonicalize_request_host;
use serde::de::{self, Deserializer};
use serde::{Serialize, Serializer};
use std::fmt;

/// Hard bound for a [`SourceId`].
pub const MAX_SOURCE_ID_BYTES: usize = 32;
/// Hard bound for an [`AccountScope`].
pub const MAX_ACCOUNT_SCOPE_BYTES: usize = 128;
/// Hard bound for a [`VariantId`].
pub const MAX_VARIANT_ID_BYTES: usize = 128;
/// Hard bound for a [`CanonicalUrl`].
pub const MAX_URL_BYTES: usize = 2048;

/// A rejected text value. Every variant is typed: hostile input never
/// silently degrades into a lossy string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TextError {
    /// The value is empty (or whitespace-only) after trimming.
    #[error("text is empty after trimming")]
    Empty,
    /// The value exceeds the field bound (bytes, not characters).
    #[error("text exceeds {max} bytes (got {actual})")]
    TooLong { max: usize, actual: usize },
    /// The value contains an ASCII/Latin control character.
    #[error("text contains a control character at byte {at}")]
    ControlChar { at: usize },
    /// The value contains a Unicode bidirectional control character.
    #[error("text contains a bidirectional control character at byte {at}")]
    BidiControl { at: usize },
}

/// A bounded, validated, trimmed string.
///
/// `MAX` is the byte bound of the field this text belongs to. Validation
/// happens on construction and on deserialization, so a `Text` value in
/// memory is always non-empty, trimmed, control-character-free and within
/// its bound.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Text<const MAX: usize>(String);

impl<const MAX: usize> Text<MAX> {
    /// Validate and construct.
    pub fn new(raw: &str) -> Result<Self, TextError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(TextError::Empty);
        }
        if trimmed.len() > MAX {
            return Err(TextError::TooLong {
                max: MAX,
                actual: trimmed.len(),
            });
        }
        for (at, ch) in trimmed.char_indices() {
            if ch.is_control() {
                return Err(TextError::ControlChar { at });
            }
            if is_bidi_control(ch) {
                return Err(TextError::BidiControl { at });
            }
        }
        Ok(Self(trimmed.to_string()))
    }

    /// The validated text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Byte length (always within `MAX`).
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Always `false`: a [`Text`] is validated non-empty at construction.
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// Internal unchecked constructor for compile-time-known labels.
    ///
    /// Callers must prove the literal is non-empty, trimmed, control-free
    /// and within `MAX`; the label tables in [`crate::error`] are covered
    /// by a unit test that validates every static label through
    /// [`Text::new`], so the unchecked path is unreachable in practice.
    pub(crate) fn from_static_label(value: &'static str) -> Self {
        Self(value.to_string())
    }
}

impl<const MAX: usize> AsRef<str> for Text<MAX> {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<const MAX: usize> fmt::Display for Text<MAX> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<const MAX: usize> Serialize for Text<MAX> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de, const MAX: usize> serde::Deserialize<'de> for Text<MAX> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_validated(deserializer, |raw| {
            Text::<MAX>::new(raw).map_err(|e| e.to_string())
        })
    }
}

/// True for Unicode bidi formatting controls (spoofing vector).
pub fn is_bidi_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

struct ValidatedStrVisitor<F>(F);

impl<'de, T, F> de::Visitor<'de> for ValidatedStrVisitor<F>
where
    F: Fn(&str) -> Result<T, String>,
{
    type Value = T;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a string")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<T, E> {
        (self.0)(value).map_err(E::custom)
    }
}

/// Deserialize a string and run `validate` on it, turning any rejection
/// into a typed serde error.
pub(crate) fn deserialize_validated<'de, D, T, F>(
    deserializer: D,
    validate: F,
) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    F: Fn(&str) -> Result<T, String>,
{
    deserializer.deserialize_str(ValidatedStrVisitor(validate))
}

/// A rejected identity string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    /// The text-level validation failed.
    #[error(transparent)]
    Text(#[from] TextError),
    /// The first character must be ASCII alphanumeric.
    #[error("identifier must start with an ASCII letter or digit")]
    InvalidFirstChar,
    /// A character outside the allowed set.
    #[error("identifier contains a disallowed character at byte {at}")]
    InvalidChar { at: usize },
}

fn validate_id_charset(value: &str) -> Result<(), IdError> {
    let mut chars = value.char_indices();
    match chars.next() {
        Some((_, ch)) if ch.is_ascii_alphanumeric() => {}
        Some(_) => return Err(IdError::InvalidFirstChar),
        None => return Err(IdError::Text(TextError::Empty)),
    }
    for (at, ch) in chars {
        if !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '-' | '_' | '.')) {
            return Err(IdError::InvalidChar { at });
        }
    }
    Ok(())
}

/// The identifier of a commerce source (marketplace or supplier feed).
///
/// Case-insensitive: `"Mouser"` and `"mouser"` are the same source, so a
/// caller can never fragment cache identity by capitalization.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceId(Text<MAX_SOURCE_ID_BYTES>);

impl SourceId {
    /// Validate and normalize (lowercase) a source identifier.
    pub fn new(raw: &str) -> Result<Self, IdError> {
        let lowered = raw.trim().to_ascii_lowercase();
        let text = Text::<MAX_SOURCE_ID_BYTES>::new(&lowered)?;
        validate_id_charset(text.as_str())?;
        Ok(Self(text))
    }

    /// The canonical lowercase identifier.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl Serialize for SourceId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for SourceId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_validated(deserializer, |raw| {
            SourceId::new(raw).map_err(|e| e.to_string())
        })
    }
}

/// The account scope a price was observed under.
///
/// Account-specific commercial data is never shared across account scopes;
/// this type is the identity that scoping keys are built from.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AccountScope(Text<MAX_ACCOUNT_SCOPE_BYTES>);

impl AccountScope {
    /// Validate and construct.
    pub fn new(raw: &str) -> Result<Self, TextError> {
        Text::<MAX_ACCOUNT_SCOPE_BYTES>::new(raw).map(Self)
    }

    /// The scope label.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for AccountScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl Serialize for AccountScope {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for AccountScope {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_validated(deserializer, |raw| {
            AccountScope::new(raw).map_err(|e| e.to_string())
        })
    }
}

/// The opaque identifier of a variant (a purchasable SKU under one offer).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VariantId(Text<MAX_VARIANT_ID_BYTES>);

impl VariantId {
    /// Validate and construct.
    pub fn new(raw: &str) -> Result<Self, TextError> {
        Text::<MAX_VARIANT_ID_BYTES>::new(raw).map(Self)
    }

    /// The variant identifier.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for VariantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl Serialize for VariantId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for VariantId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_validated(deserializer, |raw| {
            VariantId::new(raw).map_err(|e| e.to_string())
        })
    }
}

/// A rejected URL.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UrlError {
    /// Empty after trimming.
    #[error("url is empty")]
    Empty,
    /// Longer than [`MAX_URL_BYTES`].
    #[error("url exceeds {MAX_URL_BYTES} bytes (got {actual})")]
    TooLong { actual: usize },
    /// Contains a control character.
    #[error("url contains a control character at byte {at}")]
    ControlChar { at: usize },
    /// No `scheme://` prefix and no absolute URL.
    #[error("url has no scheme://authority")]
    MissingScheme,
    /// A scheme other than `http` or `https`.
    #[error("url scheme {scheme:?} is not http or https")]
    UnsupportedScheme { scheme: String },
    /// No authority (host) part.
    #[error("url has no host")]
    MissingHost,
    /// Userinfo (`user:password@host`) is never allowed in a canonical URL.
    #[error("url carries userinfo; credentials are never part of a canonical url")]
    UserInfo,
    /// The host fails the shared destination-authority canonicalization
    /// (invalid label, malformed IP literal, ambiguous numeric label).
    #[error("url host is invalid")]
    InvalidHost,
    /// Port zero, which is never a dialable destination.
    #[error("url port is invalid")]
    InvalidPort,
    /// The trusted URL parser rejected the input (bad percent-escape,
    /// forbidden host code point, malformed IP literal, ...).
    #[error("url rejected by the URL parser: {reason}")]
    Malformed { reason: String },
}

/// THE canonical marketplace URL.
///
/// There is no second URL parser here: the input is parsed by the same
/// trusted parser used at transport/egress boundaries (the `url` crate),
/// then the host is canonicalized through the shared destination authority
/// ([`faktor_security::destination::canonicalize_request_host`]) so
/// marketplace identity and egress can never disagree on host semantics:
/// ASCII lowercase, one trailing dot stripped, UTS-46 punycode for IDNs,
/// canonical dotted-quad IPv4 and canonical unbracketed IPv6.
///
/// Rejected: non-`http(s)` schemes, userinfo (credentials can never hide in
/// a canonical URL), port `0`, and hosts the shared authority refuses.
/// Default ports are removed, an empty path becomes `/`, and the fragment
/// is stripped — RFC 3986 fragments are never sent in an HTTP request, so
/// they must not participate in identity. The canonical serialization is
/// `scheme://host[:port]/path?query`; `Eq`/`Hash`/`Ord`, every identity key
/// and every digest therefore see the fragment-free canonical form only:
/// `https://x.test:443/product#one` and `https://x.test/product#two` are
/// the same value. The raw input fragment is deliberately not retained; no
/// caller in the workspace needs it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CanonicalUrl {
    /// The one canonical serialization.
    text: String,
    /// Canonical host only, unbracketed (punycode name, dotted-quad IPv4,
    /// or canonical IPv6 text).
    host: String,
    /// The origin prefix `scheme://host[:port]` (never a path or query).
    origin: String,
    /// Always `"http"` or `"https"`.
    scheme: &'static str,
}

fn map_parse_error(error: url::ParseError) -> UrlError {
    use url::ParseError as P;
    match error {
        P::EmptyHost => UrlError::MissingHost,
        P::InvalidPort | P::Overflow => UrlError::InvalidPort,
        P::InvalidIpv4Address | P::InvalidIpv6Address | P::InvalidDomainCharacter => {
            UrlError::InvalidHost
        }
        P::IdnaError => UrlError::InvalidHost,
        P::RelativeUrlWithoutBase | P::RelativeUrlWithCannotBeABaseBase => UrlError::MissingScheme,
        other => UrlError::Malformed {
            reason: other.to_string(),
        },
    }
}

impl CanonicalUrl {
    /// Parse and canonicalize. The only input lexing here is the control
    /// character rejection; scheme/host/port/path/query semantics come from
    /// the trusted parser.
    pub fn parse(raw: &str) -> Result<Self, UrlError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(UrlError::Empty);
        }
        if trimmed.len() > MAX_URL_BYTES {
            return Err(UrlError::TooLong {
                actual: trimmed.len(),
            });
        }
        if let Some((at, _)) = trimmed.char_indices().find(|(_, ch)| ch.is_control()) {
            return Err(UrlError::ControlChar { at });
        }
        let mut parsed = url::Url::parse(trimmed).map_err(map_parse_error)?;
        let scheme = match parsed.scheme() {
            "http" => "http",
            "https" => "https",
            other => {
                return Err(UrlError::UnsupportedScheme {
                    scheme: other.to_string(),
                })
            }
        };
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(UrlError::UserInfo);
        }
        let Some(host) = parsed.host() else {
            return Err(UrlError::MissingHost);
        };
        let (host_text, is_ipv4, ip) = match host {
            url::Host::Domain(name) => (name.to_string(), false, None),
            url::Host::Ipv4(v4) => (v4.to_string(), true, Some(v4.octets())),
            url::Host::Ipv6(v6) => (v6.to_string(), false, None),
        };
        let canonical_host = canonicalize_request_host(&host_text, is_ipv4, ip)
            .map_err(|_| UrlError::InvalidHost)?;
        // Port 0 is never a dialable destination; the parser already keeps
        // non-default ports and drops default ones (`:443` on https).
        if parsed.port() == Some(0) {
            return Err(UrlError::InvalidPort);
        }
        let set_host = if canonical_host.contains(':') {
            // IPv6: `Url::set_host` takes the bracketed literal.
            format!("[{canonical_host}]")
        } else {
            canonical_host.clone()
        };
        parsed.set_host(Some(&set_host)).map_err(map_parse_error)?;
        parsed.set_fragment(None);
        // The parser normalized the empty path to `/` for http(s) already;
        // re-serializing the host-swapped URL is the one canonical form.
        let text = parsed.to_string();
        if text.len() > MAX_URL_BYTES {
            return Err(UrlError::TooLong { actual: text.len() });
        }
        // The serialization is exactly `origin + path + "?" + query`; derive
        // the origin slice from the parsed parts (never a re-split of text).
        let tail_len = parsed.path().len() + parsed.query().map(|q| q.len() + 1).unwrap_or(0);
        let origin_end = text
            .len()
            .checked_sub(tail_len)
            .ok_or(UrlError::Malformed {
                reason: "serialized url shorter than its path".into(),
            })?;
        let origin = text[..origin_end].to_string();
        Ok(Self {
            text,
            host: canonical_host,
            origin,
            scheme,
        })
    }

    /// The canonical URL: `scheme://host[:port]/path?query`. The fragment
    /// is not part of the canonical form (it is stripped at parse time).
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The scheme (always `http` or `https`).
    pub fn scheme(&self) -> &str {
        self.scheme
    }

    /// The canonical host without port: lowercase, trailing-dot-free,
    /// punycode for IDNs, canonical IPv4/IPv6 text (IPv6 unbracketed).
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The origin (`scheme://host[:port]`), the part that never varies with
    /// path/query.
    pub fn origin(&self) -> &str {
        &self.origin
    }
}

impl fmt::Display for CanonicalUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl Serialize for CanonicalUrl {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.text)
    }
}

impl<'de> serde::Deserialize<'de> for CanonicalUrl {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_validated(deserializer, |raw| {
            CanonicalUrl::parse(raw).map_err(|e| e.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_trims_and_rejects_empty() {
        let text = Text::<16>::new("  hello  ").expect("valid");
        assert_eq!(text.as_str(), "hello");
        assert!(matches!(Text::<16>::new("   "), Err(TextError::Empty)));
        assert!(matches!(Text::<16>::new(""), Err(TextError::Empty)));
    }

    #[test]
    fn text_rejects_overlong_control_and_bidi() {
        let long = "x".repeat(17);
        assert!(matches!(
            Text::<16>::new(&long),
            Err(TextError::TooLong {
                max: 16,
                actual: 17
            })
        ));
        assert!(matches!(
            Text::<16>::new("ab\ncd"),
            Err(TextError::ControlChar { .. })
        ));
        assert!(matches!(
            Text::<16>::new("ab\u{202e}cd"),
            Err(TextError::BidiControl { .. })
        ));
    }

    #[test]
    fn text_bound_is_bytes_not_chars() {
        let four_byte = "\u{1f600}\u{1f600}\u{1f600}";
        assert_eq!(four_byte.len(), 12);
        assert!(Text::<12>::new(four_byte).is_ok());
        assert!(matches!(
            Text::<11>::new(four_byte),
            Err(TextError::TooLong { .. })
        ));
    }

    #[test]
    fn source_id_normalizes_case_and_rejects_hostile() {
        let a = SourceId::new("Mouser").expect("valid");
        let b = SourceId::new(" mouser ").expect("valid");
        assert_eq!(a, b);
        assert_eq!(a.as_str(), "mouser");
        assert!(SourceId::new("1688").is_ok());
        assert!(matches!(
            SourceId::new("-evil"),
            Err(IdError::InvalidFirstChar)
        ));
        assert!(matches!(
            SourceId::new("a b"),
            Err(IdError::InvalidChar { .. })
        ));
        assert!(matches!(SourceId::new(""), Err(IdError::Text(_))));
        assert!(matches!(
            SourceId::new(&"a".repeat(33)),
            Err(IdError::Text(TextError::TooLong { .. }))
        ));
    }

    #[test]
    fn variant_and_account_ids_are_bounded() {
        assert!(VariantId::new("reel-5000").is_ok());
        assert!(AccountScope::new("procurement-cn").is_ok());
        assert!(matches!(
            AccountScope::new("x".repeat(129).as_str()),
            Err(TextError::TooLong { .. })
        ));
    }

    #[test]
    fn url_accepts_and_normalizes_real_marketplace_urls() {
        let url =
            CanonicalUrl::parse("HTTPS://Detail.1688.COM/offer/123.html?x=1#frag").expect("valid");
        assert_eq!(url.as_str(), "https://detail.1688.com/offer/123.html?x=1");
        assert!(!url.as_str().contains('#'));
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host(), "detail.1688.com");
        assert_eq!(url.origin(), "https://detail.1688.com");
        assert_eq!(
            CanonicalUrl::parse("http://localhost:8080/a")
                .expect("valid")
                .as_str(),
            "http://localhost:8080/a"
        );
        // A default port is not identity: `:443` (even zero-padded) is
        // removed and serialized exactly like the bare origin.
        let default_port = CanonicalUrl::parse("https://good.com:0443/").expect("valid");
        assert_eq!(default_port.as_str(), "https://good.com/");
        assert_eq!(default_port.origin(), "https://good.com");
    }

    #[test]
    fn url_fragments_are_stripped_and_do_not_participate_in_identity() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let one = CanonicalUrl::parse("https://x.test/product#one").expect("valid");
        let two = CanonicalUrl::parse("https://x.test/product#two").expect("valid");
        assert_eq!(one, two, "fragments are not identity");
        assert_eq!(one.cmp(&two), std::cmp::Ordering::Equal);
        let hash = |url: &CanonicalUrl| {
            let mut hasher = DefaultHasher::new();
            url.hash(&mut hasher);
            hasher.finish()
        };
        assert_eq!(hash(&one), hash(&two), "fragments are not a hash identity");
        assert_eq!(one.as_str(), "https://x.test/product");
        assert_eq!(two.as_str(), "https://x.test/product");
        assert_eq!(
            one.as_str(),
            CanonicalUrl::parse("https://x.test/product")
                .expect("valid")
                .as_str(),
            "a fragmentless URL has the same canonical form"
        );

        // Fragment stripping is positional (RFC 3986): everything from the
        // first `#` is dropped, even when it precedes or contains `?`/`/`.
        assert_eq!(
            CanonicalUrl::parse("https://x.test/a?b=1#frag?c=2")
                .expect("valid")
                .as_str(),
            "https://x.test/a?b=1"
        );
        assert_eq!(
            CanonicalUrl::parse("https://x.test/a#frag?b=1")
                .expect("valid")
                .as_str(),
            "https://x.test/a"
        );
        // An empty path is normalized to `/`, so the fragment-free form of a
        // bare origin is the origin + `/`.
        assert_eq!(
            CanonicalUrl::parse("https://x.test#frag")
                .expect("valid")
                .as_str(),
            "https://x.test/"
        );

        // Path/query canonicalization rules are untouched, and an encoded
        // `#` inside the query stays data.
        assert_eq!(
            CanonicalUrl::parse("https://x.test/p?q=a%23b")
                .expect("valid")
                .as_str(),
            "https://x.test/p?q=a%23b"
        );
        assert_eq!(
            CanonicalUrl::parse("HTTPS://X.test/A?B=1")
                .expect("valid")
                .as_str(),
            "https://x.test/A?B=1"
        );

        // The canonical (fragment-free) form is what serializes.
        assert_eq!(
            serde_json::to_string(&one).expect("serialize"),
            "\"https://x.test/product\""
        );
    }

    #[test]
    fn url_identity_equivalence_classes() {
        use std::cmp::Ordering;
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let hash = |url: &CanonicalUrl| {
            let mut hasher = DefaultHasher::new();
            url.hash(&mut hasher);
            hasher.finish()
        };
        let same = |a: &str, b: &str| {
            let ua = CanonicalUrl::parse(a).expect(a);
            let ub = CanonicalUrl::parse(b).expect(b);
            assert_eq!(ua, ub, "{a} and {b} must be the same value");
            assert_eq!(ua.as_str(), ub.as_str(), "{a} vs {b}: one serialization");
            assert_eq!(ua.cmp(&ub), Ordering::Equal, "{a} vs {b}: one order");
            assert_eq!(hash(&ua), hash(&ub), "{a} vs {b}: one hash");
        };

        // Default ports (both schemes) and empty path vs `/`.
        same("https://x.test:443/p", "https://x.test/p");
        same("http://x.test:80/p", "http://x.test/p");
        same("https://x.test", "https://x.test/");
        // Host case and trailing dot (one dot is stripped by the shared
        // destination authority).
        same("https://X.TEST/p", "https://x.test/p");
        same("https://x.test./p", "https://x.test/p");
        // Canonical IPv4: alternate spellings collapse to dotted octets.
        same("https://127.000.000.001/p", "https://127.0.0.1/p");
        // IPv6 literals: full form == compressed canonical form.
        same("https://[0:0:0:0:0:0:0:1]/p", "https://[::1]/p");
        // IDN == punycode.
        same("https://ex\u{e4}mple.com/p", "https://xn--exmple-cua.com/p");

        // Distinct hosts/ports stay distinct.
        assert_ne!(
            CanonicalUrl::parse("https://x.test:8443/p").expect("valid"),
            CanonicalUrl::parse("https://x.test/p").expect("valid")
        );
        assert_ne!(
            CanonicalUrl::parse("https://x.test:443/p").expect("valid"),
            CanonicalUrl::parse("http://x.test/p").expect("valid")
        );
        // The canonical host accessor is the shared authority's text.
        assert_eq!(
            CanonicalUrl::parse("https://ex\u{e4}mple.com./p")
                .expect("valid")
                .host(),
            "xn--exmple-cua.com"
        );
        assert_eq!(
            CanonicalUrl::parse("https://[0:0:0:0:0:0:0:1]:8443/p")
                .expect("valid")
                .origin(),
            "https://[::1]:8443"
        );
    }

    /// Marketplace identity and egress must agree: the egress gate parses
    /// the SAME canonical pieces (scheme/host/port) into one
    /// [`faktor_security::destination::RequestTarget`], so a host that is
    /// one identity value can never be a different egress destination.
    #[test]
    fn marketplace_identity_and_egress_share_host_semantics() {
        use faktor_security::destination::{Decision, DestinationPolicy, RequestTarget};

        let url = CanonicalUrl::parse("https://Ex\u{e4}mple.com.:443/a").expect("valid");
        let explicit_port = url
            .origin()
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse::<u16>().ok());
        assert_eq!(explicit_port, None, "a default port is not explicit");
        let target =
            RequestTarget::from_parts(Some(url.scheme()), url.host(), explicit_port, false, None)
                .expect("canonical host is a valid target");
        assert_eq!(target.host, url.host());
        assert_eq!(target.host, "xn--exmple-cua.com");
        assert_eq!(target.port, Some(443), "egress resolves the scheme default");
        let policy =
            DestinationPolicy::parse_lines(["https://xn--exmple-cua.com:443"]).expect("policy");
        assert!(matches!(target.check_against(&policy), Decision::Allowed));

        // Every identity-equivalent spelling lands on the same target.
        for raw in [
            "HTTPS://EX\u{c4}MPLE.COM/a",
            "https://xn--exmple-cua.com./a",
            "https://ex\u{e4}mple.com:0443/a",
        ] {
            let other = CanonicalUrl::parse(raw).expect(raw);
            assert_eq!(other.host(), url.host(), "{raw}");
            let target =
                RequestTarget::from_parts(Some(other.scheme()), other.host(), None, false, None)
                    .expect("target");
            assert!(target.check_against(&policy).is_allowed(), "{raw}");
        }
    }

    #[test]
    fn url_rejects_credentials_and_hostile_shapes() {
        assert!(matches!(
            CanonicalUrl::parse("https://user:pass@evil.com/"),
            Err(UrlError::UserInfo)
        ));
        assert!(matches!(
            CanonicalUrl::parse("https://user@evil.com/"),
            Err(UrlError::UserInfo)
        ));
        assert!(matches!(
            CanonicalUrl::parse("javascript:alert(1)"),
            Err(UrlError::UnsupportedScheme { .. })
        ));
        assert!(matches!(
            CanonicalUrl::parse("file:///etc/passwd"),
            Err(UrlError::UnsupportedScheme { .. })
        ));
        assert!(matches!(
            CanonicalUrl::parse("detail.1688.com/offer/1.html"),
            Err(UrlError::MissingScheme)
        ));
        assert!(matches!(
            CanonicalUrl::parse("https://"),
            Err(UrlError::MissingHost)
        ));
        // A backslash is a path separator for special schemes (the URL
        // parser's semantics, shared with egress); it can never smuggle a
        // second authority or a userinfo.
        assert_eq!(
            CanonicalUrl::parse("https://evil.com\\@good.com/")
                .expect("url parser semantics")
                .as_str(),
            "https://evil.com/@good.com/"
        );
        assert!(matches!(
            CanonicalUrl::parse("https://good.com%00.evil/"),
            Err(UrlError::InvalidHost | UrlError::Malformed { .. })
        ));
        assert!(matches!(
            CanonicalUrl::parse("https://-bad.com/"),
            Err(UrlError::InvalidHost)
        ));
        assert!(matches!(
            CanonicalUrl::parse("https://bad-.com/"),
            Err(UrlError::InvalidHost)
        ));
        assert!(matches!(
            CanonicalUrl::parse("https://a..b/"),
            Err(UrlError::InvalidHost | UrlError::Malformed { .. })
        ));
        assert!(matches!(
            CanonicalUrl::parse("https://good.com:0/"),
            Err(UrlError::InvalidPort)
        ));
        assert!(matches!(
            CanonicalUrl::parse("https://good.com:65536/"),
            Err(UrlError::InvalidPort)
        ));
        assert!(matches!(
            CanonicalUrl::parse("https://good.com:80x/"),
            Err(UrlError::InvalidPort)
        ));
        assert!(matches!(
            CanonicalUrl::parse("https://good.com/\u{0}"),
            Err(UrlError::ControlChar { .. })
        ));
        let long = format!("https://good.com/{}", "a".repeat(MAX_URL_BYTES));
        assert!(matches!(
            CanonicalUrl::parse(&long),
            Err(UrlError::TooLong { .. })
        ));
        assert!(matches!(CanonicalUrl::parse("   "), Err(UrlError::Empty)));
    }

    #[test]
    fn text_serde_rejects_numbers_and_hostile_strings() {
        assert!(serde_json::from_str::<Text<16>>("42").is_err());
        assert!(serde_json::from_str::<Text<16>>("null").is_err());
        assert!(serde_json::from_str::<Text<16>>("\"\\u0000\"").is_err());
        assert_eq!(
            serde_json::from_str::<Text<16>>("\"ok\"")
                .expect("valid")
                .as_str(),
            "ok"
        );
    }

    #[test]
    fn url_and_id_serde_round_trip() {
        let url = CanonicalUrl::parse("https://www.lcsc.com/product/C123.html").expect("valid");
        let json = serde_json::to_string(&url).expect("serialize");
        assert_eq!(json, "\"https://www.lcsc.com/product/C123.html\"");
        assert_eq!(
            serde_json::from_str::<CanonicalUrl>(&json).expect("round trip"),
            url
        );

        let source = SourceId::new("DigiKey").expect("valid");
        assert_eq!(
            serde_json::to_string(&source).expect("serialize"),
            "\"digikey\""
        );
        assert_eq!(
            serde_json::from_str::<SourceId>("\"DIGIKEY\"").expect("round trip"),
            source
        );
        assert!(serde_json::from_str::<SourceId>("12").is_err());
        assert!(serde_json::from_str::<CanonicalUrl>("\"https://u:p@a.com/\"").is_err());
        assert!(serde_json::from_str::<CanonicalUrl>("\"https://a.com/\u{7f}\"").is_err());
    }
}
