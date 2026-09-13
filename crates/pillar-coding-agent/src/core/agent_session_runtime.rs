//! Port of packages/coding-agent/src/core/agent-session-services.ts and
//! agent-session-runtime.ts (pi v0.84.3): cwd-bound runtime services and
//! the session runtime that owns replace/switch/fork/import flows.
//!
//! divergences: the extension runner is host-injected (the JS extension
//! runtime is not ported), so `session_before_switch` /
//! `session_before_fork` / `session_shutdown` hooks surface as optional
//! cancellation and notification callbacks owned by the caller; the
//! ModelRuntime and SDK session assembly are not ported yet — the runtime
//! factory is a caller-provided closure that produces a new services
//! bundle plus session manager for a given cwd.

use std::fs;
use std::path::{Path, PathBuf};

use crate::core::resource_loader::ResourceLoader;
use crate::core::session_manager::SessionManager;
use crate::core::session_support::{
    SessionCwdIssue, assert_session_cwd_exists, format_missing_session_cwd_error,
    get_missing_session_cwd_issue,
};
use crate::core::settings_manager::{SettingsManager, SettingsManagerCreateOptions};
use crate::core::tools::path_utils::resolve_to_cwd;

// ============================================================================
// agent-session-services.ts
// ============================================================================

/// A non-fatal issue collected while creating services or sessions
/// (upstream `AgentSessionRuntimeDiagnostic`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSessionRuntimeDiagnostic {
    /// "info" | "warning" | "error".
    pub kind: &'static str,
    pub message: String,
}

/// Coherent cwd-bound runtime services for one effective session cwd
/// (upstream `AgentSessionServices`).
pub struct AgentSessionServices {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub settings_manager: std::sync::Arc<std::sync::Mutex<SettingsManager>>,
    pub resource_loader: ResourceLoader,
    pub diagnostics: Vec<AgentSessionRuntimeDiagnostic>,
}

/// Inputs for creating cwd-bound runtime services (upstream
/// `CreateAgentSessionServicesOptions`).
#[derive(Debug, Clone, Default)]
pub struct CreateAgentSessionServicesOptions {
    pub agent_dir: Option<String>,
    pub additional_extension_paths: Vec<String>,
    pub additional_skill_paths: Vec<String>,
    pub additional_prompt_template_paths: Vec<String>,
    pub additional_theme_paths: Vec<String>,
    pub no_extensions: bool,
    pub no_skills: bool,
    pub no_prompt_templates: bool,
    pub no_themes: bool,
    pub no_context_files: bool,
}

/// Create cwd-bound runtime services plus diagnostics (upstream
/// `createAgentSessionServices`). The model runtime is not ported; provider
/// registration diagnostics would be contributed by the host.
pub fn create_agent_session_services(
    cwd: &str,
    options: CreateAgentSessionServicesOptions,
) -> Result<AgentSessionServices, String> {
    let resolved_cwd = resolve_to_cwd(cwd, "/");
    let agent_dir = match &options.agent_dir {
        Some(dir) => resolve_to_cwd(dir, "/"),
        None => default_agent_dir(),
    };

    let settings_manager = std::sync::Arc::new(std::sync::Mutex::new(SettingsManager::create(
        &resolved_cwd.to_string_lossy(),
        &agent_dir,
        SettingsManagerCreateOptions {
            project_trusted: Some(true),
        },
    )));

    let mut resource_loader = ResourceLoader::new(
        &resolved_cwd.to_string_lossy(),
        crate::core::resource_loader::ResourceLoaderOptions {
            agent_dir: agent_dir.to_string_lossy().to_string(),
            additional_extension_paths: options.additional_extension_paths,
            additional_skill_paths: options.additional_skill_paths,
            additional_prompt_template_paths: options.additional_prompt_template_paths,
            additional_theme_paths: options.additional_theme_paths,
            no_extensions: options.no_extensions,
            no_skills: options.no_skills,
            no_prompt_templates: options.no_prompt_templates,
            no_themes: options.no_themes,
            no_context_files: options.no_context_files,
            system_prompt: None,
            append_system_prompt: None,
        },
        settings_manager.clone(),
    );
    resource_loader.reload(None)?;

    let mut diagnostics = Vec::new();
    // Settings diagnostics (upstream collects via settings-diagnostics.ts).
    {
        let mut settings = settings_manager.lock().unwrap();
        for error in settings.drain_errors() {
            diagnostics.push(AgentSessionRuntimeDiagnostic {
                kind: "warning",
                message: match &error.path {
                    Some(path) => format!(
                        "Invalid settings file {}: {}",
                        path.display(),
                        error.message
                    ),
                    None => format!(
                        "Invalid {} settings: {}",
                        match error.scope {
                            crate::core::settings_manager::SettingsScope::Global => "global",
                            crate::core::settings_manager::SettingsScope::Project => "project",
                        },
                        error.message
                    ),
                },
            });
        }
    }

    Ok(AgentSessionServices {
        cwd: resolved_cwd,
        agent_dir,
        settings_manager,
        resource_loader,
        diagnostics,
    })
}

