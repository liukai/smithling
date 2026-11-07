//! Storage trait tests, run against every backend at hand: local files and SQLite always; S3 when
//! `TRACEDB_TEST_S3_URL` is set (e.g. `s3://tracedb/test`, endpoint and credentials from `AWS_*`) and Postgres
//! when `TRACEDB_TEST_PG_URL` is set (e.g. `postgres://tracedb@localhost/tracedb`, password from `PGPASSWORD`).
//! Each run uses a fresh S3 prefix and a fresh Postgres database, so reruns don't see each other's data.

use bytes::Bytes;
use sqlx::Connection;
use tracedb_storage::segment::FieldStat;
use tracedb_storage::{DedupKeys, Metastore, SegmentMeta, Store};
use uuid::Uuid;

fn env_url(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ if std::env::var_os("TRACEDB_TEST_REQUIRE_CLOUD").is_some() => {
            panic!("{var} is not set but TRACEDB_TEST_REQUIRE_CLOUD is")
        }
        _ => {
            eprintln!("skipped: {var} is not set");
            None
        }
    }
}

fn run_id() -> String {
    ulid::Ulid::new().to_string().to_lowercase()
}

/// A new, empty database next to the one `base` names; returns its URL.
async fn fresh_pg(base: &str) -> String {
    let mut conn = sqlx::PgConnection::connect(base)
        .await
        .expect("connect to TRACEDB_TEST_PG_URL");
    let name = format!("tracedb_test_{}", run_id());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
    let mut url = url::Url::parse(base).expect("a postgres:// URL");
    url.set_path(&name);
    url.to_string()
}

