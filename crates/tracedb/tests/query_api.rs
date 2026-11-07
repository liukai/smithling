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
    assert_eq!(s, 200, "search is answered: {v}");
    let (s, v) = query(
        &server,
        json!({"tree_filter": "and(eq(name, \"x\"), not(search(outputs, \"rate limit\")))"}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = query(&server, json!({"filter": "search(\"\")"})).await;
    assert_eq!(
        (s, v["error"]["code"].as_str(), v["error"]["slot"].as_str()),
        (400, Some("bad_filter"), Some("filter"))
    );
    let (s, _) = query(
        &server,
        json!({"filter": "and(exists(metadata.prompt_version), exists(name))"}),
    )
    .await;
    assert_eq!(s, 200);
    let (s, v) = query(
        &server,
        json!({"filter": "and(eq(feedback_key, \"correctness\"), lt(feedback_score, 0.5))"}),
    )
    .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (400, Some("unsupported")),
        "feedback.<key> waits for the feedback index"
    );
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

async fn get(server: &Server, path: &str) -> (u16, Value) {
    let r = reqwest::get(server.url(path)).await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap())
}

#[tokio::test]
async fn field_catalog_lists_paths_and_values() {
    let server = start().await;
    let runs: Vec<Value> = (1..=3)
        .map(|n| {
            let mut r = ls_root(n, "p", n as u32);
            r["extra"] = json!({"metadata": {"prompt_version": if n == 3 { "v6" } else { "v7" }}});
            r
        })
        .collect();
    post_batch(&server, runs, vec![]).await;

    let (s, v) = get(&server, "/api/fields?project=p&prefix=meta").await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        v,
        json!({"fields": [{"path": "metadata.prompt_version", "types": [{"type": "string", "count": 3}],
                           "complete": true}], "complete": true})
    );
    let (s, v) = get(&server, "/api/fields?project=nope&time=12h").await;
    assert_eq!((s, v["fields"].as_array().map(Vec::len)), (200, Some(0)));

    let (s, v) = get(
        &server,
        "/api/fields/values?project=p&path=metadata.prompt_version&prefix=v",
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        v,
        json!({"values": [{"value": "v7", "count": 2}, {"value": "v6", "count": 1}], "other_count": 0,
               "approximate": false})
    );

    let (s, v) = get(&server, "/api/fields/values?project=p").await;
    assert_eq!((s, v["error"]["code"].as_str()), (400, Some("bad_request")));
    let (s, v) = get(&server, "/api/fields?time=yesterday").await;
    assert_eq!((s, v["error"]["code"].as_str()), (400, Some("bad_time")));
}

