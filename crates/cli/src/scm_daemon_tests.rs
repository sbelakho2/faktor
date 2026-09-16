//! Adversarial tests of the GitHub App daemon construction: fail-closed
//! payload staging (missing/world-readable/corrupt), a mock GitHub base URL
//! that syncs installations/repositories durably, webhook delivery that
//! re-syncs idempotently, and the disabled-parity construction that reads
//! nothing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use faktor_scm::{ScmStore, SqliteScmStore};
use faktor_server::native::scm_webhook::WebhookSink;

use super::{build_scm_daemon, TEST_PRIVATE_KEY};
use crate::config::{CloudCfg, CloudGithubAppCfg};
use crate::test_http::{MockServer, Reply};

const WEBHOOK_SECRET: &[u8] = b"hook-secret";
const INSTALLATION_ID: u64 = 7;

fn stage_payload(root: &Path, name: &str, body: &[u8]) {
    std::fs::create_dir_all(root).unwrap();
    let path = root.join(name);
    std::fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

fn github_app_cfg(base: &str) -> CloudCfg {
    CloudCfg {
        enabled: true,
        database: None,
        scm_database: None,
        payload_dir: None,
        sso: None,
        github_app: Some(CloudGithubAppCfg {
            enabled: true,
            app_id: Some(12345),
            private_key: Some("app.pem".into()),
            webhook_secret: Some("hook.secret".into()),
            api_base: Some(base.to_string()),
            organization: Some("org_acme".into()),
            user_agent: None,
            page_size: None,
            max_pages: None,
        }),
    }
}

fn token_reply() -> Reply {
    Reply::json(
        200,
        serde_json::json!({
            "token": "ghs_test",
            "expires_at": "2099-01-01T00:00:00Z",
            "permissions": {
                "contents": "write",
                "pull_requests": "write",
                "issues": "write",
                "metadata": "read",
            },
        }),
    )
}

fn installation_reply(id: u64) -> Reply {
    Reply::json(
        200,
        serde_json::json!([{
            "id": id,
            "account": {"login": "acme", "type": "Organization"},
            "permissions": {"contents": "write"},
        }]),
    )
}

fn repositories_reply() -> Reply {
    Reply::json(
        200,
        serde_json::json!({
            "total_count": 1,
            "repositories": [{
                "id": 11,
                "name": "widgets",
                "full_name": "acme/widgets",
                "default_branch": "main",
                "private": true,
                "archived": false,
                "html_url": "http://example.test/acme/widgets",
                "owner": {"login": "acme"},
            }],
        }),
    )
}

fn signed_webhook(body: &[u8], delivery: &str) -> faktor_scm::WebhookHeaders {
    faktor_scm::WebhookHeaders {
        signature_256: Some(format!(
            "sha256={}",
            faktor_scm::hmac_sha256_hex(WEBHOOK_SECRET, body)
        )),
        delivery_id: Some(delivery.to_string()),
        event: Some("installation".to_string()),
        timestamp_ms: None,
    }
}

fn store_path(dir: &Path) -> PathBuf {
    dir.join("scm.db")
}

#[tokio::test]
async fn missing_private_key_refuses_with_the_exact_path_and_builds_no_scm() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = github_app_cfg("http://127.0.0.1:9");
    let store: Arc<dyn ScmStore> = Arc::new(SqliteScmStore::open(&store_path(dir.path())).unwrap());
    let transport =
        Arc::new(faktor_provider::egress::PolicyCheckedHttpTransport::with_policy(None));
    let err = build_scm_daemon(&cfg, dir.path(), store.clone(), transport).unwrap_err();
    let expected = dir.path().join("payloads").join("app.pem");
    assert!(
        err.contains(&expected.display().to_string()),
        "the refusal must name {expected:?}: {err}"
    );
    assert!(
        store
            .repositories_for_organization("org_acme", 0, 10)
            .unwrap()
            .is_empty(),
        "no half-wired scm state exists"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn world_readable_private_key_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let payloads = dir.path().join("payloads");
    stage_payload(&payloads, "app.pem", TEST_PRIVATE_KEY.as_bytes());
    stage_payload(&payloads, "hook.secret", WEBHOOK_SECRET);
    std::fs::set_permissions(
        payloads.join("app.pem"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let cfg = github_app_cfg("http://127.0.0.1:9");
    let store: Arc<dyn ScmStore> = Arc::new(SqliteScmStore::open(&store_path(dir.path())).unwrap());
    let transport =
        Arc::new(faktor_provider::egress::PolicyCheckedHttpTransport::with_policy(None));
    let err = build_scm_daemon(&cfg, dir.path(), store, transport).unwrap_err();
    assert!(err.contains("0600"), "{err}");
}

#[tokio::test]
async fn corrupt_private_key_is_refused_typed() {
    let dir = tempfile::tempdir().unwrap();
    let payloads = dir.path().join("payloads");
    stage_payload(&payloads, "app.pem", b"not a pem\n");
    stage_payload(&payloads, "hook.secret", WEBHOOK_SECRET);
    let cfg = github_app_cfg("http://127.0.0.1:9");
    let store: Arc<dyn ScmStore> = Arc::new(SqliteScmStore::open(&store_path(dir.path())).unwrap());
    let transport =
        Arc::new(faktor_provider::egress::PolicyCheckedHttpTransport::with_policy(None));
    let err = build_scm_daemon(&cfg, dir.path(), store, transport).unwrap_err();
    assert!(err.contains("PRIVATE KEY"), "{err}");
}

#[tokio::test]
async fn disabled_github_app_builds_nothing_and_reads_no_payload() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn ScmStore> = Arc::new(SqliteScmStore::open(&store_path(dir.path())).unwrap());
    let transport =
        Arc::new(faktor_provider::egress::PolicyCheckedHttpTransport::with_policy(None));
    let mut cfg = github_app_cfg("http://127.0.0.1:9");
    cfg.github_app.as_mut().unwrap().enabled = false;
    assert!(build_scm_daemon(&cfg, dir.path(), store, transport)
        .unwrap()
        .is_none());
    assert!(
        !dir.path().join("payloads").exists(),
        "disabled parity: no payload directory is created or read"
    );
}

#[tokio::test]
async fn mock_github_syncs_installations_and_repositories_durably() {
    let dir = tempfile::tempdir().unwrap();
    let payloads = dir.path().join("payloads");
    stage_payload(&payloads, "app.pem", TEST_PRIVATE_KEY.as_bytes());
    stage_payload(&payloads, "hook.secret", WEBHOOK_SECRET);
    let mock = MockServer::start().await;
    mock.push(
        "GET",
        "/app/installations",
        installation_reply(INSTALLATION_ID),
    );
    mock.push("POST", "/app/installations/7/access_tokens", token_reply());
    mock.push("GET", "/installation/repositories", repositories_reply());

    let path = store_path(dir.path());
    let store = Arc::new(SqliteScmStore::open(&path).unwrap());
    let transport =
        Arc::new(faktor_provider::egress::PolicyCheckedHttpTransport::with_policy(None));
    let cfg = github_app_cfg(&mock.base());
    let daemon = build_scm_daemon(&cfg, dir.path(), store.clone(), transport)
        .unwrap()
        .expect("enabled github app builds");
    let report = daemon.sync_all().await.unwrap();
    assert_eq!(report.installations, 1);
    assert_eq!(report.repositories, 1);

    let rows = store
        .repositories_for_organization("org_acme", 0, 10)
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].owner, "acme");
    assert_eq!(rows[0].name, "widgets");
    assert_eq!(rows[0].full_name, "acme/widgets");
    assert_eq!(store.installations().unwrap().len(), 1);

    // Durable: a fresh open of the same file sees the synced rows.
    drop(store);
    let reopened = SqliteScmStore::open(&path).unwrap();
    let rows: Arc<dyn ScmStore> = Arc::new(reopened);
    assert_eq!(
        rows.repositories_for_organization("org_acme", 0, 10)
            .unwrap()
            .len(),
        1
    );
    // The app JWT authenticated the installation page (never a static
    // token): the mint used the staged private key.
    let mints = mock.requests("POST", "/app/installations/7/access_tokens");
    assert_eq!(mints.len(), 1);
    let list = mock.requests("GET", "/app/installations");
    assert_eq!(list.len(), 1);
    let auth = list[0].header("authorization").unwrap_or_default();
    assert!(auth.starts_with("Bearer ey"), "{auth}");
}

