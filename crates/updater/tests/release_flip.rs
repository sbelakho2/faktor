//! End-to-end release flip: install v1, activate v2, restart THROUGH the
//! stable bootstrap launcher and observe the NEW process's reported release
//! digest, then roll back and observe v1 again. The assertion is on the
//! RUNNING binary (tag + executable path + reported digest), not on the
//! pointer metadata.
//!
//! This is an `#[ignore]`-gated `[fault]` test (it builds the workspace CLI
//! and execs real processes). Run it explicitly:
//!
//! ```text
//! cargo build -p faktor-cli
//! cargo test -p faktor-updater --test release_flip -- --ignored --nocapture
//! ```
//!
//! What is REAL here: the install layout, the launcher (a copy of the
//! workspace `faktor-cli`, running its production bootstrap entry), the
//! signed update manifests, the pointer swaps, the rollback and the exec.
//! What is a stand-in: the release binaries themselves are two tiny rustc-
//! compiled programs (distinct bytes, distinct tags) that print the release
//! identity their launcher verified; the update manifest/artifact transport
//! is fed from memory.

#![cfg(unix)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use faktor_updater::{
    manifest, DigestProbe, InstallLayout, ReleaseOutcome, RunningComponents, TrustedKey,
    TrustedKeys, UpdateError, Updater, UpdaterConfig, VersionRange,
};

const OS: &str = "darwin";
const ARCH: &str = "arm64";

struct MemoryFetcher {
    payloads: Mutex<HashMap<String, Vec<u8>>>,
}

impl MemoryFetcher {
    fn new() -> Self {
        MemoryFetcher {
            payloads: Mutex::new(HashMap::new()),
        }
    }

    fn serve(&self, url: &str, payload: &[u8]) {
        self.payloads
            .lock()
            .unwrap()
            .insert(url.to_string(), payload.to_vec());
    }
}

#[async_trait::async_trait]
impl faktor_updater::ArtifactFetcher for MemoryFetcher {
    async fn fetch_to(
        &self,
        url: &str,
        max_bytes: u64,
        sink: &mut (dyn tokio::io::AsyncWrite + Send + Unpin),
    ) -> Result<u64, UpdateError> {
        let payload = self
            .payloads
            .lock()
            .unwrap()
            .get(url)
            .cloned()
            .ok_or_else(|| UpdateError::Transport {
                artifact: url.to_string(),
                detail: "no payload served".into(),
            })?;
        if payload.len() as u64 > max_bytes {
            return Err(UpdateError::ArtifactTooLarge {
                artifact: url.to_string(),
                max_bytes,
            });
        }
        use tokio::io::AsyncWriteExt as _;
        sink.write_all(&payload).await.unwrap();
        Ok(payload.len() as u64)
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn target_dir(root: &Path) -> PathBuf {
    match std::env::var("CARGO_TARGET_DIR") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => root.join("target"),
    }
}

/// Build (incrementally) and locate the workspace `faktor-cli`. The build is
/// unconditional so the launcher under test is THIS revision's bootstrap.
fn faktor_cli_binary() -> PathBuf {
    if let Ok(explicit) = std::env::var("FAKTOR_E2E_LAUNCHER") {
        if !explicit.is_empty() {
            return PathBuf::from(explicit);
        }
    }
    let root = workspace_root();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        .args(["build", "-p", "faktor-cli"])
        .current_dir(&root)
        .status()
        .expect("cargo build -p faktor-cli");
    assert!(status.success(), "cargo build -p faktor-cli failed");
    let target = target_dir(&root);
    for profile in ["debug", "release"] {
        let candidate = target.join(profile).join("faktor-cli");
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!(
        "faktor-cli was built but not found under {}",
        target.display()
    );
}

/// Compile one tiny release binary with rustc: distinct bytes per tag, and it
/// prints exactly the release identity the launcher exported.
fn compile_release(dir: &Path, tag: &str) -> PathBuf {
    let source = dir.join(format!("{tag}.rs"));
    std::fs::write(
        &source,
        format!(
            r#"fn main() {{
    println!("tag={tag}");
    println!("digest={{}}", std::env::var("FAKTOR_RELEASE_DIGEST").unwrap_or_default());
    println!("id={{}}", std::env::var("FAKTOR_RELEASE_ID").unwrap_or_default());
    println!("exe={{}}", std::env::current_exe().unwrap().display());
    println!("install={{}}", std::env::var("FAKTOR_INSTALL_ROOT").unwrap_or_default());
    println!("args={{}}", std::env::args().skip(1).collect::<Vec<_>>().join(","));
}}"#
        ),
    )
    .unwrap();
    let out = dir.join(format!("{tag}-faktor"));
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let status = Command::new(rustc)
        .args(["--edition", "2021", "-O"])
        .arg(&source)
        .arg("-o")
        .arg(&out)
        .status()
        .expect("rustc release helper");
    assert!(status.success(), "rustc failed for tag {tag}");
    out
}

