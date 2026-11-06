//! HTTP status codes for whole-request failures. The SDK ignores `202` bodies, so anything the
//! client must act on is a status code (contracts/http-api.md).

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tracedb_ingest::AcceptError;

pub fn error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({"error": {"code": code, "message": message.into()}})),
    )
        .into_response()
}

fn with_retry_after(mut r: Response, secs: u32) -> Response {
    r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(secs));
    r
}

pub fn accept_error(e: &AcceptError) -> Response {
    match e {
        AcceptError::Busy => with_retry_after(error(StatusCode::TOO_MANY_REQUESTS, "busy", e.to_string()), 1),
        AcceptError::Capacity => error(StatusCode::INSUFFICIENT_STORAGE, "capacity", e.to_string()),
        AcceptError::Storage(_) => {
            with_retry_after(error(StatusCode::SERVICE_UNAVAILABLE, "storage", e.to_string()), 5)
        }
        AcceptError::Closed => error(StatusCode::SERVICE_UNAVAILABLE, "closed", e.to_string()),
    }
}
