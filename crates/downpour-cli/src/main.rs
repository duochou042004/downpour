//! `dp` — the Downpour command-line client.
//!
//! **Stage-1 shape, not the intended architecture.** `docs/02-architecture.md` §3 states a hard
//! rule: clients never link the engine, because a client that can perform a transfer defeats the
//! daemon guarantee (I-12) — closing the window would stop the download. There is no daemon until
//! later stages, so `dp add` currently performs the transfer in-process. Backlog B-4 records the
//! move to IPC, and it is a move rather than a rewrite because everything below `SingleStream` is
//! already behind the `TransferProtocol` boundary.
//!
//! `main` is the one place in this codebase where an error may be printed and the process may
//! exit non-zero; `anyhow` is confined to this boundary (`.claude/rules/rust.md`).

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::{Parser, Subcommand};
use downpour_http::{H1H2Backend, SingleStream, TransportMode};
use url::Url;

/// A download manager that does not corrupt your files.
#[derive(Debug, Parser)]
#[command(name = "dp", version, about, long_about = None)]
struct Cli {
    /// Increase log verbosity. Repeatable.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Download a URL.
    Add {
        /// What to download.
        url: String,

        /// Where to put it. Defaults to the current directory.
        #[arg(short = 'o', long = "output-dir")]
        output_dir: Option<PathBuf>,

        /// Force HTTP/1.1 rather than negotiating.
        #[arg(long, conflicts_with = "http2_prior_knowledge")]
        http1: bool,

        /// Assume HTTP/2 without negotiating (h2c). For cleartext origins that speak HTTP/2,
        /// including the corpus server, where there is no TLS and therefore no ALPN.
        #[arg(long = "http2-prior-knowledge")]
        http2_prior_knowledge: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("dp: could not start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run(cli.command)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // The chain matters: "storing x failed" on its own does not say why, and the cause is
            // what a bug report needs.
            eprintln!("dp: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(command: Command) -> anyhow::Result<()> {
    match command {
        Command::Add {
            url,
            output_dir,
            http1,
            http2_prior_knowledge,
        } => {
            let url = Url::parse(&url).with_context(|| format!("{url} is not a valid URL"))?;
            let output_dir = match output_dir {
                Some(dir) => dir,
                None => {
                    std::env::current_dir().context("could not determine the current directory")?
                }
            };

            let mode = if http1 {
                TransportMode::Http1Only
            } else if http2_prior_knowledge {
                TransportMode::Http2PriorKnowledge
            } else {
                TransportMode::Negotiated
            };

            let backend = H1H2Backend::new(mode).context("could not build the HTTP backend")?;
            let path = SingleStream::new(backend)
                .download(url.clone(), &output_dir)
                .await
                .with_context(|| format!("downloading {url}"))?;

            // stdout carries the result and nothing else, so `dp add ... | xargs` works. Progress
            // and diagnostics go to stderr via tracing.
            println!("{}", path.display());
            Ok(())
        }
    }
}

fn init_tracing(verbosity: u8) {
    let default = match verbosity {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_env("DOWNPOUR_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    // Diagnostics go to stderr so they never contaminate the path on stdout.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
