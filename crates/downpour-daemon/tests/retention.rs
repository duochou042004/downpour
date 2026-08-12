//! S3-T13 — the retention policy of docs/04 §8, and the line it must not cross.
//!
//! The table in §8 is short and the sentence under it is shorter: **Downpour does not delete user
//! data on its own initiative.** So this is not a garbage collector. It removes exactly one kind
//! of thing — a recovery journal that can no longer protect anything — and it is written to be
//! read as a list of everything it refuses to touch.
//!
//! What makes that worth testing rather than asserting is B-30. A failed download keeps its
//! journal deliberately, because it is the evidence a resume is built from; the first version of
//! that reasoning also kept every *completed* download's journal forever, which is how a home
//! directory fills up with files that protect nothing. Both halves are one decision, and getting
//! either wrong is a real fault: delete too much and a resumable download is destroyed, delete
//! too little and the daemon leaks a file per download for the life of the installation.
//!
//! Every test here is deterministic. Ages come from `File::set_modified` rather than from
//! sleeping, and `now_ms` is passed in rather than read from the clock, so a failure names the
//! rule that broke instead of the machine it ran on.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use downpour_daemon::retention::{RetentionPolicy, sweep_journals};
use downpour_storage::journal::FileHeader;
use downpour_storage::metadata::{
    DownloadId, DownloadMetadata, DownloadState, IdentityMetadata, MetadataStore, PublicUrl,
    UrlHistoryEntry, UrlReference,
};
use downpour_storage::part_file::PartFile;
use downpour_storage::writer::JournalFile;
use downpour_types::{ByteRangeSpec, NegotiatedProtocol, RangeProof, RangeSupport, Validator};

static NEXT: AtomicU64 = AtomicU64::new(0);
const TOTAL: u64 = 32;
const NOW_MS: u64 = 1_780_000_000_000;
/// Comfortably past the policy's floor, so "old" is unambiguous.
const LONG_AGO: Duration = Duration::from_secs(30 * 24 * 60 * 60);

struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Self {
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "downpour-retention-{tag}-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir_all(path.join("journals")).expect("journal directory");
        fs::create_dir_all(path.join("data")).expect("data directory");
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
        let _ = fs::remove_dir_all(&self.0);
    }
}

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

/// A real journal file on disk for `id`, aged `age` before `NOW_MS`.
fn journal(dir: &Dir, id: DownloadId, age: Duration) -> PathBuf {
    let path = journal_path(dir, id);
    JournalFile::create(&path, FileHeader::new(id.as_bytes(), TOTAL, 0, [0x62; 32]))
        .expect("journal");
    age_file(&path, age);
    path
}

