//! The worker-plane domain model: workers and their versioned capability
//! advertisements, immutable job generations, leases with heartbeat
//! intervals, bounded attempts, landed results and the append-only journal.
//!
//! Invariants encoded here (not by convention):
//!
//! - a [`JobGeneration`] is minted by the scheduler and NEVER renumbered:
//!   [`JobGenerationRow`] rows are append-only, and a lease/result always
//!   names the exact generation it was accepted under;
//! - a [`RequeuePolicy`] is bounded (`1..=MAX_REQUEUE_ATTEMPTS`), so worker
//!   loss can never create an unbounded attempt loop;
//! - capability matching is a pure function ([`WorkerCapabilities::satisfies`])
//!   over ADVERTISED capabilities: a worker can only lease a job whose
//!   organization AND trust domain equal its own, and whose requirements it
//!   demonstrably meets.

use serde::{Deserialize, Serialize};

use crate::error::{WorkerError, WORKER_PROTOCOL_VERSION};
use crate::ids::{ExecutionJobId, JobGeneration, JobKey, WorkerId, WorkerLeaseId};

/// Bound on one advertised capability string (os/arch/region/toolchain/tag).
pub const MAX_CAPABILITY_BYTES: usize = 128;
/// Bound on the advertised toolchain list of one worker.
pub const MAX_TOOLCHAINS: usize = 64;
/// Bound on the advertised sandbox-capability list of one worker.
pub const MAX_SANDBOX_CAPABILITIES: usize = 32;
/// Bound on the display name of one worker.
pub const MAX_WORKER_NAME_BYTES: usize = 128;
/// Bound on the result digest (sha256 hex) carried by one result.
pub const RESULT_DIGEST_BYTES: usize = 64;
/// Bound on one journal detail string.
pub const MAX_JOURNAL_DETAIL_BYTES: usize = 512;

/// Lower bound of one lease heartbeat interval.
pub const HEARTBEAT_MIN_INTERVAL_MS: i64 = 1_000;
/// Upper bound of one lease heartbeat interval (bounded staleness: a lease
/// can never be renewed into an unbounded lifetime).
pub const HEARTBEAT_MAX_INTERVAL_MS: i64 = 300_000;
/// Default heartbeat interval of one accepted lease.
pub const HEARTBEAT_DEFAULT_INTERVAL_MS: i64 = 15_000;
/// The maximum number of attempts one bounded requeue policy may declare.
pub const MAX_REQUEUE_ATTEMPTS: u32 = 32;
/// The default bounded requeue policy (three attempts).
pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;
/// How many live leases one worker may hold at once (bounded concurrency).
pub const MAX_LIVE_LEASES_PER_WORKER: usize = 16;

/// One normalized sandbox capability tag advertised/required by a worker.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SandboxCapability(String);

impl SandboxCapability {
    pub fn try_new(raw: impl Into<String>) -> Result<Self, WorkerError> {
        let raw = normalize_token(&raw.into(), "sandbox capability")?;
        Ok(Self(raw))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SandboxCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn normalize_token(raw: &str, what: &str) -> Result<String, WorkerError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_CAPABILITY_BYTES {
        return Err(WorkerError::Malformed(format!(
            "{what} must be 1..={MAX_CAPABILITY_BYTES} bytes"
        )));
    }
    if !trimmed.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(WorkerError::Malformed(format!(
            "{what} must be printable ASCII without whitespace"
        )));
    }
    Ok(trimmed.to_ascii_lowercase())
}

/// The network profile of one worker (and the minimum one job may require).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkProfile {
    /// No network access at all.
    None,
    /// Egress through a restricted allowlist.
    EgressRestricted,
    /// Unrestricted network access.
    Full,
}

impl NetworkProfile {
    pub const fn as_str(self) -> &'static str {
        match self {
            NetworkProfile::None => "none",
            NetworkProfile::EgressRestricted => "egress_restricted",
            NetworkProfile::Full => "full",
        }
    }
}

/// One GPU advertisement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpuCapability {
    pub model: String,
    pub count: u32,
    pub memory_mb: u64,
}

