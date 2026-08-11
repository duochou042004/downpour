//! The declarative corpus case format (ADR-0010).
//!
//! Every struct here carries `deny_unknown_fields`, and that is the single most important thing
//! about this module. ADR-0010's third contract rule is that the runner **fails closed**: a
//! declarative test runner which silently ignores an assertion it does not understand is worse
//! than no runner at all, because every case reports green, the corpus metric climbs, and nothing
//! is being checked. That is risk R-7 in `state/progress.json` exactly. `deny_unknown_fields` turns
//! a typo in an expectation into a hard load error instead of a silently dropped assertion.
//!
//! There is deliberately no per-case schema version. Because an unknown key is a hard error,
//! adding one fails loudly on every case at once, and the cases live in this repository rather
//! than out in the world — so a migration is one mechanical commit, not a compatibility problem.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::content::{Content, GENERATOR_V1};
use crate::server::{
    ConcurrentRequests, Framing, IfRangeBehaviour, Mutation, MutationEffect, Protocol,
    RangeBehaviour, RedirectLocation, ServerSpec, SlowSegment, TightenCap,
};

/// One corpus case.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    /// Unique, kebab-case, stable — it gets cited in commit messages and bug reports.
    pub id: String,
    /// One of the ten categories in `docs/09-testing-strategy.md` §3.2.
    pub category: Category,
    /// What the server does wrong and what the engine must do about it.
    pub description: String,
    /// Required, and required to be non-empty: every case is traceable to the RFC clause or the
    /// invariant it exercises. A case nobody can trace is a case nobody will maintain.
    pub references: Vec<String>,
    /// Excluded from the default profile when true.
    #[serde(default)]
    pub slow: bool,
    /// The state the local filesystem is in before the transfer starts.
    ///
    /// The `local` category's pathologies are properties of the disk rather than of the server —
    /// a target that already exists, a directory nobody may write to, a `.dppart` another process
    /// already owns. None of them can be expressed by a server, which is why they need their own
    /// section rather than another server knob.
    /// How many connections the engine may use, when the case is about concurrency.
    ///
    /// Defaults to one, which is the single-stream path every case used before this existed. A
    /// connection pathology — a per-IP cap, a per-connection cap, a concurrent-request limit —
    /// only exists when more than one connection is open, so a whole category of cases about them
    /// is unreachable without this.
    #[serde(default)]
    pub connections: Option<usize>,
    /// The local preconditions this case sets up before the transfer runs.
    #[serde(default)]
    pub local: LocalCase,
    /// What the server should do.
    pub server: ServerCase,
    /// What the engine must do about it.
    pub expect: Expect,
}

/// The taxonomy from `docs/09-testing-strategy.md` §3.2.
///
/// All ten are listed even though S1 only uses three, so that a case filed under a later
/// category is a scoping error caught at load time rather than a silent misfile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    /// Range support, lies, malformed `Content-Range`, `200`-for-range, multipart.
    Ranges,
    /// Missing, weak, changed, CDN-inconsistent validators.
    Validators,
    /// No length, wrong length, chunked, encoding on a range.
    Framing,
    /// Signed URLs, referer, cookies, HTML-instead-of-file, `401`/`403`.
    Session,
    /// Per-IP caps, per-connection caps, `429`, silent drops, close-after-N.
    Connections,
    /// Chains, cross-host, loops, protocol downgrade.
    Redirects,
    /// `CONNECT`, SOCKS5, auth, proxies that break ranges.
    Proxies,
    /// h1/h2/h3 behaviour, ALPN mismatch, `Alt-Svc`, h3 fallback.
    Protocols,
    /// Disk full, permissions, path limits, collisions, network filesystems.
    Local,
    /// Manifest shapes, ordering, rejection of DRM and live.
    Media,
}

impl Category {
    /// Directory name this category's cases live under.
    #[must_use]
    pub fn directory(self) -> &'static str {
        match self {
            Self::Ranges => "ranges",
            Self::Validators => "validators",
            Self::Framing => "framing",
            Self::Session => "session",
            Self::Connections => "connections",
            Self::Redirects => "redirects",
            Self::Proxies => "proxies",
            Self::Protocols => "protocols",
            Self::Local => "local",
            Self::Media => "media",
        }
    }
}

