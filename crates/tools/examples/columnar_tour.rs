//! A hands-on tour of columnar storage (see SECRET_OF_COLUMNAR_DB.md).
//!
//! Writes the same synthetic agent runs three ways: JSON lines (a row layout, read start to finish), JSON lines with
//! two indexes (a time index and a secondary index on `status`, as a row database would keep), and Parquet (a
//! columnar layout). Then answers five questions with each and prints what each had to read.
//!
//! ```sh
//! cargo run --release -p tracedb-tools --example columnar_tour            # 1,000,000 runs
//! cargo run --release -p tracedb-tools --example columnar_tour -- 200000  # fewer
//! ```

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use datafusion::arrow::array::{Int64Array, StringArray};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::arrow::util::pretty::pretty_format_batches;
use datafusion::dataframe::DataFrame;
use datafusion::functions_aggregate::expr_fn::{avg, count};
use datafusion::logical_expr::Expr;
use datafusion::parquet::arrow::ArrowWriter;
use datafusion::parquet::basic::{Compression, ZstdLevel};
use datafusion::parquet::file::properties::{EnabledStatistics, WriterProperties};
use datafusion::parquet::file::reader::{FileReader, SerializedFileReader};
use datafusion::prelude::{ParquetReadOptions, SessionConfig, SessionContext, col, lit};
use serde::{Deserialize, Serialize};

const TOOLS: [&str; 8] = [
    "fetch_url",
    "web_search",
    "plan",
    "summarize",
    "python_exec",
    "final_answer",
    "read_file",
    "llm",
];
const ROW_GROUP_ROWS: usize = 65_536;
/// One time-index entry per this many rows (a sparse index, like a clustered B-tree's leaf pages).
const TIME_INDEX_EVERY: usize = 1_024;

/// One run, the way a row store sees it: all fields of a record together.
#[derive(Serialize, Deserialize)]
struct Run {
    run_id: i64,
    start_ms: i64,
    name: String,
    status: String,
    status_code: i64,
    latency_ms: i64,
    tokens: i64,
    batch: String,
    prompt_version: String,
    output: String,
}

/// Deterministic pseudo-random numbers (no extra dependency).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn generate(n: usize) -> Vec<Run> {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let t0 = 1_762_000_000_000i64;
    (0..n)
        .map(|i| {
            let name = TOOLS[rng.below(TOOLS.len() as u64) as usize];
            let error = rng.below(100) < 5;
            let status_code = if !error {
                200
            } else if rng.below(4) < 3 {
                429
            } else {
                500
            };
            let latency = if error {
                20_000 + rng.below(15_000)
            } else {
                50 + rng.below(4_000)
            } as i64;
            let output = if error {
                format!(
                    "HTTPError: {status_code} for https://example.com/page/{}",
                    rng.below(10_000)
                )
            } else {
                format!(
                    "fetched {} bytes from https://example.com/page/{} and summarized {} paragraphs about topic {}",
                    rng.below(90_000),
                    rng.below(10_000),
                    1 + rng.below(9),
                    rng.below(500)
                )
            };
            Run {
                run_id: i as i64,
                // Runs arrive in time order, about 20 per second.
                start_ms: t0 + i as i64 * 50,
                name: name.to_string(),
                status: if error { "error" } else { "ok" }.to_string(),
                status_code,
                latency_ms: latency,
                tokens: 100 + rng.below(8_000) as i64,
                // The first half of the time range is batch a, the second half batch b.
                batch: if i < n / 2 { "a" } else { "b" }.to_string(),
                prompt_version: if rng.below(2) == 0 { "v6" } else { "v7" }.to_string(),
                output,
            }
        })
        .collect()
}

/// The row store's indexes, as a row database would keep them next to the table.
struct RowIndexes {
    /// `(start_ms, byte offset)` every `TIME_INDEX_EVERY` rows; rows are in time order, so it's sorted.
    time: Vec<(i64, u64)>,
    /// Byte offset of every row with `status = error` (a secondary index on one column).
    errors: Vec<u64>,
}

/// Writes JSON lines; with `indexed`, also builds and saves the two indexes. Returns them (empty without).
fn write_rows(dir: &Path, runs: &[Run], indexed: bool) -> std::io::Result<RowIndexes> {
    let mut w = BufWriter::new(File::create(dir.join("runs.jsonl"))?);
    let mut idx = RowIndexes {
        time: Vec::new(),
        errors: Vec::new(),
    };
    let mut at = 0u64;
    let mut line = Vec::new();
    for (i, r) in runs.iter().enumerate() {
        line.clear();
        serde_json::to_writer(&mut line, r)?;
        line.push(b'\n');
        if indexed {
            if i % TIME_INDEX_EVERY == 0 {
                idx.time.push((r.start_ms, at));
            }
            if r.status == "error" {
                idx.errors.push(at);
            }
        }
        w.write_all(&line)?;
        at += line.len() as u64;
    }
    w.flush()?;
    if indexed {
        let mut t = BufWriter::new(File::create(dir.join("time.idx"))?);
        for (ms, off) in &idx.time {
            t.write_all(&ms.to_le_bytes())?;
            t.write_all(&off.to_le_bytes())?;
        }
        t.flush()?;
        let mut e = BufWriter::new(File::create(dir.join("status_error.idx"))?);
        for off in &idx.errors {
            e.write_all(&off.to_le_bytes())?;
        }
        e.flush()?;
    }
    Ok(idx)
}