/// The versioned capability advertisement of one worker. Registration
/// normalizes (trim/lowercase/sort/dedupe) the list fields, so two byte-
/// different spellings of the same advertisement reconcile to one version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerCapabilities {
    pub os: String,
    pub arch: String,
    pub toolchains: Vec<String>,
    pub sandbox: Vec<SandboxCapability>,
    pub network: NetworkProfile,
    pub cpu_cores: u32,
    pub memory_mb: u64,
    #[serde(default)]
    pub gpu: Option<GpuCapability>,
    pub region: String,
    /// The worker's trust domain (must equal the registration token's; a
    /// worker can only ever lease jobs of its own trust domain).
    pub trust_domain: String,
    /// The worker protocol version the advertisement was made under.
    pub protocol_version: u32,
}

impl WorkerCapabilities {
    /// Normalize the advertisement: trim/lowercase the scalar fields, sort
    /// and dedupe the toolchains and sandbox tags, bound every list.
    pub fn normalize(&mut self) -> Result<(), WorkerError> {
        self.os = normalize_token(&self.os, "os")?;
        self.arch = normalize_token(&self.arch, "arch")?;
        self.region = normalize_token(&self.region, "region")?;
        self.trust_domain = normalize_token(&self.trust_domain, "trust domain")?;
        if self.gpu.is_some() && self.cpu_cores == 0 {
            return Err(WorkerError::Malformed(
                "a GPU-advertising worker must advertise at least one CPU core".into(),
            ));
        }
        if self.toolchains.len() > MAX_TOOLCHAINS {
            return Err(WorkerError::Malformed(format!(
                "at most {MAX_TOOLCHAINS} toolchains may be advertised"
            )));
        }
        if self.sandbox.len() > MAX_SANDBOX_CAPABILITIES {
            return Err(WorkerError::Malformed(format!(
                "at most {MAX_SANDBOX_CAPABILITIES} sandbox capabilities may be advertised"
            )));
        }
        let mut toolchains = Vec::with_capacity(self.toolchains.len());
        for tool in &self.toolchains {
            toolchains.push(normalize_token(tool, "toolchain")?);
        }
        toolchains.sort();
        toolchains.dedup();
        self.toolchains = toolchains;
        let mut sandbox = Vec::with_capacity(self.sandbox.len());
        for tag in &self.sandbox {
            sandbox.push(SandboxCapability::try_new(tag.as_str().to_string())?);
        }
        sandbox.sort();
        sandbox.dedup();
        self.sandbox = sandbox;
        if self.protocol_version != WORKER_PROTOCOL_VERSION {
            return Err(WorkerError::ProtocolSkew {
                expected: WORKER_PROTOCOL_VERSION,
                got: self.protocol_version,
            });
        }
        if let Some(gpu) = &self.gpu {
            if gpu.count == 0 {
                return Err(WorkerError::Malformed(
                    "a GPU advertisement must carry at least one device".into(),
                ));
            }
            normalize_token(&gpu.model, "gpu model")?;
        }
        Ok(())
    }

    /// Whether this ADVERTISED capability set satisfies one job's
    /// requirements. Pure and total: the reason is a human string, the
    /// decision is the `Result`.
    pub fn satisfies(&self, req: &JobRequirements) -> Result<(), String> {
        if let Some(os) = &req.os {
            if &self.os != os {
                return Err(format!("requires os {os}, worker advertises {}", self.os));
            }
        }
        if let Some(arch) = &req.arch {
            if &self.arch != arch {
                return Err(format!(
                    "requires arch {arch}, worker advertises {}",
                    self.arch
                ));
            }
        }
        for tool in &req.toolchains {
            if !self.toolchains.contains(tool) {
                return Err(format!("requires toolchain {tool}"));
            }
        }
        for tag in &req.sandbox {
            if !self.sandbox.contains(tag) {
                return Err(format!("requires sandbox capability {tag}"));
            }
        }
        if let Some(network) = req.network {
            if self.network != network {
                return Err(format!(
                    "requires network profile {}, worker advertises {}",
                    network.as_str(),
                    self.network.as_str()
                ));
            }
        }
        if self.cpu_cores < req.min_cpu_cores {
            return Err(format!(
                "requires {} cpu cores, worker advertises {}",
                req.min_cpu_cores, self.cpu_cores
            ));
        }
        if self.memory_mb < req.min_memory_mb {
            return Err(format!(
                "requires {} MiB memory, worker advertises {}",
                req.min_memory_mb, self.memory_mb
            ));
        }
        if req.gpu && self.gpu.is_none() {
            return Err("requires a GPU, worker advertises none".into());
        }
        if let Some(region) = &req.region {
            if &self.region != region {
                return Err(format!(
                    "requires region {region}, worker advertises {}",
                    self.region
                ));
            }
        }
        Ok(())
    }
}

