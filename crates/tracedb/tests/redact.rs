//! Credentials never reach logs: passwords in connection URLs and `AWS_*` values are hidden at startup.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tracedb::{ServeConfig, storage_summary};
use tracedb_ingest::Limits;

const SECRETS: [&str; 4] = ["hunter2", "topsecret", "AKIAEXAMPLE", "sekret"];

#[test]
fn passwords_with_slashes_and_query_strings_are_hidden() {
    let r = tracedb_storage::redact;
    assert_eq!(r("s3://AKIA:ab/sekret@bucket/p"), "s3://AKIA:***@bucket/p");
    assert_eq!(
        r("postgres://tdb:hunter2@db/x?application_name=a@b"),
        "postgres://tdb:***@db/x?application_name=a@b"
    );
    assert_eq!(
        r("see s3://bucket/prefix and sqlite:///tmp/m.sqlite"),
        "see s3://bucket/prefix and sqlite:///tmp/m.sqlite"
    );
}

#[test]
fn summary_hides_passwords_and_aws_values() {
    let cfg = ServeConfig {
        data_dir: "/tmp/d".into(),
        addr: "127.0.0.1:0".parse().unwrap(),
        storage: Some("s3://key:sekret@bucket/prefix".into()),
        metastore: Some("postgres://tdb:hunter2@db:5432/tracedb".into()),
        limits: Limits::default(),
        ui_dir: None,
        prices: None,
    };
    let env = [
        ("AWS_SECRET_ACCESS_KEY", "topsecret"),
        ("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE"),
        ("HOME", "/home/x"),
    ]
    .map(|(k, v)| (k.to_string(), v.to_string()));
    let s = storage_summary(&cfg, env);
    for secret in SECRETS {
        assert!(!s.contains(secret), "{secret} in {s}");
    }
    assert!(s.contains("postgres://tdb:***@db:5432/tracedb"), "{s}");
    assert!(
        s.contains("AWS_ACCESS_KEY_ID=***") && s.contains("AWS_SECRET_ACCESS_KEY=***"),
        "{s}"
    );
    assert!(!s.contains("HOME"), "{s}");
}

#[test]
fn serve_startup_and_its_failure_never_print_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tracedb"))
        .args(["serve", "--port", "0", "--data"])
        .arg(dir.path())
        .args(["--metastore", "postgres://tdb:hunter2@nohost.invalid:5432/tracedb"])
        .env("AWS_SECRET_ACCESS_KEY", "topsecret")
        .env("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE")
        .env("RUST_LOG", "debug")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > Duration::from_secs(60) {
            child.kill().unwrap();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "no postgres there: {text}");
    for secret in SECRETS {
        assert!(!text.contains(secret), "{secret} in\n{text}");
    }
    assert!(text.contains("tdb:***@nohost.invalid:5432"), "{text}");
    assert!(
        text.contains("opening: storage=default metastore=postgres://tdb:***@nohost.invalid"),
        "{text}"
    );
    assert!(
        text.contains("AWS_ACCESS_KEY_ID=***") && text.contains("AWS_SECRET_ACCESS_KEY=***"),
        "{text}"
    );
}
