//! S3-C1 — a finished segment must not stop the workers that are still transferring.
//!
//! `fixed_connection_throughput.rs` gives every segment an identical per-byte budget, so all of
//! them finish at the same virtual instant. That is the one schedule in which a scheduler that
//! stops every peer whenever any worker completes still looks correct. This file supplies the
//! schedule that tells them apart: one segment completes while every peer is mid-response.
//!
//! The stagger is built from synchronisation, not from a clock. Under `start_paused` the runtime
//! auto-advances virtual time whenever it goes idle, and the download-state actor lives on its own
//! thread (ADR-0019), so a wait on the actor lets the clock jump past the peers' deadlines and the
//! two schedules collapse into one. A gate the peers block on cannot drift.
//!
//! The observation is in-flight request count, taken by the origin through a guard that also fires
//! when the pool *cancels* a request. Counting only requests that return would measure the
//! scheduler's bookkeeping instead of its behaviour.

use std::collections::BTreeSet;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
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
use tokio::sync::{Barrier, Notify};

const LENGTH: u64 = 4096;
const MIN_SPLIT: u64 = 16;
const WORKERS: usize = 8;
/// Bytes a gated peer hands over before it blocks, so a split has a real accepted prefix to fence
/// and a real unwritten suffix to reclaim.
const PREFIX: usize = 64;
const DEADLINE: Duration = Duration::from_secs(5);

#[derive(Debug, Default)]
struct OriginState {
    in_flight: usize,
    entered: usize,
    completed: usize,
    requested: Vec<Range<u64>>,
    live: Vec<Range<u64>>,
    overlapped: bool,
}

#[derive(Debug)]
struct Observation {
    in_flight: usize,
    /// Requests that entered the origin and did not deliver their whole range: the pool stopped
    /// them. This is the cost a reassignment is allowed to charge.
    stopped: usize,
    requested: Vec<Range<u64>>,
}

#[derive(Debug)]
struct GatedOrigin {
    body: Vec<u8>,
    remote: RemoteObject,
    state: Arc<Mutex<OriginState>>,
    /// Holds the first `WORKERS` requests until all of them have arrived, so the initial grants
    /// are provably live together rather than merely issued.
    initial: Barrier,
    /// Once open, nothing blocks; until then every request that is not the fast segment stops
    /// after its prefix.
    open: AtomicBool,
    release: Notify,
}

impl GatedOrigin {
    fn new(body: Vec<u8>) -> Self {
        Self {
            remote: proven_remote(u64::try_from(body.len()).expect("body length fits u64")),
            body,
            state: Arc::new(Mutex::new(OriginState::default())),
            initial: Barrier::new(WORKERS),
            open: AtomicBool::new(false),
            release: Notify::new(),
        }
    }

    fn observe(&self) -> Observation {
        let state = self.state.lock().expect("origin state is not poisoned");
        Observation {
            in_flight: state.in_flight,
            stopped: state.entered - state.completed - state.in_flight,
            requested: state.requested.clone(),
        }
    }

    fn request_count(&self) -> usize {
        self.state
            .lock()
            .expect("origin state is not poisoned")
            .requested
            .len()
    }

    fn open_the_gate(&self) {
        self.open.store(true, Ordering::SeqCst);
        self.release.notify_waiters();
    }

    async fn wait_for_the_gate(&self) {
        if self.open.load(Ordering::SeqCst) {
            return;
        }
        let notified = self.release.notified();
        if self.open.load(Ordering::SeqCst) {
            return;
        }
        notified.await;
    }

    fn enter(&self, range: Range<u64>) -> LiveRequest {
        let mut state = self.state.lock().expect("origin state is not poisoned");
        if state.live.iter().any(|live| overlaps(live, &range)) {
            state.overlapped = true;
        }
        state.live.push(range.clone());
        state.requested.push(range.clone());
        state.entered += 1;
        state.in_flight += 1;
        LiveRequest {
            state: Arc::clone(&self.state),
            range,
            completed: false,
        }
    }
}

/// Decrements on drop, so a request the pool cancels stops counting as in flight straight away.
struct LiveRequest {
    state: Arc<Mutex<OriginState>>,
    range: Range<u64>,
    completed: bool,
}

impl Drop for LiveRequest {
    fn drop(&mut self) {
        let mut state = self.state.lock().expect("origin state is not poisoned");
        if let Some(index) = state.live.iter().position(|live| live == &self.range) {
            state.live.remove(index);
        }
        state.in_flight -= 1;
        if self.completed {
            state.completed += 1;
        }
    }
}

