//! `downpourd` — the daemon that owns all transfer state.
//!
//! S2 established startup recovery. S3 adds the authenticated IPC server and the daemon-owned
//! transfer registry; clients can submit and observe work but never own its task.
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
        clippy::unreachable,
        // A debug print in a library crate is noise on a user's terminal that no log level can
        // turn off, and it is how a temporary probe survives review — one did, in this crate's
        // segmented orchestration, and was committed. Library code reports through `tracing`.
        clippy::print_stdout,
        clippy::print_stderr
    )
)]

pub mod server;
pub mod startup;
