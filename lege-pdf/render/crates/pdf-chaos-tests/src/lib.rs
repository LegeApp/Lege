//! Deterministic chaos/mutation engine for the never-panic regression gate.
//!
//! This crate is dev/test-only (`publish = false`). It is the *stable*
//! counterpart of the nightly-only `fuzz/` workspace: hand-rolled
//! xorshift64* PRNG, fixed seeds, no external dependencies — so the mutation
//! suite runs in plain `cargo test` on every machine and CI leg, forever.
//!
//! The integration tests (`tests/chaos.rs`) apply [`mutate`] to small
//! fixture PDFs and assert that every mutant either completes the
//! open → compile → render pipeline or fails with a typed error — never a
//! panic.

/// xorshift64* — tiny, deterministic, and plenty for mutation fuzzing.
/// (Vigna, "An experimental exploration of Marsaglia's xorshift
/// generators, scrambled".)
#[derive(Debug, Clone)]
pub struct XorShift64Star {
    state: u64,
}

impl XorShift64Star {
    /// Seed must be non-zero; a zero seed is remapped to a fixed constant.
    pub fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform-enough value in `0..bound` (`bound` must be non-zero).
    pub fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() % bound as u64) as usize
    }

    pub fn byte(&mut self) -> u8 {
        (self.next_u64() >> 32) as u8
    }
}

/// One deterministic mutation of `original`, derived purely from `seed`.
///
/// Kinds (chosen by the PRNG): single/multi byte flips, bit flips,
/// truncation, random-byte insertion, and block duplication — the classic
/// corruption shapes of damaged real-world files.
pub fn mutate(original: &[u8], seed: u64) -> Vec<u8> {
    let mut rng = XorShift64Star::new(seed);
    let mut data = original.to_vec();
    if data.is_empty() {
        return data;
    }
    match rng.below(6) {
        // Flip 1..=8 whole bytes to random values.
        0 => {
            let n = 1 + rng.below(8);
            for _ in 0..n {
                let i = rng.below(data.len());
                data[i] = rng.byte();
            }
        }
        // Flip 1..=16 single bits.
        1 => {
            let n = 1 + rng.below(16);
            for _ in 0..n {
                let i = rng.below(data.len());
                data[i] ^= 1 << rng.below(8);
            }
        }
        // Truncate to a random prefix.
        2 => {
            let keep = rng.below(data.len());
            data.truncate(keep);
        }
        // Insert 1..=16 random bytes at a random offset.
        3 => {
            let n = 1 + rng.below(16);
            let at = rng.below(data.len() + 1);
            let insert: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
            data.splice(at..at, insert);
        }
        // Duplicate a random block to a random destination (overwrite).
        4 => {
            let len = 1 + rng.below(64.min(data.len()));
            let src = rng.below(data.len() - len + 1);
            let dst = rng.below(data.len() - len + 1);
            let block: Vec<u8> = data[src..src + len].to_vec();
            data[dst..dst + len].copy_from_slice(&block);
        }
        // Zero a random run (simulates a bad disk sector).
        _ => {
            let len = 1 + rng.below(64.min(data.len()));
            let at = rng.below(data.len() - len + 1);
            for b in &mut data[at..at + len] {
                *b = 0;
            }
        }
    }
    data
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn prng_is_deterministic_and_nonzero() {
        let mut a = XorShift64Star::new(42);
        let mut b = XorShift64Star::new(42);
        for _ in 0..1000 {
            let v = a.next_u64();
            assert_eq!(v, b.next_u64());
            assert_ne!(v, 0, "xorshift64* never yields zero from nonzero state");
        }
        // Zero seed is remapped, not a fixed point.
        assert_ne!(XorShift64Star::new(0).next_u64(), 0);
    }

    #[test]
    fn mutate_is_deterministic() {
        let base = b"Hello, chaotic world of PDF bytes".to_vec();
        for seed in 1..200u64 {
            assert_eq!(mutate(&base, seed), mutate(&base, seed));
        }
    }

    #[test]
    fn mutate_produces_varied_outputs() {
        let base: Vec<u8> = (0..=255u8).collect();
        let distinct: std::collections::HashSet<Vec<u8>> =
            (1..=100u64).map(|s| mutate(&base, s)).collect();
        assert!(
            distinct.len() > 80,
            "only {} distinct mutants",
            distinct.len()
        );
    }
}
