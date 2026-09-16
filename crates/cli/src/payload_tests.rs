//! Adversarial tests of the operator-staged payload contract: hostile
//! names, missing files (exact expected path), symlink redirection,
//! world-readable/writable modes on unix, oversized payloads and corrupt
//! contents are all typed refusals, never a partial load.

use super::*;

fn staged(name: &str, body: &[u8]) -> (tempfile::TempDir, PayloadDir) {
    let dir = tempfile::tempdir().unwrap();
    let payloads = PayloadDir::new(dir.path());
    let path = dir.path().join(name);
    std::fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    (dir, payloads)
}

const PEM: &str = "-----BEGIN PRIVATE KEY-----\nMIIB\n-----END PRIVATE KEY-----\n";

#[test]
fn payload_names_are_plain_bounded_ascii_without_traversal() {
    for ok in ["key.pem", "github-app.pem", "a", "with-dash_and.dot"] {
        assert!(
            PayloadDir::validate_name(ok).is_ok(),
            "{ok} must be accepted"
        );
    }
    for bad in [
        "",
        ".",
        "..",
        "../escape.pem",
        "dir/key.pem",
        "dir\\key.pem",
        "C:key.pem",
        "key with space",
        "key\nnewline",
        "k\u{00e9}y",
        "\u{7f}",
    ] {
        assert!(
            PayloadDir::validate_name(bad).is_err(),
            "{bad:?} must be refused"
        );
    }
    let too_long = "k".repeat(MAX_PAYLOAD_NAME_BYTES + 1);
    assert!(PayloadDir::validate_name(&too_long).is_err());
    let exact = "k".repeat(MAX_PAYLOAD_NAME_BYTES);
    assert!(PayloadDir::validate_name(&exact).is_ok());
}

#[test]
fn missing_payload_refuses_with_the_exact_expected_path() {
    let dir = tempfile::tempdir().unwrap();
    let payloads = PayloadDir::new(dir.path().join("payloads"));
    let expected = dir.path().join("payloads").join("github-app.pem");
    match payloads.load_private_key_pem("github-app.pem") {
        Err(PayloadError::Missing { path }) => assert_eq!(path, expected),
        other => panic!("expected Missing with {expected:?}, got {other:?}"),
    }
    match payloads.load_secret("webhook.secret") {
        Err(PayloadError::Missing { path }) => {
            assert_eq!(path, dir.path().join("payloads").join("webhook.secret"))
        }
        other => panic!("expected Missing, got {other:?}"),
    }
}

#[test]
fn traversal_name_is_refused_before_any_filesystem_access() {
    // The escaping file EXISTS: the name rule alone must refuse it.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("outside.pem"), PEM).unwrap();
    let nested = dir.path().join("payloads");
    std::fs::create_dir(&nested).unwrap();
    let payloads = PayloadDir::new(nested);
    assert!(matches!(
        payloads.load_private_key_pem("../outside.pem"),
        Err(PayloadError::Name { .. })
    ));
}

#[cfg(unix)]
#[test]
fn unix_world_readable_private_key_is_refused_with_its_mode() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, payloads) = staged("key.pem", PEM.as_bytes());
    let path = dir.path().join("key.pem");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    match payloads.load_private_key_pem("key.pem") {
        Err(PayloadError::TooPermissive {
            path: refused,
            mode: 0o644,
        }) => assert_eq!(refused, path),
        other => panic!("expected TooPermissive(0644), got {other:?}"),
    }
    // Group-only access is refused too.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    assert!(matches!(
        payloads.load_private_key_pem("key.pem"),
        Err(PayloadError::TooPermissive { .. })
    ));
}

