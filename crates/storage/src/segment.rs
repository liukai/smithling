//! The segment format (data-model §2–3): one flush of one project as two immutable Parquet files.
//!
//! - **core**: one row per run version: identity, header fields, promoted attributes (as JSON text), and
//!   the bookkeeping that merging needs (`present`, seqs, `needs_merge`). Sorted by `(trace_id,
//!   dotted_order)`, ~64k-row row groups, page index, bloom filters on `run_id` and `trace_id`.
//! - **payload**: `inputs`, `outputs`, `events` for the same rows, in the same order; read only on demand.
//!
//! [`decode_core`] rebuilds each version through the M1 merge rule, so a stored version merges exactly
//! like the memtable state it was written from.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use datafusion::arrow::array::{
    Array, ArrayRef, BooleanArray, BooleanBuilder, FixedSizeBinaryArray, FixedSizeBinaryBuilder, Int64Array,
    Int64Builder, ListArray, ListBuilder, StringArray, StringBuilder, StringViewArray,
};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::parquet::arrow::ArrowWriter;
use datafusion::parquet::basic::{Compression, ZstdLevel};
use datafusion::parquet::file::properties::{EnabledStatistics, WriterProperties};
use datafusion::parquet::schema::types::ColumnPath;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracedb_model::attrs::{Attrs, Scalar};
use tracedb_model::{Op, PayloadSizes, RunEvent, RunFields, RunId, RunState, Source, TraceId, Ts, merge, present};

use crate::StorageError;

pub const ROW_GROUP_ROWS: usize = 65_536;
pub const TOP_VALUES: usize = 50;
pub const MAX_PATHS: usize = 10_000;
/// A `field_stats` row with this path marks a segment whose catalog stopped at [`MAX_PATHS`].
pub const INCOMPLETE_PATH: &str = "__incomplete__";

/// One registered segment, as the metastore records it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SegmentMeta {
    pub id: String,
    pub project: String,
    pub core_path: String,
    pub payload_path: String,
    pub min_start_key: i64,
    pub max_start_key: i64,
    pub min_trace_id: TraceId,
    pub max_trace_id: TraceId,
    pub min_seq: u64,
    pub max_seq: u64,
    pub row_count: u64,
    pub needs_merge_rows: u64,
    pub core_bytes: u64,
    pub payload_bytes: u64,
    pub created_at: i64,
}

/// Per segment, per path and type: how many rows have it, and its most common values.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FieldStat {
    pub segment_id: String,
    pub path: String,
    pub ty: String,
    pub count: u64,
    pub top_values: Vec<(Value, u64)>,
    pub other_count: u64,
}

/// A run version to write.
#[derive(Clone, Debug)]
pub struct Version {
    pub state: RunState,
    pub attrs: Attrs,
    /// Another version of this run may exist elsewhere (design §4.3).
    pub needs_merge: bool,
}

