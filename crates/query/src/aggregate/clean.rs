//! Clean side: single-version file rows (`needs_merge = false`, run neither multi-version nor in a memtable),
//! read with only the columns the request needs. An exact pushdown accumulates straight from the columns;
//! otherwise the filter is evaluated on a run holding only the fields and attributes it reads.

use std::collections::HashSet;

use datafusion::arrow::array::{Array, FixedSizeBinaryArray, Int64Array, ListArray, StringArray, StringViewArray};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::logical_expr::{Expr as DfExpr, col, lit};
use datafusion::prelude::ParquetReadOptions;
use futures_util::StreamExt;
use serde_json::Value;
use tracedb_model::agg::GroupKey;
use tracedb_model::attrs::{Attrs, Scalar};
use tracedb_model::cost;
use tracedb_model::filter::eval::visit_paths;
use tracedb_model::filter::{EvalStats, Expr, Field, Path, Row, eval};
use tracedb_model::run::{JsonMap, RunType};
use tracedb_model::{DEFAULT_PROJECT, Op, Run, RunEvent, RunFields, RunId, Source, Status, Ts};
use tracedb_storage::StorageError;
use tracedb_storage::segment::SegmentMeta;

use super::{Accs, AggStats, Facts, KeyVal, Plan};
use crate::scan::{Pinned, damaged_alone};
use crate::{Query, QueryError, pushdown};

/// Files read at once.
const FILES: usize = 16;

/// Core columns this side may read, by index.
const NAMES: [&str; 21] = [
    "run_id",
    "trace_id",
    "parent_run_id",
    "project",
    "status",
    "latency_ms",
    "input_tokens",
    "output_tokens",
    "total_tokens",
    "run_type",
    "model",
    "outputs_usage",
    "metadata",
    "extra",
    "name",
    "tags",
    "attrs",
    "thread_id",
    "error",
    "start_time",
    "end_time",
];
const RUN_ID: usize = 0;
const TRACE_ID: usize = 1;
const PARENT_RUN_ID: usize = 2;
const PROJECT: usize = 3;
const STATUS: usize = 4;
const LATENCY_MS: usize = 5;
const INPUT_TOKENS: usize = 6;
const OUTPUT_TOKENS: usize = 7;
const TOTAL_TOKENS: usize = 8;
const RUN_TYPE: usize = 9;
const MODEL: usize = 10;
const OUTPUTS_USAGE: usize = 11;
const METADATA: usize = 12;
const EXTRA: usize = 13;
const NAME: usize = 14;
const TAGS: usize = 15;
const ATTRS: usize = 16;
const THREAD_ID: usize = 17;
const ERROR: usize = 18;
const START_TIME: usize = 19;
const END_TIME: usize = 20;

pub(crate) async fn scan(
    q: &Query,
    pinned: &Pinned,
    skip: &HashSet<RunId>,
    plan: &Plan<'_>,
    accs: &mut Accs,
    stats: &mut AggStats,
    eval_stats: &mut EvalStats,
) -> Result<(), QueryError> {
    let pushed = plan.filter.map(|f| pushdown::compile(f, plan.now));
    let exact = match &pushed {
        None => true,
        Some(p) => p.as_ref().is_some_and(|p| p.exact),
    };
    let mut filter = col("needs_merge").eq(lit(false));
    for f in [
        plan.scope.df_filter(),
        pushed.flatten().map(|p| p.expr),
        plan.is_root.map(|r| col("is_root").eq(lit(r))),
    ]
    .into_iter()
    .flatten()
    {
        filter = filter.and(f);
    }
    let mut rows = Rows::new(plan, (!exact).then_some(plan.filter).flatten());
    let columns = rows.columns.clone();
    let files: Vec<&SegmentMeta> = pinned
        .version
        .segments
        .iter()
        .map(|s| &**s)
        .filter(|s| plan.scope.file_in_scope(s) && !q.ingest.is_damaged(&s.id))
        .collect();
    let reads: Vec<_> = files.iter().map(|seg| read(q, seg, filter.clone(), &columns)).collect();
    let mut done = futures_util::stream::iter(reads).buffer_unordered(FILES);
    while let Some(batches) = done.next().await {
        let Some(batches) = batches? else { continue };
        stats.files_scanned += 1;
        for b in &batches {
            rows.batch(b, skip, accs, stats, eval_stats)?;
        }
    }
    Ok(())
}

