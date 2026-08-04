//! Durable block-commit proofs for I-1, I-2, I-9, and I-10.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use downpour_intervals::{IntervalMap, IntervalState, WorkerId};
use downpour_storage::journal::{FileHeader, JournalRecord, recover_journal};
use downpour_storage::part_file::PartFile;
use downpour_storage::writer::{
    DurableData, DurableJournal, DurableWriter, JOURNAL_FLUSH_BYTES, JOURNAL_FLUSH_INTERVAL,
    JournalFile, WriterError,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(tag: &str) -> Self {
        let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "downpour-durable-writer-{tag}-{}-{serial}",
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CrashAfter {
    Write,
    DataSync,
    JournalAppend,
    JournalSync,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Event {
    Write { offset: u64, len: usize },
    DataSync,
    JournalAppend { sequence: u64 },
    JournalSync,
}

#[derive(Debug)]
struct FakeState {
    events: Vec<Event>,
    volatile_data: Vec<u8>,
    durable_data: Vec<u8>,
    volatile_records: Vec<downpour_storage::journal::FramedRecord>,
    durable_records: Vec<downpour_storage::journal::FramedRecord>,
    crash_after: Option<CrashAfter>,
}

#[derive(Clone, Debug)]
struct FakeData {
    total_length: u64,
    state: Arc<Mutex<FakeState>>,
}

#[derive(Clone, Debug)]
struct FakeJournal {
    total_length: u64,
    state: Arc<Mutex<FakeState>>,
}

impl DurableData for FakeData {
    fn total_length(&self) -> u64 {
        self.total_length
    }

    fn write_all_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), WriterError> {
        let start = usize::try_from(offset).map_err(|_| injected_error("fake data offset"))?;
        let end = start
            .checked_add(bytes.len())
            .ok_or_else(|| injected_error("fake data end"))?;
        let mut state = self.state.lock().unwrap();
        state.volatile_data[start..end].copy_from_slice(bytes);
        state.events.push(Event::Write {
            offset,
            len: bytes.len(),
        });
        if state.crash_after == Some(CrashAfter::Write) {
            return Err(injected_error("write boundary"));
        }
        Ok(())
    }

    fn sync_data(&mut self) -> Result<(), WriterError> {
        let mut state = self.state.lock().unwrap();
        state.durable_data = state.volatile_data.clone();
        state.events.push(Event::DataSync);
        if state.crash_after == Some(CrashAfter::DataSync) {
            return Err(injected_error("data sync boundary"));
        }
        Ok(())
    }
}

impl DurableJournal for FakeJournal {
    fn total_length(&self) -> u64 {
        self.total_length
    }

    fn append(
        &mut self,
        record: &downpour_storage::journal::FramedRecord,
    ) -> Result<(), WriterError> {
        let mut state = self.state.lock().unwrap();
        state.events.push(Event::JournalAppend {
            sequence: record.sequence(),
        });
        state.volatile_records.push(record.clone());
        if state.crash_after == Some(CrashAfter::JournalAppend) {
            return Err(injected_error("journal append boundary"));
        }
        Ok(())
    }

    fn sync_data(&mut self) -> Result<(), WriterError> {
        let mut state = self.state.lock().unwrap();
        state.durable_records = state.volatile_records.clone();
        state.events.push(Event::JournalSync);
        if state.crash_after == Some(CrashAfter::JournalSync) {
            return Err(injected_error("journal sync boundary"));
        }
        Ok(())
    }
}

fn injected_error(operation: &'static str) -> WriterError {
    WriterError::Io {
        operation,
        source: io::Error::other("injected crash"),
    }
}

fn fake_backends(
    total_length: u64,
    crash_after: Option<CrashAfter>,
) -> (FakeData, FakeJournal, Arc<Mutex<FakeState>>) {
    let length = usize::try_from(total_length).unwrap();
    let state = Arc::new(Mutex::new(FakeState {
        events: Vec::new(),
        volatile_data: vec![0; length],
        durable_data: vec![0; length],
        volatile_records: Vec::new(),
        durable_records: Vec::new(),
        crash_after,
    }));
    (
        FakeData {
            total_length,
            state: Arc::clone(&state),
        },
        FakeJournal {
            total_length,
            state: Arc::clone(&state),
        },
        state,
    )
}

fn map_with_grant(total_length: u64, range: std::ops::Range<u64>, worker: WorkerId) -> IntervalMap {
    let mut intervals = IntervalMap::new(total_length);
    intervals.grant(range, worker).unwrap();
    intervals
}

fn range_is_complete(intervals: &IntervalMap, start: u64, end: u64) -> bool {
    intervals.intervals().iter().any(|interval| {
        interval.start() <= start
            && end <= interval.end()
            && interval.state() == &IntervalState::Complete
    })
}

#[test]
fn completion_is_never_observable_before_data_and_journal_are_durable() {
    let scenarios = [
        Some(CrashAfter::Write),
        Some(CrashAfter::DataSync),
        Some(CrashAfter::JournalAppend),
        Some(CrashAfter::JournalSync),
        None,
    ];

    for crash_after in scenarios {
        let worker = WorkerId::new(7);
        let mut intervals = map_with_grant(8, 0..4, worker);
        let (data, journal, state) = fake_backends(8, crash_after);
        let mut writer = DurableWriter::try_new(data, journal, 0).unwrap();

        let staged = writer.stage(&mut intervals, worker, 0, b"safe", Duration::from_secs(10));
        let outcome = if crash_after == Some(CrashAfter::Write) {
            staged
        } else {
            assert!(staged.unwrap().is_empty(), "scenario {crash_after:?}");
            writer.flush(&mut intervals)
        };

        let expected_events = match crash_after {
            Some(CrashAfter::Write) => vec![Event::Write { offset: 0, len: 4 }],
            Some(CrashAfter::DataSync) => vec![Event::Write { offset: 0, len: 4 }, Event::DataSync],
            Some(CrashAfter::JournalAppend) => vec![
                Event::Write { offset: 0, len: 4 },
                Event::DataSync,
                Event::JournalAppend { sequence: 0 },
            ],
            Some(CrashAfter::JournalSync) | None => vec![
                Event::Write { offset: 0, len: 4 },
                Event::DataSync,
                Event::JournalAppend { sequence: 0 },
                Event::JournalSync,
            ],
        };
        let observed = state.lock().unwrap();
        assert_eq!(observed.events, expected_events, "scenario {crash_after:?}");

        if crash_after.is_some() {
            assert!(outcome.is_err(), "scenario {crash_after:?}");
            assert!(
                !range_is_complete(&intervals, 0, 4),
                "scenario {crash_after:?} exposed completion"
            );
            drop(observed);
            assert!(matches!(
                writer.flush(&mut intervals),
                Err(WriterError::Poisoned)
            ));
        } else {
            assert_eq!(outcome.unwrap().len(), 1);
            assert!(range_is_complete(&intervals, 0, 4));
            assert_eq!(&observed.durable_data[..4], b"safe");
            assert_eq!(observed.durable_records.len(), 1);
        }
    }
}

#[test]
fn batches_flush_at_the_exact_byte_and_time_bounds() {
    assert_eq!(JOURNAL_FLUSH_BYTES, 8 * 1024 * 1024);
    assert_eq!(JOURNAL_FLUSH_INTERVAL, Duration::from_secs(2));

    let worker = WorkerId::new(11);
    let half = JOURNAL_FLUSH_BYTES / 2;
    let mut intervals = map_with_grant(JOURNAL_FLUSH_BYTES, 0..JOURNAL_FLUSH_BYTES, worker);
    let (data, journal, state) = fake_backends(JOURNAL_FLUSH_BYTES, None);
    let mut writer = DurableWriter::try_new(data, journal, 0).unwrap();
    let block = vec![0xA5; usize::try_from(half).unwrap()];

    let first = writer
        .stage(&mut intervals, worker, 0, &block, Duration::ZERO)
        .unwrap();
    assert!(first.is_empty());
    assert!(!range_is_complete(&intervals, 0, half));
    let second = writer
        .stage(&mut intervals, worker, half, &block, Duration::ZERO)
        .unwrap();
    assert_eq!(second.len(), 2);
    assert!(range_is_complete(&intervals, 0, JOURNAL_FLUSH_BYTES));

    let state = state.lock().unwrap();
    assert_eq!(
        state.events,
        vec![
            Event::Write {
                offset: 0,
                len: usize::try_from(half).unwrap(),
            },
            Event::Write {
                offset: half,
                len: usize::try_from(half).unwrap(),
            },
            Event::DataSync,
            Event::JournalAppend { sequence: 0 },
            Event::JournalAppend { sequence: 1 },
            Event::JournalSync,
        ]
    );
    drop(state);

    let worker = WorkerId::new(12);
    let mut intervals = map_with_grant(4, 0..4, worker);
    let (data, journal, state) = fake_backends(4, None);
    let mut writer = DurableWriter::try_new(data, journal, 0).unwrap();
    let started = Duration::from_secs(30);
    assert!(
        writer
            .stage(&mut intervals, worker, 0, b"tick", started)
            .unwrap()
            .is_empty()
    );
    assert!(
        writer
            .flush_if_due(
                &mut intervals,
                started + JOURNAL_FLUSH_INTERVAL - Duration::from_nanos(1),
            )
            .unwrap()
            .is_empty()
    );
    assert!(!range_is_complete(&intervals, 0, 4));
    assert_eq!(
        writer
            .flush_if_due(&mut intervals, started + JOURNAL_FLUSH_INTERVAL)
            .unwrap()
            .len(),
        1
    );
    assert!(range_is_complete(&intervals, 0, 4));
    assert_eq!(
        state.lock().unwrap().events,
        vec![
            Event::Write { offset: 0, len: 4 },
            Event::DataSync,
            Event::JournalAppend { sequence: 0 },
            Event::JournalSync,
        ]
    );
}

#[test]
fn a_write_outside_the_current_grant_never_reaches_storage() {
    let owner = WorkerId::new(21);
    let mut intervals = map_with_grant(8, 0..4, owner);
    let (data, journal, state) = fake_backends(8, None);
    let mut writer = DurableWriter::try_new(data, journal, 0).unwrap();

    #[cfg(debug_assertions)]
    {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            writer.stage(&mut intervals, owner, 4, b"x", Duration::from_secs(1))
        }));
        assert!(outcome.is_err(), "debug grant barrier must panic");
    }
    #[cfg(not(debug_assertions))]
    {
        assert!(matches!(
            writer.stage(&mut intervals, owner, 4, b"x", Duration::from_secs(1),),
            Err(WriterError::Interval(_))
        ));
    }

    assert!(state.lock().unwrap().events.is_empty());
    assert!(!range_is_complete(&intervals, 0, 4));
}

