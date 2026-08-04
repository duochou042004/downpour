//! Versioned, checksummed recovery-journal storage.
//!
//! This module owns the stable bytes, prefix-consistent replay, durable tail repair, and
//! integrity-preserving compaction required by I-9 and I-11.

mod compaction;
mod format;
mod replay;
pub(crate) mod state;

pub use compaction::{CompactionError, compact_journal};
pub use format::{
    FileHeader, FormatError, FramedRecord, HEADER_LEN, JournalRecord, MAX_PAYLOAD_LEN,
};
pub use replay::{ReplayError, ReplayOutcome, ReplayStop, recover_journal, replay_bytes};
