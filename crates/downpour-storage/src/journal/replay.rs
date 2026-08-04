//! Prefix-consistent replay and repair of recovery journals.

use std::fs::OpenOptions;
use std::io::{self, Cursor, Read};
use std::path::Path;

use thiserror::Error;

use super::{FileHeader, FormatError, FramedRecord, HEADER_LEN};

const RECORD_PREFIX_LEN: usize = 11;
const RECORD_CHECKSUM_LEN: usize = 4;

/// Why replay stopped after a valid journal header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplayStop {
    /// The byte stream ended exactly after the header or a complete record.
    CleanEof,
    /// A frame was truncated, corrupt, unknown, or structurally invalid.
    DamagedTail {
        /// The format error raised by the first unusable frame.
        error: FormatError,
    },
    /// A checksummed frame did not have the next required sequence number.
    SequenceGap {
        /// Sequence number required at this byte boundary.
        expected: u64,
        /// Sequence number carried by the checksummed frame.
        found: u64,
    },
}

/// The exact sequential prefix recovered from a journal with a valid header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayOutcome {
    header: FileHeader,
    records: Vec<FramedRecord>,
    valid_bytes: u64,
    stop: ReplayStop,
}

impl ReplayOutcome {
    /// Returns the validated file header.
    #[must_use]
    pub const fn header(&self) -> &FileHeader {
        &self.header
    }

    /// Returns every complete, valid record before the first unusable frame.
    #[must_use]
    pub fn records(&self) -> &[FramedRecord] {
        &self.records
    }

    /// Returns the byte boundary immediately after the last valid record.
    #[must_use]
    pub const fn valid_bytes(&self) -> u64 {
        self.valid_bytes
    }

    /// Returns why replay stopped.
    #[must_use]
    pub const fn stop(&self) -> &ReplayStop {
        &self.stop
    }
}

/// A fatal journal replay failure.
#[derive(Debug, Error)]
pub enum ReplayError {
    /// The header is truncated, corrupt, or from an unsupported format.
    #[error("journal header is unusable: {0}")]
    Format(#[from] FormatError),
    /// Reading, truncating, or synchronising the journal failed.
    #[error("journal I/O failed: {0}")]
    Io(#[from] io::Error),
}

/// Replays an in-memory journal without modifying it.
///
/// Header damage is fatal. Damage after a valid header returns the exact valid sequential
/// prefix and records the stop reason in the outcome.
pub fn replay_bytes(bytes: &[u8]) -> Result<ReplayOutcome, ReplayError> {
    replay_reader(Cursor::new(bytes))
}

/// Replays a journal file and durably removes a recoverable damaged suffix.
///
/// A fatal header error leaves the file unchanged. After a valid header, a damaged frame or
/// sequence gap truncates the file to `ReplayOutcome::valid_bytes` and synchronises it before
/// returning.
pub fn recover_journal(path: &Path) -> Result<ReplayOutcome, ReplayError> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let outcome = replay_reader(&mut file)?;
    if !matches!(outcome.stop, ReplayStop::CleanEof) {
        file.set_len(outcome.valid_bytes)?;
        file.sync_all()?;
    }
    Ok(outcome)
}

fn replay_reader(mut reader: impl Read) -> Result<ReplayOutcome, ReplayError> {
    let mut header_bytes = [0_u8; HEADER_LEN];
    let header_read = read_up_to(&mut reader, &mut header_bytes)?;
    let header = FileHeader::decode(&header_bytes[..header_read])?;
    let mut records = Vec::new();
    let mut expected = 0_u64;
    let mut valid_bytes = u64::try_from(HEADER_LEN).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "journal header length overflow")
    })?;

    loop {
        let mut prefix = [0_u8; RECORD_PREFIX_LEN];
        let prefix_read = read_up_to(&mut reader, &mut prefix)?;
        if prefix_read == 0 {
            return Ok(ReplayOutcome {
                header,
                records,
                valid_bytes,
                stop: ReplayStop::CleanEof,
            });
        }
        if prefix_read < RECORD_PREFIX_LEN {
            return Ok(damaged_outcome(
                header,
                records,
                valid_bytes,
                FormatError::Truncated {
                    needed: RECORD_PREFIX_LEN,
                    actual: prefix_read,
                },
            ));
        }

        let payload_len = usize::from(u16::from_le_bytes([prefix[9], prefix[10]]));
        let suffix_len = payload_len
            .checked_add(RECORD_CHECKSUM_LEN)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "journal frame length overflow")
            })?;
        let mut frame = Vec::with_capacity(RECORD_PREFIX_LEN + suffix_len);
        frame.extend_from_slice(&prefix);
        frame.resize(RECORD_PREFIX_LEN + suffix_len, 0);
        let suffix_read = read_up_to(&mut reader, &mut frame[RECORD_PREFIX_LEN..])?;
        if suffix_read < suffix_len {
            frame.truncate(RECORD_PREFIX_LEN + suffix_read);
            return Ok(damaged_outcome(
                header,
                records,
                valid_bytes,
                FormatError::Truncated {
                    needed: RECORD_PREFIX_LEN + suffix_len,
                    actual: frame.len(),
                },
            ));
        }

        let (record, consumed) = match FramedRecord::decode_prefix(&frame) {
            Ok(decoded) => decoded,
            Err(error) => {
                return Ok(damaged_outcome(header, records, valid_bytes, error));
            }
        };
        if record.sequence() != expected {
            return Ok(ReplayOutcome {
                header,
                records,
                valid_bytes,
                stop: ReplayStop::SequenceGap {
                    expected,
                    found: record.sequence(),
                },
            });
        }

        records.push(record);
        let consumed = u64::try_from(consumed).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "journal frame length overflow")
        })?;
        valid_bytes = valid_bytes.checked_add(consumed).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "journal byte offset overflow")
        })?;
        expected = expected.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "journal sequence overflow")
        })?;
    }
}

fn damaged_outcome(
    header: FileHeader,
    records: Vec<FramedRecord>,
    valid_bytes: u64,
    error: FormatError,
) -> ReplayOutcome {
    ReplayOutcome {
        header,
        records,
        valid_bytes,
        stop: ReplayStop::DamagedTail { error },
    }
}

fn read_up_to(reader: &mut impl Read, destination: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0_usize;
    while filled < destination.len() {
        match reader.read(&mut destination[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}
