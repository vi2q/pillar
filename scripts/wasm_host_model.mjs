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
const stream = process.argv.includes("--stream");
const resume = process.argv.includes("--resume");
const hostTools = process.argv.includes("--host-tools");
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

/// Ask for a pending model request's JSON.
const readRequest = (length) => read(exports.lmpc_host_request_ptr(), length);

/// Read a guest-owned buffer: ask for its length first (that call stores the
/// content), then for its address — the address taken before may dangle.
const readGuestBuffer = (lenExport, ptrExport) => {
  const length = lenExport();
  return length === 0 ? "" : read(ptrExport(), length);
};

/// The host's scripted action, mirroring `scripted_host_action` in
/// crates/pillar-lmpc/src/lib.rs: the guest called `host_action` with
/// `{"do":…,"value":…}` and the host performs it.
const scriptedHostAction = (callJson) => {
  let arguments_ = {};
  try {
    arguments_ = JSON.parse(callJson).arguments ?? {};
  } catch {
    arguments_ = {};
  }
  const action = arguments_.do ?? "";
  const value = arguments_.value ?? "";
  return JSON.stringify({
    content: [{ type: "text", text: `host ${action}: ${value}` }],
    details: { action, value },
  });
};

/// The host's scripted model, mirroring `host_model_scripted_reply` in
/// crates/pillar-lmpc/src/lib.rs: with host tools the model asks the host to
/// act and then answers; otherwise it calls the guest's `remember` tool.
const replyFor = (requestIndex) => {
  if (hostTools) {
    return requestIndex === 0
      ? '[{"type":"toolCall","id":"call-1","name":"host_action","arguments":{"do":"narrate","value":"42"}}]'
      : '[{"type":"text","text":"the host acted"}]';
  }
  if (requestIndex === 0) {
    return '[{"type":"toolCall","id":"remember-1","name":"remember","arguments":{"value":"the answer is 42"}}]';
  }
  if (requestIndex === 1) return '[{"type":"text","text":"the answer is 42"}]';
  return '[{"type":"text","text":"42 again"}]';
};

/// Run the scripted host action the guest asked for.
const runHostAction = () => {
  const call = readGuestBuffer(
    exports.lmpc_host_tool_request_len,
    exports.lmpc_host_tool_request_ptr,
  );
  return exports.lmpc_host_tool_result(writeInput(scriptedHostAction(call)));
};

/// Answer requests until the turn ends; returns the trace when it is done.
const driveTurn = () => {
  for (let frame = 0; frame < 100_000; frame += 1) {
    const state = exports.lmpc_host_poll();
    if (state === 2) {
      return read(exports.lmpc_trace_ptr(), exports.lmpc_trace_len());
    }
    if (state === 5) {
      if (runHostAction() !== 0) {
        console.error("host_model: the guest rejected the tool result");
        process.exit(1);
      }
      continue;
    }
    if (state === 3 || state === 4) {
      console.error(`host_model: the turn ended in state ${state}`);
      process.exit(1);
    }
    if (state === 1 && !cancelled) {
      if (stream) {
        for (const delta of ["the ", "answer ", "is 42"]) {
          exports.lmpc_host_stream(writeInput(delta));
        }
      }
      exports.lmpc_host_reply(writeInput(replyFor(repliesSent)));
      repliesSent += 1;
    }
  }
  console.error("host_model: the turn did not finish");
  process.exit(1);
};

let turn = 0;
let repliesSent = 0;
let cancelled = false;

// `--resume`: run the first turn, store the conversation, then continue it in a
// *new* session (the host-side persistence round trip).
if (resume && prompts.length > 1) {
  exports.lmpc_host_turn_start(writeInput(prompts[0]), hostTools ? 1 : 0);
  driveTurn();
  const stored = readGuestBuffer(
    exports.lmpc_session_export,
    exports.lmpc_host_request_ptr,
  );
  // The scripted answers restart with the new session (the native twin does
  // the same).
  repliesSent = 0;
  exports.lmpc_host_turn_start(writeInput(prompts[1]), hostTools ? 1 : 0);
  if (exports.lmpc_session_import(writeInput(stored)) === 0) {
    console.error("host_model: the conversation was not restored");
    process.exit(1);
  }
  process.stdout.write(driveTurn());
  process.exit(0);
}

let state = exports.lmpc_host_turn_start(
  writeInput(prompts[turn]),
  hostTools ? 1 : 0,
);
for (let frame = 0; frame < 100_000; frame += 1) {
  if (state === 5) {
    // The guest called a host tool: run the engine action and answer.
    if (!hostTools || runHostAction() !== 0) {
      console.error("host_model: the guest asked for a host tool");
      process.exit(1);
    }
  }
  if (state === 1 && cancel && !cancelled) {
    // A game cancels an NPC's turn when the scene changes: stop instead of
    // answering, and print how far the turn got (state 4 = cancelled). The
    // guest needs a frame or two to drain, so stop answering from here on.
    cancelled = true;
    exports.lmpc_host_cancel();
  }
  if (state === 1 && !cancelled) {
    const request = readGuestBuffer(
      exports.lmpc_host_request_len,
      exports.lmpc_host_request_ptr,
    );
    if (!request.includes(prompts[turn])) {
      console.error(
        `host_model: the request does not carry the prompt: ${request}`,
      );
      process.exit(1);
    }
    if (stream) {
      // A real host streams as its model produces text; the guest forwards each
      // delta as a message_update event.
      for (const delta of ["the ", "answer ", "is 42"]) {
        if (exports.lmpc_host_stream(writeInput(delta)) !== 0) {
          console.error("host_model: the guest rejected a delta");
          process.exit(1);
        }
      }
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
