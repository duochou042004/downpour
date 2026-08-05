//! The one place fetched bytes become durable.
//!
//! This module owns **I-1** for the transfer path. It does not reimplement the durability
//! ordering — that lives in `downpour_storage::writer::DurableWriter`, where S2-T5 put it and
//! where the crash-boundary proofs point. What this module owns is that every byte the HTTP
//! layer accepts goes through that writer, and that no second way of putting bytes on disk
//! exists in the workspace. S1's minimal seek-then-write sink is gone; ADR-0016 records why the
//! replacement lives here rather than in `downpour-engine`, and when it leaves.
//!
//! The blocking writer is driven from async code by moving it into `spawn_blocking` and back
//! out on every call. Moving rather than sharing is deliberate: the writer is the single owner
//! of the file handle and the interval map, and a type that is physically moved cannot be
//! observed by two tasks at once. `docs/04-storage-and-recovery-spec.md` §2.2's bounded-channel
//! writer task arrives in S3 with the worker pool that makes it worth its complexity.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use downpour_intervals::{IntervalMap, WorkerId};
use downpour_storage::journal::FileHeader;
use downpour_storage::part_file::{PartFile, PartFileError};
use downpour_storage::writer::{DurableWriter, JournalFile, WriterError};

use crate::sink::{SinkError, SinkTarget};

/// The single worker identity a single-stream transfer uses.
///
/// S3 replaces this with real grants from the segment allocator. Until then there is exactly one
/// worker, and it holds the whole representation.
const SINGLE_STREAM_WORKER: WorkerId = WorkerId::new(1);

/// The durable artifacts one download owns.
#[derive(Clone, Debug)]
pub struct Artifacts {
    /// The `.dppart` receiving bytes.
    pub part_path: PathBuf,
    /// The recovery journal, absent when the representation length is unknown.
    pub journal_path: Option<PathBuf>,
}

/// Either the full storage stack, or the reduced form a lengthless representation allows.
///
/// One enum rather than two sink types, so the decision is made once. A change to the ordering
/// cannot reach one arm and miss the other, which is the failure mode B-5 exists to prevent.
enum Backing {
    /// The server stated a length: preallocated extent, journal, interval map, I-1's ordering.
    Journalled {
        writer: DurableWriter<PartFile, JournalFile>,
        intervals: IntervalMap,
    },
    /// The server stated no length and proved no ranges, so there is nothing to preallocate,
    /// nothing to bind a journal header to, and nothing a replay could ever be used for.
    Lengthless { part: PartFile },
}

impl Backing {
    fn write(&mut self, offset: u64, bytes: &[u8], now: Duration) -> Result<(), SinkError> {
        match self {
            Self::Journalled { writer, intervals } => writer
                .stage(intervals, SINGLE_STREAM_WORKER, offset, bytes, now)
                .map(|_| ())
                .map_err(|error| writer_error(offset, error)),
            Self::Lengthless { part } => {
                let length = u64::try_from(bytes.len()).map_err(|_| SinkError::LengthOverflow)?;
                let end = offset
                    .checked_add(length)
                    .ok_or(SinkError::LengthOverflow)?;
                part.extend_to(end)
                    .map_err(|error| part_error(offset, error))?;
                part.write_all_at(offset, bytes)
                    .map_err(|error| part_error(offset, error))
            }
        }
    }

    fn sync(&mut self) -> Result<(), SinkError> {
        match self {
            // The commit point. Data sync, journal append, journal sync, then the interval map
            // moves to Complete — in that order, inside the writer.
            Self::Journalled { writer, intervals } => writer
                .flush(intervals)
                .map(|_| ())
                .map_err(|error| writer_error(0, error)),
            Self::Lengthless { part } => part.sync_data().map_err(|error| part_error(0, error)),
        }
    }
}

/// A [`SinkTarget`] backed by `downpour-storage`.
pub struct StorageSink {
    /// `None` only while a blocking call owns it.
    backing: Option<Backing>,
    started: Instant,
    artifacts: Artifacts,
}

