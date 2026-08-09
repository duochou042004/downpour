//! Hello-first authentication and dispatch boundary.

use thiserror::Error;

use crate::{
    CodecError, HelloResult, PROTOCOL_VERSION, Request, Response, SessionToken, decode_request,
};

/// Daemon method adapter reached only by authenticated typed commands.
pub trait CommandHandler {
    /// Execute one already validated non-hello command.
    fn handle(&mut self, request: Request) -> Response;
}

/// Authentication, version, schema, or dispatch failure.
#[derive(Debug, Error)]
pub enum SessionError {
    /// A frame did not decode to a typed request.
    #[error("IPC request was invalid: {0}")]
    Codec(#[from] CodecError),
    /// A non-hello method arrived before authentication.
    #[error("IPC hello is required before any command")]
    AuthenticationRequired,
    /// The hello token did not match this daemon session.
    #[error("IPC session token was refused")]
    AuthenticationFailed,
    /// The requested public protocol major is newer than this daemon.
    #[error("IPC protocol version {requested} is newer than supported version {supported}")]
    UnsupportedVersion {
        /// Client-requested major.
        requested: u32,
        /// Newest daemon-supported major.
        supported: u32,
    },
    /// A connection may negotiate only once.
    #[error("IPC hello was repeated after authentication")]
    HelloRepeated,
}

/// State for one client connection. It owns no transfer handle.
pub struct Session {
    token: SessionToken,
    authenticated: bool,
}

impl Session {
    /// Start an unauthenticated connection session.
    #[must_use]
    pub const fn new(token: SessionToken) -> Self {
        Self {
            token,
            authenticated: false,
        }
    }

    /// Decode, authenticate, and optionally dispatch one JSON payload.
    pub fn handle_payload<H: CommandHandler>(
        &mut self,
        payload: &[u8],
        handler: &mut H,
    ) -> Result<(u64, Response), SessionError> {
        let (id, request) = decode_request(payload)?;
        if let Request::Hello(params) = request {
            if self.authenticated {
                return Err(SessionError::HelloRepeated);
            }
            ensure_version(params.protocol_version)?;
            if !self.token.matches_wire(params.token.expose()) {
                return Err(SessionError::AuthenticationFailed);
            }
            self.authenticated = true;
            return Ok((
                id,
                Response::Hello(HelloResult {
                    protocol_version: PROTOCOL_VERSION,
                    daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
                    capabilities: vec!["h1-segments".to_owned()],
                }),
            ));
        }
        if !self.authenticated {
            return Err(SessionError::AuthenticationRequired);
        }
        ensure_version(request_version(&request))?;
        Ok((id, handler.handle(request)))
    }
}

fn request_version(request: &Request) -> u32 {
    match request {
        Request::Hello(params) => params.protocol_version,
        Request::DownloadAdd(params) => params.protocol_version,
        Request::DownloadGet(params) | Request::DownloadResume(params) => params.protocol_version,
        Request::SystemStatus(params) => params.protocol_version,
    }
}

fn ensure_version(requested: u32) -> Result<(), SessionError> {
    if requested == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(SessionError::UnsupportedVersion {
            requested,
            supported: PROTOCOL_VERSION,
        })
    }
}
