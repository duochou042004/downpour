//! Behavioral proofs for the bounded, single-owner download-state service.

use std::io;
use std::sync::{Arc, Condvar, Mutex};

use downpour_engine::writer_service::{
    GrantWriter, MAX_WRITE_BLOCK_BYTES, WriterService, WriterServiceError,
};
use downpour_engine::{Allocation, Grant, SegmentAllocator};
use downpour_intervals::{IntervalState, WorkerId};
use downpour_storage::journal::FramedRecord;
use downpour_storage::writer::{DurableData, DurableJournal, DurableWriter, WriterError};

#[derive(Clone, Debug, Eq, PartialEq)]
enum Event {
    Write { offset: u64, bytes: Vec<u8> },
    DataSync,
    JournalAppend { sequence: u64 },
    JournalSync,
}

#[derive(Debug)]
struct FakeState {
    events: Vec<Event>,
    fail_write: bool,
}

#[derive(Debug)]
struct WriteGateState {
    enabled: bool,
    entered: bool,
    released: bool,
}

#[derive(Debug)]
struct WriteGate {
    state: Mutex<WriteGateState>,
    changed: Condvar,
}

impl WriteGate {
    fn new() -> Self {
        Self {
            state: Mutex::new(WriteGateState {
                enabled: false,
                entered: false,
                released: false,
            }),
            changed: Condvar::new(),
        }
    }

    fn enable(&self) {
        let mut state = self.state.lock().unwrap();
        state.enabled = true;
        state.entered = false;
        state.released = false;
    }

    fn block_if_enabled(&self) {
        let mut state = self.state.lock().unwrap();
        if !state.enabled {
            return;
        }
        state.entered = true;
        self.changed.notify_all();
        while !state.released {
            state = self.changed.wait(state).unwrap();
        }
        state.enabled = false;
    }

    fn wait_until_entered(&self) {
        let mut state = self.state.lock().unwrap();
        while !state.entered {
            state = self.changed.wait(state).unwrap();
        }
    }

    fn release(&self) {
        let mut state = self.state.lock().unwrap();
        state.released = true;
        self.changed.notify_all();
    }
}

#[derive(Clone, Debug)]
struct FakeData {
    total_length: u64,
    state: Arc<Mutex<FakeState>>,
    gate: Arc<WriteGate>,
}

#[derive(Clone, Debug)]
struct FakeJournal {
    total_length: u64,
    state: Arc<Mutex<FakeState>>,
}

impl DurableData for FakeData {
    fn total_length(&self) -> u64 {
        self.total_length
    }

    fn write_all_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), WriterError> {
        {
            let mut state = self.state.lock().unwrap();
            state.events.push(Event::Write {
                offset,
                bytes: bytes.to_vec(),
            });
            if state.fail_write {
                return Err(injected_error("write fake part file"));
            }
        }
        self.gate.block_if_enabled();
        Ok(())
    }

    fn sync_data(&mut self) -> Result<(), WriterError> {
        self.state.lock().unwrap().events.push(Event::DataSync);
        Ok(())
    }
}

impl DurableJournal for FakeJournal {
    fn total_length(&self) -> u64 {
        self.total_length
    }

    fn append(&mut self, record: &FramedRecord) -> Result<(), WriterError> {
        self.state
            .lock()
            .unwrap()
            .events
            .push(Event::JournalAppend {
                sequence: record.sequence(),
            });
        Ok(())
    }

    fn sync_data(&mut self) -> Result<(), WriterError> {
        self.state.lock().unwrap().events.push(Event::JournalSync);
        Ok(())
    }
}

fn injected_error(operation: &'static str) -> WriterError {
    WriterError::Io {
        operation,
        source: io::Error::other("injected writer-service failure"),
    }
}

fn service(
    total_length: u64,
    minimum_split: u64,
    capacity: usize,
) -> (WriterService, Arc<Mutex<FakeState>>, Arc<WriteGate>) {
    let state = Arc::new(Mutex::new(FakeState {
        events: Vec::new(),
        fail_write: false,
    }));
    let gate = Arc::new(WriteGate::new());
    let data = FakeData {
        total_length,
        state: Arc::clone(&state),
        gate: Arc::clone(&gate),
    };
    let journal = FakeJournal {
        total_length,
        state: Arc::clone(&state),
    };
    let writer = DurableWriter::try_new(data, journal, 0).unwrap();
    let allocator = SegmentAllocator::new(total_length, minimum_split).unwrap();
    let service = WriterService::start(writer, allocator, capacity).unwrap();
    (service, state, gate)
}

fn allocation(receipt: &downpour_engine::writer_service::AllocationReceipt) -> &Allocation {
    receipt.allocation().expect("worker should receive a grant")
}

fn grant(receipt: &downpour_engine::writer_service::AllocationReceipt) -> Grant {
    allocation(receipt).grant().clone()
}

