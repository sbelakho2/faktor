//! Shared content-freshness authority (P1 stale-source correctness).
//!
//! Index and the impact graph MUST reach the same freshness decision from the
//! same inputs. This crate is that one authority:
//!
//! * a persisted [`FileRecord`] carries the path, optional file identity,
//!   size, mtime and the BLAKE3 content digest;
//! * size/mtime/identity are only a **shortlist** (`needs_hash`): they can
//!   prove that a file is a suspect, never that unchanged bytes are unchanged;
//! * [`observe_path`] hashes a bounded number of bytes through an explicit
//!   cancellation boundary;
//! * [`classify`] compares a stored record with an observed one and NEVER
//!   reports clean without a digest on both sides (legacy metadata-only
//!   records are dirty, not clean);
//! * [`shard_of`]/[`SweepCursor`] provide the bounded rotating shard sweep a
//!   recovery pass (watcher overflow, lost events, periodic reconciliation)
//!   uses to eventually hash every material input.
//!
//! Metadata is never proof of unchanged bytes: a same-length, same-mtime
//! rewrite is invisible to `needs_hash` (so it is not a shortlist suspect) and
//! is found by the exact BLAKE3 sweep, which hashes every shard over time.

use std::fmt;
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use std::str::FromStr;

use faktor_core::cancellation::CancellationToken;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

/// Default number of recovery shards (stable path-hash assignment).
pub const DEFAULT_SHARDS: u32 = 8;

/// Canonical textual prefix of a BLAKE3 content digest.
pub const DIGEST_PREFIX: &str = "blake3:";

/// Bytes hashed per read slice. Cancellation is observed at this boundary.
const HASH_CHUNK_BYTES: usize = 64 * 1024;

/// A BLAKE3 content digest, serialized as `blake3:<64 lowercase hex>`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContentDigest([u8; 32]);

impl ContentDigest {
    /// BLAKE3 of the supplied bytes.
    pub fn of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        blake3::Hash::from_bytes(self.0).to_hex().to_string()
    }

    /// Parse the canonical `blake3:<hex>` spelling (raw 64-char hex is also
    /// accepted so a caller never has to guess the prefix).
    pub fn parse(raw: &str) -> Result<Self, DigestError> {
        let hex = raw.strip_prefix(DIGEST_PREFIX).unwrap_or(raw);
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(DigestError::Malformed);
        }
        let mut bytes = [0u8; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            let text = &hex[index * 2..index * 2 + 2];
            *byte = u8::from_str_radix(text, 16).map_err(|_| DigestError::Malformed)?;
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for ContentDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{DIGEST_PREFIX}{}", self.to_hex())
    }
}

impl fmt::Debug for ContentDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl FromStr for ContentDigest {
    type Err = DigestError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse(raw)
    }
}

impl Serialize for ContentDigest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ContentDigest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(de::Error::custom)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DigestError {
    #[error("content digest must be blake3:<64 hex characters>")]
    Malformed,
}

/// Stable file identity when the platform exposes one (device+inode on Unix,
/// volume+file index on Windows). `None` when the platform or filesystem does
/// not provide it; identity is then simply not part of the shortlist.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FileIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inode: Option<u64>,
}

impl FileIdentity {
    pub fn of(meta: &fs::Metadata) -> Option<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Some(Self {
                device: Some(meta.dev()),
                inode: Some(meta.ino()),
            })
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            let device = meta.volume_serial_number().map(u64::from);
            let inode = meta.file_index();
            (device.is_some() || inode.is_some()).then_some(Self { device, inode })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = meta;
            None
        }
    }
}

/// One persisted freshness record. `digest: None` is a LEGACY
/// metadata-only entry: it is always dirty (the bytes were never proven), not
/// clean. New records always carry a digest.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FileRecord {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<FileIdentity>,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub modified_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<ContentDigest>,
}

impl FileRecord {
    /// Metadata-only observation (never proof of content).
    pub fn from_metadata(path: impl Into<String>, meta: &fs::Metadata) -> Self {
        Self {
            path: path.into(),
            identity: FileIdentity::of(meta),
            size: meta.len(),
            modified_ms: modified_ms_of(meta),
            digest: None,
        }
    }

    pub fn with_digest(mut self, digest: ContentDigest) -> Self {
        self.digest = Some(digest);
        self
    }

    /// True when the cheap metadata signals match (identity only when both
    /// sides expose it).
    pub fn metadata_matches(&self, other: &Self) -> bool {
        self.size == other.size
            && self.modified_ms == other.modified_ms
            && match (self.identity, other.identity) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
    }
}

/// Whether a stored record and a current metadata observation make the file a
/// hash suspect. Equal metadata is a SHORTLIST decision only; it is never a
/// clean verdict (see [`classify`]).
pub fn needs_hash(previous: Option<&FileRecord>, current: &FileRecord) -> bool {
    match previous {
        None => true,
        Some(previous) => previous.digest.is_none() || !previous.metadata_matches(current),
    }
}

/// Why a record is not clean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirtyReason {
    ContentChanged,
    IdentityChanged,
    Missing,
    /// Stored entry predates content digests: unproven bytes, never clean.
    LegacyMetadataOnly,
    Unreadable,
    TooLarge,
}

