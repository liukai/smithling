//! Compaction (M4 plan): a job takes a prefix of the log — flushed (L0) files in log order, whole flushes, up to
//! the first one with a row whose window is still open — merges each run's versions into one row, and writes
//! one L1 file per project per window. Taking a prefix keeps the merge rule exact: everything left out is newer
//! than everything merged, so a run's versions keep covering disjoint seq ranges (M4 spec C8).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use smithling_model::attrs::promote;
use smithling_model::merge_versions;
use smithling_model::{RunId, RunState, Ts};
use smithling_storage::segment::{self, Version as Row};
use smithling_storage::{SegmentMeta, StorageError, Written};

use crate::Storage;

const MINUTE_US: i64 = 60_000_000;

/// Files compaction replaced, waiting for their grace period, with when they were replaced (µs).
pub type Replaced = Arc<std::sync::Mutex<Vec<(Arc<SegmentMeta>, i64)>>>;

#[derive(Clone, Debug)]
pub struct CompactConfig {
    pub enabled: bool,
    /// Width of a window of run start time (µs).
    pub window_us: i64,
    /// A window closes this long after its end (µs): late updates mostly arrive before then.
    pub settle_us: i64,
    /// Output files are split at about this size.
    pub target_bytes: u64,
    /// Rows read by one job at most; the next job continues the prefix.
    pub max_rows: u64,
    /// Replaced files stay this long before they're deleted (µs).
    pub grace_us: i64,
    /// How often the writer looks for work.
    pub interval: Duration,
}

impl Default for CompactConfig {
    fn default() -> Self {
        CompactConfig {
            enabled: true,
            window_us: 60 * MINUTE_US,
            settle_us: 15 * MINUTE_US,
            target_bytes: 256 << 20,
            max_rows: 1_000_000,
            grace_us: 24 * 60 * MINUTE_US,
            interval: Duration::from_secs(60),
        }
    }
}

/// Start of the window holding `start_key`.
pub fn window_of(start_key: i64, window_us: i64) -> i64 {
    start_key.div_euclid(window_us) * window_us
}

/// The job's inputs: whole flushes of L0 files, oldest first, while each file's newest window has closed (all of
/// them with `force`), stopping before a damaged file or past `max_rows`. Empty if there's nothing to do.
pub fn pick(
    segments: &[Arc<SegmentMeta>],
    is_damaged: &dyn Fn(&str) -> bool,
    now_us: i64,
    cfg: &CompactConfig,
    force: bool,
) -> Vec<Arc<SegmentMeta>> {
    let mut l0: Vec<Arc<SegmentMeta>> = segments.iter().filter(|s| s.tier == 0).cloned().collect();
    // Flushes cover disjoint, increasing seq ranges, so log order is `min_seq` order (not the wall clock, which
    // can step back); one flush's files share its `created_at` and sit next to each other in that order.
    l0.sort_by_key(|s| s.min_seq);
    let closed =
        |s: &SegmentMeta| force || window_of(s.max_start_key, cfg.window_us) + cfg.window_us + cfg.settle_us <= now_us;
    let mut out: Vec<Arc<SegmentMeta>> = Vec::new();
    let mut rows = 0u64;
    let mut i = 0;
    while i < l0.len() {
        let at = l0[i].created_at;
        let end = l0[i..]
            .iter()
            .position(|s| s.created_at != at)
            .map_or(l0.len(), |n| i + n);
        let flush = &l0[i..end];
        if flush.iter().any(|s| is_damaged(&s.id) || !closed(s)) {
            break;
        }
        let n: u64 = flush.iter().map(|s| s.row_count).sum();
        if !out.is_empty() && rows + n > cfg.max_rows {
            break;
        }
        rows += n;
        out.extend(flush.iter().cloned());
        i = end;
    }
    out
}

/// What one job writes; nothing is stored or registered yet.
pub struct Built {
    pub inputs: Vec<Arc<SegmentMeta>>,
    pub outputs: Vec<Written>,
    /// Run versions read from the inputs.
    pub rows_in: u64,
    /// Runs the job merged.
    pub runs: HashSet<RunId>,
}

