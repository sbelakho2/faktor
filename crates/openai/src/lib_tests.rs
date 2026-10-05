#[cfg(test)]
mod openai_contract_tests {
    use super::super::*;
    use faktor_core::cancellation::CancellationToken;
    use faktor_core::id::{OpId, SessionId};
    use faktor_provider::egress::MockHttpTransport;
    use faktor_provider::testing::{sse_body, MockAction, MockServer};
    use faktor_provider::{ContentPart, RequestMessage, RequestMeta, ToolSpec};
    use faktor_security::destination::DestinationPolicy;
    use futures::StreamExt;

    fn req(model: &str) -> GenericAgentRequest {
        GenericAgentRequest {
            model: model.into(),
            system: "sys".into(),
            messages: vec![RequestMessage {
                role: Role::User,
                content: vec![ContentPart::text("hi")],
            }],
            tools: vec![ToolSpec {
                name: "read_file".into(),
                description: "read".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            max_output: Some(1000),
            reasoning: None,
            stream: true,
            meta: RequestMeta {
                operation_id: OpId::new(1),
                session_id: SessionId::new(1),
                provider: "openai".into(),
                attempt: 0,
                deadline_ms: 5000,
                cancellation: CancellationToken::new(),
            },
        }
    }

    /// Resolved image attachments lower BYTE-EXACTLY on both wire families:
    /// Chat gets `image_url` with a base64 data URL, Responses gets
    /// `input_image` with the same data URL. The base64 and media type are
    /// asserted exactly, not approximately.
    #[tokio::test]
    async fn image_data_lowers_byte_exact_on_chat_and_responses() {
        let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 7, 8, 9];
        let media = faktor_provider::MediaBytes::new(png.clone()).unwrap();
        let expected_url = media.to_data_url("image/png");
        assert_eq!(
            expected_url,
            format!("data:image/png;base64,{}", media.to_base64()),
            "the data URL is the standard-alphabet base64 of the raw bytes"
        );
        // Chat family.
        let server = MockServer::new();
        let expected_chat = expected_url.clone();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::AssertThenRespond {
                status: 200,
                body: sse_body(&[
                    serde_json::json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}),
                ]),
                assert: Arc::new(move |body: &serde_json::Value| {
                    assert_eq!(
                        body["messages"][2]["content"],
                        serde_json::json!([
                            { "type": "text", "text": "look" },
                            {
                                "type": "image_url",
                                "image_url": { "url": expected_chat }
                            }
                        ]),
                        "Chat image lowering must be byte-exact"
                    );
                }),
            },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::permissive_for_tests(OpenAiConfig::chat(base, None));
        assert_eq!(provider.max_image_bytes(), OPENAI_MAX_IMAGE_BYTES);
        let mut r = req("m1");
        r.messages.push(RequestMessage {
            role: Role::User,
            content: vec![
                ContentPart::text("look"),
                ContentPart::image_data("image/png", png.clone()).unwrap(),
            ],
        });
        let mut stream = provider.stream(r);
        while let Some(chunk) = stream.next().await {
            if matches!(chunk.unwrap(), ProviderChunk::Done) {
                break;
            }
        }
        assert_eq!(server.request_count(), 1);