fn default_agent_dir() -> PathBuf {
    std::env::var_os("PI_AGENT_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".pi").join("agent")))
        .unwrap_or_else(|| PathBuf::from(".pi").join("agent"))
}

// ============================================================================
// agent-session-runtime.ts
// ============================================================================

/// Thrown when /import references a JSONL file path that does not exist
/// (upstream `SessionImportFileNotFoundError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionImportFileNotFoundError {
    pub file_path: PathBuf,
}

impl SessionImportFileNotFoundError {
    pub fn message(&self) -> String {
        format!("File not found: {}", self.file_path.display())
    }
}

/// A missing stored-cwd error surfaced from runtime creation (upstream
/// `MissingSessionCwdError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingSessionCwdError {
    pub issue: SessionCwdIssue,
}

impl MissingSessionCwdError {
    pub fn message(&self) -> String {
        format_missing_session_cwd_error(&self.issue)
    }
}

/// Why the current session is being torn down (upstream
/// `SessionShutdownEvent["reason"]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownReason {
    New,
    Resume,
    Fork,
    Quit,
}

impl ShutdownReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            ShutdownReason::New => "new",
            ShutdownReason::Resume => "resume",
            ShutdownReason::Fork => "fork",
            ShutdownReason::Quit => "quit",
        }
    }
}

/// Why a new session started (upstream
/// `SessionStartEvent["reason"]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStartReason {
    Startup,
    New,
    Resume,
    Fork,
}

impl SessionStartReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionStartReason::Startup => "startup",
            SessionStartReason::New => "new",
            SessionStartReason::Resume => "resume",
            SessionStartReason::Fork => "fork",
        }
    }
}

/// Host hooks the runtime invokes during session replacement (upstream the
/// extension events `session_before_switch`, `session_before_fork`,
/// `session_shutdown`, and rebind/with-session callbacks).
#[derive(Default)]
pub struct RuntimeHooks<'a> {
    /// Return true to cancel the switch/fork (upstream `session_before_switch`
    /// `cancel` result).
    pub before_switch: Option<BeforeSwitchFn<'a>>,
    /// Return true to cancel a fork (upstream `session_before_fork`).
    pub before_fork: Option<BeforeForkFn<'a>>,
    /// Runs after the previous session's shutdown handlers but before the
    /// current session is invalidated.
    pub before_session_invalidate: Option<&'a mut dyn FnMut()>,
    /// Rebind host state to the new session (upstream `rebindSession`).
    pub rebind_session: Option<&'a mut dyn FnMut(&SessionManager)>,
}

/// The result of a runtime factory invocation (upstream
/// `CreateAgentSessionRuntimeResult` minus the unported session object).
pub struct RuntimeFactoryResult {
    pub services: AgentSessionServices,
    pub session_manager: SessionManager,
    pub session_start_reason: SessionStartReason,
    pub previous_session_file: Option<PathBuf>,
    pub diagnostics: Vec<AgentSessionRuntimeDiagnostic>,
}

