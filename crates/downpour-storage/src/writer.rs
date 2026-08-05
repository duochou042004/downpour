//! Bounded durable block commits in I-1's fixed order.
//!
//! The supervisor continues to own the canonical interval map. This writer owns the data and
//! journal handles, validates every submitted range against the current allocator grant, and
//! swaps a prevalidated next map into place only after both files are durable.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Duration;

use downpour_intervals::{IntervalMap, IntervalMapError, WorkerId};
use thiserror::Error;

use crate::journal::{FileHeader, FormatError, FramedRecord, JournalRecord};
use crate::part_file::{PartFile, PartFileError};

/// Maximum age of a non-empty in-memory completion batch.
pub const JOURNAL_FLUSH_INTERVAL: Duration = Duration::from_secs(2);

/// Completed data bytes that trigger a durability flush, even before the time limit.
pub const JOURNAL_FLUSH_BYTES: u64 = 8 * 1024 * 1024;

/// Positional data storage used by the durable writer.
///
/// The trait is intentionally narrow so deterministic tests and the later simulation harness
/// can inject failures at the same boundaries as the real [`PartFile`].
pub trait DurableData: Send {
    /// Exact representation length accepted by this data file.
    fn total_length(&self) -> u64;

    /// Write the complete buffer at `offset` without a shared seek cursor.
    fn write_all_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), WriterError>;

    /// Force all preceding data writes to stable storage.
    fn sync_data(&mut self) -> Result<(), WriterError>;
}

impl DurableData for PartFile {
    fn total_length(&self) -> u64 {
        PartFile::total_length(self)
    }

    fn write_all_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), WriterError> {
        PartFile::write_all_at(self, offset, bytes).map_err(WriterError::from)
    }

    fn sync_data(&mut self) -> Result<(), WriterError> {
        PartFile::sync_data(self).map_err(WriterError::from)
    }
}

/// Append-only journal storage used by the durable writer.
pub trait DurableJournal: Send {
    /// Representation length bound into the journal header.
    fn total_length(&self) -> u64;

    /// Append one checksummed frame without rewriting existing bytes.
    fn append(&mut self, record: &FramedRecord) -> Result<(), WriterError>;

    /// Force the header and all appended frames to stable storage.
    fn sync_data(&mut self) -> Result<(), WriterError>;
}

/// An exclusively created append-only version-1 recovery journal.
#[derive(Debug)]
pub struct JournalFile {
    file: File,
    path: PathBuf,
    total_length: u64,
}

impl JournalFile {
    /// Create a new journal, write its versioned header, and durably establish it.
    ///
    /// An existing path is an ownership collision. The file is opened with append semantics so
    /// this type has no operation capable of rewriting an earlier record in place.
    pub fn create(path: impl AsRef<Path>, header: FileHeader) -> Result<Self, WriterError> {
        let path = path.as_ref().to_path_buf();
        let mut file = match OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                return Err(WriterError::JournalAlreadyExists { path });
            }
            Err(source) => {
                return Err(WriterError::Io {
                    operation: "create recovery journal exclusively",
                    source,
                });
            }
        };
        file.write_all(&header.encode())
            .map_err(|source| WriterError::Io {
                operation: "write recovery-journal header",
                source,
            })?;
        file.sync_all().map_err(|source| WriterError::Io {
            operation: "synchronise recovery-journal header",
            source,
        })?;
        sync_parent(&path).map_err(|source| WriterError::Io {
            operation: "synchronise recovery-journal directory",
            source,
        })?;

        Ok(Self {
            file,
            path,
            total_length: header.total_length(),
        })
    }

    /// Path of the exclusively owned journal.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl DurableJournal for JournalFile {
    fn total_length(&self) -> u64 {
        self.total_length
    }

    fn append(&mut self, record: &FramedRecord) -> Result<(), WriterError> {
        let encoded = record.encode()?;
        self.file
            .write_all(&encoded)
            .map_err(|source| WriterError::Io {
                operation: "append recovery-journal record",
                source,
            })
    }

    fn sync_data(&mut self) -> Result<(), WriterError> {
        self.file.sync_data().map_err(|source| WriterError::Io {
            operation: "synchronise recovery-journal records",
            source,
        })
    }
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "recovery-journal path has no parent directory",
        )
    })?;
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[derive(Clone, Debug)]
struct StagedBlock {
    range: Range<u64>,
    record_length: u32,
    worker: WorkerId,
    blake3: [u8; 32],
}

