//! `config::verification`: schema domain of the daemon config.

/// The additive `[verification]` section (daemon verification policy).
/// Strictly additive with `serde(default)`: an absent section (or absent
/// keys inside it) keep the crate defaults (quick ≤ 60 s, unit ≤ 600 s
/// inline, full in background). `quick_max_s: 0` disables the verification
/// service entirely (fail closed — mutating turns classify Unverified).
/// Unknown keys inside the section are parse errors (strict both on the
/// lenient and the strict load path).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationCfg {
    #[serde(default = "default_quick_max_s")]
    pub quick_max_s: u64,
    #[serde(default = "default_unit_max_s")]
    pub unit_max_s: u64,
    #[serde(default = "default_full_as_background")]
    pub full_as_background: bool,
    /// Opt into the ADVANCED verification-policy profile (the
    /// `advanced_verification_policy` paid value entitlement): full
    /// repository checks run with a real inline budget instead of being
    /// silently demoted to background, and the quick/unit budgets sit at
    /// their advanced floors. Default `false` = the local profile, never
    /// gated. Requesting the advanced profile on a plan that does not map
    /// the entitlement refuses daemon startup TYPED at the policy
    /// constructor (naming the entitlement and the resolved plan) — never a
    /// silent local downgrade. Requires `quick_max_s > 0`.
    #[serde(default)]
    pub advanced: bool,
    /// The OPTIONAL external (Kiwi-shaped) verification provider. Disabled
    /// by default: an absent subsection — or `enabled = false` — leaves the
    /// verification registry with the native baseline only.
    #[serde(default)]
    pub kiwi: KiwiCfg,
}

/// The additive `[verification.kiwi]` subsection: the OPTIONAL external
/// verification provider. DISABLED by default; enabling it requires a
/// bounded http(s) endpoint, and the optional bearer token is named by
/// environment variable (the token VALUE never rides config). Bounds are the
/// kiwi crate's; a hostile subsection refuses the daemon at boot before any
/// socket exists.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KiwiCfg {
    #[serde(default)]
    pub enabled: bool,
    /// The verification endpoint; required when `enabled` is true.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Environment variable holding the bearer token (optional).
    #[serde(default)]
    pub auth_env: Option<String>,
    #[serde(default = "default_kiwi_timeout_ms")]
    pub timeout_ms: u64,
    /// The explicitly NEGOTIATED operation set. Absent = the kiwi crate's
    /// conservative default set (claim verification, contracts/compatibility,
    /// generated tests, security/performance/runtime checks); the
    /// native-owned build/lint/typecheck/tests operations are never claimed
    /// implicitly. Every operation outside the negotiated set is refused
    /// typed by the adapter before any request is sent.
    #[serde(default)]
    pub operations: Option<Vec<faktor_verify::VerificationOp>>,
}

impl Default for KiwiCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: None,
            auth_env: None,
            timeout_ms: default_kiwi_timeout_ms(),
            operations: None,
        }
    }
}

impl KiwiCfg {
    /// The bounded provider config this subsection resolves to: `None` while
    /// disabled (the default). An enabled subsection without a valid
    /// endpoint — or with any hostile field — is a loud typed refusal.
    pub fn provider_config(&self) -> Result<Option<faktor_kiwi::KiwiProviderConfig>, String> {
        if !self.enabled {
            return Ok(None);
        }
        let endpoint = self
            .endpoint
            .clone()
            .filter(|endpoint| !endpoint.is_empty())
            .ok_or_else(|| "verification.kiwi.enabled requires an endpoint".to_string())?;
        let config = faktor_kiwi::KiwiProviderConfig {
            endpoint,
            auth_env: self.auth_env.clone(),
            timeout_ms: self.timeout_ms,
            operations: self.operations.clone(),
        };
        config
            .validate()
            .map_err(|error| format!("verification.kiwi: {error}"))?;
        Ok(Some(config))
    }
}

fn default_kiwi_timeout_ms() -> u64 {
    faktor_kiwi::DEFAULT_KIWI_TIMEOUT_MS
}

pub(crate) fn default_quick_max_s() -> u64 {
    60
}

pub(crate) fn default_unit_max_s() -> u64 {
    600
}

pub(crate) fn default_full_as_background() -> bool {
    true
}

impl Default for VerificationCfg {
    fn default() -> Self {
        Self {
            quick_max_s: default_quick_max_s(),
            unit_max_s: default_unit_max_s(),
            full_as_background: default_full_as_background(),
            advanced: false,
            kiwi: KiwiCfg::default(),
        }
    }
}

impl VerificationCfg {
    /// The verification policy this section resolves to WITHOUT any value
    /// gate. A numeric combination that reaches the ADVANCED (paid) profile's
    /// depth — `quick_max_s`/`unit_max_s` at the advanced floors, or
    /// `full_as_background = false` (full checks inline) — refuses typed
    /// here: the advanced profile is reachable ONLY through
    /// [`Self::policy_checked`] with the `advanced_verification_policy`
    /// entitlement. `quick_max_s == 0` stays `Ok(None)` (disabled, never
    /// advanced). The default local profile (60/600/background) is never
    /// refused.
    pub fn policy(&self) -> Result<Option<faktor_verify::exec::VerificationPolicy>, String> {
        self.local_policy()
    }

    /// The daemon's GATED resolution: `advanced = true` OR any numerically
    /// implied-advanced combination constructs the advanced profile ONLY
    /// through the checked constructor; a denial is the typed refusal
    /// (naming the entitlement, its hook, the cause and the resolved plan).
    pub fn policy_checked(
        &self,
        gate: &faktor_core::entitlement::ValueGate,
    ) -> Result<Option<faktor_verify::exec::VerificationPolicy>, String> {
        if self.quick_max_s == 0 {
            return Ok(None);
        }
        if self.advanced || self.implies_advanced() {
            return faktor_verify::exec::VerificationPolicy::advanced_checked(gate)
                .map(Some)
                .map_err(|refusal| format!("verification advanced profile refused: {refusal}"));
        }
        self.local_policy()
    }

    /// TRUE when the configured numbers (without `advanced = true`) already
    /// reach the advanced profile's depth: they still require the
    /// entitlement. Pure numeric classification (`quick_max_s == 0` is the
    /// disabled service, never advanced).
    pub fn implies_advanced(&self) -> bool {
        if self.quick_max_s == 0 {
            return false;
        }
        faktor_verify::exec::VerificationPolicy::implies_advanced_params(
            std::time::Duration::from_secs(self.quick_max_s),
            std::time::Duration::from_secs(self.unit_max_s),
            self.full_as_background,
        )
    }

    /// The pure local projection (never gated, never advanced by itself):
    /// `None` when `quick_max_s` is 0 (the verification service is DISABLED,
    /// fail closed — mutating turns classify Unverified, never silently
    /// complete). The policy is built through the checked LOCAL constructor,
    /// so an advanced-depth combination refuses typed instead of yielding an
    /// ungated paid profile.
    fn local_policy(&self) -> Result<Option<faktor_verify::exec::VerificationPolicy>, String> {
        if self.quick_max_s == 0 {
            return Ok(None);
        }
        faktor_verify::exec::VerificationPolicy::local_checked(
            std::time::Duration::from_secs(self.quick_max_s),
            std::time::Duration::from_secs(self.unit_max_s),
            self.full_as_background,
            std::time::Duration::from_secs(5),
        )
        .map(Some)
        .map_err(|refusal| {
            format!(
                "verification.quick_max_s/unit_max_s/full_as_background: {refusal}; set \
                 [verification] advanced = true with a plan that maps the entitlement"
            )
        })
    }
}
