# ADR-0011: CI on GitHub Actions; GitLab kept as a mirror with CI disabled

- **Status:** accepted
- **Date:** 2026-08-03
- **Stage:** S1
- **Deciders:** maintainer (proposed by claude-code)

## Context

GitLab's free tier gives 400 CI minutes a month. We exhausted all 400 during Stage 1 and every
pipeline now fails with `failure_reason: ci_quota_exceeded` before any job starts. CI is therefore
unavailable on the current host, which means the project has no mechanical gate — and the whole
point of `docs/agent/HARNESS.md` is that gates are mechanical rather than remembered.

The minutes did not disappear because the project is large. It is five crates and roughly 200
transitive dependencies. They disappeared because the pipeline was written without measuring it.
Job durations from the last fully green pipeline (`4924ac5`, pipeline 2727373674):

| Job | Duration | Stage |
| --- | -------: | ----- |
| `corpus` | 633.6 s | test |
| `unit` | 440.3 s | test |
| `audit` | 370.0 s | security |
| `clippy` | 136.0 s | quality |
| `fmt` | 72.4 s | quality |
| `progress`, `agent-assets`, `docs-links`, `shell-lint`, `secret-scan` | ~54 s combined | — |
| **Total billed** | **1706 s = 28.4 min** | |

At 28.4 minutes per pipeline, 400 minutes buys **14 pipelines**. Stage 1 used about twelve.

Comparing the same jobs across three consecutive pipelines on near-identical code shows the cache
was not working at all — durations never converged toward a warm-cache figure, and `clippy` got
*slower* (98 s → 116 s → 136 s):

| Job | pipeline 1 | pipeline 2 | pipeline 3 |
| --- | ---------: | ---------: | ---------: |
| `unit` | 671 s | 599 s | 440 s |
| `corpus` | 639 s | 432 s | 634 s |
| `audit` | 511 s | 501 s | 370 s |

Three causes, all verifiable in `.gitlab-ci.yml` as it stood:

1. **Tools were installed by compiling them, repeatedly.** `cargo install` appeared six times
   (`cargo-nextest` ×4, `cargo-deny`, `cargo-audit`). `cargo-audit` alone pulls several hundred
   crates including `aws-lc-sys` (C/C++ and cmake) and `gix`. Worse, `~/.cargo/bin` was **not** in
   `cache.paths`, so the compiled binaries were discarded at the end of every job and rebuilt in
   the next one.
2. **The workspace was compiled three to four times per pipeline.** `fmt`, `clippy`, `unit` and
   `corpus` were separate jobs, so each got a fresh runner and its own compile. And `unit` ran
   `cargo nextest run --workspace`, which already includes `downpour-corpus`, so the `corpus` job
   re-ran tests that had just passed — 633 s of pure duplication.
3. **Cache keys collided.** `fmt` and `clippy` are in the same stage and shared one cache key, so
   they raced and the last writer won.

## Options

### Where CI runs

| Option | Pros | Cons |
| ------ | ---- | ---- |
| **A. GitHub Actions, GitLab as a push mirror with CI off** | Standard GitHub-hosted runners are **free and unmetered for public repositories** — no minute cap to exhaust, which removes this failure mode rather than postponing it. The Rust action ecosystem is significantly better: `Swatinem/rust-cache` and `taiki-e/install-action` have no GitLab equivalents and are what make the optimisations below cheap. `gh` is already installed and authenticated. | Two hosts to keep in sync. The canonical-host question has to be answered explicitly or it rots. Requires the repository to be public — acceptable and already intended (Apache-2.0, ADR-0006). |
| B. Buy GitLab minutes | No migration; keeps one host. | Pays to keep a pipeline whose real problem is that it wastes 93% of its compute. Buying capacity to cover waste is the wrong order of operations, and the waste would still be there at the next quota. |
| C. Self-hosted runner on the dev machine | Unmetered on either host; fastest possible warm builds. | The dev machine becomes infrastructure. It must be online for CI to work, and a green build then means "green on the one machine that also produced it", which is exactly the property CI exists to avoid. Worth revisiting for the S4 benchmark suite, where a controlled machine is a feature rather than a bug. |
| D. Optimise GitLab and stay | No migration. | Optimisation is necessary in every option, but on GitLab it only buys more pipelines per 400 minutes — the cap remains, and a bad month still stops the project. |

Optimisation is not an alternative to migrating; it is required either way, and this ADR does both.

### Whether to add `sccache`

| Option | Pros | Cons |
| ------ | ---- | ---- |
| **A. No `sccache`; rely on `rust-cache` for `target/`** | `rust-cache` already caches `~/.cargo` and `target/` and is documented to cut subsequent builds by 50–80%. One mechanism, one cache budget, no wrapper in the compile path. | Leaves object-level reuse across differing cache keys on the table. |
| B. Add `sccache` alongside | Object-level caching, shareable across jobs and keys; reported large wins on some projects. | It competes for the **same 10 GB GitHub cache budget** as `rust-cache`, and its cache is documented to grow quickly, which upstream calls "less than ideal for hosted runners". Adding a compiler wrapper is also a change to the build path itself, and this project's rule is that correctness outranks speed. Two caching mechanisms fighting over one budget is a plausible way to make CI *slower* and much harder to reason about. |

## Decision

**CI moves to GitHub Actions. GitHub becomes the CI host; GitLab remains a push mirror with its
pipeline disabled** by renaming `.gitlab-ci.yml` to `.gitlab-ci.yml.disabled`, which is sufficient
because GitLab runs nothing without that file at the default path. The GitLab configuration is
**optimised anyway, not abandoned**, so that re-enabling it is a rename rather than a rewrite — and
so that the fixes are recorded where the next person looks.

