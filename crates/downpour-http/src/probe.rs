//! When cached capability evidence must be re-established before resumed work is planned.
//!
//! This module owns the decision half of **I-6**: range support is proven, never assumed — and
//! evidence proven at one moment, against one URL, is not evidence about a different moment or a
//! different URL. It also guards **I-8**, since the recorded final URL is what a resume fetches,
//! and **I-3**, because the capability evidence and the validator were recorded together.
//!
//! `docs/03-transfer-engine-spec.md` §2.3 names four triggers. They are implemented here as a
//! pure function of what is known at the moment a resume is planned, which is what makes them
//! testable exhaustively rather than through a server that happens to misbehave.
//!
//! The negative half is load-bearing. Re-probing on every transient failure would replace the
//! recorded validator with one taken from the retry's own response, and a validator fetched now
//! matches whatever the server is serving now — which is the I-3 check S2-T9 exists to make,
//! thrown away. So a `503` is not a statement about capabilities, and this module says so.

use std::time::{Duration, SystemTime};

use downpour_types::{RangeSupport, RemoteObject};
use url::Url;

/// How long capability evidence stays usable without re-probing.
///
/// `docs/03-transfer-engine-spec.md` §2.3 requires a freshness window but §9 never gave it a
/// value; fifteen minutes is the default this implementation adds, and §9 now records it.
///
/// The bound comes from both directions. Retry back-off caps at 30 s over at most `MAX_RETRIES`
/// attempts (§7), so a full in-call retry cycle is a couple of minutes — comfortably inside the
/// window, which is what stops an ordinary retry from re-probing. And a download resumed after a
/// genuine pause is outside it, which is the case the trigger exists for: signed URLs expire,
/// CDN edges rotate, and files get replaced while nobody is watching.
pub const CAPABILITY_FRESHNESS: Duration = Duration::from_secs(15 * 60);

/// Why cached capability evidence can no longer be trusted (§2.3).
///
/// Each variant carries what it observed rather than just a discriminant, so a daemon can say
/// *which* URL moved or *how* stale the evidence was without re-deriving it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReprobeTrigger {
    /// The user supplied a refreshed URL, so the cached evidence is superseded by intent.
    RefreshedUrlSupplied {
        /// The URL the user supplied.
        url: Url,
    },
    /// The URL about to be fetched is not the one the evidence was proven against (I-8).
    FinalUrlChanged {
        /// The final URL the probe recorded.
        recorded: Url,
        /// The URL this resume would fetch.
        planned: Url,
    },
    /// A worker observed a status that the recorded capabilities say should not happen.
    StatusContradictsCapabilities {
        /// The status that was observed.
        status: u16,
        /// Which recorded claim it contradicts, in terms a log line can use.
        reason: &'static str,
    },
    /// The evidence is older than the freshness window, or its age cannot be determined.
    FreshnessWindowElapsed {
        /// How long ago the probe ran. Saturates at the window when the clock moved backwards.
        elapsed: Duration,
        /// The window in force.
        window: Duration,
    },
}

/// Everything known at the moment a resume is about to be planned.
#[derive(Clone, Debug)]
pub struct ResumePlan<'a> {
    /// The capability evidence recorded when the existing bytes were fetched.
    pub recorded: &'a RemoteObject,
    /// The current time, supplied rather than read so the decision stays a pure function.
    pub now: SystemTime,
    /// The URL this resume would fetch.
    pub planned_url: &'a Url,
    /// A URL the user supplied to replace a stale one. The workflow that obtains one is S8;
    /// S2 only guarantees that supplying one invalidates the cached evidence.
    pub refreshed_url: Option<&'a Url>,
    /// The status a worker last observed, when one was observed at all.
    pub last_observed_status: Option<u16>,
}

/// Decides whether capability evidence must be re-established before a resume is planned.
#[derive(Clone, Copy, Debug)]
pub struct ReprobePolicy {
    freshness: Duration,
}

impl Default for ReprobePolicy {
    fn default() -> Self {
        Self::new(CAPABILITY_FRESHNESS)
    }
}

impl ReprobePolicy {
    /// A policy with the given freshness window.
    #[must_use]
    pub const fn new(freshness: Duration) -> Self {
        Self { freshness }
    }

    /// The freshness window in force.
    #[must_use]
    pub const fn freshness(&self) -> Duration {
        self.freshness
    }

    /// Whether a re-probe is required before this resume may be planned.
    #[must_use]
    pub fn is_required(&self, plan: &ResumePlan<'_>) -> bool {
        !self.triggers(plan).is_empty()
    }

    /// Every §2.3 trigger that applies, in the order they are listed above.
    ///
    /// All of them, not the first: a caller that logs why it re-probed should not be told half
    /// the reason, and short-circuiting would hide a second problem behind the first.
    #[must_use]
    pub fn triggers(&self, plan: &ResumePlan<'_>) -> Vec<ReprobeTrigger> {
        let mut triggers = Vec::new();

        // A supplied refresh is a signal from the user, not a string comparison. One that
        // happens to equal the recorded URL still says "I went and got this again", and staying
        // quiet because the text matched would make the trigger depend on an accident.
        if let Some(url) = plan.refreshed_url {
            triggers.push(ReprobeTrigger::RefreshedUrlSupplied { url: url.clone() });
        }

        if plan.planned_url != &plan.recorded.final_url {
            triggers.push(ReprobeTrigger::FinalUrlChanged {
                recorded: plan.recorded.final_url.clone(),
                planned: plan.planned_url.clone(),
            });
        }

        if let Some(status) = plan.last_observed_status
            && let Some(reason) = contradiction(status, &plan.recorded.range_support)
        {
            triggers.push(ReprobeTrigger::StatusContradictsCapabilities { status, reason });
        }

        // A probe timestamp in the future means the clock moved backwards, so the evidence's age
        // is unknowable. Treated as stale: the alternative trusts a clock that has already shown
        // it cannot be trusted, and re-probing costs one ranged GET.
        let elapsed = plan
            .now
            .duration_since(plan.recorded.probed_at)
            .unwrap_or(self.freshness + Duration::from_nanos(1));
        if elapsed > self.freshness {
            triggers.push(ReprobeTrigger::FreshnessWindowElapsed {
                elapsed,
                window: self.freshness,
            });
        }

        triggers
    }
}

/// Which recorded claim a status contradicts, if any.
///
/// Only statuses that say something about *capabilities* count. The transient ones a retry
/// actually meets — `408`, `425`, `429`, and the `5xx` family — say the server is unwell, not
/// that its capabilities were recorded wrongly, and treating them as evidence would re-probe on
/// every back-off.
fn contradiction(status: u16, range_support: &RangeSupport) -> Option<&'static str> {
    match status {
        // Only a contradiction when ranges were actually proven. When they were not, a
        // whole-representation answer is exactly what was expected.
        200 if range_support.is_proven() => {
            Some("range support was proven but the server answered a ranged request with 200")
        }
        // The range does not exist in the representation the server now holds, which contradicts
        // the length recorded alongside the range proof.
        416 => Some("the server rejected a range the recorded length says exists"),
        // The URL or credential that produced the evidence no longer applies. §7 routes these to
        // the refresh flow; what matters here is that the cached capabilities came with it.
        401 | 403 | 410 => {
            Some("the URL or credential that produced the recorded evidence no longer applies")
        }
        _ => None,
    }
}
