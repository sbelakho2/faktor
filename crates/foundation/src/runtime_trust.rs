//! Runtime executable trust (Rev8 S34/S35/S56, O-04/O-05).
//!
//! Trust is per EXACT executable path and machine. A recorded checksum
//! match is **not** signature verification: this daemon carries no platform
//! code-signing verifier, so every row reports `signatureState:
//! "unverified"` and says why. A mismatch refuses a start; an unrecorded
//! path needs explicit confirmation of that exact path.
//!
//! All hashing is bounded (256 MiB) and streamed, so a hostile enormous
//! path cannot exhaust memory.

#[cfg(unix)]
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
#[cfg(unix)]
use std::sync::Mutex;
#[cfg(unix)]
use std::time::SystemTime;

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::db::now_ms;
use crate::{FoundationDb, FoundationError};

pub const MAX_EXECUTABLE_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_PATH_BYTES: usize = 4_096;
pub const MAX_PUBLISHER_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustState {
    Unrecorded,
    Match,
    Mismatch,
}

impl TrustState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unrecorded => "unrecorded",
            Self::Match => "match",
            Self::Mismatch => "mismatch",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustRecord {
    pub executable_path: String,
    pub checksum: String,
    pub publisher: Option<String>,
    /// Always `unverified`: no code-signing verifier is wired.
    pub signature_state: String,
    pub signature_basis: String,
    pub confirmed_ms: i64,
    pub updated_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustReport {
    pub executable_path: String,
    pub state: TrustState,
    pub start_allowed: bool,
    pub recorded: Option<TrustRecord>,
    pub observed_checksum: Option<String>,
    pub observed_bytes: Option<u64>,
    pub signature_state: String,
    pub reason: String,
}

fn validate_path(raw: &str) -> Result<(), FoundationError> {
    if raw.is_empty() || raw.len() > MAX_PATH_BYTES || raw.contains('\u{0}') {
        return Err(FoundationError::Malformed(format!(
            "executablePath must be 1..={MAX_PATH_BYTES} bytes without NUL"
        )));
    }
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(FoundationError::Malformed(
            "executablePath must be absolute; a project-relative path can never identify a machine executable"
                .into(),
        ));
    }
    Ok(())
}

/// Stream a bounded digest of one regular file. Symlinks are resolved by the
/// caller's exact path string (`std::fs` opens whatever the path names); the
/// recorded path is the path that was confirmed, byte for byte.
pub fn digest_executable(raw: &str) -> Result<(String, u64), FoundationError> {
    validate_path(raw)?;
    let metadata = std::fs::metadata(raw)
        .map_err(|e| FoundationError::NotFound(format!("executable {raw:?}: {e}")))?;
    if !metadata.is_file() {
        return Err(FoundationError::Malformed(format!(
            "executable {raw:?} is not a regular file"
        )));
    }
    if metadata.len() > MAX_EXECUTABLE_BYTES {
        return Err(FoundationError::Oversized(format!(
            "executable {raw:?} is {} bytes; the bound is {MAX_EXECUTABLE_BYTES}",
            metadata.len()
        )));
    }
    let mut file = std::fs::File::open(raw)
        .map_err(|e| FoundationError::Store(format!("open executable {raw:?}: {e}")))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| FoundationError::Store(format!("read executable {raw:?}: {e}")))?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > MAX_EXECUTABLE_BYTES {
            return Err(FoundationError::Oversized(format!(
                "executable {raw:?} grew beyond {MAX_EXECUTABLE_BYTES} bytes while hashing"
            )));
        }
        hasher.update(&buffer[..read]);
    }
    Ok((hasher.finalize().to_hex().to_string(), total))
}

#[cfg(unix)]
const MAX_DIGEST_CACHE: usize = 8;

#[cfg(unix)]
#[derive(Clone, PartialEq, Eq)]
struct DigestFence {
    len: u64,
    inode: u64,
    modified: SystemTime,
}

#[cfg(unix)]
#[derive(Clone)]
struct CachedDigest {
    fence: DigestFence,
    digest: String,
    bytes: u64,
}

#[cfg(unix)]
fn insert_digest(
    cache: &mut HashMap<String, CachedDigest>,
    path: &str,
    fence: DigestFence,
    digest: String,
    bytes: u64,
) {
    if !cache.contains_key(path) && cache.len() >= MAX_DIGEST_CACHE {
        if let Some(oldest) = cache.keys().next().cloned() {
            cache.remove(&oldest);
        }
    }
    cache.insert(
        path.to_string(),
        CachedDigest {
            fence,
            digest,
            bytes,
        },
    );
}

#[cfg(unix)]
fn digest_fence(metadata: &std::fs::Metadata) -> DigestFence {
    use std::os::unix::fs::MetadataExt;
    DigestFence {
        len: metadata.len(),
        inode: metadata.ino(),
        modified: SystemTime::UNIX_EPOCH
            + std::time::Duration::new(
                metadata.ctime().max(0) as u64,
                metadata.ctime_nsec().clamp(0, 999_999_999) as u32,
            ),
    }
}

