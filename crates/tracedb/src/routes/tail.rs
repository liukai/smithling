//! Live tail over Server-Sent Events (design §4.3).
//!
//! Per tab: subscribe first, then send `hello` (the browser loads its view after it). Keep only the
//! newest version of each run; send the first change after a quiet spell at once and batch the rest
//! every 100 ms. A tab that falls too far behind gets `resync` and reloads; the writer never waits.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Query as Q, State};
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracedb_ingest::Change;
use tracedb_model::{RunHeader, RunId, TraceId};

use crate::AppState;

pub const FLUSH_EVERY: Duration = Duration::from_millis(100);
pub const MAX_PENDING: usize = 5_000;

#[derive(Deserialize)]
pub struct TailQs {
    project: Option<String>,
    trace: Option<TraceId>,
}

enum Filter {
    /// Listed root runs, optionally of one project (the runs list).
    Roots(Option<String>),
    /// Every run of one trace (the trace view).
    Trace(TraceId),
}

impl Filter {
    fn matches(&self, h: &RunHeader) -> bool {
        match self {
            Filter::Roots(p) => {
                h.is_root && h.start_time.is_some() && h.name.is_some() && p.as_deref().is_none_or(|p| p == h.project)
            }
            Filter::Trace(t) => h.trace_id == *t,
        }
    }
}

pub async fn tail(State(st): State<AppState>, Q(q): Q<TailQs>) -> impl IntoResponse {
    let filter = match q.trace {
        Some(t) => Filter::Trace(t),
        None => Filter::Roots(q.project.filter(|p| !p.is_empty())),
    };
    let mut rx = st.ingest.subscribe();
    let server_seq = st.query.last_seq();
    let (tx, out) = mpsc::channel::<Result<Event, Infallible>>(16);
    tokio::spawn(async move {
        let send = |e: Event| tx.send(Ok(e));
        if send(
            Event::default()
                .event("hello")
                .json_data(json!({"server_seq": server_seq}))
                .unwrap(),
        )
        .await
        .is_err()
        {
            return;
        }
        let mut pending: IndexMap<RunId, Arc<RunHeader>> = IndexMap::new();
        let mut last_flush = Instant::now() - FLUSH_EVERY;
        let mut tick = tokio::time::interval(FLUSH_EVERY);
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Ok(Change { header, .. }) if filter.matches(&header) => {
                        let newer = pending.get(&header.run_id).is_none_or(|old| old.last_seq < header.last_seq);
                        if newer {
                            pending.insert(header.run_id, header);
                        }
                        if pending.len() > MAX_PENDING {
                            pending.clear();
                            if send(resync()).await.is_err() { return; }
                        } else if last_flush.elapsed() >= FLUSH_EVERY {
                            if send(flush(&mut pending)).await.is_err() { return; }
                            last_flush = Instant::now();
                        }
                    }
                    Ok(_) => {}
                    Err(RecvError::Lagged(_)) => {
                        pending.clear();
                        if send(resync()).await.is_err() { return; }
                    }
                    Err(RecvError::Closed) => return,
                },
                _ = tick.tick() => {
                    if !pending.is_empty() {
                        if send(flush(&mut pending)).await.is_err() { return; }
                        last_flush = Instant::now();
                    }
                }
                _ = tx.closed() => return,
            }
        }
    });
    Sse::new(ReceiverStream::new(out)).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

fn flush(pending: &mut IndexMap<RunId, Arc<RunHeader>>) -> Event {
    let runs: Vec<&RunHeader> = pending.values().map(|h| h.as_ref()).collect();
    let e = Event::default().event("runs").json_data(&runs).unwrap();
    pending.clear();
    e
}

fn resync() -> Event {
    Event::default().event("resync").data("{}")
}
