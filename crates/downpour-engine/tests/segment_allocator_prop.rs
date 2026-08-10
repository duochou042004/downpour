//! Semantic proofs for the Stage 3 segment allocator and invariant I-2.
//!
//! Canonical intervals are necessary but not sufficient: replacing one owner with another keeps
//! the partition disjoint while two workers still believe they own the bytes. These tests compute
//! permission and the exact scheduling decision independently from the allocator before each call.

use std::collections::BTreeSet;
use std::ops::Range;

use downpour_engine::{Allocation, AllocatorError, Grant, SegmentAllocator};
use downpour_intervals::{Interval, IntervalState, WorkerId};
use proptest::prelude::*;
use proptest::test_runner::TestCaseResult;

const TOTAL: u64 = 64;
const MIN_SPLIT: u64 = 8;

#[derive(Clone, Debug)]
enum Operation {
    Allocate {
        worker: WorkerId,
        /// Workers with no request in flight. Generated independently of the map so the sequence
        /// reaches states where the largest active remainder is *not* splittable, which is the
        /// only place the restriction can be observed.
        quiesced: BTreeSet<WorkerId>,
    },
    Complete {
        worker: WorkerId,
        range: Range<u64>,
    },
    Abandon(WorkerId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExpectedGrant {
    worker: WorkerId,
    range: Range<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExpectedAllocation {
    grant: ExpectedGrant,
    shortened: Option<ExpectedGrant>,
}

fn arb_worker() -> impl Strategy<Value = WorkerId> {
    (0_u64..8).prop_map(WorkerId::new)
}

fn arb_range() -> impl Strategy<Value = Range<u64>> {
    (0_u64..=TOTAL, 0_u64..=TOTAL).prop_map(|(start, end)| start..end)
}

fn arb_quiesced() -> impl Strategy<Value = BTreeSet<WorkerId>> {
    proptest::collection::btree_set(arb_worker(), 0..8)
}

fn arb_operation() -> impl Strategy<Value = Operation> {
    prop_oneof![
        (arb_worker(), arb_quiesced())
            .prop_map(|(worker, quiesced)| Operation::Allocate { worker, quiesced }),
        (arb_worker(), arb_range())
            .prop_map(|(worker, range)| Operation::Complete { worker, range }),
        arb_worker().prop_map(Operation::Abandon),
    ]
}

fn worker_is_active(intervals: &[Interval], worker: WorkerId) -> bool {
    intervals.iter().any(
        |interval| matches!(interval.state(), IntervalState::InProgress { worker: owner } if *owner == worker),
    )
}

fn preferred(
    intervals: &[Interval],
    predicate: impl Fn(&IntervalState) -> bool,
) -> Option<&Interval> {
    intervals
        .iter()
        .filter(|interval| predicate(interval.state()))
        .max_by(|left, right| {
            left.len()
                .cmp(&right.len())
                .then_with(|| right.start().cmp(&left.start()))
        })
}

/// Independent statement of §4.2's S3 policy.
///
/// It reads only the public partition and does not call an allocator helper. That is what makes
/// acceptance falsifiable in the only-if direction rather than inferred from the result itself.
fn expected_allocation(
    allocator: &SegmentAllocator,
    worker: WorkerId,
    quiesced: &BTreeSet<WorkerId>,
) -> Result<Option<ExpectedAllocation>, AllocatorError> {
    let intervals = allocator.intervals();
    if worker_is_active(intervals, worker) {
        return Err(AllocatorError::WorkerAlreadyActive { worker });
    }

    if let Some(pending) = preferred(intervals, |state| matches!(state, IntervalState::Pending)) {
        return Ok(Some(ExpectedAllocation {
            grant: ExpectedGrant {
                worker,
                range: pending.start()..pending.end(),
            },
            shortened: None,
        }));
    }

    let Some(source) = preferred(
        intervals,
        |state| matches!(state, IntervalState::InProgress { worker: owner } if quiesced.contains(owner)),
    ) else {
        return Ok(None);
    };
    let Some(required) = allocator.min_split_bytes().checked_mul(2) else {
        return Ok(None);
    };
    if source.len() < required {
        return Ok(None);
    }
    let IntervalState::InProgress {
        worker: source_worker,
    } = source.state()
    else {
        return Ok(None);
    };
    let split_at = source.start() + source.len() / 2;
    Ok(Some(ExpectedAllocation {
        grant: ExpectedGrant {
            worker,
            range: split_at..source.end(),
        },
        shortened: Some(ExpectedGrant {
            worker: *source_worker,
            range: source.start()..split_at,
        }),
    }))
}

/// Every worker quiesced: the shape of the example tests, which drive the allocator directly and
/// never have a request in flight.
fn everyone() -> BTreeSet<WorkerId> {
    (0..16).map(WorkerId::new).collect()
}

fn actual_grant(grant: &Grant) -> ExpectedGrant {
    ExpectedGrant {
        worker: grant.worker(),
        range: grant.range().clone(),
    }
}

fn actual_allocation(allocation: &Allocation) -> ExpectedAllocation {
    ExpectedAllocation {
        grant: actual_grant(allocation.grant()),
        shortened: allocation.shortened().map(actual_grant),
    }
}

fn wholly_owned(intervals: &[Interval], range: &Range<u64>, worker: WorkerId) -> bool {
    range.start < range.end
        && range.end <= TOTAL
        && intervals
            .iter()
            .filter(|interval| interval.start() < range.end && range.start < interval.end())
            .all(|interval| *interval.state() == IntervalState::InProgress { worker })
}

fn wholly_complete(intervals: &[Interval], range: &Range<u64>) -> bool {
    range.start < range.end
        && range.end <= TOTAL
        && intervals
            .iter()
            .filter(|interval| interval.start() < range.end && range.start < interval.end())
            .all(|interval| *interval.state() == IntervalState::Complete)
}

fn assert_partition(intervals: &[Interval]) -> TestCaseResult {
    let mut cursor = 0;
    let mut previous = None;
    for interval in intervals {
        prop_assert_eq!(
            interval.start(),
            cursor,
            "gap or overlap before {:?}",
            interval
        );
        prop_assert!(interval.start() < interval.end());
        if let Some(previous) = previous {
            prop_assert_ne!(previous, interval.state());
        }
        cursor = interval.end();
        previous = Some(interval.state());
    }
    prop_assert_eq!(cursor, TOTAL);
    Ok(())
}

fn apply(allocator: &mut SegmentAllocator, operation: Operation) -> TestCaseResult {
    let before = allocator.clone();
    match operation {
        Operation::Allocate { worker, quiesced } => {
            let expected = expected_allocation(&before, worker, &quiesced);
            let actual = allocator.allocate(worker, &quiesced);
            match (expected, actual) {
                (Ok(expected), Ok(actual)) => {
                    prop_assert_eq!(actual.as_ref().map(actual_allocation), expected.clone());
                    match expected {
                        Some(expected) => {
                            prop_assert!(
                                wholly_owned(
                                    allocator.intervals(),
                                    &expected.grant.range,
                                    expected.grant.worker
                                ),
                                "returned grant was not installed in the canonical map"
                            );
                            if let Some(shortened) = expected.shortened {
                                prop_assert!(
                                    wholly_owned(
                                        allocator.intervals(),
                                        &shortened.range,
                                        shortened.worker
                                    ),
                                    "shortened source grant was not installed in the canonical map"
                                );
                            }
                        }
                        None => prop_assert_eq!(
                            &*allocator,
                            &before,
                            "an allocation reporting no work mutated the map"
                        ),
                    }
                }
                (Err(expected), Err(actual)) => {
                    prop_assert_eq!(actual, expected);
                    prop_assert_eq!(
                        &*allocator,
                        &before,
                        "a rejected allocation mutated the map"
                    );
                }
                (expected, actual) => prop_assert!(
                    false,
                    "allocation result disagreed with independent permission oracle: expected \
                     {expected:?}, actual {actual:?}"
                ),
            }
        }
        Operation::Complete { worker, range } => {
            let permitted = wholly_owned(before.intervals(), &range, worker);
            let result = allocator.complete_durable(worker, range.clone());
            prop_assert_eq!(
                result.is_ok(),
                permitted,
                "completion acceptance disagreed with ownership before the call for {:?}",
                range
            );
            if !permitted {
                prop_assert_eq!(&*allocator, &before, "rejected completion mutated the map");
            } else {
                prop_assert!(
                    wholly_complete(allocator.intervals(), &range),
                    "accepted completion did not mark the whole range complete"
                );
            }
        }
        Operation::Abandon(worker) => {
            let expected = before
                .intervals()
                .iter()
                .filter_map(|interval| match interval.state() {
                    IntervalState::InProgress { worker: owner } if *owner == worker => {
                        Some(interval.len())
                    }
                    _ => None,
                })
                .sum::<u64>();
            prop_assert_eq!(allocator.abandon(worker), expected);
            prop_assert!(!worker_is_active(allocator.intervals(), worker));
        }
    }
    assert_partition(allocator.intervals())
}

#[test]
fn forced_sequence_reaches_split_death_reclaim_and_stale_completion() {
    let first = WorkerId::new(1);
    let second = WorkerId::new(2);
    let replacement = WorkerId::new(3);
    let mut allocator = SegmentAllocator::new(TOTAL, MIN_SPLIT).expect("valid allocator");

    let initial = allocator
        .allocate(first, &everyone())
        .expect("allocation is valid")
        .expect("pending bytes exist");
    assert_eq!(initial.grant().range(), &(0..TOTAL));

    let split = allocator
        .allocate(second, &everyone())
        .expect("allocation is valid")
        .expect("the active grant is splittable");
    assert_eq!(split.grant().range(), &(32..64));
    assert_eq!(
        split.shortened().map(Grant::range),
        Some(&(0..32)),
        "the original worker must learn its grant was shortened"
    );

    allocator
        .complete_durable(second, 32..64)
        .expect("the recipient may complete exactly the grant it owns");
    assert!(
        wholly_complete(allocator.intervals(), &(32..64)),
        "accepted completion must change the canonical state"
    );

    assert_eq!(allocator.abandon(first), 32, "worker death reclaimed bytes");
    let reclaimed = allocator
        .allocate(replacement, &everyone())
        .expect("allocation is valid")
        .expect("reclaimed pending bytes exist");
    assert_eq!(reclaimed.grant().range(), &(0..32));

    let before = allocator.clone();
    assert!(
        allocator.complete_durable(first, 0..32).is_err(),
        "a dead worker's stale completion must not bless its replacement's bytes"
    );
    assert_eq!(allocator, before, "stale completion mutated the map");
}

#[test]
fn neither_half_may_fall_below_the_minimum_split_size() {
    let mut allocator = SegmentAllocator::new(15, 8).expect("valid allocator");
    assert!(
        allocator
            .allocate(WorkerId::new(1), &everyone())
            .expect("allocation is valid")
            .is_some()
    );
    assert_eq!(
        allocator
            .allocate(WorkerId::new(2), &everyone())
            .expect("allocation is valid"),
        None
    );
}

#[test]
fn a_zero_minimum_split_is_rejected() {
    assert_eq!(
        SegmentAllocator::new(TOTAL, 0),
        Err(AllocatorError::ZeroMinimumSplit)
    );
}

#[test]
fn the_largest_active_remainder_is_split_before_a_smaller_one() {
    let first = WorkerId::new(1);
    let second = WorkerId::new(2);
    let third = WorkerId::new(3);
    let mut allocator = SegmentAllocator::new(96, MIN_SPLIT).expect("valid allocator");

    allocator
        .allocate(first, &everyone())
        .expect("allocation is valid")
        .expect("pending bytes exist");
    allocator
        .allocate(second, &everyone())
        .expect("allocation is valid")
        .expect("the initial grant is splittable");
    allocator
        .complete_durable(second, 48..80)
        .expect("the second worker owns this prefix of its grant");

    let split = allocator
        .allocate(third, &everyone())
        .expect("allocation is valid")
        .expect("the 48-byte remainder is splittable");
    assert_eq!(
        split.grant().range(),
        &(24..48),
        "the 48-byte first-worker remainder must win over the 16-byte second-worker remainder"
    );
    assert_eq!(split.shortened().map(Grant::worker), Some(first));
}

#[test]
fn equal_active_remainders_choose_the_lowest_offset() {
    let mut allocator = SegmentAllocator::new(TOTAL, MIN_SPLIT).expect("valid allocator");
    allocator
        .allocate(WorkerId::new(1), &everyone())
        .expect("allocation is valid")
        .expect("pending bytes exist");
    allocator
        .allocate(WorkerId::new(2), &everyone())
        .expect("allocation is valid")
        .expect("the initial grant is splittable");

    let allocation = allocator
        .allocate(WorkerId::new(3), &everyone())
        .expect("allocation is valid")
        .expect("both equal remainders are splittable");
    assert_eq!(
        allocation.grant().range(),
        &(16..32),
        "equal-sized candidates must resolve toward the lowest offset"
    );
}

#[test]
fn the_largest_pending_remainder_is_granted_before_any_active_split() {
    let first = WorkerId::new(1);
    let second = WorkerId::new(2);
    let separator = WorkerId::new(3);
    let recipient = WorkerId::new(4);
    let mut allocator = SegmentAllocator::new(96, MIN_SPLIT).expect("valid allocator");

    allocator
        .allocate(first, &everyone())
        .expect("allocation is valid")
        .expect("pending bytes exist");
    allocator
        .allocate(second, &everyone())
        .expect("allocation is valid")
        .expect("the initial grant is splittable");
    allocator
        .allocate(separator, &everyone())
        .expect("allocation is valid")
        .expect("one active half is splittable");

    assert_eq!(allocator.abandon(first), 24);
    assert_eq!(allocator.abandon(second), 48);
    let allocation = allocator
        .allocate(recipient, &everyone())
        .expect("allocation is valid")
        .expect("pending bytes exist");
    assert_eq!(
        allocation.grant().range(),
        &(48..96),
        "the 48-byte pending range must win over the 24-byte pending range and active work"
    );
    assert_eq!(allocation.shortened(), None, "pending work is not a split");
}

#[test]
fn a_worker_with_a_request_in_flight_never_has_its_grant_split() {
    let busy = WorkerId::new(1);
    let idle = WorkerId::new(2);
    let mut allocator = SegmentAllocator::new(TOTAL, MIN_SPLIT).expect("valid allocator");
    allocator
        .allocate(busy, &everyone())
        .expect("allocation is valid")
        .expect("pending bytes exist");

    // The only-if direction. The interval is large enough, the requester is idle, and the sole
    // reason to refuse is that the owner has a response arriving that has never seen the new
    // boundary. Shortening it here is what turns a live worker into a fatal sink error.
    let quiesced = BTreeSet::from([idle]);
    assert_eq!(
        allocator
            .allocate(idle, &quiesced)
            .expect("allocation is valid"),
        None,
        "a grant whose owner is still transferring must not be split"
    );
    assert_eq!(
        allocator.intervals().len(),
        1,
        "a refused split must leave the map untouched"
    );

    // The if direction, from the same state: once the owner is quiesced the split happens, so the
    // refusal above was the restriction and not an unreachable configuration.
    let quiesced = BTreeSet::from([idle, busy]);
    let allocation = allocator
        .allocate(idle, &quiesced)
        .expect("allocation is valid")
        .expect("a quiesced owner's grant is splittable");
    assert_eq!(allocation.grant().range(), &(32..64));
    assert_eq!(allocation.shortened().map(Grant::worker), Some(busy));
}

#[test]
fn the_split_candidate_names_the_worker_that_would_have_to_be_stopped() {
    let first = WorkerId::new(1);
    let second = WorkerId::new(2);
    let mut allocator = SegmentAllocator::new(96, MIN_SPLIT).expect("valid allocator");
    assert_eq!(
        allocator.split_candidate(),
        None,
        "with everything pending there is nobody to stop"
    );

    allocator
        .allocate(first, &everyone())
        .expect("allocation is valid")
        .expect("pending bytes exist");
    allocator
        .allocate(second, &everyone())
        .expect("allocation is valid")
        .expect("the initial grant is splittable");
    allocator
        .complete_durable(second, 48..80)
        .expect("the second worker owns this prefix of its grant");

    // Same preference the split itself uses: the largest remainder, which is the first worker's
    // 48 bytes rather than the second worker's 16.
    assert_eq!(
        allocator.split_candidate(),
        Some(first),
        "the candidate must be the owner the allocator would actually split"
    );
    let allocation = allocator
        .allocate(WorkerId::new(3), &BTreeSet::from([WorkerId::new(3), first]))
        .expect("allocation is valid")
        .expect("the named candidate is splittable once quiesced");
    assert_eq!(allocation.shortened().map(Grant::worker), Some(first));
}

#[test]
fn no_candidate_is_named_when_nothing_is_large_enough_to_divide() {
    let worker = WorkerId::new(1);
    let mut allocator = SegmentAllocator::new(15, 8).expect("valid allocator");
    allocator
        .allocate(worker, &everyone())
        .expect("allocation is valid")
        .expect("pending bytes exist");
    assert_eq!(
        allocator.split_candidate(),
        None,
        "stopping a worker for a segment that cannot be divided costs a connection and buys \
         nothing"
    );
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 10_000,
        max_shrink_iters: 100_000,
        ..ProptestConfig::default()
    })]

    #[test]
    fn every_decision_matches_the_semantic_oracle(
        operations in proptest::collection::vec(arb_operation(), 0..100)
    ) {
        let mut allocator = SegmentAllocator::new(TOTAL, MIN_SPLIT)
            .expect("the fixed test configuration is valid");
        assert_partition(allocator.intervals())?;
        for operation in operations {
            apply(&mut allocator, operation)?;
        }
    }
}
