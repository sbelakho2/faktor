//! `provider` inline `tests` module, split out of `lib.rs` by the
//! source-size ceiling (body moved verbatim; `use super::*` keeps every
//! parent fixture in scope).
// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir

use super::*;
use faktor_core::cancellation::CancellationToken;
use faktor_core::error::ErrorKind;
use faktor_core::id::{OpId, SessionId};

fn req() -> GenericAgentRequest {
    GenericAgentRequest {
        model: "m".into(),
        system: "s".into(),
        messages: vec![],
        tools: vec![],
        max_output: None,
        reasoning: None,
        stream: true,
        meta: RequestMeta {
            operation_id: OpId::new(1),
            session_id: SessionId::new(1),
            provider: "fake".into(),
            attempt: 0,
            deadline_ms: 1000,
            cancellation: CancellationToken::new(),
        },
    }
}

#[test]
fn canonical_usage_fields_round_trip_with_defaults() {
    // Audit Phase-1 item C: a usage frame is the canonical category
    // split — uncached input, cache lines and output. Older additive
    // shapes are gone: a legacy `tokens_in` envelope is NOT a usage
    // frame (it carries no canonical categories).
    let missing = serde_json::json!({"type": "usage"});
    let usage: ProviderChunk = serde_json::from_value(missing).unwrap();
    match usage {
        ProviderChunk::Usage(usage) => {
            assert!(usage.is_zero());
            assert_eq!(usage.reported_cost, None);
            assert_eq!(usage.request_id, None);
        }
        other => panic!("usage frame mis-parsed: {other:?}"),
    }
    // And the canonical fields round-trip.
    let rich = ProviderChunk::Usage(CanonicalUsage {
        uncached_input_tokens: 10,
        cache_read_tokens: 7,
        cache_write_tokens: 2,
        output_tokens: 5,
        reasoning_tokens: 3,
        reported_cost: Some(ReportedCost {
            micro_usd: 42,
            currency: ReportedCurrency::Usd,
            source: ReportedCostSource::ProviderUsage,
            request_id: Some("req_1".into()),
        }),
        request_id: Some("req_1".into()),
    });
    let back: ProviderChunk = serde_json::from_value(serde_json::to_value(&rich).unwrap()).unwrap();
    assert_eq!(back, rich);
}

#[test]
fn total_including_cache_splits_and_refuses_hostile_rows() {
    // Wire total INCLUDING cached input: 1000 total / 600 cached ->
    // uncached 400 + cache_read 600, output untouched.
    let u = CanonicalUsage::from_total_including_cache(1000, 600, 0, 50, 0).unwrap();
    assert_eq!(
        u,
        CanonicalUsage {
            uncached_input_tokens: 400,
            cache_read_tokens: 600,
            ..CanonicalUsage::new(0, 0, 0, 50)
        }
    );
    // A full cache hit is legal (uncached 0)...
    let hit = CanonicalUsage::from_total_including_cache(600, 600, 0, 50, 0).unwrap();
    assert_eq!(hit.uncached_input_tokens, 0);
    // ...but cache > total is hostile and typed, never saturated.
    assert_eq!(
        CanonicalUsage::from_total_including_cache(100, 600, 0, 50, 0).unwrap_err(),
        UsageSplitError::CacheReadsExceedInput {
            total_input_tokens: 100,
            cache_read_tokens: 600,
        }
    );
    // Reasoning must be an informational subset of output.
    assert_eq!(
        CanonicalUsage::from_total_including_cache(1000, 0, 0, 20, 30).unwrap_err(),
        UsageSplitError::ReasoningExceedsOutput {
            output_tokens: 20,
            reasoning_tokens: 30,
        }
    );
    // Cache writes must fit the remainder after reads.
    assert_eq!(
        CanonicalUsage::from_total_including_cache(500, 400, 200, 50, 0).unwrap_err(),
        UsageSplitError::CacheWritesExceedInput {
            total_input_tokens: 500,
            cache_write_tokens: 200,
        }
    );
    // A wire that reports reasoning must have folded it into output.
    let split = CanonicalUsage {
        output_tokens: 0,
        reasoning_tokens: 3,
        ..CanonicalUsage::ZERO
    };
    assert_eq!(
        split.validate().unwrap_err(),
        UsageSplitError::ReasoningExceedsOutput {
            output_tokens: 0,
            reasoning_tokens: 3,
        }
    );
}

#[test]
fn capability_validation_rejects_tools_on_tool_less_model() {
    let caps = ModelCapabilities {
        tools: false,
        ..Default::default()
    };
    let mut r = req();
    r.tools.push(ToolSpec {
        name: "read_file".into(),
        description: "d".into(),
        input_schema: serde_json::json!({}),
        effects: ToolEffects::CONSERVATIVE,
    });
    let err = CapabilityValidator::validate(&r, &caps).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Malformed);
}

#[test]
fn capability_validation_rejects_reasoning_and_oversized_output() {
    let caps = ModelCapabilities {
        reasoning: false,
        thinking: false,
        max_output: 1000,
        ..Default::default()
    };
    let mut r = req();
    r.reasoning = Some(ReasoningMode::High);
    assert!(CapabilityValidator::validate(&r, &caps).is_err());
    r.reasoning = None;
    r.max_output = Some(2000);
    let err = CapabilityValidator::validate(&r, &caps).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Oversized);
    r.max_output = Some(1000);
    assert!(CapabilityValidator::validate(&r, &caps).is_ok());
}

/// Resolved media is vision-gated at capability validation (before any
/// adapter lowering) and is a typed refusal for a vision-less model.
#[test]
fn capability_validation_refuses_resolved_media_for_visionless_models() {
    let mut r = req();
    r.messages.push(RequestMessage {
        role: Role::User,
        content: vec![
            ContentPart::text("what is this?"),
            ContentPart::image_data("image/png", vec![0x89, b'P', b'N', b'G']).unwrap(),
        ],
    });
    let visionless = ModelCapabilities {
        vision: false,
        ..Default::default()
    };
    let err = CapabilityValidator::validate(&r, &visionless).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Malformed);
    assert!(err.message.contains("does not support vision"), "{err}");
    let vision = ModelCapabilities {
        vision: true,
        ..Default::default()
    };
    CapabilityValidator::validate(&r, &vision).expect("vision model admits media");
    // The legacy URL-shaped part is untouched by the media gate (it
    // carries no bytes and predates the attachment path).
    assert!(ContentPart::image_data("text/plain", vec![1]).is_err());
    assert!(ContentPart::image_data("IMAGE/PNG", vec![1]).is_err());
    assert!(ContentPart::image_data("image/png", Vec::new()).is_err());
}

