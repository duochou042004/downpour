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
use downpour_http::download::{DownloadError, SingleStream};
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

/// A scratch directory that cleans itself up.
struct Scratch {
    path: std::path::PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("downpour-test-{tag}-{unique}"));
        std::fs::create_dir_all(&path).expect("create the scratch directory");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// Every entry in the directory, sorted, so a test can assert on what exists.
    fn entries(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.path)
            .expect("read the scratch directory")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

async fn run(
    server: &PathologyServer,
    scratch: &Scratch,
    mode: TransportMode,
) -> Result<std::path::PathBuf, DownloadError> {
    let url = Url::parse(&server.entry_url()).expect("the server URL is well formed");
    SingleStream::new(backend(mode))
        .download(url, scratch.path())
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
    assert_eq!(partial.len(), 8192);
    assert_eq!(Content::new(42, SIZE).first_mismatch(0, &partial), None);
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
