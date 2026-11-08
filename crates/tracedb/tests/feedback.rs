//! `POST /feedback`, `PATCH /feedback/{id}` and `GET /api/feedback` (contracts/http-api.md): stored in the catalog
//! whether or not the run exists, attached when it arrives, reloaded on start, kept through kill -9.

use tracedb_server::testkit as common;

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{Server, ls_root, post_batch, uid};
use serde_json::{Value, json};

fn fb(id: &str, run: &str, key: &str, score: Value) -> Value {
    json!({"id": id, "run_id": run, "key": key, "score": score, "comment": "gave up on 429",
           "feedback_source": {"type": "api", "metadata": {}}, "extend_trace_retention": true,
           "created_at": "2026-11-06T08:10:00Z"})
}

async fn post(base: &str, body: &Value) -> (u16, Value) {
    let r = reqwest::Client::new()
        .post(format!("{base}/feedback"))
        .json(body)
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
}

async fn patch(base: &str, id: &str, body: &Value) -> (u16, Value) {
    let r = reqwest::Client::new()
        .patch(format!("{base}/feedback/{id}"))
        .json(body)
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
}

async fn get(base: &str, path: &str) -> Value {
    let r = reqwest::get(format!("{base}{path}")).await.unwrap();
    assert_eq!(r.status(), 200, "GET {path}");
    r.json().await.unwrap()
}

fn base(s: &Server) -> String {
    s.url("")
}

#[tokio::test]
async fn post_stores_and_answers_the_row() {
    let s = common::start().await;
    let b = base(&s);
    let before = tracedb_model::Ts::now();
    let (st, row) = post(&b, &fb(&uid(901), &uid(1), "correctness", json!(0.0))).await;
    assert_eq!(st, 200, "{row}");
    assert_eq!(row["id"], uid(901));
    assert_eq!(row["run_id"], uid(1));
    assert_eq!(row["key"], "correctness");
    assert_eq!(row["score"], 0.0);
    assert_eq!(row["comment"], "gave up on 429");
    assert_eq!(row["feedback_source"], json!({"type": "api", "metadata": {}}));
    assert_eq!(row["created_at"], "2026-11-06T08:10:00.000000Z");
    let modified = tracedb_model::Ts::parse(row["modified_at"].as_str().unwrap()).unwrap();
    assert!(modified >= before, "modified_at is the write time");
    assert_eq!(row["extend_trace_retention"], true, "unknown fields kept: {row}");

    let got = get(&b, &format!("/api/feedback?run_id={}", uid(1))).await;
    assert_eq!(got["feedback"], json!([row]), "the stored row reads back as answered");
}

#[tokio::test]
async fn bad_feedback_is_400() {
    let s = common::start().await;
    let b = base(&s);
    for (body, what) in [
        (json!({"id": uid(1), "key": "k", "score": 1}), "run_id"),
        (json!({"id": uid(1), "run_id": uid(2), "score": 1}), "key"),
        (json!({"id": uid(1), "run_id": uid(2), "key": "", "score": 1}), "key"),
        (
            json!({"id": uid(1), "run_id": uid(2), "key": "k", "score": "high"}),
            "score",
        ),
        (json!({"id": uid(1), "run_id": "not-a-uuid", "key": "k"}), "run_id"),
        (json!(["not", "an", "object"]), "object"),
    ] {
        let (st, body) = post(&b, &body).await;
        assert_eq!(st, 400, "{what}: {body}");
        assert_eq!(body["error"]["code"], "bad_feedback", "{what}: {body}");
        assert!(
            body["error"]["message"].as_str().unwrap().contains(what),
            "{what}: {body}"
        );
    }
    assert_eq!(get(&b, "/api/feedback?keys=true").await["keys"], json!([]));
}

