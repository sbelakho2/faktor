//! `config::commerce`: schema domain of the daemon config.

#[cfg(test)]
use super::*;

// --------------------------------------------------------------------------
// [commerce] — Faktor Acquire (docs/acquire.md §13, build step 17)
// --------------------------------------------------------------------------

/// The connector ids the section knows; any other name is a startup error.
pub const COMMERCE_CONNECTOR_IDS: &[&str] = &["1688", "alibaba", "lcsc", "mouser", "digikey"];

/// The default commerce database file name (spec §13).
pub const DEFAULT_COMMERCE_DATABASE: &str = "commerce.db";

/// Bound on one configured database file name.
pub const MAX_COMMERCE_DATABASE_BYTES: usize = 128;

/// Bound on one configured environment-variable NAME (never a value).
pub const MAX_COMMERCE_ENV_NAME_BYTES: usize = 128;

/// Bound on one configured browser profile name.
pub const MAX_COMMERCE_PROFILE_BYTES: usize = 64;

/// Bound on one configured browser executable path.
pub const MAX_COMMERCE_EXECUTABLE_BYTES: usize = 4096;

/// Upper bound on any configured cache TTL (365 days).
pub const MAX_COMMERCE_TTL_S: u64 = 31_536_000;

/// One environment-variable NAME (`[A-Za-z_][A-Za-z0-9_]*`). Values are
/// never accepted: an operator who pastes a secret into `*_env` fails
/// startup instead of silently shipping the value.
pub(crate) fn validate_env_name(field: &str, name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_COMMERCE_ENV_NAME_BYTES {
        return Err(format!(
            "commerce: {field} must be 1..={MAX_COMMERCE_ENV_NAME_BYTES} bytes"
        ));
    }
    let mut bytes = name.bytes();
    let first = bytes.next().unwrap_or(0);
    if !(first.is_ascii_alphabetic() || first == b'_')
        || !bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(format!(
            "commerce: {field} {name:?} must be an environment variable NAME \
             ([A-Za-z_][A-Za-z0-9_]*), never a credential value"
        ));
    }
    Ok(())
}

/// One browser profile name (`[a-z0-9_-]{1,64}`, the browser authority's
/// grammar), validated here so a hostile profile never reaches a path join.
pub(crate) fn validate_profile_name(field: &str, name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_COMMERCE_PROFILE_BYTES {
        return Err(format!(
            "commerce: {field} must be 1..={MAX_COMMERCE_PROFILE_BYTES} bytes"
        ));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return Err(format!("commerce: {field} {name:?} must match [a-z0-9_-]+"));
    }
    Ok(())
}

/// Normalize a present-but-disabled connector subsection away: the resolved
/// config of `{enabled: false}` is byte-identically the absent key
/// (disabled parity).
pub(crate) trait ConnectorEnabled {
    fn connector_enabled(&self) -> bool;
}

pub(crate) fn deserialize_enabled_connector<'de, D, T>(de: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de> + ConnectorEnabled,
{
    let raw = <Option<T> as serde::Deserialize>::deserialize(de)?;
    Ok(raw.filter(ConnectorEnabled::connector_enabled))
}

/// `commerce.connectors.<source>` for the API-key connectors (LCSC,
/// Mouser): the key is referenced by environment-variable name only.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommerceApiConnectorCfg {
    #[serde(default)]
    pub enabled: bool,
    /// The environment variable name holding the API key.
    #[serde(default)]
    pub api_key_env: Option<String>,
}

impl ConnectorEnabled for CommerceApiConnectorCfg {
    fn connector_enabled(&self) -> bool {
        self.enabled
    }
}

impl std::fmt::Debug for CommerceApiConnectorCfg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommerceApiConnectorCfg")
            .field("enabled", &self.enabled)
            // Env-var NAMES are operator config, but Debug output is copied
            // into logs/traces; anything credential-shaped stays redacted.
            .field(
                "api_key_env",
                &self.api_key_env.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// `commerce.connectors.digikey`: the OAuth client id + secret are
/// referenced by environment-variable names only.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommerceDigikeyConnectorCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub client_id_env: Option<String>,
    #[serde(default)]
    pub client_secret_env: Option<String>,
}

