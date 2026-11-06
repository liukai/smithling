//! Remembers recent `(run_id, op, body_hash)` keys so an SDK retry is stored once.

use std::collections::{HashMap, VecDeque};

use tracedb_model::{Op, RunId, Ts};

pub const DEFAULT_WINDOW_US: i64 = 24 * 3600 * 1_000_000;

pub type DedupKey = (RunId, Op, u64);

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
