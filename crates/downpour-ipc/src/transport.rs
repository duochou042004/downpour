//! User-scoped local endpoint and byte-stream boundary.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::{SecretString, SessionToken};

/// Native endpoint creation, connection, or byte-stream failure.
#[derive(Debug, Error)]
pub enum TransportError {
    /// Deliberate red-proof scaffold before the native transport is built.
    #[error("local IPC transport is not implemented")]
    Unavailable,
}

/// Stable filesystem locations associated with one daemon runtime root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndpointPaths {
    runtime_dir: PathBuf,
    token_file: PathBuf,
    #[cfg(unix)]
    socket_path: PathBuf,
}

impl EndpointPaths {
    /// User-private Downpour runtime directory.
    #[must_use]
    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    /// Exclusively created token file for this daemon session.
    #[must_use]
    pub fn token_file(&self) -> &Path {
        &self.token_file
    }

    /// Filesystem Unix-domain socket path.
    #[cfg(unix)]
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

/// Bound user-scoped listener and its per-daemon authentication token.
pub struct LocalListener;

impl LocalListener {
    /// Provision a fresh endpoint beneath the supplied platform runtime root.
    pub fn bind(_runtime_root: &Path) -> Result<Self, TransportError> {
        Err(TransportError::Unavailable)
    }

    /// Return the protected paths clients use to find this daemon.
    #[must_use]
    pub fn paths(&self) -> &EndpointPaths {
        unreachable!("red-proof listener cannot be constructed")
    }

    /// Clone the token used to authenticate new connection sessions.
    #[must_use]
    pub fn session_token(&self) -> SessionToken {
        unreachable!("red-proof listener cannot be constructed")
    }

    /// Accept one local client byte stream.
    pub async fn accept(&self) -> Result<LocalStream, TransportError> {
        Err(TransportError::Unavailable)
    }
}

/// One connected native local byte stream.
pub struct LocalStream;

impl LocalStream {
    /// Connect to an already provisioned daemon endpoint.
    pub async fn connect(_paths: &EndpointPaths) -> Result<Self, TransportError> {
        Err(TransportError::Unavailable)
    }

    /// Send one already framed IPC message.
    pub async fn send(&mut self, _frame: &[u8]) -> Result<(), TransportError> {
        Err(TransportError::Unavailable)
    }

    /// Receive one bounded IPC payload without its length header.
    pub async fn receive(&mut self) -> Result<Vec<u8>, TransportError> {
        Err(TransportError::Unavailable)
    }
}

/// Read the protected token text used by a local client hello.
pub fn read_client_token(_paths: &EndpointPaths) -> Result<SecretString, TransportError> {
    Err(TransportError::Unavailable)
}
