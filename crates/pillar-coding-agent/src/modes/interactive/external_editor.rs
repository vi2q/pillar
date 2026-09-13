//! Port of packages/coding-agent/src/modes/interactive/external-editor.ts
//! (pi v0.84.3): edit a prompt in `$EDITOR`.
//!
//! divergences: the child is spawned with inherited stdio in all cases
//! (upstream sets `shell: true` on Windows); the launch notice is written to
//! stdout exactly as upstream prints it.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::utils::text::strip_bom;

/// Inputs for [`edit_in_external_editor`] (upstream `ExternalEditorOptions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalEditorOptions {
    pub command: String,
    pub content: String,
}

/// Result of an external-editor run (upstream `ExternalEditorResult`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalEditorResult {
    Complete { content: String },
    Failed,
}

/// Run `command` on a temporary copy of `content` and read the edited result
/// (upstream `editInExternalEditor`).
pub fn edit_in_external_editor(options: &ExternalEditorOptions) -> ExternalEditorResult {
    let directory = std::env::temp_dir().join(format!("pi-editor-{}", pillar_ai::uuid::uuidv7()));
    if std::fs::create_dir_all(&directory).is_err() {
        return ExternalEditorResult::Failed;
    }
    let result = (|| {
        let file_path: PathBuf = directory.join("prompt.md");
        if std::fs::write(&file_path, &options.content).is_err() {
            return ExternalEditorResult::Failed;
        }

        let mut parts = options.command.split(' ');
        let editor = parts.next().unwrap_or_default();
        let editor_args: Vec<&str> = parts.collect();
        println!(
            "Launching external editor: {}\nPi will resume when the editor exits.",
            options.command
        );
        let _ = std::io::stdout().flush();

        let exit_code = run_editor(editor, &editor_args, &file_path);
        if exit_code != Some(0) {
            return ExternalEditorResult::Failed;
        }

        match std::fs::read_to_string(&file_path) {
            Ok(content) => ExternalEditorResult::Complete {
                content: strip_bom(&content)
                    .strip_suffix('\n')
                    .unwrap_or(strip_bom(&content))
                    .to_string(),
            },
            Err(_) => ExternalEditorResult::Failed,
        }
    })();
    let _ = std::fs::remove_dir_all(&directory);
    result
}

/// Spawn the editor with inherited stdio; `None` when it could not be started
/// (upstream the `error` event).
fn run_editor(editor: &str, editor_args: &[&str], file_path: &std::path::Path) -> Option<i32> {
    let child = Command::new(editor)
        .args(editor_args)
        .arg(file_path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .ok()?;
    let status = child.wait_with_output().ok()?.status;
    Some(status.code().unwrap_or(1))
}
