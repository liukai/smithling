//! Text search (design §4.2, research M3-R3): the index gives a superset, the merged run decides.
//!
//! 1. Each file in scope with a text index gives the rows holding every word on the side; a word in more
//!    than half a file's rows is skipped there. A file without an index (written by M2), with an unreadable
//!    one, or whose words are all that common gives all its rows. Rows become run ids through `payload_row`.
//! 2. Memtable versions are checked directly against their own text.
//! 3. Candidates are read a chunk at a time in page order: every version of each, with its payload, is
//!    merged, and `eval` checks the phrase on the merged text, so a replaced output stops matching.

use std::collections::{HashMap, HashSet};

use datafusion::arrow::array::{Array, BooleanArray, FixedSizeBinaryArray, Int64Array};
use datafusion::logical_expr::{Expr as DfExpr, col, lit};
use datafusion::prelude::ParquetReadOptions;
use futures_util::{StreamExt, TryStreamExt};
use tracedb_model::attrs::{Attrs, promote};
use tracedb_model::filter::{Expr, Side, contains_search};
use tracedb_model::text::{Text, phrase_in};
use tracedb_model::{RunId, RunState, TraceId, Ts, present};
use tracedb_storage::StorageError;
use tracedb_storage::fts::TextField;
use tracedb_storage::segment::{SegmentMeta, StoredRow, decode_text};

use crate::pushdown::id_lit;
use crate::scan::{self, IN_CHUNK, Merged, Pinned, ScanStats, Scope, damaged_alone, merge_group};
use crate::{Query, QueryError};

/// Files looked up at once.
const FILES: usize = 16;
/// Smaller files keep their common words: reading all their rows costs little more than the lists.
const COMMON_FROM_ROWS: u64 = 16;

/// The search the index can answer: one at the top of a slot or inside a top-level `and`.
pub(crate) fn indexed(e: &Expr) -> Option<(&[String], Side)> {
    match e {
        Expr::Search { words, side, .. } => Some((words, *side)),
        Expr::And(xs) => xs.iter().find_map(indexed),
        _ => None,
    }
}

/// Every search in `e`.
pub(crate) fn all(e: &Expr) -> Vec<(&[String], Side)> {
    match e {
        Expr::Search { words, side, .. } => vec![(&words[..], *side)],
        Expr::And(xs) | Expr::Or(xs) => xs.iter().flat_map(all).collect(),
        Expr::Not(x) => all(x),
        Expr::Cmp { .. } | Expr::Has { .. } | Expr::In { .. } | Expr::Exists { .. } => vec![],
    }
}

/// The first search not under a `not`: the one a page row's excerpt shows.
pub(crate) fn shown(e: &Expr) -> Option<(&[String], Side)> {
    match e {
        Expr::Search { words, side, .. } => Some((words, *side)),
        Expr::And(xs) | Expr::Or(xs) => xs.iter().find_map(shown),
        _ => None,
    }
}

/// The payload fields a filter's searches read (`error` is in core rows).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Need {
    pub inputs: bool,
    pub outputs: bool,
}

impl Need {
    pub(crate) fn of(e: &Expr) -> Need {
        all(e).into_iter().fold(Need::default(), |n, (_, side)| Need {
            inputs: n.inputs || matches!(side, Side::Any | Side::Inputs),
            outputs: n.outputs || matches!(side, Side::Any | Side::Outputs),
        })
    }

    fn any(self) -> bool {
        self.inputs || self.outputs
    }

    fn columns(self) -> Vec<&'static str> {
        let mut c = vec!["run_id"];
        c.extend(self.inputs.then_some("inputs"));
        c.extend(self.outputs.then_some("outputs"));
        c
    }
}

/// The conditions of `e` that don't search, as one expression: checked before any text is read.
pub(crate) fn before_text(e: &Expr) -> Option<Expr> {
    match e {
        Expr::And(xs) => {
            let mut rest: Vec<Expr> = xs.iter().filter(|x| !contains_search(x)).cloned().collect();
            match rest.len() {
                0 => None,
                1 => rest.pop(),
                _ => Some(Expr::And(rest)),
            }
        }
        x if contains_search(x) => None,
        x => Some(x.clone()),
    }
}

