//! Workspace topology discovery and the Cargo adapter.
//!
//! Cargo topology is read from the supervised `cargo metadata --no-deps
//! --format-version 1` command through the workspace's ONE
//! [`ProcessSupervisor`](faktor_terminal::ProcessSupervisor). That preserves
//! renamed dependencies (`alias = { package = "actual" }`), workspace
//! members/default-members and declared targets without a hand-written
//! manifest approximation. The decode is byte-bounded and the child has a
//! wall-clock deadline.
//!
//! **Documented fallback:** when Cargo is unavailable (the supervised spawn
//! reports `NotFound`, e.g. no `cargo` on PATH), discovery falls back to the
//! bounded structural manifest reader below and records `degraded = true`
//! with a reason. A cargo process that exists but fails (non-zero exit,
//! timeout, oversized output) is a typed error — never a silent fallback.
//!
//! Non-Cargo ecosystems are composed by the neutral adapters in
//! [`crate::topology`]: npm workspaces, Gradle settings/build files, Maven
//! poms, Python packaging metadata and a generic filesystem root. Adapters
//! are independent and may contribute units to the same repository.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use faktor_terminal::{EnvSpec, ProcessOwner, ProcessSupervisor, SpawnConfig};
use serde::Deserialize;

use crate::model::{GraphError, Revision};
use crate::topology::{
    self, AdapterKind, Topology, Unit, UnitKind, MAX_ADAPTER_BYTES, MAX_ADAPTER_UNITS,
};

pub use crate::topology::{Unit as TopologyUnit, UnitKind as TopologyUnitKind};

/// Kept for the fallback manifest reader and external callers.
pub const MAX_MANIFEST_BYTES: u64 = MAX_ADAPTER_BYTES;
pub const MAX_MEMBERS: usize = MAX_ADAPTER_UNITS;
pub const DEFAULT_METADATA_BYTES: usize = 64 * 1024 * 1024;
pub const DEFAULT_METADATA_TIMEOUT: Duration = Duration::from_secs(60);
const METADATA_CACHE_LIMIT: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrateInfo {
    pub name: String,
    pub dir: String,
    /// Workspace crate/package names this unit depends on, plus external
    /// package names (resolved to local units by [`Workspace`]).
    pub deps: BTreeSet<String>,
    pub kind: UnitKind,
    pub adapter: AdapterKind,
    /// Workspace-relative test target files/directories declared by the
    /// adapter (Cargo targets, Gradle/Maven test roots).
    pub test_targets: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub root: PathBuf,
    /// Every local unit, keyed by its stable name (Cargo crates and non-Cargo
    /// packages). `kind` distinguishes the graph node namespace.
    pub crates: BTreeMap<String, CrateInfo>,
    /// External package names (dependency names that are not local units).
    pub packages: BTreeSet<String>,
    pub skipped: Vec<String>,
    pub adapters: Vec<AdapterKind>,
    pub workspace_members: BTreeSet<String>,
    pub default_members: BTreeSet<String>,
    /// True only when the documented Cargo-unavailable fallback ran.
    pub degraded: bool,
    pub degraded_reason: Option<String>,
}

impl Workspace {
    pub fn unit_for_path(&self, path: &str) -> Option<&CrateInfo> {
        let path = path.trim_start_matches("./");
        let mut best: Option<&CrateInfo> = None;
        for info in self.crates.values() {
            if info.dir.is_empty() {
                continue;
            }
            if (path == info.dir || path.starts_with(&format!("{}/", info.dir)))
                && best.is_none_or(|current| info.dir.len() > current.dir.len())
            {
                best = Some(info);
            }
        }
        if best.is_none() {
            best = self.crates.values().find(|info| info.dir.is_empty());
        }
        best
    }

    pub fn crate_for_path(&self, path: &str) -> Option<&CrateInfo> {
        self.unit_for_path(path)
    }

