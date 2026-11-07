//! Aggregates (design §4.3, research M3-R4): every run in scope lands on exactly one side.
//!
//! - **Clean**: file rows with `needs_merge = false` whose run isn't multi-version or in a memtable. DataFusion
//!   reads only the columns the groups and metrics need, with the filter pushed down; an exact pushdown goes
//!   straight to the accumulators, otherwise rows are decoded and [`eval`] decides.
//! - **Dirty**: multi-version runs (their file rows merged once and kept, D-023) plus memtable versions, merged
//!   and evaluated like a list.
//!
//! Groups are finished from mergeable accumulators: top N by runs, `(none)`, `(other)`, tags unnested.

pub mod accum;
mod clean;
mod dirty;

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use serde::Serialize;
use tracedb_model::agg::{AggSpec, GroupKey, Metric, SumOf};
use tracedb_model::attrs::{Attrs, Scalar};
use tracedb_model::cost::{self, Prices};
use tracedb_model::filter::{AttrRoot, CmpOp, EvalStats, Expr, Field, Path, Row, Value, eval, print, status_name};
use tracedb_model::run::run_type_str;
use tracedb_model::{Run, TraceId, Ts};
use tracedb_storage::segment::start_key;

pub use accum::{Acc, Cells, Facts, Keep};

use crate::scan::{self, Scope};
use crate::{Query, QueryError, QuerySpec, QueryWarning};

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AggResult {
    pub query_id: String,
    pub partial: bool,
    /// Every run counted once (tag groups overlap; this doesn't).
    pub totals: Cells,
    pub groups: Vec<Group>,
    /// Top N applied: some runs are in `(other)`.
    pub cut: bool,
    /// Groups overlap (grouped by tag).
    pub overlap: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<QueryWarning>,
    pub stats: AggStats,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Group {
    /// One value per group key: a value, `"(none)"`, or `"(other)"`.
    pub key: Vec<serde_json::Value>,
    pub cells: Cells,
    /// Filter text selecting exactly this group's runs (added to the query's filter); `null` for a tag
    /// group's `(other)`, which no filter can say.
    #[serde(rename = "where")]
    pub where_: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct AggStats {
    pub files_scanned: usize,
    /// Runs counted from single-version file rows.
    pub clean_rows: usize,
    /// Runs counted after a merge or from a memtable (`clean_rows + dirty_runs` = `totals.runs`).
    pub dirty_runs: usize,
    pub ms: u64,
}

pub const NONE: &str = "(none)";
pub const OTHER: &str = "(other)";

/// One group-key value of a run. `Tags` is a run's whole (sorted, distinct) tag set, unnested at finish.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum KeyVal {
    None,
    Bool(bool),
    /// f64 bits in total order.
    Num(i64),
    Str(String),
    /// Present but not promoted: never a group of its own, always `(other)`.
    Unindexed,
    Tags(Vec<String>),
}

fn num_key(x: f64) -> i64 {
    let x = if x == 0.0 { 0.0 } else { x };
    let b = x.to_bits() as i64;
    b ^ (((b >> 63) as u64) >> 1) as i64
}

fn num_of(k: i64) -> f64 {
    f64::from_bits((k ^ (((k >> 63) as u64) >> 1) as i64) as u64)
}

impl KeyVal {
    pub(crate) fn of_str(s: Option<&str>) -> KeyVal {
        s.map_or(KeyVal::None, |s| KeyVal::Str(s.to_string()))
    }

    pub(crate) fn of_scalar(s: Option<&Scalar>) -> KeyVal {
        match s {
            None => KeyVal::None,
            Some(Scalar::Str(s)) => KeyVal::Str(s.clone()),
            Some(Scalar::Num(n)) => KeyVal::Num(num_key(*n)),
            Some(Scalar::Bool(b)) => KeyVal::Bool(*b),
            Some(Scalar::Unindexed) => KeyVal::Unindexed,
        }
    }

    pub(crate) fn of_tags<'a>(tags: impl Iterator<Item = &'a str>) -> KeyVal {
        let mut t: Vec<String> = tags.map(str::to_string).collect();
        t.sort();
        t.dedup();
        if t.is_empty() { KeyVal::None } else { KeyVal::Tags(t) }
    }

    fn json(&self) -> serde_json::Value {
        match self {
            KeyVal::None => NONE.into(),
            KeyVal::Bool(b) => (*b).into(),
            KeyVal::Num(n) => serde_json::Number::from_f64(num_of(*n)).map_or(serde_json::Value::Null, Into::into),
            KeyVal::Str(s) => s.as_str().into(),
            KeyVal::Unindexed | KeyVal::Tags(_) => OTHER.into(),
        }
    }

    fn value(&self) -> Option<Value> {
        match self {
            KeyVal::Bool(b) => Some(Value::Bool(*b)),
            KeyVal::Num(n) => Some(Value::Num(num_of(*n))),
            KeyVal::Str(s) => Some(Value::Str(s.clone())),
            _ => None,
        }
    }

    /// The group values a run with this key value is in.
    fn expand(&self) -> Vec<KeyVal> {
        match self {
            KeyVal::Tags(ts) => ts.iter().map(|t| KeyVal::Str(t.clone())).collect(),
            other => vec![other.clone()],
        }
    }
}

