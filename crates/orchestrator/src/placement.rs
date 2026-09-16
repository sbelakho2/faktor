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

/// The daemon-provided placement implementation (over the worker plane).
/// Object-safe: the executor holds one `Arc<dyn WorkerPlacementSeam>`.
pub trait WorkerPlacementSeam: Send + Sync {
    fn place(&self, spec: &PlacementSpec) -> Result<PlacementDecision, String>;
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
    /// fallback that would hide a broken plane).
    pub fn place(&self, spec: &PlacementSpec) -> Result<PlacementDecision, String> {
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
        fn place(&self, spec: &PlacementSpec) -> Result<PlacementDecision, String> {
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
        fn place(&self, _spec: &PlacementSpec) -> Result<PlacementDecision, String> {
            Err("worker plane unavailable".into())
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
        assert!(broken.place(&spec()).is_err());
    }

    #[test]
    fn placement_spec_is_strict_on_the_wire() {
        let mut value = serde_json::to_value(spec()).unwrap();
        value["unknown_field"] = serde_json::json!(1);
        assert!(serde_json::from_value::<PlacementSpec>(value).is_err());
    }
}
