//! S2-T6 — strict metadata, canonical checkpoint, and identity persistence proofs.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use downpour_storage::journal::MAX_PAYLOAD_LEN;
use downpour_storage::metadata::{
    Checkpoint, CompleteInterval, DownloadErrorKind, DownloadId, DownloadMetadata, DownloadState,
    IdentityMetadata, MetadataError, MetadataFormat, MetadataStore, PublicUrl, SecretRef,
    UrlHistoryEntry, UrlReference, decode_identity_snapshot, encode_identity_snapshot,
    encode_range_observation,
};
use downpour_types::{
    ByteRangeSpec, ContentDigest, DigestAlgorithm, NegotiatedProtocol, RangeObservation,
    RangeProof, RangeProofError, RangeSupport, Validator,
};
use rusqlite::{Connection, params};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(tag: &str) -> Self {
        let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "downpour-metadata-{tag}-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn database(&self) -> PathBuf {
        self.0.join("downpour.db")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn valid_id_bytes() -> [u8; 16] {
    [
        0x01, 0x91, 0x23, 0x45, 0x67, 0x89, 0x7a, 0xbc, 0x8d, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89,
        0xab,
    ]
}

fn sample_id() -> DownloadId {
    DownloadId::try_from_bytes(valid_id_bytes()).expect("fixture is RFC-variant UUIDv7")
}

fn sample_proof() -> RangeProof {
    RangeProof::from_observed_response(
        ByteRangeSpec::FromTo { first: 0, last: 0 },
        206,
        Some("bytes 0-0/10"),
        None,
        1,
    )
    .expect("fixture is a valid observed range response")
}

fn public_url(raw: &str) -> UrlReference {
    UrlReference::parse(raw, None).expect("fixture contains no persistent secret")
}

fn sample_download() -> DownloadMetadata {
    let current = public_url("https://example.test/file");
    DownloadMetadata {
        id: sample_id(),
        state: DownloadState::Transferring,
        created_at_ms: 1,
        updated_at_ms: 2,
        target_path: PathBuf::from("target.bin"),
        part_path: PathBuf::from("target.bin.part"),
        total_length: Some(10),
        covered_bytes: 4,
        queue_position: Some(7),
        priority: 3,
        error_kind: None,
        space_reserved: true,
        identity: IdentityMetadata {
            current_url: current.clone(),
            final_url: None,
            redirect_chain: vec![current.clone()],
            page_url: None,
            origin: PublicUrl::parse("https://example.test/").expect("fixture is public"),
            validator: Validator::StrongETag("\"etag-v1\"".to_owned()),
            server_digest: Some(ContentDigest {
                algorithm: DigestAlgorithm::Sha256,
                encoded: "YWJj".to_owned(),
            }),
            content_type: Some("application/octet-stream".to_owned()),
            suggested_filename: Some("file.bin".to_owned()),
            request_context_ref: Some(
                SecretRef::new("keyring:downpour/request-1").expect("fixture is a keyring ref"),
            ),
            probed_at_ms: 3,
            protocol: NegotiatedProtocol::Http2,
            range_support: RangeSupport::Proven(sample_proof()),
        },
        url_history: vec![UrlHistoryEntry {
            url: current,
            seen_at_ms: 2,
        }],
    }
}

#[test]
fn schema_round_trips_and_refuses_newer_versions() {
    let directory = TestDirectory::new("schema");
    let path = directory.database();
    let mut store = MetadataStore::open(&path).expect("an empty database initializes as v1");

    let settings = store
        .connection_settings()
        .expect("the active connection exposes its verified policy");
    assert_eq!(settings.journal_mode(), "wal");
    assert_eq!(
        settings.synchronous(),
        1,
        "SQLite NORMAL is numeric value 1"
    );
    assert!(settings.foreign_keys());
    assert_eq!(settings.busy_timeout_ms(), 5_000);
    assert!(!settings.trusted_schema());

    let expected = sample_download();
    store
        .save_download(&expected)
        .expect("valid metadata is persisted atomically");
    assert_eq!(
        store
            .load_download(expected.id)
            .expect("valid metadata loads"),
        Some(expected)
    );
    drop(store);

    let connection = Connection::open(&path).unwrap();
    connection.pragma_update(None, "user_version", 2).unwrap();
    drop(connection);

    assert!(matches!(
        MetadataStore::open(&path),
        Err(MetadataError::NewerSchemaVersion {
            found: 2,
            supported: 1
        })
    ));
    let connection = Connection::open(&path).unwrap();
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        2,
        "refusing a newer schema must not downgrade it"
    );
}

