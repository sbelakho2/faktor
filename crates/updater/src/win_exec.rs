//! The Windows object-identity launch seam: hash the release binary from an
//! open no-replace handle and keep that handle live across process creation.
//!
//! `CreateProcessW` is pathname-oriented, so a plain hash-then-spawn leaves a
//! verify→spawn swap window: a writer with access to the version directory
//! could replace the entry between the two. Windows has no `fexecve`, so this
//! seam closes the window with the Win32 sharing model instead:
//!
//! 1. the release binary is opened ONCE with `FILE_SHARE_READ` only — no
//!    `FILE_SHARE_WRITE`, no `FILE_SHARE_DELETE`. While that handle is live
//!    the I/O manager refuses every later open for write, every delete and
//!    every rename/replace of the file object (delete and rename both need
//!    access the share mode withholds). A writer that already holds the file
//!    open makes this restricted open itself fail, which is a typed refusal —
//!    never a silently weaker check;
//! 2. the sha256 digest is computed FROM the opened handle (streamed, bounded
//!    memory) and must equal the digest the pointer and the signed manifest
//!    authenticated;
//! 3. the object identity — volume serial number, 64-bit file index and
//!    creation time, read with `GetFileInformationByHandle` through the
//!    repo's `windows-sys` revision — is captured from that same handle; a
//!    handle whose information cannot be read refuses the launch typed (fail
//!    closed, never a weaker check);
//! 4. the handle stays live across `CreateProcessW`: because the pinned file
//!    can no longer be renamed, replaced, deleted or opened for writing, the
//!    pathname handed to `CreateProcessW` is bound to the verified object;
//! 5. after the child is created the identity is re-read from the handle and
//!    the pathname is re-opened and compared. Any change is a typed refusal
//!    that terminates the just-created process: the launcher never announces
//!    or detaches a child whose object identity it cannot vouch for.
//!
//! What this proves: the file object at the release pathname at
//! `CreateProcessW` time is the object whose full content was hashed, unless
//! the attacker already held a WRITABLE file mapping of it before step 1 (the
//! sharing check cannot revoke an existing mapping; that same-owner in-place
//! rewrite is outside the trust boundary, exactly as on unix) or retargeted a
//! directory junction/reparse point in the install chain: the post-creation
//! pathname re-check detects that and terminates + refuses, but the process
//! may have started before it is stopped, so the launcher never detaches it.
//! The readiness handshake remains the child's own health proof, not an
//! independent hash of the running image.
//!
//! ONE `#[allow(unsafe_code)]` scope (`handle_facts`) is the single
//! enumerated unsafe location of this crate: every raw Win32 call — the
//! identity read via `GetFileInformationByHandle` (the stable std accessors
//! for the volume serial/file index are still feature-gated) and the
//! digest-memo owner/ACL facts via `GetSecurityInfo` + the process token
//! user — sits inside its `unsafe` blocks, each carrying its own `// SAFETY:`
//! justification, and the location is enumerated in the static-authority
//! unsafe policy.

use std::fs::{File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt as _;
use std::os::windows::io::AsRawHandle as _;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    AclSizeInformation, EqualSid, GetAce, GetAclInformation, GetTokenInformation, TokenUser, ACL,
    ACL_SIZE_INFORMATION, DACL_SECURITY_INFORMATION, INHERIT_ONLY_ACE, OWNER_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::error::UpdateError;
use crate::install::{file_digest_of, DaclGrant, MemoFacts, WINDOWS_WRITE_ACCESS_MASK};

/// `FILE_SHARE_READ` (`winnt.h`): later opens of the file object may request
/// read access only. A write open is refused because `FILE_SHARE_WRITE` is
/// absent; a delete or rename/replace is refused because both need access
/// (`DELETE`) that `FILE_SHARE_DELETE` withholds.
pub(crate) const FILE_SHARE_READ: u32 = 0x0000_0001;

/// The stable Windows identity of one file object, read from an open handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ObjectIdentity {
    volume_serial: u32,
    file_index: u64,
    creation_time: u64,
}

