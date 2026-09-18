//! Native SCM webhook route tests: disabled parity (typed 409), signature
//! authentication without the daemon password, exactly-once delivery
//! claiming (duplicates drop), and the oversized/malformed refusal matrix.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use faktor_scm::{
    hmac_sha256_hex, IngestOutcome, MemoryScmStore, ScmStore, WebhookError, WebhookHeaders,
    WebhookInbox, WebhookVerifier,
};

use super::WebhookSink;
use crate::api::{serve, tests::test_deps};

const SECRET: &[u8] = b"hook-secret";
const NOW_MS: i64 = 1_700_000_000_000;

/// The counting test sink over a real durable inbox.
struct CountingSink {
    inbox: WebhookInbox,
    deliveries: AtomicUsize,
}

impl CountingSink {
    fn new(store: Arc<dyn ScmStore>) -> Self {
        Self {
            inbox: WebhookInbox::new(WebhookVerifier::new(SECRET.to_vec()).unwrap(), store),
            deliveries: AtomicUsize::new(0),
        }
    }
}

impl WebhookSink for CountingSink {
    fn deliver(
        &self,
        headers: &WebhookHeaders,
        body: &[u8],
    ) -> Result<IngestOutcome, WebhookError> {
        self.deliveries.fetch_add(1, Ordering::SeqCst);
        self.inbox.ingest(headers, body, NOW_MS)
    }
}

/// The daemon's payload discipline in miniature: authenticate, parse
/// `installation.id`, only then claim. A malformed payload is refused before
/// the durable claim, exactly like `ScmDaemon::deliver`.
struct ParsingSink {
    inbox: WebhookInbox,
}

impl WebhookSink for ParsingSink {
    fn deliver(
        &self,
        headers: &WebhookHeaders,
        body: &[u8],
    ) -> Result<IngestOutcome, WebhookError> {
        self.inbox.verify(headers, body, NOW_MS)?;
        faktor_scm::installation_of(body)?;
        self.inbox.ingest(headers, body, NOW_MS)
    }
}

/// A sink failing with one chosen typed refusal (store vs payload).
struct RefusingSink(WebhookError);

impl WebhookSink for RefusingSink {
    fn deliver(
        &self,
        _headers: &WebhookHeaders,
        _body: &[u8],
    ) -> Result<IngestOutcome, WebhookError> {
        Err(self.0.clone())
    }
}

#[tokio::test]
async fn no_wired_sink_answers_typed_409_without_the_daemon_password() {
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    assert!(deps.scm_webhook.is_none());
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let resp = client
        .post(format!("{base}/native/scm/webhook"))
        .header("x-github-delivery", "d-1")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "scm_webhook_disabled");
    // Disabled parity: the control-plane surface still demands the daemon
    // password; the webhook route did not weaken anything else.
    let scm = client
        .get(format!("{base}/native/repositories"))
        .send()
        .await
        .unwrap();
    assert_eq!(scm.status(), 401);
}

#[tokio::test]
async fn signed_delivery_is_claimed_exactly_once_and_duplicates_drop() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn ScmStore> = Arc::new(MemoryScmStore::new());
    let sink = Arc::new(CountingSink::new(store.clone()));
    let deps = test_deps(dir.path()).with_scm_webhook(sink.clone());
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let payload = br#"{"installation":{"id":7},"action":"created"}"#;

    let first = client
        .post(format!("{base}/native/scm/webhook"))
        .header("x-github-delivery", "delivery-1")
        .header("x-github-event", "installation")
        .header(
            "x-hub-signature-256",
            format!("sha256={}", hmac_sha256_hex(SECRET, payload)),
        )
        .body(payload.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    let first: serde_json::Value = first.json().await.unwrap();
    assert_eq!(first["status"], "accepted");
    assert_eq!(first["deliveryId"], "delivery-1");

    let second = client
        .post(format!("{base}/native/scm/webhook"))
        .header("x-github-delivery", "delivery-1")
        .header("x-github-event", "installation")
        .header(
            "x-hub-signature-256",
            format!("sha256={}", hmac_sha256_hex(SECRET, payload)),
        )
        .body(payload.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 200);
    let second: serde_json::Value = second.json().await.unwrap();
    assert_eq!(second["status"], "duplicate");
    assert_eq!(
        store.webhook_deliveries(10).unwrap().len(),
        1,
        "exactly one durable delivery claim"
    );
    // Both requests reached the wired sink; the DURABLE inbox decided the
    // second one is a duplicate (never re-applied).
    assert_eq!(sink.deliveries.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn bad_signature_is_401_and_never_claims_the_delivery() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn ScmStore> = Arc::new(MemoryScmStore::new());
    let deps = test_deps(dir.path()).with_scm_webhook(Arc::new(CountingSink::new(store.clone())));
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let payload = b"{}";

    let forged = client
        .post(format!("{base}/native/scm/webhook"))
        .header("x-github-delivery", "delivery-2")
        .header("x-hub-signature-256", format!("sha256={}", "00".repeat(32)))
        .body(payload.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status(), 401);
    let forged: serde_json::Value = forged.json().await.unwrap();
    assert_eq!(forged["error"]["code"], "scm_webhook_unauthorized");
    assert!(store.webhook_deliveries(10).unwrap().is_empty());

    // Missing signature is refused too (still no claim).
    let unsigned = client
        .post(format!("{base}/native/scm/webhook"))
        .header("x-github-delivery", "delivery-3")
        .body(payload.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(unsigned.status(), 401);
    assert!(store.webhook_deliveries(10).unwrap().is_empty());

    // A correctly signed redelivery of the SAME id is then accepted: the
    // forged attempt never consumed the delivery identity.
    let valid = client
        .post(format!("{base}/native/scm/webhook"))
        .header("x-github-delivery", "delivery-2")
        .header(
            "x-hub-signature-256",
            format!("sha256={}", hmac_sha256_hex(SECRET, payload)),
        )
        .body(payload.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(valid.status(), 200);
    let valid: serde_json::Value = valid.json().await.unwrap();
    assert_eq!(valid["status"], "accepted");
}

#[tokio::test]
async fn missing_delivery_id_is_400_and_oversized_body_is_413() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn ScmStore> = Arc::new(MemoryScmStore::new());
    let deps = test_deps(dir.path()).with_scm_webhook(Arc::new(CountingSink::new(store.clone())));
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);

    let no_id = client
        .post(format!("{base}/native/scm/webhook"))
        .header(
            "x-hub-signature-256",
            format!("sha256={}", hmac_sha256_hex(SECRET, b"{}")),
        )
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(no_id.status(), 400);

    // One byte over the inbox's hard body bound: refused BEFORE any claim.
    let oversized = vec![b'x'; faktor_scm::MAX_WEBHOOK_BODY_BYTES + 1];
    let resp = client
        .post(format!("{base}/native/scm/webhook"))
        .header("x-github-delivery", "delivery-4")
        .header(
            "x-hub-signature-256",
            format!("sha256={}", hmac_sha256_hex(SECRET, &oversized)),
        )
        .body(oversized)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413);
    let resp: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(resp["error"]["code"], "payload_too_large");
    assert!(store.webhook_deliveries(10).unwrap().is_empty());
}

