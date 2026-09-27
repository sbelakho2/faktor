//! `config::cloud`: schema domain of the daemon config.

#![allow(unused_imports)]

use super::*;

/// The additive `[cloud]` section: the commercial control plane (identity,
/// organizations, RBAC, synced SCM repositories).
///
/// Strict and additive:
///
/// - `enabled` (default `false`): when false — the default and the ONLY
///   value for every pre-existing config — the daemon builds NO control
///   plane and NO SCM store, creates no database file, and every
///   `/native/identity`, `/native/orgs`, `/native/repositories` and
///   `/native/approvals` request answers a typed 409 `cloud_disabled`.
///   The local daemon is byte-identical to the pre-cloud daemon;
/// - `database` / `scm_database`: optional simple FILE NAMES (relative to
///   the daemon data dir; defaults `control-plane.db` and `scm.db`).
///   Absolute paths, separators and `..` traversal are refused: the
///   control-plane state lives inside the daemon's own data directory;
/// - `payload_dir`: the operator-staged payload directory (absolute or
///   relative to the data dir; default `payloads`). SSO client secrets and
///   the GitHub App private key/webhook secret are loaded from it under the
///   strict contract of [`crate::payload`]: plain file names only, regular
///   files only, 0600-style modes on unix, bounded and strictly parsed —
///   a referenced-but-missing/corrupt/too-permissive payload refuses
///   startup (fail closed, never a half-wired cloud surface);
/// - `[cloud.sso]`: the network OIDC adapter wiring (issuer, client id and
///   the optional operator-staged client secret plus its strict
///   `client_secret_post`/`client_secret_basic` method);
/// - `[cloud.github_app]`: the GitHub App wiring (app id, staged private
///   key and webhook secret, api base, tenant organization). When enabled
///   the daemon builds the real adapter/token source/webhook inbox/sync,
///   runs one bounded initial sync after readiness and re-syncs on webhook
///   delivery; the optional `[cloud.github_app.reconcile]` sub-section adds
///   the bounded periodic re-sync timer (disabled = webhook parity);
///   `/native/scm/webhook` dispatches into the wired inbox;
/// - unknown keys, duplicates, non-object shapes and wrong value types are
///   parse errors.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Default)]
pub struct CloudCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub scm_database: Option<String>,
    #[serde(default)]
    pub payload_dir: Option<String>,
    #[serde(default)]
    pub sso: Option<CloudSsoCfg>,
    #[serde(default)]
    pub github_app: Option<CloudGithubAppCfg>,
}

/// The `[cloud.sso]` section: the daemon-wide network OIDC adapter wiring.
/// Disabled by default; while disabled (or absent) `/native/sso/*` answers
/// a typed 409 `sso_disabled` and no adapter is built.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CloudSsoCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub issuer: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    /// The payload NAME (inside `[cloud] payload_dir`) of the confidential
    /// client's secret. Optional: absent = the public PKCE client path. The
    /// Rust field carries a NAME (not the secret itself), hence the
    /// `_payload` suffix; the config wire key is `client_secret`.
    #[serde(default, rename = "client_secret")]
    pub client_secret_payload: Option<String>,
    /// How the confidential client authenticates at the token endpoint:
    /// `client_secret_post` (the default when a secret is staged) or
    /// `client_secret_basic`. Only meaningful together with `client_secret`;
    /// a method without a secret, or an unknown method, is refused at load.
    #[serde(default)]
    pub client_secret_method: Option<String>,
    #[serde(default)]
    pub discovery_max_age_ms: Option<i64>,
    #[serde(default)]
    pub jwks_max_age_ms: Option<i64>,
    #[serde(default)]
    pub max_jwks_refetches: Option<u32>,
    /// The ID-token signing algorithms this deployment explicitly allows (the
    /// header `alg` must be in the intersection of this list, the discovery
    /// document's advertised set and the selected JWK's own constraints).
    /// Absent = the adapter default `["RS256"]`; `none` and unsupported names
    /// are refused at load, and symmetric `HS*` is honored only when listed
    /// here AND the JWK is an `oct` key with `use = "sig"`.
    #[serde(default)]
    pub allowed_algorithms: Option<Vec<String>>,
}

