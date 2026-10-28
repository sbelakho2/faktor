//! Machine-owned credential vault.
//!
//! Secret MATERIAL lives only in `<data-dir>/vault/secrets/<id>` with
//! owner-only permissions. Shared config, snapshots, diagnostics, logs and
//! exports carry only an opaque `vault://<kind>/<id>` handle plus a
//! verification state. Rotation mints a NEW generation; local deletion and
//! remote revocation are distinct states.

// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir

use std::path::{Path, PathBuf};

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::db::{now_ms, FoundationDb};
use crate::FoundationError;

pub const MAX_SECRET_BYTES: usize = 64 * 1024;
const SECRET_ID_BYTES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationState {
    Unverified,
    Verified,
    Failed,
    Revoked,
}

impl VerificationState {
    pub fn as_str(self) -> &'static str {
        match self {
            VerificationState::Unverified => "unverified",
            VerificationState::Verified => "verified",
            VerificationState::Failed => "failed",
            VerificationState::Revoked => "revoked",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecretRef {
    pub handle: String,
    pub kind: String,
    pub label: String,
    pub generation: u64,
    pub verification: VerificationState,
    pub created_ms: i64,
    pub rotated_ms: Option<i64>,
}

#[derive(Clone)]
pub struct SecretVault {
    db: FoundationDb,
    root: PathBuf,
}

impl SecretVault {
    /// The vault root is `<data-dir>/vault`; secret files are created with
    /// owner-only permissions before any bytes are written.
    pub fn new(db: FoundationDb, data_dir: impl AsRef<Path>) -> Self {
        Self {
            db,
            root: data_dir.as_ref().join("vault"),
        }
    }

    fn secrets_dir(&self) -> PathBuf {
        self.root.join("secrets")
    }

    fn material_path(&self, id: &str) -> PathBuf {
        self.secrets_dir().join(id)
    }

    /// Store secret material and return its opaque handle. The raw value is
    /// NEVER persisted in the foundation database.
    pub fn store(
        &self,
        kind: &str,
        label: &str,
        secret: &[u8],
    ) -> Result<SecretRef, FoundationError> {
        validate_kind(kind)?;
        if secret.is_empty() {
            return Err(FoundationError::Malformed(
                "refusing to store an empty secret".into(),
            ));
        }
        if secret.len() > MAX_SECRET_BYTES {
            return Err(FoundationError::Oversized(format!(
                "secret is {} bytes, over {MAX_SECRET_BYTES}",
                secret.len()
            )));
        }
        if label.len() > 128 {
            return Err(FoundationError::Oversized(
                "secret label must fit 128 bytes".into(),
            ));
        }
        if crate::contains_secret_material(label) {
            return Err(FoundationError::Malformed(
                "secret label must not contain secret material; the value belongs in the secret field"
                    .into(),
            ));
        }
        let id = new_secret_id();
        std::fs::create_dir_all(self.secrets_dir())
            .map_err(|e| FoundationError::Store(format!("create vault secrets dir: {e}")))?;
        let path = self.material_path(&id);
        write_owner_only(&path, secret)?;
        let now = now_ms();
        let handle = format!("vault://{kind}/{id}");
        let result = self.db.write("vault store", |conn| {
            conn.execute(
                "INSERT INTO secret_ref(handle, kind, label, generation, verified, revoked, created_ms, rotated_ms)
                 VALUES (?1, ?2, ?3, 1, 0, 0, ?4, NULL)",
                params![handle, kind, label, now],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(SecretRef {
                handle: handle.clone(),
                kind: kind.to_string(),
                label: label.to_string(),
                generation: 1,
                verification: VerificationState::Unverified,
                created_ms: now,
                rotated_ms: None,
            })
        });
        if result.is_err() {
            let _ = std::fs::remove_file(&path);
        }
        result
    }

    /// Read secret material for a live handle. This is the ONLY API that
    /// returns bytes; callers must never log or serialize the result.
    pub fn reveal(&self, handle: &str) -> Result<Vec<u8>, FoundationError> {
        let (kind, id) = parse_handle(handle)?;
        let row: Option<(i64, i64)> = self.db.read("vault reveal", |conn| {
            conn.query_row(
                "SELECT generation, revoked FROM secret_ref WHERE handle = ?1 AND kind = ?2",
                params![handle, kind],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| FoundationError::Store(e.to_string()))
        })?;
        let Some((_generation, revoked)) = row else {
            // Foreign and missing handles are indistinguishable BY CONTRACT:
            // the message never echoes the handle, so a 404 body for a
            // handle minted in another machine's vault is byte-identical to
            // a 404 for a handle that never existed.
            return Err(FoundationError::NotFound(
                "secret reference does not exist".into(),
            ));
        };
        if revoked != 0 {
            return Err(FoundationError::Conflict(format!(
                "secret reference {handle} was deleted/revoked locally"
            )));
        }
        let bytes = std::fs::read(self.material_path(&id))
            .map_err(|e| FoundationError::Store(format!("read secret material: {e}")))?;
        if bytes.len() > MAX_SECRET_BYTES {
            return Err(FoundationError::Corrupt(
                "vault material exceeds the stored bound".into(),
            ));
        }
        Ok(bytes)
    }

    /// Mark a reference verified after an authentication probe observed it.
    pub fn mark_verified(
        &self,
        handle: &str,
        verified: bool,
    ) -> Result<SecretRef, FoundationError> {
        let (_, id) = parse_handle(handle)?;
        let state = if verified {
            VerificationState::Verified
        } else {
            VerificationState::Failed
        };
        self.db
            .write("vault verify", |conn| {
                let changed = conn
                    .execute(
                        "UPDATE secret_ref SET verified = ?2 WHERE handle = ?1 AND revoked = 0",
                        params![handle, if verified { 1 } else { 0 }],
                    )
                    .map_err(|e| FoundationError::Store(e.to_string()))?;
                if changed == 0 {
                    return Err(FoundationError::NotFound(
                        "secret reference does not exist or is revoked".into(),
                    ));
                }
                read_ref(conn, handle)
            })
            .map(|mut r| {
                r.verification = state;
                let _ = id;
                r
            })
    }

    /// Mark a reference verified ONLY when it still has `generation`.
    ///
    /// A probe resolves the credential bytes (and this generation) once, at
    /// call start. If a rotate lands while that call is in flight, the late
    /// completion belongs to the OLD material and must not mark the NEW
    /// generation verified: `Ok(None)` reports that the generation moved
    /// (or the reference was revoked) and nothing was written.
    pub fn mark_verified_generation(
        &self,
        handle: &str,
        generation: u64,
        verified: bool,
    ) -> Result<Option<SecretRef>, FoundationError> {
        parse_handle(handle)?;
        let generation = i64::try_from(generation).map_err(|_| {
            FoundationError::Malformed("secret generation exceeds the stored range".into())
        })?;
        self.db
            .write("vault verify generation", |conn| {
                let changed = conn
                    .execute(
                        "UPDATE secret_ref SET verified = ?3
                         WHERE handle = ?1 AND generation = ?2 AND revoked = 0",
                        params![handle, generation, if verified { 1 } else { 0 }],
                    )
                    .map_err(|e| FoundationError::Store(e.to_string()))?;
                if changed == 0 {
                    return Ok(None);
                }
                read_ref(conn, handle).map(Some)
            })
            .map(|reference| {
                reference.map(|mut r| {
                    r.verification = if verified {
                        VerificationState::Verified
                    } else {
                        VerificationState::Failed
                    };
                    r
                })
            })
    }

    /// Rotate: write new material, bump the generation, keep the same
    /// handle. The previous bytes are overwritten atomically.
    pub fn rotate(&self, handle: &str, secret: &[u8]) -> Result<SecretRef, FoundationError> {
        if secret.is_empty() || secret.len() > MAX_SECRET_BYTES {
            return Err(FoundationError::Oversized(
                "rotated secret must be non-empty and within the vault bound".into(),
            ));
        }
        let (_, id) = parse_handle(handle)?;
        let exists: Option<i64> = self.db.read("vault rotate", |conn| {
            conn.query_row(
                "SELECT revoked FROM secret_ref WHERE handle = ?1",
                params![handle],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| FoundationError::Store(e.to_string()))
        })?;
        match exists {
            Some(0) => {}
            Some(_) => {
                return Err(FoundationError::Conflict(
                    "a revoked reference cannot be rotated; bind a new credential".into(),
                ))
            }
            None => {
                return Err(FoundationError::NotFound(
                    "secret reference does not exist".into(),
                ))
            }
        }
        write_owner_only(&self.material_path(&id), secret)?;
        self.db.write("vault rotate", |conn| {
            conn.execute(
                "UPDATE secret_ref SET generation = generation + 1, verified = 0, rotated_ms = ?2
                 WHERE handle = ?1",
                params![handle, now_ms()],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            read_ref(conn, handle)
        })
    }

    /// Local deletion: removes the material and marks the reference revoked.
    /// This is distinct from a remote revocation, which the caller records
    /// separately; no API claims remote state was changed.
    pub fn delete(&self, handle: &str) -> Result<SecretRef, FoundationError> {
        let (_, id) = parse_handle(handle)?;
        let updated = self.db.write("vault delete", |conn| {
            conn.execute(
                "UPDATE secret_ref SET revoked = 1, verified = 0 WHERE handle = ?1",
                params![handle],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            read_ref(conn, handle)
        })?;
        match std::fs::remove_file(self.material_path(&id)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(FoundationError::Store(format!(
                    "secret row revoked but material removal failed: {e}"
                )))
            }
        }
        Ok(updated)
    }

    pub fn list(&self) -> Result<Vec<SecretRef>, FoundationError> {
        self.db.read("vault list", |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT handle, kind, label, generation, verified, revoked, created_ms, rotated_ms
                     FROM secret_ref ORDER BY created_ms ASC, handle ASC LIMIT 4096",
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(SecretRef {
                        handle: r.get(0)?,
                        kind: r.get(1)?,
                        label: r.get(2)?,
                        generation: u64::try_from(r.get::<_, i64>(3)?).unwrap_or(0),
                        verification: if r.get::<_, i64>(5)? != 0 {
                            VerificationState::Revoked
                        } else if r.get::<_, i64>(4)? != 0 {
                            VerificationState::Verified
                        } else {
                            VerificationState::Unverified
                        },
                        created_ms: r.get(6)?,
                        rotated_ms: r.get(7)?,
                    })
                })
                .map_err(|e| FoundationError::Store(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(rows)
        })
    }

    pub fn get(&self, handle: &str) -> Result<SecretRef, FoundationError> {
        self.db.read("vault get", |conn| read_ref(conn, handle))
    }
}

fn read_ref(conn: &rusqlite::Connection, handle: &str) -> Result<SecretRef, FoundationError> {
    conn.query_row(
        "SELECT handle, kind, label, generation, verified, revoked, created_ms, rotated_ms
         FROM secret_ref WHERE handle = ?1",
        params![handle],
        |r| {
            Ok(SecretRef {
                handle: r.get(0)?,
                kind: r.get(1)?,
                label: r.get(2)?,
                generation: u64::try_from(r.get::<_, i64>(3)?).unwrap_or(0),
                verification: if r.get::<_, i64>(5)? != 0 {
                    VerificationState::Revoked
                } else if r.get::<_, i64>(4)? != 0 {
                    VerificationState::Verified
                } else {
                    VerificationState::Unverified
                },
                created_ms: r.get(6)?,
                rotated_ms: r.get(7)?,
            })
        },
    )
    .optional()
    .map_err(|e| FoundationError::Store(e.to_string()))?
    .ok_or_else(|| FoundationError::NotFound("secret reference does not exist".into()))
}

fn validate_kind(kind: &str) -> Result<(), FoundationError> {
    if kind.is_empty()
        || kind.len() > 64
        || !kind
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(FoundationError::Malformed(
            "secret kind must be 1..=64 ASCII [A-Za-z0-9_-]".into(),
        ));
    }
    Ok(())
}

fn parse_handle(handle: &str) -> Result<(&str, String), FoundationError> {
    let rest = handle.strip_prefix("vault://").ok_or_else(|| {
        FoundationError::Malformed("secret reference must be vault://<kind>/<id>".into())
    })?;
    let mut parts = rest.split('/');
    let (Some(kind), Some(id), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(FoundationError::Malformed(
            "secret reference must be vault://<kind>/<id>".into(),
        ));
    };
    validate_kind(kind)?;
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(FoundationError::Malformed(
            "secret reference id is invalid".into(),
        ));
    }
    Ok((kind, id.to_string()))
}

fn write_owner_only(path: &Path, bytes: &[u8]) -> Result<(), FoundationError> {
    if let Some(parent) = path.parent() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .map_err(|e| FoundationError::Store(format!("create vault dir: {e}")))?;
        }
        #[cfg(not(unix))]
        std::fs::create_dir_all(parent)
            .map_err(|e| FoundationError::Store(format!("create vault dir: {e}")))?;
    }
    // The shared durable-atomic authority owns the temp/fsync/rename dance.
    faktor_fs::atomic::atomic_replace(path, bytes)
        .map_err(|e| FoundationError::Store(format!("write secret material: {e}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| FoundationError::Store(format!("own secret material: {e}")))?;
    }
    Ok(())
}

fn new_secret_id() -> String {
    let mut bytes = [0u8; SECRET_ID_BYTES];
    // The bytes are an opaque identifier, never a key: derive them from the
    // process-unique time and a per-process counter mixed through blake3.
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hasher = blake3::Hasher::new();
    hasher.update(&now_ms().to_le_bytes());
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(&counter.to_le_bytes());
    let digest = hasher.finalize();
    bytes.copy_from_slice(&digest.as_bytes()[..SECRET_ID_BYTES]);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault() -> (tempfile::TempDir, SecretVault) {
        let dir = tempfile::tempdir().unwrap();
        let db = FoundationDb::open(dir.path().join("f.db")).unwrap();
        let vault = SecretVault::new(db, dir.path());
        (dir, vault)
    }

    #[cfg(unix)]
    #[test]
    fn secret_material_is_owner_only_and_never_echoed_by_refs() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, vault) = vault();
        let reference = vault
            .store("provider", "OpenAI", b"sk-secret-value")
            .unwrap();
        assert!(reference.handle.starts_with("vault://provider/"));
        assert_eq!(vault.reveal(&reference.handle).unwrap(), b"sk-secret-value");
        let path = dir
            .path()
            .join("vault/secrets")
            .join(reference.handle.strip_prefix("vault://provider/").unwrap());
        let mode = std::fs::metadata(path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let listing = serde_json::to_string(&vault.list().unwrap()).unwrap();
        assert!(!listing.contains("sk-secret-value"));
    }

    #[test]
    fn secret_shaped_labels_are_refused_instead_of_persisted_as_metadata() {
        let (_dir, vault) = vault();
        let secret = "sk-A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8";
        let err = vault
            .store("provider", &format!("label {secret}"), b"material")
            .unwrap_err();
        assert!(err.to_string().contains("secret material"), "{err}");
        assert!(
            vault.list().unwrap().is_empty(),
            "a refused label persists no row"
        );
        // The value itself remains storable: only metadata is constrained.
        assert!(vault.store("provider", "prod key", b"material").is_ok());
    }

    #[test]
    fn delete_and_rotate_semantics_are_distinct_and_revocation_refuses_reads() {
        let (_dir, vault) = vault();
        let reference = vault.store("provider", "p", b"one").unwrap();
        let rotated = vault.rotate(&reference.handle, b"two").unwrap();
        assert_eq!(rotated.generation, 2);
        assert_eq!(vault.reveal(&reference.handle).unwrap(), b"two");
        vault.mark_verified(&reference.handle, true).unwrap();
        assert_eq!(
            vault.get(&reference.handle).unwrap().verification,
            VerificationState::Verified
        );
        let deleted = vault.delete(&reference.handle).unwrap();
        assert_eq!(deleted.verification, VerificationState::Revoked);
        assert!(vault.reveal(&reference.handle).is_err());
        assert!(vault.rotate(&reference.handle, b"three").is_err());
    }

    #[test]
    fn foreign_handles_and_hostile_paths_are_refused() {
        let (_dir, vault) = vault();
        assert!(vault.reveal("vault://provider/../../etc/passwd").is_err());
        assert!(vault.reveal("vault://provider/x").is_err());
        assert!(vault.reveal("sk-live").is_err());
        assert!(vault.store("bad/kind", "x", b"y").is_err());
        assert!(vault.store("provider", "x", b"").is_err());
    }

    /// A handle minted by ANOTHER machine vault and a handle that never existed
    /// are indistinguishable at every operation: the typed NotFound message does
    /// not echo the opaque handle, so the wire 404 is byte-identical (a probe
    /// cannot enumerate foreign references by error text).
    #[test]
    fn foreign_and_missing_handles_are_indistinguishable_byte_for_byte() {
        let (dir_a, vault_a) = vault();
        let (dir_b, vault_b) = vault();
        let foreign = vault_b
            .store("provider", "other machine", b"other")
            .unwrap();
        let missing = "vault://provider/00000000000000000000000000000000";
        assert_ne!(foreign.handle, missing);
        for op in [0u8, 1, 2] {
            let a = match op {
                0 => vault_a.reveal(&foreign.handle).unwrap_err().to_string(),
                1 => vault_a
                    .rotate(&foreign.handle, b"new")
                    .unwrap_err()
                    .to_string(),
                _ => vault_a.delete(&foreign.handle).unwrap_err().to_string(),
            };
            let b = match op {
                0 => vault_a.reveal(missing).unwrap_err().to_string(),
                1 => vault_a.rotate(missing, b"new").unwrap_err().to_string(),
                _ => vault_a.delete(missing).unwrap_err().to_string(),
            };
            assert_eq!(
                a, b,
                "operation {op} must not distinguish foreign from missing"
            );
            assert!(
                !a.contains(&foreign.handle),
                "the 404 body echoes the handle: {a}"
            );
        }
        // The vaults themselves are unchanged: no cross-vault delete/rotate.
        assert_eq!(vault_b.reveal(&foreign.handle).unwrap(), b"other");
        drop((dir_a, dir_b));
    }

    /// A deleted reference is a typed CONFLICT on use (not a 404): the row is
    /// provably local and revoked, and rotate refuses rather than resurrecting
    /// it. A malformed handle stays a typed Malformed before any store read.
    #[test]
    fn delete_then_use_and_rotate_are_typed_and_never_resurrect() {
        let (_dir, vault) = vault();
        let reference = vault.store("provider", "p", b"one").unwrap();
        vault.delete(&reference.handle).unwrap();
        assert!(matches!(
            vault.reveal(&reference.handle),
            Err(FoundationError::Conflict(_))
        ));
        assert!(matches!(
            vault.rotate(&reference.handle, b"two"),
            Err(FoundationError::Conflict(_))
        ));
        assert!(matches!(
            vault.get(&reference.handle).unwrap().verification,
            VerificationState::Revoked
        ));
        // The material is gone from disk as well as from the row.
        let id = reference.handle.strip_prefix("vault://provider/").unwrap();
        assert!(!_dir.path().join("vault/secrets").join(id).exists());
        for hostile in [
            "sk-live",
            "vault://provider",
            "vault://../x",
            "vault://provider/x/y",
        ] {
            assert!(
                matches!(vault.reveal(hostile), Err(FoundationError::Malformed(_))),
                "{hostile:?} must be a typed Malformed, not a fabricated miss"
            );
        }
    }

    /// When the machine secret store cannot be created (for example the vault
    /// path is occupied by a file, or the OS/volume refuses the directory), the
    /// store is a typed `Store` degradation: no row is written, no empty secret
    /// is claimed, and the caller sees an error instead of a fake handle.
    #[test]
    fn unavailable_secret_store_degrades_typed_and_writes_no_row() {
        let dir = tempfile::tempdir().unwrap();
        // Occupy `<data>/vault` with a regular file: the vault cannot create its
        // owner-only secrets directory. This is the file-backed equivalent of
        // "no OS keychain / unavailable OS secret service".
        std::fs::write(dir.path().join("vault"), b"not a directory").unwrap();
        let db = FoundationDb::open(dir.path().join("f.db")).unwrap();
        let vault = SecretVault::new(db, dir.path());
        let err = vault
            .store("provider", "unavailable", b"sk-planted")
            .unwrap_err();
        assert!(matches!(err, FoundationError::Store(_)), "{err:?}");
        assert!(
            vault.list().unwrap().is_empty(),
            "a degraded store must not journal a secret reference"
        );
    }
}
