//! Port of packages/agent/src/harness/env/nodejs.ts (pi v0.84.3) —
//! `NodeExecutionEnv` becomes `StdFsExecutionEnv`: a std/tokio filesystem
//! and process-backed execution environment.
//!
//! divergence: Node errno codes map to the stable `FileErrorCode` set via
//! `std::io::ErrorKind` instead of ENOENT/EACCES string matching. Home
//! expansion supports `~` / `~/` like upstream; `file://` URLs are decoded
//! as ordinary paths when they parse, kept raw otherwise (upstream keeps
//! malformed URLs as ordinary paths too). Shell resolution looks for
//! `/bin/bash`, then `bash` on PATH, then falls back to `sh -c` (the
//! upstream Windows Git Bash search is Windows-only and not ported).
//!
//! divergence: the detached-descendant stdio grace timer
//! (`EXIT_STDIO_GRACE_MS`) is unnecessary on Unix with
//! `tokio::process::Command::output`-style collection: the port waits on
//! the child directly and kills the process group on timeout/abort, which
//! upstream implements via `killProcessTree`.

use std::io::ErrorKind as IoErrorKind;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;

use tokio::io::AsyncReadExt;

use super::types::{
    ExecutionError, ExecutionErrorCode, FileError, FileErrorCode, FileInfo, FileKind, FileSystem,
    Shell, ShellExecOptions, ShellOutput,
};

/// Maximum timeout in milliseconds (upstream `MAX_TIMEOUT_MS`), matching
/// Node's 32-bit timer limit.
const MAX_TIMEOUT_MS: u64 = 2_147_483_647;

fn resolve_timeout_seconds(timeout: Option<f64>) -> Result<Option<u64>, ExecutionError> {
    let Some(timeout) = timeout else {
        return Ok(None);
    };
    if !timeout.is_finite() || timeout <= 0.0 {
        return Err(ExecutionError::new(
            ExecutionErrorCode::Timeout,
            "Invalid timeout: must be a finite number of seconds",
        ));
    }
    let timeout_ms = timeout * 1000.0;
    if timeout_ms as u64 > MAX_TIMEOUT_MS {
        return Err(ExecutionError::new(
            ExecutionErrorCode::Timeout,
            format!(
                "Invalid timeout: maximum is {} seconds",
                MAX_TIMEOUT_MS / 1000
            ),
        ));
    }
    Ok(Some(timeout_ms as u64))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Upstream `resolvePath`: expand `~`/`~/`, decode `file://` URLs when they
/// parse, then absolutize against `cwd` and normalize.
pub fn resolve_path(cwd: &str, path: &str) -> String {
    let mut normalized = path.to_owned();
    if normalized == "~" {
        if let Some(home) = home_dir() {
            normalized = home.to_string_lossy().into_owned();
        }
    } else if let Some(rest) = normalized.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            normalized = home.join(rest).to_string_lossy().into_owned();
        }
    } else if let Some(raw) = normalized.strip_prefix("file://") {
        // Percent-decode minimal file URL; malformed URLs stay as ordinary
        // paths (upstream keeps them too).
        let decoded = raw.replace("%20", " ");
        let candidate = if decoded.starts_with('/') {
            decoded
        } else {
            format!("/{decoded}")
        };
        if Path::new(&candidate).parent().is_some() {
            normalized = candidate;
        }
    }
    let candidate = Path::new(&normalized);
    let absolute = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        Path::new(cwd).join(candidate)
    };
    normalize_absolute(&absolute)
}

/// Lexically normalize `.` and `..` segments and duplicate separators
/// (Node `path.resolve` semantics without filesystem access).
fn normalize_absolute(path: &Path) -> String {
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !parts.is_empty() {
                    parts.pop();
                }
            }
            other => parts.push(other.as_os_str().to_os_string()),
        }
    }
    let mut normalized = PathBuf::from("/");
    for part in parts {
        normalized.push(part);
    }
    normalized.to_string_lossy().into_owned()
}

