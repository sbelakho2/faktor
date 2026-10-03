//! Adversarial destination/egress/DNS tests (task category 5, security leg).
//!
//! Production entry points: [`DestinationRule::parse`],
//! [`DestinationPolicy`], [`RequestTarget`], [`canonicalize_request_host`],
//! [`classify_ip`] and [`vet_resolved_answers`].

use super::destination::{
    canonicalize_request_host, Decision, DestinationPolicy, DestinationRule, HostMatcher,
    RequestTarget, RuleMatch, Scheme,
};
use super::network::{classify_ip, vet_resolved_answers, AddressClass, EgressAddressPolicy};
use std::net::{IpAddr, SocketAddr};

fn policy(entries: &[&str]) -> DestinationPolicy {
    DestinationPolicy::parse_lines(entries.iter().copied())
        .unwrap_or_else(|e| panic!("fixture policy must parse: {entries:?}: {e}"))
}

fn allowed(p: &DestinationPolicy, scheme: &str, host: &str, port: u16) -> bool {
    p.check(scheme, host, port, false, None).is_allowed()
}

type RuleFixture = (
    &'static str,
    Option<Scheme>,
    Option<&'static str>,
    Option<&'static str>,
    Option<u16>,
);

#[test]
fn supported_rule_forms_parse_to_exact_triples() {
    // (entry, scheme, exact host, wildcard suffix, port)
    let cases: Vec<RuleFixture> = vec![
        ("example.com", None, Some("example.com"), None, None),
        (
            "example.com:443",
            None,
            Some("example.com"),
            None,
            Some(443),
        ),
        (
            "https://example.com",
            Some(Scheme::Https),
            Some("example.com"),
            None,
            None,
        ),
        (
            "HTTPS://EXAMPLE.COM",
            Some(Scheme::Https),
            Some("example.com"),
            None,
            None,
        ),
        (
            "https://example.com:443",
            Some(Scheme::Https),
            Some("example.com"),
            None,
            Some(443),
        ),
        (
            "ws://example.com:8080",
            Some(Scheme::Ws),
            Some("example.com"),
            None,
            Some(8080),
        ),
        (
            "wss://example.com",
            Some(Scheme::Wss),
            Some("example.com"),
            None,
            None,
        ),
        ("*.example.com", None, None, Some("example.com"), None),
        (
            "https://*.example.com:443",
            Some(Scheme::Https),
            None,
            Some("example.com"),
            Some(443),
        ),
        ("127.0.0.1", None, Some("127.0.0.1"), None, None),
        (
            "127.000.000.001:80",
            None,
            Some("127.0.0.1"),
            None,
            Some(80),
        ),
        (
            "http://127.0.0.1:9911",
            Some(Scheme::Http),
            Some("127.0.0.1"),
            None,
            Some(9911),
        ),
        ("[::1]", None, Some("::1"), None, None),
        ("[::1]:8080", None, Some("::1"), None, Some(8080)),
        (
            "http://[2001:0DB8:0:0:0:0:0:1]:80",
            Some(Scheme::Http),
            Some("2001:db8::1"),
            None,
            Some(80),
        ),
        ("exämple.com", None, Some("xn--exmple-cua.com"), None, None),
        ("EXAMPLE.COM.", None, Some("example.com"), None, None),
        ("*.EXAMPLE.com", None, None, Some("example.com"), None),
        (
            "local-service:65535",
            None,
            Some("local-service"),
            None,
            Some(65535),
        ),
        ("a-b.c-d.example", None, Some("a-b.c-d.example"), None, None),
    ];
    for (entry, scheme, host, wildcard, port) in cases {
        let rule = DestinationRule::parse(entry)
            .unwrap_or_else(|e| panic!("rule must parse: {entry:?}: {e}"));
        assert_eq!(&rule.scheme, &scheme, "scheme of {entry:?}");
        assert_eq!(&rule.port, &port, "port of {entry:?}");
        match (&rule.host, host, wildcard) {
            (HostMatcher::Exact { host: got }, Some(want), None) => {
                assert_eq!(got, want, "exact host of {entry:?}")
            }
            (HostMatcher::WildcardSubdomain { suffix }, None, Some(want)) => {
                assert_eq!(suffix, want, "wildcard suffix of {entry:?}")
            }
            other => panic!("host matcher shape of {entry:?}: {other:?}"),
        }
        assert_eq!(rule.text, entry.trim(), "original text of {entry:?}");
    }
}

