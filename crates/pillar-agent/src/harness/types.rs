//! Port of packages/agent/src/harness/types.ts (pi v0.84.3) — shared
//! harness types: skills, prompt templates, and the execution environment
//! capability traits.
//!
//! divergence: upstream `Result<TValue, TError>` (an `{ok, value|error}`
//! object union) maps to `std::result::Result<TValue, FileError>` /
//! `ExecutionError` in Rust. `FileError`/`ExecutionError` classes map to
//! structs with stable `code` fields, matching the upstream public shape.

use std::future::Future;

/// Skill loaded from a `SKILL.md` file or provided by an application.
///
/// `name`, `description`, and `file_path` are inserted into the system
/// prompt in an XML-formatted block as suggested by agentskills.io. Use
/// [`format_skills_for_system_prompt`](super::system_prompt::format_skills_for_system_prompt)
/// to generate the block.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Skill {
    /// Stable skill name used for lookup and model-visible listings.
    pub name: String,
    /// Short model-visible description of when to use the skill.
    pub description: String,
    /// Full skill instructions.
    pub content: String,
    /// Path to the skill file. Used for model-visible location and
    /// resolving relative references.
    pub file_path: String,
    /// Exclude this skill from model-visible skill lists while still
    /// allowing explicit application invocation.
    pub disable_model_invocation: bool,
}

/// Prompt template that can be formatted into a prompt for explicit
/// invocation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PromptTemplate {
    /// Stable template name used for lookup or command routing.
    pub name: String,
    /// Optional description for command lists or autocomplete.
    pub description: String,
    /// Template content. Placeholders are substituted by
    /// [`format_prompt_template_invocation`](super::prompt_templates::format_prompt_template_invocation).
    pub content: String,
}

/// Metadata for one filesystem object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    /// Basename of [`path`](Self::path).
    pub name: String,
    /// Absolute addressed path in the execution environment. Symlinks are
    /// not followed.
    pub path: String,
    /// Object kind. Symlink targets are not followed; use
    /// [`FileSystem::canonical_path`] explicitly.
    pub kind: FileKind,
    /// Size in bytes.
    pub size: u64,
    /// Modification time as milliseconds since Unix epoch.
    pub mtime_ms: u64,
}

/// Kind of filesystem object. Symlinks are not followed automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Directory,
    Symlink,
}

/// Stable, backend-independent file error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileErrorCode {
    Aborted,
    NotFound,
    PermissionDenied,
    NotDirectory,
    IsDirectory,
    Invalid,
    NotSupported,
    Unknown,
}

impl FileErrorCode {
    /// Upstream snake_case code string.
    pub fn as_str(&self) -> &'static str {
        match self {
            FileErrorCode::Aborted => "aborted",
            FileErrorCode::NotFound => "not_found",
            FileErrorCode::PermissionDenied => "permission_denied",
            FileErrorCode::NotDirectory => "not_directory",
            FileErrorCode::IsDirectory => "is_directory",
            FileErrorCode::Invalid => "invalid",
            FileErrorCode::NotSupported => "not_supported",
            FileErrorCode::Unknown => "unknown",
        }
    }
}

/// Error returned by [`FileSystem`] file operations. The `Display` message
/// matches the upstream error message text.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct FileError {
    /// Backend-independent error code.
    pub code: FileErrorCode,
    /// Human-readable message.
    pub message: String,
    /// Absolute addressed path associated with the failure, when available.
    pub path: Option<String>,
}

impl FileError {
    pub fn new(code: FileErrorCode, message: impl Into<String>, path: Option<String>) -> Self {
        Self {
            code,
            message: message.into(),
            path,
        }
    }

    /// Upstream `abortResult` helper: the pre-aborted error shape.
    pub fn aborted(path: Option<String>) -> Self {
        Self::new(FileErrorCode::Aborted, "aborted", path)
    }
}

/// The harness tools' own error channel is
/// [`ToolExecuteError`](crate::types::ToolExecuteError); letting the mutation
/// queue return it directly means a queue-registration failure surfaces as a
/// tool failure with the `FileError` message unchanged.
impl From<FileError> for crate::types::ToolExecuteError {
    fn from(error: FileError) -> Self {
        Self(error.to_string())
    }
}

/// Stable, backend-independent execution error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionErrorCode {
    Aborted,
    Timeout,
    ShellUnavailable,
    SpawnError,
    CallbackError,
    Unknown,
}

impl ExecutionErrorCode {
    /// Upstream snake_case code string.
    pub fn as_str(&self) -> &'static str {
        match self {
            ExecutionErrorCode::Aborted => "aborted",
            ExecutionErrorCode::Timeout => "timeout",
            ExecutionErrorCode::ShellUnavailable => "shell_unavailable",
            ExecutionErrorCode::SpawnError => "spawn_error",
            ExecutionErrorCode::CallbackError => "callback_error",
            ExecutionErrorCode::Unknown => "unknown",
        }
    }
}

/// Error returned by [`Shell::exec`].
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ExecutionError {
    /// Backend-independent error code.
    pub code: ExecutionErrorCode,
    /// Human-readable message.
    pub message: String,
}

