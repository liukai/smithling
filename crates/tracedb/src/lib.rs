//! The `tracedb` server: HTTP routes over the ingest and query services, plus the embedded UI.

pub mod langsmith_map;
pub mod routes;
pub mod seed;

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::extract::{DefaultBodyLimit, State};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use tracedb_ingest::{Budget, Health, Ingest, IngestConfig, OpenStats};
use tracedb_query::Query;

pub const MAX_BODY_BYTES: usize = 50 * 1024 * 1024;

#[derive(Clone)]
pub struct AppState {
    pub ingest: Ingest,
    pub query: Query,
    /// Serve UI files from this folder instead of the embedded copy.
    pub ui_dir: Option<PathBuf>,
}

impl AppState {
    pub fn new(ingest: Ingest) -> AppState {
        let query = Query::new(ingest.snapshot());
        AppState {
            ingest,
            query,
            ui_dir: None,
        }
    }

    pub fn with_ui_dir(mut self, dir: Option<PathBuf>) -> AppState {
        self.ui_dir = dir;
        self
    }
}

#[derive(Clone, Debug)]
pub struct ServeConfig {
    pub data_dir: PathBuf,
    pub addr: SocketAddr,
    pub budget: Budget,
    pub ui_dir: Option<PathBuf>,
}

pub fn app(state: AppState) -> Router {
    use routes::ingest as ing;
    Router::new()
        .route("/info", get(ing::info))
        .route("/runs/batch", post(ing::runs_batch))
        .route("/runs", post(ing::create_run))
        .route("/runs/{run_id}", patch(ing::update_run))
        .route("/runs/multipart", post(ing::multipart))
        .route("/feedback", post(ing::feedback))
        .route("/v1/events", post(ing::native_events))
        .route("/healthz", get(healthz))
        .route("/api/roots", get(routes::api::roots))
        .route("/api/projects", get(routes::api::projects))
        .route("/api/traces/{trace_id}", get(routes::api::trace))
        .route("/api/runs/{run_id}", get(routes::api::run))
        .route("/api/tail", get(routes::tail::tail))
        .fallback(routes::ui::static_or_index)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

async fn healthz(State(st): State<AppState>) -> Json<Health> {
    Json(st.ingest.health())
}

/// Opens the data folder (replaying the WAL) before binding, so the port only opens when ready.
pub fn open(cfg: &ServeConfig) -> anyhow::Result<(AppState, OpenStats)> {
    let mut icfg = IngestConfig::new(&cfg.data_dir);
    icfg.budget = cfg.budget;
    let (ingest, stats) = Ingest::open(icfg)?;
    Ok((AppState::new(ingest).with_ui_dir(cfg.ui_dir.clone()), stats))
}

pub async fn serve(
    cfg: ServeConfig,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let data_dir = cfg.data_dir.clone();
    let (state, stats) = {
        let cfg = cfg.clone();
        tokio::task::spawn_blocking(move || open(&cfg)).await??
    };
    tracing::info!(
        data = %data_dir.display(),
        runs = stats.runs,
        replayed = stats.replayed_events,
        replay_ms = stats.replay_ms,
        truncated_bytes = stats.truncated_bytes,
        "data folder ready"
    );
    let listener = tokio::net::TcpListener::bind(cfg.addr).await?;
    println!(
        "tracedb listening on http://{}   data: {}",
        listener.local_addr()?,
        data_dir.display()
    );
    axum::serve(listener, app(state))
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}
