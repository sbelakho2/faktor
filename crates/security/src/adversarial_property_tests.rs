//! Deterministic property tests over the security surface (task category 10).
//!
//! Every property is driven by a fixed, explicit seed; every case is
//! asserted with a message naming the seed and the generated input. No
//! external RNG dependency: a local SplitMix64 generator keeps the corpus
//! reproducible forever.

use super::destination::{DestinationPolicy, RequestTarget};
use super::network::{classify_ip, vet_resolved_answers, AddressClass, EgressAddressPolicy};
use super::payload::{scan_payload, streaming_scan, ScanOutcome, ScanPolicy};
use super::{redact, scan_secrets, SecretPolicy};
use std::net::{IpAddr, SocketAddr};

/// Deterministic SplitMix64 — reproducible across platforms.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed)
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const SEEDS: [u64; 16] = [
    1,
    2,
    3,
    7,
    11,
    42,
    99,
    1234,
    31337,
    0xDEAD_BEEF,
    0xFEED_FACE,
    0x0BAD_F00D,
    0xC0FF_EE00,
    u64::MAX - 1,
    0xA5A5_A5A5_A5A5_A5A5,
    0x5EED_5EED,
];

fn sk(n: usize) -> String {
    format!("sk-{}", "A".repeat(n))
}
fn ghp(n: usize) -> String {
    format!("ghp_{}", "B".repeat(n))
}
fn aiza(n: usize) -> String {
    format!("AIza{}", "c".repeat(n))
}
fn aws16() -> String {
    format!("AKIA{}", "D".repeat(16))
}
fn slack(n: usize) -> String {
    format!("xoxb-{}", "e".repeat(n))
}

/// A corpus token that is either a secret or a decoy near-miss.
fn random_token(rng: &mut Rng) -> String {
    match rng.below(8) {
        0 => sk(20 + rng.below(8)),
        1 => format!("sk-{}", "A".repeat(rng.below(20))), // too short
        2 => ghp(20 + rng.below(8)),
        3 => aiza(20 + rng.below(8)),
        4 => aws16(),
        5 => slack(10 + rng.below(8)),
        6 => "-----BEGIN RSA PRIVATE KEY-----".into(),
        _ => {
            let n = rng.below(30);
            let mut s = String::new();
            for _ in 0..n {
                s.push((b'a' + rng.below(26) as u8) as char);
            }
            s
        }
    }
}

#[test]
fn redaction_never_leaves_a_detected_secret_property() {
    let policy = SecretPolicy::default();
    for seed in SEEDS {
        let mut rng = Rng::new(seed);
        for case in 0..4 {
            let parts = 1 + rng.below(12);
            let mut text = String::new();
            for _ in 0..parts {
                text.push_str(&random_token(&mut rng));
                text.push(match rng.below(3) {
                    0 => ' ',
                    1 => '!',
                    _ => '\n',
                });
            }
            let cleaned = redact(&text, &policy);
            assert!(
                scan_secrets(&cleaned, &policy).is_empty(),
                "seed {seed} case {case}: survivor after redacting {text:?} -> {cleaned:?}"
            );
            let cleaned_twice = redact(&cleaned, &policy);
            assert!(
                scan_secrets(&cleaned_twice, &policy).is_empty(),
                "seed {seed} case {case}: second redaction leaves a survivor"
            );
        }
    }
}

