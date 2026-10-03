//! Adversarial framing corpus for the ACP wire protocol.
//!
//! Drives `detect_framing`, `parse_frame`, `parse_ndjson`, `RequestId` and
//! the encoder with byte-level hostile inputs. Every row asserts its exact
//! outcome (complete/incomplete/error and consumed byte count) with a message
//! naming the row, so a framing regression names the exact corpus entry.

use serde_json::json;

use crate::protocol::{
    self, detect_framing, parse_frame, parse_ndjson, AcpMethod, Framing, MAX_FRAME_BYTES,
    MAX_HEADER_BYTES,
};
use crate::RequestId;

#[derive(Debug, PartialEq)]
enum Cl {
    Incomplete,
    Error,
    Frame(usize, serde_json::Value),
}

fn classify_cl(bytes: &[u8]) -> Cl {
    match parse_frame(bytes) {
        Ok(None) => Cl::Incomplete,
        Err(_) => Cl::Error,
        Ok(Some((consumed, value))) => Cl::Frame(consumed, value),
    }
}

struct ClRow {
    label: &'static str,
    bytes: Vec<u8>,
    expect: Cl,
}

fn cl<B: Into<Vec<u8>>>(label: &'static str, bytes: B, expect: Cl) -> ClRow {
    ClRow {
        label,
        bytes: bytes.into(),
        expect,
    }
}

fn cl_frame(body: &str) -> Vec<u8> {
    format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
}

