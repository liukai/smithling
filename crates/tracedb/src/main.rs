use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tracedb::{ServeConfig, parse_size, serve};
use tracedb_ingest::Limits;

#[derive(Parser)]
#[command(name = "tracedb", version, about = "Local trace store and viewer for AI agents")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run tracedb: accept agent traces and serve the browser UI.
    Serve {
        #[arg(long, default_value = "./.tracedb")]
        data: PathBuf,
        #[arg(long, default_value_t = 7070)]
        port: u16,
        #[arg(long, default_value = "127.0.0.1")]
        bind: IpAddr,
        /// Where data files go: a folder or `s3://bucket/prefix` (S3 settings from `AWS_*`). Default: <data>/data.
        #[arg(long, env = "TRACEDB_STORAGE")]
        storage: Option<String>,
        /// The catalog: `sqlite://path` or `postgres://…` (password from the URL or `PGPASSWORD`, never logged).
        #[arg(long, env = "TRACEDB_METASTORE")]
        metastore: Option<String>,
        /// Freeze and flush the in-memory batch at this size.
        #[arg(long, default_value = "128MB", value_parser = parse_size)]
        memtable: u64,
        /// …or when its oldest event is this old.
        #[arg(long, default_value = "10m", value_parser = parse_duration_us)]
        flush_age: i64,
        /// How long an identical retry is recognized as a duplicate.
        #[arg(long, default_value = "10m", value_parser = parse_duration_us)]
        dedup_window: i64,
        /// M1 memory budget; no longer applies (data moves to disk).
        #[arg(long, hide = true)]
        max_memory: Option<String>,
        /// M1 run limit; no longer applies.
        #[arg(long, hide = true)]
        max_runs: Option<String>,
        /// Serve UI files from this folder instead of the built-in copy (for UI development).
        #[arg(long, env = "TRACEDB_UI_DIR")]
        ui_dir: Option<PathBuf>,
    },
    /// Generate realistic research-agent traces and send them to a running tracedb.
    Seed {
        #[arg(long, default_value_t = 100)]
        traces: usize,
        #[arg(long, default_value_t = 42)]
        seed: u64,
        #[arg(long, default_value = "http://localhost:7070")]
        url: String,
        /// Send late-finishing steps' updates right away instead of 30–120 s later.
        #[arg(long)]
        no_delay: bool,
    },
    /// Run a query against a running tracedb and print a table (or JSON lines).
    Query(tracedb::cli_query::QueryArgs),
    /// Load the overnight-eval scenario and run the M2 checks; prints a pass/fail table.
    Demo(tracedb::demo::DemoArgs),
}

/// `10m`, `30s`, `2h` → microseconds.
fn parse_duration_us(s: &str) -> Result<i64, String> {
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: i64 = n
        .parse()
        .map_err(|_| format!("invalid duration {s:?}; use e.g. 30s, 10m or 2h"))?;
    let us = match unit {
        "s" => 1_000_000,
        "m" => 60_000_000,
        "h" => 3_600_000_000,
        _ => return Err(format!("invalid duration {s:?}; use e.g. 30s, 10m or 2h")),
    };
    Ok(n * us)
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,tower_http=warn".into()),
        )
        .init();
    // Usage errors exit 1 (contracts/cli.md: 2 means the server isn't reachable).
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            let _ = e.print();
            return if e.use_stderr() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    match cli.cmd {
        Cmd::Serve {
            data,
            port,
            bind,
            storage,
            metastore,
            memtable,
            flush_age,
            dedup_window,
            max_memory,
            max_runs,
            ui_dir,
        } => {
            if max_memory.is_some() || max_runs.is_some() {
                eprintln!("tracedb: --max-memory and --max-runs no longer apply: data moves to disk (M2)");
            }
            let cfg = ServeConfig {
                data_dir: data,
                addr: SocketAddr::new(bind, port),
                storage,
                metastore,
                limits: Limits {
                    memtable_bytes: memtable,
                    flush_age_us: flush_age,
                    dedup_window_us: dedup_window,
                    ..Limits::default()
                },
                ui_dir,
            };
            let shutdown = async {
                let _ = tokio::signal::ctrl_c().await;
            };
            match serve(cfg, shutdown).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("tracedb: {}", tracedb_storage::redact(&format!("{e:#}")));
                    ExitCode::FAILURE
                }
            }
        }
        Cmd::Demo(args) => {
            let code = tracedb::demo::run(&args, &mut std::io::stdout().lock(), &mut std::io::stderr()).await;
            ExitCode::from(code)
        }
        Cmd::Query(args) => {
            let code = tracedb::cli_query::run(&args, &mut std::io::stdout().lock(), &mut std::io::stderr()).await;
            ExitCode::from(code)
        }
        Cmd::Seed {
            traces,
            seed,
            url,
            no_delay,
        } => {
            let generated = tracedb::seed::generate(seed, traces, tracedb_model::Ts::now().0);
            let steps: usize = generated.iter().map(|t| t.steps.len()).sum();
            let late = generated
                .iter()
                .flat_map(|t| &t.steps)
                .filter(|s| s.late_patch.is_some())
                .count();
            if !no_delay && late > 0 {
                eprintln!(
                    "{late} steps finish late; their updates follow over the next 2 minutes (use --no-delay to skip the wait)"
                );
            }
            match tracedb::seed::send(url.trim_end_matches('/'), &generated, no_delay).await {
                Ok((posts, patches)) => {
                    println!("sent {traces} traces ({steps} steps): {posts} starts, {patches} late updates");
                    ExitCode::SUCCESS
                }
                Err(e @ tracedb::seed::SeedError::Unreachable(..)) => {
                    eprintln!("tracedb seed: {e}");
                    ExitCode::from(2)
                }
                Err(e) => {
                    eprintln!("tracedb seed: {e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