#[test]
fn version_one_with_the_wrong_shape_is_refused() {
    let directory = TestDirectory::new("bad-v1");
    let path = directory.database();
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch("CREATE TABLE downloads (wrong INTEGER); PRAGMA user_version = 1;")
        .unwrap();
    drop(connection);

    assert!(matches!(
        MetadataStore::open(&path),
        Err(MetadataError::InvalidSchema { .. })
    ));
}

#[test]
fn version_one_without_required_constraints_is_refused() {
    let directory = TestDirectory::new("bad-v1-constraints");
    let path = directory.database();
    drop(MetadataStore::open(&path).unwrap());
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "PRAGMA foreign_keys = OFF;
             DROP TABLE checkpoints;
             CREATE TABLE checkpoints (
                 download_id BLOB PRIMARY KEY,
                 journal_seq INTEGER NOT NULL,
                 covered_bytes INTEGER NOT NULL,
                 interval_map BLOB NOT NULL,
                 written_at INTEGER NOT NULL
             );",
        )
        .unwrap();
    drop(connection);

    assert!(matches!(
        MetadataStore::open(&path),
        Err(MetadataError::InvalidSchema { .. })
    ));
}

#[test]
fn persisted_range_observation_is_revalidated_before_becoming_proven() {
    let directory = TestDirectory::new("range-proof");
    let path = directory.database();
    let mut store = MetadataStore::open(&path).unwrap();
    let expected = sample_download();
    store.save_download(&expected).unwrap();
    assert!(matches!(
        store.load_download(expected.id).unwrap(),
        Some(DownloadMetadata {
            identity: IdentityMetadata {
                range_support: RangeSupport::Proven(_),
                ..
            },
            ..
        })
    ));
    drop(store);

    let invalid = RangeObservation::new(
        ByteRangeSpec::FromTo { first: 0, last: 0 },
        200,
        Some("bytes 0-0/10".to_owned()),
        None,
        1,
    );
    let invalid_bytes = encode_range_observation(&invalid).unwrap();
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE identities SET range_observation = ?1 WHERE download_id = ?2",
            params![invalid_bytes, valid_id_bytes().as_slice()],
        )
        .unwrap();
    drop(connection);

    let store = MetadataStore::open(&path).unwrap();
    assert!(matches!(
        store.load_download(sample_id()),
        Err(MetadataError::InvalidRangeObservation(
            RangeProofError::StatusNot206 { status: 200 }
        ))
    ));
}

#[test]
fn newer_range_observation_is_refused_before_validation() {
    let directory = TestDirectory::new("range-version");
    let path = directory.database();
    let mut store = MetadataStore::open(&path).unwrap();
    let expected = sample_download();
    store.save_download(&expected).unwrap();
    drop(store);

    let mut newer = encode_range_observation(sample_proof().observation()).unwrap();
    newer[1] = 2;
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE identities SET range_observation = ?1 WHERE download_id = ?2",
            params![newer, valid_id_bytes().as_slice()],
        )
        .unwrap();
    drop(connection);

    let store = MetadataStore::open(&path).unwrap();
    assert!(matches!(
        store.load_download(sample_id()),
        Err(MetadataError::NewerFormatVersion {
            format: MetadataFormat::RangeObservation,
            found: 2,
            supported: 1
        })
    ));
}

#[test]
fn range_observation_bytes_are_exact_canonical_and_versioned() {
    let observation = RangeObservation::new(
        ByteRangeSpec::FromTo { first: 0, last: 0 },
        206,
        Some("bytes 0-0/10".to_owned()),
        None,
        1,
    );
    let expected = [
        0x88, 0x01, 0x00, 0x00, 0x00, 0x18, 0xce, 0x6c, b'b', b'y', b't', b'e', b's', b' ', b'0',
        b'-', b'0', b'/', b'1', b'0', 0xf6, 0x01,
    ];
    assert_eq!(encode_range_observation(&observation).unwrap(), expected);
}

