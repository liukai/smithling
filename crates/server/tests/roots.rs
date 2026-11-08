use tracedb_server::testkit as common;

use common::*;
use serde_json::Value;

async fn get(server: &Server, path: &str) -> (u16, Value) {
    let r = reqwest::get(server.url(path)).await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap())
}

#[tokio::test]
async fn pages_newest_first_without_repeats_while_runs_arrive() {
    let server = start().await;
    post_batch(&server, (1..=30).map(|n| ls_root(n, "p", n as u32)).collect(), vec![]).await;

    let mut seen: Vec<String> = Vec::new();
    let mut path = "/api/roots?project=p&limit=10".to_string();
    let mut newest = 1000;
    loop {
        let (code, page) = get(&server, &path).await;
        assert_eq!(code, 200);
        seen.extend(
            page["runs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["run_id"].as_str().unwrap().to_string()),
        );
        newest += 1;
        post_batch(&server, vec![ls_root(newest, "p", 59)], vec![]).await;
        match page["next_cursor"].as_str() {
            Some(c) => path = format!("/api/roots?project=p&limit=10&cursor={c}"),
            None => break,
        }
    }
    let expected: Vec<String> = (1..=30).rev().map(uid).collect();
    assert_eq!(
        seen, expected,
        "newest first; runs added during paging don't shift or repeat earlier pages"
    );
}

#[tokio::test]
async fn filters_limit_and_bad_cursors() {
    let server = start().await;
    let mut done = ls_root(1, "p", 1);
    done["error"] = "boom".into();
    post_batch(
        &server,
        vec![ls_root(1, "p", 1), ls_root(2, "p", 2), ls_root(3, "q", 3)],
        vec![done],
    )
    .await;

    let (_, p) = get(&server, "/api/roots?project=p").await;
    assert_eq!(p["runs"].as_array().unwrap().len(), 2);
    let (_, errs) = get(&server, "/api/roots?status=error").await;
    assert_eq!(
        errs["runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["run_id"].clone())
            .collect::<Vec<_>>(),
        vec![Value::from(uid(1))]
    );
    let (_, projects) = get(&server, "/api/projects").await;
    assert_eq!(
        projects,
        serde_json::json!([{"name": "p", "runs": 2}, {"name": "q", "runs": 1}])
    );

    assert_eq!(get(&server, "/api/roots?status=weird").await.0, 400);
    assert_eq!(get(&server, "/api/roots?cursor=garbage").await.0, 400);
    let (_, first) = get(&server, "/api/roots?project=p&limit=1").await;
    let c = first["next_cursor"].as_str().unwrap();
    assert_eq!(
        get(&server, &format!("/api/roots?project=q&limit=1&cursor={c}"))
            .await
            .0,
        400,
        "a cursor is tied to its filter"
    );
}

#[tokio::test]
async fn limit_is_capped_at_200() {
    let server = start().await;
    for chunk in (1..=250u64).collect::<Vec<_>>().chunks(100) {
        post_batch(
            &server,
            chunk.iter().map(|n| ls_root(*n, "p", (*n % 60) as u32)).collect(),
            vec![],
        )
        .await;
    }
    let (_, page) = get(&server, "/api/roots?limit=500").await;
    assert_eq!(page["runs"].as_array().unwrap().len(), 200);
    assert!(page["next_cursor"].is_string());
}
