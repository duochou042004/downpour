# ADR-0018: Where fault injection lives, and what that means for the `local` corpus category

- **Status:** proposed
- **Date:** 2026-08-05
- **Stage:** S2
- **Deciders:** maintainer (proposed by Claude Code)

## Context

`docs/09-testing-strategy.md` §3.2 sets an initial target of 12 cases for the `local` category:
*disk full, permissions, path limits, collisions, network filesystems.* S2-T15 built ten of
them. The remaining two classes — disk full mid-transfer, and network filesystems — turned out
not to be expressible by the corpus at all, and the reason is structural rather than an absence
of effort.

The corpus runs the **real** engine against a **real** filesystem. That is the property that
makes it valuable: a case exercises `SingleStream`, `StorageSink`, `PartFile` and the journal
exactly as a user's download does, so a passing case is evidence about the shipped code rather
than about a mock. ADR-0007 makes that the corpus's whole reason to exist.

That same property is what puts these two classes out of reach:

**Disk full mid-transfer.** Making a real volume fill at a chosen byte needs either a filesystem
the test controls — a loopback image or a small tmpfs, which is not portable and needs
privileges where it is possible at all — or a fault-injection hook inside `StorageSink`. The
second is the tempting one and it is the one to refuse: a production write path carrying a
branch that exists only for tests is a branch that can be wrong in production, and the
durability path is precisely where a stray conditional is unrecoverable.

**Network filesystems.** An NFS or SMB mount cannot be created in-process. Worse, the
pathologies that actually matter — silent write reordering, a server acknowledging an `fsync` it
did not perform, a lock that does not hold across clients — are exactly the ones that do *not*
reproduce on a local mount. A case that mounted something locally and called it covered would be
a green result for a check that never ran, which is the failure mode `docs/09` §7 exists to
prevent.

`tests/sim` already covers disk-full at the storage layer, where injection is legitimate:
`DurableData` and `DurableJournal` are traits the writer was designed around, so a bounded
volume is a normal implementation of an existing seam rather than a hole cut in the write path.
`disk_full_at_97pct_preserves_durable_coverage_without_truncation` runs there today.

## Options

| Option | Pros | Cons |
| ------ | ---- | ---- |
| **Set `local`'s corpus target to 10, and name the owner of each excluded class** | The inventory becomes something that can honestly close. Each excluded pathology still has a named owner — `tests/sim` for disk-full, manual qualification for network filesystems — so nothing is dropped, only relocated | `docs/09`'s headline total falls from ~159 to ~157, and a reader comparing against the original number needs this ADR to explain it |
| Keep 12 and add a fault-injection hook to `StorageSink` | The corpus number is met exactly as written | Puts test-only branching in the production durability path. A conditional that is wrong in production is unrecoverable there, and the corpus would be buying a number with the risk it exists to eliminate |
| Keep 12 and use a loopback filesystem in CI | Genuinely end to end | Needs privileges CI does not have on the Windows runner, makes the Linux and Windows suites structurally different, and a case that only runs on one platform is a case that stops running the day that runner changes |
| Keep 12 and leave the category permanently short | No decision needed now | The category can never honestly close, so the inventory stops being a thing anyone can act on. A target nobody can meet is indistinguishable from one nobody is trying to meet |

## Decision

**Set the `local` category's corpus target to 10, and record that disk-full is owned by
`tests/sim` and network-filesystem behaviour by manual qualification before a release.**

Fault injection belongs at a seam the production code already has for its own reasons, not at a
hole cut for a test. `DurableData` and `DurableJournal` exist because the writer needs to be
tested at all; a bounded volume implementing them costs the production path nothing. A hook
inside `StorageSink` would cost it a branch.

The two relocated classes keep named owners so the coverage claim stays truthful:

| Class | Owner | Why not the corpus |
| ----- | ----- | ------------------ |
| Disk full mid-transfer | `tests/sim`, `disk_full.rs` | Needs a controllable volume or a production hook; the storage seam already exists |
| Network filesystems | Manual qualification, recorded per release | Cannot be created in-process, and the pathologies that matter do not reproduce locally |

This ADR does not weaken any invariant. I-10's guarantees are tested; only the layer that tests
them moves, and it moves to the layer that can actually observe them.

## Consequences

**Easier:** the `local` category becomes closeable, so S2-T15 and the S2 gate stop waiting on
work that was never going to arrive. The distinction between "the corpus proves this" and
"something else proves this" becomes explicit per class rather than implied by a total.

**Harder:** `docs/09`'s totals need a note, and a future reader comparing 157 against the
original ~159 has to find this ADR. Network-filesystem behaviour becomes a release checklist
item rather than something CI enforces, which is weaker — and honestly weaker, rather than
appearing covered while nothing runs.

**Accepted:** Downpour ships without automated evidence about NFS or SMB. That is the real
state today either way; this only stops the inventory from implying otherwise.

## Reversal trigger

Raise the target back to 12 if the corpus gains a way to run against a controlled filesystem on
both Linux and Windows without privileges — a userspace filesystem the test drives, or a CI
runner that can attach a scratch volume. Revisit sooner if a user reports corruption on a
network filesystem: that would make the class urgent enough to justify a qualification harness
of its own, and this ADR should be superseded rather than stretched to cover it.
