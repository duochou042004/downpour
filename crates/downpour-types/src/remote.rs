//! What the capability probe learned about a remote object.
//!
//! This module owns **I-6**: *range support is proven, never assumed.*
//!
//! The enforcement is structural rather than procedural. [`RangeSupport::Proven`] carries a
//! [`RangeProof`]; `RangeProof` has private fields and exactly one public constructor, and
//! that constructor is a pure function that returns `Err` unless it was handed a response
//! that actually validated. There is therefore no expression anywhere in the workspace that
//! produces `Proven` without the evidence, and no amount of optimism in the scheduler can
//! invent one. `Accept-Ranges: bytes` on a `HEAD` cannot reach this type at all.
//!
//! The same constructor enforces the header half of **I-5**: a ranged response carrying a
//! `Content-Encoding` other than `identity` is rejected before any body byte is accepted.

use std::time::SystemTime;

use mime::Mime;
use thiserror::Error;
use url::Url;

use crate::content_range::{ByteRangeSpec, ContentRange, ContentRangeError, RangeMismatch};

/// Evidence that an origin honoured a specific byte-range request.
///
/// Deliberately **not** `Deserialize`. A derived `Deserialize` would be a second constructor
/// that skips the validation, which would turn a compiler-enforced invariant back into a
/// convention. When S2 persists this, it must re-validate on load or store the raw
/// observation and rebuild the proof from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeProof {
    total_length: u64,
    observed: ContentRange,
    requested: ByteRangeSpec,
    observation: RangeObservation,
}

/// The raw HTTP facts from which range support may be proven.
///
/// This type is deliberately harmless to construct: it is only an observation, not a
/// capability. Persisting these fields allows storage to call
/// [`RangeProof::from_observed_response`] again after restart instead of deserializing a proof
/// or trusting a previously computed boolean (I-6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeObservation {
    requested: ByteRangeSpec,
    status: u16,
    content_range: Option<String>,
    content_encoding: Option<String>,
    body_len: u64,
}

impl RangeObservation {
    /// Records one ranged response without claiming that it is valid evidence.
    #[must_use]
    pub const fn new(
        requested: ByteRangeSpec,
        status: u16,
        content_range: Option<String>,
        content_encoding: Option<String>,
        body_len: u64,
    ) -> Self {
        Self {
            requested,
            status,
            content_range,
            content_encoding,
            body_len,
        }
    }

    /// Returns the exact range sent in the request.
    #[must_use]
    pub const fn requested_range(&self) -> ByteRangeSpec {
        self.requested
    }

    /// Returns the observed HTTP status.
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status
    }

    /// Returns the observed `Content-Range` value, when present.
    #[must_use]
    pub fn content_range(&self) -> Option<&str> {
        self.content_range.as_deref()
    }

    /// Returns the observed `Content-Encoding` value, when present.
    #[must_use]
    pub fn content_encoding(&self) -> Option<&str> {
        self.content_encoding.as_deref()
    }

    /// Returns the number of body bytes that accompanied the response.
    #[must_use]
    pub const fn body_len(&self) -> u64 {
        self.body_len
    }
}

