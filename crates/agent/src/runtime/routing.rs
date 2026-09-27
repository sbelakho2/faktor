//! `runtime::routing`: cohesive slice of the agent runtime.

use super::*;

/// True when a semantic risk level escalates risk-driven decisions (audits
/// 54/118/119): `High` (positive evidence of risk) and `Unknown` (no
/// trustworthy evidence) both escalate; Unknown is NEVER treated as Safe.
/// `None` (no provider consulted) never escalates — parity.
pub fn semantic_risk_escalates(level: Option<RiskLevel>) -> bool {
    matches!(level, Some(RiskLevel::High | RiskLevel::Unknown))
}

/// Map provider-reported affected entity paths to a [`SemanticRisk`] (audit
/// 54/57): every axis starts Unknown and only the provider's DATA moves one.
/// A degraded answer stays all-Unknown — never Safe. `Unknown` poisons the
/// STRICT join; the level is taken under `ALLOW_UNKNOWN` ONLY because the
/// successful provider response is the positive evidence the axes describe.
pub(crate) fn semantic_risk_from_paths(paths: &[String], degraded: bool) -> SemanticRisk {
    if degraded {
        return SemanticRisk::unknown();
    }
    let mut risk = SemanticRisk::unknown();
    risk.verification_gap = RiskLevel::Safe;
    risk.blast_radius = match paths.len() {
        0 => RiskLevel::Safe,
        1..=4 => RiskLevel::Low,
        5..=19 => RiskLevel::Medium,
        _ => RiskLevel::High,
    };
    for path in paths {
        let p = path.to_lowercase();
        if p.contains("auth")
            || p.contains("security")
            || p.contains("secret")
            || p.contains("credential")
            || p.contains("crypto")
        {
            risk.security_delta = RiskLevel::High;
        }
        if p.contains("unsafe") || p.contains("ffi") {
            risk.unsafe_delta = RiskLevel::High;
        }
        if p.contains("thread")
            || p.contains("async")
            || p.contains("lock")
            || p.contains("concurren")
            || p.contains("parallel")
        {
            risk.concurrency_delta = RiskLevel::High;
        }
        if p.contains("public") || p.contains("contract") || p.contains("proto") {
            risk.public_surface_delta = RiskLevel::High;
        }
        if p.contains("budget") || p.contains("billing") || p.contains("cost") {
            risk.resource_constraint_delta = RiskLevel::High;
        }
        if p.contains("network") || p.contains("egress") || p.contains("socket") {
            risk.external_effect_delta = RiskLevel::High;
        }
        if p.contains("process")
            || p.contains("command")
            || p.contains("sandbox")
            || p.contains("permission")
        {
            risk.capability_delta = RiskLevel::High;
        }
        if p.contains("session")
            || p.contains("store")
            || p.contains("persist")
            || p.contains("checkpoint")
            || p.contains("migration")
        {
            risk.contract_delta = RiskLevel::High;
        }
    }
    risk
}

/// The conservative level of a provider assessment: degraded is Unknown,
/// otherwise the worst axis under the allow-unknown policy (which still
/// yields Unknown for an all-unknown assessment).
pub(crate) fn semantic_risk_level(risk: &SemanticRisk, degraded: bool) -> RiskLevel {
    if degraded {
        return RiskLevel::Unknown;
    }
    risk.worst(&RiskPolicy::ALLOW_UNKNOWN)
}

/// Provider DATA can only ever REMOVE capability classes (audit 58): only an
/// explicit provider `High` on a capability-relevant axis restricts; absent
/// axes never restrict (absence is not evidence). The result is the set the
/// provider permits and is intersected by [`effective_capabilities`].
pub(crate) fn semantic_capability_restrictions(risk: &SemanticRisk) -> faktor_core::CapabilitySet {
    let high = |level: RiskLevel| level == RiskLevel::High;
    let mut kinds: Vec<faktor_core::CapabilityKind> = faktor_core::CapabilityKind::ALL.to_vec();
    if high(risk.external_effect_delta) {
        kinds.retain(|k| *k != faktor_core::CapabilityKind::Network);
    }
    if high(risk.capability_delta) {
        kinds.retain(|k| {
            !matches!(
                k,
                faktor_core::CapabilityKind::Execute | faktor_core::CapabilityKind::Mcp
            )
        });
    }
    faktor_core::CapabilitySet::from_kinds(&kinds)
}

