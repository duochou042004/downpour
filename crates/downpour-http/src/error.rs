//! Error taxonomy for the transfer path.
//!
//! Every variant carries enough context to act on — which URL, which offset, which status —
//! because `Error::Io(..)` with no context is not actionable in a bug report
//! (`.claude/rules/rust.md`).
//!
//! The `kind` strings returned by [`ProbeError::kind`] and [`TransferError::kind`] are **API**.
//! Corpus cases assert on them (ADR-0010) and IPC clients switch on them
//! (`docs/08-ipc-and-ui-spec.md` §3), so they are stable identifiers, never prose. Changing one
//! is a breaking change to both.

use downpour_types::RangeProofError;
use thiserror::Error;
use url::Url;

/// Why a capability probe did not produce a usable [`downpour_types::RemoteObject`].
#[derive(Debug, Error)]
pub enum ProbeError {
    /// The request never got a response.
    #[error("could not reach {url}: {source}")]
    Transport {
        /// The URL that was attempted.
        url: Url,
        /// The underlying transport error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The probe did not finish inside its deadline.
    #[error("probe of {url} timed out")]
    Timeout {
        /// The URL that was attempted.
        url: Url,
    },
    /// More redirect hops than allowed. Bounds a redirect loop, which is otherwise infinite.
    #[error("more than {limit} redirects starting from {start}")]
    TooManyRedirects {
        /// Where the chain started.
        start: Url,
        /// The hop limit that was exceeded.
        limit: u32,
    },
    /// A redirect response carried no usable `Location`.
    #[error("{url} answered {status} with no usable Location header")]
    RedirectWithoutLocation {
        /// The URL that redirected.
        url: Url,
        /// The redirect status.
        status: u16,
    },
    /// A `Location` header was not a valid URL, absolute or relative.
    #[error("{url} redirected to {location:?}, which is not a usable URL")]
    UnusableRedirectTarget {
        /// The URL that redirected.
        url: Url,
        /// The `Location` value as it arrived.
        location: String,
    },
    /// The server answered with a status that makes the transfer impossible for this URL.
    #[error("{url} answered {status}")]
    UnexpectedStatus {
        /// The final URL.
        url: Url,
        /// The status observed.
        status: u16,
        /// The `Retry-After` header, verbatim, when the response carried one.
        ///
        /// Carried on the error because the retry policy needs it and is a pure function — it has
        /// no access to the response. Threading it through a side channel instead would mean the
        /// one piece of information that makes a 503 recoverable is the one piece most easily lost.
        retry_after: Option<String>,
    },
    /// Authentication or authorisation failed, or the URL expired. Not a failure: the engine
    /// moves to `AwaitingRefresh` and keeps every byte it already has (I-8).
    #[error("{url} answered {status}; the URL or credential needs refreshing")]
    NeedsRefresh {
        /// The final URL.
        url: Url,
        /// `401`, `403` or `410`.
        status: u16,
    },
    /// The response body looks like a web page where a file was expected — a login wall or an
    /// error page answered with `200`. `docs/03-transfer-engine-spec.md` §2.1 step 7.
    ///
    /// Reported as a session problem rather than a transfer failure, because it is one: the
    /// correct response is to supply the browser's context, not to retry.
    #[error("{url} returned {content_type:?}, which looks like a web page, not the expected file")]
    LooksLikeAnErrorPage {
        /// The final URL.
        url: Url,
        /// The content type that was served.
        content_type: String,
    },
    /// A ranged probe response carried a non-identity `Content-Encoding` (I-5).
    #[error("{url} compressed a ranged response, which invalidates offset arithmetic: {source}")]
    ContentEncoding {
        /// The final URL.
        url: Url,
        /// The rejection detail.
        #[source]
        source: RangeProofError,
    },
}

impl ProbeError {
    /// The stable identifier clients and corpus cases switch on. **This is API.**
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Transport { .. } => "transport",
            Self::Timeout { .. } => "timeout",
            Self::TooManyRedirects { .. } => "too_many_redirects",
            Self::RedirectWithoutLocation { .. } => "redirect_without_location",
            Self::UnusableRedirectTarget { .. } => "unusable_redirect_target",
            Self::UnexpectedStatus { .. } => "unexpected_status",
            Self::NeedsRefresh { .. } => "needs_refresh",
            Self::LooksLikeAnErrorPage { .. } => "looks_like_error_page",
            Self::ContentEncoding { .. } => "unexpected_content_encoding",
        }
    }
}

