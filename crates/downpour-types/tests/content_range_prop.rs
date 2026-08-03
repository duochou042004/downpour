//! S1-T3 — a `Content-Range` is only ever accepted when it is consistent with the range
//! that was actually requested.
//!
//! Covers **I-5**. The failure this prevents: the server describes bytes we did not ask
//! for, the engine writes the body at the offset it wanted rather than the offset the
//! server sent, and the download finishes at exactly the expected size, full of garbage.

use downpour_types::{ByteRangeSpec, ContentRange, ContentRangeError};
use proptest::prelude::*;

// ---------------------------------------------------------------- parsing

#[test]
fn well_formed_headers_are_accepted() {
    let cases = [
        (
            "bytes 0-0/1",
            ContentRange::Bytes {
                first: 0,
                last: 0,
                complete_length: Some(1),
            },
        ),
        (
            "bytes 0-499/1234",
            ContentRange::Bytes {
                first: 0,
                last: 499,
                complete_length: Some(1234),
            },
        ),
        (
            "bytes 42-99/*",
            ContentRange::Bytes {
                first: 42,
                last: 99,
                complete_length: None,
            },
        ),
        (
            "bytes */1234",
            ContentRange::Unsatisfied {
                complete_length: 1234,
            },
        ),
        // Range units are case-insensitive (RFC 9110 §14.1).
        (
            "BYTES 0-0/1",
            ContentRange::Bytes {
                first: 0,
                last: 0,
                complete_length: Some(1),
            },
        ),
        // Surrounding whitespace and a doubled SP after the unit are tolerated; real
        // servers emit both and neither changes the meaning.
        (
            "  bytes  0-0/1  ",
            ContentRange::Bytes {
                first: 0,
                last: 0,
                complete_length: Some(1),
            },
        ),
    ];
    for (header, expected) in cases {
        assert_eq!(
            header.parse::<ContentRange>(),
            Ok(expected),
            "parsing {header:?}"
        );
    }
}

#[test]
fn malformed_headers_are_rejected() {
    let cases = [
        "",
        "bytes",
        "bytes 0-0",                                         // no complete-length
        "bytes */*",          // unsatisfied with an unknown length says nothing at all
        "items 0-0/1",        // unsupported range unit
        "bytes 5-1/10",       // reversed
        "bytes 0-10/5",       // last byte beyond the representation
        "bytes -1-5/10",      // empty first-byte-pos
        "bytes 0-/10",        // empty last-byte-pos
        "bytes 0-0/abc",      // non-numeric complete-length
        "bytes 0 - 0/10",     // internal whitespace
        "bytes 0-0/10 extra", // trailing garbage
        "bytes 0-0/-1",       // negative complete-length
        "bytes 0x0-0x0/10",   // hex
        "bytes 0-18446744073709551615/18446744073709551616", // overflows u64
    ];
    for header in cases {
        assert!(
            header.parse::<ContentRange>().is_err(),
            "{header:?} should have been rejected, got {:?}",
            header.parse::<ContentRange>()
        );
    }
}

#[test]
fn an_unsatisfied_range_is_never_consistent_with_a_satisfiable_request() {
    let unsatisfied: ContentRange = "bytes */1234".parse().expect("well formed");
    let requested = ByteRangeSpec::FromTo { first: 0, last: 0 };
    assert!(unsatisfied.is_consistent_with(requested).is_err());
}

#[test]
fn a_short_but_aligned_range_is_consistent() {
    // A server may satisfy a range request with fewer bytes than asked for. That is legal
    // and safe: the start offset is what matters, and the remainder goes back to the
    // allocator. What is never safe is a *different* start offset.
    let requested = ByteRangeSpec::FromTo {
        first: 1000,
        last: 1999,
    };
    let got: ContentRange = "bytes 1000-1499/8000".parse().expect("well formed");
    assert!(got.is_consistent_with(requested).is_ok());

    let shifted: ContentRange = "bytes 1001-1500/8000".parse().expect("well formed");
    assert!(shifted.is_consistent_with(requested).is_err());
}

#[test]
fn an_open_ended_request_only_checks_the_start() {
    let requested = ByteRangeSpec::From { first: 4096 };
    let got: ContentRange = "bytes 4096-8191/8192".parse().expect("well formed");
    assert!(got.is_consistent_with(requested).is_ok());

    let wrong: ContentRange = "bytes 0-8191/8192".parse().expect("well formed");
    assert!(wrong.is_consistent_with(requested).is_err());
}

