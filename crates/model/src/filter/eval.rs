use std::collections::BTreeMap;

use super::ast::{CmpOp, Expr, Field, Kind, Path, Value};
use crate::attrs::{Attrs, Scalar};
use crate::run::{Run, Status};
use crate::ts::Ts;

/// What a filter is evaluated against: the merged run's fields plus its promoted attributes.
pub struct Row<'a> {
    pub run: &'a Run,
    pub attrs: &'a Attrs,
}

/// Collected while evaluating, reported as query warnings.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EvalStats {
    /// path → (value type found, count) for comparisons skipped because the types differed.
    pub mixed_types: BTreeMap<String, BTreeMap<&'static str, u64>>,
    /// path → runs whose value exists but wasn't promoted (too long, an array, deeper than the limit).
    pub not_indexed: BTreeMap<String, u64>,
}

/// Evaluates `e` for one run. Comparisons only match values of the same type; a missing path matches only
/// `eq(path, null)`. `feedback.<key>` is missing on every run; see [`eval_with`].
pub fn eval(e: &Expr, row: &Row, now: Ts, stats: &mut EvalStats) -> bool {
    eval_with(e, row, &|_| None, now, stats)
}

/// [`eval`] with `feedback(key)` giving the run's mean score for `key` (`None`: no feedback with that key).
/// `search` isn't evaluated here yet and matches nothing; queries with it are rejected before they run.
pub fn eval_with(e: &Expr, row: &Row, feedback: &dyn Fn(&str) -> Option<f64>, now: Ts, stats: &mut EvalStats) -> bool {
    let ctx = Ctx { row, feedback, now };
    ctx.eval(e, stats)
}

struct Ctx<'a, 'r> {
    row: &'a Row<'r>,
    feedback: &'a dyn Fn(&str) -> Option<f64>,
    now: Ts,
}

impl Ctx<'_, '_> {
    fn eval(&self, e: &Expr, stats: &mut EvalStats) -> bool {
        match e {
            Expr::And(xs) => xs.iter().all(|x| self.eval(x, stats)),
            Expr::Or(xs) => xs.iter().any(|x| self.eval(x, stats)),
            Expr::Not(x) => !self.eval(x, stats),
            Expr::Has { path, value } => match (path, value) {
                (Path::Field(Field::Tags), Value::Str(v)) => self.row.run.tags.iter().any(|t| t == v),
                _ => false,
            },
            Expr::In { path, values } => values.iter().any(|v| self.cmp(CmpOp::Eq, path, v, stats)),
            Expr::Cmp { op, path, value } => self.cmp(*op, path, value, stats),
            Expr::Exists { path } => self.cmp(CmpOp::Neq, path, &Value::Null, stats),
            Expr::Search { .. } => false,
        }
    }

    fn cmp(&self, op: CmpOp, path: &Path, value: &Value, stats: &mut EvalStats) -> bool {
        let have = match path {
            Path::Feedback(key) => (self.feedback)(key).map_or(Found::Absent, Found::Num),
            _ => found(path, self.row),
        };
        cmp(op, path, value, have, self.row, self.now, stats)
    }
}

/// One side of a comparison, as found on the run.
#[derive(Debug)]
enum Found<'a> {
    Absent,
    /// Present but not promoted: it can't be compared.
    Unindexed,
    Str(&'a str),
    Num(f64),
    Bool(bool),
    Time(Ts),
}

fn found<'a>(path: &Path, row: &'a Row) -> Found<'a> {
    let r = row.run;
    let s = |o: &'a Option<String>| o.as_deref().map_or(Found::Absent, Found::Str);
    let n = |o: Option<u64>| o.map_or(Found::Absent, |v| Found::Num(v as f64));
    let t = |o: Option<Ts>| o.map_or(Found::Absent, Found::Time);
    match path {
        Path::Field(f) => match f {
            Field::Status => Found::Str(status_name(r.status)),
            Field::Name => s(&r.name),
            Field::RunType => Found::Str(r.run_type_raw.as_deref().unwrap_or(run_type_name(r))),
            Field::Model => s(&r.model),
            Field::ThreadId => s(&r.thread_id),
            Field::Project => Found::Str(&r.project),
            Field::Error => match r.error.as_deref() {
                Some(e) if !e.is_empty() => Found::Str(e),
                _ => Found::Absent,
            },
            Field::StartTime => t(r.start_time),
            Field::EndTime => t(r.end_time),
            Field::Latency => r.latency_ms.map_or(Found::Absent, |ms| Found::Num(ms as f64 / 1000.0)),
            Field::Tags => {
                if r.tags.is_empty() {
                    Found::Absent
                } else {
                    Found::Bool(true)
                }
            }
            Field::TotalTokens => n(r.total_tokens),
            Field::InputTokens => n(r.input_tokens),
            Field::OutputTokens => n(r.output_tokens),
            // Ids are compared by value in `cmp`; here they only need to exist.
            Field::TraceId | Field::RunId => Found::Bool(true),
            Field::ParentRunId => r.parent_run_id.map_or(Found::Absent, |_| Found::Bool(true)),
        },
        Path::Feedback(_) => Found::Absent,
        Path::Attr { .. } => match row.attrs.get(&path.text()) {
            None if !crate::attrs::promotable_path(&path.text()) => Found::Unindexed,
            None => Found::Absent,
            Some(Scalar::Str(v)) => Found::Str(v),
            Some(Scalar::Num(v)) => Found::Num(*v),
            Some(Scalar::Bool(v)) => Found::Bool(*v),
            Some(Scalar::Unindexed) => Found::Unindexed,
        },
    }
}