#[test]
fn media_bytes_json_never_carries_expanded_bytes_and_refuses_decode() {
    let bytes = vec![0x89, b'P', b'N', b'G', 1, 2, 3, 4];
    let media = MediaBytes::new(bytes.clone()).unwrap();
    let json = serde_json::to_value(&media).unwrap();
    assert_eq!(json["size"], 8);
    assert_eq!(
        json["digest"],
        serde_json::json!(media.digest().to_hex()),
        "the JSON carrier is content-addressed"
    );
    let obj = json.as_object().unwrap();
    assert_eq!(obj.len(), 2, "only digest+size may cross the JSON boundary");
    assert!(
        obj.values().all(|v| !v.is_array()),
        "no expanded byte array may appear in JSON: {json}"
    );
    // Decoding bytes from JSON is impossible by construction: durable
    // state stores attachment ids and requests re-resolve from the CAS.
    assert!(serde_json::from_value::<MediaBytes>(json).is_err());
    // A whole message containing media serializes to the digest carrier
    // and can never be decoded back into expanded bytes.
    let message = RequestMessage {
        role: Role::User,
        content: vec![
            ContentPart::text("see"),
            ContentPart::image_data("image/png", media.as_slice().to_vec()).unwrap(),
        ],
    };
    let msg_json = serde_json::to_string(&message).unwrap();
    assert!(!msg_json.contains("base64"), "{msg_json}");
    assert!(serde_json::from_str::<RequestMessage>(&msg_json).is_err());
    // The structural hard cap holds; per-provider bounds are separate.
    assert!(MediaBytes::new(vec![0u8; MAX_MEDIA_BYTES_HARD + 1]).is_err());
}

/// The adapter delivery gate: vision, mime allowlist and the provider's
/// own per-image bound, checked BEFORE any wire byte.
#[test]
fn media_delivery_gate_checks_vision_mime_and_size() {
    let mut r = req();
    r.messages.push(RequestMessage {
        role: Role::User,
        content: vec![ContentPart::image_data("image/png", vec![0x89, b'P', b'N', b'G']).unwrap()],
    });
    let vision = ModelCapabilities {
        vision: true,
        ..Default::default()
    };
    validate_media_delivery(&r, &vision, 8).expect("png of 4 bytes under an 8-byte bound");
    // Too small a provider bound refuses typedly (no truncation).
    let err = validate_media_delivery(&r, &vision, 3).unwrap_err();
    assert_eq!(err.kind, ProviderErrorKind::BadRequest);
    assert!(err.message.contains("provider bound"), "{err}");
    // Vision-less refuses even when the size fits.
    let err = validate_media_delivery(
        &r,
        &ModelCapabilities {
            vision: false,
            ..Default::default()
        },
        1 << 20,
    )
    .unwrap_err();
    assert_eq!(err.kind, ProviderErrorKind::BadRequest);
    assert!(err.message.contains("vision"), "{err}");
    // An image mime outside the closed allowlist refuses typedly.
    let mut svg = req();
    svg.messages.push(RequestMessage {
        role: Role::User,
        content: vec![ContentPart::image_data("image/svg+xml", b"<svg/>".to_vec()).unwrap()],
    });
    let err = validate_media_delivery(&svg, &vision, 1 << 20).unwrap_err();
    assert!(err.message.contains("image/svg+xml"), "{err}");
}

/// Document parts: construction is a closed-allowlist typed gate, and
/// the adapter delivery gate refuses document-less models, junk mimes,
/// per-document overruns and request-wide overruns BEFORE any wire byte.
#[test]
fn document_parts_are_typed_and_delivery_gated() {
    let pdf = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n%%EOF".to_vec();
    // Construction refuses non-documents, zero bytes and hostile names.
    assert!(ContentPart::file_data("application/zip", Some("a.zip"), pdf.clone()).is_err());
    assert!(ContentPart::file_data("application/pdf", Some("a.pdf"), Vec::new()).is_err());
    assert!(ContentPart::file_data("application/pdf", Some("../x.pdf"), pdf.clone()).is_err());
    assert!(ContentPart::file_data("APPLICATION/PDF", None, pdf.clone()).is_err());
    // `text/plain` must be UTF-8 to lower byte-exactly everywhere.
    assert!(ContentPart::file_data("text/plain", None, vec![0xff, 0xfe]).is_err());
    ContentPart::file_data("text/plain", None, b"plain text".to_vec()).unwrap();
    let part = ContentPart::file_data("application/pdf", Some("spec.pdf"), pdf.clone()).unwrap();
    match &part.kind {
        ContentKind::FileData {
            mime,
            filename,
            data,
        } => {
            assert_eq!(mime, "application/pdf");
            assert_eq!(filename.as_deref(), Some("spec.pdf"));
            assert_eq!(data.as_slice(), pdf.as_slice());
        }
        other => panic!("expected a FileData part, got {other:?}"),
    }
    // The JSON carrier keeps only digest+size and never decodes back.
    let json = serde_json::to_string(&part).unwrap();
    assert!(!json.contains("base64"), "{json}");
    assert!(!json.contains("PDF"), "bytes must never ride JSON: {json}");
    assert!(serde_json::from_str::<ContentPart>(&json).is_err());

    let req_with_doc = || {
        let mut r = req();
        r.messages.push(RequestMessage {
            role: Role::User,
            content: vec![
                ContentPart::file_data("application/pdf", Some("spec.pdf"), pdf.clone()).unwrap(),
            ],
        });
        r
    };
    let r = req_with_doc();
    validate_document_delivery(&r, true, 1 << 20).expect("a capable model admits the PDF");
    let err = validate_document_delivery(&r, false, 1 << 20).unwrap_err();
    assert_eq!(err.kind, ProviderErrorKind::BadRequest);
    assert!(err.message.contains("document input"), "{err}");
    let err = validate_document_delivery(&r, true, 4).unwrap_err();
    assert!(err.message.contains("provider bound"), "{err}");
    // A directly-built hostile mime (bypassing `file_data`) is refused
    // by the delivery gate's allowlist.
    let mut hostile = req_with_doc();
    hostile.messages[0].content[0].kind = ContentKind::FileData {
        mime: "application/zip".into(),
        filename: None,
        data: MediaBytes::new(b"PK".to_vec()).unwrap(),
    };
    let err = validate_document_delivery(&hostile, true, 1 << 20).unwrap_err();
    assert!(err.message.contains("not deliverable"), "{err}");
    // The request-wide bound refuses a set whose parts each fit.
    let mut two = req_with_doc();
    two.messages.push(RequestMessage {
        role: Role::User,
        content: vec![ContentPart::file_data(
            "application/pdf",
            None,
            vec![b'x'; MAX_REQUEST_DOCUMENT_BYTES + 1],
        )
        .unwrap()],
    });
    let err = validate_document_delivery(&two, true, MAX_MEDIA_BYTES_HARD).unwrap_err();
    assert!(
        err.message.contains("request bound"),
        "the request-wide document bound must apply: {err}"
    );
    // Capability validation: a document-less provider refuses the part
    // at request construction, a capable one admits it.
    let err = CapabilityValidator::validate_documents(&r, false).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Malformed);
    assert!(
        err.message.contains("does not support document input"),
        "{err}"
    );
    CapabilityValidator::validate_documents(&r, true).expect("capable provider admits");
    CapabilityValidator::validate_documents(&req(), false).expect("no documents, no gate");
}

