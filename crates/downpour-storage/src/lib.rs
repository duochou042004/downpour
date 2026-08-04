//! Crash-safe storage for Downpour.
//!
//! This crate owns I-1's durable block commit, I-9 and I-11 for the recovery journal, and I-10
//! for exclusive, preallocated part files.

#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable
    )
)]

pub mod journal;
pub mod part_file;
pub mod writer;
