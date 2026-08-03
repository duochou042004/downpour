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
use crate::server::{Framing, Protocol, RangeBehaviour, ServerSpec};

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
    /// Triggered mid-transfer mutations (ADR-0010's `behaviour`).
    ///
    /// Accepted so the ADR's schema stays valid, but **rejected when non-empty**: the server
    /// cannot enact them until S2, and silently ignoring them would make an `etag-changed-midway`
    /// case pass without changing anything mid-transfer. Failing closed is the whole point.
    #[serde(default)]
    pub behaviour: Vec<serde_norway::Value>,
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
        if !self.server.behaviour.is_empty() {
            return Err(invalid(
                "server.behaviour describes mid-transfer mutations, which the pathology server \
                 cannot enact until S2. Accepting it silently would let the case pass without \
                 anything changing mid-transfer"
                    .to_owned(),
            ));
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
            protocol: match self.server.protocol {
                ProtocolCase::Http11 => Protocol::Http11,
                ProtocolCase::Http2 => Protocol::H2c,
            },
            content: self.content(),
            ranges: match &self.server.ranges {
                RangesCase::Supported => RangeBehaviour::Supported,
                RangesCase::Absent => RangeBehaviour::Absent,
                RangesCase::Lies => RangeBehaviour::Lies,
                RangesCase::IgnoreButClaim => RangeBehaviour::IgnoreButClaim,
                RangesCase::ShiftedContentRange { by } => {
                    RangeBehaviour::ShiftedContentRange { by: *by }
                }
                RangesCase::OmitContentRange => RangeBehaviour::OmitContentRange,
                RangesCase::LiteralContentRange { value } => {
                    RangeBehaviour::LiteralContentRange(value.clone())
                }
                RangesCase::UnknownTotalLength => RangeBehaviour::UnknownTotalLength,
                RangesCase::MultipartByteranges => RangeBehaviour::MultipartByteranges,
            },
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
            digest: self.server.digest.clone(),
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
            redirect_loop: self.server.redirect_loop,
            // Filled in by the runner, which is the only thing that knows the other origin's URL.
            redirect_final_target: None,
            corrupt_from: self.server.corrupt_from.map(|size| size.0),
        }
    }
}
