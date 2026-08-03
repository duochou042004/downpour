//! The single-stream download: probe, fetch, verify, rename.
//!
//! This module owns **I-4** for S1: *a completed download is verified before it is named.* The
//! target is written as `<name>.dppart` and moved to its real name only after the delivered
//! length matches what the probe established. A partial file wearing the final name is
//! indistinguishable from a good one to the user and to every other program on the system, which
//! makes it worse than an obvious failure.
//!
//! **This is a stage-1 arrangement, not the intended architecture.** Orchestration belongs in
//! `downpour-engine` behind the daemon's IPC (`docs/02-architecture.md` §3, and the hard rule
//! that clients never link the engine), and durable writes belong in `downpour-storage`, which
//! owns preallocation, positional writes and the journal ordering I-1 requires. Neither crate
//! exists until S2/S3. Backlog B-4 and B-5 record the move; the [`crate::sink::SinkTarget`]
//! boundary is what makes it a substitution rather than a rewrite.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use thiserror::Error;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use url::Url;

use crate::error::{ProbeError, TransferError};
use crate::protocol::{ProbeRequest, RangeRequest, TransferProtocol};
use crate::sink::{RangeSink, SinkError, SinkTarget};

/// Extension for a download in progress. Never the final name (I-4).
pub const PART_EXTENSION: &str = "dppart";

/// A whole-file, single-connection download.
pub struct SingleStream<B> {
    backend: B,
}

impl<B: TransferProtocol> SingleStream<B> {
    /// Wrap a backend.
    pub fn new(backend: B) -> Self {
        Self { backend }
    }

    /// Probe `url`, fetch it into `target_dir`, verify it, and give it its final name.
    ///
    /// Returns the path the file ended up at. On failure the `.dppart` file is left in place when
    /// it holds bytes worth resuming from, and the final name is never created.
    ///
    /// # Errors
    ///
    /// Anything that stops a verified file existing: a probe rejection, a transport failure, a
    /// body shorter than the probe established, or a target that already exists.
    pub async fn download(&self, url: Url, target_dir: &Path) -> Result<PathBuf, DownloadError> {
        let remote = self.backend.probe(ProbeRequest::new(url)).await?;

        // Already sanitised by the probe, so it is exactly one path component and cannot escape
        // `target_dir` (S1-C3). Joining is therefore safe by construction rather than by check.
        let name = remote
            .suggested_filename
            .clone()
            .unwrap_or_else(|| "download".to_owned());
        let final_path = target_dir.join(&name);
        let part_path = target_dir.join(format!("{name}.{PART_EXTENSION}"));

        // Checked before anything is created, so a refusal leaves the directory exactly as it
        // was. Refusing rather than picking a "(1)" suffix is deliberate for S1: silently
        // replacing a file the user already has is unrecoverable, and choosing a new name is a
        // policy decision that belongs with the rest of the local-collision handling in S2.
        if tokio::fs::try_exists(&final_path).await.unwrap_or(false) {
            return Err(DownloadError::TargetExists { path: final_path });
        }

        let file = tokio::fs::File::create(&part_path)
            .await
            .map_err(|source| DownloadError::Io {
                path: part_path.clone(),
                source,
            })?;
        let target = FileTarget { file };

        // The limit is what the probe established. A server that delivers more than it declared
        // is refused at the sink rather than written, so it cannot run past the end of the
        // representation. Absent a declared length — close-delimited framing — there is nothing
        // to bound it with, and the length is whatever arrived.
        let mut sink = RangeSink::new(Box::new(target), 0, remote.total_length);

        let fetched = self
            .backend
            .fetch_range(RangeRequest::whole(remote.final_url.clone()), &mut sink)
            .await;

        // I-1's ordering, in the reduced form S1 can express: the bytes are forced to stable
        // storage before the download is treated as complete. S2 adds the journal record that
        // makes this the commit point for a *range* rather than for the whole file.
        //
        // Done before the fetch result is inspected, and deliberately so: whatever arrived is
        // what a later resume will build on, so it has to survive a crash even when the fetch
        // failed. Returning early here would leave those bytes in the page cache only.
        sink.sync().await.map_err(|source| DownloadError::Sink {
            path: part_path.clone(),
            source,
        })?;

        let outcome = fetched?;

        // Verification, before the rename and with no fast path around it (I-4).
        let delivered = sink.written();
        if let Some(expected) = remote.total_length
            && delivered != expected
        {
            return Err(DownloadError::Incomplete {
                path: part_path,
                expected: Some(expected),
                actual: delivered,
            });
        }
        if outcome.truncated {
            return Err(DownloadError::Incomplete {
                path: part_path,
                expected: remote.total_length,
                actual: delivered,
            });
        }

        // Only now. Rename is atomic within a directory on every platform we target, so there is
        // no window in which the final name refers to an incomplete file.
        tokio::fs::rename(&part_path, &final_path)
            .await
            .map_err(|source| DownloadError::Io {
                path: final_path.clone(),
                source,
            })?;

        tracing::info!(
            path = %final_path.display(),
            bytes = delivered,
            protocol = %outcome.protocol,
            "download complete"
        );
        Ok(final_path)
    }
}

