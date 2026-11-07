//! `POST /api/aggregate` (contracts/http-api.md): groups and metrics or a stats line, errors that name the
//! problem, the planted scenario's figures, and a damaged file skipped rather than a 500.

mod common;

use std::time::{Duration, Instant};

use common::*;
use serde_json::{Value, json};
use tracedb::scenario::{self, generate, load};
use tracedb_model::Ts;

async fn aggregate(server: &Server, body: Value) -> (u16, Value) {
    let r = reqwest::Client::new()
        .post(server.url("/api/aggregate"))
        .json(&body)
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.json().await.unwrap())
}

fn group<'a>(v: &'a Value, key: &str) -> &'a Value {
    v["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["key"][0] == key)
        .unwrap_or_else(|| panic!("no group {key}: {v}"))
}

async fn small(server: &Server) {
    let done = |mut r: Value, secs: u32, error: Option<&str>| {
        r["end_time"] = json!(format!("2026-11-05T03:00:{secs:02}.000000"));
        if let Some(e) = error {
            r["error"] = json!(e);
        }
        r
    };
    let mut kids = vec![];
    for i in 0..4u64 {
        let mut k = ls_child(1, 60, 10 + i, "p");
        k["name"] = json!(if i < 3 { "fetch_url" } else { "plan" });
        k["start_time"] = json!("2026-11-05T03:00:00.000000");
        kids.push(done(k, 1 + i as u32 * 10, (i == 0).then_some("HTTPError: 429")));
    }
    let mut running = ls_child(1, 60, 20, "p");
    running["name"] = json!("fetch_url");
    kids.push(running);
    kids.push(ls_root(1, "p", 60));
    post_batch(server, kids, vec![]).await;
}

