//! The pathology server.
//!
//! `docs/09-testing-strategy.md` §3.3. This is the lab in which the server-compatibility space
//! is generated deliberately, instead of being discovered over twenty years in production
//! (ADR-0007). It serves deterministic content from [`crate::content`], enacts whatever
//! misbehaviour a case describes, and records every request it received so a case can assert on
//! request shape as well as on outcome.
//!
//! **The HTTP/1.1 side is written by hand.** That is not a preference. The pathologies that
//! matter most in the `framing` category are things a conforming HTTP library exists to prevent:
//! a `Content-Length` that disagrees with the body, a response with neither a length nor chunked
//! framing, a body that stops in the middle, a syntactically malformed `Content-Range`. `hyper`
//! would correct or refuse all of them, so the corpus would silently lose exactly the coverage
//! it was built for. HTTP/1.1 is simple enough to emit by hand; HTTP/2 is not, so that side uses
//! `hyper` and is limited to pathologies expressible through it.
//!
//! **Transport is cleartext**: HTTP/1.1 plain, and HTTP/2 as h2c with prior knowledge. Tests
//! must be local and offline (`docs/09` §7), and TLS would add certificate management for no S1
//! coverage: nothing in the `ranges`, `framing` or `redirects` categories depends on it. ALPN
//! negotiation and `Alt-Svc` are `protocols`-category concerns and belong to S5.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use hyper::body::{Body as HttpBody, Bytes, Frame, SizeHint};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::content::Content;

/// Which protocol the server speaks. One server speaks one protocol; a case that needs both
/// starts two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Protocol {
    /// HTTP/1.1 in cleartext, emitted by hand.
    #[default]
    Http11,
    /// HTTP/2 over cleartext with prior knowledge, via `hyper`.
    H2c,
}

/// What a redirect hop puts in its `Location` header.
///
/// Both malformed variants are cheap for a server to produce and are seen in the wild from
/// misconfigured reverse proxies. Neither leaves the client anywhere to go, so the engine has to
/// fail with a reason rather than following nothing or retrying forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RedirectLocation {
    /// A usable target.
    #[default]
    Normal,
    /// A redirect status with **no** `Location` header at all.
    Omitted,
    /// A `Location` that is not a usable URL.
    Unusable,
}

/// How the server treats a `Range` request.
///
/// Every variant other than [`Self::Supported`] is a documented real-world pathology, and each
/// is the reason `RangeSupport::Proven` requires evidence rather than an advertisement (I-6).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RangeBehaviour {
    /// Honour the range correctly: `206`, an accurate `Content-Range`, exactly those bytes.
    #[default]
    Supported,
    /// No `Accept-Ranges`, and a ranged request gets `200` with the whole representation.
    Absent,
    /// Advertise `Accept-Ranges: bytes`, then answer a ranged GET with `200` and the whole
    /// body. The `accept-ranges-lies` case: believing the advertisement makes N workers each
    /// download the entire file.
    Lies,
    /// `206` with an accurate `Content-Range`, but the whole body regardless. Only counting the
    /// delivered bytes catches this one.
    IgnoreButClaim,
    /// `206` whose `Content-Range` describes a range this far from the one requested. Writing
    /// the body at the requested offset corrupts the file at exactly the expected size.
    ShiftedContentRange {
        /// How far the described range is shifted.
        by: u64,
    },
    /// `206` with no `Content-Range` at all.
    OmitContentRange,
    /// `206` with this exact `Content-Range` header value, however malformed. The reason the
    /// HTTP/1.1 side is hand-rolled.
    LiteralContentRange(String),
    /// `206` with `bytes first-last/*` — legal, but leaves the length unknown, so there is
    /// nothing to segment.
    UnknownTotalLength,
    /// `206` with a `multipart/byteranges` body in answer to a **single**-range request.
    ///
    /// Legal only for a multi-range request (RFC 9110 §14.4), and some appliances and proxies emit
    /// it regardless. There is no top-level `Content-Range`, so the engine must not treat the body
    /// as raw bytes — writing it would put MIME boundaries and part headers into the file, at
    /// roughly the right size.
    MultipartByteranges,
}

/// How the response body is framed. HTTP/1.1 only; HTTP/2 has its own framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Framing {
    /// An accurate `Content-Length`.
    #[default]
    ContentLength,
    /// `Transfer-Encoding: chunked` and no `Content-Length`.
    Chunked,
    /// Neither: the body ends when the connection does, so "complete" and "truncated" are
    /// indistinguishable from the framing alone.
    CloseDelimited,
    /// A `Content-Length` of `declared`, followed by the real body and a close. A client that
    /// trusts the header and keeps whatever arrived writes a short file it believes is whole.
    WrongContentLength {
        /// The length to declare, whatever the body actually is.
        declared: u64,
    },
}