/// Embedding request/response construction enforces every bound with
/// typed errors and the response can never be silently truncated.
#[test]
fn embedding_request_and_response_bounds_are_typed() {
    assert!(EmbeddingRequest::new("m", vec![]).is_err());
    assert!(EmbeddingRequest::new("", vec!["a".into()]).is_err());
    assert!(EmbeddingRequest::new("m", vec![String::new()]).is_err());
    assert!(EmbeddingRequest::new("m", vec!["a".into(); MAX_EMBEDDING_INPUTS + 1]).is_err());
    assert!(EmbeddingRequest::new("m", vec!["x".repeat(MAX_EMBEDDING_INPUT_BYTES + 1)]).is_err());
    let big = "x".repeat(MAX_EMBEDDING_TOTAL_INPUT_BYTES / MAX_EMBEDDING_INPUTS + 1);
    assert!(EmbeddingRequest::new("m", vec![big; MAX_EMBEDDING_INPUTS]).is_err());
    let ok = EmbeddingRequest::new("embed-1", vec!["alpha".into(), "beta".into()]).unwrap();
    assert_eq!(ok.inputs.len(), 2);
    // Lineage is additive: a request without meta reports no deadline
    // and equality ignores meta (retry scripts keep matching).
    assert_eq!(ok.deadline_ms(), 0);
    assert!(ok.meta.is_none());
    let meta = RequestMeta {
        operation_id: OpId::new(7),
        session_id: SessionId::new(3),
        provider: "p".into(),
        attempt: 0,
        deadline_ms: 1234,
        cancellation: CancellationToken::new(),
    };
    let with_meta = EmbeddingRequest::new("embed-1", vec!["alpha".into(), "beta".into()])
        .unwrap()
        .with_meta(meta);
    assert_eq!(with_meta.deadline_ms(), 1234);
    assert_eq!(with_meta.meta.as_ref().unwrap().operation_id, OpId::new(7));
    assert_eq!(with_meta, ok, "meta is lineage, never request identity");

    assert!(EmbeddingResponse::new(vec![]).is_err());
    assert!(EmbeddingResponse::new(vec![vec![]]).is_err());
    assert!(EmbeddingResponse::new(vec![vec![0.0, 0.0], vec![0.0]]).is_err());
    assert!(EmbeddingResponse::new(vec![vec![f32::NAN]]).is_err());
    assert!(EmbeddingResponse::new(vec![vec![0.0; MAX_EMBEDDING_DIMENSIONS + 1]]).is_err());
    let response = EmbeddingResponse::new(vec![vec![0.25, -0.5], vec![1.0, 0.0]]).unwrap();
    response.validate_for(2).unwrap();
    assert!(response.validate_for(3).is_err());
}

/// The fake provider's embedding surface: capability flag + scripted
/// responses consumed in order with captured requests.
#[test]
fn fake_provider_embeddings_are_capability_gated_scripted_and_captured() {
    let unsupported = FakeProvider::new("f", ModelCapabilities::default());
    assert!(!unsupported.supports_embeddings("e"));
    let err = unsupported
        .embed(EmbeddingRequest::new("e", vec!["a".into()]).unwrap())
        .unwrap_err();
    assert_eq!(err.kind, ProviderErrorKind::BadRequest);

    let fake = FakeProvider::new("f", ModelCapabilities::default()).with_embeddings(
        "e",
        vec![Ok(EmbeddingResponse::new(vec![vec![1.0, 2.0]]).unwrap())],
    );
    assert!(fake.supports_embeddings("e"));
    assert!(!fake.supports_embeddings("other"));
    let out = fake
        .embed(EmbeddingRequest::new("e", vec!["a".into()]).unwrap())
        .unwrap();
    assert_eq!(out.vectors, vec![vec![1.0, 2.0]]);
    assert_eq!(fake.embedding_call_count(), 1);
    assert_eq!(fake.embedding_requests()[0].inputs, vec!["a".to_string()]);
    // The script is consumed exactly once: an exhausted fake refuses.
    let err = fake
        .embed(EmbeddingRequest::new("e", vec!["b".into()]).unwrap())
        .unwrap_err();
    assert_eq!(err.kind, ProviderErrorKind::Malformed);
    assert_eq!(fake.embedding_call_count(), 2);
}

#[test]
fn normalizer_never_carries_internal_meta() {
    let r = req();
    let n = RequestNormalizer::normalize(&r);
    // Internal fields (op/session/deadline/cancellation/attempt) simply
    // do not exist on the normalized request — they cannot leak.
    let json = serde_json::to_value(&n).unwrap();
    let obj = json.as_object().unwrap();
    for leaked in [
        "operation_id",
        "session_id",
        "attempt",
        "deadline_ms",
        "cancellation",
    ] {
        assert!(!obj.contains_key(leaked), "internal field {leaked} leaked");
    }
    assert_eq!(obj.len(), 7, "frozen normalized shape");
}