#[tokio::test]
async fn id_generated_retry_replaces_patch_updates() {
    let s = common::start().await;
    let b = base(&s);
    let (st, row) = post(&b, &json!({"run_id": uid(1), "key": "k", "score": true})).await;
    assert_eq!(st, 200, "{row}");
    let id = row["id"].as_str().unwrap().to_string();
    assert!(uuid::Uuid::try_parse(&id).is_ok(), "generated id {id}");
    assert_eq!(row["score"], 1.0, "a bool score counts as 1 / 0");

    // The same id again replaces the row (an SDK retry).
    let (st, _) = post(&b, &fb(&id, &uid(1), "k", json!(0.25))).await;
    assert_eq!(st, 200);
    let rows = get(&b, &format!("/api/feedback?run_id={}", uid(1))).await;
    assert_eq!(rows["feedback"].as_array().unwrap().len(), 1, "{rows}");
    assert_eq!(rows["feedback"][0]["score"], 0.25);

    let (st, row) = patch(&b, &id, &json!({"score": 0.5, "comment": "partly"})).await;
    assert_eq!(st, 200, "{row}");
    assert_eq!(row["score"], 0.5);
    assert_eq!(row["comment"], "partly");
    assert_eq!(row["key"], "k", "fields not given are kept");
    assert_eq!(row["created_at"], "2026-11-06T08:10:00.000000Z");
    assert_eq!(row["extend_trace_retention"], true);
    let rows = get(&b, &format!("/api/feedback?run_id={}", uid(1))).await;
    assert_eq!(rows["feedback"], json!([row]));

    let (st, _) = patch(&b, &id, &json!({"score": "high"})).await;
    assert_eq!(st, 400);
    let (st, body) = patch(&b, &uid(999), &json!({"score": 1})).await;
    assert_eq!(st, 404, "{body}");
    // A UUID id in another case is the same row; a null created_at doesn't reset it.
    let (st, row) = patch(&b, &id.to_uppercase(), &json!({"created_at": null})).await;
    assert_eq!(st, 200, "{row}");
    assert_eq!(row["id"], id);
    assert_eq!(row["created_at"], "2026-11-06T08:10:00.000000Z");

    let keys = get(&b, "/api/feedback?keys=true").await;
    assert_eq!(keys["keys"], json!([{"key": "k", "count": 1, "avg": 0.5}]));
}

#[tokio::test]
async fn feedback_before_its_run_attaches_when_it_arrives() {
    let s = common::start().await;
    let b = base(&s);
    assert_eq!(post(&b, &fb(&uid(901), &uid(1), "correctness", json!(0))).await.0, 200);
    assert_eq!(post(&b, &fb(&uid(902), &uid(1), "correctness", json!(1))).await.0, 200);
    assert_eq!(
        post(&b, &fb(&uid(903), &uid(2), "helpfulness", json!(null))).await.0,
        200
    );

    let health = get(&b, "/healthz").await;
    assert_eq!(health["orphan_feedback"], 3, "{health}");
    let orphans = get(&b, "/api/feedback?orphaned=true").await;
    assert_eq!(orphans["count"], 3, "{orphans}");

    post_batch(&s, vec![ls_root(1, "p", 0)], vec![]).await;
    let orphans = get(&b, "/api/feedback?orphaned=true").await;
    assert_eq!(orphans["count"], 1, "{orphans}");
    assert_eq!(orphans["feedback"][0]["id"], uid(903));
    assert_eq!(get(&b, "/healthz").await["orphan_feedback"], 1);
    let rows = get(&b, &format!("/api/feedback?run_id={}", uid(1))).await;
    let ids: Vec<&str> = rows["feedback"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [uid(901), uid(902)]);

    let keys = get(&b, "/api/feedback?keys=true").await;
    assert_eq!(
        keys["keys"],
        json!([{"key": "correctness", "count": 2, "avg": 0.5}, {"key": "helpfulness", "count": 1, "avg": null}])
    );
    // Scoped to a project: only feedback whose run is found there.
    let keys = get(&b, "/api/feedback?keys=true&project=p").await;
    assert_eq!(keys["keys"], json!([{"key": "correctness", "count": 2, "avg": 0.5}]));
    let keys = get(&b, "/api/feedback?keys=true&project=other").await;
    assert_eq!(keys["keys"], json!([]));
    // The run started on 2026-11-05: outside the last hour, inside the last 100,000 hours.
    assert_eq!(get(&b, "/api/feedback?keys=true&time=1h").await["keys"], json!([]));
    let keys = get(&b, "/api/feedback?keys=true&time=100000h").await;
    assert_eq!(keys["keys"], json!([{"key": "correctness", "count": 2, "avg": 0.5}]));
}

