//! The host's effect broker: authorizes and executes the effects extensions
//! ask for, and records them.
//!
//! This module owns the process and filesystem calls of the host callbacks.
//! `runner.rs` must not touch them directly — a path that bypasses the broker
//! would silently skip both the policy and the audit trail, and
//! `tests/extension_safety_parity.rs` fails when one appears
//! (docs/ARCHITECTURE-REVIEW-s05c0.md 0).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pillar_coding_agent::core::effects::{
    EffectAuthorizer, EffectDecision, EffectIntent, allow_all,
};

/// Authorizes execution of extension effects and keeps an audit trail of
/// every intent it saw.
pub struct EffectBroker {
    authorizer: EffectAuthorizer,
    audit: Mutex<Vec<EffectIntent>>,
}

impl EffectBroker {
    /// Allow every effect: the project trust gate already refused untrusted
    /// extensions, so what is loaded is the user's own code.
    pub fn permissive() -> Arc<Self> {
        Self::new(allow_all())
    }

    pub fn new(authorizer: EffectAuthorizer) -> Arc<Self> {
        Arc::new(Self {
            authorizer,
            audit: Mutex::new(Vec::new()),
        })
    }

    /// The gate the session's tool-call path uses. Sharing the broker keeps
    /// the policy and the audit trail identical across tool / `exec` / `fs`.
    pub fn authorizer(self: &Arc<Self>) -> EffectAuthorizer {
        let broker = Arc::clone(self);
        Arc::new(move |intent: &EffectIntent| broker.authorize(intent))
    }

    /// The intents seen so far, in order (diagnostics and tests).
    pub fn audit(&self) -> Vec<EffectIntent> {
        self.audit
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn authorize(&self, intent: &EffectIntent) -> EffectDecision {
        self.audit
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(intent.clone());
        (self.authorizer)(intent)
    }

    /// `pi.exec`: authorize, then run the command with the session's cwd. A
    /// denial answers the same shape as a failed run, so an extension cannot
    /// tell the policy from a missing binary. The command runs through
    /// `core::exec`, so `signal` / `timeout` cancel it (upstream
    /// `execCommand`).
    pub fn exec(
        &self,
        cwd: &str,
        command: &str,
        args: &[String],
        options: &pillar_coding_agent::core::exec::ExecOptions,
    ) -> pillar_coding_agent::core::exec::ExecResult {
        let intent = EffectIntent::Exec {
            command: command.to_string(),
            args: args.to_vec(),
        };
        if let EffectDecision::Deny { reason } = self.authorize(&intent) {
            return pillar_coding_agent::core::exec::ExecResult::spawn_failure(format!(
                "pi.exec denied: {reason}"
            ));
        }
        let resolved_cwd = match options.cwd.as_deref() {
            Some(override_cwd) if !override_cwd.is_empty() => {
                let path = Path::new(override_cwd);
                if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    Path::new(cwd).join(path)
                }
            }
            _ => PathBuf::from(cwd),
        };
        pillar_coding_agent::core::exec::exec_command(
            command,
            args,
            &resolved_cwd.to_string_lossy(),
            options,
        )
    }

    /// `pillar.fs.*`: authorize, then act. Paths resolve against the session
    /// cwd before authorization, like tool calls.
    pub fn fs(
        &self,
        cwd: &str,
        op: &str,
        path: &str,
        content: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        let resolved = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            Path::new(cwd).join(path)
        };
        let path = resolved.to_string_lossy().to_string();
        let intent = match op {
            "read" => EffectIntent::FsRead { path: path.clone() },
            "write" => EffectIntent::FsWrite { path: path.clone() },
            "list" => EffectIntent::FsList { path: path.clone() },
            "stat" | "exists" => EffectIntent::FsStat { path: path.clone() },
            other => return Err(format!("pillar.fs: unknown operation {other}")),
        };
        if let EffectDecision::Deny { reason } = self.authorize(&intent) {
            return Err(format!("pillar.fs.{op} denied: {reason}"));
        }
        let resolved = Path::new(&path);
        match op {
            "read" => match std::fs::read_to_string(resolved) {
                Ok(text) => Ok(serde_json::Value::String(text)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    Ok(serde_json::Value::Null)
                }
                Err(error) => Err(format!("pillar.fs.read: {error}")),
            },
            "write" => {
                let Some(content) = content else {
                    return Err("pillar.fs.write: missing content".to_string());
                };
                if let Some(parent) = resolved.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(resolved, content)
                    .map(|_| serde_json::Value::Bool(true))
                    .map_err(|error| format!("pillar.fs.write: {error}"))
            }
            "list" => {
                let entries = std::fs::read_dir(resolved)
                    .map_err(|error| format!("pillar.fs.list: {error}"))?;
                let mut names: Vec<String> = entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.file_name().to_string_lossy().to_string())
                    .collect();
                names.sort();
                Ok(serde_json::Value::Array(
                    names.into_iter().map(serde_json::Value::String).collect(),
                ))
            }
            "stat" => match std::fs::metadata(resolved) {
                Ok(metadata) => {
                    let modified_ms = metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|duration| duration.as_millis() as u64)
                        .unwrap_or(0);
                    Ok(serde_json::json!({
                        "type": if metadata.is_dir() { "directory" } else { "file" },
                        "size": metadata.len(),
                        "modified_ms": modified_ms,
                    }))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    Ok(serde_json::Value::Null)
                }
                Err(error) => Err(format!("pillar.fs.stat: {error}")),
            },
            "exists" => Ok(serde_json::Value::Bool(resolved.exists())),
            other => Err(format!("pillar.fs: unknown operation {other}")),
        }
    }
}
