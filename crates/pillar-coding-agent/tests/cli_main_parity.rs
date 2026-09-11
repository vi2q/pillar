//! Parity tests for the CLI bootstrap helpers (upstream main.ts
//! `resolveAppMode` / `toPrintOutputMode` / `isPlainRuntimeMetadataCommand`).
//!
//! The `pillar` binary itself lives in the `pillar-cli` crate; its behavior
//! is covered by that crate's integration tests.

use pillar_coding_agent::cli::args::parse_args;
use pillar_coding_agent::cli::main::{
    AppMode, PrintOutputMode, is_plain_runtime_metadata_command, resolve_app_mode,
    to_print_output_mode,
};

fn parse(args: &[&str]) -> pillar_coding_agent::cli::args::Args {
    parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
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