/// The agent-gate effective capability set (audit 58): session envelope ∩
/// task policy ∩ provider semantic restrictions, composed through the
/// semantic crate's [`faktor_semantic::capability_intersection`] so provider
/// data can only ever REDUCE the set. Never grants: the result is always a
/// subset of `parent`.
pub fn effective_capabilities(
    parent: faktor_core::CapabilitySet,
    task_policy: faktor_core::CapabilitySet,
    semantic_restrictions: faktor_core::CapabilitySet,
) -> faktor_core::CapabilitySet {
    faktor_semantic::capability_intersection(
        parent.intersection(task_policy),
        semantic_restrictions,
    )
}

/// The session's sandbox envelope as a typed capability set: a class is
/// present unless the policy DENIES it (`Ask` still reaches the interactive
/// permission hop). No sandbox policy = the full set (today's behavior).
pub(crate) fn sandbox_capability_envelope(
    policy: Option<&faktor_sandbox::SandboxPolicy>,
) -> faktor_core::CapabilitySet {
    let Some(policy) = policy else {
        return faktor_core::CapabilitySet::ALL;
    };
    use faktor_core::CapabilityKind;
    let mut kinds: Vec<CapabilityKind> = Vec::new();
    if policy.read_workspace != faktor_sandbox::Rule::Deny
        || policy.read_external != faktor_sandbox::Rule::Deny
    {
        kinds.push(CapabilityKind::Read);
    }
    if policy.write_workspace != faktor_sandbox::Rule::Deny
        || policy.write_external != faktor_sandbox::Rule::Deny
    {
        kinds.push(CapabilityKind::Write);
    }
    if policy.execute_shell != faktor_sandbox::Rule::Deny {
        kinds.push(CapabilityKind::Execute);
    }
    // A network gate is destination-scoped, never class-denied here: the
    // installed gate itself refuses destinations, so the class stays.
    kinds.push(CapabilityKind::Network);
    if policy.mcp != faktor_sandbox::Rule::Deny {
        kinds.push(CapabilityKind::Mcp);
    }
    if policy.git != faktor_sandbox::Rule::Deny {
        kinds.push(CapabilityKind::Git);
    }
    faktor_core::CapabilitySet::from_kinds(&kinds)
}

/// The capability class of one concrete tool request.
pub(crate) fn capability_kind_of(capability: &Capability) -> faktor_core::CapabilityKind {
    use faktor_core::CapabilityKind;
    match capability {
        Capability::ReadWorkspace { .. } | Capability::ReadExternal { .. } => CapabilityKind::Read,
        Capability::WriteWorkspace { .. } | Capability::WriteExternal { .. } => {
            CapabilityKind::Write
        }
        Capability::ExecuteShell { .. } => CapabilityKind::Execute,
        Capability::Network { .. } => CapabilityKind::Network,
        Capability::Mcp { .. } => CapabilityKind::Mcp,
        Capability::Git { .. } => CapabilityKind::Git,
    }
}

