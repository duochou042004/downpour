//! B-25 — a segmented transfer resumes from durable evidence after the process that started it
//! is gone.
//!
//! Nothing here is carried in memory between the two halves. The first half writes durable
//! artifacts and drops every handle; the second half is given only the recorded identity and the
//! storage layout, exactly what a daemon would hold after a restart. What it must not do is ask
//! the server for a byte the journal already proved, because on a metered connection that is the
//! user's money, and because re-fetching a range that is already `Complete` means the allocator
//! and the journal disagree about what is durable.

use std::path::Path;
use std::sync::Arc;

use downpour_corpus::content::Content;
use downpour_corpus::server::{PathologyServer, ServerSpec};
use downpour_engine::{SegmentedDownload, StorageLayout};
use downpour_http::{H1H2Backend, ProbeRequest, TransferProtocol, TransportMode};
use downpour_intervals::{IntervalMap, WorkerId};
use downpour_storage::journal::FileHeader;
use downpour_storage::part_file::PartFile;
use downpour_storage::writer::{DurableWriter, JournalFile};

/// Four workers need four grants at or above `DEFAULT_MIN_SPLIT_BYTES` (1 MiB), so the smallest
/// representation this pool divides four ways is 8 MiB.
const LENGTH: u64 = 8 * 1024 * 1024;
const SEED: u64 = 91;
const RECOVERY_WORKER: WorkerId = WorkerId::new(u64::MAX);

struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = std::env::temp_dir().join(format!("downpour-resume-{tag}-{unique}"));
        std::fs::create_dir_all(path.join("target")).expect("create target dir");
        std::fs::create_dir_all(path.join("journals")).expect("create journal dir");
        Self(path)
    }

    fn layout(&self) -> StorageLayout {
        StorageLayout::new(self.0.join("target"), self.0.join("journals"))
    }

    fn target_dir(&self) -> std::path::PathBuf {
        self.0.join("target")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Write the durable artifacts a previous process would have left behind, then drop every handle.
///
/// The prefix is committed through the real `DurableWriter`, so the journal records are the ones
/// replay will read rather than a fixture's idea of them.
fn leave_a_half_finished_download(
    layout: &StorageLayout,
    final_path: &Path,
    transfer_id: [u8; 16],
    validator_hash: [u8; 32],
    durable: &[(u64, u64)],
) {
    let content = Content::new(SEED, LENGTH);
    let part = PartFile::create(final_path, LENGTH).expect("create the part file");
    let journal_path = layout
        .journal_dir()
        .join(format!("{}.dpj", hex(&transfer_id)));
    let journal = JournalFile::create(
        &journal_path,
        FileHeader::new(transfer_id, LENGTH, 0, validator_hash),
    )
    .expect("create the journal");
    let mut writer = DurableWriter::try_new(part, journal, 0).expect("start the writer");
    let mut intervals = IntervalMap::new(LENGTH);

    for (start, end) in durable {
        intervals
            .grant(*start..*end, RECOVERY_WORKER)
            .expect("grant the durable range");
        let bytes = content.range(*start, *end);
        writer
            .stage(
                &mut intervals,
                RECOVERY_WORKER,
                *start,
                &bytes,
                std::time::Duration::ZERO,
            )
            .expect("stage the durable range");
        writer.flush(&mut intervals).expect("commit the range");
    }
    drop(writer);
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resumed_segmented_transfer_never_refetches_a_byte_the_journal_proved() {
    let server = PathologyServer::start(ServerSpec {
        content: Content::new(SEED, LENGTH),
        etag: Some("\"resume-v1\"".to_owned()),
        ..ServerSpec::default()
    })
    .await
    .expect("server starts");

    // The recorded identity, obtained the way a first session would and then kept. Resume does not
    // re-probe: the validator that matters is the one recorded when the existing bytes were
    // fetched, and a fresh probe would replace it with one that trivially matches whatever the
    // server is serving now — which is exactly the check I-3 asks for, thrown away.
    let backend = Arc::new(H1H2Backend::new(TransportMode::Http1Only).expect("backend"));
    let remote = backend
        .probe(ProbeRequest::new(server.entry_url().parse().expect("url")))
        .await
        .expect("probe");

    let scratch = Scratch::new("half-finished");
    let layout = scratch.layout();
    let final_path = scratch.target_dir().join(
        remote
            .suggested_filename
            .clone()
            .unwrap_or_else(|| "download".to_owned()),
    );

    // Two durable runs with a hole between them and a hole at the end: the shape a killed
    // multi-worker transfer leaves, not a single truncated prefix.
    let durable: &[(u64, u64)] = &[(0, 2 * 1024 * 1024), (4 * 1024 * 1024, 6 * 1024 * 1024)];
    leave_a_half_finished_download(
        &layout,
        &final_path,
        downpour_engine::transfer_id_for(&remote.final_url),
        downpour_engine::validator_hash_of(&remote.validator),
        durable,
    );

    let requests_before = server.requests().len();
    let resumed = SegmentedDownload::new(Arc::clone(&backend), 4)
        .resume(&remote, &layout)
        .await
        .expect("the half-finished download resumes");

    assert_eq!(resumed, final_path, "resume must land on the recorded name");
    let bytes = std::fs::read(&final_path).expect("the finished file is readable");
    assert_eq!(
        u64::try_from(bytes.len()).expect("length fits u64"),
        LENGTH,
        "the resumed file has the wrong length"
    );
    assert_eq!(
        Content::new(SEED, LENGTH).first_mismatch(0, &bytes),
        None,
        "silent corruption through the resumed segmented path"
    );

    // The claim. Every range the resume asked for must fall entirely outside what the journal
    // already proved.
    let ranged: Vec<(u64, u64)> = server.requests()[requests_before..]
        .iter()
        .filter_map(|request| request.header("range"))
        .filter_map(|value| parse_range(&value))
        .collect();
    assert!(
        !ranged.is_empty(),
        "the resume must have asked for the missing bytes"
    );
    for (start, end) in &ranged {
        for (done_start, done_end) in durable {
            assert!(
                start >= done_end || done_start >= end,
                "resume requested {start}-{end}, overlapping the durable range \
                 {done_start}-{done_end} the journal already proved"
            );
        }
    }

    // And every ranged request stays conditional on the recorded representation (I-3): resume is
    // the case where splicing two versions is actually reachable.
    for request in &server.requests()[requests_before..] {
        if request.header("range").is_some() {
            assert_eq!(
                request.header("if-range").as_deref(),
                Some("\"resume-v1\""),
                "a resumed range must be conditional on the recorded validator"
            );
        }
    }
}

fn parse_range(value: &str) -> Option<(u64, u64)> {
    let rest = value.strip_prefix("bytes=")?;
    let (first, last) = rest.split_once('-')?;
    Some((first.parse().ok()?, last.parse::<u64>().ok()? + 1))
}