/// One durable worker registration row. `token_hash` is the ONLY form the
/// registration token is stored in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerRegistration {
    pub worker_id: WorkerId,
    pub organization_id: String,
    pub trust_domain: String,
    pub display_name: String,
    pub token_hash: String,
    pub revoked: bool,
    pub capabilities: WorkerCapabilities,
    /// The capability advertisement version: starts at 1, increments by
    /// exactly one whenever a re-registration changes the advertisement.
    pub capabilities_version: u64,
    pub registered_ms: i64,
    pub last_seen_ms: i64,
    pub revoked_ms: Option<i64>,
}

/// One worker registration token row (the plaintext is never stored).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerTokenRow {
    pub token_hash: String,
    pub organization_id: String,
    pub trust_domain: String,
    pub label: String,
    pub created_ms: i64,
    pub revoked: bool,
    /// `Some(worker_id)` once the token has registered exactly one worker.
    pub consumed_by: Option<String>,
}

/// The immutable requirements of one job generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobRequirements {
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub arch: Option<String>,
    #[serde(default)]
    pub toolchains: Vec<String>,
    #[serde(default)]
    pub sandbox: Vec<SandboxCapability>,
    #[serde(default)]
    pub network: Option<NetworkProfile>,
    #[serde(default)]
    pub min_cpu_cores: u32,
    #[serde(default)]
    pub min_memory_mb: u64,
    #[serde(default)]
    pub gpu: bool,
    #[serde(default)]
    pub region: Option<String>,
    pub trust_domain: String,
}

impl JobRequirements {
    pub fn normalize(&mut self) -> Result<(), WorkerError> {
        if let Some(os) = &self.os {
            self.os = Some(normalize_token(os, "os")?);
        }
        if let Some(arch) = &self.arch {
            self.arch = Some(normalize_token(arch, "arch")?);
        }
        if let Some(region) = &self.region {
            self.region = Some(normalize_token(region, "region")?);
        }
        self.trust_domain = normalize_token(&self.trust_domain, "trust domain")?;
        if self.toolchains.len() > MAX_TOOLCHAINS {
            return Err(WorkerError::Malformed(format!(
                "at most {MAX_TOOLCHAINS} required toolchains"
            )));
        }
        let mut toolchains = Vec::with_capacity(self.toolchains.len());
        for tool in &self.toolchains {
            toolchains.push(normalize_token(tool, "toolchain")?);
        }
        toolchains.sort();
        toolchains.dedup();
        self.toolchains = toolchains;
        if self.sandbox.len() > MAX_SANDBOX_CAPABILITIES {
            return Err(WorkerError::Malformed(format!(
                "at most {MAX_SANDBOX_CAPABILITIES} required sandbox capabilities"
            )));
        }
        let mut sandbox = Vec::with_capacity(self.sandbox.len());
        for tag in &self.sandbox {
            sandbox.push(SandboxCapability::try_new(tag.as_str().to_string())?);
        }
        sandbox.sort();
        sandbox.dedup();
        self.sandbox = sandbox;
        Ok(())
    }
}

/// The bounded requeue policy of one job: how many attempts a lost/expired
/// lease may consume before the job lands in a terminal failed state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequeuePolicy {
    pub max_attempts: u32,
}

impl Default for RequeuePolicy {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_MAX_ATTEMPTS,
        }
    }
}

