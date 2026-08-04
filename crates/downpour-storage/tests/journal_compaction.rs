//! Semantic-integrity proofs for journal compaction.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use downpour_storage::journal::{
    FileHeader, FramedRecord, JournalRecord, compact_journal, replay_bytes,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "downpour-journal-compaction-{}-{serial}",
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

fn block(sequence: u64, offset: u64, bytes: &[u8]) -> FramedRecord {
    FramedRecord::new(
        sequence,
        JournalRecord::BlockComplete {
            offset,
            len: u32::try_from(bytes.len()).unwrap(),
            blake3: *blake3::hash(bytes).as_bytes(),
        },
    )
}

fn write_journal(path: &Path, header: &FileHeader, records: &[FramedRecord]) -> Vec<u8> {
    let mut bytes = header.encode().to_vec();
    for record in records {
        bytes.extend_from_slice(&record.encode().unwrap());
    }
    fs::write(path, &bytes).unwrap();
    bytes
}

#[test]
fn compaction_preserves_replayed_state() {
    let directory = TestDirectory::new();
    let journal_path = directory.path().join("transfer.dpj");
    let part_path = directory.path().join("transfer.dppart");
    let part = b"abcdefghijkl";
    fs::write(&part_path, part).unwrap();
    let header = FileHeader::new([0x51; 16], 12, 4, [0x62; 32]);
    let records = vec![
        FramedRecord::new(
            0,
            JournalRecord::IdentityUpdate {
                cbor: vec![0xa1, 0x01],
            },
        ),
        block(1, 0, &part[0..4]),
        FramedRecord::new(
            2,
            JournalRecord::Checkpoint {
                covered_bytes: 4,
                wall_clock: 10,
            },
        ),
        block(3, 4, &part[4..8]),
        FramedRecord::new(
            4,
            JournalRecord::IdentityUpdate {
                cbor: vec![0xa1, 0x02],
            },
        ),
        block(5, 8, &part[8..12]),
        FramedRecord::new(6, JournalRecord::Truncate { new_length: 10 }),
    ];
    let old = write_journal(&journal_path, &header, &records);

    compact_journal(&journal_path, &part_path, 1_786_000_123).unwrap();

    let new = fs::read(&journal_path).unwrap();
    assert!(new.len() < old.len());
    let replayed = replay_bytes(&new).unwrap();
    assert_eq!(replayed.header(), &header);
    assert_eq!(
        replayed.records(),
        &[
            FramedRecord::new(
                0,
                JournalRecord::IdentityUpdate {
                    cbor: vec![0xa1, 0x01],
                },
            ),
            FramedRecord::new(
                1,
                JournalRecord::IdentityUpdate {
                    cbor: vec![0xa1, 0x02],
                },
            ),
            FramedRecord::new(2, JournalRecord::Truncate { new_length: 10 }),
            FramedRecord::new(
                3,
                JournalRecord::Checkpoint {
                    covered_bytes: 8,
                    wall_clock: 1_786_000_123,
                },
            ),
            block(4, 0, &part[0..8]),
        ]
    );
}

#[test]
fn compaction_refuses_to_bless_corrupt_part_bytes() {
    let directory = TestDirectory::new();
    let journal_path = directory.path().join("transfer.dpj");
    let part_path = directory.path().join("transfer.dppart");
    let original_part = b"abcdefgh";
    fs::write(&part_path, original_part).unwrap();
    let header = FileHeader::new([0x73; 16], 8, 4, [0x84; 32]);
    let records = vec![
        block(0, 0, &original_part[0..4]),
        block(1, 4, &original_part[4..8]),
    ];
    let old = write_journal(&journal_path, &header, &records);
    fs::write(&part_path, b"xbcdefgh").unwrap();

    let result = compact_journal(&journal_path, &part_path, 1_786_000_456);

    assert!(result.is_err());
    assert_eq!(fs::read(journal_path).unwrap(), old);
}

#[test]
fn compaction_rejects_semantically_invalid_records_without_replacement() {
    let part = b"abcdefghijkl";
    let invalid_cases = [
        vec![FramedRecord::new(
            0,
            JournalRecord::BlockComplete {
                offset: 0,
                len: 0,
                blake3: *blake3::hash(b"").as_bytes(),
            },
        )],
        vec![FramedRecord::new(
            0,
            JournalRecord::BlockComplete {
                offset: u64::MAX,
                len: 1,
                blake3: [0; 32],
            },
        )],
        vec![block(0, 8, &part[8..12]), block(1, 10, &part[10..12])],
        vec![FramedRecord::new(
            0,
            JournalRecord::Truncate { new_length: 13 },
        )],
        vec![
            FramedRecord::new(
                0,
                JournalRecord::Sealed {
                    final_blake3: *blake3::hash(part).as_bytes(),
                },
            ),
            FramedRecord::new(
                1,
                JournalRecord::Checkpoint {
                    covered_bytes: 0,
                    wall_clock: 0,
                },
            ),
        ],
    ];

    for (case, records) in invalid_cases.into_iter().enumerate() {
        let directory = TestDirectory::new();
        let journal_path = directory.path().join("transfer.dpj");
        let part_path = directory.path().join("transfer.dppart");
        fs::write(&part_path, part).unwrap();
        let header = FileHeader::new([0x95; 16], 12, 4, [0xa6; 32]);
        let old = write_journal(&journal_path, &header, &records);

        let result = compact_journal(&journal_path, &part_path, 1_786_000_789);

        assert!(result.is_err(), "invalid semantic case {case} was accepted");
        assert_eq!(
            fs::read(journal_path).unwrap(),
            old,
            "invalid semantic case {case} changed the authoritative journal"
        );
    }
}

#[test]
fn compaction_preserves_a_final_seal() {
    let directory = TestDirectory::new();
    let journal_path = directory.path().join("transfer.dpj");
    let part_path = directory.path().join("transfer.dppart");
    let part = b"abcdefgh";
    fs::write(&part_path, part).unwrap();
    let header = FileHeader::new([0xb7; 16], 8, 4, [0xc8; 32]);
    let final_blake3 = *blake3::hash(part).as_bytes();
    let records = vec![
        block(0, 0, &part[0..4]),
        block(1, 4, &part[4..8]),
        FramedRecord::new(2, JournalRecord::Sealed { final_blake3 }),
    ];
    write_journal(&journal_path, &header, &records);

    compact_journal(&journal_path, &part_path, 1_786_001_000).unwrap();

    let replayed = replay_bytes(&fs::read(journal_path).unwrap()).unwrap();
    assert_eq!(
        replayed.records().last(),
        Some(&FramedRecord::new(
            2,
            JournalRecord::Sealed { final_blake3 }
        ))
    );
}
