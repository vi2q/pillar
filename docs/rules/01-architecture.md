# 01 — Architecture

Crate layout, dependency direction, and module ownership for the pillar workspace.

本書の依存表は**現在の構造**を表す。製品目標、目標となる責任境界、profile別の切り出し条件は [開発方針](../DEVELOPMENT-STRATEGY.md) を参照。現在のallowlistを通るだけでは、ランタイム版への疎結合を達成したことにはならない。

## Workspace layout

```
crates/
├── pillar-ai/           # pi-ai port: unified multi-provider LLM API
├── pillar-agent/        # pi-agent-core port: agent loop, tool calling, state
├── pillar-coding-agent/ # pi-coding-agent port: CLI, session store, config, modes
├── pillar-tui/          # pi-tui port: terminal UI with differential rendering
├── pillar-client/       # pi-client port: remote session client
├── pillar-protocol/     # pi-protocol port: CBOR framing/codec/schemas
├── pillar-server/       # pi-server port: experimental remote server
├── pillar-telemetry/    # pi-telemetry port: telemetry contracts
├── pillar-session-store/ # pi session-backends/sqlite-node port: sqlite session backend
├── pillar-lmpc/         # NEW: the LMPC minimum (core + host services; no provider catalog, no terminal, no VM)
├── pillar-extensions-contract/ # NEW: the extension shapes the VM and the coding agent share
├── pillar-extensions/   # NEW (no pi counterpart): Luau extension runtime on luaur-rt
└── pillar-cli/          # NEW (no pi counterpart): the `pillar` binary; joins coding-agent and the Luau runtime
```