impl ConnectorEnabled for CommerceDigikeyConnectorCfg {
    fn connector_enabled(&self) -> bool {
        self.enabled
    }
}

impl std::fmt::Debug for CommerceDigikeyConnectorCfg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommerceDigikeyConnectorCfg")
            .field("enabled", &self.enabled)
            .field(
                "client_id_env",
                &self.client_id_env.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "client_secret_env",
                &self.client_secret_env.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// `commerce.connectors.<source>` profile shape: a browser profile plus the
/// connector's immutable commercial identity. The credential lives inside
/// the browser profile, never in this config; `account_scope`, `market` and
/// `locale` are injected at registration and are never taken from a tool
/// request.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct CommerceProfileConnectorCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub profile: Option<String>,
    /// The account scope every acquisition of this connector belongs to.
    #[serde(default)]
    pub account_scope: Option<String>,
    /// The market every acquisition of this connector is scoped to.
    #[serde(default)]
    pub market: Option<String>,
    /// The locale every acquisition of this connector is scoped to.
    #[serde(default)]
    pub locale: Option<String>,
}

impl ConnectorEnabled for CommerceProfileConnectorCfg {
    fn connector_enabled(&self) -> bool {
        self.enabled
    }
}

impl CommerceProfileConnectorCfg {
    /// The configured browser profile name.
    pub fn profile_name(&self) -> Option<&str> {
        self.profile.as_deref()
    }

    /// The configured account scope.
    pub fn account_scope(&self) -> Option<&str> {
        self.account_scope.as_deref()
    }

    /// The configured market.
    pub fn market(&self) -> Option<&str> {
        self.market.as_deref()
    }

    /// The configured locale.
    pub fn locale(&self) -> Option<&str> {
        self.locale.as_deref()
    }
}

/// `commerce.connectors.<source>.api` for the marketplace connectors: the
/// Open Platform / Open API credential. Every field is an environment
/// variable NAME — the config never carries a value, and `Debug` redacts
/// each configured name.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct MarketplaceApiCfg {
    /// The environment variable naming the app key (required when `api` is
    /// present).
    #[serde(default)]
    pub app_key_env: Option<String>,
    /// The environment variable naming the app secret, when the credential
    /// has one.
    #[serde(default)]
    pub app_secret_env: Option<String>,
    /// The environment variable naming a pre-authorized access token, when
    /// the operator stages one instead of interactive authorization. The
    /// connector requires the token's absolute expiry alongside it, so
    /// `access_token_expires_at_env` is required whenever this is set.
    #[serde(default)]
    pub access_token_env: Option<String>,
    /// The environment variable naming the access token's absolute expiry
    /// (unix milliseconds). Only meaningful with `access_token_env`.
    #[serde(default)]
    pub access_token_expires_at_env: Option<String>,
    /// The granted scope labels (source-specific; validated at startup).
    #[serde(default)]
    pub scopes: Vec<String>,
}

impl std::fmt::Debug for MarketplaceApiCfg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MarketplaceApiCfg")
            .field(
                "app_key_env",
                &self.app_key_env.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "app_secret_env",
                &self.app_secret_env.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "access_token_env",
                &self.access_token_env.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "access_token_expires_at_env",
                &self
                    .access_token_expires_at_env
                    .as_ref()
                    .map(|_| "<redacted>"),
            )
            .field("scopes", &self.scopes)
            .finish()
    }
}

impl MarketplaceApiCfg {
    /// The environment variable names of this credential (never values).
    pub fn env_names(&self) -> Vec<&str> {
        let mut names = Vec::new();
        for name in [
            self.app_key_env.as_deref(),
            self.app_secret_env.as_deref(),
            self.access_token_env.as_deref(),
            self.access_token_expires_at_env.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            names.push(name);
        }
        names
    }
}

