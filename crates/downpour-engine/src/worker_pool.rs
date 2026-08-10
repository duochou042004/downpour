//! Fixed HTTP/1.1 worker planning and execution.
//!
//! The pool sits above [`downpour_http::TransferProtocol`], so scheduling never names an HTTP
//! client library. It may segment only when the probe produced validated range evidence for a
//! known, locally addressable representation. Every other input is a one-request whole-stream
//! fallback.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use downpour_http::{
    RangeRequest, RangeSink, RetryDecision, RetryPolicy, SinkError, SinkTarget, TransferError,
    TransferProtocol, TransientKind,
};
use downpour_intervals::{IntervalState, WorkerId};
use downpour_storage::writer::{JOURNAL_FLUSH_INTERVAL, WriterError};
use downpour_types::{ByteRangeSpec, ContentRange, NegotiatedProtocol, RemoteObject};
use thiserror::Error;
use tokio::sync::oneshot;
use tokio::task::{JoinError, JoinSet};
use tokio::time::MissedTickBehavior;

use crate::Grant;
use crate::writer_service::{GrantWriter, WriterService, WriterServiceError};

/// Why this Stage 3 scheduler must execute exactly one whole-representation request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SingleStreamReason {
    /// The observed protocol is not HTTP/1.1; stream scheduling arrives in later stages.
    ProtocolNotHttp11,
    /// The selected backend cannot express byte ranges.
    BackendCannotRange,
    /// The probe did not prove byte-range behavior from a validated response.
    RangeNotProven,
    /// No total representation length is known.
    LengthUnknown,
    /// The remote object's length disagrees with its range proof.
    InconsistentLength,
    /// The known length cannot be represented by the local part-file implementation.
    LengthUnaddressable,
}

/// Fixed execution shape selected from probe evidence and backend capabilities.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PoolPlan {
    /// Use up to `workers` independently ranged HTTP/1.1 workers.
    Segmented {
        /// Validated representation length.
        total_length: u64,
        /// Configured fixed concurrency ceiling.
        workers: usize,
    },
    /// Execute one whole-representation request.
    SingleStream {
        /// Evidence that made segmentation unsafe or inapplicable.
        reason: SingleStreamReason,
    },
}

/// One worker's completed byte count and elapsed transfer time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerReport {
    worker: downpour_intervals::WorkerId,
    bytes: u64,
    elapsed: Duration,
}

impl WorkerReport {
    /// Stable worker identity used by allocator grants.
    #[must_use]
    pub const fn worker(&self) -> downpour_intervals::WorkerId {
        self.worker
    }

    /// Bytes accepted from the worker's response.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Time spent in the protocol fetch, retained for per-worker throughput reporting.
    #[must_use]
    pub const fn elapsed(&self) -> Duration {
        self.elapsed
    }
}

/// Observable result of one fixed-pool execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PoolReport {
    plan: PoolPlan,
    workers: Vec<WorkerReport>,
}

impl PoolReport {
    /// Execution plan that produced this report.
    #[must_use]
    pub const fn plan(&self) -> PoolPlan {
        self.plan
    }

    /// Per-worker transfer records.
    #[must_use]
    pub fn workers(&self) -> &[WorkerReport] {
        &self.workers
    }
}

