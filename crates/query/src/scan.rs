//! Collecting merged runs for a scope: memtables ∪ files, with the single-version fast path (design §4.3).
//!
//! 1. Pin: under the memtable lock take the file set (`Version`), the frozen memtable, and copies (without
//!    payloads) of the memtable runs in scope, with their promoted attributes (cached at apply time).
//! 2. Scan the files in scope with the pushed-down filter, plus every `needs_merge` row.
//! 3. Any run that has (or may have) more than one version is fetched whole — every version from every file
//!    in the time scope, whatever its project (a late update may not know its project) — and merged with
//!    the M1 merge rule.
//! 4. The caller evaluates the exact filter on the merged runs.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use datafusion::logical_expr::{Expr as DfExpr, col, lit};
use datafusion::prelude::{ParquetReadOptions, SessionContext};
use tracedb_ingest::{Ingest, Version};
use tracedb_model::attrs::{Attrs, promote};
use tracedb_model::{RunId, RunState, TraceId, merge_versions, present};
use tracedb_storage::segment::{SegmentMeta, decode_core, decode_payload, start_key};
use tracedb_storage::{Payload, StoredRow};

use crate::QueryError;
use crate::pushdown::id_lit;

/// Ids per `IN (…)` list; longer lists are split into several scans.
const IN_CHUNK: usize = 2_000;

/// Which runs a scan covers.
#[derive(Clone, Debug, Default)]
pub struct Scope {
    pub project: Option<String>,
    /// On `start_key` (the run's own start time), microseconds, inclusive.
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub trace_id: Option<TraceId>,
    pub run_id: Option<RunId>,
    /// Only these traces (trace-level conditions, live views).
    pub traces: Option<Arc<BTreeSet<TraceId>>>,
}

impl Scope {
    fn matches_key(&self, key: i64) -> bool {
        self.from.is_none_or(|f| key >= f) && self.to.is_none_or(|t| key <= t)
    }

    fn time_ok(&self, s: &SegmentMeta) -> bool {
        self.from.is_none_or(|f| s.max_start_key >= f) && self.to.is_none_or(|t| s.min_start_key <= t)
    }

    fn traces_ok(&self, s: &SegmentMeta) -> bool {
        let one = self.trace_id.is_none_or(|t| s.min_trace_id <= t && t <= s.max_trace_id);
        let set = self
            .traces
            .as_ref()
            .is_none_or(|ts| ts.range(s.min_trace_id..=s.max_trace_id).next().is_some());
        one && set
    }

    fn file_in_scope(&self, s: &SegmentMeta) -> bool {
        // Versions with an unknown project live under project "" and may belong to any project.
        let project_ok = match &self.project {
            Some(p) => s.project == *p || s.project.is_empty(),
            None => true,
        };
        project_ok && self.time_ok(s) && self.traces_ok(s)
    }

    /// Files that may hold other versions of runs in scope: same time and trace bounds, any project.
    fn file_may_hold_versions(&self, s: &SegmentMeta) -> bool {
        self.time_ok(s) && self.traces_ok(s)
    }

    fn row_in_scope(&self, st: &RunState) -> bool {
        self.matches_key(start_key(&st.run.dotted_order))
            && self.trace_id.is_none_or(|t| st.run.trace_id == t)
            && self.run_id.is_none_or(|r| st.run.run_id == r)
            && self.traces.as_ref().is_none_or(|ts| ts.contains(&st.run.trace_id))
    }

    fn df_filter(&self) -> Option<DfExpr> {
        let mut parts = Vec::new();
        if let Some(f) = self.from {
            parts.push(col("start_key").gt_eq(lit(f)));
        }
        if let Some(t) = self.to {
            parts.push(col("start_key").lt_eq(lit(t)));
        }
        if let Some(t) = self.trace_id {
            parts.push(col("trace_id").eq(id_lit(t)));
        }
        if let Some(r) = self.run_id {
            parts.push(col("run_id").eq(id_lit(r)));
        }
        if let Some(ts) = &self.traces
            && ts.len() <= IN_CHUNK
        {
            parts.push(col("trace_id").in_list(ts.iter().map(|t| id_lit(*t)).collect(), false));
        }
        parts.into_iter().reduce(DfExpr::and)
    }
}

