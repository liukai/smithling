//! In-process server and request helpers for integration tests (feature `testkit`).

use std::net::SocketAddr;

use crate::{AppState, app};
use smithling_ingest::{Ingest, IngestConfig};

pub struct Server {
    pub addr: SocketAddr,
    pub state: AppState,
    pub dir: tempfile::TempDir,
}

impl Server {
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }
}

pub async fn start() -> Server {
    start_with(|_| {}).await
}

pub async fn start_with(tweak: impl FnOnce(&mut IngestConfig)) -> Server {
    start_state(tweak, |_| {}).await
}

pub async fn start_state(tweak: impl FnOnce(&mut IngestConfig), state_tweak: impl FnOnce(&mut AppState)) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = IngestConfig::new(dir.path());
    tweak(&mut cfg);
    let (ingest, _) = Ingest::open(cfg).unwrap();
    let mut state = AppState::new(ingest);
    state_tweak(&mut state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Server { addr, state, dir }
}

use std::time::Instant;

use futures_util::StreamExt;
use serde_json::{Value, json};

pub fn uid(n: u64) -> String {
    format!("00000000-0000-4000-8000-{n:012}")
}

/// A LangSmith-shaped root run starting `minute` minutes after 02:00.
pub fn ls_root(n: u64, project: &str, minute: u32) -> Value {
    let id = uid(n);
    json!({
        "id": id, "trace_id": id,
        "dotted_order": format!("20261105T{:02}{:02}00000000Z{id}", 2 + minute / 60, minute % 60),
        "session_name": project, "name": "root", "run_type": "chain",
        "start_time": format!("2026-11-05T{:02}:{:02}:00.000000", 2 + minute / 60, minute % 60),
    })
}

pub fn ls_child(trace: u64, root_minute: u32, n: u64, project: &str) -> Value {
    let (t, id) = (uid(trace), uid(n));
    json!({
        "id": id, "trace_id": t, "parent_run_id": t,
        "dotted_order": format!("20261105T{:02}{:02}00000000Z{t}.20261105T{:02}{:02}01000000Z{id}",
            2 + root_minute / 60, root_minute % 60, 2 + root_minute / 60, root_minute % 60),
        "session_name": project, "name": format!("step-{n}"), "run_type": "tool",
        "start_time": format!("2026-11-05T{:02}:{:02}:01.000000", 2 + root_minute / 60, root_minute % 60),
    })
}

pub async fn post_batch(server: &Server, post: Vec<Value>, patch: Vec<Value>) -> Value {
    let r = reqwest::Client::new()
        .post(server.url("/runs/batch"))
        .json(&json!({"post": post, "patch": patch}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    r.json().await.unwrap()
}

pub struct SseMsg {
    pub event: String,
    pub data: Value,
    pub at: Instant,
}

/// Opens an SSE stream and forwards parsed messages; returns after `hello` arrives.
pub async fn sse(server: &Server, path: &str) -> tokio::sync::mpsc::UnboundedReceiver<SseMsg> {
    let resp = reqwest::get(server.url(path)).await.unwrap();
    assert_eq!(resp.status(), 200);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut stream = resp.bytes_stream();
    tokio::spawn(async move {
        let mut buf = String::new();
        while let Some(Ok(chunk)) = stream.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(end) = buf.find("\n\n") {
                let frame: String = buf.drain(..end + 2).collect();
                let (mut event, mut data) = (String::from("message"), String::new());
                for line in frame.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        event = v.trim().to_string();
                    } else if let Some(v) = line.strip_prefix("data:") {
                        data.push_str(v.trim());
                    }
                }
                if data.is_empty() {
                    continue;
                }
                let _ = tx.send(SseMsg {
                    event,
                    data: serde_json::from_str(&data).unwrap_or(Value::Null),
                    at: Instant::now(),
                });
            }
        }
    });
    let hello = rx.recv().await.unwrap();
    assert_eq!(hello.event, "hello");
    rx
}