impl ObjectIdentity {
    /// The `(volume serial, file index, creation time)` triple, for callers
    /// that pin the same identity vocabulary outside the launch seam (the
    /// digest memo's Windows key half in `install`).
    pub(crate) fn memo_key(&self) -> (u32, u64, u64) {
        (self.volume_serial, self.file_index, self.creation_time)
    }
}

/// The identity plus the digest-memo authorization facts of one file, read
/// through ONE read-only handle for the install layout's memo fast path:
/// the object identity (`GetFileInformationByHandle`) and the owner/DACL
/// facts (`GetSecurityInfo`: owner SID, the DACL's allow grants, and the
/// process token's user SID). `None` — the path cannot be opened or any
/// fact cannot be read — refuses memoization: the caller streams the full
/// bytes instead of trusting a weakened key.
pub(crate) fn path_memo_facts(path: &Path) -> Option<MemoPathFacts> {
    let file = File::open(path).ok()?;
    let (identity, authorization) = memo_facts_from_handle(&file, path).ok()?;
    Some(MemoPathFacts {
        identity,
        authorization,
    })
}

/// One file's stable identity plus its digest-memo authorization facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemoPathFacts {
    pub identity: ObjectIdentity,
    pub authorization: MemoFacts,
}

/// One verified release binary pinned by its open no-replace handle.
#[derive(Debug)]
pub(crate) struct VerifiedRelease {
    file: File,
    identity: ObjectIdentity,
    path: PathBuf,
}

impl VerifiedRelease {
    /// Open, hash and identify `path` through ONE handle: the digest read from
    /// the handle must equal `expected_digest`, and the identity captured from
    /// the handle must survive the hash.
    pub(crate) fn open(path: &Path, expected_digest: &str) -> Result<Self, UpdateError> {
        let mut file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(path)
            .map_err(|e| UpdateError::LaunchRefused {
                detail: format!(
                    "open {} with a no-write/no-delete share mode before launch: {e}",
                    path.display()
                ),
            })?;
        let meta = file.metadata().map_err(|e| UpdateError::LaunchRefused {
            detail: format!("stat {} before launch: {e}", path.display()),
        })?;
        if !meta.is_file() {
            return Err(UpdateError::LaunchRefused {
                detail: format!(
                    "release binary {} is not a regular file (symlinks/directories are refused)",
                    path.display()
                ),
            });
        }
        let identity = identity_from_handle(&file, path)?;
        let actual = file_digest_of(&mut file, path).map_err(|e| UpdateError::LaunchRefused {
            detail: format!("hash {} before launch: {e}", path.display()),
        })?;
        if actual != expected_digest {
            return Err(UpdateError::LaunchRefused {
                detail: format!(
                    "release digest changed between verification and launch: expected \
                     {expected_digest}, found {actual}"
                ),
            });
        }
        let after = identity_from_handle(&file, path)?;
        require_same(
            &identity,
            &after,
            path,
            "the pinned handle re-read after hashing",
        )?;
        Ok(VerifiedRelease {
            file,
            identity,
            path: path.to_path_buf(),
        })
    }

    /// The post-creation re-check: the identity read from the still-open
    /// handle AND re-opened from the pathname must both equal the verified
    /// identity. A changed object is a typed refusal; the caller terminates
    /// the just-created process.
    pub(crate) fn require_unchanged(&self) -> Result<(), UpdateError> {
        let from_handle = identity_from_handle(&self.file, &self.path)?;
        require_same(
            &self.identity,
            &from_handle,
            &self.path,
            "the pinned handle",
        )?;
        let reopened = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&self.path)
            .map_err(|e| UpdateError::LaunchRefused {
                detail: format!(
                    "the verified pathname {} no longer opens for the post-creation identity \
                     check: {e}",
                    self.path.display()
                ),
            })?;
        let from_path = identity_from_handle(&reopened, &self.path)?;
        require_same(
            &self.identity,
            &from_path,
            &self.path,
            "the pathname after creation",
        )
    }
}

/// The object identity from one open handle via `GetFileInformationByHandle`:
/// volume serial number, 64-bit file index and creation time. The handle is
/// the source of truth handed to `CreateProcessW`'s pathname, never a re-stat
/// of the path.
fn identity_from_handle(file: &File, path: &Path) -> Result<ObjectIdentity, UpdateError> {
    handle_facts(file, path, false).map(|(identity, _)| identity)
}

