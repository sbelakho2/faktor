//! The additive worker-placement seam of the orchestrator.
//!
//! When the daemon's worker plane is DISABLED (the default, and the only
//! state of every pre-existing config) the seam is inert: [`WorkerPlacement`]
//! has no implementation and every placement decision is
//! [`PlacementDecision::Local`] — local execution is byte-identical to the
//! pre-worker-plane runtime.
//!
//! When ENABLED, the task executor consults the seam BEFORE any local
//! durable write of a new run. The seam (implemented by the daemon over the
//! worker plane) either:
//!
//! - returns [`PlacementDecision::Local`] (no eligible worker: the run
//!   executes locally exactly as before), or
//! - mints one IMMUTABLE job generation in the worker plane, CAS-accepts its
//!   lease and returns [`PlacementDecision::Remote`]: the run is NOT started
//!   locally; the receipt names the remote job/lease and the worker plane's
//!   durable rows own the attempt from then on (results are accepted only
//!   while that lease and generation are current).
//!
//! A seam FAILURE is a typed refusal ([`crate::runtime::ExecError::PlacementRefused`]): a
//! broken worker plane never silently turns a remote placement into a local
//! run, and never refuses a local run silently either.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// One provider-neutral placement request. The daemon's adapter maps it onto
/// the worker plane's `JobRequirements`; the orchestrator never links the
/// worker crate (dependencies still point inward).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementSpec {
    /// The idempotency identity of the run inside the organization (a
    /// replayed start maps onto the SAME immutable generation).
    pub job_key: String,
    pub organization: String,
    pub trust_domain: String,
    /// The immutable digest of the work this job represents (64 hex).
    pub payload_digest: String,
    /// The run shape: `in_session` or `orchestrated`.
    pub kind: String,
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub arch: Option<String>,
    #[serde(default)]
    pub toolchains: Vec<String>,
    #[serde(default)]
    pub sandbox: Vec<String>,
    #[serde(default)]
    pub network: Option<String>,
    #[serde(default)]
    pub min_cpu_cores: u32,
    #[serde(default)]
    pub min_memory_mb: u64,
    #[serde(default)]
    pub gpu: bool,
    #[serde(default)]
    pub region: Option<String>,
}

/// The placement decision for one new run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementDecision {
    /// Execute locally (the default; also the answer when no eligible
    /// worker exists).
    Local,
    /// The job generation was minted and leased in the worker plane.
    Remote {
        job_id: String,
        worker_id: String,
        generation: u64,
        lease_id: String,
    },
}

/// The typed refusal of one placement consultation. A seam failure is never
/// collapsed into a string: the paid `private_runners` entitlement refusal
/// ([`PlacementRefusal::Entitlement`]) keeps the entitlement, its hook, the
/// cause and the resolved plan intact, so the executor's
/// `ExecError::PlacementRefused` message names exactly why the remote
/// placement did not happen (never a silent local fallback).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlacementRefusal {
    /// The value-entitlement gate refused the remote (private-runner)
    /// placement at this boundary.
    #[error("{0}")]
    Entitlement(#[from] faktor_core::entitlement::ValueGateRefusal),
    /// The worker plane itself refused (unavailable, CAS race,
    /// requirements, ...).
    #[error("worker plane placement refused: {0}")]
    Plane(String),
}

/// The daemon-provided placement implementation (over the worker plane).
/// Object-safe: the executor holds one `Arc<dyn WorkerPlacementSeam>`.
/// Implementations MUST consult the entitlement gate (when wired) BEFORE
/// minting any job generation, so a denied placement leaves no durable job
/// behind.
pub trait WorkerPlacementSeam: Send + Sync {
    fn place(&self, spec: &PlacementSpec) -> Result<PlacementDecision, PlacementRefusal>;
}

/// The executor's placement knob: disabled by default.
#[derive(Default)]
pub struct WorkerPlacement {
    seam: Option<Arc<dyn WorkerPlacementSeam>>,
}

impl std::fmt::Debug for WorkerPlacement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerPlacement")
            .field("enabled", &self.is_enabled())
            .finish()
    }
}

impl WorkerPlacement {
    /// The disabled seam: every decision is local.
    pub fn disabled() -> Self {
        Self { seam: None }
    }

    /// The enabled seam over the daemon's worker plane.
    pub fn enabled(seam: Arc<dyn WorkerPlacementSeam>) -> Self {
        Self { seam: Some(seam) }
    }

    pub fn is_enabled(&self) -> bool {
        self.seam.is_some()
    }

