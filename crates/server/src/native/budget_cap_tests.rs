// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! Adversarial budget-entry tests over the real native listener.
//!
//! Every test drives an HTTP entry point that accepts a cost/token cap and
//! tries to weaken it: zero (which the store reads as unlimited), negative,
//! fractional and over-`i64::MAX` amounts, and a positive cap too small to
//! fund every comparison candidate at >= 1 micro. The oracle is the reviewed
//! wire contract (typed 400/409 refusals; a 1-micro cap stays 1), never a
//! value read back from the SUT.

use reqwest::StatusCode;
use serde_json::{json, Value};

use super::foundation_r8::foundation_r8_tests::Rig;

fn money(body: &Value, key: &str) -> Option<String> {
    body[key].as_str().map(str::to_string)
}

fn seed_task(rig: &Rig) {
    let handle = rig
        .deps
        .session
        .get_session(faktor_core::id::SessionId::new(rig.session))
        .unwrap()
        .unwrap();
    crate::api::tests::seed_typed_task(&handle, 1, None, None, "cap entry");
}

#[tokio::test]
async fn session_budget_entry_refuses_ambiguous_zero_negative_fractional_and_over_max() {
    let rig = Rig::new().await;
    seed_task(&rig);
    let path = format!("/native/session/{}/budget", rig.session);
    for bad in ["0", "-1", "1.5", "abc", "9223372036854775808"] {
        let (status, body) = rig
            .put(&path, json!({"taskId": "1", "maxCostMicro": bad}))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
        assert_eq!(body["error"]["code"], "malformed", "{bad}: {body}");
    }
    // A real 1-micro cap is accepted and read back exactly (never widened by
    // a clamp); i64::MAX is the legal maximum; clear removes it explicitly.
    let (status, body) = rig
        .put(&path, json!({"taskId": "1", "maxCostMicro": "1"}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["acknowledged"], true);
    assert_eq!(money(&body["budget"], "maxCostMicro").as_deref(), Some("1"));
    let (status, body) = rig
        .put(
            &path,
            json!({"taskId": "1", "maxCostMicro": "9223372036854775807"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        money(&body["budget"], "maxCostMicro").as_deref(),
        Some("9223372036854775807")
    );
    let (status, body) = rig.put(&path, json!({"taskId": "1", "clear": true})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["budget"]["maxCostMicro"].is_null(), "{body}");
    // Both fields at once is ambiguous, never a silent pick.
    let (status, _) = rig
        .put(
            &path,
            json!({"taskId": "1", "maxCostMicro": "1", "clear": true}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn model_comparison_refuses_a_budget_too_small_to_fund_each_candidate() {
    let rig = Rig::new().await;
    let candidates = vec![
        json!({"provider": "fake", "model": "m"}),
        json!({"provider": "fake", "model": "m"}),
    ];
    // 1 micro / 2 candidates used to truncate to 0 per candidate — which the
    // durable store reads as UNLIMITED. It is a typed 400 before any run.
    for field in ["maxCostMicro", "maxTokens"] {
        let (status, body) = rig
            .post(
                "/native/models/compare",
                json!({
                    "sessionId": rig.session.to_string(),
                    "prompt": "compare",
                    "candidates": candidates.clone(),
                    field: 1,
                }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{field}: {body}");
        assert_eq!(body["error"]["code"], "malformed", "{field}: {body}");
    }
    // The smallest legal split (2 micro / 2 candidates) gives each candidate
    // exactly 1 micro (a decimal string on the wire), never 0/unlimited.
    let (status, body) = rig
        .post(
            "/native/models/compare",
            json!({
                "sessionId": rig.session.to_string(),
                "prompt": "compare",
                "candidates": candidates.clone(),
                "maxCostMicro": "2",
                "maxTokens": 2,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["budget"]["perCandidateCostMicro"].as_str(),
        Some("1"),
        "{body}"
    );
    assert_eq!(body["budget"]["perCandidateTokens"], 1, "{body}");
    for candidate in body["candidates"].as_array().unwrap() {
        assert_eq!(candidate["maxCostMicro"].as_str(), Some("1"), "{candidate}");
        assert_eq!(candidate["maxTokens"], 1, "{candidate}");
    }
    // Malformed/fractional/negative money shapes are the same typed 400 as
    // every other money entry point (this route previously took raw numbers
    // only).
    for bad in [json!("1.5"), json!(-1), json!("abc")] {
        let (status, _) = rig
            .post(
                "/native/models/compare",
                json!({
                    "sessionId": rig.session.to_string(),
                    "prompt": "compare",
                    "candidates": candidates.clone(),
                    "maxCostMicro": bad,
                }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
    }
}