#[tokio::test]
async fn webhook_delivery_resyncs_idempotently_and_bad_signatures_never_claim() {
    let dir = tempfile::tempdir().unwrap();
    let payloads = dir.path().join("payloads");
    stage_payload(&payloads, "app.pem", TEST_PRIVATE_KEY.as_bytes());
    stage_payload(&payloads, "hook.secret", WEBHOOK_SECRET);
    let mock = MockServer::start().await;
    mock.push(
        "GET",
        "/app/installations",
        installation_reply(INSTALLATION_ID),
    );
    mock.push("POST", "/app/installations/7/access_tokens", token_reply());
    mock.push("GET", "/installation/repositories", repositories_reply());
    mock.push("GET", "/installation/repositories", repositories_reply());

    let store = Arc::new(SqliteScmStore::open(&store_path(dir.path())).unwrap());
    let transport =
        Arc::new(faktor_provider::egress::PolicyCheckedHttpTransport::with_policy(None));
    let cfg = github_app_cfg(&mock.base());
    let daemon = build_scm_daemon(&cfg, dir.path(), store.clone(), transport)
        .unwrap()
        .expect("enabled github app builds");
    let run = tokio::spawn(daemon.clone().run());

    // The initial sync lands (bounded poll).
    // Background sync competes with full-suite certificate load; the
    // assertion (initial sync landed) is unchanged, the ceiling is
    // environment-independent.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(240);
    loop {
        if store
            .repositories_for_organization("org_acme", 0, 10)
            .unwrap()
            .len()
            == 1
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "initial sync never landed"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // A forged delivery is refused typed and claims nothing.
    let body = br#"{"installation":{"id":7},"action":"created"}"#;
    let forged = daemon.deliver(
        &faktor_scm::WebhookHeaders {
            signature_256: Some(format!("sha256={}", "00".repeat(32))),
            delivery_id: Some("d-forged".into()),
            event: Some("installation".into()),
            timestamp_ms: None,
        },
        body,
    );
    assert!(forged.is_err(), "a forged signature must be refused");
    assert!(store.webhook_deliveries(10).unwrap().is_empty());

    // A valid delivery triggers one idempotent re-sync (rows converge).
    let headers = signed_webhook(body, "d-1");
    let outcome = daemon.deliver(&headers, body).unwrap();
    assert!(matches!(outcome, faktor_scm::IngestOutcome::Accepted(_)));
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if mock.requests("GET", "/installation/repositories").len() >= 2 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "webhook sync never ran"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        store
            .repositories_for_organization("org_acme", 0, 10)
            .unwrap()
            .len(),
        1,
        "the upsert converges instead of duplicating"
    );
    // The duplicate delivery is answered Duplicate and the durable claim
    // count stays one (no second webhook row).
    let duplicate = daemon.deliver(&headers, body).unwrap();
    assert!(matches!(
        duplicate,
        faktor_scm::IngestOutcome::Duplicate { .. }
    ));
    assert_eq!(store.webhook_deliveries(10).unwrap().len(), 1);
    run.abort();
}
