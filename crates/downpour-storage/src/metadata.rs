//! Strict, versioned metadata and disposable checkpoint persistence.
//!
//! SQLite is the queryable copy of download identity while the append-only journal remains
//! authoritative for durable byte coverage (ADR-0004). This module owns I-6's persisted range
//! evidence, I-8's restart-stable remote identity, I-11's version refusal, and I-14's boundary
//! between public URL components and keyring references.

use std::fmt;
use std::io::Cursor;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ciborium::{from_reader, into_writer};
use downpour_types::{
    ByteRangeSpec, ContentDigest, DigestAlgorithm, NegotiatedProtocol, RangeObservation,
    RangeProof, RangeProofError, RangeSupport, Validator,
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::de::{DeserializeOwned, SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use url::Url;

use crate::journal::MAX_PAYLOAD_LEN;

const SCHEMA_VERSION: u32 = 1;
const FORMAT_VERSION: u64 = 1;
const BUSY_TIMEOUT_MS: u64 = 5_000;
const MAX_RANGE_OBSERVATION_BYTES: usize = 16 * 1024;
const MAX_CHECKPOINT_BYTES: usize = 64 * 1024 * 1024;
const MAX_CHECKPOINT_INTERVALS: usize = 1_000_000;
const MAX_REDIRECTS: usize = 64;
const MAX_HISTORY_ENTRIES: usize = 4_096;
const MAX_URL_BYTES: usize = 8_192;
const MAX_HEADER_BYTES: usize = 8_192;
const MAX_CONTENT_TYPE_BYTES: usize = 512;
const MAX_FILENAME_BYTES: usize = 4_096;
const MAX_SECRET_REF_BYTES: usize = 256;
const MAX_ERROR_KIND_BYTES: usize = 128;
const MAX_PATH_BYTES: usize = 64 * 1024;

type SchemaObject = (String, String, String, Option<String>);

const SCHEMA_SQL: &str = r#"
CREATE TABLE downloads (
    id                BLOB PRIMARY KEY CHECK(length(id) = 16),
    state             TEXT NOT NULL,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    target_path       BLOB NOT NULL,
    part_path         BLOB NOT NULL,
    total_length      INTEGER,
    covered_bytes     INTEGER NOT NULL DEFAULT 0,
    queue_position    INTEGER,
    priority          INTEGER NOT NULL DEFAULT 0,
    error_kind        TEXT,
    space_reserved    INTEGER NOT NULL DEFAULT 0 CHECK(space_reserved IN (0, 1))
);
CREATE TABLE identities (
    download_id       BLOB PRIMARY KEY REFERENCES downloads(id) ON DELETE CASCADE,
    current_url       TEXT NOT NULL,
    current_url_ref   TEXT,
    final_url         TEXT,
    final_url_ref     TEXT,
    page_url          TEXT,
    page_url_ref      TEXT,
    origin            TEXT NOT NULL,
    validator_kind    TEXT NOT NULL,
    validator_value   TEXT,
    server_digest     TEXT,
    content_type      TEXT,
    suggested_name    TEXT,
    request_context_ref TEXT,
    probed_at         INTEGER NOT NULL,
    protocol          TEXT NOT NULL,
    range_state       TEXT NOT NULL CHECK(range_state IN ('proven', 'absent', 'unknown')),
    range_observation BLOB,
    CHECK((range_state = 'proven' AND range_observation IS NOT NULL)
       OR (range_state != 'proven' AND range_observation IS NULL))
);
CREATE TABLE redirect_chain (
    download_id       BLOB NOT NULL REFERENCES downloads(id) ON DELETE CASCADE,
    hop               INTEGER NOT NULL CHECK(hop >= 0),
    public_url        TEXT NOT NULL,
    secret_ref        TEXT,
    PRIMARY KEY (download_id, hop)
);
CREATE TABLE url_history (
    download_id       BLOB NOT NULL REFERENCES downloads(id) ON DELETE CASCADE,
    entry              INTEGER NOT NULL CHECK(entry >= 0),
    public_url        TEXT NOT NULL,
    secret_ref        TEXT,
    seen_at           INTEGER NOT NULL,
    PRIMARY KEY (download_id, entry)
);
CREATE TABLE checkpoints (
    download_id       BLOB PRIMARY KEY REFERENCES downloads(id) ON DELETE CASCADE,
    journal_seq       INTEGER NOT NULL,
    covered_bytes     INTEGER NOT NULL,
    interval_map      BLOB NOT NULL,
    written_at        INTEGER NOT NULL
);
CREATE TABLE compat_profiles (
    origin            TEXT PRIMARY KEY,
    range_support     TEXT NOT NULL,
    max_useful_conns  INTEGER,
    observed_protocol TEXT,
    notes             TEXT,
    updated_at        INTEGER NOT NULL
);
CREATE INDEX idx_downloads_state ON downloads(state);
CREATE INDEX idx_downloads_queue ON downloads(queue_position) WHERE queue_position IS NOT NULL;
PRAGMA user_version = 1;
"#;

/// Which independently versioned metadata representation failed validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetadataFormat {
    /// Raw response facts used to rebuild a range proof.
    RangeObservation,
    /// SQLite's disposable interval checkpoint.
    Checkpoint,
    /// A complete identity snapshot carried by a journal record.
    IdentitySnapshot,
    /// A native target or part path.
    NativePath,
}

impl fmt::Display for MetadataFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::RangeObservation => "range observation",
            Self::Checkpoint => "checkpoint",
            Self::IdentitySnapshot => "identity snapshot",
            Self::NativePath => "native path",
        })
    }
}

/// A failure at the strict metadata boundary.
#[derive(Debug, Error)]
pub enum MetadataError {
    /// SQLite rejected an operation.
    #[error("SQLite metadata operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A database was created by a newer binary.
    #[error("SQLite schema version {found} is newer than supported version {supported}")]
    NewerSchemaVersion {
        /// Version found in `PRAGMA user_version`.
        found: u32,
        /// Newest version understood by this binary.
        supported: u32,
    },
    /// A nonempty database failed to identify its schema.
    #[error("nonempty SQLite database has user_version 0")]
    UnversionedNonemptyDatabase,
    /// A version-one schema did not have the required shape.
    #[error("invalid SQLite schema v1: {reason}")]
    InvalidSchema {
        /// Structural mismatch, containing names but never row values.
        reason: String,
    },
    /// An applied connection pragma did not read back as required.
    #[error("SQLite connection policy was not applied: {reason}")]
    InvalidConnectionPolicy {
        /// The policy mismatch.
        reason: String,
    },
    /// A CBOR payload was produced by a newer metadata format.
    #[error("{format} version {found} is newer than supported version {supported}")]
    NewerFormatVersion {
        /// The affected format.
        format: MetadataFormat,
        /// Version found in the payload.
        found: u64,
        /// Newest version understood by this binary.
        supported: u64,
    },
    /// A CBOR payload used an unsupported older or zero version.
    #[error("unsupported {format} version {found}")]
    UnsupportedFormatVersion {
        /// The affected format.
        format: MetadataFormat,
        /// Version found in the payload.
        found: u64,
    },
    /// CBOR was structurally invalid for its declared version.
    #[error("invalid {format} encoding: {reason}")]
    InvalidEncoding {
        /// The affected format.
        format: MetadataFormat,
        /// A non-secret structural description.
        reason: String,
    },
    /// Valid CBOR represented the same value with forbidden alternative bytes.
    #[error("{format} is not in its canonical versioned encoding")]
    NonCanonicalEncoding {
        /// The affected format.
        format: MetadataFormat,
    },
    /// A bounded collection advertised too many entries.
    #[error("{format} collection has {actual} entries; maximum is {maximum}")]
    CollectionLimitExceeded {
        /// The affected format.
        format: MetadataFormat,
        /// Entry count declared by the payload.
        actual: u64,
        /// Maximum accepted count.
        maximum: usize,
    },
    /// An encoded payload exceeded its persistent container.
    #[error("{format} payload has {actual} bytes; maximum is {maximum}")]
    PayloadTooLarge {
        /// The affected format.
        format: MetadataFormat,
        /// Encoded byte count.
        actual: usize,
        /// Maximum accepted byte count.
        maximum: usize,
    },
    /// A complete-interval set was not a valid coverage summary.
    #[error("invalid checkpoint: {reason}")]
    InvalidCheckpoint {
        /// The failed coverage rule.
        reason: String,
    },
    /// Persisted response facts did not reproduce a valid range proof.
    #[error("persisted range observation is not proof: {0}")]
    InvalidRangeObservation(#[source] RangeProofError),
    /// UUID bytes were not an RFC-variant version-7 identifier.
    #[error("invalid download id: {reason}")]
    InvalidDownloadId {
        /// The failed UUID property.
        reason: &'static str,
    },
    /// A URL contained secret-bearing components without a keyring reference.
    #[error("a URL with userinfo, query, or fragment requires a keyring reference")]
    SecretReferenceRequired,
    /// A keyring identifier did not use Downpour's non-secret reference grammar.
    #[error("invalid keyring reference: {reason}")]
    InvalidSecretReference {
        /// The failed grammar rule.
        reason: &'static str,
    },
    /// A failure category was not a bounded lowercase dotted identifier.
    #[error("invalid download error kind: {reason}")]
    InvalidErrorKind {
        /// The failed grammar rule.
        reason: &'static str,
    },
    /// A public URL was invalid or still contained a secret-bearing component.
    #[error("invalid public URL: {reason}")]
    InvalidPublicUrl {
        /// The failed public-URL rule.
        reason: &'static str,
    },
    /// A field exceeded its declared bound.
    #[error("metadata field {field} has {actual} bytes; maximum is {maximum}")]
    FieldTooLong {
        /// Stable field name.
        field: &'static str,
        /// Actual encoded byte count.
        actual: usize,
        /// Maximum accepted byte count.
        maximum: usize,
    },
    /// An unsigned domain value did not fit SQLite's signed integer domain.
    #[error("metadata field {field} value {value} does not fit SQLite INTEGER")]
    IntegerOutOfRange {
        /// Stable field name.
        field: &'static str,
        /// Rejected unsigned value.
        value: u64,
    },
    /// A native-only path encoding was opened on a different platform.
    #[error("native path encoding {encoding} is unsupported on this platform")]
    UnsupportedPathEncoding {
        /// Stable encoding discriminator.
        encoding: u8,
    },
    /// A persisted enum discriminator or field combination was invalid.
    #[error("invalid metadata value for {field}")]
    InvalidValue {
        /// Stable field name.
        field: &'static str,
    },
}

/// A checked 16-byte, RFC-variant UUIDv7 download identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DownloadId([u8; 16]);

impl DownloadId {
    /// Validates bytes shared by SQLite and the recovery journal.
    pub fn try_from_bytes(bytes: [u8; 16]) -> Result<Self, MetadataError> {
        if bytes[6] >> 4 != 7 {
            return Err(MetadataError::InvalidDownloadId {
                reason: "UUID version bits are not 7",
            });
        }
        if bytes[8] & 0xc0 != 0x80 {
            return Err(MetadataError::InvalidDownloadId {
                reason: "UUID variant bits are not RFC 4122",
            });
        }
        Ok(Self(bytes))
    }

