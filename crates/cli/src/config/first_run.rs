//! The explicit first-run sandbox shell choice (audit item 17).
//!
//! On platforms with no per-process network-isolation backend (macOS,
//! Windows, Linux hosts whose kernel/user-namespace policy refuses the
//! unshare), the secure default REFUSES agent shell and verification spawns
//! typed. That refusal is correct — but it must never be silently worked
//! around. This module owns the ONE explicit operator choice:
//!
//! - **(a) keep fail-closed** (the default): agent shells and verification
//!   stay OS-isolation-required; on backend-less platforms they refuse
//!   typed. Nothing is written and nothing changes.
//! - **(b) run network-capable shells** (an explicit operator grant): the
//!   agent shell-tool class switches to
//!   [`ShellExecutionMode::NetworkCapableUserGranted`] with the
//!   [`SandboxGuarantee::BestEffort`] app-level-only caveat, and the
//!   decision is recorded durably (a decision document plus an append-only
//!   audit line) with the exact trade-off text shown at grant time.
//!
//! The choice is presented (never made) at startup on backend-less
//! platforms; it is made only through `faktor sandbox choose …` (or an
//! explicit `[sandbox] shell` config, which always wins and is reported as
//! the explicit-config state). A corrupt or hostile decision file is a loud
//! error and the effective state stays fail-closed — the fail-closed
//! default is never weakened by a bad record. The decision document is
//! confirmed against the append-only audit trail on every read (the exact
//! choice, timestamp, platform, backend fact and trade-off must match one
//! entry), so editing the JSON alone — a hand-forged or flipped grant —
//! grants nothing. Both files are read through hard byte bounds (the
//! decision document 64 KiB, the scanned trail 4 MiB): a hostile
//! multi-gigabyte file is refused, never pulled into memory.
//!
//! **Every artifact read is anchored, opened once and handle-validated
//! (TOCTOU-proof).** The data dir is opened as a [`faktor_fs::RootedDir`]
//! authority; each artifact is opened through the anchored no-follow walk
//! (every component `O_NOFOLLOW`, the final open additionally `O_NONBLOCK`
//! on unix so a planted FIFO can never block the open), the OPENED HANDLE —
//! never a re-resolved pathname — is `fstat`-validated as a regular file
//! (the key is additionally required to be owner-only on unix), and the
//! bounded read happens on that SAME handle. A directory-entry race
//! (rename + symlink swap) can therefore only make an operation refuse; it
//! can never make the object inspected differ from the object read. Writes
//! stage an exclusively-created temp entry through the same authority and
//! publish it with a single anchored same-directory rename, so a swapped
//! entry is replaced, never followed.
//!
//! **Tamper evidence is keyed (HMAC-SHA256).** A daemon-local key
//! ([`SANDBOX_FIRST_RUN_KEY_FILE`], 32 random bytes created once on first
//! use, stored owner-only 0600 in the data dir, never logged) authenticates
//! the decision document and the audit trail: every audit line carries a
//! MAC over its own fields chained to the previous entry's MAC (so an edit,
//! a removal or a reorder of a retained entry breaks the chain), and the
//! decision document carries a MAC over its fields. A verification failure
//! is a TYPED refusal and the effective state stays fail-closed — no grant
//! is ever produced from an unauthenticated trail.
//!
//! **What the chain proves — and what it explicitly does NOT prove.** The
//! chain proves INTEGRITY BETWEEN RETAINED ENTRIES: authenticated content,
//! no edits, removals or reorders inside the retained window, and that the
//! decision document is consistent with that authenticated history. It does
//! NOT prove FRESHNESS of the head. Bounded compaction legitimately drops
//! predecessors and anchors the first retained entry as the new head, and a
//! MAC chain cannot distinguish that legitimate re-anchor from a
//! byte-verbatim restore of a previously-valid (decision document + trail)
//! pair: a rollback/replay of an earlier valid state VERIFIES and is
//! accepted. This is the honest, irreducible limit of an HMAC chain over
//! ONE local file domain — a same-user attacker who can restore those bytes
//! can restore (or remove) any other local file, so no check inside this
//! domain can be a freshness authority. Detecting rollback requires a
//! monotonic head stored in a DIFFERENT state domain (e.g. daemon secure
//! storage, or a control-plane monotonic sequence where configured) and
//! checked against the retained head; no such durable non-rollback domain
//! exists in this build, so the guarantee stated here is exactly the proven
//! one and the stronger claim (rollback detection) is NOT made. Upgrade
//! path: where a control-plane monotonic sequence is configured, pin the
//! head MAC there on every record and refuse a retained head older than the
//! pinned one; until then operators must treat a restored pair as
//! indistinguishable from the recorded state.
//!
//! The key is created on first use; a missing key over an existing DECISION
//! document is itself a typed refusal (re-onboarding requires removing the
//! decision deliberately), never a silent re-key, while an orphan audit
//! trail with no decision is unauthenticated history and is reset under the
//! new key.
//!
//! Where a real backend exists (Linux namespaces) nothing is faked: the
//! choice still records exactly what the operator asked for, and the spawn
//! layer continues to isolate children with the real backend.

use std::path::{Path, PathBuf};

use faktor_core::error::ErrorKind;
use faktor_fs::rooted::RootedEntryKind;
use faktor_fs::RootedDir;
use faktor_sandbox::{SandboxGuarantee, ShellExecutionMode};
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// The durable decision document under the data dir.
pub const SANDBOX_FIRST_RUN_FILE: &str = "sandbox-first-run.json";

/// The append-only audit file under the data dir (JSON lines).
pub const SANDBOX_FIRST_RUN_AUDIT_FILE: &str = "sandbox-first-run-audit.jsonl";

/// The daemon-local HMAC key file under the data dir: 32 raw random bytes,
/// created once on first use, owner-only (0600 on unix), never logged.
pub const SANDBOX_FIRST_RUN_KEY_FILE: &str = "sandbox-first-run.key";

/// Key length in bytes (exactly this; anything else is refused).
const KEY_BYTES: usize = 32;

