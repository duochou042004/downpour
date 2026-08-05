//! S2-T13 — running out of disk at 97%. This file is **S2-C5**'s proof.
//!
//! docs/04 §2.4 is short and every line of it is a refusal:
//!
//! > Never truncate the part file to free space. The user's other data is not ours to sacrifice,
//! > and truncation destroys the very bytes we would resume from.
//!
//! The temptation is real — the disk is full, we hold a large mostly-empty preallocated file, and
//! shrinking it would let the write through. Doing so trades the user's problem for a worse one:
//! the bytes we would have resumed from are gone, and the download starts over.
//!
//! The other half is quieter and matters as much. When the write fails, blocks already written
//! and staged have *not* yet been journalled, so their progress is invisible to a restart. §2.4
//! step 2 says flush the journal anyway — it is small, and there is almost always room for it.
//! Skipping that is not corruption, it is throwing away work the user already paid for.

use std::fs;
use std::time::Duration;

use downpour_corpus::content::Content;
use downpour_intervals::{IntervalMap, IntervalState, WorkerId};
use downpour_sim::{FullDiskAfter, Scenario, writer_over_a_full_disk};
use downpour_storage::journal::{JournalRecord, recover_journal};
use downpour_storage::writer::WriterError;

const TOTAL: u64 = 32 * 1024;
const BLOCK: u64 = 8 * 1024;
/// 97% of the representation, which is where §2.4's scenario is named for.
const SPACE: u64 = TOTAL * 97 / 100;
const SEED: u64 = 20260806;

fn worker() -> WorkerId {
    WorkerId::new(5)
}

/// The named proof for S2-T13 and the evidence for S2-C5.
#[test]
fn disk_full_at_97pct_preserves_durable_coverage_without_truncation() {
    let content = Content::new(SEED, TOTAL);
    let scenario = Scenario::new("disk-full");
    let disk = FullDiskAfter::new(SPACE);
    let mut intervals = IntervalMap::new(TOTAL);
    intervals.grant(0..TOTAL, worker()).expect("whole file");
    let mut writer =
        writer_over_a_full_disk(&scenario, TOTAL, &disk, 0).expect("artifacts are created");

    // Write until the disk refuses. 97% of 32 KiB is 31 744, so the fourth 8 KiB block is the
    // one that cannot fit — the classic "it died at 97%" shape.
    let mut written = 0_u64;
    let mut refusal = None;
    while written < TOTAL {
        let mut block = vec![0_u8; usize::try_from(BLOCK).expect("fits")];
        content.fill(written, &mut block);
        match writer.stage(
            &mut intervals,
            worker(),
            written,
            &block,
            Duration::from_secs(1),
        ) {
            Ok(_) => written += BLOCK,
            Err(error) => {
                refusal = Some(error);
                break;
            }
        }
    }

    let refusal = refusal.expect("a 97%-full disk must refuse the last block");
    assert!(
        matches!(refusal, WriterError::NoSpace { .. }),
        "ENOSPC must be its own error, not a generic I/O failure: the download pauses and stays \
         resumable rather than failing (I-10). Got {refusal:?}"
    );

    // §2.4 step 2: the journal is small and there is almost always room, so the work already
    // done is recorded rather than discarded.
    let durable = writer.durable_bytes_after_disk_full();
    assert!(
        durable > 0,
        "blocks written before the disk filled must be journalled, not thrown away"
    );

    drop(writer);

    // I-10's refusal, checked on the filesystem rather than inferred: the preallocated extent is
    // exactly as it was. Shrinking it is what would have let the write through.
    assert_eq!(
        fs::metadata(format!("{}.dppart", scenario.target().display()))
            .expect("the part file survives")
            .len(),
        TOTAL,
        "the part file must never be truncated to make room (docs/04 §2.4)"
    );

    // A restart sees exactly the durable prefix — no more, which would claim bytes nobody wrote,
    // and no less, which would refetch bytes already paid for.
    let replayed = recover_journal(&scenario.journal()).expect("the journal replays");
    let covered: u64 = replayed
        .records()
        .iter()
        .filter_map(|framed| match framed.record() {
            JournalRecord::BlockComplete { len, .. } => Some(u64::from(*len)),
            _ => None,
        })
        .sum();
    assert_eq!(covered, durable, "the journal and the writer must agree");
    assert!(
        covered <= SPACE,
        "no range may be claimed beyond what the disk actually accepted"
    );

    // And the bytes that are claimed are the right bytes.
    let on_disk =
        fs::read(format!("{}.dppart", scenario.target().display())).expect("read the part file");
    assert_eq!(
        content.first_mismatch(0, &on_disk[..usize::try_from(covered).expect("fits")]),
        None,
        "the durable prefix must match the generator"
    );
}

