//! The production HTTP/1.1 + HTTP/2 backend, over `reqwest`/`rustls`.
//!
//! `docs/03-transfer-engine-spec.md` §6, backend `h1h2`. Two choices here are load-bearing:
//!
//! **Redirects are followed by hand**, with `reqwest`'s own policy disabled. `reqwest` would
//! follow them and hand back only the final response, but I-8 requires the *whole chain* to be
//! captured and persisted: signed URLs, session-bound CDNs and hotlink protection all mean that
//! re-resolving from the original URL on resume produces a 403, and the naive reaction to that is
//! to throw away every byte already fetched.
//!
//! **No decompression features are enabled** (see `Cargo.toml`). A transparently decompressed
//! body would mean the bytes the engine writes are not the bytes the byte range referred to,
//! which is I-5 — a download that completes at exactly the right size and is garbage.

use std::time::Duration;

use async_trait::async_trait;
use downpour_types::{
    ByteRangeSpec, ContentDigest, ContentRange, NegotiatedProtocol, RangeProof, RangeSupport,
    RemoteObject, Validator, filename,
};
use url::Url;

use crate::error::{ProbeError, TransferError};
use crate::protocol::{
    BackendCapabilities, ProbeRequest, RangeOutcome, RangeRequest, TransferProtocol,
};
use crate::sink::RangeSink;

/// How the backend is allowed to reach the origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransportMode {
    /// Negotiate over ALPN: HTTP/2 when the origin offers it, HTTP/1.1 otherwise. What
    /// production uses.
    #[default]
    Negotiated,
    /// HTTP/1.1 only.
    Http1Only,
    /// Assume HTTP/2 without negotiating — h2c, cleartext with prior knowledge. Needed for the
    /// corpus, which is deliberately TLS-free (`docs/09-testing-strategy.md` §7), so there is no
    /// ALPN to negotiate over.
    Http2PriorKnowledge,
}

/// The `h1h2` backend.
pub struct H1H2Backend {
    client: reqwest::Client,
    mode: TransportMode,
}

impl H1H2Backend {
    /// Build a backend with the given transport mode.
    ///
    /// # Errors
    ///
    /// If the TLS backend or the system proxy configuration cannot be initialised.
    pub fn new(mode: TransportMode) -> Result<Self, BackendBuildError> {
        let mut builder = reqwest::Client::builder()
            // I-8: the chain is captured by following it ourselves. See the module note.
            .redirect(reqwest::redirect::Policy::none())
            .use_rustls_tls();

        builder = match mode {
            TransportMode::Negotiated => builder,
            TransportMode::Http1Only => builder.http1_only(),
            TransportMode::Http2PriorKnowledge => builder.http2_prior_knowledge(),
        };

        let client = builder
            .build()
            .map_err(|source| BackendBuildError { source })?;
        Ok(Self { client, mode })
    }

    /// Send one request, applying the headers a Downpour request always carries.
    async fn send(
        &self,
        url: &Url,
        range: Option<ByteRangeSpec>,
        if_range: Option<&str>,
        headers: &[(String, String)],
        timeout: Duration,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let mut request = self.client.get(url.clone()).timeout(timeout);

        // I-5, at the point where it is cheapest to get right: every request that carries a
        // Range asks for no encoding at all, so the bytes on the wire are the bytes the range
        // refers to. Sent unconditionally rather than only on ranged requests, because a
        // whole-file fetch that arrives compressed also has a length that disagrees with the
        // representation.
        request = request.header(reqwest::header::ACCEPT_ENCODING, "identity");
        if let Some(range) = range {
            request = request.header(reqwest::header::RANGE, range.header_value());
        }
        // I-3. Sent with every resume, so the server — not us — decides whether the bytes we
        // already hold still belong to the representation it is about to serve.
        if let Some(validator) = if_range {
            request = request.header(reqwest::header::IF_RANGE, validator);
        }
        for (name, value) in headers {
            request = request.header(name, value);
        }
        request.send().await
    }
}

/// The backend could not be constructed.
#[derive(Debug, thiserror::Error)]
#[error("could not build the h1h2 backend: {source}")]
pub struct BackendBuildError {
    #[source]
    source: reqwest::Error,
}

