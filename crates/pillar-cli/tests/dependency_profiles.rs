//! The dependency gate (docs/DEVELOPMENT-STRATEGY.md §9): the *resolved*
//! graph (from `Cargo.lock`) checked per composition profile, not just the
//! direct path edges that `dependency_direction.rs` asserts.
//!
//! Profiles are crate sets, not Cargo features yet (the feature split is still
//! pending): `lmpc-minimal` = the runtime core a game host embeds,
//! `lmpc-luau` = that plus the Luau runtime. The gate rejects
//! - a core crate reaching the presentation / shell / VM layers, even
//!   transitively, and
//! - the minimal profile pulling the terminal layer in at all.
//!
//! The lock graph is a conservative superset (every target / platform variant
//! is included), so a rejection here is real; a pass cannot prove a
//! platform-specific edge is absent.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;

/// The runtime core (a game host embeds this without a terminal).
const LMPC_MINIMAL: [&str; 2] = ["pillar-agent", "pillar-ai"];
/// The minimal profile plus the Luau runtime and its contract.
const LMPC_LUAU: [&str; 4] = [
    "pillar-agent",
    "pillar-ai",
    "pillar-extensions",
    "pillar-extensions-contract",
];
/// Crates a core crate must never reach.
const PRESENTATION_AND_SHELL: [&str; 4] = [
    "pillar-tui",
    "pillar-coding-agent",
    "pillar-extensions",
    "pillar-cli",
];
/// External crates that pull in a terminal, a pty, or the CLI's process layer.
const TERMINAL_LAYER: [&str; 3] = ["crossterm", "fd-lock", "portable-pty"];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("the workspace root")
        .to_path_buf()
}

/// The resolved package graph from `Cargo.lock`: package name → its direct
/// dependency names.
fn lock_graph() -> BTreeMap<String, BTreeSet<String>> {
    let lock = std::fs::read_to_string(repo_root().join("Cargo.lock")).expect("read Cargo.lock");
    let mut graph: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for block in lock.split("[[package]]").skip(1) {
        let name = block.lines().find_map(|line| {
            line.strip_prefix("name = ")
                .map(|value| value.trim_matches('"').to_string())
        });
        let Some(name) = name else { continue };
        let mut deps: BTreeSet<String> = BTreeSet::new();
        if let Some(start) = block.find("dependencies = [") {
            let list = &block[start + "dependencies = [".len()..];
            let end = list.find(']').unwrap_or(list.len());
            for entry in list[..end].split(',') {
                let entry = entry.trim().trim_matches('"').trim();
                if entry.is_empty() {
                    continue;
                }
                // A dependency may carry a version requirement after a space.
                let entry = entry.split(' ').next().unwrap_or(entry);
                deps.insert(entry.to_string());
            }
        }
        graph.insert(name, deps);
    }
    assert!(
        graph.len() > 50,
        "the lock graph parsed {} packages; the gate would be vacuous",
        graph.len()
    );
    graph
}

/// Every crate reachable from `roots` (including the roots themselves).
fn closure(graph: &BTreeMap<String, BTreeSet<String>>, roots: &[&str]) -> BTreeSet<String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<String> = roots.iter().map(|root| root.to_string()).collect();
    while let Some(name) = stack.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        for dep in graph.get(&name).into_iter().flatten() {
            stack.push(dep.clone());
        }
    }
    seen
}

fn workspace_manifests() -> Vec<(String, String)> {
    let crates_dir = repo_root().join("crates");
    let mut crates: Vec<(String, String)> = std::fs::read_dir(&crates_dir)
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

/// The dependency names a manifest declares, in any dependency table (normal,
/// dev, build, target-specific). Renamed dependencies answer the package name
/// the lock records.
fn declared_dependencies(manifest: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut in_dependencies = false;
    let mut pending_rename: Option<String> = None;
    for raw in manifest.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            in_dependencies = line.ends_with("dependencies]");
            continue;
        }
        if !in_dependencies || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, rest)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let rest = rest.trim();
        // `serde.workspace = true` / `libc.workspace = true`.
        let key = key.split('.').next().unwrap_or(key).to_string();
        if rest.starts_with('"') || rest.starts_with("true") || rest.starts_with("false") {
            names.insert(key);
            continue;
        }
        // `{ ... }`, possibly continued on the following lines.
        names.insert(key);
        if rest.contains("package =") && !rest.contains('}') {
            pending_rename = Some(String::new());
            continue;
        }
        if let Some(package) = rest.split("package =").nth(1) {
            let package = package.trim().trim_start_matches('"');
            let package = package.split('"').next().unwrap_or(package);
            if !package.is_empty() {
                names.insert(package.to_string());
            }
        }
        let _ = pending_rename.take();
    }
    names
}

/// The lock's dependency list for one workspace crate.
fn lock_dependencies(graph: &BTreeMap<String, BTreeSet<String>>, name: &str) -> BTreeSet<String> {
    graph
        .get(name)
        .cloned()
        .unwrap_or_else(|| panic!("{name} is not in Cargo.lock"))
}

