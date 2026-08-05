//! S2-T11 — verification before naming. This file is I-4's proof.
//!
//! The task's `proof` field named `tests/sim/tests/completion.rs`, but `tests/sim/` is S2-T12's
//! deliverable and T12 is blocked *by* T11 — a cycle in the decomposition. The property itself
//! needs no simulation harness: "no incomplete map is ever renamed" is a statement about the
//! completion sequence, and it is checked here directly. The *crash-injected* version of it,
//! where the process dies between the seal and the rename, is genuinely T12's and stays there.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use downpour_intervals::{IntervalMap, WorkerId};
use downpour_storage::completion::{CompletionError, DigestOutcome, verify_and_rename};
use downpour_storage::journal::{FileHeader, JournalRecord, replay_bytes};
use downpour_storage::writer::JournalFile;
use downpour_types::{ContentDigest, DigestAlgorithm};
use sha2::{Digest as _, Sha256};

static NEXT: AtomicU64 = AtomicU64::new(0);

const TOTAL: u64 = 4096;

struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Self {
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "downpour-completion-{tag}-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn content() -> Vec<u8> {
    (0..TOTAL).map(|i| u8::try_from(i % 251).unwrap()).collect()
}

/// A part file holding `bytes`, plus a journal, plus a map covering `complete_to`.
fn artifacts(
    dir: &Dir,
    bytes: &[u8],
    complete_to: u64,
) -> (PathBuf, PathBuf, JournalFile, IntervalMap) {
    let part_path = dir.path().join("payload.bin.dppart");
    fs::write(&part_path, bytes).unwrap();
    let journal_path = dir.path().join("transfer.dpj");
    let journal =
        JournalFile::create(&journal_path, FileHeader::new([7; 16], TOTAL, 0, [9; 32])).unwrap();

    let mut intervals = IntervalMap::new(TOTAL);
    let worker = WorkerId::new(1);
    if complete_to > 0 {
        intervals.grant(0..complete_to, worker).unwrap();
        intervals.complete(0..complete_to, worker).unwrap();
    }
    (part_path, journal_path, journal, intervals)
}

fn sha256_of(bytes: &[u8]) -> ContentDigest {
    ContentDigest {
        algorithm: DigestAlgorithm::Sha256,
        encoded: STANDARD.encode(Sha256::digest(bytes)),
    }
}

/// The named proof: an incomplete map never produces a final name.
///
/// The file is exactly the right length and its bytes are exactly right. Only the journal's
/// coverage is short — one range nothing ever proved durable. A length check cannot see that,
/// and a byte counter would call this download finished; the resulting file would be the
/// correct size with a hole in it, which is the failure I-4 exists to catch before the user
/// ever sees a final name.
#[test]
fn no_incomplete_map_is_ever_renamed() {
    let dir = Dir::new("incomplete-map");
    let bytes = content();
    let (part, journal_path, mut journal, intervals) = artifacts(&dir, &bytes, TOTAL - 512);
    let final_path = dir.path().join("payload.bin");

    let error = verify_and_rename(&part, &final_path, TOTAL, &intervals, None, &mut journal, 0)
        .expect_err("an incomplete map must not be renamed");

    assert!(
        matches!(
            error,
            CompletionError::IncompleteCoverage {
                total: TOTAL,
                gap,
                ..
            } if gap == TOTAL - 512
        ),
        "got {error:?}"
    );
    assert!(!final_path.exists(), "the final name must not exist (I-4)");
    assert!(part.exists(), "the part file is kept as evidence");
    assert!(
        replay_bytes(&fs::read(&journal_path).unwrap())
            .unwrap()
            .records()
            .is_empty(),
        "nothing may be sealed when verification did not pass"
    );
}

/// A full map and the right length still do not name a file whose bytes the server disowns.
#[test]
fn a_server_digest_mismatch_keeps_the_part_file_and_the_journal() {
    let dir = Dir::new("digest-mismatch");
    let bytes = content();
    let (part, journal_path, mut journal, intervals) = artifacts(&dir, &bytes, TOTAL);
    let final_path = dir.path().join("payload.bin");

    // The digest of a *different* representation: everything local agrees, and only the server's
    // evidence disagrees. This is the case no amount of local checking could catch.
    let mut other = bytes.clone();
    other[0] ^= 0xff;

    let error = verify_and_rename(
        &part,
        &final_path,
        TOTAL,
        &intervals,
        Some(&sha256_of(&other)),
        &mut journal,
        0,
    )
    .expect_err("a digest mismatch must not be renamed");

    assert!(matches!(
        error,
        CompletionError::DigestMismatch {
            algorithm: DigestAlgorithm::Sha256
        }
    ));
    assert!(!final_path.exists());
    assert_eq!(
        fs::read(&part).unwrap(),
        bytes,
        "the part file is evidence and must survive untouched (docs/04 §6)"
    );
    assert!(
        replay_bytes(&fs::read(&journal_path).unwrap())
            .unwrap()
            .records()
            .is_empty(),
        "the journal is evidence too, and nothing may be sealed"
    );
}

