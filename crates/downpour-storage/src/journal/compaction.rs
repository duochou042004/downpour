//! Integrity-preserving journal compaction and atomic replacement.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use blake3::Hasher;
use thiserror::Error;

use super::{FileHeader, FormatError, FramedRecord, JournalRecord, ReplayError, recover_journal};

const HASH_BUFFER_LEN: usize = 64 * 1024;

/// A journal cannot be compacted without weakening its recovery evidence.
#[derive(Debug, Error)]
pub enum CompactionError {
    /// The journal could not be replayed or repaired.
    #[error("journal replay failed: {0}")]
    Replay(#[from] ReplayError),
    /// Reading part bytes or replacing the journal failed.
    #[error("journal compaction I/O failed: {0}")]
    Io(#[from] io::Error),
    /// A validly framed record sequence described an impossible storage state.
    #[error("journal state is invalid: {detail}")]
    InvalidState {
        /// Static explanation of the violated semantic rule.
        detail: &'static str,
    },
    /// Part bytes no longer match an original completed-block digest.
    #[error("part bytes at offset {offset} with length {len} fail their journaled BLAKE3 digest")]
    BlockDigestMismatch {
        /// Start of the damaged completed range.
        offset: u64,
        /// Length of the damaged completed range.
        len: u32,
    },
    /// A compacted record could not be represented in journal v1.
    #[error("compacted journal record cannot be encoded: {0}")]
    Format(#[from] FormatError),
}

#[derive(Clone, Debug)]
struct CompletedBlock {
    offset: u64,
    len: u32,
    blake3: [u8; 32],
}

impl CompletedBlock {
    fn end(&self) -> Result<u64, CompactionError> {
        self.offset
            .checked_add(u64::from(self.len))
            .ok_or(CompactionError::InvalidState {
                detail: "completed block end overflows u64",
            })
    }
}

#[derive(Debug)]
struct EffectiveState {
    effective_length: u64,
    truncate: Option<u64>,
    identities: Vec<Vec<u8>>,
    blocks: Vec<CompletedBlock>,
    sealed: Option<[u8; 32]>,
}

#[derive(Debug)]
struct MergeBuilder {
    start: u64,
    len: u32,
    hasher: Hasher,
}

/// Verifies durable part bytes, writes a compacted v1 journal, and atomically replaces the old
/// journal.
///
/// The authoritative journal is not replaced if any original completed-block digest fails.
/// Opaque identity updates retain their order, and the compacted checkpoint uses `wall_clock`.
pub fn compact_journal(
    journal_path: &Path,
    part_path: &Path,
    wall_clock: u64,
) -> Result<(), CompactionError> {
    let replayed = recover_journal(journal_path)?;
    let mut state = effective_state(replayed.header(), replayed.records())?;
    let mut part = File::open(part_path)?;
    let blocks = std::mem::take(&mut state.blocks);
    let merged = verify_and_merge(&mut part, blocks)?;
    let replacement = encode_replacement(replayed.header(), state, merged, wall_clock)?;
    replace_atomically(journal_path, &replacement)?;
    Ok(())
}

fn effective_state(
    header: &FileHeader,
    records: &[FramedRecord],
) -> Result<EffectiveState, CompactionError> {
    let mut state = EffectiveState {
        effective_length: header.total_length(),
        truncate: None,
        identities: Vec::new(),
        blocks: Vec::new(),
        sealed: None,
    };

    for (index, framed) in records.iter().enumerate() {
        match framed.record() {
            JournalRecord::BlockComplete {
                offset,
                len,
                blake3,
            } => {
                if *len == 0 {
                    return Err(CompactionError::InvalidState {
                        detail: "completed block has zero length",
                    });
                }
                let block = CompletedBlock {
                    offset: *offset,
                    len: *len,
                    blake3: *blake3,
                };
                if block.end()? > state.effective_length {
                    return Err(CompactionError::InvalidState {
                        detail: "completed block lies beyond the effective length",
                    });
                }
                state.blocks.push(block);
            }
            JournalRecord::Checkpoint { .. } => {}
            JournalRecord::IdentityUpdate { cbor } => state.identities.push(cbor.clone()),
            JournalRecord::Truncate { new_length } => {
                if *new_length > state.effective_length {
                    return Err(CompactionError::InvalidState {
                        detail: "truncate increases the effective length",
                    });
                }
                state.effective_length = *new_length;
                state.truncate = Some(*new_length);
                state
                    .blocks
                    .retain(|block| block.end().is_ok_and(|end| end <= *new_length));
            }
            JournalRecord::Sealed { final_blake3 } => {
                if state.sealed.is_some() || index + 1 != records.len() {
                    return Err(CompactionError::InvalidState {
                        detail: "sealed record is not unique and final",
                    });
                }
                state.sealed = Some(*final_blake3);
            }
        }
    }

    state.blocks.sort_by_key(|block| block.offset);
    for adjacent in state.blocks.windows(2) {
        let previous_end = adjacent[0].end()?;
        if adjacent[1].offset < previous_end {
            return Err(CompactionError::InvalidState {
                detail: "surviving completed blocks overlap",
            });
        }
    }
    Ok(state)
}

fn verify_and_merge(
    part: &mut (impl Read + Seek),
    blocks: Vec<CompletedBlock>,
) -> Result<Vec<CompletedBlock>, CompactionError> {
    let mut merged = Vec::new();
    let mut current: Option<MergeBuilder> = None;
    let mut buffer = [0_u8; HASH_BUFFER_LEN];

    for block in blocks {
        if current
            .as_ref()
            .is_some_and(|builder| builder.start + u64::from(builder.len) != block.offset)
        {
            finish_merge(&mut current, &mut merged);
        }

        part.seek(SeekFrom::Start(block.offset))?;
        let mut remaining = u64::from(block.len);
        let mut original_hasher = Hasher::new();
        let mut position = block.offset;
        while remaining > 0 {
            let buffer_len = u64::try_from(buffer.len()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "hash buffer length overflow")
            })?;
            let take_u64 = remaining.min(buffer_len);
            let take = usize::try_from(take_u64).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "hash read length overflow")
            })?;
            part.read_exact(&mut buffer[..take])?;
            original_hasher.update(&buffer[..take]);
            feed_merge(&mut current, &mut merged, position, &buffer[..take])?;
            remaining -= take_u64;
            position = position
                .checked_add(take_u64)
                .ok_or(CompactionError::InvalidState {
                    detail: "completed block read offset overflows u64",
                })?;
        }

        if original_hasher.finalize().as_bytes() != &block.blake3 {
            return Err(CompactionError::BlockDigestMismatch {
                offset: block.offset,
                len: block.len,
            });
        }
    }
    finish_merge(&mut current, &mut merged);
    Ok(merged)
}

