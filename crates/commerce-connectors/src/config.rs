//! Strict connector configuration (spec §13).
//!
//! These structs are the connector-side half of the `commerce.connectors`
//! config section: every field is validated, unknown keys are a startup
//! error, and credentials are **environment variable names** — never values.
//! The daemon's config layer maps `{"commerce": {"connectors": {...}}}`
//! into these types; the crate root documents that seam.
//!
//! Disabled parity (spec §13): a connector whose `enabled` is `false` is
//! never constructed, so it performs no credential resolution, no network
//! request and no client state.

use faktor_commerce::Text;
use serde::Deserialize;

use crate::secrets::CredentialProvider;
use crate::secrets::SecretString;

/// The Mouser connector config (`commerce.connectors.mouser`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MouserConnectorConfig {
    /// Whether the connector is enabled.
    pub enabled: bool,
    /// The environment variable name holding the Search API key.
    pub api_key_env: Option<Text<128>>,
}

impl MouserConnectorConfig {
    /// The API key env var name, required when enabled.
    pub fn require_api_key_env(&self) -> Result<&str, ConfigError> {
        self.api_key_env
            .as_ref()
            .map(Text::as_str)
            .ok_or(ConfigError::MissingEnvName {
                connector: "mouser",
                field: "api_key_env",
            })
    }
}

/// The DigiKey connector config (`commerce.connectors.digikey`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DigiKeyConnectorConfig {
    /// Whether the connector is enabled.
    pub enabled: bool,
    /// The environment variable name holding the OAuth client id.
    pub client_id_env: Option<Text<128>>,
    /// The environment variable name holding the OAuth client secret.
    pub client_secret_env: Option<Text<128>>,
}

impl DigiKeyConnectorConfig {
    /// The client id env var name, required when enabled.
    pub fn require_client_id_env(&self) -> Result<&str, ConfigError> {
        self.client_id_env
            .as_ref()
            .map(Text::as_str)
            .ok_or(ConfigError::MissingEnvName {
                connector: "digikey",
                field: "client_id_env",
            })
    }

    /// The client secret env var name, required when enabled.
    pub fn require_client_secret_env(&self) -> Result<&str, ConfigError> {
        self.client_secret_env
            .as_ref()
            .map(Text::as_str)
            .ok_or(ConfigError::MissingEnvName {
                connector: "digikey",
                field: "client_secret_env",
            })
    }
}

/// The LCSC connector config (`commerce.connectors.lcsc`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LcscConnectorConfig {
    /// Whether the connector is enabled.
    pub enabled: bool,
    /// The environment variable name holding the API key.
    pub api_key_env: Option<Text<128>>,
}

impl LcscConnectorConfig {
    /// The API key env var name, required when enabled.
    pub fn require_api_key_env(&self) -> Result<&str, ConfigError> {
        self.api_key_env
            .as_ref()
            .map(Text::as_str)
            .ok_or(ConfigError::MissingEnvName {
                connector: "lcsc",
                field: "api_key_env",
            })
    }
}

/// The profile-based config shared by the browser-first marketplaces
/// (`commerce.connectors.1688`, `commerce.connectors.alibaba`). The
/// credential itself lives in the browser profile, never here.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileConnectorConfig {
    /// Whether the connector is enabled.
    pub enabled: bool,
    /// The browser profile name, when the connector needs one.
    pub profile: Option<Text<64>>,
}

/// A rejected connector configuration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The connector is disabled by configuration.
    #[error("{connector} connector is disabled")]
    Disabled {
        /// The source id.
        connector: &'static str,
    },
    /// An enabled connector is missing a required env-var-name field.
    #[error("{connector} connector requires {field} to name an environment variable")]
    MissingEnvName {
        /// The source id.
        connector: &'static str,
        /// The config field.
        field: &'static str,
    },
    /// The named environment variable is not set.
    #[error("environment variable {env_name} is not set")]
    MissingCredential {
        /// The variable name (a name, never a value).
        env_name: String,
    },
    /// The named environment variable is empty.
    #[error("environment variable {env_name} is empty")]
    EmptyCredential {
        /// The variable name (a name, never a value).
        env_name: String,
    },
    /// The named environment variable is not valid unicode.
    #[error("environment variable {env_name} is not valid unicode")]
    CredentialNotUnicode {
        /// The variable name (a name, never a value).
        env_name: String,
    },
}

/// Resolve a credential for a named environment variable and register it.
pub(crate) fn credential(
    provider: &dyn CredentialProvider,
    guard: &crate::secrets::SecretGuard,
    env_name: &str,
) -> Result<SecretString, ConfigError> {
    crate::secrets::resolve_registered(provider, guard, env_name)
}