#[test]
fn proven_range_requires_matching_download_length() {
    let directory = TestDirectory::new("range-total");
    let mut store = MetadataStore::open(directory.database()).unwrap();
    let mut missing = sample_download();
    missing.total_length = None;
    assert!(matches!(
        store.save_download(&missing),
        Err(MetadataError::InvalidValue {
            field: "identity.range_total_length"
        })
    ));

    let mut wrong = sample_download();
    wrong.total_length = Some(11);
    assert!(matches!(
        store.save_download(&wrong),
        Err(MetadataError::InvalidValue {
            field: "identity.range_total_length"
        })
    ));
    assert!(store.load_downloads().unwrap().is_empty());
}

#[test]
fn checkpoint_bytes_are_canonical_versioned_and_reject_invalid_coverage() {
    let checkpoint = Checkpoint::try_new(
        10,
        vec![
            CompleteInterval::try_new(0, 4).unwrap(),
            CompleteInterval::try_new(8, 10).unwrap(),
        ],
    )
    .unwrap();
    let expected = [
        0x84, 0x01, 0x0a, 0x06, 0x82, 0x82, 0x00, 0x04, 0x82, 0x08, 0x0a,
    ];
    assert_eq!(checkpoint.encode_cbor().unwrap(), expected);
    assert_eq!(Checkpoint::decode_cbor(&expected, 6).unwrap(), checkpoint);

    let newer = [
        0x84, 0x02, 0x0a, 0x06, 0x82, 0x82, 0x00, 0x04, 0x82, 0x08, 0x0a,
    ];
    assert!(matches!(
        Checkpoint::decode_cbor(&newer, 6),
        Err(MetadataError::NewerFormatVersion {
            format: MetadataFormat::Checkpoint,
            found: 2,
            supported: 1
        })
    ));

    let noncanonical = [
        0x84, 0x18, 0x01, 0x0a, 0x06, 0x82, 0x82, 0x00, 0x04, 0x82, 0x08, 0x0a,
    ];
    assert!(matches!(
        Checkpoint::decode_cbor(&noncanonical, 6),
        Err(MetadataError::NonCanonicalEncoding {
            format: MetadataFormat::Checkpoint
        })
    ));

    let overlap = [
        0x84, 0x01, 0x0a, 0x08, 0x82, 0x82, 0x00, 0x04, 0x82, 0x03, 0x07,
    ];
    assert!(matches!(
        Checkpoint::decode_cbor(&overlap, 8),
        Err(MetadataError::InvalidCheckpoint { .. })
    ));
    assert!(matches!(
        Checkpoint::decode_cbor(&expected, 5),
        Err(MetadataError::InvalidCheckpoint { .. })
    ));

    let mut trailing = expected.to_vec();
    trailing.push(0);
    assert!(matches!(
        Checkpoint::decode_cbor(&trailing, 6),
        Err(MetadataError::NonCanonicalEncoding {
            format: MetadataFormat::Checkpoint
        })
    ));
}

#[test]
fn checkpoint_sql_coverage_is_checked_when_the_cache_loads() {
    let directory = TestDirectory::new("checkpoint-sql");
    let path = directory.database();
    let mut store = MetadataStore::open(&path).unwrap();
    let download = sample_download();
    store.save_download(&download).unwrap();
    let checkpoint =
        Checkpoint::try_new(10, vec![CompleteInterval::try_new(0, 4).unwrap()]).unwrap();
    assert!(
        store
            .save_checkpoint(download.id, 9, 11, &checkpoint)
            .unwrap()
    );
    let persisted = store.load_checkpoint(download.id).unwrap().unwrap();
    assert_eq!(persisted.journal_sequence(), 9);
    assert_eq!(persisted.written_at_ms(), 11);
    assert_eq!(persisted.checkpoint(), &checkpoint);
    drop(store);

    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE checkpoints SET covered_bytes = 3 WHERE download_id = ?1",
            [valid_id_bytes().as_slice()],
        )
        .unwrap();
    drop(connection);
    let store = MetadataStore::open(&path).unwrap();
    assert!(matches!(
        store.load_checkpoint(sample_id()),
        Err(MetadataError::InvalidCheckpoint { .. })
    ));
}

#[test]
fn checkpoint_total_must_match_the_owning_download() {
    let directory = TestDirectory::new("checkpoint-total-save");
    let mut store = MetadataStore::open(directory.database()).unwrap();
    let download = sample_download();
    store.save_download(&download).unwrap();
    let wrong_total =
        Checkpoint::try_new(9, vec![CompleteInterval::try_new(0, 4).unwrap()]).unwrap();
    assert!(matches!(
        store.save_checkpoint(download.id, 9, 11, &wrong_total),
        Err(MetadataError::InvalidCheckpoint { .. })
    ));
    assert!(store.load_checkpoint(download.id).unwrap().is_none());
}