/// The `[cloud]` keys, in stable order (unknown-field errors list them).
pub const CLOUD_FIELDS: &[&str] = &[
    "enabled",
    "database",
    "scm_database",
    "payload_dir",
    "sso",
    "github_app",
];

/// The default payload directory name under the daemon data dir.
pub const DEFAULT_PAYLOAD_DIR: &str = "payloads";

/// Bound on one configured payload root.
pub const MAX_PAYLOAD_ROOT_BYTES: usize = 1024;

/// The default control-plane database file name.
pub const DEFAULT_CLOUD_DATABASE: &str = "control-plane.db";

/// Bound on one configured database file name.
pub const MAX_CLOUD_DATABASE_BYTES: usize = 128;

impl<'de> serde::Deserialize<'de> for CloudCfg {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error as _, MapAccess, Visitor};

        struct SectionVisitor;

        impl<'de> Visitor<'de> for SectionVisitor {
            type Value = CloudCfg;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("the [cloud] section as a JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<CloudCfg, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut out = CloudCfg::default();
                let mut seen: u8 = 0;
                while let Some(key) = map.next_key::<String>()? {
                    let (bit, name) = match key.as_str() {
                        "enabled" => (1u8, "enabled"),
                        "database" => (2, "database"),
                        "scm_database" => (4, "scm_database"),
                        "payload_dir" => (8, "payload_dir"),
                        "sso" => (16, "sso"),
                        "github_app" => (32, "github_app"),
                        other => return Err(A::Error::unknown_field(other, CLOUD_FIELDS)),
                    };
                    if seen & bit != 0 {
                        return Err(A::Error::duplicate_field(name));
                    }
                    seen |= bit;
                    match bit {
                        1 => out.enabled = map.next_value::<bool>()?,
                        2 => out.database = map.next_value::<Option<String>>()?,
                        4 => out.scm_database = map.next_value::<Option<String>>()?,
                        8 => out.payload_dir = map.next_value::<Option<String>>()?,
                        16 => out.sso = map.next_value::<Option<CloudSsoCfg>>()?,
                        _ => out.github_app = map.next_value::<Option<CloudGithubAppCfg>>()?,
                    }
                }
                // A disabled sub-section is normalized away: the resolved
                // config of `{enabled: false}` is the resolved config of the
                // absent section (disabled parity, byte-for-byte).
                if out.sso.as_ref().is_some_and(|sso| !sso.enabled) {
                    out.sso = None;
                }
                if out.github_app.as_ref().is_some_and(|app| !app.enabled) {
                    out.github_app = None;
                }
                Ok(out)
            }
        }

        de.deserialize_map(SectionVisitor)
    }
}

impl CloudCfg {
    /// Validate one configured database file name: a bounded simple file
    /// name (no directory separators, no `..`, not absolute, no control
    /// characters). The control-plane state never escapes the data dir.
    pub fn validate_database_name(kind: &str, name: &str) -> Result<(), String> {
        if name.is_empty() || name.len() > MAX_CLOUD_DATABASE_BYTES {
            return Err(format!(
                "cloud: {kind} must be 1..={MAX_CLOUD_DATABASE_BYTES} bytes"
            ));
        }
        if name.contains('/') || name.contains('\\') || name.contains("..") || name.contains(':') {
            return Err(format!(
                "cloud: {kind} {name:?} must be a plain file name (no paths, no traversal)"
            ));
        }
        if name.bytes().any(|b| b.is_ascii_control()) {
            return Err(format!(
                "cloud: {kind} {name:?} contains control characters"
            ));
        }
        if name == "." || name == ".." {
            return Err(format!("cloud: {kind} {name:?} is not a file name"));
        }
        Ok(())
    }

