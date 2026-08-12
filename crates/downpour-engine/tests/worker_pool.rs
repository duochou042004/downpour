//! Behavioral proofs for Stage 3's fixed HTTP/1.1 worker pool and fallback boundary.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;
use downpour_corpus::content::Content;
use downpour_corpus::server::{PathologyServer, RecordedRequest, ServerSpec};
use downpour_engine::SegmentAllocator;
use downpour_engine::worker_pool::{FixedWorkerPool, PoolError, PoolPlan, SingleStreamReason};
use downpour_engine::writer_service::WriterService;
use downpour_http::{
    BackendCapabilities, H1H2Backend, ProbeError, ProbeRequest, RangeOutcome, RangeRequest,
    RangeSink, SinkError, SinkTarget, TransferError, TransferProtocol, TransportMode,
};
use downpour_intervals::{IntervalState, WorkerId};
use downpour_storage::journal::FramedRecord;
use downpour_storage::writer::{DurableData, DurableJournal, DurableWriter, WriterError};
use downpour_types::{
    ByteRangeSpec, ContentRange, NegotiatedProtocol, RangeProof, RangeSupport, RemoteObject,
    Validator,
};
use tokio::sync::{Barrier, Semaphore};

#[derive(Clone, Debug, Eq, PartialEq)]
struct ObservedRequest {
    range: Option<ByteRangeSpec>,
    if_range: Option<String>,
}

#[derive(Debug)]
struct ProtocolState {
    requests: Vec<ObservedRequest>,
    active: usize,
    maximum_active: usize,
}

/// Decrements on drop, so a request the pool *cancels* stops counting as active immediately.
/// Decrementing at the end of `fetch_range` instead would leave every cancelled request active
/// forever and inflate `maximum_active` above the configured worker ceiling.
struct ActiveRequest {
    state: Arc<Mutex<ProtocolState>>,
}

impl ActiveRequest {
    fn enter(state: &Arc<Mutex<ProtocolState>>, request: ObservedRequest) -> Self {
        {
            let mut held = state.lock().unwrap();
            held.requests.push(request);
            held.active += 1;
            let active = held.active;
            held.maximum_active = held.maximum_active.max(active);
        }
        Self {
            state: Arc::clone(state),
        }
    }
}

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        self.state.lock().unwrap().active -= 1;
    }
}

#[derive(Clone, Debug)]
struct FakeProtocol {
    remote: RemoteObject,
    body: Arc<Vec<u8>>,
    capabilities: BackendCapabilities,
    state: Arc<Mutex<ProtocolState>>,
    rendezvous: Option<Arc<Barrier>>,
    pause: Option<Arc<TransferPause>>,
}

#[derive(Debug)]
struct TransferPause {
    first: u64,
    prefix: usize,
    accepted: Semaphore,
    release: Semaphore,
}

impl FakeProtocol {
    fn new(remote: RemoteObject, body: Vec<u8>) -> Self {
        Self {
            remote,
            body: Arc::new(body),
            capabilities: BackendCapabilities {
                name: "fake-h1",
                protocols: vec![NegotiatedProtocol::Http11],
                multiplexes_streams: false,
                supports_ranges: true,
            },
            state: Arc::new(Mutex::new(ProtocolState {
                requests: Vec::new(),
                active: 0,
                maximum_active: 0,
            })),
            rendezvous: None,
            pause: None,
        }
    }

    fn with_rendezvous(mut self, workers: usize) -> Self {
        self.rendezvous = Some(Arc::new(Barrier::new(workers)));
        self
    }

    fn without_range_capability(mut self) -> Self {
        self.capabilities.supports_ranges = false;
        self
    }

    fn with_mid_transfer_pause(mut self, pause: Arc<TransferPause>) -> Self {
        self.pause = Some(pause);
        self
    }

    fn observed(&self) -> (Vec<ObservedRequest>, usize) {
        let state = self.state.lock().unwrap();
        (state.requests.clone(), state.maximum_active)
    }
}

#[async_trait]
impl TransferProtocol for FakeProtocol {
    async fn probe(&self, _request: ProbeRequest) -> Result<RemoteObject, ProbeError> {
        Ok(self.remote.clone())
    }