pub struct RuntimeTrustService {
    db: FoundationDb,
    #[cfg(unix)]
    digests: Mutex<HashMap<String, CachedDigest>>,
}

impl RuntimeTrustService {
    pub fn new(db: FoundationDb) -> Self {
        Self {
            db,
            #[cfg(unix)]
            digests: Mutex::new(HashMap::new()),
        }
    }

    fn digest(&self, path: &str, fresh: bool) -> Result<(String, u64), FoundationError> {
        validate_path(path)?;
        let metadata = std::fs::metadata(path)
            .map_err(|e| FoundationError::NotFound(format!("executable {path:?}: {e}")))?;
        if !metadata.is_file() {
            return Err(FoundationError::Malformed(format!(
                "executable {path:?} is not a regular file"
            )));
        }
        if metadata.len() > MAX_EXECUTABLE_BYTES {
            return Err(FoundationError::Oversized(format!(
                "executable {path:?} is {} bytes; the bound is {MAX_EXECUTABLE_BYTES}",
                metadata.len()
            )));
        }
        #[cfg(unix)]
        let fence = digest_fence(&metadata);
        #[cfg(unix)]
        if !fresh {
            let mut cache = self
                .digests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(cached) = cache.get(path) {
                if cached.fence == fence {
                    return Ok((cached.digest.clone(), cached.bytes));
                }
            }
            let (digest, bytes) = digest_executable(path)?;
            insert_digest(&mut cache, path, fence, digest.clone(), bytes);
            return Ok((digest, bytes));
        }
        #[cfg(not(unix))]
        let _ = fresh;
        let (digest, bytes) = digest_executable(path)?;
        #[cfg(unix)]
        {
            let mut cache = self
                .digests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            insert_digest(&mut cache, path, fence, digest.clone(), bytes);
        }
        Ok((digest, bytes))
    }

    pub fn record(&self, path: &str) -> Result<Option<TrustRecord>, FoundationError> {
        self.db.read("runtime trust get", |conn| {
            conn.query_row(
                "SELECT executable_path, checksum, publisher, signature_state,
                        confirmed_ms, updated_ms
                 FROM runtime_trust WHERE executable_path = ?1",
                params![path],
                map_record,
            )
            .optional()
            .map_err(|e| FoundationError::Store(e.to_string()))
        })
    }

    /// Confirm one exact executable path. The checksum is COMPUTED here,
    /// never accepted from the caller; a caller-supplied checksum could
    /// otherwise bless a file that changed after review.
    pub fn confirm(
        &self,
        path: &str,
        publisher: Option<&str>,
    ) -> Result<TrustRecord, FoundationError> {
        validate_path(path)?;
        if let Some(publisher) = publisher {
            if publisher.len() > MAX_PUBLISHER_BYTES || publisher.chars().any(|c| c.is_control()) {
                return Err(FoundationError::Malformed(format!(
                    "publisher must be <= {MAX_PUBLISHER_BYTES} bytes without control characters"
                )));
            }
        }
        let (checksum, _) = self.digest(path, true)?;
        let now = now_ms();
        self.db.write("runtime trust confirm", |conn| {
            conn.execute(
                "INSERT INTO runtime_trust(
                   executable_path, checksum, publisher, signature_state, confirmed_ms, updated_ms)
                 VALUES (?1, ?2, ?3, 'unverified', ?4, ?4)
                 ON CONFLICT(executable_path) DO UPDATE SET
                   checksum = excluded.checksum,
                   publisher = excluded.publisher,
                   signature_state = 'unverified',
                   confirmed_ms = excluded.confirmed_ms,
                   updated_ms = excluded.updated_ms",
                params![path, checksum, publisher, now],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            conn.query_row(
                "SELECT executable_path, checksum, publisher, signature_state,
                        confirmed_ms, updated_ms
                 FROM runtime_trust WHERE executable_path = ?1",
                params![path],
                map_record,
            )
            .map_err(|e| FoundationError::Store(e.to_string()))
        })
    }

    /// Compare the CURRENT bytes at the exact path with the recorded digest.
    /// A missing file, mismatched digest or absent record is explicit.
    pub fn check(&self, path: &str) -> TrustReport {
        self.check_with(path, false)
    }

    pub fn check_fresh(&self, path: &str) -> TrustReport {
        self.check_with(path, true)
    }