/// Three-way freshness outcome. `Unverified` is NOT clean: the caller may
/// neither serve it as current nor drop a change event on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Clean,
    Dirty(DirtyReason),
    Unverified,
}

/// Compare a stored record against a freshly hashed observation.
///
/// A legacy record without a digest is dirty; an observation without a digest
/// is unverified; only equal digests (and unchanged identity when both sides
/// expose one) are clean.
pub fn classify(previous: &FileRecord, current: &FileRecord) -> Freshness {
    if previous.digest.is_none() {
        return Freshness::Dirty(DirtyReason::LegacyMetadataOnly);
    }
    if let (Some(a), Some(b)) = (previous.identity, current.identity) {
        if a != b {
            return Freshness::Dirty(DirtyReason::IdentityChanged);
        }
    }
    match (previous.digest, current.digest) {
        (Some(a), Some(b)) if a == b => Freshness::Clean,
        (Some(_), Some(_)) => Freshness::Dirty(DirtyReason::ContentChanged),
        _ => Freshness::Unverified,
    }
}

/// Bounded observation of one file. Every branch is explicit; a read failure
/// can never masquerade as "unchanged".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation {
    Hashed { record: FileRecord, bytes: u64 },
    Missing,
    NotRegular,
    TooLarge { size: u64 },
    Unreadable { reason: String },
    Cancelled,
}

/// Read and BLAKE3-hash `relative` under `root`, bounded by
/// `max_file_bytes`. Cancellation is observed before the open and between
/// every read chunk, so a huge or slow file cannot outlive a cancelled
/// operation.
pub fn observe_path(
    root: &Path,
    relative: &str,
    max_file_bytes: u64,
    cancel: Option<&CancellationToken>,
) -> Observation {
    if cancel.is_some_and(CancellationToken::is_cancelled) {
        return Observation::Cancelled;
    }
    let absolute = root.join(relative);
    let meta = match fs::symlink_metadata(&absolute) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Observation::Missing;
        }
        Err(error) => {
            return Observation::Unreadable {
                reason: error.to_string(),
            };
        }
    };
    if !meta.file_type().is_file() {
        return Observation::NotRegular;
    }
    let mut record = FileRecord::from_metadata(relative, &meta);
    if meta.len() > max_file_bytes {
        return Observation::TooLarge { size: meta.len() };
    }
    let file = match File::open(&absolute) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Observation::Missing;
        }
        Err(error) => {
            return Observation::Unreadable {
                reason: error.to_string(),
            };
        }
    };
    let mut reader = file.take(max_file_bytes.saturating_add(1));
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; HASH_CHUNK_BYTES];
    let mut bytes = 0u64;
    loop {
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return Observation::Cancelled;
        }
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                bytes = bytes.saturating_add(read as u64);
                if bytes > max_file_bytes {
                    return Observation::TooLarge { size: bytes };
                }
                hasher.update(&buffer[..read]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Observation::Unreadable {
                    reason: error.to_string(),
                };
            }
        }
    }
    record.digest = Some(ContentDigest::from_bytes(*hasher.finalize().as_bytes()));
    Observation::Hashed { record, bytes }
}

/// Stable shard assignment of one workspace-relative path.
pub fn shard_of(path: &str, shards: u32) -> u32 {
    let shards = shards.max(1);
    let hash = blake3::hash(path.as_bytes());
    u32::from(hash.as_bytes()[0]) % shards
}

/// Durable rotation position of an exact recovery sweep. A scan hashes at
/// most its budget of non-suspect files from `shard`, continuing strictly
/// after `after`; when the shard is exhausted the cursor advances, so every
/// material input is visited exactly once per full rotation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SweepCursor {
    #[serde(default)]
    pub shard: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

impl SweepCursor {
    pub fn new(shard: u32) -> Self {
        Self { shard, after: None }
    }

    /// True when `path` is the next unvisited member of the current shard.
    pub fn accepts(&self, path: &str, shards: u32) -> bool {
        shard_of(path, shards) == self.shard
            && self.after.as_deref().is_none_or(|after| path > after)
    }

    /// Record `path` as consumed within the current shard.
    pub fn advance_to(&mut self, path: &str) {
        self.after = Some(path.to_string());
    }

    /// Move to the next shard, restarting its scan position.
    pub fn next_shard(&mut self, shards: u32) {
        self.shard = (self.shard + 1) % shards.max(1);
        self.after = None;
    }
}