    async fn fetch_range(
        &self,
        request: RangeRequest,
        sink: &mut RangeSink,
    ) -> Result<RangeOutcome, TransferError> {
        let _active = ActiveRequest::enter(
            &self.state,
            ObservedRequest {
                range: request.range,
                if_range: request.if_range.clone(),
            },
        );
        let rendezvous = self.rendezvous.clone();
        if request.range.is_some()
            && let Some(rendezvous) = rendezvous
        {
            rendezvous.wait().await;
        }

        let (start, end) = requested_window(request.range, self.body.len());
        let bytes = &self.body[start..end];
        let accepted = if let Some(pause) = &self.pause
            && request.range.is_some_and(|range| match range {
                ByteRangeSpec::FromTo { first, .. } | ByteRangeSpec::From { first } => {
                    first == pause.first
                }
                ByteRangeSpec::Suffix { .. } => false,
            }) {
            let middle = pause.prefix.min(bytes.len());
            sink.accept(&bytes[..middle])
                .await
                .map_err(|source| TransferError::Sink {
                    url: request.url.clone(),
                    source,
                })?;
            pause.accepted.add_permits(1);
            let permit =
                pause
                    .release
                    .acquire()
                    .await
                    .map_err(|source| TransferError::Transport {
                        url: request.url.clone(),
                        source: Box::new(source),
                    })?;
            permit.forget();
            sink.accept(&bytes[middle..]).await
        } else {
            sink.accept(bytes).await
        };
        accepted.map_err(|source| TransferError::Sink {
            url: request.url.clone(),
            source,
        })?;

        let bytes_delivered = u64::try_from(bytes.len()).unwrap();
        let content_range = request.range.map(|_| ContentRange::Bytes {
            first: u64::try_from(start).unwrap(),
            last: u64::try_from(end - 1).unwrap(),
            complete_length: Some(u64::try_from(self.body.len()).unwrap()),
        });
        Ok(RangeOutcome {
            bytes_delivered,
            status: if request.range.is_some() { 206 } else { 200 },
            content_range,
            protocol: NegotiatedProtocol::Http11,
            truncated: false,
        })
    }

    fn capabilities(&self) -> BackendCapabilities {
        self.capabilities.clone()
    }
}

fn requested_window(range: Option<ByteRangeSpec>, total: usize) -> (usize, usize) {
    match range {
        Some(ByteRangeSpec::FromTo { first, last }) => (
            usize::try_from(first).unwrap(),
            usize::try_from(last).unwrap() + 1,
        ),
        Some(ByteRangeSpec::From { first }) => (usize::try_from(first).unwrap(), total),
        Some(ByteRangeSpec::Suffix { len }) => (total - usize::try_from(len).unwrap(), total),
        None => (0, total),
    }
}

fn proven_remote(total_length: u64, protocol: NegotiatedProtocol) -> RemoteObject {
    let proof = RangeProof::from_observed_response(
        ByteRangeSpec::FromTo { first: 0, last: 0 },
        206,
        Some(&format!("bytes 0-0/{total_length}")),
        None,
        1,
    )
    .unwrap();
    remote(Some(total_length), RangeSupport::Proven(proof), protocol)
}

fn remote(
    total_length: Option<u64>,
    range_support: RangeSupport,
    protocol: NegotiatedProtocol,
) -> RemoteObject {
    let final_url = ProbeRequest::new("https://example.test/file.bin".parse().unwrap()).url;
    RemoteObject {
        final_url: final_url.clone(),
        redirect_chain: vec![final_url],
        total_length,
        range_support,
        validator: Validator::StrongETag("\"v1\"".to_owned()),
        digest: None,
        protocol,
        suggested_filename: Some("file.bin".to_owned()),
        content_type: None,
        probed_at: SystemTime::now(),
    }
}

#[derive(Clone, Debug)]
struct MemoryData {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl DurableData for MemoryData {
    fn total_length(&self) -> u64 {
        u64::try_from(self.bytes.lock().unwrap().len()).unwrap()
    }

    fn write_all_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), WriterError> {
        let start = usize::try_from(offset).unwrap();
        let end = start + bytes.len();
        self.bytes.lock().unwrap()[start..end].copy_from_slice(bytes);
        Ok(())
    }

