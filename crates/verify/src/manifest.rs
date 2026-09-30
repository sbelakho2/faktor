//! THE bounded, rooted build-manifest reader (audit finding 4).
//!
//! `manifest_has_target()` and `default_preset_name()` used to read build
//! manifests with `std::fs::read`: the length was checked AFTER the whole
//! file — up to a hostile or sparse size — was allocated, so
//! `MAX_MANIFEST_READ` bounded the parse but never the memory. Every
//! build-system manifest read in this crate now goes through
//! [`read_manifest_bounded`]:
//!
//! - the root is opened once as an anchored [`RootedDir`] capability, so an
//!   absolute path, a `..` escape, a symlinked parent or a link swapped in
//!   later is a typed refusal — never a read of the link target;
//! - a symlink or special entry (FIFO, socket, device) is refused from its
//!   no-follow literal metadata, and the anchored open itself carries
//!   `O_NONBLOCK` and classifies the opened handle, so a special file
//!   swapped in after the metadata can never block or be read;
//! - a literal size above `max` is refused before any content allocation;
//! - the content read goes through `take(max + 1)`: a file that GROWS
//!   between the metadata and the read is refused as soon as `max + 1` bytes
//!   are observed, so the cap holds under growth too.
//!
//! Invalid UTF-8 content is returned verbatim (the reader is byte-level, so
//! the decision stays with the parser): `manifest_has_target` interprets it
//! lossily and `default_preset_name` rejects it as malformed JSON; both are
//! deterministic documented fallbacks.

use std::io::Read;
use std::path::Path;

use faktor_core::error::ErrorKind;
use faktor_fs::rooted::RootedEntryKind;
use faktor_fs::RootedDir;

/// Hard bound on every build-system manifest content read in this crate
/// (CMakePresets.json, Makefile probes, compile-command manifests).
pub const MAX_MANIFEST_READ: u64 = 256 * 1024;

/// Why one bounded manifest read refused. Every variant is terminal: an
/// unsafe or oversized manifest NEVER yields a silent empty success a caller
/// could mistake for a parsed manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestReadError {
    /// The manifest, or one of its parent components, does not exist.
    Missing { rel: String },
    /// Refused by the rooted no-follow policy before any content read: an
    /// absolute/`..`/NUL path, an unopenable root, or a symlink/special
    /// entry (FIFO, socket, device) that is never followed or opened.
    Refused { rel: String, reason: String },
    /// The entry is larger than `cap`. `observed` is the literal no-follow
    /// size when the entry was stable, otherwise the first byte count over
    /// the cap actually read (`cap + 1`).
    Oversized {
        rel: String,
        cap: u64,
        observed: u64,
    },
    /// The bytes could not be read (I/O failure after a successful open).
    Unreadable { rel: String, reason: String },
}

/// The injectable seam between the no-follow metadata classification and the
/// anchored content open: production passes a no-op. Both steps still resolve
/// through the anchored capability, so the seam can schedule a swap in the
/// window a pathname-based reader could be redirected, but it can never
/// substitute content for a read.
type BetweenMetaAndOpen<'a> = &'a mut dyn FnMut();

/// The ONE bounded build-manifest read of this crate. `root` is the
/// verification worktree; `rel` is resolved strictly under it (absolute or
/// `..`-escaping values are typed refusals). At most `max` content bytes are
/// ever returned; observing `max + 1` bytes (including through growth after
/// the metadata) is [`ManifestReadError::Oversized`].
pub fn read_manifest_bounded(
    root: &Path,
    rel: &str,
    max: u64,
) -> Result<Vec<u8>, ManifestReadError> {
    read_manifest_bounded_with(root, rel, max, &mut || {})
}

