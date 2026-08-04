//! S1-T6 — the deterministic content generator, `blake3-ctr-v1` (ADR-0010).
//!
//! This is the keystone of the whole corpus strategy. Corruption detection is not "the
//! checksum differs" but "byte 4 194 305 should be `0x7A` and is `0x00`, so the range starting
//! at 4 MiB was never written despite being marked complete". That only works if the correct
//! value of any byte is computable from a seed, in constant time, without storing the file.
//!
//! ADR-0010's third reversal trigger says a corruption finding traced back to the generator
//! would be a correctness emergency for every past green result in the corpus. The pinned
//! vector table below is what makes that detectable instead of catastrophic: if the
//! construction ever changes, these tests fail rather than silently rewriting the meaning of
//! every expectation in the corpus.

use downpour_corpus::content::{Content, GENERATOR_V1};

/// The published BLAKE3 test vector for empty input.
///
/// This is the one genuinely independent anchor in this file. Everything else pins *our*
/// construction; this pins the fact that the `blake3` crate still implements BLAKE3. Without
/// it, a silent upstream change would move every vector below in lockstep and look consistent.
#[test]
fn the_blake3_crate_still_implements_blake3() {
    assert_eq!(
        blake3::hash(b"").to_hex().as_str(),
        "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
        "the blake3 crate no longer matches the published test vector for empty input; \
         every generator vector in this file is suspect until this is explained"
    );
}

#[test]
fn the_generator_is_named_so_a_successor_can_coexist() {
    // ADR-0010: a future generator gets a new name. This one is never redefined.
    assert_eq!(GENERATOR_V1, "blake3-ctr-v1");
}

/// Pinned output. Regenerating these numbers to make a failing test pass would defeat the
/// entire point — a change here means the corpus's ground truth moved.
#[test]
fn pinned_vectors_for_seed_42() {
    let content = Content::new(42, 1 << 20);
    let head = content.range(0, 16);
    assert_eq!(
        head,
        expected_bytes(42, 0, 16),
        "the first 16 bytes of seed 42 moved; see ADR-0010 reversal trigger 3"
    );

    // Spot values around BLAKE3's internal 64-byte chunk boundaries. The construction itself
    // has no block arithmetic (ADR-0010), but the XOF underneath does, and an off-by-one there
    // is exactly the kind of bug that would corrupt only some offsets.
    for offset in [0_u64, 1, 31, 32, 33, 63, 64, 1023, 1024, 1_048_575] {
        assert_eq!(
            content.byte_at(offset),
            expected_byte(42, offset),
            "byte at offset {offset} disagrees with the specified construction"
        );
    }
}

/// The construction from ADR-0010, written out independently of the implementation so that a
/// refactor of `Content` cannot quietly change what the corpus means.
fn expected_bytes(seed: u64, offset: u64, len: usize) -> Vec<u8> {
    let mut stream = blake3::Hasher::new()
        .update(&seed.to_le_bytes())
        .finalize_xof();
    stream.set_position(offset);
    let mut out = vec![0_u8; len];
    stream.fill(&mut out);
    out
}

fn expected_byte(seed: u64, offset: u64) -> u8 {
    expected_bytes(seed, offset, 1)[0]
}

#[test]
fn every_byte_matches_the_specified_construction() {
    // Exhaustive over several XOF chunks rather than spot-checked, because a seek that lands
    // one byte out would still look plausible at most offsets.
    let content = Content::new(7, 512);
    for offset in 0..512 {
        assert_eq!(
            content.byte_at(offset),
            expected_byte(7, offset),
            "offset {offset}"
        );
    }
}