#[test]
fn malformed_rule_entries_are_refused_with_a_reason() {
    let cases: Vec<(&str, &str)> = vec![
        ("", "empty"),
        ("   ", "empty"),
        ("\t", "empty"),
        ("example.com/a", "path"),
        ("https://example.com/path", "path"),
        ("https://example.com/", "path"),
        ("exa mple.com", "whitespace"),
        ("example.com :443", "whitespace"),
        ("http://*.com", "leading label"),
        ("*.com", "leading label"),
        ("*.", "suffix"),
        ("*", "wildcard"),
        ("**.example.com", "wildcard"),
        ("*.*.example.com", "wildcard"),
        ("https://*.127.0.0.1", "IP literal"),
        ("*.127.0.0.1", "IP literal"),
        ("example.com:0", "port"),
        ("example.com:65536", "range"),
        ("example.com:99999", "range"),
        ("example.com:abc", "number"),
        ("example.com:", "empty port"),
        (":443", "empty host"),
        ("http://", "empty host"),
        ("ftp://example.com", "unsupported scheme"),
        ("file://example.com", "unsupported scheme"),
        ("1", "not a hostname"),
        ("999.1.2.3", "IPv4 literal"),
        ("256.0.0.1", "IPv4 literal"),
        ("example..com", "empty label"),
        ("-example.com", "start/end with '-'"),
        ("example-.com", "start/end with '-'"),
        ("foo_bar.com", "label"),
        ("exa!mple.com", "label"),
        ("::1", "bracketed"),
        ("[::1", "bracketed"),
        ("::1]", "']' without a leading '['"),
        ("[::1]x", "unexpected characters"),
        ("[::1]:0", "port"),
        ("[not-an-ip]", "invalid IPv6"),
        ("http://user@example.com", "label"),
        ("http://user:pass@example.com", "port"),
        ("example.com#frag", "label"),
        ("example.com?q=1", "label"),
    ];
    for (entry, needle) in cases {
        let err =
            DestinationRule::parse(entry).expect_err(&format!("rule must be refused: {entry:?}"));
        assert!(
            err.reason.to_lowercase().contains(&needle.to_lowercase()),
            "refusal reason for {entry:?} must mention {needle:?}: {err}"
        );
        assert_eq!(err.entry, entry, "refusal carries the entry {entry:?}");
    }
}

#[test]
fn duplicate_rules_are_configuration_errors() {
    let dupes: Vec<Vec<&str>> = vec![
        vec!["example.com", "example.com"],
        vec!["example.com:443", "example.com:443"],
        vec!["https://example.com", "https://example.com"],
        vec!["*.example.com", "*.example.com"],
        vec!["127.0.0.1", "127.0.0.1"],
        vec!["[::1]", "[::1]"],
        vec!["example.com", "EXAMPLE.COM."],
        vec!["127.0.0.1", "127.000.000.001"],
        vec!["exämple.com", "xn--exmple-cua.com"],
        vec!["a", "a", "b"],
    ];
    for entries in dupes {
        let err = DestinationPolicy::parse_lines(entries.iter().copied())
            .expect_err(&format!("duplicates must refuse: {entries:?}"));
        assert!(
            err.reason.contains("duplicate"),
            "duplicate reason for {entries:?}: {err}"
        );
        assert!(err.line >= 1, "line number for {entries:?}: {err}");
    }
    // Distinct scheme/port keys are not duplicates.
    let p = policy(&["example.com", "https://example.com", "example.com:443"]);
    assert_eq!(p.rules().len(), 3, "distinct triples coexist");
    // from_rules refuses duplicates through the same canonical key.
    let a = DestinationRule::parse("example.com").unwrap();
    let b = DestinationRule::parse("EXAMPLE.COM.").unwrap();
    let err = DestinationPolicy::from_rules(vec![a, b])
        .expect_err("from_rules must refuse a canonical duplicate");
    assert!(err.reason.contains("duplicate"), "reason: {err}");
}

