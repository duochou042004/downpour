# ADR-0017: Verify RFC 9530 digests with `sha2` and `base64`

- **Status:** accepted
- **Date:** 2026-08-05
- **Stage:** S2
- **Deciders:** maintainer (proposed by Claude Code)

## Context

S2-T11 must verify a server-supplied `Repr-Digest` / `Content-Digest` before the final rename
(I-4), closing B-6. S1 already parses the header into `ContentDigest { algorithm, encoded }` and
keeps the base64 payload exactly as it arrived; nothing checks it. `docs/14-tech-radar-2026.md`
already says of RFC 9530: *use opportunistically now — costs nothing when absent, valuable when
present.* The protocol decision is therefore settled. What is not settled is which code computes
SHA-256/SHA-512 and which decodes base64, and `.claude/rules/rust.md` says a dependency in the
storage path is significant enough to record.

`blake3` is already adopted and is what the journal uses per block. It cannot serve here: the
server states a SHA-2 digest, so verifying it means computing that same algorithm. This is an
interoperability requirement, not a hashing preference.

`base64` 0.22.1 is already in the resolved graph transitively. Making it a direct dependency of
`downpour-storage` adds no package to `Cargo.lock`.

The digest arrives from the network and is compared against bytes we wrote. Two properties
matter beyond correctness. The comparison must be constant-time with respect to the digest, so
that a mismatch cannot be probed byte by byte — a weak property here, since an attacker who can
already choose our bytes has easier avenues, but free to obtain and awkward to add later. And a
malformed or oversized base64 payload must be rejected before allocation rather than after.

## Options

| Option | Pros | Cons |
| ------ | ---- | ---- |
| **`sha2` 0.11 + `base64` 0.22** | RustCrypto's reference implementation, MIT/Apache-2.0, pure safe Rust with optional CPU intrinsics; `base64` is already in the graph so it costs no new package; both are ubiquitous, so a supply-chain problem is loud rather than quiet | Adds one package (`sha2`) plus its `digest`/`block-buffer` support crates |
| `ring` | Single crate for both hashing and constant-time comparison; assembly-fast | Pulls a C/assembly build into a pure-Rust storage crate, and its licence is a custom ISC-like text that `cargo deny` would need an explicit exception for. A build-toolchain dependency for two hash functions is a bad trade |
| `openssl` / `native-tls` | Already on most systems | A system library with a large surface, platform-specific build breakage, and a licence story we have deliberately avoided. `rustls` was chosen precisely to keep OpenSSL out |
| Hand-rolled SHA-256 | No dependency | A hash function we wrote is a hash function we have to prove correct, and getting it subtly wrong makes verification *worse* than absent: it would report mismatches on good files and, in the wrong direction, pass corrupt ones |
| Skip digest verification | No dependency at all | Leaves B-6 open and I-4 half-implemented. The engine would hold end-to-end integrity evidence and ignore it, which is worse than never having captured it |

## Decision

**Add `sha2` 0.11 and `base64` 0.22 as direct dependencies of `downpour-storage`, and verify a
captured RFC 9530 digest before the `Sealed` record and before the rename.**

The comparison uses a constant-time equality over the raw digest bytes. The base64 payload is
length-checked against the algorithm's expected digest size before decoding, so a header
claiming a megabyte of base64 is refused rather than allocated.

An **absent** digest is not a failure. RFC 9530 adoption is thin, and most servers will offer
nothing; verification then reduces to length and coverage, which are always checked. An
**unparseable or wrong-length** digest is also not a failure of the download — it is a failure
of the evidence, recorded and ignored, because a header we cannot interpret is no worse than a
header that was never sent. Only a digest that decodes cleanly and *disagrees with the bytes*
fails the download.

On mismatch the part file and the journal are both preserved. That is docs/04 §6's explicit
instruction and it is the right default: the bytes are evidence, the user may want to retry
rather than start over, and deleting the only copy of a suspect file makes the fault
unreproducible.

## Consequences

**Easier:** I-4 becomes complete rather than partial — length, gap-free coverage, and digest are
all checked before the rename. B-6 closes, and corpus cases may now supply a digest, which they
were forbidden from doing while the engine would have ignored it.

**Harder:** one more package in the dependency graph, and a second hash family in the workspace
alongside `blake3`. The two have distinct jobs — `blake3` is ours, for per-block journal
evidence; `sha2` is the server's, for end-to-end agreement — and conflating them would mean
either failing to verify what the server actually stated or inventing a digest the server never
sent.

**Accepted:** verification streams the whole file a second time after the transfer. For a 1 GB
download that is a real cost, paid only when a server supplied a digest. It is not optional: a
digest checked after the rename is a digest checked after the user could already have opened the
file.

## Reversal trigger

Replace `sha2` if RustCrypto stops maintaining it, if `cargo deny` flags its licence or an
advisory with no fix, or if the Rust standard library ever exposes SHA-2. Revisit the streaming
cost if profiling shows verification dominating completion for large files — the fix would be to
compute the SHA-2 incrementally during the transfer rather than to stop verifying, and the
measurement must come first.
