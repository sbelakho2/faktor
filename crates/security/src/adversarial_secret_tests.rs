//! Adversarial redaction/secret tests (task category 6).
//!
//! Every case is table-driven and individually asserted with a message
//! naming the input. Production entry points only: [`scan_secrets`],
//! [`redact`], [`CompiledSecretPolicy`], [`validate_pattern`],
//! [`SecretRegistry`] and the payload [`Scanner`].

use super::*;
use crate::payload::{
    scan_payload, scan_payload_compiled, FeedStatus, ScanOutcome, ScanPolicy, Scanner,
};
use crate::registry::{ExactSecretSource, SecretRegistry, CONFIGURED_SECRET_KIND};
use std::sync::Arc;

fn sk(n: usize) -> String {
    format!("sk-{}", "A".repeat(n))
}
fn ghp(n: usize) -> String {
    format!("ghp_{}", "B".repeat(n))
}
fn aiza(n: usize) -> String {
    format!("AIza{}", "c".repeat(n))
}
fn aws(n: usize) -> String {
    format!("AKIA{}", "D".repeat(n))
}
fn slack(n: usize) -> String {
    format!("xoxb-{}", "e".repeat(n))
}

#[test]
fn default_kinds_are_detected_with_exact_offsets() {
    // (text, expected kind, expected offset, expected len)
    let pem = "-----BEGIN RSA PRIVATE KEY-----";
    let cases: Vec<(String, &str, usize, usize)> = vec![
        (sk(20), "openai_key", 0, 23),
        (format!("x {} y", sk(20)), "openai_key", 2, 23),
        (format!("pre{}!", sk(20)), "openai_key", 3, 23),
        // The greedy run keeps consuming trailing alphanumerics: the hit
        // is the maximal class run, which is the documented semantics.
        (format!("pre{}post", sk(20)), "openai_key", 3, 27),
        (ghp(20), "github_token", 0, 24),
        (aws(16), "aws_key", 0, 20),
        (slack(10), "slack_token", 0, 15),
        (pem.to_string(), "pem_private_key", 0, pem.len()),
        (aiza(20), "google_api_key", 0, 24),
    ];
    for (text, kind, offset, len) in cases {
        let hits = scan_secrets(&text, &SecretPolicy::default());
        assert_eq!(hits.len(), 1, "exactly one hit for {text:?}: {hits:?}");
        assert_eq!(hits[0].kind, kind, "kind for {text:?}");
        assert_eq!(hits[0].offset, offset, "offset for {text:?}");
        assert_eq!(hits[0].len, len, "len for {text:?}");
        assert_eq!(
            hits[0].redacted,
            format!("<redacted:{kind}>"),
            "replacement for {text:?}"
        );
        assert!(!hits[0].snippet.is_empty(), "snippet for {text:?}");
    }
    // The two-secret string yields two ordered hits.
    let two = format!("{} {}", sk(20), ghp(20));
    let hits = scan_secrets(&two, &SecretPolicy::default());
    assert_eq!(hits.len(), 2, "two distinct secrets: {hits:?}");
    assert_eq!(hits[1].kind, "github_token", "second hit in {two:?}");
    assert_eq!(hits[1].offset, 24, "second offset in {two:?}");
}

#[test]
fn minimum_run_boundaries_are_exact() {
    // (text, must_hit)
    let cases: Vec<(String, bool)> = vec![
        (sk(19), false),
        (sk(20), true),
        (sk(21), true),
        (format!("SK-{}", "A".repeat(20)), true),
        (format!("Sk-{}", "A".repeat(20)), true),
        (format!("sk-{}", "a".repeat(20)), true),
        (format!("sk-{}", "7".repeat(20)), true),
        (format!("sk-{}", "Z".repeat(20)), true),
        (format!("sk-{}", "Z".repeat(19)), false),
        (format!("sk-{}_{}", "A".repeat(10), "B".repeat(20)), false),
        (format!("sk-{}", "é".repeat(21)), false),
        (ghp(19), false),
        (ghp(20), true),
        (format!("GHP_{}", "B".repeat(20)), true),
        (aws(15), false),
        (aws(16), true),
        (aws(17), true),
        (format!("akia{}", "d".repeat(16)), true),
        (format!("AKIA{}", "d".repeat(15)), false),
        (slack(9), false),
        (slack(10), true),
        (format!("xoxa-{}", "e".repeat(10)), true),
        (format!("xoxz-{}", "e".repeat(10)), false),
        (format!("xoxB-{}", "e".repeat(10)), true),
        ("-----BEGIN RSA PRIVATE KEY-----".into(), true),
        ("-----BEGIN OPENSSH PRIVATE KEY-----".into(), true),
        ("-----BEGIN EC PRIVATE KEY-----".into(), true),
        ("-----BEGIN DSA PRIVATE KEY-----".into(), true),
        ("-----BEGIN PGP PRIVATE KEY-----".into(), false),
        ("-----BEGIN PRIVATE KEY-----".into(), false),
        ("-----BEGIN RSA PRIVATE KEY----".into(), false),
        (aiza(19), false),
        (aiza(20), true),
        (format!("aiza{}", "c".repeat(20)), true),
    ];
    for (text, must_hit) in cases {
        let hits = scan_secrets(&text, &SecretPolicy::default());
        assert_eq!(
            !hits.is_empty(),
            must_hit,
            "hit expectation for {text:?}: {hits:?}"
        );
    }
}