#[test]
fn registry_dynamic_and_capability_source_of_truth() {
    let mut reg = ProviderRegistry::new();
    let fake = FakeProvider::with_script(
        "test",
        ModelCapabilities {
            context: 262144,
            tools: true,
            ..Default::default()
        },
        vec![ScriptedResponse::Text("hi".into()), ScriptedResponse::End],
    );
    reg.try_register(Arc::new(fake)).unwrap();
    assert_eq!(reg.ids(), vec!["test"]);
    let caps = reg.capabilities("test", "qwen3.8").unwrap();
    assert_eq!(caps.context, 262144);
    assert!(caps.tools);
    assert!(reg.capabilities("missing", "x").is_none());
}

#[test]
fn two_openai_compatible_instances_both_register_and_resolve_by_id() {
    // Two OpenAI-compatible endpoints (family "openai") with distinct
    // configured instance ids: on the old registry both inserted under
    // the family id and the second silently overwrote the first.
    let mut reg = ProviderRegistry::new();
    let caps = ModelCapabilities::default();
    for id in ["a-proxy", "b-proxy"] {
        let fake = FakeProvider::with_script(
            "openai",
            caps.clone(),
            vec![ScriptedResponse::Text(id.into()), ScriptedResponse::End],
        );
        reg.try_register(InstanceProvider::wrap(Arc::new(fake), id))
            .unwrap();
    }
    assert_eq!(reg.ids(), vec!["a-proxy", "b-proxy"]);
    assert_eq!(reg.len(), 2, "both instances must survive registration");
    assert!(reg.get("a-proxy").is_some());
    assert!(reg.get("b-proxy").is_some());
    assert!(reg.capabilities("a-proxy", "gpt-5").is_some());
}

#[test]
fn configured_instance_id_resolves_not_family_id() {
    // A provider configured with id "corp-proxy" (family "openai"):
    // sessions configured with "corp-proxy" resolve, and the family id
    // "openai" must NOT resolve to it (the old registry keyed the
    // adapter's family id, so custom ids never looked up).
    let fake = FakeProvider::with_script(
        "openai",
        ModelCapabilities::default(),
        vec![ScriptedResponse::Text("hi".into()), ScriptedResponse::End],
    );
    let wrapped = InstanceProvider::wrap(Arc::new(fake), "corp-proxy");
    assert_eq!(wrapped.identity().instance_id, "corp-proxy");
    assert_eq!(wrapped.identity().family, "openai");
    assert_eq!(
        wrapped.id(),
        "openai",
        "family id stays for capability queries"
    );

    let mut reg = ProviderRegistry::new();
    reg.try_register(wrapped).unwrap();
    assert_eq!(reg.ids(), vec!["corp-proxy"]);
    assert!(reg.get("corp-proxy").is_some());
    assert!(
        reg.get("openai").is_none(),
        "family id must not shadow the instance"
    );
}

#[test]
fn default_identity_is_instance_per_family() {
    // An unwrapped adapter gets instance_id == family: existing single-
    // instance deployments keep resolving exactly as before.
    let fake = FakeProvider::new("ollama", ModelCapabilities::default());
    assert_eq!(fake.identity(), ProviderIdentity::new("ollama", "ollama"));
}

#[test]
fn provider_error_retryability_matches_kind() {
    assert!(ProviderErrorKind::Network.retryable());
    assert!(ProviderErrorKind::Timeout.retryable());
    assert!(ProviderErrorKind::RateLimited.retryable());
    assert!(ProviderErrorKind::Server.retryable());
    assert!(!ProviderErrorKind::BadRequest.retryable());
    assert!(!ProviderErrorKind::Auth.retryable());
    assert!(!ProviderErrorKind::Cancelled.retryable());
    assert!(!ProviderErrorKind::Malformed.retryable());
}

#[tokio::test]
async fn fake_provider_stream_contract() {
    let caps = ModelCapabilities::default();
    let fake = FakeProvider::with_script(
        "f",
        caps,
        vec![
            ScriptedResponse::Text("a".into()),
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "x"}),
            },
            ScriptedResponse::End,
        ],
    );
    let chunks: Vec<_> = fake.stream(req()).collect().await;
    assert_eq!(chunks.len(), 3);
    assert!(matches!(chunks[0], Ok(ProviderChunk::Text { .. })));
    assert!(matches!(chunks[1], Ok(ProviderChunk::ToolCall { .. })));
    assert_eq!(chunks[2], Ok(ProviderChunk::Done));
}

/// CASE INV-FAKE-STREAM-BOUNDARY: one stream is one model response. A
/// scripted tool continuation is a separate response: the tool call(s)
/// go to the first stream and the following text+End to the next.
/// Before this boundary the first stream drained the whole script, so
/// the post-tool request saw an empty completion and every tool-using
/// turn failed.
#[tokio::test]
async fn fake_provider_splits_a_tool_continuation_into_two_responses() {
    let fake = FakeProvider::with_script(
        "f",
        ModelCapabilities::default(),
        vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "x"}),
            },
            ScriptedResponse::Text("after tool".into()),
            ScriptedResponse::End,
        ],
    );
    let first: Vec<_> = fake.stream(req()).collect().await;
    assert_eq!(first.len(), 2, "tool call + synthetic Done");
    assert!(matches!(first[0], Ok(ProviderChunk::ToolCall { .. })));
    assert_eq!(first[1], Ok(ProviderChunk::Done));
    let second: Vec<_> = fake.stream(req()).collect().await;
    assert_eq!(second.len(), 2);
    assert!(matches!(second[0], Ok(ProviderChunk::Text { .. })));
    assert_eq!(second[1], Ok(ProviderChunk::Done));
    let exhausted: Vec<_> = fake.stream(req()).collect().await;
    assert_eq!(exhausted, vec![Ok(ProviderChunk::Done)]);
}

