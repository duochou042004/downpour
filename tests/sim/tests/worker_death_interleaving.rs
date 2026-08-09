//! S3-T6 — worker death and adversarial interleaving proof.
//!
//! A worker accepts a prefix of its grant and then dies while two peers remain live. Every run
//! uses the real allocator, pool, writer actor and durable acknowledgements. The protocol oracle
//! independently tracks live request ranges, so "the map stayed canonical" cannot stand in for
//! the semantic claim that no two workers were allowed to write the same byte.

use std::future::pending;
use std::io;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;
use downpour_engine::SegmentAllocator;
use downpour_engine::worker_pool::FixedWorkerPool;
use downpour_engine::writer_service::WriterService;
use downpour_http::{
    BackendCapabilities, ProbeError, ProbeRequest, RangeOutcome, RangeRequest, RangeSink,
    TransferError, TransferProtocol,
};
use downpour_intervals::IntervalState;
use downpour_storage::journal::{FramedRecord, JournalRecord};
use downpour_storage::writer::{DurableData, DurableJournal, DurableWriter, WriterError};
use downpour_types::{
    ByteRangeSpec, ContentRange, NegotiatedProtocol, RangeProof, RangeSupport, RemoteObject,
    Validator,
};
use tokio::sync::Barrier;

const LENGTH: u64 = 96;
const MIN_SPLIT: u64 = 8;
const DIED_GRANT_START: u64 = 24;
const DIED_GRANT_END: u64 = 48;
const DIED_AFTER: u64 = 32;

#[derive(Clone, Debug, Eq, PartialEq)]
enum Event {
    Requested(Range<u64>),
    Died {
        grant: Range<u64>,
        accepted: Range<u64>,
    },
    ResponseCompleted(Range<u64>),
}

#[derive(Debug)]
struct ProtocolState {
    active: Vec<Range<u64>>,
    maximum_active: usize,
    overlap_observed: bool,
    events: Vec<Event>,
}

#[derive(Clone, Debug)]
struct DeathOrigin {
    body: Arc<Vec<u8>>,
    remote: RemoteObject,
    state: Arc<Mutex<ProtocolState>>,
    initial_rendezvous: Arc<Barrier>,
    death_available: Arc<AtomicBool>,
    seed: u64,
}

impl DeathOrigin {
    fn new(body: Vec<u8>, seed: u64) -> Self {
        Self {
            remote: proven_remote(u64::try_from(body.len()).unwrap()),
            body: Arc::new(body),
            state: Arc::new(Mutex::new(ProtocolState {
                active: Vec::new(),
                maximum_active: 0,
                overlap_observed: false,
                events: Vec::new(),
            })),
            initial_rendezvous: Arc::new(Barrier::new(3)),
            death_available: Arc::new(AtomicBool::new(true)),
            seed,
        }
    }

    fn observation(&self) -> ProtocolState {
        let state = self.state.lock().unwrap();
        ProtocolState {
            active: state.active.clone(),
            maximum_active: state.maximum_active,
            overlap_observed: state.overlap_observed,
            events: state.events.clone(),
        }
    }
}

struct ActiveRequest {
    state: Arc<Mutex<ProtocolState>>,
    range: Range<u64>,
}

impl ActiveRequest {
    fn enter(state: &Arc<Mutex<ProtocolState>>, range: Range<u64>) -> Self {
        {
            let mut held = state.lock().unwrap();
            if held
                .active
                .iter()
                .any(|active| ranges_overlap(active, &range))
            {
                held.overlap_observed = true;
            }
            held.active.push(range.clone());
            held.maximum_active = held.maximum_active.max(held.active.len());
            held.events.push(Event::Requested(range.clone()));
        }
        Self {
            state: Arc::clone(state),
            range,
        }
    }
}

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap();
        if let Some(index) = state.active.iter().position(|range| range == &self.range) {
            state.active.remove(index);
        }
    }
}

fn ranges_overlap(left: &Range<u64>, right: &Range<u64>) -> bool {
    left.start < right.end && right.start < left.end
}