/// The run type as filters see it.
pub(crate) fn run_type_text(run: &Run) -> &str {
    run.run_type_raw.as_deref().unwrap_or(run_type_str(run.run_type))
}

/// Key values of a merged (or decoded) run.
pub(crate) fn keys_of(keys: &[GroupKey], run: &Run, attrs: &Attrs) -> Vec<KeyVal> {
    keys.iter()
        .map(|k| match k {
            GroupKey::Name => KeyVal::of_str(run.name.as_deref()),
            GroupKey::Model => KeyVal::of_str(run.model.as_deref()),
            GroupKey::Status => KeyVal::of_str(Some(status_name(run.status))),
            GroupKey::RunType => KeyVal::of_str(Some(run_type_text(run))),
            GroupKey::Tag => KeyVal::of_tags(run.tags.iter().map(String::as_str)),
            GroupKey::Metadata(m) => KeyVal::of_scalar(attrs.get(&format!("metadata.{m}"))),
            GroupKey::FeedbackKey => KeyVal::None,
        })
        .collect()
}

/// What a request needs from each run, and the conditions a run must meet.
pub(crate) struct Plan<'a> {
    pub keys: &'a [GroupKey],
    pub scope: Scope,
    pub filter: Option<&'a Expr>,
    pub is_root: Option<bool>,
    /// Traces passing the trace and tree conditions.
    pub traces: Option<HashSet<TraceId>>,
    pub prices: &'a Prices,
    pub now: Ts,
    pub tokens: bool,
    pub cost: bool,
}

impl Plan<'_> {
    /// The conditions DataFusion can't check for a single-version row: project, trace lists and trace conditions.
    pub(crate) fn row_ok(&self, project: Option<&str>, trace: TraceId) -> bool {
        self.scope
            .project
            .as_ref()
            .is_none_or(|p| project.unwrap_or(tracedb_model::DEFAULT_PROJECT) == p)
            && self.scope.traces.as_ref().is_none_or(|t| t.contains(&trace))
            && self.traces.as_ref().is_none_or(|t| t.contains(&trace))
    }

    /// Every condition, for a merged or decoded run.
    pub(crate) fn admits(&self, run: &Run, attrs: &Attrs, stats: &mut EvalStats) -> bool {
        let timed = self.scope.from.is_some() || self.scope.to.is_some();
        (!timed || self.scope.matches_key(start_key(&run.dotted_order)))
            && self.row_ok(Some(&run.project), run.trace_id)
            && self.scope.trace_id.is_none_or(|t| run.trace_id == t)
            && self.scope.run_id.is_none_or(|r| run.run_id == r)
            && self.is_root.is_none_or(|r| run.is_root == r)
            && self
                .filter
                .is_none_or(|f| eval(f, &Row { run, attrs }, self.now, stats))
    }

    pub(crate) fn facts(&self, run: &Run) -> Facts {
        Facts {
            status: run.status,
            latency_ms: run.latency_ms.map(|v| v as i64),
            input_tokens: run.input_tokens,
            output_tokens: run.output_tokens,
            total_tokens: run.total_tokens,
            cost: if self.cost { cost::of(run, self.prices) } else { None },
            feedback: vec![],
        }
    }
}

