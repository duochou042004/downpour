//! S3 daemon dispatch semantics that must fail rather than lie about unavailable execution.

use downpour_daemon::server::{
    DEFAULT_CONNECTIONS, MAX_CONNECTIONS, TransferConfig, TransferDaemon, effective_connections,
};
use downpour_http::TransportMode;
use downpour_ipc::{
    AddOptions, AddParams, CommandHandler, PROTOCOL_VERSION, Request, Response, SecretString,
};

/// S3-T16 — the connection count is validated, bounded, and accepted.
///
/// This replaces a guard that refused every count above one with `segmented_execution_not_ready`,
/// which was honest while the daemon ran `SingleStream` for everything and is now simply wrong.
/// What still has to hold is that zero is refused rather than quietly meaning one, and that a
/// client cannot talk this daemon into opening an unbounded number of connections: the user's
/// setting is a ceiling, and `MAX_CONNECTIONS` is the ceiling on the ceiling (I-7).
#[tokio::test]
async fn a_connection_count_is_refused_at_zero_and_bounded_above() {
    let root = std::env::temp_dir().join("downpour-connection-count-bounds");
    let mut daemon = TransferDaemon::new(TransferConfig {
        target_dir: root.join("target"),
        journal_dir: root.join("journals"),
        transport_mode: TransportMode::Http1Only,
    });

    let response = daemon.handle(Request::DownloadAdd(AddParams {
        protocol_version: PROTOCOL_VERSION,
        url: SecretString::new("http://127.0.0.1:9/zero"),
        target: None,
        options: AddOptions {
            connections: Some(0),
        },
    }));
    let Response::Error(error) = response else {
        panic!("zero connections must be refused rather than silently meaning one");
    };
    assert_eq!(error.data.kind, "invalid_connections");

    // Accepted, not refused, and not taken literally either.
    for requested in [None, Some(1), Some(4), Some(u16::MAX)] {
        let response = daemon.handle(Request::DownloadAdd(AddParams {
            protocol_version: PROTOCOL_VERSION,
            url: SecretString::new("http://127.0.0.1:9/file"),
            target: None,
            options: AddOptions {
                connections: requested,
            },
        }));
        assert!(
            matches!(response, Response::Added(_)),
            "a connection count of {requested:?} must be accepted: {response:?}"
        );
    }
}

/// The ceiling on the ceiling, stated where it can be observed.
///
/// The add response carries an id rather than an effective connection count, so asserting the
/// clamp through the IPC surface would be asserting that nothing crashed. This is the decision
/// itself.
#[test]
fn a_requested_connection_count_is_clamped_rather_than_believed() {
    assert_eq!(effective_connections(Some(0)), None);
    assert_eq!(effective_connections(None), Some(DEFAULT_CONNECTIONS));
    assert_eq!(effective_connections(Some(1)), Some(1));
    assert_eq!(effective_connections(Some(4)), Some(4));
    assert_eq!(
        effective_connections(Some(u16::MAX)),
        Some(MAX_CONNECTIONS),
        "a client asking for thousands of connections is asking to be rate-limited by the origin"
    );
}
