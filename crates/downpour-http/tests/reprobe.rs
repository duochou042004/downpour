//! S2-T10 — the four capability re-probe triggers of `docs/03-transfer-engine-spec.md` §2.3.
//!
//! Cached capability evidence is what a resume plans against: which URL to fetch, whether ranges
//! are usable, how long the representation is. Every one of those can go stale while a download
//! is paused, and planning against stale evidence is how a resume ends up writing at an offset
//! the server no longer agrees with (I-6) or fetching a URL that has moved (I-8).
//!
//! The decision is deliberately a pure function of what is known, so it can be tested
//! exhaustively rather than through a server that happens to misbehave. What the triggers must
//! *not* do matters as much as what they must do: re-probing on every transient failure would
//! throw away the recorded validator's meaning on each retry, which is I-3's check discarded
//! (see S2-T9).

use std::time::{Duration, SystemTime};

use downpour_http::probe::{CAPABILITY_FRESHNESS, ReprobePolicy, ReprobeTrigger, ResumePlan};
use downpour_types::{
    ByteRangeSpec, ContentDigest, DigestAlgorithm, NegotiatedProtocol, RangeProof, RangeSupport,
    RemoteObject, Validator,
};
use url::Url;

fn url(raw: &str) -> Url {
    Url::parse(raw).expect("fixture URL is well formed")
}

fn proof() -> RangeProof {
    RangeProof::from_observed_response(
        ByteRangeSpec::FromTo { first: 0, last: 0 },
        206,
        Some("bytes 0-0/1024"),
        None,
        1,
    )
    .expect("fixture is a valid observed range response")
}

/// A download probed one minute ago, with ranges proven and a strong validator.
fn probed_object(probed_at: SystemTime) -> RemoteObject {
    RemoteObject {
        final_url: url("https://cdn.example.test/file.bin"),
        redirect_chain: vec![url("https://example.test/file.bin")],
        total_length: Some(1024),
        range_support: RangeSupport::Proven(proof()),
        validator: Validator::StrongETag("\"v1\"".to_owned()),
        digest: Some(ContentDigest {
            algorithm: DigestAlgorithm::Sha256,
            encoded: "YWJj".to_owned(),
        }),
        protocol: NegotiatedProtocol::Http2,
        suggested_filename: Some("file.bin".to_owned()),
        content_type: None,
        probed_at,
    }
}

fn one_minute_ago() -> (SystemTime, SystemTime) {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let probed_at = now - Duration::from_secs(60);
    (now, probed_at)
}

