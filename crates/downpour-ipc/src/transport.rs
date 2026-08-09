//! User-scoped local endpoint and byte-stream boundary.

use std::fs;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::io;
#[cfg(unix)]
use std::io::Write as _;
use std::path::{Path, PathBuf};

use interprocess::local_socket::tokio::{Listener as NativeListener, Stream as NativeStream};
use interprocess::local_socket::traits::tokio::{Listener as _, Stream as _};
#[cfg(unix)]
use interprocess::local_socket::{GenericFilePath, ListenerOptions};
use thiserror::Error;
use tokio::io::AsyncWriteExt as _;

use crate::{CodecError, SecretString, SessionToken, read_frame};

/// Native endpoint creation, connection, or byte-stream failure.
#[derive(Debug, Error)]
pub enum TransportError {
    /// A platform filesystem, socket, pipe, or stream operation failed.
    #[error("local IPC transport I/O failed: {0}")]
    Io(#[from] io::Error),
    /// A received frame violated the public framing contract.
    #[error("local IPC frame was invalid: {0}")]
    Codec(#[from] CodecError),
    /// The client token file did not contain one canonical token.
    #[error("local IPC token file was malformed")]
    InvalidTokenFile,
}

/// Stable filesystem locations associated with one daemon runtime root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndpointPaths {
    runtime_dir: PathBuf,
    token_file: PathBuf,
    #[cfg(unix)]
    socket_path: PathBuf,
    #[cfg(windows)]
    pipe_name: String,
}

impl EndpointPaths {
    /// Discover an existing daemon endpoint without creating or changing its protected state.
    pub fn discover(runtime_root: &Path) -> Result<Self, TransportError> {
        client_paths(runtime_root)
    }

    /// User-private Downpour runtime directory.
    #[must_use]
    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    /// Atomically replaced token file for this daemon session.
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

    /// Local-only Windows named-pipe identifier.
    #[cfg(windows)]
    #[must_use]
    pub fn pipe_name(&self) -> &str {
        &self.pipe_name
    }
}

/// Bound user-scoped listener and its per-daemon authentication token.
pub struct LocalListener {
    inner: NativeListener,
    paths: EndpointPaths,
    token: SessionToken,
}

impl LocalListener {
    /// Provision a fresh endpoint beneath the supplied platform runtime root.
    pub fn bind(runtime_root: &Path) -> Result<Self, TransportError> {
        let paths = prepare_paths(runtime_root)?;
        let inner = create_listener(&paths)?;
        let token = SessionToken::generate().map_err(io::Error::other)?;
        provision_token(&paths, &token)?;
        Ok(Self {
            inner,
            paths,
            token,
        })
    }

    /// Return the protected paths clients use to find this daemon.
    #[must_use]
    pub const fn paths(&self) -> &EndpointPaths {
        &self.paths
    }

    /// Clone the token used to authenticate new connection sessions.
    #[must_use]
    pub fn session_token(&self) -> SessionToken {
        self.token.clone()
    }

    /// Accept one local client byte stream.
    pub async fn accept(&self) -> Result<LocalStream, TransportError> {
        Ok(LocalStream {
            inner: self.inner.accept().await?,
        })
    }
}

/// One connected native local byte stream.
pub struct LocalStream {
    inner: NativeStream,
}

impl LocalStream {
    /// Connect to an already provisioned daemon endpoint.
    pub async fn connect(paths: &EndpointPaths) -> Result<Self, TransportError> {
        let name = native_name(paths)?;
        Ok(Self {
            inner: NativeStream::connect(name).await?,
        })
    }

    /// Send one already framed IPC message.
    pub async fn send(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        self.inner.write_all(frame).await?;
        Ok(())
    }

    /// Receive one bounded IPC payload without its length header.
    pub async fn receive(&mut self) -> Result<Vec<u8>, TransportError> {
        Ok(read_frame(&mut self.inner).await?)
    }
}

/// Read the protected token text used by a local client hello.
pub fn read_client_token(paths: &EndpointPaths) -> Result<SecretString, TransportError> {
    let wire = fs::read_to_string(&paths.token_file)?;
    if wire.len() != 64
        || !wire
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(TransportError::InvalidTokenFile);
    }
    Ok(SecretString::new(wire))
}

#[cfg(unix)]
fn prepare_paths(runtime_root: &Path) -> Result<EndpointPaths, TransportError> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};

    let runtime_dir = runtime_root.join("downpour");
    match fs::DirBuilder::new().mode(0o700).create(&runtime_dir) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(&runtime_dir)?;
            if !metadata.file_type().is_dir() || metadata.permissions().mode() & 0o777 != 0o700 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Downpour runtime directory is not a private real directory",
                )
                .into());
            }
            let root_metadata = fs::metadata(runtime_root)?;
            if metadata.uid() != root_metadata.uid() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Downpour runtime directory owner differs from its runtime root",
                )
                .into());
            }
        }
        Err(error) => return Err(error.into()),
    }
    Ok(EndpointPaths {
        token_file: runtime_dir.join("session.token"),
        socket_path: runtime_dir.join("daemon.sock"),
        runtime_dir,
    })
}

#[cfg(unix)]
fn client_paths(runtime_root: &Path) -> Result<EndpointPaths, TransportError> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let runtime_dir = runtime_root.join("downpour");
    let metadata = fs::symlink_metadata(&runtime_dir)?;
    if !metadata.file_type().is_dir() || metadata.permissions().mode() & 0o777 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Downpour runtime directory is not a private real directory",
        )
        .into());
    }
    let root_metadata = fs::metadata(runtime_root)?;
    if metadata.uid() != root_metadata.uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Downpour runtime directory owner differs from its runtime root",
        )
        .into());
    }
    Ok(EndpointPaths {
        token_file: runtime_dir.join("session.token"),
        socket_path: runtime_dir.join("daemon.sock"),
        runtime_dir,
    })
}

#[cfg(unix)]
fn create_listener(paths: &EndpointPaths) -> Result<NativeListener, TransportError> {
    use interprocess::os::unix::local_socket::ListenerOptionsExt as _;

    let name = native_name(paths)?;
    Ok(ListenerOptions::new()
        .name(name)
        .mode(0o600)
        .create_tokio()?)
}

#[cfg(unix)]
fn native_name(
    paths: &EndpointPaths,
) -> Result<interprocess::local_socket::Name<'_>, TransportError> {
    use interprocess::local_socket::ToFsName as _;

    Ok(paths
        .socket_path
        .as_path()
        .to_fs_name::<GenericFilePath>()?)
}

#[cfg(unix)]
fn provision_token(paths: &EndpointPaths, token: &SessionToken) -> Result<(), TransportError> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let nonce = SessionToken::generate().map_err(io::Error::other)?;
    let temporary = paths
        .runtime_dir
        .join(format!(".session.token.{}", nonce.to_wire().expose()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(token.to_wire().expose().as_bytes())?;
    file.sync_all()?;
    drop(file);
    if let Err(error) = fs::rename(&temporary, &paths.token_file) {
        let cleanup = fs::remove_file(&temporary);
        if let Err(cleanup_error) = cleanup {
            return Err(io::Error::other(format!(
                "token install failed ({error}); temporary cleanup also failed ({cleanup_error})"
            ))
            .into());
        }
        return Err(error.into());
    }
    Ok(())
}

#[cfg(windows)]
mod windows;

#[cfg(windows)]
use windows::{client_paths, create_listener, native_name, prepare_paths, provision_token};