/// Accumulators by raw key, sharing one latency-value cap.
pub(crate) struct Accs {
    pub by_key: HashMap<Vec<KeyVal>, Acc>,
    /// Some percentile was asked for.
    latency: bool,
    values: usize,
    cap: usize,
}

impl Accs {
    fn new(latency: bool, cap: usize) -> Accs {
        Accs {
            by_key: HashMap::new(),
            latency,
            values: 0,
            cap,
        }
    }

    fn keep(&self) -> Keep {
        match (self.latency, self.values <= self.cap) {
            (false, _) => Keep::Off,
            (true, true) => Keep::Values,
            (true, false) => Keep::Hist,
        }
    }

    pub(crate) fn add(&mut self, key: Vec<KeyVal>, f: &Facts) {
        let keep = self.keep();
        let acc = self.by_key.entry(key).or_insert_with(|| Acc::new(keep));
        if acc.add(f) {
            self.values += 1;
            if self.values > self.cap {
                self.by_key.values_mut().for_each(Acc::drop_values);
            }
        }
    }
}

impl Query {
    /// Latency values kept per query before percentiles come from the histogram (tests use small caps).
    pub fn with_value_cap(mut self, cap: usize) -> Query {
        self.value_cap = cap;
        self
    }

    /// The exact aggregate of the runs `q` selects (its order, limit, cursor and columns are ignored).
    pub async fn aggregate(&self, q: &QuerySpec, a: &AggSpec, prices: &Prices) -> Result<AggResult, QueryError> {
        let started = Instant::now();
        let now = Ts::now();
        let mut eval_stats = EvalStats::default();
        let scope = self.scope(q, now);
        // One pin for the trace conditions and both sides: the project's memtable runs (the walk checks the
        // rest of the scope) and every version of the multi-version runs.
        let tscope = Scope {
            project: scope.project.clone(),
            traces: scope.traces.clone(),
            ..Scope::default()
        };
        let (pinned, merged) = {
            let mut cache = self.version_rows.lock().await;
            let (pinned, frozen, multi) = scan::pin_with(&self.ingest, &tscope, true);
            cache
                .update(self, &pinned.version.segments, frozen.as_ref(), &multi)
                .await?;
            (dirty::Pin { pinned, multi }, cache.rows.clone())
        };
        let traces = self.trace_set(q, &pinned.pinned, &tscope, now, &mut eval_stats).await?;
        let has = |f: &dyn Fn(&Metric) -> bool| a.metrics.iter().any(f);
        let plan = Plan {
            keys: &a.group_by,
            scope,
            filter: q.filter.as_ref(),
            is_root: q.is_root,
            traces,
            prices,
            now,
            tokens: has(&|m| matches!(m, Metric::Sum(s) if *s != SumOf::Cost)),
            cost: has(&|m| *m == Metric::Sum(SumOf::Cost)),
        };
        let latency = has(&|m| matches!(m, Metric::Latency(_)));
        let mut accs = Accs::new(latency, self.value_cap);
        let mut stats = AggStats::default();
        let skip: HashSet<_> = pinned
            .multi
            .iter()
            .copied()
            .chain(pinned.pinned.rows.iter().map(|r| r.1.run.run_id))
            .collect();
        clean::scan(
            self,
            &pinned.pinned,
            &skip,
            &plan,
            &mut accs,
            &mut stats,
            &mut eval_stats,
        )
        .await?;
        dirty::walk(&pinned, &merged.merged, &plan, &mut accs, &mut stats, &mut eval_stats);
        let keep = if latency { Keep::Values } else { Keep::Off };
        let (totals, groups, cut) = finish(accs.by_key, keep, &a.group_by, &a.metrics, a.top as usize);
        let mut warnings = crate::eval_warnings(eval_stats);
        let skipped = self.skipped(&plan.scope);
        if !skipped.is_empty() {
            warnings.push(QueryWarning::SkippedFiles { ids: skipped });
        }
        stats.ms = started.elapsed().as_millis() as u64;
        Ok(AggResult {
            query_id: format!("q-{:016x}", xxhash_rust::xxh64::xxh64(&now.0.to_le_bytes(), q.hash())),
            partial: false,
            totals,
            groups,
            cut,
            overlap: a.group_by.contains(&GroupKey::Tag),
            warnings,
            stats,
        })
    }

