//! The catalog of data files and their field statistics: SQLite locally, Postgres in cloud mode, same SQL
//! (data-model §5). A flush registers its files and moves `flushed_seq` in one transaction.

use sqlx::any::{AnyPoolOptions, install_default_drivers};
use sqlx::{AnyPool, Row};

use crate::StorageError;
use crate::segment::{FieldStat, SegmentMeta};

pub const SCHEMA_VERSION: i64 = 1;

const MIGRATION: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS state (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        schema_version INTEGER NOT NULL,
        flushed_seq BIGINT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS segments (
        id TEXT PRIMARY KEY,
        project TEXT NOT NULL,
        core_path TEXT NOT NULL,
        payload_path TEXT NOT NULL,
        min_start_key BIGINT NOT NULL, max_start_key BIGINT NOT NULL,
        min_trace_id TEXT NOT NULL, max_trace_id TEXT NOT NULL,
        min_seq BIGINT NOT NULL, max_seq BIGINT NOT NULL,
        row_count BIGINT NOT NULL,
        needs_merge_rows BIGINT NOT NULL,
        core_bytes BIGINT NOT NULL, payload_bytes BIGINT NOT NULL,
        tier INTEGER NOT NULL DEFAULT 0,
        state TEXT NOT NULL DEFAULT 'active',
        created_at BIGINT NOT NULL)",
    "CREATE INDEX IF NOT EXISTS segments_scope ON segments (project, max_start_key)",
    "CREATE TABLE IF NOT EXISTS field_stats (
        segment_id TEXT NOT NULL,
        path TEXT NOT NULL,
        type TEXT NOT NULL,
        count BIGINT NOT NULL,
        top_values TEXT NOT NULL,
        other_count BIGINT NOT NULL,
        PRIMARY KEY (segment_id, path, type))",
];

#[derive(Clone)]
pub struct Metastore {
    pool: AnyPool,
    pg: bool,
}

impl Metastore {
    /// `sqlite://path` (created if missing) or `postgres://…`.
    pub async fn open(url: &str) -> Result<Metastore, StorageError> {
        install_default_drivers();
        let pg = url.starts_with("postgres://") || url.starts_with("postgresql://");
        let url = if !pg && !url.contains('?') {
            format!("{url}?mode=rwc")
        } else {
            url.to_string()
        };
        let pool = AnyPoolOptions::new()
            .max_connections(if pg { 4 } else { 1 })
            .connect(&url)
            .await
            .map_err(|e| StorageError::Metastore(redact(&e.to_string())))?;
        let m = Metastore { pool, pg };
        if !pg {
            sqlx::query("PRAGMA journal_mode=WAL").execute(&m.pool).await?;
            sqlx::query("PRAGMA synchronous=FULL").execute(&m.pool).await?;
        }
        for stmt in MIGRATION {
            sqlx::query(*stmt).execute(&m.pool).await?;
        }
        let have: Option<i64> = sqlx::query("SELECT schema_version FROM state WHERE id = 1")
            .fetch_optional(&m.pool)
            .await?
            .map(|r| r.get(0));
        match have {
            None => {
                sqlx::query(m.sql("INSERT INTO state (id, schema_version, flushed_seq) VALUES (1, ?, 0)"))
                    .bind(SCHEMA_VERSION)
                    .execute(&m.pool)
                    .await?;
            }
            Some(v) if v != SCHEMA_VERSION => {
                return Err(StorageError::Metastore(format!(
                    "catalog schema version {v}, this tracedb understands {SCHEMA_VERSION}"
                )));
            }
            Some(_) => {}
        }
        Ok(m)
    }

    /// `?` placeholders become `$1, $2, …` on Postgres. Input is always one of this file's fixed
    /// statements (values are bound, never formatted in), so the result is safe to run.
    fn sql(&self, q: &'static str) -> sqlx::AssertSqlSafe<String> {
        sqlx::AssertSqlSafe(self.placeholders(q))
    }

    fn placeholders(&self, q: &str) -> String {
        if !self.pg {
            return q.to_string();
        }
        let mut out = String::with_capacity(q.len() + 8);
        let mut n = 0;
        for c in q.chars() {
            if c == '?' {
                n += 1;
                out.push_str(&format!("${n}"));
            } else {
                out.push(c);
            }
        }
        out
    }

    /// Every event with `seq ≤ flushed_seq` is in a registered segment.
    pub async fn flushed_seq(&self) -> Result<u64, StorageError> {
        let r = sqlx::query("SELECT flushed_seq FROM state WHERE id = 1")
            .fetch_one(&self.pool)
            .await?;
        Ok(r.get::<i64, _>(0) as u64)
    }