/// `commerce.connectors.1688` / `commerce.connectors.alibaba` API-capable
/// shape: the browser profile plus the optional Open Platform / Open API
/// credential. At least one path must be usable.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct CommerceMarketplaceConnectorCfg {
    #[serde(default)]
    pub enabled: bool,
    /// The browser profile name; absent keeps the source's default profile
    /// when the browser path is enabled.
    #[serde(default)]
    pub browser_profile: Option<String>,
    /// The account scope every acquisition of this connector belongs to.
    #[serde(default)]
    pub account_scope: Option<String>,
    /// The optional Open Platform / Open API credential.
    #[serde(default)]
    pub api: Option<MarketplaceApiCfg>,
}

impl ConnectorEnabled for CommerceMarketplaceConnectorCfg {
    fn connector_enabled(&self) -> bool {
        self.enabled
    }
}

impl CommerceMarketplaceConnectorCfg {
    /// The configured browser profile name.
    pub fn browser_profile(&self) -> Option<&str> {
        self.browser_profile.as_deref()
    }

    /// The configured account scope.
    pub fn account_scope(&self) -> Option<&str> {
        self.account_scope.as_deref()
    }

    /// The configured API credential, when present.
    pub fn api(&self) -> Option<&MarketplaceApiCfg> {
        self.api.as_ref()
    }
}

/// One configured 1688/alibaba entry: either the profile-only shape or the
/// API-capable marketplace shape. Both are strict (`deny_unknown_fields`
/// inside each struct); mixing the two shapes' keys is refused with a typed
/// message instead of silently dropping one half.
#[derive(Clone, PartialEq, Eq, serde::Serialize, Debug)]
#[serde(untagged)]
pub enum CommerceMarketplaceConnectorEntry {
    /// The browser-profile-only shape.
    Profile(CommerceProfileConnectorCfg),
    /// The browser profile + optional Open Platform/Open API shape.
    Marketplace(CommerceMarketplaceConnectorCfg),
}

/// Every key either shape accepts (unknown-field errors list them).
pub const MARKETPLACE_CONNECTOR_FIELDS: &[&str] = &[
    "enabled",
    "profile",
    "browser_profile",
    "account_scope",
    "market",
    "locale",
    "api",
];

impl CommerceMarketplaceConnectorEntry {
    /// Whether the configured entry is enabled.
    pub fn enabled(&self) -> bool {
        match self {
            Self::Profile(cfg) => cfg.enabled,
            Self::Marketplace(cfg) => cfg.enabled,
        }
    }

    /// The configured browser profile name, whatever the shape.
    pub fn browser_profile(&self) -> Option<&str> {
        match self {
            Self::Profile(cfg) => cfg.profile_name(),
            Self::Marketplace(cfg) => cfg.browser_profile(),
        }
    }

    /// The configured account scope identity, whatever the shape.
    pub fn account_scope(&self) -> Option<&str> {
        match self {
            Self::Profile(cfg) => cfg.account_scope(),
            Self::Marketplace(cfg) => cfg.account_scope(),
        }
    }

    /// The configured market identity (marketplace shape has none).
    pub fn market(&self) -> Option<&str> {
        match self {
            Self::Profile(cfg) => cfg.market(),
            Self::Marketplace(_) => None,
        }
    }

    /// The configured locale identity (marketplace shape has none).
    pub fn locale(&self) -> Option<&str> {
        match self {
            Self::Profile(cfg) => cfg.locale(),
            Self::Marketplace(_) => None,
        }
    }

    /// The configured API credential, when the marketplace shape carries one.
    pub fn api(&self) -> Option<&MarketplaceApiCfg> {
        match self {
            Self::Profile(_) => None,
            Self::Marketplace(cfg) => cfg.api(),
        }
    }
}

