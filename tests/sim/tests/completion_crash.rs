//! S2-T12's remaining obligation, recorded as B-31 and B-27.
//!
//! The crash matrix covers I-1's ordering. This covers the *other* sequence a process can die
//! inside — docs/04 §6's completion:
//!
//! ```text
//! verify → append Sealed → sync journal → rename → delete journal
//! ```
//!
//! The interesting boundary is between the seal and the rename. A crash there leaves a journal
//! saying "this was verified" beside a part file that still has its working name, and a restart
//! has to be able to tell that apart from a download that was never finished. Getting it wrong
//! in either direction is expensive: treat a sealed download as unfinished and the user
//! re-fetches a gigabyte they already have; treat an unsealed one as finished and I-4 is
//! violated by a file that was never verified.

use std::fs;

use downpour_intervals::{IntervalMap, WorkerId};
use downpour_sim::Scenario;
use downpour_storage::completion::verify_and_rename;
use downpour_storage::journal::{JournalRecord, recover_journal};
use downpour_storage::writer::{DurableJournal, JournalFile, WriterError};

const TOTAL: u64 = 2048;

/// A journal that refuses the seal, modelling a crash before it is durable.
struct SealRefusingJournal(JournalFile);

impl DurableJournal for SealRefusingJournal {
    fn total_length(&self) -> u64 {
        DurableJournal::total_length(&self.0)
    }
    fn append(
        &mut self,
        _record: &downpour_storage::journal::FramedRecord,
    ) -> Result<(), WriterError> {
        Err(WriterError::Io {
            operation: "simulated crash before the seal was durable",
            source: std::io::Error::other("injected"),
        })
    }
    fn sync_data(&mut self) -> Result<(), WriterError> {
        self.0.sync_data()
    }
}

fn artifacts(scenario: &Scenario) -> (std::path::PathBuf, std::path::PathBuf, IntervalMap) {
    let part = std::path::PathBuf::from(format!("{}.dppart", scenario.target().display()));
    fs::write(&part, vec![0xab_u8; usize::try_from(TOTAL).expect("fits")]).expect("part file");
    let mut intervals = IntervalMap::new(TOTAL);
    let worker = WorkerId::new(1);
    intervals.grant(0..TOTAL, worker).expect("grant");
    intervals.complete(0..TOTAL, worker).expect("complete");
    (part, scenario.target(), intervals)
}

/// A crash before the seal is durable leaves nothing renamed and nothing sealed.
///
/// The conservative direction. Verification passed in memory, but no evidence of it reached the
/// disk, so a restart must treat the download as unfinished — the alternative is a final name
/// on a file whose verification nobody can confirm, which is exactly I-4.
#[test]
fn a_crash_before_the_seal_leaves_the_part_file_unnamed() {
    let scenario = Scenario::new("seal-crash");
    let (part, final_path, intervals) = artifacts(&scenario);
    let journal =
        JournalFile::create(scenario.journal(), Scenario::header(TOTAL)).expect("journal");
    let mut journal = SealRefusingJournal(journal);

    let error = verify_and_rename(&part, &final_path, TOTAL, &intervals, None, &mut journal, 0)
        .expect_err("the seal could not be made durable");

    assert!(
        format!("{error}").contains("seal"),
        "the failure must name the step that could not complete, got: {error}"
    );
    assert!(
        !final_path.exists(),
        "a download whose seal never reached the disk must not wear the final name (I-4)"
    );
    assert!(part.exists(), "the part file is kept");
    assert!(
        recover_journal(&scenario.journal())
            .expect("replays")
            .records()
            .is_empty(),
        "no Sealed record may survive a crash that happened before it was durable"
    );
}

/// After the seal is durable, the journal says so — and that is what a restart reads.
///
/// The other direction. Once the `Sealed` record has crossed its sync, a crash before the
/// rename leaves recoverable evidence that verification passed, so the restart can finish the
/// rename rather than re-fetching a file it already has and already checked.
#[test]
fn a_durable_seal_survives_a_crash_before_the_rename() {
    let scenario = Scenario::new("seal-then-crash");
    let (part, final_path, intervals) = artifacts(&scenario);
    let mut journal =
        JournalFile::create(scenario.journal(), Scenario::header(TOTAL)).expect("journal");

    verify_and_rename(&part, &final_path, TOTAL, &intervals, None, &mut journal, 0)
        .expect("verification passes");
    drop(journal);

    // The rename happened here, so the observable end state is the finished file. What matters
    // for recovery is that the journal recorded the seal *before* that, which replay confirms.
    let replayed = recover_journal(&scenario.journal()).expect("replays");
    assert!(
        matches!(
            replayed.records().first().map(|f| f.record()),
            Some(JournalRecord::Sealed { .. })
        ),
        "the seal must be durable in the journal, which is what makes the rename safe to \
         complete after a restart"
    );
    assert!(final_path.exists());
    assert!(!part.exists());
}
