//! The retention policy of `docs/04-storage-and-recovery-spec.md` §8.
//!
//! This module owns one sentence, and it is the sentence under the table rather than the table
//! itself:
//!
//! > Downpour does not delete user data on its own initiative.
//!
//! So this is not a garbage collector, and it is deliberately bad at being one. It removes
//! exactly one kind of file — a recovery journal that can no longer protect anything — and it
//! never removes a part file, whatever the state of the world around it. An orphaned `.dppart`
//! is the user's bytes, possibly the only copy of an hour's download, and §8's last row surfaces
//! it rather than deleting it.
//!
//! The other half is B-30. A failed download keeps its journal deliberately, because it is the
//! evidence a resume is built from; applying that reasoning to *every* terminal download left a
//! file behind for each one, forever, which is unbounded growth in a user's home directory. Both
//! halves are one decision. Delete too much and a resumable download is destroyed; delete too
//! little and the daemon leaks a file per download for the life of the installation.
//!
//! Nothing here reads the clock. `now_ms` is supplied by the caller, exactly as
//! [`crate::startup::recover_all`] takes it, so the rules can be tested by their own terms.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use downpour_storage::metadata::{DownloadId, DownloadState, MetadataError, MetadataStore};
use thiserror::Error;

/// How long a journal with no download must sit untouched before it is treated as abandoned.
///
/// The window exists because the store and the journal are written by different steps: a download
/// being created has a journal on disk before its row is committed. A sweep with no floor races
/// every `download.add` and can delete the journal of a transfer that is about to start.
///
/// A day, not a minute. Nothing is gained by reclaiming a few kilobytes promptly, and the cost of
/// being wrong is a destroyed transfer.
const DEFAULT_MINIMUM_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// How aggressive a sweep is permitted to be.
#[derive(Debug, Clone, Copy)]
pub struct RetentionPolicy {
    minimum_age: Duration,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            minimum_age: DEFAULT_MINIMUM_AGE,
        }
    }
}

impl RetentionPolicy {
    /// A policy with a different floor for an orphaned journal. Tests and `dp repair` want this;
    /// the daemon uses [`Self::default`].
    #[must_use]
    pub const fn with_minimum_age(minimum_age: Duration) -> Self {
        Self { minimum_age }
    }

    /// How long an orphaned journal must be untouched before the sweep will remove it.
    #[must_use]
    pub const fn minimum_age(&self) -> Duration {
        self.minimum_age
    }
}

/// What one sweep did, and what it deliberately did not do.
#[derive(Debug, Default)]
pub struct RetentionSummary {
    removed: Vec<PathBuf>,
    kept: usize,
    unreadable: usize,
}

impl RetentionSummary {
    /// How many journals were removed.
    #[must_use]
    pub fn removed(&self) -> usize {
        self.removed.len()
    }

    /// The journals that were removed, for the log and for a test that wants to name one.
    #[must_use]
    pub fn removed_paths(&self) -> &[PathBuf] {
        &self.removed
    }

    /// How many journals were examined and kept.
    #[must_use]
    pub const fn kept(&self) -> usize {
        self.kept
    }

    /// Entries the sweep could not classify and therefore left alone.
    #[must_use]
    pub const fn unreadable(&self) -> usize {
        self.unreadable
    }
}