fn cmp(op: CmpOp, path: &Path, value: &Value, have: Found, row: &Row, now: Ts, stats: &mut EvalStats) -> bool {
    if *value == Value::Null {
        if matches!(have, Found::Unindexed) {
            // A promotable path marked unindexed exists; deeper paths are unknown, so neither test matches.
            if crate::attrs::promotable_path(&path.text()) {
                return op == CmpOp::Neq;
            }
            *stats.not_indexed.entry(path.text()).or_default() += 1;
            return false;
        }
        let present = !matches!(have, Found::Absent);
        return match op {
            CmpOp::Eq => !present,
            CmpOp::Neq => present,
            _ => false,
        };
    }
    if let (Path::Field(f @ (Field::TraceId | Field::RunId | Field::ParentRunId)), Value::Str(want)) = (path, value) {
        let Ok(want) = uuid::Uuid::try_parse(want) else {
            return false;
        };
        let got = match f {
            Field::TraceId => Some(row.run.trace_id),
            Field::RunId => Some(row.run.run_id),
            _ => row.run.parent_run_id,
        };
        return match (op, got) {
            (CmpOp::Eq, Some(g)) => g == want,
            (CmpOp::Neq, Some(g)) => g != want,
            _ => false,
        };
    }
    let ord = match (&have, value) {
        (Found::Absent, _) => return false,
        (Found::Unindexed, _) => {
            *stats.not_indexed.entry(path.text()).or_default() += 1;
            return false;
        }
        (Found::Str(a), Value::Str(b)) => (*a).cmp(b.as_str()),
        (Found::Num(a), Value::Num(b)) => match a.partial_cmp(b) {
            Some(o) => o,
            None => return false,
        },
        (Found::Bool(a), Value::Bool(b)) => a.cmp(b),
        (Found::Time(a), Value::Time { at, .. }) => a.cmp(&at.resolve(now)),
        (got, _) => {
            if path.kind() == Kind::Any {
                let ty = match got {
                    Found::Str(_) => "string",
                    Found::Num(_) => "number",
                    Found::Bool(_) => "bool",
                    _ => "other",
                };
                *stats.mixed_types.entry(path.text()).or_default().entry(ty).or_default() += 1;
            }
            return false;
        }
    };
    use std::cmp::Ordering::*;
    match op {
        CmpOp::Eq => ord == Equal,
        CmpOp::Neq => ord != Equal,
        CmpOp::Gt => ord == Greater,
        CmpOp::Gte => ord != Less,
        CmpOp::Lt => ord == Less,
        CmpOp::Lte => ord != Greater,
    }
}

pub fn status_name(s: Status) -> &'static str {
    match s {
        Status::Running => "running",
        Status::Ok => "ok",
        Status::Error => "error",
    }
}

fn run_type_name(r: &Run) -> &'static str {
    use crate::run::RunType::*;
    match r.run_type {
        Llm => "llm",
        Chain => "chain",
        Tool => "tool",
        Retriever => "retriever",
        Embedding => "embedding",
        Prompt => "prompt",
        Parser => "parser",
        Agent => "agent",
        Other => "other",
    }
}

/// Paths in `e` that can't be answered from promoted attributes (too deep).
pub fn unindexed_paths(e: &Expr) -> Vec<String> {
    let mut out = Vec::new();
    visit_paths(e, &mut |p| {
        if let Path::Attr { root, key } = p
            && !crate::attrs::promotable(root.name(), key)
        {
            out.push(p.text());
        }
    });
    out.sort();
    out.dedup();
    out
}

pub fn visit_paths(e: &Expr, f: &mut impl FnMut(&Path)) {
    match e {
        Expr::And(xs) | Expr::Or(xs) => xs.iter().for_each(|x| visit_paths(x, f)),
        Expr::Not(x) => visit_paths(x, f),
        Expr::Cmp { path, .. } | Expr::Has { path, .. } | Expr::In { path, .. } | Expr::Exists { path } => f(path),
        Expr::Search { .. } => {}
    }
}

/// Whether `e` has a `search(…)` anywhere.
pub fn contains_search(e: &Expr) -> bool {
    match e {
        Expr::And(xs) | Expr::Or(xs) => xs.iter().any(contains_search),
        Expr::Not(x) => contains_search(x),
        Expr::Search { .. } => true,
        Expr::Cmp { .. } | Expr::Has { .. } | Expr::In { .. } | Expr::Exists { .. } => false,
    }
}