One binary ships: `pillar-cli` produces `pillar` (pi's `pi`). It owns the host wiring that `pillar-coding-agent` cannot (see the dependency rule below). `pillar-server`, `pillar-client`, and `pillar-protocol` are compiled but behind the same feature gates as upstream. pi's `evals` package is private upstream tooling and is not ported.

## Dependency direction

```
pillar-telemetry   (leaf)
pillar-protocol    (leaf; CBOR codec, no deps on sibling crates)
pillar-tui         (leaf among pillars; mirrors pi-tui standalone)
pillar-ai          (leaf among pillars; providers + catalog)
pillar-agent       → ai, telemetry
pillar-session-store → agent, ai (standalone backend; no crate wires it in yet)
pillar-client      → protocol
pillar-lmpc         → agent, ai
pillar-extensions-contract → agent
pillar-extensions  → extensions-contract, agent, ai (plus luaur-rt / luaur-analysis / luaur-config)
pillar-coding-agent → agent, ai, tui, protocol, extensions-contract
pillar-server      → ai, protocol, client
pillar-cli         → coding-agent, extensions, agent, ai
```

`crates/pillar-cli/tests/dependency_direction.rs` asserts this table exactly: an
added or removed edge fails the test, so a new dependency is a deliberate edit of
this block.

The *resolved* graph is gated as well (`crates/pillar-cli/tests/dependency_profiles.rs`,
docs/DEVELOPMENT-STRATEGY.md §9): it reads `Cargo.lock` and rejects the runtime core
(`pillar-agent` + `pillar-ai`) reaching the presentation, CLI, or VM layers at all,
rejects the LMPC-minimal profile pulling the terminal layer (`crossterm`, `fd-lock`,
`portable-pty`) in, and pins the Luau profile's presentation contamination to the set
it still inherits through `pillar-coding-agent` (`pillar-tui`, `crossterm`, `fd-lock`)
— a ratchet, so it cannot grow unnoticed. Both the manifest and the lock are
cross-checked, so a stale `Cargo.lock` fails the gate instead of hiding an edge.

Rules:

- 実行核から具体的なUI・VM・CLI・OS adapterへ依存しない方向を目指す。現在許可するedgeは上表とテストで管理し、切断は機能を保ったまま段階的に行う。upstreamのpackage配置は参照であって制約ではない。
- `pillar-coding-agent` must not depend on `pillar-extensions`: the extension runtime needs the runner types, so that edge would cycle. The types both sides share live in `pillar-extensions-contract` (`HostExtension`, `ExtensionRunner`, `LuauExtensionLoader`, the renderer contract, the `ctx.ui` bridge, the `exec.ts` shapes, the diagnostics), which depends only on `pillar-agent`. The VM therefore does not depend on the coding agent or the terminal layer at all, and `pillar-cli` is the only crate that depends on both and wires them (discovery, the runner build, the theme provider, the renderer payload adapters). `crates/pillar-cli/tests/dependency_profiles.rs` asserts the Luau profile reaches none of `pillar-coding-agent` / `pillar-tui` / `pillar-cli` / the terminal crates.
- No crate may depend on a `*-cli` or test-support crate.
- `pillar-ai`'s waiting goes through `clock` (`SleepFn`): retry backoff, provider timeouts, and `AbortSignal::timeout` call `pillar_ai::clock::sleep` / `timeout`, whose default is `tokio::time` natively and a host-supplied timer elsewhere. The codex provider's WebSocket transport (`tokio::net`) stays native-only (TASKS).
- `pillar-extensions` owns the VM, not the filesystem: extension sources arrive through the contract's `SourceReader` and command execution through the injected `ExecHost`, so the crate has no `std::fs` / `std::process` (asserted by `dependency_profiles::the_embedding_core_keeps_os_capabilities_behind_features`, which walks both embedding profiles). Discovery and the per-file load loop belong to the host (`pillar-coding-agent::core::extensions_luau::discover_luau_paths` + `build_luau_runner`).
- `pillar-lmpc::host_model` also runs the *host's* tools: `start_with_host_tools` adds a `host_action` tool whose call is published for the host (`needs_tool` / `tool_request_json` / `tool_result`), which is how an engine's actions reach the world; the ABI is `lmpc_host_tool_request_*` / `lmpc_host_tool_result`.
- `pillar-lmpc::host_model` stores and resumes conversations (`messages_json` / `restore`, the `lmpc_session_export` / `lmpc_session_import` ABI calls): the host owns persistence — it keeps the JSON and hands it back to a fresh session.
- `pillar-lmpc::host_model` carries streaming too: `stream_delta(text)` pushes a `TextDelta` into the pending request's stream (after a `Start`, which the loop requires before it relays partials), so the host's text arrives as `message_update` events; the ABI is `lmpc_host_stream`.
- `pillar-lmpc::host_model` also carries cancellation: `cancel()` aborts the run, voids the published request, and resolves the guest's wait with an empty answer, so a host can stop an NPC's turn without answering (`HostModelState::Cancelled` once the run drained; `lmpc_host_cancel` over the ABI).
- `pillar-lmpc::host_model` is the host-model protocol (the embedding profiles' model is host-side): the guest publishes the request (`request_json`) **and hands the request's stream to the loop immediately**, so partial text the host sends with `stream_delta_to` is relayed as `message_update` while the host is still producing it; the answer (`reply` with assistant-message JSON) is the stream's terminal event. Every published request — a model request or a host tool call — carries a **ticket**, and an answer must name it (`reply_to` / `tool_result_to` / `stream_delta_to`); an answer after a cancel, after the next turn, or for another parallel tool call is refused. Tool calls are per ticket, so one assistant message can call several in parallel and the host may answer them in any order. `restore` is only possible before the turn is polled (`can_restore`), which is why the lifecycle is create → import → start.
- The Luau layer (`pillar-extensions`) and its contract take the runtime the same way the LMPC minimum does (`pillar-agent` / `pillar-ai` with `default-features = false`): the VM never re-enables the provider catalog, the harness tools or the native search scanner. The development CLI composes that native surface at its own root — `dependency_profiles::the_luau_profile_takes_the_runtime_without_its_native_surface` asserts both sides.
- `pillar-lmpc::FrameHost` routes the runtime's process-wide timer and wall clock by the host that is *polling* (`enter()` / the pump's per-poll scope, falling back to the host installed last), so several hosts in one process keep independent clocks; its pump merges tasks enqueued during a poll with the ones still pending instead of replacing them.
- `src/abi.rs` exposes that protocol as `lmpc_*` over a fixed input buffer (tickets pack the session epoch in the high 32 bits, so replacing the session invalidates the old ones). It compiles for every target, so `tests/abi_protocol.rs` drives the ABI itself natively; `scripts/wasm_host_model.mjs` is the JavaScript host of the same protocol.
- `pillar-lmpc` builds as a `cdylib` for `wasm32-unknown-unknown` with a thin C ABI (`lmpc_demo_turn` / `lmpc_trace_ptr`); `scripts/wasm_trace.mjs` runs it under Node and `scripts/check.sh` asserts the trace equals the native one (§5-7). The clock is host-injected (`pillar_ai::clock`) because `SystemTime::now` traps on that target.
- `pillar-lmpc` carries the two host shapes the embedding needs: the native default (a thread per body, immediate waits) and `FrameHost` (no threads, no tokio: bodies and virtual timers live in the host's queue and `pump(elapsed)` drives them, which is what a game or browser frame loop provides; `TASKS` records the remaining Wasm ABI work).
- `pillar-lmpc` is the LMPC minimum as a crate: `pillar-agent` + `pillar-ai` with `default-features = false`, plus the host-service wiring (a `SpawnFn` and a clock) and a demo model, so a game host has one dependency to take. `cargo check -p pillar-lmpc --target wasm32-unknown-unknown` and `tests/minimal_turn.rs` keep it runnable without a tokio runtime.
- `pillar-ai`'s provider catalog is behind the default-on `providers` feature (per-API adapters, the HTTP transport, the model registry, provider auth): with `--no-default-features` the crate is the core a host drives through an injected `StreamFn`, and `reqwest` / `rustls` / `tokio-tungstenite` / `zstd` leave the dependency graph.
- `pillar-agent`'s OS-bound and scaffold surface is behind default-on features: `harness-tools` (the coding agent's file/shell tools, the `std`/`tokio` execution environment, the session runtime, compaction and skills — it enables `pillar-ai/providers` because the scaffold drives the model registry), `proxy`, `session-files` (the durable JSONL file backend), `search` (the native session-search scanner). `cargo check -p pillar-agent --no-default-features` is the LMPC-minimum shape and compiles natively and for `wasm32-unknown-unknown`; `dependency_profiles::the_embedding_core_keeps_os_capabilities_behind_features` fails if any *other* core source reaches `std::process` / `std::fs` / `std::net`.
- The Luau runtime is an optional dependency of `pillar-cli` behind the default-on `luau` feature: `cargo build -p pillar-cli --no-default-features` produces a binary with no VM, no `luaur`, and no extension loading (`runner::none` implements the same wiring API; `tests/dependency_profiles.rs::the_luau_feature_gates_the_vm_dependency` gates the resolved graph and `scripts/check.sh` builds it).

## Module ownership map

現在のcrateは主にupstream packageを参照して構成されている。以下は由来の地図であり、1 TS file = 1 Rust fileを内部設計の制約にしない。分離・独自強化後も元の契約と出所を追跡できるようにする。

| pillar module | upstream source |
| --- | --- |
| `pillar-ai/src/providers/*` | `packages/ai/src/providers/*` (one file per provider) |
| `pillar-ai/src/models.rs` | `packages/ai/src/models.ts` + generated catalog |
| `pillar-agent/src/agent_loop.rs` | `packages/agent/src/agent-loop.ts` |
| `pillar-agent/src/harness/*` | `packages/agent/src/harness/*` |
| `pillar-coding-agent/src/core/*` | `packages/coding-agent/src/core/*` |
| `pillar-coding-agent/src/cli/*` | `packages/coding-agent/src/cli/*` |
| `pillar-coding-agent/src/modes/*` | `packages/coding-agent/src/modes/*` |
| `pillar-coding-agent/src/migrations.rs` | `packages/coding-agent/src/migrations.ts` |
| `pillar-tui/src/*` | `packages/tui/src/*` |
| `pillar-extensions/src/*` | no upstream module; semantics from `packages/coding-agent/src/core/extensions/*` |
| `pillar-extensions-contract/src/*` | the extension contract (`core/extensions/*` shapes plus `core/exec.ts` options/results), shared by the VM and the coding agent |
| `pillar-cli/src/*` | no upstream module; wires `packages/coding-agent/src/cli.ts` + `main.ts` to the Luau runtime |

`pillar-extensions` は主要な意図的差分の一つ。TypeScript拡張runtimeをLuau VMに置き換え、採用したイベント・payload・API契約を [04-luau-extensions.md](04-luau-extensions.md) の対応に従って提供する。native検索等の独自強化も行う。Luauの必要機能上限と、UIを外せる共通契約の分離方針は [開発方針](../DEVELOPMENT-STRATEGY.md) に従う。

## Effect gate

Every side effect the host performs for an extension or for itself passes one
port, `pillar-coding-agent`'s `core::effects` (`EffectIntent` →
`EffectDecision`). The CLI implements the execution and the audit trail in
`pillar-cli`'s `EffectBroker`, and `crates/pillar-cli/tests/extension_safety_parity.rs`
fails if `runner.rs` grows a direct process or filesystem call.