#[test]
fn overlapping_staged_writes_are_rejected_before_the_second_write() {
    let owner = WorkerId::new(22);
    let mut intervals = map_with_grant(8, 0..8, owner);
    let (data, journal, state) = fake_backends(8, None);
    let mut writer = DurableWriter::try_new(data, journal, 0).unwrap();

    writer
        .stage(&mut intervals, owner, 0, b"left", Duration::from_secs(1))
        .unwrap();
    let error = writer
        .stage(&mut intervals, owner, 2, b"over", Duration::from_secs(1))
        .unwrap_err();

    assert!(matches!(error, WriterError::OverlappingStaged { .. }));
    assert_eq!(
        state.lock().unwrap().events,
        vec![Event::Write { offset: 0, len: 4 }]
    );
    assert!(!range_is_complete(&intervals, 0, 8));
}

#[test]
fn a_changed_grant_prevents_any_durability_commit_and_poisons_the_writer() {
    let original = WorkerId::new(31);
    let replacement = WorkerId::new(32);
    let mut intervals = map_with_grant(4, 0..4, original);
    let (data, journal, state) = fake_backends(4, None);
    let mut writer = DurableWriter::try_new(data, journal, 0).unwrap();
    writer
        .stage(&mut intervals, original, 0, b"old!", Duration::from_secs(1))
        .unwrap();

    assert_eq!(intervals.abandon(original), 4);
    intervals.grant(0..4, replacement).unwrap();
    assert!(matches!(
        writer.flush(&mut intervals),
        Err(WriterError::Interval(_))
    ));
    assert_eq!(
        state.lock().unwrap().events,
        vec![Event::Write { offset: 0, len: 4 }]
    );
    assert!(matches!(
        writer.flush(&mut intervals),
        Err(WriterError::Poisoned)
    ));
    assert!(!range_is_complete(&intervals, 0, 4));
}

