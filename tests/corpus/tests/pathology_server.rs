//! S1-T6 — the pathology server.
//!
//! These tests verify the *server*, not the engine. A test server that quietly serves correct
//! bytes when a case asked it to misbehave turns every corpus case built on it into a false
//! green, so each pathology is asserted here against a reference client before any engine code
//! relies on it.
//!
//! The reference client is `hyper` for HTTP/2 and a hand-written reader for HTTP/1.1 —
//! deliberately not `downpour-http`. Verifying our server with our client would be circular,
//! and the HTTP/1.1 pathologies are wire-level, so they need a reader that reports exactly what
//! arrived rather than one that tries to make sense of it.

use downpour_corpus::content::Content;
use downpour_corpus::server::{Framing, PathologyServer, Protocol, RangeBehaviour, ServerSpec};

mod client;
use client::{h1_request, h2_get};

const SIZE: u64 = 256 * 1024;

fn spec() -> ServerSpec {
    ServerSpec {
        content: Content::new(42, SIZE),
        ..ServerSpec::default()
    }
}

// ---------------------------------------------------------------- ground truth

#[tokio::test]
async fn serves_exactly_the_generated_content_over_http11() {
    let server = PathologyServer::start(spec()).await.expect("server starts");
    let response = h1_request(server.addr(), "GET", &server.entry_path(), &[]).await;

    assert_eq!(response.status, 200);
    assert_eq!(response.body.len(), usize::try_from(SIZE).expect("fits"));
    let content = Content::new(42, SIZE);
    assert_eq!(
        content.first_mismatch(0, &response.body),
        None,
        "the server did not serve the generator's bytes; every case built on it would be a lie"
    );
}

#[tokio::test]
async fn serves_exactly_the_generated_content_over_http2() {
    let server = PathologyServer::start(ServerSpec {
        protocol: Protocol::H2c,
        ..spec()
    })
    .await
    .expect("server starts");

    let response = h2_get(server.addr(), &server.entry_path(), &[]).await;
    assert_eq!(response.status, 200);
    let content = Content::new(42, SIZE);
    assert_eq!(content.first_mismatch(0, &response.body), None);
}

#[tokio::test]
async fn both_protocols_serve_identical_bytes() {
    // The same case run over h1 and h2 must produce the same file, or S1-C1 cannot be stated
    // as a single expectation across both.
    let h1 = PathologyServer::start(spec()).await.expect("server starts");
    let h2 = PathologyServer::start(ServerSpec {
        protocol: Protocol::H2c,
        ..spec()
    })
    .await
    .expect("server starts");

    let over_h1 = h1_request(h1.addr(), "GET", &h1.entry_path(), &[]).await;
    let over_h2 = h2_get(h2.addr(), &h2.entry_path(), &[]).await;
    assert_eq!(over_h1.body, over_h2.body);
}

#[tokio::test]
async fn two_independent_servers_with_the_same_seed_agree() {
    // Reproducibility at the server level: a failing case must reproduce on another machine.
    let a = PathologyServer::start(spec()).await.expect("server starts");
    let b = PathologyServer::start(spec()).await.expect("server starts");
    let from_a = h1_request(a.addr(), "GET", &a.entry_path(), &[]).await;
    let from_b = h1_request(b.addr(), "GET", &b.entry_path(), &[]).await;
    assert_eq!(from_a.body, from_b.body);
}

// ---------------------------------------------------------------- ranges

#[tokio::test]
async fn a_supported_range_is_honoured_exactly() {
    let server = PathologyServer::start(spec()).await.expect("server starts");
    let response = h1_request(
        server.addr(),
        "GET",
        &server.entry_path(),
        &[("Range", "bytes=1000-1099")],
    )
    .await;

    assert_eq!(response.status, 206);
    assert_eq!(
        response.header("content-range").as_deref(),
        Some("bytes 1000-1099/262144")
    );
    assert_eq!(response.body.len(), 100);
    let content = Content::new(42, SIZE);
    assert_eq!(content.first_mismatch(1000, &response.body), None);
}