#[test]
fn overlapping_patterns_choose_earliest_then_first_pattern() {
    // Same start, different patterns: the earlier pattern index wins and
    // the trailing overlap is consumed (no double report).
    let policy = SecretPolicy {
        scan_enabled: true,
        block_on_secret: true,
        key_patterns: vec!["secretsecret".into(), "secret".into()],
    };
    let hits = scan_secrets("secretsecret!", &policy);
    assert_eq!(hits.len(), 1, "same-start overlap must collapse: {hits:?}");
    assert_eq!(hits[0].pattern_index, 0, "first pattern wins at one start");
    assert_eq!(hits[0].len, 12, "longest same-start span is kept");

    // Different starts: the earliest start wins even when a later pattern
    // would match longer.
    let policy = SecretPolicy {
        scan_enabled: true,
        block_on_secret: true,
        key_patterns: vec!["abcde".into(), "bcdefgh".into()],
    };
    let hits = scan_secrets("abcdefgh", &policy);
    assert_eq!(hits.len(), 1, "cross-start overlap: {hits:?}");
    assert_eq!(hits[0].pattern_index, 0, "earliest start wins");
    assert_eq!(hits[0].offset, 0);
    assert_eq!(hits[0].len, 5, "later longer pattern is dropped");

    // A hit consumes its span: a second pattern inside it is not reported.
    let embedded = format!("{} {}", sk(20), ghp(20));
    let hits = scan_secrets(&embedded, &SecretPolicy::default());
    assert_eq!(hits.len(), 2, "space-separated defaults: {hits:?}");
    // No separator: the greedy sk- run swallows the ghp_ prefix letters and
    // the second pattern can no longer start at an unconsumed offset.
    let swallowed = format!("x{}y{}", sk(20), ghp(20));
    let hits = scan_secrets(&swallowed, &SecretPolicy::default());
    assert_eq!(
        hits.len(),
        1,
        "a consumed span shadows the following pattern: {hits:?}"
    );
    assert_eq!(hits[0].kind, "openai_key", "first pattern owns the span");
    let inside = format!("sk-{}", "AIza".repeat(20));
    let hits = scan_secrets(&inside, &SecretPolicy::default());
    assert_eq!(
        hits.len(),
        1,
        "AIza runs inside a single sk- hit are shadowed: {hits:?}"
    );
    assert_eq!(
        hits[0].kind, "openai_key",
        "the earliest pattern consumes the whole run: {hits:?}"
    );

    // A sub-secret pattern inside a longer first hit never double-counts.
    let policy = SecretPolicy {
        scan_enabled: true,
        block_on_secret: true,
        key_patterns: vec!["[a-z0-9]{8,}".into(), "bcdef".into()],
    };
    let hits = scan_secrets("abcdefghij", &policy);
    assert_eq!(hits.len(), 1, "consumed span: {hits:?}");
    assert_eq!(hits[0].pattern_index, 0);
}