#[test]
fn policy_decisions_are_full_triple_matches() {
    let cases: Vec<(&[&str], &str, &str, u16, bool)> = vec![
        (&["example.com"], "https", "example.com", 443, true),
        (&["example.com"], "http", "example.com", 80, true),
        (&["example.com"], "ws", "example.com", 80, true),
        (&["example.com"], "wss", "example.com", 443, true),
        (&["example.com"], "https", "example.com", 8443, true),
        (&["example.com"], "", "example.com", 443, false),
        (&["example.com:443"], "https", "example.com", 443, true),
        (&["example.com:443"], "http", "example.com", 443, true),
        (&["example.com:443"], "https", "example.com", 444, false),
        (&["https://example.com"], "https", "example.com", 1, true),
        (&["https://example.com"], "http", "example.com", 443, false),
        (&["https://example.com"], "wss", "example.com", 443, false),
        (&["*.example.com"], "https", "api.example.com", 443, true),
        (&["*.example.com"], "https", "a.b.example.com", 443, true),
        (&["*.example.com"], "https", "example.com", 443, false),
        (&["*.example.com"], "https", "evil-example.com", 443, false),
        (&["*.example.com"], "https", "example.com.evil", 443, false),
        (&["*.example.com"], "https", "xexample.com", 443, false),
        (
            &["https://*.example.com:443"],
            "https",
            "a.example.com",
            443,
            true,
        ),
        (
            &["https://*.example.com:443"],
            "https",
            "a.example.com",
            444,
            false,
        ),
        (
            &["https://*.example.com:443"],
            "http",
            "a.example.com",
            443,
            false,
        ),
        (&["example.com"], "https", "other.com", 443, false),
        (&["example.com"], "https", "EXAMPLE.COM", 443, true),
        (&["example.com"], "https", "example.com.", 443, true),
        (&["example.com"], "https", "example.com.evil", 443, false),
        (&["exämple.com"], "https", "xn--exmple-cua.com", 443, true),
        (&["xn--exmple-cua.com"], "https", "exämple.com", 443, true),
        (&["example.com"], "https", "examp1e.com", 443, false),
        (&["http://127.0.0.1:9911"], "http", "127.0.0.1", 9911, true),
        (&["http://127.0.0.1:9911"], "http", "127.0.0.1", 9912, false),
        (
            &["http://127.0.0.1:9911"],
            "http",
            "127.000.0.01",
            9911,
            true,
        ),
        (&["[::1]:8080"], "https", "::1", 8080, true),
        (&["[::1]:8080"], "https", "0:0:0:0:0:0:0:1", 8080, true),
        (&["[::1]:8080"], "https", "::1", 8081, false),
        (&["[2001:DB8::1]"], "https", "2001:db8:0:0::1", 443, true),
    ];
    for (entries, scheme, host, port, want) in cases {
        let p = policy(entries);
        assert_eq!(
            allowed(&p, scheme, host, port),
            want,
            "policy {entries:?} on {scheme}://{host}:{port}"
        );
    }
}

