//! Neutral repository topology adapters.
//!
//! Every adapter reads one manifest family (never a second graph authority)
//! and emits the same neutral [`Unit`] records: a local crate/package name, a
//! workspace-relative directory, its declared dependency names and any test
//! target paths. [`crate::manifest`] composes the adapters, deduplicates the
//! records and the graph builder turns them into `crate:`/`package:` nodes.
//!
//! All reads are byte- and count-bounded, directory walks are sorted, and
//! every adapter is deterministic: the output is a pure function of the
//! repository bytes.
//!
//! # Native metadata first, structural fallback documented
//!
//! The npm adapter reads the `package-lock.json` workspace package graph
//! (lockfile v2/v3) when it is present: that is npm's own resolved model and
//! preserves workspace membership, direct dependencies and test layouts
//! without re-deriving them from prose. The Gradle adapter asks Gradle itself
//! for the project/dependency model through a bounded init script
//! (`--no-daemon --offline`), supervised by the workspace's
//! [`ProcessSupervisor`](faktor_terminal::ProcessSupervisor). Python resolves
//! PEP 621 `pyproject.toml` metadata and installed distribution metadata
//! (`*.dist-info/METADATA`, `*.egg-info/PKG-INFO`) first.
//!
//! Structural readers remain as the typed *degraded* fallback: every
//! [`AdapterOutcome`] records whether native metadata served the unit and,
//! when not, the exact reason. The Cargo adapter is served by the supervised
//! `cargo metadata` path in [`crate::manifest`].
//!
//! A native probe that is unavailable, times out or exits non-zero is a
//! documented degradation, never a silently invented graph. Children are
//! spawned only through the shared supervisor with a wall deadline, bounded
//! stdout/stderr and no daemon (`gradle --no-daemon`); a probe that never
//! returns is killed with its process group.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use faktor_terminal::{EnvSpec, ProcessOwner, ProcessSupervisor, SpawnConfig};

use crate::model::GraphError;

pub const MAX_ADAPTER_BYTES: u64 = 512 * 1024;
pub const MAX_NATIVE_METADATA_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_ADAPTER_UNITS: usize = 4096;
pub const MAX_ADAPTER_DEPTH: usize = 6;
pub const MAX_ADAPTER_FILES: usize = 8192;
pub const DEFAULT_NATIVE_TIMEOUT: Duration = Duration::from_secs(60);
const NATIVE_STDERR_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum UnitKind {
    Crate,
    Package,
}

impl UnitKind {
    pub fn node_id(self, name: &str) -> String {
        match self {
            UnitKind::Crate => crate::model::crate_id(name),
            UnitKind::Package => crate::model::package_id(name),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AdapterKind {
    Cargo,
    Npm,
    Gradle,
    Maven,
    Python,
    GenericFilesystem,
}

impl AdapterKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AdapterKind::Cargo => "cargo",
            AdapterKind::Npm => "npm",
            AdapterKind::Gradle => "gradle",
            AdapterKind::Maven => "maven",
            AdapterKind::Python => "python",
            AdapterKind::GenericFilesystem => "generic-filesystem",
        }
    }
}

/// How one adapter produced its units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AdapterMode {
    /// The ecosystem's own metadata served the graph (npm lockfile graph,
    /// supervised Gradle model, PEP 621/installed Python metadata).
    NativeMetadata,
    /// A bounded structural reader stood in for unavailable native
    /// metadata.
    StructuralFallback,
}

impl AdapterMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AdapterMode::NativeMetadata => "native-metadata",
            AdapterMode::StructuralFallback => "structural-fallback",
        }
    }
}

/// Per-adapter provenance and degradation record. `degraded` is true exactly
/// when a structural fallback was used; `detail` always names the native
/// source or the fallback reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterOutcome {
    pub adapter: AdapterKind,
    pub mode: AdapterMode,
    pub degraded: bool,
    pub detail: String,
}

impl AdapterOutcome {
    pub fn native(adapter: AdapterKind, detail: impl Into<String>) -> Self {
        Self {
            adapter,
            mode: AdapterMode::NativeMetadata,
            degraded: false,
            detail: detail.into(),
        }
    }

    pub fn fallback(adapter: AdapterKind, detail: impl Into<String>) -> Self {
        Self {
            adapter,
            mode: AdapterMode::StructuralFallback,
            degraded: true,
            detail: detail.into(),
        }
    }
}

/// Explicit native-metadata probe settings. `enabled = false` forces the
/// documented structural fallbacks (tests and Cargo-less deployments).
#[derive(Debug, Clone)]
pub struct NativeProbe {
    pub supervisor: Option<Arc<ProcessSupervisor>>,
    pub timeout: Duration,
    pub max_bytes: usize,
    pub enabled: bool,
    /// Test/deployment override for the Gradle executable (defaults to
    /// `gradle` on PATH).
    pub gradle_command: Option<String>,
}