/// Everything a case can ask the server to do.
///
/// `Default` is a well-behaved server, so a case states only its pathology and the rest reads as
/// "correct". That keeps a case's diff to the thing it is actually testing.
#[derive(Debug, Clone)]
pub struct ServerSpec {
    /// Which protocol to speak.
    pub protocol: Protocol,
    /// The representation to serve.
    pub content: Content,
    /// How to treat `Range`.
    pub ranges: RangeBehaviour,
    /// How to frame the body.
    pub framing: Framing,
    /// `ETag`, verbatim — including a weak `W/"…"` when that is the point.
    pub etag: Option<String>,
    /// `Last-Modified`, verbatim.
    pub last_modified: Option<String>,
    /// `Content-Encoding` to declare. The body is *not* actually encoded: what is under test is
    /// whether the engine refuses to write it at a raw offset (I-5), not whether it can inflate.
    pub content_encoding: Option<String>,
    /// `Content-Type`.
    pub content_type: Option<String>,
    /// `Content-Disposition`, verbatim, traversal attempts included.
    pub content_disposition: Option<String>,
    /// `Repr-Digest`, verbatim (RFC 9530).
    pub digest: Option<String>,
    /// Statuses for a chain of redirect hops. Hop `i` redirects to hop `i + 1`; the last
    /// redirects to the content.
    pub redirect_chain: Vec<u16>,
    /// Extra headers, appended verbatim after everything else.
    pub extra_headers: Vec<(String, String)>,
    /// Stop writing the body after this many bytes and close.
    pub truncate_body_after: Option<u64>,
    /// Answer with this status instead of `200`/`206`.
    pub status_override: Option<u16>,
    /// Serve these bytes instead of generated content — an HTML login page, for instance.
    pub body_override: Option<Vec<u8>>,
    /// Fail this many *body* requests transiently before serving correctly.
    ///
    /// A failing request is truncated mid-body and the connection closed, which is what a reset looks
    /// like to a client. Only requests whose planned body exceeds one byte are counted, so the
    /// one-byte capability probe is untouched and the failure lands where it is interesting — in the
    /// transfer. This is what proves the retry policy actually recovers rather than merely deciding
    /// (docs/03 §7, exit criterion S1-C7).
    pub transient_body_failures: u32,
    /// Answer this many requests with `transient_status` and a `Retry-After` header before serving
    /// correctly. Proves `Retry-After` is honoured rather than replaced by our own back-off.
    pub transient_status_failures: u32,
    /// The `Retry-After` value sent with those failures.
    pub transient_retry_after: Option<String>,
    /// The status those failures carry. `503` unless a case says otherwise.
    pub transient_status: u16,
    /// Answer `HEAD` with headers and no body, as a conforming server does.
    ///
    /// The probe never sends `HEAD` — `docs/03` §2.1 step 2 says only `GET` observations are
    /// authoritative, because servers routinely answer the two differently. This exists so a case
    /// can prove the engine does not consult `HEAD` even when `HEAD` would tell it what it wants to
    /// hear (I-6's `head-differs-from-get`).
    pub answer_head: bool,
    /// Omit the terminating zero-length chunk of a chunked response, then close.
    ///
    /// Distinct from `truncate_body_after`, which produces a *well-formed* chunked response that is
    /// merely short. Without the terminator the message is incomplete at the protocol level, which
    /// a conforming client should detect from the framing rather than from a size comparison.
    pub omit_chunked_terminator: bool,
    /// How redirect hops report their destination.
    pub redirect_location: RedirectLocation,
    /// Where the last redirect hop points, instead of this server's own content.
    ///
    /// Set by the runner to another origin's URL, which is how a cross-host redirect is expressed:
    /// one server serves one origin, so a chain that leaves the origin needs two of them. I-8's
    /// proof list names `redirect-chain-to-other-host`, and hotlink protection and session-bound
    /// CDNs are exactly the situations where the chain's destination differs from where it started.
    pub redirect_final_target: Option<String>,
    /// Serve the entry path as a redirect that points at itself.
    ///
    /// A loop is only terminable by a hop limit, so this is how a case checks that the limit
    /// exists rather than that some particular chain length happens to work.
    pub redirect_loop: bool,
    /// Corrupt every byte from this offset onward, by inverting it.
    ///
    /// Exists for exactly one purpose: proving that the corpus can detect corruption at all. Every
    /// other case relies on the runner's byte comparison firing when it should, and without a
    /// server that can actually serve wrong bytes, that comparison is only ever observed passing —
    /// which is indistinguishable from a comparison that never runs. Used only by
    /// `tests/corpus/self-test/`.
    pub corrupt_from: Option<u64>,
}