/// A fixed-pool request that could not be executed safely.
#[derive(Debug, Error)]
pub enum PoolError {
    /// A pool without a worker cannot make progress.
    #[error("fixed worker count must be greater than zero")]
    ZeroWorkers,
    /// The caller selected an execution method that contradicts the fail-closed plan.
    #[error("worker execution does not match plan {plan:?}")]
    WrongPlan {
        /// Plan selected from probe evidence.
        plan: PoolPlan,
    },
    /// The writer actor could not serialize a storage or allocation operation.
    #[error("writer service failed: {0}")]
    Writer(#[from] WriterServiceError),
    /// The protocol backend rejected or failed one worker request.
    #[error("worker transfer failed: {0}")]
    Transfer(Box<TransferError>),
    /// A worker-facing range sink could not commit its bytes.
    #[error("worker sink failed: {0}")]
    Sink(#[from] SinkError),
    /// A spawned worker task panicked or was cancelled unexpectedly.
    #[error("worker task did not finish cleanly: {source}")]
    WorkerJoin {
        /// Tokio task failure.
        #[source]
        source: JoinError,
    },
    /// The configured worker index did not fit the allocator's stable identifier.
    #[error("fixed worker index {index} does not fit in u64")]
    WorkerIdOverflow {
        /// Unrepresentable index.
        index: usize,
    },
    /// The writer actor and probe describe different representations.
    #[error("writer covers {writer_length} bytes but the probe established {remote_length}")]
    WriterLengthMismatch {
        /// Length represented by the actor map.
        writer_length: u64,
        /// Length validated by the probe.
        remote_length: u64,
    },
    /// A backend outcome contradicted the exact request or sink observation.
    #[error("worker {worker:?} returned an invalid outcome: {reason}")]
    InvalidOutcome {
        /// Worker whose response was inconsistent.
        worker: WorkerId,
        /// Stable diagnostic reason.
        reason: &'static str,
    },
    /// All workers stopped but durable coverage was not complete.
    #[error("fixed worker pool stopped with incomplete durable coverage")]
    IncompleteCoverage,
}

impl From<TransferError> for PoolError {
    fn from(error: TransferError) -> Self {
        Self::Transfer(Box::new(error))
    }
}

/// Stage 3's configured, non-adaptive HTTP/1.1 worker pool.
pub struct FixedWorkerPool<B> {
    backend: Arc<B>,
    workers: usize,
}

struct RunningWorker {
    cancel: Option<oneshot::Sender<()>>,
    grant: Grant,
    started: Instant,
}

enum WorkerEvent {
    Finished {
        worker: WorkerId,
        result: Result<WorkerReport, PoolError>,
    },
    Cancelled {
        worker: WorkerId,
    },
}

impl<B: TransferProtocol + 'static> FixedWorkerPool<B> {
    /// Bind one reusable protocol backend and a fixed worker ceiling.
    pub fn new(backend: Arc<B>, workers: usize) -> Result<Self, PoolError> {
        if workers == 0 {
            return Err(PoolError::ZeroWorkers);
        }
        Ok(Self { backend, workers })
    }

    /// Select segmented or whole-stream execution from validated probe evidence.
    #[must_use]
    pub fn plan(&self, remote: &RemoteObject) -> PoolPlan {
        if remote.protocol != NegotiatedProtocol::Http11 {
            return single(SingleStreamReason::ProtocolNotHttp11);
        }
        if !self.backend.capabilities().supports_ranges {
            return single(SingleStreamReason::BackendCannotRange);
        }
        let Some(proven_length) = remote.range_support.total_length() else {
            return single(SingleStreamReason::RangeNotProven);
        };
        let Some(total_length) = remote.total_length else {
            return single(SingleStreamReason::LengthUnknown);
        };
        if total_length != proven_length {
            return single(SingleStreamReason::InconsistentLength);
        }
        if i64::try_from(total_length).is_err() {
            return single(SingleStreamReason::LengthUnaddressable);
        }
        PoolPlan::Segmented {
            total_length,
            workers: self.workers,
        }
    }

    /// Execute a segmented plan against the sole writer service.
    ///
    /// A worker that finishes its segment asks for more work. Pending work is handed over with
    /// nothing interrupted. When there is none, exactly one peer — the owner of the segment the
    /// allocator would split — is stopped so its grant can be shortened safely; every other
    /// connection keeps transferring. See [`SegmentAllocator::allocate`] for why a live grant
    /// cannot be shortened underneath its own response.
    pub async fn execute_segmented(
        &self,
        remote: &RemoteObject,
        writer: &WriterService,
    ) -> Result<PoolReport, PoolError> {
        let plan = self.plan(remote);
        let PoolPlan::Segmented {
            total_length,
            workers,
        } = plan
        else {
            return Err(PoolError::WrongPlan { plan });
        };
        self.ensure_writer_identity(writer, total_length).await?;

        let mut pool_workers = Vec::with_capacity(workers);
        for index in 0..workers {
            let raw = u64::try_from(index).map_err(|_| PoolError::WorkerIdOverflow { index })?;
            pool_workers.push(WorkerId::new(raw));
        }
        let everyone = pool_workers.iter().copied().collect::<BTreeSet<_>>();

        // Nothing is running yet, so every worker is quiesced and each request after the first
        // splits the largest grant issued so far.
        let mut grants = BTreeMap::new();
        for worker in &pool_workers {
            let receipt = writer.allocate(*worker, everyone.clone()).await?;
            let Some(allocation) = receipt.allocation() else {
                break;
            };
            if let Some(shortened) = allocation.shortened() {
                grants.insert(shortened.worker(), shortened.clone());
            }
            grants.insert(*worker, allocation.grant().clone());
        }
        if grants.is_empty() {
            return Err(PoolError::IncompleteCoverage);
        }

        let mut scheduler = Scheduler {
            backend: Arc::clone(&self.backend),
            writer,
            url: remote.final_url.clone(),
            total_length,
            workers: pool_workers,
            tasks: JoinSet::new(),
            running: BTreeMap::new(),
            idle: VecDeque::new(),
            parked: BTreeSet::new(),
            stopping: None,
            reports: Vec::new(),
        };
        // A worker the representation was too small to give a share of is parked rather than
        // forgotten, so an abandon that puts bytes back into Pending can still reach it.
        for worker in scheduler.workers.clone() {
            if !grants.contains_key(&worker) {
                scheduler.parked.insert(worker);
            }
        }
        for grant in grants.into_values() {
            scheduler.spawn(grant);
        }

        let mut ticker = tokio::time::interval(JOURNAL_FLUSH_INTERVAL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        ticker.tick().await;
        let retry_policy = RetryPolicy::default();
        let mut transient_failures = 0_u32;

        loop {
            scheduler.service_idle_workers().await?;

            if scheduler.tasks.is_empty() {
                writer.flush().await?;
                let snapshot = writer.snapshot().await?;
                if snapshot
                    .allocator()
                    .intervals()
                    .iter()
                    .all(|interval| interval.state() == &IntervalState::Complete)
                {
                    break;
                }
                // Nothing is running and no idle worker could be given work. Any interval still
                // owned by a worker without a task is one this pool must finish itself.
                let revived = scheduler.revive_untasked_grants(&snapshot);
                if revived == 0 {
                    return Err(PoolError::IncompleteCoverage);
                }
                continue;
            }

            tokio::select! {
                joined = scheduler.tasks.join_next() => {
                    let Some(joined) = joined else {
                        scheduler.reclaim_after_failure().await?;
                        return Err(PoolError::IncompleteCoverage);
                    };
                    match joined {
                        Ok(WorkerEvent::Finished { worker, result: Ok(report) }) => {
                            scheduler.running.remove(&worker);
                            scheduler.reports.push(report);
                            scheduler.make_idle(worker);
                        }
                        Ok(WorkerEvent::Finished { worker, result: Err(error) }) => {
                            let Some(attempt) = scheduler.running.remove(&worker) else {
                                scheduler.reclaim_after_failure().await?;
                                return Err(PoolError::IncompleteCoverage);
                            };
                            let Some(kind) = retryable_worker_failure(&error) else {
                                scheduler.reclaim_after_failure().await?;
                                return Err(error);
                            };
                            match retry_policy.decide(kind, transient_failures, None) {
                                RetryDecision::GiveUp => {
                                    scheduler.reclaim_after_failure().await?;
                                    return Err(error);
                                }
                                RetryDecision::RetryAfter(delay) => {
                                    transient_failures = transient_failures.saturating_add(1);

                                    // The failed fetch is gone, but its accepted prefix may still
                                    // be staged. Abandon fences it durably and returns only the
                                    // unwritten suffix to Pending, so the attempt's byte count is
                                    // exact at this moment and the remainder is available to any
                                    // idle worker. Peers are untouched: none of them was writing
                                    // inside this grant.
                                    writer.abandon(worker).await?;
                                    scheduler.record_stopped_attempt(&attempt).await?;
                                    scheduler.make_idle(worker);
                                    scheduler.unpark_everyone();
                                    tokio::time::sleep(delay).await;
                                }
                            }
                        }
                        Ok(WorkerEvent::Cancelled { worker }) => {
                            let Some(attempt) = scheduler.running.remove(&worker) else {
                                scheduler.reclaim_after_failure().await?;
                                return Err(PoolError::IncompleteCoverage);
                            };
                            // Stopped so its segment could be split. Fence what it accepted before
                            // measuring it: after the split, bytes inside the old grant belong to
                            // two workers and the count would no longer be attributable.
                            scheduler.record_stopped_attempt(&attempt).await?;
                            if scheduler.stopping == Some(worker) {
                                scheduler.stopping = None;
                            }
                        }
                        Err(source) => {
                            scheduler.reclaim_after_failure().await?;
                            return Err(PoolError::WorkerJoin { source });
                        }
                    }
                }
                _ = ticker.tick() => {
                    writer.flush_if_due().await?;
                }
            }
        }
        writer.flush().await?;
        let snapshot = writer.snapshot().await?;
        if snapshot
            .allocator()
            .intervals()
            .iter()
            .any(|interval| interval.state() != &IntervalState::Complete)
        {
            return Err(PoolError::IncompleteCoverage);
        }
        let mut reports = scheduler.reports;
        reports.sort_by_key(WorkerReport::worker);
        Ok(PoolReport {
            plan,
            workers: reports,
        })
    }

    /// Execute a whole-representation fallback through exactly one sink.
    pub async fn execute_single(
        &self,
        remote: &RemoteObject,
        mut sink: RangeSink,
    ) -> Result<PoolReport, PoolError> {
        let plan = self.plan(remote);
        if matches!(plan, PoolPlan::Segmented { .. }) {
            return Err(PoolError::WrongPlan { plan });
        }
        let started = Instant::now();
        let outcome = self
            .backend
            .fetch_range(RangeRequest::whole(remote.final_url.clone()), &mut sink)
            .await?;
        let written = sink.written();
        if outcome.status != 200
            || outcome.content_range.is_some()
            || outcome.truncated
            || outcome.bytes_delivered != written
            || remote
                .total_length
                .is_some_and(|total_length| total_length != written)
        {
            return Err(PoolError::InvalidOutcome {
                worker: WorkerId::new(0),
                reason: "whole response did not match its sink or known representation length",
            });
        }
        sink.sync().await?;
        Ok(PoolReport {
            plan,
            workers: vec![WorkerReport {
                worker: WorkerId::new(0),
                bytes: written,
                elapsed: started.elapsed(),
            }],
        })
    }

    async fn ensure_writer_identity(
        &self,
        writer: &WriterService,
        remote_length: u64,
    ) -> Result<(), PoolError> {
        let snapshot = writer.snapshot().await?;
        let writer_length = snapshot
            .allocator()
            .intervals()
            .last()
            .map_or(0, downpour_intervals::Interval::end);
        if writer_length != remote_length {
            return Err(PoolError::WriterLengthMismatch {
                writer_length,
                remote_length,
            });
        }
        Ok(())
    }
}

/// One segmented execution's live scheduling state.
///
/// The pool owns worker task lifecycle; the allocator owns byte ownership. This type is the
/// boundary between them, and its whole job is to keep the two facts that must agree in step:
/// which workers have a request in flight, and which grants therefore may not be shortened.
struct Scheduler<'a, B> {
    backend: Arc<B>,
    writer: &'a WriterService,
    url: url::Url,
    total_length: u64,
    workers: Vec<WorkerId>,
    tasks: JoinSet<WorkerEvent>,
    running: BTreeMap<WorkerId, RunningWorker>,
    /// Workers holding no grant, in the order they became free.
    idle: VecDeque<WorkerId>,
    /// Workers that asked for work and found none to take. They come back when an abandon puts
    /// bytes back into Pending; until then, waking them would only re-ask the same question.
    parked: BTreeSet<WorkerId>,
    /// The one peer stopped so its segment can be split, until its cancellation is observed.
    stopping: Option<WorkerId>,
    reports: Vec<WorkerReport>,
}

impl<B: TransferProtocol + 'static> Scheduler<'_, B> {
    /// Workers with no request in flight. Only these may have a grant shortened by a split.
    fn quiesced(&self) -> BTreeSet<WorkerId> {
        self.workers
            .iter()
            .copied()
            .filter(|worker| !self.running.contains_key(worker))
            .collect()
    }

