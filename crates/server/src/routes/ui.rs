//! The browser UI: the copy embedded at build time, or files from `--ui-dir` (M2-FR-24). Unknown non-API
//! paths serve `index.html` so client-side routes work.

use std::path::{Component, Path};

use axum::extract::State;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

use crate::AppState;

#[derive(RustEmbed)]
#[folder = "$OUT_DIR/ui"]
struct Assets;

pub async fn static_or_index(State(st): State<AppState>, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.starts_with("api/") {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    match &st.ui_dir {
        Some(dir) => from_dir(dir, path).await,
        None => embedded(path),
    }
}

fn embedded(path: &str) -> Response {
    match Assets::get(path).filter(|_| !path.is_empty()) {
        Some(f) => ([(header::CONTENT_TYPE, f.metadata.mimetype().to_string())], f.data).into_response(),
        None => match Assets::get("index.html") {
            Some(f) => ([(header::CONTENT_TYPE, "text/html; charset=utf-8".to_string())], f.data).into_response(),
            None => (StatusCode::NOT_FOUND, "UI not built").into_response(),
        },
    }
}

async fn from_dir(dir: &Path, path: &str) -> Response {
    let rel = Path::new(path);
    let safe = rel.components().all(|c| matches!(c, Component::Normal(_)));
    if safe && !path.is_empty() {
        let file = dir.join(rel);
        if let Ok(bytes) = tokio::fs::read(&file).await {
            let mime = mime_guess::from_path(&file).first_or_octet_stream().to_string();
            return ([(header::CONTENT_TYPE, mime)], bytes).into_response();
        }
    }
    match tokio::fs::read(dir.join("index.html")).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "text/html; charset=utf-8".to_string())], bytes).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, format!("no index.html in {}", dir.display())).into_response(),
    }
}
