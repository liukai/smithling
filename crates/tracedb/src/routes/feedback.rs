//! `GET /api/feedback`: one run's feedback, orphans, or per-key counts (contracts/http-api.md).

use std::collections::HashSet;

use axum::Json;
use axum::extract::{Query as Q, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use tracedb_ingest::FeedbackLookup;
use tracedb_model::RunId;
use tracedb_model::filter::{Expr, Field, Path, TimeAt, Value};
use tracedb_query::{MAX_LIMIT, QueryError, QuerySpec, TimeRange};

use crate::AppState;
use crate::routes::errors::{error, feedback_error};

#[derive(Deserialize)]
pub struct FeedbackQs {
    run_id: Option<String>,
    #[serde(default)]
    orphaned: bool,
    #[serde(default)]
    keys: bool,
    project: Option<String>,
    /// `"12h"`: the last such span.
    time: Option<String>,
}

fn bad(m: impl Into<String>) -> Response {
    error(StatusCode::BAD_REQUEST, "bad_request", m)
}

/// The runs among `runs` found in `project` and the time span, read in `in(run_id, […])` queries.
async fn in_scope(
    st: &AppState,
    runs: Vec<RunId>,
    project: Option<&str>,
    time: TimeRange,
) -> Result<HashSet<RunId>, QueryError> {
    let mut found = HashSet::new();
    for chunk in runs.chunks(MAX_LIMIT) {
        let ids: Vec<Value> = chunk.iter().map(|r| Value::Str(r.to_string())).collect();
        let spec = QuerySpec {
            project: project.map(str::to_string),
            time,
            filter: Some(Expr::In {
                path: Path::Field(Field::RunId),
                values: ids,
            }),
            limit: Some(chunk.len()),
            ..QuerySpec::default()
        };
        found.extend(st.query.query(&spec).await?.runs.iter().map(|r| r.header.run_id));
    }
    Ok(found)
}

pub async fn list(State(st): State<AppState>, Q(q): Q<FeedbackQs>) -> Response {
    let rows = |runs| async { st.ingest.feedback_for_runs(runs).await };
    if let Some(r) = &q.run_id {
        let Ok(run) = r.parse::<RunId>() else {
            return bad(format!("run_id must be a UUID, got {r:?}"));
        };
        return match rows(vec![run]).await {
            Ok(rows) => Json(json!({"feedback": rows})).into_response(),
            Err(e) => feedback_error(&e),
        };
    }
    if q.orphaned {
        return match rows(st.ingest.feedback_runs_missing()).await {
            Ok(rows) => Json(json!({"count": rows.len(), "feedback": rows})).into_response(),
            Err(e) => feedback_error(&e),
        };
    }
    if q.keys {
        let project = q.project.as_deref().filter(|p| !p.is_empty());
        let mut time = TimeRange::default();
        if let Some(t) = q.time.as_deref().filter(|t| !t.is_empty()) {
            match TimeAt::parse(&format!("-{}", t.trim_start_matches('-'))) {
                Some(at) => time.from = Some(at),
                None => return bad(format!("time: expected e.g. \"12h\", got {t:?}")),
            }
        }
        let fb = st.ingest.feedback();
        let scope = if project.is_some() || time.from.is_some() {
            match in_scope(&st, fb.run_ids(), project, time).await {
                Ok(s) => Some(s),
                Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
            }
        } else {
            None
        };
        let keys = fb.keys(&|r| scope.as_ref().is_none_or(|s| s.contains(r)));
        return Json(json!({"keys": keys})).into_response();
    }
    bad("ask for one of run_id=…, orphaned=true or keys=true")
}
