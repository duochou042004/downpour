//! Retry and back-off policy.
//!
//! `docs/03-transfer-engine-spec.md` §7, and the half of "**robust** single-stream downloader" that
//! the stage is named for. Without this a single connection reset fails an entire download, which is
//! the most common failure a download manager exists to survive.
//!
//! The policy is a **pure function** over (what went wrong, which attempt this is, what the server
//! said). That is deliberate: it makes the whole table in §7 testable exhaustively without a
//! network, and it keeps the decision out of the transport code where it would be entangled with
//! I/O and hard to reason about.
//!
//! Two rules are easy to get subtly wrong and are called out in the code below:
//!
//! - **`Retry-After` is honoured exactly, never shortened.** Ignoring a server that asked for 30
//!   seconds is how a rate limit becomes a ban. The one exception is an absurd value, which is
//!   clamped so the engine cannot be wedged indefinitely by a broken or hostile origin.
//! - **Back-off is per-origin, not per-worker.** S1 has one worker so this cannot yet be observed,
//!   but [`RetryState`] is keyed by origin from the start: retrofitting that after S3 opens N
//!   connections would mean finding every call site under pressure, and five workers each retrying
//!   independently against a rate-limited origin is a self-inflicted denial of service.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

/// Why a request might be worth trying again.
///
/// Mapped from a status by [`TransientKind::from_status`], or constructed directly from a transport
/// failure. Deliberately not `#[non_exhaustive]`: the set is the table in §7, and adding to it
/// should be a visible change to every match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransientKind {
    /// The connection was reset, refused, or dropped.
    ConnectionReset,
    /// The request or the body timed out. Also `408`, which is a timeout rather than an ordinary
    /// client error — a distinction that is easy to lose.
    Timeout,
    /// `429`. Back off, and honour `Retry-After` if it is there.
    RateLimited,
    /// `503`. Same treatment as `429`; separate so logs and `dp explain` can tell them apart.
    ServiceUnavailable,
    /// Another `5xx`. Retry up to the budget, then the download is `Stalled`.
    ServerError,
    /// The body ended before the length the response promised. Transient — the connection died —
    /// but it must count against the budget, or a server that truncates every response loops
    /// forever.
    TruncatedBody,
    /// A `4xx` that retrying cannot fix. Retrying a `404` cannot change it, and it would spend the
    /// budget a later transient failure needs.
    NotRetryable,
}

impl TransientKind {
    /// Classify a response status against the table in `docs/03-transfer-engine-spec.md` §7.
    ///
    /// `None` means "not this module's business": `401`, `403` and `410` go to the URL-refresh flow
    /// (I-8), which keeps the bytes already fetched instead of retrying a URL that has expired.
    #[must_use]
    pub fn from_status(status: u16) -> Option<Self> {
        match status {
            401 | 403 | 410 => None,
            408 => Some(Self::Timeout),
            429 => Some(Self::RateLimited),
            503 => Some(Self::ServiceUnavailable),
            500..=599 => Some(Self::ServerError),
            400..=499 => Some(Self::NotRetryable),
            _ => None,
        }
    }

    /// Whether this kind is worth another attempt at all, budget aside.
    #[must_use]
    pub fn is_retryable(self) -> bool {
        !matches!(self, Self::NotRetryable)
    }
}

/// What to do about a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    /// Wait this long, then try again.
    RetryAfter(Duration),
    /// Stop. Either the budget is spent or the failure is not the retryable kind.
    GiveUp,
}

/// The budget and the curve.
///
/// Defaults are the parameters in `docs/03-transfer-engine-spec.md` §9. Changing one requires a
/// benchmark showing why, per that section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    max_retries: u32,
    base_delay: Duration,
    max_delay: Duration,
    max_server_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            // §9 MAX_RETRIES.
            max_retries: 5,
            // §7 "base 500 ms, cap 30 s".
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
            // Not from the spec: a ceiling on what a *server* may ask for. Honouring Retry-After
            // exactly is the rule, but a value of a year is broken or hostile, and obeying it would
            // wedge the download with no way for a user to tell why.
            //
            // One hour, not five minutes. A ten-minute Retry-After is an ordinary thing for a rate
            // limiter to send and must be obeyed; the ceiling exists only to catch the absurd. Two
            // tests caught this by contradicting each other when the ceiling was 300 s — one
            // asserting 600 s is honoured, the other asserting absurd values are clamped.
            max_server_delay: Duration::from_secs(3600),
        }
    }
}

impl RetryPolicy {
    /// How many retries are allowed after the first attempt.
    #[must_use]
    pub fn max_retries(&self) -> u32 {
        self.max_retries
    }

    /// The first computed delay, and the floor for every later one.
    #[must_use]
    pub fn base_delay(&self) -> Duration {
        self.base_delay
    }

    /// The ceiling on a *computed* delay.
    #[must_use]
    pub fn max_delay(&self) -> Duration {
        self.max_delay
    }