#[test]
fn webhook_error_classification_is_frozen_and_retryability_is_typed() {
    let cases: &[(WebhookError, &str, u16, bool)] = &[
        (WebhookError::BodyTooLarge, "payload_too_large", 413, false),
        (
            WebhookError::MissingSignature,
            "scm_webhook_unauthorized",
            401,
            false,
        ),
        (
            WebhookError::MalformedSignature,
            "scm_webhook_unauthorized",
            401,
            false,
        ),
        (
            WebhookError::SignatureMismatch,
            "scm_webhook_unauthorized",
            401,
            false,
        ),
        (
            WebhookError::StaleTimestamp,
            "scm_webhook_unauthorized",
            401,
            false,
        ),
        (WebhookError::MissingDeliveryId, "malformed", 400, false),
        (
            WebhookError::MalformedPayload("installation.id refused".into()),
            "malformed",
            400,
            false,
        ),
        (
            WebhookError::Store("db down".into()),
            "scm_webhook_unavailable",
            503,
            true,
        ),
    ];
    for (e, code, status, retryable) in cases {
        // The typed error is the single retryability authority.
        assert_eq!(e.retryable(), *retryable, "{e:?}");
        let api = super::webhook_err(e.clone());
        assert_eq!(api.code, *code, "{e:?}");
        assert_eq!(api.http_status, *status, "{e:?}");
        assert_eq!(api.retryable, *retryable, "{e:?}");
        assert_eq!(api.message, e.to_string(), "{e:?}");
    }
}

#[tokio::test]
async fn malformed_installation_id_is_400_not_retryable_and_never_claimed() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn ScmStore> = Arc::new(MemoryScmStore::new());
    let sink = Arc::new(ParsingSink {
        inbox: WebhookInbox::new(
            WebhookVerifier::new(SECRET.to_vec()).unwrap(),
            store.clone(),
        ),
    });
    let deps = test_deps(dir.path()).with_scm_webhook(sink);
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);

    let body = br#"{"installation":{"id":0},"action":"created"}"#;
    let resp = client
        .post(format!("{base}/native/scm/webhook"))
        .header("x-github-delivery", "delivery-malformed")
        .header("x-github-event", "installation")
        .header(
            "x-hub-signature-256",
            format!("sha256={}", hmac_sha256_hex(SECRET, body)),
        )
        .body(body.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let err: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(err["error"]["code"], "malformed");
    assert_eq!(err["error"]["retryable"], false);
    assert!(err["error"]["message"]
        .as_str()
        .unwrap()
        .contains("installation.id"));
    assert!(
        store.webhook_deliveries(10).unwrap().is_empty(),
        "a malformed payload must be refused before any durable claim"
    );

    // A valid id over the same sink is unchanged: 200 accepted and claimed.
    let valid = br#"{"installation":{"id":7},"action":"created"}"#;
    let resp = client
        .post(format!("{base}/native/scm/webhook"))
        .header("x-github-delivery", "delivery-valid")
        .header("x-github-event", "installation")
        .header(
            "x-hub-signature-256",
            format!("sha256={}", hmac_sha256_hex(SECRET, valid)),
        )
        .body(valid.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let ok: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(ok["status"], "accepted");
    assert_eq!(store.webhook_deliveries(10).unwrap().len(), 1);
}

#[tokio::test]
async fn store_failure_is_still_a_retryable_503() {
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path()).with_scm_webhook(Arc::new(RefusingSink(WebhookError::Store(
        "sqlite is busy".into(),
    ))));
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let resp = client
        .post(format!("{base}/native/scm/webhook"))
        .header("x-github-delivery", "delivery-store")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
    let err: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(err["error"]["code"], "scm_webhook_unavailable");
    assert_eq!(err["error"]["retryable"], true);
}