#[test]
fn the_lock_graph_and_the_manifests_agree() {
    let graph = lock_graph();
    for (name, manifest) in workspace_manifests() {
        let declared = declared_dependencies(&manifest);
        let resolved = lock_dependencies(&graph, &name);
        for dep in &declared {
            assert!(
                resolved.contains(dep),
                "{name} declares {dep}, but Cargo.lock does not resolve it for that crate \
                 (stale lock? run `cargo build`)"
            );
        }
        for dep in &resolved {
            assert!(
                declared.contains(dep),
                "{name} resolves {dep} in Cargo.lock, but the manifest does not declare it \
                 (stale lock? run `cargo build`)"
            );
        }
    }
}

/// The contract crate is a leaf for the embedding profiles: taking the shared
/// extension shapes must not drag the coding agent, the TUI, or the CLI in.
#[test]
fn the_extension_contract_is_a_leaf() {
    let graph = lock_graph();
    let reached = closure(&graph, &["pillar-extensions-contract"]);
    let leaked: Vec<&String> = reached
        .iter()
        .filter(|name| {
            PRESENTATION_AND_SHELL.contains(&name.as_str())
                || TERMINAL_LAYER.contains(&name.as_str())
        })
        .collect();
    assert!(leaked.is_empty(), "the contract reaches {leaked:?}");
}

#[test]
fn lmpc_minimal_excludes_the_presentation_shell_and_terminal() {
    let graph = lock_graph();
    let reached = closure(&graph, &LMPC_MINIMAL);
    let leaked: Vec<&String> = reached
        .iter()
        .filter(|name| {
            PRESENTATION_AND_SHELL.contains(&name.as_str())
                || TERMINAL_LAYER.contains(&name.as_str())
        })
        .collect();
    assert!(
        leaked.is_empty(),
        "the LMPC minimal profile reaches {leaked:?}; the runtime core must not pull \
         the terminal, the CLI, or the VM in"
    );
}

/// Core crates never reach the presentation / shell / VM layers, transitively.
#[test]
fn core_crates_do_not_reach_the_presentation_or_shell() {
    let graph = lock_graph();
    for core in [
        "pillar-ai",
        "pillar-agent",
        "pillar-telemetry",
        "pillar-protocol",
    ] {
        let reached = closure(&graph, &[core]);
        let leaked: Vec<&String> = reached
            .iter()
            .filter(|name| {
                PRESENTATION_AND_SHELL.contains(&name.as_str())
                    || TERMINAL_LAYER.contains(&name.as_str())
            })
            .collect();
        assert!(leaked.is_empty(), "{core} reaches {leaked:?}");
    }
}

/// The Luau profile is clean: with the contract crate in place the VM no
/// longer reaches the coding agent, the TUI, or the terminal layer. This is
/// what makes "Luau present or absent" an independent build axis
/// (docs/DEVELOPMENT-STRATEGY.md §5-3): a regression here (for example the VM
/// re-acquiring a dependency on `pillar-coding-agent`) fails the gate.
#[test]
fn lmpc_luau_excludes_the_presentation_shell_and_terminal() {
    let graph = lock_graph();
    let reached = closure(&graph, &LMPC_LUAU);
    let leaked: Vec<&String> = reached
        .iter()
        .filter(|name| {
            // The profile's own roots are its members, not contamination.
            !LMPC_LUAU.contains(&name.as_str())
                && (PRESENTATION_AND_SHELL.contains(&name.as_str())
                    || TERMINAL_LAYER.contains(&name.as_str()))
        })
        .collect();
    assert!(
        leaked.is_empty(),
        "the Luau profile reaches {leaked:?}; taking the VM must not pull the coding agent, \
         the TUI, or the terminal layer in"
    );
}

/// The names `cargo tree` resolves for one feature set of a crate.
///
/// The lock graph above is a superset (every feature and target variant), so
/// it cannot express "the Luau axis is off". `cargo tree` resolves the feature
/// set exactly, which is what the build axis has to be gated on.
fn tree_names(package: &str, no_default_features: bool) -> BTreeSet<String> {
    let mut command = Command::new(env!("CARGO"));
    command.args([
        "tree",
        "-p",
        package,
        "-e",
        "normal",
        "--prefix",
        "none",
        "--no-dedupe",
        "--locked",
        // The gate must not touch the network (CI runs offline).
        "--offline",
    ]);
    if no_default_features {
        command.arg("--no-default-features");
    }
    let output = command.output().expect("run cargo tree");
    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_string)
        .collect()
}

/// The Luau runtime is a *build* axis, not only a dependency property: with
/// `--no-default-features` the binary must resolve without the VM crate or
/// `luaur` (docs/DEVELOPMENT-STRATEGY.md §5-3). The default build keeps them,
/// so a feature that silently stops gating anything fails here too.
#[test]
fn the_luau_feature_gates_the_vm_dependency() {
    let without = tree_names("pillar-cli", true);
    assert!(
        !without.contains("pillar-extensions"),
        "a --no-default-features build still resolves the VM crate"
    );
    assert!(
        !without
            .iter()
            .any(|name| name.starts_with("luaur") || name.starts_with("mlua")),
        "a --no-default-features build still resolves a Luau engine: {without:?}"
    );

    let with = tree_names("pillar-cli", false);
    assert!(
        with.contains("pillar-extensions") && with.iter().any(|name| name.starts_with("luaur")),
        "the default build must contain the Luau runtime (the gate above would be vacuous)"
    );
}