    fn spawn(&mut self, grant: Grant) {
        let worker = grant.worker();
        self.idle.retain(|idle| *idle != worker);
        self.parked.remove(&worker);
        spawn_ranged_worker(
            &mut self.tasks,
            &mut self.running,
            Arc::clone(&self.backend),
            grant,
            self.writer,
            self.url.clone(),
            self.total_length,
        );
    }

    fn make_idle(&mut self, worker: WorkerId) {
        self.parked.remove(&worker);
        if !self.idle.contains(&worker) {
            self.idle.push_back(worker);
        }
    }

    fn unpark_everyone(&mut self) {
        for worker in std::mem::take(&mut self.parked) {
            if !self.idle.contains(&worker) {
                self.idle.push_back(worker);
            }
        }
    }

    /// Give every free worker something to do, stopping at most one peer to make room.
    ///
    /// The loop stops as soon as an idle worker cannot be served, because the only way to serve it
    /// is to wait for the peer this call just stopped.
    async fn service_idle_workers(&mut self) -> Result<(), PoolError> {
        while let Some(worker) = self.idle.front().copied() {
            let quiesced = self.quiesced();
            let receipt = self.writer.allocate(worker, quiesced).await?;
            if let Some(allocation) = receipt.allocation() {
                let grant = allocation.grant().clone();
                let shortened = allocation.shortened().cloned();
                self.spawn(grant);
                // A split shortens a quiesced worker's grant. It has no task by construction, so
                // it needs one again for the half it kept — otherwise those bytes have an owner
                // and no fetcher, and the transfer stalls short of complete.
                if let Some(shortened) = shortened {
                    self.spawn(shortened);
                }
                continue;
            }

            // No pending work, and nothing already quiesced is large enough to divide.
            let Some(candidate) = self.writer.split_candidate().await? else {
                self.idle.pop_front();
                self.parked.insert(worker);
                continue;
            };
            if !self.running.contains_key(&candidate) {
                // The allocator declined a quiesced candidate, so nothing here is divisible.
                self.idle.pop_front();
                self.parked.insert(worker);
                continue;
            }
            if self.stopping.is_some() {
                // One peer is already stopping for a split. Stopping a second would cost a
                // connection for work that has not been asked for yet.
                break;
            }
            if let Some(active) = self.running.get_mut(&candidate)
                && let Some(cancel) = active.cancel.take()
            {
                let _stopped_for_a_split = cancel.send(()).is_ok();
            }
            self.stopping = Some(candidate);
            break;
        }
        Ok(())
    }

