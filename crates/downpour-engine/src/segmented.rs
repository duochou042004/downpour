//! Engine-owned segmented orchestration: probe, plan, fetch through N workers, verify, rename.
//!
//! This is the same shape as [`crate::download::SingleStream`] and owns the same invariant: I-4,
//! *a completed download is verified before it is named*. What changes is who fetches. The
//! allocator hands out disjoint grants, the fixed pool runs them, and the state actor performs the
//! completion sequence because ADR-0019 makes it the owner of the part file, the journal and the
//! canonical interval map at once.
//!
//! **Segmentation is never assumed.** The plan comes from the probe, and anything short of a
//! validated `206` for a known, locally addressable length falls back to one whole-representation
//! stream (I-6). The fallback is the same code path a single-connection download takes, not a
//! second implementation of it.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use downpour_http::{
    ProbeError, ProbeRequest, RangeOutcome, RangeRequest, RangeSink, TransferError,
    TransferProtocol,
};
use url::Url;

use downpour_storage::journal::FileHeader;
use downpour_storage::part_file::PartFile;
use downpour_storage::recovery::durable_state;
use downpour_storage::writer::{DurableWriter, JournalFile};
use downpour_types::RemoteObject;

use crate::download::{DownloadError, SingleStream, StorageLayout};
use crate::storage_sink::{create_journalled_artifacts, journal_path_for};
use crate::worker_pool::{FixedWorkerPool, PoolPlan};
use crate::writer_service::WriterService;
use crate::{DEFAULT_MIN_SPLIT_BYTES, SegmentAllocator};

/// How many writer commands may wait behind one download's state actor.
///
/// Bounded on purpose (ADR-0019): an unbounded queue between N network workers and one disk is an
/// out-of-memory bug waiting for a fast link.
const WRITER_QUEUE_CAPACITY: usize = 32;

/// Probe, segment, verify and rename, with a single-stream fallback.
pub struct SegmentedDownload<B> {
    backend: Arc<B>,
    connections: usize,
}