#[tokio::test]
async fn the_probe_range_returns_exactly_one_byte() {
    // `Range: bytes=0-0` is the capability probe (docs/03 §2.1). Getting this wrong on the
    // server side would make every ranges case meaningless.
    let server = PathologyServer::start(spec()).await.expect("server starts");
    let response = h1_request(
        server.addr(),
        "GET",
        &server.entry_path(),
        &[("Range", "bytes=0-0")],
    )
    .await;

    assert_eq!(response.status, 206);
    assert_eq!(
        response.header("content-range").as_deref(),
        Some("bytes 0-0/262144")
    );
    assert_eq!(response.body.len(), 1);
    assert_eq!(response.body[0], Content::new(42, SIZE).byte_at(0));
}

#[tokio::test]
async fn an_open_ended_range_runs_to_the_end() {
    let server = PathologyServer::start(spec()).await.expect("server starts");
    let response = h1_request(
        server.addr(),
        "GET",
        &server.entry_path(),
        &[("Range", "bytes=262100-")],
    )
    .await;
    assert_eq!(response.status, 206);
    assert_eq!(response.body.len(), 44);
    assert_eq!(
        response.header("content-range").as_deref(),
        Some("bytes 262100-262143/262144")
    );
}

#[tokio::test]
async fn a_suffix_range_returns_the_tail() {
    let server = PathologyServer::start(spec()).await.expect("server starts");
    let response = h1_request(
        server.addr(),
        "GET",
        &server.entry_path(),
        &[("Range", "bytes=-100")],
    )
    .await;
    assert_eq!(response.status, 206);
    assert_eq!(response.body.len(), 100);
    assert_eq!(
        response.header("content-range").as_deref(),
        Some("bytes 262044-262143/262144")
    );
}

#[tokio::test]
async fn accept_ranges_absent_means_a_range_request_gets_the_whole_body() {
    let server = PathologyServer::start(ServerSpec {
        ranges: RangeBehaviour::Absent,
        ..spec()
    })
    .await
    .expect("server starts");

    let response = h1_request(
        server.addr(),
        "GET",
        &server.entry_path(),
        &[("Range", "bytes=0-0")],
    )
    .await;
    assert_eq!(response.status, 200);
    assert_eq!(response.header("accept-ranges"), None);
    assert_eq!(response.body.len(), usize::try_from(SIZE).expect("fits"));
}

#[tokio::test]
async fn accept_ranges_lies_advertises_support_and_then_ignores_it() {
    // The pathology that makes eight workers download the file eight times.
    let server = PathologyServer::start(ServerSpec {
        ranges: RangeBehaviour::Lies,
        ..spec()
    })
    .await
    .expect("server starts");

    let response = h1_request(
        server.addr(),
        "GET",
        &server.entry_path(),
        &[("Range", "bytes=0-0")],
    )
    .await;
    assert_eq!(response.header("accept-ranges").as_deref(), Some("bytes"));
    assert_eq!(
        response.status, 200,
        "the lie is the 200, after advertising bytes"
    );
    assert_eq!(response.body.len(), usize::try_from(SIZE).expect("fits"));
}

#[tokio::test]
async fn a_206_can_be_served_with_the_whole_body() {
    // Right status, right Content-Range, whole body anyway. Only counting bytes catches it.
    let server = PathologyServer::start(ServerSpec {
        ranges: RangeBehaviour::IgnoreButClaim,
        ..spec()
    })
    .await
    .expect("server starts");

    let response = h1_request(
        server.addr(),
        "GET",
        &server.entry_path(),
        &[("Range", "bytes=0-0")],
    )
    .await;
    assert_eq!(response.status, 206);
    assert_eq!(
        response.header("content-range").as_deref(),
        Some("bytes 0-0/262144")
    );
    assert_eq!(response.body.len(), usize::try_from(SIZE).expect("fits"));
}

