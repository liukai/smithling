//! `GET /api/aggregate/stream`: an `estimate`, then the `result` (or an `error`), over Server-Sent Events
//! (framing as `routes/tail.rs`). The stream owns the computation's cancel token: closing the connection drops
//! it and stops the scan.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Query as Q, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use smithling_query::aggregate::AggMsg;
use tokio_stream::StreamExt;

use crate::AppState;
use crate::routes::api::{AggBody, BadRequest, agg_request, take_skipped};
use crate::routes::errors::error;

#[derive(Deserialize)]
pub struct StreamQs {
    /// URL-encoded `POST /api/aggregate` body.
    q: Option<String>,
}

fn result(e: Event, mut r: smithling_query::AggResult) -> Event {
    let skipped = take_skipped(&mut r);
    let mut v = serde_json::to_value(r).expect("results serialize");
    if !skipped.is_empty() {
        v["skipped_files"] = json!(skipped);
    }
    e.json_data(v).expect("json serializes")
}

fn event(m: AggMsg) -> Event {
    let e = Event::default();
    match m {
        AggMsg::Estimate(r) => result(e.event("estimate"), r),
        AggMsg::Result(r) => result(e.event("result"), r),
        AggMsg::Error { query_id, error } => e
            .event("error")
            .json_data(json!({"query_id": query_id, "code": "query_failed", "message": error.to_string()}))
            .expect("json serializes"),
    }
}

/// A bad request as one `error` event with the `POST /api/aggregate` error body (no query was started, so
/// `query_id` is `null`).
async fn refused(r: Response) -> Response {
    let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap_or_default();
    let mut err = serde_json::from_slice::<Value>(&body).map_or(Value::Null, |v| v["error"].clone());
    if let Some(o) = err.as_object_mut() {
        o.insert("query_id".into(), Value::Null);
    }
    let once = Event::default().event("error").json_data(err).expect("json serializes");
    Sse::new(tokio_stream::once(Ok::<_, Infallible>(once))).into_response()
}

pub async fn stream(State(st): State<AppState>, Q(qs): Q<StreamQs>) -> Response {
    let bad = |m: String| BadRequest(Box::new(error(StatusCode::BAD_REQUEST, "bad_request", m)));
    let parsed =
        qs.q.ok_or_else(|| bad("q: the URL-encoded POST /api/aggregate body is required".into()))
            .and_then(|q| serde_json::from_str::<AggBody>(&q).map_err(|e| bad(format!("q: {e}"))))
            .and_then(|b| agg_request(&b));
    let (spec, agg) = match parsed {
        Ok(x) => x,
        Err(BadRequest(r)) => return refused(*r).await,
    };
    let events = st
        .query
        .aggregate_stream(spec, agg, st.prices.clone(), st.agg_stream)
        .map(|m| Ok::<_, Infallible>(event(m)));
    Sse::new(events)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}
