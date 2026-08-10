//! Reconciling one download's durable artifacts after an unclean shutdown.
//!
//! This module owns docs/04 §5 step 2 for a single download: part file, recovery journal,
//! interval coverage, and the SQLite record, brought back into agreement. Iterating downloads
//! at daemon start and emitting the recovery summary belong to S2-T14.
//!
//! Two rules decide every question here.
//!
//! **The journal is the only authority (docs/04 §4.1).** SQLite's checkpoint is a disposable
//! cache; it can be stale, it can be ahead, it can be unreadable. None of that may change what
//! is marked `Complete`, because a byte marked complete is a byte resume will never fetch
//! again. Trusting a checkpoint that ran ahead of the journal produces a file of exactly the
//! right size with a hole in the middle, and every integrity check that does not hash the
//! content passes. That is the corruption I-1 exists to prevent, and it is invisible until the
//! user opens the file.
//!
//! **Nothing is resumed and nothing stays granted.** A rebuilt map contains only `Complete` and
//! `Pending`; no `InProgress` interval survives a restart, because the worker that held it is
//! gone and its unflushed bytes were never durable. The download is left `Paused` — the user,
//! not the daemon, decides whether ten transfers should restart on boot.

use std::io;
use std::ops::Range;
use std::path::Path;

use downpour_intervals::{IntervalMap, IntervalMapError, WorkerId};
use thiserror::Error;

use crate::journal::state::{CompletedBlock, effective_state};
use crate::journal::{ReplayError, ReplayStop, recover_journal};
use crate::metadata::{
    Checkpoint, CompleteInterval, DownloadErrorKind, DownloadId, DownloadState, MetadataError,
    MetadataStore,
};
use crate::part_file::{PartFile, PartFileError};
use downpour_types::RangeSupport;

/// The allocator identity recovery uses to move replayed ranges into `Complete`.
///
/// The interval map has no "already complete" constructor by design: completion is only
/// reachable through a grant, so the type system keeps I-2 true. Recovery therefore grants and
/// completes each replayed range in turn. The identity never escapes this module — by the time
/// reconciliation returns, no interval is `InProgress`.
const RECOVERY_WORKER: WorkerId = WorkerId::new(u64::MAX);

/// How the durable journal and the disposable SQLite checkpoint compared.
///
/// Only the journal ever wins. This is reported so a daemon can log the disagreement, which is
/// the earliest visible symptom of a database rolled back behind the journal's back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckpointDivergence {
    /// No checkpoint row existed for this download.
    NoCheckpoint,
    /// The cache row could not be decoded or failed validation, and was ignored.
    CheckpointUnreadable,
    /// The cache matched the journal exactly.
    Agreed,
    /// The normal case: the checkpoint lags because it is written less often than the journal.
    JournalAhead {
        /// Bytes the journal proves durable.
        journal_covered_bytes: u64,
        /// Bytes the stale cache claimed.
        sqlite_covered_bytes: u64,
    },
    /// The cache claimed more than the journal proves. The journal still wins.
    SqliteAhead {
        /// Bytes the journal proves durable.
        journal_covered_bytes: u64,
        /// Bytes the cache claimed without evidence.
        sqlite_covered_bytes: u64,
    },
    /// Equal totals over a different set of ranges — a cache built from other evidence.
    SameCoverageDifferentIntervals {
        /// The byte total both sides agree on.
        covered_bytes: u64,
    },
}

/// What reconciliation found, and did, about the part file's physical extent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartFileExtent {
    /// The file was exactly the representation length.
    Intact,
    /// The file was short and has been grown back; bytes past `observed_length` were not trusted.
    Reextended {
        /// The extent found on disk.
        observed_length: u64,
        /// The extent restored.
        total_length: u64,
    },
    /// The file was longer than the representation and was deliberately left alone (I-10).
    Overlong {
        /// The extent found on disk.
        observed_length: u64,
        /// The representation length.
        total_length: u64,
    },
}

/// The reconciled durable state of one download.
#[derive(Clone, Debug)]
pub struct Reconciliation {
    state: DownloadState,
    error_kind: Option<DownloadErrorKind>,
    intervals: IntervalMap,
    covered_bytes: u64,
    next_sequence: u64,
    sealed: Option<[u8; 32]>,
    divergence: CheckpointDivergence,
    extent: PartFileExtent,
    journal_tail_repaired: bool,
    space_reserved: bool,
}

impl Reconciliation {
    /// The lifecycle state the download was left in: `Paused`, or `Failed` with a reason.
    ///
    /// Never `Transferring`. Recovery does not resume anything.
    #[must_use]
    pub const fn state(&self) -> DownloadState {
        self.state
    }

    /// The stable failure category when the download could not be reconciled.
    #[must_use]
    pub const fn error_kind(&self) -> Option<&DownloadErrorKind> {
        self.error_kind.as_ref()
    }