/// A version read back from a core file. Payload fields are not loaded (`inputs`/`outputs`/`events` are
/// `None`; their `present` bits and sizes are kept).
#[derive(Clone, Debug)]
pub struct StoredRow {
    pub state: RunState,
    pub attrs: Attrs,
    pub needs_merge: bool,
    pub payload_row: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Payload {
    pub inputs: Option<Value>,
    pub outputs: Option<Value>,
    pub events: Option<Vec<Value>>,
}

pub struct Written {
    pub meta: SegmentMeta,
    pub stats: Vec<FieldStat>,
    pub core: Bytes,
    pub payload: Bytes,
}

pub fn core_schema() -> SchemaRef {
    let id = |n: &str, null: bool| Field::new(n, DataType::FixedSizeBinary(16), null);
    let s = |n: &str| Field::new(n, DataType::Utf8, true);
    let i = |n: &str, null: bool| Field::new(n, DataType::Int64, null);
    Arc::new(Schema::new(vec![
        id("run_id", false),
        id("trace_id", false),
        id("parent_run_id", true),
        Field::new("dotted_order", DataType::Utf8, false),
        i("start_key", false),
        i("depth", false),
        Field::new("is_root", DataType::Boolean, false),
        s("project"),
        s("address"),
        s("name"),
        s("run_type"),
        i("start_time", true),
        i("end_time", true),
        Field::new("status", DataType::Utf8, false),
        i("latency_ms", true),
        s("error"),
        Field::new(
            "tags",
            DataType::List(Arc::new(Field::new_list_field(DataType::Utf8, true))),
            true,
        ),
        s("thread_id"),
        s("model"),
        i("input_tokens", true),
        i("output_tokens", true),
        i("total_tokens", true),
        s("metadata"),
        s("extra"),
        s("dropped"),
        s("outputs_usage"),
        Field::new("attrs", DataType::Utf8, false),
        i("present", false),
        i("first_seq", false),
        i("last_seq", false),
        i("event_count", false),
        i("created_at", false),
        i("updated_at", false),
        Field::new("needs_merge", DataType::Boolean, false),
        i("inputs_bytes", false),
        i("outputs_bytes", false),
        i("events_bytes", false),
        i("payload_row", false),
    ]))
}

pub fn payload_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("run_id", DataType::FixedSizeBinary(16), false),
        Field::new("inputs", DataType::Utf8, true),
        Field::new("outputs", DataType::Utf8, true),
        Field::new("events", DataType::Utf8, true),
    ]))
}

/// The run's own start time, from the last segment of its `dotted_order`; every version has it.
pub fn start_key(dotted_order: &str) -> i64 {
    let seg = dotted_order.rsplit('.').next().unwrap_or(dotted_order);
    let b = seg.as_bytes();
    if b.len() < 22 {
        return 0;
    }
    let ts = format!(
        "{}-{}-{}T{}:{}:{}.{}Z",
        &seg[0..4],
        &seg[4..6],
        &seg[6..8],
        &seg[9..11],
        &seg[11..13],
        &seg[13..15],
        &seg[15..21]
    );
    Ts::parse(&ts).map_or(0, |t| t.0)
}

fn json_text(v: &Value) -> String {
    serde_json::to_string(v).expect("JSON values serialize")
}

/// Writes one project's versions as a segment. Rows are sorted by `(trace_id, dotted_order)` first.
pub fn write(project: &str, mut rows: Vec<Version>, created_at: Ts) -> Result<Written, StorageError> {
    rows.sort_by(|a, b| {
        (a.state.run.trace_id, &a.state.run.dotted_order).cmp(&(b.state.run.trace_id, &b.state.run.dotted_order))
    });
    let id = ulid::Ulid::new().to_string();
    let day = created_at.to_string().get(..10).unwrap_or("1970-01-01").to_string();
    let dir = folder(project);
    let core_path = format!("{dir}/{day}/seg-{id}.core.parquet");
    let payload_path = format!("{dir}/{day}/seg-{id}.payload.parquet");

    let core = encode_core(&rows)?;
    let payload = encode_payload(&rows)?;
    let stats = field_stats(&id, &rows);
    let keys: Vec<i64> = rows.iter().map(|r| start_key(&r.state.run.dotted_order)).collect();
    let meta = SegmentMeta {
        id,
        project: project.to_string(),
        core_path,
        payload_path,
        min_start_key: keys.iter().copied().min().unwrap_or(0),
        max_start_key: keys.iter().copied().max().unwrap_or(0),
        min_trace_id: rows.first().map(|r| r.state.run.trace_id).unwrap_or_default(),
        max_trace_id: rows.last().map(|r| r.state.run.trace_id).unwrap_or_default(),
        min_seq: rows.iter().map(|r| r.state.first_seq).min().unwrap_or(0),
        max_seq: rows.iter().map(|r| r.state.last_seq).max().unwrap_or(0),
        row_count: rows.len() as u64,
        needs_merge_rows: rows.iter().filter(|r| r.needs_merge).count() as u64,
        core_bytes: core.len() as u64,
        payload_bytes: payload.len() as u64,
        created_at: created_at.0,
    };
    Ok(Written {
        meta,
        stats,
        core,
        payload,
    })
}

