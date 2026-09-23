//! The ONE reusable plaintext-secret wrapper (P0 plaintext-secret fix).
//!
//! [`SecretValue`] wraps a heap secret in [`zeroize::Zeroizing`], so the
//! bytes are wiped when the value drops. It deliberately exposes NO
//! `Display`, `Serialize`/`Deserialize` or `AsRef<str>`: a secret can only
//! leave through the explicit [`SecretValue::expose`] accessor, so every
//! site that turns a secret back into text is visible in review. `Debug` is
//! redacted (`SecretValue([redacted])`), which makes the wrapper safe to
//! embed in derived/custom `Debug` output, logs, panic formatting and
//! diagnostics. Equality is constant-time.
//!
//! This type is the single secret-representation primitive for daemon auth
//! (`ServerPassword`/`AuthToken`) and provider credentials (API keys,
//! extra headers). New code that holds a credential should use it instead
//! of `String`.
//!
//! What it does NOT do: it never claims the surrounding process is
//! leak-free — copies made by callers through [`SecretValue::expose`] (for
//! example a transient `format!`) are the caller's responsibility, and the
//! OS/environment may hold the original. It removes accidental disclosure
//! (Debug/Display/serde/panic dumps) and shortens secret lifetimes in
//! memory.

use std::fmt;

use zeroize::Zeroizing;

/// Why a secret could not be read from the environment. The value (or any
/// part of it) is never carried in the error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretValueError {
    /// The environment variable is not set.
    Missing { var: &'static str },
    /// The environment variable is set but empty/whitespace-only — an empty
    /// secret is no secret at all.
    Empty { var: &'static str },
    /// The environment variable is set to bytes that are not valid UTF-8.
    NotUnicode { var: &'static str },
}

impl fmt::Display for SecretValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretValueError::Missing { var } => {
                write!(f, "environment variable {var} is not set")
            }
            SecretValueError::Empty { var } => {
                write!(f, "environment variable {var} is set but empty")
            }
            SecretValueError::NotUnicode { var } => {
                write!(
                    f,
                    "environment variable {var} is set to non-UTF-8 bytes (value withheld)"
                )
            }
        }
    }
}

impl std::error::Error for SecretValueError {}

/// A wrapped plaintext secret. Zeroized on drop, redacted `Debug`, no
/// `Display`/serde, constant-time equality, explicit [`Self::expose`].
#[derive(Clone)]
pub struct SecretValue(Zeroizing<String>);

impl SecretValue {
    /// Wrap an explicit secret. Callers that take secrets from
    /// configuration should prefer [`SecretValue::try_from_env`] plus
    /// their own strength validation.
    pub fn new(secret: impl Into<String>) -> Self {
        Self(Zeroizing::new(secret.into()))
    }

    /// Read a required secret from the environment. Missing, empty and
    /// non-UTF-8 values are typed errors (never a silent fallback to an
    /// empty or missing credential).
    pub fn try_from_env(var: &'static str) -> Result<Self, SecretValueError> {
        match std::env::var(var) {
            Ok(value) if !value.trim().is_empty() => Ok(Self::new(value)),
            Ok(_) => Err(SecretValueError::Empty { var }),
            Err(std::env::VarError::NotPresent) => Err(SecretValueError::Missing { var }),
            Err(std::env::VarError::NotUnicode(_)) => Err(SecretValueError::NotUnicode { var }),
        }
    }

    /// The explicit escape hatch: the ONLY way to read the plaintext.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Constant-time comparison against a candidate (length mismatch is
    /// `false`; a fixed-length secret does not leak more than its length).
    pub fn ct_eq(&self, candidate: &str) -> bool {
        ct_eq_str(self.expose(), candidate)
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue([redacted])")
    }
}

impl PartialEq for SecretValue {
    fn eq(&self, other: &Self) -> bool {
        self.ct_eq(other.expose())
    }
}

impl Eq for SecretValue {}

impl From<String> for SecretValue {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for SecretValue {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<&String> for SecretValue {
    fn from(value: &String) -> Self {
        Self::new(value)
    }
}

/// Constant-time equality of two byte strings, without pulling in a
/// crypto-dependency: equal length compares every byte; unequal length
/// returns `false` without comparing (length is not secret material for
/// the fixed-length credentials this crate compares).
pub fn ct_eq_str(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recognisable planted secret: every redaction assertion below
    /// searches for this exact marker.
    const PLANTED: &str = "PLANTED-SECRET-do-not-leak-0123456789abcdef";

    #[test]
    fn debug_is_redacted_and_unforgeable_by_derive() {
        let secret = SecretValue::new(PLANTED);
        let debug = format!("{secret:?}");
        assert_eq!(debug, "SecretValue([redacted])");
        assert!(!debug.contains(PLANTED));
        // Nested/derived formatting (Option, Vec, tuple) must stay redacted.
        for rendered in [
            format!("{:?}", Some(&secret)),
            format!("{:?}", vec![secret.clone()]),
            format!("{:?}", (secret.clone(), 1u8)),
        ] {
            assert!(!rendered.contains(PLANTED), "leaked via {rendered}");
        }
    }

    #[test]
    fn panic_style_formatting_never_carries_the_value() {
        // A panic abort/log path formats values with Debug; the payload the
        // panic machinery would print must not contain the secret.
        let secret = SecretValue::new(PLANTED);
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("provider credential {secret:?}")
        }))
        .expect_err("the closure must panic");
        let message = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default();
        assert!(
            !message.contains(PLANTED),
            "panic payload leaked the secret: {message}"
        );
    }

    #[test]
    fn equality_is_value_equality() {
        let a = SecretValue::new(PLANTED);
        let b = SecretValue::new(PLANTED);
        let c = SecretValue::new("other");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.ct_eq(PLANTED));
        assert!(!a.ct_eq(&PLANTED[..PLANTED.len() - 1]));
        assert!(!a.ct_eq(""));
    }

    #[test]
    fn try_from_env_rejects_missing_and_empty_and_reads_present() {
        // A process-wide unique name: no other test can race this one.
        const VAR: &str = "FAKTOR_TEST_SECRET_VALUE_TRAP";
        std::env::remove_var(VAR);
        assert_eq!(
            SecretValue::try_from_env(VAR).unwrap_err(),
            SecretValueError::Missing { var: VAR }
        );
        std::env::set_var(VAR, "");
        assert_eq!(
            SecretValue::try_from_env(VAR).unwrap_err(),
            SecretValueError::Empty { var: VAR }
        );
        std::env::set_var(VAR, "   ");
        assert_eq!(
            SecretValue::try_from_env(VAR).unwrap_err(),
            SecretValueError::Empty { var: VAR }
        );
        std::env::set_var(VAR, PLANTED);
        let secret = SecretValue::try_from_env(VAR).unwrap();
        assert_eq!(secret.expose(), PLANTED);
        std::env::remove_var(VAR);
    }

    #[test]
    fn error_display_never_carries_a_value() {
        // Errors carry only the variable NAME, never bytes read from it.
        let err = SecretValueError::NotUnicode { var: "K" };
        let rendered = format!("{err} {err:?}");
        assert!(!rendered.contains(PLANTED));
    }
}