    /// Traces whose root matches `trace_filter` and that hold a run matching `tree_filter` (`None`: no such
    /// conditions). Not bounded in time: a root may start before the window, a step after it.
    async fn trace_set(
        &self,
        q: &QuerySpec,
        pinned: &scan::Pinned,
        tscope: &Scope,
        now: Ts,
        eval_stats: &mut EvalStats,
    ) -> Result<Option<HashSet<TraceId>>, QueryError> {
        let mut ok: Option<HashSet<TraceId>> = None;
        for (f, is_root) in [(&q.trace_filter, Some(true)), (&q.tree_filter, None)] {
            let Some(f) = f else { continue };
            let mut stats = scan::ScanStats::default();
            let found: HashSet<TraceId> = self
                .matching(pinned, tscope, Some(f), is_root, now, &mut stats, eval_stats)
                .await?
                .into_iter()
                .map(|m| m.state.run.trace_id)
                .filter(|t| ok.as_ref().is_none_or(|ok| ok.contains(t)))
                .collect();
            ok = Some(found);
        }
        Ok(ok)
    }
}

/// Totals, then the shown groups: top N by runs (ties by key), `(none)` (one key), `(other)`; and whether
/// N cut anything off.
fn finish(
    by_key: HashMap<Vec<KeyVal>, Acc>,
    keep: Keep,
    keys: &[GroupKey],
    metrics: &[Metric],
    top: usize,
) -> (Cells, Vec<Group>, bool) {
    let raws: Vec<(Vec<KeyVal>, Acc)> = by_key.into_iter().collect();
    let mut totals = Acc::new(keep);
    for (_, a) in &raws {
        totals.merge(a);
    }
    let totals = totals.finish(metrics);
    if keys.is_empty() {
        return (totals, vec![], false);
    }
    let single = keys.len() == 1;
    // Group → (runs, raw indexes); per raw, its groups.
    let mut groups: HashMap<Vec<KeyVal>, (u64, Vec<usize>)> = HashMap::new();
    let mut raw_groups: Vec<Vec<Vec<KeyVal>>> = Vec::with_capacity(raws.len());
    for (i, (raw, acc)) in raws.iter().enumerate() {
        let mut tuples: Vec<Vec<KeyVal>> = vec![vec![]];
        for v in raw {
            tuples = tuples
                .into_iter()
                .flat_map(|t| {
                    v.expand().into_iter().map(move |x| {
                        let mut t = t.clone();
                        t.push(x);
                        t
                    })
                })
                .collect();
        }
        for t in &tuples {
            let e = groups.entry(t.clone()).or_default();
            e.0 += acc.runs;
            e.1.push(i);
        }
        raw_groups.push(tuples);
    }
    let rankable = |g: &Vec<KeyVal>| !g.contains(&KeyVal::Unindexed) && !(single && g[0] == KeyVal::None);
    let mut ranked: Vec<_> = groups.iter().filter(|(g, _)| rankable(g)).collect();
    ranked.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(b.0)));
    let shown: Vec<&Vec<KeyVal>> = ranked.iter().take(top).map(|(g, _)| *g).collect();
    let none_row: Option<&Vec<KeyVal>> = single
        .then(|| groups.get_key_value(&vec![KeyVal::None]).map(|(k, _)| k))
        .flatten();
    let merged = |idx: &[usize]| {
        let mut acc = Acc::new(keep);
        for &i in idx {
            acc.merge(&raws[i].1);
        }
        acc.finish(metrics)
    };
    let mut out: Vec<Group> = shown
        .iter()
        .chain(none_row.iter())
        .map(|g| Group {
            key: g.iter().map(KeyVal::json).collect(),
            cells: merged(&groups[*g].1),
            where_: Some(print(&and_all(
                keys.iter().zip(g.iter()).map(|(k, v)| where_one(k, v)).collect(),
            ))),
        })
        .collect();
    let in_top: HashSet<&Vec<KeyVal>> = shown.iter().copied().chain(none_row).collect();
    let other: Vec<usize> = (0..raws.len())
        .filter(|&i| raw_groups[i].iter().any(|g| !in_top.contains(g)))
        .collect();
    let cut = !other.is_empty();
    if cut {
        out.push(Group {
            key: vec![OTHER.into(); keys.len()],
            cells: merged(&other),
            where_: where_other(keys, &shown),
        });
    }
    (totals, out, cut)
}

