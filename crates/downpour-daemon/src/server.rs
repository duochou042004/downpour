//! Daemon-owned transfer registry and authenticated IPC connection serving.
//!
//! This module is the process boundary for I-12: a client connection may submit and observe a
//! transfer, but it never owns the task that performs it. Disconnecting a session drops only that
//! session; the registry and its spawned engine task remain daemon-owned.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use downpour_engine::{SingleStream, StorageLayout};
use downpour_http::{H1H2Backend, TransportMode};
use downpour_ipc::{
    CommandHandler, DownloadId, DownloadView, ErrorData, LocalStream, PROTOCOL_VERSION, Request,
    Response, RpcError, Session, SessionError, SessionToken, StateResult, SystemStatus,
    TransportError, WireState, encode_response,
};
use thiserror::Error;
use url::Url;

/// Filesystem and fixed-pool settings owned by one daemon instance.
#[derive(Clone, Debug)]
pub struct TransferConfig {
    /// Default target directory when an add request supplies none.
    pub target_dir: PathBuf,
    /// Directory containing recovery journals.
    pub journal_dir: PathBuf,
    /// Protocol selection owned by this daemon instance.
    pub transport_mode: TransportMode,
}

/// Cloneable command endpoint whose spawned transfers outlive client sessions.
#[derive(Clone)]
pub struct TransferDaemon {
    config: Arc<TransferConfig>,
    registry: Arc<Mutex<BTreeMap<DownloadId, DownloadRecord>>>,
    started: Instant,
}

#[derive(Clone, Debug)]
struct DownloadRecord {
    state: WireState,
    covered: u64,
    total: Option<u64>,
    error_kind: Option<String>,
}

impl TransferDaemon {
    /// Create a daemon command endpoint. No transfer is started until `download.add` is handled.
    #[must_use]
    pub fn new(config: TransferConfig) -> Self {
        Self {
            config: Arc::new(config),
            registry: Arc::new(Mutex::new(BTreeMap::new())),
            started: Instant::now(),
        }
    }

    fn add(&self, params: downpour_ipc::AddParams) -> Response {
        let url = match Url::parse(params.url.expose()) {
            Ok(url) if matches!(url.scheme(), "http" | "https") => url,
            Ok(_) | Err(_) => {
                return rpc_error("invalid_url", "download URL is not HTTP(S)", false, None);
            }
        };
        match params.options.connections {
            Some(0) => {
                return rpc_error(
                    "invalid_connections",
                    "connection count must be greater than zero",
                    false,
                    None,
                );
            }
            None | Some(1) => {}
            Some(_) => {
                return rpc_error(
                    "segmented_execution_not_ready",
                    "daemon completion is not yet wired to the segmented worker pool",
                    false,
                    None,
                );
            }
        }
        let id = match fresh_download_id() {
            Ok(id) => id,
            Err(message) => return rpc_error("id_generation_failed", message, true, None),
        };
        let target_dir = params
            .target
            .map_or_else(|| self.config.target_dir.clone(), PathBuf::from);
        let layout = StorageLayout::new(target_dir, &self.config.journal_dir);
        {
            let mut registry = match self.registry.lock() {
                Ok(registry) => registry,
                Err(_) => {
                    return rpc_error(
                        "state_unavailable",
                        "daemon transfer state is unavailable",
                        true,
                        None,
                    );
                }
            };
            registry.insert(
                id.clone(),
                DownloadRecord {
                    state: WireState::Submitted,
                    covered: 0,
                    total: None,
                    error_kind: None,
                },
            );
        }

        let registry = Arc::clone(&self.registry);
        let task_id = id.clone();
        let transport_mode = self.config.transport_mode;
        tokio::spawn(async move {
            update_state(&registry, &task_id, WireState::Probing, 0, None, None);
            let outcome = match H1H2Backend::new(transport_mode) {
                Ok(backend) => SingleStream::new(backend).download(url, &layout).await,
                Err(_) => {
                    update_state(
                        &registry,
                        &task_id,
                        WireState::Failed,
                        0,
                        None,
                        Some("backend_unavailable".to_owned()),
                    );
                    return;
                }
            };
            match outcome {
                Ok(path) => {
                    let length = tokio::fs::metadata(path).await.ok().map(|item| item.len());
                    update_state(
                        &registry,
                        &task_id,
                        WireState::Completed,
                        length.unwrap_or(0),
                        length,
                        None,
                    );
                }
                Err(error) => update_state(
                    &registry,
                    &task_id,
                    WireState::Failed,
                    0,
                    None,
                    Some(error.kind().to_owned()),
                ),
            }
        });

        Response::Added(StateResult {
            protocol_version: PROTOCOL_VERSION,
            id,
            state: WireState::Submitted,
        })
    }

