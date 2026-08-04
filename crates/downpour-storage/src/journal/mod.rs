//! Versioned, checksummed recovery-journal bytes.
//!
//! This module owns the stable on-disk representation. It deliberately does not perform I/O or
//! replay: those operations build on this codec in later S2 tasks.

mod format;

pub use format::{
    FileHeader, FormatError, FramedRecord, HEADER_LEN, JournalRecord, MAX_PAYLOAD_LEN,
};
