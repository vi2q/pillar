#!/usr/bin/env bash
# The CI entry point: everything a change must pass, with the gates that must not
# be skippable marked as required.
#
#   scripts/ci.sh          # build, lint, tests, smoke, Wasm comparison
#
# Difference from `scripts/check.sh` (which a developer runs): this one
# - sets `CARGO_NET_OFFLINE=true`, so the build and the tests cannot reach the
#   network (a dependency the local cache lacks fails here instead of silently
#   downloading),
# - requires the Wasm comparison (the artifact must build, node must be there,
#   the wasm32 target must be installed) instead of skipping it: a gate that
#   skips itself is not a gate,
# - fails early when the target is missing, before the long build.
#
# What this does **not** prove (docs/DEVELOPMENT.md § verification): the smoke
# run's `PILLAR_OFFLINE`/dead proxy and the source-string lints are aids, not
# isolation proofs. The isolation claims that are checked mechanically are the
# empty Wasm import list (`scripts/wasm_imports.mjs`) and the resolved dependency
# profiles (`cargo test -p pillar-cli --test dependency_profiles`).
set -euo pipefail

cd "$(dirname "$0")/.."

if ! rustup target list --installed 2>/dev/null | grep -qx "wasm32-unknown-unknown"; then
    echo "ci: the wasm32-unknown-unknown target is required: rustup target add wasm32-unknown-unknown" >&2
    exit 1
fi
if ! command -v node >/dev/null 2>&1; then
    echo "ci: node is required (the Wasm host runs the artifact)" >&2
    exit 1
fi

export CARGO_NET_OFFLINE=true
export PILLAR_REQUIRE_WASM_COMPARE=1

bash "$(dirname "$0")/check.sh"
bash "$(dirname "$0")/wasm_compare.sh"

printf '\n== ci ok\n'
