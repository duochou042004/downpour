//! The canonical interval map responsible for enforcing Downpour invariant I-2.
//!
//! The map is a partition of `[0, total_length)`. Mutations are transactional: an invalid
//! grant, split, or completion returns an error without changing the partition. Scheduling
//! policy deliberately lives elsewhere; this crate only owns byte-range state and the rule
//! that two workers can never own the same byte.

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

use std::error::Error;
use std::fmt;
use std::ops::Range;

/// Stable identity of a worker that owns an in-progress interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkerId(u64);

impl WorkerId {
    /// Creates a worker identity from the allocator's stable numeric id.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric identity assigned by the allocator.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// The durability state of one byte interval.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntervalState {
    /// The bytes have not been assigned to a worker.
    Pending,
    /// The bytes are assigned but are not yet durably committed.
    InProgress {
        /// The only worker permitted to write these bytes.
        worker: WorkerId,
    },
    /// The bytes and their journal record are durable.
    Complete,
}

/// One non-empty half-open interval in the canonical partition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interval {
    start: u64,
    end: u64,
    state: IntervalState,
}

impl Interval {
    /// Returns the inclusive start offset.
    #[must_use]
    pub const fn start(&self) -> u64 {
        self.start
    }

    /// Returns the exclusive end offset.
    #[must_use]
    pub const fn end(&self) -> u64 {
        self.end
    }

    /// Returns the interval's current state.
    #[must_use]
    pub const fn state(&self) -> &IntervalState {
        &self.state
    }

    /// Returns the number of bytes in this interval.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.end - self.start
    }

    /// Reports whether this interval contains no bytes.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// A rejected interval-map mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntervalMapError {
    /// The requested half-open range is empty, reversed, or outside the file.
    InvalidRange {
        /// Requested inclusive start.
        start: u64,
        /// Requested exclusive end.
        end: u64,
        /// File length that bounds all valid ranges.
        total_length: u64,
    },
    /// A grant covered bytes that were not all pending.
    RangeNotPending {
        /// Requested inclusive start.
        start: u64,
        /// Requested exclusive end.
        end: u64,
    },
    /// A completion covered bytes not wholly owned by the reporting worker.
    RangeNotOwned {
        /// Requested inclusive start.
        start: u64,
        /// Requested exclusive end.
        end: u64,
        /// Worker that reported the completion.
        worker: WorkerId,
    },
    /// No interval owned by the source worker contains the requested split point.
    SplitPointUnavailable {
        /// Requested split offset.
        at: u64,
        /// Worker expected to own both sides before the split.
        source: WorkerId,
    },
    /// Splitting to the same worker would create no new grant.
    SplitWorkerUnchanged {
        /// Worker supplied as both source and recipient.
        worker: WorkerId,
    },
}

impl fmt::Display for IntervalMapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRange {
                start,
                end,
                total_length,
            } => write!(
                formatter,
                "range [{start}, {end}) is not a non-empty subset of [0, {total_length})"
            ),
            Self::RangeNotPending { start, end } => {
                write!(formatter, "range [{start}, {end}) is not wholly pending")
            }
            Self::RangeNotOwned { start, end, worker } => write!(
                formatter,
                "range [{start}, {end}) is not wholly owned by worker {}",
                worker.get()
            ),
            Self::SplitPointUnavailable { at, source } => write!(
                formatter,
                "offset {at} is not inside a grant owned by worker {}",
                source.get()
            ),
            Self::SplitWorkerUnchanged { worker } => write!(
                formatter,
                "worker {} cannot split a grant to itself",
                worker.get()
            ),
        }
    }
}

impl Error for IntervalMapError {}

/// Canonical partition of a download's complete byte space.
///
/// Adjacent intervals with equal state are merged after every successful mutation. The
/// partition is empty only for a zero-length representation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntervalMap {
    total_length: u64,
    intervals: Vec<Interval>,
}

impl IntervalMap {
    /// Creates a map whose entire byte space is pending.
    #[must_use]
    pub fn new(total_length: u64) -> Self {
        let intervals = if total_length == 0 {
            Vec::new()
        } else {
            vec![Interval {
                start: 0,
                end: total_length,
                state: IntervalState::Pending,
            }]
        };
        Self {
            total_length,
            intervals,
        }
    }

    /// Returns the represented file length.
    #[must_use]
    pub const fn total_length(&self) -> u64 {
        self.total_length
    }

    /// Returns the canonical intervals in increasing offset order.
    #[must_use]
    pub fn intervals(&self) -> &[Interval] {
        &self.intervals
    }

    /// Assigns a pending range to one worker.
    ///
    /// The request may cover part of a pending interval. It is rejected atomically if any
    /// requested byte is already assigned or complete.
    pub fn grant(&mut self, range: Range<u64>, worker: WorkerId) -> Result<(), IntervalMapError> {
        self.validate_range(&range)?;
        if !self.range_matches(&range, |state| matches!(state, IntervalState::Pending)) {
            return Err(IntervalMapError::RangeNotPending {
                start: range.start,
                end: range.end,
            });
        }

        self.replace_range(range, IntervalState::InProgress { worker });
        Ok(())
    }