/// One file's matching rows (`None`: the file is damaged and now skipped).
async fn read(
    q: &Query,
    seg: &SegmentMeta,
    filter: DfExpr,
    columns: &[usize],
) -> Result<Option<Vec<RecordBatch>>, QueryError> {
    let columns: Vec<&str> = columns.iter().map(|&c| NAMES[c]).collect();
    let attempt = async {
        let url = q.ingest.storage().store.url(&seg.core_path);
        let df = q.ctx.read_parquet(url, ParquetReadOptions::default()).await?;
        Ok::<_, QueryError>(df.filter(filter)?.select_columns(&columns)?.collect().await?)
    };
    match attempt.await {
        Ok(b) => Ok(Some(b)),
        Err(e) if damaged_alone(&q.ingest, seg, &e).await => Ok(None),
        Err(e) => Err(e),
    }
}

fn field_column(f: Field) -> usize {
    match f {
        Field::Status => STATUS,
        Field::Name => NAME,
        Field::RunType => RUN_TYPE,
        Field::Model => MODEL,
        Field::ThreadId => THREAD_ID,
        Field::Project => PROJECT,
        Field::Error => ERROR,
        Field::StartTime => START_TIME,
        Field::EndTime => END_TIME,
        Field::Latency => LATENCY_MS,
        Field::Tags => TAGS,
        Field::TotalTokens => TOTAL_TOKENS,
        Field::InputTokens => INPUT_TOKENS,
        Field::OutputTokens => OUTPUT_TOKENS,
        Field::TraceId => TRACE_ID,
        Field::RunId => RUN_ID,
        Field::ParentRunId => PARENT_RUN_ID,
    }
}

