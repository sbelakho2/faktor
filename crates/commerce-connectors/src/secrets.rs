//! Secret discipline for connector credentials.
//!
//! Spec §10/§13: API credentials live outside the model — config references
//! **environment variable names**, values are registered with the outbound
//! secret scanner, and they never appear in schema, arguments,
//! `ToolOutcome`, model context, traces or browser command lines.
//!
//! This module is the connector-side enforcement:
//!
//! * [`SecretString`] wraps a credential value. It has no `Display`, no
//!   `Serialize`, no `Deserialize`; its `Debug` is `<redacted>`; the
//!   plaintext is only reachable through a crate-private accessor used to
//!   build one wire header.
//! * [`SecretGuard`] wraps the existing Faktor secret-scanning utility
//!   (`faktor-security`'s exact-value [`SecretRegistry`] plus the frozen
//!   [`SecretPolicy`] patterns). Registration fingerprints the value; the
//!   plaintext is never retained by the scanner. [`SecretGuard::scrub`]
//!   redacts every registered value and every pattern hit, and every
//!   diagnostic detail string passes through it before it can reach a sink.
//! * [`CredentialProvider`] resolves a name to a value. The production
//!   implementation reads the process environment; tests inject a map, and
//!   either way only the *name* may appear in an error.
//!
//! Nothing here is a substitute for the transport's own outbound scan: the
//! injected transport scans the payload again. This is the connector half of
//! a two-sided guarantee.

use std::fmt;
use std::sync::Mutex;

use faktor_security::registry::SecretRegistry;
use faktor_security::{redact, SecretHit, SecretPolicy, REDACTION_TRUNCATION_MARKER};

use crate::config::ConfigError;

/// At most this many redactions per [`SecretGuard::scrub`] call (bounded
/// output growth on hostile input). When more remain, the entire
/// unprocessed tail becomes one [`REDACTION_TRUNCATION_MARKER`] instead of
/// being appended raw.
const MAX_SCRUB_HITS: usize = 64;

/// A credential value.
///
/// Deliberately unprintable and unserializable: the only way out is the
/// crate-private [`SecretString::expose`], which the request builders use to
/// place the value into exactly one header.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    /// Wrap a value. Callers register it with a [`SecretGuard`].
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// The plaintext, for wire construction only.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    /// True when the credential is the empty string.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The credential length in bytes (never the value).
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(<redacted>)")
    }
}

/// The shared secret scanner: exact registered values plus the frozen
/// pattern policy.
pub struct SecretGuard {
    registry: Mutex<SecretRegistry>,
    policy: SecretPolicy,
}

impl Default for SecretGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretGuard {
    /// A guard with the frozen default pattern policy and no registered
    /// values.
    pub fn new() -> Self {
        Self {
            registry: Mutex::new(SecretRegistry::new()),
            policy: SecretPolicy::default(),
        }
    }

    /// A guard with an explicit policy.
    pub fn with_policy(policy: SecretPolicy) -> Self {
        Self {
            registry: Mutex::new(SecretRegistry::new()),
            policy,
        }
    }

    /// Register one credential value (fingerprinted; the plaintext is never
    /// retained). Empty values are ignored, as are duplicates.
    pub fn register(&self, value: &str) {
        if value.is_empty() {
            return;
        }
        let mut registry = self.lock();
        registry.register(value.as_bytes());
    }

    /// How many exact values are registered.
    pub fn registered_len(&self) -> usize {
        self.lock().len()
    }

    /// Every hit (exact registered values first, then pattern hits).
    pub fn hits(&self, text: &str) -> Vec<SecretHit> {
        let mut hits = self.lock().scan_exact(text.as_bytes());
        hits.extend(faktor_security::scan_secrets(text, &self.policy));
        hits
    }

    /// True when `text` contains a registered value or a pattern hit.
    pub fn contains_secret(&self, text: &str) -> bool {
        !self.hits(text).is_empty()
    }

