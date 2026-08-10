//! Daemon-owned transfer registry and authenticated IPC connection serving.
//!
//! This module is the process boundary for I-12: a client connection may submit and observe a
//! transfer, but it never owns the task that performs it. Disconnecting a session drops only that
//! session; the registry and its spawned engine task remain daemon-owned.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use downpour_engine::{SegmentedDownload, StorageLayout};
use downpour_http::{H1H2Backend, TransferProtocol as _, TransportMode};
use downpour_ipc::{
    CommandHandler, DownloadId, DownloadView, ErrorData, LocalStream, PROTOCOL_VERSION, Request,
    Response, RpcError, Session, SessionError, SessionToken, StateResult, SystemStatus,
    TransportError, WireState, encode_response,
};
use downpour_storage::metadata::{
    DownloadMetadata, DownloadState, IdentityMetadata, MetadataError, MetadataStore, PublicUrl,
    UrlReference,
};
use thiserror::Error;
use url::Url;

/// The daemon's durable state could not be opened, read, or written.
#[derive(Debug, thiserror::Error)]
pub enum DaemonStateError {
    /// The metadata store refused, or was written by a newer schema (I-11).
    #[error("daemon metadata store failed: {0}")]
    Metadata(#[from] MetadataError),
    /// Startup reconciliation could not complete.
    #[error("daemon startup recovery failed: {0}")]
    Startup(#[from] crate::startup::StartupError),
    /// A guarded structure was poisoned by a panic in another task.
    #[error("daemon transfer state is unavailable")]
    Unavailable,
}

/// Move a download's durable record to a terminal state.
///
/// Best effort by design: the bytes are already on disk and the transfer is over, so a store that
/// refuses here must not turn a finished download into a failed one. It is logged rather than
/// swallowed, because a record stuck at `Transferring` is one the next process will reconcile
/// against a part file that is no longer there.
fn record_terminal(
    store: &Arc<Mutex<MetadataStore>>,
    id: &DownloadId,
    state: DownloadState,
    covered: u64,
    error_kind: Option<&str>,
) {
    if let Err(error) = write_terminal(store, id, state, covered, error_kind) {
        tracing::warn!(%error, ?state, "could not record a download's final state");
    }
}

fn write_terminal(
    store: &Arc<Mutex<MetadataStore>>,
    id: &DownloadId,
    state: DownloadState,
    covered: u64,
    error_kind: Option<&str>,
) -> Result<(), DaemonStateError> {
    let stored = stored_id(id)?;
    let mut store = store.lock().map_err(|_| DaemonStateError::Unavailable)?;
    let Some(mut metadata) = store.load_download(stored)? else {
        return Ok(());
    };
    metadata.state = state;
    metadata.covered_bytes = covered;
    metadata.updated_at_ms = now_ms();
    metadata.error_kind = match error_kind {
        Some(kind) => Some(downpour_storage::metadata::DownloadErrorKind::new(kind)?),
        None => None,
    };
    store.save_download(&metadata)?;
    Ok(())
}

/// Write the durable record that makes this download recoverable by a later process.
///
/// Everything here comes from the probe, which is why it cannot happen earlier. The identity is
/// what I-8 requires be persisted and replayed on resume, and the validator inside it is what I-3
/// compares against when the resumed transfer asks for a range.
fn persist_probed(
    store: &Arc<Mutex<MetadataStore>>,
    id: &DownloadId,
    remote: &downpour_types::RemoteObject,
    target_dir: &std::path::Path,
    journal_dir: &std::path::Path,
) -> Result<(), DaemonStateError> {
    let stored = stored_id(id)?;
    let final_path = target_dir.join(
        remote
            .suggested_filename
            .clone()
            .unwrap_or_else(|| "download".to_owned()),
    );
    let mut part_name = std::ffi::OsString::from(final_path.as_os_str());
    part_name.push(".dppart");

    let current = UrlReference::parse(remote.final_url.as_str(), None)?;
    let origin = PublicUrl::parse(&format!(
        "{}://{}",
        remote.final_url.scheme(),
        remote.final_url.authority()
    ))?;
    let mut redirect_chain = Vec::with_capacity(remote.redirect_chain.len());
    for hop in &remote.redirect_chain {
        redirect_chain.push(UrlReference::parse(hop.as_str(), None)?);
    }
    let now = now_ms();

    let metadata = DownloadMetadata {
        id: stored,
        // The state an unclean shutdown would find: this record exists precisely so a later
        // process can tell that a transfer was in flight when the process died.
        state: DownloadState::Transferring,
        created_at_ms: now,
        updated_at_ms: now,
        target_path: final_path,
        part_path: PathBuf::from(part_name),
        total_length: remote.total_length,
        covered_bytes: 0,
        queue_position: None,
        priority: 0,
        error_kind: None,
        space_reserved: false,
        url_history: Vec::new(),
        identity: IdentityMetadata {
            current_url: current.clone(),
            final_url: Some(current),
            redirect_chain,
            page_url: None,
            origin,
            validator: remote.validator.clone(),
            server_digest: remote.digest.clone(),
            content_type: remote.content_type.as_ref().map(ToString::to_string),
            suggested_filename: remote.suggested_filename.clone(),
            request_context_ref: None,
            probed_at_ms: now,
            protocol: remote.protocol,
            range_support: remote.range_support.clone(),
        },
    };
    let _ = journal_dir;
    let mut store = store.lock().map_err(|_| DaemonStateError::Unavailable)?;
    store.save_download(&metadata)?;
    Ok(())
}

/// The IPC form of a stored download id.
///
/// The two types hold the same sixteen bytes: the IPC one as 32 lowercase hex characters, the
/// stored one as a validated UUIDv7. `fresh_download_id` mints them so this conversion is total
/// in the direction that matters, and B-48 is what happens when it is not.
fn wire_id(id: downpour_storage::metadata::DownloadId) -> Result<DownloadId, &'static str> {
    let mut hex = String::with_capacity(32);
    for byte in id.as_bytes() {
        use std::fmt::Write as _;
        write!(hex, "{byte:02x}").map_err(|_| "identifier could not be rendered")?;
    }
    DownloadId::new(&hex)
}

/// The stored form of an IPC download id.
fn stored_id(id: &DownloadId) -> Result<downpour_storage::metadata::DownloadId, MetadataError> {
    let text = id.as_str();
    let mut bytes = [0_u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        let pair = text
            .get(index * 2..index * 2 + 2)
            .ok_or(MetadataError::InvalidDownloadId {
                reason: "identifier is shorter than sixteen bytes",
            })?;
        *byte = u8::from_str_radix(pair, 16).map_err(|_| MetadataError::InvalidDownloadId {
            reason: "identifier is not hexadecimal",
        })?;
    }
    downpour_storage::metadata::DownloadId::try_from_bytes(bytes)
}

/// The wire state a stored lifecycle state presents as.
const fn wire_state(state: DownloadState) -> WireState {
    match state {
        DownloadState::Submitted => WireState::Submitted,
        DownloadState::Probing => WireState::Probing,
        DownloadState::Planned => WireState::Planned,
        DownloadState::Transferring => WireState::Transferring,
        DownloadState::Paused => WireState::Paused,
        DownloadState::Stalled => WireState::Stalled,
        // The wire has no `AwaitingRefresh`, and inventing one here would be an IPC contract
        // change for a state nothing in S3 can produce — URL refresh is S8. `Paused` rather than
        // `Stalled` because both mean stopped, but `Stalled` promises automatic progress under
        // backoff and this state is waiting for a human or the extension to supply a URL.
        // Recorded as B-50; the wire needs the variant before S8 makes it reachable.
        DownloadState::AwaitingRefresh => WireState::Paused,
        DownloadState::Verifying => WireState::Verifying,
        DownloadState::Completed => WireState::Completed,
        DownloadState::Failed => WireState::Failed,
    }
}

/// Milliseconds since the Unix epoch, for records that carry a timestamp.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Connections used when a client does not ask for a number.
///
/// One, deliberately: S3's pool is fixed rather than measured, so a default above one would open
/// connections nobody asked for and no evidence justifies. The adaptive controller that earns a
/// higher number is S4 (I-7).
pub const DEFAULT_CONNECTIONS: usize = 1;

/// Hard ceiling on a client-requested connection count.
///
/// The user's setting is a ceiling, and this is the ceiling on the ceiling: a client asking for
/// hundreds of connections is asking to be rate-limited or blocked by the origin, which makes the
/// download slower and is the failure I-7 exists to prevent.
pub const MAX_CONNECTIONS: usize = 16;

/// How many connections a client's request actually turns into.
///
/// A ceiling, never a target (I-7). `None` means the client expressed no preference and gets
/// [`DEFAULT_CONNECTIONS`]; zero is not a preference, it is a request for a pool that cannot make
/// progress, and is refused rather than silently read as one.
#[must_use]
pub fn effective_connections(requested: Option<u16>) -> Option<usize> {
    match requested {
        Some(0) => None,
        Some(requested) => Some(usize::from(requested).min(MAX_CONNECTIONS)),
        None => Some(DEFAULT_CONNECTIONS),
    }
}

/// Filesystem and fixed-pool settings owned by one daemon instance.
#[derive(Clone, Debug)]
pub struct TransferConfig {
    /// Default target directory when an add request supplies none.
    pub target_dir: PathBuf,
    /// Directory containing recovery journals.
    pub journal_dir: PathBuf,
    /// SQLite database holding every download's durable record.
    ///
    /// The daemon's in-memory registry is a cache of this and nothing more: a transfer that is
    /// not written here cannot be found by the process that comes after this one, which is the
    /// whole of B-25.
    pub database_path: PathBuf,
    /// Protocol selection owned by this daemon instance.
    pub transport_mode: TransportMode,
}

/// Cloneable command endpoint whose spawned transfers outlive client sessions.
#[derive(Clone)]
pub struct TransferDaemon {
    config: Arc<TransferConfig>,
    registry: Arc<Mutex<BTreeMap<DownloadId, DownloadRecord>>>,
    /// Guarded rather than opened per call because `rusqlite::Connection` is `Send` but not
    /// `Sync`, and every transfer task needs to write its own progress.
    store: Arc<Mutex<MetadataStore>>,
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
    /// Create a daemon command endpoint and open its durable store.
    ///
    /// No transfer is started until `download.add` is handled, and nothing is recovered until
    /// [`Self::recover`] is called — docs/04 §5 step (f) is explicit that recovery establishes
    /// what is true and stops, because auto-resuming ten downloads on boot is a good way to be
    /// uninstalled.
    ///
    /// # Errors
    ///
    /// When the metadata store cannot be opened, or was written by a newer schema (I-11).
    pub fn new(config: TransferConfig) -> Result<Self, DaemonStateError> {
        let store = MetadataStore::open(&config.database_path).map_err(DaemonStateError::from)?;
        Ok(Self {
            config: Arc::new(config),
            registry: Arc::new(Mutex::new(BTreeMap::new())),
            store: Arc::new(Mutex::new(store)),
            started: Instant::now(),
        })
    }

