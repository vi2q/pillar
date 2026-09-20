//! Ports of small pi v0.84.3 core modules:
//! - defaults.ts (`DEFAULT_THINKING_LEVEL`, `THINKING_LEVEL_OPTIONS`)
//! - session-export.ts (`exportSessionToJsonl`)
//! - slash-commands.ts (`SlashCommandInfo`, `BUILTIN_SLASH_COMMANDS`)
//! - session-cwd.ts (missing stored-cwd detection and error formatting)
//! - settings-diagnostics.ts (drain + dedupe settings diagnostics)

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::core::session_manager::{
    CURRENT_SESSION_VERSION, FileEntry, SessionHeader, SessionManager, entry_to_json,
};
use crate::core::settings_manager::{SettingsError, SettingsScope};

type TrailingEntriesFn<'a> = &'a dyn Fn(&str, &str) -> Vec<serde_json::Value>;
use crate::core::source_info::SourceInfo;

// ============================================================================
// defaults.ts
// ============================================================================

pub const DEFAULT_THINKING_LEVEL: &str = "medium";
pub const THINKING_LEVEL_OPTIONS: [&str; 7] =
    ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

// ============================================================================
// session-export.ts
// ============================================================================

/// Write the current session branch and optional trailing export-only
/// entries as JSONL (upstream `exportSessionToJsonl`).
///
/// `trailing_entries` receives (parentId, timestamp) and returns extra
/// entries appended after the branch. When `output_path` is None a
/// timestamped `session-<ts>.jsonl` filename is used, resolved against
/// `cwd`.
pub fn export_session_to_jsonl(
    session_manager: &SessionManager,
    output_path: Option<&str>,
    cwd: &str,
    trailing_entries: Option<TrailingEntriesFn<'_>>,
) -> Result<PathBuf, String> {
    let timestamp = iso_timestamp_now();
    let default_name = format!("session-{}.jsonl", timestamp.replace([':', '.'], "-"));
    let file_path =
        crate::core::tools::path_utils::resolve_to_cwd(output_path.unwrap_or(&default_name), cwd);

    if let Some(dir) = file_path.parent()
        && !dir.exists()
    {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }

    let header = SessionHeader {
        r#type: "session".to_string(),
        version: Some(CURRENT_SESSION_VERSION),
        id: session_manager.session_id().to_string(),
        timestamp: timestamp.clone(),
        cwd: session_manager.cwd().to_string(),
        parent_session: None,
    };
    let mut lines = vec![serde_json::to_string(&header).unwrap_or_default()];

    let mut parent_id = String::new();
    let mut have_parent = false;
    for entry in session_manager.get_branch(None) {
        // The port's entries carry their own parent links; re-serialize with
        // the chained parentId like upstream (spread + parentId override).
        let mut value = entry_to_json(entry);
        if let Some(obj) = value.as_object_mut() {
            obj.insert(
                "parentId".to_string(),
                if have_parent {
                    json!(parent_id)
                } else {
                    json!(null)
                },
            );
        }
        lines.push(serde_json::to_string(&value).unwrap_or_default());
        parent_id = entry.id().to_string();
        have_parent = true;
    }
    if let Some(trailing) = trailing_entries {
        for value in trailing(&parent_id, &timestamp) {
            lines.push(serde_json::to_string(&value).unwrap_or_default());
        }
    }

    let mut content = lines.join("\n");
    content.push('\n');
    fs::write(&file_path, content).map_err(|e| e.to_string())?;
    Ok(file_path)
}

/// Current UTC time in ISO-8601 with millisecond precision (matches the
/// upstream `new Date().toISOString()` shape). Uses the `std::time` epoch
/// and a small formatter — no chrono dependency.
pub fn iso_timestamp_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format_epoch_millis(now.as_millis() as u64)
}