/// CASE INV-FAKE-STREAM-BATCH: a batch of tool calls is still ONE
/// response; the continuation text is the next one. No stream may
/// replay consumed responses.
#[tokio::test]
async fn fake_provider_batches_consecutive_tool_calls_in_one_response() {
    let fake = FakeProvider::with_script(
        "f",
        ModelCapabilities::default(),
        vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "x"}),
            },
            ScriptedResponse::ToolCall {
                id: "c2".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "y"}),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
    );
    let first: Vec<_> = fake.stream(req()).collect().await;
    assert_eq!(first.len(), 3, "two tool calls + synthetic Done");
    assert!(first[..2]
        .iter()
        .all(|item| matches!(item, Ok(ProviderChunk::ToolCall { .. }))));
    assert_eq!(first[2], Ok(ProviderChunk::Done));
    let second: Vec<_> = fake.stream(req()).collect().await;
    assert_eq!(second.len(), 2);
    assert!(matches!(second[0], Ok(ProviderChunk::Text { .. })));
    assert_eq!(second[1], Ok(ProviderChunk::Done));
}

#[tokio::test]
async fn fake_provider_dies_mid_stream() {
    let fake = FakeProvider::die_mid_stream("f", ModelCapabilities::default());
    let chunks: Vec<_> = fake.stream(req()).collect().await;
    assert!(chunks.len() == 2 && chunks[0].is_ok() && chunks[1].is_err());
    assert_eq!(
        chunks[1].as_ref().unwrap_err().kind,
        ProviderErrorKind::Network
    );
}

#[test]
fn duplicate_instance_id_is_conflict_and_first_entry_wins() {
    let caps_a = ModelCapabilities {
        context: 1000,
        ..Default::default()
    };
    let caps_b = ModelCapabilities {
        context: 2000,
        ..Default::default()
    };
    let first = FakeProvider::with_script(
        "family",
        caps_a.clone(),
        vec![
            ScriptedResponse::Text("first".into()),
            ScriptedResponse::End,
        ],
    );
    let first: Arc<dyn Provider> = Arc::new(first);
    let second = FakeProvider::with_script(
        "family",
        caps_b,
        vec![
            ScriptedResponse::Text("second".into()),
            ScriptedResponse::End,
        ],
    );
    let mut reg = ProviderRegistry::new();
    assert!(reg.try_register(first.clone()).is_ok());
    // Same id, a DIFFERENT instance (fresh Arc over equal content) is a
    // typed Conflict and the FIRST registration stays untouched.
    let err = reg.try_register(Arc::new(second)).unwrap_err();
    assert_eq!(
        err.kind,
        faktor_core::error::ErrorKind::Conflict,
        "duplicate instance id is a Conflict"
    );
    assert_eq!(reg.len(), 1, "the first entry is never replaced");
    let caps = reg.capabilities("family", "m").unwrap();
    assert_eq!(
        caps.context, 1000,
        "capabilities come from the FIRST registration"
    );
    assert!(
        Arc::ptr_eq(&reg.get("family").unwrap(), &first),
        "the stored instance is exactly the first Arc, never a replacement"
    );
    // Same id, the SAME instance (a clone of the first Arc) is an
    // idempotent no-op: Ok, nothing changes, nothing is duplicated.
    assert!(reg.try_register(first.clone()).is_ok());
    assert_eq!(reg.len(), 1);
    assert_eq!(reg.capabilities("family", "m").unwrap().context, 1000);
    assert!(
        Arc::ptr_eq(&reg.get("family").unwrap(), &first),
        "the idempotent re-registration kept the original instance"
    );
    // Distinct instance ids of the same family still coexist.
    let wrapped = InstanceProvider::wrap(
        Arc::new(FakeProvider::new("family", ModelCapabilities::default())),
        "second-instance",
    );
    assert!(reg.try_register(wrapped).is_ok());
    assert_eq!(reg.len(), 2);
}

#[test]
fn hostile_and_case_variant_instance_ids_are_typed_refusals() {
    use faktor_core::error::ErrorKind;
    // Empty instance ids never enter the map.
    let mut reg = ProviderRegistry::new();
    let empty = Arc::new(FakeProvider::with_script(
        "",
        ModelCapabilities::default(),
        vec![ScriptedResponse::End],
    ));
    let err = reg.try_register(empty).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Malformed, "{err}");
    assert_eq!(reg.len(), 0, "the hostile registration never lands");
    // Oversized instance ids are refused with the typed Oversized kind.
    let huge = Arc::new(FakeProvider::with_script(
        &"x".repeat(MAX_PROVIDER_INSTANCE_ID_BYTES + 1),
        ModelCapabilities::default(),
        vec![ScriptedResponse::End],
    ));
    let err = reg.try_register(huge).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Oversized, "{err}");
    assert_eq!(reg.len(), 0);
    // The boundary is exact: MAX bytes is still a valid id.
    let ok = Arc::new(FakeProvider::with_script(
        &"y".repeat(MAX_PROVIDER_INSTANCE_ID_BYTES),
        ModelCapabilities::default(),
        vec![ScriptedResponse::End],
    ));
    assert!(reg.try_register(ok).is_ok());
    assert_eq!(reg.len(), 1);
    // An empty id must not be recoverable through the idempotent path
    // either: hostile ids are refused before any duplicate logic runs.
    assert!(reg
        .try_register(Arc::new(FakeProvider::with_script(
            "",
            ModelCapabilities::default(),
            vec![ScriptedResponse::End],
        )))
        .is_err());

    // Duplicate-key case variants: an id that differs from a registered
    // key ONLY by case is a typed Conflict; the FIRST registration is
    // kept and the case variant never lands.
    let mut reg = ProviderRegistry::new();
    let first: Arc<dyn Provider> = Arc::new(FakeProvider::with_script(
        "Corp-Proxy",
        ModelCapabilities {
            context: 3000,
            ..Default::default()
        },
        vec![ScriptedResponse::End],
    ));
    assert!(reg.try_register(first.clone()).is_ok());
    for hostile in ["corp-proxy", "CORP-PROXY", "cOrP-pRoXy"] {
        let variant = Arc::new(FakeProvider::with_script(
            hostile,
            ModelCapabilities {
                context: 4000,
                ..Default::default()
            },
            vec![ScriptedResponse::End],
        ));
        let err = reg.try_register(variant).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict, "{hostile:?}: {err}");
    }
    assert_eq!(reg.len(), 1, "no case variant ever registers");
    assert_eq!(reg.capabilities("Corp-Proxy", "m").unwrap().context, 3000);
    assert!(
        Arc::ptr_eq(&reg.get("Corp-Proxy").unwrap(), &first),
        "the FIRST registration survives every case-variant attack"
    );
    // A genuinely distinct id in the same family still registers.
    assert!(reg
        .try_register(InstanceProvider::wrap(
            Arc::new(FakeProvider::new(
                "Corp-Proxy",
                ModelCapabilities::default()
            )),
            "second-proxy",
        ))
        .is_ok());
    assert_eq!(reg.len(), 2);
    // Resolution stays exact-case: hostile lookups of the registered
    // canonical key are what the registry serves, and case-swapped
    // lookups miss (there is no silent canonicalization of lookups).
    assert!(reg.get("Corp-Proxy").is_some());
    assert!(reg.get("corp-proxy").is_none());
}

