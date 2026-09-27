//! `config::workers`: schema domain of the daemon config.

#![allow(unused_imports)]

use super::*;

/// The additive `[workers]` section: the remote/VPC worker plane.
///
/// Strict and additive:
///
/// - `enabled` (default `false`): when false — the default and the only
///   value for every pre-existing config — the daemon builds NO worker
///   plane, creates no worker database file, every `/native/workers*` and
///   `/native/jobs/*` route answers a typed 409 `workers_disabled`, and the
///   TaskExecutor's placement seam stays DISABLED (local execution is
///   byte-identical to the pre-worker-plane daemon);
/// - `database`: an optional simple FILE NAME (default `workers.db`) for
///   the durable worker plane. The worker crate owns its OWN migration
///   ladder (`user_version` v1); the control-plane/billing `user_version`
///   is never touched;
/// - `organization`: the organization the plane leases jobs for (required
///   when enabled);
/// - `trust_domain`: the trust domain of the plane's jobs (default: the
///   organization id). Workers register into it and can only ever lease
///   jobs of their own organization AND trust domain;
/// - `os` / `arch` / `toolchains` / `network` / `region` / `min_cpu_cores`
///   / `min_memory_mb` / `gpu`: the placement REQUIREMENTS the daemon
///   demands of a worker before a task run is placed remotely. Empty
///   defaults mean "any registered worker of the trust domain".
///
/// When enabled AND at least one eligible worker is registered, a new task
/// run is placed remotely: the plane mints one immutable job generation,
/// CAS-accepts its lease and the local executor starts NOTHING (the receipt
/// names the remote job). Without an eligible worker the run executes
/// locally exactly as before.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Default)]
pub struct WorkersCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub organization: Option<String>,
    #[serde(default)]
    pub trust_domain: Option<String>,
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub arch: Option<String>,
    #[serde(default)]
    pub toolchains: Vec<String>,
    #[serde(default)]
    pub network: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub min_cpu_cores: u32,
    #[serde(default)]
    pub min_memory_mb: u64,
    #[serde(default)]
    pub gpu: bool,
}

/// The `[workers]` keys, in stable order (unknown-field errors list them).
pub const WORKERS_FIELDS: &[&str] = &[
    "enabled",
    "database",
    "organization",
    "trust_domain",
    "os",
    "arch",
    "toolchains",
    "network",
    "region",
    "min_cpu_cores",
    "min_memory_mb",
    "gpu",
];

/// The default worker-plane database file name (relative to the daemon data
/// dir).
pub const DEFAULT_WORKERS_DATABASE: &str = "workers.db";

impl<'de> serde::Deserialize<'de> for WorkersCfg {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error as _, MapAccess, Visitor};

        struct SectionVisitor;

        impl<'de> Visitor<'de> for SectionVisitor {
            type Value = WorkersCfg;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("the [workers] section as a JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<WorkersCfg, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut out = WorkersCfg::default();
                let mut seen: u16 = 0;
                while let Some(key) = map.next_key::<String>()? {
                    let (bit, name) = match key.as_str() {
                        "enabled" => (1u16, "enabled"),
                        "database" => (2, "database"),
                        "organization" => (4, "organization"),
                        "trust_domain" => (8, "trust_domain"),
                        "os" => (16, "os"),
                        "arch" => (32, "arch"),
                        "toolchains" => (64, "toolchains"),
                        "network" => (128, "network"),
                        "region" => (256, "region"),
                        "min_cpu_cores" => (512, "min_cpu_cores"),
                        "min_memory_mb" => (1024, "min_memory_mb"),
                        "gpu" => (2048, "gpu"),
                        other => return Err(A::Error::unknown_field(other, WORKERS_FIELDS)),
                    };
                    if seen & bit != 0 {
                        return Err(A::Error::duplicate_field(name));
                    }
                    seen |= bit;
                    match bit {
                        1 => out.enabled = map.next_value::<bool>()?,
                        2 => out.database = map.next_value::<Option<String>>()?,
                        4 => out.organization = map.next_value::<Option<String>>()?,
                        8 => out.trust_domain = map.next_value::<Option<String>>()?,
                        16 => out.os = map.next_value::<Option<String>>()?,
                        32 => out.arch = map.next_value::<Option<String>>()?,
                        64 => out.toolchains = map.next_value::<Vec<String>>()?,
                        128 => out.network = map.next_value::<Option<String>>()?,
                        256 => out.region = map.next_value::<Option<String>>()?,
                        512 => out.min_cpu_cores = map.next_value::<u32>()?,
                        1024 => out.min_memory_mb = map.next_value::<u64>()?,
                        _ => out.gpu = map.next_value::<bool>()?,
                    }
                }
                Ok(out)
            }
        }

        de.deserialize_map(SectionVisitor)
    }
}

