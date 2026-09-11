//! Integration tests for the `pillar` binary (upstream cli.ts entry behavior).

use std::process::Command;

fn pillar_bin() -> &'static str {
    env!("CARGO_BIN_EXE_pillar")
}

#[test]
fn binary_version_prints_pinned_pi_version() {
    let output = Command::new(pillar_bin())
        .arg("--version")
        .output()
        .expect("run pillar");
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "0.84.3");
}

#[test]
fn binary_help_prints_usage() {
    let output = Command::new(pillar_bin())
        .arg("--help")
        .output()
        .expect("run pillar");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("pi - AI coding assistant"));
    assert!(stdout.contains("Usage:"));
}

#[test]
fn binary_reports_unknown_short_option() {
    let output = Command::new(pillar_bin())
        .arg("-z")
        .output()
        .expect("run pillar");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Unknown option: -z"));
}

#[test]
fn binary_rejects_file_args_in_rpc_mode() {
    let output = Command::new(pillar_bin())
        .args(["--mode", "rpc", "@prompt.md"])
        .output()
        .expect("run pillar");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("@file arguments are not supported in RPC mode")
    );
}