/// A block that crossed the journal sync boundary and became observable as complete.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableBlock {
    sequence: u64,
    range: Range<u64>,
    worker: WorkerId,
    blake3: [u8; 32],
}

impl DurableBlock {
    /// Journal sequence assigned to this completed block.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Exact half-open byte range that is now durable.
    #[must_use]
    pub fn range(&self) -> &Range<u64> {
        &self.range
    }

    /// Worker whose allocator grant authorized the write.
    #[must_use]
    pub const fn worker(&self) -> WorkerId {
        self.worker
    }

    /// BLAKE3 digest persisted in the `BlockComplete` record.
    #[must_use]
    pub const fn blake3(&self) -> &[u8; 32] {
        &self.blake3
    }
}

/// Per-download writer that batches block commits without weakening their order.
#[derive(Debug)]
pub struct DurableWriter<D, J> {
    data: D,
    journal: J,
    next_sequence: u64,
    staged: Vec<StagedBlock>,
    staged_bytes: u64,
    first_staged_at: Option<Duration>,
    poisoned: bool,
    /// Offset at which the volume refused a write, once that has happened.
    disk_full_at: Option<u64>,
    /// Bytes whose journal record crossed the commit point.
    durable_bytes: u64,
}

impl<D: DurableData, J: DurableJournal> DurableWriter<D, J> {
    /// Bind one part file and one journal that describe the same representation.
    pub fn try_new(data: D, journal: J, next_sequence: u64) -> Result<Self, WriterError> {
        let data_length = data.total_length();
        let journal_length = journal.total_length();
        if data_length != journal_length {
            return Err(WriterError::LengthMismatch {
                data_length,
                journal_length,
            });
        }
        Ok(Self {
            data,
            journal,
            next_sequence,
            staged: Vec::new(),
            staged_bytes: 0,
            first_staged_at: None,
            poisoned: false,
            disk_full_at: None,
            durable_bytes: 0,
        })
    }

    /// Positionally write and stage one non-empty block owned by `worker`.
    ///
    /// A byte-bound flush happens before this call returns when the batch reaches 8 MiB.
    /// Otherwise the block remains `InProgress` until [`Self::flush_if_due`] or [`Self::flush`]
    /// crosses the two durability sync boundaries.
    pub fn stage(
        &mut self,
        intervals: &mut IntervalMap,
        worker: WorkerId,
        offset: u64,
        bytes: &[u8],
        now: Duration,
    ) -> Result<Vec<DurableBlock>, WriterError> {
        self.ensure_usable()?;
        self.ensure_interval_identity(intervals)?;
        if bytes.is_empty() {
            return Err(WriterError::EmptyBlock);
        }
        let length = u64::try_from(bytes.len())
            .map_err(|_| WriterError::BlockTooLarge { length: u64::MAX })?;
        let record_length =
            u32::try_from(length).map_err(|_| WriterError::BlockTooLarge { length })?;
        let end = offset
            .checked_add(length)
            .ok_or(WriterError::RangeOverflow { offset, length })?;
        let range = offset..end;
        self.ensure_current_grant(intervals, range.clone(), worker)?;
        self.ensure_not_staged(&range)?;
        let next_staged_bytes = self
            .staged_bytes
            .checked_add(length)
            .ok_or(WriterError::BatchLengthOverflow)?;
        let digest = *blake3::hash(bytes).as_bytes();

        if let Err(error) = self.data.write_all_at(offset, bytes) {
            if let Some(offset) = no_space_offset(&error, offset) {
                // docs/04 §2.4. The write is refused, and the part file is emphatically NOT
                // shortened to make room: the extent we hold is where the bytes we would resume
                // from live, and the user's other data is not ours to sacrifice either.
                //
                // Everything already staged had its bytes written before this failure, so the
                // journal — which is small, and for which there is almost always room — is
                // flushed to record it. Skipping that is not corruption, but it discards work
                // the user already paid for, and after a pause is exactly when that hurts.
                if let Err(flush_error) = self.flush(intervals) {
                    tracing::warn!(
                        %flush_error,
                        "could not record staged progress after the volume filled"
                    );
                }
                self.disk_full_at = Some(offset);
                return Err(WriterError::NoSpace { offset });
            }
            self.poisoned = true;
            return Err(error);
        }
        self.staged.push(StagedBlock {
            range,
            record_length,
            worker,
            blake3: digest,
        });
        self.staged_bytes = next_staged_bytes;
        self.first_staged_at.get_or_insert(now);

        if self.staged_bytes >= JOURNAL_FLUSH_BYTES {
            self.flush(intervals)
        } else {
            Ok(Vec::new())
        }
    }

