//! Directory and SQLite storage backends used by the classes that are plain
//! files (browser profiles, terminal agent homes, rebuildable caches,
//! updater remnants, SQLite databases/WAL).

use std::path::{Component, Path, PathBuf};

use crate::class::StorageClass;
use crate::error::{GovernorError, GovernorResult};
use crate::governor::StorageGovernor;
use crate::ledger::ObjectRow;
use crate::object::{EvictionOutcome, StorageBackend, StorageObject};

/// Bound on one directory walk: a hostile tree can never spin the governor
/// forever. Exceeding it is a typed backend failure (health degrades), never
/// a silent partial inventory.
pub const MAX_WALK_ENTRIES: usize = 1_000_000;

#[derive(Debug, Clone)]
pub struct DirectoryBackend {
    class: StorageClass,
    roots: Vec<PathBuf>,
    /// Objects younger than this are not evicted (active downloads/temp
    /// files). Zero = age is not a guard.
    pub min_age_ms: i64,
}

impl DirectoryBackend {
    pub fn new(class: StorageClass, root: impl Into<PathBuf>) -> Self {
        Self {
            class,
            roots: vec![root.into()],
            min_age_ms: 0,
        }
    }

    pub fn with_roots(class: StorageClass, roots: Vec<PathBuf>) -> Self {
        Self {
            class,
            roots,
            min_age_ms: 0,
        }
    }

    pub fn with_min_age(mut self, min_age_ms: i64) -> Self {
        self.min_age_ms = min_age_ms;
        self
    }

    fn resolve(&self, key: &str) -> Option<PathBuf> {
        if self.roots.len() == 1 {
            let rel = safe_relative(key)?;
            Some(self.roots[0].join(rel))
        } else {
            let (index, rel) = key.split_once(':')?;
            let root = self.roots.get(index.parse::<usize>().ok()?)?;
            Some(root.join(safe_relative(rel)?))
        }
    }

    fn key_for(&self, index: usize, rel: &Path) -> String {
        let rel = rel.to_string_lossy().replace('\\', "/");
        if self.roots.len() == 1 {
            rel
        } else {
            format!("{index}:{rel}")
        }
    }
}

