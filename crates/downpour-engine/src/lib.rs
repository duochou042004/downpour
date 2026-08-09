//! Download scheduling and the sole allocator of worker-owned byte ranges.
//!
//! Stage 3 starts with the pure segment allocator. Network workers and durable storage are
//! deliberately absent from this first proof commit: the allocator contract must be able to fail
//! before either caller exists.

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

use std::ops::Range;

use downpour_intervals::{Interval, IntervalMap, IntervalMapError, WorkerId};
use thiserror::Error;

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
///
/// This proof scaffold intentionally issues no grants yet. The first S3 commit establishes a
/// behavioral red test; the following implementation commit supplies the policy.
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

    /// Allocate work to an idle worker.
    ///
    /// The proof commit returns no work so the behavior test is red for the intended reason.
    pub fn allocate(&mut self, _worker: WorkerId) -> Result<Option<Allocation>, AllocatorError> {
        Ok(None)
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
}
