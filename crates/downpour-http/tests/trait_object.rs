//! S1-T7 — `TransferProtocol` is object-safe, and the sink is append-only.
//!
//! Object safety looks like a triviality and is not. ADR-0005's entire value rests on backends
//! being swappable at runtime — the `h3` backend behind a feature flag, and the simulator that
//! `docs/09-testing-strategy.md` §4 depends on. If a future signature change quietly broke
//! `dyn TransferProtocol`, nothing else in the workspace would notice until the simulation was
//! written and could not be. This test fails at compile time instead.
//!
//! The stub backend also demonstrates the other half of ADR-0005's design rule: a backend can be
//! written with no `reqwest` type anywhere in its signature. If that stopped being true, the
//! abstraction would be leaking.

use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;
use downpour_http::sink::SinkError;
use downpour_http::{
    BackendCapabilities, ProbeError, ProbeRequest, RangeOutcome, RangeRequest, RangeSink,
    SinkTarget, TransferError, TransferProtocol,
};
use downpour_types::{
    ByteRangeSpec, NegotiatedProtocol, RangeProof, RangeSupport, RemoteObject, Validator,
};
use url::Url;

// ---------------------------------------------------------------- a backend with no HTTP in it

/// A backend built entirely from `downpour-types` and this crate's own vocabulary. The simulator
/// in S2 will have exactly this shape.
struct StubBackend {
    total_length: u64,
}

#[async_trait]
impl TransferProtocol for StubBackend {
    async fn probe(&self, request: ProbeRequest) -> Result<RemoteObject, ProbeError> {
        let probe_range = ByteRangeSpec::FromTo { first: 0, last: 0 };
        let proof = RangeProof::from_observed_response(
            probe_range,
            206,
            Some(&format!("bytes 0-0/{}", self.total_length)),
            None,
            1,
        )
        .map_err(|source| ProbeError::ContentEncoding {
            url: request.url.clone(),
            source,
        })?;

        Ok(RemoteObject {
            final_url: request.url.clone(),
            redirect_chain: vec![request.url],
            total_length: Some(self.total_length),
            range_support: RangeSupport::Proven(proof),
            validator: Validator::from_headers(Some("\"stub\""), None),
            digest: None,
            protocol: NegotiatedProtocol::Http11,
            suggested_filename: Some("stub.bin".to_owned()),
            content_type: None,
            probed_at: SystemTime::now(),
        })
    }

    async fn fetch_range(
        &self,
        _request: RangeRequest,
        sink: &mut RangeSink,
    ) -> Result<RangeOutcome, TransferError> {
        sink.accept(b"stub")
            .await
            .map_err(|source| TransferError::Sink {
                url: Url::parse("http://stub.invalid/").unwrap_or_else(|_| unreachable!()),
                source,
            })?;
        Ok(RangeOutcome {
            bytes_delivered: 4,
            status: 206,
            content_range: None,
            protocol: NegotiatedProtocol::Http11,
            truncated: false,
        })
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            name: "stub",
            protocols: vec![NegotiatedProtocol::Http11],
            multiplexes_streams: false,
            supports_ranges: true,
        }
    }
}

// ---------------------------------------------------------------- a target that records writes

/// One recorded write: the absolute offset it landed at, and the bytes.
type RecordedWrite = (u64, Vec<u8>);

#[derive(Default, Clone)]
struct RecordingTarget {
    writes: Arc<Mutex<Vec<RecordedWrite>>>,
    syncs: Arc<Mutex<usize>>,
}

