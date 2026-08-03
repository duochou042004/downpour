---
name: release-check
description: Walk the Downpour release checklist and report honestly what is ready and what is not, including the scorecard with evidence. Use before tagging any release.
when_to_use: Preparing a release; asked whether the project is ready to ship; scoring the project against the 9.5/10 target.
argument-hint: "[version being prepared]"
allowed-tools: Read, Bash, Grep, Glob
---

# Release check

Level 3 of `docs/agent/DEFINITION-OF-DONE.md`, operationalised. Read
`docs/11-packaging-release.md` §7 and `docs/00-vision-and-scorecard.md`.

## The blocker

**Any silent-corruption finding blocks the release outright**, regardless of every other
result. It does not average out against the other 75 points. Check this first and state it
first:

```bash
jq '.metrics.silent_corruption_findings, .scorecard.blocking_findings' state/progress.json
```

If either is non-zero, stop. Report the blocker and do not continue scoring — a scorecard
that follows an unaddressed corruption finding invites someone to weigh it against the total.

## Score the scorecard

For each of the six rows, produce the score **and the evidence**:

| Row | Weight | Evidence must be |
| --- | -----: | ---------------- |
| 1 Correctness | 25 | Corpus + simulation results, corruption count |
| 2 Resume and recovery | 20 | Crash matrix results per platform |
| 3 Speed | 20 | Benchmark table vs IDM/AB/aria2/browser |
| 4 Compatibility | 15 | Corpus pass rate with failures enumerated |
| 5 Browser and media | 15 | Manual browser matrix + media assembly verification |
| 6 Resources, UX, packaging | 5 | RSS/CPU measurements + clean-machine installs |

**A row with no evidence scores zero, not "assumed fine".** Partially working scores the
fraction that works.

## Then the checklist

Walk `docs/11-packaging-release.md` §7 line by line. For each: done with evidence, or not done.

## Output

```markdown
## Release check — v0.9.0

**Blocker check:** 0 silent-corruption findings ✓

| Row | Weight | Score | Weighted | Evidence |
| --- | -----: | ----: | -------: | -------- |
| 1 Correctness | 25 | 100% | 25.0 | 159/159 corpus, 50k sim seeds, 0 corruption |
| 2 Resume | 20 | 95% | 19.0 | crash-matrix green; Windows slow-disk case untested |
| … |
| **Total** | | | **92.5** | |

**Target is 95. Not ready.**

**Gaps:**
1. Row 2: Windows slow-flush device untested (−1.0)
2. Row 5: Firefox capture broken on 3 of 20 manual sites (−4.5)
3. Row 6: no rpm build (−1.0)
```

## Rule

Report the gaps. A release check that concludes "ready" when it is not has not saved anyone
time — it has moved the discovery to the users, which is the most expensive place for it to
happen. Say plainly whether the target is met.