impl StorageSink {
    /// Create the durable artifacts for one download.
    ///
    /// `target` is the *final* path; the part file is `<target>.dppart` beside it, so the
    /// eventual rename is same-filesystem and therefore atomic. The journal is named from a
    /// digest of the final URL, which makes it stable across the retries of one `download` call
    /// without needing an id allocator that does not exist until the daemon does.
    ///
    /// Creation is exclusive on both files. A collision means another owner holds this download,
    /// and proceeding would interleave two writers into one file.
    pub fn create(
        target: &Path,
        journal_dir: &Path,
        total_length: Option<u64>,
        transfer_id: [u8; 16],
        validator_hash: [u8; 32],
    ) -> Result<Self, SinkError> {
        let (backing, artifacts) = match total_length {
            Some(total_length) => {
                let part =
                    PartFile::create(target, total_length).map_err(|error| part_error(0, error))?;
                let part_path = part.path().to_path_buf();
                // Block size 0: a single stream has no fixed granularity, since a block is
                // whatever the transport happened to deliver. Nothing reads this field — replay
                // and compaction both work from the records — so recording a made-up value would
                // be worse than recording the absence of one.
                let header = FileHeader::new(transfer_id, total_length, 0, validator_hash);
                let journal =
                    JournalFile::create(journal_path_for(journal_dir, transfer_id), header)
                        .map_err(|error| writer_error(0, error))?;
                let journal_path = journal.path().to_path_buf();
                let writer = DurableWriter::try_new(part, journal, 0)
                    .map_err(|error| writer_error(0, error))?;

                let mut intervals = IntervalMap::new(total_length);
                // The whole representation, to the one worker a single stream has. A zero-length
                // object has no range to grant, and granting an empty one is rejected.
                if total_length > 0 {
                    intervals
                        .grant(0..total_length, SINGLE_STREAM_WORKER)
                        .map_err(|error| SinkError::Io {
                            offset: 0,
                            source: std::io::Error::other(error.to_string()),
                        })?;
                }
                (
                    Backing::Journalled { writer, intervals },
                    Artifacts {
                        part_path,
                        journal_path: Some(journal_path),
                    },
                )
            }
            None => {
                let part =
                    PartFile::create_growable(target).map_err(|error| part_error(0, error))?;
                let part_path = part.path().to_path_buf();
                (
                    Backing::Lengthless { part },
                    Artifacts {
                        part_path,
                        journal_path: None,
                    },
                )
            }
        };

        Ok(Self {
            backing: Some(backing),
            started: Instant::now(),
            artifacts,
        })
    }

    /// The durable artifacts this sink created.
    #[must_use]
    pub fn artifacts(&self) -> &Artifacts {
        &self.artifacts
    }

    /// Run one blocking operation on the writer, moving it out and back.
    async fn on_writer<F>(&mut self, operation: F) -> Result<(), SinkError>
    where
        F: FnOnce(&mut Backing) -> Result<(), SinkError> + Send + 'static,
    {
        let mut backing = self.backing.take().ok_or(SinkError::Io {
            offset: 0,
            source: std::io::Error::other("storage sink was left without its writer"),
        })?;
        let (backing, result) = tokio::task::spawn_blocking(move || {
            let result = operation(&mut backing);
            (backing, result)
        })
        .await
        .map_err(|error| SinkError::Io {
            offset: 0,
            source: std::io::Error::other(error.to_string()),
        })?;
        self.backing = Some(backing);
        result
    }
}

#[async_trait]
impl SinkTarget for StorageSink {
    async fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), SinkError> {
        // Copied because the write crosses onto a blocking thread and the borrow cannot. One
        // allocation per chunk; ADR-0016 records it as accepted and names what replaces it.
        let owned = bytes.to_vec();
        let now = self.started.elapsed();
        self.on_writer(move |backing| backing.write(offset, &owned, now))
            .await
    }

    async fn sync(&mut self) -> Result<(), SinkError> {
        self.on_writer(Backing::sync).await
    }
}

fn journal_path_for(journal_dir: &Path, transfer_id: [u8; 16]) -> PathBuf {
    let mut name = String::with_capacity(36);
    for byte in transfer_id {
        name.push_str(&format!("{byte:02x}"));
    }
    name.push_str(".dpj");
    journal_dir.join(name)
}

/// Map a writer failure onto the sink's vocabulary, keeping `ENOSPC` distinguishable.
///
/// `NoSpace` is a separate variant rather than a generic I/O error because running out of disk
/// must pause the download cleanly instead of truncating it (I-10), and S2-T13 dispatches on it.
fn writer_error(offset: u64, error: WriterError) -> SinkError {
    match error {
        WriterError::PartFile(error) => part_error(offset, error),
        WriterError::Io { source, .. } => classify_io(offset, source),
        other => SinkError::Io {
            offset,
            source: std::io::Error::other(other.to_string()),
        },
    }
}

fn part_error(offset: u64, error: PartFileError) -> SinkError {
    match error {
        PartFileError::Io { source, .. } => classify_io(offset, source),
        PartFileError::OutOfBounds {
            offset: start,
            length,
            total_length,
        } => SinkError::BeyondGrant {
            grant: total_length,
            already_written: start,
            attempted: length,
        },
        other => SinkError::Io {
            offset,
            source: std::io::Error::other(other.to_string()),
        },
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