/// How the server should behave.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerCase {
    /// Which protocol to speak.
    #[serde(default)]
    pub protocol: ProtocolCase,
    /// The representation to serve.
    pub content: ContentCase,
    /// How to treat `Range`.
    #[serde(default)]
    pub ranges: RangesCase,
    /// How to frame the body.
    #[serde(default)]
    pub framing: FramingCase,
    /// Extra response headers, verbatim.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// `ETag`, verbatim — including `W/"…"` when a weak validator is the point.
    #[serde(default)]
    pub etag: Option<String>,
    /// `Last-Modified`, verbatim.
    #[serde(default)]
    pub last_modified: Option<String>,
    /// `Content-Encoding` to declare. The body is not actually encoded; what is under test is
    /// whether the engine refuses to write it (I-5).
    #[serde(default)]
    pub content_encoding: Option<String>,
    /// `Content-Type`.
    #[serde(default)]
    pub content_type: Option<String>,
    /// `Content-Disposition`, verbatim, traversal attempts included.
    #[serde(default)]
    pub content_disposition: Option<String>,
    /// `Repr-Digest` (RFC 9530).
    #[serde(default)]
    /// `Repr-Digest`, verbatim — or the literal `computed`, which asks the server to state the
    /// true SHA-256 of the content it is about to serve.
    ///
    /// A literal base64 digest for a given seed and size is unmaintainable by hand, so without
    /// `computed` the corpus could only ever prove the *mismatch* direction — and a verification
    /// step that always fails looks exactly like one that works (B-28).
    pub digest: Option<String>,
    /// Statuses for a chain of redirect hops.
    #[serde(default)]
    pub redirect_chain: Vec<u16>,
    /// Stop writing the body after this many bytes and close.
    #[serde(default)]
    pub truncate_body_after: Option<ByteSize>,
    /// Answer with this status instead of `200`/`206`.
    #[serde(default)]
    pub status: Option<u16>,
    /// Serve this literal text instead of generated content — an HTML login page, say.
    #[serde(default)]
    pub body: Option<String>,
    /// Serve the entry path as a redirect that points at itself, so only a hop limit can stop it.
    #[serde(default)]
    pub redirect_loop: bool,
    /// Fail this many body requests transiently before serving correctly.
    #[serde(default)]
    pub transient_body_failures: u32,
    /// Answer this many requests with `transient_status` plus `Retry-After` before serving correctly.
    #[serde(default)]
    pub transient_status_failures: u32,
    /// The `Retry-After` value those failures carry.
    #[serde(default)]
    pub transient_retry_after: Option<String>,
    /// The status those failures carry. Defaults to 503.
    #[serde(default = "default_transient_status")]
    pub transient_status: u16,
    /// Omit the terminating zero-length chunk of a chunked response, then close.
    #[serde(default)]
    pub omit_chunked_terminator: bool,
    /// What redirect hops put in their `Location` header.
    #[serde(default)]
    pub redirect_location: RedirectLocationCase,
    /// How the SECOND origin treats ranges, when it must differ from the first.
    ///
    /// I-6's `cdn-edge-disagrees` needs this: the pathology is an edge and an origin that do not
    /// agree about range support.
    #[serde(default)]
    pub content_origin_ranges: Option<RangesCase>,
    /// Send the redirect chain to a **second origin**, which serves the content.
    ///
    /// One server serves one origin, so the runner starts two: the first only redirects, the
    /// second holds the representation. Requires a non-empty `redirect_chain`.
    #[serde(default)]
    pub cross_origin: bool,
    /// Corrupt every byte from this offset onward. Only for `self-test/` fixtures: it exists to
    /// prove the runner's corruption comparison actually fires.
    #[serde(default)]
    pub corrupt_from: Option<ByteSize>,
    /// How the server treats `If-Range` on a resume.
    #[serde(default)]
    pub if_range: IfRangeCase,
    /// Triggered mid-transfer mutations (ADR-0010's `behaviour`).
    ///
    /// Enacted since S2-T9. Every field is `deny_unknown_fields`, so a mutation the server
    /// cannot perform is a load error rather than a case that silently changes nothing — which
    /// is what would make an `etag-changed-midway` case pass without any ETag ever changing.
    #[serde(default)]
    pub behaviour: Vec<BehaviourCase>,
    /// Serve at most this many connections at once, dropping the rest without answering.
    ///
    /// A per-IP cap as an origin actually applies one: the socket is accepted and then dropped,
    /// with no status and no reason. Answering would make it the `429` pathology instead.
    #[serde(default)]
    pub max_concurrent_connections: Option<usize>,
    /// Answer the connection beyond `max_concurrent_connections` with this status instead of
    /// dropping it.
    ///
    /// The other way an origin enforces a cap. A drop produces a transport error; an answer
    /// produces a *status*, which is a different path through the engine's retry classification
    /// and the one I-7 names when it says concurrency falls back on `429`.
    #[serde(default)]
    pub cap_status: Option<u16>,
    /// Change `max_concurrent_connections` to a new value once this many requests have been
    /// served.
    ///
    /// A limit that moves under a plan the engine already committed to. Distinct from a steady
    /// cap: at the low value the engine would never have segmented this far, and at the high one
    /// nothing is ever refused.
    #[serde(default)]
    pub tighten_cap: Option<TightenCapCase>,
    /// Accept at most this many connections in total, ever, dropping every one after them.
    ///
    /// A budget rather than a concurrency limit: it does not clear when a peer finishes, so
    /// waiting cannot recover it and only reuse of an already-open connection can.
    #[serde(default)]
    pub max_total_connections: Option<usize>,
    /// End this many responses part way through the header block.
    ///
    /// The point on the response timeline between "nothing arrived" and "the body was cut short":
    /// the client has parsed a status line and some headers, and has no complete message.
    #[serde(default)]
    pub close_mid_headers: usize,
    /// Read and discard this many requests that arrived on a connection which had already
    /// answered one, without responding to them.
    ///
    /// The keep-alive race. Unlike `close_after_requests`, the socket is alive when the client
    /// picks it out of the pool and dies with a request already written into it. A count rather
    /// than a switch, because a race is an occasional event: an origin that hangs up on *every*
    /// reused request cannot be survived by a client that reuses connections at all, and
    /// adapting to that is pool policy rather than retry (see the backlog).
    #[serde(default)]
    pub hangup_on_reused_request: usize,
    /// After this many responses on a connection, write a second copy of one nobody asked for.
    ///
    /// The extra response stays in the socket, so the next request sent on that connection reads
    /// an answer belonging to a different range.
    #[serde(default)]
    pub duplicate_response_after: Option<usize>,
    /// Close after this many body bytes on every response, whatever was asked for.
    ///
    /// Every attempt advances, which is what separates this from a truncation: the transfer
    /// finishes only if a retry asks for the remainder rather than for the grant again.
    #[serde(default)]
    pub close_after_body_bytes: Option<ByteSize>,
    /// Cut every response that reaches this offset in the representation, and close.
    ///
    /// A point in the *file* rather than in the response, so the ranges before it finish and the
    /// one across it never can.
    #[serde(default)]
    pub drop_at_offset: Option<ByteSize>,
    /// Serve the segment from this offset onward at a trickle, while its peers run at full speed.
    #[serde(default)]
    pub slow_segment: Option<SlowSegmentCase>,
    /// Honour ranges for this many ranged responses, then answer every one with the whole
    /// representation.
    ///
    /// Range evidence that was real when the probe took it and worthless afterwards. Distinct
    /// from `ranges: lies`, which never honours one: there the probe refuses to segment, so the
    /// engine never commits workers to ranges it cannot get.
    #[serde(default)]
    pub withdraw_ranges_after: Option<usize>,
    /// Answer exactly this ranged response, counting from one, with the whole representation.
    ///
    /// One node in a fleet that was never configured for ranges, rather than an origin that
    /// withdraws them for good.
    #[serde(default)]
    pub ignore_range_at: Option<usize>,
    /// Apply `ranges: shifted_content_range` only from this ranged response onward.
    ///
    /// The probe is answered honestly, so segmentation is permitted on evidence that was true.
    #[serde(default)]
    pub shift_ranges_after: Option<usize>,
    /// Answer every ranged response after this one with `416`, however satisfiable the range is.
    #[serde(default)]
    pub status_416_after: Option<usize>,
    /// State the total as `*` in every `Content-Range` after this ranged response.
    #[serde(default)]
    pub unknown_total_after: Option<usize>,
    /// Serve the body from the start of the representation while describing the requested range.
    ///
    /// The one range pathology no header check can catch: nothing the server says is untrue.
    #[serde(default)]
    pub serve_wrong_offset: bool,
    /// Honour the first byte position of a range and serve to the end of the representation.
    #[serde(default)]
    pub ignore_range_end: bool,
    /// Declare a `Content-Length` of half what the `Content-Range` spans, and send that much.
    #[serde(default)]
    pub halve_content_length_on_ranges: bool,
    /// Serve no more than this much of any range, describing honestly what was served.
    #[serde(default)]
    pub cap_range_span: Option<ByteSize>,
    /// Limit how many requests may be in flight at once, across every connection.
    ///
    /// A limit on work rather than on sockets: it clears when a peer finishes, not when a
    /// connection closes, so nothing in the connection count reveals it.
    #[serde(default)]
    pub concurrent_requests: Option<ConcurrentRequestsCase>,
    /// Close the listener after this many connections, refusing every later connect at the kernel.
    #[serde(default)]
    pub stop_listening_after_connections: Option<usize>,
    /// `Retry-After` on a response rejected by `concurrent_requests` or by the connection cap.
    #[serde(default)]
    pub cap_retry_after: Option<String>,
    /// After this many ranged responses, report a different total in `Content-Range`.
    #[serde(default)]
    pub inconsistent_total_after: Option<usize>,
    /// Accept and immediately close this many connections, with no response at all.
    #[serde(default)]
    pub close_without_responding: usize,
    /// Close the connection after this many requests on it, without an error.
    ///
    /// A keep-alive the origin silently stops honouring. A client that assumes its pooled
    /// connection is still good writes into a closed socket.
    #[serde(default)]
    pub close_after_requests: Option<usize>,
}

