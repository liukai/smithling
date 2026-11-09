//! Read API: queries, root list (M1), projects, field catalog, trace tree, single run (contracts/http-api.md).

use axum::Json;
use axum::extract::{Path, Query as Q, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use smithling_ingest::Include;
use smithling_model::agg::{AggSpec, Compare, GroupKey, Metric, parse_stats};
use smithling_model::filter::{CmpOp, Expr, Field, Path as FPath, TimeAt, Value, parse_filter};
use smithling_model::{RunId, TraceId};
use smithling_query::{OrderBy, QueryError, QuerySpec, TimeRange};

use crate::AppState;
use crate::routes::errors::error;

/// `POST /api/query` body. Filter slots are LangSmith filter text (contracts/filter-language.md).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QueryBody {
    pub project: Option<String>,
    pub time: Option<TimeBody>,
    pub filter: Option<String>,
    pub trace_filter: Option<String>,
    pub tree_filter: Option<String>,
    pub is_root: Option<bool>,
    pub order: Option<OrderBody>,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
    pub columns: Vec<String>,
    /// Count every match (`stats.matched`), not only the runs read for the page.
    pub count: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TimeBody {
    /// `"12h"`, `"30m"`, `"7d"`: the last such span.
    pub last: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderBody {
    pub by: Option<String>,
    pub dir: Option<String>,
}

/// A request error that names what was wrong (and where, for filter text).
pub struct BadRequest(pub Box<Response>);

fn bad(code: &str, message: impl Into<String>) -> BadRequest {
    BadRequest(Box::new(error(StatusCode::BAD_REQUEST, code, message)))
}

fn filter_slot(slot: &str, text: &Option<String>) -> Result<Option<Expr>, BadRequest> {
    let Some(t) = text.as_deref().filter(|t| !t.trim().is_empty()) else {
        return Ok(None);
    };
    let slot_error = |code: &str, message: &str, start: usize, end: usize| {
        BadRequest(Box::new(
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {
                    "code": code, "message": message, "slot": slot, "start": start, "end": end,
                }})),
            )
                .into_response(),
        ))
    };
    let e = parse_filter(t).map_err(|e| slot_error("bad_filter", &e.message, e.start, e.end))?;
    Ok(Some(e))
}

fn time_at(field: &str, s: &str) -> Result<TimeAt, BadRequest> {
    TimeAt::parse(s).ok_or_else(|| {
        bad(
            "bad_time",
            format!("{field}: expected an ISO-8601 time or e.g. \"-12h\""),
        )
    })
}

impl QueryBody {
    pub fn to_spec(&self) -> Result<QuerySpec, BadRequest> {
        let mut time = TimeRange::default();
        if let Some(t) = &self.time {
            if let Some(last) = &t.last {
                time.from = Some(time_at("time.last", &format!("-{}", last.trim_start_matches('-')))?);
            }
            if let Some(f) = &t.from {
                time.from = Some(time_at("time.from", f)?);
            }
            if let Some(to) = &t.to {
                time.to = Some(time_at("time.to", to)?);
            }
        }
        let (order, ascending) = match &self.order {
            None => (OrderBy::StartTime, false),
            Some(o) => {
                let by = match o.by.as_deref().unwrap_or("start_time") {
                    "start_time" => OrderBy::StartTime,
                    "latency" => OrderBy::Latency,
                    other => {
                        return Err(bad(
                            "bad_order",
                            format!("order.by {other:?}: use start_time or latency"),
                        ));
                    }
                };
                let asc = match o.dir.as_deref().unwrap_or("desc") {
                    "desc" => false,
                    "asc" => true,
                    other => return Err(bad("bad_order", format!("order.dir {other:?}: use asc or desc"))),
                };
                (by, asc)
            }
        };
        let columns = self
            .columns
            .iter()
            .map(|c| column_path(c).ok_or_else(|| bad("bad_column", format!("unknown column {c:?}"))))
            .collect::<Result<_, _>>()?;
        Ok(QuerySpec {
            project: self.project.clone().filter(|p| !p.is_empty()),
            time,
            filter: filter_slot("filter", &self.filter)?,
            trace_filter: filter_slot("trace_filter", &self.trace_filter)?,
            tree_filter: filter_slot("tree_filter", &self.tree_filter)?,
            is_root: self.is_root,
            order,
            ascending,
            limit: self.limit,
            cursor: self.cursor.clone(),
            columns,
            traces: None,
            count: self.count,
            runs: None,
        })
    }
}

fn column_path(c: &str) -> Option<FPath> {
    if let Some(f) = Field::from_name(c) {
        return Some(FPath::Field(f));
    }
    let probe = format!("eq({c}, null)");
    match parse_filter(&probe).ok()? {
        Expr::Cmp { path, .. } => Some(path),
        _ => None,
    }
}