#[test]
fn infallible_provider_registration_api_is_gone_from_the_crate_source() {
    // P0-41 compile proof: the infallible duplicate-accepting
    // `ProviderRegistry::register` shim (warn-on-conflict) no longer
    // exists anywhere in this crate's source. If a future wave re-adds
    // an infallible registration API, this test fails at compile/run
    // time by scanning the crate sources.
    let mut scanned = 0usize;
    let reg_sig = ["fn ", "register"].concat();
    let warn_marker = ["provider ", "registration ", "rejected"].concat();
    for entry in std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/src")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        scanned += 1;
        let src = std::fs::read_to_string(&path).unwrap();
        for line in src.lines() {
            // A registration method for Arc<dyn Provider> that does NOT
            // return Result is the infallible footgun.
            if line.contains(&reg_sig) && line.contains("Arc<dyn Provider>") {
                assert!(
                    line.contains("-> Result"),
                    "{}:{}: infallible provider registration API re-added: {line}",
                    path.display(),
                    line
                );
            }
            // The warn-on-duplicate shim marker must never reappear.
            assert!(
                !line.contains(&warn_marker),
                "{}:{}: warn-on-duplicate registration shim re-added",
                path.display(),
                line
            );
        }
    }
    assert!(
        scanned >= 2,
        "the source scan must actually cover the provider crate files (found {scanned})"
    );
}

#[test]
fn tokenizer_mapping_matrix_rows() {
    use TokenFamily as F;
    // o200k rows: gpt-4o/gpt-4.1/o1/o3/gpt-5 families. Specific
    // prefixes must win over the generic `gpt-4` cl100k row.
    for model in [
        "gpt-4o",
        "gpt-4o-mini",
        "gpt-4o1",
        "gpt-4.1",
        "gpt-4.1-mini",
        "o1",
        "o1-mini",
        "o1-pro",
        "o3",
        "o3-mini",
        "gpt-5",
        "gpt-5-mini",
        "GPT-5",
        "gpt-5-codex",
    ] {
        let id = tokenizer_for(model, None);
        assert_eq!(
            id,
            TokenizerId::O200K_BASE,
            "{model} must map to o200k_base (got {id})"
        );
        assert_eq!(id.family, F::O200kBase);
        assert_eq!(id.version, 1, "all named rows are generation 1");
    }
    // cl100k rows: gpt-3.5 / gpt-4 (incl. turbo + 4.5 leftovers).
    for model in [
        "gpt-4",
        "gpt-4-turbo",
        "gpt-4-1106-preview",
        "gpt-4.5",
        "gpt-3.5",
        "gpt-3.5-turbo",
        "GPT-4",
    ] {
        assert_eq!(
            tokenizer_for(model, None),
            TokenizerId::CL100K_BASE,
            "{model} must map to cl100k_base"
        );
    }
    // Anthropic / Gemini / Llama rows.
    for model in [
        "claude-3-5-sonnet",
        "claude-3-7-sonnet",
        "claude-sonnet-4-5",
        "claude-opus-4-1",
        "Claude-Opus-4-1",
        "claude-haiku-4-5",
    ] {
        assert_eq!(
            tokenizer_for(model, None),
            TokenizerId::ANTHROPIC,
            "{model} must map to anthropic"
        );
    }
    for model in [
        "gemini-2.5-pro",
        "gemini-2.5-flash",
        "gemini-3",
        "Gemini-2.5-Pro",
    ] {
        assert_eq!(
            tokenizer_for(model, None),
            TokenizerId::GEMINI,
            "{model} must map to gemini"
        );
    }
    for model in [
        "llama-3.3-70b",
        "Llama-3.1-8B",
        "qwen3.8",
        "qwen3-coder",
        "qwen-2.5-72b",
    ] {
        assert_eq!(
            tokenizer_for(model, None),
            TokenizerId::LLAMA,
            "{model} must map to llama"
        );
    }
    // Slash-qualified routed model strings resolve by the last segment.
    assert_eq!(
        tokenizer_for("anthropic/claude-sonnet-4-5", None),
        TokenizerId::ANTHROPIC
    );
    assert_eq!(tokenizer_for("openai/gpt-5", None), TokenizerId::O200K_BASE);
    assert_eq!(tokenizer_for("ollama/qwen3.8", None), TokenizerId::LLAMA);
}

#[test]
fn tokenizer_mapping_unknown_models_are_generic_and_never_upgraded() {
    // Unknown/empty/hostile model strings conservatively map to the
    // GenericEstimator fallback — a hint NEVER upgrades them to a named
    // family (an OpenAI-compatible endpoint is not an OpenAI tokenizer).
    for model in [
        "",
        "/",
        "default",
        "my-custom-model",
        "gpt",
        "gpt-x",
        "o",
        "o0",
        "xqwen",
        "gemma-2-9b",
        "mistral-large",
        "deepseek-chat",
        "deepseek-reasoner",
        "gpt5",
        "gpt_5",
        "😀-model",
    ] {
        let hint = Some("openai");
        assert_eq!(
            tokenizer_for(model, hint),
            TokenizerId::GENERIC_ESTIMATOR,
            "{model:?} must conservatively map to generic_estimator"
        );
    }
}

#[test]
fn tokenizer_mapping_deepseek_rows_depend_on_the_deployment_hint() {
    // deepseek weights are llama-family ONLY when a llama-family runtime
    // (ollama/llama.cpp) serves them; the official deepseek API keeps
    // its own non-llama tokenizer → conservative generic fallback.
    for model in ["deepseek-chat", "deepseek-reasoner", "deepseek-v3"] {
        assert_eq!(
            tokenizer_for(model, None),
            TokenizerId::GENERIC_ESTIMATOR,
            "{model} on the official API is NOT llama-family"
        );
        assert_eq!(
            tokenizer_for(model, Some("deepseek")),
            TokenizerId::GENERIC_ESTIMATOR
        );
        assert_eq!(
            tokenizer_for(model, Some("ollama")),
            TokenizerId::LLAMA,
            "{model} under a llama-family runtime maps to llama"
        );
        assert_eq!(
            tokenizer_for(model, Some("local-llama-cpp")),
            TokenizerId::LLAMA
        );
    }
}