    fn get(&self, id: DownloadId) -> Response {
        let registry = match self.registry.lock() {
            Ok(registry) => registry,
            Err(_) => {
                return rpc_error(
                    "state_unavailable",
                    "daemon transfer state is unavailable",
                    true,
                    Some(id),
                );
            }
        };
        let Some(record) = registry.get(&id) else {
            return rpc_error("not_found", "download was not found", false, Some(id));
        };
        Response::Download(DownloadView {
            protocol_version: PROTOCOL_VERSION,
            id,
            state: record.state,
            covered: record.covered,
            total: record.total,
            error_kind: record.error_kind.clone(),
        })
    }

    fn status(&self) -> Response {
        let registry = match self.registry.lock() {
            Ok(registry) => registry,
            Err(_) => {
                return rpc_error(
                    "state_unavailable",
                    "daemon transfer state is unavailable",
                    true,
                    None,
                );
            }
        };
        let active = registry
            .values()
            .filter(|record| {
                matches!(
                    record.state,
                    WireState::Probing
                        | WireState::Planned
                        | WireState::Transferring
                        | WireState::Verifying
                )
            })
            .count();
        let queued = registry
            .values()
            .filter(|record| record.state == WireState::Submitted)
            .count();
        Response::Status(SystemStatus {
            protocol_version: PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            uptime_seconds: self.started.elapsed().as_secs(),
            active: u32::try_from(active).unwrap_or(u32::MAX),
            queued: u32::try_from(queued).unwrap_or(u32::MAX),
            throughput: 0,
        })
    }
}

impl CommandHandler for TransferDaemon {
    fn handle(&mut self, request: Request) -> Response {
        match request {
            Request::DownloadAdd(params) => self.add(params),
            Request::DownloadGet(params) => self.get(params.id),
            Request::DownloadResume(params) => rpc_error(
                "resume_not_ready",
                "cross-process resume is not available until S3-T9",
                true,
                Some(params.id),
            ),
            Request::SystemStatus(_) => self.status(),
            Request::Hello(_) => rpc_error(
                "invalid_dispatch",
                "hello must be handled by the authenticated session",
                false,
                None,
            ),
        }
    }
}

/// One authenticated client connection could not be served.
#[derive(Debug, Error)]
pub enum ServerError {
    /// The native byte stream failed or carried an invalid frame.
    #[error("daemon IPC transport failed: {0}")]
    Transport(#[from] TransportError),
    /// Authentication, version negotiation, or typed dispatch failed.
    #[error("daemon IPC session failed: {0}")]
    Session(#[from] SessionError),
    /// A typed response could not be encoded under the public contract.
    #[error("daemon IPC response encoding failed: {0}")]
    Codec(#[from] downpour_ipc::CodecError),
}

/// Serve one client without tying any spawned transfer to the connection lifetime.
pub async fn serve_connection<H: CommandHandler + Send + 'static>(
    mut stream: LocalStream,
    token: SessionToken,
    mut handler: H,
) -> Result<(), ServerError> {
    let mut session = Session::new(token);
    loop {
        let payload = stream.receive().await?;
        let (id, response) = session.handle_payload(&payload, &mut handler)?;
        let frame = encode_response(id, &response)?;
        stream.send(&frame).await?;
    }
}

fn fresh_download_id() -> Result<DownloadId, &'static str> {
    let token = SessionToken::generate().map_err(|_| "OS random source refused an ID")?;
    let wire = token.to_wire();
    DownloadId::new(&wire.expose()[..32])
}

fn update_state(
    registry: &Arc<Mutex<BTreeMap<DownloadId, DownloadRecord>>>,
    id: &DownloadId,
    state: WireState,
    covered: u64,
    total: Option<u64>,
    error_kind: Option<String>,
) {
    let Ok(mut registry) = registry.lock() else {
        return;
    };
    if let Some(record) = registry.get_mut(id) {
        record.state = state;
        record.covered = covered;
        record.total = total;
        record.error_kind = error_kind;
    }
}

fn rpc_error(
    kind: &str,
    message: &str,
    recoverable: bool,
    download_id: Option<DownloadId>,
) -> Response {
    Response::Error(RpcError {
        code: -32_000,
        message: message.to_owned(),
        data: ErrorData {
            protocol_version: PROTOCOL_VERSION,
            kind: kind.to_owned(),
            download_id,
            recoverable,
            suggestion: recoverable.then(|| "retry".to_owned()),
        },
    })
}