/// `body` plus `skipped_files: [ids]` when damaged files in its scope weren't read (M3-FR-18). Not inside
/// `warnings`: a run's `warnings` are its own.
fn with_skipped(status: StatusCode, body: impl serde::Serialize, skipped: Vec<String>) -> Response {
    let mut v = serde_json::to_value(body).expect("serializable");
    if !skipped.is_empty()
        && let Some(o) = v.as_object_mut()
    {
        o.insert("skipped_files".into(), json!(skipped));
    }
    (status, Json(v)).into_response()
}

fn not_found(message: &str, skipped: Vec<String>) -> Response {
    with_skipped(
        StatusCode::NOT_FOUND,
        json!({"error": {"code": "not_found", "message": message}}),
        skipped,
    )
}

fn query_error(e: QueryError) -> Response {
    match e {
        QueryError::BadCursor => error(StatusCode::BAD_REQUEST, "bad_cursor", e.to_string()),
        other => error(StatusCode::INTERNAL_SERVER_ERROR, "query_failed", other.to_string()),
    }
}

pub async fn query(
    State(st): State<AppState>,
    body: Result<Json<QueryBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let body = match body {
        Ok(Json(b)) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, "bad_request", e.body_text()),
    };
    let spec = match body.to_spec() {
        Ok(s) => s,
        Err(BadRequest(r)) => return *r,
    };
    match st.query.query(&spec).await {
        Ok(page) => Json(page).into_response(),
        Err(e) => query_error(e),
    }
}

#[derive(Deserialize)]
pub struct RootsQs {
    project: Option<String>,
    status: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
}

pub fn parse_status(s: &str) -> Option<&'static str> {
    ["running", "ok", "error"].into_iter().find(|v| *v == s)
}

/// M1's runs list as a query: roots whose start arrived, as part of the filter so pages are never short.
pub fn listed_roots() -> QuerySpec {
    let known = |f| Expr::Cmp {
        op: CmpOp::Neq,
        path: FPath::Field(f),
        value: Value::Null,
    };
    QuerySpec {
        filter: Some(Expr::And(vec![known(Field::StartTime), known(Field::Name)])),
        is_root: Some(true),
        ..QuerySpec::default()
    }
}

/// M1's root list, answered as a query (optional status) so old clients keep working.
pub async fn roots(State(st): State<AppState>, Q(q): Q<RootsQs>) -> Response {
    let mut spec = listed_roots();
    if let Some(s) = q.status.as_deref().filter(|s| !s.is_empty())
        && let Some(Expr::And(all)) = &mut spec.filter
    {
        match parse_status(s) {
            Some(v) => all.push(Expr::Cmp {
                op: CmpOp::Eq,
                path: FPath::Field(Field::Status),
                value: Value::Str(v.into()),
            }),
            None => return error(StatusCode::BAD_REQUEST, "bad_status", format!("unknown status {s:?}")),
        }
    }
    spec.project = q.project.filter(|p| !p.is_empty());
    spec.limit = q.limit;
    spec.cursor = q.cursor;
    match st.query.query(&spec).await {
        Ok(page) => {
            let runs: Vec<_> = page.runs.into_iter().map(|r| r.header).collect();
            Json(json!({"runs": runs, "next_cursor": page.next_cursor})).into_response()
        }
        Err(e) => query_error(e),
    }
}

pub async fn projects(State(st): State<AppState>) -> Response {
    match st.query.projects().await {
        Ok(p) => Json(p).into_response(),
        Err(e) => query_error(e),
    }
}

pub async fn trace(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let Ok(id) = id.parse::<TraceId>() else {
        return error(StatusCode::BAD_REQUEST, "bad_id", "trace id is not a UUID");
    };
    match st.query.trace(id).await {
        Ok(Some(t)) => with_skipped(StatusCode::OK, t, st.query.skipped_for_trace(id)),
        Ok(None) => not_found("no such trace", st.query.skipped_for_trace(id)),
        Err(e) => query_error(e),
    }
}

#[derive(Deserialize)]
pub struct RunQs {
    include: Option<String>,
}