/// The named proof for S2-T10, and the reason B-16 exists.
///
/// Each of §2.3's four triggers must independently force a re-probe, and a resume that hits none
/// of them must not. Both halves matter. A policy that never fires plans against evidence that
/// may be hours stale; one that always fires re-probes on every retry, and a validator taken
/// from the retry's own response matches whatever the server is serving now — which is exactly
/// the I-3 check that S2-T9 exists to make.
#[test]
fn each_spec_trigger_forces_a_probe_and_an_unrelated_resume_does_not() {
    let policy = ReprobePolicy::default();
    let (now, probed_at) = one_minute_ago();
    let recorded = probed_object(probed_at);

    // ---- the baseline: nothing has changed, so nothing is re-probed.
    let unchanged = ResumePlan {
        recorded: &recorded,
        now,
        planned_url: &recorded.final_url,
        refreshed_url: None,
        last_observed_status: None,
    };
    assert_eq!(
        policy.triggers(&unchanged),
        Vec::new(),
        "an ordinary resume must reuse its recorded evidence"
    );
    assert!(!policy.is_required(&unchanged));

    // ---- trigger 1: the final URL changed (redirect target moved).
    let moved = url("https://other-cdn.example.test/file.bin");
    let plan = ResumePlan {
        planned_url: &moved,
        ..unchanged.clone()
    };
    assert_eq!(
        policy.triggers(&plan),
        vec![ReprobeTrigger::FinalUrlChanged {
            recorded: recorded.final_url.clone(),
            planned: moved.clone(),
        }],
        "capabilities were proven against the recorded URL, not this one (I-8)"
    );

    // ---- trigger 2: an observed status contradicts the recorded capabilities.
    // Ranges are recorded as Proven; a 200 to a ranged request says they are not.
    let plan = ResumePlan {
        last_observed_status: Some(200),
        ..unchanged.clone()
    };
    assert_eq!(
        policy.triggers(&plan),
        vec![ReprobeTrigger::StatusContradictsCapabilities {
            status: 200,
            reason: "range support was proven but the server answered a ranged request with 200",
        }],
        "a 200 where 206 was proven contradicts the recorded capability (I-6)"
    );

    // ---- trigger 3: the freshness window elapsed.
    let stale = probed_object(now - CAPABILITY_FRESHNESS - Duration::from_secs(1));
    let plan = ResumePlan {
        recorded: &stale,
        planned_url: &stale.final_url,
        ..unchanged.clone()
    };
    assert_eq!(
        policy.triggers(&plan),
        vec![ReprobeTrigger::FreshnessWindowElapsed {
            elapsed: CAPABILITY_FRESHNESS + Duration::from_secs(1),
            window: CAPABILITY_FRESHNESS,
        }],
        "evidence older than the window is not evidence about the representation now"
    );

    // ---- trigger 4: the user supplied a refreshed URL.
    let refreshed = url("https://cdn.example.test/file.bin?token=fresh");
    let plan = ResumePlan {
        refreshed_url: Some(&refreshed),
        ..unchanged.clone()
    };
    assert_eq!(
        policy.triggers(&plan),
        vec![ReprobeTrigger::RefreshedUrlSupplied {
            url: refreshed.clone(),
        }],
        "a supplied refresh invalidates cached capability evidence outright"
    );
    assert!(policy.is_required(&plan));
}

/// A transient failure is not a capability contradiction, and must not force a re-probe.
///
/// This is the half that keeps the policy honest. `429`, `503` and `500` are the statuses a
/// retry actually meets, and treating them as capability evidence would re-probe on every
/// back-off — discarding the recorded validator's meaning each time.
#[test]
fn a_transient_status_is_not_a_capability_contradiction() {
    let policy = ReprobePolicy::default();
    let (now, probed_at) = one_minute_ago();
    let recorded = probed_object(probed_at);

    for status in [408, 425, 429, 500, 502, 503, 504] {
        let plan = ResumePlan {
            recorded: &recorded,
            now,
            planned_url: &recorded.final_url,
            refreshed_url: None,
            last_observed_status: Some(status),
        };
        assert_eq!(
            policy.triggers(&plan),
            Vec::new(),
            "status {status} is a transient failure, not a statement about capabilities"
        );
    }
}

/// The statuses that *do* contradict recorded capabilities, and why each one does.
#[test]
fn each_contradicting_status_is_recognised_for_its_own_reason() {
    let policy = ReprobePolicy::default();
    let (now, probed_at) = one_minute_ago();
    let proven = probed_object(probed_at);

    // 416 says the range we asked for does not exist in the representation the server holds,
    // which contradicts the length recorded alongside the range proof.
    // 401/403/410 say the URL or credential that produced the evidence no longer applies.
    for status in [401, 403, 410, 416] {
        let plan = ResumePlan {
            recorded: &proven,
            now,
            planned_url: &proven.final_url,
            refreshed_url: None,
            last_observed_status: Some(status),
        };
        let triggers = policy.triggers(&plan);
        assert!(
            matches!(
                triggers.as_slice(),
                [ReprobeTrigger::StatusContradictsCapabilities { status: seen, .. }] if *seen == status
            ),
            "status {status} must contradict recorded capabilities, got {triggers:?}"
        );
    }

    // A 200 only contradicts something if ranges were *proven*. When they were never proven, a
    // whole-representation answer is exactly what was expected.
    let mut unproven = probed_object(probed_at);
    unproven.range_support = RangeSupport::Absent;
    let plan = ResumePlan {
        recorded: &unproven,
        now,
        planned_url: &unproven.final_url,
        refreshed_url: None,
        last_observed_status: Some(200),
    };
    assert_eq!(
        policy.triggers(&plan),
        Vec::new(),
        "a 200 contradicts nothing when range support was never claimed"
    );
}