/// Domain separation of the audit-entry and decision MACs: an entry MAC can
/// never be replayed as a decision MAC (or vice versa).
const AUDIT_MAC_DOMAIN: &[u8] = b"faktor-sandbox-first-run-audit/v1";
const DECISION_MAC_DOMAIN: &[u8] = b"faktor-sandbox-first-run-decision/v1";

/// The chain anchor of the first entry of a (possibly compacted) trail.
fn genesis_mac() -> String {
    "0".repeat(64)
}

/// Rotation bound: the audit file holds at most this many bytes before
/// being compacted (the retained complete-line tail plus the new entry), so
/// the trail stays bounded without sidecar files.
const AUDIT_MAX_BYTES: u64 = 1024 * 1024;

/// Hard bound on the decision document bytes: a legitimate record is a few
/// hundred bytes; anything larger is not one and is refused WITHOUT being
/// read into memory (a hostile multi-gigabyte file must never OOM the
/// daemon at startup or in doctor).
const DECISION_MAX_BYTES: u64 = 64 * 1024;

/// Hard bound on the audit bytes scanned to confirm a record. The live
/// trail is compacted at [`AUDIT_MAX_BYTES`], so a much larger file is
/// hostile or corrupt: it is refused unread instead of being pulled
/// wholesale into memory.
const AUDIT_CONFIRM_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Schema version of the decision document; only 1 is accepted.
pub const SANDBOX_FIRST_RUN_VERSION: u32 = 1;

/// The typed first-run choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxShellChoice {
    /// Option (a): keep the fail-closed default.
    FailClosed,
    /// Option (b): the explicit network-capable shell grant.
    NetworkCapableUserGranted,
}

impl SandboxShellChoice {
    /// The stable snake_case tag (also the serde tag).
    pub const fn as_tag(self) -> &'static str {
        match self {
            SandboxShellChoice::FailClosed => "fail_closed",
            SandboxShellChoice::NetworkCapableUserGranted => "network_capable_user_granted",
        }
    }

    /// The shell-execution mode this choice resolves to: `None` keeps the
    /// crate's secure default (fail-closed).
    pub const fn shell_mode(self) -> Option<ShellExecutionMode> {
        match self {
            SandboxShellChoice::FailClosed => None,
            SandboxShellChoice::NetworkCapableUserGranted => {
                Some(ShellExecutionMode::NetworkCapableUserGranted)
            }
        }
    }

    /// The network guarantee this choice resolves to (the app-level-only
    /// `BestEffort` caveat for the grant, the secure default otherwise).
    pub const fn network_guarantee(self) -> SandboxGuarantee {
        match self {
            SandboxShellChoice::FailClosed => SandboxGuarantee::Required,
            SandboxShellChoice::NetworkCapableUserGranted => SandboxGuarantee::BestEffort,
        }
    }
}

/// The exact trade-off text recorded with a network-capable grant. Frozen:
/// the doctor line, the CLI notice and the durable record carry this same
/// sentence, so the grant can never be presented as equivalent strength.
pub const NETWORK_CAPABLE_SHELL_TRADE_OFF: &str =
    "network-capable shells (strictly weaker than the fail-closed default): agent shell and \
     verification children inherit the daemon's network namespace — a permitted shell can \
     open its own sockets and direct network access is possible; the app-level destination \
     gate still applies, but no OS-level network isolation backs this grant";

/// The durable decision document.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SandboxFirstRunRecord {
    pub version: u32,
    pub choice: SandboxShellChoice,
    pub decided_at_ms: i64,
    /// The platform the choice was recorded on (`std::env::consts::OS`).
    pub platform: String,
    /// Whether a per-process network-isolation backend existed at decision
    /// time (the reason the choice was presented).
    pub network_backend_available: bool,
    /// The exact trade-off text shown at decision time (the frozen sentence
    /// for a grant; empty for fail-closed).
    pub trade_off: String,
    /// HMAC-SHA256 (lowercase hex) over this document's own fields under the
    /// daemon-local key; the document is additionally confirmed field-for-
    /// field against a chain-verified audit entry. A document edited without
    /// the key fails verification typed. `#[serde(default)]`: a document
    /// without it still PARSES and then fails verification (fail closed),
    /// instead of masquerading as a schema error.
    #[serde(default)]
    pub mac: String,
}

/// One append-only audit line, chained to its predecessor.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SandboxFirstRunAuditEntry {
    event: String,
    choice: SandboxShellChoice,
    decided_at_ms: i64,
    platform: String,
    network_backend_available: bool,
    trade_off: String,
    pid: u32,
    /// MAC of the previous VALID entry of the trail (`0` * 64 when this is
    /// the first entry of a trail, including the retained prefix after a
    /// bounded compaction). An edited/removed/reordered retained entry
    /// breaks the linkage of every entry after it. Linkage is integrity,
    /// NOT freshness: a byte-verbatim restore of an earlier valid trail
    /// re-anchors silently at its first entry (module docs).
    prev_mac: String,
    /// HMAC-SHA256 (lowercase hex) over every field above, this one included.
    mac: String,
}

/// The effect of the first-run choice on THIS daemon process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxFirstRunDecision {
    /// `[sandbox] shell` is set in the config file: the operator's explicit
    /// config wins and the durable record (if any) is informational only.
    ExplicitConfig,
    /// Fail-closed (the secure default): either explicitly recorded, or no
    /// choice was needed because the platform has a kernel backend.
    FailClosed,
    /// The durable explicit grant is in force.
    NetworkCapableUserGranted,
    /// A backend-less platform with no explicit choice anywhere: the typed
    /// choice must be presented; the EFFECTIVE state is fail-closed.
    Pending,
}

impl SandboxFirstRunDecision {
    pub const fn as_tag(self) -> &'static str {
        match self {
            SandboxFirstRunDecision::ExplicitConfig => "explicit_config",
            SandboxFirstRunDecision::FailClosed => "fail_closed",
            SandboxFirstRunDecision::NetworkCapableUserGranted => "network_capable_user_granted",
            SandboxFirstRunDecision::Pending => "pending",
        }
    }
}