#[test]
fn multibyte_boundaries_keep_byte_offsets_valid() {
    let key = sk(20);
    let text = format!("é{key}漢字");
    let hits = scan_secrets(&text, &SecretPolicy::default());
    assert_eq!(hits.len(), 1, "multibyte wrap: {hits:?}");
    assert_eq!(hits[0].offset, 2, "é is two bytes, {text:?}");
    assert_eq!(hits[0].len, key.len(), "secret byte length in {text:?}");
    assert!(
        text.is_char_boundary(hits[0].offset),
        "offset must sit on a char boundary: {text:?}"
    );
    assert!(
        text.is_char_boundary(hits[0].offset + hits[0].len),
        "end must sit on a char boundary: {text:?}"
    );

    for (pre, post) in [
        ("😀", "😀"),
        ("\u{200b}", "\u{feff}"),
        ("é", "ß"),
        ("日本語", "한국어"),
        ("\u{0301}", "\u{0301}"),
    ] {
        let text = format!("{pre}{key}{post}");
        let hits = scan_secrets(&text, &SecretPolicy::default());
        assert_eq!(hits.len(), 1, "wrap {pre:?}/{post:?}: {hits:?}");
        assert_eq!(
            hits[0].offset,
            pre.len(),
            "byte offset after {pre:?} in {text:?}"
        );
        assert!(
            hits[0].len == key.len(),
            "key length after {pre:?} in {text:?}"
        );
    }

    // A decoy multibyte run before the secret does not truncate scanning.
    let buried = format!("{}{}", "😀".repeat(100), sk(20));
    let hits = scan_secrets(&buried, &SecretPolicy::default());
    assert_eq!(hits.len(), 1, "buried after emoji flood: {hits:?}");
    assert_eq!(
        hits[0].offset,
        400,
        "each 😀 is 4 bytes, offset in a {}-byte text",
        buried.len()
    );
}

#[test]
fn redact_removes_every_default_kind_and_leaves_no_survivor() {
    let pem = "-----BEGIN OPENSSH PRIVATE KEY-----";
    let cases: Vec<(String, &str)> = vec![
        (sk(20), "openai_key"),
        (format!("a {} b", sk(20)), "openai_key"),
        (ghp(20), "github_token"),
        (aws(16), "aws_key"),
        (slack(10), "slack_token"),
        (pem.to_string(), "pem_private_key"),
        (aiza(20), "google_api_key"),
        (format!("{}{}{}", sk(20), ghp(20), aws(16)), "openai_key"),
    ];
    for (text, kind) in cases {
        let cleaned = redact(&text, &SecretPolicy::default());
        assert!(
            scan_secrets(&cleaned, &SecretPolicy::default()).is_empty(),
            "no survivor after redacting {text:?}: {cleaned:?}"
        );
        assert!(
            cleaned.contains(&format!("<redacted:{kind}>")),
            "kind marker for {text:?}: {cleaned:?}"
        );
    }
    // Plain surrounding text is preserved byte-for-byte.
    let text = format!("keep-start {} keep-end", sk(20));
    let cleaned = redact(&text, &SecretPolicy::default());
    assert!(
        cleaned.starts_with("keep-start ") && cleaned.ends_with(" keep-end"),
        "surrounding text preserved: {cleaned:?}"
    );
    // A secret in a multibyte context redacts without corrupting UTF-8.
    let text = format!("é{}漢", sk(20));
    let cleaned = redact(&text, &SecretPolicy::default());
    assert!(
        cleaned.starts_with('é') && cleaned.ends_with('漢'),
        "multibyte context preserved: {cleaned:?}"
    );
    assert!(
        scan_secrets(&cleaned, &SecretPolicy::default()).is_empty(),
        "no survivor in multibyte context: {cleaned:?}"
    );
}