impl RequeuePolicy {
    pub fn validate(&self) -> Result<(), WorkerError> {
        if self.max_attempts == 0 || self.max_attempts > MAX_REQUEUE_ATTEMPTS {
            return Err(WorkerError::Malformed(format!(
                "requeue max_attempts must be 1..={MAX_REQUEUE_ATTEMPTS}"
            )));
        }
        Ok(())
    }
}

/// The state of one execution job (the job ROOT; per-generation state lives
/// in [`JobGenerationRow::state`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// A generation is assigned and may be leased.
    Assigned,
    /// A live lease exists for the current generation.
    Leased,
    /// A worker result landed for the current generation.
    Completed,
    /// Terminal: attempts exhausted or explicitly failed.
    Failed,
}

impl JobState {
    pub const fn as_str(self) -> &'static str {
        match self {
            JobState::Assigned => "assigned",
            JobState::Leased => "leased",
            JobState::Completed => "completed",
            JobState::Failed => "failed",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, JobState::Completed | JobState::Failed)
    }
}

/// One job ROOT row: identity, immutable requirements, the current
/// generation pointer and the terminal state. `current_generation` only ever
/// advances (a requeue); it never rewinds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionJob {
    pub job_id: ExecutionJobId,
    pub organization_id: String,
    pub trust_domain: String,
    pub job_key: JobKey,
    pub requirements: JobRequirements,
    /// The immutable digest of the work this job represents.
    pub payload_digest: String,
    pub requeue: RequeuePolicy,
    /// The worker the scheduler PREFERS (assignment); `None` = open
    /// assignment, where the first eligible worker's CAS accept wins.
    #[serde(default)]
    pub assigned_worker: Option<WorkerId>,
    pub created_ms: i64,
    pub current_generation: JobGeneration,
    pub state: JobState,
}

/// The state of one immutable generation row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationState {
    Assigned,
    Leased,
    Completed,
    Lost,
    /// Superseded by a newer generation after a lost lease.
    Superseded,
    Failed,
}

impl GenerationState {
    pub const fn as_str(self) -> &'static str {
        match self {
            GenerationState::Assigned => "assigned",
            GenerationState::Leased => "leased",
            GenerationState::Completed => "completed",
            GenerationState::Lost => "lost",
            GenerationState::Superseded => "superseded",
            GenerationState::Failed => "failed",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            GenerationState::Completed
                | GenerationState::Lost
                | GenerationState::Superseded
                | GenerationState::Failed
        )
    }
}

/// One immutable job generation row (append-only in the store).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobGenerationRow {
    pub job_id: ExecutionJobId,
    pub organization_id: String,
    pub generation: JobGeneration,
    pub state: GenerationState,
    pub created_ms: i64,
    pub ended_ms: Option<i64>,
    pub reason: Option<String>,
}

/// One lease of one job generation by one worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseState {
    /// Renewable; results are accepted only while `Live` and current.
    Live,
    Expired,
    /// A newer generation exists; any result under this lease is stale.
    Superseded,
    /// The worker was revoked.
    Revoked,
    /// A result landed under this lease.
    Completed,
}

impl LeaseState {
    pub const fn as_str(self) -> &'static str {
        match self {
            LeaseState::Live => "live",
            LeaseState::Expired => "expired",
            LeaseState::Superseded => "superseded",
            LeaseState::Revoked => "revoked",
            LeaseState::Completed => "completed",
        }
    }
}

/// One durable lease row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerLease {
    pub lease_id: WorkerLeaseId,
    pub organization_id: String,
    pub job_id: ExecutionJobId,
    pub generation: JobGeneration,
    pub worker_id: WorkerId,
    pub state: LeaseState,
    pub heartbeat_interval_ms: i64,
    pub accepted_ms: i64,
    pub last_heartbeat_ms: i64,
    pub expires_at_ms: i64,
    pub ended_ms: Option<i64>,
    pub reason: Option<String>,
}

impl WorkerLease {
    pub fn is_live(&self) -> bool {
        self.state == LeaseState::Live
    }

    pub fn is_expired_at(&self, now_ms: i64) -> bool {
        now_ms >= self.expires_at_ms
    }
}

/// The state of one attempt at one job generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState {
    Pending,
    Leased,
    Completed,
    Lost,
    Failed,
}

