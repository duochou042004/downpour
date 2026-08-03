//! S1-T2 — validator classification and digest capture, from probe step 5 and step 6 of
//! `docs/03-transfer-engine-spec.md` §2.1.
//!
//! The validator half covers **I-3** at its source. A weak `ETag` may compare equal across
//! two representations that differ byte for byte, so treating one as resumable is exactly the
//! condition that splices half of version A onto half of version B at precisely the expected
//! file size. It is classified as "no validator" here, once, rather than at each call site.

use downpour_types::{ContentDigest, DigestAlgorithm, Validator};

#[test]
fn a_strong_etag_is_preferred_over_last_modified() {
    let v = Validator::from_headers(Some("\"abc123\""), Some("Mon, 03 Aug 2026 10:00:00 GMT"));
    assert_eq!(v, Validator::StrongETag("\"abc123\"".to_owned()));
    assert!(v.is_strong());
    assert_eq!(v.if_range_value(), Some("\"abc123\""));
}

#[test]
fn a_weak_etag_is_not_a_validator() {
    // The `weak-etag-only` corpus case. W/ is case-sensitive per RFC 9110 §8.8.3.
    let v = Validator::from_headers(Some("W/\"abc123\""), None);
    assert_eq!(v, Validator::None);
    assert!(!v.is_strong());
    assert_eq!(v.if_range_value(), None);
}

#[test]
fn a_weak_etag_falls_back_to_last_modified_when_offered() {
    let v = Validator::from_headers(Some("W/\"abc\""), Some("Mon, 03 Aug 2026 10:00:00 GMT"));
    assert_eq!(
        v,
        Validator::LastModified("Mon, 03 Aug 2026 10:00:00 GMT".to_owned())
    );
    assert!(
        !v.is_strong(),
        "Last-Modified is usable but is not a strong validator"
    );
    assert_eq!(v.if_range_value(), Some("Mon, 03 Aug 2026 10:00:00 GMT"));
}

#[test]
fn an_unquoted_etag_is_not_trusted() {
    // A bare token is not a valid entity-tag. Rather than guess at quoting, treat it as
    // absent: a wrong validator is worse than no validator.
    assert_eq!(
        Validator::from_headers(Some("abc123"), None),
        Validator::None
    );
}

#[test]
fn empty_and_whitespace_headers_are_treated_as_absent() {
    assert_eq!(Validator::from_headers(Some(""), Some("")), Validator::None);
    assert_eq!(
        Validator::from_headers(Some("   "), Some("  ")),
        Validator::None
    );
    assert_eq!(Validator::from_headers(None, None), Validator::None);
}

#[test]
fn a_no_validator_response_offers_nothing_for_if_range() {
    // The `no-validator` corpus case. Resume is unsafe here and S2 must fall back to a
    // sample-range comparison rather than an If-Range request.
    assert_eq!(Validator::from_headers(None, None).if_range_value(), None);
}

#[test]
fn a_digest_is_captured_and_the_strongest_offered_wins() {
    let d = ContentDigest::parse("sha-256=:BBBB:, sha-512=:AAAA:").expect("both understood");
    assert_eq!(d.algorithm, DigestAlgorithm::Sha512);
    assert_eq!(d.encoded, "AAAA");
    assert_eq!(d.algorithm.token(), "sha-512");
}

#[test]
fn an_unknown_digest_algorithm_is_skipped_not_rejected() {
    // A digest we cannot check is no worse than no digest, so an unknown algorithm alongside
    // a known one must not discard the known one.
    let d = ContentDigest::parse("md5=:XXXX:, sha-256=:BBBB:").expect("sha-256 is understood");
    assert_eq!(d.algorithm, DigestAlgorithm::Sha256);
    assert_eq!(d.encoded, "BBBB");

    assert_eq!(ContentDigest::parse("md5=:XXXX:"), None);
}

#[test]
fn a_malformed_digest_yields_nothing() {
    for raw in [
        "",
        "sha-256",
        "sha-256=",
        "sha-256=BBBB",
        "sha-256=:",
        "sha-256=::",
    ] {
        assert_eq!(ContentDigest::parse(raw), None, "parsing {raw:?}");
    }
}

#[test]
fn digest_parameters_are_case_insensitive() {
    let d = ContentDigest::parse("SHA-256=:BBBB:").expect("the token is case-insensitive");
    assert_eq!(d.algorithm, DigestAlgorithm::Sha256);
}
