//! Every run id with a version in a data file, as a bloom filter (D-025): a new start for one of them is a
//! second version, so the run joins the multi-version set. A false positive only sends a run to the slower,
//! merging read path; there are no false negatives.
//!
//! Sized for 2M ids at 1% false positives (≈2.4 MB), or twice the ids found at start; when a stage fills, a
//! new one twice its size is added.

use smithling_model::RunId;

pub const DEFAULT_CAPACITY: usize = 2_000_000;
const HASHES: u32 = 7;
/// −ln(0.01) / ln(2)² bits per id for 1% false positives.
const BITS_PER_ID: f64 = 9.585;

#[derive(Clone, Debug)]
struct Stage {
    bits: Vec<u64>,
    capacity: usize,
    len: usize,
}

impl Stage {
    fn new(capacity: usize) -> Stage {
        let bits = ((capacity as f64 * BITS_PER_ID) as usize).max(64);
        Stage {
            bits: vec![0; bits.div_ceil(64)],
            capacity,
            len: 0,
        }
    }

    fn slots(&self, (h1, h2): (u64, u64)) -> impl Iterator<Item = usize> + use<> {
        let m = (self.bits.len() * 64) as u64;
        (0..HASHES as u64).map(move |i| (h1.wrapping_add(i.wrapping_mul(h2)) % m) as usize)
    }

    fn contains(&self, h: (u64, u64)) -> bool {
        self.slots(h).all(|b| self.bits[b / 64] & (1 << (b % 64)) != 0)
    }

    fn insert(&mut self, h: (u64, u64)) {
        for b in self.slots(h) {
            self.bits[b / 64] |= 1 << (b % 64);
        }
        self.len += 1;
    }
}

#[derive(Clone, Debug)]
pub struct FlushedIds {
    stages: Vec<Stage>,
}

fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn hashes(id: &RunId) -> (u64, u64) {
    let v = id.as_u128();
    let h1 = mix((v as u64) ^ mix((v >> 64) as u64));
    (h1, mix(h1 ^ 0x9E37_79B9_7F4A_7C15) | 1)
}

impl FlushedIds {
    /// Room for `expected` ids (at least [`DEFAULT_CAPACITY`]) before a second stage is added.
    pub fn new(expected: usize) -> FlushedIds {
        FlushedIds {
            stages: vec![Stage::new(expected.max(DEFAULT_CAPACITY))],
        }
    }

    pub fn with_capacity(capacity: usize) -> FlushedIds {
        FlushedIds {
            stages: vec![Stage::new(capacity.max(1))],
        }
    }

    pub fn contains(&self, id: &RunId) -> bool {
        let h = hashes(id);
        self.stages.iter().any(|s| s.contains(h))
    }

    pub fn insert(&mut self, id: &RunId) {
        let h = hashes(id);
        if self.stages.iter().any(|s| s.contains(h)) {
            return;
        }
        let last = self.stages.last().expect("one stage");
        if last.len >= last.capacity {
            let next = Stage::new(last.capacity * 2);
            self.stages.push(next);
        }
        self.stages.last_mut().expect("one stage").insert(h);
    }

    pub fn bytes(&self) -> usize {
        self.stages.iter().map(|s| s.bits.len() * 8).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn id(n: u128) -> RunId {
        RunId::from_u128(n)
    }

    proptest! {
        #[test]
        fn no_false_negatives(ids in proptest::collection::vec(any::<u128>(), 0..3_000), cap in 1usize..500) {
            let mut f = FlushedIds::with_capacity(cap);
            for &i in &ids {
                f.insert(&id(i));
            }
            for &i in &ids {
                prop_assert!(f.contains(&id(i)));
            }
        }
    }

    #[test]
    fn about_one_percent_false_positives_when_full_and_after_growing() {
        for (cap, n) in [(50_000, 50_000), (10_000, 70_000)] {
            let mut f = FlushedIds::with_capacity(cap);
            // Sequential ids, as the tests' `uuid(n)` helpers make: the hash must spread them.
            for i in 0..n {
                f.insert(&id(0x4000_8000_0000_0000_0000 | i));
            }
            let fp = (0..100_000u128).filter(|i| f.contains(&id(1 << 100 | i))).count();
            let limit = if cap == n as usize { 1_500 } else { 4_000 };
            assert!(fp < limit, "cap {cap}, n {n}: {fp} false positives in 100k");
        }
    }

    #[test]
    fn two_million_ids_take_about_2_4_mb() {
        let f = FlushedIds::new(0);
        assert!((2_300_000..2_500_000).contains(&f.bytes()), "{}", f.bytes());
    }
}
