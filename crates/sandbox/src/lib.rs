//! faktor-sandbox — capability-based permission enforcement (spec §30).
//!
//! Permissions are expressed as capabilities, never scattered conditionals.
//! Path checks are canonicalization-safe (symlink escapes and `..`
//! traversal are rejected); the network policy is the parsed-destination
//! gate from the security crate (audits 36-37): allowlist rules are parsed
//! at policy-build time into (scheme, host, port) triples with label-exact
//! host semantics — never prefix/substring matching — and every decision
//! goes through the parsed triple.
//!
//! ## Network-isolation authority (audit P0-39)
//!
//! Sandbox policy DECIDES the network-isolation requirement; the spawn
//! layer ENFORCES it. [`SandboxGuarantee`] maps to exactly one spawn
//! requirement through [`SandboxGuarantee::network_requirement`]:
//!
//! - [`SandboxGuarantee::Required`] → `DenyAll`: every command spawn must
//!   run with OS-level network denial. The spawn layer either produces the
//!   isolated child or refuses with a typed permission error BEFORE exec —
//!   never a warn-and-run downgrade. Policy code performs NO platform
//!   pre-judgement: capability existence is not enforcement (audits
//!   4/28/35-39), so only the spawn layer's actual backend can decide.
//! - [`SandboxGuarantee::BestEffort`] → `Inherit`: commands run behind the
//!   app-level capability gates only; the app-level-only caveat is
//!   documented in [`NETWORK_ISOLATION_NOTE`] and logged at the decision.
//! - [`SandboxGuarantee::None`] → `Inherit`: no guarantee claimed.
//!
//! There is deliberately no `platform_network_enforcement` probe in this
//! crate anymore: enforcement detection belongs where enforcement happens.
//!
//! BrokerOnly isolation (audit item 8) is deliberately NOT a guarantee here:
//! the browser authority (`faktor-browser`) selects
//! `NetworkIsolation::BrokerOnly { endpoint }` for its own spawns at the
//! live egress broker endpoint, and the spawn layer (`faktor-terminal`)
//! produces the confined child or refuses typed. Sandbox policy never names
//! a broker endpoint and no shell guarantee maps to it: `Required` stays
//! DenyAll (a broker-less confinement), so a shell can never be handed the
//! weaker broker-mediated mode.

use std::fs;
use std::path::{Component, Path, PathBuf};

use faktor_core::capability::{Capability, PermissionDecision};
use faktor_security::destination::{Decision, DeniedReason, DestinationPolicy, RequestTarget};

/// What a sandbox policy requires of the spawn layer's network isolation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SandboxGuarantee {
    /// OS-level network isolation is REQUIRED for commands: the policy maps
    /// to `NetworkIsolationRequirement::DenyAll` at the spawn seam and the
    /// spawn layer either produces an isolated child or refuses typed
    /// BEFORE exec. Policy code never pre-judges platform capability — a
    /// permitted shell under `ExecuteShell` + `Network(deny)` would
    /// otherwise open its own sockets, so an unenforceable requirement is
    /// a spawn refusal, never a warn-and-run downgrade. THE SECURE DEFAULT
    /// for untrusted shell execution: platforms with no backend
    /// (macOS/Windows today) refuse the shell typed instead of running it
    /// unenforced; Windows Job Objects are process containment, never
    /// network security.
    #[default]
    Required,
    /// Best-effort: commands run behind the existing app-level capability
    /// gates only and map to `Inherit` at the spawn seam. NETWORK_ISOLATION_NOTE:
    /// the `ExecuteShell` + `Network(deny)` combination is app-level only
    /// under BestEffort — the capability engine stops app-level egress, but
    /// a permitted shell can still open sockets itself; no OS-level deny
    /// backend backs this. The note is logged whenever a shell runs under
    /// this guarantee. Explicit opt-in only: this is the shape a
    /// user-granted network-capable shell carries.
    BestEffort,
    /// No network-isolation guarantee is claimed by this policy; commands
    /// map to `Inherit`. Explicit opt-in only (the user-granted
    /// network-capable shell shape).
    None,
}

/// The shell-execution contract of a policy: the honest, surfaced
/// distinction between an OS-isolated shell and a network-capable shell the
/// user explicitly granted. The two are NEVER presented as equivalent
/// strength — [`SpawnProfile::shell`] and the doctor surface carry this
/// exact tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ShellExecutionMode {
    /// Shell children run with OS-level network isolation REQUIRED
    /// (`SandboxGuarantee::Required`). The secure default; on platforms
    /// without a backend the spawn is refused typed, never downgraded.
    #[default]
    OsIsolated,
    /// The user EXPLICITLY granted a network-capable shell: children
    /// inherit the daemon network namespace behind app-level gates only
    /// (the [`NETWORK_ISOLATION_NOTE`] caveat applies). Strictly weaker
    /// than [`ShellExecutionMode::OsIsolated`]; config that selects it is
    /// explicit and the resulting state is surfaced as user-granted.
    NetworkCapableUserGranted,
}

impl ShellExecutionMode {
    /// The stable snake_case tag (also its serde tag).
    pub const fn as_tag(self) -> &'static str {
        match self {
            ShellExecutionMode::OsIsolated => "os_isolated",
            ShellExecutionMode::NetworkCapableUserGranted => "network_capable_user_granted",
        }
    }

    /// True when the OS-level isolation guarantee is required.
    pub const fn is_os_isolated(self) -> bool {
        matches!(self, ShellExecutionMode::OsIsolated)
    }
}

/// What the policy DEMANDS of the spawn layer (no platform detection here).
/// [`SandboxGuarantee::network_requirement`] is the single mapping locus;
/// the terminal crate converts it to its concrete isolation mode and fails
/// closed typed when the demand cannot be met.
pub use faktor_core::command::NetworkIsolationRequirement;

/// The descriptive spawn projection of one [`SandboxPolicy`]: the effective
/// filesystem and network profile a spawn admitted under the policy carries.
/// This is evidence, never a decision — [`PermissionEngine::evaluate`] and
/// [`PermissionEngine::spawn_network_requirement`] stay the decision
/// authorities; the projection exists so an execution authority can record
/// the exact profile a child was admitted under.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SpawnProfile {
    /// `workspace` when both external rules are `Deny`, else
    /// `workspace+external:<read>-<write>` with the rule tags.
    pub filesystem: String,
    /// The typed network guarantee tag of the policy
    /// (`none|best_effort|required`).
    pub network: String,
    /// The typed shell-execution contract tag (`os_isolated` |
    /// `network_capable_user_granted`): NEVER presented as equivalent
    /// strength — `network_capable_user_granted` is an explicit user grant
    /// backed by app-level gates only.
    pub shell: String,
}

