//! The frozen CODING benchmark corpus: real multi-language code repositories
//! with objectively verified fixes (debugging, migrations, security fixes,
//! cross-file refactors).
//!
//! Corpus layout (under `coding-corpus/<task-id>/`):
//!
//! - `task.json` — strict metadata: id, frozen order, category, language,
//!   runtime, task class/phase and the goal text handed to a model;
//! - `repo/` — the pristine (BUGGY) repository. `repo/verify.sh` is the
//!   objective verification: it must FAIL on the pristine tree and PASS once
//!   the defect is fixed;
//! - `reference/` — the reference fix (used only by the corpus contract test
//!   and the labeled FIXTURE capture; never sent to a live model).
//!
//! Every verification runs a real subprocess (`sh verify.sh`) under a hard
//! wall bound with capped output capture. The corpus loader is adversarial:
//! symlinks, escapes, oversized shapes, missing verification, identical
//! reference trees and malformed metadata are refused before any run.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use faktor_core::model::{RiskBucket, RouterPhase, TaskClass};
use serde::{Deserialize, Serialize};

use crate::{copy_tree, io_err, reject_symlinks, require_regular, VerifyResult};

/// Corpus shape bounds (frozen, small, deterministic).
pub const CODING_TASK_MIN: usize = 8;
pub const CODING_TASK_MAX: usize = 12;
/// Hard wall-clock bound on ONE verification subprocess run.
pub const CODING_VERIFY_TIMEOUT: Duration = Duration::from_secs(30);
/// Attempt cap per task (a failed attempt still spent its call cost).
pub const MAX_CANDIDATE_ATTEMPTS: u32 = 4;
/// Per-file and total bounds of one fixture repository.
pub const MAX_REPO_FILES: usize = 64;
pub const MAX_REPO_FILE_BYTES: usize = 64 * 1024;
pub const MAX_REPO_TOTAL_BYTES: usize = 512 * 1024;
/// Longest accepted goal text.
pub const MAX_GOAL_BYTES: usize = 2048;
/// Bounds of one model candidate.
pub const MAX_CANDIDATE_FILES: usize = 16;
pub const MAX_CANDIDATE_FILE_BYTES: usize = 64 * 1024;
pub const MAX_CANDIDATE_TOTAL_BYTES: usize = 256 * 1024;
pub const MAX_CANDIDATE_TEXT_BYTES: usize = 256 * 1024;
pub const MAX_CANDIDATE_BLOCKS: usize = 8;
/// Longest accepted relative path of one candidate file.
pub const MAX_CANDIDATE_PATH_BYTES: usize = 256;
/// Capped verification output kept per stream.
pub const VERIFY_OUTPUT_CAP: usize = 16 * 1024;
/// Capped tail of verification output fed back to a repairing model.
pub const REPAIR_FEEDBACK_CAP: usize = 2000;

/// The engineering category of one corpus task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodingCategory {
    Debugging,
    Migration,
    SecurityFix,
    CrossFileRefactor,
}

impl CodingCategory {
    pub const ALL: [CodingCategory; 4] = [
        CodingCategory::Debugging,
        CodingCategory::Migration,
        CodingCategory::SecurityFix,
        CodingCategory::CrossFileRefactor,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            CodingCategory::Debugging => "debugging",
            CodingCategory::Migration => "migration",
            CodingCategory::SecurityFix => "security-fix",
            CodingCategory::CrossFileRefactor => "cross-file-refactor",
        }
    }
}

/// The source language of one corpus task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodingLanguage {
    Shell,
    Python,
    Awk,
}

impl CodingLanguage {
    pub fn as_str(self) -> &'static str {
        match self {
            CodingLanguage::Shell => "shell",
            CodingLanguage::Python => "python",
            CodingLanguage::Awk => "awk",
        }
    }
}

/// The interpreter the verification needs on PATH.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodingRuntime {
    Sh,
    Python3,
    Awk,
}

impl CodingRuntime {
    pub fn as_str(self) -> &'static str {
        match self {
            CodingRuntime::Sh => "sh",
            CodingRuntime::Python3 => "python3",
            CodingRuntime::Awk => "awk",
        }
    }

    fn matches_language(self, language: CodingLanguage) -> bool {
        matches!(
            (self, language),
            (CodingRuntime::Sh, CodingLanguage::Shell)
                | (CodingRuntime::Python3, CodingLanguage::Python)
                | (CodingRuntime::Awk, CodingLanguage::Awk)
        )
    }
}