pub async fn run(State(st): State<AppState>, Path(id): Path<String>, Q(q): Q<RunQs>) -> Response {
    let Ok(id) = id.parse::<RunId>() else {
        return error(StatusCode::BAD_REQUEST, "bad_id", "run id is not a UUID");
    };
    let mut include = Include::default();
    for part in q
        .include
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        match part {
            "inputs" => include.inputs = true,
            "outputs" => include.outputs = true,
            "events" => include.events = true,
            other => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "bad_include",
                    format!("unknown include {other:?}"),
                );
            }
        }
    }
    match st.query.run(id, include).await {
        Ok(Some(r)) => {
            let skipped = st.query.skipped_for_trace(r.header.trace_id);
            with_skipped(StatusCode::OK, r, skipped)
        }
        Ok(None) => not_found(
            "no such run",
            st.query.skipped(&smithling_query::Scope {
                run_id: Some(id),
                ..Default::default()
            }),
        ),
        Err(e) => query_error(e),
    }
}

#[derive(Deserialize)]
pub struct FieldsQs {
    project: Option<String>,
    /// `"12h"`: the last such span.
    time: Option<String>,
    path: Option<String>,
    #[serde(default)]
    prefix: String,
}

impl FieldsQs {
    fn scope(&self) -> Result<(Option<&str>, TimeRange), BadRequest> {
        let mut time = TimeRange::default();
        if let Some(t) = self.time.as_deref().filter(|t| !t.is_empty()) {
            time.from = Some(time_at("time", &format!("-{}", t.trim_start_matches('-')))?);
        }
        Ok((self.project.as_deref().filter(|p| !p.is_empty()), time))
    }
}

pub async fn fields(State(st): State<AppState>, Q(q): Q<FieldsQs>) -> Response {
    match q.scope() {
        Ok((project, time)) => match st.query.fields(project, time, &q.prefix).await {
            Ok(f) => with_skipped(StatusCode::OK, f, st.query.skipped_in(project, time)),
            Err(e) => query_error(e),
        },
        Err(BadRequest(r)) => *r,
    }
}

pub async fn field_values(State(st): State<AppState>, Q(q): Q<FieldsQs>) -> Response {
    let Some(path) = q.path.as_deref().filter(|p| !p.is_empty()) else {
        return error(StatusCode::BAD_REQUEST, "bad_request", "path is required");
    };
    match q.scope() {
        Ok((project, time)) => match st.query.values(project, time, path, &q.prefix).await {
            Ok(v) => with_skipped(StatusCode::OK, v, st.query.skipped_in(project, time)),
            Err(e) => query_error(e),
        },
        Err(BadRequest(r)) => *r,
    }
}

/// `POST /api/aggregate` body: the query fields of `POST /api/query` plus an `AggSpec`, or a `stats` line in
/// place of `group_by` + `metrics` (data-model §7). Order, limit, cursor, columns and count are accepted and
/// ignored.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AggBody {
    project: Option<String>,
    time: Option<TimeBody>,
    filter: Option<String>,
    trace_filter: Option<String>,
    tree_filter: Option<String>,
    is_root: Option<bool>,
    stats: Option<String>,
    group_by: Option<serde_json::Value>,
    metrics: Option<serde_json::Value>,
    view: Option<serde_json::Value>,
    top: Option<serde_json::Value>,
    bucket: Option<serde_json::Value>,
    compare: Option<serde_json::Value>,
    order: Option<serde_json::Value>,
    limit: Option<serde_json::Value>,
    cursor: Option<serde_json::Value>,
    columns: Option<serde_json::Value>,
    count: Option<serde_json::Value>,
}

impl AggBody {
    fn agg(&self) -> Result<AggSpec, BadRequest> {
        let mut raw = serde_json::Map::new();
        for (k, v) in [
            ("group_by", &self.group_by),
            ("metrics", &self.metrics),
            ("view", &self.view),
            ("top", &self.top),
            ("bucket", &self.bucket),
            ("compare", &self.compare),
        ] {
            if let Some(v) = v {
                raw.insert(k.into(), v.clone());
            }
        }
        let spec = match &self.stats {
            Some(_) if self.group_by.is_some() || self.metrics.is_some() || self.top.is_some() => {
                return Err(bad(
                    "bad_stats",
                    "give either stats or group_by + metrics (+ top), not both",
                ));
            }
            Some(line) => {
                let parsed = parse_stats(line).map_err(|e| {
                    BadRequest(Box::new(
                        (
                            StatusCode::BAD_REQUEST,
                            Json(json!({"error": {
                                "code": "bad_stats", "message": e.message, "start": e.start, "end": e.end,
                            }})),
                        )
                            .into_response(),
                    ))
                })?;
                raw.insert(
                    "metrics".into(),
                    json!(parsed.metrics.iter().map(Metric::text).collect::<Vec<_>>()),
                );
                raw.insert(
                    "group_by".into(),
                    json!(parsed.group_by.iter().map(GroupKey::text).collect::<Vec<_>>()),
                );
                raw.insert("top".into(), json!(parsed.top));
                serde_json::from_value::<AggSpec>(raw.into()).map_err(|e| bad("bad_stats", e.to_string()))?
            }
            None => serde_json::from_value::<AggSpec>(raw.into()).map_err(|e| bad("bad_stats", e.to_string()))?,
        };
        if let Compare::Filter(f) = &spec.compare {
            filter_slot("compare.filter", &Some(smithling_model::filter::print(f)))?;
        }
        if spec
            .metrics
            .iter()
            .filter(|m| matches!(m, Metric::AvgFeedback(_)))
            .count()
            > 1
        {
            return Err(bad("bad_stats", "one avg(feedback.<key>) per request"));
        }
        for k in &spec.group_by {
            if let GroupKey::Metadata(m) = k
                && !smithling_model::attrs::promotable("metadata", m)
            {
                return Err(bad(
                    "bad_stats",
                    format!("metadata.{m} is deeper than the levels kept for filters and groups"),
                ));
            }
        }
        Ok(spec)
    }
}

