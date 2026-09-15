//! Per-actor ChaCha8 streams with explicit word-consumption counters.

use rand::{RngCore, SeedableRng};
use rand_chacha::ChaCha8Rng;
use sha2::{Digest, Sha256};

pub struct CountedRng {
    inner: ChaCha8Rng,
    consumed: u64,
}

impl CountedRng {
    #[must_use]
    pub fn new(seed: [u8; 32]) -> Self {
        Self {
            inner: ChaCha8Rng::from_seed(seed),
            consumed: 0,
        }
    }

    #[must_use]
    pub fn at(seed: [u8; 32], consumed: u64) -> Self {
        let mut rng = Self::new(seed);
        for _ in 0..consumed {
            let _ = rng.next_u64();
        }
        rng
    }

    #[must_use]
    pub const fn consumed(&self) -> u64 {
        self.consumed
    }

    pub fn next_u64(&mut self) -> u64 {
        self.consumed += 1;
        self.inner.next_u64()
    }

    pub fn bounded(&mut self, upper_exclusive: u64) -> u64 {
        let draw = self.next_u64();
        if upper_exclusive == 0 {
            0
        } else {
            draw % upper_exclusive
        }
    }

    pub fn chance_ppm(&mut self, probability_ppm: u32) -> bool {
        self.bounded(1_000_000) < u64::from(probability_ppm.min(1_000_000))
    }
}

#[must_use]
pub fn derive_seed(master_seed: u64, actor: &str, stream: &str) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(master_seed.to_le_bytes());
    digest.update([0]);
    digest.update(actor.as_bytes());
    digest.update([0]);
    digest.update(stream.as_bytes());
    digest.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counted_stream_reconstructs_exactly() {
        let seed = derive_seed(9, "agent-1", "decision");
        let mut first = CountedRng::new(seed);
        let values = [
            first.next_u64(),
            first.bounded(7),
            u64::from(first.chance_ppm(500_000)),
        ];
        assert_eq!(first.consumed(), 3);

        let mut replay = CountedRng::at(seed, 0);
        let replayed = [
            replay.next_u64(),
            replay.bounded(7),
            u64::from(replay.chance_ppm(500_000)),
        ];
        assert_eq!(values, replayed);
        assert_eq!(CountedRng::at(seed, 3).consumed(), 3);
    }

    #[test]
    fn bounded_zero_is_defined_and_still_counted() {
        let mut rng = CountedRng::new([3; 32]);
        assert_eq!(rng.bounded(0), 0);
        assert_eq!(rng.consumed(), 1);
        assert!(!rng.chance_ppm(0));
        assert!(rng.chance_ppm(1_000_000));
    }
}
