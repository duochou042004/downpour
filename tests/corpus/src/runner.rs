//! The corpus case runner.
//!
//! Enacts a [`Case`], runs the engine against it, and checks the expectations. Two of those checks
//! are **unconditional** and are not keys a case can set (ADR-0010):
//!
//! 1. **Byte-for-byte comparison against the generator.** Whatever file the run left behind — the
//!    finished file or the `.dppart` — is compared to `blake3-ctr-v1`. This is imposed here rather
//!    than written per case because it is the check most worth having and therefore the one most
//!    likely to be omitted under deadline, and because corruption is the failure that is invisible
//!    in casual testing (risk R-2).
//! 2. **`file_renamed`.** Asserted on every case, so I-4 is tested everywhere instead of once.
//!
//! The runner also **fails closed**. A case that will not load is a failure, never a skip: a
//! runner that quietly ignores what it does not understand reports green forever while checking
//! nothing.

use std::path::{Path, PathBuf};

use downpour_http::{H1H2Backend, SingleStream, TransportMode};
use downpour_types::RangeSupport;

use crate::case::{Case, FinalState, ProtocolCase, RangeSupportCase};
use crate::server::PathologyServer;

/// The outcome of running one case.
#[derive(Debug)]
pub struct CaseReport {
    /// Which case this is.
    pub id: String,
    /// Every expectation that was violated. Empty means the case passed.
    pub failures: Vec<String>,
    /// Bytes that ended up on disk, for the metrics.
    pub bytes_on_disk: u64,
    /// Whether a corruption finding was made. Any non-zero count is a release blocker.
    pub silent_corruption: bool,
}

impl CaseReport {
    /// Whether the case passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }

    /// A multi-line report suitable for a test failure message.
    #[must_use]
    pub fn describe(&self) -> String {
        if self.passed() {
            return format!("{}: pass", self.id);
        }
        let mut out = format!(
            "{}: {} expectation(s) violated",
            self.id,
            self.failures.len()
        );
        for failure in &self.failures {
            out.push_str("\n  - ");
            out.push_str(failure);
        }
        out
    }
}

