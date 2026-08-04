//! Manual little-endian encoding for journal format version 1.

use crc::{CRC_32_ISCSI, Crc};
use thiserror::Error;

const MAGIC: [u8; 4] = *b"DPJ1";
const FORMAT_VERSION: u16 = 1;
const SUPPORTED_FLAGS: u16 = 0;
const HEADER_CHECKSUM_OFFSET: usize = 68;
const RECORD_PREFIX_LEN: usize = 11;
const CHECKSUM_LEN: usize = 4;
const BLOCK_COMPLETE_PAYLOAD_LEN: usize = 44;
const CHECKPOINT_PAYLOAD_LEN: usize = 16;
const TRUNCATE_PAYLOAD_LEN: usize = 8;
const SEALED_PAYLOAD_LEN: usize = 32;
const BLOCK_COMPLETE_KIND: u8 = 0x01;
const CHECKPOINT_KIND: u8 = 0x02;
const IDENTITY_UPDATE_KIND: u8 = 0x03;
const TRUNCATE_KIND: u8 = 0x04;
const SEALED_KIND: u8 = 0x05;
const CRC32C: Crc<u32> = Crc::<u32>::new(&CRC_32_ISCSI);

/// Number of bytes in a version-1 journal file header.
pub const HEADER_LEN: usize = 72;

/// Largest payload representable by the version-1 record length field.
pub const MAX_PAYLOAD_LEN: usize = 65_535;

/// The fixed metadata at the beginning of a version-1 journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileHeader {
    transfer_id: [u8; 16],
    total_length: u64,
    block_size: u32,
    merkle_root: [u8; 32],
}

impl FileHeader {
    /// Creates a header for one transfer.
    #[must_use]
    pub const fn new(
        transfer_id: [u8; 16],
        total_length: u64,
        block_size: u32,
        merkle_root: [u8; 32],
    ) -> Self {
        Self {
            transfer_id,
            total_length,
            block_size,
            merkle_root,
        }
    }

    /// Returns the transfer UUID bytes stored by the journal.
    #[must_use]
    pub const fn transfer_id(&self) -> &[u8; 16] {
        &self.transfer_id
    }

    /// Returns the immutable remote-object length recorded at journal creation.
    #[must_use]
    pub const fn total_length(&self) -> u64 {
        self.total_length
    }

    /// Returns the block size used by `BlockComplete` records.
    #[must_use]
    pub const fn block_size(&self) -> u32 {
        self.block_size
    }

    /// Returns the header's initial Merkle-root field.
    #[must_use]
    pub const fn merkle_root(&self) -> &[u8; 32] {
        &self.merkle_root
    }

    /// Encodes this header into the canonical version-1 representation.
    #[must_use]
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut encoded = [0_u8; HEADER_LEN];
        encoded[0..4].copy_from_slice(&MAGIC);
        encoded[4..6].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        encoded[6..8].copy_from_slice(&SUPPORTED_FLAGS.to_le_bytes());
        encoded[8..24].copy_from_slice(&self.transfer_id);
        encoded[24..32].copy_from_slice(&self.total_length.to_le_bytes());
        encoded[32..36].copy_from_slice(&self.block_size.to_le_bytes());
        encoded[36..HEADER_CHECKSUM_OFFSET].copy_from_slice(&self.merkle_root);
        let checksum = checksum(&encoded[..HEADER_CHECKSUM_OFFSET]);
        encoded[HEADER_CHECKSUM_OFFSET..HEADER_LEN].copy_from_slice(&checksum.to_le_bytes());
        encoded
    }

    /// Decodes and validates one version-1 file header.
    ///
    /// Bytes after the fixed header are ignored so a caller may pass the beginning of a complete
    /// journal file.
    pub fn decode(encoded: &[u8]) -> Result<Self, FormatError> {
        require_len(encoded, HEADER_LEN)?;

        let magic = array_at::<4>(encoded, 0)?;
        if magic != MAGIC {
            return Err(FormatError::InvalidMagic { found: magic });
        }

        let stored_checksum = read_u32(encoded, HEADER_CHECKSUM_OFFSET)?;
        let computed_checksum = checksum(&encoded[..HEADER_CHECKSUM_OFFSET]);
        if stored_checksum != computed_checksum {
            return Err(FormatError::HeaderChecksumMismatch {
                stored: stored_checksum,
                computed: computed_checksum,
            });
        }

        let version = read_u16(encoded, 4)?;
        if version != FORMAT_VERSION {
            return Err(FormatError::UnsupportedVersion {
                found: version,
                supported: FORMAT_VERSION,
            });
        }

        let flags = read_u16(encoded, 6)?;
        if flags != SUPPORTED_FLAGS {
            return Err(FormatError::UnsupportedFlags { found: flags });
        }

        Ok(Self {
            transfer_id: array_at::<16>(encoded, 8)?,
            total_length: read_u64(encoded, 24)?,
            block_size: read_u32(encoded, 32)?,
            merkle_root: array_at::<32>(encoded, 36)?,
        })
    }
}