/// The runtime factory: recreates cwd-bound services for an effective cwd
/// and session manager (upstream `CreateAgentSessionRuntimeFactory`).
pub type BeforeSwitchFn<'a> = &'a mut dyn FnMut(&str, Option<&str>) -> bool;
type BeforeForkFn<'a> = &'a mut dyn FnMut(&str, &str) -> bool;
type RuntimeFactory<'a> =
    &'a mut dyn FnMut(RuntimeFactoryInput) -> Result<RuntimeFactoryResult, String>;

/// Inputs passed to the runtime factory.
pub struct RuntimeFactoryInput {
    pub cwd: String,
    pub agent_dir: String,
    pub session_manager: SessionManager,
    pub session_start_reason: SessionStartReason,
    pub previous_session_file: Option<PathBuf>,
}

/// Outcome of a replacement flow (upstream `{ cancelled }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplacementOutcome {
    pub cancelled: bool,
}

/// Outcome of a fork (upstream `{ cancelled, selectedText? }`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForkOutcome {
    pub cancelled: bool,
    pub selected_text: Option<String>,
}

/// Owns the current services plus session manager and implements the
/// replace/switch/fork/import flows (upstream `AgentSessionRuntime`; the
/// owned AgentSession object itself is not ported yet).
pub struct AgentSessionRuntime {
    services: AgentSessionServices,
    session_manager: SessionManager,
    diagnostics: Vec<AgentSessionRuntimeDiagnostic>,
}

impl AgentSessionRuntime {
    /// Create the initial runtime (upstream `createAgentSessionRuntime`):
    /// asserts the stored cwd exists, then invokes the factory.
    pub fn create(
        factory: RuntimeFactory<'_>,
        cwd: &str,
        agent_dir: &str,
        session_manager: SessionManager,
    ) -> Result<Self, String> {
        if let Some(issue) = get_missing_session_cwd_issue(&session_manager, cwd) {
            return Err(format_missing_session_cwd_error(&issue));
        }
        let result = factory(RuntimeFactoryInput {
            cwd: cwd.to_string(),
            agent_dir: agent_dir.to_string(),
            session_manager,
            session_start_reason: SessionStartReason::Startup,
            previous_session_file: None,
        })?;
        Ok(Self {
            diagnostics: result.diagnostics,
            services: result.services,
            session_manager: result.session_manager,
        })
    }

    pub fn services(&self) -> &AgentSessionServices {
        &self.services
    }

    pub fn session_manager(&self) -> &SessionManager {
        &self.session_manager
    }

    pub fn session_manager_mut(&mut self) -> &mut SessionManager {
        &mut self.session_manager
    }

    /// Consume the runtime, yielding the bound services, session manager, and
    /// diagnostics (used by hosts that build a live session from a
    /// replacement).
    pub fn into_parts(
        self,
    ) -> (
        AgentSessionServices,
        SessionManager,
        Vec<AgentSessionRuntimeDiagnostic>,
    ) {
        (self.services, self.session_manager, self.diagnostics)
    }

    pub fn cwd(&self) -> &Path {
        &self.services.cwd
    }

    pub fn diagnostics(&self) -> &[AgentSessionRuntimeDiagnostic] {
        &self.diagnostics
    }

    fn emit_before_switch(
        hooks: &mut RuntimeHooks<'_>,
        reason: &str,
        target: Option<&str>,
    ) -> bool {
        hooks
            .before_switch
            .as_mut()
            .is_some_and(|callback| callback(reason, target))
    }

    fn emit_before_fork(hooks: &mut RuntimeHooks<'_>, entry_id: &str, position: &str) -> bool {
        hooks
            .before_fork
            .as_mut()
            .is_some_and(|callback| callback(entry_id, position))
    }