/// TRUE when the runtime binary is present and executable on this host.
/// A missing runtime makes a task's runs `runtime_unavailable` (recorded,
/// never a silent pass).
pub fn runtime_available(runtime: CodingRuntime) -> bool {
    let mut cmd = match runtime {
        CodingRuntime::Sh => {
            let mut c = Command::new("sh");
            c.arg("-c").arg("exit 0");
            c
        }
        CodingRuntime::Python3 => {
            let mut c = Command::new("python3");
            c.arg("-c").arg("pass");
            c
        }
        CodingRuntime::Awk => {
            let mut c = Command::new("awk");
            c.arg("BEGIN { exit 0 }");
            c
        }
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// One frozen coding task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodingTaskSpec {
    pub id: String,
    pub order: u32,
    pub category: CodingCategory,
    pub language: CodingLanguage,
    pub runtime: CodingRuntime,
    pub task_class: TaskClass,
    pub phase: RouterPhase,
    pub risk_bucket: RiskBucket,
    pub goal: String,
    pub repo: PathBuf,
    pub reference: PathBuf,
    /// Pristine repository files (relative path → content), sorted.
    pub repo_files: BTreeMap<String, String>,
    /// Reference-fix files (relative path → content), sorted.
    pub reference_files: BTreeMap<String, String>,
}

impl CodingTaskSpec {
    pub fn verify_script(&self) -> PathBuf {
        self.repo.join("verify.sh")
    }

    /// Deterministic output-token budget handed to the router.
    pub fn output_budget_tokens(&self) -> u64 {
        ((self.goal.len() as u64) / 4).clamp(64, 2048)
    }
}

/// A loaded coding corpus plus its content digest (frozen-corpus evidence).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodingCorpus {
    pub tasks: Vec<CodingTaskSpec>,
    pub digest: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTask {
    id: String,
    order: u32,
    category: CodingCategory,
    language: CodingLanguage,
    runtime: CodingRuntime,
    task_class: TaskClass,
    phase: RouterPhase,
    goal: String,
}

/// Load and validate the coding corpus rooted at `root`. Fails loudly on any
/// hostile or inconsistent shape.
pub fn load_coding_corpus(root: &Path) -> Result<CodingCorpus, String> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(root)
        .map_err(|e| io_err("read coding corpus root", e))?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            entry.file_type().ok().filter(|t| t.is_dir())?;
            Some(entry.path())
        })
        .collect();
    dirs.sort();
    if dirs.len() < CODING_TASK_MIN || dirs.len() > CODING_TASK_MAX {
        return Err(format!(
            "coding corpus has {} tasks; expected between {CODING_TASK_MIN} and {CODING_TASK_MAX}",
            dirs.len()
        ));
    }
    let mut tasks = Vec::with_capacity(dirs.len());
    for dir in &dirs {
        tasks.push(load_coding_task(dir)?);
    }
    tasks.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.id.cmp(&b.id)));
    for pair in tasks.windows(2) {
        if pair[0].order == pair[1].order {
            return Err(format!(
                "coding corpus order {} is not unique ({} vs {})",
                pair[0].order, pair[0].id, pair[1].id
            ));
        }
    }
    let mut ids: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.len() != tasks.len() {
        return Err("coding corpus task ids are not unique".into());
    }
    let categories: BTreeSet<&str> = tasks.iter().map(|t| t.category.as_str()).collect();
    if categories.len() != CodingCategory::ALL.len() {
        return Err(format!(
            "coding corpus must cover all {} categories, found {categories:?}",
            CodingCategory::ALL.len()
        ));
    }
    let languages: BTreeSet<CodingLanguage> = tasks.iter().map(|t| t.language).collect();
    if languages.len() < 2 {
        return Err("coding corpus must be multi-language (>= 2 languages)".into());
    }
    let digest = crate::corpus_digest(root)?;
    Ok(CodingCorpus { tasks, digest })
}

