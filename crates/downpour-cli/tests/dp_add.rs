//! S1-T12 — `dp add <url>` end to end.
//!
//! These run the built `dp` binary as a subprocess rather than calling the library, because what
//! is under test is the thing a user actually invokes: argument parsing, the exit code, what lands
//! on stdout, and what lands on stderr. A library-level test would pass with a binary that never
//! ran.

use std::path::{Path, PathBuf};
use std::process::Command;

use downpour_corpus::content::Content;
use downpour_corpus::server::{PathologyServer, Protocol, RangeBehaviour, ServerSpec};

const SIZE: u64 = 256 * 1024;

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

fn dp_add(url: &str, scratch: &Scratch, extra: &[&str]) -> Run {
    let mut command = Command::new(dp_binary());
    command
        .arg("add")
        .arg(url)
        .arg("--output-dir")
        .arg(scratch.path())
        .args(extra);
    let output = command.output().expect("dp runs");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn dp_add_downloads_a_file_and_prints_its_path() {
    let server = PathologyServer::start(spec()).await.expect("server starts");
    let scratch = Scratch::new("ok");

    let run = dp_add(&server.entry_url(), &scratch, &["--http1"]);

    assert_eq!(run.code, Some(0), "stderr was: {}", run.stderr);
    let printed = run.stdout.trim();
    assert_eq!(
        Path::new(printed),
        scratch.path().join("content"),
        "stdout must carry the path and nothing else, so `dp add | xargs` works"
    );
    assert_eq!(scratch.entries(), vec!["content"]);

    let bytes = std::fs::read(printed).expect("the printed path is readable");
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

    let run = dp_add(&server.entry_url(), &scratch, &["--http2-prior-knowledge"]);

    assert_eq!(run.code, Some(0), "stderr was: {}", run.stderr);
    let bytes = std::fs::read(run.stdout.trim()).expect("readable");
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
    let run = dp_add(&server.entry_url(), &scratch, &["--http1"]);
    assert_eq!(run.code, Some(0), "stderr was: {}", run.stderr);
    let bytes = std::fs::read(run.stdout.trim()).expect("readable");
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

    let run = dp_add(&server.entry_url(), &scratch, &["--http1"]);

    assert_eq!(run.code, Some(1));
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
        run.stderr.contains("Content-Encoding") || run.stderr.contains("compressed"),
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

    let run = dp_add(&server.entry_url(), &scratch, &["--http1"]);

    assert_eq!(run.code, Some(1));
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
    let run = dp_add("not a url", &scratch, &[]);
    assert_eq!(run.code, Some(1));
    assert!(
        run.stderr.contains("not a valid URL"),
        "got: {}",
        run.stderr
    );
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