#[test]
fn deny_reasons_name_the_closest_rule_and_match_depth() {
    let p = policy(&["https://example.com:443", "other.com"]);
    // Host+port match, scheme misses.
    let decision = p.check("http", "example.com", 443, false, None);
    match decision {
        Decision::Denied(reason) => {
            assert_eq!(
                reason.rule_fired.as_deref(),
                Some("https://example.com:443"),
                "fired rule"
            );
            assert_eq!(reason.matched, RuleMatch::HostAndPort, "depth");
        }
        Decision::Allowed => panic!("http on the https rule must deny"),
    }
    // Host matches, port misses.
    let decision = p.check("https", "example.com", 444, false, None);
    match decision {
        Decision::Denied(reason) => {
            assert_eq!(
                reason.rule_fired.as_deref(),
                Some("https://example.com:443")
            );
            assert_eq!(reason.matched, RuleMatch::Host, "depth");
        }
        Decision::Allowed => panic!("port 444 must deny"),
    }
    // Nothing matches at all.
    let decision = p.check("https", "third.com", 443, false, None);
    match decision {
        Decision::Denied(reason) => {
            assert_eq!(reason.rule_fired, None, "no rule fired");
            assert_eq!(reason.matched, RuleMatch::None, "depth");
        }
        Decision::Allowed => panic!("unknown host must deny"),
    }
    // An unparseable host yields plain default-deny, never a panic.
    for junk in ["", " ", "not a host", "*", "..", "foo..bar", "user@host"] {
        let decision = p.check("https", junk, 443, false, None);
        match decision {
            Decision::Denied(reason) => {
                assert_eq!(reason.rule_fired, None, "junk host {junk:?}");
                assert_eq!(reason.matched, RuleMatch::None, "junk host {junk:?}");
            }
            Decision::Allowed => panic!("junk host {junk:?} must deny under an installed policy"),
        }
    }
    // An installed but empty policy denies everything.
    let empty = DestinationPolicy::empty();
    assert!(empty.is_empty(), "explicitly empty");
    for host in ["example.com", "127.0.0.1", "::1", "anything.test"] {
        assert!(
            !allowed(&empty, "https", host, 443),
            "empty policy must deny {host}"
        );
    }
    // A rule with no port never matches the 0 "unknown port" sentinel.
    let p = policy(&["example.com"]);
    assert!(
        !allowed(&p, "https", "example.com", 0),
        "port 0 is the no-port sentinel and rule port None is not 'any'"
    );
}

#[test]
fn wildcard_label_boundaries_are_exact() {
    let p = policy(&["*.example.com"]);
    let allowed_hosts = [
        "a.example.com",
        "a.b.example.com",
        "0.example.com",
        "a-b.example.com",
        "a.b.c.d.example.com",
        "xn--exmple-cua.example.com",
        "A.EXAMPLE.COM",
        "a.example.com.",
    ];
    for host in allowed_hosts {
        assert!(
            allowed(&p, "https", host, 443),
            "wildcard must allow subdomain {host:?}"
        );
    }
    let denied_hosts = [
        "example.com",
        "example.com.evil",
        "evil-example.com",
        "aexample.com",
        "a.example.com.evil",
        ".example.com",
        "a..example.com",
        "a.example.comx",
    ];
    for host in denied_hosts {
        assert!(
            !allowed(&p, "https", host, 443),
            "wildcard must deny {host:?}"
        );
    }
    // An exact rule is never a suffix rule.
    let exact = policy(&["example.com"]);
    for host in ["a.example.com", "evil-example.com", "example.com.evil"] {
        assert!(
            !allowed(&exact, "https", host, 443),
            "exact rule must deny suffix trick {host:?}"
        );
    }
}

