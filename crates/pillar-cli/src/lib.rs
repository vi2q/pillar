//! The `pillar` binary crate: joins `pillar-coding-agent` with the Luau
//! extension runtime (`pillar-extensions`).
//!
//! The binary entry lives in `main.rs`; the host wiring lives in [`runner`]
//! so it can be exercised by tests and reused by the runtime bootstrap, and
//! the startup trust decision lives in [`trust`].

pub mod runner;
pub mod trust;
