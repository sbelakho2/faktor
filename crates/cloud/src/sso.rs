//! The SSO login flow on top of the asynchronous OIDC adapter: the
//! authorization-code + PKCE start, the single-use state/nonce exchange and
//! the control-plane session mint.
//!
//! Invariants (fail closed):
//!
//! - `start` mints a cryptographically random `state`, `nonce` and PKCE
//!   verifier; the pending login is bounded in count and TTL. The verifier
//!   and nonce never leave this process (only the S256 challenge and the
//!   state ride the authorization URL);
//! - `callback` is SINGLE USE: the state is removed before the exchange, so
//!   a replayed callback can never mint a second session. The redirect URI
//!   must equal the one the login started with;
//! - the ID token must verify with the nonce of THAT login; a token for a
//!   different nonce, an expired token or an unknown key is refused typed;
//! - the verified claims map to a membership role through the configured
//!   group map (highest matched group wins; unlisted = the least-privilege
//!   viewer default), and only then does the control plane resolve/create
//!   the user, link the external subject and mint ONE one-shot session
//!   token. Existing memberships keep their role (the IdP authorizes
//!   joining, never a privilege change).
//!
//! Secret authority note (P2-B): `state` and `nonce` are credential material
//! whose security value does NOT follow from their names — they are the
//! single-use CSRF credential and the ID-token replay binding. They are
//! therefore DOMAIN SECRET TYPES ([`OAuthState`], [`OidcNonce`]) wrapping
//! [`SecretValue`], and the pending-login map is keyed by SHA-256(state)
//! rather than the plaintext. A source scan over secret-ish field names
//! ("secret", "token", "password") can never be the PRIMARY secret
//! authority, precisely because ordinary names like `state`/`nonce` carry
//! security value; the type wrapper — not the identifier — is what keeps
//! them out of `Debug`/`Display`/serde/errors. The only plaintext escape is
//! the wire projection ([`SsoStart::into_wire_response`]) the frontend must
//! present back at callback.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use faktor_security::secret::SecretValue;
use rand::Rng;
use sha2::{Digest, Sha256};

use crate::enterprise::SsoConfigRef;
use crate::error::ControlPlaneError;
use crate::ids::OrganizationId;
use crate::oidc::{
    ClaimMapping, CodeExchangeRequest, IdTokenExpectations, OidcError, OidcMembership,
};
use crate::oidc_net::{urlencode, AsyncOidcAdapter};
use crate::rbac::Role;
use crate::service::{sha256_hex, Clock, ControlPlane, ExternalLogin};

/// How long one started login may wait for its callback.
pub const SSO_STATE_TTL_MS: i64 = 10 * 60 * 1000;
/// Bound on concurrently pending (started, not yet completed) logins.
pub const MAX_PENDING_SSO_LOGINS: usize = 256;

/// The OIDC `state` of one started login: the single-use CSRF credential the
/// callback must present back. A domain secret TYPE by security value, not
/// by name: `state` does not look secret-ish, so no field-name scan can be
/// the authority that keeps it out of logs — the wrapper is.
///
/// Deliberately has NO `Display`, serde or public accessor. Plaintext leaves
/// only through [`SsoStart::into_wire_response`]; the pending map holds the
/// SHA-256 digest, never the plaintext.
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthState(SecretValue);

impl OAuthState {
    fn new(value: String) -> Self {
        Self(SecretValue::new(value))
    }

    /// The only in-process reader of the plaintext (module-private).
    fn expose(&self) -> &str {
        self.0.expose()
    }

    /// The pending-map key: SHA-256 of the plaintext, so a dump of the
    /// pending map yields digests, never the credentials themselves.
    fn digest_key(&self) -> String {
        state_key(self.expose())
    }
}

impl fmt::Debug for OAuthState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OAuthState([redacted])")
    }
}

/// The OIDC `nonce` of one started login: the replay binding of the ID token.
/// A domain secret type for the same reason as [`OAuthState`] (see the
/// module-level secret authority note): it never appears in a rendering and
/// leaves the process only inside the authorization URL.
#[derive(Clone, PartialEq, Eq)]
pub struct OidcNonce(SecretValue);

impl OidcNonce {
    fn new(value: String) -> Self {
        Self(SecretValue::new(value))
    }

    /// The only in-process reader of the plaintext (module-private).
    fn expose(&self) -> &str {
        self.0.expose()
    }
}

impl fmt::Debug for OidcNonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OidcNonce([redacted])")
    }
}

/// The pending-map key of a presented state: SHA-256 hex of the plaintext.
/// Both the insert (mint) and the lookup (callback) sides go through this,
/// so the map itself never holds credential material.
fn state_key(state: &str) -> String {
    sha256_hex(state.as_bytes())
}