    /// The ceiling on a delay a *server* asked for via `Retry-After`.
    #[must_use]
    pub fn max_server_delay(&self) -> Duration {
        self.max_server_delay
    }

    /// Decide what to do about `kind` on attempt `attempt` (zero-based), given the response's
    /// `Retry-After` header if it carried one.
    ///
    /// ```
    /// use std::time::Duration;
    /// use downpour_http::retry::{RetryDecision, RetryPolicy, TransientKind};
    ///
    /// let policy = RetryPolicy::default();
    ///
    /// // A server that names a number is obeyed, not second-guessed.
    /// assert_eq!(
    ///     policy.decide(TransientKind::RateLimited, 0, Some("120")),
    ///     RetryDecision::RetryAfter(Duration::from_secs(120)),
    /// );
    ///
    /// // Retrying a 404 cannot change it.
    /// assert_eq!(
    ///     policy.decide(TransientKind::NotRetryable, 0, None),
    ///     RetryDecision::GiveUp,
    /// );
    /// ```
    #[must_use]
    pub fn decide(
        &self,
        kind: TransientKind,
        attempt: u32,
        retry_after: Option<&str>,
    ) -> RetryDecision {
        if !kind.is_retryable() || attempt >= self.max_retries {
            return RetryDecision::GiveUp;
        }

        // The server's instruction wins over our curve. Clamped, but not shortened toward zero.
        if let Some(header) = retry_after
            && let Some(requested) = parse_retry_after(header, SystemTime::now())
        {
            return RetryDecision::RetryAfter(requested.min(self.max_server_delay));
        }

        RetryDecision::RetryAfter(self.backoff(attempt))
    }

    /// Jittered exponential back-off: `base * 2^attempt`, capped, then randomised within the upper
    /// half of that window — "equal jitter", so the delay lands in `[full/2, full)`.
    ///
    /// Jitter matters more than it looks. Without it, N workers that failed together retry together
    /// forever, which reproduces the thundering herd the back-off exists to prevent.
    ///
    /// The window is deliberately below the nominal curve rather than above it. Jittering upward
    /// would make every delay longer than the spec's stated base and would collapse to no spread at
    /// all once the cap is reached, which is exactly when decorrelation matters most. Half of 500 ms
    /// is 250 ms, which is not hammering anything.
    fn backoff(&self, attempt: u32) -> Duration {
        let exponential = self
            .base_delay
            .checked_mul(1_u32.checked_shl(attempt.min(16)).unwrap_or(u32::MAX))
            .unwrap_or(self.max_delay)
            .min(self.max_delay);

        let millis = u64::try_from(exponential.as_millis()).unwrap_or(u64::MAX);
        let half = millis / 2;
        // Deterministic per-call randomness without pulling in a PRNG: the low bits of the clock
        // are more than enough to decorrelate retries, and nothing here is security-sensitive.
        let spread = if half == 0 {
            0
        } else {
            jitter_source() % half.max(1)
        };
        Duration::from_millis(half.saturating_add(spread).max(1))
    }
}

/// A cheap, non-cryptographic source of variation for jitter.
fn jitter_source() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0)
}

/// Parse `Retry-After` (RFC 9110 §10.2.3): either delay-seconds or an HTTP-date.
///
/// Returns `None` for anything unparseable, which the caller treats as "fall back to computed
/// back-off". Never treat a malformed header as "retry immediately" — that hammers the origin — nor
/// as fatal, which would fail a download over a cosmetic header bug.
fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    // delay-seconds is a plain non-negative integer. Rejecting "12.5" and "-5" explicitly rather
    // than letting a lenient parse round or wrap.
    if trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return trimmed.parse::<u64>().ok().map(Duration::from_secs);
    }

    // HTTP-date. A date already in the past means "retry now" rather than "wait a negative time".
    let when = httpdate::parse_http_date(trimmed).ok()?;
    Some(when.duration_since(now).unwrap_or(Duration::ZERO))
}

/// Per-origin retry bookkeeping.
///
/// **Keyed by origin, not by worker, and that is the point** (§7's closing line). S1 runs one
/// worker so the distinction is invisible today; S3 opens N, and at that point five workers each
/// keeping their own counter against one rate-limited origin is a self-inflicted denial of service.
/// Introducing the shape now costs nothing and means S3 inherits the correct behaviour instead of
/// having to retrofit it.
#[derive(Debug, Default)]
pub struct RetryState {
    attempts: HashMap<String, u32>,
}

impl RetryState {
    /// A fresh tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many attempts this origin has already consumed.
    #[must_use]
    pub fn attempts(&self, origin: &str) -> u32 {
        self.attempts.get(origin).copied().unwrap_or(0)
    }

    /// Record a failed attempt against `origin` and return the new count.
    pub fn record_failure(&mut self, origin: &str) -> u32 {
        let counter = self.attempts.entry(origin.to_owned()).or_insert(0);
        *counter = counter.saturating_add(1);
        *counter
    }

    /// Forget an origin's history, after a success.
    pub fn reset(&mut self, origin: &str) {
        self.attempts.remove(origin);
    }
}