    /// The rebuilt allocator state: `Complete` where the journal proves it, `Pending` elsewhere.
    #[must_use]
    pub const fn intervals(&self) -> &IntervalMap {
        &self.intervals
    }

    /// Total bytes the journal proves durable.
    #[must_use]
    pub const fn covered_bytes(&self) -> u64 {
        self.covered_bytes
    }

    /// The sequence number the next journal append must carry.
    #[must_use]
    pub const fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    /// The final-file digest when the journal carried a `Sealed` record.
    ///
    /// Reported, never acted on. A seal means verification passed, but not that the rename
    /// completed, and I-4 does not allow inferring a finished download from a journal record.
    /// Completing a sealed download is S2-T11's job.
    #[must_use]
    pub const fn sealed(&self) -> Option<&[u8; 32]> {
        self.sealed.as_ref()
    }

    /// How the SQLite cache compared with the journal before it was rewritten.
    #[must_use]
    pub const fn divergence(&self) -> &CheckpointDivergence {
        &self.divergence
    }

    /// What was found, and done, about the part file's extent.
    #[must_use]
    pub const fn extent(&self) -> &PartFileExtent {
        &self.extent
    }

    /// Whether a damaged journal suffix was discarded and durably removed.
    #[must_use]
    pub const fn journal_tail_repaired(&self) -> bool {
        self.journal_tail_repaired
    }

    /// Whether the full extent is known to have physical space reserved after recovery.
    #[must_use]
    pub const fn space_reserved(&self) -> bool {
        self.space_reserved
    }
}

