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

/// B-25 — the ids the daemon hands out must be ids its own storage will accept.
///
/// The IPC id is 32 lowercase hex characters; the metadata store's id is the same sixteen bytes
/// but validated as a UUIDv7, because those bytes are also the recovery journal's transfer id and
/// the SQLite primary key. `fresh_download_id` minted 128 random bits, which satisfy the version
/// and variant bits by accident roughly once in sixty-four.
///
/// Nothing caught it because the daemon never persists anything yet. It would have surfaced as
/// the first `download.add` after persistence landed failing on a validation error that has
/// nothing to do with the download, for most ids but not all — which is the worst shape a bug
/// can have.
///
/// Run over many ids rather than one, because a single sample passes by chance often enough to
/// be a flaky green.
#[test]
fn every_download_id_the_daemon_mints_is_accepted_by_its_own_metadata_store() {
    let mut minted = std::collections::BTreeSet::new();
    for _ in 0..256 {
        let id = downpour_daemon::server::fresh_download_id().expect("the OS random source works");
        let text = id.as_str();
        assert_eq!(text.len(), 32, "the IPC id is 32 hex characters: {text}");

        let mut bytes = [0_u8; 16];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)
                .expect("the IPC id is hexadecimal");
        }
        downpour_storage::metadata::DownloadId::try_from_bytes(bytes).unwrap_or_else(|error| {
            panic!("the daemon minted {text}, which its own store rejects: {error}")
        });
        minted.insert(text.to_owned());
    }
    assert_eq!(minted.len(), 256, "ids must not repeat");
}