impl Default for ServerSpec {
    fn default() -> Self {
        Self {
            protocol: Protocol::default(),
            content: Content::new(0, 0),
            ranges: RangeBehaviour::default(),
            framing: Framing::default(),
            etag: None,
            last_modified: None,
            content_encoding: None,
            content_type: None,
            content_disposition: None,
            digest: None,
            redirect_chain: Vec::new(),
            extra_headers: Vec::new(),
            truncate_body_after: None,
            status_override: None,
            body_override: None,
            transient_body_failures: 0,
            transient_status_failures: 0,
            transient_retry_after: None,
            transient_status: 503,
            answer_head: true,
            omit_chunked_terminator: false,
            redirect_location: RedirectLocation::Normal,
            redirect_final_target: None,
            redirect_loop: false,
            corrupt_from: None,
        }
    }
}

/// A request the server received, as it arrived.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    /// Request method.
    pub method: String,
    /// Request target, path and query.
    pub path: String,
    /// Header names are lowercased; values are verbatim.
    pub headers: Vec<(String, String)>,
}

impl RecordedRequest {
    /// Look up a header by name, case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    }
}

/// How many transient failures each budget still owes.
///
/// Shared across connections on purpose: a retry opens a new connection, and a per-connection counter
/// would let every attempt fail forever, which would prove the opposite of what the case intends.
#[derive(Debug)]
struct TransientBudget {
    body: AtomicU32,
    status: AtomicU32,
}

impl TransientBudget {
    fn new(spec: &ServerSpec) -> Self {
        Self {
            body: AtomicU32::new(spec.transient_body_failures),
            status: AtomicU32::new(spec.transient_status_failures),
        }
    }

    /// Claim one failure from `counter`, returning whether one was available.
    fn claim(counter: &AtomicU32) -> bool {
        counter
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
    }
}

/// A running pathology server. Stops when dropped.
pub struct PathologyServer {
    addr: SocketAddr,
    spec: Arc<ServerSpec>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    accept_task: tokio::task::JoinHandle<()>,
}

impl Drop for PathologyServer {
    fn drop(&mut self) {
        // Dropping the handle stops the accept loop; in-flight connections end with it. Tests
        // are short-lived, so there is nothing to drain.
        self.accept_task.abort();
    }
}

impl PathologyServer {
    /// Bind an ephemeral port on loopback and start serving.
    ///
    /// An ephemeral port rather than a fixed one so that corpus cases can run in parallel — with
    /// a fixed port they would collide, and the collision would look like a flaky test.
    pub async fn start(spec: ServerSpec) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let spec = Arc::new(spec);
        let requests = Arc::new(Mutex::new(Vec::new()));

        let budget = Arc::new(TransientBudget::new(&spec));
        let accept_task = tokio::spawn({
            let spec = Arc::clone(&spec);
            let requests = Arc::clone(&requests);
            let budget = Arc::clone(&budget);
            async move {
                loop {
                    let Ok((stream, _peer)) = listener.accept().await else {
                        return;
                    };
                    let spec = Arc::clone(&spec);
                    let requests = Arc::clone(&requests);
                    let budget = Arc::clone(&budget);
                    tokio::spawn(async move {
                        match spec.protocol {
                            Protocol::Http11 => {
                                serve_http11(stream, &spec, &requests, &budget).await;
                            }
                            Protocol::H2c => {
                                serve_h2c(stream, &spec, &requests, &budget).await;
                            }
                        }
                    });
                }
            }
        });

        Ok(Self {
            addr,
            spec,
            requests,
            accept_task,
        })
    }

    /// The address to connect to.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The base URL, without a path.
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// A full URL for `path`.
    #[must_use]
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    /// The path a client should start from: the first redirect hop if the case has a chain,
    /// otherwise the content itself.
    #[must_use]
    pub fn entry_path(&self) -> String {
        if self.spec.redirect_loop {
            LOOP_PATH.to_owned()
        } else if self.spec.redirect_chain.is_empty() {
            CONTENT_PATH.to_owned()
        } else {
            format!("{HOP_PREFIX}0")
        }
    }

    /// The full URL a client should start from.
    #[must_use]
    pub fn entry_url(&self) -> String {
        self.url(&self.entry_path())
    }

    /// Every request received so far, in arrival order.
    #[must_use]
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// How many requests have arrived. Handshake and reuse assertions count this.
    #[must_use]
    pub fn request_count(&self) -> usize {
        self.requests.lock().map(|guard| guard.len()).unwrap_or(0)
    }
}

