//! The `pillar` binary crate: joins `pillar-coding-agent` with the Luau
//! extension runtime (`pillar-extensions`).
//!
//! The binary entry lives in `main.rs`; the host wiring lives in [`runner`]
//! so it can be exercised by tests and reused by the runtime bootstrap, the
//! startup trust decision in [`trust`], and the effect gate in [`effects`].

pub mod auth;
pub mod commands;
pub mod effects;
#[cfg(feature = "luau")]
pub mod extension_sources;
pub mod runner;
pub mod self_update;
pub mod trust;
