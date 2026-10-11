//! `egress::budget_tests`: out-of-line response-budget tests, moved out of
//! `egress.rs` to keep the checked transport under its recorded source-size
//! ceiling.

use super::*;
use futures::StreamExt;

type BodyStream = futures::stream::BoxStream<'static, Result<Vec<u8>, std::io::Error>>;

/// A body stream that never yields a chunk (the stalled-head shape).
fn pending_body() -> BodyStream {
    Box::pin(futures::stream::pending())
}

/// A body that yields the scripted `(delay_ms, bytes)` chunks and then
/// stays silent forever (the stalled-idle / dripped shapes).
fn chunks_then_pending(chunks: Vec<(u64, Vec<u8>)>) -> BodyStream {
    let scripted = futures::stream::iter(chunks).then(|(delay_ms, bytes)| async move {
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
        Ok::<Vec<u8>, std::io::Error>(bytes)
    });
    Box::pin(scripted.chain(futures::stream::pending()))
}

/// A body that yields the scripted chunks and then ends cleanly.
fn chunks_then_end(chunks: Vec<Vec<u8>>) -> BodyStream {
    Box::pin(futures::stream::iter(chunks.into_iter().map(Ok)))
}

/// A transport serving ONE scripted body stream (no network).
struct ScriptedBodyTransport {
    body: std::sync::Mutex<Option<BodyStream>>,
}

impl ScriptedBodyTransport {
    fn new(body: BodyStream) -> Self {
        Self {
            body: std::sync::Mutex::new(Some(body)),
        }
    }
}

impl HttpTransport for ScriptedBodyTransport {
    fn execute(&self, req: Request) -> BoxFuture<'_, Result<CheckedResponse, EgressError>> {
        let body = self
            .body
            .lock()
            .unwrap()
            .take()
            .expect("one scripted body per test");
        let url = req.url().clone();
        Box::pin(async move {
            let response = http::Response::builder()
                .status(200u16)
                .url(url)
                .body(Body::wrap_stream(body))
                .map_err(|e| EgressError::Build(e.to_string()))?;
            Ok(CheckedResponse::from_response(Response::from(response)))
        })
    }
}

fn request() -> RawRequest {
    RawRequest::new("GET", "https://allowed.example/data?token=PLANTED_SECRET")
        .route(RouteLabel::ProviderStream)
}

/// Run one scripted body through `execute_raw` under `budget`, with an
/// outer test guard so a broken bound surfaces as a test timeout.
async fn run(body: BodyStream, budget: ResponseBudget) -> Result<RawResponse, EgressError> {
    let transport = ScriptedBodyTransport::new(body);
    tokio::time::timeout(
        Duration::from_secs(10),
        execute_raw(&transport, request(), &budget),
    )
    .await
    .expect("the budget must fire without the test timeout")
}

#[tokio::test]
async fn head_component_fires_typed_on_a_silent_body() {
    let budget = ResponseBudget::from_millis(60, 60, 5_000, 1024, None);
    let err = run(pending_body(), budget).await.unwrap_err();
    match &err {
        EgressError::ResponseBudgetExceeded {
            component: BudgetComponent::Head,
            limit: 60,
            url,
        } => {
            // The diagnostic carries only the credential-free shape.
            assert_eq!(url.host(), "allowed.example");
            assert!(url.query_present());
        }
        other => panic!("expected a typed Head budget error, got {other:?}"),
    }
    // The planted query secret never renders.
    assert!(!err.to_string().contains("PLANTED_SECRET"), "{err}");
    assert!(!format!("{err:?}").contains("PLANTED_SECRET"), "{err:?}");
}