    /// Reconcile every unclean download and seed the registry from what survived.
    ///
    /// Returns how many downloads are usable afterwards. Nothing is resumed: a recovered download
    /// is `Paused` and waits to be asked.
    ///
    /// # Errors
    ///
    /// When the store cannot be read. One download failing to reconcile is not an error — that
    /// download is recorded as failed and the others continue.
    pub fn recover(&mut self) -> Result<usize, DaemonStateError> {
        let journal_dir = self.config.journal_dir.clone();
        let mut store = self
            .store
            .lock()
            .map_err(|_| DaemonStateError::Unavailable)?;
        // Reconcile first: it is what turns an interrupted download into a `Paused` one, and it
        // deliberately skips anything already terminal.
        let summary = crate::startup::recover_all(&mut store, &journal_dir, now_ms())?;
        let resumable = summary
            .downloads()
            .iter()
            .filter(|record| record.is_recovered())
            .count();

        // Then seed from the store rather than from the recovery pass. The registry is a cache of
        // what is persisted, and a download that reconciliation skipped because it had already
        // finished is still a download this daemon must be able to answer questions about — a
        // completed transfer disappearing on restart is the same class of wrong as a lost one.
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| DaemonStateError::Unavailable)?;
        for metadata in store.load_downloads()? {
            let Ok(id) = wire_id(metadata.id) else {
                continue;
            };
            registry.insert(
                id,
                DownloadRecord {
                    state: wire_state(metadata.state),
                    covered: metadata.covered_bytes,
                    total: metadata.total_length,
                    error_kind: metadata
                        .error_kind
                        .as_ref()
                        .map(|kind| kind.as_str().to_owned()),
                },
            );
        }
        Ok(resumable)
    }