/// A semantic journal record before framing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JournalRecord {
    /// Marks one block as durably written and records its BLAKE3 digest.
    BlockComplete {
        /// Inclusive file offset of the completed block.
        offset: u64,
        /// Number of bytes in the completed block.
        len: u32,
        /// BLAKE3 digest of the block bytes.
        blake3: [u8; 32],
    },
    /// Summarizes journal progress without changing completion state.
    Checkpoint {
        /// Number of bytes covered when the checkpoint was written.
        covered_bytes: u64,
        /// Wall-clock timestamp supplied by the journal owner.
        wall_clock: u64,
    },
    /// Carries opaque CBOR describing a remote-identity update.
    IdentityUpdate {
        /// Canonical CBOR bytes; semantic validation belongs to the identity layer.
        cbor: Vec<u8>,
    },
    /// Records an intentional reduction of the part-file length.
    Truncate {
        /// New part-file length.
        new_length: u64,
    },
    /// Marks a journal sealed with the completed file's BLAKE3 digest.
    Sealed {
        /// BLAKE3 digest of the complete file.
        final_blake3: [u8; 32],
    },
}

/// One journal record together with its monotonically increasing sequence number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FramedRecord {
    sequence: u64,
    record: JournalRecord,
}

impl FramedRecord {
    /// Associates a record with its journal sequence number.
    #[must_use]
    pub const fn new(sequence: u64, record: JournalRecord) -> Self {
        Self { sequence, record }
    }

    /// Returns the record sequence number.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the semantic record carried by this frame.
    #[must_use]
    pub const fn record(&self) -> &JournalRecord {
        &self.record
    }

    /// Encodes this frame into the canonical version-1 representation.
    pub fn encode(&self) -> Result<Vec<u8>, FormatError> {
        let (kind, payload) = encode_payload(&self.record);
        if payload.len() > MAX_PAYLOAD_LEN {
            return Err(FormatError::PayloadTooLarge {
                actual: payload.len(),
                maximum: MAX_PAYLOAD_LEN,
            });
        }
        let payload_len =
            u16::try_from(payload.len()).map_err(|_| FormatError::PayloadTooLarge {
                actual: payload.len(),
                maximum: MAX_PAYLOAD_LEN,
            })?;

        let mut encoded = Vec::with_capacity(RECORD_PREFIX_LEN + payload.len() + CHECKSUM_LEN);
        encoded.extend_from_slice(&self.sequence.to_le_bytes());
        encoded.push(kind);
        encoded.extend_from_slice(&payload_len.to_le_bytes());
        encoded.extend_from_slice(&payload);
        encoded.extend_from_slice(&checksum(&encoded).to_le_bytes());
        Ok(encoded)
    }

