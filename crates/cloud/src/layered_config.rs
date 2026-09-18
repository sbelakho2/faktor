//! Layered configuration with explicit POLICY vs PREFERENCE semantics.
//!
//! Layers, outermost to innermost:
//!
//! `system` (deployment) -> `organization` policy -> `repository` policy ->
//! `user` preference -> `session` preference -> `task` override.
//!
//! Semantics are structural, not conventional:
//!
//! - A **policy** layer constrains: the effective policy for a key is the
//!   INTERSECTION of every policy layer's allowed set. A layer that names an
//!   option outside the accumulated ceiling is a typed refusal
//!   ([`LayeredConfigError::PolicyLoosened`]) — a task override can never
//!   loosen an organization's `network = deny` (the org policy
//!   `{"none"}` and a task policy `{"none","provider"}` is refused, not
//!   merged);
//! - a **preference** layer chooses: the innermost preference wins, and a
//!   preference outside the effective policy is a typed refusal
//!   ([`LayeredConfigError::PreferenceRefusedByPolicy`]), never a silent
//!   clamp;
//! - a layer's semantic and value type must agree (a policy value on a
//!   preference layer is malformed) and layers must be supplied in
//!   scope order (a task layer sandboxed under a session? refused).
//!
//! The resolved [`EffectiveConfig`] carries a deterministic digest over the
//! effective values AND the ordered layer stamps (scope + revision), so
//! changing any layer changes the digest; the digest is what gets stored
//! with proof/audit rows ([`ConfigAttestation`]) to make the
//! verification-relevant configuration attributable.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// One configuration layer scope, ordered outermost to innermost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigScope {
    System,
    Organization,
    Repository,
    User,
    Session,
    Task,
}

impl ConfigScope {
    pub const ALL: [ConfigScope; 6] = [
        ConfigScope::System,
        ConfigScope::Organization,
        ConfigScope::Repository,
        ConfigScope::User,
        ConfigScope::Session,
        ConfigScope::Task,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            ConfigScope::System => "system",
            ConfigScope::Organization => "organization",
            ConfigScope::Repository => "repository",
            ConfigScope::User => "user",
            ConfigScope::Session => "session",
            ConfigScope::Task => "task",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|s| s.as_str() == raw)
    }

    const fn rank(self) -> u8 {
        match self {
            ConfigScope::System => 0,
            ConfigScope::Organization => 1,
            ConfigScope::Repository => 2,
            ConfigScope::User => 3,
            ConfigScope::Session => 4,
            ConfigScope::Task => 5,
        }
    }
}

/// What a layer DOES: constrain (policy) or choose (preference).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayerSemantics {
    Policy,
    Preference,
}

/// One well-known configuration key. Unknown keys are refused at parse time
/// (strict DTOs); the list is bounded by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigKey {
    /// Network access: option values are `none` | `provider` | `mcp`.
    Network,
    /// Provider ids the scope may use.
    Providers,
    /// Model ids the scope may use.
    Models,
    /// Privileged tool grants (tool names).
    ToolGrants,
    /// Retention class names (must be retention classes of the enterprise
    /// plane; the value list is the allowed set).
    Retention,
}

impl ConfigKey {
    pub const ALL: [ConfigKey; 5] = [
        ConfigKey::Network,
        ConfigKey::Providers,
        ConfigKey::Models,
        ConfigKey::ToolGrants,
        ConfigKey::Retention,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            ConfigKey::Network => "network",
            ConfigKey::Providers => "providers",
            ConfigKey::Models => "models",
            ConfigKey::ToolGrants => "tool_grants",
            ConfigKey::Retention => "retention",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == raw)
    }
}

/// One layer value: a policy's allowed set or a preference's chosen value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum LayerValue {
    Policy(Vec<String>),
    Preference(String),
}

/// One configuration layer. `revision` is the durable revision of the layer
/// (the attribution stamp); `scope_ref` names the repository/user/session
/// the layer belongs to (`None` for system/organization).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigLayer {
    pub scope: ConfigScope,
    pub semantics: LayerSemantics,
    #[serde(default)]
    pub scope_ref: Option<String>,
    pub revision: u64,
    pub values: BTreeMap<ConfigKey, LayerValue>,
}

