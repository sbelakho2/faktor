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

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
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
use crate::service::{Clock, ControlPlane, ExternalLogin};

/// How long one started login may wait for its callback.
pub const SSO_STATE_TTL_MS: i64 = 10 * 60 * 1000;
/// Bound on concurrently pending (started, not yet completed) logins.
pub const MAX_PENDING_SSO_LOGINS: usize = 256;

/// One started login: the authorization URL to redirect the browser to plus
/// the state the callback must present (the nonce is returned for the local
/// test surface; production adapters keep it private to the exchange).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsoStart {
    pub authorization_url: String,
    pub state: String,
    pub nonce: String,
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
    nonce: String,
    code_verifier: String,
    created_ms: i64,
}

/// The SSO login authority over one asynchronous OIDC adapter.
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
        let state = Self::random_hex(32);
        let nonce = Self::random_hex(32);
        let code_verifier = Self::random_hex(32);
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(code_verifier.as_bytes()));
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
                state.clone(),
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
            ("state", state.clone()),
            ("nonce", nonce.clone()),
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
            let Some(login) = pending.remove(state) else {
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
                code: code.to_string(),
                redirect_uri: redirect_uri.to_string(),
                code_verifier: pending.code_verifier,
            })
            .await?;
        let claims = self
            .adapter
            .verify_id_token(
                &tokens.id_token,
                &IdTokenExpectations {
                    issuer: sso.issuer.clone(),
                    audience: sso.client_id.clone(),
                    nonce: Some(pending.nonce),
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