    /// Flush a non-empty batch once it reaches the normative two-second age bound.
    pub fn flush_if_due(
        &mut self,
        intervals: &mut IntervalMap,
        now: Duration,
    ) -> Result<Vec<DurableBlock>, WriterError> {
        self.ensure_usable()?;
        let due = self.first_staged_at.is_some_and(|started| {
            now.checked_sub(started)
                .is_some_and(|elapsed| elapsed >= JOURNAL_FLUSH_INTERVAL)
        });
        if due {
            self.flush(intervals)
        } else {
            Ok(Vec::new())
        }
    }

    /// Commit every staged block in write → data sync → journal append → journal sync → Complete
    /// order.
    ///
    /// The complete next interval map is validated before either sync. Any failure poisons this
    /// writer: its on-disk state must be replayed before more work is accepted, because an I/O
    /// error may have occurred after the operating system performed the requested side effect.
    pub fn flush(&mut self, intervals: &mut IntervalMap) -> Result<Vec<DurableBlock>, WriterError> {
        self.ensure_usable()?;
        self.ensure_interval_identity(intervals)?;
        if self.staged.is_empty() {
            return Ok(Vec::new());
        }

        let mut next_intervals = intervals.clone();
        for block in &self.staged {
            if let Err(error) = next_intervals.complete(block.range.clone(), block.worker) {
                self.poisoned = true;
                return Err(WriterError::Interval(error));
            }
        }

        let record_count =
            u64::try_from(self.staged.len()).map_err(|_| WriterError::SequenceExhausted)?;
        let next_sequence = self
            .next_sequence
            .checked_add(record_count)
            .ok_or(WriterError::SequenceExhausted)?;
        let mut completed = Vec::with_capacity(self.staged.len());
        let mut records = Vec::with_capacity(self.staged.len());
        for (index, block) in self.staged.iter().enumerate() {
            let index = u64::try_from(index).map_err(|_| WriterError::SequenceExhausted)?;
            let sequence = self
                .next_sequence
                .checked_add(index)
                .ok_or(WriterError::SequenceExhausted)?;
            records.push(FramedRecord::new(
                sequence,
                JournalRecord::BlockComplete {
                    offset: block.range.start,
                    len: block.record_length,
                    blake3: block.blake3,
                },
            ));
            completed.push(DurableBlock {
                sequence,
                range: block.range.clone(),
                worker: block.worker,
                blake3: block.blake3,
            });
        }

        if let Err(error) = self.data.sync_data() {
            self.poisoned = true;
            return Err(error);
        }
        for record in &records {
            if let Err(error) = self.journal.append(record) {
                self.poisoned = true;
                return Err(error);
            }
        }
        if let Err(error) = self.journal.sync_data() {
            self.poisoned = true;
            return Err(error);
        }

        *intervals = next_intervals;
        self.next_sequence = next_sequence;
        for block in &completed {
            self.durable_bytes = self
                .durable_bytes
                .saturating_add(block.range.end - block.range.start);
        }
        self.staged.clear();
        self.staged_bytes = 0;
        self.first_staged_at = None;
        Ok(completed)
    }

