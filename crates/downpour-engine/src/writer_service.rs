//! Bounded asynchronous access to one download's blocking durable state.
//!
//! The service protocol follows ADR-0019. One actor owns the [`SegmentAllocator`] and
//! [`DurableWriter`]; workers receive [`GrantWriter`] handles that append from an allocator-fixed
//! cursor and expose no absolute-offset operation. Blocking file and journal operations stay on
//! the actor's dedicated thread, while the bounded channel provides explicit back-pressure to
//! asynchronous workers.

use std::io;
use std::ops::Range;
use std::time::Duration;

use downpour_intervals::WorkerId;
use downpour_storage::writer::{
    DurableBlock, DurableData, DurableJournal, DurableWriter, WriterError,
};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use crate::{Allocation, AllocatorError, Grant, SegmentAllocator};

/// Default number of writer commands allowed to wait behind the actor.
pub const DEFAULT_WRITER_QUEUE_CAPACITY: usize = 32;

/// Largest owned payload one write command may place in the bounded queue.
pub const MAX_WRITE_BLOCK_BYTES: usize = 256 * 1024;

/// One allocation decision together with blocks committed by its ownership fence.
#[derive(Debug)]
pub struct AllocationReceipt {
    allocation: Option<Allocation>,
    durable: Vec<DurableBlock>,
}

impl AllocationReceipt {
    /// Work granted to the requesting worker, or `None` at the unsplittable tail.
    #[must_use]
    pub const fn allocation(&self) -> Option<&Allocation> {
        self.allocation.as_ref()
    }

    /// Blocks made durable before the allocation changed ownership.
    #[must_use]
    pub fn durable(&self) -> &[DurableBlock] {
        &self.durable
    }
}

/// One accepted sequential write and any blocks it made durable.
#[derive(Debug)]
pub struct WriteReceipt {
    range: Range<u64>,
    durable: Vec<DurableBlock>,
}

impl WriteReceipt {
    /// Exact range accepted under the grant.
    #[must_use]
    pub const fn range(&self) -> &Range<u64> {
        &self.range
    }

    /// Blocks that crossed the journal sync boundary during this command.
    #[must_use]
    pub fn durable(&self) -> &[DurableBlock] {
        &self.durable
    }
}

/// Reclaimed ownership together with blocks committed by the abandon fence.
#[derive(Debug)]
pub struct AbandonReceipt {
    released: u64,
    durable: Vec<DurableBlock>,
}

impl AbandonReceipt {
    /// Bytes returned from this worker to pending state.
    #[must_use]
    pub const fn released(&self) -> u64 {
        self.released
    }

    /// Blocks made durable before ownership was reclaimed.
    #[must_use]
    pub fn durable(&self) -> &[DurableBlock] {
        &self.durable
    }
}

/// Canonical state returned by a snapshot or clean shutdown.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriterSnapshot {
    allocator: SegmentAllocator,
}

impl WriterSnapshot {
    /// Sole allocator state at the actor's serialization point.
    #[must_use]
    pub const fn allocator(&self) -> &SegmentAllocator {
        &self.allocator
    }
}

