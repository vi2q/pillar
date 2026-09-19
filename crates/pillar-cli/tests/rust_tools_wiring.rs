//! The opt-in Rust workflow wiring (design §4, §9): the native host adapters
//! assemble into one toolkit whose four tools are the `rs_*` set. The CLI
//! appends these to `custom_tools` only when `PILLAR_RUST_TOOLS` is set.
//!
//! This does not launch Cargo: the host's metadata is seeded directly, so the
//! test stays offline and deterministic.

#![cfg(not(target_arch = "wasm32"))]

use pillar_agent::rust_tools::host::OwnerId;
use pillar_agent::rust_tools::{RustToolLimits, RustToolkit};
use pillar_cli::effects::EffectBroker;
use pillar_coding_agent::core::rust_host::NativeRustHost;

fn toolkit() -> RustToolkit {
    let broker = EffectBroker::permissive();
    let host = NativeRustHost::new("/tmp", Some(broker.authorizer()));
    host.metadata().set_saved_metadata(
        r#"{"packages":[],"workspace_members":[],"workspace_default_members":[],"workspace_root":"/tmp","target_directory":"/tmp/target"}"#,
    );
    host.toolkit(
        OwnerId::new(pillar_ai::uuid::uuidv7()),
        RustToolLimits::default(),
    )
}

#[test]
fn the_native_host_assembles_the_four_rs_tools() {
    let names: Vec<String> = toolkit()
        .tools()
        .into_iter()
        .map(|tool| tool.tool.name)
        .collect();
    assert_eq!(
        names,
        vec!["rs_verify_plan", "rs_run", "rs_job", "rs_diagnostics"]
    );
}

#[test]
fn the_opt_in_env_flag_is_off_by_default() {
    // The CLI only registers the tools when PILLAR_RUST_TOOLS is set; the
    // default environment here does not set it.
    let enabled = matches!(
        std::env::var("PILLAR_RUST_TOOLS").ok().as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    );
    assert!(
        !enabled,
        "PILLAR_RUST_TOOLS must not be set in the test env"
    );
}
