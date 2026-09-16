#!/usr/bin/env bash
# The verification gate: build, lint, test and a sandboxed smoke run.
#
#   scripts/check.sh          # everything (the CI entry point)
#   scripts/check.sh --quick  # build, lint and smoke only (skips the test sweep)
#
# The dependency-direction check runs as part of `cargo test` and reads
# `crates/*/Cargo.toml` against the table in docs/rules/01-architecture.md.
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

say "clippy"
cargo clippy --locked --workspace --all-targets -- -D warnings

if [ "$quick" -eq 0 ]; then
    say "tests"
    cargo test --locked --workspace
else
    say "tests (skipped: --quick)"
fi

say "smoke (isolated HOME / cwd, offline, dead proxy)"
sandbox=$(mktemp -d)
cleanup() { rm -rf "$sandbox"; }
trap cleanup EXIT
mkdir -p "$sandbox/home" "$sandbox/agent" "$sandbox/project"
binary="$root/target/debug/pillar"

# No user configuration, no credentials, no reachable network: the binary must
# still start, answer its own flags, and fail a run with a clear error rather
# than hang or fall back to the real home.
export HOME="$sandbox/home"
export PILLAR_CODING_AGENT_DIR="$sandbox/agent"
export PILLAR_OFFLINE=1
export HTTPS_PROXY=http://127.0.0.1:9
export HTTP_PROXY="$HTTPS_PROXY"
export ALL_PROXY="$HTTPS_PROXY"

version=$(run_timeout 60 "$binary" --version)
[ -n "$version" ] || { echo "smoke: --version printed nothing" >&2; exit 1; }

run_timeout 60 "$binary" --help | grep -q "Usage:" || {
    echo "smoke: --help has no usage line" >&2
    exit 1
}

cd "$sandbox/project"
set +e
output=$(run_timeout 120 "$binary" --mode json "hello" 2>&1)
status=$?
set -e
[ "$status" -ne 0 ] || { echo "smoke: an unauthenticated run must fail" >&2; exit 1; }
[ "$status" -ne 124 ] || { echo "smoke: the run hung" >&2; exit 1; }
printf '%s' "$output" | grep -qi "model" || {
    echo "smoke: the failure does not explain the missing model: $output" >&2
    exit 1
}

# Isolation: everything the run wrote stays in the sandbox (the agent dir is
# redirected, so no ~/.pillar appears).
[ ! -e "$sandbox/home/.pillar" ] || {
    echo "smoke: the run wrote to the real home layout" >&2
    exit 1
}

say "ok"