fn write_columns(path: &Path, runs: &[Run]) -> Result<(), Box<dyn std::error::Error>> {
    let strs = |f: fn(&Run) -> &str| Arc::new(StringArray::from_iter_values(runs.iter().map(f)));
    let ints = |f: fn(&Run) -> i64| Arc::new(Int64Array::from_iter_values(runs.iter().map(f)));
    let schema = Arc::new(Schema::new(vec![
        Field::new("run_id", DataType::Int64, false),
        Field::new("start_ms", DataType::Int64, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("status", DataType::Utf8, false),
        Field::new("status_code", DataType::Int64, false),
        Field::new("latency_ms", DataType::Int64, false),
        Field::new("tokens", DataType::Int64, false),
        Field::new("batch", DataType::Utf8, false),
        Field::new("prompt_version", DataType::Utf8, false),
        Field::new("output", DataType::Utf8, false),
    ]));
    // Each column becomes one contiguous array: this transposition is the whole idea.
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            ints(|r| r.run_id),
            ints(|r| r.start_ms),
            strs(|r| &r.name),
            strs(|r| &r.status),
            ints(|r| r.status_code),
            ints(|r| r.latency_ms),
            ints(|r| r.tokens),
            strs(|r| &r.batch),
            strs(|r| &r.prompt_version),
            strs(|r| &r.output),
        ],
    )?;
    // The same settings tracedb uses for its data files (crates/storage/src/segment.rs).
    let props = WriterProperties::builder()
        .set_max_row_group_row_count(Some(ROW_GROUP_ROWS))
        .set_statistics_enabled(EnabledStatistics::Page)
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(3)?))
        .build();
    let mut w = ArrowWriter::try_new(File::create(path)?, schema, Some(props))?;
    w.write(&batch)?;
    w.close()?;
    Ok(())
}

fn mb(bytes: u64) -> String {
    if bytes < 1_000_000 {
        format!("{:.1} KB", bytes as f64 / 1e3)
    } else {
        format!("{:.1} MB", bytes as f64 / 1e6)
    }
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

/// Bytes on disk per column, summed over row groups, and the encodings Parquet chose.
fn column_report(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let reader = SerializedFileReader::new(File::open(path)?)?;
    let meta = reader.metadata();
    let groups = meta.row_groups();
    println!("  {} row groups of up to {ROW_GROUP_ROWS} rows", groups.len());
    println!("  {:<15} {:>10} {:>12}   encodings", "column", "on disk", "per row");
    let rows = meta.file_metadata().num_rows() as f64;
    for (i, c) in meta.file_metadata().schema_descr().columns().iter().enumerate() {
        let bytes: i64 = groups.iter().map(|g| g.column(i).compressed_size()).sum();
        let enc = groups[0]
            .column(i)
            .encodings()
            .map(|e| format!("{e:?}"))
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "  {:<15} {:>10} {:>10.2} B   {enc}",
            c.name(),
            mb(bytes as u64),
            bytes as f64 / rows
        );
    }
    Ok(())
}

/// Pulls the scan's I/O and pruning counters out of an EXPLAIN ANALYZE plan.
fn scan_metrics(plan: &str) -> String {
    let wanted = [
        ("bytes_scanned", "read"),
        ("row_groups_pruned_statistics", "row groups"),
        ("page_index_rows_pruned", "rows by page index"),
    ];
    let mut out = Vec::new();
    for (key, label) in wanted {
        if let Some(at) = plan.find(&format!("{key}=")) {
            let rest = &plan[at + key.len() + 1..];
            let end = rest.find([',', ']']).unwrap_or(rest.len());
            out.push(format!("{label} {}", rest[..end].trim()));
        }
    }
    out.join(" · ")
}

/// Reads every line (the plain row store: no index, start to finish).
fn scan_rows(path: &Path, mut each: impl FnMut(&Run)) -> std::io::Result<()> {
    for line in BufReader::new(File::open(path)?).lines() {
        each(&serde_json::from_str(&line?)?);
    }
    Ok(())
}