/// Folder name for a project: readable letters, digits and `-_.`, plus a short hash of the real name, so two
/// projects never share a folder (`a/b` vs `a_b`, or `Foo` vs `foo` on a case-insensitive disk).
fn folder(project: &str) -> String {
    if project.is_empty() {
        return "_unknown".into();
    }
    let s: String = project
        .chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    // FNV-1a: stable across builds and platforms.
    let h = project.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100_0000_01b3)
    });
    format!("{}-{:08x}", s.trim_start_matches('.'), h as u32)
}

fn props() -> WriterProperties {
    WriterProperties::builder()
        .set_max_row_group_row_count(Some(ROW_GROUP_ROWS))
        .set_statistics_enabled(EnabledStatistics::Page)
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(3).expect("valid level")))
        .set_column_bloom_filter_enabled(ColumnPath::from("run_id"), true)
        .set_column_bloom_filter_enabled(ColumnPath::from("trace_id"), true)
        .build()
}

fn to_parquet(batch: RecordBatch, props: WriterProperties) -> Result<Bytes, StorageError> {
    let mut buf = Vec::new();
    let mut w = ArrowWriter::try_new(&mut buf, batch.schema(), Some(props))?;
    w.write(&batch)?;
    w.close()?;
    Ok(Bytes::from(buf))
}

