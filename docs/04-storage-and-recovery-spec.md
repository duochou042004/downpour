# Storage and Recovery Specification

**Status: Normative.** This document owns invariants I-1, I-4, I-9, I-10, I-11.

The storage layer is where a download manager either is or is not trustworthy. Everything
here is written to make one guarantee: **after any crash, at any moment, the bytes we claim
to have are the bytes we actually have.**

---

## 1. On-disk layout

For a download with id `D` targeting `~/Downloads/ubuntu.iso`:

```
~/Downloads/
  ubuntu.iso.dppart              # the data, sparse, preallocated to full length
$XDG_DATA_HOME/downpour/
  downpour.db                    # SQLite (WAL): queue, identities, checkpoints
  journals/
    D.dpj                        # append-only recovery journal for D
```

The part file lives **next to the target**, not in a temp directory, because:

- it guarantees the final rename is same-filesystem and therefore atomic;
- it means the user can see space being consumed where they expect;
- moving a partial download between filesystems is then an explicit operation, not an
  accidental one at completion time.

The journal lives with application state, not with the data, so that clearing a downloads
folder does not silently destroy recovery information for an active transfer.

---

## 2. The part file

### 2.1 Creation

1. Create `<target>.dppart` exclusively (`O_EXCL` / `CREATE_NEW`). A collision means another
   process owns this download — do not proceed.
2. Mark it sparse where supported (`FSCTL_SET_SPARSE` on Windows; sparse by default on
   ext4/xfs/btrfs).
3. Preallocate the full length (I-10):
   - Linux: `fallocate(FALLOC_FL_KEEP_SIZE)`, falling back to `posix_fallocate`, falling back
     to `ftruncate` + accepting that space is not reserved.
   - Windows: `SetFileInformationByHandle(FileAllocationInfo)`. `SetFileValidData` is **not**
     used — it requires a privilege and exposes uninitialised disk contents.
4. Record which preallocation method succeeded. If none reserved space, set
   `space_reserved: false` and warn: `ENOSPC` becomes likely rather than impossible.

Preallocating up front converts "disk full at 97%" from a data-integrity event into a
start-time error, which is the whole point.

### 2.2 Writing

- Positional writes only (`pwrite` / `WriteFile` with an `OVERLAPPED` offset). No shared seek
  cursor, so workers do not contend and cannot race the file position.
- One file handle per download, owned by the writer task. Workers send `(offset, bytes)` over
  a bounded channel.
- The writer never accepts an offset outside the sender's current grant. In debug builds this
  is a panic; in release it is a hard error that fails the download. This is the runtime
  enforcement of I-2.

### 2.3 Durability

The ordering is fixed (I-1):

```
1. pwrite(bytes @ offset)
2. fdatasync(fd)                       # Linux;  FlushFileBuffers on Windows
3. append journal record {offset, len, blake3, seq}
4. fdatasync(journal_fd)
5. allocator marks the interval Complete
```

Steps 2 and 4 are expensive, so they are **batched**, not skipped:

- Accumulate completed blocks in memory.
- Flush when either `JOURNAL_FLUSH_INTERVAL` (2 s) or `JOURNAL_FLUSH_BYTES` (8 MiB) is
  reached, whichever comes first.
- Intervals stay `InProgress` until their batch is durable. Unflushed work is simply re-fetched
  after a crash — a cost measured in seconds, not in correctness.

> **Never** move step 5 before step 4 to improve a benchmark. That single reordering is the
> corruption bug that has shipped in most download managers at some point. If a change makes
> the engine faster by weakening this ordering, the change is wrong.

### 2.4 `ENOSPC`

On a write failure with "no space":

1. Stop all workers for this download immediately.
2. Flush the journal (it is small; there is almost always room).
3. Move the download to `Paused` with reason `DiskFull`.
4. Emit an event so the UI can tell the user which volume and how much is needed.

Never truncate the part file to free space. The user's other data is not ours to sacrifice,
and truncation destroys the very bytes we would resume from.