#[test]
fn checkpoint_total_is_rechecked_when_the_cache_loads() {
    let directory = TestDirectory::new("checkpoint-total-load");
    let path = directory.database();
    let mut store = MetadataStore::open(&path).unwrap();
    let download = sample_download();
    store.save_download(&download).unwrap();
    let checkpoint =
        Checkpoint::try_new(10, vec![CompleteInterval::try_new(0, 4).unwrap()]).unwrap();
    store
        .save_checkpoint(download.id, 9, 11, &checkpoint)
        .unwrap();
    drop(store);

    let wrong_total = Checkpoint::try_new(9, vec![CompleteInterval::try_new(0, 4).unwrap()])
        .unwrap()
        .encode_cbor()
        .unwrap();
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE checkpoints SET interval_map = ?1 WHERE download_id = ?2",
            params![wrong_total, valid_id_bytes().as_slice()],
        )
        .unwrap();
    drop(connection);

    let store = MetadataStore::open(&path).unwrap();
    assert!(matches!(
        store.load_checkpoint(sample_id()),
        Err(MetadataError::InvalidCheckpoint { .. })
    ));
}

#[test]
fn checkpoint_collection_limit_is_checked_before_allocation() {
    let mut malicious = vec![0x84, 0x01, 0x00, 0x00];
    cbor_head(&mut malicious, 4, 1_000_001);
    assert!(matches!(
        Checkpoint::decode_cbor(&malicious, 0),
        Err(MetadataError::CollectionLimitExceeded {
            format: MetadataFormat::Checkpoint,
            ..
        })
    ));
}

#[test]
fn identity_snapshots_are_canonical_versioned_and_secret_free() {
    let expected = sample_download();
    let encoded = encode_identity_snapshot(&expected).unwrap();
    assert_eq!(encoded, expected_identity_v1());
    assert_eq!(decode_identity_snapshot(&encoded).unwrap(), expected);

    let mut newer = encoded.clone();
    newer[1] = 2;
    assert!(matches!(
        decode_identity_snapshot(&newer),
        Err(MetadataError::NewerFormatVersion {
            format: MetadataFormat::IdentitySnapshot,
            found: 2,
            supported: 1
        })
    ));

    let mut noncanonical = vec![encoded[0], 0x18, 0x01];
    noncanonical.extend_from_slice(&encoded[2..]);
    assert!(matches!(
        decode_identity_snapshot(&noncanonical),
        Err(MetadataError::NonCanonicalEncoding {
            format: MetadataFormat::IdentitySnapshot
        })
    ));

    let mut trailing = encoded;
    trailing.push(0);
    assert!(matches!(
        decode_identity_snapshot(&trailing),
        Err(MetadataError::NonCanonicalEncoding {
            format: MetadataFormat::IdentitySnapshot
        })
    ));
}

#[test]
fn identity_snapshot_range_observation_is_revalidated() {
    let encoded = encode_identity_snapshot(&sample_download()).unwrap();
    let valid_observation = [
        0x88, 0x01, 0x00, 0x00, 0x00, 0x18, 0xce, 0x6c, b'b', b'y', b't', b'e', b's', b' ', b'0',
        b'-', b'0', b'/', b'1', b'0', 0xf6, 0x01,
    ];
    let start = encoded
        .windows(valid_observation.len())
        .position(|window| window == valid_observation)
        .expect("identity fixture embeds the separately pinned observation bytes");
    let mut invalid = encoded;
    invalid[start + 6] = 0xc8; // Still-canonical status 200 instead of 206.
    assert!(matches!(
        decode_identity_snapshot(&invalid),
        Err(MetadataError::InvalidRangeObservation(
            RangeProofError::StatusNot206 { status: 200 }
        ))
    ));
}

#[test]
fn identity_snapshot_respects_the_journal_payload_bound() {
    let mut download = sample_download();
    let long_public_url = format!("https://example.test/{}", "x".repeat(8_000));
    let long_reference = public_url(&long_public_url);
    download.identity.redirect_chain = vec![long_reference; 10];
    assert!(matches!(
        encode_identity_snapshot(&download),
        Err(MetadataError::PayloadTooLarge {
            format: MetadataFormat::IdentitySnapshot,
            maximum: MAX_PAYLOAD_LEN,
            ..
        })
    ));
}

