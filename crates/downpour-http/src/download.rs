//! The single-stream download: probe, fetch, verify, rename.
//!
//! This module owns **I-4** for S1: *a completed download is verified before it is named.* The
//! target is written as `<name>.dppart` and moved to its real name only after the delivered
//! length matches what the probe established. A partial file wearing the final name is
//! indistinguishable from a good one to the user and to every other program on the system, which
//! makes it worse than an obvious failure.
//!
//! **This is a stage-1 arrangement, not the intended architecture.** Orchestration belongs in
//! `downpour-engine` behind the daemon's IPC (`docs/02-architecture.md` §3, and the hard rule
//! that clients never link the engine), and durable writes belong in `downpour-storage`, which
//! owns preallocation, positional writes and the journal ordering I-1 requires. Neither crate
//! exists until S2/S3. Backlog B-4 and B-5 record the move; the [`crate::sink::SinkTarget`]
//! boundary is what makes it a substitution rather than a rewrite.

use std::path::{Path, PathBuf};

use downpour_types::Validator;
use thiserror::Error;
use url::Url;

use crate::error::{ProbeError, TransferError};
use crate::protocol::{ProbeRequest, RangeRequest, TransferProtocol};
use crate::retry::{RetryDecision, RetryPolicy, RetryState, TransientKind};
use crate::sink::{RangeSink, SinkError};
use crate::storage_sink::{Artifacts, StorageSink};
use downpour_types::ByteRangeSpec;

/// Extension for a download in progress. Never the final name (I-4).
pub const PART_EXTENSION: &str = "dppart";

/// Extension for a download's recovery journal.
pub const JOURNAL_EXTENSION: &str = "dpj";

/// Where a download's data and its recovery state live.
///
/// Two directories rather than one, because `docs/04-storage-and-recovery-spec.md` §1 puts the
/// part file next to the target — so the final rename is same-filesystem and therefore atomic —
/// and the journal with application state, so that clearing a downloads folder does not silently
/// destroy the recovery information for an active transfer.
#[derive(Clone, Debug)]
pub struct StorageLayout {
    target_dir: PathBuf,
    journal_dir: PathBuf,
}

impl StorageLayout {
    /// Data in `target_dir`, recovery journals in `journal_dir`.
    pub fn new(target_dir: impl Into<PathBuf>, journal_dir: impl Into<PathBuf>) -> Self {
        Self {
            target_dir: target_dir.into(),
            journal_dir: journal_dir.into(),
        }
    }

    /// Where the finished file and its `.dppart` live.
    #[must_use]
    pub fn target_dir(&self) -> &Path {
        &self.target_dir
    }

    /// Where recovery journals live.
    #[must_use]
    pub fn journal_dir(&self) -> &Path {
        &self.journal_dir
    }
}

/// A whole-file, single-connection download.
pub struct SingleStream<B> {
    backend: B,
    policy: RetryPolicy,
}