fn cl_rows() -> Vec<ClRow> {
    let mut rows = Vec::new();
    let frame_ok =
        |body: &str| Cl::Frame(cl_frame(body).len(), serde_json::from_str(body).unwrap());

    rows.push(cl("empty", Vec::new(), Cl::Incomplete));
    rows.push(cl(
        "partial-terminator",
        b"Content-Length: 2\r\n\r".to_vec(),
        Cl::Incomplete,
    ));
    rows.push(cl(
        "short-body",
        b"Content-Length: 5\r\n\r\nhel".to_vec(),
        Cl::Incomplete,
    ));
    rows.push(cl(
        "header-only",
        b"Content-Length: 5\r\n\r\n".to_vec(),
        Cl::Incomplete,
    ));
    rows.push(cl(
        "no-terminator-under-bound",
        b"Content-Length: 5\r\n".to_vec(),
        Cl::Incomplete,
    ));
    rows.push(cl(
        "no-terminator-over-header-bound",
        vec![b'x'; MAX_HEADER_BYTES + 1],
        Cl::Error,
    ));
    rows.push(cl(
        "garbage-with-terminator",
        b"GET / HTTP/1.1\r\nHost: x\r\n\r\n".to_vec(),
        Cl::Error,
    ));
    rows.push(cl(
        "header-line-without-colon",
        b"Content-Length 5\r\n\r\nhello".to_vec(),
        Cl::Error,
    ));
    rows.push(cl(
        "duplicate-content-length",
        b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
        Cl::Error,
    ));
    rows.push(cl(
        "duplicate-case-insensitive-name",
        b"content-length: 2\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
        Cl::Error,
    ));
    rows.push(cl(
        "missing-content-length",
        b"X-Trace: 1\r\n\r\n{}".to_vec(),
        Cl::Error,
    ));
    rows.push(cl("empty-header-lines", b"\r\n\r\n{}".to_vec(), Cl::Error));
    for (label, value) in [
        ("negative", "-1"),
        ("float", "2.5"),
        ("hex", "0x2"),
        ("empty", ""),
        ("letters", "abc"),
        ("u64-overflow", "99999999999999999999"),
        ("whitespace-only", "   "),
        ("internal-space", "2 3"),
    ] {
        rows.push(cl(
            label,
            format!("Content-Length: {value}\r\n\r\n{{}}").into_bytes(),
            Cl::Error,
        ));
    }
    rows.push(cl(
        "declared-over-16mib",
        format!("Content-Length: {}\r\n\r\n", MAX_FRAME_BYTES + 1).into_bytes(),
        Cl::Error,
    ));
    rows.push(cl(
        "declared-at-16mib-bound-incomplete",
        format!("Content-Length: {MAX_FRAME_BYTES}\r\n\r\n").into_bytes(),
        Cl::Incomplete,
    ));
    {
        let bytes = b"content-LENGTH: 2\r\n\r\n{}".to_vec();
        let consumed = bytes.len();
        rows.push(cl(
            "case-insensitive-header-name-accepted",
            bytes,
            Cl::Frame(consumed, json!({})),
        ));
    }
    {
        let bytes = b"Content-Length:    2   \r\n\r\n{}".to_vec();
        let consumed = bytes.len();
        rows.push(cl(
            "spaces-around-value",
            bytes,
            Cl::Frame(consumed, json!({})),
        ));
    }
    {
        let bytes = b"X-A: 1\r\nContent-Length: 2\r\nX-B: 2\r\n\r\n{}".to_vec();
        let consumed = bytes.len();
        rows.push(cl(
            "unknown-headers-ignored",
            bytes,
            Cl::Frame(consumed, json!({})),
        ));
    }
    rows.push(cl(
        "too-many-header-lines",
        {
            let mut v = Vec::new();
            for i in 0..70 {
                v.extend_from_slice(format!("X-{i}: 1\r\n").as_bytes());
            }
            v.extend_from_slice(b"Content-Length: 2\r\n\r\n{}");
            v
        },
        Cl::Error,
    ));
    rows.push(cl("invalid-json-body", cl_frame("{not json"), Cl::Error));
    rows.push(cl("json-object", cl_frame("{}"), frame_ok("{}")));
    rows.push(cl(
        "json-array-batch",
        cl_frame("[1,2]"),
        Cl::Frame(cl_frame("[1,2]").len(), json!([1, 2])),
    ));
    rows.push(cl(
        "json-string",
        cl_frame("\"id\""),
        Cl::Frame(cl_frame("\"id\"").len(), json!("id")),
    ));
    rows.push(cl(
        "json-null",
        cl_frame("null"),
        Cl::Frame(cl_frame("null").len(), serde_json::Value::Null),
    ));
    rows.push(cl(
        "body-non-utf8",
        {
            let mut v = b"Content-Length: 2\r\n\r\n".to_vec();
            v.extend_from_slice(&[0xff, 0xfe]);
            v
        },
        Cl::Error,
    ));
    rows.push(cl(
        "two-frames-first-consumed",
        {
            let mut v = cl_frame("{}");
            v.extend_from_slice(&cl_frame("{\"a\":1}"));
            v
        },
        Cl::Frame(cl_frame("{}").len(), json!({})),
    ));
    rows.push(cl(
        "declared-shorter-consumes-prefix",
        b"Content-Length: 2\r\n\r\n{}leftover".to_vec(),
        Cl::Frame(b"Content-Length: 2\r\n\r\n{}".len(), json!({})),
    ));
    rows.push(cl(
        "declared-longer-incomplete-even-with-json",
        b"Content-Length: 100\r\n\r\n{}".to_vec(),
        Cl::Incomplete,
    ));
    rows.push(cl(
        "raw-crlf-inside-json-string-is-invalid-json",
        cl_frame("\"a\r\nb\""),
        Cl::Error,
    ));
    rows.push(cl(
        "colon-only-header-line",
        b": 2\r\n\r\n{}".to_vec(),
        Cl::Error,
    ));
    rows.push(cl(
        "nul-in-header-name",
        b"Content-Length\0X: 2\r\n\r\n{}".to_vec(),
        Cl::Error,
    ));
    rows.push(cl(
        "zero-length-body-is-not-json",
        b"Content-Length: 0\r\n\r\n".to_vec(),
        Cl::Error,
    ));
    {
        let bytes = b"Content-Length: +2\r\n\r\n{}".to_vec();
        let consumed = bytes.len();
        rows.push(cl(
            "leading-plus-length",
            bytes,
            Cl::Frame(consumed, json!({})),
        ));
    }
    {
        let bytes = b"Content-Length: 2\t\r\n\r\n{}".to_vec();
        let consumed = bytes.len();
        rows.push(cl(
            "trailing-tab-value",
            bytes,
            Cl::Frame(consumed, json!({})),
        ));
    }
    rows.push(cl(
        "declared-past-two-terminators-is-incomplete",
        b"Content-Length: 9\r\n\r\n{}\r\n\r\n".to_vec(),
        Cl::Incomplete,
    ));
    rows
}