fn header_value(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(std::borrow::ToOwned::to_owned)
}

fn protocol_of(response: &reqwest::Response) -> NegotiatedProtocol {
    match response.version() {
        reqwest::Version::HTTP_2 => NegotiatedProtocol::Http2,
        reqwest::Version::HTTP_3 => NegotiatedProtocol::Http3,
        _ => NegotiatedProtocol::Http11,
    }
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// Whether a content type is a web page rather than the file that was asked for.
///
/// `docs/03-transfer-engine-spec.md` §2.1 step 7. A login wall answered with `200 text/html` is
/// one of the most common reasons a bare-URL download "succeeds" and produces a useless file.
fn looks_like_a_web_page(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|value| {
        let value = value.trim().to_ascii_lowercase();
        value.starts_with("text/html") || value.starts_with("application/xhtml+xml")
    })
}

/// Whether a body's first bytes look like an HTML document when the headers claimed otherwise.
///
/// The magic-byte half of `docs/03-transfer-engine-spec.md` §2.1 step 7 — the text says the check is
/// on "content-type, **or** magic bytes", and [`looks_like_a_web_page`] only covers the first. This
/// catches what that one misses: a login page served as `application/octet-stream`, which is what an
/// expired session looks like on an origin that sets the type from the file it *meant* to serve.
///
/// Deliberately conditional on the declared type being absent or generic. An explicit `text/html` is
/// already refused at the probe, and firing on a type we have no reason to doubt would reject
/// legitimate files whose first byte happens to be `<`.
fn body_looks_like_a_web_page(content_type: Option<&str>, head: &[u8]) -> bool {
    let declared_generic = match content_type {
        None => true,
        Some(value) => {
            let value = value.trim().to_ascii_lowercase();
            value.starts_with("application/octet-stream")
                || value.starts_with("binary/octet-stream")
                || value.starts_with("application/binary")
        }
    };
    if !declared_generic {
        return false;
    }

    // Skip a UTF-8 BOM and leading whitespace: error pages routinely have both, and a sniff that
    // insisted the document begin at byte zero would miss them.
    let mut rest = head.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(head);
    while let [first, tail @ ..] = rest {
        if first.is_ascii_whitespace() {
            rest = tail;
        } else {
            break;
        }
    }

    let prefix: Vec<u8> = rest.iter().take(64).map(u8::to_ascii_lowercase).collect();
    const MARKERS: [&[u8]; 5] = [
        b"<!doctype html",
        b"<html",
        b"<head",
        b"<body",
        b"<!doctype",
    ];
    MARKERS.iter().any(|marker| prefix.starts_with(marker))
}