#[test]
fn sqlite_never_contains_url_queries_or_request_secrets() {
    const URL_SECRET: &str = "TOP_SECRET_QUERY";
    const PASSWORD_SECRET: &str = "password";
    const REQUEST_SECRET: &str = "COOKIE_SECRET";

    let directory = TestDirectory::new("secrets");
    let path = directory.database();
    let url_ref = SecretRef::new("keyring:downpour/url-1").unwrap();
    let redacted = UrlReference::parse(
        &format!("https://user:password@example.test/file?signature={URL_SECRET}#private"),
        Some(url_ref.clone()),
    )
    .expect("a keyring reference authorizes redaction of the working URL");
    assert_eq!(redacted.public_url().as_str(), "https://example.test/file");
    assert_eq!(redacted.secret_ref(), Some(&url_ref));
    assert!(matches!(
        UrlReference::parse(
            &format!("https://example.test/file?signature={URL_SECRET}"),
            None
        ),
        Err(MetadataError::SecretReferenceRequired)
    ));
    assert!(matches!(
        SecretRef::new(&format!("Cookie: session={REQUEST_SECRET}")),
        Err(MetadataError::InvalidSecretReference { .. })
    ));

    let mut download = sample_download();
    download.identity.current_url = redacted.clone();
    download.identity.redirect_chain = vec![redacted.clone()];
    download.url_history = vec![UrlHistoryEntry {
        url: redacted,
        seen_at_ms: 4,
    }];
    download.identity.request_context_ref =
        Some(SecretRef::new("keyring:downpour/request-secret-fixture").unwrap());

    let identity_bytes = encode_identity_snapshot(&download).unwrap();
    assert!(!contains(&identity_bytes, URL_SECRET.as_bytes()));
    assert!(!contains(&identity_bytes, PASSWORD_SECRET.as_bytes()));
    assert!(!contains(&identity_bytes, REQUEST_SECRET.as_bytes()));

    let mut store = MetadataStore::open(&path).unwrap();
    store.save_download(&download).unwrap();
    drop(store);

    let connection = Connection::open(&path).unwrap();
    let (stored_url, stored_ref, request_ref): (String, Option<String>, Option<String>) =
        connection
            .query_row(
                "SELECT current_url, current_url_ref, request_context_ref FROM identities",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
    assert_eq!(stored_url, "https://example.test/file");
    assert_eq!(stored_ref.as_deref(), Some("keyring:downpour/url-1"));
    assert_eq!(
        request_ref.as_deref(),
        Some("keyring:downpour/request-secret-fixture")
    );
    drop(connection);

    for entry in fs::read_dir(directory.path()).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            let bytes = fs::read(path).unwrap();
            assert!(!contains(&bytes, URL_SECRET.as_bytes()));
            assert!(!contains(&bytes, PASSWORD_SECRET.as_bytes()));
            assert!(!contains(&bytes, REQUEST_SECRET.as_bytes()));
        }
    }
}

#[test]
fn error_kinds_are_typed_and_cannot_carry_server_text() {
    let kind = DownloadErrorKind::new("network.timeout").expect("stable kind is valid");
    assert_eq!(kind.as_str(), "network.timeout");
    assert!(matches!(
        DownloadErrorKind::new("GET https://example.test/file?signature=SERVER_SECRET"),
        Err(MetadataError::InvalidErrorKind { .. })
    ));

    let directory = TestDirectory::new("error-kind");
    let mut store = MetadataStore::open(directory.database()).unwrap();
    let mut expected = sample_download();
    expected.error_kind = Some(kind);
    store.save_download(&expected).unwrap();
    assert_eq!(store.load_download(expected.id).unwrap(), Some(expected));
}

#[test]
fn url_history_preserves_order_when_timestamps_match() {
    let directory = TestDirectory::new("history-order");
    let mut store = MetadataStore::open(directory.database()).unwrap();
    let mut expected = sample_download();
    expected.url_history = vec![
        UrlHistoryEntry {
            url: public_url("https://example.test/first"),
            seen_at_ms: 5,
        },
        UrlHistoryEntry {
            url: public_url("https://example.test/second"),
            seen_at_ms: 5,
        },
    ];
    store.save_download(&expected).unwrap();
    assert_eq!(store.load_download(expected.id).unwrap(), Some(expected));
}