/// The inspected first-run state of one daemon process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxFirstRunState {
    pub decision: SandboxFirstRunDecision,
    /// True when this build/platform has a per-process network-isolation
    /// backend (Linux network namespaces, macOS sandbox profiles). On
    /// backend-less platforms the choice is presented at startup.
    pub backend_available: bool,
    /// The durable record when one exists.
    pub record: Option<SandboxFirstRunRecord>,
    /// A corrupt decision document (loud, and the state stays fail-closed).
    pub corrupt_reason: Option<String>,
}

impl SandboxFirstRunState {
    /// True when the startup notice must present the typed choice.
    pub fn pending(&self) -> bool {
        self.decision == SandboxFirstRunDecision::Pending
    }
}

/// True when THIS build/platform has a per-process network-isolation
/// backend (the same compile-time fact the terminal crate reports: Linux
/// network namespaces or macOS sandbox profiles).
pub fn network_backend_available() -> bool {
    faktor_terminal::deny_all_supported()
}

/// The decision document path under `data_dir`.
pub fn decision_path(data_dir: &Path) -> PathBuf {
    data_dir.join(SANDBOX_FIRST_RUN_FILE)
}

/// The audit file path under `data_dir`.
pub fn audit_path(data_dir: &Path) -> PathBuf {
    data_dir.join(SANDBOX_FIRST_RUN_AUDIT_FILE)
}

/// The HMAC key path under `data_dir`.
pub fn key_path(data_dir: &Path) -> PathBuf {
    data_dir.join(SANDBOX_FIRST_RUN_KEY_FILE)
}

// ---------------------------------------------------------------------------
// The anchored artifact authority: one data-dir handle per operation, one
// open per artifact, handle-validated reads (the TOCTOU discipline).
// ---------------------------------------------------------------------------

/// The decision document name as a rooted-relative path.
fn decision_rel() -> &'static Path {
    Path::new(SANDBOX_FIRST_RUN_FILE)
}

/// The audit file name as a rooted-relative path.
fn audit_rel() -> &'static Path {
    Path::new(SANDBOX_FIRST_RUN_AUDIT_FILE)
}

/// The integrity key name as a rooted-relative path.
fn key_rel() -> &'static Path {
    Path::new(SANDBOX_FIRST_RUN_KEY_FILE)
}

/// Open the data dir as the rooted authority for the first-run artifacts:
/// the directory entry is opened ONCE and every artifact resolves relative
/// to that handle. `Ok(None)` when the data dir does not exist yet (nothing
/// can be there); a symlinked or non-directory data-dir entry is a typed
/// refusal (fail closed — the containment root must be real).
fn data_root(data_dir: &Path) -> Result<Option<RootedDir>, String> {
    match RootedDir::open(data_dir) {
        Ok(root) => Ok(Some(root)),
        Err(e) if e.kind == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {}", data_dir.display(), e.message)),
    }
}

/// The module's canonical refusal for a non-regular artifact object.
fn refuse_non_regular(label: &Path, detail: &str) -> String {
    format!(
        "{}: not a regular file (refusing symlinks, FIFOs, sockets and devices): {detail}",
        label.display()
    )
}

/// Open `rel` under `root` with the anchored no-follow walk and validate
/// the OPENED HANDLE: the entry must be a regular file, classified by
/// `fstat` of the handle itself — never by re-resolving a pathname.
/// `Ok(None)` when the entry does not exist. A symlink, FIFO, socket,
/// device or directory is a typed refusal; the final open carries
/// `O_NONBLOCK` on unix, so a planted FIFO can never block the open.
fn open_regular(
    root: &RootedDir,
    rel: &Path,
    label: &Path,
) -> Result<Option<std::fs::File>, String> {
    let file = match root.open_read(rel) {
        Ok(file) => file,
        Err(e) if e.kind == ErrorKind::NotFound => return Ok(None),
        Err(e) if e.kind == ErrorKind::Permission => {
            return Err(refuse_non_regular(label, &e.message))
        }
        Err(e) => return Err(format!("{}: {}", label.display(), e.message)),
    };
    // The handle — not the pathname — is classified: what was validated is
    // exactly what will be read.
    let meta = file
        .metadata()
        .map_err(|e| format!("{}: {e}", label.display()))?;
    if !meta.is_file() {
        return Err(refuse_non_regular(
            label,
            "the opened handle is not a regular file",
        ));
    }
    Ok(Some(file))
}

/// TRUE when the artifact exists as a REGULAR file through the anchored
/// no-follow walk; `Ok(false)` when nothing is there; anything else (a
/// symlink, FIFO, socket, device or directory) is a typed refusal. The
/// classification never opens the entry, so a planted FIFO can never block.
fn regular_artifact_exists(root: &RootedDir, rel: &Path, label: &Path) -> Result<bool, String> {
    match root.entry_meta(rel) {
        Ok(Some(meta)) if meta.kind == RootedEntryKind::File => Ok(true),
        Ok(Some(_)) => Err(refuse_non_regular(label, "the entry is not a regular file")),
        Ok(None) => Ok(false),
        Err(e) => Err(format!("{}: {}", label.display(), e.message)),
    }
}

/// Test seam: fires with the artifact's label path AFTER the anchored open
/// validated the HANDLE and BEFORE the bounded read, so adversarial tests
/// can swap the entry (rename + symlink) in exactly that window and prove
/// the read still operates on the validated handle — never on a re-resolved
/// pathname.
#[cfg(test)]
type ArtifactReadSeam = Box<dyn Fn(&Path) + Send>;
#[cfg(test)]
static ARTIFACT_READ_SEAM: std::sync::OnceLock<std::sync::Mutex<Option<ArtifactReadSeam>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
pub(crate) fn install_artifact_read_seam(hook: ArtifactReadSeam) {
    let slot = ARTIFACT_READ_SEAM.get_or_init(|| std::sync::Mutex::new(None));
    *slot.lock().expect("artifact read seam poisoned") = Some(hook);
}