/// Where the representation lives.
pub const CONTENT_PATH: &str = "/content";
/// Redirect hops are `/hop/0`, `/hop/1`, …
pub const HOP_PREFIX: &str = "/hop/";
/// A redirect that points at itself, so only a hop limit can stop it.
pub const LOOP_PATH: &str = "/loop";

/// The parsed shape of a request `Range` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestedRange {
    FromTo(u64, u64),
    From(u64),
    Suffix(u64),
}

impl RequestedRange {
    /// Resolve against a known total length into an inclusive `[first, last]`, or `None` when
    /// the range cannot be satisfied.
    fn resolve(self, total: u64) -> Option<(u64, u64)> {
        if total == 0 {
            return None;
        }
        match self {
            Self::FromTo(first, last) => {
                if first >= total {
                    None
                } else {
                    Some((first, last.min(total - 1)))
                }
            }
            Self::From(first) => {
                if first >= total {
                    None
                } else {
                    Some((first, total - 1))
                }
            }
            Self::Suffix(len) => {
                if len == 0 {
                    None
                } else {
                    Some((total.saturating_sub(len), total - 1))
                }
            }
        }
    }
}

fn parse_range(value: &str) -> Option<RequestedRange> {
    let spec = value.trim().strip_prefix("bytes=")?;
    // Only the first range spec is honoured; multipart range responses are an S3 concern.
    let first_spec = spec.split(',').next()?.trim();
    let (first, last) = first_spec.split_once('-')?;
    match (first.trim(), last.trim()) {
        ("", suffix) => suffix.parse().ok().map(RequestedRange::Suffix),
        (start, "") => start.parse().ok().map(RequestedRange::From),
        (start, end) => {
            let start = start.parse().ok()?;
            let end = end.parse().ok()?;
            if start > end {
                None
            } else {
                Some(RequestedRange::FromTo(start, end))
            }
        }
    }
}

/// What to answer with, decided from the request and the spec, before any bytes are written.
struct Plan {
    status: u16,
    /// Headers in emission order. Emitted verbatim, so a malformed value stays malformed.
    headers: Vec<(String, String)>,
    /// Byte range of the representation to send, inclusive, or `None` for no body.
    body: Option<(u64, u64)>,
    /// Literal body, which wins over `body` when present.
    literal_body: Option<Vec<u8>>,
}

