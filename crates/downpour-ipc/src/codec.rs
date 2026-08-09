//! Bounded frame and strict JSON-RPC codec.

use tokio::io::{AsyncRead, AsyncReadExt};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{Request, Response, ResponseKind};

/// Maximum JSON payload accepted after the four-byte length header.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Framing or typed JSON-RPC validation failure.
#[derive(Debug, Error)]
pub enum CodecError {
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
    let (method, params) = match _request {
        Request::Hello(params) => ("hello", serde_json::to_value(params)),
        Request::DownloadAdd(params) => ("download.add", serde_json::to_value(params)),
        Request::DownloadGet(params) => ("download.get", serde_json::to_value(params)),
        Request::DownloadResume(params) => ("download.resume", serde_json::to_value(params)),
        Request::SystemStatus(params) => ("system.status", serde_json::to_value(params)),
    };
    let envelope = RequestEnvelope {
        jsonrpc: "2.0",
        id: _id,
        method,
        params: params.map_err(|_| CodecError::InvalidMessage)?,
    };
    encode_json(&envelope)
}

/// Decode one JSON-RPC request payload after framing.
pub fn decode_request(_payload: &[u8]) -> Result<(u64, Request), CodecError> {
    let envelope: OwnedRequestEnvelope =
        serde_json::from_slice(_payload).map_err(|_| CodecError::InvalidMessage)?;
    if envelope.jsonrpc != "2.0" {
        return Err(CodecError::InvalidMessage);
    }
    let request = match envelope.method.as_str() {
        "hello" => Request::Hello(decode_params(envelope.params)?),
        "download.add" => Request::DownloadAdd(decode_params(envelope.params)?),
        "download.get" => Request::DownloadGet(decode_params(envelope.params)?),
        "download.resume" => Request::DownloadResume(decode_params(envelope.params)?),
        "system.status" => Request::SystemStatus(decode_params(envelope.params)?),
        _ => return Err(CodecError::InvalidMessage),
    };
    Ok((envelope.id, request))
}

/// Encode one typed response, including its four-byte frame header.
pub fn encode_response(_id: u64, _response: &Response) -> Result<Vec<u8>, CodecError> {
    match _response {
        Response::Error(error) => encode_json(&ErrorEnvelope {
            jsonrpc: "2.0",
            id: _id,
            error,
        }),
        Response::Hello(result) => encode_success(_id, result),
        Response::Added(result) | Response::Resumed(result) => encode_success(_id, result),
        Response::Download(result) => encode_success(_id, result),
        Response::Status(result) => encode_success(_id, result),
    }
}

/// Decode one JSON-RPC response payload according to the request registered for its ID.
pub fn decode_response(
    _payload: &[u8],
    _expected: ResponseKind,
) -> Result<(u64, Response), CodecError> {
    let value: Value = serde_json::from_slice(_payload).map_err(|_| CodecError::InvalidMessage)?;
    if value.get("error").is_some() {
        let envelope: OwnedErrorEnvelope =
            serde_json::from_value(value).map_err(|_| CodecError::InvalidMessage)?;
        if envelope.jsonrpc != "2.0" {
            return Err(CodecError::InvalidMessage);
        }
        return Ok((envelope.id, Response::Error(envelope.error)));
    }
    let envelope: OwnedSuccessEnvelope =
        serde_json::from_value(value).map_err(|_| CodecError::InvalidMessage)?;
    if envelope.jsonrpc != "2.0" {
        return Err(CodecError::InvalidMessage);
    }
    let response = match _expected {
        ResponseKind::Hello => Response::Hello(decode_params(envelope.result)?),
        ResponseKind::Added => Response::Added(decode_params(envelope.result)?),
        ResponseKind::Download => Response::Download(decode_params(envelope.result)?),
        ResponseKind::Resumed => Response::Resumed(decode_params(envelope.result)?),
        ResponseKind::Status => Response::Status(decode_params(envelope.result)?),
    };
    Ok((envelope.id, response))
}

/// Read one bounded payload without allocating an oversized declared body.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>, CodecError> {
    let mut header = [0_u8; 4];
    reader
        .read_exact(&mut header)
        .await
        .map_err(|_| CodecError::TruncatedHeader)?;
    let declared =
        usize::try_from(u32::from_le_bytes(header)).map_err(|_| CodecError::InvalidMessage)?;
    if declared > MAX_FRAME_BYTES {
        return Err(CodecError::FrameTooLarge {
            declared,
            maximum: MAX_FRAME_BYTES,
        });
    }
    let mut payload = vec![0_u8; declared];
    reader
        .read_exact(&mut payload)
        .await
        .map_err(|_| CodecError::TruncatedBody)?;
    Ok(payload)
}

#[derive(Serialize)]
struct RequestEnvelope {
    jsonrpc: &'static str,
    id: u64,
    method: &'static str,
    params: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnedRequestEnvelope {
    jsonrpc: String,
    id: u64,
    method: String,
    params: Value,
}

#[derive(Serialize)]
struct SuccessEnvelope<'a, T> {
    jsonrpc: &'static str,
    id: u64,
    result: &'a T,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnedSuccessEnvelope {
    jsonrpc: String,
    id: u64,
    result: Value,
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    jsonrpc: &'static str,
    id: u64,
    error: &'a crate::RpcError,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnedErrorEnvelope {
    jsonrpc: String,
    id: u64,
    error: crate::RpcError,
}

fn encode_success<T: Serialize>(id: u64, result: &T) -> Result<Vec<u8>, CodecError> {
    encode_json(&SuccessEnvelope {
        jsonrpc: "2.0",
        id,
        result,
    })
}

fn encode_json(value: &impl Serialize) -> Result<Vec<u8>, CodecError> {
    let payload = serde_json::to_vec(value).map_err(|_| CodecError::InvalidMessage)?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(CodecError::FrameTooLarge {
            declared: payload.len(),
            maximum: MAX_FRAME_BYTES,
        });
    }
    let length = u32::try_from(payload.len()).map_err(|_| CodecError::FrameTooLarge {
        declared: payload.len(),
        maximum: MAX_FRAME_BYTES,
    })?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&length.to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

fn decode_params<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, CodecError> {
    serde_json::from_value(value).map_err(|_| CodecError::InvalidMessage)
}
