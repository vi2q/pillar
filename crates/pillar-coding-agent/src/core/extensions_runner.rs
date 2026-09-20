//! The runner lives in the extension contract (`pillar_extensions_contract::runner`):
//! the VM builds `HostExtension` values and the coding agent drives the runner,
//! so neither crate owns the other (docs/DEVELOPMENT-STRATEGY.md §4).
//!
//! Re-exported here so existing paths keep working.

pub use pillar_extensions_contract::{
    BuiltinKeybinding, DiscoveredResources, ExtensionError, ExtensionEventPayload, ExtensionFlag,
    ExtensionHandler, ExtensionRunner, ExtensionShortcut, HandlerResult, HostExtension,
    ProjectTrustDecision, RESERVED_KEYBINDINGS_FOR_EXTENSION_CONFLICTS, RegisteredCommand,
    ResolvedCommand, build_builtin_keybindings, emit_project_trust_event,
    emit_session_shutdown_event, in_extension_dispatch, queue_extension_event,
};