#[test]
fn metadata_enum_and_optional_variants_round_trip_without_defaults() {
    let directory = TestDirectory::new("variants");
    let mut store = MetadataStore::open(directory.database()).unwrap();
    let mut expected = sample_download();
    expected.state = DownloadState::AwaitingRefresh;
    expected.identity.final_url = Some(public_url("https://cdn.example.test/final"));
    expected.identity.page_url = Some(public_url("https://example.test/page"));
    expected.identity.redirect_chain = vec![
        public_url("https://example.test/file"),
        public_url("https://cdn.example.test/final"),
    ];
    expected.identity.validator = Validator::LastModified("Tue, 04 Aug 2026 12:00:00 GMT".into());
    expected.identity.server_digest = Some(ContentDigest {
        algorithm: DigestAlgorithm::Sha512,
        encoded: "ZGVm".to_owned(),
    });
    expected.identity.protocol = NegotiatedProtocol::Http3;
    expected.identity.range_support = RangeSupport::Absent;
    assert_eq!(
        decode_identity_snapshot(&encode_identity_snapshot(&expected).unwrap()).unwrap(),
        expected
    );
    store.save_download(&expected).unwrap();
    assert_eq!(store.load_download(expected.id).unwrap(), Some(expected));

    let mut unknown = sample_download();
    unknown.identity.validator = Validator::None;
    unknown.identity.server_digest = None;
    unknown.identity.protocol = NegotiatedProtocol::Http11;
    unknown.identity.range_support = RangeSupport::Unknown;
    assert_eq!(
        decode_identity_snapshot(&encode_identity_snapshot(&unknown).unwrap()).unwrap(),
        unknown
    );
    store.save_download(&unknown).unwrap();
    assert_eq!(store.load_download(unknown.id).unwrap(), Some(unknown));
}

#[test]
fn unversioned_nonempty_database_is_refused_without_mutation() {
    let directory = TestDirectory::new("unversioned");
    let path = directory.database();
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE legacy_marker (value TEXT); INSERT INTO legacy_marker VALUES ('keep');",
        )
        .unwrap();
    let before = schema_objects(&connection);
    drop(connection);

    assert!(matches!(
        MetadataStore::open(&path),
        Err(MetadataError::UnversionedNonemptyDatabase)
    ));

    let connection = Connection::open(&path).unwrap();
    assert_eq!(schema_objects(&connection), before);
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        connection
            .query_row("SELECT value FROM legacy_marker", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "keep"
    );
}

#[test]
fn unversioned_database_with_only_freelist_pages_is_refused() {
    let directory = TestDirectory::new("unversioned-freelist");
    let path = directory.database();
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE remnant (value TEXT);
             INSERT INTO remnant VALUES ('old-private-data');
             DROP TABLE remnant;",
        )
        .unwrap();
    assert!(schema_objects(&connection).is_empty());
    assert!(
        connection
            .pragma_query_value(None, "page_count", |row| row.get::<_, i64>(0))
            .unwrap()
            > 0
    );
    drop(connection);

    assert!(matches!(
        MetadataStore::open(&path),
        Err(MetadataError::UnversionedNonemptyDatabase)
    ));
}

#[test]
fn download_ids_refuse_non_v7_bytes() {
    let mut wrong_version = valid_id_bytes();
    wrong_version[6] = 0x60;
    assert!(matches!(
        DownloadId::try_from_bytes(wrong_version),
        Err(MetadataError::InvalidDownloadId { .. })
    ));

    let mut wrong_variant = valid_id_bytes();
    wrong_variant[8] = 0x40;
    assert!(matches!(
        DownloadId::try_from_bytes(wrong_variant),
        Err(MetadataError::InvalidDownloadId { .. })
    ));

    let directory = TestDirectory::new("bad-id-row");
    let path = directory.database();
    let mut store = MetadataStore::open(&path).unwrap();
    store.save_download(&sample_download()).unwrap();
    drop(store);
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "INSERT INTO downloads
             (id, state, created_at, updated_at, target_path, part_path, covered_bytes, priority, space_reserved)
             SELECT ?1, state, created_at, updated_at, target_path, part_path, covered_bytes, priority, space_reserved
             FROM downloads LIMIT 1",
            [wrong_version.as_slice()],
        )
        .unwrap();
    drop(connection);
    let store = MetadataStore::open(&path).unwrap();
    assert!(matches!(
        store.load_downloads(),
        Err(MetadataError::InvalidDownloadId { .. })
    ));
}

