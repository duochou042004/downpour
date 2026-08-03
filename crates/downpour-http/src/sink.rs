//! Where fetched bytes go.
//!
//! This module owns a structural precondition for **I-2** (two segments never write to the same
//! byte offset). A backend cannot choose an offset, because a [`RangeSink`] has no API for
//! choosing one: it is created at a base offset by whoever granted the range, and the backend can
//! only *append* to it. Offsets are therefore decided exactly once, by the allocator, and a
//! misbehaving or confused backend cannot write outside its grant even by accident.
//!
//! S1 has one target, a `.dppart` file. S2 replaces it with `downpour-storage`'s writer, which
//! adds preallocation, positional writes and the durability ordering that I-1 requires. The
//! [`SinkTarget`] boundary is what makes that a substitution rather than a rewrite.

use async_trait::async_trait;
use thiserror::Error;

/// Somewhere bytes can be put. Implemented by storage; never by a protocol backend.
#[async_trait]
pub trait SinkTarget: Send {
    /// Write `bytes` at `offset`.
    ///
    /// Positional rather than sequential because S3 has many workers writing one file
    /// concurrently, and a shared seek cursor would make them contend.
    async fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), SinkError>;

    /// Force everything written so far to stable storage.
    ///
    /// **The commit point for I-1.** A range is only ever recorded as complete after this
    /// returns, never before. Reversing that ordering is the classic corruption bug: the process
    /// dies between "wrote to page cache" and "fsync", the block map says complete, resume skips
    /// the range, and the file has a hole that the size and the checksum both fail to notice.
    async fn sync(&mut self) -> Result<(), SinkError>;
}

/// An append-only window onto a [`SinkTarget`], fixed at a base offset.
///
/// The type deliberately exposes no way to seek. That is the whole design: a backend receives a
/// sink, appends what it fetched, and physically cannot address any other part of the file.
pub struct RangeSink {
    target: Box<dyn SinkTarget>,
    base_offset: u64,
    written: u64,
    limit: Option<u64>,
}

impl RangeSink {
    /// A sink that appends to `target`, starting at `base_offset`.
    ///
    /// `limit`, when set, is the most this sink will accept. Bytes beyond it are refused rather
    /// than written: a server that sends more than its `Content-Range` claimed must not be able
    /// to overwrite the range next to ours.
    #[must_use]
    pub fn new(target: Box<dyn SinkTarget>, base_offset: u64, limit: Option<u64>) -> Self {
        Self {
            target,
            base_offset,
            written: 0,
            limit,
        }
    }

    /// Append `bytes`.
    ///
    /// Refuses, without writing anything, if the write would exceed `limit`. Partial acceptance
    /// is not offered: a caller that could write "some of" an over-long chunk would have to
    /// decide where to cut, and that decision belongs to the allocator.
    pub async fn accept(&mut self, bytes: &[u8]) -> Result<(), SinkError> {
        let len = u64::try_from(bytes.len()).map_err(|_| SinkError::LengthOverflow)?;
        if let Some(limit) = self.limit
            && self.written.saturating_add(len) > limit
        {
            return Err(SinkError::BeyondGrant {
                grant: limit,
                already_written: self.written,
                attempted: len,
            });
        }
        let offset = self
            .base_offset
            .checked_add(self.written)
            .ok_or(SinkError::LengthOverflow)?;
        self.target.write_at(offset, bytes).await?;
        self.written = self.written.saturating_add(len);
        Ok(())
    }

    /// Force everything written so far to stable storage (I-1).
    pub async fn sync(&mut self) -> Result<(), SinkError> {
        self.target.sync().await
    }

    /// How many bytes have been appended.
    #[must_use]
    pub fn written(&self) -> u64 {
        self.written
    }

    /// The offset the next byte will land at.
    #[must_use]
    pub fn next_offset(&self) -> u64 {
        self.base_offset.saturating_add(self.written)
    }

    /// The offset this sink started at.
    #[must_use]
    pub fn base_offset(&self) -> u64 {
        self.base_offset
    }
}

impl std::fmt::Debug for RangeSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `SinkTarget` is not Debug, and requiring it would constrain storage for no benefit.
        f.debug_struct("RangeSink")
            .field("base_offset", &self.base_offset)
            .field("written", &self.written)
            .field("limit", &self.limit)
            .finish_non_exhaustive()
    }
}

/// Why a write could not be accepted.
#[derive(Debug, Error)]
pub enum SinkError {
    /// The write would have gone past the granted range. Refusing is what keeps a server that
    /// over-delivers from corrupting the neighbouring range (I-2).
    #[error(
        "write of {attempted} bytes would exceed the {grant}-byte grant \
         ({already_written} already written)"
    )]
    BeyondGrant {
        /// How many bytes this sink was granted.
        grant: u64,
        /// How many have been written so far.
        already_written: u64,
        /// How many the caller tried to add.
        attempted: u64,
    },
    /// The target ran out of space. Handled explicitly rather than as a generic I/O error,
    /// because `ENOSPC` must pause the download cleanly instead of truncating it (I-10).
    #[error("no space left on device while writing at offset {offset}")]
    NoSpace {
        /// Where the write was attempted.
        offset: u64,
    },
    /// Underlying I/O failure, with the offset that failed so the report is actionable.
    #[error("write failed at offset {offset}: {source}")]
    Io {
        /// Where the write was attempted.
        offset: u64,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A length did not fit in a `u64`. Cannot happen on any real platform; handled rather than
    /// cast away, because an `as` cast here would silently truncate a file offset.
    #[error("a buffer length or offset does not fit in u64")]
    LengthOverflow,
}