/// One started login: the authorization URL to redirect the browser to plus
/// the single-use state the callback must present (the nonce stays private
/// to the authority and rides only the authorization URL to the IdP).
///
/// `Debug` is CUSTOM and redacts every field: the URL embeds the plaintext
/// state and nonce as query parameters, so a derived rendering would leak
/// both secrets even though the fields are wrapped.
#[derive(Clone, PartialEq, Eq)]
pub struct SsoStart {
    authorization_url: String,
    state: OAuthState,
    nonce: OidcNonce,
}

impl fmt::Debug for SsoStart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SsoStart")
            .field("authorization_url", &"[redacted]")
            .field("state", &self.state)
            .field("nonce", &self.nonce)
            .finish()
    }
}

impl SsoStart {
    /// The wire projection: the authorization URL the browser is redirected
    /// to and the plaintext state the frontend must present back at
    /// callback. This constructor is the ONLY plaintext escape hatch for the
    /// state; the returned type still redacts `Debug`, so only the explicit
    /// accessors (used by the wire serializer) read the values.
    pub fn into_wire_response(self) -> SsoStartWireResponse {
        SsoStartWireResponse {
            authorization_url: self.authorization_url,
            state: self.state.expose().to_string(),
        }
    }
}

/// The wire projection of one started login (P2-B): carries the
/// authorization URL and the state for the frontend. Serialized only by the
/// route layer; `Debug` stays redacted so an accidental log never leaks the
/// embedded credentials.
#[derive(Clone, PartialEq, Eq)]
pub struct SsoStartWireResponse {
    authorization_url: String,
    state: String,
}

impl SsoStartWireResponse {
    pub fn authorization_url(&self) -> &str {
        &self.authorization_url
    }

    pub fn state(&self) -> &str {
        &self.state
    }
}

impl fmt::Debug for SsoStartWireResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SsoStartWireResponse")
            .field("authorization_url", &"[redacted]")
            .field("state", &"[redacted]")
            .finish()
    }
}

/// One completed SSO login: the control-plane session (token visible once)
/// and the membership decision that produced it.
#[derive(Debug, Clone)]
pub struct SsoLoginOutcome {
    pub login: ExternalLogin,
    pub membership: OidcMembership,
}

struct PendingLogin {
    organization: OrganizationId,
    redirect_uri: String,
    /// Wrapped: the nonce is the replay binding of the ID token and never
    /// leaves this process except inside the authorization URL.
    nonce: OidcNonce,
    /// Wrapped: the PKCE verifier is a credential (it proves possession of
    /// the code) and is moved straight into the exchange request.
    code_verifier: SecretValue,
    created_ms: i64,
}

/// The SSO login authority over one asynchronous OIDC adapter. The pending
/// map is keyed by SHA-256(state) — never the plaintext state.
pub struct SsoLogin {
    adapter: Arc<dyn AsyncOidcAdapter>,
    clock: Arc<dyn Clock>,
    pending: Mutex<BTreeMap<String, PendingLogin>>,
}

impl std::fmt::Debug for SsoLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SsoLogin")
            .field(
                "pending",
                &self.pending.lock().map(|p| p.len()).unwrap_or(0),
            )
            .finish()
    }
}

impl SsoLogin {
    pub fn new(adapter: Arc<dyn AsyncOidcAdapter>, clock: Arc<dyn Clock>) -> Self {
        Self {
            adapter,
            clock,
            pending: Mutex::new(BTreeMap::new()),
        }
    }

