//! Production proof-basis probes (hardening): the tool-version and
//! environment-identity evidence the orchestrator's proof basis is built
//! from BEFORE a root verification record is created or reused.
//!
//! Bounded everything:
//! - at most [`MAX_TOOL_PROBES`] distinct tools are probed per basis build;
//! - every probe runs through the daemon's ONE [`ProcessSupervisor`] with
//!   [`PROBE_TIMEOUT`] and a sanitized `EnvSpec::toolchain()` environment;
//! - probe output is truncated to [`MAX_PROBE_VERSION_BYTES`];
//! - the PATH-resolved executable identity hashes at most
//!   [`MAX_IDENTITY_SAMPLE_BYTES`] (head + tail + size), never a whole
//!   binary.
//!
//! Honest degradation: a missing supervisor, a tool that does not exist on
//! PATH, a failed probe and a timed-out probe each produce an EXPLICIT
//! marker version — the field is never silently empty and never guessed.
//! Determinism: for identical inputs (same tools, same versions, same
//! PATH) two builds produce byte-identical lists, so the proof basis digest
//! is reproducible immediately before reuse.

use std::collections::BTreeSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use faktor_core::cancellation::CancellationToken;
use faktor_core::state::ToolVersion;
use faktor_terminal::{EnvSpec, ProcessOwner, ProcessSupervisor, SpawnConfig};

/// Hard bound on the number of tool probes of ONE basis build.
pub const MAX_TOOL_PROBES: usize = 12;
/// Deliberate bound on one probe (a wedged tool must never hang the
/// daemon; the supervisor kills it at the deadline).
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound on the captured version text written into the basis.
pub const MAX_PROBE_VERSION_BYTES: usize = 128;
/// Bound on one PATH-resolved executable identity's digest sample: the head
/// and the tail of the file, each at most this many bytes (plus the size).
pub const MAX_IDENTITY_SAMPLE_BYTES: u64 = 64 * 1024;

/// The tool set always probed (the documented build/toolchain families).
pub const BASE_PROBE_TOOLS: &[&str] = &["rustc", "cargo", "node", "npm", "pnpm", "yarn", "bun"];

/// The verification-relevant environment allowlist (fixed order): values
/// are copied verbatim when set, `<absent>` otherwise. Secrets and
/// unrelated names are never observed.
pub const PROBE_ENV_KEYS: &[&str] = &[
    "RUSTUP_TOOLCHAIN",
    "RUSTFLAGS",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_TARGET",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTC_WRAPPER",
    "PROFILE",
    "CARGO_FEATURES",
];

/// The bounded result of one proof-basis probe build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofProbeReport {
    /// One entry per probed tool (always non-empty: the custom verifier
    /// binary row is always present).
    pub tools: Vec<ToolVersion>,
    /// The environment projection: allowlisted fields + the target triple
    /// (from `rustc -vV`) + PATH-resolved executable identities.
    pub env_projection: Vec<(String, String)>,
}

/// Probe the proof-basis tool versions and environment projection. The tool
/// list is the deterministic union of [`BASE_PROBE_TOOLS`] and every
/// distinct check program (bounded, sorted). `None` supervisor degrades
/// every probe to an explicit marker (a daemon without a supervisor can
/// never claim it observed a version).
pub async fn probe_proof_basis(
    supervisor: Option<&Arc<ProcessSupervisor>>,
    check_programs: &[String],
) -> ProofProbeReport {
    let mut tools: BTreeSet<String> = BASE_PROBE_TOOLS.iter().map(|t| (*t).to_string()).collect();
    for program in check_programs {
        let name = program_basename(program);
        if !name.is_empty() {
            tools.insert(name);
        }
    }
    let tools: Vec<String> = tools.into_iter().take(MAX_TOOL_PROBES).collect();

    let mut tool_versions: Vec<ToolVersion> = Vec::with_capacity(tools.len() + 1);
    let mut env_projection: Vec<(String, String)> = PROBE_ENV_KEYS
        .iter()
        .map(|key| ((*key).to_string(), probe_env_value(key)))
        .collect();
    for tool in &tools {
        let version = probe_tool_version(supervisor, tool).await;
        tool_versions.push(ToolVersion {
            tool: tool.clone(),
            version: truncate_bytes(&version, MAX_PROBE_VERSION_BYTES),
        });
        env_projection.push((format!("path:{tool}"), resolve_executable_identity(tool)));
    }
    // The target triple is itself immutable evidence: derive it from the
    // real compiler probe (never guessed from the host process).
    let target_triple = rustc_target_triple(supervisor).await;
    env_projection.push(("target_triple".to_string(), target_triple));
    // The custom verifier binary is the daemon itself: version + a bounded
    // content digest of the running executable.
    tool_versions.push(custom_verifier_tool());
    env_projection.sort();
    env_projection.dedup();
    ProofProbeReport {
        tools: tool_versions,
        env_projection,
    }
}