#[tokio::test]
async fn a_content_range_can_be_shifted_away_from_the_request() {
    let server = PathologyServer::start(ServerSpec {
        ranges: RangeBehaviour::ShiftedContentRange { by: 4096 },
        ..spec()
    })
    .await
    .expect("server starts");

    let response = h1_request(
        server.addr(),
        "GET",
        &server.entry_path(),
        &[("Range", "bytes=0-0")],
    )
    .await;
    assert_eq!(response.status, 206);
    assert_eq!(
        response.header("content-range").as_deref(),
        Some("bytes 4096-4096/262144")
    );
}

#[tokio::test]
async fn a_206_can_omit_content_range_entirely() {
    let server = PathologyServer::start(ServerSpec {
        ranges: RangeBehaviour::OmitContentRange,
        ..spec()
    })
    .await
    .expect("server starts");

    let response = h1_request(
        server.addr(),
        "GET",
        &server.entry_path(),
        &[("Range", "bytes=0-0")],
    )
    .await;
    assert_eq!(response.status, 206);
    assert_eq!(response.header("content-range"), None);
}

#[tokio::test]
async fn a_literal_malformed_content_range_can_be_served() {
    // The server must be able to emit headers a conforming HTTP library would refuse to
    // produce, which is the reason the HTTP/1.1 side is hand-rolled.
    let server = PathologyServer::start(ServerSpec {
        ranges: RangeBehaviour::LiteralContentRange("bytes 0 - 0/262144".to_owned()),
        ..spec()
    })
    .await
    .expect("server starts");

    let response = h1_request(
        server.addr(),
        "GET",
        &server.entry_path(),
        &[("Range", "bytes=0-0")],
    )
    .await;
    assert_eq!(
        response.header("content-range").as_deref(),
        Some("bytes 0 - 0/262144")
    );
}

#[tokio::test]
async fn a_206_can_report_an_unknown_total_length() {
    let server = PathologyServer::start(ServerSpec {
        ranges: RangeBehaviour::UnknownTotalLength,
        ..spec()
    })
    .await
    .expect("server starts");

    let response = h1_request(
        server.addr(),
        "GET",
        &server.entry_path(),
        &[("Range", "bytes=0-0")],
    )
    .await;
    assert_eq!(
        response.header("content-range").as_deref(),
        Some("bytes 0-0/*")
    );
}

#[tokio::test]
async fn an_unsatisfiable_range_gets_a_416() {
    let server = PathologyServer::start(spec()).await.expect("server starts");
    let response = h1_request(
        server.addr(),
        "GET",
        &server.entry_path(),
        &[("Range", "bytes=999999999-")],
    )
    .await;
    assert_eq!(response.status, 416);
    assert_eq!(
        response.header("content-range").as_deref(),
        Some("bytes */262144")
    );
}

// ---------------------------------------------------------------- framing

#[tokio::test]
async fn chunked_framing_carries_the_same_bytes_and_no_content_length() {
    let server = PathologyServer::start(ServerSpec {
        framing: Framing::Chunked,
        ..spec()
    })
    .await
    .expect("server starts");

    let response = h1_request(server.addr(), "GET", &server.entry_path(), &[]).await;
    assert_eq!(response.header("content-length"), None);
    assert_eq!(
        response.header("transfer-encoding").as_deref(),
        Some("chunked")
    );
    assert_eq!(response.body.len(), usize::try_from(SIZE).expect("fits"));
    assert_eq!(
        Content::new(42, SIZE).first_mismatch(0, &response.body),
        None
    );
}

#[tokio::test]
async fn close_delimited_framing_declares_neither_length_nor_chunking() {
    // The `no length` pathology: the body ends when the connection does, so the client cannot
    // distinguish "complete" from "truncated" by framing alone.
    let server = PathologyServer::start(ServerSpec {
        framing: Framing::CloseDelimited,
        ..spec()
    })
    .await
    .expect("server starts");

    let response = h1_request(server.addr(), "GET", &server.entry_path(), &[]).await;
    assert_eq!(response.header("content-length"), None);
    assert_eq!(response.header("transfer-encoding"), None);
    assert_eq!(response.body.len(), usize::try_from(SIZE).expect("fits"));
}