impl RangeProof {
    /// The only way to obtain a `RangeProof`, and therefore the only way to reach
    /// [`RangeSupport::Proven`].
    ///
    /// Implements the validation table in `docs/03-transfer-engine-spec.md` §2.1 step 4. The
    /// checks are ordered so that the header-only ones run before the body is considered:
    ///
    /// 1. the status is `206` — a `200` proves nothing, whatever headers came with it;
    /// 2. `Content-Encoding` is absent or `identity` (I-5);
    /// 3. `Content-Range` is present and well formed;
    /// 4. it is consistent with `requested` (I-5);
    /// 5. the total length is known, or there is nothing to segment;
    /// 6. the body length matches the range the header described — this is what catches a
    ///    server that sets the right headers and then streams the whole file anyway.
    ///
    /// ```
    /// use downpour_types::{ByteRangeSpec, RangeProof};
    /// let probe = ByteRangeSpec::FromTo { first: 0, last: 0 };
    ///
    /// let proof = RangeProof::from_observed_response(
    ///     probe, 206, Some("bytes 0-0/1048576"), None, 1,
    /// )?;
    /// assert_eq!(proof.total_length(), 1_048_576);
    ///
    /// // The `accept-ranges-lies` pathology: a 200 in answer to a ranged GET.
    /// assert!(RangeProof::from_observed_response(
    ///     probe, 200, Some("bytes 0-0/1048576"), None, 1,
    /// ).is_err());
    /// # Ok::<(), downpour_types::RangeProofError>(())
    /// ```
    pub fn from_observed_response(
        requested: ByteRangeSpec,
        status: u16,
        content_range: Option<&str>,
        content_encoding: Option<&str>,
        body_len: u64,
    ) -> Result<Self, RangeProofError> {
        let observation = RangeObservation::new(
            requested,
            status,
            content_range.map(str::to_owned),
            content_encoding.map(str::to_owned),
            body_len,
        );
        if status != 206 {
            return Err(RangeProofError::StatusNot206 { status });
        }

        // I-5, checked from the headers before a single body byte is accepted. Ranged
        // requests go out with `Accept-Encoding: identity`, so a compressed answer means the
        // bytes on the wire no longer correspond to the byte range that was asked for.
        if let Some(encoding) = content_encoding {
            let encoding = encoding.trim();
            if !encoding.is_empty() && !encoding.eq_ignore_ascii_case("identity") {
                return Err(RangeProofError::UnexpectedContentEncoding {
                    encoding: encoding.to_owned(),
                });
            }
        }

        let header = content_range.ok_or(RangeProofError::ContentRangeAbsent)?;
        let observed: ContentRange = header.parse()?;
        observed.is_consistent_with(requested)?;

        let total_length = observed
            .complete_length()
            .ok_or(RangeProofError::TotalLengthUnknown)?;
        let expected = observed.len().ok_or(RangeProofError::TotalLengthUnknown)?;
        if body_len != expected {
            return Err(RangeProofError::BodyLengthMismatch {
                expected,
                actual: body_len,
            });
        }

        Ok(Self {
            total_length,
            observed,
            requested,
            observation,
        })
    }

    /// Total length of the representation, taken from the validated `Content-Range`.
    #[must_use]
    pub fn total_length(&self) -> u64 {
        self.total_length
    }

    /// The range the server described.
    #[must_use]
    pub fn observed_range(&self) -> ContentRange {
        self.observed
    }

    /// The range that was requested, which the observation was checked against.
    #[must_use]
    pub fn requested_range(&self) -> ByteRangeSpec {
        self.requested
    }

    /// Returns the raw response facts that produced this proof.
    ///
    /// Storage persists this observation and replays the validating constructor on load;
    /// it never serializes `RangeProof` itself.
    #[must_use]
    pub const fn observation(&self) -> &RangeObservation {
        &self.observation
    }
}

/// Why a response failed to prove range support.
///
/// Every variant is a server pathology from `docs/01-idm-teardown.md` §3, and each one
/// corresponds to a compatibility corpus case.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RangeProofError {
    /// The status was not `206`. `Accept-Ranges: bytes` plus a `200` is the single most
    /// common lie a server tells about range support.
    #[error("status {status} is not 206 Partial Content, so range support is not proven")]
    StatusNot206 {
        /// The status that was actually observed.
        status: u16,
    },
    /// A ranged response carried a body encoding, which invalidates offset arithmetic (I-5).
    #[error("ranged response carries Content-Encoding {encoding:?}; offsets would be wrong")]
    UnexpectedContentEncoding {
        /// The encoding the server applied.
        encoding: String,
    },
    /// A `206` with no `Content-Range` does not say which bytes are in the body.
    #[error("206 response has no Content-Range, so the enclosed bytes are unidentified")]
    ContentRangeAbsent,
    /// The `Content-Range` header did not parse.
    #[error(transparent)]
    ContentRangeMalformed(#[from] ContentRangeError),
    /// The `Content-Range` described a range other than the one requested.
    #[error(transparent)]
    ContentRangeMismatch(#[from] RangeMismatch),
    /// The server did not state the representation length, so there is nothing to segment.
    #[error("the representation length is unknown, so segmentation is not possible")]
    TotalLengthUnknown,
    /// The body length did not match the range the header described. Usually means the
    /// server ignored the `Range` header and streamed the whole representation.
    #[error("body is {actual} bytes but Content-Range described {expected}")]
    BodyLengthMismatch {
        /// Length implied by the validated `Content-Range`.
        expected: u64,
        /// Length actually delivered.
        actual: u64,
    },
}

/// Whether this origin will serve byte ranges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeSupport {
    /// Proven by an observed, validated `206`. Reachable only via [`RangeProof`] (I-6).
    Proven(RangeProof),
    /// Observed not to support ranges. Single stream, and do not ask again this session.
    Absent,
    /// Not yet established. Treated exactly like [`Self::Absent`] by the scheduler; the
    /// distinction exists so that "we never asked" is not logged as "the server refused".
    Unknown,
}