    /// Returns the exact bytes used as the SQLite key and journal transfer id.
    #[must_use]
    pub const fn as_bytes(self) -> [u8; 16] {
        self.0
    }
}

/// A query-free, credential-free HTTP(S) URL safe for plaintext persistence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicUrl(Url);

impl PublicUrl {
    /// Parses a URL only when all secret-bearing components are absent.
    pub fn parse(raw: &str) -> Result<Self, MetadataError> {
        let url = Url::parse(raw).map_err(|_| MetadataError::InvalidPublicUrl {
            reason: "URL does not parse",
        })?;
        Self::try_from_url(url)
    }

    fn try_from_url(url: Url) -> Result<Self, MetadataError> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(MetadataError::InvalidPublicUrl {
                reason: "scheme is not HTTP or HTTPS",
            });
        }
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(MetadataError::InvalidPublicUrl {
                reason: "userinfo, query, or fragment is present",
            });
        }
        check_len("public_url", url.as_str(), MAX_URL_BYTES)?;
        Ok(Self(url))
    }

    /// Returns the normalized public URL.
    #[must_use]
    pub const fn as_url(&self) -> &Url {
        &self.0
    }

    /// Returns the normalized URL text stored in SQLite and CBOR.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// An opaque OS-keyring locator, never the credential or URL it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretRef(String);

impl SecretRef {
    /// Validates Downpour's keyring-reference namespace and safe character set.
    pub fn new(raw: &str) -> Result<Self, MetadataError> {
        if raw.len() > MAX_SECRET_REF_BYTES {
            return Err(MetadataError::InvalidSecretReference {
                reason: "reference is too long",
            });
        }
        let Some(suffix) = raw.strip_prefix("keyring:downpour/") else {
            return Err(MetadataError::InvalidSecretReference {
                reason: "reference is outside keyring:downpour/",
            });
        };
        if suffix.is_empty()
            || !suffix.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/')
            })
        {
            return Err(MetadataError::InvalidSecretReference {
                reason: "reference suffix has an invalid character",
            });
        }
        Ok(Self(raw.to_owned()))
    }

    /// Returns the non-secret keyring locator.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A persistent URL split into public text and an optional keyring locator for its full value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UrlReference {
    public_url: PublicUrl,
    secret_ref: Option<SecretRef>,
}

impl UrlReference {
    /// Parses a working URL, requiring a keyring locator before removing any userinfo, query,
    /// or fragment from the persistent representation.
    pub fn parse(raw: &str, secret_ref: Option<SecretRef>) -> Result<Self, MetadataError> {
        let mut url = Url::parse(raw).map_err(|_| MetadataError::InvalidPublicUrl {
            reason: "URL does not parse",
        })?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(MetadataError::InvalidPublicUrl {
                reason: "scheme is not HTTP or HTTPS",
            });
        }
        let has_private_component = !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some();
        if has_private_component && secret_ref.is_none() {
            return Err(MetadataError::SecretReferenceRequired);
        }
        if has_private_component {
            url.set_username("")
                .map_err(|()| MetadataError::InvalidPublicUrl {
                    reason: "URL userinfo cannot be removed",
                })?;
            url.set_password(None)
                .map_err(|()| MetadataError::InvalidPublicUrl {
                    reason: "URL password cannot be removed",
                })?;
            url.set_query(None);
            url.set_fragment(None);
        }
        Ok(Self {
            public_url: PublicUrl::try_from_url(url)?,
            secret_ref,
        })
    }

    /// Returns the query-free public URL.
    #[must_use]
    pub const fn public_url(&self) -> &PublicUrl {
        &self.public_url
    }

    /// Returns the locator for the full secret-bearing URL, when one was needed.
    #[must_use]
    pub const fn secret_ref(&self) -> Option<&SecretRef> {
        self.secret_ref.as_ref()
    }
}

/// Lifecycle state persisted for queue and restart decisions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownloadState {
    /// Accepted and queued.
    Submitted,
    /// Capability probe in flight.
    Probing,
    /// Identity known and transfer planned.
    Planned,
    /// One or more workers active.
    Transferring,
    /// User-initiated pause.
    Paused,
    /// Waiting under retry backoff.
    Stalled,
    /// Waiting for a refreshed URL or credential.
    AwaitingRefresh,
    /// All bytes present and verification running.
    Verifying,
    /// Verified and renamed.
    Completed,
    /// Unrecoverable, with partial artifacts retained.
    Failed,
}

/// A stable failure category that cannot carry arbitrary server text or a URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadErrorKind(String);

impl DownloadErrorKind {
    /// Validates a bounded lowercase identifier such as `validator_mismatch` or
    /// `network.timeout`.
    ///
    /// Underscores are permitted inside a segment, and that is not cosmetic. Every kind the
    /// engine emits is snake_case and `docs/08-ipc-and-ui-spec.md` §3's worked example is
    /// `"kind": "validator_mismatch"`, so a grammar that refused them refused the vocabulary the
    /// spec defines. It did not refuse it loudly: the daemon builds a kind and a download's
    /// terminal state in one operation, so a rejected kind discarded the state with it and a
    /// download that had failed stayed `Paused` for ever (B-65).
    ///
    /// What must stay refused is server text. A kind is stored and displayed, and a signed URL or
    /// a `Set-Cookie` pasted into one would put a secret somewhere I-14 says it may never be, so
    /// uppercase, spaces, punctuation and a leading or trailing separator are all still errors.
    pub fn new(raw: &str) -> Result<Self, MetadataError> {
        if raw.is_empty() || raw.len() > MAX_ERROR_KIND_BYTES {
            return Err(MetadataError::InvalidErrorKind {
                reason: "identifier is empty or too long",
            });
        }
        if !raw.split('.').all(|segment| {
            let mut bytes = segment.bytes();
            bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
                && bytes.all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || byte == b'-'
                        || byte == b'_'
                })
                && !segment.ends_with(['-', '_'])
        }) {
            return Err(MetadataError::InvalidErrorKind {
                reason: "identifier is not a lowercase dotted or underscored identifier",
            });
        }
        Ok(Self(raw.to_owned()))
    }

    /// Returns the stable non-secret category text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Persisted remote identity used to resume the same representation after restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityMetadata {
    /// Submitted or most recently refreshed URL reference.
    pub current_url: UrlReference,
    /// Redirect destination that actually worked.
    pub final_url: Option<UrlReference>,
    /// Every observed redirect hop in order (I-8).
    pub redirect_chain: Vec<UrlReference>,
    /// Browser page that initiated the download, when known.
    pub page_url: Option<UrlReference>,
    /// Query-free origin used for compatibility knowledge.
    pub origin: PublicUrl,
    /// Representation validator used by resume policy.
    pub validator: Validator,
    /// Server-supplied end-to-end digest, when offered.
    pub server_digest: Option<ContentDigest>,
    /// Response media type.
    pub content_type: Option<String>,
    /// Server-suggested filename before sanitization.
    pub suggested_filename: Option<String>,
    /// Keyring locator for cookies, authorization, and other request context.
    pub request_context_ref: Option<SecretRef>,
    /// Probe time as Unix milliseconds.
    pub probed_at_ms: u64,
    /// Negotiated HTTP protocol.
    pub protocol: NegotiatedProtocol,
    /// Range capability, with raw evidence retained inside a proven value.
    pub range_support: RangeSupport,
}

/// One query-free URL-history observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UrlHistoryEntry {
    /// Public URL and optional keyring locator.
    pub url: UrlReference,
    /// Observation time as Unix milliseconds.
    pub seen_at_ms: u64,
}

/// Complete queryable metadata for one download.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadMetadata {
    /// Stable UUIDv7 download id.
    pub id: DownloadId,
    /// Current lifecycle state.
    pub state: DownloadState,
    /// Creation time as Unix milliseconds.
    pub created_at_ms: u64,
    /// Last update time as Unix milliseconds.
    pub updated_at_ms: u64,
    /// Final target path, represented losslessly.
    pub target_path: PathBuf,
    /// Crash-safe part-file path, represented losslessly.
    pub part_path: PathBuf,
    /// Immutable representation length, when known.
    pub total_length: Option<u64>,
    /// Durable bytes summarized from the journal.
    pub covered_bytes: u64,
    /// Stable queue order, when queued.
    pub queue_position: Option<u64>,
    /// Scheduling priority.
    pub priority: i32,
    /// Stable failure category, without arbitrary server-originated detail.
    pub error_kind: Option<DownloadErrorKind>,
    /// Whether allocation still covers the requested file length.
    pub space_reserved: bool,
    /// Remote representation identity.
    pub identity: IdentityMetadata,
    /// Historical working URLs in observation order.
    pub url_history: Vec<UrlHistoryEntry>,
}

/// One non-empty half-open interval known complete in a checkpoint cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompleteInterval {
    start: u64,
    end: u64,
}

impl CompleteInterval {
    /// Creates a non-empty half-open interval.
    pub fn try_new(start: u64, end: u64) -> Result<Self, MetadataError> {
        if start >= end {
            return Err(MetadataError::InvalidCheckpoint {
                reason: "complete interval is empty or reversed".to_owned(),
            });
        }
        Ok(Self { start, end })
    }

    /// Returns the inclusive start offset.
    #[must_use]
    pub const fn start(self) -> u64 {
        self.start
    }

    /// Returns the exclusive end offset.
    #[must_use]
    pub const fn end(self) -> u64 {
        self.end
    }
}

/// A bounded, validated set of complete intervals used only as a SQLite replay cache.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    total_length: u64,
    covered_bytes: u64,
    intervals: Vec<CompleteInterval>,
}

impl Checkpoint {
    /// Validates ordered, disjoint complete intervals and computes their checked union size.
    pub fn try_new(
        total_length: u64,
        intervals: Vec<CompleteInterval>,
    ) -> Result<Self, MetadataError> {
        validate_intervals(total_length, &intervals, None)
    }

    /// Returns the immutable representation length.
    #[must_use]
    pub const fn total_length(&self) -> u64 {
        self.total_length
    }

    /// Returns the checked sum of complete interval lengths.
    #[must_use]
    pub const fn covered_bytes(&self) -> u64 {
        self.covered_bytes
    }

    /// Returns complete intervals in increasing offset order.
    #[must_use]
    pub fn intervals(&self) -> &[CompleteInterval] {
        &self.intervals
    }

    /// Encodes canonical checkpoint-format version 1 bytes.
    pub fn encode_cbor(&self) -> Result<Vec<u8>, MetadataError> {
        let wire = CheckpointWireV1(
            1,
            self.total_length,
            self.covered_bytes,
            BoundedVec(
                self.intervals
                    .iter()
                    .map(|interval| CompleteIntervalWire(interval.start, interval.end))
                    .collect(),
            ),
        );
        let encoded = encode_wire(&wire, MetadataFormat::Checkpoint)?;
        check_payload_len(
            MetadataFormat::Checkpoint,
            encoded.len(),
            MAX_CHECKPOINT_BYTES,
        )?;
        Ok(encoded)
    }

