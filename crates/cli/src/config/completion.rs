//! `config::completion`: schema domain of the daemon config.

#[cfg(test)]
use super::*;

/// The additive `[completion]` section (P2 follow-up): how a contracted
/// run's ordered commit/push/PR steps execute.
///
/// Strict by construction: unknown keys are parse errors (derived
/// `deny_unknown_fields`), non-string values are type errors, and the
/// resolved [`faktor_orchestrator::runtime::completion_steps::CompletionStepsConfig`]
/// is validated (bounded remote/base branch; a bounded single-line PR
/// command/argv with known placeholders only and `{branch}` required; the
/// legacy `pr_command` string keeps every historic security check, while
/// the additive `pr_program`/`pr_args` typed argv preserves spaces per
/// element). An absent section keeps the inert defaults: push targets
/// `origin`, the PR base is `main`, and an unconfigured PR records the
/// documented `Skipped` outcome — a requested PR step is never silently
/// invented.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Default)]
pub struct CompletionCfg {
    /// The git remote the push step targets (default `origin`).
    pub remote: Option<String>,
    /// The base branch rendered into the PR template (default `main`).
    pub base_branch: Option<String>,
    /// DEPRECATED whitespace-split PR command template, e.g.
    /// `"gh pr create --head {branch} --base {base}"`. `None` (the default)
    /// means a requested PR step records `Skipped` (unless `pr_program` is
    /// configured). Mutually exclusive with `pr_program`.
    pub pr_command: Option<String>,
    /// The typed argv program (P3): executed directly through the supervisor
    /// with no shell, so paths/arguments with spaces are representable.
    /// Additive to the legacy `pr_command`.
    pub pr_program: Option<String>,
    /// The typed argv arguments; every element substitutes
    /// `{branch}`/`{base}`/`{remote}` independently. Requires `pr_program`.
    pub pr_args: Vec<String>,
}

/// Map-only strict parsing: a positional JSON array must never configure the
/// section by position, duplicates are refused and unknown keys are typed
/// errors — the same discipline the completion contract itself uses.
impl<'de> serde::Deserialize<'de> for CompletionCfg {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error as _, MapAccess, Visitor};

        struct SectionVisitor;

        impl<'de> Visitor<'de> for SectionVisitor {
            type Value = CompletionCfg;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("the [completion] section as a JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<CompletionCfg, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut out = CompletionCfg::default();
                let mut seen: u8 = 0;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "remote" => {
                            if seen & 1 != 0 {
                                return Err(A::Error::duplicate_field("remote"));
                            }
                            seen |= 1;
                            out.remote = map.next_value()?;
                        }
                        "base_branch" => {
                            if seen & 2 != 0 {
                                return Err(A::Error::duplicate_field("base_branch"));
                            }
                            seen |= 2;
                            out.base_branch = map.next_value()?;
                        }
                        "pr_command" => {
                            if seen & 4 != 0 {
                                return Err(A::Error::duplicate_field("pr_command"));
                            }
                            seen |= 4;
                            out.pr_command = map.next_value()?;
                        }
                        "pr_program" => {
                            if seen & 8 != 0 {
                                return Err(A::Error::duplicate_field("pr_program"));
                            }
                            seen |= 8;
                            out.pr_program = map.next_value()?;
                        }
                        "pr_args" => {
                            if seen & 16 != 0 {
                                return Err(A::Error::duplicate_field("pr_args"));
                            }
                            seen |= 16;
                            out.pr_args = map.next_value()?;
                        }
                        other => {
                            return Err(A::Error::unknown_field(
                                other,
                                &[
                                    "remote",
                                    "base_branch",
                                    "pr_command",
                                    "pr_program",
                                    "pr_args",
                                ],
                            ));
                        }
                    }
                }
                Ok(out)
            }
        }

        de.deserialize_map(SectionVisitor)
    }
}

impl CompletionCfg {
    /// Resolve and STRICTLY validate the completion-step execution policy.
    /// Any invalid remote/base/template is an error: a typo never half-runs.
    pub fn steps_config(
        &self,
    ) -> Result<faktor_orchestrator::runtime::completion_steps::CompletionStepsConfig, String> {
        let config = faktor_orchestrator::runtime::completion_steps::CompletionStepsConfig {
            remote: self.remote.clone().unwrap_or_else(|| "origin".to_string()),
            base_branch: self
                .base_branch
                .clone()
                .unwrap_or_else(|| "main".to_string()),
            pr_command: self.pr_command.clone(),
            pr_program: self.pr_program.clone(),
            pr_args: self.pr_args.clone(),
        };
        config.validate().map_err(|e| format!("completion: {e}"))?;
        Ok(config)
    }
}

#[cfg(test)]
#[path = "completion_cfg_tests.rs"]
mod completion_cfg_tests;
