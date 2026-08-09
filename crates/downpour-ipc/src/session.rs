//! Hello-first authentication and dispatch boundary.

use thiserror::Error;

use crate::{CodecError, Request, Response, SessionToken};

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
    /// The deliberate red-proof scaffold has no session state machine yet.
    #[error("IPC session is not implemented")]
    Unavailable,
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
    _token: SessionToken,
}

impl Session {
    /// Start an unauthenticated connection session.
    #[must_use]
    pub const fn new(token: SessionToken) -> Self {
        Self { _token: token }
    }

    /// Decode, authenticate, and optionally dispatch one JSON payload.
    pub fn handle_payload<H: CommandHandler>(
        &mut self,
        _payload: &[u8],
        _handler: &mut H,
    ) -> Result<(u64, Response), SessionError> {
        Err(SessionError::Unavailable)
    }
}
