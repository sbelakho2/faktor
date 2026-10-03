//! Adversarial provider-transport tests (task category 7, stream/sanitize
//! leg). Production entry points: [`crate::transport::utf8_line_stream`],
//! [`crate::transport::guarded_lines`], [`crate::sanitize::ErrorScrubber`]
//! and the `ProviderError` retryability contract.

use super::*;
use crate::sanitize::{
    auth_shaped_text, is_auth_status, ErrorScrubber, MAX_ERROR_DIAGNOSTIC_BYTES,
};
use crate::transport::{
    guarded_lines, utf8_line_stream, StreamDeadlines, MAX_LINE_BYTES, PROVIDER_CEILING_MS,
};
use futures::StreamExt;
use std::time::Duration;

fn collect_lines(
    chunks: Vec<Result<Vec<u8>, String>>,
    max_line: usize,
) -> Vec<Result<String, ProviderError>> {
    let stream = futures::stream::iter(chunks);
    let mut lines = Box::pin(utf8_line_stream(stream, max_line));
    let mut out = Vec::new();
    while let Some(item) = futures::executor::block_on(lines.next()) {
        out.push(item);
    }
    out
}

fn one_chunk(body: &[u8], max_line: usize) -> Vec<Result<String, ProviderError>> {
    collect_lines(vec![Ok(body.to_vec())], max_line)
}

fn lines_of(body: &[u8], max_line: usize) -> Vec<String> {
    one_chunk(body, max_line)
        .into_iter()
        .map(|r| r.unwrap_or_else(|e| panic!("line decode failed: {e}")))
        .collect()
}

#[test]
fn malformed_utf8_at_every_byte_offset_is_one_loud_terminal_error() {
    let body = b"data: {\"text\":\"ok\"}\n";
    for offset in 0..=body.len() {
        let mut hostile = body.to_vec();
        hostile.insert(offset, 0xFF);
        let mut results = one_chunk(&hostile, 4096);
        assert!(
            !results.is_empty(),
            "offset {offset}: a 0xFF insertion must be observable"
        );
        // Either the line before the injection decodes, then exactly one
        // Malformed error; or the first line already carries the bad byte.
        let err_count = results.iter().filter(|r| r.is_err()).count();
        assert_eq!(
            err_count, 1,
            "offset {offset}: exactly one error for {hostile:?}"
        );
        let err = results
            .iter()
            .find_map(|r| r.as_ref().err())
            .expect("one error");
        assert_eq!(
            err.kind,
            ProviderErrorKind::Malformed,
            "offset {offset}: invalid UTF-8 is Malformed, got {err:?}"
        );
        assert!(
            err.message.contains("utf-8"),
            "offset {offset}: message names the decode failure: {}",
            err.message
        );
        // The stream is terminally dead: exactly one error, then None.
        results.retain(|r| r.is_err());
        assert_eq!(results.len(), 1, "offset {offset}: no repeated errors");
    }
    // A valid multibyte rune split across chunks is NOT malformed.
    let text = "data: héllo 漢字 😀\n";
    let bytes = text.as_bytes();
    for split in 0..=bytes.len() {
        let chunks = vec![Ok(bytes[..split].to_vec()), Ok(bytes[split..].to_vec())];
        let results = collect_lines(chunks, 4096);
        assert_eq!(
            results.len(),
            1,
            "split {split}: exactly one assembled line"
        );
        assert_eq!(
            results[0].as_deref(),
            Ok(text.trim_end_matches('\n')),
            "split {split}: valid UTF-8 must reassemble"
        );
    }
}