fn plan(spec: &ServerSpec, path: &str, range_header: Option<&str>) -> Plan {
    // Redirects first: a hop never looks at Range or serves content.
    if path == LOOP_PATH {
        return Plan {
            status: 302,
            headers: vec![
                ("Location".to_owned(), LOOP_PATH.to_owned()),
                ("Content-Length".to_owned(), "0".to_owned()),
            ],
            body: None,
            literal_body: None,
        };
    }
    if let Some(index) = path
        .strip_prefix(HOP_PREFIX)
        .and_then(|i| i.parse::<usize>().ok())
        && let Some(status) = spec.redirect_chain.get(index).copied()
    {
        let next = if index + 1 < spec.redirect_chain.len() {
            format!("{HOP_PREFIX}{}", index + 1)
        } else {
            // An absolute URL when the case sends the chain to another origin; otherwise this
            // server's own content, resolved relatively.
            spec.redirect_final_target
                .clone()
                .unwrap_or_else(|| CONTENT_PATH.to_owned())
        };
        let mut headers = vec![("Content-Length".to_owned(), "0".to_owned())];
        match spec.redirect_location {
            RedirectLocation::Normal => headers.push(("Location".to_owned(), next)),
            // No Location at all: the status says "go elsewhere" and names nowhere.
            RedirectLocation::Omitted => {}
            // A scheme-only string cannot be resolved against the current URL, so it is not a
            // usable target however leniently it is read.
            RedirectLocation::Unusable => {
                headers.push(("Location".to_owned(), "http://".to_owned()));
            }
        }
        return Plan {
            status,
            headers,
            body: None,
            literal_body: None,
        };
    }

    let total = spec.content.len();
    let mut headers: Vec<(String, String)> = Vec::new();

    // Whether ranges are advertised is a separate question from whether they are honoured.
    // That gap is the `accept-ranges-lies` pathology.
    let advertises = !matches!(spec.ranges, RangeBehaviour::Absent);
    if advertises {
        headers.push(("Accept-Ranges".to_owned(), "bytes".to_owned()));
    }

    let requested = range_header.and_then(parse_range);
    let honour = matches!(
        spec.ranges,
        RangeBehaviour::Supported
            | RangeBehaviour::IgnoreButClaim
            | RangeBehaviour::ShiftedContentRange { .. }
            | RangeBehaviour::OmitContentRange
            | RangeBehaviour::LiteralContentRange(_)
            | RangeBehaviour::UnknownTotalLength
            | RangeBehaviour::MultipartByteranges
    );

    let mut status = 200_u16;
    let mut body = Some((0_u64, total.saturating_sub(1)));

    if let Some(requested) = requested
        && honour
    {
        match requested.resolve(total) {
            None => {
                // 416 with `bytes */total` is the conformant answer, and a case needs it to
                // check that the engine does not treat an unsatisfiable range as a failure of
                // range support in general.
                status = 416;
                headers.push(("Content-Range".to_owned(), format!("bytes */{total}")));
                return Plan {
                    status,
                    headers,
                    body: None,
                    literal_body: Some(Vec::new()),
                };
            }
            Some((first, last)) => {
                status = 206;
                match &spec.ranges {
                    RangeBehaviour::OmitContentRange => {}
                    RangeBehaviour::LiteralContentRange(literal) => {
                        headers.push(("Content-Range".to_owned(), literal.clone()));
                    }
                    RangeBehaviour::MultipartByteranges => {
                        // No top-level Content-Range: in a real multipart response the ranges are
                        // described per part, which is exactly why a client that expects one header
                        // and one body must refuse this rather than guess.
                        const BOUNDARY: &str = "DOWNPOUR_BOUNDARY";
                        headers.push((
                            "Content-Type".to_owned(),
                            format!("multipart/byteranges; boundary={BOUNDARY}"),
                        ));
                        let part = spec.content.range(first, last.saturating_add(1));
                        let mut envelope = Vec::new();
                        envelope.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
                        envelope.extend_from_slice(b"Content-Type: application/octet-stream\r\n");
                        envelope.extend_from_slice(
                            format!("Content-Range: bytes {first}-{last}/{total}\r\n\r\n")
                                .as_bytes(),
                        );
                        envelope.extend_from_slice(&part);
                        envelope.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
                        return Plan {
                            status: 206,
                            headers,
                            body: None,
                            literal_body: Some(envelope),
                        };
                    }
                    RangeBehaviour::UnknownTotalLength => {
                        headers.push((
                            "Content-Range".to_owned(),
                            format!("bytes {first}-{last}/*"),
                        ));
                    }
                    RangeBehaviour::ShiftedContentRange { by } => {
                        headers.push((
                            "Content-Range".to_owned(),
                            format!("bytes {}-{}/{total}", first + by, last + by),
                        ));
                    }
                    _ => {
                        headers.push((
                            "Content-Range".to_owned(),
                            format!("bytes {first}-{last}/{total}"),
                        ));
                    }
                }
                // The claim is in the header; whether the body agrees is the pathology.
                body = if matches!(spec.ranges, RangeBehaviour::IgnoreButClaim) {
                    Some((0, total.saturating_sub(1)))
                } else {
                    Some((first, last))
                };
            }
        }
    }

    if let Some(etag) = &spec.etag {
        headers.push(("ETag".to_owned(), etag.clone()));
    }
    if let Some(last_modified) = &spec.last_modified {
        headers.push(("Last-Modified".to_owned(), last_modified.clone()));
    }
    if let Some(encoding) = &spec.content_encoding {
        headers.push(("Content-Encoding".to_owned(), encoding.clone()));
    }
    if let Some(content_type) = &spec.content_type {
        headers.push(("Content-Type".to_owned(), content_type.clone()));
    }
    if let Some(disposition) = &spec.content_disposition {
        headers.push(("Content-Disposition".to_owned(), disposition.clone()));
    }
    if let Some(digest) = &spec.digest {
        headers.push(("Repr-Digest".to_owned(), digest.clone()));
    }
    for (name, value) in &spec.extra_headers {
        headers.push((name.clone(), value.clone()));
    }

    if let Some(override_status) = spec.status_override {
        status = override_status;
    }

    Plan {
        status,
        headers,
        body,
        literal_body: spec.body_override.clone(),
    }
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        410 => "Gone",
        416 => "Range Not Satisfiable",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

/// Bytes the plan will actually deliver.
/// How many bytes the plan will deliver, computed without producing them.
///
/// Separate from producing the bytes because a 1 GB case must never materialise 1 GB: the whole
/// point of the deterministic generator is that any range is computable on demand, and buffering
/// the body here would throw that away and cap the corpus at whatever fits in memory.
fn planned_body_len(spec: &ServerSpec, plan: &Plan) -> u64 {
    if let Some(literal) = &plan.literal_body {
        return u64::try_from(literal.len()).unwrap_or(0);
    }
    match plan.body {
        None => 0,
        Some((first, last)) if !spec.content.is_empty() => {
            last.saturating_sub(first).saturating_add(1)
        }
        Some(_) => 0,
    }
}

/// How many bytes will actually be written, after any truncation the case asked for.
fn bytes_to_write(spec: &ServerSpec, plan: &Plan) -> u64 {
    let full = planned_body_len(spec, plan);
    match spec.truncate_body_after {
        Some(limit) => limit.min(full),
        None => full,
    }
}

/// Chunk size for streaming a generated body. Large enough that the per-chunk overhead does not
/// dominate a gigabyte, small enough to exercise a client's chunk-boundary handling.
const STREAM_CHUNK: usize = 64 * 1024;

// ---------------------------------------------------------------- HTTP/1.1

async fn serve_http11(
    mut stream: TcpStream,
    spec: &ServerSpec,
    requests: &Arc<Mutex<Vec<RecordedRequest>>>,
    budget: &Arc<TransientBudget>,
) {
    let mut buffered: Vec<u8> = Vec::new();

    loop {
        // Read until a complete request head has arrived.
        let head_end = loop {
            if let Some(position) = find(&buffered, b"\r\n\r\n") {
                break position;
            }
            let mut chunk = [0_u8; 4096];
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(read) => buffered.extend_from_slice(&chunk[..read]),
            }
        };

        let head = String::from_utf8_lossy(&buffered[..head_end]).into_owned();
        buffered.drain(..head_end + 4);

        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap_or_default();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_owned();
        let path = parts.next().unwrap_or_default().to_owned();

        let mut headers = Vec::new();
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
            }
        }

        let range_header = headers
            .iter()
            .find(|(name, _)| name == "range")
            .map(|(_, value)| value.clone());
        let wants_close = headers
            .iter()
            .any(|(name, value)| name == "connection" && value.eq_ignore_ascii_case("close"));

        if let Ok(mut log) = requests.lock() {
            log.push(RecordedRequest {
                method: method.clone(),
                path: path.clone(),
                headers: headers.clone(),
            });
        }

        let mut plan = plan(spec, &path, range_header.as_deref());
        let mut full_len = planned_body_len(spec, &plan);
        let mut send_len = bytes_to_write(spec, &plan);

        // A transient status failure: answer with the configured status and Retry-After, spending one
        // unit of budget. Checked before the body failure so a case can use either independently.
        let mut forced_close = false;
        if spec.transient_status_failures > 0 && TransientBudget::claim(&budget.status) {
            let mut headers = vec![("Content-Length".to_owned(), "0".to_owned())];
            if let Some(after) = &spec.transient_retry_after {
                headers.push(("Retry-After".to_owned(), after.clone()));
            }
            plan = Plan {
                status: spec.transient_status,
                headers,
                body: None,
                literal_body: Some(Vec::new()),
            };
            full_len = 0;
            send_len = 0;
        } else if spec.transient_body_failures > 0
            && full_len > 1
            && TransientBudget::claim(&budget.body)
        {
            // Truncate mid-body and close: to the client this is a connection reset part way
            // through, which is the failure the retry policy exists for. Only bodies longer than one
            // byte qualify, so the capability probe is never the victim.
            send_len = full_len / 2;
            forced_close = true;
        }
        // A HEAD response carries the headers a GET would, Content-Length included, and no body.
        if method.eq_ignore_ascii_case("HEAD") && spec.answer_head {
            send_len = 0;
        }
        let truncated = send_len < full_len;

        // A redirect or an override answers with its own framing; only a content response takes
        // the case's framing pathology.
        let is_content_response = plan.body.is_some() || plan.literal_body.is_some();
        let framing = if is_content_response {
            spec.framing
        } else {
            Framing::ContentLength
        };

        let mut response = format!(
            "HTTP/1.1 {} {}\r\n",
            plan.status,
            reason_phrase(plan.status)
        );
        for (name, value) in &plan.headers {
            response.push_str(&format!("{name}: {value}\r\n"));
        }

        match framing {
            Framing::ContentLength => {
                response.push_str(&format!("Content-Length: {full_len}\r\n"));
            }
            Framing::WrongContentLength { declared } => {
                response.push_str(&format!("Content-Length: {declared}\r\n"));
            }
            Framing::Chunked => {
                response.push_str("Transfer-Encoding: chunked\r\n");
            }
            Framing::CloseDelimited => {
                response.push_str("Connection: close\r\n");
            }
        }
        response.push_str("\r\n");

        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }

        let wrote_body = match framing {
            Framing::Chunked => write_chunked(&mut stream, spec, &plan, send_len).await,
            _ => write_plain(&mut stream, spec, &plan, send_len).await,
        };
        if wrote_body.is_err() {
            return;
        }
        if stream.flush().await.is_err() {
            return;
        }

        // Any of these three means the response cannot be followed by another on this
        // connection: the client asked to close, the framing has no other terminator, or the
        // body deliberately disagrees with what was declared.
        let must_close = wants_close
            || matches!(
                framing,
                Framing::CloseDelimited | Framing::WrongContentLength { .. }
            )
            || truncated
            || forced_close
            || spec.omit_chunked_terminator;
        if must_close {
            let _ = stream.shutdown().await;
            return;
        }
    }
}