#[tokio::test]
async fn a_flushed_run_is_not_an_orphan() {
    let s = common::start_with(|c| c.limits.memtable_bytes = 1).await;
    let b = base(&s);
    assert_eq!(post(&b, &fb(&uid(901), &uid(1), "k", json!(1))).await.0, 200);
    post_batch(&s, vec![ls_root(1, "p", 0)], vec![]).await;
    post_batch(&s, vec![ls_root(2, "p", 1)], vec![]).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while s.state.ingest.health().segments == 0 || s.state.ingest.health().frozen {
        assert!(Instant::now() < deadline, "no flush");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(get(&b, "/healthz").await["orphan_feedback"], 0);
    assert_eq!(get(&b, "/api/feedback?orphaned=true").await["count"], 0);
}

#[tokio::test]
async fn get_api_feedback_needs_one_question() {
    let s = common::start().await;
    let r = reqwest::get(s.url("/api/feedback")).await.unwrap();
    assert_eq!(r.status(), 400);
    let r = reqwest::get(s.url("/api/feedback?run_id=nope")).await.unwrap();
    assert_eq!(r.status(), 400);
}

fn spawn(data: &std::path::Path) -> (Child, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tracedb"))
        .args(["serve", "--data", data.to_str().unwrap(), "--port", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let url = line
        .split_whitespace()
        .find(|w| w.starts_with("http://"))
        .expect("listening line")
        .to_string();
    (child, url)
}

/// Feedback sent before its run is confirmed only once the catalog has it: kill -9 right after the reply,
/// restart, and it is still there (an orphan); the run then arrives and it attaches, across another kill.
#[tokio::test]
async fn feedback_sent_before_its_run_survives_kill_9() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, url) = spawn(dir.path());
    let (st, _) = post(&url, &fb(&uid(901), &uid(1), "correctness", json!(0))).await;
    assert_eq!(st, 200);
    let (st, _) = patch(&url, &uid(901), &json!({"comment": "patched"})).await;
    assert_eq!(st, 200);
    child.kill().unwrap();
    child.wait().unwrap();

    let (mut child, url) = spawn(dir.path());
    assert_eq!(get(&url, "/healthz").await["orphan_feedback"], 1);
    let orphans = get(&url, "/api/feedback?orphaned=true").await;
    assert_eq!(orphans["count"], 1);
    assert_eq!(orphans["feedback"][0]["comment"], "patched");
    let r = reqwest::Client::new()
        .post(format!("{url}/runs/batch"))
        .json(&json!({"post": [ls_root(1, "p", 0)]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    assert_eq!(get(&url, "/healthz").await["orphan_feedback"], 0);
    child.kill().unwrap();
    child.wait().unwrap();

    let (mut child, url) = spawn(dir.path());
    assert_eq!(get(&url, "/healthz").await["orphan_feedback"], 0);
    let rows = get(&url, &format!("/api/feedback?run_id={}", uid(1))).await;
    assert_eq!(rows["feedback"][0]["id"], uid(901), "{rows}");
    let keys = get(&url, "/api/feedback?keys=true&project=p").await;
    assert_eq!(keys["keys"], json!([{"key": "correctness", "count": 1, "avg": 0.0}]));
    child.kill().unwrap();
    child.wait().unwrap();
}

/// Through the read API: filter, average and group by feedback, and the field catalog (Story 5 scenario 3).
#[tokio::test]
async fn feedback_is_filterable_averaged_and_listed() {
    let s = common::start().await;
    let b = base(&s);
    let mut fail = ls_root(2, "p", 1);
    fail["error"] = json!("HTTPError: 429");
    fail["end_time"] = json!("2026-11-05T02:01:30.000000");
    post_batch(&s, vec![ls_root(1, "p", 0), fail, ls_root(3, "p", 2)], vec![]).await;
    for (id, run, score) in [(901, 1, 1.0), (902, 2, 0.0), (903, 2, 0.5)] {
        assert_eq!(
            post(&b, &fb(&uid(id), &uid(run), "correctness", json!(score))).await.0,
            200
        );
    }
    // Stored as sent, but no filter can name it.
    assert_eq!(post(&b, &fb(&uid(904), &uid(1), "user rating", json!(1))).await.0, 200);
    let http = reqwest::Client::new();
    let api = |path: &str, body: Value| {
        let req = http.post(format!("{b}{path}")).json(&body);
        async move {
            let r = req.send().await.unwrap();
            let st = r.status().as_u16();
            (st, r.json::<Value>().await.unwrap())
        }
    };
    for f in [
        "lt(feedback.correctness, 0.5)",
        "and(eq(feedback_key, \"correctness\"), lt(feedback_score, 0.5))",
    ] {
        let (st, page) = api("/api/query", json!({"filter": f, "project": "p"})).await;
        assert_eq!(st, 200, "{page}");
        let ids: Vec<&str> = page["runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["run_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, [uid(2)], "{f}: run 2's mean is 0.25");
    }
    let (st, agg) = api(
        "/api/aggregate",
        json!({"project": "p", "stats": "by status | count, avg(feedback.correctness)"}),
    )
    .await;
    assert_eq!(st, 200, "{agg}");
    assert_eq!(
        agg["totals"]["feedback_avg"], 0.625,
        "mean of the run means 1 and 0.25: {agg}"
    );
    assert_eq!(agg["totals"]["feedback_n"], 2);
    let (st, agg) = api(
        "/api/aggregate",
        json!({"project": "p", "stats": "by feedback_key | count"}),
    )
    .await;
    assert_eq!(st, 200, "{agg}");
    let groups: Vec<(Value, Value, Value)> = agg["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| (g["key"][0].clone(), g["cells"]["runs"].clone(), g["where"].clone()))
        .collect();
    assert_eq!(
        groups,
        [
            (json!("correctness"), json!(2), json!("exists(feedback.correctness)")),
            (json!("user rating"), json!(1), Value::Null),
            (json!("(none)"), json!(1), Value::Null),
        ]
    );
    let (st, agg) = api(
        "/api/aggregate",
        json!({"project": "p", "stats": "by feedback_key, status | count"}),
    )
    .await;
    assert_eq!(st, 200, "{agg}");
    for g in agg["groups"].as_array().unwrap() {
        let Some(w) = g["where"].as_str() else {
            assert!(
                ["(none)", "user rating", "(other)"].contains(&g["key"][0].as_str().unwrap()),
                "{g}"
            );
            continue;
        };
        let (st, page) = api("/api/query", json!({"project": "p", "filter": w, "count": true})).await;
        assert_eq!(
            (st, page["stats"]["matched"].clone()),
            (200, g["cells"]["runs"].clone()),
            "{w}"
        );
    }
    assert_eq!(agg["overlap"], true);
    let fields = get(&b, "/api/fields?prefix=feedback").await;
    assert_eq!(
        fields["fields"],
        json!([{"path": "feedback.correctness", "types": [{"type": "number", "count": 2}], "complete": true}])
    );
}
