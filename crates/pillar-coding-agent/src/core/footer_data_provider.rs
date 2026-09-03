//! Port of packages/coding-agent/src/core/footer-data-provider.ts (pi
//! v0.84.3), the resolution core: git branch reading from HEAD (regular
//! repos, worktrees with gitdir pointers, detached HEAD, reftable
//! `.invalid` fallback via git), and the status bookkeeping the footer
//! needs.
//!
//! divergences: file watching (fs.watch / watchFile / reftable watchers
//! and their retry timers) is a host-event-loop concern; the port covers
//! the synchronous resolution and the status map/count bookkeeping, with
//! branch-change callbacks surfaced for the host to drive.

use std::collections::BTreeMap;
use std::process::{Command, Stdio};

use crate::core::resource_loader::GitPaths;

/// Watch debounce interval (upstream `WATCH_DEBOUNCE_MS`).
pub const WATCH_DEBOUNCE_MS: u64 = 500;

/// Whether HEAD polling is needed (upstream `shouldPollGitHead`): WSL
/// accessing a repo on a Windows-mounted path (`/mnt/<drive>/...`).
pub fn should_poll_git_head(repo_dir: &std::path::Path, is_wsl: bool) -> bool {
    is_wsl && is_windows_mounted_repo_path(repo_dir)
}

/// Whether the path is a Windows-mounted drive under WSL (upstream
/// `isWindowsMountedRepoPath`).
pub fn is_windows_mounted_repo_path(repo_dir: &std::path::Path) -> bool {
    let path = repo_dir.to_string_lossy();
    let mut chars = path.chars();
    let matches = path.starts_with("/mnt/")
        && path[5..]
            .chars()
            .next()
            .map(|c| c.is_ascii_alphabetic())
            .unwrap_or(false)
        && {
            let rest = &path[6..];
            rest.is_empty() || rest.starts_with('/')
        };
    chars.next();
    matches
}

/// Whether this is a WSL environment (upstream `isWslEnvironment`): linux
/// with WSL_DISTRO_NAME or WSL_INTEROP set.
pub fn is_wsl_environment(
    is_linux: bool,
    wsl_distro_name: Option<&str>,
    wsl_interop: Option<&str>,
) -> bool {
    is_linux && (wsl_distro_name.is_some() || wsl_interop.is_some())
}

/// Ask git for the current branch (upstream `resolveBranchWithGitSync`):
/// `git --no-optional-locks symbolic-ref --quiet --short HEAD`; null on
/// detached HEAD or when git is unavailable.
pub fn resolve_branch_with_git(repo_dir: &std::path::Path) -> Option<String> {
    let output = Command::new("git")
        .args([
            "--no-optional-locks",
            "symbolic-ref",
            "--quiet",
            "--short",
            "HEAD",
        ])
        .current_dir(repo_dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if output.status.success() {
        let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if branch.is_empty() {
            None
        } else {
            Some(branch)
        }
    } else {
        None
    }
}

/// Read the branch from a HEAD file (upstream the resolveGitBranch
/// bodies): `ref: refs/heads/<branch>` yields the branch (with the
/// `.invalid` reftable marker falling back to git); anything else is a
/// detached HEAD; unreadable paths yield None.
pub fn resolve_git_branch(git_paths: &GitPaths) -> Option<String> {
    let content = std::fs::read_to_string(&git_paths.head_path).ok()?;
    let content = content.trim();
    if let Some(branch) = content.strip_prefix("ref: refs/heads/") {
        if branch == ".invalid" {
            return Some(
                resolve_branch_with_git(&git_paths.repo_dir)
                    .unwrap_or_else(|| "detached".to_string()),
            );
        }
        return Some(branch.to_string());
    }
    Some("detached".to_string())
}

/// Footer state bookkeeping (upstream `FooterDataProvider`, minus file
/// watchers).
#[derive(Default)]
pub struct FooterDataProvider {
    extension_statuses: BTreeMap<String, String>,
    cached_branch: Option<Option<String>>,
    available_provider_count: usize,
    branch_change_callbacks: Vec<Box<dyn Fn() + Send>>,
}

impl FooterDataProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// Current git branch; None when not in a repo, "detached" for
    /// detached HEAD (upstream `getGitBranch`).
    pub fn git_branch(&mut self, git_paths: Option<&GitPaths>) -> Option<String> {
        if self.cached_branch.is_none() {
            self.cached_branch = Some(git_paths.and_then(resolve_git_branch));
        }
        self.cached_branch.clone().flatten()
    }

    /// Invalidate the cached branch (host-side after watchers fire).
    pub fn invalidate_branch(&mut self) {
        self.cached_branch = None;
    }

    /// Extension status texts set via ctx.ui.setStatus (upstream
    /// `getExtensionStatuses`).
    pub fn extension_statuses(&self) -> &BTreeMap<String, String> {
        &self.extension_statuses
    }

    /// Set an extension status; None removes it (upstream
    /// `setExtensionStatus`).
    pub fn set_extension_status(&mut self, key: &str, text: Option<&str>) {
        match text {
            Some(text) => {
                self.extension_statuses
                    .insert(key.to_string(), text.to_string());
            }
            None => {
                self.extension_statuses.remove(key);
            }
        }
    }

    /// Clear all extension statuses (upstream `clearExtensionStatuses`).
    pub fn clear_extension_statuses(&mut self) {
        self.extension_statuses.clear();
    }

    /// Number of unique providers with available models (upstream
    /// `getAvailableProviderCount` / `setAvailableProviderCount`).
    pub fn available_provider_count(&self) -> usize {
        self.available_provider_count
    }

    pub fn set_available_provider_count(&mut self, count: usize) {
        self.available_provider_count = count;
    }

    /// Subscribe to branch changes (upstream `onBranchChange`); the host
    /// calls notify_branch_change when its watchers fire.
    pub fn on_branch_change(&mut self, callback: Box<dyn Fn() + Send>) -> usize {
        self.branch_change_callbacks.push(callback);
        self.branch_change_callbacks.len() - 1
    }

    /// Remove a previously registered callback (upstream the
    /// unsubscribe closure).
    pub fn off_branch_change(&mut self, index: usize) {
        if index < self.branch_change_callbacks.len() {
            drop(self.branch_change_callbacks.remove(index));
        }
    }

    /// Notify all branch-change callbacks (upstream
    /// `notifyBranchChange`).
    pub fn notify_branch_change(&self) {
        for callback in &self.branch_change_callbacks {
            callback();
        }
    }
}
