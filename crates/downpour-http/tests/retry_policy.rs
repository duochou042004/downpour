//! S1-T15 — the retry and back-off policy from `docs/03-transfer-engine-spec.md` §7.
//!
//! Exit criterion **S1-C7**. The policy is a pure function over (what went wrong, which attempt
//! this is, what the server said) so it can be tested exhaustively without a network — the timing
//! behaviour it produces is then observed against the pathology server separately.
//!
//! Two rules here are load-bearing and easy to get subtly wrong:
//!
//! - **`Retry-After` is honoured exactly, never shortened.** A server that says "wait 30 seconds"
//!   and gets a request at 29 is being told the client will not cooperate, and the usual
//!   consequence is a longer ban rather than a served file.
//! - **Back-off is per-origin, not per-worker** (§7's closing line). Five workers each retrying
//!   independently against a rate-limited origin is a self-inflicted denial of service. S1 has one
//!   worker, so this cannot be observed yet — but the *type* has to carry the origin now, because
//!   retrofitting it after S3 opens N connections means finding every call site under pressure.

use std::time::Duration;

use downpour_http::retry::{RetryDecision, RetryPolicy, TransientKind};

fn policy() -> RetryPolicy {
    RetryPolicy::default()
}

// ---------------------------------------------------------------- the budget

#[test]
fn the_default_budget_matches_the_spec() {
    // docs/03 §9: MAX_RETRIES 5, base 500 ms, cap 30 s.
    let policy = policy();
    assert_eq!(policy.max_retries(), 5);
    assert_eq!(policy.base_delay(), Duration::from_millis(500));
    assert_eq!(policy.max_delay(), Duration::from_secs(30));
}

#[test]
fn a_transient_failure_is_retried_until_the_budget_is_spent() {
    let policy = policy();
    for attempt in 0..5 {
        let decision = policy.decide(TransientKind::ConnectionReset, attempt, None);
        assert!(
            matches!(decision, RetryDecision::RetryAfter(_)),
            "attempt {attempt} should retry, got {decision:?}"
        );
    }
    // The sixth attempt has exhausted MAX_RETRIES.
    assert!(matches!(
        policy.decide(TransientKind::ConnectionReset, 5, None),
        RetryDecision::GiveUp
    ));
}

#[test]
fn back_off_grows_exponentially_and_is_capped() {
    let policy = policy();
    let mut previous = Duration::ZERO;
    for attempt in 0..8 {
        let RetryDecision::RetryAfter(delay) = policy.decide(TransientKind::Timeout, attempt, None)
        else {
            // Past the budget the answer is GiveUp, which the previous test covers.
            break;
        };
        assert!(
            delay <= policy.max_delay(),
            "attempt {attempt} exceeded the cap: {delay:?}"
        );
        assert!(
            delay >= previous || previous >= policy.max_delay(),
            "attempt {attempt} went backwards"
        );
        previous = delay;
    }
    assert!(
        previous > policy.base_delay(),
        "the delay never grew: {previous:?}"
    );
}

#[test]
fn back_off_is_jittered_so_retries_do_not_synchronise() {
    // Without jitter, N workers that failed together retry together, which reproduces the thundering
    // herd the back-off exists to prevent. Sampling the same attempt must not always give the same
    // delay.
    let policy = policy();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..64 {
        if let RetryDecision::RetryAfter(delay) =
            policy.decide(TransientKind::ConnectionReset, 3, None)
        {
            seen.insert(delay.as_millis());
        }
    }
    assert!(
        seen.len() > 1,
        "delay for attempt 3 is deterministic ({seen:?}); no jitter applied"
    );
}

// ---------------------------------------------------------------- Retry-After

#[test]
fn retry_after_in_seconds_is_honoured_exactly() {
    // Not scaled, not jittered, not capped by max_delay: the server named a number.
    let policy = policy();
    let decision = policy.decide(TransientKind::RateLimited, 0, Some("120"));
    assert_eq!(
        decision,
        RetryDecision::RetryAfter(Duration::from_secs(120))
    );
}

#[test]
fn retry_after_overrides_the_back_off_cap() {
    // 30 s is our cap for *computed* delays, but ten minutes is an ordinary thing for a rate
    // limiter to ask for and must be obeyed — shortening it is how a rate limit becomes a ban. The
    // ceiling on server-requested delays exists to catch the absurd, not the merely long.
    let policy = policy();
    let decision = policy.decide(TransientKind::ServiceUnavailable, 0, Some("600"));
    assert_eq!(
        decision,
        RetryDecision::RetryAfter(Duration::from_secs(600))
    );
}

#[test]
fn retry_after_zero_means_retry_immediately() {
    let policy = policy();
    assert_eq!(
        policy.decide(TransientKind::RateLimited, 0, Some("0")),
        RetryDecision::RetryAfter(Duration::ZERO)
    );
}