    fn check_with(&self, path: &str, fresh: bool) -> TrustReport {
        let recorded = match self.record(path) {
            Ok(record) => record,
            Err(e) => {
                return TrustReport {
                    executable_path: path.to_string(),
                    state: TrustState::Unrecorded,
                    start_allowed: false,
                    recorded: None,
                    observed_checksum: None,
                    observed_bytes: None,
                    signature_state: "unverified".into(),
                    reason: format!("trust store read failed: {e}"),
                }
            }
        };
        if recorded.is_none() && !fresh {
            return TrustReport {
                executable_path: path.to_string(),
                state: TrustState::Unrecorded,
                start_allowed: false,
                recorded: None,
                observed_checksum: None,
                observed_bytes: None,
                signature_state: "unverified".into(),
                reason: "this exact path has not been confirmed on this machine".into(),
            };
        }
        let (observed, bytes) = match self.digest(path, fresh) {
            Ok((digest, bytes)) => (Some(digest), Some(bytes)),
            Err(e) => {
                return TrustReport {
                    executable_path: path.to_string(),
                    state: TrustState::Unrecorded,
                    start_allowed: false,
                    recorded,
                    observed_checksum: None,
                    observed_bytes: None,
                    signature_state: "unverified".into(),
                    reason: format!("the executable cannot be read at this exact path: {e}"),
                }
            }
        };
        match &recorded {
            None => TrustReport {
                executable_path: path.to_string(),
                state: TrustState::Unrecorded,
                start_allowed: false,
                recorded: None,
                observed_checksum: observed,
                observed_bytes: bytes,
                signature_state: "unverified".into(),
                reason: "this exact path has not been confirmed on this machine".into(),
            },
            Some(record) if Some(record.checksum.as_str()) == observed.as_deref() => TrustReport {
                executable_path: path.to_string(),
                state: TrustState::Match,
                start_allowed: true,
                recorded,
                observed_checksum: observed,
                observed_bytes: bytes,
                signature_state: "unverified".into(),
                reason: "recorded checksum matches; this is not a signature verification claim"
                    .into(),
            },
            Some(_) => TrustReport {
                executable_path: path.to_string(),
                state: TrustState::Mismatch,
                start_allowed: false,
                recorded,
                observed_checksum: observed,
                observed_bytes: bytes,
                signature_state: "unverified".into(),
                reason: "the executable bytes changed since confirmation; refuse the start".into(),
            },
        }
    }
}

fn map_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<TrustRecord> {
    Ok(TrustRecord {
        executable_path: row.get(0)?,
        checksum: row.get(1)?,
        publisher: row.get(2)?,
        signature_state: row.get(3)?,
        signature_basis: "no_code_signing_verifier_wired".into(),
        confirmed_ms: row.get(4)?,
        updated_ms: row.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn service() -> (tempfile::TempDir, RuntimeTrustService) {
        let dir = tempfile::tempdir().unwrap();
        let db = FoundationDb::open(dir.path().join("foundation.db")).unwrap();
        (dir, RuntimeTrustService::new(db))
    }

    fn exe(dir: &Path, body: &[u8]) -> String {
        let path = dir.join("faktor-daemon");
        std::fs::write(&path, body).unwrap();
        path.to_string_lossy().to_string()
    }

    #[test]
    fn unrecorded_match_and_mismatch_are_exact_and_never_claim_signatures() {
        let (dir, svc) = service();
        let path = exe(dir.path(), b"v1");
        let unrecorded = svc.check(&path);
        assert_eq!(unrecorded.state, TrustState::Unrecorded);
        assert!(!unrecorded.start_allowed);
        assert_eq!(unrecorded.signature_state, "unverified");
        assert!(unrecorded.observed_checksum.is_none());
        let explicit = svc.check_fresh(&path);
        assert_eq!(explicit.state, TrustState::Unrecorded);
        assert!(explicit.observed_checksum.is_some());

        let record = svc.confirm(&path, Some("Faktor Release")).unwrap();
        assert_eq!(record.signature_state, "unverified");
        assert_eq!(record.signature_basis, "no_code_signing_verifier_wired");
        let matched = svc.check(&path);
        assert_eq!(matched.state, TrustState::Match);
        assert!(matched.start_allowed);
        assert!(!matched.reason.contains("verified"));

        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"tamper")
            .unwrap();
        let mismatch = svc.check(&path);
        assert_eq!(mismatch.state, TrustState::Mismatch);
        assert!(!mismatch.start_allowed);
        assert!(mismatch.recorded.is_some());
        // A different path is NOT covered by this exact-path confirmation.
        std::fs::create_dir_all(dir.path().join("other")).unwrap();
        let other = exe(&dir.path().join("other"), b"v1");
        assert_eq!(svc.check(&other).state, TrustState::Unrecorded);
    }

    #[cfg(unix)]
    #[test]
    fn same_length_rewrite_invalidates_the_status_digest_cache() {
        let (dir, svc) = service();
        let path = exe(dir.path(), b"aaaa");
        svc.confirm(&path, None).unwrap();
        assert_eq!(svc.check(&path).state, TrustState::Match);
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&path, b"bbbb").unwrap();
        std::fs::File::open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert_eq!(svc.check(&path).state, TrustState::Mismatch);
    }

    #[test]
    fn hostile_paths_and_oversized_files_are_refused() {
        let (dir, svc) = service();
        assert!(matches!(
            svc.confirm("relative/daemon", None),
            Err(FoundationError::Malformed(_))
        ));
        let long = format!("/{}", "a".repeat(MAX_PATH_BYTES));
        assert!(matches!(
            svc.confirm(&long, None),
            Err(FoundationError::Malformed(_))
        ));
        let path = exe(dir.path(), b"x");
        assert!(matches!(
            svc.confirm(&path, Some(&"p".repeat(MAX_PUBLISHER_BYTES + 1))),
            Err(FoundationError::Malformed(_))
        ));
    }
}