/// Reads every input, merges each run's versions, marks runs that also have a row in an earlier L1 file of the
/// same window (`l1`: the active compacted files), and writes one or more L1 files per (project, window).
pub fn build(
    storage: &Storage,
    inputs: Vec<Arc<SegmentMeta>>,
    l1: &[Arc<SegmentMeta>],
    cfg: &CompactConfig,
    now: Ts,
) -> Result<Built, StorageError> {
    let mut versions: HashMap<RunId, Vec<RunState>> = HashMap::new();
    let mut rows_in = 0u64;
    let mut bytes_in = 0u64;
    for seg in &inputs {
        let (store, core, payload) = (storage.store.clone(), seg.core_path.clone(), seg.payload_path.clone());
        let (c, p) = storage
            .block(async move { Ok::<_, StorageError>((store.get(&core).await?, store.get(&payload).await?)) })?;
        bytes_in += (c.len() + p.len()) as u64;
        for v in segment::read_all(c, p)? {
            rows_in += 1;
            versions.entry(v.state.run.run_id).or_default().push(v.state);
        }
    }
    let runs: HashSet<RunId> = versions.keys().copied().collect();
    let merged: Vec<RunState> = versions.into_values().filter_map(merge_versions).collect();

    let window = |st: &RunState| window_of(segment::start_key(&st.run.dotted_order), cfg.window_us);
    let windows: HashSet<i64> = merged.iter().map(window).collect();
    let mut earlier: HashSet<RunId> = HashSet::new();
    for s in l1
        .iter()
        .filter(|s| windows.contains(&window_of(s.min_start_key, cfg.window_us)))
    {
        let (store, core) = (storage.store.clone(), s.core_path.clone());
        let c = storage.block(async move { store.get(&core).await })?;
        earlier.extend(segment::run_ids(c)?);
    }

    let mut groups: BTreeMap<(String, i64), Vec<Row>> = BTreeMap::new();
    for st in merged {
        let project = if st.project_set {
            st.run.project.clone()
        } else {
            String::new()
        };
        let key = (project, window(&st));
        groups.entry(key).or_default().push(Row {
            attrs: promote(&st.run),
            // A run whose first version here is an update has its start elsewhere (earlier or later).
            needs_merge: earlier.contains(&st.run.run_id) || st.created_by_patch(),
            state: st,
        });
    }
    let per_row = bytes_in.checked_div(rows_in).unwrap_or(1).max(1);
    let rows_per_file = (cfg.target_bytes / per_row).max(1) as usize;
    let mut outputs = Vec::new();
    for ((project, _), mut rows) in groups {
        // Sorted before splitting, so split files hold disjoint trace ranges.
        rows.sort_by(|a, b| {
            (a.state.run.trace_id, &a.state.run.dotted_order).cmp(&(b.state.run.trace_id, &b.state.run.dotted_order))
        });
        while !rows.is_empty() {
            let rest = rows.split_off(rows.len().min(rows_per_file));
            outputs.push(segment::write_tier(&project, rows, now, 1)?);
            rows = rest;
        }
    }
    Ok(Built {
        inputs,
        outputs,
        rows_in,
        runs,
    })
}

/// What a finished job did.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct CompactReport {
    pub inputs: usize,
    pub outputs: usize,
    pub rows_in: u64,
    pub rows_out: u64,
    pub ms: u64,
}

/// Compaction state shown in `/healthz`.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct CompactHealth {
    pub enabled: bool,
    pub running: bool,
    pub last_run_at: Option<Ts>,
    pub last_error: Option<String>,
    pub last: Option<CompactReport>,
    pub jobs: u64,
}

/// Runs jobs one at a time against the writer's state (M4 plan: steps 6–8).
pub(crate) struct Compactor {
    pub mem: Arc<std::sync::RwLock<crate::memtable::Mem>>,
    pub mvset: Arc<std::sync::RwLock<HashSet<RunId>>>,
    pub storage: Arc<Storage>,
    pub damaged: Arc<std::sync::RwLock<BTreeMap<String, crate::Damaged>>>,
    pub replaced: Replaced,
    pub cfg: CompactConfig,
    pub health: std::sync::Mutex<CompactHealth>,
    /// One job at a time.
    pub job: std::sync::Mutex<()>,
    /// Runs merged by a job, to leave the multi-version set once no query holds a file set from before its swap.
    pub shrink: std::sync::Mutex<Vec<(crate::memtable::Era, HashSet<RunId>)>>,
}