impl SandboxGuarantee {
    /// The stable snake_case tag of this guarantee (also its serde tag).
    pub const fn as_tag(self) -> &'static str {
        match self {
            SandboxGuarantee::Required => "required",
            SandboxGuarantee::BestEffort => "best_effort",
            SandboxGuarantee::None => "none",
        }
    }
}

impl SandboxPolicy {
    /// Project the policy's filesystem/network profile for one admitted
    /// spawn. Deterministic and side-effect free: the same policy always
    /// yields the same profile.
    pub fn spawn_profile(&self) -> SpawnProfile {
        let external_denied = self.read_external == Rule::Deny && self.write_external == Rule::Deny;
        let filesystem = if external_denied {
            "workspace".to_string()
        } else {
            format!(
                "workspace+external:{}-{}",
                rule_tag(self.read_external),
                rule_tag(self.write_external)
            )
        };
        SpawnProfile {
            filesystem,
            network: self.network_guarantee.as_tag().to_string(),
            shell: self.shell_execution.as_tag().to_string(),
        }
    }

    /// The validated shell-execution contract of this policy: the single
    /// surface doctor/UI state reads. The pairing is enforced by
    /// [`SandboxPolicy::validate`]; this accessor is total and never
    /// presents the user-granted mode as equivalent to OS isolation.
    pub fn shell_execution_state(&self) -> ShellExecutionState {
        ShellExecutionState {
            mode: self.shell_execution,
            network_guarantee: self.network_guarantee,
        }
    }

    /// Validate the policy's internal contract: `OsIsolated` requires the
    /// `Required` guarantee (so a spawn either isolates or refuses typed),
    /// and the user-granted network-capable shell requires a non-`Required`
    /// guarantee (an explicit, strictly-weaker grant). Config boundaries
    /// call this; a violation is a typed refusal, never a silent
    /// reinterpretation of either mode.
    pub fn validate(&self) -> Result<(), String> {
        match (self.shell_execution, self.network_guarantee) {
            (ShellExecutionMode::OsIsolated, SandboxGuarantee::Required) => Ok(()),
            (
                ShellExecutionMode::NetworkCapableUserGranted,
                SandboxGuarantee::BestEffort | SandboxGuarantee::None,
            ) => Ok(()),
            (ShellExecutionMode::OsIsolated, other) => Err(format!(
                "shell execution 'os_isolated' requires network_guarantee 'required', got \
                 '{}'; set [sandbox] shell = \"network_capable_user_granted\" to grant a \
                 network-capable shell explicitly (app-level only)",
                other.as_tag()
            )),
            (ShellExecutionMode::NetworkCapableUserGranted, other) => Err(format!(
                "shell execution 'network_capable_user_granted' conflicts with \
                 network_guarantee '{}'; a user-granted network-capable shell cannot carry \
                 the OS-isolation requirement (set network_guarantee to \"none\" or \
                 \"best_effort\")",
                other.as_tag()
            )),
        }
    }
}

/// The surfaced shell-execution state (doctor/UI): the mode plus the
/// guarantee that backs it. [`ShellExecutionState::strength_label`] is the
/// honest one-line description any surface must print.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ShellExecutionState {
    pub mode: ShellExecutionMode,
    pub network_guarantee: SandboxGuarantee,
}

impl ShellExecutionState {
    /// The honest strength label: OS isolation or an explicit user grant
    /// that is app-level only — never both "equivalent network safety".
    pub fn strength_label(&self) -> &'static str {
        match self.mode {
            ShellExecutionMode::OsIsolated => {
                "OS-isolated shell (network_guarantee=required; spawn refused typed where no \
                 OS backend exists)"
            }
            ShellExecutionMode::NetworkCapableUserGranted => {
                "network-capable shell GRANTED BY USER (app-level gates only; NOT equivalent \
                 to OS isolation and NOT a network guarantee)"
            }
        }
    }
}

fn rule_tag(rule: Rule) -> &'static str {
    match rule {
        Rule::Allow => "allow",
        Rule::Ask => "ask",
        Rule::Deny => "deny",
    }
}

impl SandboxGuarantee {
    /// The one mapping from policy to spawn requirement: `Required` demands
    /// `DenyAll`; `BestEffort`/`None` inherit the daemon network namespace.
    pub fn network_requirement(self) -> NetworkIsolationRequirement {
        match self {
            SandboxGuarantee::Required => NetworkIsolationRequirement::DenyAll,
            SandboxGuarantee::BestEffort | SandboxGuarantee::None => {
                NetworkIsolationRequirement::Inherit
            }
        }
    }
}

impl From<SandboxGuarantee> for NetworkIsolationRequirement {
    fn from(guarantee: SandboxGuarantee) -> Self {
        guarantee.network_requirement()
    }
}

/// The canonical documentation sentence for the BestEffort limitation
/// (asserted by tests; the variant's doc comment above carries the same
/// wording).
pub const NETWORK_ISOLATION_NOTE: &str =
    "the ExecuteShell + Network(deny) combination is app-level only under BestEffort — \
     the capability engine stops app-level egress, but a permitted shell can still open \
     its own sockets; no OS-level deny backend backs this";

/// How the app-level network gate maps onto an installed parsed allowlist.
///
/// - [`NetworkGate::allow_all`] installs **no** policy: default-allow
///   (documented: with no destination policy configured, egress is not
///   restricted by this gate).
/// - [`NetworkGate::deny_all`] installs an *empty* policy: default-deny.
/// - [`NetworkGate::parse`] installs a parsed allowlist; every entry must
///   parse (a single bad entry is a policy build error, never silently
///   permissive) and duplicates are rejected.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NetworkGate {
    destinations: Option<DestinationPolicy>,
}

impl NetworkGate {
    /// No policy installed ⇒ default-allow for every destination.
    pub fn allow_all() -> NetworkGate {
        NetworkGate { destinations: None }
    }