/// The download is resumable afterwards: the remaining bytes land once space is freed.
#[test]
fn the_download_resumes_once_space_is_freed() {
    let content = Content::new(SEED, TOTAL);
    let scenario = Scenario::new("disk-freed");
    let disk = FullDiskAfter::new(SPACE);
    let mut intervals = IntervalMap::new(TOTAL);
    intervals.grant(0..TOTAL, worker()).expect("whole file");
    let mut writer =
        writer_over_a_full_disk(&scenario, TOTAL, &disk, 0).expect("artifacts are created");

    let mut written = 0_u64;
    while written < TOTAL {
        let mut block = vec![0_u8; usize::try_from(BLOCK).expect("fits")];
        content.fill(written, &mut block);
        if writer
            .stage(
                &mut intervals,
                worker(),
                written,
                &block,
                Duration::from_secs(1),
            )
            .is_err()
        {
            break;
        }
        written += BLOCK;
    }
    let durable = writer.durable_bytes_after_disk_full();
    drop(writer);

    // The user frees space. Resume from the durable prefix, exactly as recovery would.
    disk.free();
    let part_path = format!("{}.dppart", scenario.target().display());
    let part = downpour_storage::part_file::PartFile::open_existing(&part_path, TOTAL)
        .expect("the part file reopens")
        .into_part();
    let mut offset = durable;
    while offset < TOTAL {
        let len = BLOCK.min(TOTAL - offset);
        let mut chunk = vec![0_u8; usize::try_from(len).expect("fits")];
        content.fill(offset, &mut chunk);
        part.write_all_at(offset, &chunk).expect("resume writes");
        offset += len;
    }
    part.sync_data().expect("resume syncs");
    drop(part);

    let finished = fs::read(&part_path).expect("read");
    assert_eq!(u64::try_from(finished.len()).expect("fits"), TOTAL);
    assert_eq!(
        content.first_mismatch(0, &finished),
        None,
        "a download paused by a full disk must finish byte-exact once space returns"
    );
}

/// No grant survives a disk-full pause, so a restart cannot mistake one for progress.
#[test]
fn a_disk_full_pause_leaves_no_in_progress_claim() {
    let content = Content::new(SEED, TOTAL);
    let scenario = Scenario::new("disk-full-claims");
    let disk = FullDiskAfter::new(SPACE);
    let mut intervals = IntervalMap::new(TOTAL);
    intervals.grant(0..TOTAL, worker()).expect("whole file");
    let mut writer =
        writer_over_a_full_disk(&scenario, TOTAL, &disk, 0).expect("artifacts are created");

    let mut written = 0_u64;
    while written < TOTAL {
        let mut block = vec![0_u8; usize::try_from(BLOCK).expect("fits")];
        content.fill(written, &mut block);
        if writer
            .stage(
                &mut intervals,
                worker(),
                written,
                &block,
                Duration::from_secs(1),
            )
            .is_err()
        {
            break;
        }
        written += BLOCK;
    }
    drop(writer);

    let replayed = recover_journal(&scenario.journal()).expect("replays");
    let mut rebuilt = IntervalMap::new(TOTAL);
    let recovery = WorkerId::new(u64::MAX);
    for framed in replayed.records() {
        if let JournalRecord::BlockComplete { offset, len, .. } = framed.record() {
            let range = *offset..offset + u64::from(*len);
            rebuilt.grant(range.clone(), recovery).expect("grant");
            rebuilt.complete(range, recovery).expect("complete");
        }
    }
    assert!(
        !rebuilt
            .intervals()
            .iter()
            .any(|i| matches!(i.state(), IntervalState::InProgress { .. })),
        "a restart after a disk-full pause must not inherit a grant"
    );
}
