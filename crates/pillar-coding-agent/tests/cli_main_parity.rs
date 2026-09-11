//! Parity tests for the CLI bootstrap helpers and the `pillar` binary
//! (upstream main.ts `resolveAppMode` / `toPrintOutputMode` +
//! `isPlainRuntimeMetadataCommand`, and the cli.ts entry behavior).

use std::process::Command;

use pillar_coding_agent::cli::args::parse_args;
use pillar_coding_agent::cli::main::{
    AppMode, PrintOutputMode, is_plain_runtime_metadata_command, resolve_app_mode,
    to_print_output_mode,
};

fn parse(args: &[&str]) -> pillar_coding_agent::cli::args::Args {
    parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
}

fn pillar_bin() -> &'static str {
    env!("CARGO_BIN_EXE_pillar")
}

#[test]
fn resolve_app_mode_prefers_rpc() {
    let parsed = parse(&["--mode", "rpc"]);
    assert_eq!(resolve_app_mode(&parsed, true, true), AppMode::Rpc);
}

#[test]
fn resolve_app_mode_prefers_json() {
    let parsed = parse(&["--mode", "json"]);
    assert_eq!(resolve_app_mode(&parsed, true, true), AppMode::Json);
}

#[test]
fn resolve_app_mode_honors_print_flag() {
    let parsed = parse(&["--print"]);
    assert_eq!(resolve_app_mode(&parsed, true, true), AppMode::Print);
}

#[test]
fn resolve_app_mode_falls_back_to_print_when_not_a_tty() {
    let parsed = parse(&[]);
    assert_eq!(resolve_app_mode(&parsed, false, true), AppMode::Print);
    assert_eq!(resolve_app_mode(&parsed, true, false), AppMode::Print);
}

#[test]
fn resolve_app_mode_defaults_to_interactive() {
    let parsed = parse(&[]);
    assert_eq!(resolve_app_mode(&parsed, true, true), AppMode::Interactive);
}

#[test]
fn to_print_output_mode_maps_json() {
    assert_eq!(to_print_output_mode(AppMode::Json), PrintOutputMode::Json);
    assert_eq!(to_print_output_mode(AppMode::Print), PrintOutputMode::Text);
    assert_eq!(
        to_print_output_mode(AppMode::Interactive),
        PrintOutputMode::Text
    );
}

#[test]
fn plain_runtime_metadata_command_detection() {
    assert!(is_plain_runtime_metadata_command(&parse(&["--help"])));
    assert!(is_plain_runtime_metadata_command(&parse(&[
        "--list-models"
    ])));
    assert!(!is_plain_runtime_metadata_command(&parse(&[
        "--help", "--print"
    ])));
    assert!(!is_plain_runtime_metadata_command(&parse(&[
        "--mode", "json"
    ])));
    assert!(!is_plain_runtime_metadata_command(&parse(&[])));
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
