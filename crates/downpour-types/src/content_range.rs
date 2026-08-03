//! `Range` and `Content-Range` header handling.
//!
//! This module owns the request-side half of **I-5**: a response range is *checked against
//! the range that was actually requested*, never trusted. The failure it prevents is the
//! quiet one — the server describes bytes we did not ask for, the engine writes the body at
//! the offset it wanted, and the download finishes at exactly the expected size, full of
//! garbage. Nothing short of hashing the content notices.
//!
//! Grammar: RFC 9110 §14.1.2 (`Range`) and §14.4 (`Content-Range`).

use core::fmt;
use core::str::FromStr;

use thiserror::Error;

/// A byte range as sent in a request `Range` header (RFC 9110 §14.1.2).
///
/// Both bounds are inclusive, which is HTTP's convention and not Rust's. The type exists
/// partly so that the off-by-one lives in one place instead of at every call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteRangeSpec {
    /// `bytes=first-last`. Both positions are inclusive.
    FromTo {
        /// First byte position, inclusive.
        first: u64,
        /// Last byte position, inclusive.
        last: u64,
    },
    /// `bytes=first-` — from `first` to the end of the representation.
    From {
        /// First byte position, inclusive.
        first: u64,
    },
    /// `bytes=-len` — the final `len` bytes of the representation.
    Suffix {
        /// How many bytes from the end.
        len: u64,
    },
}

impl ByteRangeSpec {
    /// The wire form for a `Range` request header, without the header name.
    ///
    /// ```
    /// use downpour_types::ByteRangeSpec;
    /// // The capability probe's request (docs/03-transfer-engine-spec.md §2.1).
    /// assert_eq!(ByteRangeSpec::FromTo { first: 0, last: 0 }.header_value(), "bytes=0-0");
    /// ```
    #[must_use]
    pub fn header_value(&self) -> String {
        match *self {
            Self::FromTo { first, last } => format!("bytes={first}-{last}"),
            Self::From { first } => format!("bytes={first}-"),
            Self::Suffix { len } => format!("bytes=-{len}"),
        }
    }

    /// How many bytes this asks for, when that is knowable without the representation
    /// length. [`Self::From`] is open-ended, so it returns `None`.
    #[must_use]
    pub fn requested_len(&self) -> Option<u64> {
        match *self {
            Self::FromTo { first, last } => last.checked_sub(first)?.checked_add(1),
            Self::From { .. } => None,
            Self::Suffix { len } => Some(len),
        }
    }
}

impl fmt::Display for ByteRangeSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.header_value())
    }
}

/// A parsed `Content-Range` response header (RFC 9110 §14.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentRange {
    /// A satisfied range: `bytes first-last/complete-length`, or `bytes first-last/*` when
    /// the server does not know the total length.
    Bytes {
        /// First byte position of the enclosed data, inclusive.
        first: u64,
        /// Last byte position of the enclosed data, inclusive.
        last: u64,
        /// Total length of the representation, when the server stated it.
        complete_length: Option<u64>,
    },
    /// `bytes */complete-length` — the requested range was not satisfiable. Accompanies a
    /// `416`, and is never consistent with a satisfiable request.
    Unsatisfied {
        /// Total length of the representation.
        complete_length: u64,
    },
}

impl ContentRange {
    /// Number of bytes the enclosed data should contain, or `None` for
    /// [`Self::Unsatisfied`], which encloses none.
    ///
    /// Infallible because parsing rejects `last == u64::MAX`; see [`ContentRangeError`].
    #[must_use]
    pub fn len(&self) -> Option<u64> {
        match *self {
            Self::Bytes { first, last, .. } => Some(last - first + 1),
            Self::Unsatisfied { .. } => None,
        }
    }