#[test]
fn sqlite_integer_overflow_is_rejected_before_a_transaction() {
    let directory = TestDirectory::new("integer-bound");
    let mut store = MetadataStore::open(directory.database()).unwrap();
    let mut download = sample_download();
    download.covered_bytes = 0;
    download.identity.range_support = RangeSupport::Absent;
    download.total_length = Some((i64::MAX as u64) + 1);
    assert!(matches!(
        store.save_download(&download),
        Err(MetadataError::IntegerOutOfRange {
            field: "total_length",
            ..
        })
    ));
    assert!(store.load_downloads().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn native_paths_round_trip_without_loss() {
    use std::os::unix::ffi::OsStringExt;

    let directory = TestDirectory::new("unix-path");
    let mut store = MetadataStore::open(directory.database()).unwrap();
    let mut expected = sample_download();
    expected.target_path = PathBuf::from(OsString::from_vec(vec![b't', 0xff, b'g']));
    expected.part_path = PathBuf::from(OsString::from_vec(vec![b'p', 0xfe]));
    assert_eq!(
        decode_identity_snapshot(&encode_identity_snapshot(&expected).unwrap()).unwrap(),
        expected
    );
    store.save_download(&expected).unwrap();
    assert_eq!(store.load_download(expected.id).unwrap(), Some(expected));
}

#[cfg(windows)]
#[test]
fn native_paths_round_trip_without_loss() {
    use std::os::windows::ffi::OsStringExt;

    let directory = TestDirectory::new("windows-path");
    let mut store = MetadataStore::open(directory.database()).unwrap();
    let mut expected = sample_download();
    expected.target_path = PathBuf::from(OsString::from_wide(&[b't' as u16, 0xd800, b'g' as u16]));
    expected.part_path = PathBuf::from(OsString::from_wide(&[b'p' as u16, 0xdfff]));
    assert_eq!(
        decode_identity_snapshot(&encode_identity_snapshot(&expected).unwrap()).unwrap(),
        expected
    );
    store.save_download(&expected).unwrap();
    assert_eq!(store.load_download(expected.id).unwrap(), Some(expected));
}

fn schema_objects(connection: &Connection) -> Vec<(String, String)> {
    let mut statement = connection
        .prepare(
            "SELECT type, name FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
        )
        .unwrap();
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn expected_identity_v1() -> Vec<u8> {
    let mut out = Vec::new();
    cbor_head(&mut out, 4, 15);
    cbor_uint(&mut out, 1);
    cbor_bytes(&mut out, &valid_id_bytes());
    cbor_uint(&mut out, 3);
    cbor_uint(&mut out, 1);
    cbor_uint(&mut out, 2);
    cbor_path(&mut out, b"target.bin");
    cbor_path(&mut out, b"target.bin.part");
    cbor_uint(&mut out, 10);
    cbor_uint(&mut out, 4);
    cbor_uint(&mut out, 7);
    cbor_uint(&mut out, 3);
    cbor_null(&mut out);
    cbor_bool(&mut out, true);

    cbor_head(&mut out, 4, 14);
    cbor_url_ref(&mut out, "https://example.test/file", None);
    cbor_null(&mut out);
    cbor_head(&mut out, 4, 1);
    cbor_url_ref(&mut out, "https://example.test/file", None);
    cbor_null(&mut out);
    cbor_text(&mut out, "https://example.test/");
    cbor_head(&mut out, 4, 2);
    cbor_uint(&mut out, 0);
    cbor_text(&mut out, "\"etag-v1\"");
    cbor_head(&mut out, 4, 2);
    cbor_uint(&mut out, 0);
    cbor_text(&mut out, "YWJj");
    cbor_text(&mut out, "application/octet-stream");
    cbor_text(&mut out, "file.bin");
    cbor_text(&mut out, "keyring:downpour/request-1");
    cbor_uint(&mut out, 3);
    cbor_uint(&mut out, 1);
    cbor_uint(&mut out, 0);
    let observation = RangeObservation::new(
        ByteRangeSpec::FromTo { first: 0, last: 0 },
        206,
        Some("bytes 0-0/10".to_owned()),
        None,
        1,
    );
    cbor_bytes(
        &mut out,
        &encode_range_observation(&observation).expect("range fixture encodes"),
    );

    cbor_head(&mut out, 4, 1);
    cbor_head(&mut out, 4, 2);
    cbor_url_ref(&mut out, "https://example.test/file", None);
    cbor_uint(&mut out, 2);
    out
}

fn cbor_path(out: &mut Vec<u8>, bytes: &[u8]) {
    cbor_head(out, 4, 2);
    cbor_uint(out, 0);
    cbor_bytes(out, bytes);
}

fn cbor_url_ref(out: &mut Vec<u8>, public: &str, secret_ref: Option<&str>) {
    cbor_head(out, 4, 2);
    cbor_text(out, public);
    match secret_ref {
        Some(reference) => cbor_text(out, reference),
        None => cbor_null(out),
    }
}

fn cbor_bool(out: &mut Vec<u8>, value: bool) {
    out.push(if value { 0xf5 } else { 0xf4 });
}

fn cbor_null(out: &mut Vec<u8>) {
    out.push(0xf6);
}

fn cbor_uint(out: &mut Vec<u8>, value: u64) {
    cbor_head(out, 0, value);
}

fn cbor_text(out: &mut Vec<u8>, value: &str) {
    cbor_head(out, 3, u64::try_from(value.len()).unwrap());
    out.extend_from_slice(value.as_bytes());
}

fn cbor_bytes(out: &mut Vec<u8>, value: &[u8]) {
    cbor_head(out, 2, u64::try_from(value.len()).unwrap());
    out.extend_from_slice(value);
}

fn cbor_head(out: &mut Vec<u8>, major: u8, value: u64) {
    let prefix = major << 5;
    match value {
        0..=23 => out.push(prefix | u8::try_from(value).unwrap()),
        24..=0xff => {
            out.push(prefix | 24);
            out.push(u8::try_from(value).unwrap());
        }
        0x100..=0xffff => {
            out.push(prefix | 25);
            out.extend_from_slice(&u16::try_from(value).unwrap().to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(prefix | 26);
            out.extend_from_slice(&u32::try_from(value).unwrap().to_be_bytes());
        }
        _ => {
            out.push(prefix | 27);
            out.extend_from_slice(&value.to_be_bytes());
        }
    }
}

/// Every error kind the engine can actually produce must be storable.
///
/// B-65. `DownloadErrorKind` accepted `network.timeout` and rejected `segmented_transfer`,
/// because its grammar allowed hyphens between dots and not underscores — while every kind the
/// engine emits is snake_case, and `docs/08-ipc-and-ui-spec.md` §3's own worked example is
/// `"kind": "validator_mismatch"`. Fifteen of the thirty-five kinds in the codebase contain an
/// underscore.
///
/// The consequence was not a rejected string. `write_terminal` in the daemon builds the kind and
/// the state in one operation, so a kind the store refused discarded the *state* with it: a
/// download that failed stayed `Paused` for ever, the user was never told, and the next startup
/// tried to reconcile a transfer that was already dead.
#[test]
fn every_error_kind_the_engine_emits_is_storable() {
    // Taken from the `kind()` implementations in downpour-http, downpour-engine and
    // downpour-storage, which are the API clients switch on (docs/08 §3).
    for kind in [
        "transport",
        "timeout",
        "truncated_body",
        "unexpected_status",
        "looks_like_error_page",
        "unusable_range_response",
        "over_delivery",
        "validator_mismatch",
        "unexpected_content_encoding",
        "too_many_redirects",
        "redirect_without_location",
        "unusable_redirect_target",
        "needs_refresh",
        "segmented_transfer",
        "resume_length_mismatch",
        "target_exists",
        "digest_mismatch",
        "unverified",
        "incomplete",
        // The dotted form stays valid; this is a widening, not a replacement.
        "network.timeout",
        "storage.part-file-missing",
    ] {
        assert!(
            DownloadErrorKind::new(kind).is_ok(),
            "{kind} is a kind the engine emits and the store refused it, which discards the \
             download state written alongside it"
        );
    }
}

/// Widening the grammar must not let server text back in. I-14.
#[test]
fn an_error_kind_still_cannot_carry_server_text() {
    for rejected in [
        "GET https://example.test/file?signature=SERVER_SECRET",
        "Set-Cookie: session=abc",
        "UPPERCASE_KIND",
        "kind with spaces",
        "",
        "_leading",
        "trailing_",
    ] {
        assert!(
            matches!(
                DownloadErrorKind::new(rejected),
                Err(MetadataError::InvalidErrorKind { .. })
            ),
            "{rejected:?} was accepted as an error kind"
        );
    }
}
