//! S3 daemon dispatch semantics that must fail rather than lie about unavailable execution.

use downpour_daemon::server::{TransferConfig, TransferDaemon};
use downpour_http::TransportMode;
use downpour_ipc::{
    AddOptions, AddParams, CommandHandler, PROTOCOL_VERSION, Request, Response, SecretString,
};

#[tokio::test]
async fn an_unwired_multi_connection_request_is_refused_instead_of_silently_running_single() {
    let root = std::env::temp_dir().join("downpour-unwired-segmented-dispatch");
    let mut daemon = TransferDaemon::new(TransferConfig {
        target_dir: root.join("target"),
        journal_dir: root.join("journals"),
        transport_mode: TransportMode::Http1Only,
    });
    let response = daemon.handle(Request::DownloadAdd(AddParams {
        protocol_version: PROTOCOL_VERSION,
        url: SecretString::new("http://127.0.0.1:9/file"),
        target: None,
        options: AddOptions {
            connections: Some(4),
        },
    }));
    let Response::Error(error) = response else {
        panic!("the daemon accepted a connection count its execution path does not consume");
    };
    assert_eq!(error.data.kind, "segmented_execution_not_ready");

    let response = daemon.handle(Request::DownloadAdd(AddParams {
        protocol_version: PROTOCOL_VERSION,
        url: SecretString::new("http://127.0.0.1:9/single"),
        target: None,
        options: AddOptions {
            connections: Some(1),
        },
    }));
    assert!(
        matches!(response, Response::Added(_)),
        "the refusal guard also rejected the wired single-stream path: {response:?}"
    );
}
