//! The single semantic reading of a replayed record sequence.
//!
//! Compaction and recovery must agree byte for byte on which completed blocks survive a
//! `Truncate`, which record orders are impossible, and what the effective length is. Two
//! implementations of that rule would eventually disagree, and the disagreement would be a
//! silent-corruption bug: compaction would bless bytes recovery had discarded, or recovery
//! would skip bytes compaction had merged. So there is exactly one fold, and both callers use
//! it.

use super::{FileHeader, FramedRecord, JournalRecord};

/// A validly framed record sequence described an impossible storage state.
///
/// Framing, checksums, and versions are already settled by replay. This is the layer above:
/// records that decode cleanly but cannot all be true at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct JournalStateError {
    /// Static explanation of the violated semantic rule.
    pub(crate) detail: &'static str,
}

/// One completed block as the journal recorded it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompletedBlock {
    pub(crate) offset: u64,
    pub(crate) len: u32,
    pub(crate) blake3: [u8; 32],
}

impl CompletedBlock {
    pub(crate) fn end(&self) -> Result<u64, JournalStateError> {
        self.offset
            .checked_add(u64::from(self.len))
            .ok_or(JournalStateError {
                detail: "completed block end overflows u64",
            })
    }
}

/// What a replayed record prefix says about the download, after every record is applied.
#[derive(Debug)]
pub(crate) struct EffectiveState {
    /// Representation length after any `Truncate`, starting from the journal header.
    pub(crate) effective_length: u64,
    /// The final effective truncate, when one was recorded.
    pub(crate) truncate: Option<u64>,
    /// Opaque identity CBOR payloads in observation order.
    pub(crate) identities: Vec<Vec<u8>>,
    /// Surviving completed blocks, sorted by offset and proven non-overlapping.
    pub(crate) blocks: Vec<CompletedBlock>,
    /// The final-file digest, when the download was sealed.
    pub(crate) sealed: Option<[u8; 32]>,
}

/// Folds a replayed record prefix into the state it describes.
///
/// A `Truncate` invalidates every block that crosses or lies beyond its new end rather than
/// cropping one into trusted data — a partially trusted block is exactly the thing a checksum
/// cannot catch later.
pub(crate) fn effective_state(
    header: &FileHeader,
    records: &[FramedRecord],
) -> Result<EffectiveState, JournalStateError> {
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
                    return Err(JournalStateError {
                        detail: "completed block has zero length",
                    });
                }
                let block = CompletedBlock {
                    offset: *offset,
                    len: *len,
                    blake3: *blake3,
                };
                if block.end()? > state.effective_length {
                    return Err(JournalStateError {
                        detail: "completed block lies beyond the effective length",
                    });
                }
                state.blocks.push(block);
            }
            JournalRecord::Checkpoint { .. } => {}
            JournalRecord::IdentityUpdate { cbor } => state.identities.push(cbor.clone()),
            JournalRecord::Truncate { new_length } => {
                if *new_length > state.effective_length {
                    return Err(JournalStateError {
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
                    return Err(JournalStateError {
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
            return Err(JournalStateError {
                detail: "surviving completed blocks overlap",
            });
        }
    }
    Ok(state)
}
