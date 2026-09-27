//! `config::updater`: schema domain of the daemon config.

#![allow(unused_imports)]

use super::*;

/// The additive `[updater]` section: the signed updater/distribution
/// lifecycle (stable/beta/dev channels, ed25519 operator keys, staged
/// content-addressed installs).
///
/// Strict and additive:
///
/// - `enabled` (default `false`): when false — the default and the ONLY
///   value for every pre-existing config — the daemon builds NO updater,
///   creates no `update.db` and no install directory, and every
///   `/native/updater/*` route answers a typed 409 `updater_disabled`.
///   The local daemon is byte-identical to the pre-updater daemon;
/// - `channel` (default `stable`): one of `stable|beta|dev`; only manifests
///   the configured channel accepts can advance;
/// - `install_root` (default `install`): the install directory that holds
///   the content-addressed artifacts and the atomic `current` pointer.
///   Either an absolute path or a relative path INSIDE the data dir (no
///   `..`, no control characters, bounded);
/// - `keys` (required when enabled): the operator ed25519 allowlist, each
///   `{id, public_key}` with `public_key` = base64 of the raw 32-byte key.
///   An empty allowlist refuses every manifest (there is no implicit trust
///   anchor), so an enabled section without keys is a config error;
/// - `max_artifact_bytes` (default 256 MiB, cap 4 GiB) and `clock_skew_ms`
///   (default 5 minutes, cap 1 hour) are bounded;
/// - `allow_legacy_manifests_once` (default `false`) is the documented
///   one-time escape hatch for a signed manifest that predates the
///   `release_generation` anti-rollback counter: it is admissible only while
///   the durable per-channel high-water mark is still 0, and admitting it
///   consumes the allowance durably. Leave it off once releases carry the
///   generation.
///
/// Unknown keys, duplicates, non-object shapes and wrong value types are
/// parse errors.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Default)]
pub struct UpdaterCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub install_root: Option<String>,
    #[serde(default)]
    pub max_artifact_bytes: Option<u64>,
    #[serde(default)]
    pub clock_skew_ms: Option<i64>,
    #[serde(default)]
    pub allow_legacy_manifests_once: bool,
    #[serde(default)]
    pub keys: Vec<UpdaterKeyCfg>,
}

/// One allowlisted operator key: an identity and its raw base64 ed25519
/// public key (32 bytes).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdaterKeyCfg {
    pub id: String,
    pub public_key: String,
}

/// The `[updater]` keys, in stable order (unknown-field errors list them).
pub const UPDATER_FIELDS: &[&str] = &[
    "enabled",
    "channel",
    "install_root",
    "max_artifact_bytes",
    "clock_skew_ms",
    "allow_legacy_manifests_once",
    "keys",
];

/// The default install directory name under the daemon data dir.
pub const DEFAULT_UPDATER_INSTALL_ROOT: &str = "install";

/// The default updater database file name.
pub const DEFAULT_UPDATER_DATABASE: &str = "update.db";

/// Bound on the configured install root.
pub const MAX_UPDATER_INSTALL_ROOT_BYTES: usize = 1024;

/// Bound on the operator key allowlist.
pub const MAX_UPDATER_KEYS: usize = 8;

/// The largest `max_artifact_bytes` an operator may configure (4 GiB).
pub const MAX_UPDATER_ARTIFACT_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// The largest accepted clock skew (1 hour).
pub const MAX_UPDATER_CLOCK_SKEW_MS: i64 = 60 * 60 * 1000;

impl<'de> serde::Deserialize<'de> for UpdaterCfg {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error as _, MapAccess, Visitor};

        struct SectionVisitor;

        impl<'de> Visitor<'de> for SectionVisitor {
            type Value = UpdaterCfg;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("the [updater] section as a JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<UpdaterCfg, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut out = UpdaterCfg::default();
                let mut seen: u8 = 0;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "enabled" => {
                            if seen & 1 != 0 {
                                return Err(A::Error::duplicate_field("enabled"));
                            }
                            seen |= 1;
                            out.enabled = map.next_value::<bool>()?;
                        }
                        "channel" => {
                            if seen & 2 != 0 {
                                return Err(A::Error::duplicate_field("channel"));
                            }
                            seen |= 2;
                            out.channel = map.next_value::<Option<String>>()?;
                        }
                        "install_root" => {
                            if seen & 4 != 0 {
                                return Err(A::Error::duplicate_field("install_root"));
                            }
                            seen |= 4;
                            out.install_root = map.next_value::<Option<String>>()?;
                        }
                        "max_artifact_bytes" => {
                            if seen & 8 != 0 {
                                return Err(A::Error::duplicate_field("max_artifact_bytes"));
                            }
                            seen |= 8;
                            out.max_artifact_bytes = map.next_value::<Option<u64>>()?;
                        }
                        "clock_skew_ms" => {
                            if seen & 16 != 0 {
                                return Err(A::Error::duplicate_field("clock_skew_ms"));
                            }
                            seen |= 16;
                            out.clock_skew_ms = map.next_value::<Option<i64>>()?;
                        }
                        "keys" => {
                            if seen & 32 != 0 {
                                return Err(A::Error::duplicate_field("keys"));
                            }
                            seen |= 32;
                            out.keys = map.next_value::<Vec<UpdaterKeyCfg>>()?;
                        }
                        "allow_legacy_manifests_once" => {
                            if seen & 64 != 0 {
                                return Err(A::Error::duplicate_field(
                                    "allow_legacy_manifests_once",
                                ));
                            }
                            seen |= 64;
                            out.allow_legacy_manifests_once = map.next_value::<bool>()?;
                        }
                        other => return Err(A::Error::unknown_field(other, UPDATER_FIELDS)),
                    }
                }
                Ok(out)
            }
        }

        de.deserialize_map(SectionVisitor)
    }
}

