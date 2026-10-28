// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! `faktor install` — the reproducible trusted-install step.
//!
//! The VS Code extension auto-starts an executable only when it is a
//! machine-scoped path INSIDE the configured install root and its recorded
//! sha256 matches the bytes on disk. This command creates exactly that
//! identity without a manual copy/hash/settings dance:
//!
//! ```text
//! <dest>/
//!   faktor                     the installed executable (0755)
//!   faktor.sha256              `<sha256>  faktor` (the trust record)
//!   install-identity.json      version, digest, layout, installed_at_ms
//!   INSTALL.md                 machine-scoped settings guidance
//!   install-settings.json      the exact settings object, machine-parseable
//! ```
//!
//! This is the IN-ROOT EXECUTABLE model: the signed bootstrap layout
//! (`launcher` + `current` + `versions/`) is not required. When that layout
//! already exists in `<dest>` the installer refuses instead of silently
//! shadowing the launcher, so there is exactly one identity per root.
//!
//! The copy is streamed (bounded memory) into a temp file in the same
//! directory, hashed while writing, fsynced, re-hashed, and adopted through
//! the same atomic rename discipline as every other durable writer.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use faktor_fs::atomic::{atomic_adopt, atomic_replace};
use sha2::{Digest, Sha256};

/// The installed executable name (the extension resolves this in-root).
pub const INSTALLED_BINARY_NAME: &str = "faktor";
/// The checksum record the extension's trust gate reads (`<bin>.sha256`).
pub const CHECKSUM_SUFFIX: &str = ".sha256";
/// Identity document schema.
pub const IDENTITY_SCHEMA: &str = "faktor-install-identity/v1";
/// Guidance/identity files written beside the executable.
pub const IDENTITY_FILE_NAME: &str = "install-identity.json";
pub const GUIDANCE_FILE_NAME: &str = "INSTALL.md";
pub const SETTINGS_FILE_NAME: &str = "install-settings.json";

#[derive(Debug, Clone)]
pub struct InstallRequest {
    pub dest: PathBuf,
    pub source: Option<PathBuf>,
    /// Included in the settings guidance when the operator runs a non-default
    /// daemon data dir (the live profile does).
    pub data_dir: Option<PathBuf>,
    pub force: bool,
    pub json: bool,
}

#[derive(Debug, Clone)]
pub struct InstallReceipt {
    pub dest: PathBuf,
    pub executable: PathBuf,
    pub checksum: PathBuf,
    pub identity: PathBuf,
    pub guidance: PathBuf,
    pub settings: PathBuf,
    pub version: String,
    pub sha256: String,
    pub bytes: u64,
}

/// Run the command; prints the receipt and returns the process exit code.
pub fn run(request: InstallRequest) -> i32 {
    match install(&request) {
        Ok(receipt) => {
            if request.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "schema": "faktor-install-result/v1",
                        "status": "installed",
                        "layout": "in-root-executable",
                        "dest": receipt.dest.display().to_string(),
                        "executable": receipt.executable.display().to_string(),
                        "checksum": receipt.checksum.display().to_string(),
                        "identity": receipt.identity.display().to_string(),
                        "guidance": receipt.guidance.display().to_string(),
                        "settings": receipt.settings.display().to_string(),
                        "version": receipt.version,
                        "sha256": receipt.sha256,
                        "bytes": receipt.bytes,
                    })
                );
            } else {
                println!(
                    "installed {} ({}, {} bytes)",
                    receipt.executable.display(),
                    &receipt.sha256[..12],
                    receipt.bytes
                );
                println!("  checksum: {}", receipt.checksum.display());
                println!("  identity: {}", receipt.identity.display());
                println!("  guidance: {}", receipt.guidance.display());
                println!(
                    "  settings: {}  (add to VS Code USER settings; never workspace)",
                    receipt.settings.display()
                );
            }
            0
        }
        Err(error) => {
            eprintln!("faktor install: {error}");
            2
        }
    }
}

