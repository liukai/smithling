//! Remembers recent `(run_id, op, body_hash)` keys so an SDK retry is stored once. Each flush saves the keys
//! of its events in the catalog (D-022), so a restart rebuilds the window without keeping flushed WAL files.
//! In memory a key is a 16-byte fingerprint, and at most [`DEFAULT_CAP`] are kept (D-028).

use std::collections::{HashSet, VecDeque};

use tracedb_model::{Op, RunId, Ts};

/// 10 minutes: SDK retries come within seconds; a longer window doesn't fit in memory at M2 scale (D-008).
pub const DEFAULT_WINDOW_US: i64 = 10 * 60 * 1_000_000;

pub type DedupKey = (RunId, Op, u64);

/// One packed key: run id (16 B) + op (1 B) + body hash (8 B LE) + received µs (8 B LE).
const PACKED_LEN: usize = 16 + 1 + 8 + 8;

pub fn pack(keys: &[(DedupKey, Ts)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(keys.len() * PACKED_LEN);
    for ((id, op, hash), at) in keys {
        out.extend_from_slice(id.as_bytes());
        out.push(crate::wal::op_byte(*op));
        out.extend_from_slice(&hash.to_le_bytes());
        out.extend_from_slice(&at.0.to_le_bytes());
    }
    out
}

/// The reverse of [`pack`]; `None` if the bytes aren't a whole number of valid keys.
pub fn unpack(bytes: &[u8]) -> Option<Vec<(DedupKey, Ts)>> {
    if !bytes.len().is_multiple_of(PACKED_LEN) {
        return None;
    }
    let (keys, _) = bytes.as_chunks::<PACKED_LEN>();
    keys.iter()
        .map(|k| {
            let id = RunId::from_bytes(k[..16].try_into().unwrap());
            let op = crate::wal::byte_op(k[16])?;
            let hash = u64::from_le_bytes(k[17..25].try_into().unwrap());
            let at = Ts(i64::from_le_bytes(k[25..].try_into().unwrap()));
            Some(((id, op, hash), at))
        })
        .collect()
}

/// Keys kept at most: SDK retries come within seconds, and 1M keys (≈50 MB) is ~1 min of uploads at 20k events/s.
pub const DEFAULT_CAP: usize = 1 << 20;

/// A key as a 128-bit fingerprint (two differently seeded 64-bit hashes): if they behave like random bits,
/// ~1e-27 odds that two of a million keys collide, i.e. that a new event is taken for a retry.
type Fp = (u64, u64);

fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn fingerprint((id, op, hash): &DedupKey) -> Fp {
    let v = id.as_u128();
    let words = [v as u64, (v >> 64) as u64, *hash, crate::wal::op_byte(*op) as u64];
    let chain = |seed: u64, add: u64| words.iter().fold(seed, |h, w| mix(h ^ w.wrapping_add(add)));
    (
        chain(0x243F_6A88_85A3_08D3, 0),
        chain(0x1319_8A2E_0370_7344, 0x9E37_79B9_7F4A_7C15),
    )
}

pub struct Dedup {
    seen: HashSet<Fp>,
    /// (key, received µs), oldest first.
    order: VecDeque<(Fp, i64)>,
    window_us: i64,
    cap: usize,
}

impl Dedup {
    pub fn new(window_us: i64) -> Dedup {
        Dedup::with_cap(window_us, DEFAULT_CAP)
    }

    /// At most `cap` keys: past it the oldest is forgotten early.
    pub fn with_cap(window_us: i64, cap: usize) -> Dedup {
        Dedup {
            seen: HashSet::new(),
            order: VecDeque::new(),
            window_us,
            cap: cap.max(1),
        }
    }

    pub fn contains(&self, key: &DedupKey) -> bool {
        self.seen.contains(&fingerprint(key))
    }

    /// Records `key` as seen at `at`. Returns false if it was already seen.
    pub fn insert(&mut self, key: DedupKey, at: Ts) -> bool {
        let fp = fingerprint(&key);
        if !self.seen.insert(fp) {
            return false;
        }
        self.order.push_back((fp, at.0));
        while self.order.len() > self.cap {
            self.pop();
        }
        true
    }

    fn pop(&mut self) {
        if let Some((fp, _)) = self.order.pop_front() {
            self.seen.remove(&fp);
        }
    }

    /// Forgets keys older than the window, and gives back memory the window no longer needs.
    pub fn prune(&mut self, now: Ts) {
        while self.order.front().is_some_and(|&(_, at)| now.0 - at > self.window_us) {
            self.pop();
        }
        // After a burst the tables shrink back as the window empties.
        let n = self.order.len();
        if self.order.capacity() > 1024.max(4 * n) {
            self.seen.shrink_to(2 * n);
            self.order.shrink_to(2 * n);
        }
    }

    pub fn in_window(&self, at: Ts, now: Ts) -> bool {
        now.0 - at.0 <= self.window_us
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    /// Memory held by the keys (tables' capacity, not counting allocator rounding).
    pub fn heap_bytes(&self) -> usize {
        self.seen.capacity() * (size_of::<Fp>() + 1) + self.order.capacity() * size_of::<(Fp, i64)>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u128) -> DedupKey {
        (RunId::from_u128(n), Op::Post, n as u64 * 7)
    }

    #[test]
    fn keys_are_forgotten_after_the_window_and_past_the_cap() {
        let mut d = Dedup::with_cap(10, 3);
        for n in 0..3 {
            assert!(d.insert(key(n), Ts(n as i64)));
        }
        assert!(!d.insert(key(1), Ts(5)), "already seen");
        assert!(d.contains(&key(0)));
        // Past the cap the oldest key goes first.
        d.insert(key(3), Ts(3));
        assert_eq!(d.len(), 3);
        assert!(!d.contains(&key(0)) && d.contains(&key(3)));
        d.prune(Ts(12));
        assert!(!d.contains(&key(1)) && d.contains(&key(2)) && d.contains(&key(3)));
        d.prune(Ts(100));
        assert!(d.is_empty());
    }

    #[test]
    fn a_key_differs_from_one_with_another_op_or_body() {
        let mut d = Dedup::with_cap(10, 100);
        let (id, h) = (RunId::from_u128(9), 42);
        d.insert((id, Op::Post, h), Ts(0));
        assert!(d.contains(&(id, Op::Post, h)));
        assert!(!d.contains(&(id, Op::Patch, h)));
        assert!(!d.contains(&(id, Op::Post, h + 1)));
        assert!(!d.contains(&(RunId::from_u128(10), Op::Post, h)));
    }

    #[test]
    fn a_key_takes_about_50_bytes() {
        let mut d = Dedup::with_cap(i64::MAX, usize::MAX);
        for n in 0..100_000 {
            d.insert(key(n), Ts(0));
        }
        let per_key = d.heap_bytes() / d.len();
        assert!(per_key <= 64, "{per_key} B per key");
    }
}