fn safe_relative(key: &str) -> Option<PathBuf> {
    if key.is_empty() || key.contains('\0') {
        return None;
    }
    let path = Path::new(key);
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

impl StorageBackend for DirectoryBackend {
    fn canonical_directory_key(&self, root: &Path, rel: &Path) -> Option<String> {
        let index = self.roots.iter().position(|candidate| {
            candidate == root
                || (candidate.exists()
                    && root.exists()
                    && candidate.canonicalize().ok() == root.canonicalize().ok())
        })?;
        Some(self.key_for(index, rel))
    }

    fn class(&self) -> StorageClass {
        self.class
    }

    fn scan(&self, _governor: &StorageGovernor) -> GovernorResult<Vec<StorageObject>> {
        let mut out = Vec::new();
        let mut visited = 0usize;
        for (index, root) in self.roots.iter().enumerate() {
            let mut stack = vec![root.clone()];
            while let Some(dir) = stack.pop() {
                let read_dir = match std::fs::read_dir(&dir) {
                    Ok(read_dir) => read_dir,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => {
                        return Err(GovernorError::Backend(format!(
                            "scan {}: {e}",
                            dir.display()
                        )))
                    }
                };
                for entry in read_dir {
                    visited += 1;
                    if visited > MAX_WALK_ENTRIES {
                        return Err(GovernorError::Backend(format!(
                            "directory scan exceeded the {MAX_WALK_ENTRIES}-entry bound under {}",
                            root.display()
                        )));
                    }
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(e) => {
                            return Err(GovernorError::Backend(format!(
                                "scan entry under {}: {e}",
                                dir.display()
                            )))
                        }
                    };
                    let file_type = match entry.file_type() {
                        Ok(file_type) => file_type,
                        Err(e) => {
                            return Err(GovernorError::Backend(format!(
                                "scan entry type under {}: {e}",
                                dir.display()
                            )))
                        }
                    };
                    if file_type.is_symlink() {
                        continue;
                    }
                    let path = entry.path();
                    if file_type.is_dir() {
                        stack.push(path);
                        continue;
                    }
                    if !file_type.is_file() {
                        continue;
                    }
                    let rel = match path.strip_prefix(root) {
                        Ok(rel) => rel,
                        Err(_) => continue,
                    };
                    let meta = match entry.metadata() {
                        Ok(meta) => meta,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(e) => {
                            return Err(GovernorError::Backend(format!(
                                "scan metadata {}: {e}",
                                path.display()
                            )))
                        }
                    };
                    let last_used_ms = meta
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0);
                    out.push(StorageObject {
                        key: self.key_for(index, rel),
                        bytes: meta.len(),
                        last_used_ms,
                        digest: None,
                        required: false,
                    });
                }
            }
        }
        Ok(out)
    }

    fn directory_roots(&self) -> Option<(&[PathBuf], i64)> {
        Some((&self.roots, self.min_age_ms))
    }

    fn evict(
        &self,
        _governor: &StorageGovernor,
        object: &StorageObject,
    ) -> GovernorResult<EvictionOutcome> {
        if self.min_age_ms > 0 {
            let now = crate::ledger::now_ms();
            if now.saturating_sub(object.last_used_ms) < self.min_age_ms {
                return Ok(EvictionOutcome::Refused {
                    detail: format!(
                        "object {} is younger than the {} ms remnant bound",
                        object.key, self.min_age_ms
                    ),
                });
            }
        }
        let Some(path) = self.resolve(&object.key) else {
            return Err(GovernorError::Malformed(format!(
                "directory key {:?} escapes its root",
                object.key
            )));
        };
        let bytes = match std::fs::metadata(&path) {
            Ok(meta) => meta.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(EvictionOutcome::Absent)
            }
            Err(e) => {
                return Err(GovernorError::Backend(format!(
                    "stat {}: {e}",
                    path.display()
                )))
            }
        };
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(EvictionOutcome::Absent)
            }
            Err(e) => {
                return Ok(EvictionOutcome::Refused {
                    detail: format!("remove {}: {e}", path.display()),
                })
            }
        }
        // Remove now-empty parent directories up to (but never including)
        // the root; a failed cleanup is not an eviction failure.
        for root in &self.roots {
            if !path.starts_with(root) {
                continue;
            }
            let mut parent = path.parent();
            while let Some(dir) = parent {
                if dir == root || !dir.starts_with(root) {
                    break;
                }
                match std::fs::remove_dir(dir) {
                    Ok(()) => parent = dir.parent(),
                    Err(_) => break,
                }
            }
        }
        Ok(EvictionOutcome::Evicted {
            bytes,
            verified: false,
            digest: None,
        })
    }
}

/// SQLite databases, WAL and SHM sidecars under a data directory. A `.db` is
/// never deleted (it is a live authority); a `-wal` is reclaimed by a
/// non-blocking TRUNCATE checkpoint, which is the only growth that can be
/// bounded without losing committed rows.
#[derive(Debug, Clone)]
pub struct SqliteBackend {
    root: PathBuf,
}

const SQLITE_SKIP_DIRS: &[&str] = &[
    "storage-governor",
    "cas",
    "backups",
    "commercial-backups",
    "target",
];

impl SqliteBackend {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn db_path(&self, key: &str) -> Option<PathBuf> {
        Some(self.root.join(safe_relative(key)?))
    }
}

impl StorageBackend for SqliteBackend {
    fn class(&self) -> StorageClass {
        StorageClass::SqliteWal
    }

