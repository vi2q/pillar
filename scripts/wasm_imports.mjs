// Assert the LMPC artifact imports nothing.
//
// The embedding profile claims the module needs only what the host gives it: its
// own memory. That is a checkable property — `WebAssembly.Module.imports` lists
// every host function or global a module needs — so the gate asserts it instead
// of trusting prose (docs/DEVELOPMENT.md § verification).
//
// Usage: node scripts/wasm_imports.mjs <module.wasm>

import { readFile } from "node:fs/promises";

const path = process.argv[2];
if (!path) {
  console.error("usage: node scripts/wasm_imports.mjs <module.wasm>");
  process.exit(2);
}

const bytes = await readFile(path);
const module = await WebAssembly.compile(bytes);
const imports = WebAssembly.Module.imports(module);
if (imports.length !== 0) {
  for (const entry of imports) {
    console.error(`  imports ${entry.module}.${entry.name} (${entry.kind})`);
  }
  console.error(
    `wasm_imports: the module asks the host for ${imports.length} import(s)`,
  );
  process.exit(1);
}
console.log("wasm_imports: the module imports nothing");
