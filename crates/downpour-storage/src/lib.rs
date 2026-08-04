//! Crash-safe storage for Downpour.
//!
//! This crate owns I-1's durable block commit, I-6's persisted range evidence, I-8's restart-
//! stable identity, I-9 and I-11 for versioned recovery state, I-10 for exclusive preallocated
//! part files, and I-14's plaintext-secret persistence boundary.

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
pub mod metadata;
pub mod part_file;
pub mod recovery;
pub mod writer;