    /// Decodes canonical version-1 bytes and checks the independently stored SQL byte count.
    pub fn decode_cbor(encoded: &[u8], sql_covered_bytes: u64) -> Result<Self, MetadataError> {
        peek_version(encoded, MetadataFormat::Checkpoint, MAX_CHECKPOINT_BYTES)?;
        preflight_checkpoint_count(encoded)?;
        let wire: CheckpointWireV1 = decode_wire(encoded, MetadataFormat::Checkpoint)?;
        require_canonical(encoded, &wire, MetadataFormat::Checkpoint)?;
        let intervals = wire
            .3
            .0
            .into_iter()
            .map(|interval| CompleteInterval::try_new(interval.0, interval.1))
            .collect::<Result<Vec<_>, _>>()?;
        validate_intervals(wire.1, &intervals, Some((wire.2, sql_covered_bytes)))
    }
}

/// One checkpoint row together with its journal position and write time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistedCheckpoint {
    journal_sequence: u64,
    written_at_ms: u64,
    checkpoint: Checkpoint,
}

impl PersistedCheckpoint {
    /// Returns the last journal sequence summarized by this cache row.
    #[must_use]
    pub const fn journal_sequence(&self) -> u64 {
        self.journal_sequence
    }

    /// Returns the cache write time as Unix milliseconds.
    #[must_use]
    pub const fn written_at_ms(&self) -> u64 {
        self.written_at_ms
    }

    /// Returns the validated complete-interval checkpoint.
    #[must_use]
    pub const fn checkpoint(&self) -> &Checkpoint {
        &self.checkpoint
    }
}

/// Values read back from the active SQLite connection after policy application.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionSettings {
    journal_mode: String,
    synchronous: i64,
    foreign_keys: bool,
    busy_timeout_ms: i64,
    trusted_schema: bool,
}

impl ConnectionSettings {
    /// Returns SQLite's normalized journal-mode name.
    #[must_use]
    pub fn journal_mode(&self) -> &str {
        &self.journal_mode
    }

    /// Returns SQLite's numeric synchronous level (`1` is `NORMAL`).
    #[must_use]
    pub const fn synchronous(&self) -> i64 {
        self.synchronous
    }

    /// Returns whether foreign-key enforcement is active.
    #[must_use]
    pub const fn foreign_keys(&self) -> bool {
        self.foreign_keys
    }

    /// Returns the active SQLite busy timeout in milliseconds.
    #[must_use]
    pub const fn busy_timeout_ms(&self) -> i64 {
        self.busy_timeout_ms
    }

    /// Returns whether application-defined functions may be used by schema objects.
    #[must_use]
    pub const fn trusted_schema(&self) -> bool {
        self.trusted_schema
    }
}

/// Encodes raw range evidence without turning it into a capability proof.
pub fn encode_range_observation(observation: &RangeObservation) -> Result<Vec<u8>, MetadataError> {
    let wire = RangeObservationWireV1::from_observation(observation)?;
    let encoded = encode_wire(&wire, MetadataFormat::RangeObservation)?;
    check_payload_len(
        MetadataFormat::RangeObservation,
        encoded.len(),
        MAX_RANGE_OBSERVATION_BYTES,
    )?;
    Ok(encoded)
}

/// Encodes a complete, idempotent identity snapshot for one journal `IdentityUpdate`.
pub fn encode_identity_snapshot(metadata: &DownloadMetadata) -> Result<Vec<u8>, MetadataError> {
    validate_metadata(metadata)?;
    let wire = IdentitySnapshotWireV1::from_metadata(metadata)?;
    let encoded = encode_wire(&wire, MetadataFormat::IdentitySnapshot)?;
    check_payload_len(
        MetadataFormat::IdentitySnapshot,
        encoded.len(),
        MAX_PAYLOAD_LEN,
    )?;
    Ok(encoded)
}

/// Decodes a complete canonical identity snapshot and revalidates any range proof.
pub fn decode_identity_snapshot(encoded: &[u8]) -> Result<DownloadMetadata, MetadataError> {
    peek_version(encoded, MetadataFormat::IdentitySnapshot, MAX_PAYLOAD_LEN)?;
    let wire: IdentitySnapshotWireV1 = decode_wire(encoded, MetadataFormat::IdentitySnapshot)?;
    require_canonical(encoded, &wire, MetadataFormat::IdentitySnapshot)?;
    let metadata = wire.into_metadata()?;
    validate_metadata(&metadata)?;
    Ok(metadata)
}

/// A connection-scoped handle to Downpour's strict SQLite metadata schema.
pub struct MetadataStore {
    connection: Connection,
}