/// Load ONE coding task directory (adversarial-loader evidence helper).
pub fn load_coding_task(dir: &Path) -> Result<CodingTaskSpec, String> {
    let dir_name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("coding task directory {} has no name", dir.display()))?;
    reject_symlinks(dir)?;
    let meta_path = dir.join("task.json");
    let meta = fs::read_to_string(&meta_path).map_err(|e| io_err("read task.json", e))?;
    if meta.len() > 64 * 1024 {
        return Err(format!("{dir_name}: task.json exceeds 64 KiB"));
    }
    let raw: RawTask =
        serde_json::from_str(&meta).map_err(|e| format!("{dir_name}: invalid task.json: {e}"))?;
    if raw.id != dir_name {
        return Err(format!(
            "{dir_name}: task id {:?} does not match its directory",
            raw.id
        ));
    }
    if raw.id.is_empty()
        || raw.id.len() > 64
        || !raw
            .id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(format!(
            "{dir_name}: task id must be [a-z0-9-] and <= 64 bytes"
        ));
    }
    if raw.order == 0 || raw.order > 1000 {
        return Err(format!("{dir_name}: order must be between 1 and 1000"));
    }
    if !raw.runtime.matches_language(raw.language) {
        return Err(format!(
            "{dir_name}: runtime {} does not match language {}",
            raw.runtime.as_str(),
            raw.language.as_str()
        ));
    }
    if raw.goal.trim().is_empty() || raw.goal.len() > MAX_GOAL_BYTES {
        return Err(format!(
            "{dir_name}: goal must be non-empty and <= {MAX_GOAL_BYTES} bytes"
        ));
    }
    let repo = dir.join("repo");
    let reference = dir.join("reference");
    require_regular(&repo.join("verify.sh"))?;
    let repo_files = read_tree(&repo)?;
    if !repo_files.contains_key("verify.sh") {
        return Err(format!("{dir_name}: repo/verify.sh missing"));
    }
    if !reference.is_dir() {
        return Err(format!("{dir_name}: reference/ directory missing"));
    }
    let reference_files = read_tree(&reference)?;
    if reference_files.is_empty() {
        return Err(format!("{dir_name}: reference fix is empty"));
    }
    if reference_files == repo_files {
        return Err(format!(
            "{dir_name}: reference fix is identical to the repo (no defect planted?)"
        ));
    }
    Ok(CodingTaskSpec {
        id: raw.id,
        order: raw.order,
        category: raw.category,
        language: raw.language,
        runtime: raw.runtime,
        task_class: raw.task_class,
        phase: raw.phase,
        risk_bucket: match raw.task_class {
            TaskClass::Easy => RiskBucket::Low,
            TaskClass::Medium => RiskBucket::Medium,
            TaskClass::Hard => RiskBucket::High,
        },
        goal: raw.goal,
        repo,
        reference,
        repo_files,
        reference_files,
    })
}

/// Read every regular file under `root` into a sorted relative-path map.
pub fn read_tree(root: &Path) -> Result<BTreeMap<String, String>, String> {
    reject_symlinks(root)?;
    let mut files: Vec<PathBuf> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).map_err(|e| io_err(&dir.display().to_string(), e))? {
            let entry = entry.map_err(|e| io_err("read tree entry", e))?;
            let ft = entry
                .file_type()
                .map_err(|e| io_err("stat tree entry", e))?;
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    if files.len() > MAX_REPO_FILES {
        return Err(format!(
            "{} has {} files (cap {MAX_REPO_FILES})",
            root.display(),
            files.len()
        ));
    }
    let mut map = BTreeMap::new();
    let mut total: usize = 0;
    for file in &files {
        let rel = file
            .strip_prefix(root)
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = fs::read(file).map_err(|e| io_err(&file.display().to_string(), e))?;
        if bytes.len() > MAX_REPO_FILE_BYTES {
            return Err(format!("{rel}: file exceeds {MAX_REPO_FILE_BYTES} bytes"));
        }
        total = total.saturating_add(bytes.len());
        if total > MAX_REPO_TOTAL_BYTES {
            return Err(format!(
                "{} exceeds the {MAX_REPO_TOTAL_BYTES}-byte tree bound",
                root.display()
            ));
        }
        let content = String::from_utf8(bytes)
            .map_err(|_| format!("{rel}: not valid UTF-8 (corpus files must be UTF-8)"))?;
        map.insert(rel, content);
    }
    Ok(map)
}

/// The corpus root shipped with this crate.
pub fn coding_corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("coding-corpus")
}

// --------------------------------------------------------------- candidates

/// One parsed model candidate: complete new contents for the files it
/// changes. Applying it never touches `verify.sh` (the objective verification
/// must not be tamperable by the candidate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub files: BTreeMap<String, String>,
}