#[cfg(test)]
pub(crate) fn clear_artifact_read_seam() {
    if let Some(slot) = ARTIFACT_READ_SEAM.get() {
        *slot.lock().expect("artifact read seam poisoned") = None;
    }
}

#[cfg(test)]
fn artifact_read_seam(label: &Path) {
    if let Some(slot) = ARTIFACT_READ_SEAM.get() {
        if let Some(hook) = slot.lock().expect("artifact read seam poisoned").as_ref() {
            hook(label);
        }
    }
}
#[cfg(not(test))]
fn artifact_read_seam(_label: &Path) {}

/// Read at most `cap` bytes FROM THE ALREADY-OPENED, VALIDATED HANDLE,
/// refusing (never truncating) anything larger. The file's own metadata is
/// only a hint: the READ itself is capped, so a stat-size-zero stream file
/// or a file growing mid-read can never exceed the bound (the bounded-read
/// rule of audit 50e2dca). No pathname is involved between the bound and
/// the bytes.
fn read_bounded_handle(
    file: &mut std::fs::File,
    cap: u64,
    label: &Path,
) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    file.take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{}: {e}", label.display()))?;
    if bytes.len() as u64 > cap {
        return Err(format!(
            "{}: larger than the {cap}-byte bound; refusing to read it",
            label.display()
        ));
    }
    Ok(bytes)
}

/// The ONE read path for the decision document and the audit trail: open
/// through the anchored authority, validate the HANDLE as a regular file,
/// then bounded-read from that SAME handle. `Ok(None)` when the entry does
/// not exist.
fn read_artifact(
    root: &RootedDir,
    rel: &Path,
    label: &Path,
    cap: u64,
) -> Result<Option<Vec<u8>>, String> {
    let Some(mut file) = open_regular(root, rel, label)? else {
        return Ok(None);
    };
    artifact_read_seam(label);
    read_bounded_handle(&mut file, cap, label).map(Some)
}

/// Validate the OPENED key handle before any byte is read: `open_regular`
/// already proved the handle is a regular file; on unix the handle's mode
/// must additionally deny group/other access (0600 or stricter) — a key any
/// other identity can read or write cannot anchor the tamper evidence it
/// vouches for, so it is refused typed. Non-unix hosts have no POSIX mode
/// bits (documented platform limitation, parity with key creation).
fn validate_key_handle(file: &std::fs::File, label: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let meta = file
            .metadata()
            .map_err(|e| format!("{}: {e}", label.display()))?;
        let mode = meta.mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(format!(
                "{}: the integrity key is accessible to other users (mode {mode:o}); refusing \
                 it — a key others can read or write cannot anchor the tamper evidence",
                label.display()
            ));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (file, label);
    }
    Ok(())
}

