// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! Credential rotation during an IN-FLIGHT provider probe.
//!
//! The contract is generation-fenced: the credential bytes are resolved once
//! when the call starts, and a concurrent rotate affects only the NEXT call.
//! A probe must never mix the old and new material, fail spuriously, or send
//! the new secret on a request that already resolved the old one. The fake
//! origin records exactly which Authorization header each request carried;
//! that record (not a SUT status) is the oracle.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use faktor_foundation::providers::{ConnectionDraft, ProviderService};
use faktor_foundation::{FoundationDb, SecretVault, VerificationState};

const OLD: &str = "sk-rotation-OLD-0123456789abcdef";
const NEW: &str = "sk-rotation-NEW-fedcba9876543210";

fn draft(endpoint: String, credential_ref: String) -> ConnectionDraft {
    ConnectionDraft {
        id: "rotation".into(),
        family: "openai".into(),
        label: "rotation".into(),
        endpoint: Some(endpoint),
        credential_ref: Some(credential_ref),
        models: Vec::new(),
        allow_loopback: Some(true),
        api_flavor: None,
        trust_origin: None,
        connect_timeout_ms: Some(1_000),
        first_byte_timeout_ms: Some(2_000),
        total_timeout_ms: Some(5_000),
        keep_alive: None,
        proxy_url: None,
        tls_mode: None,
        ca_cert_ref: None,
    }
}

#[tokio::test]
async fn rotate_during_an_in_flight_probe_is_generation_fenced() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_by_server = seen.clone();
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        for request_index in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut authorization = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                if line == "\r\n" {
                    break;
                }
                if line.to_ascii_lowercase().starts_with("authorization:") {
                    authorization = line
                        .split_once(':')
                        .map(|(_, value)| value.trim().to_string())
                        .unwrap_or_default();
                }
            }
            seen_by_server.lock().unwrap().push(authorization);
            if request_index == 0 {
                started_tx.send(()).unwrap();
                // Hold the first request in flight until the rotate lands.
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            let body = r#"{"data":[{"id":"m"}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
        }
    });

    let dir = tempfile::tempdir().unwrap();
    let db = FoundationDb::open(dir.path().join("f.db")).unwrap();
    let vault = SecretVault::new(db.clone(), dir.path());
    let reference = vault.store("provider", "rotation", OLD.as_bytes()).unwrap();
    let service = Arc::new(ProviderService::new(db, vault.clone()));
    service
        .save(
            &draft(format!("http://{addr}"), reference.handle.clone()),
            None,
        )
        .unwrap();

    let probe = {
        let service = service.clone();
        tokio::spawn(async move { service.probe("rotation").await })
    };
    // Wait until the first request has resolved OLD and is held in flight.
    tokio::task::spawn_blocking(move || started_rx.recv_timeout(Duration::from_secs(5)))
        .await
        .unwrap()
        .unwrap();
    // Rotate while the request is in flight. The SAME handle keeps its
    // generation fence; only the material changes.
    let rotated = vault.rotate(&reference.handle, NEW.as_bytes()).unwrap();
    assert_eq!(rotated.generation, 2);
    release_tx.send(()).unwrap();
    let first = probe.await.unwrap().unwrap();
    assert_eq!(first.state, faktor_foundation::HealthState::Ready);
    // The old generation's success must NOT verify the new material.
    assert_eq!(
        vault.get(&reference.handle).unwrap().verification,
        VerificationState::Unverified,
        "a late completion from the in-flight generation must not verify the rotated one"
    );

    // The next call resolves the new generation.
    let second = service.probe("rotation").await.unwrap();
    assert_eq!(second.state, faktor_foundation::HealthState::Ready);
    assert_eq!(
        vault.get(&reference.handle).unwrap().verification,
        VerificationState::Verified,
        "the probe of the current generation verifies it"
    );
    server.join().unwrap();

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!(
        seen[0].contains(&format!("Bearer {OLD}")),
        "the in-flight request must keep the generation it resolved: {seen:?}"
    );
    assert!(
        !seen[0].contains(NEW),
        "the in-flight request must never mix in the rotated secret: {seen:?}"
    );
    assert!(
        seen[1].contains(&format!("Bearer {NEW}")),
        "the next call must use the new generation: {seen:?}"
    );
    assert!(
        !seen[1].contains(OLD),
        "the next call must not replay the old secret: {seen:?}"
    );
}
