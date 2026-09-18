//! The SSO/OIDC adapter seam.
//!
//! [`OidcAdapter`] is the whole contract the control plane needs from an
//! identity provider: discovery, authorization-code exchange, ID-token
//! verification with JWKS rotation, and claim -> membership mapping. Real
//! adapters (network + provider JWKS) implement it outside this crate; the
//! deterministic [`FakeOidcAdapter`] here implements the SAME contract with
//! HMAC-SHA256-signed tokens and an in-memory key set, so every security
//! property (signature, expiry, issuer, audience, nonce, key rotation) is
//! exercised without a network.
//!
//! Inbound SCIM provisioning is deliberately NOT part of this contract: no
//! SCIM push/deprovision surface exists, none is declared here, and nothing
//! in this tree calls one. An identity provider's directory sync is an
//! adapter outside this seam; this crate only consumes OIDC claims.

use std::collections::BTreeMap;
use std::sync::Mutex;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::rbac::Role;

/// The discovery document an adapter resolves for an issuer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcDiscovery {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
    pub supported_algorithms: Vec<String>,
}

/// One authorization-code exchange request (PKCE).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeExchangeRequest {
    pub code: String,
    pub redirect_uri: String,
    pub code_verifier: String,
}

/// The token endpoint response (the ID token is the verification input).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcTokenSet {
    pub access_token: String,
    pub id_token: String,
    pub token_type: String,
    pub expires_in_s: i64,
}

/// What a verifier must check besides the signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdTokenExpectations {
    pub issuer: String,
    pub audience: String,
    #[serde(default)]
    pub nonce: Option<String>,
    pub now_ms: i64,
    #[serde(default)]
    pub clock_skew_ms: i64,
}

/// The verified ID-token claims (bounded; never the raw token).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcClaims {
    pub issuer: String,
    pub subject: String,
    pub audience: Vec<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub email_verified: bool,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
    #[serde(default)]
    pub nonce: Option<String>,
    /// Group memberships (the membership-mapping input).
    #[serde(default)]
    pub groups: Vec<String>,
}

/// How claims map to one organization membership.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimMapping {
    pub email_claim: String,
    pub groups_claim: String,
    pub group_roles: BTreeMap<String, Role>,
    pub default_role: Role,
    pub require_verified_email: bool,
}

/// The membership decision produced from verified claims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcMembership {
    pub subject: String,
    pub email: String,
    pub role: Role,
    /// The groups that produced the role (bounded by the claim size).
    pub matched_groups: Vec<String>,
}

/// Typed OIDC refusal. Every variant is a distinct cause; none leaks token
/// material.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OidcError {
    #[error("oidc discovery unavailable: {0}")]
    DiscoveryUnavailable(String),
    #[error("oidc code exchange refused: {0}")]
    CodeExchangeRefused(String),
    #[error("oidc id token signature is invalid")]
    BadSignature,
    #[error("oidc id token header is malformed: {0}")]
    Malformed(String),
    #[error("oidc id token signed by an unknown key {0:?}")]
    UnknownKey(String),
    #[error("oidc id token is expired")]
    Expired,
    #[error("oidc id token is not yet valid")]
    NotYetValid,
    #[error("oidc id token issuer {actual:?} does not match {expected:?}")]
    WrongIssuer { expected: String, actual: String },
    #[error("oidc id token audience does not include {expected:?}")]
    WrongAudience { expected: String },
    /// The header `alg` is not in the intersection of the discovery set, the
    /// signing key's own constraints and the configured allowed algorithms
    /// (each set is named so the refusal is diagnosable without guessing).
    #[error(
        "oidc id token alg {alg:?} is refused: discovery advertises {discovery:?}, \
         the signing key permits {jwk:?}, the configuration allows {configured:?}"
    )]
    AlgorithmRefused {
        alg: String,
        discovery: Vec<String>,
        jwk: Vec<String>,
        configured: Vec<String>,
    },
    /// The `azp` (authorized party) claim is missing where required (a
    /// multi-valued `aud`) or names a party other than the expected client.
    #[error("oidc id token azp {actual:?} does not match the authorized party {expected:?}")]
    WrongAzp {
        expected: String,
        actual: Option<String>,
    },
    /// A numeric time claim (`exp`/`iat`) cannot be represented in
    /// milliseconds without overflowing — impossible, refused typed instead
    /// of wrapping or panicking.
    #[error("oidc id token claim {claim:?} value {value} is out of the representable range")]
    TimestampOutOfRange { claim: String, value: i64 },
    #[error("oidc id token nonce does not match")]
    NonceMismatch,
    #[error("oidc login state is unknown, expired or already used")]
    StateInvalid,
    #[error("oidc login redirect_uri does not match the one the login started with")]
    RedirectMismatch,
    #[error("oidc membership refused: {0}")]
    MembershipRefused(String),
}

