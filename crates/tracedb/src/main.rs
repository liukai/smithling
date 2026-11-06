use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tracedb::{ServeConfig, serve};
use tracedb_ingest::Budget;
use tracedb_ingest::budget::{DEFAULT_MAX_RUNS, parse_bytes};

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
        /// Memory budget for runs held in memory (M0 keeps everything in memory), e.g. 2GB.
        #[arg(long, default_value = "2GB", value_parser = parse_size)]
        max_memory: u64,
        #[arg(long, default_value_t = DEFAULT_MAX_RUNS)]
        max_runs: usize,
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
}

fn parse_size(s: &str) -> Result<u64, String> {
    parse_bytes(s).ok_or_else(|| format!("invalid size {s:?}; use e.g. 512MB or 2GB"))
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,tower_http=warn".into()),
        )
        .init();
    match Cli::parse().cmd {
        Cmd::Serve {
            data,
            port,
            bind,
            max_memory,
            max_runs,
        } => {
            let cfg = ServeConfig {
                data_dir: data,
                addr: SocketAddr::new(bind, port),
                budget: Budget {
                    max_bytes: max_memory,
                    max_runs,
                },
            };
            let shutdown = async {
                let _ = tokio::signal::ctrl_c().await;
            };
            match serve(cfg, shutdown).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("tracedb: {e:#}");
                    ExitCode::FAILURE
                }
            }
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