#[test]
fn real_part_and_journal_replay_exactly_the_durable_batch() {
    let directory = TestDirectory::new("real-files");
    let target = directory.path().join("payload.bin");
    let journal_path = directory.path().join("transfer.dpj");
    let header = FileHeader::new([0x51; 16], 16, 4, [0x62; 32]);
    let part = PartFile::create(&target, 16).unwrap();
    let part_path = part.path().to_path_buf();
    let journal = JournalFile::create(&journal_path, header).unwrap();
    let mut writer = DurableWriter::try_new(part, journal, 0).unwrap();
    let owner = WorkerId::new(41);
    let mut intervals = map_with_grant(16, 4..8, owner);

    writer
        .stage(&mut intervals, owner, 4, b"real", Duration::from_secs(1))
        .unwrap();
    assert_eq!(writer.flush(&mut intervals).unwrap().len(), 1);
    drop(writer);

    let part_bytes = fs::read(part_path).unwrap();
    assert_eq!(&part_bytes[4..8], b"real");
    let replayed = recover_journal(&journal_path).unwrap();
    assert_eq!(replayed.records().len(), 1);
    assert!(matches!(
        replayed.records()[0].record(),
        JournalRecord::BlockComplete {
            offset: 4,
            len: 4,
            blake3,
        } if blake3 == blake3::hash(b"real").as_bytes()
    ));
}