/// The query and aggregate an `AggBody` asks for.
pub(crate) fn agg_request(body: &AggBody) -> Result<(QuerySpec, AggSpec), BadRequest> {
    let query = QueryBody {
        project: body.project.clone(),
        time: body.time.clone(),
        filter: body.filter.clone(),
        trace_filter: body.trace_filter.clone(),
        tree_filter: body.tree_filter.clone(),
        is_root: body.is_root,
        ..QueryBody::default()
    };
    let (spec, agg) = (query.to_spec()?, body.agg()?);
    if matches!(agg.compare, Compare::Shift(_)) && spec.time.from.is_none() {
        return Err(bad(
            "bad_stats",
            "compare.shift needs a window start: time.last or time.from (--time in the CLI)",
        ));
    }
    Ok((spec, agg))
}

/// Damaged files skipped, taken out of the warnings (they're written as `skipped_files`).
pub(crate) fn take_skipped(res: &mut smithling_query::AggResult) -> Vec<String> {
    let skipped: Vec<String> = res
        .warnings
        .iter()
        .find_map(|w| match w {
            smithling_query::QueryWarning::SkippedFiles { ids } => Some(ids.clone()),
            _ => None,
        })
        .unwrap_or_default();
    res.warnings
        .retain(|w| !matches!(w, smithling_query::QueryWarning::SkippedFiles { .. }));
    skipped
}

pub async fn aggregate(
    State(st): State<AppState>,
    body: Result<Json<AggBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let body = match body {
        Ok(Json(b)) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, "bad_request", e.body_text()),
    };
    let (spec, agg) = match agg_request(&body) {
        Ok(x) => x,
        Err(BadRequest(r)) => return *r,
    };
    match st.query.aggregate(&spec, &agg, &st.prices).await {
        Ok(mut res) => {
            let skipped = take_skipped(&mut res);
            with_skipped(StatusCode::OK, res, skipped)
        }
        Err(e) => query_error(e),
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompactBody {
    /// Treat every window as closed (tests and the demo); otherwise only settled windows are compacted.
    #[serde(default)]
    force: bool,
}

/// `POST /api/compact` (M4 T021): runs compaction jobs until nothing is left to do, then deletes the replaced
/// files that are due. An empty body is `{}`. Blocks until done; the background job keeps running either way.
pub async fn compact(State(st): State<AppState>, body: axum::body::Bytes) -> Response {
    let body: CompactBody = if body.iter().all(u8::is_ascii_whitespace) {
        CompactBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(b) => b,
            Err(e) => return error(StatusCode::BAD_REQUEST, "bad_request", e.to_string()),
        }
    };
    let ingest = st.ingest.clone();
    let before = ingest.health().files;
    let done = tokio::task::spawn_blocking(move || {
        let (mut jobs, mut inputs, mut outputs, mut rows_in, mut rows_out) = (0, 0, 0, 0, 0);
        while let Some(r) = ingest.compact(body.force)? {
            jobs += 1;
            (inputs, outputs) = (inputs + r.inputs, outputs + r.outputs);
            (rows_in, rows_out) = (rows_in + r.rows_in, rows_out + r.rows_out);
        }
        let deleted = ingest.delete_replaced()?;
        Ok::<_, smithling_storage::StorageError>(json!({
            "jobs": jobs, "inputs": inputs, "outputs": outputs, "rows_in": rows_in, "rows_out": rows_out,
            "deleted": deleted,
        }))
    })
    .await;
    match done {
        Ok(Ok(mut out)) => {
            out["files_before"] = json!(before);
            out["files_after"] = json!(st.ingest.health().files);
            Json(out).into_response()
        }
        Ok(Err(e)) => error(StatusCode::SERVICE_UNAVAILABLE, "storage", e.to_string()),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}