#[test]
fn every_chunk_split_produces_identical_lines() {
    let body = "data: one\n\nid: 2\ndata: two\r\ndata: héllo 漢字\nlast";
    let expected: Vec<String> = vec![
        "data: one".into(),
        "".into(),
        "id: 2".into(),
        "data: two".into(),
        "data: héllo 漢字".into(),
        "last".into(),
    ];
    for split in 0..=body.len() {
        let chunks = vec![
            Ok(body.as_bytes()[..split].to_vec()),
            Ok(body.as_bytes()[split..].to_vec()),
        ];
        let got: Vec<String> = collect_lines(chunks, 4096)
            .into_iter()
            .map(|r| r.unwrap_or_else(|e| panic!("split {split}: {e}")))
            .collect();
        assert_eq!(got, expected, "split {split}: lines must be identical");
    }
    // The whole body as one chunk is the same view.
    let got = lines_of(body.as_bytes(), 4096);
    assert_eq!(got, expected, "single-chunk view");
}

#[test]
fn line_cap_boundaries_are_exact_and_checked_before_emit() {
    // Exactly at the cap is legal.
    let mut body = vec![b'x'; 64];
    body.push(b'\n');
    body.extend_from_slice(b"tail\n");
    let got = lines_of(&body, 64);
    assert_eq!(got.len(), 2, "at-cap line then tail");
    assert_eq!(got[0].len(), 64, "at-cap line retained fully");
    assert_eq!(got[1], "tail");

    // One byte over the cap is a loud terminal error.
    let mut over = vec![b'x'; 65];
    over.push(b'\n');
    let results = one_chunk(&over, 64);
    assert_eq!(results.len(), 1, "no line emitted past the cap");
    let err = results[0].as_ref().expect_err("over-cap must error");
    assert_eq!(err.kind, ProviderErrorKind::Malformed, "over-cap kind");
    assert!(
        err.message.contains("exceeds"),
        "over-cap message: {}",
        err.message
    );

    // An incomplete buffer already over the cap errors before more reads.
    let mut results = collect_lines(vec![Ok(vec![b'y'; 65]), Ok(b"more\n".to_vec())], 64);
    let err = results.remove(0).unwrap_err();
    assert_eq!(
        err.kind,
        ProviderErrorKind::Malformed,
        "incomplete cap kind"
    );
    assert!(results.is_empty(), "terminal after the cap breach");

    // A giant single chunk with a newline fires exactly once, before emit.
    let mut giant = vec![b'z'; 128 * 1024];
    giant.push(b'\n');
    let mut results = one_chunk(&giant, 64 * 1024);
    assert_eq!(results.len(), 1, "giant line: exactly one result");
    let err = results.remove(0).unwrap_err();
    assert_eq!(err.kind, ProviderErrorKind::Malformed, "giant line kind");

    // The frozen default cap is respected for a body just under it.
    let mut near = vec![b'a'; MAX_LINE_BYTES - 1];
    near.push(b'\n');
    let got = lines_of(&near, MAX_LINE_BYTES);
    assert_eq!(got.len(), 1, "one line under the default cap");
    assert_eq!(got[0].len(), MAX_LINE_BYTES - 1, "default cap line length");
}

#[test]
fn crlf_bare_cr_and_empty_line_semantics() {
    let cases: Vec<(&[u8], Vec<&str>)> = vec![
        (b"a\r\nb\n", vec!["a", "b"]),
        (b"a\rb\n", vec!["a\rb"]),
        (b"\r\n", vec![""]),
        (b"\n", vec![""]),
        (b"\n\n", vec!["", ""]),
        (b"a\n\n\nb\n", vec!["a", "", "", "b"]),
        (b"\r\n\r\n", vec!["", ""]),
        (b"data: x\r\n\r\n", vec!["data: x", ""]),
        (b"bare-cr-only", vec!["bare-cr-only"]),
        (b"trailing\r", vec!["trailing\r"]),
    ];
    for (body, expected) in cases {
        let got = lines_of(body, 4096);
        assert_eq!(
            got,
            expected.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            "line semantics for {body:?}"
        );
    }
    // A CRLF split exactly between \r and \n still strips the CR.
    let chunks = vec![Ok(b"a\r".to_vec()), Ok(b"\nb\n".to_vec())];
    let got: Vec<String> = collect_lines(chunks, 4096)
        .into_iter()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(got, vec!["a", "b"], "split CRLF strips the CR");
}

