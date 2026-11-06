mod common;

use common::*;
use serde_json::Value;

async fn get(server: &Server, path: &str) -> (u16, Value) {
    let r = reqwest::get(server.url(path)).await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap())
}

#[tokio::test]
async fn trace_is_headers_only_in_dotted_order() {
    let server = start().await;
    let mut c1 = ls_child(1, 5, 11, "p");
    c1["inputs"] = serde_json::json!({"url": "https://example.com", "pad": "x".repeat(40_000)});
    post_batch(&server, vec![ls_root(1, "p", 5), c1, ls_child(1, 5, 12, "p")], vec![]).await;

    let (code, t) = get(&server, &format!("/api/traces/{}", uid(1))).await;
    assert_eq!(code, 200);
    assert_eq!(t["run_count"], 3);
    assert_eq!(t["has_running"], true);
    assert_eq!(t["root"]["run_id"], uid(1));
    let runs = t["runs"].as_array().unwrap();
    assert_eq!(runs[0]["run_id"], uid(1), "root first");
    for r in runs {
        assert!(
            r.get("inputs").is_none() && r.get("outputs").is_none(),
            "no payloads in the tree"
        );
    }
    assert!(
        runs.iter()
            .any(|r| r["payload_sizes"]["inputs"].as_u64().unwrap() > 40_000)
    );
}

#[tokio::test]
async fn run_payloads_only_when_included() {
    let server = start().await;
    let mut r = ls_root(1, "p", 5);
    r["inputs"] = serde_json::json!({"q": "hello"});
    r["outputs"] = serde_json::json!({"a": "world"});
    post_batch(&server, vec![r], vec![]).await;
    let id = uid(1);
    let (_, bare) = get(&server, &format!("/api/runs/{id}")).await;
    assert!(bare.get("inputs").is_none());
    let (_, some) = get(&server, &format!("/api/runs/{id}?include=inputs")).await;
    assert_eq!(some["inputs"]["q"], "hello");
    assert!(some.get("outputs").is_none());
    assert_eq!(get(&server, &format!("/api/runs/{id}?include=bogus")).await.0, 400);
    assert_eq!(get(&server, &format!("/api/runs/{}", uid(99))).await.0, 404);
    assert_eq!(get(&server, "/api/runs/not-a-uuid").await.0, 400);
}

#[tokio::test]
async fn orphan_child_shows_in_its_trace_before_the_parent_arrives() {
    let server = start().await;
    post_batch(&server, vec![ls_child(1, 5, 11, "p")], vec![]).await;
    let (code, t) = get(&server, &format!("/api/traces/{}", uid(1))).await;
    assert_eq!(code, 200);
    assert_eq!(t["run_count"], 1);
    assert!(t["root"].is_null());
    post_batch(&server, vec![ls_root(1, "p", 5)], vec![]).await;
    let (_, t) = get(&server, &format!("/api/traces/{}", uid(1))).await;
    assert_eq!(t["run_count"], 2);
    assert_eq!(t["root"]["child_count"], 1);
}
