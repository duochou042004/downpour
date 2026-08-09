//! Daemon-owned transfer registry and authenticated IPC connection serving.
//!
//! This module is the process boundary for I-12: a client connection may submit and observe a
//! transfer, but it never owns the task that performs it. The initial S3-T8 proof lands against an
//! explicit unavailable scaffold so the lifetime claim cannot pass before that boundary exists.

use std::path::PathBuf;
use std::sync::Arc;

use downpour_ipc::{
    CommandHandler, ErrorData, LocalStream, PROTOCOL_VERSION, Request, Response, RpcError,
    SessionToken,
};
use thiserror::Error;

/// Filesystem and fixed-pool settings owned by one daemon instance.
#[derive(Clone, Debug)]
pub struct TransferConfig {
    /// Default target directory when an add request supplies none.
    pub target_dir: PathBuf,
    /// Directory containing recovery journals.
    pub journal_dir: PathBuf,
    /// Default fixed HTTP/1.1 connection count.
    pub connections: usize,
}

/// Cloneable command endpoint whose spawned transfers outlive client sessions.
#[derive(Clone)]
pub struct TransferDaemon {
    config: Arc<TransferConfig>,
}

impl TransferDaemon {
    /// Create a daemon command endpoint. No transfer is started until `download.add` is handled.
    #[must_use]
    pub fn new(config: TransferConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }
}

impl CommandHandler for TransferDaemon {
    fn handle(&mut self, _request: Request) -> Response {
        let _config_is_owned_by_the_daemon = &self.config;
        Response::Error(RpcError {
            code: -32_000,
            message: "daemon transfer service is not available".to_owned(),
            data: ErrorData {
                protocol_version: PROTOCOL_VERSION,
                kind: "unavailable".to_owned(),
                download_id: None,
                recoverable: true,
                suggestion: Some("retry".to_owned()),
            },
        })
    }
}

/// One authenticated client connection could not be served.
#[derive(Debug, Error)]
pub enum ServerError {
    /// S3-T8's deliberate red scaffold has no connection loop yet.
    #[error("daemon IPC server is not available")]
    Unavailable,
}

/// Serve one client without tying any spawned transfer to the connection lifetime.
pub async fn serve_connection<H: CommandHandler + Send + 'static>(
    _stream: LocalStream,
    _token: SessionToken,
    _handler: H,
) -> Result<(), ServerError> {
    Err(ServerError::Unavailable)
}
