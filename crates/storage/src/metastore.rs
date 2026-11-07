//! The catalog of data files and their field statistics: SQLite locally, Postgres in cloud mode, same SQL
//! (data-model §5). A flush registers its files, saves its dedup keys and moves `flushed_seq` in one transaction.

use sqlx::any::{AnyPoolOptions, install_default_drivers};
use sqlx::{AnyPool, Row};

use crate::StorageError;
use crate::segment::{FieldStat, SegmentMeta};

pub const SCHEMA_VERSION: i64 = 2;

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

/// Added after the first M2 release; `CREATE … IF NOT EXISTS` upgrades older catalogs in place.
const DEDUP_TABLE: &str = "CREATE TABLE IF NOT EXISTS dedup_keys (
        flushed_seq BIGINT PRIMARY KEY,
        newest_at BIGINT NOT NULL,
        keys BLOB NOT NULL)";

/// schema_version 1 → 2 (data-model §3): the text index columns and the feedback table, in the transaction that
/// moves the version.
const V2: &[&str] = &[
    "ALTER TABLE segments ADD COLUMN fts_path  TEXT",
    "ALTER TABLE segments ADD COLUMN fts_bytes BIGINT",
    "CREATE TABLE IF NOT EXISTS feedback (
        id          TEXT PRIMARY KEY,
        run_id      TEXT NOT NULL,
        trace_id    TEXT,
        key         TEXT NOT NULL,
        score       DOUBLE PRECISION,
        value       TEXT,
        comment     TEXT,
        correction  TEXT,
        source      TEXT,
        created_at  BIGINT NOT NULL,
        modified_at BIGINT NOT NULL,
        raw         TEXT NOT NULL)",
    "CREATE INDEX IF NOT EXISTS feedback_run ON feedback (run_id)",
    "CREATE INDEX IF NOT EXISTS feedback_key ON feedback (key)",
];

/// The dedup keys of one flush's events, packed by the caller (the catalog doesn't read them).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DedupKeys {
    /// The flush's `flushed_seq`.
    pub flushed_seq: u64,
    /// Newest `received_at` among the keys (µs); rows older than the dedup window are deleted.
    pub newest_at: i64,
    pub keys: Vec<u8>,
}

/// One `feedback` row (data-model §3). `raw` is the whole stored item; the other columns are its parts, for SQL.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FeedbackRow {
    pub id: String,
    pub run_id: String,
    pub trace_id: Option<String>,
    pub key: String,
    pub score: Option<f64>,
    /// JSON.
    pub value: Option<String>,
    pub comment: Option<String>,
    /// JSON.
    pub correction: Option<String>,
    /// JSON `feedback_source`.
    pub source: Option<String>,
    pub created_at: i64,
    pub modified_at: i64,
    pub raw: String,
}

