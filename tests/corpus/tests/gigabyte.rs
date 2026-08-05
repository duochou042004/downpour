//! S1-T13 — a 1 GB file, downloaded correctly over HTTP/1.1 and HTTP/2.
//!
//! Exit criterion **S1-C1**, and the broadest statement of **S1-C6** (zero silent corruption)
//! available in S1: a gigabyte compared byte for byte against the generator, with the first
//! disagreement reported by its exact offset.
//!
//! These are `#[ignore]`d so they stay out of the every-push budget
//! (`docs/09-testing-strategy.md` §6 allows under five minutes there). Run them with:
//!
//! ```text
//! just corpus-slow
//! ```
//!
//! Neither side ever holds a gigabyte in memory. The server generates each chunk as it is polled
//! for, and the verification below streams the file past the generator a megabyte at a time.
//! Buffering either side would make the test a memory-pressure experiment rather than a
//! correctness one, and would cap the corpus at whatever fits in RAM.

use std::io::Read;
use std::path::{Path, PathBuf};

use downpour_corpus::content::Content;
use downpour_corpus::server::{PathologyServer, Protocol, ServerSpec};
use downpour_http::{H1H2Backend, SingleStream, TransportMode};
use url::Url;

/// One gigabyte, decimal, as the exit criterion words it.
const ONE_GB: u64 = 1_000_000_000;
const SEED: u64 = 20260803;

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("downpour-gb-{tag}-{unique}"));
        std::fs::create_dir_all(&path).expect("create the scratch directory");
        Self { path }
    }

    /// Recovery journals go beside the data, never in it (docs/04 §1).
    fn journals(&self) -> PathBuf {
        let path = self.path.join("journals");
        std::fs::create_dir_all(&path).expect("create the journal directory");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // A gigabyte is worth cleaning up even when the test fails.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Compare a file against the generator without loading it, reporting the first disagreement by
/// its absolute offset — "byte 4 194 305 should be 0x7A and is 0x00" rather than "files differ".
fn verify_streaming(path: &Path, content: &Content, expected_len: u64) {
    let metadata = std::fs::metadata(path).expect("the downloaded file exists");
    assert_eq!(metadata.len(), expected_len, "wrong length on disk");

    const WINDOW: usize = 1024 * 1024;
    let mut file = std::fs::File::open(path).expect("open the downloaded file");
    let mut buffer = vec![0_u8; WINDOW];
    let mut offset = 0_u64;

    loop {
        let read = file.read(&mut buffer).expect("read the downloaded file");
        if read == 0 {
            break;
        }
        if let Some(mismatch) = content.first_mismatch(offset, &buffer[..read]) {
            panic!(
                "silent corruption at byte {}: expected {:#04x}, found {:#04x}",
                mismatch.offset, mismatch.expected, mismatch.actual
            );
        }
        offset = offset.saturating_add(u64::try_from(read).expect("a window length fits a u64"));
    }
    assert_eq!(
        offset, expected_len,
        "read a different number of bytes than the file reports"
    );
}

async fn download_a_gigabyte(protocol: Protocol, mode: TransportMode, tag: &str) {
    let content = Content::new(SEED, ONE_GB);
    let server = PathologyServer::start(ServerSpec {
        protocol,
        content,
        ..ServerSpec::default()
    })
    .await
    .expect("server starts");
    let scratch = Scratch::new(tag);

    let url = Url::parse(&server.entry_url()).expect("the server URL is well formed");
    let backend = H1H2Backend::new(mode).expect("the h1h2 backend builds");

    let started = std::time::Instant::now();
    let path = SingleStream::new(backend)
        .download(
            url,
            &downpour_http::StorageLayout::new(&scratch.path, scratch.journals()),
        )
        .await
        .expect("a gigabyte downloads");
    let elapsed = started.elapsed();

    verify_streaming(&path, &content, ONE_GB);

    // Not an assertion — a throughput floor here would be a flaky test on a loaded machine, and
    // speed is S4's business. Printed so a regression is visible in the log.
    let throughput = ONE_GB as f64 / elapsed.as_secs_f64() / 1_000_000.0;
    println!("{tag}: 1 GB verified in {elapsed:.1?} ({throughput:.0} MB/s)");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "slow: transfers and verifies 1 GB. Run with `just corpus-slow`."]
async fn a_gigabyte_over_http11_is_byte_exact() {
    download_a_gigabyte(Protocol::Http11, TransportMode::Http1Only, "h1").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "slow: transfers and verifies 1 GB. Run with `just corpus-slow`."]
async fn a_gigabyte_over_http2_is_byte_exact() {
    download_a_gigabyte(Protocol::H2c, TransportMode::Http2PriorKnowledge, "h2c").await;
}

/// The verifier has to be able to fail, or the two tests above prove nothing.
///
/// Not ignored: it is cheap, and it is what makes the gigabyte results meaningful.
#[test]
fn the_verifier_detects_a_single_flipped_byte() {
    let content = Content::new(SEED, 4 * 1024 * 1024);
    let scratch = Scratch::new("verifier");
    let path = scratch.path.join("sample.bin");

    let mut bytes = content.range(0, 4 * 1024 * 1024);
    // Deep inside the third window, so a verifier that only checked the first chunk would miss it.
    let victim = 3 * 1024 * 1024 + 12345;
    bytes[victim] ^= 0xFF;
    std::fs::write(&path, &bytes).expect("write the corrupted sample");

    let outcome = std::panic::catch_unwind(|| {
        verify_streaming(&path, &content, 4 * 1024 * 1024);
    });
    assert!(outcome.is_err(), "the verifier accepted a corrupted file");
}