impl Compactor {
    /// One job: pick, build, store, swap, publish. `Ok(None)` when there was nothing to compact.
    pub fn run(&self, force: bool) -> Result<Option<CompactReport>, StorageError> {
        let _job = self.job.lock().unwrap();
        self.health.lock().unwrap().running = true;
        let r = self.job(force);
        let mut h = self.health.lock().unwrap();
        h.running = false;
        h.last_run_at = Some(Ts::now());
        match &r {
            Ok(Some(rep)) => {
                h.last_error = None;
                h.last = Some(rep.clone());
                h.jobs += 1;
            }
            Ok(None) => h.last_error = None,
            Err(e) => h.last_error = Some(e.to_string()),
        }
        drop(h);
        self.apply_shrink();
        r
    }

    fn job(&self, force: bool) -> Result<Option<CompactReport>, StorageError> {
        let started = std::time::Instant::now();
        let version = self.mem.read().unwrap().version.clone();
        // Damaged files are out of the version but still hold versions: they must stop the prefix.
        let damaged: Vec<Arc<SegmentMeta>> = self
            .damaged
            .read()
            .unwrap()
            .values()
            .map(|d| d.segment.clone())
            .collect();
        let bad: HashSet<String> = damaged.iter().map(|s| s.id.clone()).collect();
        let all: Vec<Arc<SegmentMeta>> = version.segments.iter().chain(&damaged).cloned().collect();
        let now = Ts::now();
        let inputs = pick(&all, &|id| bad.contains(id), now.0, &self.cfg, force);
        if inputs.is_empty() {
            return Ok(None);
        }
        let l1: Vec<Arc<SegmentMeta>> = version.segments.iter().filter(|s| s.tier > 0).cloned().collect();
        let built = build(&self.storage, inputs, &l1, &self.cfg, now)?;
        let input_ids: Vec<String> = built.inputs.iter().map(|s| s.id.clone()).collect();
        let metas: Vec<SegmentMeta> = built.outputs.iter().map(|w| w.meta.clone()).collect();
        let stats: Vec<_> = built.outputs.iter().flat_map(|w| w.stats.iter().cloned()).collect();
        // Known to the catalog before any byte is stored, so start-up can tell them from a stranger's files.
        let (meta, m2) = (self.storage.meta.clone(), metas.clone());
        self.storage.block(async move { meta.register_pending(&m2).await })?;
        self.store(&built)?;
        crate::crash_point("compact_after_put");
        let at = Ts::now().0;
        // The same outputs every attempt, so a retry after a commit that landed (lost reply) is "already done".
        let mut backoff = Duration::from_millis(100);
        for attempt in 1.. {
            let (meta, ids, m2, s2) = (
                self.storage.meta.clone(),
                input_ids.clone(),
                metas.clone(),
                stats.clone(),
            );
            match self
                .storage
                .block(async move { meta.commit_compaction(&ids, &m2, &s2, at).await })
            {
                Ok(()) => break,
                Err(e) if e.to_string().contains(smithling_storage::metastore::SWAP_CONFLICT) => {
                    self.unstore(&built);
                    return Err(e);
                }
                // Left pending: the next start removes them if the commit never landed.
                Err(e) if attempt >= 5 => return Err(e),
                Err(e) => {
                    tracing::warn!(error = %e, attempt, "compaction commit failed; retrying");
                    std::thread::sleep(backoff);
                    backoff *= 2;
                }
            }
        }
        crate::crash_point("compact_after_commit");
        let runs = built.runs.clone();
        let old = {
            let mut mem = self.mem.write().unwrap();
            let removed: HashSet<String> = input_ids.iter().cloned().collect();
            let next = Arc::new(mem.version.with_compaction(&removed, metas, stats));
            std::mem::replace(&mut mem.version, next).era.clone()
        };
        self.replaced
            .lock()
            .unwrap()
            .extend(built.inputs.iter().map(|s| (s.clone(), at)));
        self.shrink.lock().unwrap().push((old, runs));
        let rep = CompactReport {
            inputs: built.inputs.len(),
            outputs: built.outputs.len(),
            rows_in: built.rows_in,
            rows_out: built.outputs.iter().map(|w| w.meta.row_count).sum(),
            ms: started.elapsed().as_millis() as u64,
        };
        tracing::info!(
            inputs = rep.inputs,
            outputs = rep.outputs,
            rows_in = rep.rows_in,
            rows_out = rep.rows_out,
            ms = rep.ms,
            "compacted"
        );
        Ok(Some(rep))
    }

