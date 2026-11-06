//! Field catalog (data-model §10): the paths runs in scope have, with types and counts, and a path's most
//! common values. Summed from the pinned version's per-file stats plus stats computed now over the memtables.
//!
//! Counts are approximate by design: a file counts whole when its time range overlaps the scope, a run whose
//! versions sit in several places counts once per version, and files of versions with no known project are
//! left out of a project's catalog.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;
use tracedb_model::attrs::Attrs;
use tracedb_model::{RunId, RunState, Ts};
use tracedb_storage::segment::{FieldStat, INCOMPLETE_PATH, SegmentMeta, StatsBuilder, TOP_VALUES, start_key};

use crate::{Query, TimeRange};

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

static NO_ATTRS: Attrs = Attrs::new();
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

impl Query {
    /// Stats rows in scope: from files (shared) and from the memtables (computed now).
    fn catalog(&self, project: Option<&str>, time: TimeRange) -> (Vec<Arc<FieldStat>>, Vec<FieldStat>) {
        let now = Ts::now();
        let scope = Scope {
            project,
            from: time.from.map(|t| t.resolve(now).0),
            to: time.to.map(|t| t.resolve(now).0),
        };
        let mem = self.ingest.mem();
        let (version, frozen, ids) = {
            let mem = mem.read().unwrap();
            let ids: Vec<RunId> = mem.active.states().map(|st| st.run.run_id).collect();
            (mem.version.clone(), mem.frozen.clone(), ids)
        };
        // Runs frozen or updated between batches may be missed or seen newer; the catalog is approximate.
        let mut stats = StatsBuilder::default();
        for batch in ids.chunks(MEM_BATCH) {
            let mem = mem.read().unwrap();
            for st in batch
                .iter()
                .filter_map(|id| mem.active.get(id))
                .filter(|st| scope.run(st))
            {
                stats.add_run(st, mem.active.attrs(&st.run.run_id).unwrap_or(&NO_ATTRS));
            }
        }
        if let Some(f) = frozen {
            for st in f.states().filter(|st| scope.run(st)) {
                stats.add_run(st, f.attrs(&st.run.run_id).unwrap_or(&NO_ATTRS));
            }
        }
        let fresh = stats.finish("memtable");
        let ids: HashSet<&str> = version
            .segments
            .iter()
            .filter(|s| scope.file(s))
            .map(|s| s.id.as_str())
            .collect();
        let files = version
            .field_stats
            .iter()
            .filter(|f| ids.contains(f.segment_id.as_str()))
            .cloned()
            .collect();
        (files, fresh)
    }

    /// Paths starting with `prefix`, most common first.
    pub fn fields(&self, project: Option<&str>, time: TimeRange, prefix: &str) -> Fields {
        let (files, fresh) = self.catalog(project, time);
        let mut complete = true;
        // path → (type → count, all values known)
        let mut by_path: BTreeMap<&str, (BTreeMap<&str, u64>, bool)> = BTreeMap::new();
        for f in files.iter().map(|f| &**f).chain(&fresh) {
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
        let mut fields: Vec<FieldInfo> = by_path
            .into_iter()
            .map(|(path, (types, exact))| {
                let mut types: Vec<TypeCount> = types
                    .into_iter()
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
        Fields { fields, complete }
    }

    /// The most common values (at most 50) of `path` whose text starts with `prefix`.
    pub fn values(&self, project: Option<&str>, time: TimeRange, path: &str, prefix: &str) -> Values {
        let (files, fresh) = self.catalog(project, time);
        let (mut counts, mut other_count) = (HashMap::<String, (Value, u64)>::new(), 0);
        for f in files.iter().map(|f| &**f).chain(&fresh).filter(|f| f.path == path) {
            other_count += f.other_count;
            for (v, n) in &f.top_values {
                counts.entry(v.to_string()).or_insert_with(|| (v.clone(), 0)).1 += n;
            }
        }
        let approximate = other_count > 0;
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
        Values {
            values,
            other_count,
            approximate,
        }
    }
}