impl<B: TransferProtocol + 'static> SegmentedDownload<B> {
    /// Bind one reusable protocol backend and a fixed connection ceiling.
    #[must_use]
    pub fn new(backend: Arc<B>, connections: usize) -> Self {
        Self {
            backend,
            connections,
        }
    }

    /// Run one download to its final, verified name.
    ///
    /// # Errors
    ///
    /// Any probe, transfer, storage or verification failure. Nothing is deleted on failure: the
    /// part file and journal are what a later resume builds on (docs/04 §6).
    pub async fn download(
        &self,
        url: Url,
        layout: &StorageLayout,
    ) -> Result<PathBuf, DownloadError> {
        // One connection is not a degenerate segmented transfer, it is the single-stream
        // download, and that path already owns S2's retry, If-Range resume and re-probe triggers.
        // Routing it through the pool would silently trade all of that for segmentation machinery
        // it cannot use, so the default download would become *less* resilient than in S2.
        // Resume for the segmented path is S3-T9.
        if self.connections <= 1 {
            return SingleStream::new(ArcBackend(Arc::clone(&self.backend)))
                .download(url, layout)
                .await;
        }

        let remote = self
            .backend
            .probe(ProbeRequest::new(url.clone()))
            .await
            .map_err(DownloadError::from)?;

        let pool = FixedWorkerPool::new(Arc::clone(&self.backend), self.connections)
            .map_err(|source| DownloadError::Segmented { source })?;
        let PoolPlan::Segmented { total_length, .. } = pool.plan(&remote) else {
            // Not segmentable. The single-stream path already owns probe, retry, resume and the
            // completion sequence for this case, so it runs it rather than this module growing a
            // second copy that would drift from it.
            return SingleStream::new(ArcBackend(Arc::clone(&self.backend)))
                .download(url, layout)
                .await;
        };

        let final_path = crate::download::final_path_for(layout.target_dir(), &remote);
        crate::download::refuse_occupied_target(&final_path).await?;
        let journal_dir = layout.journal_dir().to_path_buf();
        let transfer_id = layout.transfer_id_for_remote(&remote);
        let validator_hash = crate::download::validator_hash_of(&remote.validator);
        let target = final_path.clone();
        let (writer, artifacts) = tokio::task::spawn_blocking(move || {
            create_journalled_artifacts(
                &target,
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

        let allocator = SegmentAllocator::new(total_length, DEFAULT_MIN_SPLIT_BYTES)
            .map_err(|source| DownloadError::Allocator { source })?;
        let service = WriterService::start(writer, allocator, WRITER_QUEUE_CAPACITY)
            .map_err(|source| DownloadError::Writer { source })?;

        // From here the artifacts exist, so every exit has to leave them in a resumable state
        // rather than unwind past the actor.
        let transferred = pool.execute_segmented(&remote, &service).await;
        if let Err(source) = transferred {
            drop(service.shutdown().await);
            return Err(DownloadError::Segmented { source });
        }

        let completion = service
            .complete(&artifacts.part_path, &final_path, remote.digest.clone())
            .await;
        drop(service.shutdown().await);
        completion.map_err(|source| DownloadError::Writer { source })?;

        tracing::info!(
            path = %final_path.display(),
            bytes = total_length,
            connections = self.connections,
            "segmented download complete"
        );
        Ok(final_path)
    }

    /// Resume a download from the durable artifacts a previous process left behind.
    ///
    /// `remote` is the **recorded** identity, not a fresh probe. Re-probing would replace the
    /// validator captured when the existing bytes were fetched with one that trivially matches
    /// whatever the server is serving now, which is exactly the comparison I-3 asks for, thrown
    /// away (I-8 is the reason it is recorded in the first place).
    ///
    /// Nothing in memory carries across. The journal is the only authority for what is durable,
    /// the allocator is rebuilt from it, and every byte it proved is unreachable to every grant
    /// this call issues — so the transfer asks the server only for what is actually missing.
    ///
    /// # Errors
    ///
    /// Any storage, replay, transfer or verification failure. Nothing is deleted: the part file
    /// and journal remain what a further resume would build on (docs/04 §6).
    pub async fn resume(
        &self,
        remote: &RemoteObject,
        layout: &StorageLayout,
    ) -> Result<PathBuf, DownloadError> {
        let final_path = crate::download::final_path_for(layout.target_dir(), remote);
        crate::download::refuse_occupied_target(&final_path).await?;
        let transfer_id = layout.transfer_id_for_remote(remote);
        let validator_hash = crate::download::validator_hash_of(&remote.validator);
        let journal_path = journal_path_for(layout.journal_dir(), transfer_id);

        let durable = tokio::task::spawn_blocking({
            let journal_path = journal_path.clone();
            move || durable_state(&journal_path)
        })
        .await
        .map_err(|error| DownloadError::Io {
            path: journal_path.clone(),
            source: std::io::Error::other(error.to_string()),
        })?
        .map_err(|source| DownloadError::Recovery { source })?;

        let total_length = durable.effective_length();
        if remote.total_length != Some(total_length) {
            // The journal describes a different representation length from the one the recorded
            // identity claims. Writing into it would put bytes at offsets that mean something
            // else. Refuse rather than pick one.
            return Err(DownloadError::ResumeLengthMismatch {
                journal: total_length,
                recorded: remote.total_length,
            });
        }

        let next_sequence = durable.next_sequence();
        let intervals = durable.into_intervals();
        let allocator = SegmentAllocator::resume(intervals, DEFAULT_MIN_SPLIT_BYTES)
            .map_err(|source| DownloadError::Allocator { source })?;

        let part_path = part_path_of(&final_path);
        let header = FileHeader::new(transfer_id, total_length, 0, validator_hash);
        let writer = tokio::task::spawn_blocking({
            let part_path = part_path.clone();
            let journal_path = journal_path.clone();
            move || {
                // Both no-follow, and the journal header is compared rather than trusted. Between
                // the process that died and this one, anything at all may have happened to these
                // paths.
                let part = PartFile::open_existing(&part_path, total_length)
                    .map_err(downpour_storage::writer::WriterError::from)?;
                let journal = JournalFile::open_existing(&journal_path, &header)?;
                DurableWriter::try_new(part.into_part(), journal, next_sequence)
            }
        })
        .await
        .map_err(|error| DownloadError::Io {
            path: part_path.clone(),
            source: std::io::Error::other(error.to_string()),
        })?
        .map_err(|source| DownloadError::Resume { source })?;

        let pool = FixedWorkerPool::new(Arc::clone(&self.backend), self.connections)
            .map_err(|source| DownloadError::Segmented { source })?;
        let service = WriterService::start(writer, allocator, WRITER_QUEUE_CAPACITY)
            .map_err(|source| DownloadError::Writer { source })?;

        let transferred = pool.execute_segmented(remote, &service).await;
        if let Err(source) = transferred {
            drop(service.shutdown().await);
            return Err(DownloadError::Segmented { source });
        }

        let completion = service
            .complete(&part_path, &final_path, remote.digest.clone())
            .await;
        drop(service.shutdown().await);
        completion.map_err(|source| DownloadError::Writer { source })?;

        tracing::info!(
            path = %final_path.display(),
            bytes = total_length,
            connections = self.connections,
            "segmented download resumed and completed"
        );
        Ok(final_path)
    }
}

/// The `.dppart` beside a final path, by the same rule that created it.
fn part_path_of(final_path: &std::path::Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(final_path.as_os_str());
    name.push(".dppart");
    PathBuf::from(name)
}

/// Lets the shared backend be handed to [`SingleStream`], which takes ownership of one.
///
/// The fallback has to run against the *same* client so it reuses whatever connection the probe
/// established, rather than opening a second pool for the same origin.
struct ArcBackend<B>(Arc<B>);

#[async_trait]
impl<B: TransferProtocol> TransferProtocol for ArcBackend<B> {
    async fn probe(
        &self,
        request: ProbeRequest,
    ) -> Result<downpour_types::RemoteObject, ProbeError> {
        self.0.probe(request).await
    }

    async fn fetch_range(
        &self,
        request: RangeRequest,
        sink: &mut RangeSink,
    ) -> Result<RangeOutcome, TransferError> {
        self.0.fetch_range(request, sink).await
    }

    fn capabilities(&self) -> downpour_http::BackendCapabilities {
        self.0.capabilities()
    }
}