Intents: `ToolCall`, `Exec`, `Fs{Read,Write,List,Stat}`, `PackageInstall`,
`PackageRemove`. Two defaults are deliberate:

- the session's tool hook denies a call whose authorizer answers `Deny`, and a
  missing authorizer allows (the host owns the trust decision);
- the package manager is the other way round: without an authorizer it refuses
  to install, remove or update a package, because those fetch or delete code the
  user never approved (startup resolution never installs).

## Session store contract

A session is one append-only entry tree plus a leaf pointer: entries carry
`id` / `parentId` / `timestamp`, and labels and session names are entries too,
so replaying the file rebuilds the live state. `pillar-coding-agent`'s session
manager owns that state and its JSONL codec (v3) is the canonical store for the
CLI.

- **Operations**: `open` (load → migrate → repair), `append` (one entry, one
  line), `rewrite` (migration, torn-tail repair, branch extraction), and the
  read-only listing used by the pickers. The loader is the only reader.
- **Damage**: a truncated last line is a torn append — it is dropped and the
  file repaired on the next open. A damaged line in the middle fails the open
  with its line numbers. A file older than `CURRENT_SESSION_VERSION` may carry
  entries without ids, which migration assigns; migration keeps the replaced
  bytes as `<file>.bak`.
- **Durability**: `append` is a buffered `write_all` with no fsync (one per
  token delta would dominate the cost) — the next open repairs a torn tail.
  `rewrite` publishes through a sibling temp file, fsync, rename. A failed
  append rolls the live state back, so memory never claims an entry the file
  does not have.