fn feed_merge(
    current: &mut Option<MergeBuilder>,
    merged: &mut Vec<CompletedBlock>,
    mut position: u64,
    mut bytes: &[u8],
) -> Result<(), CompactionError> {
    while !bytes.is_empty() {
        if current.is_none() {
            *current = Some(MergeBuilder {
                start: position,
                len: 0,
                hasher: Hasher::new(),
            });
        }
        let builder = current.as_mut().ok_or(CompactionError::InvalidState {
            detail: "merged-range builder is unexpectedly absent",
        })?;
        let room = u64::from(u32::MAX) - u64::from(builder.len);
        let bytes_len = u64::try_from(bytes.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "hash input length overflow")
        })?;
        let take_u64 = room.min(bytes_len);
        let take = usize::try_from(take_u64).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "merged hash length overflow")
        })?;
        builder.hasher.update(&bytes[..take]);
        let take_u32 = u32::try_from(take).map_err(|_| CompactionError::InvalidState {
            detail: "merged record length exceeds u32",
        })?;
        builder.len = builder
            .len
            .checked_add(take_u32)
            .ok_or(CompactionError::InvalidState {
                detail: "merged record length overflows u32",
            })?;
        position = position
            .checked_add(take_u64)
            .ok_or(CompactionError::InvalidState {
                detail: "merged record offset overflows u64",
            })?;
        bytes = &bytes[take..];
        if builder.len == u32::MAX {
            finish_merge(current, merged);
        }
    }
    Ok(())
}

fn finish_merge(current: &mut Option<MergeBuilder>, merged: &mut Vec<CompletedBlock>) {
    if let Some(builder) = current.take()
        && builder.len != 0
    {
        merged.push(CompletedBlock {
            offset: builder.start,
            len: builder.len,
            blake3: *builder.hasher.finalize().as_bytes(),
        });
    }
}

