//! Engine-owned single-stream orchestration: probe, fetch, verify, rename.
//!
//! This module owns **I-4** for S1: *a completed download is verified before it is named.* The
//! target is written as `<name>.dppart` and moved to its real name only after the delivered
//! length matches what the probe established. A partial file wearing the final name is
//! indistinguishable from a good one to the user and to every other program on the system, which
//! makes it worse than an obvious failure.
//!
//! This module moved out of `downpour-http` at S3-T8, completing ADR-0016's required reversal.
//! Protocol code now owns only network behavior; orchestration and its concrete durable adapter
//! live together here, above the [`downpour_http::SinkTarget`] boundary.

use std::path::{Path, PathBuf};

use downpour_types::Validator;
use thiserror::Error;
use url::Url;

use crate::storage_sink::{Artifacts, StorageSink};
use downpour_http::{
    ProbeError, ProbeRequest, RangeRequest, RangeSink, ReprobePolicy, ResumePlan, RetryDecision,
    RetryPolicy, RetryState, SinkError, TransferError, TransferProtocol, TransientKind,
};
use downpour_types::{ByteRangeSpec, RemoteObject};
use std::time::SystemTime;

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
    reprobe: ReprobePolicy,
}

impl<B: TransferProtocol> SingleStream<B> {
    /// Wrap a backend, with the retry policy from `docs/03-transfer-engine-spec.md` §9.
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            policy: RetryPolicy::default(),
            reprobe: ReprobePolicy::default(),
        }
    }

    /// Override the capability freshness policy. Tests use this to force a re-probe.
    #[must_use]
    pub fn with_reprobe_policy(mut self, reprobe: ReprobePolicy) -> Self {
        self.reprobe = reprobe;
        self
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
        let final_path = final_path_for(target_dir, &remote);

        refuse_occupied_target(&final_path).await?;

        // Creating the durable artifacts is blocking work — preallocation, an exclusive create,
        // a header write and two syncs — so it does not run on the executor.
        let journal_dir = layout.journal_dir().to_path_buf();
        let target_for_sink = final_path.clone();
        let total_length = remote.total_length;
        let transfer_id = transfer_id_for(&remote.final_url);
        let validator_hash = validator_hash_of(&remote.validator);
        let digest = remote.digest.clone();
        let target = tokio::task::spawn_blocking(move || {
            StorageSink::create(
                &target_for_sink,
                &journal_dir,
                total_length,
                transfer_id,
                validator_hash,
                digest,
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
            recorded: remote,
            last_observed_status: None,
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
        // §2.3, at the moment resumed work is planned rather than after it has been sent.
        // A re-probe refreshes *capabilities* — which URL, whether ranges still work, how long
        // the representation is. It never refreshes the validator: one fetched now would match
        // whatever the server is serving now and prove nothing, which is the I-3 check S2-T9
        // exists to make. `held.validator` stays the one the existing bytes were fetched under.
        let plan = ResumePlan {
            recorded: &held.recorded,
            now: SystemTime::now(),
            planned_url: &held.final_url,
            // The workflow that obtains a refreshed URL is S8; S2 only guarantees that one
            // supplied here invalidates the cached evidence.
            refreshed_url: None,
            last_observed_status: held.last_observed_status,
        };
        let triggers = self.reprobe.triggers(&plan);
        if !triggers.is_empty() {
            tracing::info!(
                url = %held.final_url,
                ?triggers,
                "capability evidence is stale; re-probing before planning the resume"
            );
            held = self.refresh_capabilities(held).await?;
        }

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

    /// Re-establish capability evidence for a session whose cached evidence went stale (§2.3).
    ///
    /// What a re-probe may change is deliberately narrow. The final URL and the range evidence
    /// are refreshed, because those are what the resume plans against. The **validator is not**:
    /// it belongs to the bytes already on disk, and replacing it would make `If-Range` compare
    /// the server against itself. If the fresh probe shows a different validator or a different
    /// length, the representation changed while we were away — that is I-3, caught one request
    /// earlier than `If-Range` would have caught it, and it is a hard stop either way.
    async fn refresh_capabilities(&self, mut held: Session) -> Result<Session, DownloadError> {
        let fresh = self
            .backend
            .probe(ProbeRequest::new(held.final_url.clone()))
            .await?;

        let changed = fresh.validator != held.validator
            || (fresh.total_length.is_some() && fresh.total_length != held.total_length);
        if changed {
            return Err(DownloadError::Transfer(TransferError::ValidatorMismatch {
                url: fresh.final_url,
                resume_offset: held.durable_prefix_end(),
                status: 200,
                validator: held
                    .validator
                    .if_range_value()
                    .unwrap_or("<none recorded>")
                    .to_owned(),
            }));
        }

        held.final_url = fresh.final_url.clone();
        held.recorded = fresh;
        held.last_observed_status = None;
        Ok(held)
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
                held.last_observed_status = observed_status_of(&error);
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

        // Only now, and only through the full sequence: length on disk, gap-free coverage, the
        // server's digest when one was offered, then the seal, then the rename (docs/04 §6).
        // A byte counter agreeing is not verification — it cannot see a hole, and it cannot see
        // that the bytes are a different representation from the one the server meant to send.
        let final_path = held.final_path.clone();
        if let Err(source) = held.sink.verify_and_rename(final_path.clone()).await {
            // Nothing is deleted. The part file and journal are evidence, and the user may want
            // to retry rather than start over (docs/04 §6).
            let path = part_path.clone();
            *session = Some(held);
            return Err(DownloadError::Unverified { path, source });
        }

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
    /// The validator recorded when the existing bytes were fetched. Never refreshed (I-3).
    validator: Validator,
    /// The capability evidence a resume plans against. A re-probe replaces this; the validator
    /// above deliberately survives it.
    recorded: RemoteObject,
    final_url: Url,
    total_length: Option<u64>,
    final_path: PathBuf,
    artifacts: Artifacts,
    /// The status the last attempt observed, when one reached a response at all.
    last_observed_status: Option<u16>,
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
    /// The segmented worker pool could not run this transfer to full coverage.
    #[error("segmented transfer failed: {source}")]
    Segmented {
        /// Scheduling or worker failure.
        #[source]
        source: crate::worker_pool::PoolError,
    },
    /// The byte space could not be prepared for segmentation.
    #[error("segment allocator refused this representation: {source}")]
    Allocator {
        /// Allocator configuration failure.
        #[source]
        source: crate::AllocatorError,
    },
    /// The download-state actor failed to start, store, or complete this transfer.
    #[error("download-state actor failed: {source}")]
    Writer {
        /// Actor, storage or verification failure.
        #[source]
        source: crate::writer_service::WriterServiceError,
    },
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
    /// Verification refused to give the file its final name (I-4).
    ///
    /// Distinct from [`Self::Incomplete`], which is a byte count disagreeing. This is one of the
    /// checks that a byte count cannot make: a hole the journal never covered, or bytes the
    /// server's own digest disowns.
    #[error("{path} did not pass verification and was not renamed: {source}")]
    Unverified {
        /// The `.dppart`, kept as evidence.
        path: PathBuf,
        /// Which check refused.
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
            Self::Unverified { .. } => "unverified",
            // Verification is verification wherever it runs, so the segmented path reports the
            // same kind for the same refusal rather than a wrapper clients would have to learn.
            Self::Writer {
                source: crate::writer_service::WriterServiceError::Unverified { .. },
            } => "unverified",
            Self::Segmented { .. } => "segmented_transfer",
            Self::Allocator { .. } => "allocator",
            Self::Writer { .. } => "download_state",
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
/// Where a probed representation's finished file belongs.
///
/// The name is already sanitised by the probe, so it is exactly one path component and cannot
/// escape `target_dir` (S1-C3). Joining is therefore safe by construction rather than by check.
pub(crate) fn final_path_for(target_dir: &Path, remote: &RemoteObject) -> PathBuf {
    let name = remote
        .suggested_filename
        .clone()
        .unwrap_or_else(|| "download".to_owned());
    target_dir.join(&name)
}

/// Refuse a target that already holds anything at all, before anything is created.
///
/// Shared by the single-stream and segmented paths deliberately. Refusing rather than picking a
/// "(1)" suffix is a policy decision that belongs with the rest of the local-collision handling;
/// silently replacing a file the user already has is unrecoverable.
///
/// `symlink_metadata` does not follow links, and that is the whole reason it is used here.
/// `try_exists` and `exists` both follow: for a symlink whose destination does not exist they
/// report the path as free, and it is not free — the rename at the end of a download replaces the
/// link itself, so proceeding destroys something the user put there without ever saying so.
/// Anything at this path at all, file or directory or link, broken or not, belongs to the user.
/// Found by local/a-dangling-symlink-occupies-the-target.
pub(crate) async fn refuse_occupied_target(final_path: &Path) -> Result<(), DownloadError> {
    match tokio::fs::symlink_metadata(final_path).await {
        Ok(_) => Err(DownloadError::TargetExists {
            path: final_path.to_path_buf(),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        // A check that could not answer must never be read as "nothing is there". Permission
        // denied on a parent, a symlink loop, or a name too long all arrive here, and reading any
        // of them as absence is permission to proceed over the user's data.
        Err(source) => Err(DownloadError::Io {
            path: final_path.to_path_buf(),
            source,
        }),
    }
}

pub(crate) fn transfer_id_for(final_url: &Url) -> [u8; 16] {
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
pub(crate) fn validator_hash_of(validator: &Validator) -> [u8; 32] {
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

/// The status a failed transfer observed, when it reached a response at all.
///
/// Feeds §2.3's "a worker received a status inconsistent with the recorded capabilities"
/// trigger. A transport failure or a timeout observed no status, and reporting one would invent
/// evidence.
fn observed_status_of(error: &TransferError) -> Option<u16> {
    match error {
        TransferError::UnexpectedStatus { status, .. }
        | TransferError::ValidatorMismatch { status, .. } => Some(*status),
        _ => None,
    }
}