impl MetadataStore {
    /// Opens a file-backed database, initializing an empty file and refusing unknown schemas.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MetadataError> {
        let mut connection = Connection::open(path)?;
        let version = schema_version(&connection)?;
        match version {
            0 => {
                if schema_object_count(&connection)? != 0 || physical_page_count(&connection)? != 0
                {
                    return Err(MetadataError::UnversionedNonemptyDatabase);
                }
                let transaction = connection.transaction()?;
                transaction.execute_batch(SCHEMA_SQL)?;
                transaction.commit()?;
            }
            SCHEMA_VERSION => validate_schema(&connection)?,
            found if found > SCHEMA_VERSION => {
                return Err(MetadataError::NewerSchemaVersion {
                    found,
                    supported: SCHEMA_VERSION,
                });
            }
            found => {
                return Err(MetadataError::InvalidSchema {
                    reason: format!("unsupported older schema version {found}"),
                });
            }
        }
        validate_schema(&connection)?;
        apply_connection_policy(&connection)?;
        let store = Self { connection };
        store.verify_connection_policy()?;
        Ok(store)
    }

    /// Reads the active pragma values for diagnostics and policy proofs.
    pub fn connection_settings(&self) -> Result<ConnectionSettings, MetadataError> {
        let journal_mode = self
            .connection
            .pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))?;
        let synchronous = self
            .connection
            .pragma_query_value(None, "synchronous", |row| row.get::<_, i64>(0))?;
        let foreign_keys = self
            .connection
            .pragma_query_value(None, "foreign_keys", |row| row.get::<_, i64>(0))?;
        let busy_timeout_ms = self
            .connection
            .pragma_query_value(None, "busy_timeout", |row| row.get::<_, i64>(0))?;
        let trusted_schema = self
            .connection
            .pragma_query_value(None, "trusted_schema", |row| row.get::<_, i64>(0))?;
        Ok(ConnectionSettings {
            journal_mode,
            synchronous,
            foreign_keys: foreign_keys == 1,
            busy_timeout_ms,
            trusted_schema: trusted_schema == 1,
        })
    }

    /// Inserts or replaces a complete metadata snapshot in one transaction.
    pub fn save_download(&mut self, metadata: &DownloadMetadata) -> Result<(), MetadataError> {
        let prepared = PreparedDownload::new(metadata)?;
        let transaction = self.connection.transaction()?;
        prepared.write(&transaction)?;
        transaction.commit()?;
        Ok(())
    }

    /// Loads one complete metadata snapshot, validating every persistent boundary.
    pub fn load_download(&self, id: DownloadId) -> Result<Option<DownloadMetadata>, MetadataError> {
        let id_bytes = id.as_bytes();
        let row = self
            .connection
            .query_row(
                "SELECT id, state, created_at, updated_at, target_path, part_path,
                        total_length, covered_bytes, queue_position, priority, error_kind,
                        space_reserved
                 FROM downloads WHERE id = ?1",
                [id_bytes.as_slice()],
                DownloadRow::from_row,
            )
            .optional()?;
        row.map(|row| self.finish_download(row)).transpose()
    }

    /// Loads every download in UUID byte order and refuses a malformed row rather than skipping it.
    pub fn load_downloads(&self) -> Result<Vec<DownloadMetadata>, MetadataError> {
        let mut statement = self
            .connection
            .prepare("SELECT id FROM downloads ORDER BY id")?;
        let raw_ids = statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        raw_ids
            .into_iter()
            .map(|bytes| {
                let id = download_id_from_slice(&bytes)?;
                self.load_download(id)?.ok_or(MetadataError::InvalidValue {
                    field: "downloads.id",
                })
            })
            .collect()
    }

    /// Writes a disposable checkpoint cache row.
    ///
    /// Returns `false` if a future valid checkpoint representation exceeds the cache byte cap;
    /// callers then rely on authoritative journal replay.
    pub fn save_checkpoint(
        &mut self,
        id: DownloadId,
        journal_sequence: u64,
        written_at_ms: u64,
        checkpoint: &Checkpoint,
    ) -> Result<bool, MetadataError> {
        let owning_total = self.download_total_length(id)?;
        if checkpoint.total_length != owning_total {
            return Err(MetadataError::InvalidCheckpoint {
                reason: "checkpoint total differs from owning download".to_owned(),
            });
        }
        let encoded = match checkpoint.encode_cbor() {
            Ok(encoded) => encoded,
            Err(MetadataError::PayloadTooLarge {
                format: MetadataFormat::Checkpoint,
                ..
            }) => return Ok(false),
            Err(error) => return Err(error),
        };
        let journal_sequence = sqlite_u64("journal_seq", journal_sequence)?;
        let covered_bytes = sqlite_u64("covered_bytes", checkpoint.covered_bytes)?;
        let written_at_ms = sqlite_u64("written_at", written_at_ms)?;
        let id_bytes = id.as_bytes();
        self.connection.execute(
            "INSERT INTO checkpoints
                 (download_id, journal_seq, covered_bytes, interval_map, written_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(download_id) DO UPDATE SET
                 journal_seq = excluded.journal_seq,
                 covered_bytes = excluded.covered_bytes,
                 interval_map = excluded.interval_map,
                 written_at = excluded.written_at",
            params![
                id_bytes.as_slice(),
                journal_sequence,
                covered_bytes,
                encoded,
                written_at_ms
            ],
        )?;
        Ok(true)
    }

    /// Loads and validates one disposable checkpoint cache row.
    pub fn load_checkpoint(
        &self,
        id: DownloadId,
    ) -> Result<Option<PersistedCheckpoint>, MetadataError> {
        let id_bytes = id.as_bytes();
        let row = self
            .connection
            .query_row(
                "SELECT c.journal_seq, c.covered_bytes, c.interval_map, c.written_at,
                        d.total_length
                 FROM checkpoints AS c
                 JOIN downloads AS d ON d.id = c.download_id
                 WHERE c.download_id = ?1",
                [id_bytes.as_slice()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(sequence, covered, encoded, written, owning_total)| {
            let journal_sequence = rust_u64("journal_seq", sequence)?;
            let covered_bytes = rust_u64("covered_bytes", covered)?;
            let written_at_ms = rust_u64("written_at", written)?;
            let checkpoint = Checkpoint::decode_cbor(&encoded, covered_bytes)?;
            let owning_total = optional_rust_u64("downloads.total_length", owning_total)?.ok_or(
                MetadataError::InvalidCheckpoint {
                    reason: "owning download has no immutable total".to_owned(),
                },
            )?;
            if checkpoint.total_length != owning_total {
                return Err(MetadataError::InvalidCheckpoint {
                    reason: "checkpoint total differs from owning download".to_owned(),
                });
            }
            Ok(PersistedCheckpoint {
                journal_sequence,
                written_at_ms,
                checkpoint,
            })
        })
        .transpose()
    }

    fn download_total_length(&self, id: DownloadId) -> Result<u64, MetadataError> {
        let id_bytes = id.as_bytes();
        let total = self
            .connection
            .query_row(
                "SELECT total_length FROM downloads WHERE id = ?1",
                [id_bytes.as_slice()],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()?;
        let total = total.ok_or(MetadataError::InvalidCheckpoint {
            reason: "owning download does not exist".to_owned(),
        })?;
        optional_rust_u64("downloads.total_length", total)?.ok_or(
            MetadataError::InvalidCheckpoint {
                reason: "owning download has no immutable total".to_owned(),
            },
        )
    }

    fn verify_connection_policy(&self) -> Result<(), MetadataError> {
        let settings = self.connection_settings()?;
        if !settings.journal_mode.eq_ignore_ascii_case("wal") {
            return Err(MetadataError::InvalidConnectionPolicy {
                reason: "journal_mode is not WAL".to_owned(),
            });
        }
        if settings.synchronous != 1 {
            return Err(MetadataError::InvalidConnectionPolicy {
                reason: "synchronous is not NORMAL".to_owned(),
            });
        }
        if !settings.foreign_keys {
            return Err(MetadataError::InvalidConnectionPolicy {
                reason: "foreign_keys is disabled".to_owned(),
            });
        }
        if settings.busy_timeout_ms
            != i64::try_from(BUSY_TIMEOUT_MS).map_err(|_| {
                MetadataError::InvalidConnectionPolicy {
                    reason: "busy-timeout constant does not fit SQLite".to_owned(),
                }
            })?
        {
            return Err(MetadataError::InvalidConnectionPolicy {
                reason: "busy_timeout is not 5000 ms".to_owned(),
            });
        }
        if settings.trusted_schema {
            return Err(MetadataError::InvalidConnectionPolicy {
                reason: "trusted_schema is enabled".to_owned(),
            });
        }
        Ok(())
    }

    fn finish_download(&self, row: DownloadRow) -> Result<DownloadMetadata, MetadataError> {
        let identity = self.load_identity(row.id)?;
        let url_history = self.load_history(row.id)?;
        let metadata = DownloadMetadata {
            id: row.id,
            state: row.state,
            created_at_ms: row.created_at_ms,
            updated_at_ms: row.updated_at_ms,
            target_path: row.target_path,
            part_path: row.part_path,
            total_length: row.total_length,
            covered_bytes: row.covered_bytes,
            queue_position: row.queue_position,
            priority: row.priority,
            error_kind: row.error_kind,
            space_reserved: row.space_reserved,
            identity,
            url_history,
        };
        validate_metadata(&metadata)?;
        Ok(metadata)
    }

    fn load_identity(&self, id: DownloadId) -> Result<IdentityMetadata, MetadataError> {
        let id_bytes = id.as_bytes();
        let row = self.connection.query_row(
            "SELECT current_url, current_url_ref, final_url, final_url_ref, page_url,
                    page_url_ref, origin, validator_kind, validator_value, server_digest,
                    content_type, suggested_name, request_context_ref, probed_at, protocol,
                    range_state, range_observation
             FROM identities WHERE download_id = ?1",
            [id_bytes.as_slice()],
            IdentityRow::from_row,
        )?;
        let redirect_chain = self.load_redirects(id)?;
        row.into_identity(redirect_chain)
    }

    fn load_redirects(&self, id: DownloadId) -> Result<Vec<UrlReference>, MetadataError> {
        let id_bytes = id.as_bytes();
        let mut statement = self.connection.prepare(
            "SELECT public_url, secret_ref FROM redirect_chain
             WHERE download_id = ?1 ORDER BY hop",
        )?;
        let rows = statement
            .query_map([id_bytes.as_slice()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        if rows.len() > MAX_REDIRECTS {
            return Err(MetadataError::CollectionLimitExceeded {
                format: MetadataFormat::IdentitySnapshot,
                actual: u64::try_from(rows.len()).unwrap_or(u64::MAX),
                maximum: MAX_REDIRECTS,
            });
        }
        rows.into_iter()
            .map(|(public, reference)| url_reference_from_parts(public, reference))
            .collect()
    }

    fn load_history(&self, id: DownloadId) -> Result<Vec<UrlHistoryEntry>, MetadataError> {
        let id_bytes = id.as_bytes();
        let mut statement = self.connection.prepare(
            "SELECT public_url, secret_ref, seen_at FROM url_history
             WHERE download_id = ?1 ORDER BY entry",
        )?;
        let rows = statement
            .query_map([id_bytes.as_slice()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        if rows.len() > MAX_HISTORY_ENTRIES {
            return Err(MetadataError::CollectionLimitExceeded {
                format: MetadataFormat::IdentitySnapshot,
                actual: u64::try_from(rows.len()).unwrap_or(u64::MAX),
                maximum: MAX_HISTORY_ENTRIES,
            });
        }
        rows.into_iter()
            .map(|(public, reference, seen_at)| {
                Ok(UrlHistoryEntry {
                    url: url_reference_from_parts(public, reference)?,
                    seen_at_ms: rust_u64("url_history.seen_at", seen_at)?,
                })
            })
            .collect()
    }
}

struct PreparedDownload {
    metadata: DownloadMetadata,
    target_path: Vec<u8>,
    part_path: Vec<u8>,
    range_state: &'static str,
    range_observation: Option<Vec<u8>>,
    validator_kind: &'static str,
    validator_value: Option<String>,
    digest: Option<String>,
    state: &'static str,
    protocol: &'static str,
}

impl PreparedDownload {
    fn new(metadata: &DownloadMetadata) -> Result<Self, MetadataError> {
        let _identity_snapshot = encode_identity_snapshot(metadata)?;
        let target_path = encode_path(&metadata.target_path)?;
        let part_path = encode_path(&metadata.part_path)?;
        let (range_state, range_observation) = match &metadata.identity.range_support {
            RangeSupport::Proven(proof) => (
                "proven",
                Some(encode_range_observation(proof.observation())?),
            ),
            RangeSupport::Absent => ("absent", None),
            RangeSupport::Unknown => ("unknown", None),
        };
        let (validator_kind, validator_value) = validator_parts(&metadata.identity.validator);
        let digest = metadata
            .identity
            .server_digest
            .as_ref()
            .map(|digest| format!("{}:{}", digest.algorithm.token(), digest.encoded));
        Ok(Self {
            metadata: metadata.clone(),
            target_path,
            part_path,
            range_state,
            range_observation,
            validator_kind,
            validator_value,
            digest,
            state: state_name(metadata.state),
            protocol: protocol_name(metadata.identity.protocol),
        })
    }

    fn write(&self, transaction: &Transaction<'_>) -> Result<(), MetadataError> {
        let id = self.metadata.id.as_bytes();
        transaction.execute(
            "INSERT INTO downloads
                 (id, state, created_at, updated_at, target_path, part_path, total_length,
                  covered_bytes, queue_position, priority, error_kind, space_reserved)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(id) DO UPDATE SET
                 state = excluded.state,
                 created_at = excluded.created_at,
                 updated_at = excluded.updated_at,
                 target_path = excluded.target_path,
                 part_path = excluded.part_path,
                 total_length = excluded.total_length,
                 covered_bytes = excluded.covered_bytes,
                 queue_position = excluded.queue_position,
                 priority = excluded.priority,
                 error_kind = excluded.error_kind,
                 space_reserved = excluded.space_reserved",
            params![
                id.as_slice(),
                self.state,
                sqlite_u64("created_at", self.metadata.created_at_ms)?,
                sqlite_u64("updated_at", self.metadata.updated_at_ms)?,
                self.target_path,
                self.part_path,
                optional_sqlite_u64("total_length", self.metadata.total_length)?,
                sqlite_u64("covered_bytes", self.metadata.covered_bytes)?,
                optional_sqlite_u64("queue_position", self.metadata.queue_position)?,
                self.metadata.priority,
                self.metadata
                    .error_kind
                    .as_ref()
                    .map(DownloadErrorKind::as_str),
                self.metadata.space_reserved,
            ],
        )?;

        let final_parts = optional_url_parts(self.metadata.identity.final_url.as_ref());
        let page_parts = optional_url_parts(self.metadata.identity.page_url.as_ref());
        transaction.execute(
            "INSERT INTO identities
                 (download_id, current_url, current_url_ref, final_url, final_url_ref,
                  page_url, page_url_ref, origin, validator_kind, validator_value,
                  server_digest, content_type, suggested_name, request_context_ref, probed_at,
                  protocol, range_state, range_observation)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                     ?15, ?16, ?17, ?18)
             ON CONFLICT(download_id) DO UPDATE SET
                 current_url = excluded.current_url,
                 current_url_ref = excluded.current_url_ref,
                 final_url = excluded.final_url,
                 final_url_ref = excluded.final_url_ref,
                 page_url = excluded.page_url,
                 page_url_ref = excluded.page_url_ref,
                 origin = excluded.origin,
                 validator_kind = excluded.validator_kind,
                 validator_value = excluded.validator_value,
                 server_digest = excluded.server_digest,
                 content_type = excluded.content_type,
                 suggested_name = excluded.suggested_name,
                 request_context_ref = excluded.request_context_ref,
                 probed_at = excluded.probed_at,
                 protocol = excluded.protocol,
                 range_state = excluded.range_state,
                 range_observation = excluded.range_observation",
            params![
                id.as_slice(),
                self.metadata.identity.current_url.public_url.as_str(),
                optional_secret_text(self.metadata.identity.current_url.secret_ref.as_ref()),
                final_parts.0,
                final_parts.1,
                page_parts.0,
                page_parts.1,
                self.metadata.identity.origin.as_str(),
                self.validator_kind,
                self.validator_value,
                self.digest,
                self.metadata.identity.content_type,
                self.metadata.identity.suggested_filename,
                optional_secret_text(self.metadata.identity.request_context_ref.as_ref()),
                sqlite_u64("probed_at", self.metadata.identity.probed_at_ms)?,
                self.protocol,
                self.range_state,
                self.range_observation,
            ],
        )?;

        transaction.execute(
            "DELETE FROM redirect_chain WHERE download_id = ?1",
            [id.as_slice()],
        )?;
        for (hop, url) in self.metadata.identity.redirect_chain.iter().enumerate() {
            let hop = i64::try_from(hop).map_err(|_| MetadataError::IntegerOutOfRange {
                field: "redirect_chain.hop",
                value: u64::try_from(hop).unwrap_or(u64::MAX),
            })?;
            transaction.execute(
                "INSERT INTO redirect_chain (download_id, hop, public_url, secret_ref)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    id.as_slice(),
                    hop,
                    url.public_url.as_str(),
                    optional_secret_text(url.secret_ref.as_ref())
                ],
            )?;
        }

        transaction.execute(
            "DELETE FROM url_history WHERE download_id = ?1",
            [id.as_slice()],
        )?;
        for (entry_index, entry) in self.metadata.url_history.iter().enumerate() {
            let entry_index =
                i64::try_from(entry_index).map_err(|_| MetadataError::IntegerOutOfRange {
                    field: "url_history.entry",
                    value: u64::try_from(entry_index).unwrap_or(u64::MAX),
                })?;
            transaction.execute(
                "INSERT INTO url_history (download_id, entry, public_url, secret_ref, seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    id.as_slice(),
                    entry_index,
                    entry.url.public_url.as_str(),
                    optional_secret_text(entry.url.secret_ref.as_ref()),
                    sqlite_u64("url_history.seen_at", entry.seen_at_ms)?
                ],
            )?;
        }
        Ok(())
    }
}

struct DownloadRow {
    id: DownloadId,
    state: DownloadState,
    created_at_ms: u64,
    updated_at_ms: u64,
    target_path: PathBuf,
    part_path: PathBuf,
    total_length: Option<u64>,
    covered_bytes: u64,
    queue_position: Option<u64>,
    priority: i32,
    error_kind: Option<DownloadErrorKind>,
    space_reserved: bool,
}

impl DownloadRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let id = row.get::<_, Vec<u8>>(0)?;
        let state = row.get::<_, String>(1)?;
        let created_at = row.get::<_, i64>(2)?;
        let updated_at = row.get::<_, i64>(3)?;
        let target_path = row.get::<_, Vec<u8>>(4)?;
        let part_path = row.get::<_, Vec<u8>>(5)?;
        let total_length = row.get::<_, Option<i64>>(6)?;
        let covered_bytes = row.get::<_, i64>(7)?;
        let queue_position = row.get::<_, Option<i64>>(8)?;
        let priority = row.get::<_, i32>(9)?;
        let error_kind = row.get::<_, Option<String>>(10)?;
        let space_reserved = row.get::<_, i64>(11)?;
        Self::from_raw(
            id,
            state,
            created_at,
            updated_at,
            target_path,
            part_path,
            total_length,
            covered_bytes,
            queue_position,
            priority,
            error_kind,
            space_reserved,
        )
        .map_err(to_sql_conversion_error)
    }

    #[allow(clippy::too_many_arguments)]
    fn from_raw(
        id: Vec<u8>,
        state: String,
        created_at: i64,
        updated_at: i64,
        target_path: Vec<u8>,
        part_path: Vec<u8>,
        total_length: Option<i64>,
        covered_bytes: i64,
        queue_position: Option<i64>,
        priority: i32,
        error_kind: Option<String>,
        space_reserved: i64,
    ) -> Result<Self, MetadataError> {
        Ok(Self {
            id: download_id_from_slice(&id)?,
            state: parse_state(&state)?,
            created_at_ms: rust_u64("created_at", created_at)?,
            updated_at_ms: rust_u64("updated_at", updated_at)?,
            target_path: decode_path(&target_path)?,
            part_path: decode_path(&part_path)?,
            total_length: optional_rust_u64("total_length", total_length)?,
            covered_bytes: rust_u64("covered_bytes", covered_bytes)?,
            queue_position: optional_rust_u64("queue_position", queue_position)?,
            priority,
            error_kind: error_kind
                .map(|raw| DownloadErrorKind::new(&raw))
                .transpose()?,
            space_reserved: parse_sql_bool("space_reserved", space_reserved)?,
        })
    }
}

struct IdentityRow {
    current_url: String,
    current_url_ref: Option<String>,
    final_url: Option<String>,
    final_url_ref: Option<String>,
    page_url: Option<String>,
    page_url_ref: Option<String>,
    origin: String,
    validator_kind: String,
    validator_value: Option<String>,
    server_digest: Option<String>,
    content_type: Option<String>,
    suggested_name: Option<String>,
    request_context_ref: Option<String>,
    probed_at: i64,
    protocol: String,
    range_state: String,
    range_observation: Option<Vec<u8>>,
}

impl IdentityRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            current_url: row.get(0)?,
            current_url_ref: row.get(1)?,
            final_url: row.get(2)?,
            final_url_ref: row.get(3)?,
            page_url: row.get(4)?,
            page_url_ref: row.get(5)?,
            origin: row.get(6)?,
            validator_kind: row.get(7)?,
            validator_value: row.get(8)?,
            server_digest: row.get(9)?,
            content_type: row.get(10)?,
            suggested_name: row.get(11)?,
            request_context_ref: row.get(12)?,
            probed_at: row.get(13)?,
            protocol: row.get(14)?,
            range_state: row.get(15)?,
            range_observation: row.get(16)?,
        })
    }

    fn into_identity(
        self,
        redirect_chain: Vec<UrlReference>,
    ) -> Result<IdentityMetadata, MetadataError> {
        let range_support = range_support_from_parts(&self.range_state, self.range_observation)?;
        Ok(IdentityMetadata {
            current_url: url_reference_from_parts(self.current_url, self.current_url_ref)?,
            final_url: optional_url_reference_from_parts(self.final_url, self.final_url_ref)?,
            redirect_chain,
            page_url: optional_url_reference_from_parts(self.page_url, self.page_url_ref)?,
            origin: PublicUrl::parse(&self.origin)?,
            validator: validator_from_parts(&self.validator_kind, self.validator_value)?,
            server_digest: digest_from_text(self.server_digest)?,
            content_type: self.content_type,
            suggested_filename: self.suggested_name,
            request_context_ref: self
                .request_context_ref
                .map(|raw| SecretRef::new(&raw))
                .transpose()?,
            probed_at_ms: rust_u64("probed_at", self.probed_at)?,
            protocol: parse_protocol(&self.protocol)?,
            range_support,
        })
    }
}