#[test]
fn nul_and_control_bytes_are_valid_utf8_lines() {
    for body in [
        b"data: \x00\n".as_slice(),
        b"data: \x07\x08\x1b[0m\n".as_slice(),
        b"\x00\x01\x02\n".as_slice(),
    ] {
        let results = one_chunk(body, 4096);
        assert_eq!(
            results.len(),
            1,
            "control bytes {body:?} are valid UTF-8 lines"
        );
        assert!(
            results[0].is_ok(),
            "control bytes {body:?} must not be Malformed: {:?}",
            results[0]
        );
    }
}

#[test]
fn transport_errors_are_single_network_items() {
    let results = collect_lines(
        vec![
            Ok(b"first\n".to_vec()),
            Err("connection reset by peer".into()),
            Ok(b"never-seen\n".to_vec()),
        ],
        4096,
    );
    assert_eq!(results.len(), 2, "line then error; stream terminates");
    assert_eq!(results[0].as_deref(), Ok("first"), "first line intact");
    let err = results[1].as_ref().expect_err("network error");
    assert_eq!(err.kind, ProviderErrorKind::Network, "network kind");
    assert!(err.retryable, "network errors are retryable");
    assert!(
        err.message.contains("connection reset"),
        "message carries the transport cause: {}",
        err.message
    );
}

#[test]
fn guarded_lines_timeout_stages_are_individually_typed() {
    // First byte: nothing ever arrives.
    let silent = futures::stream::unfold((), |()| async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        Some((Ok::<String, ProviderError>("late".into()), ()))
    });
    let dl = StreamDeadlines {
        first_byte_ms: 40,
        idle_ms: 40,
        overall_ms: 0,
    };
    let mut s = Box::pin(guarded_lines(silent, dl, None));
    let err = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(async { tokio::time::timeout(Duration::from_secs(2), s.next()).await })
        .expect("first-byte window must terminate")
        .expect("an item")
        .expect_err("first-byte timeout");
    assert_eq!(err.kind, ProviderErrorKind::Timeout, "first-byte kind");
    assert!(err.retryable, "first-byte timeout retryable");
    assert!(
        err.message.contains("first byte"),
        "first-byte message: {}",
        err.message
    );

    // Idle: one item, then silence.
    let lines = futures::stream::unfold(true, |first| async move {
        if first {
            Some((Ok::<String, ProviderError>("a".into()), false))
        } else {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Some((Ok::<String, ProviderError>("never".into()), false))
        }
    });
    let dl = StreamDeadlines {
        first_byte_ms: 1000,
        idle_ms: 40,
        overall_ms: 0,
    };
    let mut s = Box::pin(guarded_lines(lines, dl, None));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let first = rt
        .block_on(async { tokio::time::timeout(Duration::from_secs(2), s.next()).await })
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first, "a", "first line passes through");
    let err = rt
        .block_on(async { tokio::time::timeout(Duration::from_secs(2), s.next()).await })
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(err.kind, ProviderErrorKind::Timeout, "idle kind");
    assert!(
        err.message.contains("idle"),
        "idle message: {}",
        err.message
    );
}

#[test]
fn guarded_lines_overall_deadline_and_dead_state() {
    // A slow-but-alive stream is killed by the OVERALL deadline, not idle.
    let lines = futures::stream::unfold(0u32, |i| async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        Some((Ok::<String, ProviderError>(format!("i{i}")), i + 1))
    });
    let dl = StreamDeadlines {
        first_byte_ms: 5_000,
        idle_ms: 5_000,
        overall_ms: 90,
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    rt.block_on(async move {
        let mut s = Box::pin(guarded_lines(lines, dl, None));
        let mut items = 0usize;
        loop {
            match tokio::time::timeout(Duration::from_secs(3), s.next())
                .await
                .expect("overall deadline terminates")
            {
                Some(Ok(_)) => items += 1,
                Some(Err(e)) => {
                    assert_eq!(e.kind, ProviderErrorKind::Timeout, "overall kind");
                    assert!(
                        e.message.contains("overall"),
                        "overall message: {}",
                        e.message
                    );
                    break;
                }
                None => panic!("must end with an overall timeout error"),
            }
        }
        assert!(items >= 1, "at least one line before the overall deadline");
        // Terminal dead state: exactly one error, then None.
        assert!(
            tokio::time::timeout(Duration::from_millis(300), s.next())
                .await
                .expect("dead state")
                .is_none(),
            "no events after the terminal overall timeout"
        );
    });
}