/// Format epoch milliseconds as ISO-8601 UTC (`YYYY-MM-DDTHH:MM:SS.mmmZ`).
pub fn format_epoch_millis(millis: u64) -> String {
    let days_since_epoch = millis / 86_400_000;
    let rem = millis % 86_400_000;
    let (year, month, day) = civil_from_days(days_since_epoch as i64);
    let hour = rem / 3_600_000;
    let minute = (rem / 60_000) % 60;
    let second = (rem / 1000) % 60;
    let ms = rem % 1000;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{ms:03}Z")
}

/// Howard Hinnant's `civil_from_days` algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ============================================================================
// slash-commands.ts
// ============================================================================

/// Slash command source (upstream `SlashCommandSource`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommandSource {
    Extension,
    Prompt,
    Skill,
}

impl SlashCommandSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            SlashCommandSource::Extension => "extension",
            SlashCommandSource::Prompt => "prompt",
            SlashCommandSource::Skill => "skill",
        }
    }
}

/// A slash command resolved from a resource (upstream `SlashCommandInfo`).
#[derive(Debug, Clone, PartialEq)]
pub struct SlashCommandInfo {
    pub name: String,
    pub description: Option<String>,
    pub source: SlashCommandSource,
    pub source_info: SourceInfo,
}

/// A built-in slash command (upstream `BuiltinSlashCommand`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinSlashCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub argument_hint: Option<&'static str>,
}

const fn cmd(name: &'static str, description: &'static str) -> BuiltinSlashCommand {
    BuiltinSlashCommand {
        name,
        description,
        argument_hint: None,
    }
}

const fn cmd_arg(
    name: &'static str,
    description: &'static str,
    argument_hint: &'static str,
) -> BuiltinSlashCommand {
    BuiltinSlashCommand {
        name,
        description,
        argument_hint: Some(argument_hint),
    }
}

/// Built-in slash commands (upstream `BUILTIN_SLASH_COMMANDS`, verbatim).
pub const BUILTIN_SLASH_COMMANDS: [BuiltinSlashCommand; 22] = [
    cmd("settings", "Open settings menu"),
    cmd_arg(
        "model",
        "Select model (opens selector UI)",
        "<provider/model>",
    ),
    cmd("tree", "Navigate session tree (switch branches)"),
    cmd_arg("thinking", "Set thinking level", "<level>"),
    cmd("scoped-models", "Enable/disable models for Ctrl+P cycling"),
    cmd(
        "export",
        "Export session (HTML default, or specify path: .html/.jsonl)",
    ),
    cmd("import", "Import and resume a session from a JSONL file"),
    cmd("share", "Share session as a secret GitHub gist"),
    cmd("copy", "Copy last agent message to clipboard"),
    cmd("name", "Set session display name"),
    cmd("session", "Show session info and stats"),
    cmd("changelog", "Show changelog entries"),
    cmd("hotkeys", "Show all keyboard shortcuts"),
    cmd("fork", "Create a new fork from a previous user message"),
    cmd(
        "clone",
        "Duplicate the current session at the current position",
    ),
    cmd("trust", "Save project trust decision for future sessions"),
    cmd_arg("login", "Configure provider authentication", "<provider>"),
    cmd("logout", "Remove provider authentication"),
    cmd("new", "Start a new session"),
    cmd("compact", "Manually compact the session context"),
    cmd("resume", "Resume a different session"),
    cmd(
        "reload",
        "Reload keybindings, extensions, skills, prompts, themes, and context files",
    ),
];

// ============================================================================
// session-cwd.ts
// ============================================================================

/// A stored-session cwd issue (upstream `SessionCwdIssue`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCwdIssue {
    pub session_file: Option<PathBuf>,
    pub session_cwd: String,
    pub fallback_cwd: String,
}