enum Strs<'a> {
    Plain(&'a StringArray),
    View(&'a StringViewArray),
}

impl<'a> Strs<'a> {
    fn of(a: &'a dyn Array) -> Result<Strs<'a>, StorageError> {
        if let Some(s) = a.as_any().downcast_ref::<StringArray>() {
            return Ok(Strs::Plain(s));
        }
        a.as_any()
            .downcast_ref::<StringViewArray>()
            .map(Strs::View)
            .ok_or_else(|| StorageError::Format("expected a string column".into()))
    }

    fn get(&self, i: usize) -> Option<&'a str> {
        match self {
            Strs::Plain(s) => (!s.is_null(i)).then(|| s.value(i)),
            Strs::View(s) => (!s.is_null(i)).then(|| s.value(i)),
        }
    }
}

/// The projected columns of one batch, by index; absent ones were not asked for.
enum Col<'a> {
    Strs(Strs<'a>),
    Ints(&'a Int64Array),
    Ids(&'a FixedSizeBinaryArray),
    List(&'a ListArray),
}

struct Cols<'a>([Option<Col<'a>>; NAMES.len()]);

impl<'a> Cols<'a> {
    fn of(b: &'a RecordBatch, wanted: &[usize]) -> Result<Cols<'a>, StorageError> {
        let mut c = Cols(std::array::from_fn(|_| None));
        for &n in wanted {
            let a = &**b
                .column_by_name(NAMES[n])
                .ok_or_else(|| StorageError::Format(format!("missing column {}", NAMES[n])))?;
            let any = a.as_any();
            c.0[n] = Some(if let Some(x) = any.downcast_ref::<Int64Array>() {
                Col::Ints(x)
            } else if let Some(x) = any.downcast_ref::<FixedSizeBinaryArray>() {
                Col::Ids(x)
            } else if let Some(x) = any.downcast_ref::<ListArray>() {
                Col::List(x)
            } else {
                Col::Strs(Strs::of(a)?)
            });
        }
        Ok(c)
    }

    fn str(&self, n: usize, i: usize) -> Option<&'a str> {
        match &self.0[n] {
            Some(Col::Strs(s)) => s.get(i),
            _ => None,
        }
    }

    fn int(&self, n: usize, i: usize) -> Option<i64> {
        match &self.0[n] {
            Some(Col::Ints(a)) if !a.is_null(i) => Some(a.value(i)),
            _ => None,
        }
    }

    fn id(&self, n: usize, i: usize) -> Option<RunId> {
        match &self.0[n] {
            Some(Col::Ids(a)) if !a.is_null(i) => uuid::Uuid::from_slice(a.value(i)).ok(),
            _ => None,
        }
    }

    fn tags(&self, i: usize) -> Vec<&'a str> {
        let Some(Col::List(t)) = &self.0[TAGS] else {
            return vec![];
        };
        if t.is_null(i) {
            return vec![];
        }
        let (from, to) = (t.value_offsets()[i] as usize, t.value_offsets()[i + 1] as usize);
        match Strs::of(&**t.values()) {
            Ok(s) => (from..to).filter_map(|j| s.get(j)).collect(),
            Err(_) => vec![],
        }
    }
}

/// `"<path>":` as the `attrs` JSON writes the key.
fn attr_key(path: &str) -> String {
    format!("{}:", serde_json::to_string(path).expect("a string serializes"))
}

/// A run with only the fields some reader needs set (the rest stay at their defaults).
fn blank_run() -> Run {
    let id = uuid::Uuid::nil();
    let ev = RunEvent {
        op: Op::Post,
        event_seq: 0,
        received_at: Ts(0),
        source: Source::Native,
        body_hash: 0,
        fields: RunFields {
            id: Some(id),
            trace_id: Some(id),
            dotted_order: Some(format!("20260101T000000000000Z{id}")),
            ..RunFields::default()
        },
    };
    let mut slot = None;
    tracedb_model::merge(&mut slot, &ev).expect("a run with an id merges");
    slot.expect("merged").run
}

/// An inexact filter, evaluated on a run built from the columns it reads.
struct Light<'f> {
    filter: &'f Expr,
    fields: Vec<Field>,
    /// (path, its `attrs` key)
    attrs: Vec<(String, String)>,
    run: Run,
}

impl Light<'_> {
    fn fill(&mut self, c: &Cols, i: usize) -> Attrs {
        let r = &mut self.run;
        let s = |n: Field| c.str(field_column(n), i).map(str::to_string);
        let ts = |n: Field| c.int(field_column(n), i).map(Ts);
        let tok = |n: Field| c.int(field_column(n), i).map(|v| v as u64);
        for f in &self.fields {
            match f {
                Field::Status => r.status = status_of(c.str(STATUS, i)),
                Field::Name => r.name = s(Field::Name),
                Field::RunType => {
                    (r.run_type, r.run_type_raw) = c.str(RUN_TYPE, i).map_or((RunType::Other, None), RunType::from_raw)
                }
                Field::Model => r.model = s(Field::Model),
                Field::ThreadId => r.thread_id = s(Field::ThreadId),
                Field::Project => r.project = s(Field::Project).unwrap_or_else(|| DEFAULT_PROJECT.into()),
                Field::Error => r.error = s(Field::Error),
                Field::StartTime => r.start_time = ts(Field::StartTime),
                Field::EndTime => r.end_time = ts(Field::EndTime),
                Field::Latency => r.latency_ms = tok(Field::Latency),
                Field::Tags => r.tags = c.tags(i).into_iter().map(str::to_string).collect(),
                Field::TotalTokens => r.total_tokens = tok(Field::TotalTokens),
                Field::InputTokens => r.input_tokens = tok(Field::InputTokens),
                Field::OutputTokens => r.output_tokens = tok(Field::OutputTokens),
                Field::TraceId => r.trace_id = c.id(TRACE_ID, i).unwrap_or_default(),
                Field::RunId => r.run_id = c.id(RUN_ID, i).unwrap_or_default(),
                Field::ParentRunId => r.parent_run_id = c.id(PARENT_RUN_ID, i),
            }
        }
        let text = c.str(ATTRS, i).unwrap_or("");
        self.attrs
            .iter()
            .filter_map(|(p, k)| Some((p.clone(), attr_value(text, k)?)))
            .collect()
    }
}

