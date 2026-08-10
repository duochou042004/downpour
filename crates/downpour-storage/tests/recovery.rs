//! S2-T7 — unclean-shutdown reconciliation proofs for I-1, I-9, I-10, and I-11.
//!
//! Every case here builds real durable artifacts — a preallocated part file, an append-only
//! journal, and a SQLite record — then reconciles them the way a daemon would after a crash.
//! The claim under test is docs/04 §5: the journal is the only authority for what is durable,
//! and nothing a restart produces may be `InProgress` or auto-resuming.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use downpour_intervals::{IntervalMap, IntervalState, WorkerId};
use downpour_storage::journal::{FileHeader, FramedRecord, JournalRecord};
use downpour_storage::metadata::{
    Checkpoint, CompleteInterval, DownloadId, DownloadMetadata, DownloadState, IdentityMetadata,
    MetadataStore, PublicUrl, UrlHistoryEntry, UrlReference,
};
use downpour_storage::part_file::PartFile;
use downpour_storage::recovery::{
    CheckpointDivergence, PartFileExtent, RecoveryError, reconcile_download,
};
use downpour_storage::writer::{DurableWriter, JournalFile};
use downpour_types::{ByteRangeSpec, NegotiatedProtocol, RangeProof, RangeSupport, Validator};
use rusqlite::{Connection, params};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

const TOTAL_LENGTH: u64 = 32;
const NOW_MS: u64 = 1_770_000_000_000;

/// Version-1 file header, from docs/04 §3.2.
const HEADER_BYTES: u64 = 72;