        // Responses family: the native item protocol.
        let server = MockServer::new();
        let expected_responses = expected_url.clone();
        server.route(
            "POST",
            "/responses",
            MockAction::AssertThenRespond {
                status: 200,
                body: sse_body(&[
                    serde_json::json!({"type":"response.output_text.delta","delta":"ok"}),
                    serde_json::json!({"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}),
                ]),
                assert: Arc::new(move |body: &serde_json::Value| {
                    assert_eq!(
                        body["input"][1]["content"],
                        serde_json::json!([
                            { "type": "input_text", "text": "look" },
                            {
                                "type": "input_image",
                                "image_url": expected_responses
                            }
                        ]),
                        "Responses image lowering must be byte-exact"
                    );
                }),
            },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::permissive_for_tests(OpenAiConfig::responses(base, None));
        let mut r = req("m1");
        r.messages.push(RequestMessage {
            role: Role::User,
            content: vec![
                ContentPart::text("look"),
                ContentPart::image_data("image/png", png).unwrap(),
            ],
        });
        let mut stream = provider.stream(r);
        while let Some(chunk) = stream.next().await {
            if matches!(chunk.unwrap(), ProviderChunk::Done) {
                break;
            }
        }
        assert_eq!(server.request_count(), 1);
    }

    /// Resolved DOCUMENT attachments lower BYTE-EXACTLY on both wire
    /// families: Chat gets `{type: "file"}` with the display filename and a
    /// base64 data URL, Responses gets `input_file` with the same fields.
    #[tokio::test]
    async fn document_data_lowers_byte_exact_on_chat_and_responses() {
        let pdf: Vec<u8> = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n%%EOF".to_vec();
        let media = faktor_provider::MediaBytes::new(pdf.clone()).unwrap();
        let expected_url = media.to_data_url("application/pdf");
        // Chat family.
        let server = MockServer::new();
        let expected_chat = expected_url.clone();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::AssertThenRespond {
                status: 200,
                body: sse_body(&[
                    serde_json::json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}),
                ]),
                assert: Arc::new(move |body: &serde_json::Value| {
                    assert_eq!(
                        body["messages"][2]["content"],
                        serde_json::json!([
                            { "type": "text", "text": "read" },
                            {
                                "type": "file",
                                "file": {
                                    "filename": "spec.pdf",
                                    "file_data": expected_chat
                                }
                            }
                        ]),
                        "Chat document lowering must be byte-exact"
                    );
                }),
            },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::permissive_for_tests(OpenAiConfig::chat(base, None));
        assert!(provider.document_capable("m1"));
        let mut r = req("m1");
        r.messages.push(RequestMessage {
            role: Role::User,
            content: vec![
                ContentPart::text("read"),
                ContentPart::file_data("application/pdf", Some("spec.pdf"), pdf.clone()).unwrap(),
            ],
        });
        let mut stream = provider.stream(r);
        while let Some(chunk) = stream.next().await {
            if matches!(chunk.unwrap(), ProviderChunk::Done) {
                break;
            }
        }
        assert_eq!(server.request_count(), 1);

        // Responses family.
        let server = MockServer::new();
        let expected_responses = expected_url.clone();
        server.route(
            "POST",
            "/responses",
            MockAction::AssertThenRespond {
                status: 200,
                body: sse_body(&[
                    serde_json::json!({"type":"response.output_text.delta","delta":"ok"}),
                    serde_json::json!({"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}),
                ]),
                assert: Arc::new(move |body: &serde_json::Value| {
                    assert_eq!(
                        body["input"][1]["content"],
                        serde_json::json!([
                            { "type": "input_text", "text": "read" },
                            {
                                "type": "input_file",
                                "filename": "spec.pdf",
                                "file_data": expected_responses
                            }
                        ]),
                        "Responses document lowering must be byte-exact"
                    );
                }),
            },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::permissive_for_tests(OpenAiConfig::responses(base, None));
        let mut r = req("m1");
        r.messages.push(RequestMessage {
            role: Role::User,
            content: vec![
                ContentPart::text("read"),
                ContentPart::file_data("application/pdf", Some("spec.pdf"), pdf).unwrap(),
            ],
        });
        let mut stream = provider.stream(r);
        while let Some(chunk) = stream.next().await {
            if matches!(chunk.unwrap(), ProviderChunk::Done) {
                break;
            }
        }
        assert_eq!(server.request_count(), 1);
    }

    /// A vision-less model and an over-bound image are refused by the
    /// adapter delivery gate BEFORE any wire byte (request count stays 0).
    #[tokio::test]
    async fn image_delivery_gate_is_typed_and_pre_wire() {
        let server = MockServer::new();
        let base = server.base_url().await;
        let provider = OpenAiProvider::permissive_for_tests(
            OpenAiConfig::chat(base.clone(), None).with_model(
                "m1",
                ModelCapabilities {
                    vision: false,
                    ..Default::default()
                },
            ),
        );
        let mut r = req("m1");
        r.messages.push(RequestMessage {
            role: Role::User,
            content: vec![
                ContentPart::image_data("image/png", vec![0x89, b'P', b'N', b'G']).unwrap(),
            ],
        });
        let err = provider.stream(r).next().await.unwrap().unwrap_err();
        assert_eq!(err.kind, ProviderErrorKind::BadRequest);
        assert!(err.message.contains("vision"), "{err}");
        assert_eq!(server.request_count(), 0, "no wire byte on a gate refusal");

        // Over the provider's own per-image bound: typed, pre-wire.
        let provider = OpenAiProvider::permissive_for_tests(OpenAiConfig::chat(base, None));
        let mut r = req("m1");
        r.messages.push(RequestMessage {
            role: Role::User,
            content: vec![ContentPart {
                kind: ContentKind::ImageData {
                    mime: "image/png".into(),
                    data: faktor_provider::MediaBytes::new(vec![0u8; OPENAI_MAX_IMAGE_BYTES + 1])
                        .unwrap(),
                },
                tool_call_id: None,
            }],
        });
        let err = provider.stream(r).next().await.unwrap().unwrap_err();
        assert_eq!(err.kind, ProviderErrorKind::BadRequest);
        assert!(err.message.contains("exceeds"), "{err}");
        assert_eq!(
            server.request_count(),
            0,
            "no wire byte on an oversize refusal"
        );
    }

    #[test]
    fn usage_parse_splits_cached_input_and_hostile_rows_error() {
        // Audit Phase-1 item C: `prompt_tokens` is the TOTAL input INCLUDING
        // the cached portion — the canonical frame must arrive with the
        // cached tokens split into `cache_read_tokens` (billed at the cache
        // line) and the remainder as `uncached_input_tokens`. Reasoning
        // rides inside `completion_tokens`; the detail is informational.
        let frame = serde_json::json!({
            "id": "chatcmpl-9",
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "prompt_tokens_details": {"cached_tokens": 40},
                "completion_tokens_details": {"reasoning_tokens": 30}
            }
        });
        let mut accs = Vec::new();
        let mut pending = std::collections::VecDeque::new();
        let chunk = parse_chat_chunk(&frame, &mut accs, &mut pending)
            .expect("usage chunk")
            .expect("usage frame");
        assert_eq!(
            chunk,
            ProviderChunk::Usage(CanonicalUsage {
                uncached_input_tokens: 60,
                cache_read_tokens: 40,
                cache_write_tokens: 0,
                output_tokens: 50,
                reasoning_tokens: 30,
                reported_cost: None,
                request_id: Some("chatcmpl-9".into()),
            })
        );
        // Missing cache details: the conservative category is uncached =
        // the reported total (never invent a cheaper cache line).
        let no_cache = serde_json::json!({
            "usage": {"prompt_tokens": 1000, "completion_tokens": 50}
        });
        let chunk = parse_chat_chunk(&no_cache, &mut accs, &mut pending)
            .expect("usage chunk")
            .expect("usage frame");
        assert!(matches!(
            chunk,
            ProviderChunk::Usage(CanonicalUsage {
                uncached_input_tokens: 1000,
                cache_read_tokens: 0,
                output_tokens: 50,
                request_id: None,
                ..
            })
        ));
        // Hostile: cached tokens CANNOT exceed the input total they are a
        // subset of — typed Malformed, never a silent zero/saturate.
        let hostile = serde_json::json!({
            "usage": {
                "prompt_tokens": 0,
                "completion_tokens": 0,
                "prompt_tokens_details": {"cached_tokens": 7}
            }
        });
        let err = parse_chat_chunk(&hostile, &mut accs, &mut pending)
            .expect_err("cache > input total must be Malformed");
        assert_eq!(err.kind, ProviderErrorKind::Malformed);
        assert!(!err.retryable);
        // Hostile: reasoning detail exceeding the output total.
        let hostile_reasoning = serde_json::json!({
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 3,
                "completion_tokens_details": {"reasoning_tokens": 30}
            }
        });
        let err = parse_chat_chunk(&hostile_reasoning, &mut accs, &mut pending)
            .expect_err("reasoning > output must be Malformed");
        assert_eq!(err.kind, ProviderErrorKind::Malformed);
        // An all-zero envelope carries nothing: no chunk.
        let zero = serde_json::json!({"usage": {}});
        assert!(parse_chat_chunk(&zero, &mut accs, &mut pending)
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn tool_call_accumulates_and_completes() {
        let server = MockServer::new();
        let body = sse_body(&[
            serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read_file","arguments":"{\"path\":"}}]}}]}),
            serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.rs\"}"}}]}}]}),
            serde_json::json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
        ]);
        server.route(
            "POST",
            "/chat/completions",
            MockAction::Respond { status: 200, body },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::permissive_for_tests(OpenAiConfig::chat(base, None));
        let mut stream = provider.stream(req("m"));
        let mut call = None;
        while let Some(chunk) = stream.next().await {
            match chunk.unwrap() {
                ProviderChunk::ToolCall {
                    name,
                    input,
                    complete,
                    ..
                } => {
                    assert!(complete);
                    call = Some((name, input));
                }
                ProviderChunk::Done => break,
                _ => {}
            }
        }
        let (name, input) = call.expect("tool call emitted");
        assert_eq!(name, "read_file");
        assert_eq!(input["path"], "a.rs");
    }

    #[tokio::test]
    async fn rate_limit_and_auth_mapped() {
        let server = MockServer::new();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::Respond {
                status: 429,
                body: r#"{"error":{"message":"rate limited"}}"#.into(),
            },
        );
        let base = server.base_url().await;
        let provider =
            OpenAiProvider::permissive_for_tests(OpenAiConfig::chat(base, Some("k".into())));
        let mut stream = provider.stream(req("m"));
        let err = stream.next().await.unwrap().unwrap_err();
        assert_eq!(err.kind, ProviderErrorKind::RateLimited);
        assert!(err.retryable);
    }

    #[tokio::test]
    async fn malformed_sse_line_is_malformed_error() {
        let server = MockServer::new();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::Respond {
                status: 200,
                body: "data: {not json}\n\ndata: [DONE]\n\n".into(),
            },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::permissive_for_tests(OpenAiConfig::chat(base, None));
        let mut stream = provider.stream(req("m"));
        let first = stream.next().await.unwrap();
        assert!(first.is_err(), "malformed SSE must be an error");
    }

    #[tokio::test]
    async fn stream_ends_without_done_is_a_typed_failure() {
        // A body that ends without `finish_reason`/`[DONE]` is a dropped
        // connection, never a completed turn: the truncated text used to be
        // accepted as success.
        let server = MockServer::new();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::Respond {
                status: 200,
                body: "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n".into(),
            },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::permissive_for_tests(OpenAiConfig::chat(base, None));
        let mut stream = provider.stream(req("m"));
        let mut got_text = false;
        let err = loop {
            match stream.next().await.expect("an item") {
                Ok(ProviderChunk::Text { .. }) => got_text = true,
                Ok(ProviderChunk::Done) => panic!("EOF must not be Done"),
                Ok(_) => {}
                Err(e) => break e,
            }
        };
        assert!(got_text, "the delta before the drop is delivered");
        assert_eq!(err.kind, ProviderErrorKind::Malformed);
        assert!(
            err.message.contains("before finish_reason"),
            "{}",
            err.message
        );
    }

    #[tokio::test]
    async fn network_death_maps_to_network_error() {
        // No server listening on this port: connect error.
        let provider =
            OpenAiProvider::permissive_for_tests(OpenAiConfig::chat("http://127.0.0.1:1", None));
        let mut stream = provider.stream(req("m"));
        let first = stream.next().await.unwrap();
        assert!(first.is_err());
        assert_eq!(first.unwrap_err().kind, ProviderErrorKind::Network);
    }

    #[test]
    fn declared_models_are_served_with_generic_capabilities() {
        let cfg = OpenAiConfig::chat("http://x", None)
            .with_models(vec!["fault-429".to_string(), "toolcall".to_string()]);
        let provider = OpenAiProvider::permissive_for_tests(cfg);
        let models = provider.known_models();
        assert!(models.contains(&"fault-429".to_string()), "{models:?}");
        assert!(models.contains(&"toolcall".to_string()), "{models:?}");
        let caps = provider.capabilities("fault-429");
        assert!(caps.streaming && caps.tools, "{caps:?}");
        assert!(caps.context >= 128_000, "{caps:?}");
        // An unlisted id still falls back to the same generic set.
        assert_eq!(provider.capabilities("unlisted").context, caps.context);
    }

    #[test]
    fn capabilities_default_and_override() {
        let p = OpenAiProvider::permissive_for_tests(OpenAiConfig::chat("http://x", None));
        let caps = p.capabilities("unknown-model");
        assert!(caps.tools);
        assert_eq!(caps.context, 128_000);
        let p =
            OpenAiProvider::permissive_for_tests(OpenAiConfig::chat("http://x", None).with_model(
                "small",
                ModelCapabilities {
                    context: 8192,
                    tools: false,
                    ..Default::default()
                },
            ));
        assert!(!p.capabilities("small").tools);
        assert_eq!(p.capabilities("small").context, 8192);
    }

    #[tokio::test]
    async fn responses_body_is_native_items_not_chat_shape() {
        // The Responses codec lowers to the model's OWN item protocol:
        // system -> top-level instructions; text/images -> input_text /
        // input_image; assistant tool calls -> top-level function_call
        // items; results -> function_call_output keyed by call_id; tools ->
        // the flattened Responses tool shape. A chat-shaped body (messages /
        // nested function wrapper / assistant tool_calls) fails these
        // assertions.
        let server = MockServer::new();
        let asserted = Arc::new(std::sync::Mutex::new(None::<serde_json::Value>));
        {
            let asserted = asserted.clone();
            server.route(
                "POST",
                "/responses",
                MockAction::AssertThenRespond {
                    status: 200,
                    body: "{}".into(),
                    assert: Arc::new(move |body| {
                        *asserted.lock().unwrap() = Some(body.clone());
                    }),
                },
            );
        }
        let base = server.base_url().await;
        let provider = OpenAiProvider::permissive_for_tests(OpenAiConfig::responses(base, None));
        let mut g = req("m");
        g.reasoning = Some(faktor_core::model::ReasoningMode::Medium);
        g.messages = vec![
            RequestMessage {
                role: Role::User,
                content: vec![
                    ContentPart::text("list src/"),
                    ContentPart {
                        kind: ContentKind::Image {
                            url: "https://example.test/a.png".into(),
                        },
                        tool_call_id: None,
                    },
                ],
            },
            RequestMessage {
                role: Role::Assistant,
                content: vec![
                    ContentPart::text("calling"),
                    ContentPart::tool_call(
                        "call_1",
                        "read_file",
                        serde_json::json!({"path": "src/a.rs"}),
                    ),
                ],
            },
            RequestMessage {
                role: Role::User,
                content: vec![ContentPart::tool_result("fn a() {}", false, "call_1")],
            },
        ];
        let mut stream = provider.stream(g);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), stream.next()).await;
        let body = asserted.lock().unwrap().clone().expect("request asserted");
        assert_eq!(body["model"], "m");
        assert_eq!(body["stream"], true);
        // The cacheable prefix rides top-level `instructions`, never an
        // input message.
        assert_eq!(body["instructions"], "sys");
        // Native tools: flattened function rows, never chat nested ones.
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["tools"][0]["parameters"]["type"], "object");
        assert!(
            body["tools"][0].get("function").is_none(),
            "tools must not use the chat wrapper: {}",
            body["tools"][0]
        );
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["max_output_tokens"], 1000);
        assert_eq!(body["reasoning"]["effort"], "medium");
        let input = body["input"].as_array().unwrap();
        assert!(
            input.iter().any(|i| i["role"] == "user"
                && i["content"][0]["type"] == "input_text"
                && i["content"][0]["text"] == "list src/"
                && i["content"][1]["type"] == "input_image"
                && i["content"][1]["image_url"] == "https://example.test/a.png"),
            "user text+images lower to native input parts: {input:?}"
        );
        let assistant = input
            .iter()
            .find(|i| i["role"] == "assistant")
            .expect("assistant text item present");
        assert_eq!(assistant["content"][0]["type"], "output_text");
        assert_eq!(assistant["content"][0]["text"], "calling");
        assert!(
            assistant.get("tool_calls").is_none(),
            "function calls are top-level items, never assistant tool_calls"
        );
        let call = input
            .iter()
            .find(|i| i["type"] == "function_call")
            .expect("top-level function_call item present");
        assert_eq!(call["call_id"], "call_1");
        assert_eq!(call["name"], "read_file");
        let args: serde_json::Value =
            serde_json::from_str(call["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["path"], "src/a.rs");
        assert!(
            input.iter().any(|i| i["type"] == "function_call_output"
                && i["call_id"] == "call_1"
                && i["output"] == "fn a() {}"),
            "tool results lower to function_call_output: {input:?}"
        );
        // Nothing chat-shaped may appear anywhere in the body.
        let rendered = serde_json::to_string(&body).unwrap();
        for banned in ["tool_result", "tool_calls", "\"function\":{", "max_tokens"] {
            assert!(!rendered.contains(banned), "{banned} leaked: {rendered}");
        }
        // Internal request metadata never leaks either.
        for leaked in ["operation_id", "session_id", "deadline_ms", "cancellation"] {
            assert!(!rendered.contains(leaked), "{leaked} leaked: {rendered}");
        }
    }

    /// One `data: <json>` SSE frame.
    fn ev(v: serde_json::Value) -> String {
        format!("data: {v}\n\n")
    }

    fn responses_provider(base: String) -> Arc<dyn Provider> {
        OpenAiProvider::permissive_for_tests(OpenAiConfig::responses(base, None))
    }

    /// Drain a stream to its full item sequence (errors preserved).
    async fn drain(mut stream: ProviderStream) -> Vec<Result<ProviderChunk, ProviderError>> {
        let mut out = Vec::new();
        while let Some(item) = stream.next().await {
            out.push(item);
        }
        out
    }

    /// A raw HTTP/1.1 server that answers the first request with SSE chunked
    /// headers, writes `chunks` (each its own HTTP chunk), then either stalls
    /// forever with the body open (`stall = true`) or terminates the chunked
    /// body. Lets cancellation/idle tests run against a genuinely LIVE
    /// stream, not a completed one.
    async fn slow_sse_server(chunks: Vec<Vec<u8>>, stall: bool) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut head = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let Ok(n) = socket.read(&mut buf).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                head.extend_from_slice(&buf[..n]);
                if head.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
                )
                .await;
            for chunk in &chunks {
                let _ = socket
                    .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                    .await;
                let _ = socket.write_all(chunk).await;
                let _ = socket.write_all(b"\r\n").await;
                let _ = socket.flush().await;
            }
            if stall {
                let mut sink = [0u8; 1024];
                while let Ok(n) = socket.read(&mut sink).await {
                    if n == 0 {
                        break;
                    }
                }
            } else {
                let _ = socket.write_all(b"0\r\n\r\n").await;
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn responses_stream_parses_text_reasoning_and_tool_calls() {
        // Native events: both reasoning delta variants, text deltas, a
        // function call whose ITEM id differs from its wire call_id and
        // whose arguments arrive fragmented, an authoritative item-done,
        // completed with usage. Ordered chunks, one assembled call, usage
        // LAST, exactly one Done.
        let server = MockServer::new();
        server.route(
            "POST",
            "/responses",
            MockAction::Sse {
                status: 200,
                events: vec![
                    ev(serde_json::json!({"type": "response.created", "response": {"id": "r1"}})),
                    ev(serde_json::json!({"type": "response.reasoning_text.delta", "item_id": "rs_1", "delta": "thinking "})),
                    ev(serde_json::json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "delta": "hard"})),
                    // The real wire frames named events (`event:` line then
                    // `data:`); the named line must be skipped, never
                    // mistaken for data.
                    "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"delta\":\"hello \"}\n\n".to_string(),
                    ev(serde_json::json!({"type": "response.output_text.delta", "item_id": "msg_1", "delta": "world"})),
                    ev(serde_json::json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "function_call", "id": "fc_item_1", "call_id": "call_1", "name": "read_file", "arguments": ""}})),
                    ev(serde_json::json!({"type": "response.function_call_arguments.delta", "item_id": "fc_item_1", "delta": "{\"path\":"})),
                    ev(serde_json::json!({"type": "response.function_call_arguments.delta", "item_id": "fc_item_1", "delta": "\"a.rs\"}"})),
                    ev(serde_json::json!({"type": "response.output_item.done", "item": {"type": "function_call", "id": "fc_item_1", "call_id": "call_1", "name": "read_file", "arguments": "{\"path\":\"a.rs\"}"}})),
                    ev(serde_json::json!({"type": "response.completed", "response": {"id": "r1", "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}}})),
                    "data: [DONE]\n\n".to_string(),
                ],
            },
        );
        let base = server.base_url().await;
        let items = drain(responses_provider(base).stream(req("m"))).await;
        let text: String = items
            .iter()
            .filter_map(|i| match i {
                Ok(ProviderChunk::Text { text }) => Some(text.clone()),
                _ => None,
            })
            .collect();
        let reasoning: String = items
            .iter()
            .filter_map(|i| match i {
                Ok(ProviderChunk::Reasoning { text }) => Some(text.clone()),
                _ => None,
            })
            .collect();
        let calls: Vec<&ProviderChunk> = items
            .iter()
            .filter_map(|i| match i {
                Ok(c @ ProviderChunk::ToolCall { .. }) => Some(c),
                _ => None,
            })
            .collect();
        assert_eq!(text, "hello world");
        assert_eq!(reasoning, "thinking hard");
        assert_eq!(calls.len(), 1, "one assembled call: {items:?}");
        match calls[0] {
            ProviderChunk::ToolCall {
                id,
                name,
                input,
                complete,
            } => {
                assert_eq!(id, "call_1", "the generic id is the wire call_id");
                assert_eq!(name, "read_file");
                assert_eq!(input, &serde_json::json!({"path": "a.rs"}));
                assert!(*complete);
            }
            _ => unreachable!(),
        }
        // Usage: 10 total input / 0 cached -> uncached 10; request id kept.
        let usage_at = items
            .iter()
            .position(|i| matches!(i, Ok(ProviderChunk::Usage(_))))
            .expect("usage frame");
        assert_eq!(
            items[usage_at],
            Ok(ProviderChunk::Usage(CanonicalUsage {
                uncached_input_tokens: 10,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: 5,
                reasoning_tokens: 0,
                reported_cost: None,
                request_id: Some("r1".into()),
            }))
        );
        // Terminal semantics: exactly one Done, last, usage immediately
        // before it, nothing after.
        assert_eq!(
            items
                .iter()
                .filter(|i| matches!(i, Ok(ProviderChunk::Done)))
                .count(),
            1
        );
        assert_eq!(items.last(), Some(&Ok(ProviderChunk::Done)));
        assert_eq!(usage_at + 1, items.len() - 1);
        assert!(items[..usage_at].iter().all(|i| i.is_ok()));
    }

    #[tokio::test]
    async fn responses_stream_assembles_multiple_calls_with_byte_split_arguments() {
        // TWO simultaneous function calls with interleaved fragments, the
        // whole SSE body delivered in 7-byte HTTP chunks (splits fall
        // mid-line, mid-JSON and mid-string). Item ids differ from call
        // ids; call B's fragments are overwritten by an authoritative
        // `function_call_arguments.done`; call A completes from fragments +
        // `output_item.done`. Both calls must assemble independently.
        let frame = |v: serde_json::Value| format!("data: {v}\n\n");
        let frames: Vec<String> = vec![
            frame(
                serde_json::json!({"type": "response.output_item.added", "item": {"type": "function_call", "id": "fc_a", "call_id": "call_a", "name": "read_file", "arguments": ""}}),
            ),
            frame(
                serde_json::json!({"type": "response.output_item.added", "item": {"type": "function_call", "id": "fc_b", "call_id": "call_b", "name": "list_dir", "arguments": ""}}),
            ),
            frame(
                serde_json::json!({"type": "response.function_call_arguments.delta", "item_id": "fc_a", "delta": "{\"path\":"}),
            ),
            frame(
                serde_json::json!({"type": "response.function_call_arguments.delta", "item_id": "fc_b", "delta": "{\"depth\":"}),
            ),
            frame(
                serde_json::json!({"type": "response.function_call_arguments.delta", "item_id": "fc_a", "delta": "\"src/"}),
            ),
            frame(
                serde_json::json!({"type": "response.function_call_arguments.delta", "item_id": "fc_b", "delta": "9}"}),
            ),
            frame(
                serde_json::json!({"type": "response.function_call_arguments.done", "item_id": "fc_b", "arguments": "{\"depth\":2}"}),
            ),
            frame(
                serde_json::json!({"type": "response.function_call_arguments.delta", "item_id": "fc_a", "delta": "a.rs\"}"}),
            ),
            frame(
                serde_json::json!({"type": "response.output_item.done", "item": {"type": "function_call", "id": "fc_a", "call_id": "call_a", "name": "read_file", "arguments": "{\"path\":\"src/a.rs\"}"}}),
            ),
            frame(serde_json::json!({"type": "response.completed", "response": {"id": "r2"}})),
            "data: [DONE]\n\n".to_string(),
        ];
        let full = frames.concat().into_bytes();
        let chunks: Vec<Vec<u8>> = full.chunks(7).map(|c| c.to_vec()).collect();
        let server = MockServer::new();
        server.route(
            "POST",
            "/responses",
            MockAction::ChunkedSse {
                status: 200,
                chunks,
            },
        );
        let base = server.base_url().await;
        let items = drain(responses_provider(base).stream(req("m"))).await;
        let calls: Vec<&ProviderChunk> = items
            .iter()
            .filter_map(|i| match i {
                Ok(c @ ProviderChunk::ToolCall { .. }) => Some(c),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 2, "both calls assemble: {items:?}");
        match calls[0] {
            ProviderChunk::ToolCall {
                id, name, input, ..
            } => {
                assert_eq!(id, "call_a");
                assert_eq!(name, "read_file");
                assert_eq!(input, &serde_json::json!({"path": "src/a.rs"}));
            }
            _ => unreachable!(),
        }
        match calls[1] {
            ProviderChunk::ToolCall {
                id, name, input, ..
            } => {
                assert_eq!(id, "call_b");
                assert_eq!(name, "list_dir");
                assert_eq!(
                    input,
                    &serde_json::json!({"depth": 2}),
                    "the authoritative done event replaces fragments"
                );
            }
            _ => unreachable!(),
        }
        assert_eq!(
            items
                .iter()
                .filter(|i| matches!(i, Ok(ProviderChunk::Done)))
                .count(),
            1
        );
        assert_eq!(items.last(), Some(&Ok(ProviderChunk::Done)));
    }

    #[tokio::test]
    async fn responses_usage_flushes_after_tool_calls_and_before_done() {
        // A completed event carrying BOTH an unflushed function call and a
        // usage frame: the call comes first, usage is the LAST chunk before
        // the single Done.
        let server = MockServer::new();
        server.route(
            "POST",
            "/responses",
            MockAction::Sse {
                status: 200,
                events: vec![
                    ev(serde_json::json!({"type": "response.output_item.added", "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "echo", "arguments": "{\"x\":1}"}})),
                    ev(serde_json::json!({"type": "response.completed", "response": {"id": "r1", "usage": {"input_tokens": 7, "input_tokens_details": {"cached_tokens": 2}, "output_tokens": 3, "output_tokens_details": {"reasoning_tokens": 1}}}})),
                ],
            },
        );
        let base = server.base_url().await;
        let items = drain(responses_provider(base).stream(req("m"))).await;
        assert_eq!(items.len(), 3, "{items:?}");
        assert!(matches!(items[0], Ok(ProviderChunk::ToolCall { .. })));
        assert_eq!(
            items[1],
            Ok(ProviderChunk::Usage(CanonicalUsage {
                uncached_input_tokens: 5,
                cache_read_tokens: 2,
                cache_write_tokens: 0,
                output_tokens: 3,
                reasoning_tokens: 1,
                reported_cost: None,
                request_id: Some("r1".into()),
            }))
        );
        assert_eq!(items[2], Ok(ProviderChunk::Done));
    }

    #[tokio::test]
    async fn responses_malformed_frame_is_typed_malformed_and_terminal() {
        // A data line that is not JSON is a broken stream: exactly one typed
        // Malformed error, no chunks after it.
        let server = MockServer::new();
        server.route(
            "POST",
            "/responses",
            MockAction::Respond {
                status: 200,
                body: "data: {not json}\n\ndata: [DONE]\n\n".into(),
            },
        );
        let base = server.base_url().await;
        let items = drain(responses_provider(base).stream(req("m"))).await;
        assert_eq!(items.len(), 1, "{items:?}");
        let err = items[0].as_ref().expect_err("malformed");
        assert_eq!(err.kind, ProviderErrorKind::Malformed);
        assert!(!err.retryable);
    }

    #[tokio::test]
    async fn responses_unknown_item_fragment_is_dropped_never_injects() {
        // A fragment for a function call the server never announced is
        // DROPPED (event-order processing): no call is fabricated and no
        // stored call is injected; the announced call still completes.
        let server = MockServer::new();
        server.route(
            "POST",
            "/responses",
            MockAction::Sse {
                status: 200,
                events: vec![
                    ev(serde_json::json!({"type": "response.output_item.added", "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "echo", "arguments": ""}})),
                    ev(serde_json::json!({"type": "response.function_call_arguments.delta", "item_id": "fc_ghost", "delta": "{\"evil\":1}"})),
                    ev(serde_json::json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "delta": "{\"x\":1}"})),
                    ev(serde_json::json!({"type": "response.completed", "response": {"id": "r1"}})),
                ],
            },
        );
        let base = server.base_url().await;
        let items = drain(responses_provider(base).stream(req("m"))).await;
        assert_eq!(items.len(), 2, "one call then Done: {items:?}");
        assert_eq!(
            items[0],
            Ok(ProviderChunk::ToolCall {
                id: "call_1".into(),
                name: "echo".into(),
                input: serde_json::json!({"x": 1}),
                complete: true,
            })
        );
        assert_eq!(items[1], Ok(ProviderChunk::Done));
    }

    #[tokio::test]
    async fn responses_error_events_are_typed_by_structured_code() {
        // SSE `error` events carry structured codes: auth is terminal, rate
        // limits stay retryable, unknown codes are terminal BadRequest. The
        // stream ends at the error: later deltas never surface.
        for (code, expect_kind, expect_retryable) in [
            ("invalid_api_key", ProviderErrorKind::Auth, false),
            ("rate_limit_exceeded", ProviderErrorKind::RateLimited, true),
            ("server_error", ProviderErrorKind::BadRequest, false),
        ] {
            let server = MockServer::new();
            server.route(
                "POST",
                "/responses",
                MockAction::Sse {
                    status: 200,
                    events: vec![
                        ev(serde_json::json!({"type": "response.output_text.delta", "item_id": "m1", "delta": "partial"})),
                        ev(serde_json::json!({"type": "error", "code": code, "message": "boom"})),
                        ev(serde_json::json!({"type": "response.output_text.delta", "item_id": "m1", "delta": "after"})),
                    ],
                },
            );
            let base = server.base_url().await;
            let items = drain(responses_provider(base).stream(req("m"))).await;
            assert_eq!(items.len(), 2, "{code}: {items:?}");
            assert!(matches!(items[0], Ok(ProviderChunk::Text { .. })));
            let err = items[1].as_ref().expect_err("typed error");
            assert_eq!(err.kind, expect_kind, "{code}");
            assert_eq!(err.retryable, expect_retryable, "{code}");
            assert_eq!(err.code.as_deref(), Some(code));
        }
    }

    #[tokio::test]
    async fn responses_no_chunks_survive_any_terminal_event() {
        // completion is a hard terminal: later text/reasoning deltas and a
        // trailing [DONE] never produce chunks.
        let server = MockServer::new();
        server.route(
            "POST",
            "/responses",
            MockAction::Sse {
                status: 200,
                events: vec![
                    ev(serde_json::json!({"type": "response.output_text.delta", "item_id": "m1", "delta": "before"})),
                    ev(serde_json::json!({"type": "response.completed", "response": {"id": "r1"}})),
                    ev(serde_json::json!({"type": "response.output_text.delta", "item_id": "m1", "delta": "after"})),
                    ev(serde_json::json!({"type": "response.reasoning_text.delta", "item_id": "rs", "delta": "after"})),
                    "data: [DONE]\n\n".to_string(),
                ],
            },
        );
        let base = server.base_url().await;
        let items = drain(responses_provider(base).stream(req("m"))).await;
        assert_eq!(
            items,
            vec![
                Ok(ProviderChunk::Text {
                    text: "before".into()
                }),
                Ok(ProviderChunk::Done)
            ]
        );
    }

    #[tokio::test]
    async fn responses_cancellation_mid_stream_is_cancelled_and_terminal() {
        // The server sends one real delta and then holds the stream open.
        // Cancelling mid-stream must surface one typed Cancelled error
        // promptly, then end the stream.
        let base = slow_sse_server(
            vec![ev(serde_json::json!({"type": "response.output_text.delta", "item_id": "m1", "delta": "partial"})).into_bytes()],
            true,
        )
        .await;
        let cancel = CancellationToken::new();
        let mut g = req("m");
        g.meta.cancellation = cancel.clone();
        let mut stream = responses_provider(base).stream(g);
        match stream.next().await {
            Some(Ok(ProviderChunk::Text { text })) => assert_eq!(text, "partial"),
            other => panic!("expected the first delta, got {other:?}"),
        }
        let t0 = std::time::Instant::now();
        cancel.cancel();
        let item = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("cancellation must wake the live stream")
            .expect("an error item");
        let err = item.expect_err("cancelled");
        assert_eq!(err.kind, ProviderErrorKind::Cancelled);
        assert!(!err.retryable);
        assert!(
            t0.elapsed() < std::time::Duration::from_millis(1500),
            "cancel must surface promptly: {:?}",
            t0.elapsed()
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), stream.next())
                .await
                .expect("terminal error ends the stream")
                .is_none()
        );
    }

    #[tokio::test]
    async fn responses_request_meta_deadline_bounds_silent_stream() {
        // `RequestMeta::deadline_ms` is the operation deadline: a silent
        // server must time out at the overall bound (retryable Timeout),
        // never wait out the 60s first-byte default, and end the stream.
        let server = MockServer::new();
        server.route("POST", "/responses", MockAction::Silent { status: 200 });
        let base = server.base_url().await;
        let mut g = req("m");
        g.meta.deadline_ms = 1200;
        let t0 = std::time::Instant::now();
        let mut stream = responses_provider(base).stream(g);
        let item = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("the meta deadline must terminate the silent stream")
            .expect("an error item");
        let err = item.expect_err("must be a timeout");
        assert_eq!(err.kind, ProviderErrorKind::Timeout);
        assert!(err.retryable);
        assert!(
            t0.elapsed() < std::time::Duration::from_millis(2500),
            "meta deadline must fire at its overall bound: {:?}",
            t0.elapsed()
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), stream.next())
                .await
                .expect("stream must end after the terminal timeout")
                .is_none()
        );
    }

    #[tokio::test]
    async fn responses_first_byte_and_idle_deadlines_fire() {
        // First byte: connected + silent -> Timeout before any item.
        let server = MockServer::new();
        server.route("POST", "/responses", MockAction::Silent { status: 200 });
        let base = server.base_url().await;
        let transport: Arc<dyn HttpTransport> = Arc::new(PolicyCheckedHttpTransport::permissive());
        let mut stream = Box::pin(responses_stream(
            transport,
            format!("{base}/responses"),
            authorization_headers(None).unwrap(),
            responses_body(&req("m")),
            StreamDeadlines {
                first_byte_ms: 300,
                idle_ms: 3000,
                overall_ms: 0,
            },
            None,
        ));
        let item = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("first-byte bound must fire")
            .expect("an error item");
        let err = item.expect_err("timeout");
        assert_eq!(err.kind, ProviderErrorKind::Timeout);
        assert!(err.retryable);

        // Idle: one delta arrives, then silence -> the idle bound fires.
        let base = slow_sse_server(
            vec![ev(serde_json::json!({"type": "response.output_text.delta", "item_id": "m1", "delta": "tick"})).into_bytes()],
            true,
        )
        .await;
        let transport: Arc<dyn HttpTransport> = Arc::new(PolicyCheckedHttpTransport::permissive());
        let mut stream = Box::pin(responses_stream(
            transport,
            format!("{base}/responses"),
            authorization_headers(None).unwrap(),
            responses_body(&req("m")),
            StreamDeadlines {
                first_byte_ms: 5000,
                idle_ms: 300,
                overall_ms: 0,
            },
            None,
        ));
        match stream.next().await {
            Some(Ok(ProviderChunk::Text { text })) => assert_eq!(text, "tick"),
            other => panic!("expected the first delta, got {other:?}"),
        }
        let t0 = std::time::Instant::now();
        let item = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("idle bound must fire")
            .expect("an error item");
        let err = item.expect_err("idle timeout");
        assert_eq!(err.kind, ProviderErrorKind::Timeout);
        assert!(err.retryable);
        assert!(
            t0.elapsed() < std::time::Duration::from_millis(1500),
            "idle bound must fire promptly: {:?}",
            t0.elapsed()
        );
    }

    #[tokio::test]
    async fn http_status_retry_classification_is_shared_by_both_families() {
        // ONE status classifier serves both wire families: 429/5xx are
        // retryable, auth and every other 4xx are terminal, structured body
        // hints override the bare 400/403 statuses, and the envelope's
        // retryability is the provider crate's shared
        // `ProviderErrorKind::retryable()`.
        let bare = r#"{"error":{"message":"nope"}}"#;
        let bad_key = r#"{"error":{"code":"invalid_api_key","message":"bad"}}"#;
        let quota = r#"{"error":{"status":"RESOURCE_EXHAUSTED","message":"quota"}}"#;
        let denied = r#"{"error":{"status":"PERMISSION_DENIED","message":"nope"}}"#;
        for (status, body, expect_kind, expect_retryable) in [
            (429u16, bare, ProviderErrorKind::RateLimited, true),
            (500, bare, ProviderErrorKind::Server, true),
            (503, bare, ProviderErrorKind::Server, true),
            (400, bare, ProviderErrorKind::Malformed, false),
            (401, bare, ProviderErrorKind::Auth, false),
            (403, bare, ProviderErrorKind::Auth, false),
            (404, bare, ProviderErrorKind::BadRequest, false),
            // Structured body hints override the bare status.
            (400, bad_key, ProviderErrorKind::Auth, false),
            (400, quota, ProviderErrorKind::RateLimited, true),
            (403, quota, ProviderErrorKind::RateLimited, true),
            (403, denied, ProviderErrorKind::Auth, false),
        ] {
            for responses in [false, true] {
                let server = MockServer::new();
                let path = if responses {
                    "/responses"
                } else {
                    "/chat/completions"
                };
                server.route(
                    "POST",
                    path,
                    MockAction::Respond {
                        status,
                        body: body.into(),
                    },
                );
                let base = server.base_url().await;
                let provider: Arc<dyn Provider> = if responses {
                    OpenAiProvider::permissive_for_tests(OpenAiConfig::responses(base, None))
                } else {
                    OpenAiProvider::permissive_for_tests(OpenAiConfig::chat(base, None))
                };
                let mut stream = provider.stream(req("m"));
                let err = stream.next().await.unwrap().unwrap_err();
                assert_eq!(
                    err.kind, expect_kind,
                    "status {status} responses={responses}"
                );
                assert_eq!(
                    err.retryable, expect_retryable,
                    "status {status} responses={responses}"
                );
                assert_eq!(err.retryable, err.kind.retryable());
                assert_eq!(err.code.as_deref(), Some(status.to_string().as_str()));
            }
        }
    }

    #[tokio::test]
    async fn openai_family_dispatch_selects_body_and_endpoint() {
        // wire_body dispatch: Responses -> native item body, Chat -> chat
        // body; never both shapes.
        let responses = OpenAiProvider {
            config: OpenAiConfig::responses("http://x", None),
            transport: Arc::new(PolicyCheckedHttpTransport::permissive()),
            quirks: OpenAiQuirks::default(),
        };
        let chat = OpenAiProvider {
            config: OpenAiConfig::chat("http://x", None),
            transport: Arc::new(PolicyCheckedHttpTransport::permissive()),
            quirks: OpenAiQuirks::default(),
        };
        let r = responses.wire_body(&req("m"));
        assert!(
            r.get("input").is_some() && r.get("messages").is_none(),
            "responses body: {r}"
        );
        assert_eq!(r["stream"], true);
        let c = chat.wire_body(&req("m"));
        assert!(
            c.get("messages").is_some() && c.get("input").is_none(),
            "chat body: {c}"
        );
        assert_eq!(c["stream"], true);

        // stream() dispatch: the family picks the endpoint + parser pair
        // (chat keeps its own URL, locked here for the pair).
        for (responses, expected_path) in [
            (true, "http://mock.invalid/responses"),
            (false, "http://mock.invalid/chat/completions"),
        ] {
            let mock = Arc::new(MockHttpTransport::new(200, "data: [DONE]\n\n"));
            let transport: Arc<dyn HttpTransport> = mock.clone();
            let config = if responses {
                OpenAiConfig::responses("http://mock.invalid", None)
            } else {
                OpenAiConfig::chat("http://mock.invalid", None)
            };
            let provider = OpenAiProvider::build(config, transport);
            let items = drain(provider.stream(req("m"))).await;
            assert_eq!(items, vec![Ok(ProviderChunk::Done)]);
            assert_eq!(
                mock.requests(),
                vec![("POST".to_string(), expected_path.to_string())],
                "responses={responses}"
            );
        }
    }
    #[tokio::test]
    async fn assistant_tool_call_and_tool_result_lower_to_wire_messages() {
        // (a) The exact request shape after a tool runs: the assistant turn
        // carries `tool_calls` (NOT {type:"tool_call"} content blocks) and
        // the result is a separate role:"tool" message (NOT a
        // {type:"tool_result"} block inside a user message).
        let server = MockServer::new();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::AssertThenRespond {
                status: 200,
                body: String::new(),
                assert: Arc::new(|body: &serde_json::Value| {
                    let raw = serde_json::to_string(body).unwrap();
                    for banned in ["\"type\":\"tool_call\"", "\"type\":\"tool_result\""] {
                        assert!(
                            !raw.contains(banned),
                            "lowered body must not contain {banned}: {raw}"
                        );
                    }
                    let msgs = body["messages"].as_array().expect("messages array");
                    assert_eq!(msgs.len(), 3, "system + assistant + tool message");
                    assert_eq!(msgs[0]["role"], "system");
                    assert_eq!(msgs[1]["role"], "assistant");
                    // content is text-only; the call rides tool_calls.
                    assert_eq!(msgs[1]["content"].as_array().unwrap().len(), 1);
                    assert_eq!(msgs[1]["content"][0]["type"], "text");
                    let tc = &msgs[1]["tool_calls"][0];
                    assert_eq!(tc["id"], "call_1");
                    assert_eq!(tc["type"], "function");
                    assert_eq!(tc["function"]["name"], "echo");
                    assert_eq!(
                        tc["function"]["arguments"], r#"{"x":1}"#,
                        "arguments must be the JSON STRING of the object"
                    );
                    assert_eq!(msgs[2]["role"], "tool");
                    assert_eq!(msgs[2]["tool_call_id"], "call_1");
                    assert_eq!(msgs[2]["content"], "echo: {\"x\":1}");
                    assert!(
                        msgs[2].get("tool_calls").is_none(),
                        "tool messages never carry tool_calls"
                    );
                }),
            },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::permissive_for_tests(OpenAiConfig::chat(base, None));
        let mut r = req("m");
        r.messages = vec![
            RequestMessage {
                role: Role::Assistant,
                content: vec![
                    ContentPart::text("calling now"),
                    ContentPart::tool_call("call_1", "echo", serde_json::json!({"x": 1})),
                ],
            },
            RequestMessage {
                role: Role::User,
                content: vec![ContentPart::tool_result("echo: {\"x\":1}", false, "call_1")],
            },
        ];
        let mut stream = provider.stream(r);
        // The mocked response body is EMPTY: the lowering assertions above
        // are the point of this test. An empty body has no terminal SSE
        // marker, so the stream must end in the typed dropped-connection
        // failure — never a fabricated Done.
        let err = stream
            .next()
            .await
            .expect("an item")
            .expect_err("an empty body is not a completion");
        assert_eq!(err.kind, ProviderErrorKind::Malformed, "{err:?}");
    }

    #[tokio::test]
    async fn parallel_tool_calls_lower_to_two_tool_calls_entries() {
        // (b) One assistant turn with two parallel calls → two tool_calls
        // entries with distinct ids and stringified JSON arguments; content
        // stays text-only.
        let server = MockServer::new();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::AssertThenRespond {
                status: 200,
                body: String::new(),
                assert: Arc::new(|body: &serde_json::Value| {
                    let raw = serde_json::to_string(body).unwrap();
                    for banned in ["\"type\":\"tool_call\"", "\"type\":\"tool_result\""] {
                        assert!(!raw.contains(banned), "no tool blocks allowed: {raw}");
                    }
                    let msgs = body["messages"].as_array().unwrap();
                    assert_eq!(msgs.len(), 2, "system + assistant message");
                    assert_eq!(msgs[0]["role"], "system");
                    let calls = msgs[1]["tool_calls"].as_array().unwrap();
                    assert_eq!(calls.len(), 2);
                    let ids: Vec<&str> = calls.iter().map(|c| c["id"].as_str().unwrap()).collect();
                    assert_eq!(ids, vec!["call_a", "call_b"], "ids must stay distinct");
                    assert_eq!(calls[0]["function"]["name"], "read_file");
                    assert_eq!(calls[0]["function"]["arguments"], r#"{"path":"a.rs"}"#);
                    assert_eq!(calls[1]["function"]["name"], "list_dir");
                    assert_eq!(
                        calls[1]["function"]["arguments"],
                        r#"{"path":"src","depth":2}"#
                    );
                    let content = msgs[1]["content"].as_array().unwrap();
                    assert_eq!(content.len(), 1, "text-only content");
                    assert_eq!(content[0]["type"], "text");
                }),
            },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::permissive_for_tests(OpenAiConfig::chat(base, None));
        let mut r = req("m");
        r.messages = vec![RequestMessage {
            role: Role::Assistant,
            content: vec![
                ContentPart::text("two calls"),
                ContentPart::tool_call("call_a", "read_file", serde_json::json!({"path": "a.rs"})),
                ContentPart::tool_call(
                    "call_b",
                    "list_dir",
                    serde_json::json!({"path": "src", "depth": 2}),
                ),
            ],
        }];
        let mut stream = provider.stream(r);
        while let Some(chunk) = stream.next().await {
            if let Ok(ProviderChunk::Done) = chunk {
                break;
            }
        }
    }

    #[tokio::test]
    async fn two_index_keyed_tool_calls_accumulate_independently() {
        // (c) Two SIMULTANEOUS tool calls whose fragments interleave across
        // frames: each index accumulates its own arguments and both complete
        // at the finishing marker, in index order.
        let server = MockServer::new();
        let body = sse_body(&[
            serde_json::json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"c1","function":{"name":"read_file","arguments":"{\"path\":"}},
                {"index":1,"id":"c2","function":{"name":"sum","arguments":"{\"nums\":"}}
            ]}}]}),
            serde_json::json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":"\"a.rs\""}},
                {"index":1,"function":{"arguments":"[1,2]"}}
            ]}}]}),
            serde_json::json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":"}"}},
                {"index":1,"function":{"arguments":"}"}}
            ]}}]}),
            serde_json::json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
        ]);
        server.route(
            "POST",
            "/chat/completions",
            MockAction::Respond { status: 200, body },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::permissive_for_tests(OpenAiConfig::chat(base, None));
        let mut stream = provider.stream(req("m"));
        let mut calls: Vec<(String, serde_json::Value, bool)> = Vec::new();
        while let Some(chunk) = stream.next().await {
            match chunk.unwrap() {
                ProviderChunk::ToolCall {
                    id,
                    name: _,
                    input,
                    complete,
                } => calls.push((id, input, complete)),
                ProviderChunk::Done => break,
                _ => {}
            }
        }
        assert_eq!(calls.len(), 2, "both calls must complete: {calls:?}");
        assert!(
            calls.iter().all(|(_, _, complete)| *complete),
            "chunks appear only at the finishing marker, marked complete"
        );
        assert_eq!(calls[0].0, "c1", "index 0 flushes first");
        assert_eq!(calls[0].1["path"], "a.rs");
        assert_eq!(calls[1].0, "c2");
        assert_eq!(calls[1].1["nums"], serde_json::json!([1, 2]));
    }

    #[tokio::test]
    async fn sse_frame_split_across_http_chunks_assembles() {
        // Adversarial transport: a well-behaved server whose frame is
        // fragmented MID-LINE by HTTP chunking. The old per-chunk
        // `.lines()` code corrupted this into garbage lines.
        let server = MockServer::new();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::ChunkedSse {
                status: 200,
                chunks: vec![
                    b"data: {\"choices\":[{\"delta\":{\"content\":\"par".to_vec(),
                    b"tial reply\"}}]}".to_vec(),
                    b"\n\n".to_vec(),
                    b"data: [DONE]\n\n".to_vec(),
                ],
            },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::permissive_for_tests(OpenAiConfig::chat(base, None));
        let mut stream = provider.stream(req("gpt-x"));
        let mut text = String::new();
        let mut done = false;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(ProviderChunk::Text { text: t }) => text.push_str(&t),
                Ok(ProviderChunk::Done) => {
                    done = true;
                    break;
                }
                Err(e) => panic!("fragmented SSE must assemble, got {e:?}"),
                _ => {}
            }
        }
        assert!(done);
        assert_eq!(text, "partial reply");
    }

    #[tokio::test]
    async fn multibyte_rune_split_across_http_chunks_assembles() {
        // Split "héllo" between the two bytes of é across HTTP chunks.
        let server = MockServer::new();
        let e = "é".as_bytes();
        let mut c1 = b"data: {\"choices\":[{\"delta\":{\"content\":\"h".to_vec();
        c1.push(e[0]);
        let mut c2 = vec![e[1]];
        c2.extend_from_slice(b"llo\"}}]}");
        server.route(
            "POST",
            "/chat/completions",
            MockAction::ChunkedSse {
                status: 200,
                chunks: vec![c1, c2, b"\n\n".to_vec(), b"data: [DONE]\n\n".to_vec()],
            },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::permissive_for_tests(OpenAiConfig::chat(base, None));
        let mut stream = provider.stream(req("gpt-x"));
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(ProviderChunk::Text { text: t }) => text.push_str(&t),
                Ok(ProviderChunk::Done) => break,
                Err(e) => panic!("split rune must assemble, got {e:?}"),
                _ => {}
            }
        }
        assert_eq!(text, "héllo");
    }

    #[tokio::test]
    async fn oversized_sse_line_is_loud_error_and_stream_ends() {
        // Hostile: one giant unbroken line. Bounded memory + loud error.
        let mut big = "data: ".to_string();
        big.extend(std::iter::repeat_n('x', 2 * 1024 * 1024));
        let server = MockServer::new();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::ChunkedSse {
                status: 200,
                chunks: vec![big.into_bytes(), b"\n\ndata: [DONE]\n\n".to_vec()],
            },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::permissive_for_tests(OpenAiConfig::chat(base, None));
        let mut stream = provider.stream(req("gpt-x"));
        let mut saw_err = false;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Err(e) if e.kind == ProviderErrorKind::Malformed => {
                    saw_err = true;
                    break;
                }
                Ok(_) => {}
                Err(e) => panic!("expected Malformed, got {e:?}"),
            }
        }
        assert!(saw_err, "oversized SSE line must be a loud Malformed error");
    }

    #[tokio::test]
    async fn request_meta_deadline_bounds_silent_stream() {
        // Audit round 15: `RequestMeta::deadline_ms` is the operation
        // deadline. A silent server with meta.deadline_ms = 1200 must error
        // Timeout at the overall bound (~1.2s, well inside 2.5s) instead of
        // waiting out the 60s first-byte default.
        let server = MockServer::new();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::Silent { status: 200 },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::permissive_for_tests(OpenAiConfig::chat(base, None));
        let mut g = req("gpt-x");
        g.meta.deadline_ms = 1200;
        let mut stream = provider.stream(g);
        let t0 = std::time::Instant::now();
        let item = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("the meta deadline must terminate the silent stream")
            .expect("an error item");
        let err = item.expect_err("must be a timeout");
        assert_eq!(err.kind, ProviderErrorKind::Timeout);
        assert!(err.retryable);
        assert!(
            t0.elapsed() < std::time::Duration::from_millis(2500),
            "meta deadline must fire at its overall bound: {:?}",
            t0.elapsed()
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), stream.next())
                .await
                .expect("the stream must end after the terminal error")
                .is_none(),
            "no further events after the meta-deadline timeout"
        );
    }

    #[tokio::test]
    async fn silent_server_never_hangs_the_stream() {
        // Audit round 9 (P1): an already-connected provider that sends
        // nothing must surface a retryable Timeout — the transport guard's
        // first-byte deadline — instead of hanging the turn forever.
        let server = MockServer::new();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::Silent { status: 200 },
        );
        let base = server.base_url().await;
        let transport: Arc<dyn HttpTransport> = Arc::new(PolicyCheckedHttpTransport::permissive());
        let headers = authorization_headers(None).unwrap();
        let deadlines = faktor_provider::transport::StreamDeadlines {
            first_byte_ms: 300,
            idle_ms: 300,
            overall_ms: 3000,
        };
        let body =
            serde_json::json!({"model": "gpt-x", "messages": [{"role": "user", "content": "hi"}]});
        let mut stream = Box::pin(openai_stream(
            transport,
            format!("{base}/chat/completions"),
            headers,
            ExtraHeaders::empty(),
            body,
            deadlines,
            None,
        ));
        let item = tokio::time::timeout(std::time::Duration::from_secs(10), stream.next())
            .await
            .expect("silent server must terminate via the transport guard")
            .expect("an error item");
        let err = item.expect_err("must be an error");
        assert!(
            matches!(err.kind, ProviderErrorKind::Timeout),
            "expected retryable timeout: {err:?}"
        );
    }

    // ------------------------------------------------------- egress (P0-36)

    fn allow_only(port: u16) -> Arc<dyn HttpTransport> {
        Arc::new(PolicyCheckedHttpTransport::with_policy_for_tests(
            DestinationPolicy::parse_lines([&format!("http://127.0.0.1:{port}")]).unwrap(),
        ))
    }

    fn https_only(port: u16) -> Arc<dyn HttpTransport> {
        Arc::new(PolicyCheckedHttpTransport::with_policy_for_tests(
            DestinationPolicy::parse_lines([&format!("https://127.0.0.1:{port}")]).unwrap(),
        ))
    }

    async fn first_error(mut stream: ProviderStream) -> ProviderError {
        stream
            .next()
            .await
            .expect("an item")
            .expect_err("the first item must be the deny error")
    }

    #[tokio::test]
    async fn egress_allowlist_gates_the_chat_path_before_connect() {
        let server = MockServer::new();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::Respond {
                status: 200,
                body: sse_body(&[serde_json::json!({
                    "choices":[{"delta":{"content":"allowed"},"finish_reason":"stop"}]
                })]),
            },
        );
        let base = server.base_url().await;
        let port = reqwest::Url::parse(&base).unwrap().port().unwrap();

        // Allowed: the mock is exactly the allowlisted destination; the
        // SSE response streams normally through the injected transport.
        let provider =
            OpenAiProvider::build(OpenAiConfig::chat(base.clone(), None), allow_only(port));
        let mut stream = provider.stream(req("gpt-x"));
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            match chunk.unwrap() {
                ProviderChunk::Text { text: t } => text.push_str(&t),
                ProviderChunk::Done => break,
                _ => {}
            }
        }
        assert_eq!(text, "allowed");
        assert_eq!(server.request_count(), 1);

        // A second instance whose policy allows a DIFFERENT port: the same
        // request is denied BEFORE any network byte (counter stays at 1).
        let denied = OpenAiProvider::build(
            OpenAiConfig::chat(base.clone(), None),
            allow_only(port.wrapping_add(1)),
        );
        let err = first_error(denied.stream(req("gpt-x"))).await;
        assert!(err.message.contains("denied"), "{}", err.message);
        assert!(!err.retryable, "denied destinations are never retried");
        assert_eq!(server.request_count(), 1, "deny happened before connect");

        // https-to-http mismatch: an https-only allowlist rule denies the
        // plain-http request before connect.
        let mismatch = OpenAiProvider::build(OpenAiConfig::chat(base, None), https_only(port));
        let err = first_error(mismatch.stream(req("gpt-x"))).await;
        assert!(err.message.contains("denied"), "{}", err.message);
        assert_eq!(server.request_count(), 1, "scheme mismatch: no connect");
    }

    #[tokio::test]
    async fn egress_allowlist_gates_the_responses_codec_path_too() {
        // The Responses family runs a SECOND request path (/responses); the
        // injected transport must gate it identically.
        let server = MockServer::new();
        server.route(
            "POST",
            "/responses",
            MockAction::Sse {
                status: 200,
                events: vec![
                    "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"m1\",\"delta\":\"hi\"}\n\n".into(),
                    "data: {\"type\":\"response.completed\"}\n\n".into(),
                    "data: [DONE]\n\n".into(),
                ],
            },
        );
        let base = server.base_url().await;
        let port = reqwest::Url::parse(&base).unwrap().port().unwrap();
        let provider = OpenAiProvider::build(
            OpenAiConfig::responses(base.clone(), None),
            allow_only(port),
        );
        let mut stream = provider.stream(req("m"));
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            match chunk.unwrap() {
                ProviderChunk::Text { text: t } => text.push_str(&t),
                ProviderChunk::Done => break,
                _ => {}
            }
        }
        assert_eq!(text, "hi");
        assert_eq!(server.request_count(), 1);

        let denied = OpenAiProvider::build(
            OpenAiConfig::responses(base, None),
            allow_only(port.wrapping_add(1)),
        );
        let err = first_error(denied.stream(req("m"))).await;
        assert!(err.message.contains("denied"), "{}", err.message);
        assert_eq!(server.request_count(), 1, "no connect on the deny path");
    }

    #[tokio::test]
    async fn non_sse_response_and_mock_transport_prove_no_real_http_needed() {
        // Non-SSE (plain JSON) endpoint response: the transport is used and
        // the stream still terminates cleanly.
        let server = MockServer::new();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::Respond {
                status: 200,
                body: "{\"ok\": true}".into(),
            },
        );
        let base = server.base_url().await;
        let port = reqwest::Url::parse(&base).unwrap().port().unwrap();
        let provider =
            OpenAiProvider::build(OpenAiConfig::chat(base.clone(), None), allow_only(port));
        let mut stream = provider.stream(req("gpt-x"));
        let err = match stream.next().await.expect("an item") {
            Ok(ProviderChunk::Done) => {
                panic!("a plain JSON body must not be an empty completed turn")
            }
            Ok(_) => panic!("a plain JSON body carries no SSE chunk"),
            Err(e) => e,
        };
        assert_eq!(err.kind, ProviderErrorKind::Malformed, "{err:?}");
        assert_eq!(
            server.request_count(),
            1,
            "the transport carried the request"
        );

        // MockHttpTransport: a canned SSE body drives the adapter's parser
        // with NO network involved at all (no server exists here).
        let canned = sse_body(&[serde_json::json!({
            "choices":[{"delta":{"content":"canned"},"finish_reason":"stop"}]
        })]);
        let mock = Arc::new(MockHttpTransport::new(200, canned));
        let as_transport: Arc<dyn HttpTransport> = mock.clone();
        let provider = OpenAiProvider::build(
            OpenAiConfig::chat("http://mock.invalid", None),
            as_transport,
        );
        let mut stream = provider.stream(req("gpt-x"));
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            match chunk.unwrap() {
                ProviderChunk::Text { text: t } => text.push_str(&t),
                ProviderChunk::Done => break,
                _ => {}
            }
        }
        assert_eq!(text, "canned");
        assert_eq!(mock.request_count(), 1, "the transport was executed");
        assert_eq!(
            mock.requests(),
            vec![(
                "POST".to_string(),
                "http://mock.invalid/chat/completions".to_string()
            )]
        );
    }

    // ------------------------------------------------- canonical usage

    /// Shared canonical-usage conformance for the Chat Completions wire
    /// (audit Phase-1 item C): mock frames shaped exactly like real SSE
    /// usage frames (`prompt_tokens` totals including the cached portion,
    /// detail objects, top-level request id) drive the REAL provider.
    mod canonical_usage_conformance {
        use super::*;
        use faktor_provider::canonical_usage_conformance;
        use faktor_provider::CanonicalUsage;

        /// A real-wire chat usage frame. `junk` adds unknown fields at
        /// every level (unknown fields must never panic).
        fn usage_frame(
            prompt: u64,
            completion: u64,
            cached: Option<u64>,
            reasoning: Option<u64>,
            id: Option<&str>,
            junk: bool,
        ) -> serde_json::Value {
            let mut usage = serde_json::json!({
                "prompt_tokens": prompt,
                "completion_tokens": completion,
            });
            if let Some(c) = cached {
                usage["prompt_tokens_details"] =
                    serde_json::json!({"cached_tokens": c, "audio_tokens": 0});
            }
            if let Some(r) = reasoning {
                usage["completion_tokens_details"] = serde_json::json!({"reasoning_tokens": r});
            }
            if junk {
                usage["prompt_tokens_details"] =
                    serde_json::json!({"cached_tokens": 0, "totally_unknown": {"n": [1, 2]}});
                usage["unknown_usage_field"] = serde_json::json!("x");
                usage["cost"] = serde_json::json!({"currency": "usd", "amount": 0.01});
            }
            let mut frame = serde_json::json!({"choices": [{"delta": {}}], "usage": usage});
            if let Some(id) = id {
                frame["id"] = serde_json::json!(id);
            }
            if junk {
                frame["unknown_top"] = serde_json::json!([1, {"a": true}]);
                frame["created"] = serde_json::json!(0);
            }
            frame
        }

        fn sse(v: serde_json::Value) -> String {
            sse_body(&[v])
        }

        fn exp(
            uncached: u64,
            cache_read: u64,
            output: u64,
            reasoning: u64,
            request_id: Option<&str>,
        ) -> CanonicalUsage {
            CanonicalUsage {
                uncached_input_tokens: uncached,
                cache_read_tokens: cache_read,
                cache_write_tokens: 0,
                output_tokens: output,
                reasoning_tokens: reasoning,
                reported_cost: None,
                request_id: request_id.map(str::to_string),
            }
        }

        canonical_usage_conformance! {
            driver: chat_family_canonical_usage_conformance,
            family: faktor_provider::usage_conformance::WireFamily::InclusiveTotal,
            label: "openai chat completions",
            request: || req("m1"),
            provider: |base: String| OpenAiProvider::permissive_for_tests(OpenAiConfig::chat(base, None)),
            method: "POST",
            path: "/chat/completions",
            cases: vec![
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "total_incl_cached_split",
                    sse(usage_frame(1000, 50, Some(600), None, None, false)),
                    exp(400, 600, 50, 0, None),
                ),
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "cache_detail_missing_uncached_total",
                    sse(usage_frame(1000, 50, None, None, None, false)),
                    exp(1000, 0, 50, 0, None),
                ),
                faktor_provider::usage_conformance::WireUsageCase::malformed(
                    "hostile_cache_over_total",
                    sse(usage_frame(100, 50, Some(600), None, None, false)),
                ),
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "reasoning_subset_inside_output",
                    sse(usage_frame(1000, 50, None, Some(30), None, false)),
                    exp(1000, 0, 50, 30, None),
                ),
                faktor_provider::usage_conformance::WireUsageCase::malformed(
                    "hostile_reasoning_over_output",
                    sse(usage_frame(1000, 20, None, Some(30), None, false)),
                ),
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "unknown_fields_never_panic",
                    sse(usage_frame(1000, 50, Some(0), None, None, true)),
                    exp(1000, 0, 50, 0, None),
                ),
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "request_id_preserved",
                    sse(usage_frame(1000, 50, None, None, Some("chatcmpl-conf-1"), false)),
                    exp(1000, 0, 50, 0, Some("chatcmpl-conf-1")),
                ),
            ]
        }
    }

    /// Canonical-usage conformance for the native Responses wire: the usage
    /// envelope rides `response.completed.response.usage`, and `input_tokens`
    /// totals INCLUDE the cached portion (InclusiveTotal family). The driver
    /// proves each frame is canonical and is the LAST chunk before Done;
    /// hostile rows are typed Malformed.
    mod responses_canonical_usage_conformance {
        use super::*;
        use faktor_provider::canonical_usage_conformance;
        use faktor_provider::CanonicalUsage;

        /// A real-wire `response.completed` envelope. `junk` adds unknown
        /// fields at every level (unknown fields must never panic).
        fn completed(
            input: u64,
            output: u64,
            cached: Option<u64>,
            reasoning: Option<u64>,
            id: Option<&str>,
            junk: bool,
        ) -> String {
            let mut usage = serde_json::json!({
                "input_tokens": input,
                "output_tokens": output,
                "total_tokens": input + output,
            });
            if let Some(c) = cached {
                usage["input_tokens_details"] =
                    serde_json::json!({"cached_tokens": c, "audio_tokens": 0});
            }
            if let Some(r) = reasoning {
                usage["output_tokens_details"] = serde_json::json!({"reasoning_tokens": r});
            }
            if junk {
                usage["unknown_usage_field"] = serde_json::json!("x");
                usage["input_tokens_details"] =
                    serde_json::json!({"cached_tokens": 0, "totally_unknown": {"n": [1, 2]}});
            }
            let mut response = serde_json::json!({"usage": usage, "status": "completed"});
            if let Some(id) = id {
                response["id"] = serde_json::json!(id);
            }
            if junk {
                response["unknown_response_field"] = serde_json::json!([1, {"a": true}]);
            }
            let mut event = serde_json::json!({"type": "response.completed", "response": response});
            if junk {
                event["unknown_event_field"] = serde_json::json!({"z": 1});
            }
            format!("data: {event}\n\ndata: [DONE]\n\n")
        }

        fn exp(
            uncached: u64,
            cache_read: u64,
            output: u64,
            reasoning: u64,
            request_id: Option<&str>,
        ) -> CanonicalUsage {
            CanonicalUsage {
                uncached_input_tokens: uncached,
                cache_read_tokens: cache_read,
                cache_write_tokens: 0,
                output_tokens: output,
                reasoning_tokens: reasoning,
                reported_cost: None,
                request_id: request_id.map(str::to_string),
            }
        }

        canonical_usage_conformance! {
            driver: responses_family_canonical_usage_conformance,
            family: faktor_provider::usage_conformance::WireFamily::InclusiveTotal,
            label: "openai responses",
            request: || req("m1"),
            provider: |base: String| OpenAiProvider::permissive_for_tests(OpenAiConfig::responses(base, None)),
            method: "POST",
            path: "/responses",
            cases: vec![
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "total_incl_cached_split",
                    completed(1000, 50, Some(600), None, None, false),
                    exp(400, 600, 50, 0, None),
                ),
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "cache_detail_missing_uncached_total",
                    completed(1000, 50, None, None, None, false),
                    exp(1000, 0, 50, 0, None),
                ),
                faktor_provider::usage_conformance::WireUsageCase::malformed(
                    "hostile_cache_over_total",
                    completed(100, 50, Some(600), None, None, false),
                ),
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "reasoning_subset_inside_output",
                    completed(1000, 50, None, Some(30), None, false),
                    exp(1000, 0, 50, 30, None),
                ),
                faktor_provider::usage_conformance::WireUsageCase::malformed(
                    "hostile_reasoning_over_output",
                    completed(1000, 20, None, Some(30), None, false),
                ),
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "unknown_fields_never_panic",
                    completed(1000, 50, Some(0), None, None, true),
                    exp(1000, 0, 50, 0, None),
                ),
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "request_id_preserved",
                    completed(1000, 50, None, None, Some("resp-conf-1"), false),
                    exp(1000, 0, 50, 0, Some("resp-conf-1")),
                ),
            ]
        }
    }

    /// A raw HTTP/1.1 server that reads the request head, writes `head`
    /// (empty = never answer headers), streams `body_prefix` bytes, then
    /// stalls with the connection open — proving reads are wall-clock and
    /// byte bounded instead of hanging or buffering.
    async fn stalling_http_server(head: &'static str, body_prefix: usize) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            if !head.is_empty() {
                let _ = socket.write_all(head.as_bytes()).await;
            }
            if body_prefix > 0 {
                let chunk = vec![b'x'; body_prefix];
                let _ = socket.write_all(&chunk).await;
                let _ = socket.flush().await;
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        format!("http://{addr}")
    }

    fn error_path_provider(base: String) -> Arc<dyn Provider> {
        OpenAiProvider::permissive_for_tests(OpenAiConfig::chat(base, None))
    }

    #[tokio::test]
    async fn oversize_error_body_is_capped_and_the_status_still_classifies() {
        let head = "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 1048576\r\n\r\n";
        let base = stalling_http_server(head, 256 * 1024).await;
        let mut stream = error_path_provider(base).stream(req("m"));
        let started = std::time::Instant::now();
        let item = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("a bounded error-body read must not hang")
            .expect("exactly one error item");
        let err = item.expect_err("a 500 must surface as an error");
        assert_eq!(err.kind, ProviderErrorKind::Server, "{err:?}");
        assert!(err.message.contains("truncated"), "{}", err.message);
        assert!(
            err.message.len() <= OPENAI_ERROR_BODY_MAX_BYTES + 128,
            "error body kept {} bytes past the cap",
            err.message.len()
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(4));
    }

    #[tokio::test]
    async fn stalled_error_body_is_bounded_by_the_wall_clock_read_bound() {
        let head = "HTTP/1.1 429 Too Many Requests\r\ncontent-length: 999\r\n\r\n";
        let base = stalling_http_server(head, 0).await;
        let mut request = req("m");
        request.meta.deadline_ms = 250;
        let mut stream = error_path_provider(base).stream(request);
        let started = std::time::Instant::now();
        let err = tokio::time::timeout(std::time::Duration::from_secs(3), stream.next())
            .await
            .expect("a stalled error body must not hang")
            .expect("one item")
            .expect_err("429 must surface");
        assert_eq!(err.kind, ProviderErrorKind::RateLimited, "{err:?}");
        assert!(err.message.contains("exceeded"), "{}", err.message);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[tokio::test]
    async fn a_server_that_never_answers_headers_is_a_typed_timeout() {
        let base = stalling_http_server("", 0).await;
        let mut request = req("m");
        request.meta.deadline_ms = 250;
        let mut stream = error_path_provider(base).stream(request);
        let started = std::time::Instant::now();
        let err = tokio::time::timeout(std::time::Duration::from_secs(3), stream.next())
            .await
            .expect("a header stall must not hang")
            .expect("one item")
            .expect_err("no response headers is an error");
        assert_eq!(err.kind, ProviderErrorKind::Timeout, "{err:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    /// P0 plaintext-secret lock: a planted key must never render through
    /// `Debug`, panic formatting, serialized diagnostics or the credential
    /// error path, and an unencodable key fails the REQUEST with a typed
    /// error instead of being dropped (which would send anonymously).
    #[tokio::test]
    async fn api_key_never_leaks_and_invalid_key_fails_the_request() {
        const PLANTED: &str = "sk-PLANTED-openai-secret-0123456789abcdef";
        let cfg = OpenAiConfig::chat("http://127.0.0.1:1/v1", Some(SecretValue::new(PLANTED)));
        let mut rendered = vec![format!("{cfg:?}")];
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("provider config: {cfg:?}")
        }))
        .expect_err("must panic");
        if let Some(message) = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
        {
            rendered.push(message);
        }
        rendered.push(serde_json::to_string(&format!("{cfg:?}")).unwrap());
        for text in &rendered {
            assert!(!text.contains(PLANTED), "api key leaked: {text}");
        }
        assert!(rendered[0].contains("[redacted]"));

        // Unencodable credential: typed error whose Display/Debug carry no
        // credential bytes.
        let bad = SecretValue::new("sk-planted\r\nX-Injected: yes");
        let err = authorization_headers(Some(&bad)).unwrap_err();
        for text in [format!("{err}"), format!("{err:?}")] {
            assert!(!text.contains("planted"), "credential leaked: {text}");
            assert!(!text.contains("X-Injected"), "credential leaked: {text}");
        }
        // The provider request fails with the typed Auth error BEFORE any
        // network byte — never an anonymous request.
        let provider = OpenAiProvider::permissive_for_tests(OpenAiConfig::chat(
            "http://127.0.0.1:1/v1",
            Some(bad),
        ));
        let mut stream = provider.stream(req("m"));
        let first = stream.next().await.expect("one item");
        let err = first.expect_err("must fail, not send anonymously");
        assert_eq!(err.kind, ProviderErrorKind::Auth, "{err:?}");
        assert!(!err.message.contains("planted"), "{}", err.message);
    }

    /// Adversarial: a provider/gateway error body that echoes request
    /// credentials must never reach the error in raw form — the bearer
    /// credential and pattern-shaped secrets are scrubbed, bounded
    /// diagnostics survive, and 401/403 bodies are withheld entirely.
    /// Covers BOTH classify call sites (chat + responses families).
    #[tokio::test]
    async fn error_bodies_are_scrubbed_and_auth_bodies_withheld() {
        const EXACT: &str = "exact-credential-value-9f2a";
        const PATTERN: &str = "sk-abcdefghijklmnopqrstuvwx";
        fn body(sentinel: &str) -> String {
            format!(r#"{{"error":{{"message":"{sentinel} {EXACT} {PATTERN}"}}}}"#)
        }
        fn transport() -> Arc<dyn HttpTransport> {
            Arc::new(PolicyCheckedHttpTransport::permissive())
        }
        fn headers() -> reqwest::header::HeaderMap {
            let mut h = reqwest::header::HeaderMap::new();
            h.insert("authorization", format!("Bearer {EXACT}").parse().unwrap());
            h
        }
        let assert_no_secret = |err: &ProviderError, leaked: &[&str]| {
            let serialized = serde_json::to_string(&err.message).expect("serialize");
            let rendered = format!("{err}|{err:?}|{serialized}");
            for secret in leaked {
                assert!(!rendered.contains(secret), "secret leaked: {rendered}");
            }
        };

        // Chat family, non-auth: scrubbed bounded diagnostic, status kept.
        let server = MockServer::new();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::Respond {
                status: 429,
                body: body("RATE-BODY-SENTINEL"),
            },
        );
        let base = server.base_url().await;
        let mut stream = Box::pin(openai_stream(
            transport(),
            format!("{base}/chat/completions"),
            headers(),
            ExtraHeaders::empty(),
            serde_json::json!({"model": "m"}),
            StreamDeadlines::default(),
            None,
        ));
        let err = stream
            .next()
            .await
            .expect("one item")
            .expect_err("429 must fail");
        assert_eq!(err.kind, ProviderErrorKind::RateLimited);
        assert_eq!(err.code.as_deref(), Some("429"));
        assert_no_secret(&err, &[EXACT, PATTERN]);
        assert!(err.message.contains("HTTP 429"), "{}", err.message);
        assert!(
            err.message.len() <= faktor_provider::sanitize::MAX_ERROR_DIAGNOSTIC_BYTES + 128,
            "diagnostic must stay bounded: {}",
            err.message.len()
        );

        // Responses family, non-auth: same scrub on the second call site.
        let server = MockServer::new();
        server.route(
            "POST",
            "/responses",
            MockAction::Respond {
                status: 500,
                body: body("RATE-BODY-SENTINEL"),
            },
        );
        let base = server.base_url().await;
        let mut stream = Box::pin(responses_stream(
            transport(),
            format!("{base}/responses"),
            headers(),
            serde_json::json!({"model": "m"}),
            StreamDeadlines::default(),
            None,
        ));
        let err = stream
            .next()
            .await
            .expect("one item")
            .expect_err("500 must fail");
        assert_eq!(err.kind, ProviderErrorKind::Server);
        assert_no_secret(&err, &[EXACT, PATTERN]);

        // Auth: the arbitrary upstream body is not preserved at all.
        let server = MockServer::new();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::Respond {
                status: 401,
                body: body("AUTH-BODY-SENTINEL"),
            },
        );
        let base = server.base_url().await;
        let mut stream = Box::pin(openai_stream(
            transport(),
            format!("{base}/chat/completions"),
            headers(),
            ExtraHeaders::empty(),
            serde_json::json!({"model": "m"}),
            StreamDeadlines::default(),
            None,
        ));
        let err = stream
            .next()
            .await
            .expect("one item")
            .expect_err("401 must fail");
        assert_eq!(err.kind, ProviderErrorKind::Auth);
        assert_no_secret(&err, &["AUTH-BODY-SENTINEL", EXACT, PATTERN]);
        assert!(err.message.contains("withheld"), "{}", err.message);
    }

    /// Adversarial: in-stream error payloads that arrive under an HTTP 2xx
    /// (Responses `error`/`response.failed` events and malformed SSE data
    /// lines) are hostile text too. Planted exact credentials (registered
    /// from the request's own Authorization header) and pattern-shaped
    /// secrets never reach `message`, `Display`, `Debug` or the
    /// JSON-serialized message/code; auth-shaped events withhold the
    /// upstream message entirely; the diagnostic stays bounded.
    #[tokio::test]
    async fn in_stream_2xx_error_payloads_are_scrubbed_or_withheld() {
        const EXACT: &str = "exact-credential-value-9f2a";
        const PATTERN: &str = "sk-abcdefghijklmnopqrstuvwx";
        fn transport() -> Arc<dyn HttpTransport> {
            Arc::new(PolicyCheckedHttpTransport::permissive())
        }
        fn headers() -> reqwest::header::HeaderMap {
            let mut h = reqwest::header::HeaderMap::new();
            h.insert("authorization", format!("Bearer {EXACT}").parse().unwrap());
            h
        }
        fn assert_no_secret(err: &ProviderError, sentinels: &[&str]) {
            let rendered = format!(
                "{err}|{err:?}|{}|{:?}",
                serde_json::to_string(&err.message).expect("serialize message"),
                serde_json::to_string(&err.code).expect("serialize code"),
            );
            for secret in [EXACT, PATTERN].iter().chain(sentinels) {
                assert!(!rendered.contains(secret), "secret leaked: {rendered}");
            }
        }
        async fn responses_err(events: Vec<String>) -> ProviderError {
            let server = MockServer::new();
            server.route(
                "POST",
                "/responses",
                MockAction::Sse {
                    status: 200,
                    events,
                },
            );
            let base = server.base_url().await;
            let mut stream = Box::pin(responses_stream(
                transport(),
                format!("{base}/responses"),
                headers(),
                serde_json::json!({"model": "m"}),
                StreamDeadlines::default(),
                None,
            ));
            stream
                .next()
                .await
                .expect("one item")
                .expect_err("2xx error event must fail the stream")
        }
        async fn chat_err(events: Vec<String>) -> ProviderError {
            let server = MockServer::new();
            server.route(
                "POST",
                "/chat/completions",
                MockAction::Sse {
                    status: 200,
                    events,
                },
            );
            let base = server.base_url().await;
            let mut stream = Box::pin(openai_stream(
                transport(),
                format!("{base}/chat/completions"),
                headers(),
                ExtraHeaders::empty(),
                serde_json::json!({"model": "m"}),
                StreamDeadlines::default(),
                None,
            ));
            stream
                .next()
                .await
                .expect("one item")
                .expect_err("2xx error payload must fail the stream")
        }

        // Responses error event, non-auth: scrubbed and bounded, plain
        // non-secret text still visible.
        let err = responses_err(vec![ev(serde_json::json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "code": "invalid_request_error",
                "message": format!("RESP-SENTINEL {EXACT} {PATTERN}"),
            },
        }))])
        .await;
        assert_eq!(err.kind, ProviderErrorKind::BadRequest);
        assert!(err.message.contains("RESP-SENTINEL"), "{}", err.message);
        assert!(
            err.message.len() <= faktor_provider::sanitize::MAX_ERROR_DIAGNOSTIC_BYTES + 128,
            "diagnostic must stay bounded: {}",
            err.message.len()
        );
        assert_no_secret(&err, &[]);

        // Responses error event, auth-shaped: the upstream message is
        // withheld entirely; the secret planted in the `code` is scrubbed
        // even though classification still reads the raw code.
        let err = responses_err(vec![ev(serde_json::json!({
            "type": "error",
            "error": {
                "type": "invalid_api_key",
                "code": format!("invalid_api_key-{EXACT}"),
                "message": format!("AUTH-RESP-SENTINEL {EXACT} {PATTERN}"),
            },
        }))])
        .await;
        assert_eq!(err.kind, ProviderErrorKind::Auth);
        assert!(err.message.contains("withheld"), "{}", err.message);
        assert_no_secret(&err, &["AUTH-RESP-SENTINEL"]);

        // Chat family, malformed 2xx data line: scrubbed, bounded.
        let err = chat_err(vec![format!(
            "data: not-json CHAT-SENTINEL {EXACT} {PATTERN}\n\n"
        )])
        .await;
        assert_eq!(err.kind, ProviderErrorKind::Malformed);
        assert!(err.message.contains("CHAT-SENTINEL"), "{}", err.message);
        assert!(
            err.message.len() <= faktor_provider::sanitize::MAX_ERROR_DIAGNOSTIC_BYTES + 128,
            "diagnostic must stay bounded: {}",
            err.message.len()
        );
        assert_no_secret(&err, &[]);

        // Chat family, auth-shaped malformed line: withheld.
        let err = chat_err(vec![format!(
            "data: authentication_error AUTH-CHAT-SENTINEL {EXACT} {PATTERN}\n\n"
        )])
        .await;
        assert_eq!(err.kind, ProviderErrorKind::Malformed);
        assert!(err.message.contains("withheld"), "{}", err.message);
        assert_no_secret(&err, &["AUTH-CHAT-SENTINEL"]);
    }

    /// A dropped upstream connection (body EOF before `finish_reason`/
    /// `[DONE]` for chat, before a terminal response event for responses)
    /// must be a TYPED failure: the truncated text was previously accepted
    /// as a completed turn.
    #[tokio::test]
    async fn a_truncated_upstream_stream_is_a_typed_failure() {
        use futures::StreamExt as _;

        // Chat family: one content delta, then the body closes.
        let server = MockServer::new();
        let delta = serde_json::json!({
            "choices": [{"index": 0, "delta": {"content": "partial "}, "finish_reason": null}]
        })
        .to_string();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::ChunkedSse {
                status: 200,
                chunks: vec![format!("data: {delta}\n\n").into_bytes()],
            },
        );
        let base = server.base_url().await;
        let mut stream = Box::pin(openai_stream(
            Arc::new(PolicyCheckedHttpTransport::permissive()),
            format!("{base}/chat/completions"),
            reqwest::header::HeaderMap::new(),
            ExtraHeaders::empty(),
            serde_json::json!({"model": "m"}),
            StreamDeadlines::default(),
            None,
        ));
        let first = stream.next().await.expect("delta").expect("text delta");
        assert!(matches!(first, ProviderChunk::Text { .. }), "{first:?}");
        let err = stream
            .next()
            .await
            .expect("error item")
            .expect_err("must fail");
        assert_eq!(err.kind, ProviderErrorKind::Malformed);
        assert!(
            err.message.contains("before finish_reason"),
            "{}",
            err.message
        );

        // Responses family: one output delta, then the body closes.
        let server = MockServer::new();
        let delta = serde_json::json!({
            "type": "response.output_text.delta",
            "delta": "partial "
        })
        .to_string();
        server.route(
            "POST",
            "/responses",
            MockAction::ChunkedSse {
                status: 200,
                chunks: vec![format!("data: {delta}\n\n").into_bytes()],
            },
        );
        let base = server.base_url().await;
        let mut stream = Box::pin(responses_stream(
            Arc::new(PolicyCheckedHttpTransport::permissive()),
            format!("{base}/responses"),
            reqwest::header::HeaderMap::new(),
            serde_json::json!({"model": "m"}),
            StreamDeadlines::default(),
            None,
        ));
        let first = stream.next().await.expect("delta").expect("text delta");
        assert!(matches!(first, ProviderChunk::Text { .. }), "{first:?}");
        let err = stream
            .next()
            .await
            .expect("error item")
            .expect_err("must fail");
        assert_eq!(err.kind, ProviderErrorKind::Malformed);
        assert!(
            err.message.contains("before a terminal response event"),
            "{}",
            err.message
        );
    }
}