/// One merged run plus what the filter and views need.
#[derive(Clone, Debug)]
pub struct Merged {
    pub state: RunState,
    pub attrs: Attrs,
    /// More than one version was merged.
    pub multi: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ScanStats {
    pub files_total: usize,
    pub files_scanned: usize,
    pub rows_read: usize,
    pub rows_merged: usize,
}

struct Fresh {
    version: Arc<Version>,
    /// (state without payload, promoted attributes)
    rows: Vec<(RunState, Attrs)>,
}

fn fresh(ingest: &Ingest, scope: &Scope) -> Fresh {
    let mem = ingest.mem();
    let mem = mem.read().unwrap();
    let mut rows = Vec::new();
    let mut collect = |store: &tracedb_ingest::store::RunStore| {
        let take = |st: &RunState| {
            let attrs = store.attrs(&st.run.run_id).cloned().unwrap_or_else(|| promote(&st.run));
            (st.without_payload(), attrs)
        };
        if let Some(id) = scope.run_id {
            rows.extend(store.get(&id).filter(|st| scope.row_in_scope(st)).map(take));
        } else if let Some(t) = scope.trace_id {
            rows.extend(store.trace_run_ids(&t).iter().filter_map(|id| store.get(id)).map(take));
        } else {
            rows.extend(store.states().filter(|st| scope.row_in_scope(st)).map(take));
        }
    };
    collect(&mem.active);
    if let Some(f) = &mem.frozen {
        collect(f);
    }
    Fresh {
        version: mem.version.clone(),
        rows,
    }
}

/// Reads core rows from `files` matching `filter`.
async fn read_core(
    ctx: &SessionContext,
    ingest: &Ingest,
    files: &[&SegmentMeta],
    filter: Option<DfExpr>,
) -> Result<Vec<StoredRow>, QueryError> {
    if files.is_empty() {
        return Ok(vec![]);
    }
    let store = &ingest.storage().store;
    let urls: Vec<String> = files.iter().map(|s| store.url(&s.core_path)).collect();
    let mut df = ctx.read_parquet(urls, ParquetReadOptions::default()).await?;
    if let Some(f) = filter {
        df = df.filter(f)?;
    }
    let mut out = Vec::new();
    for b in df.collect().await? {
        out.extend(decode_core(&b)?);
    }
    Ok(out)
}

/// Every version, in `files`, of the given runs (chunked `run_id IN (…)` scans; blooms skip most files).
async fn versions_of(
    ctx: &SessionContext,
    ingest: &Ingest,
    files: &[&SegmentMeta],
    ids: &[RunId],
    scope: &Scope,
) -> Result<Vec<StoredRow>, QueryError> {
    let mut out = Vec::new();
    for chunk in ids.chunks(IN_CHUNK) {
        let by_id = col("run_id").in_list(chunk.iter().map(|id| id_lit(*id)).collect(), false);
        let filter = match scope.df_filter() {
            Some(s) => s.and(by_id),
            None => by_id,
        };
        out.extend(read_core(ctx, ingest, files, Some(filter)).await?);
    }
    Ok(out)
}

/// Merged runs in `scope` whose file rows pass `pushed` (rows that may have other versions always pass).
pub async fn collect(
    ctx: &SessionContext,
    ingest: &Ingest,
    scope: &Scope,
    pushed: Option<DfExpr>,
    stats: &mut ScanStats,
) -> Result<Vec<Merged>, QueryError> {
    let fresh = fresh(ingest, scope);
    let all: Vec<&SegmentMeta> = fresh.version.segments.iter().map(|s| &**s).collect();
    let files: Vec<&SegmentMeta> = all.iter().copied().filter(|s| scope.file_in_scope(s)).collect();
    stats.files_total += all.len();
    stats.files_scanned += files.len();

    let row_filter = pushed.map(|p| p.or(col("needs_merge").eq(lit(true))));
    let filter = match (scope.df_filter(), row_filter) {
        (Some(a), Some(b)) => Some(a.and(b)),
        (a, b) => a.or(b),
    };
    let rows = read_core(ctx, ingest, &files, filter).await?;
    stats.rows_read += rows.len();

    // Runs that may have several versions: marked in a file, created by an update, or in the multi-version set.
    let mvset = ingest.multi_version();
    let multi_ids: HashSet<RunId> = {
        let mv = mvset.read().unwrap();
        rows.iter()
            .map(|r| (&r.state, r.needs_merge))
            .chain(fresh.rows.iter().map(|(s, _)| (s, false)))
            .filter(|(st, needs)| *needs || st.created_by_patch() || mv.contains(&st.run.run_id))
            .map(|(st, _)| st.run.run_id)
            .collect()
    };
    // Fetch every version of those runs, from every file that may hold one (any project).
    let mut extra = Vec::new();
    if !multi_ids.is_empty() {
        let vfiles: Vec<&SegmentMeta> = all
            .iter()
            .copied()
            .filter(|s| scope.file_may_hold_versions(s))
            .collect();
        let ids: Vec<RunId> = multi_ids.iter().copied().collect();
        extra = versions_of(ctx, ingest, &vfiles, &ids, scope).await?;
        stats.rows_read += extra.len();
    }

    let mut groups: HashMap<RunId, Vec<(RunState, Attrs)>> = HashMap::new();
    let mut seen: HashSet<(RunId, u64)> = HashSet::new();
    for (st, attrs) in rows
        .into_iter()
        .chain(extra)
        .map(|r| (r.state, r.attrs))
        .chain(fresh.rows)
    {
        if seen.insert((st.run.run_id, st.first_seq)) {
            groups.entry(st.run.run_id).or_default().push((st, attrs));
        }
    }
    // A run seen more than once is merged even if nothing marked it (e.g. a second start after a flush).
    Ok(groups
        .into_values()
        .map(|versions| {
            let multi = versions.len() > 1;
            if multi {
                stats.rows_merged += versions.len();
            }
            merge_group(versions, multi)
        })
        .collect())
}

/// Merges versions and their attributes: for each attribute root, the newest version that set it wins.
fn merge_group(mut versions: Vec<(RunState, Attrs)>, multi: bool) -> Merged {
    if versions.len() == 1 {
        let (state, attrs) = versions.pop().expect("one version");
        return Merged { state, attrs, multi };
    }
    versions.sort_by_key(|(st, _)| st.first_seq);
    let mut attrs = Attrs::new();
    for (root, bit) in [
        ("metadata.", present::METADATA),
        ("inputs.", present::INPUTS),
        ("outputs.", present::OUTPUTS),
    ] {
        if let Some((_, a)) = versions.iter().rev().find(|(st, _)| st.present & bit != 0) {
            attrs.extend(
                a.iter()
                    .filter(|(k, _)| k.starts_with(root))
                    .map(|(k, v)| (k.clone(), v.clone())),
            );
        }
    }
    let state = merge_versions(versions.into_iter().map(|(st, _)| st).collect()).expect("at least one version");
    Merged { state, attrs, multi }
}

/// The payload of one run: from the active/frozen memtable if it's there, else from payload files. For each
/// field the newest version that set it wins (same rule as the merge).
pub async fn payload(ctx: &SessionContext, ingest: &Ingest, run: RunId, scope: &Scope) -> Result<Payload, QueryError> {
    let (version, mem_versions) = {
        let mem = ingest.mem();
        let mem = mem.read().unwrap();
        let mut v: Vec<RunState> = Vec::new();
        if let Some(f) = &mem.frozen {
            v.extend(f.get(&run).cloned());
        }
        v.extend(mem.active.get(&run).cloned());
        (mem.version.clone(), v)
    };
    let files: Vec<&SegmentMeta> = version
        .segments
        .iter()
        .map(|s| &**s)
        .filter(|s| scope.file_in_scope(s))
        .collect();
    // (first_seq, present bits, payload)
    let mut versions: Vec<(u64, u32, Payload)> = mem_versions
        .into_iter()
        .map(|st| {
            (
                st.first_seq,
                st.present,
                Payload {
                    inputs: st.run.inputs,
                    outputs: st.run.outputs,
                    events: st.run.events,
                },
            )
        })
        .collect();
    // A file holds at most one version of a run (one memtable each), so core and payload rows pair up per
    // file. Bloom filters on run_id let most files answer "not here" from metadata alone.
    let reads = files.iter().map(|seg| async move {
        let core = read_core(ctx, ingest, &[*seg], Some(col("run_id").eq(id_lit(run)))).await?;
        let Some(row) = core.into_iter().next() else {
            return Ok::<_, QueryError>(None);
        };
        let store = &ingest.storage().store;
        let df = ctx
            .read_parquet(store.url(&seg.payload_path), ParquetReadOptions::default())
            .await?
            .filter(col("run_id").eq(id_lit(run)))?;
        let mut p = Payload::default();
        for b in df.collect().await? {
            if let Some((_, got)) = decode_payload(&b)?.into_iter().next() {
                p = got;
            }
        }
        Ok(Some((row.state.first_seq, row.state.present, p)))
    });
    for r in futures_util::future::join_all(reads).await {
        if let Some(v) = r? {
            versions.push(v);
        }
    }
    versions.sort_by_key(|v| v.0);
    let mut out = Payload::default();
    for (_, bits, p) in versions {
        if bits & present::INPUTS != 0 {
            out.inputs = p.inputs;
        }
        if bits & present::OUTPUTS != 0 {
            out.outputs = p.outputs;
        }
        if bits & present::EVENTS != 0 {
            out.events = p.events;
        }
    }
    Ok(out)
}