#[test]
fn planted_secret_offsets_are_exact_property() {
    let policy = SecretPolicy::default();
    for seed in SEEDS {
        let mut rng = Rng::new(seed ^ 0x5111_5111);
        for case in 0..3 {
            let mut text = String::new();
            let mut planted: Vec<(usize, usize, &'static str)> = Vec::new();
            let plants = 1 + rng.below(6);
            for _ in 0..plants {
                let (secret, kind): (String, &'static str) = match rng.below(5) {
                    0 => (sk(20 + rng.below(5)), "openai_key"),
                    1 => (ghp(20), "github_token"),
                    2 => (aiza(20), "google_api_key"),
                    3 => (aws16(), "aws_key"),
                    _ => (slack(10), "slack_token"),
                };
                // Space separators guarantee the greedy class run stops at
                // the planted boundary.
                text.push_str(" pad ");
                let start = text.len();
                text.push_str(&secret);
                planted.push((start, secret.len(), kind));
                text.push(' ');
            }
            let hits = scan_secrets(&text, &policy);
            assert_eq!(
                hits.len(),
                planted.len(),
                "seed {seed} case {case}: every plant is one hit: {hits:?} text={text:?}"
            );
            for (i, ((start, len, kind), hit)) in planted.iter().zip(&hits).enumerate() {
                assert_eq!(
                    hit.offset, *start,
                    "seed {seed} case {case} plant {i}: offset in {text:?}"
                );
                assert_eq!(
                    hit.len, *len,
                    "seed {seed} case {case} plant {i}: len in {text:?}"
                );
                assert_eq!(
                    hit.kind, *kind,
                    "seed {seed} case {case} plant {i}: kind in {text:?}"
                );
            }
        }
    }
}

#[test]
fn host_canonicalization_is_idempotent_property() {
    for seed in SEEDS {
        let mut rng = Rng::new(seed ^ 0x2222_3333);
        for case in 0..4 {
            let labels = 1 + rng.below(4);
            let mut host = String::new();
            for label in 0..labels {
                if label > 0 {
                    host.push('.');
                }
                // A leading ASCII letter keeps every label name-shaped (an
                // all-numeric single label is deliberately refused by the
                // canonicalizer as ambiguous IP shorthand).
                host.push((b'a' + rng.below(26) as u8) as char);
                let n = rng.below(10);
                for _ in 0..n {
                    let c = match rng.below(36) {
                        0..=25 => (b'a' + rng.below(26) as u8) as char,
                        _ => (b'0' + rng.below(10) as u8) as char,
                    };
                    host.push(c);
                }
            }
            // Randomly flip case and add a trailing dot.
            if rng.below(2) == 0 {
                host = host.to_uppercase();
            }
            if rng.below(2) == 0 {
                host.push('.');
            }
            let once = super::destination::canonicalize_request_host(&host, false, None);
            match once {
                Ok(canonical) => {
                    let twice =
                        super::destination::canonicalize_request_host(&canonical, false, None);
                    assert_eq!(
                        twice.as_deref(),
                        Ok(canonical.as_str()),
                        "seed {seed} case {case}: canonicalization not idempotent for {host:?}"
                    );
                }
                Err(e) => panic!("seed {seed} case {case}: generated host {host:?} refused: {e}"),
            }
        }
    }
}

#[test]
fn exact_policy_allows_only_the_canonical_host_property() {
    for seed in SEEDS {
        let mut rng = Rng::new(seed ^ 0x4444_5555);
        for case in 0..3 {
            let label = format!(
                "{}{}",
                (b'a' + rng.below(26) as u8) as char,
                "x".repeat(1 + rng.below(8))
            );
            let domain = format!("{label}.example.test");
            let policy = DestinationPolicy::parse_lines([domain.as_str()]).unwrap();
            let allowed = |host: &str| policy.check("https", host, 443, false, None).is_allowed();
            assert!(
                allowed(&domain),
                "seed {seed} case {case}: exact rule must allow {domain:?}"
            );
            assert!(
                allowed(&domain.to_uppercase()),
                "seed {seed} case {case}: case-insensitive match for {domain:?}"
            );
            assert!(
                allowed(&format!("{domain}.")),
                "seed {seed} case {case}: trailing dot canonicalizes for {domain:?}"
            );
            for denied in [
                format!("evil-{domain}"),
                format!("{domain}.evil"),
                format!("x{domain}"),
                format!("{domain}x"),
                format!("{label}{label}.example.test"),
                format!("{label}.example.test.evil"),
                format!("{label}.example"),
            ] {
                assert!(
                    !allowed(&denied),
                    "seed {seed} case {case}: {denied:?} must not match the exact rule {domain:?}"
                );
            }
            let target = RequestTarget::parse(&format!("https://{domain}/p")).unwrap();
            assert!(
                policy
                    .check(
                        &target.scheme,
                        &target.host,
                        target.port.unwrap_or(0),
                        target.is_ipv4,
                        target.ip
                    )
                    .is_allowed(),
                "seed {seed} case {case}: parsed target {target:?} must match {domain:?}"
            );
        }
    }
}

#[test]
fn address_class_policy_consistency_property() {
    for seed in SEEDS {
        let mut rng = Rng::new(seed ^ 0x6666_7777);
        for case in 0..4 {
            let v4 = (rng.next() & 0xFFFF_FFFF) as u32;
            let ip = if rng.below(2) == 0 {
                IpAddr::from(std::net::Ipv4Addr::from(v4))
            } else {
                IpAddr::from(std::net::Ipv6Addr::from(
                    (rng.next() as u128) << 64 | rng.next() as u128,
                ))
            };
            let class = classify_ip(ip);
            for policy in [EgressAddressPolicy::EXTERNAL, EgressAddressPolicy::LOCAL] {
                let expected = class == AddressClass::Global
                    || (class == AddressClass::Loopback && policy.allow_loopback());
                assert_eq!(
                    policy.permits(ip),
                    expected,
                    "seed {seed} case {case}: permits({ip}, {class:?}) under {policy:?}"
                );
                let addr = SocketAddr::new(ip, 443);
                assert_eq!(
                    vet_resolved_answers(&[addr], policy).is_ok(),
                    expected,
                    "seed {seed} case {case}: vet([{ip}]) under {policy:?} must mirror permits"
                );
            }
            assert_eq!(
                classify_ip(ip),
                class,
                "seed {seed} case {case}: classification must be deterministic for {ip}"
            );
            assert!(
                !class.as_str().is_empty(),
                "seed {seed} case {case}: class label for {ip}"
            );
        }
    }
}

#[test]
fn streaming_and_whole_payload_scans_agree_property() {
    for seed in SEEDS {
        let mut rng = Rng::new(seed ^ 0x8888_9999);
        for case in 0..4 {
            let secret = sk(20 + rng.below(4));
            let mut payload = Vec::new();
            let prefix = rng.below(64);
            payload.extend(std::iter::repeat_n(b'z', prefix));
            payload.extend_from_slice(secret.as_bytes());
            payload.extend(std::iter::repeat_n(b'y', rng.below(64)));
            let whole = scan_payload(&payload, &ScanPolicy::default());
            let split = 1 + rng.below(payload.len().saturating_sub(1).max(1));
            let split = split.min(payload.len());
            let chunks: Vec<Vec<u8>> = vec![payload[..split].to_vec(), payload[split..].to_vec()];
            let streamed = streaming_scan(chunks, &ScanPolicy::default());
            match (&whole, &streamed) {
                (ScanOutcome::Found(a), ScanOutcome::Found(b)) => {
                    assert_eq!(
                        a.len(),
                        b.len(),
                        "seed {seed} case {case}: hit count agreement at split {split}"
                    );
                    assert_eq!(
                        (a[0].offset, a[0].len, &a[0].kind),
                        (b[0].offset, b[0].len, &b[0].kind),
                        "seed {seed} case {case}: first hit agreement at split {split}"
                    );
                }
                (ScanOutcome::Clean, ScanOutcome::Clean) => {
                    assert!(
                        scan_secrets(&String::from_utf8_lossy(&payload), &SecretPolicy::default())
                            .is_empty(),
                        "seed {seed} case {case}: whole scan says Clean but text scan found a hit"
                    );
                }
                (a, b) => panic!(
                    "seed {seed} case {case}: whole vs streamed disagree at split {split}: \
                     {a:?} vs {b:?}"
                ),
            }
        }
    }
}
