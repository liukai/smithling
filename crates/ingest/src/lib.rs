//! Ingest service: accepts events, makes them durable in the WAL, applies them to the active memtable,
//! publishes changes, and only then confirms. A full memtable is frozen and flushed to data files in the
//! background (design §4.2). Readers get a consistent view of memtables and files; they never write.

mod backfill;
pub mod budget;
pub mod dedup;
pub mod feedback;
pub mod flush;
pub mod flushed;
pub mod hub;
pub mod memtable;
pub mod projects;
mod recover;
pub mod store;
pub mod wal;
mod writer;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{Semaphore, broadcast, oneshot};
use tracedb_model::validate::json_size;
use tracedb_model::{
    Op, Reason, RunFields, RunHeader, RunId, RunState, RunView, Source, Status, TraceId, TraceView, Ts, merge_versions,
    validate,
};
use tracedb_storage::cache::Cache;
use tracedb_storage::{Metastore, SegmentMeta, StorageError, Store};

pub use feedback::{Feedback, FeedbackError, FeedbackLookup, Feedbacks, KeyStat};
pub use flush::FlushHealth;
pub use memtable::{Mem, Version};
pub use store::{Change, Include, RootKey};

use crate::feedback::FeedbackStore;
use crate::flushed::FlushedIds;
use crate::hub::Hub;
use crate::wal::WalError;

pub const DEFAULT_QUEUE_EVENTS: usize = 10_000;
/// Heap, as [`store::RunStore::bytes`] estimates it (M2: 128 MB of JSON, ~4× that in heap).
pub const DEFAULT_MEMTABLE_BYTES: u64 = 64 * 1024 * 1024;
pub const DEFAULT_FLUSH_AGE_US: i64 = 2 * 60 * 1_000_000;
pub const DEFAULT_MAX_UNFLUSHED_WAL: u64 = 1024 * 1024 * 1024;

/// When to freeze a memtable and when to tell clients to slow down (design §4.1, R-8, R-10), and the memory
/// held for reads.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub memtable_bytes: u64,
    pub flush_age_us: i64,
    pub max_unflushed_wal: u64,
    pub dedup_window_us: i64,
    /// Dedup keys kept at most (the oldest go first).
    pub dedup_keys: usize,
    /// The data cache's bound ([`tracedb_storage::cache::Cache`]).
    pub cache_bytes: u64,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            memtable_bytes: DEFAULT_MEMTABLE_BYTES,
            flush_age_us: DEFAULT_FLUSH_AGE_US,
            max_unflushed_wal: DEFAULT_MAX_UNFLUSHED_WAL,
            dedup_window_us: dedup::DEFAULT_WINDOW_US,
            dedup_keys: dedup::DEFAULT_CAP,
            cache_bytes: tracedb_storage::cache::DEFAULT_BYTES,
        }
    }
}

#[derive(Clone, Debug)]
pub struct IngestConfig {
    pub data_dir: PathBuf,
    /// Data files: a folder or `s3://bucket/prefix`. Default: `<data>/data`.
    pub storage: Option<String>,
    /// Catalog: `sqlite://…` or `postgres://…`. Default: `sqlite://<data>/meta.sqlite`.
    pub metastore: Option<String>,
    pub limits: Limits,
    pub queue_events: usize,
    pub wal_roll_bytes: u64,
    pub hub_capacity: usize,
    /// Tests only: delay every flush by this much (after it has read the multi-version set).
    pub test_flush_delay: Option<Duration>,
    /// Tests only: the first this many flush commits that land report an error, as if the reply were lost.
    pub test_lost_commit_replies: u32,
}

impl IngestConfig {
    pub fn new(data_dir: impl Into<PathBuf>) -> IngestConfig {
        IngestConfig {
            data_dir: data_dir.into(),
            storage: None,
            metastore: None,
            limits: Limits::default(),
            queue_events: DEFAULT_QUEUE_EVENTS,
            wal_roll_bytes: wal::DEFAULT_ROLL_BYTES,
            hub_capacity: hub::DEFAULT_CAPACITY,
            test_flush_delay: None,
            test_lost_commit_replies: 0,
        }
    }

