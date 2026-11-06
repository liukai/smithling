//! Replays real LangSmith SDK traffic (recorded by scripts/record-sdk-fixtures.py) and compares the
//! stored runs with the recorder's independent expectation.

mod common;

use std::path::PathBuf;

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

        for line in std::fs::read_to_string(dir.join("requests.jsonl")).unwrap().lines() {
            let req: Value = serde_json::from_str(line).unwrap();
            let (method, path) = (req["method"].as_str().unwrap(), req["path"].as_str().unwrap());
            let url = server.url(path);
            let resp = match method {
                "GET" => http.get(url).send().await.unwrap(),
                "POST" => http.post(url).json(&req["body"]).send().await.unwrap(),
                "PATCH" => http.patch(url).json(&req["body"]).send().await.unwrap(),
                m => panic!("{version}: unexpected method {m}"),
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
        }

        let expected: Vec<Value> =
            serde_json::from_str(&std::fs::read_to_string(dir.join("expected-runs.json")).unwrap()).unwrap();
        let snap = server.state.ingest.snapshot();
        assert_eq!(
            server.state.ingest.health().runs,
            expected.len(),
            "{version}: run count"
        );
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