#[async_trait]
impl TransferProtocol for DeathOrigin {
    async fn probe(&self, _request: ProbeRequest) -> Result<RemoteObject, ProbeError> {
        Ok(self.remote.clone())
    }

    async fn fetch_range(
        &self,
        request: RangeRequest,
        sink: &mut RangeSink,
    ) -> Result<RangeOutcome, TransferError> {
        let range = requested_range(request.range, self.body.len());
        let _active = ActiveRequest::enter(&self.state, range.clone());
        if is_initial(&range) {
            self.initial_rendezvous.wait().await;
        }

        if range == (DIED_GRANT_START..DIED_GRANT_END)
            && self.death_available.swap(false, Ordering::SeqCst)
        {
            let start = usize::try_from(range.start).unwrap();
            let end = usize::try_from(DIED_AFTER).unwrap();
            sink.accept(&self.body[start..end])
                .await
                .map_err(|source| TransferError::Sink {
                    url: request.url.clone(),
                    source,
                })?;
            self.state.lock().unwrap().events.push(Event::Died {
                grant: range,
                accepted: DIED_GRANT_START..DIED_AFTER,
            });
            return Err(TransferError::Transport {
                url: request.url,
                source: Box::new(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "deterministic worker death",
                )),
            });
        }

        if range == (0..24) || range == (48..96) {
            pending::<()>().await;
        }

        let yields = usize::try_from(
            self.seed
                .wrapping_mul(0x9E37_79B9)
                .wrapping_add(range.start)
                % 5,
        )
        .unwrap();
        for _ in 0..yields {
            tokio::task::yield_now().await;
        }
        let start = usize::try_from(range.start).unwrap();
        let end = usize::try_from(range.end).unwrap();
        sink.accept(&self.body[start..end])
            .await
            .map_err(|source| TransferError::Sink {
                url: request.url.clone(),
                source,
            })?;
        self.state
            .lock()
            .unwrap()
            .events
            .push(Event::ResponseCompleted(range.clone()));

        Ok(RangeOutcome {
            bytes_delivered: range.end - range.start,
            status: 206,
            content_range: Some(ContentRange::Bytes {
                first: range.start,
                last: range.end - 1,
                complete_length: Some(u64::try_from(self.body.len()).unwrap()),
            }),
            protocol: NegotiatedProtocol::Http11,
            truncated: false,
        })
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            name: "sim-worker-death-h1",
            protocols: vec![NegotiatedProtocol::Http11],
            multiplexes_streams: false,
            supports_ranges: true,
        }
    }
}

fn is_initial(range: &Range<u64>) -> bool {
    range == &(0..24) || range == &(24..48) || range == &(48..96)
}