/// `seq u64 | kind u8 | payload_len u16 | 44-byte payload | crc32c u32`.
const BLOCK_COMPLETE_FRAME_BYTES: u64 = 11 + 44 + 4;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(tag: &str) -> Self {
        let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "downpour-recovery-{tag}-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn database(&self) -> PathBuf {
        self.0.join("downpour.db")
    }

    fn journal(&self) -> PathBuf {
        self.0.join("transfer.dpj")
    }

    fn target(&self) -> PathBuf {
        self.0.join("payload.bin")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn sample_id() -> DownloadId {
    DownloadId::try_from_bytes([
        0x01, 0x91, 0x23, 0x45, 0x67, 0x89, 0x7a, 0xbc, 0x8d, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89,
        0xab,
    ])
    .expect("fixture is RFC-variant UUIDv7")
}

fn sample_download(part_path: &Path, target_path: &Path) -> DownloadMetadata {
    let current = UrlReference::parse("https://example.test/file", None)
        .expect("fixture contains no persistent secret");
    DownloadMetadata {
        id: sample_id(),
        // The state an unclean shutdown leaves behind: the daemon died mid-transfer.
        state: DownloadState::Transferring,
        created_at_ms: 1,
        updated_at_ms: 2,
        target_path: target_path.to_path_buf(),
        part_path: part_path.to_path_buf(),
        total_length: Some(TOTAL_LENGTH),
        covered_bytes: 0,
        queue_position: Some(1),
        priority: 0,
        error_kind: None,
        space_reserved: true,
        identity: IdentityMetadata {
            current_url: current.clone(),
            final_url: None,
            redirect_chain: vec![current.clone()],
            page_url: None,
            origin: PublicUrl::parse("https://example.test/").expect("fixture is public"),
            validator: Validator::StrongETag("\"etag-v1\"".to_owned()),
            server_digest: None,
            content_type: None,
            suggested_filename: None,
            request_context_ref: None,
            probed_at_ms: 3,
            protocol: NegotiatedProtocol::Http2,
            range_support: RangeSupport::Proven(
                RangeProof::from_observed_response(
                    ByteRangeSpec::FromTo { first: 0, last: 0 },
                    206,
                    Some("bytes 0-0/32"),
                    None,
                    1,
                )
                .expect("fixture is a valid observed range response"),
            ),
        },
        url_history: vec![UrlHistoryEntry {
            url: current,
            seen_at_ms: 2,
        }],
    }
}

fn header(total_length: u64) -> FileHeader {
    FileHeader::new(sample_id().as_bytes(), total_length, 8, [0x62; 32])
}

/// Durably commit `blocks` through the real writer, exactly as a live transfer would.
///
/// Returns the `.dppart` path. The writer is dropped before returning so every handle is
/// closed, which is the state a crashed daemon leaves behind from the next process's view.
fn commit_durable_blocks(directory: &TestDirectory, blocks: &[(u64, &[u8])]) -> PathBuf {
    let part = PartFile::create(directory.target(), TOTAL_LENGTH).unwrap();
    let part_path = part.path().to_path_buf();
    let journal = JournalFile::create(directory.journal(), header(TOTAL_LENGTH)).unwrap();
    let mut writer = DurableWriter::try_new(part, journal, 0).unwrap();
    let mut intervals = IntervalMap::new(TOTAL_LENGTH);
    let worker = WorkerId::new(11);

    for (offset, bytes) in blocks {
        let end = offset + u64::try_from(bytes.len()).unwrap();
        intervals.grant(*offset..end, worker).unwrap();
        writer
            .stage(
                &mut intervals,
                worker,
                *offset,
                bytes,
                Duration::from_secs(1),
            )
            .unwrap();
        writer.flush(&mut intervals).unwrap();
    }
    drop(writer);
    part_path
}

fn open_store(directory: &TestDirectory, part_path: &Path) -> MetadataStore {
    let mut store = MetadataStore::open(directory.database()).expect("an empty database is v1");
    store
        .save_download(&sample_download(part_path, &directory.target()))
        .expect("the fixture download is valid");
    store
}

fn complete_ranges(intervals: &IntervalMap) -> Vec<(u64, u64)> {
    intervals
        .intervals()
        .iter()
        .filter(|interval| *interval.state() == IntervalState::Complete)
        .map(|interval| (interval.start(), interval.end()))
        .collect()
}

fn pending_ranges(intervals: &IntervalMap) -> Vec<(u64, u64)> {
    intervals
        .intervals()
        .iter()
        .filter(|interval| *interval.state() == IntervalState::Pending)
        .map(|interval| (interval.start(), interval.end()))
        .collect()
}

/// The named proof for S2-T7, and the reason the task exists.
///
/// SQLite claims more coverage than the journal can prove. If SQLite were allowed to win, the
/// rebuilt map would mark `[8, 16)` complete, resume would skip it, and the finished file would
/// carry an eight-byte hole at exactly the expected total size — the silent corruption I-1
/// exists to prevent. The journal is the only authority, and no grant survives a restart.
#[test]
fn journal_is_authoritative_over_sqlite_and_no_in_progress_interval_survives_restart() {
    let directory = TestDirectory::new("journal-authority");
    let part_path = commit_durable_blocks(&directory, &[(0, b"aaaaaaaa"), (16, b"cccccccc")]);
    let mut store = open_store(&directory, &part_path);

    // The disposable cache overstates progress: it also claims [8, 16), which never reached
    // the journal. A truncated journal, a stale checkpoint write, or a rolled-back WAL all
    // produce exactly this.
    let overstated = Checkpoint::try_new(
        TOTAL_LENGTH,
        vec![
            CompleteInterval::try_new(0, 8).unwrap(),
            CompleteInterval::try_new(8, 16).unwrap(),
            CompleteInterval::try_new(16, 24).unwrap(),
        ],
    )
    .unwrap();
    assert!(
        store
            .save_checkpoint(sample_id(), 9, 5, &overstated)
            .unwrap(),
        "the overstated checkpoint fits the cache"
    );

    let reconciled = reconcile_download(&mut store, sample_id(), &directory.journal(), NOW_MS)
        .expect("reconciliation succeeds against intact artifacts");

    assert_eq!(
        reconciled.covered_bytes(),
        16,
        "only the two journalled blocks are durable"
    );
    assert_eq!(
        complete_ranges(reconciled.intervals()),
        vec![(0, 8), (16, 24)],
        "SQLite's unproven [8, 16) must not become Complete"
    );
    assert_eq!(
        pending_ranges(reconciled.intervals()),
        vec![(8, 16), (24, 32)],
        "everything the journal cannot prove is refetched"
    );
    assert!(
        !reconciled
            .intervals()
            .intervals()
            .iter()
            .any(|interval| matches!(interval.state(), IntervalState::InProgress { .. })),
        "no grant survives a restart"
    );
    assert_eq!(
        *reconciled.divergence(),
        CheckpointDivergence::SqliteAhead {
            journal_covered_bytes: 16,
            sqlite_covered_bytes: 24,
        },
        "the disagreement is reported, not hidden"
    );
    assert_eq!(*reconciled.extent(), PartFileExtent::Intact);
    assert_eq!(reconciled.next_sequence(), 2);
    assert!(reconciled.sealed().is_none());

    // Never auto-resume on start (docs/04 §5 step 2f).
    assert_eq!(reconciled.state(), DownloadState::Paused);
    assert!(reconciled.error_kind().is_none());

    let persisted = store
        .load_download(sample_id())
        .expect("the record is readable")
        .expect("the record is kept");
    assert_eq!(persisted.state, DownloadState::Paused);
    assert_eq!(persisted.covered_bytes, 16);

    // The cache is rebuilt from the journal rather than left overstating progress.
    let cached = store
        .load_checkpoint(sample_id())
        .expect("the rewritten checkpoint is valid")
        .expect("reconciliation writes one");
    assert_eq!(cached.checkpoint().covered_bytes(), 16);
    assert_eq!(
        cached
            .checkpoint()
            .intervals()
            .iter()
            .map(|interval| (interval.start(), interval.end()))
            .collect::<Vec<_>>(),
        vec![(0, 8), (16, 24)]
    );
    assert_eq!(cached.journal_sequence(), 2);
}

/// A checkpoint that merely lags the journal is the normal case, not a fault.
#[test]
fn a_lagging_sqlite_checkpoint_is_reported_as_the_journal_running_ahead() {
    let directory = TestDirectory::new("journal-ahead");
    let part_path = commit_durable_blocks(&directory, &[(0, b"aaaaaaaa"), (16, b"cccccccc")]);
    let mut store = open_store(&directory, &part_path);
    let lagging =
        Checkpoint::try_new(TOTAL_LENGTH, vec![CompleteInterval::try_new(0, 8).unwrap()]).unwrap();
    store.save_checkpoint(sample_id(), 1, 5, &lagging).unwrap();

    let reconciled = reconcile_download(&mut store, sample_id(), &directory.journal(), NOW_MS)
        .expect("reconciliation succeeds");

    assert_eq!(
        *reconciled.divergence(),
        CheckpointDivergence::JournalAhead {
            journal_covered_bytes: 16,
            sqlite_covered_bytes: 8,
        }
    );
    assert_eq!(
        complete_ranges(reconciled.intervals()),
        vec![(0, 8), (16, 24)]
    );
    assert_eq!(reconciled.state(), DownloadState::Paused);
}

/// A part file shorter than its journal claims cannot prove the bytes past its end.
///
/// This is I-1 at the filesystem boundary: the journal record survived, the data did not.
/// Re-extending the file must never let those bytes be skipped on resume.
#[test]
fn blocks_beyond_a_short_part_file_are_not_trusted_and_the_extent_is_restored() {
    let directory = TestDirectory::new("short-part");
    let part_path = commit_durable_blocks(&directory, &[(0, b"aaaaaaaa"), (16, b"cccccccc")]);

    // Simulate a filesystem that lost the tail: the second block's bytes are gone.
    OpenOptions::new()
        .write(true)
        .open(&part_path)
        .unwrap()
        .set_len(12)
        .unwrap();

    let mut store = open_store(&directory, &part_path);
    let reconciled = reconcile_download(&mut store, sample_id(), &directory.journal(), NOW_MS)
        .expect("a short part file is recoverable, not fatal");

    assert_eq!(
        complete_ranges(reconciled.intervals()),
        vec![(0, 8)],
        "[16, 24) is journalled but its bytes are past the observed end"
    );
    assert_eq!(reconciled.covered_bytes(), 8);
    assert_eq!(
        *reconciled.extent(),
        PartFileExtent::Reextended {
            observed_length: 12,
            total_length: TOTAL_LENGTH,
        }
    );
    assert_eq!(
        fs::metadata(&part_path).unwrap().len(),
        TOTAL_LENGTH,
        "the extent is restored so resume can write positionally again"
    );
    assert_eq!(reconciled.state(), DownloadState::Paused);
}

/// I-10: recovery never shortens a part file to make it fit.
#[test]
fn an_overlong_part_file_is_reported_and_never_truncated() {
    let directory = TestDirectory::new("overlong-part");
    let part_path = commit_durable_blocks(&directory, &[(0, b"aaaaaaaa")]);
    OpenOptions::new()
        .write(true)
        .open(&part_path)
        .unwrap()
        .set_len(TOTAL_LENGTH + 64)
        .unwrap();

    let mut store = open_store(&directory, &part_path);
    let reconciled = reconcile_download(&mut store, sample_id(), &directory.journal(), NOW_MS)
        .expect("an overlong part file is reported, not fatal");

    assert_eq!(
        *reconciled.extent(),
        PartFileExtent::Overlong {
            observed_length: TOTAL_LENGTH + 64,
            total_length: TOTAL_LENGTH,
        }
    );
    assert_eq!(
        fs::metadata(&part_path).unwrap().len(),
        TOTAL_LENGTH + 64,
        "recovery must not destroy bytes it did not write"
    );
    assert_eq!(complete_ranges(reconciled.intervals()), vec![(0, 8)]);
}

/// I-9: a torn tail costs the last batch, never the records before it.
#[test]
fn a_torn_journal_tail_yields_only_its_valid_prefix() {
    let directory = TestDirectory::new("torn-tail");
    let part_path = commit_durable_blocks(&directory, &[(0, b"aaaaaaaa"), (16, b"cccccccc")]);
    let journal_path = directory.journal();
    let intact = fs::metadata(&journal_path).unwrap().len();

    // Drop the last four bytes: the final record's checksum is now incomplete.
    OpenOptions::new()
        .write(true)
        .open(&journal_path)
        .unwrap()
        .set_len(intact - 4)
        .unwrap();

    let mut store = open_store(&directory, &part_path);
    let reconciled = reconcile_download(&mut store, sample_id(), &journal_path, NOW_MS)
        .expect("a torn tail is recoverable");

    assert_eq!(
        complete_ranges(reconciled.intervals()),
        vec![(0, 8)],
        "the torn record contributes nothing"
    );
    assert_eq!(reconciled.next_sequence(), 1);
    assert!(reconciled.journal_tail_repaired());
    assert_eq!(
        fs::metadata(&journal_path).unwrap().len(),
        HEADER_BYTES + BLOCK_COMPLETE_FRAME_BYTES,
        "the damaged suffix is durably removed, leaving exactly one record"
    );
    assert_eq!(reconciled.state(), DownloadState::Paused);
}

/// I-11: state written by a newer format is refused, never reinterpreted.
#[test]
fn a_newer_journal_version_is_refused_without_touching_the_artifacts() {
    let directory = TestDirectory::new("newer-journal");
    let part_path = commit_durable_blocks(&directory, &[(0, b"aaaaaaaa")]);
    let journal_path = directory.journal();

    // Version 2 with a repaired header checksum: structurally sound, semantically unknown.
    let mut bytes = fs::read(&journal_path).unwrap();
    bytes[4..6].copy_from_slice(&2_u16.to_le_bytes());
    let checksum = crc::Crc::<u32>::new(&crc::CRC_32_ISCSI).checksum(&bytes[..68]);
    bytes[68..72].copy_from_slice(&checksum.to_le_bytes());
    fs::write(&journal_path, &bytes).unwrap();

    let mut store = open_store(&directory, &part_path);
    let reconciled = reconcile_download(&mut store, sample_id(), &journal_path, NOW_MS)
        .expect("an unreadable journal fails the download rather than the daemon");

    assert_eq!(reconciled.state(), DownloadState::Failed);
    assert_eq!(
        reconciled.error_kind().map(|kind| kind.as_str()),
        Some("storage.journal-unreadable")
    );
    assert_eq!(
        fs::read(&journal_path).unwrap(),
        bytes,
        "a journal we cannot read is evidence, not garbage"
    );
    let persisted = store
        .load_download(sample_id())
        .unwrap()
        .expect("the record is kept");
    assert_eq!(persisted.state, DownloadState::Failed);
}

/// docs/04 §5 step 2a: a missing part file fails the download and keeps its record.
#[test]
fn a_missing_part_file_fails_the_download_without_discarding_its_record() {
    let directory = TestDirectory::new("missing-part");
    let part_path = commit_durable_blocks(&directory, &[(0, b"aaaaaaaa")]);
    fs::remove_file(&part_path).unwrap();

    let mut store = open_store(&directory, &part_path);
    let reconciled = reconcile_download(&mut store, sample_id(), &directory.journal(), NOW_MS)
        .expect("a missing part file is a download failure, not a daemon failure");

    assert_eq!(reconciled.state(), DownloadState::Failed);
    assert_eq!(
        reconciled.error_kind().map(|kind| kind.as_str()),
        Some("storage.part-file-missing")
    );
    assert_eq!(reconciled.covered_bytes(), 0);
    let persisted = store
        .load_download(sample_id())
        .unwrap()
        .expect("the record survives");
    assert_eq!(persisted.state, DownloadState::Failed);
}

/// A journal belonging to another transfer must never be applied to this one.
#[test]
fn a_journal_bound_to_another_transfer_is_refused() {
    let directory = TestDirectory::new("foreign-journal");
    let part_path = commit_durable_blocks(&directory, &[(0, b"aaaaaaaa")]);
    let foreign = directory.path().join("foreign.dpj");
    JournalFile::create(
        &foreign,
        FileHeader::new([0x77; 16], TOTAL_LENGTH, 8, [0x62; 32]),
    )
    .unwrap();

    let mut store = open_store(&directory, &part_path);
    let reconciled = reconcile_download(&mut store, sample_id(), &foreign, NOW_MS)
        .expect("a mismatched journal is a download failure");

    assert_eq!(reconciled.state(), DownloadState::Failed);
    assert_eq!(
        reconciled.error_kind().map(|kind| kind.as_str()),
        Some("storage.journal-mismatched")
    );
}

/// Without durable evidence there is no durable coverage — never a guess from SQLite.
#[test]
fn a_missing_journal_leaves_every_byte_pending_rather_than_trusting_sqlite() {
    let directory = TestDirectory::new("missing-journal");
    let part_path = commit_durable_blocks(&directory, &[(0, b"aaaaaaaa")]);
    fs::remove_file(directory.journal()).unwrap();
    let mut store = open_store(&directory, &part_path);
    let claimed =
        Checkpoint::try_new(TOTAL_LENGTH, vec![CompleteInterval::try_new(0, 8).unwrap()]).unwrap();
    store.save_checkpoint(sample_id(), 1, 5, &claimed).unwrap();

    let reconciled = reconcile_download(&mut store, sample_id(), &directory.journal(), NOW_MS)
        .expect("a missing journal is recoverable by refetching");

    assert_eq!(reconciled.covered_bytes(), 0);
    assert!(complete_ranges(reconciled.intervals()).is_empty());
    assert_eq!(pending_ranges(reconciled.intervals()), vec![(0, 32)]);
    assert_eq!(reconciled.state(), DownloadState::Paused);
}

/// A `Truncate` record reduces the representation, and blocks past the new end are dropped.
#[test]
fn a_truncate_record_shrinks_coverage_and_the_recorded_total() {
    let directory = TestDirectory::new("truncate");
    let part_path = commit_durable_blocks(&directory, &[(0, b"aaaaaaaa"), (16, b"cccccccc")]);
    let journal_path = directory.journal();
    let mut journal = OpenOptions::new().append(true).open(&journal_path).unwrap();
    use std::io::Write;
    journal
        .write_all(
            &FramedRecord::new(2, JournalRecord::Truncate { new_length: 12 })
                .encode()
                .unwrap(),
        )
        .unwrap();
    journal.sync_all().unwrap();
    drop(journal);

    let mut store = open_store(&directory, &part_path);
    let reconciled = reconcile_download(&mut store, sample_id(), &journal_path, NOW_MS)
        .expect("a truncate is applied, not rejected");

    assert_eq!(reconciled.intervals().total_length(), 12);
    assert_eq!(
        complete_ranges(reconciled.intervals()),
        vec![(0, 8)],
        "[16, 24) lies beyond the new end"
    );
    assert_eq!(reconciled.covered_bytes(), 8);
    let persisted = store.load_download(sample_id()).unwrap().unwrap();
    assert_eq!(persisted.total_length, Some(12));
    assert_eq!(
        persisted.identity.range_support,
        RangeSupport::Unknown,
        "range support proven against the old representation cannot survive it (I-6)"
    );
}

/// A terminal download has nothing to reconcile; asking is a caller error, loudly.
#[test]
fn reconciling_a_terminal_download_is_refused() {
    let directory = TestDirectory::new("terminal");
    let part_path = commit_durable_blocks(&directory, &[(0, b"aaaaaaaa")]);
    let mut store = MetadataStore::open(directory.database()).unwrap();
    let mut metadata = sample_download(&part_path, &directory.target());
    metadata.state = DownloadState::Completed;
    store.save_download(&metadata).unwrap();

    let error = reconcile_download(&mut store, sample_id(), &directory.journal(), NOW_MS)
        .expect_err("a completed download is not reconciled");
    assert!(matches!(
        error,
        RecoveryError::AlreadyTerminal {
            state: DownloadState::Completed
        }
    ));
}

/// A corrupt checkpoint is an inconvenience, not data loss (docs/04 §4.1).
#[test]
fn an_unreadable_checkpoint_does_not_stop_recovery() {
    let directory = TestDirectory::new("corrupt-checkpoint");
    let part_path = commit_durable_blocks(&directory, &[(0, b"aaaaaaaa")]);
    let mut store = open_store(&directory, &part_path);
    let valid =
        Checkpoint::try_new(TOTAL_LENGTH, vec![CompleteInterval::try_new(0, 8).unwrap()]).unwrap();
    store.save_checkpoint(sample_id(), 1, 5, &valid).unwrap();

    // Damage the disposable cache blob behind the store's back, the way a partially written
    // page or an unrelated tool would.
    Connection::open(directory.database())
        .unwrap()
        .execute(
            "UPDATE checkpoints SET interval_map = ?1 WHERE download_id = ?2",
            params![vec![0xff_u8; 12], sample_id().as_bytes().as_slice()],
        )
        .unwrap();

    let reconciled = reconcile_download(&mut store, sample_id(), &directory.journal(), NOW_MS)
        .expect("an unreadable cache row never blocks recovery");

    assert_eq!(
        *reconciled.divergence(),
        CheckpointDivergence::CheckpointUnreadable
    );
    assert_eq!(complete_ranges(reconciled.intervals()), vec![(0, 8)]);
    assert_eq!(reconciled.state(), DownloadState::Paused);
}

/// Append one already-framed record to an existing journal, as a later batch would.
fn append_record(journal_path: &Path, record: FramedRecord) {
    use std::io::Write;
    let mut journal = OpenOptions::new().append(true).open(journal_path).unwrap();
    journal.write_all(&record.encode().unwrap()).unwrap();
    journal.sync_all().unwrap();
}

/// I-4: a `Sealed` record proves verification passed, never that the rename happened.
///
/// Treating a seal as "Completed" would let a download whose `.dppart` is still sitting there
/// be reported as finished — a partial file wearing the final name is exactly what I-4 exists
/// to prevent. Recovery reports the seal and stops; acting on it is S2-T11's job.
#[test]
fn a_sealed_journal_is_reported_but_never_completes_the_download() {
    let directory = TestDirectory::new("sealed");
    let part_path = commit_durable_blocks(&directory, &[(0, b"aaaaaaaa"), (16, b"cccccccc")]);
    append_record(
        &directory.journal(),
        FramedRecord::new(
            2,
            JournalRecord::Sealed {
                final_blake3: [0xab; 32],
            },
        ),
    );

    let mut store = open_store(&directory, &part_path);
    let reconciled = reconcile_download(&mut store, sample_id(), &directory.journal(), NOW_MS)
        .expect("a sealed journal is readable");

    assert_eq!(reconciled.sealed(), Some(&[0xab_u8; 32]));
    assert_eq!(
        reconciled.state(),
        DownloadState::Paused,
        "a journal record alone never completes a download (I-4)"
    );
    let persisted = store.load_download(sample_id()).unwrap().unwrap();
    assert_eq!(persisted.state, DownloadState::Paused);
}

/// Records that frame and checksum cleanly can still be jointly impossible.
///
/// Two completed blocks claiming the same bytes cannot both be true. Picking one would be a
/// guess about which write actually landed, so recovery refuses and keeps every artifact.
#[test]
fn a_semantically_impossible_journal_fails_without_destroying_evidence() {
    let directory = TestDirectory::new("impossible");
    let part_path = commit_durable_blocks(&directory, &[(0, b"aaaaaaaa")]);
    let journal_path = directory.journal();
    append_record(
        &journal_path,
        FramedRecord::new(
            1,
            JournalRecord::BlockComplete {
                offset: 4,
                len: 8,
                blake3: [0x11; 32],
            },
        ),
    );
    let bytes = fs::read(&journal_path).unwrap();

    let mut store = open_store(&directory, &part_path);
    let reconciled = reconcile_download(&mut store, sample_id(), &journal_path, NOW_MS)
        .expect("an impossible journal fails the download, not the daemon");

    assert_eq!(reconciled.state(), DownloadState::Failed);
    assert_eq!(
        reconciled.error_kind().map(|kind| kind.as_str()),
        Some("storage.journal-invalid")
    );
    assert_eq!(
        fs::read(&journal_path).unwrap(),
        bytes,
        "the journal is evidence for dp repair, not garbage"
    );
}

/// I-8: reconciliation rewrites progress, never the identity a resume depends on.
///
/// Recomputing the identity from the original URL on resume is how signed URLs and
/// session-bound CDNs turn into a 403 and a restart from zero.
#[test]
fn reconciliation_preserves_the_recorded_identity() {
    let directory = TestDirectory::new("identity");
    let part_path = commit_durable_blocks(&directory, &[(0, b"aaaaaaaa")]);
    let mut store = open_store(&directory, &part_path);
    let before = store.load_download(sample_id()).unwrap().unwrap();

    reconcile_download(&mut store, sample_id(), &directory.journal(), NOW_MS)
        .expect("reconciliation succeeds");

    let after = store.load_download(sample_id()).unwrap().unwrap();
    assert_eq!(after.identity, before.identity);
    assert_eq!(after.url_history, before.url_history);
    assert_eq!(after.target_path, before.target_path);
    assert_eq!(after.part_path, before.part_path);
    assert_eq!(after.created_at_ms, before.created_at_ms);
    assert_eq!(after.priority, before.priority);
    assert_eq!(after.queue_position, before.queue_position);
}

/// B-37 at the layer that would act on it: reconciliation refuses a linked part path.
///
/// `PartFile::open_existing` refuses the link itself, but recovery is what decides whether a
/// download is resumable, and a download it declares resumable is one a daemon will write into.
/// So the refusal has to arrive as a stable state and error kind rather than as a panic or an
/// I/O error indistinguishable from a transient one — and the victim has to still be there.
#[test]
#[cfg(unix)]
fn a_download_whose_part_path_is_a_symlink_is_failed_and_the_target_is_untouched() {
    let directory = TestDirectory::new("symlinked-part-path");
    let part_path = commit_durable_blocks(&directory, &[(0, b"aaaaaaaa")]);
    let mut store = open_store(&directory, &part_path);

    // Stand in the way the hazard actually arrives: the daemon is down, and between its death and
    // its restart the part file is replaced by a link to something else.
    let victim_path = directory.path().join("something-the-user-cares-about");
    let victim_bytes = b"not this download's to overwrite".to_vec();
    fs::write(&victim_path, &victim_bytes).expect("write the victim");
    fs::remove_file(&part_path).expect("remove the real part file");
    std::os::unix::fs::symlink(&victim_path, &part_path).expect("plant the symlink");

    let reconciliation = reconcile_download(&mut store, sample_id(), &directory.journal(), NOW_MS)
        .expect("reconciliation must reach a decision rather than fail to run");

    assert_eq!(reconciliation.state(), DownloadState::Failed);
    assert_eq!(
        reconciliation
            .error_kind()
            .map(downpour_storage::metadata::DownloadErrorKind::as_str),
        Some("storage.part-file-not-regular"),
        "the refusal needs a stable kind a client can switch on"
    );
    assert_eq!(
        fs::read(&victim_path).expect("the victim is still readable"),
        victim_bytes,
        "recovery wrote through the symlink"
    );
    assert!(
        fs::symlink_metadata(&part_path)
            .expect("the link is still there")
            .file_type()
            .is_symlink(),
        "the link itself is evidence and is left alone"
    );
}

/// The presence check answers about the part file, not about whatever the path leads to.
///
/// `exists()` follows links and cannot distinguish these two. A dangling symlink reads as absent,
/// so the download is failed as "missing" and a later create would follow the link and write
/// through it — the same family as B-30 and B-37 reached from a third side. A directory reads as
/// present, so recovery goes on to open it and reports whatever errno that produced, which is
/// indistinguishable from a transient fault. Both are "something is at this path and it is not
/// this download's part file", and both must say so.
#[test]
#[cfg(unix)]
fn a_part_path_that_is_a_dangling_link_or_a_directory_is_refused_by_name() {
    for (tag, plant) in [("dangling-link", 0_u8), ("directory", 1)] {
        let directory = TestDirectory::new(tag);
        let part_path = commit_durable_blocks(&directory, &[(0, b"aaaaaaaa")]);
        let mut store = open_store(&directory, &part_path);
        fs::remove_file(&part_path).expect("remove the real part file");
        if plant == 0 {
            std::os::unix::fs::symlink(directory.path().join("nothing-here"), &part_path)
                .expect("plant a dangling symlink");
        } else {
            fs::create_dir(&part_path).expect("plant a directory");
        }

        let reconciliation =
            reconcile_download(&mut store, sample_id(), &directory.journal(), NOW_MS)
                .expect("reconciliation must reach a decision rather than fail to run");

        assert_eq!(reconciliation.state(), DownloadState::Failed, "{tag}");
        assert_eq!(
            reconciliation
                .error_kind()
                .map(downpour_storage::metadata::DownloadErrorKind::as_str),
            Some("storage.part-file-not-regular"),
            "{tag}: the refusal must name what is wrong, not report an opaque I/O failure"
        );
    }
}