/// The identity plus the digest-memo authorization facts of one open handle.
fn memo_facts_from_handle(
    file: &File,
    path: &Path,
) -> Result<(ObjectIdentity, MemoFacts), UpdateError> {
    handle_facts(file, path, true)
}

/// `ACCESS_ALLOWED_ACE_TYPE` (`winnt.h`): a plain allow ACE (header, mask,
/// then the trustee SID at offset 8).
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
/// `ACCESS_ALLOWED_OBJECT_ACE_TYPE` (`winnt.h`): an allow ACE scoped to
/// object GUIDs; the trustee SID sits behind optional GUIDs, so a
/// write-capable one is conservatively recorded without a trustee.
const ACCESS_ALLOWED_OBJECT_ACE_TYPE: u8 = 5;
/// Bound on the allow grants copied out of one DACL; a longer ACL refuses
/// memoization (fail closed) instead of truncating the grant list.
const MAX_DACL_GRANTS: usize = 64;
/// Stack buffer for the `TokenUser` query: `sizeof(TOKEN_USER)` plus the
/// largest possible SID (`SECURITY_MAX_SID_SIZE`, 68 bytes) fits far below
/// this.
const TOKEN_USER_BUF_BYTES: usize = 512;

/// The ONE `#[allow(unsafe_code)]` scope of this crate (the static-authority
/// unsafe policy enumerates this file as the sole unsafe location): every
/// raw Win32 call — the identity read, the process token user lookup, the
/// owner/DACL read and the ACE walk — is lexically inside an `unsafe` block
/// below, each carrying its own `// SAFETY:` justification. `security =
/// false` skips the owner/ACL reads for callers that only need the identity
/// (the launch seam), so those paths make no security call at all.
#[allow(unsafe_code)]
fn handle_facts(
    file: &File,
    path: &Path,
    security: bool,
) -> Result<(ObjectIdentity, MemoFacts), UpdateError> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `file` owns a live handle for the duration of the call and
    // `info` is a valid writable out-parameter; GetFileInformationByHandle
    // writes only into `info` and reports failure through its return value,
    // checked immediately below.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) };
    if ok == 0 {
        return Err(UpdateError::LaunchRefused {
            detail: format!(
                "GetFileInformationByHandle on the verified handle of {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ),
        });
    }
    let identity = ObjectIdentity {
        volume_serial: info.dwVolumeSerialNumber,
        file_index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        creation_time: (u64::from(info.ftCreationTime.dwHighDateTime) << 32)
            | u64::from(info.ftCreationTime.dwLowDateTime),
    };
    if !security {
        return Ok((identity, MemoFacts::default()));
    }
    let mut facts = MemoFacts::default();
    // SAFETY: every raw call below runs against objects owned by this
    // process: `file` keeps its handle live for the whole call, the token is
    // opened and closed here, the security descriptor is allocated by
    // GetSecurityInfo and released with LocalFree before returning, every
    // out-parameter is an initialized local, and every pointer obtained is
    // null-checked before use. Any failure leaves `facts` non-authorizing
    // (fail closed), never a weaker check.
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        let mut user_sid: PSID = std::ptr::null_mut();
        let mut token_open = false;
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) != 0 {
            token_open = true;
            let mut user_buf = [0u8; TOKEN_USER_BUF_BYTES];
            let mut needed: u32 = 0;
            if GetTokenInformation(
                token,
                TokenUser,
                user_buf.as_mut_ptr().cast(),
                user_buf.len() as u32,
                &mut needed,
            ) != 0
                && needed as usize <= user_buf.len()
            {
                user_sid = (*user_buf.as_ptr().cast::<TOKEN_USER>()).User.Sid;
            }
        }
        let mut owner_sid: PSID = std::ptr::null_mut();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let security_read = GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner_sid,
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        ) == 0;
        if security_read && !user_sid.is_null() && !owner_sid.is_null() {
            // The trustee comparisons run through `EqualSid`, so the
            // self-relative SIDs inside the descriptor are compared
            // correctly regardless of their stored form.
            facts.owner_is_process_user = EqualSid(owner_sid, user_sid) != 0;
            if !dacl.is_null() {
                let mut size = ACL_SIZE_INFORMATION::default();
                if GetAclInformation(
                    dacl,
                    (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
                    std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                    AclSizeInformation,
                ) != 0
                {
                    let mut read = true;
                    let mut grants: Vec<DaclGrant> = Vec::new();
                    let mut index = 0u32;
                    while index < size.AceCount {
                        let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
                        if GetAce(dacl, index, &mut ace) == 0 || ace.is_null() {
                            read = false;
                            break;
                        }
                        index += 1;
                        let bytes = ace as *const u8;
                        let ace_type = *bytes;
                        if ace_type == ACCESS_ALLOWED_OBJECT_ACE_TYPE {
                            let mask = (bytes.add(4) as *const u32).read_unaligned();
                            if mask & WINDOWS_WRITE_ACCESS_MASK != 0 {
                                if grants.len() == MAX_DACL_GRANTS {
                                    read = false;
                                    break;
                                }
                                grants.push(DaclGrant {
                                    mask,
                                    trustee_is_owner: false,
                                    trustee_is_process_user: false,
                                });
                            }
                            continue;
                        }
                        if ace_type != ACCESS_ALLOWED_ACE_TYPE {
                            continue;
                        }
                        let flags = u32::from(*bytes.add(1));
                        if flags & INHERIT_ONLY_ACE != 0 {
                            // Inherit-only ACEs grant nothing on the file
                            // itself.
                            continue;
                        }
                        if grants.len() == MAX_DACL_GRANTS {
                            read = false;
                            break;
                        }
                        let mask = (bytes.add(4) as *const u32).read_unaligned();
                        let sid = bytes.add(8) as PSID;
                        grants.push(DaclGrant {
                            mask,
                            trustee_is_owner: EqualSid(sid, owner_sid) != 0,
                            trustee_is_process_user: EqualSid(sid, user_sid) != 0,
                        });
                    }
                    if read {
                        facts.grants = grants;
                        facts.dacl_read = true;
                    }
                }
            }
        }
        if !descriptor.is_null() {
            LocalFree(descriptor);
        }
        if token_open {
            CloseHandle(token);
        }
    }
    Ok((identity, facts))
}