fn encode_core(rows: &[Version]) -> Result<Bytes, StorageError> {
    let n = rows.len();
    let mut run_id = FixedSizeBinaryBuilder::with_capacity(n, 16);
    let mut trace_id = FixedSizeBinaryBuilder::with_capacity(n, 16);
    let mut parent = FixedSizeBinaryBuilder::with_capacity(n, 16);
    let mut tags = ListBuilder::new(StringBuilder::new());
    let mut is_root = BooleanBuilder::with_capacity(n);
    let mut needs_merge = BooleanBuilder::with_capacity(n);
    let mut strs: Vec<StringBuilder> = (0..15).map(|_| StringBuilder::new()).collect();
    let mut ints: Vec<Int64Builder> = (0..18).map(|_| Int64Builder::with_capacity(n)).collect();
    for (row, v) in rows.iter().enumerate() {
        let st = &v.state;
        let r = &st.run;
        let has = |bit: u32| st.present & bit != 0;
        run_id.append_value(r.run_id.as_bytes())?;
        trace_id.append_value(r.trace_id.as_bytes())?;
        match r.parent_run_id {
            Some(p) => parent.append_value(p.as_bytes())?,
            None => parent.append_null(),
        }
        is_root.append_value(r.is_root);
        needs_merge.append_value(v.needs_merge);
        if has(present::TAGS) {
            for t in &r.tags {
                tags.values().append_value(t);
            }
            tags.append(true);
        } else {
            tags.append(false);
        }
        let opt = |b: bool, s: Option<&str>| if b { s.map(str::to_string) } else { None };
        let svals: [Option<String>; 15] = [
            Some(r.dotted_order.clone()),
            opt(has(present::PROJECT), Some(&r.project)),
            opt(has(present::ADDRESS), r.address.as_deref()),
            opt(has(present::NAME), r.name.as_deref()),
            has(present::RUN_TYPE).then(|| {
                r.run_type_raw
                    .clone()
                    .unwrap_or_else(|| tracedb_model::run::run_type_str(r.run_type).to_string())
            }),
            Some(tracedb_model::filter::status_name(r.status).to_string()),
            opt(has(present::ERROR), r.error.as_deref()),
            r.thread_id.clone(),
            r.model.clone(),
            has(present::METADATA).then(|| json_text(&Value::Object(r.metadata.clone()))),
            has(present::EXTRA).then(|| json_text(&Value::Object(r.extra.clone()))),
            has(present::DROPPED).then(|| serde_json::to_string(&r.dropped).expect("map serializes")),
            Some(serde_json::to_string(&v.attrs).expect("attrs serialize")),
            st.outputs_usage.as_ref().map(|u| json_text(&Value::Object(u.clone()))),
            None,
        ];
        for (b, val) in strs.iter_mut().zip(svals) {
            b.append_option(val);
        }
        let t = |b: bool, ts: Option<Ts>| if b { ts.map(|t| t.0) } else { None };
        let ivals: [Option<i64>; 18] = [
            Some(start_key(&r.dotted_order)),
            Some(r.depth as i64),
            t(has(present::START), r.start_time),
            t(has(present::END), r.end_time),
            r.latency_ms.map(|v| v as i64),
            r.input_tokens.map(|v| v as i64),
            r.output_tokens.map(|v| v as i64),
            r.total_tokens.map(|v| v as i64),
            Some(st.present as i64),
            Some(st.first_seq as i64),
            Some(st.last_seq as i64),
            Some(st.event_count as i64),
            Some(st.created_at.0),
            Some(st.updated_at.0),
            Some(st.payload_sizes.inputs as i64),
            Some(st.payload_sizes.outputs as i64),
            Some(st.payload_sizes.events as i64),
            Some(row as i64),
        ];
        for (b, val) in ints.iter_mut().zip(ivals) {
            b.append_option(val);
        }
    }
    let mut s: Vec<ArrayRef> = strs.into_iter().map(|mut b| Arc::new(b.finish()) as ArrayRef).collect();
    let mut i: Vec<ArrayRef> = ints.into_iter().map(|mut b| Arc::new(b.finish()) as ArrayRef).collect();
    let mut take_s = |k: usize| std::mem::replace(&mut s[k], Arc::new(StringArray::from(Vec::<String>::new())));
    let (
        dotted,
        project,
        address,
        name,
        run_type,
        status,
        error,
        thread,
        model,
        metadata,
        extra,
        dropped,
        attrs,
        usage,
    ) = (
        take_s(0),
        take_s(1),
        take_s(2),
        take_s(3),
        take_s(4),
        take_s(5),
        take_s(6),
        take_s(7),
        take_s(8),
        take_s(9),
        take_s(10),
        take_s(11),
        take_s(12),
        take_s(13),
    );
    let mut take_i = |k: usize| std::mem::replace(&mut i[k], Arc::new(Int64Array::from(Vec::<i64>::new())));
    let cols: Vec<ArrayRef> = vec![
        Arc::new(run_id.finish()),
        Arc::new(trace_id.finish()),
        Arc::new(parent.finish()),
        dotted,
        take_i(0),
        take_i(1),
        Arc::new(is_root.finish()),
        project,
        address,
        name,
        run_type,
        take_i(2),
        take_i(3),
        status,
        take_i(4),
        error,
        Arc::new(tags.finish()),
        thread,
        model,
        take_i(5),
        take_i(6),
        take_i(7),
        metadata,
        extra,
        dropped,
        usage,
        attrs,
        take_i(8),
        take_i(9),
        take_i(10),
        take_i(11),
        take_i(12),
        take_i(13),
        Arc::new(needs_merge.finish()),
        take_i(14),
        take_i(15),
        take_i(16),
        take_i(17),
    ];
    to_parquet(RecordBatch::try_new(core_schema(), cols)?, props())
}