    /// Record what a stopped attempt actually delivered, at the moment its ownership is fenced.
    ///
    /// Measured here rather than at the end of the transfer because the grant's bytes stop being
    /// attributable to this attempt as soon as the range is split or reclaimed. Nothing inside a
    /// grant is `Complete` when it is issued, so every completed byte inside it now is this
    /// attempt's own.
    async fn record_stopped_attempt(&mut self, attempt: &RunningWorker) -> Result<(), PoolError> {
        self.writer.flush().await?;
        let snapshot = self.writer.snapshot().await?;
        let bytes = completed_bytes_inside(snapshot.allocator(), attempt.grant.range());
        if bytes > 0 {
            self.reports.push(WorkerReport {
                worker: attempt.grant.worker(),
                bytes,
                elapsed: attempt.started.elapsed(),
            });
        }
        Ok(())
    }

    /// Give a task back to every grant whose worker has none. Returns how many were revived.
    fn revive_untasked_grants(
        &mut self,
        snapshot: &crate::writer_service::WriterSnapshot,
    ) -> usize {
        let grants = active_grants(snapshot.allocator());
        let mut revived = 0;
        for grant in grants {
            if !self.running.contains_key(&grant.worker()) {
                self.spawn(grant);
                revived += 1;
            }
        }
        revived
    }