/// Every trigger that applies is reported, not just the first one found.
///
/// A caller that logs why it re-probed should not be told half the reason, and a policy that
/// short-circuits would hide a second problem behind the first.
#[test]
fn every_applicable_trigger_is_reported() {
    let policy = ReprobePolicy::default();
    let (now, _) = one_minute_ago();
    let stale = probed_object(now - CAPABILITY_FRESHNESS - Duration::from_secs(30));
    let moved = url("https://elsewhere.example.test/file.bin");
    let refreshed = url("https://cdn.example.test/file.bin?token=fresh");

    let triggers = policy.triggers(&ResumePlan {
        recorded: &stale,
        now,
        planned_url: &moved,
        refreshed_url: Some(&refreshed),
        last_observed_status: Some(416),
    });

    assert_eq!(triggers.len(), 4, "got {triggers:?}");
    assert!(matches!(
        triggers[0],
        ReprobeTrigger::RefreshedUrlSupplied { .. }
    ));
    assert!(matches!(
        triggers[1],
        ReprobeTrigger::FinalUrlChanged { .. }
    ));
    assert!(matches!(
        triggers[2],
        ReprobeTrigger::StatusContradictsCapabilities { .. }
    ));
    assert!(matches!(
        triggers[3],
        ReprobeTrigger::FreshnessWindowElapsed { .. }
    ));
}

/// A clock that moved backwards makes the evidence's age unknowable, so it is treated as stale.
///
/// The alternative — assuming zero elapsed and reusing the evidence — trusts a clock that has
/// already demonstrated it cannot be trusted. Re-probing costs one ranged GET.
#[test]
fn a_probe_timestamp_in_the_future_is_treated_as_stale() {
    let policy = ReprobePolicy::default();
    let (now, _) = one_minute_ago();
    let skewed = probed_object(now + Duration::from_secs(3600));

    let triggers = policy.triggers(&ResumePlan {
        recorded: &skewed,
        now,
        planned_url: &skewed.final_url,
        refreshed_url: None,
        last_observed_status: None,
    });

    assert!(
        matches!(
            triggers.as_slice(),
            [ReprobeTrigger::FreshnessWindowElapsed { .. }]
        ),
        "an unknowable age must re-probe rather than reuse, got {triggers:?}"
    );
}

/// A refreshed URL equal to the recorded one is still a refresh.
///
/// The user re-fetched the page and handed us a URL; that it happens to be textually identical
/// says nothing about whether the bytes behind it still are. Comparing them and staying quiet
/// would make the trigger depend on an accident.
#[test]
fn a_refreshed_url_identical_to_the_recorded_one_still_invalidates_the_evidence() {
    let policy = ReprobePolicy::default();
    let (now, probed_at) = one_minute_ago();
    let recorded = probed_object(probed_at);
    let same = recorded.final_url.clone();

    let triggers = policy.triggers(&ResumePlan {
        recorded: &recorded,
        now,
        planned_url: &recorded.final_url,
        refreshed_url: Some(&same),
        last_observed_status: None,
    });

    assert_eq!(
        triggers,
        vec![ReprobeTrigger::RefreshedUrlSupplied { url: same }],
        "a supplied refresh is a signal from the user, not a string comparison"
    );
}

