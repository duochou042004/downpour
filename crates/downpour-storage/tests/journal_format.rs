//! S2-T2 — byte-level proofs for the versioned recovery-journal codec.
//!
//! These fixtures are independent of the implementation. If a serializer default, integer
//! byte order, field width, CRC polynomial, or CRC coverage changes, this suite must go red.

use downpour_storage::journal::{
    FileHeader, FormatError, FramedRecord, HEADER_LEN, JournalRecord, MAX_PAYLOAD_LEN,
};

const HEADER_V1: [u8; 72] = [
    0x44, 0x50, 0x4a, 0x31, 0x01, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
    0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
    0x08, 0x09, 0x0a, 0x0b, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
    0x1c, 0x1d, 0x1e, 0x1f, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b,
    0x2c, 0x2d, 0x2e, 0x2f, 0x84, 0x5b, 0x77, 0xd4,
];

const HEADER_V2: [u8; 72] = [
    0x44, 0x50, 0x4a, 0x31, 0x02, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
    0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
    0x08, 0x09, 0x0a, 0x0b, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
    0x1c, 0x1d, 0x1e, 0x1f, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b,
    0x2c, 0x2d, 0x2e, 0x2f, 0x82, 0x6a, 0x64, 0x48,
];

const HEADER_WITH_UNKNOWN_FLAGS: [u8; 72] = [
    0x44, 0x50, 0x4a, 0x31, 0x01, 0x00, 0x01, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
    0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
    0x08, 0x09, 0x0a, 0x0b, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
    0x1c, 0x1d, 0x1e, 0x1f, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b,
    0x2c, 0x2d, 0x2e, 0x2f, 0x22, 0x50, 0x87, 0xd8,
];

const BLOCK_COMPLETE: [u8; 59] = [
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x2c, 0x00, 0x10, 0x11, 0x12, 0x13, 0x14,
    0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38,
    0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f, 0x1e, 0xb9, 0x9a, 0x4f,
];

const CHECKPOINT: [u8; 31] = [
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x10, 0x00, 0x10, 0x11, 0x12, 0x13, 0x14,
    0x15, 0x16, 0x17, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x91, 0xf2, 0x4e, 0x8a,
];

const IDENTITY_UPDATE: [u8; 19] = [
    0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x04, 0x00, 0xa1, 0x61, 0x78, 0x01, 0xbe,
    0x66, 0x65, 0x9d,
];

const TRUNCATE: [u8; 23] = [
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x08, 0x00, 0x30, 0x31, 0x32, 0x33, 0x34,
    0x35, 0x36, 0x37, 0xc7, 0x84, 0xfd, 0x4b,
];

const SEALED: [u8; 47] = [
    0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x20, 0x00, 0x40, 0x41, 0x42, 0x43, 0x44,
    0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b, 0x4c, 0x4d, 0x4e, 0x4f, 0x50, 0x51, 0x52, 0x53, 0x54,
    0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x5b, 0x5c, 0x5d, 0x5e, 0x5f, 0x5f, 0x88, 0xf9, 0x9a,
];

const UNKNOWN_KIND: [u8; 15] = [
    0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x7f, 0x00, 0x00, 0x34, 0x2f, 0x9d, 0x71,
];

const BAD_BLOCK_LENGTH: [u8; 16] = [
    0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x01, 0x00, 0xaa, 0xa5, 0xe8, 0xe9, 0x3d,
];

fn ascending<const N: usize>(start: u8) -> [u8; N] {
    std::array::from_fn(|index| start.wrapping_add(u8::try_from(index).expect("small fixture")))
}

fn sample_header() -> FileHeader {
    FileHeader::new(
        ascending::<16>(0),
        u64::from_le_bytes(ascending::<8>(0)),
        u32::from_le_bytes(ascending::<4>(8)),
        ascending::<32>(0x10),
    )
}

fn sample_records() -> Vec<FramedRecord> {
    vec![
        FramedRecord::new(
            0,
            JournalRecord::BlockComplete {
                offset: u64::from_le_bytes(ascending::<8>(0x10)),
                len: u32::from_le_bytes(ascending::<4>(0x18)),
                blake3: ascending::<32>(0x20),
            },
        ),
        FramedRecord::new(
            1,
            JournalRecord::Checkpoint {
                covered_bytes: u64::from_le_bytes(ascending::<8>(0x10)),
                wall_clock: u64::from_le_bytes(ascending::<8>(0x20)),
            },
        ),
        FramedRecord::new(
            2,
            JournalRecord::IdentityUpdate {
                cbor: vec![0xa1, 0x61, 0x78, 0x01],
            },
        ),
        FramedRecord::new(
            3,
            JournalRecord::Truncate {
                new_length: u64::from_le_bytes(ascending::<8>(0x30)),
            },
        ),
        FramedRecord::new(
            4,
            JournalRecord::Sealed {
                final_blake3: ascending::<32>(0x40),
            },
        ),
    ]
}

