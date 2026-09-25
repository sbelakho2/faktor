//! Lazy instructions + skills (audit: import rules WITHOUT dumping every
//! rule into every prompt; conditional activation; instruction-epoch
//! detection; skills that cost zero prompt tokens until loaded).
//!
//! Discovery is cheap and pure; only `active_for` produces output and only
//! `load_skill` reads full skill bodies.
//!
//! Per-workspace resolution (P0-32): [`InstructionResolver`] turns a durable
//! workspace id into the loaded instruction set of that workspace's root —
//! the root ALWAYS comes from a [`WorkspaceRootProvider`] (the daemon
//! session store), never from the process CWD and never from a static
//! config default. File hashes and instruction epochs (P0-33) are BLAKE3
//! digests, durable across processes and Rust versions — never the
//! unspecified `DefaultHasher`. Authority rule files beyond
//! [`MAX_RULE_BYTES`] (P0-34) are a loud typed error, never a silent
//! truncation; lower-priority optional imports that exceed the cap are
//! skipped whole with a surfaced [`RuleSkip`] entry — no partial text ever
//! reaches a prompt.
//!
//! # Rooted discovery (workspace-escape containment)
//!
//! Rule discovery and rule reads share ONE admitted [`RootedDir`] of the
//! rule root: every candidate is classified through no-follow metadata
//! (`entry_meta`/directory listings), every read goes through
//! [`RootedDir::read`], and the discovery output is workspace-RELATIVE —
//! an absolute path is never queued for a later pathname open. A
//! symlink/reparse point in an authority slot (top-level AGENTS.md /
//! FAKTOR.md / CLAUDE.md) is a loud [`RulesLoadError::Unreadable`]; a link
//! anywhere else (an optional import or a convention directory) is a
//! surfaced whole-entry [`RuleSkip`] and is NEVER followed, so an
//! adversarial checkout can never ingest model instructions from outside
//! the workspace.
//!
//! Skills follow the SAME discipline: [`SkillRegistry::discover`] and every
//! on-demand [`SkillRegistry::load_skill`] body read share ONE admitted
//! [`RootedDir`] of the workspace root, every [`SkillMeta::path`] is
//! workspace-relative (no absolute path is ever stored or reopened), and
//! candidates are classified with no-follow metadata. Skills are optional
//! best-effort payloads, so — exactly like optional rule imports and unlike
//! the authority rule files — a symlink/reparse point, special file, or
//! unreadable skill entry is skipped WHOLE and surfaced as a [`SkillSkip`]
//! (never followed, never partially ingested); `load_skill` answers `None`
//! for it.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use faktor_core::error::ErrorKind;
use faktor_fs::rooted::{RootedDir, RootedEntryKind};
use faktor_fs::ContentDigest;

/// Deterministic precedence (higher wins when both are active). Faktor
/// native rules outrank every imported convention.
pub const PRECEDENCE: [(RuleSourceKind, u8); 9] = [
    (RuleSourceKind::FaktorNative, 100),
    (RuleSourceKind::AgentsMd, 90),
    (RuleSourceKind::ClaudeMd, 80),
    (RuleSourceKind::GeminiMd, 70),
    (RuleSourceKind::CopilotInstructions, 60),
    (RuleSourceKind::CursorRules, 50),
    (RuleSourceKind::WindsurfRules, 40),
    (RuleSourceKind::ContinueRules, 30),
    (RuleSourceKind::LegacyRules, 10),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleSourceKind {
    AgentsMd,
    ClaudeMd,
    GeminiMd,
    CopilotInstructions,
    CursorRules,
    WindsurfRules,
    ContinueRules,
    FaktorNative,
    LegacyRules,
}

impl RuleSourceKind {
    pub fn priority(self) -> u8 {
        PRECEDENCE
            .iter()
            .find(|(k, _)| *k == self)
            .map(|(_, p)| *p)
            .unwrap_or(0)
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

fn hex32(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn dehex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in s.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let hi = HEX.iter().position(|c| *c == pair[0])? as u8;
        let lo = HEX.iter().position(|c| *c == pair[1])? as u8;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

macro_rules! digest_newtype {
    ($name:ident, $doc:expr) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name([u8; 32]);

        impl $name {
            #[inline]
            pub fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }

            /// Little-endian projection of the first 8 digest bytes. Used
            /// ONLY at seams whose durable row shapes predate the 256-bit
            /// types (env-snapshot rows and env-content keys are `u64`
            /// columns today); full-strength equality comparisons always
            /// use the 256-bit value.
            #[inline]
            pub fn as_u64(self) -> u64 {
                u64::from_le_bytes(self.0[..8].try_into().expect("8 bytes"))
            }
        }

        impl From<blake3::Hash> for $name {
            fn from(h: blake3::Hash) -> Self {
                Self(*h.as_bytes())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", hex32(&self.0))
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&hex32(&self.0))
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let raw = <String as serde::Deserialize>::deserialize(d)?;
                dehex32(&raw)
                    .map(Self)
                    .ok_or_else(|| serde::de::Error::custom("expected 64 lowercase hex chars"))
            }
        }
    };
}

digest_newtype!(InstructionHash, "BLAKE3-256 digest of one (rule path, rule content) pair — the durable file identity of a loaded rule.");
digest_newtype!(InstructionEpoch, "BLAKE3-256 digest over the ordered (path, hash) pairs of a loaded rule tree — the durable instruction epoch.");

/// Typed load failures of the rule loader (P0-34). Authority rule files
/// (top-level AGENTS.md / FAKTOR.md / CLAUDE.md) beyond
/// [`MAX_RULE_BYTES`] — or unreadable — are a LOUD error; the loader never
/// half-reads an authority file into a prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RulesLoadError {
    Oversized(String),
    Unreadable(String),
    EpochMismatch {
        expected: InstructionEpoch,
        actual: InstructionEpoch,
    },
}

impl fmt::Display for RulesLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oversized(m) | Self::Unreadable(m) => write!(f, "{m}"),
            Self::EpochMismatch { expected, actual } => write!(
                f,
                "live instruction epoch {actual} differs from the required epoch {expected}; the pinned snapshot must be read, never the live env"
            ),
        }
    }
}

impl std::error::Error for RulesLoadError {}

/// A surfaced skip of one rule file that exceeded a bound or could not be
/// read: the file NEVER contributes partial text to the loaded set — it is
/// either loaded whole or absent whole, and its absence is recorded here.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RuleSkip {
    /// Workspace-relative rule path.
    pub path: String,
    /// On-disk size when known (oversized files); `None` for unreadable ones.
    pub bytes: Option<u64>,
    /// Human reason: `oversized: <bytes> bytes > <cap>` or `unreadable: <err>`.
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Instruction {
    pub source: RuleSourceKind,
    pub path: String,
    pub scope: String,
    pub content: String,
    pub hash: InstructionHash,
    pub priority: u8,
    pub reason_loaded: String,
}

pub const MAX_RULE_BYTES: usize = 64 * 1024;
const MAX_WALK_DEPTH: usize = 16;

/// The convention directory (relative to root) each kind lives under.
fn convention_root_for(kind: RuleSourceKind) -> &'static str {
    match kind {
        RuleSourceKind::CursorRules => ".cursor/rules",
        RuleSourceKind::WindsurfRules => ".windsurf/rules",
        RuleSourceKind::ContinueRules => ".continue/rules",
        RuleSourceKind::FaktorNative => ".faktor/rules",
        RuleSourceKind::LegacyRules => ".faktor/legacy",
        _ => "",
    }
}

/// `/`-normalized lossy string of `path`. The durable string forms of this
/// crate (instruction paths, scopes, snapshot keys, skip entries) are
/// `/`-separated on every platform, so Windows' native `\` separators are
/// folded to `/` at every string boundary. On Unix this is the identity
/// (`\` can legally occur inside a file name there and is never a
/// separator).
fn path_str(path: &Path) -> String {
    let s = path.to_string_lossy();
    if cfg!(windows) {
        s.replace('\\', "/")
    } else {
        s.into_owned()
    }
}

/// Deterministic BLAKE3 digest of (path, content) — the durable file
/// identity (P0-33). Stable across processes and Rust versions, and
/// separator-agnostic: the path bytes go through [`path_str`], so the same
/// tree hashes identically whether a path was built with `/` or the native
/// separator (a Windows walk mixes both).
fn hash_of(path: &Path, content: &str) -> InstructionHash {
    let mut h = blake3::Hasher::new();
    h.update(path_str(path).as_bytes());
    h.update(b"\0");
    h.update(content.as_bytes());
    InstructionHash::from(h.finalize())
}

/// Content-only digest projection (the `bytes_hash` of env records).
fn content_hash_proj(content: &[u8]) -> u64 {
    blake3::hash(content).as_bytes()[..8]
        .try_into()
        .map(u64::from_le_bytes)
        .expect("8 bytes")
}

/// Bounded read outcome of ONE rule file. Oversized is decided by the
/// at-open `fstat` size: the rooted read either returns the whole file
/// ([`ContentDigest::Full`]) or exactly `cap` bytes
/// ([`ContentDigest::Slice`]), so a hostile multi-GiB file never enters RAM
/// — and is reported with its size, never truncated.
enum RuleFileRead {
    Missing,
    Unreadable(String),
    Oversized(u64),
    Full(String),
}

/// Test seam: fired with a rule's workspace-relative path immediately AFTER
/// discovery enumerated it and BEFORE its rooted no-follow read, so
/// adversarial tests can swap the entry in exactly the enumeration -> read
/// window. Test-only; production builds compile a no-op.
#[cfg(test)]
type ReadSeam = Box<dyn Fn(&Path) + Send>;
#[cfg(test)]
static READ_SEAM: Mutex<Option<ReadSeam>> = Mutex::new(None);
/// Serializes tests that install the process-global read seam.
#[cfg(test)]
static READ_SEAM_LOCK: Mutex<()> = Mutex::new(());
#[cfg(test)]
fn read_seam(rel: &Path) {
    if let Some(hook) = READ_SEAM
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
    {
        hook(rel);
    }
}
#[cfg(not(test))]
fn read_seam(_rel: &Path) {}

/// Read one rule file through the admitted rooted authority: the parent
/// chain and the final entry are resolved component-by-component with
/// no-follow semantics, so a symlink/reparse point on any component is
/// refused typed and never followed outside the root. `cap` is
/// [`MAX_RULE_BYTES`]; the read is bounded by the at-open `fstat` size, so
/// an oversized file costs at most `cap` bytes of RAM and the surfaced size
/// comes from no-follow metadata.
fn read_rule_file(rooted: &RootedDir, rel: &Path, cap: usize) -> RuleFileRead {
    read_seam(rel);
    let meta = match rooted.entry_meta(rel) {
        Ok(Some(meta)) => meta,
        Ok(None) => return RuleFileRead::Missing,
        Err(e) if e.kind == ErrorKind::NotFound => return RuleFileRead::Missing,
        Err(e) => return RuleFileRead::Unreadable(e.to_string()),
    };
    match meta.kind {
        RootedEntryKind::File => {}
        RootedEntryKind::Symlink => {
            return RuleFileRead::Unreadable(format!(
                "unsafe entry: {} is a symlink/reparse point; the no-follow rooted read refuses to follow it",
                rel.display()
            ))
        }
        RootedEntryKind::Directory | RootedEntryKind::Other => {
            return RuleFileRead::Unreadable(format!(
                "unsafe entry: {} is not a regular file; refusing to open it",
                rel.display()
            ))
        }
    }
    match rooted.read(rel, cap) {
        Ok(data) => match data.digest {
            ContentDigest::Full(_) => {
                RuleFileRead::Full(String::from_utf8_lossy(&data.bytes).into_owned())
            }
            // The read stopped exactly at the cap: the on-disk size from the
            // no-follow metadata is the honest number a skip/error carries.
            ContentDigest::Slice { .. } => RuleFileRead::Oversized(meta.size.max(data.size as u64)),
        },
        Err(e) if e.kind == ErrorKind::NotFound => RuleFileRead::Missing,
        Err(e) => RuleFileRead::Unreadable(e.to_string()),
    }
}

/// Authority rule files: top-level AGENTS.md / FAKTOR.md / CLAUDE.md at the
/// repo root (P0-34). These feed the prompt unconditionally or near it, so
/// an oversized, unreadable, or unsafe (link/special-file) one is a loud
/// error — never silently omitted or half-read. Everything else
/// (scoped/directory rules, GEMINI.md, copilot instructions, legacy
/// imports, ...) is a lower-priority import: oversize/unreadable/unsafe
/// entries are skipped WHOLE with a surfaced entry.
fn is_authority_rel_path(rel: &str) -> bool {
    matches!(rel, "AGENTS.md" | "FAKTOR.md" | "CLAUDE.md")
}

/// Bound on the entries ONE rule-directory listing may return; a larger
/// directory surfaces a whole-directory skip (its returned prefix is honest
/// but incomplete) instead of a silent truncation.
const MAX_RULE_DIR_ENTRIES: usize = 16_384;

/// One regular rule file found by rooted discovery; `rel` is
/// workspace-relative and `/`-normalized on every platform.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DiscoveredRuleFile {
    kind: RuleSourceKind,
    rel: String,
}

/// One rooted discovery pass: the regular rule files plus the whole-entry
/// refusals (links, special files, unlistable directories) surfaced as
/// [`RuleSkip`] rows — unsafe entries are never followed and never become
/// rules.
struct RuleDiscovery {
    files: Vec<DiscoveredRuleFile>,
    skips: Vec<RuleSkip>,
}

fn unsafe_entry_skip(rel: &str, detail: String) -> RuleSkip {
    RuleSkip {
        path: rel.to_string(),
        bytes: None,
        reason: format!("unsafe entry: {detail}"),
    }
}

fn is_rule_extension(name: &std::ffi::OsStr) -> bool {
    Path::new(name)
        .extension()
        .map(|x| x == "md" || x == "mdc")
        .unwrap_or(false)
}

/// Discovery over the admitted [`RootedDir`]: every candidate is classified
/// through no-follow metadata, every returned path is workspace-relative
/// (an absolute path is never queued for a later pathname open), and a
/// symlink/reparse point or special file is surfaced as a [`RuleSkip`] and
/// NEVER followed or descended into.
fn discover_rooted(rooted: &RootedDir) -> RuleDiscovery {
    let mut files = Vec::new();
    let mut skips = Vec::new();

    // Top-level well-known rule files, classified by no-follow metadata.
    for (kind, names) in [
        (RuleSourceKind::FaktorNative, &["FAKTOR.md"][..]),
        (RuleSourceKind::AgentsMd, &["AGENTS.md"][..]),
        (RuleSourceKind::ClaudeMd, &["CLAUDE.md"][..]),
        (RuleSourceKind::GeminiMd, &["GEMINI.md"][..]),
        (
            RuleSourceKind::CopilotInstructions,
            &[".github/copilot-instructions.md"][..],
        ),
        (
            RuleSourceKind::WindsurfRules,
            &[".windsurfrules", ".windsurf/rules.md"][..],
        ),
    ] {
        for n in names {
            match rooted.entry_meta(Path::new(n)) {
                Ok(Some(meta)) if meta.kind == RootedEntryKind::File => {
                    files.push(DiscoveredRuleFile {
                        kind,
                        rel: (*n).to_string(),
                    })
                }
                Ok(Some(meta)) => skips.push(unsafe_entry_skip(
                    n,
                    format!(
                        "{n} is not a regular file ({:?}); refusing to follow it",
                        meta.kind
                    ),
                )),
                Ok(None) => {}
                Err(e) => skips.push(RuleSkip {
                    path: (*n).to_string(),
                    bytes: None,
                    reason: format!("unreadable: {e}"),
                }),
            }
        }
    }

    // Directory rule globs: every `*.md`/`*.mdc` under the dir, with the
    // dir path as its activation scope. Depth is tracked PER DIRECTORY as
    // (dir, depth-below-root) pairs (audit 32): a hostile deep branch cuts
    // itself off at MAX_WALK_DEPTH without consuming the allowance of
    // sibling branches — a shallow file next to a 30-deep tree is still
    // discovered.
    for (kind, dir_rel) in [
        (RuleSourceKind::CursorRules, ".cursor/rules"),
        (RuleSourceKind::WindsurfRules, ".windsurf/rules"),
        (RuleSourceKind::ContinueRules, ".continue/rules"),
        (RuleSourceKind::FaktorNative, ".faktor/rules"),
        (RuleSourceKind::LegacyRules, ".faktor/legacy"),
    ] {
        let dir_path = Path::new(dir_rel);
        match rooted.entry_meta(dir_path) {
            Ok(None) => continue,
            Ok(Some(meta)) if meta.kind == RootedEntryKind::Directory => {}
            Ok(Some(meta)) => {
                skips.push(unsafe_entry_skip(
                    dir_rel,
                    format!(
                        "{dir_rel} is not a real directory ({:?}); refusing to enter it",
                        meta.kind
                    ),
                ));
                continue;
            }
            Err(e) => {
                skips.push(RuleSkip {
                    path: dir_rel.to_string(),
                    bytes: None,
                    reason: format!("unreadable: {e}"),
                });
                continue;
            }
        }
        let mut walk: VecDeque<(PathBuf, usize)> =
            VecDeque::from([(dir_path.to_path_buf(), 0usize)]);
        while let Some((d, depth)) = walk.pop_front() {
            if depth > MAX_WALK_DEPTH {
                continue;
            }
            let listing = match rooted.list_entries(&d, MAX_RULE_DIR_ENTRIES) {
                Ok(listing) => listing,
                Err(e) if e.kind == ErrorKind::NotFound => continue,
                Err(e) => {
                    skips.push(RuleSkip {
                        path: path_str(&d),
                        bytes: None,
                        reason: format!("unreadable: {e}"),
                    });
                    continue;
                }
            };
            if listing.overflowed {
                skips.push(RuleSkip {
                    path: path_str(&d),
                    bytes: None,
                    reason: format!(
                        "unreadable: directory holds more than {MAX_RULE_DIR_ENTRIES} entries; its listing is refused as incomplete"
                    ),
                });
            }
            for entry in listing.entries {
                match entry.kind {
                    RootedEntryKind::Directory => walk.push_back((entry.rel, depth + 1)),
                    RootedEntryKind::File => {
                        if is_rule_extension(&entry.name) {
                            files.push(DiscoveredRuleFile {
                                kind,
                                rel: path_str(&entry.rel),
                            });
                        }
                    }
                    RootedEntryKind::Symlink => skips.push(unsafe_entry_skip(
                        &path_str(&entry.rel),
                        "symlink/reparse point under a rule directory; never followed".to_string(),
                    )),
                    RootedEntryKind::Other => skips.push(unsafe_entry_skip(
                        &path_str(&entry.rel),
                        "special file under a rule directory; never opened".to_string(),
                    )),
                }
            }
        }
    }

    // Deterministic: by kind priority desc, then workspace-relative path
    // asc (the epoch input order of a live load).
    files.sort_by(|a, b| {
        b.kind
            .priority()
            .cmp(&a.kind.priority())
            .then_with(|| a.rel.cmp(&b.rel))
    });
    RuleDiscovery { files, skips }
}