/// One segment served far more slowly than the others.
///
/// Keyed by the offset the response body starts at rather than by which connection it arrives on.
/// A connection ordinal is not stable — the third socket to be accepted is whichever one the pool
/// happened to open third, and it may carry a large range, a small one, or a retry — which made
/// the observed delay count vary between runs. The straggler is a property of the *segment*, which
/// is also what `docs/01` §3.5 describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlowSegmentCase {
    /// Responses whose body begins at or after this offset are trickled.
    pub from_offset: ByteSize,
    /// How long to pause before each chunk.
    pub delay_ms: u64,
}

/// A limit on requests in flight, and what the origin does with the ones over it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConcurrentRequestsCase {
    /// How many requests may be answered at once.
    pub limit: usize,
    /// What happens to a request that arrives over the limit.
    pub then: OverLimitCase,
}

/// What an origin does with a request beyond its in-flight limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OverLimitCase {
    /// Hold it until a peer finishes. The client is told nothing and sees only latency.
    Wait,
    /// Answer `429` and close.
    Reject,
}

/// A concurrency cap that changes part way through the transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TightenCapCase {
    /// How many requests are served under the original cap before it changes.
    pub after_requests: usize,
    /// The cap that applies from then on.
    pub to: usize,
}

/// The local preconditions a case sets up before the transfer runs.
///
/// Every field defaults to "nothing unusual", so a case that does not mention `local` behaves
/// exactly as it did before this existed.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalCase {
    /// Create a file at the final name before the download starts, holding these bytes.
    ///
    /// The user already has this file. Overwriting it is unrecoverable, so the engine must
    /// refuse rather than silently replace it.
    #[serde(default)]
    pub existing_target: Option<String>,
    /// Create the `.dppart` before the download starts, holding these bytes.
    ///
    /// A part file that already exists means another owner holds this download. Both artifacts
    /// are created exclusively precisely so that collision is loud rather than an interleaving
    /// of two writers into one file.
    #[serde(default)]
    pub existing_part: Option<String>,
    /// Create a *directory* at the final name before the download starts.
    ///
    /// Distinct from `existing_target`: the filesystem calls behave differently — the exclusive
    /// create fails with `EISDIR` rather than `EEXIST`, and a rename onto a non-empty directory
    /// fails at the end rather than the start — while the requirement is identical, because
    /// whatever is there belongs to the user either way.
    #[serde(default)]
    pub existing_target_directory: bool,
    /// Make the target directory unwritable before the download starts.
    ///
    /// Unix only, and deliberately so rather than by accident: Windows' read-only attribute on a
    /// *directory* does not stop files being created inside it, so there is no way to express
    /// this precondition there. The case is excluded on other platforms and says so, rather than
    /// being written to pass everywhere by asserting less.
    ///
    /// It is also excluded for a user who bypasses permission checks — root — because a green
    /// result for a check that cannot fail is worse than an honestly absent one.
    #[serde(default)]
    pub read_only_target_dir: bool,
    /// Create a *dangling* symlink at the final name — one whose destination does not exist.
    ///
    /// This is the case where asking "is anything there?" gets the wrong answer. `exists` and
    /// `try_exists` both follow the link, find nothing at the far end, and report that the path
    /// is free. It is not free: the user has a symlink there, and the rename at the end of a
    /// download replaces the link itself rather than following it, so proceeding destroys
    /// something the engine promised to leave alone.
    ///
    /// Dangling specifically, because a symlink pointing at a file that *does* exist is caught by
    /// any existence check and proves nothing about which call was used.
    ///
    /// Unix only: creating a symlink on Windows needs privileges or developer mode, so a case
    /// that ran there would be testing the runner's environment rather than the engine.
    #[serde(default)]
    pub dangling_symlink_at_target: bool,
    /// Create a file holding these bytes, then a symlink at the `.dppart` path pointing at it.
    ///
    /// The part file is where every byte of a transfer lands, so a symlink there is the version
    /// of this hazard that actually costs data: following it writes the whole download through
    /// the link and over whatever the destination was. The bytes are asserted byte-identical
    /// afterwards, which is what makes "the engine refused" distinguishable from "the engine
    /// wrote somewhere else and said nothing".
    ///
    /// Unix only, for the same reason as [`Self::dangling_symlink_at_target`].
    #[serde(default)]
    pub symlink_at_part: Option<String>,
}