    /// Consult the seam. Disabled = local, always; enabled = the seam's
    /// decision, and a seam failure is a typed refusal (never a silent
    /// fallback that would hide a broken plane or an unentitled private
    /// runner).
    pub fn place(&self, spec: &PlacementSpec) -> Result<PlacementDecision, PlacementRefusal> {
        match &self.seam {
            None => Ok(PlacementDecision::Local),
            Some(seam) => seam.place(spec),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AlwaysRemote;

    impl WorkerPlacementSeam for AlwaysRemote {
        fn place(&self, spec: &PlacementSpec) -> Result<PlacementDecision, PlacementRefusal> {
            Ok(PlacementDecision::Remote {
                job_id: format!("job_{}", spec.job_key),
                worker_id: "wrk_1".into(),
                generation: 1,
                lease_id: "lease_1".into(),
            })
        }
    }

    struct Broken;

    impl WorkerPlacementSeam for Broken {
        fn place(&self, _spec: &PlacementSpec) -> Result<PlacementDecision, PlacementRefusal> {
            Err(PlacementRefusal::Plane("worker plane unavailable".into()))
        }
    }

    struct DeniedByPlan;

    impl WorkerPlacementSeam for DeniedByPlan {
        fn place(&self, _spec: &PlacementSpec) -> Result<PlacementDecision, PlacementRefusal> {
            Err(PlacementRefusal::Entitlement(
                faktor_core::entitlement::ValueGateRefusal {
                    entitlement: faktor_core::entitlement::ValueEntitlement::PrivateRunners,
                    hook: faktor_core::entitlement::ValueEntitlement::PrivateRunners
                        .enforcement_hook(),
                    cause: faktor_core::entitlement::ValueRefusalCause::NotInPlan,
                    plan_id: Some("pro".into()),
                    organization_id: Some("org_1".into()),
                    message:
                        "value entitlement private_runners (hook worker_placement) not granted for organization org_1: not_in_plan (plan Some(\"pro\"))"
                            .into(),
                },
            ))
        }
    }

    fn spec() -> PlacementSpec {
        PlacementSpec {
            job_key: "sess-1/run-1".into(),
            organization: "org_1".into(),
            trust_domain: "org_1".into(),
            payload_digest: "a".repeat(64),
            kind: "in_session".into(),
            os: Some("linux".into()),
            arch: None,
            toolchains: vec!["rust".into()],
            sandbox: vec![],
            network: None,
            min_cpu_cores: 1,
            min_memory_mb: 1024,
            gpu: false,
            region: None,
        }
    }

    #[test]
    fn disabled_placement_is_always_local() {
        let placement = WorkerPlacement::disabled();
        assert!(!placement.is_enabled());
        assert_eq!(placement.place(&spec()).unwrap(), PlacementDecision::Local);
    }

    #[test]
    fn enabled_placement_delegates_and_a_failure_is_loud() {
        let remote = WorkerPlacement::enabled(Arc::new(AlwaysRemote));
        assert!(remote.is_enabled());
        match remote.place(&spec()).unwrap() {
            PlacementDecision::Remote {
                job_id,
                worker_id,
                generation,
                lease_id,
            } => {
                assert_eq!(job_id, "job_sess-1/run-1");
                assert_eq!(worker_id, "wrk_1");
                assert_eq!(generation, 1);
                assert_eq!(lease_id, "lease_1");
            }
            other => panic!("expected remote, got {other:?}"),
        }
        let broken = WorkerPlacement::enabled(Arc::new(Broken));
        assert!(matches!(
            broken.place(&spec()),
            Err(PlacementRefusal::Plane(ref message)) if message.contains("unavailable")
        ));

        // A paid-entitlement denial stays TYPED through the seam: the
        // entitlement, hook and plan are intact at the executor boundary.
        let denied = WorkerPlacement::enabled(Arc::new(DeniedByPlan));
        let refusal = denied.place(&spec()).unwrap_err();
        match refusal {
            PlacementRefusal::Entitlement(refusal) => {
                assert_eq!(
                    refusal.entitlement,
                    faktor_core::entitlement::ValueEntitlement::PrivateRunners
                );
                assert_eq!(refusal.hook.as_str(), "worker_placement");
                assert_eq!(refusal.plan_id.as_deref(), Some("pro"));
                assert!(refusal.to_string().contains("private_runners"));
            }
            other => panic!("expected the typed entitlement refusal, got {other:?}"),
        }
    }

    #[test]
    fn placement_spec_is_strict_on_the_wire() {
        // Break caught: a deny_unknown_fields regression that silently
        // accepts an attacker-controlled placement field, or a missing-field
        // default that mints a job with no payload digest.
        let mut value = serde_json::to_value(spec()).unwrap();
        value["unknown_field"] = serde_json::json!(1);
        let err = serde_json::from_value::<PlacementSpec>(value).unwrap_err();
        assert!(
            err.to_string().contains("unknown field `unknown_field`"),
            "{err}"
        );
        let mut missing = serde_json::to_value(spec()).unwrap();
        missing.as_object_mut().unwrap().remove("payload_digest");
        let err = serde_json::from_value::<PlacementSpec>(missing).unwrap_err();
        assert!(
            err.to_string().contains("missing field `payload_digest`"),
            "{err}"
        );
    }
}
