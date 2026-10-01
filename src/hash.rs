//! Hash maps keyed by engine-assigned integer ids.
//!
//! The std `HashMap` uses SipHash with a random seed to resist hash-flooding
//! attacks on attacker-chosen keys. Here every key is an id assigned by the
//! engine itself (accounts, orders), so that protection buys nothing. A
//! multiply-rotate hash in the style of FxHash (rustc's own hasher) is several
//! times cheaper and has no random seed, so even iteration order is
//! reproducible. The engine never lets map iteration order reach the event
//! stream anyway: everything order-sensitive uses `BTreeMap`/`BTreeSet`.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

/// FxHash-style hasher: `hash = (hash.rotl(5) ^ word) * SEED` per word.
#[derive(Clone, Copy, Default)]
pub struct FxHasher {
    hash: u64,
}

impl FxHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.add(u64::from_le_bytes(c.try_into().expect("8 bytes")));
        }
        for &b in chunks.remainder() {
            self.add(b as u64);
        }
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(i as u64);
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(i as u64);
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

pub type FxBuild = BuildHasherDefault<FxHasher>;

/// Map used for account and order indexes.
pub type FastMap<K, V> = HashMap<K, V, FxBuild>;
pub type FastSet<K> = HashSet<K, FxBuild>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::BuildHasher;

    #[test]
    fn deterministic_and_spread() {
        let b = FxBuild::default();
        assert_eq!(b.hash_one(42u64), FxBuild::default().hash_one(42u64));
        // Sequential ids must not collide in the low bits used for buckets.
        let mut low: Vec<u64> = (1..=4096u64).map(|i| b.hash_one(i) & 4095).collect();
        low.sort_unstable();
        low.dedup();
        assert!(low.len() > 2_500, "poor spread: {}", low.len());
    }
}
