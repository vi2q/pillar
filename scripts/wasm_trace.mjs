// Run the LMPC artifact in a Wasm host and print the turn's trace.
//
// The point (docs/DEVELOPMENT-STRATEGY.md §5-7): the same input on the same
// recorded model produces the same event trace as the native binary. This host
// provides what the module needs — nothing but its own memory: the turn runs on
// the crate's frame host (no threads, no timers, no filesystem), so there is no
// import to satisfy.
//
// Usage: node scripts/wasm_trace.mjs target/wasm32-unknown-unknown/debug/pillar_lmpc.wasm

import { readFile } from "node:fs/promises";

const path = process.argv[2];
if (!path) {
  console.error("usage: node scripts/wasm_trace.mjs <module.wasm>");
  process.exit(2);
}

const bytes = await readFile(path);
const { instance } = await WebAssembly.instantiate(bytes, {});
const exports = instance.exports;

const readTrace = (length) => {
  const pointer = exports.lmpc_trace_ptr();
  return new TextDecoder().decode(
    new Uint8Array(exports.memory.buffer, pointer, length),
  );
};

// A panic on this target aborts the module (the exports throw) but the hook
// leaves its message in the trace buffer, so read it out here.
exports.lmpc_init?.();
let length;
try {
  length = exports.lmpc_demo_turn();
} catch (error) {
  const message = readTrace(Number(exports.lmpc_trace_len?.() ?? 0));
  console.error(`wasm_trace: the turn trapped: ${message || error}`);
  process.exit(1);
}
if (length === 0) {
  console.error("wasm_trace: the turn produced no trace");
  process.exit(1);
}
process.stdout.write(readTrace(length));