/// Stream `send_len` bytes of the planned body, generating each chunk on demand.
async fn write_plain(
    stream: &mut TcpStream,
    spec: &ServerSpec,
    plan: &Plan,
    send_len: u64,
) -> std::io::Result<()> {
    let mut buffer = vec![0_u8; STREAM_CHUNK];
    let mut written = 0_u64;
    while written < send_len {
        let piece = next_chunk(spec, plan, written, send_len, &mut buffer);
        if piece.is_empty() {
            break;
        }
        stream.write_all(piece).await?;
        written = written.saturating_add(u64::try_from(piece.len()).unwrap_or(0));
    }
    Ok(())
}

/// The same, with chunked transfer-encoding framing.
async fn write_chunked(
    stream: &mut TcpStream,
    spec: &ServerSpec,
    plan: &Plan,
    send_len: u64,
) -> std::io::Result<()> {
    // Several chunks rather than one: a single chunk covering the whole body would not exercise a
    // client's chunk-boundary handling, which is the point of the chunked case.
    let mut buffer = vec![0_u8; 16 * 1024];
    let mut written = 0_u64;
    while written < send_len {
        let piece = next_chunk(spec, plan, written, send_len, &mut buffer);
        if piece.is_empty() {
            break;
        }
        stream
            .write_all(format!("{:x}\r\n", piece.len()).as_bytes())
            .await?;
        stream.write_all(piece).await?;
        stream.write_all(b"\r\n").await?;
        written = written.saturating_add(u64::try_from(piece.len()).unwrap_or(0));
    }
    if spec.omit_chunked_terminator {
        // Leave the message unterminated and let the caller close the connection.
        return Ok(());
    }
    stream.write_all(b"0\r\n\r\n").await
}