#[async_trait]
impl TransferProtocol for H1H2Backend {
    async fn probe(&self, request: ProbeRequest) -> Result<RemoteObject, ProbeError> {
        // The probe is a ranged GET, never a HEAD. Servers routinely answer HEAD differently
        // from GET, so only a GET observation is authoritative (docs/03 §2.1 step 2).
        const PROBE_RANGE: ByteRangeSpec = ByteRangeSpec::FromTo { first: 0, last: 0 };

        let start = request.url.clone();
        let mut current = request.url.clone();
        let mut chain = vec![current.clone()];
        let mut hops = 0_u32;
        // Cleared, once, if the server reports the probe range itself as unsatisfiable.
        let mut probe_with_range = true;

        loop {
            if hops > request.max_redirects {
                return Err(ProbeError::TooManyRedirects {
                    start,
                    limit: request.max_redirects,
                });
            }
            let response = self
                .send(
                    &current,
                    if probe_with_range {
                        Some(PROBE_RANGE)
                    } else {
                        None
                    },
                    // The probe never resumes, so it never conditions on a validator.
                    None,
                    &request.headers,
                    request.timeout,
                )
                .await
                .map_err(|source| {
                    if source.is_timeout() {
                        ProbeError::Timeout {
                            url: current.clone(),
                        }
                    } else {
                        ProbeError::Transport {
                            url: current.clone(),
                            source: Box::new(source),
                        }
                    }
                })?;

            let status = response.status().as_u16();

            if is_redirect(status) {
                let location = header_value(&response, "location").ok_or(
                    ProbeError::RedirectWithoutLocation {
                        url: current.clone(),
                        status,
                    },
                )?;
                // Resolved against the current URL, so a relative Location works.
                let next =
                    current
                        .join(&location)
                        .map_err(|_| ProbeError::UnusableRedirectTarget {
                            url: current.clone(),
                            location: location.clone(),
                        })?;
                current = next.clone();
                chain.push(next);
                hops += 1;
                continue;
            }

            // A 416 means the *probe range* was unsatisfiable, not that the representation is
            // unusable. An empty representation has no byte 0, and some servers refuse
            // `bytes=0-0` outright. Either way ranges are not usable here, but the file may be
            // perfectly downloadable — docs/03 §2.1 says a non-206 means "single stream", not
            // "fail". So drop the Range header and ask once more.
            if status == 416 && probe_with_range {
                tracing::debug!(
                    url = %current,
                    "server reports bytes=0-0 unsatisfiable; re-probing without a Range"
                );
                probe_with_range = false;
                continue;
            }

            // 401/403/410 are not failures. They mean the URL or the credential went stale, and
            // the engine's correct response is to ask for a fresh one while keeping every byte it
            // already holds (I-8) — not to restart from zero.
            if matches!(status, 401 | 403 | 410) {
                return Err(ProbeError::NeedsRefresh {
                    url: current,
                    status,
                });
            }
            if !matches!(status, 200 | 206) {
                let retry_after = header_value(&response, "retry-after");
                return Err(ProbeError::UnexpectedStatus {
                    url: current,
                    status,
                    retry_after,
                });
            }

            let content_type = header_value(&response, "content-type");
            if looks_like_a_web_page(content_type.as_deref()) {
                return Err(ProbeError::LooksLikeAnErrorPage {
                    url: current,
                    content_type: content_type.unwrap_or_default(),
                });
            }

            let content_range = header_value(&response, "content-range");
            let content_encoding = header_value(&response, "content-encoding");
            let content_length = header_value(&response, "content-length")
                .and_then(|value| value.parse::<u64>().ok());
            let protocol = protocol_of(&response);
            let validator = Validator::from_headers(
                header_value(&response, "etag").as_deref(),
                header_value(&response, "last-modified").as_deref(),
            );
            let digest = header_value(&response, "repr-digest")
                .or_else(|| header_value(&response, "content-digest"))
                .as_deref()
                .and_then(ContentDigest::parse);
            let suggested_filename = header_value(&response, "content-disposition");

            // The body has to be read before anything can be concluded: the pathology where a
            // server sets `206` and a correct `Content-Range` and then streams the whole
            // representation is invisible from the headers alone.
            let body = response
                .bytes()
                .await
                .map_err(|source| ProbeError::Transport {
                    url: current.clone(),
                    source: Box::new(source),
                })?;
            let body_len = u64::try_from(body.len()).unwrap_or(u64::MAX);

            // A rangeless probe cannot prove anything about ranges, but it still must not accept
            // an encoded body: the length would disagree with the representation and the
            // verification before rename (I-4) would compare against the wrong size.
            if !probe_with_range {
                if let Some(encoding) = content_encoding.as_deref()
                    && !encoding.trim().is_empty()
                    && !encoding.trim().eq_ignore_ascii_case("identity")
                {
                    return Err(ProbeError::ContentEncoding {
                        url: current,
                        source: downpour_types::RangeProofError::UnexpectedContentEncoding {
                            encoding: encoding.trim().to_owned(),
                        },
                    });
                }
                let final_url = current.clone();
                let resolved_name = filename::resolve(suggested_filename.as_deref(), &final_url);
                return Ok(RemoteObject {
                    suggested_filename: Some(resolved_name),
                    final_url,
                    redirect_chain: chain,
                    total_length: content_length,
                    range_support: RangeSupport::Absent,
                    validator,
                    digest,
                    protocol,
                    content_type: content_type.and_then(|value| value.parse().ok()),
                    probed_at: std::time::SystemTime::now(),
                });
            }

            // I-6: this is the *only* place `Proven` can come from, and it is a pure function
            // over what was observed. Everything else is `Absent`.
            let range_support = match RangeProof::from_observed_response(
                PROBE_RANGE,
                status,
                content_range.as_deref(),
                content_encoding.as_deref(),
                body_len,
            ) {
                Ok(proof) => RangeSupport::Proven(proof),
                // I-5 is reported rather than downgraded. A compressed response is not "a server
                // without range support", it is a response whose bytes must not be written, and
                // conflating the two would silently permit the corruption.
                Err(error @ downpour_types::RangeProofError::UnexpectedContentEncoding { .. }) => {
                    return Err(ProbeError::ContentEncoding {
                        url: current,
                        source: error,
                    });
                }
                Err(reason) => {
                    tracing::debug!(
                        url = %current,
                        status,
                        reason = %reason,
                        "range support not proven; falling back to a single stream"
                    );
                    RangeSupport::Absent
                }
            };

            // A validated Content-Range is authoritative. Content-Length is only usable when the
            // response was the whole representation — on a 206 it describes the part, not the
            // whole, and treating it as the total is a classic off-by-a-lot.
            let total_length =
                range_support
                    .total_length()
                    .or(if status == 200 { content_length } else { None });

            let final_url = current.clone();
            // `resolve` already falls back from Content-Disposition to the URL path to
            // "download", so this is always Some. The field stays an Option because the spec
            // types it that way and S7 will carry a name the browser supplied instead.
            let resolved_name = filename::resolve(suggested_filename.as_deref(), &final_url);

            return Ok(RemoteObject {
                suggested_filename: Some(resolved_name),
                final_url,
                redirect_chain: chain,
                total_length,
                range_support,
                validator,
                digest,
                protocol,
                content_type: content_type.and_then(|value| value.parse().ok()),
                probed_at: std::time::SystemTime::now(),
            });
        }
    }