#[test]
fn tokenizer_id_is_total_order_display_and_serde_stable() {
    use TokenFamily as F;
    // Derived Ord: variant declaration order, then version. A sorted
    // vec is deterministic across processes (cache keys rely on it).
    let mut ids = vec![
        TokenizerId::GENERIC_ESTIMATOR,
        TokenizerId {
            family: F::O200kBase,
            version: 2,
        },
        TokenizerId::CL100K_BASE,
        TokenizerId::LLAMA,
        TokenizerId::GEMINI,
        TokenizerId::ANTHROPIC,
        TokenizerId::O200K_BASE,
    ];
    let expected = vec![
        TokenizerId::O200K_BASE,
        TokenizerId {
            family: F::O200kBase,
            version: 2,
        },
        TokenizerId::CL100K_BASE,
        TokenizerId::ANTHROPIC,
        TokenizerId::GEMINI,
        TokenizerId::LLAMA,
        TokenizerId::GENERIC_ESTIMATOR,
    ];
    ids.sort();
    assert_eq!(ids, expected, "deterministic total order");
    assert!(
        TokenizerId::O200K_BASE
            < TokenizerId {
                family: F::O200kBase,
                version: 2,
            }
    );
    // Display.
    assert_eq!(TokenizerId::O200K_BASE.to_string(), "o200k_base@v1");
    assert_eq!(
        TokenizerId::GENERIC_ESTIMATOR.to_string(),
        "generic_estimator@v1"
    );
    assert_eq!(TokenFamily::Anthropic.to_string(), "anthropic");
    // Serde round-trips with the frozen wire names (snake_case).
    let json = serde_json::to_value(TokenizerId::O200K_BASE).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"family": "o200k_base", "version": 1})
    );
    assert_eq!(
        serde_json::from_value::<TokenizerId>(json).unwrap(),
        TokenizerId::O200K_BASE
    );
    assert_eq!(
        serde_json::from_value::<TokenizerId>(serde_json::json!(
            {"family": "cl100k_base", "version": 1}
        ))
        .unwrap(),
        TokenizerId::CL100K_BASE
    );
    // Hostile json: unknown family is a serde error, never a silent map.
    assert!(serde_json::from_value::<TokenizerId>(serde_json::json!(
        {"family": "not_a_family", "version": 1}
    ))
    .is_err());
    // Default is the conservative fallback.
    assert_eq!(TokenizerId::default(), TokenizerId::GENERIC_ESTIMATOR);
}

#[test]
fn tokenizer_mapping_case_and_whitespace_hostile_rows() {
    // Case folding and trim are part of the mapping contract; whitespace
    // INSIDE the name is not stripped (hostile rows stay generic).
    assert_eq!(tokenizer_for("  GPT-5  ", None), TokenizerId::O200K_BASE);
    assert_eq!(
        tokenizer_for("\tclaude-sonnet-4", None),
        TokenizerId::ANTHROPIC
    );
    assert_eq!(tokenizer_for("gpt-5\n", None), TokenizerId::O200K_BASE);
    assert_eq!(tokenizer_for("gpt-4o\n ", None), TokenizerId::O200K_BASE);
    assert_eq!(
        tokenizer_for("gpt-4o", None),
        tokenizer_for(" GPT-4O\n", None),
        "leading whitespace, case and trailing newlines never change the identity"
    );
    assert_eq!(
        tokenizer_for("qwen3.8", None),
        tokenizer_for("ollama/qwen3.8", None),
        "the provider prefix before the last '/' never changes the identity"
    );
}

#[test]
fn attachment_limits_contract_is_one_canonical_serializable_shape() {
    let limits = AttachmentLimits::for_model(
        7 * 1024 * 1024,
        10 * 1024 * 1024,
        MAX_MODEL_IMAGE_BYTES,
        true,
        MAX_MODEL_DOCUMENT_BYTES,
    );
    let json = serde_json::to_value(&limits).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "maxUploadBytes": 7_340_032u64,
            "maxRequestBytes": 10_485_760u64,
            "maxAttachmentBytes": faktor_core::attachment::MAX_ATTACHMENT_BYTES,
            "image": {
                "mimes": [
                    {"mime": "image/png", "maxBytes": 5_242_880u64},
                    {"mime": "image/jpeg", "maxBytes": 5_242_880u64},
                    {"mime": "image/gif", "maxBytes": 5_242_880u64},
                    {"mime": "image/webp", "maxBytes": 5_242_880u64}
                ],
                "maxRequestBytes": 16_777_216u64
            },
            "document": {
                "capable": true,
                "mimes": [
                    {"mime": "application/pdf", "maxBytes": 8_388_608u64},
                    {"mime": "text/plain", "maxBytes": 8_388_608u64}
                ],
                "maxRequestBytes": 16_777_216u64
            }
        })
    );
    // The advertised contract round-trips strictly: unknown members and
    // missing members are typed serde refusals, never silent defaults.
    let back: AttachmentLimits = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(back, limits);
    let mut extra = json.clone();
    extra["extra"] = serde_json::json!(1);
    assert!(serde_json::from_value::<AttachmentLimits>(extra).is_err());
    let mut missing = json.clone();
    missing["document"]
        .as_object_mut()
        .unwrap()
        .remove("capable");
    assert!(serde_json::from_value::<AttachmentLimits>(missing).is_err());
    // A provider bound above the structural ceiling is capped, never
    // advertised larger than any resolvable attachment; a document-less
    // provider advertises capable=false with its honest bound.
    let capped = AttachmentLimits::for_model(
        1,
        2,
        MAX_MEDIA_BYTES_HARD + 4096,
        false,
        MAX_MEDIA_BYTES_HARD + 4096,
    );
    assert_eq!(
        capped.image.mimes[0].max_bytes, MAX_MEDIA_BYTES_HARD as u64,
        "the structural ceiling caps every advertised per-MIME bound"
    );
    assert_eq!(
        capped.document.mimes[1].max_bytes,
        MAX_MEDIA_BYTES_HARD as u64
    );
    assert!(!capped.document.capable);
}