#[test]
fn redaction_cap_truncates_the_entire_unprocessed_tail() {
    let policy = SecretPolicy::default();
    let thirty_two: String = (0..32).map(|_| format!("{} ", sk(20))).collect();
    let cleaned = redact(&thirty_two, &policy);
    assert_eq!(
        cleaned.matches("<redacted:openai_key>").count(),
        32,
        "exactly 32 replacements under the cap"
    );
    assert!(
        !cleaned.contains(REDACTION_TRUNCATION_MARKER),
        "no marker at exactly the cap: {cleaned:?}"
    );

    let thirty_three: String = (0..33).map(|_| format!("{} ", sk(20))).collect();
    let cleaned = redact(&thirty_three, &policy);
    assert_eq!(
        cleaned.matches("<redacted:openai_key>").count(),
        32,
        "cap holds at 32 replacements"
    );
    assert!(
        cleaned.ends_with(REDACTION_TRUNCATION_MARKER),
        "tail beyond the cap becomes the marker: {cleaned:?}"
    );
    assert!(
        scan_secrets(&cleaned, &policy).is_empty(),
        "no survivor past the cap"
    );

    // A decoy prefix cannot shield a real secret behind the cap: the
    // 33rd+ occurrences are replaced by the marker, never copied raw.
    for decoys in [32usize, 64, 100] {
        let mut text: String = (0..decoys).map(|_| format!("{} ", sk(20))).collect();
        let real = ghp(20);
        text.push_str(&real);
        let cleaned = redact(&text, &policy);
        assert!(
            !cleaned.contains(&real),
            "real secret past {decoys} decoys must not survive: {cleaned:?}"
        );
        assert!(
            scan_secrets(&cleaned, &policy).is_empty(),
            "no survivor behind {decoys} decoys: {cleaned:?}"
        );
    }

    // 1000 hits stay bounded: 32 replacements + one marker.
    let flood: String = (0..1000).map(|_| format!("{} ", sk(20))).collect();
    let cleaned = redact(&flood, &policy);
    assert_eq!(
        cleaned.matches("<redacted:openai_key>").count(),
        32,
        "flood stays capped"
    );
    assert_eq!(
        cleaned.matches(REDACTION_TRUNCATION_MARKER).count(),
        1,
        "exactly one truncation marker on a flood"
    );
    assert!(
        cleaned.len() < 2000,
        "flood output stays bounded, got {} bytes",
        cleaned.len()
    );
}

#[test]
fn cross_pattern_overlap_redaction_has_no_winner_leak() {
    let policy = SecretPolicy::default();
    // sk- directly followed by an AKIA run: sk consumes 20 A's; AKIA can
    // still start after the consumed span.
    let text = format!("{} {}", sk(20), aws(16));
    let hits = scan_secrets(&text, &policy);
    assert_eq!(hits.len(), 2, "non-overlapping kinds: {hits:?}");
    let cleaned = redact(&text, &policy);
    assert!(cleaned.contains("<redacted:openai_key>"), "{cleaned:?}");
    assert!(cleaned.contains("<redacted:aws_key>"), "{cleaned:?}");
    assert!(scan_secrets(&cleaned, &policy).is_empty(), "{cleaned:?}");

    // A pattern starting before another but ending after it: earliest start
    // wins, so the later-start pattern must be fully removed by redaction.
    let custom = SecretPolicy {
        scan_enabled: true,
        block_on_secret: true,
        key_patterns: vec!["[A-Za-z0-9]{10,}".into(), "ZZZ[A-Za-z0-9]{3,}".into()],
    };
    let text = "aaaaaaaaaaZZZbbbb".to_string();
    let cleaned = redact(&text, &custom);
    assert!(
        !cleaned.contains("ZZZbbbb") && !cleaned.contains("aaaaaaaaaa"),
        "earliest start consumes the overlap: {cleaned:?}"
    );
    assert!(scan_secrets(&cleaned, &custom).is_empty(), "{cleaned:?}");
}

#[test]
fn disabled_and_empty_policies_are_inert() {
    let hits = scan_secrets(&sk(20), &SecretPolicy::default());
    assert_eq!(hits.len(), 1, "control: default policy detects");
    let off = SecretPolicy {
        scan_enabled: false,
        ..SecretPolicy::default()
    };
    assert!(
        scan_secrets(&sk(20), &off).is_empty(),
        "scan_enabled=false is inert"
    );
    assert_eq!(
        redact(&sk(20), &off),
        sk(20),
        "disabled redaction returns the input"
    );
    let no_patterns = SecretPolicy {
        scan_enabled: true,
        block_on_secret: true,
        key_patterns: vec![],
    };
    assert!(
        scan_secrets(&sk(20), &no_patterns).is_empty(),
        "no patterns = no hits"
    );
    assert_eq!(
        redact(&sk(20), &no_patterns),
        sk(20),
        "no patterns = unchanged input"
    );
    for empty in ["", " ", "\n\t", "é", "😀"] {
        assert!(
            scan_secrets(empty, &SecretPolicy::default()).is_empty(),
            "empty/whitespace input {empty:?} must yield no hits"
        );
        assert_eq!(
            redact(empty, &SecretPolicy::default()),
            empty,
            "redact preserves {empty:?}"
        );
    }
    // scan_enabled=false still compiles the strict gate (it is config).
    let compiled = CompiledSecretPolicy::try_from(off).expect("disabled policy compiles");
    assert!(
        compiled.scan_text(&sk(20)).is_empty(),
        "compiled disabled policy is inert"
    );
}

