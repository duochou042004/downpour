//! Deterministic simulation of the boundaries a crash can fall on.
//!
//! This crate exists for one claim: **after a crash at any point in I-1's ordering, the bytes we
//! go on to claim are the bytes we actually have.** That ordering is
//!
//! ```text
//! pwrite → data sync → journal append → journal sync → mark Complete
//! ```
//!
//! and the interesting failures live *between* those steps, not inside them. A process killed
//! after the pwrite but before the data sync has bytes in the page cache that may or may not
//! survive; one killed after the journal append but before the journal sync has a record that
//! may or may not survive. In both cases the only safe reading is the conservative one, and the
//! only way to know we take it is to stop at each boundary and look.
//!
//! What makes this a *simulation* rather than an integration test is that nothing here is timed
//! or raced. A boundary is chosen by index, the failure is injected exactly there, and the run
//! is reproducible from that index alone — so a failure names the boundary that broke rather
//! than "sometimes".
//!
//! The oracle is deliberately independent: `downpour_corpus::content::Content` regenerates the
//! expected bytes from a seed, and the comparison is byte-for-byte against that, never against a
//! checksum of our own making (`docs/09-testing-strategy.md` §7 rule 5). A bug in our hashing
//! cannot hide a bug in our writing.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use downpour_storage::journal::FileHeader;
use downpour_storage::part_file::PartFile;
use downpour_storage::writer::{
    DurableData, DurableJournal, DurableWriter, JournalFile, WriterError,
};

/// Every point in I-1's ordering at which a process can die.
///
/// Named after the operation that has *just completed*, so `DataSync` means "the pwrite and the
/// data sync both happened, and nothing after them did". `Complete` is the last boundary and is
/// the only one at which the interval map has moved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Boundary {
    /// Before anything at all: the block was never written.
    BeforeWrite,
    /// After `pwrite`, before the data reached stable storage.
    Write,
    /// After the data sync, before the journal record was appended.
    DataSync,
    /// After the journal append, before the journal reached stable storage.
    JournalAppend,
    /// After the journal sync — the commit point — but before the map moved.
    JournalSync,
    /// After the interval map marked the range complete. The batch fully landed.
    Complete,
}

impl Boundary {
    /// Every boundary, in the order the writer crosses them.
    #[must_use]
    pub const fn all() -> [Self; 6] {
        [
            Self::BeforeWrite,
            Self::Write,
            Self::DataSync,
            Self::JournalAppend,
            Self::JournalSync,
            Self::Complete,
        ]
    }

    /// Whether a range committed to this boundary **must** be claimable after a restart.
    ///
    /// A lower bound, deliberately, not an equality. Past the journal sync the record is durable
    /// and losing it would be lost progress, so it must survive. Before the sync the record may
    /// or may not survive — an append that reached the page cache but not the platter is exactly
    /// the case nobody can predict, and this simulation cannot demonstrate the loss because it
    /// never drops the page cache.
    ///
    /// So the guarantee under test is asymmetric, and that asymmetry is the point: **claiming
    /// less than this is lost progress, claiming a byte that is wrong is corruption.** Only the
    /// second is a correctness failure, which is why every claimed range is byte-checked no
    /// matter which boundary produced it.
    #[must_use]
    pub const fn range_must_be_claimed(self) -> bool {
        matches!(self, Self::JournalSync | Self::Complete)
    }
}

/// A crash injected at a chosen boundary.
///
/// Modelled as an error rather than a `panic!` or a real `abort`, because the writer's own
/// contract is that a failed operation poisons it — so an injected failure exercises the same
/// path a real I/O error would, and the artifacts left on disk are the ones a killed process
/// would leave.
#[derive(Debug)]
pub struct CrashPoint {
    at: Boundary,
    skip: u64,
    crossed: AtomicU64,
}

impl CrashPoint {
    /// Crash the first time `at` is reached.
    #[must_use]
    pub const fn new(at: Boundary) -> Self {
        Self::after(at, 0)
    }

    /// Crash on the crossing after `skip` earlier ones.
    ///
    /// A scenario that needs a cleanly committed block *before* the crash — as a control, so the
    /// test can tell "this block survived" from "nothing was ever written" — skips that block's
    /// crossings rather than trying to distinguish them afterwards.
    #[must_use]
    pub const fn after(at: Boundary, skip: u64) -> Self {
        Self {
            at,
            skip,
            crossed: AtomicU64::new(0),
        }
    }

    /// Whether this boundary should fail now.
    fn should_fail(&self, boundary: Boundary) -> bool {
        boundary == self.at && self.crossed.fetch_add(1, Ordering::SeqCst) == self.skip
    }

    fn injected(boundary: Boundary) -> WriterError {
        WriterError::Io {
            operation: "simulated crash",
            source: io::Error::other(format!("injected crash at {boundary:?}")),
        }
    }
}

/// A part file that can be made to fail at a chosen boundary.
pub struct CrashingData<'a> {
    inner: PartFile,
    crash: &'a CrashPoint,
}

