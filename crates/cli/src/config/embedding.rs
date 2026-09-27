//! `config::embedding`: schema domain of the daemon config.

#![allow(unused_imports)]

use super::*;

/// The additive `[embeddings]` section: ONE selected semantic embedding
/// provider. Strict by construction (map-only parsing: unknown keys,
/// duplicate keys, non-object shapes and wrong value types are parse
/// errors) and strictly additive (an absent section keeps no embedder):///
/// - `provider` names a REGISTERED provider instance id;
/// - `model` names the embedding model that instance serves;
/// - `policy` decides what an unresolvable selection means:
///   `best_effort` (default) degrades to no embedder with a warning, while
///   `required` refuses startup — the daemon never boots claiming semantic
///   retrieval it cannot honor.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EmbeddingCfg {
    pub provider: String,
    pub model: String,
    pub policy: EmbeddingPolicy,
}

/// Selection strictness of the `[embeddings]` section. See [`EmbeddingCfg`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingPolicy {
    /// An unresolvable selection degrades to no embedder (honest lexical/
    /// symbol-only retrieval) with a warning.
    #[default]
    BestEffort,
    /// An unresolvable selection fails daemon startup.
    Required,
}

impl<'de> serde::Deserialize<'de> for EmbeddingPolicy {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        let value = String::deserialize(de)?;
        match value.as_str() {
            "best_effort" => Ok(Self::BestEffort),
            "required" => Ok(Self::Required),
            other => Err(D::Error::custom(format!(
                "unknown embedding policy {other:?}; expected \"best_effort\" or \"required\""
            ))),
        }
    }
}

/// The `[embeddings]` keys, in stable order (unknown-field errors list
/// them).
pub const EMBEDDING_FIELDS: &[&str] = &["provider", "model", "policy"];

/// Strict bound of the configured embedding provider/model ids: long enough
/// for real ids, short enough to stay journal-safe.
pub const MAX_EMBEDDING_ID_BYTES: usize = 256;

impl<'de> serde::Deserialize<'de> for EmbeddingCfg {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error as _, MapAccess, Visitor};

        struct SectionVisitor;

        impl<'de> Visitor<'de> for SectionVisitor {
            type Value = EmbeddingCfg;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("the [embeddings] section as a JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<EmbeddingCfg, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut provider: Option<String> = None;
                let mut model: Option<String> = None;
                let mut policy: Option<EmbeddingPolicy> = None;
                let mut seen: u8 = 0;
                while let Some(key) = map.next_key::<String>()? {
                    let (bit, name) = match key.as_str() {
                        "provider" => (1u8, "provider"),
                        "model" => (2, "model"),
                        "policy" => (4, "policy"),
                        other => return Err(A::Error::unknown_field(other, EMBEDDING_FIELDS)),
                    };
                    if seen & bit != 0 {
                        return Err(A::Error::duplicate_field(name));
                    }
                    seen |= bit;
                    match bit {
                        1 => provider = Some(map.next_value()?),
                        2 => model = Some(map.next_value()?),
                        _ => policy = Some(map.next_value()?),
                    }
                }
                Ok(EmbeddingCfg {
                    provider: provider.ok_or_else(|| A::Error::missing_field("provider"))?,
                    model: model.ok_or_else(|| A::Error::missing_field("model"))?,
                    policy: policy.unwrap_or_default(),
                })
            }
        }

        de.deserialize_map(SectionVisitor)
    }
}

impl EmbeddingCfg {
    /// Semantic validation shared by both load paths: ids are non-empty and
    /// bounded. A registry-resolvable check happens at daemon build, where
    /// the provider registry exists.
    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [("provider", &self.provider), ("model", &self.model)] {
            if value.trim().is_empty() {
                return Err(format!("embeddings: {name} is empty"));
            }
            if value.len() > MAX_EMBEDDING_ID_BYTES {
                return Err(format!(
                    "embeddings: {name} exceeds {MAX_EMBEDDING_ID_BYTES} bytes"
                ));
            }
        }
        Ok(())
    }
}