fn ids_of(v: &Value) -> Vec<String> {
    v["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["run_id"].as_str().unwrap().to_string())
        .collect()
}

/// `match` (contracts/http-api.md): only with a search, only on page rows; offsets are characters.
#[tokio::test]
async fn search_returns_excerpts_and_search_stats() {
    let server = start().await;
    let mut a = ls_root(1, "p", 1);
    a["inputs"] = json!({"url": "https://api.example.com/v1"});
    let mut b = ls_root(2, "p", 2);
    b["inputs"] = json!({"q": "café ☕ — 429 Too Many Requests"});
    let mut c = ls_root(3, "p", 3);
    c["inputs"] = json!({"q": "too many cooks"});
    post_batch(&server, vec![a, b, c], vec![]).await;
    let mut end = json!({"id": uid(1), "trace_id": uid(1),
        "dotted_order": ls_root(1, "p", 1)["dotted_order"].clone(),
        "end_time": "2026-11-05T02:01:31.000000",
        "error": "HTTPError: 429 Too Many Requests", "outputs": {"body": "429 Too Many Requests"}});
    post_batch(&server, vec![], vec![end.take()]).await;

    let (s, v) = query(&server, json!({"filter": "search(\"429 too many requests\")"})).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(ids_of(&v), vec![uid(2), uid(1)], "newest first");
    let m = &v["runs"][1]["match"];
    assert_eq!(
        (
            m["path"].as_str(),
            m["excerpt"].as_str(),
            m["start"].as_u64(),
            m["end"].as_u64()
        ),
        (
            Some("error"),
            Some("HTTPError: 429 Too Many Requests"),
            Some(11),
            Some(32)
        )
    );
    let m = &v["runs"][0]["match"];
    assert_eq!(m["path"], "inputs.q");
    let excerpt: Vec<char> = m["excerpt"].as_str().unwrap().chars().collect();
    let (start, end) = (
        m["start"].as_u64().unwrap() as usize,
        m["end"].as_u64().unwrap() as usize,
    );
    assert_eq!(
        excerpt[start..end].iter().collect::<String>(),
        "429 Too Many Requests",
        "character offsets"
    );
    for k in ["index_files", "scan_files", "candidates", "payload_rows", "cache_hits"] {
        assert!(v["stats"][k].is_u64(), "stats.{k}: {}", v["stats"]);
    }
    assert!(v["stats"]["candidates"].as_u64().unwrap() >= 2);

    // A side, a page of one: the excerpt is only on the page's run.
    let (_, v) = query(&server, json!({"filter": "search(inputs, \"too many\")", "limit": 1})).await;
    assert_eq!(ids_of(&v), vec![uid(3)]);
    assert_eq!(v["runs"][0]["match"]["excerpt"], "too many cooks");
    assert!(v["next_cursor"].is_string());
    // No search, no match.
    let (_, v) = query(&server, json!({"filter": "eq(name, \"root\")"})).await;
    assert!(v["runs"].as_array().unwrap().iter().all(|r| r.get("match").is_none()));
    // Under `not`, nothing to show.
    let (_, v) = query(&server, json!({"filter": "not(search(\"429\"))"})).await;
    assert_eq!(ids_of(&v), vec![uid(3)]);
    assert!(v["runs"][0].get("match").is_none());
}

/// Story 1 scenario 1: a search, a tag and a condition on any run in the trace, together.
#[tokio::test]
async fn search_with_a_tag_and_a_tree_condition() {
    let server = start().await;
    let mut posts = Vec::new();
    for t in 1..=4u64 {
        let mut root = ls_root(t * 10, "p", t as u32);
        root["tags"] = json!([if t % 2 == 0 { "batch=a" } else { "batch=b" }]);
        let mut kid = ls_child(t * 10, t as u32, t * 10 + 1, "p");
        kid["tags"] = root["tags"].clone();
        kid["outputs"] = json!({"body": if t <= 3 { "got 429 Too Many Requests" } else { "fine" }});
        if t != 2 {
            kid["error"] = json!("HTTPError: 429 Too Many Requests");
        }
        posts.extend([root, kid]);
    }
    post_batch(&server, posts, vec![]).await;
    let (s, v) = query(
        &server,
        json!({
            "filter": "and(search(outputs, \"429 Too Many Requests\"), has(tags, \"batch=b\"))",
            "tree_filter": "neq(error, null)",
        }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(ids_of(&v), vec![uid(31), uid(11)]);
    // The search as the tree condition: roots of traces with a matching run.
    let (_, v) = query(
        &server,
        json!({"is_root": true, "tree_filter": "search(error, \"too many\")", "filter": "has(tags, \"batch=a\")"}),
    )
    .await;
    assert_eq!(ids_of(&v), vec![uid(40)]);
}

/// `post_batch`, retrying while a small memtable makes the server ask to slow down.
async fn post_patient(server: &Server, post: Vec<Value>, patch: Vec<Value>) {
    loop {
        let r = reqwest::Client::new()
            .post(server.url("/runs/batch"))
            .json(&json!({"post": post, "patch": patch}))
            .send()
            .await
            .unwrap();
        match r.status().as_u16() {
            202 => return,
            429 | 503 => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
            s => panic!("{s}"),
        }
    }
}

/// Story 1 scenario 2: the late update's text is found with the merged run, from memory and files alike.
#[tokio::test]
async fn late_update_text_is_found_across_memtable_and_files() {
    let server = start_with(|c| c.limits.memtable_bytes = 4096).await;
    let roots = |from: u64, to: u64| -> Vec<Value> {
        (from..=to)
            .map(|n| {
                let mut r = ls_root(n, "p", n as u32);
                r["inputs"] = json!({"url": format!("https://example.com/{n}"), "pad": "x".repeat(150)});
                r
            })
            .collect()
    };
    let flushed = |server: &Server| {
        let h = server.state.ingest.health();
        !h.frozen && h.runs < 10
    };
    for n in (1..=40u64).step_by(5) {
        post_patient(&server, roots(n, n + 4), vec![]).await;
    }
    while !flushed(&server) {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let patches: Vec<Value> = (1..=40u64)
        .filter(|n| n % 4 == 0)
        .map(|n| {
            json!({"id": uid(n), "trace_id": uid(n), "dotted_order": ls_root(n, "p", n as u32)["dotted_order"].clone(),
                "end_time": "2026-11-05T03:00:00.000000", "outputs": {"body": format!("rate limited: 429 Too Many Requests #{n}")},
                "error": "HTTPError: 429 Too Many Requests"})
        })
        .collect();
    post_patient(&server, vec![], patches).await;
    let want: Vec<String> = (1..=40u64).rev().filter(|n| n % 4 == 0).map(uid).collect();
    for in_files in [false, true] {
        if in_files {
            for n in (41..=60u64).step_by(5) {
                post_patient(&server, roots(n, n + 4), vec![]).await;
            }
            while !flushed(&server) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
        let body = json!({"filter": "search(outputs, \"429 Too Many Requests\")", "limit": 200});
        let (s, v) = query(&server, body).await;
        assert_eq!(s, 200, "{v}");
        assert_eq!(ids_of(&v), want, "updates in files: {in_files}");
        assert!(v["stats"]["index_files"].as_u64().unwrap() >= 1, "{}", v["stats"]);
        assert!(
            v["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .all(|w| w["kind"] != "not_indexed")
        );
    }
}

/// SC-01: on the scenario data, the phrase finds exactly the 31 planted runs, all `fetch_url`, with excerpts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scenario_phrase_finds_the_31() {
    use tracedb::scenario::{self, generate, load};
    const MB: u64 = 1024 * 1024;
    let server = start_with(|c| c.limits.memtable_bytes = MB).await;
    let s = generate(7, tracedb_model::Ts::now().0, MB);
    load(&format!("http://{}", server.addr), &s).await.unwrap();
    let filter = format!("search(\"{}\")", scenario::PHRASE);
    let (s, v) = query(&server, json!({"filter": filter, "limit": 200})).await;
    assert_eq!(s, 200, "{v}");
    let runs = v["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 31, "{}", v["stats"]);
    assert!(runs.iter().all(|r| r["name"] == "fetch_url"));
    assert!(
        runs.iter()
            .all(|r| r["match"]["excerpt"].as_str().unwrap().contains(scenario::PHRASE))
    );
    assert!(v["stats"]["index_files"].as_u64().unwrap() >= 1, "{}", v["stats"]);
    eprintln!("stats: {}", v["stats"]);
    let (_, v) = query(&server, json!({"filter": filter, "count": true, "limit": 1})).await;
    assert_eq!(v["stats"]["matched"], 31);
}