impl RangeSupport {
    /// Whether segmentation is permitted. The scheduler asks this and nothing else.
    #[must_use]
    pub fn is_proven(&self) -> bool {
        matches!(self, Self::Proven(_))
    }

    /// The proven total length, if any.
    #[must_use]
    pub fn total_length(&self) -> Option<u64> {
        match self {
            Self::Proven(proof) => Some(proof.total_length()),
            Self::Absent | Self::Unknown => None,
        }
    }

    /// The underlying evidence, for logging and `dp explain`.
    #[must_use]
    pub fn proof(&self) -> Option<&RangeProof> {
        match self {
            Self::Proven(proof) => Some(proof),
            Self::Absent | Self::Unknown => None,
        }
    }
}

/// The validator recorded for a representation, used with `If-Range` on resume (I-3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Validator {
    /// A strong `ETag`, quoted as it arrived. The only thing that makes resume safe.
    StrongETag(String),
    /// A `Last-Modified` date, used only when the server offers nothing stronger.
    LastModified(String),
    /// Nothing usable. A weak `ETag` (`W/"…"`) lands here deliberately: it may compare equal
    /// across representations that differ byte-for-byte, which is precisely the condition
    /// that splices two versions of a file together.
    None,
}

impl Validator {
    /// Classify the validators a response offered, preferring the strongest.
    ///
    /// ```
    /// use downpour_types::Validator;
    /// // A weak ETag is not a validator we can resume against.
    /// assert_eq!(
    ///     Validator::from_headers(Some("W/\"abc\""), Some("Mon, 3 Aug 2026 10:00:00 GMT")),
    ///     Validator::LastModified("Mon, 3 Aug 2026 10:00:00 GMT".to_owned()),
    /// );
    /// ```
    #[must_use]
    pub fn from_headers(etag: Option<&str>, last_modified: Option<&str>) -> Self {
        if let Some(etag) = etag {
            let etag = etag.trim();
            // "W/" is case-sensitive in RFC 9110 §8.8.3.
            if !etag.is_empty() && !etag.starts_with("W/") && etag.starts_with('"') {
                return Self::StrongETag(etag.to_owned());
            }
        }
        if let Some(last_modified) = last_modified {
            let last_modified = last_modified.trim();
            if !last_modified.is_empty() {
                return Self::LastModified(last_modified.to_owned());
            }
        }
        Self::None
    }

    /// Whether this is a strong validator. Only a strong `ETag` is.
    #[must_use]
    pub fn is_strong(&self) -> bool {
        matches!(self, Self::StrongETag(_))
    }

    /// The value to send in `If-Range` on resume, if any.
    #[must_use]
    pub fn if_range_value(&self) -> Option<&str> {
        match self {
            Self::StrongETag(v) | Self::LastModified(v) => Some(v),
            Self::None => None,
        }
    }
}

/// A digest algorithm we understand from RFC 9530.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestAlgorithm {
    /// `sha-256`.
    Sha256,
    /// `sha-512`.
    Sha512,
}

impl DigestAlgorithm {
    /// The RFC 9530 token for this algorithm.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Sha256 => "sha-256",
            Self::Sha512 => "sha-512",
        }
    }
}