/// A matching digest seals first, then renames — and the seal records the file's BLAKE3.
#[test]
fn a_verified_download_is_sealed_before_it_is_named() {
    let dir = Dir::new("verified");
    let bytes = content();
    let (part, journal_path, mut journal, intervals) = artifacts(&dir, &bytes, TOTAL);
    let final_path = dir.path().join("payload.bin");

    let sealed = verify_and_rename(
        &part,
        &final_path,
        TOTAL,
        &intervals,
        Some(&sha256_of(&bytes)),
        &mut journal,
        0,
    )
    .expect("everything agrees");

    assert_eq!(
        sealed.digest_checked(),
        DigestOutcome::Verified(DigestAlgorithm::Sha256)
    );
    assert_eq!(sealed.final_path(), final_path);
    assert!(!part.exists(), "the part file became the final file");
    assert_eq!(fs::read(&final_path).unwrap(), bytes);

    let replayed = replay_bytes(&fs::read(&journal_path).unwrap()).unwrap();
    assert!(
        matches!(
            replayed.records().first().map(FramedRecordExt::record_of),
            Some(JournalRecord::Sealed { final_blake3 }) if final_blake3 == blake3::hash(&bytes).as_bytes()
        ),
        "the seal must carry the digest of the bytes that were verified"
    );
    assert_eq!(sealed.final_blake3(), blake3::hash(&bytes).as_bytes());
}

/// Length is checked before anything reads the contents.
#[test]
fn a_part_file_of_the_wrong_length_is_refused_before_its_bytes_are_read() {
    let dir = Dir::new("wrong-length");
    let bytes = content();
    let (part, _journal_path, mut journal, intervals) = artifacts(&dir, &bytes[..100], TOTAL);
    let final_path = dir.path().join("payload.bin");

    let error = verify_and_rename(&part, &final_path, TOTAL, &intervals, None, &mut journal, 0)
        .expect_err("a short file is not the representation");

    assert!(matches!(
        error,
        CompletionError::LengthMismatch {
            expected: TOTAL,
            actual: 100
        }
    ));
    assert!(!final_path.exists());
}

/// B-20: an overlong part file is a length mismatch, not something to quietly shorten.
///
/// Recovery deliberately leaves such a file alone because I-10 forbids shortening it. This is
/// where that decision gets consumed: the file is refused, and its bytes are preserved.
#[test]
fn an_overlong_part_file_is_refused_rather_than_shortened() {
    let dir = Dir::new("overlong");
    let mut bytes = content();
    bytes.extend_from_slice(&[0xab; 64]);
    let (part, _journal_path, mut journal, intervals) = artifacts(&dir, &bytes, TOTAL);
    let final_path = dir.path().join("payload.bin");

    let error = verify_and_rename(&part, &final_path, TOTAL, &intervals, None, &mut journal, 0)
        .expect_err("an overlong file is not the representation either");

    assert!(matches!(
        error,
        CompletionError::LengthMismatch {
            expected: TOTAL,
            actual
        } if actual == TOTAL + 64
    ));
    assert_eq!(
        fs::read(&part).unwrap().len(),
        usize::try_from(TOTAL).unwrap() + 64,
        "verification never shortens a file to make it fit (I-10)"
    );
}

/// A digest we cannot read is reported, not treated as either success or failure.
///
/// Failing the download on a server's bad syntax punishes the user for someone else's mistake;
/// silently calling it verified would let `DigestOutcome::Verified` mean two different things.
#[test]
fn an_undecodable_digest_is_reported_as_unusable_and_does_not_fail_the_download() {
    for encoded in ["not base64 at all!!", "YWJj", &"A".repeat(200)] {
        let dir = Dir::new("unusable-digest");
        let bytes = content();
        let (part, _journal_path, mut journal, intervals) = artifacts(&dir, &bytes, TOTAL);
        let final_path = dir.path().join("payload.bin");

        let sealed = verify_and_rename(
            &part,
            &final_path,
            TOTAL,
            &intervals,
            Some(&ContentDigest {
                algorithm: DigestAlgorithm::Sha256,
                encoded: encoded.to_owned(),
            }),
            &mut journal,
            0,
        )
        .expect("unreadable evidence proves nothing either way");

        assert!(
            matches!(sealed.digest_checked(), DigestOutcome::Unusable { .. }),
            "{encoded:?} should be unusable, got {:?}",
            sealed.digest_checked()
        );
        assert!(final_path.exists());
    }
}

/// No digest at all is the common case and is not a fault.
#[test]
fn an_absent_digest_verifies_on_length_and_coverage_alone() {
    let dir = Dir::new("no-digest");
    let bytes = content();
    let (part, _journal_path, mut journal, intervals) = artifacts(&dir, &bytes, TOTAL);
    let final_path = dir.path().join("payload.bin");

    let sealed = verify_and_rename(&part, &final_path, TOTAL, &intervals, None, &mut journal, 0)
        .expect("length and coverage are enough when nothing else was offered");

    assert_eq!(sealed.digest_checked(), DigestOutcome::NotOffered);
    assert!(final_path.exists());
}

/// Helper so the match above reads cleanly.
trait FramedRecordExt {
    fn record_of(&self) -> &JournalRecord;
}

impl FramedRecordExt for downpour_storage::journal::FramedRecord {
    fn record_of(&self) -> &JournalRecord {
        self.record()
    }
}