/// How a case's server treats `If-Range`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IfRangeCase {
    /// RFC 9110 §13.1.5: the range when the validator matches, the whole representation when not.
    #[default]
    Honoured,
    /// Answer any request carrying `If-Range` with `200` and the whole representation.
    Ignored,
}

/// One entry of ADR-0010's `behaviour` list: a trigger and what it changes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BehaviourCase {
    /// When this fires.
    pub at: TriggerCase,
    /// What it changes.
    pub then: EffectCase,
}

/// When a [`BehaviourCase`] fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerCase {
    /// Total body bytes served across all requests, absolute or a percentage of the content.
    pub bytes_served: ServedAmount,
}

/// An amount of served content: `40%`, or an absolute size like `8KiB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServedAmount {
    /// A percentage of the representation length.
    Percent(u64),
    /// An absolute byte count.
    Bytes(ByteSize),
}

impl ServedAmount {
    /// Resolve against the representation length.
    #[must_use]
    pub fn resolve(self, total: u64) -> u64 {
        match self {
            Self::Percent(percent) => total.saturating_mul(percent) / 100,
            Self::Bytes(size) => size.0,
        }
    }
}

impl<'de> Deserialize<'de> for ServedAmount {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Integer(u64),
            Text(String),
        }

        match Raw::deserialize(deserializer)? {
            Raw::Integer(value) => Ok(Self::Bytes(ByteSize(value))),
            Raw::Text(text) => {
                let trimmed = text.trim();
                if let Some(number) = trimmed.strip_suffix('%') {
                    let percent: u64 = number
                        .trim()
                        .parse()
                        .map_err(|_| D::Error::custom(format!("bad percentage: {text}")))?;
                    if percent > 100 {
                        return Err(D::Error::custom(format!("percentage above 100: {text}")));
                    }
                    return Ok(Self::Percent(percent));
                }
                parse_byte_size(trimmed)
                    .map(|bytes| Self::Bytes(ByteSize(bytes)))
                    .map_err(D::Error::custom)
            }
        }
    }
}

/// What a [`BehaviourCase`] changes when it fires.
///
/// Exactly one effect is implemented. `deny_unknown_fields` means a case naming any other
/// mutation fails to load rather than loading and doing nothing.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectCase {
    /// Replace the `ETag` with this value, verbatim.
    pub set_etag: String,
}