fn file_error_from_io(error: &std::io::Error, fallback_path: &str) -> FileError {
    let path = Some(fallback_path.to_owned());
    match error.kind() {
        IoErrorKind::NotFound => FileError::new(FileErrorCode::NotFound, error.to_string(), path),
        IoErrorKind::PermissionDenied => {
            FileError::new(FileErrorCode::PermissionDenied, error.to_string(), path)
        }
        IoErrorKind::NotADirectory => {
            FileError::new(FileErrorCode::NotDirectory, error.to_string(), path)
        }
        IoErrorKind::IsADirectory => {
            FileError::new(FileErrorCode::IsDirectory, error.to_string(), path)
        }
        IoErrorKind::InvalidInput | IoErrorKind::InvalidData => {
            FileError::new(FileErrorCode::Invalid, error.to_string(), path)
        }
        _ => FileError::new(FileErrorCode::Unknown, error.to_string(), path),
    }
}

fn file_kind_from_metadata(metadata: &std::fs::Metadata) -> Option<FileKind> {
    let file_type = metadata.file_type();
    if file_type.is_file() {
        Some(FileKind::File)
    } else if file_type.is_dir() {
        Some(FileKind::Directory)
    } else if file_type.is_symlink() {
        Some(FileKind::Symlink)
    } else {
        None
    }
}

fn file_info_from_metadata(
    path: &str,
    metadata: &std::fs::Metadata,
) -> Result<FileInfo, FileError> {
    let Some(kind) = file_kind_from_metadata(metadata) else {
        return Err(FileError::new(
            FileErrorCode::Invalid,
            "Unsupported file type",
            Some(path.to_owned()),
        ));
    };
    let name = Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_owned());
    let mtime_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    Ok(FileInfo {
        name,
        path: path.to_owned(),
        kind,
        size: metadata.len(),
        mtime_ms,
    })
}

/// std filesystem + process execution environment (upstream
/// `NodeExecutionEnv`).
pub struct StdFsExecutionEnv {
    cwd: String,
    shell_path: Option<String>,
    shell_env: Vec<(String, String)>,
    active_child_pids: Mutex<Vec<u32>>,
}

impl StdFsExecutionEnv {
    pub fn new(cwd: &str) -> Self {
        Self {
            cwd: cwd.to_owned(),
            shell_path: None,
            shell_env: Vec::new(),
            active_child_pids: Mutex::new(Vec::new()),
        }
    }

    /// Upstream `shellPath` option: use an explicit shell binary.
    pub fn with_shell_path(mut self, shell_path: Option<String>) -> Self {
        self.shell_path = shell_path;
        self
    }

    /// Upstream `shellEnv` option: default variables for shell commands.
    pub fn with_shell_env(mut self, shell_env: Vec<(String, String)>) -> Self {
        self.shell_env = shell_env;
        self
    }

    async fn path_exists(path: &str) -> bool {
        tokio::fs::metadata(path).await.is_ok()
    }

    async fn shell_config(&self) -> Result<(String, Vec<String>), ExecutionError> {
        if let Some(custom) = &self.shell_path {
            if Self::path_exists(custom).await {
                return Ok((custom.clone(), vec!["-c".to_owned()]));
            }
            return Err(ExecutionError::new(
                ExecutionErrorCode::ShellUnavailable,
                format!("Custom shell path not found: {custom}"),
            ));
        }
        if Self::path_exists("/bin/bash").await {
            return Ok(("/bin/bash".to_owned(), vec!["-c".to_owned()]));
        }
        if let Some(found) = Self::find_bash_on_path().await {
            return Ok((found, vec!["-c".to_owned()]));
        }
        Ok(("sh".to_owned(), vec!["-c".to_owned()]))
    }