#[test]
fn header_accessors_preserve_validator_identity() {
    let header = sample_header();

    assert_eq!(header.transfer_id(), &ascending::<16>(0));
    assert_eq!(header.total_length(), u64::from_le_bytes(ascending::<8>(0)));
    assert_eq!(header.block_size(), u32::from_le_bytes(ascending::<4>(8)));
    assert_eq!(header.validator_hash(), &ascending::<32>(0x10));
}

#[test]
fn journal_records_round_trip_and_refuse_newer_versions() {
    assert_eq!(HEADER_LEN, 72);
    let header = sample_header();
    assert_eq!(FileHeader::decode(&header.encode()), Ok(header));

    for record in sample_records() {
        let encoded = record.encode().expect("sample payloads fit the v1 frame");
        let (decoded, consumed) = FramedRecord::decode_prefix(&encoded)
            .expect("bytes emitted by the encoder must decode");
        assert_eq!(decoded, record);
        assert_eq!(consumed, encoded.len());
    }

    assert!(matches!(
        FileHeader::decode(&HEADER_V2),
        Err(FormatError::UnsupportedVersion { found: 2, .. })
    ));
}

#[test]
fn golden_bytes_pin_the_header_and_every_record_kind() {
    assert_eq!(sample_header().encode(), HEADER_V1);

    let expected: [&[u8]; 5] = [
        &BLOCK_COMPLETE,
        &CHECKPOINT,
        &IDENTITY_UPDATE,
        &TRUNCATE,
        &SEALED,
    ];
    for (record, expected_bytes) in sample_records().into_iter().zip(expected) {
        assert_eq!(
            record.encode().expect("sample payloads fit the v1 frame"),
            expected_bytes
        );
    }
}

#[test]
fn checksums_cover_header_and_record_framing_not_only_payloads() {
    let mut corrupt_header = HEADER_V1;
    corrupt_header[24] ^= 0x01;
    assert!(matches!(
        FileHeader::decode(&corrupt_header),
        Err(FormatError::HeaderChecksumMismatch { .. })
    ));

    for index in [0_usize, 8, 9, 11, BLOCK_COMPLETE.len() - 1] {
        let mut corrupt_record = BLOCK_COMPLETE.to_vec();
        if index == 9 {
            corrupt_record[index] = 43;
        } else {
            corrupt_record[index] ^= 0x01;
        }
        assert!(matches!(
            FramedRecord::decode_prefix(&corrupt_record),
            Err(FormatError::RecordChecksumMismatch { .. })
        ));
    }
}

#[test]
fn decoder_rejects_unknown_or_structurally_invalid_v1_data() {
    assert!(matches!(
        FileHeader::decode(&HEADER_WITH_UNKNOWN_FLAGS),
        Err(FormatError::UnsupportedFlags { found: 1 })
    ));

    let mut bad_magic = HEADER_V1;
    bad_magic[0] = b'X';
    assert!(matches!(
        FileHeader::decode(&bad_magic),
        Err(FormatError::InvalidMagic { .. })
    ));

    assert!(matches!(
        FramedRecord::decode_prefix(&UNKNOWN_KIND),
        Err(FormatError::UnknownRecordKind { found: 0x7f })
    ));
    assert!(matches!(
        FramedRecord::decode_prefix(&BAD_BLOCK_LENGTH),
        Err(FormatError::InvalidPayloadLength {
            kind: 0x01,
            expected: 44,
            actual: 1,
        })
    ));

    for end in 0..BLOCK_COMPLETE.len() {
        assert!(matches!(
            FramedRecord::decode_prefix(&BLOCK_COMPLETE[..end]),
            Err(FormatError::Truncated { .. })
        ));
    }
}

#[test]
fn identity_payload_length_is_bounded_before_encoding() {
    let maximum = FramedRecord::new(
        9,
        JournalRecord::IdentityUpdate {
            cbor: vec![0; MAX_PAYLOAD_LEN],
        },
    );
    let encoded = maximum.encode().expect("u16::MAX bytes fit exactly");
    let (decoded, consumed) =
        FramedRecord::decode_prefix(&encoded).expect("maximum frame is decodable");
    assert_eq!(decoded, maximum);
    assert_eq!(consumed, encoded.len());

    let too_large = FramedRecord::new(
        10,
        JournalRecord::IdentityUpdate {
            cbor: vec![0; MAX_PAYLOAD_LEN + 1],
        },
    );
    assert!(matches!(
        too_large.encode(),
        Err(FormatError::PayloadTooLarge {
            actual,
            maximum: MAX_PAYLOAD_LEN,
        }) if actual == MAX_PAYLOAD_LEN + 1
    ));
}