    fn finish_replacement(
        services: AgentSessionServices,
        session_manager: SessionManager,
        diagnostics: Vec<AgentSessionRuntimeDiagnostic>,
        hooks: &mut RuntimeHooks<'_>,
    ) -> Self {
        if let Some(rebind) = hooks.rebind_session.as_mut() {
            rebind(&session_manager);
        }
        Self {
            services,
            session_manager,
            diagnostics,
        }
    }

    /// Resume a persisted session file (upstream `switchSession`).
    pub fn switch_session(
        self,
        session_path: &str,
        cwd_override: Option<&str>,
        hooks: &mut RuntimeHooks<'_>,
        factory: RuntimeFactory<'_>,
    ) -> Result<(ReplacementOutcome, Self), String> {
        if Self::emit_before_switch(hooks, "resume", Some(session_path)) {
            return Ok((ReplacementOutcome { cancelled: true }, self));
        }

        let previous_session_file = self.session_manager.session_file().map(Path::to_path_buf);
        let session_manager = SessionManager::open(Path::new(session_path), None, cwd_override)?;
        if let Some(issue) =
            get_missing_session_cwd_issue(&session_manager, &self.services.cwd.to_string_lossy())
        {
            return Err(format_missing_session_cwd_error(&issue));
        }
        let target = session_manager.session_file().map(Path::to_path_buf);
        // teardownCurrent: abort + session_shutdown + invalidate + dispose.
        if let Some(callback) = hooks.before_session_invalidate.as_mut() {
            callback();
        }
        let cwd = session_manager.cwd().to_string();
        let result = factory(RuntimeFactoryInput {
            cwd,
            agent_dir: self.services.agent_dir.to_string_lossy().to_string(),
            session_manager,
            session_start_reason: SessionStartReason::Resume,
            previous_session_file: target,
        })?;
        let _ = previous_session_file;
        Ok((
            ReplacementOutcome { cancelled: false },
            Self::finish_replacement(
                result.services,
                result.session_manager,
                result.diagnostics,
                hooks,
            ),
        ))
    }

    /// Start a new session (upstream `newSession`).
    pub fn new_session(
        self,
        parent_session: Option<&str>,
        hooks: &mut RuntimeHooks<'_>,
        factory: RuntimeFactory<'_>,
    ) -> Result<(ReplacementOutcome, Self), String> {
        if Self::emit_before_switch(hooks, "new", None) {
            return Ok((ReplacementOutcome { cancelled: true }, self));
        }
        let previous_session_file = self.session_manager.session_file().map(Path::to_path_buf);
        let session_dir = self.session_manager.session_dir().to_path_buf();
        let mut session_manager = if self.session_manager.is_persisted() {
            SessionManager::create(
                &self.services.cwd.to_string_lossy(),
                Some(&session_dir),
                None,
            )?
        } else {
            SessionManager::in_memory(&self.services.cwd.to_string_lossy(), None)?
        };
        if let Some(parent) = parent_session {
            session_manager.new_session(Some(&crate::core::session_manager::NewSessionOptions {
                id: None,
                parent_session: Some(parent.to_string()),
            }));
        }

        if let Some(callback) = hooks.before_session_invalidate.as_mut() {
            callback();
        }
        let result = factory(RuntimeFactoryInput {
            cwd: self.services.cwd.to_string_lossy().to_string(),
            agent_dir: self.services.agent_dir.to_string_lossy().to_string(),
            session_manager,
            session_start_reason: SessionStartReason::New,
            previous_session_file,
        })?;
        Ok((
            ReplacementOutcome { cancelled: false },
            Self::finish_replacement(
                result.services,
                result.session_manager,
                result.diagnostics,
                hooks,
            ),
        ))
    }