    pub fn storage_spec(&self) -> String {
        self.storage
            .clone()
            .unwrap_or_else(|| self.data_dir.join("data").display().to_string())
    }

    pub fn metastore_url(&self) -> String {
        self.metastore
            .clone()
            .unwrap_or_else(|| format!("sqlite://{}", self.data_dir.join("meta.sqlite").display()))
    }
}

/// One uploaded item before it is numbered.
#[derive(Clone, Debug)]
pub struct Incoming {
    pub op: Op,
    pub source: Source,
    pub body_hash: u64,
    pub fields: RunFields,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Rejected {
    pub id: Option<RunId>,
    pub reason: Reason,
    pub detail: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct AcceptResult {
    pub accepted: usize,
    pub rejected: Vec<Rejected>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AcceptError {
    #[error("tracedb is busy; retry shortly")]
    Busy,
    #[error("storage error: {0}")]
    Storage(String),
    #[error("ingest is shut down")]
    Closed,
}

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error(transparent)]
    Wal(#[from] WalError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("could not start a thread: {0}")]
    Thread(std::io::Error),
    #[error("data folder {0} is in use by another tracedb process")]
    Locked(PathBuf),
    #[error("could not lock the data folder: {0}")]
    Lock(std::io::Error),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OpenStats {
    pub replayed_events: u64,
    /// Events already in data files, skipped by replay: left by a crash between a flush's catalog commit and its
    /// WAL trim (or by an older M2 folder). Their dedup keys are saved, then their WAL files trimmed.
    pub skipped_flushed: u64,
    pub replay_rejected: u64,
    pub wal_files: usize,
    pub truncated_bytes: u64,
    pub runs: usize,
    pub segments: usize,
    pub orphans_removed: usize,
    pub multi_version_runs: usize,
    pub flushed_seq: u64,
    pub replay_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Health {
    pub ok: bool,
    pub wal_ok: bool,
    pub flush_ok: bool,
    pub error: Option<String>,
    pub flush_error: Option<String>,
    /// Runs in the active and frozen memtables.
    pub runs: usize,
    pub memtable_bytes: u64,
    pub frozen: bool,
    pub unflushed_wal_bytes: u64,
    pub segments: usize,
    /// Data files without a text index (written by M2, until backfilled); searched by reading them.
    pub unindexed_segments: usize,
    /// Run versions in data files.
    pub segment_rows: u64,
    pub flushed_seq: u64,
    pub flushes: u64,
    pub last_flush_at: Option<Ts>,
    /// Registered segments found missing or unreadable, at start or while serving; their runs are not served.
    pub damaged_segments: Vec<Damaged>,
    /// The ids of `damaged_segments` (the M2 field).
    pub corrupt_segments: Vec<String>,
    /// Feedback items whose run hasn't arrived (a flushed-ids false positive can hide one).
    pub orphan_feedback: usize,
    /// Keys in the dedup window (0 once it has drained).
    pub dedup_keys: u64,
    /// Bytes in the data cache.
    pub cache_bytes: u64,
}

/// A registered segment that can't be served (M3-FR-18).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Damaged {
    pub id: String,
    /// `missing` or `unreadable`.
    pub reason: String,
    pub since: Ts,
    #[serde(skip)]
    pub segment: Arc<SegmentMeta>,
}

/// Test hook: `TRACEDB_TEST_CRASH_AT=<point>` aborts the process at that point of a flush.
pub(crate) fn crash_point(point: &str) {
    if std::env::var("TRACEDB_TEST_CRASH_AT").is_ok_and(|p| p == point) {
        eprintln!("crash point: {point}");
        std::process::abort();
    }
}

/// Shared counters between the writer and the flusher.
#[derive(Debug, Default)]
pub struct Counters {
    pub unflushed_wal: AtomicU64,
    pub flushed_seq: AtomicU64,
    /// Keys in the writer's dedup window.
    pub dedup_keys: AtomicU64,
}

/// Storage handles plus a small runtime of their own, so sync code (the flusher, `open`) can wait on
/// async storage calls from any thread.
pub struct Storage {
    pub store: Store,
    pub meta: Metastore,
    /// Footers, index pages, postings and payload pages read back (counted in idle memory).
    pub cache: Arc<Cache>,
    rt: StorageRuntime,
}

struct StorageRuntime(Option<tokio::runtime::Runtime>);

impl Drop for StorageRuntime {
    fn drop(&mut self) {
        // Allowed from inside async code, unlike dropping a runtime.
        if let Some(rt) = self.0.take() {
            rt.shutdown_background();
        }
    }
}

impl Storage {
    pub fn open(cfg: &IngestConfig) -> Result<Storage, StorageError> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("tracedb-storage")
            .enable_all()
            .build()?;
        let store = Store::open(&cfg.storage_spec())?;
        let url = cfg.metastore_url();
        let meta = block_on(&rt, async move { Metastore::open(&url).await })?;
        Ok(Storage {
            store,
            meta,
            cache: Arc::new(Cache::new(cfg.limits.cache_bytes)),
            rt: StorageRuntime(Some(rt)),
        })
    }

    /// Runs `fut` on the storage runtime and waits for it. Safe to call from any thread, including from
    /// inside another runtime (it only blocks the calling thread).
    pub fn block<F>(&self, fut: F) -> F::Output
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        block_on(self.rt.0.as_ref().expect("storage runtime is running"), fut)
    }

    /// Runs `fut` on the storage runtime and awaits it from any runtime without blocking a thread.
    pub async fn run<F>(&self, fut: F) -> F::Output
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let rt = self.rt.0.as_ref().expect("storage runtime is running");
        match rt.spawn(fut).await {
            Ok(v) => v,
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(e) => panic!("storage task cancelled: {e}"),
        }
    }
}