---

## 3. The recovery journal

### 3.1 Why a journal and not just SQLite

Block completions are high-frequency and small. Writing each to SQLite means a transaction per
block; batching them into SQLite means holding uncommitted progress in memory, which is
exactly what we must not do. An append-only journal gives O(1) appends, trivially correct
crash semantics, and a torn-tail behaviour that is easy to reason about.

SQLite remains the queryable store for everything that is not per-block progress.

### 3.2 Format

```
┌──────────────────────── file header (72 bytes) ────────────────────────┐
│ magic "DPJ1" (4) │ format_version u16 │ flags u16 │ download_id (16)   │
│ total_length u64 │ block_size u32 │ validator_hash (32) │ crc32c u32   │
└────────────────────────────────────────────────────────────────────────┘
┌──────────────────────── record (variable) ─────────────────────────────┐
│ seq u64 │ kind u8 │ payload_len u16 │ payload […] │ crc32c u32         │
└────────────────────────────────────────────────────────────────────────┘
```

ADR-0012 fixes the v1 byte contract:

- all integers are little-endian;
- the header CRC32C covers its first 68 bytes, including magic, version and flags;
- a record CRC32C covers the sequence, kind, payload length and payload — every byte before
  the checksum, not only the payload;
- CRC32C means CRC-32/ISCSI (Castagnoli), whose check value for `123456789` is `0xe3069283`;
- v1 is format version `1` and permits no non-zero flag bit;
- `payload_len` bounds every record payload to 65,535 bytes before allocation;
- unknown kinds and unsupported versions are errors. An incompatible change or new record
  kind increments the file-header version so an older binary refuses the journal under I-11.

The fixed payload sizes are 44 bytes for `BlockComplete`, 16 for `Checkpoint`, 8 for
`Truncate`, and 32 for `Sealed`. `IdentityUpdate` carries opaque CBOR bytes at this layer;
S2-T6 owns their schema and semantic validation.

Record kinds:

| Kind | Payload | Meaning |
| ---- | ------- | ------- |
| `0x01 BlockComplete` | `offset u64, len u32, blake3 [32]` | These bytes are durable |
| `0x02 Checkpoint` | `covered_bytes u64, wall_clock u64` | Summary; lets replay start late |
| `0x03 IdentityUpdate` | CBOR `DownloadIdentity` delta | URL refreshed, validator changed |
| `0x04 Truncate` | `new_length u64` | Server reported a different length; blocks past it invalid |
| `0x05 Sealed` | `final_blake3 [32]` | Verification passed; file renamed |

### 3.3 Replay

```
open journal
verify file header crc
seq_expected = 0
for each record:
    if crc invalid                 → stop (torn tail; discard remainder)
    if seq != seq_expected         → stop (gap; discard remainder)
    apply record
    seq_expected += 1
truncate journal to the last valid record
```

Guarantees (I-9):

- Replay always yields a **prefix-consistent** state. Never a partially applied record.
- A torn tail costs at most `JOURNAL_FLUSH_INTERVAL` of progress.
- A corrupt middle record is impossible to distinguish from a torn tail, and is treated the
  same way — conservatively, by discarding everything after it.
- Replay never panics. Property-tested against truncated and bit-flipped journals.

### 3.4 Optional block verification on replay

Config `verify_on_resume`:

| Value | Behaviour |
| ----- | --------- |
| `off` | Trust the journal. Fastest. |
| `sample` (default) | Verify a random 1% of blocks against their recorded BLAKE3 |
| `full` | Verify every block. Slow, but conclusive after a hardware fault |

The BLAKE3-per-block record is what makes this possible at all: hashing is fast enough that
recording a hash per block is not a bottleneck, so we get verification for free later.

### 3.5 Compaction

Journals grow linearly with the file. When a journal exceeds `JOURNAL_COMPACT_BYTES`
(default 4 MiB), write a new journal containing a header, one `Checkpoint`, and the merged
interval set, then atomically replace. Compaction is crash-safe: build `D.dpj.new`, fsync,
rename over `D.dpj`.