#[test]
fn catalog_provider_index_keeps_first_match_and_fallback_semantics() {
    // Complexity-audit regression: the indexed lookup must be
    // indistinguishable from the pre-fix linear `find` — first
    // insertion wins for a repeated id, an unnamed id answers the
    // declared-generic default, and `known_models` keeps insertion
    // order (duplicates included).
    let named = ModelCapabilities {
        tools: true,
        ..ModelCapabilities::default()
    };
    let replaced = ModelCapabilities {
        vision: true,
        ..ModelCapabilities::default()
    };
    let mut provider = CatalogProvider::new("catalog", ModelCapabilities::default());
    provider
        .add_model("m1", named.clone())
        .add_model("m2", ModelCapabilities::default())
        .add_model("m1", replaced);
    assert_eq!(provider.capabilities("m1"), named, "first row wins");
    assert_eq!(provider.capabilities("m2"), ModelCapabilities::default());
    assert_eq!(
        provider.capabilities("missing"),
        ModelCapabilities::default(),
        "unnamed models answer the declared default"
    );
    assert!(matches!(
        provider.capability_resolution("m1"),
        CapabilityResolution { .. }
    ));
    assert_eq!(
        provider.known_models(),
        vec!["m1".to_string(), "m2".to_string(), "m1".to_string()],
        "insertion order, duplicates included"
    );
    // The declared-generic fallback is never `Known`.
    assert_ne!(
        provider.capability_resolution("missing").authority,
        provider.capability_resolution("m1").authority
    );
}

// ---- audit item 9: ToolEffects authority, not ResourceClass ----------

/// Adversarial authority seed: `is_read_only` is a conjunction over the
/// FIVE mutating axes. Flipping ANY one of them false→true must break
/// read-only authority; the read axes must never do so.
#[test]
fn read_only_requires_every_mutating_axis_false_and_read_axes_never_veto() {
    for veto in [
        |e: &mut ToolEffects| e.workspace_write = true,
        |e: &mut ToolEffects| e.process_exec = true,
        |e: &mut ToolEffects| e.network_write = true,
        |e: &mut ToolEffects| e.external_mutation = true,
        |e: &mut ToolEffects| e.git_mutation = true,
    ] {
        let mut effects = ToolEffects::READ_ONLY;
        assert!(effects.is_read_only());
        veto(&mut effects);
        assert!(
            !effects.is_read_only(),
            "a single mutating axis must break conversational authority: {effects:?}"
        );
    }
    for read_veto in [
        |e: &mut ToolEffects| e.workspace_read = false,
        |e: &mut ToolEffects| e.network_read = false,
    ] {
        let mut effects = ToolEffects::READ_ONLY;
        read_veto(&mut effects);
        assert!(
            effects.is_read_only(),
            "a read axis never disqualifies conversational authority: {effects:?}"
        );
    }
    // The exact audit trap: a Network-class-shaped effects value that
    // mutates the external world is NOT read-only, whatever its
    // scheduling class claims elsewhere.
    let mut network_mutating = ToolEffects::NETWORK_READ_ONLY;
    network_mutating.external_mutation = true;
    assert!(!network_mutating.is_read_only());
}

/// Old serialized ToolSpec JSON (no `effects` key) must decode — the
/// wire shape is backward-compatible — and MUST land on the
/// conservative all-mutating shape: unknown ⇒ mutating for authority.
#[test]
fn old_serialized_tool_spec_without_effects_decodes_conservative_and_not_read_only() {
    let old = serde_json::json!({
        "name": "read_file",
        "description": "d",
        "input_schema": { "type": "object" }
    });
    let spec: ToolSpec = serde_json::from_value(old).expect("old specs must still decode");
    assert_eq!(spec.effects, ToolEffects::CONSERVATIVE);
    assert!(
        !spec.effects.is_read_only(),
        "an effects-free legacy spec must never be conversational-authority-clean"
    );
    // A PARTIAL effects object is also fail-closed per axis: every
    // undeclared axis decays to true (mutating).
    let partial: ToolSpec = serde_json::from_value(serde_json::json!({
        "name": "t",
        "description": "d",
        "input_schema": {},
        "effects": { "workspace_read": true, "network_read": true }
    }))
    .expect("partial effects decode");
    assert!(!partial.effects.is_read_only());
    assert!(partial.effects.workspace_write);
    assert!(partial.effects.external_mutation);
    // A future UNDECLARED axis in the JSON is a hard decode error
    // (deny_unknown_fields): authority metadata never silently
    // under-declares under an old binary.
    let unknown_axis = serde_json::json!({
        "name": "t",
        "description": "d",
        "input_schema": {},
        "effects": {
            "workspace_read": true, "workspace_write": false,
            "process_exec": false, "network_read": false,
            "network_write": false, "external_mutation": false,
            "git_mutation": false, "hypothetical_future_axis": true
        }
    });
    assert!(serde_json::from_value::<ToolSpec>(unknown_axis).is_err());
}

/// An explicit read-only declaration round-trips byte-exactly and is
/// the ONLY shape that is conversational-authority-clean; the
/// conservative shape serializes its mutating axes as `true`, never as
/// an absent (implicitly-false) key.
#[test]
fn explicit_effects_round_trip_and_conservative_serializes_axes_true() {
    let spec = ToolSpec {
        name: "search".into(),
        description: "d".into(),
        input_schema: serde_json::json!({}),
        effects: ToolEffects::READ_ONLY,
    };
    let bytes = serde_json::to_vec(&spec).unwrap();
    let back: ToolSpec = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(back, spec);
    assert!(back.effects.is_read_only());
    let conservative = serde_json::to_value(ToolSpec {
        name: "t".into(),
        description: "d".into(),
        input_schema: serde_json::json!({}),
        effects: ToolEffects::CONSERVATIVE,
    })
    .unwrap();
    for axis in [
        "workspace_read",
        "workspace_write",
        "process_exec",
        "network_read",
        "network_write",
        "external_mutation",
        "git_mutation",
    ] {
        assert_eq!(
            conservative["effects"][axis],
            serde_json::Value::Bool(true),
            "the conservative shape must serialize `{axis}` as an explicit true"
        );
    }
}
