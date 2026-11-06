#![allow(dead_code)]

use std::net::SocketAddr;

use tracedb::{AppState, app};
use tracedb_ingest::{Ingest, IngestConfig};

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
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = IngestConfig::new(dir.path());
    tweak(&mut cfg);
    let (ingest, _) = Ingest::open(cfg).unwrap();
    let state = AppState { ingest };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Server { addr, state, dir }
}