/// Risk-driven tool-batch parallelism reduction (audit 54). `None` = no
/// provider consulted: the scheduler defaults, byte-identical. `High`/
/// `Unknown` serialize every class to at most one op in flight; `Medium`
/// clamps the wide classes; `Safe`/`Low` keep the defaults.
pub fn risk_adjusted_resource_limits(
    risk: Option<RiskLevel>,
) -> faktor_core::resource::ResourceLimits {
    use faktor_core::resource::ResourceClass;
    let mut limits = faktor_core::resource::ResourceLimits::default();
    let Some(level) = risk else {
        return limits;
    };
    let clamp = |limits: &mut faktor_core::resource::ResourceLimits, class, max: usize| {
        let current = limits.get(class);
        limits.limits.insert(class, current.min(max));
    };
    match level {
        RiskLevel::High | RiskLevel::Unknown => {
            for class in ResourceClass::ALL {
                limits.limits.insert(class, 1);
            }
        }
        RiskLevel::Medium => {
            clamp(&mut limits, ResourceClass::DiskRead, 4);
            clamp(&mut limits, ResourceClass::DiskWrite, 2);
            clamp(&mut limits, ResourceClass::Network, 2);
            clamp(&mut limits, ResourceClass::Cpu, 1);
            clamp(&mut limits, ResourceClass::Terminal, 1);
            clamp(&mut limits, ResourceClass::Mcp, 1);
        }
        RiskLevel::Safe | RiskLevel::Low => {}
    }
    limits
}

/// The runtime's own semantic-risk → verified-outcome risk bucket mapping
/// (audit items 13/14/L): a risk dimension of the outcome registry must be
/// decided by the runtime's risk model at settlement time — never inferred
/// from provider/model names. The model call intent's semantic risk scores
/// escalate when stalled evidence forces riskier continuation (10 = ordinary
/// implement iteration, 70 = stalled-evidence escalation); the buckets are
/// the coarse registry dimension.
pub(crate) fn risk_bucket_of(semantic_risk: u8) -> RiskBucket {
    match semantic_risk {
        r if r < 50 => RiskBucket::Low,
        r if r < 80 => RiskBucket::Medium,
        _ => RiskBucket::High,
    }
}

#[derive(Debug, Clone)]
pub struct AgentCard {
    pub session_id: SessionId,
    pub title: String,
    pub status: String, // running | waiting | completed | failed | needs-input
}

impl AgentRuntime {
    /// The ONE semantic-provider registry this runtime consults (audits
    /// 48-54/58/79): the daemon graph builds it once and hands the SAME
    /// `Arc` to [`AgentDeps`] and the server surface, so the native
    /// introspection endpoints can never report a parallel registry.
    /// Conformance tests assert pointer identity through this accessor.
    pub fn semantic_registry(&self) -> &Arc<faktor_semantic::SemanticProviderRegistry> {
        &self.deps.semantic
    }

    /// Drive the typed task row across the machine's legal edges to
    /// `target`, re-reading the row's expected revision before EVERY
    /// transition (a concurrent writer between edges refuses with the typed
    /// RevisionMismatch — never a blind overwrite). Terminal rows and pairs
    /// the machine cannot connect (e.g. NeedsVerification/Verifying have no
    /// edge to Blocked) error loudly: callers plan through
    /// [`task_route`] first.
    pub(crate) fn route_task_to(
        &self,
        handle: &faktor_session::SessionHandle,
        task_id: TaskId,
        target: TaskState,
    ) -> faktor_core::Result<Task> {
        let task = handle
            .get_task(task_id)?
            .ok_or_else(|| Error::not_found(format!("task {task_id}")))?;
        if task.state == target {
            return Ok(task);
        }
        let Some(route) = task_route(task.state, target) else {
            return Err(Error::conflict(format!(
                "task {task_id} at {:?} cannot reach {target:?} on the state machine",
                task.state
            )));
        };
        let mut row = task;
        for edge in route {
            let rev = handle.task_revision(task_id)?;
            row = handle.transition_task(task_id, rev, edge, None)?;
        }
        Ok(row)
    }

    pub(crate) fn provider_for(
        &self,
        handle: &faktor_session::SessionHandle,
    ) -> faktor_core::Result<Arc<dyn faktor_provider::Provider>> {
        let provider_id = handle.provider()?;
        self.deps
            .providers
            .get(&provider_id)
            .ok_or_else(|| Error::not_found(format!("provider {provider_id} not registered")))
    }
}