/// Strict, map-only parsing of one 1688/alibaba entry: unknown keys are named
/// precisely (`unknown field`) and the two shapes never mix.
impl<'de> serde::Deserialize<'de> for CommerceMarketplaceConnectorEntry {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error as _, MapAccess, Visitor};

        struct EntryVisitor;

        impl<'de> Visitor<'de> for EntryVisitor {
            type Value = CommerceMarketplaceConnectorEntry;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("the commerce.connectors.<source> section as a JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut values = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !MARKETPLACE_CONNECTOR_FIELDS.contains(&key.as_str()) {
                        return Err(A::Error::unknown_field(&key, MARKETPLACE_CONNECTOR_FIELDS));
                    }
                    if values.contains_key(&key) {
                        return Err(A::Error::custom(format!(
                            "duplicate field `{key}` in a commerce connector section"
                        )));
                    }
                    values.insert(key, map.next_value::<serde_json::Value>()?);
                }
                let profile_shape = ["profile", "market", "locale"]
                    .iter()
                    .any(|key| values.contains_key(*key));
                if profile_shape {
                    for marketplace_only in ["browser_profile", "api"] {
                        if values.contains_key(marketplace_only) {
                            return Err(A::Error::custom(format!(
                                "commerce connector config mixes the profile and marketplace \
                                 shapes: `{marketplace_only}` is not accepted with \
                                 `profile`/`market`/`locale`"
                            )));
                        }
                    }
                    serde_json::from_value::<CommerceProfileConnectorCfg>(
                        serde_json::Value::Object(values),
                    )
                    .map(CommerceMarketplaceConnectorEntry::Profile)
                    .map_err(A::Error::custom)
                } else {
                    serde_json::from_value::<CommerceMarketplaceConnectorCfg>(
                        serde_json::Value::Object(values),
                    )
                    .map(CommerceMarketplaceConnectorEntry::Marketplace)
                    .map_err(A::Error::custom)
                }
            }
        }

        de.deserialize_map(EntryVisitor)
    }
}

pub(crate) fn deserialize_enabled_marketplace<'de, D>(
    de: D,
) -> Result<Option<CommerceMarketplaceConnectorEntry>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = <Option<CommerceMarketplaceConnectorEntry> as serde::Deserialize>::deserialize(de)?;
    Ok(raw.filter(CommerceMarketplaceConnectorEntry::enabled))
}

/// The `commerce.connectors` map: exactly the five known source ids; any
/// other key is a startup error. A present-but-disabled connector resolves
/// to `None` (disabled parity).
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct CommerceConnectorsCfg {
    #[serde(rename = "1688", deserialize_with = "deserialize_enabled_marketplace")]
    pub china1688: Option<CommerceMarketplaceConnectorEntry>,
    #[serde(deserialize_with = "deserialize_enabled_marketplace")]
    pub alibaba: Option<CommerceMarketplaceConnectorEntry>,
    #[serde(deserialize_with = "deserialize_enabled_connector")]
    pub lcsc: Option<CommerceApiConnectorCfg>,
    #[serde(deserialize_with = "deserialize_enabled_connector")]
    pub mouser: Option<CommerceApiConnectorCfg>,
    #[serde(deserialize_with = "deserialize_enabled_connector")]
    pub digikey: Option<CommerceDigikeyConnectorCfg>,
}

impl CommerceConnectorsCfg {
    /// Every enabled connector as `(source id, credential requirement)`,
    /// sorted by source id.
    pub fn enabled(&self) -> Vec<(&'static str, ConnectorCredential<'_>)> {
        let mut rows: Vec<(&'static str, ConnectorCredential<'_>)> = Vec::new();
        if let Some(cfg) = &self.china1688 {
            rows.push(("1688", marketplace_credential(cfg)));
        }
        if let Some(cfg) = &self.alibaba {
            rows.push(("alibaba", marketplace_credential(cfg)));
        }
        if let Some(cfg) = &self.lcsc {
            rows.push((
                "lcsc",
                ConnectorCredential::ApiKey(cfg.api_key_env.as_deref()),
            ));
        }
        if let Some(cfg) = &self.mouser {
            rows.push((
                "mouser",
                ConnectorCredential::ApiKey(cfg.api_key_env.as_deref()),
            ));
        }
        if let Some(cfg) = &self.digikey {
            rows.push((
                "digikey",
                ConnectorCredential::OAuthPair(
                    cfg.client_id_env.as_deref(),
                    cfg.client_secret_env.as_deref(),
                ),
            ));
        }
        rows
    }
}