    /// Whether the enclosed data is empty. Only [`Self::Unsatisfied`] is.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len().is_none()
    }

    /// The total representation length, when the server stated it.
    #[must_use]
    pub fn complete_length(&self) -> Option<u64> {
        match *self {
            Self::Bytes {
                complete_length, ..
            } => complete_length,
            Self::Unsatisfied { complete_length } => Some(complete_length),
        }
    }

    /// Check this response range against the range that was actually requested.
    ///
    /// The rule is asymmetric on purpose. The **start** must match exactly: an offset we did
    /// not ask for is never safe to write at. The **end** may fall short, because a server is
    /// allowed to satisfy a range request with fewer bytes than asked for, and the remainder
    /// simply goes back to the allocator.
    ///
    /// ```
    /// use downpour_types::{ByteRangeSpec, ContentRange};
    /// let requested = ByteRangeSpec::FromTo { first: 1000, last: 1999 };
    ///
    /// // Short but aligned: legal.
    /// let short: ContentRange = "bytes 1000-1499/8000".parse()?;
    /// assert!(short.is_consistent_with(requested).is_ok());
    ///
    /// // Shifted by one byte: rejected. Writing this at offset 1000 corrupts the file.
    /// let shifted: ContentRange = "bytes 1001-1500/8000".parse()?;
    /// assert!(shifted.is_consistent_with(requested).is_err());
    /// # Ok::<(), downpour_types::ContentRangeError>(())
    /// ```
    pub fn is_consistent_with(&self, requested: ByteRangeSpec) -> Result<(), RangeMismatch> {
        let (first, last, complete_length) = match *self {
            Self::Bytes {
                first,
                last,
                complete_length,
            } => (first, last, complete_length),
            Self::Unsatisfied { .. } => return Err(RangeMismatch::Unsatisfied),
        };

        match requested {
            ByteRangeSpec::FromTo {
                first: want_first,
                last: want_last,
            } => {
                if first != want_first {
                    return Err(RangeMismatch::WrongStart {
                        requested: want_first,
                        returned: first,
                    });
                }
                if last > want_last {
                    return Err(RangeMismatch::EndBeyondRequest {
                        requested_last: want_last,
                        returned_last: last,
                    });
                }
                Ok(())
            }
            ByteRangeSpec::From { first: want_first } => {
                if first != want_first {
                    return Err(RangeMismatch::WrongStart {
                        requested: want_first,
                        returned: first,
                    });
                }
                Ok(())
            }
            ByteRangeSpec::Suffix { len } => {
                // A suffix's start position is a function of the total length, so without a
                // stated total length there is nothing to check it against. Unverifiable is
                // treated as inconsistent — never as "probably fine".
                let Some(complete_length) = complete_length else {
                    return Err(RangeMismatch::UnverifiableSuffix);
                };
                let want_first = complete_length.saturating_sub(len);
                let want_last = complete_length - 1; // parsing guarantees complete_length >= 1
                if first != want_first {
                    return Err(RangeMismatch::WrongStart {
                        requested: want_first,
                        returned: first,
                    });
                }
                if last != want_last {
                    return Err(RangeMismatch::EndBeyondRequest {
                        requested_last: want_last,
                        returned_last: last,
                    });
                }
                Ok(())
            }
        }
    }
}

impl fmt::Display for ContentRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Bytes {
                first,
                last,
                complete_length,
            } => match complete_length {
                Some(total) => write!(f, "bytes {first}-{last}/{total}"),
                None => write!(f, "bytes {first}-{last}/*"),
            },
            Self::Unsatisfied { complete_length } => write!(f, "bytes */{complete_length}"),
        }
    }
}

impl FromStr for ContentRange {
    type Err = ContentRangeError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let s = raw.trim();
        if s.is_empty() {
            return Err(ContentRangeError::Empty);
        }

        let (unit, rest) = s.split_once(' ').ok_or(ContentRangeError::Malformed(
            "no space after the range unit",
        ))?;
        if !unit.eq_ignore_ascii_case("bytes") {
            return Err(ContentRangeError::UnsupportedUnit(unit.to_owned()));
        }
        // Range units are case-insensitive and a doubled SP after the unit is emitted by
        // real servers. Neither changes the meaning, so both are tolerated. Everything
        // after this point is strict.
        let rest = rest.trim_start_matches(' ');

        let (range_part, length_part) = rest.split_once('/').ok_or(
            ContentRangeError::Malformed("no '/' before the complete-length"),
        )?;
        if range_part.is_empty() || length_part.is_empty() {
            return Err(ContentRangeError::Malformed(
                "empty range or complete-length",
            ));
        }
        if range_part.bytes().any(|b| b.is_ascii_whitespace())
            || length_part.bytes().any(|b| b.is_ascii_whitespace())
        {
            return Err(ContentRangeError::Malformed("internal whitespace"));
        }

