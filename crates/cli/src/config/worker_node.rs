//! `config::worker_node`: schema domain of the daemon config.

#![allow(unused_imports)]

use super::*;

/// The additive `[worker_node]` section: THIS host acting as a remote worker
/// node of a control plane (the client half of the `[workers]` plane).
///
/// Strict and additive:
///
/// - `enabled` (default `false`): while disabled `faktor worker run` refuses
///   typed before any network or filesystem effect — the disabled-parity
///   state of every pre-existing config;
/// - `worker_id`, `token` XOR `token_payload`, `control_plane_url`: the
///   registration identity, credential and daemon base URL (required when
///   enabled; the token never rides the config log line). `token_payload`
///   names a payload inside `payload_dir` (the strict operator-staged
///   contract: regular file, 0600-style on unix, bounded, non-empty); a
///   missing/corrupt/too-permissive token payload refuses before any
///   network or filesystem effect beyond the payload read itself
///   (`token_file` is accepted as a legacy alias and treated as a payload
///   NAME — absolute paths are refused);
/// - `trust_domain` (required) and the capability advertisement (`os`,
///   `arch`, `toolchains`, `sandbox`, `network`, `region`, `cpu_cores`,
///   `memory_mb`, `gpu`): the advertisement the plane reconciles. Only jobs
///   whose required toolchains/sandbox/network profile the worker advertises
///   are ever claimed;
/// - `claim_deadline_ms` / `claim_interval_ms` / `heartbeat_interval_ms`:
///   the bounded long-poll/heartbeat cadence (clamped by the worker crate);
/// - `payload_dir`: the local directory the job payloads are staged in
///   (`<payload_dir>/<digest>`) AND the directory `token_payload` is
///   resolved from (default `<data_dir>/worker_payloads`); a missing job
///   payload is fail-closed (`PayloadUnavailable`: nothing executes,
///   nothing submits);
/// - `discard_workspace_on_success` (default `true`): bounded disk;
/// - `iterations`: the bounded loop budget of one `worker run` invocation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct WorkerNodeCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub worker_id: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
    /// The payload NAME (inside `payload_dir`) holding the registration
    /// token. `token_file` is accepted as a legacy alias.
    #[serde(default, alias = "token_file")]
    pub token_payload: Option<String>,
    #[serde(default)]
    pub control_plane_url: Option<String>,
    #[serde(default)]
    pub trust_domain: Option<String>,
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
    pub region: Option<String>,
    #[serde(default)]
    pub cpu_cores: u32,
    #[serde(default)]
    pub memory_mb: u64,
    #[serde(default)]
    pub gpu: bool,
    #[serde(default)]
    pub claim_deadline_ms: Option<i64>,
    #[serde(default)]
    pub claim_interval_ms: Option<i64>,
    #[serde(default)]
    pub heartbeat_interval_ms: Option<i64>,
    #[serde(default)]
    pub payload_dir: Option<String>,
    #[serde(default)]
    pub discard_workspace_on_success: Option<bool>,
    #[serde(default)]
    pub iterations: Option<u32>,
}

