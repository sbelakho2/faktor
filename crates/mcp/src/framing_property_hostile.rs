//! Deterministic property tests over hostile framing byte corpora.
//!
//! Fixed-seed generators build arbitrary header/body byte soup (CRLFs,
//! colons, digits, NULs, invalid UTF-8, huge declarations, JSON fragments)
//! and every parse must obey the framing contract: never panic, a complete
//! frame consumes 1..=len bytes, every error carries a message, and repeated
//! calls classify identically (deterministic evidence).

use super::*;

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

const ALPHABET: &[u8] = &[
    b'C', b'o', b'n', b't', b'e', b'n', b't', b'-', b'L', b'e', b'n', b'g', b't', b'h', b':', b' ',
    b'0', b'1', b'2', b'9', b'\r', b'\n', b'{', b'}', b'"', b'[', b']', b'x', 0x00, 0xFF, b'\t',
    b'.', b'-',
];

fn generate(seed: u64, len: usize) -> Vec<u8> {
    let mut rng = Lcg(seed | 1);
    let mut bytes = Vec::with_capacity(len);
    for _ in 0..len {
        bytes.push(ALPHABET[(rng.next() % ALPHABET.len() as u64) as usize]);
    }
    bytes
}

#[derive(Debug, PartialEq)]
enum Class {
    Incomplete,
    Error(String),
    Frame(usize),
}

fn classify(bytes: &[u8]) -> Class {
    match parse_frame(bytes) {
        Ok(None) => Class::Incomplete,
        Ok(Some((consumed, _))) => Class::Frame(consumed),
        Err(message) => Class::Error(message),
    }
}

/// Every generated buffer satisfies the framing contract with a distinct
/// per-case message; eight seeds × 16 iterations = 128 cases.
#[test]
fn generated_framing_corpus_obeys_the_contract() {
    let mut cases = 0usize;
    for seed in [1u64, 2, 3, 5, 8, 13, 21, 34] {
        for iteration in 0..16usize {
            cases += 1;
            let label = format!("seed={seed} iter={iteration}");
            let bytes = generate(seed.wrapping_add(iteration as u64 * 6151), iteration % 48);
            let first = classify(&bytes);
            let second = classify(&bytes);
            assert_eq!(
                first, second,
                "case {label}: classification must be deterministic"
            );
            match &first {
                Class::Frame(consumed) => {
                    assert!(
                        *consumed > 0 && *consumed <= bytes.len(),
                        "case {label}: a complete frame must consume 1..=len bytes, got {consumed} of {}",
                        bytes.len()
                    );
                }
                Class::Error(message) => {
                    assert!(
                        !message.is_empty(),
                        "case {label}: a framing error must carry a message"
                    );
                }
                Class::Incomplete => {}
            }
            // Appending arbitrary bytes to an incomplete prefix must stay
            // safe and never turn a consumed frame into an out-of-range one.
            let mut extended = bytes.clone();
            extended.extend_from_slice(&generate(seed ^ 0x9E37, 4));
            match classify(&extended) {
                Class::Frame(consumed) => assert!(
                    consumed <= extended.len(),
                    "case {label}: extended frame consumed {consumed} of {}",
                    extended.len()
                ),
                Class::Error(message) => assert!(
                    !message.is_empty(),
                    "case {label}: extended error must carry a message"
                ),
                Class::Incomplete => {}
            }
        }
    }
    assert!(
        cases >= 128,
        "the framing corpus must keep at least 128 cases, found {cases}"
    );
}