impl Candidate {
    pub fn digest(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"faktor.coding.candidate.v1\0");
        for (path, content) in &self.files {
            hasher.update(path.as_bytes());
            hasher.update(&[0]);
            hasher.update(&(content.len() as u64).to_le_bytes());
            hasher.update(content.as_bytes());
        }
        hasher.finalize().to_hex().to_string()
    }

    /// The candidate files as the fenced JSON payload a model is asked to
    /// produce (deterministic; used by the labeled fixture generator).
    pub fn to_json_text(&self) -> String {
        let value = serde_json::json!({ "files": self.files });
        format!("```json\n{}\n```\n", value)
    }
}

/// Validate one candidate-relative path: relative, no traversal, no control
/// characters, bounded, and never the verification script itself.
pub fn validate_candidate_path(path: &str) -> Result<(), String> {
    if path.is_empty() || path.len() > MAX_CANDIDATE_PATH_BYTES {
        return Err("candidate path must be 1..=256 bytes".into());
    }
    if path.contains('\\') {
        return Err(format!("candidate path {path:?} contains a backslash"));
    }
    if path.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(format!(
            "candidate path {path:?} contains control characters"
        ));
    }
    let parsed = Path::new(path);
    for component in parsed.components() {
        match component {
            Component::Normal(part) if !part.is_empty() => {}
            _ => {
                return Err(format!(
                    "candidate path {path:?} must be relative without '.' or '..' components"
                ))
            }
        }
    }
    if path == "verify.sh" || path.ends_with("/verify.sh") {
        return Err("candidate may not modify the objective verification script".into());
    }
    Ok(())
}

/// Extract the LAST fenced (optionally ```json) code block and parse it as
/// `{"files": {<path>: <content>}}`; a bare JSON object is accepted as a
/// fallback. Everything is bounded; every path is validated.
pub fn parse_model_candidate(text: &str) -> Result<Candidate, String> {
    if text.len() > MAX_CANDIDATE_TEXT_BYTES {
        return Err(format!(
            "model output exceeds {MAX_CANDIDATE_TEXT_BYTES} bytes"
        ));
    }
    let blocks = fenced_blocks(text);
    let mut last_err: Option<String> = None;
    for block in blocks.iter().rev() {
        match parse_files_payload(block) {
            Ok(candidate) => return Ok(candidate),
            Err(e) => last_err = Some(e),
        }
    }
    match parse_files_payload(text) {
        Ok(candidate) => Ok(candidate),
        Err(e) => Err(last_err.unwrap_or(e)),
    }
}

fn fenced_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<Vec<&str>> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            match current.take() {
                Some(block) => {
                    blocks.push(block.join("\n"));
                    if blocks.len() > MAX_CANDIDATE_BLOCKS {
                        break;
                    }
                }
                None => current = Some(Vec::new()),
            }
            continue;
        }
        if let Some(block) = current.as_mut() {
            block.push(line);
        }
    }
    blocks
}

fn parse_files_payload(payload: &str) -> Result<Candidate, String> {
    let value: serde_json::Value = serde_json::from_str(payload.trim())
        .map_err(|e| format!("payload is not valid JSON: {e}"))?;
    let files = value
        .get("files")
        .and_then(|f| f.as_object())
        .ok_or_else(|| "payload must be a JSON object with a \"files\" object".to_string())?;
    if files.is_empty() {
        return Err("candidate carries no files".into());
    }
    if files.len() > MAX_CANDIDATE_FILES {
        return Err(format!(
            "candidate carries {} files (cap {MAX_CANDIDATE_FILES})",
            files.len()
        ));
    }
    let mut map = BTreeMap::new();
    let mut total: usize = 0;
    for (path, content) in files {
        validate_candidate_path(path)?;
        let content = content
            .as_str()
            .ok_or_else(|| format!("candidate file {path:?} content must be a JSON string"))?;
        if content.len() > MAX_CANDIDATE_FILE_BYTES {
            return Err(format!(
                "candidate file {path:?} exceeds {MAX_CANDIDATE_FILE_BYTES} bytes"
            ));
        }
        total = total.saturating_add(content.len());
        if total > MAX_CANDIDATE_TOTAL_BYTES {
            return Err(format!(
                "candidate exceeds the {MAX_CANDIDATE_TOTAL_BYTES}-byte total bound"
            ));
        }
        map.insert(path.clone(), content.to_string());
    }
    Ok(Candidate { files: map })
}