impl ExecutionError {
    pub fn new(code: ExecutionErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Chunk callback shared by exec stream handlers.
pub type StreamCallback = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// Options for [`Shell::exec`].
#[derive(Clone, Default)]
pub struct ShellExecOptions {
    /// Working directory for the command. Relative paths resolve against
    /// the environment's cwd. Defaults to the environment cwd.
    pub cwd: Option<String>,
    /// Environment variables for the command. Values override inherited
    /// defaults when `inherit_env` is true.
    pub env: Vec<(String, String)>,
    /// Whether to inherit the environment's default variables. Default true
    /// (upstream `inheritEnv ?? true`).
    pub inherit_env: Option<bool>,
    /// Timeout in seconds. `None` = no timeout.
    pub timeout: Option<f64>,
    /// Abort signal used to terminate the command. `None` = no abort.
    pub abort_signal: Option<crate::abort::AbortSignal>,
    /// Called with stdout chunks as they are produced.
    pub on_stdout: Option<StreamCallback>,
    /// Called with stderr chunks as they are produced.
    pub on_stderr: Option<StreamCallback>,
}

impl std::fmt::Debug for ShellExecOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellExecOptions")
            .field("cwd", &self.cwd)
            .field("env", &self.env)
            .field("inherit_env", &self.inherit_env)
            .field("timeout", &self.timeout)
            .field("on_stdout", &self.on_stdout.is_some())
            .field("on_stderr", &self.on_stderr.is_some())
            .finish()
    }
}

/// Result of a successful shell execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Filesystem capability used by the harness.
///
/// Paths passed to methods may be absolute or relative to
/// [`cwd`](FileSystem::cwd). Operation methods must never panic; all
/// failures, including unexpected backend failures, are encoded in the
/// returned `Result` (upstream non-throwing contract).
pub trait FileSystem: Send + Sync {
    /// Current working directory for relative paths.
    fn cwd(&self) -> &str;

    /// Return an absolute addressed path without requiring it to exist and
    /// without resolving symlinks.
    fn absolute_path(&self, path: &str) -> impl Future<Output = Result<String, FileError>> + Send;

    /// Join path segments in the filesystem namespace without requiring the
    /// result to exist.
    fn join_path(&self, parts: &[&str]) -> impl Future<Output = Result<String, FileError>> + Send;

    /// Read a UTF-8 text file.
    fn read_text_file(&self, path: &str) -> impl Future<Output = Result<String, FileError>> + Send;

    /// Read UTF-8 text lines, stopping once `max_lines` lines are read.
    fn read_text_lines(
        &self,
        path: &str,
        max_lines: Option<usize>,
    ) -> impl Future<Output = Result<Vec<String>, FileError>> + Send;

    /// Read a binary file.
    fn read_binary_file(
        &self,
        path: &str,
    ) -> impl Future<Output = Result<Vec<u8>, FileError>> + Send;

    /// Create or overwrite a file, creating parent directories when
    /// supported.
    fn write_file(
        &self,
        path: &str,
        content: &[u8],
    ) -> impl Future<Output = Result<(), FileError>> + Send;

    /// Create or append to a file, creating parent directories when
    /// supported.
    fn append_file(
        &self,
        path: &str,
        content: &[u8],
    ) -> impl Future<Output = Result<(), FileError>> + Send;

    /// Atomically rename a file, replacing the destination when it exists.
    fn rename_file(
        &self,
        source_path: &str,
        destination_path: &str,
    ) -> impl Future<Output = Result<(), FileError>> + Send;

    /// Return metadata for the addressed path without following symlinks.
    fn file_info(&self, path: &str) -> impl Future<Output = Result<FileInfo, FileError>> + Send;

    /// List direct children of a directory without following symlinks.
    fn list_dir(&self, path: &str)
    -> impl Future<Output = Result<Vec<FileInfo>, FileError>> + Send;

    /// Return the canonical path for an existing path, resolving symlinks
    /// where supported.
    fn canonical_path(&self, path: &str) -> impl Future<Output = Result<String, FileError>> + Send;

    /// Return false for missing paths. Other errors return a `FileError`.
    fn exists(&self, path: &str) -> impl Future<Output = Result<bool, FileError>> + Send;

    /// Create a directory. Defaults: `recursive: true`.
    fn create_dir(
        &self,
        path: &str,
        recursive: bool,
    ) -> impl Future<Output = Result<(), FileError>> + Send;

    /// Remove a file or directory. Defaults: `recursive: false`,
    /// `force: false`.
    fn remove(
        &self,
        path: &str,
        recursive: bool,
        force: bool,
    ) -> impl Future<Output = Result<(), FileError>> + Send;

    /// Create a temporary directory and return its absolute path.
    /// Defaults: `prefix: "tmp-"`.
    fn create_temp_dir(
        &self,
        prefix: &str,
    ) -> impl Future<Output = Result<String, FileError>> + Send;

    /// Create a temporary file and return its absolute path.
    fn create_temp_file(
        &self,
        prefix: &str,
        suffix: &str,
    ) -> impl Future<Output = Result<String, FileError>> + Send;

    /// Release filesystem resources. Must be best-effort and must not
    /// panic.
    fn cleanup(&self) -> impl Future<Output = ()> + Send;
}

/// Shell execution capability used by the harness.
pub trait Shell: Send + Sync {
    /// Execute a shell command in the filesystem cwd unless
    /// `options.cwd` is provided.
    fn exec(
        &self,
        command: &str,
        options: Option<ShellExecOptions>,
    ) -> impl Future<Output = Result<ShellOutput, ExecutionError>> + Send;

    /// Release shell resources. Must be best-effort and must not panic.
    fn cleanup(&self) -> impl Future<Output = ()> + Send;
}

/// Filesystem and process execution environment used by the harness
/// (upstream `interface ExecutionEnv extends FileSystem, Shell {}`).
pub trait ExecutionEnv: FileSystem + Shell {}
impl<T: FileSystem + Shell> ExecutionEnv for T {}
