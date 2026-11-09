//! Collecting merged runs for a scope: memtables ∪ files, with the single-version fast path (design §4.3).
//!
//! 1. Pin: under the memtable lock take the file set (`Version`), the frozen memtable, and copies (without
//!    payloads, except the text a search needs) of the memtable runs in scope, with their promoted attributes
//!    (cached at apply time). One pin can serve several narrower scopes (start-time windows).
//! 2. Scan the files in scope with the pushed-down filter, plus every `needs_merge` row.
//! 3. Any run that has (or may have) more than one version is fetched whole — every version from every file
//!    in the time scope, whatever its project (a late update may not know its project) — and merged with
//!    the M1 merge rule.
//! 4. The caller evaluates the exact filter on the merged runs.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use datafusion::logical_expr::{Expr as DfExpr, col, lit};
use datafusion::prelude::{ParquetReadOptions, SessionContext};
use smithling_ingest::store::RunStore;
use smithling_ingest::{Ingest, Version};
use smithling_model::attrs::{Attrs, promote};
use smithling_model::filter::Side;
use smithling_model::text::{Text, phrase_in};
use smithling_model::{RunId, RunState, TraceId, merge_versions, present};
use smithling_storage::segment::{SegmentMeta, decode_core, start_key};
use smithling_storage::{Payload, StoredRow};

use crate::QueryError;
use crate::pushdown::id_lit;

/// Ids per `IN (…)` list; longer lists are split into several scans.
pub const IN_CHUNK: usize = 2_000;

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
    /// Only runs whose merged start key is in this range, with their versions read from the whole scope (a
    /// conflicting later `dotted_order` can put a version in another window).
    pub window: Option<(i64, i64)>,
    /// Only these runs (a search's matches, for aggregates).
    pub runs: Option<Arc<HashSet<RunId>>>,
}

impl Scope {
    pub(crate) fn narrowed(&self) -> Scope {
        let Some((a, b)) = self.window else {
            return self.clone();
        };
        Scope {
            from: Some(self.from.map_or(a, |f| f.max(a))),
            to: Some(self.to.map_or(b, |t| t.min(b))),
            window: None,
            ..self.clone()
        }
    }