#[test]
fn an_http_date_retry_after_is_understood() {
    // RFC 9110 §10.2.3 allows either delay-seconds or an HTTP-date, and real CDNs send both. A
    // parser that only handled integers would silently fall back to its own back-off and retry
    // early against a server that had asked for longer.
    let policy = policy();
    let far_future =
        httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(300));
    let RetryDecision::RetryAfter(delay) =
        policy.decide(TransientKind::RateLimited, 0, Some(&far_future))
    else {
        panic!("an HTTP-date Retry-After must be understood");
    };
    // Allow a couple of seconds of slack for the clock moving between formatting and parsing.
    assert!(
        delay >= Duration::from_secs(290) && delay <= Duration::from_secs(300),
        "expected about 300 s, got {delay:?}"
    );
}

#[test]
fn an_http_date_in_the_past_means_retry_immediately() {
    let policy = policy();
    let past = httpdate::fmt_http_date(std::time::SystemTime::now() - Duration::from_secs(60));
    assert_eq!(
        policy.decide(TransientKind::RateLimited, 0, Some(&past)),
        RetryDecision::RetryAfter(Duration::ZERO)
    );
}

#[test]
fn a_malformed_retry_after_falls_back_to_computed_back_off() {
    // Never treat an unparseable header as "retry now", which would hammer the origin, nor as
    // fatal, which would fail a download over a cosmetic header bug.
    let policy = policy();
    for bad in ["", "soon", "-5", "12.5", "NaN", "Tue, 99 Xxx 9999"] {
        let decision = policy.decide(TransientKind::RateLimited, 0, Some(bad));
        assert!(
            matches!(decision, RetryDecision::RetryAfter(d) if d >= policy.base_delay() / 2),
            "{bad:?} should fall back to computed back-off, got {decision:?}"
        );
    }
}

#[test]
fn an_absurd_retry_after_is_clamped_rather_than_trusted() {
    // A server asking us to wait a year is either broken or hostile. Honouring it exactly would
    // hang the download forever, so it is clamped to a documented ceiling — the one place where
    // "honour it exactly" yields to not being wedged.
    let policy = policy();
    let RetryDecision::RetryAfter(delay) =
        policy.decide(TransientKind::RateLimited, 0, Some("31536000"))
    else {
        panic!("should still be a retry decision");
    };
    assert!(delay <= policy.max_server_delay(), "not clamped: {delay:?}");
    assert!(
        delay > policy.max_delay(),
        "clamped too aggressively: {delay:?}"
    );
}

// ---------------------------------------------------------------- what is NOT retried

#[test]
fn a_client_error_is_not_retried() {
    // §7: a 4xx other than 401/403/408/410/429 is unrecoverable for this URL. Retrying cannot
    // change a 404, and doing so wastes the budget that a later transient failure will need.
    let policy = policy();
    assert_eq!(
        policy.decide(TransientKind::NotRetryable, 0, None),
        RetryDecision::GiveUp
    );
}

#[test]
fn a_rate_limit_without_retry_after_still_backs_off() {
    let policy = policy();
    let RetryDecision::RetryAfter(delay) = policy.decide(TransientKind::RateLimited, 0, None)
    else {
        panic!("429 without Retry-After must still be retried");
    };
    // Equal jitter, so attempt 0 lands in [base/2, base). Asserting `>= base` would encode an
    // assumption the spec does not make; what matters is that the wait is meaningful.
    assert!(
        delay >= policy.base_delay() / 2,
        "{delay:?} is too short to be a back-off"
    );
    assert!(
        delay < policy.base_delay(),
        "{delay:?} exceeds the first-attempt window"
    );
}

#[test]
fn classification_from_a_status_matches_the_spec_table() {
    // The exact table in docs/03 §7. Getting 408 wrong is a common slip: it is a *timeout*, which
    // is transient, not an ordinary client error.
    let cases = [
        (408_u16, Some(TransientKind::Timeout)),
        (429, Some(TransientKind::RateLimited)),
        (500, Some(TransientKind::ServerError)),
        (502, Some(TransientKind::ServerError)),
        (503, Some(TransientKind::ServiceUnavailable)),
        (504, Some(TransientKind::ServerError)),
        // Handled by the refresh flow (I-8), not by retrying the same URL.
        (401, None),
        (403, None),
        (410, None),
        // Unrecoverable for this URL.
        (400, Some(TransientKind::NotRetryable)),
        (404, Some(TransientKind::NotRetryable)),
        (451, Some(TransientKind::NotRetryable)),
    ];
    for (status, expected) in cases {
        assert_eq!(
            TransientKind::from_status(status),
            expected,
            "status {status}"
        );
    }
}

#[test]
fn a_truncated_body_is_retryable_and_counts_against_the_budget() {
    // §7's last row. It is transient — the connection died — but it must not be free, or a server
    // that truncates every response would loop forever.
    let policy = policy();
    assert!(matches!(
        policy.decide(TransientKind::TruncatedBody, 0, None),
        RetryDecision::RetryAfter(_)
    ));
    assert_eq!(
        policy.decide(TransientKind::TruncatedBody, 5, None),
        RetryDecision::GiveUp
    );
}