/// What the in-memory feedback index holds per row.
#[derive(Clone, Debug, PartialEq)]
pub struct FeedbackKey {
    pub id: String,
    pub run_id: String,
    pub key: String,
    pub score: Option<f64>,
}

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
        let dedup_table = if pg {
            DEDUP_TABLE.replace("BLOB", "BYTEA")
        } else {
            DEDUP_TABLE.to_string()
        };
        sqlx::query(sqlx::AssertSqlSafe(dedup_table)).execute(&m.pool).await?;
        let have: Option<i64> = sqlx::query("SELECT schema_version FROM state WHERE id = 1")
            .fetch_optional(&m.pool)
            .await?
            .map(|r| r.get(0));
        // A new catalog starts at version 1 and is upgraded like an M2 one, so both take the same path.
        if have.is_none() {
            sqlx::query("INSERT INTO state (id, schema_version, flushed_seq) VALUES (1, 1, 0)")
                .execute(&m.pool)
                .await?;
        }
        match have.unwrap_or(1) {
            1 => {
                let mut tx = m.pool.begin().await?;
                // Taken first, so a second process opening the same catalog waits, then sees version 2.
                let moved = sqlx::query("UPDATE state SET schema_version = 2 WHERE id = 1 AND schema_version = 1")
                    .execute(&mut *tx)
                    .await?;
                if moved.rows_affected() == 1 {
                    for stmt in V2 {
                        sqlx::query(*stmt).execute(&mut *tx).await?;
                    }
                }
                tx.commit().await?;
            }
            SCHEMA_VERSION => {}
            v => {
                return Err(StorageError::Metastore(format!(
                    "catalog schema version {v}, this tracedb understands {SCHEMA_VERSION}"
                )));
            }
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
                    min_seq, max_seq, row_count, needs_merge_rows, core_bytes, payload_bytes, created_at,
                    fts_path, fts_bytes
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
                    fts_path: r.get(15),
                    fts_bytes: r.get::<Option<i64>, _>(16).unwrap_or(0) as u64,
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

    /// Every saved set of dedup keys, oldest flush first.
    pub async fn dedup_keys(&self) -> Result<Vec<DedupKeys>, StorageError> {
        let rows = sqlx::query("SELECT flushed_seq, newest_at, keys FROM dedup_keys ORDER BY flushed_seq")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .iter()
            .map(|r| DedupKeys {
                flushed_seq: r.get::<i64, _>(0) as u64,
                newest_at: r.get(1),
                keys: r.get(2),
            })
            .collect())
    }

    /// Saves `keys` outside a flush (replacing a set saved for the same `flushed_seq`).
    pub async fn save_dedup_keys(&self, keys: &DedupKeys) -> Result<(), StorageError> {
        let mut tx = self.pool.begin().await?;
        self.upsert_dedup(&mut tx, keys).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn upsert_dedup(&self, tx: &mut sqlx::Transaction<'_, sqlx::Any>, k: &DedupKeys) -> Result<(), StorageError> {
        sqlx::query(self.sql(
            "INSERT INTO dedup_keys (flushed_seq, newest_at, keys) VALUES (?, ?, ?)
             ON CONFLICT (flushed_seq) DO UPDATE SET newest_at = excluded.newest_at, keys = excluded.keys",
        ))
        .bind(k.flushed_seq as i64)
        .bind(k.newest_at)
        .bind(k.keys.clone())
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Registers a flush, all or nothing: its segments and field stats, its dedup keys, and the new
    /// `flushed_seq`. Older key sets whose newest key is before `forget_before` (µs) are deleted.
    ///
    /// Segment ids are minted once per flush job, so a retry whose earlier commit landed (a lost reply) finds
    /// them all registered: that is "already committed", `Ok` with no change.
    pub async fn commit_flush(
        &self,
        segments: &[SegmentMeta],
        stats: &[FieldStat],
        flushed_seq: u64,
        dedup: &DedupKeys,
        forget_before: i64,
    ) -> Result<(), StorageError> {
        let mut tx = self.pool.begin().await?;
        let mut existing = 0;
        for s in segments {
            existing += sqlx::query(self.sql("SELECT 1 FROM segments WHERE id = ?"))
                .bind(&s.id)
                .fetch_optional(&mut *tx)
                .await?
                .is_some() as usize;
        }
        if existing > 0 {
            let have = sqlx::query("SELECT flushed_seq FROM state WHERE id = 1")
                .fetch_one(&mut *tx)
                .await?
                .get::<i64, _>(0) as u64;
            tx.rollback().await?;
            if existing == segments.len() && have >= flushed_seq {
                return Ok(());
            }
            return Err(StorageError::Metastore(format!(
                "{existing} of {} segments are already registered (flushed_seq {have}, this flush {flushed_seq})",
                segments.len()
            )));
        }
        let ins_seg = self.sql(
            "INSERT INTO segments (id, project, core_path, payload_path, min_start_key, max_start_key,
                 min_trace_id, max_trace_id, min_seq, max_seq, row_count, needs_merge_rows, core_bytes,
                 payload_bytes, created_at, fts_path, fts_bytes)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
                .bind(s.fts_path.clone())
                .bind(s.fts_path.as_ref().map(|_| s.fts_bytes as i64))
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
        // Never moves backwards: a stale or repeated flush must not make replay redo flushed events.
        let moved = sqlx::query(self.sql("UPDATE state SET flushed_seq = ? WHERE id = 1 AND flushed_seq <= ?"))
            .bind(flushed_seq as i64)
            .bind(flushed_seq as i64)
            .execute(&mut *tx)
            .await?;
        if moved.rows_affected() != 1 {
            return Err(StorageError::Metastore(format!(
                "flushed_seq would move backwards to {flushed_seq}"
            )));
        }
        self.upsert_dedup(&mut tx, dedup).await?;
        sqlx::query(self.sql("DELETE FROM dedup_keys WHERE newest_at < ? AND flushed_seq < ?"))
            .bind(forget_before)
            .bind(dedup.flushed_seq as i64)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Records a sidecar written after the segment's flush (backfill): one segment per transaction.
    pub async fn set_fts(&self, segment_id: &str, fts_path: &str, fts_bytes: u64) -> Result<(), StorageError> {
        let r = sqlx::query(self.sql("UPDATE segments SET fts_path = ?, fts_bytes = ? WHERE id = ?"))
            .bind(fts_path)
            .bind(fts_bytes as i64)
            .bind(segment_id)
            .execute(&self.pool)
            .await?;
        if r.rows_affected() != 1 {
            return Err(StorageError::Metastore(format!("no segment {segment_id}")));
        }
        Ok(())
    }
}

impl Metastore {
    /// Inserts or replaces rows by `id`, all in one transaction.
    pub async fn put_feedback(&self, rows: &[FeedbackRow]) -> Result<(), StorageError> {
        let mut tx = self.pool.begin().await?;
        let ins = self.sql(
            "INSERT INTO feedback (id, run_id, trace_id, key, score, value, comment, correction, source,
                 created_at, modified_at, raw)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET run_id = excluded.run_id, trace_id = excluded.trace_id,
                 key = excluded.key, score = excluded.score, value = excluded.value, comment = excluded.comment,
                 correction = excluded.correction, source = excluded.source, created_at = excluded.created_at,
                 modified_at = excluded.modified_at, raw = excluded.raw",
        );
        for r in rows {
            sqlx::query(sqlx::AssertSqlSafe(ins.0.clone()))
                .bind(&r.id)
                .bind(&r.run_id)
                .bind(r.trace_id.clone())
                .bind(&r.key)
                .bind(r.score)
                .bind(r.value.clone())
                .bind(r.comment.clone())
                .bind(r.correction.clone())
                .bind(r.source.clone())
                .bind(r.created_at)
                .bind(r.modified_at)
                .bind(&r.raw)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// The stored item (`raw`) with this id.
    pub async fn feedback_raw(&self, id: &str) -> Result<Option<String>, StorageError> {
        Ok(sqlx::query(self.sql("SELECT raw FROM feedback WHERE id = ?"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .map(|r| r.get(0)))
    }

    /// Every stored item (`raw`) for these runs, by run, oldest first.
    pub async fn feedback_for_runs(&self, run_ids: &[String]) -> Result<Vec<String>, StorageError> {
        let mut out = Vec::new();
        for chunk in run_ids.chunks(500) {
            let marks = vec!["?"; chunk.len()].join(", ");
            let q = self.placeholders(&format!(
                "SELECT raw FROM feedback WHERE run_id IN ({marks}) ORDER BY run_id, created_at, id"
            ));
            // Only `?` marks are formatted in; the ids are bound.
            let mut query = sqlx::query(sqlx::AssertSqlSafe(q));
            for id in chunk {
                query = query.bind(id);
            }
            out.extend(query.fetch_all(&self.pool).await?.iter().map(|r| r.get::<String, _>(0)));
        }
        Ok(out)
    }

    /// Id, run, key and score of every row, for the in-memory index.
    pub async fn feedback_keys(&self) -> Result<Vec<FeedbackKey>, StorageError> {
        let rows = sqlx::query("SELECT id, run_id, key, score FROM feedback")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .iter()
            .map(|r| FeedbackKey {
                id: r.get(0),
                run_id: r.get(1),
                key: r.get(2),
                score: r.get(3),
            })
            .collect())
    }
}

/// Removes a password from a connection URL before it is shown anywhere.
pub fn redact(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let token = tail.find(char::is_whitespace).unwrap_or(tail.len());
        let slash = tail[..token].find('/').unwrap_or(token);
        let query = tail[..token].find(['?', '#']).unwrap_or(token);
        // A password may contain `/` (AWS secrets do): then the userinfo runs to the last `@` before a query.
        let at = tail[..query]
            .rfind('@')
            .filter(|&at| at < slash || tail[..at].contains(':'));
        let end = match at {
            Some(at) => {
                let user = tail[..at].split(':').next().unwrap_or("");
                let end = tail[at..token].find('/').map_or(token, |j| at + j);
                out.push_str(user);
                out.push_str(":***");
                out.push_str(&tail[at..end]);
                end
            }
            None => {
                out.push_str(&tail[..slash]);
                slash
            }
        };
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}