fn overlaps(left: &Range<u64>, right: &Range<u64>) -> bool {
    left.start < right.end && right.start < left.end
}

#[async_trait]
impl TransferProtocol for GatedOrigin {
    async fn probe(&self, _request: ProbeRequest) -> Result<RemoteObject, ProbeError> {
        Ok(self.remote.clone())
    }

    async fn fetch_range(
        &self,
        request: RangeRequest,
        sink: &mut RangeSink,
    ) -> Result<RangeOutcome, TransferError> {
        let range = requested_range(request.range, self.body.len());
        let mut live = self.enter(range.clone());
        if self.request_count() <= WORKERS {
            self.initial.wait().await;
        }

        let start = usize::try_from(range.start).expect("offset fits usize");
        let end = usize::try_from(range.end).expect("offset fits usize");
        // The segment that starts at zero is the fast connection: it completes while every peer is
        // still mid-response. Everything else hands over a prefix and then waits.
        if range.start != 0 {
            let middle = (start + PREFIX).min(end);
            sink.accept(&self.body[start..middle])
                .await
                .map_err(|source| TransferError::Sink {
                    url: request.url.clone(),
                    source,
                })?;
            self.wait_for_the_gate().await;
            sink.accept(&self.body[middle..end])
                .await
                .map_err(|source| TransferError::Sink {
                    url: request.url.clone(),
                    source,
                })?;
        } else {
            sink.accept(&self.body[start..end])
                .await
                .map_err(|source| TransferError::Sink {
                    url: request.url.clone(),
                    source,
                })?;
        }
        live.completed = true;
        drop(live);

        Ok(RangeOutcome {
            bytes_delivered: range.end - range.start,
            status: 206,
            content_range: Some(ContentRange::Bytes {
                first: range.start,
                last: range.end - 1,
                complete_length: Some(
                    u64::try_from(self.body.len()).expect("body length fits u64"),
                ),
            }),
            protocol: NegotiatedProtocol::Http11,
            truncated: false,
        })
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            name: "sim-gated-h1",
            protocols: vec![NegotiatedProtocol::Http11],
            multiplexes_streams: false,
            supports_ranges: true,
        }
    }
}