    /// Bytes recorded durable by the time the volume filled.
    ///
    /// Meaningful only after [`WriterError::NoSpace`]. Reported so a caller can pause the
    /// download at an exact, resumable point rather than guessing at one.
    #[must_use]
    pub fn durable_bytes_after_disk_full(&self) -> u64 {
        self.durable_bytes
    }

    /// Whether the volume refused a write, and where.
    #[must_use]
    pub const fn disk_full_at(&self) -> Option<u64> {
        self.disk_full_at
    }

    /// The sequence the next journal append must carry.
    ///
    /// Exposed so the completion sequence can append its `Sealed` record contiguously; the
    /// writer stays the only thing that assigns sequences during a transfer.
    #[must_use]
    pub const fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    /// Borrow the journal so the completion sequence can seal it.
    ///
    /// Only safe once every staged block has been flushed: the writer's own invariant is that
    /// nothing appends between its records, and sealing a journal with work still staged would
    /// place the seal before the blocks it claims to cover.
    pub const fn journal_mut(&mut self) -> &mut J {
        &mut self.journal
    }

    /// Whether any block is staged but not yet durable.
    #[must_use]
    pub fn has_staged_work(&self) -> bool {
        !self.staged.is_empty()
    }

    fn ensure_usable(&self) -> Result<(), WriterError> {
        if self.poisoned {
            Err(WriterError::Poisoned)
        } else {
            Ok(())
        }
    }

    fn ensure_interval_identity(&self, intervals: &IntervalMap) -> Result<(), WriterError> {
        let interval_length = intervals.total_length();
        let data_length = self.data.total_length();
        if interval_length == data_length {
            Ok(())
        } else {
            Err(WriterError::IntervalLengthMismatch {
                data_length,
                interval_length,
            })
        }
    }

    fn ensure_current_grant(
        &self,
        intervals: &IntervalMap,
        range: Range<u64>,
        worker: WorkerId,
    ) -> Result<(), WriterError> {
        let mut candidate = intervals.clone();
        if let Err(error) = candidate.complete(range, worker) {
            debug_assert!(
                false,
                "durable writer rejected an out-of-grant write: {error}"
            );
            return Err(WriterError::Interval(error));
        }
        Ok(())
    }

    fn ensure_not_staged(&self, range: &Range<u64>) -> Result<(), WriterError> {
        if let Some(existing) = self
            .staged
            .iter()
            .find(|block| block.range.start < range.end && range.start < block.range.end)
        {
            return Err(WriterError::OverlappingStaged {
                existing_start: existing.range.start,
                existing_end: existing.range.end,
                attempted_start: range.start,
                attempted_end: range.end,
            });
        }
        Ok(())
    }
}

