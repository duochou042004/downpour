//! `downpourd` process lifecycle and platform-state wiring.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context as _;
use downpour_daemon::server::{TransferConfig, TransferDaemon, serve_connection};
use downpour_http::TransportMode;
use downpour_ipc::LocalListener;

fn main() -> ExitCode {
    init_tracing();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("downpourd: could not start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("downpourd: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> anyhow::Result<()> {
    let runtime_root = runtime_root()?;
    let data_root = data_root()?;
    let journal_dir = data_root.join("journals");
    std::fs::create_dir_all(&runtime_root)
        .with_context(|| format!("creating runtime root {}", runtime_root.display()))?;
    std::fs::create_dir_all(&journal_dir)
        .with_context(|| format!("creating journal directory {}", journal_dir.display()))?;
    let target_dir = std::env::current_dir().context("resolving the default target directory")?;
    let listener = LocalListener::bind(&runtime_root).context("binding the local IPC endpoint")?;
    let token = listener.session_token();
    let daemon = TransferDaemon::new(TransferConfig {
        target_dir,
        journal_dir,
        transport_mode: TransportMode::Negotiated,
    });
    loop {
        let stream = listener
            .accept()
            .await
            .context("accepting a local client")?;
        let connection_token = token.clone();
        let connection_daemon = daemon.clone();
        tokio::spawn(async move {
            if let Err(error) = serve_connection(stream, connection_token, connection_daemon).await
            {
                tracing::debug!(%error, "local client disconnected");
            }
        });
    }
}

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

fn data_root() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("DOWNPOUR_DATA_ROOT") {
        return Ok(PathBuf::from(path));
    }
    let dirs = directories::ProjectDirs::from("", "", "downpour")
        .context("no application-data directory for this user")?;
    Ok(dirs.data_dir().to_path_buf())
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("DOWNPOUR_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
