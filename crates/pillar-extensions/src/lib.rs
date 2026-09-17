//! pillar-extensions: Luau extension runtime (pi v0.84.3 extension
//! system, per docs/rules/04-luau-extensions.md).
//!
//! Extensions are Luau scripts executed by an embedded luaur VM. Each
//! file loads as a module returning a setup function (mirroring pi's
//! default-export factory); the host calls it with the `@pillar` API
//! table. The VM is sandboxed by capability — an extension only
//! reaches what the host injects.
//!
//! divergences: event names keep pi's snake_case; the host API
//! surface is snake_case (pillar.on / pillar.register_tool) per the
//! naming-convention table.

pub mod bridge;
pub mod loader;
pub mod runtime;

pub use runtime::{ExtensionLoadError, ExtensionRuntime, LoadedExtension};