/// Lowercase hex of a byte string.
fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// TRUE for a lowercase-or-uppercase 64-hex string (a blake3/HMAC digest).
fn is_hex64(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// HMAC-SHA256 (lowercase hex) over `domain` plus NUL-separated parts, so no
/// field concatenation can be confused with another.
fn hmac_hex(key: &[u8; KEY_BYTES], domain: &[u8], parts: &[&[u8]]) -> String {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(domain);
    for part in parts {
        mac.update(&[0]);
        mac.update(part);
    }
    to_hex(&mac.finalize().into_bytes())
}

/// The MAC of one audit entry over every field plus its chain predecessor.
fn audit_entry_mac(key: &[u8; KEY_BYTES], entry: &SandboxFirstRunAuditEntry) -> String {
    let decided_at = entry.decided_at_ms.to_string();
    let pid = entry.pid.to_string();
    hmac_hex(
        key,
        AUDIT_MAC_DOMAIN,
        &[
            entry.event.as_bytes(),
            entry.choice.as_tag().as_bytes(),
            decided_at.as_bytes(),
            entry.platform.as_bytes(),
            if entry.network_backend_available {
                b"true"
            } else {
                b"false"
            },
            entry.trade_off.as_bytes(),
            pid.as_bytes(),
            entry.prev_mac.as_bytes(),
        ],
    )
}

/// The MAC of the decision document over its own fields. The document is
/// additionally confirmed field-for-field against a chain-verified audit
/// entry, so both the document and the trail must authenticate.
fn decision_mac(key: &[u8; KEY_BYTES], record: &SandboxFirstRunRecord) -> String {
    let version = record.version.to_string();
    let decided_at = record.decided_at_ms.to_string();
    hmac_hex(
        key,
        DECISION_MAC_DOMAIN,
        &[
            version.as_bytes(),
            record.choice.as_tag().as_bytes(),
            decided_at.as_bytes(),
            record.platform.as_bytes(),
            if record.network_backend_available {
                b"true"
            } else {
                b"false"
            },
            record.trade_off.as_bytes(),
        ],
    )
}

/// TRUE when every parseable audit entry authenticates under `key` with an
/// intact chain. The first parsed entry is the anchor: bounded compaction
/// legitimately drops predecessors and anchors the first retained entry, so
/// anchoring is never a freshness check (the chain proves integrity between
/// retained entries — a verbatim restore of a previously-valid pair is
/// accepted BY DESIGN; module docs). Unparseable lines carry no chain and
/// are ignored, exactly like verification. The read goes through the
/// validated handle and is byte-bounded; an oversized trail is a typed
/// error.
fn trail_authenticates(
    root: &RootedDir,
    key: &[u8; KEY_BYTES],
    data_dir: &Path,
) -> Result<bool, String> {
    let Some(bytes) = read_artifact(
        root,
        audit_rel(),
        &audit_path(data_dir),
        AUDIT_CONFIRM_MAX_BYTES,
    )?
    else {
        return Ok(true);
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut prev: Option<String> = None;
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<SandboxFirstRunAuditEntry>(line) else {
            continue;
        };
        if !is_hex64(&entry.mac) || !is_hex64(&entry.prev_mac) {
            return Ok(false);
        }
        if let Some(prev) = &prev {
            if &entry.prev_mac != prev {
                return Ok(false);
            }
        }
        if entry.mac != audit_entry_mac(key, &entry) {
            return Ok(false);
        }
        prev = Some(entry.mac.clone());
    }
    Ok(true)
}

/// Read the HMAC key through the anchored authority. `Ok(None)` when the
/// file is absent; a non-regular key object, an other-accessible key or a
/// wrong-length key is a typed refusal (never silently re-keyed).
fn read_key(root: &RootedDir, data_dir: &Path) -> Result<Option<[u8; KEY_BYTES]>, String> {
    let label = key_path(data_dir);
    let Some(mut file) = open_regular(root, key_rel(), &label)? else {
        return Ok(None);
    };
    validate_key_handle(&file, &label)?;
    artifact_read_seam(&label);
    let bytes = read_bounded_handle(&mut file, KEY_BYTES as u64, &label)?;
    if bytes.len() != KEY_BYTES {
        return Err(format!(
            "{}: the integrity key must be exactly {KEY_BYTES} bytes (got {}); refusing it",
            label.display(),
            bytes.len()
        ));
    }
    let mut key = [0u8; KEY_BYTES];
    key.copy_from_slice(&bytes);
    Ok(Some(key))
}

/// Load the daemon-local key, creating it ON FIRST USE (32 random bytes,
/// owner-only 0600 on unix, never logged). A missing key while a durable
/// DECISION document with a WELL-FORMED MAC exists is a typed refusal: that
/// document may only be authenticated by its original key, and minting a
/// fresh one would make it permanently unverifiable — re-onboarding requires
/// removing the decision deliberately. A document whose MAC is absent or
/// malformed cannot have been signed by ANY key (forged or truncated), so it
/// is replaced by the authenticated record instead of bricking first use;
/// an orphan audit trail (no authenticatable decision) is likewise
/// unauthenticated history and is reset under the new key.
fn load_or_create_key(data_dir: &Path) -> Result<([u8; KEY_BYTES], bool), String> {
    let mut root = data_root(data_dir)?;
    if let Some(root_ref) = &root {
        if let Some(key) = read_key(root_ref, data_dir)? {
            return Ok((key, false));
        }
        let decision = decision_path(data_dir);
        if let Some(bytes) = read_artifact(root_ref, decision_rel(), &decision, DECISION_MAX_BYTES)?
        {
            let claims_authentication = serde_json::from_slice::<SandboxFirstRunRecord>(&bytes)
                .map(|record| is_hex64(&record.mac))
                .unwrap_or(false);
            if claims_authentication {
                return Err(format!(
                    "{} is missing while a durable sandbox first-run decision carrying an HMAC \
                     exists; refusing to mint a new integrity key over an existing decision \
                     (remove the decision and audit files to re-onboard)",
                    key_path(data_dir).display()
                ));
            }
        }
    }
    let root = match root.take() {
        Some(root) => root,
        None => {
            std::fs::create_dir_all(data_dir)
                .map_err(|e| format!("{}: {e}", data_dir.display()))?;
            data_root(data_dir)?.ok_or_else(|| {
                format!("{}: data dir vanished after creation", data_dir.display())
            })?
        }
    };
    use rand::Rng as _;
    let mut key = [0u8; KEY_BYTES];
    rand::rng().fill(&mut key);
    let label = key_path(data_dir);
    let mut file = match root.open_create_new(key_rel()) {
        Ok(file) => file,
        Err(e) => {
            // A concurrent first use won the race: adopt the existing key.
            if let Some(existing) = read_key(&root, data_dir)? {
                return Ok((existing, false));
            }
            return Err(format!("{}: {}", label.display(), e.message));
        }
    };
    use std::io::Write as _;
    file.write_all(&key)
        .map_err(|e| format!("{}: {e}", label.display()))?;
    file.sync_all()
        .map_err(|e| format!("{}: {e}", label.display()))?;
    // Belt and braces on the HANDLE (fchmod), never a pathname chmod: a
    // platform umask that widened the creation mode is corrected on the
    // object that was just created. (Non-unix hosts have no POSIX mode
    // bits; the honest platform limitation is documented in the module
    // docs.)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("{}: {e}", label.display()))?;
    }
    drop(file);
    Ok((key, true))
}

/// ONE validated-handle read of the current trail for the append path: the
/// MAC of its last well-formed entry (the chaining target for the next
/// line) and the retained content the next publish carries — the whole
/// trail when within [`AUDIT_MAX_BYTES`], otherwise the bounded
/// complete-line tail window (exactly the content compaction keeps, so a
/// hostile oversized trail is never pulled into memory). `(genesis, "")`
/// when the trail is absent.
fn trail_for_append(root: &RootedDir, data_dir: &Path) -> Result<(String, String), String> {
    let label = audit_path(data_dir);
    let Some(mut file) = open_regular(root, audit_rel(), &label)? else {
        return Ok((genesis_mac(), String::new()));
    };
    artifact_read_seam(&label);
    let len = file
        .metadata()
        .map_err(|e| format!("{}: {e}", label.display()))?
        .len();
    let content: Vec<u8> = if len > AUDIT_MAX_BYTES {
        use std::io::{Read as _, Seek as _, SeekFrom};
        // Only a BOUNDED TAIL of the file is ever read: the retention window
        // is at most half the bound plus one byte, and trimming to the first
        // newline inside it drops the possibly partial leading line.
        let window = (AUDIT_MAX_BYTES / 2) as usize + 1;
        let start = len.saturating_sub(window as u64);
        file.seek(SeekFrom::Start(start))
            .map_err(|e| format!("{}: {e}", label.display()))?;
        let mut content = Vec::new();
        file.by_ref()
            .take(window as u64)
            .read_to_end(&mut content)
            .map_err(|e| format!("{}: {e}", label.display()))?;
        let cut = content
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|offset| offset + 1)
            .unwrap_or(content.len());
        content.split_off(cut)
    } else {
        read_bounded_handle(&mut file, AUDIT_MAX_BYTES, &label)?
    };
    let text = String::from_utf8_lossy(&content);
    let mut last = genesis_mac();
    for line in text.lines() {
        if let Ok(entry) = serde_json::from_str::<SandboxFirstRunAuditEntry>(line) {
            if is_hex64(&entry.prev_mac) && is_hex64(&entry.mac) {
                last = entry.mac;
            }
        }
    }
    Ok((last, text.into_owned()))
}