#[test]
fn separate_batches_keep_journal_sequences_contiguous() {
    let owner = WorkerId::new(50);
    let mut intervals = map_with_grant(8, 0..8, owner);
    let (data, journal, state) = fake_backends(8, None);
    let mut writer = DurableWriter::try_new(data, journal, 0).unwrap();

    writer
        .stage(&mut intervals, owner, 0, b"left", Duration::from_secs(1))
        .unwrap();
    let first = writer.flush(&mut intervals).unwrap();
    writer
        .stage(&mut intervals, owner, 4, b"rght", Duration::from_secs(2))
        .unwrap();
    let second = writer.flush(&mut intervals).unwrap();

    assert_eq!(first[0].sequence(), 0);
    assert_eq!(second[0].sequence(), 1);
    assert_eq!(
        state
            .lock()
            .unwrap()
            .durable_records
            .iter()
            .map(downpour_storage::journal::FramedRecord::sequence)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert!(range_is_complete(&intervals, 0, 8));
}

#[test]
fn journal_creation_is_exclusive_and_never_rewrites_an_existing_header() {
    let directory = TestDirectory::new("journal-exclusive");
    let path = directory.path().join("transfer.dpj");
    let first_header = FileHeader::new([0x71; 16], 16, 4, [0x72; 32]);
    let conflicting_header = FileHeader::new([0x81; 16], 32, 8, [0x82; 32]);
    let journal = JournalFile::create(&path, first_header.clone()).unwrap();
    assert_eq!(journal.path(), path);
    let before = fs::read(&path).unwrap();

    let collision = JournalFile::create(&path, conflicting_header).unwrap_err();

    assert!(matches!(
        collision,
        WriterError::JournalAlreadyExists { .. }
    ));
    assert_eq!(fs::read(path).unwrap(), before);
    assert_eq!(before, first_header.encode());
}

#[test]
fn empty_blocks_are_refused_before_io() {
    let (data, journal, state) = fake_backends(4, None);
    let mut writer = DurableWriter::try_new(data, journal, 0).unwrap();
    let owner = WorkerId::new(51);
    let mut intervals = map_with_grant(4, 0..4, owner);
    assert!(matches!(
        writer.stage(&mut intervals, owner, 0, b"", Duration::from_secs(1),),
        Err(WriterError::EmptyBlock)
    ));
    assert!(state.lock().unwrap().events.is_empty());
}

#[test]
fn mismatched_storage_identities_are_refused() {
    let (data, _, _) = fake_backends(4, None);
    let (_, journal, _) = fake_backends(5, None);
    assert!(matches!(
        DurableWriter::try_new(data, journal, 0),
        Err(WriterError::LengthMismatch {
            data_length: 4,
            journal_length: 5,
        })
    ));
}

#[test]
fn mismatched_interval_identity_is_refused_before_io() {
    let (data, journal, state) = fake_backends(4, None);
    let mut writer = DurableWriter::try_new(data, journal, 0).unwrap();
    let owner = WorkerId::new(52);
    let mut intervals = map_with_grant(5, 0..5, owner);

    assert!(matches!(
        writer.stage(&mut intervals, owner, 0, b"x", Duration::from_secs(1),),
        Err(WriterError::IntervalLengthMismatch {
            data_length: 4,
            interval_length: 5,
        })
    ));
    assert!(state.lock().unwrap().events.is_empty());
}