#[async_trait]
impl SinkTarget for RecordingTarget {
    async fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), SinkError> {
        if let Ok(mut writes) = self.writes.lock() {
            writes.push((offset, bytes.to_vec()));
        }
        Ok(())
    }

    async fn sync(&mut self) -> Result<(), SinkError> {
        if let Ok(mut syncs) = self.syncs.lock() {
            *syncs += 1;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- the object-safety claim

#[tokio::test]
async fn the_trait_is_usable_behind_a_box() {
    // The claim ADR-0005 rests on: a backend chosen at runtime.
    let backend: Box<dyn TransferProtocol> = Box::new(StubBackend { total_length: 4096 });

    let remote = backend
        .probe(ProbeRequest::new(
            Url::parse("http://example.invalid/file.bin").expect("test URL"),
        ))
        .await
        .expect("the stub probes successfully");

    assert!(remote.range_support.is_proven());
    assert_eq!(remote.total_length, Some(4096));
    assert_eq!(backend.capabilities().name, "stub");
}

#[tokio::test]
async fn several_backends_can_be_held_in_one_collection() {
    // What runtime backend selection actually looks like: h1h2 and the simulator in one registry.
    let backends: Vec<Box<dyn TransferProtocol>> = vec![
        Box::new(StubBackend { total_length: 1 }),
        Box::new(StubBackend { total_length: 2 }),
    ];
    let names: Vec<&str> = backends.iter().map(|b| b.capabilities().name).collect();
    assert_eq!(names, vec!["stub", "stub"]);
}

#[tokio::test]
async fn a_trait_object_can_be_shared_across_tasks() {
    // The engine holds one backend and many workers use it concurrently, so `Send + Sync` on the
    // trait object is a real requirement rather than a formality.
    let backend: Arc<dyn TransferProtocol> = Arc::new(StubBackend { total_length: 8 });
    let mut handles = Vec::new();
    for _ in 0..4 {
        let backend = Arc::clone(&backend);
        handles.push(tokio::spawn(async move {
            backend
                .probe(ProbeRequest::new(
                    Url::parse("http://example.invalid/x").expect("test URL"),
                ))
                .await
                .map(|remote| remote.total_length)
        }));
    }
    for handle in handles {
        let result = handle.await.expect("the task completes");
        assert_eq!(result.expect("the probe succeeds"), Some(8));
    }
}

#[tokio::test]
async fn the_real_backend_is_also_a_trait_object() {
    // Construction only — no network. The point is that H1H2Backend coerces to the trait object,
    // which is what the engine will hold.
    let backend: Box<dyn TransferProtocol> = Box::new(
        downpour_http::H1H2Backend::new(downpour_http::TransportMode::Negotiated)
            .expect("the h1h2 backend builds"),
    );
    let capabilities = backend.capabilities();
    assert_eq!(capabilities.name, "h1h2");
    assert!(capabilities.supports_ranges);
    assert!(
        capabilities.multiplexes_streams,
        "a negotiated backend may reach HTTP/2, so capacity can be added as streams (docs/03 §3.4)"
    );
}

#[tokio::test]
async fn an_http1_only_backend_reports_that_it_cannot_multiplex() {
    // The scheduler decides *what* to add — a stream or a connection — from this flag alone, and
    // never from a version check of its own (ADR-0005).
    let backend =
        downpour_http::H1H2Backend::new(downpour_http::TransportMode::Http1Only).expect("builds");
    let capabilities = backend.capabilities();
    assert_eq!(capabilities.protocols, vec![NegotiatedProtocol::Http11]);
    assert!(!capabilities.multiplexes_streams);
}

// ---------------------------------------------------------------- the append-only sink

#[tokio::test]
async fn a_sink_appends_from_its_base_offset_and_cannot_seek() {
    let target = RecordingTarget::default();
    let mut sink = RangeSink::new(Box::new(target.clone()), 4096, None);

    assert_eq!(sink.base_offset(), 4096);
    assert_eq!(sink.next_offset(), 4096);

    sink.accept(b"hello").await.expect("accepted");
    sink.accept(b"world").await.expect("accepted");

    assert_eq!(sink.written(), 10);
    assert_eq!(sink.next_offset(), 4106);

    let writes = target.writes.lock().expect("lock").clone();
    assert_eq!(
        writes,
        vec![(4096, b"hello".to_vec()), (4101, b"world".to_vec())]
    );
}

#[tokio::test]
async fn a_sink_refuses_to_write_past_its_grant() {
    // I-2's precondition. A server that over-delivers must not be able to reach the range next to
    // this one, and refusing is the only safe answer — a partial write would need the sink to
    // decide where to cut, which is the allocator's decision.
    let target = RecordingTarget::default();
    let mut sink = RangeSink::new(Box::new(target.clone()), 0, Some(8));

    sink.accept(b"12345678")
        .await
        .expect("exactly the grant is fine");
    let error = sink
        .accept(b"9")
        .await
        .expect_err("one byte past the grant is refused");
    assert!(matches!(
        error,
        SinkError::BeyondGrant {
            grant: 8,
            already_written: 8,
            attempted: 1
        }
    ));

    // And nothing was written by the refused call.
    let writes = target.writes.lock().expect("lock").clone();
    assert_eq!(
        writes.len(),
        1,
        "the refused write must not have reached the target"
    );
}

#[tokio::test]
async fn an_over_long_chunk_is_refused_whole() {
    let target = RecordingTarget::default();
    let mut sink = RangeSink::new(Box::new(target.clone()), 0, Some(4));
    let error = sink.accept(b"toolong").await.expect_err("refused");
    assert!(matches!(error, SinkError::BeyondGrant { attempted: 7, .. }));
    assert_eq!(sink.written(), 0);
    assert!(target.writes.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn sync_reaches_the_target() {
    // I-1's commit point has to actually be plumbed through; a sync that silently did nothing
    // would make the durability ordering a comment rather than a guarantee.
    let target = RecordingTarget::default();
    let mut sink = RangeSink::new(Box::new(target.clone()), 0, None);
    sink.accept(b"x").await.expect("accepted");
    sink.sync().await.expect("synced");
    assert_eq!(*target.syncs.lock().expect("lock"), 1);
}