impl WorkerNodeCfg {
    /// Validate the section (called by [`Config::validate`] on both load
    /// paths). A disabled section validates nothing beyond its shape.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(payload_dir) = &self.payload_dir {
            WorkerNodeCfg::validate_payload_dir(payload_dir)?;
        }
        if let Some(name) = &self.token_payload {
            crate::payload::PayloadDir::validate_name(name)
                .map_err(|e| format!("worker_node: token_payload {e}"))?;
        }
        if !self.enabled {
            return Ok(());
        }
        let _ = self.worker_id()?;
        let _ = self.control_plane_url()?;
        let _ = self.trust_domain()?;
        match (&self.token, &self.token_payload) {
            (Some(_), Some(_)) => return Err(
                "worker_node: `token` and `token_payload` are mutually exclusive; set exactly one"
                    .into(),
            ),
            (None, None) => return Err(
                "worker_node: an enabled [worker_node] section requires `token` or `token_payload`"
                    .into(),
            ),
            _ => {}
        }
        let _ = self.capabilities()?;
        if let Some(deadline) = self.claim_deadline_ms {
            if !(0..=faktor_worker::MAX_CLAIM_DEADLINE_MS).contains(&deadline) {
                return Err(format!(
                    "worker_node: claim_deadline_ms must be 0..={}",
                    faktor_worker::MAX_CLAIM_DEADLINE_MS
                ));
            }
        }
        if let Some(interval) = self.claim_interval_ms {
            if interval < faktor_worker::MIN_CLAIM_INTERVAL_MS {
                return Err(format!(
                    "worker_node: claim_interval_ms must be >= {}",
                    faktor_worker::MIN_CLAIM_INTERVAL_MS
                ));
            }
        }
        if let Some(iterations) = self.iterations {
            if iterations == 0 || iterations > faktor_worker::MAX_LOOP_ITERATIONS {
                return Err(format!(
                    "worker_node: iterations must be 1..={}",
                    faktor_worker::MAX_LOOP_ITERATIONS
                ));
            }
        }
        Ok(())
    }

    pub fn worker_id(&self) -> Result<faktor_worker::WorkerId, String> {
        let raw = self.worker_id.clone().ok_or_else(|| {
            "worker_node: an enabled [worker_node] section requires `worker_id`".to_string()
        })?;
        faktor_worker::WorkerId::try_new(raw).map_err(|e| format!("worker_node: {e}"))
    }

    /// Validate one configured payload directory: bounded ASCII, no control
    /// characters, no `..` traversal (absolute paths allowed; relative paths
    /// live under the data dir).
    pub fn validate_payload_dir(raw: &str) -> Result<(), String> {
        if raw.is_empty() || raw.len() > MAX_PAYLOAD_ROOT_BYTES || !raw.is_ascii() {
            return Err(format!(
                "worker_node: payload_dir must be 1..={MAX_PAYLOAD_ROOT_BYTES} ASCII bytes"
            ));
        }
        if raw.bytes().any(|b| b.is_ascii_control()) {
            return Err("worker_node: payload_dir contains control characters".into());
        }
        if raw.split(['/', '\\']).any(|part| part == "..") {
            return Err(format!(
                "worker_node: payload_dir {raw:?} must not contain `..` traversal"
            ));
        }
        Ok(())
    }

    /// The resolved staged payload directory (default
    /// `<data_dir>/worker_payloads`): job payloads AND the registration
    /// token payload resolve under it.
    pub fn payload_root(&self, data_dir: &Path) -> Result<std::path::PathBuf, String> {
        match &self.payload_dir {
            None => Ok(data_dir.join("worker_payloads")),
            Some(raw) => {
                Self::validate_payload_dir(raw)?;
                let raw_path = Path::new(raw);
                if raw_path.is_absolute() {
                    Ok(raw_path.to_path_buf())
                } else {
                    Ok(data_dir.join(raw_path))
                }
            }
        }
    }

    /// The daemon base URL (http(s), no trailing slash).
    pub fn control_plane_url(&self) -> Result<String, String> {
        let raw = self.control_plane_url.clone().ok_or_else(|| {
            "worker_node: an enabled [worker_node] section requires `control_plane_url`".to_string()
        })?;
        let raw = raw.trim_end_matches('/').to_string();
        if !(raw.starts_with("https://") || raw.starts_with("http://")) || raw.len() > 2048 {
            return Err("worker_node: control_plane_url must be an http(s) URL".into());
        }
        Ok(raw)
    }

    pub fn trust_domain(&self) -> Result<String, String> {
        let raw = self.trust_domain.clone().ok_or_else(|| {
            "worker_node: an enabled [worker_node] section requires `trust_domain`".to_string()
        })?;
        let raw = raw.trim().to_ascii_lowercase();
        if raw.is_empty() || raw.len() > 128 || !raw.bytes().all(|b| b.is_ascii_graphic()) {
            return Err("worker_node: trust_domain must be 1..=128 printable ASCII bytes".into());
        }
        Ok(raw)
    }

    /// The versioned capability advertisement this node registers.
    pub fn capabilities(&self) -> Result<faktor_worker::WorkerCapabilities, String> {
        let network = match self.network.as_deref() {
            None | Some("none") => faktor_worker::NetworkProfile::None,
            Some("egress_restricted") => faktor_worker::NetworkProfile::EgressRestricted,
            Some("full") => faktor_worker::NetworkProfile::Full,
            Some(other) => {
                return Err(format!(
                    "worker_node: network {other:?} must be one of none|egress_restricted|full"
                ));
            }
        };
        let cpu_cores = if self.cpu_cores > 0 {
            self.cpu_cores
        } else {
            std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(1)
        };
        let memory_mb = if self.memory_mb > 0 {
            self.memory_mb
        } else {
            4_096
        };
        let sandbox = self
            .sandbox
            .iter()
            .map(|tag| faktor_worker::SandboxCapability::try_new(tag.clone()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("worker_node: {e}"))?;
        let mut capabilities = faktor_worker::WorkerCapabilities {
            os: self
                .os
                .clone()
                .unwrap_or_else(|| std::env::consts::OS.to_string()),
            arch: self
                .arch
                .clone()
                .unwrap_or_else(|| std::env::consts::ARCH.to_string()),
            toolchains: self.toolchains.clone(),
            sandbox,
            network,
            cpu_cores,
            memory_mb,
            gpu: self.gpu.then(|| faktor_worker::GpuCapability {
                model: "unknown".into(),
                count: 1,
                memory_mb: 0,
            }),
            region: self.region.clone().unwrap_or_else(|| "local".to_string()),
            trust_domain: self.trust_domain()?,
            protocol_version: faktor_worker::WORKER_PROTOCOL_VERSION,
        };
        capabilities
            .normalize()
            .map_err(|e| format!("worker_node: {e}"))?;
        Ok(capabilities)
    }
}