/// Run one case in `scratch`, which must be an empty directory owned by the caller.
pub async fn run_case(case: &Case, scratch: &Path) -> CaseReport {
    let mut failures: Vec<String> = Vec::new();

    let fail = |reason: String| CaseReport {
        id: case.id.clone(),
        failures: vec![reason],
        bytes_on_disk: 0,
        silent_corruption: false,
    };

    // A cross-origin case needs two servers: one server serves one origin, so a chain that leaves
    // its origin cannot be expressed with a single listener. The content origin is started first so
    // its URL can be handed to the redirecting one.
    //
    // `_content_origin` is bound rather than dropped because a `PathologyServer` stops when it is
    // dropped — letting it fall out of scope here would kill the origin the chain points at, and
    // the case would fail with a connection error that looked like an engine bug.
    let mut content_origin = None;
    let mut spec = case.server_spec();
    if case.server.cross_origin {
        let mut origin_spec = spec.clone();
        // The second origin serves the representation and redirects nowhere.
        origin_spec.redirect_chain = Vec::new();
        origin_spec.redirect_loop = false;
        let origin = match PathologyServer::start(origin_spec).await {
            Ok(origin) => origin,
            Err(error) => return fail(format!("the content origin did not start: {error}")),
        };
        spec.redirect_final_target = Some(origin.url("/content"));
        content_origin = Some(origin);
    }

    let server = match PathologyServer::start(spec).await {
        Ok(server) => server,
        Err(error) => return fail(format!("the pathology server did not start: {error}")),
    };
    let _content_origin = content_origin;

    let mode = match case.server.protocol {
        // Pinned rather than negotiated, because the corpus is cleartext: there is no ALPN to
        // negotiate over (backlog B-8), so the case's protocol has to be asserted on the client.
        ProtocolCase::Http11 => TransportMode::Http1Only,
        ProtocolCase::Http2 => TransportMode::Http2PriorKnowledge,
    };

    let backend = match H1H2Backend::new(mode) {
        Ok(backend) => backend,
        Err(error) => {
            return CaseReport {
                id: case.id.clone(),
                failures: vec![format!("the backend did not build: {error}")],
                bytes_on_disk: 0,
                silent_corruption: false,
            };
        }
    };

    let url = match url::Url::parse(&server.entry_url()) {
        Ok(url) => url,
        Err(error) => {
            return CaseReport {
                id: case.id.clone(),
                failures: vec![format!("the server URL did not parse: {error}")],
                bytes_on_disk: 0,
                silent_corruption: false,
            };
        }
    };

    // The probe is run separately from the download so that a case can assert on what the probe
    // concluded (I-6, I-8) as well as on the outcome. It costs one extra request against a local
    // server, which is cheaper than not being able to check the classification at all.
    if case.expect.range_support.is_some()
        || case.expect.total_length.is_some()
        || case.expect.redirect_chain_len.is_some()
        || case.expect.crosses_origin.is_some()
    {
        use downpour_http::{ProbeRequest, TransferProtocol};
        match backend.probe(ProbeRequest::new(url.clone())).await {
            Ok(remote) => {
                if let Some(expected) = case.expect.range_support {
                    let actual = &remote.range_support;
                    let matches = match expected {
                        RangeSupportCase::Proven => actual.is_proven(),
                        RangeSupportCase::Absent => matches!(actual, RangeSupport::Absent),
                    };
                    if !matches {
                        failures.push(format!(
                            "expected range_support {expected:?} but the probe concluded {actual:?}"
                        ));
                    }
                }
                if let Some(expected) = case.expect.total_length
                    && remote.total_length != Some(expected.0)
                {
                    failures.push(format!(
                        "expected total_length {} but the probe established {:?}",
                        expected.0, remote.total_length
                    ));
                }
                if let Some(expected) = case.expect.redirect_chain_len
                    && remote.redirect_chain.len() != expected
                {
                    failures.push(format!(
                        "expected a redirect chain of {expected} URLs but recorded {}: {:?}",
                        remote.redirect_chain.len(),
                        remote.redirect_chain
                    ));
                }
                if let Some(expected) = case.expect.crosses_origin {
                    // Compared on authority, not on the whole URL: a chain that merely changed
                    // path has not left its origin, and it is leaving the origin that makes a
                    // signed URL stop working on resume (I-8).
                    let entry_authority = url.authority().to_owned();
                    let final_authority = remote.final_url.authority().to_owned();
                    let crossed = entry_authority != final_authority;
                    if crossed != expected {
                        failures.push(format!(
                            "expected crosses_origin {expected}, but the chain went from \
                             {entry_authority} to {final_authority}"
                        ));
                    }
                }
            }
            Err(error) => {
                // Only a problem if the case expected the probe to succeed; a case whose whole
                // point is a probe rejection legitimately gets here.
                if case.expect.final_state == FinalState::Completed {
                    failures.push(format!(
                        "the probe failed but the case expects success: {error}"
                    ));
                }
            }
        }
    }

    let outcome = SingleStream::new(backend).download(url, scratch).await;

    // ---- expectation: final state and error kind
    match (&outcome, case.expect.final_state) {
        (Ok(_), FinalState::Completed) => {}
        (Err(error), FinalState::Failed) => {
            if let Some(expected) = &case.expect.error_kind
                && error.kind() != expected
            {
                failures.push(format!(
                    "expected error_kind {expected:?} but got {:?} ({error})",
                    error.kind()
                ));
            }
        }
        (Ok(path), FinalState::Failed) => failures.push(format!(
            "expected the download to fail with {:?} but it succeeded, producing {}",
            case.expect.error_kind,
            path.display()
        )),
        (Err(error), FinalState::Completed) => {
            failures.push(format!(
                "expected the download to succeed but it failed: {error}"
            ));
        }
    }

    // ---- expectation: filename
    if let Some(expected) = &case.expect.filename
        && let Ok(path) = &outcome
    {
        let actual = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if actual != expected {
            failures.push(format!(
                "expected the file to be named {expected:?}, got {actual:?}"
            ));
        }
    }

    // ---- unconditional: I-4. The final name exists if and only if the case says so.
    let entries = directory_entries(scratch);
    let renamed = entries.iter().any(|name| !name.ends_with(".dppart"));
    if renamed != case.expect.file_renamed {
        failures.push(format!(
            "file_renamed is {} but the target directory contains {:?} (I-4)",
            case.expect.file_renamed, entries
        ));
    }

    // ---- unconditional: silent corruption. No case may opt out of this (ADR-0010).
    let content = case.content();
    let mut bytes_on_disk = 0_u64;
    let mut silent_corruption = false;
    for name in &entries {
        let path = scratch.join(name);
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        bytes_on_disk = bytes_on_disk.saturating_add(u64::try_from(bytes.len()).unwrap_or(0));

        // A literal body is not generated content, so there is nothing to compare it against.
        if case.server.body.is_some() {
            continue;
        }
        if let Some(mismatch) = content.first_mismatch(0, &bytes) {
            silent_corruption = true;
            failures.push(format!(
                "SILENT CORRUPTION in {name}: byte {} should be {:#04x} and is {:#04x}",
                mismatch.offset, mismatch.expected, mismatch.actual
            ));
        }
    }

    // ---- a completed download must be complete, not merely renamed
    if case.expect.final_state == FinalState::Completed
        && bytes_on_disk != case.server.content.size.0
    {
        failures.push(format!(
            "completed, but {bytes_on_disk} bytes are on disk where {} were expected",
            case.server.content.size.0
        ));
    }

    CaseReport {
        id: case.id.clone(),
        failures,
        bytes_on_disk,
        silent_corruption,
    }
}

fn directory_entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Every `.yaml` file under `root`, sorted, so a run is deterministic and countable.
///
/// # Errors
///
/// If `root` cannot be walked.
pub fn discover(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "yaml") {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// A scratch directory for one case, removed when dropped.
pub struct CaseScratch {
    path: PathBuf,
}

impl CaseScratch {
    /// Create a fresh directory for `case_id`.
    ///
    /// # Errors
    ///
    /// If the directory cannot be created.
    pub fn new(case_id: &str) -> std::io::Result<Self> {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("downpour-case-{case_id}-{unique}"));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    /// The directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for CaseScratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
