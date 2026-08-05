//! S1-T8 — the capability probe against the pathology server.
//!
//! `docs/03-transfer-engine-spec.md` §2. These live in the corpus crate rather than in
//! `downpour-http` for two reasons: `docs/09-testing-strategy.md` puts "engine reaction to a
//! server behaviour" in the corpus layer, and `downpour-corpus` can depend on `downpour-http`
//! normally, whereas the reverse would be a dependency cycle held together only by Cargo's
//! tolerance for circular dev-dependencies.
//!
//! The load-bearing assertion is **I-6**: exactly one server behaviour yields
//! `RangeSupport::Proven`, and every other one — including all four ways a server can send a
//! `206` that does not mean what it says — yields `Absent`.

use std::time::Duration;

use downpour_corpus::content::Content;
use downpour_corpus::server::{Framing, PathologyServer, Protocol, RangeBehaviour, ServerSpec};
use downpour_http::{H1H2Backend, ProbeError, ProbeRequest, TransferProtocol, TransportMode};
use downpour_types::{NegotiatedProtocol, Validator};
use url::Url;

const SIZE: u64 = 256 * 1024;

fn spec() -> ServerSpec {
    ServerSpec {
        content: Content::new(42, SIZE),
        ..ServerSpec::default()
    }
}

fn backend() -> H1H2Backend {
    H1H2Backend::new(TransportMode::Http1Only).expect("the h1h2 backend builds")
}

fn h2_backend() -> H1H2Backend {
    H1H2Backend::new(TransportMode::Http2PriorKnowledge).expect("the h1h2 backend builds")
}

fn request(url: &str) -> ProbeRequest {
    let mut request = ProbeRequest::new(Url::parse(url).expect("the server's URL is well formed"));
    // Short, so a hung server fails the test rather than stalling the suite.
    request.timeout = Duration::from_secs(10);
    request
}

// ---------------------------------------------------------------- I-6: exactly one way in

#[tokio::test]
async fn a_validated_206_proves_range_support() {
    let server = PathologyServer::start(spec()).await.expect("server starts");
    let remote = backend()
        .probe(request(&server.entry_url()))
        .await
        .expect("a conforming server probes cleanly");

    assert!(
        remote.range_support.is_proven(),
        "a validated 206 is the one thing that proves it"
    );
    assert_eq!(remote.total_length, Some(SIZE));
    assert_eq!(remote.protocol, NegotiatedProtocol::Http11);
}

/// Every one of these is a server that could fool a client into segmenting when it must not.
/// Four of the five answer with `206`.
#[tokio::test]
async fn no_other_server_behaviour_proves_range_support() {
    let cases: Vec<(&str, RangeBehaviour)> = vec![
        (
            "no Accept-Ranges, 200 for a ranged GET",
            RangeBehaviour::Absent,
        ),
        ("advertises bytes, then answers 200", RangeBehaviour::Lies),
        (
            "206 with a correct Content-Range and the whole body",
            RangeBehaviour::IgnoreButClaim,
        ),
        (
            "206 describing a different range",
            RangeBehaviour::ShiftedContentRange { by: 4096 },
        ),
        (
            "206 with no Content-Range at all",
            RangeBehaviour::OmitContentRange,
        ),
        (
            "206 with a malformed Content-Range",
            RangeBehaviour::LiteralContentRange("bytes 0 - 0/262144".to_owned()),
        ),
        (
            "206 with an unknown total length",
            RangeBehaviour::UnknownTotalLength,
        ),
    ];

    for (description, ranges) in cases {
        let server = PathologyServer::start(ServerSpec { ranges, ..spec() })
            .await
            .expect("server starts");
        let remote = backend()
            .probe(request(&server.entry_url()))
            .await
            .unwrap_or_else(|error| panic!("{description}: probe failed outright: {error}"));

        assert!(
            !remote.range_support.is_proven(),
            "{description}: this must NOT prove range support (I-6)"
        );
    }
}

#[tokio::test]
async fn an_unknown_total_length_leaves_nothing_to_segment() {
    let server = PathologyServer::start(ServerSpec {
        ranges: RangeBehaviour::UnknownTotalLength,
        ..spec()
    })
    .await
    .expect("server starts");
    let remote = backend()
        .probe(request(&server.entry_url()))
        .await
        .expect("probes");
    assert!(!remote.range_support.is_proven());
    assert_eq!(
        remote.total_length, None,
        "a 206 with bytes X-Y/* states no total, and Content-Length on a 206 describes only \
         the part — taking it as the whole is an off-by-a-lot"
    );
}

#[tokio::test]
async fn a_200_response_takes_its_length_from_content_length() {
    // The legitimate use of Content-Length: the response *is* the whole representation.
    let server = PathologyServer::start(ServerSpec {
        ranges: RangeBehaviour::Absent,
        ..spec()
    })
    .await
    .expect("server starts");
    let remote = backend()
        .probe(request(&server.entry_url()))
        .await
        .expect("probes");
    assert!(!remote.range_support.is_proven());
    assert_eq!(remote.total_length, Some(SIZE));
}

// ---------------------------------------------------------------- I-5

