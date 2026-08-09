//! Bounded frame and strict JSON-RPC codec.

use tokio::io::{AsyncRead, AsyncReadExt};

use crate::{Request, Response, ResponseKind};
use thiserror::Error;

/// Maximum JSON payload accepted after the four-byte length header.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Framing or typed JSON-RPC validation failure.
#[derive(Debug, Error)]
pub enum CodecError {
    /// The deliberate red-proof scaffold has no codec yet.
    #[error("IPC codec is not implemented")]
    Unavailable,
    /// The declared body exceeded the pre-allocation ceiling.
    #[error("IPC frame declares {declared} bytes, above the {maximum}-byte limit")]
    FrameTooLarge {
        /// Untrusted declared body length.
        declared: usize,
        /// Fixed accepted maximum.
        maximum: usize,
    },
    /// The stream ended before the four-byte frame header.
    #[error("IPC frame header was truncated")]
    TruncatedHeader,
    /// The stream ended inside the declared body.
    #[error("IPC frame body was truncated")]
    TruncatedBody,
    /// JSON-RPC envelope or method-specific schema validation failed.
    #[error("IPC JSON-RPC message is malformed")]
    InvalidMessage,
}

/// Encode one typed request, including its four-byte frame header.
pub fn encode_request(_id: u64, _request: &Request) -> Result<Vec<u8>, CodecError> {
    Err(CodecError::Unavailable)
}

/// Decode one JSON-RPC request payload after framing.
pub fn decode_request(_payload: &[u8]) -> Result<(u64, Request), CodecError> {
    Err(CodecError::Unavailable)
}

/// Encode one typed response, including its four-byte frame header.
pub fn encode_response(_id: u64, _response: &Response) -> Result<Vec<u8>, CodecError> {
    Err(CodecError::Unavailable)
}

/// Decode one JSON-RPC response payload according to the request registered for its ID.
pub fn decode_response(
    _payload: &[u8],
    _expected: ResponseKind,
) -> Result<(u64, Response), CodecError> {
    Err(CodecError::Unavailable)
}

/// Read one bounded payload without allocating an oversized declared body.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>, CodecError> {
    let mut header = [0_u8; 4];
    reader
        .read_exact(&mut header)
        .await
        .map_err(|_| CodecError::TruncatedHeader)?;
    Err(CodecError::Unavailable)
}
