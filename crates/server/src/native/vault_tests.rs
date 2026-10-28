// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! Adversarial credential-vault route tests over the real native listener.
//!
//! The reviewed wire contract owns the expectations: opaque handles only,
//! byte-identical 404 for foreign and missing handles, typed 400 for
//! malformed bodies, typed conflict when a deleted reference is used, and a
//! typed 500 degradation when the machine secret store is unavailable. A
//! planted secret is scanned against EVERY response body.

use reqwest::StatusCode;
use serde_json::{json, Value};

use super::foundation_r8::foundation_r8_tests::Rig;

/// A recognizable planted credential value (shape accepted by the local
/// secret scanner, never a real key).
const PLANTED: &str = "sk-PLANTEDvaultSECRET0123456789abcdef";
const PLANTED_ROTATED: &str = "sk-PLANTEDvaultROTATED9876543210fedcba";

fn percent_encode_handle(handle: &str) -> String {
    let mut out = String::new();
    for byte in handle.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn code(body: &Value) -> &str {
    body["error"]["code"]
        .as_str()
        .or_else(|| body["code"].as_str())
        .unwrap_or("")
}

fn handle_of(body: &Value) -> String {
    body["handle"]
        .as_str()
        .unwrap_or_else(|| panic!("no opaque handle in {body}"))
        .to_string()
}

#[tokio::test]
async fn credential_routes_store_rotate_list_delete_and_never_echo_the_secret() {
    let rig = Rig::new().await;
    // Store: the response is the opaque reference only.
    let (status, stored) = rig
        .post(
            "/native/providers/credentials",
            json!({"kind": "provider", "label": "prod key", "secret": PLANTED}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{stored}");
    assert!(!stored.to_string().contains(PLANTED), "{stored}");
    let handle = handle_of(&stored);
    assert!(handle.starts_with("vault://provider/"), "{handle}");

    // List: references + verification state only.
    let (status, listed) = rig.get("/native/providers/credentials").await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(!listed.to_string().contains(PLANTED), "{listed}");
    let credentials = listed["credentials"].as_array().unwrap();
    assert_eq!(credentials.len(), 1, "{listed}");
    assert_eq!(credentials[0]["handle"], handle);
    assert_eq!(credentials[0]["generation"], 1);

    // Rotate: same handle, generation 2, only the new material is live.
    let (status, rotated) = rig
        .post(
            "/native/providers/credentials/rotate",
            json!({"handle": handle, "secret": PLANTED_ROTATED}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{rotated}");
    assert_eq!(rotated["generation"], 2, "{rotated}");
    let encoded = rotated.to_string();
    assert!(
        !encoded.contains(PLANTED) && !encoded.contains(PLANTED_ROTATED),
        "{rotated}"
    );

    // A connection can bind the reference; every provider/config/diagnostic
    // response carries the opaque handle only.
    let (status, saved) = rig
        .post(
            "/native/providers/connections",
            json!({
                "id": "vault-bound", "family": "ollama", "label": "bound",
                "endpoint": "http://127.0.0.1:9", "models": ["llama3:8b"],
                "allow_loopback": true, "credential_ref": handle,
                "connect_timeout_ms": 100, "total_timeout_ms": 200
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["credential_ref"], handle);

    // Probe with a dead endpoint: a typed failed health, never the secret.
    let (status, health) = rig
        .post(
            "/native/catalog/probe",
            json!({"connectionId": "vault-bound"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{health}");
    assert!(!health.to_string().contains(PLANTED));

    // Delete: local revocation only, and the response never echoes bytes.
    let path = format!(
        "/native/providers/credentials/{}",
        percent_encode_handle(&handle)
    );
    let (status, deleted) = rig.delete(&path).await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted["reference"]["handle"], handle);
    assert_eq!(deleted["remoteRevoked"], false);
    let encoded = deleted.to_string();
    assert!(
        !encoded.contains(PLANTED) && !encoded.contains(PLANTED_ROTATED),
        "{deleted}"
    );
    // Delete-then-use is a typed CONFLICT on the probe path (a local
    // revocation, not a missing foreign handle), and the response carries no
    // material.
    let (status, refused) = rig
        .post(
            "/native/catalog/probe",
            json!({"connectionId": "vault-bound"}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    let encoded = refused.to_string();
    assert!(
        !encoded.contains(PLANTED) && !encoded.contains(PLANTED_ROTATED),
        "{refused}"
    );
    // Binding a NEW connection to the deleted reference is refused typed at
    // save time, not deferred to the first call.
    let (status, refused) = rig
        .post(
            "/native/providers/connections",
            json!({
                "id": "after-delete", "family": "ollama", "label": "after",
                "endpoint": "http://127.0.0.1:9", "models": ["llama3:8b"],
                "allow_loopback": true, "credential_ref": handle
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(code(&refused), "malformed", "{refused}");
    assert!(!refused.to_string().contains(PLANTED), "{refused}");
    // Rotating a revoked handle is refused typed (never resurrected).
    let (status, refused) = rig
        .post(
            "/native/providers/credentials/rotate",
            json!({"handle": handle, "secret": PLANTED_ROTATED}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");

    // Planted-secret scan over EVERY remaining read channel.
    let mut channels: Vec<(String, String)> = Vec::new();
    for (path, value) in [
        (
            "/native/providers/credentials",
            rig.get("/native/providers/credentials").await.1,
        ),
        (
            "/native/providers/connections",
            rig.get("/native/providers/connections").await.1,
        ),
        (
            "/native/config/snapshot",
            rig.get("/native/config/snapshot").await.1,
        ),
        (
            "/native/config/export",
            rig.get("/native/config/export").await.1,
        ),
        (
            "/native/diagnostics/preview",
            rig.get("/native/diagnostics/preview").await.1,
        ),
    ] {
        channels.push((path.to_string(), value.to_string()));
    }
    let (_, export) = rig.post("/native/diagnostics/export", json!({})).await;
    channels.push(("/native/diagnostics/export".into(), export.to_string()));
    let (_, session_export) = rig
        .post(
            &format!("/native/session/{}/export", rig.session),
            json!({"redact": true}),
        )
        .await;
    channels.push((
        "/native/session/{id}/export".into(),
        session_export.to_string(),
    ));
    for (channel, body) in channels {
        assert!(
            !body.contains(PLANTED) && !body.contains(PLANTED_ROTATED),
            "planted vault secret leaked on {channel}: {body}"
        );
    }
}

#[tokio::test]
async fn malformed_bodies_are_typed_400_and_store_nothing() {
    let rig = Rig::new().await;
    let bad = [
        (
            json!({"kind": "provider", "label": "x", "secret": ""}),
            "malformed",
        ),
        (
            json!({"kind": "bad/kind", "label": "x", "secret": "value"}),
            "malformed",
        ),
        (
            json!({"kind": "provider", "label": "x".repeat(129), "secret": "value"}),
            "oversized",
        ),
        (
            json!({"kind": "provider", "label": format!("leak {PLANTED}"), "secret": "value"}),
            "malformed",
        ),
        (
            json!({"kind": "provider", "label": "x", "secret": "a".repeat(64 * 1024 + 1)}),
            "oversized",
        ),
        (json!({"kind": "provider", "label": "x"}), "malformed"),
        (
            json!({"kind": "provider", "label": "x", "secret": 7}),
            "malformed",
        ),
    ];
    for (body, expected_code) in bad {
        let (status, refused) = rig
            .post("/native/providers/credentials", body.clone())
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {refused}");
        assert_eq!(code(&refused), expected_code, "{body}: {refused}");
        assert!(!refused.to_string().contains(PLANTED), "{refused}");
    }
    // Rotation with a malformed/foreign handle is a typed 400/404, never a
    // silent mint.
    for hostile in ["sk-live", "vault://provider", "vault://provider/x/y"] {
        let (status, refused) = rig
            .post(
                "/native/providers/credentials/rotate",
                json!({"handle": hostile, "secret": PLANTED}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{hostile}: {refused}");
    }
    let (_, listed) = rig.get("/native/providers/credentials").await;
    assert_eq!(listed["credentials"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn foreign_and_missing_handles_are_indistinguishable_at_the_wire() {
    let rig = Rig::new().await;
    // A handle minted in a DIFFERENT machine vault is foreign to this daemon.
    let foreign_dir = tempfile::tempdir().unwrap();
    let foreign_db =
        faktor_foundation::FoundationDb::open(foreign_dir.path().join("f.db")).unwrap();
    let foreign_vault = faktor_foundation::SecretVault::new(foreign_db, foreign_dir.path());
    let foreign = foreign_vault
        .store("provider", "foreign", b"foreign-secret")
        .unwrap()
        .handle;
    let missing = "vault://provider/00000000000000000000000000000000";
    let foreign_path = format!(
        "/native/providers/credentials/{}",
        percent_encode_handle(&foreign)
    );
    let missing_path = format!(
        "/native/providers/credentials/{}",
        percent_encode_handle(missing)
    );
    let (foreign_status, foreign_body) = rig.delete(&foreign_path).await;
    let (missing_status, missing_body) = rig.delete(&missing_path).await;
    assert_eq!(foreign_status, StatusCode::NOT_FOUND);
    assert_eq!(missing_status, StatusCode::NOT_FOUND);
    assert_eq!(
        foreign_body.to_string(),
        missing_body.to_string(),
        "a foreign handle must be indistinguishable from a missing one"
    );
    // The same indistinguishability on the rotate channel.
    let (foreign_status, foreign_body) = rig
        .post(
            "/native/providers/credentials/rotate",
            json!({"handle": foreign, "secret": "x"}),
        )
        .await;
    let (missing_status, missing_body) = rig
        .post(
            "/native/providers/credentials/rotate",
            json!({"handle": missing, "secret": "x"}),
        )
        .await;
    assert_eq!(foreign_status, StatusCode::NOT_FOUND);
    assert_eq!(missing_status, StatusCode::NOT_FOUND);
    assert_eq!(foreign_body.to_string(), missing_body.to_string());
}

#[tokio::test]
async fn unavailable_machine_secret_store_is_a_typed_500_never_a_fake_handle() {
    // The file-backed vault has no OS keychain dependency; an unavailable
    // store is the equivalent degradation. Occupy `<data>/vault` with a file
    // so the owner-only secrets directory cannot be created.
    let rig = Rig::new().await;
    std::fs::write(
        rig.dir.path().join("foundation.db").join("vault"),
        b"not a directory",
    )
    .unwrap();
    let (status, refused) = rig
        .post(
            "/native/providers/credentials",
            json!({"kind": "provider", "label": "x", "secret": PLANTED}),
        )
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{refused}");
    assert_eq!(code(&refused), "store", "{refused}");
    assert_eq!(refused["retryable"], false, "{refused}");
    assert!(!refused.to_string().contains(PLANTED), "{refused}");
    let (_, listed) = rig.get("/native/providers/credentials").await;
    assert_eq!(
        listed["credentials"].as_array().unwrap().len(),
        0,
        "a degraded store never claims a stored reference"
    );
}