#[tokio::test]
async fn a_compressed_ranged_response_is_an_error_not_a_downgrade() {
    // The distinction this asserts is the whole of I-5's teeth. A compressed ranged response is
    // NOT "a server without range support" — it is a response whose bytes must never be written
    // at a raw offset. Downgrading it to Absent would let the transfer proceed as a single
    // stream over bytes that do not correspond to the representation, which completes at exactly
    // the right size and is garbage.
    let server = PathologyServer::start(ServerSpec {
        content_encoding: Some("gzip".to_owned()),
        ..spec()
    })
    .await
    .expect("server starts");

    let error = backend()
        .probe(request(&server.entry_url()))
        .await
        .expect_err("a gzipped ranged response must fail the probe, not downgrade it");

    assert_eq!(error.kind(), "unexpected_content_encoding");
    assert!(
        matches!(error, ProbeError::ContentEncoding { .. }),
        "got {error:?}"
    );
}

#[tokio::test]
async fn an_identity_content_encoding_is_not_treated_as_a_pathology() {
    let server = PathologyServer::start(ServerSpec {
        content_encoding: Some("identity".to_owned()),
        ..spec()
    })
    .await
    .expect("server starts");
    let remote = backend()
        .probe(request(&server.entry_url()))
        .await
        .expect("identity is fine");
    assert!(remote.range_support.is_proven());
}

#[tokio::test]
async fn the_probe_sends_accept_encoding_identity() {
    // I-5 begins on the request side. If this header stops being sent, a server that would have
    // compressed becomes free to, and every other I-5 check is then relying on the server's
    // restraint. Asserted on the recorded request, not on our own code.
    let server = PathologyServer::start(spec()).await.expect("server starts");
    let _ = backend()
        .probe(request(&server.entry_url()))
        .await
        .expect("probes");

    let requests = server.requests();
    let probe = requests.first().expect("the probe made a request");
    assert_eq!(probe.header("accept-encoding").as_deref(), Some("identity"));
    assert_eq!(
        probe.header("range").as_deref(),
        Some("bytes=0-0"),
        "the probe is a ranged GET, never a HEAD (docs/03 §2.1 step 2)"
    );
    assert_eq!(probe.method, "GET");
}

// ---------------------------------------------------------------- I-8: the redirect chain

#[tokio::test]
async fn every_redirect_hop_is_recorded_including_the_first() {
    let server = PathologyServer::start(ServerSpec {
        redirect_chain: vec![301, 302, 307],
        ..spec()
    })
    .await
    .expect("server starts");

    let remote = backend()
        .probe(request(&server.entry_url()))
        .await
        .expect("probes");

    // The submitted URL, three hops, and the content: I-8 needs the whole chain persisted,
    // because re-resolving from the original URL on resume is what gets a 403 on a signed URL.
    let chain: Vec<String> = remote
        .redirect_chain
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        chain,
        vec![
            server.url("/hop/0"),
            server.url("/hop/1"),
            server.url("/hop/2"),
            server.url("/content"),
        ]
    );
    assert_eq!(remote.final_url.to_string(), server.url("/content"));
    assert!(
        remote.range_support.is_proven(),
        "the transfer still works after redirects"
    );
}

#[tokio::test]
async fn a_chainless_probe_still_records_the_url_it_started_from() {
    let server = PathologyServer::start(spec()).await.expect("server starts");
    let remote = backend()
        .probe(request(&server.entry_url()))
        .await
        .expect("probes");
    assert_eq!(remote.redirect_chain, vec![remote.final_url.clone()]);
}

#[tokio::test]
async fn a_redirect_loop_is_bounded_rather_than_followed_forever() {
    let server = PathologyServer::start(spec()).await.expect("server starts");
    let mut probe = request(&server.url("/loop"));
    probe.max_redirects = 4;

    let error = backend()
        .probe(probe)
        .await
        .expect_err("a loop must terminate the probe");
    assert_eq!(error.kind(), "too_many_redirects");
    assert!(
        matches!(error, ProbeError::TooManyRedirects { limit: 4, .. }),
        "got {error:?}"
    );
    assert!(
        server.request_count() <= 6,
        "the hop limit must actually bound the requests, got {}",
        server.request_count()
    );
}

// ---------------------------------------------------------------- session pathologies

#[tokio::test]
async fn an_html_page_where_a_binary_was_expected_is_a_session_problem() {
    // docs/03 §2.1 step 7. Answered 200, so nothing but inspecting the content type catches it.
    // Reported as a session problem because that is what it is: the fix is to supply the
    // browser's context, not to retry.
    let server = PathologyServer::start(ServerSpec {
        content_type: Some("text/html; charset=utf-8".to_owned()),
        body_override: Some(b"<html><body>Please log in</body></html>".to_vec()),
        ..spec()
    })
    .await
    .expect("server starts");

    let error = backend()
        .probe(request(&server.entry_url()))
        .await
        .expect_err("an HTML login page is not a successful probe");
    assert_eq!(error.kind(), "looks_like_error_page");
}