impl<B: TransferProtocol> SingleStream<B> {
    /// Wrap a backend, with the retry policy from `docs/03-transfer-engine-spec.md` §9.
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            policy: RetryPolicy::default(),
        }
    }

    /// Override the retry policy. Tests use this to shrink the delays.
    #[must_use]
    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Probe, fetch, verify and rename — retrying transient failures per §7.
    ///
    /// **Each attempt restarts from byte 0**, and that is deliberate rather than lazy. Resuming from
    /// the bytes already in the `.dppart` requires proving the remote representation has not changed
    /// in the meantime, which means `If-Range` against a strong validator — that is I-3, and it
    /// arrives with the rest of the resume machinery in S2. Resuming without it would splice two
    /// versions of a file together at exactly the expected size, which is the specific corruption
    /// I-3 exists to prevent. So S1's retry is correct-but-wasteful on purpose.
    ///
    /// # Errors
    ///
    /// The last error seen, once the failure is not retryable or the budget is spent.
    pub async fn download(
        &self,
        url: Url,
        layout: &StorageLayout,
    ) -> Result<PathBuf, DownloadError> {
        // Keyed by origin, not by attempt, because that is what §7 requires once S3 opens N
        // connections. With one worker it is a map of size one.
        let mut retries = RetryState::new();
        let origin = format!("{}://{}", url.scheme(), url.authority());

        // Carried across attempts so a retry can resume into the same durable artifacts
        // instead of starting a second copy of the download.
        let mut session: Option<Session> = None;

        loop {
            let error = match self.attempt(url.clone(), layout, &mut session).await {
                Ok(path) => {
                    retries.reset(&origin);
                    return Ok(path);
                }
                Err(error) => error,
            };

            let Some(kind) = transient_kind_of(&error) else {
                return Err(error);
            };
            // `record_failure` returns the count including this one, so the attempt index the policy
            // sees is one less.
            let failures = retries.record_failure(&origin);
            let attempt = failures.saturating_sub(1);

            match self.policy.decide(kind, attempt, retry_after_of(&error)) {
                RetryDecision::RetryAfter(delay) => {
                    // Resume needs two things, and neither is optional. A validator usable for
                    // `If-Range`, so the server can tell us whether our bytes still belong to
                    // the representation it is about to serve (I-3) — a weak ETag will not do,
                    // since it may compare equal across representations that differ byte for
                    // byte. And durable bytes to resume *from*, which means past the writer's
                    // commit point, not merely written.
                    let resume_from = session.as_ref().map_or(0, Session::durable_prefix_end);
                    let resumable = session
                        .as_ref()
                        .is_some_and(|held| held.validator.if_range_value().is_some())
                        && resume_from > 0;

                    if resumable {
                        tracing::warn!(
                            url = %url,
                            attempt = failures,
                            delay_ms = delay.as_millis(),
                            resume_from,
                            error = %error,
                            "transient failure; resuming with If-Range"
                        );
                    } else {
                        tracing::warn!(
                            url = %url,
                            attempt = failures,
                            delay_ms = delay.as_millis(),
                            error = %error,
                            "transient failure; restarting from byte 0"
                        );
                        // Starting over means the previous attempt's artifacts must go: both
                        // files are created exclusively, so a retry would otherwise collide with
                        // its own predecessor. When the budget runs out instead, the last
                        // attempt's artifacts survive — that is what a later resume builds on.
                        if let Some(previous) = session.take() {
                            remove_artifacts(&previous.artifacts).await;
                        }
                    }
                    tokio::time::sleep(delay).await;
                }
                RetryDecision::GiveUp => return Err(error),
            }
        }
    }

    /// One attempt: probe (or resume an existing session), fetch, verify, rename.
    ///
    /// The session is left in `session` on failure so [`Self::download`] can either resume into
    /// it or discard it. A resumed attempt does not re-probe: the validator that matters is the
    /// one recorded when the existing bytes were fetched, and re-probing would replace it with a
    /// fresh one that trivially matches whatever the server is serving now — which is exactly
    /// the check I-3 asks for, thrown away. Re-probe triggers are S2-T10.
    async fn attempt(
        &self,
        url: Url,
        layout: &StorageLayout,
        session: &mut Option<Session>,
    ) -> Result<PathBuf, DownloadError> {
        if let Some(existing) = session.take() {
            return self.resume(existing, session).await;
        }

        let target_dir = layout.target_dir();
        let remote = self.backend.probe(ProbeRequest::new(url)).await?;

        // Already sanitised by the probe, so it is exactly one path component and cannot escape
        // `target_dir` (S1-C3). Joining is therefore safe by construction rather than by check.
        let name = remote
            .suggested_filename
            .clone()
            .unwrap_or_else(|| "download".to_owned());
        let final_path = target_dir.join(&name);

        // Checked before anything is created, so a refusal leaves the directory exactly as it
        // was. Refusing rather than picking a "(1)" suffix is deliberate for S1: silently
        // replacing a file the user already has is unrecoverable, and choosing a new name is a
        // policy decision that belongs with the rest of the local-collision handling in S2.
        if tokio::fs::try_exists(&final_path).await.unwrap_or(false) {
            return Err(DownloadError::TargetExists { path: final_path });
        }

        // Creating the durable artifacts is blocking work — preallocation, an exclusive create,
        // a header write and two syncs — so it does not run on the executor.
        let journal_dir = layout.journal_dir().to_path_buf();
        let target_for_sink = final_path.clone();
        let total_length = remote.total_length;
        let transfer_id = transfer_id_for(&remote.final_url);
        let validator_hash = validator_hash_of(&remote.validator);
        let target = tokio::task::spawn_blocking(move || {
            StorageSink::create(
                &target_for_sink,
                &journal_dir,
                total_length,
                transfer_id,
                validator_hash,
            )
        })
        .await
        .map_err(|error| DownloadError::Io {
            path: final_path.clone(),
            source: std::io::Error::other(error.to_string()),
        })?
        .map_err(|source| DownloadError::Sink {
            path: final_path.clone(),
            source,
        })?;
        let artifacts = target.artifacts().clone();

        // The limit is what the probe established. A server that delivers more than it declared
        // is refused at the sink rather than written, so it cannot run past the end of the
        // representation. Absent a declared length — close-delimited framing — there is nothing
        // to bound it with, and the length is whatever arrived.
        let sink = RangeSink::new(Box::new(target), 0, remote.total_length);
        let held = Session {
            sink,
            validator: remote.validator.clone(),
            final_url: remote.final_url.clone(),
            total_length: remote.total_length,
            final_path,
            artifacts,
        };
        let request = RangeRequest::whole(held.final_url.clone());
        self.run(held, request, session).await
    }

    /// Continue an existing session from its durable prefix, conditional on the validator (I-3).
    async fn resume(
        &self,
        mut held: Session,
        session: &mut Option<Session>,
    ) -> Result<PathBuf, DownloadError> {
        let from = held.durable_prefix_end();
        let Some(validator) = held.validator.if_range_value().map(str::to_owned) else {
            // download() only resumes when a usable validator exists, so reaching here would be
            // a logic error rather than a server behaviour. Refuse rather than silently
            // continuing without the one check that makes resume safe.
            let path = held.artifacts.part_path.clone();
            *session = Some(held);
            return Err(DownloadError::Incomplete {
                path,
                expected: None,
                actual: from,
            });
        };

        // The window moves; the target does not. The backend still cannot address a byte outside
        // what it was granted, it is just granted a different part of the file now.
        let remaining = held.total_length.map(|total| total.saturating_sub(from));
        held.sink.rebase(from, remaining);
        let request = RangeRequest::resume(
            held.final_url.clone(),
            ByteRangeSpec::From { first: from },
            validator,
        );
        self.run(held, request, session).await
    }

    /// Fetch into a session, make it durable, verify, and rename.
    async fn run(
        &self,
        mut held: Session,
        request: RangeRequest,
        session: &mut Option<Session>,
    ) -> Result<PathBuf, DownloadError> {
        let part_path = held.artifacts.part_path.clone();
        let fetched = self.backend.fetch_range(request, &mut held.sink).await;

        // I-1's commit point, in full: inside the writer this is data sync, journal append,
        // journal sync, then the interval map moves to Complete. Nothing here reorders it.
        //
        // Done before the fetch result is inspected, and deliberately so: whatever arrived is
        // what a later resume will build on, so it has to survive a crash even when the fetch
        // failed. Returning early here would leave those bytes in the page cache only.
        if let Err(source) = held.sink.sync().await {
            let path = part_path.clone();
            *session = Some(held);
            return Err(DownloadError::Sink { path, source });
        }

        let outcome = match fetched {
            Ok(outcome) => outcome,
            Err(error) => {
                *session = Some(held);
                return Err(error.into());
            }
        };

        // Verification, before the rename and with no fast path around it (I-4). `next_offset`
        // rather than `written`: after a resume the window starts at the durable prefix, so what
        // matters is where the file now ends, not how much this attempt contributed.
        let delivered = held.sink.next_offset();
        let total_length = held.total_length;
        if total_length.is_some_and(|expected| delivered != expected) || outcome.truncated {
            let path = part_path.clone();
            *session = Some(held);
            return Err(DownloadError::Incomplete {
                path,
                expected: total_length,
                actual: delivered,
            });
        }

        // Only now. Rename is atomic within a directory on every platform we target, so there is
        // no window in which the final name refers to an incomplete file.
        let final_path = held.final_path.clone();
        tokio::fs::rename(&part_path, &final_path)
            .await
            .map_err(|source| DownloadError::Io {
                path: final_path.clone(),
                source,
            })?;

        tracing::info!(
            path = %final_path.display(),
            bytes = delivered,
            protocol = %outcome.protocol,
            "download complete"
        );
        Ok(final_path)
    }
}