#[test]
fn request_target_parse_rejects_credentials_and_hostile_spellings() {
    let accepted: Vec<(&str, &str, &str, Option<u16>)> = vec![
        ("https://example.com", "https", "example.com", Some(443)),
        ("HTTP://EXAMPLE.COM", "http", "example.com", Some(80)),
        (
            "https://example.com:8443",
            "https",
            "example.com",
            Some(8443),
        ),
        (
            "https://example.com/some/path?q=1#frag",
            "https",
            "example.com",
            Some(443),
        ),
        (
            "wss://api.example.com:443/ws",
            "wss",
            "api.example.com",
            Some(443),
        ),
        ("example.com", "", "example.com", None),
        ("example.com:8080", "", "example.com", Some(8080)),
        ("EXAMPLE.COM.", "", "example.com", None),
        ("xn--exmple-cua.com", "", "xn--exmple-cua.com", None),
        ("exämple.com", "", "xn--exmple-cua.com", None),
        ("127.0.0.1", "", "127.0.0.1", None),
        ("127.000.000.001", "", "127.0.0.1", None),
        ("127.1", "", "127.1", None),
        ("0x7f.0.0.1", "", "0x7f.0.0.1", None),
        ("[::1]", "", "::1", None),
        ("[::1]:8080", "", "::1", Some(8080)),
        (
            "http://[2001:DB8::1]:80/path",
            "http",
            "2001:db8::1",
            Some(80),
        ),
        ("  https://example.com  ", "https", "example.com", Some(443)),
    ];
    for (text, scheme, host, port) in accepted {
        let target =
            RequestTarget::parse(text).unwrap_or_else(|e| panic!("must parse {text:?}: {e}"));
        assert_eq!(target.scheme, scheme, "scheme of {text:?}");
        assert_eq!(target.host, host, "host of {text:?}");
        assert_eq!(target.port, port, "port of {text:?}");
    }

    let rejected: Vec<(&str, &str)> = vec![
        ("", "empty"),
        ("   ", "empty"),
        ("ftp://example.com", "unsupported scheme"),
        ("file:///etc/passwd", "unsupported scheme"),
        ("https://user:pass@example.com/", "port"),
        ("https://user@example.com/", "not a valid hostname"),
        ("https://example.com:0", "port 0"),
        ("https://example.com:65536", "out of range"),
        ("https://example.com:abc", "numeric"),
        ("https://example.com:", "empty port"),
        ("https://:443", "empty host"),
        ("https://", "empty host"),
        ("example.com/path", "bare host"),
        ("example.com?q=1", "query or fragment"),
        ("::1", "bracketed"),
        ("http://[::1]x", "unexpected characters"),
        ("http://[::1]:0", "port 0"),
        ("http://[not-ip]", "invalid IPv6"),
        ("http://exa mple.com", "hostname"),
        ("http://foo_bar.com", "label"),
        ("http://-bad.com", "start/end"),
        ("http://a..b", "empty label"),
        ("http://999.0.0.1", "IPv4 literal"),
        ("http://\u{0}.example.com", "label"),
    ];
    for (text, needle) in rejected {
        let err = RequestTarget::parse(text)
            .expect_err(&format!("destination must be refused: {text:?}"));
        assert!(
            err.to_lowercase().contains(&needle.to_lowercase()),
            "reason for {text:?} must mention {needle:?}: {err}"
        );
    }
    // Over-long destinations are refused before parsing.
    let long = format!("https://{}.com", "a".repeat(5000));
    assert!(
        RequestTarget::parse(&long).is_err(),
        "a 5000-char destination must be refused"
    );
    let long_rule = "a".repeat(5000);
    assert!(
        DestinationRule::parse(&long_rule).is_err(),
        "a 5000-char rule must be refused"
    );
}

#[test]
fn from_parts_normalizes_scheme_case_and_default_ports() {
    let t = RequestTarget::from_parts(Some("HTTPS"), "EXAMPLE.COM.", None, false, None).unwrap();
    assert_eq!(t.scheme, "https", "scheme lowercased");
    assert_eq!(t.host, "example.com", "host canonicalized");
    assert_eq!(t.port, Some(443), "scheme default port");
    let t =
        RequestTarget::from_parts(Some("http"), "example.com", Some(8080), false, None).unwrap();
    assert_eq!(t.port, Some(8080), "explicit port wins over default");
    let t = RequestTarget::from_parts(None, "example.com", None, false, None).unwrap();
    assert_eq!(t.scheme, "", "unknown scheme");
    assert_eq!(t.port, None, "unknown port without a scheme");
    let t = RequestTarget::from_parts(Some("ws"), "example.com", None, false, None).unwrap();
    assert_eq!(t.port, Some(80), "ws default");
    let t = RequestTarget::from_parts(Some("wss"), "example.com", None, false, None).unwrap();
    assert_eq!(t.port, Some(443), "wss default");
    assert!(
        RequestTarget::from_parts(Some("ftp"), "example.com", None, false, None).is_err(),
        "unsupported scheme refuses"
    );
    assert!(
        RequestTarget::from_parts(None, "bad host", None, false, None).is_err(),
        "junk host refuses"
    );
    // The literal-IP hint wins over textual spelling.
    let t = RequestTarget::from_parts(
        Some("http"),
        "127.000.0.01",
        Some(80),
        true,
        Some([127, 0, 0, 1]),
    )
    .unwrap();
    assert_eq!(t.host, "127.0.0.1", "IPv4 hint canonicalizes octets");
    assert!(t.is_ipv4, "is_ipv4 preserved");
    assert_eq!(t.ip, Some([127, 0, 0, 1]), "ip preserved");
}

