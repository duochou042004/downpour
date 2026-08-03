//! S1-T2 — `RangeSupport::Proven` is unreachable without a validated 206.
//!
//! Covers **I-6** (range support is proven, never assumed) and the header half of
//! **I-5** (a `Content-Encoding` on a ranged response is rejected, not written).
//!
//! Two halves make the invariant structural rather than aspirational:
//!
//! 1. The compiler enforces reachability. `RangeProof`'s fields are private and it has
//!    exactly one public constructor, so `RangeSupport::Proven` cannot be built anywhere
//!    in the workspace without going through that constructor. No runtime test can assert
//!    this; it is a privacy property, and it is why the type is shaped this way.
//! 2. These tests enforce that the constructor is strict. Each one is a real server
//!    pathology from `docs/01-idm-teardown.md` §3, and each must come back `Err`.

use downpour_types::{ByteRangeSpec, RangeProof, RangeProofError, RangeSupport};

/// The probe request from `docs/03-transfer-engine-spec.md` §2.1: `Range: bytes=0-0`.
const PROBE: ByteRangeSpec = ByteRangeSpec::FromTo { first: 0, last: 0 };

#[test]
fn a_validated_206_proves_range_support() {
    let proof = RangeProof::from_observed_response(PROBE, 206, Some("bytes 0-0/1048576"), None, 1)
        .expect("a well-formed 206 for the requested range is exactly what proves support");

    assert_eq!(proof.total_length(), 1_048_576);

    // And it is the only way to reach `Proven`.
    let support = RangeSupport::Proven(proof);
    assert!(support.is_proven());
    assert_eq!(support.total_length(), Some(1_048_576));
}

#[test]
fn a_200_never_proves_range_support() {
    // The `accept-ranges-lies` pathology: the server advertises ranges, then answers a
    // ranged GET with the whole representation. Eight workers would download the file
    // eight times and assemble nonsense.
    let err = RangeProof::from_observed_response(PROBE, 200, Some("bytes 0-0/1048576"), None, 1)
        .expect_err("a 200 is never proof, whatever headers came with it");
    assert!(matches!(err, RangeProofError::StatusNot206 { status: 200 }));
}

#[test]
fn a_416_never_proves_range_support() {
    let err = RangeProof::from_observed_response(PROBE, 416, Some("bytes */1048576"), None, 0)
        .expect_err("416 means the range was not satisfied");
    assert!(matches!(err, RangeProofError::StatusNot206 { status: 416 }));
}

#[test]
fn an_absent_content_range_is_rejected() {
    // `content-range-absent`: a 206 with no Content-Range tells us nothing about which
    // bytes are in the body, so there is no offset it is safe to write them at.
    let err = RangeProof::from_observed_response(PROBE, 206, None, None, 1)
        .expect_err("a 206 without Content-Range is unusable");
    assert!(matches!(err, RangeProofError::ContentRangeAbsent));
}

#[test]
fn a_malformed_content_range_is_rejected() {
    let err = RangeProof::from_observed_response(PROBE, 206, Some("bytes 0-0"), None, 1)
        .expect_err("no complete-length means the header is malformed");
    assert!(matches!(err, RangeProofError::ContentRangeMalformed(_)));
}

#[test]
fn a_content_range_for_a_different_range_is_rejected() {
    // `content-range-mismatch`: we asked for byte 0 and the server describes byte 4096.
    // Writing this body at offset 0 corrupts the file at exactly the right size.
    let err =
        RangeProof::from_observed_response(PROBE, 206, Some("bytes 4096-4096/1048576"), None, 1)
            .expect_err("a range we did not ask for is never trusted");
    assert!(matches!(err, RangeProofError::ContentRangeMismatch { .. }));
}

#[test]
fn a_gzipped_range_response_is_rejected() {
    // `gzip-on-range` (I-5). The bytes on the wire no longer correspond to the byte range
    // that was requested. This is checked from the headers, before any body is accepted.
    let err =
        RangeProof::from_observed_response(PROBE, 206, Some("bytes 0-0/1048576"), Some("gzip"), 1)
            .expect_err("a non-identity Content-Encoding on a ranged response is rejected");
    assert!(matches!(
        err,
        RangeProofError::UnexpectedContentEncoding { .. }
    ));
}

#[test]
fn an_explicit_identity_content_encoding_is_fine() {
    // `Accept-Encoding: identity` was sent, so `Content-Encoding: identity` is the
    // correct, conformant answer and must not be treated as a pathology.
    RangeProof::from_observed_response(PROBE, 206, Some("bytes 0-0/64"), Some("identity"), 1)
        .expect("identity is not an encoding that shifts offsets");
}

#[test]
fn an_unknown_total_length_is_rejected() {
    // `bytes 0-0/*` is legal HTTP but leaves the representation length unknown, so there
    // is nothing to segment. Single stream, no proof.
    let err = RangeProof::from_observed_response(PROBE, 206, Some("bytes 0-0/*"), None, 1)
        .expect_err("an unknown total length cannot support segmentation");
    assert!(matches!(err, RangeProofError::TotalLengthUnknown));
}

#[test]
fn a_full_body_behind_a_206_is_rejected() {
    // The server sets the right status and header but ignores the range and streams the
    // whole file. Only counting the body catches this one.
    let err =
        RangeProof::from_observed_response(PROBE, 206, Some("bytes 0-0/1048576"), None, 1_048_576)
            .expect_err("a body longer than the described range means ranges were ignored");
    assert!(matches!(
        err,
        RangeProofError::BodyLengthMismatch {
            expected: 1,
            actual: 1_048_576
        }
    ));
}

#[test]
fn a_short_body_behind_a_206_is_rejected() {
    // A 100-byte range must be requested for this to reach the body check at all; with the
    // 1-byte probe range the header itself is already inconsistent.
    let requested = ByteRangeSpec::FromTo { first: 0, last: 99 };
    let err =
        RangeProof::from_observed_response(requested, 206, Some("bytes 0-99/1048576"), None, 50)
            .expect_err("a truncated body does not match the range it claims to be");
    assert!(matches!(
        err,
        RangeProofError::BodyLengthMismatch {
            expected: 100,
            actual: 50
        }
    ));
}

#[test]
fn absent_and_unknown_are_distinguishable_and_neither_is_proven() {
    assert!(!RangeSupport::Absent.is_proven());
    assert!(!RangeSupport::Unknown.is_proven());
    assert_eq!(RangeSupport::Absent.total_length(), None);
    assert_eq!(RangeSupport::Unknown.total_length(), None);
}
