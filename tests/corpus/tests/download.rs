//! S1-T9 — the single-stream download: `.dppart`, verify, atomic rename.
//!
//! Exit criteria S1-C4 and S1-C6. The assertion that appears in almost every test here is
//! **I-4**: the final filename exists only when the file behind it is complete and verified. A
//! partial file wearing the final name is indistinguishable from a good one to the user and to
//! every other program on the system, which is what makes it worse than an obvious failure.
//!
//! Corruption is checked by byte comparison against the generator, never by a checksum of our
//! own making (`docs/09-testing-strategy.md` §7 rule 5).

use std::path::Path;

use downpour_corpus::content::Content;
use downpour_corpus::server::{Framing, PathologyServer, Protocol, RangeBehaviour, ServerSpec};
use downpour_engine::{DownloadError, SingleStream, StorageLayout};
use downpour_http::{H1H2Backend, TransportMode};
use url::Url;

const SIZE: u64 = 512 * 1024;

fn spec() -> ServerSpec {
    ServerSpec {
        content: Content::new(42, SIZE),
        ..ServerSpec::default()
    }
}

fn backend(mode: TransportMode) -> H1H2Backend {
    H1H2Backend::new(mode).expect("the h1h2 backend builds")
}

/// A scratch area that cleans itself up.
///
/// Data and recovery state are separate directories, matching docs/04 §1: the journal lives with
/// application state rather than beside the download, so clearing a downloads folder cannot
/// silently destroy the evidence a resume depends on. Keeping them apart here also means
/// [`Self::entries`] still answers "what did the user end up with", which is what the I-4
/// assertions in this file are about.
struct Scratch {
    root: std::path::PathBuf,
    data: std::path::PathBuf,
    journals: std::path::PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!("downpour-test-{tag}-{unique}"));
        let data = root.join("data");
        let journals = root.join("journals");
        std::fs::create_dir_all(&data).expect("create the scratch data directory");
        std::fs::create_dir_all(&journals).expect("create the scratch journal directory");
        Self {
            root,
            data,
            journals,
        }
    }

    fn path(&self) -> &Path {
        &self.data
    }

    fn layout(&self) -> StorageLayout {
        StorageLayout::new(&self.data, &self.journals)
    }

    /// Every entry in the data directory, sorted, so a test can assert on what exists.
    fn entries(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.data)
            .expect("read the scratch directory")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// The bytes of the single recovery journal, when one was written.
    fn journal_bytes(&self) -> Option<Vec<u8>> {
        let mut found: Vec<std::path::PathBuf> = std::fs::read_dir(&self.journals)
            .expect("read the journal directory")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("dpj"))
            .collect();
        found.sort();
        match found.as_slice() {
            [] => None,
            [one] => Some(std::fs::read(one).expect("read the journal")),
            many => panic!("expected at most one journal, found {}", many.len()),
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn run(
    server: &PathologyServer,
    scratch: &Scratch,
    mode: TransportMode,
) -> Result<std::path::PathBuf, DownloadError> {
    let url = Url::parse(&server.entry_url()).expect("the server URL is well formed");
    SingleStream::new(backend(mode))
        .download(url, &scratch.layout())
        .await
}

// ---------------------------------------------------------------- the happy path

#[tokio::test]
async fn a_complete_download_is_byte_exact_and_ends_up_at_its_final_name() {
    let server = PathologyServer::start(spec()).await.expect("server starts");
    let scratch = Scratch::new("happy");

    let path = run(&server, &scratch, TransportMode::Http1Only)
        .await
        .expect("downloads");

    assert_eq!(path.file_name().and_then(|n| n.to_str()), Some("content"));
    assert_eq!(
        scratch.entries(),
        vec!["content"],
        "no .dppart may be left behind"
    );

    let bytes = std::fs::read(&path).expect("read the downloaded file");
    assert_eq!(u64::try_from(bytes.len()).expect("fits"), SIZE);
    assert_eq!(
        Content::new(42, SIZE).first_mismatch(0, &bytes),
        None,
        "silent corruption: the file differs from the generator"
    );
}

#[tokio::test]
async fn a_download_over_http2_is_byte_exact_too() {
    let server = PathologyServer::start(ServerSpec {
        protocol: Protocol::H2c,
        ..spec()
    })
    .await
    .expect("server starts");
    let scratch = Scratch::new("h2");

    let path = run(&server, &scratch, TransportMode::Http2PriorKnowledge)
        .await
        .expect("downloads over h2c");

    let bytes = std::fs::read(&path).expect("read the downloaded file");
    assert_eq!(Content::new(42, SIZE).first_mismatch(0, &bytes), None);
    assert_eq!(u64::try_from(bytes.len()).expect("fits"), SIZE);
}

#[tokio::test]
async fn chunked_framing_produces_the_same_file() {
    let server = PathologyServer::start(ServerSpec {
        framing: Framing::Chunked,
        ..spec()
    })
    .await
    .expect("server starts");
    let scratch = Scratch::new("chunked");
    let path = run(&server, &scratch, TransportMode::Http1Only)
        .await
        .expect("downloads");
    let bytes = std::fs::read(&path).expect("read");
    assert_eq!(Content::new(42, SIZE).first_mismatch(0, &bytes), None);
    assert_eq!(u64::try_from(bytes.len()).expect("fits"), SIZE);
}

#[tokio::test]
async fn a_server_without_range_support_still_downloads_in_one_stream() {
    // S3-C4's precondition and the ordinary case for a great many servers: no ranges, so no
    // segmentation, but the download must still work.
    let server = PathologyServer::start(ServerSpec {
        ranges: RangeBehaviour::Absent,
        ..spec()
    })
    .await
    .expect("server starts");
    let scratch = Scratch::new("noranges");
    let path = run(&server, &scratch, TransportMode::Http1Only)
        .await
        .expect("downloads");
    let bytes = std::fs::read(&path).expect("read");
    assert_eq!(Content::new(42, SIZE).first_mismatch(0, &bytes), None);
}

#[tokio::test]
async fn the_filename_comes_from_content_disposition_when_offered() {
    let server = PathologyServer::start(ServerSpec {
        content_disposition: Some("attachment; filename=\"report.bin\"".to_owned()),
        ..spec()
    })
    .await
    .expect("server starts");
    let scratch = Scratch::new("disposition");
    let path = run(&server, &scratch, TransportMode::Http1Only)
        .await
        .expect("downloads");
    assert_eq!(
        path.file_name().and_then(|n| n.to_str()),
        Some("report.bin")
    );
    assert_eq!(scratch.entries(), vec!["report.bin"]);
}

#[tokio::test]
async fn a_traversal_in_content_disposition_cannot_write_outside_the_target_directory() {
    // S1-C3 end to end, not just at the sanitiser: the file must land inside the directory it
    // was told to use, whatever the server suggested.
    let server = PathologyServer::start(ServerSpec {
        content_disposition: Some("attachment; filename=\"../../escaped.bin\"".to_owned()),
        ..spec()
    })
    .await
    .expect("server starts");
    let scratch = Scratch::new("traversal");
    let path = run(&server, &scratch, TransportMode::Http1Only)
        .await
        .expect("downloads");

    assert_eq!(
        path.parent(),
        Some(scratch.path()),
        "the file escaped its directory"
    );
    assert_eq!(scratch.entries(), vec!["escaped.bin"]);
}

// ---------------------------------------------------------------- I-4: never rename early

#[tokio::test]
async fn a_truncated_body_does_not_produce_a_file_with_the_final_name() {
    // The failure this prevents is the quiet one: a short file wearing the real filename, which
    // nothing on the system can tell from a good one.
    let server = PathologyServer::start(ServerSpec {
        truncate_body_after: Some(4096),
        ..spec()
    })
    .await
    .expect("server starts");
    let scratch = Scratch::new("truncated");

    let error = run(&server, &scratch, TransportMode::Http1Only)
        .await
        .expect_err("a truncated download must not succeed");
    // Reported as a truncation, not as a generic transport failure. docs/03 §7 gives truncation
    // its own retry policy — return the unwritten remainder, count it against the budget — and
    // that decision needs to know how much arrived.
    assert_eq!(error.kind(), "truncated_body");

    let entries = scratch.entries();
    assert!(
        !entries.contains(&"content".to_owned()),
        "the final name must not exist: {entries:?}"
    );
    assert_eq!(
        entries,
        vec!["content.dppart"],
        "the partial file is kept, under the .dppart name, so a later resume can use it"
    );
}

#[tokio::test]
async fn a_server_that_lies_about_content_length_is_caught_before_any_file_exists() {
    // This server declares a length longer than what it sends on *every* response, including the
    // one-byte probe. So the probe is where it is caught — which is the earliest and safest place,
    // since nothing has been created yet. Written this way rather than asserting a transfer-level
    // failure because that is what actually happens, and a test that claimed otherwise would be
    // describing an engine that does not exist.
    let server = PathologyServer::start(ServerSpec {
        framing: Framing::WrongContentLength {
            declared: SIZE + 4096,
        },
        ..spec()
    })
    .await
    .expect("server starts");
    let scratch = Scratch::new("wronglength");

    let error = run(&server, &scratch, TransportMode::Http1Only)
        .await
        .expect_err("a body shorter than declared is not a complete download");
    assert_eq!(error.kind(), "transport");
    assert!(
        scratch.entries().is_empty(),
        "the probe failed, so no .dppart should have been created either: {:?}",
        scratch.entries()
    );
}

#[tokio::test]
async fn the_partial_file_holds_correct_bytes_so_a_resume_can_trust_them() {
    // S2 will resume from these bytes. If what landed were wrong, resume would splice correct
    // bytes onto corrupt ones and produce a file of exactly the right size.
    let server = PathologyServer::start(ServerSpec {
        truncate_body_after: Some(8192),
        ..spec()
    })
    .await
    .expect("server starts");
    let scratch = Scratch::new("partialbytes");
    let _ = run(&server, &scratch, TransportMode::Http1Only)
        .await
        .expect_err("incomplete");

    let partial = std::fs::read(scratch.path().join("content.dppart")).expect("read the partial");
    // Preallocated, so the file is full length from the start (I-10) and the bytes that arrived
    // are a correct *prefix* of it rather than the whole of it. What resume needs is that those
    // bytes are right, and that the journal — not the file length — says how far they go.
    assert_eq!(u64::try_from(partial.len()).expect("fits"), SIZE);
    assert_eq!(
        Content::new(42, SIZE).first_mismatch(0, &partial[..8192]),
        None
    );
}

// ---------------------------------------------------------------- I-5

#[tokio::test]
async fn a_compressed_body_is_never_written() {
    // S1-C4. The probe rejects it first, so nothing reaches the disk at all — which is the
    // strongest possible form of "never writes a body received with an unexpected
    // Content-Encoding".
    let server = PathologyServer::start(ServerSpec {
        content_encoding: Some("gzip".to_owned()),
        ..spec()
    })
    .await
    .expect("server starts");
    let scratch = Scratch::new("gzip");

    let error = run(&server, &scratch, TransportMode::Http1Only)
        .await
        .expect_err("a compressed body must not be written");
    assert_eq!(error.kind(), "unexpected_content_encoding");
    assert!(
        scratch.entries().is_empty(),
        "nothing at all should have been created"
    );
}

// ---------------------------------------------------------------- session pathologies

#[tokio::test]
async fn an_html_error_page_is_not_saved_as_the_file() {
    let server = PathologyServer::start(ServerSpec {
        content_type: Some("text/html; charset=utf-8".to_owned()),
        body_override: Some(b"<html>Please log in</html>".to_vec()),
        ..spec()
    })
    .await
    .expect("server starts");
    let scratch = Scratch::new("html");

    let error = run(&server, &scratch, TransportMode::Http1Only)
        .await
        .expect_err("a login page is not the file that was asked for");
    assert_eq!(error.kind(), "looks_like_error_page");
    assert!(scratch.entries().is_empty());
}

#[tokio::test]
async fn a_redirect_chain_is_followed_to_the_content() {
    let server = PathologyServer::start(ServerSpec {
        redirect_chain: vec![301, 302, 307],
        ..spec()
    })
    .await
    .expect("server starts");
    let scratch = Scratch::new("redirects");
    let path = run(&server, &scratch, TransportMode::Http1Only)
        .await
        .expect("downloads");
    let bytes = std::fs::read(&path).expect("read");
    assert_eq!(Content::new(42, SIZE).first_mismatch(0, &bytes), None);
    assert_eq!(
        path.file_name().and_then(|n| n.to_str()),
        Some("content"),
        "the name comes from the final URL, not the one that was submitted"
    );
}

// ---------------------------------------------------------------- local hazards

#[tokio::test]
async fn an_existing_file_is_not_overwritten() {
    // Refusing is the conservative choice for S1: silently replacing a file the user already has
    // is unrecoverable, and picking a "(1)" suffix is a policy decision that belongs with the
    // rest of the local-collision handling in S2.
    let server = PathologyServer::start(spec()).await.expect("server starts");
    let scratch = Scratch::new("exists");
    std::fs::write(scratch.path().join("content"), b"precious").expect("seed the target");

    let error = run(&server, &scratch, TransportMode::Http1Only)
        .await
        .expect_err("an existing file must not be replaced");
    assert_eq!(error.kind(), "target_exists");
    assert_eq!(
        std::fs::read(scratch.path().join("content")).expect("read"),
        b"precious",
        "the existing file must be untouched"
    );
}

#[tokio::test]
async fn a_zero_length_representation_still_produces_a_file() {
    let server = PathologyServer::start(ServerSpec {
        content: Content::new(1, 0),
        ..ServerSpec::default()
    })
    .await
    .expect("server starts");
    let scratch = Scratch::new("empty");
    let path = run(&server, &scratch, TransportMode::Http1Only)
        .await
        .expect("downloads");
    assert_eq!(std::fs::read(&path).expect("read").len(), 0);
    assert_eq!(scratch.entries(), vec!["content"]);
}

// ---------------------------------------------------------------- S2-T8: verified storage

/// Every byte that reaches the disk goes through `downpour-storage`, and the journal proves it.
///
/// This is the named proof for **S2-T8**. Before it, `downpour-http` wrote through a minimal
/// seek-then-write sink of its own: no preallocation, no journal, no interval map, and a
/// durability ordering that existed only as a comment. That sink is gone, and the evidence that
/// it is gone is that a successful download now leaves a recovery journal whose `BlockComplete`
/// records reconstruct the file exactly.
///
/// The digest check is the part that matters. Asserting a journal *exists* would only prove
/// something wrote a file; asserting that every recorded BLAKE3 matches the generator's bytes at
/// that offset proves the durable record and the delivered bytes are the same bytes. A sink that
/// journalled optimistically — recording ranges it had not actually written — would produce a
/// perfectly well-formed journal and fail here.
#[tokio::test]
async fn every_successful_download_uses_verified_storage() {
    use downpour_storage::journal::{JournalRecord, replay_bytes};

    let server = PathologyServer::start(ServerSpec {
        truncate_body_after: Some(64 * 1024),
        ..spec()
    })
    .await
    .expect("server starts");
    let scratch = Scratch::new("verified-storage");

    // Interrupted rather than completed, and deliberately so. A *verified* download deletes its
    // journal (docs/04 §6 step 10), so the durable record only exists to be inspected while the
    // download is still resumable — which is exactly when it matters. The claim is unchanged:
    // whatever the journal says is durable really is on disk, and really is the right bytes.
    let _ = run(&server, &scratch, TransportMode::Http1Only)
        .await
        .expect_err("the body is truncated");

    let content = Content::new(42, SIZE);
    let bytes = std::fs::read(scratch.path().join("content.dppart")).expect("read the part file");
    assert_eq!(
        scratch.entries(),
        vec!["content.dppart"],
        "the journal belongs with application state, not next to the data (docs/04 §1)"
    );

    let journal = scratch
        .journal_bytes()
        .expect("a recovery journal was written");
    let replayed = replay_bytes(&journal).expect("the journal replays");
    assert_eq!(
        replayed.header().total_length(),
        SIZE,
        "the journal is bound to the representation the probe established"
    );

    // Rebuild coverage from the durable record alone.
    let mut covered: Vec<(u64, u64)> = Vec::new();
    for framed in replayed.records() {
        if let JournalRecord::BlockComplete {
            offset,
            len,
            blake3,
        } = framed.record()
        {
            let len64 = u64::from(*len);
            assert_eq!(
                content.first_mismatch(
                    *offset,
                    &bytes[usize::try_from(*offset).expect("fits")..]
                        [..usize::try_from(len64).expect("fits")]
                ),
                None,
                "journalled range [{offset}, {}) does not match the generator",
                offset + len64
            );
            let recorded = blake3::hash(
                &bytes[usize::try_from(*offset).expect("fits")..]
                    [..usize::try_from(len64).expect("fits")],
            );
            assert_eq!(
                recorded.as_bytes(),
                blake3,
                "the journal's digest for [{offset}, {}) is not the digest of those bytes",
                offset + len64
            );
            covered.push((*offset, offset + len64));
        }
    }
    covered.sort_unstable();

    assert!(
        !covered.is_empty(),
        "the transfer recorded no blocks at all"
    );
    let mut next = 0_u64;
    for (start, end) in &covered {
        assert_eq!(*start, next, "gap or overlap in journalled coverage");
        next = *end;
    }
    assert_eq!(
        next,
        64 * 1024,
        "the journal must account for exactly the bytes that arrived — no more, which would \
         claim bytes nobody wrote, and no less, which would refetch bytes already durable"
    );
}

/// I-10: the extent is reserved before the first byte, not grown as bytes arrive.
///
/// S1's sink created an empty file and let it grow, so "disk full at 97%" was discovered at 97%.
/// Preallocating converts that into a start-time error, which is the whole point of I-10 — and
/// the observable consequence is that an interrupted download leaves a *full-length* sparse part
/// file rather than a short one.
#[tokio::test]
async fn the_part_file_is_preallocated_to_its_full_length_before_any_byte_arrives() {
    let server = PathologyServer::start(ServerSpec {
        truncate_body_after: Some(8192),
        ..spec()
    })
    .await
    .expect("server starts");
    let scratch = Scratch::new("preallocated");

    let _ = run(&server, &scratch, TransportMode::Http1Only)
        .await
        .expect_err("the body is truncated");

    let part = scratch.path().join("content.dppart");
    let metadata = std::fs::metadata(&part).expect("the part file survives for a later resume");
    assert_eq!(
        metadata.len(),
        SIZE,
        "the part file is preallocated to the representation length, not grown to what arrived"
    );
}

/// The same URL can be downloaded twice. Regression test for B-29.
///
/// S2-T8 made both durable artifacts exclusive creates and named the journal from a digest of
/// the final URL, and B-22 left the journal in place after a successful download. Together those
/// meant the *second* download of any URL collided with the first one's journal and failed —
/// user-visible as "delete the file, fetch it again, get an error". It surfaced first as an
/// intermittent CLI test failure, because ephemeral ports are recycled and a reused port
/// reproduces the same final URL.
///
/// docs/04 §6 step 10 deletes the journal once the download is verified, which is what makes the
/// exclusive create a real ownership check rather than a one-shot latch. S2-T11 made that safe:
/// before it, deleting on the strength of a byte counter is exactly what I-4 forbids.
#[tokio::test]
async fn the_same_url_can_be_downloaded_twice() {
    let server = PathologyServer::start(spec()).await.expect("server starts");
    let scratch = Scratch::new("twice");

    let first = run(&server, &scratch, TransportMode::Http1Only)
        .await
        .expect("the first download succeeds");
    assert_eq!(
        scratch.journal_bytes(),
        None,
        "a verified download deletes its journal (docs/04 §6 step 10); leaving it behind both \
         leaks state forever and makes the next download of this URL collide"
    );
    std::fs::remove_file(&first).expect("the user removes the file and fetches it again");

    let second = run(&server, &scratch, TransportMode::Http1Only)
        .await
        .expect("the second download of the same URL must also succeed");

    let bytes = std::fs::read(&second).expect("read");
    assert_eq!(Content::new(42, SIZE).first_mismatch(0, &bytes), None);
    assert_eq!(scratch.journal_bytes(), None);
}

/// A failed download does not poison its URL. Regression test for B-30.
///
/// The mirror of `the_same_url_can_be_downloaded_twice`, which covered B-29. There a
/// *successful* download's journal blocked the next attempt; here a *failed* one does, and the
/// user-facing fault is the same and worse — the download the user most wants to retry is the
/// one that failed.
///
/// A failed download keeps its journal deliberately: it is evidence, and a resume depends on it.
/// What must not happen is that evidence blocking a fresh attempt. The part file is created
/// exclusively, so getting past it proves no other owner holds this target — which makes any
/// journal still sitting there orphaned, and safe to replace.
#[tokio::test]
async fn a_failed_download_does_not_block_a_later_attempt_at_the_same_url() {
    // ONE server, so both attempts resolve to the same final URL and therefore the same journal
    // path — which is the whole point. Two servers would sit on different ports, produce
    // different URLs, and never collide, so the test would pass without exercising anything.
    //
    // The budget is larger than MAX_RETRIES, so the first download exhausts its retries and
    // fails; by the second the budget is spent and the transfer succeeds.
    let server = PathologyServer::start(ServerSpec {
        transient_body_failures: 8,
        ..spec()
    })
    .await
    .expect("server starts");
    let scratch = Scratch::new("retry-after-failure");

    let _ = run(&server, &scratch, TransportMode::Http1Only)
        .await
        .expect_err("the retry budget is exhausted");
    assert!(
        scratch.journal_bytes().is_some(),
        "a failed download keeps its journal as evidence"
    );

    // The user clears the partial file and tries the same URL again. The journal from the failed
    // attempt is still sitting at the same path, and must not stop them.
    std::fs::remove_file(scratch.path().join("content.dppart")).expect("remove the partial");
    let path = run(&server, &scratch, TransportMode::Http1Only)
        .await
        .expect("the retry must not be blocked by the failed attempt's journal");

    let bytes = std::fs::read(&path).expect("read");
    assert_eq!(Content::new(42, SIZE).first_mismatch(0, &bytes), None);
}
