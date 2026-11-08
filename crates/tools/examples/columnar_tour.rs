//! A hands-on tour of columnar storage (see SECRET_OF_COLUMNAR_DB.md).
//!
//! Writes the same synthetic agent runs twice, as JSON lines (a row layout) and as Parquet (a columnar layout),
//! then answers three questions against both and prints what each had to read.
//!
//! ```sh
//! cargo run --release -p tracedb-tools --example columnar_tour            # 1,000,000 runs
//! cargo run --release -p tracedb-tools --example columnar_tour -- 200000  # fewer
//! ```

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use datafusion::arrow::array::{Int64Array, StringArray};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::arrow::util::pretty::pretty_format_batches;
use datafusion::functions_aggregate::expr_fn::{avg, count};
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

/// One run, the way a row store sees it: all fields of a record together.
#[derive(Serialize, Deserialize)]
struct Run {
    run_id: i64,
    start_ms: i64,
    name: String,
    status: String,
    latency_ms: i64,
    tokens: i64,
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
            let latency = if error {
                20_000 + rng.below(15_000)
            } else {
                50 + rng.below(4_000)
            } as i64;
            let output = if error {
                format!(
                    "HTTPError: 429 Too Many Requests for https://example.com/page/{}",
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
                latency_ms: latency,
                tokens: 100 + rng.below(8_000) as i64,
                output,
            }
        })
        .collect()
}

fn write_rows(path: &Path, runs: &[Run]) -> std::io::Result<()> {
    let mut w = BufWriter::new(File::create(path)?);
    for r in runs {
        serde_json::to_writer(&mut w, r)?;
        w.write_all(b"\n")?;
    }
    w.flush()
}

fn write_columns(path: &Path, runs: &[Run]) -> Result<(), Box<dyn std::error::Error>> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("run_id", DataType::Int64, false),
        Field::new("start_ms", DataType::Int64, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("status", DataType::Utf8, false),
        Field::new("latency_ms", DataType::Int64, false),
        Field::new("tokens", DataType::Int64, false),
        Field::new("output", DataType::Utf8, false),
    ]));
    // Each column becomes one contiguous array: this transposition is the whole idea.
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(Int64Array::from_iter_values(runs.iter().map(|r| r.run_id))),
            Arc::new(Int64Array::from_iter_values(runs.iter().map(|r| r.start_ms))),
            Arc::new(StringArray::from_iter_values(runs.iter().map(|r| r.name.as_str()))),
            Arc::new(StringArray::from_iter_values(runs.iter().map(|r| r.status.as_str()))),
            Arc::new(Int64Array::from_iter_values(runs.iter().map(|r| r.latency_ms))),
            Arc::new(Int64Array::from_iter_values(runs.iter().map(|r| r.tokens))),
            Arc::new(StringArray::from_iter_values(runs.iter().map(|r| r.output.as_str()))),
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
    format!("{:.1} MB", bytes as f64 / 1e6)
}

/// Bytes on disk per column, summed over row groups, and the encodings Parquet chose.
fn column_report(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let reader = SerializedFileReader::new(File::open(path)?)?;
    let meta = reader.metadata();
    let groups = meta.row_groups();
    println!("  {} row groups of up to {ROW_GROUP_ROWS} rows", groups.len());
    println!("  {:<12} {:>10} {:>12}   encodings", "column", "on disk", "per row");
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
            "  {:<12} {:>10} {:>10.2} B   {enc}",
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
        "bytes_scanned",
        "row_groups_pruned_statistics",
        "page_index_rows_pruned",
    ];
    let mut out = Vec::new();
    for key in wanted {
        if let Some(at) = plan.find(&format!("{key}=")) {
            let rest = &plan[at..];
            let end = rest.find([',', ']']).unwrap_or(rest.len());
            out.push(rest[..end].trim().to_string());
        }
    }
    out.join("  ")
}

