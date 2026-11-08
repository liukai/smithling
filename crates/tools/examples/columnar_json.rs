//! The same filter on typed columns vs inside one JSON text column (see SECRET_OF_COLUMNAR_DB.md §8).
//!
//! smithling keeps promoted values (`metadata.*`, small `inputs.*` / `outputs.*` leaves) in one JSON text column,
//! `attrs`. This writes 1,000,000 rows with `prompt_version` and `status_code` both as typed columns and inside such
//! a JSON column, then runs the same filter both ways.
//!
//! ```sh
//! cargo run --release -p smithling-tools --example columnar_json
//! ```

use std::fs::File;
use std::sync::Arc;
use std::time::Instant;

use datafusion::arrow::array::{Int64Array, StringArray};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::arrow::util::pretty::pretty_format_batches;
use datafusion::functions_aggregate::expr_fn::count;
use datafusion::logical_expr::Expr;
use datafusion::parquet::arrow::ArrowWriter;
use datafusion::parquet::basic::{Compression, ZstdLevel};
use datafusion::parquet::file::properties::{EnabledStatistics, WriterProperties};
use datafusion::parquet::file::reader::{FileReader, SerializedFileReader};
use datafusion::prelude::{ParquetReadOptions, SessionConfig, SessionContext, col, lit};

const ROWS: usize = 1_000_000;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut rnd = |m: u64| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x % m
    };
    let (mut version, mut code, mut attrs) = (Vec::new(), Vec::new(), Vec::new());
    for i in 0..ROWS {
        let v = if rnd(2) == 0 { "v6" } else { "v7" };
        let c: i64 = if rnd(100) >= 5 {
            200
        } else if rnd(4) < 3 {
            429
        } else {
            500
        };
        version.push(v);
        code.push(c);
        // What smithling's `attrs` column holds for a run: its promoted leaves as JSON text.
        attrs.push(format!(
            r#"{{"metadata.prompt_version":"{v}","metadata.task_id":"t-{}","metadata.user":"u-{}","outputs.status_code":{c}}}"#,
            i / 200,
            rnd(5_000)
        ));
    }
    let schema = Arc::new(Schema::new(vec![
        Field::new("prompt_version", DataType::Utf8, false),
        Field::new("status_code", DataType::Int64, false),
        Field::new("attrs", DataType::Utf8, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from_iter_values(version)),
            Arc::new(Int64Array::from(code)),
            Arc::new(StringArray::from_iter_values(attrs.iter().map(String::as_str))),
        ],
    )?;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("runs.parquet");
    let props = WriterProperties::builder()
        .set_max_row_group_row_count(Some(65_536))
        .set_statistics_enabled(EnabledStatistics::Page)
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(3)?))
        .build();
    let mut w = ArrowWriter::try_new(File::create(&path)?, schema, Some(props))?;
    w.write(&batch)?;
    w.close()?;

    println!("On disk:");
    let reader = SerializedFileReader::new(File::open(&path)?)?;
    let meta = reader.metadata();
    for (i, c) in meta.file_metadata().schema_descr().columns().iter().enumerate() {
        let bytes: i64 = meta.row_groups().iter().map(|g| g.column(i).compressed_size()).sum();
        println!("  {:<16} {:>8.1} KB", c.name(), bytes as f64 / 1e3);
    }

    let cfg = SessionConfig::new().set_bool("datafusion.execution.parquet.pushdown_filters", true);
    let ctx = SessionContext::new_with_config(cfg);
    let file = path.to_str().unwrap();
    println!("\nprompt_version = v7 and status_code = 429:");
    for (label, filter) in [
        (
            "typed columns",
            col("prompt_version")
                .eq(lit("v7"))
                .and(col("status_code").eq(lit(429i64))),
        ),
        (
            "inside JSON text (substring)",
            // smithling's pushdown does the same: a text match on `"path":value`, then an exact check (pushdown.rs).
            col("attrs")
                .like(lit(r#"%"metadata.prompt_version":"v7"%"#))
                .and(col("attrs").like(lit(r#"%"outputs.status_code":429%"#))),
        ),
    ] {
        run(&ctx, file, label, filter).await?;
    }
    Ok(())
}

async fn run(ctx: &SessionContext, file: &str, label: &str, filter: Expr) -> Result<(), Box<dyn std::error::Error>> {
    let q = || async {
        ctx.read_parquet(file, ParquetReadOptions::default())
            .await?
            .filter(filter.clone())?
            .aggregate(vec![], vec![count(lit(1))])
    };
    let t = Instant::now();
    let out = q().await?.collect().await?;
    let ms = t.elapsed().as_secs_f64() * 1e3;
    let n = out[0]
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .map_or(0, |a| a.value(0));
    let plan = pretty_format_batches(&q().await?.explain(false, true)?.collect().await?)?.to_string();
    let read = plan
        .find("bytes_scanned=")
        .map(|at| &plan[at + 14..])
        .map(|r| &r[..r.find([',', ']']).unwrap_or(r.len())])
        .unwrap_or("?");
    println!("  {label:<30} {n:>7} matches  {ms:>5.0} ms  read {read}");
    Ok(())
}