        let complete_length = if length_part == "*" {
            None
        } else {
            Some(parse_decimal(length_part)?)
        };

        if range_part == "*" {
            return match complete_length {
                Some(complete_length) => Ok(Self::Unsatisfied { complete_length }),
                // `bytes */*` states neither which bytes are enclosed nor how many there
                // are in total. It conveys nothing and is treated as malformed.
                None => Err(ContentRangeError::Malformed(
                    "both the range and the complete-length are unknown",
                )),
            };
        }

        let (first_raw, last_raw) =
            range_part
                .split_once('-')
                .ok_or(ContentRangeError::Malformed(
                    "no '-' between the byte positions",
                ))?;
        let first = parse_decimal(first_raw)?;
        let last = parse_decimal(last_raw)?;

        if first > last {
            return Err(ContentRangeError::Reversed { first, last });
        }
        // Rejected so that `len()` — `last - first + 1` — cannot overflow, which is what
        // lets every caller treat a parsed range's length as infallible. A representation
        // of 2^64 bytes does not exist.
        if last == u64::MAX {
            return Err(ContentRangeError::NumericOverflow);
        }
        if let Some(complete_length) = complete_length
            && last >= complete_length
        {
            return Err(ContentRangeError::LastByteBeyondComplete {
                last,
                complete_length,
            });
        }

        Ok(Self::Bytes {
            first,
            last,
            complete_length,
        })
    }
}

/// Strict decimal parse. HTTP's grammar is `1*DIGIT`, so a sign, a prefix, or anything
/// non-numeric is a malformed header rather than something to guess at.
fn parse_decimal(s: &str) -> Result<u64, ContentRangeError> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ContentRangeError::Malformed(
            "expected a decimal digit sequence",
        ));
    }
    s.parse().map_err(|_| ContentRangeError::NumericOverflow)
}

/// Why a `Content-Range` header could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ContentRangeError {
    /// The header was absent or empty after trimming.
    #[error("Content-Range is empty")]
    Empty,
    /// The range unit was something other than `bytes`.
    #[error("unsupported range unit {0:?}; only `bytes` is understood")]
    UnsupportedUnit(String),
    /// The header did not match the grammar. The payload says which part failed.
    #[error("malformed Content-Range: {0}")]
    Malformed(&'static str),
    /// `first-byte-pos` was greater than `last-byte-pos`.
    #[error("Content-Range is reversed: first byte {first} is after last byte {last}")]
    Reversed {
        /// The stated first byte position.
        first: u64,
        /// The stated last byte position.
        last: u64,
    },
    /// The last byte position fell outside the stated representation length.
    #[error("last byte {last} lies outside a representation of {complete_length} bytes")]
    LastByteBeyondComplete {
        /// The stated last byte position.
        last: u64,
        /// The stated total length.
        complete_length: u64,
    },
    /// A byte position or length did not fit in a `u64`.
    #[error("a byte position or length in Content-Range does not fit in u64")]
    NumericOverflow,
}

/// Why a response range was not consistent with the range that was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RangeMismatch {
    /// The server said the range could not be satisfied.
    #[error("the server reported the requested range as unsatisfiable")]
    Unsatisfied,
    /// The enclosed data starts somewhere other than where we asked it to. This is the
    /// dangerous one: writing it at the requested offset corrupts the file silently.
    #[error("Content-Range starts at byte {returned}, but byte {requested} was requested")]
    WrongStart {
        /// The first byte position that was requested.
        requested: u64,
        /// The first byte position the server described.
        returned: u64,
    },
    /// The enclosed data extends past the end of the requested range.
    #[error("Content-Range ends at byte {returned_last}, past the requested {requested_last}")]
    EndBeyondRequest {
        /// The last byte position that was requested.
        requested_last: u64,
        /// The last byte position the server described.
        returned_last: u64,
    },
    /// A suffix range was returned without a total length, so its start cannot be checked.
    #[error("a suffix range cannot be verified without a stated complete-length")]
    UnverifiableSuffix,
}