/// Reads lines from `offset` to the end of the file; returns the bytes read.
fn scan_rows_from(path: &Path, offset: u64, mut each: impl FnMut(&Run)) -> std::io::Result<u64> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut read = 0;
    for line in BufReader::new(f).lines() {
        let line = line?;
        read += line.len() as u64 + 1;
        each(&serde_json::from_str(&line)?);
    }
    Ok(read)
}

/// Fetches the row at each offset (one seek per row, as an index lookup does); returns the bytes read.
fn fetch_rows(path: &Path, offsets: &[u64], mut each: impl FnMut(&Run)) -> std::io::Result<u64> {
    let mut r = BufReader::new(File::open(path)?);
    let (mut read, mut line) = (0, String::new());
    for &off in offsets {
        r.seek(SeekFrom::Start(off))?;
        line.clear();
        read += r.read_line(&mut line)? as u64;
        each(&serde_json::from_str(&line)?);
    }
    Ok(read)
}

/// The first offset whose block can hold `start_ms >= cutoff` (binary search on the sparse time index).
fn time_seek(idx: &[(i64, u64)], cutoff: i64) -> u64 {
    let i = idx.partition_point(|(t, _)| *t < cutoff);
    idx[i.saturating_sub(1)].1
}

struct Columns<'a> {
    ctx: &'a SessionContext,
    path: String,
}

impl Columns<'_> {
    async fn df(&self) -> datafusion::error::Result<DataFrame> {
        self.ctx
            .read_parquet(self.path.as_str(), ParquetReadOptions::default())
            .await
    }

    /// Runs a filtered count and prints the answer, time and the scan's counters.
    async fn count(&self, filter: Expr) -> Result<(), Box<dyn std::error::Error>> {
        let q = || async {
            self.df()
                .await?
                .filter(filter.clone())?
                .aggregate(vec![], vec![count(lit(1))])
        };
        let t = Instant::now();
        let out = q().await?.collect().await?;
        let took = ms(t);
        let n = out[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .map_or(0, |a| a.value(0));
        let plan = pretty_format_batches(&q().await?.explain(false, true)?.collect().await?)?.to_string();
        println!(
            "  {:<22} {n:>8}  {took:>7.0} ms   {}",
            "columns (Parquet)",
            scan_metrics(&plan)
        );
        Ok(())
    }
}

