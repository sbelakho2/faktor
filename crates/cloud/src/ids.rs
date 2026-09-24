//! Typed control-plane identities and secrets.
//!
//! Every id is a distinct type with a strict shape (bounded printable ASCII
//! without whitespace); deserialization re-validates, so a hostile DTO can
//! never smuggle an invalid identity into the domain. Secrets (session
//! tokens, invitation tokens, service-account tokens) are exposed exactly
//! once at issuance and stored ONLY as SHA-256 hashes.

use std::fmt;

use faktor_security::secret::SecretValue;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

use crate::error::ControlPlaneError;

/// Bound on any control-plane id.
pub const MAX_ID_BYTES: usize = 128;
/// Bound on one bearer token (session / invitation / service account).
pub const MAX_TOKEN_BYTES: usize = 512;

fn validate_id(kind: &str, value: &str) -> Result<(), ControlPlaneError> {
    if value.is_empty() || value.len() > MAX_ID_BYTES {
        return Err(ControlPlaneError::Malformed(format!(
            "{kind} id must be 1..={MAX_ID_BYTES} bytes"
        )));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\')
    {
        return Err(ControlPlaneError::Malformed(format!(
            "{kind} id must be printable ASCII without whitespace, quotes or backslashes"
        )));
    }
    Ok(())
}

macro_rules! string_id {
    ($name:ident, $kind:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn try_new(raw: impl Into<String>) -> Result<Self, ControlPlaneError> {
                let raw = raw.into();
                validate_id($kind, &raw)?;
                Ok(Self(raw))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(d)?;
                Self::try_new(raw).map_err(D::Error::custom)
            }
        }
    };
}

string_id!(OrganizationId, "organization");
string_id!(UserId, "user");
string_id!(MembershipId, "membership");
string_id!(InvitationId, "invitation");
string_id!(AuthSessionId, "auth session");
string_id!(ExternalIdentityId, "external identity");
string_id!(ServiceAccountId, "service account");
string_id!(ApprovalId, "approval");
// Wave 3 commercial metering identities (billing domain).
string_id!(BillingAccountId, "billing account");
string_id!(SubscriptionId, "subscription");
string_id!(UsageEventId, "usage event");
string_id!(CreditEntryId, "credit entry");
string_id!(InFlightTxnId, "in-flight transaction");
// Enterprise plane identities (retention/deletion domain).
string_id!(ArtifactId, "artifact");
string_id!(DeletionJobId, "deletion job");

/// The sha256 hex of one token (the ONLY form a token is stored in).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TokenHash(String);

impl TokenHash {
    pub fn try_new(raw: impl Into<String>) -> Result<Self, ControlPlaneError> {
        let raw = raw.into();
        if raw.len() != 64 || !raw.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ControlPlaneError::Malformed(
                "token hash must be 64 lowercase hex characters".into(),
            ));
        }
        Ok(Self(raw))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Hash one presented token (the lookup key of every token table).
    pub fn of(token: &str) -> Self {
        Self(crate::service::sha256_hex(token.as_bytes()))
    }
}

impl fmt::Display for TokenHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One plaintext secret token, exposed exactly once at issuance. The value
/// is a [`SecretValue`]: zeroized on drop, redacted `Debug`, no `Display` and
/// no serde; it leaves only through [`SecretToken::expose`].
#[derive(Clone, PartialEq, Eq)]
pub struct SecretToken(SecretValue);

impl SecretToken {
    pub fn try_new(raw: impl Into<String>) -> Result<Self, ControlPlaneError> {
        let raw = raw.into();
        if raw.is_empty() || raw.len() > MAX_TOKEN_BYTES || raw.contains(char::is_whitespace) {
            return Err(ControlPlaneError::Malformed(
                "token must be 1..=512 non-whitespace bytes".into(),
            ));
        }
        Ok(Self(SecretValue::new(raw)))
    }

    /// The secret value (call sites must never log it).
    pub fn expose(&self) -> &str {
        self.0.expose()
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretToken(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_bounded_and_revalidated_on_deserialize() {
        assert!(OrganizationId::try_new("").is_err());
        assert!(OrganizationId::try_new("has space").is_err());
        assert!(OrganizationId::try_new("a".repeat(MAX_ID_BYTES + 1)).is_err());
        assert!(OrganizationId::try_new("org_01h").is_ok());
        assert!(serde_json::from_str::<UserId>("\"bad id\"").is_err());
        assert_eq!(
            serde_json::from_str::<UserId>("\"usr_1\"")
                .unwrap()
                .as_str(),
            "usr_1"
        );
    }

    #[test]
    fn token_hash_shape_and_value_are_exact() {
        assert!(TokenHash::try_new("short").is_err());
        assert!(
            TokenHash::try_new("Z".repeat(64)).is_err(),
            "uppercase hex is not canonical"
        );
        let hash = TokenHash::of("secret");
        assert_eq!(hash.as_str().len(), 64);
        assert_eq!(
            hash.as_str(),
            "2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b"
        );
    }

    #[test]
    fn secret_tokens_are_redacted_and_bounded() {
        let token = SecretToken::try_new("abc123").unwrap();
        assert!(!format!("{token:?}").contains("abc123"));
        assert_eq!(token.expose(), "abc123");
        assert!(SecretToken::try_new("").is_err());
        assert!(SecretToken::try_new("a b").is_err());
        assert!(SecretToken::try_new("x".repeat(MAX_TOKEN_BYTES + 1)).is_err());
    }

    /// The compile-time negative proof: `SecretToken` has NO `Display` and
    /// NO `Serialize`. The probe below only compiles while neither impl
    /// exists (the `AmbiguousIfImpl` shape: an extra candidate impl makes the
    /// `_` placeholder un-inferable).
    macro_rules! assert_no_display_no_serialize {
        ($ty:ty) => {{
            trait AmbiguousIfImpl<A> {
                fn probe() {}
            }
            impl<T: ?Sized> AmbiguousIfImpl<()> for T {}
            impl<T: ?Sized + std::fmt::Display> AmbiguousIfImpl<u8> for T {}
            impl<T: ?Sized + ::serde::Serialize> AmbiguousIfImpl<u16> for T {}
            let _ = <$ty as AmbiguousIfImpl<_>>::probe;
        }};
    }

    #[test]
    fn planted_secret_never_leaks_through_debug_display_serde_or_panic() {
        assert_no_display_no_serialize!(SecretToken);
        const PLANTED: &str = "PLANTED-TOKEN-do-not-leak-0123456789abcdef";
        let token = SecretToken::try_new(PLANTED).unwrap();
        // Debug through every nesting level.
        for rendered in [
            format!("{token:?}"),
            format!("{:?}", Some(token.clone())),
            format!("{:?}", vec![token.clone()]),
            format!("{:?}", (token.clone(), 1u8)),
        ] {
            assert!(!rendered.contains(PLANTED), "leaked via {rendered}");
        }
        assert_eq!(format!("{token:?}"), "SecretToken(<redacted>)");
        // Panic formatting must stay redacted.
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("session mint {token:?}")
        }))
        .expect_err("the closure must panic");
        let message = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default();
        assert!(
            !message.contains(PLANTED),
            "panic payload leaked: {message}"
        );
        // A validation refusal names only the SHAPE, never the planted bytes.
        let err = SecretToken::try_new(format!("{PLANTED} with space")).unwrap_err();
        let rendered = format!("{err} {err:?}");
        assert!(!rendered.contains(PLANTED), "error leaked: {rendered}");
    }
}