#[tokio::test]
async fn groups_by_name_with_metrics_or_a_stats_line() {
    let server = start().await;
    small(&server).await;
    let body = json!({"project": "p", "is_root": false, "group_by": ["name"],
                      "metrics": ["count", "error_rate", "p95(latency)", "p50(latency)"]});
    let (s, v) = aggregate(&server, body).await;
    assert_eq!(s, 200, "{v}");
    assert!(v["query_id"].as_str().unwrap().starts_with("q-"));
    assert_eq!(v["partial"], false);
    assert_eq!((v["cut"].clone(), v["overlap"].clone()), (json!(false), json!(false)));
    for k in ["files_scanned", "clean_rows", "dirty_runs", "ms"] {
        assert!(v["stats"][k].is_u64(), "{k}: {v}");
    }
    let f = group(&v, "fetch_url");
    let c = &f["cells"];
    assert_eq!(
        (c["runs"].clone(), c["finished"].clone(), c["running"].clone()),
        (json!(4), json!(3), json!(1))
    );
    assert_eq!(c["errors"], 1);
    assert_eq!(c["error_rate"], 1.0 / 3.0);
    // Latencies 1 s, 11 s, 21 s: nearest rank in ms.
    assert_eq!(
        (c["p50"].clone(), c["p95"].clone(), c["n_latency"].clone()),
        (json!(11_000), json!(21_000), json!(3))
    );
    assert_eq!(c["cost_pico"], Value::Null, "not asked for");
    assert_eq!(f["where"], r#"eq(name, "fetch_url")"#);
    assert_eq!(v["totals"]["runs"], 5);

    let (s, w) = aggregate(
        &server,
        json!({"project": "p", "is_root": false, "stats": "by name | count, error_rate, p95(latency), p50(latency)"}),
    )
    .await;
    assert_eq!(s, 200, "{w}");
    assert_eq!(w["groups"], v["groups"]);
    assert_eq!(w["totals"], v["totals"]);

    // Totals only, and top N with (other).
    // The query fields of `/api/query` are accepted, the list-only ones ignored.
    let (_, t) = aggregate(
        &server,
        json!({"project": "p", "stats": "count", "count": true, "limit": 5}),
    )
    .await;
    assert_eq!(
        (t["totals"]["runs"].clone(), t["groups"].clone()),
        (json!(6), json!([]))
    );
    let (_, t) = aggregate(&server, json!({"project": "p", "stats": "by name | count | top 1"})).await;
    assert_eq!(t["cut"], true);
    let keys: Vec<&Value> = t["groups"].as_array().unwrap().iter().map(|g| &g["key"][0]).collect();
    assert_eq!(keys, [&json!("fetch_url"), &json!("(other)")]);
    assert_eq!(t["groups"][1]["cells"]["runs"], 2);
    assert_eq!(
        t["groups"][1]["where"],
        r#"and(exists(name), not(in(name, ["fetch_url"])))"#
    );
}

#[tokio::test]
async fn bad_requests_name_the_problem() {
    let server = start().await;
    let cases = [
        (json!({"filter": "eq(name", "metrics": ["count"]}), "bad_filter"),
        (json!({"metrics": ["count", "median(latency)"]}), "bad_stats"),
        (json!({"group_by": ["colour"], "metrics": ["count"]}), "bad_stats"),
        (
            json!({"group_by": ["name", "status", "model"], "metrics": ["count"]}),
            "bad_stats",
        ),
        (json!({"metrics": ["count"], "bucket": "7m"}), "bad_stats"),
        (
            json!({"metrics": ["count"], "compare": {"shift": "yesterday"}}),
            "bad_stats",
        ),
        (json!({"group_by": ["name"]}), "bad_stats"),
        (json!({"stats": "by name | count", "metrics": ["count"]}), "bad_stats"),
        (
            json!({"metrics": ["count"], "compare": {"filter": "eq(name"}}),
            "bad_stats",
        ),
        (json!({"metrics": ["count"], "compare": {"shift": "1d"}}), "bad_stats"),
        (
            json!({"metrics": ["avg(feedback.correctness)", "avg(feedback.helpfulness)"]}),
            "bad_stats",
        ),
        (
            json!({"group_by": ["metadata.a.b.c.d.e"], "metrics": ["count"]}),
            "bad_stats",
        ),
    ];
    for (body, code) in cases {
        let (s, v) = aggregate(&server, body.clone()).await;
        assert_eq!(
            (s, v["error"]["code"].as_str().unwrap_or("")),
            (400, code),
            "{body} → {v}"
        );
    }
    let (_, v) = aggregate(&server, json!({"filter": "eq(name", "metrics": ["count"]})).await;
    assert_eq!(v["error"]["slot"], "filter");
    let (_, v) = aggregate(&server, json!({"stats": "by name | count, p97(latency)"})).await;
    assert_eq!(v["error"]["code"], "bad_stats");
    assert_eq!(
        (v["error"]["start"].clone(), v["error"]["end"].clone()),
        (json!(17), json!(20)),
        "{v}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scenario_batch_a_by_name() {
    const MB: u64 = 1024 * 1024;
    let server = start_with(|c| c.limits.memtable_bytes = MB).await;
    let s = generate(7, Ts::now().0, MB);
    load(&format!("http://{}", server.addr), &s).await.unwrap();
    assert!(server.state.ingest.health().segments >= 2);
    let (status, v) = aggregate(
        &server,
        json!({"project": scenario::PROJECT, "filter": format!("has(tags, {:?})", scenario::BATCH_TAG),
               "group_by": ["name"], "metrics": ["count", "error_rate", "p95(latency)", "sum(cost)"]}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let f = &group(&v, "fetch_url")["cells"];
    assert_eq!(f["runs"], scenario::FETCH_URL_RUNS);
    assert_eq!(f["errors"], scenario::BATCH_A_FETCH_ERRORS);
    assert_eq!(f["error_rate"], 0.14);
    assert_eq!(f["p95"], scenario::BATCH_A_FETCH_P95_MS);
    assert_eq!(f["cost_pico"], Value::Null, "fetch_url isn't a model call");
    assert_eq!(v["totals"]["cost_pico"], scenario::BATCH_A_COST_PICO);
    assert_eq!(v["totals"]["no_price_runs"], scenario::BATCH_A_NO_PRICE_RUNS);
    assert!(v["stats"]["clean_rows"].as_u64().unwrap() > 0 && v["stats"]["dirty_runs"].as_u64().unwrap() > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_damaged_file_is_skipped_not_a_500() {
    let server = start_with(|c| {
        c.limits.memtable_bytes = 32 * 1024;
        c.limits.flush_age_us = 100_000;
    })
    .await;
    let n = 200u64;
    for chunk in (1..=n).collect::<Vec<_>>().chunks(20) {
        let posts: Vec<Value> = chunk
            .iter()
            .map(|&i| {
                let mut r = ls_root(i, "p", (i % 600) as u32);
                r["inputs"] = json!({"q": "x".repeat(400)});
                r
            })
            .collect();
        loop {
            let r = reqwest::Client::new()
                .post(server.url("/runs/batch"))
                .json(&json!({"post": posts}))
                .send()
                .await
                .unwrap();
            match r.status().as_u16() {
                202 => break,
                429 => tokio::time::sleep(Duration::from_millis(20)).await,
                s => panic!("upload: {s}"),
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while server.state.ingest.health().runs > 0 {
        assert!(Instant::now() < deadline, "not flushed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let lost = {
        let mem = server.state.ingest.mem();
        let m = mem.read().unwrap();
        assert!(m.version.segments.len() >= 2);
        m.version.segments[0].clone()
    };
    std::fs::remove_file(server.dir.path().join("data").join(&lost.core_path)).unwrap();
    let (s, v) = aggregate(&server, json!({"project": "p", "stats": "count"})).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["skipped_files"], json!([lost.id]));
    assert_eq!(v["totals"]["runs"], n - lost.row_count);
}

fn cells<'a>(groups: &'a Value, key: &str) -> &'a Value {
    &groups.as_array().unwrap().iter().find(|g| g["key"][0] == key).unwrap()["cells"]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scenario_batch_a_vs_b_side_by_side_and_overlaid() {
    const MB: u64 = 1024 * 1024;
    let server = start_with(|c| c.limits.memtable_bytes = MB).await;
    let s = generate(7, Ts::now().0, MB);
    load(&format!("http://{}", server.addr), &s).await.unwrap();
    let tag = |t: &str| format!("has(tags, {t:?})");
    let body = |view: &str| {
        json!({"project": scenario::PROJECT, "filter": tag(scenario::BATCH_TAG),
               "group_by": ["name"], "metrics": ["count", "error_rate", "p95(latency)", "sum(cost)"],
               "view": view, "compare": {"filter": tag(scenario::BATCH_B_TAG)}})
    };
    let (status, v) = aggregate(&server, body("table")).await;
    assert_eq!(status, 200, "{v}");
    let b = &v["compare"]["side_b"];
    let (fa, fb) = (cells(&v["groups"], "fetch_url"), cells(&b["groups"], "fetch_url"));
    assert_eq!(
        (fa["error_rate"].clone(), fa["p95"].clone()),
        (json!(0.14), json!(scenario::BATCH_A_FETCH_P95_MS))
    );
    assert_eq!(
        (fb["error_rate"].clone(), fb["p95"].clone()),
        (json!(0.02), json!(scenario::BATCH_B_FETCH_P95_MS))
    );
    assert_eq!(fb["errors"], scenario::BATCH_B_FETCH_ERRORS);
    assert_eq!(v["totals"]["cost_pico"], scenario::BATCH_A_COST_PICO);
    assert_eq!(b["totals"]["cost_pico"], scenario::BATCH_B_COST_PICO);
    // Same shape, no nested comparison; groups paired with a's.
    for k in ["query_id", "partial", "totals", "groups", "cut", "overlap", "stats"] {
        assert!(b.get(k).is_some(), "side_b.{k}");
    }
    assert!(b.get("compare").is_none());
    let keys = |g: &Value| {
        g.as_array()
            .unwrap()
            .iter()
            .map(|g| g["key"].clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        keys(&v["groups"]),
        keys(&b["groups"])
            .into_iter()
            .take(v["groups"].as_array().unwrap().len())
            .collect::<Vec<_>>()
    );

    let (status, v) = aggregate(&server, body("series")).await;
    assert_eq!(status, 200, "{v}");
    let (sa, sb) = (&v["series"], &v["compare"]["side_b"]["series"]);
    assert_eq!((sa["offset"].clone(), sb["offset"].clone()), (json!(true), json!(true)));
    assert_eq!(
        sa["buckets"].as_array().unwrap().len(),
        sb["buckets"].as_array().unwrap().len()
    );
    assert_eq!(
        (
            sa["buckets"][0]["offset_ms"].clone(),
            sb["buckets"][0]["offset_ms"].clone()
        ),
        (json!(0), json!(0))
    );
    assert!(sa["buckets"][0]["t"].is_null());

    // A shift: the last day against the day before (batch b against batch a), on offsets from each window's start.
    let (status, v) = aggregate(
        &server,
        json!({"project": scenario::PROJECT, "time": {"last": "24h"}, "group_by": ["name"], "metrics": ["count"],
               "view": "series", "bucket": "1h", "compare": {"shift": "1d"}}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let (sa, sb) = (&v["series"], &v["compare"]["side_b"]["series"]);
    assert_eq!((sa["offset"].clone(), sb["offset"].clone()), (json!(true), json!(true)));
    assert_eq!(sa["step_ms"], 3_600_000);
    assert_eq!(sa["buckets"].as_array().unwrap().len(), 25);
    assert_eq!(sb["buckets"].as_array().unwrap().len(), 25);
    assert!(v["compare"]["side_b"]["totals"]["runs"].as_u64().unwrap() > 0);
    // Distribution: fixed bins, a count per group per bin.
    let (status, v) = aggregate(
        &server,
        json!({"project": scenario::PROJECT, "filter": tag(scenario::BATCH_TAG), "group_by": ["name"],
               "metrics": ["count"], "view": "dist"}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let d = &v["dist"];
    let bins = d["bins_ms"].as_array().unwrap().len();
    assert!(bins > 0 && d["where_per_bin"].as_array().unwrap().len() == bins);
    assert_eq!(
        d["per_group"].as_array().unwrap().len(),
        v["groups"].as_array().unwrap().len()
    );
}