fn assert_interval(
    snapshot: &downpour_engine::writer_service::WriterSnapshot,
    index: usize,
    start: u64,
    end: u64,
    expected: &IntervalState,
) {
    let interval = &snapshot.allocator().intervals()[index];
    assert_eq!((interval.start(), interval.end()), (start, end));
    assert_eq!(interval.state(), expected);
}

#[tokio::test]
async fn receipts_claim_completion_only_after_the_journal_sync_boundary() {
    let worker = WorkerId::new(1);
    let (service, state, _) = service(8, 1, 4);
    let assigned = service.allocate(worker).await.unwrap();
    assert!(assigned.durable().is_empty());
    let mut writer = service.writer_for(&grant(&assigned));

    let staged = writer.write(b"safe".to_vec()).await.unwrap();
    assert_eq!(staged.range(), &(0..4));
    assert!(staged.durable().is_empty());
    assert_eq!(
        state.lock().unwrap().events,
        vec![Event::Write {
            offset: 0,
            bytes: b"safe".to_vec(),
        }]
    );

    let durable = service.flush().await.unwrap();
    assert_eq!(durable.len(), 1);
    assert_eq!(durable[0].range(), &(0..4));
    assert_eq!(
        state.lock().unwrap().events,
        vec![
            Event::Write {
                offset: 0,
                bytes: b"safe".to_vec(),
            },
            Event::DataSync,
            Event::JournalAppend { sequence: 0 },
            Event::JournalSync,
        ]
    );
    let snapshot = service.snapshot().await.unwrap();
    assert_interval(&snapshot, 0, 0, 4, &IntervalState::Complete);
    assert_interval(&snapshot, 1, 4, 8, &IntervalState::InProgress { worker });
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn split_is_a_durability_fence_and_stale_bytes_never_reach_storage() {
    let first = WorkerId::new(11);
    let second = WorkerId::new(12);
    let (service, state, _) = service(64, 16, 4);
    let first_assignment = service.allocate(first).await.unwrap();
    let mut stale_writer = service.writer_for(&grant(&first_assignment));
    stale_writer.write(vec![0xA1; 8]).await.unwrap();

    let second_assignment = service.allocate(second).await.unwrap();
    assert_eq!(second_assignment.durable().len(), 1);
    assert_eq!(second_assignment.durable()[0].range(), &(0..8));
    assert_eq!(allocation(&second_assignment).grant().range(), &(36..64));
    assert_eq!(
        allocation(&second_assignment)
            .shortened()
            .expect("split must report the shortened source")
            .range(),
        &(8..36)
    );
    assert_eq!(
        state.lock().unwrap().events,
        vec![
            Event::Write {
                offset: 0,
                bytes: vec![0xA1; 8],
            },
            Event::DataSync,
            Event::JournalAppend { sequence: 0 },
            Event::JournalSync,
        ]
    );

    let error = stale_writer.write(vec![0xB2; 32]).await.unwrap_err();
    assert!(matches!(
        error,
        WriterServiceError::Writer(WriterError::Interval(_))
    ));
    assert_eq!(state.lock().unwrap().events.len(), 4);

    let snapshot = service.snapshot().await.unwrap();
    assert_interval(&snapshot, 0, 0, 8, &IntervalState::Complete);
    assert_interval(
        &snapshot,
        1,
        8,
        36,
        &IntervalState::InProgress { worker: first },
    );
    assert_interval(
        &snapshot,
        2,
        36,
        64,
        &IntervalState::InProgress { worker: second },
    );
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn abandon_flushes_staged_bytes_before_reclaiming_only_the_remainder() {
    let worker = WorkerId::new(21);
    let (service, state, _) = service(16, 1, 4);
    let assigned = service.allocate(worker).await.unwrap();
    let mut writer = service.writer_for(&grant(&assigned));
    writer.write(b"kept".to_vec()).await.unwrap();

    let abandoned = service.abandon(worker).await.unwrap();
    assert_eq!(abandoned.released(), 12);
    assert_eq!(abandoned.durable().len(), 1);
    assert_eq!(abandoned.durable()[0].range(), &(0..4));
    assert_eq!(
        state.lock().unwrap().events,
        vec![
            Event::Write {
                offset: 0,
                bytes: b"kept".to_vec(),
            },
            Event::DataSync,
            Event::JournalAppend { sequence: 0 },
            Event::JournalSync,
        ]
    );

    let stale_error = writer.write(b"lost".to_vec()).await.unwrap_err();
    assert!(matches!(
        stale_error,
        WriterServiceError::Writer(WriterError::Interval(_))
    ));
    assert_eq!(state.lock().unwrap().events.len(), 4);
    let snapshot = service.snapshot().await.unwrap();
    assert_interval(&snapshot, 0, 0, 4, &IntervalState::Complete);
    assert_interval(&snapshot, 1, 4, 16, &IntervalState::Pending);
    service.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_owned_command_queue_reaches_its_exact_configured_bound() {
    let first = WorkerId::new(31);
    let second = WorkerId::new(32);
    let (service, _, gate) = service(64, 16, 1);
    let first_assignment = service.allocate(first).await.unwrap();
    let second_assignment = service.allocate(second).await.unwrap();
    let mut first_writer = service.writer_for(&grant(&first_assignment));
    let mut second_writer = service.writer_for(&grant(&second_assignment));
    gate.enable();

    let first_write = tokio::spawn(async move { first_writer.write(vec![0x31; 4]).await });
    let wait_gate = Arc::clone(&gate);
    tokio::task::spawn_blocking(move || wait_gate.wait_until_entered())
        .await
        .unwrap();
    let second_write = tokio::spawn(async move { second_writer.write(vec![0x32; 4]).await });

    for _ in 0..100 {
        if service.remaining_capacity() == 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        service.remaining_capacity(),
        0,
        "one waiting command must fill the queue"
    );
    assert!(
        !first_write.is_finished(),
        "the actor is still inside the first write"
    );
    assert!(
        !second_write.is_finished(),
        "the queued write cannot have a receipt yet"
    );

    gate.release();
    assert_eq!(first_write.await.unwrap().unwrap().range(), &(0..4));
    assert_eq!(second_write.await.unwrap().unwrap().range(), &(32..36));
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn worker_handles_advance_sequentially_and_payload_limits_fail_before_io() {
    let worker = WorkerId::new(41);
    let (service, state, _) = service(16, 1, 2);
    let assigned = service.allocate(worker).await.unwrap();
    let mut writer: GrantWriter = service.writer_for(&grant(&assigned));
    assert_eq!(writer.next_offset(), 0);

    assert_eq!(
        writer.write(b"abc".to_vec()).await.unwrap().range(),
        &(0..3)
    );
    assert_eq!(writer.next_offset(), 3);
    assert_eq!(writer.write(b"de".to_vec()).await.unwrap().range(), &(3..5));
    assert_eq!(writer.next_offset(), 5);
    let events_before_rejection = state.lock().unwrap().events.clone();

    let error = writer
        .write(vec![0; MAX_WRITE_BLOCK_BYTES + 1])
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        WriterServiceError::BlockTooLarge {
            actual,
            maximum: MAX_WRITE_BLOCK_BYTES,
        } if actual == MAX_WRITE_BLOCK_BYTES + 1
    ));
    assert_eq!(state.lock().unwrap().events, events_before_rejection);
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_flushes_before_exit_and_cloned_worker_handles_observe_the_stop() {
    let worker = WorkerId::new(51);
    let (service, _, _) = service(8, 1, 2);
    let assigned = service.allocate(worker).await.unwrap();
    let mut writer = service.writer_for(&grant(&assigned));
    writer.write(b"done".to_vec()).await.unwrap();

    let snapshot = service.shutdown().await.unwrap();
    assert_interval(&snapshot, 0, 0, 4, &IntervalState::Complete);
    let error = writer.write(b"nope".to_vec()).await.unwrap_err();
    assert!(matches!(error, WriterServiceError::ActorStopped));
}

#[tokio::test]
async fn backend_failures_are_returned_and_zero_capacity_is_refused() {
    let total_length = 4;
    let state = Arc::new(Mutex::new(FakeState {
        events: Vec::new(),
        fail_write: true,
    }));
    let gate = Arc::new(WriteGate::new());
    let data = FakeData {
        total_length,
        state: Arc::clone(&state),
        gate,
    };
    let journal = FakeJournal {
        total_length,
        state: Arc::clone(&state),
    };
    let writer = DurableWriter::try_new(data, journal, 0).unwrap();
    let allocator = SegmentAllocator::new(total_length, 1).unwrap();
    let service = WriterService::start(writer, allocator, 1).unwrap();
    let worker = WorkerId::new(61);
    let assigned = service.allocate(worker).await.unwrap();
    let mut worker_writer = service.writer_for(&grant(&assigned));

    let error = worker_writer.write(b"fail".to_vec()).await.unwrap_err();
    assert!(matches!(
        error,
        WriterServiceError::Writer(WriterError::Io {
            operation: "write fake part file",
            ..
        })
    ));
    assert_eq!(
        state.lock().unwrap().events,
        vec![Event::Write {
            offset: 0,
            bytes: b"fail".to_vec(),
        }]
    );

    let state = Arc::new(Mutex::new(FakeState {
        events: Vec::new(),
        fail_write: false,
    }));
    let data = FakeData {
        total_length,
        state: Arc::clone(&state),
        gate: Arc::new(WriteGate::new()),
    };
    let journal = FakeJournal {
        total_length,
        state,
    };
    let writer = DurableWriter::try_new(data, journal, 0).unwrap();
    let allocator = SegmentAllocator::new(total_length, 1).unwrap();
    assert!(matches!(
        WriterService::start(writer, allocator, 0),
        Err(WriterServiceError::ZeroQueueCapacity)
    ));
}