#[test]
fn strict_pattern_validation_accepts_only_the_documented_subset() {
    let accepted = [
        "sk-[A-Za-z0-9]{20,}",
        "ghp_[A-Za-z0-9]{20,}",
        "AKIA[0-9A-Z]{16}",
        "xox[baprs]-[A-Za-z0-9-]{10,}",
        "-----BEGIN (RSA|OPENSSH|EC|DSA) PRIVATE KEY-----",
        "AIza[0-9A-Za-z_-]{20,}",
        "literal",
        " ",
        "a[b]{2,}c",
        "(one|two|three)",
        "x[0-9]{1}",
        // `[a-]` is a class of two single members under the documented
        // grammar (a trailing '-' is a member here, not a range).
        "[a-]",
    ];
    for pattern in accepted {
        assert!(
            validate_pattern(pattern).is_ok(),
            "pattern must compile: {pattern:?}"
        );
    }
    let rejected = [
        "", "^start", "end$", "a.b", "a*", "a+", "a?", "a\\d", "a|b", "()", "[", "]", "[a", "[]",
        "{2}", "[a]{0}", "[a]{0,}", "[a]{1,2}", "(a|)", "(|a)", "(a|b", "a)", "[z-a]", "(a[b])",
        "a{}", "[a]b{2}",
    ];
    for pattern in rejected {
        let err =
            validate_pattern(pattern).expect_err(&format!("pattern must be refused: {pattern:?}"));
        assert_eq!(
            err.pattern, pattern,
            "refusal names the offending pattern {pattern:?}"
        );
        assert!(
            !err.reason.is_empty(),
            "refusal carries a reason for {pattern:?}"
        );
        assert!(
            format!("{err}").starts_with("secret pattern"),
            "display is self-describing for {pattern:?}: {err}"
        );
    }
}

#[test]
fn compiled_policy_matches_legacy_and_labels_custom_patterns() {
    let custom = SecretPolicy {
        scan_enabled: true,
        block_on_secret: true,
        key_patterns: vec!["tok-[a-z0-9]{4,}".into(), "bearer [A-Za-z0-9]{8,}".into()],
    };
    let compiled = CompiledSecretPolicy::try_from(custom.clone()).expect("custom compiles");
    for text in [
        "x tok-abcd12 y".to_string(),
        "bearer abcdefgh".to_string(),
        format!("tok-{}", "z".repeat(100)),
    ] {
        let legacy = scan_secrets(&text, &custom);
        let strict = compiled.scan_text(&text);
        assert_eq!(
            strict, legacy,
            "compiled scan must match legacy for {text:?}"
        );
        assert!(
            !strict.is_empty(),
            "custom pattern must hit {text:?}: {strict:?}"
        );
        assert!(
            strict[0].kind.starts_with("pattern"),
            "custom kind label for {text:?}: {}",
            strict[0].kind
        );
        assert_eq!(
            compiled.redact_text(&text),
            redact(&text, &custom),
            "compiled redact must match legacy for {text:?}"
        );
    }
    // The Debug form never echoes configured pattern text.
    let debug = format!("{compiled:?}");
    assert!(
        debug.contains("pattern_count"),
        "Debug reports counts: {debug}"
    );
    assert!(
        !debug.contains("tok-") && !debug.contains("bearer"),
        "Debug leaks pattern text: {debug}"
    );
    assert_eq!(
        format!("{compiled}"),
        "CompiledSecretPolicy(scan_enabled=true, block_on_secret=true, patterns=2)",
        "Display is count-only"
    );
    // A bad custom pattern refuses the WHOLE policy, naming the entry.
    let bad = SecretPolicy {
        scan_enabled: true,
        block_on_secret: true,
        key_patterns: vec!["good".into(), "bad.*".into()],
    };
    let err = CompiledSecretPolicy::try_from(bad).expect_err("bad custom pattern must refuse");
    assert_eq!(err.pattern, "bad.*", "refusal names the bad pattern");
    assert!(
        err.reason.contains("metacharacter"),
        "refusal names the construct: {err}"
    );
}

