//! The flusher: frozen memtable → Parquet files → catalog → WAL trim (design §4.2).
//!
//! The order is the whole safety argument. Files are written first, each segment's text index sidecar after
//! its core and payload files (a crash leaves orphans, deleted on start); then one metastore transaction
//! registers them, saves the events' dedup keys and moves `flushed_seq` (a crash after it means replay skips `seq ≤ flushed_seq`); only then are WAL files deleted.
//! At every step each confirmed event, and its dedup key, is in the WAL, in the catalog, or both.
//!
//! A job's files and segment ids are made once and reused by every retry, so a retry after a commit that
//! landed but whose reply was lost is "already committed" in the catalog, not a second registration.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use tracedb_model::attrs::promote;
use tracedb_model::{RunId, Ts};
use tracedb_storage::fts;
use tracedb_storage::segment::{self, Version as Row};
use tracedb_storage::{DedupKeys, FieldStat, SegmentMeta, StorageError, Written};

use crate::dedup::{self, DedupKey};
use crate::flushed::FlushedIds;
use crate::memtable::Mem;
use crate::projects::Projects;
use crate::store::RunStore;
use crate::{Counters, Storage, crash_point, wal};

pub(crate) struct FlushJob {
    pub frozen: Arc<RunStore>,
    /// Every event in `frozen` has `seq ≤ max_seq`.
    pub max_seq: u64,
    /// Dedup keys of the events written to the WAL since the previous freeze.
    pub keys: Vec<(DedupKey, Ts)>,
}

pub(crate) struct Flusher {
    pub mem: Arc<RwLock<Mem>>,
    pub mvset: Arc<RwLock<HashSet<RunId>>>,
    pub flushed: Arc<Mutex<FlushedIds>>,
    pub projects: Arc<RwLock<Projects>>,
    pub storage: Arc<Storage>,
    pub counters: Arc<Counters>,
    pub flush_health: Arc<Mutex<FlushHealth>>,
    pub wal_dir: PathBuf,
    pub dedup_window: Duration,
    /// Tests only: slow every flush down (to exercise write slowdown).
    pub test_delay: Option<Duration>,
    /// Tests only: this many landed commits report an error, as a lost reply would.
    pub lost_replies: AtomicU32,
    /// Set when ingest shuts down: stop retrying a failing flush (the WAL still has the data).
    pub stop: Arc<std::sync::atomic::AtomicBool>,
}