fn block_on<F>(rt: &tokio::runtime::Runtime, fut: F) -> F::Output
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    rt.spawn(async move {
        let _ = tx.send(fut.await);
    });
    rx.recv().expect("storage task finished")
}

/// Read-only access to the fresh data (active + frozen memtables), for the query service and live tail.
pub trait Snapshot: Send + Sync {
    fn run(&self, id: RunId, include: Include) -> Option<RunView>;
    fn trace(&self, trace_id: TraceId) -> Option<TraceView>;
    fn roots(
        &self,
        project: Option<&str>,
        status: Option<Status>,
        after: Option<RootKey>,
        limit: usize,
    ) -> (Vec<RunHeader>, Option<RootKey>);
    fn projects(&self) -> Vec<(String, usize)>;
    fn last_seq(&self) -> u64;
}

#[derive(Clone)]
struct MemReader(Arc<RwLock<Mem>>);

/// The newest merged state of a run across the frozen and active memtables.
fn fresh_state(mem: &Mem, id: &RunId) -> Option<RunState> {
    let a = mem.active.get(id).cloned();
    let f = mem.frozen.as_ref().and_then(|f| f.get(id)).cloned();
    match (f, a) {
        (Some(f), Some(a)) => merge_versions(vec![f, a]),
        (f, a) => a.or(f),
    }
}

fn child_count(mem: &Mem, id: RunId) -> u32 {
    mem.active.child_count_of(id) + mem.frozen.as_ref().map_or(0, |f| f.child_count_of(id))
}

impl Snapshot for MemReader {
    fn run(&self, id: RunId, include: Include) -> Option<RunView> {
        let mem = self.0.read().unwrap();
        let st = fresh_state(&mem, &id)?;
        Some(RunView {
            header: RunHeader::from_state(&st, child_count(&mem, id)),
            inputs: include.inputs.then(|| st.run.inputs.clone()).flatten(),
            outputs: include.outputs.then(|| st.run.outputs.clone()).flatten(),
            events: include.events.then(|| st.run.events.clone()).flatten(),
        })
    }