fn status_of(s: Option<&str>) -> Status {
    match s {
        Some("ok") => Status::Ok,
        Some("error") => Status::Error,
        _ => Status::Running,
    }
}

/// Reads facts and keys straight from projected columns.
struct Rows<'p, 'a> {
    plan: &'p Plan<'a>,
    /// Model calls are costed through `cost::of` on this run, with only the fields it reads set.
    priced: Run,
    /// `"metadata.<k>":` per metadata key.
    key_attrs: Vec<Option<String>>,
    light: Option<Light<'a>>,
    columns: Vec<usize>,
}

impl<'p, 'a> Rows<'p, 'a> {
    fn new(plan: &'p Plan<'a>, inexact: Option<&'a Expr>) -> Rows<'p, 'a> {
        let light = inexact.map(|filter| {
            let (mut fields, mut attrs) = (Vec::new(), Vec::new());
            visit_paths(filter, &mut |p| match p {
                Path::Field(f) => fields.push(*f),
                Path::Attr { .. } => attrs.push((p.text(), attr_key(&p.text()))),
                Path::Feedback(_) => {}
            });
            fields.sort_by_key(|f| field_column(*f));
            fields.dedup();
            attrs.sort();
            attrs.dedup();
            Light {
                filter,
                fields,
                attrs,
                run: blank_run(),
            }
        });
        let key_attrs = plan
            .keys
            .iter()
            .map(|k| match k {
                GroupKey::Metadata(m) => Some(attr_key(&format!("metadata.{m}"))),
                _ => None,
            })
            .collect();
        let mut rows = Rows {
            plan,
            priced: blank_run(),
            key_attrs,
            light,
            columns: vec![],
        };
        rows.columns = rows.wanted();
        rows
    }

    fn wanted(&self) -> Vec<usize> {
        let plan = self.plan;
        let mut c = vec![RUN_ID, TRACE_ID, PROJECT, STATUS, LATENCY_MS];
        if plan.tokens || plan.cost {
            c.extend([INPUT_TOKENS, OUTPUT_TOKENS, TOTAL_TOKENS]);
        }
        if plan.cost {
            c.extend([RUN_TYPE, MODEL, OUTPUTS_USAGE, METADATA, EXTRA]);
        }
        for k in plan.keys {
            c.push(match k {
                GroupKey::Name => NAME,
                GroupKey::RunType => RUN_TYPE,
                GroupKey::Model => MODEL,
                GroupKey::Status => STATUS,
                GroupKey::Tag => TAGS,
                GroupKey::Metadata(_) => ATTRS,
                GroupKey::FeedbackKey => continue,
            });
        }
        if let Some(l) = &self.light {
            c.extend(l.fields.iter().map(|f| field_column(*f)));
            if !l.attrs.is_empty() {
                c.push(ATTRS);
            }
        }
        c.sort();
        c.dedup();
        c
    }

