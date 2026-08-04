//! Property proofs for I-2: the interval map remains a canonical partition after every
//! attempted operation, including invalid and repeated operations.

use std::ops::Range;

use downpour_intervals::{IntervalMap, IntervalState, WorkerId};
use proptest::prelude::*;
use proptest::test_runner::TestCaseResult;

const TOTAL: u64 = 64;

#[derive(Clone, Debug)]
enum Operation {
    Grant {
        range: Range<u64>,
        worker: WorkerId,
    },
    Split {
        at: u64,
        source: WorkerId,
        recipient: WorkerId,
    },
    Complete {
        range: Range<u64>,
        worker: WorkerId,
    },
    Abandon {
        worker: WorkerId,
    },
}

fn arb_worker() -> impl Strategy<Value = WorkerId> {
    (0_u64..6).prop_map(WorkerId::new)
}

fn arb_range() -> impl Strategy<Value = Range<u64>> {
    (0_u64..=TOTAL, 0_u64..=TOTAL).prop_map(|(start, end)| start..end)
}

fn arb_operation() -> impl Strategy<Value = Operation> {
    prop_oneof![
        (arb_range(), arb_worker()).prop_map(|(range, worker)| Operation::Grant { range, worker }),
        (0_u64..=TOTAL, arb_worker(), arb_worker()).prop_map(|(at, source, recipient)| {
            Operation::Split {
                at,
                source,
                recipient,
            }
        }),
        (arb_range(), arb_worker())
            .prop_map(|(range, worker)| Operation::Complete { range, worker }),
        arb_worker().prop_map(|worker| Operation::Abandon { worker }),
    ]
}

fn arb_operation_sequence() -> impl Strategy<Value = Vec<Operation>> {
    proptest::collection::vec(arb_operation(), 0..80)
}

fn apply(map: &mut IntervalMap, operation: Operation) -> TestCaseResult {
    let before = map.clone();
    match operation {
        Operation::Grant { range, worker } => {
            if map.grant(range, worker).is_err() {
                prop_assert_eq!(map, &before, "a rejected grant mutated the map");
            }
        }
        Operation::Split {
            at,
            source,
            recipient,
        } => {
            if map.split_in_progress(at, source, recipient).is_err() {
                prop_assert_eq!(map, &before, "a rejected split mutated the map");
            }
        }
        Operation::Complete { range, worker } => {
            if map.complete(range, worker).is_err() {
                prop_assert_eq!(map, &before, "a rejected completion mutated the map");
            }
        }
        Operation::Abandon { worker } => {
            map.abandon(worker);
        }
    }
    Ok(())
}

fn assert_laws(map: &IntervalMap, total: u64) -> TestCaseResult {
    let intervals = map.intervals();
    if total == 0 {
        prop_assert!(intervals.is_empty());
        return Ok(());
    }

    prop_assert!(!intervals.is_empty());
    let mut cursor = 0_u64;
    let mut previous_state: Option<&IntervalState> = None;

    for interval in intervals {
        prop_assert_eq!(
            interval.start(),
            cursor,
            "gap or overlap before {:?}",
            interval
        );
        prop_assert!(
            interval.start() < interval.end(),
            "zero-length {:?}",
            interval
        );
        if let Some(previous) = previous_state {
            prop_assert_ne!(
                previous,
                interval.state(),
                "adjacent equal states were not merged"
            );
        }
        cursor = interval.end();
        previous_state = Some(interval.state());
    }

    prop_assert_eq!(cursor, total, "partition does not cover [0, total)");
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 10_000,
        max_shrink_iters: 100_000,
        ..ProptestConfig::default()
    })]

    /// S2-C3's load-bearing proof: arbitrary operation sequences preserve every map law after
    /// every operation, not merely in the final state.
    #[test]
    fn intervals_stay_disjoint_and_total(operations in arb_operation_sequence()) {
        let mut map = IntervalMap::new(TOTAL);
        assert_laws(&map, TOTAL)?;

        for operation in operations {
            apply(&mut map, operation)?;
            assert_laws(&map, TOTAL)?;
        }
    }
}

proptest! {
    /// Splitting an active grant and durably completing both halves has the same canonical
    /// result as completing the original whole grant.
    #[test]
    fn completing_both_split_halves_equals_completing_the_whole(split_at in 1_u64..TOTAL) {
        let first = WorkerId::new(1);
        let second = WorkerId::new(2);

        let mut whole = IntervalMap::new(TOTAL);
        whole.grant(0..TOTAL, first).expect("the whole map is pending");
        whole
            .complete(0..TOTAL, first)
            .expect("the whole grant belongs to the worker");

        let mut split = IntervalMap::new(TOTAL);
        split.grant(0..TOTAL, first).expect("the whole map is pending");
        split
            .split_in_progress(split_at, first, second)
            .expect("the split point is strictly inside the active grant");
        split
            .complete(0..split_at, first)
            .expect("the left half belongs to the original worker");
        split
            .complete(split_at..TOTAL, second)
            .expect("the right half belongs to the recipient");

        prop_assert_eq!(split.intervals(), whole.intervals());
        assert_laws(&split, TOTAL)?;
    }

    /// Independent completions commute: task scheduling order cannot alter durable coverage.
    #[test]
    fn disjoint_completions_are_order_independent(split_at in 1_u64..TOTAL) {
        let first = WorkerId::new(1);
        let second = WorkerId::new(2);

        let mut left_then_right = IntervalMap::new(TOTAL);
        left_then_right
            .grant(0..split_at, first)
            .expect("left range is pending");
        left_then_right
            .grant(split_at..TOTAL, second)
            .expect("right range is pending");

        let mut right_then_left = left_then_right.clone();
        left_then_right
            .complete(0..split_at, first)
            .expect("left grant belongs to first worker");
        left_then_right
            .complete(split_at..TOTAL, second)
            .expect("right grant belongs to second worker");

        right_then_left
            .complete(split_at..TOTAL, second)
            .expect("right grant belongs to second worker");
        right_then_left
            .complete(0..split_at, first)
            .expect("left grant belongs to first worker");

        prop_assert_eq!(left_then_right.intervals(), right_then_left.intervals());
        assert_laws(&left_then_right, TOTAL)?;
    }
}

#[test]
fn zero_length_files_have_an_empty_total_partition() {
    let map = IntervalMap::new(0);
    assert!(map.intervals().is_empty());
}