#[test]
fn secret_registry_exact_scan_is_position_accurate() {
    let mut registry = SecretRegistry::new();
    assert!(registry.is_empty(), "fresh registry is empty");
    registry.register(&[]);
    assert_eq!(registry.len(), 0, "empty secret is refused");
    registry.register(b"hunter2");
    registry.register(b"hunter2");
    assert_eq!(registry.len(), 1, "duplicate registration is a no-op");
    registry.register("é漢".as_bytes());
    registry.register(b"\x00\x01\x02");
    assert_eq!(registry.len(), 3, "distinct secrets register");

    let payload = b"x hunter2 \x00\x01\x02 y hunter2";
    let hits = registry.scan_exact(payload);
    assert_eq!(hits.len(), 3, "occurrences found: {hits:?}");
    assert_eq!(hits[0].offset, 2, "first occurrence offset");
    assert_eq!(hits[0].len, 7, "first occurrence length");
    assert_eq!(hits[0].kind, CONFIGURED_SECRET_KIND, "kind label");
    assert!(
        hits[0].snippet.is_empty(),
        "registry hits never echo candidate bytes: {:?}",
        hits[0].snippet
    );
    assert_eq!(hits[1].offset, 10, "binary occurrence offset");
    assert_eq!(hits[2].offset, 16, "second textual occurrence offset");
    assert!(
        registry.scan_exact(b"hunter").is_empty(),
        "prefix of a registered secret is not a hit"
    );
    assert!(
        registry.scan_exact(b"xxhunter2xx").len() == 1,
        "substring occurrence is found"
    );
    // The registry never stores plaintext: a Debug render carries none.
    let rendered = format!("{registry:?}");
    assert!(
        !rendered.contains("hunter2"),
        "registry Debug leaked plaintext: {rendered}"
    );
}

struct RotatingSecrets {
    values: Vec<Arc<[u8]>>,
}

impl ExactSecretSource for RotatingSecrets {
    fn active_secrets(&self) -> Vec<Arc<[u8]>> {
        self.values.clone()
    }
}

#[test]
fn registry_active_source_is_merged_per_scan() {
    let mut registry = SecretRegistry::new();
    registry.register(b"configured-value");
    let active: Arc<dyn ExactSecretSource> = Arc::new(RotatingSecrets {
        values: vec![Arc::from(b"live-token".as_slice())],
    });
    let registry = registry.with_active_source(active);
    let hits = registry.scan_exact(b"a configured-value b live-token c");
    assert_eq!(hits.len(), 2, "configured + active both found: {hits:?}");
    assert_eq!(hits[0].offset, 2, "configured offset");
    assert_eq!(hits[1].offset, 21, "live offset");
    // An active source that empties between scans stops matching.
    let empty: Arc<dyn ExactSecretSource> = Arc::new(RotatingSecrets { values: vec![] });
    let registry = SecretRegistry::new().with_active_source(empty);
    assert!(
        registry.scan_exact(b"live-token").is_empty(),
        "no active values = no hits"
    );
}

#[test]
fn payload_scanner_detects_boundary_crossing_secrets() {
    let key = sk(20).into_bytes();
    // Feed byte-at-a-time through the streaming scanner: every split of the
    // key must still be recognized exactly once.
    let mut scanner = Scanner::new(&ScanPolicy::default());
    for (fed, b) in key.iter().enumerate() {
        assert_eq!(
            scanner.feed(std::slice::from_ref(b)),
            FeedStatus::Ok,
            "feed byte {fed}"
        );
    }
    match scanner.finish() {
        ScanOutcome::Found(hits) => {
            assert_eq!(hits.len(), 1, "split secret found once: {hits:?}");
            assert_eq!(hits[0].kind, "openai_key", "kind of split secret");
            assert_eq!(hits[0].offset, 0, "offset of split secret");
        }
        other => panic!("byte-at-a-time split must be found, got {other:?}"),
    }

    // A secret split across a chunk boundary at every offset.
    for split in 0..=key.len() {
        let mut scanner = Scanner::new(&ScanPolicy::default());
        let (a, b) = key.split_at(split);
        assert_eq!(scanner.feed(a), FeedStatus::Ok, "head split at {split}");
        assert_eq!(scanner.feed(b), FeedStatus::Ok, "tail split at {split}");
        match scanner.finish() {
            ScanOutcome::Found(hits) => assert!(
                hits.iter().any(|h| h.kind == "openai_key"),
                "split at {split} must still detect: {hits:?}"
            ),
            other => panic!("split at {split} must be found, got {other:?}"),
        }
    }

    // Whole-payload variant of the same boundary matrix.
    for split in [0usize, 1, 2, 3, 5, 11, 22, 23] {
        let mut text = b"prefix ".to_vec();
        text.extend_from_slice(&key);
        let (a, b) = text.split_at(split.min(text.len()));
        let chunks: Vec<&[u8]> = vec![a, b];
        let outcome = crate::payload::streaming_scan(
            chunks.into_iter().map(|c| c.to_vec()),
            &ScanPolicy::default(),
        );
        match outcome {
            ScanOutcome::Found(hits) => {
                assert!(!hits.is_empty(), "whole-payload split at {split} detects")
            }
            other => panic!("whole-payload split at {split}: {other:?}"),
        }
    }
}