/// Which protocol a case runs over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
pub enum ProtocolCase {
    /// HTTP/1.1 in cleartext.
    #[serde(rename = "http/1.1")]
    #[default]
    Http11,
    /// HTTP/2 as h2c. Backlog B-8: not h2 over TLS, so ALPN is out of scope until S5.
    #[serde(rename = "http/2")]
    Http2,
}

/// What a redirect hop puts in its `Location` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RedirectLocationCase {
    /// A usable target.
    #[default]
    Normal,
    /// No `Location` header at all.
    Omitted,
    /// A `Location` that is not a usable URL.
    Unusable,
}

/// The representation to generate.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentCase {
    /// How many bytes. Accepts `1MB`, `512KiB`, or a plain integer.
    pub size: ByteSize,
    /// Which generator. Must name a generator this build implements (ADR-0010).
    #[serde(default = "default_generator")]
    pub generator: String,
    /// Seed. Fixing it is what makes a failing case reproducible forever.
    pub seed: u64,
}

fn default_transient_status() -> u16 {
    503
}

fn default_generator() -> String {
    GENERATOR_V1.to_owned()
}

/// How the server treats `Range`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum RangesCase {
    /// Honour ranges correctly.
    #[default]
    Supported,
    /// No `Accept-Ranges`, and a ranged request gets the whole body.
    Absent,
    /// Advertise `bytes`, then answer `200` anyway.
    Lies,
    /// `206` with a correct `Content-Range` but the whole body.
    IgnoreButClaim,
    /// `206` describing a range this far from the one requested.
    ShiftedContentRange {
        /// How far the described range is shifted.
        by: u64,
    },
    /// `206` with no `Content-Range`.
    OmitContentRange,
    /// `206` with this exact header value, however malformed.
    LiteralContentRange {
        /// The literal header value.
        value: String,
    },
    /// `206` with `bytes first-last/*`.
    UnknownTotalLength,
    /// `206` with a `multipart/byteranges` body in answer to a single-range request.
    MultipartByteranges,
}

/// How the body is framed. HTTP/1.1 only (backlog B-9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum FramingCase {
    /// An accurate `Content-Length`.
    #[default]
    ContentLength,
    /// `Transfer-Encoding: chunked`.
    Chunked,
    /// Neither a length nor chunking: the body ends with the connection.
    CloseDelimited,
    /// A `Content-Length` that disagrees with the body.
    WrongContentLength {
        /// The length to declare.
        declared: ByteSize,
    },
}