    fn trace(&self, trace_id: TraceId) -> Option<TraceView> {
        let mem = self.0.read().unwrap();
        let mut ids: Vec<RunId> = mem.active.trace_run_ids(&trace_id).to_vec();
        if let Some(f) = &mem.frozen {
            ids.extend_from_slice(f.trace_run_ids(&trace_id));
        }
        ids.sort();
        ids.dedup();
        if ids.is_empty() {
            return None;
        }
        let mut runs: Vec<RunHeader> = ids
            .iter()
            .filter_map(|id| fresh_state(&mem, id).map(|st| RunHeader::from_state(&st, child_count(&mem, *id))))
            .collect();
        runs.sort_by(|a, b| a.dotted_order.cmp(&b.dotted_order));
        Some(TraceView {
            root: runs.iter().find(|r| r.is_root).cloned(),
            run_count: runs.len(),
            has_running: runs.iter().any(|r| r.status == Status::Running),
            runs,
        })
    }

    fn roots(
        &self,
        project: Option<&str>,
        status: Option<Status>,
        after: Option<RootKey>,
        limit: usize,
    ) -> (Vec<RunHeader>, Option<RootKey>) {
        let mem = self.0.read().unwrap();
        // Candidates from both memtables; a run in both is merged, then filtered and ordered again.
        let mut cands: Vec<RunHeader> = mem.active.roots(project, None, after, usize::MAX / 2).0;
        if let Some(f) = &mem.frozen {
            cands.extend(f.roots(project, None, after, usize::MAX / 2).0);
        }
        let mut seen = HashSet::new();
        let mut rows: Vec<(RootKey, RunHeader)> = cands
            .into_iter()
            .filter(|h| seen.insert(h.run_id))
            .filter_map(|h| fresh_state(&mem, &h.run_id))
            .filter(|st| st.is_listed_root() && status.is_none_or(|s| st.run.status == s))
            .filter_map(|st| {
                let key = RootKey {
                    start_time: st.run.start_time?,
                    run_id: st.run.run_id,
                };
                Some((key, RunHeader::from_state(&st, child_count(&mem, st.run.run_id))))
            })
            .collect();
        rows.sort_by_key(|r| (std::cmp::Reverse(r.0.start_time), r.0.run_id));
        let more = rows.len() > limit;
        rows.truncate(limit);
        let next = if more { rows.last().map(|r| r.0) } else { None };
        (rows.into_iter().map(|r| r.1).collect(), next)
    }

    fn projects(&self) -> Vec<(String, usize)> {
        let mem = self.0.read().unwrap();
        let mut counts: std::collections::BTreeMap<String, usize> = mem.active.projects().into_iter().collect();
        if let Some(f) = &mem.frozen {
            for (p, n) in f.projects() {
                *counts.entry(p).or_default() += n;
            }
        }
        counts.into_iter().collect()
    }

    fn last_seq(&self) -> u64 {
        let mem = self.0.read().unwrap();
        mem.active
            .last_seq()
            .max(mem.frozen.as_ref().map_or(0, |f| f.last_seq()))
            .max(mem.version.flushed_seq)
    }
}

struct Inner {
    tx: Mutex<Option<mpsc::Sender<writer::Request>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    flusher: Mutex<Option<JoinHandle<()>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    permits: Arc<Semaphore>,
    queue_events: usize,
    mem: Arc<RwLock<Mem>>,
    mvset: Arc<RwLock<HashSet<RunId>>>,
    projects: Arc<RwLock<projects::Projects>>,
    backfill: Mutex<Option<JoinHandle<()>>>,
    hub: Hub,
    health: Arc<Mutex<Option<String>>>,
    flush_health: Arc<Mutex<FlushHealth>>,
    counters: Arc<Counters>,
    storage: Arc<Storage>,
    damaged: RwLock<BTreeMap<String, Damaged>>,
    flushed: Arc<Mutex<FlushedIds>>,
    feedback: FeedbackStore,
    /// Held for the life of the process: a second process on the same folder would delete our files as orphans.
    _lock: std::fs::File,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Stop the writer and wait for it, then let the flusher finish (or abandon a failing) flush and wait
        // for it too, so a reopen in the same process never races an old flush. Anything not yet flushed
        // stays in the WAL and is replayed on the next start.
        self.tx.lock().unwrap().take();
        if let Some(t) = self.thread.lock().unwrap().take() {
            let _ = t.join();
        }
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.backfill.lock().unwrap().take() {
            let _ = t.join();
        }
        if let Some(t) = self.flusher.lock().unwrap().take() {
            let _ = t.join();
        }
    }
}

