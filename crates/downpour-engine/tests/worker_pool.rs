//! Behavioral proofs for Stage 3's fixed HTTP/1.1 worker pool and fallback boundary.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;
use downpour_engine::SegmentAllocator;
use downpour_engine::worker_pool::{FixedWorkerPool, PoolError, PoolPlan, SingleStreamReason};
use downpour_engine::writer_service::WriterService;
use downpour_http::{
    BackendCapabilities, ProbeError, ProbeRequest, RangeOutcome, RangeRequest, RangeSink,
    SinkError, SinkTarget, TransferError, TransferProtocol,
};
use downpour_storage::journal::FramedRecord;
use downpour_storage::writer::{DurableData, DurableJournal, DurableWriter, WriterError};
use downpour_types::{
    ByteRangeSpec, ContentRange, NegotiatedProtocol, RangeProof, RangeSupport, RemoteObject,
    Validator,
};
use tokio::sync::Barrier;

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

#[derive(Clone, Debug)]
struct FakeProtocol {
    remote: RemoteObject,
    body: Arc<Vec<u8>>,
    capabilities: BackendCapabilities,
    state: Arc<Mutex<ProtocolState>>,
    rendezvous: Option<Arc<Barrier>>,
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
        let rendezvous = {
            let mut state = self.state.lock().unwrap();
            state.requests.push(ObservedRequest {
                range: request.range,
                if_range: request.if_range.clone(),
            });
            state.active += 1;
            state.maximum_active = state.maximum_active.max(state.active);
            self.rendezvous.clone()
        };
        if request.range.is_some()
            && let Some(rendezvous) = rendezvous
        {
            rendezvous.wait().await;
        }

        let (start, end) = requested_window(request.range, self.body.len());
        let bytes = &self.body[start..end];
        let accepted = sink.accept(bytes).await;
        self.state.lock().unwrap().active -= 1;
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
}

impl DurableJournal for MemoryJournal {
    fn total_length(&self) -> u64 {
        self.total_length
    }

    fn append(&mut self, _record: &FramedRecord) -> Result<(), WriterError> {
        Ok(())
    }

    fn sync_data(&mut self) -> Result<(), WriterError> {
        Ok(())
    }
}

fn writer_service(total_length: u64, minimum_split: u64) -> (WriterService, Arc<Mutex<Vec<u8>>>) {
    let bytes = Arc::new(Mutex::new(vec![0; usize::try_from(total_length).unwrap()]));
    let data = MemoryData {
        bytes: Arc::clone(&bytes),
    };
    let journal = MemoryJournal { total_length };
    let writer = DurableWriter::try_new(data, journal, 0).unwrap();
    let allocator = SegmentAllocator::new(total_length, minimum_split).unwrap();
    (WriterService::start(writer, allocator, 8).unwrap(), bytes)
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
    let (writer, stored) = writer_service(64, 16);

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

fn observed_range(first: u64, last: u64) -> ObservedRequest {
    ObservedRequest {
        range: Some(ByteRangeSpec::FromTo { first, last }),
        if_range: None,
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
