//! Parity tests for footer-data-provider.ts (pi v0.84.3), the resolution
//! core: git branch reading from HEAD files, detached HEAD, reftable
//! fallback, and the footer status bookkeeping.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use pillar_coding_agent::core::footer_data_provider::{
    FooterDataProvider, WATCH_DEBOUNCE_MS, is_windows_mounted_repo_path, is_wsl_environment,
    resolve_branch_with_git, resolve_git_branch, should_poll_git_head,
};
use pillar_coding_agent::core::resource_loader::GitPaths;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pillar-footer-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn git_paths(repo: &std::path::Path) -> GitPaths {
    GitPaths {
        repo_dir: repo.to_path_buf(),
        common_git_dir: repo.join(".git"),
        head_path: repo.join(".git").join("HEAD"),
    }
}

// --- branch resolution ----------------------------------------------------------------------

#[test]
fn branch_read_from_regular_repo_head() {
    let repo = temp_dir("regular");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(repo.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();

    let branch = resolve_git_branch(&git_paths(&repo));
    assert_eq!(branch.as_deref(), Some("main"));
}

#[test]
fn detached_head_reports_detached() {
    let repo = temp_dir("detached");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(repo.join(".git").join("HEAD"), "abc123def456\n").unwrap();

    assert_eq!(
        resolve_git_branch(&git_paths(&repo)).as_deref(),
        Some("detached")
    );
}

#[test]
fn unreadable_head_reports_none() {
    let repo = temp_dir("missing");
    // No .git directory at all.
    assert_eq!(resolve_git_branch(&git_paths(&repo)), None);
}

#[test]
fn reftable_invalid_falls_back_to_detached() {
    let repo = temp_dir("reftable");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    // A reftable repo's HEAD points at refs/heads/.invalid; with no git
    // binary output available the fallback reports "detached".
    std::fs::write(repo.join(".git").join("HEAD"), "ref: refs/heads/.invalid\n").unwrap();

    // resolve_branch_with_git fails gracefully when git is not a repo
    // (status != 0), so the fallback yields "detached".
    let branch = resolve_git_branch(&git_paths(&repo));
    assert_eq!(branch.as_deref(), Some("detached"));

    // A real git repo resolves via git: use this repo itself.
    let real = resolve_branch_with_git(std::path::Path::new(env!("CARGO_MANIFEST_DIR")));
    if let Some(branch) = real {
        assert!(!branch.is_empty());
    }
}

#[test]
fn worktree_gitdir_head_resolves() {
    let main_repo = temp_dir("wt-main");
    std::fs::create_dir_all(main_repo.join(".git")).unwrap();
    std::fs::write(
        main_repo.join(".git").join("HEAD"),
        "ref: refs/heads/main\n",
    )
    .unwrap();

    let worktree = temp_dir("wt-linked");
    let wt_git_dir = main_repo.join(".git").join("worktrees").join("linked");
    std::fs::create_dir_all(&wt_git_dir).unwrap();
    std::fs::write(wt_git_dir.join("HEAD"), "ref: refs/heads/linked\n").unwrap();
    std::fs::write(wt_git_dir.join("commondir"), "../../../.git\n").unwrap();

    let paths = GitPaths {
        repo_dir: worktree.clone(),
        common_git_dir: main_repo.join(".git"),
        head_path: wt_git_dir.join("HEAD"),
    };
    assert_eq!(resolve_git_branch(&paths).as_deref(), Some("linked"));
}

// --- WSL / polling helpers --------------------------------------------------------------------

#[test]
fn wsl_polling_only_for_windows_mounted_repos() {
    assert!(is_wsl_environment(true, Some("Ubuntu"), None));
    assert!(is_wsl_environment(true, None, Some("1")));
    assert!(!is_wsl_environment(true, None, None));
    assert!(!is_wsl_environment(false, Some("Ubuntu"), None));

    assert!(is_windows_mounted_repo_path(std::path::Path::new(
        "/mnt/c/repo"
    )));
    assert!(is_windows_mounted_repo_path(std::path::Path::new("/mnt/D")));
    assert!(!is_windows_mounted_repo_path(std::path::Path::new(
        "/home/user/repo"
    )));
    assert!(!is_windows_mounted_repo_path(std::path::Path::new("/mnt/")));

    assert!(should_poll_git_head(
        std::path::Path::new("/mnt/c/repo"),
        true
    ));
    assert!(!should_poll_git_head(
        std::path::Path::new("/home/u/repo"),
        true
    ));
    assert_eq!(WATCH_DEBOUNCE_MS, 500);
}

// --- status bookkeeping -------------------------------------------------------------------------

#[test]
fn extension_statuses_set_remove_and_clear() {
    let mut footer = FooterDataProvider::new();
    footer.set_extension_status("ext-a", Some("running"));
    footer.set_extension_status("ext-b", Some("idle"));
    assert_eq!(
        footer.extension_statuses().get("ext-a").map(String::as_str),
        Some("running")
    );
    // None removes.
    footer.set_extension_status("ext-a", None);
    assert!(!footer.extension_statuses().contains_key("ext-a"));
    footer.clear_extension_statuses();
    assert!(footer.extension_statuses().is_empty());
}

#[test]
fn provider_count_round_trip() {
    let mut footer = FooterDataProvider::new();
    assert_eq!(footer.available_provider_count(), 0);
    footer.set_available_provider_count(3);
    assert_eq!(footer.available_provider_count(), 3);
}

#[test]
fn branch_cache_and_callbacks() {
    let repo = temp_dir("cache");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(repo.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
    let paths = git_paths(&repo);

    let mut footer = FooterDataProvider::new();
    // Cached: first read resolves, subsequent reads reuse.
    assert_eq!(footer.git_branch(Some(&paths)).as_deref(), Some("main"));
    // Even with a now-missing HEAD, the cache holds.
    std::fs::remove_file(repo.join(".git").join("HEAD")).unwrap();
    assert_eq!(footer.git_branch(Some(&paths)).as_deref(), Some("main"));

    // Invalidate and re-read: now None (unreadable).
    footer.invalidate_branch();
    assert_eq!(footer.git_branch(Some(&paths)), None);

    // Callbacks fire on notify and can be removed.
    let calls = Arc::new(AtomicUsize::new(0));
    let calls2 = calls.clone();
    let index = footer.on_branch_change(Box::new(move || {
        calls2.fetch_add(1, Ordering::SeqCst);
    }));
    footer.notify_branch_change();
    footer.notify_branch_change();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    footer.off_branch_change(index);
    footer.notify_branch_change();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
