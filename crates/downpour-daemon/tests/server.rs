//! S3 daemon dispatch semantics that must fail rather than lie about unavailable execution.

use downpour_daemon::server::{
    DEFAULT_CONNECTIONS, MAX_CONNECTIONS, TransferConfig, TransferDaemon, effective_connections,
};
use downpour_http::TransportMode;
use downpour_ipc::{
    AddOptions, AddParams, CommandHandler, PROTOCOL_VERSION, Request, Response, SecretString,
    WireState,
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
    std::fs::create_dir_all(&root).expect("the daemon's data root exists before it opens a store");
    let mut daemon = TransferDaemon::new(TransferConfig {
        target_dir: root.join("target"),
        journal_dir: root.join("journals"),
        database_path: root.join("downpour.db"),
        transport_mode: TransportMode::Http1Only,
    })
    .expect("the daemon opens its store");

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

/// B-25 — a download the daemon started survives the process that started it.
///
/// This is the point of the daemon architecture as far as recovery is concerned. The first
/// instance probes, records, and transfers; it is then discarded entirely, in-memory registry and
/// all. A second instance is given nothing but the same directories and has to find the download.
///
/// The record is written after the probe rather than at `download.add`, because before the probe
/// there is no representation length, no validator and no part path — a record written then would
/// name a file that does not exist. It is written again when the transfer ends, because a record
/// left at `Transferring` describes a part file that has since been renamed away, and the next
/// process reconciles that as a missing part file and reports a finished download as broken.
#[tokio::test(flavor = "multi_thread")]
async fn a_download_outlives_the_daemon_instance_that_started_it() {
    use downpour_corpus::content::Content;
    use downpour_corpus::server::{PathologyServer, ServerSpec};

    const LENGTH: u64 = 8 * 1024 * 1024;
    let server = PathologyServer::start(ServerSpec {
        content: Content::new(11, LENGTH),
        etag: Some("\"outlives-v1\"".to_owned()),
        ..ServerSpec::default()
    })
    .await
    .expect("server starts");

    let root = std::env::temp_dir().join(format!(
        "downpour-outlives-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    let config = || TransferConfig {
        target_dir: root.join("target"),
        journal_dir: root.join("journals"),
        database_path: root.join("downpour.db"),
        transport_mode: TransportMode::Http1Only,
    };
    std::fs::create_dir_all(root.join("target")).expect("target dir");
    std::fs::create_dir_all(root.join("journals")).expect("journal dir");

    let mut first = TransferDaemon::new(config()).expect("the first daemon opens its store");
    let response = first.handle(Request::DownloadAdd(AddParams {
        protocol_version: PROTOCOL_VERSION,
        url: SecretString::new(server.entry_url()),
        target: None,
        options: AddOptions {
            connections: Some(4),
        },
    }));
    let Response::Added(added) = response else {
        panic!("the add was refused: {response:?}");
    };

    // While the transfer is live, its journal must be at the path startup recovery will look for.
    // Asserted here rather than after completion because a verified download retires its journal,
    // and asserted at all because the completed-download assertions below cannot see it:
    // reconciliation skips terminal downloads, so a journal named by the wrong rule would never be
    // looked for and the mistake would not surface until a restart mid-transfer (B-51).
    let expected_journal = root
        .join("journals")
        .join(format!("{}.dpj", added.id.as_str()));
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            if expected_journal.exists() {
                return;
            }
            if let Ok(entries) = std::fs::read_dir(root.join("journals"))
                && entries.count() > 0
            {
                // Something was written, under a different name. Fail now with what is actually
                // there rather than timing out with nothing to say.
                let names: Vec<String> = std::fs::read_dir(root.join("journals"))
                    .expect("the journal directory is readable")
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect();
                panic!(
                    "startup recovery looks for {}, but the engine wrote {names:?}",
                    expected_journal.display()
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the journal must be named from the allocated download id");

    // Wait on the durable record, not on the in-memory one: the claim is about what survives.
    let terminal = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let store = downpour_storage::metadata::MetadataStore::open(root.join("downpour.db"))
                .expect("the store opens");
            let records = store.load_downloads().expect("the store reads");
            if let Some(record) = records.first()
                && record.state == downpour_storage::metadata::DownloadState::Completed
            {
                return record.clone();
            }
            drop(store);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the daemon must record the download durably, from the probe through to completion");

    assert_eq!(
        terminal.total_length,
        Some(LENGTH),
        "the record must carry what the probe established"
    );
    assert!(
        matches!(
            terminal.identity.validator,
            downpour_types::Validator::StrongETag(_) | downpour_types::Validator::LastModified(_)
        ),
        "the record must carry a usable validator, or a resume has nothing to send If-Range with: \
         {:?}",
        terminal.identity.validator
    );

    // Everything the first instance held is now gone.
    drop(first);

    let mut second = TransferDaemon::new(config()).expect("the second daemon opens its store");
    let resumable = second.recover().expect("startup recovery runs");
    assert_eq!(
        resumable, 0,
        "a completed download is not resumable, and recovery must not offer it as one"
    );

    let view = second.handle(Request::DownloadGet(downpour_ipc::IdParams {
        protocol_version: PROTOCOL_VERSION,
        id: added.id.clone(),
    }));
    let Response::Download(view) = view else {
        panic!("a download recorded by a previous instance must be reachable: {view:?}");
    };
    assert_eq!(view.id, added.id, "the recorded id must round-trip");
    assert_eq!(view.state, WireState::Completed);
    assert_eq!(view.total, Some(LENGTH));

    // docs/04 §8 row 1, at the moment it applies rather than at the next restart. The startup
    // sweep would take this journal eventually, and "eventually" is however long the daemon runs;
    // a file per completed download until reboot is the growth B-30 is about.
    let retired = root
        .join("journals")
        .join(format!("{}.dpj", added.id.as_str()));
    assert!(
        !retired.exists(),
        "a verified and renamed download kept its journal at {}; §8 deletes it on completion",
        retired.display()
    );

    let bytes = std::fs::read(root.join("target").join("content")).expect("the file is there");
    assert_eq!(
        Content::new(11, LENGTH).first_mismatch(0, &bytes),
        None,
        "silent corruption through the recorded daemon path"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// B-25, end to end: a transfer interrupted by a process exit is finished by the next process.
///
/// The interrupted state is built directly rather than by killing a live transfer, because that
/// is exactly what a killed process leaves behind — durable artifacts with holes, and a record
/// still saying `Transferring` — and because the pool retries past a one-shot server gate from a
/// different offset, so there is no cheap way to hold a real transfer open.
///
/// Two durable runs with a hole between them and a hole at the end: the shape several workers
/// leave, not a truncated prefix.
#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_transfer_is_finished_by_the_next_process() {
    use downpour_corpus::content::Content;
    use downpour_corpus::server::{PathologyServer, ServerSpec};
    use downpour_storage::metadata::MetadataStore;

    const LENGTH: u64 = 8 * 1024 * 1024;
    const DURABLE: &[(u64, u64)] = &[(0, 2 * 1024 * 1024), (4 * 1024 * 1024, 6 * 1024 * 1024)];

    let server = PathologyServer::start(ServerSpec {
        content: Content::new(23, LENGTH),
        etag: Some("\"interrupted-v1\"".to_owned()),
        ..ServerSpec::default()
    })
    .await
    .expect("server starts");

    let root = std::env::temp_dir().join(format!(
        "downpour-interrupted-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    std::fs::create_dir_all(root.join("target")).expect("target dir");
    std::fs::create_dir_all(root.join("journals")).expect("journal dir");
    let config = TransferConfig {
        target_dir: root.join("target"),
        journal_dir: root.join("journals"),
        database_path: root.join("downpour.db"),
        transport_mode: TransportMode::Http1Only,
    };

    let id = leave_an_interrupted_download(&config, &server.entry_url(), LENGTH, DURABLE).await;

    // A fresh process. It has the directories and nothing else.
    let mut daemon = TransferDaemon::new(config).expect("the daemon opens its store");
    let resumable = daemon.recover().expect("startup recovery runs");
    assert_eq!(
        resumable, 1,
        "an interrupted download must reconcile to a resumable one"
    );
    let before = daemon.handle(Request::DownloadGet(downpour_ipc::IdParams {
        protocol_version: PROTOCOL_VERSION,
        id: id.clone(),
    }));
    let Response::Download(before) = before else {
        panic!("the recovered download must be reachable: {before:?}");
    };
    assert_eq!(
        before.state,
        WireState::Paused,
        "recovery establishes what is true and stops; it does not auto-resume"
    );
    assert_eq!(
        before.covered,
        DURABLE.iter().map(|(s, e)| e - s).sum::<u64>(),
        "the recovered record must carry what the journal proved"
    );

    let requests_before = server.requests().len();
    let response = daemon.handle(Request::DownloadResume(downpour_ipc::IdParams {
        protocol_version: PROTOCOL_VERSION,
        id: id.clone(),
    }));
    assert!(
        matches!(response, Response::Resumed(_)),
        "resume must be accepted now that the identity and the interval map are both durable: \
         {response:?}"
    );

    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let store = MetadataStore::open(root.join("downpour.db")).expect("the store opens");
            let records = store.load_downloads().expect("the store reads");
            if records.first().is_some_and(|record| {
                record.state == downpour_storage::metadata::DownloadState::Completed
            }) {
                return;
            }
            drop(store);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the resumed transfer must finish");

    // The same rule on the resume path: a download finished by a later process retires its
    // journal too, and this is the test where that journal was written by hand rather than by
    // the transfer, so nothing about its provenance can excuse it.
    let retired = root.join("journals").join(format!("{}.dpj", id.as_str()));
    assert!(
        !retired.exists(),
        "a resumed download that completed kept its journal at {}",
        retired.display()
    );

    let bytes = std::fs::read(root.join("target").join("content")).expect("the file is there");
    assert_eq!(
        Content::new(23, LENGTH).first_mismatch(0, &bytes),
        None,
        "silent corruption through the resumed daemon path"
    );

    // The claim that makes resume worth having: the bytes the journal already proved are not
    // fetched a second time.
    for request in &server.requests()[requests_before..] {
        let Some(range) = request.header("range") else {
            continue;
        };
        let Some((start, end)) = range
            .strip_prefix("bytes=")
            .and_then(|rest| rest.split_once('-'))
            .and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()? + 1)))
        else {
            continue;
        };
        for (done_start, done_end) in DURABLE {
            assert!(
                start >= *done_end || *done_start >= end,
                "the resume requested {start}-{end}, overlapping the durable range \
                 {done_start}-{done_end} the journal already proved"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// Write exactly what a process killed mid-transfer leaves behind, and return its id.
///
/// The identity comes from a real probe and the durable blocks go through the real
/// `DurableWriter`, so the journal records and the SQLite row are the ones a resume will actually
/// read rather than a fixture's idea of them.
async fn leave_an_interrupted_download(
    config: &TransferConfig,
    entry_url: &str,
    length: u64,
    durable: &[(u64, u64)],
) -> downpour_ipc::DownloadId {
    use downpour_corpus::content::Content;
    use downpour_http::{H1H2Backend, ProbeRequest, TransferProtocol as _};
    use downpour_intervals::{IntervalMap, WorkerId};
    use downpour_storage::journal::FileHeader;
    use downpour_storage::part_file::PartFile;
    use downpour_storage::writer::{DurableWriter, JournalFile};

    let backend = H1H2Backend::new(config.transport_mode).expect("backend");
    let remote = backend
        .probe(ProbeRequest::new(entry_url.parse().expect("url")))
        .await
        .expect("probe");

    let id = downpour_daemon::server::fresh_download_id().expect("mint an id");
    let stored = downpour_storage::metadata::DownloadId::try_from_bytes(hex_bytes(id.as_str()))
        .expect("the minted id is storable");

    let final_path = config.target_dir.join(
        remote
            .suggested_filename
            .clone()
            .unwrap_or_else(|| "download".to_owned()),
    );
    let part = PartFile::create(&final_path, length).expect("part file");
    let part_path = part.path().to_path_buf();
    let journal_path = config.journal_dir.join(format!("{}.dpj", id.as_str()));
    let journal = JournalFile::create(
        &journal_path,
        FileHeader::new(
            stored.as_bytes(),
            length,
            0,
            downpour_engine::validator_hash_of(&remote.validator),
        ),
    )
    .expect("journal");
    let mut writer = DurableWriter::try_new(part, journal, 0).expect("writer");
    let mut intervals = IntervalMap::new(length);
    let content = Content::new(23, length);
    let worker = WorkerId::new(0);
    for (start, end) in durable {
        intervals.grant(*start..*end, worker).expect("grant");
        writer
            .stage(
                &mut intervals,
                worker,
                *start,
                &content.range(*start, *end),
                std::time::Duration::ZERO,
            )
            .expect("stage");
        writer.flush(&mut intervals).expect("commit");
    }
    drop(writer);

    let url = downpour_storage::metadata::UrlReference::parse(remote.final_url.as_str(), None)
        .expect("public url");
    let origin = downpour_storage::metadata::PublicUrl::parse(&format!(
        "{}://{}",
        remote.final_url.scheme(),
        remote.final_url.authority()
    ))
    .expect("origin");
    let mut store = downpour_storage::metadata::MetadataStore::open(&config.database_path)
        .expect("store opens");
    store
        .save_download(&downpour_storage::metadata::DownloadMetadata {
            id: stored,
            // What a process killed mid-transfer leaves: still claiming to be transferring.
            state: downpour_storage::metadata::DownloadState::Transferring,
            created_at_ms: 1,
            updated_at_ms: 2,
            target_path: final_path,
            part_path,
            total_length: Some(length),
            covered_bytes: 0,
            queue_position: None,
            priority: 0,
            error_kind: None,
            space_reserved: true,
            url_history: Vec::new(),
            identity: downpour_storage::metadata::IdentityMetadata {
                current_url: url.clone(),
                final_url: Some(url.clone()),
                redirect_chain: vec![url],
                page_url: None,
                origin,
                validator: remote.validator.clone(),
                server_digest: remote.digest.clone(),
                content_type: remote.content_type.as_ref().map(ToString::to_string),
                suggested_filename: remote.suggested_filename.clone(),
                request_context_ref: None,
                probed_at_ms: 3,
                protocol: remote.protocol,
                range_support: remote.range_support.clone(),
            },
        })
        .expect("the interrupted record is valid");
    id
}

fn hex_bytes(text: &str) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).expect("hex");
    }
    bytes
}
