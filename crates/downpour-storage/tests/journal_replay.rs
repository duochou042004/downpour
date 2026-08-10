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

fn resume_header() -> FileHeader {
    FileHeader::new([7_u8; 16], 64, 0, [9_u8; 32])
}

fn block_record(sequence: u64, offset: u64, len: u32) -> FramedRecord {
    FramedRecord::new(
        sequence,
        JournalRecord::BlockComplete {
            offset,
            len,
            blake3: [0_u8; 32],
        },
    )
}

/// B-25 — resume appends to the journal it recovered from, and only to that one.
///
/// A resumed transfer writes new durable evidence after the records replay already accepted. It
/// has to land in the same file: a fresh journal would describe a representation whose earlier
/// bytes it cannot account for, and the next replay would then contradict the part file it is
/// supposed to explain.
#[test]
fn a_recovered_journal_reopens_for_append_and_keeps_every_earlier_record() {
    use downpour_storage::writer::{DurableJournal, JournalFile};

    let directory = TestDirectory::new();
    let path = directory.path().join("resume.dpj");
    {
        let mut journal = JournalFile::create(&path, resume_header()).expect("create");
        journal
            .append(&block_record(0, 0, 8))
            .expect("first record");
        journal.sync_data().expect("first sync");
    }

    let before = recover_journal(&path).expect("the journal replays");
    assert_eq!(before.records().len(), 1);

    let mut reopened =
        JournalFile::open_existing(&path, before.header()).expect("a recovered journal reopens");
    reopened.append(&block_record(1, 8, 8)).expect("append");
    reopened.sync_data().expect("sync");
    drop(reopened);

    let after = recover_journal(&path).expect("the appended journal replays");
    assert_eq!(
        after.records().len(),
        2,
        "the earlier record must survive the reopen"
    );
    assert_eq!(
        after.header(),
        before.header(),
        "the header must not change"
    );
}

/// The header on disk is compared, not trusted.
///
/// It binds the transfer id, the representation length and the validator hash. A journal that
/// disagrees belongs to a different representation, and appending to it would produce durable
/// evidence for a file that was never fetched (I-3, I-11).
#[test]
fn reopening_a_journal_for_a_different_representation_is_refused() {
    use downpour_storage::writer::{JournalFile, WriterError};

    let directory = TestDirectory::new();
    let path = directory.path().join("other.dpj");
    JournalFile::create(&path, resume_header()).expect("create");

    // Same transfer, different validator: the file changed on the server between sessions.
    let different = FileHeader::new([7_u8; 16], 64, 0, [1_u8; 32]);
    let error = JournalFile::open_existing(&path, &different)
        .expect_err("a journal for another representation must be refused");
    assert!(
        matches!(error, WriterError::JournalIdentityMismatch { .. }),
        "the refusal must name the reason: {error:?}"
    );

    // Unchanged on disk: it is evidence for whatever download it does belong to.
    let replayed = recover_journal(&path).expect("the journal is intact");
    assert_eq!(replayed.header(), &resume_header());
}

/// A journal path is as plantable as a part path, and appending through a link writes this
/// download's recovery evidence into somebody else's file (B-37, same policy).
#[test]
#[cfg(unix)]
fn a_journal_path_that_is_a_symlink_is_refused_and_its_target_is_untouched() {
    use downpour_storage::writer::JournalFile;

    let directory = TestDirectory::new();
    let victim_path = directory.path().join("someone-elses-file");
    let victim_bytes = b"not this download's recovery evidence".to_vec();
    fs::write(&victim_path, &victim_bytes).expect("write the victim");
    let path = directory.path().join("linked.dpj");
    std::os::unix::fs::symlink(&victim_path, &path).expect("plant the symlink");

    JournalFile::open_existing(&path, &resume_header())
        .expect_err("a journal path that is a symlink must be refused");
    assert_eq!(
        fs::read(&victim_path).expect("the victim is still readable"),
        victim_bytes,
        "the symlink target was appended to"
    );
}