    /// Validate one configured payload root: bounded ASCII, no control
    /// characters, no `..` traversal. Absolute paths are allowed; relative
    /// paths live under the daemon data dir.
    pub fn validate_payload_root(raw: &str) -> Result<(), String> {
        if raw.is_empty() || raw.len() > MAX_PAYLOAD_ROOT_BYTES || !raw.is_ascii() {
            return Err(format!(
                "cloud: payload_dir must be 1..={MAX_PAYLOAD_ROOT_BYTES} ASCII bytes"
            ));
        }
        if raw.bytes().any(|b| b.is_ascii_control()) {
            return Err("cloud: payload_dir contains control characters".into());
        }
        if raw.split(['/', '\\']).any(|part| part == "..") {
            return Err(format!(
                "cloud: payload_dir {raw:?} must not contain `..` traversal"
            ));
        }
        Ok(())
    }

    /// Validate the section (called by [`Config::validate`] on both load
    /// paths). A disabled section validates nothing beyond its shape.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(database) = &self.database {
            Self::validate_database_name("database", database)?;
        }
        if let Some(scm_database) = &self.scm_database {
            Self::validate_database_name("scm_database", scm_database)?;
        }
        if let Some(payload_dir) = &self.payload_dir {
            Self::validate_payload_root(payload_dir)?;
        }
        if let Some(sso) = &self.sso {
            sso.validate()?;
        }
        if let Some(github_app) = &self.github_app {
            github_app.validate()?;
        }
        Ok(())
    }

    /// The resolved operator-staged payload directory under `data_dir`
    /// (default `<data_dir>/payloads`). Validated: a hostile root is refused
    /// at resolution, never silently escaped.
    pub fn payload_root(&self, data_dir: &Path) -> Result<std::path::PathBuf, String> {
        match &self.payload_dir {
            None => Ok(data_dir.join(DEFAULT_PAYLOAD_DIR)),
            Some(raw) => {
                Self::validate_payload_root(raw)?;
                let raw_path = Path::new(raw);
                if raw_path.is_absolute() {
                    Ok(raw_path.to_path_buf())
                } else {
                    Ok(data_dir.join(raw_path))
                }
            }
        }
    }

    /// The resolved control-plane database path under `data_dir` (`None`
    /// when the section is disabled — the daemon then never creates it).
    pub fn control_plane_path(
        &self,
        data_dir: &Path,
    ) -> Result<Option<std::path::PathBuf>, String> {
        if !self.enabled {
            return Ok(None);
        }
        self.validate()?;
        let name = self
            .database
            .clone()
            .unwrap_or_else(|| DEFAULT_CLOUD_DATABASE.to_string());
        Ok(Some(data_dir.join(name)))
    }

    /// The resolved SCM database path under `data_dir` (`None` when the
    /// section is disabled).
    pub fn scm_path(&self, data_dir: &Path) -> Result<Option<std::path::PathBuf>, String> {
        if !self.enabled {
            return Ok(None);
        }
        self.validate()?;
        let name = self
            .scm_database
            .clone()
            .unwrap_or_else(|| DEFAULT_SCM_DATABASE.to_string());
        Ok(Some(data_dir.join(name)))
    }
}

