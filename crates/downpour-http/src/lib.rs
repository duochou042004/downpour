//! Protocol backends for Downpour.
//!
//! The engine's entire view of the network is the [`TransferProtocol`] trait (ADR-0005). Nothing
//! above this crate names an HTTP library, which is what lets an experimental HTTP/3 stack be
//! added or removed without touching the scheduler, and what lets the deterministic simulation
//! substitute the network wholesale.
//!
//! Invariants this crate owns:
//!
//! | Invariant | How |
//! | --------- | --- |
//! | I-6 — range support is proven | [`h1h2::H1H2Backend::probe`] is the only source of `RangeSupport::Proven`, and it gets one from a pure validator over an observed response |
//! | I-5 — encoding never corrupts offsets | Every request sends `Accept-Encoding: identity`, no decompression feature is enabled, and a non-identity response is rejected before the first byte reaches the sink |
//! | I-8 — redirect state is captured | Redirects are followed by hand so the whole chain is recorded, not just the final URL |
//! | I-2 (precondition) — no overlapping writes | [`sink::RangeSink`] is append-only from a fixed base offset; a backend has no API for choosing an offset |

#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable
    )
)]

pub mod download;
pub mod error;
pub mod h1h2;
pub mod protocol;
pub mod retry;
pub mod sink;

pub use download::{DownloadError, SingleStream};
pub use error::{ProbeError, TransferError};
pub use h1h2::{H1H2Backend, TransportMode};
pub use protocol::{
    BackendCapabilities, ProbeRequest, RangeOutcome, RangeRequest, TransferProtocol,
};
pub use retry::{RetryDecision, RetryPolicy, RetryState, TransientKind};
pub use sink::{RangeSink, SinkError, SinkTarget};
