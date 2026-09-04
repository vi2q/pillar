//! Port of packages/server/src (pi v0.84.3): the server-side session
//! manager and protocol decision core.
//!
//! divergences: transports (Unix listener), the async connection
//! pump, and UUID generation stay host-side; the port exposes the
//! session lifecycle decisions over trait-injected runtimes.

pub mod listener;
pub mod protocol_bridge;
pub mod server;
pub mod sessions;
pub mod snapshots;
pub mod testing;
pub mod testing_client;
