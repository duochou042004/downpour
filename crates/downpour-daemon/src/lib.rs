//! `downpourd` — the daemon that owns all transfer state.
//!
//! S2 builds exactly one thing here: **startup recovery**. IPC, scheduling, and client
//! migration are S3 and later, and adding them early would fix their shape before the engine
//! that has to live with it exists.
//!
//! This module owns the daemon half of **I-12**. The daemon outliving every UI only means
//! something if it can also outlive *itself* — a machine that lost power mid-transfer has to
//! come back to a state that is honest about what it holds, or the architecture bought nothing.

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

pub mod startup;