pub(crate) fn marketplace_credential(
    cfg: &CommerceMarketplaceConnectorEntry,
) -> ConnectorCredential<'_> {
    match cfg {
        CommerceMarketplaceConnectorEntry::Profile(profile) => {
            ConnectorCredential::Profile(profile.profile_name())
        }
        CommerceMarketplaceConnectorEntry::Marketplace(marketplace) => {
            ConnectorCredential::Marketplace {
                browser_profile: marketplace.browser_profile(),
                api: marketplace.api().map(|api| MarketplaceApiEnvNames {
                    app_key_env: api.app_key_env.as_deref(),
                    app_secret_env: api.app_secret_env.as_deref(),
                    access_token_env: api.access_token_env.as_deref(),
                    access_token_expires_at_env: api.access_token_expires_at_env.as_deref(),
                }),
            }
        }
    }
}

/// The env-var NAMES of one marketplace API credential (never values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarketplaceApiEnvNames<'a> {
    pub app_key_env: Option<&'a str>,
    pub app_secret_env: Option<&'a str>,
    pub access_token_env: Option<&'a str>,
    pub access_token_expires_at_env: Option<&'a str>,
}

impl MarketplaceApiEnvNames<'_> {
    /// Every configured env-var name.
    pub fn all(&self) -> Vec<&str> {
        [
            self.app_key_env,
            self.app_secret_env,
            self.access_token_env,
            self.access_token_expires_at_env,
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

/// The credential requirement of one enabled connector: env-var NAMES (or a
/// profile), never values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorCredential<'a> {
    /// A profile-based browser connector.
    Profile(Option<&'a str>),
    /// A 1688/alibaba marketplace connector: an optional browser profile plus
    /// an optional Open Platform / Open API credential.
    Marketplace {
        /// The configured browser profile, when the browser path is used.
        browser_profile: Option<&'a str>,
        /// The configured API credential env-var names, when present.
        api: Option<MarketplaceApiEnvNames<'a>>,
    },
    /// A single API key env var name.
    ApiKey(Option<&'a str>),
    /// An OAuth client id + secret env var name pair.
    OAuthPair(Option<&'a str>, Option<&'a str>),
}

/// The `commerce.cache` field-level TTLs (seconds; spec §13).
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct CommerceCacheCfg {
    #[serde(default = "default_commerce_discovery_ttl_s")]
    pub discovery_ttl_s: u64,
    #[serde(default = "default_commerce_product_ttl_s")]
    pub product_ttl_s: u64,
    #[serde(default = "default_commerce_price_ttl_s")]
    pub price_ttl_s: u64,
    #[serde(default = "default_commerce_stock_ttl_s")]
    pub stock_ttl_s: u64,
    #[serde(default = "default_commerce_supplier_ttl_s")]
    pub supplier_ttl_s: u64,
}

pub(crate) fn default_commerce_discovery_ttl_s() -> u64 {
    1800
}

pub(crate) fn default_commerce_product_ttl_s() -> u64 {
    21_600
}

pub(crate) fn default_commerce_price_ttl_s() -> u64 {
    1800
}

pub(crate) fn default_commerce_stock_ttl_s() -> u64 {
    900
}

pub(crate) fn default_commerce_supplier_ttl_s() -> u64 {
    86_400
}

impl Default for CommerceCacheCfg {
    fn default() -> Self {
        Self {
            discovery_ttl_s: default_commerce_discovery_ttl_s(),
            product_ttl_s: default_commerce_product_ttl_s(),
            price_ttl_s: default_commerce_price_ttl_s(),
            stock_ttl_s: default_commerce_stock_ttl_s(),
            supplier_ttl_s: default_commerce_supplier_ttl_s(),
        }
    }
}

impl CommerceCacheCfg {
    pub(crate) fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("discovery_ttl_s", self.discovery_ttl_s),
            ("product_ttl_s", self.product_ttl_s),
            ("price_ttl_s", self.price_ttl_s),
            ("stock_ttl_s", self.stock_ttl_s),
            ("supplier_ttl_s", self.supplier_ttl_s),
        ] {
            if value == 0 || value > MAX_COMMERCE_TTL_S {
                return Err(format!(
                    "commerce cache: {name} must be 1..={MAX_COMMERCE_TTL_S} seconds"
                ));
            }
        }
        Ok(())
    }
}

/// The `commerce.browser` block (spec §13): lazy Chromium startup, headed
/// only for interactive login, idle shutdown and hard page/browser bounds.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct CommerceBrowserCfg {
    /// Master browser switch. Default `false`: enabling it is an explicit
    /// operator decision (browser acquisition is the last acquisition
    /// path).
    #[serde(default)]
    pub enabled: bool,
    /// Explicit Chromium executable path; absent = resolve from PATH.
    #[serde(default)]
    pub executable: Option<String>,
    /// Headless acquisition. Interactive `commerce login` always overrides
    /// this to a headed window.
    #[serde(default = "default_commerce_browser_headless")]
    pub headless: bool,
    /// Kill an unused browser after this many seconds.
    #[serde(default = "default_commerce_browser_idle_s")]
    pub idle_shutdown_s: u64,
    /// Maximum simultaneously live browser children.
    #[serde(default = "default_commerce_browser_max")]
    pub max_browsers: usize,
    /// Maximum simultaneously open pages per profile.
    #[serde(default = "default_commerce_browser_pages")]
    pub max_pages_per_profile: usize,
}