fn and_all(mut xs: Vec<Expr>) -> Expr {
    if xs.len() == 1 {
        xs.pop().expect("one")
    } else {
        Expr::And(xs)
    }
}

fn key_path(k: &GroupKey) -> Path {
    match k {
        GroupKey::Name => Path::Field(Field::Name),
        GroupKey::RunType => Path::Field(Field::RunType),
        GroupKey::Model => Path::Field(Field::Model),
        GroupKey::Status => Path::Field(Field::Status),
        GroupKey::Tag => Path::Field(Field::Tags),
        GroupKey::Metadata(m) => Path::Attr {
            root: AttrRoot::Metadata,
            key: m.clone(),
        },
        GroupKey::FeedbackKey => Path::Feedback(String::new()),
    }
}

fn where_one(k: &GroupKey, v: &KeyVal) -> Expr {
    let path = key_path(k);
    match (k, v.value()) {
        (GroupKey::Tag, Some(value)) => Expr::Has { path, value },
        (_, value) => Expr::Cmp {
            op: CmpOp::Eq,
            path,
            value: value.unwrap_or(Value::Null),
        },
    }
}

/// `(other)` as a filter. One key: has the key, and none of the shown values. Two: in none of the shown
/// groups. A run with several tags can be in a shown group and in `(other)`, which no filter can say.
fn where_other(keys: &[GroupKey], shown: &[&Vec<KeyVal>]) -> Option<String> {
    if keys.contains(&GroupKey::Tag) {
        return None;
    }
    if let [k] = keys {
        let exists = Expr::Exists { path: key_path(k) };
        let values: Vec<Value> = shown.iter().filter_map(|g| g[0].value()).collect();
        if values.is_empty() {
            return Some(print(&exists));
        }
        let shown = Expr::In {
            path: key_path(k),
            values,
        };
        return Some(print(&Expr::And(vec![exists, Expr::Not(Box::new(shown))])));
    }
    if shown.is_empty() {
        return None;
    }
    let any: Vec<Expr> = shown
        .iter()
        .map(|g| and_all(keys.iter().zip(g.iter()).map(|(k, v)| where_one(k, v)).collect()))
        .collect();
    Some(print(&Expr::Not(Box::new(Expr::Or(any)))))
}
