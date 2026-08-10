//! S1-T12 — `dp add <url>` end to end.
//!
//! These run the built `dp` binary as a subprocess rather than calling the library, because what
//! is under test is the thing a user actually invokes: argument parsing, the exit code, what lands
//! on stdout, and what lands on stderr. A library-level test would pass with a binary that never
//! ran.

use std::path::{Path, PathBuf};
use std::process::Command;

use downpour_corpus::content::Content;
use downpour_corpus::server::{
    DigestSpec, Mutation, MutationEffect, PathologyServer, Protocol, RangeBehaviour, ServerSpec,
};
use downpour_daemon::server::{TransferConfig, TransferDaemon, serve_connection};
use downpour_http::TransportMode;
use downpour_ipc::LocalListener;

const SIZE: u64 = 256 * 1024;
/// Four workers need four grants at or above `DEFAULT_MIN_SPLIT_BYTES` (1 MiB), so the smallest
/// representation this pool will divide four ways is 8 MiB. Below the floor a split costs more in
/// round trips than it saves, which is why `SIZE` downloads on one connection however many are
/// asked for — see the companion assertion below.
const SEGMENTED_SIZE: u64 = 8 * 1024 * 1024;

fn spec() -> ServerSpec {
    ServerSpec {
        content: Content::new(42, SIZE),
        ..ServerSpec::default()
    }
}

/// The `dp` binary Cargo built alongside this test.
///
/// `CARGO_BIN_EXE_dp` is set by Cargo only for integration tests in `dp`'s own package, and it
/// guarantees the binary was built first. That guarantee is the reason this test lives here rather
/// than beside the other corpus tests: from another package the path would have to be guessed, and
/// a guess that misses turns into a test that silently never ran the binary.
fn dp_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dp"))
}

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("downpour-cli-{tag}-{unique}"));
        std::fs::create_dir_all(&path).expect("create the scratch directory");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn entries(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.path)
            .expect("read the scratch directory")
            .filter_map(Result::ok)
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

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

struct DaemonHarness {
    runtime_root: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl DaemonHarness {
    async fn start(target_dir: &Path, mode: TransportMode) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let runtime_root = std::env::temp_dir().join(format!("downpour-cli-daemon-{unique}"));
        let journal_dir = runtime_root.join("journals");
        std::fs::create_dir_all(&runtime_root).expect("create daemon runtime root");
        std::fs::create_dir_all(&journal_dir).expect("create daemon journal directory");
        let listener = LocalListener::bind(&runtime_root).expect("bind daemon endpoint");
        let token = listener.session_token();
        let daemon = TransferDaemon::new(TransferConfig {
            target_dir: target_dir.to_path_buf(),
            journal_dir,
            transport_mode: mode,
        });
        let task = tokio::spawn(async move {
            let Ok(stream) = listener.accept().await else {
                return;
            };
            let _ = serve_connection(stream, token, daemon).await;
        });
        Self { runtime_root, task }
    }
}

impl Drop for DaemonHarness {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.runtime_root);
    }
}

fn dp_add(url: &str, scratch: &Scratch, daemon: &DaemonHarness) -> Run {
    dp_add_with(url, scratch, daemon, &[])
}

