//! Shared types for Downpour.
//!
//! This crate is **pure**: no I/O, no async, no clock reads, no logging. That is what makes
//! it exhaustively property-testable, and it is why several engine invariants live here
//! rather than in the code that happens to use them.
//!
//! Invariants enforced by construction in this crate:
//!
//! | Invariant | How |
//! | --------- | --- |
//! | I-6 — range support is proven, never assumed | [`RangeSupport::Proven`] needs a [`RangeProof`], which has one validating constructor |
//! | I-5 — content encoding never corrupts offsets | [`RangeProof::from_observed_response`] rejects a non-identity encoding; [`ContentRange::is_consistent_with`] rejects a shifted range |
//! | I-3 — resume never splices representations | [`Validator::from_headers`] classifies a weak `ETag` as no validator at all |
//! | S1-C3 — sanitisation cannot escape a directory | [`filename::sanitise`] returns exactly one normal path component |
//!
//! "Enforced by construction" means the compiler refuses the mistake, not that a reviewer is
//! expected to notice it. Where that was achievable it was worth the extra type.

// The engine and its libraries return errors; they do not abort the process. A panic in the
// daemon takes down every active transfer. Tests are exempt — see .claude/rules/rust.md.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable
    )
)]

pub mod content_range;
pub mod filename;
pub mod remote;

pub use content_range::{ByteRangeSpec, ContentRange, ContentRangeError, RangeMismatch};
pub use remote::{
    ContentDigest, DigestAlgorithm, NegotiatedProtocol, RangeProof, RangeProofError, RangeSupport,
    RemoteObject, Validator,
};