#[test]
fn guarded_lines_cancellation_is_wake_driven_and_terminal() {
    let silent = futures::stream::unfold((), |()| async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        Some((Ok::<String, ProviderError>("late".into()), ()))
    });
    let dl = StreamDeadlines {
        first_byte_ms: 60_000,
        idle_ms: 60_000,
        overall_ms: 0,
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    rt.block_on(async move {
        // Pre-cancelled token errors without any wait.
        let cancel = faktor_core::cancellation::CancellationToken::new();
        cancel.cancel();
        let started = std::time::Instant::now();
        let mut s = Box::pin(guarded_lines(silent, dl, Some(cancel)));
        let err = tokio::time::timeout(Duration::from_millis(500), s.next())
            .await
            .expect("pre-cancelled must resolve immediately")
            .unwrap()
            .unwrap_err();
        assert_eq!(err.kind, ProviderErrorKind::Cancelled, "pre-cancel kind");
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "pre-cancel must not wait: {:?}",
            started.elapsed()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(200), s.next())
                .await
                .expect("dead after cancel")
                .is_none(),
            "cancelled guard is terminal"
        );

        // Mid-stream cancellation wakes a parked guard promptly.
        let silent = futures::stream::unfold((), |()| async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Some((Ok::<String, ProviderError>("late".into()), ()))
        });
        let cancel = faktor_core::cancellation::CancellationToken::new();
        let mut s = Box::pin(guarded_lines(silent, dl, Some(cancel.clone())));
        let started = std::time::Instant::now();
        let waker = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            cancel.cancel();
        });
        let err = tokio::time::timeout(Duration::from_secs(2), s.next())
            .await
            .expect("cancel must surface")
            .unwrap()
            .unwrap_err();
        assert_eq!(err.kind, ProviderErrorKind::Cancelled, "mid-cancel kind");
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "mid-cancel is prompt: {:?}",
            started.elapsed()
        );
        waker.await.unwrap();
    });
}

#[test]
fn provider_error_retryability_contract_is_frozen() {
    let retryable = [
        ProviderErrorKind::Network,
        ProviderErrorKind::Timeout,
        ProviderErrorKind::RateLimited,
        ProviderErrorKind::Server,
    ];
    for kind in retryable {
        let err = ProviderError::new(kind.clone(), "x");
        assert!(err.retryable, "{kind:?} must be retryable");
        assert!(kind.retryable(), "{kind:?}.retryable()");
    }
    let terminal = [
        ProviderErrorKind::Auth,
        ProviderErrorKind::BadRequest,
        ProviderErrorKind::Cancelled,
        ProviderErrorKind::Malformed,
    ];
    for kind in terminal {
        let err = ProviderError::new(kind.clone(), "x");
        assert!(!err.retryable, "{kind:?} must not be retryable");
        assert!(!kind.retryable(), "{kind:?}.retryable()");
    }
    // Codes ride alongside without changing retryability.
    let err = ProviderError::with_code(ProviderErrorKind::RateLimited, "429", "slow down");
    assert!(err.retryable, "429 hint is retryable");
    assert_eq!(err.code.as_deref(), Some("429"), "code retained");
    let err = ProviderError::with_code(ProviderErrorKind::Auth, "401", "bad key");
    assert!(!err.retryable, "401 is terminal");
    // Display never loses the kind.
    assert!(
        format!("{err}").contains("Auth"),
        "Display names the kind: {err}"
    );
}