fn require_same(
    expected: &ObjectIdentity,
    observed: &ObjectIdentity,
    path: &Path,
    source: &str,
) -> Result<(), UpdateError> {
    if expected != observed {
        return Err(UpdateError::LaunchRefused {
            detail: format!(
                "release object identity changed for {} ({source}): expected volume {:08x} file \
                 index {} creation {}, observed volume {:08x} file index {} creation {}; refusing \
                 the launch",
                path.display(),
                expected.volume_serial,
                expected.file_index,
                expected.creation_time,
                observed.volume_serial,
                observed.file_index,
                observed.creation_time
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::sha256_hex;
    use std::fs;

    fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    /// ERROR_ACCESS_DENIED (5) and ERROR_SHARING_VIOLATION (32) are the two
    /// I/O-manager refusals the sharing check reports.
    fn is_sharing_refusal(error: &std::io::Error) -> bool {
        matches!(error.raw_os_error(), Some(5) | Some(32))
    }

    #[test]
    fn the_verified_open_hashes_and_identifies_the_pinned_handle() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = b"faktor release bytes";
        let path = write_file(dir.path(), "faktor", bytes);
        let verified = VerifiedRelease::open(&path, &sha256_hex(bytes)).unwrap();
        let observed = identity_from_handle(&verified.file, &path).unwrap();
        assert_eq!(verified.identity, observed);
        verified.require_unchanged().unwrap();
    }

    #[test]
    fn a_digest_mismatch_refuses_typed() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "faktor", b"other bytes");
        let err = VerifiedRelease::open(&path, &sha256_hex(b"verified bytes")).unwrap_err();
        assert_eq!(err.code(), "launch_refused");
        assert!(err.to_string().contains("digest changed"), "{err}");
    }

    /// The identity-refusal path: an object whose stored identity differs from
    /// the pinned handle is refused typed and both identities are named.
    #[test]
    fn an_identity_mismatch_refuses_typed_and_names_both_identities() {
        let dir = tempfile::tempdir().unwrap();
        let pinned = write_file(dir.path(), "faktor", b"same bytes");
        let other = write_file(dir.path(), "other", b"same bytes");
        let verified = VerifiedRelease::open(&pinned, &sha256_hex(b"same bytes")).unwrap();
        let other_handle = OpenOptions::new().read(true).open(&other).unwrap();
        let other_identity = identity_from_handle(&other_handle, &other).unwrap();
        assert_ne!(
            verified.identity, other_identity,
            "distinct files must carry distinct identities, or the fixture is vacuous"
        );
        let forged = VerifiedRelease {
            file: verified.file,
            identity: other_identity,
            path: verified.path,
        };
        let err = forged.require_unchanged().unwrap_err();
        assert_eq!(err.code(), "launch_refused");
        assert!(err.to_string().contains("identity changed"), "{err}");
        assert!(
            err.to_string()
                .contains(&format!("{:08x}", other_identity.volume_serial)),
            "{err}"
        );
    }

    /// The share mode is the replacement barrier: while the verified handle is
    /// live, a write open, a delete and a rename are all refused by the I/O
    /// manager (and a plain read open is still allowed); after the handle is
    /// dropped the same rename succeeds.
    #[test]
    fn the_no_replace_share_mode_blocks_write_delete_and_rename() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "faktor", b"verified bytes");
        let moved = dir.path().join("moved");
        let pinned = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&path)
            .unwrap();
        let attempts: [(&str, Option<std::io::Error>); 4] = [
            (
                "write open",
                OpenOptions::new().write(true).open(&path).err(),
            ),
            (
                "append open",
                OpenOptions::new().append(true).open(&path).err(),
            ),
            ("delete", fs::remove_file(&path).err()),
            ("rename", fs::rename(&path, &moved).err()),
        ];
        for (what, error) in attempts {
            let error = error.unwrap_or_else(|| {
                panic!("{what} must be refused while the pinned handle is live")
            });
            assert!(
                is_sharing_refusal(&error),
                "{what}: unexpected error {error:?}"
            );
        }
        // Restrictive is not exclusive: a second READ open is allowed.
        let reader = OpenOptions::new().read(true).open(&path).unwrap();
        drop(reader);
        drop(pinned);
        fs::rename(&path, &moved).unwrap();
        assert!(!path.exists());
    }

    /// A writer already holding the file open refuses the verified open: the
    /// launcher cannot pin the object, so it must fail typed rather than fall
    /// back to a weaker pathname check.
    #[test]
    fn a_writer_already_holding_the_binary_open_refuses_the_verified_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "faktor", b"bytes");
        let writer = OpenOptions::new().write(true).open(&path).unwrap();
        let err = VerifiedRelease::open(&path, &sha256_hex(b"bytes")).unwrap_err();
        assert_eq!(err.code(), "launch_refused");
        assert!(err.to_string().contains("share mode"), "{err}");
        drop(writer);
        assert!(VerifiedRelease::open(&path, &sha256_hex(b"bytes")).is_ok());
    }

    /// The digest-memo facts read through the single unsafe authority: a
    /// test-created file's DACL is readable in full and the identity from
    /// the facts equals the plain identity read (both OBSERVED, never
    /// assumed — on elevated hosts the owner is the Administrators group,
    /// which the memo's owner check refuses by design).
    #[test]
    fn memo_facts_read_the_owner_dacl_and_process_user() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "artifact", b"bytes");
        let facts = path_memo_facts(&path).expect("facts for an openable file");
        assert!(
            facts.authorization.dacl_read,
            "the temp file's DACL must be read in full"
        );
        let file = File::open(&path).unwrap();
        assert_eq!(facts.identity, identity_from_handle(&file, &path).unwrap());
    }

    /// A path that cannot be opened refuses memoization (`None`), so the
    /// install layout streams the full bytes instead of trusting a weakened
    /// key.
    #[test]
    fn memo_facts_refuse_an_unopenable_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("not-there");
        assert!(path_memo_facts(&missing).is_none());
    }
}
