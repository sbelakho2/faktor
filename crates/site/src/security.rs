// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
#![allow(clippy::items_after_test_module)]

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
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
const MAX_FORWARDED_BYTES: usize = 1024;
const MAX_FORWARDED_HOPS: usize = 32;
const MAX_IP_LITERAL_BYTES: usize = 64;
const PEER_IP_HEADER: &str = "x-faktor-peer-ip";

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
    let Some(peer) = single_ip_header(headers, PEER_IP_HEADER) else {
        return "local".to_string();
    };
    if !config.trusts_proxy(peer) {
        return peer.to_string();
    }
    match forwarded_chain(headers) {
        Some(chain) => derived_client(&chain, peer, config).to_string(),
        None => peer.to_string(),
    }
}

/// Walks the forwarding chain from the hop nearest to the socket peer toward
/// the origin. Every hop that sits inside `trusted_proxy_cidrs` is skipped; the
/// first untrusted address is the client. An all-trusted chain yields the
/// leftmost (originating) address.
fn derived_client(chain: &[IpAddr], peer: IpAddr, config: &Config) -> IpAddr {
    let mut client = peer;
    for address in chain.iter().rev() {
        if config.trusts_proxy(*address) {
            client = *address;
        } else {
            return *address;
        }
    }
    client
}

fn single_ip_header(headers: &HeaderMap, name: &str) -> Option<IpAddr> {
    let raw = headers.get(name)?.to_str().ok()?;
    parse_ip_literal(raw)
}

fn forwarded_chain(headers: &HeaderMap) -> Option<Vec<IpAddr>> {
    parse_forwarded(headers).or_else(|| parse_x_forwarded_for(headers))
}

fn parse_forwarded(headers: &HeaderMap) -> Option<Vec<IpAddr>> {
    let raw = joined_header(headers, "forwarded")?;
    let elements = split_quoted(&raw, ',')?;
    if elements.is_empty() || elements.len() > MAX_FORWARDED_HOPS {
        return None;
    }
    let mut chain = Vec::with_capacity(elements.len());
    for element in elements {
        let pairs = split_quoted(element, ';')?;
        let mut address = None;
        for pair in pairs {
            let (name, value) = pair.split_once('=')?;
            if name.trim().eq_ignore_ascii_case("for") {
                if address.is_some() {
                    return None;
                }
                address = Some(forwarded_value_ip(value)?);
            }
        }
        chain.push(address?);
    }
    Some(chain)
}

fn parse_x_forwarded_for(headers: &HeaderMap) -> Option<Vec<IpAddr>> {
    let raw = joined_header(headers, "x-forwarded-for")?;
    let elements = raw.split(',').collect::<Vec<_>>();
    if elements.is_empty() || elements.len() > MAX_FORWARDED_HOPS {
        return None;
    }
    elements
        .into_iter()
        .map(|element| parse_ip_literal(element.trim()))
        .collect()
}

fn joined_header(headers: &HeaderMap, name: &str) -> Option<String> {
    let mut joined = String::new();
    for value in headers.get_all(name) {
        let value = value.to_str().ok()?;
        if joined.len() + value.len() + usize::from(!joined.is_empty()) > MAX_FORWARDED_BYTES {
            return None;
        }
        if !joined.is_empty() {
            joined.push(',');
        }
        joined.push_str(value);
    }
    if joined.trim().is_empty() {
        return None;
    }
    Some(joined)
}

fn split_quoted(raw: &str, delimiter: char) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (index, ch) in raw.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            _ if ch == delimiter && !quoted => {
                parts.push(&raw[start..index]);
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    if quoted || escaped {
        return None;
    }
    parts.push(&raw[start..]);
    Some(parts)
}

fn forwarded_value_ip(raw: &str) -> Option<IpAddr> {
    let value = raw.trim();
    let unquoted = if let Some(inner) = value.strip_prefix('"') {
        let inner = inner.strip_suffix('"')?;
        let mut unescaped = String::with_capacity(inner.len());
        let mut escaped = false;
        for ch in inner.chars() {
            if escaped {
                unescaped.push(ch);
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                return None;
            } else {
                unescaped.push(ch);
            }
        }
        if escaped {
            return None;
        }
        unescaped
    } else {
        if value.contains('"') {
            return None;
        }
        value.to_string()
    };
    parse_ip_literal(&unquoted)
}