/// What the engine must do.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// Where the download must end up.
    pub final_state: FinalState,
    /// The stable `kind` string the failure must report. Required when `final_state` is
    /// `failed`: asserting only "it failed" would pass for the wrong reason, which is how a
    /// corpus stops detecting regressions.
    #[serde(default)]
    pub error_kind: Option<String>,
    /// Whether the final filename must exist. Asserted on **every** case (I-4).
    pub file_renamed: bool,
    /// The name the file must end up with, when the case pins it.
    #[serde(default)]
    pub filename: Option<String>,
    /// What the probe must have concluded about ranges (I-6).
    #[serde(default)]
    pub range_support: Option<RangeSupportCase>,
    /// The length the probe must have established.
    #[serde(default)]
    pub total_length: Option<ByteSize>,
    /// How many URLs the recorded redirect chain must contain, including the submitted one.
    #[serde(default)]
    pub redirect_chain_len: Option<usize>,
    /// The server must have received at least this many requests.
    ///
    /// The only way a case can prove a RETRY happened. A case that merely expects `completed` after
    /// injecting a transient failure passes just as well when the injection did nothing, so it
    /// asserts nothing about recovery — the same trap that made the first cross-host case vacuous.
    /// Counting requests distinguishes "recovered" from "never broke".
    #[serde(default)]
    pub min_requests: Option<usize>,
    /// Assert that no request used `HEAD`.
    ///
    /// `docs/03` §2.1 step 2: only `GET` observations are authoritative. A case that checks only the
    /// *conclusion* cannot tell whether the engine reached it by consulting `HEAD`, so this checks
    /// the requests the server actually recorded.
    #[serde(default)]
    pub forbids_head: Option<bool>,
    /// Whether no request may carry `If-Range` — that is, no resume may be attempted.
    ///
    /// Without this, a case whose point is "the engine must NOT resume here" is indistinguishable
    /// from one where it resumed successfully: an unchanged representation serves the range
    /// happily, so the file comes out correct either way and the case passes while proving
    /// nothing. Asserting on the request is the only way to see the decision rather than its
    /// accidental outcome.
    #[serde(default)]
    pub forbids_if_range: Option<bool>,
    /// The server must have accepted at least this many transport connections.
    ///
    /// What distinguishes a connection pathology from an ordinary download that happens to
    /// succeed. A probe and a body normally share one keep-alive connection, so a case whose
    /// point is "the origin stopped honouring keep-alive" is indistinguishable from a healthy
    /// transfer unless the connection count is asserted.
    #[serde(default)]
    pub min_connections: Option<usize>,
    /// The server must have dropped at least this many connections for exceeding its cap.
    ///
    /// The only way a capped case can prove the cap bit. Without it, a client that simply never
    /// opened a second connection passes identically to one that was refused, and the case would
    /// stay green if the cap were removed from the server entirely.
    #[serde(default)]
    pub min_refused_connections: Option<usize>,
    /// The origin's cap must have answered at least this many requests with its cap status.
    ///
    /// The polite cap's equivalent of `min_refused_connections`, and needed for the same reason:
    /// a client that never opened the extra connection produces exactly the same file as one that
    /// was told `429` and recovered, so without this the case stays green with the cap removed.
    #[serde(default)]
    pub min_capped_responses: Option<usize>,
    /// No more than this many connections may have carried a request.
    ///
    /// The only upper bound among the connection observations, and the only one that can say
    /// "the transfer finished inside the origin's budget". Every floor is satisfied by an engine
    /// that opened far too many and had the excess dropped, which is the opposite of the
    /// behaviour a connection budget requires.
    ///
    /// Counted as accepted minus refused, not as accepted: the kernel completes the handshake
    /// before the server can decide anything, so a connection the origin dropped is one the
    /// engine opened and got nothing from — it must not count against a budget the origin itself
    /// enforced.
    #[serde(default)]
    pub max_served_connections: Option<usize>,
    /// The server must have written at least this many responses nobody asked for.
    ///
    /// Without it a desync case is an ordinary download: the extra response is invisible in the
    /// outcome when the engine handles it correctly, which is precisely when the case is green.
    #[serde(default)]
    pub min_desynced_responses: Option<usize>,
    /// No single connection may have carried more than this many requests.
    ///
    /// Reuse, asserted from the server's side. A healthy origin carries the probe and the first
    /// range on one socket, so pinning this to one says something actively prevented that — which
    /// is the only way a case can show that a poisoned connection was retired rather than reused.
    #[serde(default)]
    pub max_requests_per_connection: Option<usize>,
    /// The server must have seen no more than this many requests.
    ///
    /// The only upper bound on requests, and the only way to assert that the engine did *not*
    /// retry. Every other case can show recovery; a case whose point is that recovery was never
    /// needed has nothing to show unless the absence is asserted.
    #[serde(default)]
    pub max_requests: Option<usize>,
    /// The server must have delayed at least this many body chunks.
    ///
    /// Without it a shaper that never fired leaves an ordinary download that passes for the wrong
    /// reason.
    #[serde(default)]
    pub min_delayed_chunks: Option<usize>,
    /// At least this many ranged requests must have been answered with the whole representation.
    #[serde(default)]
    pub min_ranges_ignored: Option<usize>,
    /// At least this many ranged requests must have been refused as unsatisfiable.
    #[serde(default)]
    pub min_unsatisfiable_responses: Option<usize>,
    /// At least this many `Content-Range` headers must have stated their total as `*`.
    #[serde(default)]
    pub min_starless_totals: Option<usize>,
    /// At least this many responses must have served a different span than was requested.
    ///
    /// Covers both directions — a range narrowed by a cap and one widened to the end of the file.
    /// Neither is malformed, so no other observation here can see that anything happened.
    #[serde(default)]
    pub min_respanned_ranges: Option<usize>,
    /// The server must not have written more than this many body bytes in total.
    ///
    /// The only observation that bounds *work* rather than outcome, and the only way to see the
    /// first half of what I-6 warns about: "downloads the file eight times and assembles
    /// nonsense". The second half is caught by the byte comparison; the waste is invisible to
    /// every other assertion here, because a download that fetched the representation once per
    /// worker and then failed looks exactly like one that failed immediately.
    #[serde(default)]
    pub max_body_bytes_served: Option<ByteSize>,
    /// At least this many requests must have waited for the origin's in-flight limit.
    ///
    /// A queue is invisible in the outcome: the file is identical whether the requests overlapped
    /// or were issued one at a time. Without this the case cannot tell an engine that queued
    /// behind the origin from one that never asked for concurrency at all.
    #[serde(default)]
    pub min_waited_requests: Option<usize>,
    /// The run must have taken at least this long.
    ///
    /// The only assertion in the corpus that is about the clock, and it exists for the one claim
    /// an outcome cannot carry: that a wait the server asked for was actually taken. The same file
    /// arrives whether the engine honoured `Retry-After` or substituted its own curve, so nothing
    /// else can tell them apart. Safe as a *floor* only because every other delay in a corpus run
    /// is collapsed to milliseconds; it must never be paired with an upper bound, which would make
    /// it a performance test on shared CI hardware.
    #[serde(default)]
    pub min_elapsed_ms: Option<u64>,
    /// Whether the final URL must be on a different origin than the submitted one.
    ///
    /// Without this, a cross-host case is indistinguishable from a same-host one: the chain length
    /// and the filename come out identical either way, so the case would pass even if the runner
    /// ignored `cross_origin` entirely. Verified by a mutation that did exactly that.
    #[serde(default)]
    pub crosses_origin: Option<bool>,
}

/// Terminal state of a case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FinalState {
    /// Verified and renamed.
    Completed,
    /// Did not produce a verified file.
    Failed,
}

/// Expected range-support classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RangeSupportCase {
    /// Proven by a validated `206` — the only thing that may set this (I-6).
    Proven,
    /// Observed absent.
    Absent,
}

