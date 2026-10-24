#![allow(clippy::items_after_test_module)]

use std::collections::HashMap;
use std::sync::Mutex;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::http::{header, HeaderMap};
use base64::Engine;
use rand::RngCore;
use rand_core::OsRng;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::error::{SiteError, SiteResult};

pub const SESSION_COOKIE: &str = "faktor_site_session";
pub const CSRF_COOKIE: &str = "faktor_site_csrf";
const MAX_COOKIE_HEADER: usize = 8192;
const MAX_COOKIE_VALUE: usize = 256;
const MAX_RATE_ENTRIES: usize = 4096;

pub fn random_secret() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn hash_secret(value: &str) -> String {
    hex_bytes(&Sha256::digest(value.as_bytes()))
}

pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

pub fn valid_secret_shape(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

pub fn hash_password(password: &str) -> SiteResult<String> {
    validate_password(password)?;
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|_| SiteError::internal_message("password hashing failed"))
}

pub fn verify_password(stored_hash: &str, password: &str) -> bool {
    if password.is_empty() || password.len() > 256 {
        return false;
    }
    let Ok(parsed) = PasswordHash::new(stored_hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

pub fn validate_password(password: &str) -> SiteResult<()> {
    let bytes = password.as_bytes();
    if bytes.len() < 12 || bytes.len() > 256 {
        return Err(SiteError::Unprocessable(
            "Use a password between 12 and 256 bytes.",
        ));
    }
    if bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Err(SiteError::Unprocessable("Choose a stronger password."));
    }
    if bytes.contains(&0) {
        return Err(SiteError::BadRequest(
            "The password contains a control byte.",
        ));
    }
    Ok(())
}

pub fn normalize_email(raw: &str) -> Option<String> {
    let email = raw.trim().to_ascii_lowercase();
    if email.len() < 5 || email.len() > 254 || !email.is_ascii() {
        return None;
    }
    if email
        .bytes()
        .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return None;
    }
    let (local, domain) = email.rsplit_once('@')?;
    if local.is_empty() || local.len() > 64 || domain.is_empty() || domain.len() > 253 {
        return None;
    }
    if !domain.contains('.')
        || domain.starts_with('.')
        || domain.ends_with('.')
        || domain.contains("..")
    {
        return None;
    }
    if local.starts_with('.') || local.ends_with('.') || local.contains("..") {
        return None;
    }
    if !domain
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'.')
    {
        return None;
    }
    if domain
        .split('.')
        .any(|label| label.is_empty() || label.starts_with('-') || label.ends_with('-'))
    {
        return None;
    }
    Some(email)
}