fn encode_payload(rows: &[Version]) -> Result<Bytes, StorageError> {
    let mut run_id = FixedSizeBinaryBuilder::with_capacity(rows.len(), 16);
    let (mut inputs, mut outputs, mut events) = (StringBuilder::new(), StringBuilder::new(), StringBuilder::new());
    for v in rows {
        let r = &v.state.run;
        let p = v.state.present;
        let unloaded = (p & present::INPUTS != 0 && r.inputs.is_none())
            || (p & present::OUTPUTS != 0 && r.outputs.is_none())
            || (p & present::EVENTS != 0 && r.events.is_none());
        if unloaded {
            return Err(StorageError::Format(format!(
                "run {} has a payload that isn't loaded; writing it would drop the payload",
                r.run_id
            )));
        }
        run_id.append_value(r.run_id.as_bytes())?;
        inputs.append_option(r.inputs.as_ref().map(json_text));
        outputs.append_option(r.outputs.as_ref().map(json_text));
        events.append_option(r.events.as_ref().map(|e| json_text(&Value::Array(e.clone()))));
    }
    let batch = RecordBatch::try_new(
        payload_schema(),
        vec![
            Arc::new(run_id.finish()),
            Arc::new(inputs.finish()),
            Arc::new(outputs.finish()),
            Arc::new(events.finish()),
        ],
    )?;
    let props = WriterProperties::builder()
        .set_max_row_group_row_count(Some(ROW_GROUP_ROWS))
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(3).expect("valid level")))
        .set_column_bloom_filter_enabled(ColumnPath::from("run_id"), true)
        .build();
    to_parquet(batch, props)
}

/// String columns may come back as `Utf8` or `Utf8View` (DataFusion reads views by default).
enum Strs<'a> {
    Plain(&'a StringArray),
    View(&'a StringViewArray),
}

impl<'a> Strs<'a> {
    fn of(a: &'a ArrayRef) -> Result<Strs<'a>, StorageError> {
        if let Some(s) = a.as_any().downcast_ref::<StringArray>() {
            Ok(Strs::Plain(s))
        } else if let Some(s) = a.as_any().downcast_ref::<StringViewArray>() {
            Ok(Strs::View(s))
        } else {
            Err(StorageError::Format(format!(
                "expected a string column, got {}",
                a.data_type()
            )))
        }
    }

    fn get(&self, i: usize) -> Option<&'a str> {
        match self {
            Strs::Plain(a) => (!a.is_null(i)).then(|| a.value(i)),
            Strs::View(a) => (!a.is_null(i)).then(|| a.value(i)),
        }
    }
}

struct Cols<'a> {
    batch: &'a RecordBatch,
}

impl<'a> Cols<'a> {
    fn col(&self, name: &str) -> Result<&'a ArrayRef, StorageError> {
        self.batch
            .column_by_name(name)
            .ok_or_else(|| StorageError::Format(format!("missing column {name}")))
    }
    fn strs(&self, name: &str) -> Result<Strs<'a>, StorageError> {
        Strs::of(self.col(name)?)
    }
    fn ints(&self, name: &str) -> Result<&'a Int64Array, StorageError> {
        self.col(name)?
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| StorageError::Format(format!("column {name} is not Int64")))
    }
    fn bools(&self, name: &str) -> Result<&'a BooleanArray, StorageError> {
        self.col(name)?
            .as_any()
            .downcast_ref::<BooleanArray>()
            .ok_or_else(|| StorageError::Format(format!("column {name} is not Boolean")))
    }
    fn ids(&self, name: &str) -> Result<&'a FixedSizeBinaryArray, StorageError> {
        self.col(name)?
            .as_any()
            .downcast_ref::<FixedSizeBinaryArray>()
            .filter(|a| a.value_length() == 16)
            .ok_or_else(|| StorageError::Format(format!("column {name} is not FixedSizeBinary(16)")))
    }
}

fn uuid_at(a: &FixedSizeBinaryArray, i: usize) -> Option<uuid::Uuid> {
    if a.is_null(i) {
        return None;
    }
    uuid::Uuid::from_slice(a.value(i)).ok()
}

fn int_at(a: &Int64Array, i: usize) -> Option<i64> {
    (!a.is_null(i)).then(|| a.value(i))
}