    /// Puts every output file (core, payload, then the text index sidecar); names are never reused, so one that
    /// already holds these bytes (a retry) counts as stored. On failure, removes what it put.
    fn store(&self, built: &Built) -> Result<(), StorageError> {
        let files = files_of(built);
        let store = self.storage.store.clone();
        self.storage.block(async move {
            let put = async {
                for (path, bytes) in &files {
                    match store.put_new(path, bytes.clone()).await {
                        Err(e) if e.already_exists() => {}
                        r => r?,
                    }
                }
                Ok::<_, StorageError>(())
            }
            .await;
            if put.is_err() {
                for (path, _) in &files {
                    let _ = store.delete(path).await;
                }
            }
            put
        })
    }

    /// Best effort: outputs of a refused swap, files then their pending rows (start-up removes any left).
    fn unstore(&self, built: &Built) {
        let files = files_of(built);
        let ids: Vec<String> = built.outputs.iter().map(|w| w.meta.id.clone()).collect();
        let (store, meta) = (self.storage.store.clone(), self.storage.meta.clone());
        self.storage.block(async move {
            for (path, _) in &files {
                let _ = store.delete(path).await;
            }
            for id in &ids {
                let _ = meta.delete_pending(id).await;
            }
        });
    }

    /// Lets merged runs leave the multi-version set, job by job in swap order, once no file set from before the
    /// job's swap is held (every version of that era is gone), keeping runs that still have a version in a
    /// memtable or a marked row in an active file.
    pub fn apply_shrink(&self) {
        let ready: Vec<HashSet<RunId>> = {
            let mut pending = self.shrink.lock().unwrap();
            // In order: a later era can only be free once the earlier ones are.
            let n = pending
                .iter()
                .take_while(|(era, _)| Arc::strong_count(era) == 1)
                .count();
            pending.drain(..n).map(|(_, runs)| runs).collect()
        };
        if ready.is_empty() {
            return;
        }
        let mut remove: HashSet<RunId> = ready.into_iter().flatten().collect();
        loop {
            let version = self.mem.read().unwrap().version.clone();
            let marked = match marked_ids(&self.storage, &version) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(error = %e, "could not read marked rows; the multi-version set keeps its runs");
                    return;
                }
            };
            let mem = self.mem.read().unwrap();
            if mem.version.id != version.id {
                continue; // a flush landed meanwhile: its marked rows must be read too
            }
            remove.retain(|id| {
                !marked.contains(id)
                    && mem.active.get(id).is_none()
                    && !mem.frozen.as_ref().is_some_and(|f| f.get(id).is_some())
            });
            self.mvset.write().unwrap().retain(|id| !remove.contains(id));
            return;
        }
    }
}

/// Run ids of `needs_merge` rows in every active file that has any.
fn marked_ids(storage: &Storage, version: &crate::memtable::Version) -> Result<HashSet<RunId>, StorageError> {
    let segs: Vec<SegmentMeta> = version
        .segments
        .iter()
        .filter(|s| s.needs_merge_rows > 0)
        .map(|s| (**s).clone())
        .collect();
    let store = storage.store.clone();
    let found = storage.block(async move { segment::inspect_all(store, segs).await });
    let mut out = HashSet::new();
    for r in found {
        out.extend(r?.needs_merge);
    }
    Ok(out)
}

fn files_of(built: &Built) -> Vec<(String, bytes::Bytes)> {
    built
        .outputs
        .iter()
        .flat_map(|w| {
            let mut f = vec![
                (w.meta.core_path.clone(), w.core.clone()),
                (w.meta.payload_path.clone(), w.payload.clone()),
            ];
            if let Some(stem) = &w.meta.fts_path {
                f.push((smithling_storage::fts::dict_path(stem), w.fts.dict.clone()));
                f.push((smithling_storage::fts::post_path(stem), w.fts.post.clone()));
            }
            f
        })
        .collect()
}