/// Why a range fetch did not complete.
#[derive(Debug, Error)]
pub enum TransferError {
    /// The connection failed or was reset. The range goes back to the allocator and is retried
    /// with jittered back-off (`docs/03-transfer-engine-spec.md` §7).
    #[error("transport failure fetching {url}: {source}")]
    Transport {
        /// The URL being fetched.
        url: Url,
        /// The underlying transport error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The fetch did not finish inside its deadline.
    #[error("fetch of {url} timed out after {bytes_delivered} bytes")]
    Timeout {
        /// The URL being fetched.
        url: Url,
        /// How much had arrived when the deadline passed.
        bytes_delivered: u64,
    },
    /// The body ended before the length the response declared.
    ///
    /// Distinguished from [`Self::Transport`] on purpose: `docs/03-transfer-engine-spec.md` §7
    /// gives truncation its own policy — return the unwritten remainder to the allocator and
    /// count it against the retry budget — and that decision needs to know how much did arrive.
    /// Collapsing it into a generic transport error throws away both facts.
    #[error("{url} delivered {delivered} of {expected} bytes before the connection ended")]
    TruncatedBody {
        /// The URL being fetched.
        url: Url,
        /// What the response declared.
        expected: u64,
        /// What actually arrived and was written.
        delivered: u64,
    },
    /// The response status was not usable for a body fetch.
    #[error("{url} answered {status} where a body was expected")]
    UnexpectedStatus {
        /// The URL being fetched.
        url: Url,
        /// The status observed.
        status: u16,
        /// The `Retry-After` header, verbatim, when the response carried one.
        retry_after: Option<String>,
    },
    /// The body's first bytes look like a web page even though the headers did not say so.
    ///
    /// The other half of `docs/03-transfer-engine-spec.md` §2.1 step 7 — "content-type, **or magic
    /// bytes**". Catches a login page served as `application/octet-stream`, which is what an expired
    /// session looks like on an origin that sets the type from the file it meant to serve.
    #[error("{url} sent a body that looks like a web page, declared as {declared_type:?}")]
    LooksLikeAnErrorPage {
        /// The URL being fetched.
        url: Url,
        /// The content type the response declared, if any.
        declared_type: String,
    },
    /// A ranged request came back with a body encoding, or a `Content-Range` inconsistent with
    /// what was asked for. **Rejected without writing anything** (I-5) — this is the error whose
    /// absence produces a file of exactly the right size, full of garbage.
    #[error("{url} answered a ranged request unusably: {source}")]
    UnusableRangeResponse {
        /// The URL being fetched.
        url: Url,
        /// Which check failed.
        #[source]
        source: RangeProofError,
    },
    /// The server delivered more than the range it described. Refused rather than written, so it
    /// cannot overwrite a neighbouring range (I-2).
    #[error("{url} delivered more bytes than the range it described: {source}")]
    OverDelivery {
        /// The URL being fetched.
        url: Url,
        /// The sink's refusal.
        #[source]
        source: crate::sink::SinkError,
    },
    /// Writing the delivered bytes failed.
    #[error("could not store bytes from {url}: {source}")]
    Sink {
        /// The URL being fetched.
        url: Url,
        /// The underlying sink error.
        #[source]
        source: crate::sink::SinkError,
    },
    /// A resume carrying `If-Range` was answered with the whole representation (I-3).
    ///
    /// `If-Range` means "send me this range **only if** the representation still matches this
    /// validator". A `200` is the server saying it does not: what is on the wire is a different
    /// version from the one the existing bytes came from. This is a hard stop, never permission
    /// to overwrite from byte zero and never permission to write the body at the resume offset —
    /// either would splice two versions of a file together at exactly the expected size, which
    /// every integrity check that does not hash the content would pass.
    #[error(
        "{url} answered a resume from byte {resume_offset} with {status}, so the representation \
         changed since validator {validator} was recorded; refusing to splice"
    )]
    ValidatorMismatch {
        /// The URL being fetched.
        url: Url,
        /// The offset the resume asked to continue from.
        resume_offset: u64,
        /// The status that arrived where `206` was required.
        status: u16,
        /// The validator sent in `If-Range`, recorded when the existing bytes were fetched.
        validator: String,
    },
}

impl TransferError {
    /// The stable identifier clients and corpus cases switch on. **This is API.**
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Transport { .. } => "transport",
            Self::Timeout { .. } => "timeout",
            Self::TruncatedBody { .. } => "truncated_body",
            Self::UnexpectedStatus { .. } => "unexpected_status",
            // Deliberately the same kind the probe reports: a corpus case asserting
            // looks_like_error_page should not have to know which layer noticed.
            Self::LooksLikeAnErrorPage { .. } => "looks_like_error_page",
            Self::UnusableRangeResponse { .. } => "unusable_range_response",
            Self::OverDelivery { .. } => "over_delivery",
            Self::Sink { .. } => "sink",
            Self::ValidatorMismatch { .. } => "validator_mismatch",
        }
    }
}
