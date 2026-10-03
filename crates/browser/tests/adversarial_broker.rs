//! Adversarial browser-broker tests (task category 8): download filename /
//! directory sanitization, download bounds, websocket closure, profile
//! symlink swaps, CONNECT/forward framing and hop-by-hop stripping.
//!
//! Production entry points only: [`faktor_browser::download`] helpers,
//! [`faktor_browser::InterceptionPolicy`], [`faktor_browser::HostPattern`],
//! [`faktor_browser::EgressBroker`] over real loopback sockets and
//! [`faktor_browser::ProfileStore`].

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use faktor_browser::download::{
    download_filename, fail_closed_u64, header_value, is_response_stage, validate_relative_dir,
};
use faktor_browser::{
    BlockReason, BrokerConfig, DestinationPolicy, DownloadManager, DownloadPolicy, DownloadState,
    EgressAddressPolicy, EgressBroker, HostPattern, InterceptionDecision, InterceptionPolicy,
    ProfileStore, ProxyCredentials, ResourceType, UpstreamProxy, UpstreamSelector,
};

// ------------------------------------------------------------- helpers

async fn spawn_upstream_full(seen: Arc<Mutex<Vec<String>>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut byte = [0u8; 1];
                while !buf.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => buf.push(byte[0]),
                    }
                }
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf).to_string());
                let body = "upstream-ok";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    addr
}

fn upstream_config(upstream: SocketAddr) -> BrokerConfig {
    BrokerConfig {
        policy: DestinationPolicy::first_party_only(vec![HostPattern::parse("127.0.0.1").unwrap()])
            .with_allow_loopback(true)
            .with_allowed_ports(vec![9, 80, 443]),
        upstream: UpstreamSelector::new(Some(
            UpstreamProxy::new("127.0.0.1", upstream.port())
                .with_address_policy(EgressAddressPolicy::LOCAL),
        )),
        ..BrokerConfig::default()
    }
}

async fn send_raw(addr: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf)).await;
    String::from_utf8_lossy(&buf).to_string()
}

// ------------------------------------------------- download filenames