impl Default for NativeProbe {
    fn default() -> Self {
        Self {
            supervisor: None,
            timeout: DEFAULT_NATIVE_TIMEOUT,
            max_bytes: MAX_NATIVE_METADATA_BYTES as usize,
            enabled: true,
            gradle_command: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    pub name: String,
    pub dir: String,
    pub kind: UnitKind,
    pub deps: BTreeSet<String>,
    /// Workspace-relative test target files declared by the adapter (Cargo
    /// targets, Gradle/Maven test roots). Empty when the adapter has none.
    pub test_targets: BTreeSet<String>,
    pub adapter: AdapterKind,
}

impl Unit {
    pub fn new(
        name: impl Into<String>,
        dir: impl Into<String>,
        kind: UnitKind,
        adapter: AdapterKind,
    ) -> Self {
        Self {
            name: name.into(),
            dir: normalize_dir(&dir.into()),
            kind,
            deps: BTreeSet::new(),
            test_targets: BTreeSet::new(),
            adapter,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Topology {
    pub units: BTreeMap<String, Unit>,
    pub adapters: Vec<AdapterKind>,
    pub skipped: Vec<String>,
    pub outcomes: Vec<AdapterOutcome>,
}

impl Topology {
    pub fn insert(&mut self, unit: Unit) {
        if self.units.len() >= MAX_ADAPTER_UNITS {
            self.skipped
                .push(format!("adapter unit bound reached at {}", unit.name));
            return;
        }
        // A later adapter never overwrites an earlier unit with the same
        // name; the collision is visible instead of silently ambiguous.
        if let Some(existing) = self.units.get(&unit.name) {
            if existing.dir != unit.dir || existing.kind != unit.kind {
                self.skipped.push(format!(
                    "unit name collision: {} ({} vs {})",
                    unit.name, existing.dir, unit.dir
                ));
            }
            return;
        }
        self.units.insert(unit.name.clone(), unit);
    }

    fn note(&mut self, adapter: AdapterKind) {
        if !self.adapters.contains(&adapter) {
            self.adapters.push(adapter);
        }
    }

    fn note_outcome(&mut self, outcome: AdapterOutcome) {
        if !self
            .outcomes
            .iter()
            .any(|existing| existing.adapter == outcome.adapter && existing.mode == outcome.mode)
        {
            self.outcomes.push(outcome);
        }
    }
}

pub fn normalize_dir(raw: &str) -> String {
    let trimmed = raw.replace('\\', "/");
    let trimmed = trimmed.trim_matches('/');
    if trimmed.is_empty() || trimmed == "." {
        String::new()
    } else {
        trimmed
            .trim_start_matches("./")
            .trim_end_matches('/')
            .to_string()
    }
}

pub fn read_text(path: &Path) -> Result<String, GraphError> {
    read_text_limited(path, MAX_ADAPTER_BYTES)
}

pub fn read_text_limited(path: &Path, max: u64) -> Result<String, GraphError> {
    let meta = fs::symlink_metadata(path)
        .map_err(|error| GraphError::Malformed(format!("{}: {error}", path.display())))?;
    if !meta.file_type().is_file() {
        return Err(GraphError::Malformed(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    if meta.len() > max {
        return Err(GraphError::Oversized {
            what: "adapter manifest",
            max: max as usize,
            actual: meta.len() as usize,
        });
    }
    let bytes = fs::read(path)
        .map_err(|error| GraphError::Malformed(format!("{}: {error}", path.display())))?;
    String::from_utf8(bytes)
        .map_err(|error| GraphError::Malformed(format!("{} is not UTF-8: {error}", path.display())))
}

fn exists_file(root: &Path, relative: &str) -> bool {
    fs::symlink_metadata(root.join(relative))
        .map(|meta| meta.file_type().is_file())
        .unwrap_or(false)
}

fn first_existing(root: &Path, candidates: &[&str]) -> Option<String> {
    candidates
        .iter()
        .find(|candidate| exists_file(root, candidate))
        .map(|candidate| (*candidate).to_string())
}

fn sorted_dirs(root: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut dirs: Vec<String> = entries
        .flatten()
        .filter(|entry| {
            entry
                .file_type()
                .map(|kind| kind.is_dir() && !kind.is_symlink())
                .unwrap_or(false)
        })
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| !name.starts_with('.') && name != "node_modules" && name != "target")
        .collect();
    dirs.sort();
    dirs
}

/// Lexical containment gate applied to a declared workspace path before any
/// filesystem call. Absolute paths and parent traversal are refused.
fn validate_pattern(raw: &str) -> Result<(), &'static str> {
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err("absolute path is outside the workspace root");
    }
    for component in path.components() {
        match component {
            Component::ParentDir => return Err("path escapes the workspace root"),
            Component::RootDir | Component::Prefix(_) => {
                return Err("absolute path is outside the workspace root")
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    Ok(())
}

fn canonical_within(root: &Path, candidate: &Path) -> Option<PathBuf> {
    let canonical = candidate.canonicalize().ok()?;
    canonical.starts_with(root).then_some(canonical)
}

/// Expand the small workspace glob subset npm/Gradle/Cargo declarations use:
/// literal directories plus a single trailing `*`/`**` segment. Every
/// directory is proven to be inside the canonical root before it is read, and
/// symlink escapes are refused rather than followed.
fn expand_dir_patterns_checked(root: &Path, patterns: &[String]) -> (Vec<String>, Vec<String>) {
    let mut out = BTreeSet::new();
    let mut skipped = Vec::new();
    for raw in patterns.iter().take(MAX_ADAPTER_UNITS) {
        if let Err(reason) = validate_pattern(raw) {
            skipped.push(format!("{raw}: {reason}"));
            continue;
        }
        let pattern = normalize_dir(raw);
        if pattern.is_empty() {
            out.insert(String::new());
            continue;
        }
        let segments: Vec<&str> = pattern.split('/').collect();
        let wildcard = segments
            .iter()
            .position(|segment| *segment == "*" || *segment == "**");
        let Some(index) = wildcard else {
            match canonical_within(root, &root.join(&pattern)) {
                Some(_) => {
                    out.insert(pattern);
                }
                None => skipped.push(format!(
                    "{pattern}: directory is missing or resolves outside the workspace root"
                )),
            }
            continue;
        };
        let base = segments[..index].join("/");
        let base_path = if base.is_empty() {
            root.to_path_buf()
        } else {
            match canonical_within(root, &root.join(&base)) {
                Some(path) => path,
                None => {
                    skipped.push(format!(
                        "{pattern}: wildcard base is missing or resolves outside the workspace root"
                    ));
                    continue;
                }
            }
        };
        let depth = segments.len().saturating_sub(index + 1);
        let mut frontier = vec![(base_path, base)];
        for _ in 0..=depth.min(MAX_ADAPTER_DEPTH) {
            let mut next = Vec::new();
            for (path, relative) in frontier {
                for entry in sorted_dirs(&path) {
                    let entry_path = path.join(&entry);
                    let Some(canonical_entry) = canonical_within(root, &entry_path) else {
                        skipped.push(format!(
                            "{relative}/{entry}: resolves outside the workspace root"
                        ));
                        continue;
                    };
                    let child = if relative.is_empty() {
                        entry.clone()
                    } else {
                        format!("{relative}/{entry}")
                    };
                    let candidate: Vec<&str> = child.split('/').collect();
                    if candidate.len() <= segments.len() {
                        if candidate.len() == segments.len() {
                            out.insert(child.clone());
                        }
                        next.push((canonical_entry, child));
                    }
                }
            }
            frontier = next;
            if frontier.is_empty() {
                break;
            }
        }
    }
    (out.into_iter().take(MAX_ADAPTER_UNITS).collect(), skipped)
}

/// Cargo/npm member patterns share the same bounded single-wildcard glob
/// expansion; kept public so the Cargo fallback and adapters agree.
pub fn expand_member_patterns(root: &Path, patterns: &[String]) -> Vec<String> {
    expand_dir_patterns_checked(root, patterns).0
}

/// Checked expansion used by the Cargo fallback so refused members are
/// reported in the build stats instead of disappearing silently.
pub(crate) fn expand_member_patterns_checked(
    root: &Path,
    patterns: &[String],
) -> (Vec<String>, Vec<String>) {
    expand_dir_patterns_checked(root, patterns)
}

fn relative_to_root(root: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(root)
        .ok()
        .map(|relative| normalize_dir(&relative.to_string_lossy()))
}

fn parse_json(text: &str, label: &str) -> Result<serde_json::Value, GraphError> {
    serde_json::from_str(text).map_err(|error| GraphError::Malformed(format!("{label}: {error}")))
}

fn json_string(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_string)
}

fn dependency_names(value: &serde_json::Value, key: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    match value.get(key) {
        Some(serde_json::Value::Object(map)) => {
            for name in map.keys().take(MAX_ADAPTER_UNITS) {
                if is_package_name(name) {
                    out.insert(name.clone());
                }
            }
        }
        Some(serde_json::Value::Array(items)) => {
            for item in items.iter().take(MAX_ADAPTER_UNITS) {
                if let Some(name) = item.as_str() {
                    if is_package_name(name) {
                        out.insert(name.to_string());
                    }
                }
            }
        }
        _ => {}
    }
    out
}

fn is_package_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && !name.chars().any(char::is_control)
        && !name.contains("..")
}

fn npm_workspace_patterns(value: &serde_json::Value) -> Vec<String> {
    let Some(workspaces) = value.get("workspaces") else {
        return Vec::new();
    };
    let list = match workspaces {
        serde_json::Value::Array(items) => items.clone(),
        serde_json::Value::Object(map) => map
            .get("packages")
            .and_then(|packages| packages.as_array())
            .cloned()
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    list.iter()
        .take(MAX_ADAPTER_UNITS)
        .filter_map(|item| item.as_str())
        .map(str::to_string)
        .collect()
}

fn npm_unit(root: &Path, dir: &str, adapter: AdapterKind) -> Option<Unit> {
    let manifest = if dir.is_empty() {
        root.join("package.json")
    } else {
        root.join(dir).join("package.json")
    };
    let text = read_text(&manifest).ok()?;
    let value = parse_json(&text, "package.json").ok()?;
    let name = json_string(&value, "name")?;
    if !is_package_name(&name) {
        return None;
    }
    let mut unit = Unit::new(name, dir, UnitKind::Package, adapter);
    for key in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        unit.deps.extend(dependency_names(&value, key));
    }
    Some(unit)
}

fn package_test_targets(root: &Path, dir: &str, candidates: &[&str]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for candidate in candidates {
        let path = if dir.is_empty() {
            (*candidate).to_string()
        } else {
            format!("{dir}/{candidate}")
        };
        if root.join(&path).is_dir() {
            out.insert(path);
        }
    }
    out
}

/// npm's resolved workspace graph from `package-lock.json` (lockfile v2/v3).
/// This is native metadata: unit membership, names and direct dependencies
/// come from npm's own resolved `packages` map, never re-inferred from
/// `workspaces` globs. A hostile package key is refused rather than followed.
fn npm_lock_topology(root: &Path) -> Result<Topology, String> {
    let lock = root.join("package-lock.json");
    let text = read_text_limited(&lock, MAX_NATIVE_METADATA_BYTES)
        .map_err(|error| format!("package-lock.json: {error}"))?;
    let value = parse_json(&text, "package-lock.json")
        .map_err(|error| format!("package-lock.json: {error}"))?;
    let version = value
        .get("lockfileVersion")
        .and_then(|value| value.as_u64())
        .ok_or_else(|| "package-lock.json has no numeric lockfileVersion".to_string())?;
    if version < 2 {
        return Err(format!(
            "lockfileVersion {version} has no workspace package graph (v2+ required)"
        ));
    }
    let packages = value
        .get("packages")
        .and_then(|value| value.as_object())
        .ok_or_else(|| "package-lock.json has no packages map".to_string())?;
    let mut topology = Topology::default();
    topology.note(AdapterKind::Npm);
    for (raw_dir, entry) in packages.iter().take(MAX_ADAPTER_UNITS.saturating_mul(4)) {
        if raw_dir.starts_with("node_modules/")
            || raw_dir.contains("/node_modules/")
            || entry.get("link").and_then(|link| link.as_bool()) == Some(true)
        {
            continue;
        }
        let dir = normalize_dir(raw_dir);
        if let Err(reason) = validate_pattern(&dir) {
            topology
                .skipped
                .push(format!("package-lock entry {raw_dir}: {reason}"));
            continue;
        }
        if !dir.is_empty() {
            match canonical_within(root, &root.join(&dir)) {
                Some(path) if path.is_dir() => {}
                _ => {
                    topology.skipped.push(format!(
                        "package-lock entry {dir}: directory is missing or escapes"
                    ));
                    continue;
                }
            }
        }
        let Some(name) = json_string(entry, "name") else {
            continue; // a linked/package entry without an identity is not a unit
        };
        if !is_package_name(&name) {
            topology
                .skipped
                .push(format!("package-lock entry {dir}: invalid package name"));
            continue;
        }
        let mut unit = Unit::new(name, &dir, UnitKind::Package, AdapterKind::Npm);
        for key in [
            "dependencies",
            "devDependencies",
            "peerDependencies",
            "optionalDependencies",
        ] {
            unit.deps.extend(dependency_names(entry, key));
        }
        unit.test_targets = package_test_targets(
            root,
            &dir,
            &["test", "tests", "__tests__", "src/test", "spec"],
        );
        topology.insert(unit);
    }
    if topology.units.is_empty() {
        return Err("package-lock.json resolved no workspace packages".into());
    }
    Ok(topology)
}

fn npm_structural(root: &Path) -> Topology {
    let mut topology = Topology::default();
    let Some(_manifest) = first_existing(root, &["package.json"]) else {
        return topology;
    };
    let Ok(text) = read_text(&root.join("package.json")) else {
        return topology;
    };
    let Ok(value) = parse_json(&text, "package.json") else {
        return topology;
    };
    topology.note(AdapterKind::Npm);
    if let Some(unit) = npm_unit(root, "", AdapterKind::Npm) {
        topology.insert(unit);
    }
    let (dirs, skipped) = expand_dir_patterns_checked(root, &npm_workspace_patterns(&value));
    topology.skipped.extend(skipped);
    for dir in dirs {
        if let Some(unit) = npm_unit(root, &dir, AdapterKind::Npm) {
            topology.insert(unit);
        } else if !exists_file(root, &format!("{dir}/package.json")) {
            topology.skipped.push(format!("{dir}/package.json"));
        }
    }
    topology
}

/// Structural npm fallback. Public callers get the documented degraded
/// outcome; the Cargo/npm composition path uses [`discover_npm_native`].
pub fn discover_npm(root: &Path) -> Topology {
    let mut topology = npm_structural(root);
    if !topology.units.is_empty() {
        topology.note_outcome(AdapterOutcome::fallback(
            AdapterKind::Npm,
            "structural package.json/workspaces inference (native npm metadata not requested)",
        ));
    }
    topology
}

/// npm adapter: native `package-lock.json` workspace graph first, bounded
/// structural inference only when that metadata is unavailable.
pub fn discover_npm_native(root: &Path, probe: &NativeProbe) -> Topology {
    let has_manifest = exists_file(root, "package.json");
    let has_lock = exists_file(root, "package-lock.json");
    if !has_manifest && !has_lock {
        return Topology::default();
    }
    if has_lock && probe.enabled {
        match npm_lock_topology(root) {
            Ok(mut topology) => {
                let lock =
                    read_text_limited(&root.join("package-lock.json"), MAX_NATIVE_METADATA_BYTES)
                        .ok()
                        .and_then(|text| {
                            parse_json(&text, "package-lock.json")
                                .ok()
                                .and_then(|value| {
                                    value
                                        .get("lockfileVersion")
                                        .and_then(|version| version.as_u64())
                                })
                        })
                        .unwrap_or(2);
                topology.note_outcome(AdapterOutcome::native(
                    AdapterKind::Npm,
                    format!("package-lock.json workspace graph (lockfileVersion {lock})"),
                ));
                return topology;
            }
            Err(reason) => {
                let mut topology = npm_structural(root);
                topology.note_outcome(AdapterOutcome::fallback(
                    AdapterKind::Npm,
                    format!("package-lock native graph unavailable: {reason}"),
                ));
                return topology;
            }
        }
    }
    let mut topology = npm_structural(root);
    if !topology.units.is_empty() {
        let reason = if probe.enabled {
            "no package-lock.json workspace graph; structural package.json/workspaces inference"
        } else {
            "native metadata disabled; structural package.json/workspaces inference"
        };
        topology.note_outcome(AdapterOutcome::fallback(AdapterKind::Npm, reason));
    }
    topology
}

fn gradle_name(dir: &str) -> String {
    if dir.is_empty() {
        "root".to_string()
    } else {
        dir.replace('/', "-")
    }
}

fn gradle_project_dir(project: &str) -> String {
    normalize_dir(
        project
            .trim()
            .trim_matches('\'')
            .trim_matches('"')
            .trim_start_matches(':')
            .replace(':', "/")
            .as_str(),
    )
}

fn quoted_list(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() && out.len() < MAX_ADAPTER_UNITS {
        let quote = bytes[index];
        if quote == b'\'' || quote == b'"' {
            let start = index + 1;
            let mut end = start;
            while end < bytes.len() && bytes[end] != quote {
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

fn gradle_includes(text: &str) -> Vec<String> {
    let mut projects = Vec::new();
    for line in text.lines().take(MAX_ADAPTER_FILES) {
        let trimmed = line.split("//").next().unwrap_or("").trim();
        if let Some(rest) = trimmed
            .strip_prefix("include(")
            .or_else(|| trimmed.strip_prefix("include "))
        {
            for project in quoted_list(rest) {
                if project.starts_with(':') {
                    projects.push(gradle_project_dir(&project));
                }
            }
        }
    }
    projects
}

fn gradle_dependencies(text: &str) -> BTreeSet<String> {
    let mut deps = BTreeSet::new();
    for line in text.lines().take(MAX_ADAPTER_FILES) {
        let trimmed = line.split("//").next().unwrap_or("").trim();
        if !(trimmed.contains("implementation")
            || trimmed.contains("api ")
            || trimmed.contains("compile")
            || trimmed.contains("runtimeOnly"))
        {
            continue;
        }
        for quoted in quoted_list(trimmed) {
            if let Some(project) = quoted.strip_prefix("project(") {
                let project = project.trim_end_matches(')');
                let dir = gradle_project_dir(project);
                if !dir.is_empty() {
                    deps.insert(dir);
                }
            } else if let Some(project) = quoted.strip_prefix(':') {
                let dir = gradle_project_dir(project);
                if !dir.is_empty() {
                    deps.insert(dir);
                }
            }
        }
        if let Some(start) = trimmed.find("project(':") {
            let rest = &trimmed[start + "project('".len()..];
            if let Some(end) = rest.find('\'') {
                let dir = gradle_project_dir(&rest[..end]);
                if !dir.is_empty() {
                    deps.insert(dir);
                }
            }
        }
        if let Some(start) = trimmed.find("project(\":") {
            let rest = &trimmed[start + "project(\"".len()..];
            if let Some(end) = rest.find('"') {
                let dir = gradle_project_dir(&rest[..end]);
                if !dir.is_empty() {
                    deps.insert(dir);
                }
            }
        }
    }
    deps
}

fn gradle_adapter(root: &Path, settings: &str, build: &str) -> Topology {
    let mut topology = Topology::default();
    topology.note(AdapterKind::Gradle);
    let Ok(settings_text) = read_text(&root.join(settings)) else {
        return topology;
    };
    let projects = gradle_includes(&settings_text);
    let mut project_dirs = BTreeSet::new();
    for dir in projects {
        project_dirs.insert(dir);
    }
    if project_dirs.is_empty() {
        project_dirs.insert(String::new());
    }
    for dir in project_dirs {
        let build_file = if dir.is_empty() {
            build.to_string()
        } else {
            let kts = format!("{dir}/build.gradle.kts");
            let groovy = format!("{dir}/build.gradle");
            if exists_file(root, &kts) {
                kts
            } else if exists_file(root, &groovy) {
                groovy
            } else {
                topology.skipped.push(format!("{dir}/build.gradle"));
                continue;
            }
        };
        let Ok(text) = read_text(&root.join(&build_file)) else {
            topology.skipped.push(build_file);
            continue;
        };
        let name = if dir.is_empty() {
            root.file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| "root".into())
        } else {
            gradle_name(&dir)
        };
        if name.is_empty() {
            continue;
        }
        let mut unit = Unit::new(name, &dir, UnitKind::Package, AdapterKind::Gradle);
        for dep in gradle_dependencies(&text) {
            unit.deps.insert(gradle_name(&dep));
        }
        for test in ["src/test/java", "src/test/kotlin", "src/test"] {
            let path = if dir.is_empty() {
                test.to_string()
            } else {
                format!("{dir}/{test}")
            };
            if root.join(&path).is_dir() {
                unit.test_targets.insert(path);
            }
        }
        topology.insert(unit);
    }
    topology
}

fn gradle_structural(root: &Path) -> Topology {
    let settings = first_existing(root, &["settings.gradle", "settings.gradle.kts"]);
    let build = first_existing(root, &["build.gradle", "build.gradle.kts"]);
    match (settings, build) {
        (Some(settings), Some(build)) => gradle_adapter(root, &settings, &build),
        (Some(settings), None) => gradle_adapter(root, &settings, "build.gradle"),
        (None, Some(build)) => gradle_adapter(root, "settings.gradle", &build),
        (None, None) => Topology::default(),
    }
}

pub fn discover_gradle(root: &Path) -> Topology {
    let mut topology = gradle_structural(root);
    if !topology.units.is_empty() {
        topology.note_outcome(AdapterOutcome::fallback(
            AdapterKind::Gradle,
            "structural settings/build evaluation (native Gradle metadata not requested)",
        ));
    }
    topology
}

/// The bounded Gradle model probe. The script only reads the project model
/// (project names/dirs and project-dependency edges); it never resolves
/// external configurations, and `--offline --no-daemon` keeps the child
/// from downloading artifacts or leaving a daemon behind.
const GRADLE_INIT_SCRIPT: &str = r#"import groovy.json.JsonOutput
gradle.projectsEvaluated {
  rootProject.tasks.register('faktorTopology') {
    doLast {
      def out = gradle.rootProject.allprojects.collect { p ->
        def deps = []
        p.configurations.findAll { c ->
          ['implementation','api','compileOnly','runtimeOnly','testImplementation','testRuntimeOnly'].contains(c.name)
        }.each { c ->
          c.dependencies.withType(ProjectDependency).each { d -> deps << d.path }
        }
        def rel = gradle.rootProject.projectDir.toPath().relativize(p.projectDir.toPath()).toString().replace('\\','/')
        [name: p.name, path: p.path, dir: rel, deps: deps.unique().sort()]
      }
      println 'FAKTOR_TOPOLOGY:' + JsonOutput.toJson(out)
    }
  }
}
"#;

fn gradle_probe(root: &Path, probe: &NativeProbe) -> Result<Topology, String> {
    let supervisor = match probe.supervisor.clone() {
        Some(supervisor) => supervisor,
        None => ProcessSupervisor::try_shared()
            .map_err(|error| format!("process supervisor unavailable: {error}"))?,
    };
    let mut script =
        tempfile::NamedTempFile::new().map_err(|error| format!("gradle init script: {error}"))?;
    script
        .write_all(GRADLE_INIT_SCRIPT.as_bytes())
        .and_then(|()| script.flush())
        .map_err(|error| format!("gradle init script: {error}"))?;
    let script_path = script
        .path()
        .to_str()
        .ok_or_else(|| "gradle init script path is not UTF-8".to_string())?
        .to_string();
    let output = supervisor
        .run_sync(
            SpawnConfig {
                cmd: probe
                    .gradle_command
                    .clone()
                    .unwrap_or_else(|| "gradle".into()),
                args: vec![
                    "--no-daemon".into(),
                    "--offline".into(),
                    "--console=plain".into(),
                    "--quiet".into(),
                    "-I".into(),
                    script_path,
                    "faktorTopology".into(),
                ],
                cwd: root.to_path_buf(),
                env: EnvSpec::toolchain(),
                owner: ProcessOwner::Daemon,
                ..Default::default()
            },
            probe.timeout,
            probe.max_bytes,
            NATIVE_STDERR_BYTES,
        )
        .map_err(|error| format!("gradle probe spawn: {error}"))?;
    if output.timed_out {
        return Err(format!(
            "gradle probe exceeded its {} ms deadline (child killed with its process group)",
            probe.timeout.as_millis()
        ));
    }
    if output.stdout_truncated {
        return Err(format!(
            "gradle probe stdout exceeded the {}-byte bound",
            probe.max_bytes
        ));
    }
    if output.exit_code != Some(0) {
        return Err(format!(
            "gradle probe exited {:?}: {}",
            output.exit_code,
            output.stderr_head.trim()
        ));
    }
    let payload = output
        .stdout_head
        .lines()
        .filter_map(|line| line.strip_prefix("FAKTOR_TOPOLOGY:"))
        .next_back()
        .ok_or_else(|| "gradle probe printed no FAKTOR_TOPOLOGY payload".to_string())?;
    let entries: Vec<serde_json::Value> = serde_json::from_str(payload)
        .map_err(|error| format!("gradle probe payload is not JSON: {error}"))?;
    let mut dir_by_path: BTreeMap<String, String> = BTreeMap::new();
    for entry in entries.iter().take(MAX_ADAPTER_UNITS) {
        let Some(path) = json_string(entry, "path") else {
            continue;
        };
        let Some(dir) = json_string(entry, "dir") else {
            continue;
        };
        let dir = normalize_dir(&dir);
        if let Err(reason) = validate_pattern(&dir) {
            return Err(format!("gradle project {path} directory refused: {reason}"));
        }
        match canonical_within(root, &root.join(&dir)) {
            Some(path) if path.is_dir() => {}
            _ => {
                return Err(format!(
                    "gradle project {path} directory is missing or escapes"
                ))
            }
        }
        dir_by_path.insert(path, dir);
    }
    let mut topology = Topology::default();
    topology.note(AdapterKind::Gradle);
    for entry in entries.iter().take(MAX_ADAPTER_UNITS) {
        let Some(path) = json_string(entry, "path") else {
            continue;
        };
        let Some(dir) = dir_by_path.get(&path).cloned() else {
            continue;
        };
        let name = if dir.is_empty() {
            root.file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| "root".into())
        } else {
            gradle_name(&dir)
        };
        if name.is_empty() || !is_package_name(&name) {
            topology
                .skipped
                .push(format!("gradle project {path}: invalid name"));
            continue;
        }
        let mut unit = Unit::new(name, &dir, UnitKind::Package, AdapterKind::Gradle);
        if let Some(deps) = entry.get("deps").and_then(|value| value.as_array()) {
            for dep in deps.iter().take(MAX_ADAPTER_UNITS) {
                let Some(dep_path) = dep.as_str() else {
                    continue;
                };
                let Some(dep_dir) = dir_by_path.get(dep_path) else {
                    topology.skipped.push(format!(
                        "gradle project {path}: unknown dependency {dep_path}"
                    ));
                    continue;
                };
                let dep_name = if dep_dir.is_empty() {
                    root.file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .unwrap_or_else(|| "root".into())
                } else {
                    gradle_name(dep_dir)
                };
                if is_package_name(&dep_name) {
                    unit.deps.insert(dep_name);
                }
            }
        }
        unit.test_targets = package_test_targets(
            root,
            &dir,
            &["src/test/java", "src/test/kotlin", "src/test"],
        );
        topology.insert(unit);
    }
    if topology.units.is_empty() {
        return Err("gradle probe resolved no projects".into());
    }
    Ok(topology)
}

/// Gradle adapter: the supervised Gradle model first (`--no-daemon
/// --offline`), the bounded settings/build reader only as a degraded
/// fallback.
pub fn discover_gradle_native(root: &Path, probe: &NativeProbe) -> Topology {
    let has_build = exists_file(root, "settings.gradle")
        || exists_file(root, "settings.gradle.kts")
        || exists_file(root, "build.gradle")
        || exists_file(root, "build.gradle.kts");
    if !has_build {
        return Topology::default();
    }
    if probe.enabled {
        match gradle_probe(root, probe) {
            Ok(mut topology) => {
                topology.note_outcome(AdapterOutcome::native(
                    AdapterKind::Gradle,
                    "supervised gradle --no-daemon --offline topology probe",
                ));
                return topology;
            }
            Err(reason) => {
                let mut topology = gradle_structural(root);
                topology.note_outcome(AdapterOutcome::fallback(
                    AdapterKind::Gradle,
                    format!("gradle native model unavailable: {reason}"),
                ));
                return topology;
            }
        }
    }
    let mut topology = gradle_structural(root);
    if !topology.units.is_empty() {
        topology.note_outcome(AdapterOutcome::fallback(
            AdapterKind::Gradle,
            "native metadata disabled; structural settings/build evaluation",
        ));
    }
    topology
}

fn xml_text_between<'a>(text: &'a str, open: &str, close: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut rest = text;
    while out.len() < MAX_ADAPTER_UNITS {
        let Some(start) = rest.find(open) else {
            break;
        };
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(close) else {
            break;
        };
        out.push(&after[..end]);
        rest = &after[end + close.len()..];
    }
    out
}

fn txt(text: &str, tag: &str) -> Option<String> {
    xml_text_between(text, &format!("<{tag}>"), &format!("</{tag}>"))
        .into_iter()
        .next()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn xml_block<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    let end = text[start..].find(close)? + start;
    Some(&text[start..end])
}

fn xml_remove_blocks(text: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(open) {
        out.push_str(&rest[..start]);
        let after = &rest[start + open.len()..];
        match after.find(close) {
            Some(end) => rest = &after[end + close.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Parse the direct children of one XML block into `(tag, text)` pairs.
/// Nested markup is ignored (Maven property values are plain text).
fn xml_pairs(block: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = block;
    while out.len() < MAX_ADAPTER_UNITS {
        let Some(start) = rest.find('<') else {
            break;
        };
        let after = &rest[start + 1..];
        let Some(end) = after.find('>') else {
            break;
        };
        let tag = after[..end].trim().trim_end_matches('/').to_string();
        rest = &after[end + 1..];
        if tag.is_empty() || tag.starts_with('/') || tag.starts_with('?') || tag.starts_with('!') {
            continue;
        }
        let closing = format!("</{tag}>");
        let Some(close_at) = rest.find(&closing) else {
            continue;
        };
        out.push((tag, rest[..close_at].trim().to_string()));
        rest = &rest[close_at + closing.len()..];
    }
    out
}

#[derive(Debug, Clone, Default)]
struct RawPom {
    group: Option<String>,
    artifact: Option<String>,
    version: Option<String>,
    parent_group: Option<String>,
    parent_artifact: Option<String>,
    parent_version: Option<String>,
    parent_relative: Option<String>,
    modules: Vec<String>,
    deps: Vec<(String, String)>,
    props: BTreeMap<String, String>,
}

fn read_raw_pom(root: &Path, dir: &str) -> Result<RawPom, String> {
    let path = if dir.is_empty() {
        root.join("pom.xml")
    } else {
        root.join(dir).join("pom.xml")
    };
    let text = read_text(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut raw = RawPom::default();
    if let Some(parent) = xml_block(&text, "<parent>", "</parent>") {
        raw.parent_group = txt(parent, "groupId");
        raw.parent_artifact = txt(parent, "artifactId");
        raw.parent_version = txt(parent, "version");
        raw.parent_relative = txt(parent, "relativePath");
    }
    let project = xml_remove_blocks(&text, "<parent>", "</parent>");
    raw.group = txt(&project, "groupId");
    raw.artifact = txt(&project, "artifactId");
    raw.version = txt(&project, "version");
    if let Some(modules) = xml_block(&project, "<modules>", "</modules>") {
        raw.modules = xml_text_between(modules, "<module>", "</module>")
            .into_iter()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .take(MAX_ADAPTER_UNITS)
            .collect();
    }
    if let Some(properties) = xml_block(&project, "<properties>", "</properties>") {
        for (key, value) in xml_pairs(properties).into_iter().take(MAX_ADAPTER_UNITS) {
            if !key.is_empty() && !value.contains('<') {
                raw.props.insert(key, value);
            }
        }
    }
    // Dependency management and build tooling declare versions/plugins, not
    // project dependencies; profile dependencies are conditional and are not
    // part of the default model.
    let dependency_text = xml_remove_blocks(
        &project,
        "<dependencyManagement>",
        "</dependencyManagement>",
    );
    let dependency_text = xml_remove_blocks(&dependency_text, "<build>", "</build>");
    let dependency_text = xml_remove_blocks(&dependency_text, "<profiles>", "</profiles>");
    for block in xml_text_between(&dependency_text, "<dependency>", "</dependency>") {
        let group = txt(block, "groupId").unwrap_or_default();
        let artifact = txt(block, "artifactId").unwrap_or_default();
        if !artifact.is_empty() {
            raw.deps.push((group, artifact));
        }
    }
    Ok(raw)
}

/// Lexically join a `..`-bearing Maven relative path while proving the result
/// stays at or below the workspace root. Absolute paths and root escapes are
/// refused (Maven modules outside the repository are not followed).
fn normalize_relative(base: &str, relative: &str) -> Result<String, String> {
    if relative.starts_with('/') || relative.starts_with('\\') {
        return Err("absolute module path".into());
    }
    let mut stack: Vec<String> = if base.is_empty() {
        Vec::new()
    } else {
        base.split('/').map(str::to_string).collect()
    };
    for component in Path::new(relative).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if stack.pop().is_none() {
                    return Err("path escapes the workspace root".into());
                }
            }
            Component::Normal(part) => stack.push(part.to_string_lossy().to_string()),
            Component::RootDir | Component::Prefix(_) => return Err("absolute module path".into()),
        }
    }
    Ok(normalize_dir(&stack.join("/")))
}

fn interpolate(value: &str, context: &BTreeMap<String, String>) -> String {
    let mut out = value.to_string();
    for _ in 0..4 {
        let Some(start) = out.find("${") else {
            break;
        };
        let Some(end) = out[start + 2..].find('}') else {
            break;
        };
        let key = &out[start + 2..start + 2 + end];
        let Some(replacement) = context.get(key) else {
            break;
        };
        out.replace_range(start..start + 2 + end + 1, replacement);
    }
    out
}

#[derive(Debug, Clone)]
struct ResolvedPom {
    dir: String,
    group: String,
    artifact: String,
    version: String,
    modules: Vec<String>,
    deps: Vec<String>,
    props: BTreeMap<String, String>,
}

fn resolve_pom(
    root: &Path,
    dir: &str,
    cache: &mut BTreeMap<String, ResolvedPom>,
    visiting: &mut BTreeSet<String>,
    depth: usize,
) -> Result<ResolvedPom, String> {
    if let Some(cached) = cache.get(dir) {
        return Ok(cached.clone());
    }
    if depth > MAX_ADAPTER_DEPTH || !visiting.insert(dir.to_string()) {
        return Err(format!("pom parent chain is cyclic or too deep at {dir}"));
    }
    let raw = read_raw_pom(root, dir)?;
    let mut parent = None;
    if raw.parent_artifact.is_some() {
        let relative = raw
            .parent_relative
            .clone()
            .unwrap_or_else(|| "../pom.xml".to_string());
        let parent_file = if relative.ends_with(".xml") {
            normalize_relative(dir, &relative)
        } else {
            normalize_relative(dir, &format!("{relative}/pom.xml"))
        };
        if let Ok(parent_dir) = parent_file {
            let parent_pom = parent_dir
                .strip_suffix("/pom.xml")
                .or_else(|| parent_dir.strip_suffix("pom.xml"))
                .unwrap_or(&parent_dir)
                .trim_end_matches('/')
                .to_string();
            if root.join(&parent_pom).join("pom.xml").is_file() {
                parent = Some(resolve_pom(root, &parent_pom, cache, visiting, depth + 1)?);
            }
        }
    }
    let mut props = parent
        .as_ref()
        .map(|parent| parent.props.clone())
        .unwrap_or_default();
    props.extend(raw.props.clone());
    let inherited_group = parent
        .as_ref()
        .map(|parent| parent.group.clone())
        .or_else(|| raw.parent_group.clone())
        .unwrap_or_default();
    let inherited_version = parent
        .as_ref()
        .map(|parent| parent.version.clone())
        .or_else(|| raw.parent_version.clone())
        .unwrap_or_default();
    let artifact = raw.artifact.clone().unwrap_or_default();
    let mut context = props.clone();
    context.insert("project.groupId".into(), inherited_group.clone());
    context.insert("pom.groupId".into(), inherited_group.clone());
    context.insert("project.parent.groupId".into(), inherited_group.clone());
    context.insert("project.version".into(), inherited_version.clone());
    context.insert("pom.version".into(), inherited_version.clone());
    context.insert("project.parent.version".into(), inherited_version.clone());
    context.insert("project.artifactId".into(), artifact.clone());
    let group = interpolate(raw.group.as_deref().unwrap_or(&inherited_group), &context);
    let version = interpolate(
        raw.version.as_deref().unwrap_or(&inherited_version),
        &context,
    );
    context.insert("project.groupId".into(), group.clone());
    context.insert("project.version".into(), version.clone());
    let artifact = interpolate(&artifact, &context);
    let mut modules = Vec::new();
    for module in raw.modules.iter().take(MAX_ADAPTER_UNITS) {
        let module = interpolate(module, &context);
        match normalize_relative(dir, &module) {
            Ok(module_dir) => modules.push(module_dir),
            Err(reason) => {
                // Keep the raw module text in the error path; the caller
                // records it as skipped.
                modules.push(format!("!{module}: {reason}"));
            }
        }
    }
    let mut deps: BTreeSet<String> = parent
        .as_ref()
        .map(|parent| parent.deps.iter().cloned().collect())
        .unwrap_or_default();
    for (group, artifact) in raw.deps.iter().take(MAX_ADAPTER_UNITS) {
        let group = interpolate(group, &context);
        let artifact = interpolate(artifact, &context);
        if artifact.is_empty() || artifact.contains("${") {
            continue;
        }
        deps.insert(if group.is_empty() || group.contains("${") {
            artifact
        } else {
            format!("{group}:{artifact}")
        });
    }
    let resolved = ResolvedPom {
        dir: dir.to_string(),
        group,
        artifact,
        version,
        modules,
        deps: deps.into_iter().collect(),
        props,
    };
    cache.insert(dir.to_string(), resolved.clone());
    visiting.remove(dir);
    Ok(resolved)
}

fn maven_topology(root: &Path) -> Result<Topology, String> {
    if !exists_file(root, "pom.xml") {
        return Ok(Topology::default());
    }
    let mut cache = BTreeMap::new();
    let mut visiting = BTreeSet::new();
    let mut topology = Topology::default();
    topology.note(AdapterKind::Maven);
    let mut queue = vec![String::new()];
    let mut seen = BTreeSet::new();
    while let Some(dir) = queue.pop() {
        if !seen.insert(dir.clone()) {
            continue;
        }
        if seen.len() > MAX_ADAPTER_UNITS {
            topology.skipped.push("maven module bound reached".into());
            break;
        }
        let resolved = match resolve_pom(root, &dir, &mut cache, &mut visiting, 0) {
            Ok(resolved) => resolved,
            Err(reason) => {
                topology.skipped.push(format!("{dir}/pom.xml: {reason}"));
                continue;
            }
        };
        if resolved.artifact.is_empty() || !is_package_name(&resolved.artifact) {
            topology
                .skipped
                .push(format!("{dir}/pom.xml: missing or invalid artifactId"));
            continue;
        }
        let mut unit = Unit::new(
            resolved.artifact.clone(),
            &resolved.dir,
            UnitKind::Package,
            AdapterKind::Maven,
        );
        unit.deps = resolved.deps.iter().cloned().collect();
        unit.test_targets =
            package_test_targets(root, &resolved.dir, &["src/test/java", "src/test/kotlin"]);
        topology.insert(unit);
        for module in resolved.modules.iter().take(MAX_ADAPTER_UNITS) {
            if let Some(module) = module.strip_prefix('!') {
                topology.skipped.push(format!("maven module: {module}"));
                continue;
            }
            if !root.join(module).join("pom.xml").is_file() {
                topology.skipped.push(format!("{module}/pom.xml"));
                continue;
            }
            queue.push(module.clone());
        }
    }
    Ok(topology)
}

pub fn discover_maven(root: &Path) -> Topology {
    let mut topology = maven_topology(root).unwrap_or_else(|reason| {
        let mut topology = Topology::default();
        topology.note(AdapterKind::Maven);
        topology.skipped.push(reason);
        topology
    });
    if !topology.units.is_empty() {
        topology.note_outcome(AdapterOutcome::fallback(
            AdapterKind::Maven,
            "bounded recursive pom model (Maven offline plugin resolution is not portable; no daemon)",
        ));
    }
    topology
}

/// Maven adapter entry used by workspace composition. There is no practical
/// daemon-free Maven model dump that works offline without a cached plugin,
/// so the recursive pom model is the documented degraded path; it still
/// resolves parents, properties, nested modules and local dependency edges.
pub fn discover_maven_native(root: &Path, _probe: &NativeProbe) -> Topology {
    discover_maven(root)
}

fn strip_toml_comment(line: &str) -> &str {
    let mut quote = None;
    for (index, ch) in line.char_indices() {
        match ch {
            '"' | '\'' if quote == Some(ch) => quote = None,
            '"' | '\'' if quote.is_none() => quote = Some(ch),
            '#' if quote.is_none() => return &line[..index],
            _ => {}
        }
    }
    line
}

fn toml_string_value(value: &str) -> Option<String> {
    let value = value.trim();
    let inner = value
        .strip_prefix('"')
        .and_then(|value| value.split('"').next())
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.split('\'').next())
        })?;
    (!inner.is_empty()).then(|| inner.to_string())
}

#[derive(Debug, Default)]
struct PyProject {
    name: Option<String>,
    deps: BTreeSet<String>,
    dependencies_dynamic: bool,
    requirement_files: Vec<String>,
}

fn add_requirement(target: &mut BTreeSet<String>, raw: &str) {
    if let Some(name) = python_dependency_name(raw) {
        target.insert(name);
    }
}

fn parse_pyproject(text: &str) -> PyProject {
    let mut project = PyProject::default();
    let mut section = String::new();
    // (kind, key) while a multi-line TOML array is being collected.
    let mut collecting: Option<(String, String)> = None;
    for raw_line in text.lines().take(MAX_ADAPTER_FILES) {
        let line = strip_toml_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        if let Some((kind, key)) = collecting.clone() {
            match kind.as_str() {
                "deps" => {
                    for value in quoted_list(line) {
                        add_requirement(&mut project.deps, &value);
                    }
                }
                "files" => {
                    project.requirement_files.extend(quoted_list(line));
                }
                "dynamic" => {
                    for value in quoted_list(line) {
                        if value == "dependencies" {
                            project.dependencies_dynamic = true;
                        }
                    }
                }
                _ => {}
            }
            if line.contains(']') {
                collecting = None;
            }
            let _ = key;
            continue;
        }
        if line.starts_with('[') && line.contains(']') {
            section = line.trim_matches('[').trim_matches(']').trim().to_string();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            if section == "tool.poetry.dependencies"
                || (section.starts_with("tool.poetry.group.") && section.ends_with(".dependencies"))
            {
                // Poetry dependency names may be bare keys with inline
                // tables; the split above still handles them.
            }
            continue;
        };
        let key = key.trim().trim_matches('"').trim_matches('\'');
        let value = value.trim();
        match section.as_str() {
            "project" => match key {
                "name" => project.name = toml_string_value(value),
                "dependencies" => {
                    for dep in quoted_list(value) {
                        add_requirement(&mut project.deps, &dep);
                    }
                    if !value.contains(']') {
                        collecting = Some(("deps".into(), key.into()));
                    }
                }
                "dynamic" => {
                    for dynamic in quoted_list(value) {
                        if dynamic == "dependencies" {
                            project.dependencies_dynamic = true;
                        }
                    }
                    if !value.contains(']') {
                        collecting = Some(("dynamic".into(), key.into()));
                    }
                }
                _ => {}
            },
            "project.optional-dependencies" => {
                for dep in quoted_list(value) {
                    add_requirement(&mut project.deps, &dep);
                }
                if !value.contains(']') {
                    collecting = Some(("deps".into(), key.into()));
                }
            }
            "tool.poetry" if key == "name" => project.name = toml_string_value(value),
            "tool.poetry.dependencies"
            | "tool.poetry.dev-dependencies"
            | "tool.poetry.group.dev.dependencies" => {
                if key != "python" {
                    add_requirement(&mut project.deps, key);
                }
            }
            section
                if section.starts_with("tool.poetry.group.")
                    && section.ends_with(".dependencies") =>
            {
                if key != "python" {
                    add_requirement(&mut project.deps, key);
                }
            }
            "tool.setuptools.dynamic" if key == "dependencies" => {
                for file in quoted_list(value) {
                    project.requirement_files.push(file);
                }
                if !value.contains(']') {
                    collecting = Some(("files".into(), key.into()));
                }
            }
            _ => {}
        }
    }
    project
}

fn normalize_project_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut separator = false;
    for ch in name.chars() {
        if matches!(ch, '-' | '_' | '.') {
            separator = true;
            continue;
        }
        if separator && !out.is_empty() {
            out.push('-');
        }
        separator = false;
        out.push(ch.to_ascii_lowercase());
    }
    out
}

/// Installed distribution metadata (`*.dist-info/METADATA` or
/// `*.egg-info/PKG-INFO`) for this project. The walk is bounded and sorted;
/// a name match is preferred over the first file, so a vendored fixture
/// cannot shadow the project's own metadata.
fn installed_python_metadata(
    root: &Path,
    expected: Option<&str>,
) -> Option<(String, BTreeSet<String>)> {
    let files = walk_files(root, "", &BTreeSet::new());
    let mut candidates: Vec<(bool, String, String, BTreeSet<String>)> = Vec::new();
    for path in files.into_iter().take(MAX_ADAPTER_FILES) {
        let metadata = path.ends_with("/METADATA") && path.contains(".dist-info/")
            || path.ends_with("/PKG-INFO") && path.contains(".egg-info/");
        if !metadata {
            continue;
        }
        let Ok(text) = read_text(&root.join(&path)) else {
            continue;
        };
        let mut name = None;
        let mut deps = BTreeSet::new();
        let mut last_was_requirement = false;
        for line in text.lines().take(MAX_ADAPTER_FILES) {
            if line.trim().is_empty() {
                break;
            }
            if line.starts_with(' ') && last_was_requirement {
                continue; // folded continuation of the previous header
            }
            let Some((header, value)) = line.split_once(':') else {
                last_was_requirement = false;
                continue;
            };
            match header {
                "Name" => name = Some(value.trim().to_string()),
                "Requires-Dist" => {
                    add_requirement(&mut deps, value);
                    last_was_requirement = true;
                }
                _ => last_was_requirement = false,
            }
        }
        if let Some(name) = name {
            let matches = expected
                .map(|expected| normalize_project_name(expected) == normalize_project_name(&name))
                .unwrap_or(false);
            candidates.push((matches, name, path, deps));
        }
    }
    candidates.sort_by(|a, b| (!a.0, &a.2).cmp(&(!b.0, &b.2)));
    candidates
        .into_iter()
        .next()
        .map(|(_, name, _, deps)| (name, deps))
}

fn read_requirement_files(root: &Path, files: &[String]) -> BTreeSet<String> {
    let mut deps = BTreeSet::new();
    for file in files.iter().take(64) {
        let relative = match validate_pattern(file) {
            Ok(()) => normalize_dir(file),
            Err(_) => continue,
        };
        let Some(path) = canonical_within(root, &root.join(&relative)) else {
            continue;
        };
        let Ok(text) = read_text(&path) else {
            continue;
        };
        for line in text.lines().take(MAX_ADAPTER_FILES) {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() || line.starts_with('-') {
                continue;
            }
            add_requirement(&mut deps, line);
        }
    }
    deps
}

fn setup_cfg_project(text: &str) -> (Option<String>, BTreeSet<String>) {
    let mut name = None;
    let mut deps = BTreeSet::new();
    let mut section = String::new();
    let mut collecting = false;
    for line in text.lines().take(MAX_ADAPTER_FILES) {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section = trimmed
                .trim_matches('[')
                .trim_matches(']')
                .trim()
                .to_string();
            collecting = false;
            continue;
        }
        if section == "metadata" {
            if let Some((key, value)) = trimmed.split_once('=') {
                if key.trim() == "name" {
                    name = Some(value.trim().to_string()).filter(|value| !value.is_empty());
                }
            }
            continue;
        }
        if section != "options" {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once('=') {
            if key.trim() == "install_requires" {
                collecting = true;
                for dep in value.split(',') {
                    add_requirement(&mut deps, dep);
                }
                continue;
            }
        }
        if collecting {
            if line.starts_with(' ') || line.starts_with('\t') {
                for dep in trimmed.split(',') {
                    add_requirement(&mut deps, dep);
                }
            } else {
                collecting = false;
            }
        }
    }
    (name, deps)
}

fn setup_py_project(text: &str) -> (Option<String>, BTreeSet<String>) {
    let name = text
        .split("name=")
        .nth(1)
        .and_then(|rest| quoted_list(rest).into_iter().next());
    let mut deps = BTreeSet::new();
    if let Some(rest) = text.split("install_requires").nth(1) {
        for dep in quoted_list(rest) {
            add_requirement(&mut deps, &dep);
        }
    }
    (name, deps)
}

fn python_dependency_name(raw: &str) -> Option<String> {
    let name = raw
        .trim()
        .split(|ch: char| {
            ch == '['
                || ch == ';'
                || ch == '='
                || ch == '<'
                || ch == '>'
                || ch == '!'
                || ch == '~'
                || ch == ' '
                || ch == ','
        })
        .next()?
        .trim();
    if is_package_name(name) {
        Some(name.to_string())
    } else {
        None
    }
}

fn python_unit(root: &Path, native: &mut Option<String>) -> Option<Unit> {
    let mut name = None;
    let mut deps = BTreeSet::new();
    if exists_file(root, "pyproject.toml") {
        let text = read_text(&root.join("pyproject.toml")).ok()?;
        let project = parse_pyproject(&text);
        name = project.name.clone();
        deps = project.deps.clone();
        if project.dependencies_dynamic {
            if let Some((metadata_name, metadata_deps)) =
                installed_python_metadata(root, name.as_deref())
            {
                name = Some(metadata_name);
                deps = metadata_deps;
                *native = Some("pyproject.toml + installed distribution metadata".into());
            } else {
                *native = Some("pyproject.toml (dynamic dependencies unresolved)".into());
            }
        } else if name.is_some() {
            *native = Some("pyproject.toml (PEP 621/Poetry metadata)".into());
        }
        deps.extend(read_requirement_files(root, &project.requirement_files));
    }
    if name.is_none() {
        if let Some((metadata_name, metadata_deps)) =
            installed_python_metadata(root, name.as_deref())
        {
            name = Some(metadata_name);
            deps = metadata_deps;
            *native = Some("installed distribution metadata".into());
        }
    }
    if name.is_none() && exists_file(root, "setup.cfg") {
        let text = read_text(&root.join("setup.cfg")).ok()?;
        let (setup_name, setup_deps) = setup_cfg_project(&text);
        name = setup_name;
        deps = setup_deps;
    }
    if name.is_none() && exists_file(root, "setup.py") {
        let text = read_text(&root.join("setup.py")).ok()?;
        let (setup_name, setup_deps) = setup_py_project(&text);
        name = setup_name;
        deps = setup_deps;
    }
    let name = name?;
    if !is_package_name(&name) {
        return None;
    }
    let mut unit = Unit::new(name, "", UnitKind::Package, AdapterKind::Python);
    unit.deps = deps;
    unit.test_targets = package_test_targets(root, "", &["tests", "test"]);
    Some(unit)
}

pub fn discover_python(root: &Path) -> Topology {
    let mut topology = Topology::default();
    if !(exists_file(root, "pyproject.toml")
        || exists_file(root, "setup.py")
        || exists_file(root, "setup.cfg"))
    {
        return topology;
    }
    topology.note(AdapterKind::Python);
    let mut native = None;
    if let Some(unit) = python_unit(root, &mut native) {
        topology.insert(unit);
    }
    let detail = native.unwrap_or_else(|| "structural setup.cfg/setup.py inference".into());
    topology.note_outcome(if detail.starts_with("structural") {
        AdapterOutcome::fallback(AdapterKind::Python, detail)
    } else {
        AdapterOutcome::native(AdapterKind::Python, detail)
    });
    topology
}

/// Python adapter: pyproject/installed metadata first, setup.cfg/setup.py
/// structural inference only as the documented degraded fallback.
pub fn discover_python_native(root: &Path, probe: &NativeProbe) -> Topology {
    if !probe.enabled {
        return discover_python(root);
    }
    discover_python(root)
}

pub fn generic_filesystem(root: &Path) -> Topology {
    let mut topology = Topology::default();
    topology.note(AdapterKind::GenericFilesystem);
    let name = root
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .filter(|name| is_package_name(name))
        .unwrap_or_else(|| "repository".to_string());
    topology.insert(Unit::new(
        name,
        "",
        UnitKind::Package,
        AdapterKind::GenericFilesystem,
    ));
    topology
}

/// Expand a directory walk for one unit, sorted and bounded.
pub fn walk_files(root: &Path, dir: &str, excluded: &BTreeSet<String>) -> Vec<String> {
    let base = if dir.is_empty() {
        root.to_path_buf()
    } else {
        root.join(dir)
    };
    let mut out = Vec::new();
    let mut stack = vec![(base, 0usize)];
    while let Some((current, depth)) = stack.pop() {
        if depth > MAX_ADAPTER_DEPTH || out.len() >= MAX_ADAPTER_FILES {
            break;
        }
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == ".git"
                || name == "target"
                || name == "node_modules"
                || name.starts_with('.')
                || name == "build"
                || name == "dist"
            {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            if kind.is_dir() {
                let relative = relative_to_root(root, &path).unwrap_or_default();
                if excluded.contains(&relative) {
                    continue;
                }
                stack.push((path, depth + 1));
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let Some(relative) = relative_to_root(root, &path) else {
                continue;
            };
            out.push(relative);
            if out.len() >= MAX_ADAPTER_FILES {
                break;
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

pub fn manifest_path_for(dir: &str, file: &str) -> PathBuf {
    if dir.is_empty() {
        PathBuf::from(file)
    } else {
        PathBuf::from(dir).join(file)
    }
}