/// Fill `buffer` with the next piece of the planned body and return the filled slice.
///
/// `offset` counts from the start of the *body*, not of the representation, so a ranged response
/// generates from the range's first byte.
fn next_chunk<'b>(
    spec: &ServerSpec,
    plan: &Plan,
    offset: u64,
    send_len: u64,
    buffer: &'b mut [u8],
) -> &'b [u8] {
    let remaining = send_len.saturating_sub(offset);
    let take = usize::try_from(remaining)
        .unwrap_or(usize::MAX)
        .min(buffer.len());

    if let Some(literal) = &plan.literal_body {
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(literal.len());
        let end = start.saturating_add(take).min(literal.len());
        let slice = literal.get(start..end).unwrap_or(&[]);
        let out = buffer.get_mut(..slice.len()).unwrap_or(&mut []);
        out.copy_from_slice(slice);
        return out;
    }

    let first = plan.body.map_or(0, |(first, _)| first);
    let absolute = first.saturating_add(offset);
    let out = buffer.get_mut(..take).unwrap_or(&mut []);
    spec.content.fill(absolute, out);
    corrupt(spec, absolute, out);
    out
}

/// Invert every byte at or after `spec.corrupt_from`. See that field's documentation.
fn corrupt(spec: &ServerSpec, absolute_start: u64, buffer: &mut [u8]) {
    let Some(from) = spec.corrupt_from else {
        return;
    };
    for (index, byte) in buffer.iter_mut().enumerate() {
        let offset = absolute_start.saturating_add(u64::try_from(index).unwrap_or(0));
        if offset >= from {
            *byte = !*byte;
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ---------------------------------------------------------------- HTTP/2 (h2c)

/// A response body generated chunk by chunk as `hyper` polls for it.
///
/// Exists so that an HTTP/2 case can serve a gigabyte without holding a gigabyte. Buffering the
/// body would cap the corpus at whatever fits in memory and would throw away the whole reason the
/// content generator is seekable.
struct GeneratedBody {
    content: Content,
    /// Set when the case supplies literal bytes (an HTML error page, say) instead of content.
    literal: Option<Vec<u8>>,
    /// Offset in the representation at which this body begins, so a ranged response generates
    /// from the range's first byte rather than from zero.
    first: u64,
    sent: u64,
    send_len: u64,
    /// See `ServerSpec::corrupt_from`.
    corrupt_from: Option<u64>,
}

impl GeneratedBody {
    fn empty() -> Self {
        Self {
            content: Content::new(0, 0),
            literal: Some(Vec::new()),
            first: 0,
            sent: 0,
            send_len: 0,
            corrupt_from: None,
        }
    }
}

impl HttpBody for GeneratedBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        // Every field is Unpin, so the pin can be discarded rather than projected through.
        let this = self.get_mut();
        if this.sent >= this.send_len {
            return Poll::Ready(None);
        }
        let remaining = this.send_len.saturating_sub(this.sent);
        let take = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(STREAM_CHUNK);

        let chunk = if let Some(literal) = &this.literal {
            let start = usize::try_from(this.sent)
                .unwrap_or(usize::MAX)
                .min(literal.len());
            let end = start.saturating_add(take).min(literal.len());
            Bytes::copy_from_slice(literal.get(start..end).unwrap_or(&[]))
        } else {
            let mut buffer = vec![0_u8; take];
            let absolute = this.first.saturating_add(this.sent);
            this.content.fill(absolute, &mut buffer);
            if let Some(from) = this.corrupt_from {
                for (index, byte) in buffer.iter_mut().enumerate() {
                    if absolute.saturating_add(u64::try_from(index).unwrap_or(0)) >= from {
                        *byte = !*byte;
                    }
                }
            }
            Bytes::from(buffer)
        };

        let len = u64::try_from(chunk.len()).unwrap_or(0);
        if len == 0 {
            return Poll::Ready(None);
        }
        this.sent = this.sent.saturating_add(len);
        Poll::Ready(Some(Ok(Frame::data(chunk))))
    }

    fn size_hint(&self) -> SizeHint {
        // Exact, so hyper can set Content-Length without buffering the body to measure it.
        SizeHint::with_exact(self.send_len)
    }
}

async fn serve_h2c(
    stream: TcpStream,
    spec: &ServerSpec,
    requests: &Arc<Mutex<Vec<RecordedRequest>>>,
    budget: &Arc<TransientBudget>,
) {
    let io = hyper_util::rt::TokioIo::new(stream);
    let service =
        hyper::service::service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
            let spec = spec.clone();
            let requests = Arc::clone(requests);
            let budget = Arc::clone(budget);
            async move {
                let path = request.uri().path().to_owned();
                let headers: Vec<(String, String)> = request
                    .headers()
                    .iter()
                    .map(|(name, value)| {
                        (
                            name.as_str().to_ascii_lowercase(),
                            String::from_utf8_lossy(value.as_bytes()).into_owned(),
                        )
                    })
                    .collect();

                if let Ok(mut log) = requests.lock() {
                    log.push(RecordedRequest {
                        method: request.method().as_str().to_owned(),
                        path: path.clone(),
                        headers: headers.clone(),
                    });
                }

                let range_header = headers
                    .iter()
                    .find(|(name, _)| name == "range")
                    .map(|(_, value)| value.clone());

                let mut plan = plan(&spec, &path, range_header.as_deref());
                let mut send_len = bytes_to_write(&spec, &plan);

                if spec.transient_status_failures > 0 && TransientBudget::claim(&budget.status) {
                    let mut headers = vec![];
                    if let Some(after) = &spec.transient_retry_after {
                        headers.push(("Retry-After".to_owned(), after.clone()));
                    }
                    plan = Plan {
                        status: spec.transient_status,
                        headers,
                        body: None,
                        literal_body: Some(Vec::new()),
                    };
                    send_len = 0;
                } else if spec.transient_body_failures > 0
                    && planned_body_len(&spec, &plan) > 1
                    && TransientBudget::claim(&budget.body)
                {
                    // hyper ends the stream cleanly at whatever the body yields, so the client sees
                    // a short body rather than a reset. Either way it is less than promised, which is
                    // what the retry has to survive.
                    send_len = planned_body_len(&spec, &plan) / 2;
                }
                let body = GeneratedBody {
                    content: spec.content,
                    literal: plan.literal_body.clone(),
                    first: plan.body.map_or(0, |(first, _)| first),
                    sent: 0,
                    send_len,
                    corrupt_from: spec.corrupt_from,
                };

                let mut builder = hyper::Response::builder().status(plan.status);
                for (name, value) in &plan.headers {
                    builder = builder.header(name, value);
                }
                // HTTP/2 has no Content-Length requirement and no chunked encoding; the framing
                // pathologies are HTTP/1.1-only and are documented as such on `Framing`.
                let response = builder
                    .body(body)
                    .unwrap_or_else(|_| hyper::Response::new(GeneratedBody::empty()));
                Ok::<_, std::convert::Infallible>(response)
            }
        });

    if let Err(error) =
        hyper::server::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
            .serve_connection(io, service)
            .await
    {
        // A client that closes mid-stream is normal in this test server; surface anything else.
        eprintln!("h2c connection ended: {error}");
    }
}
