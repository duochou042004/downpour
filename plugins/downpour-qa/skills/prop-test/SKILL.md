---
name: prop-test
description: Write a property test for a Downpour data structure — assert the algebraic invariants over generated operation sequences rather than checking hand-picked examples. Use for the interval map, the journal, parsers, and filename sanitisation.
when_to_use: Working on downpour-intervals or the journal; writing a parser; any pure data structure with invariants that must always hold.
argument-hint: "[the structure or invariant]"
allowed-tools: Read, Write, Edit, Grep, Glob, Bash
---

# Write a property test

Example tests check the cases you thought of. The bugs are in the cases you did not. For the
structures that carry Downpour's invariants, assert the **laws**.

Read `docs/09-testing-strategy.md` §2.

## Find the law first

Before writing code, state the property in one sentence.

| Structure | Laws |
| --------- | ---- |
| Interval map | Intervals are always disjoint (I-2). Their union is always exactly `[0, total)`. No zero-length interval exists. Adjacent same-state intervals are merged. |
| Interval map (algebraic) | Any interleaving of the same operation multiset yields the same coverage. Split-then-complete-both equals complete-whole. |
| Journal | Replay of any prefix, truncation, or bit-flip yields a prefix-consistent state and never panics (I-9). |
| Filename sanitiser | Output never escapes the target directory. Output is always a valid name on the platform. Sanitising twice equals sanitising once (idempotent). |
| `Content-Range` parser | Never accepts a range inconsistent with what was requested. Never panics. |
| Concurrency controller | Never exceeds the ceiling. Always able to reach 1. Monotone in the back-off signal. |

## Shape

```rust
proptest! {
    #![proptest_config(ProptestConfig { cases: 10_000, ..Default::default() })]

    #[test]
    fn intervals_stay_disjoint_and_total(ops in arb_operation_sequence()) {
        let mut map = IntervalMap::new(TOTAL);
        for op in ops {
            map.apply(op);
            prop_assert!(map.all_disjoint());
            prop_assert_eq!(map.union_len(), TOTAL);
            prop_assert!(map.no_zero_length());
        }
    }
}
```

## Getting the generator right

The generator is where property tests succeed or fail. A generator that only produces
well-formed input tests nothing interesting.

- Generate **sequences of operations**, not single values. The bugs are in the interactions.
- Include the operations that should not happen: completing an interval twice, abandoning a
  grant that was never issued, splitting at a boundary, splitting at offset 0 and at `total`.
- For parsers, generate malformed input deliberately: truncated, oversized, wrong types, deeply
  nested, integer values at and past the type boundary.
- Bias toward small values. Bugs cluster near 0, 1, and the boundary — not at 2^31.

## Rules

- Assert **inside** the loop, after every operation. Asserting only at the end tells you the
  final state was fine and hides which operation broke the intermediate one.
- Keep the property pure. If the test needs I/O, it is not a property test — it is an
  integration test wearing the wrong hat.
- When a property test finds a failure, **keep the minimised case as a named regression test**.
  `proptest` shrinks to a minimal counterexample; that counterexample is worth preserving.
