//! Fixed HTTP/1.1 worker planning and execution.
//!
//! The pool sits above [`downpour_http::TransferProtocol`], so scheduling never names an HTTP
//! client library. It may segment only when the probe produced validated range evidence for a
//! known, locally addressable representation. Every other input is a one-request whole-stream
//! fallback.

use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use downpour_http::{
    RangeRequest, RangeSink, SinkError, SinkTarget, TransferError, TransferProtocol,
};
use downpour_intervals::{IntervalState, WorkerId};
use downpour_storage::writer::{JOURNAL_FLUSH_INTERVAL, WriterError};
use downpour_types::{ByteRangeSpec, ContentRange, NegotiatedProtocol, RemoteObject};
use thiserror::Error;
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

        let mut grants = BTreeMap::new();
        for index in 0..workers {
            let raw = u64::try_from(index).map_err(|_| PoolError::WorkerIdOverflow { index })?;
            let worker = WorkerId::new(raw);
            let receipt = writer.allocate(worker).await?;
            let Some(allocation) = receipt.allocation() else {
                break;
            };
            if let Some(shortened) = allocation.shortened() {
                grants.insert(shortened.worker(), shortened.clone());
            }
            grants.insert(worker, allocation.grant().clone());
        }
        if grants.is_empty() {
            return Err(PoolError::IncompleteCoverage);
        }

        let allocated_workers = grants.keys().copied().collect::<Vec<_>>();
        let mut tasks = JoinSet::new();
        for (worker, grant) in grants {
            let backend = Arc::clone(&self.backend);
            let url = remote.final_url.clone();
            let grant_writer = writer.writer_for(&grant);
            tasks.spawn(run_ranged_worker(
                backend,
                worker,
                grant,
                grant_writer,
                url,
                total_length,
            ));
        }

        let mut ticker = tokio::time::interval(JOURNAL_FLUSH_INTERVAL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        ticker.tick().await;
        let mut reports = Vec::with_capacity(allocated_workers.len());
        while !tasks.is_empty() {
            tokio::select! {
                joined = tasks.join_next() => {
                    let Some(joined) = joined else {
                        self.reclaim_after_failure(&mut tasks, writer, &allocated_workers).await?;
                        return Err(PoolError::IncompleteCoverage);
                    };
                    match joined {
                        Ok(Ok(report)) => reports.push(report),
                        Ok(Err(error)) => {
                            self.reclaim_after_failure(&mut tasks, writer, &allocated_workers).await?;
                            return Err(error);
                        }
                        Err(source) => {
                            self.reclaim_after_failure(&mut tasks, writer, &allocated_workers).await?;
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
        reports.sort_by_key(|report| report.worker());
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

    async fn reclaim_after_failure(
        &self,
        tasks: &mut JoinSet<Result<WorkerReport, PoolError>>,
        writer: &WriterService,
        workers: &[WorkerId],
    ) -> Result<(), PoolError> {
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        for worker in workers {
            writer.abandon(*worker).await?;
        }
        Ok(())
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