    pub(crate) fn matches_key(&self, key: i64) -> bool {
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

    pub fn file_in_scope(&self, s: &SegmentMeta) -> bool {
        // Versions with an unknown project live under project "" and may belong to any project.
        let project_ok = match &self.project {
            Some(p) => s.project == *p || s.project.is_empty(),
            None => true,
        };
        project_ok && self.time_ok(s) && self.traces_ok(s)
    }

    /// Files that may hold other versions of runs in scope: same time and trace bounds, any project.
    pub(crate) fn file_may_hold_versions(&self, s: &SegmentMeta) -> bool {
        self.time_ok(s) && self.traces_ok(s)
    }

    fn row_in_scope(&self, key: i64, st: &RunState) -> bool {
        self.matches_key(key)
            && self.trace_id.is_none_or(|t| st.run.trace_id == t)
            && self.run_id.is_none_or(|r| st.run.run_id == r)
            && self.traces.as_ref().is_none_or(|ts| ts.contains(&st.run.trace_id))
            && self.has_run(st.run.run_id)
    }

    pub(crate) fn has_run(&self, id: RunId) -> bool {
        self.runs.as_ref().is_none_or(|r| r.contains(&id))
    }

    pub(crate) fn df_filter(&self) -> Option<DfExpr> {
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
        if let Some(rs) = &self.runs
            && rs.len() <= IN_CHUNK
        {
            parts.push(col("run_id").in_list(rs.iter().map(|r| id_lit(*r)).collect(), false));
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
    /// Taken whole from the kept merged rows: `state` lacks `metadata`, `extra` and `outputs_usage` (see
    /// [`Merged::shown`]).
    pub(crate) kept: Option<Arc<crate::fields::Kept>>,
}

impl Merged {
    /// The state as a result row shows it.
    pub fn shown(&self) -> std::borrow::Cow<'_, RunState> {
        match &self.kept {
            Some(k) => std::borrow::Cow::Owned(k.full().0),
            None => std::borrow::Cow::Borrowed(&self.state),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ScanStats {
    pub files_total: usize,
    pub files_scanned: usize,
    pub rows_read: usize,
    pub rows_merged: usize,
    /// Files searched for other versions of multi-version runs.
    pub files_version_lookup: usize,
    /// Rows read by that search (included in `rows_read`).
    pub merge_rows: usize,
    /// Files a text search narrowed with their index.
    pub index_files: usize,
    /// Files a text search read whole: no index (M2 files), an unreadable one, or only very common words.
    pub scan_files: usize,
    /// Runs a text search checked against their merged text.
    pub candidates: usize,
    /// Payload rows read for that check.
    pub payload_rows: usize,
    /// Files whose index footer was already in memory.
    pub cache_hits: usize,
    /// Files a single run's payload was read from.
    pub payload_files: usize,
    /// Files in scope without a text index.
    pub not_indexed: BTreeSet<String>,
    scanned: HashSet<String>,
    looked_up: HashSet<String>,
}

/// The file set and the memtable runs in a scope, taken together under the memtable lock.
pub struct Pinned {
    pub version: Arc<Version>,
    /// (start key, state, promoted attributes), by start key. Without payloads, except as [`pin_text`] says.
    pub(crate) rows: Vec<(i64, RunState, Attrs)>,
    /// Taken by [`pin_text`].
    pub(crate) text: bool,
    /// The merged file rows of every multi-version run, for exactly `version`'s files (D-023): read instead of
    /// fetching their versions again.
    pub(crate) kept: Option<Arc<crate::fields::Rows>>,
}

impl Pinned {
    /// Start keys of the pinned memtable runs, ascending.
    pub fn keys(&self) -> Vec<i64> {
        self.rows.iter().map(|r| r.0).collect()
    }

    fn rows_in(&self, scope: &Scope) -> impl Iterator<Item = (RunState, Attrs)> {
        let start = scope.from.map_or(0, |f| self.rows.partition_point(|r| r.0 < f));
        let end = scope
            .to
            .map_or(self.rows.len(), |t| self.rows.partition_point(|r| r.0 <= t));
        self.rows[start..end.max(start)]
            .iter()
            .filter(|(k, st, _)| scope.row_in_scope(*k, st))
            .map(|(_, st, a)| (st.clone(), a.clone()))
    }
}

pub fn pin(ingest: &Ingest, scope: &Scope) -> Pinned {
    pin_with(ingest, scope, false).0
}

/// [`pin`], with `inputs` and `outputs` copied for the memtable versions whose own text matches one of
/// `searches`. Leaving out the others' text changes no search's answer: a merged field comes whole from one
/// version, and theirs match none of the phrases.
pub(crate) fn pin_text(ingest: &Ingest, scope: &Scope, searches: &[(&[String], Side)]) -> Pinned {
    pin_inner(ingest, scope, false, searches).0
}

/// [`pin_text`], plus the frozen memtable and the multi-version set taken with it (to bring the kept merged
/// rows up to this pin).
pub(crate) fn pin_for_kept(
    ingest: &Ingest,
    scope: &Scope,
    searches: &[(&[String], Side)],
) -> (Pinned, Option<Arc<RunStore>>, HashSet<RunId>) {
    let (pinned, frozen, _) = pin_inner(ingest, scope, false, searches);
    // The set only shrinks once no query holds a file set from before a compaction's swap (M4), so a copy
    // taken now holds every run the pin marks.
    let mv = ingest.multi_version().read().unwrap().clone();
    (pinned, frozen, mv)
}

/// [`pin`], plus (if `multi`) the multi-version set and every memtable version of its runs whatever the scope,
/// and the frozen memtable, all under the memtable lock.
pub(crate) fn pin_with(ingest: &Ingest, scope: &Scope, multi: bool) -> (Pinned, Option<Arc<RunStore>>, HashSet<RunId>) {
    pin_inner(ingest, scope, multi, &[])
}

fn pin_inner(
    ingest: &Ingest,
    scope: &Scope,
    multi: bool,
    searches: &[(&[String], Side)],
) -> (Pinned, Option<Arc<RunStore>>, HashSet<RunId>) {
    let mem = ingest.mem();
    let mem = mem.read().unwrap();
    let mv: HashSet<RunId> = if multi {
        ingest.multi_version().read().unwrap().clone()
    } else {
        HashSet::new()
    };
    let projects = ingest.projects();
    // Memtable runs are kept or left out by project exactly as their files will be (`file_in_scope`).
    let projects = scope.project.as_ref().map(|p| (p, projects.read().unwrap()));
    let in_project = |st: &RunState| {
        projects.as_ref().is_none_or(|(p, map)| {
            let filed = map.filed(st);
            filed == p.as_str() || filed.is_empty()
        })
    };
    let mut rows = Vec::new();
    fn keyed(st: &RunState) -> (i64, &RunState) {
        (start_key(&st.run.dotted_order), st)
    }
    let mut collect = |store: &smithling_ingest::store::RunStore| {
        let in_scope = |(k, st): &(i64, &RunState)| scope.row_in_scope(*k, st) && in_project(st);
        let take = |(k, st): (i64, &RunState)| {
            let attrs = store.attrs(&st.run.run_id).cloned().unwrap_or_else(|| promote(&st.run));
            let mut copy = st.without_payload();
            if searches.iter().any(|(w, side)| phrase_in(&Text::of(&st.run), w, *side)) {
                copy.run.inputs = st.run.inputs.clone();
                copy.run.outputs = st.run.outputs.clone();
            }
            (k, copy, attrs)
        };
        if multi {
            let keep = |(k, st): &(i64, &RunState)| mv.contains(&st.run.run_id) || in_scope(&(*k, *st));
            rows.extend(store.states().map(keyed).filter(keep).map(take));
        } else if let Some(id) = scope.run_id {
            rows.extend(store.get(&id).map(keyed).filter(in_scope).map(take));
        } else if let Some(t) = scope.trace_id {
            rows.extend(
                store
                    .trace_run_ids(&t)
                    .iter()
                    .filter_map(|id| store.get(id))
                    .filter(|st| in_project(st))
                    .map(keyed)
                    .map(take),
            );
        } else if let Some(ts) = scope.traces.as_ref().filter(|ts| ts.len() < store.len()) {
            let ids = ts.iter().flat_map(|t| store.trace_run_ids(t));
            rows.extend(ids.filter_map(|id| store.get(id)).map(keyed).filter(in_scope).map(take));
        } else {
            rows.extend(store.states().map(keyed).filter(in_scope).map(take));
        }
    };
    collect(&mem.active);
    if let Some(f) = &mem.frozen {
        collect(f);
    }
    rows.sort_by_key(|r| r.0);
    let pinned = Pinned {
        version: mem.version.clone(),
        rows,
        text: !searches.is_empty(),
        kept: None,
    };
    (pinned, mem.frozen.clone(), mv)
}

/// Whether a read error says a file is missing or unreadable (not a network hiccup or a bug), and which.
fn damage_of(e: &QueryError) -> Option<&'static str> {
    use datafusion::arrow::error::ArrowError;
    use datafusion::error::DataFusionError as D;
    use smithling_storage::segment::{boxed_damage, damage, parquet_damage, store_damage};
    fn external(e: &(dyn std::error::Error + Send + Sync + 'static)) -> Option<&'static str> {
        match e.downcast_ref::<D>() {
            Some(e) => engine(e),
            None => boxed_damage(e),
        }
    }
    fn engine(e: &D) -> Option<&'static str> {
        match e.find_root() {
            D::ObjectStore(e) => store_damage(e),
            D::ParquetError(e) => parquet_damage(e),
            D::ArrowError(e, _) => match &**e {
                ArrowError::ExternalError(inner) => external(&**inner),
                ArrowError::IoError(..) => None,
                _ => Some("unreadable"),
            },
            D::External(inner) => external(&**inner),
            _ => None,
        }
    }
    match e {
        QueryError::Storage(e) => damage(e),
        QueryError::Engine(e) => engine(e),
        QueryError::BadCursor => None,
    }
}

/// After `e` from reading only `seg`: marks it damaged if its footers or `e` say it is. True if marked.
pub(crate) async fn damaged_alone(ingest: &Ingest, seg: &SegmentMeta, e: &QueryError) -> bool {
    if !probe(ingest, &[seg]).await.is_empty() {
        return true;
    }
    let Some(reason) = damage_of(e) else {
        return false;
    };
    ingest.mark_damaged(seg, reason);
    true
}

/// After a failed read: checks each file's footers, marks the missing or unreadable ones damaged (so later
/// reads skip them) and returns their ids. Empty if every footer looks fine.
async fn probe(ingest: &Ingest, files: &[&SegmentMeta]) -> HashSet<String> {
    use futures_util::StreamExt;
    let checks: Vec<_> = files
        .iter()
        .map(|s| {
            let (store, seg) = (ingest.storage().store.clone(), (*s).clone());
            async move {
                let r = smithling_storage::segment::check(&store, &seg).await;
                (seg, r)
            }
        })
        .collect();
    let results: Vec<_> = futures_util::stream::iter(checks)
        .buffer_unordered(LOOKUP_FILES)
        .collect()
        .await;
    let mut bad = HashSet::new();
    for (s, r) in results {
        if let Some(reason) = r.err().as_ref().and_then(smithling_storage::segment::damage) {
            ingest.mark_damaged(&s, reason);
            bad.insert(s.id);
        }
    }
    bad
}

/// Reads core rows from `files` matching `filter`. Files known to be damaged are skipped; a read that fails
/// because of one marks it (found by its footers, else by reading each file alone) and is retried without
/// it (M3-FR-18).
pub(crate) async fn read_core(
    ctx: &SessionContext,
    ingest: &Ingest,
    files: &[&SegmentMeta],
    filter: Option<DfExpr>,
) -> Result<Vec<StoredRow>, QueryError> {
    let mut files: Vec<&SegmentMeta> = files.iter().copied().filter(|s| !ingest.is_damaged(&s.id)).collect();
    loop {
        if files.is_empty() {
            return Ok(vec![]);
        }
        match read_core_from(ctx, ingest, &files, filter.clone()).await {
            Ok(rows) => return Ok(rows),
            Err(e) => {
                let mut bad = probe(ingest, &files).await;
                // Damage past the footers: only a decoding error is worth reading each file alone for.
                if bad.is_empty() && damage_of(&e).is_some() {
                    bad = read_each(ctx, ingest, &files, &filter).await;
                }
                if bad.is_empty() {
                    return Err(e);
                }
                files.retain(|s| !bad.contains(&s.id));
            }
        }
    }
}

/// Reads each file alone; marks those whose read fails as damaged and returns their ids.
async fn read_each(
    ctx: &SessionContext,
    ingest: &Ingest,
    files: &[&SegmentMeta],
    filter: &Option<DfExpr>,
) -> HashSet<String> {
    use futures_util::StreamExt;
    async fn read_one<'a>(
        ctx: &SessionContext,
        ingest: &Ingest,
        seg: &'a SegmentMeta,
        filter: Option<DfExpr>,
    ) -> (&'a SegmentMeta, Option<QueryError>) {
        (seg, read_core_from(ctx, ingest, &[seg], filter).await.err())
    }
    let reads: Vec<_> = files
        .iter()
        .map(|seg| read_one(ctx, ingest, seg, filter.clone()))
        .collect();
    let failed: Vec<_> = futures_util::stream::iter(reads)
        .buffer_unordered(LOOKUP_FILES)
        .collect()
        .await;
    let mut bad = HashSet::new();
    for (seg, e) in failed {
        if let Some(e) = e
            && damaged_alone(ingest, seg, &e).await
        {
            bad.insert(seg.id.clone());
        }
    }
    bad
}

async fn read_core_from(
    ctx: &SessionContext,
    ingest: &Ingest,
    files: &[&SegmentMeta],
    filter: Option<DfExpr>,
) -> Result<Vec<StoredRow>, QueryError> {
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

/// Files read at once by [`versions_of`].
const LOOKUP_FILES: usize = 16;

/// Every version, in `files`, of the runs in `ids`. A short id list is one `run_id IN (…)` scan over every
/// file (bloom filters skip most); a long one reads each file's run ids first, then the rows of the runs it holds.
pub(crate) async fn versions_of(
    ctx: &SessionContext,
    ingest: &Ingest,
    files: &[&SegmentMeta],
    ids: &HashSet<RunId>,
    scope: &Scope,
) -> Result<Vec<StoredRow>, QueryError> {
    if ids.is_empty() || files.is_empty() {
        return Ok(vec![]);
    }
    if ids.len() <= IN_CHUNK {
        let list: Vec<RunId> = ids.iter().copied().collect();
        return read_core(ctx, ingest, files, Some(by_ids(&list, scope))).await;
    }
    Ok(versions_by_file(ctx, ingest, files, ids, scope)
        .await?
        .into_iter()
        .map(|(_, r)| r)
        .collect())
}

fn by_ids(list: &[RunId], scope: &Scope) -> DfExpr {
    let by_id = col("run_id").in_list(list.iter().map(|id| id_lit(*id)).collect(), false);
    match scope.df_filter() {
        Some(s) => s.and(by_id),
        None => by_id,
    }
}

/// [`versions_of`], with each row's file: every file is read on its own.
pub(crate) async fn versions_by_file<'a>(
    ctx: &SessionContext,
    ingest: &Ingest,
    files: &[&'a SegmentMeta],
    ids: &HashSet<RunId>,
    scope: &Scope,
) -> Result<Vec<(&'a SegmentMeta, StoredRow)>, QueryError> {
    use futures_util::{StreamExt, TryStreamExt};
    if ids.is_empty() || files.is_empty() {
        return Ok(vec![]);
    }
    let short: Option<Vec<RunId>> = (ids.len() <= IN_CHUNK).then(|| ids.iter().copied().collect());
    let reads: Vec<_> = files
        .iter()
        .map(|seg| versions_in(ctx, ingest, seg, short.as_deref(), ids, scope))
        .collect();
    let per_file: Vec<Vec<(&'a SegmentMeta, StoredRow)>> = futures_util::stream::iter(reads)
        .buffer_unordered(LOOKUP_FILES)
        .try_collect()
        .await?;
    Ok(per_file.into_iter().flatten().collect())
}

async fn versions_in<'a>(
    ctx: &SessionContext,
    ingest: &Ingest,
    seg: &'a SegmentMeta,
    short: Option<&[RunId]>,
    ids: &HashSet<RunId>,
    scope: &Scope,
) -> Result<Vec<(&'a SegmentMeta, StoredRow)>, QueryError> {
    let held: Vec<RunId>;
    let list = match short {
        Some(list) => list,
        None => {
            held = read_ids(ctx, ingest, seg, scope.df_filter())
                .await?
                .into_iter()
                .filter(|id| ids.contains(id))
                .collect();
            &held[..]
        }
    };
    if list.is_empty() {
        return Ok(vec![]);
    }
    let tag = |rows: Vec<StoredRow>| rows.into_iter().map(|r| (seg, r));
    // A file mostly made of these runs (a late-update file) is read whole.
    if list.len() > IN_CHUNK || (short.is_none() && list.len() as u64 * 2 > seg.row_count) {
        let rows = read_core(ctx, ingest, &[seg], scope.df_filter()).await?;
        return Ok(tag(rows).filter(|(_, r)| ids.contains(&r.state.run.run_id)).collect());
    }
    Ok(tag(read_core(ctx, ingest, &[seg], Some(by_ids(list, scope))).await?).collect())
}

/// Rows of `files` matching `filter`, with their file (one scan per file).
pub(crate) async fn read_core_each<'a>(
    ctx: &SessionContext,
    ingest: &Ingest,
    files: &[&'a SegmentMeta],
    filter: Option<DfExpr>,
) -> Result<Vec<(&'a SegmentMeta, StoredRow)>, QueryError> {
    use futures_util::{StreamExt, TryStreamExt};
    let reads: Vec<_> = files
        .iter()
        .map(|seg| read_tagged(ctx, ingest, seg, filter.clone()))
        .collect();
    let per_file: Vec<Vec<(&'a SegmentMeta, StoredRow)>> = futures_util::stream::iter(reads)
        .buffer_unordered(LOOKUP_FILES)
        .try_collect()
        .await?;
    Ok(per_file.into_iter().flatten().collect())
}

async fn read_tagged<'a>(
    ctx: &SessionContext,
    ingest: &Ingest,
    seg: &'a SegmentMeta,
    filter: Option<DfExpr>,
) -> Result<Vec<(&'a SegmentMeta, StoredRow)>, QueryError> {
    let rows = read_core(ctx, ingest, &[seg], filter).await?;
    Ok(rows.into_iter().map(|r| (seg, r)).collect())
}

/// The run ids of one file's rows matching `filter` (reads only the columns it needs); none if it's damaged.
async fn read_ids(
    ctx: &SessionContext,
    ingest: &Ingest,
    seg: &SegmentMeta,
    filter: Option<DfExpr>,
) -> Result<Vec<RunId>, QueryError> {
    if ingest.is_damaged(&seg.id) {
        return Ok(vec![]);
    }
    match read_ids_from(ctx, ingest, seg, filter).await {
        Err(e) if damaged_alone(ingest, seg, &e).await => Ok(vec![]),
        r => r,
    }
}

async fn read_ids_from(
    ctx: &SessionContext,
    ingest: &Ingest,
    seg: &SegmentMeta,
    filter: Option<DfExpr>,
) -> Result<Vec<RunId>, QueryError> {
    use datafusion::arrow::array::{Array, FixedSizeBinaryArray};
    let store = &ingest.storage().store;
    let mut df = ctx
        .read_parquet(store.url(&seg.core_path), ParquetReadOptions::default())
        .await?;
    if let Some(f) = filter {
        df = df.filter(f)?;
    }
    let mut out = Vec::new();
    for b in df.select_columns(&["run_id"])?.collect().await? {
        let ids = b
            .column(0)
            .as_any()
            .downcast_ref::<FixedSizeBinaryArray>()
            .ok_or_else(|| smithling_storage::StorageError::Format("run_id is not FixedSizeBinary".into()))?;
        out.extend(
            (0..ids.len())
                .filter(|&i| !ids.is_null(i))
                .filter_map(|i| RunId::from_slice(ids.value(i)).ok()),
        );
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
    collect_pinned(ctx, ingest, &pin(ingest, scope), scope, pushed, stats).await
}

/// [`collect`] over a pin taken for `scope` or a wider one.
pub async fn collect_pinned(
    ctx: &SessionContext,
    ingest: &Ingest,
    pinned: &Pinned,
    scope: &Scope,
    pushed: Option<DfExpr>,
    stats: &mut ScanStats,
) -> Result<Vec<Merged>, QueryError> {
    // A run belongs to one trace, so trace chunks can be collected separately; each keeps its `IN (…)` list.
    if let Some(ts) = scope.traces.as_ref().filter(|ts| ts.len() > IN_CHUNK) {
        let ids: Vec<TraceId> = ts.iter().copied().collect();
        let mut out = Vec::new();
        for chunk in ids.chunks(IN_CHUNK) {
            let part = Scope {
                traces: Some(Arc::new(chunk.iter().copied().collect())),
                ..scope.clone()
            };
            out.extend(Box::pin(collect_pinned(ctx, ingest, pinned, &part, pushed.clone(), stats)).await?);
        }
        return Ok(out);
    }
    let inner = scope.narrowed();
    let mut fresh: Vec<(RunState, Attrs)> = pinned.rows_in(&inner).collect();
    let all: Vec<&SegmentMeta> = pinned.version.segments.iter().map(|s| &**s).collect();
    let files: Vec<&SegmentMeta> = all.iter().copied().filter(|s| inner.file_in_scope(s)).collect();
    stats.files_total = stats.files_total.max(all.len());
    stats.scanned.extend(files.iter().map(|s| s.id.clone()));
    stats.files_scanned = stats.scanned.len();

    // With the kept merged rows (D-023), multi-version runs come from them, not from their file rows.
    let kept = pinned.kept.as_deref();
    let row_filter = match (pushed, kept) {
        (Some(p), None) => Some(p.or(col("needs_merge").eq(lit(true)))),
        (p, _) => p,
    };
    let filter = match (inner.df_filter(), row_filter) {
        (Some(a), Some(b)) => Some(a.and(b)),
        (a, b) => a.or(b),
    };
    let mut rows = read_core(ctx, ingest, &files, filter).await?;
    stats.rows_read += rows.len();

    // Runs that may have several versions: marked in a file, created by an update, or in the multi-version set.
    let mvset = ingest.multi_version();
    let mut multi_ids: HashSet<RunId> = {
        let mv = mvset.read().unwrap();
        rows.iter()
            .map(|r| (&r.state, r.needs_merge))
            .chain(fresh.iter().map(|(s, _)| (s, false)))
            .filter(|(st, needs)| *needs || st.created_by_patch() || mv.contains(&st.run.run_id))
            .map(|(st, _)| st.run.run_id)
            .collect()
    };
    // Kept runs (D-023) in this window and project by their merged file rows; one with a memtable version here
    // is merged with it below whatever its file rows say (they may lack the project or the start).
    let mut in_window: Vec<RunId> = Vec::new();
    if let Some(kept) = kept {
        rows.retain(|r| !kept.ids.contains(&r.state.run.run_id));
        for (id, k) in &kept.merged {
            let st = &k.state;
            if inner.row_in_scope(start_key(&st.run.dotted_order), st)
                && scope.project.as_ref().is_none_or(|p| st.run.project == *p)
            {
                in_window.push(*id);
            }
        }
        multi_ids.extend(&in_window);
    }
    // A run in this window may have memtable versions anywhere in the scope.
    if scope.window.is_some() && !multi_ids.is_empty() {
        fresh.extend(
            pinned
                .rows_in(scope)
                .filter(|(st, _)| multi_ids.contains(&st.run.run_id)),
        );
    }
    if let Some(kept) = kept {
        multi_ids.retain(|id| !kept.ids.contains(id));
    }
    // Fetch every version of the others, from every file that may hold one (any project).
    let mut extra = Vec::new();
    if !multi_ids.is_empty() {
        let vfiles: Vec<&SegmentMeta> = all
            .iter()
            .copied()
            .filter(|s| scope.file_may_hold_versions(s))
            .collect();
        extra = versions_of(ctx, ingest, &vfiles, &multi_ids, scope).await?;
        stats.looked_up.extend(vfiles.iter().map(|s| s.id.clone()));
        stats.files_version_lookup = stats.looked_up.len();
        stats.rows_read += extra.len();
        stats.merge_rows += extra.len();
    }

    let mut groups: HashMap<RunId, Vec<(RunState, Attrs)>> = HashMap::new();
    let mut seen: HashSet<(RunId, u64)> = HashSet::new();
    for (st, attrs) in rows.into_iter().chain(extra).map(|r| (r.state, r.attrs)).chain(fresh) {
        if seen.insert((st.run.run_id, st.first_seq)) {
            groups.entry(st.run.run_id).or_default().push((st, attrs));
        }
    }
    // Kept runs alone stay slim (no `metadata`/`extra`) until shown; merging needs them whole.
    let mut alone: Vec<Arc<crate::fields::Kept>> = Vec::new();
    if let Some(kept) = kept {
        for (id, vs) in groups.iter_mut() {
            if let Some(k) = kept.merged.get(id) {
                vs.push(k.full());
            }
        }
        alone.extend(
            in_window
                .iter()
                .filter(|id| !groups.contains_key(id))
                .map(|id| kept.merged[id].clone()),
        );
    }
    stats.rows_merged += alone.len();
    // A run seen more than once is merged even if nothing marked it (e.g. a second start after a flush).
    let mut out: Vec<Merged> = groups
        .into_values()
        .map(|versions| {
            let multi = versions.len() > 1;
            if multi {
                stats.rows_merged += versions.len();
            }
            merge_group(versions, multi)
        })
        .collect();
    out.extend(alone.into_iter().map(|k| Merged {
        state: k.state.clone(),
        attrs: k.attrs.clone(),
        multi: true,
        kept: Some(k),
    }));
    out.retain(|m| inner.matches_key(start_key(&m.state.run.dotted_order)));
    Ok(out)
}

/// Merges versions and their attributes: for each attribute root, the newest version that set it wins.
pub(crate) fn merge_group(mut versions: Vec<(RunState, Attrs)>, multi: bool) -> Merged {
    if versions.len() == 1 {
        let (state, attrs) = versions.pop().expect("one version");
        return Merged {
            state,
            attrs,
            multi,
            kept: None,
        };
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
    Merged {
        state,
        attrs,
        multi,
        kept: None,
    }
}

/// One run's versions, for [`crate::Query::run`]: its memtable states (with payload) and its file rows (core
/// only), from one pinned file set.
pub(crate) struct RunVersions {
    pub version: Arc<Version>,
    mem: Vec<(RunState, Attrs)>,
    files: Vec<StoredRow>,
}

/// Reads `run`'s versions: one `run_id = …` scan over every file (bloom filters skip most); `None` if it has none.
pub(crate) async fn run_versions(
    ctx: &SessionContext,
    ingest: &Ingest,
    run: RunId,
    stats: &mut ScanStats,
) -> Result<Option<RunVersions>, QueryError> {
    let (version, mem) = {
        let m = ingest.mem();
        let m = m.read().unwrap();
        let mut v = Vec::new();
        for store in m.frozen.as_deref().into_iter().chain(std::iter::once(&m.active)) {
            if let Some(st) = store.get(&run) {
                v.push((
                    st.clone(),
                    store.attrs(&run).cloned().unwrap_or_else(|| promote(&st.run)),
                ));
            }
        }
        (m.version.clone(), v)
    };
    let files: Vec<&SegmentMeta> = version.segments.iter().map(|s| &**s).collect();
    stats.files_total = stats.files_total.max(files.len());
    let rows = read_core(ctx, ingest, &files, Some(col("run_id").eq(id_lit(run)))).await?;
    stats.rows_read += rows.len();
    if rows.is_empty() && mem.is_empty() {
        return Ok(None);
    }
    Ok(Some(RunVersions {
        version,
        mem,
        files: rows,
    }))
}

impl RunVersions {
    pub(crate) fn merged(&self) -> Merged {
        let mut seen = HashSet::new();
        let versions: Vec<(RunState, Attrs)> = self
            .files
            .iter()
            .map(|r| (r.state.clone(), r.attrs.clone()))
            .chain(self.mem.iter().map(|(st, a)| (st.without_payload(), a.clone())))
            .filter(|(st, _)| seen.insert(st.first_seq))
            .collect();
        let multi = versions.len() > 1;
        merge_group(versions, multi)
    }

    /// The payload fields asked for, each from the newest version that set it (the merge rule), read only
    /// from the files that hold a version setting one of them.
    pub(crate) async fn payload(
        &self,
        ingest: &Ingest,
        run: RunId,
        (inputs, outputs, events): (bool, bool, bool),
        stats: &mut ScanStats,
    ) -> Result<Payload, QueryError> {
        let bits = [
            (inputs, present::INPUTS),
            (outputs, present::OUTPUTS),
            (events, present::EVENTS),
        ]
        .iter()
        .filter(|b| b.0)
        .fold(0, |acc, b| acc | b.1);
        // (first_seq, present bits, payload)
        let mut versions: Vec<(u64, u32, Payload)> = self
            .mem
            .iter()
            .map(|(st, _)| {
                let r = &st.run;
                let p = Payload {
                    inputs: r.inputs.clone(),
                    outputs: r.outputs.clone(),
                    events: r.events.clone(),
                };
                (st.first_seq, st.present, p)
            })
            .collect();
        let (store, cache) = (&ingest.storage().store, &ingest.storage().cache);
        // A version is in a file of the flush that took its events (one file per project), told apart by run id.
        let reads = self.files.iter().filter(|r| r.state.present & bits != 0).map(|row| {
            let (seq, trace) = (row.state.first_seq, row.state.run.trace_id);
            let candidates: Vec<&SegmentMeta> = self
                .version
                .segments
                .iter()
                .map(|s| &**s)
                .filter(|s| s.min_seq <= seq && seq <= s.max_seq && s.min_trace_id <= trace && trace <= s.max_trace_id)
                // A version that names its project is filed under it; a row number past a file's rows isn't in it.
                .filter(|s| !row.state.project_set || s.project == row.state.run.project)
                .filter(|s| (row.payload_row as u64) < s.row_count && !ingest.is_damaged(&s.id))
                .collect();
            async move {
                let mut opened = 0;
                for seg in candidates {
                    opened += 1;
                    let got = smithling_storage::segment::read_payload_rows(
                        store,
                        cache,
                        seg,
                        &[row.payload_row],
                        inputs,
                        outputs,
                        events,
                    )
                    .await;
                    match got.map_err(QueryError::from) {
                        Ok(got) => {
                            if let Some((_, _, p)) = got.into_iter().find(|g| g.1 == run) {
                                return Ok((opened, Some((row.state.first_seq, row.state.present, p))));
                            }
                        }
                        Err(e) if damaged_alone(ingest, seg, &e).await => {}
                        Err(e) => return Err(e),
                    }
                }
                Ok::<_, QueryError>((opened, None))
            }
        });
        for r in futures_util::future::join_all(reads).await {
            let (opened, v) = r?;
            stats.payload_files += opened;
            stats.payload_rows += v.is_some() as usize;
            versions.extend(v);
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
}

/// How many runs name `parent` as theirs, from the memtables and the `trace`'s file rows that do (not the
/// whole trace).
pub(crate) async fn child_count(
    ctx: &SessionContext,
    ingest: &Ingest,
    trace: TraceId,
    parent: RunId,
    stats: &mut ScanStats,
) -> Result<u32, QueryError> {
    let (version, mut ids) = {
        let m = ingest.mem();
        let m = m.read().unwrap();
        let mut ids: HashSet<RunId> = HashSet::new();
        for store in m.frozen.as_deref().into_iter().chain(std::iter::once(&m.active)) {
            ids.extend(
                store
                    .trace_run_ids(&trace)
                    .iter()
                    .filter(|id| store.get(id).is_some_and(|st| st.run.parent_run_id == Some(parent))),
            );
        }
        (m.version.clone(), ids)
    };
    let scope = Scope {
        trace_id: Some(trace),
        ..Scope::default()
    };
    let files: Vec<&SegmentMeta> = version
        .segments
        .iter()
        .map(|s| &**s)
        .filter(|s| scope.file_may_hold_versions(s))
        .collect();
    let filter = col("trace_id")
        .eq(id_lit(trace))
        .and(col("parent_run_id").eq(id_lit(parent)));
    let rows = read_core(ctx, ingest, &files, Some(filter)).await?;
    stats.rows_read += rows.len();
    ids.extend(rows.iter().map(|r| r.state.run.run_id));
    Ok(ids.len() as u32)
}
