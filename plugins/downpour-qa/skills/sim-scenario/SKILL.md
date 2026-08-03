---
name: sim-scenario
description: Write a deterministic simulation scenario for Downpour — adversarial timing, ordering, crash injection, or network conditions, reproducible from a seed. Use for anything involving concurrency, crash safety, or the concurrency controller's behaviour over time.
when_to_use: Testing crash recovery; testing worker scheduling or work stealing; testing the concurrency controller; a bug that only appears under load or timing pressure.
argument-hint: "[the timing or failure behaviour to test]"
allowed-tools: Read, Write, Edit, Grep, Glob, Bash
---

# Write a simulation scenario

The corpus tests *what* the engine does. Simulation tests what happens when timing is
adversarial — the failures that appear once a week in production and never in CI.

Read `docs/09-testing-strategy.md` §4 first.

## Why this works at all

The `TransferProtocol` trait (ADR-0005) lets the simulator substitute the entire network layer,
and the clock is injected. Everything derives from a seed, so **a failing seed reproduces
exactly, on any machine, forever**. "Intermittent" is not a category this project accepts.

## Anatomy

```rust
#[test]
fn <scenario_name>() {
    for seed in FIXED_SEEDS {           // plus a random range nightly
        let sim = Simulation::new(seed)
            .with_content(Content::deterministic(500 * MB))
            .with_network(Network::lossy(0.02).rtt_jitter(10..200))
            .with_server(Server::per_ip_cap(50 * MBPS))
            .with_crash_policy(CrashPolicy::AtEveryWriteBoundary);

        let outcome = sim.run_download_to_completion_with_restarts();

        assert!(outcome.file_matches_expected_content(), "seed {seed}");
        assert_eq!(outcome.silent_corruptions, 0, "seed {seed}");
        assert!(outcome.bytes_refetched < 0.05 * outcome.total, "seed {seed}");
    }
}
```

**Every assertion message includes the seed.** A failure that does not tell you which seed to
re-run has thrown away the main benefit of the whole approach.

## Adversity to reach for

| Dimension | Options |
| --------- | ------- |
| Crash | at every write boundary, at fsync, at journal append, at rename, random |
| Network | loss, RTT jitter, bandwidth variation, disconnect and restore, one slow path |
| Server | per-IP cap, per-connection cap, `429` storm, silent drop, close-after-N-bytes |
| Scheduling | randomised task ordering, one worker starved, workers dying at random |
| Disk | slow flush, full at a given percentage, transient write errors |
| Clock | jump forward, jump backward, freeze |

## Rules

- **Assert the invariant, not the outcome.** `file_matches_expected_content()` is the assertion.
  "It completed" is not — it can complete and be wrong, which is the whole problem.
- Always assert `silent_corruptions == 0`.
- Assert on wasted work too (`bytes_refetched`). A scheduler that is correct but re-downloads
  40% of the file is a bug the correctness assertion will not catch.
- **A failing seed found by the nightly random run is added to `FIXED_SEEDS` permanently**, with
  a comment naming the bug it found. Over time that array becomes a record of every timing bug
  the project has ever had.