/// Every rule file discoverable under `root` (bounded, deterministic).
///
/// Discovery goes through the [`RootedDir`] authority of `root`: returned
/// paths are workspace-RELATIVE (never absolute and never queued for a
/// later pathname open), every candidate is classified with no-follow
/// metadata, and symlinks/reparse points or special files are never
/// returned as rule files. An absent or unopenable root yields no files.
pub fn discover_rule_files(root: &Path) -> Vec<(RuleSourceKind, PathBuf)> {
    let Ok(rooted) = RootedDir::open(root) else {
        return Vec::new();
    };
    discover_rooted(&rooted)
        .files
        .into_iter()
        .map(|f| (f.kind, PathBuf::from(f.rel)))
        .collect()
}

/// Loaded rule tree. `active_for` returns only rules whose scope/keywords
/// match the prompt or the touched files.
#[derive(Debug)]
pub struct Instructions {
    rules: Vec<Instruction>,
    skipped: Vec<RuleSkip>,
    epoch: InstructionEpoch,
    root: PathBuf,
}

impl Instructions {
    /// Load the LIVE rule tree of `root` (P0-34) through ONE admitted
    /// [`RootedDir`]: discovery and every read are handle-relative with
    /// no-follow semantics, so a symlink/reparse point can never redirect
    /// a rule read outside the root. An authority file
    /// (top-level AGENTS.md / FAKTOR.md / CLAUDE.md) beyond
    /// [`MAX_RULE_BYTES`], unreadable, or an unsafe entry (link/special
    /// file) is a typed error, NEVER a silent truncation or omission.
    /// Optional oversized/unsafe imports are skipped whole and surfaced
    /// via [`Instructions::skipped`].
    pub fn load(root: &Path) -> Result<Self, RulesLoadError> {
        let (rules, skipped) = load_rules(root)?;
        let epoch = Self::compute_epoch(&rules);
        Ok(Self {
            rules,
            skipped,
            epoch,
            root: root.to_path_buf(),
        })
    }

    /// Strict epoch-pinned load: loads the LIVE tree and refuses — with a
    /// typed [`RulesLoadError::EpochMismatch`] — when the live
    /// instruction epoch differs from the pinned `expected_epoch`. There is
    /// never a silent drift: a caller that requires the environment of a
    /// snapshot must read the snapshot (audit 97), never fall back to
    /// whatever the filesystem holds now.
    pub fn load_at_epoch(
        root: &Path,
        expected_epoch: InstructionEpoch,
    ) -> Result<Self, RulesLoadError> {
        let live = Self::load(root)?;
        if live.epoch == expected_epoch {
            Ok(live)
        } else {
            Err(RulesLoadError::EpochMismatch {
                expected: expected_epoch,
                actual: live.epoch,
            })
        }
    }

    /// Build an epoch-pinned instruction tree from one immutable
    /// [`EnvSnapshot`] and its captured file contents. Reads ONLY the
    /// snapshot: the live filesystem is never consulted, so rules the
    /// parent changed after the snapshot was taken can never bleed into a
    /// child bound to it. Every entry is verified against the snapshot's
    /// recorded hashes (missing content or tampered bytes are loud typed
    /// errors, never a silent skip or a live fallback). The epoch is
    /// recomputed over the re-verified (path, hash) pairs in discovery
    /// order, so it equals the digest a live load of the same tree
    /// computes.
    pub fn from_snapshot(
        snap: &EnvSnapshot,
        content: &BTreeMap<String, String>,
    ) -> Result<Self, EnvSnapshotError> {
        // Structural validation FIRST: every recorded path must be a rule
        // file a discovery pass could produce (hostile rows are malformed
        // no matter what their hashes claim), then the content verification.
        for rel in snap.workspace_paths.keys() {
            let rel_path = Path::new(rel);
            kind_for_rel_path(rel_path).ok_or_else(|| {
                EnvSnapshotError::Malformed(format!(
                    "snapshot path {rel:?} is not a rule file any discovery pass could produce"
                ))
            })?;
        }
        verify_snapshot_content(snap, content)?;
        let root = snap.root.clone();
        let mut rules = Vec::new();
        for rel in snap.workspace_paths.keys() {
            let rel_path = Path::new(rel);
            let kind = kind_for_rel_path(rel_path).expect("validated above");
            let Some(text) = content.get(rel) else {
                return Err(EnvSnapshotError::Missing(format!(
                    "snapshot content for {rel:?}"
                )));
            };
            let abs = root.join(rel_path);
            rules.push(Instruction {
                source: kind,
                path: rel.clone(),
                scope: scope_of(&root, kind, &abs),
                content: text.clone(),
                hash: hash_of(&abs, text),
                priority: kind.priority(),
                reason_loaded: String::new(),
            });
        }
        // Discovery order (kind priority desc, path asc) is the epoch
        // input order of a live load of the same tree.
        rules.sort_by(|a, b| {
            b.source
                .priority()
                .cmp(&a.source.priority())
                .then_with(|| a.path.cmp(&b.path))
        });
        let epoch = Self::compute_epoch(&rules);
        Ok(Self {
            rules,
            skipped: snap.skipped.clone(),
            epoch,
            root,
        })
    }

    /// BLAKE3 over the ordered (path, hash) pairs — durable, deterministic.
    fn compute_epoch(rules: &[Instruction]) -> InstructionEpoch {
        let mut h = blake3::Hasher::new();
        for r in rules {
            h.update(r.path.as_bytes());
            h.update(r.hash.as_bytes());
        }
        InstructionEpoch::from(h.finalize())
    }

    /// True when any rule file changed since load (instruction epoch —
    /// stale rules must never silently govern a long task). A tree that
    /// became unloadable (hostile oversized authority file) is a typed
    /// error — never a silent "unchanged".
    pub fn reload_if_changed(&mut self) -> Result<bool, RulesLoadError> {
        let fresh = Self::load(&self.root)?;
        if fresh.epoch != self.epoch {
            *self = fresh;
            return Ok(true);
        }
        Ok(false)
    }

    pub fn epoch(&self) -> InstructionEpoch {
        self.epoch
    }

    /// Surfaced whole-file skips (P0-34): optional rule files that were
    /// never read (oversized or unreadable) are listed here with their size
    /// and reason — they are never half-loaded.
    pub fn skipped(&self) -> &[RuleSkip] {
        &self.skipped
    }

    /// True when the tree holds at least one rule file. An empty tree's
    /// epoch is a vacuous stamp; epoch-pinning callers may treat an empty
    /// tree like "no rules loaded".
    pub fn has_rules(&self) -> bool {
        !self.rules.is_empty()
    }

    /// Conditional activation: Faktor-native top-level + AGENTS.md always
    /// load; scoped rules activate when the prompt or a touched file
    /// matches their scope path or a `# Scope:` keyword directive.
    pub fn active_for(&self, prompt: &str, touched: &[String]) -> Vec<Instruction> {
        let mut out = Vec::new();
        for r in &self.rules {
            let top = r.scope.is_empty();
            let mut reason = None;
            if top
                && matches!(
                    r.source,
                    RuleSourceKind::FaktorNative | RuleSourceKind::AgentsMd
                )
            {
                reason = Some("always".to_string());
            } else {
                // Directory containment of a touched file activates
                // (rule's own dir path as scope), separator-agnostically.
                if touched.iter().any(|t| touched_in_scope(t, &r.scope)) {
                    reason = Some(format!("scope:{}", r.scope));
                }
            }
            if reason.is_none() {
                // Keyword directives: leading "# Scope: a,b" lines.
                for line in r.content.lines().take(8) {
                    if let Some(kws) = line.trim_start().strip_prefix("# Scope:") {
                        for kw in kws.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
                            if prompt.to_lowercase().contains(&kw.to_lowercase()) {
                                reason = Some(format!("keyword:{kw}"));
                                break;
                            }
                        }
                    }
                    if reason.is_some() {
                        break;
                    }
                }
            }
            if let Some(reason_loaded) = reason {
                let mut i = r.clone();
                i.reason_loaded = reason_loaded;
                out.push(i);
            }
        }
        out
    }
}

// -------------------------------------------------------- env snapshots (audit 97)

/// Hard cap on the number of rule files one env snapshot may capture. A
/// discovery that would exceed it is a typed [`EnvSnapshotError::Oversized`]
/// — snapshots never silently drop files (bounded everything).
pub const MAX_SNAPSHOT_PATHS: usize = 64;
/// Hard cap on the TOTAL captured rule bytes of one snapshot (per-file
/// reads are already bounded by [`MAX_RULE_BYTES`]). Exceeding it is a
/// typed [`EnvSnapshotError::Oversized`], never a silent truncation.
pub const MAX_SNAPSHOT_TOTAL_BYTES: usize = 512 * 1024;
/// Hard cap on one workspace-relative rule path inside a snapshot.
pub const MAX_SNAPSHOT_PATH_CHARS: usize = 1024;
/// Hard cap on one snapshot id (chars).
pub const MAX_SNAPSHOT_ID_CHARS: usize = 128;

/// Typed failures of the snapshot machinery (audit 97). Every variant is a
/// LOUD refusal — there is never a silent fallback to the live environment.
///
/// `EpochMismatch` keeps its historic `u64` payloads: orchestrator code
/// (outside this crate) pattern-matches this enum exhaustively and formats
/// the pair. New epoch-pinned loaders use
/// [`RulesLoadError::EpochMismatch`] with the 256-bit [`InstructionEpoch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvSnapshotError {
    Oversized(String),
    Malformed(String),
    Missing(String),
    Tampered(String),
    EpochMismatch { expected: u64, actual: u64 },
}

impl fmt::Display for EnvSnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oversized(m)
            | Self::Malformed(m)
            | Self::Missing(m)
            | Self::Tampered(m) => write!(f, "{m}"),
            Self::EpochMismatch { expected, actual } => write!(
                f,
                "live instruction epoch {actual} differs from the required epoch {expected}; the pinned snapshot must be read, never the live env"
            ),
        }
    }
}

impl std::error::Error for EnvSnapshotError {}

/// One captured rule file. `rules_hash` is the 64-bit little-endian
/// projection of the BLAKE3 digest of (path, content) — the SAME digest
/// [`Instruction::hash`] carries — and `bytes_hash` the projection of the
/// content-only BLAKE3 digest. Both projections exist because the durable
/// env rows and env-content keys this crate feeds (orchestrator side) are
/// `u64`-shaped today; the projections are stable across processes and Rust
/// versions (P0-33).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EnvFileRecord {
    pub rules_hash: u64,
    pub bytes_hash: u64,
}

/// An IMMUTABLE, epoch-pinned capture of a directory's rule environment
/// (repo knowledge / AGENTS.md / instructions), taken once at child spawn.
/// A child bound to it reads ONLY these rules for the rest of its life —
/// later changes to the parent's environment never bleed into it. Paths are
/// workspace-relative (lossy strings, map keys) so the snapshot is portable
/// across roots; `root` is the capture root used to recompute the recorded
/// path-content hashes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EnvSnapshot {
    pub snapshot_id: String,
    /// 64-bit projection of the BLAKE3 instruction-epoch digest over the
    /// ordered (path, rules_hash) pairs — equal to
    /// `Instructions::load(root).epoch().as_u64()` over an unchanged tree.
    pub instruction_epoch: u64,
    /// The root directory the snapshot was captured from.
    pub root: PathBuf,
    /// Workspace-relative rule path -> recorded hashes, sorted by path.
    pub workspace_paths: BTreeMap<String, EnvFileRecord>,
    /// Surfaced whole-file skips during capture (optional oversized /
    /// unreadable rule files; never partial text).
    #[serde(default)]
    pub skipped: Vec<RuleSkip>,
    pub taken_ms: i64,
}

