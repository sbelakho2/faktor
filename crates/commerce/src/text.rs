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
//! - [`CanonicalUrl`] accepts only absolute `http`/`https` URLs with a
//!   plain host, no userinfo (credentials can never hide in a canonical
//!   URL), no fragment (the fragment is stripped during canonicalization: it
//!   is never sent in a request and never participates in identity), and a
//!   bounded length.

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
    /// Contains a non-ASCII character (IDN hosts must be punycode).
    #[error("url contains a non-ASCII character at byte {at}")]
    NonAscii { at: usize },
    /// Contains a control character.
    #[error("url contains a control character at byte {at}")]
    ControlChar { at: usize },
    /// Contains whitespace or a character that is never valid in a URL.
    #[error("url contains an invalid character at byte {at}")]
    InvalidChar { at: usize },
    /// No `scheme://` prefix.
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
    /// A host label is empty or contains a disallowed character.
    #[error("url host is invalid")]
    InvalidHost,
    /// A port is empty, non-numeric, zero or above 65535.
    #[error("url port is invalid")]
    InvalidPort,
}

/// An absolute, credential-free `http`/`https` URL.
///
/// This is a strict parser for hostile input, not a general URL library: it
/// accepts exactly what a marketplace product reference can be. The scheme
/// and host are normalized to lowercase; path/query keep their original
/// case.
///
/// The fragment is stripped during canonicalization — RFC 3986 fragments are
/// never sent in HTTP requests, so they must not participate in identity.
/// `Eq`/`Hash`/`Ord`, every identity key and every digest therefore see the
/// fragment-free canonical form `scheme://host[:port]/path?query` only:
/// `https://x.test/product#one` and `https://x.test/product#two` are the
/// same value. The raw input fragment is deliberately not retained; no
/// caller in the workspace needs it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CanonicalUrl(String);

impl CanonicalUrl {
    /// Parse and validate.
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
        for (at, ch) in trimmed.char_indices() {
            if !ch.is_ascii() {
                return Err(UrlError::NonAscii { at });
            }
            if ch.is_control() {
                return Err(UrlError::ControlChar { at });
            }
            if ch == ' ' || matches!(ch, '"' | '<' | '>' | '\\' | '^' | '`' | '{' | '|' | '}') {
                return Err(UrlError::InvalidChar { at });
            }
        }
        let (scheme, rest) = trimmed.split_once("://").ok_or(UrlError::MissingScheme)?;
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return Err(UrlError::UnsupportedScheme { scheme });
        }
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        let tail = &rest[authority_end..];
        if authority.is_empty() {
            return Err(UrlError::MissingHost);
        }
        if authority.contains('@') {
            return Err(UrlError::UserInfo);
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        };
        validate_host(host)?;
        if let Some(port) = port {
            validate_port(port)?;
        }
        let host = host.to_ascii_lowercase();
        // The fragment starts at the first `#` and ends the URL; it is never
        // sent and never part of identity, so it is dropped here. A `?` or
        // `/` inside the fragment is fragment data, not path or query.
        let path_query = tail.split_once('#').map_or(tail, |(before, _)| before);
        let normalized = match port {
            Some(port) => format!("{scheme}://{host}:{port}{path_query}"),
            None => format!("{scheme}://{host}{path_query}"),
        };
        Ok(Self(normalized))
    }

    /// The normalized canonical URL:
    /// `scheme://host[:port]/path?query`. The fragment is not part of the
    /// canonical form (it is stripped at parse time).
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The scheme (always `http` or `https`).
    pub fn scheme(&self) -> &str {
        self.0.split_once("://").map(|(s, _)| s).unwrap_or("")
    }

    /// The lowercased host (without port).
    pub fn host(&self) -> &str {
        let rest = self.0.split_once("://").map(|(_, r)| r).unwrap_or("");
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        authority
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(authority)
    }

    /// The origin (`scheme://host[:port]`), the part that never varies with
    /// path/query.
    pub fn origin(&self) -> &str {
        let rest = self.0.split_once("://").map(|(_, r)| r).unwrap_or("");
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        &self.0[..self.0.len() - rest.len() + authority_end]
    }
}

fn validate_host(host: &str) -> Result<(), UrlError> {
    if host.is_empty() || host.len() > 253 {
        return Err(UrlError::InvalidHost);
    }
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(UrlError::InvalidHost);
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(UrlError::InvalidHost);
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(UrlError::InvalidHost);
        }
    }
    Ok(())
}

fn validate_port(port: &str) -> Result<(), UrlError> {
    if port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()) {
        return Err(UrlError::InvalidPort);
    }
    let value: u32 = port.parse().map_err(|_| UrlError::InvalidPort)?;
    if value == 0 || value > 65_535 {
        return Err(UrlError::InvalidPort);
    }
    Ok(())
}

impl fmt::Display for CanonicalUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for CanonicalUrl {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
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
        assert_eq!(
            CanonicalUrl::parse("https://good.com:0443/")
                .expect("valid")
                .origin(),
            "https://good.com:0443"
        );
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
        assert_eq!(
            CanonicalUrl::parse("https://x.test#frag")
                .expect("valid")
                .as_str(),
            "https://x.test"
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
    fn url_rejects_credentials_and_hostile_shapes() {
        assert!(matches!(
            CanonicalUrl::parse("https://user:pass@evil.com/"),
            Err(UrlError::UserInfo)
        ));
        assert!(matches!(
            CanonicalUrl::parse("javascript:alert(1)"),
            Err(UrlError::MissingScheme)
        ));
        assert!(matches!(
            CanonicalUrl::parse("file:///etc/passwd"),
            Err(UrlError::UnsupportedScheme { .. })
        ));
        assert!(matches!(
            CanonicalUrl::parse("https://"),
            Err(UrlError::MissingHost)
        ));
        assert!(matches!(
            CanonicalUrl::parse("https://evil.com\\@good.com/"),
            Err(UrlError::InvalidChar { .. })
        ));
        assert!(matches!(
            CanonicalUrl::parse("https://good.com%00.evil/"),
            Err(UrlError::InvalidHost)
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
            Err(UrlError::InvalidHost)
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
            CanonicalUrl::parse("https://good.com:/"),
            Err(UrlError::InvalidPort)
        ));
        assert!(matches!(
            CanonicalUrl::parse("https://good.com:80x/"),
            Err(UrlError::InvalidPort)
        ));
        assert!(matches!(
            CanonicalUrl::parse("https://ex\u{e4}mple.com/"),
            Err(UrlError::NonAscii { .. })
        ));
        assert!(matches!(
            CanonicalUrl::parse("https://good.com/a b"),
            Err(UrlError::InvalidChar { .. })
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