fn signed_manifest(
    key: &SigningKey,
    version: &str,
    generation: u64,
    payload: &[u8],
) -> (Vec<u8>, String) {
    let digest = manifest::sha256_hex(payload);
    let name = format!("faktor-cli-{version}-{OS}-{ARCH}");
    let url = format!("https://mirror.test/{version}/{name}");
    let mut doc = manifest::UpdateManifest {
        schema: manifest::UPDATE_MANIFEST_SCHEMA.to_string(),
        channel: faktor_updater::Channel::Stable,
        version: version.to_string(),
        commit: format!("{version:0>40}").replace('.', "b"),
        release_generation: Some(generation),
        artifacts: vec![manifest::Artifact {
            name,
            os: OS.into(),
            arch: ARCH.into(),
            sha256: digest.clone(),
            url,
            size: Some(payload.len() as u64),
        }],
        compatibility: manifest::Compatibility {
            cli: VersionRange::from_parts("0.1.0", "9.9.9").unwrap(),
            daemon: VersionRange::from_parts("0.1.0", "9.9.9").unwrap(),
            vscode: VersionRange::from_parts("*", "*").unwrap(),
            jetbrains: VersionRange::from_parts("*", "*").unwrap(),
            schema: faktor_updater::compat::SchemaRange { min: 1, max: 1 },
        },
        issued_at: 1_700_000_000_000,
        expires_at: 1_700_000_000_000 + 30 * 24 * 60 * 60 * 1000,
        certification: None,
        signature: None,
    };
    let signing_payload = doc.signing_payload().unwrap();
    let signature = key.sign(&signing_payload);
    doc.signature = Some(manifest::ManifestSignature {
        algorithm: "ed25519".into(),
        identity: "operator".into(),
        public_key: BASE64.encode(key.verifying_key().to_bytes()),
        value: BASE64.encode(signature.to_bytes()),
    });
    (serde_json::to_vec(&doc).unwrap(), digest)
}

/// Run the stable bootstrap launcher and parse the release identity the
/// RUNNING binary reported on stdout.
struct Running {
    tag: String,
    digest: String,
    id: String,
    exe: PathBuf,
    install: String,
}

fn launch_through_bootstrap(launcher: &Path, args: &[&str]) -> Running {
    let output = Command::new(launcher)
        .args(args)
        .output()
        .expect("spawn the bootstrap launcher");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the bootstrap launcher refused (exit {:?}): {stderr}",
        output.status.code()
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let field = |name: &str| -> String {
        stdout
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")))
            .unwrap_or_else(|| panic!("no {name}= in the running binary output: {stdout}"))
            .to_string()
    };
    assert!(
        stderr.contains("launcher: activating release"),
        "the launcher must announce the release it resolved: {stderr}"
    );
    Running {
        tag: field("tag"),
        digest: field("digest"),
        id: field("id"),
        exe: PathBuf::from(field("exe")),
        install: field("install"),
    }
}

