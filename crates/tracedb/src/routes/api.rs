//! Read API: root list, projects, trace tree, single run.

use axum::Json;
use axum::extract::{Path, Query as Q, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tracedb_ingest::Include;
use tracedb_model::{RunId, Status, TraceId};
use tracedb_query::RootsParams;

use crate::AppState;
use crate::routes::errors::error;

#[derive(Deserialize)]
pub struct RootsQs {
    project: Option<String>,
    status: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
}

pub fn parse_status(s: &str) -> Option<Status> {
    match s {
        "running" => Some(Status::Running),
        "ok" => Some(Status::Ok),
        "error" => Some(Status::Error),
        _ => None,
    }
}

pub async fn roots(State(st): State<AppState>, Q(q): Q<RootsQs>) -> Response {
    let status = match q.status.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => match parse_status(s) {
            Some(v) => Some(v),
            None => return error(StatusCode::BAD_REQUEST, "bad_status", format!("unknown status {s:?}")),
        },
        None => None,
    };
    let params = RootsParams {
        project: q.project.filter(|p| !p.is_empty()),
        status,
        cursor: q.cursor,
        limit: q.limit,
    };
    match st.query.roots(&params) {
        Ok(page) => Json(page).into_response(),
        Err(e) => error(StatusCode::BAD_REQUEST, "bad_cursor", e.to_string()),
    }
}

pub async fn projects(State(st): State<AppState>) -> Response {
    Json(st.query.projects()).into_response()
}

pub async fn trace(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let Ok(id) = id.parse::<TraceId>() else {
        return error(StatusCode::BAD_REQUEST, "bad_id", "trace id is not a UUID");
    };
    match st.query.trace(id) {
        Some(t) => Json(t).into_response(),
        None => error(StatusCode::NOT_FOUND, "not_found", "no such trace"),
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
    match st.query.run(id, include) {
        Some(r) => Json(r).into_response(),
        None => error(StatusCode::NOT_FOUND, "not_found", "no such run"),
    }
}
