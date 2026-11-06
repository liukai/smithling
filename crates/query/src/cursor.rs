//! Opaque keyset cursor for the root list: base64 of `{start_time, run_id, filter_hash}`.
//! The filter hash makes a cursor from one filter unusable with another.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use tracedb_ingest::RootKey;
use tracedb_model::{RunId, Ts};

#[derive(Serialize, Deserialize)]
struct Wire {
    start_time: i64,
    run_id: RunId,
    filter_hash: u64,
}

pub fn encode(key: RootKey, filter_hash: u64) -> String {
    let w = Wire {
        start_time: key.start_time.0,
        run_id: key.run_id,
        filter_hash,
    };
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(&w).expect("serializable"))
}

/// Returns `None` for a malformed cursor or one made for a different filter.
pub fn decode(s: &str, filter_hash: u64) -> Option<RootKey> {
    let bytes = URL_SAFE_NO_PAD.decode(s).ok()?;
    let w: Wire = serde_json::from_slice(&bytes).ok()?;
    (w.filter_hash == filter_hash).then_some(RootKey {
        start_time: Ts(w.start_time),
        run_id: w.run_id,
    })
}
