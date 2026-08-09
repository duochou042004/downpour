//! The engine's concrete bridge from fetched bytes to durable storage.
//!
//! This module owns **I-1** for the transfer path. It does not reimplement the durability
//! ordering — that lives in `downpour_storage::writer::DurableWriter`, where S2-T5 put it and
//! where the crash-boundary proofs point. What this module owns is that every byte the HTTP
//! layer accepts goes through that writer, and that no second way of putting bytes on disk
//! exists in the workspace. S1's minimal seek-then-write sink is gone; this module's move into
//! `downpour-engine` completes ADR-0016's expected S3 reversal.
//!
//! The blocking writer is driven from async code by moving it into `spawn_blocking` and back
//! out on every call. Moving rather than sharing is deliberate: the writer is the single owner
//! of the file handle and the interval map, and a type that is physically moved cannot be
//! observed by two tasks at once. `docs/04-storage-and-recovery-spec.md` §2.2's bounded-channel
//! writer task arrives in S3 with the worker pool that makes it worth its complexity.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use downpour_intervals::{Interval, IntervalMap, IntervalState, WorkerId};
use downpour_storage::completion::{CompletionError, Sealed, verify_and_rename};
use downpour_storage::journal::FileHeader;
use downpour_storage::part_file::{PartFile, PartFileError};
use downpour_storage::writer::{DurableWriter, JournalFile, WriterError};
use downpour_types::ContentDigest;

use downpour_http::{SinkError, SinkTarget};

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
    /// Run docs/04 §6 against this download's own artifacts.
    ///
    /// A lengthless representation cannot be verified this way: with no stated length there is
    /// no length to check, no interval map to prove gap-free, and no journal to seal. Such a
    /// download is renamed on the strength of the transfer having ended, which is the honest
    /// limit of what can be known about it — and is why it is also non-resumable.
    fn complete(
        &mut self,
        part_path: &Path,
        final_path: &Path,
        digest: Option<&ContentDigest>,
    ) -> Result<Option<Sealed>, SinkError> {
        match self {
            Self::Journalled { writer, intervals } => {
                if writer.has_staged_work() {
                    return Err(SinkError::Io {
                        offset: 0,
                        source: std::io::Error::other(
                            "cannot seal a journal with work still staged; flush first",
                        ),
                    });
                }
                let total = intervals.total_length();
                let next_sequence = writer.next_sequence();
                verify_and_rename(
                    part_path,
                    final_path,
                    total,
                    intervals,
                    digest,
                    writer.journal_mut(),
                    next_sequence,
                )
                .map(Some)
                .map_err(completion_error)
            }
            Self::Lengthless { .. } => {
                std::fs::rename(part_path, final_path)
                    .map_err(|source| SinkError::Io { offset: 0, source })?;
                Ok(None)
            }
        }
    }

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
    /// The server's RFC 9530 evidence, checked before the rename (I-4).
    digest: Option<ContentDigest>,
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
        digest: Option<ContentDigest>,
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
                // The part file was just created exclusively, which proves no other owner holds
                // this target. A journal still sitting at this download's path is therefore
                // orphaned — it has no part file to protect, and nothing can be resumed from it —
                // so it is replaced rather than allowed to block.
                //
                // Without this, one failed download poisons its URL forever: the journal it
                // correctly kept as evidence collides with the next attempt's exclusive create.
                // That is B-30, and it is the same user-facing fault as B-29 reached from the
                // other side — there a SUCCESSFUL download's journal blocked the next one.
                let journal_path = journal_path_for(journal_dir, transfer_id);
                let journal = match JournalFile::create(&journal_path, header.clone()) {
                    Ok(journal) => journal,
                    Err(WriterError::JournalAlreadyExists { .. }) => {
                        tracing::info!(
                            path = %journal_path.display(),
                            "replacing an orphaned recovery journal: its part file is gone"
                        );
                        std::fs::remove_file(&journal_path)
                            .map_err(|source| SinkError::Io { offset: 0, source })?;
                        JournalFile::create(&journal_path, header)
                            .map_err(|error| writer_error(0, error))?
                    }
                    Err(error) => return Err(writer_error(0, error)),
                };
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
            digest,
        })
    }

    /// The durable artifacts this sink created.
    #[must_use]
    pub fn artifacts(&self) -> &Artifacts {
        &self.artifacts
    }

    /// Run one blocking operation on the writer, moving it out and back.
    /// Verify this download, give it its final name, and retire its journal (docs/04 §6).
    ///
    /// Step 10's deletion is what makes the exclusive create an ownership check rather than a
    /// one-shot latch: a journal that outlives its verified download blocks the next download of
    /// the same URL forever, and leaks a file per download besides. It happens only after
    /// verification passed and the rename succeeded — deleting it on the strength of a byte
    /// counter is exactly what I-4 forbids, which is why this could not land before S2-T11.
    pub async fn complete(&mut self, final_path: PathBuf) -> Result<Option<Sealed>, SinkError> {
        let part_path = self.artifacts.part_path.clone();
        let digest = self.digest.clone();
        let mut backing = self.backing.take().ok_or(SinkError::Io {
            offset: 0,
            source: std::io::Error::other("storage sink was left without its writer"),
        })?;
        let (backing, result) = tokio::task::spawn_blocking(move || {
            let result = backing.complete(&part_path, &final_path, digest.as_ref());
            (backing, result)
        })
        .await
        .map_err(|error| SinkError::Io {
            offset: 0,
            source: std::io::Error::other(error.to_string()),
        })?;
        self.backing = Some(backing);
        let sealed = result?;

        // Only now, and never on any failure path: on a digest mismatch or a short file the
        // journal is evidence the user may want to retry from (docs/04 §6).
        if let Some(journal) = self.artifacts.journal_path.take()
            && let Err(error) = tokio::fs::remove_file(&journal).await
        {
            // The download is verified and named; a journal we could not remove is litter and a
            // future collision, not a reason to fail a download that succeeded.
            tracing::warn!(
                path = %journal.display(),
                %error,
                "could not remove the recovery journal of a verified download"
            );
        }
        Ok(sealed)
    }

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
    async fn verify_and_rename(&mut self, final_path: PathBuf) -> Result<(), SinkError> {
        self.complete(final_path).await.map(|_| ())
    }

    /// Read from the interval map, which only reaches `Complete` past the writer's commit
    /// point — so this can never name a byte that is merely staged. A representation with no
    /// stated length has no map and reports zero: it cannot be resumed anyway, since we only
    /// got there because ranges were never proven.
    fn durable_prefix_end(&self) -> u64 {
        match &self.backing {
            Some(Backing::Journalled { intervals, .. }) => intervals
                .intervals()
                .iter()
                .find(|interval| *interval.state() != IntervalState::Complete)
                .map_or_else(|| intervals.total_length(), Interval::start),
            _ => 0,
        }
    }

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

/// Map a completion failure onto the sink's vocabulary.
///
/// Every variant is a refusal to name the file, so they all become I/O errors carrying the
/// reason. The distinction the caller needs is preserved in the message, and `DownloadError`
/// gives the whole thing a stable kind.
fn completion_error(error: CompletionError) -> SinkError {
    SinkError::Io {
        offset: 0,
        source: std::io::Error::other(error.to_string()),
    }
}