impl EnvSnapshot {
    /// Capture the rule environment of `root` exactly as
    /// [`Instructions::load`] would see it (same discovery, same per-file
    /// read bound, same ordering — so the snapshot epoch matches a live
    /// load of an unchanged tree). Bounded: at most [`MAX_SNAPSHOT_PATHS`]
    /// rule files and at most [`MAX_SNAPSHOT_TOTAL_BYTES`] total captured
    /// bytes; beyond either the capture fails with a typed Oversized error
    /// and returns NOTHING (never a silently truncated snapshot). An
    /// authority rule file (top-level AGENTS.md / FAKTOR.md / CLAUDE.md)
    /// beyond [`MAX_RULE_BYTES`] also fails loudly, and a link/special-file
    /// in an authority slot is refused too; an oversized optional import is
    /// skipped whole and surfaced in `snapshot.skipped`. Returns the
    /// snapshot plus the captured file contents keyed by the same
    /// workspace-relative paths (contents are what a pinned read serves;
    /// they are stored separately so identical content is stored once).
    ///
    /// Discovery and every read share ONE [`RootedDir`] of `root`: no
    /// absolute pathname is reopened, and no symlink/reparse point is ever
    /// followed, so a hostile checkout cannot capture instructions from
    /// outside the workspace.
    pub fn capture(
        root: &Path,
        snapshot_id: &str,
        taken_ms: i64,
    ) -> Result<CapturedEnv, EnvSnapshotError> {
        let rooted = match RootedDir::open(root) {
            Ok(rooted) => rooted,
            Err(e) => {
                return Err(EnvSnapshotError::Malformed(format!(
                    "snapshot root {:?} is not an openable directory ({e})",
                    root
                )))
            }
        };
        if snapshot_id.is_empty()
            || snapshot_id.chars().count() > MAX_SNAPSHOT_ID_CHARS
            || !snapshot_id.is_ascii()
            || snapshot_id.contains('/')
            || snapshot_id.contains('\\')
        {
            return Err(EnvSnapshotError::Malformed(format!(
                "snapshot id {snapshot_id:?} must be 1..={MAX_SNAPSHOT_ID_CHARS} ASCII characters without '/' or '\\'"
            )));
        }
        let discovery = discover_rooted(&rooted);
        let mut workspace_paths = BTreeMap::new();
        let mut content = BTreeMap::new();
        let mut skipped = Vec::new();
        let mut total_bytes = 0usize;
        let mut epoch = blake3::Hasher::new();
        for skip in discovery.skips {
            if is_authority_rel_path(&skip.path) {
                return Err(EnvSnapshotError::Malformed(format!(
                    "authority rule file {} is an unsafe/unreadable entry ({}); refusing a capture that silently omits or follows it",
                    skip.path, skip.reason
                )));
            }
            skipped.push(skip);
        }
        for cand in discovery.files {
            let rel_str = cand.rel;
            let rel_path = Path::new(&rel_str);
            if rel_str.chars().count() > MAX_SNAPSHOT_PATH_CHARS {
                return Err(EnvSnapshotError::Oversized(format!(
                    "rule path {rel_str:?} exceeds {MAX_SNAPSHOT_PATH_CHARS} characters"
                )));
            }
            if workspace_paths.len() >= MAX_SNAPSHOT_PATHS {
                return Err(EnvSnapshotError::Oversized(format!(
                    "rule environment of {:?} has more than {MAX_SNAPSHOT_PATHS} rule files",
                    root
                )));
            }
            let authority = is_authority_rel_path(&rel_str);
            // The absolute join is the historic hash input (and is never
            // opened): the read itself goes through `rooted`.
            let hash_path = root.join(rel_path);
            match read_rule_file(&rooted, rel_path, MAX_RULE_BYTES) {
                RuleFileRead::Missing => {}
                RuleFileRead::Unreadable(err) if authority => {
                    return Err(EnvSnapshotError::Malformed(format!(
                        "authority rule file {rel_str} is unreadable ({err}); refusing a capture that silently omits it"
                    )));
                }
                RuleFileRead::Oversized(len) if authority => {
                    return Err(EnvSnapshotError::Oversized(format!(
                        "authority rule file {rel_str} is {len} bytes — over the {MAX_RULE_BYTES} rule bound; refusing a capture that half-reads it"
                    )));
                }
                RuleFileRead::Unreadable(err) => {
                    skipped.push(RuleSkip {
                        path: rel_str.clone(),
                        bytes: None,
                        reason: format!("unreadable: {err}"),
                    });
                }
                RuleFileRead::Oversized(len) => {
                    skipped.push(RuleSkip {
                        path: rel_str.clone(),
                        bytes: Some(len),
                        reason: format!("oversized: {len} bytes > {MAX_RULE_BYTES}"),
                    });
                }
                RuleFileRead::Full(text) => {
                    if total_bytes.saturating_add(text.len()) > MAX_SNAPSHOT_TOTAL_BYTES {
                        return Err(EnvSnapshotError::Oversized(format!(
                            "rule environment of {:?} exceeds {MAX_SNAPSHOT_TOTAL_BYTES} total bytes",
                            root
                        )));
                    }
                    total_bytes += text.len();
                    let rules_hash = hash_of(&hash_path, &text);
                    // instruction_epoch must digest the same (rel path,
                    // 32-byte rules hash) pairs in the same order as
                    // compute_epoch over loaded rules.
                    epoch.update(rel_str.as_bytes());
                    epoch.update(rules_hash.as_bytes());
                    workspace_paths.insert(
                        rel_str.clone(),
                        EnvFileRecord {
                            rules_hash: rules_hash.as_u64(),
                            bytes_hash: content_hash_proj(text.as_bytes()),
                        },
                    );
                    content.insert(rel_str, text);
                }
            }
        }
        let digest = epoch.finalize();
        Ok(CapturedEnv {
            snapshot: EnvSnapshot {
                snapshot_id: snapshot_id.to_string(),
                instruction_epoch: InstructionEpoch::from(digest).as_u64(),
                root: root.to_path_buf(),
                workspace_paths,
                skipped,
                taken_ms,
            },
            content,
        })
    }
}

/// A captured snapshot plus the file contents needed to serve a pinned
/// read. Contents are deliberately kept separate from the (small) snapshot
/// so a durable store can deduplicate unchanged content by hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedEnv {
    pub snapshot: EnvSnapshot,
    /// Workspace-relative path -> captured file content.
    pub content: BTreeMap<String, String>,
}

/// Verify that `content` satisfies EVERY entry of the snapshot: every
/// recorded path has content, the content bytes hash to the recorded
/// `bytes_hash`, and (path, content) hashes to the recorded `rules_hash`.
/// A missing or tampered entry is a loud typed error — pinned reads never
/// skip an entry and never fall back to the live filesystem.
pub fn verify_snapshot_content(
    snap: &EnvSnapshot,
    content: &BTreeMap<String, String>,
) -> Result<(), EnvSnapshotError> {
    for (rel, rec) in &snap.workspace_paths {
        let Some(text) = content.get(rel) else {
            return Err(EnvSnapshotError::Missing(format!(
                "content for rule path {rel:?}"
            )));
        };
        if content_hash_proj(text.as_bytes()) != rec.bytes_hash {
            return Err(EnvSnapshotError::Tampered(format!(
                "content bytes of {rel:?} do not match the recorded bytes hash"
            )));
        }
        if hash_of(&snap.root.join(rel), text).as_u64() != rec.rules_hash {
            return Err(EnvSnapshotError::Tampered(format!(
                "content of {rel:?} does not match the recorded (path, content) rules hash"
            )));
        }
    }
    Ok(())
}

/// Open the rule root as ONE admitted [`RootedDir`]. A root that does not
/// exist is `Ok(None)` (the documented empty-tree case); any other open
/// failure is a loud typed refusal, never a silent empty rule set.
fn open_rule_root(root: &Path) -> Result<Option<RootedDir>, RulesLoadError> {
    match RootedDir::open(root) {
        Ok(rooted) => Ok(Some(rooted)),
        Err(e) if e.kind == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(RulesLoadError::Unreadable(format!(
            "rule root {} is not an openable directory ({e}); refusing to guess an empty rule tree",
            root.display()
        ))),
    }
}

/// Load every rule of `root` in discovery order (the epoch input order)
/// through ONE admitted [`RootedDir`]. Authority files that are oversized,
/// unreadable, or unsafe (symlink/reparse point/special file in an
/// authority slot) FAIL the whole load (P0-34); optional imports with the
/// same conditions are skipped whole with a surfaced [`RuleSkip`] — partial
/// text never reaches rules and a link is never followed.
fn load_rules(root: &Path) -> Result<(Vec<Instruction>, Vec<RuleSkip>), RulesLoadError> {
    let Some(rooted) = open_rule_root(root)? else {
        return Ok((Vec::new(), Vec::new()));
    };
    let discovery = discover_rooted(&rooted);
    let mut rules = Vec::new();
    let mut skipped = Vec::new();
    for skip in discovery.skips {
        if is_authority_rel_path(&skip.path) {
            return Err(RulesLoadError::Unreadable(format!(
                "authority rule file {} is an unsafe/unreadable entry ({}); refusing to load rules that silently omit or follow it",
                skip.path, skip.reason
            )));
        }
        skipped.push(skip);
    }
    for cand in discovery.files {
        let rel_path = Path::new(&cand.rel);
        let authority = is_authority_rel_path(&cand.rel);
        // The absolute join is the historic hash/scope input and is NEVER
        // opened: the read itself goes through the shared `rooted`.
        let hash_path = root.join(rel_path);
        match read_rule_file(&rooted, rel_path, MAX_RULE_BYTES) {
            RuleFileRead::Missing => {}
            RuleFileRead::Unreadable(err) if authority => {
                return Err(RulesLoadError::Unreadable(format!(
                    "authority rule file {} could not be read ({err}); refusing to load rules that silently omit it",
                    cand.rel
                )));
            }
            RuleFileRead::Oversized(len) if authority => {
                return Err(RulesLoadError::Oversized(format!(
                    "authority rule file {} is {len} bytes — over the {MAX_RULE_BYTES} rule bound; refusing to half-load it",
                    cand.rel
                )));
            }
            RuleFileRead::Unreadable(err) => {
                skipped.push(RuleSkip {
                    path: cand.rel.clone(),
                    bytes: None,
                    reason: format!("unreadable: {err}"),
                });
            }
            RuleFileRead::Oversized(len) => {
                skipped.push(RuleSkip {
                    path: cand.rel.clone(),
                    bytes: Some(len),
                    reason: format!("oversized: {len} bytes > {MAX_RULE_BYTES}"),
                });
            }
            RuleFileRead::Full(content) => {
                rules.push(Instruction {
                    source: cand.kind,
                    path: cand.rel.clone(),
                    scope: scope_of(root, cand.kind, &hash_path),
                    content: content.clone(),
                    hash: hash_of(&hash_path, &content),
                    priority: cand.kind.priority(),
                    reason_loaded: String::new(),
                });
            }
        }
    }
    Ok((rules, skipped))
}

/// `/`-normalized workspace-relative components of a path-shaped string.
/// BOTH `/` and `\` are separators on every platform: touched files arrive
/// `\`-separated from Windows callers and `/`-separated from snapshots and
/// tests, and only component comparison is safe (`ab` must never match
/// scope `a`). Empty and `.` components are dropped; `None` is returned for
/// absolute forms (`/x`, `\x`, `C:\x`) and any `..` component — such a form
/// can escape the workspace and must never match a scope.
fn rel_components(s: &str) -> Option<Vec<&str>> {
    if s.starts_with('/') || s.starts_with('\\') {
        return None;
    }
    if s.as_bytes().get(1) == Some(&b':') {
        return None;
    }
    let mut out = Vec::new();
    for part in s.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => return None,
            p => out.push(p),
        }
    }
    Some(out)
}

/// `/`-joined form of [`rel_components`].
fn normalized_rel_string(s: &str) -> Option<String> {
    Some(rel_components(s)?.join("/"))
}

/// True when the workspace-relative touched file `touched` is contained in
/// the rule scope directory `scope`. Both operands accept either separator
/// style. Component-wise containment means a prefix-lookalike sibling
/// (`ab` vs scope `a`) never matches, and a traversal (`..`) or absolute
/// form never matches any scope. An empty scope (top-level rule) contains
/// every well-formed touched path — the historic "a root-scoped import is
/// activated by any touched file" behavior.
fn touched_in_scope(touched: &str, scope: &str) -> bool {
    match (rel_components(touched), rel_components(scope)) {
        (Some(t), Some(s)) => t.starts_with(&s),
        _ => false,
    }
}

/// The activation scope of one rule file (workspace-relative subdirectory
/// under its convention root; top-level rules carry an empty scope). The
/// convention root is stripped component-wise via `Path::strip_prefix`, so
/// it matches no matter which separator the walker produced; the result is
/// `/`-normalized like every other durable path string.
fn scope_of(root: &Path, kind: RuleSourceKind, path: &Path) -> String {
    let Some(rel_parent) = path.parent().and_then(|p| p.strip_prefix(root).ok()) else {
        return String::new();
    };
    // Strip the convention root so activation uses the workspace-relative
    // subdir (frontend/App.tsx -> frontend).
    let convention = convention_root_for(kind);
    let stripped = if convention.is_empty() {
        rel_parent
    } else {
        rel_parent.strip_prefix(convention).unwrap_or(rel_parent)
    };
    path_str(stripped)
}

/// The rule kind a workspace-relative path carries — exactly the
/// membership of [`discover_rule_files`]: top-level well-known names plus
/// `*.md`/`*.mdc` files under the convention directories. Comparison is
/// separator-agnostic (both `/` and `\` forms match on every platform).
/// Any other path — including traversal and absolute forms — is `None`: a
/// pinned snapshot entry that maps to `None` is malformed (hostile rows can
/// never become rules).
pub fn kind_for_rel_path(rel: &Path) -> Option<RuleSourceKind> {
    let normalized = normalized_rel_string(&rel.to_string_lossy())?;
    for (kind, names) in [
        (RuleSourceKind::FaktorNative, &["FAKTOR.md"][..]),
        (RuleSourceKind::AgentsMd, &["AGENTS.md"][..]),
        (RuleSourceKind::ClaudeMd, &["CLAUDE.md"][..]),
        (RuleSourceKind::GeminiMd, &["GEMINI.md"][..]),
        (
            RuleSourceKind::CopilotInstructions,
            &[".github/copilot-instructions.md"][..],
        ),
        (
            RuleSourceKind::WindsurfRules,
            &[".windsurfrules", ".windsurf/rules.md"][..],
        ),
    ] {
        for n in names {
            if normalized == *n {
                return Some(kind);
            }
        }
    }
    let last = normalized.rsplit('/').next().unwrap_or("");
    let is_md =
        (last.len() > 3 && last.ends_with(".md")) || (last.len() > 4 && last.ends_with(".mdc"));
    if is_md {
        for (kind, dir) in [
            (RuleSourceKind::CursorRules, ".cursor/rules"),
            (RuleSourceKind::WindsurfRules, ".windsurf/rules"),
            (RuleSourceKind::ContinueRules, ".continue/rules"),
            (RuleSourceKind::FaktorNative, ".faktor/rules"),
            (RuleSourceKind::LegacyRules, ".faktor/legacy"),
        ] {
            if normalized == dir || normalized.starts_with(&format!("{dir}/")) {
                return Some(kind);
            }
        }
    }
    None
}

// ------------------------------------------------- per-workspace resolver (P0-32)

/// The durable workspace-root source of the resolver. Implemented in the
/// daemon (cli) over `SessionManager` — and in tests over their own session
/// managers — NEVER over the process CWD or a static config root.
///
/// The trait lives HERE (not in faktor-core, which this crate cannot depend
/// on, and not in faktor-session, which must not depend on this crate):
/// callers implement it for their own local type, so there is no dependency
/// cycle.
pub trait WorkspaceRootProvider: Send + Sync {
    /// The durable root directory of `workspace_id` (raw `WorkspaceId` from
    /// the daemon workspace table). `None` for an unknown workspace or a
    /// workspace without a root — the caller resolves that to
    /// [`LoadedInstructions::Empty`], never an error.
    fn workspace_root(&self, workspace_id: u64) -> Option<PathBuf>;
}

/// Default bound on cached loaded instruction sets per resolver.
pub const DEFAULT_RESOLVER_CACHE_ENTRIES: usize = 32;

/// Provider that never resolves a root: every resolution is Empty. Used by
/// code paths (and tests) that must not serve repository rules — the exact
/// behavioral equivalent of the historic optional loader being absent.
struct NoRoots;

impl WorkspaceRootProvider for NoRoots {
    fn workspace_root(&self, _workspace_id: u64) -> Option<PathBuf> {
        None
    }
}

/// A resolver over [`NoRoots`]: every `resolve` returns
/// [`LoadedInstructions::Empty`]. Drop-in replacement for the historic
/// "no instructions loader wired" state.
pub fn no_roots_resolver() -> Arc<InstructionResolver> {
    Arc::new(InstructionResolver::new(
        Arc::new(NoRoots),
        DEFAULT_RESOLVER_CACHE_ENTRIES,
    ))
}

/// LRU over loaded rule trees keyed by (root, epoch). A pinned-epoch
/// request is served from this cache even after the live tree changed; once
/// evicted it refuses loudly (the durable snapshot is the only other source
/// of old trees — this crate has none). Never unbounded.
struct RuleCache {
    entries: HashMap<(PathBuf, InstructionEpoch), Arc<Instructions>>,
    order: VecDeque<(PathBuf, InstructionEpoch)>,
    cap: usize,
}