fn fields(side: Side) -> &'static [TextField] {
    match side {
        Side::Any => &[TextField::Error, TextField::Outputs, TextField::Inputs],
        Side::Inputs => &[TextField::Inputs],
        Side::Outputs => &[TextField::Outputs],
        Side::Error => &[TextField::Error],
    }
}

/// A run that may match: a version's start key, the run, its trace, and the file that version is in
/// (index in the pinned `Version`; `None`: the memtable).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cand {
    pub key: i64,
    pub run_id: RunId,
    pub trace_id: TraceId,
    pub file: Option<usize>,
}

/// One file's candidates.
#[derive(Default)]
struct FileHits {
    keys: Vec<(i64, RunId, TraceId)>,
    indexed: bool,
    no_index: bool,
    cache_hits: usize,
}

/// Runs in `scope` that may hold `words` on `side` and pass `pre` (the filter's other conditions), unsorted;
/// a run may appear more than once.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn candidates(
    q: &Query,
    pinned: &Pinned,
    scope: &Scope,
    words: &[String],
    side: Side,
    pre: Option<&Expr>,
    now: Ts,
    stats: &mut ScanStats,
) -> Result<Vec<Cand>, QueryError> {
    let pre = pre.and_then(|p| crate::pushdown::compile(p, now)).map(|p| p.expr);
    let segs = &pinned.version.segments;
    let looks: Vec<_> = (0..segs.len())
        .filter(|&i| scope.file_in_scope(&segs[i]) && !q.ingest.is_damaged(&segs[i].id))
        .map(|i| file_hits(q, i, &segs[i], scope, words, side, pre.as_ref()))
        .collect();
    stats.files_total = stats.files_total.max(segs.len());
    let per_file: Vec<(usize, FileHits)> = futures_util::stream::iter(looks)
        .buffer_unordered(FILES)
        .try_collect()
        .await?;
    let mut out = Vec::new();
    for (i, h) in per_file {
        let seg = &segs[i];
        if h.indexed {
            stats.index_files += 1;
        } else {
            stats.scan_files += 1;
        }
        if h.no_index {
            stats.not_indexed.insert(seg.id.clone());
        }
        stats.cache_hits += h.cache_hits.min(1);
        out.extend(h.keys.into_iter().map(|(key, run_id, trace_id)| Cand {
            key,
            run_id,
            trace_id,
            file: Some(i),
        }));
    }
    // A pin without text can't check its memtable runs: all of them are candidates.
    out.extend(
        pinned
            .rows
            .iter()
            .filter(|(_, st, _)| !pinned.text || phrase_in(&Text::of(&st.run), words, side))
            .map(|(k, st, _)| Cand {
                key: *k,
                run_id: st.run.run_id,
                trace_id: st.run.trace_id,
                file: None,
            }),
    );
    Ok(out)
}

async fn file_hits(
    q: &Query,
    i: usize,
    seg: &SegmentMeta,
    scope: &Scope,
    words: &[String],
    side: Side,
    pre: Option<&DfExpr>,
) -> Result<(usize, FileHits), QueryError> {
    let mut h = FileHits {
        no_index: seg.fts_path.is_none(),
        ..FileHits::default()
    };
    let mut rows: Option<Vec<u32>> = None;
    if q.index && seg.fts_path.is_some() {
        let max_df = if seg.row_count < COMMON_FROM_ROWS {
            u32::MAX
        } else {
            (seg.row_count / 2) as u32
        };
        let mut union = Vec::new();
        let mut all = false;
        for f in fields(side) {
            match q.fts.lookup_within(seg, words, *f, max_df).await {
                Ok(l) => {
                    h.cache_hits += l.cached as usize;
                    match l.rows {
                        Some(r) => union.extend(r),
                        None => {
                            all = true;
                            break;
                        }
                    }
                }
                Err(e) if unindexed(&e) => {
                    tracing::warn!(segment = %seg.id, error = %e, "text index unreadable; reading the file instead");
                    h.no_index = true;
                    all = true;
                    break;
                }
                Err(e) => return Err(e.into()),
            }
        }
        if !all {
            union.sort_unstable();
            union.dedup();
            h.indexed = true;
            rows = Some(union);
        }
    }
    if rows.as_ref().is_none_or(|r| !r.is_empty()) {
        h.keys = row_keys(q, seg, rows.as_deref(), scope, pre).await?;
    }
    Ok((i, h))
}