/// The window is configurable, and the boundary is not off by one.
#[test]
fn the_freshness_window_is_configurable_and_its_boundary_is_exact() {
    let policy = ReprobePolicy::new(Duration::from_secs(120));
    let (now, _) = one_minute_ago();

    let exactly_at = probed_object(now - Duration::from_secs(120));
    assert_eq!(
        policy.triggers(&ResumePlan {
            recorded: &exactly_at,
            now,
            planned_url: &exactly_at.final_url,
            refreshed_url: None,
            last_observed_status: None,
        }),
        Vec::new(),
        "evidence exactly at the window is still inside it"
    );

    let just_past = probed_object(now - Duration::from_secs(121));
    assert!(
        !policy
            .triggers(&ResumePlan {
                recorded: &just_past,
                now,
                planned_url: &just_past.final_url,
                refreshed_url: None,
                last_observed_status: None,
            })
            .is_empty(),
        "one second past the window is outside it"
    );
}

// ------------------------------------------------- the policy is consumed, not merely defined

mod wiring {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use downpour_http::probe::ReprobePolicy;
    use downpour_http::{
        BackendCapabilities, ProbeError, ProbeRequest, RangeOutcome, RangeRequest, RangeSink,
        RetryPolicy, SingleStream, StorageLayout, TransferError, TransferProtocol,
    };
    use downpour_types::{
        ByteRangeSpec, NegotiatedProtocol, RangeProof, RangeSupport, RemoteObject, Validator,
    };
    use url::Url;

    const TOTAL: u64 = 64;

    /// A backend that counts probes and fails the first body, so a resume is always attempted.
    struct CountingBackend {
        probes: Arc<AtomicU32>,
        /// The ETag each successive probe reports, so a test can make the representation change.
        etags: Mutex<Vec<&'static str>>,
        bodies: AtomicU32,
    }

    impl CountingBackend {
        fn new(etags: Vec<&'static str>) -> Self {
            Self {
                probes: Arc::new(AtomicU32::new(0)),
                etags: Mutex::new(etags),
                bodies: AtomicU32::new(0),
            }
        }
    }

    #[async_trait]
    impl TransferProtocol for CountingBackend {
        async fn probe(&self, request: ProbeRequest) -> Result<RemoteObject, ProbeError> {
            let index = self.probes.fetch_add(1, Ordering::SeqCst) as usize;
            let etag = self
                .etags
                .lock()
                .ok()
                .and_then(|held| held.get(index).copied().or_else(|| held.last().copied()))
                .unwrap_or("\"v1\"");
            let probe_range = ByteRangeSpec::FromTo { first: 0, last: 0 };
            let proof = RangeProof::from_observed_response(
                probe_range,
                206,
                Some(&format!("bytes 0-0/{TOTAL}")),
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
                total_length: Some(TOTAL),
                range_support: RangeSupport::Proven(proof),
                validator: Validator::StrongETag(etag.to_owned()),
                digest: None,
                protocol: NegotiatedProtocol::Http11,
                suggested_filename: Some("file.bin".to_owned()),
                content_type: None,
                probed_at: std::time::SystemTime::now(),
            })
        }

        async fn fetch_range(
            &self,
            request: RangeRequest,
            sink: &mut RangeSink,
        ) -> Result<RangeOutcome, TransferError> {
            // First body arrives short and dies, so the retry has durable bytes to resume from.
            let first = self.bodies.fetch_add(1, Ordering::SeqCst) == 0;
            let start = usize::try_from(sink.base_offset()).expect("fits");
            let end = if first {
                32
            } else {
                usize::try_from(TOTAL).expect("fits")
            };
            let bytes: Vec<u8> = (start..end)
                .map(|i| u8::try_from(i % 251).expect("fits"))
                .collect();
            sink.accept(&bytes)
                .await
                .map_err(|source| TransferError::Sink {
                    url: request.url.clone(),
                    source,
                })?;
            if first {
                return Err(TransferError::TruncatedBody {
                    url: request.url,
                    expected: TOTAL,
                    delivered: 32,
                });
            }
            Ok(RangeOutcome {
                bytes_delivered: u64::try_from(end - start).expect("fits"),
                status: if sink.base_offset() > 0 { 206 } else { 200 },
                content_range: None,
                truncated: false,
                protocol: NegotiatedProtocol::Http11,
            })
        }

