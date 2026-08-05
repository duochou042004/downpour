//! Verification before naming: docs/04 §6, and the whole of **I-4**.
//!
//! A partial file wearing the final name is indistinguishable from a good one — to the user, to
//! every other program on the system, and to any later integrity check that trusts the name.
//! That is what makes it worse than an obvious failure, and it is why the rename is the *last*
//! step here rather than a step among others.
//!
//! The order is fixed and each check answers a different question. Length asks whether the file
//! on disk is the size the representation claimed. Coverage asks whether the journal proves
//! every byte inside it durable — a file can be exactly the right length and still be a hole
//! surrounded by data, which no length check can see. The digest asks whether those bytes are
//! the bytes the *server* meant to send, which neither of the first two can know. Only when all
//! three agree does the part file get a name.
//!
//! On failure nothing is deleted. The part file and the journal are evidence: docs/04 §6 says so
//! for a digest mismatch, and the same reasoning covers the rest — the user may want to retry
//! rather than start over, and discarding the only copy of a suspect file makes the fault
//! unreproducible.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use downpour_intervals::{IntervalMap, IntervalState};
use downpour_types::{ContentDigest, DigestAlgorithm};
use sha2::{Digest as _, Sha256, Sha512};
use thiserror::Error;

use crate::journal::{FramedRecord, JournalRecord};
use crate::writer::{DurableJournal, WriterError};

/// Streaming buffer for the verification pass. Large enough that a gigabyte is not dominated by
/// per-read overhead, small enough that verification is not a memory-pressure experiment.
const VERIFY_BUFFER: usize = 64 * 1024;

/// The largest base64 payload a digest header may carry before it is refused.
///
/// A SHA-512 digest is 64 bytes, which is 88 base64 characters. Anything beyond a small margin
/// on that is not a digest, and is rejected before it is decoded rather than after.
const MAX_DIGEST_BASE64: usize = 128;

/// Why a completed download could not be verified, and therefore was not named.
///
/// Each variant is a distinct question that failed, because "verification failed" is not
/// actionable in a bug report and the three checks have genuinely different causes.
#[derive(Debug, Error)]
pub enum CompletionError {
    /// The file on disk is not the length the representation claimed.
    #[error("part file holds {actual} bytes but the representation is {expected}")]
    LengthMismatch {
        /// The representation length.
        expected: u64,
        /// What the file actually holds.
        actual: u64,
    },
    /// The journal does not prove every byte durable.
    ///
    /// Distinct from a length mismatch on purpose: a file can be exactly the right size and
    /// still contain a range nothing ever wrote, which is the hole a byte counter cannot see.
    #[error("coverage is incomplete: {covered} of {total} bytes are durable, first gap at {gap}")]
    IncompleteCoverage {
        /// Bytes the interval map proves complete.
        covered: u64,
        /// The representation length.
        total: u64,
        /// Start of the first range that is not complete.
        gap: u64,
    },
    /// The server supplied a digest and the bytes do not match it.
    #[error("server digest does not match the delivered bytes ({algorithm:?})")]
    DigestMismatch {
        /// Which algorithm was compared.
        algorithm: DigestAlgorithm,
    },
    /// Reading the part file, appending the seal, or renaming failed.
    #[error("could not {operation}: {source}")]
    Io {
        /// The operation whose result could not be ignored.
        operation: &'static str,
        /// The underlying failure.
        #[source]
        source: io::Error,
    },
    /// The `Sealed` record could not be appended.
    #[error("could not seal the recovery journal: {0}")]
    Seal(#[from] WriterError),
}

/// What verification concluded about a download that claims to be complete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sealed {
    final_path: PathBuf,
    final_blake3: [u8; 32],
    digest_checked: DigestOutcome,
}

impl Sealed {
    /// Where the verified file now lives.
    #[must_use]
    pub fn final_path(&self) -> &Path {
        &self.final_path
    }