/// Read the durable decision through an ALREADY-OPENED data-dir authority.
/// `Ok(None)` when none exists; a corrupt, oversized, non-regular or
/// schema-mismatched document is a typed error (the caller keeps the
/// fail-closed default and surfaces the refusal).
fn load_decision_in(
    root: &RootedDir,
    data_dir: &Path,
) -> Result<Option<SandboxFirstRunRecord>, String> {
    let path = decision_path(data_dir);
    let Some(bytes) = read_artifact(root, decision_rel(), &path, DECISION_MAX_BYTES)? else {
        return Ok(None);
    };
    let record: SandboxFirstRunRecord =
        serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    if record.version != SANDBOX_FIRST_RUN_VERSION {
        return Err(format!(
            "{}: unsupported version {} (this build accepts {SANDBOX_FIRST_RUN_VERSION})",
            path.display(),
            record.version
        ));
    }
    Ok(Some(record))
}

/// Read the durable decision. `Ok(None)` when none exists (or the data dir
/// does not exist yet); a corrupt, oversized, non-regular or
/// schema-mismatched document is a typed error (the caller keeps the
/// fail-closed default and surfaces the refusal).
///
/// This is the RAW decode only: the document's HMAC and its audit-chain
/// confirmation are enforced by [`audit_confirms`] on every granting path
/// ([`inspect`] / [`apply_effective_choice`]) — this call alone never
/// produces a grant. `inspect` uses the anchored variant to share ONE
/// data-dir authority across the decision, key and audit reads.
#[allow(dead_code)] // raw-decode surface pinned by tests; inspect shares one anchored authority
pub fn load_decision(data_dir: &Path) -> Result<Option<SandboxFirstRunRecord>, String> {
    match data_root(data_dir)? {
        Some(root) => load_decision_in(&root, data_dir),
        None => Ok(None),
    }
}

/// The audit event name of one choice: shared by the writer and the
/// confirmation read, so a record must be backed by the event for ITS OWN
/// choice (a flipped `choice` field can never reuse the other event).
fn audit_event(choice: SandboxShellChoice) -> &'static str {
    match choice {
        SandboxShellChoice::FailClosed => "sandbox_choice_fail_closed_recorded",
        SandboxShellChoice::NetworkCapableUserGranted => "sandbox_network_capable_shell_granted",
    }
}

/// Confirm a decision document against the append-only trail under the
/// daemon-local HMAC key:
///
/// 1. the document's own MAC must verify under the key;
/// 2. every parseable audit entry must carry well-formed MACs and its MAC
///    must verify over its exact fields chained to the previous entry (the
///    first parsed entry is the chain anchor — bounded compaction retains a
///    tail of complete lines whose predecessor may have been dropped, so
///    anchoring the first retained entry is legitimate; linkage is
///    integrity between retained entries, NOT freshness, and a
///    byte-verbatim restore of a previously-valid pair verifies BY DESIGN —
///    see the module docs); an edited, removed or reordered retained entry
///    therefore breaks the chain;
/// 3. one chain-verified entry must match the document's exact event,
///    choice, timestamp, platform, backend fact and trade-off text.
///
/// Any failure is a TYPED refusal: the caller keeps the fail-closed default
/// (no grant) and reports it. Without the key a same-user attacker editing
/// the decision and its audit entry cannot produce a verifying pair. The
/// scan is byte-bounded ([`AUDIT_CONFIRM_MAX_BYTES`]) and runs entirely on
/// handles validated once by the anchored authority.
fn audit_confirms(
    root: &RootedDir,
    record: &SandboxFirstRunRecord,
    data_dir: &Path,
) -> Result<(), String> {
    let path = audit_path(data_dir);
    let refusal = |detail: String| {
        format!(
            "the decision document {} fails integrity verification: {detail}; refusing it and \
             keeping the fail-closed default",
            decision_path(data_dir).display()
        )
    };
    if !is_hex64(&record.mac) {
        return Err(refusal(
            "the document carries no valid HMAC (forged or truncated)".into(),
        ));
    }
    let key = match read_key(root, data_dir) {
        Ok(Some(key)) => key,
        Ok(None) => {
            return Err(refusal(format!(
                "the daemon-local integrity key {} is missing",
                key_path(data_dir).display()
            )))
        }
        Err(e) => return Err(refusal(e)),
    };
    let bytes = match read_artifact(root, audit_rel(), &path, AUDIT_CONFIRM_MAX_BYTES) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Err(refusal(format!("{} is absent", path.display()))),
        Err(e) => return Err(refusal(e)),
    };
    let text = String::from_utf8_lossy(&bytes);
    let expected_event = audit_event(record.choice);
    let mut prev: Option<String> = None;
    let mut matched = false;
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<SandboxFirstRunAuditEntry>(line) else {
            continue;
        };
        if !is_hex64(&entry.mac) || !is_hex64(&entry.prev_mac) {
            return Err(refusal(format!(
                "an audit entry in {} carries a malformed HMAC (tampered)",
                path.display()
            )));
        }
        if let Some(prev) = &prev {
            if &entry.prev_mac != prev {
                return Err(refusal(format!(
                    "the audit chain in {} is broken (an entry was replaced, removed or \
                     reordered)",
                    path.display()
                )));
            }
        }
        if entry.mac != audit_entry_mac(&key, &entry) {
            return Err(refusal(format!(
                "an audit entry in {} fails its HMAC (tampered)",
                path.display()
            )));
        }
        prev = Some(entry.mac.clone());
        if entry.event == expected_event
            && entry.choice == record.choice
            && entry.decided_at_ms == record.decided_at_ms
            && entry.platform == record.platform
            && entry.network_backend_available == record.network_backend_available
            && entry.trade_off == record.trade_off
        {
            // Remember the match but keep walking: the ENTIRE trail must
            // authenticate before the document is accepted, so a reordered
            // trail cannot sneak a genuine entry to the anchor position.
            matched = true;
        }
    }
    if matched {
        if decision_mac(&key, record) != record.mac {
            return Err(refusal(
                "the document's HMAC does not verify under the daemon-local key (tampered)".into(),
            ));
        }
        return Ok(());
    }
    Err(refusal(format!(
        "the document matches no chain-verified audit entry in {} (an operator decision is \
         confirmed only by its authenticated append-only record)",
        path.display(),
    )))
}

