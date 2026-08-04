//! Crash-safe storage for Downpour.
//!
//! This crate owns I-9 and I-11 for the recovery-journal format, replay, repair, and compaction.
//! Part-file allocation and durable block commits are added by later S2 tasks.

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
