//! B-64 — a worker that succeeds having delivered only a prefix of its grant.
//!
//! B-62 made the pool accept a ranged response narrower than the grant it was issued for, because
//! origins that cap how large a range they will serve are ordinary and refusing them stopped the
//! download outright. That introduced a path nothing else here reaches: a worker **succeeds**
//! holding an unfinished grant, the scheduler fences what it wrote and hands the remainder back,
//! and some other worker picks the remainder up while peers are still transferring.
//!
//! That is an ordering claim, and `docs/09-testing-strategy.md` §2 puts ordering at this layer. A
//! corpus case can only report the file that came out; it cannot see whether two workers held the
//! same byte at the same moment on the way there, because the interleaving that would prove it is
//! exactly the interleaving a passing run does not exhibit. The oracle below watches the requests
//! themselves, so "the map ended up canonical" cannot stand in for "no two workers were ever
//! allowed to write the same byte".
//!
//! The second test is the crash. The fence and the re-grant are two steps, and a process that dies
//! between them must come back claiming exactly the bytes it wrote — not the grant it had been
//! issued, and not nothing.

use std::collections::BTreeSet;
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
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
use downpour_sim::{Boundary, CrashPoint, Scenario, writer_over_real_files};
use downpour_storage::journal::{JournalRecord, recover_journal};
use downpour_types::{
    ByteRangeSpec, ContentRange, NegotiatedProtocol, RangeProof, RangeSupport, RemoteObject,
    Validator,
};

/// Big enough for three grants at the allocator's floor, small enough to read in a failure.
const LENGTH: u64 = 96;
const MIN_SPLIT: u64 = 8;
/// A quarter of a grant, so every worker needs four responses and the re-grant path runs often.
const CAP: u64 = 8;

/// Byte `i` is `i`, so a misplaced range is visible as a value rather than as a hash.
fn body() -> Vec<u8> {
    (0..usize::try_from(LENGTH).expect("fits"))
        .map(|index| u8::try_from(index % 251).expect("fits"))
        .collect()
}

fn proven_remote() -> RemoteObject {
    let proof = RangeProof::from_observed_response(
        ByteRangeSpec::FromTo { first: 0, last: 0 },
        206,
        Some(&format!("bytes 0-0/{LENGTH}")),
        None,
        1,
    )
    .expect("a conforming probe response");
    let final_url = ProbeRequest::new("https://capped.test/file.bin".parse().expect("url")).url;
    RemoteObject {
        final_url: final_url.clone(),
        redirect_chain: vec![final_url],
        total_length: Some(LENGTH),
        range_support: RangeSupport::Proven(proof),
        validator: Validator::StrongETag("\"capped-v1\"".to_owned()),
        digest: None,
        protocol: NegotiatedProtocol::Http11,
        suggested_filename: Some("file.bin".to_owned()),
        content_type: None,
        probed_at: SystemTime::now(),
    }
}

/// Where the data actually lives: `PartFile::create` writes to `<target>.dppart`, not to the
/// target itself, which is I-4's rule that a file only takes its final name after verification.
fn part_path(scenario: &Scenario) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{}.dppart", scenario.target().display()))
}

fn requested(range: Option<ByteRangeSpec>) -> Range<u64> {
    match range {
        Some(ByteRangeSpec::FromTo { first, last }) => first..last + 1,
        Some(ByteRangeSpec::From { first }) => first..LENGTH,
        Some(ByteRangeSpec::Suffix { len }) => LENGTH.saturating_sub(len)..LENGTH,
        None => 0..LENGTH,
    }
}

/// Independent bookkeeping of who holds which bytes, kept by the origin rather than by the engine.
///
/// The engine's own interval map cannot be the witness for a claim about the engine's own interval
/// map. This is what the *server* saw: which ranges were in flight at the same instant.
#[derive(Debug, Default)]
struct Oracle {
    live: Vec<Range<u64>>,
    overlapped: Option<(Range<u64>, Range<u64>)>,
    maximum_live: usize,
    served: Vec<Range<u64>>,
}

/// A range held for exactly as long as its request is in flight.
///
/// An RAII guard rather than a pair of calls, and that is load-bearing. The pool cancels a live
/// worker when it wants to split its grant, which drops the `fetch_range` future mid-await — an
/// explicit release at the end of the function never runs, the range stays live forever, and the
/// tail granted afterwards looks like an overlap that never happened. That false positive fired
/// on one run in five before this was a guard.
struct Held {
    oracle: Arc<Mutex<Oracle>>,
    range: Range<u64>,
}

impl Held {
    fn enter(oracle: &Arc<Mutex<Oracle>>, range: Range<u64>) -> Self {
        if let Ok(mut held) = oracle.lock() {
            if let Some(clash) = held
                .live
                .iter()
                .find(|live| live.start < range.end && range.start < live.end)
            {
                // Recorded rather than asserted here: this runs inside the origin, and a panic in
                // a spawned worker surfaces as a join error that names neither range.
                let clash = clash.clone();
                held.overlapped.get_or_insert((clash, range.clone()));
            }
            held.live.push(range.clone());
            let live = held.live.len();
            held.maximum_live = held.maximum_live.max(live);
        }
        Self {
            oracle: Arc::clone(oracle),
            range,
        }
    }

