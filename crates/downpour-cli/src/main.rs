//! `dp` — the Downpour command-line client.
//!
//! The CLI is an authenticated IPC client. It owns no transfer, storage handle, journal, or engine
//! task; exiting or being killed can therefore affect only this client connection (I-12).
//!
//! `main` is the one place in this codebase where an error may be printed and the process may
//! exit non-zero; `anyhow` is confined to this boundary (`.claude/rules/rust.md`).

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::{Parser, Subcommand};
use downpour_ipc::{
    AddOptions, AddParams, EndpointPaths, HelloParams, IdParams, LocalStream, PROTOCOL_VERSION,
    Request, Response, ResponseKind, SecretString, WireState, decode_response, encode_request,
    read_client_token,
};
use thiserror::Error;

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

        /// Fixed HTTP/1.1 connection count requested from the daemon.
        #[arg(long)]
        connections: Option<u16>,
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
            error.exit_code()
        }
    }
}

#[derive(Debug, Error)]
enum ClientError {
    #[error("{0}")]
    General(#[source] anyhow::Error),
    #[error("daemon is unreachable: {0}")]
    Daemon(#[source] anyhow::Error),
    #[error("download failed: {0}")]
    DownloadFailed(String),
}

impl ClientError {
    fn exit_code(&self) -> ExitCode {
        match self {
            Self::General(_) => ExitCode::FAILURE,
            Self::Daemon(_) => ExitCode::from(3),
            Self::DownloadFailed(_) => ExitCode::from(4),
        }
    }
}

async fn run(command: Command) -> Result<(), ClientError> {
    match command {
        Command::Add {
            url,
            output_dir,
            connections,
        } => {
            let output_dir = match output_dir {
                Some(dir) => dir,
                None => std::env::current_dir()
                    .context("could not determine the current directory")
                    .map_err(ClientError::General)?,
            };
            let target = output_dir
                .to_str()
                .context("output directory is not representable by the IPC path schema")
                .map_err(ClientError::General)?
                .to_owned();
            let mut client = IpcClient::connect().await?;
            let response = client
                .call(
                    Request::DownloadAdd(AddParams {
                        protocol_version: PROTOCOL_VERSION,
                        url: SecretString::new(url),
                        target: Some(target),
                        options: AddOptions { connections },
                    }),
                    ResponseKind::Added,
                )
                .await?;
            let Response::Added(added) = response else {
                return Err(response_error(response));
            };
            let id = added.id;
            loop {
                let response = client
                    .call(
                        Request::DownloadGet(IdParams {
                            protocol_version: PROTOCOL_VERSION,
                            id: id.clone(),
                        }),
                        ResponseKind::Download,
                    )
                    .await?;
                match response {
                    Response::Download(view) if view.state == WireState::Completed => {
                        println!("{}", id.as_str());
                        return Ok(());
                    }
                    Response::Download(view) if view.state == WireState::Failed => {
                        return Err(ClientError::DownloadFailed(
                            view.error_kind.unwrap_or_else(|| "unknown".to_owned()),
                        ));
                    }
                    Response::Download(_) => {
                        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    }
                    other => return Err(response_error(other)),
                }
            }
        }
    }
}

struct IpcClient {
    stream: LocalStream,
    next_id: u64,
}

impl IpcClient {
    async fn connect() -> Result<Self, ClientError> {
        let runtime_root = runtime_root().map_err(ClientError::Daemon)?;
        let paths = EndpointPaths::discover(&runtime_root)
            .context("discovering the daemon endpoint")
            .map_err(ClientError::Daemon)?;
        let token = read_client_token(&paths)
            .context("reading the daemon session token")
            .map_err(ClientError::Daemon)?;
        let stream = LocalStream::connect(&paths)
            .await
            .context("connecting to downpourd")
            .map_err(ClientError::Daemon)?;
        let mut client = Self { stream, next_id: 1 };
        let response = client
            .call(
                Request::Hello(HelloParams {
                    protocol_version: PROTOCOL_VERSION,
                    client: format!("dp/{}", env!("CARGO_PKG_VERSION")),
                    token,
                }),
                ResponseKind::Hello,
            )
            .await?;
        if matches!(response, Response::Hello(_)) {
            Ok(client)
        } else {
            Err(response_error(response))
        }
    }

    async fn call(
        &mut self,
        request: Request,
        expected: ResponseKind,
    ) -> Result<Response, ClientError> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let frame = encode_request(id, &request)
            .context("encoding an IPC request")
            .map_err(ClientError::General)?;
        self.stream
            .send(&frame)
            .await
            .context("sending an IPC request")
            .map_err(ClientError::Daemon)?;
        let payload = self
            .stream
            .receive()
            .await
            .context("receiving an IPC response")
            .map_err(ClientError::Daemon)?;
        let (response_id, response) = decode_response(&payload, expected)
            .context("decoding an IPC response")
            .map_err(ClientError::Daemon)?;
        if response_id != id {
            return Err(ClientError::Daemon(anyhow::anyhow!(
                "daemon replied with response id {response_id}, expected {id}"
            )));
        }
        Ok(response)
    }
}

fn response_error(response: Response) -> ClientError {
    match response {
        Response::Error(error) => ClientError::DownloadFailed(error.data.kind),
        _ => ClientError::Daemon(anyhow::anyhow!(
            "daemon returned an unexpected response type"
        )),
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

/// Root beneath which the daemon provisions its protected socket or named-pipe token state.
///
/// Tests override it per process; production follows the platform paths in `docs/02` §7.
fn runtime_root() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("DOWNPOUR_RUNTIME_ROOT") {
        return Ok(PathBuf::from(path));
    }
    #[cfg(unix)]
    {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .context("XDG_RUNTIME_DIR is not set")
    }
    #[cfg(windows)]
    {
        let dirs = directories::ProjectDirs::from("", "", "downpour")
            .context("no local application-data directory for this user")?;
        Ok(dirs.data_local_dir().join("runtime"))
    }
}
