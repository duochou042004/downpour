//! The Downpour compatibility corpus.
//!
//! `docs/09-testing-strategy.md` §3 and ADR-0007: *the corpus is the product, the engine is
//! what runs against it.* IDM's real advantage is twenty years of accumulated knowledge about
//! how servers misbehave. We cannot out-wait that, so we out-generate it — produce the
//! pathology space deliberately, in a lab, and turn every finding into a permanent
//! deterministic test.
//!
//! This crate is **test-only** (`publish = false`, never a dependency of a release binary) and
//! will contain three things:
//!
//! - [`content`] — the frozen deterministic content generator that provides ground truth for
//!   every byte, and therefore makes corruption detection exact rather than probabilistic.
//! - the pathology server, which enacts whatever a case describes (S1-T6).
//! - the case runner, which imposes the assertions no case may opt out of (S1-T10, ADR-0010).
//!
//! Because this crate is only ever compiled for tests, it may `unwrap` where the engine crates
//! may not: a panic here fails a test, it does not take down a user's transfers.

pub mod content;
pub mod server;
