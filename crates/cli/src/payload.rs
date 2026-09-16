//! The operator-staged payload contract: private keys, client secrets and
//! worker tokens live in ONE configured directory as plain files whose names
//! are referenced from config, never as inline config values.
//!
//! Invariants (fail closed):
//!
//! - a payload name is a plain bounded ASCII file name: no separators, no
//!   `..`, no control characters; anything else is refused BEFORE any
//!   filesystem access;
//! - the file must be a regular file (symlinks and directories are
//!   refused): a symlink could silently redirect the read outside the
//!   staged directory;
//! - on unix the file mode must be 0600-style: any group/other access bit
//!   (`0o077`) refuses the payload, and the staged directory itself must
//!   not be group/other writable (other users must not be able to swap a
//!   staged secret);
//! - reads are bounded per payload kind and materialized with a hard cap
//!   (a private key is not a 1 GiB file); a missing file is refused with
//!   the EXACT path it expected;
//! - every parse is strict: a private key must be an unencrypted PKCS#8 PEM
//!   envelope, a secret must be non-empty printable ASCII without
//!   whitespace. A corrupt payload is a typed construction refusal, never a
//!   partial load.

use std::io::Read;
use std::path::{Path, PathBuf};

/// Bound on one payload file name.
pub const MAX_PAYLOAD_NAME_BYTES: usize = 128;
/// Bound on a secret payload (client secret, webhook secret, token).
pub const MAX_SECRET_PAYLOAD_BYTES: u64 = 4 * 1024;
/// Bound on a private-key PEM payload.
pub const MAX_PRIVATE_KEY_PAYLOAD_BYTES: u64 = 64 * 1024;

/// The typed refusal of the staged-payload contract. Every variant carries
/// the exact path (or name) the contract expected, so an operator can act
/// on it without reproducing the join.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PayloadError {
    #[error(
        "payload name {name:?} is not a plain bounded file name \
         (no paths, no traversal, no control characters)"
    )]
    Name { name: String },
    #[error("payload {path} is missing")]
    Missing { path: PathBuf },
    #[error("payload {path} is not a regular file (symlinks and directories are refused)")]
    NotAFile { path: PathBuf },
    #[error("payload directory {path} is not a directory")]
    NotADirectory { path: PathBuf },
    #[error(
        "payload {path} has mode {mode:03o}, expected 0600-style \
         (no group/other access)"
    )]
    TooPermissive { path: PathBuf, mode: u32 },
    #[error(
        "payload directory {path} has mode {mode:03o} and is group/other \
         writable; staged secrets must not be replaceable by other users"
    )]
    DirectoryTooPermissive { path: PathBuf, mode: u32 },
    #[error("payload {path} is {bytes} bytes, above the {max}-byte bound for a {what}")]
    TooLarge {
        path: PathBuf,
        what: &'static str,
        bytes: u64,
        max: u64,
    },
    #[error("payload {path} could not be read: {message}")]
    Unreadable { path: PathBuf, message: String },
    #[error("payload {path} is corrupt: {message}")]
    Malformed { path: PathBuf, message: String },
}

/// One staged payload directory. Construction reads nothing; every load
/// re-validates the directory and the file, so a swap after startup is
/// refused on the next read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadDir {
    root: PathBuf,
}