fn scan_rows(path: &Path, mut each: impl FnMut(&Run)) -> std::io::Result<()> {
    for line in BufReader::new(File::open(path)?).lines() {
        let run: Run = serde_json::from_str(&line?)?;
        each(&run);
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let n: usize = std::env::args()
        .nth(1)
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(1_000_000);
    let dir = tempfile::tempdir()?;
    let (rows_path, cols_path) = (dir.path().join("runs.jsonl"), dir.path().join("runs.parquet"));

    println!("Generating {n} runs…");
    let runs = generate(n);
    write_rows(&rows_path, &runs)?;
    write_columns(&cols_path, &runs)?;
    let cutoff = runs[n - n / 100].start_ms; // the newest 1% of runs
    drop(runs);

    let row_bytes = std::fs::metadata(&rows_path)?.len();
    let col_bytes = std::fs::metadata(&cols_path)?.len();
    println!("\n1. Storage");
    println!("  rows    (JSON lines): {}", mb(row_bytes));
    println!(
        "  columns (Parquet):    {}  ({:.0}% of the rows file)",
        mb(col_bytes),
        100.0 * col_bytes as f64 / row_bytes as f64
    );
    column_report(&cols_path)?;

    let cfg = SessionConfig::new().set_bool("datafusion.execution.parquet.pushdown_filters", true);
    let ctx = SessionContext::new_with_config(cfg);
    let runs_df = || ctx.read_parquet(cols_path.to_str().unwrap(), ParquetReadOptions::default());

    // Q1. SELECT count(*) FROM runs WHERE status = 'error'
    println!("\n2. How many runs failed?");
    let t = Instant::now();
    let mut errors = 0u64;
    scan_rows(&rows_path, |r| errors += (r.status == "error") as u64)?;
    println!(
        "  rows:    {errors:>8} errors in {:>6.0} ms, reading {}",
        t.elapsed().as_secs_f64() * 1e3,
        mb(row_bytes)
    );
    let q1 = || async {
        runs_df()
            .await?
            .filter(col("status").eq(lit("error")))?
            .aggregate(vec![], vec![count(lit(1))])
    };
    let t = Instant::now();
    let out = q1().await?.collect().await?;
    let ms = t.elapsed().as_secs_f64() * 1e3;
    let got = out[0]
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .map_or(0, |a| a.value(0));
    let plan = pretty_format_batches(&q1().await?.explain(false, true)?.collect().await?)?.to_string();
    println!("  columns: {got:>8} errors in {ms:>6.0} ms, {}", scan_metrics(&plan));

    // Q2. SELECT count(*) FROM runs WHERE start_ms >= <newest 1%>
    println!("\n3. How many runs started in the last 1% of the time range?");
    let t = Instant::now();
    let mut recent = 0u64;
    scan_rows(&rows_path, |r| recent += (r.start_ms >= cutoff) as u64)?;
    println!(
        "  rows:    {recent:>8} runs   in {:>6.0} ms, reading {}",
        t.elapsed().as_secs_f64() * 1e3,
        mb(row_bytes)
    );
    let q2 = || async {
        runs_df()
            .await?
            .filter(col("start_ms").gt_eq(lit(cutoff)))?
            .aggregate(vec![], vec![count(lit(1))])
    };
    let t = Instant::now();
    let out = q2().await?.collect().await?;
    let ms = t.elapsed().as_secs_f64() * 1e3;
    let got = out[0]
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .map_or(0, |a| a.value(0));
    let plan = pretty_format_batches(&q2().await?.explain(false, true)?.collect().await?)?.to_string();
    println!("  columns: {got:>8} runs   in {ms:>6.0} ms, {}", scan_metrics(&plan));

    // Q3. SELECT name, avg(latency_ms) FROM runs GROUP BY name
    println!("\n4. Average latency per tool");
    let t = Instant::now();
    let mut sums = std::collections::HashMap::<String, (i64, i64)>::new();
    scan_rows(&rows_path, |r| {
        let e = sums.entry(r.name.clone()).or_default();
        e.0 += r.latency_ms;
        e.1 += 1;
    })?;
    println!(
        "  rows:    {} groups in {:>6.0} ms, reading {}",
        sums.len(),
        t.elapsed().as_secs_f64() * 1e3,
        mb(row_bytes)
    );
    let q3 = || async {
        runs_df()
            .await?
            .aggregate(vec![col("name")], vec![avg(col("latency_ms")).alias("avg_latency_ms")])
    };
    let t = Instant::now();
    let out = q3().await?.sort(vec![col("name").sort(true, false)])?.collect().await?;
    let ms = t.elapsed().as_secs_f64() * 1e3;
    let plan = pretty_format_batches(&q3().await?.explain(false, true)?.collect().await?)?.to_string();
    println!(
        "  columns: {} groups in {ms:>6.0} ms, {}",
        out.iter().map(|b| b.num_rows()).sum::<usize>(),
        scan_metrics(&plan)
    );
    println!("{}", pretty_format_batches(&out)?);
    Ok(())
}