/// The OIDC adapter seam.
pub trait OidcAdapter: Send + Sync {
    /// Resolve the issuer's discovery document.
    fn discovery(&self, issuer: &str) -> Result<OidcDiscovery, OidcError>;
    /// Exchange one authorization code (PKCE) for a token set.
    fn exchange_code(&self, request: &CodeExchangeRequest) -> Result<OidcTokenSet, OidcError>;
    /// Verify one ID token: signature (JWKS, rotation-aware), expiry with
    /// skew, issuer, audience and nonce.
    fn verify_id_token(
        &self,
        id_token: &str,
        expected: &IdTokenExpectations,
    ) -> Result<OidcClaims, OidcError>;
    /// Map verified claims to a membership role.
    fn map_membership(
        &self,
        claims: &OidcClaims,
        mapping: &ClaimMapping,
    ) -> Result<OidcMembership, OidcError>;
}

/// One JWKS key view (metadata only; never key material).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JwkView {
    pub kid: String,
    pub alg: String,
    pub kty: String,
}

/// A deterministic OIDC adapter for tests and local parity: HMAC-SHA256
/// signed tokens, an in-memory authorization-code map and a rotatable key
/// set. It implements the full [`OidcAdapter`] contract; nothing about it
/// touches the network.
pub struct FakeOidcAdapter {
    issuer: String,
    client_id: String,
    keys: Mutex<Vec<FakeKey>>,
    active_kid: Mutex<String>,
    codes: Mutex<BTreeMap<String, OidcClaims>>,
}

#[derive(Clone)]
struct FakeKey {
    kid: String,
    secret: Vec<u8>,
    retired: bool,
}

impl FakeOidcAdapter {
    /// Build a fake adapter. `secret` is the initial signing key.
    pub fn new(issuer: &str, client_id: &str, secret: &[u8]) -> Self {
        Self {
            issuer: issuer.to_string(),
            client_id: client_id.to_string(),
            keys: Mutex::new(vec![FakeKey {
                kid: "kid-1".into(),
                secret: secret.to_vec(),
                retired: false,
            }]),
            active_kid: Mutex::new("kid-1".into()),
            codes: Mutex::new(BTreeMap::new()),
        }
    }

    /// Rotate the JWKS: install a new active key and retire every previous
    /// key (a rotated-out kid no longer verifies).
    pub fn rotate_jwks(&self, new_kid: &str, new_secret: &[u8]) {
        let mut keys = self.keys.lock().unwrap_or_else(|p| p.into_inner());
        for key in keys.iter_mut() {
            key.retired = true;
        }
        keys.push(FakeKey {
            kid: new_kid.to_string(),
            secret: new_secret.to_vec(),
            retired: false,
        });
        *self.active_kid.lock().unwrap_or_else(|p| p.into_inner()) = new_kid.to_string();
    }

    /// Retire one key without installing a new one.
    pub fn retire_kid(&self, kid: &str) -> bool {
        let mut keys = self.keys.lock().unwrap_or_else(|p| p.into_inner());
        let mut found = false;
        for key in keys.iter_mut() {
            if key.kid == kid {
                key.retired = true;
                found = true;
            }
        }
        found
    }