/// Content-Length parser corpus: every row asserts its exact class and, for
/// complete frames, the exact consumed byte count and decoded value.
#[test]
fn acp_content_length_corpus_matches_every_outcome() {
    let rows = cl_rows();
    assert!(
        rows.len() >= 40,
        "the CL corpus must keep at least 40 rows, found {}",
        rows.len()
    );
    for r in rows {
        let got = classify_cl(&r.bytes);
        match (&r.expect, &got) {
            (Cl::Incomplete, Cl::Incomplete) | (Cl::Error, Cl::Error) => {}
            (Cl::Frame(want_len, want), Cl::Frame(got_len, got)) => {
                assert_eq!(got_len, want_len, "case {:?}: consumed length", r.label);
                assert_eq!(got, want, "case {:?}: decoded value", r.label);
            }
            _ => panic!("case {:?}: outcome mismatch (got {:?})", r.label, got),
        }
    }
}

#[derive(Debug, PartialEq)]
enum Nd {
    Incomplete,
    Error,
    Message(usize, serde_json::Value),
}

fn classify_nd(bytes: &[u8]) -> Nd {
    match parse_ndjson(bytes) {
        Ok(None) => Nd::Incomplete,
        Err(_) => Nd::Error,
        Ok(Some((consumed, value))) => Nd::Message(consumed, value),
    }
}

struct NdRow {
    label: &'static str,
    bytes: Vec<u8>,
    expect: Nd,
}

fn nd(label: &'static str, bytes: &[u8], expect: Nd) -> NdRow {
    NdRow {
        label,
        bytes: bytes.to_vec(),
        expect,
    }
}

/// NDJSON corpus: blank-line floods, CRLF, multi-message buffers, invalid
/// lines and the oversized-line fatal bound.
#[test]
fn acp_ndjson_corpus_matches_every_outcome() {
    let mut rows = vec![
        nd("empty", b"", Nd::Incomplete),
        nd("no-newline-partial", b"{\"a\":1}", Nd::Incomplete),
        nd(
            "oversized-line-without-newline",
            &vec![b'x'; MAX_FRAME_BYTES + 1],
            Nd::Error,
        ),
        nd(
            "one-object",
            b"{\"a\":1}\n",
            Nd::Message(8, json!({"a": 1})),
        ),
        nd(
            "crlf-object",
            b"{\"a\":1}\r\n",
            Nd::Message(9, json!({"a": 1})),
        ),
        nd(
            "blank-line-then-object",
            b"\n{\"a\":1}\n",
            Nd::Message(9, json!({"a": 1})),
        ),
        nd(
            "whitespace-lines-then-object",
            b" \r\n\t\n{\"a\":1}\n",
            Nd::Message(13, json!({"a": 1})),
        ),
        nd("blank-only", b"\n", Nd::Incomplete),
        nd("many-blank-then-partial", b"\n\n\n{", Nd::Incomplete),
        nd("invalid-json-line", b"not json\n", Nd::Error),
        nd("empty-object", b"{}\n", Nd::Message(3, json!({}))),
        nd("array-line", b"[1,2]\n", Nd::Message(6, json!([1, 2]))),
        nd("scalar-line", b"42\n", Nd::Message(3, json!(42))),
        nd(
            "two-messages-first-consumed",
            b"{}\n{\"b\":2}\n",
            Nd::Message(3, json!({})),
        ),
        nd(
            "trailing-partial-after-message",
            b"{}\n{",
            Nd::Message(3, json!({})),
        ),
        nd("nul-in-line", b"\"a\0b\"\n", Nd::Error),
        nd("invalid-utf8-line", b"\xff\xfe\n", Nd::Error),
    ];
    // Deep nesting just inside and far beyond serde's recursion bound.
    {
        let depth = 64;
        let line = format!("{}{}\n", "[".repeat(depth), "]".repeat(depth));
        rows.push(NdRow {
            label: "deep-nesting-within-bound",
            bytes: line.as_bytes().to_vec(),
            expect: Nd::Message(line.len(), serde_json::from_str(line.trim_end()).unwrap()),
        });
        let depth = 200_000;
        let line = format!("{}{}\n", "[".repeat(depth), "]".repeat(depth));
        rows.push(NdRow {
            label: "deep-nesting-past-recursion-bound",
            bytes: line.into_bytes(),
            expect: Nd::Error,
        });
    }
    for r in rows {
        let got = classify_nd(&r.bytes);
        match (&r.expect, &got) {
            (Nd::Incomplete, Nd::Incomplete) | (Nd::Error, Nd::Error) => {}
            (Nd::Message(want_len, want), Nd::Message(got_len, got)) => {
                assert_eq!(got_len, want_len, "case {:?}: consumed length", r.label);
                assert_eq!(got, want, "case {:?}: decoded value", r.label);
            }
            _ => panic!("case {:?}: outcome mismatch (got {:?})", r.label, got),
        }
    }
}