    /// Record what this request actually delivered. Only a response that completed counts.
    fn served(&self, range: Range<u64>) {
        if let Ok(mut held) = self.oracle.lock() {
            held.served.push(range);
        }
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        if let Ok(mut held) = self.oracle.lock()
            && let Some(index) = held.live.iter().position(|live| live == &self.range)
        {
            held.live.remove(index);
        }
    }
}

/// An origin that honours the start of every range and serves at most [`CAP`] bytes of it.
///
/// Exactly what S3, CloudFront and most CDNs do. Every response is complete, well formed and
/// honest: the `Content-Range` describes precisely the narrower span in the body.
struct CappedOrigin {
    body: Vec<u8>,
    remote: RemoteObject,
    oracle: Arc<Mutex<Oracle>>,
    requests: AtomicUsize,
}

impl CappedOrigin {
    fn new() -> Self {
        Self {
            body: body(),
            remote: proven_remote(),
            oracle: Arc::new(Mutex::new(Oracle::default())),
            requests: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl TransferProtocol for CappedOrigin {
    async fn probe(&self, _request: ProbeRequest) -> Result<RemoteObject, ProbeError> {
        Ok(self.remote.clone())
    }

    async fn fetch_range(
        &self,
        request: RangeRequest,
        sink: &mut RangeSink,
    ) -> Result<RangeOutcome, TransferError> {
        let asked = requested(request.range);
        let served = asked.start..asked.end.min(asked.start + CAP);
        self.requests.fetch_add(1, Ordering::SeqCst);
        let held = Held::enter(&self.oracle, asked.clone());

        // Yield between claiming the range and answering it, so that peers are genuinely in flight
        // while this one is held. Without it the runtime is free to run each worker to completion
        // in turn and the oracle never sees more than one live range, which would make the
        // overlap assertion below unfalsifiable.
        tokio::task::yield_now().await;

        let start = usize::try_from(served.start).expect("fits");
        let end = usize::try_from(served.end).expect("fits");
        sink.accept(&self.body[start..end])
            .await
            .map_err(|source| TransferError::Sink {
                url: request.url.clone(),
                source,
            })?;
        held.served(served.clone());

        Ok(RangeOutcome {
            bytes_delivered: served.end - served.start,
            status: 206,
            // Honest: this describes the narrower span, not the one that was asked for.
            content_range: Some(ContentRange::Bytes {
                first: served.start,
                last: served.end - 1,
                complete_length: Some(LENGTH),
            }),
            protocol: NegotiatedProtocol::Http11,
            truncated: false,
        })
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            name: "sim-capped-range",
            protocols: vec![NegotiatedProtocol::Http11],
            multiplexes_streams: false,
            supports_ranges: true,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_capped_origin_never_lets_two_workers_hold_one_byte() {
    let origin = Arc::new(CappedOrigin::new());
    let pool = FixedWorkerPool::new(Arc::clone(&origin), 3).expect("three workers");

    let scenario = Scenario::new("capped-regrant-ownership");
    // Leaked because `WriterService` owns its writer on its own thread, so the crash point has to
    // outlive this scope. Never armed here: this test wants the transfer to finish.
    let crash: &'static CrashPoint =
        Box::leak(Box::new(CrashPoint::after(Boundary::Complete, u64::MAX)));
    let writer = writer_over_real_files(&scenario, LENGTH, crash, 0).expect("artifacts");
    let allocator = SegmentAllocator::new(LENGTH, MIN_SPLIT).expect("allocator");
    let service = WriterService::start(writer, allocator, 8).expect("writer actor");

    let report = match pool.execute_segmented(&origin.remote, &service).await {
        Ok(report) => report,
        Err(error) => panic!("a capped origin must not fail the transfer: {error}"),
    };

    // Copied out rather than asserted under the guard: the assertions below are followed by an
    // await, and a std Mutex held across one is a deadlock waiting for a scheduler that reorders.
    let (overlapped, maximum_live, served) = {
        let oracle = origin.oracle.lock().expect("oracle");
        (
            oracle.overlapped.clone(),
            oracle.maximum_live,
            oracle.served.clone(),
        )
    };
    let responses = origin.requests.load(Ordering::SeqCst);

    assert_eq!(
        overlapped, None,
        "two workers held overlapping ranges at the same moment: {overlapped:?}"
    );
    // The claim is about *concurrent* ownership, so a run in which the workers never overlapped
    // proves nothing about it. This is the assertion that would have caught the whole scenario
    // degenerating into three sequential transfers.
    assert!(
        maximum_live > 1,
        "no two requests were ever in flight together, so nothing here constrains concurrent \
         ownership; observed at most {maximum_live} live"
    );
    // Every response was capped, so the transfer can only have finished by re-granting
    // remainders — 96 bytes at 8 a time cannot be fetched in fewer than 12 responses, and three
    // workers would otherwise have taken three.
    assert!(
        served.iter().all(|range| range.end - range.start <= CAP),
        "a response exceeded the cap, so the origin is not the one this test describes"
    );
    assert!(
        responses >= usize::try_from(LENGTH / CAP).expect("fits"),
        "only {responses} responses for {LENGTH} bytes capped at {CAP}; the cap cannot have applied"
    );

    // What is deliberately NOT asserted here: that the completed responses tile the
    // representation exactly, or even that they never repeat a byte. Both look like they should
    // hold and neither does. A worker cancelled for a split is dropped mid-`accept`, so bytes it
    // already delivered can be fenced durably by a response that never completed — one run served
    // 64..72 that way and the tiling assertion failed on a transfer that was entirely correct. By
    // the same token, if such bytes were NOT fenced the remainder is re-granted from where they
    // began and serving them again is right rather than wrong. The sound claim is the one above:
    // no two requests were ever in flight over the same byte. The bytes themselves are checked
    // against the generator at the end, which is the check that cannot be argued with.

    assert_eq!(
        report
            .workers()
            .iter()
            .map(|worker| worker.bytes())
            .sum::<u64>(),
        LENGTH,
        "the reported byte counts do not add up to the representation"
    );

    let snapshot = service.shutdown().await.expect("shutdown");
    let intervals = snapshot.allocator().intervals();
    assert_eq!(
        intervals.len(),
        1,
        "coverage was not coalesced: {intervals:?}"
    );
    assert_eq!(intervals[0].state(), &IntervalState::Complete);
    assert_eq!(
        std::fs::read(part_path(&scenario)).expect("part file"),
        body(),
        "the assembled bytes are not the representation"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crash_between_fencing_a_prefix_and_regranting_it_claims_only_what_it_wrote() {
    let origin = Arc::new(CappedOrigin::new());
    let pool = FixedWorkerPool::new(Arc::clone(&origin), 3).expect("three workers");

    let scenario = Scenario::new("capped-regrant-crash");
    // Two blocks commit cleanly, then the journal stops reaching the platter. The survivors are
    // the control: without them a run that wrote nothing at all would satisfy every assertion
    // below, since "claims only what it wrote" is trivially true of a process that wrote nothing.
    let crash: &'static CrashPoint =
        Box::leak(Box::new(CrashPoint::after(Boundary::JournalSync, 2)));
    let writer = writer_over_real_files(&scenario, LENGTH, crash, 0).expect("artifacts");
    let allocator = SegmentAllocator::new(LENGTH, MIN_SPLIT).expect("allocator");
    let service = WriterService::start(writer, allocator, 8).expect("writer actor");

    let outcome = pool.execute_segmented(&origin.remote, &service).await;
    assert!(
        outcome.is_err(),
        "the injected crash did not stop the transfer, so nothing was interrupted"
    );
    drop(service.shutdown().await);

    // Replay is the only authority for what survived. Nothing in memory carries across a crash,
    // which is the whole point of asking the question this way.
    let replayed = recover_journal(&scenario.journal()).expect("the journal replays");
    let mut claimed: Vec<Range<u64>> = Vec::new();
    for framed in replayed.records() {
        if let JournalRecord::BlockComplete { offset, len, .. } = framed.record() {
            claimed.push(*offset..offset + u64::from(*len));
        }
    }
    assert!(
        !claimed.is_empty(),
        "no block survived the crash, so this run cannot distinguish a correct fence from a \
         writer that never committed anything"
    );
    // And the crash landed after at least one remainder had been re-granted. Three workers that
    // each answered once would have exercised none of B-62's path, and every assertion below
    // would hold of a transfer that never fenced anything.
    assert!(
        origin.requests.load(Ordering::SeqCst) > 3,
        "only {} responses before the crash, which is one per worker: no remainder was ever \
         re-granted and this run does not reach the path it is written for",
        origin.requests.load(Ordering::SeqCst)
    );

    // No byte is claimed twice. A fence that released a prefix while the worker kept its grant, or
    // a re-grant that handed out bytes the fence had already committed, shows up here as an
    // overlap between two durable records.
    claimed.sort_by_key(|range| range.start);
    for pair in claimed.windows(2) {
        assert!(
            pair[0].end <= pair[1].start,
            "two durable records claim the same bytes: {:?} and {:?}",
            pair[0],
            pair[1]
        );
    }

    // Every byte the journal claims is on the platter and correct. This is the prefix-consistency
    // claim: what came back is a prefix of the truth, never a claim over bytes that never landed.
    let stored = std::fs::read(part_path(&scenario)).expect("part file");
    let truth = body();
    let mut durable = BTreeSet::new();
    for range in &claimed {
        for offset in range.clone() {
            let index = usize::try_from(offset).expect("fits");
            assert_eq!(
                stored[index], truth[index],
                "byte {offset} is claimed durable and holds {:#04x} where {:#04x} belongs",
                stored[index], truth[index]
            );
            durable.insert(offset);
        }
    }
    assert!(
        durable.len() < usize::try_from(LENGTH).expect("fits"),
        "the crash landed after the transfer had already finished, so it interrupted nothing"
    );
}