fn unindexed(e: &StorageError) -> bool {
    matches!(e, StorageError::NotIndexed(_) | StorageError::Format(_)) || tracedb_storage::segment::damage(e).is_some()
}

/// (start key, run id, trace id) of the core rows at `rows` (payload row numbers; `None`: every row) in
/// `scope`, without the single-version rows `pre` (pushed down, a superset) rules out.
async fn row_keys(
    q: &Query,
    seg: &SegmentMeta,
    rows: Option<&[u32]>,
    scope: &Scope,
    pre: Option<&DfExpr>,
) -> Result<Vec<(i64, RunId, TraceId)>, QueryError> {
    let by_row = rows
        .filter(|r| r.len() <= IN_CHUNK)
        .map(|r| col("payload_row").in_list(r.iter().map(|n| lit(*n as i64)).collect(), false));
    let filter = [scope.df_filter(), by_row].into_iter().flatten().reduce(DfExpr::and);
    let read = async {
        let url = q.ingest.storage().store.url(&seg.core_path);
        let mut df = q.ctx.read_parquet(url, ParquetReadOptions::default()).await?;
        if let Some(f) = filter {
            df = df.filter(f)?;
        }
        let pass = pre.map_or(lit(true), |p| p.clone().is_not_false());
        let cols = ["run_id", "start_key", "payload_row", "trace_id", "needs_merge"].map(col);
        Ok::<_, QueryError>(
            df.select(cols.into_iter().chain([pass.alias("pass")]))?
                .collect()
                .await?,
        )
    };
    let batches = match read.await {
        Ok(b) => b,
        Err(e) if damaged_alone(&q.ingest, seg, &e).await => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    let mv = q.ingest.multi_version();
    let mv = mv.read().unwrap();
    let mut out = Vec::new();
    for b in batches {
        let bad = |c: &str| StorageError::Format(format!("core column {c} has the wrong type"));
        let ids = b
            .column(0)
            .as_any()
            .downcast_ref::<FixedSizeBinaryArray>()
            .ok_or_else(|| bad("run_id"))?;
        let keys = b
            .column(1)
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| bad("start_key"))?;
        let at = b
            .column(2)
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| bad("payload_row"))?;
        let traces = b
            .column(3)
            .as_any()
            .downcast_ref::<FixedSizeBinaryArray>()
            .ok_or_else(|| bad("trace_id"))?;
        let bools = |n: usize, c: &str| {
            b.column(n)
                .as_any()
                .downcast_ref::<BooleanArray>()
                .ok_or_else(|| bad(c))
        };
        let (needs_merge, pass) = (bools(4, "needs_merge")?, bools(5, "pass")?);
        for i in 0..b.num_rows() {
            if ids.is_null(i) || rows.is_some_and(|r| r.binary_search(&(at.value(i) as u32)).is_err()) {
                continue;
            }
            let Ok(id) = RunId::from_slice(ids.value(i)) else {
                continue;
            };
            // Another version may change what `pre` reads.
            if !(pass.value(i) || needs_merge.value(i) || mv.contains(&id)) {
                continue;
            }
            if let Ok(t) = TraceId::from_slice(traces.value(i)) {
                out.push((keys.value(i), id, t));
            }
        }
    }
    Ok(out)
}