/// A writer-service command that could not be completed safely.
#[derive(Debug, Error)]
pub enum WriterServiceError {
    /// A zero-capacity channel cannot provide a rendezvous or bounded queue.
    #[error("writer queue capacity must be greater than zero")]
    ZeroQueueCapacity,
    /// A single queued payload exceeded the documented byte bound.
    #[error("write block of {actual} bytes exceeds the {maximum}-byte queue payload limit")]
    BlockTooLarge {
        /// Submitted payload length.
        actual: usize,
        /// Maximum accepted payload length.
        maximum: usize,
    },
    /// Sequential cursor arithmetic overflowed the file-offset domain.
    #[error("grant cursor overflow")]
    CursorOverflow,
    /// The sequential write would cross the grant originally issued to this handle.
    #[error("write [{start}, {end}) exceeds grant [{grant_start}, {grant_end})")]
    BeyondGrant {
        /// Attempted inclusive start.
        start: u64,
        /// Attempted exclusive end.
        end: u64,
        /// Grant inclusive start.
        grant_start: u64,
        /// Grant exclusive end.
        grant_end: u64,
    },
    /// The actor stopped before accepting the command.
    #[error("download-state actor is not running")]
    ActorStopped,
    /// The actor accepted the command but stopped before replying.
    #[error("download-state actor stopped without replying")]
    ReplyDropped,
    /// The operating system refused to create the dedicated state-owner thread.
    #[error("could not start download-state actor: {source}")]
    ActorStart {
        /// Thread-creation failure.
        #[source]
        source: io::Error,
    },
    /// Tokio could not run the blocking thread-join operation to completion.
    #[error("could not join download-state actor: {source}")]
    ActorJoin {
        /// Blocking-task failure.
        #[source]
        source: tokio::task::JoinError,
    },
    /// The dedicated actor thread panicked instead of shutting down cleanly.
    #[error("download-state actor panicked during shutdown")]
    ActorPanicked,
    /// The canonical allocator rejected a control mutation.
    #[error("allocator command failed: {0}")]
    Allocator(#[from] AllocatorError),
    /// The durable writer rejected or failed a storage operation.
    #[error("durable writer command failed: {0}")]
    Writer(#[from] WriterError),
}

type ServiceResult<T> = Result<T, WriterServiceError>;
type Reply<T> = oneshot::Sender<ServiceResult<T>>;

enum Command {
    Allocate {
        worker: WorkerId,
        reply: Reply<AllocationReceipt>,
    },
    Write {
        worker: WorkerId,
        offset: u64,
        bytes: Vec<u8>,
        now: Duration,
        reply: Reply<WriteReceipt>,
    },
    Flush {
        reply: Reply<Vec<DurableBlock>>,
    },
    FlushIfDue {
        now: Duration,
        reply: Reply<Vec<DurableBlock>>,
    },
    Abandon {
        worker: WorkerId,
        reply: Reply<AbandonReceipt>,
    },
    Snapshot {
        reply: Reply<WriterSnapshot>,
    },
    Shutdown {
        reply: Reply<WriterSnapshot>,
    },
}

/// Exclusive control handle for one download-state actor.
pub struct WriterService {
    sender: mpsc::Sender<Command>,
    actor: Option<std::thread::JoinHandle<()>>,
    started_at: tokio::time::Instant,
}

impl WriterService {
    /// Create the bounded service around one allocator and durable writer.
    ///
    /// # Errors
    ///
    /// When `capacity` is zero.
    pub fn start<D, J>(
        writer: DurableWriter<D, J>,
        allocator: SegmentAllocator,
        capacity: usize,
    ) -> ServiceResult<Self>
    where
        D: DurableData + 'static,
        J: DurableJournal + 'static,
    {
        if capacity == 0 {
            return Err(WriterServiceError::ZeroQueueCapacity);
        }
        let (sender, receiver) = mpsc::channel(capacity);
        let actor = std::thread::Builder::new()
            .name("downpour-download-state".to_owned())
            .spawn(move || StateActor::new(writer, allocator).run(receiver))
            .map_err(|source| WriterServiceError::ActorStart { source })?;
        Ok(Self {
            sender,
            actor: Some(actor),
            started_at: tokio::time::Instant::now(),
        })
    }

    /// Number of additional commands the bounded queue can currently accept.
    #[must_use]
    pub fn remaining_capacity(&self) -> usize {
        self.sender.capacity()
    }

    /// Request work for one idle worker.
    pub async fn allocate(&self, worker: WorkerId) -> ServiceResult<AllocationReceipt> {
        let (reply, receiver) = oneshot::channel();
        self.send(Command::Allocate { worker, reply }, receiver)
            .await
    }

    /// Create a sequential worker-facing handle for an allocator-issued grant.
    #[must_use]
    pub fn writer_for(&self, grant: &Grant) -> GrantWriter {
        GrantWriter {
            sender: self.sender.clone(),
            grant: grant.clone(),
            next_offset: grant.range().start,
            started_at: self.started_at,
        }
    }

    /// Force all staged blocks through I-1's durability boundary.
    pub async fn flush(&self) -> ServiceResult<Vec<DurableBlock>> {
        let (reply, receiver) = oneshot::channel();
        self.send(Command::Flush { reply }, receiver).await
    }

    /// Commit a staged batch once its normative age bound has elapsed.
    pub async fn flush_if_due(&self) -> ServiceResult<Vec<DurableBlock>> {
        let (reply, receiver) = oneshot::channel();
        self.send(
            Command::FlushIfDue {
                now: self.started_at.elapsed(),
                reply,
            },
            receiver,
        )
        .await
    }

    /// Durably fence and reclaim every grant owned by `worker`.
    pub async fn abandon(&self, worker: WorkerId) -> ServiceResult<AbandonReceipt> {
        let (reply, receiver) = oneshot::channel();
        self.send(Command::Abandon { worker, reply }, receiver)
            .await
    }

    /// Read the sole canonical allocator state at the actor's serialization point.
    pub async fn snapshot(&self) -> ServiceResult<WriterSnapshot> {
        let (reply, receiver) = oneshot::channel();
        self.send(Command::Snapshot { reply }, receiver).await
    }

    /// Flush, snapshot, and stop the actor.
    pub async fn shutdown(mut self) -> ServiceResult<WriterSnapshot> {
        let (reply, receiver) = oneshot::channel();
        let result = self.send(Command::Shutdown { reply }, receiver).await;
        let actor = self.actor.take().ok_or(WriterServiceError::ActorStopped)?;
        tokio::task::spawn_blocking(move || actor.join())
            .await
            .map_err(|source| WriterServiceError::ActorJoin { source })?
            .map_err(|_| WriterServiceError::ActorPanicked)?;
        result
    }

    async fn send<T>(
        &self,
        command: Command,
        receiver: oneshot::Receiver<ServiceResult<T>>,
    ) -> ServiceResult<T> {
        self.sender
            .send(command)
            .await
            .map_err(|_| WriterServiceError::ActorStopped)?;
        receiver
            .await
            .map_err(|_| WriterServiceError::ReplyDropped)?
    }
}

struct StateActor<D, J> {
    writer: DurableWriter<D, J>,
    allocator: SegmentAllocator,
}

impl<D: DurableData, J: DurableJournal> StateActor<D, J> {
    fn new(writer: DurableWriter<D, J>, allocator: SegmentAllocator) -> Self {
        Self { writer, allocator }
    }

    fn run(mut self, mut receiver: mpsc::Receiver<Command>) {
        while let Some(command) = receiver.blocking_recv() {
            match command {
                Command::Allocate { worker, reply } => {
                    drop(reply.send(self.allocate(worker)));
                }
                Command::Write {
                    worker,
                    offset,
                    bytes,
                    now,
                    reply,
                } => {
                    drop(reply.send(self.write(worker, offset, &bytes, now)));
                }
                Command::Flush { reply } => {
                    drop(reply.send(self.flush()));
                }
                Command::FlushIfDue { now, reply } => {
                    drop(reply.send(self.flush_if_due(now)));
                }
                Command::Abandon { worker, reply } => {
                    drop(reply.send(self.abandon(worker)));
                }
                Command::Snapshot { reply } => {
                    drop(reply.send(Ok(self.snapshot())));
                }
                Command::Shutdown { reply } => {
                    let result = self.shutdown();
                    receiver.close();
                    drop(reply.send(result));
                    break;
                }
            }
        }
    }

    fn allocate(&mut self, worker: WorkerId) -> ServiceResult<AllocationReceipt> {
        let durable = self.flush()?;
        let allocation = self.allocator.allocate(worker)?;
        Ok(AllocationReceipt {
            allocation,
            durable,
        })
    }

    fn write(
        &mut self,
        worker: WorkerId,
        offset: u64,
        bytes: &[u8],
        now: Duration,
    ) -> ServiceResult<WriteReceipt> {
        let length = u64::try_from(bytes.len()).map_err(|_| WriterServiceError::CursorOverflow)?;
        let end = offset
            .checked_add(length)
            .ok_or(WriterServiceError::CursorOverflow)?;
        let range = offset..end;

        // DurableWriter deliberately fires a debug assertion on an ownership breach. An old
        // GrantWriter is ordinary concurrent input to this actor after a split or abandon, so
        // reject it as a value before entering that lower-level invariant tripwire or doing I/O.
        let mut candidate = self.allocator.interval_map_mut().clone();
        candidate
            .complete(range.clone(), worker)
            .map_err(WriterError::Interval)?;

        let durable = self.writer.stage(
            self.allocator.interval_map_mut(),
            worker,
            offset,
            bytes,
            now,
        )?;
        Ok(WriteReceipt { range, durable })
    }

    fn flush(&mut self) -> ServiceResult<Vec<DurableBlock>> {
        self.writer
            .flush(self.allocator.interval_map_mut())
            .map_err(WriterServiceError::from)
    }

    fn flush_if_due(&mut self, now: Duration) -> ServiceResult<Vec<DurableBlock>> {
        self.writer
            .flush_if_due(self.allocator.interval_map_mut(), now)
            .map_err(WriterServiceError::from)
    }

    fn abandon(&mut self, worker: WorkerId) -> ServiceResult<AbandonReceipt> {
        let durable = self.flush()?;
        let released = self.allocator.abandon(worker);
        Ok(AbandonReceipt { released, durable })
    }

    fn snapshot(&self) -> WriterSnapshot {
        WriterSnapshot {
            allocator: self.allocator.clone(),
        }
    }

    fn shutdown(&mut self) -> ServiceResult<WriterSnapshot> {
        self.flush()?;
        Ok(self.snapshot())
    }
}

/// Worker-facing sequential cursor over one allocator-issued grant.
///
/// There is deliberately no absolute-offset method. A worker supplies bytes; this type decides
/// where they land and the actor revalidates that range against current ownership before I/O.
pub struct GrantWriter {
    sender: mpsc::Sender<Command>,
    grant: Grant,
    next_offset: u64,
    started_at: tokio::time::Instant,
}

impl GrantWriter {
    /// Offset at which the next accepted byte will land.
    #[must_use]
    pub const fn next_offset(&self) -> u64 {
        self.next_offset
    }

    /// Append one owned payload at the current grant cursor.
    pub async fn write(&mut self, bytes: Vec<u8>) -> ServiceResult<WriteReceipt> {
        if bytes.len() > MAX_WRITE_BLOCK_BYTES {
            return Err(WriterServiceError::BlockTooLarge {
                actual: bytes.len(),
                maximum: MAX_WRITE_BLOCK_BYTES,
            });
        }
        let length = u64::try_from(bytes.len()).map_err(|_| WriterServiceError::CursorOverflow)?;
        let end = self
            .next_offset
            .checked_add(length)
            .ok_or(WriterServiceError::CursorOverflow)?;
        if end > self.grant.range().end {
            return Err(WriterServiceError::BeyondGrant {
                start: self.next_offset,
                end,
                grant_start: self.grant.range().start,
                grant_end: self.grant.range().end,
            });
        }

        let (reply, receiver) = oneshot::channel();
        self.sender
            .send(Command::Write {
                worker: self.grant.worker(),
                offset: self.next_offset,
                bytes,
                now: self.started_at.elapsed(),
                reply,
            })
            .await
            .map_err(|_| WriterServiceError::ActorStopped)?;
        let receipt = receiver
            .await
            .map_err(|_| WriterServiceError::ReplyDropped)??;
        self.next_offset = end;
        Ok(receipt)
    }

    /// Force all currently staged blocks for this download through the durability boundary.
    pub async fn flush(&self) -> ServiceResult<Vec<DurableBlock>> {
        let (reply, receiver) = oneshot::channel();
        self.sender
            .send(Command::Flush { reply })
            .await
            .map_err(|_| WriterServiceError::ActorStopped)?;
        receiver
            .await
            .map_err(|_| WriterServiceError::ReplyDropped)?
    }
}