    /// Decodes the first complete frame in `encoded` and returns its consumed byte count.
    ///
    /// A prefix that stops anywhere before the final checksum byte is reported as truncated,
    /// allowing replay to distinguish a torn tail from a corrupt complete frame.
    pub fn decode_prefix(encoded: &[u8]) -> Result<(Self, usize), FormatError> {
        require_len(encoded, RECORD_PREFIX_LEN)?;
        let sequence = read_u64(encoded, 0)?;
        let kind = byte_at(encoded, 8)?;
        let payload_len = usize::from(read_u16(encoded, 9)?);
        let checksum_offset =
            RECORD_PREFIX_LEN
                .checked_add(payload_len)
                .ok_or(FormatError::Truncated {
                    needed: usize::MAX,
                    actual: encoded.len(),
                })?;
        let frame_len =
            checksum_offset
                .checked_add(CHECKSUM_LEN)
                .ok_or(FormatError::Truncated {
                    needed: usize::MAX,
                    actual: encoded.len(),
                })?;
        require_len(encoded, frame_len)?;

        let stored_checksum = read_u32(encoded, checksum_offset)?;
        let computed_checksum = checksum(&encoded[..checksum_offset]);
        if stored_checksum != computed_checksum {
            return Err(FormatError::RecordChecksumMismatch {
                sequence,
                stored: stored_checksum,
                computed: computed_checksum,
            });
        }

        let payload =
            encoded
                .get(RECORD_PREFIX_LEN..checksum_offset)
                .ok_or(FormatError::Truncated {
                    needed: checksum_offset,
                    actual: encoded.len(),
                })?;
        let record = decode_payload(kind, payload)?;
        Ok((Self { sequence, record }, frame_len))
    }
}

/// A journal byte sequence that cannot be represented or safely decoded.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum FormatError {
    /// The available bytes end before a complete header or record.
    #[error("journal bytes are truncated: need {needed} bytes, have {actual}")]
    Truncated {
        /// Minimum number of bytes required.
        needed: usize,
        /// Number of bytes supplied.
        actual: usize,
    },
    /// The header does not begin with the version-1 journal magic.
    #[error("invalid journal magic: found {found:02x?}")]
    InvalidMagic {
        /// Four bytes found in the magic field.
        found: [u8; 4],
    },
    /// The header CRC-32C does not match its stored value.
    #[error("journal header checksum mismatch: stored {stored:#010x}, computed {computed:#010x}")]
    HeaderChecksumMismatch {
        /// CRC-32C read from the header.
        stored: u32,
        /// CRC-32C computed over the protected header bytes.
        computed: u32,
    },
    /// The journal uses a format version this build cannot decode.
    #[error("unsupported journal version {found}; this build supports version {supported}")]
    UnsupportedVersion {
        /// Version read from the header.
        found: u16,
        /// Sole version supported by this codec.
        supported: u16,
    },
    /// A version-1 header has flag bits whose semantics are unknown.
    #[error("unsupported journal flags {found:#06x}")]
    UnsupportedFlags {
        /// Flag bits read from the header.
        found: u16,
    },
    /// A complete record frame has a bad CRC-32C.
    #[error(
        "journal record {sequence} checksum mismatch: stored {stored:#010x}, computed {computed:#010x}"
    )]
    RecordChecksumMismatch {
        /// Sequence number present in the damaged frame.
        sequence: u64,
        /// CRC-32C read from the frame.
        stored: u32,
        /// CRC-32C computed over the protected frame bytes.
        computed: u32,
    },
    /// A checksummed version-1 frame has no defined semantic record kind.
    #[error("unknown version-1 journal record kind {found:#04x}")]
    UnknownRecordKind {
        /// Numeric record kind found in the frame.
        found: u8,
    },
    /// A fixed-size record has a payload length different from its definition.
    #[error("record kind {kind:#04x} requires {expected} payload bytes, found {actual}")]
    InvalidPayloadLength {
        /// Numeric version-1 record kind.
        kind: u8,
        /// Required payload size.
        expected: usize,
        /// Payload size declared by the frame.
        actual: usize,
    },
    /// A record payload cannot fit the version-1 `u16` length field.
    #[error("journal record payload has {actual} bytes; maximum is {maximum}")]
    PayloadTooLarge {
        /// Payload size requested by the caller.
        actual: usize,
        /// Largest size representable by this format.
        maximum: usize,
    },
}