    fn scan(&self, _governor: &StorageGovernor) -> GovernorResult<Vec<StorageObject>> {
        let mut out = Vec::new();
        let mut frontier = vec![(self.root.clone(), 0usize)];
        let mut visited = 0usize;
        while let Some((dir, depth)) = frontier.pop() {
            let read_dir = match std::fs::read_dir(&dir) {
                Ok(read_dir) => read_dir,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(GovernorError::Backend(format!(
                        "scan {}: {e}",
                        dir.display()
                    )))
                }
            };
            for entry in read_dir {
                visited += 1;
                if visited > MAX_WALK_ENTRIES {
                    return Err(GovernorError::Backend(
                        "sqlite scan exceeded its entry bound".into(),
                    ));
                }
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(_) => continue,
                };
                let file_type = match entry.file_type() {
                    Ok(file_type) => file_type,
                    Err(_) => continue,
                };
                if file_type.is_symlink() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                if file_type.is_dir() {
                    if depth >= 4
                        || name.starts_with('.')
                        || SQLITE_SKIP_DIRS.contains(&name.as_str())
                    {
                        continue;
                    }
                    frontier.push((entry.path(), depth + 1));
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }
                let governed =
                    name.ends_with(".db") || name.ends_with(".db-wal") || name.ends_with(".db-shm");
                if !governed {
                    continue;
                }
                let path = entry.path();
                let Ok(rel) = path.strip_prefix(&self.root) else {
                    continue;
                };
                let meta = match entry.metadata() {
                    Ok(meta) => meta,
                    Err(_) => continue,
                };
                let last_used_ms = meta
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                let key = rel.to_string_lossy().replace('\\', "/");
                // A live database, its SHM and the governor's own ledger are
                // never eviction candidates; only a WAL can be reclaimed (and
                // that is a checkpoint, not a deletion).
                let required = !key.ends_with(".db-wal");
                out.push(StorageObject {
                    key,
                    bytes: meta.len(),
                    last_used_ms,
                    digest: None,
                    required,
                });
            }
        }
        Ok(out)
    }

    fn evict(
        &self,
        _governor: &StorageGovernor,
        object: &StorageObject,
    ) -> GovernorResult<EvictionOutcome> {
        let Some(path) = self.db_path(&object.key) else {
            return Err(GovernorError::Malformed(format!(
                "sqlite key {:?} escapes the data root",
                object.key
            )));
        };
        if !object.key.ends_with(".db-wal") {
            return Ok(EvictionOutcome::Refused {
                detail: "a live SQLite database/shm is never deleted; only a WAL may be truncated"
                    .into(),
            });
        }
        let db = PathBuf::from(&object.key[..object.key.len() - "-wal".len()]);
        let db_path = self
            .db_path(&db.to_string_lossy())
            .ok_or_else(|| GovernorError::Malformed("sqlite wal path".into()))?;
        let before = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let conn = match rusqlite::Connection::open(&db_path) {
            Ok(conn) => conn,
            Err(e) => {
                return Ok(EvictionOutcome::Refused {
                    detail: format!("open {} for WAL truncate: {e}", db_path.display()),
                })
            }
        };
        let _ = conn.execute_batch("PRAGMA busy_timeout = 0;");
        let checkpoint = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        });
        let _ = conn.execute_batch("PRAGMA busy_timeout = 5000;");
        match checkpoint {
            Ok((busy, log_frames)) if busy != 0 => {
                return Ok(EvictionOutcome::Refused {
                    detail: format!(
                        "wal_checkpoint(TRUNCATE) is busy ({} pinned frames); the WAL keeps its {} bytes",
                        log_frames, before
                    ),
                })
            }
            Ok(_) => {}
            Err(e) => {
                return Ok(EvictionOutcome::Refused {
                    detail: format!("wal_checkpoint(TRUNCATE) refused: {e}"),
                })
            }
        }
        let after = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if after >= before {
            // A checkpoint that cannot shrink the sidecar must not be
            // reported as an eviction: the ledger would otherwise delete a
            // row that still names bytes on disk and fake free room.
            return Ok(EvictionOutcome::Refused {
                detail: format!("wal_checkpoint(TRUNCATE) left the {before} byte WAL unchanged"),
            });
        }
        if after == 0 {
            return Ok(EvictionOutcome::Evicted {
                bytes: before,
                verified: true,
                digest: None,
            });
        }
        Ok(EvictionOutcome::Reclaimed {
            bytes: before - after,
            remaining_bytes: after,
            verified: true,
        })
    }
}

/// Convert a backend scan into ledger rows without inventing a digest.
pub fn object_row(class: StorageClass, object: &StorageObject) -> ObjectRow {
    ObjectRow {
        class,
        key: object.key.clone(),
        bytes: object.bytes,
        digest: object.digest.clone(),
        last_used_ms: object.last_used_ms,
        required: object.required,
    }
}