impl CloudSsoCfg {
    /// Validate the section. A disabled section validates nothing beyond its
    /// shape; an enabled one requires an http(s) issuer without a trailing
    /// slash and a bounded client id, a bounded payload name for the
    /// optional client secret and the documented cache bounds.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(client_secret) = &self.client_secret_payload {
            crate::payload::PayloadDir::validate_name(client_secret)
                .map_err(|e| format!("cloud sso: client_secret {e}"))?;
        }
        if let Some(method) = &self.client_secret_method {
            if self.client_secret_payload.is_none() {
                return Err(
                    "cloud sso: client_secret_method requires a `client_secret` payload name"
                        .into(),
                );
            }
            let _ = self.client_auth_method()?;
            let _ = method;
        }
        if !self.enabled {
            return Ok(());
        }
        let _ = self.issuer()?;
        let _ = self.client_id()?;
        for (field, value) in [
            ("discovery_max_age_ms", self.discovery_max_age_ms),
            ("jwks_max_age_ms", self.jwks_max_age_ms),
        ] {
            if let Some(value) = value {
                if value <= 0 || value > faktor_cloud::MAX_CACHE_MAX_AGE_MS {
                    return Err(format!(
                        "cloud sso: {field} must be 1..={}",
                        faktor_cloud::MAX_CACHE_MAX_AGE_MS
                    ));
                }
            }
        }
        if let Some(refetches) = self.max_jwks_refetches {
            if refetches > faktor_cloud::MAX_JWKS_REFETCHES {
                return Err(format!(
                    "cloud sso: max_jwks_refetches must be <= {}",
                    faktor_cloud::MAX_JWKS_REFETCHES
                ));
            }
        }
        if let Some(algorithms) = &self.allowed_algorithms {
            if algorithms.is_empty() || algorithms.len() > faktor_cloud::MAX_ALLOWED_ALGORITHMS {
                return Err(format!(
                    "cloud sso: allowed_algorithms must carry 1..={} entries",
                    faktor_cloud::MAX_ALLOWED_ALGORITHMS
                ));
            }
            for algorithm in algorithms {
                if algorithm == "none" {
                    return Err(
                        "cloud sso: \"none\" is never an accepted id-token signing algorithm"
                            .into(),
                    );
                }
                if !faktor_cloud::SUPPORTED_ALGORITHMS.contains(&algorithm.as_str()) {
                    return Err(format!(
                        "cloud sso: allowed_algorithms entry {algorithm:?} is not supported \
                         (supported: {})",
                        faktor_cloud::SUPPORTED_ALGORITHMS.join(", ")
                    ));
                }
            }
        }
        Ok(())
    }

    /// The resolved allowed-algorithm policy: the configured list, or the
    /// adapter default (`["RS256"]`) when absent. Validated on the way out.
    pub fn allowed_algorithms(&self) -> Result<Vec<String>, String> {
        match &self.allowed_algorithms {
            Some(algorithms) => {
                faktor_cloud::NetworkOidcConfig::validate_allowed_algorithms(algorithms)
                    .map_err(|e| format!("cloud sso: {e}"))?;
                Ok(algorithms.clone())
            }
            None => Ok(faktor_cloud::DEFAULT_ALLOWED_ALGORITHMS
                .iter()
                .map(|alg| (*alg).to_string())
                .collect()),
        }
    }

    /// The configured issuer (required when enabled).
    pub fn issuer(&self) -> Result<String, String> {
        let raw = self
            .issuer
            .clone()
            .ok_or_else(|| "cloud sso: an enabled section requires `issuer`".to_string())?;
        if !(raw.starts_with("https://") || raw.starts_with("http://")) || raw.ends_with('/') {
            return Err("cloud sso: issuer must be an http(s) URL without a trailing slash".into());
        }
        if raw.len() > 2048 {
            return Err("cloud sso: issuer must be at most 2048 bytes".into());
        }
        Ok(raw)
    }

    /// The configured client id (required when enabled).
    pub fn client_id(&self) -> Result<String, String> {
        let raw = self
            .client_id
            .clone()
            .ok_or_else(|| "cloud sso: an enabled section requires `client_id`".to_string())?;
        if raw.trim().is_empty() || raw.len() > 256 {
            return Err("cloud sso: client_id must be 1..=256 bytes".into());
        }
        Ok(raw)
    }

    /// The strict confidential-client auth method: `client_secret_post`
    /// (the default when a secret is staged) or `client_secret_basic`.
    /// Callers reach this only when `client_secret` is configured; the
    /// method must select a confidential exchange (never `none`).
    pub fn client_auth_method(&self) -> Result<faktor_cloud::ClientAuthMethod, String> {
        let raw = self
            .client_secret_method
            .as_deref()
            .unwrap_or("client_secret_post");
        let method = faktor_cloud::ClientAuthMethod::parse(raw).ok_or_else(|| {
            format!(
                "cloud sso: client_secret_method {raw:?} must be client_secret_post \
                 or client_secret_basic"
            )
        })?;
        if !method.requires_secret() {
            return Err(
                "cloud sso: client_secret_method must select a confidential method \
                 (client_secret_post or client_secret_basic)"
                    .into(),
            );
        }
        Ok(method)
    }
}
