//! Model catalog / capability and provider registry projections.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use faktor_core::model::ModelCapabilities;

use super::*;
use crate::api::AppState;

/// Provenance of one native catalog entry (docs/native-protocol.md):
/// `liveProbe` when the provider reports a LIVE runtime context limit
/// for the model (e.g. an Ollama `/api/ps` allocation); otherwise
/// `providerCatalog` for entries carrying a non-default capability
/// profile (configured or probed), and `conservativeDefault` for entries
/// still at the fail-safe default profile (unprobed).
pub(crate) fn catalog_source(
    p: &dyn faktor_provider::Provider,
    model: &str,
    caps: &ModelCapabilities,
) -> &'static str {
    if p.runtime_context_limit(model).is_some() {
        "liveProbe"
    } else if *caps == ModelCapabilities::default() {
        "conservativeDefault"
    } else {
        "providerCatalog"
    }
}

/// The advertised attachment admission contract of ONE provider/model: the
/// daemon's HTTP upload/request ceilings plus the provider's own image and
/// document delivery bounds, assembled by the ONE Rust source of truth
/// (`faktor_provider::AttachmentLimits::for_model`). Every model-catalog
/// surface emits this exact value — clients consume it instead of mirroring
/// daemon constants.
pub(crate) fn attachment_limits_for(
    p: &dyn faktor_provider::Provider,
    model: &str,
) -> faktor_provider::AttachmentLimits {
    faktor_provider::AttachmentLimits::for_model(
        MAX_ATTACHMENT_UPLOAD_BYTES as u64,
        crate::api::MAX_BODY_BYTES as u64,
        p.max_image_bytes(),
        p.document_capable(model),
        p.max_document_bytes(),
    )
}

/// `GET /models` — the flat native model catalog: every registered
/// provider instance × its known models × capabilities and the advertised
/// attachment admission contract (docs/native-protocol.md).
pub(crate) async fn native_models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let mut out = Vec::new();
    for p in state.deps.agent.deps().providers.all() {
        // Registry key (instance id) — the same id session rows store.
        let instance = p.identity().instance_id.clone();
        for model in p.known_models() {
            let caps = p.capabilities(&model);
            let source = catalog_source(p.as_ref(), &model, &caps);
            out.push(serde_json::json!({
                "provider": instance,
                "model": model,
                "context": caps.context,
                "maxOutput": caps.max_output,
                "tools": caps.tools,
                "parallelTools": caps.parallel_tools,
                "reasoning": caps.reasoning,
                "thinking": caps.thinking,
                "vision": caps.vision,
                "structuredOutput": caps.json_schema,
                "embeddings": caps.embeddings,
                "streaming": caps.streaming,
                "source": source,
                "documentCapable": p.document_capable(&model),
                "attachmentLimits": attachment_limits_for(p.as_ref(), &model),
            }));
        }
    }
    out.sort_by(|a, b| {
        (a["provider"].as_str(), a["model"].as_str())
            .cmp(&(b["provider"].as_str(), b["model"].as_str()))
    });
    Json(out).into_response()
}

/// `GET /capabilities` — native introspection map:
/// `{ "<provider>": { models: [{id, capabilities}],
/// runtimeContextLimitSupported } }` (docs/native-protocol.md).
pub(crate) async fn native_capabilities(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let mut sorted = Vec::new();
    for p in state.deps.agent.deps().providers.all() {
        let instance = p.identity().instance_id.clone();
        let mut models = Vec::new();
        let mut live = false;
        for m in p.known_models() {
            if p.runtime_context_limit(&m).is_some() {
                live = true;
            }
            models.push(serde_json::json!({
                "id": m,
                "capabilities": p.capabilities(&m),
                "documentCapable": p.document_capable(&m),
                "attachmentLimits": attachment_limits_for(p.as_ref(), &m),
            }));
        }
        models.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        sorted.push((
            instance,
            serde_json::json!({
                "models": models,
                "runtimeContextLimitSupported": live,
            }),
        ));
    }
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut map = serde_json::Map::new();
    for (instance, entry) in sorted {
        map.insert(instance, entry);
    }
    Json(serde_json::Value::Object(map)).into_response()
}

// ------------------------------------------------- native v1: audits 55-56
// Liveness/readiness and the durable session listings (all auth-gated like
// every daemon route). Response bodies are native JSON over durable state:
// session rows, turn records, the task ledger, checkpoint rows, memory
// facts and live PTYs — never foreign wire shapes. Hostile ids are 400
// (unparseable/0) or 404 (unknown); handlers never panic on them.