impl UpdaterCfg {
    /// The configured channel (default `stable`).
    pub fn channel(&self) -> Result<faktor_updater::Channel, String> {
        let raw = self.channel.as_deref().unwrap_or("stable");
        faktor_updater::Channel::parse(raw)
            .ok_or_else(|| format!("updater: channel {raw:?} must be stable|beta|dev"))
    }

    /// Validate one install-root value: bounded ASCII, no control
    /// characters, no `..` traversal. Absolute paths are allowed; relative
    /// paths live under the daemon data dir.
    pub fn validate_install_root(raw: &str) -> Result<(), String> {
        if raw.is_empty() || raw.len() > MAX_UPDATER_INSTALL_ROOT_BYTES || !raw.is_ascii() {
            return Err(format!(
                "updater: install_root must be 1..={MAX_UPDATER_INSTALL_ROOT_BYTES} ASCII bytes"
            ));
        }
        if raw.bytes().any(|b| b.is_ascii_control()) {
            return Err("updater: install_root contains control characters".into());
        }
        if raw.split(['/', '\\']).any(|part| part == "..") {
            return Err(format!(
                "updater: install_root {raw:?} must not contain `..` traversal"
            ));
        }
        Ok(())
    }

    /// Validate the section (called by [`Config::validate`] on both load
    /// paths). A disabled section validates nothing beyond its shape.
    pub fn validate(&self) -> Result<(), String> {
        self.channel()?;
        if let Some(install_root) = &self.install_root {
            Self::validate_install_root(install_root)?;
        }
        if let Some(max) = self.max_artifact_bytes {
            if max == 0 || max > MAX_UPDATER_ARTIFACT_BYTES {
                return Err(format!(
                    "updater: max_artifact_bytes must be 1..={MAX_UPDATER_ARTIFACT_BYTES}"
                ));
            }
        }
        if let Some(skew) = self.clock_skew_ms {
            if !(0..=MAX_UPDATER_CLOCK_SKEW_MS).contains(&skew) {
                return Err(format!(
                    "updater: clock_skew_ms must be 0..={MAX_UPDATER_CLOCK_SKEW_MS}"
                ));
            }
        }
        if self.keys.len() > MAX_UPDATER_KEYS {
            return Err(format!(
                "updater: at most {MAX_UPDATER_KEYS} operator keys may be configured"
            ));
        }
        let mut seen: Vec<&str> = Vec::with_capacity(self.keys.len());
        for key in &self.keys {
            if seen.contains(&key.id.as_str()) {
                return Err(format!("updater: key id {:?} is configured twice", key.id));
            }
            seen.push(&key.id);
        }
        if self.enabled && self.keys.is_empty() {
            return Err(
                "updater: an enabled [updater] section requires at least one operator key \
                 (an empty allowlist refuses every manifest)"
                    .into(),
            );
        }
        // Key material is validated whenever it is present, so a bad key is
        // a startup error BEFORE an operator flips `enabled`.
        self.trusted_keys()?;
        Ok(())
    }

    /// The operator key allowlist as the updater expects it.
    pub fn trusted_keys(&self) -> Result<faktor_updater::TrustedKeys, String> {
        let mut keys = Vec::with_capacity(self.keys.len());
        for key in &self.keys {
            keys.push(
                faktor_updater::TrustedKey::from_base64(&key.id, &key.public_key)
                    .map_err(|e| format!("updater: {e}"))?,
            );
        }
        faktor_updater::TrustedKeys::new(keys).map_err(|e| format!("updater: {e}"))
    }

    /// The resolved artifact bound.
    pub fn max_artifact_bytes_resolved(&self) -> u64 {
        self.max_artifact_bytes
            .unwrap_or(faktor_updater::DEFAULT_MAX_ARTIFACT_BYTES)
    }

    /// The resolved clock skew.
    pub fn clock_skew_ms_resolved(&self) -> i64 {
        self.clock_skew_ms
            .unwrap_or(faktor_updater::DEFAULT_CLOCK_SKEW_MS)
    }

    /// The documented one-time legacy-manifest allowance (default false).
    pub fn allow_legacy_manifests_once_resolved(&self) -> bool {
        self.allow_legacy_manifests_once
    }

    /// The install root under `data_dir` (`None` when the section is
    /// disabled — the daemon then never creates it).
    pub fn install_root_path(&self, data_dir: &Path) -> Result<Option<std::path::PathBuf>, String> {
        if !self.enabled {
            return Ok(None);
        }
        self.validate()?;
        let raw = self
            .install_root
            .clone()
            .unwrap_or_else(|| DEFAULT_UPDATER_INSTALL_ROOT.to_string());
        let path = std::path::PathBuf::from(&raw);
        Ok(Some(if path.is_absolute() {
            path
        } else {
            data_dir.join(path)
        }))
    }

    /// The resolved updater database path under `data_dir` (`None` when the
    /// section is disabled).
    pub fn database_path(&self, data_dir: &Path) -> Result<Option<std::path::PathBuf>, String> {
        if !self.enabled {
            return Ok(None);
        }
        self.validate()?;
        Ok(Some(data_dir.join(DEFAULT_UPDATER_DATABASE)))
    }
}