    async fn find_bash_on_path() -> Option<String> {
        let path_var = std::env::var("PATH").unwrap_or_default();
        for dir in path_var.split(':') {
            if dir.is_empty() {
                continue;
            }
            let candidate = Path::new(dir).join("bash");
            if tokio::fs::metadata(&candidate).await.is_ok() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
        None
    }

    fn build_shell_env(&self, options: Option<&ShellExecOptions>) -> Vec<(String, String)> {
        let Some(options) = options else {
            return self.shell_env.clone();
        };
        // Upstream default is `inheritEnv ?? true`.
        if !options.inherit_env.unwrap_or(true) {
            // Upstream `{ ...extraEnv }` without base: only overrides.
            return options.env.clone();
        }
        // Upstream: { ...process.env, ...baseEnv, ...extraEnv }.
        let mut merged: Vec<(String, String)> = std::env::vars().collect();
        for (key, value) in &self.shell_env {
            merged.retain(|(k, _)| k != key);
            merged.push((key.clone(), value.clone()));
        }
        for (key, value) in &options.env {
            merged.retain(|(k, _)| k != key);
            merged.push((key.clone(), value.clone()));
        }
        merged
    }
}

impl FileSystem for StdFsExecutionEnv {
    fn cwd(&self) -> &str {
        &self.cwd
    }

    async fn absolute_path(&self, path: &str) -> Result<String, FileError> {
        Ok(resolve_path(&self.cwd, path))
    }

    async fn join_path(&self, parts: &[&str]) -> Result<String, FileError> {
        let mut joined = PathBuf::new();
        for part in parts {
            joined.push(part);
        }
        Ok(joined.to_string_lossy().into_owned())
    }

    async fn read_text_file(&self, path: &str) -> Result<String, FileError> {
        let resolved = resolve_path(&self.cwd, path);
        tokio::fs::read_to_string(&resolved)
            .await
            .map_err(|error| file_error_from_io(&error, &resolved))
    }

    async fn read_text_lines(
        &self,
        path: &str,
        max_lines: Option<usize>,
    ) -> Result<Vec<String>, FileError> {
        let resolved = resolve_path(&self.cwd, path);
        if let Some(max_lines) = max_lines {
            if max_lines == 0 {
                return Ok(Vec::new());
            }
        }
        let content = tokio::fs::read_to_string(&resolved)
            .await
            .map_err(|error| file_error_from_io(&error, &resolved))?;
        let mut lines: Vec<String> = Vec::new();
        for line in content.split('\n') {
            lines.push(line.to_owned());
            if let Some(max_lines) = max_lines {
                if lines.len() >= max_lines {
                    break;
                }
            }
        }
        Ok(lines)
    }

    async fn read_binary_file(&self, path: &str) -> Result<Vec<u8>, FileError> {
        let resolved = resolve_path(&self.cwd, path);
        tokio::fs::read(&resolved)
            .await
            .map_err(|error| file_error_from_io(&error, &resolved))
    }

