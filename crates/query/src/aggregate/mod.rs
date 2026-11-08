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
pub mod progressive;
pub mod series;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use serde::{Serialize, Serializer};
use smithling_ingest::{FeedbackLookup, Feedbacks};
use smithling_model::agg::{AggSpec, Compare, GroupKey, Metric, SumOf, View};
use smithling_model::attrs::{Attrs, Scalar};
use smithling_model::cost::{self, Prices};
use smithling_model::filter::{
    AttrRoot, CmpOp, EvalStats, Expr, Field, Path, Row, TimeAt, Value, eval_with, print, status_name,
};
use smithling_model::run::run_type_str;
use smithling_model::{Run, RunId, TraceId, Ts};
use smithling_storage::segment::{SegmentMeta, start_key};

pub use accum::{Acc, Cells, Facts, Keep};
pub use progressive::{AggMsg, AggStream, Cancel, StreamOpts};
pub use series::{Dist, Series, SeriesBucket};

use crate::scan::{self, Scope};
use crate::{Query, QueryError, QuerySpec, QueryWarning, TimeRange};

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AggResult {
    pub query_id: String,
    /// An estimate from a sample; the exact result follows.
    pub partial: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampled_pct: Option<f32>,
    /// Every run counted once (tag groups overlap; this doesn't).
    pub totals: Cells,
    pub groups: Vec<Group>,
    /// Top N applied: some runs are in `(other)`.
    pub cut: bool,
    /// Groups overlap (grouped by tag).
    pub overlap: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series: Option<Series>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dist: Option<Dist>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compare: Option<Compared>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<QueryWarning>,
    pub stats: AggStats,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Compared {
    /// Side a's groups, in its order, with side b's cells (`null` where b has no such runs), then b's `(other)`.
    pub side_b: Box<AggResult>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Group {
    /// One value per group key: a value, `"(none)"`, or `"(other)"`.
    pub key: Vec<serde_json::Value>,
    /// `runs == 0` only on side b of a comparison, for a group b has no runs in; written `null`.
    #[serde(serialize_with = "cells_or_null")]
    pub cells: Cells,
    /// Filter text selecting exactly this group's runs (added to the query's filter); `null` for a tag
    /// group's `(other)`, which no filter can say.
    #[serde(rename = "where")]
    pub where_: Option<String>,
}

fn cells_or_null<S: Serializer>(c: &Cells, s: S) -> Result<S::Ok, S::Error> {
    if c.runs == 0 {
        s.serialize_none()
    } else {
        c.serialize(s)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct AggStats {
    pub files_scanned: usize,
    /// Runs counted from single-version file rows.
    pub clean_rows: usize,
    /// Runs counted after a merge or from a memtable (`clean_rows + dirty_runs` = `totals.runs`, unless
    /// `partial`: an estimate's totals are scaled, these aren't).
    pub dirty_runs: usize,
    pub ms: u64,
}

impl AggStats {
    fn add(&mut self, o: &AggStats) {
        self.files_scanned += o.files_scanned;
        self.clean_rows += o.clean_rows;
        self.dirty_runs += o.dirty_runs;
    }
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

/// `feedback.<key>` prints and parses back as itself (keys are stored as sent, e.g. with spaces).
pub(crate) fn filterable_feedback_key(key: &str) -> bool {
    matches!(
        smithling_model::filter::parse_filter(&format!("exists(feedback.{key})")),
        Ok(Expr::Exists { path: Path::Feedback(k) }) if k == key
    )
}

/// The feedback keys `run` has a score for, as one value (unnested at finish like tags).
pub(crate) fn feedback_keys(fb: &Feedbacks, run: RunId) -> KeyVal {
    let scores = fb.scores(&run);
    KeyVal::of_tags(scores.iter().filter(|(_, s)| s.is_some()).map(|(k, _)| k.as_str()))
}

/// Key values of a merged (or decoded) run.
pub(crate) fn keys_of(keys: &[GroupKey], run: &Run, attrs: &Attrs, fb: &Feedbacks) -> Vec<KeyVal> {
    keys.iter()
        .map(|k| match k {
            GroupKey::Name => KeyVal::of_str(run.name.as_deref()),
            GroupKey::Model => KeyVal::of_str(run.model.as_deref()),
            GroupKey::Status => KeyVal::of_str(Some(status_name(run.status))),
            GroupKey::RunType => KeyVal::of_str(Some(run_type_text(run))),
            GroupKey::Tag => KeyVal::of_tags(run.tags.iter().map(String::as_str)),
            GroupKey::Metadata(m) => KeyVal::of_scalar(attrs.get(&format!("metadata.{m}"))),
            GroupKey::FeedbackKey => feedback_keys(fb, run.run_id),
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
    pub traces: Option<Arc<HashSet<TraceId>>>,
    pub prices: &'a Prices,
    pub now: Ts,
    pub tokens: bool,
    pub cost: bool,
    /// Runs are bucketed by `start_time`.
    pub series: bool,
    pub feedback: Feedbacks,
    /// The key of `avg(feedback.<key>)`, if asked for.
    pub feedback_metric: Option<&'a str>,
}

impl Plan<'_> {
    /// The conditions DataFusion can't check for a single-version row: project, trace lists and trace conditions.
    pub(crate) fn row_ok(&self, project: Option<&str>, trace: TraceId) -> bool {
        self.scope
            .project
            .as_ref()
            .is_none_or(|p| project.unwrap_or(smithling_model::DEFAULT_PROJECT) == p)
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
            && self.scope.has_run(run.run_id)
            && self.is_root.is_none_or(|r| run.is_root == r)
            && self.filter.is_none_or(|f| self.eval(f, run, attrs, stats))
    }

    /// `f` on `run`, `feedback.<key>` read from the feedback index.
    pub(crate) fn eval(&self, f: &Expr, run: &Run, attrs: &Attrs, stats: &mut EvalStats) -> bool {
        let fb = |k: &str| self.feedback.mean(&run.run_id, k);
        eval_with(f, &Row { run, attrs }, &fb, self.now, stats)
    }

    /// The run's mean score for the `avg(feedback.<key>)` key, if it has one.
    pub(crate) fn feedback_of(&self, run: RunId) -> Vec<(String, f64)> {
        self.feedback_metric
            .and_then(|k| Some((k.to_string(), self.feedback.mean(&run, k)?)))
            .into_iter()
            .collect()
    }

    pub(crate) fn facts(&self, run: &Run) -> Facts {
        Facts {
            status: run.status,
            latency_ms: run.latency_ms.map(|v| v as i64),
            input_tokens: run.input_tokens,
            output_tokens: run.output_tokens,
            total_tokens: run.total_tokens,
            cost: if self.cost { cost::of(run, self.prices) } else { None },
            feedback: self.feedback_of(run.run_id),
            start_us: run.start_time.map(|t| t.0),
        }
    }
}

/// Accumulators by raw key (and per minute, or per latency bin, for the chart views), sharing one
/// latency-value cap.
#[derive(Clone)]
pub(crate) struct Accs {
    pub by_key: HashMap<Vec<KeyVal>, Acc>,
    view: View,
    /// Raw key → minute of `start_time` → runs, for a time series.
    slots: HashMap<Vec<KeyVal>, HashMap<i64, Acc>>,
    /// Raw key → finished runs per latency bin, for a distribution.
    bins: HashMap<Vec<KeyVal>, Box<[u64; accum::BINS]>>,
    /// First and last `start_time` counted (time series only), microseconds.
    first: Option<i64>,
    last: Option<i64>,
    /// Some percentile was asked for.
    latency: bool,
    values: usize,
    cap: usize,
}

impl Accs {
    fn new(latency: bool, cap: usize, view: View) -> Accs {
        Accs {
            by_key: HashMap::new(),
            view,
            slots: HashMap::new(),
            bins: HashMap::new(),
            first: None,
            last: None,
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

    fn kept(&mut self, n: usize) {
        self.values += n;
        if self.values > self.cap && n > 0 {
            self.by_key.values_mut().for_each(Acc::drop_values);
            self.slots
                .values_mut()
                .flat_map(|m| m.values_mut())
                .for_each(Acc::drop_values);
        }
    }

    pub(crate) fn add(&mut self, key: Vec<KeyVal>, f: &Facts) {
        let keep = self.keep();
        match self.view {
            View::Table => {}
            View::Series => {
                if let Some(t) = f.start_us {
                    self.first = Some(self.first.map_or(t, |x| x.min(t)));
                    self.last = Some(self.last.map_or(t, |x| x.max(t)));
                    if !self.slots.contains_key(&key) {
                        self.slots.insert(key.clone(), HashMap::new());
                    }
                    let per = self.slots.get_mut(&key).expect("just added");
                    per.entry(t.div_euclid(series::MINUTE_US))
                        .or_insert_with(|| Acc::new(keep))
                        .add(f);
                }
            }
            View::Dist => {
                if let Some(ms) = f.latency_ms.filter(|_| f.status != smithling_model::Status::Running) {
                    if !self.bins.contains_key(&key) {
                        self.bins.insert(key.clone(), Box::new([0; accum::BINS]));
                    }
                    self.bins.get_mut(&key).expect("just added")[accum::bin_of(ms)] += 1;
                }
            }
        }
        // The cap counts each run's latency once; a series keeps it per minute too.
        let kept = self.by_key.entry(key).or_insert_with(|| Acc::new(keep)).add(f);
        self.kept(usize::from(kept));
    }

    fn merge(&mut self, o: Accs) {
        let keep = self.keep();
        for (k, a) in o.by_key {
            self.by_key.entry(k).or_insert_with(|| Acc::new(keep)).merge(&a);
        }
        for (k, per) in o.slots {
            let mine = self.slots.entry(k).or_default();
            for (m, a) in per {
                mine.entry(m).or_insert_with(|| Acc::new(keep)).merge(&a);
            }
        }
        for (k, b) in o.bins {
            let mine = self.bins.entry(k).or_insert_with(|| Box::new([0; accum::BINS]));
            for (m, c) in mine.iter_mut().zip(b.iter()) {
                *m += c;
            }
        }
        self.first = self.first.into_iter().chain(o.first).min();
        self.last = self.last.into_iter().chain(o.last).max();
        self.kept(o.values);
    }
}

/// Dirty runs taken by a pass: all, or a share by run-id hash (a sample, then the rest).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Pick {
    All,
    Below(u64),
    From(u64),
}

impl Pick {
    fn hash(id: &RunId) -> u64 {
        xxhash_rust::xxh64::xxh64(id.as_bytes(), 0x5a3d)
    }

    /// `share` of the runs, and the rest.
    pub(crate) fn split(share: f64) -> (Pick, Pick) {
        if share >= 1.0 {
            return (Pick::All, Pick::Below(0));
        }
        let t = (share.max(0.0) * u64::MAX as f64) as u64;
        (Pick::Below(t), Pick::From(t))
    }

    pub(crate) fn takes(self, id: &RunId) -> bool {
        match self {
            Pick::All => true,
            Pick::Below(t) => Pick::hash(id) < t,
            Pick::From(t) => Pick::hash(id) >= t,
        }
    }
}

/// Where a side's chart axis starts: UTC time (alone), its window start (a shift), or its first run (two
/// filters).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Origin {
    Time,
    Window,
    FirstRun,
}

/// One pin of everything a request reads: data files, memtable runs, the multi-version set and (unless cold)
/// their merged file rows.
pub(crate) struct Snap {
    pub pin: dirty::Pin,
    pub merged: Option<Arc<crate::fields::Rows>>,
    /// Ids the clean side leaves to the dirty side.
    pub skip: HashSet<RunId>,
}

impl Snap {
    fn new(pin: dirty::Pin, merged: Option<Arc<crate::fields::Rows>>) -> Snap {
        let skip = pin
            .multi
            .iter()
            .copied()
            .chain(pin.pinned.rows.iter().map(|r| r.1.run.run_id))
            .collect();
        Snap { pin, merged, skip }
    }
}

/// One side of a request: its plan and the data files in its scope (by start time).
pub(crate) struct Side<'a> {
    pub plan: Plan<'a>,
    pub files: Vec<Arc<SegmentMeta>>,
    pub origin: Origin,
    pub latency: bool,
}

impl Side<'_> {
    pub(crate) fn files(&self) -> Vec<&SegmentMeta> {
        self.files.iter().map(|s| &**s).collect()
    }

    /// Single-version rows in scope, by the catalog.
    pub(crate) fn clean_rows(files: &[&SegmentMeta]) -> u64 {
        files
            .iter()
            .map(|s| s.row_count.saturating_sub(s.needs_merge_rows))
            .sum()
    }
}

/// What one pass over (some of) a side's data added up.
#[derive(Clone)]
pub(crate) struct Partial {
    pub accs: Accs,
    pub stats: AggStats,
    pub eval: EvalStats,
    /// Data files read.
    pub done: HashSet<String>,
}

impl Partial {
    fn new(side: &Side, cap: usize, view: View) -> Partial {
        Partial {
            accs: Accs::new(side.latency, cap, view),
            stats: AggStats::default(),
            eval: EvalStats::default(),
            done: HashSet::new(),
        }
    }

    pub(crate) fn merge(&mut self, o: Partial) {
        self.accs.merge(o.accs);
        self.stats.add(&o.stats);
        add_eval(&mut self.eval, o.eval);
        self.done.extend(o.done);
    }
}

/// How to write up a finished computation.
pub(crate) struct Writeup {
    pub query_id: String,
    pub started: Instant,
    /// Per side, the scale of an estimate and its sampled share in percent.
    pub estimate: Vec<Option<(f64, f32)>>,
}

impl Query {
    /// Latency values kept per query before percentiles come from the histogram (tests use small caps).
    pub fn with_value_cap(mut self, cap: usize) -> Query {
        self.value_cap = cap;
        self
    }

    /// The exact aggregate of the runs `q` selects (its order, limit, cursor and columns are ignored).
    pub async fn aggregate(&self, q: &QuerySpec, a: &AggSpec, prices: &Prices) -> Result<AggResult, QueryError> {
        let cancel = Cancel::default();
        let now = Ts::now();
        let id = query_id(q, now);
        Ok(self
            .aggregate_with(q, a, prices, now, id, &cancel)
            .await?
            .expect("never cancelled"))
    }

    /// The exact aggregate; `None` once `cancel` is set.
    pub(crate) async fn aggregate_with(
        &self,
        q: &QuerySpec,
        a: &AggSpec,
        prices: &Prices,
        now: Ts,
        query_id: String,
        cancel: &Cancel,
    ) -> Result<Option<AggResult>, QueryError> {
        let started = Instant::now();
        let specs = self.with_searches(side_specs(q, a, now)).await?;
        let snap = self.snap(&self.pin_scope(q)).await?;
        self.exact_over(&snap, q, &specs, a, prices, now, query_id, started, cancel)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn exact_over(
        &self,
        snap: &Snap,
        q: &QuerySpec,
        specs: &[(QuerySpec, Origin)],
        a: &AggSpec,
        prices: &Prices,
        now: Ts,
        query_id: String,
        started: Instant,
        cancel: &Cancel,
    ) -> Result<Option<AggResult>, QueryError> {
        let mut eval_stats = EvalStats::default();
        let traces = self.trace_set(q, snap, now, &mut eval_stats).await?;
        let sides = self.sides(snap, specs, a, prices, now, traces);
        let parts = futures_util::future::try_join_all(sides.iter().map(|side| async {
            self.pass(snap, side, &side.files(), Pick::All, a.view, cancel, None)
                .await
        }))
        .await?;
        if cancel.is_cancelled() {
            return Ok(None);
        }
        let n = sides.len();
        Ok(Some(self.write_up(
            &sides,
            &parts.iter().collect::<Vec<_>>(),
            a,
            eval_stats,
            Writeup {
                query_id,
                started,
                estimate: vec![None; n],
            },
        )))
    }

    /// Project and trace list: what the pin needs (both sides share them).
    pub(crate) fn pin_scope(&self, q: &QuerySpec) -> Scope {
        let scope = self.scope(q, Ts::now());
        Scope {
            project: scope.project,
            traces: scope.traces,
            ..Scope::default()
        }
    }

    /// The project's memtable runs (the walk checks the rest of the scope) and every version of the
    /// multi-version runs, pinned with the set under one lock.
    pub(crate) async fn snap(&self, tscope: &Scope) -> Result<Snap, QueryError> {
        let mut cache = self.version_rows.lock().await;
        let (pinned, frozen, multi) = scan::pin_with(&self.ingest, tscope, true);
        cache
            .update(self, &pinned.version.segments, frozen.as_ref(), &multi)
            .await?;
        Ok(Snap::new(dirty::Pin { pinned, multi }, Some(cache.rows.clone())))
    }

    /// A pin without the merged file rows of multi-version runs (not waiting for them to be read).
    pub(crate) fn snap_cold(&self, tscope: &Scope) -> Snap {
        let (pinned, _, multi) = scan::pin_with(&self.ingest, tscope, true);
        Snap::new(dirty::Pin { pinned, multi }, None)
    }

    /// Reads the merged file rows of multi-version runs in the background, so the first aggregate after a
    /// start doesn't wait for them.
    pub fn warm_aggregates(&self) {
        let this = self.clone();
        tokio::spawn(async move {
            if let Err(e) = this.snap(&Scope::default()).await {
                tracing::warn!("warming aggregates: {e}");
            }
        });
    }

    pub(crate) fn sides<'a>(
        &self,
        snap: &Snap,
        specs: &'a [(QuerySpec, Origin)],
        a: &'a AggSpec,
        prices: &'a Prices,
        now: Ts,
        traces: Option<Arc<HashSet<TraceId>>>,
    ) -> Vec<Side<'a>> {
        let has = |f: &dyn Fn(&Metric) -> bool| a.metrics.iter().any(f);
        specs
            .iter()
            .map(|(q, origin)| {
                let scope = self.scope(q, now);
                let mut files: Vec<Arc<SegmentMeta>> = snap
                    .pin
                    .pinned
                    .version
                    .segments
                    .iter()
                    .filter(|s| scope.file_in_scope(s) && !self.ingest.is_damaged(&s.id))
                    .cloned()
                    .collect();
                files.sort_by(|x, y| (x.min_start_key, &x.id).cmp(&(y.min_start_key, &y.id)));
                Side {
                    plan: Plan {
                        keys: &a.group_by,
                        scope,
                        filter: q.filter.as_ref(),
                        is_root: q.is_root,
                        traces: traces.clone(),
                        prices,
                        now,
                        tokens: has(&|m| matches!(m, Metric::Sum(s) if *s != SumOf::Cost)),
                        cost: has(&|m| *m == Metric::Sum(SumOf::Cost)),
                        series: a.view == View::Series,
                        feedback: self.feedback.clone(),
                        feedback_metric: a.metrics.iter().find_map(|m| match m {
                            Metric::AvgFeedback(k) => Some(k.as_str()),
                            _ => None,
                        }),
                    },
                    files,
                    origin: *origin,
                    latency: has(&|m| matches!(m, Metric::Latency(_))),
                }
            })
            .collect()
    }

    /// The clean side over `files`, then the dirty runs `pick` takes.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn pass(
        &self,
        snap: &Snap,
        side: &Side<'_>,
        files: &[&SegmentMeta],
        pick: Pick,
        view: View,
        cancel: &Cancel,
        deadline: Option<Instant>,
    ) -> Result<Partial, QueryError> {
        let mut p = self.clean_pass(snap, side, files, view, cancel, deadline).await?;
        self.dirty_pass(snap, side, pick, &mut p, cancel);
        Ok(p)
    }

    pub(crate) async fn clean_pass(
        &self,
        snap: &Snap,
        side: &Side<'_>,
        files: &[&SegmentMeta],
        view: View,
        cancel: &Cancel,
        deadline: Option<Instant>,
    ) -> Result<Partial, QueryError> {
        let mut p = Partial::new(side, self.value_cap, view);
        let done = clean::scan(
            self,
            files,
            &snap.skip,
            &side.plan,
            &mut p.accs,
            &mut p.stats,
            &mut p.eval,
            cancel,
            deadline,
        )
        .await?;
        p.done = done.into_iter().collect();
        Ok(p)
    }

    pub(crate) fn dirty_pass(&self, snap: &Snap, side: &Side<'_>, pick: Pick, p: &mut Partial, cancel: &Cancel) {
        if cancel.is_cancelled() {
            return;
        }
        dirty::walk(
            &snap.pin,
            snap.merged.as_deref().map(|r| &r.merged),
            &side.plan,
            &mut p.accs,
            &mut p.stats,
            &mut p.eval,
            pick,
            cancel,
        );
    }

    /// Groups, charts and the comparison from each side's accumulators.
    pub(crate) fn write_up(
        &self,
        sides: &[Side],
        parts: &[&Partial],
        a: &AggSpec,
        mut eval_stats: EvalStats,
        w: Writeup,
    ) -> AggResult {
        let latency = sides.first().is_some_and(|s| s.latency);
        let keep = if latency { Keep::Values } else { Keep::Off };
        let mut finished: Vec<Finished> = Vec::new();
        for p in parts {
            let forced = finished.first().map(|f| f.shown.as_slice());
            finished.push(finish(
                &p.accs.by_key,
                keep,
                &a.group_by,
                &a.metrics,
                a.top as usize,
                forced,
            ));
        }
        let groups = |f: &Finished| if a.group_by.is_empty() { 1 } else { f.groups.len() };
        let mut charts: Vec<(Option<Series>, Option<Dist>)> = vec![(None, None); parts.len()];
        match a.view {
            View::Table => {}
            View::Series => {
                let spans: Vec<_> = sides
                    .iter()
                    .zip(parts)
                    .map(|(s, p)| {
                        let from = match s.origin {
                            Origin::FirstRun => None,
                            Origin::Time | Origin::Window => s.plan.scope.from,
                        };
                        series::span(&p.accs, from, s.plan.scope.to)
                    })
                    .collect();
                let axes = series::axes(a.bucket, &spans, sides.len() > 1);
                for (i, p) in parts.iter().enumerate() {
                    let step = series::step_min(a.bucket, 0);
                    let s = match axes[i] {
                        Some(axis) => series::series(
                            &p.accs.slots,
                            &finished[i].members,
                            groups(&finished[i]),
                            axis,
                            &a.metrics,
                            keep,
                            w.estimate[i].map(|e| e.0),
                        ),
                        None => Series {
                            step_ms: axes.iter().flatten().next().map_or(step, |x| x.step) * 60_000,
                            offset: sides.len() > 1,
                            buckets: vec![],
                        },
                    };
                    charts[i].0 = Some(s);
                }
            }
            View::Dist => {
                let counts: Vec<_> = parts
                    .iter()
                    .zip(&finished)
                    .map(|(p, f)| series::bin_counts(&p.accs.bins, &f.members, groups(f)))
                    .collect();
                let refs: Vec<&[[u64; accum::BINS]]> = counts.iter().map(Vec::as_slice).collect();
                let range = series::bin_range(&refs);
                for (i, c) in counts.iter().enumerate() {
                    charts[i].1 = Some(series::dist(c, range, w.estimate[i].map(|e| e.0)));
                }
            }
        }
        let mut stats: Vec<AggStats> = parts.iter().map(|p| p.stats.clone()).collect();
        for p in parts {
            add_eval(&mut eval_stats, p.eval.clone());
        }
        let mut warnings = crate::eval_warnings(eval_stats);
        let skipped: Vec<String> = {
            let mut ids: Vec<String> = sides.iter().flat_map(|s| self.skipped(&s.plan.scope)).collect();
            ids.sort();
            ids.dedup();
            ids
        };
        if !skipped.is_empty() {
            warnings.push(QueryWarning::SkippedFiles { ids: skipped });
        }
        let ms = w.started.elapsed().as_millis() as u64;
        let mut results: Vec<AggResult> = finished
            .into_iter()
            .zip(charts)
            .zip(stats.iter_mut())
            .zip(&w.estimate)
            .map(|(((f, (series, dist)), st), est)| {
                st.ms = ms;
                let scale = |c: Cells| est.map_or(c.clone(), |e| c.scaled(e.0));
                AggResult {
                    query_id: w.query_id.clone(),
                    partial: est.is_some(),
                    sampled_pct: est.map(|e| e.1),
                    totals: scale(f.totals),
                    groups: f
                        .groups
                        .into_iter()
                        .map(|g| Group {
                            cells: if g.cells.runs == 0 { g.cells } else { scale(g.cells) },
                            ..g
                        })
                        .collect(),
                    cut: f.cut,
                    overlap: a.group_by.iter().any(overlaps),
                    series,
                    dist,
                    compare: None,
                    warnings: vec![],
                    stats: st.clone(),
                }
            })
            .collect();
        let mut first = results.remove(0);
        first.warnings = warnings;
        first.compare = results.pop().map(|b| Compared { side_b: Box::new(b) });
        first
    }

    /// Traces whose root matches `trace_filter` and that hold a run matching `tree_filter` (`None`: no such
    /// conditions). Not bounded in time: a root may start before the window, a step after it.
    pub(crate) async fn trace_set(
        &self,
        q: &QuerySpec,
        snap: &Snap,
        now: Ts,
        eval_stats: &mut EvalStats,
    ) -> Result<Option<Arc<HashSet<TraceId>>>, QueryError> {
        let tscope = self.pin_scope(q);
        let mut ok: Option<HashSet<TraceId>> = None;
        for (f, is_root) in [(&q.trace_filter, Some(true)), (&q.tree_filter, None)] {
            let Some(f) = f else { continue };
            let mut stats = scan::ScanStats::default();
            let found: HashSet<TraceId> = self
                .matching(&snap.pin.pinned, &tscope, Some(f), is_root, now, &mut stats, eval_stats)
                .await?
                .into_iter()
                .map(|m| m.state.run.trace_id)
                .filter(|t| ok.as_ref().is_none_or(|ok| ok.contains(t)))
                .collect();
            ok = Some(found);
        }
        Ok(ok.map(Arc::new))
    }
}

fn add_eval(into: &mut EvalStats, o: EvalStats) {
    for (path, counts) in o.mixed_types {
        let e = into.mixed_types.entry(path).or_default();
        for (ty, n) in counts {
            *e.entry(ty).or_default() += n;
        }
    }
    for (path, n) in o.not_indexed {
        *into.not_indexed.entry(path).or_default() += n;
    }
}

pub(crate) fn query_id(q: &QuerySpec, now: Ts) -> String {
    format!("q-{:016x}", xxhash_rust::xxh64::xxh64(&now.0.to_le_bytes(), q.hash()))
}

/// Side a, and side b of a comparison: the same query with the other filter, or its window shifted back.
pub(crate) fn side_specs(q: &QuerySpec, a: &AggSpec, now: Ts) -> Vec<(QuerySpec, Origin)> {
    match &a.compare {
        Compare::None => vec![(q.clone(), Origin::Time)],
        Compare::Filter(f) => vec![
            (q.clone(), Origin::FirstRun),
            (
                QuerySpec {
                    filter: Some(f.clone()),
                    ..q.clone()
                },
                Origin::FirstRun,
            ),
        ],
        Compare::Shift(s) => {
            let d = match TimeAt::parse(&format!("-{s}")) {
                Some(TimeAt::Rel(us)) => -us,
                _ => 0,
            };
            let back = |t: Ts| Some(TimeAt::Abs(Ts(t.0.saturating_sub(d))));
            let b = QuerySpec {
                time: TimeRange {
                    from: q.time.from.and_then(|t| back(t.resolve(now))),
                    to: back(q.time.to.map_or(now, |t| t.resolve(now))),
                },
                ..q.clone()
            };
            vec![(q.clone(), Origin::Window), (b, Origin::Window)]
        }
    }
}

/// The groups of one side, with each raw key's groups for the charts.
struct Finished {
    totals: Cells,
    groups: Vec<Group>,
    cut: bool,
    /// Groups shown before `(other)`, in order (side b of a comparison shows side a's).
    shown: Vec<Vec<KeyVal>>,
    members: series::Members,
}

/// Totals, then the shown groups: top N by runs (ties by key) and `(none)` (one key), or `forced`; then
/// `(other)`; and whether anything is in `(other)`.
fn finish(
    by_key: &HashMap<Vec<KeyVal>, Acc>,
    keep: Keep,
    keys: &[GroupKey],
    metrics: &[Metric],
    top: usize,
    forced: Option<&[Vec<KeyVal>]>,
) -> Finished {
    let raws: Vec<(&Vec<KeyVal>, &Acc)> = by_key.iter().collect();
    let mut totals = Acc::new(keep);
    for (_, a) in &raws {
        totals.merge(a);
    }
    let totals = totals.finish(metrics);
    if keys.is_empty() {
        return Finished {
            totals,
            groups: vec![],
            cut: false,
            shown: vec![],
            members: raws.iter().map(|(k, _)| ((*k).clone(), vec![0])).collect(),
        };
    }
    let single = keys.len() == 1;
    // Group → (runs, raw indexes); per raw, its groups.
    let mut groups: HashMap<Vec<KeyVal>, (u64, Vec<usize>)> = HashMap::new();
    let mut raw_groups: Vec<Vec<Vec<KeyVal>>> = Vec::with_capacity(raws.len());
    for (i, (raw, acc)) in raws.iter().enumerate() {
        let mut tuples: Vec<Vec<KeyVal>> = vec![vec![]];
        for v in raw.iter() {
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
    let (shown, ranked_n) = match forced {
        Some(f) => (f.to_vec(), f.len()),
        None => {
            let rankable = |g: &Vec<KeyVal>| !g.contains(&KeyVal::Unindexed) && !(single && g[0] == KeyVal::None);
            let mut ranked: Vec<_> = groups.iter().filter(|(g, _)| rankable(g)).collect();
            ranked.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(b.0)));
            let mut shown: Vec<Vec<KeyVal>> = ranked.iter().take(top).map(|(g, _)| (*g).clone()).collect();
            let n = shown.len();
            if single && groups.contains_key(&vec![KeyVal::None]) {
                shown.push(vec![KeyVal::None]);
            }
            (shown, n)
        }
    };
    let merged = |idx: &[usize]| {
        let mut acc = Acc::new(keep);
        for &i in idx {
            acc.merge(raws[i].1);
        }
        acc.finish(metrics)
    };
    let mut out: Vec<Group> = shown
        .iter()
        .map(|g| Group {
            key: g.iter().map(KeyVal::json).collect(),
            cells: groups.get(g).map_or_else(|| merged(&[]), |(_, idx)| merged(idx)),
            where_: where_group(keys, g).map(|e| print(&e)),
        })
        .collect();
    let index: HashMap<&Vec<KeyVal>, usize> = shown.iter().enumerate().map(|(i, g)| (g, i)).collect();
    let other: Vec<usize> = (0..raws.len())
        .filter(|&i| raw_groups[i].iter().any(|g| !index.contains_key(g)))
        .collect();
    let cut = !other.is_empty();
    let mut members: series::Members = HashMap::new();
    for (i, (raw, _)) in raws.iter().enumerate() {
        let mut gs: Vec<usize> = raw_groups[i].iter().filter_map(|g| index.get(g).copied()).collect();
        if cut && other.binary_search(&i).is_ok() {
            gs.push(out.len());
        }
        gs.sort();
        gs.dedup();
        members.insert((*raw).clone(), gs);
    }
    if cut {
        // Side b of a comparison can have `(none)` runs where side a had none to show.
        let none_too = single && groups.contains_key(&vec![KeyVal::None]) && !index.contains_key(&vec![KeyVal::None]);
        out.push(Group {
            key: vec![OTHER.into(); keys.len()],
            cells: merged(&other),
            where_: where_other(keys, &shown[..ranked_n], none_too),
        });
    }
    Finished {
        totals,
        groups: out,
        cut,
        shown,
        members,
    }
}

/// A run can be in several groups of this key.
fn overlaps(k: &GroupKey) -> bool {
    matches!(k, GroupKey::Tag | GroupKey::FeedbackKey)
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
        GroupKey::FeedbackKey => unreachable!("feedback_key groups are written as exists(feedback.<key>)"),
    }
}

/// `None` for a feedback key's `(none)` (no filter says "no feedback at all") or a key no filter can name.
fn where_one(k: &GroupKey, v: &KeyVal) -> Option<Expr> {
    if *k == GroupKey::FeedbackKey {
        return match v {
            KeyVal::Str(key) if filterable_feedback_key(key) => Some(Expr::Exists {
                path: Path::Feedback(key.clone()),
            }),
            _ => None,
        };
    }
    let path = key_path(k);
    Some(match (k, v.value()) {
        (GroupKey::Tag, Some(value)) => Expr::Has { path, value },
        (_, value) => Expr::Cmp {
            op: CmpOp::Eq,
            path,
            value: value.unwrap_or(Value::Null),
        },
    })
}

fn where_group(keys: &[GroupKey], g: &[KeyVal]) -> Option<Expr> {
    let parts: Option<Vec<Expr>> = keys.iter().zip(g.iter()).map(|(k, v)| where_one(k, v)).collect();
    parts.map(and_all)
}

/// `(other)` as a filter. One key: has the key, and none of the shown values. Two: in none of the shown
/// groups. A run with several tags can be in a shown group and in `(other)`, which no filter can say.
fn where_other(keys: &[GroupKey], shown: &[Vec<KeyVal>], none_too: bool) -> Option<String> {
    if keys.iter().any(overlaps) {
        return None;
    }
    if let [k] = keys {
        let exists = Expr::Exists { path: key_path(k) };
        let values: Vec<Value> = shown.iter().filter_map(|g| g[0].value()).collect();
        if values.is_empty() {
            return (!none_too).then(|| print(&exists));
        }
        let not_shown = Expr::Not(Box::new(Expr::In {
            path: key_path(k),
            values,
        }));
        return Some(print(&if none_too {
            not_shown
        } else {
            Expr::And(vec![exists, not_shown])
        }));
    }
    if shown.is_empty() {
        return None;
    }
    let any: Vec<Expr> = shown.iter().filter_map(|g| where_group(keys, g)).collect();
    Some(print(&Expr::Not(Box::new(Expr::Or(any)))))
}
