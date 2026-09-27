//! `config::tasks`: schema domain of the daemon config.

use super::*;

/// The additive `[tasks]` section (P0 mutation isolation; wave-24 policy).
///
/// The section is strictly additive with `serde(default)` and an absent
/// section keeping the crate default `mutation_mode: Shadow` — every
/// MUTATING task works in a daemon-owned isolated candidate and is
/// integrated back into the user checkout with a conflict-aware CAS commit.
/// There is NO direct-owner mode any more: `mutation_mode:
/// "direct_compat"` is a strict parse error naming the removal, and so is
/// the legacy `shadow_mutation = false` value.
///
/// The pre-wave-24 boolean key `shadow_mutation` is still accepted as a
/// LEGACY alias for its historical `true` meaning only (shadow mutation on),
/// but it is an error to specify BOTH keys — the file never says two
/// different things. Unknown keys inside the section are parse errors
/// (strict on both load paths).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Default)]
pub struct TasksCfg {
    #[serde(default)]
    pub mutation_mode: MutationMode,
}

impl<'de> serde::Deserialize<'de> for TasksCfg {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct File {
            #[serde(default)]
            mutation_mode: Option<MutationMode>,
            /// Legacy pre-wave-24 alias (config_version 1 files written
            /// before the mode existed). `true` = shadow mutation on; the
            /// `false` value (direct) was REMOVED and is a strict error.
            #[serde(default)]
            shadow_mutation: Option<bool>,
        }
        let file = File::deserialize(de)?;
        match (file.mutation_mode, file.shadow_mutation) {
            (Some(_), Some(_)) => Err(D::Error::custom(
                "conflicting [tasks] keys: mutation_mode and the legacy shadow_mutation alias cannot both be present",
            )),
            (Some(m), None) => Ok(Self { mutation_mode: m }),
            (None, Some(true)) => Ok(Self {
                mutation_mode: MutationMode::Shadow,
            }),
            (None, Some(false)) => Err(D::Error::custom(
                "the legacy [tasks] shadow_mutation = false was removed: every mutating run executes in an isolated candidate (shadow mutation); there is no direct-owner mode",
            )),
            (None, None) => Ok(Self {
                mutation_mode: MutationMode::Shadow,
            }),
        }
    }
}