    async fn reclaim_after_failure(&mut self) -> Result<(), PoolError> {
        self.tasks.abort_all();
        while self.tasks.join_next().await.is_some() {}
        self.running.clear();
        for worker in self.workers.clone() {
            self.writer.abandon(worker).await?;
        }
        Ok(())
    }
}

fn spawn_ranged_worker<B: TransferProtocol + 'static>(
    tasks: &mut JoinSet<WorkerEvent>,
    running: &mut BTreeMap<WorkerId, RunningWorker>,
    backend: Arc<B>,
    grant: Grant,
    writer: &WriterService,
    url: url::Url,
    total_length: u64,
) {
    let worker = grant.worker();
    let grant_writer = writer.writer_for(&grant);
    let (cancel, cancelled) = oneshot::channel();
    let started = Instant::now();
    let task_grant = grant.clone();
    tasks.spawn(async move {
        tokio::select! {
            biased;
            result = run_ranged_worker(
                backend,
                worker,
                task_grant,
                grant_writer,
                url,
                total_length,
            ) => WorkerEvent::Finished { worker, result },
            _ = cancelled => WorkerEvent::Cancelled { worker },
        }
    });
    running.insert(
        worker,
        RunningWorker {
            cancel: Some(cancel),
            grant,
            started,
        },
    );
}

fn active_grants(allocator: &crate::SegmentAllocator) -> Vec<Grant> {
    allocator
        .intervals()
        .iter()
        .filter_map(|interval| match interval.state() {
            IntervalState::InProgress { worker } => Some(Grant {
                worker: *worker,
                range: interval.start()..interval.end(),
            }),
            _ => None,
        })
        .collect()
}

