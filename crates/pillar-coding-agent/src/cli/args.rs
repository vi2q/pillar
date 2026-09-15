//! Port of packages/coding-agent/src/cli/args.ts (pi v0.84.3): CLI argument
//! parsing and help display.
//!
//! divergence: upstream reads `APP_NAME` / `CONFIG_DIR_NAME` from
//! `package.json` (`piConfig`) and colorizes help with chalk. The port names
//! itself `pillar` ([`APP_NAME`]) and reports its own version, but keeps the
//! pillar config identity ([`CONFIG_DIR_NAME`] = ".pillar" and the `PILLAR_*`
//! environment variables) so existing sessions, settings, credentials, and
//! extensions stay interoperable; help renders as plain text.

use std::collections::BTreeMap;

/// Binary/agent name (upstream `APP_NAME`).
pub const APP_NAME: &str = "pillar";

/// Agent config directory environment variable (upstream `ENV_AGENT_DIR`).
pub const ENV_AGENT_DIR: &str = "PILLAR_CODING_AGENT_DIR";

/// Session directory environment variable (upstream `ENV_SESSION_DIR`).
pub const ENV_SESSION_DIR: &str = "PILLAR_CODING_AGENT_SESSION_DIR";

/// Version reported by `--version` (upstream `VERSION` from package.json).
/// The port reports its own version (upstream's is pinned in the port notes
/// for parity work).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Output mode (`--mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Text,
    Json,
    Rpc,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Json => "json",
            Self::Rpc => "rpc",
        }
    }
}

/// Interactive TUI mode (`--tui-mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiMode {
    Regular,
    Fullscreen,
}

impl TuiMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Regular => "regular",
            Self::Fullscreen => "fullscreen",
        }
    }
}

/// `--list-models` with or without a search pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListModels {
    /// `--list-models` with no pattern.
    All,
    /// `--list-models <pattern>`.
    Search(String),
}

/// A diagnostic emitted while parsing (upstream `{ type, message }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgDiagnostic {
    pub kind: DiagnosticKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticKind {
    Warning,
    Error,
}

/// Value of an unrecognized long flag (potentially an extension flag).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlagValue {
    Boolean(bool),
    String(String),
}

/// Parsed CLI arguments (upstream `Args`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Args {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<Vec<String>>,
    pub thinking: Option<String>,
    pub continue_session: Option<bool>,
    pub resume: Option<bool>,
    pub help: Option<bool>,
    pub version: Option<bool>,
    pub mode: Option<Mode>,
    pub name: Option<String>,
    pub no_session: Option<bool>,
    pub session: Option<String>,
    pub session_id: Option<String>,
    pub fork: Option<String>,
    pub session_dir: Option<String>,
    pub models: Option<Vec<String>>,
    pub tools: Option<Vec<String>>,
    pub exclude_tools: Option<Vec<String>>,
    pub no_tools: Option<bool>,
    pub no_builtin_tools: Option<bool>,
    pub extensions: Option<Vec<String>>,
    pub no_extensions: Option<bool>,
    pub print: Option<bool>,
    pub export: Option<String>,
    pub no_skills: Option<bool>,
    pub skills: Option<Vec<String>>,
    pub prompt_templates: Option<Vec<String>>,
    pub no_prompt_templates: Option<bool>,
    pub themes: Option<Vec<String>>,
    pub use_theme: Option<String>,
    pub no_themes: Option<bool>,
    pub no_context_files: Option<bool>,
    pub list_models: Option<ListModels>,
    pub offline: Option<bool>,
    pub tui_mode: Option<TuiMode>,
    pub verbose: Option<bool>,
    pub project_trust_override: Option<bool>,
    pub messages: Vec<String>,
    pub file_args: Vec<String>,
    /// Unknown flags (potentially extension flags), name -> value.
    pub unknown_flags: BTreeMap<String, FlagValue>,
    pub diagnostics: Vec<ArgDiagnostic>,
}

pub const VALID_THINKING_LEVELS: [&str; 7] =
    ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Whether a string is a valid pi thinking level.
pub fn is_valid_thinking_level(level: &str) -> bool {
    VALID_THINKING_LEVELS.contains(&level)
}

