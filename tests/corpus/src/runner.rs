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
    /// Why this case could not run here, when it could not.
    ///
    /// Neither a pass nor a failure. A precondition some platforms cannot express — an
    /// unwritable directory on Windows — must not be reported as green, because a case counted
    /// as passing on a platform where it never ran is exactly the false confidence the corpus
    /// exists to avoid. Skips are surfaced and counted separately.
    pub skipped: Option<&'static str>,
}

impl CaseReport {
    /// Whether the case passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.failures.is_empty() && self.skipped.is_none()
    }

    /// Whether this case could not run here, and why.
    ///
    /// Distinct from passing. A case counted as green on a platform where it never ran is the
    /// false confidence the corpus exists to avoid, so a skip is neither a pass nor a failure.
    #[must_use]
    pub const fn skipped(&self) -> Option<&'static str> {
        self.skipped
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
///
/// `scratch` is the *data* directory: everything in it at the end is what the user would be left
/// with, which is what the byte-comparison and file-count expectations are written against.
/// Recovery journals go in a sibling, matching docs/04 §1 — and keeping them out of `scratch` is
/// what stops a journal from being compared against the content generator and reported as
/// corruption.
pub async fn run_case(case: &Case, scratch: &Path) -> CaseReport {
    let mut failures: Vec<String> = Vec::new();

    let fail = |reason: String| CaseReport {
        id: case.id.clone(),
        failures: vec![reason],
        bytes_on_disk: 0,
        silent_corruption: false,
        skipped: None,
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
        // The whole point of cdn-edge-disagrees: the two origins must be able to differ.
        if let Some(override_ranges) = &case.server.content_origin_ranges {
            origin_spec.ranges = crate::case::ranges_to_behaviour(override_ranges);
        }
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
                skipped: None,
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
                skipped: None,
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

    // Local preconditions, before anything is fetched. A `local` case is about what the engine
    // does when the disk is already in a particular state, so the state has to exist first.
    let mut restore_permissions = None;
    if let Some(bytes) = &case.local.existing_target
        && let Err(error) = std::fs::write(scratch.join("content"), bytes)
    {
        return fail(format!("could not create the existing target: {error}"));
    }
    if let Some(bytes) = &case.local.existing_part
        && let Err(error) = std::fs::write(scratch.join("content.dppart"), bytes)
    {
        return fail(format!("could not create the existing part file: {error}"));
    }
    if case.local.existing_target_directory
        && let Err(error) = std::fs::create_dir(scratch.join("content"))
    {
        return fail(format!(
            "could not create the existing target directory: {error}"
        ));
    }
    if case.local.read_only_target_dir {
        if !cfg!(unix) {
            // Not a pass and not a failure: the precondition cannot be expressed here at all.
            // Reported as skipped so the count stays honest on every platform.
            return CaseReport {
                id: case.id.clone(),
                failures: Vec::new(),
                bytes_on_disk: 0,
                silent_corruption: false,
                skipped: Some(
                    "directory permissions cannot express an unwritable target on this platform",
                ),
            };
        }
        match make_read_only(scratch) {
            Ok(Some(original)) => restore_permissions = Some(original),
            // Running as a user who bypasses permission checks. Reporting a pass here would be a
            // green result for a check that never ran, which is worse than an absent test.
            Ok(None) => {
                return CaseReport {
                    id: case.id.clone(),
                    failures: Vec::new(),
                    bytes_on_disk: 0,
                    silent_corruption: false,
                    skipped: Some(
                        "this user bypasses permission checks, so the restriction cannot bite",
                    ),
                };
            }
            Err(error) => return fail(format!("could not make the target read-only: {error}")),
        }
    }

    // Symlink preconditions. Both live outside the scratch directory on purpose: a link pointing
    // *out* of the download folder is the shape the hazard actually has, and it keeps the victim
    // clear of the content scan below, which would otherwise read a file the case placed itself.
    let mut symlink_victim = None;
    if case.local.dangling_symlink_at_target || case.local.symlink_at_part.is_some() {
        if !cfg!(unix) {
            // Creating a symlink on Windows needs privileges or developer mode. Reported as
            // skipped rather than passed: a case that silently did not run is the failure mode
            // docs/09 §7 exists to prevent.
            return CaseReport {
                id: case.id.clone(),
                failures: Vec::new(),
                bytes_on_disk: 0,
                silent_corruption: false,
                skipped: Some("symlinks cannot be created without privileges on this platform"),
            };
        }
        if case.local.dangling_symlink_at_target {
            // Deliberately never created, so the link dangles and every existence check that
            // follows it reports the path as free.
            let destination = beside(scratch, "-symlink-destination-that-does-not-exist");
            if let Err(error) = make_symlink(&destination, &scratch.join("content")) {
                return fail(format!(
                    "could not create the dangling target symlink: {error}"
                ));
            }
        }
        if let Some(bytes) = &case.local.symlink_at_part {
            let victim = beside(scratch, "-symlink-victim");
            if let Err(error) = std::fs::write(&victim, bytes) {
                return fail(format!("could not create the symlink victim: {error}"));
            }
            if let Err(error) = make_symlink(&victim, &scratch.join("content.dppart")) {
                return fail(format!("could not create the part-file symlink: {error}"));
            }
            symlink_victim = Some((victim, bytes.clone()));
        }
    }

    // Journals live beside the data directory, never inside it: see `run_case`.
    let journal_dir = match journal_dir_beside(scratch) {
        Ok(path) => path,
        Err(error) => return fail(format!("could not create the journal directory: {error}")),
    };

    // Fast retry delays: see RetryPolicy::fast_for_tests for why, and note Retry-After is still
    // honoured exactly, so `retry-after-is-honoured` still waits the second the server asked for.
    let outcome = SingleStream::new(backend)
        .with_retry_policy(downpour_http::RetryPolicy::fast_for_tests())
        .download(
            url,
            &downpour_http::StorageLayout::new(scratch, &journal_dir),
        )
        .await;

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

    // ---- expectation: a retry actually happened
    if let Some(expected) = case.expect.min_requests {
        let seen = server.request_count();
        if seen < expected {
            failures.push(format!(
                "expected the server to see at least {expected} requests but it saw {seen}; \
                 no retry occurred, so this case proves nothing about recovery"
            ));
        }
    }

    // ---- expectation: no HEAD was used (I-6; docs/03 §2.1 step 2)
    if case.expect.forbids_head == Some(true) {
        let heads: Vec<String> = server
            .requests()
            .iter()
            .filter(|r| r.method.eq_ignore_ascii_case("HEAD"))
            .map(|r| r.path.clone())
            .collect();
        if !heads.is_empty() {
            failures.push(format!(
                "the engine sent {} HEAD request(s) ({heads:?}); only GET observations are \
                 authoritative (docs/03 §2.1 step 2)",
                heads.len()
            ));
        }
    }

    // Permissions go back before anything inspects the directory, or the runner cannot read it.
    if let Some(original) = restore_permissions {
        let _ = std::fs::set_permissions(scratch, original);
    }

    // ---- unconditional: a file the user already had must come back byte-identical.
    //
    // The strongest statement this category can make. "The download failed" is not enough: it
    // has to have failed without touching something it was never asked to touch.
    if let Some(expected) = &case.local.existing_target {
        match std::fs::read(scratch.join("content")) {
            Ok(found) if found == expected.as_bytes() => {}
            Ok(found) => failures.push(format!(
                "the pre-existing target was modified: {} bytes where {} were placed",
                found.len(),
                expected.len()
            )),
            Err(error) => failures.push(format!("the pre-existing target was destroyed: {error}")),
        }
    }

    // ---- unconditional: a symlink the user had must still be a symlink.
    //
    // Read with `symlink_metadata`, which does not follow, because every call that does follow
    // reports a dangling link as absent — and that confusion is the whole point of the case.
    if case.local.dangling_symlink_at_target {
        match std::fs::symlink_metadata(scratch.join("content")) {
            Ok(metadata) if metadata.is_symlink() => {}
            Ok(_) => failures.push(
                "the dangling symlink at the final name was replaced by a real file: the engine \
                 treated 'the destination does not exist' as 'the path is free', and the rename \
                 destroyed a link the user put there"
                    .to_owned(),
            ),
            Err(error) => failures.push(format!(
                "the symlink at the final name is gone entirely: {error}"
            )),
        }
    }

    // ---- unconditional: nothing may be written through a symlink at the part path.
    //
    // The part file takes every byte of the transfer, so following the link would overwrite the
    // destination with the download. Comparing the bytes is what separates "refused" from "wrote
    // somewhere else and reported nothing".
    if let Some((victim, expected)) = &symlink_victim {
        match std::fs::read(victim) {
            Ok(found) if found == expected.as_bytes() => {}
            Ok(found) => failures.push(format!(
                "the download was written through the symlink at the part path: the destination \
                 holds {} bytes where {} were placed",
                found.len(),
                expected.len()
            )),
            Err(error) => failures.push(format!(
                "the symlink destination was destroyed by the transfer: {error}"
            )),
        }
    }

    // ---- expectation: no resume was attempted (I-3)
    if case.expect.forbids_if_range == Some(true) {
        let conditioned: Vec<String> = server
            .requests()
            .iter()
            .filter(|request| {
                request
                    .headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case("if-range"))
            })
            .map(|request| request.path.clone())
            .collect();
        if !conditioned.is_empty() {
            failures.push(format!(
                "the engine sent {} request(s) carrying If-Range ({conditioned:?}), but this \
                 representation offers no validator usable for one, so no resume may be \
                 attempted at all (I-3)",
                conditioned.len()
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
    //
    // A finished file is compared whole. An unfinished `.dppart` is not: since S2-T8 it is
    // preallocated to the representation length (I-10), so most of it is a hole that no byte has
    // been written to yet, and comparing that against the generator would report the absence of
    // bytes nobody claimed to have. What is claimed is exactly what the recovery journal records,
    // so the durable ranges are what get compared — which also checks the journal and the file
    // agree, and would catch a journal that recorded a range it never wrote.
    let content = case.content();
    let durable = durable_ranges(&journal_dir);
    let mut bytes_on_disk = 0_u64;
    let mut silent_corruption = false;
    for name in &entries {
        let path = scratch.join(name);
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let unfinished = name.ends_with(".dppart");
        let compare: Vec<(u64, u64)> = if unfinished {
            durable.clone().unwrap_or_default()
        } else {
            vec![(0, u64::try_from(bytes.len()).unwrap_or(0))]
        };
        bytes_on_disk = bytes_on_disk.saturating_add(if unfinished {
            compare.iter().map(|(start, end)| end - start).sum()
        } else {
            u64::try_from(bytes.len()).unwrap_or(0)
        });

        // A literal body is not generated content, so there is nothing to compare it against.
        if case.server.body.is_some() {
            continue;
        }
        // Neither is a file the case put there itself. Comparing the user's own pre-existing
        // file against the generator would report its survival as corruption — which is exactly
        // backwards, since surviving untouched is the whole point.
        if case.local.existing_target.is_some() && name == "content" {
            continue;
        }
        for (start, end) in compare {
            let (Ok(from), Ok(to)) = (usize::try_from(start), usize::try_from(end)) else {
                continue;
            };
            let Some(window) = bytes.get(from..to) else {
                failures.push(format!(
                    "{name} claims durable bytes [{start}, {end}) but holds only {}",
                    bytes.len()
                ));
                continue;
            };
            if let Some(mismatch) = content.first_mismatch(start, window) {
                silent_corruption = true;
                failures.push(format!(
                    "SILENT CORRUPTION in {name}: byte {} should be {:#04x} and is {:#04x}",
                    mismatch.offset, mismatch.expected, mismatch.actual
                ));
            }
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
        skipped: None,
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

/// Drop write permission on `dir`, returning the permissions to restore.
///
/// `Ok(None)` means the process would not be stopped by the change — root, or a platform without
/// meaningful directory permissions — and the caller must refuse to report a pass rather than
/// run a check that cannot fail.
fn make_read_only(dir: &Path) -> std::io::Result<Option<std::fs::Permissions>> {
    let original = std::fs::metadata(dir)?.permissions();
    let mut restricted = original.clone();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        restricted.set_mode(0o500);
    }
    #[cfg(not(unix))]
    {
        restricted.set_readonly(true);
    }
    std::fs::set_permissions(dir, restricted)?;

    // Prove the restriction actually bites before letting a case depend on it.
    let probe = dir.join(".downpour-permission-probe");
    match std::fs::write(&probe, b"x") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            let _ = std::fs::set_permissions(dir, original);
            Ok(None)
        }
        Err(_) => Ok(Some(original)),
    }
}

/// The byte ranges the recovery journal records as durable, when a journal was written.
///
/// `None` means no journal exists — either nothing was created, or the representation had no
/// stated length and therefore no journal to bind (ADR-0016). In both cases there is no claim
/// about which bytes are durable, so there is nothing to compare.
fn durable_ranges(journal_dir: &Path) -> Option<Vec<(u64, u64)>> {
    use downpour_storage::journal::{JournalRecord, replay_bytes};

    let mut journals: Vec<PathBuf> = std::fs::read_dir(journal_dir)
        .ok()?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("dpj"))
        .collect();
    journals.sort();
    let bytes = std::fs::read(journals.first()?).ok()?;
    let replayed = replay_bytes(&bytes).ok()?;

    let mut ranges: Vec<(u64, u64)> = replayed
        .records()
        .iter()
        .filter_map(|framed| match framed.record() {
            JournalRecord::BlockComplete { offset, len, .. } => {
                Some((*offset, offset + u64::from(*len)))
            }
            _ => None,
        })
        .collect();
    ranges.sort_unstable();
    Some(ranges)
}

/// The journal directory for a case whose data directory is `scratch`.
///
/// A sibling rather than a child, so nothing the runner counts or byte-compares can ever be a
/// journal.
/// A path beside `scratch` rather than inside it, sharing its uniqueness.
///
/// Used for symlink destinations, which must stay clear of the target directory so the content
/// scan never reads a file the case placed itself.
fn beside(scratch: &Path, suffix: &str) -> PathBuf {
    let mut name = scratch.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Create a symlink at `link` pointing at `destination`.
///
/// Only ever reached on Unix — the caller reports the case as skipped elsewhere — but it has to
/// compile everywhere, so the non-Unix arm is an honest error rather than a silent success.
#[cfg(unix)]
fn make_symlink(destination: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(destination, link)
}

#[cfg(not(unix))]
fn make_symlink(_destination: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "symlinks are not created on this platform",
    ))
}

fn journal_dir_beside(scratch: &Path) -> std::io::Result<PathBuf> {
    let mut name = scratch.as_os_str().to_os_string();
    name.push("-journals");
    let path = PathBuf::from(name);
    std::fs::create_dir_all(&path)?;
    Ok(path)
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
        if let Ok(journals) = journal_dir_beside(&self.path) {
            let _ = std::fs::remove_dir_all(journals);
        }
    }
}