#[tokio::test]
async fn a_content_length_can_disagree_with_the_body() {
    // Declares more than it sends, then closes. A client that trusts Content-Length and writes
    // whatever arrived produces a short file it believes is complete.
    let server = PathologyServer::start(ServerSpec {
        framing: Framing::WrongContentLength {
            declared: SIZE + 1024,
        },
        ..spec()
    })
    .await
    .expect("server starts");

    let response = h1_request(server.addr(), "GET", &server.entry_path(), &[]).await;
    assert_eq!(response.header("content-length").as_deref(), Some("263168"));
    assert_eq!(
        response.body.len(),
        usize::try_from(SIZE).expect("fits"),
        "the body is short"
    );
}

#[tokio::test]
async fn a_body_can_be_truncated_mid_transfer() {
    let server = PathologyServer::start(ServerSpec {
        truncate_body_after: Some(1024),
        ..spec()
    })
    .await
    .expect("server starts");

    let response = h1_request(server.addr(), "GET", &server.entry_path(), &[]).await;
    assert_eq!(response.header("content-length").as_deref(), Some("262144"));
    assert_eq!(response.body.len(), 1024);
    // What did arrive must still be correct, or a truncation case cannot distinguish "short"
    // from "short and corrupt".
    assert_eq!(
        Content::new(42, SIZE).first_mismatch(0, &response.body),
        None
    );
}

#[tokio::test]
async fn a_content_encoding_can_be_declared_on_a_ranged_response() {
    // `gzip-on-range` (I-5). The header is a lie — the body is not actually compressed — because
    // what is under test is whether the engine refuses to write it, not whether it can inflate.
    let server = PathologyServer::start(ServerSpec {
        content_encoding: Some("gzip".to_owned()),
        ..spec()
    })
    .await
    .expect("server starts");

    let response = h1_request(
        server.addr(),
        "GET",
        &server.entry_path(),
        &[("Range", "bytes=0-99")],
    )
    .await;
    assert_eq!(response.status, 206);
    assert_eq!(response.header("content-encoding").as_deref(), Some("gzip"));
}

// ---------------------------------------------------------------- redirects

#[tokio::test]
async fn a_redirect_chain_is_walked_hop_by_hop() {
    let server = PathologyServer::start(ServerSpec {
        redirect_chain: vec![301, 302, 307],
        ..spec()
    })
    .await
    .expect("server starts");

    // Hop 0 points at hop 1, and so on; the last hop points at the content.
    let first = h1_request(server.addr(), "GET", &server.entry_path(), &[]).await;
    assert_eq!(first.status, 301);
    let second_path = first
        .header("location")
        .expect("a redirect carries Location");

    let second = h1_request(server.addr(), "GET", &second_path, &[]).await;
    assert_eq!(second.status, 302);
    let third_path = second
        .header("location")
        .expect("a redirect carries Location");

    let third = h1_request(server.addr(), "GET", &third_path, &[]).await;
    assert_eq!(third.status, 307);
    let content_path = third
        .header("location")
        .expect("a redirect carries Location");

    let final_response = h1_request(server.addr(), "GET", &content_path, &[]).await;
    assert_eq!(final_response.status, 200);
    assert_eq!(
        final_response.body.len(),
        usize::try_from(SIZE).expect("fits")
    );
}

#[tokio::test]
async fn a_redirect_loop_never_terminates_on_its_own() {
    let server = PathologyServer::start(spec()).await.expect("server starts");
    let response = h1_request(server.addr(), "GET", "/loop", &[]).await;
    assert_eq!(response.status, 302);
    assert_eq!(
        response.header("location").as_deref(),
        Some("/loop"),
        "the loop must point at itself, so only a hop limit can stop it"
    );
}

// ---------------------------------------------------------------- observation

