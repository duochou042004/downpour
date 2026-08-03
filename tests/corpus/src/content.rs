//! The deterministic content generator, `blake3-ctr-v1`.
//!
//! This module owns the corpus's **ground truth**. Every corruption finding in the project is
//! stated in terms of the function defined here, which is why ADR-0010 freezes it: a future
//! generator gets a new name and coexists, and this one is never redefined.
//!
//! The construction, exactly as specified in ADR-0010:
//!
//! ```text
//! stream(seed)    = BLAKE3 extendable output of seed.to_le_bytes()   // 8 bytes in
//! byte(seed, off) = stream(seed)[off]                                 // XOF seek, O(1)
//! ```
//!
//! Two lines, with no arithmetic of our own. That is deliberate. The generator is the corpus's
//! oracle, so a bug *here* is the one bug the corpus cannot catch — and the counter-mode
//! alternative, which indexes fixed-size blocks, has an off-by-one failure mode that corrupts
//! only *some* offsets. That is precisely the shape of bug this generator exists to detect, so
//! it must not be possible in the detector.
//!
//! Three properties matter, and each has a test in `tests/content_generator.rs`:
//!
//! - **Constant-time random access.** A 1 GB case needs no 1 GB fixture, and verifying one
//!   range does not require generating the ranges before it.
//! - **No long runs of zeros.** The corruption most worth catching is a hole — a range marked
//!   complete that was never written, which reads back as zeros. A generator that emitted zero
//!   runs would make that hole invisible at exactly those offsets.
//! - **Incompressible.** A body that arrived gzipped (I-5) must not be able to pass for a
//!   plausible length.

/// The generator name every corpus case cites. A successor gets a new name (ADR-0010).
pub const GENERATOR_V1: &str = "blake3-ctr-v1";

/// A deterministic representation of `len` bytes derived from `seed`.
///
/// Cheap to clone and to construct: it stores two integers and computes everything on demand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Content {
    seed: u64,
    len: u64,
}

/// The first byte at which an actual buffer disagreed with the generator.
///
/// This shape exists so that a corruption report names an actionable offset. "The file differs"
/// costs hours of bisection; "byte 4 194 305 should be 0x7A and is 0x00" names the range that
/// was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mismatch {
    /// Absolute offset in the representation, not an offset into the compared buffer.
    pub offset: u64,
    /// What the generator says should be there.
    pub expected: u8,
    /// What was actually found.
    pub actual: u8,
}

impl Content {
    /// A representation of `len` bytes derived from `seed`.
    #[must_use]
    pub fn new(seed: u64, len: u64) -> Self {
        Self { seed, len }
    }

    /// Total length of the representation.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the representation is zero-length.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The seed this content derives from.
    #[must_use]
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Fill `out` with the correct bytes starting at `offset`.
    ///
    /// This is the bulk path — roughly 0.88 GiB/s on one core — and the only place the stream is
    /// touched, so [`Self::byte_at`] and [`Self::range`] cannot drift away from it.
    ///
    /// Defined for any offset, including past [`Self::len`]: the generator is an infinite stream
    /// and the length is a separate fact about the case. That keeps "the server sent more than it
    /// claimed" expressible.
    pub fn fill(&self, offset: u64, out: &mut [u8]) {
        let mut stream = blake3::Hasher::new()
            .update(&self.seed.to_le_bytes())
            .finalize_xof();
        stream.set_position(offset);
        stream.fill(out);
    }

    /// The correct byte at `offset`, in constant time (about 107 ns in isolation).
    #[must_use]
    pub fn byte_at(&self, offset: u64) -> u8 {
        let mut one = [0_u8; 1];
        self.fill(offset, &mut one);
        one[0]
    }

    /// The correct bytes for `[start, end)`.
    ///
    /// # Panics
    ///
    /// If the range does not fit in memory. This is test-only code; a corpus case asking for a
    /// range larger than addressable memory is a broken case, and failing loudly is correct.
    #[must_use]
    pub fn range(&self, start: u64, end: u64) -> Vec<u8> {
        let len = usize::try_from(end.saturating_sub(start))
            .expect("a corpus case asked for a range larger than addressable memory");
        let mut out = vec![0_u8; len];
        self.fill(start, &mut out);
        out
    }

    /// Compare `actual`, which was read at `offset`, against the generator.
    ///
    /// Returns the first disagreement, or `None` if every byte matches. This is the corpus's
    /// corruption check, which the runner applies to every case unconditionally (ADR-0010).
    ///
    /// Comparison is chunked so that verifying a 1 GB file does not allocate 1 GB, but the
    /// report is per byte so the offset is exact.
    #[must_use]
    pub fn first_mismatch(&self, offset: u64, actual: &[u8]) -> Option<Mismatch> {
        const CHUNK: usize = 64 * 1024;
        let mut expected = vec![0_u8; CHUNK];
        let mut position = 0_usize;

        while position < actual.len() {
            let take = CHUNK.min(actual.len() - position);
            let window = expected.get_mut(..take)?;
            let absolute = offset.saturating_add(u64::try_from(position).unwrap_or(u64::MAX));
            self.fill(absolute, window);

            let found = actual.get(position..position + take)?;
            if window != found {
                for (index, (want, got)) in window.iter().zip(found.iter()).enumerate() {
                    if want != got {
                        return Some(Mismatch {
                            offset: absolute.saturating_add(u64::try_from(index).unwrap_or(0)),
                            expected: *want,
                            actual: *got,
                        });
                    }
                }
            }
            position += take;
        }
        None
    }
}
