//! The capability ledger gate.
//!
//! `capabilities.json` (docs/DEVELOPMENT-STRATEGY.md §2-3) is the project's
//! claim about what an extension, a tool, or a host can actually call, and in
//! what state. A ledger that drifts from the code is worse than none — it is a
//! published promise nobody checks — so this test is the mechanical part:
//!
//! - every row has the required fields with values from the documented sets;
//! - the **live Luau surface** (the `pillar.*` table an extension sees, walked
//!   in the VM) equals the `surface = "luau"` rows: a newly published function
//!   without a ledger row fails, and a ledger row with nothing behind it fails;
//! - a row claiming `verified` names at least one test that exists in the
//!   repository, and a `scenario` claiming it runs does too — a claim without a
//!   test is prose;
//! - `surface = "wired"` rows must not be published under a name we do not
//!   have (they carry the name the code uses, so the surface check above covers
//!   the Luau case);
//! - every capability a scenario names exists, in the direction that matters
//!   (a scenario cannot lean on a capability that is not in the ledger).
//!
//! What it does *not* check (yet): the surfaces that are not enumerable from a
//! test — `ctx.*` (built per dispatch), tool names, and the C ABI — are checked
//! only for the presence of their adapter file and their tests. Turning the
//! `ctx.*` enumeration into a real dispatch is a docs/TASKS.md item.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use pillar_extensions::runtime::ExtensionRuntime;

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .parent()
        .expect("the repository root")
        .to_path_buf()
}

fn ledger() -> serde_json::Value {
    let path = repository_root().join("capabilities.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// Every `fn <name>` in the repository's Rust sources (test functions included),
/// so `tests = [...]` names can be resolved.
fn defined_functions(root: &Path) -> BTreeSet<String> {
    fn visit(dir: &Path, into: &mut BTreeSet<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name == "target") {
                    continue;
                }
                visit(&path, into);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                for line in text.lines() {
                    let mut line = line.trim_start();
                    // Test functions are `async fn`/`pub fn` as often as `fn`.
                    for prefix in ["pub(crate) ", "pub ", "async ", "unsafe "] {
                        if let Some(rest) = line.strip_prefix(prefix) {
                            line = rest;
                        }
                    }
                    let Some(rest) = line.strip_prefix("fn ") else {
                        continue;
                    };
                    let name: String = rest
                        .chars()
                        .take_while(|character| {
                            character.is_ascii_alphanumeric() || *character == '_'
                        })
                        .collect();
                    if !name.is_empty() {
                        into.insert(name);
                    }
                }
            }
        }
    }
    let mut functions = BTreeSet::new();
    visit(&root.join("crates"), &mut functions);
    functions
}

/// The `pillar.*` names an extension actually sees, walked in the VM.
fn live_luau_surface() -> BTreeSet<String> {
    let runtime = ExtensionRuntime::new();
    let lua = runtime.vm().clone();
    let script = r#"
        local pillar = require("@pillar")
        local out = {}
        local function walk(prefix, node)
            for key, value in pairs(node) do
                local path = prefix .. "." .. key
                if type(value) == "table" then
                    walk(path, value)
                elseif not (key == "__pillar_signal_id") then
                    table.insert(out, path)
                end
            end
        end
        walk("pillar", pillar)
        return out
    "#;
    let mut names: Vec<String> = lua
        .load(script)
        .eval()
        .expect("the pillar surface enumerates");
    names.sort();
    names.into_iter().collect()
}

const STATES: [&str; 2] = ["wired", "verified"];