---

## 4. SQLite schema

WAL mode, `synchronous = NORMAL` (the journal is our durability mechanism, not SQLite),
`foreign_keys = ON`, `busy_timeout = 5000`.

```sql
PRAGMA user_version = 1;   -- schema version (I-11)

CREATE TABLE downloads (
    id                TEXT PRIMARY KEY,        -- UUIDv7: time-ordered
    state             TEXT NOT NULL,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    target_path       TEXT NOT NULL,
    part_path         TEXT NOT NULL,
    total_length      INTEGER,
    covered_bytes     INTEGER NOT NULL DEFAULT 0,
    queue_position    INTEGER,
    priority          INTEGER NOT NULL DEFAULT 0,
    error_kind        TEXT,
    error_detail      TEXT,
    space_reserved    INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE identities (
    download_id       TEXT PRIMARY KEY REFERENCES downloads(id) ON DELETE CASCADE,
    current_url       TEXT NOT NULL,
    final_url         TEXT,
    redirect_chain    TEXT,                    -- JSON array
    page_url          TEXT,
    origin            TEXT NOT NULL,
    validator_kind    TEXT NOT NULL,           -- strong-etag | last-modified | none
    validator_value   TEXT,
    server_digest     TEXT,                    -- RFC 9530, algorithm:base64
    content_type      TEXT,
    suggested_name    TEXT,
    request_context   TEXT NOT NULL,           -- JSON; secrets are keyring references only
    probed_at         INTEGER NOT NULL
);

CREATE TABLE url_history (
    download_id       TEXT NOT NULL REFERENCES downloads(id) ON DELETE CASCADE,
    url               TEXT NOT NULL,
    seen_at           INTEGER NOT NULL,
    PRIMARY KEY (download_id, seen_at)
);

CREATE TABLE checkpoints (
    download_id       TEXT PRIMARY KEY REFERENCES downloads(id) ON DELETE CASCADE,
    journal_seq       INTEGER NOT NULL,
    covered_bytes     INTEGER NOT NULL,
    interval_map      BLOB NOT NULL,           -- compact encoding of Complete intervals
    written_at        INTEGER NOT NULL
);

CREATE TABLE compat_profiles (
    origin            TEXT PRIMARY KEY,
    range_support     TEXT NOT NULL,
    max_useful_conns  INTEGER,
    observed_protocol TEXT,
    notes             TEXT,
    updated_at        INTEGER NOT NULL
);

CREATE INDEX idx_downloads_state ON downloads(state);
CREATE INDEX idx_downloads_queue ON downloads(queue_position) WHERE queue_position IS NOT NULL;
```

Notes:

- **UUIDv7** ids: time-ordered, so index locality is good and listing by creation is free.
- `request_context` never contains secret material. Cookies and auth headers are stored in
  the OS keyring; this column holds *references* (I-14).
- `compat_profiles` is the beginning of accumulated compatibility intelligence — what we
  learned about an origin, reused next time. It is a cache and is always safe to delete.

### 3.6 Authority

**If SQLite and the journal disagree, the journal wins.** SQLite can be rebuilt by scanning
`journals/`. A `dp repair` command does exactly that. This is why a corrupt database is an
inconvenience rather than data loss.

---

## 5. Recovery on daemon start

```
1. Open SQLite. If it fails to open or fails an integrity check → rebuild from journals/.
2. For every download not in {Completed, Failed}:
   a. Does the part file exist?           no  → Failed(PartFileMissing), keep the record
   b. Is its length == total_length?      no  → re-preallocate; blocks past EOF are invalid
   c. Replay the journal (§3.3)
   d. Compare replayed coverage with the SQLite checkpoint
        journal ahead   → normal (checkpoint lags); update SQLite
        SQLite ahead    → journal was truncated; trust the journal, log a warning
   e. Rebuild the interval map: Complete ∪ Pending, no InProgress survives a restart
   f. State ← Paused. Never auto-resume on start.
3. Emit a recovery summary event.
```