/// A byte count, written as `1MB`, `512KiB`, `1GiB`, or a plain integer.
///
/// Both SI and binary suffixes are accepted because both appear in the specs: the exit criterion
/// says "1 GB" and filesystem limits are binary, and silently treating one as the other is the
/// kind of small wrongness that makes a length assertion mean nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSize(pub u64);

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Integer(u64),
            Text(String),
        }

        match Raw::deserialize(deserializer)? {
            Raw::Integer(value) => Ok(Self(value)),
            Raw::Text(text) => parse_byte_size(&text).map(Self).map_err(D::Error::custom),
        }
    }
}

fn parse_byte_size(text: &str) -> Result<u64, String> {
    let trimmed = text.trim();
    let digits_end = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (number, suffix) = trimmed.split_at(digits_end);
    let value: u64 = number
        .parse()
        .map_err(|_| format!("{text:?} does not start with a number"))?;

    let multiplier = match suffix.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1_u64,
        "KB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "KIB" | "K" => 1024,
        "MIB" | "M" => 1024 * 1024,
        "GIB" | "G" => 1024 * 1024 * 1024,
        other => return Err(format!("unknown size suffix {other:?} in {text:?}")),
    };
    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("{text:?} overflows u64"))
}

/// Why a case could not be turned into something runnable.
#[derive(Debug, thiserror::Error)]
pub enum CaseError {
    /// The YAML did not parse, or carried a key the runner does not understand.
    #[error("{path}: {source}")]
    Parse {
        /// The file that failed.
        path: String,
        /// The parse error.
        #[source]
        source: serde_norway::Error,
    },
    /// The case is structurally valid YAML but not usable.
    #[error("{id}: {reason}")]
    Invalid {
        /// The case id.
        id: String,
        /// What is wrong.
        reason: String,
    },
}

impl Case {
    /// Load a case from a YAML file.
    ///
    /// # Errors
    ///
    /// If the file cannot be read, does not parse, carries an unknown key, or fails validation.
    pub fn from_path(path: &std::path::Path) -> Result<Self, CaseError> {
        let text = std::fs::read_to_string(path).map_err(|error| CaseError::Invalid {
            id: path.display().to_string(),
            reason: format!("could not read: {error}"),
        })?;
        let case: Self = serde_norway::from_str(&text).map_err(|source| CaseError::Parse {
            path: path.display().to_string(),
            source,
        })?;
        case.validate()?;
        Ok(case)
    }