#[test]
fn canonicalize_request_host_edges() {
    type CanonFixture = (
        &'static str,
        bool,
        Option<[u8; 4]>,
        Result<&'static str, &'static str>,
    );
    let cases: Vec<CanonFixture> = vec![
        ("example.com", false, None, Ok("example.com")),
        ("EXAMPLE.COM", false, None, Ok("example.com")),
        ("example.com.", false, None, Ok("example.com")),
        ("a.b.c.example.com", false, None, Ok("a.b.c.example.com")),
        ("127.0.0.1", false, None, Ok("127.0.0.1")),
        ("127.000.000.001", false, None, Ok("127.0.0.1")),
        ("255.255.255.255", false, None, Ok("255.255.255.255")),
        ("exämple.com", false, None, Ok("xn--exmple-cua.com")),
        ("XN--EXMPLE-CUA.COM", false, None, Ok("xn--exmple-cua.com")),
        ("::1", false, None, Ok("::1")),
        ("::FFFF:127.0.0.1", false, None, Ok("::ffff:127.0.0.1")),
        ("2001:0DB8:0:0:0:0:0:1", false, None, Ok("2001:db8::1")),
        ("1", false, None, Err("not a hostname")),
        ("127.1", false, None, Ok("127.1")), // name, never shorthand-expanded
        ("0x7f.0.0.1", false, None, Ok("0x7f.0.0.1")),
        ("not-an-ip", false, None, Ok("not-an-ip")),
        ("999.1.2.3", false, None, Err("IPv4")),
        ("bad host", false, None, Err("label")),
        ("foo_bar.com", false, None, Err("label")),
        ("-a.com", false, None, Err("start/end")),
        ("a-.com", false, None, Err("start/end")),
        ("a..b", false, None, Err("empty label")),
        ("*", false, None, Err("wildcard")),
        (".", false, None, Err("empty host")),
        ("..", false, None, Err("empty label")),
        ("", false, None, Err("empty host")),
    ];
    for (host, is_ipv4, ip, want) in cases {
        let got = canonicalize_request_host(host, is_ipv4, ip);
        match want {
            Ok(expect) => {
                assert_eq!(
                    got.as_deref(),
                    Ok(expect),
                    "canonicalization of {host:?} (is_ipv4={is_ipv4})"
                );
            }
            Err(needle) => {
                let err = got.expect_err(&format!("{host:?} must be refused"));
                assert!(
                    err.to_lowercase().contains(&needle.to_lowercase()),
                    "reason for {host:?} must mention {needle:?}: {err}"
                );
            }
        }
    }
    // Decimal/octal/hex shorthand for loopback is never expanded into an
    // address: it stays a name or is refused, so it cannot alias 127.0.0.1.
    for shorthand in ["2130706433", "017700000001", "0x7f000001"] {
        if let Ok(name) = canonicalize_request_host(shorthand, false, None) {
            assert_ne!(name, "127.0.0.1", "shorthand {shorthand:?} must not alias");
        }
    }
    assert_ne!(
        canonicalize_request_host("127.1", false, None).unwrap(),
        "127.0.0.1",
        "two-label shorthand stays a name"
    );
    // A 64-char label is refused, a 63-char label is fine.
    let label63 = format!("{}.com", "a".repeat(63));
    assert!(
        canonicalize_request_host(&label63, false, None).is_ok(),
        "63-char label is legal"
    );
    let label64 = format!("{}.com", "a".repeat(64));
    assert!(
        canonicalize_request_host(&label64, false, None).is_err(),
        "64-char label is refused"
    );
    // IDN confusables never canonicalize onto ASCII targets.
    for spoof in ["еxample.com", "exampie.com", "examp1e.com"] {
        let got = canonicalize_request_host(spoof, false, None)
            .unwrap_or_else(|e| panic!("spoof {spoof:?} parses as a name: {e}"));
        assert_ne!(got, "example.com", "confusable {spoof:?} must not alias");
    }
    // Fullwidth separators are UTS-46-mapped to '.', which is the IDNA
    // contract (not a confusable alias).
    assert_eq!(
        canonicalize_request_host("example．com", false, None).as_deref(),
        Ok("example.com"),
        "UTS-46 maps the fullwidth full stop"
    );
}