Step (f) is deliberate. After an unclean shutdown, the user may have been mid-something, the
network may have changed, or the machine may be on a metered connection. Auto-resuming ten
downloads on boot is a good way to be uninstalled. The user (or a config option) decides.

---

## 6. Verification and completion

When the interval map covers `[0, total_length)`:

```
1. State ← Verifying.
2. fsync the part file.
3. Check length on disk == total_length.                     fail → Failed(LengthMismatch)
4. Check the interval map has no gaps.                       fail → resume the gaps
5. If a server digest exists (RFC 9530):
      stream the file, compute the digest, compare.          fail → Failed(DigestMismatch)
6. If verify_on_resume == full, verify every block hash.
7. Compute the final BLAKE3; append a Sealed record.
8. Resolve the final filename (§7).
9. Atomically rename .dppart → final name.                   (I-4)
10. Delete the journal. State ← Completed.
```

Order matters: the rename is last, and it only happens after every check has passed. A
`.dppart` file is never renamed on the strength of a byte counter alone.

On `DigestMismatch`, keep the part file and the journal. That is evidence, and the user may
want to retry rather than start over.

---

## 7. Filename resolution

Priority order:

1. An explicit user-specified name.
2. `Content-Disposition: attachment; filename*=` (RFC 5987 / RFC 6266, UTF-8 form preferred).
3. `Content-Disposition; filename=`.
4. The last path segment of the final URL, percent-decoded.
5. A generated name from the content type and timestamp.

Then sanitise:

- Strip path separators and control characters.
- Windows: reject reserved device names (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`,
  `LPT1`–`LPT9`), trailing dots and spaces; enforce the path length limit.
- Never allow the name to escape the target directory. A `filename` of `../../.bashrc` is an
  attack, and the check must be on the resolved path, not on the string.
- On collision: `name (2).ext`, `name (3).ext`, … Never overwrite silently. Never append to
  an existing file.

---

## 8. Cleanup and retention

| Event | Part file | Journal | DB row |
| ----- | --------- | ------- | ------ |
| Completed | renamed | deleted | kept (history) |
| Failed | kept | kept | kept |
| Cancelled by user | deleted (with confirmation) | deleted | kept, marked cancelled |
| Removed from history | deleted | deleted | deleted |
| Orphaned part file (no DB row) | reported by `dp repair`, never auto-deleted | — | — |

Downpour does not delete user data on its own initiative. An orphaned `.dppart` is surfaced,
not removed.

---

## 9. Platform specifics

| Concern | Linux | Windows |
| ------- | ----- | ------- |
| Durable write | `fdatasync` | `FlushFileBuffers` |
| Preallocate | `fallocate` → `posix_fallocate` → `ftruncate` | `FileAllocationInfo` |
| Sparse | default on ext4/xfs/btrfs | `FSCTL_SET_SPARSE` |
| Positional write | `pwrite` | `WriteFile` + `OVERLAPPED` |
| Atomic rename | `rename(2)` | `MoveFileEx(MOVEFILE_REPLACE_EXISTING)` |
| Free space | `statvfs` | `GetDiskFreeSpaceEx` |
| Path limits | 255 bytes/component | 260 chars unless long paths enabled; use `\\?\` |
| Case sensitivity | sensitive | insensitive — collision check must be case-insensitive |

Network filesystems (NFS, SMB, sshfs) do not honour these guarantees reliably. Detect them,
warn once, and record it on the download so a later corruption report can be attributed
correctly.

### `io_uring`

Attractive for the write path at multi-gigabit speeds, and a real 2026 option. **Not in 1.0.**
The write path is not the bottleneck at the speeds Downpour will actually see, and adding a
second I/O model next to Tokio's threadpool doubles the crash-safety surface — the one area
where we cannot afford extra complexity. Revisit when benchmarks show the writer is the
constraint. Tracked in `14-tech-radar-2026.md`.