    fn add(&self, params: downpour_ipc::AddParams) -> Response {
        let url = match Url::parse(params.url.expose()) {
            Ok(url) if matches!(url.scheme(), "http" | "https") => url,
            Ok(_) | Err(_) => {
                return rpc_error("invalid_url", "download URL is not HTTP(S)", false, None);
            }
        };
        let Some(connections) = effective_connections(params.options.connections) else {
            return rpc_error(
                "invalid_connections",
                "connection count must be greater than zero",
                false,
                None,
            );
        };
        let id = match fresh_download_id() {
            Ok(id) => id,
            Err(message) => return rpc_error("id_generation_failed", message, true, None),
        };
        let target_dir = params
            .target
            .map_or_else(|| self.config.target_dir.clone(), PathBuf::from);
        let target_dir_for_record = target_dir.clone();
        // The daemon allocates the id, so the journal is named from it rather than from a hash of
        // the URL — which is what startup recovery looks for (B-51).
        let layout = match stored_id(&id) {
            Ok(stored) => StorageLayout::new(target_dir, &self.config.journal_dir)
                .with_transfer_id(stored.as_bytes()),
            Err(_) => {
                return rpc_error(
                    "invalid_id",
                    "minted download id is not storable",
                    true,
                    None,
                );
            }
        };
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
        let store = Arc::clone(&self.store);
        let task_id = id.clone();
        let transport_mode = self.config.transport_mode;
        let journal_dir = self.config.journal_dir.clone();
        let record_target = target_dir_for_record.clone();
        tokio::spawn(async move {
            update_state(&registry, &task_id, WireState::Probing, 0, None, None);
            let outcome = match H1H2Backend::new(transport_mode) {
                Ok(backend) => {
                    let backend = std::sync::Arc::new(backend);
                    // Persist what the probe established, before the first byte is written.
                    //
                    // Not at `download.add`: before the probe there is no representation length,
                    // no validator and no part path, so a record written then would name a file
                    // that does not exist and carry nothing a resume could build on. This is the
                    // moment the download becomes recoverable by a later process (B-25).
                    if let Ok(remote) = backend
                        .probe(downpour_http::ProbeRequest::new(url.clone()))
                        .await
                        && let Err(error) =
                            persist_probed(&store, &task_id, &remote, &record_target, &journal_dir)
                    {
                        // A transfer that cannot be recorded is one no later process can resume.
                        // It still runs — losing the bytes helps nobody — but the operator is
                        // told, because silent non-recoverability is the failure this task exists
                        // to remove.
                        tracing::warn!(%error, "download will not be recoverable: its record could not be written");
                    }
                    update_state(&registry, &task_id, WireState::Transferring, 0, None, None);
                    SegmentedDownload::new(backend, connections)
                        .download(url, &layout)
                        .await
                }
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
                    // The record has to reach a terminal state too. Left at `Transferring`, the
                    // next process reconciles a download whose part file was renamed away and
                    // fails it as part-file-missing — a finished download reported as broken.
                    record_terminal(
                        &store,
                        &task_id,
                        DownloadState::Completed,
                        length.unwrap_or(0),
                        None,
                    );
                }
                Err(error) => {
                    update_state(
                        &registry,
                        &task_id,
                        WireState::Failed,
                        0,
                        None,
                        Some(error.kind().to_owned()),
                    );
                    record_terminal(
                        &store,
                        &task_id,
                        DownloadState::Failed,
                        0,
                        Some(error.kind()),
                    );
                }
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

/// Mint a fresh download identifier.
///
/// These bytes are not only the IPC id. They become the SQLite primary key and the recovery
/// journal's transfer id, and the metadata store validates them as a UUIDv7 — so 128 random bits
/// are not enough, they satisfy the version and variant bits by accident about once in sixty-four.
///
/// UUIDv7 rather than v4 because the leading 48 bits are a millisecond timestamp: ids sort by
/// creation, which keeps the SQLite primary-key index appending rather than inserting into the
/// middle, and makes a directory of journals readable in the order the downloads started.
///
/// # Errors
///
/// When the operating system's random source refuses, or the clock is before the Unix epoch.
pub fn fresh_download_id() -> Result<DownloadId, &'static str> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "system clock is before the Unix epoch")?
        .as_millis();
    let millis = u64::try_from(millis).map_err(|_| "system clock is beyond a 48-bit epoch")?;

    let token = SessionToken::generate().map_err(|_| "OS random source refused an ID")?;
    let wire = token.to_wire();
    let random = wire.expose();
    let mut bytes = [0_u8; 16];
    // 48-bit big-endian milliseconds, per RFC 9562 section 5.7.
    bytes[0..6].copy_from_slice(&millis.to_be_bytes()[2..8]);
    for (index, byte) in bytes[6..].iter_mut().enumerate() {
        *byte = u8::from_str_radix(
            random
                .get(index * 2..index * 2 + 2)
                .ok_or("token was shorter than the identifier needs")?,
            16,
        )
        .map_err(|_| "token was not hexadecimal")?;
    }
    // Version 7 in the high nibble of byte 6, RFC 4122 variant in the top two bits of byte 8.
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    let mut hex = String::with_capacity(32);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(hex, "{byte:02x}").map_err(|_| "identifier could not be rendered")?;
    }
    DownloadId::new(&hex)
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
