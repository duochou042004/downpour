//! Typed version-1 request, result, and error payloads.

use std::fmt;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

/// The only IPC major understood by this build.
pub const PROTOCOL_VERSION: u32 = 1;

/// One decoded S3 request after method-specific schema validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Request {
    /// Authenticate and negotiate the public contract.
    Hello(HelloParams),
    /// Submit a new download.
    DownloadAdd(AddParams),
    /// Read one download.
    DownloadGet(IdParams),
    /// Resume one paused download.
    DownloadResume(IdParams),
    /// Read daemon status.
    SystemStatus(VersionParams),
}

/// One typed result or stable JSON-RPC fault.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Response {
    /// Successful hello result.
    Hello(HelloResult),
    /// Successful add result.
    Added(StateResult),
    /// Successful get result.
    Download(DownloadView),
    /// Successful resume result.
    Resumed(StateResult),
    /// Successful daemon status result.
    Status(SystemStatus),
    /// Stable machine-readable failure.
    Error(RpcError),
}

/// Expected result schema for a JSON-RPC response ID.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseKind {
    /// Hello result.
    Hello,
    /// Add result.
    Added,
    /// Download view.
    Download,
    /// Resume result.
    Resumed,
    /// System status.
    Status,
}

/// Version negotiation and authentication parameters.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelloParams {
    /// Requested public IPC major.
    pub protocol_version: u32,
    /// Human-readable client build identifier.
    pub client: String,
    /// Redacted-on-debug session token wire text.
    pub token: SecretString,
}

/// Successful version negotiation result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelloResult {
    /// Negotiated public IPC major.
    pub protocol_version: u32,
    /// Daemon build identifier.
    pub daemon_version: String,
    /// Additive feature names available to the client.
    pub capabilities: Vec<String>,
}

/// S3's fixed-worker options accepted by `download.add`.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AddOptions {
    /// Requested HTTP/1.1 worker ceiling, when explicitly supplied.
    pub connections: Option<u16>,
}

/// Minimal S3 add request. Browser context is additive in S7.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AddParams {
    /// Public IPC major carried by this message.
    pub protocol_version: u32,
    /// User-submitted HTTP(S) URL. It is secret-bearing and must never be logged.
    pub url: SecretString,
    /// Optional native target path text supplied by the local client.
    pub target: Option<String>,
    /// Transfer options available in S3.
    pub options: AddOptions,
}

/// Validated opaque download identity used on the wire.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct DownloadId(String);

impl DownloadId {
    /// Validate a bounded lowercase hexadecimal download identifier.
    pub fn new(raw: &str) -> Result<Self, &'static str> {
        if raw.len() != 32
            || !raw
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err("download id must be exactly 32 lowercase hexadecimal characters");
        }
        Ok(Self(raw.to_owned()))
    }

    /// Borrow the stable wire representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for DownloadId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::new(&raw).map_err(D::Error::custom)
    }
}

/// Secret-bearing wire text whose formatting is always redacted.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    /// Wrap text that may cross IPC but must never appear in formatting or logs.
    #[must_use]
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// Expose the value only at an explicit protocol or credential boundary.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted]")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted]")
    }
}

/// Common versioned request for a single download.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IdParams {
    /// Public IPC major carried by this message.
    pub protocol_version: u32,
    /// Target download.
    pub id: DownloadId,
}

/// Version-only parameters for parameterless system methods.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VersionParams {
    /// Public IPC major carried by this message.
    pub protocol_version: u32,
}

/// Stable lifecycle names exposed to clients.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WireState {
    /// Accepted and queued.
    Submitted,
    /// Capability probe in flight.
    Probing,
    /// Identity established and transfer planned.
    Planned,
    /// One or more workers active.
    Transferring,
    /// User-paused or recovered but not auto-resumed.
    Paused,
    /// Waiting under bounded retry backoff.
    Stalled,
    /// All bytes present and verification running.
    Verifying,
    /// Verified and named.
    Completed,
    /// Unrecoverable failure with evidence retained.
    Failed,
}

/// Common state-changing result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StateResult {
    /// Public IPC major carried by this message.
    pub protocol_version: u32,
    /// Affected download.
    pub id: DownloadId,
    /// State after the command.
    pub state: WireState,
}

/// Query projection for one download.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadView {
    /// Public IPC major carried by this message.
    pub protocol_version: u32,
    /// Download identity.
    pub id: DownloadId,
    /// Current daemon-owned state.
    pub state: WireState,
    /// Durably covered bytes.
    pub covered: u64,
    /// Known representation length.
    pub total: Option<u64>,
}

/// Minimal daemon status returned in S3.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SystemStatus {
    /// Public IPC major carried by this message.
    pub protocol_version: u32,
    /// Daemon build identifier.
    pub version: String,
    /// Whole seconds since this daemon instance started.
    pub uptime_seconds: u64,
    /// Active transfer count.
    pub active: u32,
    /// Queued transfer count.
    pub queued: u32,
    /// Aggregate bytes per second.
    pub throughput: u64,
}

/// Stable JSON-RPC error data.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorData {
    /// Public IPC major carried by this message.
    pub protocol_version: u32,
    /// Stable machine-readable error name.
    pub kind: String,
    /// Related download, when any.
    pub download_id: Option<DownloadId>,
    /// Whether a later request may recover.
    pub recoverable: bool,
    /// Stable suggested client action.
    pub suggestion: Option<String>,
}

/// Typed JSON-RPC failure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RpcError {
    /// JSON-RPC application error code.
    pub code: i32,
    /// Human-readable text; clients must switch on `data.kind` instead.
    pub message: String,
    /// Stable structured failure data.
    pub data: ErrorData,
}
