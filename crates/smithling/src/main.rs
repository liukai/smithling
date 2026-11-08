use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use smithling_ingest::Limits;
use smithling_server::{ServeConfig, parse_size, serve};

/// Returns freed memory to the OS (the system allocator kept ~1 GB after a load); tuned in `.cargo/config.toml`.
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[derive(Parser)]
#[command(name = "smithling", version, about = "Local trace store and viewer for AI agents")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run smithling: accept agent traces and serve the browser UI.
    Serve {
        #[arg(long, default_value = "./.smithling")]
        data: PathBuf,
        #[arg(long, default_value_t = 7070)]
        port: u16,
        #[arg(long, default_value = "127.0.0.1")]
        bind: IpAddr,
        /// Where data files go: a folder or `s3://bucket/prefix` (S3 settings from `AWS_*`). Default: <data>/data.
        #[arg(long, env = "SMITHLING_STORAGE")]
        storage: Option<String>,
        /// The catalog: `sqlite://path` or `postgres://…` (password from the URL or `PGPASSWORD`, never logged).
        #[arg(long, env = "SMITHLING_METASTORE")]
        metastore: Option<String>,
        /// Freeze and flush the in-memory batch when its heap reaches this size (M2: 128MB, counted as JSON).
        #[arg(long, default_value = "64MB", value_parser = parse_size)]
        memtable: u64,
        /// …or when its oldest event is this old (M2: 10m).
        #[arg(long, default_value = "2m", value_parser = parse_duration_us)]
        flush_age: i64,
        /// How long an identical retry is recognized as a duplicate.
        #[arg(long, default_value = "10m", value_parser = parse_duration_us)]
        dedup_window: i64,
        /// …remembering at most this many recent uploads (16 bytes each, plus the table).
        #[arg(long, default_value_t = smithling_ingest::dedup::DEFAULT_CAP)]
        dedup_keys: usize,
        /// Memory for file footers, index pages and payload pages read back (counted in idle memory).
        #[arg(long, default_value = "32MB", value_parser = parse_size)]
        cache: u64,
        /// M1 memory budget; no longer applies (data moves to disk).
        #[arg(long, hide = true)]
        max_memory: Option<String>,
        /// M1 run limit; no longer applies.
        #[arg(long, hide = true)]
        max_runs: Option<String>,
        /// Serve UI files from this folder instead of the built-in copy (for UI development).
        #[arg(long, env = "SMITHLING_UI_DIR")]
        ui_dir: Option<PathBuf>,
        /// Model prices (USD per 1M tokens) replacing the built-in entries by name. Default: <data>/prices.toml if present.
        #[arg(long)]
        prices: Option<PathBuf>,
    },
    #[cfg(feature = "tools")]
    /// Generate realistic research-agent traces and send them to a running smithling.
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
    #[cfg(feature = "tools")]
    /// Run a query against a running smithling and print a table (or JSON lines).
    Query(smithling_tools::cli_query::QueryArgs),
    #[cfg(feature = "tools")]
    /// Load the overnight-eval scenario and run the M2 and M3 checks (`--checks` picks); prints a pass/fail table.
    Demo(smithling_tools::demo::DemoArgs),
    #[cfg(feature = "tools")]
    /// Load the reference volume, time each query type and print P50/P99 against the targets.
    Bench(smithling_tools::bench::BenchArgs),
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

/// jemalloc's thread caches are off (`.cargo/config.toml`) except on the runtime's workers while they run: one
/// hands its cache back each time it goes idle, so an idle server keeps no cached memory (research M3-R12).
fn main() -> ExitCode {
    fn tcache(on: bool) {
        use tikv_jemalloc_ctl::{Access, AsName};
        let _ = b"thread.tcache.enabled\0".name().write(on);
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        // Turning a thread's cache off hands its contents back; blocking-pool threads never have one.
        .on_thread_unpark(|| tcache(true))
        .on_thread_park(|| tcache(false))
        .build();
    match rt {
        Ok(rt) => rt.block_on(run()),
        Err(e) => {
            eprintln!("smithling: could not start: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> ExitCode {
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
            dedup_keys,
            cache,
            max_memory,
            max_runs,
            ui_dir,
            prices,
        } => {
            if max_memory.is_some() || max_runs.is_some() {
                eprintln!("smithling: --max-memory and --max-runs no longer apply: data moves to disk (M2)");
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
                    dedup_keys,
                    cache_bytes: cache,
                    ..Limits::default()
                },
                ui_dir,
                prices,
            };
            let shutdown = async {
                let _ = tokio::signal::ctrl_c().await;
            };
            match serve(cfg, shutdown).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("smithling: {}", smithling_storage::redact(&format!("{e:#}")));
                    ExitCode::FAILURE
                }
            }
        }
        #[cfg(feature = "tools")]
        Cmd::Demo(args) => {
            let code = smithling_tools::demo::run(&args, &mut std::io::stdout().lock(), &mut std::io::stderr()).await;
            ExitCode::from(code)
        }
        #[cfg(feature = "tools")]
        Cmd::Bench(args) => {
            let code = smithling_tools::bench::run(&args, &mut std::io::stdout().lock(), &mut std::io::stderr()).await;
            ExitCode::from(code)
        }
        #[cfg(feature = "tools")]
        Cmd::Query(args) => {
            let code =
                smithling_tools::cli_query::run(&args, &mut std::io::stdout().lock(), &mut std::io::stderr()).await;
            ExitCode::from(code)
        }
        #[cfg(feature = "tools")]
        Cmd::Seed {
            traces,
            seed,
            url,
            no_delay,
        } => {
            let generated = smithling_tools::seed::generate_before(seed, traces, smithling_model::Ts::now().0);
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
            match smithling_tools::seed::send(url.trim_end_matches('/'), &generated, no_delay).await {
                Ok((posts, patches)) => {
                    println!("sent {traces} traces ({steps} steps): {posts} starts, {patches} late updates");
                    ExitCode::SUCCESS
                }
                Err(e @ smithling_tools::seed::SeedError::Unreachable(..)) => {
                    eprintln!("smithling seed: {e}");
                    ExitCode::from(2)
                }
                Err(e) => {
                    eprintln!("smithling seed: {e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