impl WorkersCfg {
    /// The resolved worker-plane database path under `data_dir` (`None`
    /// when the section is disabled — the daemon then never creates it).
    pub fn workers_path(&self, data_dir: &Path) -> Result<Option<std::path::PathBuf>, String> {
        if !self.enabled {
            return Ok(None);
        }
        self.validate()?;
        let name = self
            .database
            .clone()
            .unwrap_or_else(|| DEFAULT_WORKERS_DATABASE.to_string());
        Ok(Some(data_dir.join(name)))
    }

    /// The organization the plane leases jobs for (required when enabled).
    pub fn organization(&self) -> Result<faktor_cloud::OrganizationId, String> {
        let raw = self.organization.clone().ok_or_else(|| {
            "workers: an enabled [workers] section requires `organization`".to_string()
        })?;
        faktor_cloud::OrganizationId::try_new(raw).map_err(|e| format!("workers: {e}"))
    }

    /// The plane's trust domain (default: the organization id).
    pub fn trust_domain(&self) -> Result<String, String> {
        let organization = self.organization()?;
        match &self.trust_domain {
            None => Ok(organization.as_str().to_string()),
            Some(raw) => {
                let trimmed = raw.trim();
                if trimmed.is_empty() || trimmed.len() > 128 {
                    return Err("workers: trust_domain must be 1..=128 bytes".into());
                }
                if !trimmed.bytes().all(|b| b.is_ascii_graphic()) {
                    return Err(
                        "workers: trust_domain must be printable ASCII without whitespace".into(),
                    );
                }
                Ok(trimmed.to_ascii_lowercase())
            }
        }
    }

    /// The placement requirement defaults demanded of an eligible worker.
    pub fn requirements(&self) -> Result<faktor_worker::JobRequirements, String> {
        let trust_domain = self.trust_domain()?;
        let network = match self.network.as_deref() {
            None => None,
            Some("none") => Some(faktor_worker::NetworkProfile::None),
            Some("egress_restricted") => Some(faktor_worker::NetworkProfile::EgressRestricted),
            Some("full") => Some(faktor_worker::NetworkProfile::Full),
            Some(other) => {
                return Err(format!(
                    "workers: network {other:?} must be one of none|egress_restricted|full"
                ));
            }
        };
        let mut requirements = faktor_worker::JobRequirements {
            os: self.os.clone(),
            arch: self.arch.clone(),
            toolchains: self.toolchains.clone(),
            sandbox: Vec::new(),
            network,
            min_cpu_cores: self.min_cpu_cores,
            min_memory_mb: self.min_memory_mb,
            gpu: self.gpu,
            region: self.region.clone(),
            trust_domain,
        };
        requirements
            .normalize()
            .map_err(|e| format!("workers: {e}"))?;
        Ok(requirements)
    }

    /// Validate the section (called by [`Config::validate`] on both load
    /// paths). A disabled section validates nothing beyond its shape.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(database) = &self.database {
            CloudCfg::validate_database_name("workers database", database)?;
        }
        if !self.enabled {
            return Ok(());
        }
        let _ = self.organization()?;
        let _ = self.trust_domain()?;
        let _ = self.requirements()?;
        Ok(())
    }
}