    /// Fork from a previous entry (upstream `fork`).
    pub fn fork(
        self,
        entry_id: &str,
        position: &str,
        hooks: &mut RuntimeHooks<'_>,
        factory: RuntimeFactory<'_>,
    ) -> Result<(ForkOutcome, Self), String> {
        let position = if position.is_empty() {
            "before"
        } else {
            position
        };
        if Self::emit_before_fork(hooks, entry_id, position) {
            return Ok((
                ForkOutcome {
                    cancelled: true,
                    selected_text: None,
                },
                self,
            ));
        }
        let Some(selected_entry) = self.session_manager.get_entry(entry_id).cloned() else {
            return Err("Invalid entry ID for forking".to_string());
        };

        let target_leaf_id: Option<String>;
        let mut selected_text: Option<String> = None;
        if position == "at" {
            target_leaf_id = Some(selected_entry.id().to_string());
        } else {
            let message = match &selected_entry {
                crate::core::session_entries::SessionEntry::Message(message_entry) => {
                    Some(&message_entry.message)
                }
                _ => None,
            };
            let is_user_text = message
                .map(|m| {
                    matches!(
                        m,
                        crate::core::messages::CodingAgentMessage::Base(
                            pillar_ai::types::Message::User { .. }
                        )
                    )
                })
                .unwrap_or(false);
            if !is_user_text {
                return Err("Invalid entry ID for forking".to_string());
            }
            target_leaf_id = selected_entry.parent_id().map(str::to_string);
            if let Some(crate::core::messages::CodingAgentMessage::Base(
                pillar_ai::types::Message::User {
                    content: pillar_ai::types::UserContent::Text(text),
                    ..
                },
            )) = message
            {
                selected_text = Some(text.clone());
            }
        }

        let previous_session_file = self.session_manager.session_file().map(Path::to_path_buf);
        if self.session_manager.is_persisted() {
            let current_session_file =
                self.session_manager
                    .session_file()
                    .map(Path::to_path_buf)
                    .ok_or_else(|| "Persisted session is missing a session file".to_string())?;
            let session_dir = self.session_manager.session_dir().to_path_buf();
            let Some(target_leaf_id) = target_leaf_id else {
                let mut session_manager = SessionManager::create(
                    &self.services.cwd.to_string_lossy(),
                    Some(&session_dir),
                    None,
                )?;
                session_manager.new_session(Some(
                    &crate::core::session_manager::NewSessionOptions {
                        id: None,
                        parent_session: Some(current_session_file.to_string_lossy().to_string()),
                    },
                ));
                if let Some(callback) = hooks.before_session_invalidate.as_mut() {
                    callback();
                }
                let result = factory(RuntimeFactoryInput {
                    cwd: self.services.cwd.to_string_lossy().to_string(),
                    agent_dir: self.services.agent_dir.to_string_lossy().to_string(),
                    session_manager,
                    session_start_reason: SessionStartReason::Fork,
                    previous_session_file,
                })?;
                return Ok((
                    ForkOutcome {
                        cancelled: false,
                        selected_text,
                    },
                    Self::finish_replacement(
                        result.services,
                        result.session_manager,
                        result.diagnostics,
                        hooks,
                    ),
                ));
            };

            if !current_session_file.exists() {
                return Err(
                    "This session has not been saved yet. Wait for the first assistant response before cloning or forking it."
                        .to_string(),
                );
            }
            let mut session_manager =
                SessionManager::open(&current_session_file, Some(&session_dir), None)?;
            let forked_session_path = session_manager.create_branched_session(&target_leaf_id)?;
            if forked_session_path.is_none() {
                return Err("Failed to create forked session".to_string());
            }
            if let Some(callback) = hooks.before_session_invalidate.as_mut() {
                callback();
            }
            let cwd = session_manager.cwd().to_string();
            let result = factory(RuntimeFactoryInput {
                cwd,
                agent_dir: self.services.agent_dir.to_string_lossy().to_string(),
                session_manager,
                session_start_reason: SessionStartReason::Fork,
                previous_session_file,
            })?;
            return Ok((
                ForkOutcome {
                    cancelled: false,
                    selected_text,
                },
                Self::finish_replacement(
                    result.services,
                    result.session_manager,
                    result.diagnostics,
                    hooks,
                ),
            ));
        }

        // In-memory fork.
        let mut session_manager = self.session_manager;
        match target_leaf_id {
            None => {
                session_manager.new_session(Some(
                    &crate::core::session_manager::NewSessionOptions {
                        id: None,
                        parent_session: previous_session_file
                            .as_ref()
                            .map(|p| p.to_string_lossy().to_string()),
                    },
                ));
            }
            Some(leaf_id) => {
                session_manager.create_branched_session(&leaf_id)?;
            }
        }
        if let Some(callback) = hooks.before_session_invalidate.as_mut() {
            callback();
        }
        let result = factory(RuntimeFactoryInput {
            cwd: self.services.cwd.to_string_lossy().to_string(),
            agent_dir: self.services.agent_dir.to_string_lossy().to_string(),
            session_manager,
            session_start_reason: SessionStartReason::Fork,
            previous_session_file,
        })?;
        Ok((
            ForkOutcome {
                cancelled: false,
                selected_text,
            },
            Self::finish_replacement(
                result.services,
                result.session_manager,
                result.diagnostics,
                hooks,
            ),
        ))
    }