    /// The BLAKE3 of the complete file, as recorded in the journal's `Sealed` record.
    #[must_use]
    pub const fn final_blake3(&self) -> &[u8; 32] {
        &self.final_blake3
    }

    /// What happened to the server's digest evidence.
    #[must_use]
    pub const fn digest_checked(&self) -> DigestOutcome {
        self.digest_checked
    }
}

/// What became of the server's RFC 9530 evidence.
///
/// Reported rather than collapsed into a boolean, because "the server offered nothing" and "the
/// server offered something we could not read" are different facts about the world, and a
/// scorecard that counts verified downloads must not count the second as the first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DigestOutcome {
    /// No digest was offered. Most servers; not a fault.
    NotOffered,
    /// A digest was offered, decoded, and matched the bytes.
    Verified(DigestAlgorithm),
    /// A digest was offered but could not be interpreted, so it proved nothing either way.
    ///
    /// Deliberately not a failure: a header we cannot read is no worse than a header that was
    /// never sent, and failing the download on it would punish the user for a server's bad
    /// syntax. It is surfaced so it can be logged rather than silently equated with success.
    Unusable {
        /// Why the evidence could not be used.
        reason: &'static str,
    },
}

/// Verify a completed download and, only then, give it its final name.
///
/// Runs docs/04 §6 steps 2 through 9. Step 10 — deleting the journal — belongs to the caller,
/// which owns the journal handle; this function appends the `Sealed` record that makes deletion
/// safe.
///
/// # Errors
///
/// Any check that fails. The part file and journal are left exactly as they were found.
pub fn verify_and_rename(
    part_path: &Path,
    final_path: &Path,
    total_length: u64,
    intervals: &IntervalMap,
    digest: Option<&ContentDigest>,
    journal: &mut impl DurableJournal,
    next_sequence: u64,
) -> Result<Sealed, CompletionError> {
    // Step 3. Length on disk, before anything reads the contents: a file of the wrong size
    // cannot be the representation, whatever its bytes say.
    let actual = fs::metadata(part_path)
        .map_err(|source| CompletionError::Io {
            operation: "measure the part file before verification",
            source,
        })?
        .len();
    if actual != total_length {
        return Err(CompletionError::LengthMismatch {
            expected: total_length,
            actual,
        });
    }

    // Step 4. Gap-free coverage. The length check above cannot see a hole; this can, because the
    // interval map only reaches Complete past the writer's commit point (I-1).
    check_coverage(intervals, total_length)?;

    // Steps 5 and 7 in one pass over the file. Both digests are computed together so a 1 GB
    // download is streamed once rather than twice.
    let (final_blake3, server_digest) = hash_file(part_path, digest)?;

    let digest_checked = match (digest, server_digest) {
        (None, _) => DigestOutcome::NotOffered,
        (Some(offered), Some(computed)) => {
            // A digest we cannot decode proves nothing either way, so the download stands and
            // the unusable evidence is reported rather than equated with success.
            let Some(expected) = decode_digest(offered) else {
                return seal_and_rename(
                    part_path,
                    final_path,
                    final_blake3,
                    DigestOutcome::Unusable {
                        reason: "digest payload is not valid base64 of the algorithm's length",
                    },
                    journal,
                    next_sequence,
                );
            };
            if !constant_time_eq(&expected, &computed) {
                return Err(CompletionError::DigestMismatch {
                    algorithm: offered.algorithm,
                });
            }
            DigestOutcome::Verified(offered.algorithm)
        }
        (Some(_), None) => DigestOutcome::Unusable {
            reason: "digest algorithm is not one this build can compute",
        },
    };

    seal_and_rename(
        part_path,
        final_path,
        final_blake3,
        digest_checked,
        journal,
        next_sequence,
    )
}