/// Write `bytes` at `rel` through the data-dir authority: the payload is
/// staged in a uniquely-named internal temp created EXCLUSIVELY through the
/// anchored walk (never by pathname), fsynced, then published as a single
/// same-directory rename (the fs-atomic authority's rooted publish). On any
/// failure the staged temp is removed and the destination keeps its
/// previous state; a symlink or special entry swapped in at the destination
/// name is replaced by the rename — never followed.
fn write_artifact(root: &RootedDir, rel: &Path, bytes: &[u8], label: &Path) -> Result<(), String> {
    use rand::Rng as _;
    let name = rel.to_string_lossy();
    let staged_rel = PathBuf::from(format!(
        ".{name}{}{}-{}",
        faktor_fs::atomic::FAKTOR_TEMP_MARKER,
        std::process::id(),
        rand::rng().random::<u64>(),
    ));
    let mut file = root
        .open_create_new(&staged_rel)
        .map_err(|e| format!("{}: {}", label.display(), e.message))?;
    use std::io::Write as _;
    let staged = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if let Err(e) = staged {
        let _ = root.remove_file(&staged_rel);
        return Err(format!("{}: {e}", label.display()));
    }
    root.atomic_publish(&staged_rel, rel).map_err(|e| {
        let _ = root.remove_file(&staged_rel);
        format!("{}: {}", label.display(), e.message)
    })
}

/// Record an explicit operator choice: one append-only audit line FIRST
/// (fsynced, MAC-chained under the daemon-local key), then the durable
/// decision document (MAC'd, anchored atomic replace), so a readable
/// decision is always backed by its authenticated trail. The key is created
/// on first use. All writes go through the data-dir authority (a staged
/// temp entry plus a single anchored rename), so no write can follow a
/// swapped entry. Returns the stored record.
pub fn record_decision(
    data_dir: &Path,
    choice: SandboxShellChoice,
    decided_at_ms: i64,
) -> Result<SandboxFirstRunRecord, String> {
    std::fs::create_dir_all(data_dir).map_err(|e| format!("{}: {e}", data_dir.display()))?;
    let Some(root) = data_root(data_dir)? else {
        return Err(format!(
            "{}: data dir vanished after creation",
            data_dir.display()
        ));
    };
    let (key, key_created) = load_or_create_key(data_dir)?;
    let trail_exists = regular_artifact_exists(&root, audit_rel(), &audit_path(data_dir))?;
    let orphan_trail = key_created && trail_exists;
    // Recording over a trail that does NOT authenticate under the CURRENT
    // key (tampered entry, or the key was replaced) is refused typed:
    // appending there would produce a decision record that can never
    // verify. The operator re-onboards by removing the decision and audit
    // files.
    if !key_created && trail_exists && !trail_authenticates(&root, &key, data_dir)? {
        return Err(format!(
            "the sandbox first-run audit trail {} fails HMAC verification under the daemon-local \
             key; refusing to record over a tampered trail or a rotated key (remove the decision \
             and audit files to re-onboard)",
            audit_path(data_dir).display()
        ));
    }
    let mut record = SandboxFirstRunRecord {
        version: SANDBOX_FIRST_RUN_VERSION,
        choice,
        decided_at_ms,
        platform: std::env::consts::OS.to_string(),
        network_backend_available: network_backend_available(),
        trade_off: match choice {
            SandboxShellChoice::FailClosed => String::new(),
            SandboxShellChoice::NetworkCapableUserGranted => {
                NETWORK_CAPABLE_SHELL_TRADE_OFF.to_string()
            }
        },
        mac: String::new(),
    };
    // One validated-handle snapshot read of the current trail supplies BOTH
    // the chaining MAC and the retained content the publish carries; an
    // orphan trail starts a fresh chain (its old bytes cannot be
    // authenticated under the new key and chaining onto them would poison
    // every future verification).
    let (prev_mac, retained) = if orphan_trail {
        (genesis_mac(), String::new())
    } else {
        trail_for_append(&root, data_dir)?
    };
    let chained = SandboxFirstRunAuditEntry {
        event: audit_event(record.choice).to_string(),
        choice: record.choice,
        decided_at_ms: record.decided_at_ms,
        platform: record.platform.clone(),
        network_backend_available: record.network_backend_available,
        trade_off: record.trade_off.clone(),
        pid: std::process::id(),
        prev_mac,
        mac: String::new(),
    };
    let entry = SandboxFirstRunAuditEntry {
        mac: audit_entry_mac(&key, &chained),
        ..chained
    };
    record.mac = decision_mac(&key, &record);
    let bytes = serde_json::to_vec_pretty(&record).map_err(|e| e.to_string())?;
    // The audit line lands FIRST: a decision document is only ever read as
    // an operator decision when its authenticated append-only entry already
    // exists, so a forged document alone can never grant anything. A crash
    // between the two writes leaves an audit entry with the PREVIOUS (or
    // no) decision document — the effective state stays exactly what it
    // was, never a half grant.
    let line = serde_json::to_string(&entry).map_err(|e| e.to_string())?;
    let mut compacted = String::with_capacity(retained.len() + line.len() + 1);
    compacted.push_str(&retained);
    compacted.push_str(&line);
    compacted.push('\n');
    write_artifact(
        &root,
        audit_rel(),
        compacted.as_bytes(),
        &audit_path(data_dir),
    )?;
    write_artifact(&root, decision_rel(), &bytes, &decision_path(data_dir))?;
    Ok(record)
}

