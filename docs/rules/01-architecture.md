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
pillar-extensions  → coding-agent, agent, ai, tui (plus luaur-rt / luaur-analysis / luaur-config)
pillar-coding-agent → agent, ai, tui, protocol
pillar-server      → ai, protocol, client
pillar-cli         → coding-agent, extensions, agent, ai
```

`crates/pillar-cli/tests/dependency_direction.rs` asserts this table exactly: an
added or removed edge fails the test, so a new dependency is a deliberate edit of
this block.

Rules:

- 実行核から具体的なUI・VM・CLI・OS adapterへ依存しない方向を目指す。現在許可するedgeは上表とテストで管理し、切断は機能を保ったまま段階的に行う。upstreamのpackage配置は参照であって制約ではない。
- `pillar-coding-agent` must not depend on `pillar-extensions`: the extension runtime needs the coding-agent runner types, so that edge would cycle. The inversion is the `LuauExtensionLoader` trait (defined in coding-agent, implemented in `pillar-extensions`); `pillar-cli` is the only crate that depends on both and wires them.
- No crate may depend on a `*-cli` or test-support crate.

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