    /// The current JWKS (metadata only).
    pub fn jwks(&self) -> Vec<JwkView> {
        self.keys
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|key| !key.retired)
            .map(|key| JwkView {
                kid: key.kid.clone(),
                alg: "HS256".into(),
                kty: "oct".into(),
            })
            .collect()
    }

    /// Register an authorization code with its claims (the fake's token
    /// endpoint lookup table).
    pub fn issue_code(&self, code: &str, claims: OidcClaims) {
        self.codes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(code.to_string(), claims);
    }

    /// Sign one claims set with the ACTIVE key, bypassing `issue_code` (used
    /// to forge tokens in adversarial tests).
    pub fn sign_claims(&self, claims: &OidcClaims) -> String {
        let kid = self
            .active_kid
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        self.sign_with_kid(&kid, claims)
    }

    /// Sign one claims set under an arbitrary kid (forgery seam: a test can
    /// sign with an old secret under the new kid).
    pub fn sign_with_kid(&self, kid: &str, claims: &OidcClaims) -> String {
        let secret = self
            .keys
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|key| key.kid == kid)
            .map(|key| key.secret.clone())
            .unwrap_or_else(|| b"unknown-kid".to_vec());
        let header = serde_json::json!({"alg": "HS256", "kid": kid, "typ": "JWT"});
        let payload = serde_json::json!({
            "iss": claims.issuer,
            "sub": claims.subject,
            "aud": claims.audience,
            "email": claims.email,
            "email_verified": claims.email_verified,
            "iat": claims.issued_at_ms / 1000,
            "exp": claims.expires_at_ms / 1000,
            "nonce": claims.nonce,
            "groups": claims.groups,
        });
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header = engine.encode(serde_json::to_vec(&header).expect("header encodes"));
        let payload = engine.encode(serde_json::to_vec(&payload).expect("payload encodes"));
        let signing_input = format!("{header}.{payload}");
        let signature = hmac_sha256(&secret, signing_input.as_bytes());
        format!("{signing_input}.{}", engine.encode(signature))
    }
}

