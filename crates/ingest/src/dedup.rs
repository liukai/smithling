//! Remembers recent `(run_id, op, body_hash)` keys so an SDK retry is stored once. Each flush saves the keys
//! of its events in the catalog (D-022), so a restart rebuilds the window without keeping flushed WAL files.

use std::collections::{HashMap, VecDeque};

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

pub struct Dedup {
    seen: HashMap<DedupKey, Ts>,
    order: VecDeque<(DedupKey, Ts)>,
    window_us: i64,
}

impl Dedup {
    pub fn new(window_us: i64) -> Dedup {
        Dedup {
            seen: HashMap::new(),
            order: VecDeque::new(),
            window_us,
        }
    }

    pub fn contains(&self, key: &DedupKey) -> bool {
        self.seen.contains_key(key)
    }

    /// Records `key` as seen at `at`. Returns false if it was already seen.
    pub fn insert(&mut self, key: DedupKey, at: Ts) -> bool {
        if self.seen.contains_key(&key) {
            return false;
        }
        self.seen.insert(key, at);
        self.order.push_back((key, at));
        true
    }

    /// Forgets keys older than the window.
    pub fn prune(&mut self, now: Ts) {
        while let Some((key, at)) = self.order.front().copied() {
            if now.0 - at.0 <= self.window_us {
                break;
            }
            self.order.pop_front();
            self.seen.remove(&key);
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
}