- **Single writer**: one process per session file. The manager remembers the
  length it last wrote and reports a different one (another process, an editor,
  or a deleted file) instead of interleaving or recreating. There is no OS
  lock: `create` already uses an exclusive create, and a lock per entry costs
  more than the interleaving it would prevent.
- **Other backends**: the v3 JSONL codec above is the only session store the CLI
  uses. `pillar-agent`'s harness session (`harness/session`) is the eval
  harness's own storage, not a session backend, and `pillar-session-store`'s
  SQLite backend is a standalone implementation that no crate wires in yet
  (`tests/dependency_direction.rs` fails if that changes).
- **Decision (2026-09-16)**: keep one store and keep SQLite unwired. The corpus
  in `pillar-coding-agent/tests/session_manager_parity.rs`
  (`measure_session_scale`) measured, in a debug build: 1000 sessions listed in
  18 ms, session info for 1000 files in 58 ms, 10k entries appended in 0.65 s
  and reloaded in 0.35 s — an index buys nothing at that scale, while a second
  store would split the truth the contract is meant to keep single. Revisit when
  the *measured* cost of listing / loading stops being negligible (e.g. listing
  over ~100 ms at a user's real session count), and then wire the SQLite backend
  behind this contract with the same invariants (reload == live state, torn tail
  repaired, foreign write reported, one writer).

## Upstream checkouts

The pinned pi and luaur revisions live outside the repo (see [06-upstream-sync.md](06-upstream-sync.md)). Never vendor upstream source into the workspace, even as reference copies.

## Generated code

pi generates `models.generated.ts` from live provider catalogs. pillar mirrors this: `pillar-ai/src/models_generated.rs` is generated, never hand-edited; the generator lives in `pillar-ai/src/bin/generate-models.rs`. The generated file is committed, and diffs to it are always acceptable in a commit that regenerates it.