#[test]
fn payload_absolute_cap_fails_closed() {
    let policy = ScanPolicy {
        max_payload_bytes: Some(10),
        overlap_max: 4096,
    };
    match scan_payload(&sk(20).into_bytes(), &policy) {
        ScanOutcome::TooLargeForPolicy => {}
        other => panic!("oversized payload must fail closed, got {other:?}"),
    }
    match scan_payload(b"short", &policy) {
        ScanOutcome::Clean | ScanOutcome::Found(_) => {}
        other => panic!("payload within the cap must scan, got {other:?}"),
    }
    // A hit in the prefix never downgrades TooLargeForPolicy.
    let mut payload = sk(20).into_bytes();
    payload.extend_from_slice(&[b'x'; 100]);
    assert!(
        matches!(
            scan_payload(&payload, &policy),
            ScanOutcome::TooLargeForPolicy
        ),
        "a prefix hit cannot hide the cap failure"
    );
    // Exactly at the cap is allowed and Clean when no secret is present.
    match scan_payload(&[b'x'; 10], &policy) {
        ScanOutcome::Clean => {}
        other => panic!("exactly at the cap must be allowed, got {other:?}"),
    }
    // The compiled-policy path shares the cap contract.
    let compiled = CompiledSecretPolicy::try_from(SecretPolicy::default()).unwrap();
    assert!(
        matches!(
            scan_payload_compiled(&payload, &policy, &compiled),
            ScanOutcome::TooLargeForPolicy
        ),
        "compiled payload scan honors the cap"
    );
}

#[test]
fn payload_hit_flood_is_capped_but_never_clean() {
    // 9000 hits exceed MAX_PAYLOAD_HITS (8192): the scan must report Found
    // with a capped list, never Clean and never an unbounded Vec.
    let mut payload = Vec::new();
    for _ in 0..9000 {
        payload.extend_from_slice(b"sk-AAAAAAAAAAAAAAAAAAAA ");
    }
    match scan_payload(&payload, &ScanPolicy::default()) {
        ScanOutcome::Found(hits) => {
            assert!(
                hits.len() <= crate::payload::MAX_PAYLOAD_HITS,
                "hit list bounded, got {}",
                hits.len()
            );
            assert!(!hits.is_empty(), "capped list is still non-empty");
        }
        other => panic!("a hit flood must report Found, got {other:?}"),
    }
}

#[test]
fn redaction_survivor_property_over_hostile_shapes() {
    let policy = SecretPolicy::default();
    let shapes: Vec<String> = vec![
        format!("{}{}", "/".repeat(100), sk(20)),
        format!("{}{}", "é".repeat(100), ghp(20)),
        format!("{} {} {}", sk(20), aiza(20), ghp(20)),
        format!("{}{}", "x".repeat(64), aws(16)),
        format!("{} {}", "😀".repeat(64), slack(10)),
        format!("pre-inline {} post-inline", sk(20)),
        format!("{}{}{}", sk(20), sk(20), sk(20)),
        format!("0{}1{}2", ghp(20), ghp(20)),
        format!("{}{}", pem_header(), aiza(20)),
        sk(20).to_uppercase(),
    ];
    for shape in shapes {
        let cleaned = redact(&shape, &policy);
        assert!(
            scan_secrets(&cleaned, &policy).is_empty(),
            "survivor after redacting {shape:?} -> {cleaned:?}"
        );
        assert!(
            cleaned.len() <= shape.len() + 64 * 32,
            "redaction output stays bounded for {shape:?}"
        );
    }
}

fn pem_header() -> String {
    "-----BEGIN EC PRIVATE KEY-----".to_string()
}