fn lock_data_dir(dir: &Path) -> Result<std::fs::File, OpenError> {
    std::fs::create_dir_all(dir).map_err(OpenError::Lock)?;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("LOCK"))
        .map_err(OpenError::Lock)?;
    match f.try_lock() {
        Ok(()) => Ok(f),
        Err(std::fs::TryLockError::WouldBlock) => Err(OpenError::Locked(dir.to_path_buf())),
        Err(std::fs::TryLockError::Error(e)) => Err(OpenError::Lock(e)),
    }
}

#[derive(Clone)]
pub struct Ingest {
    inner: Arc<Inner>,
}

impl Ingest {
    /// Opens storage and the catalog, removes orphan files, rebuilds the multi-version set, replays the WAL
    /// past `flushed_seq` into the active memtable, then starts the writer and the flusher (design §4.6).
    pub fn open(cfg: IngestConfig) -> Result<(Ingest, OpenStats), OpenError> {
        let lock = lock_data_dir(&cfg.data_dir)?;
        let storage = Arc::new(Storage::open(&cfg)?);
        let recover::Recovered {
            wal,
            wal_stats,
            active,
            dedup,
            pending,
            mvset,
            flushed,
            version,
            damaged,
            stats: open_stats,
        } = recover::recover(&cfg, &storage)?;
        let flushed_seq = version.flushed_seq;
        let mut projects = projects::Projects::default();
        for st in active.states().filter(|st| st.project_set) {
            projects.note(st.run.run_id, &st.run.project);
        }
        let projects = Arc::new(RwLock::new(projects));

        let mem = Arc::new(RwLock::new(Mem {
            active,
            frozen: None,
            version: Arc::new(version),
        }));
        let mvset = Arc::new(RwLock::new(mvset));
        let flushed = Arc::new(Mutex::new(flushed));
        let feedback = FeedbackStore::load(storage.clone())?;
        let unflushed = wal::unflushed_bytes(&cfg.data_dir.join("wal"), flushed_seq).unwrap_or(0);
        let counters = Arc::new(Counters {
            unflushed_wal: AtomicU64::new(unflushed),
            flushed_seq: AtomicU64::new(flushed_seq),
            dedup_keys: AtomicU64::new(dedup.len() as u64),
        });
        let flush_health = Arc::new(Mutex::new(FlushHealth::default()));
        let hub = Hub::new(cfg.hub_capacity);
        let health = Arc::new(Mutex::new(None));

        let (flush_tx, flush_rx) = mpsc::channel();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flusher = flush::Flusher {
            mem: mem.clone(),
            mvset: mvset.clone(),
            flushed: flushed.clone(),
            projects: projects.clone(),
            storage: storage.clone(),
            counters: counters.clone(),
            flush_health: flush_health.clone(),
            wal_dir: cfg.data_dir.join("wal"),
            dedup_window: Duration::from_micros(cfg.limits.dedup_window_us.max(0) as u64),
            test_delay: cfg.test_flush_delay,
            lost_replies: std::sync::atomic::AtomicU32::new(cfg.test_lost_commit_replies),
            stop: stop.clone(),
        };
        let flusher_thread = std::thread::Builder::new()
            .name("tracedb-flusher".into())
            .spawn(move || flusher.run(flush_rx))
            .map_err(OpenError::Thread)?;

        let (tx, rx) = mpsc::channel();
        let shared = writer::Shared {
            mem: mem.clone(),
            hub: hub.clone(),
            health: health.clone(),
            mvset: mvset.clone(),
            flushed: flushed.clone(),
            projects: projects.clone(),
            counters: counters.clone(),
            limits: cfg.limits,
            flush_tx,
        };
        let last_seq = wal_stats.last_seq.max(flushed_seq);
        let thread = std::thread::Builder::new()
            .name("tracedb-writer".into())
            .spawn(move || writer::run(rx, wal, dedup, pending, last_seq, shared))
            .map_err(OpenError::Thread)?;

        let backfill = {
            let (mem, storage, projects, stop) = (mem.clone(), storage.clone(), projects.clone(), stop.clone());
            let (health, age) = (flush_health.clone(), cfg.limits.flush_age_us.max(0) as u64);
            std::thread::Builder::new()
                .name("tracedb-backfill".into())
                .spawn(move || {
                    flush::backfill_projects(&mem, &storage, &projects, &stop);
                    backfill::run(&mem, &storage, &health, Duration::from_micros(age), &stop);
                })
                .map_err(OpenError::Thread)?
        };
        let inner = Inner {
            tx: Mutex::new(Some(tx)),
            thread: Mutex::new(Some(thread)),
            flusher: Mutex::new(Some(flusher_thread)),
            backfill: Mutex::new(Some(backfill)),
            stop,
            permits: Arc::new(Semaphore::new(cfg.queue_events)),
            queue_events: cfg.queue_events,
            mem,
            mvset,
            projects,
            hub,
            health,
            flush_health,
            counters,
            storage,
            damaged: RwLock::new(damaged.into_iter().map(|d| (d.id.clone(), d)).collect()),
            flushed,
            feedback,
            _lock: lock,
        };
        Ok((Ingest { inner: Arc::new(inner) }, open_stats))
    }