#[test]
fn download_filename_neutralizes_hostile_dispositions() {
    // (disposition, url, expected)
    let cases: Vec<(Option<&str>, &str, &str)> = vec![
        (
            Some("attachment; filename=../../etc/passwd"),
            "https://x.test/dl",
            "passwd",
        ),
        (
            Some(r#"attachment; filename="..\..\evil.exe""#),
            "https://x.test/dl",
            "evil.exe",
        ),
        (
            Some(r#"attachment; filename="C:\Windows\evil.dll""#),
            "https://x.test/dl",
            "evil.dll",
        ),
        (
            Some("attachment; filename=a:b|c"),
            "https://x.test/dl",
            "ab|c",
        ),
        (
            Some("attachment; filename= .hidden "),
            "https://x.test/dl",
            "hidden",
        ),
        (
            Some("attachment; filename=..."),
            "https://x.test/url.bin",
            "download-req-1",
        ),
        (
            Some("attachment; filename=\u{0}a\u{7}b.pdf"),
            "https://x.test/dl",
            "ab.pdf",
        ),
        (
            Some("attachment; filename=résumé.pdf"),
            "https://x.test/dl",
            "résumé.pdf",
        ),
        (
            Some("attachment; filename=日本語.pdf"),
            "https://x.test/dl",
            "日本語.pdf",
        ),
        // Plain filename is NOT percent-decoded: the literal stays one
        // component and cannot introduce a separator.
        (
            Some("attachment; filename=%2e%2e%2fevil.txt"),
            "https://x.test/dl",
            "%2e%2e%2fevil.txt",
        ),
        // filename* IS percent-decoded, then re-sanitized.
        (
            Some("attachment; filename*=UTF-8''%2e%2e%2fevil.txt"),
            "https://x.test/dl",
            "evil.txt",
        ),
        (
            Some("attachment; filename*=UTF-8''%E2%82%AC.txt"),
            "https://x.test/dl",
            "€.txt",
        ),
        (
            Some("attachment; filename*=UTF-8''%ZZ"),
            "https://x.test/dl",
            "%ZZ",
        ),
        (
            Some("attachment; filename*=UTF-8''a%20b.txt"),
            "https://x.test/dl",
            "a b.txt",
        ),
        // A malformed part aborts disposition parsing; the URL fallback is
        // still sanitized.
        (
            Some("attachment; broken; filename=evil.sh"),
            "https://x.test/url.bin",
            "url.bin",
        ),
        (None, "https://x.test/a/b/file.zip?q=1#frag", "file.zip"),
        (None, "https://x.test/a%2Fb", "b"),
        (None, "https://x.test/a%5Cb", "b"),
        (None, "https://x.test/", "download-req-1"),
        (Some("attachment"), "", "download-req-1"),
        (
            Some("attachment; filename=.."),
            "https://x.test/",
            "download-req-1",
        ),
    ];
    for (disposition, url, expected) in cases {
        assert_eq!(
            download_filename(disposition, url, "req-1"),
            expected,
            "download_filename({disposition:?}, {url:?})"
        );
    }
    // Fallback names are token-sanitized and bounded.
    assert_eq!(
        download_filename(
            Some("attachment; filename=.."),
            "https://x.test/",
            "req-123"
        ),
        "download-req-123",
        "request-id fallback"
    );
    assert_eq!(
        download_filename(None, "https://x.test/", "../../x"),
        "download-x",
        "request-id traversal is token-filtered"
    );
    assert_eq!(
        download_filename(None, "https://x.test/", ""),
        "download-stream",
        "empty request-id fallback"
    );
    assert_eq!(
        download_filename(None, "https://x.test/", "😀id"),
        "download-id",
        "non-ascii request-id is filtered"
    );
}

#[test]
fn download_filename_is_always_one_bounded_component() {
    let hostile = [
        "attachment; filename=../../../../root/.ssh/id_rsa",
        "attachment; filename=\\\\server\\share\\x",
        "attachment; filename=.",
        "attachment; filename=..",
        "attachment; filename=/",
        "attachment; filename=\\",
        "attachment; filename=:",
        "attachment; filename=..... ..",
        "attachment; filename=a\r\nb.txt",
        "attachment; filename=\u{202e}gpj.exe",
        "attachment; filename=\u{200b}\u{200b}",
        "attachment; filename=\u{0}",
        "attachment; filename=😀😀😀.txt",
        "attachment; filename*=UTF-8''%2e%2e%5c%2e%2e%5cx",
        "attachment; filename*=UTF-8''%00evil",
        "attachment; filename*=UTF-8''",
        "attachment; filename=\"\"",
        "attachment",
        "; filename=x",
        "filename=../../x",
    ];
    for disposition in hostile {
        for url in [
            "https://x.test/a/b/c.bin?x=1",
            "https://x.test/",
            "file:///etc/passwd",
            "",
            "not a url at all",
        ] {
            let name = download_filename(Some(disposition), url, "req/..\\id");
            assert!(
                !name.is_empty(),
                "name for {disposition:?} / {url:?} must be non-empty"
            );
            assert!(
                !name.contains('/') && !name.contains('\\'),
                "name for {disposition:?} / {url:?} keeps separators: {name:?}"
            );
            assert!(
                !name.contains(':') && !name.chars().any(char::is_control),
                "name for {disposition:?} / {url:?} keeps hostile chars: {name:?}"
            );
            assert!(
                name.chars().count() <= 200,
                "name for {disposition:?} / {url:?} exceeds the bound: {}",
                name.chars().count()
            );
            assert!(
                name != "." && name != "..",
                "name for {disposition:?} / {url:?} is a dot component"
            );
        }
    }
    // An overlong disposition is bounded to 200 chars.
    let long = format!("attachment; filename={}", "a".repeat(500));
    let name = download_filename(Some(&long), "https://x.test/", "r");
    assert_eq!(name.chars().count(), 200, "overlong filename is bounded");
    assert!(name.chars().all(|c| c == 'a'), "bounded name is the prefix");
}

#[test]
fn download_directory_policy_is_relative_only() {
    let accepted: Vec<(&str, &str)> = vec![
        ("downloads", "downloads"),
        ("./downloads", "downloads"),
        ("a/./b", "a/b"),
        ("a//b", "a/b"),
        ("d/", "d"),
        ("a/b/c", "a/b/c"),
        ("...", "..."),
        ("with space", "with space"),
        ("résumé", "résumé"),
    ];
    for (input, expected) in accepted {
        let got = validate_relative_dir(input)
            .unwrap_or_else(|e| panic!("directory {input:?} must validate: {e}"));
        assert_eq!(
            got,
            std::path::PathBuf::from(expected),
            "validated directory for {input:?}"
        );
        assert!(got.is_relative(), "directory {input:?} stays relative");
        assert!(
            !got.components().any(|c| matches!(
                c,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )),
            "directory {input:?} has no parent/root/prefix"
        );
    }
    let rejected = [
        "",
        "..",
        "../x",
        "a/../../b",
        "/absolute",
        "a/..",
        "a/../..",
        "a\0b",
    ];
    for input in rejected {
        assert!(
            validate_relative_dir(input).is_err(),
            "directory {input:?} must be refused"
        );
    }
    // Length bound: 512 accepted, 513 refused.
    let ok = "a".repeat(512);
    assert!(
        validate_relative_dir(&ok).is_ok(),
        "512-byte directory is accepted"
    );
    let too_long = "a".repeat(513);
    assert!(
        validate_relative_dir(&too_long).is_err(),
        "513-byte directory is refused"
    );
    // Policy shape rules.
    let disabled = DownloadPolicy::default();
    assert_eq!(
        disabled.validate().ok(),
        Some(None),
        "disabled policy validates with no directory"
    );
    let missing_dir = DownloadPolicy {
        enabled: true,
        directory: None,
        max_bytes: 1,
    };
    assert!(
        missing_dir.validate().is_err(),
        "enabled without a directory is refused"
    );
    let zero_bytes = DownloadPolicy {
        enabled: true,
        directory: Some("dl".into()),
        max_bytes: 0,
    };
    assert!(zero_bytes.validate().is_err(), "max_bytes 0 is refused");
    let escaping = DownloadPolicy {
        enabled: true,
        directory: Some("../dl".into()),
        max_bytes: 1,
    };
    assert!(
        escaping.validate().is_err(),
        "parent-dir download directory is refused"
    );
    let good = DownloadPolicy {
        enabled: true,
        directory: Some("dl/inner".into()),
        max_bytes: 1024,
    };
    assert_eq!(
        good.validate().unwrap(),
        Some(std::path::PathBuf::from("dl/inner")),
        "valid enabled policy"
    );
    let (method, params) = good.set_behavior_command();
    assert_eq!(method, "Browser.setDownloadBehavior", "behavior command");
    assert_eq!(params["behavior"], "deny", "native downloads always denied");
}

#[test]
fn download_bounds_helpers_fail_closed() {
    use serde_json::Value;
    type BoundFixture = (Option<Value>, Result<Option<u64>, ()>);
    let cases: Vec<BoundFixture> = vec![
        (None, Ok(None)),
        (Some(Value::Null), Ok(None)),
        (Some(json!(0)), Ok(Some(0))),
        (Some(json!(1)), Ok(Some(1))),
        (Some(json!(u64::MAX)), Ok(Some(u64::MAX))),
        (Some(json!(-1)), Err(())),
        (Some(json!(-0.5)), Err(())),
        (Some(json!(1e30)), Err(())),
        (Some(json!("5")), Err(())),
        (Some(json!(true)), Err(())),
        (Some(json!({})), Err(())),
        (Some(json!([1])), Err(())),
    ];
    for (value, expected) in cases {
        let got = fail_closed_u64(value.as_ref()).map_err(|_| ());
        assert_eq!(got, expected, "fail_closed_u64({value:?}) must match");
    }

    let headers = vec![
        json!({"name": "Content-Type", "value": "application/pdf"}),
        json!({"name": "content-disposition", "value": "attachment; filename=a.txt"}),
        json!({"name": "X-Bad", "value": 7}),
        json!({"value": "no-name"}),
        json!({"name": "X-Bad", "value": "recovered"}),
        json!({"name": "X-Empty", "value": ""}),
    ];
    assert_eq!(
        header_value(&headers, "CONTENT-TYPE").as_deref(),
        Some("application/pdf"),
        "header lookup is case-insensitive"
    );
    assert_eq!(
        header_value(&headers, "Content-Disposition").as_deref(),
        Some("attachment; filename=a.txt"),
        "attachment header found"
    );
    assert_eq!(
        header_value(&headers, "x-bad").as_deref(),
        Some("recovered"),
        "a malformed duplicate does not hide the string value"
    );
    assert_eq!(header_value(&headers, "missing"), None, "absent header");
    assert_eq!(
        header_value(&headers, "x-empty"),
        Some(String::new()),
        "empty value is still a value"
    );

    assert!(
        is_response_stage(&json!({"responseStatusCode": 200})),
        "status marks the response stage"
    );
    assert!(
        is_response_stage(&json!({"responseHeaders": []})),
        "headers mark the response stage"
    );
    assert!(
        is_response_stage(&json!({"responseErrorReason": "x"})),
        "error reason marks the response stage"
    );
    assert!(
        !is_response_stage(&json!({"request": {}})),
        "request stage is not a response stage"
    );
    assert!(
        !is_response_stage(&json!({})),
        "empty params are not a response stage"
    );
}

#[test]
fn download_manager_rejects_hostile_native_records() {
    let tmp = tempfile::tempdir().unwrap();
    let root = faktor_fs::RootedDir::create(&tmp.path().join("profiles")).unwrap();
    root.create_dir_all(std::path::Path::new("p1")).unwrap();
    let policy = DownloadPolicy {
        enabled: true,
        directory: Some("dl".into()),
        max_bytes: 16,
    };
    let manager = DownloadManager::new(policy, root, std::path::Path::new("p1")).unwrap();

    // A native announcement is denied and recorded Cancelled.
    let denial = manager
        .observe_native(
            "Browser.downloadWillBegin",
            &json!({
                "guid": "g1",
                "url": "https://first.test/file.pdf",
                "suggestedFilename": "../../evil.pdf",
                "frameId": "f1"
            }),
        )
        .expect("native announcement is denied");
    assert_eq!(denial.guid, "g1", "guid retained");
    let record = manager.records().pop().expect("record");
    assert_eq!(record.state, DownloadState::Cancelled, "cancelled state");
    assert_eq!(
        record.suggested_filename, "../../evil.pdf",
        "the hostile suggestion is recorded verbatim as evidence, never used as a path"
    );

    // Malformed field types: a missing guid rejects; non-string url and
    // filename default to empty (never panic, never used as a path).
    manager.observe_native(
        "Browser.downloadWillBegin",
        &json!({"guid": 7, "url": "https://x.test/a", "suggestedFilename": "a"}),
    );
    for (guid, url, filename) in [
        ("g2", json!(7), json!("a")),
        ("g3", json!("https://x.test/a"), json!(7)),
    ] {
        manager.observe_native(
            "Browser.downloadWillBegin",
            &json!({"guid": guid, "url": url, "suggestedFilename": filename}),
        );
    }
    let records = manager.records();
    let rejected = records
        .iter()
        .filter(|r| r.state == DownloadState::Rejected)
        .count();
    assert_eq!(
        rejected, 0,
        "a guid-less rejection is accounted, not recorded"
    );
    assert!(
        manager.stats().rejected_total >= 1,
        "rejected accounting counts the guid-less refusal: {:?}",
        manager.stats()
    );
    let g2 = records.iter().find(|r| r.guid == "g2").expect("g2 record");
    assert_eq!(
        g2.url, "",
        "a non-string url defaults to empty rather than being used"
    );
    assert_eq!(
        g2.state,
        DownloadState::Cancelled,
        "the record is still denied"
    );
    let g3 = records.iter().find(|r| r.guid == "g3").expect("g3 record");
    assert_eq!(
        g3.suggested_filename, "",
        "a non-string filename defaults to empty"
    );
    assert!(
        manager.last_error().is_some(),
        "last_error names the malformed field"
    );

    // A guid flood stays bounded by the record ring.
    for i in 0..1000u32 {
        manager.observe_native(
            "Browser.downloadWillBegin",
            &json!({
                "guid": format!("flood-{i}"),
                "url": "https://x.test/a",
                "suggestedFilename": "a"
            }),
        );
    }
    assert!(
        manager.records().len() <= 256,
        "record ring stays bounded: {}",
        manager.records().len()
    );

    // Native progress events without a prior announcement stay bounded too.
    manager.observe_native(
        "Browser.downloadProgress",
        &json!({
            "guid": "ghost",
            "receivedBytes": -1,
            "totalBytes": 1e30,
        }),
    );
    assert!(
        manager.records().len() <= 256,
        "progress flood stays bounded"
    );
}

// ------------------------------------------- interception / host patterns

#[test]
fn websocket_and_resource_type_interception_is_exact() {
    let destinations = DestinationPolicy::first_party_only(vec![
        HostPattern::parse("allowed.test").unwrap(),
        HostPattern::parse("*.cdn.test").unwrap(),
    ]);
    let blocking = InterceptionPolicy::new(destinations.clone()).with_block_websockets(true);
    let allowing = InterceptionPolicy::new(destinations).with_block_websockets(false);

    let blocked = blocking.decide("https://allowed.test/socket", "websocket");
    match blocked {
        InterceptionDecision::Block { reason } => {
            assert_eq!(
                reason,
                BlockReason::ResourceTypeBlocked,
                "websocket block reason"
            );
        }
        InterceptionDecision::Continue => panic!("websockets must be blocked with the flag on"),
    }
    for spelling in ["WebSocket", "WEBSOCKET", "websocket"] {
        assert!(
            !blocking
                .decide("https://allowed.test/s", spelling)
                .is_allowed(),
            "resource-type spelling {spelling:?} still blocked"
        );
    }
    // With the flag off, an allowed host continues for the exact CDP
    // spelling; a lowercase spelling parses as `Other`, which is
    // default-blocked (case is part of the CDP contract).
    assert!(
        allowing
            .decide("https://allowed.test/socket", "WebSocket")
            .is_allowed(),
        "websocket to a first-party host continues when not blocked"
    );
    assert_eq!(
        ResourceType::parse("websocket"),
        ResourceType::Other,
        "lowercase websocket is an unknown type"
    );
    match allowing.decide("https://allowed.test/socket", "websocket") {
        InterceptionDecision::Block { reason } => {
            assert_eq!(
                reason,
                BlockReason::ResourceTypeBlocked,
                "unknown lowercase websocket is default-blocked"
            );
        }
        InterceptionDecision::Continue => {
            panic!("lowercase websocket must be default-blocked as Other")
        }
    }
    // wss is outside the default scheme allowlist: the scheme gate wins.
    match allowing.decide("wss://allowed.test/socket", "WebSocket") {
        InterceptionDecision::Block { reason } => {
            assert_eq!(
                reason,
                BlockReason::SchemeNotAllowed,
                "wss scheme denied by default policy"
            );
        }
        InterceptionDecision::Continue => panic!("wss must be scheme-denied by default"),
    }
    // Unknown hosts stay blocked regardless of resource type.
    for resource in ["Document", "XHR", "Fetch", "Script"] {
        let decision = allowing.decide("https://evil.test/a", resource);
        match decision {
            InterceptionDecision::Block { reason } => {
                assert_eq!(
                    reason,
                    BlockReason::NotFirstParty,
                    "unknown host reason for {resource}"
                );
            }
            InterceptionDecision::Continue => {
                panic!("unknown host must be blocked for {resource}")
            }
        }
    }
    // Default-blocked resource types are dropped even on allowed hosts.
    for (resource, expected) in [
        ("Media", ResourceType::Media),
        ("Image", ResourceType::Image),
        ("Font", ResourceType::Font),
    ] {
        let decision = allowing.decide("https://allowed.test/a", resource);
        match decision {
            InterceptionDecision::Block { reason } => {
                assert_eq!(
                    reason,
                    BlockReason::ResourceTypeBlocked,
                    "default-blocked {resource}"
                );
            }
            InterceptionDecision::Continue => panic!("{resource} must be blocked by default"),
        }
        assert_eq!(
            ResourceType::parse(resource),
            expected,
            "resource-type parse for {resource}"
        );
    }
    // Script/document requests to a first-party host continue.
    for resource in ["Document", "Script", "XHR", "Fetch"] {
        assert!(
            allowing
                .decide("https://allowed.test/", resource)
                .is_allowed(),
            "first-party {resource} continues"
        );
    }
    // Wildcard suffix matches the apex and subdomains.
    assert!(
        allowing
            .decide("https://cdn.test/a.js", "Script")
            .is_allowed(),
        "wildcard apex"
    );
    assert!(
        allowing
            .decide("https://a.b.cdn.test/a.js", "Script")
            .is_allowed(),
        "wildcard subdomain"
    );
    assert!(
        !allowing
            .decide("https://cdn.test.evil.test/a.js", "Script")
            .is_allowed(),
        "suffix lookalike denied"
    );
}

#[test]
fn host_pattern_spoofs_never_match() {
    let exact = HostPattern::parse("example.com").unwrap();
    let suffix = HostPattern::parse("*.example.com").unwrap();
    for spoof in [
        "example.com.evil",
        "evil-example.com",
        "aexample.com",
        "example.comx",
        "not a host",
        "user@example.com",
        "",
        "example.com:443",
        "http://example.com",
        "*",
    ] {
        let _ = spoof;
        assert!(
            !exact.matches(spoof),
            "exact rule must not match spoof {spoof:?}"
        );
    }
    // Canonical-equivalent spellings DO match (trailing dot/IDN punycode).
    for canonical in ["example.com", "EXAMPLE.COM", "example.com."] {
        assert!(
            exact.matches(canonical),
            "exact rule must match canonical {canonical:?}"
        );
    }
    assert!(
        HostPattern::parse("exämple.com")
            .unwrap()
            .matches("xn--exmple-cua.com"),
        "IDN pattern matches punycode host"
    );
    // Suffix matches apex and subdomains, never lookalikes.
    for allowed in [
        "example.com",
        "a.example.com",
        "a.b.example.com",
        "EXAMPLE.COM.",
    ] {
        assert!(
            suffix.matches(allowed),
            "suffix rule must match {allowed:?}"
        );
    }
    for denied in [
        "example.com.evil",
        "evil-example.com",
        "aexample.com",
        "example.comx",
        "notexample.com",
    ] {
        assert!(
            !suffix.matches(denied),
            "suffix rule must not match {denied:?}"
        );
    }
    // The explicit `*` matches everything parseable, including literals.
    let any = HostPattern::parse("*").unwrap();
    for host in ["example.com", "127.0.0.1", "::1", "deep.sub.test"] {
        assert!(any.matches(host), "`*` must match {host:?}");
    }
    // Bad pattern syntax is refused.
    for bad in [
        "",
        " ",
        "*.",
        "$",
        "exa mple.com",
        "example.com:443",
        "http://x",
    ] {
        assert!(
            HostPattern::parse(bad).is_err(),
            "pattern {bad:?} must be refused"
        );
    }
    // Wildcards only in the documented position.
    assert!(
        HostPattern::parse("a.*.example.com").is_err(),
        "interior wildcard is refused"
    );
}

// --------------------------------------------------------- broker wiring

#[tokio::test]
async fn hostile_forward_requests_never_reach_the_upstream() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_upstream_full(seen.clone()).await;
    let broker = EgressBroker::start(upstream_config(upstream))
        .await
        .unwrap();

    let hostile = [
        // Port not in the allowlist.
        "CONNECT 127.0.0.1:8 HTTP/1.1\r\nHost: 127.0.0.1:8\r\n\r\n",
        // Unknown host.
        "GET http://evil.test/x HTTP/1.1\r\nHost: evil.test\r\n\r\n",
        // Unsupported scheme.
        "GET ftp://127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        // Userinfo in the absolute target.
        "GET http://user:pw@127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        // Bare LF inside the head.
        "GET http://127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\nX-A: b\nX-B: c\r\n\r\n",
        // Empty header name.
        "GET http://127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\n: v\r\n\r\n",
        // Header name with a space.
        "GET http://127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\nBad Header: v\r\n\r\n",
        // obs-fold continuation.
        "GET http://127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\nX-A: b\r\n c\r\n\r\n",
    ];
    for request in hostile {
        let response = send_raw(broker.addr(), request).await;
        let label = request.lines().next().unwrap_or("");
        assert!(
            !response.is_empty(),
            "hostile request {label:?} must get a response, got empty"
        );
        assert!(
            !response.starts_with("HTTP/1.1 2"),
            "hostile request {label:?} must be refused, got {response:?}"
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            0,
            "hostile request {label:?} reached the upstream"
        );
    }
    broker.shutdown().await;
}

#[tokio::test]
async fn hop_by_hop_headers_and_proxy_credentials_are_isolated() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_upstream_full(seen.clone()).await;
    let mut config = upstream_config(upstream);
    config.upstream = UpstreamSelector::new(Some(
        UpstreamProxy::new("127.0.0.1", upstream.port())
            .with_credentials(ProxyCredentials::new("up", "pw"))
            .with_address_policy(EgressAddressPolicy::LOCAL),
    ));
    let broker = EgressBroker::start(config).await.unwrap();

    let response = send_raw(
        broker.addr(),
        "GET http://127.0.0.1:9/x?q=1 HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Connection: keep-alive, X-Hop\r\nX-Hop: drop-me\r\n\
         Proxy-Connection: keep-alive\r\nKeep-Alive: timeout=5\r\nTE: trailers\r\n\
         Trailer: X-T\r\nUpgrade: websocket\r\n\
         Proxy-Authorization: Basic ZXZpbA==\r\n\
         X-Keep: kept\r\n\r\n",
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "valid forward must succeed: {response:?}"
    );
    let heads = seen.lock().unwrap().clone();
    assert_eq!(heads.len(), 1, "exactly one upstream request: {heads:?}");
    let head = heads[0].to_lowercase();
    for stripped in [
        "x-hop",
        "proxy-connection",
        "keep-alive",
        "te:",
        "trailer",
        "upgrade",
    ] {
        assert!(
            !head.contains(stripped),
            "hop-by-hop {stripped:?} reached upstream: {head}"
        );
    }
    assert!(
        !head.contains("zxzpb"), // base64("evil")
        "client-supplied proxy credentials reached upstream: {head}"
    );
    let expected_credential = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .encode("up:pw")
            .to_lowercase()
    };
    assert!(
        head.contains(&format!("proxy-authorization: basic {expected_credential}")),
        "the configured upstream credential is the only one: {head}"
    );
    assert!(
        head.contains("x-keep: kept"),
        "end-to-end headers are preserved: {head}"
    );
    assert!(
        head.contains("host: 127.0.0.1"),
        "Host is preserved for the destination: {head}"
    );
    assert!(
        head.starts_with("get http://127.0.0.1:9/x?q=1 http/1.1"),
        "the absolute target is preserved for the upstream proxy: {head}"
    );
    broker.shutdown().await;
}