        fn capabilities(&self) -> BackendCapabilities {
            BackendCapabilities {
                name: "counting-stub",
                protocols: vec![NegotiatedProtocol::Http11],
                multiplexes_streams: false,
                supports_ranges: true,
            }
        }
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let path = std::env::temp_dir().join(format!("downpour-reprobe-{tag}-{unique}"));
            std::fs::create_dir_all(path.join("data")).expect("data dir");
            std::fs::create_dir_all(path.join("journals")).expect("journal dir");
            Self(path)
        }
        fn layout(&self) -> StorageLayout {
            StorageLayout::new(self.0.join("data"), self.0.join("journals"))
        }
        fn data(&self) -> PathBuf {
            self.0.join("data")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn url() -> Url {
        Url::parse("http://stub.invalid/file.bin").expect("well formed")
    }

    fn stream(backend: CountingBackend, freshness: Duration) -> SingleStream<CountingBackend> {
        SingleStream::new(backend)
            .with_retry_policy(RetryPolicy::fast_for_tests())
            .with_reprobe_policy(ReprobePolicy::new(freshness))
    }

    /// With a window that has already elapsed, the resume re-probes before it is planned.
    ///
    /// Without this the policy would be a well-tested function nobody calls — which is exactly
    /// what B-16 recorded about `probed_at`, and exactly the shape of dead weight that looks
    /// like coverage.
    #[tokio::test]
    async fn an_elapsed_freshness_window_makes_the_resume_re_probe() {
        let backend = CountingBackend::new(vec!["\"v1\""]);
        let probes = Arc::clone(&backend.probes);
        let scratch = Scratch::new("stale");

        let path = stream(backend, Duration::ZERO)
            .download(url(), &scratch.layout())
            .await
            .expect("the resume completes after re-probing");

        assert_eq!(
            probes.load(Ordering::SeqCst),
            2,
            "one probe to start, one to re-establish stale capability evidence"
        );
        assert_eq!(std::fs::read(&path).expect("read").len(), 64);
        assert_eq!(
            entries(&scratch.data()),
            vec!["file.bin"],
            "no .dppart may be left behind"
        );
    }

    /// Inside the window, a retry reuses its recorded evidence and does not re-probe.
    #[tokio::test]
    async fn a_fresh_window_makes_the_resume_reuse_its_recorded_evidence() {
        let backend = CountingBackend::new(vec!["\"v1\""]);
        let probes = Arc::clone(&backend.probes);
        let scratch = Scratch::new("fresh");

        stream(backend, Duration::from_secs(3600))
            .download(url(), &scratch.layout())
            .await
            .expect("the resume completes");

        assert_eq!(
            probes.load(Ordering::SeqCst),
            1,
            "an in-window retry must not re-probe; a validator fetched now would match whatever \
             the server is serving now, which is I-3's check thrown away"
        );
    }

    /// A re-probe that finds a different validator stops rather than resuming.
    ///
    /// This is I-3 caught one request earlier than `If-Range` would have caught it. The
    /// representation changed while we were away, so the durable bytes belong to a version the
    /// server no longer has, and continuing would splice.
    #[tokio::test]
    async fn a_re_probe_that_finds_a_changed_validator_refuses_to_resume() {
        let backend = CountingBackend::new(vec!["\"v1\"", "\"v2\""]);
        let scratch = Scratch::new("changed");

        let error = stream(backend, Duration::ZERO)
            .download(url(), &scratch.layout())
            .await
            .expect_err("a changed representation must not be resumed into");

        assert_eq!(error.kind(), "validator_mismatch");
        assert_eq!(
            entries(&scratch.data()),
            vec!["file.bin.dppart"],
            "the part file is kept as evidence and never renamed (I-4)"
        );
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read dir")
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}
