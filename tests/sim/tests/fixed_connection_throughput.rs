//! S3-T5 — deterministic fixed-connection throughput proof.
//!
//! The origin gives every HTTP/1.1 request an independent byte-rate budget and imposes no
//! aggregate cap. Tokio virtual time makes the comparison exact: no wall-clock scheduler load,
//! CI contention, or release-build speed can change the result.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use downpour_engine::SegmentAllocator;
use downpour_engine::worker_pool::FixedWorkerPool;
use downpour_engine::writer_service::WriterService;
use downpour_http::{
    BackendCapabilities, ProbeError, ProbeRequest, RangeOutcome, RangeRequest, RangeSink,
    TransferError, TransferProtocol,
};
use downpour_storage::journal::FramedRecord;
use downpour_storage::writer::{DurableData, DurableJournal, DurableWriter, WriterError};
use downpour_types::{
    ByteRangeSpec, ContentRange, NegotiatedProtocol, RangeProof, RangeSupport, RemoteObject,
    Validator,
};

const LENGTH: u64 = 64;
const MIN_SPLIT: u64 = 16;

#[derive(Debug)]
struct OriginState {
    active: usize,
    maximum_active: usize,
    requests: usize,
    first_started: Option<tokio::time::Instant>,
    last_finished: Option<tokio::time::Instant>,
}

#[derive(Clone, Debug)]
struct SimulatedOrigin {
    body: Arc<Vec<u8>>,
    remote: RemoteObject,
    state: Arc<Mutex<OriginState>>,
}

impl SimulatedOrigin {
    fn new(body: Vec<u8>) -> Self {
        Self {
            remote: proven_remote(u64::try_from(body.len()).unwrap()),
            body: Arc::new(body),
            state: Arc::new(Mutex::new(OriginState {
                active: 0,
                maximum_active: 0,
                requests: 0,
                first_started: None,
                last_finished: None,
            })),
        }
    }

    fn observation(&self) -> (usize, usize, Duration) {
        let state = self.state.lock().unwrap();
        let elapsed = state
            .first_started
            .zip(state.last_finished)
            .map_or(Duration::ZERO, |(first, last)| last - first);
        (state.requests, state.maximum_active, elapsed)
    }
}

#[async_trait]
impl TransferProtocol for SimulatedOrigin {
    async fn probe(&self, _request: ProbeRequest) -> Result<RemoteObject, ProbeError> {
        Ok(self.remote.clone())
    }

    async fn fetch_range(
        &self,
        request: RangeRequest,
        sink: &mut RangeSink,
    ) -> Result<RangeOutcome, TransferError> {
        {
            let mut state = self.state.lock().unwrap();
            let now = tokio::time::Instant::now();
            state.active += 1;
            state.maximum_active = state.maximum_active.max(state.active);
            state.requests += 1;
            state.first_started = Some(state.first_started.map_or(now, |first| first.min(now)));
        }
        let (start, end) = requested_window(request.range, self.body.len());

        let delivered = u64::try_from(end - start).unwrap();
        // One virtual millisecond per byte, independently for every HTTP/1.1 connection. There
        // is deliberately no aggregate server budget: four disjoint 16-byte requests therefore
        // finish together in 16 ms, while one 64-byte request takes 64 ms.
        tokio::time::sleep(Duration::from_millis(delivered)).await;
        {
            let mut state = self.state.lock().unwrap();
            let now = tokio::time::Instant::now();
            state.last_finished = Some(state.last_finished.map_or(now, |last| last.max(now)));
        }
        let accepted = sink.accept(&self.body[start..end]).await;
        self.state.lock().unwrap().active -= 1;
        accepted.map_err(|source| TransferError::Sink {
            url: request.url.clone(),
            source,
        })?;

        Ok(RangeOutcome {
            bytes_delivered: delivered,
            status: 206,
            content_range: Some(ContentRange::Bytes {
                first: u64::try_from(start).unwrap(),
                last: u64::try_from(end - 1).unwrap(),
                complete_length: Some(u64::try_from(self.body.len()).unwrap()),
            }),
            protocol: NegotiatedProtocol::Http11,
            truncated: false,
        })
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            name: "sim-independent-h1",
            protocols: vec![NegotiatedProtocol::Http11],
            multiplexes_streams: false,
            supports_ranges: true,
        }
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

fn proven_remote(total_length: u64) -> RemoteObject {
    let proof = RangeProof::from_observed_response(
        ByteRangeSpec::FromTo { first: 0, last: 0 },
        206,
        Some(&format!("bytes 0-0/{total_length}")),
        None,
        1,
    )
    .unwrap();
    let final_url = ProbeRequest::new("https://sim.test/file.bin".parse().unwrap()).url;
    RemoteObject {
        final_url: final_url.clone(),
        redirect_chain: vec![final_url],
        total_length: Some(total_length),
        range_support: RangeSupport::Proven(proof),
        validator: Validator::StrongETag("\"sim-v1\"".to_owned()),
        digest: None,
        protocol: NegotiatedProtocol::Http11,
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

#[derive(Debug)]
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

struct Run {
    wire_elapsed: Duration,
    requests: usize,
    maximum_active: usize,
    bytes: Vec<u8>,
}

async fn run_with(workers: usize, body: &[u8]) -> Run {
    let origin = Arc::new(SimulatedOrigin::new(body.to_vec()));
    let pool = FixedWorkerPool::new(Arc::clone(&origin), workers).unwrap();
    let stored = Arc::new(Mutex::new(vec![0; body.len()]));
    let writer = DurableWriter::try_new(
        MemoryData {
            bytes: Arc::clone(&stored),
        },
        MemoryJournal {
            total_length: u64::try_from(body.len()).unwrap(),
        },
        0,
    )
    .unwrap();
    let allocator = SegmentAllocator::new(u64::try_from(body.len()).unwrap(), MIN_SPLIT).unwrap();
    let writer = WriterService::start(writer, allocator, 16).unwrap();
    let report = pool
        .execute_segmented(&origin.remote, &writer)
        .await
        .unwrap();
    assert_eq!(
        report
            .workers()
            .iter()
            .map(|worker| worker.bytes())
            .sum::<u64>(),
        u64::try_from(body.len()).unwrap()
    );
    writer.shutdown().await.unwrap();
    let (requests, maximum_active, wire_elapsed) = origin.observation();
    let bytes = stored.lock().unwrap().clone();
    Run {
        wire_elapsed,
        requests,
        maximum_active,
        bytes,
    }
}

#[tokio::test(start_paused = true)]
async fn four_independent_connections_beat_one_by_an_explicit_margin() {
    let body = (0_u8..u8::try_from(LENGTH).unwrap()).collect::<Vec<_>>();
    let single = run_with(1, &body).await;
    let parallel = run_with(4, &body).await;

    assert_eq!(single.bytes, body);
    assert_eq!(parallel.bytes, body);
    assert_eq!((single.requests, single.maximum_active), (1, 1));
    assert_eq!((parallel.requests, parallel.maximum_active), (4, 4));
    assert_eq!(single.wire_elapsed, Duration::from_millis(LENGTH));
    assert_eq!(parallel.wire_elapsed, Duration::from_millis(LENGTH / 4));
    assert!(
        parallel.wire_elapsed * 2 <= single.wire_elapsed,
        "four independent connections took {:?}, one took {:?}",
        parallel.wire_elapsed,
        single.wire_elapsed
    );
}
