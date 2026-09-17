#!/usr/bin/env bash
# The §5-7 comparison gate (docs/DEVELOPMENT-STRATEGY.md): the same input on the
# same recorded model must produce the same trace in a Wasm host as natively.
#
#   scripts/wasm_compare.sh          # requires node + the wasm32 target
#
# This is a separate script so that it can be required on its own in CI, and so
# that its *arguments* are testable without a real toolchain and without an
# artifact: `crates/pillar-cli/tests/check_gate_arguments.rs` runs it with
# `cargo` / `node` stubs and inspects the recorded argv. A broken line
# continuation (a stray literal `\`, or a `\\n` in `printf`) silently changes the
# command that runs, which `bash -n` cannot see — hence the argv check.
set -euo pipefail

cd "$(dirname "$0")/.."
root=$PWD
artifact="$root/target/wasm32-unknown-unknown/debug/pillar_lmpc.wasm"

say() { printf '\n== %s\n' "$*"; }

if ! command -v node >/dev/null 2>&1; then
    say "wasm trace comparison (skipped: node is not installed)"
    exit 0
fi

# The comparison needs the artifact that the host runs.
cargo build --locked -q -p pillar-lmpc --target wasm32-unknown-unknown

host_model() {
    node "$root/scripts/wasm_host_model.mjs" "$artifact" "$@"
}

say "wasm trace matches native (§5-7)"
native_trace=$(cargo run --locked -q -p pillar-lmpc -- "remember something")
wasm_trace=$(node "$root/scripts/wasm_trace.mjs" "$artifact")
if [ "$native_trace" != "$wasm_trace" ]; then
    printf 'native:\n%s\nwasm:\n%s\n' "$native_trace" "$wasm_trace" >&2
    echo "check: the Wasm trace differs from the native one" >&2
    exit 1
fi

# The host-model protocol: the Wasm host supplies the model answers (including a
# tool call), and the trace must still match native.
say "wasm host model matches native (§4 host model)"
native_host=$(cargo run --locked -q -p pillar-lmpc -- --host-model "remember something")
wasm_host=$(host_model "remember something")
if [ "$native_host" != "$wasm_host" ]; then
    printf 'native:\n%s\nwasm:\n%s\n' "$native_host" "$wasm_host" >&2
    echo "check: the Wasm host-model trace differs from the native one" >&2
    exit 1
fi

# A session spans turns: the second turn must see the first one (the NPC keeps
# its state), and the trace must still match native.
say "wasm multi-turn session matches native"
native_turns=$(cargo run --locked -q -p pillar-lmpc -- --host-model \
    "my name is Ada" "what is my name?")
wasm_turns=$(host_model \
    "my name is Ada" "what is my name?")
if [ "$native_turns" != "$wasm_turns" ]; then
    printf 'native:\n%s\nwasm:\n%s\n' "$native_turns" "$wasm_turns" >&2
    echo "check: the Wasm multi-turn trace differs from the native one" >&2
    exit 1
fi

# Streaming: the host sends partial text before its final answer, and the guest
# must forward each delta as a message_update.
say "wasm streaming updates match native"
native_stream=$(cargo run --locked -q -p pillar-lmpc -- --host-model --stream "tell me")
wasm_stream=$(host_model --stream "tell me")
if [ "$native_stream" != "$wasm_stream" ]; then
    printf 'native:\n%s\nwasm:\n%s\n' "$native_stream" "$wasm_stream" >&2
    echo "check: the Wasm streaming trace differs from the native one" >&2
    exit 1
fi
printf '%s' "$wasm_stream" | grep -q "message_update" || {
    echo "check: the streamed turn produced no message_update events" >&2
    exit 1
}

# Storing and resuming a conversation: the host exports the conversation, starts
# a new session, imports it, and continues.
say "wasm stored conversation resume matches native"
native_resume=$(cargo run --locked -q -p pillar-lmpc -- --host-model --resume \
    "my name is Ada" "what is my name?")
wasm_resume=$(host_model \
    --resume "my name is Ada" "what is my name?")
if [ "$native_resume" != "$wasm_resume" ]; then
    printf 'native:\n%s\nwasm:\n%s\n' "$native_resume" "$wasm_resume" >&2
    echo "check: the Wasm resume trace differs from the native one" >&2
    exit 1
fi

# Host-side tools: the model asks the host to act and the host runs it (engine
# actions), natively and in the Wasm host.
say "wasm host tools match native"
native_tool=$(cargo run --locked -q -p pillar-lmpc -- --host-tools "narrate")
wasm_tool=$(host_model \
    --host-tools "narrate")
if [ "$native_tool" != "$wasm_tool" ]; then
    printf 'native:\n%s\nwasm:\n%s\n' "$native_tool" "$wasm_tool" >&2
    echo "check: the Wasm host-tool trace differs from the native one" >&2
    exit 1
fi
printf '%s' "$wasm_tool" | grep -q "toolResult: host narrate: 42" || {
    echo "check: the host action did not run: $wasm_tool" >&2
    exit 1
}

# Host-driven cancellation: the guest must stop instead of waiting for a model
# answer it will never get.
say "wasm host cancel"
wasm_cancel=$(host_model \
    "remember something" --cancel)
printf '%s' "$wasm_cancel" | grep -q "user: remember something" || {
    echo "check: the cancelled turn lost its prompt: $wasm_cancel" >&2
    exit 1
}
if printf '%s' "$wasm_cancel" | grep -q "the answer is 42"; then
    echo "check: the cancelled turn still produced a model answer: $wasm_cancel" >&2
    exit 1
fi