/// The `faktor` build that would execute the check (this process), with a
/// bounded content digest of its own executable. An unreadable executable
/// is an explicit marker.
fn custom_verifier_tool() -> ToolVersion {
    let version = faktor_agent::runtime::VERIFICATION_IMPL_VERSION;
    let digest = std::env::current_exe()
        .ok()
        .and_then(|exe| bounded_file_digest(&exe))
        .unwrap_or_else(|| "<binary-unavailable>".to_string());
    ToolVersion {
        tool: "faktor-verifier".to_string(),
        version: format!("{version}#{digest}"),
    }
}

async fn probe_tool_version(supervisor: Option<&Arc<ProcessSupervisor>>, tool: &str) -> String {
    let Some(supervisor) = supervisor else {
        return "<probe-unavailable:no-supervisor>".to_string();
    };
    let cfg = SpawnConfig {
        cmd: tool.to_string(),
        args: vec!["--version".to_string()],
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        env: EnvSpec::toolchain(),
        owner: ProcessOwner::Daemon,
        capture: true,
        ..Default::default()
    };
    match supervisor
        .run(cfg, PROBE_TIMEOUT, CancellationToken::new())
        .await
    {
        Ok(out) => match out.exit_code {
            Some(0) => {
                let line = first_output_line(&out.excerpt);
                if line.is_empty() {
                    "<probe-empty:exit-0>".to_string()
                } else {
                    line
                }
            }
            code => {
                let line = first_output_line(&out.excerpt);
                let suffix = code
                    .map(|c| format!("exit-{c}"))
                    .unwrap_or_else(|| "signal".to_string());
                if line.is_empty() {
                    format!("<probe-failed:{suffix}>")
                } else {
                    format!("<probe-failed:{suffix}:{line}>")
                }
            }
        },
        Err(e) => {
            let message = e.to_string();
            if message.to_ascii_lowercase().contains("kill")
                || message.to_ascii_lowercase().contains("timeout")
            {
                "<probe-timeout>".to_string()
            } else {
                "<probe-unavailable:not-found>".to_string()
            }
        }
    }
}

/// The first non-empty, non-marker line of a bounded command excerpt.
fn first_output_line(excerpt: &str) -> String {
    excerpt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("[exit code:"))
        .unwrap_or_default()
        .to_string()
}

/// Parse the `host:` triple from `rustc -vV`; an absent/failed rustc probe
/// is explicit, never guessed.
async fn rustc_target_triple(supervisor: Option<&Arc<ProcessSupervisor>>) -> String {
    let Some(supervisor) = supervisor else {
        return "<probe-unavailable:no-supervisor>".to_string();
    };
    let cfg = SpawnConfig {
        cmd: "rustc".to_string(),
        args: vec!["-vV".to_string()],
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        env: EnvSpec::toolchain(),
        owner: ProcessOwner::Daemon,
        capture: true,
        ..Default::default()
    };
    match supervisor
        .run(cfg, PROBE_TIMEOUT, CancellationToken::new())
        .await
    {
        Ok(out) if out.exit_code == Some(0) => out
            .excerpt
            .lines()
            .map(str::trim)
            .find_map(|line| line.strip_prefix("host:").map(|t| t.trim().to_string()))
            .filter(|triple| !triple.is_empty())
            .unwrap_or_else(|| "<probe-empty:no-host-line>".to_string()),
        Ok(_) => "<probe-failed:rustc-vV>".to_string(),
        Err(_) => "<probe-unavailable:not-found>".to_string(),
    }
}