#[cfg(unix)]
#[test]
fn unix_world_writable_payload_directory_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, payloads) = staged("key.pem", PEM.as_bytes());
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    match payloads.load_private_key_pem("key.pem") {
        Err(PayloadError::DirectoryTooPermissive { mode: 0o777, .. }) => {}
        other => panic!("expected DirectoryTooPermissive, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn unix_symlink_payload_is_refused() {
    let (dir, payloads) = staged("real.pem", PEM.as_bytes());
    std::os::unix::fs::symlink(dir.path().join("real.pem"), dir.path().join("link.pem")).unwrap();
    match payloads.load_private_key_pem("link.pem") {
        Err(PayloadError::NotAFile { path }) => assert_eq!(path, dir.path().join("link.pem")),
        other => panic!("expected NotAFile, got {other:?}"),
    }
}

#[test]
fn private_key_envelope_and_utf8_are_strictly_checked() {
    let (_dir, payloads) = staged("key.pem", PEM.as_bytes());
    assert!(payloads.load_private_key_pem("key.pem").is_ok());

    let (_dir2, payloads2) = staged("bad.pem", b"not a pem at all\n");
    match payloads2.load_private_key_pem("bad.pem") {
        Err(PayloadError::Malformed { message, .. }) => {
            assert!(message.contains("PRIVATE KEY"), "{message}")
        }
        other => panic!("expected Malformed, got {other:?}"),
    }

    // BEGIN without END is corrupt, not a key.
    let (_dir3, payloads3) = staged("half.pem", b"-----BEGIN PRIVATE KEY-----\nMIIB\n");
    assert!(matches!(
        payloads3.load_private_key_pem("half.pem"),
        Err(PayloadError::Malformed { .. })
    ));

    // Non-UTF8 bytes are refused typed.
    let (_dir4, payloads4) = staged("bin.pem", &[0xff, 0xfe, 0x00, 0x01]);
    assert!(matches!(
        payloads4.load_private_key_pem("bin.pem"),
        Err(PayloadError::Malformed { .. })
    ));

    // An empty file is corrupt, never "an empty key".
    let (_dir5, payloads5) = staged("empty.pem", b"");
    assert!(matches!(
        payloads5.load_private_key_pem("empty.pem"),
        Err(PayloadError::Malformed { .. })
    ));
}

#[test]
fn secrets_are_trimmed_but_reject_inner_whitespace_and_oversize() {
    let (_dir, payloads) = staged("webhook.secret", b"super-secret\n");
    assert_eq!(
        payloads.load_secret("webhook.secret").unwrap(),
        "super-secret"
    );

    let (_dir2, payloads2) = staged("space.secret", b"two words\n");
    match payloads2.load_secret("space.secret") {
        Err(PayloadError::Malformed { message, .. }) => {
            assert!(message.contains("whitespace"), "{message}")
        }
        other => panic!("expected Malformed, got {other:?}"),
    }

    let (_dir3, payloads3) = staged(
        "big.secret",
        &vec![b'a'; MAX_SECRET_PAYLOAD_BYTES as usize + 1],
    );
    match payloads3.load_secret("big.secret") {
        Err(PayloadError::TooLarge { max, .. }) => assert_eq!(max, MAX_SECRET_PAYLOAD_BYTES),
        other => panic!("expected TooLarge, got {other:?}"),
    }

    // A directory is never a payload.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("nested")).unwrap();
    let payloads = PayloadDir::new(dir.path());
    assert!(matches!(
        payloads.load_secret("nested"),
        Err(PayloadError::NotAFile { .. })
    ));
}

#[test]
fn oversized_private_key_pem_is_refused_by_the_kind_bound() {
    let mut body = Vec::from(&b"-----BEGIN PRIVATE KEY-----\n"[..]);
    body.extend(vec![b'A'; MAX_PRIVATE_KEY_PAYLOAD_BYTES as usize]);
    body.extend_from_slice(b"\n-----END PRIVATE KEY-----\n");
    let (_dir, payloads) = staged("huge.pem", &body);
    match payloads.load_private_key_pem("huge.pem") {
        Err(PayloadError::TooLarge { max, .. }) => assert_eq!(max, MAX_PRIVATE_KEY_PAYLOAD_BYTES),
        other => panic!("expected TooLarge, got {other:?}"),
    }
}

#[test]
fn a_file_in_place_of_the_payload_directory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("payloads"), b"not a directory").unwrap();
    let payloads = PayloadDir::new(dir.path().join("payloads"));
    match payloads.load_secret("any.secret") {
        Err(PayloadError::NotADirectory { path }) => {
            assert_eq!(path, dir.path().join("payloads"))
        }
        other => panic!("expected NotADirectory, got {other:?}"),
    }
}

#[test]
fn refused_paths_never_include_the_payload_bytes() {
    // Error rendering is operator-facing: it must name paths, never contents.
    let (_dir, payloads) = staged("secret.txt", b"top-secret-value\n");
    let rendered = payloads
        .load_private_key_pem("secret.txt")
        .unwrap_err()
        .to_string();
    assert!(!rendered.contains("top-secret-value"), "{rendered}");
    assert!(rendered.contains("secret.txt"), "{rendered}");
}
