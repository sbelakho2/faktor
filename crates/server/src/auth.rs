//! Local auth: the frontend generates a 64-hex `FAKTOR_SERVER_PASSWORD` and
//! passes it to the daemon via env; the daemon never prints it. Every request
//! carries one of the Faktor-native claims:
//!
//! - `Authorization: Bearer <FAKTOR_SERVER_PASSWORD>`, or
//! - `x-faktor-server-password: <FAKTOR_SERVER_PASSWORD>`, or
//! - `Authorization: Bearer <per-start AuthToken>` — the legacy per-start
//!   token mechanism, retained (never persisted; a restart invalidates stale
//!   UI connections, which is the point of local auth).
//!
//! There is no product-compat auth arm: no `Basic` form is parsed, no fixed
//! username is special-cased, and no header is silently upgraded or
//! downgraded between schemes.
//!
//! # Migration note for pre-cutover clients
//!
//! Releases before the Faktor-native auth cutover accepted
//! `Authorization: Basic <base64(username:password)>` with one fixed,
//! product-derived username. That compatibility arm was removed: a `Basic`
//! header is now rejected exactly like any other unknown scheme (this module
//! has no Basic parser at all), so existing clients MUST migrate to the
//! `Bearer` form — `Authorization: Bearer <FAKTOR_SERVER_PASSWORD>` — or to
//! the `x-faktor-server-password` header. There is deliberately no downgrade
//! path: a rejected claim is a loud 401, never a fallback to a weaker scheme.

use rand::Rng;

/// The server password: read from `FAKTOR_SERVER_PASSWORD` or generated.
/// Comparisons are constant-time; the value is never serialized or logged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerPassword(String);

impl ServerPassword {
    /// Read `FAKTOR_SERVER_PASSWORD` from the environment. When missing (or
    /// empty/whitespace), generate a random 64-hex secret and log it at
    /// DEBUG — never stdout.
    pub fn from_env() -> Self {
        match std::env::var("FAKTOR_SERVER_PASSWORD") {
            Ok(v) if !v.trim().is_empty() => Self(v),
            _ => {
                let pw = Self::generate();
                tracing::debug!(
                    "FAKTOR_SERVER_PASSWORD not set; generated ephemeral server password"
                );
                pw
            }
        }
    }