    async fn write_file(&self, path: &str, content: &[u8]) -> Result<(), FileError> {
        let resolved = resolve_path(&self.cwd, path);
        if let Some(parent) = Path::new(&resolved).parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| file_error_from_io(&error, &resolved))?;
        }
        tokio::fs::write(&resolved, content)
            .await
            .map_err(|error| file_error_from_io(&error, &resolved))
    }

    async fn append_file(&self, path: &str, content: &[u8]) -> Result<(), FileError> {
        let resolved = resolve_path(&self.cwd, path);
        if let Some(parent) = Path::new(&resolved).parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| file_error_from_io(&error, &resolved))?;
        }
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&resolved)
            .await
            .map_err(|error| file_error_from_io(&error, &resolved))?;
        file.write_all(content)
            .await
            .map_err(|error| file_error_from_io(&error, &resolved))
    }

    async fn rename_file(
        &self,
        source_path: &str,
        destination_path: &str,
    ) -> Result<(), FileError> {
        let source = resolve_path(&self.cwd, source_path);
        let destination = resolve_path(&self.cwd, destination_path);
        tokio::fs::rename(&source, &destination)
            .await
            .map_err(|error| file_error_from_io(&error, &source))
    }

    async fn file_info(&self, path: &str) -> Result<FileInfo, FileError> {
        let resolved = resolve_path(&self.cwd, path);
        let metadata = tokio::fs::symlink_metadata(&resolved)
            .await
            .map_err(|error| file_error_from_io(&error, &resolved))?;
        file_info_from_metadata(&resolved, &metadata)
    }

    async fn list_dir(&self, path: &str) -> Result<Vec<FileInfo>, FileError> {
        let resolved = resolve_path(&self.cwd, path);
        let mut entries = tokio::fs::read_dir(&resolved)
            .await
            .map_err(|error| file_error_from_io(&error, &resolved))?;
        let mut infos = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|error| file_error_from_io(&error, &resolved))?
        {
            let entry_path = entry.path();
            let entry_path = normalize_absolute(&entry_path);
            let metadata = tokio::fs::symlink_metadata(&entry_path)
                .await
                .map_err(|error| file_error_from_io(&error, &entry_path))?;
            infos.push(file_info_from_metadata(&entry_path, &metadata)?);
        }
        Ok(infos)
    }

    async fn canonical_path(&self, path: &str) -> Result<String, FileError> {
        let resolved = resolve_path(&self.cwd, path);
        tokio::fs::canonicalize(&resolved)
            .await
            .map(|canonical| canonical.to_string_lossy().into_owned())
            .map_err(|error| file_error_from_io(&error, &resolved))
    }

    async fn exists(&self, path: &str) -> Result<bool, FileError> {
        match self.file_info(path).await {
            Ok(_) => Ok(true),
            Err(error) if error.code == FileErrorCode::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    async fn create_dir(&self, path: &str, recursive: bool) -> Result<(), FileError> {
        let resolved = resolve_path(&self.cwd, path);
        let result = if recursive {
            tokio::fs::create_dir_all(&resolved).await
        } else {
            tokio::fs::create_dir(&resolved).await
        };
        result.map_err(|error| file_error_from_io(&error, &resolved))
    }

    async fn remove(&self, path: &str, recursive: bool, force: bool) -> Result<(), FileError> {
        let resolved = resolve_path(&self.cwd, path);
        let result = if recursive {
            tokio::fs::remove_dir_all(&resolved).await
        } else {
            match tokio::fs::remove_dir(&resolved).await {
                Ok(()) => Ok(()),
                Err(error)
                    if error.kind() == IoErrorKind::NotADirectory
                        || error.kind() == IoErrorKind::InvalidInput =>
                {
                    tokio::fs::remove_file(&resolved).await
                }
                Err(error) => Err(error),
            }
        };
        result
            .map_err(|error| file_error_from_io(&error, &resolved))
            .or_else(|error| {
                // Upstream `rm({ force: true })`: not_found errors are
                // swallowed and reported as success.
                if force && error.code == FileErrorCode::NotFound {
                    Ok(())
                } else {
                    Err(error)
                }
            })
    }

    async fn create_temp_dir(&self, prefix: &str) -> Result<String, FileError> {
        let base = std::env::temp_dir().join(format!("{prefix}{}", uuid_like()));
        tokio::fs::create_dir_all(&base)
            .await
            .map_err(|error| file_error_from_io(&error, &base.to_string_lossy()))?;
        Ok(base.to_string_lossy().into_owned())
    }

    async fn create_temp_file(&self, prefix: &str, suffix: &str) -> Result<String, FileError> {
        let dir = self.create_temp_dir("tmp-").await?;
        let file_path = Path::new(&dir).join(format!("{prefix}{}{suffix}", uuid_like()));
        let file_path = file_path.to_string_lossy().into_owned();
        tokio::fs::write(&file_path, b"")
            .await
            .map_err(|error| file_error_from_io(&error, &file_path))?;
        Ok(file_path)
    }

    async fn cleanup(&self) {
        let pids: Vec<u32> = self
            .active_child_pids
            .lock()
            .expect("active child pids lock")
            .drain(..)
            .collect();
        for pid in pids {
            kill_process_tree(pid);
        }
    }
}

impl Shell for StdFsExecutionEnv {
    async fn exec(
        &self,
        command: &str,
        options: Option<ShellExecOptions>,
    ) -> Result<ShellOutput, ExecutionError> {
        if let Some(options) = &options {
            if let Some(signal) = &options.abort_signal {
                if signal.is_aborted() {
                    return Err(ExecutionError::new(ExecutionErrorCode::Aborted, "aborted"));
                }
            }
        }
        let timeout_ms = resolve_timeout_seconds(options.as_ref().and_then(|o| o.timeout))?;

        let cwd = match options.as_ref().and_then(|o| o.cwd.clone()) {
            Some(cwd) => resolve_path(&self.cwd, &cwd),
            None => self.cwd.clone(),
        };
        let (shell, args) = self.shell_config().await?;
        if !Self::path_exists(&cwd).await {
            return Err(ExecutionError::new(
                ExecutionErrorCode::SpawnError,
                format!("Working directory does not exist: {cwd}\nCannot execute bash commands."),
            ));
        }

        let mut cmd = tokio::process::Command::new(&shell);
        cmd.args(&args)
            .arg(command)
            .current_dir(&cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());
        #[cfg(unix)]
        {
            // Group-kill support: the whole tree dies on timeout/abort
            // (upstream killProcessTree via `process.kill(-pid)`).
            cmd.process_group(0);
        }
        for (key, value) in self.build_shell_env(options.as_ref()) {
            cmd.env(key, value);
        }
        // `inherit_env: false` replaces the environment (upstream `{
        // ...extraEnv }` without spreading process.env). tokio Command
        // inherits the parent env by default, so clear it first; the
        // merged env (or just the overrides) was applied above.
        if matches!(options.as_ref().and_then(|o| o.inherit_env), Some(false)) {
            cmd.env_clear();
            for (key, value) in self.build_shell_env(options.as_ref()) {
                cmd.env(key, value);
            }
        }

        let mut child = cmd.spawn().map_err(|error| {
            ExecutionError::new(ExecutionErrorCode::SpawnError, error.to_string())
        })?;
        let child_pid = child.id();
        if let Some(pid) = child_pid {
            self.active_child_pids
                .lock()
                .expect("active child pids lock")
                .push(pid);
        }

        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();

        // Read both pipes to completion while streaming chunks to the
        // callbacks (upstream on('data')). Callback failures abort the
        // command via the process group kill (upstream callbackError +
        // onAbort).
        let stdout_callback = options.as_ref().and_then(|o| o.on_stdout.clone());
        let stderr_callback = options.as_ref().and_then(|o| o.on_stderr.clone());

        let abort_signal = options.as_ref().and_then(|o| o.abort_signal.clone());
        let timed_out = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Drive pipe reads and the child wait concurrently; timeout and
        // abort race against completion (upstream setTimeout + abort
        // listener + close event). Pipe reads run in detached tasks that
        // append into shared buffers, so the wait future owns nothing but
        // the child handle (borrow conflicts otherwise).
        let buffers =
            std::sync::Arc::new(std::sync::Mutex::new((Vec::<u8>::new(), Vec::<u8>::new())));
        let buffers_stdout = std::sync::Arc::clone(&buffers);
        let buffers_stderr = std::sync::Arc::clone(&buffers);
        let stdout_task = tokio::spawn(async move {
            if let Some(mut pipe) = stdout_pipe {
                let mut chunk = [0u8; 8192];
                loop {
                    match pipe.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            buffers_stdout
                                .lock()
                                .expect("stdout buffer lock")
                                .0
                                .extend_from_slice(&chunk[..n]);
                            if let Some(callback) = &stdout_callback {
                                if let Ok(text) = std::str::from_utf8(&chunk[..n]) {
                                    callback(text);
                                }
                            }
                        }
                    }
                }
            }
        });
        let stderr_task = tokio::spawn(async move {
            if let Some(mut pipe) = stderr_pipe {
                let mut chunk = [0u8; 8192];
                loop {
                    match pipe.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            buffers_stderr
                                .lock()
                                .expect("stderr buffer lock")
                                .1
                                .extend_from_slice(&chunk[..n]);
                            if let Some(callback) = &stderr_callback {
                                if let Ok(text) = std::str::from_utf8(&chunk[..n]) {
                                    callback(text);
                                }
                            }
                        }
                    }
                }
            }
        });

        enum WaitOutcome {
            Exited(std::io::Result<std::process::ExitStatus>),
            Killed,
        }

        let wait = async { child.wait().await };
        tokio::pin!(wait);

        let mut killed = false;
        let outcome = tokio::select! {
            status = &mut wait => WaitOutcome::Exited(status),
            _ = async {
                match timeout_ms {
                    Some(timeout_ms) => tokio::time::sleep(tokio::time::Duration::from_millis(timeout_ms)).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                timed_out.store(true, std::sync::atomic::Ordering::SeqCst);
                if let Some(pid) = child_pid {
                    kill_process_tree(pid);
                }
                killed = true;
                WaitOutcome::Killed
            }
            _ = async {
                match &abort_signal {
                    Some(signal) => signal.aborted().await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                if let Some(pid) = child_pid {
                    kill_process_tree(pid);
                }
                killed = true;
                WaitOutcome::Killed
            }
        };

        // Wait for pipes to reach EOF (the kill above closes them) so
        // buffers are complete, then reap the child.
        let _ = stdout_task.await;
        let _ = stderr_task.await;
        if killed {
            let _ = (&mut wait).await;
        }

        if let Some(pid) = child_pid {
            self.active_child_pids
                .lock()
                .expect("active child pids lock")
                .retain(|p| *p != pid);
        }

        if timed_out.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(ExecutionError::new(
                ExecutionErrorCode::Timeout,
                format!(
                    "timeout:{}",
                    options.as_ref().and_then(|o| o.timeout).unwrap_or_default()
                ),
            ));
        }
        if abort_signal.as_ref().is_some_and(|s| s.is_aborted()) {
            return Err(ExecutionError::new(ExecutionErrorCode::Aborted, "aborted"));
        }

        let (stdout_raw, stderr_raw) = {
            let guard = buffers.lock().expect("output buffer lock");
            (guard.0.clone(), guard.1.clone())
        };
        let stdout_text = String::from_utf8_lossy(&stdout_raw).into_owned();
        let stderr_text = String::from_utf8_lossy(&stderr_raw).into_owned();
        let status = match outcome {
            WaitOutcome::Exited(status) => status,
            WaitOutcome::Killed => unreachable!("killed outcomes returned above"),
        };
        let exit_code = status
            .map_err(|error| {
                ExecutionError::new(ExecutionErrorCode::SpawnError, error.to_string())
            })?
            .code()
            .unwrap_or(0);
        Ok(ShellOutput {
            stdout: stdout_text,
            stderr: stderr_text,
            exit_code,
        })
    }

    async fn cleanup(&self) {
        FileSystem::cleanup(self).await;
    }
}

fn kill_process_tree(pid: u32) {
    // The port spawns with its own process group (upstream detached +
    // `process.kill(-pid)`); kill the group via the external kill binary
    // (no libc dependency).
    let _ = std::process::Command::new("kill")
        .args(["-9", &format!("-{pid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Random suffix for temp entries (upstream randomUUID / mkdtemp).
pub fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}{:04x}", nanos, (nanos % 0xffff) as u16)
}

/// Unique-per-call suffix for integration test temp roots.
pub fn test_suffix() -> String {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    format!("{}-{n}", uuid_like())
}