fn parse_ip_literal(raw: &str) -> Option<IpAddr> {
    let value = raw.trim();
    if value.is_empty() || value.len() > MAX_IP_LITERAL_BYTES {
        return None;
    }
    let address = if let Some(rest) = value.strip_prefix('[') {
        let (address, tail) = rest.split_once(']')?;
        if !tail.is_empty() {
            let port = tail.strip_prefix(':')?;
            if port.is_empty()
                || port.len() > 5
                || !port.bytes().all(|byte| byte.is_ascii_digit())
                || port.parse::<u16>().is_err()
            {
                return None;
            }
        }
        address.parse::<Ipv6Addr>().ok().map(IpAddr::V6)?
    } else if value.contains(':') {
        value.parse::<Ipv6Addr>().ok().map(IpAddr::V6)?
    } else {
        value.parse::<Ipv4Addr>().ok().map(IpAddr::V4)?
    };
    Some(address.to_canonical())
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

    fn proxy_config(cidrs: &[&str]) -> Config {
        let mut config = crate::test_support::test_config();
        config.trusted_proxy_cidrs = cidrs.iter().map(|cidr| cidr.parse().unwrap()).collect();
        config
    }

    fn request_headers(peer: Option<&str>, forwarded: &[(&'static str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(peer) = peer {
            headers.insert(
                PEER_IP_HEADER,
                axum::http::HeaderValue::from_str(peer).unwrap(),
            );
        }
        for (name, value) in forwarded {
            headers.insert(
                axum::http::HeaderName::from_static(name),
                axum::http::HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn untrusted_peer_cannot_spoof_forwarding_headers() {
        let config = proxy_config(&["10.0.0.0/8"]);
        for headers in [
            request_headers(Some("198.51.100.7"), &[("x-forwarded-for", "1.2.3.4")]),
            request_headers(
                Some("198.51.100.7"),
                &[("forwarded", "for=1.2.3.4;proto=https")],
            ),
            request_headers(
                Some("198.51.100.7"),
                &[("forwarded", "for=1.2.3.4"), ("x-forwarded-for", "1.2.3.4")],
            ),
        ] {
            assert_eq!(client_ip(&headers, &config), "198.51.100.7");
            assert_eq!(
                headers.get(PEER_IP_HEADER).unwrap().to_str().unwrap(),
                "198.51.100.7"
            );
        }
        let no_peer = request_headers(None, &[("x-forwarded-for", "1.2.3.4")]);
        assert_eq!(client_ip(&no_peer, &config), "local");
    }

    #[test]
    fn trusted_peer_forwarding_headers_are_honored() {
        let config = proxy_config(&["10.0.0.0/8"]);
        let forwarded = request_headers(
            Some("10.0.0.5"),
            &[("forwarded", "for=203.0.113.9;proto=https")],
        );
        assert_eq!(client_ip(&forwarded, &config), "203.0.113.9");
        let xff = request_headers(Some("10.0.0.5"), &[("x-forwarded-for", "203.0.113.9")]);
        assert_eq!(client_ip(&xff, &config), "203.0.113.9");
        let real = request_headers(Some("10.0.0.5"), &[("x-real-ip", "203.0.113.9")]);
        assert_eq!(client_ip(&real, &config), "10.0.0.5");
        let none = request_headers(Some("10.0.0.5"), &[]);
        assert_eq!(client_ip(&none, &config), "10.0.0.5");
    }

    #[test]
    fn multi_hop_trusted_chain_resolves_the_origin() {
        let config = proxy_config(&["10.0.0.0/8", "192.0.2.0/24"]);
        let headers = request_headers(
            Some("10.0.0.5"),
            &[("x-forwarded-for", "198.51.100.9, 10.0.0.6, 192.0.2.7")],
        );
        assert_eq!(client_ip(&headers, &config), "198.51.100.9");
        let fully_trusted = request_headers(
            Some("10.0.0.5"),
            &[("x-forwarded-for", "192.0.2.8, 10.0.0.6, 192.0.2.7")],
        );
        assert_eq!(client_ip(&fully_trusted, &config), "192.0.2.8");
    }

    #[test]
    fn mixed_chain_uses_the_rightmost_untrusted_address() {
        let config = proxy_config(&["10.0.0.0/8"]);
        let headers = request_headers(
            Some("10.0.0.5"),
            &[("x-forwarded-for", "1.2.3.4, 10.0.0.6, 198.51.100.8")],
        );
        assert_eq!(client_ip(&headers, &config), "198.51.100.8");
        let headers = request_headers(
            Some("10.0.0.5"),
            &[("x-forwarded-for", "1.2.3.4, 198.51.100.8, 10.0.0.6")],
        );
        assert_eq!(client_ip(&headers, &config), "198.51.100.8");
    }

    #[test]
    fn malformed_or_oversized_forwarding_headers_are_ignored() {
        let config = proxy_config(&["10.0.0.0/8"]);
        for value in [
            "not-an-ip",
            "203.0.113.9, not-an-ip",
            "203.0.113.9,,198.51.100.1",
            "unknown",
            "[2001:db8::1",
            "[2001:db8::1]:not-a-port",
        ] {
            let headers = request_headers(Some("10.0.0.5"), &[("x-forwarded-for", value)]);
            assert_eq!(client_ip(&headers, &config), "10.0.0.5", "{value}");
        }
        for value in [
            "for=_hidden",
            "for=\"2001:db8::1",
            "for=203.0.113.9, proto=https",
            "for=203.0.113.9;for=198.51.100.1",
        ] {
            let headers = request_headers(Some("10.0.0.5"), &[("forwarded", value)]);
            assert_eq!(client_ip(&headers, &config), "10.0.0.5", "{value}");
        }
        let oversized = format!("203.0.113.9, {}", "10.0.0.6, ".repeat(200));
        let headers = request_headers(Some("10.0.0.5"), &[("x-forwarded-for", &oversized)]);
        assert_eq!(client_ip(&headers, &config), "10.0.0.5");
        let malformed_forwarded = request_headers(
            Some("10.0.0.5"),
            &[
                ("forwarded", "not valid"),
                ("x-forwarded-for", "203.0.113.9"),
            ],
        );
        assert_eq!(client_ip(&malformed_forwarded, &config), "203.0.113.9");
    }

    #[test]
    fn forwarding_addresses_are_strictly_normalized() {
        let config = proxy_config(&["10.0.0.0/8"]);
        let headers = request_headers(
            Some("10.0.0.5"),
            &[("x-forwarded-for", "2001:0DB8:0:0:0:0:0:1")],
        );
        assert_eq!(client_ip(&headers, &config), "2001:db8::1");
        let headers = request_headers(
            Some("10.0.0.5"),
            &[("forwarded", "for=\"[2001:db8::1]:443\"")],
        );
        assert_eq!(client_ip(&headers, &config), "2001:db8::1");
        let headers = request_headers(Some("10.0.0.5"), &[("x-forwarded-for", "::ffff:192.0.2.1")]);
        assert_eq!(client_ip(&headers, &config), "192.0.2.1");
    }
}