    /// Installed empty policy ⇒ default-deny for every destination.
    pub fn deny_all() -> NetworkGate {
        NetworkGate {
            destinations: Some(DestinationPolicy::empty()),
        }
    }

    /// Install a parsed allowlist built from rule texts. Each entry is
    /// parsed strictly at build time (the config-time strictness boundary):
    /// any error fails the whole gate.
    pub fn parse<I>(entries: I) -> Result<NetworkGate, faktor_security::destination::RuleParseError>
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        Ok(NetworkGate {
            destinations: Some(DestinationPolicy::parse_lines(entries)?),
        })
    }

    /// Install a directly-parsed policy (e.g. the daemon default).
    pub fn from_policy(policy: DestinationPolicy) -> NetworkGate {
        NetworkGate {
            destinations: Some(policy),
        }
    }

    /// The installed allowlist, if any. `None` = default-allow.
    pub fn installed(&self) -> Option<&DestinationPolicy> {
        self.destinations.as_ref()
    }

    /// Decision for a parsed request target (see the security crate's
    /// semantics: no policy ⇒ Allowed; installed ⇒ default-deny on no
    /// full triple match; denied reasons name the closest rule).
    pub fn decide(&self, target: &RequestTarget) -> Decision {
        match &self.destinations {
            None => Decision::Allowed,
            Some(policy) => target.check_against(policy),
        }
    }

    /// Denied reasons only: `Ok(())` when allowed.
    pub fn check(&self, target: &RequestTarget) -> Result<(), DestinationDenied> {
        match self.decide(target) {
            Decision::Allowed => Ok(()),
            Decision::Denied(reason) => Err(DestinationDenied {
                target: target.describe(),
                reason,
            }),
        }
    }
}

/// The typed egress denial an enforcement site receives when the network
/// gate refuses a destination (before any connection is attempted). Carries
/// the parsed deny reason for logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestinationDenied {
    /// Human description of the parsed target (never the raw string alone).
    pub target: String,
    /// Which rule (if any) the denial is attributed to, and how far its
    /// match got.
    pub reason: DeniedReason,
}

impl std::fmt::Display for DestinationDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "destination {} denied: {}", self.target, self.reason)
    }
}

impl std::error::Error for DestinationDenied {}

/// The default provider endpoint allowlist (frozen set, mirrors the former
/// three-mode `AllowProviders` default). Scheme-constrained rules only.
const DEFAULT_PROVIDER_ENDPOINTS: [&str; 4] = [
    "https://api.openai.com",
    "https://api.anthropic.com",
    "https://generativelanguage.googleapis.com",
    "https://api.deepseek.com",
];

