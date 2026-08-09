//! Downpour's bounded, versioned, authenticated local IPC contract.
//!
//! This crate owns I-11's IPC format and I-13's daemon-side validation boundary. It contains no
//! transfer engine or storage dependency: clients can speak the public contract without gaining a
//! second path to download state.

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

mod codec;
mod session;
mod token;
mod transport;
mod types;

pub use codec::{
    CodecError, MAX_FRAME_BYTES, decode_request, decode_response, encode_request, encode_response,
    read_frame,
};
pub use session::{CommandHandler, Session, SessionError};
pub use token::{SessionToken, TokenError};
pub use transport::{EndpointPaths, LocalListener, LocalStream, TransportError, read_client_token};
pub use types::{
    AddOptions, AddParams, DownloadId, DownloadView, ErrorData, HelloParams, HelloResult, IdParams,
    PROTOCOL_VERSION, Request, Response, ResponseKind, RpcError, SecretString, StateResult,
    SystemStatus, VersionParams, WireState,
};