fn dp_add_with(url: &str, scratch: &Scratch, daemon: &DaemonHarness, extra: &[&str]) -> Run {
    let mut command = Command::new(dp_binary());
    command
        .arg("add")
        .arg(url)
        .arg("--output-dir")
        .arg(scratch.path())
        .args(extra)
        .env("DOWNPOUR_RUNTIME_ROOT", &daemon.runtime_root);
    let output = command.output().expect("dp runs");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn dp_add_downloads_a_file_and_prints_its_id() {
    let server = PathologyServer::start(spec()).await.expect("server starts");
    let scratch = Scratch::new("ok");
    let daemon = DaemonHarness::start(scratch.path(), TransportMode::Http1Only).await;

    let run = dp_add(&server.entry_url(), &scratch, &daemon);

    assert_eq!(run.code, Some(0), "stderr was: {}", run.stderr);
    assert!(
        run.stdout.trim().len() == 32
            && run
                .stdout
                .trim()
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit()),
        "stdout must carry one stable download ID and nothing else: {:?}",
        run.stdout
    );
    assert_eq!(scratch.entries(), vec!["content"]);

    let bytes = std::fs::read(scratch.path().join("content")).expect("final file is readable");
    assert_eq!(
        Content::new(42, SIZE).first_mismatch(0, &bytes),
        None,
        "silent corruption through the CLI path"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dp_add_works_over_http2() {
    let server = PathologyServer::start(ServerSpec {
        protocol: Protocol::H2c,
        ..spec()
    })
    .await
    .expect("server starts");
    let scratch = Scratch::new("h2");
    let daemon = DaemonHarness::start(scratch.path(), TransportMode::Http2PriorKnowledge).await;

    let run = dp_add(&server.entry_url(), &scratch, &daemon);

    assert_eq!(run.code, Some(0), "stderr was: {}", run.stderr);
    let bytes = std::fs::read(scratch.path().join("content")).expect("readable");
    assert_eq!(Content::new(42, SIZE).first_mismatch(0, &bytes), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn dp_add_downloads_from_a_server_without_range_support() {
    let server = PathologyServer::start(ServerSpec {
        ranges: RangeBehaviour::Absent,
        ..spec()
    })
    .await
    .expect("server starts");
    let scratch = Scratch::new("noranges");
    let daemon = DaemonHarness::start(scratch.path(), TransportMode::Http1Only).await;
    let run = dp_add(&server.entry_url(), &scratch, &daemon);
    assert_eq!(run.code, Some(0), "stderr was: {}", run.stderr);
    let bytes = std::fs::read(scratch.path().join("content")).expect("readable");
    assert_eq!(Content::new(42, SIZE).first_mismatch(0, &bytes), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn dp_add_exits_non_zero_and_writes_no_file_when_the_probe_refuses() {
    // I-5 through the whole stack. A zero exit code here would make the failure invisible to any
    // script wrapping dp, which is worse than the download not working.
    let server = PathologyServer::start(ServerSpec {
        content_encoding: Some("gzip".to_owned()),
        ..spec()
    })
    .await
    .expect("server starts");
    let scratch = Scratch::new("gzip");
    let daemon = DaemonHarness::start(scratch.path(), TransportMode::Http1Only).await;

    let run = dp_add(&server.entry_url(), &scratch, &daemon);

    assert_eq!(run.code, Some(4));
    assert!(
        run.stdout.trim().is_empty(),
        "nothing may be printed to stdout on failure"
    );
    assert!(
        scratch.entries().is_empty(),
        "no file may be created: {:?}",
        scratch.entries()
    );
    assert!(
        run.stderr.contains("unexpected_content_encoding"),
        "the reason must be reported, got: {}",
        run.stderr
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dp_add_leaves_no_final_name_when_the_body_is_truncated() {
    let server = PathologyServer::start(ServerSpec {
        truncate_body_after: Some(4096),
        ..spec()
    })
    .await
    .expect("server starts");
    let scratch = Scratch::new("truncated");
    let daemon = DaemonHarness::start(scratch.path(), TransportMode::Http1Only).await;

    let run = dp_add(&server.entry_url(), &scratch, &daemon);

    assert_eq!(run.code, Some(4));
    let entries = scratch.entries();
    assert!(
        !entries.contains(&"content".to_owned()),
        "I-4 violated: {entries:?}"
    );
    assert_eq!(entries, vec!["content.dppart"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn dp_add_reports_a_bad_url_without_touching_the_network() {
    let scratch = Scratch::new("badurl");
    let daemon = DaemonHarness::start(scratch.path(), TransportMode::Http1Only).await;
    let run = dp_add("not a url", &scratch, &daemon);
    assert_eq!(run.code, Some(4));
    assert!(run.stderr.contains("invalid_url"), "got: {}", run.stderr);
    assert!(scratch.entries().is_empty());
}

#[test]
fn dp_reports_a_version_and_a_help_text() {
    // Cheap, and it catches a clap configuration that panics at startup — which would otherwise
    // only show up in front of a user.
    let version = Command::new(dp_binary())
        .arg("--version")
        .output()
        .expect("dp runs");
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).contains("dp"));

    let help = Command::new(dp_binary())
        .arg("--help")
        .output()
        .expect("dp runs");
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(
        text.contains("add"),
        "the add subcommand must be discoverable: {text}"
    );
}

/// S3-T16 — the daemon actually performs a segmented download.
///
/// Until this landed, `downpour-daemon` ran `SingleStream` for every transfer and refused any
/// connection count above one with `segmented_execution_not_ready`. The allocator, writer service
/// and worker pool existed and were tested, and no user could reach them (B-40). This is the proof
/// that the product path and the engine path are the same path.
#[tokio::test(flavor = "multi_thread")]
async fn dp_add_with_four_connections_segments_the_transfer_and_lands_byte_exact() {
    let server = PathologyServer::start(ServerSpec {
        content: Content::new(42, SEGMENTED_SIZE),
        ..ServerSpec::default()
    })
    .await
    .expect("server starts");
    let scratch = Scratch::new("segmented");
    let daemon = DaemonHarness::start(scratch.path(), TransportMode::Http1Only).await;

    let run = dp_add_with(
        &server.entry_url(),
        &scratch,
        &daemon,
        &["--connections", "4"],
    );

    assert_eq!(run.code, Some(0), "stderr was: {}", run.stderr);
    assert_eq!(scratch.entries(), vec!["content"]);
    let bytes = std::fs::read(scratch.path().join("content")).expect("final file is readable");
    assert_eq!(
        u64::try_from(bytes.len()).expect("length fits u64"),
        SEGMENTED_SIZE,
        "the segmented path produced a file of the wrong length"
    );
    assert_eq!(
        Content::new(42, SEGMENTED_SIZE).first_mismatch(0, &bytes),
        None,
        "silent corruption through the segmented daemon path"
    );

    // The file being right is necessary and not sufficient: a daemon that quietly ran one stream
    // would produce exactly the same file. The server is what can tell them apart.
    let ranged = server
        .requests()
        .iter()
        .filter(|request| {
            request
                .header("range")
                .is_some_and(|value| value != "bytes=0-0")
        })
        .count();
    assert!(
        ranged >= 4,
        "four connections must produce at least four ranged requests, saw {ranged}: {:?}",
        server
            .requests()
            .iter()
            .map(|request| request.header("range"))
            .collect::<Vec<_>>()
    );
    assert!(
        server.maximum_simultaneous_connections() >= 2,
        "a segmented transfer must have had more than one connection open at once, peak was {}",
        server.maximum_simultaneous_connections()
    );
}

/// The other half of the ceiling rule: asking for connections does not create them.
///
/// A representation too small to divide at `DEFAULT_MIN_SPLIT_BYTES` runs on one connection
/// however many were requested. Splitting it would cost more round trips and handshakes than the
/// parallelism could return (`docs/03-transfer-engine-spec.md` §4.2).
#[tokio::test(flavor = "multi_thread")]
async fn dp_add_below_the_split_floor_uses_one_connection_however_many_are_asked_for() {
    let server = PathologyServer::start(spec()).await.expect("server starts");
    let scratch = Scratch::new("below-floor");
    let daemon = DaemonHarness::start(scratch.path(), TransportMode::Http1Only).await;

    let run = dp_add_with(
        &server.entry_url(),
        &scratch,
        &daemon,
        &["--connections", "8"],
    );

    assert_eq!(run.code, Some(0), "stderr was: {}", run.stderr);
    let bytes = std::fs::read(scratch.path().join("content")).expect("final file is readable");
    assert_eq!(Content::new(42, SIZE).first_mismatch(0, &bytes), None);
    let ranged = server
        .requests()
        .iter()
        .filter(|request| {
            request
                .header("range")
                .is_some_and(|value| value != "bytes=0-0")
        })
        .count();
    assert_eq!(
        ranged,
        1,
        "a 256 KiB representation is below the 1 MiB split floor and must be fetched once: {:?}",
        server
            .requests()
            .iter()
            .map(|request| request.header("range"))
            .collect::<Vec<_>>()
    );
}

/// I-4 on the segmented path: verified before it is named, with no fast path around it.
///
/// The pool refuses incomplete coverage on its own, so a truncated segmented transfer never
/// reaches the rename. A digest mismatch is different: every byte arrived, the interval map is
/// complete, the length on disk is right, and the file is still not the representation the server
/// meant to send. Only the completion sequence can tell, which is why removing it leaves every
/// other test green.
#[tokio::test(flavor = "multi_thread")]
async fn a_segmented_transfer_whose_digest_disagrees_is_not_given_the_final_name() {
    let server = PathologyServer::start(ServerSpec {
        content: Content::new(42, SEGMENTED_SIZE),
        // Well-formed, correct algorithm, wrong bytes — the shape a mid-transfer substitution or a
        // cache serving a stale representation produces.
        digest: Some(DigestSpec::Literal(
            "sha-256=:UjfWbtkjaXhFXHo0IdcHqDcgSv5hDkCLnYCPUcYbHnk=:".to_owned(),
        )),
        ..ServerSpec::default()
    })
    .await
    .expect("server starts");
    let scratch = Scratch::new("segmented-digest");
    let daemon = DaemonHarness::start(scratch.path(), TransportMode::Http1Only).await;

    let run = dp_add_with(
        &server.entry_url(),
        &scratch,
        &daemon,
        &["--connections", "4"],
    );

    assert_ne!(run.code, Some(0), "stdout was: {}", run.stdout);
    assert!(
        run.stderr.contains("unverified"),
        "the refusal must name verification, not a generic failure: {}",
        run.stderr
    );
    assert!(
        !scratch.entries().contains(&"content".to_owned()),
        "a file that failed verification must not wear the final name: {:?}",
        scratch.entries()
    );
    // The part file is evidence for a later resume and is deliberately not deleted (docs/04 §6).
    assert!(
        scratch
            .entries()
            .iter()
            .any(|name| name.ends_with(".dppart")),
        "the partial file must survive as resumable evidence: {:?}",
        scratch.entries()
    );
}

/// I-3 on the segmented path: a representation that changes mid-transfer is refused, not spliced.
///
/// This is the corruption that makes cross-process resume dangerous and is why every ranged
/// worker request carries `If-Range`. Several workers are fetching disjoint ranges of one file;
/// the server swaps the representation partway through. Without the conditional, the remaining
/// workers fetch ranges of the *new* representation and write them at offsets belonging to the
/// old one, and the finished file is part one version and part another at exactly the expected
/// size — which the length check passes, the coverage check passes, and only a content hash
/// would catch.
///
/// The server answers a stale `If-Range` with `200` and the whole body, which the pool refuses
/// because it asked for a range. So the failure is loud and the bytes already on disk stay
/// resumable.
#[tokio::test(flavor = "multi_thread")]
async fn a_segmented_transfer_refuses_a_representation_that_changes_underneath_it() {
    let server = PathologyServer::start(ServerSpec {
        content: Content::new(42, SEGMENTED_SIZE),
        etag: Some("\"v1\"".to_owned()),
        behaviour: vec![Mutation {
            at_bytes_served: SEGMENTED_SIZE / 8,
            then: MutationEffect::SetEtag("\"v2\"".to_owned()),
        }],
        ..ServerSpec::default()
    })
    .await
    .expect("server starts");
    let scratch = Scratch::new("segmented-etag-change");
    let daemon = DaemonHarness::start(scratch.path(), TransportMode::Http1Only).await;

    let run = dp_add_with(
        &server.entry_url(),
        &scratch,
        &daemon,
        &["--connections", "4"],
    );

    assert_ne!(
        run.code,
        Some(0),
        "a representation change under a segmented transfer must not succeed: {}",
        run.stdout
    );
    assert!(
        !scratch.entries().contains(&"content".to_owned()),
        "no file may wear the final name after the representation changed: {:?}",
        scratch.entries()
    );
    // Every ranged request has to have carried the conditional, or the refusal above happened for
    // some other reason and this case proves nothing about I-3.
    let requests = server.requests();
    let ranged: Vec<_> = requests
        .iter()
        .filter(|request| {
            request
                .header("range")
                .is_some_and(|value| value != "bytes=0-0")
        })
        .collect();
    assert!(
        !ranged.is_empty(),
        "the transfer must have issued ranged requests"
    );
    assert!(
        ranged
            .iter()
            .all(|request| request.header("if-range").as_deref() == Some("\"v1\"")),
        "every ranged request must be conditional on the recorded validator: {:?}",
        ranged
            .iter()
            .map(|request| request.header("if-range"))
            .collect::<Vec<_>>()
    );
}
