//! Port of the `main.ts` bootstrap helpers for the `pillar` binary:
//! argument-driven app-mode resolution and the plain-metadata shortcut.
//!
//! The full runtime bootstrap (settings/session/services/agent wiring and the
//! interactive/rpc/print mode dispatch) is not ported yet; see docs/TASKS.md.

use crate::cli::args::{Args, Mode};

/// Application mode (upstream `AppMode` from `core/project-trust.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    Print,
    Json,
    Rpc,
    Interactive,
}

impl AppMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Print => "print",
            Self::Json => "json",
            Self::Rpc => "rpc",
            Self::Interactive => "interactive",
        }
    }
}

/// Output mode for print/json runs (upstream `toPrintOutputMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrintOutputMode {
    Text,
    Json,
}

/// Upstream `resolveAppMode`.
pub fn resolve_app_mode(parsed: &Args, stdin_is_tty: bool, stdout_is_tty: bool) -> AppMode {
    if parsed.mode == Some(Mode::Rpc) {
        return AppMode::Rpc;
    }
    if parsed.mode == Some(Mode::Json) {
        return AppMode::Json;
    }
    if parsed.print == Some(true) || !stdin_is_tty || !stdout_is_tty {
        return AppMode::Print;
    }
    AppMode::Interactive
}

/// Upstream `toPrintOutputMode`.
pub fn to_print_output_mode(app_mode: AppMode) -> PrintOutputMode {
    if app_mode == AppMode::Json {
        PrintOutputMode::Json
    } else {
        PrintOutputMode::Text
    }
}

/// Upstream `isPlainRuntimeMetadataCommand`.
pub fn is_plain_runtime_metadata_command(parsed: &Args) -> bool {
    parsed.print != Some(true)
        && parsed.mode.is_none()
        && (parsed.help == Some(true) || parsed.list_models.is_some())
}
