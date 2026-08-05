//! Reconciling every unclean download when the daemon starts.
//!
//! docs/04 §5 step 2 covers one download; S2-T7 implemented it. This is step 1 and step 3
//! around it — open the store, walk everything that is not already terminal, and emit a summary
//! — plus the rule that gives the whole sequence its shape:
//!
//! > Step (f) is deliberate. After an unclean shutdown, the user may have been mid-something,
//! > the network may have changed, or the machine may be on a metered connection. Auto-resuming
//! > ten downloads on boot is a good way to be uninstalled.
//!
//! So nothing here starts a transfer. Recovery establishes what is true and stops.
//!
//! One download failing to reconcile must not stop the others. A corrupt journal in one
//! download is that download's problem; taking the daemon down with it would turn a recoverable
//! fault into an outage, and the user would lose access to nine healthy transfers because of
//! the tenth.

use std::path::{Path, PathBuf};

use downpour_storage::metadata::{DownloadId, DownloadState, MetadataError, MetadataStore};
use downpour_storage::recovery::{Reconciliation, RecoveryError, reconcile_download};
use thiserror::Error;

/// What happened to one download during startup recovery.
#[derive(Debug)]
pub struct RecoveredDownload {
    id: DownloadId,
    outcome: Result<Reconciliation, RecoveryError>,
}

impl RecoveredDownload {
    /// Which download this was.
    #[must_use]
    pub const fn id(&self) -> DownloadId {
        self.id
    }

    /// The reconciliation, or why it could not be performed.
    pub const fn outcome(&self) -> &Result<Reconciliation, RecoveryError> {
        &self.outcome
    }

    /// Whether this download is usable after recovery.
    #[must_use]
    pub fn is_recovered(&self) -> bool {
        matches!(
            self.outcome.as_ref().map(Reconciliation::state),
            Ok(DownloadState::Paused)
        )
    }
}

/// What one startup pass established, in the shape docs/04 §5 step 3 asks to be emitted.
#[derive(Debug)]
pub struct RecoverySummary {
    downloads: Vec<RecoveredDownload>,
    skipped: usize,
}

impl RecoverySummary {
    /// Every download that was reconciled or attempted, in store order.
    #[must_use]
    pub fn downloads(&self) -> &[RecoveredDownload] {
        &self.downloads
    }

    /// Downloads already in a terminal state, which recovery does not touch.
    #[must_use]
    pub const fn skipped(&self) -> usize {
        self.skipped
    }

    /// How many came back usable and paused.
    #[must_use]
    pub fn recovered(&self) -> usize {
        self.downloads
            .iter()
            .filter(|download| download.is_recovered())
            .count()
    }

    /// How many could not be reconciled and were left for the user to decide about.
    #[must_use]
    pub fn failed(&self) -> usize {
        self.downloads.len() - self.recovered()
    }
}

/// Why startup recovery could not run at all.
///
/// Reserved for failures that make the whole pass impossible. A single download that cannot be
/// reconciled is recorded in the summary, never raised here — one bad journal must not cost the
/// user access to every other transfer.
#[derive(Debug, Error)]
pub enum StartupError {
    /// The metadata store could not be opened or read.
    #[error("startup recovery could not use the metadata store: {0}")]
    Metadata(#[from] MetadataError),
}

/// Reconcile every download that an unclean shutdown may have left inconsistent.
///
/// `journal_dir` is where recovery journals live (docs/04 §1). Downloads already `Completed` or
/// `Failed` are skipped: they have nothing in flight, and reconciling them would be work with no
/// possible effect.
///
/// **Nothing is resumed.** Every recovered download is left `Paused`.
///
/// # Errors
///
/// Only if the store itself cannot be used.
pub fn recover_all(
    store: &mut MetadataStore,
    journal_dir: &Path,
    now_ms: u64,
) -> Result<RecoverySummary, StartupError> {
    let all = store.load_downloads()?;
    let mut downloads = Vec::new();
    let mut skipped = 0_usize;

    for metadata in all {
        if matches!(
            metadata.state,
            DownloadState::Completed | DownloadState::Failed
        ) {
            skipped += 1;
            continue;
        }
        let journal = journal_path_for(journal_dir, metadata.id);
        let outcome = reconcile_download(store, metadata.id, &journal, now_ms);
        if let Err(error) = &outcome {
            // Recorded, not raised. The other downloads still need recovering, and the user
            // needs to be told about this one rather than have the daemon refuse to start.
            tracing::warn!(%error, "a download could not be reconciled at startup");
        }
        downloads.push(RecoveredDownload {
            id: metadata.id,
            outcome,
        });
    }

    let summary = RecoverySummary { downloads, skipped };
    tracing::info!(
        recovered = summary.recovered(),
        failed = summary.failed(),
        skipped = summary.skipped(),
        "startup recovery complete; nothing was resumed"
    );
    Ok(summary)
}

/// `<journal_dir>/<id>.dpj`, as docs/04 §1 lays it out.
fn journal_path_for(journal_dir: &Path, id: DownloadId) -> PathBuf {
    let mut name = String::with_capacity(36);
    for byte in id.as_bytes() {
        name.push_str(&format!("{byte:02x}"));
    }
    name.push_str(".dpj");
    journal_dir.join(name)
}