/// A server-supplied content digest (`Repr-Digest` / `Content-Digest`, RFC 9530).
///
/// Free end-to-end integrity when a server offers it. **S1 captures it only** — verification
/// belongs to the storage layer in S2, which owns I-4. The base64 payload is kept exactly as
/// it arrived so that nothing is lost in the meantime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentDigest {
    /// Which algorithm the server used.
    pub algorithm: DigestAlgorithm,
    /// The base64 payload, as received, without the surrounding colons.
    pub encoded: String,
}

impl ContentDigest {
    /// Parse a `Repr-Digest` or `Content-Digest` header, taking the strongest algorithm we
    /// understand and ignoring the rest.
    ///
    /// The value is an RFC 8941 dictionary of byte-sequence members, e.g.
    /// `sha-256=:4REjxQ4yrqk+ceD5N7Ur9w==:`. Unknown algorithms are skipped rather than
    /// treated as an error: a digest we cannot check is no worse than no digest at all.
    ///
    /// ```
    /// use downpour_types::{ContentDigest, DigestAlgorithm};
    /// let d = ContentDigest::parse(r#"sha-512=:AAAA:, sha-256=:BBBB:"#)
    ///     .expect("both members are understood");
    /// assert_eq!(d.algorithm, DigestAlgorithm::Sha512);
    /// assert_eq!(d.encoded, "AAAA");
    /// ```
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let mut best: Option<Self> = None;
        for member in raw.split(',') {
            let Some((key, value)) = member.split_once('=') else {
                continue;
            };
            let algorithm = match key.trim().to_ascii_lowercase().as_str() {
                "sha-256" => DigestAlgorithm::Sha256,
                "sha-512" => DigestAlgorithm::Sha512,
                _ => continue,
            };
            let value = value.trim();
            let Some(encoded) = value.strip_prefix(':').and_then(|v| v.strip_suffix(':')) else {
                continue;
            };
            if encoded.is_empty() {
                continue;
            }
            let candidate = Self {
                algorithm,
                encoded: encoded.to_owned(),
            };
            let stronger = match &best {
                Some(current) => {
                    algorithm == DigestAlgorithm::Sha512
                        && current.algorithm == DigestAlgorithm::Sha256
                }
                None => true,
            };
            if stronger {
                best = Some(candidate);
            }
        }
        best
    }
}

/// Which HTTP version the connection negotiated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegotiatedProtocol {
    /// HTTP/1.1.
    Http11,
    /// HTTP/2.
    Http2,
    /// HTTP/3 over QUIC. Feature-gated and off by default until S6 (ADR-0005).
    Http3,
}

impl std::fmt::Display for NegotiatedProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Http11 => "HTTP/1.1",
            Self::Http2 => "HTTP/2",
            Self::Http3 => "HTTP/3",
        })
    }
}

/// The output of the capability probe: everything the engine needs to plan a transfer.
///
/// `docs/03-transfer-engine-spec.md` §2.2.
#[derive(Debug, Clone)]
pub struct RemoteObject {
    /// Where the redirect chain ended. This, not the submitted URL, is what gets fetched.
    pub final_url: Url,
    /// Every URL in the chain including the first, in order (I-8). Persisted, because
    /// re-resolving from the original URL on resume is how signed-URL downloads get a 403.
    pub redirect_chain: Vec<Url>,
    /// Total representation length, when the server stated one.
    pub total_length: Option<u64>,
    /// Whether ranges are proven usable. Only a validated `206` sets `Proven` (I-6).
    pub range_support: RangeSupport,
    /// The validator to resume against (I-3).
    pub validator: Validator,
    /// A server-supplied digest, if offered (RFC 9530).
    pub digest: Option<ContentDigest>,
    /// Which protocol the probe negotiated.
    pub protocol: NegotiatedProtocol,
    /// The filename the server suggested, before sanitisation.
    pub suggested_filename: Option<String>,
    /// The response content type, used to detect an HTML error page where a binary was
    /// expected (`docs/03-transfer-engine-spec.md` §2.1 step 7).
    pub content_type: Option<Mime>,
    /// When the probe ran, for the freshness window that triggers a re-probe (§2.3).
    pub probed_at: SystemTime,
}
