//! The `tracedb` server: HTTP routes over the ingest and query services, plus the embedded UI.

pub mod cli_query;
pub mod demo;
pub mod langsmith_map;
pub mod routes;
pub mod scenario;
pub mod seed;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, State};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use tracedb_ingest::{Health, Ingest, IngestConfig, Limits, OpenStats};
use tracedb_model::cost::Prices;
use tracedb_query::Query;

/// `512MB`, `2GB` → bytes (for `--memtable`).
pub fn parse_size(s: &str) -> Result<u64, String> {
    tracedb_ingest::budget::parse_bytes(s).ok_or_else(|| format!("invalid size {s:?}; use e.g. 512MB or 2GB"))
}

/// Where data goes, for the startup log: passwords in URLs and every `AWS_*` value hidden.
pub fn storage_summary(cfg: &ServeConfig, env: impl IntoIterator<Item = (String, String)>) -> String {
    let show = |v: &Option<String>| v.as_deref().map_or("default".to_string(), tracedb_storage::redact);
    let mut aws: Vec<String> = env
        .into_iter()
        .filter(|(k, _)| k.starts_with("AWS_"))
        .map(|(k, _)| format!("{k}=***"))
        .collect();
    aws.sort();
    let mut s = format!("storage={} metastore={}", show(&cfg.storage), show(&cfg.metastore));
    if !aws.is_empty() {
        s.push_str(&format!(" aws={}", aws.join(",")));
    }
    s
}

pub const MAX_BODY_BYTES: usize = 50 * 1024 * 1024;

#[derive(Clone)]
pub struct AppState {
    pub ingest: Ingest,
    pub query: Query,
    /// Serve UI files from this folder instead of the embedded copy.
    pub ui_dir: Option<PathBuf>,
    pub prices: Arc<Prices>,
}

impl AppState {
    pub fn new(ingest: Ingest) -> AppState {
        let query = Query::new(ingest.clone());
        AppState {
            ingest,
            query,
            ui_dir: None,
            prices: Arc::new(Prices::embedded()),
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
    /// Data files: a folder or `s3://bucket/prefix` (default `<data>/data`).
    pub storage: Option<String>,
    /// Catalog: `sqlite://…` or `postgres://…` (default `<data>/meta.sqlite`).
    pub metastore: Option<String>,
    pub limits: Limits,
    pub ui_dir: Option<PathBuf>,
    /// Price list replacing embedded entries by name (default `<data>/prices.toml` if present).
    pub prices: Option<PathBuf>,
}

pub fn app(state: AppState) -> Router {
    use routes::ingest as ing;
    Router::new()
        .route("/info", get(ing::info))
        .route("/runs/batch", post(ing::runs_batch))
        .route("/runs", post(ing::create_run))
        .route("/runs/{run_id}", patch(ing::update_run))
        .route("/runs/multipart", post(routes::multipart::runs_multipart))
        .route("/feedback", post(ing::feedback))
        .route("/v1/events", post(ing::native_events))
        .route("/healthz", get(healthz))
        .route("/api/query", post(routes::api::query))
        .route("/api/roots", get(routes::api::roots))
        .route("/api/projects", get(routes::api::projects))
        .route("/api/fields", get(routes::api::fields))
        .route("/api/fields/values", get(routes::api::field_values))
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

/// Embedded prices, then `--prices` or `<data>/prices.toml`; read once at start, no network.
pub fn load_prices(cfg: &ServeConfig) -> anyhow::Result<Prices> {
    let mut prices = Prices::embedded();
    let user = cfg.data_dir.join("prices.toml");
    match &cfg.prices {
        Some(path) => prices.merge_user(path)?,
        None if user.exists() => prices.merge_user(&user)?,
        None => {}
    }
    Ok(prices)
}

/// Opens the data folder (replaying the WAL) before binding, so the port only opens when ready.
pub fn open(cfg: &ServeConfig) -> anyhow::Result<(AppState, OpenStats)> {
    let prices = load_prices(cfg)?;
    let mut icfg = IngestConfig::new(&cfg.data_dir);
    icfg.storage = cfg.storage.clone();
    icfg.metastore = cfg.metastore.clone();
    icfg.limits = cfg.limits;
    // Tests only: roll WAL files small so a flush has whole files to trim.
    if let Some(b) = std::env::var("TRACEDB_TEST_WAL_ROLL").ok().and_then(|v| v.parse().ok()) {
        icfg.wal_roll_bytes = b;
    }
    let (ingest, stats) = Ingest::open(icfg)?;
    let mut state = AppState::new(ingest).with_ui_dir(cfg.ui_dir.clone());
    state.prices = Arc::new(prices);
    Ok((state, stats))
}

pub async fn serve(
    cfg: ServeConfig,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let data_dir = cfg.data_dir.clone();
    tracing::info!(data = %data_dir.display(), "opening: {}", storage_summary(&cfg, std::env::vars()));
    let (state, stats) = {
        let cfg = cfg.clone();
        tokio::task::spawn_blocking(move || open(&cfg)).await??
    };
    tracing::info!(
        data = %data_dir.display(),
        runs = stats.runs,
        segments = stats.segments,
        flushed_seq = stats.flushed_seq,
        replayed = stats.replayed_events,
        orphans_removed = stats.orphans_removed,
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
