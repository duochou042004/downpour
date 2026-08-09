//! Fixed HTTP/1.1 worker planning and execution.
//!
//! The pool sits above [`downpour_http::TransferProtocol`], so scheduling never names an HTTP
//! client library. It may segment only when the probe produced validated range evidence for a
//! known, locally addressable representation. Every other input is a one-request whole-stream
//! fallback.

use std::sync::Arc;
use std::time::Duration;

use downpour_http::{RangeSink, TransferProtocol};
use downpour_types::RemoteObject;
use thiserror::Error;

use crate::writer_service::WriterService;

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
    /// An empty representation has no non-empty byte range to grant.
    EmptyRepresentation,
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
    /// Deliberate red-proof scaffold: no worker execution exists yet.
    #[error("fixed worker pool is not running")]
    NotRunning,
}

/// Stage 3's configured, non-adaptive HTTP/1.1 worker pool.
pub struct FixedWorkerPool<B> {
    #[allow(
        dead_code,
        reason = "used by the implementation after the red proof commit"
    )]
    backend: Arc<B>,
    #[allow(
        dead_code,
        reason = "used by the implementation after the red proof commit"
    )]
    workers: usize,
}

impl<B: TransferProtocol> FixedWorkerPool<B> {
    /// Bind one reusable protocol backend and a fixed worker ceiling.
    pub fn new(backend: Arc<B>, workers: usize) -> Result<Self, PoolError> {
        if workers == 0 {
            return Err(PoolError::ZeroWorkers);
        }
        Ok(Self { backend, workers })
    }

    /// Select segmented or whole-stream execution from validated probe evidence.
    #[must_use]
    pub fn plan(&self, _remote: &RemoteObject) -> PoolPlan {
        PoolPlan::SingleStream {
            reason: SingleStreamReason::RangeNotProven,
        }
    }

    /// Execute a segmented plan against the sole writer service.
    pub async fn execute_segmented(
        &self,
        _remote: &RemoteObject,
        _writer: &WriterService,
    ) -> Result<PoolReport, PoolError> {
        Err(PoolError::NotRunning)
    }

    /// Execute a whole-representation fallback through exactly one sink.
    pub async fn execute_single(
        &self,
        _remote: &RemoteObject,
        _sink: RangeSink,
    ) -> Result<PoolReport, PoolError> {
        Err(PoolError::NotRunning)
    }
}