pub(crate) fn default_commerce_browser_headless() -> bool {
    true
}

pub(crate) fn default_commerce_browser_idle_s() -> u64 {
    300
}

pub(crate) fn default_commerce_browser_max() -> usize {
    2
}

pub(crate) fn default_commerce_browser_pages() -> usize {
    1
}

impl Default for CommerceBrowserCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            executable: None,
            headless: default_commerce_browser_headless(),
            idle_shutdown_s: default_commerce_browser_idle_s(),
            max_browsers: default_commerce_browser_max(),
            max_pages_per_profile: default_commerce_browser_pages(),
        }
    }
}

impl CommerceBrowserCfg {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if !(1..=86_400).contains(&self.idle_shutdown_s) {
            return Err("commerce browser: idle_shutdown_s must be 1..=86400".into());
        }
        if !(1..=16).contains(&self.max_browsers) {
            return Err("commerce browser: max_browsers must be 1..=16".into());
        }
        if !(1..=8).contains(&self.max_pages_per_profile) {
            return Err("commerce browser: max_pages_per_profile must be 1..=8".into());
        }
        if let Some(executable) = &self.executable {
            if executable.is_empty() || executable.len() > MAX_COMMERCE_EXECUTABLE_BYTES {
                return Err(format!(
                    "commerce browser: executable must be 1..={MAX_COMMERCE_EXECUTABLE_BYTES} bytes"
                ));
            }
            if executable.bytes().any(|b| b.is_ascii_control()) {
                return Err("commerce browser: executable contains control characters".into());
            }
        }
        Ok(())
    }
}

/// The additive `[commerce]` section (docs/acquire.md §13).
///
/// Strict by construction: unknown keys anywhere in the section are startup
/// errors (the derived `deny_unknown_fields` on every nested block plus the
/// fixed connector-id map), non-matching value types are type errors, and
/// [`CommerceCfg::validate`] enforces the semantic bounds. Credentials are
/// referenced by environment-variable NAME only; the section never carries
/// a value and `Debug` redacts every credential-shaped field.
///
/// Disabled parity (spec §13): absent, `{}` and `{enabled: false}` resolve
/// byte-identically — no commerce tool registered, no database created, no
/// connector client, browser profile or broker state, no network request
/// and no schema tokens.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct CommerceCfg {
    /// Whether Faktor Acquire is enabled at all.
    pub enabled: bool,
    /// The commerce database file name. Only the default `commerce.db` is
    /// supported by the commerce store today; a different name is refused at
    /// validation instead of being silently ignored.
    pub database: Option<String>,
    /// Field-level cache TTLs.
    pub cache: CommerceCacheCfg,
    /// Browser acquisition bounds.
    pub browser: CommerceBrowserCfg,
    /// The per-source connector configuration.
    pub connectors: CommerceConnectorsCfg,
}

