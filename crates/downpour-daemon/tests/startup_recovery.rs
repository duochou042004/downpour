//! S2-T14 — startup recovery. The daemon half of **I-12**.
//!
//! The daemon outliving every UI only means something if it can also outlive *itself*. A machine
//! that lost power mid-transfer has to come back to a state that is honest about what it holds,
//! and it has to do so for every download rather than the first one that happens to be intact.
//!
//! docs/04 §5 step (f) is the rule these tests exist to pin: **nothing is resumed.** Auto-starting
//! ten transfers on boot, when the user may be on a metered connection or may have wanted none of
//! them, is how a download manager gets uninstalled.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use downpour_daemon::startup::recover_all;
use downpour_intervals::{IntervalMap, WorkerId};
use downpour_storage::journal::FileHeader;
use downpour_storage::metadata::{
    DownloadId, DownloadMetadata, DownloadState, IdentityMetadata, MetadataStore, PublicUrl,
    UrlHistoryEntry, UrlReference,
};
use downpour_storage::part_file::PartFile;
use downpour_storage::writer::{DurableWriter, JournalFile};
use downpour_types::{ByteRangeSpec, NegotiatedProtocol, RangeProof, RangeSupport, Validator};

static NEXT: AtomicU64 = AtomicU64::new(0);
const TOTAL: u64 = 32;
const NOW_MS: u64 = 1_780_000_000_000;

struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Self {
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "downpour-startup-{tag}-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir_all(path.join("journals")).unwrap();
        fs::create_dir_all(path.join("data")).unwrap();
        Self(path)
    }
    fn db(&self) -> PathBuf {
        self.0.join("downpour.db")
    }
    fn journals(&self) -> PathBuf {
        self.0.join("journals")
    }
    fn data(&self) -> PathBuf {
        self.0.join("data")
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

/// A valid UUIDv7 that differs per `n`, so a test can hold several downloads at once.
fn id(n: u8) -> DownloadId {
    DownloadId::try_from_bytes([
        0x01, 0x91, 0x23, 0x45, 0x67, 0x89, 0x7a, 0xbc, 0x8d, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, n,
    ])
    .expect("fixture is RFC-variant UUIDv7")
}

fn journal_path(dir: &Dir, id: DownloadId) -> PathBuf {
    let mut name = String::new();
    for byte in id.as_bytes() {
        name.push_str(&format!("{byte:02x}"));
    }
    name.push_str(".dpj");
    dir.journals().join(name)
}

fn metadata(dir: &Dir, id: DownloadId, part: &Path, state: DownloadState) -> DownloadMetadata {
    let url = UrlReference::parse("https://example.test/file", None).expect("public");
    DownloadMetadata {
        id,
        state,
        created_at_ms: 1,
        updated_at_ms: 2,
        target_path: dir.data().join("payload.bin"),
        part_path: part.to_path_buf(),
        total_length: Some(TOTAL),
        covered_bytes: 0,
        queue_position: None,
        priority: 0,
        error_kind: None,
        space_reserved: true,
        identity: IdentityMetadata {
            current_url: url.clone(),
            final_url: None,
            redirect_chain: vec![url.clone()],
            page_url: None,
            origin: PublicUrl::parse("https://example.test/").expect("public"),
            validator: Validator::StrongETag("\"v1\"".to_owned()),
            server_digest: None,
            content_type: None,
            suggested_filename: None,
            request_context_ref: None,
            probed_at_ms: 3,
            protocol: NegotiatedProtocol::Http11,
            range_support: RangeSupport::Proven(
                RangeProof::from_observed_response(
                    ByteRangeSpec::FromTo { first: 0, last: 0 },
                    206,
                    Some("bytes 0-0/32"),
                    None,
                    1,
                )
                .expect("valid observed range response"),
            ),
        },
        url_history: vec![UrlHistoryEntry { url, seen_at_ms: 2 }],
    }
}

/// Durable artifacts holding `bytes` at offset 0, as a crashed transfer would leave them.
fn artifacts(dir: &Dir, id: DownloadId, name: &str, bytes: &[u8]) -> PathBuf {
    let target = dir.data().join(name);
    let part = PartFile::create(&target, TOTAL).expect("part file");
    let part_path = part.path().to_path_buf();
    let journal = JournalFile::create(
        journal_path(dir, id),
        FileHeader::new(id.as_bytes(), TOTAL, 0, [0x62; 32]),
    )
    .expect("journal");
    let mut writer = DurableWriter::try_new(part, journal, 0).expect("writer");
    let mut intervals = IntervalMap::new(TOTAL);
    let worker = WorkerId::new(1);
    let end = u64::try_from(bytes.len()).expect("fits");
    intervals.grant(0..end, worker).expect("grant");
    writer
        .stage(&mut intervals, worker, 0, bytes, Duration::from_secs(1))
        .expect("stage");
    writer.flush(&mut intervals).expect("flush");
    part_path
}

/// The named proof for S2-T14.
///
/// Three downloads: two left mid-transfer by the crash, one already completed. Both unclean ones
/// must come back reconciled and **Paused**; the completed one must not be touched at all.
#[test]
fn unclean_downloads_are_reconciled_and_left_paused() {
    let dir = Dir::new("mixed");
    let mut store = MetadataStore::open(dir.db()).expect("empty database is v1");

    let first = artifacts(&dir, id(0x01), "first.bin", b"aaaaaaaa");
    let second = artifacts(&dir, id(0x02), "second.bin", b"bbbbbbbbbbbbbbbb");
    let done = artifacts(&dir, id(0x03), "third.bin", b"cccc");

    store
        .save_download(&metadata(
            &dir,
            id(0x01),
            &first,
            DownloadState::Transferring,
        ))
        .expect("save");
    store
        .save_download(&metadata(&dir, id(0x02), &second, DownloadState::Stalled))
        .expect("save");
    store
        .save_download(&metadata(&dir, id(0x03), &done, DownloadState::Completed))
        .expect("save");

    let summary = recover_all(&mut store, &dir.journals(), NOW_MS).expect("the store is usable");

    assert_eq!(summary.recovered(), 2, "both unclean downloads come back");
    assert_eq!(summary.failed(), 0);
    assert_eq!(
        summary.skipped(),
        1,
        "a completed download has nothing in flight and is not touched"
    );

    for expected in [(id(0x01), 8_u64), (id(0x02), 16_u64)] {
        let persisted = store
            .load_download(expected.0)
            .expect("readable")
            .expect("kept");
        assert_eq!(
            persisted.state,
            DownloadState::Paused,
            "docs/04 §5 step (f): recovery never auto-resumes"
        );
        assert_eq!(
            persisted.covered_bytes, expected.1,
            "coverage comes from the journal, not from what SQLite last remembered"
        );
    }

    // The completed download is left exactly as it was, including its state.
    assert_eq!(
        store
            .load_download(id(0x03))
            .expect("readable")
            .expect("kept")
            .state,
        DownloadState::Completed
    );
}

/// One unrecoverable download does not cost the user the others.
///
/// A corrupt journal is that download's problem. Taking the daemon down with it would turn a
/// recoverable fault into an outage, and nine healthy transfers would be collateral.
#[test]
fn one_unrecoverable_download_does_not_stop_the_rest() {
    let dir = Dir::new("partial-failure");
    let mut store = MetadataStore::open(dir.db()).expect("v1");

    let healthy = artifacts(&dir, id(0x01), "healthy.bin", b"aaaaaaaa");
    let broken = artifacts(&dir, id(0x02), "broken.bin", b"bbbbbbbb");
    // Its part file is gone: the bytes cannot be recovered, whatever the journal says.
    fs::remove_file(&broken).expect("remove the part file");

    store
        .save_download(&metadata(
            &dir,
            id(0x01),
            &healthy,
            DownloadState::Transferring,
        ))
        .expect("save");
    store
        .save_download(&metadata(
            &dir,
            id(0x02),
            &broken,
            DownloadState::Transferring,
        ))
        .expect("save");

    let summary = recover_all(&mut store, &dir.journals(), NOW_MS).expect("the pass completes");

    assert_eq!(summary.downloads().len(), 2);
    assert_eq!(
        summary.recovered(),
        1,
        "the healthy download still recovers"
    );
    assert_eq!(summary.failed(), 1);

    assert_eq!(
        store.load_download(id(0x01)).unwrap().unwrap().state,
        DownloadState::Paused
    );
    let broken_record = store.load_download(id(0x02)).unwrap().unwrap();
    assert_eq!(
        broken_record.state,
        DownloadState::Failed,
        "the record is kept and marked, not deleted"
    );
    assert_eq!(
        broken_record
            .error_kind
            .map(|kind| kind.as_str().to_owned()),
        Some("storage.part-file-missing".to_owned())
    );
}

/// A download whose recovery *errors* also does not stop the pass.
///
/// Distinct from the case above, which fails recoverably — a missing part file produces a
/// reconciled `Failed` record, which is a normal outcome. This one makes `reconcile_download`
/// return an error outright, by putting a directory where its journal should be. That is the
/// branch a daemon would hit on a genuinely broken filesystem, and it is the one that would take
/// every other transfer down with it if the pass propagated instead of recording.
#[test]
fn a_download_whose_recovery_errors_does_not_stop_the_pass() {
    let dir = Dir::new("hard-failure");
    let mut store = MetadataStore::open(dir.db()).expect("v1");

    let healthy = artifacts(&dir, id(0x01), "healthy.bin", b"aaaaaaaa");
    let cursed = artifacts(&dir, id(0x02), "cursed.bin", b"bbbbbbbb");
    // A directory where the journal file belongs: opening it fails at the OS level.
    let journal = journal_path(&dir, id(0x02));
    fs::remove_file(&journal).expect("remove the journal");
    fs::create_dir(&journal).expect("put a directory in its place");

    store
        .save_download(&metadata(
            &dir,
            id(0x01),
            &healthy,
            DownloadState::Transferring,
        ))
        .expect("save");
    store
        .save_download(&metadata(
            &dir,
            id(0x02),
            &cursed,
            DownloadState::Transferring,
        ))
        .expect("save");

    let summary = recover_all(&mut store, &dir.journals(), NOW_MS)
        .expect("a broken journal is one download's problem, not the daemon's");

    assert_eq!(summary.downloads().len(), 2);
    assert_eq!(
        summary.recovered(),
        1,
        "the healthy download still recovers"
    );
    assert!(
        summary
            .downloads()
            .iter()
            .any(|download| download.outcome().is_err()),
        "the failure must be recorded in the summary rather than swallowed"
    );
    assert_eq!(
        store.load_download(id(0x01)).unwrap().unwrap().state,
        DownloadState::Paused
    );
}

/// An empty store is a normal first start, not an error.
#[test]
fn a_store_with_nothing_in_it_recovers_nothing_and_succeeds() {
    let dir = Dir::new("empty");
    let mut store = MetadataStore::open(dir.db()).expect("v1");

    let summary = recover_all(&mut store, &dir.journals(), NOW_MS).expect("a first start works");

    assert_eq!(summary.downloads().len(), 0);
    assert_eq!(summary.recovered(), 0);
    assert_eq!(summary.skipped(), 0);
}