/// Rebuilds the versions in a core-file batch (any column order; all core columns must be present).
pub fn decode_core(batch: &RecordBatch) -> Result<Vec<StoredRow>, StorageError> {
    let c = Cols { batch };
    let (run_id, trace_id, parent) = (c.ids("run_id")?, c.ids("trace_id")?, c.ids("parent_run_id")?);
    let dotted = c.strs("dotted_order")?;
    let (project, address, name, run_type, error) = (
        c.strs("project")?,
        c.strs("address")?,
        c.strs("name")?,
        c.strs("run_type")?,
        c.strs("error")?,
    );
    let (metadata, extra, dropped, attrs) = (
        c.strs("metadata")?,
        c.strs("extra")?,
        c.strs("dropped")?,
        c.strs("attrs")?,
    );
    let usage = c.strs("outputs_usage")?;
    let (start, end) = (c.ints("start_time")?, c.ints("end_time")?);
    let (pres, first, last, count) = (
        c.ints("present")?,
        c.ints("first_seq")?,
        c.ints("last_seq")?,
        c.ints("event_count")?,
    );
    let (created, updated, prow) = (c.ints("created_at")?, c.ints("updated_at")?, c.ints("payload_row")?);
    let (ib, ob, eb) = (
        c.ints("inputs_bytes")?,
        c.ints("outputs_bytes")?,
        c.ints("events_bytes")?,
    );
    let needs = c.bools("needs_merge")?;
    let tags = c
        .col("tags")?
        .as_any()
        .downcast_ref::<ListArray>()
        .ok_or_else(|| StorageError::Format("tags is not a list".into()))?;
    let json_obj = |s: Option<&str>| -> Result<Option<serde_json::Map<String, Value>>, StorageError> {
        match s {
            None => Ok(None),
            Some(t) => match serde_json::from_str(t)? {
                Value::Object(m) => Ok(Some(m)),
                _ => Err(StorageError::Format("expected a JSON object".into())),
            },
        }
    };
    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        let bits = pres.value(i) as u32;
        let tag_list = if tags.is_null(i) {
            None
        } else {
            let v = tags.value(i);
            let s = Strs::of(&v)?;
            Some((0..v.len()).filter_map(|k| s.get(k).map(str::to_string)).collect())
        };
        let fields = RunFields {
            id: uuid_at(run_id, i),
            trace_id: uuid_at(trace_id, i),
            parent_run_id: uuid_at(parent, i),
            dotted_order: dotted.get(i).map(str::to_string),
            project: project.get(i).map(str::to_string),
            address: address.get(i).map(str::to_string),
            name: name.get(i).map(str::to_string),
            run_type: run_type.get(i).map(str::to_string),
            start_time: int_at(start, i).map(Ts),
            end_time: int_at(end, i).map(Ts),
            error: error.get(i).map(str::to_string),
            tags: tag_list,
            metadata: json_obj(metadata.get(i))?,
            extra: json_obj(extra.get(i))?,
            inputs: None,
            outputs: None,
            events: None,
            dropped: dropped.get(i).map(serde_json::from_str).transpose()?,
        };
        let ev = RunEvent {
            op: if bits & present::CREATED_BY_PATCH != 0 {
                Op::Patch
            } else {
                Op::Post
            },
            event_seq: last.value(i) as u64,
            received_at: Ts(updated.value(i)),
            source: Source::Native,
            body_hash: 0,
            fields,
        };
        let mut slot = None;
        merge(&mut slot, &ev).map_err(|e| StorageError::Format(format!("row {i}: {e}")))?;
        let mut st = slot.expect("merge fills the slot");
        st.first_seq = first.value(i) as u64;
        st.last_seq = last.value(i) as u64;
        st.event_count = count.value(i) as u32;
        st.created_at = Ts(created.value(i));
        st.updated_at = Ts(updated.value(i));
        st.present = bits;
        st.outputs_usage = json_obj(usage.get(i))?;
        st.payload_sizes = PayloadSizes {
            inputs: ib.value(i) as u64,
            outputs: ob.value(i) as u64,
            events: eb.value(i) as u64,
        };
        tracedb_model::derive(&mut st);
        let attrs: Attrs = match attrs.get(i) {
            Some(t) => serde_json::from_str(t)?,
            None => Attrs::new(),
        };
        out.push(StoredRow {
            state: st,
            attrs,
            needs_merge: needs.value(i),
            payload_row: prow.value(i) as u32,
        });
    }
    Ok(out)
}