#[test]
fn special_range_ips_are_classified_and_never_global_by_embedding() {
    let cases: Vec<(&str, AddressClass)> = vec![
        ("8.8.8.8", AddressClass::Global),
        ("1.1.1.1", AddressClass::Global),
        ("93.184.216.34", AddressClass::Global),
        ("127.0.0.1", AddressClass::Loopback),
        ("127.255.255.254", AddressClass::Loopback),
        ("10.0.0.1", AddressClass::Private),
        ("172.16.0.1", AddressClass::Private),
        ("172.31.255.254", AddressClass::Private),
        ("192.168.1.1", AddressClass::Private),
        ("169.254.169.254", AddressClass::LinkLocal),
        ("100.64.0.1", AddressClass::Cgnat),
        ("192.0.2.1", AddressClass::Documentation),
        ("198.51.100.1", AddressClass::Documentation),
        ("203.0.113.1", AddressClass::Documentation),
        ("198.18.0.1", AddressClass::Benchmark),
        ("224.0.0.1", AddressClass::Multicast),
        ("239.255.255.255", AddressClass::Multicast),
        ("255.255.255.255", AddressClass::Reserved),
        ("0.0.0.0", AddressClass::Unspecified),
        ("0.1.2.3", AddressClass::Reserved),
        ("240.0.0.1", AddressClass::Reserved),
        ("192.88.99.1", AddressClass::Reserved),
        ("192.0.0.1", AddressClass::Reserved),
        ("192.0.0.8", AddressClass::Reserved),
        ("192.0.0.9", AddressClass::Global),
        ("192.0.0.10", AddressClass::Global),
        ("::", AddressClass::Unspecified),
        ("::1", AddressClass::Loopback),
        ("fe80::1", AddressClass::LinkLocal),
        ("fc00::1", AddressClass::Private),
        ("fd12:3456::1", AddressClass::Private),
        ("2001:db8::1", AddressClass::Documentation),
        ("3fff::1", AddressClass::Documentation),
        ("2001:2::1", AddressClass::Benchmark),
        ("ff02::1", AddressClass::Multicast),
        ("2001:4860:4860::8888", AddressClass::Global),
        ("2606:4700::1111", AddressClass::Global),
        ("4000::1", AddressClass::Reserved),
        ("::2", AddressClass::Reserved),
        ("5f00::1", AddressClass::Reserved),
        ("100::1", AddressClass::Reserved),
        ("100:0:0:1::1", AddressClass::Reserved),
        // Transition/tunneling forms are never classified by the embedded
        // IPv4 address: global-looking embeddings stay reserved.
        ("::ffff:8.8.8.8", AddressClass::Reserved),
        ("::ffff:127.0.0.1", AddressClass::Reserved),
        ("::ffff:169.254.169.254", AddressClass::Reserved),
        ("64:ff9b::808:808", AddressClass::Reserved),
        ("64:ff9b::7f00:1", AddressClass::Reserved),
        ("64:ff9b:1::a00:1", AddressClass::Reserved),
        ("2002:808:808::", AddressClass::Reserved),
        ("2002:7f00:1::", AddressClass::Reserved),
        ("2001::1", AddressClass::Reserved),
        ("0:0:0:0:0:0:0:1", AddressClass::Loopback),
    ];
    for (text, want) in cases {
        let ip: IpAddr = text
            .parse()
            .unwrap_or_else(|e| panic!("fixture IP {text:?}: {e}"));
        assert_eq!(classify_ip(ip), want, "classification of {text:?}");
    }
}