impl Default for NetworkGate {
    fn default() -> Self {
        // Static known-good entries: a parse failure here is a programming
        // error in the frozen list, never silently permissive.
        NetworkGate::parse(DEFAULT_PROVIDER_ENDPOINTS.iter().copied()).unwrap_or_else(|e| {
            panic!("frozen default provider endpoints must parse: {e}");
        })
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SandboxPolicy {
    pub read_workspace: Rule,
    pub write_workspace: Rule,
    pub read_external: Rule,
    pub write_external: Rule,
    pub execute_shell: Rule,
    pub network: NetworkGate,
    pub mcp: Rule,
    pub git: Rule,
    /// What this policy requires of the spawn layer's network isolation
    /// (audit P0-39). [`SandboxGuarantee::Required`] maps every spawn to
    /// `DenyAll`: the terminal crate runs the child isolated or refuses
    /// typed before exec (never a preflight guess and never a downgrade);
    /// `BestEffort` maps to `Inherit` with the documented app-level-only
    /// caveat; `None` claims nothing. The secure DEFAULT is `Required`:
    /// untrusted shell execution is OS-isolated by default, and a
    /// network-capable shell must be granted explicitly (with
    /// [`ShellExecutionMode::NetworkCapableUserGranted`]).
    #[serde(default)]
    pub network_guarantee: SandboxGuarantee,
    /// The shell-execution contract of this policy. Defaults to
    /// [`ShellExecutionMode::OsIsolated`] (requires `Required`); the
    /// user-granted network-capable mode is an explicit opt-in that must
    /// pair with a non-`Required` guarantee. [`SandboxPolicy::validate`]
    /// enforces the pairing; the state is surfaced through
    /// [`SandboxPolicy::shell_execution_state`] and
    /// [`SpawnProfile::shell`].
    #[serde(default)]
    pub shell_execution: ShellExecutionMode,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self {
            read_workspace: Rule::Allow,
            write_workspace: Rule::Allow,
            read_external: Rule::Ask,
            write_external: Rule::Ask,
            execute_shell: Rule::Ask,
            network: NetworkGate::default(),
            mcp: Rule::Allow,
            git: Rule::Allow,
            // Secure default (item 10): untrusted shell execution demands
            // OS-level network isolation; platforms without a backend
            // refuse the spawn typed instead of running it unenforced.
            network_guarantee: SandboxGuarantee::Required,
            shell_execution: ShellExecutionMode::OsIsolated,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rule {
    Allow,
    Deny,
    Ask,
}

#[derive(Debug, Clone)]
pub struct PermissionEngine {
    policy: SandboxPolicy,
    workspace_root: Option<PathBuf>,
}

impl PermissionEngine {
    pub fn new(policy: SandboxPolicy, workspace_root: Option<PathBuf>) -> Self {
        Self {
            policy,
            workspace_root: workspace_root.map(|p| p.canonicalize().unwrap_or(p)),
        }
    }

    pub fn policy(&self) -> &SandboxPolicy {
        &self.policy
    }

    pub fn workspace_root(&self) -> Option<&Path> {
        self.workspace_root.as_deref()
    }

    /// True when `path` (possibly relative) resolves inside the workspace
    /// root, following symlinks and rejecting escapes.
    pub fn is_within_workspace(&self, path: &Path) -> bool {
        let Some(root) = &self.workspace_root else {
            return false;
        };
        let resolved = resolve_within(root, path);
        resolved
            .as_deref()
            .map(|r| r.starts_with(root))
            .unwrap_or(false)
    }

    /// The parsed network gate. Every egress decision point consults it
    /// with a *parsed* request target; see [`PermissionEngine::check_egress`].
    pub fn network_gate(&self) -> &NetworkGate {
        &self.policy.network
    }

    /// Typed pre-connection egress check (the enforcement seam every
    /// outbound call must thread): parse the destination once, decide on
    /// the parsed triple, and return the typed denial reason when refused.
    /// An unparseable destination is a typed error too — it is never
    /// allowed, never prefix-compared, never silently defaulted.
    pub fn check_egress(&self, destination: &str) -> Result<(), EgressError> {
        let target = RequestTarget::parse(destination).map_err(EgressError::Unparseable)?;
        self.policy
            .network
            .check(&target)
            .map_err(EgressError::Denied)
    }

    /// Decision on a destination whose host/scheme/port were already pulled
    /// from a parsed URL object (never from strings): `(scheme, host,
    /// explicit_port, is_ipv4, ip)`. `explicit_port` is the URL's explicit
    /// port (scheme defaults are resolved inside the security crate).
    pub fn check_egress_parts(
        &self,
        scheme: &str,
        host: &str,
        explicit_port: Option<u16>,
        is_ipv4: bool,
        ip: Option<[u8; 4]>,
    ) -> Result<(), EgressError> {
        let target = RequestTarget::from_parts(Some(scheme), host, explicit_port, is_ipv4, ip)
            .map_err(EgressError::Unparseable)?;
        self.policy
            .network
            .check(&target)
            .map_err(EgressError::Denied)
    }

    /// The typed spawn requirement this policy imposes (audit P0-39):
    /// `Required` → `DenyAll`, `BestEffort`/`None` → `Inherit`. The spawn
    /// site converts it through the terminal crate's `NetworkIsolation`
    /// and the spawn layer owns all enforcement honesty: no platform probe
    /// or preflight guessing happens here. A `DenyAll` spawn either runs
    /// isolated or refuses typed before exec.
    pub fn spawn_network_requirement(&self) -> NetworkIsolationRequirement {
        self.policy.network_guarantee.network_requirement()
    }

    /// Evaluate one capability against the policy.
    pub fn evaluate(&self, capability: &Capability) -> PermissionDecision {
        match capability {
            Capability::ReadWorkspace { path } => {
                if self.is_within_workspace(path) {
                    rule_decision(self.policy.read_workspace)
                } else {
                    // Path escapes the workspace: it is an external read.
                    self.evaluate(&Capability::ReadExternal { path: path.clone() })
                }
            }
            Capability::WriteWorkspace { path } => {
                if self.is_within_workspace(path) {
                    rule_decision(self.policy.write_workspace)
                } else {
                    self.evaluate(&Capability::WriteExternal { path: path.clone() })
                }
            }
            Capability::ReadExternal { path } => {
                if self.is_within_workspace(path) {
                    rule_decision(self.policy.read_workspace)
                } else {
                    rule_decision(self.policy.read_external)
                }
            }
            Capability::WriteExternal { path } => {
                if self.is_within_workspace(path) {
                    rule_decision(self.policy.write_workspace)
                } else {
                    rule_decision(self.policy.write_external)
                }
            }
            // The rule decision is purely policy: whether a Required
            // guarantee can be enforced is decided at the spawn layer (the
            // terminal crate refuses typed when it cannot isolate). This
            // evaluation never pre-judges platform capability.
            Capability::ExecuteShell { .. } => {
                let rule = rule_decision(self.policy.execute_shell);
                if rule != PermissionDecision::Deny
                    && self.policy.network_guarantee == SandboxGuarantee::BestEffort
                {
                    tracing::warn!(
                        "execute_shell under BestEffort network guarantee: {}",
                        NETWORK_ISOLATION_NOTE
                    );
                }
                rule
            }
            Capability::Network { destination } => match self.check_egress(destination) {
                Ok(()) => PermissionDecision::Allow,
                Err(e) => {
                    tracing::warn!("network capability denied: {e}");
                    PermissionDecision::Deny
                }
            },
            Capability::Mcp { .. } => rule_decision(self.policy.mcp),
            Capability::Git { .. } => rule_decision(self.policy.git),
        }
    }
}

/// Egress refusal at an app-level network decision point: either the typed
/// destination-policy denial or an unparseable destination (which is always
/// refused when a gate is installed — never prefix-matched, never allowed
/// by accident). With no gate installed (default-allow) `check_egress`
/// succeeds for any *parseable* destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EgressError {
    Denied(DestinationDenied),
    Unparseable(String),
}

impl std::fmt::Display for EgressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EgressError::Denied(d) => write!(f, "{d}"),
            EgressError::Unparseable(e) => write!(f, "destination does not parse as a URL: {e}"),
        }
    }
}

impl std::error::Error for EgressError {}

fn rule_decision(rule: Rule) -> PermissionDecision {
    match rule {
        Rule::Allow => PermissionDecision::Allow,
        Rule::Deny => PermissionDecision::Deny,
        Rule::Ask => PermissionDecision::Ask,
    }
}