pub fn install(request: &InstallRequest) -> Result<InstallReceipt, String> {
    if request.dest.as_os_str().is_empty() {
        return Err("--dest must not be empty".into());
    }
    if !request.dest.is_absolute() {
        return Err(format!(
            "--dest {} must be an absolute path (the extension refuses relative paths)",
            request.dest.display()
        ));
    }
    if request.dest.exists() && !request.dest.is_dir() {
        return Err(format!(
            "--dest {} is not a directory",
            request.dest.display()
        ));
    }
    refuse_release_layout(&request.dest)?;
    create_private_dir_all(&request.dest)
        .map_err(|e| format!("create {}: {e}", request.dest.display()))?;

    let source = match &request.source {
        Some(path) => path.clone(),
        None => std::env::current_exe()
            .map_err(|e| format!("cannot resolve the running executable: {e}"))?,
    };
    let source = source
        .canonicalize()
        .map_err(|e| format!("source {}: {e}", source.display()))?;
    if !source.is_file() {
        return Err(format!("source {} is not a regular file", source.display()));
    }

    let executable = request.dest.join(installed_binary_name());
    if executable.exists() && !request.force {
        return Err(format!(
            "{} already exists; pass --force to replace it (the checksum is rewritten)",
            executable.display()
        ));
    }
    if source == executable {
        return Err(format!(
            "{} is the running install; choose another --dest or --source",
            executable.display()
        ));
    }

    let (sha256, bytes) = stage_verified_copy(&source, &executable)?;
    let version = faktor_core::VERSION.to_string();
    let checksum = write_checksum(&executable, &sha256)?;
    let identity = write_identity(&request.dest, &executable, &sha256, &version, bytes)?;
    let settings = write_settings(&request.dest, &executable, request.data_dir.as_deref())?;
    let guidance = write_guidance(
        &request.dest,
        &executable,
        &sha256,
        &version,
        bytes,
        request.data_dir.as_deref(),
    )?;

    Ok(InstallReceipt {
        dest: request.dest.clone(),
        executable,
        checksum,
        identity,
        guidance,
        settings,
        version,
        sha256,
        bytes,
    })
}

/// A signed bootstrap layout owns the root; a second identity beside it would
/// make "which binary runs" ambiguous. Refuse loudly instead.
fn refuse_release_layout(dest: &Path) -> Result<(), String> {
    for marker in ["launcher", "current"] {
        if dest.join(marker).exists() {
            return Err(format!(
                "{} already holds a signed release layout ({marker}); use `faktor updater` for that root or choose a fresh --dest",
                dest.display()
            ));
        }
    }
    if dest.join("versions").is_dir() {
        return Err(format!(
            "{} already holds a signed release layout (versions/); use `faktor updater` for that root or choose a fresh --dest",
            dest.display()
        ));
    }
    Ok(())
}

fn installed_binary_name() -> &'static str {
    #[cfg(windows)]
    {
        "faktor.exe"
    }
    #[cfg(not(windows))]
    {
        INSTALLED_BINARY_NAME
    }
}

/// Stream the source into a same-directory temp, hash while writing, fsync,
/// re-hash the staged bytes, then atomically adopt them as the executable.
fn stage_verified_copy(source: &Path, executable: &Path) -> Result<(String, u64), String> {
    let parent = executable
        .parent()
        .ok_or_else(|| format!("{} has no parent", executable.display()))?;
    let tmp = temp_path(parent, "install");
    let staged = (|| -> Result<(String, u64), String> {
        let mut input =
            File::open(source).map_err(|e| format!("open {}: {e}", source.display()))?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| format!("create {}: {e}", tmp.display()))?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        let mut bytes = 0u64;
        loop {
            let read = input
                .read(&mut buffer)
                .map_err(|e| format!("read {}: {e}", source.display()))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            output
                .write_all(&buffer[..read])
                .map_err(|e| format!("write {}: {e}", tmp.display()))?;
            bytes += read as u64;
        }
        output
            .sync_all()
            .map_err(|e| format!("fsync {}: {e}", tmp.display()))?;
        drop(output);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))
                .map_err(|e| format!("chmod {}: {e}", tmp.display()))?;
        }
        let sha256 = format!("{:x}", hasher.finalize());
        let staged_sha256 = sha256_of(&tmp)?;
        if staged_sha256 != sha256 {
            return Err(format!(
                "staged copy {} does not match the source digest ({} != {})",
                tmp.display(),
                staged_sha256,
                sha256
            ));
        }
        Ok((sha256, bytes))
    })();
    match staged {
        Ok(result) => match adopt(&tmp, executable) {
            Ok(()) => Ok(result),
            Err(error) => {
                let _ = fs::remove_file(&tmp);
                Err(error)
            }
        },
        Err(error) => {
            let _ = fs::remove_file(&tmp);
            Err(error)
        }
    }
}

fn adopt(tmp: &Path, executable: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        // rename(2) cannot replace an existing file on Windows; the temp is
        // already fsynced and same-directory, so removing the old identity
        // first is the narrowest possible non-atomic window.
        if executable.exists() {
            fs::remove_file(executable)
                .map_err(|e| format!("remove {}: {e}", executable.display()))?;
        }
    }
    atomic_adopt(tmp, executable).map_err(|e| format!("install {}: {e}", executable.display()))
}

fn write_checksum(executable: &Path, sha256: &str) -> Result<PathBuf, String> {
    let path = checksum_path(executable);
    let name = executable
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(INSTALLED_BINARY_NAME);
    atomic_replace(&path, format!("{sha256}  {name}\n").as_bytes())
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

fn checksum_path(executable: &Path) -> PathBuf {
    let mut name = executable
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from(INSTALLED_BINARY_NAME));
    name.push(CHECKSUM_SUFFIX);
    executable.with_file_name(name)
}

