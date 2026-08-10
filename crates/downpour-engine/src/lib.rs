//! Download scheduling and the sole allocator of worker-owned byte ranges.
//!
//! Stage 3 builds dynamic HTTP/1.1 segmentation around one canonical allocator. Blocking durable
//! writes are serialized with allocation through the bounded state actor in [`writer_service`];
//! network workers remain a later increment and may only write through allocator-issued grants.

#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable
    )
)]

use std::collections::BTreeSet;
use std::ops::Range;

use downpour_intervals::{Interval, IntervalMap, IntervalMapError, IntervalState, WorkerId};
use thiserror::Error;

pub mod download;
pub mod segmented;
pub mod storage_sink;
pub mod worker_pool;
pub mod writer_service;

pub use download::{DownloadError, SingleStream, StorageLayout};
pub use segmented::SegmentedDownload;
pub use storage_sink::{Artifacts, StorageSink};

/// Default lower bound for either half of a split grant.
pub const DEFAULT_MIN_SPLIT_BYTES: u64 = 1024 * 1024;

/// One exact byte range owned by one worker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
    worker: WorkerId,
    range: Range<u64>,
}

impl Grant {
    /// Worker that exclusively owns this range.
    #[must_use]
    pub const fn worker(&self) -> WorkerId {
        self.worker
    }

    /// Exact half-open range the worker may write.
    #[must_use]
    pub const fn range(&self) -> &Range<u64> {
        &self.range
    }
}

/// Result of one idle worker asking the allocator for work.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Allocation {
    grant: Grant,
    shortened: Option<Grant>,
}

impl Allocation {
    /// New grant issued to the requesting worker.
    #[must_use]
    pub const fn grant(&self) -> &Grant {
        &self.grant
    }

    /// Existing source grant shortened by a split, if work came from an active worker.
    #[must_use]
    pub const fn shortened(&self) -> Option<&Grant> {
        self.shortened.as_ref()
    }
}

/// A segment-allocation request that could not be honoured safely.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum AllocatorError {
    /// A zero split floor would permit zero-useful-work churn.
    #[error("minimum split size must be greater than zero")]
    ZeroMinimumSplit,
    /// A worker that already owns bytes asked for a second grant.
    #[error("worker {worker:?} already owns an active grant")]
    WorkerAlreadyActive {
        /// Worker that made the invalid request.
        worker: WorkerId,
    },
    /// The canonical interval map rejected the mutation.
    #[error("interval map rejected allocation: {0}")]
    Interval(#[from] IntervalMapError),
}

/// Sole allocator for one download's byte space.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentAllocator {
    intervals: IntervalMap,
    min_split_bytes: u64,
}

impl SegmentAllocator {
    /// Create a pending map over `[0, total_length)`.
    pub fn new(total_length: u64, min_split_bytes: u64) -> Result<Self, AllocatorError> {
        if min_split_bytes == 0 {
            return Err(AllocatorError::ZeroMinimumSplit);
        }
        Ok(Self {
            intervals: IntervalMap::new(total_length),
            min_split_bytes,
        })
    }

    /// Canonical interval partition, exposed for scheduling evidence and recovery.
    #[must_use]
    pub fn intervals(&self) -> &[Interval] {
        self.intervals.intervals()
    }

    /// Configured lower bound for either half of a split.
    #[must_use]
    pub const fn min_split_bytes(&self) -> u64 {
        self.min_split_bytes
    }