fn read_manifest_bounded_with(
    root: &Path,
    rel: &str,
    max: u64,
    between_meta_and_open: BetweenMetaAndOpen<'_>,
) -> Result<Vec<u8>, ManifestReadError> {
    let rel_string = rel.to_string();
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let rooted = RootedDir::open(&canonical).map_err(|e| ManifestReadError::Refused {
        rel: rel_string.clone(),
        reason: format!(
            "cannot anchor the manifest root {}: {}",
            root.display(),
            e.message
        ),
    })?;
    let rel_path = Path::new(rel);
    match rooted.entry_meta(rel_path) {
        Ok(None) => return Err(ManifestReadError::Missing { rel: rel_string }),
        Ok(Some(meta)) if meta.kind == RootedEntryKind::Symlink => {
            return Err(ManifestReadError::Refused {
                rel: rel_string,
                reason: "the manifest is a symlink the rooted reader refuses to follow".into(),
            })
        }
        Ok(Some(meta)) if meta.kind != RootedEntryKind::File => {
            return Err(ManifestReadError::Refused {
                rel: rel_string,
                reason: "the manifest is not a plain regular file (FIFO/socket/device)".into(),
            })
        }
        // The literal no-follow size refuses a sparse/hostile manifest
        // before any content allocation; a file that grows after this point
        // is caught by the take-bounded read below.
        Ok(Some(meta)) if meta.size > max => {
            return Err(ManifestReadError::Oversized {
                rel: rel_string,
                cap: max,
                observed: meta.size,
            })
        }
        Ok(Some(_)) => {}
        Err(e) => return Err(map_rooted_error(&rel_string, e)),
    }
    between_meta_and_open();
    let mut file = rooted.open_read(rel_path).map_err(|e| match e.kind {
        ErrorKind::NotFound => ManifestReadError::Missing {
            rel: rel_string.clone(),
        },
        ErrorKind::Permission | ErrorKind::Malformed => ManifestReadError::Refused {
            rel: rel_string.clone(),
            reason: e.message,
        },
        _ => ManifestReadError::Unreadable {
            rel: rel_string.clone(),
            reason: e.message,
        },
    })?;
    let limit = max.saturating_add(1);
    let mut limited = file.by_ref().take(limit);
    let mut chunk = [0u8; 64 * 1024];
    let mut bytes: Vec<u8> = Vec::with_capacity((limit.min(chunk.len() as u64)) as usize);
    loop {
        let remaining = limit.saturating_sub(bytes.len() as u64);
        if remaining == 0 {
            break;
        }
        let want = remaining.min(chunk.len() as u64) as usize;
        let n = limited
            .read(&mut chunk[..want])
            .map_err(|e| ManifestReadError::Unreadable {
                rel: rel_string.clone(),
                reason: format!("read {rel:?}: {e}"),
            })?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
    if bytes.len() as u64 > max {
        return Err(ManifestReadError::Oversized {
            rel: rel_string,
            cap: max,
            observed: bytes.len() as u64,
        });
    }
    Ok(bytes)
}

fn map_rooted_error(rel: &str, e: faktor_core::error::Error) -> ManifestReadError {
    match e.kind {
        ErrorKind::NotFound => ManifestReadError::Missing {
            rel: rel.to_string(),
        },
        ErrorKind::Permission | ErrorKind::Malformed | ErrorKind::Oversized => {
            ManifestReadError::Refused {
                rel: rel.to_string(),
                reason: e.message,
            }
        }
        _ => ManifestReadError::Unreadable {
            rel: rel.to_string(),
            reason: e.message,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: u64 = 4096;

    fn write(root: &Path, rel: &str, bytes: &[u8]) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
    }

    fn read(root: &Path, rel: &str) -> Result<Vec<u8>, ManifestReadError> {
        read_manifest_bounded(root, rel, CAP)
    }

    #[test]
    fn size_cap_minus_one_is_accepted_whole() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = vec![b'a'; (CAP - 1) as usize];
        write(dir.path(), "CMakePresets.json", &bytes);
        assert_eq!(read(dir.path(), "CMakePresets.json").unwrap(), bytes);
    }

    #[test]
    fn size_exactly_cap_is_accepted_whole() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = vec![b'b'; CAP as usize];
        write(dir.path(), "Makefile", &bytes);
        assert_eq!(read(dir.path(), "Makefile").unwrap(), bytes);
    }

    #[test]
    fn size_cap_plus_one_is_refused_oversized() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = vec![b'c'; (CAP + 1) as usize];
        write(dir.path(), "compile_commands.json", &bytes);
        assert_eq!(
            read(dir.path(), "compile_commands.json"),
            Err(ManifestReadError::Oversized {
                rel: "compile_commands.json".into(),
                cap: CAP,
                observed: CAP + 1,
            })
        );
    }

    #[test]
    fn huge_sparse_manifest_is_refused_from_literal_metadata_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("CMakePresets.json");
        let file = std::fs::File::create(&path).unwrap();
        let sparse_len: u64 = 200 * 1024 * 1024;
        file.set_len(sparse_len).unwrap();
        drop(file);
        let started = std::time::Instant::now();
        let err = read(dir.path(), "CMakePresets.json").unwrap_err();
        assert_eq!(
            err,
            ManifestReadError::Oversized {
                rel: "CMakePresets.json".into(),
                cap: CAP,
                observed: sparse_len,
            }
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the sparse manifest must be refused from metadata, never read"
        );
    }

    #[test]
    fn manifest_that_grows_after_metadata_is_refused_at_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "CMakePresets.json",
            &vec![b'x'; (CAP - 1) as usize],
        );
        let grow_root = dir.path().to_path_buf();
        let mut seam = move || {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(grow_root.join("CMakePresets.json"))
                .unwrap();
            f.write_all(&vec![b'y'; 64 * 1024]).unwrap();
        };
        let err = read_manifest_bounded_with(dir.path(), "CMakePresets.json", CAP, &mut seam)
            .unwrap_err();
        assert_eq!(
            err,
            ManifestReadError::Oversized {
                rel: "CMakePresets.json".into(),
                cap: CAP,
                observed: CAP + 1,
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_swapped_after_metadata_is_refused_inside_the_rooted_policy() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("root");
        let outside = base.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        write(
            &root,
            "CMakePresets.json",
            br#"{"configurePresets":[{"name":"honest"}]}"#,
        );
        std::fs::write(
            outside.join("CMakePresets.json"),
            br#"{"configurePresets":[{"name":"EXTERNAL-SECRET-9f3c"}]}"#,
        )
        .unwrap();
        let swap_root = root.clone();
        let swap_outside = outside.clone();
        let mut seam = move || {
            std::fs::rename(
                swap_root.join("CMakePresets.json"),
                swap_root.join("honest.bak"),
            )
            .unwrap();
            std::os::unix::fs::symlink(
                swap_outside.join("CMakePresets.json"),
                swap_root.join("CMakePresets.json"),
            )
            .unwrap();
        };
        let err =
            read_manifest_bounded_with(&root, "CMakePresets.json", CAP, &mut seam).unwrap_err();
        assert!(
            matches!(err, ManifestReadError::Refused { ref rel, .. } if rel == "CMakePresets.json"),
            "the swapped symlink must be a rooted-policy refusal: {err:?}"
        );
        let honest = read(&root, "honest.bak").unwrap();
        assert!(String::from_utf8_lossy(&honest).contains("honest"));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_manifest_is_refused_and_never_followed() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("root");
        let outside = base.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(
            outside.join("CMakePresets.json"),
            br#"{"configurePresets":[{"name":"EXTERNAL-SECRET-9f3c"}]}"#,
        )
        .unwrap();
        std::os::unix::fs::symlink(
            outside.join("CMakePresets.json"),
            root.join("CMakePresets.json"),
        )
        .unwrap();
        let err = read(&root, "CMakePresets.json").unwrap_err();
        assert!(
            matches!(err, ManifestReadError::Refused { .. }),
            "a symlinked manifest must be refused: {err:?}"
        );
    }

    #[test]
    fn absolute_and_parent_escaping_paths_are_typed_refusals() {
        let dir = tempfile::tempdir().unwrap();
        for rel in ["/etc/hostname", "../outside.json", "sub/../../outside.json"] {
            match read(dir.path(), rel) {
                Err(ManifestReadError::Refused { .. }) => {}
                other => panic!("{rel:?} must be a rooted-policy refusal, got {other:?}"),
            }
        }
    }

    #[test]
    fn missing_manifest_is_a_typed_missing_refusal() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read(dir.path(), "CMakePresets.json"),
            Err(ManifestReadError::Missing {
                rel: "CMakePresets.json".into(),
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn directory_and_socket_manifests_are_refused_as_not_regular() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("CMakePresets.json")).unwrap();
        assert!(
            matches!(
                read(dir.path(), "CMakePresets.json"),
                Err(ManifestReadError::Refused { .. })
            ),
            "a directory manifest must be refused"
        );
        let _listener =
            std::os::unix::net::UnixListener::bind(dir.path().join("Makefile")).unwrap();
        assert!(
            matches!(
                read(dir.path(), "Makefile"),
                Err(ManifestReadError::Refused { .. })
            ),
            "a socket manifest must be refused"
        );
    }

    #[cfg(unix)]
    #[test]
    fn fifo_manifest_is_refused_without_ever_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("CMakePresets.json");
        match std::process::Command::new("mkfifo").arg(&fifo).status() {
            Ok(status) if status.success() => {}
            _ => {
                eprintln!("skipping FIFO manifest test: mkfifo is not available");
                return;
            }
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let root = dir.path().to_path_buf();
        let worker = std::thread::spawn(move || {
            let _ = tx.send(read_manifest_bounded(&root, "CMakePresets.json", CAP));
        });
        let outcome = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("a FIFO manifest must be refused, never opened and blocked on");
        assert!(
            matches!(outcome, Err(ManifestReadError::Refused { .. })),
            "a FIFO manifest must be refused: {outcome:?}"
        );
        worker.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn fifo_swapped_after_metadata_is_refused_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "CMakePresets.json", b"{}");
        let probe = dir.path().join(".mkfifo-probe");
        match std::process::Command::new("mkfifo").arg(&probe).status() {
            Ok(status) if status.success() => {
                let _ = std::fs::remove_file(&probe);
            }
            _ => {
                eprintln!("skipping swapped-FIFO test: mkfifo is not available");
                return;
            }
        }
        let fifo_root = dir.path().to_path_buf();
        let (tx, rx) = std::sync::mpsc::channel();
        let root = dir.path().to_path_buf();
        let worker = std::thread::spawn(move || {
            let mut seam = move || {
                let rel = fifo_root.join("CMakePresets.json");
                std::fs::remove_file(&rel).unwrap();
                std::process::Command::new("mkfifo")
                    .arg(&rel)
                    .status()
                    .unwrap();
            };
            let _ = tx.send(read_manifest_bounded_with(
                &root,
                "CMakePresets.json",
                CAP,
                &mut seam,
            ));
        });
        let outcome = rx.recv_timeout(std::time::Duration::from_secs(5)).expect(
            "a FIFO swapped in after metadata must be refused, never opened and blocked on",
        );
        assert!(
            matches!(outcome, Err(ManifestReadError::Refused { .. })),
            "the swapped FIFO must be a rooted-policy refusal: {outcome:?}"
        );
        worker.join().unwrap();
    }

    #[test]
    fn invalid_utf8_bytes_are_returned_verbatim_never_reinterpreted() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = vec![0xff, 0xfe, 0x00, b'{', 0x80];
        write(dir.path(), "CMakePresets.json", &bytes);
        assert_eq!(read(dir.path(), "CMakePresets.json").unwrap(), bytes);
    }
}