    /// Reassigns the right side of one active grant to a different worker.
    ///
    /// Scheduling policy chooses `at`; this method only proves that the point lies strictly
    /// inside a grant owned by `source`, preserving non-overlapping ownership by construction.
    pub fn split_in_progress(
        &mut self,
        at: u64,
        source: WorkerId,
        recipient: WorkerId,
    ) -> Result<(), IntervalMapError> {
        if source == recipient {
            return Err(IntervalMapError::SplitWorkerUnchanged { worker: source });
        }

        let Some(index) = self.intervals.iter().position(|interval| {
            interval.start < at
                && at < interval.end
                && interval.state == IntervalState::InProgress { worker: source }
        }) else {
            return Err(IntervalMapError::SplitPointUnavailable { at, source });
        };

        let original = self.intervals[index].clone();
        self.intervals[index] = Interval {
            start: original.start,
            end: at,
            state: IntervalState::InProgress { worker: source },
        };
        self.intervals.insert(
            index + 1,
            Interval {
                start: at,
                end: original.end,
                state: IntervalState::InProgress { worker: recipient },
            },
        );
        self.normalise();
        Ok(())
    }

    /// Marks a worker-owned range complete after the storage layer confirms durability.
    ///
    /// This type cannot prove durability itself. Its API requires the caller to make this
    /// transition only after I-1's storage commit point; S2's durable writer owns that proof.
    pub fn complete(
        &mut self,
        range: Range<u64>,
        worker: WorkerId,
    ) -> Result<(), IntervalMapError> {
        self.validate_range(&range)?;
        if !self.range_matches(&range, |state| {
            *state == IntervalState::InProgress { worker }
        }) {
            return Err(IntervalMapError::RangeNotOwned {
                start: range.start,
                end: range.end,
                worker,
            });
        }

        self.replace_range(range, IntervalState::Complete);
        Ok(())
    }

    /// Returns every active grant owned by `worker` to pending state.
    ///
    /// The return value is the number of bytes released. Abandoning an unknown worker is a
    /// no-op, which makes repeated worker-death notifications idempotent.
    pub fn abandon(&mut self, worker: WorkerId) -> u64 {
        let mut released = 0_u64;
        for interval in &mut self.intervals {
            if interval.state == (IntervalState::InProgress { worker }) {
                released += interval.len();
                interval.state = IntervalState::Pending;
            }
        }
        self.normalise();
        released
    }

    fn validate_range(&self, range: &Range<u64>) -> Result<(), IntervalMapError> {
        if range.start >= range.end || range.end > self.total_length {
            return Err(IntervalMapError::InvalidRange {
                start: range.start,
                end: range.end,
                total_length: self.total_length,
            });
        }
        Ok(())
    }

    fn range_matches(
        &self,
        range: &Range<u64>,
        predicate: impl Fn(&IntervalState) -> bool,
    ) -> bool {
        self.intervals
            .iter()
            .filter(|interval| interval.start < range.end && range.start < interval.end)
            .all(|interval| predicate(&interval.state))
    }

    fn replace_range(&mut self, range: Range<u64>, replacement: IntervalState) {
        let mut next = Vec::with_capacity(self.intervals.len() + 2);
        for interval in &self.intervals {
            if interval.end <= range.start || range.end <= interval.start {
                Self::push_merged(&mut next, interval.clone());
                continue;
            }

            if interval.start < range.start {
                Self::push_merged(
                    &mut next,
                    Interval {
                        start: interval.start,
                        end: range.start,
                        state: interval.state.clone(),
                    },
                );
            }

            Self::push_merged(
                &mut next,
                Interval {
                    start: interval.start.max(range.start),
                    end: interval.end.min(range.end),
                    state: replacement.clone(),
                },
            );

            if range.end < interval.end {
                Self::push_merged(
                    &mut next,
                    Interval {
                        start: range.end,
                        end: interval.end,
                        state: interval.state.clone(),
                    },
                );
            }
        }
        self.intervals = next;
        debug_assert!(self.invariants_hold());
    }

    fn normalise(&mut self) {
        let old = std::mem::take(&mut self.intervals);
        let mut next = Vec::with_capacity(old.len());
        for interval in old {
            Self::push_merged(&mut next, interval);
        }
        self.intervals = next;
        debug_assert!(self.invariants_hold());
    }

    fn push_merged(intervals: &mut Vec<Interval>, interval: Interval) {
        if let Some(previous) = intervals.last_mut()
            && previous.end == interval.start
            && previous.state == interval.state
        {
            previous.end = interval.end;
            return;
        }
        intervals.push(interval);
    }

    fn invariants_hold(&self) -> bool {
        if self.total_length == 0 {
            return self.intervals.is_empty();
        }

        let mut cursor = 0_u64;
        let mut previous_state: Option<&IntervalState> = None;
        for interval in &self.intervals {
            if interval.start != cursor || interval.start >= interval.end {
                return false;
            }
            if previous_state == Some(&interval.state) {
                return false;
            }
            cursor = interval.end;
            previous_state = Some(&interval.state);
        }
        cursor == self.total_length
    }
}