The pipeline is restructured around the measurements above:

1. **One Rust job, not four.** `fmt`, `clippy`, `nextest --workspace` and `cargo test --doc` run in
   a single job so the workspace is compiled once and one cache is restored once. The redundant
   `corpus` job is deleted; `--workspace` already covers it.
2. **Tools arrive as prebuilt binaries.** `taiki-e/install-action` downloads `cargo-nextest`,
   `cargo-deny` and `cargo-audit` from GitHub Releases instead of compiling them. This is the single
   largest line item: `audit` alone was 370–511 s of compiling a tool.
3. **Real caching.** `Swatinem/rust-cache@v2`, which keys on the toolchain version and the Cargo
   files, caches `~/.cargo` **including `bin/`** — the entry whose absence made every `cargo install`
   recompile — prunes stale artifacts, and refuses to save a broken build. It saves on **every**
   branch: see the correction under Measured outcome, where restricting saves to the main branches
   turned out to leave every feature-branch run cold.
4. **`--locked` everywhere**, so CI can never silently resolve a different dependency graph than the
   committed `Cargo.lock`. This is a correctness property, not a speed one.
5. **`CARGO_INCREMENTAL=0` and `CARGO_PROFILE_TEST_DEBUG=0`.** Incremental compilation is a
   local-development optimisation that costs time and cache size in CI; test debug info inflates
   `target/` and therefore every cache push and pull.
6. **`concurrency` with `cancel-in-progress`.** Superseded pushes stop immediately. This worked on
   GitLab via `interruptible: true` and is the reason several Stage 1 pipelines were cancelled
   rather than wasted.
7. **The harness jobs stay separate and Rust-free.** `check-progress.mjs`, the agent-asset sync
   check, the link checker, shellcheck and the secret scan need no toolchain and finish in seconds.
   Keeping them out of the Rust job means a progress-file typo fails in ten seconds instead of ten
   minutes.
8. **The 1 GB corpus cases stay out of the per-push path.** They are `#[ignore]`d and run by
   `just corpus-slow` on a schedule and on demand, per `docs/09-testing-strategy.md` §6's
   five-minute budget for every-push work.

**No `sccache` for now**, for the reasons in the options table.

## Consequences

**Easier:** CI stops being a scarce resource, so the harness's mechanical gates actually run on
every push — which is the property Stage 0 was built to guarantee. A progress-file error fails in
seconds. Adding a tool to CI costs a line rather than six minutes of compiling.

**Harder:** two hosts. The canonical answer is recorded here to stop it rotting: **GitHub is where
CI runs and where merge requests are gated; GitLab is a mirror.** `README.md`, `CONTRIBUTING.md` and
`docs/11-packaging-release.md` are updated in the same change, and branch protection has to be
configured on GitHub as it was on GitLab, or `master` loses the protection ADR-0003's release story
assumes.

**Accepted:** a dependency on third-party actions in the trust path (`Swatinem/rust-cache`,
`taiki-e/install-action`). They are pinned to major versions rather than commit SHAs, which is the
common trade-off; for a project that ships binaries, pinning to SHAs is the stricter choice and is
recorded as a reversal trigger below rather than done now. Also accepted: the public repository is
now required for free CI, which was already the intent but is now load-bearing.

## Measured outcome

Taken after the change, on the same workspace, so the before-and-after is comparable.

| | Total | Coverage |
| --- | ----: | ------- |
| GitLab, last green pipeline | 1706 s (28.4 min) | Linux only |
| GitHub Actions, cold cache, Windows job included | 412 s (6.9 min) | Linux + Windows |
| **GitHub Actions, warm cache** | **196 s (3.3 min)** | Linux + Windows |

Per job, cold → warm: `rust` 112 s → 38 s, `windows` 264 s → 128 s, `security` 23–28 s (against
370–511 s on GitLab, which compiled `cargo-audit` every run), `harness` 5–8 s (against ~35 s spread
over three jobs). Against GitLab that is **88% less compute for more coverage** — and the Windows
job is coverage the project never had, which is how it went eleven months without anyone noticing
nothing had ever been compiled for Windows.

Runs: `30826183828` (cold), `30826537689` (cold, Windows added), `30827248189` (warm).

One correction the measurement forced: the first version restricted `save-if` to `master` and
`develop`, following advice written for repositories with many concurrent branches competing for the
10 GB budget. The cold run showed `rust-cache`'s save step taking 0.0 s on a feature branch — which
is where a single maintainer does nearly all their work, so every run would have stayed cold
forever. Removed.

## Reversal trigger

1. **A supply-chain incident in any pinned action, or a project decision to ship signed release
   artifacts from CI.** Either makes major-version pinning insufficient: move to full commit-SHA
   pins for every third-party action, and review whether release jobs should run on a self-hosted
   runner instead. This is the most likely of the three to fire.
2. **The warm Rust job exceeds five minutes**, or the cold job exceeds fifteen. That means the
   measures above have stopped being enough — most likely because the dependency graph grew past
   what a 10 GB cache holds. Response, in order: split the Rust job by crate so caches are smaller
   and more specific; then reconsider `sccache` with a measured before-and-after; then a self-hosted
   or larger runner. Do not reach for `sccache` before measuring, because it competes for the same
   cache budget.
3. **GitHub changes public-repository billing**, or the project becomes private. Then the free-and
   unmetered premise is gone and this reduces to option C, a self-hosted runner — which the S4
   benchmark work may want regardless, since a controlled machine is a requirement there.
