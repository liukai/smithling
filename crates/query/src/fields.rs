//! Field catalog (data-model §10): the paths runs in scope have, with types and counts, and a path's most
//! common values. Summed from the pinned version's per-file stats plus stats computed now over the memtables;
//! runs in the multi-version set are taken out of both and counted once, merged (D-023).
//!
//! Counts are approximate where: a file counts whole when its time range overlaps the scope; a file cut a
//! path's values off at its top 50; a run became multi-version during the request; or a run got a second
//! start after its first was flushed (not in the multi-version set, D-016); or a file stopped at 10,000 paths.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use datafusion::logical_expr::{col, lit};
use serde::Serialize;
use serde_json::Value;
use tracedb_ingest::store::RunStore;
use tracedb_model::attrs::{Attrs, promote};
use tracedb_model::{RunId, RunState, Ts};
use tracedb_storage::segment::{FieldStat, INCOMPLETE_PATH, SegmentMeta, StatsBuilder, TOP_VALUES, start_key};

use crate::scan::{self, merge_group, read_core, versions_of};
use crate::{Query, QueryError, TimeRange};

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TypeCount {
    #[serde(rename = "type")]
    pub ty: String,
    pub count: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FieldInfo {
    pub path: String,
    pub types: Vec<TypeCount>,
    /// Every value of this path is known with its exact count (no file cut it off at its top 50).
    pub complete: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Fields {
    pub fields: Vec<FieldInfo>,
    /// No file in scope stopped listing paths at the 10,000-path cap.
    pub complete: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ValueCount {
    pub value: Value,
    pub count: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Values {
    pub values: Vec<ValueCount>,
    /// Runs with the path whose value isn't listed (listed counts + `other_count` = runs with the path).
    pub other_count: u64,
    /// Some file only kept its top 50 values, so listed counts may be low and unlisted values may match.
    pub approximate: bool,
}

/// Active-memtable runs read per lock hold, so suggestions never stall the writer for long.
const MEM_BATCH: usize = 4_096;

struct Scope<'a> {
    project: Option<&'a str>,
    from: Option<i64>,
    to: Option<i64>,
}

impl Scope<'_> {
    fn file(&self, s: &SegmentMeta) -> bool {
        self.project.is_none_or(|p| s.project == p)
            && self.from.is_none_or(|f| s.max_start_key >= f)
            && self.to.is_none_or(|t| s.min_start_key <= t)
    }

    fn run(&self, st: &RunState) -> bool {
        let key = start_key(&st.run.dotted_order);
        self.project.is_none_or(|p| st.project_set && st.run.project == p)
            && self.from.is_none_or(|f| key >= f)
            && self.to.is_none_or(|t| key <= t)
    }
}

/// File rows of multi-version runs, kept across requests: files are immutable and the set only grows, so a
/// request reads only new ids (in every file) and new files (rows marked `needs_merge`). Requests pin and
/// update under one lock, so the rows never come from files newer than the caller's pin.
#[derive(Default)]
pub struct VersionRows {
    segments: Vec<Arc<SegmentMeta>>,
    known: HashSet<String>,
    /// Ids that joined while this memtable was being flushed: its file may not mark them `needs_merge` (the
    /// flusher reads the set once, at its start), so new files are searched for them until it's published.
    flushing: Option<(Arc<RunStore>, Vec<RunId>)>,
    rows: Arc<Rows>,
}

#[derive(Clone, Default)]
struct Rows {
    ids: HashSet<RunId>,
    /// Per run, its file rows merged.
    merged: HashMap<RunId, (RunState, Attrs)>,
    /// Per file, stats of its rows of multi-version runs, and those finished.
    minus: HashMap<String, (StatsBuilder, Vec<FieldStat>)>,
}

impl VersionRows {
    async fn update(
        &mut self,
        q: &Query,
        files: &[Arc<SegmentMeta>],
        frozen: Option<&Arc<RunStore>>,
        multi: &HashSet<RunId>,
    ) -> Result<(), QueryError> {
        let new: Vec<Arc<SegmentMeta>> = files.iter().filter(|s| !self.known.contains(&s.id)).cloned().collect();
        let new_ids: Vec<RunId> = multi.iter().filter(|id| !self.rows.ids.contains(id)).copied().collect();
        let none = scan::Scope::default();
        let mut read = Vec::new();
        if !new_ids.is_empty() {
            let all: Vec<&SegmentMeta> = self.segments.iter().chain(&new).map(|s| &**s).collect();
            read.extend(versions_of(&q.ctx, &q.ingest, &all, &new_ids, &none).await?);
        }
        let pending: HashSet<RunId> = self.flushing.iter().flat_map(|f| f.1.iter().copied()).collect();
        if !new.is_empty() && !self.rows.ids.is_empty() {
            let files: Vec<&SegmentMeta> = new.iter().map(|s| &**s).collect();
            let marked = read_core(&q.ctx, &q.ingest, &files, Some(col("needs_merge").eq(lit(true)))).await?;
            let ids = &self.rows.ids;
            read.extend(
                marked
                    .into_iter()
                    .filter(|r| ids.contains(&r.state.run.run_id) && !pending.contains(&r.state.run.run_id)),
            );
            if !pending.is_empty() {
                let pending: Vec<RunId> = pending.iter().copied().collect();
                read.extend(versions_of(&q.ctx, &q.ingest, &files, &pending, &none).await?);
            }
        }
        // Nothing is kept until every read is done, so a failed or dropped request leaves no gap.
        self.flushing = match (self.flushing.take(), frozen) {
            (Some((f, mut ids)), Some(now)) if Arc::ptr_eq(&f, now) => {
                ids.extend(&new_ids);
                Some((f, ids))
            }
            (_, now) => now.map(|f| (f.clone(), new_ids.clone())),
        };
        self.known.extend(new.iter().map(|s| s.id.clone()));
        self.segments.extend(new);
        if new_ids.is_empty() && read.is_empty() {
            return Ok(());
        }
        let rows = Arc::make_mut(&mut self.rows);
        rows.ids.extend(new_ids);
        // A file holds one memtable's rows of one project, and memtables cover disjoint seq ranges.
        let mut by_project: HashMap<&str, Vec<&SegmentMeta>> = HashMap::new();
        for s in &self.segments {
            by_project.entry(s.project.as_str()).or_default().push(s);
        }
        for segs in by_project.values_mut() {
            segs.sort_by_key(|s| s.min_seq);
        }
        let mut touched: HashMap<RunId, Vec<(RunState, Attrs)>> = HashMap::new();
        let mut touched_files = HashSet::new();
        for row in read {
            let st = &row.state;
            let project = if st.project_set { st.run.project.as_str() } else { "" };
            let segs = by_project.get(project).map_or(&[][..], |v| &v[..]);
            let i = segs.partition_point(|s| s.min_seq <= st.first_seq);
            let Some(seg) = i.checked_sub(1).map(|i| segs[i]).filter(|s| st.last_seq <= s.max_seq) else {
                debug_assert!(false, "a row read from a file matches no file");
                continue;
            };
            rows.minus.entry(seg.id.clone()).or_default().0.add_run(st, &row.attrs);
            touched_files.insert(seg.id.clone());
            touched.entry(st.run.run_id).or_default().push((row.state, row.attrs));
        }
        for id in touched_files {
            let (b, done) = rows.minus.get_mut(&id).expect("just added");
            *done = b.clone().finish_all(&id);
        }
        // Rows read later are from later files, so merging onto the kept state continues the same fold.
        for (id, mut vs) in touched {
            vs.extend(rows.merged.remove(&id));
            let m = merge_group(vs, true);
            rows.merged.insert(id, (m.state, m.attrs));
        }
        Ok(())
    }
}

/// Stats rows in scope. `minus` holds the file rows of multi-version runs, to subtract; their merged runs
/// are in `fresh`.
struct Catalog {
    files: Vec<Arc<FieldStat>>,
    fresh: Vec<FieldStat>,
    minus: Vec<FieldStat>,
}

impl Catalog {
    fn added(&self) -> impl Iterator<Item = &FieldStat> {
        self.files.iter().map(|f| &**f).chain(&self.fresh)
    }
}

impl Query {
    async fn catalog(&self, project: Option<&str>, time: TimeRange) -> Result<Catalog, QueryError> {
        let now = Ts::now();
        let scope = Scope {
            project,
            from: time.from.map(|t| t.resolve(now).0),
            to: time.to.map(|t| t.resolve(now).0),
        };
        let mem = self.ingest.mem();
        let mvset = self.ingest.multi_version();
        let (version, frozen, ids, cache) = {
            let mut cache = self.version_rows.lock().await;
            let (version, frozen, ids, multi) = {
                let mem = mem.read().unwrap();
                let ids: Vec<RunId> = mem.active.states().map(|st| st.run.run_id).collect();
                let multi = mvset.read().unwrap().clone();
                (mem.version.clone(), mem.frozen.clone(), ids, multi)
            };
            cache.update(self, &version.segments, frozen.as_ref(), &multi).await?;
            (version, frozen, ids, cache.rows.clone())
        };
        // Runs frozen or updated between batches may be missed or seen newer; the catalog is approximate.
        let mut stats = StatsBuilder::default();
        let mut versions: HashMap<RunId, Vec<(RunState, Attrs)>> = HashMap::new();
        let mut add = |st: &RunState, attrs: Option<&Attrs>| {
            if cache.ids.contains(&st.run.run_id) {
                let attrs = attrs.cloned().unwrap_or_else(|| promote(&st.run));
                versions
                    .entry(st.run.run_id)
                    .or_default()
                    .push((st.without_payload(), attrs));
            } else if scope.run(st) {
                match attrs {
                    Some(a) => stats.add_run(st, a),
                    None => stats.add_run(st, &promote(&st.run)),
                }
            }
        };
        for batch in ids.chunks(MEM_BATCH) {
            let mem = mem.read().unwrap();
            for st in batch.iter().filter_map(|id| mem.active.get(id)) {
                add(st, mem.active.attrs(&st.run.run_id));
            }
        }
        if let Some(f) = &frozen {
            for st in f.states() {
                add(st, f.attrs(&st.run.run_id));
            }
        }
        for (id, (st, attrs)) in &cache.merged {
            if !versions.contains_key(id) && scope.run(st) {
                stats.add_run(st, attrs);
            }
        }
        // Memtable versions are newer than every file row kept for this pin.
        for (id, mut vs) in versions {
            vs.extend(cache.merged.get(&id).cloned());
            let m = merge_group(vs, true);
            if scope.run(&m.state) {
                stats.add_run(&m.state, &m.attrs);
            }
        }
        let counted: HashSet<&str> = version
            .segments
            .iter()
            .filter(|s| scope.file(s))
            .map(|s| s.id.as_str())
            .collect();
        let minus = counted
            .iter()
            .filter_map(|id| cache.minus.get(*id))
            .flat_map(|(_, done)| done.iter().cloned())
            .collect();
        let files = version
            .field_stats
            .iter()
            .filter(|f| counted.contains(f.segment_id.as_str()))
            .cloned()
            .collect();
        Ok(Catalog {
            files,
            fresh: stats.finish("memtable"),
            minus,
        })
    }

    /// Paths starting with `prefix`, most common first.
    pub async fn fields(&self, project: Option<&str>, time: TimeRange, prefix: &str) -> Result<Fields, QueryError> {
        let c = self.catalog(project, time).await?;
        let mut complete = true;
        // path → (type → count, all values known)
        let mut by_path: BTreeMap<&str, (BTreeMap<&str, u64>, bool)> = BTreeMap::new();
        for f in c.added() {
            if f.path == INCOMPLETE_PATH {
                complete = false;
                continue;
            }
            if !f.path.starts_with(prefix) {
                continue;
            }
            let e = by_path.entry(&f.path).or_insert_with(|| (BTreeMap::new(), true));
            *e.0.entry(&f.ty).or_default() += f.count;
            e.1 &= f.other_count == 0;
        }
        for f in &c.minus {
            if let Some(n) = by_path
                .get_mut(f.path.as_str())
                .and_then(|e| e.0.get_mut(f.ty.as_str()))
            {
                *n = n.saturating_sub(f.count);
            }
        }
        let mut fields: Vec<FieldInfo> = by_path
            .into_iter()
            .filter(|(_, (types, _))| types.values().any(|&n| n > 0))
            .map(|(path, (types, exact))| {
                let mut types: Vec<TypeCount> = types
                    .into_iter()
                    .filter(|&(_, count)| count > 0)
                    .map(|(ty, count)| TypeCount { ty: ty.into(), count })
                    .collect();
                types.sort_by_key(|t| std::cmp::Reverse(t.count));
                FieldInfo {
                    path: path.into(),
                    types,
                    complete: exact,
                }
            })
            .collect();
        let total = |f: &FieldInfo| f.types.iter().map(|t| t.count).sum::<u64>();
        fields.sort_by(|a, b| total(b).cmp(&total(a)).then_with(|| a.path.cmp(&b.path)));
        Ok(Fields { fields, complete })
    }

    /// The most common values (at most 50) of `path` whose text starts with `prefix`.
    pub async fn values(
        &self,
        project: Option<&str>,
        time: TimeRange,
        path: &str,
        prefix: &str,
    ) -> Result<Values, QueryError> {
        let c = self.catalog(project, time).await?;
        let (mut counts, mut other_count) = (HashMap::<String, (Value, u64)>::new(), 0);
        for f in c.added().filter(|f| f.path == path) {
            other_count += f.other_count;
            for (v, n) in &f.top_values {
                counts.entry(v.to_string()).or_insert_with(|| (v.clone(), 0)).1 += n;
            }
        }
        let approximate = other_count > 0;
        for f in c.minus.iter().filter(|f| f.path == path) {
            for (v, n) in &f.top_values {
                let listed = counts.get_mut(&v.to_string()).map_or(0, |e| {
                    let k = e.1.min(*n);
                    e.1 -= k;
                    k
                });
                other_count = other_count.saturating_sub(n - listed);
            }
        }
        counts.retain(|_, e| e.1 > 0);
        let listed: u64 = counts.values().map(|(_, n)| n).sum();
        let text = |v: &Value| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let mut values: Vec<ValueCount> = counts
            .into_values()
            .filter(|(v, _)| text(v).starts_with(prefix))
            .map(|(value, count)| ValueCount { value, count })
            .collect();
        values.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| text(&a.value).cmp(&text(&b.value))));
        values.truncate(TOP_VALUES);
        other_count += listed - values.iter().map(|v| v.count).sum::<u64>();
        Ok(Values {
            values,
            other_count,
            approximate,
        })
    }
}