/// Why reconciliation could not run at all.
///
/// A download that cannot be *recovered* is not an error: it becomes a `Failed` reconciliation
/// whose record and artifacts are kept, because they are evidence. These variants are reserved
/// for the cases where the caller asked for something impossible, or the machine failed.
#[derive(Debug, Error)]
pub enum RecoveryError {
    /// No download with this id exists in the store.
    #[error("no download record for the requested id")]
    UnknownDownload,
    /// The download had already reached a terminal state; there is nothing to reconcile.
    #[error("download is already terminal: {state:?}")]
    AlreadyTerminal {
        /// The terminal state found in the record.
        state: DownloadState,
    },
    /// The metadata store could not be read or written.
    #[error("recovery metadata access failed: {0}")]
    Metadata(#[from] MetadataError),
    /// The part file could not be reopened, measured, or re-extended.
    #[error("recovery part-file access failed: {0}")]
    PartFile(#[from] PartFileError),
    /// The journal could not be read or repaired.
    #[error("recovery journal I/O failed: {0}")]
    Io(#[from] io::Error),
    /// The rebuilt interval map rejected a replayed range.
    #[error("recovery could not rebuild the interval map: {0}")]
    Interval(#[from] IntervalMapError),
}

/// Reconcile one download's part file, journal, interval coverage, and SQLite record.
///
/// Runs docs/04 §5 step 2 in its documented order — part-file presence, then extent, then
/// journal replay, then checkpoint arbitration, then the rebuilt map — and persists the result.
/// The download is left `Paused`, or `Failed` with a stable error kind and every artifact
/// preserved.
pub fn reconcile_download(
    store: &mut MetadataStore,
    id: DownloadId,
    journal_path: &Path,
    now_ms: u64,
) -> Result<Reconciliation, RecoveryError> {
    let mut metadata = store
        .load_download(id)?
        .ok_or(RecoveryError::UnknownDownload)?;
    if matches!(
        metadata.state,
        DownloadState::Completed | DownloadState::Failed
    ) {
        return Err(RecoveryError::AlreadyTerminal {
            state: metadata.state,
        });
    }

    // Step 2a — the bytes themselves. Without them nothing else is worth reading.
    //
    // `symlink_metadata` rather than `exists`, which follows: a dangling symlink at this path
    // reports as absent while a symlink to a real file reports as present, and neither answer is
    // about the part file. The reopen below refuses a link outright (B-37); this only makes the
    // diagnosis honest when the path holds something that is not this download's part file.
    match std::fs::symlink_metadata(&metadata.part_path) {
        Ok(found) if found.file_type().is_file() => {}
        Ok(_) => {
            return fail(
                store,
                &mut metadata,
                "storage.part-file-not-regular",
                now_ms,
            );
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return fail(store, &mut metadata, "storage.part-file-missing", now_ms);
        }
        // A check that could not answer is not permission to proceed.
        Err(_) => {
            return fail(store, &mut metadata, "storage.part-file-unreadable", now_ms);
        }
    }
    let Some(total_length) = metadata.total_length else {
        // A part file exists for a representation whose length was never recorded. The two
        // cannot be reconciled, and guessing a length is how offsets get written into the
        // wrong file.
        return fail(store, &mut metadata, "storage.length-unknown", now_ms);
    };

    // Step 2b — the extent, measured before it is restored.
    let part_path = metadata.part_path.clone();
    let recovered = match PartFile::open_existing(&part_path, total_length) {
        Ok(recovered) => recovered,
        // Refused between the check above and the open, or planted while the daemon was down.
        Err(PartFileError::NotARegularFile { .. }) => {
            return fail(
                store,
                &mut metadata,
                "storage.part-file-not-regular",
                now_ms,
            );
        }
        Err(error) => return Err(error.into()),
    };
    let observed_length = recovered.observed_length();
    let extent = if recovered.reextended() {
        PartFileExtent::Reextended {
            observed_length,
            total_length,
        }
    } else if observed_length > total_length {
        PartFileExtent::Overlong {
            observed_length,
            total_length,
        }
    } else {
        PartFileExtent::Intact
    };
    let space_reserved = recovered.space_reserved();
    drop(recovered);

    // Step 2c — replay, repairing a damaged suffix in place.
    let replayed = match recover_journal(journal_path) {
        Ok(outcome) => Some(outcome),
        Err(ReplayError::Io(source)) if source.kind() == io::ErrorKind::NotFound => None,
        // Header damage, an unsupported version, or unsupported flags. I-11 refuses rather than
        // reinterprets, and the journal is left byte-for-byte intact as evidence.
        Err(ReplayError::Format(_)) => {
            return fail(store, &mut metadata, "storage.journal-unreadable", now_ms);
        }
        Err(ReplayError::Io(source)) => return Err(RecoveryError::Io(source)),
    };

    let Some(replayed) = replayed else {
        // No durable evidence at all. Every byte is refetched, which is slow and correct.
        let intervals = IntervalMap::new(total_length);
        return persist(
            store,
            &mut metadata,
            Reconciliation {
                state: DownloadState::Paused,
                error_kind: None,
                intervals,
                covered_bytes: 0,
                next_sequence: 0,
                sealed: None,
                divergence: CheckpointDivergence::NoCheckpoint,
                extent,
                journal_tail_repaired: false,
                space_reserved,
            },
            total_length,
            now_ms,
        );
    };

    // A journal that belongs to another transfer, or describes another representation, must
    // never be applied to this one: its offsets mean nothing here.
    if replayed.header().transfer_id() != &id.as_bytes()
        || replayed.header().total_length() != total_length
    {
        return fail(store, &mut metadata, "storage.journal-mismatched", now_ms);
    }

    let journal_tail_repaired = !matches!(replayed.stop(), ReplayStop::CleanEof);
    let next_sequence = u64::try_from(replayed.records().len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "journal record count overflows u64",
        )
    })?;

    let Ok(state) = effective_state(replayed.header(), replayed.records()) else {
        // Records that frame and checksum cleanly but cannot all be true at once. Refusing is
        // the only safe reading; the artifacts stay for `dp repair` and for a bug report.
        return fail(store, &mut metadata, "storage.journal-invalid", now_ms);
    };
    let effective_length = state.effective_length;
    let sealed = state.sealed;

    // Step 2e — bytes the journal claims but the part file no longer physically holds are not
    // durable, whatever the record says. Discarding them costs a refetch; trusting them writes
    // a hole into the finished file.
    let mut blocks = state.blocks;
    blocks.retain(|block| block.end().is_ok_and(|end| end <= observed_length));
    let merged = merge_adjacent(&blocks)?;

    let mut intervals = IntervalMap::new(effective_length);
    let mut covered_bytes = 0_u64;
    for range in &merged {
        intervals.grant(range.clone(), RECOVERY_WORKER)?;
        intervals.complete(range.clone(), RECOVERY_WORKER)?;
        covered_bytes = covered_bytes
            .checked_add(range.end - range.start)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "covered byte count overflows u64",
                )
            })?;
    }

    // Step 2d — compare, report, and let the journal win regardless of the answer.
    let divergence = compare_checkpoint(store, id, covered_bytes, &merged);

    persist(
        store,
        &mut metadata,
        Reconciliation {
            state: DownloadState::Paused,
            error_kind: None,
            intervals,
            covered_bytes,
            next_sequence,
            sealed,
            divergence,
            extent,
            journal_tail_repaired,
            space_reserved,
        },
        effective_length,
        now_ms,
    )
}

/// Merge blocks that touch, so the rebuilt map holds canonical ranges.
///
/// The fold has already sorted them and proven they do not overlap.
fn merge_adjacent(blocks: &[CompletedBlock]) -> Result<Vec<Range<u64>>, RecoveryError> {
    let mut merged: Vec<Range<u64>> = Vec::new();
    for block in blocks {
        let end = block
            .end()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.detail.to_owned()))?;
        match merged.last_mut() {
            Some(last) if last.end == block.offset => last.end = end,
            _ => merged.push(block.offset..end),
        }
    }
    Ok(merged)
}

