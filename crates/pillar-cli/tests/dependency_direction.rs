//! The workspace's dependency direction is part of the architecture
//! (docs/rules/01-architecture.md): inner crates must not reach outwards, and
//! an added or removed edge has to be a deliberate change.
//!
//! The table below mirrors that document exactly. Update both together.

use std::path::Path;

/// The allowed (and expected) pillar-to-pillar dependency edges per crate.
const ALLOWED: &[(&str, &[&str])] = &[
    ("pillar-agent", &["pillar-ai", "pillar-telemetry"]),
    ("pillar-ai", &[]),
    (
        "pillar-cli",
        &[
            "pillar-agent",
            "pillar-ai",
            "pillar-coding-agent",
            "pillar-extensions",
        ],
    ),
    ("pillar-client", &["pillar-protocol"]),
    (
        "pillar-coding-agent",
        &["pillar-agent", "pillar-ai", "pillar-protocol", "pillar-tui"],
    ),
    (
        "pillar-extensions",
        &[
            "pillar-agent",
            "pillar-ai",
            "pillar-coding-agent",
            "pillar-tui",
        ],
    ),
    ("pillar-protocol", &[]),
    (
        "pillar-server",
        &["pillar-ai", "pillar-client", "pillar-protocol"],
    ),
    ("pillar-session-store", &["pillar-agent", "pillar-ai"]),
    ("pillar-telemetry", &[]),
    ("pillar-tui", &[]),
];

/// The pillar dependencies a crate declares through a `path` dependency.
fn path_dependencies(manifest: &str) -> Vec<String> {
    let mut deps: Vec<String> = manifest
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let (name, rest) = line.split_once('=')?;
            let name = name.trim();
            if !rest.trim_start().starts_with('{') || !rest.contains("path =") {
                return None;
            }
            name.starts_with("pillar-").then(|| name.to_string())
        })
        .collect();
    deps.sort();
    deps.dedup();
    deps
}

fn workspace_crates() -> Vec<(String, String)> {
    let crates_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir");
    let mut crates: Vec<(String, String)> = std::fs::read_dir(crates_dir)
        .expect("read crates dir")
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("pillar-") {
                return None;
            }
            let manifest = std::fs::read_to_string(entry.path().join("Cargo.toml")).ok()?;
            Some((name, manifest))
        })
        .collect();
    crates.sort();
    crates
}

#[test]
fn crate_dependencies_match_the_architecture_table() {
    let crates = workspace_crates();
    assert_eq!(
        crates.len(),
        ALLOWED.len(),
        "the table lists every workspace crate"
    );
    for (name, manifest) in &crates {
        let Some((_, expected)) = ALLOWED.iter().find(|(crate_name, _)| crate_name == name) else {
            panic!("{name} is not in the dependency table");
        };
        let mut expected: Vec<String> = expected.iter().map(|dep| dep.to_string()).collect();
        expected.sort();
        assert_eq!(
            path_dependencies(manifest),
            expected,
            "{name} declares a dependency the architecture table does not:\n\
             update docs/rules/01-architecture.md and this table together"
        );
    }
}

/// The rule that keeps the layering from inverting: nothing depends on the
/// binary's crate (or on test-support crates).
#[test]
fn no_crate_depends_on_the_cli() {
    for (name, manifest) in workspace_crates() {
        assert!(
            !path_dependencies(&manifest)
                .iter()
                .any(|dep| dep == "pillar-cli"),
            "{name} must not depend on pillar-cli"
        );
    }
}