#[test]
fn a_suffix_request_needs_a_known_length_to_be_verifiable() {
    let requested = ByteRangeSpec::Suffix { len: 100 };

    let verifiable: ContentRange = "bytes 900-999/1000".parse().expect("well formed");
    assert!(verifiable.is_consistent_with(requested).is_ok());

    // Without a complete-length there is no way to know whether 900 is the right start.
    // Unverifiable is treated as inconsistent, never as "probably fine".
    let unverifiable: ContentRange = "bytes 900-999/*".parse().expect("well formed");
    assert!(unverifiable.is_consistent_with(requested).is_err());

    let wrong_start: ContentRange = "bytes 899-999/1000".parse().expect("well formed");
    assert!(wrong_start.is_consistent_with(requested).is_err());
}

#[test]
fn the_length_of_a_parsed_range_is_inclusive_of_both_ends() {
    let cr: ContentRange = "bytes 0-0/1".parse().expect("well formed");
    assert_eq!(cr.len(), Some(1));
    let cr: ContentRange = "bytes 100-199/1000".parse().expect("well formed");
    assert_eq!(cr.len(), Some(100));
    let cr: ContentRange = "bytes */1000".parse().expect("well formed");
    assert_eq!(cr.len(), None);
}

#[test]
fn a_request_header_round_trips_through_its_own_wire_form() {
    assert_eq!(
        ByteRangeSpec::FromTo { first: 0, last: 0 }.header_value(),
        "bytes=0-0"
    );
    assert_eq!(
        ByteRangeSpec::From { first: 4096 }.header_value(),
        "bytes=4096-"
    );
    assert_eq!(
        ByteRangeSpec::Suffix { len: 500 }.header_value(),
        "bytes=-500"
    );
}

// ---------------------------------------------------------------- properties

proptest! {
    /// The law: a `Content-Range` is consistent with a `FromTo` request if and only if it
    /// starts at exactly the requested offset and ends no later than requested. Anything
    /// else must be rejected — there is no "close enough" for a file offset.
    #[test]
    fn never_accepts_a_range_inconsistent_with_the_request(
        req_first in 0u64..100_000,
        req_len in 1u64..100_000,
        got_first in 0u64..100_000,
        got_len in 1u64..100_000,
        total in 1u64..1_000_000,
    ) {
        let requested = ByteRangeSpec::FromTo { first: req_first, last: req_first + req_len - 1 };
        let got_last = got_first + got_len - 1;
        let header = format!("bytes {got_first}-{got_last}/{total}");

        let expected_consistent =
            got_first == req_first && got_last < req_first + req_len && got_last < total;

        match header.parse::<ContentRange>() {
            Ok(cr) => prop_assert_eq!(
                cr.is_consistent_with(requested).is_ok(),
                expected_consistent,
                "header {:?} against request {:?}", header, requested
            ),
            // The only reason a header of this shape fails to parse is that the last byte
            // lies outside the representation, which is also inconsistent.
            Err(ContentRangeError::LastByteBeyondComplete { .. }) => {
                prop_assert!(!expected_consistent);
            }
            Err(e) => prop_assert!(false, "unexpected parse error {:?} for {:?}", e, header),
        }
    }

    /// Parsing never panics, whatever arrives on the wire. This header is attacker-influenced
    /// on any origin the user does not control.
    #[test]
    fn parsing_never_panics(raw in proptest::collection::vec(any::<char>(), 0..80)) {
        let s: String = raw.into_iter().collect();
        let _ = s.parse::<ContentRange>();
    }

    /// A well-formed header parses to values that re-serialise to the same header, so the
    /// parser is not quietly normalising a range into a different one.
    #[test]
    fn parsing_preserves_the_range(first in 0u64..100_000, len in 1u64..100_000) {
        let last = first + len - 1;
        let total = last + 1;
        let header = format!("bytes {first}-{last}/{total}");
        let cr = header.parse::<ContentRange>().expect("well formed by construction");
        prop_assert_eq!(cr, ContentRange::Bytes {
            first,
            last,
            complete_length: Some(total),
        });
        prop_assert_eq!(cr.len(), Some(len));
    }
}