/// `GET /native/providers` — the registry view of every registered provider
/// (audit P0-64): instance/family identity, the models it can serve with
/// their capability profiles, the capability source, and a live health
/// snapshot. The daemon exposes no per-provider rate-limit/cooldown state
/// to the server layer (adapters track those privately), so `health` is the
/// honest static snapshot: registration + live runtime-context probe
/// support + configured context limit. Never emits secrets (auth/endpoint
/// metadata stays in the provider layer).
pub(crate) async fn native_providers(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let mut out: Vec<serde_json::Value> = Vec::new();
    for p in state.deps.agent.deps().providers.all() {
        let identity = p.identity();
        let mut models: Vec<serde_json::Value> = Vec::new();
        for model in p.known_models() {
            let caps = p.capabilities(&model);
            models.push(serde_json::json!({
                "model": model,
                "context": caps.context,
                "maxOutput": caps.max_output,
                "tools": caps.tools,
                "parallelTools": caps.parallel_tools,
                "reasoning": caps.reasoning,
                "thinking": caps.thinking,
                "vision": caps.vision,
                "structuredOutput": caps.json_schema,
                "embeddings": caps.embeddings,
                "streaming": caps.streaming,
                "source": catalog_source(p.as_ref(), &model, &caps),
            }));
        }
        models.sort_by(|a, b| a["model"].as_str().cmp(&b["model"].as_str()));
        out.push(serde_json::json!({
            "instanceId": identity.instance_id,
            "family": identity.family,
            "models": models,
            "runtimeContextLimitSupported": p
                .known_models()
                .iter()
                .any(|m| p.runtime_context_limit(m).is_some()),
            "health": {
                "status": "registered",
                "note": "rate-limit/cooldown state is adapter-private and not exposed to the server; this snapshot is the registry view",
            },
        }));
    }
    out.sort_by(|a, b| a["instanceId"].as_str().cmp(&b["instanceId"].as_str()));
    Json(out).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> serde_json::Value {
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/attachment-limits.json"
        ))
        .expect("fixtures/attachment-limits.json is the shared daemon/IDE limit contract");
        serde_json::from_str(&raw).expect("fixture is valid JSON")
    }

    #[test]
    fn attachment_limits_fixture_pins_the_daemon_constants() {
        let fixture = fixture();
        // The canonical block is exactly what the default provider profile
        // (document-capable, daemon-wide bounds) advertises: the fixture is
        // the shared cross-language contract, never a second truth.
        let canonical = attachment_limits_for(
            &faktor_provider::FakeProvider::new("fixture", ModelCapabilities::default())
                .with_documents(),
            "default",
        );
        assert_eq!(
            serde_json::to_value(&canonical).unwrap(),
            fixture["canonical"],
            "fixture canonical block drifted from the Rust source of truth"
        );
        // The emergency ceilings are byte-identical to the daemon constants
        // the IDEs fall back to before the catalog is read, and they are
        // CONSERVATIVE: never above the advertised per-MIME bounds.
        let emergency = &fixture["emergencyCeiling"];
        assert_eq!(
            emergency["maxUploadBytes"].as_u64().unwrap(),
            MAX_ATTACHMENT_UPLOAD_BYTES as u64
        );
        assert_eq!(
            emergency["maxRequestBytes"].as_u64().unwrap(),
            crate::api::MAX_BODY_BYTES as u64
        );
        assert_eq!(
            emergency["maxImageBytes"].as_u64().unwrap(),
            faktor_provider::MAX_MODEL_IMAGE_BYTES as u64
        );
        assert_eq!(
            emergency["maxDocumentBytes"].as_u64().unwrap(),
            faktor_provider::MAX_MODEL_DOCUMENT_BYTES as u64
        );
        assert_eq!(
            emergency["imageMimes"],
            serde_json::json!(faktor_provider::SUPPORTED_IMAGE_MIMES)
        );
        assert_eq!(
            emergency["documentMimes"],
            serde_json::json!(faktor_provider::SUPPORTED_DOCUMENT_MIMES)
        );
        for mime in fixture["canonical"]["image"]["mimes"].as_array().unwrap() {
            assert!(
                mime["maxBytes"].as_u64().unwrap() >= emergency["maxImageBytes"].as_u64().unwrap(),
                "the emergency image ceiling can never exceed the advertised per-MIME bound"
            );
        }
        for mime in fixture["canonical"]["document"]["mimes"]
            .as_array()
            .unwrap()
        {
            assert!(
                mime["maxBytes"].as_u64().unwrap()
                    >= emergency["maxDocumentBytes"].as_u64().unwrap(),
                "the emergency document ceiling can never exceed the advertised per-MIME bound"
            );
        }
    }

    #[test]
    fn attachment_limits_are_per_model_and_track_provider_bounds() {
        let plain = attachment_limits_for(
            &faktor_provider::FakeProvider::new("p", ModelCapabilities::default()),
            "m",
        );
        assert!(
            !plain.document.capable,
            "a provider that does not advertise documents must say so"
        );
        assert_eq!(
            plain.image.mimes.len(),
            faktor_provider::SUPPORTED_IMAGE_MIMES.len()
        );
        assert_eq!(
            plain.image.mimes[0].max_bytes,
            faktor_provider::MAX_MODEL_IMAGE_BYTES as u64
        );
        assert_eq!(
            plain.image.max_request_bytes,
            faktor_provider::MAX_REQUEST_IMAGE_BYTES as u64
        );
        assert_eq!(plain.document.mimes[0].mime, "application/pdf");
        assert_eq!(
            plain.document.max_request_bytes,
            faktor_provider::MAX_REQUEST_DOCUMENT_BYTES as u64
        );
        assert_eq!(plain.max_upload_bytes, MAX_ATTACHMENT_UPLOAD_BYTES as u64);
        assert_eq!(plain.max_request_bytes, crate::api::MAX_BODY_BYTES as u64);
        assert_eq!(
            plain.max_attachment_bytes,
            faktor_core::attachment::MAX_ATTACHMENT_BYTES
        );
    }
}