pub fn validate_display_name(raw: &str) -> SiteResult<String> {
    let name = raw.trim();
    if name.is_empty() || name.chars().count() > 80 {
        return Err(SiteError::Unprocessable(
            "Use a display name between 1 and 80 characters.",
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(SiteError::BadRequest(
            "The display name contains a control character.",
        ));
    }
    Ok(name.to_string())
}

pub fn validate_org_name(raw: &str) -> SiteResult<String> {
    let name = raw.trim();
    if name.is_empty() || name.chars().count() > 120 {
        return Err(SiteError::Unprocessable(
            "Use an organization name between 1 and 120 characters.",
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(SiteError::BadRequest(
            "The organization name contains a control character.",
        ));
    }
    Ok(name.to_string())
}

pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
        if out.len() >= 48 {
            break;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "workspace".to_string()
    } else {
        out
    }
}

pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    if raw.len() > MAX_COOKIE_HEADER {
        return None;
    }
    for pair in raw.split(';') {
        let pair = pair.trim();
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key == name
            && !value.is_empty()
            && value.len() <= MAX_COOKIE_VALUE
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return Some(value.to_string());
        }
    }
    None
}

pub fn session_cookie(config: &Config, value: &str, max_age_secs: i64) -> String {
    cookie(config, SESSION_COOKIE, value, max_age_secs)
}

pub fn csrf_cookie(config: &Config, value: &str) -> String {
    cookie(config, CSRF_COOKIE, value, 3_600)
}

pub fn clear_session_cookie(config: &Config) -> String {
    cookie(config, SESSION_COOKIE, "", 0)
}

pub fn clear_csrf_cookie(config: &Config) -> String {
    cookie(config, CSRF_COOKIE, "", 0)
}

fn cookie(config: &Config, name: &str, value: &str, max_age_secs: i64) -> String {
    let secure = if config.cookie_secure { "; Secure" } else { "" };
    if max_age_secs == 0 {
        format!("{name}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax{secure}")
    } else {
        format!("{name}={value}; Path=/; Max-Age={max_age_secs}; HttpOnly; SameSite=Lax{secure}")
    }
}

pub fn client_ip(headers: &HeaderMap, config: &Config) -> String {
    if config.trust_proxy {
        if let Some(forwarded) = headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
        {
            if let Some(first) = forwarded.split(',').next() {
                let candidate = first.trim();
                if !candidate.is_empty() && candidate.len() <= 64 {
                    return candidate.to_string();
                }
            }
        }
        if let Some(real) = headers
            .get("x-real-ip")
            .and_then(|value| value.to_str().ok())
        {
            if !real.is_empty() && real.len() <= 64 {
                return real.to_string();
            }
        }
    }
    if let Some(peer) = headers
        .get("x-faktor-peer-ip")
        .and_then(|value| value.to_str().ok())
    {
        if !peer.is_empty() && peer.len() <= 64 {
            return peer.to_string();
        }
    }
    "local".to_string()
}

pub fn client_tag(headers: &HeaderMap, config: &Config) -> String {
    let ip = client_ip(headers, config);
    let agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .chars()
        .take(160)
        .collect::<String>();
    hash_secret(&format!("{ip}\n{agent}"))[..16].to_string()
}

pub fn same_origin(headers: &HeaderMap, config: &Config) -> bool {
    if let Some(site) = headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
    {
        if site.eq_ignore_ascii_case("cross-site") {
            return false;
        }
    }
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let expected = config.base_origin();
    origin == expected || origin == format!("{expected}/")
}

pub fn safe_next(raw: Option<&str>, fallback: &str) -> String {
    match raw {
        Some(value)
            if value.starts_with('/')
                && !value.starts_with("//")
                && !value.contains('\\')
                && value.len() <= 512
                && !value.chars().any(char::is_control) =>
        {
            value.to_string()
        }
        _ => fallback.to_string(),
    }
}

#[derive(Debug)]
struct Window {
    count: u32,
    reset_ms: i64,
}

#[derive(Default)]
pub struct RateLimiter {
    windows: Mutex<HashMap<String, Window>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn check(&self, key: &str, limit: u32, window_ms: i64, now_ms: i64) -> SiteResult<()> {
        if key.len() > 256 || limit == 0 || window_ms <= 0 {
            return Err(SiteError::internal_message(
                "invalid rate-limit configuration",
            ));
        }
        let mut windows = self
            .windows
            .lock()
            .map_err(|_| SiteError::internal_message("rate limiter lock is poisoned"))?;
        if windows.len() >= MAX_RATE_ENTRIES {
            windows.retain(|_, window| window.reset_ms > now_ms);
            if windows.len() >= MAX_RATE_ENTRIES && !windows.contains_key(key) {
                return Err(SiteError::RateLimited {
                    retry_after_secs: 60,
                });
            }
        }
        let window = windows.entry(key.to_string()).or_insert(Window {
            count: 0,
            reset_ms: now_ms.saturating_add(window_ms),
        });
        if now_ms >= window.reset_ms {
            window.count = 0;
            window.reset_ms = now_ms.saturating_add(window_ms);
        }
        if window.count >= limit {
            let retry_after_secs = ((window.reset_ms - now_ms).max(1) + 999) / 1000;
            return Err(SiteError::RateLimited {
                retry_after_secs: retry_after_secs as u64,
            });
        }
        window.count = window.count.saturating_add(1);
        Ok(())
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub fn looks_like_pan(value: &str) -> bool {
    let digits: String = value.chars().filter(|ch| ch.is_ascii_digit()).collect();
    if value.chars().any(|ch| ch.is_ascii_digit()) && digits.len() >= 13 && digits.len() <= 19 {
        let compact: String = value
            .chars()
            .filter(|ch| !ch.is_ascii_whitespace() && *ch != '-')
            .collect();
        if compact.chars().all(|ch| ch.is_ascii_digit()) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hashing_is_salted_and_verifies() {
        let first = hash_password("correct horse battery staple").unwrap();
        let second = hash_password("correct horse battery staple").unwrap();
        assert_ne!(first, second);
        assert!(verify_password(&first, "correct horse battery staple"));
        assert!(!verify_password(&first, "wrong password value"));
        assert!(hash_password("short").is_err());
    }

    #[test]
    fn email_and_slug_validation_rejects_hostile_shapes() {
        assert!(normalize_email(" Person@Example.COM ").is_some());
        for bad in [
            "a@b",
            "@example.com",
            "a b@example.com",
            "a@-x.com",
            "a@x..com",
        ] {
            assert!(normalize_email(bad).is_none(), "{bad}");
        }
        assert_eq!(slugify("  A team / workspace  "), "a-team-workspace");
    }

    #[test]
    fn rate_limiter_is_bounded_and_reports_retry() {
        let limiter = RateLimiter::new();
        for _ in 0..3 {
            limiter.check("k", 3, 10_000, 1_000).unwrap();
        }
        let error = limiter.check("k", 3, 10_000, 1_000).unwrap_err();
        assert_eq!(error.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
        limiter.check("k", 3, 10_000, 20_000).unwrap();
    }

    #[test]
    fn safe_next_rejects_absolute_and_scheme_relative_paths() {
        assert_eq!(safe_next(Some("//evil.example"), "/account/"), "/account/");
        assert_eq!(
            safe_next(Some("https://evil.example"), "/account/"),
            "/account/"
        );
        assert_eq!(
            safe_next(Some("/account/invoices/"), "/account/"),
            "/account/invoices/"
        );
    }

    #[test]
    fn cookies_have_security_attributes() {
        let mut config = crate::test_support::test_config();
        config.cookie_secure = true;
        let cookie = session_cookie(&config, "abc", 60);
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("Secure"));
        assert!(cookie.contains("SameSite=Lax"));
    }
}
