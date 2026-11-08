//! Text index backfill for files written before the index existed (M2 folders; design §4.2, q7 settled):
//! once no flush has run for a flush age, one such file gets a sidecar, written as new objects, then its
//! `fts_path` in one catalog update. A crash between the two leaves an orphan, deleted on start. Until then
//! the file is searched by reading it, so this is never needed for correct results.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use smithling_model::Ts;
use smithling_storage::{SegmentMeta, StorageError, fts, segment};

use crate::flush::FlushHealth;
use crate::memtable::Mem;
use crate::{Storage, crash_point};

pub(crate) fn run(
    mem: &RwLock<Mem>,
    storage: &Storage,
    flush_health: &Mutex<FlushHealth>,
    flush_age: Duration,
    stop: &AtomicBool,
) {
    let poll = (flush_age / 4).clamp(Duration::from_millis(10), Duration::from_secs(1));
    let mut busy_at = Instant::now();
    let mut flushes = flush_health.lock().unwrap().flushes;
    let mut failed: HashSet<String> = HashSet::new();
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(poll);
        let now_flushes = flush_health.lock().unwrap().flushes;
        if now_flushes != flushes {
            (flushes, busy_at) = (now_flushes, Instant::now());
        }
        if busy_at.elapsed() < flush_age {
            continue;
        }
        let next = {
            let mem = mem.read().unwrap();
            mem.version
                .segments
                .iter()
                .find(|s| s.fts_path.is_none() && !failed.contains(&s.id))
                .cloned()
        };
        let Some(seg) = next else { continue };
        let started = Instant::now();
        match one(mem, storage, &seg) {
            Ok(bytes) => tracing::info!(
                segment = %seg.id,
                bytes,
                ms = started.elapsed().as_millis() as u64,
                "wrote a text index for a file written without one"
            ),
            Err(e) => {
                tracing::warn!(segment = %seg.id, error = %e, "could not index an old file; it is still searched by reading it");
                failed.insert(seg.id.clone());
            }
        }
        busy_at = Instant::now();
    }
}

fn one(mem: &RwLock<Mem>, storage: &Storage, seg: &SegmentMeta) -> Result<u64, StorageError> {
    let (store, core, payload) = (storage.store.clone(), seg.core_path.clone(), seg.payload_path.clone());
    let (core, payload) =
        storage.block(async move { Ok::<_, StorageError>((store.get(&core).await?, store.get(&payload).await?)) })?;
    let side = segment::sidecar_of(core, payload)?;
    let stem = format!(
        "{}.fts-{}",
        seg.core_path.trim_end_matches(".core.parquet"),
        Ts::now().0
    );
    let bytes = side.bytes();
    let (store, s) = (storage.store.clone(), stem.clone());
    storage.block(async move {
        store.put_new(&fts::dict_path(&s), side.dict).await?;
        store.put_new(&fts::post_path(&s), side.post).await
    })?;
    crash_point("after_backfill_put");
    let (meta, id, s) = (storage.meta.clone(), seg.id.clone(), stem.clone());
    storage.block(async move { meta.set_fts(&id, &s, bytes).await })?;
    let mut mem = mem.write().unwrap();
    mem.version = Arc::new(mem.version.with_fts(&seg.id, &stem, bytes));
    Ok(bytes)
}
