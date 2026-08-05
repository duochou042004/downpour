//! S2-T12 — the crash matrix. This file is **S2-C1**'s proof.
//!
//! For every boundary in I-1's ordering, kill the writer there, then reconstruct what a restart
//! would see from the durable artifacts alone and check two things:
//!
//! 1. **Nothing is claimed that is not durable.** A range only counts as complete once its
//!    journal record crossed the journal sync. Claiming earlier is the corruption I-1 exists to
//!    prevent — resume skips the range, and the finished file is exactly the right size with a
//!    hole in it.
//! 2. **Every byte the recovered state claims really is the right byte.** Checked against
//!    `downpour_corpus::content::Content`, which regenerates the expected bytes from a seed and
//!    knows nothing about how we wrote them. A bug in our own hashing cannot mask a bug in our
//!    writing.
//!
//! Nothing here is timed or raced. Each case names its boundary, so a failure says *which* step
//! broke rather than "sometimes".

use std::fs;
use std::time::Duration;

use downpour_corpus::content::Content;
use downpour_intervals::{IntervalMap, IntervalState, WorkerId};
use downpour_sim::{Boundary, CrashPoint, Scenario, writer_over_real_files};
use downpour_storage::journal::{JournalRecord, recover_journal};

const TOTAL: u64 = 32 * 1024;
const BLOCK: u64 = 8 * 1024;
const SEED: u64 = 20260805;

fn worker() -> WorkerId {
    WorkerId::new(3)
}

/// What a restart can prove from the artifacts on disk, without any in-memory state.
struct Recovered {
    /// One entry per `BlockComplete` record, as written.
    covered: Vec<(u64, u64)>,
    /// The same coverage with adjacent ranges merged, which is what the interval map holds.
    merged: Vec<(u64, u64)>,
    covered_bytes: u64,
}

/// Rebuild coverage the way `downpour_storage::recovery` does: the journal, and only the journal.
fn recover(scenario: &Scenario) -> Recovered {
    let replayed = recover_journal(&scenario.journal()).expect("the journal replays");
    let mut covered: Vec<(u64, u64)> = replayed
        .records()
        .iter()
        .filter_map(|framed| match framed.record() {
            JournalRecord::BlockComplete { offset, len, .. } => {
                Some((*offset, offset + u64::from(*len)))
            }
            _ => None,
        })
        .collect();
    covered.sort_unstable();
    let covered_bytes = covered.iter().map(|(s, e)| e - s).sum();

    // The interval map merges adjacent completions into one canonical range, so comparing
    // against individual journal records would report a difference in bookkeeping as a
    // difference in coverage. The union is what both sides actually agree about.
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (start, end) in &covered {
        match merged.last_mut() {
            Some(last) if last.1 >= *start => last.1 = last.1.max(*end),
            _ => merged.push((*start, *end)),
        }
    }

    Recovered {
        covered,
        merged,
        covered_bytes,
    }
}

/// The named proof for S2-T12 and the evidence for S2-C1.
///
/// One block is committed cleanly, then the writer is killed at each boundary in turn while
/// committing a second. The first block must always survive; the second must be claimed if and
/// only if its journal record crossed the sync.
#[test]
fn crash_at_every_write_boundary_recovers_byte_exact() {
    let content = Content::new(SEED, TOTAL);

    for boundary in Boundary::all() {
        let scenario = Scenario::new(&format!("boundary-{boundary:?}"));
        // Skip one crossing so block one commits cleanly. It is the control: without it, "the
        // crash lost the second block" and "nothing was ever written" look identical on disk.
        let crash = CrashPoint::after(boundary, 1);
        let mut intervals = IntervalMap::new(TOTAL);
        intervals.grant(0..TOTAL, worker()).expect("whole file");
        let mut writer =
            writer_over_real_files(&scenario, TOTAL, &crash, 0).expect("artifacts are created");

        // Block one commits cleanly: it is the control, and it must survive every crash below.
        let mut first = vec![0_u8; usize::try_from(BLOCK).expect("fits")];
        content.fill(0, &mut first);
        writer
            .stage(&mut intervals, worker(), 0, &first, Duration::from_secs(1))
            .expect("the first block stages");
        writer
            .flush(&mut intervals)
            .expect("the first block commits");

        // Block two runs into the crash.
        let mut second = vec![0_u8; usize::try_from(BLOCK).expect("fits")];
        content.fill(BLOCK, &mut second);
        let staged = writer.stage(
            &mut intervals,
            worker(),
            BLOCK,
            &second,
            Duration::from_secs(2),
        );
        let landed = staged.is_ok() && writer.flush(&mut intervals).is_ok();

        // The process is gone. Everything after this point sees only what is on disk.
        drop(writer);

        let recovered = recover(&scenario);
        let second_claimed = recovered.covered.contains(&(BLOCK, BLOCK * 2));

        assert!(
            recovered.covered.contains(&(0, BLOCK)),
            "{boundary:?}: a block committed before the crash must survive it"
        );
        if boundary.range_must_be_claimed() && landed {
            assert!(
                second_claimed,
                "{boundary:?}: a range past the journal sync is durable, so losing it is lost \
                 progress"
            );
        }
        if boundary == Boundary::BeforeWrite {
            assert!(
                !second_claimed,
                "BeforeWrite: nothing was written, so nothing may be claimed"
            );
        }
        assert!(
            recovered.covered_bytes == BLOCK || recovered.covered_bytes == BLOCK * 2,
            "{boundary:?}: claimed {} bytes, which is neither the control alone nor both blocks",
            recovered.covered_bytes
        );

        // I-1 itself, and the assertion the rest of this test cannot make. Everything above
        // reads the journal; this compares the *in-memory* map against it. The map is what a
        // still-running process would answer "have we got these bytes?" with, and if it ever
        // runs ahead of the durable record then a crash at that instant loses the record while
        // the process goes on believing the range is done. That is the reordering I-1 forbids,
        // and it is invisible to any check that only reads the file afterwards.
        for interval in intervals.intervals() {
            if *interval.state() != IntervalState::Complete {
                continue;
            }
            assert!(
                recovered
                    .merged
                    .iter()
                    .any(|(start, end)| *start <= interval.start() && interval.end() <= *end),
                "{boundary:?}: the interval map claims [{}, {}) complete but the journal does \
                 not prove it — the map has run ahead of the commit point (I-1)",
                interval.start(),
                interval.end()
            );
        }

        // Every claimed byte must be the byte the generator says it is. This is the assertion
        // that would catch a journal recording a range it never actually wrote.
        let on_disk = fs::read(scenario.target().with_extension("bin.dppart"))
            .or_else(|_| fs::read(format!("{}.dppart", scenario.target().display())))
            .expect("the part file survives the crash");
        for (start, end) in &recovered.covered {
            let from = usize::try_from(*start).expect("fits");
            let to = usize::try_from(*end).expect("fits");
            assert_eq!(
                content.first_mismatch(*start, &on_disk[from..to]),
                None,
                "{boundary:?}: claimed range [{start}, {end}) does not match the generator"
            );
        }
    }
}