#[tokio::test]
async fn every_request_is_recorded_with_its_headers() {
    // docs/09 §3.3: the server records what it received, so a case can assert on request shape
    // and not only on the outcome. I-5 needs this — "was Accept-Encoding: identity actually
    // sent?" is a question about the request, not the response.
    let server = PathologyServer::start(spec()).await.expect("server starts");
    let _ = h1_request(
        server.addr(),
        "GET",
        &server.entry_path(),
        &[("Range", "bytes=0-0"), ("Accept-Encoding", "identity")],
    )
    .await;

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/content");
    assert_eq!(request.header("range").as_deref(), Some("bytes=0-0"));
    assert_eq!(
        request.header("accept-encoding").as_deref(),
        Some("identity")
    );
}

#[tokio::test]
async fn requests_are_recorded_over_http2_as_well() {
    let server = PathologyServer::start(ServerSpec {
        protocol: Protocol::H2c,
        ..spec()
    })
    .await
    .expect("server starts");

    let _ = h2_get(
        server.addr(),
        &server.entry_path(),
        &[("Range", "bytes=0-0")],
    )
    .await;
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].header("range").as_deref(), Some("bytes=0-0"));
}

#[tokio::test]
async fn a_keep_alive_connection_serves_more_than_one_request() {
    // S3-C3 asserts connection reuse by counting handshakes, which only means something if the
    // server actually supports keep-alive. Verified now, while it is cheap.
    let server = PathologyServer::start(spec()).await.expect("server starts");
    let responses = client::h1_pipeline(
        server.addr(),
        &[
            (
                server.entry_path(),
                vec![("Range".to_owned(), "bytes=0-9".to_owned())],
            ),
            (
                server.entry_path(),
                vec![("Range".to_owned(), "bytes=10-19".to_owned())],
            ),
        ],
    )
    .await;

    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0].status, 206);
    assert_eq!(responses[1].status, 206);
    assert_eq!(
        server.requests().len(),
        2,
        "both requests arrived on one connection"
    );
}

// ---------------------------------------------------------------- metadata

#[tokio::test]
async fn validators_and_disposition_are_served_when_configured() {
    let server = PathologyServer::start(ServerSpec {
        etag: Some("\"v1\"".to_owned()),
        last_modified: Some("Mon, 03 Aug 2026 10:00:00 GMT".to_owned()),
        content_disposition: Some("attachment; filename=\"report.bin\"".to_owned()),
        content_type: Some("application/octet-stream".to_owned()),
        ..spec()
    })
    .await
    .expect("server starts");

    let response = h1_request(server.addr(), "GET", &server.entry_path(), &[]).await;
    assert_eq!(response.header("etag").as_deref(), Some("\"v1\""));
    assert_eq!(
        response.header("last-modified").as_deref(),
        Some("Mon, 03 Aug 2026 10:00:00 GMT")
    );
    assert_eq!(
        response.header("content-disposition").as_deref(),
        Some("attachment; filename=\"report.bin\"")
    );
    assert_eq!(
        response.header("content-type").as_deref(),
        Some("application/octet-stream")
    );
}

#[tokio::test]
async fn an_html_error_page_can_be_served_where_a_binary_was_expected() {
    // docs/03 §2.1 step 7: the "server sends a login page" pathology. It answers 200 with
    // text/html, so only inspecting the content type or the magic bytes catches it.
    let body = b"<html><body>Please log in</body></html>".to_vec();
    let server = PathologyServer::start(ServerSpec {
        content_type: Some("text/html; charset=utf-8".to_owned()),
        body_override: Some(body.clone()),
        ..spec()
    })
    .await
    .expect("server starts");

    let response = h1_request(server.addr(), "GET", &server.entry_path(), &[]).await;
    assert_eq!(response.status, 200);
    assert_eq!(
        response.header("content-type").as_deref(),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(response.body, body);
}

#[tokio::test]
async fn a_status_override_lets_a_case_demand_any_status() {
    for status in [401_u16, 403, 410, 429, 500, 503] {
        let server = PathologyServer::start(ServerSpec {
            status_override: Some(status),
            ..spec()
        })
        .await
        .expect("server starts");
        let response = h1_request(server.addr(), "GET", &server.entry_path(), &[]).await;
        assert_eq!(response.status, status);
    }
}