impl AttemptState {
    pub const fn as_str(self) -> &'static str {
        match self {
            AttemptState::Pending => "pending",
            AttemptState::Leased => "leased",
            AttemptState::Completed => "completed",
            AttemptState::Lost => "lost",
            AttemptState::Failed => "failed",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            AttemptState::Completed | AttemptState::Lost | AttemptState::Failed
        )
    }
}

/// One durable attempt row. `(job_id, generation, attempt)` is the primary
/// key: an attempt is journaled exactly once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobAttempt {
    pub job_id: ExecutionJobId,
    pub generation: JobGeneration,
    pub attempt: u32,
    #[serde(default)]
    pub worker_id: Option<WorkerId>,
    #[serde(default)]
    pub lease_id: Option<WorkerLeaseId>,
    pub state: AttemptState,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
    pub reason: Option<String>,
}

/// The outcome one worker reports for one generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobResultOutcome {
    Succeeded,
    Failed,
}

/// One landed result. `(job_id, generation)` is unique: a duplicate
/// acceptance is structurally impossible to land twice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobResult {
    pub job_id: ExecutionJobId,
    pub generation: JobGeneration,
    pub worker_id: WorkerId,
    pub lease_id: WorkerLeaseId,
    pub digest: String,
    pub outcome: JobResultOutcome,
    pub accepted_ms: i64,
}

/// The append-only journal of the worker plane: every registration change,
/// lease transition and refusal is recorded. `seq` is the durable order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalEntry {
    pub seq: i64,
    pub organization_id: String,
    pub kind: String,
    #[serde(default)]
    pub job_id: Option<String>,
    #[serde(default)]
    pub worker_id: Option<String>,
    #[serde(default)]
    pub generation: Option<JobGeneration>,
    pub at_ms: i64,
    #[serde(default)]
    pub detail: String,
}

impl JournalEntry {
    pub fn new(
        organization_id: &str,
        kind: &str,
        at_ms: i64,
        detail: impl Into<String>,
    ) -> Result<Self, WorkerError> {
        let detail = detail.into();
        if detail.len() > MAX_JOURNAL_DETAIL_BYTES {
            return Err(WorkerError::Malformed(format!(
                "journal detail exceeds {MAX_JOURNAL_DETAIL_BYTES} bytes"
            )));
        }
        Ok(Self {
            seq: 0,
            organization_id: organization_id.to_string(),
            kind: kind.to_string(),
            job_id: None,
            worker_id: None,
            generation: None,
            at_ms,
            detail,
        })
    }

    pub fn with_job(mut self, job: &ExecutionJobId, generation: JobGeneration) -> Self {
        self.job_id = Some(job.to_string());
        self.generation = Some(generation);
        self
    }

    pub fn with_worker(mut self, worker: &WorkerId) -> Self {
        self.worker_id = Some(worker.to_string());
        self
    }
}

/// Clamp one requested heartbeat interval into the bounded staleness window.
pub fn clamp_heartbeat_interval(requested_ms: i64) -> i64 {
    requested_ms.clamp(HEARTBEAT_MIN_INTERVAL_MS, HEARTBEAT_MAX_INTERVAL_MS)
}

/// A public worker view (never carries the token hash).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerView {
    pub worker_id: WorkerId,
    pub organization_id: String,
    pub trust_domain: String,
    pub display_name: String,
    pub revoked: bool,
    pub capabilities: WorkerCapabilities,
    pub capabilities_version: u64,
    pub registered_ms: i64,
    pub last_seen_ms: i64,
}

impl From<&WorkerRegistration> for WorkerView {
    fn from(r: &WorkerRegistration) -> Self {
        Self {
            worker_id: r.worker_id.clone(),
            organization_id: r.organization_id.clone(),
            trust_domain: r.trust_domain.clone(),
            display_name: r.display_name.clone(),
            revoked: r.revoked,
            capabilities: r.capabilities.clone(),
            capabilities_version: r.capabilities_version,
            registered_ms: r.registered_ms,
            last_seen_ms: r.last_seen_ms,
        }
    }
}

/// One page of worker rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerPage {
    pub items: Vec<WorkerView>,
    pub next_cursor: Option<String>,
}