fn write_identity(
    dest: &Path,
    executable: &Path,
    sha256: &str,
    version: &str,
    bytes: u64,
) -> Result<PathBuf, String> {
    let path = dest.join(IDENTITY_FILE_NAME);
    let document = serde_json::json!({
        "schema": IDENTITY_SCHEMA,
        "layout": "in-root-executable",
        "executable": executable.file_name().and_then(OsStr::to_str).unwrap_or(INSTALLED_BINARY_NAME),
        "version": version,
        "sha256": sha256,
        "bytes": bytes,
        "installed_at_ms": now_ms(),
    });
    let text = serde_json::to_vec_pretty(&document)
        .map_err(|e| format!("serialize {}: {e}", path.display()))?;
    atomic_replace(&path, &text).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

fn settings_object(dest: &Path, executable: &Path, data_dir: Option<&Path>) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    object.insert(
        "faktor.installRoot".into(),
        serde_json::Value::String(dest.display().to_string()),
    );
    object.insert(
        "faktor.binaryPath".into(),
        serde_json::Value::String(executable.display().to_string()),
    );
    if let Some(data_dir) = data_dir {
        object.insert(
            "faktor.dataDir".into(),
            serde_json::Value::String(data_dir.display().to_string()),
        );
    }
    serde_json::Value::Object(object)
}