fn encode_replacement(
    header: &FileHeader,
    state: EffectiveState,
    merged: Vec<CompletedBlock>,
    wall_clock: u64,
) -> Result<Vec<u8>, CompactionError> {
    let covered_bytes = merged.iter().try_fold(0_u64, |total, block| {
        total
            .checked_add(u64::from(block.len))
            .ok_or(CompactionError::InvalidState {
                detail: "covered byte count overflows u64",
            })
    })?;
    let mut semantic_records = Vec::new();
    for cbor in state.identities {
        semantic_records.push(JournalRecord::IdentityUpdate { cbor });
    }
    if let Some(new_length) = state.truncate {
        semantic_records.push(JournalRecord::Truncate { new_length });
    }
    semantic_records.push(JournalRecord::Checkpoint {
        covered_bytes,
        wall_clock,
    });
    for block in merged {
        semantic_records.push(JournalRecord::BlockComplete {
            offset: block.offset,
            len: block.len,
            blake3: block.blake3,
        });
    }
    if let Some(final_blake3) = state.sealed {
        semantic_records.push(JournalRecord::Sealed { final_blake3 });
    }

    let mut encoded = header.encode().to_vec();
    for (sequence, record) in semantic_records.into_iter().enumerate() {
        let sequence = u64::try_from(sequence).map_err(|_| CompactionError::InvalidState {
            detail: "compacted journal has too many records",
        })?;
        encoded.extend_from_slice(&FramedRecord::new(sequence, record).encode()?);
    }
    Ok(encoded)
}

fn replace_atomically(path: &Path, replacement: &[u8]) -> io::Result<()> {
    replace_atomically_with_hook(path, replacement, |_| Ok(()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplacementBoundary {
    NewFileCreated,
    NewFileSynced,
    Renamed,
    DirectorySynced,
}

fn replace_atomically_with_hook(
    path: &Path,
    replacement: &[u8],
    mut hook: impl FnMut(ReplacementBoundary) -> io::Result<()>,
) -> io::Result<()> {
    let new_path = sibling_new_path(path);
    let mut new_file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&new_path)?;
    hook(ReplacementBoundary::NewFileCreated)?;
    new_file.write_all(replacement)?;
    new_file.sync_all()?;
    hook(ReplacementBoundary::NewFileSynced)?;
    drop(new_file);

    fs::rename(&new_path, path)?;
    hook(ReplacementBoundary::Renamed)?;
    sync_parent(path)?;
    hook(ReplacementBoundary::DirectorySynced)?;
    Ok(())
}

fn sibling_new_path(path: &Path) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(".new");
    PathBuf::from(name)
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "journal path has no parent directory",
        )
    })?;
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{ReplacementBoundary, replace_atomically_with_hook};
    use crate::journal::{FileHeader, replay_bytes};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "downpour-compaction-boundary-{}-{serial}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn journal(marker: u8) -> Vec<u8> {
        FileHeader::new([marker; 16], 16, 4, [marker; 32])
            .encode()
            .to_vec()
    }

    #[test]
    fn compaction_survives_each_replace_boundary() {
        let boundaries = [
            ReplacementBoundary::NewFileCreated,
            ReplacementBoundary::NewFileSynced,
            ReplacementBoundary::Renamed,
            ReplacementBoundary::DirectorySynced,
        ];

        for boundary in boundaries {
            let directory = TestDirectory::new();
            let journal_path = directory.path().join("transfer.dpj");
            let old = journal(0x11);
            let new = journal(0x22);
            fs::write(&journal_path, &old).unwrap();

            let result = replace_atomically_with_hook(&journal_path, &new, |observed| {
                if observed == boundary {
                    Err(io::Error::other("injected crash boundary"))
                } else {
                    Ok(())
                }
            });
            assert!(result.is_err());

            let authoritative = fs::read(&journal_path).unwrap();
            let replayed = replay_bytes(&authoritative).unwrap();
            let expected_marker = match boundary {
                ReplacementBoundary::NewFileCreated | ReplacementBoundary::NewFileSynced => 0x11,
                ReplacementBoundary::Renamed | ReplacementBoundary::DirectorySynced => 0x22,
            };
            assert_eq!(replayed.header().transfer_id(), &[expected_marker; 16]);
        }
    }
}
