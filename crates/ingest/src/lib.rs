//! Ingest service: accepts events, makes them durable in the WAL, applies them to the run store,
//! publishes changes, and only then confirms. Readers use [`Snapshot`]; they never write.

pub mod budget;
pub mod dedup;
pub mod hub;
pub mod store;
pub mod wal;
mod writer;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::thread::JoinHandle;
use std::time::Instant;

use serde::Serialize;
use tokio::sync::{Semaphore, broadcast, oneshot};
use tracedb_model::validate::json_size;
use tracedb_model::{
    Op, Reason, RunFields, RunHeader, RunId, RunView, Source, Status, TraceId, TraceView, Ts, validate,
};

pub use budget::Budget;
pub use store::{Change, Include, RootKey};

use crate::dedup::Dedup;
use crate::hub::Hub;
use crate::store::RunStore;
use crate::wal::{ReplayStats, Wal, WalError};

pub const DEFAULT_QUEUE_EVENTS: usize = 10_000;

#[derive(Clone, Debug)]
pub struct IngestConfig {
    pub data_dir: PathBuf,
    pub budget: Budget,
    pub queue_events: usize,
    pub wal_roll_bytes: u64,
    pub hub_capacity: usize,
    pub dedup_window_us: i64,
}

impl IngestConfig {
    pub fn new(data_dir: impl Into<PathBuf>) -> IngestConfig {
        IngestConfig {
            data_dir: data_dir.into(),
            budget: Budget::default(),
            queue_events: DEFAULT_QUEUE_EVENTS,
            wal_roll_bytes: wal::DEFAULT_ROLL_BYTES,
            hub_capacity: hub::DEFAULT_CAPACITY,
            dedup_window_us: dedup::DEFAULT_WINDOW_US,
        }
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
    #[error("too many events in flight; retry shortly")]
    Busy,
    #[error("memory budget reached; new runs are refused until M2 adds disk storage")]
    Capacity,
    #[error("storage error: {0}")]
    Storage(String),
    #[error("ingest is shut down")]
    Closed,
}

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error(transparent)]
    Wal(#[from] WalError),
    #[error("could not start the writer thread: {0}")]
    Thread(std::io::Error),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OpenStats {
    pub replayed_events: u64,
    pub replay_rejected: u64,
    pub wal_files: usize,
    pub truncated_bytes: u64,
    pub runs: usize,
    pub replay_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Health {
    pub ok: bool,
    pub wal_ok: bool,
    pub error: Option<String>,
    pub runs: usize,
    pub memory_bytes: u64,
    pub capacity_bytes: u64,
    pub max_runs: usize,
}

/// Read-only access to the run store, for the query service and live tail.
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
struct StoreReader(Arc<RwLock<RunStore>>);

impl Snapshot for StoreReader {
    fn run(&self, id: RunId, include: Include) -> Option<RunView> {
        self.0.read().unwrap().run(id, include)
    }
    fn trace(&self, trace_id: TraceId) -> Option<TraceView> {
        self.0.read().unwrap().trace(trace_id)
    }
    fn roots(
        &self,
        project: Option<&str>,
        status: Option<Status>,
        after: Option<RootKey>,
        limit: usize,
    ) -> (Vec<RunHeader>, Option<RootKey>) {
        self.0.read().unwrap().roots(project, status, after, limit)
    }
    fn projects(&self) -> Vec<(String, usize)> {
        self.0.read().unwrap().projects()
    }
    fn last_seq(&self) -> u64 {
        self.0.read().unwrap().last_seq()
    }
}

struct Inner {
    tx: Mutex<Option<mpsc::Sender<writer::Request>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    permits: Arc<Semaphore>,
    queue_events: usize,
    store: Arc<RwLock<RunStore>>,
    hub: Hub,
    budget: Budget,
    health: Arc<Mutex<Option<String>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.tx.lock().unwrap().take();
        if let Some(t) = self.thread.lock().unwrap().take() {
            let _ = t.join();
        }
    }
}

#[derive(Clone)]
pub struct Ingest {
    inner: Arc<Inner>,
}

impl Ingest {
    /// Replays the WAL into a fresh run store, then starts the writer.
    pub fn open(cfg: IngestConfig) -> Result<(Ingest, OpenStats), OpenError> {
        let started = Instant::now();
        let mut store = RunStore::new();
        let mut dedup = Dedup::new(cfg.dedup_window_us);
        let now = Ts::now();
        let mut replay_rejected = 0;
        let (wal, stats): (Wal, ReplayStats) = Wal::open(&cfg.data_dir.join("wal"), cfg.wal_roll_bytes, |ev| {
            if let Some(id) = ev.fields.id
                && dedup.in_window(ev.received_at, now)
            {
                dedup.insert((id, ev.op, ev.body_hash), ev.received_at);
            }
            if store.apply(&ev).is_err() {
                replay_rejected += 1;
            }
        })?;
        let open_stats = OpenStats {
            replayed_events: stats.records,
            replay_rejected,
            wal_files: stats.files,
            truncated_bytes: stats.truncated_bytes,
            runs: store.len(),
            replay_ms: started.elapsed().as_millis() as u64,
        };

        let store = Arc::new(RwLock::new(store));
        let hub = Hub::new(cfg.hub_capacity);
        let health = Arc::new(Mutex::new(None));
        let (tx, rx) = mpsc::channel();
        let shared = writer::Shared {
            store: store.clone(),
            hub: hub.clone(),
            budget: cfg.budget,
            health: health.clone(),
        };
        let last_seq = stats.last_seq;
        let thread = std::thread::Builder::new()
            .name("tracedb-writer".into())
            .spawn(move || writer::run(rx, wal, dedup, last_seq, shared))
            .map_err(OpenError::Thread)?;

        let inner = Inner {
            tx: Mutex::new(Some(tx)),
            thread: Mutex::new(Some(thread)),
            permits: Arc::new(Semaphore::new(cfg.queue_events)),
            queue_events: cfg.queue_events,
            store,
            hub,
            budget: cfg.budget,
            health,
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

    pub fn snapshot(&self) -> Arc<dyn Snapshot> {
        Arc::new(StoreReader(self.inner.store.clone()))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Change> {
        self.inner.hub.subscribe()
    }

    pub fn health(&self) -> Health {
        let error = self.inner.health.lock().unwrap().clone();
        let store = self.inner.store.read().unwrap();
        Health {
            ok: error.is_none(),
            wal_ok: error.is_none(),
            error,
            runs: store.len(),
            memory_bytes: store.bytes(),
            capacity_bytes: self.inner.budget.max_bytes,
            max_runs: self.inner.budget.max_runs,
        }
    }
}