/// Inspect the first-run state WITHOUT mutating the config: reads the
/// durable record, CONFIRMS it against the append-only audit trail, and
/// combines it with the config's explicit `shell` value. A corrupt,
/// oversized or unconfirmed record yields a `Pending`/`FailClosed` state
/// with `corrupt_reason` set — never a grant, and never a record presented
/// as recorded. The data dir is opened ONCE and every artifact read is
/// handle-validated against that same authority.
pub fn inspect(data_dir: &Path, config: &crate::config::Config) -> SandboxFirstRunState {
    let backend_available = network_backend_available();
    let (record, corrupt_reason) = match data_root(data_dir) {
        Ok(Some(root)) => match load_decision_in(&root, data_dir) {
            Ok(Some(record)) => match audit_confirms(&root, &record, data_dir) {
                Ok(()) => (Some(record), None),
                Err(reason) => (None, Some(reason)),
            },
            Ok(None) => (None, None),
            Err(reason) => (None, Some(reason)),
        },
        Ok(None) => (None, None),
        Err(reason) => (None, Some(reason)),
    };
    let decision = if config.sandbox.shell.is_some() {
        // The operator's explicit config is a declaration and wins over any
        // durable record — even an unreadable one (which is still a loud
        // issue, reported alongside).
        SandboxFirstRunDecision::ExplicitConfig
    } else if corrupt_reason.is_some() {
        // Corruption never weakens the default: fail closed, loudly.
        SandboxFirstRunDecision::FailClosed
    } else {
        match (&record, backend_available) {
            (Some(record), _) => match record.choice {
                SandboxShellChoice::FailClosed => SandboxFirstRunDecision::FailClosed,
                SandboxShellChoice::NetworkCapableUserGranted => {
                    SandboxFirstRunDecision::NetworkCapableUserGranted
                }
            },
            (None, true) => SandboxFirstRunDecision::FailClosed,
            (None, false) => SandboxFirstRunDecision::Pending,
        }
    };
    SandboxFirstRunState {
        decision,
        backend_available,
        record,
        corrupt_reason,
    }
}

/// Apply the first-run choice to a config loaded by a daemon entry point:
/// an explicit `[sandbox] shell` always wins; otherwise a durable grant
/// switches the AGENT shell class to `network_capable_user_granted` with
/// the app-level-only `BestEffort` guarantee. Returns the inspected state
/// (after mutation) so callers can present it.
pub fn apply_effective_choice(
    config: &mut crate::config::Config,
    data_dir: &Path,
) -> SandboxFirstRunState {
    let state = inspect(data_dir, config);
    if state.corrupt_reason.is_some() {
        tracing::error!(
            "sandbox first-run decision is unreadable; keeping the fail-closed default: {}",
            state.corrupt_reason.as_deref().unwrap_or_default()
        );
    }
    if state.decision == SandboxFirstRunDecision::NetworkCapableUserGranted {
        // The record is only reachable when the config left `shell` unset
        // (otherwise `inspect` reports ExplicitConfig), so this mutation
        // cannot override an operator's config.
        config.sandbox.shell = SandboxShellChoice::NetworkCapableUserGranted.shell_mode();
        config.sandbox.network_guarantee =
            SandboxShellChoice::NetworkCapableUserGranted.network_guarantee();
    }
    state
}

/// The startup notice text for a state: `None` when nothing must be
/// presented. This is the typed presentation (the choice itself is only
/// made through `faktor sandbox choose …` or the config file).
pub fn startup_notice(state: &SandboxFirstRunState, data_dir: &Path) -> Option<String> {
    let data_dir_display = data_dir.display();
    if state.pending() {
        return Some(format!(
            "sandbox first-run choice REQUIRED on platform '{}' (no per-process \
             network-isolation backend): agent shell and verification commands stay \
             FAIL-CLOSED (the secure default) until an operator chooses explicitly.\n\
             \x20 (a) keep fail-closed (default, nothing to do): run `faktor sandbox choose \
             --fail-closed --data-dir {data_dir_display}` to record that decision.\n\
             \x20 (b) grant network-capable shells: run `faktor sandbox choose \
             --network-capable-shell --data-dir {data_dir_display}` after reading the trade-off:\n\
             \x20     {NETWORK_CAPABLE_SHELL_TRADE_OFF}",
            std::env::consts::OS,
        ));
    }
    match state.decision {
        SandboxFirstRunDecision::NetworkCapableUserGranted => Some(format!(
            "sandbox first-run choice: the operator GRANTED network-capable shells \
             (recorded at {}); {NETWORK_CAPABLE_SHELL_TRADE_OFF}",
            decision_path(data_dir).display(),
        )),
        SandboxFirstRunDecision::ExplicitConfig => Some(
            "sandbox shell contract comes from the explicit [sandbox] shell config key \
             (config wins over any durable first-run record)"
                .to_string(),
        ),
        SandboxFirstRunDecision::FailClosed => state.corrupt_reason.as_ref().map(|reason| {
            format!(
                "sandbox first-run decision is UNREADABLE ({reason}); the fail-closed default \
                 remains in force — fix or remove the decision file"
            )
        }),
        // Unreachable: `pending()` returned above; kept total for the type.
        SandboxFirstRunDecision::Pending => None,
    }
}

/// Present the startup notice on stderr and in the log (never stdout: the
/// daemon's stdout carries the frozen startup line). A no-op when there is
/// nothing to present.
pub fn announce_startup(data_dir: &Path, config: &crate::config::Config) {
    let state = inspect(data_dir, config);
    if let Some(text) = startup_notice(&state, data_dir) {
        match state.decision {
            SandboxFirstRunDecision::NetworkCapableUserGranted => tracing::info!("{text}"),
            SandboxFirstRunDecision::ExplicitConfig => tracing::debug!("{text}"),
            _ => tracing::warn!("{text}"),
        }
        eprintln!("faktor sandbox: {text}");
    }
}