/// Why a block could not safely cross the durable-writer commit point.
#[derive(Debug, Error)]
pub enum WriterError {
    /// Part-file creation, bounds, or I/O failed.
    #[error("part-file operation failed: {0}")]
    PartFile(#[from] PartFileError),
    /// A journal frame could not be encoded in version 1.
    #[error("recovery-journal record could not be encoded: {0}")]
    Format(#[from] FormatError),
    /// The allocator did not authorize a submitted or staged range.
    #[error("allocator rejected durable-writer range: {0}")]
    Interval(#[from] IntervalMapError),
    /// A filesystem or injected durability operation failed.
    #[error("could not {operation}: {source}")]
    Io {
        /// Operation whose result could not be ignored.
        operation: &'static str,
        /// Underlying I/O failure.
        #[source]
        source: io::Error,
    },
    /// The journal path already exists and therefore belongs to another owner or recovery.
    #[error("recovery journal already exists: {path}", path = .path.display())]
    JournalAlreadyExists {
        /// Colliding journal path.
        path: PathBuf,
    },
    /// The part file and journal header describe different representations.
    #[error("part-file length {data_length} does not match journal length {journal_length}")]
    LengthMismatch {
        /// Length accepted by the part file.
        data_length: u64,
        /// Length bound into the journal header.
        journal_length: u64,
    },
    /// The supplied interval map covers a different representation than the bound storage.
    #[error("part-file length {data_length} does not match interval-map length {interval_length}")]
    IntervalLengthMismatch {
        /// Length accepted by the part file.
        data_length: u64,
        /// Length represented by the allocator.
        interval_length: u64,
    },
    /// A zero-length completion cannot create journal evidence.
    #[error("durable blocks must contain at least one byte")]
    EmptyBlock,
    /// A block cannot fit the journal version-1 `u32` length field.
    #[error("durable block length {length} exceeds the journal version-1 limit")]
    BlockTooLarge {
        /// Rejected byte length.
        length: u64,
    },
    /// Offset plus length exceeded the `u64` file domain.
    #[error("durable block [{offset}, +{length}) overflows the file offset domain")]
    RangeOverflow {
        /// Submitted starting offset.
        offset: u64,
        /// Submitted byte length.
        length: u64,
    },
    /// A second staged write would touch bytes already staged in the same batch.
    #[error(
        "staged write [{attempted_start}, {attempted_end}) overlaps existing staged range \
         [{existing_start}, {existing_end})"
    )]
    OverlappingStaged {
        /// Existing staged range start.
        existing_start: u64,
        /// Existing staged range end.
        existing_end: u64,
        /// Rejected range start.
        attempted_start: u64,
        /// Rejected range end.
        attempted_end: u64,
    },
    /// The in-memory completed-byte count overflowed.
    #[error("durable batch byte count overflowed u64")]
    BatchLengthOverflow,
    /// No sequential journal number remains for the complete batch.
    #[error("recovery-journal sequence space is exhausted")]
    SequenceExhausted,
    /// The volume is full. Its own variant, not a generic I/O failure, because I-10 requires a
    /// clean resumable pause rather than a failed download — and the part file is never
    /// truncated to make room, whatever that costs (docs/04 §2.4).
    #[error("no space left on the volume while writing at offset {offset}")]
    NoSpace {
        /// Where the write was refused.
        offset: u64,
    },
    /// A preceding failure may have partially changed disk state; replay is required first.
    #[error("durable writer is poisoned and must be reconstructed from journal replay")]
    Poisoned,
}

/// Whether a writer failure is the volume filling up, and at which offset.
///
/// `std::io::ErrorKind::StorageFull` is still unstable, so the raw code is matched. Worth the
/// platform constants because I-10 turns a full disk into a clean resumable pause rather than a
/// failed download, and that dispatch needs the distinction.
fn no_space_offset(error: &WriterError, offset: u64) -> Option<u64> {
    /// `ENOSPC` on Linux and the other Unixes we target.
    const ENOSPC: i32 = 28;
    /// `ERROR_HANDLE_DISK_FULL`.
    const WIN_HANDLE_DISK_FULL: i32 = 39;
    /// `ERROR_DISK_FULL`.
    const WIN_DISK_FULL: i32 = 112;

    let raw = match error {
        WriterError::PartFile(PartFileError::Io { source, .. })
        | WriterError::Io { source, .. } => source.raw_os_error(),
        WriterError::NoSpace { offset } => return Some(*offset),
        _ => None,
    }?;
    let full = (cfg!(unix) && raw == ENOSPC)
        || (cfg!(windows) && (raw == WIN_HANDLE_DISK_FULL || raw == WIN_DISK_FULL));
    full.then_some(offset)
}
