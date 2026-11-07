//! Upload routes: LangSmith-compatible (`/info`, `/runs…`, `/feedback`) and native (`/v1/events`).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tracedb_ingest::{AcceptError, AcceptResult, Incoming, Rejected};
use tracedb_model::validate::Reject;
use tracedb_model::{Op, Source};

use crate::AppState;
use crate::langsmith_map::{Dialect, incoming};
use crate::routes::errors::{accept_error, error};

pub const BATCH_SIZE_LIMIT: usize = 100;
pub const BATCH_SIZE_LIMIT_BYTES: usize = 20 * 1024 * 1024;

pub async fn info() -> Json<Value> {
    Json(json!({
        "version": "",
        "tracedb_version": env!("CARGO_PKG_VERSION"),
        "license_expiration_time": null,
        "batch_ingest_config": {
            "use_multipart_endpoint": true,
            // The SDK uses this object as is (no defaults), so every key it reads must be present.
            "scale_up_qsize_trigger": 200,
            "scale_up_nthreads_limit": 32,
            "scale_down_nempty_trigger": 4,
            "size_limit": BATCH_SIZE_LIMIT,
            "size_limit_bytes": BATCH_SIZE_LIMIT_BYTES,
        },
        "instance_flags": {"zstd_compression_enabled": true},
    }))
}

#[derive(Deserialize)]
pub struct Batch {
    #[serde(default)]
    post: Vec<Value>,
    #[serde(default)]
    patch: Vec<Value>,
}

pub async fn runs_batch(State(st): State<AppState>, Json(b): Json<Batch>) -> Response {
    let items = b
        .post
        .iter()
        .map(|v| (Op::Post, v))
        .chain(b.patch.iter().map(|v| (Op::Patch, v)));
    submit(&st, items, Source::LsBatch, Dialect::LangSmith).await
}

pub async fn create_run(State(st): State<AppState>, Json(v): Json<Value>) -> Response {
    submit(&st, [(Op::Post, &v)], Source::LsSingle, Dialect::LangSmith).await
}

pub async fn update_run(State(st): State<AppState>, Path(run_id): Path<String>, Json(mut v): Json<Value>) -> Response {
    if let Some(obj) = v.as_object_mut() {
        obj.entry("id").or_insert(Value::String(run_id));
    }
    submit(&st, [(Op::Patch, &v)], Source::LsSingle, Dialect::LangSmith).await
}

pub async fn feedback() -> Response {
    error(
        StatusCode::NOT_IMPLEMENTED,
        "unsupported",
        "feedback is supported from M3",
    )
}

#[derive(Deserialize)]
pub struct NativeBatch {
    events: Vec<NativeEvent>,
}

#[derive(Deserialize)]
pub struct NativeEvent {
    op: Op,
    fields: Value,
}

pub async fn native_events(State(st): State<AppState>, Json(b): Json<NativeBatch>) -> Response {
    submit(
        &st,
        b.events.iter().map(|e| (e.op, &e.fields)),
        Source::Native,
        Dialect::Native,
    )
    .await
}

async fn submit<'a>(
    st: &AppState,
    items: impl IntoIterator<Item = (Op, &'a Value)>,
    source: Source,
    dialect: Dialect,
) -> Response {
    match accept_items(st, items, source, dialect).await {
        Ok(r) => (StatusCode::ACCEPTED, Json(r)).into_response(),
        Err(e) => accept_error(&e),
    }
}

/// Maps and writes items; mapping rejections come first in `rejected`.
pub(crate) async fn accept_items<'a>(
    st: &AppState,
    items: impl IntoIterator<Item = (Op, &'a Value)>,
    source: Source,
    dialect: Dialect,
) -> Result<AcceptResult, AcceptError> {
    let mut ok: Vec<Incoming> = Vec::new();
    let mut rejected: Vec<Rejected> = Vec::new();
    for (op, v) in items {
        match incoming(op, source, v, dialect) {
            Ok(it) => ok.push(it),
            Err((id, Reject { reason, detail })) => rejected.push(Rejected { id, reason, detail }),
        }
    }
    let mut r = st.ingest.accept(ok).await?;
    r.rejected.splice(0..0, rejected);
    Ok(r)
}