/// One candidate file applied to a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedFile {
    pub path: String,
    /// Previous content (`None` when the file did not exist).
    pub before: Option<String>,
    pub after: String,
}

/// Apply a candidate to `workspace` without ever following or replacing a
/// symlink and without touching `verify.sh`. Returns the applied files in
/// sorted path order with their previous contents.
pub fn apply_candidate(
    workspace: &Path,
    candidate: &Candidate,
) -> Result<Vec<AppliedFile>, String> {
    let mut applied = Vec::with_capacity(candidate.files.len());
    for (path, content) in &candidate.files {
        validate_candidate_path(path)?;
        let dst = workspace.join(path);
        let mut cur = workspace.to_path_buf();
        for component in Path::new(path).components() {
            cur.push(component);
            let is_leaf = cur == dst;
            match fs::symlink_metadata(&cur) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(format!(
                        "refusing to write through symlink {}",
                        cur.display()
                    ));
                }
                Ok(meta) if !is_leaf && !meta.is_dir() => {
                    return Err(format!("{} is not a directory", cur.display()));
                }
                Ok(meta) if is_leaf && !meta.is_file() => {
                    return Err(format!("{} is not a regular file", cur.display()));
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let before = match fs::symlink_metadata(&dst) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(format!("refusing to replace symlink {}", dst.display()));
                }
                Some(fs::read_to_string(&dst).map_err(|e| io_err(&dst.display().to_string(), e))?)
            }
            Err(_) => None,
        };
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).map_err(|e| io_err(&parent.display().to_string(), e))?;
        }
        fs::write(&dst, content).map_err(|e| io_err(&dst.display().to_string(), e))?;
        applied.push(AppliedFile {
            path: path.clone(),
            before,
            after: content.clone(),
        });
    }
    Ok(applied)
}

/// Materialize a pristine copy of a coding task's repository at `dst`.
pub fn copy_coding_repo(task: &CodingTaskSpec, dst: &Path) -> std::io::Result<()> {
    copy_tree(&task.repo, dst)
}

// ------------------------------------------------------- real verification

/// One real verification subprocess run with capped output capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verification {
    pub result: VerifyResult,
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub millis: u64,
}

impl Verification {
    pub fn passed(&self) -> bool {
        self.result == VerifyResult::Passed
    }

    /// The bounded failure excerpt fed to a repairing model, normalized for
    /// determinism: the run's workspace path and the host temp dir are
    /// replaced with fixed placeholders, so a replayed run reconstructs the
    /// exact same repair prompt as the capture (tracebacks and temp-file
    /// messages embed paths that differ per run).
    pub fn failure_excerpt_for(&self, workspace: &Path) -> Option<String> {
        if self.passed() {
            return None;
        }
        let mut text = String::new();
        if !self.stderr_tail.trim().is_empty() {
            text.push_str(&self.stderr_tail);
        }
        if !self.stdout_tail.trim().is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&self.stdout_tail);
        }
        let mut text = text.trim().to_string();
        if text.is_empty() {
            return None;
        }
        // Replace in specificity order: the CANONICAL spelling of a path
        // first, then its possibly-aliased non-canonical spelling. Hosts whose
        // temp dir carries a symlink alias (macOS `/var` -> `/private/var`)
        // make interpreters print the canonical form; replacing the shorter
        // non-canonical prefix inside it would strand the differing prefix
        // (`/private<workspace>`), making the same verification excerpt a
        // different byte string per host.
        let mut variants: Vec<(String, &'static str)> = Vec::new();
        let workspace_text = workspace.to_string_lossy().into_owned();
        if let Ok(canonical) = workspace.canonicalize() {
            let canonical = canonical.to_string_lossy().into_owned();
            if !canonical.is_empty() && canonical != workspace_text {
                variants.push((canonical, "<workspace>"));
            }
        }
        if !workspace_text.is_empty() {
            variants.push((workspace_text, "<workspace>"));
        }
        let tmp = std::env::temp_dir();
        let tmp = tmp.to_string_lossy().into_owned();
        if !tmp.is_empty() && tmp != "/" {
            if let Ok(canonical) = std::env::temp_dir().canonicalize() {
                let canonical = canonical.to_string_lossy().into_owned();
                if !canonical.is_empty() && canonical != tmp {
                    variants.push((canonical, "<tmp>"));
                }
            }
            variants.push((tmp, "<tmp>"));
        }
        for (variant, placeholder) in variants {
            if !variant.is_empty() {
                text = text.replace(&variant, placeholder);
            }
        }
        let start = text.len().saturating_sub(REPAIR_FEEDBACK_CAP);
        Some(text[start..].to_string())
    }
}