    /// Resolve a declared dependency name to a local unit name. Cargo uses
    /// package names; Maven declarations are `groupId:artifactId`.
    pub fn local_unit(&self, dependency: &str) -> Option<&str> {
        if let Some((name, _)) = self.crates.get_key_value(dependency) {
            return Some(name);
        }
        let artifact = dependency.rsplit(':').next()?;
        if artifact != dependency {
            if let Some((name, _)) = self.crates.get_key_value(artifact) {
                return Some(name);
            }
        }
        None
    }

    pub fn crate_names(&self) -> Vec<String> {
        self.crates
            .values()
            .filter(|unit| unit.kind == UnitKind::Crate)
            .map(|unit| unit.name.clone())
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct TopologyOptions {
    /// The daemon/CLI supervisor. `None` uses the standalone shared
    /// supervisor ([`ProcessSupervisor::try_shared`]); this crate never
    /// constructs `std::process::Command`.
    pub supervisor: Option<Arc<ProcessSupervisor>>,
    /// Attempt supervised `cargo metadata` first. `false` forces the
    /// documented bounded parser (tests and Cargo-less deployments); the
    /// result is marked degraded.
    pub cargo_metadata: bool,
    /// Compose non-Cargo adapters even when Cargo metadata succeeded.
    pub adapters: bool,
    pub metadata_timeout: Duration,
    pub metadata_max_bytes: usize,
}

impl Default for TopologyOptions {
    fn default() -> Self {
        Self {
            supervisor: None,
            cargo_metadata: true,
            adapters: true,
            metadata_timeout: DEFAULT_METADATA_TIMEOUT,
            metadata_max_bytes: DEFAULT_METADATA_BYTES,
        }
    }
}

impl TopologyOptions {
    pub fn parser_only() -> Self {
        Self {
            cargo_metadata: false,
            ..Self::default()
        }
    }
}

fn read_bounded(path: &Path, max: u64) -> Result<String, GraphError> {
    let meta = fs::symlink_metadata(path)
        .map_err(|e| GraphError::Malformed(format!("{}: {e}", path.display())))?;
    if !meta.file_type().is_file() {
        return Err(GraphError::Malformed(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    if meta.len() > max {
        return Err(GraphError::Oversized {
            what: "manifest",
            max: max as usize,
            actual: meta.len() as usize,
        });
    }
    let bytes =
        fs::read(path).map_err(|e| GraphError::Malformed(format!("{}: {e}", path.display())))?;
    String::from_utf8(bytes)
        .map_err(|e| GraphError::Malformed(format!("{} is not UTF-8: {e}", path.display())))
}

#[derive(Debug, Default)]
struct Manifest {
    package: Option<String>,
    members: Vec<String>,
    default_members: Vec<String>,
    deps: BTreeSet<String>,
}

/// Bounded structural fallback for the documented Cargo-unavailable case.
/// It reads the dependency key AND `package = "actual"` so renamed path
/// dependencies keep their real package identity even without Cargo.
fn parse_manifest(text: &str) -> Manifest {
    #[derive(Clone, Copy)]
    enum ListKind {
        Members,
        Defaults,
    }
    let mut out = Manifest::default();
    let mut section = String::new();
    let mut list = None;
    for line in text.lines() {
        let trimmed = line.split('#').next().unwrap_or("").trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(kind) = list {
            let target = match kind {
                ListKind::Members => &mut out.members,
                ListKind::Defaults => &mut out.default_members,
            };
            for quoted in quoted_strings(trimmed) {
                target.push(quoted);
            }
            if trimmed.contains(']') {
                list = None;
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('[') {
            if let Some(header) = rest.split(']').next() {
                section = header.trim().to_string();
                if section == "workspace" {
                    list = if trimmed.contains("default-members") {
                        Some(ListKind::Defaults)
                    } else if trimmed.contains("members") {
                        Some(ListKind::Members)
                    } else {
                        None
                    };
                    if let Some(kind) = list {
                        let target = match kind {
                            ListKind::Members => &mut out.members,
                            ListKind::Defaults => &mut out.default_members,
                        };
                        for quoted in quoted_strings(trimmed) {
                            target.push(quoted);
                        }
                        if trimmed.contains(']') {
                            list = None;
                        }
                    }
                }
            }
            continue;
        }
        if section == "workspace" {
            if let Some(rest) = trimmed.strip_prefix("default-members") {
                if rest.trim_start().starts_with('=') {
                    for quoted in quoted_strings(rest) {
                        out.default_members.push(quoted);
                    }
                    if !rest.contains(']') {
                        list = Some(ListKind::Defaults);
                    }
                    continue;
                }
            }
            if let Some(rest) = trimmed.strip_prefix("members") {
                if rest.trim_start().starts_with('=') {
                    for quoted in quoted_strings(rest) {
                        out.members.push(quoted);
                    }
                    if !rest.contains(']') {
                        list = Some(ListKind::Members);
                    }
                    continue;
                }
            }
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim().trim_matches('"');
        if section == "package" && key == "name" {
            if let Some(name) = quoted_strings(value).into_iter().next() {
                out.package = Some(name);
            }
            continue;
        }
        if is_dependency_section(&section) {
            let dep_key = key
                .split('.')
                .next()
                .unwrap_or(key)
                .trim()
                .trim_start_matches('"')
                .trim_end_matches('"');
            if !is_dep_name(dep_key) {
                continue;
            }
            // `package = "actual"` renames the dependency key; the graph
            // edge must target the actual package identity.
            let actual = value
                .split("package")
                .nth(1)
                .and_then(|rest| quoted_strings(rest).into_iter().next());
            let name = actual
                .filter(|name| is_dep_name(name))
                .unwrap_or_else(|| dep_key.to_string());
            out.deps.insert(name);
        }
    }
    out
}

fn is_dependency_section(section: &str) -> bool {
    section == "dependencies"
        || section == "dev-dependencies"
        || section == "build-dependencies"
        || section.ends_with(".dependencies")
        || section.ends_with(".dev-dependencies")
        || section.ends_with(".build-dependencies")
}

fn is_dep_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

fn quoted_strings(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'"' {
            let start = index + 1;
            let mut end = start;
            while end < bytes.len() && bytes[end] != b'"' {
                end += 1;
            }
            if end < bytes.len() {
                out.push(text[start..end].to_string());
                index = end + 1;
                continue;
            }
        }
        index += 1;
    }
    out
}

pub fn discover(root: &Path) -> Result<Workspace, GraphError> {
    discover_with(root, &Revision::default(), &TopologyOptions::default())
}

pub fn discover_with(
    root: &Path,
    revision: &Revision,
    options: &TopologyOptions,
) -> Result<Workspace, GraphError> {
    let root = root
        .canonicalize()
        .map_err(|error| GraphError::Malformed(format!("workspace root: {error}")))?;
    // Cache only a revision-bound, clean topology: an unbound or dirty
    // revision has no stable identity, so the manifests are re-read.
    let cacheable = options.cargo_metadata && revision.is_bound() && !revision.dirty;
    let key = (
        root.clone(),
        revision.commit.clone(),
        revision.dirty,
        options.cargo_metadata,
        options.adapters,
    );
    if cacheable {
        if let Some(cached) = metadata_cache()
            .lock()
            .ok()
            .and_then(|cache| cache.get(&key).cloned())
        {
            return Ok(cached);
        }
    }
    let workspace = discover_topology(&root, options)?;
    if cacheable {
        if let Ok(mut cache) = metadata_cache().lock() {
            if cache.len() >= METADATA_CACHE_LIMIT {
                cache.clear();
            }
            cache.insert(key, workspace.clone());
        }
    }
    Ok(workspace)
}

fn metadata_cache() -> &'static Mutex<BTreeMap<CacheKey, Workspace>> {
    static CACHE: OnceLock<Mutex<BTreeMap<CacheKey, Workspace>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

type CacheKey = (PathBuf, Option<String>, bool, bool, bool);

fn discover_topology(root: &Path, options: &TopologyOptions) -> Result<Workspace, GraphError> {
    let mut topology = Topology::default();
    let mut degraded = false;
    let mut degraded_reason = None;
    let mut members = BTreeSet::new();
    let mut defaults = BTreeSet::new();

    let cargo_present = root.join("Cargo.toml").exists();
    if cargo_present {
        if options.cargo_metadata {
            match metadata_units(root, options) {
                Ok(metadata) => {
                    for unit in metadata.units {
                        topology.insert(unit);
                    }
                    topology.skipped.extend(metadata.skipped);
                    members = metadata.members;
                    defaults = metadata.defaults;
                    topology.adapters.push(AdapterKind::Cargo);
                }
                Err(MetadataFailure::Unavailable(reason)) => {
                    degraded = true;
                    degraded_reason = Some(format!(
                        "cargo metadata unavailable ({reason}); used the bounded manifest fallback"
                    ));
                    let fallback = fallback_cargo(root)?;
                    topology.units.extend(fallback.units);
                    topology.skipped.extend(fallback.skipped);
                    members = fallback.members;
                    defaults = fallback.defaults;
                    topology.adapters.push(AdapterKind::Cargo);
                }
                Err(MetadataFailure::Failed(reason)) => {
                    return Err(GraphError::Malformed(format!(
                        "cargo metadata failed: {reason}"
                    )));
                }
            }
        } else {
            degraded = true;
            degraded_reason =
                Some("cargo metadata disabled; used the bounded manifest fallback".into());
            let fallback = fallback_cargo(root)?;
            topology.units.extend(fallback.units);
            topology.skipped.extend(fallback.skipped);
            members = fallback.members;
            defaults = fallback.defaults;
            topology.adapters.push(AdapterKind::Cargo);
        }
    }

    if options.adapters {
        for adapter in [
            topology::discover_npm(root),
            topology::discover_gradle(root),
            topology::discover_maven(root),
            topology::discover_python(root),
        ] {
            if !adapter.units.is_empty() {
                for unit in adapter.units.into_values() {
                    topology.insert(unit);
                }
                topology.skipped.extend(adapter.skipped);
                for kind in adapter.adapters {
                    if !topology.adapters.contains(&kind) {
                        topology.adapters.push(kind);
                    }
                }
            }
        }
    }

    if topology.units.is_empty() {
        let generic = topology::generic_filesystem(root);
        for unit in generic.units.into_values() {
            topology.insert(unit);
        }
        for kind in generic.adapters {
            if !topology.adapters.contains(&kind) {
                topology.adapters.push(kind);
            }
        }
    }
    if topology.units.is_empty() {
        return Err(GraphError::Malformed(
            "repository has no discoverable unit (no Cargo/npm/Gradle/Maven/Python manifest or files)"
                .into(),
        ));
    }

    let mut crates = BTreeMap::new();
    for unit in topology.units.into_values().take(MAX_ADAPTER_UNITS) {
        let info = CrateInfo {
            name: unit.name.clone(),
            dir: unit.dir.clone(),
            deps: unit.deps,
            kind: unit.kind,
            adapter: unit.adapter,
            test_targets: unit.test_targets,
        };
        crates.insert(info.name.clone(), info);
    }
    let workspace = Workspace {
        root: root.to_path_buf(),
        crates,
        packages: BTreeSet::new(),
        skipped: topology.skipped,
        adapters: topology.adapters,
        workspace_members: members,
        default_members: defaults,
        degraded,
        degraded_reason,
    };
    Ok(with_external_packages(workspace))
}

fn with_external_packages(mut workspace: Workspace) -> Workspace {
    let mut packages = BTreeSet::new();
    for unit in workspace.crates.values() {
        for dep in &unit.deps {
            if workspace.local_unit(dep).is_none() {
                packages.insert(dep.clone());
            }
        }
    }
    workspace.packages = packages;
    workspace
}

struct Fallback {
    units: BTreeMap<String, Unit>,
    members: BTreeSet<String>,
    defaults: BTreeSet<String>,
    skipped: Vec<String>,
}

fn fallback_cargo(root: &Path) -> Result<Fallback, GraphError> {
    let root_manifest = root.join("Cargo.toml");
    let text = read_bounded(&root_manifest, MAX_MANIFEST_BYTES)?;
    let parsed = parse_manifest(&text);
    let mut skipped = Vec::new();
    let mut member_dirs = if parsed.members.is_empty() {
        if parsed.package.is_some() {
            vec![String::new()]
        } else {
            Vec::new()
        }
    } else {
        let (dirs, refused) = topology::expand_member_patterns_checked(root, &parsed.members);
        skipped.extend(refused);
        dirs
    };
    member_dirs.sort();
    member_dirs.dedup();
    let mut units = BTreeMap::new();
    for dir in member_dirs {
        let label = if dir.is_empty() {
            ".".to_string()
        } else {
            dir.clone()
        };
        let manifest_path = if dir.is_empty() {
            root.join("Cargo.toml")
        } else {
            root.join(&dir).join("Cargo.toml")
        };
        // Re-prove containment at the manifest read site: a symlinked
        // `Cargo.toml` may not redirect the read outside the root.
        let manifest_path = match manifest_path.canonicalize() {
            Ok(path) if path.starts_with(root) => path,
            Ok(_) => {
                skipped.push(format!(
                    "{label}/Cargo.toml: resolves outside the workspace root"
                ));
                continue;
            }
            Err(_) => continue,
        };
        let manifest = read_bounded(&manifest_path, MAX_MANIFEST_BYTES)?;
        let manifest = parse_manifest(&manifest);
        let Some(name) = manifest.package else {
            continue;
        };
        let mut unit = Unit::new(name, &dir, UnitKind::Crate, AdapterKind::Cargo);
        unit.deps = manifest.deps;
        for test in ["tests", "benches"] {
            let path = if dir.is_empty() {
                test.to_string()
            } else {
                format!("{dir}/{test}")
            };
            if root.join(&path).is_dir() {
                unit.test_targets.insert(path);
            }
        }
        units.insert(unit.name.clone(), unit);
    }
    // A virtual workspace with no readable member manifests still needs a
    // root unit for root-level source files.
    if units.is_empty() && parsed.package.is_some() {
        units.insert(
            text_package_name(&text).unwrap_or_else(|| "root".into()),
            Unit::new(
                text_package_name(&text).unwrap_or_else(|| "root".into()),
                "",
                UnitKind::Crate,
                AdapterKind::Cargo,
            ),
        );
    }
    let defaults = parsed
        .default_members
        .iter()
        .filter_map(|dir| {
            let dir = topology::normalize_dir(dir);
            units
                .values()
                .find(|unit| unit.dir == dir)
                .map(|unit| unit.name.clone())
        })
        .collect();
    let members = units.keys().cloned().collect();
    Ok(Fallback {
        units,
        members,
        defaults,
        skipped,
    })
}

fn text_package_name(text: &str) -> Option<String> {
    parse_manifest(text).package
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<MetaPackage>,
    workspace_members: Vec<String>,
    #[serde(default)]
    workspace_default_members: Vec<String>,
}

#[derive(Deserialize)]
struct MetaPackage {
    name: String,
    id: String,
    manifest_path: String,
    #[serde(default)]
    dependencies: Vec<MetaDependency>,
    #[serde(default)]
    targets: Vec<MetaTarget>,
}

#[derive(Deserialize)]
struct MetaDependency {
    name: String,
    #[serde(default)]
    rename: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    target: Option<String>,
}

#[derive(Deserialize)]
struct MetaTarget {
    #[serde(default)]
    kind: Vec<String>,
    #[serde(default)]
    src_path: String,
}

struct MetadataUnits {
    units: Vec<Unit>,
    members: BTreeSet<String>,
    defaults: BTreeSet<String>,
    skipped: Vec<String>,
}

enum MetadataFailure {
    /// Cargo itself is not installed/executable: the ONLY fallback case.
    Unavailable(String),
    Failed(String),
}

fn metadata_units(
    root: &Path,
    options: &TopologyOptions,
) -> Result<MetadataUnits, MetadataFailure> {
    let supervisor = match options.supervisor.clone() {
        Some(supervisor) => supervisor,
        None => ProcessSupervisor::try_shared()
            .map_err(|error| MetadataFailure::Unavailable(error.to_string()))?,
    };
    let output = supervisor
        .run_sync(
            SpawnConfig {
                cmd: "cargo".into(),
                args: vec![
                    "metadata".into(),
                    "--no-deps".into(),
                    "--format-version".into(),
                    "1".into(),
                ],
                cwd: root.to_path_buf(),
                env: EnvSpec::toolchain(),
                owner: ProcessOwner::Daemon,
                ..Default::default()
            },
            options.metadata_timeout,
            options.metadata_max_bytes,
            256 * 1024,
        )
        .map_err(|error| {
            if error.kind == faktor_core::error::ErrorKind::NotFound {
                MetadataFailure::Unavailable(error.to_string())
            } else {
                MetadataFailure::Failed(error.to_string())
            }
        })?;
    if output.timed_out {
        return Err(MetadataFailure::Failed(
            "cargo metadata exceeded its deadline".into(),
        ));
    }
    if output.stdout_truncated {
        return Err(MetadataFailure::Failed(format!(
            "cargo metadata output exceeded the {}-byte bound",
            options.metadata_max_bytes
        )));
    }
    if output.exit_code != Some(0) {
        return Err(MetadataFailure::Failed(format!(
            "exit {:?}: {}",
            output.exit_code,
            output.stderr_head.trim()
        )));
    }
    let metadata: Metadata = serde_json::from_str(&output.stdout_head)
        .map_err(|error| MetadataFailure::Failed(format!("invalid JSON: {error}")))?;
    let member_ids: BTreeSet<&str> = metadata
        .workspace_members
        .iter()
        .map(String::as_str)
        .collect();
    // Cargo reports the explicit default-members; an empty list means the
    // default is every workspace member.
    let default_ids: BTreeSet<&str> = if metadata.workspace_default_members.is_empty() {
        member_ids.clone()
    } else {
        metadata
            .workspace_default_members
            .iter()
            .map(String::as_str)
            .collect()
    };
    let mut units = Vec::new();
    let mut members = BTreeSet::new();
    let mut defaults = BTreeSet::new();
    let mut skipped = Vec::new();
    for package in metadata.packages {
        if !member_ids.contains(package.id.as_str()) {
            continue;
        }
        let manifest = PathBuf::from(&package.manifest_path);
        // Cargo will happily report a member declared as `../outside`.
        // Containment is enforced on the canonical manifest BEFORE any of
        // its directory, targets or sources are trusted.
        let manifest = match manifest.canonicalize() {
            Ok(path) if path.starts_with(root) => path,
            Ok(_) => {
                skipped.push(format!(
                    "{}: member manifest resolves outside the workspace root",
                    package.name
                ));
                continue;
            }
            Err(_) => {
                skipped.push(format!(
                    "{}: member manifest is missing or unreadable",
                    package.name
                ));
                continue;
            }
        };
        let dir = manifest
            .parent()
            .and_then(|parent| parent.strip_prefix(root).ok())
            .map(|relative| topology::normalize_dir(&relative.to_string_lossy()))
            .unwrap_or_default();
        let mut unit = Unit::new(
            package.name.clone(),
            dir,
            UnitKind::Crate,
            AdapterKind::Cargo,
        );
        for dependency in &package.dependencies {
            // `rename` is the dependency KEY; `name` is the actual package.
            // The graph edge targets the actual identity, preserving the
            // `package = "actual"` semantics.
            if let Some(rename) = &dependency.rename {
                if rename.is_empty() {
                    continue;
                }
            }
            let _ = (&dependency.kind, &dependency.target);
            if is_dep_name(&dependency.name) {
                unit.deps.insert(dependency.name.clone());
            }
        }
        for target in &package.targets {
            if target.kind.iter().any(|kind| kind == "test") && !target.src_path.is_empty() {
                if let Ok(relative) = Path::new(&target.src_path).strip_prefix(root) {
                    unit.test_targets
                        .insert(topology::normalize_dir(&relative.to_string_lossy()));
                }
            }
        }
        members.insert(unit.name.clone());
        if default_ids.contains(package.id.as_str()) {
            defaults.insert(unit.name.clone());
        }
        units.push(unit);
    }
    Ok(MetadataUnits {
        units,
        members,
        defaults,
        skipped,
    })
}