/// Trim a session display name; whitespace-only values are rejected
/// (upstream `normalizeSessionName`).
pub fn normalize_session_name(value: &str) -> Option<String> {
    let name = value.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

fn error(message: impl Into<String>) -> ArgDiagnostic {
    ArgDiagnostic {
        kind: DiagnosticKind::Error,
        message: message.into(),
    }
}

fn warning(message: impl Into<String>) -> ArgDiagnostic {
    ArgDiagnostic {
        kind: DiagnosticKind::Warning,
        message: message.into(),
    }
}

/// Look up the value after a flag, consuming it (upstream `args[++i]`).
fn value_at(args: &[String], index: &mut usize) -> Option<String> {
    if *index + 1 < args.len() {
        *index += 1;
        Some(args[*index].clone())
    } else {
        None
    }
}

/// Upstream `parseArgs`.
pub fn parse_args(args: &[String]) -> Args {
    let mut result = Args::default();
    let mut i = 0usize;
    while i < args.len() {
        let arg = args[i].as_str();

        if arg == "--" {
            for positional in &args[i + 1..] {
                if let Some(rest) = positional.strip_prefix('@') {
                    result.file_args.push(rest.to_string());
                } else {
                    result.messages.push(positional.clone());
                }
            }
            break;
        } else if arg == "--help" || arg == "-h" {
            result.help = Some(true);
        } else if arg == "--version" || arg == "-v" {
            result.version = Some(true);
        } else if arg == "--mode" && i + 1 < args.len() {
            let mode = value_at(args, &mut i).expect("checked length");
            match mode.as_str() {
                "text" => result.mode = Some(Mode::Text),
                "json" => result.mode = Some(Mode::Json),
                "rpc" => result.mode = Some(Mode::Rpc),
                _ => {}
            }
        } else if arg == "--continue" || arg == "-c" {
            result.continue_session = Some(true);
        } else if arg == "--resume" || arg == "-r" {
            result.resume = Some(true);
        } else if arg == "--provider" && i + 1 < args.len() {
            result.provider = value_at(args, &mut i);
        } else if arg == "--model" && i + 1 < args.len() {
            result.model = value_at(args, &mut i);
        } else if arg == "--api-key" && i + 1 < args.len() {
            result.api_key = value_at(args, &mut i);
        } else if arg == "--system-prompt" && i + 1 < args.len() {
            result.system_prompt = value_at(args, &mut i);
        } else if arg == "--append-system-prompt" && i + 1 < args.len() {
            let value = value_at(args, &mut i).expect("checked length");
            result
                .append_system_prompt
                .get_or_insert_with(Vec::new)
                .push(value);
        } else if arg == "--name" || arg == "-n" {
            if i + 1 < args.len() {
                result.name = value_at(args, &mut i);
            } else {
                result.diagnostics.push(error("--name requires a value"));
            }
        } else if arg == "--no-session" {
            result.no_session = Some(true);
        } else if arg == "--session" && i + 1 < args.len() {
            result.session = value_at(args, &mut i);
        } else if arg == "--session-id" && i + 1 < args.len() {
            result.session_id = value_at(args, &mut i);
        } else if arg == "--fork" && i + 1 < args.len() {
            result.fork = value_at(args, &mut i);
        } else if arg == "--session-dir" && i + 1 < args.len() {
            result.session_dir = value_at(args, &mut i);
        } else if arg == "--models" && i + 1 < args.len() {
            let value = value_at(args, &mut i).expect("checked length");
            result.models = Some(value.split(',').map(|s| s.trim().to_string()).collect());
        } else if arg == "--no-tools" || arg == "-nt" {
            result.no_tools = Some(true);
        } else if arg == "--no-builtin-tools" || arg == "-nbt" {
            result.no_builtin_tools = Some(true);
        } else if (arg == "--tools" || arg == "-t") && i + 1 < args.len() {
            let value = value_at(args, &mut i).expect("checked length");
            result.tools = Some(split_names(&value));
        } else if (arg == "--exclude-tools" || arg == "-xt") && i + 1 < args.len() {
            let value = value_at(args, &mut i).expect("checked length");
            result.exclude_tools = Some(split_names(&value));
        } else if arg == "--thinking" && i + 1 < args.len() {
            let level = value_at(args, &mut i).expect("checked length");
            if is_valid_thinking_level(&level) {
                result.thinking = Some(level);
            } else {
                result.diagnostics.push(warning(format!(
                    "Invalid thinking level \"{level}\". Valid values: {}",
                    VALID_THINKING_LEVELS.join(", ")
                )));
            }
        } else if arg == "--print" || arg == "-p" {
            result.print = Some(true);
            if let Some(next) = args.get(i + 1) {
                if !next.starts_with('@') && (!next.starts_with('-') || next.starts_with("---")) {
                    result.messages.push(next.clone());
                    i += 1;
                }
            }
        } else if arg == "--export" && i + 1 < args.len() {
            result.export = value_at(args, &mut i);
        } else if (arg == "--extension" || arg == "-e") && i + 1 < args.len() {
            let value = value_at(args, &mut i).expect("checked length");
            result.extensions.get_or_insert_with(Vec::new).push(value);
        } else if arg == "--no-extensions" || arg == "-ne" {
            result.no_extensions = Some(true);
        } else if arg == "--skill" && i + 1 < args.len() {
            let value = value_at(args, &mut i).expect("checked length");
            result.skills.get_or_insert_with(Vec::new).push(value);
        } else if arg == "--prompt-template" && i + 1 < args.len() {
            let value = value_at(args, &mut i).expect("checked length");
            result
                .prompt_templates
                .get_or_insert_with(Vec::new)
                .push(value);
        } else if arg == "--theme" && i + 1 < args.len() {
            let value = value_at(args, &mut i).expect("checked length");
            result.themes.get_or_insert_with(Vec::new).push(value);
        } else if arg == "--use-theme" {
            match args.get(i + 1) {
                Some(name) if !name.starts_with('-') => {
                    result.use_theme = Some(name.clone());
                    i += 1;
                }
                _ => result
                    .diagnostics
                    .push(error("--use-theme requires a theme name")),
            }
        } else if arg == "--no-skills" || arg == "-ns" {
            result.no_skills = Some(true);
        } else if arg == "--no-prompt-templates" || arg == "-np" {
            result.no_prompt_templates = Some(true);
        } else if arg == "--no-themes" {
            result.no_themes = Some(true);
        } else if arg == "--no-context-files" || arg == "-nc" {
            result.no_context_files = Some(true);
        } else if arg == "--list-models" {
            match args.get(i + 1) {
                Some(next) if !next.starts_with('-') && !next.starts_with('@') => {
                    result.list_models = Some(ListModels::Search(next.clone()));
                    i += 1;
                }
                _ => result.list_models = Some(ListModels::All),
            }
        } else if arg == "--tui-mode" {
            match args.get(i + 1) {
                Some(mode) if mode == "regular" => {
                    result.tui_mode = Some(TuiMode::Regular);
                    i += 1;
                }
                Some(mode) if mode == "fullscreen" => {
                    result.tui_mode = Some(TuiMode::Fullscreen);
                    i += 1;
                }
                Some(mode) if mode.starts_with('-') => {
                    result
                        .diagnostics
                        .push(error("--tui-mode requires regular or fullscreen"));
                }
                Some(mode) => {
                    i += 1;
                    result.diagnostics.push(error(format!(
                        "Invalid TUI mode \"{mode}\". Valid values: regular, fullscreen"
                    )));
                }
                None => result
                    .diagnostics
                    .push(error("--tui-mode requires regular or fullscreen")),
            }
        } else if arg == "--verbose" {
            result.verbose = Some(true);
        } else if arg == "--approve" || arg == "-a" {
            result.project_trust_override = Some(true);
        } else if arg == "--no-approve" || arg == "-na" {
            result.project_trust_override = Some(false);
        } else if arg == "--offline" {
            result.offline = Some(true);
        } else if let Some(path) = arg.strip_prefix('@') {
            result.file_args.push(path.to_string());
        } else if let Some(flag) = arg.strip_prefix("--") {
            if let Some(eq_index) = flag.find('=') {
                result.unknown_flags.insert(
                    flag[..eq_index].to_string(),
                    FlagValue::String(flag[eq_index + 1..].to_string()),
                );
            } else {
                match args.get(i + 1) {
                    Some(next) if !next.starts_with('-') && !next.starts_with('@') => {
                        result
                            .unknown_flags
                            .insert(flag.to_string(), FlagValue::String(next.clone()));
                        i += 1;
                    }
                    _ => {
                        result
                            .unknown_flags
                            .insert(flag.to_string(), FlagValue::Boolean(true));
                    }
                }
            }
        } else if arg.starts_with('-') {
            result
                .diagnostics
                .push(error(format!("Unknown option: {arg}")));
        } else {
            result.messages.push(arg.to_string());
        }

        i += 1;
    }
    result
}

fn split_names(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect()
}