#[tokio::test]
async fn connect_tunnel_framing_and_closure() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_upstream_full(seen.clone()).await;
    let broker = EgressBroker::start(upstream_config(upstream))
        .await
        .unwrap();

    let response = send_raw(
        broker.addr(),
        "CONNECT 127.0.0.1:9 HTTP/1.1\r\nHost: 127.0.0.1:9\r\n\r\n",
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "allowed CONNECT must establish: {response:?}"
    );
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "exactly one upstream CONNECT"
    );
    let head = seen.lock().unwrap()[0].to_lowercase();
    assert!(
        head.starts_with("connect 127.0.0.1:9 http/1.1"),
        "authority-form CONNECT is preserved: {head}"
    );
    assert!(
        !head.contains("proxy-authorization"),
        "a credential-free CONNECT must not invent one: {head}"
    );

    // A CONNECT with a body must not be smuggled into the tunnel: it is
    // refused before the upstream leg.
    let before = seen.lock().unwrap().len();
    let response = send_raw(
        broker.addr(),
        "CONNECT 127.0.0.1:9 HTTP/1.1\r\nHost: 127.0.0.1:9\r\n\
         Content-Length: 4\r\n\r\nbody",
    )
    .await;
    assert!(
        !response.starts_with("HTTP/1.1 2"),
        "a CONNECT with a body must be refused: {response:?}"
    );
    assert_eq!(
        seen.lock().unwrap().len(),
        before,
        "the CONNECT-with-body never opened an upstream leg"
    );

    // CONNECT to a non-allowlisted port is refused before upstream.
    let before = seen.lock().unwrap().len();
    let response = send_raw(
        broker.addr(),
        "CONNECT 127.0.0.1:8 HTTP/1.1\r\nHost: 127.0.0.1:8\r\n\r\n",
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 403"),
        "port-not-allowed CONNECT must be 403: {response:?}"
    );
    assert_eq!(
        seen.lock().unwrap().len(),
        before,
        "refused CONNECT never reached the upstream"
    );
    broker.shutdown().await;
}