#[test]
fn error_scrubber_redacts_patterns_exact_values_and_request_credentials() {
    let scrubber = ErrorScrubber::new();
    let exact = "exact-credential-value-9f2a";
    scrubber.register_secret(exact);
    scrubber.register_secret(""); // ignored
    assert_eq!(scrubber.registered_len(), 1, "empty registration ignored");

    let cases: Vec<String> = vec![
        format!("upstream said sk-{}", "A".repeat(24)),
        format!("key ghp_{} rejected", "B".repeat(24)),
        format!("token {exact} leaked"),
        format!("{} around {exact}", "é".repeat(10)),
        format!("{exact}{exact}"),
        format!("x{exact}y"),
    ];
    for text in cases {
        let scrubbed = scrubber.scrub(&text);
        assert!(
            !scrubbed.contains(exact),
            "registered exact value survived {text:?}: {scrubbed:?}"
        );
        assert!(
            !scrubbed.contains(&format!("sk-{}", "A".repeat(24)))
                && !scrubbed.contains(&format!("ghp_{}", "B".repeat(24))),
            "pattern hit survived {text:?}: {scrubbed:?}"
        );
    }

    // Credential headers register the token AND the full value.
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        "Bearer header-token-1234567890".parse().unwrap(),
    );
    headers.insert("x-api-key", "header-key-abcdef".parse().unwrap());
    headers.insert("x-unrelated", "not-a-secret".parse().unwrap());
    let with_creds = scrubber.with_request_credentials(
        &headers,
        "https://api.example.com/v1?key=query-key-value&access_token=query-token-value&api-key=dash-key-value&API_KEY=upper-key&limit=5",
    );
    for secret in [
        "header-token-1234567890",
        "Bearer header-token-1234567890",
        "header-key-abcdef",
        "query-key-value",
        "query-token-value",
        "dash-key-value",
        "upper-key",
    ] {
        let echoed = format!("error body echoed {secret} back");
        let scrubbed = with_creds.scrub(&echoed);
        assert!(
            !scrubbed.contains(secret),
            "credential {secret:?} survived: {scrubbed:?}"
        );
    }
    // Non-credential query values and headers are not registered.
    assert!(
        with_creds.scrub("limit=5 not-a-secret").contains("limit=5"),
        "non-credential values are untouched"
    );
    assert!(
        with_creds.scrub("not-a-secret").contains("not-a-secret"),
        "unrelated header value is not registered"
    );
    // Registered length reflects only real credentials: 1 exact + the
    // Authorization token and full value + the x-api-key value + four
    // credential-named query values.
    assert_eq!(
        with_creds.registered_len(),
        1 + 2 + 1 + 4,
        "one exact + two header values (token+full) + one api-key header + four query values"
    );
}

#[test]
fn error_scrubber_diagnostic_withholds_auth_bodies() {
    let scrubber = ErrorScrubber::new();
    let planted = format!("key=sk-{}", "C".repeat(24));
    for status in [401u16, 403, 407] {
        let diagnostic = scrubber.diagnostic(status, &planted);
        assert!(
            diagnostic.contains("withheld"),
            "status {status} must withhold the body: {diagnostic}"
        );
        assert!(
            !diagnostic.contains("sk-"),
            "status {status} leaked the body: {diagnostic}"
        );
        assert!(is_auth_status(status), "status {status} is auth-classified");
    }
    for status in [400u16, 404, 409, 422, 429, 500, 503] {
        assert!(!is_auth_status(status), "status {status} is not auth-class");
        let diagnostic = scrubber.diagnostic(status, "some upstream detail");
        assert!(
            diagnostic.starts_with(&format!("HTTP {status}")),
            "status {status} prefix: {diagnostic}"
        );
        assert!(
            diagnostic.contains("some upstream detail"),
            "status {status} keeps non-auth detail: {diagnostic}"
        );
    }
    // Empty body yields the bare status line.
    let empty = scrubber.diagnostic(500, "   ");
    assert_eq!(empty, "HTTP 500", "empty body diagnostic");
    // A huge hostile body is bounded with the truncation marker.
    let huge = "x".repeat(MAX_ERROR_DIAGNOSTIC_BYTES * 2);
    let bounded = scrubber.diagnostic(500, &huge);
    assert!(
        bounded.contains("[diagnostic truncated]"),
        "oversized diagnostic is marked"
    );
    assert!(
        bounded.len() < MAX_ERROR_DIAGNOSTIC_BYTES + 64,
        "diagnostic stays bounded: {} bytes",
        bounded.len()
    );
    // Multibyte text cut at the bound stays valid UTF-8.
    let multibyte = "😀".repeat(MAX_ERROR_DIAGNOSTIC_BYTES);
    let bounded = scrubber.diagnostic(500, &multibyte);
    assert!(
        std::str::from_utf8(bounded.as_bytes()).is_ok(),
        "bounded diagnostic is valid UTF-8"
    );
}