    fn batch(
        &mut self,
        b: &RecordBatch,
        skip: &HashSet<RunId>,
        accs: &mut Accs,
        stats: &mut AggStats,
        eval_stats: &mut EvalStats,
    ) -> Result<(), StorageError> {
        let c = Cols::of(b, &self.columns)?;
        for i in 0..b.num_rows() {
            let (Some(run_id), Some(trace)) = (c.id(RUN_ID, i), c.id(TRACE_ID, i)) else {
                continue;
            };
            if skip.contains(&run_id) || !self.plan.row_ok(c.str(PROJECT, i), trace) {
                continue;
            }
            if let Some(l) = &mut self.light {
                let attrs = l.fill(&c, i);
                if !eval(
                    l.filter,
                    &Row {
                        run: &l.run,
                        attrs: &attrs,
                    },
                    self.plan.now,
                    eval_stats,
                ) {
                    continue;
                }
            }
            stats.clean_rows += 1;
            let tok = |n: usize| c.int(n, i).map(|v| v as u64);
            let mut f = Facts {
                status: status_of(c.str(STATUS, i)),
                latency_ms: c.int(LATENCY_MS, i),
                input_tokens: tok(INPUT_TOKENS),
                output_tokens: tok(OUTPUT_TOKENS),
                total_tokens: tok(TOTAL_TOKENS),
                cost: None,
                feedback: vec![],
            };
            let rt = c.str(RUN_TYPE, i);
            if self.plan.cost && rt == Some("llm") {
                let json = |n: usize| -> Option<JsonMap> {
                    match serde_json::from_str(c.str(n, i)?).ok()? {
                        Value::Object(m) => Some(m),
                        _ => None,
                    }
                };
                let usage = json(OUTPUTS_USAGE).or_else(|| json(METADATA)?.get("usage_metadata")?.as_object().cloned());
                let r = &mut self.priced;
                r.run_type = RunType::Llm;
                r.model = c.str(MODEL, i).map(str::to_string);
                r.input_tokens = f.input_tokens;
                r.output_tokens = f.output_tokens;
                // `extra` (often large) is only read when the usage reported no cost.
                r.reported_cost_pico = cost::reported_pico(usage.as_ref(), &JsonMap::new())
                    .or_else(|| cost::reported_pico(usage.as_ref(), &json(EXTRA)?));
                f.cost = cost::of(r, self.plan.prices);
            }
            let keys = self
                .plan
                .keys
                .iter()
                .zip(&self.key_attrs)
                .map(|(k, ak)| match k {
                    GroupKey::Name => KeyVal::of_str(c.str(NAME, i)),
                    GroupKey::Model => KeyVal::of_str(c.str(MODEL, i)),
                    GroupKey::Status => KeyVal::of_str(c.str(STATUS, i)),
                    GroupKey::RunType => KeyVal::of_str(Some(rt.unwrap_or("other"))),
                    GroupKey::Tag => KeyVal::of_tags(c.tags(i).into_iter()),
                    GroupKey::Metadata(_) => KeyVal::of_scalar(
                        attr_value(c.str(ATTRS, i).unwrap_or(""), ak.as_deref().unwrap_or_default()).as_ref(),
                    ),
                    GroupKey::FeedbackKey => KeyVal::None,
                })
                .collect();
            accs.add(keys, &f);
        }
        Ok(())
    }
}

/// The value of one key in an `attrs` JSON object (`key` is `"<path>":`), without parsing the rest. Keys
/// follow `{` or `,`; inside a string value a `"` is escaped, so a match there is never preceded by either.
fn attr_value(text: &str, key: &str) -> Option<Scalar> {
    let mut from = 0;
    while let Some(p) = text[from..].find(key).map(|p| p + from) {
        if p > 0 && matches!(text.as_bytes()[p - 1], b'{' | b',') {
            let rest = &text[p + key.len()..];
            let v = serde_json::Deserializer::from_str(rest)
                .into_iter::<Value>()
                .next()?
                .ok()?;
            return Some(match v {
                Value::String(s) => Scalar::Str(s),
                Value::Number(n) => n.as_f64().map_or(Scalar::Unindexed, Scalar::Num),
                Value::Bool(b) => Scalar::Bool(b),
                _ => Scalar::Unindexed,
            });
        }
        from = p + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attr_value_reads_one_key_like_the_full_parse() {
        let mut a = Attrs::new();
        a.insert("metadata.k".into(), Scalar::Str("v\",\"metadata.k\":1".into()));
        a.insert("metadata.k2".into(), Scalar::Num(2.5));
        a.insert("metadata.kb".into(), Scalar::Bool(true));
        a.insert("metadata.long".into(), Scalar::Unindexed);
        a.insert("metadata.x".into(), Scalar::Str("{\"metadata.k2\":9}".into()));
        let text = serde_json::to_string(&a).unwrap();
        for k in ["k", "k2", "kb", "long", "x", "missing", "k.sub"] {
            let path = format!("metadata.{k}");
            let key = format!("{}:", serde_json::to_string(&path).unwrap());
            assert_eq!(attr_value(&text, &key), a.get(&path).cloned(), "{k} in {text}");
        }
    }
}