/// Framing detection: whitespace is skipped, the first non-whitespace byte
/// decides, and only `{`/`[` select NDJSON.
#[test]
fn acp_framing_detection_corpus() {
    let rows: [(&str, &[u8], Option<Framing>); 16] = [
        ("empty", b"", None),
        ("spaces", b"   ", None),
        ("crlf-tab-space", b"\r\n\t ", None),
        ("object", b"{}", Some(Framing::Ndjson)),
        ("array", b"[]", Some(Framing::Ndjson)),
        ("object-after-ws", b" \r\n {}", Some(Framing::Ndjson)),
        ("array-after-tab", b"\t[", Some(Framing::Ndjson)),
        (
            "cl-header",
            b"Content-Length: 2",
            Some(Framing::ContentLength),
        ),
        (
            "cl-lowercase",
            b"content-length: 2",
            Some(Framing::ContentLength),
        ),
        (
            "cl-after-ws",
            b" \r\nContent-Length: 2\r\n\r\n",
            Some(Framing::ContentLength),
        ),
        ("digit-first", b"1", Some(Framing::ContentLength)),
        ("quote-first", b"\"x\"", Some(Framing::ContentLength)),
        ("nul-first", b"\0", Some(Framing::ContentLength)),
        ("invalid-utf8-first", b"\xff", Some(Framing::ContentLength)),
        ("utf8-bom", b"\xef\xbb\xbf{}", Some(Framing::ContentLength)),
        ("newline-only", b"\n", None),
    ];
    for (label, bytes, expect) in rows {
        assert_eq!(
            detect_framing(bytes),
            expect,
            "case {label}: detection must match the first non-whitespace byte"
        );
    }
}

/// RequestId parsing: only non-negative u64 integers and strings are valid;
/// floats, negatives, nulls, bools and objects are refused; round-trips echo
/// the exact wire value.
#[test]
fn acp_request_id_corpus_is_exact() {
    let valid: [(&str, serde_json::Value, RequestId); 9] = [
        ("zero", json!(0), RequestId::Number(0)),
        ("one", json!(1), RequestId::Number(1)),
        ("u64-max", json!(u64::MAX), RequestId::Number(u64::MAX)),
        ("empty-string", json!(""), RequestId::String(String::new())),
        (
            "uuid-string",
            json!("f47ac10b-58cc-4372-a567-0e02b2c3d479"),
            RequestId::String("f47ac10b-58cc-4372-a567-0e02b2c3d479".into()),
        ),
        (
            "numeric-string",
            json!("12"),
            RequestId::String("12".into()),
        ),
        ("space-string", json!(" "), RequestId::String(" ".into())),
        (
            "unicode-string",
            json!("\u{1F600}"),
            RequestId::String("\u{1F600}".into()),
        ),
        (
            "long-string",
            json!("x".repeat(10_000)),
            RequestId::String("x".repeat(10_000)),
        ),
    ];
    for (label, value, expected) in valid {
        let parsed = RequestId::from_value(&value)
            .unwrap_or_else(|| panic!("case {label}: must parse as a request id"));
        assert_eq!(parsed, expected, "case {label}: parsed id");
        assert_eq!(
            parsed.to_value(),
            value,
            "case {label}: the wire value must be echoed verbatim"
        );
        match &expected {
            RequestId::Number(n) => {
                assert_eq!(parsed.as_u64(), Some(*n), "case {label}: as_u64");
                assert!(
                    parsed.as_str().is_none(),
                    "case {label}: numeric has no str form"
                );
            }
            RequestId::String(s) => {
                assert_eq!(parsed.as_str(), Some(s.as_str()), "case {label}: as_str");
                assert!(
                    parsed.as_u64().is_none(),
                    "case {label}: string has no numeric form"
                );
            }
        }
    }

    let invalid = [
        ("null", json!(null)),
        ("bool-true", json!(true)),
        ("bool-false", json!(false)),
        ("float", json!(1.5)),
        ("negative", json!(-1)),
        ("array", json!([1])),
        ("object", json!({"id": 1})),
        ("bignum", json!(1e30)),
    ];
    for (label, value) in invalid {
        assert!(
            RequestId::from_value(&value).is_none(),
            "case {label}: {value} must not parse as a request id"
        );
    }
}

