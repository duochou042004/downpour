# ADR-0018: Where fault injection lives

- **Status:** accepted
- **Date:** 2026-08-05
- **Stage:** S2
- **Deciders:** maintainer (delegated), proposed and revised by Claude Code

> **Revised before acceptance, 2026-08-05.** The first draft decided two things: where fault
> injection belongs, and that the `local` corpus category's target should drop from 12 to 10. The
> first was right and is kept unchanged. The second was wrong, and the reasoning that killed it is
> recorded under *Options* below, because the mistake is instructive: it inferred that a category
> could not be filled from the fact that two *named examples* could not be. See the reversal
> trigger for what would make the target question worth reopening.

## Context

`docs/09-testing-strategy.md` §3.2 sets an initial target of 12 cases for the `local` category:
*disk full, permissions, path limits, collisions, network filesystems.* S2-T15 built ten of them.
Two of the five named classes — disk full mid-transfer, and network filesystems — are not
expressible by the corpus, and the reason is structural rather than an absence of effort.

The corpus runs the **real** engine against a **real** filesystem. That is the property that
makes it valuable: a case exercises `SingleStream`, `StorageSink`, `PartFile` and the journal
exactly as a user's download does, so a passing case is evidence about the shipped code rather
than about a mock. ADR-0007 makes that the corpus's whole reason to exist.

That same property is what puts these two classes out of reach:

**Disk full mid-transfer.** Making a real volume fill at a chosen byte needs either a filesystem
the test controls — a loopback image or a small tmpfs, which is not portable and needs privileges
where it is possible at all — or a fault-injection hook inside `StorageSink`. The second is the
tempting one and it is the one to refuse: a production write path carrying a branch that exists
only for tests is a branch that can be wrong in production, and the durability path is precisely
where a stray conditional is unrecoverable.

**Network filesystems.** An NFS or SMB mount cannot be created in-process. Worse, the pathologies
that actually matter — silent write reordering, a server acknowledging an `fsync` it did not
perform, a lock that does not hold across clients — are exactly the ones that do *not* reproduce
on a local mount. A case that mounted something locally and called it covered would be a green
result for a check that never ran, which is the failure mode `docs/09` §7 exists to prevent.

`tests/sim` already covers disk-full at the storage layer, where injection is legitimate:
`DurableData` and `DurableJournal` are traits the writer was designed around, so a bounded volume
is a normal implementation of an existing seam rather than a hole cut in the write path.
`disk_full_at_97pct_preserves_durable_coverage_without_truncation` runs there today.

## Options

| Option | Pros | Cons |
| ------ | ---- | ---- |
| **Decide only the injection boundary; leave `local`'s target at 12 and fill it with local pathologies the corpus *can* express** | Answers the question that is actually irreversible — where a test-only branch may live — without spending the corpus to do it. docs/09's totals need no note | Requires finding two more genuinely distinct local pathologies rather than declaring the category closed. That search is work, and it is the work the category is for |
| Also lower `local`'s target to 12 → 10 *(the first draft's decision, rejected on revision)* | The inventory closes today | Infers "the category cannot be filled" from "two named examples cannot be". §3.2's row is an illustrative list, not an enumeration — and the search it skipped immediately produced two cases, one of which found a live bug. It also spends the one asset the project's thesis rests on to make a number go green |
| Keep 12 and add a fault-injection hook to `StorageSink` | The corpus number is met exactly as written | Puts test-only branching in the production durability path. A conditional that is wrong in production is unrecoverable there, and the corpus would be buying a number with the risk it exists to eliminate |
| Keep 12 and use a loopback filesystem in CI | Genuinely end to end | Needs privileges CI does not have on the Windows runner, makes the Linux and Windows suites structurally different, and a case that only runs on one platform is a case that stops running the day that runner changes |

## Decision

**Fault injection belongs at a seam the production code already has for its own reasons, never at
a hole cut for a test.** `DurableData` and `DurableJournal` exist because the writer needs to be
testable at all; a bounded volume implementing them costs the production path nothing. A hook
inside `StorageSink` would cost it a branch.

Consequently, disk-full mid-transfer is owned by `tests/sim` and network-filesystem behaviour by
manual qualification before a release:

| Class | Owner | Why not the corpus |
| ----- | ----- | ------------------ |
| Disk full mid-transfer | `tests/sim`, `disk_full.rs` | Needs a controllable volume or a production hook; the storage seam already exists |
| Network filesystems | Manual qualification, recorded per release | Cannot be created in-process, and the pathologies that matter do not reproduce locally |

**`local`'s target stays at 12, and docs/09 §3.2 is unchanged.** Relocating two classes does not
shrink the category: §3.2's row names five *examples* of local pathology, not the complete set,
and a local pathology is anything that is a property of the filesystem rather than of the server.
The two cases that close the category are both of that kind and neither needs a controlled volume:

- `local/a-dangling-symlink-occupies-the-target` — a symlink whose destination does not exist.
  Every existence check that follows links reports the path as free; `rename` then replaces the
  link itself. **This case found a live bug**: `SingleStream::download` used `try_exists`, so the
  engine completed the download and destroyed the user's symlink without reporting anything.
  Fixed by checking with `symlink_metadata`, which does not follow.
- `local/a-symlink-occupies-the-part-file-path` — a symlink where the `.dppart` goes, pointing
  outside the download directory. Passed on arrival, because `create_new` is `O_CREAT | O_EXCL`
  and POSIX requires that to refuse a symlink rather than follow it. Mutating it to `create`
  writes the entire download over the destination, which the case detects by comparing the
  destination's bytes.

This ADR does not weaken any invariant. I-10's guarantees are tested; only the layer that tests
them moves, and it moves to the layer that can actually observe them.

## Consequences

**Easier:** the injection boundary is now a rule rather than a judgement call, so the next time a
durability test is hard to write, the question is "which existing seam?" rather than "how small a
hook?". The distinction between "the corpus proves this" and "something else proves this" is
explicit per class rather than implied by a total.

**Harder:** network-filesystem behaviour is a release checklist item rather than something CI
enforces, which is weaker — and honestly weaker, rather than appearing covered while nothing runs.
Filling a category whose obvious examples are exhausted requires looking for pathologies of the
same *kind* rather than the same name, which is slower than lowering the number.

**Accepted:** Downpour ships without automated evidence about NFS or SMB. That is the real state
today either way; this only stops the inventory from implying otherwise.

## Reversal trigger

Revisit the **injection boundary** if the corpus gains a way to run against a controlled
filesystem on both Linux and Windows without privileges — a userspace filesystem the test drives,
or a CI runner that can attach a scratch volume. That would let disk-full return to the corpus
without a production hook, and this ADR should be superseded rather than stretched.

Revisit **`local`'s target** only in the direction of raising it. Lowering a corpus target is
spending the one asset the project's thesis rests on — the README's claim is that IDM's moat
cannot be out-coded, only out-tested — so a target that is hard to meet is a backlog item, not a
number to adjust. If a future category genuinely cannot be filled, the honest move is to record
the shortfall against the original number, which the inventory guard already prints on every run.

Reopen immediately if a user reports corruption on a network filesystem: that would make the class
urgent enough to justify a qualification harness of its own.
