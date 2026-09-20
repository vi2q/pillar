//! `pillar --update` / `pillar update self` / `pillar update pi`: the
//! self-updater's command shape and its routing, without invoking cargo.

use std::sync::Mutex;

use pillar_cli::self_update;
use pillar_cli::commands::run_subcommand_with;
use pillar_coding_agent::cli::args::parse_args;

fn temp_dir(name: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pillar-self-update-{}-{}-{name}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn cargo_install_command_is_the_documented_one() {
    assert_eq!(
        self_update::cargo_install_args(),
        vec![
            "install".to_string(),
            "--git".to_string(),
            "https://github.com/vi2q/pillar".to_string(),
            "pillar-cli".to_string(),
            "--force".to_string(),
        ]
    );
}

#[test]
fn self_update_sources_are_self_and_pi() {
    assert!(self_update::is_self_update_source(Some("self")));
    assert!(self_update::is_self_update_source(Some("pi")));
    assert!(!self_update::is_self_update_source(Some("npm:@foo/bar")));
    assert!(!self_update::is_self_update_source(None));
}

#[test]
fn run_checked_reports_progress_and_success() {
    let seen: Mutex<Vec<(String, Vec<String>)>> = Mutex::new(Vec::new());
    let runner = |program: &str, args: &[String]| {
        seen.lock()
            .unwrap()
            .push((program.to_string(), args.to_vec()));
        Ok(0)
    };
    let mut out: Vec<u8> = Vec::new();
    self_update::run_checked(&runner, &mut out, false).expect("success");
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("Updating pillar"), "{text}");
    assert!(text.contains("Updated pillar"), "{text}");
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        &[("cargo".to_string(), self_update::cargo_install_args())]
    );
}

#[test]
fn run_checked_fails_when_cargo_fails() {
    let runner = |_program: &str, _args: &[String]| Ok(101);
    let mut out: Vec<u8> = Vec::new();
    let error = self_update::run_checked(&runner, &mut out, false).expect_err("failure");
    assert!(error.contains("exit 101"), "{error}");
}

#[test]
fn run_checked_refuses_offline_without_invoking_cargo() {
    let runner = |_program: &str, _args: &[String]| -> Result<i32, String> {
        panic!("cargo must not run in offline mode");
    };
    let mut out: Vec<u8> = Vec::new();
    let error = self_update::run_checked(&runner, &mut out, true).expect_err("offline");
    assert!(error.contains("offline mode"), "{error}");
}

/// `pillar update self` routes to the self-updater, not the package manager.
#[test]
fn update_self_routes_to_the_self_updater() {
    let cwd = temp_dir("cwd");
    let agent_dir = temp_dir("agent");
    let parsed = parse_args(&["update".to_string(), "self".to_string()]);
    let args = parsed.subcommand.expect("subcommand");

    let calls = Mutex::new(0usize);
    let runner = |_program: &str, _args: &[String]| {
        *calls.lock().unwrap() += 1;
        Ok(0)
    };
    let mut out: Vec<u8> = Vec::new();
    run_subcommand_with(
        &args,
        &cwd.to_string_lossy(),
        &agent_dir.to_string_lossy(),
        None,
        &mut out,
        &runner,
    )
    .expect("self update");
    assert_eq!(*calls.lock().unwrap(), 1);
    assert!(
        String::from_utf8(out).unwrap().contains("Updated pillar"),
        "the self-updater ran"
    );
}