    fn lock_pending(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, PendingLogin>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn random_hex(bytes: usize) -> String {
        let mut rng = rand::rng();
        let mut out = String::with_capacity(bytes * 2);
        for _ in 0..bytes {
            let byte: u8 = rng.random();
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }

    /// Start one login: resolve the issuer's discovery document, mint the
    /// state/nonce/PKCE trio and return the authorization URL.
    pub async fn start(
        &self,
        organization: &OrganizationId,
        sso: &SsoConfigRef,
        redirect_uri: &str,
    ) -> Result<SsoStart, OidcError> {
        require_enabled(sso)?;
        if redirect_uri.trim().is_empty() || redirect_uri.len() > 2048 {
            return Err(OidcError::Malformed(
                "redirect_uri must be 1..=2048 bytes".into(),
            ));
        }
        let discovery = self.adapter.discovery(&sso.issuer).await?;
        let state = OAuthState::new(Self::random_hex(32));
        let nonce = OidcNonce::new(Self::random_hex(32));
        let code_verifier = SecretValue::new(Self::random_hex(32));
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(code_verifier.expose().as_bytes()));
        let now = self.clock.now_ms();
        {
            let mut pending = self.lock_pending();
            pending.retain(|_, login| now.saturating_sub(login.created_ms) < SSO_STATE_TTL_MS);
            if pending.len() >= MAX_PENDING_SSO_LOGINS {
                return Err(OidcError::Malformed(format!(
                    "too many pending sso logins (bound {MAX_PENDING_SSO_LOGINS})"
                )));
            }
            pending.insert(
                state.digest_key(),
                PendingLogin {
                    organization: organization.clone(),
                    redirect_uri: redirect_uri.to_string(),
                    nonce: nonce.clone(),
                    code_verifier: code_verifier.clone(),
                    created_ms: now,
                },
            );
        }
        let query = [
            ("response_type", "code".to_string()),
            ("client_id", sso.client_id.clone()),
            ("redirect_uri", redirect_uri.to_string()),
            ("scope", "openid email profile".to_string()),
            ("state", state.expose().to_string()),
            ("nonce", nonce.expose().to_string()),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256".to_string()),
        ];
        let mut url = discovery.authorization_endpoint;
        url.push(if url.contains('?') { '&' } else { '?' });
        for (index, (name, value)) in query.iter().enumerate() {
            if index > 0 {
                url.push('&');
            }
            url.push_str(name);
            url.push('=');
            url.push_str(&urlencode(value));
        }
        Ok(SsoStart {
            authorization_url: url,
            state,
            nonce,
        })
    }

    /// Complete one login: consume the state (single use), exchange the code
    /// with the PKCE verifier, verify the ID token against THAT login's
    /// nonce, map the membership and mint the control-plane session.
    pub async fn callback(
        &self,
        control_plane: &ControlPlane,
        organization: &OrganizationId,
        sso: &SsoConfigRef,
        redirect_uri: &str,
        code: &str,
        state: &str,
    ) -> Result<SsoLoginOutcome, OidcError> {
        require_enabled(sso)?;
        let now = self.clock.now_ms();
        let pending = {
            let mut pending = self.lock_pending();
            // The wire carries the plaintext state; the map is keyed by its
            // SHA-256, so only the digest is ever stored or compared here.
            let Some(login) = pending.remove(&state_key(state)) else {
                return Err(OidcError::StateInvalid);
            };
            if now.saturating_sub(login.created_ms) >= SSO_STATE_TTL_MS {
                return Err(OidcError::StateInvalid);
            }
            login
        };
        if pending.organization != *organization {
            return Err(OidcError::StateInvalid);
        }
        if pending.redirect_uri != redirect_uri {
            return Err(OidcError::RedirectMismatch);
        }
        let _discovery = self.adapter.discovery(&sso.issuer).await?;
        let tokens = self
            .adapter
            .exchange_code(&CodeExchangeRequest {
                code: SecretValue::new(code),
                redirect_uri: redirect_uri.to_string(),
                code_verifier: pending.code_verifier,
            })
            .await?;
        let claims = self
            .adapter
            .verify_id_token(
                tokens.id_token.expose(),
                &IdTokenExpectations {
                    issuer: sso.issuer.clone(),
                    audience: sso.client_id.clone(),
                    nonce: Some(pending.nonce.expose().to_string()),
                    now_ms: now,
                    clock_skew_ms: 0,
                },
            )
            .await?;
        let mapping = ClaimMapping {
            email_claim: "email".into(),
            groups_claim: sso.membership_claim.clone(),
            group_roles: sso.group_role_map.clone(),
            default_role: Role::Viewer,
            require_verified_email: true,
        };
        let membership = self.adapter.map_membership(&claims, &mapping)?;
        let login = control_plane
            .login_external(
                organization,
                &sso.issuer,
                &membership.subject,
                &membership.email,
                &membership.email,
                claims.email_verified,
                membership.role,
            )
            .map_err(control_plane_error_into_oidc)?;
        Ok(SsoLoginOutcome { login, membership })
    }
}

fn require_enabled(sso: &SsoConfigRef) -> Result<(), OidcError> {
    sso.validate()
        .map_err(|e| OidcError::Malformed(format!("sso configuration: {e}")))?;
    if !sso.enabled {
        return Err(OidcError::MembershipRefused(
            "sso is disabled for this organization".into(),
        ));
    }
    Ok(())
}

/// Map a control-plane refusal onto the OIDC surface without leaking
/// organization-existence truth (a foreign/missing org is a provider-level
/// refusal here; the route layer answers its own 404 first).
fn control_plane_error_into_oidc(error: ControlPlaneError) -> OidcError {
    match error {
        ControlPlaneError::Unauthorized(message) => OidcError::MembershipRefused(message),
        ControlPlaneError::Conflict(message) => OidcError::MembershipRefused(message),
        ControlPlaneError::NotFound(message) => OidcError::MembershipRefused(message),
        other => OidcError::MembershipRefused(other.to_string()),
    }
}

#[cfg(test)]
#[path = "sso_tests.rs"]
mod sso_tests;