/// Every version of the runs `ids` in `scope` (any window), merged; the runs `keep` passes before their text
/// is read get the `need`ed text and are returned. With `held`, the files known to hold a version of each:
/// other files are read only for runs that may have more than one version (the M2 fast path).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn collect_text(
    q: &Query,
    pinned: &Pinned,
    scope: &Scope,
    ids: &HashSet<RunId>,
    held: Option<&HashSet<usize>>,
    need: Need,
    keep: &mut (dyn FnMut(&Merged) -> bool + Send),
    stats: &mut ScanStats,
) -> Result<Vec<Merged>, QueryError> {
    let payload = need.any();
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let vscope = Scope {
        window: None,
        ..scope.clone()
    };
    let segs = &pinned.version.segments;
    let files = |keep: &dyn Fn(usize) -> bool| -> Vec<&SegmentMeta> {
        (0..segs.len())
            .filter(|&i| keep(i) && vscope.file_may_hold_versions(&segs[i]))
            .map(|i| &*segs[i])
            .collect()
    };
    let mem: Vec<(RunState, Attrs)> = if pinned.text || !payload {
        pinned
            .rows
            .iter()
            .filter(|(_, st, _)| ids.contains(&st.run.run_id))
            .map(|(_, st, a)| (st.clone(), a.clone()))
            .collect()
    } else {
        mem_versions(q, ids)
    };
    let mut rows = match held {
        None => scan::versions_by_file(&q.ctx, &q.ingest, &files(&|_| true), ids, &vscope).await?,
        Some(held) => {
            let mut rows =
                scan::versions_by_file(&q.ctx, &q.ingest, &files(&|i| held.contains(&i)), ids, &vscope).await?;
            let multi: HashSet<RunId> = {
                let mv = q.ingest.multi_version();
                let mv = mv.read().unwrap();
                rows.iter()
                    .map(|(_, r)| (&r.state, r.needs_merge))
                    .chain(mem.iter().map(|(st, _)| (st, false)))
                    .filter(|(st, needs)| *needs || st.created_by_patch() || mv.contains(&st.run.run_id))
                    .map(|(st, _)| st.run.run_id)
                    .collect()
            };
            let others = files(&|i| !held.contains(&i));
            rows.extend(scan::versions_by_file(&q.ctx, &q.ingest, &others, &multi, &vscope).await?);
            rows
        }
    };
    stats.rows_read += rows.len();
    let mut seen: HashSet<(RunId, u64)> = HashSet::new();
    rows.retain(|(_, r)| seen.insert((r.state.run.run_id, r.state.first_seq)));
    let mem: Vec<(RunState, Attrs)> = mem
        .into_iter()
        .filter(|(st, _)| seen.insert((st.run.run_id, st.first_seq)))
        .collect();
    let merge_all = |rows: Vec<(RunState, Attrs)>, stats: &mut ScanStats| -> Vec<Merged> {
        let mut groups: HashMap<RunId, Vec<(RunState, Attrs)>> = HashMap::new();
        for (st, attrs) in rows {
            groups.entry(st.run.run_id).or_default().push((st, attrs));
        }
        groups
            .into_values()
            .map(|versions| {
                let multi = versions.len() > 1;
                if multi {
                    stats.rows_merged += versions.len();
                }
                merge_group(versions, multi)
            })
            .collect()
    };
    if !payload {
        let all = rows.into_iter().map(|(_, r)| (r.state, r.attrs)).chain(mem).collect();
        return Ok(merge_all(all, stats).into_iter().filter(|m| keep(m)).collect());
    }
    // The cheaper conditions first, on the runs merged without their text.
    let before: Vec<(RunState, Attrs)> = rows
        .iter()
        .map(|(_, r)| (r.state.clone(), r.attrs.clone()))
        .chain(mem.iter().cloned())
        .collect();
    let kept: HashSet<RunId> = merge_all(before, &mut ScanStats::default())
        .into_iter()
        .filter(|m| keep(m))
        .map(|m| m.state.run.run_id)
        .collect();
    rows.retain(|(_, r)| kept.contains(&r.state.run.run_id));
    attach_payload(q, &mut rows, need, stats).await?;
    let all = rows
        .into_iter()
        .map(|(_, r)| (r.state, r.attrs))
        .chain(mem.into_iter().filter(|(st, _)| kept.contains(&st.run.run_id)))
        .collect();
    Ok(merge_all(all, stats))
}

