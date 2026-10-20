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
//! repository bytes. Adapters never spawn processes; the Cargo adapter is
//! served by the supervised `cargo metadata` path in [`crate::manifest`].

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::model::GraphError;

pub const MAX_ADAPTER_BYTES: u64 = 512 * 1024;
pub const MAX_ADAPTER_UNITS: usize = 4096;
pub const MAX_ADAPTER_DEPTH: usize = 6;
pub const MAX_ADAPTER_FILES: usize = 8192;

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
    let meta = fs::symlink_metadata(path)
        .map_err(|error| GraphError::Malformed(format!("{}: {error}", path.display())))?;
    if !meta.file_type().is_file() {
        return Err(GraphError::Malformed(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    if meta.len() > MAX_ADAPTER_BYTES {
        return Err(GraphError::Oversized {
            what: "adapter manifest",
            max: MAX_ADAPTER_BYTES as usize,
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

pub fn discover_npm(root: &Path) -> Topology {
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

pub fn discover_gradle(root: &Path) -> Topology {
    let settings = first_existing(root, &["settings.gradle", "settings.gradle.kts"]);
    let build = first_existing(root, &["build.gradle", "build.gradle.kts"]);
    match (settings, build) {
        (Some(settings), Some(build)) => gradle_adapter(root, &settings, &build),
        (Some(settings), None) => gradle_adapter(root, &settings, "build.gradle"),
        (None, Some(build)) => gradle_adapter(root, "settings.gradle", &build),
        (None, None) => Topology::default(),
    }
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

fn maven_module_dirs(text: &str) -> Vec<String> {
    xml_text_between(text, "<module>", "</module>")
        .into_iter()
        .map(normalize_dir)
        .filter(|dir| !dir.is_empty() && !dir.contains(".."))
        .take(MAX_ADAPTER_UNITS)
        .collect()
}

fn txt(text: &str, tag: &str) -> Option<String> {
    xml_text_between(text, &format!("<{tag}>"), &format!("</{tag}>"))
        .into_iter()
        .next()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn maven_dependencies(text: &str) -> BTreeSet<String> {
    let mut deps = BTreeSet::new();
    for block in xml_text_between(text, "<dependency>", "</dependency>") {
        if txt(block, "scope").as_deref() == Some("test") {
            // Test-scope dependencies still influence the test target; keep
            // them as external packages but they do not change unit names.
        }
        let group = txt(block, "groupId").unwrap_or_default();
        let artifact = txt(block, "artifactId").unwrap_or_default();
        if artifact.is_empty() {
            continue;
        }
        let name = if group.is_empty() {
            artifact
        } else {
            format!("{group}:{artifact}")
        };
        deps.insert(name);
    }
    deps
}

fn maven_unit(root: &Path, dir: &str) -> Option<Unit> {
    let manifest = if dir.is_empty() {
        root.join("pom.xml")
    } else {
        root.join(dir).join("pom.xml")
    };
    let text = read_text(&manifest).ok()?;
    let artifact = txt(&text, "artifactId")?;
    let name = artifact.clone();
    let mut unit = Unit::new(name, dir, UnitKind::Package, AdapterKind::Maven);
    unit.deps = maven_dependencies(&text);
    for test in ["src/test/java", "src/test/kotlin"] {
        let path = if dir.is_empty() {
            test.to_string()
        } else {
            format!("{dir}/{test}")
        };
        if root.join(&path).is_dir() {
            unit.test_targets.insert(path);
        }
    }
    let _ = artifact;
    Some(unit)
}

pub fn discover_maven(root: &Path) -> Topology {
    let mut topology = Topology::default();
    if !exists_file(root, "pom.xml") {
        return topology;
    }
    let Ok(text) = read_text(&root.join("pom.xml")) else {
        return topology;
    };
    topology.note(AdapterKind::Maven);
    if let Some(unit) = maven_unit(root, "") {
        topology.insert(unit);
    }
    for dir in maven_module_dirs(&text) {
        if let Some(unit) = maven_unit(root, &dir) {
            topology.insert(unit);
        } else {
            topology.skipped.push(format!("{dir}/pom.xml"));
        }
    }
    topology
}

fn toml_string(text: &str, key: &str) -> Option<String> {
    for line in text.lines().take(MAX_ADAPTER_FILES) {
        let trimmed = line.split('#').next().unwrap_or("").trim();
        let Some((candidate, value)) = trimmed.split_once('=') else {
            continue;
        };
        if candidate.trim().trim_matches('"') != key {
            continue;
        }
        let value = value.trim();
        if let Some(inner) = value.strip_prefix('"').and_then(|v| v.split('"').next()) {
            if !inner.is_empty() {
                return Some(inner.to_string());
            }
        }
        if let Some(inner) = value.strip_prefix('\'').and_then(|v| v.split('\'').next()) {
            if !inner.is_empty() {
                return Some(inner.to_string());
            }
        }
    }
    None
}

fn toml_array_strings(text: &str, key: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut collecting = false;
    for line in text.lines().take(MAX_ADAPTER_FILES) {
        let trimmed = line.split('#').next().unwrap_or("").trim();
        if !collecting {
            let Some(rest) = trimmed.strip_prefix(key) else {
                continue;
            };
            let Some(rest) = rest.trim_start().strip_prefix('=') else {
                continue;
            };
            let rest = rest.trim();
            collecting = true;
            for value in quoted_list(rest) {
                out.push(value);
            }
            if rest.contains(']') {
                collecting = false;
            }
            continue;
        }
        for value in quoted_list(trimmed) {
            out.push(value);
        }
        if trimmed.contains(']') {
            break;
        }
    }
    out.into_iter().take(MAX_ADAPTER_UNITS).collect()
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
        })
        .next()?
        .trim();
    if is_package_name(name) {
        Some(name.to_string())
    } else {
        None
    }
}

fn python_unit(root: &Path) -> Option<Unit> {
    let mut name = None;
    let mut deps = BTreeSet::new();
    if exists_file(root, "pyproject.toml") {
        let text = read_text(&root.join("pyproject.toml")).ok()?;
        name = toml_string(&text, "name");
        for dep in toml_array_strings(&text, "dependencies") {
            if let Some(name) = python_dependency_name(&dep) {
                deps.insert(name);
            }
        }
    }
    if name.is_none() && exists_file(root, "setup.py") {
        let text = read_text(&root.join("setup.py")).ok()?;
        name = toml_string(&text, "name").or_else(|| {
            text.split("name=")
                .nth(1)
                .and_then(|rest| quoted_list(rest).into_iter().next())
        });
        if let Some(rest) = text.split("install_requires").nth(1) {
            for dep in quoted_list(rest) {
                if let Some(name) = python_dependency_name(&dep) {
                    deps.insert(name);
                }
            }
        }
    }
    if name.is_none() && exists_file(root, "setup.cfg") {
        let text = read_text(&root.join("setup.cfg")).ok()?;
        name = toml_string(&text, "name");
        if let Some(rest) = text.split("install_requires").nth(1) {
            for dep in rest.lines().take(MAX_ADAPTER_UNITS) {
                let trimmed = dep.trim();
                if trimmed.is_empty() || trimmed.starts_with('[') {
                    continue;
                }
                if let Some(name) = python_dependency_name(trimmed) {
                    deps.insert(name);
                }
            }
        }
    }
    let name = name?;
    if !is_package_name(&name) {
        return None;
    }
    let mut unit = Unit::new(name, "", UnitKind::Package, AdapterKind::Python);
    unit.deps = deps;
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
    if let Some(unit) = python_unit(root) {
        topology.insert(unit);
    }
    topology
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