/// The full durable status of one job: root, generations, attempts, the
/// live/most-recent lease and the landed result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobStatus {
    pub job: ExecutionJob,
    pub generations: Vec<JobGenerationRow>,
    pub attempts: Vec<JobAttempt>,
    pub lease: Option<WorkerLease>,
    pub result: Option<JobResult>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps() -> WorkerCapabilities {
        WorkerCapabilities {
            os: "Linux".into(),
            arch: "X86_64".into(),
            toolchains: vec!["Rust".into(), "rust".into(), "Node".into()],
            sandbox: vec![
                SandboxCapability::try_new("Seccomp").unwrap(),
                SandboxCapability::try_new("seccomp").unwrap(),
                SandboxCapability::try_new("fs_jail").unwrap(),
            ],
            network: NetworkProfile::EgressRestricted,
            cpu_cores: 8,
            memory_mb: 16_384,
            gpu: Some(GpuCapability {
                model: "A100".into(),
                count: 1,
                memory_mb: 40_960,
            }),
            region: "Eu-West".into(),
            trust_domain: "org_1".into(),
            protocol_version: WORKER_PROTOCOL_VERSION,
        }
    }

    #[test]
    fn capability_normalization_is_stable() {
        let mut a = caps();
        a.normalize().unwrap();
        let mut b = a.clone();
        b.toolchains.reverse();
        b.normalize().unwrap();
        assert_eq!(a, b, "advertisement order never changes the version");
        assert_eq!(a.toolchains, vec!["node", "rust"]);
        assert_eq!(a.sandbox.len(), 2);
        assert_eq!(a.os, "linux");
    }

    #[test]
    fn protocol_skew_is_typed_during_normalization() {
        let mut c = caps();
        c.protocol_version = WORKER_PROTOCOL_VERSION + 1;
        let err = c.normalize().unwrap_err();
        assert!(matches!(err, WorkerError::ProtocolSkew { got, .. } if got == 2));
    }

    #[test]
    fn requirement_matching_is_exact_on_trust_relevant_fields() {
        let mut c = caps();
        c.normalize().unwrap();
        let mut req = JobRequirements {
            os: Some("linux".into()),
            arch: Some("x86_64".into()),
            toolchains: vec!["rust".into()],
            sandbox: vec![SandboxCapability::try_new("seccomp").unwrap()],
            network: Some(NetworkProfile::EgressRestricted),
            min_cpu_cores: 8,
            min_memory_mb: 16_384,
            gpu: true,
            region: Some("eu-west".into()),
            trust_domain: "org_1".into(),
        };
        req.normalize().unwrap();
        assert!(c.satisfies(&req).is_ok());
        req.min_cpu_cores = 9;
        assert!(c.satisfies(&req).is_err());
        req.min_cpu_cores = 1;
        req.toolchains = vec!["go".into()];
        assert!(c.satisfies(&req).is_err());
        req.toolchains = vec!["rust".into()];
        req.region = Some("us-east".into());
        assert!(c.satisfies(&req).is_err());
    }

    #[test]
    fn requeue_policy_is_bounded() {
        assert!(RequeuePolicy { max_attempts: 0 }.validate().is_err());
        assert!(RequeuePolicy {
            max_attempts: MAX_REQUEUE_ATTEMPTS + 1
        }
        .validate()
        .is_err());
        assert!(RequeuePolicy {
            max_attempts: MAX_REQUEUE_ATTEMPTS
        }
        .validate()
        .is_ok());
    }

    #[test]
    fn heartbeat_intervals_are_clamped() {
        assert_eq!(
            clamp_heartbeat_interval(0),
            HEARTBEAT_MIN_INTERVAL_MS,
            "a zero interval never yields an instantly-expired lease rule"
        );
        assert_eq!(
            clamp_heartbeat_interval(i64::MAX),
            HEARTBEAT_MAX_INTERVAL_MS
        );
        assert_eq!(
            clamp_heartbeat_interval(HEARTBEAT_DEFAULT_INTERVAL_MS),
            HEARTBEAT_DEFAULT_INTERVAL_MS
        );
    }
}