    /// Redact every registered value and pattern hit. Never panics,
    /// whatever the input. If either redaction stage finds more hits than
    /// its cap, the ENTIRE unprocessed tail is replaced by
    /// [`REDACTION_TRUNCATION_MARKER`] — no occurrence past a cap is ever
    /// copied through raw.
    pub fn scrub(&self, text: &str) -> String {
        let after_patterns = redact(text, &self.policy);
        let exact = self.lock().scan_exact(after_patterns.as_bytes());
        if exact.is_empty() {
            return after_patterns;
        }
        let bytes = after_patterns.as_bytes();
        let capped = exact.len() > MAX_SCRUB_HITS;
        let mut out = String::with_capacity(after_patterns.len() + 32);
        let mut cursor = 0usize;
        for hit in exact.into_iter().take(MAX_SCRUB_HITS) {
            let start = hit.offset.min(bytes.len());
            let end = (hit.offset + hit.len).min(bytes.len());
            if start < cursor || start > end {
                continue;
            }
            // Offsets come from a byte scan over `after_patterns`; they are
            // always on char boundaries of ASCII-registered values, but a
            // defensive boundary check keeps this panic-free even if a
            // pattern rewrite shifted the text.
            if !after_patterns.is_char_boundary(start) || !after_patterns.is_char_boundary(end) {
                continue;
            }
            out.push_str(&after_patterns[cursor..start]);
            out.push_str(&hit.redacted);
            cursor = end;
        }
        if capped {
            out.push_str(REDACTION_TRUNCATION_MARKER);
        } else {
            out.push_str(&after_patterns[cursor..]);
        }
        out
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SecretRegistry> {
        // Poison-tolerant: a panic elsewhere must never turn a secret scan
        // into a panic or a silent pass-through.
        self.registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl fmt::Debug for SecretGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretGuard")
            .field("registered", &self.registered_len())
            .field("policy", &self.policy.scan_enabled)
            .finish()
    }
}

/// Resolves an environment variable *name* to a credential value.
pub trait CredentialProvider: Send + Sync {
    /// Resolve one name. Errors carry the name, never the value.
    fn resolve(&self, env_name: &str) -> Result<SecretString, ConfigError>;
}

/// The production provider: the process environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnvCredentials;

impl CredentialProvider for ProcessEnvCredentials {
    fn resolve(&self, env_name: &str) -> Result<SecretString, ConfigError> {
        match std::env::var(env_name) {
            Ok(value) if !value.trim().is_empty() => Ok(SecretString::new(value)),
            Ok(_) => Err(ConfigError::EmptyCredential {
                env_name: env_name.to_string(),
            }),
            Err(std::env::VarError::NotPresent) => Err(ConfigError::MissingCredential {
                env_name: env_name.to_string(),
            }),
            Err(std::env::VarError::NotUnicode(_)) => Err(ConfigError::CredentialNotUnicode {
                env_name: env_name.to_string(),
            }),
        }
    }
}

/// Resolve one credential and register it with the guard in one step, so a
/// resolved value can never exist un-registered in a connector.
pub fn resolve_registered(
    provider: &dyn CredentialProvider,
    guard: &SecretGuard,
    env_name: &str,
) -> Result<SecretString, ConfigError> {
    let value = provider.resolve(env_name)?;
    guard.register(value.expose());
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL: &str = "connector-real-credential-9f2a";
    const DECOY: &str = "connector-decoy-credential-1a2b";
    const PATTERN_DECOY: &str = "AKIA0123456789ABCDEF";

    fn guard_with_both() -> SecretGuard {
        let guard = SecretGuard::new();
        guard.register(DECOY);
        guard.register(REAL);
        guard
    }

    #[test]
    fn registered_values_and_pattern_hits_are_redacted() {
        let guard = guard_with_both();
        let text = format!("url?key={REAL} and {PATTERN_DECOY} end");
        let scrubbed = guard.scrub(&text);
        assert!(!scrubbed.contains(REAL), "{scrubbed}");
        assert!(!scrubbed.contains(PATTERN_DECOY), "{scrubbed}");
        assert!(scrubbed.contains("url?key="), "{scrubbed}");
    }

    #[test]
    fn exact_decoys_never_shield_the_real_credential() {
        let guard = guard_with_both();
        let mut text = (0..MAX_SCRUB_HITS)
            .map(|_| DECOY)
            .collect::<Vec<_>>()
            .join(" ");
        text.push(' ');
        text.push_str(REAL);
        let scrubbed = guard.scrub(&text);
        assert_eq!(scrubbed.matches(DECOY).count(), 0, "{scrubbed}");
        assert_eq!(
            scrubbed.matches(REAL).count(),
            0,
            "real credential past the cap leaked: {scrubbed}"
        );
        assert!(scrubbed.contains(REDACTION_TRUNCATION_MARKER), "{scrubbed}");
    }

    #[test]
    fn pattern_decoys_never_shield_a_registered_credential() {
        let guard = guard_with_both();
        let mut text = (0..33).map(|_| PATTERN_DECOY).collect::<Vec<_>>().join(" ");
        text.push(' ');
        text.push_str(REAL);
        let scrubbed = guard.scrub(&text);
        assert!(!scrubbed.contains(PATTERN_DECOY), "{scrubbed}");
        assert!(!scrubbed.contains(REAL), "{scrubbed}");
        assert!(scrubbed.contains(REDACTION_TRUNCATION_MARKER), "{scrubbed}");
    }

    #[test]
    fn over_cap_exact_repetitions_never_pass_through() {
        let guard = SecretGuard::new();
        guard.register(REAL);
        let text = (0..MAX_SCRUB_HITS + 5)
            .map(|_| REAL)
            .collect::<Vec<_>>()
            .join(" ");
        let scrubbed = guard.scrub(&text);
        assert_eq!(scrubbed.matches(REAL).count(), 0, "{scrubbed}");
        assert_eq!(
            scrubbed.matches("<redacted:configured_secret>").count(),
            MAX_SCRUB_HITS
        );
        assert!(scrubbed.contains(REDACTION_TRUNCATION_MARKER), "{scrubbed}");
    }

    #[test]
    fn exact_cap_boundary_is_exact_and_fail_closed() {
        let guard = SecretGuard::new();
        guard.register(REAL);
        let at_cap = (0..MAX_SCRUB_HITS)
            .map(|_| REAL)
            .collect::<Vec<_>>()
            .join(" ");
        let scrubbed = guard.scrub(&at_cap);
        assert_eq!(scrubbed.matches(REAL).count(), 0);
        assert!(
            !scrubbed.contains(REDACTION_TRUNCATION_MARKER),
            "a cap-sized input must keep its tail: {scrubbed}"
        );
        let over = format!("{at_cap} {REAL}");
        let scrubbed = guard.scrub(&over);
        assert_eq!(scrubbed.matches(REAL).count(), 0);
        assert!(scrubbed.contains(REDACTION_TRUNCATION_MARKER), "{scrubbed}");
    }

    #[test]
    fn multibyte_text_at_the_exact_cap_never_panics() {
        let guard = SecretGuard::new();
        guard.register(REAL);
        let mut text = (0..MAX_SCRUB_HITS)
            .map(|i| format!("é{i} {REAL} ü"))
            .collect::<Vec<_>>()
            .join(" ");
        text.push_str(" finale é ");
        text.push_str(REAL);
        let scrubbed = guard.scrub(&text);
        assert!(!scrubbed.contains(REAL), "{scrubbed}");
        assert!(scrubbed.contains(REDACTION_TRUNCATION_MARKER), "{scrubbed}");
    }

    #[test]
    fn under_cap_scrub_preserves_clean_text() {
        let guard = SecretGuard::new();
        guard.register(REAL);
        let text = format!("before {REAL} after words");
        let scrubbed = guard.scrub(&text);
        assert_eq!(scrubbed, "before <redacted:configured_secret> after words");
        assert!(!scrubbed.contains(REDACTION_TRUNCATION_MARKER));
    }

    #[test]
    fn property_no_registered_secret_survives_any_decoy_count() {
        let guard = guard_with_both();
        for decoy_count in [
            0usize,
            1,
            MAX_SCRUB_HITS - 1,
            MAX_SCRUB_HITS,
            MAX_SCRUB_HITS + 1,
            3 * MAX_SCRUB_HITS,
        ] {
            let mut text = (0..decoy_count)
                .map(|i| if i % 2 == 0 { DECOY } else { PATTERN_DECOY })
                .collect::<Vec<_>>()
                .join(" ");
            text.push(' ');
            text.push_str(REAL);
            let scrubbed = guard.scrub(&text);
            assert!(
                !scrubbed.contains(DECOY),
                "decoy_count={decoy_count}: {scrubbed}"
            );
            assert!(
                !scrubbed.contains(REAL),
                "decoy_count={decoy_count}: {scrubbed}"
            );
            assert!(
                !scrubbed.contains(PATTERN_DECOY),
                "decoy_count={decoy_count}: {scrubbed}"
            );
        }
    }
}