impl<'a> CrashingData<'a> {
    /// Wrap a real part file.
    #[must_use]
    pub const fn new(inner: PartFile, crash: &'a CrashPoint) -> Self {
        Self { inner, crash }
    }
}

impl DurableData for CrashingData<'_> {
    fn total_length(&self) -> u64 {
        PartFile::total_length(&self.inner)
    }

    fn write_all_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), WriterError> {
        if self.crash.should_fail(Boundary::BeforeWrite) {
            return Err(CrashPoint::injected(Boundary::BeforeWrite));
        }
        PartFile::write_all_at(&self.inner, offset, bytes).map_err(WriterError::from)?;
        if self.crash.should_fail(Boundary::Write) {
            return Err(CrashPoint::injected(Boundary::Write));
        }
        Ok(())
    }

    fn sync_data(&mut self) -> Result<(), WriterError> {
        PartFile::sync_data(&self.inner).map_err(WriterError::from)?;
        if self.crash.should_fail(Boundary::DataSync) {
            return Err(CrashPoint::injected(Boundary::DataSync));
        }
        Ok(())
    }
}

/// A journal that can be made to fail at a chosen boundary, and that loses unsynced appends.
///
/// Buffering until `sync_data` is what makes the simulation faithful. A journal that wrote
/// straight through would always contain everything that was appended, so a crash between the
/// append and the sync would be indistinguishable from one after the sync — and the reordering
/// I-1 forbids, where the interval map moves before the record is durable, would be invisible.
/// A real crash loses what never reached the platter; so does this.
pub struct CrashingJournal<'a> {
    inner: JournalFile,
    crash: &'a CrashPoint,
    unsynced: Vec<downpour_storage::journal::FramedRecord>,
}

impl<'a> CrashingJournal<'a> {
    /// Wrap a real journal.
    #[must_use]
    pub const fn new(inner: JournalFile, crash: &'a CrashPoint) -> Self {
        Self {
            inner,
            crash,
            unsynced: Vec::new(),
        }
    }
}

impl DurableJournal for CrashingJournal<'_> {
    fn total_length(&self) -> u64 {
        DurableJournal::total_length(&self.inner)
    }

    fn append(
        &mut self,
        record: &downpour_storage::journal::FramedRecord,
    ) -> Result<(), WriterError> {
        self.unsynced.push(record.clone());
        if self.crash.should_fail(Boundary::JournalAppend) {
            return Err(CrashPoint::injected(Boundary::JournalAppend));
        }
        Ok(())
    }

    fn sync_data(&mut self) -> Result<(), WriterError> {
        // Checked *before* the buffer reaches the file, because that is what the boundary means:
        // the sync was issued and did not complete, so nothing it was carrying is durable. Both
        // this and `JournalAppend` therefore lose the record, which is correct — they are both
        // before I-1's commit point, and the whole claim is that nothing may be marked complete
        // until after it.
        if self.crash.should_fail(Boundary::JournalSync) {
            return Err(CrashPoint::injected(Boundary::JournalSync));
        }
        for record in std::mem::take(&mut self.unsynced) {
            self.inner.append(&record)?;
        }
        self.inner.sync_data()
    }
}

/// A scratch area for one scenario, removed when dropped.
pub struct Scenario {
    root: PathBuf,
}

static NEXT: AtomicU64 = AtomicU64::new(0);

impl Scenario {
    /// A fresh, uniquely named directory.
    ///
    /// # Panics
    ///
    /// If the directory cannot be created. This is test scaffolding, not engine code.
    #[must_use]
    pub fn new(tag: &str) -> Self {
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "downpour-sim-{tag}-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create the scenario directory");
        Self { root }
    }

    /// The scenario directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Where the download's data lives.
    #[must_use]
    pub fn target(&self) -> PathBuf {
        self.root.join("payload.bin")
    }

    /// Where the recovery journal lives.
    #[must_use]
    pub fn journal(&self) -> PathBuf {
        self.root.join("transfer.dpj")
    }

    /// The journal header every scenario in this crate uses.
    #[must_use]
    pub fn header(total_length: u64) -> FileHeader {
        FileHeader::new([0x5a; 16], total_length, 0, [0x6b; 32])
    }
}

impl Drop for Scenario {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Build a writer over real files that fails at `crash`.
///
/// # Errors
///
/// If either artifact cannot be created.
pub fn writer_over_real_files<'a>(
    scenario: &Scenario,
    total_length: u64,
    crash: &'a CrashPoint,
    next_sequence: u64,
) -> Result<DurableWriter<CrashingData<'a>, CrashingJournal<'a>>, WriterError> {
    let part = PartFile::create(scenario.target(), total_length)?;
    let journal = JournalFile::create(scenario.journal(), Scenario::header(total_length))?;
    DurableWriter::try_new(
        CrashingData::new(part, crash),
        CrashingJournal::new(journal, crash),
        next_sequence,
    )
}