#[tokio::test]
async fn idle_component_fires_typed_after_a_partial_body() {
    let body = chunks_then_pending(vec![(0, b"first".to_vec())]);
    let budget = ResponseBudget::from_millis(2_000, 60, 5_000, 1024, None);
    let err = run(body, budget).await.unwrap_err();
    assert!(
        matches!(
            &err,
            EgressError::ResponseBudgetExceeded {
                component: BudgetComponent::Idle,
                limit: 60,
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn total_component_fires_typed_on_a_slow_drip() {
    // A chunk every 30 ms is inside the idle window, so only the overall
    // deadline can stop the drip.
    let chunks: Vec<(u64, Vec<u8>)> = (0..40).map(|_| (30, vec![b'x'])).collect();
    let budget = ResponseBudget::from_millis(5_000, 5_000, 150, 100_000, None);
    let err = run(chunks_then_pending(chunks), budget).await.unwrap_err();
    assert!(
        matches!(
            &err,
            EgressError::ResponseBudgetExceeded {
                component: BudgetComponent::Total,
                limit: 150,
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn bytes_component_fires_typed_before_the_crossing_chunk_is_handed_out() {
    let body = chunks_then_end(vec![vec![7u8; 64], vec![7u8; 64]]);
    let budget = ResponseBudget::from_millis(2_000, 2_000, 5_000, 100, None);
    let err = run(body, budget).await.unwrap_err();
    assert!(
        matches!(
            &err,
            EgressError::ResponseBudgetExceeded {
                component: BudgetComponent::Bytes,
                limit: 100,
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn frames_component_fires_typed_on_the_over_cap_frame() {
    let body = chunks_then_end(vec![vec![1], vec![2], vec![3]]);
    let budget = ResponseBudget::from_millis(2_000, 2_000, 5_000, 1024, Some(2));
    let err = run(body, budget).await.unwrap_err();
    assert!(
        matches!(
            &err,
            EgressError::ResponseBudgetExceeded {
                component: BudgetComponent::Frames,
                limit: 2,
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_body_inside_its_budget_materializes() {
    let body = chunks_then_end(vec![b"ab".to_vec(), b"cd".to_vec()]);
    let budget = ResponseBudget::from_millis(2_000, 2_000, 5_000, 64, Some(4));
    let response = run(body, budget).await.unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"abcd");
}

#[tokio::test]
async fn budgeted_stream_emits_exactly_one_typed_error_then_ends() {
    let transport = ScriptedBodyTransport::new(pending_body());
    let url = Url::parse("https://allowed.example/stream").unwrap();
    let response = transport
        .execute(Request::new(Method::GET, url))
        .await
        .unwrap();
    let budget = ResponseBudget::from_millis(50, 50, 5_000, 1024, None);
    let mut stream = Box::pin(response.into_budgeted(budget).into_stream());
    let first = stream.next().await.expect("one error item").unwrap_err();
    assert!(
        matches!(
            first,
            EgressError::ResponseBudgetExceeded {
                component: BudgetComponent::Head,
                ..
            }
        ),
        "{first:?}"
    );
    assert!(stream.next().await.is_none(), "stream ends after the error");
}

/// `read_json_bounded` parses a normal JSON body and types a malformed
/// one WITHOUT ever echoing the hostile bytes.
#[tokio::test]
async fn read_json_bounded_parses_normal_json_and_never_echoes_a_malformed_body() {
    #[derive(serde::Deserialize, PartialEq, Eq, Debug)]
    struct Row {
        models: Vec<String>,
    }
    let budget = ResponseBudget::from_millis(2_000, 2_000, 5_000, 4096, None);
    let transport = MockHttpTransport::new(200, r#"{"models":["qwen3.8"]}"#);
    let resp = execute_get(&transport, "https://allowed.example/api/tags")
        .await
        .unwrap();
    let row: Row = read_json_bounded(resp, &budget).await.unwrap();
    assert_eq!(row.models, vec!["qwen3.8"]);

    // The typed parse refusal never carries a body byte (a planted
    // secret in the malformed body must not render through any path).
    const SECRET: &str = "MALFORMED-BODY-SECRET-9f2a";
    let transport = MockHttpTransport::new(200, format!("not json {SECRET}"));
    let resp = execute_get(&transport, "https://allowed.example/api/tags")
        .await
        .unwrap();
    let err = read_json_bounded::<serde_json::Value>(resp, &budget)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, EgressError::MalformedResponse { .. }),
        "{err:?}"
    );
    for rendered in [err.to_string(), format!("{err:?}")] {
        assert!(!rendered.contains(SECRET), "body bytes leaked: {rendered}");
    }
}

/// A HOSTILE giant JSON body is cut at `max_bytes` BEFORE the crossing
/// chunk is handed out: bounded allocation by construction, typed error,
/// and the (unparseable) prefix is never parsed into a value.
#[tokio::test]
async fn read_json_bounded_cuts_a_giant_json_body_by_max_bytes() {
    let budget = ResponseBudget::from_millis(2_000, 2_000, 5_000, 128, None);
    let giant = format!(r#"{{"models":["{}"]}}"#, "x".repeat(64 * 1024));
    let transport = MockHttpTransport::new(200, giant);
    let resp = execute_get(&transport, "https://allowed.example/api/ps")
        .await
        .unwrap();
    let err = read_json_bounded::<serde_json::Value>(resp, &budget)
        .await
        .unwrap_err();
    assert!(
        matches!(
            &err,
            EgressError::ResponseBudgetExceeded {
                component: BudgetComponent::Bytes,
                limit: 128,
                ..
            }
        ),
        "{err:?}"
    );
    // The same body inside its budget parses: the cut is the budget, not
    // the content.
    let transport = MockHttpTransport::new(200, r#"{"models":[]}"#);
    let resp = execute_get(&transport, "https://allowed.example/api/ps")
        .await
        .unwrap();
    let value: serde_json::Value = read_json_bounded(resp, &budget).await.unwrap();
    assert_eq!(value["models"], serde_json::json!([]));
}

/// A STREAMING JSON body that drips a prefix and then goes silent is cut
/// by the idle window, typed, with no unbounded wait.
#[tokio::test]
async fn read_json_bounded_cuts_a_streaming_json_body_by_idle() {
    let body = chunks_then_pending(vec![(0, br#"{"models":[{"name":"q"#.to_vec())]);
    let transport = ScriptedBodyTransport::new(body);
    let resp = execute_get(&transport, "https://allowed.example/api/show")
        .await
        .unwrap();
    let budget = ResponseBudget::from_millis(2_000, 60, 5_000, 4096, None);
    let err = read_json_bounded::<serde_json::Value>(resp, &budget)
        .await
        .unwrap_err();
    assert!(
        matches!(
            &err,
            EgressError::ResponseBudgetExceeded {
                component: BudgetComponent::Idle,
                limit: 60,
                ..
            }
        ),
        "{err:?}"
    );
}

/// An unbound reader (the crate-internal unbound constructor only) is a
/// typed refusal, never an unbudgeted read.
#[tokio::test]
async fn unbound_body_read_is_a_typed_refusal() {
    let raw = Response::from(
        http::Response::builder()
            .status(200u16)
            .url(Url::parse("https://allowed.example/data").unwrap())
            .body(Body::from("payload"))
            .unwrap(),
    );
    let mut body = BudgetedBody::unbudgeted(raw);
    let err = body.next_chunk().await.unwrap_err();
    assert!(matches!(err, EgressError::UnboundResponseBody), "{err:?}");
    assert!(body.next_chunk().await.unwrap().is_none());
}

/// The fallible-construction contract (item 9): production constructors
/// return `Result`, a build failure is the typed `ClientBuild` (never a
/// panicking or fallback path), and the deleted `unwrap_or_else`
/// fallback cannot reappear unnoticed.
#[test]
fn checked_client_construction_is_fallible_without_a_fallback_path() {
    let client = CheckedHttpClient::try_with_policy(DestinationPolicy::empty())
        .expect("an empty policy still builds the one checked client");
    assert!(client.policy().is_some());
    let scanned = CheckedHttpClient::try_with_policy_and_scan(
        DestinationPolicy::empty(),
        Some(OutboundScanConfig::default()),
    )
    .expect("client build");
    assert!(scanned.outbound_scan().is_some());
    let transport = PolicyCheckedHttpTransport::try_with_policy_scan_and_addresses(
        DestinationPolicy::empty(),
        None,
        EgressAddressPolicy::EXTERNAL,
    )
    .expect("client build");
    assert_eq!(transport.address_policy(), EgressAddressPolicy::EXTERNAL);
    let rendered = EgressError::ClientBuild {
        detail: "tls backend unavailable".into(),
    }
    .to_string();
    assert_eq!(
        rendered,
        "egress client construction failed: tls backend unavailable"
    );
    // Source certification of the deleted fallback: the old fallback
    // client constructor must not come back. Markers are assembled at
    // runtime so this test's own text cannot satisfy them.
    let own = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/egress.rs"))
        .expect("egress.rs is readable");
    let fallback_fn = format!("fn redirect_disabled{}", "_client");
    assert!(
        !own.contains(&fallback_fn),
        "the fallback client constructor returned; construction must stay fallible"
    );
    let fallback_call = format!("unwrap_or_else(|_| default_timeout{}", "_client");
    assert!(
        !own.contains(&fallback_call),
        "the unwrap_or_else client fallback returned; start-up must fail typed"
    );
}
