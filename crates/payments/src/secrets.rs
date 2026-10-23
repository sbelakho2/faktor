//! Secret references, never secret values in configuration. A config names
//! an environment variable; the process resolves it once at construction and
//! the resolved value is redacted from `Debug` and zeroized on drop.

use std::collections::BTreeMap;
use std::fmt;

use zeroize::Zeroize;

/// One resolved secret. `Debug` is always `<redacted>`.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Resolve an environment-variable NAME to its value. The production source
/// is [`EnvSecrets`]; tests inject a static map without touching the process
/// environment (parallel tests must not race on `set_var`).
pub trait SecretSource: Send + Sync {
    fn get(&self, env_name: &str) -> Option<SecretString>;
}

/// The process environment.
#[derive(Debug, Default)]
pub struct EnvSecrets;

impl SecretSource for EnvSecrets {
    fn get(&self, env_name: &str) -> Option<SecretString> {
        std::env::var(env_name).ok().map(SecretString::new)
    }
}

/// A deterministic test source.
#[derive(Debug, Default)]
pub struct StaticSecrets {
    values: BTreeMap<String, String>,
}

impl StaticSecrets {
    pub fn new(values: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            values: values.into_iter().collect(),
        }
    }
}

impl SecretSource for StaticSecrets {
    fn get(&self, env_name: &str) -> Option<SecretString> {
        self.values.get(env_name).cloned().map(SecretString::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_renders_the_secret_value() {
        let secret = SecretString::new("sk_live_do_not_print");
        assert_eq!(format!("{secret:?}"), "<redacted>");
        assert!(!format!("{secret:?}").contains("do_not_print"));
        let source = StaticSecrets::new([("X".into(), "value".into())]);
        assert_eq!(source.get("X").unwrap().expose(), "value");
        assert!(source.get("Y").is_none());
    }
}