fn completed_bytes_inside(
    allocator: &crate::SegmentAllocator,
    grant: &std::ops::Range<u64>,
) -> u64 {
    allocator
        .intervals()
        .iter()
        .filter(|interval| interval.state() == &IntervalState::Complete)
        .map(|interval| {
            interval
                .end()
                .min(grant.end)
                .saturating_sub(interval.start().max(grant.start))
        })
        .sum()
}

fn retryable_worker_failure(error: &PoolError) -> Option<TransientKind> {
    let PoolError::Transfer(source) = error else {
        return None;
    };
    match source.as_ref() {
        TransferError::Transport { .. } => Some(TransientKind::ConnectionReset),
        TransferError::Timeout { .. } => Some(TransientKind::Timeout),
        TransferError::TruncatedBody { .. } => Some(TransientKind::TruncatedBody),
        TransferError::UnexpectedStatus { .. }
        | TransferError::LooksLikeAnErrorPage { .. }
        | TransferError::UnusableRangeResponse { .. }
        | TransferError::OverDelivery { .. }
        | TransferError::Sink { .. }
        | TransferError::ValidatorMismatch { .. } => None,
    }
}

fn single(reason: SingleStreamReason) -> PoolPlan {
    PoolPlan::SingleStream { reason }
}

async fn run_ranged_worker<B: TransferProtocol + 'static>(
    backend: Arc<B>,
    worker: WorkerId,
    grant: Grant,
    grant_writer: GrantWriter,
    url: url::Url,
    total_length: u64,
) -> Result<WorkerReport, PoolError> {
    let started = Instant::now();
    let range = grant.range().clone();
    let expected = range.end - range.start;
    let last = range.end.checked_sub(1).ok_or(PoolError::InvalidOutcome {
        worker,
        reason: "allocator issued an empty range",
    })?;
    let requested = ByteRangeSpec::FromTo {
        first: range.start,
        last,
    };
    let mut sink = RangeSink::new(
        Box::new(GrantTarget::new(grant_writer)),
        range.start,
        Some(expected),
    );
    let outcome = backend
        .fetch_range(RangeRequest::ranged(url, requested), &mut sink)
        .await?;
    let observed = sink.written();
    if outcome.status != 206
        || outcome.protocol != NegotiatedProtocol::Http11
        || outcome.truncated
        || outcome.bytes_delivered != expected
        || observed != expected
        || !content_range_matches(outcome.content_range, &range, total_length)
    {
        return Err(PoolError::InvalidOutcome {
            worker,
            reason: "ranged response did not match its exact allocator grant",
        });
    }
    sink.sync().await?;
    Ok(WorkerReport {
        worker,
        bytes: observed,
        elapsed: started.elapsed(),
    })
}

fn content_range_matches(
    content_range: Option<ContentRange>,
    range: &std::ops::Range<u64>,
    total_length: u64,
) -> bool {
    matches!(
        content_range,
        Some(ContentRange::Bytes {
            first,
            last,
            complete_length: Some(total),
        }) if first == range.start
            && last.checked_add(1) == Some(range.end)
            && total == total_length
    )
}

struct GrantTarget {
    writer: GrantWriter,
}

impl GrantTarget {
    fn new(writer: GrantWriter) -> Self {
        Self { writer }
    }
}

#[async_trait]
impl SinkTarget for GrantTarget {
    async fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), SinkError> {
        if offset != self.writer.next_offset() {
            return Err(SinkError::Io {
                offset,
                source: io::Error::other("range sink did not append at the grant cursor"),
            });
        }
        self.writer
            .write(bytes.to_vec())
            .await
            .map(|_| ())
            .map_err(|error| writer_service_error(offset, error))
    }

    async fn sync(&mut self) -> Result<(), SinkError> {
        self.writer
            .flush()
            .await
            .map(|_| ())
            .map_err(|error| writer_service_error(self.writer.next_offset(), error))
    }
}

fn writer_service_error(offset: u64, error: WriterServiceError) -> SinkError {
    match error {
        WriterServiceError::BeyondGrant {
            start,
            end,
            grant_start,
            grant_end,
        } => SinkError::BeyondGrant {
            grant: grant_end - grant_start,
            already_written: start.saturating_sub(grant_start),
            attempted: end.saturating_sub(start),
        },
        WriterServiceError::Writer(WriterError::NoSpace { offset }) => {
            SinkError::NoSpace { offset }
        }
        other => SinkError::Io {
            offset,
            source: io::Error::other(other.to_string()),
        },
    }
}