impl RuleCache {
    fn new(cap: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            cap: cap.max(1),
        }
    }

    fn get(&mut self, key: &(PathBuf, InstructionEpoch)) -> Option<Arc<Instructions>> {
        if !self.entries.contains_key(key) {
            return None;
        }
        self.order.retain(|k| k != key);
        self.order.push_back(key.clone());
        self.entries.get(key).cloned()
    }

    fn put(&mut self, key: (PathBuf, InstructionEpoch), value: Arc<Instructions>) {
        if self.entries.contains_key(&key) {
            return;
        }
        if self.entries.len() >= self.cap {
            if let Some(evicted) = self.order.pop_front() {
                self.entries.remove(&evicted);
            }
        }
        self.order.push_back(key.clone());
        self.entries.insert(key, value);
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Per-workspace instruction resolver (P0-32): resolves a workspace id to
/// the loaded instruction set of its DURABLE root, with a bounded LRU cache
/// over (root, epoch) entries.
pub struct InstructionResolver {
    provider: Arc<dyn WorkspaceRootProvider>,
    cache: Mutex<RuleCache>,
}

impl InstructionResolver {
    /// `cap` bounds the cache; `0` degenerates to 1 (never unbounded).
    pub fn new(provider: Arc<dyn WorkspaceRootProvider>, cap: usize) -> Self {
        Self {
            provider,
            cache: Mutex::new(RuleCache::new(cap)),
        }
    }

    /// Resolve `workspace_id` to its loaded instruction set.
    ///
    /// - No durable root (unknown workspace / rootless session):
    ///   [`LoadedInstructions::Empty`] — documented, never an error.
    /// - A hostile tree (authority rule file oversized/unreadable): typed
    ///   error, never a silent truncation.
    /// - `pinned_epoch: Some(e)`: the request wants the OLD tree of epoch
    ///   `e`. Served from the cache when still present; otherwise the live
    ///   tree is loaded and served ONLY when its epoch still equals `e`;
    ///   any other outcome is a loud [`RulesLoadError::EpochMismatch`] —
    ///   never silently serving today's rules as yesterday's.
    pub fn resolve(
        &self,
        workspace_id: u64,
        pinned_epoch: Option<InstructionEpoch>,
    ) -> Result<LoadedInstructions, RulesLoadError> {
        let Some(root) = self.provider.workspace_root(workspace_id) else {
            return Ok(LoadedInstructions::Empty);
        };
        match pinned_epoch {
            Some(epoch) => {
                let key = (root.clone(), epoch);
                if let Some(cached) = self.cache_get(&key) {
                    return Ok(LoadedInstructions::Loaded(cached));
                }
                let live = Arc::new(Instructions::load(&root)?);
                if live.epoch() == epoch {
                    self.cache_put(key, live.clone());
                    Ok(LoadedInstructions::Loaded(live))
                } else {
                    Err(RulesLoadError::EpochMismatch {
                        expected: epoch,
                        actual: live.epoch(),
                    })
                }
            }
            None => {
                let loaded = Arc::new(Instructions::load(&root)?);
                let key = (root, loaded.epoch());
                self.cache_put(key, loaded.clone());
                Ok(LoadedInstructions::Loaded(loaded))
            }
        }
    }

    fn cache_get(&self, key: &(PathBuf, InstructionEpoch)) -> Option<Arc<Instructions>> {
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        cache.get(key)
    }

    fn cache_put(&self, key: (PathBuf, InstructionEpoch), value: Arc<Instructions>) {
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        cache.put(key, value);
    }

    /// Test/observability probe: current number of cached rule trees
    /// (always <= the resolver cap).
    pub fn cache_len(&self) -> usize {
        let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        cache.len()
    }

    /// The resolver cap (cache bound).
    pub fn cache_cap(&self) -> usize {
        let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        cache.cap
    }
}

/// Result of one [`InstructionResolver::resolve`]: either the loaded rule
/// tree of the workspace's durable root, or [`LoadedInstructions::Empty`]
/// for sessions whose workspace carries no durable root.
#[derive(Debug, Clone)]
pub enum LoadedInstructions {
    Empty,
    Loaded(Arc<Instructions>),
}

impl LoadedInstructions {
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }

    pub fn instructions(&self) -> Option<&Instructions> {
        match self {
            Self::Empty => None,
            Self::Loaded(ins) => Some(ins.as_ref()),
        }
    }

    pub fn epoch(&self) -> Option<InstructionEpoch> {
        self.instructions().map(Instructions::epoch)
    }

    pub fn active_for(&self, prompt: &str, touched: &[String]) -> Vec<Instruction> {
        self.instructions()
            .map(|ins| ins.active_for(prompt, touched))
            .unwrap_or_default()
    }

    /// True when the loaded tree holds at least one rule file (`Empty` is
    /// false).
    pub fn has_rules(&self) -> bool {
        self.instructions()
            .map(Instructions::has_rules)
            .unwrap_or(false)
    }

    /// Surfaced whole-file skips of the loaded set (empty for
    /// [`LoadedInstructions::Empty`]).
    pub fn skipped(&self) -> &[RuleSkip] {
        self.instructions()
            .map(Instructions::skipped)
            .unwrap_or(&[])
    }
}

// ---------------------------------------------------------------- skills

/// Skills: metadata only at discovery; bodies load on demand. `path` is the
/// skill's workspace-RELATIVE, `/`-normalized `SKILL.md` location: no
/// absolute path is ever stored, and no read ever re-resolves a pathname
/// outside the admitted [`RootedDir`] that discovered the skill.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    /// Workspace-relative `/`-normalized path of the skill's `SKILL.md`.
    pub path: String,
    pub summary: String,
    pub keywords: Vec<String>,
}

pub const MAX_SKILL_ENTRIES: usize = 2000;
const SKILL_SUMMARY_CAP: usize = 400;
const SKILL_BODY_CAP: usize = 64 * 1024;

/// A surfaced whole-skill refusal of the last discovery: a skills-directory
/// entry or `SKILL.md` that could not be read honestly (symlink/reparse
/// point, special file, unreadable entry, or an incomplete directory
/// listing). Skills are OPTIONAL best-effort payloads — exactly like
/// optional rule imports and unlike the authority rule files — so such an
/// entry is skipped WHOLE, never followed, never partially ingested, and
/// never becomes a [`SkillMeta`]; [`SkillRegistry::load_skill`] answers
/// `None` for it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SkillSkip {
    /// Workspace-relative `/`-normalized path of the refused entry.
    pub path: String,
    pub reason: String,
}

pub struct SkillRegistry {
    entries: Vec<SkillMeta>,
    skips: Vec<SkillSkip>,
    /// The ONE admitted [`RootedDir`] shared by discovery and every
    /// on-demand body read: retained so reads never re-resolve the root
    /// path (a root swapped after discovery cannot redirect a read).
    rooted: Option<RootedDir>,
}

/// Outcome of one skill-file read through the admitted [`RootedDir`]: a
/// refusal carries the surfaced reason and is never a partial body.
enum SkillFileRead {
    Missing,
    Refused(String),
    Content(String),
}

/// Read one skill file through the admitted rooted authority, firing the
/// same test seam as the rule surface (after enumeration, before the rooted
/// read, so swap windows are testable). The parent chain and the final
/// entry are classified with no-follow metadata: a symlink/reparse point on
/// any component, a special file, or an unreadable entry is a typed
/// [`SkillFileRead::Refused`] and is NEVER followed. The read is bounded by
/// `cap`: a larger honest file contributes its capped prefix (skills are
/// defined as bounded prefixes — the historical behavior), while an unsafe
/// entry contributes nothing.
fn read_skill_file(rooted: &RootedDir, rel: &Path, cap: usize) -> SkillFileRead {
    read_seam(rel);
    let meta = match rooted.entry_meta(rel) {
        Ok(Some(meta)) => meta,
        Ok(None) => return SkillFileRead::Missing,
        Err(e) if e.kind == ErrorKind::NotFound => return SkillFileRead::Missing,
        // A refused component walk (symlink/reparse point or non-directory
        // ancestor) is an unsafe entry, never a plain read failure.
        Err(e) if e.kind == ErrorKind::Permission => {
            return SkillFileRead::Refused(format!("unsafe entry: {rel:?}: {e}"))
        }
        Err(e) => return SkillFileRead::Refused(format!("unreadable: {e}")),
    };
    match meta.kind {
        RootedEntryKind::File => {}
        RootedEntryKind::Symlink => {
            return SkillFileRead::Refused(format!(
                "unsafe entry: {} is a symlink/reparse point; the no-follow rooted read refuses to follow it",
                rel.display()
            ))
        }
        RootedEntryKind::Directory | RootedEntryKind::Other => {
            return SkillFileRead::Refused(format!(
                "unsafe entry: {} is not a regular file; refusing to open it",
                rel.display()
            ))
        }
    }
    match rooted.read(rel, cap) {
        Ok(data) => SkillFileRead::Content(String::from_utf8_lossy(&data.bytes).into_owned()),
        Err(e) if e.kind == ErrorKind::NotFound => SkillFileRead::Missing,
        Err(e) if e.kind == ErrorKind::Permission => {
            SkillFileRead::Refused(format!("unsafe entry: {rel:?}: {e}"))
        }
        Err(e) => SkillFileRead::Refused(format!("unreadable: {e}")),
    }
}

impl SkillRegistry {
    /// Discover skills under `.faktor/skills` and `.claude/skills` through
    /// ONE admitted [`RootedDir`] of `root`: entries are classified with
    /// no-follow metadata (a symlink/reparse point or special file is
    /// surfaced as a [`SkillSkip`] and never followed), every candidate is
    /// read through THAT authority with the summary cap, and every stored
    /// path is workspace-relative. An unopenable root is surfaced too —
    /// never silently reported as "no skills".
    pub fn discover(root: &Path) -> Self {
        let rooted = match RootedDir::open(root) {
            Ok(rooted) => Some(rooted),
            Err(e) if e.kind == ErrorKind::NotFound => None,
            Err(e) => {
                return Self {
                    entries: Vec::new(),
                    skips: vec![SkillSkip {
                        path: ".".to_string(),
                        reason: format!("unsafe or unreadable root ({}): {e}", root.display()),
                    }],
                    rooted: None,
                }
            }
        };
        let mut entries = Vec::new();
        let mut skips = Vec::new();
        let mut capped = false;
        if let Some(rooted) = rooted.as_ref() {
            for dir_rel in [".faktor/skills", ".claude/skills"] {
                let dir = Path::new(dir_rel);
                match rooted.entry_meta(dir) {
                    Ok(None) => continue,
                    Ok(Some(meta)) if meta.kind == RootedEntryKind::Directory => {}
                    Ok(Some(meta)) => {
                        skips.push(SkillSkip {
                            path: dir_rel.to_string(),
                            reason: format!(
                                "unsafe entry: {dir_rel} is not a real directory ({:?}); refusing to enter it",
                                meta.kind
                            ),
                        });
                        continue;
                    }
                    Err(e) if e.kind == ErrorKind::NotFound => continue,
                    Err(e) if e.kind == ErrorKind::Permission => {
                        skips.push(SkillSkip {
                            path: dir_rel.to_string(),
                            reason: format!(
                                "unsafe entry: {dir_rel} could not be entered no-follow: {e}"
                            ),
                        });
                        continue;
                    }
                    Err(e) => {
                        skips.push(SkillSkip {
                            path: dir_rel.to_string(),
                            reason: format!("unreadable: {e}"),
                        });
                        continue;
                    }
                }
                let listing = match rooted.list_entries(dir, MAX_SKILL_ENTRIES) {
                    Ok(listing) => listing,
                    Err(e) if e.kind == ErrorKind::NotFound => continue,
                    Err(e) => {
                        skips.push(SkillSkip {
                            path: dir_rel.to_string(),
                            reason: format!("unreadable: {e}"),
                        });
                        continue;
                    }
                };
                if listing.overflowed {
                    skips.push(SkillSkip {
                        path: dir_rel.to_string(),
                        reason: format!(
                            "unreadable: directory holds more than {MAX_SKILL_ENTRIES} entries; its listing is refused as incomplete"
                        ),
                    });
                }
                let mut candidates = listing.entries;
                candidates.sort_by(|a, b| a.rel.cmp(&b.rel));
                for entry in candidates {
                    if entries.len() >= MAX_SKILL_ENTRIES {
                        capped = true;
                        break;
                    }
                    match entry.kind {
                        RootedEntryKind::Directory => {}
                        RootedEntryKind::Symlink => {
                            skips.push(SkillSkip {
                                path: path_str(&entry.rel),
                                reason: "unsafe entry: symlink/reparse point under a skills directory; never followed"
                                    .to_string(),
                            });
                            continue;
                        }
                        RootedEntryKind::Other => {
                            skips.push(SkillSkip {
                                path: path_str(&entry.rel),
                                reason: "unsafe entry: special file under a skills directory; never opened"
                                    .to_string(),
                            });
                            continue;
                        }
                        // A plain file directly under the skills root has no
                        // `SKILL.md` inside it; it is not a skill.
                        RootedEntryKind::File => continue,
                    }
                    let rel = entry.rel.join("SKILL.md");
                    let rel_str = path_str(&rel);
                    let name = entry.name.to_string_lossy().into_owned();
                    let raw = match read_skill_file(rooted, &rel, SKILL_SUMMARY_CAP) {
                        SkillFileRead::Missing => continue,
                        SkillFileRead::Refused(reason) => {
                            skips.push(SkillSkip {
                                path: rel_str,
                                reason,
                            });
                            continue;
                        }
                        SkillFileRead::Content(text) => text,
                    };
                    // Frontmatter-lite summary: first heading/paragraph + any
                    // keywords line. Never the full body.
                    let mut description = String::new();
                    let mut keywords = Vec::new();
                    for line in raw.lines().take(6) {
                        if let Some(d) = line.strip_prefix("# ") {
                            if description.is_empty() {
                                description = d.trim().to_string();
                            }
                        }
                        if let Some(k) = line.strip_prefix("## Keywords:") {
                            keywords = k
                                .split(',')
                                .map(|s| s.trim().to_string())
                                .filter(|s| !s.is_empty())
                                .collect();
                        }
                    }
                    entries.push(SkillMeta {
                        name,
                        description,
                        path: rel_str,
                        summary: raw,
                        keywords,
                    });
                }
            }
        }
        if capped {
            skips.push(SkillSkip {
                path: ".".to_string(),
                reason: format!(
                    "unreadable: more than {MAX_SKILL_ENTRIES} skills under the workspace; discovery refused the additional entries as incomplete"
                ),
            });
        }
        Self {
            entries,
            skips,
            rooted,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whole-entry refusals of the last discovery (unsafe links/special
    /// files, unreadable or incomplete listings): nothing listed here was
    /// followed or ingested.
    pub fn skipped(&self) -> &[SkillSkip] {
        &self.skips
    }

    pub fn find(&self, query: &str) -> Vec<&SkillMeta> {
        let q = query.to_lowercase();
        let mut hits: Vec<(&SkillMeta, u8)> = Vec::new();
        for e in &self.entries {
            let mut score = 0u8;
            if e.name.to_lowercase().contains(&q) {
                score += 4;
            }
            if e.description.to_lowercase().contains(&q) {
                score += 2;
            }
            if e.keywords.iter().any(|k| q.contains(&k.to_lowercase())) {
                score += 3;
            }
            if score > 0 {
                hits.push((e, score));
            }
        }
        hits.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.name.cmp(&b.0.name)));
        hits.into_iter().map(|(e, _)| e).take(10).collect()
    }

    /// Full body ONLY on demand (bounded by [`SKILL_BODY_CAP`]), read
    /// through the SAME admitted [`RootedDir`] as discovery: `None` when the
    /// name is unknown, the file vanished, or the re-read is refused (a
    /// symlink/reparse swap on any component is never followed). A larger
    /// honest body contributes its capped prefix, byte-identical to the
    /// historical pathname read.
    pub fn load_skill(&self, name: &str) -> Option<String> {
        let e = self.entries.iter().find(|e| e.name == name)?;
        let rooted = self.rooted.as_ref()?;
        match read_skill_file(rooted, Path::new(&e.path), SKILL_BODY_CAP) {
            SkillFileRead::Content(text) => Some(text),
            SkillFileRead::Missing | SkillFileRead::Refused(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn precedence_agents_over_claude_conflict() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "AGENTS.md", "always: no unsafe\n");
        write(d.path(), "CLAUDE.md", "always: use unsafe everywhere\n");
        let ins = Instructions::load(d.path()).unwrap();
        let active = ins.active_for("do it", &[]);
        assert_eq!(
            active.len(),
            1,
            "only AGENTS loads at top level: {active:?}"
        );
        assert!(active[0].content.contains("no unsafe"));
        assert_eq!(active[0].source, RuleSourceKind::AgentsMd);
    }

    #[test]
    fn faktor_native_outranks_agents() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "FAKTOR.md", "faktor rules\n");
        write(d.path(), "AGENTS.md", "agents rules\n");
        let ins = Instructions::load(d.path()).unwrap();
        let active = ins.active_for("x", &[]);
        let first = active
            .iter()
            .find(|i| i.content.contains("faktor rules"))
            .unwrap();
        assert_eq!(first.source, RuleSourceKind::FaktorNative);
        assert!(active.iter().any(|i| i.content.contains("agents rules")));
    }

    #[test]
    fn conditional_scope_activation() {
        let d = tempfile::tempdir().unwrap();
        // A top-level convention rule needs a keyword directive; a rule in
        // a SUBDIRECTORY activates when a touched file lives under it.
        write(
            d.path(),
            ".cursor/rules/backend.mdc",
            "# Scope: api\nbackend rules\n",
        );
        write(
            d.path(),
            ".cursor/rules/frontend/ux.mdc",
            "frontend style\n",
        );
        let ins = Instructions::load(d.path()).unwrap();
        let active = ins.active_for("fix the api server", &[]);
        assert!(
            active.iter().any(|i| i.path.contains("backend")),
            "keyword directive activates the top-level rule"
        );
        assert!(
            !active.iter().any(|i| i.path.contains("frontend")),
            "unrelated rules never load"
        );
        // Touching a file under the subdirectory scope activates it.
        let touched = ins.active_for("anything", &["frontend/App.tsx".into()]);
        assert!(
            touched.iter().any(|i| i.path.contains("frontend")),
            "subdirectory scope activates on contained touched files"
        );
    }