    /// Allocate work to an idle worker using Stage 3's deterministic policy.
    ///
    /// Pending work wins. When no pending interval remains, the largest interval **owned by a
    /// quiesced worker** is split in half if both resulting grants meet [`Self::min_split_bytes`].
    /// Equal-sized candidates are resolved toward the lowest offset so replaying the same state
    /// produces the same decision.
    ///
    /// `quiesced` names the workers that have no request in flight. Shortening the grant of a
    /// worker whose response is still arriving would make the rest of that response fail against
    /// a boundary it has never seen, so a scheduler must stop a worker before its segment can be
    /// split — see [`Self::split_candidate`] for choosing which one. The restriction lives here
    /// rather than in the scheduler so that "a live grant is never shortened underneath its own
    /// response" is a property of the allocator instead of a convention the caller must remember.
    pub fn allocate(
        &mut self,
        worker: WorkerId,
        quiesced: &BTreeSet<WorkerId>,
    ) -> Result<Option<Allocation>, AllocatorError> {
        if self.intervals.intervals().iter().any(|interval| {
            matches!(
                interval.state(),
                IntervalState::InProgress { worker: owner } if *owner == worker
            )
        }) {
            return Err(AllocatorError::WorkerAlreadyActive { worker });
        }

        if let Some(range) = self.preferred_range(|state| matches!(state, IntervalState::Pending)) {
            self.intervals.grant(range.clone(), worker)?;
            return Ok(Some(Allocation {
                grant: Grant { worker, range },
                shortened: None,
            }));
        }

        let Some(required) = self.min_split_bytes.checked_mul(2) else {
            return Ok(None);
        };
        let Some((range, source)) = self.preferred_active(|owner| quiesced.contains(&owner)) else {
            return Ok(None);
        };
        if range.end - range.start < required {
            return Ok(None);
        }

        let split_at = range.start + (range.end - range.start) / 2;
        self.intervals.split_in_progress(split_at, source, worker)?;
        Ok(Some(Allocation {
            grant: Grant {
                worker,
                range: split_at..range.end,
            },
            shortened: Some(Grant {
                worker: source,
                range: range.start..split_at,
            }),
        }))
    }

    /// Mark a durably committed sub-range complete for its owning worker.
    pub fn complete_durable(
        &mut self,
        worker: WorkerId,
        range: Range<u64>,
    ) -> Result<(), AllocatorError> {
        self.intervals.complete(range, worker)?;
        Ok(())
    }

    /// Return every active grant of a dead or retiring worker to pending state.
    pub fn abandon(&mut self, worker: WorkerId) -> u64 {
        self.intervals.abandon(worker)
    }

    /// The worker whose grant the next split would target, ignoring whether it is quiesced.
    ///
    /// A scheduler with an idle worker and no pending work calls this to learn which single peer
    /// it must stop before [`Self::allocate`] can split anything. `None` means no active interval
    /// is large enough to split, so the idle worker retires instead — stopping a peer for a
    /// segment that cannot be divided would cost a connection and buy nothing.
    #[must_use]
    pub fn split_candidate(&self) -> Option<WorkerId> {
        let required = self.min_split_bytes.checked_mul(2)?;
        let (range, worker) = self.preferred_active(|_| true)?;
        (range.end - range.start >= required).then_some(worker)
    }

    fn preferred_range(&self, predicate: impl Fn(&IntervalState) -> bool) -> Option<Range<u64>> {
        self.intervals
            .intervals()
            .iter()
            .filter(|interval| predicate(interval.state()))
            .max_by(|left, right| {
                left.len()
                    .cmp(&right.len())
                    .then_with(|| right.start().cmp(&left.start()))
            })
            .map(|interval| interval.start()..interval.end())
    }

    fn preferred_active(
        &self,
        eligible: impl Fn(WorkerId) -> bool,
    ) -> Option<(Range<u64>, WorkerId)> {
        self.intervals
            .intervals()
            .iter()
            .filter_map(|interval| match interval.state() {
                IntervalState::InProgress { worker } if eligible(*worker) => {
                    Some((interval, *worker))
                }
                _ => None,
            })
            .max_by(|(left, _), (right, _)| {
                left.len()
                    .cmp(&right.len())
                    .then_with(|| right.start().cmp(&left.start()))
            })
            .map(|(interval, worker)| (interval.start()..interval.end(), worker))
    }

    pub(crate) fn interval_map_mut(&mut self) -> &mut IntervalMap {
        &mut self.intervals
    }
}