    /// Checks a schema cannot express.
    fn validate(&self) -> Result<(), CaseError> {
        let invalid = |reason: String| CaseError::Invalid {
            id: self.id.clone(),
            reason,
        };

        if self.references.is_empty() {
            return Err(invalid(
                "references is empty; every case must name the RFC clause or invariant it \
                 exercises so it can be maintained by someone who did not write it"
                    .to_owned(),
            ));
        }
        if self.server.content.generator != GENERATOR_V1 {
            return Err(invalid(format!(
                "generator {:?} is not implemented by this build; ADR-0010 gives a new generator \
                 a new name rather than redefining {GENERATOR_V1}",
                self.server.content.generator
            )));
        }
        // A mutation that fires at or before byte zero is not a *mid*-transfer mutation: the
        // representation would already have changed before the first request, so the case would
        // pass without anything ever changing under the client — the exact hole the old blanket
        // rejection existed to prevent.
        for entry in &self.server.behaviour {
            if entry.at.bytes_served.resolve(self.server.content.size.0) == 0 {
                return Err(invalid(
                    "server.behaviour has a mutation triggering at zero bytes served, so nothing \
                     changes mid-transfer and the case would pass without exercising a change"
                        .to_owned(),
                ));
            }
        }
        if self.expect.final_state == FinalState::Failed && self.expect.error_kind.is_none() {
            return Err(invalid(
                "final_state is failed but no error_kind is given; \"it failed somehow\" passes \
                 for the wrong reason and stops detecting regressions"
                    .to_owned(),
            ));
        }
        if self.server.cross_origin && self.server.redirect_chain.is_empty() {
            return Err(invalid(
                "cross_origin is set but redirect_chain is empty, so nothing would leave the \
                 first origin"
                    .to_owned(),
            ));
        }
        if self.expect.final_state == FinalState::Completed && !self.expect.file_renamed {
            return Err(invalid(
                "final_state is completed but file_renamed is false, which cannot both be true \
                 (I-4)"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    /// The content this case's server serves.
    #[must_use]
    pub fn content(&self) -> Content {
        Content::new(self.server.content.seed, self.server.content.size.0)
    }

    /// Translate the case into a [`ServerSpec`].
    #[must_use]
    pub fn server_spec(&self) -> ServerSpec {
        ServerSpec {
            max_concurrent_connections: self.server.max_concurrent_connections,
            cap_status: self.server.cap_status,
            tighten_cap: self.server.tighten_cap.map(|tighten| TightenCap {
                after_requests: tighten.after_requests,
                to: tighten.to,
            }),
            max_total_connections: self.server.max_total_connections,
            close_mid_headers: self.server.close_mid_headers,
            hangup_on_reused_request: self.server.hangup_on_reused_request,
            duplicate_response_after: self.server.duplicate_response_after,
            close_after_body_bytes: self.server.close_after_body_bytes.map(|size| size.0),
            drop_at_offset: self.server.drop_at_offset.map(|size| size.0),
            concurrent_requests: self
                .server
                .concurrent_requests
                .map(|limit| ConcurrentRequests {
                    limit: limit.limit,
                    reject: matches!(limit.then, OverLimitCase::Reject),
                }),
            stop_listening_after_connections: self.server.stop_listening_after_connections,
            cap_retry_after: self.server.cap_retry_after.clone(),
            withdraw_ranges_after: self.server.withdraw_ranges_after,
            ignore_range_at: self.server.ignore_range_at,
            shift_ranges_after: self.server.shift_ranges_after,
            status_416_after: self.server.status_416_after,
            unknown_total_after: self.server.unknown_total_after,
            serve_wrong_offset: self.server.serve_wrong_offset,
            ignore_range_end: self.server.ignore_range_end,
            halve_content_length_on_ranges: self.server.halve_content_length_on_ranges,
            cap_range_span: self.server.cap_range_span.map(|size| size.0),
            slow_segment: self.server.slow_segment.map(|slow| SlowSegment {
                from_offset: slow.from_offset.0,
                delay: std::time::Duration::from_millis(slow.delay_ms),
            }),
            inconsistent_total_after: self.server.inconsistent_total_after,
            close_without_responding: self.server.close_without_responding,
            close_after_requests: self.server.close_after_requests,
            protocol: match self.server.protocol {
                ProtocolCase::Http11 => Protocol::Http11,
                ProtocolCase::Http2 => Protocol::H2c,
            },
            content: self.content(),
            ranges: ranges_to_behaviour(&self.server.ranges),
            if_range: match self.server.if_range {
                IfRangeCase::Honoured => IfRangeBehaviour::Honoured,
                IfRangeCase::Ignored => IfRangeBehaviour::Ignored,
            },
            behaviour: self
                .server
                .behaviour
                .iter()
                .map(|entry| Mutation {
                    at_bytes_served: entry.at.bytes_served.resolve(self.server.content.size.0),
                    then: MutationEffect::SetEtag(entry.then.set_etag.clone()),
                })
                .collect(),
            framing: match self.server.framing {
                FramingCase::ContentLength => Framing::ContentLength,
                FramingCase::Chunked => Framing::Chunked,
                FramingCase::CloseDelimited => Framing::CloseDelimited,
                FramingCase::WrongContentLength { declared } => Framing::WrongContentLength {
                    declared: declared.0,
                },
            },
            etag: self.server.etag.clone(),
            last_modified: self.server.last_modified.clone(),
            content_encoding: self.server.content_encoding.clone(),
            content_type: self.server.content_type.clone(),
            content_disposition: self.server.content_disposition.clone(),
            digest: match self.server.digest.as_deref() {
                Some("computed") => Some(crate::server::DigestSpec::Computed),
                Some(literal) => Some(crate::server::DigestSpec::Literal(literal.to_owned())),
                None => None,
            },
            redirect_chain: self.server.redirect_chain.clone(),
            extra_headers: self
                .server
                .headers
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
            truncate_body_after: self.server.truncate_body_after.map(|size| size.0),
            status_override: self.server.status,
            body_override: self
                .server
                .body
                .as_ref()
                .map(|text| text.as_bytes().to_vec()),
            transient_body_failures: self.server.transient_body_failures,
            transient_status_failures: self.server.transient_status_failures,
            transient_retry_after: self.server.transient_retry_after.clone(),
            transient_status: self.server.transient_status,
            answer_head: true,
            omit_chunked_terminator: self.server.omit_chunked_terminator,
            redirect_location: match self.server.redirect_location {
                RedirectLocationCase::Normal => RedirectLocation::Normal,
                RedirectLocationCase::Omitted => RedirectLocation::Omitted,
                RedirectLocationCase::Unusable => RedirectLocation::Unusable,
            },
            redirect_loop: self.server.redirect_loop,
            // Filled in by the runner, which is the only thing that knows the other origin's URL.
            redirect_final_target: None,
            corrupt_from: self.server.corrupt_from.map(|size| size.0),
        }
    }
}

/// Map a case's declared range behaviour onto the server's.
///
/// A free function rather than a method so the runner can apply it to a *second* origin, which is
/// what I-6's `cdn-edge-disagrees` needs: the pathology is an edge and an origin that disagree, and
/// that cannot be expressed while both servers share one spec.
#[must_use]
pub fn ranges_to_behaviour(ranges: &RangesCase) -> RangeBehaviour {
    match ranges {
        RangesCase::Supported => RangeBehaviour::Supported,
        RangesCase::Absent => RangeBehaviour::Absent,
        RangesCase::Lies => RangeBehaviour::Lies,
        RangesCase::IgnoreButClaim => RangeBehaviour::IgnoreButClaim,
        RangesCase::ShiftedContentRange { by } => RangeBehaviour::ShiftedContentRange { by: *by },
        RangesCase::OmitContentRange => RangeBehaviour::OmitContentRange,
        RangesCase::LiteralContentRange { value } => {
            RangeBehaviour::LiteralContentRange(value.clone())
        }
        RangesCase::UnknownTotalLength => RangeBehaviour::UnknownTotalLength,
        RangesCase::MultipartByteranges => RangeBehaviour::MultipartByteranges,
    }
}