impl Default for CommerceCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            database: Some(DEFAULT_COMMERCE_DATABASE.to_string()),
            cache: CommerceCacheCfg::default(),
            browser: CommerceBrowserCfg::default(),
            connectors: CommerceConnectorsCfg::default(),
        }
    }
}

impl CommerceCfg {
    /// The resolved database file name (default when absent).
    pub fn database_name(&self) -> &str {
        self.database
            .as_deref()
            .unwrap_or(DEFAULT_COMMERCE_DATABASE)
    }

    /// Strict validation: database name, TTL bounds, browser bounds and the
    /// enabled connectors' credential requirements. A nested enabled section
    /// under `enabled: false` is refused (the daemon never boots a
    /// half-configured commerce surface).
    pub fn validate(&self) -> Result<(), String> {
        let database = self.database_name();
        if database.is_empty()
            || database.len() > MAX_COMMERCE_DATABASE_BYTES
            || database.contains('/')
            || database.contains('\\')
            || database.contains("..")
            || database.contains(':')
            || database.bytes().any(|b| b.is_ascii_control())
        {
            return Err(format!(
                "commerce: database {database:?} must be a plain file name (no paths, no traversal)"
            ));
        }
        if database != DEFAULT_COMMERCE_DATABASE {
            return Err(format!(
                "commerce: database {database:?} is not supported; the commerce store owns \
                 {DEFAULT_COMMERCE_DATABASE:?}"
            ));
        }
        self.cache.validate()?;
        self.browser.validate()?;
        let enabled = self.connectors.enabled();
        if !enabled.is_empty() && !self.enabled {
            return Err("commerce: an enabled connector requires [commerce] enabled = true".into());
        }
        if self.browser.enabled && !self.enabled {
            return Err(
                "commerce: an enabled browser block requires [commerce] enabled = true".into(),
            );
        }
        for (source, credential) in enabled {
            match credential {
                ConnectorCredential::Profile(profile) => {
                    let Some(profile) = profile else {
                        return Err(format!(
                            "commerce connectors: {source} requires a browser profile name"
                        ));
                    };
                    validate_profile_name(&format!("connectors.{source}.profile"), profile)?;
                }
                ConnectorCredential::Marketplace {
                    browser_profile,
                    api,
                } => {
                    if let Some(profile) = browser_profile {
                        validate_profile_name(
                            &format!("connectors.{source}.browser_profile"),
                            profile,
                        )?;
                    }
                    if !self.browser.enabled && api.is_none() {
                        return Err(format!(
                            "commerce connectors: {source} enables neither the browser path \
                             (browser disabled) nor an `api` credential"
                        ));
                    }
                    let Some(api) = api else { continue };
                    let Some(app_key_env) = api.app_key_env else {
                        return Err(format!(
                            "commerce connectors: {source}.api requires app_key_env to name an \
                             environment variable"
                        ));
                    };
                    let Some(app_secret_env) = api.app_secret_env else {
                        return Err(format!(
                            "commerce connectors: {source}.api requires app_secret_env to name \
                             an environment variable"
                        ));
                    };
                    validate_env_name(
                        &format!("connectors.{source}.api.app_key_env"),
                        app_key_env,
                    )?;
                    validate_env_name(
                        &format!("connectors.{source}.api.app_secret_env"),
                        app_secret_env,
                    )?;
                    for (field, name) in [
                        ("access_token_env", api.access_token_env),
                        (
                            "access_token_expires_at_env",
                            api.access_token_expires_at_env,
                        ),
                    ] {
                        if let Some(name) = name {
                            validate_env_name(&format!("connectors.{source}.api.{field}"), name)?;
                        }
                    }
                    if api.access_token_env.is_some() && api.access_token_expires_at_env.is_none() {
                        return Err(format!(
                            "commerce connectors: {source}.api requires \
                             access_token_expires_at_env alongside access_token_env"
                        ));
                    }
                    if api.access_token_expires_at_env.is_some() && api.access_token_env.is_none() {
                        return Err(format!(
                            "commerce connectors: {source}.api requires access_token_env \
                             alongside access_token_expires_at_env"
                        ));
                    }
                }
                ConnectorCredential::ApiKey(api_key_env) => {
                    let Some(api_key_env) = api_key_env else {
                        return Err(format!(
                            "commerce connectors: {source} requires api_key_env to name an \
                             environment variable"
                        ));
                    };
                    validate_env_name(&format!("connectors.{source}.api_key_env"), api_key_env)?;
                }
                ConnectorCredential::OAuthPair(client_id, client_secret) => {
                    let (Some(client_id), Some(client_secret)) = (client_id, client_secret) else {
                        return Err(format!(
                            "commerce connectors: {source} requires client_id_env and \
                             client_secret_env to name environment variables"
                        ));
                    };
                    validate_env_name(&format!("connectors.{source}.client_id_env"), client_id)?;
                    validate_env_name(
                        &format!("connectors.{source}.client_secret_env"),
                        client_secret,
                    )?;
                }
            }
        }
        // Identity scopes and API scope labels are validated per source, with
        // the source's own known scope vocabulary.
        for (source, entry) in [
            ("1688", self.connectors.china1688.as_ref()),
            ("alibaba", self.connectors.alibaba.as_ref()),
        ] {
            let Some(entry) = entry else { continue };
            for (field, value) in [
                ("account_scope", entry.account_scope()),
                ("market", entry.market()),
                ("locale", entry.locale()),
            ] {
                if let Some(value) = value {
                    validate_identity_value(&format!("connectors.{source}.{field}"), value)?;
                }
            }
            if let Some(api) = entry.api() {
                if api.scopes.is_empty() {
                    return Err(format!(
                        "commerce connectors: {source}.api requires at least one scope from {}{}",
                        marketplace_scope_labels(source).join(", "),
                        if marketplace_scope_labels(source).is_empty() {
                            " (no api scopes are defined for this source)"
                        } else {
                            ""
                        }
                    ));
                }
                let known = marketplace_scope_labels(source);
                if known.is_empty() {
                    return Err(format!(
                        "commerce connectors: {source} does not support an `api` credential"
                    ));
                }
                let mut seen: Vec<&str> = Vec::new();
                for scope in &api.scopes {
                    if !known.contains(&scope.as_str()) {
                        return Err(format!(
                            "commerce connectors: {source}.api scope {scope:?} is unknown \
                             (known: {})",
                            known.join(", ")
                        ));
                    }
                    if seen.contains(&scope.as_str()) {
                        return Err(format!(
                            "commerce connectors: {source}.api scope {scope:?} is duplicated"
                        ));
                    }
                    seen.push(scope.as_str());
                }
            }
        }
        Ok(())
    }
}

/// The scope labels the marketplace APIs accept per source.
pub fn marketplace_scope_labels(source: &str) -> &'static [&'static str] {
    match source {
        "1688" => &[
            "discovery",
            "product",
            "price",
            "stock",
            "supplier",
            "variants",
            "moq",
        ],
        "alibaba" => &[
            "seller_product",
            "buyer_discovery",
            "trade_terms",
            "supplier_profile",
        ],
        _ => &[],
    }
}

/// Validate one identity field through the commerce domain's own bounded text
/// type: bounded, non-empty, control-character-free.
pub(crate) fn validate_identity_value(field: &str, value: &str) -> Result<(), String> {
    faktor_commerce::Text::<{ faktor_commerce::MAX_ACCOUNT_SCOPE_BYTES }>::new(value)
        .map_err(|error| format!("commerce connectors: {field} is invalid: {error}"))?;
    Ok(())
}

#[cfg(test)]
#[path = "commerce_config_tests.rs"]
mod commerce_config_tests;