#[test]
fn the_capability_ledger_matches_the_code() {
    let root = repository_root();
    let ledger = ledger();
    assert_eq!(ledger["schema_version"], 1, "the ledger's schema version");

    let mut problems: Vec<String> = Vec::new();
    let mut declared: BTreeSet<String> = BTreeSet::new();
    let mut ids: BTreeSet<String> = BTreeSet::new();

    let apis = ledger["api"].as_array().cloned().unwrap_or_default();
    for row in &apis {
        let id = row["id"].as_str().unwrap_or_default().to_owned();
        let surface = row["surface"].as_str().unwrap_or_default();
        let name = row["name"].as_str().unwrap_or_default();
        let state = row["state"].as_str().unwrap_or_default();
        if !ids.insert(id.clone()) {
            problems.push(format!("{id}: duplicate capability id"));
        }
        if !STATES.contains(&state) {
            problems.push(format!("{id}: unknown state `{state}`"));
        }
        for field in ["adapter", "permission"] {
            if row[field].as_str().unwrap_or_default().is_empty() {
                problems.push(format!("{id}: `{field}` is required"));
            }
        }
        let adapter = row["adapter"].as_str().unwrap_or_default();
        if !adapter.is_empty() && !root.join(adapter).exists() {
            problems.push(format!("{id}: the adapter file `{adapter}` does not exist"));
        }
        if surface == "luau" {
            if name.is_empty() {
                problems.push(format!("{id}: a luau row names the published path"));
            } else if !declared.insert(name.to_owned()) {
                problems.push(format!("{id}: `{name}` is declared twice"));
            }
        }
        if state == "verified" && row["tests"].as_array().is_none_or(Vec::is_empty) {
            problems.push(format!(
                "{id}: `verified` needs at least one test that pins it"
            ));
        }
    }

    let functions = defined_functions(&root);
    for row in &apis {
        let id = row["id"].as_str().unwrap_or_default();
        for key in ["tests", "interrupt_tests"] {
            for test in row[key].as_array().cloned().unwrap_or_default() {
                let test = test.as_str().unwrap_or_default();
                if !functions.contains(test) {
                    problems.push(format!(
                        "{id}: `{key}` names `{test}`, which is not defined"
                    ));
                }
            }
        }
    }

    let gaps = ledger["gap"].as_array().cloned().unwrap_or_default();
    for row in &gaps {
        let id = row["id"].as_str().unwrap_or_default();
        if !ids.insert(id.to_owned()) {
            problems.push(format!("{id}: duplicate gap id"));
        }
        if row["needed_by"].as_str().unwrap_or_default().is_empty() {
            problems.push(format!("{id}: a gap says which requirement it serves"));
        }
        if !row["name"].as_str().unwrap_or_default().is_empty() {
            problems.push(format!(
                "{id}: a gap is unpublished; it must not carry a published name"
            ));
        }
    }

    let scenarios = ledger["scenario"].as_array().cloned().unwrap_or_default();
    for row in &scenarios {
        let id = row["id"].as_str().unwrap_or_default();
        if !ids.insert(id.to_owned()) {
            problems.push(format!("{id}: duplicate scenario id"));
        }
        for capability in row["capabilities"].as_array().cloned().unwrap_or_default() {
            let capability = capability.as_str().unwrap_or_default();
            // A scenario may lean on a published capability or on a gap: a
            // missing path is exactly the reason the gap is listed.
            let known = apis
                .iter()
                .chain(gaps.iter())
                .any(|entry| entry["id"].as_str() == Some(capability));
            if !known {
                problems.push(format!(
                    "{id}: names `{capability}`, which is neither a capability nor a gap"
                ));
            }
        }
        let state = row["state"].as_str().unwrap_or_default();
        if !["running", "missing"].contains(&state) {
            problems.push(format!("{id}: unknown scenario state `{state}`"));
        }
        if state == "running" && row["tests"].as_array().is_none_or(Vec::is_empty) {
            problems.push(format!(
                "{id}: a running scenario needs a test that drives it"
            ));
        }
        for test in row["tests"].as_array().cloned().unwrap_or_default() {
            let test = test.as_str().unwrap_or_default();
            if !functions.contains(test) {
                problems.push(format!(
                    "{id}: `tests` names `{test}`, which is not defined"
                ));
            }
        }
    }

    // The surface check: both directions, so a new function cannot be published
    // silently and a ledger row cannot promise something that is not there.
    let live = live_luau_surface();
    let live: BTreeSet<String> = live.into_iter().collect();
    for name in live.difference(&declared) {
        problems.push(format!(
            "the VM publishes `{name}`, which no ledger row declares"
        ));
    }
    for name in declared.difference(&live) {
        problems.push(format!(
            "the ledger declares `{name}`, which the VM does not publish"
        ));
    }

    assert!(
        problems.is_empty(),
        "the capability ledger and the code disagree:\n  - {}",
        problems.join("\n  - ")
    );
}
