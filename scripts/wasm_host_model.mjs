// A Wasm host that *owns the model*: it implements the host-model protocol of
// `pillar-lmpc` (docs/DEVELOPMENT-STRATEGY.md §4 "host model").
//
// This is the shape an engine or a page uses: the module runs the turn, and
// whenever it needs a model response it publishes the request and waits. The
// scripted answers here are the same ones the native `lmpc-minimal --host-model`
// path uses, so the two traces have to match (§5-7).
//
// Protocol (see crates/pillar-lmpc/src/wasm.rs):
//   write prompt -> input buffer
//   lmpc_host_turn_start(len) -> 0 running / 1 needs model / 2 done / 3 failed
//   loop: lmpc_host_poll()
//     needs model: read lmpc_host_request_ptr()/len(), write the reply JSON into
//                  the input buffer, lmpc_host_reply(len)
//     done:        read the trace (lmpc_trace_ptr()/len())
//
// Usage: node scripts/wasm_host_model.mjs <module.wasm> [prompt]

import { readFile } from "node:fs/promises";

const path = process.argv[2];
const cancel = process.argv.includes("--cancel");
// Every positional argument is one turn on the same session (an NPC keeps its
// state), which is what the native `--host-model <p1> <p2>` does.
const prompts = process.argv
  .slice(3)
  .filter((argument) => !argument.startsWith("--"));
if (prompts.length === 0) prompts.push("remember something");
if (!path) {
  console.error(
    "usage: node scripts/wasm_host_model.mjs <module.wasm> [prompt]",
  );
  process.exit(2);
}

const { instance } = await WebAssembly.instantiate(await readFile(path), {});
const exports = instance.exports;
exports.lmpc_init?.();

const memory = () => new Uint8Array(exports.memory.buffer);
const encoder = new TextEncoder();
const decoder = new TextDecoder();

const writeInput = (text) => {
  const bytes = encoder.encode(text);
  const capacity = exports.lmpc_input_cap();
  if (bytes.length > capacity) {
    throw new Error(`the input buffer holds ${capacity} bytes`);
  }
  // Ask for the pointer first: a guest call may grow the memory, which
  // detaches any view taken before it.
  const pointer = exports.lmpc_input_ptr();
  memory().set(bytes, pointer);
  return bytes.length;
};
const read = (pointer, length) =>
  decoder.decode(memory().subarray(pointer, pointer + length));

// The host's scripted model, mirroring `host_model_scripted_reply` in
// crates/pillar-lmpc/src/lib.rs: call the `remember` tool, then answer with it.
const replyFor = (requestIndex) => {
  if (requestIndex === 0) {
    return '[{"type":"toolCall","id":"remember-1","name":"remember","arguments":{"value":"the answer is 42"}}]';
  }
  if (requestIndex === 1) return '[{"type":"text","text":"the answer is 42"}]';
  return '[{"type":"text","text":"42 again"}]';
};

let turn = 0;
let state = exports.lmpc_host_turn_start(writeInput(prompts[turn]));
let repliesSent = 0;
let cancelled = false;
for (let frame = 0; frame < 100_000; frame += 1) {
  if (state === 1 && cancel && !cancelled) {
    // A game cancels an NPC's turn when the scene changes: stop instead of
    // answering, and print how far the turn got (state 4 = cancelled). The
    // guest needs a frame or two to drain, so stop answering from here on.
    cancelled = true;
    exports.lmpc_host_cancel();
  }
  if (state === 1 && !cancelled) {
    const requestLength = exports.lmpc_host_request_len();
    const request = read(exports.lmpc_host_request_ptr(), requestLength);
    if (!request.includes(prompts[turn])) {
      console.error(
        `host_model: the request does not carry the prompt: ${request}`,
      );
      process.exit(1);
    }
    const reply = replyFor(repliesSent);
    repliesSent += 1;
    if (exports.lmpc_host_reply(writeInput(reply)) !== 0) {
      console.error("host_model: the guest rejected the reply");
      process.exit(1);
    }
  }
  if (state === 2) {
    // The turn finished: send the next prompt as another turn on the same
    // session, or print the trace when there is none left.
    turn += 1;
    if (turn >= prompts.length) {
      process.stdout.write(
        read(exports.lmpc_trace_ptr(), exports.lmpc_trace_len()),
      );
      process.exit(0);
    }
    state = exports.lmpc_host_say(writeInput(prompts[turn]));
    continue;
  }
  if (state === 4) {
    // The host cancelled the turn: the trace shows how far it got.
    process.stdout.write(
      read(exports.lmpc_trace_ptr(), exports.lmpc_trace_len()),
    );
    process.exit(0);
  }
  if (state === 3) {
    console.error(
      `host_model: the turn failed: ${read(exports.lmpc_trace_ptr(), exports.lmpc_trace_len())}`,
    );
    process.exit(1);
  }
  state = exports.lmpc_host_poll();
}
console.error("host_model: the turn did not finish");
process.exit(1);