#[tokio::test]
#[ignore = "[fault] real v1->v2->v1 release flip through the stable bootstrap launcher; run explicitly with --ignored --nocapture"]
async fn the_running_binary_changes_with_the_activated_release() {
    let cli = faktor_cli_binary();
    eprintln!("[e2e] launcher source: {}", cli.display());

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("install");
    let helpers = dir.path().join("helpers");
    std::fs::create_dir_all(&helpers).unwrap();
    let v1_bytes = std::fs::read(compile_release(&helpers, "v1")).unwrap();
    let v2_bytes = std::fs::read(compile_release(&helpers, "v2")).unwrap();
    assert_ne!(
        manifest::sha256_hex(&v1_bytes),
        manifest::sha256_hex(&v2_bytes),
        "the two release artifacts must be distinct bytes"
    );

    let layout = InstallLayout::open(&root).unwrap();
    let launcher = layout.install_launcher(&cli).unwrap();
    assert_eq!(launcher, root.join("launcher"));
    assert!(layout.launcher_path().is_file());

    let key = SigningKey::from_bytes(&[9u8; 32]);
    let trusted: TrustedKeys = TrustedKeys::new(vec![TrustedKey::from_base64(
        "operator",
        &BASE64.encode(key.verifying_key().to_bytes()),
    )
    .unwrap()])
    .unwrap();
    let fetcher = Arc::new(MemoryFetcher::new());
    let updater = Updater::new(
        UpdaterConfig {
            channel: faktor_updater::Channel::Stable,
            install_root: root.clone(),
            keys: trusted,
            max_artifact_bytes: 1024 * 1024,
            clock_skew_ms: manifest::DEFAULT_CLOCK_SKEW_MS,
            host_os: OS.into(),
            host_arch: ARCH.into(),
            local_version: "0.1.0".into(),
            allow_legacy_manifests_once: false,
        },
        Arc::new(faktor_updater::MemoryUpdaterStore::new()),
        fetcher.clone(),
        Arc::new(DigestProbe),
    )
    .unwrap();
    let mut now = 1_700_000_000_000i64;

    // --- install v1 -------------------------------------------------------
    let (signed_v1, v1_digest) = signed_manifest(&key, "0.1.0", 1, &v1_bytes);
    fetcher.serve(
        "https://mirror.test/0.1.0/faktor-cli-0.1.0-darwin-arm64",
        &v1_bytes,
    );
    now += 1;
    let staged_v1 = updater
        .stage_release(
            &signed_v1,
            &RunningComponents::from_strs(Some("0.1.0"), Some("0.1.0"), None, None, Some(1))
                .unwrap(),
            None,
            now,
        )
        .await
        .unwrap();
    now += 1;
    assert!(matches!(
        updater.activate_release(now, None).unwrap(),
        ReleaseOutcome::Activated { .. }
    ));
    now += 1;
    updater.finalize_release(&v1_digest, now).unwrap();
    let v1 = launch_through_bootstrap(&launcher, &["probe"]);
    eprintln!(
        "[e2e] v1 running: tag={} digest={} exe={}",
        v1.tag,
        v1.digest,
        v1.exe.display()
    );
    assert_eq!(v1.tag, "v1");
    assert_eq!(v1.digest, v1_digest);
    assert_eq!(v1.id, staged_v1.release_id);
    assert_eq!(v1.exe, layout.release_binary(&staged_v1.release_id));
    assert_eq!(v1.install, root.display().to_string());

    // --- activate v2 ------------------------------------------------------
    let (signed_v2, v2_digest) = signed_manifest(&key, "0.2.0", 2, &v2_bytes);
    fetcher.serve(
        "https://mirror.test/0.2.0/faktor-cli-0.2.0-darwin-arm64",
        &v2_bytes,
    );
    now += 1;
    let staged_v2 = updater
        .stage_release(
            &signed_v2,
            &RunningComponents::from_strs(Some("0.1.0"), Some("0.1.0"), None, None, Some(1))
                .unwrap(),
            None,
            now,
        )
        .await
        .unwrap();
    now += 1;
    assert!(matches!(
        updater.activate_release(now, None).unwrap(),
        ReleaseOutcome::Activated { .. }
    ));
    // The supervisor restarts through the launcher: the NEW process must
    // report the NEW digest.
    let v2 = launch_through_bootstrap(&launcher, &["probe"]);
    eprintln!(
        "[e2e] v2 running: tag={} digest={} exe={}",
        v2.tag,
        v2.digest,
        v2.exe.display()
    );
    assert_eq!(v2.tag, "v2");
    assert_eq!(v2.digest, v2_digest);
    assert_ne!(v2.digest, v1.digest, "the RUNNING release digest changed");
    assert_ne!(v2.exe, v1.exe, "the RUNNING executable path changed");
    assert_eq!(v2.exe, layout.release_binary(&staged_v2.release_id));
    now += 1;
    updater.finalize_release(&v2_digest, now).unwrap();

    // --- roll back to v1 --------------------------------------------------
    now += 1;
    updater.rollback(now).unwrap();
    let rolled_back = launch_through_bootstrap(&launcher, &["probe"]);
    eprintln!(
        "[e2e] after rollback running: tag={} digest={} exe={}",
        rolled_back.tag,
        rolled_back.digest,
        rolled_back.exe.display()
    );
    assert_eq!(rolled_back.tag, "v1");
    assert_eq!(rolled_back.digest, v1_digest);
    assert_eq!(rolled_back.exe, v1.exe);
    assert_ne!(rolled_back.digest, v2_digest);

    // --- the real CLI reports its build identity additively ---------------
    // (the exact contract the launcher exports; `faktor build` is what a
    // supervisor runs to observe which artifact is live)
    let cli_digest = {
        use sha2::Digest as _;
        let mut hasher = sha2::Sha256::new();
        hasher.update(std::fs::read(&cli).unwrap());
        format!("{:x}", hasher.finalize())
    };
    let report = Command::new(&cli)
        .arg("build")
        .env("FAKTOR_RELEASE_ID", "0.2.0-e2e")
        .env("FAKTOR_RELEASE_DIGEST", &cli_digest)
        .env("FAKTOR_INSTALL_ROOT", &root)
        .output()
        .expect("run faktor build");
    assert!(report.status.success());
    let parsed: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
    assert_eq!(parsed["schema"], "faktor-build-report/v1");
    assert_eq!(parsed["release_id"], "0.2.0-e2e");
    assert_eq!(parsed["release_digest"], cli_digest.as_str());
    assert_eq!(
        parsed["self_digest_matches_release"], true,
        "the running CLI's own hash must match the verified release digest"
    );
    assert_eq!(parsed["version"], faktor_core::VERSION);
    eprintln!(
        "[e2e] real CLI build report: version={} release_digest={} matches={}",
        parsed["version"], parsed["release_digest"], parsed["self_digest_matches_release"]
    );

    // --- a corrupted activated binary is refused, never executed ----------
    let tampered = layout.release_binary(&staged_v1.release_id);
    let mut bytes = std::fs::read(&tampered).unwrap();
    bytes.push(0x00);
    std::fs::write(&tampered, &bytes).unwrap();
    let output = Command::new(&launcher)
        .arg("probe")
        .output()
        .expect("spawn the bootstrap launcher");
    assert_eq!(output.status.code(), Some(3), "launch refusal exit code");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("digest mismatch"),
        "the refusal names the digest mismatch: {stderr}"
    );
    eprintln!("[e2e] corrupted binary refused: {}", stderr.trim());
    eprintln!("[e2e] ALL E2E ASSERTIONS PASSED (running binary changed: {v1_digest:.12} -> {v2_digest:.12} -> {v1_digest:.12})");
}