    /// Import a session JSONL file and switch to it (upstream
    /// `importFromJsonl`).
    pub fn import_from_jsonl(
        self,
        input_path: &str,
        cwd_override: Option<&str>,
        hooks: &mut RuntimeHooks<'_>,
        factory: RuntimeFactory<'_>,
    ) -> Result<(ReplacementOutcome, Self), String> {
        let resolved_path = resolve_to_cwd(input_path, "/");
        if !resolved_path.exists() {
            return Err(SessionImportFileNotFoundError {
                file_path: resolved_path.clone(),
            }
            .message());
        }

        let session_dir = self.session_manager.session_dir().to_path_buf();
        if !session_dir.exists() {
            fs::create_dir_all(&session_dir).map_err(|e| e.to_string())?;
        }

        let file_name = resolved_path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        let destination_path = session_dir.join(file_name);
        let destination_display = destination_path.to_string_lossy().to_string();
        if Self::emit_before_switch(hooks, "resume", Some(&destination_display)) {
            return Ok((ReplacementOutcome { cancelled: true }, self));
        }

        let previous_session_file = self.session_manager.session_file().map(Path::to_path_buf);
        if destination_path
            .canonicalize()
            .unwrap_or_else(|_| destination_path.clone())
            != resolved_path
        {
            fs::copy(&resolved_path, &destination_path).map_err(|e| e.to_string())?;
        }

        let session_manager =
            SessionManager::open(&destination_path, Some(&session_dir), cwd_override)?;
        if let Some(issue) =
            get_missing_session_cwd_issue(&session_manager, &self.services.cwd.to_string_lossy())
        {
            return Err(format_missing_session_cwd_error(&issue));
        }
        if let Some(callback) = hooks.before_session_invalidate.as_mut() {
            callback();
        }
        let cwd = session_manager.cwd().to_string();
        let result = factory(RuntimeFactoryInput {
            cwd,
            agent_dir: self.services.agent_dir.to_string_lossy().to_string(),
            session_manager,
            session_start_reason: SessionStartReason::Resume,
            previous_session_file,
        })?;
        let _ = previous_session_file;
        Ok((
            ReplacementOutcome { cancelled: false },
            Self::finish_replacement(
                result.services,
                result.session_manager,
                result.diagnostics,
                hooks,
            ),
        ))
    }

    /// Shut down the runtime (upstream `dispose`).
    pub fn dispose(self, hooks: &mut RuntimeHooks<'_>) {
        if let Some(callback) = hooks.before_session_invalidate.as_mut() {
            callback();
        }
    }
}

// Keep assert_session_cwd_exists referenced for API parity with upstream
// exports even though the runtime uses the issue-returning helper.
#[allow(dead_code)]
fn _assert_cwd(session_manager: &SessionManager, fallback_cwd: &str) -> Result<(), String> {
    assert_session_cwd_exists(session_manager, fallback_cwd)
}
