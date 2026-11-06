//! Opaque keyset cursor: base64 of `{key, run_id, query_hash}` (data-model §9). The query hash covers
//! everything that changes results, so a cursor from one query can't be used with another.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use tracedb_model::RunId;

/// Position in an ordered result: the order value (start key or latency) and the run id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Key {
    pub value: i64,
    pub run_id: RunId,
}

#[derive(Serialize, Deserialize)]
struct Wire {
    key: Key,
    query_hash: u64,
}

pub fn encode(key: Key, query_hash: u64) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(&Wire { key, query_hash }).expect("serializable"))
}

/// `None` for a malformed cursor or one made for a different query.
pub fn decode(s: &str, query_hash: u64) -> Option<Key> {
    let bytes = URL_SAFE_NO_PAD.decode(s).ok()?;
    let w: Wire = serde_json::from_slice(&bytes).ok()?;
    (w.query_hash == query_hash).then_some(w.key)
}