fn parquet(rows: i64) -> Bytes {
    use datafusion::arrow::array::Int64Array;
    use datafusion::arrow::record_batch::RecordBatch;
    use datafusion::parquet::arrow::ArrowWriter;
    let batch =
        RecordBatch::try_from_iter([("n", std::sync::Arc::new(Int64Array::from_iter_values(0..rows)) as _)]).unwrap();
    let mut buf = Vec::new();
    let mut w = ArrowWriter::try_new(&mut buf, batch.schema(), None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
    Bytes::from(buf)
}

async fn object_contract(store: Store) {
    assert!(store.list().await.unwrap().is_empty(), "starts empty");
    let a = "p-1/2026-11-05/seg-a.core.parquet";
    let b = "p-1/2026-11-05/seg-a.payload.parquet";
    let bytes = parquet(1_000);
    store.put_new(a, bytes.clone()).await.unwrap();
    store.put_new(b, Bytes::from_static(b"payload")).await.unwrap();
    assert!(
        store.put_new(a, Bytes::from_static(b"other")).await.is_err(),
        "a put to an existing name fails"
    );
    assert_eq!(store.get(a).await.unwrap(), bytes, "write-once: the first put stays");
    let mut names = store.list().await.unwrap();
    names.sort();
    assert_eq!(names, vec![a.to_string(), b.to_string()]);

    // DataFusion reads the file through the store with range reads (footer, then column chunks).
    let ctx = datafusion::prelude::SessionContext::new();
    store.register(&ctx);
    let df = ctx
        .read_parquet(store.url(a), datafusion::prelude::ParquetReadOptions::default())
        .await
        .unwrap();
    let rows: usize = df.collect().await.unwrap().iter().map(|b| b.num_rows()).sum();
    assert_eq!(rows, 1_000);

    store.delete(a).await.unwrap();
    store.delete(a).await.unwrap();
    assert_eq!(store.list().await.unwrap(), vec![b.to_string()]);
    assert!(store.get(a).await.is_err());
    store.delete(b).await.unwrap();
    assert!(store.list().await.unwrap().is_empty());
}

fn segment(n: u128, seq: u64) -> (SegmentMeta, Vec<FieldStat>) {
    let id = format!("seg-{}", ulid::Ulid::new());
    let meta = SegmentMeta {
        id: id.clone(),
        project: "eval-agent".into(),
        core_path: format!("eval-agent-1/2026-11-05/{id}.core.parquet"),
        payload_path: format!("eval-agent-1/2026-11-05/{id}.payload.parquet"),
        min_start_key: 1_762_300_000_000_000 + n as i64,
        max_start_key: 1_762_300_000_000_000 + n as i64 * 2,
        min_trace_id: Uuid::from_u128(n),
        max_trace_id: Uuid::from_u128(n + 100),
        min_seq: seq.saturating_sub(9),
        max_seq: seq,
        row_count: 10,
        needs_merge_rows: 3,
        core_bytes: 4_096,
        payload_bytes: 8_192,
        created_at: 1_762_300_000_000_000,
    };
    let stats = vec![FieldStat {
        segment_id: id,
        path: "metadata.prompt_version".into(),
        ty: "string".into(),
        count: 10,
        top_values: vec![(serde_json::json!("v6"), 6), (serde_json::json!("v7"), 4)],
        other_count: 0,
    }];
    (meta, stats)
}

/// Every byte value, so a text column (or a lossy BYTEA mapping) would show.
fn keys(flushed_seq: u64, newest_at: i64) -> DedupKeys {
    DedupKeys {
        flushed_seq,
        newest_at,
        keys: (0..=255u8).chain([0, flushed_seq as u8]).collect(),
    }
}

async fn metastore_contract(url: &str) {
    let m = Metastore::open(url).await.unwrap();
    drop(Metastore::open(url).await.unwrap());
    assert_eq!(m.flushed_seq().await.unwrap(), 0);
    assert!(m.active_segments().await.unwrap().is_empty());
    assert!(m.dedup_keys().await.unwrap().is_empty());

    let (s1, f1) = segment(1, 42);
    m.commit_flush(std::slice::from_ref(&s1), &f1, 42, &keys(42, 1_000), 0)
        .await
        .unwrap();

    // All or nothing: a duplicate segment id rolls back the stats, dedup keys and flushed_seq too.
    let (s2, f2) = segment(2, 99);
    assert!(
        m.commit_flush(
            &[s2.clone(), s1.clone()],
            &[f2, f1.clone()].concat(),
            99,
            &keys(99, 2_000),
            0
        )
        .await
        .is_err()
    );
    assert_eq!(m.flushed_seq().await.unwrap(), 42);
    assert_eq!(m.active_segments().await.unwrap(), vec![s1.clone()]);
    assert_eq!(m.field_stats().await.unwrap(), f1);
    assert_eq!(m.dedup_keys().await.unwrap(), vec![keys(42, 1_000)]);

    // flushed_seq never moves backwards.
    let (s3, f3) = segment(3, 10);
    assert!(
        m.commit_flush(std::slice::from_ref(&s3), &f3, 10, &keys(10, 2_000), 0)
            .await
            .is_err()
    );
    drop(m);

    let m = Metastore::open(url).await.unwrap();
    assert_eq!(m.flushed_seq().await.unwrap(), 42, "persists across reopen");
    assert_eq!(m.active_segments().await.unwrap(), vec![s1.clone()]);
    assert_eq!(m.field_stats().await.unwrap(), f1);
    assert_eq!(m.dedup_keys().await.unwrap(), vec![keys(42, 1_000)]);

    // Dedup keys older than the window go, but the newest flush keeps its own.
    let mut want = vec![s1];
    for (seq, at, forget) in [(50, 1_500, 1_200), (60, 1_600, 1_200)] {
        let (s, f) = segment(seq as u128, seq);
        m.commit_flush(std::slice::from_ref(&s), &f, seq, &keys(seq, at), forget)
            .await
            .unwrap();
        want.push(s);
    }
    assert_eq!(m.dedup_keys().await.unwrap(), vec![keys(50, 1_500), keys(60, 1_600)]);
    let (s, f) = segment(70, 70);
    m.commit_flush(std::slice::from_ref(&s), &f, 70, &keys(70, 1_700), 1_000_000)
        .await
        .unwrap();
    want.push(s);
    assert_eq!(m.dedup_keys().await.unwrap(), vec![keys(70, 1_700)]);
    assert_eq!(m.active_segments().await.unwrap(), want, "ordered by min_seq");
    assert_eq!(m.flushed_seq().await.unwrap(), 70);

    // Saving outside a flush replaces the set for the same flushed_seq.
    m.save_dedup_keys(&keys(70, 1_800)).await.unwrap();
    assert_eq!(m.dedup_keys().await.unwrap(), vec![keys(70, 1_800)]);
    drop(m);

    // A catalog from before dedup_keys is upgraded in place on open.
    sqlx::any::install_default_drivers();
    let pool = sqlx::AnyPool::connect(url).await.unwrap();
    sqlx::query("DROP TABLE dedup_keys").execute(&pool).await.unwrap();
    pool.close().await;
    let m = Metastore::open(url).await.unwrap();
    assert!(m.dedup_keys().await.unwrap().is_empty());
    m.save_dedup_keys(&keys(70, 1_900)).await.unwrap();
    assert_eq!(m.dedup_keys().await.unwrap(), vec![keys(70, 1_900)]);
    assert_eq!(m.flushed_seq().await.unwrap(), 70);
}

#[tokio::test]
async fn local_object_store() {
    let dir = tempfile::tempdir().unwrap();
    object_contract(Store::local(&dir.path().join("data")).unwrap()).await;
}

#[tokio::test]
async fn s3_object_store() {
    let Some(base) = env_url("TRACEDB_TEST_S3_URL") else {
        return;
    };
    let store = Store::open(&format!("{}/traits-{}", base.trim_end_matches('/'), run_id())).unwrap();
    object_contract(store).await;
}

#[tokio::test]
async fn sqlite_metastore() {
    let dir = tempfile::tempdir().unwrap();
    metastore_contract(&format!("sqlite://{}", dir.path().join("meta.sqlite").display())).await;
}

#[tokio::test]
async fn postgres_metastore() {
    let Some(base) = env_url("TRACEDB_TEST_PG_URL") else {
        return;
    };
    metastore_contract(&fresh_pg(&base).await).await;
}
