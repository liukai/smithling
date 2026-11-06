//! The embedded browser UI. Unknown non-API paths serve `index.html` so client-side routes work.

use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../ui/dist"]
struct Assets;

pub async fn static_or_index(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.starts_with("api/") {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    match Assets::get(path).filter(|_| !path.is_empty()) {
        Some(f) => ([(header::CONTENT_TYPE, f.metadata.mimetype().to_string())], f.data).into_response(),
        None => match Assets::get("index.html") {
            Some(f) => ([(header::CONTENT_TYPE, "text/html; charset=utf-8".to_string())], f.data).into_response(),
            None => (StatusCode::NOT_FOUND, "UI not built").into_response(),
        },
    }
}