    /// Validates each item, then waits until the valid ones are durable and applied.
    pub async fn accept(&self, items: Vec<Incoming>) -> Result<AcceptResult, AcceptError> {
        let mut rejected = Vec::new();
        let mut valid = Vec::with_capacity(items.len());
        let mut bytes = 0;
        for mut it in items {
            match validate(it.op, &mut it.fields) {
                Ok(_) => {
                    bytes += json_size(&it.fields) as usize;
                    valid.push(it);
                }
                Err(rej) => rejected.push(Rejected {
                    id: it.fields.id,
                    reason: rej.reason,
                    detail: rej.detail,
                }),
            }
        }
        if valid.is_empty() {
            return Ok(AcceptResult { accepted: 0, rejected });
        }
        if let Some(err) = self.inner.health.lock().unwrap().clone() {
            return Err(AcceptError::Storage(err));
        }

        let n = valid.len().clamp(1, self.inner.queue_events) as u32;
        let permit = self
            .inner
            .permits
            .clone()
            .try_acquire_many_owned(n)
            .map_err(|_| AcceptError::Busy)?;
        let (reply, wait) = oneshot::channel();
        let req = writer::Request {
            items: valid,
            bytes,
            reply,
            _permit: permit,
        };
        {
            let tx = self.inner.tx.lock().unwrap();
            tx.as_ref()
                .ok_or(AcceptError::Closed)?
                .send(req)
                .map_err(|_| AcceptError::Closed)?;
        }
        let mut result = wait.await.map_err(|_| AcceptError::Closed)??;
        result.rejected.extend(rejected);
        Ok(result)
    }

    /// Fresh data only (active + frozen memtables); the query service adds files.
    pub fn snapshot(&self) -> Arc<dyn Snapshot> {
        Arc::new(MemReader(self.inner.mem.clone()))
    }

    /// Memtables and the pinned file set, for the query service (design §4.3).
    pub fn mem(&self) -> Arc<RwLock<Mem>> {
        self.inner.mem.clone()
    }

    pub fn multi_version(&self) -> Arc<RwLock<HashSet<RunId>>> {
        self.inner.mvset.clone()
    }

    /// Recent runs' projects: where a version that doesn't name its project is filed.
    pub fn projects(&self) -> Arc<RwLock<projects::Projects>> {
        self.inner.projects.clone()
    }

    pub fn storage(&self) -> Arc<Storage> {
        self.inner.storage.clone()
    }

    /// Segments that can't be served, by id.
    pub fn damaged(&self) -> Vec<Damaged> {
        self.inner.damaged.read().unwrap().values().cloned().collect()
    }

    pub fn is_damaged(&self, id: &str) -> bool {
        self.inner.damaged.read().unwrap().contains_key(id)
    }