#[test]
fn filling_a_buffer_agrees_with_reading_byte_by_byte() {
    // `byte_at` and `range` both delegate to `fill`, so this asserts the seek is correct at
    // unaligned offsets — the case where a mis-seek would still return plausible-looking bytes.
    let content = Content::new(99, 4096);
    for (offset, len) in [
        (0_u64, 1_usize),
        (0, 32),
        (0, 100),
        (1, 1),
        (1, 63),
        (31, 2),
        (31, 34),
        (32, 32),
        (33, 95),
        (1000, 1000),
        (4000, 96),
    ] {
        let mut buffer = vec![0_u8; len];
        content.fill(offset, &mut buffer);
        let one_at_a_time: Vec<u8> = (offset
            ..offset + u64::try_from(len).expect("test length fits"))
            .map(|o| content.byte_at(o))
            .collect();
        assert_eq!(
            buffer, one_at_a_time,
            "fill({offset}, {len}) diverged from byte_at"
        );
    }
}

#[test]
fn two_independent_instances_agree() {
    // Reproducibility across runs is what makes a failing corpus case reproducible forever.
    let a = Content::new(12345, 65536);
    let b = Content::new(12345, 65536);
    assert_eq!(a.range(0, 65536), b.range(0, 65536));
}

#[test]
fn different_seeds_produce_different_content() {
    let a = Content::new(1, 4096);
    let b = Content::new(2, 4096);
    assert_ne!(a.range(0, 4096), b.range(0, 4096));
}

#[test]
fn there_are_no_long_runs_of_zeros() {
    // The corruption we most need to see is a hole: a range marked complete that was never
    // written, which reads back as zeros. If the generator itself produced a long zero run,
    // that hole would be invisible at exactly that offset.
    let content = Content::new(3, 1 << 20);
    let bytes = content.range(0, 1 << 20);
    let mut run = 0_usize;
    let mut longest = 0_usize;
    for byte in bytes {
        if byte == 0 {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    assert!(
        longest < 8,
        "longest zero run was {longest} bytes; a hole could hide in that"
    );
}

#[test]
fn the_content_is_not_compressible() {
    // A `gzip-on-range` bug (I-5) must not be able to hide in compressible content: if the
    // generator's output shrank under compression, a body that arrived gzipped could be a
    // plausible length. Shannon entropy per byte should be very close to 8 bits.
    let content = Content::new(5, 1 << 18);
    let bytes = content.range(0, 1 << 18);
    let mut histogram = [0_u64; 256];
    for byte in &bytes {
        histogram[usize::from(*byte)] += 1;
    }
    let total = f64::from(u32::try_from(bytes.len()).expect("test size fits a u32"));
    let entropy: f64 = histogram
        .iter()
        .filter(|count| **count > 0)
        .map(|count| {
            let p = (*count as f64) / total;
            -p * p.log2()
        })
        .sum();
    assert!(
        entropy > 7.99,
        "entropy was {entropy} bits/byte; the content is compressible"
    );
}

#[test]
fn a_mismatch_is_reported_with_its_absolute_offset() {
    // The diagnostic that makes a corruption report actionable. "The file differs" costs hours;
    // "byte 4096 should be 0x7A and is 0x00" names the range that was skipped.
    let content = Content::new(42, 8192);
    let mut buffer = content.range(4096, 4096 + 256);
    let expected = buffer[100];
    buffer[100] = expected.wrapping_add(1);

    let mismatch = content
        .first_mismatch(4096, &buffer)
        .expect("the byte was corrupted");
    assert_eq!(mismatch.offset, 4096 + 100);
    assert_eq!(mismatch.expected, expected);
    assert_eq!(mismatch.actual, expected.wrapping_add(1));
}

#[test]
fn a_correct_buffer_reports_no_mismatch() {
    let content = Content::new(42, 8192);
    let buffer = content.range(1000, 2000);
    assert_eq!(content.first_mismatch(1000, &buffer), None);
}

#[test]
fn a_hole_of_zeros_is_detected() {
    // The exact scenario I-1 exists to prevent: a range recorded as complete whose bytes were
    // never actually written.
    let content = Content::new(42, 1 << 16);
    let mut file = content.range(0, 1 << 16);
    file[4096..8192].fill(0);

    let mismatch = content
        .first_mismatch(0, &file)
        .expect("a 4 KiB hole must be detected");
    assert_eq!(
        mismatch.offset, 4096,
        "the report should name the start of the hole"
    );
    assert_eq!(mismatch.actual, 0);
}