impl ConfigLayer {
    pub fn validate(&self) -> Result<(), LayeredConfigError> {
        if let Some(scope_ref) = &self.scope_ref {
            if scope_ref.is_empty() || scope_ref.len() > 256 {
                return Err(LayeredConfigError::Malformed(
                    "layer scope_ref must be 1..=256 bytes".into(),
                ));
            }
            if scope_ref.bytes().any(|b| b.is_ascii_control()) {
                return Err(LayeredConfigError::Malformed(
                    "layer scope_ref contains control characters".into(),
                ));
            }
        }
        for (key, value) in &self.values {
            match (self.semantics, value) {
                (LayerSemantics::Policy, LayerValue::Policy(options)) => {
                    if options.is_empty() {
                        return Err(LayeredConfigError::Malformed(format!(
                            "policy layer for {:?} carries an empty allowed set",
                            key.as_str()
                        )));
                    }
                    let mut seen = BTreeSet::new();
                    for option in options {
                        if option.is_empty() || option.len() > 256 {
                            return Err(LayeredConfigError::Malformed(format!(
                                "policy option for {} must be 1..=256 bytes",
                                key.as_str()
                            )));
                        }
                        if option.bytes().any(|b| b.is_ascii_control()) {
                            return Err(LayeredConfigError::Malformed(format!(
                                "policy option for {} contains control characters",
                                key.as_str()
                            )));
                        }
                        if !seen.insert(option.as_str()) {
                            return Err(LayeredConfigError::Malformed(format!(
                                "policy option {option:?} repeats in {}",
                                key.as_str()
                            )));
                        }
                    }
                }
                (LayerSemantics::Preference, LayerValue::Preference(value)) => {
                    if value.is_empty() || value.len() > 256 {
                        return Err(LayeredConfigError::Malformed(format!(
                            "preference for {} must be 1..=256 bytes",
                            key.as_str()
                        )));
                    }
                    if value.bytes().any(|b| b.is_ascii_control()) {
                        return Err(LayeredConfigError::Malformed(format!(
                            "preference for {} contains control characters",
                            key.as_str()
                        )));
                    }
                }
                (LayerSemantics::Policy, LayerValue::Preference(_)) => {
                    return Err(LayeredConfigError::Malformed(format!(
                        "policy layer for {} carries a preference value",
                        key.as_str()
                    )));
                }
                (LayerSemantics::Preference, LayerValue::Policy(_)) => {
                    return Err(LayeredConfigError::Malformed(format!(
                        "preference layer for {} carries a policy value",
                        key.as_str()
                    )));
                }
            }
        }
        Ok(())
    }
}

/// One resolved preference with its attribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreferenceResolution {
    pub value: String,
    pub scope: ConfigScope,
    pub revision: u64,
}

/// The resolved effective configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveConfig {
    /// Intersection of every policy layer's allowed set, per key.
    pub policies: BTreeMap<ConfigKey, BTreeSet<String>>,
    /// The winning preference per key (innermost), with attribution.
    pub preferences: BTreeMap<ConfigKey, PreferenceResolution>,
    /// Deterministic digest over the effective values AND the ordered layer
    /// stamps; changing any layer changes this digest.
    pub digest: String,
}

impl EffectiveConfig {
    /// Whether a value is permitted by the effective policy for `key`
    /// (no policy entry = unrestricted).
    pub fn policy_allows(&self, key: ConfigKey, value: &str) -> bool {
        self.policies
            .get(&key)
            .map(|allowed| allowed.contains(value))
            .unwrap_or(true)
    }

    /// The attribution record stored with proof/audit rows.
    pub fn attestation(&self) -> ConfigAttestation {
        ConfigAttestation {
            digest: self.digest.clone(),
        }
    }
}

/// The digest stored with proof/audit rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigAttestation {
    pub digest: String,
}

/// A typed layered-config refusal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LayeredConfigError {
    #[error(
        "policy loosening refused: layer {scope} ({semantics}) would widen {key} from {ceiling:?} to {attempted:?}"
    )]
    PolicyLoosened {
        scope: &'static str,
        semantics: &'static str,
        key: &'static str,
        ceiling: Vec<String>,
        attempted: Vec<String>,
    },
    #[error(
        "preference refused by policy: {key} = {value:?} is outside the allowed set {allowed:?}"
    )]
    PreferenceRefusedByPolicy {
        key: &'static str,
        value: String,
        allowed: Vec<String>,
    },
    #[error("layered configuration is malformed: {0}")]
    Malformed(String),
}

impl From<LayeredConfigError> for crate::error::ControlPlaneError {
    fn from(e: LayeredConfigError) -> Self {
        match e {
            LayeredConfigError::Malformed(message) => {
                crate::error::ControlPlaneError::Malformed(message)
            }
            other => crate::error::ControlPlaneError::Conflict(other.to_string()),
        }
    }
}