fn write_settings(
    dest: &Path,
    executable: &Path,
    data_dir: Option<&Path>,
) -> Result<PathBuf, String> {
    let path = dest.join(SETTINGS_FILE_NAME);
    let text = serde_json::to_vec_pretty(&settings_object(dest, executable, data_dir))
        .map_err(|e| format!("serialize {}: {e}", path.display()))?;
    atomic_replace(&path, &text).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

fn write_guidance(
    dest: &Path,
    executable: &Path,
    sha256: &str,
    version: &str,
    bytes: u64,
    data_dir: Option<&Path>,
) -> Result<PathBuf, String> {
    let path = dest.join(GUIDANCE_FILE_NAME);
    let settings = serde_json::to_string_pretty(&settings_object(dest, executable, data_dir))
        .map_err(|e| format!("serialize settings guidance: {e}"))?;
    let text = format!(
        "# Faktor in-root install\n\n\
         Installed executable: `{exe}`\n\
         Version: `{version}`\n\
         SHA-256: `{sha256}`\n\
         Size: {bytes} bytes\n\
         Trust record: `{exe}.sha256`\n\n\
         This root uses the IN-ROOT EXECUTABLE model: the verified executable and\n\
         its checksum live directly in the install root. No `launcher`, `current`\n\
         or `versions/` layout is required. If a signed launcher layout exists in\n\
         the same root, the extension starts that launcher instead; this installer\n\
         refuses to mix the two.\n\n\
         Add these MACHINE-SCOPED settings to VS Code **User** settings (the\n\
         extension ignores and refuses workspace-scoped values):\n\n\
         ```json\n{settings}\n```\n\n\
         The settings values are also written verbatim to `{settings_file}`.\n\
         The extension re-hashes `{exe}` against `{exe}.sha256` before every\n\
         start; a mismatch is a typed refusal, never a fallback.\n",
        exe = executable.display(),
        settings_file = SETTINGS_FILE_NAME,
    );
    atomic_replace(&path, text.as_bytes()).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

fn sha256_of(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(path)
}

fn temp_path(parent: &Path, label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    parent.join(format!(
        ".faktor-{label}-{}-{nonce}.tmp",
        std::process::id()
    ))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    fn request(dest: &Path, source: &Path) -> InstallRequest {
        InstallRequest {
            dest: dest.to_path_buf(),
            source: Some(source.to_path_buf()),
            data_dir: Some(dest.join("data")),
            force: false,
            json: false,
        }
    }

    #[test]
    fn creates_the_verified_in_root_identity_and_machine_settings_guidance() {
        let root = tempfile::tempdir().unwrap();
        let source = fixture(root.path(), "faktor-cli", b"#!/bin/sh\nprintf ok\n");
        let dest = root.path().join("trusted");
        let receipt = install(&request(&dest, &source)).unwrap();

        let installed = dest.join(installed_binary_name());
        assert_eq!(receipt.executable, installed);
        assert_eq!(fs::read(&installed).unwrap(), b"#!/bin/sh\nprintf ok\n");

        let record = fs::read_to_string(dest.join(format!(
            "{}{}",
            installed_binary_name(),
            CHECKSUM_SUFFIX
        )))
        .unwrap();
        let expected = sha256_of(&installed).unwrap();
        assert!(record.starts_with(&expected), "{record}");
        assert!(record.contains(installed_binary_name()), "{record}");
        assert_eq!(receipt.sha256, expected);

        let identity: serde_json::Value =
            serde_json::from_slice(&fs::read(&receipt.identity).unwrap()).unwrap();
        assert_eq!(identity["schema"], IDENTITY_SCHEMA);
        assert_eq!(identity["layout"], "in-root-executable");
        assert_eq!(identity["sha256"], expected.as_str());
        assert_eq!(identity["version"], faktor_core::VERSION);

        let settings: serde_json::Value =
            serde_json::from_slice(&fs::read(&receipt.settings).unwrap()).unwrap();
        assert_eq!(settings["faktor.installRoot"], dest.display().to_string());
        assert_eq!(
            settings["faktor.binaryPath"],
            installed.display().to_string()
        );
        assert_eq!(
            settings["faktor.dataDir"],
            dest.join("data").display().to_string()
        );

        let guidance = fs::read_to_string(&receipt.guidance).unwrap();
        assert!(guidance.contains("IN-ROOT EXECUTABLE"));
        assert!(guidance.contains("MACHINE-SCOPED"));
        assert!(guidance.contains(INSTALLED_BINARY_NAME));
        assert!(guidance.contains(&expected));
    }

    #[test]
    fn refuses_a_root_that_already_holds_a_signed_launcher_layout() {
        let root = tempfile::tempdir().unwrap();
        let source = fixture(root.path(), "faktor-cli", b"bin");
        for marker in ["launcher", "current", "versions"] {
            let dest = root.path().join(format!("dest-{marker}"));
            fs::create_dir_all(&dest).unwrap();
            if marker == "versions" {
                fs::create_dir_all(dest.join(marker)).unwrap();
            } else {
                fs::write(dest.join(marker), b"x").unwrap();
            }
            let error = install(&request(&dest, &source)).unwrap_err();
            assert!(error.contains("signed release layout"), "{error}");
        }
    }

    #[test]
    fn refuses_relative_dest_missing_source_and_a_file_destination() {
        let root = tempfile::tempdir().unwrap();
        let source = fixture(root.path(), "faktor-cli", b"bin");
        let relative = InstallRequest {
            dest: PathBuf::from("relative-install"),
            ..request(root.path(), &source)
        };
        assert!(install(&relative).unwrap_err().contains("absolute path"));

        let missing = fixture(root.path(), "missing", b"bin");
        fs::remove_file(&missing).unwrap();
        let error = install(&InstallRequest {
            source: Some(missing),
            ..request(&root.path().join("dest"), &source)
        })
        .unwrap_err();
        assert!(error.contains("source"), "{error}");

        let file_dest = fixture(root.path(), "file-dest", b"x");
        assert!(install(&InstallRequest {
            dest: file_dest.clone(),
            ..request(&file_dest, &source)
        })
        .unwrap_err()
        .contains("not a directory"));
    }

    #[test]
    fn refuses_a_silent_overwrite_and_force_replaces_the_identity() {
        let root = tempfile::tempdir().unwrap();
        let first = fixture(root.path(), "first", b"first-bytes");
        let second = fixture(root.path(), "second", b"second-bytes");
        let dest = root.path().join("trusted");
        let receipt = install(&request(&dest, &first)).unwrap();
        let first_digest = receipt.sha256.clone();

        let error = install(&request(&dest, &second)).unwrap_err();
        assert!(error.contains("already exists"), "{error}");
        assert_eq!(
            fs::read(dest.join(installed_binary_name())).unwrap(),
            b"first-bytes"
        );

        let replaced = install(&InstallRequest {
            force: true,
            ..request(&dest, &second)
        })
        .unwrap();
        assert_ne!(replaced.sha256, first_digest);
        assert_eq!(
            fs::read(dest.join(installed_binary_name())).unwrap(),
            b"second-bytes"
        );
        let record = fs::read_to_string(dest.join(format!(
            "{}{}",
            installed_binary_name(),
            CHECKSUM_SUFFIX
        )))
        .unwrap();
        assert!(record.starts_with(&replaced.sha256), "{record}");
    }

    #[test]
    fn a_corrupt_checksum_record_can_never_be_written_by_a_successful_install() {
        let root = tempfile::tempdir().unwrap();
        let source = fixture(root.path(), "faktor-cli", b"payload");
        let dest = root.path().join("trusted");
        let receipt = install(&request(&dest, &source)).unwrap();
        let record = fs::read_to_string(&receipt.checksum).unwrap();
        let recorded = record.split_whitespace().next().unwrap();
        assert_eq!(recorded.len(), 64, "{record}");
        assert!(recorded.chars().all(|c| c.is_ascii_hexdigit()), "{record}");
        assert_eq!(recorded, sha256_of(&receipt.executable).unwrap());
        assert_eq!(
            record.split_whitespace().last().unwrap(),
            installed_binary_name()
        );
    }
}