/// The memtable versions of `ids` with their text, read now (for a pin taken without it).
fn mem_versions(q: &Query, ids: &HashSet<RunId>) -> Vec<(RunState, Attrs)> {
    let mem = q.ingest.mem();
    let mem = mem.read().unwrap();
    let mut out = Vec::new();
    for store in std::iter::once(&mem.active).chain(mem.frozen.as_deref()) {
        for id in ids {
            if let Some(st) = store.get(id) {
                let attrs = store.attrs(id).cloned().unwrap_or_else(|| promote(&st.run));
                let mut copy = st.without_payload();
                copy.run.inputs = st.run.inputs.clone();
                copy.run.outputs = st.run.outputs.clone();
                out.push((copy, attrs));
            }
        }
    }
    out
}

/// Reads the `need`ed text of the rows that set it, file by file (a file holds one version of a run).
async fn attach_payload(
    q: &Query,
    rows: &mut [(&SegmentMeta, StoredRow)],
    need: Need,
    stats: &mut ScanStats,
) -> Result<(), QueryError> {
    let bits = if need.inputs { present::INPUTS } else { 0 } | if need.outputs { present::OUTPUTS } else { 0 };
    let mut by_file: HashMap<&str, (&SegmentMeta, Vec<RunId>)> = HashMap::new();
    for (seg, r) in rows.iter() {
        if r.state.present & bits != 0 {
            by_file
                .entry(&seg.id)
                .or_insert_with(|| (*seg, Vec::new()))
                .1
                .push(r.state.run.run_id);
        }
    }
    let reads: Vec<_> = by_file
        .into_values()
        .map(|(seg, ids)| file_text(q, seg, ids, need))
        .collect();
    let texts: Vec<HashMap<(String, RunId), InOut>> = futures_util::stream::iter(reads)
        .buffer_unordered(FILES)
        .try_collect()
        .await?;
    let mut texts: HashMap<(String, RunId), InOut> = texts.into_iter().flatten().collect();
    stats.payload_rows += texts.len();
    for (seg, r) in rows.iter_mut() {
        if let Some((inputs, outputs)) = texts.remove(&(seg.id.clone(), r.state.run.run_id)) {
            let st = &mut r.state;
            if need.inputs && st.present & present::INPUTS != 0 {
                st.run.inputs = inputs;
            }
            if need.outputs && st.present & present::OUTPUTS != 0 {
                st.run.outputs = outputs;
            }
        }
    }
    Ok(())
}

type InOut = (Option<serde_json::Value>, Option<serde_json::Value>);

async fn file_text(
    q: &Query,
    seg: &SegmentMeta,
    ids: Vec<RunId>,
    need: Need,
) -> Result<HashMap<(String, RunId), InOut>, QueryError> {
    let mut out = HashMap::new();
    for chunk in ids.chunks(IN_CHUNK) {
        let read = async {
            let url = q.ingest.storage().store.url(&seg.payload_path);
            let df = q
                .ctx
                .read_parquet(url, ParquetReadOptions::default())
                .await?
                .filter(col("run_id").in_list(chunk.iter().map(|id| id_lit(*id)).collect(), false))?
                .select_columns(&need.columns())?;
            let mut got = Vec::new();
            for b in df.collect().await? {
                got.extend(decode_text(&b)?);
            }
            Ok::<_, QueryError>(got)
        };
        match read.await {
            Ok(got) => out.extend(
                got.into_iter()
                    .map(|(id, p)| ((seg.id.clone(), id), (p.inputs, p.outputs))),
            ),
            // The file is now skipped (and reported); its versions are merged without their text.
            Err(e) if damaged_alone(&q.ingest, seg, &e).await => break,
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}
