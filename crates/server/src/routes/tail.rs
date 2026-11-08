//! Live views over Server-Sent Events: framing only; the query service keeps the query live (design §4.5).

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Query as Q, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use smithling_model::TraceId;
use smithling_query::{QuerySpec, TailMsg};
use tokio_stream::StreamExt;

use crate::AppState;
use crate::routes::api::{BadRequest, QueryBody, listed_roots};
use crate::routes::errors::error;

#[derive(Deserialize)]
pub struct TailQs {
    /// URL-encoded `POST /api/query` body.
    q: Option<String>,
    project: Option<String>,
    trace: Option<TraceId>,
}

/// `?q=` is the query; M1's `?project=` (unless `q` has one) / `?trace=` narrow it, or alone give M1's runs
/// list / trace view (which ignores `project`).
fn spec(qs: TailQs) -> Result<QuerySpec, BadRequest> {
    let m1_trace = qs.q.is_none();
    let mut spec = match qs.q {
        Some(q) => serde_json::from_str::<QueryBody>(&q)
            .map_err(|e| {
                BadRequest(Box::new(error(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    format!("q: {e}"),
                )))
            })?
            .to_spec()?,
        None if qs.trace.is_some() => QuerySpec::default(),
        None => listed_roots(),
    };
    if spec.project.is_none() && !(m1_trace && qs.trace.is_some()) {
        spec.project = qs.project.filter(|p| !p.is_empty());
    }
    if let Some(t) = qs.trace {
        spec.traces = Some([t].into());
    }
    Ok(spec)
}

fn event(m: TailMsg) -> Event {
    let e = Event::default();
    match m {
        TailMsg::Hello { server_seq } => e.event("hello").json_data(json!({"server_seq": server_seq})),
        TailMsg::Runs(runs) => e.event("runs").json_data(runs),
        TailMsg::Left(ids) => e.event("left").json_data(ids),
        TailMsg::Resync => Ok(e.event("resync").data("{}")),
    }
    .expect("tail messages serialize")
}

pub async fn tail(State(st): State<AppState>, Q(qs): Q<TailQs>) -> Response {
    let q = match spec(qs) {
        Ok(q) => q,
        Err(BadRequest(r)) => return *r,
    };
    let events = st.query.tail(q).map(|m| Ok::<_, Infallible>(event(m)));
    Sse::new(events)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}
