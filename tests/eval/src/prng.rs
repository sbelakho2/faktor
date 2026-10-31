// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! Deterministic seeded PRNG for the fixture generators.
//!
//! SplitMix64 (public-domain reference algorithm): one u64 state, no
//! dependencies, bit-exact reproducible across platforms and Rust versions —
//! the polyglot and legacy corpora are a pure function of the seed.

/// SplitMix64 generator: same seed ⇒ same stream ⇒ byte-identical corpus.
#[derive(Debug, Clone)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Next raw u64 (the reference SplitMix64 step).
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform value in `[low, high)` via rejection-free Lemire-style multiply
    /// (deterministic; bias is irrelevant for fixture generation).
    pub fn range(&mut self, low: u64, high: u64) -> u64 {
        debug_assert!(high > low);
        let span = high - low;
        low + (self.next_u64() % span)
    }

    /// Lowercase-hex token of `digits` characters (deterministic filler).
    pub fn hex_token(&mut self, digits: usize) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(digits);
        while out.len() < digits {
            let v = self.next_u64();
            for shift in (0..16).map(|i| i * 4) {
                out.push(HEX[((v >> shift) & 0xf) as usize] as char);
                if out.len() == digits {
                    break;
                }
            }
        }
        out
    }

    /// Deterministic pick from a non-empty slice.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.range(0, items.len() as u64) as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::SplitMix64;

    #[test]
    fn prng_determinism_same_seed_same_stream() {
        let mut a = SplitMix64::new(0xdead_beef);
        let mut b = SplitMix64::new(0xdead_beef);
        for _ in 0..64 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn prng_different_seeds_diverge() {
        let mut a = SplitMix64::new(1);
        let mut b = SplitMix64::new(2);
        let same = (0..8).filter(|_| a.next_u64() == b.next_u64()).count();
        assert_eq!(same, 0, "independent seeds must not share stream values");
    }

    #[test]
    fn prng_range_stays_in_bounds() {
        let mut r = SplitMix64::new(7);
        for _ in 0..256 {
            let v = r.range(3, 11);
            assert!((3..11).contains(&v));
        }
    }
}