/// Project one allowlisted environment value: never an empty string (an
/// absent/unset name is explicit).
fn probe_env_value(key: &str) -> String {
    match std::env::var(key) {
        Ok(value) if !value.is_empty() => truncate_bytes(&value, MAX_PROBE_VERSION_BYTES),
        _ => "<absent>".to_string(),
    }
}

fn program_basename(program: &str) -> String {
    Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(program)
        .trim_end_matches(".exe")
        .to_string()
}

/// Resolve `tool` on the daemon PATH: the absolute path of the first
/// executable candidate plus a bounded content digest
/// (`<path>@blake3:<hex>`), or an explicit `<not-found>` marker.
pub fn resolve_executable_identity(tool: &str) -> String {
    match resolve_on_path(tool) {
        Some(path) => match bounded_file_digest(&path) {
            Some(digest) => format!("{}@{digest}", path.display()),
            None => format!("{}@<unreadable>", path.display()),
        },
        None => "<not-found>".to_string(),
    }
}

fn resolve_on_path(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(tool);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            // PATHEXT-aware resolution (`.exe`/`.cmd`/...); only the
            // documented executable extensions are considered.
            for ext in ["exe", "cmd", "bat", "com"] {
                let candidate = dir.join(format!("{tool}.{ext}"));
                if is_executable_file(&candidate) {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// `blake3:`-prefixed digest of a bounded sample of the file: its size plus
/// its first and last [`MAX_IDENTITY_SAMPLE_BYTES`] bytes. Stable for a
/// fixed binary and never materializes the whole file.
pub fn bounded_file_digest(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"faktor-exec-identity:v1\0");
    hasher.update(&len.to_le_bytes());
    let mut head = vec![0u8; MAX_IDENTITY_SAMPLE_BYTES.min(len) as usize];
    if !head.is_empty() {
        file.read_exact(&mut head).ok()?;
        hasher.update(&head);
    }
    if len > MAX_IDENTITY_SAMPLE_BYTES {
        file.seek(SeekFrom::End(-(MAX_IDENTITY_SAMPLE_BYTES as i64)))
            .ok()?;
        let mut tail = vec![0u8; MAX_IDENTITY_SAMPLE_BYTES as usize];
        file.read_exact(&mut tail).ok()?;
        hasher.update(&tail);
    }
    Some(format!("blake3:{}", hasher.finalize().to_hex()))
}

/// Truncate on a UTF-8 byte boundary; bounded probe text is never split
/// mid-codepoint.
fn truncate_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_are_explicit_and_never_empty() {
        assert!(truncate_bytes("abcdef", 3) == "abc");
        assert_eq!(truncate_bytes("a\u{4e00}b", 2), "a");
        assert_eq!(first_output_line(""), "");
        assert_eq!(
            first_output_line("rustc 1.79.0\n[exit code: 0]\n"),
            "rustc 1.79.0"
        );
        assert_eq!(program_basename("/usr/bin/node"), "node");
        assert_eq!(program_basename("node"), "node");
    }

    #[test]
    fn no_supervisor_degrades_explicitly() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let report = runtime.block_on(probe_proof_basis(None, &["cargo".to_string()]));
        assert!(
            report
                .tools
                .iter()
                .filter(|t| t.tool != "faktor-verifier")
                .all(|t| t.version.starts_with('<')),
            "every probe must be an explicit marker: {:?}",
            report.tools
        );
        assert!(report
            .tools
            .iter()
            .any(|t| t.tool == "faktor-verifier" && t.version.contains("faktor-agent")));
        assert!(report
            .env_projection
            .iter()
            .any(|(k, v)| k == "target_triple" && v.starts_with("<probe-unavailable")));
    }

    #[test]
    fn executable_identity_is_bounded_and_explicit() {
        let missing = resolve_executable_identity("definitely-not-a-real-tool-xyz");
        assert_eq!(missing, "<not-found>");
        // A real file on this machine (the test binary itself) resolves with
        // a blake3 digest.
        let exe = std::env::current_exe().unwrap();
        let digest = bounded_file_digest(&exe).expect("test binary is readable");
        assert!(digest.starts_with("blake3:"));
        assert_eq!(digest, bounded_file_digest(&exe).unwrap(), "deterministic");
    }
}