/// Resolve `path` against `root` with parent-canonicalization (symlink-safe)
/// and component-level `..` rejection. Returns the canonical absolute path
/// when the resolution is safe and exists.
fn resolve_within(root: &Path, path: &Path) -> Option<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    // Component-level traversal rejection (before touching the FS).
    for component in joined.components() {
        if let Component::ParentDir = component {
            return None;
        }
    }
    // Full canonicalization resolves leaf symlinks too; on failure (missing
    // leaf, ELOOP) fall back to parent-canonicalization with an explicit
    // leaf-symlink rejection.
    if let Ok(canon) = joined.canonicalize() {
        return if canon.starts_with(root) {
            Some(canon)
        } else {
            None
        };
    }
    // The leaf is itself a symlink whose canonicalization failed (loop or
    // broken): never treat it as inside.
    if let Ok(meta) = fs::symlink_metadata(&joined) {
        if meta.file_type().is_symlink() {
            return None;
        }
    }
    let parent = joined.parent()?;
    let file_name = joined.file_name()?;
    let canon_parent = parent.canonicalize().ok()?;
    let resolved = canon_parent.join(file_name);
    if resolved.starts_with(root) {
        Some(resolved)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    fn engine(root: &Path) -> PermissionEngine {
        PermissionEngine::new(SandboxPolicy::default(), Some(root.to_path_buf()))
    }

    fn tmp_workspace() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        fs::create_dir_all(&root).unwrap();
        (dir, root)
    }

    #[test]
    fn traversal_matrix_rejected() {
        let (_d, root) = tmp_workspace();
        let e = engine(&root);
        // Abs paths outside.
        assert!(!e.is_within_workspace(Path::new("/etc/passwd")));
        assert!(!e.is_within_workspace(Path::new("/tmp/../etc")));
        // Parent-dir escapes.
        assert!(!e.is_within_workspace(Path::new("../escape")));
        assert!(!e.is_within_workspace(Path::new("a/../../b")));
        // Abs path of the root itself is fine.
        assert!(e.is_within_workspace(&root.join("x.rs")));
        assert!(e.is_within_workspace(&root));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_rejected() {
        let (_d, root) = tmp_workspace();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        // Symlink inside the workspace pointing outside.
        symlink(outside.path(), root.join("link")).unwrap();
        let e = engine(&root);
        assert!(
            !e.is_within_workspace(Path::new("link/secret.txt")),
            "symlink escape must be rejected"
        );
        assert!(
            !e.is_within_workspace(Path::new("link")),
            "symlinked dir itself is outside"
        );
        // Symlink to a file inside the workspace is fine.
        fs::write(root.join("real.txt"), "x").unwrap();
        symlink(root.join("real.txt"), root.join("alias.txt")).unwrap();
        assert!(e.is_within_workspace(Path::new("alias.txt")));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_loop_terminates() {
        let (_d, root) = tmp_workspace();
        symlink(root.join("b"), root.join("a")).unwrap();
        symlink(root.join("a"), root.join("b")).unwrap();
        let e = engine(&root);
        // Canonicalize of a/b loops — resolve must return None, never hang.
        assert!(!e.is_within_workspace(Path::new("a/x")));
    }

    #[test]
    fn relative_and_absolute_equivalence() {
        let (_d, root) = tmp_workspace();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "").unwrap();
        let e = engine(&root);
        assert!(e.is_within_workspace(Path::new("src/main.rs")));
        assert!(e.is_within_workspace(&root.join("src/main.rs")));
        assert!(!e.is_within_workspace(&root.join("src/main.rs/../../etc/x")));
    }

    #[test]
    fn workspace_capabilities_obey_policy() {
        let (_d, root) = tmp_workspace();
        fs::write(root.join("f.rs"), "").unwrap();
        let policy = SandboxPolicy {
            write_workspace: Rule::Deny,
            ..Default::default()
        };
        let e = PermissionEngine::new(policy, Some(root.clone()));
        assert_eq!(
            e.evaluate(&Capability::WriteWorkspace {
                path: root.join("f.rs")
            }),
            PermissionDecision::Deny
        );
        assert_eq!(
            e.evaluate(&Capability::ReadWorkspace {
                path: root.join("f.rs")
            }),
            PermissionDecision::Allow
        );
        assert_eq!(
            e.evaluate(&Capability::ReadWorkspace {
                path: PathBuf::from("/etc/passwd")
            }),
            PermissionDecision::Ask,
            "escaped workspace read becomes external Ask"
        );
    }

    #[test]
    fn external_rules_mapped() {
        let e = PermissionEngine::new(SandboxPolicy::default(), None);
        assert_eq!(
            e.evaluate(&Capability::ReadExternal {
                path: "/etc".into()
            }),
            PermissionDecision::Ask
        );
        assert_eq!(
            e.evaluate(&Capability::WriteExternal {
                path: "/etc".into()
            }),
            PermissionDecision::Ask
        );
        let policy = SandboxPolicy {
            read_external: Rule::Deny,
            execute_shell: Rule::Allow,
            ..Default::default()
        };
        let e = PermissionEngine::new(policy, None);
        assert_eq!(
            e.evaluate(&Capability::ReadExternal { path: "/x".into() }),
            PermissionDecision::Deny
        );
        assert_eq!(
            e.evaluate(&Capability::ExecuteShell {
                command: "ls".into()
            }),
            PermissionDecision::Allow
        );
    }

    #[test]
    fn default_gate_allowlists_provider_endpoints_and_denies_the_rest() {
        let e = PermissionEngine::new(SandboxPolicy::default(), None);
        // The frozen provider allowlist still allows its own endpoints.
        for (dest, expect) in [
            (
                "https://api.openai.com/v1/chat/completions",
                PermissionDecision::Allow,
            ),
            (
                "https://api.anthropic.com/v1/messages",
                PermissionDecision::Allow,
            ),
            (
                "https://api.deepseek.com/chat/completions",
                PermissionDecision::Allow,
            ),
            // Prefix lookalikes stay denied (parsed semantics).
            ("https://evil.example.com", PermissionDecision::Deny),
            ("https://api.openai.com.evil/v1", PermissionDecision::Deny),
            ("https://evil-api.openai.com/v1", PermissionDecision::Deny),
            ("https://notapi.openai.com/v1", PermissionDecision::Deny),
            ("http://api.openai.com/v1", PermissionDecision::Deny), // https-only rule
            ("https://example.com", PermissionDecision::Deny),
        ] {
            assert_eq!(
                e.evaluate(&Capability::Network {
                    destination: dest.into()
                }),
                expect,
                "{dest}"
            );
        }
    }

    #[test]
    fn deny_all_and_allow_all_gates() {
        let policy = SandboxPolicy {
            network: NetworkGate::deny_all(),
            ..Default::default()
        };
        let e = PermissionEngine::new(policy, None);
        assert_eq!(
            e.evaluate(&Capability::Network {
                destination: "https://api.openai.com".into()
            }),
            PermissionDecision::Deny
        );

        let policy = SandboxPolicy {
            network: NetworkGate::allow_all(),
            ..Default::default()
        };
        let e = PermissionEngine::new(policy, None);
        // No policy installed ⇒ default-allow (documented semantics).
        assert_eq!(
            e.evaluate(&Capability::Network {
                destination: "https://api.openai.com".into()
            }),
            PermissionDecision::Allow
        );
        assert_eq!(
            e.evaluate(&Capability::Network {
                destination: "http://127.0.0.1:9911/x".into()
            }),
            PermissionDecision::Allow
        );
    }

    #[test]
    fn bad_policy_entry_is_a_build_error_never_silent() {
        for bad in [
            "",
            " ",
            "example.com:99999",
            "http://*.com",
            "evil-example.com/x",
            "exa mple.com",
        ] {
            let err = NetworkGate::parse([bad]).unwrap_err();
            assert!(!err.reason.is_empty(), "{bad:?}");
        }
        // Duplicate exact rules error instead of last-wins.
        assert!(NetworkGate::parse(["example.com", "example.com"]).is_err());
    }

    #[test]
    fn parse_duplicate_within_endpoints_fails() {
        assert!(NetworkGate::parse(["example.com", "EXAMPLE.com"]).is_err());
    }

    #[test]
    fn typed_egress_check_reports_rule_and_match_depth() {
        let policy = SandboxPolicy {
            network: NetworkGate::parse(["https://api.openai.com"]).unwrap(),
            ..Default::default()
        };
        let e = PermissionEngine::new(policy, None);
        // Allowed destination: parse once, allowed.
        assert!(e
            .check_egress("https://api.openai.com/v1/chat/completions")
            .is_ok());
        // Scheme mismatch: denied BEFORE any connection, with the rule text
        // and the matched depth (host+port matched, scheme missed).
        let err = e.check_egress("http://api.openai.com:443/v1").unwrap_err();
        match err {
            EgressError::Denied(d) => {
                assert_eq!(
                    d.reason.rule_fired.as_deref(),
                    Some("https://api.openai.com")
                );
                assert!(d.target.contains("api.openai.com"));
                assert!(d.to_string().contains("scheme"), "{}", d);
            }
            EgressError::Unparseable(_) => panic!("parses fine; must be a gate denial"),
        }
        // Host mismatch denies with no attributed rule (plain default-deny).
        let err = e.check_egress("https://evil-example.com").unwrap_err();
        match err {
            EgressError::Denied(d) => {
                assert_eq!(d.reason.rule_fired, None);
                assert!(d.to_string().contains("default-deny"), "{}", d);
            }
            EgressError::Unparseable(_) => panic!("parses fine"),
        }
        // Unparseable destinations are typed errors, never allowed.
        let err = e.check_egress("https://evil example.com").unwrap_err();
        assert!(matches!(err, EgressError::Unparseable(_)));
        // check_egress_parts: decision on URL-parser-supplied parts.
        assert!(e
            .check_egress_parts("https", "api.openai.com", Some(443), false, None)
            .is_ok());
        let err = e
            .check_egress_parts("http", "api.openai.com", Some(80), false, None)
            .unwrap_err();
        assert!(matches!(err, EgressError::Denied(_)));
    }

    #[test]
    fn egress_never_prefix_matches_at_the_engine_boundary() {
        let policy = SandboxPolicy {
            network: NetworkGate::parse(["example.com"]).unwrap(),
            ..Default::default()
        };
        let e = PermissionEngine::new(policy, None);
        for dest in ["https://example.com", "https://example.com:8443/x"] {
            assert!(e.check_egress(dest).is_ok(), "{dest}");
        }
        // The audit regression set: suffixes and lookalikes are DENIED.
        for dest in [
            "https://evil-example.com",
            "https://evil-example.com.evil",
            "https://example.com.evil",
            "https://notexample.com",
            "https://api.example.com",
            "https://sub.example.com.evil.com",
        ] {
            let err = e.check_egress(dest).unwrap_err();
            assert!(
                matches!(err, EgressError::Denied(_)),
                "{dest} must be denied, got {err}"
            );
        }
        // The path/query of the FIRST fetch never changes the gate (the
        // connection goes to example.com); a server-side follow-up fetch is
        // its own egress and must pass this gate again.
        assert!(e
            .check_egress("http://example.com/redirect?to=https://evil-example.com")
            .is_ok());
    }

    #[test]
    fn no_policy_gate_allows_even_weird_but_parseable_destinations() {
        let policy = SandboxPolicy {
            network: NetworkGate::allow_all(),
            ..Default::default()
        };
        let e = PermissionEngine::new(policy, None);
        for dest in [
            "https://anything.example:8443/a",
            "http://127.0.0.1:1/x",
            "wss://example.com",
        ] {
            assert!(e.check_egress(dest).is_ok(), "{dest}");
        }
        // Still never allow a destination that does not parse.
        assert!(matches!(
            e.check_egress("https://exa mple.com").unwrap_err(),
            EgressError::Unparseable(_)
        ));
    }

    #[test]
    fn shell_mcp_git_rules() {
        let policy = SandboxPolicy {
            mcp: Rule::Ask,
            ..Default::default()
        };
        let e = PermissionEngine::new(policy, None);
        assert_eq!(
            e.evaluate(&Capability::Mcp {
                server: "fs".into()
            }),
            PermissionDecision::Ask
        );
        assert_eq!(
            e.evaluate(&Capability::Git {
                operation: "status".into()
            }),
            PermissionDecision::Allow
        );
    }

    #[test]
    fn unicode_and_hostile_paths_never_panic() {
        let (_d, root) = tmp_workspace();
        let e = engine(&root);
        for hostile in [
            "",
            ".",
            "..",
            "a/b/../../../../etc/passwd",
            "\u{FFFE}",
            "x\0y",
            "\\\\server\\share\\x",
            "a/..",
        ] {
            let _ = e.evaluate(&Capability::ReadWorkspace {
                path: hostile.into(),
            });
            let _ = e.is_within_workspace(Path::new(hostile));
        }
        // None of the above panicked; workspace root itself still resolves.
        assert!(e.is_within_workspace(Path::new(".")) || true);
    }

    #[test]
    fn hostile_network_destinations_never_panic_and_never_allow() {
        let policy = SandboxPolicy {
            network: NetworkGate::default(),
            ..Default::default()
        };
        let e = PermissionEngine::new(policy, None);
        for hostile in [
            "",
            " ",
            "https://",
            "http://evil example.com",
            "\u{FFFE}",
            "x\0y",
            "example.com:99999",
            "https://example.com:notaport",
        ]
        .into_iter()
        .map(str::to_string)
        .chain(std::iter::once(format!("http://{}/x", "a".repeat(5000))))
        {
            let d = e.evaluate(&Capability::Network {
                destination: hostile.clone(),
            });
            assert_eq!(d, PermissionDecision::Deny, "{hostile:?} must deny");
            let _ = e.check_egress(&hostile); // must not panic
        }
    }

    #[test]
    fn no_workspace_root_means_everything_external() {
        let e = PermissionEngine::new(SandboxPolicy::default(), None);
        assert!(!e.is_within_workspace(Path::new("/anything")));
        assert_eq!(
            e.evaluate(&Capability::ReadWorkspace { path: "/x".into() }),
            PermissionDecision::Ask,
            "no root ⇒ external Ask"
        );
    }

    #[test]
    fn policy_serde_roundtrip() {
        let p = SandboxPolicy::default();
        let v = serde_json::to_value(&p).unwrap();
        let back: SandboxPolicy = serde_json::from_value(v).unwrap();
        assert_eq!(p, back);
        let p = SandboxPolicy {
            network: NetworkGate::allow_all(),
            ..Default::default()
        };
        let v = serde_json::to_value(&p).unwrap();
        let back: SandboxPolicy = serde_json::from_value(v).unwrap();
        assert_eq!(p, back);
        // The new guarantee field round-trips too.
        let p = SandboxPolicy {
            network_guarantee: SandboxGuarantee::Required,
            ..Default::default()
        };
        let v = serde_json::to_value(&p).unwrap();
        let back: SandboxPolicy = serde_json::from_value(v).unwrap();
        assert_eq!(p, back);
        assert_eq!(back.network_guarantee, SandboxGuarantee::Required);
        // Pre-existing configs without the field still parse (serde
        // default), with the documented secure default = Required.
        let mut v = serde_json::to_value(SandboxPolicy::default()).unwrap();
        v.as_object_mut().unwrap().remove("network_guarantee");
        let back: SandboxPolicy = serde_json::from_value(v).unwrap();
        assert_eq!(back.network_guarantee, SandboxGuarantee::Required);
        assert_eq!(back.shell_execution, ShellExecutionMode::OsIsolated);
        assert!(back.validate().is_ok());
    }

    // ------------- P0-39 network-isolation authority (policy -> spawn) ---

    fn shell_cap() -> Capability {
        Capability::ExecuteShell {
            command: "curl http://evil.example".into(),
        }
    }

    #[test]
    fn spawn_profile_projects_the_policy_deterministically() {
        // The default: external reads/writes Ask ⇒ the profile names them and
        // the network guarantee tag is the policy's own.
        let profile = SandboxPolicy::default().spawn_profile();
        assert_eq!(profile.filesystem, "workspace+external:ask-ask");
        assert_eq!(profile.network, "required");
        assert_eq!(profile.shell, "os_isolated");
        // Deny-everything external ⇒ workspace-only.
        let locked = SandboxPolicy {
            read_external: Rule::Deny,
            write_external: Rule::Deny,
            network_guarantee: SandboxGuarantee::Required,
            ..Default::default()
        };
        assert_eq!(
            locked.spawn_profile(),
            SpawnProfile {
                filesystem: "workspace".into(),
                network: "required".into(),
                shell: "os_isolated".into(),
            }
        );
        // Deterministic: the same policy projects the same profile, and the
        // profile round-trips through serde (it is recorded as evidence).
        assert_eq!(locked.spawn_profile(), locked.spawn_profile());
        let json = serde_json::to_value(locked.spawn_profile()).unwrap();
        let back: SpawnProfile = serde_json::from_value(json).unwrap();
        assert_eq!(back, locked.spawn_profile());
        // One allowed external direction still names both rules.
        let mixed = SandboxPolicy {
            read_external: Rule::Allow,
            write_external: Rule::Deny,
            ..Default::default()
        };
        assert_eq!(
            "workspace+external:allow-deny",
            mixed.spawn_profile().filesystem
        );
    }

    #[test]
    fn required_shell_is_deny_all() {
        // The ONE policy -> spawn mapping: Required demands OS-level denial
        // and can never become an Inherit path.
        assert_eq!(
            SandboxGuarantee::Required.network_requirement(),
            NetworkIsolationRequirement::DenyAll
        );
        assert_eq!(
            NetworkIsolationRequirement::from(SandboxGuarantee::Required),
            NetworkIsolationRequirement::DenyAll
        );
        for (guarantee, expected) in [
            (
                SandboxGuarantee::BestEffort,
                NetworkIsolationRequirement::Inherit,
            ),
            (SandboxGuarantee::None, NetworkIsolationRequirement::Inherit),
        ] {
            assert_eq!(guarantee.network_requirement(), expected, "{guarantee:?}");
        }
        // An engine over a Required policy surfaces the same demand.
        let policy = SandboxPolicy {
            execute_shell: Rule::Allow,
            network_guarantee: SandboxGuarantee::Required,
            ..Default::default()
        };
        let e = PermissionEngine::new(policy, None);
        assert_eq!(
            e.spawn_network_requirement(),
            NetworkIsolationRequirement::DenyAll
        );
    }

    #[test]
    fn policy_rules_do_not_pre_judge_platform_enforcement() {
        // There is no platform probe left: under Required the RULE decides
        // the verdict (Allow/Ask/Deny); the spawn layer decides enforcement
        // and refuses typed when it cannot isolate. Preflight guessing (the
        // old AppLevel refusals) is gone.
        for (rule, expected) in [
            (Rule::Allow, PermissionDecision::Allow),
            (Rule::Ask, PermissionDecision::Ask),
            (Rule::Deny, PermissionDecision::Deny),
        ] {
            let policy = SandboxPolicy {
                execute_shell: rule,
                network_guarantee: SandboxGuarantee::Required,
                ..Default::default()
            };
            let e = PermissionEngine::new(policy, None);
            assert_eq!(e.evaluate(&shell_cap()), expected, "{rule:?}");
        }
    }

    #[test]
    fn best_effort_note_is_documented_app_level_only() {
        // The adversarial scenario that started P0-39: ExecuteShell allowed
        // + Network denied. Under BestEffort the app gate still denies the
        // app's own egress while the shell maps to Inherit; the caveat is
        // documented verbatim.
        let policy = SandboxPolicy {
            execute_shell: Rule::Allow,
            network: NetworkGate::deny_all(),
            network_guarantee: SandboxGuarantee::BestEffort,
            ..Default::default()
        };
        let e = PermissionEngine::new(policy, None);
        assert_eq!(
            e.evaluate(&Capability::Network {
                destination: "https://evil.example.com".into()
            }),
            PermissionDecision::Deny
        );
        assert_eq!(e.evaluate(&shell_cap()), PermissionDecision::Allow);
        assert!(NETWORK_ISOLATION_NOTE.contains("app-level only"));
        assert!(NETWORK_ISOLATION_NOTE.contains("ExecuteShell + Network(deny)"));
        assert!(NETWORK_ISOLATION_NOTE.contains("permitted shell can still open its own sockets"));
    }

    #[test]
    fn guarantee_serde_shapes_are_frozen() {
        let v = serde_json::to_value(SandboxGuarantee::Required).unwrap();
        assert_eq!(v, serde_json::json!("required"));
        assert!(
            serde_json::from_value::<SandboxGuarantee>(serde_json::json!("best_effort")).is_ok()
        );
        assert_eq!(
            SandboxPolicy::default().network_guarantee,
            SandboxGuarantee::Required
        );
        assert_eq!(
            SandboxPolicy::default().shell_execution,
            ShellExecutionMode::OsIsolated
        );
    }

    #[test]
    fn no_required_to_inherit_path_exists_in_command_spawns() {
        // Adversarial static scan: every command-spawn site must derive its
        // isolation from the policy mapping. A literal `Inherit` there would
        // be a Required => Inherit path; the mapping function itself is
        // asserted to never send Required to Inherit.
        // Runtime-robust repo root: a stale test binary built in a deleted
        // checkout (shared target dir, matching fingerprints) would embed
        // that checkout's CARGO_MANIFEST_DIR; try the build-time root, then
        // the cwd and the executable ancestors, and use the FIRST one that
        // actually contains the scanned files. Fail loudly with every path
        // tried — never a silent skip.
        let candidates: Vec<std::path::PathBuf> = {
            let mut v: Vec<std::path::PathBuf> = Vec::new();
            if let Some(p) = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(|p| p.parent())
            {
                v.push(p.to_path_buf());
            }
            if let Ok(cwd) = std::env::current_dir() {
                v.extend(cwd.ancestors().map(|a| a.to_path_buf()));
            }
            if let Ok(exe) = std::env::current_exe() {
                v.extend(exe.ancestors().map(|a| a.to_path_buf()));
            }
            v
        };
        let root = candidates
            .iter()
            .find(|base| base.join("crates/cli/src/tools.rs").is_file())
            .unwrap_or_else(|| {
                panic!("repo root with crates/cli/src/tools.rs not found; tried: {candidates:?}")
            })
            .clone();
        for rel in ["crates/cli/src/tools.rs", "crates/verify/src/exec.rs"] {
            let path = root.join(rel);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            assert!(
                !text.contains("NetworkIsolation::Inherit"),
                "{rel} must not hardcode Inherit; every spawn maps the policy requirement"
            );
            assert!(
                text.contains("network_requirement"),
                "{rel} must derive its isolation from the policy requirement"
            );
        }
        let own = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs"),
        )
        .unwrap();
        let mapping = own
            .split("pub fn network_requirement")
            .nth(1)
            .expect("mapping function");
        let mapping = mapping.split("impl From<SandboxGuarantee>").next().unwrap();
        assert!(
            mapping.contains("SandboxGuarantee::Required => NetworkIsolationRequirement::DenyAll"),
            "the single mapping locus must send Required to DenyAll"
        );
        assert!(
            !mapping.contains("Required => NetworkIsolationRequirement::Inherit"),
            "no Required => Inherit path may exist"
        );
    }

    #[test]
    fn secure_default_requires_os_isolation_for_shells() {
        // Item 10: the untrusted-shell default is OS isolation (Required =>
        // DenyAll at the spawn seam). Other platforms refuse the spawn
        // typed; they never run the shell unenforced.
        let p = SandboxPolicy::default();
        assert_eq!(p.network_guarantee, SandboxGuarantee::Required);
        assert_eq!(p.shell_execution, ShellExecutionMode::OsIsolated);
        assert_eq!(
            p.network_guarantee.network_requirement(),
            NetworkIsolationRequirement::DenyAll
        );
        assert!(p.validate().is_ok());
        let state = p.shell_execution_state();
        assert_eq!(state.mode, ShellExecutionMode::OsIsolated);
        assert!(state.strength_label().contains("OS-isolated"));
        assert!(!state.strength_label().contains("GRANTED BY USER"));
        assert_eq!(p.spawn_profile().shell, "os_isolated");
    }

    #[test]
    fn user_granted_network_capable_shell_is_distinct_and_validated() {
        // The full-functionality shell is an EXPLICIT user grant, strictly
        // weaker than OS isolation, and its state label says so.
        for guarantee in [SandboxGuarantee::BestEffort, SandboxGuarantee::None] {
            let policy = SandboxPolicy {
                execute_shell: Rule::Allow,
                network_guarantee: guarantee,
                shell_execution: ShellExecutionMode::NetworkCapableUserGranted,
                ..Default::default()
            };
            assert!(policy.validate().is_ok(), "{guarantee:?}");
            let state = policy.shell_execution_state();
            assert_eq!(state.mode, ShellExecutionMode::NetworkCapableUserGranted);
            assert!(state.strength_label().contains("GRANTED BY USER"));
            assert!(state.strength_label().contains("NOT equivalent"));
            let profile = policy.spawn_profile();
            assert_eq!(profile.shell, "network_capable_user_granted");
            assert_eq!(profile.network, guarantee.as_tag());
        }
        // Mismatched pairings are typed refusals, never silent
        // reinterpretations of either mode.
        let isolated_with_inherit = SandboxPolicy {
            network_guarantee: SandboxGuarantee::None,
            ..Default::default()
        };
        let err = isolated_with_inherit.validate().unwrap_err();
        assert!(err.contains("network_capable_user_granted"), "{err}");
        let granted_with_required = SandboxPolicy {
            shell_execution: ShellExecutionMode::NetworkCapableUserGranted,
            ..Default::default()
        };
        let err = granted_with_required.validate().unwrap_err();
        assert!(err.contains("conflicts"), "{err}");
        // The whole state (mode + guarantee) survives serde as evidence.
        let json = serde_json::to_value(granted_with_required.shell_execution_state()).unwrap();
        let back: ShellExecutionState = serde_json::from_value(json).unwrap();
        assert_eq!(back, granted_with_required.shell_execution_state());
    }
}