#[test]
fn error_scrubber_in_stream_event_diagnostic_rules() {
    let scrubber = ErrorScrubber::new();
    let secret = format!("sk-{}", "D".repeat(24));
    let payload = format!("{{\"error\":\"boom {secret}\"}}");
    let diag = scrubber.event_diagnostic("sse error event", &payload, false);
    assert!(
        !diag.contains(&secret),
        "in-stream payload must be scrubbed: {diag}"
    );
    assert!(
        diag.starts_with("sse error event:"),
        "label preserved: {diag}"
    );
    let withheld = scrubber.event_diagnostic("sse error event", &payload, true);
    assert!(
        withheld.contains("withheld") && !withheld.contains("boom"),
        "auth-shaped in-stream payload withheld: {withheld}"
    );
    // Empty payload yields the bare label.
    assert_eq!(
        scrubber.event_diagnostic("frame", "   ", false),
        "frame",
        "empty in-stream payload"
    );
    // auth_shaped_text matrix.
    let true_cases = [
        "invalid_api_key",
        "InvalidApiKey",
        "authentication_error",
        "unauthorized",
        "forbidden",
        "invalid_credentials",
        "permission_denied",
        "invalid key",
        "api-key",
    ];
    for text in true_cases {
        assert!(auth_shaped_text(text), "{text:?} must be auth-shaped");
    }
    let false_cases = [
        "rate limit exceeded",
        "server overloaded",
        "bad request shape",
        "model not found",
        "context length exceeded",
        "quota exceeded",
    ];
    for text in false_cases {
        assert!(!auth_shaped_text(text), "{text:?} must NOT be auth-shaped");
    }
    // scrub_bounded keeps the cap on a char boundary.
    let long = format!("{}{}", "😀".repeat(1000), secret);
    let bounded = scrubber.scrub_bounded(&long, 128);
    assert!(bounded.len() <= 128, "scrub_bounded caps bytes");
    assert!(!bounded.contains(&secret), "scrub_bounded still redacts");
    // Debug is counts-only.
    let debug = format!("{scrubber:?}");
    assert!(
        debug.contains("registered"),
        "Debug reports counts: {debug}"
    );
    assert!(
        !debug.contains(&secret) && !debug.contains("sk-"),
        "Debug leaks no secret: {debug}"
    );
}

#[test]
fn scrubber_flood_caps_and_never_passes_the_tail_raw() {
    let scrubber = ErrorScrubber::new();
    scrubber.register_secret("flood-secret-value");
    let mut flood = String::new();
    for _ in 0..200 {
        flood.push_str("flood-secret-value ");
    }
    let real_tail = "flood-secret-value-real";
    scrubber.register_secret(real_tail);
    flood.push_str(real_tail);
    let scrubbed = scrubber.scrub(&flood);
    assert!(
        !scrubbed.contains(real_tail),
        "the tail past the cap must never be copied raw: {scrubbed}"
    );
    assert!(
        scrubbed.len() <= 64 * 40 + 64,
        "flood output is bounded by the hit cap: {} bytes",
        scrubbed.len()
    );
    assert!(
        scrubbed.contains(faktor_security::REDACTION_TRUNCATION_MARKER),
        "a capped scrub carries the truncation marker"
    );
    assert!(
        PROVIDER_CEILING_MS >= MAX_LINE_BYTES as u64,
        "diagnostic constants stay sane"
    );
}