    /// Records a segment found missing or unreadable while serving and stops serving it: later reads don't
    /// see it, reads in flight skip it.
    pub fn mark_damaged(&self, segment: &SegmentMeta, reason: &str) {
        {
            let mut d = self.inner.damaged.write().unwrap();
            if d.contains_key(&segment.id) {
                return;
            }
            tracing::error!(segment = %segment.id, reason, "a data file can't be read; its runs are not served");
            d.insert(
                segment.id.clone(),
                Damaged {
                    id: segment.id.clone(),
                    reason: reason.to_string(),
                    since: Ts::now(),
                    segment: Arc::new(segment.clone()),
                },
            );
        }
        let mut mem = self.inner.mem.write().unwrap();
        mem.version = Arc::new(mem.version.without(&segment.id));
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Change> {
        self.inner.hub.subscribe()
    }

    /// Stores feedback items in one catalog transaction; confirmed once committed.
    pub async fn put_feedback(&self, items: Vec<Feedback>) -> Result<Vec<Feedback>, FeedbackError> {
        self.inner.feedback.put(items).await
    }

    /// Replaces the given fields of a stored item.
    pub async fn patch_feedback(&self, id: &str, patch: serde_json::Value) -> Result<Feedback, FeedbackError> {
        self.inner.feedback.patch(id, patch).await
    }

    /// Every stored item for these runs, by run, oldest first.
    pub async fn feedback_for_runs(&self, runs: Vec<RunId>) -> Result<Vec<Feedback>, FeedbackError> {
        self.inner.feedback.for_runs(runs).await
    }

    /// The in-memory feedback index (`run_id → (key, score)`).
    pub fn feedback(&self) -> Feedbacks {
        self.inner.feedback.index()
    }

    /// Runs some feedback points at that are in neither memtable and not in the flushed-ids filter. A filter
    /// false positive (about 1%) makes a missing run look present; there are no false "missing".
    pub fn feedback_runs_missing(&self) -> Vec<RunId> {
        let mut ids = self.feedback().run_ids();
        {
            let mem = self.inner.mem.read().unwrap();
            ids.retain(|id| !mem.active.contains(id) && !mem.frozen.as_ref().is_some_and(|f| f.contains(id)));
            // Same lock order as the writer (memtables, then the filter); the flusher adds to the filter before
            // it drops the frozen memtable, so a run in flight is in one of them.
            let flushed = self.inner.flushed.lock().unwrap();
            ids.retain(|id| !flushed.contains(id));
        }
        ids
    }

    pub fn health(&self) -> Health {
        let error = self.inner.health.lock().unwrap().clone();
        let fh = self.inner.flush_health.lock().unwrap().clone();
        let damaged = self.damaged();
        let fb = self.feedback();
        let orphan_feedback = self.feedback_runs_missing().iter().map(|r| fb.items(r)).sum();
        let cache_bytes = self.inner.storage.cache.bytes();
        let mem = self.inner.mem.read().unwrap();
        Health {
            ok: error.is_none() && fh.error.is_none() && damaged.is_empty(),
            wal_ok: error.is_none(),
            flush_ok: fh.error.is_none(),
            error,
            flush_error: fh.error,
            runs: mem.active.len() + mem.frozen.as_ref().map_or(0, |f| f.len()),
            memtable_bytes: mem.active.bytes(),
            frozen: mem.frozen.is_some(),
            unflushed_wal_bytes: self.inner.counters.unflushed_wal.load(Ordering::Relaxed),
            segments: mem.version.segments.len(),
            unindexed_segments: mem.version.segments.iter().filter(|s| s.fts_path.is_none()).count(),
            segment_rows: mem.version.segments.iter().map(|s| s.row_count).sum(),
            flushed_seq: mem.version.flushed_seq,
            flushes: fh.flushes,
            last_flush_at: fh.last_flush_at,
            corrupt_segments: damaged.iter().map(|d| d.id.clone()).collect(),
            damaged_segments: damaged,
            orphan_feedback,
            dedup_keys: self.inner.counters.dedup_keys.load(Ordering::Relaxed),
            cache_bytes,
        }
    }
}