/// Steps 7 through 9: seal, then name. Never the other way round (I-4).
fn seal_and_rename(
    part_path: &Path,
    final_path: &Path,
    final_blake3: [u8; 32],
    digest_checked: DigestOutcome,
    journal: &mut impl DurableJournal,
    next_sequence: u64,
) -> Result<Sealed, CompletionError> {
    // The seal is durable before the rename, so a crash between the two leaves a journal that
    // says "this was verified" next to a part file that still has its working name — which
    // recovery can act on. A rename before the seal would leave the opposite: a final name whose
    // verification nobody can confirm.
    journal.append(&FramedRecord::new(
        next_sequence,
        JournalRecord::Sealed { final_blake3 },
    ))?;
    journal.sync_data()?;

    fs::rename(part_path, final_path).map_err(|source| CompletionError::Io {
        operation: "rename the verified part file to its final name",
        source,
    })?;

    Ok(Sealed {
        final_path: final_path.to_path_buf(),
        final_blake3,
        digest_checked,
    })
}

/// The interval map must cover `[0, total)` with nothing but `Complete`.
fn check_coverage(intervals: &IntervalMap, total: u64) -> Result<(), CompletionError> {
    let mut covered = 0_u64;
    let mut gap = None;
    for interval in intervals.intervals() {
        if *interval.state() == IntervalState::Complete {
            covered = covered.saturating_add(interval.len());
        } else if gap.is_none() {
            gap = Some(interval.start());
        }
    }
    if let Some(gap) = gap {
        return Err(CompletionError::IncompleteCoverage {
            covered,
            total,
            gap,
        });
    }
    if covered != total {
        return Err(CompletionError::IncompleteCoverage {
            covered,
            total,
            gap: covered,
        });
    }
    Ok(())
}

/// Stream the file once, computing our BLAKE3 and the server's algorithm together.
fn hash_file(
    path: &Path,
    digest: Option<&ContentDigest>,
) -> Result<([u8; 32], Option<Vec<u8>>), CompletionError> {
    let mut file = File::open(path).map_err(|source| CompletionError::Io {
        operation: "open the part file for verification",
        source,
    })?;
    let mut blake = blake3::Hasher::new();
    let mut sha256 =
        matches!(digest.map(|d| d.algorithm), Some(DigestAlgorithm::Sha256)).then(Sha256::new);
    let mut sha512 =
        matches!(digest.map(|d| d.algorithm), Some(DigestAlgorithm::Sha512)).then(Sha512::new);

    let mut buffer = vec![0_u8; VERIFY_BUFFER];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| CompletionError::Io {
                operation: "read the part file for verification",
                source,
            })?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        blake.update(chunk);
        if let Some(hasher) = sha256.as_mut() {
            hasher.update(chunk);
        }
        if let Some(hasher) = sha512.as_mut() {
            hasher.update(chunk);
        }
    }

    let server = sha256
        .map(|hasher| hasher.finalize().to_vec())
        .or_else(|| sha512.map(|hasher| hasher.finalize().to_vec()));
    Ok((*blake.finalize().as_bytes(), server))
}

/// Decode a digest payload, refusing anything that is not the algorithm's exact length.
///
/// Length-checked before decoding, so a header claiming a megabyte of base64 is refused rather
/// than allocated.
fn decode_digest(digest: &ContentDigest) -> Option<Vec<u8>> {
    if digest.encoded.len() > MAX_DIGEST_BASE64 {
        return None;
    }
    let expected_len = match digest.algorithm {
        DigestAlgorithm::Sha256 => 32,
        DigestAlgorithm::Sha512 => 64,
    };
    let decoded = STANDARD.decode(digest.encoded.as_bytes()).ok()?;
    (decoded.len() == expected_len).then_some(decoded)
}

/// Compare two digests without leaking where they first differ.
///
/// A weak property here — an attacker who can already choose our bytes has easier avenues — but
/// free to obtain and awkward to retrofit, so it is done at the point the comparison is written.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (a, b) in left.iter().zip(right) {
        diff |= a ^ b;
    }
    diff == 0
}