impl PayloadDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Validate one payload NAME: 1..=128 printable ASCII graphic bytes,
    /// no `/`, `\` or `:` separators, not `.` or `..`.
    pub fn validate_name(name: &str) -> Result<(), PayloadError> {
        let refused = || PayloadError::Name {
            name: name.to_string(),
        };
        if name.is_empty() || name.len() > MAX_PAYLOAD_NAME_BYTES {
            return Err(refused());
        }
        if !name.is_ascii() || name.bytes().any(|b| !b.is_ascii_graphic()) {
            return Err(refused());
        }
        if name.contains('/') || name.contains('\\') || name.contains(':') {
            return Err(refused());
        }
        if name == "." || name == ".." {
            return Err(refused());
        }
        Ok(())
    }

    /// The exact path a payload name resolves to (validated).
    pub fn path_of(&self, name: &str) -> Result<PathBuf, PayloadError> {
        Self::validate_name(name)?;
        Ok(self.root.join(name))
    }

    /// The staged directory must exist as a real directory and (on unix)
    /// must not be group/other writable. A MISSING directory is not an
    /// error here: every payload then refuses with its exact expected path.
    fn check_root(&self) -> Result<(), PayloadError> {
        let root = &self.root;
        let metadata = match std::fs::symlink_metadata(root) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(PayloadError::Unreadable {
                    path: root.clone(),
                    message: e.to_string(),
                })
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(PayloadError::NotADirectory { path: root.clone() });
        }
        check_directory_mode(root, &metadata)
    }

    fn read_bounded(
        &self,
        name: &str,
        what: &'static str,
        max_bytes: u64,
    ) -> Result<(PathBuf, Vec<u8>), PayloadError> {
        let path = self.path_of(name)?;
        self.check_root()?;
        let link_metadata = std::fs::symlink_metadata(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                PayloadError::Missing { path: path.clone() }
            } else {
                PayloadError::Unreadable {
                    path: path.clone(),
                    message: e.to_string(),
                }
            }
        })?;
        if link_metadata.file_type().is_symlink() {
            return Err(PayloadError::NotAFile { path });
        }
        let file = std::fs::File::open(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                PayloadError::Missing { path: path.clone() }
            } else {
                PayloadError::Unreadable {
                    path: path.clone(),
                    message: e.to_string(),
                }
            }
        })?;
        // The opened descriptor's metadata is the authoritative mode/size
        // (the pre-open check above cannot be raced into a size/mode lie for
        // the bytes actually read).
        let metadata = file.metadata().map_err(|e| PayloadError::Unreadable {
            path: path.clone(),
            message: e.to_string(),
        })?;
        if !metadata.is_file() {
            return Err(PayloadError::NotAFile { path });
        }
        check_file_mode(&path, &metadata)?;
        let mut body = Vec::new();
        file.take(max_bytes.saturating_add(1))
            .read_to_end(&mut body)
            .map_err(|e| PayloadError::Unreadable {
                path: path.clone(),
                message: e.to_string(),
            })?;
        if body.len() as u64 > max_bytes {
            return Err(PayloadError::TooLarge {
                path,
                what,
                bytes: body.len() as u64,
                max: max_bytes,
            });
        }
        if body.is_empty() {
            return Err(PayloadError::Malformed {
                path,
                message: "the payload is empty".into(),
            });
        }
        Ok((path, body))
    }

    /// Load an unencrypted PKCS#8 private key PEM (`BEGIN PRIVATE KEY`).
    /// The envelope is checked here; the key itself is parsed by the
    /// consumer at construction (an eager, typed parse).
    pub fn load_private_key_pem(&self, name: &str) -> Result<String, PayloadError> {
        let (path, bytes) =
            self.read_bounded(name, "private key", MAX_PRIVATE_KEY_PAYLOAD_BYTES)?;
        let text = String::from_utf8(bytes).map_err(|_| PayloadError::Malformed {
            path: path.clone(),
            message: "not valid UTF-8".into(),
        })?;
        let trimmed = text.trim_end();
        if !text.starts_with("-----BEGIN PRIVATE KEY-----")
            || !trimmed.ends_with("-----END PRIVATE KEY-----")
        {
            return Err(PayloadError::Malformed {
                path,
                message: "not an unencrypted PKCS#8 private key PEM \
                          (BEGIN/END PRIVATE KEY envelope)"
                    .into(),
            });
        }
        Ok(text)
    }

    /// Load one secret payload: non-empty printable ASCII without
    /// whitespace after trimming the surrounding line ending.
    pub fn load_secret(&self, name: &str) -> Result<String, PayloadError> {
        let (path, bytes) = self.read_bounded(name, "secret", MAX_SECRET_PAYLOAD_BYTES)?;
        let text = String::from_utf8(bytes).map_err(|_| PayloadError::Malformed {
            path: path.clone(),
            message: "not valid UTF-8".into(),
        })?;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(PayloadError::Malformed {
                path,
                message: "the secret is empty".into(),
            });
        }
        if !trimmed.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(PayloadError::Malformed {
                path,
                message: "the secret contains whitespace or control characters".into(),
            });
        }
        Ok(trimmed.to_string())
    }
}

#[cfg(unix)]
fn check_file_mode(path: &Path, metadata: &std::fs::Metadata) -> Result<(), PayloadError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(PayloadError::TooPermissive {
            path: path.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_file_mode(_path: &Path, _metadata: &std::fs::Metadata) -> Result<(), PayloadError> {
    Ok(())
}

#[cfg(unix)]
fn check_directory_mode(path: &Path, metadata: &std::fs::Metadata) -> Result<(), PayloadError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o022 != 0 {
        return Err(PayloadError::DirectoryTooPermissive {
            path: path.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_directory_mode(_path: &Path, _metadata: &std::fs::Metadata) -> Result<(), PayloadError> {
    Ok(())
}

#[cfg(test)]
#[path = "payload_tests.rs"]
mod payload_tests;
