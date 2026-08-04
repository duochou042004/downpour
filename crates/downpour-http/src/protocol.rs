//! The `TransferProtocol` boundary.
//!
//! This module owns the abstraction ADR-0005 exists for: **the engine never touches an HTTP
//! library.** Two things depend on that holding.
//!
//! 1. HTTP/3's Rust ecosystem is still explicitly experimental (`h3` is at 0.0.8 and documents
//!    that its API may change). Building the scheduler on it directly would mean the scheduler
//!    churns whenever the library does.
//! 2. The deterministic simulation (`docs/09-testing-strategy.md` §4) has to substitute the
//!    entire network layer. It can, because the simulator is just another backend.
//!
//! **Design rule from ADR-0005:** if the scheduler ever needs to know which HTTP version it is
//! talking to, that knowledge flows through [`BackendCapabilities`] — never a downcast, never a
//! version check inside the scheduler. The reversal trigger is capability flags multiplying to
//! let the scheduler special-case a backend; that means the abstraction has failed and should be
//! redesigned rather than removed, because the simulation depends on it.

use std::time::Duration;

use async_trait::async_trait;
use downpour_types::{ByteRangeSpec, ContentRange, NegotiatedProtocol, RemoteObject};
use url::Url;

use crate::error::{ProbeError, TransferError};
use crate::sink::RangeSink;

/// Ask the remote what it will actually let us do.
#[derive(Debug, Clone)]
pub struct ProbeRequest {
    /// Where to start. The probe follows redirects from here and records the chain.
    pub url: Url,
    /// Request headers to replay — referer, cookies, and anything else that made the transfer
    /// work in the browser (I-8). Empty for a bare URL.
    pub headers: Vec<(String, String)>,
    /// How many redirect hops to follow before giving up. Bounds a redirect loop.
    pub max_redirects: u32,
    /// Overall probe deadline. `PROBE_TIMEOUT` in `docs/03-transfer-engine-spec.md` §9.
    pub timeout: Duration,
}

impl ProbeRequest {
    /// A probe of `url` with the spec defaults and no replayed context.
    #[must_use]
    pub fn new(url: Url) -> Self {
        Self {
            url,
            headers: Vec::new(),
            // Ten is what browsers use; the exact number matters less than it being finite.
            max_redirects: 10,
            timeout: Duration::from_secs(15),
        }
    }
}

/// Fetch one byte range, or the whole representation when `range` is `None`.
#[derive(Debug, Clone)]
pub struct RangeRequest {
    /// The URL to fetch. Already resolved — the caller passes the final URL from the probe, so
    /// a signed URL is not re-resolved and invalidated (I-8).
    pub url: Url,
    /// The range to ask for, or `None` for the whole representation.
    pub range: Option<ByteRangeSpec>,
    /// Headers to replay.
    pub headers: Vec<(String, String)>,
    /// Deadline for the whole fetch.
    pub timeout: Duration,
}

impl RangeRequest {
    /// A whole-representation fetch of `url`.
    #[must_use]
    pub fn whole(url: Url) -> Self {
        Self {
            url,
            range: None,
            headers: Vec::new(),
            timeout: Duration::from_secs(300),
        }
    }

    /// A fetch of one range of `url`.
    #[must_use]
    pub fn ranged(url: Url, range: ByteRangeSpec) -> Self {
        Self {
            url,
            range: Some(range),
            headers: Vec::new(),
            timeout: Duration::from_secs(300),
        }
    }
}

/// What a fetch actually delivered — not what it was asked for.
///
/// The distinction is the point. A server may deliver less than requested, and the engine has to
/// know the difference so the remainder goes back to the allocator instead of being assumed
/// present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeOutcome {
    /// Bytes accepted by the sink.
    pub bytes_delivered: u64,
    /// The response status.
    pub status: u16,
    /// The validated `Content-Range`, when the response carried a usable one.
    pub content_range: Option<ContentRange>,
    /// Which protocol carried it.
    pub protocol: NegotiatedProtocol,
    /// Whether the body ended before its declared length.
    pub truncated: bool,
}

/// What a backend can offer.
///
/// This is the *only* channel through which the scheduler learns anything protocol-specific
/// (ADR-0005). Adding a flag here to let the scheduler special-case one backend is the signal
/// that the abstraction needs redesigning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendCapabilities {
    /// Stable identifier, for logs and `dp explain`.
    pub name: &'static str,
    /// Protocols this backend may negotiate.
    pub protocols: Vec<NegotiatedProtocol>,
    /// Whether extra capacity can be added as streams on an existing connection rather than as
    /// new connections. This is what `docs/03-transfer-engine-spec.md` §3.4 keys off, and it is
    /// why "32 sockets" is no longer automatically the right answer.
    pub multiplexes_streams: bool,
    /// Whether the backend can express a byte-range request at all. A backend that cannot is
    /// restricted to single-stream transfers regardless of what the server supports.
    pub supports_ranges: bool,
}

/// The engine's entire view of the network.
///
/// Object-safe on purpose: backends are selected at runtime, and the simulator is one of them.
/// `crates/downpour-http/tests/trait_object.rs` asserts that property directly, because losing
/// it would be a silent, compile-time-only regression that breaks the simulation.
#[async_trait]
pub trait TransferProtocol: Send + Sync {
    /// Discover what the remote will actually let us do. Never trusts advertisement (I-6).
    async fn probe(&self, request: ProbeRequest) -> Result<RemoteObject, ProbeError>;

    /// Fetch one range into `sink`. Returns what was actually delivered.
    async fn fetch_range(
        &self,
        request: RangeRequest,
        sink: &mut RangeSink,
    ) -> Result<RangeOutcome, TransferError>;

    /// What this backend can offer.
    fn capabilities(&self) -> BackendCapabilities;
}