/// Why a sweep could not run at all.
///
/// One journal that cannot be classified is kept and counted, never raised: a directory the
/// daemon cannot fully understand is not a reason to refuse to start.
#[derive(Debug, Error)]
pub enum RetentionError {
    /// The metadata store could not be read, so nothing can be classified.
    #[error("the retention sweep could not use the metadata store: {0}")]
    Metadata(#[from] MetadataError),
    /// The journal directory could not be listed.
    #[error("the retention sweep could not read {path}: {source}")]
    Directory {
        /// The directory that could not be listed.
        path: PathBuf,
        /// Why not.
        #[source]
        source: std::io::Error,
    },
}

/// Remove the journals that can no longer protect anything, and nothing else.
///
/// A journal is removed when either:
///
/// - its download is `Completed` — §8's first row, the file has been verified and renamed and the
///   journal has no further use; or
/// - no download in the store owns it **and** it has been untouched for longer than the policy's
///   floor, so it cannot belong to a transfer being set up right now.
///
/// Everything else is kept, including every non-terminal state, `Failed` (§8's second row keeps
/// the journal precisely because a resume needs it), a young orphan, a file whose name is not a
/// download id, and every part file anywhere.
///
/// # Errors
///
/// Only if the store cannot be read or the directory cannot be listed.
pub fn sweep_journals(
    store: &MetadataStore,
    journal_dir: &Path,
    policy: RetentionPolicy,
    now_ms: u64,
) -> Result<RetentionSummary, RetentionError> {
    let owners = store.load_downloads()?;
    let mut summary = RetentionSummary::default();

    let entries = fs::read_dir(journal_dir).map_err(|source| RetentionError::Directory {
        path: journal_dir.to_path_buf(),
        source,
    })?;

    for entry in entries {
        let Ok(entry) = entry else {
            // A directory entry that cannot be read is one the sweep knows nothing about, and
            // "delete what you cannot identify" is how a cleanup routine loses data.
            summary.unreadable += 1;
            continue;
        };
        let path = entry.path();
        let Some(id) = journal_id_of(&path) else {
            // Not a journal, or a `.dpj` whose name is not a download id. The daemon owns this
            // directory but not everything that may end up in it.
            summary.unreadable += 1;
            continue;
        };

        let owner = owners.iter().find(|download| download.id == id);
        let removable = match owner.map(|download| download.state) {
            // §8 row 1. Verified and renamed; the journal's work is done.
            Some(DownloadState::Completed) => true,
            // §8 row 2, and every unfinished state. A journal that some download can still
            // resume from is not ours to remove.
            Some(_) => false,
            // No row owns it. Old enough that it cannot be a transfer being set up right now.
            None => is_older_than(&path, policy.minimum_age, now_ms),
        };

        if !removable {
            summary.kept += 1;
            continue;
        }

        match fs::remove_file(&path) {
            Ok(()) => {
                tracing::info!(path = %path.display(), "removed a journal that protects nothing");
                summary.removed.push(path);
            }
            Err(error) => {
                // Failing to remove a file is untidy, never dangerous: the state on disk is the
                // state that was already there. It must not stop the sweep or the daemon.
                tracing::warn!(path = %path.display(), %error, "a journal could not be removed");
                summary.kept += 1;
            }
        }
    }

    tracing::info!(
        removed = summary.removed(),
        kept = summary.kept(),
        unreadable = summary.unreadable(),
        "retention sweep complete; no part file was touched"
    );
    Ok(summary)
}

/// The [`DownloadId`] a journal's filename encodes, or `None` if this is not a journal of ours.
///
/// `<32 hex characters>.dpj`, the naming in docs/04 §1. Anything else — an editor's backup, a
/// half-copied file, a `.dpj` from somewhere else — is not something the sweep can attribute, and
/// what it cannot attribute it does not touch.
fn journal_id_of(path: &Path) -> Option<DownloadId> {
    if path.extension()?.to_str()? != "dpj" {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    if stem.len() != 32 {
        return None;
    }
    let mut bytes = [0_u8; 16];
    for (index, slot) in bytes.iter_mut().enumerate() {
        let pair = stem.get(index * 2..index * 2 + 2)?;
        *slot = u8::from_str_radix(pair, 16).ok()?;
    }
    DownloadId::try_from_bytes(bytes).ok()
}

/// Whether `path` was last modified longer than `minimum_age` before `now_ms`.
///
/// Fails closed. A file whose age cannot be established is treated as too young to remove,
/// because the only cost of keeping it is a few kilobytes and the cost of the other answer is a
/// transfer nobody can resume.
fn is_older_than(path: &Path, minimum_age: Duration, now_ms: u64) -> bool {
    let Ok(modified) = fs::metadata(path).and_then(|metadata| metadata.modified()) else {
        return false;
    };
    let now = SystemTime::UNIX_EPOCH + Duration::from_millis(now_ms);
    now.duration_since(modified)
        .is_ok_and(|age| age > minimum_age)
}

/// Apply §8's terminal rows to a download that has just reached `state`.
///
/// The gate lives here rather than at the call site so that the rule can be tested by its own
/// terms. It is the most dangerous decision in this module: `Completed` releases the journal,
/// and every other state — `Failed` above all — keeps it, because a failed download's journal is
/// the evidence its resume is built from and the download a user most wants to retry is exactly
/// the one that just failed.
pub fn retire_journal_for(journal_dir: &Path, id: DownloadId, state: DownloadState) {
    if state == DownloadState::Completed {
        retire_completed_journal(journal_dir, id);
    }
}

/// Remove the journal of a download that has just been verified and renamed.
///
/// §8's first row, applied when it applies rather than at the next restart. The startup sweep
/// would catch this eventually, but "eventually" is however long the daemon runs, and a file per
/// completed download until reboot is the growth B-30 is about.
///
/// Gated by the caller on `Completed` and nothing else. A `Failed` download keeps its journal,
/// which is the whole of §8's second row. Failure to remove is logged and ignored: the disk holds
/// what it already held, and a finished download must not be reported as broken over a stale
/// file the next sweep will take.
pub fn retire_completed_journal(journal_dir: &Path, id: DownloadId) {
    let mut name = String::with_capacity(36);
    for byte in id.as_bytes() {
        name.push_str(&format!("{byte:02x}"));
    }
    name.push_str(".dpj");
    let path = journal_dir.join(name);
    match fs::remove_file(&path) {
        Ok(()) => tracing::debug!(path = %path.display(), "retired a completed download's journal"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "a completed journal could not be removed");
        }
    }
}