    /// A random 64-hex secret.
    pub fn generate() -> Self {
        let mut rng = rand::rng();
        let bytes: [u8; 32] = rng.random();
        Self(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// Wrap an explicit secret (`auth.set` rotation). Callers validate the
    /// bounds; comparisons stay constant-time through [`ServerPassword`]'s
    /// own methods.
    pub fn new(secret: String) -> Self {
        Self(secret)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Check one `Authorization` header value against the password.
    /// Accepted: `Bearer <password>` only. Anything else (missing header,
    /// `Basic`, any other scheme, malformed/oversized value, wrong password)
    /// is rejected. `Basic` is not parsed at all — the pre-cutover
    /// compatibility arm is gone (see the module migration note).
    pub fn check_authorization(&self, header: Option<&str>) -> bool {
        let Some(header) = header else {
            return false;
        };
        if header.len() > MAX_AUTH_HEADER_BYTES {
            return false;
        }
        let Some(bearer) = header.strip_prefix("Bearer ") else {
            return false;
        };
        ct_eq(self.as_str().as_bytes(), bearer.as_bytes())
    }
}

/// The bound on one `Authorization` header value (bounded everything): a
/// header larger than this is rejected without any comparison.
pub const MAX_AUTH_HEADER_BYTES: usize = 4096;

/// Constant-time comparison of two fixed-length byte strings.
fn ct_eq(expected: &[u8], actual: &[u8]) -> bool {
    if expected.len() != actual.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in expected.iter().zip(actual.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Accepts the password from either `Authorization: Bearer <pw>` or
/// `x-faktor-server-password: <pw>`. No header → false; any mismatch → false.
pub fn check_password(
    password: &ServerPassword,
    authorization: Option<&str>,
    x_faktor_server_password: Option<&str>,
) -> bool {
    let expected = password.as_str().as_bytes();
    if let Some(authorization) = authorization {
        if authorization.len() > MAX_AUTH_HEADER_BYTES {
            return false;
        }
        if let Some(bearer) = authorization.strip_prefix("Bearer ") {
            if ct_eq(expected, bearer.as_bytes()) {
                return true;
            }
        }
    }
    if let Some(header) = x_faktor_server_password {
        if header.len() > MAX_AUTH_HEADER_BYTES {
            return false;
        }
        if ct_eq(expected, header.as_bytes()) {
            return true;
        }
    }
    false
}

/// Random per-start bearer token. Never persisted (a restart invalidates
/// stale UI connections, which is the point of local auth).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthToken(String);

impl AuthToken {
    pub fn generate() -> Self {
        let mut rng = rand::rng();
        let bytes: [u8; 32] = rng.random();
        Self(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Constant-time comparison (no length leakage beyond the token's own fixed
/// length).
pub fn check_bearer(token: &AuthToken, header: Option<&str>) -> bool {
    let Some(header) = header else {
        return false;
    };
    let Some(bearer) = header.strip_prefix("Bearer ") else {
        return false;
    };
    ct_eq(token.as_str().as_bytes(), bearer.as_bytes())
}

/// Constant-time check of one `Authorization: Bearer <value>` header against
/// an expected opaque bearer VALUE (the worker plane's own transport
/// credential, which is configured as plain text and is deliberately NOT the
/// daemon password and NOT a worker registration token). Same strictness as
/// [`check_bearer`]: exact `Bearer ` prefix, bounded header, full-value
/// constant-time equality, oversized headers refused before any comparison.
pub fn check_bearer_value(expected: &str, header: Option<&str>) -> bool {
    let Some(header) = header else {
        return false;
    };
    if header.len() > MAX_AUTH_HEADER_BYTES {
        return false;
    }
    let Some(bearer) = header.strip_prefix("Bearer ") else {
        return false;
    };
    ct_eq(expected.as_bytes(), bearer.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn tokens_are_random_and_hex() {
        let a = AuthToken::generate();
        let b = AuthToken::generate();
        assert_ne!(a, b);
        assert_eq!(a.as_str().len(), 64);
        assert!(a.as_str().bytes().all(|c| c.is_ascii_hexdigit()));
        let p = ServerPassword::generate();
        assert_eq!(p.as_str().len(), 64);
        assert!(p.as_str().bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(p, ServerPassword::generate());
    }

    #[test]
    fn bearer_matching_is_exact_and_strict() {
        let t = AuthToken::generate();
        assert!(check_bearer(&t, Some(&format!("Bearer {}", t.as_str()))));
        assert!(!check_bearer(&t, None));
        assert!(!check_bearer(&t, Some("")));
        assert!(!check_bearer(&t, Some(t.as_str())), "missing Bearer prefix");
        assert!(!check_bearer(&t, Some("bearer x")), "case-sensitive scheme");
        assert!(!check_bearer(&t, Some("Bearer x")), "wrong token");
        assert!(!check_bearer(&t, Some(&format!("Bearer {}/1", t.as_str()))));
        // Prefix-only headers must not match.
        assert!(!check_bearer(
            &t,
            Some(&format!("Bearer {}", &t.as_str()[..32]))
        ));
        // Trailing garbage must not match.
        assert!(!check_bearer(
            &t,
            Some(&format!("Bearer {} extra", t.as_str()))
        ));
    }

    #[test]
    fn token_survives_clone_and_display() {
        let t = AuthToken::generate();
        let t2 = t.clone();
        assert_eq!(t, t2);
        assert_eq!(t.as_str().to_string(), t.as_str());
    }

    #[test]
    fn server_password_reads_env_var() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("FAKTOR_SERVER_PASSWORD", "a".repeat(64));
        let pw = ServerPassword::from_env();
        assert_eq!(pw.as_str(), "a".repeat(64));
        std::env::remove_var("FAKTOR_SERVER_PASSWORD");
    }

    #[test]
    fn server_password_generates_when_env_missing_or_empty() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("FAKTOR_SERVER_PASSWORD");
        let pw = ServerPassword::from_env();
        assert_eq!(pw.as_str().len(), 64);
        assert!(pw.as_str().bytes().all(|c| c.is_ascii_hexdigit()));
        // Empty and whitespace-only values are treated as missing (a secret
        // that is empty is no secret at all).
        std::env::set_var("FAKTOR_SERVER_PASSWORD", "");
        let pw = ServerPassword::from_env();
        assert_eq!(pw.as_str().len(), 64);
        std::env::set_var("FAKTOR_SERVER_PASSWORD", "   ");
        let pw = ServerPassword::from_env();
        assert_eq!(pw.as_str().len(), 64);
        std::env::remove_var("FAKTOR_SERVER_PASSWORD");
    }

    #[test]
    fn password_accepted_in_both_header_forms() {
        let pw = ServerPassword::generate();
        // Bearer form.
        let auth = Some(&format!("Bearer {}", pw.as_str())[..]);
        assert!(check_password(&pw, auth, None));
        // x-faktor-server-password form.
        assert!(check_password(&pw, None, Some(pw.as_str())));
        // Both forms can even be sent together.
        assert!(check_password(&pw, auth, Some(pw.as_str())));
    }

    #[test]
    fn password_rejects_missing_wrong_and_partial() {
        let pw = ServerPassword::generate();
        assert!(!check_password(&pw, None, None), "no header");
        assert!(!check_password(&pw, Some(""), None));
        assert!(!check_password(&pw, None, Some("")));
        assert!(!check_password(&pw, Some("Bearer wrong"), None));
        assert!(!check_password(
            &pw,
            Some("Bearer wrong"),
            Some("also wrong")
        ));
        // Missing Bearer prefix on the auth header.
        assert!(!check_password(&pw, Some(pw.as_str()), None));
        // Scheme is case-sensitive.
        assert!(!check_password(
            &pw,
            Some(&format!("bearer {}", pw.as_str())),
            None
        ));
        // Prefix of the real password must not match.
        assert!(!check_password(
            &pw,
            Some(&format!("Bearer {}", &pw.as_str()[..32])),
            None
        ));
        // Trailing garbage must not match.
        assert!(!check_password(
            &pw,
            Some(&format!("Bearer {} extra", pw.as_str())),
            None
        ));
        // Wrong x-faktor-server-password value.
        assert!(!check_password(&pw, None, Some("wrong")));
        // Header containing the password with surrounding whitespace is
        // rejected (headers are exact).
        assert!(!check_password(
            &pw,
            None,
            Some(&format!(" {}", pw.as_str()))
        ));
    }

    #[test]
    fn password_and_legacy_token_are_independent() {
        // The per-start token flow must keep working alongside the password
        // flow.
        let token = AuthToken::generate();
        let pw = ServerPassword::generate();
        assert!(check_bearer(
            &token,
            Some(&format!("Bearer {}", token.as_str()))
        ));
        assert!(!check_password(
            &pw,
            Some(&format!("Bearer {}", token.as_str())),
            None
        ));
        assert!(!check_bearer(
            &token,
            Some(&format!("Bearer {}", pw.as_str()))
        ));
    }

    fn base64(value: &str) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(value)
    }

    #[test]
    fn basic_auth_is_rejected_entirely() {
        // The retired compatibility arm must be GONE: even a Basic header
        // carrying the correct password is rejected (unknown scheme, no
        // parser). This is the regression lock for the migration.
        let pw = ServerPassword::generate();
        for header in [
            format!("Basic {}", base64(&format!("legacy:{}", pw.as_str()))),
            format!("Basic {}", base64(&format!("admin:{}", pw.as_str()))),
            format!("Basic {}", base64(pw.as_str())),
            format!("basic {}", base64(pw.as_str())),
            format!("Basic {}", base64(&format!("legacy:{}", "wrong"))),
        ] {
            assert!(
                !pw.check_authorization(Some(&header)),
                "{header:?} must be rejected (Basic is not an accepted scheme)"
            );
            assert!(!check_password(&pw, Some(&header), None));
        }
        // The same credentials in the native Bearer form ARE accepted, so
        // the rejection above is about the scheme, not the secret.
        assert!(pw.check_authorization(Some(&format!("Bearer {}", pw.as_str()))));
        assert!(check_password(
            &pw,
            Some(&format!("Bearer {}", pw.as_str())),
            None
        ));
    }

    #[test]
    fn basic_auth_malformed_base64_rejected() {
        let pw = ServerPassword::generate();
        for bad in [
            "Basic !!!not-base64!!!",
            "Basic YTpi",  // truncated (missing padding)
            "Basic YTpi=", // wrong padding
            "Basic \u{00a0}\u{00a0}",
            "Basic -----",
        ] {
            assert!(
                !pw.check_authorization(Some(bad)),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn auth_garbage_and_missing_headers_rejected() {
        let pw = ServerPassword::generate();
        assert!(!pw.check_authorization(None));
        assert!(!pw.check_authorization(Some("")));
        assert!(!pw.check_authorization(Some("Basic")));
        assert!(!pw.check_authorization(Some("Digest YTpi")));
        assert!(!pw.check_authorization(Some(pw.as_str())));
        assert!(!pw.check_authorization(Some("Token 12345")));
        assert!(!pw.check_authorization(Some("Bearer ")));
    }

    #[test]
    fn oversized_header_rejected_before_any_comparison() {
        let pw = ServerPassword::generate();
        let huge = format!("Bearer {}", "A".repeat(MAX_AUTH_HEADER_BYTES));
        assert!(!pw.check_authorization(Some(&huge)));
        assert!(!check_password(&pw, Some(&huge), None));
        assert!(!check_password(&pw, None, Some(&huge)));
        // Exactly at the bound is still rejected only when the payload is
        // invalid; a legitimately sized header is fine.
        let ok = format!("Bearer {}", pw.as_str());
        assert!(ok.len() < MAX_AUTH_HEADER_BYTES);
        assert!(pw.check_authorization(Some(&ok)));
    }

    #[test]
    fn bearer_and_x_faktor_forms_still_work() {
        let pw = ServerPassword::generate();
        // Bearer through the same entry point.
        let header = format!("Bearer {}", pw.as_str());
        assert!(pw.check_authorization(Some(&header)));
        assert!(!pw.check_authorization(Some("Bearer wrong")));
        // x-faktor-server-password is a separate header; check_password covers
        // it (the Authorization entry point must NOT accept it).
        assert!(check_password(&pw, None, Some(pw.as_str())));
        assert!(!pw.check_authorization(Some(pw.as_str())));
    }

    #[test]
    fn legacy_token_is_not_the_password_in_any_scheme() {
        let pw = ServerPassword::generate();
        let token = AuthToken::generate();
        let header = format!("Bearer {}", token.as_str());
        assert!(!pw.check_authorization(Some(&header)));
        assert!(!check_password(&pw, Some(&header), None));
        assert!(!pw.check_authorization(Some(&format!(
            "Basic {}",
            base64(&format!("legacy:{}", token.as_str()))
        ))));
        // And the token path still accepts its own bearer.
        assert!(check_bearer(&token, Some(&header)));
    }
}