fn schema_version(connection: &Connection) -> Result<u32, MetadataError> {
    connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(Into::into)
}

fn schema_object_count(connection: &Connection) -> Result<u64, MetadataError> {
    let count = connection.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    rust_u64("sqlite_schema.count", count)
}

fn physical_page_count(connection: &Connection) -> Result<u64, MetadataError> {
    let count = connection.pragma_query_value(None, "page_count", |row| row.get::<_, i64>(0))?;
    rust_u64("page_count", count)
}

fn validate_schema(connection: &Connection) -> Result<(), MetadataError> {
    let reference = Connection::open_in_memory()?;
    reference.execute_batch(SCHEMA_SQL)?;
    if schema_description(connection)? != schema_description(&reference)? {
        return Err(MetadataError::InvalidSchema {
            reason: "tables, constraints, or indexes differ from schema v1".to_owned(),
        });
    }
    Ok(())
}

fn schema_description(connection: &Connection) -> Result<Vec<SchemaObject>, MetadataError> {
    connection
        .prepare(
            "SELECT type, name, tbl_name, sql FROM sqlite_schema
             WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
        )?
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn apply_connection_policy(connection: &Connection) -> Result<(), MetadataError> {
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;
    connection.pragma_update(None, "trusted_schema", "OFF")?;
    Ok(())
}

fn state_name(state: DownloadState) -> &'static str {
    match state {
        DownloadState::Submitted => "submitted",
        DownloadState::Probing => "probing",
        DownloadState::Planned => "planned",
        DownloadState::Transferring => "transferring",
        DownloadState::Paused => "paused",
        DownloadState::Stalled => "stalled",
        DownloadState::AwaitingRefresh => "awaiting-refresh",
        DownloadState::Verifying => "verifying",
        DownloadState::Completed => "completed",
        DownloadState::Failed => "failed",
    }
}

fn state_code(state: DownloadState) -> u8 {
    match state {
        DownloadState::Submitted => 0,
        DownloadState::Probing => 1,
        DownloadState::Planned => 2,
        DownloadState::Transferring => 3,
        DownloadState::Paused => 4,
        DownloadState::Stalled => 5,
        DownloadState::AwaitingRefresh => 6,
        DownloadState::Verifying => 7,
        DownloadState::Completed => 8,
        DownloadState::Failed => 9,
    }
}

fn parse_state(raw: &str) -> Result<DownloadState, MetadataError> {
    match raw {
        "submitted" => Ok(DownloadState::Submitted),
        "probing" => Ok(DownloadState::Probing),
        "planned" => Ok(DownloadState::Planned),
        "transferring" => Ok(DownloadState::Transferring),
        "paused" => Ok(DownloadState::Paused),
        "stalled" => Ok(DownloadState::Stalled),
        "awaiting-refresh" => Ok(DownloadState::AwaitingRefresh),
        "verifying" => Ok(DownloadState::Verifying),
        "completed" => Ok(DownloadState::Completed),
        "failed" => Ok(DownloadState::Failed),
        _ => Err(MetadataError::InvalidValue {
            field: "downloads.state",
        }),
    }
}

fn state_from_code(code: u8) -> Result<DownloadState, MetadataError> {
    match code {
        0 => Ok(DownloadState::Submitted),
        1 => Ok(DownloadState::Probing),
        2 => Ok(DownloadState::Planned),
        3 => Ok(DownloadState::Transferring),
        4 => Ok(DownloadState::Paused),
        5 => Ok(DownloadState::Stalled),
        6 => Ok(DownloadState::AwaitingRefresh),
        7 => Ok(DownloadState::Verifying),
        8 => Ok(DownloadState::Completed),
        9 => Ok(DownloadState::Failed),
        _ => Err(MetadataError::InvalidValue {
            field: "identity.state",
        }),
    }
}

fn protocol_name(protocol: NegotiatedProtocol) -> &'static str {
    match protocol {
        NegotiatedProtocol::Http11 => "http/1.1",
        NegotiatedProtocol::Http2 => "h2",
        NegotiatedProtocol::Http3 => "h3",
    }
}

fn protocol_code(protocol: NegotiatedProtocol) -> u8 {
    match protocol {
        NegotiatedProtocol::Http11 => 0,
        NegotiatedProtocol::Http2 => 1,
        NegotiatedProtocol::Http3 => 2,
    }
}

fn parse_protocol(raw: &str) -> Result<NegotiatedProtocol, MetadataError> {
    match raw {
        "http/1.1" => Ok(NegotiatedProtocol::Http11),
        "h2" => Ok(NegotiatedProtocol::Http2),
        "h3" => Ok(NegotiatedProtocol::Http3),
        _ => Err(MetadataError::InvalidValue {
            field: "identities.protocol",
        }),
    }
}

fn protocol_from_code(code: u8) -> Result<NegotiatedProtocol, MetadataError> {
    match code {
        0 => Ok(NegotiatedProtocol::Http11),
        1 => Ok(NegotiatedProtocol::Http2),
        2 => Ok(NegotiatedProtocol::Http3),
        _ => Err(MetadataError::InvalidValue {
            field: "identity.protocol",
        }),
    }
}

