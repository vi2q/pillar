//! Sandboxed Luau extensions and isolated Code mode execution.
//!
//! Code mode keeps each VM on its dedicated execution thread. The default
//! `extension-runtime` feature also provides the shared extension VM, loader
//! and host bridge. Extensions can reach only capabilities injected by the host.

#[cfg(feature = "extension-runtime")]
pub mod bridge;
pub mod codemode;
#[cfg(feature = "extension-runtime")]
pub mod loader;
#[cfg(feature = "extension-runtime")]
pub mod runtime;

#[cfg(feature = "extension-runtime")]
pub use runtime::{
    ExtensionLoadError, ExtensionRuntime, HostCallRequest, LoadedExtension, ToolCall, ToolStep,
    VmBudget,
};