    fn sync_data(&mut self) -> Result<(), WriterError> {
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct MemoryJournal {
    total_length: u64,
    synced: Arc<Semaphore>,
}

impl DurableJournal for MemoryJournal {
    fn total_length(&self) -> u64 {
        self.total_length
    }

    fn append(&mut self, _record: &FramedRecord) -> Result<(), WriterError> {
        Ok(())
    }

    fn sync_data(&mut self) -> Result<(), WriterError> {
        self.synced.add_permits(1);
        Ok(())
    }
}

fn writer_service(
    total_length: u64,
    minimum_split: u64,
) -> (WriterService, Arc<Mutex<Vec<u8>>>, Arc<Semaphore>) {
    let bytes = Arc::new(Mutex::new(vec![0; usize::try_from(total_length).unwrap()]));
    let synced = Arc::new(Semaphore::new(0));
    let data = MemoryData {
        bytes: Arc::clone(&bytes),
    };
    let journal = MemoryJournal {
        total_length,
        synced: Arc::clone(&synced),
    };
    let writer = DurableWriter::try_new(data, journal, 0).unwrap();
    let allocator = SegmentAllocator::new(total_length, minimum_split).unwrap();
    (
        WriterService::start(writer, allocator, 8).unwrap(),
        bytes,
        synced,
    )
}

#[derive(Debug)]
struct RecordingTarget {
    bytes: Arc<Mutex<Vec<u8>>>,
    syncs: Arc<AtomicUsize>,
}

#[async_trait]
impl SinkTarget for RecordingTarget {
    async fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), SinkError> {
        let start = usize::try_from(offset).map_err(|_| SinkError::LengthOverflow)?;
        let end = start
            .checked_add(bytes.len())
            .ok_or(SinkError::LengthOverflow)?;
        let mut recorded = self.bytes.lock().unwrap();
        if recorded.len() < end {
            recorded.resize(end, 0);
        }
        recorded[start..end].copy_from_slice(bytes);
        Ok(())
    }

    async fn sync(&mut self) -> Result<(), SinkError> {
        self.syncs.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn segmentation_is_selected_only_from_proven_addressable_http11_evidence() {
    let valid = proven_remote(64, NegotiatedProtocol::Http11);
    let backend = Arc::new(FakeProtocol::new(valid.clone(), vec![0; 64]));
    let pool = FixedWorkerPool::new(Arc::clone(&backend), 4).unwrap();
    assert_eq!(
        pool.plan(&valid),
        PoolPlan::Segmented {
            total_length: 64,
            workers: 4,
        }
    );

    let cases = [
        (
            remote(Some(64), RangeSupport::Absent, NegotiatedProtocol::Http11),
            SingleStreamReason::RangeNotProven,
        ),
        (
            remote(Some(64), RangeSupport::Unknown, NegotiatedProtocol::Http11),
            SingleStreamReason::RangeNotProven,
        ),
        (
            {
                let mut value = valid.clone();
                value.total_length = None;
                value
            },
            SingleStreamReason::LengthUnknown,
        ),
        (
            {
                let mut value = valid.clone();
                value.total_length = Some(65);
                value
            },
            SingleStreamReason::InconsistentLength,
        ),
        (
            proven_remote(i64::MAX as u64 + 1, NegotiatedProtocol::Http11),
            SingleStreamReason::LengthUnaddressable,
        ),
        (
            proven_remote(64, NegotiatedProtocol::Http2),
            SingleStreamReason::ProtocolNotHttp11,
        ),
    ];
    for (remote, reason) in cases {
        assert_eq!(pool.plan(&remote), PoolPlan::SingleStream { reason });
    }

    let no_ranges =
        Arc::new(FakeProtocol::new(valid.clone(), vec![0; 64]).without_range_capability());
    let pool = FixedWorkerPool::new(no_ranges, 4).unwrap();
    assert_eq!(
        pool.plan(&valid),
        PoolPlan::SingleStream {
            reason: SingleStreamReason::BackendCannotRange,
        }
    );
    assert!(matches!(
        FixedWorkerPool::new(backend, 0),
        Err(PoolError::ZeroWorkers)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn proven_ranges_run_concurrently_inside_disjoint_allocator_grants() {
    let body = (0_u8..64).collect::<Vec<_>>();
    let remote = proven_remote(64, NegotiatedProtocol::Http11);
    let backend = Arc::new(FakeProtocol::new(remote.clone(), body.clone()).with_rendezvous(4));
    let pool = FixedWorkerPool::new(Arc::clone(&backend), 4).unwrap();
    let (writer, stored, _) = writer_service(64, 16);

    let report = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        pool.execute_segmented(&remote, &writer),
    )
    .await
    .expect("all four ranged workers must reach the rendezvous")
    .unwrap();
    assert_eq!(report.plan(), pool.plan(&remote));
    assert_eq!(report.workers().len(), 4);
    assert_eq!(
        report
            .workers()
            .iter()
            .map(|worker| worker.bytes())
            .sum::<u64>(),
        64
    );

    let (mut requests, maximum_active) = backend.observed();
    requests.sort_by_key(|request| match request.range {
        Some(ByteRangeSpec::FromTo { first, .. }) => first,
        _ => u64::MAX,
    });
    assert_eq!(maximum_active, 4);
    assert_eq!(
        requests,
        vec![
            observed_range(0, 15),
            observed_range(16, 31),
            observed_range(32, 47),
            observed_range(48, 63),
        ]
    );
    assert_eq!(*stored.lock().unwrap(), body);

    let snapshot = writer.shutdown().await.unwrap();
    assert_eq!(snapshot.allocator().intervals().len(), 1);
    assert_eq!(
        snapshot.allocator().intervals()[0].state(),
        &downpour_intervals::IntervalState::Complete
    );
}

/// Every ranged worker request carries `If-Range` when the probe recorded a usable validator.
///
/// I-3 across a process exit: a segmented resume writes into the holes of a file whose other bytes
/// came from an earlier representation. Without `If-Range` the server happily serves the ranges of
/// whatever it holds *now*, and the finished file is half one version and half another at exactly
/// the expected size — the one corruption every check that does not hash the content passes.
///
/// Sent on fresh transfers too, not only resumes. The cost is one header; what it buys is that a
/// representation swapped between the probe and a worker's request turns into a `200` the pool
/// already refuses, instead of bytes written at an offset that no longer means anything.
fn observed_range(first: u64, last: u64) -> ObservedRequest {
    ObservedRequest {
        range: Some(ByteRangeSpec::FromTo { first, last }),
        if_range: Some("\"v1\"".to_owned()),
    }
}

#[tokio::test]
async fn absent_range_support_executes_one_whole_request_and_one_sync() {
    let body = b"one honest stream".to_vec();
    let remote = remote(
        Some(u64::try_from(body.len()).unwrap()),
        RangeSupport::Absent,
        NegotiatedProtocol::Http11,
    );
    let backend = Arc::new(FakeProtocol::new(remote.clone(), body.clone()));
    let pool = FixedWorkerPool::new(Arc::clone(&backend), 8).unwrap();
    assert_eq!(
        pool.plan(&remote),
        PoolPlan::SingleStream {
            reason: SingleStreamReason::RangeNotProven,
        }
    );
    let stored = Arc::new(Mutex::new(Vec::new()));
    let syncs = Arc::new(AtomicUsize::new(0));
    let sink = RangeSink::new(
        Box::new(RecordingTarget {
            bytes: Arc::clone(&stored),
            syncs: Arc::clone(&syncs),
        }),
        0,
        Some(u64::try_from(body.len()).unwrap()),
    );

    let report = pool.execute_single(&remote, sink).await.unwrap();
    assert_eq!(report.plan(), pool.plan(&remote));
    assert_eq!(report.workers().len(), 1);
    assert_eq!(
        report.workers()[0].bytes(),
        u64::try_from(body.len()).unwrap()
    );
    assert_eq!(*stored.lock().unwrap(), body);
    assert_eq!(syncs.load(Ordering::SeqCst), 1);
    assert_eq!(
        backend.observed(),
        (
            vec![ObservedRequest {
                range: None,
                if_range: None,
            }],
            1,
        )
    );
}

#[tokio::test(start_paused = true)]
async fn a_slow_open_range_flushes_at_the_exact_journal_interval() {
    let body = b"slow".to_vec();
    let remote = proven_remote(4, NegotiatedProtocol::Http11);
    let pause = Arc::new(TransferPause {
        first: 0,
        prefix: 2,
        accepted: Semaphore::new(0),
        release: Semaphore::new(0),
    });
    let backend = Arc::new(
        FakeProtocol::new(remote.clone(), body.clone()).with_mid_transfer_pause(Arc::clone(&pause)),
    );
    let pool = FixedWorkerPool::new(backend, 1).unwrap();
    let (writer, stored, journal_synced) = writer_service(4, 1);
    let writer = Arc::new(writer);
    let test_started = tokio::time::Instant::now();
    let task_writer = Arc::clone(&writer);
    let mut transfer =
        tokio::spawn(async move { pool.execute_segmented(&remote, task_writer.as_ref()).await });

    tokio::select! {
        permit = pause.accepted.acquire() => permit.unwrap().forget(),
        result = &mut transfer => panic!("worker stopped before staging bytes: {result:?}"),
    }
    let permit = tokio::time::timeout(
        downpour_storage::writer::JOURNAL_FLUSH_INTERVAL + std::time::Duration::from_secs(1),
        journal_synced.acquire(),
    )
    .await
    .expect("the pool must drive the due journal flush while the response stays open")
    .unwrap();
    permit.forget();
    assert_eq!(
        test_started.elapsed(),
        downpour_storage::writer::JOURNAL_FLUSH_INTERVAL
    );
    let after = writer.snapshot().await.unwrap();
    assert_eq!(
        (
            after.allocator().intervals()[0].start(),
            after.allocator().intervals()[0].end()
        ),
        (0, 2)
    );
    assert_eq!(
        after.allocator().intervals()[0].state(),
        &downpour_intervals::IntervalState::Complete
    );
    assert_eq!(&stored.lock().unwrap()[..2], b"sl");

    pause.release.add_permits(1);
    transfer.await.unwrap().unwrap();
    let writer = match Arc::try_unwrap(writer) {
        Ok(writer) => writer,
        Err(_) => panic!("transfer retained the writer control handle"),
    };
    writer.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_split_fences_partial_source_bytes_before_reassigning_its_remainder() {
    let body = (0_u8..64).collect::<Vec<_>>();
    let remote = proven_remote(64, NegotiatedProtocol::Http11);
    let pause = Arc::new(TransferPause {
        first: 32,
        prefix: 8,
        accepted: Semaphore::new(0),
        release: Semaphore::new(0),
    });
    let backend = Arc::new(
        FakeProtocol::new(remote.clone(), body.clone()).with_mid_transfer_pause(Arc::clone(&pause)),
    );
    let pool = FixedWorkerPool::new(Arc::clone(&backend), 2).unwrap();
    let (writer, stored, _) = writer_service(64, 8);
    let writer = Arc::new(writer);
    let transfer_writer = Arc::clone(&writer);
    let mut transfer = tokio::spawn(async move {
        pool.execute_segmented(&remote, transfer_writer.as_ref())
            .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), pause.accepted.acquire())
        .await
        .expect("the source must stage its first half before cancellation")
        .unwrap()
        .forget();
    let report = tokio::time::timeout(std::time::Duration::from_secs(1), &mut transfer)
        .await
        .expect("the pool must cancel the source rather than wait for its held old response")
        .unwrap()
        .unwrap();
    assert_eq!(pause.release.available_permits(), 0);

    let (mut requests, maximum_active) = backend.observed();
    requests.sort_by_key(|request| match request.range {
        Some(ByteRangeSpec::FromTo { first, .. }) => first,
        _ => u64::MAX,
    });
    assert_eq!(maximum_active, 2);
    assert_eq!(
        requests,
        vec![
            observed_range(0, 31),
            observed_range(32, 63),
            observed_range(40, 51),
            observed_range(52, 63),
        ]
    );
    assert_eq!(
        report
            .workers()
            .iter()
            .filter(|record| record.worker() == WorkerId::new(0))
            .map(|record| record.bytes())
            .sum::<u64>(),
        44
    );
    assert_eq!(
        report
            .workers()
            .iter()
            .filter(|record| record.worker() == WorkerId::new(1))
            .map(|record| record.bytes())
            .sum::<u64>(),
        20
    );
    assert_eq!(*stored.lock().unwrap(), body);

    let writer = Arc::try_unwrap(writer).unwrap_or_else(|_| panic!("transfer retained writer"));
    let snapshot = writer.shutdown().await.unwrap();
    assert_eq!(snapshot.allocator().intervals().len(), 1);
    assert_eq!(
        snapshot.allocator().intervals()[0].state(),
        &IntervalState::Complete
    );
}

/// S3-C3 — connections are reused, not re-established, after a segment completes.
///
/// The count is taken once the transfer is over. An earlier version of this proof read the
/// handshake count at a mid-transfer instant and compared connection identifiers, which asserted
/// reqwest's idle-pool checkout order rather than anything this engine decides; it passed on Linux
/// and failed on Windows on identical source (B-39). What the engine does control is how many
/// times it *has* to reconnect, and that is what this measures.
///
/// The arithmetic is exact and has no tolerance in it. Two workers open two connections. Worker 0
/// finishes its half and is given a split of worker 1's, which costs worker 1 its connection
/// because a response cannot be shortened underneath itself — one reconnect, and one only. Worker
/// 0's second request must ride its existing connection. Three handshakes for four requests; a
/// pool that re-established after every completed segment would need four.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_completed_segment_costs_no_new_handshake_and_a_split_costs_exactly_one() {
    const LENGTH: u64 = 64;
    let content = Content::new(73, LENGTH);
    let server = PathologyServer::start_holding_first_range(
        ServerSpec {
            content,
            ..ServerSpec::default()
        },
        32,
    )
    .await
    .unwrap();

    // The probe runs on its own client so the connection it opens cannot be confused with, or
    // silently donated to, the pool's workers. This test is about the pool's handshakes.
    let prober = H1H2Backend::new(TransportMode::Http1Only).unwrap();
    let remote = prober
        .probe(ProbeRequest::new(server.entry_url().parse().unwrap()))
        .await
        .unwrap();
    let probe_connections = server.accepted_connection_count();
    let probe_requests = server.requests().len();

    let backend = Arc::new(H1H2Backend::new(TransportMode::Http1Only).unwrap());
    let pool = FixedWorkerPool::new(Arc::clone(&backend), 2).unwrap();
    let (writer, stored, _) = writer_service(LENGTH, 16);
    let writer = Arc::new(writer);
    let transfer_writer = Arc::clone(&writer);
    let mut transfer = tokio::spawn(async move {
        pool.execute_segmented(&remote, transfer_writer.as_ref())
            .await
    });

    // Establish the scenario before measuring it: worker 1 is genuinely held mid-response, and
    // worker 0 genuinely finished its own grant, so the reassignment below is a real split of live
    // work rather than a leftover pending range.
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        server.wait_until_range_held(),
    )
    .await
    .expect("the second grant must reach the server gate");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let snapshot = writer.snapshot().await.unwrap();
            if snapshot.allocator().intervals().iter().any(|interval| {
                interval.start() == 0
                    && interval.end() == 32
                    && interval.state() == &IntervalState::Complete
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("worker 0 must durably finish its initial grant while worker 1 is held");

    // Wait for the scheduler to actually reassign before releasing the gate. Releasing first lets
    // worker 1 finish its original grant, and then there is no split to measure.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if request_with_range(&server.requests(), "bytes=48-63").is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("worker 0 must be given a split of the held peer's segment");

    server.release_held_range();
    let report = tokio::time::timeout(std::time::Duration::from_secs(10), &mut transfer)
        .await
        .expect("the pool must finish once the held peer is released")
        .unwrap()
        .unwrap();

    let ranged_requests = server.requests().len() - probe_requests;
    let handshakes = server.accepted_connection_count() - probe_connections;
    assert_eq!(
        ranged_requests,
        4,
        "expected worker 0's grant, worker 1's grant, worker 0's split and worker 1's remainder, \
         got {:?}",
        server.requests()
    );
    assert_eq!(
        handshakes, 3,
        "two workers plus exactly one reconnect for the peer whose segment was split; a pool \
         that re-established a connection after a completed segment would need {ranged_requests}"
    );
    assert_eq!(
        server.held_range_count(),
        1,
        "the gate must have stopped exactly one response"
    );

    // The reassignment has to be the split it claims to be, or the counts above describe a
    // different scenario.
    assert!(
        request_with_range(&server.requests(), "bytes=48-63").is_some(),
        "worker 0 must have been given the upper half of worker 1's segment: {:?}",
        server.requests()
    );
    assert!(
        request_with_range(&server.requests(), "bytes=32-47").is_some(),
        "worker 1 must have been restarted on the half it kept: {:?}",
        server.requests()
    );
    assert_eq!(
        report
            .workers()
            .iter()
            .filter(|record| record.worker() == WorkerId::new(0))
            .count(),
        2,
        "stable worker 0 must have completed its initial grant and the reassigned split"
    );
    assert_eq!(
        report
            .workers()
            .iter()
            .map(|record| record.bytes())
            .sum::<u64>(),
        LENGTH
    );
    assert_eq!(*stored.lock().unwrap(), content.range(0, LENGTH));

    let writer = Arc::try_unwrap(writer).unwrap_or_else(|_| panic!("transfer retained writer"));
    let snapshot = writer.shutdown().await.unwrap();
    assert_eq!(snapshot.allocator().intervals().len(), 1);
    assert_eq!(
        snapshot.allocator().intervals()[0].state(),
        &IntervalState::Complete
    );
}

fn request_with_range(requests: &[RecordedRequest], range: &str) -> Option<RecordedRequest> {
    requests
        .iter()
        .find(|request| request.header("range").as_deref() == Some(range))
        .cloned()
}