fn validator_parts(validator: &Validator) -> (&'static str, Option<String>) {
    match validator {
        Validator::StrongETag(value) => ("strong-etag", Some(value.clone())),
        Validator::LastModified(value) => ("last-modified", Some(value.clone())),
        Validator::None => ("none", None),
    }
}

fn validator_from_parts(kind: &str, value: Option<String>) -> Result<Validator, MetadataError> {
    match (kind, value) {
        ("strong-etag", Some(value)) => Ok(Validator::StrongETag(value)),
        ("last-modified", Some(value)) => Ok(Validator::LastModified(value)),
        ("none", None) => Ok(Validator::None),
        _ => Err(MetadataError::InvalidValue {
            field: "identities.validator",
        }),
    }
}

fn digest_from_text(raw: Option<String>) -> Result<Option<ContentDigest>, MetadataError> {
    raw.map(|raw| {
        let (algorithm, encoded) = raw.split_once(':').ok_or(MetadataError::InvalidValue {
            field: "identities.server_digest",
        })?;
        let algorithm = match algorithm {
            "sha-256" => DigestAlgorithm::Sha256,
            "sha-512" => DigestAlgorithm::Sha512,
            _ => {
                return Err(MetadataError::InvalidValue {
                    field: "identities.server_digest",
                });
            }
        };
        if encoded.is_empty() {
            return Err(MetadataError::InvalidValue {
                field: "identities.server_digest",
            });
        }
        Ok(ContentDigest {
            algorithm,
            encoded: encoded.to_owned(),
        })
    })
    .transpose()
}

fn optional_secret_text(reference: Option<&SecretRef>) -> Option<&str> {
    reference.map(SecretRef::as_str)
}

fn optional_url_parts(url: Option<&UrlReference>) -> (Option<&str>, Option<&str>) {
    match url {
        Some(url) => (
            Some(url.public_url.as_str()),
            optional_secret_text(url.secret_ref.as_ref()),
        ),
        None => (None, None),
    }
}

fn url_reference_from_parts(
    public: String,
    reference: Option<String>,
) -> Result<UrlReference, MetadataError> {
    Ok(UrlReference {
        public_url: PublicUrl::parse(&public)?,
        secret_ref: reference.map(|raw| SecretRef::new(&raw)).transpose()?,
    })
}

fn optional_url_reference_from_parts(
    public: Option<String>,
    reference: Option<String>,
) -> Result<Option<UrlReference>, MetadataError> {
    match (public, reference) {
        (Some(public), reference) => url_reference_from_parts(public, reference).map(Some),
        (None, None) => Ok(None),
        (None, Some(_)) => Err(MetadataError::InvalidValue {
            field: "URL reference without public URL",
        }),
    }
}

fn range_support_from_parts(
    state: &str,
    observation: Option<Vec<u8>>,
) -> Result<RangeSupport, MetadataError> {
    match (state, observation) {
        ("proven", Some(encoded)) => {
            let observation = decode_range_observation(&encoded)?;
            let proof = RangeProof::from_observed_response(
                observation.requested_range(),
                observation.status(),
                observation.content_range(),
                observation.content_encoding(),
                observation.body_len(),
            )
            .map_err(MetadataError::InvalidRangeObservation)?;
            Ok(RangeSupport::Proven(proof))
        }
        ("absent", None) => Ok(RangeSupport::Absent),
        ("unknown", None) => Ok(RangeSupport::Unknown),
        _ => Err(MetadataError::InvalidValue {
            field: "identities.range_state",
        }),
    }
}

#[derive(Serialize, Deserialize)]
struct RangeObservationWireV1(
    u8,
    u8,
    u64,
    Option<u64>,
    u16,
    Option<String>,
    Option<String>,
    u64,
);

impl RangeObservationWireV1 {
    fn from_observation(observation: &RangeObservation) -> Result<Self, MetadataError> {
        if let Some(value) = observation.content_range() {
            check_len("range.content_range", value, MAX_HEADER_BYTES)?;
        }
        if let Some(value) = observation.content_encoding() {
            check_len("range.content_encoding", value, MAX_HEADER_BYTES)?;
        }
        let (kind, first, second) = match observation.requested_range() {
            ByteRangeSpec::FromTo { first, last } => (0, first, Some(last)),
            ByteRangeSpec::From { first } => (1, first, None),
            ByteRangeSpec::Suffix { len } => (2, len, None),
        };
        Ok(Self(
            1,
            kind,
            first,
            second,
            observation.status(),
            observation.content_range().map(str::to_owned),
            observation.content_encoding().map(str::to_owned),
            observation.body_len(),
        ))
    }

    fn into_observation(self) -> Result<RangeObservation, MetadataError> {
        let requested = match (self.1, self.2, self.3) {
            (0, first, Some(last)) => ByteRangeSpec::FromTo { first, last },
            (1, first, None) => ByteRangeSpec::From { first },
            (2, len, None) => ByteRangeSpec::Suffix { len },
            _ => {
                return Err(MetadataError::InvalidValue {
                    field: "range.requested",
                });
            }
        };
        if let Some(value) = &self.5 {
            check_len("range.content_range", value, MAX_HEADER_BYTES)?;
        }
        if let Some(value) = &self.6 {
            check_len("range.content_encoding", value, MAX_HEADER_BYTES)?;
        }
        Ok(RangeObservation::new(
            requested, self.4, self.5, self.6, self.7,
        ))
    }
}

#[derive(Serialize, Deserialize)]
struct CompleteIntervalWire(u64, u64);

#[derive(Serialize, Deserialize)]
struct CheckpointWireV1(
    u8,
    u64,
    u64,
    BoundedVec<CompleteIntervalWire, MAX_CHECKPOINT_INTERVALS>,
);

#[derive(Serialize, Deserialize)]
struct IdentitySnapshotWireV1(
    u8,
    ByteString,
    u8,
    u64,
    u64,
    PathWire,
    PathWire,
    Option<u64>,
    u64,
    Option<u64>,
    i32,
    Option<String>,
    bool,
    IdentityWire,
    BoundedVec<HistoryWire, MAX_HISTORY_ENTRIES>,
);

#[derive(Serialize, Deserialize)]
struct IdentityWire(
    UrlReferenceWire,
    Option<UrlReferenceWire>,
    BoundedVec<UrlReferenceWire, MAX_REDIRECTS>,
    Option<UrlReferenceWire>,
    String,
    ValidatorWire,
    Option<DigestWire>,
    Option<String>,
    Option<String>,
    Option<String>,
    u64,
    u8,
    u8,
    Option<ByteString>,
);

#[derive(Serialize, Deserialize)]
struct UrlReferenceWire(String, Option<String>);

#[derive(Serialize, Deserialize)]
struct ValidatorWire(u8, Option<String>);

#[derive(Serialize, Deserialize)]
struct DigestWire(u8, String);

#[derive(Serialize, Deserialize)]
struct HistoryWire(UrlReferenceWire, u64);

#[derive(Serialize, Deserialize)]
struct PathWire(u8, ByteString);

impl IdentitySnapshotWireV1 {
    fn from_metadata(metadata: &DownloadMetadata) -> Result<Self, MetadataError> {
        let (range_state, range_observation) = match &metadata.identity.range_support {
            RangeSupport::Proven(proof) => (
                0,
                Some(ByteString(encode_range_observation(proof.observation())?)),
            ),
            RangeSupport::Absent => (1, None),
            RangeSupport::Unknown => (2, None),
        };
        let identity = IdentityWire(
            UrlReferenceWire::from_reference(&metadata.identity.current_url),
            metadata
                .identity
                .final_url
                .as_ref()
                .map(UrlReferenceWire::from_reference),
            BoundedVec(
                metadata
                    .identity
                    .redirect_chain
                    .iter()
                    .map(UrlReferenceWire::from_reference)
                    .collect(),
            ),
            metadata
                .identity
                .page_url
                .as_ref()
                .map(UrlReferenceWire::from_reference),
            metadata.identity.origin.as_str().to_owned(),
            ValidatorWire::from_validator(&metadata.identity.validator),
            metadata
                .identity
                .server_digest
                .as_ref()
                .map(DigestWire::from_digest),
            metadata.identity.content_type.clone(),
            metadata.identity.suggested_filename.clone(),
            metadata
                .identity
                .request_context_ref
                .as_ref()
                .map(|reference| reference.as_str().to_owned()),
            metadata.identity.probed_at_ms,
            protocol_code(metadata.identity.protocol),
            range_state,
            range_observation,
        );
        Ok(Self(
            1,
            ByteString(metadata.id.as_bytes().to_vec()),
            state_code(metadata.state),
            metadata.created_at_ms,
            metadata.updated_at_ms,
            path_to_wire(&metadata.target_path)?,
            path_to_wire(&metadata.part_path)?,
            metadata.total_length,
            metadata.covered_bytes,
            metadata.queue_position,
            metadata.priority,
            metadata
                .error_kind
                .as_ref()
                .map(|kind| kind.as_str().to_owned()),
            metadata.space_reserved,
            identity,
            BoundedVec(
                metadata
                    .url_history
                    .iter()
                    .map(|entry| {
                        HistoryWire(
                            UrlReferenceWire::from_reference(&entry.url),
                            entry.seen_at_ms,
                        )
                    })
                    .collect(),
            ),
        ))
    }

    fn into_metadata(self) -> Result<DownloadMetadata, MetadataError> {
        let IdentityWire(
            current_url,
            final_url,
            redirect_chain,
            page_url,
            origin,
            validator,
            digest,
            content_type,
            suggested_filename,
            request_context_ref,
            probed_at_ms,
            protocol,
            range_state,
            range_observation,
        ) = self.13;
        let range_support = range_support_from_wire(range_state, range_observation)?;
        Ok(DownloadMetadata {
            id: download_id_from_slice(&self.1.0)?,
            state: state_from_code(self.2)?,
            created_at_ms: self.3,
            updated_at_ms: self.4,
            target_path: path_from_wire(self.5)?,
            part_path: path_from_wire(self.6)?,
            total_length: self.7,
            covered_bytes: self.8,
            queue_position: self.9,
            priority: self.10,
            error_kind: self
                .11
                .map(|raw| DownloadErrorKind::new(&raw))
                .transpose()?,
            space_reserved: self.12,
            identity: IdentityMetadata {
                current_url: current_url.into_reference()?,
                final_url: final_url
                    .map(UrlReferenceWire::into_reference)
                    .transpose()?,
                redirect_chain: redirect_chain
                    .0
                    .into_iter()
                    .map(UrlReferenceWire::into_reference)
                    .collect::<Result<Vec<_>, _>>()?,
                page_url: page_url.map(UrlReferenceWire::into_reference).transpose()?,
                origin: PublicUrl::parse(&origin)?,
                validator: validator.into_validator()?,
                server_digest: digest.map(DigestWire::into_digest).transpose()?,
                content_type,
                suggested_filename,
                request_context_ref: request_context_ref
                    .map(|raw| SecretRef::new(&raw))
                    .transpose()?,
                probed_at_ms,
                protocol: protocol_from_code(protocol)?,
                range_support,
            },
            url_history: self
                .14
                .0
                .into_iter()
                .map(|entry| {
                    Ok(UrlHistoryEntry {
                        url: entry.0.into_reference()?,
                        seen_at_ms: entry.1,
                    })
                })
                .collect::<Result<Vec<_>, MetadataError>>()?,
        })
    }
}

