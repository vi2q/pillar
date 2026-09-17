#!/usr/bin/env bash
# The verification gate: build, lint, test and a sandboxed smoke run.
#
#   scripts/check.sh          # everything (the CI entry point)
#   scripts/check.sh --quick  # build, lint and smoke only (skips the test sweep)
#
# The dependency checks run as part of `cargo test`: the architecture table
# (`dependency_direction`) and the profile / feature-resolved graph
# (`dependency_profiles`, which also builds nothing).
set -euo pipefail

cd "$(dirname "$0")/.."
root=$PWD
quick=0
if [ "${1:-}" = "--quick" ]; then
    quick=1
fi

say() { printf '\n== %s\n' "$*"; }

# `timeout` is not part of a stock macOS userland.
run_timeout() {
    local seconds=$1
    shift
    if command -v timeout >/dev/null 2>&1; then
        timeout "$seconds" "$@"
        return $?
    fi
    if command -v gtimeout >/dev/null 2>&1; then
        gtimeout "$seconds" "$@"
        return $?
    fi
    local status=0
    "$@" &
    local pid=$!
    (
        sleep "$seconds"
        kill -0 "$pid" 2>/dev/null && kill "$pid" 2>/dev/null
    ) &
    local watcher=$!
    wait "$pid" || status=$?
    kill "$watcher" 2>/dev/null || true
    wait "$watcher" 2>/dev/null || true
    return $status
}

say "build (locked)"
cargo build --locked --workspace

# The build axis of docs/DEVELOPMENT-STRATEGY.md §5-3: the binary must compile
# without the Luau runtime (`cargo test` gates the resolved graph; this proves
# it compiles).
say "build without Luau"
cargo build --locked -p pillar-cli --no-default-features

say "clippy"
cargo clippy --locked --workspace --all-targets -- -D warnings

if [ "$quick" -eq 0 ]; then
    say "tests"
    cargo test --locked --workspace
else
    say "tests (skipped: --quick)"
fi

# The Wasm host direction (docs/DEVELOPMENT-STRATEGY.md §5-2/§5-6): the runtime
# core and the Luau VM must at least compile for the embedding target. This is
# a compile gate only — running a turn on a real Wasm host is still unverified
# (TASKS), and tokio / `std` availability at *runtime* is not proven by it.
if [ "$quick" -eq 0 ]; then
    if rustup target list --installed 2>/dev/null | grep -qx "wasm32-unknown-unknown"; then
        say "wasm32-unknown-unknown (LMPC minimal + Luau)"
        cargo check --locked -p pillar-agent -p pillar-extensions --target wasm32-unknown-unknown
        # The LMPC minimum is the core without its OS-bound features (the
        # coding agent's tools / execution environment, the file session
        # backend, the native search scanner): it must still compile, natively
        # and for the embedding target.
        say "LMPC minimum artifact (host services only)"
        cargo check --locked -p pillar-lmpc --target wasm32-unknown-unknown
        # The §5-7 comparison: the same input on the same recorded model must
        # produce the same trace in a Wasm host as natively. It lives in its own
        # script so that CI can require that gate directly, and so that its
        # arguments are testable with stubs
        # (crates/pillar-cli/tests/check_gate_arguments.rs).
        bash "$root/scripts/wasm_compare.sh"
        say "LMPC minimal without the OS-bound features"
        cargo check --locked -p pillar-agent --no-default-features
        cargo check --locked -p pillar-agent --no-default-features --target wasm32-unknown-unknown
    else
        say "wasm32-unknown-unknown (skipped: run 'rustup target add wasm32-unknown-unknown')"
    fi
fi

# The smoke assertions run against both feature sets: the build axis of
# docs/DEVELOPMENT-STRATEGY.md §5-3 is only met if the binary also *runs*
# without the VM (it binds an empty runner and needs no extension file).
smoke() {
    local label=$1 binary=$2
    say "smoke: $label (isolated HOME / cwd, offline, dead proxy)"
    local version
    version=$(run_timeout 60 "$binary" --version)
    [ -n "$version" ] || {
        echo "smoke: --version printed nothing" >&2
        exit 1
    }

    run_timeout 60 "$binary" --help | grep -q "Usage:" || {
        echo "smoke: --help has no usage line" >&2
        exit 1
    }

    cd "$sandbox/project"
    set +e
    local output status
    output=$(run_timeout 120 "$binary" --mode json "hello" 2>&1)
    status=$?
    set -e
    [ "$status" -ne 0 ] || {
        echo "smoke: an unauthenticated run must fail" >&2
        exit 1
    }
    [ "$status" -ne 124 ] || {
        echo "smoke: the run hung" >&2
        exit 1
    }
    printf '%s' "$output" | grep -qi "model" || {
        echo "smoke: the failure does not explain the missing model: $output" >&2
        exit 1
    }
}

sandbox=$(mktemp -d)
cleanup() { rm -rf "$sandbox"; }
trap cleanup EXIT
mkdir -p "$sandbox/home" "$sandbox/agent" "$sandbox/project"
binary="$root/target/debug/pillar"

# No user configuration, no credentials, no reachable network: the binary must
# still start, answer its own flags, and fail a run with a clear error rather
# than hang or fall back to the real home. The real HOME is kept so the
# mid-script rebuild below still finds the toolchain (rustup lives in it).
real_home=${HOME:-}
export HOME="$sandbox/home"
export PILLAR_CODING_AGENT_DIR="$sandbox/agent"
export PILLAR_OFFLINE=1
export HTTPS_PROXY=http://127.0.0.1:9
export HTTP_PROXY="$HTTPS_PROXY"
export ALL_PROXY="$HTTPS_PROXY"

# The LMPC artifact runs one turn on host services (no provider catalog, no
# terminal, no VM): its trace is the profile's smoke.
say "smoke: LMPC minimum artifact"
lmpc=$(env HOME="$real_home" cargo run --locked -q -p pillar-lmpc -- "remember something")
printf '%s' "$lmpc" | grep -q "toolResult: remembered: the answer is 42" || {
    echo "smoke: the LMPC turn did not complete: $lmpc" >&2
    exit 1
}

# `target/debug/pillar` currently holds the no-Luau build.
smoke "no Luau" "$binary"

say "rebuild with Luau (the smoke run below uses the default binary)"
cd "$root"
env HOME="$real_home" cargo build --locked -p pillar-cli

smoke "default" "$binary"

# Isolation: everything the runs wrote stays in the sandbox (the agent dir is
# redirected, so no ~/.pillar appears).
[ ! -e "$sandbox/home/.pillar" ] || {
    echo "smoke: the run wrote to the real home layout" >&2
    exit 1
}

say "ok"