/// A job's files, made once and kept across its retries.
struct Prepared {
    created_at: Ts,
    written: Vec<Written>,
    /// Every file is stored (a failed commit keeps them: it may have landed).
    stored: bool,
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
            let mut prepared = None;
            loop {
                // A panic in one attempt (a bug, or a storage task dying) must not stop flushing for good.
                let attempt =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.flush(&job, &mut prepared)));
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

    fn prepare(&self, job: &FlushJob) -> Result<Prepared, StorageError> {
        let now = Ts::now();
        let written = {
            let mvset = self.mvset.read().unwrap();
            let projects = self.projects.read().unwrap();
            let mut by_project: BTreeMap<String, Vec<Row>> = BTreeMap::new();
            for st in job.frozen.states() {
                // A version that doesn't name its project is filed under its run's, if known; the row
                // itself is unchanged.
                let project = projects.filed(st).to_string();
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
            drop(projects);
            by_project
                .into_iter()
                .map(|(project, rows)| segment::write(&project, rows, now))
                .collect::<Result<Vec<_>, _>>()?
        };
        if let Some(d) = self.test_delay {
            std::thread::sleep(d);
        }
        Ok(Prepared {
            created_at: now,
            written,
            stored: false,
        })
    }

    fn flush(&self, job: &FlushJob, prepared: &mut Option<Prepared>) -> Result<(), StorageError> {
        let started = std::time::Instant::now();
        if prepared.is_none() {
            *prepared = Some(self.prepare(job)?);
        }
        let p = prepared.as_mut().expect("prepared");
        let metas: Vec<SegmentMeta> = p.written.iter().map(|w| w.meta.clone()).collect();
        let stats: Vec<FieldStat> = p.written.iter().flat_map(|w| w.stats.iter().cloned()).collect();
        let store = self.storage.store.clone();
        if !p.stored {
            // Core, payload, then the text index sidecar (dictionary, postings), each with the crash point after it.
            let files: Vec<_> = p
                .written
                .iter()
                .flat_map(|w| {
                    let mut f = vec![
                        (w.meta.core_path.clone(), w.core.clone(), Some("during_put")),
                        (w.meta.payload_path.clone(), w.payload.clone(), None),
                    ];
                    if let Some(stem) = &w.meta.fts_path {
                        f.push((fts::dict_path(stem), w.fts.dict.clone(), None));
                        f.push((fts::post_path(stem), w.fts.post.clone(), Some("after_fts_put")));
                    }
                    f
                })
                .collect();
            self.storage.block(async move {
                let put = async {
                    // On a retry a name may already hold these bytes (a put that landed before the attempt
                    // failed); names are never reused, so that counts as stored.
                    let stored = |r: Result<(), StorageError>| match r {
                        Err(e) if e.already_exists() => Ok(()),
                        r => r,
                    };
                    for (path, bytes, point) in &files {
                        stored(store.put_new(path, bytes.clone()).await)?;
                        if let Some(point) = point {
                            crash_point(point);
                        }
                    }
                    Ok::<_, StorageError>(())
                }
                .await;
                if let Err(e) = put {
                    // Best effort: these files were never registered.
                    for (path, ..) in &files {
                        let _ = store.delete(path).await;
                    }
                    return Err(e);
                }
                Ok(())
            })?;
            p.stored = true;
        }
        crash_point("after_put");
        let meta = self.storage.meta.clone();
        let (m2, s2) = (metas.clone(), stats.clone());
        let max_seq = job.max_seq;
        let keys = DedupKeys {
            flushed_seq: max_seq,
            newest_at: job.keys.iter().map(|k| k.1.0).max().unwrap_or(0),
            keys: dedup::pack(&job.keys),
        };
        let forget_before = p.created_at.0.saturating_sub(self.dedup_window.as_micros() as i64);
        // A failed commit may still have landed (e.g. a lost reply), so its files are kept and the retry commits
        // the same segment ids; if it didn't land, they are orphans, deleted on the next start.
        self.storage
            .block(async move { meta.commit_flush(&m2, &s2, max_seq, &keys, forget_before).await })?;
        crash_point("after_commit");
        if self
            .lost_replies
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(StorageError::Metastore("test: the commit reply was lost".into()));
        }
        // Before the frozen memtable goes, so a new start for one of these runs always finds it in one of them.
        {
            let mut flushed = self.flushed.lock().unwrap();
            for st in job.frozen.states() {
                flushed.insert(&st.run.run_id);
            }
        }
        self.publish(metas, stats, job.max_seq);
        self.counters.flushed_seq.store(job.max_seq, Ordering::Relaxed);
        match wal::trim(&self.wal_dir, job.max_seq) {
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

/// After a restart, learns the projects of the newest files' runs (oldest of them first, so a run's first
/// project wins), so their late updates are filed under them. Best effort and off the flush path: a run not
/// found (yet) stays unknown, which is safe.
pub(crate) fn backfill_projects(mem: &RwLock<Mem>, storage: &Storage, projects: &RwLock<Projects>, stop: &AtomicBool) {
    let started = std::time::Instant::now();
    let mut segs: Vec<Arc<SegmentMeta>> = {
        let mem = mem.read().unwrap();
        mem.version
            .segments
            .iter()
            .filter(|s| !s.project.is_empty())
            .cloned()
            .collect()
    };
    segs.sort_by_key(|s| std::cmp::Reverse(s.max_seq));
    let mut rows = 0;
    segs.retain(|s| {
        rows += s.row_count;
        rows <= crate::projects::GENERATION as u64
    });
    let mut found = 0;
    for s in segs.iter().rev() {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let (store, path) = (storage.store.clone(), s.core_path.clone());
        let ids = storage
            .block(async move { store.get(&path).await })
            .and_then(segment::run_ids);
        match ids {
            Ok(ids) => {
                found += ids.len();
                projects
                    .write()
                    .unwrap()
                    .backfill(ids.into_iter().map(|id| (id, s.project.as_str())));
            }
            Err(e) => tracing::warn!(segment = %s.id, error = %e, "could not read run ids; skipped"),
        }
    }
    if found > 0 {
        tracing::info!(
            files = segs.len(),
            runs = found,
            ms = started.elapsed().as_millis() as u64,
            "learned recent runs' projects"
        );
    }
}
