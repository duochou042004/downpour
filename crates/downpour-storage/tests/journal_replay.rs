//! File-repair regressions for fatal headers and checksummed sequence gaps.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use downpour_storage::journal::{
    FileHeader, FramedRecord, HEADER_LEN, JournalRecord, ReplayError, ReplayStop, recover_journal,
    replay_bytes,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "downpour-journal-replay-{}-{serial}",
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

fn header() -> FileHeader {
    FileHeader::new([0x31; 16], 64, 16, [0x42; 32])
}

fn checkpoint(sequence: u64) -> FramedRecord {
    FramedRecord::new(
        sequence,
        JournalRecord::Checkpoint {
            covered_bytes: sequence * 16,
            wall_clock: 1_786_000_000,
        },
    )
}

#[test]
fn invalid_header_is_fatal_and_unchanged() {
    let directory = TestDirectory::new();
    let path = directory.path().join("transfer.dpj");
    let mut bytes = header().encode().to_vec();
    bytes.extend_from_slice(&checkpoint(0).encode().unwrap());
    bytes[12] ^= 0x01;
    fs::write(&path, &bytes).unwrap();

    let error = recover_journal(&path).unwrap_err();

    assert!(matches!(error, ReplayError::Format(_)));
    assert_eq!(fs::read(path).unwrap(), bytes);
}

#[test]
fn valid_crc_sequence_gap_stops_before_gap() {
    let first = checkpoint(0);
    let gap = checkpoint(2);
    let mut bytes = header().encode().to_vec();
    bytes.extend_from_slice(&first.encode().unwrap());
    let first_end = bytes.len() as u64;
    bytes.extend_from_slice(&gap.encode().unwrap());

    let replayed = replay_bytes(&bytes).unwrap();

    assert_eq!(replayed.records(), &[first]);
    assert_eq!(replayed.valid_bytes(), first_end);
    assert_eq!(
        replayed.stop(),
        &ReplayStop::SequenceGap {
            expected: 1,
            found: 2,
        }
    );

    let directory = TestDirectory::new();
    let path = directory.path().join("transfer.dpj");
    fs::write(&path, &bytes).unwrap();
    let recovered = recover_journal(&path).unwrap();
    assert_eq!(recovered.valid_bytes(), first_end);
    assert_eq!(fs::metadata(path).unwrap().len(), first_end);
    assert!(first_end > HEADER_LEN as u64);
}