// --------------------------------------------------------------- profiles

#[test]
fn profile_names_refuse_separators_and_class_spoofs() {
    let accepted = ["a", "p_1", "procurement-cn", &"x".repeat(64)];
    for name in accepted {
        assert!(
            faktor_browser::validate_profile_name(name).is_ok(),
            "profile name {name:?} must be accepted"
        );
    }
    let rejected = [
        "",
        "A",
        "Upper",
        "with space",
        "with.dot",
        ".",
        "..",
        "../escape",
        "..\\escape",
        "a/b",
        "a\\b",
        "a\0b",
        "a\nb",
        "café",
        "😀",
        "-leading-dash-is-fine?", // '?' refused
        &"x".repeat(65) as &str,
    ];
    for name in rejected {
        assert!(
            faktor_browser::validate_profile_name(name).is_err(),
            "profile name {name:?} must be refused"
        );
    }
}

#[test]
fn profile_store_refuses_symlink_swaps_including_cookie_and_incognito() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("marker"), b"keep").unwrap();
        let root = tmp.path().join("profiles");
        let store = ProfileStore::open(root.clone()).unwrap();

        // A profile entry swapped for a symlink is refused everywhere.
        symlink(&outside, root.join("p1")).unwrap();
        assert!(
            store.profile_dir("p1").is_err(),
            "profile_dir through a symlink must be refused"
        );
        assert!(
            store.scratch_dir("p1").is_err(),
            "scratch_dir through a symlink must be refused"
        );
        assert!(
            store.cookie_store_path("p1").is_err(),
            "cookie_store_path through a symlink must be refused"
        );
        assert!(
            store.wipe("p1").is_err(),
            "wipe through a symlink must be refused"
        );
        assert!(!store.exists("p1"), "a symlinked profile never exists");
        assert!(
            outside.join("marker").exists(),
            "the symlink target must be untouched"
        );
        assert!(
            !outside.join("scratch").exists() && !outside.join("Default").exists(),
            "no directory was created through the swap"
        );

        // Replacing the ROOT path with a symlink after open: the anchored
        // authority keeps operating on the original directory.
        let real_root = tmp.path().join("profiles-real");
        std::fs::rename(&root, &real_root).unwrap();
        symlink(&outside, &root).unwrap();
        let _ = store.profile_dir("p2");
        assert!(
            !outside.join("p2").exists(),
            "the swapped root path must never receive profile data"
        );
        assert!(
            real_root.join("p2").exists(),
            "the anchored original root receives the profile"
        );

        // The `.incognito` container swapped for a symlink is refused.
        std::fs::remove_file(&root).unwrap();
        std::fs::rename(&real_root, &root).unwrap();
        symlink(&outside, root.join(".incognito")).unwrap();
        assert!(
            store.incognito("probe").is_err(),
            "incognito through a symlinked container must be refused"
        );
        assert!(
            !outside.join("scratch").exists() && outside.join("marker").exists(),
            "incognito swap leaves the target untouched"
        );
    }
}