    #[test]
    fn scope_activation_is_separator_agnostic_and_containment_safe() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "AGENTS.md", "always: root rules\n");
        write(d.path(), ".cursor/rules/sub/agents.mdc", "sub rules\n");
        write(d.path(), ".cursor/rules/sub2/other.mdc", "sibling rules\n");
        let ins = Instructions::load(d.path()).unwrap();
        let sub_rule = ".cursor/rules/sub/agents.mdc";
        // Explicitly constructed separator forms (never OS-generated) both
        // activate the subdirectory scope.
        for touched in [
            "sub/App.tsx",
            "sub\\App.tsx",
            "sub/deep/nested.rs",
            "sub\\deep\\nested.rs",
        ] {
            let active = ins.active_for("anything", &[touched.into()]);
            assert!(
                active.iter().any(|i| i.path == sub_rule),
                "touched {touched:?} must activate {sub_rule}: {active:?}"
            );
        }
        // Prefix-lookalike siblings, traversal escapes and absolute forms
        // never activate the sub scope.
        for touched in [
            "sub2/App.tsx",
            "sub2\\App.tsx",
            "sub/../evil.rs",
            "sub\\..\\evil.rs",
            "../sub/evil.rs",
            "/sub/App.tsx",
            "\\sub\\App.tsx",
            "C:\\sub\\App.tsx",
            "C:/sub/App.tsx",
        ] {
            let active = ins.active_for("anything", &[touched.into()]);
            assert!(
                !active.iter().any(|i| i.path == sub_rule),
                "touched {touched:?} must never activate {sub_rule}: {active:?}"
            );
        }
    }

    #[test]
    fn containment_rejects_escapes_and_prefix_siblings() {
        for ok in ["a/b.txt", "a\\b.txt", "./a/b.txt", "a//b.txt", "a"] {
            assert!(touched_in_scope(ok, "a"), "{ok:?} is inside scope a");
        }
        for bad in [
            "ab/x",
            "ab\\x",
            "a/../b.txt",
            "a\\..\\b.txt",
            "../a/b.txt",
            "..\\a\\b.txt",
            "/a/b.txt",
            "\\a\\b.txt",
            "C:/a/b.txt",
            "C:\\a\\b.txt",
        ] {
            assert!(
                !touched_in_scope(bad, "a"),
                "{bad:?} must not match scope a"
            );
        }
        // The root scope (empty) contains every well-formed touched path —
        // but never a hostile traversal form. A hostile scope can never
        // widen containment either.
        assert!(touched_in_scope("a/b.txt", ""));
        assert!(touched_in_scope("a\\b.txt", ""));
        assert!(!touched_in_scope("../x", ""));
        assert!(!touched_in_scope("a/b.txt", ".."));
        // Both separator styles normalize to the same durable string.
        assert_eq!(
            normalized_rel_string(".cursor\\rules\\sub\\x.mdc"),
            normalized_rel_string(".cursor/rules/sub/x.mdc")
        );
        assert_eq!(normalized_rel_string("..\\x"), None);
        assert_eq!(
            kind_for_rel_path(Path::new(".cursor\\rules\\x.mdc")),
            Some(RuleSourceKind::CursorRules)
        );
        assert_eq!(
            kind_for_rel_path(Path::new(".cursor/rules/../AGENTS.md")),
            None
        );
    }

    #[test]
    fn keyword_directive_activation() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            ".cursor/rules/db.mdc",
            "# Scope: postgres, sql\nuse pools\n",
        );
        let ins = Instructions::load(d.path()).unwrap();
        assert!(ins
            .active_for("postgres connection", &[])
            .iter()
            .any(|i| i.path.contains("db")));
        assert!(!ins
            .active_for("frontend colors", &[])
            .iter()
            .any(|i| i.path.contains("db")));
        let a = ins.active_for("sql tuning", &[]);
        assert!(a[0].reason_loaded.contains("keyword:sql"));
    }

    #[test]
    fn epoch_flips_on_change() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "AGENTS.md", "v1 rules\n");
        let mut ins = Instructions::load(d.path()).unwrap();
        let e0 = ins.epoch();
        assert!(!ins.reload_if_changed().unwrap());
        write(d.path(), "AGENTS.md", "v2 rules (changed mid-task)\n");
        assert!(
            ins.reload_if_changed().unwrap(),
            "stale epoch must be detected"
        );
        assert_ne!(ins.epoch(), e0);
        let active = ins.active_for("x", &[]);
        assert!(active[0].content.contains("v2"));
    }

    #[test]
    fn legacy_import_has_lowest_priority_and_never_always_loads() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            ".faktor/legacy/import.md",
            "# Scope: import\nlegacy content\n",
        );
        let ins = Instructions::load(d.path()).unwrap();
        assert!(!ins
            .active_for("anything unrelated", &[])
            .iter()
            .any(|i| i.content.contains("legacy")));
        let a = ins.active_for("import config", &[]);
        assert!(a
            .iter()
            .any(|i| i.source == RuleSourceKind::LegacyRules && i.priority == 10));
    }

    #[test]
    fn skills_cost_zero_prompt_tokens_until_loaded() {
        let d = tempfile::tempdir().unwrap();
        for i in 0..1000 {
            write(
                d.path(),
                &format!(".faktor/skills/skill-{i:04}/SKILL.md"),
                &format!(
                    "# Skill {i}\n## Keywords: kw-{i}\nfull body of skill {i} repeated to length\n"
                ),
            );
        }
        let reg = SkillRegistry::discover(d.path());
        assert_eq!(reg.len(), 1000);
        // Discovery output = metadata summaries only; total metadata bytes
        // stay far below full bodies (each body is 60+ bytes -> full would
        // be >= 60k; summaries capped at 400 each but only 6 lines read).
        let total_meta: usize = reg.entries.iter().map(|e| e.summary.len()).sum();
        assert!(total_meta < 1000 * 120, "summaries bounded");
        // Unrelated query: no hit, nothing loaded.
        assert!(reg.find("completely unrelated").is_empty());
        // Targeted load returns ONLY that skill's body, bounded.
        let body = reg.load_skill("skill-0042").unwrap();
        assert!(
            body.contains("full body of skill 42"),
            "body head: {:?}",
            &body[..body.len().min(120)]
        );
        assert!(body.len() <= SKILL_BODY_CAP);
        assert!(reg.load_skill("missing").is_none());
    }

    // ---------------------------------------- rooted skills (workspace escape)
    // The same adversarial posture as the rule surface: skill entries
    // pointing outside through symlinks, entries swapped after enumeration,
    // nested link directories, and honest trees that must stay
    // byte-identical to the historical pathname reads (the golden functions
    // below reproduce the old pathname semantics verbatim).

    #[test]
    fn global_skill_cap_is_surfaced_not_silently_dropped() {
        let d = tempfile::tempdir().unwrap();
        for i in 0..MAX_SKILL_ENTRIES {
            write(
                d.path(),
                &format!(".faktor/skills/s-{i:05}/SKILL.md"),
                "# S\nbody\n",
            );
        }
        write(d.path(), ".claude/skills/extra/SKILL.md", "# Extra\nbody\n");
        let reg = SkillRegistry::discover(d.path());
        assert_eq!(reg.len(), MAX_SKILL_ENTRIES, "global cap holds");
        let cap_skip = reg
            .skipped()
            .iter()
            .find(|s| s.path == ".")
            .expect("global cap surfaced");
        assert!(cap_skip.reason.contains("more than 2000"), "{cap_skip:?}");
    }

    /// The pre-rooted skill read (historical pathname semantics), reproduced
    /// in the test as the golden reference.
    fn legacy_read_bounded(path: &Path, cap: usize) -> Option<String> {
        let bytes = std::fs::read(path).ok()?;
        Some(String::from_utf8_lossy(&bytes[..bytes.len().min(cap)]).into_owned())
    }

    /// The pre-rooted discovery (historical pathname semantics), reproduced
    /// as the golden reference: same roots, same order, same summary parse.
    fn legacy_skill_entries(root: &Path) -> Vec<SkillMeta> {
        let mut entries = Vec::new();
        for dir in [root.join(".faktor/skills"), root.join(".claude/skills")] {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut names: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
            names.sort();
            for n in names {
                let meta_path = n.join("SKILL.md");
                let Some(raw) = legacy_read_bounded(&meta_path, SKILL_SUMMARY_CAP) else {
                    continue;
                };
                let name = n
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let mut description = String::new();
                let mut keywords = Vec::new();
                for line in raw.lines().take(6) {
                    if let Some(d) = line.strip_prefix("# ") {
                        if description.is_empty() {
                            description = d.trim().to_string();
                        }
                    }
                    if let Some(k) = line.strip_prefix("## Keywords:") {
                        keywords = k
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                    }
                }
                entries.push(SkillMeta {
                    name,
                    description,
                    path: meta_path.to_string_lossy().into_owned(),
                    summary: raw,
                    keywords,
                });
            }
        }
        entries
    }

    #[test]
    fn honest_skills_load_byte_identical_to_the_legacy_pathname_reads() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            ".faktor/skills/alpha/SKILL.md",
            "# Alpha skill\n## Keywords: alpha, one\nalpha body\n",
        );
        write(
            d.path(),
            ".claude/skills/beta/SKILL.md",
            "# Beta skill\nbeta body\n## Keywords: two\n",
        );
        // A directory without a SKILL.md is not a skill.
        write(d.path(), ".faktor/skills/not-a-skill/notes.txt", "notes\n");
        // A summary beyond SKILL_SUMMARY_CAP and a body beyond
        // SKILL_BODY_CAP must both come back as the historical capped
        // prefixes, byte for byte.
        let long_summary = format!("# Long skill\n{}\n", "summary line\n".repeat(60));
        write(d.path(), ".faktor/skills/gamma/SKILL.md", &long_summary);
        let huge_body = format!("# Huge skill\n{}", "x".repeat(SKILL_BODY_CAP + 5000));
        write(d.path(), ".claude/skills/delta/SKILL.md", &huge_body);

        let reg = SkillRegistry::discover(d.path());
        let legacy = legacy_skill_entries(d.path());
        assert_eq!(reg.len(), legacy.len(), "same skill set");
        assert!(reg.skipped().is_empty(), "{:?}", reg.skipped());
        for (new, old) in reg.entries.iter().zip(&legacy) {
            assert_eq!(new.name, old.name, "same discovery order and names");
            assert_eq!(new.description, old.description);
            assert_eq!(new.keywords, old.keywords);
            assert_eq!(new.summary, old.summary, "summary bytes identical");
            assert!(new.summary.len() <= SKILL_SUMMARY_CAP);
            assert!(
                Path::new(&new.path).is_relative(),
                "stored path is workspace-relative: {}",
                new.path
            );
            assert_eq!(
                d.path().join(&new.path),
                PathBuf::from(&old.path),
                "relative path addresses the same file"
            );
            let body = reg.load_skill(&new.name).unwrap();
            assert_eq!(
                body,
                legacy_read_bounded(Path::new(&old.path), SKILL_BODY_CAP).unwrap(),
                "body bytes identical to the legacy pathname read (including capped prefixes)"
            );
            assert!(body.len() <= SKILL_BODY_CAP);
        }
        assert_eq!(reg.find("alpha")[0].name, "alpha");
    }

    #[cfg(unix)]
    #[test]
    fn skill_file_symlinked_outside_is_refused_and_outside_bytes_never_ingested() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("ws");
        let outside = base.path().join("outside");
        std::fs::create_dir_all(root.join(".faktor/skills/evil")).unwrap();
        std::fs::create_dir_all(root.join(".faktor/skills/honest")).unwrap();
        std::fs::create_dir_all(root.join(".faktor/skills/nested")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let secret = "OUTSIDE SKILL SECRET\n";
        std::fs::write(outside.join("SKILL.md"), secret).unwrap();
        std::fs::write(
            root.join(".faktor/skills/honest/SKILL.md"),
            "# Honest\nhonest body\n",
        )
        .unwrap();
        // (a) SKILL.md itself is a symlink to the outside secret.
        std::os::unix::fs::symlink(
            outside.join("SKILL.md"),
            root.join(".faktor/skills/evil/SKILL.md"),
        )
        .unwrap();
        // (b) a whole skill directory is a symlink to the outside directory.
        std::os::unix::fs::symlink(&outside, root.join(".faktor/skills/link-dir")).unwrap();
        // (c) a nested directory symlink inside a skill dir: discovery never
        // descends past the direct skill directory, so it is never entered.
        std::os::unix::fs::symlink(&outside, root.join(".faktor/skills/nested/deeper")).unwrap();

        let reg = SkillRegistry::discover(&root);
        assert_eq!(
            reg.entries
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            vec!["honest"],
            "only the honest skill is discovered"
        );
        assert!(reg.load_skill("evil").is_none());
        assert!(reg.load_skill("link-dir").is_none());
        assert!(reg.load_skill("nested").is_none());
        let evil = reg
            .skipped()
            .iter()
            .find(|s| s.path == ".faktor/skills/evil/SKILL.md")
            .expect("surfaced SKILL.md refusal");
        assert!(evil.reason.contains("unsafe entry"), "{evil:?}");
        let link = reg
            .skipped()
            .iter()
            .find(|s| s.path == ".faktor/skills/link-dir")
            .expect("surfaced skill-dir refusal");
        assert!(link.reason.contains("unsafe entry"), "{link:?}");
        // The outside bytes were never ingested, and are intact.
        assert!(reg.load_skill("honest").unwrap().contains("honest body"));
        for e in &reg.entries {
            assert!(!e.summary.contains("OUTSIDE SKILL SECRET"), "{e:?}");
            assert!(!e.description.contains("OUTSIDE SKILL SECRET"), "{e:?}");
        }
        for s in reg.skipped() {
            assert!(!Path::new(&s.path).is_absolute(), "{s:?}");
        }
        assert_eq!(
            std::fs::read_to_string(outside.join("SKILL.md")).unwrap(),
            secret
        );
    }

    #[cfg(unix)]
    #[test]
    fn skill_directory_swapped_for_outside_symlink_between_discovery_and_load_is_refused() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("ws");
        let outside = base.path().join("outside");
        std::fs::create_dir_all(root.join(".faktor/skills/swap")).unwrap();
        std::fs::create_dir_all(root.join(".faktor/skills/file-swap")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(
            root.join(".faktor/skills/swap/SKILL.md"),
            "# Swap\nhonest swap body\n",
        )
        .unwrap();
        std::fs::write(
            root.join(".faktor/skills/file-swap/SKILL.md"),
            "# File swap\nhonest file body\n",
        )
        .unwrap();
        std::fs::write(outside.join("SKILL.md"), "OUTSIDE SWAP SECRET\n").unwrap();

        let reg = SkillRegistry::discover(&root);
        assert!(reg.load_skill("swap").unwrap().contains("honest swap body"));
        // Between enumeration and the on-demand read: swap the SKILL
        // DIRECTORY for a symlink to the outside directory.
        std::fs::remove_dir_all(root.join(".faktor/skills/swap")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join(".faktor/skills/swap")).unwrap();
        assert!(
            reg.load_skill("swap").is_none(),
            "a directory swapped for an outside symlink must refuse the read"
        );
        // And the SKILL.md itself swapped for an outside symlink.
        std::fs::remove_file(root.join(".faktor/skills/file-swap/SKILL.md")).unwrap();
        std::os::unix::fs::symlink(
            outside.join("SKILL.md"),
            root.join(".faktor/skills/file-swap/SKILL.md"),
        )
        .unwrap();
        assert!(
            reg.load_skill("file-swap").is_none(),
            "a SKILL.md swapped for an outside symlink must refuse the read"
        );
        // Re-discovery surfaces both as whole-skill refusals; outside bytes
        // never enter the registry.
        let reg2 = SkillRegistry::discover(&root);
        assert!(reg2.is_empty());
        let swapped_dir = reg2
            .skipped()
            .iter()
            .find(|s| s.path == ".faktor/skills/swap")
            .expect("swapped dir surfaced");
        assert!(
            swapped_dir.reason.contains("unsafe entry"),
            "{swapped_dir:?}"
        );
        let swapped_file = reg2
            .skipped()
            .iter()
            .find(|s| s.path == ".faktor/skills/file-swap/SKILL.md")
            .expect("swapped file surfaced");
        assert!(
            swapped_file.reason.contains("unsafe entry"),
            "{swapped_file:?}"
        );
        assert!(!reg2
            .entries
            .iter()
            .any(|e| e.summary.contains("OUTSIDE SWAP SECRET")));
        assert_eq!(
            std::fs::read_to_string(outside.join("SKILL.md")).unwrap(),
            "OUTSIDE SWAP SECRET\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn seam_swap_of_skill_directory_between_enumeration_and_read_is_refused() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("ws");
        let outside = base.path().join("outside");
        std::fs::create_dir_all(root.join(".faktor/skills/swap")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(
            root.join(".faktor/skills/swap/SKILL.md"),
            "# Swap\nhonest seam body\n",
        )
        .unwrap();
        std::fs::write(outside.join("SKILL.md"), "OUTSIDE SEAM SECRET\n").unwrap();
        let swap_root = root.clone();
        let outside_dir = outside.clone();
        let (_lock, _clear) = install_read_seam(move |rel| {
            if rel == Path::new(".faktor/skills/swap/SKILL.md") {
                let _ = std::fs::remove_dir_all(swap_root.join(".faktor/skills/swap"));
                let _ =
                    std::os::unix::fs::symlink(&outside_dir, swap_root.join(".faktor/skills/swap"));
            }
        });
        let reg = SkillRegistry::discover(&root);
        assert!(reg.is_empty(), "the swapped skill must not be ingested");
        let skip = reg
            .skipped()
            .iter()
            .find(|s| s.path == ".faktor/skills/swap/SKILL.md")
            .expect("swap in the enumeration -> read window surfaced");
        assert!(skip.reason.contains("unsafe entry"), "{skip:?}");
        assert!(!reg
            .entries
            .iter()
            .any(|e| e.summary.contains("OUTSIDE SEAM SECRET")));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_skills_convention_dir_is_refused_and_never_entered() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("ws");
        let outside = base.path().join("outside");
        std::fs::create_dir_all(root.join(".faktor")).unwrap();
        std::fs::create_dir_all(outside.join("skill-x")).unwrap();
        std::fs::write(outside.join("skill-x/SKILL.md"), "OUTSIDE DIR SECRET\n").unwrap();
        std::os::unix::fs::symlink(&outside, root.join(".faktor/skills")).unwrap();
        let reg = SkillRegistry::discover(&root);
        assert!(reg.is_empty());
        let skip = reg
            .skipped()
            .iter()
            .find(|s| s.path == ".faktor/skills")
            .expect("linked convention dir surfaced");
        assert!(skip.reason.contains("unsafe entry"), "{skip:?}");
        assert!(!reg
            .entries
            .iter()
            .any(|e| e.summary.contains("OUTSIDE DIR SECRET")));
    }

    #[cfg(unix)]
    #[test]
    fn nested_skill_directory_symlink_is_never_descended() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("ws");
        let outside = base.path().join("outside");
        write(
            &root,
            ".faktor/skills/outer/SKILL.md",
            "# Outer\nouter body\n",
        );
        std::fs::create_dir_all(outside.join("deep")).unwrap();
        std::fs::write(outside.join("deep/SKILL.md"), "OUTSIDE NESTED SKILL\n").unwrap();
        std::os::unix::fs::symlink(&outside, root.join(".faktor/skills/outer/inner")).unwrap();
        let reg = SkillRegistry::discover(&root);
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.entries[0].name, "outer");
        assert!(reg.load_skill("outer").unwrap().contains("outer body"));
        assert!(reg.load_skill("inner").is_none());
        assert!(reg.load_skill("deep").is_none());
        assert!(
            reg.skipped().is_empty(),
            "nested directories are never enumerated: {:?}",
            reg.skipped()
        );
        assert!(!reg
            .entries
            .iter()
            .any(|e| e.summary.contains("OUTSIDE NESTED SKILL")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_reparse_skill_entries_are_refused_like_unix_links() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("ws");
        let outside = base.path().join("outside");
        std::fs::create_dir_all(root.join(".faktor/skills/honest")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(
            root.join(".faktor/skills/honest/SKILL.md"),
            "# Honest\nhonest body\n",
        )
        .unwrap();
        std::fs::write(outside.join("SKILL.md"), "OUTSIDE WIN SECRET\n").unwrap();
        if std::os::windows::fs::symlink_dir(&outside, root.join(".faktor/skills/link-dir"))
            .is_err()
        {
            // Creating a symlink needs SeCreateSymbolicLinkPrivilege or
            // developer mode; the fs crate's Windows seam suite covers the
            // handle-anchored refusal on runners that grant it.
            return;
        }
        let reg = SkillRegistry::discover(&root);
        assert_eq!(reg.entries.len(), 1);
        assert!(reg.load_skill("link-dir").is_none());
        let skip = reg
            .skipped()
            .iter()
            .find(|s| s.path == ".faktor/skills/link-dir")
            .expect("surfaced reparse refusal");
        assert!(skip.reason.contains("unsafe entry"), "{skip:?}");
        assert!(!reg
            .entries
            .iter()
            .any(|e| e.summary.contains("OUTSIDE WIN SECRET")));
    }

    #[test]
    fn hostile_deep_walk_is_bounded() {
        let d = tempfile::tempdir().unwrap();
        let mut p = d.path().join(".cursor/rules");
        for _ in 0..40 {
            p = p.join("a");
        }
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("deep.md"), "deep\n").unwrap();
        let ins = Instructions::load(d.path()).unwrap();
        assert!(ins.rules.len() <= 1, "depth cap bounds the walk");
    }

    #[test]
    fn per_directory_depth_keeps_shallow_siblings_visible() {
        // Audit 32 regression: depth accounting must be PER DIRECTORY
        // BRANCH, never one global counter shared across sibling branches.
        // A shallow file at depth 5 next to TWO 30-deep hostile trees must
        // still be discovered (the old shared counter let the deep branches
        // consume the whole allowance and cut the shallow one too), while
        // each deep tree is still truncated on its own at MAX_WALK_DEPTH.
        let d = tempfile::tempdir().unwrap();
        for tree in ["deep-a", "deep-b"] {
            let mut p = d.path().join(".cursor/rules").join(tree);
            for _ in 0..30 {
                p = p.join("x");
            }
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("bottom.md"), "too deep\n").unwrap();
        }
        let mut shallow = d.path().join(".cursor/rules/shallow");
        for _ in 0..5 {
            shallow = shallow.join("y");
        }
        std::fs::create_dir_all(&shallow).unwrap();
        std::fs::write(shallow.join("near.md"), "shallow rules\n").unwrap();
        let ins = Instructions::load(d.path()).unwrap();
        let near: Vec<&Instruction> = ins
            .rules
            .iter()
            .filter(|r| r.path.contains("near"))
            .collect();
        let bottom: Vec<&Instruction> = ins
            .rules
            .iter()
            .filter(|r| r.path.contains("bottom"))
            .collect();
        assert_eq!(
            near.len(),
            1,
            "the depth-5 sibling file must be discovered: {}",
            ins.rules
                .iter()
                .map(|r| r.path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        assert_eq!(
            bottom.len(),
            0,
            "both 30-deep branches must still be cut at MAX_WALK_DEPTH"
        );
    }

    // ------------------------------------------------- hashes + epochs (P0-33)

    #[test]
    fn hashes_and_epochs_are_blake3_durable_and_content_keyed() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "AGENTS.md", "always: durable rules\n");
        write(
            d.path(),
            ".cursor/rules/frontend/ux.mdc",
            "# Scope: ui\nfrontend style\n",
        );
        // Two loads of identical content -> identical hashes AND epochs.
        let a = Instructions::load(d.path()).unwrap();
        let b = Instructions::load(d.path()).unwrap();
        assert_eq!(a.epoch(), b.epoch());
        assert_eq!(a.rules.len(), 2);
        for (x, y) in a.rules.iter().zip(&b.rules) {
            assert_eq!(x.hash, y.hash, "identical content -> identical hash");
            assert_eq!(x.content, y.content);
        }
        // Different content -> different hash and epoch.
        write(d.path(), "AGENTS.md", "always: changed rules\n");
        let c = Instructions::load(d.path()).unwrap();
        assert_ne!(c.epoch(), a.epoch());
        assert_ne!(
            c.rules.iter().find(|r| r.path == "AGENTS.md").unwrap().hash,
            a.rules.iter().find(|r| r.path == "AGENTS.md").unwrap().hash
        );
        // Sequential loads after a rewrite agree (process-independent
        // algorithm: the digest is a pure function of bytes).
        let d2 = Instructions::load(d.path()).unwrap();
        assert_eq!(d2.epoch(), c.epoch());
    }

    #[test]
    fn digest_newtypes_serde_roundtrip_as_hex() {
        // Compile-time serde bounds (no JSON crate lives in this crate;
        // the serde glue is the hex codec below).
        fn assert_serde<T: serde::Serialize + serde::de::DeserializeOwned>() {}
        assert_serde::<InstructionHash>();
        assert_serde::<InstructionEpoch>();
        assert_serde::<RuleSkip>();
        let h = hash_of(Path::new("AGENTS.md"), "rules\n");
        // hex32/dehex32 are exact inverses; the wire format is lowercase
        // hex of the 32 digest bytes.
        let enc = hex32(h.as_bytes());
        assert_eq!(enc.len(), 64);
        assert_eq!(InstructionHash(dehex32(&enc).unwrap()), h);
        assert_eq!(h.to_string(), enc);
        assert!(dehex32(&enc[..63]).is_none(), "short hex rejected");
        assert!(dehex32(&"z".repeat(64)).is_none(), "non-hex rejected");
        assert!(dehex32(&enc.to_uppercase()).is_none(), "uppercase rejected");
        // Projection is deterministic and lossy-by-design (seam-only).
        assert_eq!(
            h.as_u64(),
            u64::from_le_bytes(h.as_bytes()[..8].try_into().unwrap())
        );
        // The empty tree's epoch is deterministic: BLAKE3 of nothing.
        let empty = Instructions::load(Path::new("/nonexistent-rule-root-xyz"))
            .unwrap()
            .epoch();
        assert_eq!(empty, InstructionEpoch::from(blake3::hash(b"")));
    }

    // ------------------------------------------- oversized authority rules (P0-34)

    #[test]
    fn oversized_authority_agents_md_fails_loading_loudly() {
        // (a) A 100 KiB AGENTS.md whose critical rule starts at byte 70 KiB:
        // the load MUST fail loudly (typed Oversized) instead of silently
        // omitting the rule.
        let d = tempfile::tempdir().unwrap();
        let mut body = vec![b'x'; 100 * 1024];
        body[70 * 1024..70 * 1024 + 22].copy_from_slice(b"CRITICAL RULE AT 70KIB");
        let text = String::from_utf8_lossy(&body);
        write(d.path(), "AGENTS.md", &text);
        let err = Instructions::load(d.path()).unwrap_err();
        assert!(
            matches!(err, RulesLoadError::Oversized(_)),
            "authority oversize must be a typed Oversized error: {err:?}"
        );
        assert!(err.to_string().contains("AGENTS.md"), "{err}");
        assert!(err.to_string().contains("rule bound"), "{err}");
        // The snapshot capture refuses the same tree with its typed error.
        let err2 = EnvSnapshot::capture(d.path(), "env-a", 1).unwrap_err();
        assert!(matches!(err2, EnvSnapshotError::Oversized(_)), "{err2:?}");
        assert!(err2.to_string().contains("AGENTS.md"), "{err2}");
        // FAKTOR.md and CLAUDE.md are authority too.
        for name in ["FAKTOR.md", "CLAUDE.md"] {
            let d2 = tempfile::tempdir().unwrap();
            write(d2.path(), name, &"y".repeat(MAX_RULE_BYTES + 1));
            let err3 = Instructions::load(d2.path()).unwrap_err();
            assert!(
                matches!(err3, RulesLoadError::Oversized(_)),
                "{name}: {err3:?}"
            );
            assert!(err3.to_string().contains(name), "{err3}");
        }
    }

    #[test]
    fn oversized_optional_import_is_surfaced_skip_never_partial() {
        // (b) An optional imported file over the cap: load SUCCEEDS, the
        // set records a surfaced-skip entry, and no partial text reaches
        // the rules.
        let d = tempfile::tempdir().unwrap();
        let mut body = vec![b'z'; MAX_RULE_BYTES + 8192];
        body[0..36].copy_from_slice(b"# Scope: hostile\nTAIL MARKER AT 70K\n");
        body[MAX_RULE_BYTES..MAX_RULE_BYTES + 23].copy_from_slice(b"SECRET RULE BEYOND CAP\n");
        let text = String::from_utf8_lossy(&body);
        write(d.path(), ".cursor/rules/huge.mdc", &text);
        write(d.path(), "AGENTS.md", "always: sane rules\n");
        let ins = Instructions::load(d.path()).unwrap();
        // The set records the surfaced skip with the file and its size.
        let skip = ins
            .skipped()
            .iter()
            .find(|s| s.path == ".cursor/rules/huge.mdc")
            .expect("surfaced skip entry must exist");
        assert_eq!(skip.bytes, Some(text.len() as u64));
        assert!(skip.reason.contains("oversized"), "{skip:?}");
        // No partial text reaches the rules: no rule from that file at all,
        // and nothing beyond the cap anywhere.
        assert!(
            !ins.rules.iter().any(|r| r.path.contains("huge")),
            "the oversized optional file must not be half-loaded"
        );
        assert!(
            !ins.active_for("hostile", &[])
                .iter()
                .any(|i| i.content.contains("SECRET RULE BEYOND CAP")),
            "no partial text may reach activation"
        );
        // The snapshot capture behaves identically and surfaces the skip on
        // the durable snapshot.
        let captured = EnvSnapshot::capture(d.path(), "env-a", 1).unwrap();
        let cskip = captured
            .snapshot
            .skipped
            .iter()
            .find(|s| s.path == ".cursor/rules/huge.mdc")
            .expect("capture must surface the same skip");
        assert_eq!(cskip.bytes, skip.bytes);
        assert!(!captured
            .snapshot
            .workspace_paths
            .contains_key(".cursor/rules/huge.mdc"));
        assert!(!captured.content.contains_key(".cursor/rules/huge.mdc"));
        // A pinned read from the snapshot surfaces the skip too.
        let pinned = Instructions::from_snapshot(&captured.snapshot, &captured.content).unwrap();
        assert!(pinned
            .skipped()
            .iter()
            .any(|s| s.path == ".cursor/rules/huge.mdc"));
        assert!(!pinned
            .active_for("hostile", &[])
            .iter()
            .any(|i| i.content.contains("SECRET RULE BEYOND CAP")));
    }

    #[test]
    fn oversized_import_skip_is_found_with_both_separator_forms() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "AGENTS.md", "always: sane rules\n");
        let text = "z".repeat(MAX_RULE_BYTES + 1);
        write(d.path(), ".cursor/rules/huge.mdc", &text);
        let ins = Instructions::load(d.path()).unwrap();
        let find = |query: &str| {
            ins.skipped()
                .iter()
                .find(|s| normalized_rel_string(&s.path) == normalized_rel_string(query))
        };
        let skip = find(".cursor/rules/huge.mdc").expect("surfaced skip entry must exist");
        assert_eq!(
            skip.path, ".cursor/rules/huge.mdc",
            "durable skip paths are /-normalized on every platform"
        );
        assert_eq!(skip.bytes, Some(text.len() as u64));
        assert!(skip.reason.contains("oversized"), "{skip:?}");
        // The same surfaced entry is found from a `\`-separated query.
        assert!(find(".cursor\\rules\\huge.mdc").is_some());
        // The snapshot surfaces the identical normalized entry.
        let captured = EnvSnapshot::capture(d.path(), "env-a", 1).unwrap();
        assert!(captured
            .snapshot
            .skipped
            .iter()
            .any(|s| s.path == ".cursor/rules/huge.mdc"));
    }

    #[test]
    fn rule_at_exact_cap_loads_fully() {
        // (c) A root AGENTS.md exactly at the cap loads FULLY — the marker
        // written at the very last bytes is present; the cap is not off by
        // one in either direction.
        let d = tempfile::tempdir().unwrap();
        let mut body = vec![b'r'; MAX_RULE_BYTES];
        body[MAX_RULE_BYTES - 23..].copy_from_slice(b"FINAL BYTES LOAD WHOLE\n");
        let text = String::from_utf8_lossy(&body);
        write(d.path(), "AGENTS.md", &text);
        let ins = Instructions::load(d.path()).unwrap();
        let agent = ins
            .rules
            .iter()
            .find(|r| r.path == "AGENTS.md")
            .expect("at-cap AGENTS.md loads");
        assert_eq!(
            agent.content.len(),
            MAX_RULE_BYTES,
            "loaded fully, not truncated"
        );
        assert!(agent.content.ends_with("FINAL BYTES LOAD WHOLE\n"));
        assert!(ins.skipped().is_empty());
    }

    // --------------------------------------- env snapshots (audit 97)

    #[test]
    fn snapshot_epoch_equals_live_load_epoch_of_the_same_tree() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "AGENTS.md", "always: pinned rules v1\n");
        write(
            d.path(),
            ".cursor/rules/frontend/ux.mdc",
            "# Scope: ui\nfrontend style\n",
        );
        let live = Instructions::load(d.path()).unwrap();
        let captured = EnvSnapshot::capture(d.path(), "env-a", 7).unwrap();
        assert_eq!(captured.snapshot.instruction_epoch, live.epoch().as_u64());
        assert_eq!(captured.snapshot.workspace_paths.len(), 2);
        assert!(captured.content["AGENTS.md"].contains("v1"));
        assert!(captured.snapshot.skipped.is_empty());
    }

    #[test]
    fn pinned_reads_serve_spawn_time_rules_after_the_parent_changed_them() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "AGENTS.md", "always: rule V1\n");
        let captured = EnvSnapshot::capture(d.path(), "env-a", 1).unwrap();
        // The parent's AGENTS.md changes AFTER the snapshot was taken.
        write(d.path(), "AGENTS.md", "always: rule V2\n");
        let pinned = Instructions::from_snapshot(&captured.snapshot, &captured.content).unwrap();
        assert_eq!(pinned.epoch().as_u64(), captured.snapshot.instruction_epoch);
        let active = pinned.active_for("anything", &[]);
        assert!(
            active.iter().any(|i| i.content.contains("rule V1")),
            "the pinned tree must still serve the spawn-time rule: {active:?}"
        );
        assert!(
            !active.iter().any(|i| i.content.contains("rule V2")),
            "later parent changes must never bleed into the pinned tree"
        );
        // The live loader sees the new rule (its own epoch moved) — exactly
        // the drift a pinned read must refuse to take silently.
        let live = Instructions::load(d.path()).unwrap();
        assert!(live.active_for("x", &[])[0].content.contains("rule V2"));
        assert_ne!(live.epoch().as_u64(), captured.snapshot.instruction_epoch);
    }

    #[test]
    fn two_snapshots_of_different_epochs_stay_consistent_independently() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "AGENTS.md", "always: epoch-1 rules\n");
        let a = EnvSnapshot::capture(d.path(), "env-a", 1).unwrap();
        write(d.path(), "AGENTS.md", "always: epoch-2 rules\n");
        let b = EnvSnapshot::capture(d.path(), "env-b", 2).unwrap();
        assert_ne!(
            a.snapshot.instruction_epoch, b.snapshot.instruction_epoch,
            "different rules must flip the snapshot epoch"
        );
        let pa = Instructions::from_snapshot(&a.snapshot, &a.content).unwrap();
        let pb = Instructions::from_snapshot(&b.snapshot, &b.content).unwrap();
        let va = pa.active_for("x", &[]);
        let vb = pb.active_for("x", &[]);
        assert!(va[0].content.contains("epoch-1"));
        assert!(vb[0].content.contains("epoch-2"));
        assert_eq!(pa.epoch().as_u64(), a.snapshot.instruction_epoch);
        assert_eq!(pb.epoch().as_u64(), b.snapshot.instruction_epoch);
    }

    #[test]
    fn load_at_epoch_refuses_drift_loudly_never_silently() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "AGENTS.md", "always: v1\n");
        let e0 = Instructions::load(d.path()).unwrap().epoch();
        write(d.path(), "AGENTS.md", "always: v2\n");
        let err = Instructions::load_at_epoch(d.path(), e0).expect_err("epoch mismatch");
        assert!(matches!(
            err,
            RulesLoadError::EpochMismatch { expected, actual }
                if expected == e0 && actual != e0
        ));
        // An unchanged tree still loads at its epoch.
        let e1 = Instructions::load(d.path()).unwrap().epoch();
        assert_eq!(
            e1,
            Instructions::load_at_epoch(d.path(), e1).unwrap().epoch()
        );
    }

    #[test]
    fn pinned_reads_refuse_missing_or_tampered_content() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "AGENTS.md", "always: rule\n");
        let captured = EnvSnapshot::capture(d.path(), "env-a", 1).unwrap();
        // Missing content: loud, typed — never a skip.
        let mut partial = captured.content.clone();
        partial.remove("AGENTS.md");
        let err = Instructions::from_snapshot(&captured.snapshot, &partial)
            .expect_err("missing content refused");
        assert!(matches!(err, EnvSnapshotError::Missing(_)), "{err:?}");
        // Tampered bytes: loud, typed — never a silent substitution.
        let mut tampered = captured.content.clone();
        tampered.insert("AGENTS.md".into(), "always: EVIL\n".into());
        let err = Instructions::from_snapshot(&captured.snapshot, &tampered)
            .expect_err("tampered content refused");
        assert!(matches!(err, EnvSnapshotError::Tampered(_)), "{err:?}");
        // An extra content entry shadows nothing: the snapshot's path set
        // is authoritative.
        let mut extra = captured.content.clone();
        extra.insert("nope.md".into(), "never a rule\n".into());
        let pinned = Instructions::from_snapshot(&captured.snapshot, &extra).unwrap();
        assert_eq!(pinned.active_for("x", &[]).len(), 1);
    }

    #[test]
    fn hostile_snapshot_paths_are_malformed_never_rules() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "AGENTS.md", "always: ok\n");
        let mut captured = EnvSnapshot::capture(d.path(), "env-a", 1).unwrap();
        assert_eq!(
            kind_for_rel_path(std::path::Path::new("AGENTS.md")),
            Some(RuleSourceKind::AgentsMd)
        );
        assert_eq!(
            kind_for_rel_path(std::path::Path::new(".cursor/rules/x.mdc")),
            Some(RuleSourceKind::CursorRules)
        );
        assert_eq!(kind_for_rel_path(std::path::Path::new("evil/x.md")), None);
        // A captured path set that could never come from discovery is a
        // malformed snapshot.
        captured.snapshot.workspace_paths.insert(
            "evil/x.md".into(),
            EnvFileRecord {
                rules_hash: 1,
                bytes_hash: 1,
            },
        );
        captured.content.insert("evil/x.md".into(), "x".into());
        let err = Instructions::from_snapshot(&captured.snapshot, &captured.content)
            .expect_err("malformed path refused");
        assert!(matches!(err, EnvSnapshotError::Malformed(_)), "{err:?}");
    }

    #[test]
    fn capture_is_bounded_with_typed_oversized_never_truncation() {
        let d = tempfile::tempdir().unwrap();
        // Path cap: MAX_SNAPSHOT_PATHS + 1 rule files.
        for i in 0..(MAX_SNAPSHOT_PATHS + 1) {
            write(
                d.path(),
                &format!(".cursor/rules/f{i:04}.mdc"),
                "# Scope: k{i}\nrule body\n",
            );
        }
        let err = EnvSnapshot::capture(d.path(), "env-a", 1).unwrap_err();
        assert!(matches!(err, EnvSnapshotError::Oversized(_)), "{err:?}");
        assert!(err.to_string().contains("rule files"), "{err}");
        // Total-byte cap: fewer files, each read at the per-file bound, so
        // 10 x 64 KiB blows the total bound.
        let d2 = tempfile::tempdir().unwrap();
        let body = "b".repeat(MAX_RULE_BYTES);
        for i in 0..10 {
            write(d2.path(), &format!(".cursor/rules/g{i:02}.mdc"), &body);
        }
        let err2 = EnvSnapshot::capture(d2.path(), "env-a", 1).unwrap_err();
        assert!(matches!(err2, EnvSnapshotError::Oversized(_)), "{err2:?}");
        assert!(err2.to_string().contains("total bytes"), "{err2}");
        // Hostile ids and roots refuse loudly.
        assert!(EnvSnapshot::capture(d.path(), "", 1).is_err());
        assert!(EnvSnapshot::capture(d.path(), "a/b", 1).is_err());
        assert!(EnvSnapshot::capture(d.path(), "caf\u{e9}", 1).is_err());
        assert!(EnvSnapshot::capture(&d.path().join("missing"), "env-a", 1).is_err());
    }

    #[test]
    fn snapshot_hashes_are_stable_across_copies_and_serde_shaped() {
        // The snapshot types must be serde-ready (the orchestrator persists
        // them as JSON rows); the compile-time bound is the contract.
        fn assert_serde<T: serde::Serialize + serde::de::DeserializeOwned>() {}
        assert_serde::<EnvSnapshot>();
        assert_serde::<EnvFileRecord>();
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "AGENTS.md", "always: durable rules\n");
        let captured = EnvSnapshot::capture(d.path(), "env-a", 42).unwrap();
        // A second capture of the same unchanged tree carries identical
        // hashes — the dedup identity of unchanged envs.
        let again = EnvSnapshot::capture(d.path(), "env-b", 43).unwrap();
        assert_eq!(
            again.snapshot.instruction_epoch,
            captured.snapshot.instruction_epoch
        );
        assert_eq!(
            again.snapshot.workspace_paths,
            captured.snapshot.workspace_paths
        );
        assert_eq!(again.content, captured.content);
        let pinned = Instructions::from_snapshot(&captured.snapshot, &captured.content).unwrap();
        assert!(pinned.active_for("x", &[])[0]
            .content
            .contains("durable rules"));
    }

    // ------------------------------------------------- per-workspace resolver (P0-32)

    /// Fake durable-root provider: id -> root, mirroring the daemon
    /// workspace table (no CWD, no config defaults anywhere).
    struct MapRoots(HashMap<u64, PathBuf>);

    impl WorkspaceRootProvider for MapRoots {
        fn workspace_root(&self, workspace_id: u64) -> Option<PathBuf> {
            self.0.get(&workspace_id).cloned()
        }
    }

    fn resolver_with(roots: HashMap<u64, PathBuf>, cap: usize) -> InstructionResolver {
        InstructionResolver::new(Arc::new(MapRoots(roots)), cap)
    }

    #[test]
    fn resolver_uses_durable_roots_and_returns_empty_for_rootless_sessions() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "AGENTS.md", "always: durable-root rules\n");
        let resolver = resolver_with(HashMap::from([(7, d.path().to_path_buf())]), 8);
        // Unknown / rootless workspace ids: Empty — documented, never an
        // error and never the process CWD.
        assert!(resolver.resolve(1, None).unwrap().is_empty());
        assert!(resolver.resolve(999, None).unwrap().is_empty());
        // The durable root is served with its content.
        let loaded = resolver.resolve(7, None).unwrap();
        let active = loaded.active_for("anything", &[]);
        assert!(active
            .iter()
            .any(|i| i.content.contains("durable-root rules")));
        assert!(loaded.epoch().is_some());
        // A hostile tree at the durable root is a typed error.
        let hostile = tempfile::tempdir().unwrap();
        write(hostile.path(), "AGENTS.md", &"h".repeat(MAX_RULE_BYTES + 1));
        let r2 = resolver_with(HashMap::from([(8, hostile.path().to_path_buf())]), 8);
        assert!(matches!(
            r2.resolve(8, None),
            Err(RulesLoadError::Oversized(_))
        ));
    }

    #[test]
    fn resolver_pinned_epoch_serves_the_old_tree_after_a_rewrite() {
        // (d) Resolver cache semantics: after AGENTS.md is replaced, a
        // re-resolve returns the NEW content with a NEW epoch, while a
        // request pinned to the OLD epoch still sees the OLD content (the
        // tree the old env snapshot was taken from).
        let d = tempfile::tempdir().unwrap();
        let roots = HashMap::from([(1, d.path().to_path_buf())]);
        let resolver = resolver_with(roots, 8);
        write(d.path(), "AGENTS.md", "always: rule V1\n");
        let v1 = resolver.resolve(1, None).unwrap();
        let e1 = v1.epoch().unwrap();
        assert!(v1.active_for("x", &[])[0].content.contains("V1"));
        write(d.path(), "AGENTS.md", "always: rule V2\n");
        let v2 = resolver.resolve(1, None).unwrap();
        let e2 = v2.epoch().unwrap();
        assert_ne!(e1, e2, "a rewrite must move the durable epoch");
        assert!(v2.active_for("x", &[])[0].content.contains("V2"));
        // Pinned to the OLD epoch: the resolver serves the cached OLD tree,
        // exactly the spawn-time content an env snapshot of epoch e1 holds.
        let pinned_old = resolver.resolve(1, Some(e1)).unwrap();
        assert_eq!(pinned_old.epoch(), Some(e1));
        assert!(
            pinned_old.active_for("x", &[])[0].content.contains("V1"),
            "an old env snapshot must still see the old epoch content"
        );
        // A fresh load still matches epoch e2 (no cross-contamination).
        assert_eq!(resolver.resolve(1, Some(e2)).unwrap().epoch(), Some(e2));
    }

    #[test]
    fn resolver_refuses_pinned_epochs_it_cannot_serve_after_eviction() {
        // Once the old tree is evicted from the bounded cache, a pinned
        // request for the evicted epoch cannot reconstruct yesterday's
        // content from the live filesystem — it refuses loudly instead of
        // silently serving today's rules as yesterday's.
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "AGENTS.md", "always: rule V1\n");
        // cap 1: resolving the rewritten tree evicts the (root, e1) entry.
        let resolver = resolver_with(HashMap::from([(1, d.path().to_path_buf())]), 1);
        let e1 = resolver.resolve(1, None).unwrap().epoch().unwrap();
        write(d.path(), "AGENTS.md", "always: rule V2\n");
        assert!(resolver.resolve(1, None).is_ok(), "new epoch loads");
        assert_eq!(resolver.cache_len(), 1, "only the newest epoch is cached");
        let err = resolver.resolve(1, Some(e1)).unwrap_err();
        assert!(
            matches!(
                err,
                RulesLoadError::EpochMismatch { expected, actual }
                    if expected == e1 && actual != e1
            ),
            "{err:?}"
        );
    }

    #[test]
    fn resolver_cache_stays_bounded_under_many_roots() {
        // (e) Cache boundedness: 500 distinct roots must keep the cache
        // under its cap, evicting LRU entries (oldest first).
        let base = tempfile::tempdir().unwrap();
        let mut roots = HashMap::new();
        for i in 0..500u64 {
            let sub = base.path().join(format!("root-{i}"));
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join("AGENTS.md"), format!("always: rules of {i}\n")).unwrap();
            roots.insert(i, sub);
        }
        let resolver = resolver_with(roots, 16);
        // Resolve every root; afterwards the cache must still be under its
        // cap, and the resolver never grew without bound.
        for i in 0..500u64 {
            let loaded = resolver.resolve(i, None).unwrap();
            assert!(loaded.active_for("x", &[])[0]
                .content
                .contains(&format!("rules of {i}")));
            assert!(
                resolver.cache_len() <= resolver.cache_cap(),
                "never unbounded"
            );
        }
        assert_eq!(resolver.cache_len(), 16, "full LRU under cap");
        // The most recent roots are cached (LRU): a re-resolve of an old
        // evicted root re-loads from disk with identical content (idempotent
        // resolution — content equality, not cache presence).
        let again = resolver.resolve(0, None).unwrap();
        assert!(again.active_for("x", &[])[0].content.contains("rules of 0"));
        assert!(resolver.cache_len() <= resolver.cache_cap());
    }

    // ------------------------------ rooted discovery / workspace escape
    // These tests attempt to break the loader with adversarial checkouts:
    // rule slots pointing at outside files/dirs through symlinks, entries
    // swapped between discovery and read, and special files. Honest trees
    // must stay byte-identical (the golden assembly test).

    struct ReadSeamClear;
    impl Drop for ReadSeamClear {
        fn drop(&mut self) {
            *READ_SEAM.lock().unwrap_or_else(|p| p.into_inner()) = None;
        }
    }

    fn install_read_seam(
        f: impl Fn(&Path) + Send + 'static,
    ) -> (std::sync::MutexGuard<'static, ()>, ReadSeamClear) {
        let guard = READ_SEAM_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Keyed to the installing thread: the seam is process-global, and
        // unrelated tests run concurrently in this binary, so only the
        // test's own `Instructions::load` call may fire the swap.
        let installer = std::thread::current().id();
        *READ_SEAM
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Box::new(move |rel| {
            if std::thread::current().id() == installer {
                f(rel);
            }
        }));
        (guard, ReadSeamClear)
    }

    #[test]
    fn rooted_discovery_returns_workspace_relative_paths_only() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "AGENTS.md", "root rule\n");
        write(d.path(), ".cursor/rules/sub/x.mdc", "scoped rule\n");
        let discovered = discover_rule_files(d.path());
        assert_eq!(discovered.len(), 2);
        for (_, p) in &discovered {
            assert!(
                p.is_relative(),
                "discovery must never queue an absolute path: {p:?}"
            );
        }
        let rels: Vec<String> = discovered.iter().map(|(_, p)| path_str(p)).collect();
        assert_eq!(rels, vec!["AGENTS.md", ".cursor/rules/sub/x.mdc"]);
    }

    #[test]
    fn honest_tree_loads_byte_identically_through_rooted_discovery() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "AGENTS.md", "agents-rule\n");
        write(d.path(), "FAKTOR.md", "faktor-rule\n");
        write(d.path(), "CLAUDE.md", "claude-rule\n");
        write(d.path(), "GEMINI.md", "gemini-rule\n");
        write(
            d.path(),
            ".github/copilot-instructions.md",
            "copilot-rule\n",
        );
        write(d.path(), ".windsurfrules", "windsurf-legacy-rule\n");
        write(d.path(), ".windsurf/rules.md", "windsurf-rule\n");
        write(d.path(), ".cursor/rules/top.mdc", "cursor-top\n");
        write(d.path(), ".cursor/rules/nested/deep.md", "cursor-deep\n");
        write(d.path(), ".faktor/rules/native.md", "native-rule\n");
        write(d.path(), ".continue/rules/cont.md", "continue-rule\n");
        write(
            d.path(),
            ".faktor/legacy/import.md",
            "# Scope: legacy\nlegacy-rule\n",
        );
        let live = Instructions::load(d.path()).unwrap();
        assert!(live.skipped().is_empty(), "{:?}", live.skipped());
        // Byte-exact assembled material: priority, path, scope, content.
        let assembled: String = live
            .rules
            .iter()
            .map(|r| format!("{}|{}|{}|{}\n", r.priority, r.path, r.scope, r.content))
            .collect();
        let expected_assembled = "\
100|.faktor/rules/native.md||native-rule\n\n\
100|FAKTOR.md||faktor-rule\n\n\
90|AGENTS.md||agents-rule\n\n\
80|CLAUDE.md||claude-rule\n\n\
70|GEMINI.md||gemini-rule\n\n\
60|.github/copilot-instructions.md|.github|copilot-rule\n\n\
50|.cursor/rules/nested/deep.md|nested|cursor-deep\n\n\
50|.cursor/rules/top.mdc||cursor-top\n\n\
40|.windsurf/rules.md|.windsurf|windsurf-rule\n\n\
40|.windsurfrules||windsurf-legacy-rule\n\n\
30|.continue/rules/cont.md||continue-rule\n\n\
10|.faktor/legacy/import.md||# Scope: legacy\nlegacy-rule\n\n";
        assert_eq!(assembled, expected_assembled);
        // Public discovery produces the SAME ordered, relative paths.
        let discovered: Vec<(RuleSourceKind, String)> = discover_rule_files(d.path())
            .into_iter()
            .map(|(k, p)| (k, path_str(&p)))
            .collect();
        let expected_discovered: Vec<(RuleSourceKind, String)> = live
            .rules
            .iter()
            .map(|r| (r.source, r.path.clone()))
            .collect();
        assert_eq!(discovered, expected_discovered);
        // Snapshot capture sees the identical environment: same epoch,
        // same relative paths, same bytes, and the pinned tree re-derives
        // the same epoch.
        let captured = EnvSnapshot::capture(d.path(), "env-honest", 7).unwrap();
        assert!(captured.snapshot.skipped.is_empty());
        assert_eq!(captured.snapshot.instruction_epoch, live.epoch().as_u64());
        assert_eq!(captured.content.len(), live.rules.len());
        for r in &live.rules {
            assert_eq!(
                captured.content.get(&r.path).map(String::as_str),
                Some(r.content.as_str()),
                "captured bytes must equal live rule bytes for {}",
                r.path
            );
        }
        let pinned = Instructions::from_snapshot(&captured.snapshot, &captured.content).unwrap();
        assert_eq!(pinned.epoch(), live.epoch());
        assert_eq!(
            pinned.active_for("fix the api server", &["nested/App.tsx".into()]),
            live.active_for("fix the api server", &["nested/App.tsx".into()])
        );
    }

    #[cfg(unix)]
    #[test]
    fn authority_rule_symlink_is_loudly_refused_and_outside_never_ingested() {
        for name in ["AGENTS.md", "FAKTOR.md", "CLAUDE.md"] {
            let outside = tempfile::tempdir().unwrap();
            let secret = format!("OUTSIDE {name} SECRET\n");
            std::fs::write(outside.path().join("secret.md"), &secret).unwrap();
            let d = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(outside.path().join("secret.md"), d.path().join(name))
                .unwrap();
            let err = Instructions::load(d.path()).expect_err("authority symlink refused");
            assert!(
                matches!(err, RulesLoadError::Unreadable(_)),
                "{name}: {err:?}"
            );
            let msg = err.to_string();
            assert!(msg.contains(name), "{msg}");
            assert!(msg.contains("unsafe entry"), "{msg}");
            // The snapshot capture refuses the same tree loudly too.
            let err2 = EnvSnapshot::capture(d.path(), "env-a", 1)
                .expect_err("capture refuses authority symlink");
            assert!(matches!(err2, EnvSnapshotError::Malformed(_)), "{err2:?}");
            assert!(err2.to_string().contains(name), "{err2}");
            // The outside bytes are intact and were never ingested.
            assert_eq!(
                std::fs::read_to_string(outside.path().join("secret.md")).unwrap(),
                secret
            );
        }
        // An authority SLOT that is a directory (not just a link) is loud.
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("AGENTS.md")).unwrap();
        let err = Instructions::load(d.path()).expect_err("authority dir refused");
        assert!(matches!(err, RulesLoadError::Unreadable(_)), "{err:?}");
        assert!(err.to_string().contains("unsafe entry"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_root_is_refused_not_followed() {
        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "AGENTS.md", "OUTSIDE ROOT SECRET\n");
        let base = tempfile::tempdir().unwrap();
        let link = base.path().join("ws");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let err = Instructions::load(&link).expect_err("symlinked root refused");
        assert!(matches!(err, RulesLoadError::Unreadable(_)), "{err:?}");
        assert!(EnvSnapshot::capture(&link, "env-a", 1).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn optional_rule_symlink_escaping_with_dotdot_is_surfaced_skip_never_followed() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("ws");
        let outside = base.path().join("outside");
        std::fs::create_dir_all(root.join(".faktor/rules")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.md"), "OUTSIDE IMPORT SECRET\n").unwrap();
        // Literal `../../../outside/secret.md` from `.faktor/rules` resolves
        // outside the workspace root.
        std::os::unix::fs::symlink(
            "../../../outside/secret.md",
            root.join(".faktor/rules/foo.md"),
        )
        .unwrap();
        let discovered = discover_rule_files(&root);
        assert!(
            discovered.is_empty(),
            "a link is never a discovered rule file: {discovered:?}"
        );
        let ins = Instructions::load(&root).unwrap();
        let skip = ins
            .skipped()
            .iter()
            .find(|s| s.path == ".faktor/rules/foo.md")
            .expect("surfaced skip for the linked import");
        assert!(skip.reason.contains("unsafe entry"), "{skip:?}");
        assert!(ins.rules.is_empty());
        assert!(!ins
            .active_for("anything", &[])
            .iter()
            .any(|i| i.content.contains("OUTSIDE IMPORT SECRET")));
        // The snapshot capture surfaces the same refusal and never ingests.
        let captured = EnvSnapshot::capture(&root, "env-a", 1).unwrap();
        assert!(captured
            .snapshot
            .skipped
            .iter()
            .any(|s| s.path == ".faktor/rules/foo.md"));
        assert!(!captured
            .snapshot
            .workspace_paths
            .contains_key(".faktor/rules/foo.md"));
        assert!(captured.content.is_empty());
        let pinned = Instructions::from_snapshot(&captured.snapshot, &captured.content).unwrap();
        assert!(pinned.rules.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_convention_directory_is_refused_at_the_component_walk() {
        for relative_target in [false, true] {
            let base = tempfile::tempdir().unwrap();
            let root = base.path().join("ws");
            let outside = base.path().join("outside");
            std::fs::create_dir_all(root.join(".cursor")).unwrap();
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("evil.mdc"), "OUTSIDE DIR RULE\n").unwrap();
            let target = if relative_target {
                PathBuf::from("../../outside")
            } else {
                outside.clone()
            };
            std::os::unix::fs::symlink(&target, root.join(".cursor/rules")).unwrap();
            let ins = Instructions::load(&root).unwrap();
            let skip = ins
                .skipped()
                .iter()
                .find(|s| s.path == ".cursor/rules")
                .expect("surfaced refusal for the linked convention dir");
            assert!(skip.reason.contains("unsafe entry"), "{skip:?}");
            assert!(ins.rules.is_empty());
            assert!(!ins
                .active_for("anything", &[])
                .iter()
                .any(|i| i.content.contains("OUTSIDE DIR RULE")));
            let captured = EnvSnapshot::capture(&root, "env-a", 1).unwrap();
            assert!(captured
                .snapshot
                .skipped
                .iter()
                .any(|s| s.path == ".cursor/rules"));
            assert!(captured.snapshot.workspace_paths.is_empty());
        }
    }

    #[cfg(unix)]
    #[test]
    fn nested_symlinked_directory_is_refused_and_never_descended() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("ws");
        let outside = base.path().join("outside");
        write(&root, ".faktor/rules/honest.md", "honest-rule\n");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("nested.mdc"), "OUTSIDE NESTED RULE\n").unwrap();
        std::os::unix::fs::symlink(&outside, root.join(".faktor/rules/sub")).unwrap();
        let ins = Instructions::load(&root).unwrap();
        assert!(ins
            .rules
            .iter()
            .any(|r| r.path == ".faktor/rules/honest.md"));
        let skip = ins
            .skipped()
            .iter()
            .find(|s| s.path == ".faktor/rules/sub")
            .expect("nested dir refusal surfaced");
        assert!(skip.reason.contains("unsafe entry"), "{skip:?}");
        assert!(!ins
            .rules
            .iter()
            .any(|r| r.content.contains("OUTSIDE NESTED")));
        let captured = EnvSnapshot::capture(&root, "env-a", 1).unwrap();
        assert!(captured
            .snapshot
            .workspace_paths
            .contains_key(".faktor/rules/honest.md"));
        assert!(captured
            .snapshot
            .skipped
            .iter()
            .any(|s| s.path == ".faktor/rules/sub"));
        assert!(!captured
            .snapshot
            .workspace_paths
            .keys()
            .any(|k| k.contains("nested")));
    }

    #[cfg(unix)]
    #[test]
    fn special_file_in_rule_tree_is_surfaced_and_never_opened() {
        // A UNIX socket named like a rule must never be opened (a read would
        // block/fail): classification refuses it before the open.
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".faktor/rules")).unwrap();
        let _sock =
            std::os::unix::net::UnixListener::bind(d.path().join(".faktor/rules/pipe.md")).unwrap();
        let ins = Instructions::load(d.path()).unwrap();
        assert!(ins.rules.is_empty());
        let skip = ins
            .skipped()
            .iter()
            .find(|s| s.path == ".faktor/rules/pipe.md")
            .expect("special file surfaced");
        assert!(skip.reason.contains("unsafe entry"), "{skip:?}");
    }

    #[cfg(unix)]
    #[test]
    fn seam_swap_of_authority_file_between_enumeration_and_read_is_loud() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.md"), "OUTSIDE SEAM SECRET\n").unwrap();
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "AGENTS.md", "honest authority\n");
        let swap_root = d.path().to_path_buf();
        let swap_target = outside.path().join("secret.md");
        let (_lock, _clear) = install_read_seam(move |rel| {
            if rel == Path::new("AGENTS.md") {
                let _ = std::fs::remove_file(swap_root.join("AGENTS.md"));
                let _ = std::os::unix::fs::symlink(&swap_target, swap_root.join("AGENTS.md"));
            }
        });
        let err = Instructions::load(d.path()).expect_err("swapped authority file refused");
        assert!(matches!(err, RulesLoadError::Unreadable(_)), "{err:?}");
        let msg = err.to_string();
        assert!(
            msg.contains("AGENTS.md") && msg.contains("unsafe entry"),
            "{msg}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn seam_swap_of_optional_rule_between_enumeration_and_read_is_surfaced_skip() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.md"), "OUTSIDE SEAM IMPORT\n").unwrap();
        let d = tempfile::tempdir().unwrap();
        write(d.path(), ".faktor/rules/note.md", "honest note\n");
        let swap_root = d.path().to_path_buf();
        let swap_target = outside.path().join("secret.md");
        let (_lock, _clear) = install_read_seam(move |rel| {
            if rel == Path::new(".faktor/rules/note.md") {
                let _ = std::fs::remove_file(swap_root.join(".faktor/rules/note.md"));
                let _ = std::os::unix::fs::symlink(
                    &swap_target,
                    swap_root.join(".faktor/rules/note.md"),
                );
            }
        });
        let ins = Instructions::load(d.path()).unwrap();
        let skip = ins
            .skipped()
            .iter()
            .find(|s| s.path == ".faktor/rules/note.md")
            .expect("swapped optional rule surfaced");
        assert!(skip.reason.contains("unsafe entry"), "{skip:?}");
        assert!(!ins
            .rules
            .iter()
            .any(|r| r.content.contains("OUTSIDE SEAM IMPORT")));
        // The same window through snapshot capture is refused identically:
        // capture re-discovers, so the swapped entry is classified unsafe.
        let captured = EnvSnapshot::capture(d.path(), "env-a", 1).unwrap();
        assert!(captured.content.is_empty());
        assert!(!captured
            .snapshot
            .workspace_paths
            .contains_key(".faktor/rules/note.md"));
    }

    #[cfg(unix)]
    #[test]
    fn swap_after_discovery_never_reopens_the_stale_relative_path() {
        // Enumerate first (the relative path is the ONLY thing retained),
        // then swap the entry for an escaping symlink: the read resolves
        // through the admitted root with no-follow semantics and refuses.
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.md"), "OUTSIDE STALE SECRET\n").unwrap();
        let d = tempfile::tempdir().unwrap();
        write(d.path(), ".faktor/rules/note.md", "honest note\n");
        let before = discover_rule_files(d.path());
        assert_eq!(before.len(), 1);
        assert!(before[0].1.is_relative());
        std::fs::remove_file(d.path().join(".faktor/rules/note.md")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.md"),
            d.path().join(".faktor/rules/note.md"),
        )
        .unwrap();
        let rooted = RootedDir::open(d.path()).unwrap();
        let read = match read_rule_file(&rooted, Path::new(".faktor/rules/note.md"), MAX_RULE_BYTES)
        {
            RuleFileRead::Unreadable(err) => err,
            _ => panic!("stale-path swap must be refused, never followed"),
        };
        assert!(read.contains("unsafe entry"), "{read}");
        let ins = Instructions::load(d.path()).unwrap();
        assert!(!ins
            .active_for("anything", &[])
            .iter()
            .any(|i| i.content.contains("OUTSIDE STALE SECRET")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_reparse_rule_entries_are_refused_like_unix_links() {
        // Windows parity (statically reviewed; runs on the Windows lane):
        // the same rooted classification refuses a directory reparse point
        // (symlink/junction) at the convention directory before any descent.
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(outside.path().join("rules")).unwrap();
        std::fs::write(outside.path().join("rules/evil.mdc"), "OUTSIDE WIN RULE\n").unwrap();
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".cursor")).unwrap();
        if std::os::windows::fs::symlink_dir(
            outside.path().join("rules"),
            d.path().join(".cursor/rules"),
        )
        .is_err()
        {
            // Creating a symlink needs SeCreateSymbolicLinkPrivilege or
            // developer mode; the fs crate's Windows seam suite covers the
            // handle-anchored refusal on runners that grant it.
            return;
        }
        let ins = Instructions::load(d.path()).unwrap();
        assert!(ins.rules.is_empty());
        assert!(ins
            .skipped()
            .iter()
            .any(|s| s.path == ".cursor/rules" && s.reason.contains("unsafe entry")));
        assert!(!ins
            .rules
            .iter()
            .any(|r| r.content.contains("OUTSIDE WIN RULE")));
    }
}