impl UrlReferenceWire {
    fn from_reference(reference: &UrlReference) -> Self {
        Self(
            reference.public_url.as_str().to_owned(),
            reference
                .secret_ref
                .as_ref()
                .map(|secret| secret.as_str().to_owned()),
        )
    }

    fn into_reference(self) -> Result<UrlReference, MetadataError> {
        url_reference_from_parts(self.0, self.1)
    }
}

impl ValidatorWire {
    fn from_validator(validator: &Validator) -> Self {
        match validator {
            Validator::StrongETag(value) => Self(0, Some(value.clone())),
            Validator::LastModified(value) => Self(1, Some(value.clone())),
            Validator::None => Self(2, None),
        }
    }

    fn into_validator(self) -> Result<Validator, MetadataError> {
        match (self.0, self.1) {
            (0, Some(value)) => Ok(Validator::StrongETag(value)),
            (1, Some(value)) => Ok(Validator::LastModified(value)),
            (2, None) => Ok(Validator::None),
            _ => Err(MetadataError::InvalidValue {
                field: "identity.validator",
            }),
        }
    }
}

impl DigestWire {
    fn from_digest(digest: &ContentDigest) -> Self {
        let algorithm = match digest.algorithm {
            DigestAlgorithm::Sha256 => 0,
            DigestAlgorithm::Sha512 => 1,
        };
        Self(algorithm, digest.encoded.clone())
    }

    fn into_digest(self) -> Result<ContentDigest, MetadataError> {
        let algorithm = match self.0 {
            0 => DigestAlgorithm::Sha256,
            1 => DigestAlgorithm::Sha512,
            _ => {
                return Err(MetadataError::InvalidValue {
                    field: "identity.digest.algorithm",
                });
            }
        };
        if self.1.is_empty() {
            return Err(MetadataError::InvalidValue {
                field: "identity.digest.value",
            });
        }
        Ok(ContentDigest {
            algorithm,
            encoded: self.1,
        })
    }
}

fn range_support_from_wire(
    state: u8,
    observation: Option<ByteString>,
) -> Result<RangeSupport, MetadataError> {
    match (state, observation) {
        (0, Some(encoded)) => {
            let observation = decode_range_observation(&encoded.0)?;
            let proof = RangeProof::from_observed_response(
                observation.requested_range(),
                observation.status(),
                observation.content_range(),
                observation.content_encoding(),
                observation.body_len(),
            )
            .map_err(MetadataError::InvalidRangeObservation)?;
            Ok(RangeSupport::Proven(proof))
        }
        (1, None) => Ok(RangeSupport::Absent),
        (2, None) => Ok(RangeSupport::Unknown),
        _ => Err(MetadataError::InvalidValue {
            field: "identity.range_support",
        }),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ByteString(Vec<u8>);

impl Serialize for ByteString {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for ByteString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ByteStringVisitor;

        impl<'de> Visitor<'de> for ByteStringVisitor {
            type Value = ByteString;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a definite byte string")
            }

            fn visit_bytes<E: serde::de::Error>(self, value: &[u8]) -> Result<Self::Value, E> {
                Ok(ByteString(value.to_vec()))
            }

            fn visit_byte_buf<E: serde::de::Error>(self, value: Vec<u8>) -> Result<Self::Value, E> {
                Ok(ByteString(value))
            }
        }

        deserializer.deserialize_byte_buf(ByteStringVisitor)
    }
}

#[derive(Debug, PartialEq, Eq)]
struct BoundedVec<T, const MAX: usize>(Vec<T>);

impl<T: Serialize, const MAX: usize> Serialize for BoundedVec<T, MAX> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for value in &self.0 {
            sequence.serialize_element(value)?;
        }
        sequence.end()
    }
}

impl<'de, T: Deserialize<'de>, const MAX: usize> Deserialize<'de> for BoundedVec<T, MAX> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct BoundedVecVisitor<T, const MAX: usize>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>, const MAX: usize> Visitor<'de> for BoundedVecVisitor<T, MAX> {
            type Value = BoundedVec<T, MAX>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "a definite array with at most {MAX} entries")
            }

            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> Result<Self::Value, A::Error> {
                let Some(size) = sequence.size_hint() else {
                    return Err(serde::de::Error::custom("indefinite arrays are forbidden"));
                };
                if size > MAX {
                    return Err(serde::de::Error::custom("collection limit exceeded"));
                }
                let mut values = Vec::with_capacity(size);
                while let Some(value) = sequence.next_element()? {
                    if values.len() == MAX {
                        return Err(serde::de::Error::custom("collection limit exceeded"));
                    }
                    values.push(value);
                }
                Ok(BoundedVec(values))
            }
        }

        deserializer.deserialize_seq(BoundedVecVisitor::<T, MAX>(PhantomData))
    }
}

fn validate_metadata(metadata: &DownloadMetadata) -> Result<(), MetadataError> {
    sqlite_u64("created_at", metadata.created_at_ms)?;
    sqlite_u64("updated_at", metadata.updated_at_ms)?;
    optional_sqlite_u64("total_length", metadata.total_length)?;
    sqlite_u64("covered_bytes", metadata.covered_bytes)?;
    optional_sqlite_u64("queue_position", metadata.queue_position)?;
    sqlite_u64("probed_at", metadata.identity.probed_at_ms)?;
    if let Some(total_length) = metadata.total_length
        && metadata.covered_bytes > total_length
    {
        return Err(MetadataError::InvalidValue {
            field: "downloads.covered_bytes",
        });
    }
    if let RangeSupport::Proven(proof) = &metadata.identity.range_support
        && metadata.total_length != Some(proof.total_length())
    {
        return Err(MetadataError::InvalidValue {
            field: "identity.range_total_length",
        });
    }
    if metadata.identity.redirect_chain.len() > MAX_REDIRECTS {
        return Err(MetadataError::CollectionLimitExceeded {
            format: MetadataFormat::IdentitySnapshot,
            actual: u64::try_from(metadata.identity.redirect_chain.len()).unwrap_or(u64::MAX),
            maximum: MAX_REDIRECTS,
        });
    }
    if metadata.url_history.len() > MAX_HISTORY_ENTRIES {
        return Err(MetadataError::CollectionLimitExceeded {
            format: MetadataFormat::IdentitySnapshot,
            actual: u64::try_from(metadata.url_history.len()).unwrap_or(u64::MAX),
            maximum: MAX_HISTORY_ENTRIES,
        });
    }
    if let Some(value) = &metadata.error_kind {
        check_len("error_kind", value.as_str(), MAX_ERROR_KIND_BYTES)?;
    }
    validate_identity(&metadata.identity)?;
    for entry in &metadata.url_history {
        sqlite_u64("url_history.seen_at", entry.seen_at_ms)?;
        validate_url_reference(&entry.url)?;
    }
    let _target = path_to_wire(&metadata.target_path)?;
    let _part = path_to_wire(&metadata.part_path)?;
    Ok(())
}

fn validate_identity(identity: &IdentityMetadata) -> Result<(), MetadataError> {
    validate_url_reference(&identity.current_url)?;
    if let Some(url) = &identity.final_url {
        validate_url_reference(url)?;
    }
    for url in &identity.redirect_chain {
        validate_url_reference(url)?;
    }
    if let Some(url) = &identity.page_url {
        validate_url_reference(url)?;
    }
    check_len("origin", identity.origin.as_str(), MAX_URL_BYTES)?;
    match &identity.validator {
        Validator::StrongETag(value) | Validator::LastModified(value) => {
            check_len("validator", value, MAX_HEADER_BYTES)?;
        }
        Validator::None => {}
    }
    if let Some(digest) = &identity.server_digest {
        check_len("server_digest", &digest.encoded, MAX_HEADER_BYTES)?;
        if digest.encoded.is_empty() {
            return Err(MetadataError::InvalidValue {
                field: "server_digest",
            });
        }
    }
    if let Some(value) = &identity.content_type {
        check_len("content_type", value, MAX_CONTENT_TYPE_BYTES)?;
    }
    if let Some(value) = &identity.suggested_filename {
        check_len("suggested_filename", value, MAX_FILENAME_BYTES)?;
    }
    if let Some(reference) = &identity.request_context_ref {
        check_len(
            "request_context_ref",
            reference.as_str(),
            MAX_SECRET_REF_BYTES,
        )?;
    }
    if let RangeSupport::Proven(proof) = &identity.range_support {
        let _encoded = encode_range_observation(proof.observation())?;
    }
    Ok(())
}

fn validate_url_reference(reference: &UrlReference) -> Result<(), MetadataError> {
    check_len("public_url", reference.public_url.as_str(), MAX_URL_BYTES)?;
    if let Some(secret_ref) = &reference.secret_ref {
        check_len("secret_ref", secret_ref.as_str(), MAX_SECRET_REF_BYTES)?;
    }
    Ok(())
}

fn validate_intervals(
    total_length: u64,
    intervals: &[CompleteInterval],
    expected_covered: Option<(u64, u64)>,
) -> Result<Checkpoint, MetadataError> {
    if intervals.len() > MAX_CHECKPOINT_INTERVALS {
        return Err(MetadataError::CollectionLimitExceeded {
            format: MetadataFormat::Checkpoint,
            actual: u64::try_from(intervals.len()).unwrap_or(u64::MAX),
            maximum: MAX_CHECKPOINT_INTERVALS,
        });
    }
    let mut previous_end = None;
    let mut covered_bytes = 0_u64;
    for interval in intervals {
        if interval.start >= interval.end {
            return Err(MetadataError::InvalidCheckpoint {
                reason: "complete interval is empty or reversed".to_owned(),
            });
        }
        if interval.end > total_length {
            return Err(MetadataError::InvalidCheckpoint {
                reason: "complete interval exceeds total length".to_owned(),
            });
        }
        if previous_end.is_some_and(|end| interval.start < end) {
            return Err(MetadataError::InvalidCheckpoint {
                reason: "complete intervals overlap or are out of order".to_owned(),
            });
        }
        covered_bytes = covered_bytes
            .checked_add(interval.end - interval.start)
            .ok_or_else(|| MetadataError::InvalidCheckpoint {
                reason: "covered byte count overflowed".to_owned(),
            })?;
        previous_end = Some(interval.end);
    }
    if let Some((encoded, sql)) = expected_covered
        && (covered_bytes != encoded || covered_bytes != sql)
    {
        return Err(MetadataError::InvalidCheckpoint {
            reason: "interval union differs from encoded or SQL covered_bytes".to_owned(),
        });
    }
    Ok(Checkpoint {
        total_length,
        covered_bytes,
        intervals: intervals.to_vec(),
    })
}