fn read_capped<R: Read + Send + 'static>(
    mut reader: R,
    cap: usize,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if buf.len() < cap {
                        let take = (cap - buf.len()).min(n);
                        buf.extend_from_slice(&chunk[..take]);
                    }
                }
            }
        }
        buf
    })
}

/// Run `<workspace>/verify.sh` with `sh`, bounded by `timeout`, capturing a
/// capped tail of stdout/stderr. The verdict is the REAL process exit status.
pub fn run_task_verification(workspace: &Path, timeout: Duration) -> Verification {
    let started = Instant::now();
    let mut child = match Command::new("sh")
        .arg("verify.sh")
        .current_dir(workspace)
        // Deterministic, side-effect-free verification: never let an
        // interpreter write mtime-dependent bytecode caches into the
        // workspace the final-tree digest covers.
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            return Verification {
                result: VerifyResult::SpawnFailed(e.to_string()),
                stdout_tail: String::new(),
                stderr_tail: String::new(),
                millis: started.elapsed().as_millis() as u64,
            }
        }
    };
    let stdout = child
        .stdout
        .take()
        .map(|s| read_capped(s, VERIFY_OUTPUT_CAP));
    let stderr = child
        .stderr
        .take()
        .map(|s| read_capped(s, VERIFY_OUTPUT_CAP));
    let deadline = started + timeout;
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    break VerifyResult::Passed;
                }
                break VerifyResult::Failed;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break VerifyResult::TimedOut;
                }
                sleep(Duration::from_millis(2));
            }
            Err(e) => break VerifyResult::SpawnFailed(e.to_string()),
        }
    };
    let stdout_tail = stdout
        .and_then(|handle| handle.join().ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
        .unwrap_or_default();
    let stderr_tail = stderr
        .and_then(|handle| handle.join().ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
        .unwrap_or_default();
    Verification {
        result,
        stdout_tail,
        stderr_tail,
        millis: started.elapsed().as_millis() as u64,
    }
}

// ------------------------------------------------------------------ fixture

/// The deterministic candidate recipe the unlabeled FIXTURE capture encodes
/// per task and attempt. The fixture capture is machinery proof only (see
/// `fixture`), never model-quality evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixtureRecipe {
    /// The unchanged pristine files ("no change" attempt).
    Pristine,
    /// The full reference fix.
    Reference,
    /// Cross-file partial fix (only the first app updated) for the refactor
    /// task: exercises a candidate that is neither pristine nor the fix.
    PartialFirstApp,
}

/// The frozen fixture plan: task order → per-attempt recipe. Deterministic
/// and documented; it exists to drive every measurement path of the replay
/// machinery (first-pass, rework, local→frontier fallback, unsolvable,
/// partial fix) without pretending to be a model.
pub fn fixture_plan(task: &CodingTaskSpec) -> Vec<FixtureRecipe> {
    match task.order {
        3 | 9 => vec![FixtureRecipe::Pristine, FixtureRecipe::Reference],
        6 => vec![FixtureRecipe::Pristine, FixtureRecipe::Pristine],
        8 => vec![FixtureRecipe::PartialFirstApp, FixtureRecipe::Reference],
        _ => vec![FixtureRecipe::Reference],
    }
}

/// The candidate files for one fixture recipe over `task`.
pub fn fixture_candidate(task: &CodingTaskSpec, recipe: FixtureRecipe) -> Candidate {
    let mut files = match recipe {
        FixtureRecipe::Pristine => task.repo_files.clone(),
        FixtureRecipe::Reference => merged_reference(task),
        FixtureRecipe::PartialFirstApp => {
            let mut files = merged_reference(task);
            files.insert(
                "app_b.py".into(),
                task.repo_files
                    .get("app_b.py")
                    .expect("partial fixture requires app_b.py in the repo")
                    .clone(),
            );
            files
        }
    };
    files.remove("verify.sh");
    Candidate { files }
}

fn merged_reference(task: &CodingTaskSpec) -> BTreeMap<String, String> {
    let mut files = task.repo_files.clone();
    for (path, content) in &task.reference_files {
        files.insert(path.clone(), content.clone());
    }
    files.remove("verify.sh");
    files
}