fn requested_range(range: Option<ByteRangeSpec>, total: usize) -> Range<u64> {
    let total = u64::try_from(total).expect("body length fits u64");
    match range {
        Some(ByteRangeSpec::FromTo { first, last }) => first..last + 1,
        Some(ByteRangeSpec::From { first }) => first..total,
        Some(ByteRangeSpec::Suffix { len }) => total - len..total,
        None => 0..total,
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
    .expect("a validated 206 proves range support");
    let final_url = ProbeRequest::new(
        "https://stagger.test/file.bin"
            .parse()
            .expect("the fixture URL parses"),
    )
    .url;
    RemoteObject {
        final_url: final_url.clone(),
        redirect_chain: vec![final_url],
        total_length: Some(total_length),
        range_support: RangeSupport::Proven(proof),
        validator: Validator::StrongETag("\"stagger-v1\"".to_owned()),
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
        u64::try_from(
            self.bytes
                .lock()
                .expect("stored bytes are not poisoned")
                .len(),
        )
        .expect("body length fits u64")
    }

    fn write_all_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), WriterError> {
        let start = usize::try_from(offset).expect("offset fits usize");
        let end = start + bytes.len();
        self.bytes
            .lock()
            .expect("stored bytes are not poisoned")
            .get_mut(start..end)
            .expect("the fixture file covers every accepted range")
            .copy_from_slice(bytes);
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

fn fixture_body() -> Vec<u8> {
    (0..LENGTH)
        .map(|index| u8::try_from(index % 251).expect("modulus fits u8"))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_finished_segment_never_stops_the_peers_that_are_still_transferring() {
    let body = fixture_body();
    let origin = Arc::new(GatedOrigin::new(body.clone()));
    let pool = FixedWorkerPool::new(Arc::clone(&origin), WORKERS).expect("a non-zero pool");
    let stored = Arc::new(Mutex::new(vec![0; body.len()]));
    let durable = DurableWriter::try_new(
        MemoryData {
            bytes: Arc::clone(&stored),
        },
        MemoryJournal {
            total_length: LENGTH,
        },
        0,
    )
    .expect("the fixture writer starts");
    let allocator = SegmentAllocator::new(LENGTH, MIN_SPLIT).expect("a non-zero split floor");
    let writer =
        Arc::new(WriterService::start(durable, allocator, 16).expect("the state actor starts"));

    let transfer_origin = Arc::clone(&origin);
    let transfer_writer = Arc::clone(&writer);
    let mut transfer = tokio::spawn(async move {
        pool.execute_segmented(&transfer_origin.remote, transfer_writer.as_ref())
            .await
    });

    // The scenario has to be the one described, or nothing below proves anything. Wait until the
    // fast segment is durably complete while every peer is still parked at the gate.
    tokio::time::timeout(DEADLINE, async {
        loop {
            let snapshot = writer.snapshot().await.expect("the actor answers");
            if snapshot.allocator().intervals().iter().any(|interval| {
                interval.start() == 0
                    && interval.state() == &downpour_intervals::IntervalState::Complete
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the fast segment must finish while its peers are held");

    let initial = origin.observe();
    assert_eq!(
        initial.requested.len(),
        WORKERS,
        "all {WORKERS} initial grants must reach the origin before any reassignment: {:?}",
        initial.requested
    );
    assert_eq!(
        initial
            .requested
            .iter()
            .map(|range| range.end - range.start)
            .sum::<u64>(),
        LENGTH,
        "the initial grants must partition the representation: {:?}",
        initial.requested
    );

    // Now wait for the scheduler to react. The first reassignment is the moment it has decided
    // what to do about an idle worker, so it is the earliest point at which the cost of that
    // decision is fully paid and observable.
    let reacted = tokio::time::timeout(DEADLINE, async {
        loop {
            if origin.request_count() > WORKERS {
                break origin.observe();
            }
            tokio::task::yield_now().await;
        }
    })
    .await;

    let reacted = match reacted {
        Ok(reacted) => reacted,
        Err(_) => {
            origin.open_the_gate();
            let _ = tokio::time::timeout(DEADLINE, &mut transfer).await;
            panic!("the idle worker never received a reassignment");
        }
    };

    // The claim. Reassigning work to the worker that just finished may cost at most the one peer
    // whose segment is being split. Every other connection is still transferring.
    assert!(
        reacted.stopped <= 1,
        "reassigning one idle worker stopped {} in-flight requests; at most the peer whose \
         segment is split may be stopped. requests so far: {:?}",
        reacted.stopped,
        reacted.requested
    );
    assert!(
        reacted.in_flight >= WORKERS - 1,
        "only {} requests were in flight after the first reassignment; {} peers were still \
         transferring when the fast segment finished, so the pool must not have idled them. \
         requests so far: {:?}",
        reacted.in_flight,
        WORKERS - 1,
        reacted.requested
    );

    origin.open_the_gate();
    let report = tokio::time::timeout(DEADLINE, &mut transfer)
        .await
        .expect("the transfer finishes once the gate opens")
        .expect("the transfer task does not panic")
        .expect("the staggered transfer completes");

    // Concurrency is not allowed to be bought with overlapping ownership (I-2), and the bytes
    // still have to be right.
    let (overlapped, requested) = {
        let state = origin.state.lock().expect("origin state is not poisoned");
        (state.overlapped, state.requested.clone())
    };
    assert!(
        !overlapped,
        "two requests were live over the same bytes: {requested:?}"
    );
    let distinct = requested
        .iter()
        .map(|range| (range.start, range.end))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        distinct.len(),
        requested.len(),
        "the same byte range was requested twice, so accepted work was discarded and refetched: \
         {requested:?}"
    );

    assert_eq!(
        report
            .workers()
            .iter()
            .map(|worker| worker.bytes())
            .sum::<u64>(),
        LENGTH,
        "per-attempt byte accounting lost or duplicated bytes"
    );
    assert_eq!(
        *stored.lock().expect("stored bytes are not poisoned"),
        body,
        "the assembled file differs"
    );

    let writer = Arc::try_unwrap(writer)
        .unwrap_or_else(|_| panic!("the transfer retained the writer control handle"));
    let snapshot = writer
        .shutdown()
        .await
        .expect("the state actor stops cleanly");
    assert_eq!(snapshot.allocator().intervals().len(), 1);
    assert_eq!(
        snapshot.allocator().intervals()[0].state(),
        &downpour_intervals::IntervalState::Complete
    );
}