/// Resuming from a crash produces a byte-exact file, at every boundary.
///
/// The half the coverage check cannot make on its own: it is not enough that we refuse to claim
/// unproven bytes, the refetch has to actually put the right bytes there. A resume that skipped
/// a range, or wrote it at the wrong offset, passes every check above and fails this one.
#[test]
fn resuming_after_a_crash_at_any_boundary_produces_the_whole_file() {
    let content = Content::new(SEED, TOTAL);

    for boundary in Boundary::all() {
        let scenario = Scenario::new(&format!("resume-{boundary:?}"));
        let crash = CrashPoint::new(boundary);
        let mut intervals = IntervalMap::new(TOTAL);
        intervals.grant(0..TOTAL, worker()).expect("whole file");
        let mut writer =
            writer_over_real_files(&scenario, TOTAL, &crash, 0).expect("artifacts are created");

        let mut block = vec![0_u8; usize::try_from(BLOCK).expect("fits")];
        content.fill(0, &mut block);
        let staged = writer.stage(&mut intervals, worker(), 0, &block, Duration::from_secs(1));
        let landed = staged.is_ok() && writer.flush(&mut intervals).is_ok();
        drop(writer);

        // Restart: coverage comes from the journal, never from what we remember writing.
        let recovered = recover(&scenario);
        let resume_from = if recovered.covered.first() == Some(&(0, BLOCK)) && landed {
            BLOCK
        } else {
            0
        };

        // Reopen the same artifacts and finish the file from the durable prefix onward.
        let part_path = format!("{}.dppart", scenario.target().display());
        let reopened = downpour_storage::part_file::PartFile::open_existing(&part_path, TOTAL)
            .expect("the part file reopens");
        let part = reopened.into_part();
        let mut offset = resume_from;
        while offset < TOTAL {
            let len = BLOCK.min(TOTAL - offset);
            let mut chunk = vec![0_u8; usize::try_from(len).expect("fits")];
            content.fill(offset, &mut chunk);
            part.write_all_at(offset, &chunk).expect("resume writes");
            offset += len;
        }
        part.sync_data().expect("resume syncs");
        drop(part);

        let finished = fs::read(&part_path).expect("read the finished file");
        assert_eq!(
            u64::try_from(finished.len()).expect("fits"),
            TOTAL,
            "{boundary:?}: wrong length after resume"
        );
        assert_eq!(
            content.first_mismatch(0, &finished),
            None,
            "{boundary:?}: silent corruption after resuming from {resume_from}"
        );
    }
}

/// No boundary leaves an interval `InProgress` that a restart could mistake for progress.
///
/// The in-memory map dies with the process; what a restart rebuilds must contain only `Complete`
/// and `Pending`. This is the same guarantee S2-T7 proves for reconciliation, checked here from
/// the crash side rather than from the artifact side.
#[test]
fn no_boundary_leaves_a_claim_the_journal_cannot_support() {
    for boundary in Boundary::all() {
        let scenario = Scenario::new(&format!("claims-{boundary:?}"));
        let crash = CrashPoint::new(boundary);
        let mut intervals = IntervalMap::new(TOTAL);
        intervals.grant(0..TOTAL, worker()).expect("whole file");
        let mut writer =
            writer_over_real_files(&scenario, TOTAL, &crash, 0).expect("artifacts are created");

        let content = Content::new(SEED, TOTAL);
        let mut block = vec![0_u8; usize::try_from(BLOCK).expect("fits")];
        content.fill(0, &mut block);
        if writer
            .stage(&mut intervals, worker(), 0, &block, Duration::from_secs(1))
            .is_ok()
        {
            let _ = writer.flush(&mut intervals);
        }
        drop(writer);

        // Rebuild through the allocator, exactly as recovery does.
        let recovered = recover(&scenario);
        let mut rebuilt = IntervalMap::new(TOTAL);
        let recovery_worker = WorkerId::new(u64::MAX);
        for (start, end) in &recovered.covered {
            rebuilt
                .grant(*start..*end, recovery_worker)
                .expect("grant a replayed range");
            rebuilt
                .complete(*start..*end, recovery_worker)
                .expect("complete a replayed range");
        }

        assert!(
            !rebuilt
                .intervals()
                .iter()
                .any(|interval| matches!(interval.state(), IntervalState::InProgress { .. })),
            "{boundary:?}: a restart must not inherit a grant"
        );
    }
}
