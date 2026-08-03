---
name: bench-compare
description: Run and interpret the Downpour comparative benchmark against IDM, AB Download Manager, aria2 and browser-native download under shaped network conditions. Use at stage gates, before a release, and whenever a performance claim needs evidence.
when_to_use: A stage gate needs speed evidence; someone claims a change made things faster; preparing release notes; investigating a report that Downpour is slower than an alternative.
argument-hint: "[optional: condition or competitor to focus on]"
allowed-tools: Read, Write, Bash, Grep
---

# Comparative benchmark

We claim 9.5/10 against IDM (`docs/00-vision-and-scorecard.md`). That claim needs measurement.
Losing a case is acceptable; **not knowing you lost it is not**.

Read `docs/09-testing-strategy.md` §5.

## Setup

Same file, same server, same shaped conditions, run against Downpour, IDM (Windows only),
AB Download Manager, `aria2c`, and browser-native download.

Conditions, shaped with `tc netem`:

| Condition | Shape |
| --------- | ----- |
| fast-low-latency | 1 Gbps, 5 ms |
| fast-high-latency | 1 Gbps, 200 ms |
| slow-lossy | 10 Mbps, 100 ms, 2% loss |
| mobile-like | 50 Mbps, 60 ms, variable |
| per-connection-capped | 5 Mbps per connection |
| per-ip-capped | 50 Mbps total |
| cdn-like | multiple edges, varying rates |

## Metrics

| Metric | What it tells you |
| ------ | ----------------- |
| Time to first byte | Probe overhead |
| Time to 90% of link | How fast the controller finds the right concurrency |
| Mean throughput | The headline |
| Tail time (last 5%) | Where naive schedulers lose — ETA splitting should show here |
| Handshake count | Efficiency; matters most on high-RTT links |
| Bytes re-fetched | Waste from retries and redundant fetch |
| Peak RSS, mean CPU | Scorecard row 6 |
| Resume success rate | Correctness under interruption |
| **Silent corruption count** | **Must be zero for every tool, every run** |

## Interpretation

- **A tie on a saturated link is a tie, not a loss.** If both tools reach line rate, there is
  nothing to win. Do not report that as a deficit.
- **Report where we lose.** A benchmark that only shows wins is marketing. The release notes
  carry the losses too.
- **Check the tail specifically.** Mean throughput can look identical while the last 5% takes
  twice as long, and the tail is what the user actually experiences as "it hung at 97%".
- **Watch for the self-inflicted case:** if Downpour is slower with more workers than with one,
  the controller has a bug and this is a correctness finding (G6), not a performance note.

## Output

A table of tool × condition × metric, with the raw data preserved so someone else can re-run
it. Then: what we won, what we lost, and what the loss is attributable to.

A benchmark you cannot reproduce is not evidence.
