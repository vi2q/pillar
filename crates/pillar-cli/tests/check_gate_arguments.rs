//! The gate's own regression test: `scripts/wasm_compare.sh` is executed with
//! `cargo` / `node` stubs, and the *recorded argv* is inspected.
//!
//! Why this exists (policy review sb39f, R1): the comparison steps were written
//! with `\\` at the end of a continuation line, so instead of continuing the
//! command they passed a literal `\` and ended it — the next line became a
//! separate command (`"my name is Ada": command not found`, exit 127). `bash -n`
//! passes on that, and the branch is skipped wherever node or the wasm target is
//! missing, so "the comparison code exists" was mistaken for "the gate ran".
//! The same escaping mistake also turns `\n` in a `printf` format into a literal
//! backslash-n.
//!
//! The stub prints a fake trace that satisfies the branch's positive assertions
//! (`message_update`, `toolResult: host narrate: 42`, the cancelled prompt), so
//! this also checks that those assertions are reachable at all: if a branch's
//! arguments are mangled, the stub's output no longer matches and the gate fails
//! before the argv assertions even run.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("the workspace root")
        .to_path_buf()
}

/// A stub that records its argv and prints a trace satisfying the gate.
const STUB: &str = r#"#!/usr/bin/env bash
log=${PILLAR_GATE_ARGV_LOG:?}
{
    printf '%s' "$(basename "$0")"
    for argument in "$@"; do printf '\t%s' "$argument"; done
    printf '\n'
} >> "$log"
printf 'events: agent_start,agent_end\nuser: remember something\nassistant: done\n'
for argument in "$@"; do
    case "$argument" in
        --stream) printf 'events: message_update,message_update,message_update,message_end\n' ;;
        --host-tools) printf 'toolResult: host narrate: 42\n' ;;
        --cancel) printf 'assistant: \n' ;;
    esac
done
"#;

/// One recorded invocation: the program name and its arguments.
struct Invocation {
    program: String,
    arguments: Vec<String>,
}

impl Invocation {
    fn has(&self, argument: &str) -> bool {
        self.arguments.iter().any(|value| value == argument)
    }
}

/// Run the comparison gate with stubbed `cargo` / `node` and return the argv log.
fn run_gate_with_stubs(sandbox: &Path) -> Vec<Invocation> {
    let bin = sandbox.join("bin");
    fs::create_dir_all(&bin).expect("create the stub dir");
    for program in ["cargo", "node"] {
        let path = bin.join(program);
        fs::write(&path, STUB).expect("write the stub");
        let mut permissions = fs::metadata(&path).expect("stat the stub").permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        fs::set_permissions(&path, permissions).expect("make the stub executable");
    }
    let log = sandbox.join("argv.log");
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new("bash")
        .arg(repo_root().join("scripts/wasm_compare.sh"))
        .env("PATH", path_env)
        .env("PILLAR_GATE_ARGV_LOG", &log)
        .env_remove("PILLAR_OFFLINE")
        .output()
        .expect("run the comparison gate");
    assert!(
        output.status.success(),
        "the gate failed with the stubs (its own assertions did not hold):\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let recorded = fs::read_to_string(&log).expect("the stubs recorded their argv");
    recorded
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut fields = line.split('\t');
            let program = fields.next().unwrap_or_default().to_string();
            Invocation {
                program,
                arguments: fields.map(str::to_string).collect(),
            }
        })
        .collect()
}

#[test]
fn the_wasm_comparison_gate_passes_whole_arguments() {
    let sandbox = std::env::temp_dir().join(format!("pillar-gate-argv-{}", std::process::id()));
    fs::create_dir_all(&sandbox).expect("create the sandbox");
    let invocations = run_gate_with_stubs(&sandbox);

    // A mangled continuation line leaves a lone `\` behind (and does not carry
    // the rest of the line), and a `\\n` in a printf format leaves `\n` literal.
    for invocation in &invocations {
        for argument in &invocation.arguments {
            assert_ne!(
                argument, "\\",
                "a literal backslash reached {}: {:?} (a `\\\\` line ending?)",
                invocation.program, invocation.arguments
            );
            assert!(
                !argument.contains("\\n"),
                "a literal backslash-n reached {}: {:?} (a `\\\\n` in printf?)",
                invocation.program,
                invocation.arguments
            );
        }
    }

    // Every comparison branch ran: five host-model cases plus the plain one.
    let node = invocations
        .iter()
        .filter(|invocation| invocation.program == "node")
        .count();
    let cargo = invocations
        .iter()
        .filter(|invocation| invocation.program == "cargo")
        .count();
    assert!(
        node >= 6 && cargo >= 6,
        "node={node} cargo={cargo}: {:?}",
        invocations
            .iter()
            .map(|invocation| format!("{} {}", invocation.program, invocation.arguments.join(" ")))
            .collect::<Vec<String>>()
    );

    // The multi-turn branch must reach one invocation with *both* prompts; with
    // the broken continuation the second prompt became its own command, so the
    // trace comparison degenerated into a no-op.
    let wasm_turns = invocations
        .iter()
        .filter(|invocation| invocation.program == "node" && invocation.has("my name is Ada"))
        .collect::<Vec<&Invocation>>();
    assert_eq!(wasm_turns.len(), 2, "the multi-turn and resume branches");
    for invocation in wasm_turns {
        assert!(
            invocation.has("what is my name?"),
            "the second prompt must be an argument of the same call: {:?}",
            invocation.arguments
        );
    }
    let native_turns = invocations
        .iter()
        .find(|invocation| invocation.program == "cargo" && invocation.has("my name is Ada"))
        .expect("the native side of the multi-turn branch");
    assert!(
        native_turns.has("what is my name?"),
        "the native side kept the prompts in one call: {:?}",
        native_turns.arguments
    );

    // The flags of each branch reached the host as arguments.
    for flag in ["--stream", "--resume", "--host-tools", "--cancel"] {
        let wasm = invocations
            .iter()
            .find(|invocation| invocation.program == "node" && invocation.has(flag))
            .unwrap_or_else(|| panic!("the {flag} branch did not reach the Wasm host"));
        assert!(
            wasm.arguments.iter().any(|value| value.ends_with(".wasm")),
            "the {flag} branch must hand the artifact to the host: {:?}",
            wasm.arguments
        );
    }

    fs::remove_dir_all(&sandbox).ok();
}