/// A `.dppart` file on disk.
///
/// Replaced in S2 by `downpour-storage`'s writer, which adds sparse preallocation and real
/// positional writes. The seek-then-write here is correct for one sequential stream and would
/// not be for many concurrent workers, which is exactly why S3 depends on that replacement.
struct FileTarget {
    file: tokio::fs::File,
}

#[async_trait]
impl SinkTarget for FileTarget {
    async fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), SinkError> {
        self.file
            .seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|source| classify_io(offset, source))?;
        self.file
            .write_all(bytes)
            .await
            .map_err(|source| classify_io(offset, source))?;
        Ok(())
    }

    async fn sync(&mut self) -> Result<(), SinkError> {
        // `sync_data` rather than `sync_all`: the file's data must be durable, but its metadata
        // timestamps need not be, and the difference is measurable on every write.
        self.file
            .sync_data()
            .await
            .map_err(|source| classify_io(0, source))
    }
}

/// Separate `ENOSPC` from other I/O failures.
///
/// Worth the platform constants because running out of disk at 97% is common and must pause the
/// download cleanly rather than truncate it (I-10). `std::io::ErrorKind::StorageFull` is still
/// unstable, so the raw code is matched instead.
fn classify_io(offset: u64, error: std::io::Error) -> SinkError {
    /// `ENOSPC` on Linux and the other Unixes we target.
    const ENOSPC: i32 = 28;
    /// `ERROR_HANDLE_DISK_FULL`.
    const WIN_HANDLE_DISK_FULL: i32 = 39;
    /// `ERROR_DISK_FULL`.
    const WIN_DISK_FULL: i32 = 112;

    match error.raw_os_error() {
        Some(ENOSPC) if cfg!(unix) => SinkError::NoSpace { offset },
        Some(WIN_HANDLE_DISK_FULL | WIN_DISK_FULL) if cfg!(windows) => {
            SinkError::NoSpace { offset }
        }
        _ => SinkError::Io {
            offset,
            source: error,
        },
    }
}

/// Why a download did not produce a verified file.
#[derive(Debug, Error)]
pub enum DownloadError {
    /// The probe rejected the URL or the response.
    #[error(transparent)]
    Probe(#[from] ProbeError),
    /// The transfer failed.
    #[error(transparent)]
    Transfer(#[from] TransferError),
    /// Fewer bytes arrived than the probe established. **The final name is not created** — this
    /// is the error whose absence produces a short file that looks complete (I-4).
    #[error("{path} holds {actual} bytes but {expected:?} were expected; not renamed")]
    Incomplete {
        /// The `.dppart` file, kept so a later resume can use what did arrive.
        path: PathBuf,
        /// What the probe established, when it established anything.
        expected: Option<u64>,
        /// What actually arrived.
        actual: u64,
    },
    /// Something already exists at the final path.
    #[error("{path} already exists; refusing to overwrite it")]
    TargetExists {
        /// The path that is occupied.
        path: PathBuf,
    },
    /// A write or a sync failed.
    #[error("storing {path} failed: {source}")]
    Sink {
        /// The file being written.
        path: PathBuf,
        /// The underlying sink error.
        #[source]
        source: SinkError,
    },
    /// A filesystem operation around the transfer failed.
    #[error("{path}: {source}")]
    Io {
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
}

impl DownloadError {
    /// The stable identifier clients and corpus cases switch on. **This is API.**
    ///
    /// Probe and transfer failures report the underlying kind rather than a wrapper, so a case
    /// asserting `unexpected_content_encoding` does not have to know at which layer the response
    /// was refused — only that it was.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Probe(error) => error.kind(),
            Self::Transfer(error) => error.kind(),
            Self::Incomplete { .. } => "incomplete",
            Self::TargetExists { .. } => "target_exists",
            Self::Sink { .. } => "sink",
            Self::Io { .. } => "io",
        }
    }
}
