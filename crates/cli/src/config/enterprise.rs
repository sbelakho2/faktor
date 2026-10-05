//! `config::enterprise`: schema domain of the daemon config.

use super::*;

/// The additive `[enterprise]` section: the enterprise retention/audit/
/// admin plane (retention classes + guarded GC, the append-only audit
/// ledger, deletion jobs, admin settings and the effective-config
/// attestation). Disabled by default: no database is created, every
/// `/native/enterprise/*` route answers a typed 409 `enterprise_disabled`,
/// and the daemon is otherwise byte-identical.
///
/// Strict by construction (`deny_unknown_fields`: unknown keys, duplicates
/// and wrong value types are parse errors on both load paths):
///
/// - `enabled` (default `false`);
/// - `database`: an optional simple FILE NAME relative to the data dir
///   (default `enterprise.db`); paths/traversal are refused;
/// - `organization`: the tenant the local operator administers (required
///   when enabled; also the principal used by the `faktor enterprise`
///   local-parity subcommands);
/// - `[enterprise.policy]`: the LOCAL organization policy layer of the
///   layered configuration (allowed sets; intersect-only);
/// - `[enterprise.preferences]`: the LOCAL user preference layer (chosen
///   values; refused when outside the policy).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct EnterpriseCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub organization: Option<String>,
    #[serde(default)]
    pub policy: EnterprisePolicyCfg,
    #[serde(default)]
    pub preferences: EnterprisePreferenceCfg,
}

/// The `[enterprise.policy]` keys: allowed sets per configuration key
/// (intersect-only policy semantics; an empty list is refused by the
/// resolver, never silently accepted).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct EnterprisePolicyCfg {
    #[serde(default)]
    pub network: Option<Vec<String>>,
    #[serde(default)]
    pub providers: Option<Vec<String>>,
    #[serde(default)]
    pub models: Option<Vec<String>>,
    #[serde(default)]
    pub tool_grants: Option<Vec<String>>,
    #[serde(default)]
    pub retention: Option<Vec<String>>,
}

/// The `[enterprise.preferences]` keys: the chosen value per configuration
/// key (a preference outside the effective policy is refused).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct EnterprisePreferenceCfg {
    #[serde(default)]
    pub network: Option<String>,
    #[serde(default)]
    pub providers: Option<String>,
    #[serde(default)]
    pub models: Option<String>,
    #[serde(default)]
    pub tool_grants: Option<String>,
    #[serde(default)]
    pub retention: Option<String>,
}

/// The default enterprise-plane database file name (relative to the daemon
/// data dir).
pub const DEFAULT_ENTERPRISE_DATABASE: &str = "enterprise.db";

impl EnterpriseCfg {
    /// The resolved database path (`None` while disabled).
    pub fn enterprise_path(&self, data_dir: &Path) -> Result<Option<std::path::PathBuf>, String> {
        if !self.enabled {
            return Ok(None);
        }
        let name = self
            .database
            .clone()
            .unwrap_or_else(|| DEFAULT_ENTERPRISE_DATABASE.to_string());
        CloudCfg::validate_database_name("enterprise database", &name)?;
        Ok(Some(data_dir.join(name)))
    }

    /// The tenant the local operator administers.
    pub fn organization(&self) -> Result<Option<faktor_cloud::OrganizationId>, String> {
        if !self.enabled {
            return Ok(None);
        }
        let raw = self
            .organization
            .as_deref()
            .ok_or("enterprise: an enabled [enterprise] section requires `organization`")?;
        let trimmed = raw.trim();
        faktor_cloud::OrganizationId::try_new(trimmed)
            .map(Some)
            .map_err(|e| format!("enterprise: organization: {e}"))
    }

    /// The ordered local layers of the layered configuration: the
    /// organization POLICY layer (optional) then the user PREFERENCE layer
    /// (optional). Semantics and ceilings are enforced by
    /// [`faktor_cloud::resolve_layers`], the ONE resolver both this local
    /// parity path and the server route use.
    pub fn layers(&self) -> Result<Vec<faktor_cloud::ConfigLayer>, String> {
        use faktor_cloud::{ConfigKey, ConfigLayer, ConfigScope, LayerSemantics, LayerValue};
        let mut layers = Vec::new();
        let policy_values: Vec<(ConfigKey, LayerValue)> = [
            (ConfigKey::Network, self.policy.network.as_ref()),
            (ConfigKey::Providers, self.policy.providers.as_ref()),
            (ConfigKey::Models, self.policy.models.as_ref()),
            (ConfigKey::ToolGrants, self.policy.tool_grants.as_ref()),
            (ConfigKey::Retention, self.policy.retention.as_ref()),
        ]
        .into_iter()
        .filter_map(|(key, values)| values.map(|values| (key, LayerValue::Policy(values.clone()))))
        .collect();
        if !policy_values.is_empty() {
            layers.push(ConfigLayer {
                scope: ConfigScope::Organization,
                semantics: LayerSemantics::Policy,
                scope_ref: self.organization.clone(),
                revision: 1,
                values: policy_values.into_iter().collect(),
            });
        }
        let preference_values: Vec<(ConfigKey, LayerValue)> = [
            (ConfigKey::Network, self.preferences.network.as_ref()),
            (ConfigKey::Providers, self.preferences.providers.as_ref()),
            (ConfigKey::Models, self.preferences.models.as_ref()),
            (ConfigKey::ToolGrants, self.preferences.tool_grants.as_ref()),
            (ConfigKey::Retention, self.preferences.retention.as_ref()),
        ]
        .into_iter()
        .filter_map(|(key, value)| value.map(|value| (key, LayerValue::Preference(value.clone()))))
        .collect();
        if !preference_values.is_empty() {
            layers.push(ConfigLayer {
                scope: ConfigScope::User,
                semantics: LayerSemantics::Preference,
                scope_ref: self.organization.clone(),
                revision: 1,
                values: preference_values.into_iter().collect(),
            });
        }
        for layer in &layers {
            layer.validate().map_err(|e| format!("enterprise: {e}"))?;
        }
        Ok(layers)
    }

    /// Validate the section (called by [`Config::validate`] on both load
    /// paths). A disabled section validates nothing beyond its shape.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(database) = &self.database {
            CloudCfg::validate_database_name("enterprise database", database)?;
        }
        if !self.enabled {
            return Ok(());
        }
        let _ = self.organization()?;
        let layers = self.layers()?;
        // The ONE resolver decides whether preferences fit policy: without
        // this the config booted and created enterprise DBs before the
        // refusal appeared at first use.
        faktor_cloud::resolve_layers(&layers).map_err(|e| format!("enterprise: {e}"))?;
        Ok(())
    }
}