/// Encoder round-trips: `encode`/`encode_line` outputs re-parse to the same
/// value through their own parser, and `frame` always emits a numeric id.
#[test]
fn acp_encoder_round_trips_every_shape() {
    let values = [
        json!({}),
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        json!({"a": [1, 2, 3], "b": {"c": null}}),
        json!("plain string"),
        json!(42),
        json!([true, false, null]),
        json!({"text": "\u{1F600} \u{e9}"}),
    ];
    for value in values {
        let encoded = protocol::encode(&value).unwrap();
        let (consumed, parsed) = parse_frame(&encoded)
            .unwrap_or_else(|e| panic!("encode output must parse: {e}: {value}"))
            .unwrap_or_else(|| panic!("encode output must be complete: {value}"));
        assert_eq!(consumed, encoded.len(), "CL round-trip consumed all");
        assert_eq!(parsed, value, "CL round-trip value: {value}");

        let line = protocol::encode_line(&value).unwrap();
        let (consumed, parsed) = parse_ndjson(&line)
            .unwrap_or_else(|e| panic!("encode_line output must parse: {e}: {value}"))
            .unwrap_or_else(|| panic!("encode_line output must be complete: {value}"));
        assert_eq!(consumed, line.len(), "NDJSON round-trip consumed all");
        assert_eq!(parsed, value, "NDJSON round-trip value: {value}");
    }
    let framed = protocol::frame("session/prompt".into(), 9, json!({"sessionId": "s"}));
    let (_, value) = parse_frame(&framed).unwrap().unwrap();
    assert_eq!(value["id"], json!(9), "frame must carry the numeric id");
    assert_eq!(value["method"], json!("session/prompt"));
    let notification = protocol::notification_frame("session/update".into(), json!({"x": 1}));
    let (_, value) = parse_frame(&notification).unwrap().unwrap();
    assert!(
        value.get("id").is_none(),
        "a JSON-RPC notification must omit the id member entirely: {value}"
    );
}

/// Method round-trip: every known method string maps to its variant and back;
/// unknown strings are refused (never silently coerced).
#[test]
fn acp_method_round_trip_corpus() {
    for method in [
        AcpMethod::Initialize,
        AcpMethod::Shutdown,
        AcpMethod::SessionNew,
        AcpMethod::SessionLoad,
        AcpMethod::SessionPrompt,
        AcpMethod::SessionCancel,
        AcpMethod::SessionAbort,
        AcpMethod::SessionList,
        AcpMethod::AgentInfo,
        AcpMethod::Authenticate,
        AcpMethod::RequestPermission,
        AcpMethod::FsReadTextFile,
        AcpMethod::FsWriteTextFile,
        AcpMethod::TerminalCreate,
        AcpMethod::TerminalInput,
        AcpMethod::TerminalResize,
        AcpMethod::TerminalKill,
        AcpMethod::TerminalClose,
        AcpMethod::TerminalList,
    ] {
        let wire = method.as_str();
        assert_eq!(
            AcpMethod::from_wire_str(wire),
            Some(method),
            "method {wire:?} must round-trip"
        );
        assert_eq!(
            wire.parse::<AcpMethod>(),
            Ok(method),
            "FromStr for {wire:?} must round-trip"
        );
    }
    for unknown in [
        "",
        "initialize ",
        "Initialize",
        "session/new ",
        "session/unknown",
        "terminal/exec",
        "fs/read",
        "\u{1F600}",
    ] {
        assert!(
            AcpMethod::from_wire_str(unknown).is_none(),
            "unknown method {unknown:?} must be refused"
        );
        assert!(
            unknown.parse::<AcpMethod>().is_err(),
            "FromStr must refuse unknown method {unknown:?}"
        );
    }
}