fn digest_of(
    layers: &[ConfigLayer],
    policies: &BTreeMap<ConfigKey, BTreeSet<String>>,
    preferences: &BTreeMap<ConfigKey, PreferenceResolution>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    for layer in layers {
        parts.push(format!(
            "layer\0{}\0{}\0{}\0{}",
            layer.scope.as_str(),
            match layer.semantics {
                LayerSemantics::Policy => "policy",
                LayerSemantics::Preference => "preference",
            },
            layer.scope_ref.as_deref().unwrap_or(""),
            layer.revision
        ));
        for (key, value) in &layer.values {
            match value {
                LayerValue::Policy(options) => {
                    let mut options = options.clone();
                    options.sort();
                    parts.push(format!(
                        "value\0{}\0policy\0{}",
                        key.as_str(),
                        options.join(",")
                    ));
                }
                LayerValue::Preference(value) => {
                    parts.push(format!("value\0{}\0preference\0{value}", key.as_str()));
                }
            }
        }
    }
    for (key, allowed) in policies {
        parts.push(format!(
            "effective_policy\0{}\0{}",
            key.as_str(),
            allowed.iter().cloned().collect::<Vec<_>>().join(",")
        ));
    }
    for (key, resolution) in preferences {
        parts.push(format!(
            "effective_preference\0{}\0{}\0{}\0{}",
            key.as_str(),
            resolution.value,
            resolution.scope.as_str(),
            resolution.revision
        ));
    }
    parts.sort();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"faktor-effective-config:v1\0");
    hasher.update(parts.join("\n").as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// Resolve the ordered layers into the effective configuration. See the
/// module docs for the exact policy/preference semantics.
pub fn resolve_layers(layers: &[ConfigLayer]) -> Result<EffectiveConfig, LayeredConfigError> {
    let mut policies: BTreeMap<ConfigKey, BTreeSet<String>> = BTreeMap::new();
    let mut preferences: BTreeMap<ConfigKey, PreferenceResolution> = BTreeMap::new();
    let mut last_rank: Option<u8> = None;
    for layer in layers {
        layer.validate()?;
        if let Some(rank) = last_rank {
            if layer.scope.rank() < rank {
                return Err(LayeredConfigError::Malformed(format!(
                    "layer {} is out of scope order (after a deeper layer)",
                    layer.scope.as_str()
                )));
            }
        }
        last_rank = Some(layer.scope.rank());
        for (key, value) in &layer.values {
            match value {
                LayerValue::Policy(options) => {
                    let attempted: BTreeSet<String> = options.iter().cloned().collect();
                    if let Some(ceiling) = policies.get(key) {
                        if !attempted.is_subset(ceiling) {
                            return Err(LayeredConfigError::PolicyLoosened {
                                scope: layer.scope.as_str(),
                                semantics: "policy",
                                key: key.as_str(),
                                ceiling: ceiling.iter().cloned().collect(),
                                attempted: attempted.iter().cloned().collect(),
                            });
                        }
                    }
                    // Intersection-only: the new ceiling is the intersection.
                    // The FIRST policy layer for a key establishes it.
                    match policies.get(key) {
                        Some(ceiling) => {
                            let intersected = ceiling.intersection(&attempted).cloned().collect();
                            policies.insert(*key, intersected);
                        }
                        None => {
                            policies.insert(*key, attempted);
                        }
                    }
                }
                LayerValue::Preference(value) => {
                    if let Some(allowed) = policies.get(key) {
                        if !allowed.contains(value) {
                            return Err(LayeredConfigError::PreferenceRefusedByPolicy {
                                key: key.as_str(),
                                value: value.clone(),
                                allowed: allowed.iter().cloned().collect(),
                            });
                        }
                    }
                    // Innermost preference wins: later layers overwrite.
                    preferences.insert(
                        *key,
                        PreferenceResolution {
                            value: value.clone(),
                            scope: layer.scope,
                            revision: layer.revision,
                        },
                    );
                }
            }
        }
    }
    let digest = digest_of(layers, &policies, &preferences);
    Ok(EffectiveConfig {
        policies,
        preferences,
        digest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(
        scope: ConfigScope,
        semantics: LayerSemantics,
        revision: u64,
        values: &[(ConfigKey, LayerValue)],
    ) -> ConfigLayer {
        ConfigLayer {
            scope,
            semantics,
            scope_ref: None,
            revision,
            values: values.iter().cloned().collect(),
        }
    }

    fn policy(key: ConfigKey, options: &[&str]) -> (ConfigKey, LayerValue) {
        (
            key,
            LayerValue::Policy(options.iter().map(|s| s.to_string()).collect()),
        )
    }

    fn preference(key: ConfigKey, value: &str) -> (ConfigKey, LayerValue) {
        (key, LayerValue::Preference(value.to_string()))
    }

    #[test]
    fn policy_layers_intersect_only_and_loosening_is_refused() {
        let org = layer(
            ConfigScope::Organization,
            LayerSemantics::Policy,
            1,
            &[policy(ConfigKey::Network, &["none"])],
        );
        // Narrowing is allowed (intersection).
        let repo = layer(
            ConfigScope::Repository,
            LayerSemantics::Policy,
            1,
            &[policy(ConfigKey::Network, &["none"])],
        );
        let resolved = resolve_layers(&[org.clone(), repo]).unwrap();
        assert_eq!(
            resolved
                .policies
                .get(&ConfigKey::Network)
                .unwrap()
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["none".to_string()]
        );
        assert!(resolved.policy_allows(ConfigKey::Network, "none"));
        assert!(!resolved.policy_allows(ConfigKey::Network, "provider"));

        // A task override naming an option outside the org ceiling is a
        // typed refusal: deny cannot be loosened.
        let task = layer(
            ConfigScope::Task,
            LayerSemantics::Policy,
            1,
            &[policy(ConfigKey::Network, &["none", "provider"])],
        );
        let refused = resolve_layers(&[org.clone(), task]).unwrap_err();
        assert!(matches!(
            refused,
            LayeredConfigError::PolicyLoosened { key: "network", .. }
        ));

        // A policy layer naming ONLY options the ceiling forbids is refused
        // too (a policy can narrow, never smuggle a new option in).
        let disjoint = layer(
            ConfigScope::Repository,
            LayerSemantics::Policy,
            2,
            &[policy(ConfigKey::Network, &["mcp"])],
        );
        assert!(matches!(
            resolve_layers(&[org.clone(), disjoint]).unwrap_err(),
            LayeredConfigError::PolicyLoosened { key: "network", .. }
        ));

        // A narrower layer that stays inside the ceiling is accepted: the
        // effective policy is the intersection.
        let narrowed = layer(
            ConfigScope::Repository,
            LayerSemantics::Policy,
            3,
            &[policy(ConfigKey::Providers, &["anthropic"])],
        );
        let org_providers = layer(
            ConfigScope::Organization,
            LayerSemantics::Policy,
            1,
            &[policy(ConfigKey::Providers, &["anthropic", "openai"])],
        );
        let resolved = resolve_layers(&[org_providers, narrowed]).unwrap();
        assert_eq!(
            resolved
                .policies
                .get(&ConfigKey::Providers)
                .unwrap()
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["anthropic".to_string()]
        );
        assert!(!resolved.policy_allows(ConfigKey::Providers, "openai"));
    }

    #[test]
    fn preferences_override_preferences_but_never_policy() {
        let org = layer(
            ConfigScope::Organization,
            LayerSemantics::Policy,
            1,
            &[policy(ConfigKey::Providers, &["anthropic", "openai"])],
        );
        let user = layer(
            ConfigScope::User,
            LayerSemantics::Preference,
            3,
            &[preference(ConfigKey::Providers, "openai")],
        );
        let session = layer(
            ConfigScope::Session,
            LayerSemantics::Preference,
            1,
            &[preference(ConfigKey::Providers, "anthropic")],
        );
        let task = layer(
            ConfigScope::Task,
            LayerSemantics::Preference,
            2,
            &[preference(ConfigKey::Providers, "openai")],
        );
        let resolved = resolve_layers(&[org.clone(), user, session, task]).unwrap();
        let winner = resolved.preferences.get(&ConfigKey::Providers).unwrap();
        assert_eq!(winner.value, "openai");
        assert_eq!(winner.scope, ConfigScope::Task);

        // A preference outside the policy is refused, not clamped.
        let rogue = layer(
            ConfigScope::Task,
            LayerSemantics::Preference,
            3,
            &[preference(ConfigKey::Providers, "some-other-provider")],
        );
        let refused = resolve_layers(&[org, rogue]).unwrap_err();
        assert!(matches!(
            refused,
            LayeredConfigError::PreferenceRefusedByPolicy {
                key: "providers",
                ..
            }
        ));
    }

    #[test]
    fn semantic_and_value_mismatch_and_out_of_order_layers_are_malformed() {
        let mismatched = ConfigLayer {
            scope: ConfigScope::Task,
            semantics: LayerSemantics::Policy,
            scope_ref: None,
            revision: 1,
            values: BTreeMap::from([preference(ConfigKey::Network, "none")]),
        };
        assert!(matches!(
            resolve_layers(&[mismatched]).unwrap_err(),
            LayeredConfigError::Malformed(_)
        ));
        let deep = layer(
            ConfigScope::Task,
            LayerSemantics::Preference,
            1,
            &[preference(ConfigKey::Network, "none")],
        );
        let shallow = layer(
            ConfigScope::Session,
            LayerSemantics::Preference,
            1,
            &[preference(ConfigKey::Network, "none")],
        );
        assert!(matches!(
            resolve_layers(&[deep, shallow]).unwrap_err(),
            LayeredConfigError::Malformed(_)
        ));
    }

    #[test]
    fn the_full_six_layer_stack_resolves_inward_with_policy_intersection() {
        let layers = vec![
            layer(
                ConfigScope::System,
                LayerSemantics::Policy,
                1,
                &[policy(ConfigKey::Network, &["none", "provider", "mcp"])],
            ),
            layer(
                ConfigScope::Organization,
                LayerSemantics::Policy,
                1,
                &[policy(ConfigKey::Network, &["none", "provider"])],
            ),
            ConfigLayer {
                scope: ConfigScope::Repository,
                semantics: LayerSemantics::Policy,
                scope_ref: Some("repo_1".into()),
                revision: 1,
                values: BTreeMap::from([policy(ConfigKey::Network, &["none"])]),
            },
            ConfigLayer {
                scope: ConfigScope::User,
                semantics: LayerSemantics::Preference,
                scope_ref: Some("usr_1".into()),
                revision: 1,
                values: BTreeMap::from([preference(ConfigKey::Network, "none")]),
            },
            ConfigLayer {
                scope: ConfigScope::Session,
                semantics: LayerSemantics::Preference,
                scope_ref: Some("ses_1".into()),
                revision: 1,
                values: BTreeMap::from([preference(ConfigKey::Network, "none")]),
            },
        ];
        let effective = resolve_layers(&layers).unwrap();
        assert_eq!(
            effective
                .policies
                .get(&ConfigKey::Network)
                .unwrap()
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["none".to_string()],
            "system x org x repo intersect"
        );
        assert_eq!(
            effective
                .preferences
                .get(&ConfigKey::Network)
                .unwrap()
                .scope,
            ConfigScope::Session,
            "the innermost preference wins"
        );

        // A task override trying to re-enable provider is refused.
        let mut with_task = layers.clone();
        with_task.push(layer(
            ConfigScope::Task,
            LayerSemantics::Policy,
            1,
            &[policy(ConfigKey::Network, &["provider"])],
        ));
        assert!(matches!(
            resolve_layers(&with_task).unwrap_err(),
            LayeredConfigError::PolicyLoosened { .. }
        ));
    }

    #[test]
    fn effective_digest_changes_when_any_layer_changes_and_is_deterministic() {
        let base = vec![
            layer(
                ConfigScope::Organization,
                LayerSemantics::Policy,
                1,
                &[policy(ConfigKey::Providers, &["anthropic"])],
            ),
            layer(
                ConfigScope::User,
                LayerSemantics::Preference,
                1,
                &[preference(ConfigKey::Providers, "anthropic")],
            ),
        ];
        let first = resolve_layers(&base).unwrap();
        let repeated = resolve_layers(&base).unwrap();
        assert_eq!(first.digest, repeated.digest, "resolution is deterministic");
        assert_eq!(
            first.attestation().digest,
            first.digest,
            "the attestation carries the digest"
        );

        // Changing a policy layer's value changes the digest.
        let mut changed_value = base.clone();
        changed_value[0] = layer(
            ConfigScope::Organization,
            LayerSemantics::Policy,
            1,
            &[policy(ConfigKey::Providers, &["anthropic", "openai"])],
        );
        assert_ne!(resolve_layers(&changed_value).unwrap().digest, first.digest);

        // Changing only a layer REVISION changes the digest even when the
        // effective values are identical (attribution is part of the
        // digest).
        let mut changed_revision = base.clone();
        changed_revision[1].revision = 2;
        assert_ne!(
            resolve_layers(&changed_revision).unwrap().digest,
            first.digest
        );
    }
}