#[tokio::test]
async fn an_expired_or_forbidden_url_asks_for_a_refresh_rather_than_failing() {
    // I-8: 401/403/410 mean the URL or credential went stale. The engine must keep every byte it
    // holds and ask for a fresh URL, never restart from zero.
    for status in [401_u16, 403, 410] {
        let server = PathologyServer::start(ServerSpec {
            status_override: Some(status),
            ..spec()
        })
        .await
        .expect("server starts");
        let error = backend()
            .probe(request(&server.entry_url()))
            .await
            .expect_err("these are not successful probes");
        assert_eq!(error.kind(), "needs_refresh", "status {status}");
        assert!(
            matches!(error, ProbeError::NeedsRefresh { .. }),
            "status {status}: {error:?}"
        );
    }
}

#[tokio::test]
async fn a_server_error_is_distinguished_from_a_stale_url() {
    for status in [500_u16, 503] {
        let server = PathologyServer::start(ServerSpec {
            status_override: Some(status),
            ..spec()
        })
        .await
        .expect("server starts");
        let error = backend()
            .probe(request(&server.entry_url()))
            .await
            .expect_err("not a probe");
        assert_eq!(
            error.kind(),
            "unexpected_status",
            "status {status} is a server problem, not a stale URL — conflating them would send \
             the user to refresh a URL that is fine"
        );
    }
}

// ---------------------------------------------------------------- recorded metadata

#[tokio::test]
async fn a_strong_validator_is_recorded_and_a_weak_one_is_not() {
    // I-3 at the point of capture.
    let strong = PathologyServer::start(ServerSpec {
        etag: Some("\"v1\"".to_owned()),
        ..spec()
    })
    .await
    .expect("server starts");
    let remote = backend()
        .probe(request(&strong.entry_url()))
        .await
        .expect("probes");
    assert_eq!(remote.validator, Validator::StrongETag("\"v1\"".to_owned()));
    assert!(remote.validator.is_strong());

    let weak = PathologyServer::start(ServerSpec {
        etag: Some("W/\"v1\"".to_owned()),
        last_modified: Some("Mon, 03 Aug 2026 10:00:00 GMT".to_owned()),
        ..spec()
    })
    .await
    .expect("server starts");
    let remote = backend()
        .probe(request(&weak.entry_url()))
        .await
        .expect("probes");
    assert_eq!(
        remote.validator,
        Validator::LastModified("Mon, 03 Aug 2026 10:00:00 GMT".to_owned()),
        "a weak ETag must not be recorded as usable — it can compare equal across \
         representations that differ byte for byte, which is what splices two versions together"
    );
    assert!(!remote.validator.is_strong());
}

#[tokio::test]
async fn a_server_digest_is_captured_when_offered() {
    let server = PathologyServer::start(ServerSpec {
        digest: Some(downpour_corpus::server::DigestSpec::Literal(
            "sha-256=:BBBB:".to_owned(),
        )),
        ..spec()
    })
    .await
    .expect("server starts");
    let remote = backend()
        .probe(request(&server.entry_url()))
        .await
        .expect("probes");
    let digest = remote
        .digest
        .expect("the digest was offered, so it must be captured");
    assert_eq!(digest.encoded, "BBBB");
}

#[tokio::test]
async fn a_filename_is_resolved_from_content_disposition_and_sanitised() {
    let server = PathologyServer::start(ServerSpec {
        content_disposition: Some("attachment; filename=\"../../etc/passwd\"".to_owned()),
        ..spec()
    })
    .await
    .expect("server starts");
    let remote = backend()
        .probe(request(&server.entry_url()))
        .await
        .expect("probes");
    assert_eq!(
        remote.suggested_filename.as_deref(),
        Some("passwd"),
        "a traversal in the header must not survive into the suggested name"
    );
}

#[tokio::test]
async fn a_filename_falls_back_to_the_url_path() {
    let server = PathologyServer::start(spec()).await.expect("server starts");
    let remote = backend()
        .probe(request(&server.entry_url()))
        .await
        .expect("probes");
    assert_eq!(remote.suggested_filename.as_deref(), Some("content"));
}

// ---------------------------------------------------------------- protocols and framing

#[tokio::test]
async fn the_probe_works_over_http2_and_reports_it() {
    let server = PathologyServer::start(ServerSpec {
        protocol: Protocol::H2c,
        ..spec()
    })
    .await
    .expect("server starts");
    let remote = h2_backend()
        .probe(request(&server.entry_url()))
        .await
        .expect("probes over h2c");

    assert_eq!(remote.protocol, NegotiatedProtocol::Http2);
    assert!(remote.range_support.is_proven());
    assert_eq!(remote.total_length, Some(SIZE));
}

#[tokio::test]
async fn a_chunked_response_still_probes() {
    // Chunked framing carries no Content-Length, but the probe's length comes from the validated
    // Content-Range, so it must be unaffected.
    let server = PathologyServer::start(ServerSpec {
        framing: Framing::Chunked,
        ..spec()
    })
    .await
    .expect("server starts");
    let remote = backend()
        .probe(request(&server.entry_url()))
        .await
        .expect("probes");
    assert!(remote.range_support.is_proven());
    assert_eq!(remote.total_length, Some(SIZE));
}
