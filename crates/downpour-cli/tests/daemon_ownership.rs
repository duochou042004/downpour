//! S3-T8 — the daemon, never the submitting CLI, owns a transfer.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use downpour_corpus::content::Content;
use downpour_corpus::server::{PathologyServer, ServerSpec};
use downpour_daemon::server::{TransferConfig, TransferDaemon, serve_connection};
use downpour_ipc::{CommandHandler, LocalListener, Request, Response};

const SIZE: u64 = 8 * 1024 * 1024;

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!("downpour-daemon-owner-{unique}"));
        std::fs::create_dir_all(&root).expect("create scratch root");
        Self { root }
    }

    fn child(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct HoldAddResponse {
    daemon: TransferDaemon,
    submitted: Option<SyncSender<()>>,
    release: Receiver<()>,
}

impl CommandHandler for HoldAddResponse {
    fn handle(&mut self, request: Request) -> Response {
        let is_add = matches!(request, Request::DownloadAdd(_));
        let response = self.daemon.handle(request);
        if is_add {
            if let Some(submitted) = self.submitted.take() {
                let _ = submitted.send(());
            }
            let _ = self.release.recv();
        }
        response
    }
}

fn dp_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dp"))
}

fn spawn_dp(url: &str, output: &Path, runtime_root: &Path) -> Child {
    Command::new(dp_binary())
        .arg("add")
        .arg(url)
        .arg("--output-dir")
        .arg(output)
        .arg("--http1")
        .env("DOWNPOUR_RUNTIME_ROOT", runtime_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn dp")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killing_cli_after_submission_does_not_stop_daemon_progress() {
    let content = Content::new(73, SIZE);
    let server = PathologyServer::start_holding_first_range(
        ServerSpec {
            content,
            ..ServerSpec::default()
        },
        0,
    )
    .await
    .expect("start held origin");
    let scratch = Scratch::new();
    let output = scratch.child("output");
    let journals = scratch.child("journals");
    let runtime_root = scratch.child("runtime");
    std::fs::create_dir_all(&output).expect("create output directory");
    std::fs::create_dir_all(&journals).expect("create journal directory");
    std::fs::create_dir_all(&runtime_root).expect("create runtime root");

    let listener = LocalListener::bind(&runtime_root).expect("bind daemon endpoint");
    let paths = listener.paths().clone();
    let token = listener.session_token();
    let daemon = TransferDaemon::new(TransferConfig {
        target_dir: output.clone(),
        journal_dir: journals,
        connections: 4,
    });
    let (submitted_tx, submitted_rx) = sync_channel(1);
    let (release_tx, release_rx) = sync_channel(1);
    let connection = tokio::spawn(async move {
        let stream = listener.accept().await.expect("accept dp");
        serve_connection(
            stream,
            token,
            HoldAddResponse {
                daemon,
                submitted: Some(submitted_tx),
                release: release_rx,
            },
        )
        .await
    });

    let mut cli = spawn_dp(
        &server.entry_url(),
        &output,
        paths.runtime_dir().parent().expect("runtime root"),
    );
    let submitted =
        tokio::task::spawn_blocking(move || submitted_rx.recv_timeout(Duration::from_secs(3)))
            .await
            .expect("join submission observer");
    if submitted.is_err() {
        let _ = cli.kill();
        let output = cli.wait_with_output().expect("collect failed dp");
        panic!(
            "the daemon never accepted the submitted download; stdout={:?}, stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    cli.kill().expect("kill dp after daemon submission");
    let status = cli.wait().expect("reap killed dp");
    assert!(!status.success(), "the proof must actually kill a live CLI");
    release_tx.send(()).expect("release daemon response");
    server.release_held_range();

    let final_path = output.join("content");
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if tokio::fs::try_exists(&final_path).await.unwrap_or(false) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("daemon transfer did not finish after the CLI died");

    let bytes = tokio::fs::read(&final_path).await.expect("read final file");
    assert_eq!(
        content.first_mismatch(0, &bytes),
        None,
        "the daemon-owned result must remain byte-exact"
    );
    let _ = connection.await;
}