/// One download's live state, carried across the retries of a single `download` call.
///
/// It exists so a retry can continue into the same durable artifacts rather than start a second
/// copy. The validator is the one recorded when the existing bytes were fetched and is never
/// refreshed, because a validator taken from the retry's own response would match whatever the
/// server is serving now and prove nothing (I-3).
struct Session {
    sink: RangeSink,
    validator: Validator,
    final_url: Url,
    total_length: Option<u64>,
    final_path: PathBuf,
    artifacts: Artifacts,
}

impl Session {
    /// Where a resume may continue from: the end of the contiguous durable prefix.
    fn durable_prefix_end(&self) -> u64 {
        self.sink.durable_prefix_end()
    }
}

/// Which transient class an error belongs to, or `None` if retrying cannot help.
///
/// The mapping is the table in `docs/03-transfer-engine-spec.md` §7. `NeedsRefresh` is absent on
/// purpose: a `401`, `403` or `410` means the URL or credential went stale, and the answer is the
/// refresh flow (I-8) which keeps the bytes already fetched — not hammering the same dead URL.
fn transient_kind_of(error: &DownloadError) -> Option<TransientKind> {
    match error {
        DownloadError::Probe(ProbeError::Transport { .. })
        | DownloadError::Transfer(TransferError::Transport { .. }) => {
            Some(TransientKind::ConnectionReset)
        }
        DownloadError::Probe(ProbeError::Timeout { .. })
        | DownloadError::Transfer(TransferError::Timeout { .. }) => Some(TransientKind::Timeout),
        DownloadError::Transfer(TransferError::TruncatedBody { .. }) => {
            Some(TransientKind::TruncatedBody)
        }
        // A body shorter than the probe established, caught at verification rather than by the
        // transport. Same underlying cause, so the same treatment.
        DownloadError::Incomplete { .. } => Some(TransientKind::TruncatedBody),
        DownloadError::Probe(ProbeError::UnexpectedStatus { status, .. })
        | DownloadError::Transfer(TransferError::UnexpectedStatus { status, .. }) => {
            TransientKind::from_status(*status)
        }
        // Everything else is a decision, not an accident: a refused encoding, a login page, an
        // occupied target, a full disk. Retrying changes none of them.
        _ => None,
    }
}