fn requested_range(range: Option<ByteRangeSpec>, total: usize) -> Range<u64> {
    match range {
        Some(ByteRangeSpec::FromTo { first, last }) => first..last + 1,
        Some(ByteRangeSpec::From { first }) => first..u64::try_from(total).unwrap(),
        Some(ByteRangeSpec::Suffix { len }) => {
            u64::try_from(total).unwrap() - len..u64::try_from(total).unwrap()
        }
        None => 0..u64::try_from(total).unwrap(),
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
    let final_url = ProbeRequest::new("https://death.test/file.bin".parse().unwrap()).url;
    RemoteObject {
        final_url: final_url.clone(),
        redirect_chain: vec![final_url],
        total_length: Some(total_length),
        range_support: RangeSupport::Proven(proof),
        validator: Validator::StrongETag("\"death-v1\"".to_owned()),
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
    completions: Arc<Mutex<Vec<Range<u64>>>>,
}

impl DurableJournal for MemoryJournal {
    fn total_length(&self) -> u64 {
        self.total_length
    }

    fn append(&mut self, record: &FramedRecord) -> Result<(), WriterError> {
        if let JournalRecord::BlockComplete { offset, len, .. } = record.record() {
            self.completions
                .lock()
                .unwrap()
                .push(*offset..offset + u64::from(*len));
        }
        Ok(())
    }

    fn sync_data(&mut self) -> Result<(), WriterError> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_death_and_interleaving_fuzz_preserve_exclusive_byte_ownership() {
    let body = (0_u8..u8::try_from(LENGTH).unwrap()).collect::<Vec<_>>();

    for seed in 0..16_u64 {
        let origin = Arc::new(DeathOrigin::new(body.clone(), seed));
        let pool = FixedWorkerPool::new(Arc::clone(&origin), 3).unwrap();
        let stored = Arc::new(Mutex::new(vec![0; body.len()]));
        let completions = Arc::new(Mutex::new(Vec::new()));
        let durable = DurableWriter::try_new(
            MemoryData {
                bytes: Arc::clone(&stored),
            },
            MemoryJournal {
                total_length: LENGTH,
                completions: Arc::clone(&completions),
            },
            0,
        )
        .unwrap();
        let allocator = SegmentAllocator::new(LENGTH, MIN_SPLIT).unwrap();
        let writer = WriterService::start(durable, allocator, 16).unwrap();

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            pool.execute_segmented(&origin.remote, &writer),
        )
        .await
        .unwrap_or_else(|_| panic!("seed {seed}: worker recovery deadlocked"));

        // Establish that the failure is behavioral, not an unreachable fixture: all three
        // initial grants became simultaneously live and the designated worker accepted its
        // prefix before the injected connection reset.
        let reached = origin.observation();
        for initial in [0..24, 24..48, 48..96] {
            assert!(
                reached.events.contains(&Event::Requested(initial.clone())),
                "seed {seed}: initial grant {initial:?} never reached the origin"
            );
        }
        assert_eq!(reached.maximum_active, 3, "seed {seed}");
        assert!(
            reached.events.contains(&Event::Died {
                grant: DIED_GRANT_START..DIED_GRANT_END,
                accepted: DIED_GRANT_START..DIED_AFTER,
            }),
            "seed {seed}: the injected worker death was not reached"
        );
        assert!(
            completions
                .lock()
                .unwrap()
                .contains(&(DIED_GRANT_START..DIED_AFTER)),
            "seed {seed}: the failed worker's accepted prefix never crossed the durability fence"
        );

        let report =
            outcome.unwrap_or_else(|error| panic!("seed {seed}: worker recovery failed: {error}"));
        assert_eq!(
            report
                .workers()
                .iter()
                .map(|worker| worker.bytes())
                .sum::<u64>(),
            LENGTH,
            "seed {seed}: per-attempt byte accounting lost or duplicated bytes"
        );
        assert_eq!(
            *stored.lock().unwrap(),
            body,
            "seed {seed}: final bytes differ"
        );

        let observation = origin.observation();
        assert_eq!(observation.maximum_active, 3, "seed {seed}");
        assert!(!observation.overlap_observed, "seed {seed}");
        assert!(observation.active.is_empty(), "seed {seed}");
        assert_eq!(
            observation
                .events
                .iter()
                .filter(|event| matches!(event, Event::Died { .. }))
                .count(),
            1,
            "seed {seed}: the scenario did not prove one worker death"
        );
        assert!(
            observation
                .events
                .contains(&Event::Requested(DIED_AFTER..DIED_GRANT_END)),
            "seed {seed}: only the failed grant's unwritten remainder was not reclaimed"
        );
        assert!(
            observation.events.iter().any(|event| matches!(
                event,
                Event::Requested(range) if range.start >= 60 && range.end <= LENGTH
            )),
            "seed {seed}: no active grant was split after recovery"
        );

        let committed = completions.lock().unwrap().clone();
        assert!(
            committed.contains(&(DIED_GRANT_START..DIED_AFTER)),
            "seed {seed}: accepted bytes from the dead worker were not durably fenced"
        );
        assert!(
            committed
                .windows(2)
                .any(|pair| pair[0].start > pair[1].start),
            "seed {seed}: durable completions never arrived out of offset order: {committed:?}"
        );

        let snapshot = writer.shutdown().await.unwrap();
        assert_eq!(snapshot.allocator().intervals().len(), 1, "seed {seed}");
        assert_eq!(
            snapshot.allocator().intervals()[0].state(),
            &IntervalState::Complete,
            "seed {seed}"
        );
    }
}
