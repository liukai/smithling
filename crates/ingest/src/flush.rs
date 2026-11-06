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
use crate::{Counters, Storage, wal};

pub(crate) struct FlushJob {
    pub frozen: Arc<RunStore>,
    /// Every event in `frozen` has `seq ≤ max_seq`.
    pub max_seq: u64,
    /// WAL bytes written for this memtable (subtracted from the unflushed total once flushed).
    pub wal_bytes: u64,
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
}

#[derive(Clone, Debug, Default)]
pub struct FlushHealth {
    pub error: Option<String>,
    pub last_flush_at: Option<Ts>,
    pub flushes: u64,
}

/// Test hook: `TRACEDB_TEST_CRASH_AT=<point>` aborts the process at that point of a flush.
fn crash_point(point: &str) {
    if std::env::var("TRACEDB_TEST_CRASH_AT").is_ok_and(|p| p == point) {
        eprintln!("crash point: {point}");
        std::process::abort();
    }
}

impl Flusher {
    pub fn run(self, rx: Receiver<FlushJob>) {
        while let Ok(job) = rx.recv() {
            let mut backoff = Duration::from_millis(200);
            loop {
                match self.flush(&job) {
                    Ok(()) => break,
                    Err(e) => {
                        tracing::error!(error = %e, retry_in_ms = backoff.as_millis() as u64, "flush failed; the frozen memtable is kept");
                        self.flush_health.lock().unwrap().error = Some(e.to_string());
                        std::thread::sleep(backoff);
                        backoff = (backoff * 2).min(Duration::from_secs(30));
                    }
                }
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
                    attrs: promote(&st.run),
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
            for w in &written {
                store.put_new(&w.meta.core_path, w.core.clone()).await?;
                store.put_new(&w.meta.payload_path, w.payload.clone()).await?;
            }
            crash_point("after_put");
            meta.commit_flush(&m2, &s2, max_seq).await?;
            crash_point("after_commit");
            Ok::<_, StorageError>(())
        })?;
        self.publish(metas, stats, job.max_seq);
        self.counters.unflushed_wal.fetch_sub(
            job.wal_bytes.min(self.counters.unflushed_wal.load(Ordering::Relaxed)),
            Ordering::Relaxed,
        );
        self.counters.flushed_seq.store(job.max_seq, Ordering::Relaxed);
        let keep_since = SystemTime::now() - self.dedup_window;
        match wal::trim(&self.wal_dir, job.max_seq, keep_since) {
            Ok(n) if n > 0 => tracing::debug!(files = n, "trimmed wal"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "wal trim failed; will retry after the next flush"),
        }
        crash_point("after_trim");
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