/// The `Retry-After` the server sent, if the error carries one.
fn retry_after_of(error: &DownloadError) -> Option<&str> {
    match error {
        DownloadError::Probe(ProbeError::UnexpectedStatus { retry_after, .. })
        | DownloadError::Transfer(TransferError::UnexpectedStatus { retry_after, .. }) => {
            retry_after.as_deref()
        }
        _ => None,
    }
}

/// Why a download did not produce a verified file.
#[derive(Debug, Error)]
pub enum DownloadError {
    /// The probe rejected the URL or the response.
    #[error(transparent)]
    Probe(#[from] ProbeError),
    /// The transfer failed.
    #[error(transparent)]
    Transfer(#[from] TransferError),
    /// Fewer bytes arrived than the probe established. **The final name is not created** — this
    /// is the error whose absence produces a short file that looks complete (I-4).
    #[error("{path} holds {actual} bytes but {expected:?} were expected; not renamed")]
    Incomplete {
        /// The `.dppart` file, kept so a later resume can use what did arrive.
        path: PathBuf,
        /// What the probe established, when it established anything.
        expected: Option<u64>,
        /// What actually arrived.
        actual: u64,
    },
    /// Something already exists at the final path.
    #[error("{path} already exists; refusing to overwrite it")]
    TargetExists {
        /// The path that is occupied.
        path: PathBuf,
    },
    /// A write or a sync failed.
    #[error("storing {path} failed: {source}")]
    Sink {
        /// The file being written.
        path: PathBuf,
        /// The underlying sink error.
        #[source]
        source: SinkError,
    },
    /// A filesystem operation around the transfer failed.
    #[error("{path}: {source}")]
    Io {
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
}

impl DownloadError {
    /// The stable identifier clients and corpus cases switch on. **This is API.**
    ///
    /// Probe and transfer failures report the underlying kind rather than a wrapper, so a case
    /// asserting `unexpected_content_encoding` does not have to know at which layer the response
    /// was refused — only that it was.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Probe(error) => error.kind(),
            Self::Transfer(error) => error.kind(),
            Self::Incomplete { .. } => "incomplete",
            Self::TargetExists { .. } => "target_exists",
            Self::Sink { .. } => "sink",
            Self::Io { .. } => "io",
        }
    }
}

/// A stable journal identity for one representation.
///
/// Derived from the final URL rather than allocated, because there is no id allocator until the
/// daemon owns one. Deterministic is the useful property here: the retries inside one
/// [`SingleStream::download`] call resolve to the same journal path, so a retry collides with its
/// own predecessor rather than silently accumulating orphans — and the collision is what forces
/// the cleanup to be explicit.
fn transfer_id_for(final_url: &Url) -> [u8; 16] {
    let digest = blake3::hash(final_url.as_str().as_bytes());
    let mut id = [0_u8; 16];
    id.copy_from_slice(&digest.as_bytes()[..16]);
    id
}

/// The journal header's binding to the remote validator (I-3).
///
/// A representation with no usable validator hashes to zero rather than to something
/// arbitrary — "nothing to compare against" is the honest record, and it is what a resume must
/// refuse on.
fn validator_hash_of(validator: &Validator) -> [u8; 32] {
    match validator {
        Validator::StrongETag(value) => {
            *blake3::hash(format!("etag:{value}").as_bytes()).as_bytes()
        }
        Validator::LastModified(value) => {
            *blake3::hash(format!("last-modified:{value}").as_bytes()).as_bytes()
        }
        Validator::None => [0_u8; 32],
    }
}

/// Remove one attempt's durable artifacts before the next attempt recreates them.
///
/// Failures are logged rather than propagated: the caller is already handling a transient error
/// and is about to retry, and a leftover file makes the retry fail loudly at its exclusive
/// create. Silently swallowing the result would hide that, so it is warned about instead.
async fn remove_artifacts(artifacts: &Artifacts) {
    if let Err(error) = tokio::fs::remove_file(&artifacts.part_path).await {
        tracing::warn!(
            path = %artifacts.part_path.display(),
            %error,
            "could not remove the previous attempt's part file"
        );
    }
    if let Some(journal) = &artifacts.journal_path
        && let Err(error) = tokio::fs::remove_file(journal).await
    {
        tracing::warn!(
            path = %journal.display(),
            %error,
            "could not remove the previous attempt's recovery journal"
        );
    }
}