    async fn fetch_range(
        &self,
        request: RangeRequest,
        sink: &mut RangeSink,
    ) -> Result<RangeOutcome, TransferError> {
        let mut response = self
            .send(
                &request.url,
                request.range,
                request.if_range.as_deref(),
                &request.headers,
                request.timeout,
            )
            .await
            .map_err(|source| {
                if source.is_timeout() {
                    TransferError::Timeout {
                        url: request.url.clone(),
                        bytes_delivered: 0,
                    }
                } else {
                    TransferError::Transport {
                        url: request.url.clone(),
                        source: Box::new(source),
                    }
                }
            })?;

        let status = response.status().as_u16();
        // I-3, checked before the body is touched. `If-Range` means "this range only if the
        // representation still matches"; anything but a 206 is the server saying it does not.
        // Neither writing the body at the resume offset nor restarting over the existing bytes
        // is acceptable — both splice two versions at exactly the expected size.
        if let Some(validator) = &request.if_range
            && status != 206
        {
            return Err(TransferError::ValidatorMismatch {
                url: request.url.clone(),
                resume_offset: request.range.map_or(0, |range| match range {
                    ByteRangeSpec::From { first } => first,
                    ByteRangeSpec::FromTo { first, .. } => first,
                    ByteRangeSpec::Suffix { .. } => 0,
                }),
                status,
                validator: validator.clone(),
            });
        }
        if !matches!(status, 200 | 206) {
            let retry_after = header_value(&response, "retry-after");
            return Err(TransferError::UnexpectedStatus {
                url: request.url,
                status,
                retry_after,
            });
        }
        let declared_type = header_value(&response, "content-type");

        let protocol = protocol_of(&response);
        let declared_length =
            header_value(&response, "content-length").and_then(|value| value.parse::<u64>().ok());
        let content_encoding = header_value(&response, "content-encoding");
        let content_range_header = header_value(&response, "content-range");

        // Validated before the first byte is accepted, so a bad response never reaches the file.
        // This is the ordering I-5 actually requires: checking after writing would already have
        // corrupted the target.
        let content_range = match request.range {
            Some(requested) => {
                let proof = RangeProof::from_observed_response(
                    requested,
                    status,
                    content_range_header.as_deref(),
                    content_encoding.as_deref(),
                    // The body length cannot be known yet, so it is asserted to match and
                    // re-checked against what actually arrives below.
                    content_range_header
                        .as_deref()
                        .and_then(|header| header.parse::<ContentRange>().ok())
                        .and_then(|parsed| parsed.len())
                        .unwrap_or_default(),
                )
                .map_err(|source| TransferError::UnusableRangeResponse {
                    url: request.url.clone(),
                    source,
                })?;
                Some(proof.observed_range())
            }
            None => {
                // A whole-representation fetch still must not arrive encoded: the length would
                // disagree with the representation and the verification before rename (I-4)
                // would compare against the wrong size.
                if let Some(encoding) = content_encoding.as_deref()
                    && !encoding.trim().is_empty()
                    && !encoding.trim().eq_ignore_ascii_case("identity")
                {
                    return Err(TransferError::UnusableRangeResponse {
                        url: request.url.clone(),
                        source: downpour_types::RangeProofError::UnexpectedContentEncoding {
                            encoding: encoding.trim().to_owned(),
                        },
                    });
                }
                None
            }
        };

        // What the response committed to delivering. Computed before the body is read so that a
        // stream which dies part way can be reported as a truncation rather than as an opaque
        // transport failure — the distinction drives the retry policy in docs/03 §7.
        let promised = content_range
            .and_then(|range| range.len())
            .or(declared_length);

        let mut delivered = 0_u64;
        loop {
            let chunk = response.chunk().await.map_err(|source| {
                if source.is_timeout() {
                    TransferError::Timeout {
                        url: request.url.clone(),
                        bytes_delivered: delivered,
                    }
                } else if let Some(expected) = promised.filter(|expected| delivered < *expected) {
                    // Derived from the length we already knew rather than from the shape of the
                    // library's error, which would be fragile and would change under us.
                    TransferError::TruncatedBody {
                        url: request.url.clone(),
                        expected,
                        delivered,
                    }
                } else {
                    TransferError::Transport {
                        url: request.url.clone(),
                        source: Box::new(source),
                    }
                }
            })?;
            let Some(chunk) = chunk else { break };
            if chunk.is_empty() {
                continue;
            }

            // Checked on the first bytes and BEFORE they reach the sink, so a login page is never
            // written. The probe cannot do this: its body is one byte, far too little to sniff.
            if delivered == 0 && body_looks_like_a_web_page(declared_type.as_deref(), &chunk) {
                return Err(TransferError::LooksLikeAnErrorPage {
                    url: request.url.clone(),
                    declared_type: declared_type.unwrap_or_default(),
                });
            }

            sink.accept(&chunk).await.map_err(|source| match source {
                crate::sink::SinkError::BeyondGrant { .. } => TransferError::OverDelivery {
                    url: request.url.clone(),
                    source,
                },
                other => TransferError::Sink {
                    url: request.url.clone(),
                    source: other,
                },
            })?;
            delivered = delivered.saturating_add(u64::try_from(chunk.len()).unwrap_or(0));
        }

        // A body that stopped short of what was declared is reported, not silently accepted. The
        // caller returns the unwritten remainder to the allocator (docs/03 §7); treating it as
        // complete is how a short file ends up believed whole.
        let expected = content_range
            .and_then(|range| range.len())
            .or(declared_length);
        let truncated = expected.is_some_and(|expected| delivered < expected);

        Ok(RangeOutcome {
            bytes_delivered: delivered,
            status,
            content_range,
            protocol,
            truncated,
        })
    }

    fn capabilities(&self) -> BackendCapabilities {
        let protocols = match self.mode {
            TransportMode::Negotiated => {
                vec![NegotiatedProtocol::Http11, NegotiatedProtocol::Http2]
            }
            TransportMode::Http1Only => vec![NegotiatedProtocol::Http11],
            TransportMode::Http2PriorKnowledge => vec![NegotiatedProtocol::Http2],
        };
        BackendCapabilities {
            name: "h1h2",
            multiplexes_streams: protocols.contains(&NegotiatedProtocol::Http2),
            protocols,
            supports_ranges: true,
        }
    }
}