/// Move a file's modification time into the past, deterministically.
///
/// `File::set_modified` rather than sleeping: an age-based rule tested by waiting is a rule
/// tested by the machine's load average.
fn age_file(path: &Path, age: Duration) {
    let handle = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open to age");
    let when = SystemTime::UNIX_EPOCH + Duration::from_millis(NOW_MS) - age;
    handle.set_modified(when).expect("set the modification time");
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

fn part(dir: &Dir, name: &str) -> PathBuf {
    let target = dir.data().join(name);
    let file = PartFile::create(&target, TOTAL).expect("part file");
    file.path().to_path_buf()
}

/// The named proof: what §8 removes, and everything it must not.
///
/// One sweep over eight downloads, because the rules only mean anything together. A policy that
/// deletes completed journals is trivial to write and trivially wrong if it also touches the
/// failed one beside it, and a run that only ever sees one download at a time cannot tell.
#[test]
fn only_journals_that_can_protect_nothing_are_removed() {
    let dir = Dir::new("mixed");
    let mut store = MetadataStore::open(dir.db()).expect("empty database is v1");

    // Terminal and verified: §8 says the journal goes.
    let completed = journal(&dir, id(0x01), LONG_AGO);
    // Terminal and NOT verified: §8 says the journal is kept, because it is what a resume needs.
    let failed = journal(&dir, id(0x02), LONG_AGO);
    // Everything unfinished, however long ago it was touched.
    let paused = journal(&dir, id(0x03), LONG_AGO);
    let transferring = journal(&dir, id(0x04), LONG_AGO);
    let stalled = journal(&dir, id(0x05), LONG_AGO);
    let awaiting = journal(&dir, id(0x06), LONG_AGO);
    // No row at all, and old: nothing in the store can ever resume from it.
    let orphan_old = journal(&dir, id(0x07), LONG_AGO);
    // No row at all, and new: a download being added right now looks exactly like this.
    let orphan_new = journal(&dir, id(0x08), Duration::from_secs(0));

    for (n, state) in [
        (0x01, DownloadState::Completed),
        (0x02, DownloadState::Failed),
        (0x03, DownloadState::Paused),
        (0x04, DownloadState::Transferring),
        (0x05, DownloadState::Stalled),
        (0x06, DownloadState::AwaitingRefresh),
    ] {
        let part = part(&dir, &format!("file-{n:02x}.bin"));
        store
            .save_download(&metadata(&dir, id(n), &part, state))
            .expect("save");
    }

    let summary = sweep_journals(
        &store,
        &dir.journals(),
        RetentionPolicy::default(),
        NOW_MS,
    )
    .expect("the sweep runs");

    assert!(!completed.exists(), "a completed download's journal protects nothing and §8 deletes it");
    assert!(!orphan_old.exists(), "a journal with no download to belong to protects nothing");

    assert!(failed.exists(), "a failed download's journal is the evidence a resume is built from");
    assert!(paused.exists(), "a paused download is resumable and its journal must survive");
    assert!(transferring.exists(), "a live download's journal must survive");
    assert!(stalled.exists(), "a stalled download is still going and its journal must survive");
    assert!(awaiting.exists(), "a download waiting for a URL is resumable and must survive");
    assert!(
        orphan_new.exists(),
        "a journal younger than the floor may belong to a download being added right now"
    );

    assert_eq!(summary.removed(), 2, "exactly the two that protect nothing");
    assert_eq!(summary.kept(), 6);
}

/// No part file is ever removed, whatever else is true of it.
///
/// §8's last row and the sentence under the table: an orphaned `.dppart` is *surfaced*, never
/// deleted. It is the user's bytes — possibly the only copy of a download they spent an hour on —
/// and the daemon does not get to decide it is rubbish.
#[test]
fn a_sweep_never_removes_a_part_file() {
    let dir = Dir::new("parts");
    let store = MetadataStore::open(dir.db()).expect("empty database is v1");

    // Every part file here is orphaned: the store is empty, so not one of them has a row.
    let orphans = ["a.bin", "b.bin", "c.bin"].map(|name| part(&dir, name));
    let journal = journal(&dir, id(0x01), LONG_AGO);

    let summary = sweep_journals(
        &store,
        &dir.journals(),
        RetentionPolicy::default(),
        NOW_MS,
    )
    .expect("the sweep runs");

    assert!(!journal.exists(), "the orphaned journal should have gone");
    for orphan in &orphans {
        assert!(
            orphan.exists(),
            "{} was removed; §8 surfaces an orphaned part file and never deletes it",
            orphan.display()
        );
    }
    assert_eq!(summary.removed(), 1);
}

/// The floor is a floor: an orphan is removed once it is old enough and not before.
///
/// The window matters because the store and the journal are written by different steps. A
/// download being created has a journal on disk before its row is committed, so a sweep with no
/// floor races every `download.add` and can delete the journal of a transfer that is about to
/// start.
#[test]
fn an_orphan_survives_until_it_is_older_than_the_floor() {
    let dir = Dir::new("floor");
    let store = MetadataStore::open(dir.db()).expect("empty database is v1");
    let policy = RetentionPolicy::default();

    let orphan = journal(&dir, id(0x01), policy.minimum_age() / 2);
    let summary = sweep_journals(&store, &dir.journals(), policy, NOW_MS).expect("sweep");
    assert!(
        orphan.exists(),
        "an orphan half the floor's age was removed; the floor is what makes the sweep safe \
         against a download being added right now"
    );
    assert_eq!(summary.removed(), 0);

    // The same file, now past the floor. Nothing else about it changed.
    age_file(&orphan, policy.minimum_age() + Duration::from_secs(1));
    let summary = sweep_journals(&store, &dir.journals(), policy, NOW_MS).expect("sweep");
    assert!(!orphan.exists(), "an orphan past the floor should have been removed");
    assert_eq!(summary.removed(), 1);
}

/// A file in the journal directory that is not a journal is left alone.
///
/// The daemon owns the directory but not everything that may end up in it — an editor's backup, a
/// half-copied file, a user's note. Removing "everything that is not a known journal" is how a
/// cleanup routine becomes the thing that loses data.
#[test]
fn a_sweep_leaves_alone_what_it_does_not_recognise() {
    let dir = Dir::new("strangers");
    let store = MetadataStore::open(dir.db()).expect("empty database is v1");

    let note = dir.journals().join("notes.txt");
    fs::write(&note, b"not a journal").expect("write");
    age_file(&note, LONG_AGO);
    let almost = dir.journals().join("not-hex.dpj");
    fs::write(&almost, b"not a journal either").expect("write");
    age_file(&almost, LONG_AGO);

    let summary = sweep_journals(
        &store,
        &dir.journals(),
        RetentionPolicy::default(),
        NOW_MS,
    )
    .expect("the sweep runs");

    assert!(note.exists(), "a file that is not a journal was removed");
    assert!(
        almost.exists(),
        "a .dpj whose name is not a download id was removed; the daemon cannot know whose it is"
    );
    assert_eq!(summary.removed(), 0);
}
