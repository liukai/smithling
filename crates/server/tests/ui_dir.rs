//! `--ui-dir`: UI files come from a folder instead of the embedded copy (M2-FR-24).

use smithling_ingest::{Ingest, IngestConfig};
use smithling_server::{AppState, app};

#[tokio::test]
async fn serves_files_and_index_fallback_from_the_folder() {
    let data = tempfile::tempdir().unwrap();
    let ui = tempfile::tempdir().unwrap();
    std::fs::write(ui.path().join("index.html"), "<h1>dev build</h1>").unwrap();
    std::fs::create_dir(ui.path().join("assets")).unwrap();
    std::fs::write(ui.path().join("assets/app.js"), "console.log(1)").unwrap();
    std::fs::write(data.path().join("secret.txt"), "no").unwrap();

    let (ingest, _) = Ingest::open(IngestConfig::new(data.path())).unwrap();
    let state = AppState::new(ingest).with_ui_dir(Some(ui.path().to_path_buf()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app(state)).await.unwrap() });
    let get = |p: &str| {
        let url = format!("http://{addr}{p}");
        async move {
            let r = reqwest::get(url).await.unwrap();
            let ct = r.headers()[reqwest::header::CONTENT_TYPE].to_str().unwrap().to_string();
            (r.status().as_u16(), ct, r.text().await.unwrap())
        }
    };

    let (s, ct, body) = get("/assets/app.js").await;
    assert_eq!((s, body.as_str()), (200, "console.log(1)"));
    assert!(ct.contains("javascript"), "{ct}");
    assert_eq!(
        get("/trace/abc").await.2,
        "<h1>dev build</h1>",
        "client routes get index.html"
    );
    let (_, _, body) = get("/../secret.txt").await;
    assert_ne!(body, "no", "paths can't leave the folder");
    assert_eq!(get("/%2e%2e/secret.txt").await.2, "<h1>dev build</h1>");
}
