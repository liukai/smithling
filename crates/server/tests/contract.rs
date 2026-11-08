//! Replays real LangSmith SDK traffic (recorded by scripts/record-sdk-fixtures.py) and compares the
//! stored runs with the recorder's independent expectation.

use tracedb_server::testkit as common;

use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::Value;
use tracedb_ingest::Include;

fn fixture_dirs() -> Vec<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    assert!(!dirs.is_empty(), "no fixtures under {}", root.display());
    dirs
}

#[tokio::test]
async fn recorded_sdk_traffic_is_fully_accepted_and_merged() {
    for dir in fixture_dirs() {
        let version = dir.file_name().unwrap().to_string_lossy().to_string();
        let server = common::start().await;
        let http = reqwest::Client::new();

        let mut multipart = 0;
        let mut feedback_calls = 0;
        for line in std::fs::read_to_string(dir.join("requests.jsonl")).unwrap().lines() {
            let req: Value = serde_json::from_str(line).unwrap();
            let (method, path) = (req["method"].as_str().unwrap(), req["path"].as_str().unwrap());
            let url = server.url(path);
            let resp = match (method, req["body_b64"].as_str()) {
                ("GET", _) => http.get(url).send().await.unwrap(),
                ("POST", Some(b64)) => {
                    multipart += 1;
                    let mut r = http.post(url).body(BASE64.decode(b64).unwrap());
                    for (k, v) in req["headers"].as_object().unwrap() {
                        r = r.header(k.as_str(), v.as_str().unwrap());
                    }
                    r.send().await.unwrap()
                }
                ("POST", None) => http.post(url).json(&req["body"]).send().await.unwrap(),
                ("PATCH", None) => http.patch(url).json(&req["body"]).send().await.unwrap(),
                (m, _) => panic!("{version}: unexpected method {m}"),
            };
            let status = resp.status();
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            assert!(status.is_success(), "{version}: {method} {path} -> {status} {body}");
            if path.starts_with("/runs") {
                assert_eq!(
                    body["rejected"],
                    serde_json::json!([]),
                    "{version}: {path} rejected items: {body}"
                );
            }
            if path.starts_with("/feedback") {
                feedback_calls += 1;
            }
        }

        assert_eq!(
            multipart > 0,
            version.contains("multipart"),
            "{version}: multipart requests replayed: {multipart}"
        );
        let expected: Vec<Value> =
            serde_json::from_str(&std::fs::read_to_string(dir.join("expected-runs.json")).unwrap()).unwrap();
        let snap = server.state.ingest.snapshot();
        assert_eq!(
            server.state.ingest.health().runs,
            expected.len(),
            "{version}: run count"
        );
        let expected_feedback: Vec<Value> = match std::fs::read_to_string(dir.join("expected-feedback.json")) {
            Ok(s) => serde_json::from_str(&s).unwrap(),
            Err(_) => Vec::new(),
        };
        assert_eq!(
            feedback_calls > 0,
            !expected_feedback.is_empty(),
            "{version}: feedback calls replayed: {feedback_calls}"
        );
        let mut stored_feedback = 0u64;
        for want in &expected_feedback {
            let run = want["run_id"].as_str().unwrap();
            let rows: Value = http
                .get(server.url(&format!("/api/feedback?run_id={run}")))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let got = rows["feedback"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["id"] == want["id"])
                .unwrap_or_else(|| panic!("{version}: feedback {} not stored: {rows}", want["id"]));
            for (k, v) in want.as_object().unwrap() {
                assert_eq!(&got[k], v, "{version}: feedback {} field {k}", want["id"]);
            }
            stored_feedback += 1;
        }
        let health = server.state.ingest.health();
        assert_eq!(health.orphan_feedback, 0, "{version}: every feedback's run arrived");
        let keys: Value = http
            .get(server.url("/api/feedback?keys=true"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let total: u64 = keys["keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k["count"].as_u64().unwrap())
            .sum();
        assert_eq!(total, stored_feedback, "{version}: feedback rows stored (SC-08)");
        for want in &expected {
            let id = want["run_id"].as_str().unwrap().parse().unwrap();
            let got = snap
                .run(id, Include::default())
                .unwrap_or_else(|| panic!("{version}: missing run {id}"))
                .header;
            let got = serde_json::to_value(&got).unwrap();
            for k in [
                "trace_id",
                "parent_run_id",
                "name",
                "run_type",
                "project",
                "address",
                "status",
            ] {
                assert_eq!(got[k], want[k], "{version}: run {id} field {k}");
            }
            for (k, size) in [("has_inputs", "inputs"), ("has_outputs", "outputs")] {
                let has = got["payload_sizes"][size].as_u64().unwrap() > 0;
                assert_eq!(has, want[k].as_bool().unwrap(), "{version}: run {id} {k}");
            }
        }
    }
}
