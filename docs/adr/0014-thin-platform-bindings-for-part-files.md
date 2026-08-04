# ADR-0014: Use thin platform bindings for truthful part-file allocation

- **Status:** proposed
- **Date:** 2026-08-04
- **Stage:** S2
- **Deciders:** maintainer (proposed by Codex)

## Context

S2-T4 must create a part file exclusively, reserve its full length before the first write,
mark it sparse where Windows supports that feature, and perform positional writes without a
shared seek cursor. These are the platform-specific foundations for I-2 and I-10. The public
API must also report the truth: a successful logical resize is not evidence that physical
space was reserved.

The normative storage specification requires this Linux fallback chain:
`fallocate(FALLOC_FL_KEEP_SIZE)` then `posix_fallocate` then `ftruncate`, with the final method
recorded as not reserving space. On Windows it requires `FSCTL_SET_SPARSE` and
`SetFileInformationByHandle(FileAllocationInfo)`, and explicitly forbids `SetFileValidData`.
The latter needs a volume-management privilege and can expose data left in newly allocated
clusters. Rust 1.97.1 has safe positional-write extension traits on both platforms, but it does
not expose safe APIs for the required allocation and Windows sparse-file operations.

The dependency audit found no general filesystem crate that implements this contract:

- `fs4` 1.1.0 provides a safe `allocate` wrapper, but its Linux path makes one `fallocate`
  call with no required fallback chain or method report. Windows sparse marking would still
  need a separate raw call.
- `file_alloc` 0.1.3 falls back to writing zeroes and uses the forbidden `SetFileValidData`
  path on Windows. It also declares no MSRV and brings Tokio into a synchronous storage
  primitive.
- `nix` would safely wrap the one `posix_fallocate` call but would add a new general Unix
  dependency while leaving the necessary Windows FFI untouched.

`rustix` 1.1.4, `libc` 0.2.189 and `windows-sys` 0.61.2 are already present in the resolved
workspace graph. Making them target-specific direct dependencies adds no package to
`Cargo.lock`. Their declared MSRVs are Rust 1.63, 1.65 and 1.71 respectively, all below
Downpour's Rust 1.97.1 MSRV. Their licences are MIT/Apache-2.0-compatible. `rustix` provides a
safe `fallocate` wrapper; `libc` and `windows-sys` are raw bindings whose use must be isolated
and audited.

## Options

| Option | Pros | Cons |
| ------ | ---- | ---- |
| **Safe standard positional I/O plus thin private bindings using `rustix`, `libc`, and `windows-sys`** | Implements the exact normative fallback chain; reports the actual method; adds no resolved package; keeps platform policy in Downpour; lets Windows verify allocation after sparse marking | Requires four small first-party FFI call sites and direct responsibility for classifying platform errors |
| `fs4` for allocation plus a custom Windows sparse wrapper | Mature safe allocation API; cross-platform surface | Linux behavior does not match the required fallback chain or expose which method worked; still needs raw Win32 FFI; adds policy we would have to work around |
| `nix` plus `rustix` on Linux and custom Win32 bindings | Avoids first-party Unix unsafe; established Unix wrappers | Adds a broad crate for one fallback call; still needs Win32 unsafe and does not reduce the platform contract we own |
| `file_alloc` | Offers one cross-platform asynchronous method and a zero-fill fallback | Windows uses forbidden `SetFileValidData`; no declared MSRV; forces Tokio into this synchronous primitive; does not report truthful reservation metadata |
| `File::set_len` on both platforms | Entirely safe and dependency-free | Creates a logical sparse length without reserving space, so `space_reserved: true` would be false and I-10's start-time `ENOSPC` protection would not exist |

## Decision

**Use Rust's standard positional-write traits and a private, narrowly allowed FFI module over
`rustix` 1.1.4, `libc` 0.2.189, and `windows-sys` 0.61.2 for part-file preparation.**

The part file is created with `OpenOptions::create_new(true)`, so an existing file or symlink
is a collision rather than something Downpour truncates. Lengths that cannot fit the
platform's signed 64-bit file offsets are rejected before creation. A zero-length object needs
no allocation and records `NotNeeded` with `space_reserved: true`.

Linux first calls safe `rustix::fs::fallocate` with `FallocateFlags::KEEP_SIZE`. Only an
unsupported-operation result advances to `libc::posix_fallocate`; operational failures such as
`ENOSPC`, `EFBIG`, or `EIO` fail creation instead of being hidden by a weaker method. Only an
unsupported `posix_fallocate` advances to `File::set_len`, recorded as `SetLength` with
`space_reserved: false`. Every successful reserving path sets the exact logical length before
returning.

Windows first requests `FSCTL_SET_SPARSE`, then requests the full allocation with
`SetFileInformationByHandle(FileAllocationInfo)`, sets the logical length, and independently
queries `FileStandardInfo.AllocationSize`. It records `FileAllocationInfo` and
`space_reserved: true` only when the queried allocation covers the requested length. An
unsupported filesystem operation falls back to logical length with `space_reserved: false`;
`ERROR_DISK_FULL`, I/O errors, and access errors remain failures. The sparse attribute permits
holes but does not prove or disprove current physical allocation, which is why the allocation
query and Windows CI test are mandatory. `SetFileValidData` is never called.

The only production `unsafe` is the one POSIX and three Win32 FFI call sites. They live in
private platform modules with a module-local `allow(unsafe_code)` and a `// SAFETY:` argument
at every call. The workspace continues to deny unsafe everywhere else. The safe public
`PartFile` API checks `offset + length` before delegating to
`std::os::unix::fs::FileExt::write_all_at` or the looped Windows `seek_write`; it performs no
durability commit, journal append, or grant ownership decision, which remain S2-T5.

## Consequences

**Easier:** Downpour implements the written platform contract without a general filesystem
abstraction that hides fallbacks. `PreallocationMethod` and `space_reserved` remain truthful,
and the Windows test can catch an interaction between sparse marking and physical allocation
instead of trusting return codes. Positional writes use the standard library's maintained OS
adapters.

**Harder:** Downpour owns error classification and a small amount of FFI. Linux and Windows
need separate behavioral tests, and non-Linux Unix targets are deliberately not implied by a
project that currently promises Linux and Windows.

**Accepted:** a filesystem may support logical sizing but not reservation. In that case the
part file is usable and explicitly reports `space_reserved: false`; later write-time `ENOSPC`
handling remains required by S2-T13. A preparation failure can leave the exclusively created
`.dppart` as an orphan rather than risking deletion of a path that may have been replaced;
the recovery specification already requires orphan reporting and forbids automatic deletion.

## Reversal trigger

Replace the private bindings if stable Rust exposes all three required operations with the
same semantics, or if a maintained Apache-2.0-compatible crate implements the exact Linux
fallback chain, Windows sparse marking, post-allocation verification, and method reporting
without `SetFileValidData` or a new async runtime. Revisit the Windows sequence immediately if
CI on NTFS or ReFS shows that a successful `FileAllocationInfo` request followed by
`FileStandardInfo` cannot demonstrate full allocation on a sparse file; do not weaken the
test or claim `space_reserved: true` without that evidence.