fn row_line(label: &str, n: u64, t: Instant, read: u64) {
    println!("  {label:<22} {n:>8}  {:>7.0} ms   read {}", ms(t), mb(read));
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let n: usize = std::env::args()
        .nth(1)
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(1_000_000);
    let plain = tempfile::tempdir()?;
    let indexed = tempfile::tempdir()?;
    let cols_path = plain.path().join("runs.parquet");

    println!("Generating {n} runs…");
    let runs = generate(n);
    let t = Instant::now();
    write_rows(plain.path(), &runs, false)?;
    let write_plain = ms(t);
    let t = Instant::now();
    let idx = write_rows(indexed.path(), &runs, true)?;
    let write_indexed = ms(t);
    let t = Instant::now();
    write_columns(&cols_path, &runs)?;
    let write_cols = ms(t);
    let cutoff = runs[n - n / 100].start_ms; // the newest 1% of runs
    let last_quarter = runs[n - n / 4].start_ms; // the newest 25%
    drop(runs);

    let rows_path = plain.path().join("runs.jsonl");
    let ix_rows = indexed.path().join("runs.jsonl");
    let size = |p: &Path| std::fs::metadata(p).map(|m| m.len());
    let row_bytes = size(&rows_path)?;
    let time_idx = size(&indexed.path().join("time.idx"))?;
    let err_idx = size(&indexed.path().join("status_error.idx"))?;
    let col_bytes = size(&cols_path)?;

    println!("\n1. Storage, and what the indexes cost");
    println!("  {:<34} {:>10} {:>12}", "", "on disk", "write time");
    println!(
        "  {:<34} {:>10} {:>9.0} ms",
        "rows (JSON lines)",
        mb(row_bytes),
        write_plain
    );
    println!(
        "  {:<34} {:>10} {:>9.0} ms   (+{} time index, +{} status index)",
        "rows + 2 indexes",
        mb(row_bytes + time_idx + err_idx),
        write_indexed,
        mb(time_idx),
        mb(err_idx)
    );
    println!(
        "  {:<34} {:>10} {:>9.0} ms   ({:.0}% of the rows file)",
        "columns (Parquet)",
        mb(col_bytes),
        write_cols,
        100.0 * col_bytes as f64 / row_bytes as f64
    );
    column_report(&cols_path)?;

    let cfg = SessionConfig::new().set_bool("datafusion.execution.parquet.pushdown_filters", true);
    let ctx = SessionContext::new_with_config(cfg);
    let columns = Columns {
        ctx: &ctx,
        path: cols_path.to_str().unwrap().to_string(),
    };
    println!("\n  {:<22} {:>8}  {:>10}   what was read", "", "answer", "time");

    // Q1. SELECT count(*) FROM runs WHERE status = 'error'
    println!("\n2. How many runs failed?");
    let t = Instant::now();
    let mut c = 0;
    scan_rows(&rows_path, |r| c += (r.status == "error") as u64)?;
    row_line("rows, no index", c, t, row_bytes);
    let t = Instant::now();
    row_line("rows + status index", idx.errors.len() as u64, t, err_idx);
    columns.count(col("status").eq(lit("error"))).await?;

    // Q2. SELECT count(*) FROM runs WHERE start_ms >= <newest 1%>
    println!("\n3. How many runs started in the newest 1% of the time range?");
    let t = Instant::now();
    let mut c = 0;
    scan_rows(&rows_path, |r| c += (r.start_ms >= cutoff) as u64)?;
    row_line("rows, no index", c, t, row_bytes);
    let t = Instant::now();
    let mut c = 0;
    let read = scan_rows_from(&ix_rows, time_seek(&idx.time, cutoff), |r| {
        c += (r.start_ms >= cutoff) as u64
    })?;
    row_line("rows + time index", c, t, read);
    columns.count(col("start_ms").gt_eq(lit(cutoff))).await?;

    // Q3. SELECT name, avg(latency_ms) FROM runs GROUP BY name
    println!("\n4. Average latency per tool (no index helps a full aggregate)");
    let t = Instant::now();
    let mut sums = std::collections::HashMap::<String, (i64, i64)>::new();
    scan_rows(&rows_path, |r| {
        let e = sums.entry(r.name.clone()).or_default();
        e.0 += r.latency_ms;
        e.1 += 1;
    })?;
    row_line("rows, any index", sums.len() as u64, t, row_bytes);
    let q3 = || async {
        columns
            .df()
            .await?
            .aggregate(vec![col("name")], vec![avg(col("latency_ms")).alias("avg_latency_ms")])
    };
    let t = Instant::now();
    let out = q3().await?.sort(vec![col("name").sort(true, false)])?.collect().await?;
    let took = ms(t);
    let plan = pretty_format_batches(&q3().await?.explain(false, true)?.collect().await?)?.to_string();
    let groups: usize = out.iter().map(|b| b.num_rows()).sum();
    println!(
        "  {:<22} {groups:>8}  {took:>7.0} ms   {}",
        "columns (Parquet)",
        scan_metrics(&plan)
    );

    // Q4 (SmithDB "narrow it down"). Errors in the newest 25% of the time range, batch b, slower than 30 s.
    println!("\n5. Narrow it down: errors in the newest 25%, batch b, slower than 30 s (4 conditions)");
    let matches4 =
        |r: &Run| r.start_ms >= last_quarter && r.status == "error" && r.batch == "b" && r.latency_ms > 30_000;
    let t = Instant::now();
    let mut c = 0;
    scan_rows(&rows_path, |r| c += matches4(r) as u64)?;
    row_line("rows, no index", c, t, row_bytes);
    // A row database picks one index, then checks the other conditions on the rows it fetches.
    let t = Instant::now();
    let mut c = 0;
    let read = scan_rows_from(&ix_rows, time_seek(&idx.time, last_quarter), |r| {
        c += matches4(r) as u64
    })?;
    row_line("rows + time index", c, t, read + time_idx);
    let t = Instant::now();
    let mut c = 0;
    let read = fetch_rows(&ix_rows, &idx.errors, |r| c += matches4(r) as u64)?;
    row_line("rows + status index", c, t, read + err_idx);
    columns
        .count(
            col("start_ms")
                .gt_eq(lit(last_quarter))
                .and(col("status").eq(lit("error")))
                .and(col("batch").eq(lit("b")))
                .and(col("latency_ms").gt(lit(30_000i64))),
        )
        .await?;

    // Q5 (SmithDB "look inside the JSON"). prompt_version = v7 and status_code = 429: no index on either column.
    println!("\n6. Look inside: prompt_version = v7 and status_code = 429 (neither column indexed)");
    let matches5 = |r: &Run| r.prompt_version == "v7" && r.status_code == 429;
    let t = Instant::now();
    let mut c = 0;
    scan_rows(&rows_path, |r| c += matches5(r) as u64)?;
    row_line("rows, no index", c, t, row_bytes);
    println!(
        "  {:<22} {:>8}  {:>10}   neither column has an index: same full scan",
        "rows + indexes", "", ""
    );
    columns
        .count(
            col("prompt_version")
                .eq(lit("v7"))
                .and(col("status_code").eq(lit(429i64))),
        )
        .await?;

    println!(
        "\nAverage latency per tool (columns):\n{}",
        pretty_format_batches(&out)?
    );
    Ok(())
}