pub(crate) fn hmac_sha256(secret: &[u8], message: &[u8]) -> Vec<u8> {
    const BLOCK: usize = 64;
    let mut key = if secret.len() > BLOCK {
        Sha256::digest(secret).to_vec()
    } else {
        secret.to_vec()
    };
    key.resize(BLOCK, 0);
    let mut inner = Vec::with_capacity(BLOCK + message.len());
    let mut outer = Vec::with_capacity(BLOCK + 32);
    for byte in &key {
        inner.push(byte ^ 0x36);
        outer.push(byte ^ 0x5c);
    }
    inner.extend_from_slice(message);
    let inner_digest = Sha256::digest(&inner);
    outer.extend_from_slice(&inner_digest);
    Sha256::digest(&outer).to_vec()
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

impl OidcAdapter for FakeOidcAdapter {
    fn discovery(&self, issuer: &str) -> Result<OidcDiscovery, OidcError> {
        if issuer != self.issuer {
            return Err(OidcError::DiscoveryUnavailable(format!(
                "issuer {issuer:?} is not served by this adapter"
            )));
        }
        Ok(OidcDiscovery {
            issuer: self.issuer.clone(),
            authorization_endpoint: format!("{}/authorize", self.issuer),
            token_endpoint: format!("{}/token", self.issuer),
            jwks_uri: format!("{}/jwks", self.issuer),
            supported_algorithms: vec!["HS256".into()],
        })
    }

    fn exchange_code(&self, request: &CodeExchangeRequest) -> Result<OidcTokenSet, OidcError> {
        if request.code.is_empty()
            || request.redirect_uri.is_empty()
            || request.code_verifier.is_empty()
        {
            return Err(OidcError::CodeExchangeRefused(
                "code, redirect_uri and code_verifier are required".into(),
            ));
        }
        let claims = self
            .codes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&request.code)
            .cloned()
            .ok_or_else(|| OidcError::CodeExchangeRefused("unknown or already-used code".into()))?;
        if !claims.audience.iter().any(|aud| aud == &self.client_id) {
            return Err(OidcError::CodeExchangeRefused(
                "the authorization code was issued for a different client".into(),
            ));
        }
        Ok(OidcTokenSet {
            access_token: base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(hmac_sha256(b"access", request.code.as_bytes())),
            id_token: self.sign_claims(&claims),
            token_type: "Bearer".into(),
            expires_in_s: 3600,
        })
    }

    fn verify_id_token(
        &self,
        id_token: &str,
        expected: &IdTokenExpectations,
    ) -> Result<OidcClaims, OidcError> {
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let mut segments = id_token.split('.');
        let (Some(header_b64), Some(payload_b64), Some(signature_b64), None) = (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
        ) else {
            return Err(OidcError::Malformed(
                "a JWT must have exactly three dot-separated segments".into(),
            ));
        };
        let header: serde_json::Value = serde_json::from_slice(
            &engine
                .decode(header_b64)
                .map_err(|e| OidcError::Malformed(format!("header base64: {e}")))?,
        )
        .map_err(|e| OidcError::Malformed(format!("header json: {e}")))?;
        let kid = header
            .get("kid")
            .and_then(|value| value.as_str())
            .ok_or_else(|| OidcError::Malformed("header carries no kid".into()))?;
        let alg = header
            .get("alg")
            .and_then(|value| value.as_str())
            .ok_or_else(|| OidcError::Malformed("header carries no alg".into()))?;
        if alg != "HS256" {
            return Err(OidcError::Malformed(format!(
                "unsupported JWT alg {alg:?} (this adapter verifies HS256)"
            )));
        }
        let key = self
            .keys
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|key| key.kid == kid && !key.retired)
            .cloned()
            .ok_or_else(|| OidcError::UnknownKey(kid.to_string()))?;
        let signature = engine
            .decode(signature_b64)
            .map_err(|e| OidcError::Malformed(format!("signature base64: {e}")))?;
        let signing_input = format!("{header_b64}.{payload_b64}");
        let expected_signature = hmac_sha256(&key.secret, signing_input.as_bytes());
        if !constant_time_eq(&signature, &expected_signature) {
            return Err(OidcError::BadSignature);
        }
        let payload: serde_json::Value = serde_json::from_slice(
            &engine
                .decode(payload_b64)
                .map_err(|e| OidcError::Malformed(format!("payload base64: {e}")))?,
        )
        .map_err(|e| OidcError::Malformed(format!("payload json: {e}")))?;
        let text = |field: &str| -> Result<String, OidcError> {
            payload
                .get(field)
                .and_then(|value| value.as_str())
                .map(str::to_string)
                .ok_or_else(|| {
                    OidcError::Malformed(format!("claim {field:?} is missing or not text"))
                })
        };
        let number = |field: &str| -> Result<i64, OidcError> {
            payload
                .get(field)
                .and_then(|value| value.as_i64())
                .ok_or_else(|| {
                    OidcError::Malformed(format!("claim {field:?} is missing or not a number"))
                })
        };
        let issuer = text("iss")?;
        if issuer != expected.issuer {
            return Err(OidcError::WrongIssuer {
                expected: expected.issuer.clone(),
                actual: issuer,
            });
        }
        let audience = match payload.get("aud") {
            Some(serde_json::Value::String(single)) => vec![single.clone()],
            Some(serde_json::Value::Array(list)) => list
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect(),
            _ => {
                return Err(OidcError::Malformed(
                    "claim \"aud\" is missing or malformed".into(),
                ));
            }
        };
        if !audience.iter().any(|aud| aud == &expected.audience) {
            return Err(OidcError::WrongAudience {
                expected: expected.audience.clone(),
            });
        }
        let expires_at_ms = number("exp")?.saturating_mul(1000);
        let skew = expected.clock_skew_ms.max(0);
        if expires_at_ms + skew < expected.now_ms {
            return Err(OidcError::Expired);
        }
        let issued_at_ms = number("iat")?.saturating_mul(1000);
        if issued_at_ms - skew > expected.now_ms {
            return Err(OidcError::NotYetValid);
        }
        let nonce = payload
            .get("nonce")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        if let Some(expected_nonce) = &expected.nonce {
            if nonce.as_deref() != Some(expected_nonce.as_str()) {
                return Err(OidcError::NonceMismatch);
            }
        }
        Ok(OidcClaims {
            issuer,
            subject: text("sub")?,
            audience,
            email: payload
                .get("email")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            email_verified: payload
                .get("email_verified")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
            issued_at_ms,
            expires_at_ms,
            nonce,
            groups: payload
                .get("groups")
                .and_then(|value| value.as_array())
                .map(|list| {
                    list.iter()
                        .filter_map(|value| value.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    fn map_membership(
        &self,
        claims: &OidcClaims,
        mapping: &ClaimMapping,
    ) -> Result<OidcMembership, OidcError> {
        map_membership_claims(claims, mapping)
    }
}

/// The ONE claim -> membership mapping shared by every adapter (local fake
/// and the network adapter): verified email when demanded, highest matched
/// group wins, the configured default role otherwise. Pure.
pub(crate) fn map_membership_claims(
    claims: &OidcClaims,
    mapping: &ClaimMapping,
) -> Result<OidcMembership, OidcError> {
    if mapping.require_verified_email && !claims.email_verified {
        return Err(OidcError::MembershipRefused(
            "the id token does not carry a verified email".into(),
        ));
    }
    let email = claims.email.clone().ok_or_else(|| {
        OidcError::MembershipRefused("the id token carries no email claim".into())
    })?;
    let mut matched_groups: Vec<String> = Vec::new();
    let mut role: Option<Role> = None;
    for group in &claims.groups {
        if let Some(mapped) = mapping.group_roles.get(group) {
            matched_groups.push(group.clone());
            role = Some(match role {
                Some(current) if current.rank() >= mapped.rank() => current,
                _ => *mapped,
            });
        }
    }
    Ok(OidcMembership {
        subject: claims.subject.clone(),
        email,
        role: role.unwrap_or(mapping.default_role),
        matched_groups,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ISSUER: &str = "https://idp.example";
    const CLIENT: &str = "faktor-local";

    fn claims(now_ms: i64) -> OidcClaims {
        OidcClaims {
            issuer: ISSUER.into(),
            subject: "sub-1".into(),
            audience: vec![CLIENT.into()],
            email: Some("user@example.test".into()),
            email_verified: true,
            issued_at_ms: now_ms - 1000,
            expires_at_ms: now_ms + 60_000,
            nonce: Some("nonce-1".into()),
            groups: vec!["faktor-admins".into(), "faktor-viewers".into()],
        }
    }

    fn expectations(now_ms: i64) -> IdTokenExpectations {
        IdTokenExpectations {
            issuer: ISSUER.into(),
            audience: CLIENT.into(),
            nonce: Some("nonce-1".into()),
            now_ms,
            clock_skew_ms: 0,
        }
    }

    #[test]
    fn fake_happy_path_discovery_exchange_verify_and_map() {
        let now = 1_700_000_000_000i64;
        let adapter = FakeOidcAdapter::new(ISSUER, CLIENT, b"secret-one");
        let discovery = adapter.discovery(ISSUER).unwrap();
        assert_eq!(discovery.issuer, ISSUER);
        assert!(adapter.discovery("https://other.example").is_err());

        adapter.issue_code("code-1", claims(now));
        let tokens = adapter
            .exchange_code(&CodeExchangeRequest {
                code: "code-1".into(),
                redirect_uri: "https://app.example/cb".into(),
                code_verifier: "verifier".into(),
            })
            .unwrap();
        let verified = adapter
            .verify_id_token(&tokens.id_token, &expectations(now))
            .unwrap();
        assert_eq!(verified.subject, "sub-1");
        assert!(verified.email_verified);

        let mapping = ClaimMapping {
            email_claim: "email".into(),
            groups_claim: "groups".into(),
            group_roles: BTreeMap::from([
                ("faktor-admins".to_string(), Role::Admin),
                ("faktor-viewers".to_string(), Role::Viewer),
            ]),
            default_role: Role::Viewer,
            require_verified_email: true,
        };
        let membership = adapter.map_membership(&verified, &mapping).unwrap();
        assert_eq!(membership.role, Role::Admin, "highest matched group wins");
        assert_eq!(membership.email, "user@example.test");
        assert_eq!(membership.matched_groups.len(), 2);

        // An unknown code never exchanges.
        assert!(adapter
            .exchange_code(&CodeExchangeRequest {
                code: "code-unknown".into(),
                redirect_uri: "https://app.example/cb".into(),
                code_verifier: "verifier".into(),
            })
            .is_err());
    }

    #[test]
    fn jwks_rotation_retires_the_old_kid_and_bad_signatures_are_refused() {
        let now = 1_700_000_000_000i64;
        let adapter = FakeOidcAdapter::new(ISSUER, CLIENT, b"secret-one");
        let old_token = adapter.sign_claims(&claims(now));
        assert!(adapter
            .verify_id_token(&old_token, &expectations(now))
            .is_ok());

        adapter.rotate_jwks("kid-2", b"secret-two");
        assert_eq!(adapter.jwks().len(), 1);
        assert_eq!(adapter.jwks()[0].kid, "kid-2");
        assert_eq!(
            adapter
                .verify_id_token(&old_token, &expectations(now))
                .unwrap_err(),
            OidcError::UnknownKey("kid-1".into()),
            "a rotated-out key is refused"
        );

        // A token signed with the new key verifies.
        let new_token = adapter.sign_claims(&claims(now));
        assert!(adapter
            .verify_id_token(&new_token, &expectations(now))
            .is_ok());

        // A forged token: old secret under the NEW kid.
        let forged = {
            let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(br#"{"alg":"HS256","kid":"kid-2","typ":"JWT"}"#);
            let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(br#"{"iss":"https://idp.example","sub":"sub-1","aud":"faktor-local","exp":99999999999,"iat":1}"#);
            let input = format!("{header}.{payload}");
            let signature = hmac_sha256(b"secret-one", input.as_bytes());
            format!(
                "{input}.{}",
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature)
            )
        };
        assert_eq!(
            adapter
                .verify_id_token(&forged, &expectations(now))
                .unwrap_err(),
            OidcError::BadSignature
        );

        // Tampering with the payload invalidates the signature too.
        let mut tampered = new_token.clone();
        let last = tampered.pop().unwrap();
        tampered.push(if last == 'A' { 'B' } else { 'A' });
        assert!(adapter
            .verify_id_token(&tampered, &expectations(now))
            .is_err());
    }

    #[test]
    fn expired_wrong_issuer_audience_and_nonce_are_refused() {
        let now = 1_700_000_000_000i64;
        let adapter = FakeOidcAdapter::new(ISSUER, CLIENT, b"secret-one");
        let mut expired = claims(now);
        expired.expires_at_ms = now - 1;
        let token = adapter.sign_claims(&expired);
        assert_eq!(
            adapter
                .verify_id_token(&token, &expectations(now))
                .unwrap_err(),
            OidcError::Expired
        );

        let token = adapter.sign_claims(&claims(now));
        let mut wrong_issuer = expectations(now);
        wrong_issuer.issuer = "https://other.example".into();
        assert!(matches!(
            adapter.verify_id_token(&token, &wrong_issuer).unwrap_err(),
            OidcError::WrongIssuer { .. }
        ));
        let mut wrong_audience = expectations(now);
        wrong_audience.audience = "another-client".into();
        assert!(matches!(
            adapter
                .verify_id_token(&token, &wrong_audience)
                .unwrap_err(),
            OidcError::WrongAudience { .. }
        ));
        let mut wrong_nonce = expectations(now);
        wrong_nonce.nonce = Some("nonce-other".into());
        assert_eq!(
            adapter.verify_id_token(&token, &wrong_nonce).unwrap_err(),
            OidcError::NonceMismatch
        );
        // Clock skew: a token 30 s from expiry is fine with a 60 s skew.
        let mut near_expiry = claims(now);
        near_expiry.expires_at_ms = now + 30_000;
        near_expiry.nonce = Some("nonce-1".into());
        let token = adapter.sign_claims(&near_expiry);
        let mut skewed = expectations(now + 40_000);
        skewed.clock_skew_ms = 60_000;
        assert!(adapter.verify_id_token(&token, &skewed).is_ok());
    }

    #[test]
    fn membership_refuses_unverified_email_and_defaults_role() {
        let adapter = FakeOidcAdapter::new(ISSUER, CLIENT, b"secret-one");
        let mut unverified = claims(1_700_000_000_000);
        unverified.email_verified = false;
        let mapping = ClaimMapping {
            email_claim: "email".into(),
            groups_claim: "groups".into(),
            group_roles: BTreeMap::new(),
            default_role: Role::Member,
            require_verified_email: true,
        };
        assert!(matches!(
            adapter.map_membership(&unverified, &mapping).unwrap_err(),
            OidcError::MembershipRefused(_)
        ));
        let mut no_email = claims(1_700_000_000_000);
        no_email.email = None;
        let relaxed = ClaimMapping {
            require_verified_email: false,
            ..mapping.clone()
        };
        assert!(matches!(
            adapter.map_membership(&no_email, &relaxed).unwrap_err(),
            OidcError::MembershipRefused(_)
        ));
        let membership = adapter
            .map_membership(&claims(1_700_000_000_000), &relaxed)
            .unwrap();
        assert_eq!(membership.role, Role::Member, "no group match = default");
    }
}
