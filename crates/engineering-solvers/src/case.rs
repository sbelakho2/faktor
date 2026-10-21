use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use engineering_core::{Digest, EngineeringValue, PropertyRevision, RevisionId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MAX_INPUT_FILES: usize = 64;
pub const MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_CASE_FILES: usize = 10_000;
pub const MAX_CASE_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_CASE_FILE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_SETTINGS: usize = 64;
pub const MAX_SETTINGS_BYTES: usize = 64 * 1024;
pub const MAX_SOLVE_DEADLINE_SECS: u64 = 600;

#[derive(Debug, Error)]
pub enum CaseError {
    #[error("io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("input file {name} exceeds {max} bytes")]
    InputTooLarge { name: String, max: usize },
    #[error("too many input files: {max}")]
    TooManyInputs { max: usize },
    #[error("too many case files: {max}")]
    TooManyCaseFiles { max: usize },
    #[error("case directory exceeds {max} bytes")]
    CaseTooLarge { max: u64 },
    #[error("symlinks are refused in a frozen case: {path}")]
    Symlink { path: String },
    #[error("case path escapes the root: {path}")]
    PathTraversal { path: String },
    #[error("duplicate input name {name:?}")]
    DuplicateInput { name: String },
    #[error("empty {field}")]
    EmptyField { field: &'static str },
    #[error("settings exceed {max} entries")]
    TooManySettings { max: usize },
    #[error("serialized settings exceed {max} bytes")]
    SettingsTooLarge { max: usize },
    #[error("input {name:?} was mutated after freezing")]
    InputMutated { name: String },
    #[error("case directory file {path:?} was mutated after freezing")]
    CaseFileMutated { path: String },
    #[error("missing case directory")]
    MissingCaseDirectory,
    #[error("not a directory: {path}")]
    NotADirectory { path: String },
    #[error("model reference is empty")]
    InvalidModel,
    #[error("serialization failed: {reason}")]
    Serialization { reason: String },
}

pub type CaseResult<T> = Result<T, CaseError>;

fn io(path: &Path, source: std::io::Error) -> CaseError {
    CaseError::Io {
        path: path.display().to_string(),
        source,
    }
}

/// Resolve an input path and refuse anything that leaves the case workdir or
/// is a symlink. A `FrozenCase` is deserializable, so this check runs at freeze
/// AND at verification; a forged/path-swapped case cannot turn a solver run
/// into a read primitive for host files.
fn confined_input(workdir: &Path, path: &Path) -> CaseResult<PathBuf> {
    let root = workdir.canonicalize().map_err(|error| io(workdir, error))?;
    let metadata = std::fs::symlink_metadata(path).map_err(|error| io(path, error))?;
    if metadata.file_type().is_symlink() {
        return Err(CaseError::Symlink {
            path: path.display().to_string(),
        });
    }
    let resolved = path.canonicalize().map_err(|error| io(path, error))?;
    if !resolved.starts_with(&root) {
        return Err(CaseError::PathTraversal {
            path: path.display().to_string(),
        });
    }
    Ok(resolved)
}

fn safe_relative(relative: &str) -> bool {
    !relative.is_empty()
        && Path::new(relative)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

fn read_bounded(path: &Path, max: usize) -> CaseResult<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|error| io(path, error))?;
    let metadata = file.metadata().map_err(|error| io(path, error))?;
    if !metadata.is_file() {
        return Err(CaseError::NotADirectory {
            path: path.display().to_string(),
        });
    }
    let mut bytes = Vec::new();
    file.take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| io(path, error))?;
    if bytes.len() > max {
        return Err(CaseError::InputTooLarge {
            name: path.display().to_string(),
            max,
        });
    }
    Ok(bytes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisRequest {
    Drc,
    BoardExport,
    CircuitSimulation,
    Mesh3d,
    SteadyCfd,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    pub id: String,
    pub version: String,
    pub digest: Digest,
}

impl ModelRef {
    pub fn new(id: impl Into<String>, version: impl Into<String>) -> CaseResult<Self> {
        let id = id.into();
        let version = version.into();
        if id.trim().is_empty() || version.trim().is_empty() {
            return Err(CaseError::InvalidModel);
        }
        let digest = Digest::of_text(&format!("{id}@{version}"));
        Ok(Self {
            id,
            version,
            digest,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrozenInput {
    pub name: String,
    pub path: PathBuf,
    pub digest: Digest,
    pub byte_len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrozenFile {
    pub digest: Digest,
    pub byte_len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrozenDirectory {
    pub root: PathBuf,
    pub files: BTreeMap<String, FrozenFile>,
}

#[derive(Debug, Clone)]
pub struct CaseSpec {
    pub case_id: String,
    pub revision: RevisionId,
    pub operation_id: u64,
    pub analysis: AnalysisRequest,
    pub workdir: PathBuf,
    pub inputs: Vec<(String, PathBuf)>,
    pub case_directory: Option<PathBuf>,
    pub boundary_conditions: BTreeMap<String, EngineeringValue>,
    pub property_revisions: Vec<PropertyRevision>,
    pub settings: BTreeMap<String, serde_json::Value>,
    pub model: ModelRef,
}

impl CaseSpec {
    pub fn new(
        case_id: impl Into<String>,
        revision: RevisionId,
        operation_id: u64,
        analysis: AnalysisRequest,
        workdir: impl Into<PathBuf>,
        model: ModelRef,
    ) -> Self {
        Self {
            case_id: case_id.into(),
            revision,
            operation_id,
            analysis,
            workdir: workdir.into(),
            inputs: Vec::new(),
            case_directory: None,
            boundary_conditions: BTreeMap::new(),
            property_revisions: Vec::new(),
            settings: BTreeMap::new(),
            model,
        }
    }

    pub fn with_input(mut self, name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        self.inputs.push((name.into(), path.into()));
        self
    }

    pub fn with_setting(mut self, name: impl Into<String>, value: serde_json::Value) -> Self {
        self.settings.insert(name.into(), value);
        self
    }

    pub fn with_case_directory(mut self, root: impl Into<PathBuf>) -> Self {
        self.case_directory = Some(root.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrozenCase {
    pub case_id: String,
    pub revision: RevisionId,
    pub operation_id: u64,
    pub analysis: AnalysisRequest,
    pub workdir: PathBuf,
    pub inputs: BTreeMap<String, FrozenInput>,
    pub case_directory: Option<FrozenDirectory>,
    pub boundary_conditions: BTreeMap<String, EngineeringValue>,
    pub property_revisions: Vec<PropertyRevision>,
    pub settings: BTreeMap<String, serde_json::Value>,
    pub model: ModelRef,
    pub input_digest: Digest,
    pub settings_digest: Digest,
}

#[derive(Serialize)]
struct DigestView<'a> {
    analysis: AnalysisRequest,
    revision: RevisionId,
    inputs: Vec<(&'a str, &'a Digest, u64)>,
    directory: Vec<(&'a str, &'a Digest, u64)>,
}

impl FrozenCase {
    pub fn freeze(spec: CaseSpec) -> CaseResult<Self> {
        if spec.case_id.trim().is_empty() {
            return Err(CaseError::EmptyField { field: "case_id" });
        }
        let workdir_meta =
            std::fs::metadata(&spec.workdir).map_err(|error| io(&spec.workdir, error))?;
        if !workdir_meta.is_dir() {
            return Err(CaseError::NotADirectory {
                path: spec.workdir.display().to_string(),
            });
        }
        if spec.inputs.len() > MAX_INPUT_FILES {
            return Err(CaseError::TooManyInputs {
                max: MAX_INPUT_FILES,
            });
        }
        let mut inputs = BTreeMap::new();
        for (name, path) in spec.inputs {
            if name.trim().is_empty() {
                return Err(CaseError::EmptyField {
                    field: "input name",
                });
            }
            if inputs.contains_key(&name) {
                return Err(CaseError::DuplicateInput { name });
            }
            let path = confined_input(&spec.workdir, &path)?;
            let bytes = read_bounded(&path, MAX_INPUT_BYTES)?;
            let digest = Digest::of_bytes(&bytes);
            inputs.insert(
                name.clone(),
                FrozenInput {
                    name,
                    path,
                    digest,
                    byte_len: bytes.len() as u64,
                },
            );
        }
        let case_directory = spec
            .case_directory
            .as_deref()
            .map(freeze_directory)
            .transpose()?;
        if spec.settings.len() > MAX_SETTINGS {
            return Err(CaseError::TooManySettings { max: MAX_SETTINGS });
        }
        let settings_digest =
            Digest::of_json(&spec.settings).map_err(|error| CaseError::Serialization {
                reason: error.to_string(),
            })?;
        let settings_bytes =
            serde_json::to_vec(&spec.settings).map_err(|error| CaseError::Serialization {
                reason: error.to_string(),
            })?;
        if settings_bytes.len() > MAX_SETTINGS_BYTES {
            return Err(CaseError::SettingsTooLarge {
                max: MAX_SETTINGS_BYTES,
            });
        }
        let view = DigestView {
            analysis: spec.analysis,
            revision: spec.revision,
            inputs: inputs
                .values()
                .map(|input| (input.name.as_str(), &input.digest, input.byte_len))
                .collect(),
            directory: case_directory
                .as_ref()
                .map(|directory| {
                    directory
                        .files
                        .iter()
                        .map(|(name, file)| (name.as_str(), &file.digest, file.byte_len))
                        .collect()
                })
                .unwrap_or_default(),
        };
        let input_digest = Digest::of_json(&view).map_err(|error| CaseError::Serialization {
            reason: error.to_string(),
        })?;
        Ok(Self {
            case_id: spec.case_id,
            revision: spec.revision,
            operation_id: spec.operation_id,
            analysis: spec.analysis,
            workdir: spec.workdir,
            inputs,
            case_directory,
            boundary_conditions: spec.boundary_conditions,
            property_revisions: spec.property_revisions,
            settings: spec.settings,
            model: spec.model,
            input_digest,
            settings_digest,
        })
    }

    pub fn input(&self, name: &str) -> Option<&FrozenInput> {
        self.inputs.get(name)
    }

    pub fn input_path(&self, name: &str) -> Option<&Path> {
        self.inputs.get(name).map(|input| input.path.as_path())
    }

    /// Verify every frozen input and case-directory file before a solver
    /// run. A mutation is a typed refusal, never a silent re-run on changed
    /// data.
    pub fn verify_frozen(&self) -> CaseResult<()> {
        for (name, input) in &self.inputs {
            let confined = confined_input(&self.workdir, &input.path)
                .map_err(|_| CaseError::InputMutated { name: name.clone() })?;
            if confined != input.path {
                return Err(CaseError::InputMutated { name: name.clone() });
            }
            let bytes = read_bounded(&input.path, MAX_INPUT_BYTES)
                .map_err(|_| CaseError::InputMutated { name: name.clone() })?;
            if bytes.len() as u64 != input.byte_len || Digest::of_bytes(&bytes) != input.digest {
                return Err(CaseError::InputMutated { name: name.clone() });
            }
        }
        if let Some(directory) = &self.case_directory {
            let root_meta = std::fs::symlink_metadata(&directory.root)
                .map_err(|error| io(&directory.root, error))?;
            if root_meta.file_type().is_symlink() {
                return Err(CaseError::Symlink {
                    path: directory.root.display().to_string(),
                });
            }
            if !root_meta.is_dir() {
                return Err(CaseError::NotADirectory {
                    path: directory.root.display().to_string(),
                });
            }
            for (relative, frozen) in &directory.files {
                if !safe_relative(relative) {
                    return Err(CaseError::PathTraversal {
                        path: relative.clone(),
                    });
                }
                let path = directory.root.join(relative);
                let bytes = read_bounded(&path, MAX_CASE_FILE_BYTES as usize).map_err(|_| {
                    CaseError::CaseFileMutated {
                        path: relative.clone(),
                    }
                })?;
                if bytes.len() as u64 != frozen.byte_len
                    || Digest::of_bytes(&bytes) != frozen.digest
                {
                    return Err(CaseError::CaseFileMutated {
                        path: relative.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    pub fn settings_string(&self, name: &str) -> Option<&str> {
        self.settings.get(name).and_then(|value| value.as_str())
    }

    pub fn settings_u64(&self, name: &str) -> Option<u64> {
        self.settings.get(name).and_then(|value| value.as_u64())
    }

    pub fn settings_f64(&self, name: &str) -> Option<f64> {
        self.settings.get(name).and_then(|value| value.as_f64())
    }
}

fn freeze_directory(root: &Path) -> CaseResult<FrozenDirectory> {
    let metadata = std::fs::symlink_metadata(root).map_err(|error| io(root, error))?;
    if metadata.file_type().is_symlink() {
        return Err(CaseError::Symlink {
            path: root.display().to_string(),
        });
    }
    if !metadata.is_dir() {
        return Err(CaseError::NotADirectory {
            path: root.display().to_string(),
        });
    }
    let mut files = BTreeMap::new();
    let mut total: u64 = 0;
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((directory, prefix)) = stack.pop() {
        let entries = std::fs::read_dir(&directory).map_err(|error| io(&directory, error))?;
        let mut entries: Vec<_> = entries
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| io(&directory, error))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == "." || name == ".." || name.contains('\0') {
                return Err(CaseError::PathTraversal { path: name });
            }
            let relative = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).map_err(|error| io(&path, error))?;
            if metadata.file_type().is_symlink() {
                return Err(CaseError::Symlink {
                    path: path.display().to_string(),
                });
            }
            if metadata.is_dir() {
                if files.len() + 1 > MAX_CASE_FILES {
                    return Err(CaseError::TooManyCaseFiles {
                        max: MAX_CASE_FILES,
                    });
                }
                stack.push((path, relative));
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            if metadata.len() > MAX_CASE_FILE_BYTES {
                return Err(CaseError::CaseTooLarge {
                    max: MAX_CASE_FILE_BYTES,
                });
            }
            total = total.saturating_add(metadata.len());
            if total > MAX_CASE_BYTES {
                return Err(CaseError::CaseTooLarge {
                    max: MAX_CASE_BYTES,
                });
            }
            let bytes = read_bounded(&path, MAX_CASE_FILE_BYTES as usize)?;
            files.insert(
                relative,
                FrozenFile {
                    digest: Digest::of_bytes(&bytes),
                    byte_len: bytes.len() as u64,
                },
            );
            if files.len() > MAX_CASE_FILES {
                return Err(CaseError::TooManyCaseFiles {
                    max: MAX_CASE_FILES,
                });
            }
        }
    }
    Ok(FrozenDirectory {
        root: root.to_path_buf(),
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freezing_binds_content_and_detects_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("board.kicad_pcb");
        std::fs::write(&input, b"(kicad_pcb)").unwrap();
        let model = ModelRef::new("test", "1.0.0").unwrap();
        let case = FrozenCase::freeze(
            CaseSpec::new(
                "case",
                RevisionId::new(1),
                7,
                AnalysisRequest::Drc,
                dir.path(),
                model,
            )
            .with_input("board", &input),
        )
        .unwrap();
        case.verify_frozen().unwrap();
        std::fs::write(&input, b"(kicad_pcb (version 1))").unwrap();
        assert!(matches!(
            case.verify_frozen(),
            Err(CaseError::InputMutated { .. })
        ));
    }

    #[test]
    fn inputs_outside_the_workdir_or_via_symlink_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir(&work).unwrap();
        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, b"host secret").unwrap();
        let model = ModelRef::new("test", "1.0.0").unwrap();
        let outside_case = FrozenCase::freeze(
            CaseSpec::new(
                "case",
                RevisionId::new(1),
                1,
                AnalysisRequest::Drc,
                &work,
                model.clone(),
            )
            .with_input("secret", &outside),
        );
        assert!(matches!(outside_case, Err(CaseError::PathTraversal { .. })));

        let link = work.join("link.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        #[cfg(unix)]
        {
            let linked = FrozenCase::freeze(
                CaseSpec::new(
                    "case",
                    RevisionId::new(1),
                    1,
                    AnalysisRequest::Drc,
                    &work,
                    model,
                )
                .with_input("secret", &link),
            );
            assert!(matches!(linked, Err(CaseError::Symlink { .. })));
        }
    }

    #[test]
    fn a_forged_case_directory_key_cannot_escape_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        let case_root = work.join("case");
        std::fs::create_dir_all(&case_root).unwrap();
        std::fs::write(case_root.join("controlDict"), b"x").unwrap();
        let model = ModelRef::new("test", "1.0.0").unwrap();
        let case = FrozenCase::freeze(
            CaseSpec::new(
                "case",
                RevisionId::new(1),
                1,
                AnalysisRequest::SteadyCfd,
                &work,
                model,
            )
            .with_case_directory(case_root),
        )
        .unwrap();
        let mut value = serde_json::to_value(&case).unwrap();
        let files = value["case_directory"]["files"]
            .as_object_mut()
            .expect("files");
        let entry = files.values().next().cloned().unwrap();
        files.clear();
        files.insert("../escape".to_string(), entry);
        let forged: FrozenCase = serde_json::from_value(value).unwrap();
        assert!(matches!(
            forged.verify_frozen(),
            Err(CaseError::PathTraversal { .. })
        ));
    }

    #[test]
    fn case_directories_refuse_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let case_root = dir.path().join("case");
        std::fs::create_dir(&case_root).unwrap();
        std::fs::write(case_root.join("controlDict"), b"x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(case_root.join("controlDict"), case_root.join("link")).unwrap();
        let model = ModelRef::new("test", "1.0.0").unwrap();
        let result = FrozenCase::freeze(CaseSpec {
            case_id: "case".to_string(),
            revision: RevisionId::new(1),
            operation_id: 1,
            analysis: AnalysisRequest::SteadyCfd,
            workdir: dir.path().to_path_buf(),
            inputs: Vec::new(),
            case_directory: Some(case_root),
            boundary_conditions: BTreeMap::new(),
            property_revisions: Vec::new(),
            settings: BTreeMap::new(),
            model,
        });
        assert!(matches!(result, Err(CaseError::Symlink { .. })));
    }
}
