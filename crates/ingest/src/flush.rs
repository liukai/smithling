//! The flusher: frozen memtable → Parquet files → catalog → WAL trim (design §4.2).
//!
//! The order is the whole safety argument. Files are written first (a crash leaves orphans, deleted on
//! start); then one metastore transaction registers them and moves `flushed_seq` (a crash after it means
//! replay skips `seq ≤ flushed_seq`); only then are WAL files deleted. At every step each confirmed event
//! is in the WAL, in registered files, or both.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use tracedb_model::attrs::promote;
use tracedb_model::{RunId, Ts};
use tracedb_storage::segment::{self, Version as Row};
use tracedb_storage::{FieldStat, SegmentMeta, StorageError};

use crate::memtable::Mem;
use crate::store::RunStore;
use crate::{Counters, Storage, crash_point, wal};

pub(crate) struct FlushJob {
    pub frozen: Arc<RunStore>,
    /// Every event in `frozen` has `seq ≤ max_seq`.
    pub max_seq: u64,
}

pub(crate) struct Flusher {
    pub mem: Arc<RwLock<Mem>>,
    pub mvset: Arc<RwLock<HashSet<RunId>>>,
    pub storage: Arc<Storage>,
    pub counters: Arc<Counters>,
    pub flush_health: Arc<Mutex<FlushHealth>>,
    pub wal_dir: PathBuf,
    pub dedup_window: Duration,
    /// Tests only: slow every flush down (to exercise write slowdown).
    pub test_delay: Option<Duration>,
    /// Set when ingest shuts down: stop retrying a failing flush (the WAL still has the data).
    pub stop: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Clone, Debug, Default)]
pub struct FlushHealth {
    pub error: Option<String>,
    pub last_flush_at: Option<Ts>,
    pub flushes: u64,
}

impl Flusher {
    pub fn run(self, rx: Receiver<FlushJob>) {
        while let Ok(job) = rx.recv() {
            let mut backoff = Duration::from_millis(200);
            loop {
                // A panic in one attempt (a bug, or a storage task dying) must not stop flushing for good.
                let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.flush(&job)));
                let err = match attempt {
                    Ok(Ok(())) => break,
                    Ok(Err(e)) => e.to_string(),
                    Err(p) => format!(
                        "flush panicked: {}",
                        p.downcast_ref::<String>()
                            .cloned()
                            .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                            .unwrap_or_default()
                    ),
                };
                tracing::error!(error = %err, retry_in_ms = backoff.as_millis() as u64, "flush failed; the frozen memtable is kept");
                self.flush_health.lock().unwrap().error = Some(err);
                if self.stop.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(backoff);
                if self.stop.load(Ordering::Relaxed) {
                    return;
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }

    fn flush(&self, job: &FlushJob) -> Result<(), StorageError> {
        if let Some(d) = self.test_delay {
            std::thread::sleep(d);
        }
        let started = std::time::Instant::now();
        let now = Ts::now();
        let (metas, stats, written) = {
            let mvset = self.mvset.read().unwrap();
            let mut by_project: BTreeMap<String, Vec<Row>> = BTreeMap::new();
            for st in job.frozen.states() {
                let project = if st.project_set {
                    st.run.project.clone()
                } else {
                    String::new()
                };
                let needs_merge = st.created_by_patch() || mvset.contains(&st.run.run_id);
                by_project.entry(project).or_default().push(Row {
                    attrs: job
                        .frozen
                        .attrs(&st.run.run_id)
                        .cloned()
                        .unwrap_or_else(|| promote(&st.run)),
                    state: st.clone(),
                    needs_merge,
                });
            }
            let mut metas = Vec::new();
            let mut stats = Vec::new();
            let mut written = Vec::new();
            for (project, rows) in by_project {
                let w = segment::write(&project, rows, now)?;
                metas.push(w.meta.clone());
                stats.extend(w.stats.iter().cloned());
                written.push(w);
            }
            (metas, stats, written)
        };
        let store = self.storage.store.clone();
        let meta = self.storage.meta.clone();
        let (m2, s2) = (metas.clone(), stats.clone());
        let max_seq = job.max_seq;
        self.storage.block(async move {
            let put = async {
                for w in &written {
                    store.put_new(&w.meta.core_path, w.core.clone()).await?;
                    crash_point("during_put");
                    store.put_new(&w.meta.payload_path, w.payload.clone()).await?;
                }
                Ok::<_, StorageError>(())
            }
            .await;
            if let Err(e) = put {
                // Best effort: these files were never registered.
                for w in &written {
                    let _ = store.delete(&w.meta.core_path).await;
                    let _ = store.delete(&w.meta.payload_path).await;
                }
                return Err(e);
            }
            crash_point("after_put");
            // A failed commit may still have landed (e.g. a lost reply), so its files are kept; if it didn't,
            // they are orphans, deleted on the next start.
            meta.commit_flush(&m2, &s2, max_seq).await?;
            crash_point("after_commit");
            Ok(())
        })?;
        self.publish(metas, stats, job.max_seq);
        self.counters.flushed_seq.store(job.max_seq, Ordering::Relaxed);
        let keep_since = SystemTime::now() - self.dedup_window;
        match wal::trim(&self.wal_dir, job.max_seq, keep_since) {
            Ok(n) if n > 0 => tracing::debug!(files = n, "trimmed wal"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "wal trim failed; will retry after the next flush"),
        }
        crash_point("after_trim");
        // Recount from the files rather than keeping a running total: exact after restarts and trims.
        if let Ok(b) = wal::unflushed_bytes(&self.wal_dir, job.max_seq) {
            self.counters.unflushed_wal.store(b, Ordering::Relaxed);
        }
        let mut h = self.flush_health.lock().unwrap();
        h.error = None;
        h.last_flush_at = Some(Ts::now());
        h.flushes += 1;
        tracing::info!(
            runs = job.frozen.len(),
            max_seq = job.max_seq,
            ms = started.elapsed().as_millis() as u64,
            "flushed memtable"
        );
        Ok(())
    }

    /// Readers see the new files and stop seeing the frozen memtable in the same step.
    fn publish(&self, metas: Vec<SegmentMeta>, stats: Vec<FieldStat>, flushed_seq: u64) {
        let mut mem = self.mem.write().unwrap();
        mem.version = Arc::new(mem.version.with_flush(metas, stats, flushed_seq));
        mem.frozen = None;
    }
}
