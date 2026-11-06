//! `POST /api/query` (contracts/http-api.md): results, errors that point at the problem, paging that stays
//! stable while data arrives, and M1's `/api/roots` answered by the same planner.

mod common;

use std::collections::HashSet;

use common::*;
use serde_json::{Value, json};

async fn query(server: &Server, body: Value) -> (u16, Value) {
    let r = reqwest::Client::new()
        .post(server.url("/api/query"))
        .json(&body)
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.json().await.unwrap())
}

#[tokio::test]
async fn filters_tree_conditions_and_columns() {
    let server = start().await;
    let mut a = ls_root(1, "p", 1);
    a["tags"] = json!(["batch=x"]);
    a["extra"] = json!({"metadata": {"prompt_version": "v7"}});
    let b = ls_root(2, "p", 2);
    let mut kid = ls_child(2, 2, 21, "p");
    kid["error"] = json!("boom");
    kid["end_time"] = json!("2026-11-05T02:03:00.000000");
    post_batch(&server, vec![a, b, kid], vec![]).await;

    let (s, v) = query(
        &server,
        json!({"filter": "has(tags, \"batch=x\")", "columns": ["metadata.prompt_version"]}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["runs"].as_array().unwrap().len(), 1);
    assert_eq!(v["runs"][0]["columns"]["metadata.prompt_version"], "v7");
    assert!(v["stats"]["ms"].is_number());

    let (_, v) = query(
        &server,
        json!({"is_root": true, "tree_filter": "eq(status, \"error\")"}),
    )
    .await;
    let ids: Vec<&str> = v["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["run_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![uid(2).as_str()], "the root whose child errored");
}

#[tokio::test]
async fn errors_name_the_slot_and_the_place() {
    let server = start().await;
    let (s, v) = query(&server, json!({"tree_filter": "eq(staus, \"error\")"})).await;
    assert_eq!(s, 400);
    assert_eq!(v["error"]["code"], "bad_filter");
    assert_eq!(v["error"]["slot"], "tree_filter");
    assert_eq!(
        (v["error"]["start"].as_u64(), v["error"]["end"].as_u64()),
        (Some(3), Some(8))
    );
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("did you mean \"status\"")
    );

    let (s, v) = query(&server, json!({"filter": "search(\"x\")"})).await;
    assert_eq!((s, v["error"]["code"].as_str()), (400, Some("unsupported")));
    let (s, v) = query(&server, json!({"cursor": "nope"})).await;
    assert_eq!((s, v["error"]["code"].as_str()), (400, Some("bad_cursor")));
    let (s, v) = query(&server, json!({"bogus": 1})).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (400, Some("bad_request")),
        "unknown fields are rejected, not ignored"
    );
}

#[tokio::test]
async fn pages_have_no_repeats_or_gaps_while_runs_arrive() {
    let server = start().await;
    post_batch(&server, (1..=30).map(|n| ls_root(n, "p", n as u32)).collect(), vec![]).await;
    let mut body = json!({"is_root": true, "limit": 7});
    let mut seen = Vec::new();
    let mut extra = 100;
    loop {
        let (_, v) = query(&server, body.clone()).await;
        seen.extend(
            v["runs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["run_id"].as_str().unwrap().to_string()),
        );
        // New, newer runs arrive between pages; they sort ahead of the cursor and never shift it.
        post_batch(&server, vec![ls_root(extra, "p", 59)], vec![]).await;
        extra += 1;
        match v["next_cursor"].as_str() {
            Some(c) => body["cursor"] = json!(c),
            None => break,
        }
    }
    let unique: HashSet<_> = seen.iter().collect();
    assert_eq!(unique.len(), seen.len(), "no repeats");
    for n in 1..=30 {
        assert!(unique.contains(&uid(n)), "run {n} missing");
    }
}

#[tokio::test]
async fn m1_roots_endpoint_still_answers() {
    let server = start().await;
    post_batch(&server, vec![ls_root(1, "p", 1), ls_root(2, "q", 2)], vec![]).await;
    let v: Value = reqwest::get(server.url("/api/roots?project=p"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["runs"].as_array().unwrap().len(), 1);
    assert!(v.get("next_cursor").is_some());
}