pub fn decode_payload(batch: &RecordBatch) -> Result<Vec<(RunId, Payload)>, StorageError> {
    let c = Cols { batch };
    let ids = c.ids("run_id")?;
    let (inputs, outputs, events) = (c.strs("inputs")?, c.strs("outputs")?, c.strs("events")?);
    (0..batch.num_rows())
        .map(|i| {
            let parse = |s: Option<&str>| s.map(serde_json::from_str::<Value>).transpose();
            let events = match parse(events.get(i))? {
                Some(Value::Array(a)) => Some(a),
                Some(_) => return Err(StorageError::Format("events is not an array".into())),
                None => None,
            };
            Ok((
                uuid_at(ids, i).ok_or_else(|| StorageError::Format(format!("payload row {i} has no run_id")))?,
                Payload {
                    inputs: parse(inputs.get(i))?,
                    outputs: parse(outputs.get(i))?,
                    events,
                },
            ))
        })
        .collect()
}

/// Rows with the path, and value (as JSON text) → (value, count).
type PathCounts = (u64, HashMap<String, (Value, u64)>);

/// Field catalog stats for one segment (data-model §10): built-in fields and every promoted path.
pub fn field_stats(segment_id: &str, rows: &[Version]) -> Vec<FieldStat> {
    let mut counts: HashMap<(String, &'static str), PathCounts> = HashMap::new();
    let mut incomplete = false;
    let mut add = |path: &str, ty: &'static str, value: Value| {
        let key = (path.to_string(), ty);
        if !counts.contains_key(&key) && counts.len() >= MAX_PATHS {
            incomplete = true;
            return;
        }
        let e = counts.entry(key).or_default();
        e.0 += 1;
        let k = json_text(&value);
        e.1.entry(k).or_insert((value, 0)).1 += 1;
    };
    for v in rows {
        let r = &v.state.run;
        add("status", "string", tracedb_model::filter::status_name(r.status).into());
        if let Some(n) = &r.name {
            add("name", "string", n.clone().into());
        }
        add(
            "run_type",
            "string",
            r.run_type_raw
                .clone()
                .unwrap_or_else(|| tracedb_model::run::run_type_str(r.run_type).into())
                .into(),
        );
        if let Some(m) = &r.model {
            add("model", "string", m.clone().into());
        }
        if v.state.project_set {
            add("project", "string", r.project.clone().into());
        }
        for t in &r.tags {
            add("tags", "list", t.clone().into());
        }
        for (path, val) in &v.attrs {
            let (ty, value) = match val {
                Scalar::Str(s) => ("string", Value::from(s.clone())),
                Scalar::Num(n) => (
                    "number",
                    serde_json::Number::from_f64(*n)
                        .map(Value::Number)
                        .unwrap_or(Value::Null),
                ),
                Scalar::Bool(b) => ("bool", Value::Bool(*b)),
                Scalar::Unindexed => ("unindexed", Value::Null),
            };
            add(path, ty, value);
        }
    }
    let mut out: Vec<FieldStat> = counts
        .into_iter()
        .map(|((path, ty), (count, values))| {
            let mut vals: Vec<(Value, u64)> = values.into_values().collect();
            vals.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| json_text(&a.0).cmp(&json_text(&b.0))));
            vals.truncate(TOP_VALUES);
            let top: u64 = vals.iter().map(|v| v.1).sum();
            FieldStat {
                segment_id: segment_id.to_string(),
                path,
                ty: ty.to_string(),
                count,
                other_count: count - top,
                top_values: vals,
            }
        })
        .collect();
    if incomplete {
        out.push(FieldStat {
            segment_id: segment_id.to_string(),
            path: INCOMPLETE_PATH.into(),
            ty: "marker".into(),
            count: 0,
            top_values: vec![],
            other_count: 0,
        });
    }
    out.sort_by(|a, b| (&a.path, &a.ty).cmp(&(&b.path, &b.ty)));
    out
}