/// Read the disposable cache and classify how it compares with the journal.
///
/// A cache row that cannot be decoded is reported and ignored, never propagated: docs/04 §4.1
/// makes a corrupt database an inconvenience rather than data loss, because everything in it
/// can be rebuilt by scanning `journals/`.
fn compare_checkpoint(
    store: &MetadataStore,
    id: DownloadId,
    journal_covered_bytes: u64,
    merged: &[Range<u64>],
) -> CheckpointDivergence {
    let cached = match store.load_checkpoint(id) {
        Ok(Some(cached)) => cached,
        Ok(None) => return CheckpointDivergence::NoCheckpoint,
        Err(_) => return CheckpointDivergence::CheckpointUnreadable,
    };
    let sqlite_covered_bytes = cached.checkpoint().covered_bytes();
    let same_intervals = cached.checkpoint().intervals().len() == merged.len()
        && cached
            .checkpoint()
            .intervals()
            .iter()
            .zip(merged)
            .all(|(cached, range)| cached.start() == range.start && cached.end() == range.end);

    if sqlite_covered_bytes == journal_covered_bytes {
        if same_intervals {
            CheckpointDivergence::Agreed
        } else {
            CheckpointDivergence::SameCoverageDifferentIntervals {
                covered_bytes: journal_covered_bytes,
            }
        }
    } else if sqlite_covered_bytes < journal_covered_bytes {
        CheckpointDivergence::JournalAhead {
            journal_covered_bytes,
            sqlite_covered_bytes,
        }
    } else {
        CheckpointDivergence::SqliteAhead {
            journal_covered_bytes,
            sqlite_covered_bytes,
        }
    }
}

/// Record a download that cannot be recovered, keeping the record and every artifact.
fn fail(
    store: &mut MetadataStore,
    metadata: &mut crate::metadata::DownloadMetadata,
    kind: &'static str,
    now_ms: u64,
) -> Result<Reconciliation, RecoveryError> {
    let error_kind = DownloadErrorKind::new(kind)?;
    let total_length = metadata.total_length.unwrap_or(0);
    metadata.state = DownloadState::Failed;
    metadata.error_kind = Some(error_kind.clone());
    metadata.covered_bytes = 0;
    metadata.updated_at_ms = now_ms;
    store.save_download(metadata)?;

    Ok(Reconciliation {
        state: DownloadState::Failed,
        error_kind: Some(error_kind),
        intervals: IntervalMap::new(total_length),
        covered_bytes: 0,
        next_sequence: 0,
        sealed: None,
        divergence: CheckpointDivergence::NoCheckpoint,
        extent: PartFileExtent::Intact,
        journal_tail_repaired: false,
        space_reserved: false,
    })
}

/// Write the journal's truth back into SQLite, then hand the reconciliation to the caller.
///
/// The download row is written before the checkpoint because the checkpoint is validated
/// against the row's `total_length`, which a `Truncate` record may just have changed.
fn persist(
    store: &mut MetadataStore,
    metadata: &mut crate::metadata::DownloadMetadata,
    reconciled: Reconciliation,
    effective_length: u64,
    now_ms: u64,
) -> Result<Reconciliation, RecoveryError> {
    if metadata.total_length != Some(effective_length) {
        // A `Truncate` means the server reported a different representation than the one the
        // capability probe examined. Range support proven against 32 bytes says nothing about
        // 12, so the evidence is discarded rather than carried across (I-6). `Unknown` is the
        // honest state — "we have not asked about *this* representation" — and it is one of
        // B-16's re-probe triggers.
        metadata.identity.range_support = RangeSupport::Unknown;
    }
    metadata.state = reconciled.state;
    metadata.error_kind = None;
    metadata.total_length = Some(effective_length);
    metadata.covered_bytes = reconciled.covered_bytes;
    metadata.space_reserved = reconciled.space_reserved;
    metadata.updated_at_ms = now_ms;
    store.save_download(metadata)?;

    let intervals = reconciled
        .intervals
        .intervals()
        .iter()
        .filter(|interval| *interval.state() == downpour_intervals::IntervalState::Complete)
        .map(|interval| CompleteInterval::try_new(interval.start(), interval.end()))
        .collect::<Result<Vec<_>, _>>()?;
    let checkpoint = Checkpoint::try_new(effective_length, intervals)?;
    // A checkpoint too large for the cache is not an error: replay remains authoritative and
    // the daemon simply pays for it at the next start.
    store.save_checkpoint(metadata.id, reconciled.next_sequence, now_ms, &checkpoint)?;

    Ok(reconciled)
}