/// Detect a missing stored session cwd (upstream
/// `getMissingSessionCwdIssue`).
pub fn get_missing_session_cwd_issue(
    session_manager: &SessionManager,
    fallback_cwd: &str,
) -> Option<SessionCwdIssue> {
    let session_file = session_manager.session_file()?.to_path_buf();
    let session_cwd = session_manager.cwd();
    if session_cwd.is_empty() || Path::new(session_cwd).exists() {
        return None;
    }
    Some(SessionCwdIssue {
        session_file: Some(session_file),
        session_cwd: session_cwd.to_string(),
        fallback_cwd: fallback_cwd.to_string(),
    })
}

/// Format the issue as an error (upstream `formatMissingSessionCwdError`).
pub fn format_missing_session_cwd_error(issue: &SessionCwdIssue) -> String {
    let session_file = issue
        .session_file
        .as_ref()
        .map(|f| format!("\nSession file: {}", f.display()))
        .unwrap_or_default();
    format!(
        "Stored session working directory does not exist: {}{}\nCurrent working directory: {}",
        issue.session_cwd, session_file, issue.fallback_cwd
    )
}

/// Format the issue as a prompt (upstream `formatMissingSessionCwdPrompt`).
pub fn format_missing_session_cwd_prompt(issue: &SessionCwdIssue) -> String {
    format!(
        "cwd from session file does not exist\n{}\n\ncontinue in current cwd\n{}",
        issue.session_cwd, issue.fallback_cwd
    )
}

/// Throw the cwd error when the stored cwd is missing (upstream
/// `assertSessionCwdExists`).
pub fn assert_session_cwd_exists(
    session_manager: &SessionManager,
    fallback_cwd: &str,
) -> Result<(), String> {
    match get_missing_session_cwd_issue(session_manager, fallback_cwd) {
        Some(issue) => Err(format_missing_session_cwd_error(&issue)),
        None => Ok(()),
    }
}

// ============================================================================
// settings-diagnostics.ts
// ============================================================================

/// A runtime diagnostic (upstream `AgentSessionRuntimeDiagnostic`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSessionRuntimeDiagnostic {
    pub diagnostic_type: &'static str,
    pub message: String,
}

/// Collect settings diagnostics (upstream `collectSettingsDiagnostics`).
pub fn collect_settings_diagnostics(
    settings_manager: &mut SessionSettingsSource<'_>,
) -> Vec<AgentSessionRuntimeDiagnostic> {
    settings_manager
        .drain_errors()
        .into_iter()
        .map(|error| AgentSessionRuntimeDiagnostic {
            diagnostic_type: "warning",
            message: match &error.path {
                Some(path) => format!(
                    "Invalid settings file {}: {}",
                    path.display(),
                    error.message
                ),
                None => format!(
                    "Invalid {} settings: {}",
                    match error.scope {
                        SettingsScope::Global => "global",
                        SettingsScope::Project => "project",
                    },
                    error.message
                ),
            },
        })
        .collect()
}

/// Narrow source trait so diagnostics can be collected without a full
/// SettingsManager (mirrors upstream taking a SettingsManager).
pub struct SessionSettingsSource<'a>(pub &'a mut crate::core::settings_manager::SettingsManager);

impl SessionSettingsSource<'_> {
    fn drain_errors(&mut self) -> Vec<SettingsError> {
        self.0.drain_errors()
    }
}

/// Remove duplicate type/message diagnostics while preserving first
/// occurrence (upstream `deduplicateDiagnostics`).
pub fn deduplicate_diagnostics(
    diagnostics: Vec<AgentSessionRuntimeDiagnostic>,
) -> Vec<AgentSessionRuntimeDiagnostic> {
    let mut seen = std::collections::BTreeSet::new();
    diagnostics
        .into_iter()
        .filter(|diagnostic| {
            let key = format!("{}\0{}", diagnostic.diagnostic_type, diagnostic.message);
            seen.insert(key)
        })
        .collect()
}

// Re-export for parity tests exercising the FileEntry JSON shape.
#[allow(unused_imports)]
use FileEntry as _FileEntry;