#[test]
fn mixed_and_rebinding_answer_sets_are_refused_whole() {
    fn sa(ip: &str, port: u16) -> SocketAddr {
        SocketAddr::new(ip.parse().unwrap(), port)
    }
    // A mixed public/private answer set is refused whole, naming the first
    // refused address — never filtered down to the permitted subset.
    let mixed = [
        sa("93.184.216.34", 443),
        sa("10.0.0.1", 443),
        sa("8.8.8.8", 443),
    ];
    let refusal = vet_resolved_answers(&mixed, EgressAddressPolicy::EXTERNAL)
        .expect_err("mixed answers must refuse");
    assert_eq!(refusal.address, sa("10.0.0.1", 443), "refused address");
    assert_eq!(refusal.class, AddressClass::Private, "refused class");
    // Rebinding-style answer change: the same hostname resolving to a
    // special class on the second answer is refused even in a
    // public-first list, under EXTERNAL and LOCAL alike (loopback is
    // LOCAL's explicit exception and is checked separately below).
    for second in [
        "127.0.0.1",
        "::1",
        "169.254.169.254",
        "::ffff:127.0.0.1",
        "10.0.0.1",
        "fe80::1",
    ] {
        let answers = [sa("93.184.216.34", 443), sa(second, 443)];
        assert!(
            vet_resolved_answers(&answers, EgressAddressPolicy::EXTERNAL).is_err(),
            "EXTERNAL must refuse a public+{second} answer set whole"
        );
    }
    for second in [
        "169.254.169.254",
        "::ffff:127.0.0.1",
        "10.0.0.1",
        "fe80::1",
        "64:ff9b::7f00:1",
    ] {
        let answers = [sa("93.184.216.34", 443), sa(second, 443)];
        assert!(
            vet_resolved_answers(&answers, EgressAddressPolicy::LOCAL).is_err(),
            "LOCAL must still refuse a public+{second} answer set whole"
        );
    }
    // LOCAL's one explicit exception: a public+loopback set is permitted.
    for second in ["127.0.0.1", "::1"] {
        let answers = [sa("93.184.216.34", 443), sa(second, 443)];
        assert!(
            vet_resolved_answers(&answers, EgressAddressPolicy::LOCAL).is_ok(),
            "LOCAL permits its explicit loopback rule with {second}"
        );
    }
    // Pure-global sets pass both policies; empty sets are the caller's rule.
    let global = [sa("93.184.216.34", 443), sa("2606:4700::1111", 443)];
    assert!(
        vet_resolved_answers(&global, EgressAddressPolicy::EXTERNAL).is_ok(),
        "pure global set passes EXTERNAL"
    );
    assert!(
        vet_resolved_answers(&[], EgressAddressPolicy::EXTERNAL).is_ok(),
        "empty set is the caller's operational rule"
    );
    // EXTERNAL refuses loopback; LOCAL permits it and nothing else.
    assert!(
        !EgressAddressPolicy::EXTERNAL.permits(sa("127.0.0.1", 1).ip()),
        "EXTERNAL refuses loopback"
    );
    assert!(
        EgressAddressPolicy::LOCAL.permits(sa("::1", 1).ip()),
        "LOCAL permits loopback"
    );
    let special: Vec<&str> = vec![
        "169.254.169.254",
        "10.0.0.1",
        "100.64.0.1",
        "192.0.2.1",
        "198.18.0.1",
        "224.0.0.1",
        "0.0.0.0",
        "192.0.0.1",
        "fe80::1",
        "fc00::1",
        "2001:db8::1",
        "ff02::1",
        "::ffff:127.0.0.1",
        "64:ff9b::7f00:1",
        "2002:7f00:1::",
    ];
    for text in special {
        let addr = sa(text, 443);
        assert!(
            !EgressAddressPolicy::LOCAL.permits(addr.ip()),
            "LOCAL must still refuse {text}"
        );
        assert!(
            vet_resolved_answers(&[addr], EgressAddressPolicy::LOCAL).is_err(),
            "vet must refuse {text} even under LOCAL"
        );
    }
    assert!(
        EgressAddressPolicy::LOCAL.allow_loopback(),
        "LOCAL carries the explicit loopback rule"
    );
    assert!(
        !EgressAddressPolicy::EXTERNAL.allow_loopback(),
        "EXTERNAL carries no loopback exception"
    );
    assert!(
        EgressAddressPolicy::default() == EgressAddressPolicy::EXTERNAL,
        "default is external"
    );
    assert_eq!(
        EgressAddressPolicy::from_allow_loopback(true),
        EgressAddressPolicy::LOCAL,
        "the explicit flag builds LOCAL"
    );
    assert_eq!(AddressClass::Global.as_str(), "global", "stable label");
    assert_eq!(
        AddressClass::LinkLocal.as_str(),
        "link_local",
        "stable label"
    );
}
