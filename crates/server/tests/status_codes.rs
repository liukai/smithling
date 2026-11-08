//! Whole-request failures use status codes the SDK acts on (contracts/http-api.md).

use tracedb_server::testkit as common;

use axum::http::StatusCode;
use serde_json::{Value, json};
use tracedb_ingest::AcceptError;
use tracedb_server::routes::errors::accept_error;

fn root(n: u32, project: &str) -> Value {
    let id = format!("00000000-0000-4000-8000-{n:012}");
    json!({
        "id": id, "trace_id": id, "dotted_order": format!("20261105T021400000000Z{id}"),
        "session_name": project, "name": "r", "run_type": "chain", "start_time": "2026-11-05T02:14:00.000000"
    })
}

#[test]
fn accept_errors_map_to_retryable_or_final_codes() {
    let cases = [
        (AcceptError::Busy, StatusCode::TOO_MANY_REQUESTS, Some("1")),
        (
            AcceptError::Storage("disk full".into()),
            StatusCode::SERVICE_UNAVAILABLE,
            Some("5"),
        ),
    ];
    for (err, status, retry) in cases {
        let r = accept_error(&err);
        assert_eq!(r.status(), status, "{err}");
        assert_eq!(
            r.headers().get("retry-after").map(|v| v.to_str().unwrap()),
            retry,
            "{err}"
        );
    }
}

#[tokio::test]
async fn unsupported_paths_are_explicit() {
    let server = common::start().await;
    let http = reqwest::Client::new();
    let fb = http
        .post(server.url("/feedback"))
        .json(&json!({"run_id": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        fb.status(),
        400,
        "bad feedback is final; a 501 would be re-raised into agent code"
    );
    let mp = http.post(server.url("/runs/multipart")).body("x").send().await.unwrap();
    assert_eq!(mp.status(), 400, "a 404 makes the SDK fall back, a 415 drops the batch");
}

#[tokio::test]
async fn info_has_every_batch_key_the_sdk_reads() {
    let server = common::start().await;
    let info: Value = reqwest::get(server.url("/info")).await.unwrap().json().await.unwrap();
    assert_eq!(info["version"], "", "empty version skips the SDK's version warnings");
    let cfg = &info["batch_ingest_config"];
    assert_eq!(cfg["use_multipart_endpoint"], true);
    for k in [
        "scale_up_qsize_trigger",
        "scale_up_nthreads_limit",
        "scale_down_nempty_trigger",
        "size_limit",
        "size_limit_bytes",
    ] {
        assert!(cfg[k].is_number(), "missing {k}: the SDK reads it without a default");
    }
}

#[tokio::test]
async fn oversized_body_is_413() {
    let server = common::start().await;
    let big = vec![b'x'; tracedb_server::MAX_BODY_BYTES + 1];
    let r = reqwest::Client::new()
        .post(server.url("/runs/batch"))
        .header("content-type", "application/json")
        .body(big)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413);
}

#[tokio::test]
async fn bad_items_are_reported_and_the_rest_accepted() {
    let server = common::start().await;
    let mut wrong_tags = root(2, "p");
    wrong_tags["tags"] = json!("not-a-list");
    let r: Value = reqwest::Client::new()
        .post(server.url("/runs/batch"))
        .json(&json!({"post": [root(1, "p"), wrong_tags, {"id": "nope"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["accepted"], 1);
    let reasons: Vec<&str> = r["rejected"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["reason"].as_str().unwrap())
        .collect();
    assert_eq!(reasons, vec!["invalid_field", "invalid_uuid"]);
}

#[tokio::test]
async fn healthz_has_every_contract_field() {
    let server = common::start().await;
    let h: Value = reqwest::get(server.url("/healthz"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    for k in [
        "ok",
        "wal_ok",
        "flush_ok",
        "memtable_bytes",
        "frozen",
        "unflushed_wal_bytes",
        "segments",
        "last_flush_at",
        "corrupt_segments",
        "orphan_feedback",
        "cache_bytes",
        "dedup_keys",
    ] {
        assert!(h.get(k).is_some(), "/healthz lacks {k}: {h}");
    }
    assert_eq!(h["ok"], true);
    assert_eq!(h["frozen"], false);
    assert_eq!(h["corrupt_segments"], json!([]));
    assert_eq!(h["orphan_feedback"], 0);
}
