# Definition of Done

Three levels. A thing is done at its level only when **every** box is true. There is no
partial credit and no "done except".

---

## Level 1 — A task is done

- [ ] The behaviour exists and a test demonstrates it.
- [ ] That test **fails** when the implementation is reverted. (Actually check this. A test
      that passes against an empty implementation is worse than no test.)
- [ ] The test is at the right layer (`09-testing-strategy.md`): property test for data
      structures, corpus case for server behaviour, simulation for timing and crash safety.
- [ ] `cargo fmt --all -- --check` passes.
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` passes with no new allows.
- [ ] `cargo nextest run --workspace` passes.
- [ ] No `unwrap`, `expect`, or `panic!` added to engine or daemon paths.
- [ ] Any new `unsafe` has a `// SAFETY:` comment and, if non-trivial, an ADR.
- [ ] Errors are handled, not swallowed. No `let _ = fallible()`.
- [ ] Public items have doc comments; new modules have a `//!` header naming the invariant
      they own.
- [ ] `/invariant-check` walked, with each invariant marked "not affected" or covered.
- [ ] `state/progress.json` updated: task status, `proof` field naming the real test, and a
      `session_log` entry.
- [ ] `node scripts/check-progress.mjs` passes.
- [ ] Anything discovered but not done is in `backlog`, not in the diff.

## Level 2 — A stage is done

- [ ] Every task in the stage is Level-1 done.
- [ ] Every `exit_criteria` entry has named, reproducible evidence in the `/stage-gate` table.
- [ ] No criterion is partially met. Partial is not met.
- [ ] The full corpus passes: `just corpus`.
- [ ] The simulation suite passes across its seed set: `just sim`.
- [ ] Crash injection passes at every write boundary introduced by this stage.
- [ ] No performance regression against the recorded baseline, or the regression is recorded
      and accepted in writing with a reason.
- [ ] Every ADR the stage needed is written and accepted — not "proposed", not implied by code.
- [ ] `CLAUDE.md` and `AGENTS.md` still agree with each other and with reality.
- [ ] Specs updated where the implementation taught us the spec was wrong.
- [ ] `14-tech-radar-2026.md` reflects every dependency actually in `Cargo.toml`.
- [ ] The maintainer has reviewed the gate table and approved advancement.

## Level 3 — A release is done

- [ ] The scorecard in `00-vision-and-scorecard.md` is scored, with evidence per row.
- [ ] **Zero** silent-corruption findings across the entire corpus and simulation suite.
      This is not a threshold to negotiate — it is the release blocker.
- [ ] Crash recovery verified on both Linux and Windows, on both an SSD and a spinning disk
      or an equivalent slow-flush device.
- [ ] Resume verified across: process kill, OS reboot, network loss, disk full, and a
      changed remote representation (which must refuse, not splice — I-3).
- [ ] Comparative benchmark against IDM, AB Download Manager, aria2, and browser-native
      download, on the local test matrix, with results published in the release notes.
      Losing a case is acceptable; not knowing you lost it is not.
- [ ] Browser capture verified on current Chrome, Edge, and Firefox.
- [ ] Memory and CPU measured under a 20-download queue and recorded.
- [ ] Installers built and verified on a clean machine for each supported target.
- [ ] Update mechanism tested from the previous release, including a downgrade attempt that
      is correctly refused (I-11).
- [ ] `SECURITY.md` process live; dependency audit clean (`cargo deny`, `cargo audit`).
- [ ] Licence and attribution complete for every bundled dependency.
- [ ] Known limitations documented honestly in the release notes — including what Downpour
      does *not* do (DRM, protected streams, sites requiring bespoke handling).

---

## The honesty clause

If a box is not ticked, say so plainly and say why. An agent that reports a stage complete
with an unmet criterion has not saved anyone time — it has moved the discovery of the gap to
a point where more work is built on top of it, which is the single most expensive mistake
available in this project.

"Nine of eleven criteria met; `crash-matrix` fails on seeds 7 and 41; here is the output" is a
good report. "Stage complete ✅" when it is not is a failure of the job, not a rounding error.
