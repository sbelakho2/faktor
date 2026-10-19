//! Cargo workspace and package-dependency adapter.
//!
//! Only the structural subset the graph needs is parsed: workspace members,
//! package names and dependency keys (normal, build, dev and target-specific
//! tables). Manifest reads are byte-bounded; a missing or malformed member is
//! skipped visibly in the build stats rather than fabricating a crate.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::model::GraphError;

pub const MAX_MANIFEST_BYTES: u64 = 512 * 1024;
pub const MAX_MEMBERS: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrateInfo {
    pub name: String,
    pub dir: String,
    /// Workspace crate/package names this crate depends on.
    pub deps: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub root: PathBuf,
    pub crates: BTreeMap<String, CrateInfo>,
    /// External package names (dependency keys that are not workspace
    /// members).
    pub packages: BTreeSet<String>,
    pub skipped: Vec<String>,
}

pub fn read_bounded(path: &Path, max: u64) -> Result<String, GraphError> {
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
    deps: BTreeSet<String>,
}

fn parse_manifest(text: &str) -> Manifest {
    let mut out = Manifest::default();
    let mut section = String::new();
    let mut in_members = false;
    for line in text.lines() {
        let trimmed = line.split('#').next().unwrap_or("").trim();
        if trimmed.is_empty() {
            continue;
        }
        if in_members {
            for quoted in quoted_strings(trimmed) {
                out.members.push(quoted);
            }
            if trimmed.contains(']') {
                in_members = false;
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('[') {
            if let Some(header) = rest.split(']').next() {
                section = header.trim().to_string();
                if section == "workspace" && trimmed.contains("members") {
                    in_members = true;
                }
            }
            continue;
        }
        if section == "workspace" && trimmed.starts_with("members") {
            in_members = !trimmed.contains(']');
            for quoted in quoted_strings(trimmed) {
                out.members.push(quoted);
            }
            continue;
        }
        let Some((key, _value)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if section == "package" && key == "name" {
            if let Some(name) = quoted_strings(trimmed).into_iter().next() {
                out.package = Some(name);
            }
            continue;
        }
        if is_dependency_section(&section) {
            let dep = key
                .split('.')
                .next()
                .unwrap_or(key)
                .trim()
                .trim_start_matches('"')
                .trim_end_matches('"');
            if is_dep_name(dep) {
                out.deps.insert(dep.to_string());
            }
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
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
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

fn expand_members(root: &Path, patterns: &[String]) -> Vec<String> {
    let mut out = BTreeSet::new();
    for pattern in patterns.iter().take(MAX_MEMBERS) {
        let pattern = pattern.trim_end_matches('/');
        if !pattern.contains('*') {
            out.insert(pattern.to_string());
            continue;
        }
        let (prefix, suffix) = match pattern.split_once('*') {
            Some(parts) => parts,
            None => continue,
        };
        let base = prefix.trim_end_matches('/');
        let dir = root.join(base);
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) && name.ends_with(suffix) {
                out.insert(format!("{base}/{name}").trim_start_matches('/').to_string());
            }
        }
    }
    out.into_iter().take(MAX_MEMBERS).collect()
}

pub fn discover(root: &Path) -> Result<Workspace, GraphError> {
    let root_manifest = root.join("Cargo.toml");
    let text = read_bounded(&root_manifest, MAX_MANIFEST_BYTES)?;
    let parsed = parse_manifest(&text);
    let mut member_dirs = if parsed.members.is_empty() {
        if parsed.package.is_some() {
            vec![String::new()]
        } else {
            Vec::new()
        }
    } else {
        expand_members(root, &parsed.members)
    };
    member_dirs.sort();
    member_dirs.dedup();
    let mut crates = BTreeMap::new();
    let mut skipped = Vec::new();
    for dir in member_dirs {
        let manifest_path = if dir.is_empty() {
            root.join("Cargo.toml")
        } else {
            root.join(&dir).join("Cargo.toml")
        };
        if !manifest_path.exists() {
            skipped.push(format!("{dir}/Cargo.toml"));
            continue;
        }
        let manifest = read_bounded(&manifest_path, MAX_MANIFEST_BYTES)?;
        let manifest = parse_manifest(&manifest);
        let Some(name) = manifest.package else {
            skipped.push(format!("{dir}/Cargo.toml"));
            continue;
        };
        crates.insert(
            name.clone(),
            CrateInfo {
                name,
                dir: dir.replace('\\', "/"),
                deps: manifest.deps,
            },
        );
    }
    // A dependency key that names a workspace member is a workspace edge;
    // everything else is an external package node.
    let names: BTreeSet<String> = crates.keys().cloned().collect();
    let mut packages = BTreeSet::new();
    for info in crates.values() {
        for dep in &info.deps {
            if !names.contains(dep) {
                packages.insert(dep.clone());
            }
        }
    }
    Ok(Workspace {
        root: root.to_path_buf(),
        crates,
        packages,
        skipped,
    })
}

impl Workspace {
    pub fn crate_for_path(&self, path: &str) -> Option<&CrateInfo> {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parser_reads_workspace_members_and_deps() {
        let manifest = parse_manifest(
            r#"
[workspace]
members = [
  "crates/a",
  "crates/*",
]

[workspace.dependencies]
serde = "1"

[package]
name = "root"

[dependencies]
faktor-core = { path = "../core" }
serde = "1"

[target.'cfg(unix)'.dev-dependencies]
libc = "0.2"
"#,
        );
        assert_eq!(manifest.package.as_deref(), Some("root"));
        assert!(manifest.members.contains(&"crates/a".to_string()));
        assert!(manifest.members.contains(&"crates/*".to_string()));
        assert!(manifest.deps.contains("faktor-core"));
        assert!(manifest.deps.contains("serde"));
        assert!(manifest.deps.contains("libc"));
    }

    #[test]
    fn member_expansion_ignores_path_traversal_patterns() {
        assert_eq!(
            expand_members(Path::new("/nonexistent"), &["../*".to_string()]).len(),
            0
        );
    }
}