    pub async fn active_segments(&self) -> Result<Vec<SegmentMeta>, StorageError> {
        let rows = sqlx::query(
            "SELECT id, project, core_path, payload_path, min_start_key, max_start_key, min_trace_id, max_trace_id,
                    min_seq, max_seq, row_count, needs_merge_rows, core_bytes, payload_bytes, created_at
             FROM segments WHERE state = 'active' ORDER BY min_seq",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let id = |i: usize| -> Result<uuid::Uuid, StorageError> {
                    uuid::Uuid::try_parse(&r.get::<String, _>(i)).map_err(|e| StorageError::Metastore(e.to_string()))
                };
                Ok(SegmentMeta {
                    id: r.get(0),
                    project: r.get(1),
                    core_path: r.get(2),
                    payload_path: r.get(3),
                    min_start_key: r.get(4),
                    max_start_key: r.get(5),
                    min_trace_id: id(6)?,
                    max_trace_id: id(7)?,
                    min_seq: r.get::<i64, _>(8) as u64,
                    max_seq: r.get::<i64, _>(9) as u64,
                    row_count: r.get::<i64, _>(10) as u64,
                    needs_merge_rows: r.get::<i64, _>(11) as u64,
                    core_bytes: r.get::<i64, _>(12) as u64,
                    payload_bytes: r.get::<i64, _>(13) as u64,
                    created_at: r.get(14),
                })
            })
            .collect()
    }

    pub async fn field_stats(&self) -> Result<Vec<FieldStat>, StorageError> {
        let rows = sqlx::query(
            "SELECT f.segment_id, f.path, f.type, f.count, f.top_values, f.other_count
             FROM field_stats f JOIN segments s ON s.id = f.segment_id WHERE s.state = 'active'",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(FieldStat {
                    segment_id: r.get(0),
                    path: r.get(1),
                    ty: r.get(2),
                    count: r.get::<i64, _>(3) as u64,
                    top_values: serde_json::from_str(&r.get::<String, _>(4))
                        .map_err(|e| StorageError::Metastore(e.to_string()))?,
                    other_count: r.get::<i64, _>(5) as u64,
                })
            })
            .collect()
    }

    /// Registers a flush: its segments and field stats, and the new `flushed_seq`, all or nothing.
    pub async fn commit_flush(
        &self,
        segments: &[SegmentMeta],
        stats: &[FieldStat],
        flushed_seq: u64,
    ) -> Result<(), StorageError> {
        let mut tx = self.pool.begin().await?;
        let ins_seg = self.sql(
            "INSERT INTO segments (id, project, core_path, payload_path, min_start_key, max_start_key,
                 min_trace_id, max_trace_id, min_seq, max_seq, row_count, needs_merge_rows, core_bytes,
                 payload_bytes, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        );
        for s in segments {
            sqlx::query(sqlx::AssertSqlSafe(ins_seg.0.clone()))
                .bind(&s.id)
                .bind(&s.project)
                .bind(&s.core_path)
                .bind(&s.payload_path)
                .bind(s.min_start_key)
                .bind(s.max_start_key)
                .bind(s.min_trace_id.to_string())
                .bind(s.max_trace_id.to_string())
                .bind(s.min_seq as i64)
                .bind(s.max_seq as i64)
                .bind(s.row_count as i64)
                .bind(s.needs_merge_rows as i64)
                .bind(s.core_bytes as i64)
                .bind(s.payload_bytes as i64)
                .bind(s.created_at)
                .execute(&mut *tx)
                .await?;
        }
        let ins_stat = self.sql(
            "INSERT INTO field_stats (segment_id, path, type, count, top_values, other_count)
             VALUES (?, ?, ?, ?, ?, ?)",
        );
        for f in stats {
            sqlx::query(sqlx::AssertSqlSafe(ins_stat.0.clone()))
                .bind(&f.segment_id)
                .bind(&f.path)
                .bind(&f.ty)
                .bind(f.count as i64)
                .bind(serde_json::to_string(&f.top_values).expect("serializable"))
                .bind(f.other_count as i64)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query(self.sql("UPDATE state SET flushed_seq = ? WHERE id = 1"))
            .bind(flushed_seq as i64)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}

/// Removes a password from a connection URL before it is shown anywhere.
pub fn redact(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let end = tail.find(|c: char| c == '/' || c.is_whitespace()).unwrap_or(tail.len());
        match tail[..end].rfind('@') {
            Some(at) => {
                let user = tail[..at].split(':').next().unwrap_or("");
                out.push_str(user);
                out.push_str(":***");
                out.push_str(&tail[at..end]);
            }
            None => out.push_str(&tail[..end]),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}