fn decode_range_observation(encoded: &[u8]) -> Result<RangeObservation, MetadataError> {
    peek_version(
        encoded,
        MetadataFormat::RangeObservation,
        MAX_RANGE_OBSERVATION_BYTES,
    )?;
    let wire: RangeObservationWireV1 = decode_wire(encoded, MetadataFormat::RangeObservation)?;
    require_canonical(encoded, &wire, MetadataFormat::RangeObservation)?;
    wire.into_observation()
}

fn encode_wire<T: Serialize>(value: &T, format: MetadataFormat) -> Result<Vec<u8>, MetadataError> {
    let mut encoded = Vec::new();
    into_writer(value, &mut encoded).map_err(|error| MetadataError::InvalidEncoding {
        format,
        reason: error.to_string(),
    })?;
    Ok(encoded)
}

fn decode_wire<T: DeserializeOwned>(
    encoded: &[u8],
    format: MetadataFormat,
) -> Result<T, MetadataError> {
    let mut cursor = Cursor::new(encoded);
    let value = from_reader(&mut cursor).map_err(|error| MetadataError::InvalidEncoding {
        format,
        reason: error.to_string(),
    })?;
    let consumed =
        usize::try_from(cursor.position()).map_err(|_| MetadataError::InvalidEncoding {
            format,
            reason: "decoder position does not fit memory size".to_owned(),
        })?;
    if consumed != encoded.len() {
        return Err(MetadataError::NonCanonicalEncoding { format });
    }
    Ok(value)
}

fn require_canonical<T: Serialize>(
    original: &[u8],
    value: &T,
    format: MetadataFormat,
) -> Result<(), MetadataError> {
    if encode_wire(value, format)? != original {
        return Err(MetadataError::NonCanonicalEncoding { format });
    }
    Ok(())
}

fn peek_version(
    encoded: &[u8],
    format: MetadataFormat,
    maximum: usize,
) -> Result<(), MetadataError> {
    check_payload_len(format, encoded.len(), maximum)?;
    let (major, _length, offset) = parse_cbor_head(encoded, 0, format)?;
    if major != 4 {
        return Err(MetadataError::InvalidEncoding {
            format,
            reason: "top-level item is not an array".to_owned(),
        });
    }
    let (version_major, version, _offset) = parse_cbor_head(encoded, offset, format)?;
    if version_major != 0 {
        return Err(MetadataError::InvalidEncoding {
            format,
            reason: "version is not an unsigned integer".to_owned(),
        });
    }
    if version > FORMAT_VERSION {
        return Err(MetadataError::NewerFormatVersion {
            format,
            found: version,
            supported: FORMAT_VERSION,
        });
    }
    if version != FORMAT_VERSION {
        return Err(MetadataError::UnsupportedFormatVersion {
            format,
            found: version,
        });
    }
    Ok(())
}

fn preflight_checkpoint_count(encoded: &[u8]) -> Result<(), MetadataError> {
    let format = MetadataFormat::Checkpoint;
    let (_, _, after_top) = parse_cbor_head(encoded, 0, format)?;
    let (_, _, after_version) = parse_cbor_head(encoded, after_top, format)?;
    let (_, _, after_total) = parse_cbor_head(encoded, after_version, format)?;
    let (_, _, after_covered) = parse_cbor_head(encoded, after_total, format)?;
    let (major, count, _) = parse_cbor_head(encoded, after_covered, format)?;
    if major != 4 {
        return Err(MetadataError::InvalidEncoding {
            format,
            reason: "checkpoint intervals are not an array".to_owned(),
        });
    }
    let maximum =
        u64::try_from(MAX_CHECKPOINT_INTERVALS).map_err(|_| MetadataError::InvalidEncoding {
            format,
            reason: "checkpoint interval limit does not fit u64".to_owned(),
        })?;
    if count > maximum {
        return Err(MetadataError::CollectionLimitExceeded {
            format,
            actual: count,
            maximum: MAX_CHECKPOINT_INTERVALS,
        });
    }
    Ok(())
}

fn parse_cbor_head(
    encoded: &[u8],
    offset: usize,
    format: MetadataFormat,
) -> Result<(u8, u64, usize), MetadataError> {
    let initial = *encoded
        .get(offset)
        .ok_or_else(|| MetadataError::InvalidEncoding {
            format,
            reason: "truncated CBOR header".to_owned(),
        })?;
    let major = initial >> 5;
    let additional = initial & 0x1f;
    let mut next = offset
        .checked_add(1)
        .ok_or_else(|| MetadataError::InvalidEncoding {
            format,
            reason: "CBOR offset overflowed".to_owned(),
        })?;
    let (value, bytes) = match additional {
        value @ 0..=23 => (u64::from(value), 0),
        24 => (0, 1),
        25 => (0, 2),
        26 => (0, 4),
        27 => (0, 8),
        _ => {
            return Err(MetadataError::NonCanonicalEncoding { format });
        }
    };
    if bytes == 0 {
        return Ok((major, value, next));
    }
    let end = next
        .checked_add(bytes)
        .ok_or_else(|| MetadataError::InvalidEncoding {
            format,
            reason: "CBOR header length overflowed".to_owned(),
        })?;
    let payload = encoded
        .get(next..end)
        .ok_or_else(|| MetadataError::InvalidEncoding {
            format,
            reason: "truncated CBOR integer".to_owned(),
        })?;
    next = end;
    let decoded = match bytes {
        1 => u64::from(payload[0]),
        2 => u64::from(u16::from_be_bytes([payload[0], payload[1]])),
        4 => u64::from(u32::from_be_bytes([
            payload[0], payload[1], payload[2], payload[3],
        ])),
        8 => u64::from_be_bytes([
            payload[0], payload[1], payload[2], payload[3], payload[4], payload[5], payload[6],
            payload[7],
        ]),
        _ => {
            return Err(MetadataError::InvalidEncoding {
                format,
                reason: "invalid CBOR integer width".to_owned(),
            });
        }
    };
    Ok((major, decoded, next))
}

fn check_payload_len(
    format: MetadataFormat,
    actual: usize,
    maximum: usize,
) -> Result<(), MetadataError> {
    if actual > maximum {
        return Err(MetadataError::PayloadTooLarge {
            format,
            actual,
            maximum,
        });
    }
    Ok(())
}

fn check_len(field: &'static str, value: &str, maximum: usize) -> Result<(), MetadataError> {
    if value.len() > maximum {
        return Err(MetadataError::FieldTooLong {
            field,
            actual: value.len(),
            maximum,
        });
    }
    Ok(())
}

fn sqlite_u64(field: &'static str, value: u64) -> Result<i64, MetadataError> {
    i64::try_from(value).map_err(|_| MetadataError::IntegerOutOfRange { field, value })
}

fn optional_sqlite_u64(
    field: &'static str,
    value: Option<u64>,
) -> Result<Option<i64>, MetadataError> {
    value.map(|value| sqlite_u64(field, value)).transpose()
}

fn rust_u64(field: &'static str, value: i64) -> Result<u64, MetadataError> {
    u64::try_from(value).map_err(|_| MetadataError::InvalidValue { field })
}

fn optional_rust_u64(
    field: &'static str,
    value: Option<i64>,
) -> Result<Option<u64>, MetadataError> {
    value.map(|value| rust_u64(field, value)).transpose()
}

fn parse_sql_bool(field: &'static str, value: i64) -> Result<bool, MetadataError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(MetadataError::InvalidValue { field }),
    }
}

fn download_id_from_slice(bytes: &[u8]) -> Result<DownloadId, MetadataError> {
    let array = <[u8; 16]>::try_from(bytes).map_err(|_| MetadataError::InvalidDownloadId {
        reason: "identifier is not exactly 16 bytes",
    })?;
    DownloadId::try_from_bytes(array)
}

fn to_sql_conversion_error(error: MetadataError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Null, Box::new(error))
}

fn path_to_wire(path: &Path) -> Result<PathWire, MetadataError> {
    if let Some(text) = path.to_str() {
        check_path_len(text.len())?;
        return Ok(PathWire(0, ByteString(text.as_bytes().to_vec())));
    }

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        check_path_len(bytes.len())?;
        Ok(PathWire(1, ByteString(bytes.to_vec())))
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
        let byte_len = wide
            .len()
            .checked_mul(2)
            .ok_or(MetadataError::FieldTooLong {
                field: "native_path",
                actual: usize::MAX,
                maximum: MAX_PATH_BYTES,
            })?;
        check_path_len(byte_len)?;
        let mut bytes = Vec::with_capacity(byte_len);
        for unit in wide {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        Ok(PathWire(2, ByteString(bytes)))
    }
}

fn path_from_wire(wire: PathWire) -> Result<PathBuf, MetadataError> {
    check_path_len(wire.1.0.len())?;
    match wire.0 {
        0 => String::from_utf8(wire.1.0).map(PathBuf::from).map_err(|_| {
            MetadataError::InvalidEncoding {
                format: MetadataFormat::NativePath,
                reason: "portable path is not UTF-8".to_owned(),
            }
        }),
        1 => unix_path_from_bytes(wire.1.0),
        2 => windows_path_from_bytes(wire.1.0),
        encoding => Err(MetadataError::UnsupportedPathEncoding { encoding }),
    }
}

fn encode_path(path: &Path) -> Result<Vec<u8>, MetadataError> {
    let wire = path_to_wire(path)?;
    encode_wire(&wire, MetadataFormat::NativePath)
}

fn decode_path(encoded: &[u8]) -> Result<PathBuf, MetadataError> {
    check_payload_len(
        MetadataFormat::NativePath,
        encoded.len(),
        MAX_PATH_BYTES + 16,
    )?;
    let wire: PathWire = decode_wire(encoded, MetadataFormat::NativePath)?;
    require_canonical(encoded, &wire, MetadataFormat::NativePath)?;
    path_from_wire(wire)
}

#[cfg(unix)]
fn unix_path_from_bytes(bytes: Vec<u8>) -> Result<PathBuf, MetadataError> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(not(unix))]
fn unix_path_from_bytes(_bytes: Vec<u8>) -> Result<PathBuf, MetadataError> {
    Err(MetadataError::UnsupportedPathEncoding { encoding: 1 })
}

#[cfg(windows)]
fn windows_path_from_bytes(bytes: Vec<u8>) -> Result<PathBuf, MetadataError> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    if !bytes.len().is_multiple_of(2) {
        return Err(MetadataError::InvalidEncoding {
            format: MetadataFormat::NativePath,
            reason: "Windows path has an odd UTF-16 byte count".to_owned(),
        });
    }
    let wide = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    Ok(PathBuf::from(OsString::from_wide(&wide)))
}

#[cfg(not(windows))]
fn windows_path_from_bytes(_bytes: Vec<u8>) -> Result<PathBuf, MetadataError> {
    Err(MetadataError::UnsupportedPathEncoding { encoding: 2 })
}

fn check_path_len(actual: usize) -> Result<(), MetadataError> {
    if actual > MAX_PATH_BYTES {
        return Err(MetadataError::FieldTooLong {
            field: "native_path",
            actual,
            maximum: MAX_PATH_BYTES,
        });
    }
    Ok(())
}