/// Milliseconds since the Unix epoch, 0 when unavailable.
pub fn modified_ms_of(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn record(path: &str, size: u64, modified_ms: i64, digest: ContentDigest) -> FileRecord {
        FileRecord {
            path: path.to_string(),
            identity: Some(FileIdentity {
                device: Some(1),
                inode: Some(2),
            }),
            size,
            modified_ms,
            digest: Some(digest),
        }
    }

    #[test]
    fn same_size_same_mtime_with_different_bytes_is_dirty() {
        let old = ContentDigest::of(b"pub fn old() {}");
        let new = ContentDigest::of(b"pub fn new() {}");
        assert_eq!(old.to_hex().len(), new.to_hex().len());
        let previous = record("src/lib.rs", 15, 7, old);
        let current = record("src/lib.rs", 15, 7, new);
        assert!(!needs_hash(Some(&previous), &current));
        assert_eq!(
            classify(&previous, &current),
            Freshness::Dirty(DirtyReason::ContentChanged)
        );
    }

    #[test]
    fn metadata_only_legacy_entry_is_dirty_not_clean() {
        let mut previous = record("src/lib.rs", 4, 1, ContentDigest::of(b"same"));
        previous.digest = None;
        let current = record("src/lib.rs", 4, 1, ContentDigest::of(b"same"));
        assert_eq!(
            classify(&previous, &current),
            Freshness::Dirty(DirtyReason::LegacyMetadataOnly)
        );
        assert!(needs_hash(Some(&previous), &current));
    }

    #[test]
    fn observation_without_digest_is_unverified_not_clean() {
        let previous = record("src/lib.rs", 4, 1, ContentDigest::of(b"same"));
        let mut current = previous.clone();
        current.digest = None;
        assert_eq!(classify(&previous, &current), Freshness::Unverified);
    }

    #[test]
    fn identity_change_is_dirty_even_when_bytes_match() {
        let digest = ContentDigest::of(b"same");
        let previous = record("src/lib.rs", 4, 1, digest);
        let mut current = previous.clone();
        current.identity = Some(FileIdentity {
            device: Some(1),
            inode: Some(99),
        });
        assert!(
            needs_hash(Some(&previous), &current),
            "identity is a shortlist suspect"
        );
        assert_eq!(
            classify(&previous, &current),
            Freshness::Dirty(DirtyReason::IdentityChanged)
        );
    }

    #[test]
    fn deletion_is_missing_and_never_clean() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = observe_path(dir.path(), "gone.rs", 1024, None);
        assert_eq!(outcome, Observation::Missing);
    }

    #[test]
    fn identical_rewrite_with_same_digest_is_clean() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        fs::write(&path, b"pub fn a() {}").unwrap();
        let meta = fs::metadata(&path).unwrap();
        let previous = FileRecord::from_metadata("a.rs", &meta)
            .with_digest(ContentDigest::of(b"pub fn a() {}"));
        let Observation::Hashed { record, .. } = observe_path(dir.path(), "a.rs", 1024, None)
        else {
            panic!("expected a hashed observation");
        };
        assert_eq!(classify(&previous, &record), Freshness::Clean);
    }

    #[test]
    fn oversized_and_unreadable_observations_are_explicit() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("big.bin"), vec![b'x'; 128]).unwrap();
        assert_eq!(
            observe_path(dir.path(), "big.bin", 64, None),
            Observation::TooLarge { size: 128 }
        );
        fs::create_dir(dir.path().join("sub")).unwrap();
        assert_eq!(
            observe_path(dir.path(), "sub", 64, None),
            Observation::NotRegular
        );
    }

    #[test]
    fn cancellation_is_observed_before_hashing() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), b"pub fn a() {}").unwrap();
        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(
            observe_path(dir.path(), "a.rs", 1024, Some(&token)),
            Observation::Cancelled
        );
    }

    #[test]
    fn digest_text_roundtrips_and_refuses_hostile_shapes() {
        let digest = ContentDigest::of(b"canonical");
        let text = digest.to_string();
        assert_eq!(ContentDigest::parse(&text).unwrap(), digest);
        assert_eq!(ContentDigest::parse(&digest.to_hex()).unwrap(), digest);
        for hostile in ["", "blake3:", "blake3:zz", "sha256:abcd", &"a".repeat(63)] {
            assert!(ContentDigest::parse(hostile).is_err(), "{hostile:?}");
        }
        let json = serde_json::to_string(&digest).unwrap();
        assert!(json.starts_with("\"blake3:"));
        assert_eq!(
            serde_json::from_str::<ContentDigest>(&json).unwrap(),
            digest
        );
    }

    #[test]
    fn sweep_cursor_rotates_and_visits_each_path_without_repeats() {
        let paths: Vec<String> = (0..64).map(|i| format!("f{i:03}")).collect();
        let mut cursor = SweepCursor::new(0);
        let mut seen = std::collections::BTreeSet::new();
        while seen.len() < paths.len() {
            let mut hashed = 0usize;
            let mut empty_shards = 0u32;
            while hashed < 3 && empty_shards < DEFAULT_SHARDS {
                let mut visited_any = false;
                for path in &paths {
                    if hashed >= 3 || seen.len() >= paths.len() {
                        break;
                    }
                    if cursor.accepts(path, DEFAULT_SHARDS) {
                        assert!(seen.insert(path.clone()), "a path must not repeat");
                        cursor.advance_to(path);
                        hashed += 1;
                        visited_any = true;
                    }
                }
                if !visited_any {
                    cursor.next_shard(DEFAULT_SHARDS);
                    empty_shards += 1;
                }
            }
        }
        assert_eq!(seen.len(), 64);
    }
}