fn encode_payload(record: &JournalRecord) -> (u8, Vec<u8>) {
    match record {
        JournalRecord::BlockComplete {
            offset,
            len,
            blake3,
        } => {
            let mut payload = Vec::with_capacity(BLOCK_COMPLETE_PAYLOAD_LEN);
            payload.extend_from_slice(&offset.to_le_bytes());
            payload.extend_from_slice(&len.to_le_bytes());
            payload.extend_from_slice(blake3);
            (BLOCK_COMPLETE_KIND, payload)
        }
        JournalRecord::Checkpoint {
            covered_bytes,
            wall_clock,
        } => {
            let mut payload = Vec::with_capacity(CHECKPOINT_PAYLOAD_LEN);
            payload.extend_from_slice(&covered_bytes.to_le_bytes());
            payload.extend_from_slice(&wall_clock.to_le_bytes());
            (CHECKPOINT_KIND, payload)
        }
        JournalRecord::IdentityUpdate { cbor } => (IDENTITY_UPDATE_KIND, cbor.clone()),
        JournalRecord::Truncate { new_length } => {
            (TRUNCATE_KIND, new_length.to_le_bytes().to_vec())
        }
        JournalRecord::Sealed { final_blake3 } => (SEALED_KIND, final_blake3.to_vec()),
    }
}

fn decode_payload(kind: u8, payload: &[u8]) -> Result<JournalRecord, FormatError> {
    match kind {
        BLOCK_COMPLETE_KIND => {
            require_payload_len(kind, payload, BLOCK_COMPLETE_PAYLOAD_LEN)?;
            Ok(JournalRecord::BlockComplete {
                offset: read_u64(payload, 0)?,
                len: read_u32(payload, 8)?,
                blake3: array_at::<32>(payload, 12)?,
            })
        }
        CHECKPOINT_KIND => {
            require_payload_len(kind, payload, CHECKPOINT_PAYLOAD_LEN)?;
            Ok(JournalRecord::Checkpoint {
                covered_bytes: read_u64(payload, 0)?,
                wall_clock: read_u64(payload, 8)?,
            })
        }
        IDENTITY_UPDATE_KIND => Ok(JournalRecord::IdentityUpdate {
            cbor: payload.to_vec(),
        }),
        TRUNCATE_KIND => {
            require_payload_len(kind, payload, TRUNCATE_PAYLOAD_LEN)?;
            Ok(JournalRecord::Truncate {
                new_length: read_u64(payload, 0)?,
            })
        }
        SEALED_KIND => {
            require_payload_len(kind, payload, SEALED_PAYLOAD_LEN)?;
            Ok(JournalRecord::Sealed {
                final_blake3: array_at::<32>(payload, 0)?,
            })
        }
        _ => Err(FormatError::UnknownRecordKind { found: kind }),
    }
}

fn require_payload_len(kind: u8, payload: &[u8], expected: usize) -> Result<(), FormatError> {
    if payload.len() == expected {
        Ok(())
    } else {
        Err(FormatError::InvalidPayloadLength {
            kind,
            expected,
            actual: payload.len(),
        })
    }
}

fn checksum(bytes: &[u8]) -> u32 {
    CRC32C.checksum(bytes)
}

fn require_len(bytes: &[u8], needed: usize) -> Result<(), FormatError> {
    if bytes.len() >= needed {
        Ok(())
    } else {
        Err(FormatError::Truncated {
            needed,
            actual: bytes.len(),
        })
    }
}

fn byte_at(bytes: &[u8], offset: usize) -> Result<u8, FormatError> {
    bytes.get(offset).copied().ok_or(FormatError::Truncated {
        needed: offset.saturating_add(1),
        actual: bytes.len(),
    })
}

fn array_at<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], FormatError> {
    let end = offset.checked_add(N).ok_or(FormatError::Truncated {
        needed: usize::MAX,
        actual: bytes.len(),
    })?;
    let slice = bytes.get(offset..end).ok_or(FormatError::Truncated {
        needed: end,
        actual: bytes.len(),
    })?;
    <[u8; N]>::try_from(slice).map_err(|_| FormatError::Truncated {
        needed: end,
        actual: bytes.len(),
    })
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, FormatError> {
    Ok(u16::from_le_bytes(array_at(bytes, offset)?))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, FormatError> {
    Ok(u32::from_le_bytes(array_at(bytes, offset)?))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, FormatError> {
    Ok(u64::from_le_bytes(array_at(bytes, offset)?))
}
