# 01 — Architecture

Crate layout, dependency direction, and module ownership for the pillar workspace.

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
pillar-ai         → telemetry
pillar-agent      → ai
pillar-tui        → (leaf among pillars; mirrors pi-tui standalone)
pillar-extensions → coding-agent (runner types), agent, luaur-rt, luaur-analysis, luaur-config
pillar-coding-agent → agent, ai, tui, protocol, telemetry, session-store
pillar-cli        → coding-agent, extensions
pillar-server     → protocol, client
pillar-client     → protocol
pillar-session-store → (leaf; rusqlite, loaded behind a feature by coding-agent)
```

Rules:

- Dependencies point the same way as upstream package deps. If pi's `packages/agent` does not import `packages/tui`, `pillar-agent` must not depend on `pillar-tui`.
- `pillar-coding-agent` must not depend on `pillar-extensions`: the extension runtime needs the coding-agent runner types, so that edge would cycle. The inversion is the `LuauExtensionLoader` trait (defined in coding-agent, implemented in `pillar-extensions`); `pillar-cli` is the only crate that depends on both and wires them.
- No crate may depend on a `*-cli` or test-support crate.

## Module ownership map

Each pillar crate mirrors an upstream package directory. Port one upstream module to one pillar module; keep the mapping greppable.

| pillar module | upstream source |
| --- | --- |
| `pillar-ai/src/providers/*` | `packages/ai/src/providers/*` (one file per provider) |
| `pillar-ai/src/models.rs` | `packages/ai/src/models.ts` + generated catalog |
| `pillar-agent/src/agent_loop.rs` | `packages/agent/src/agent-loop.ts` |
| `pillar-agent/src/harness/*` | `packages/agent/src/harness/*` |
| `pillar-coding-agent/src/core/*` | `packages/coding-agent/src/core/*` |
| `pillar-coding-agent/src/cli/*` | `packages/coding-agent/src/cli/*` |
| `pillar-coding-agent/src/modes/*` | `packages/coding-agent/src/modes/*` |
| `pillar-tui/src/*` | `packages/tui/src/*` |
| `pillar-extensions/src/*` | no upstream module; semantics from `packages/coding-agent/src/core/extensions/*` |
| `pillar-cli/src/*` | no upstream module; wires `packages/coding-agent/src/cli.ts` + `main.ts` to the Luau runtime |

`pillar-extensions` is the deliberate divergence: pi's extension system is TypeScript-in-TS-runtime (loader.ts, runner.ts, wrapper.ts); pillar replaces the loader and runner with a Luau VM while keeping every event, payload shape, and API method documented in [04-luau-extensions.md](04-luau-extensions.md).

## Upstream checkouts

The pinned pi and luaur revisions live outside the repo (see [06-upstream-sync.md](06-upstream-sync.md)). Never vendor upstream source into the workspace, even as reference copies.

## Generated code

pi generates `models.generated.ts` from live provider catalogs. pillar mirrors this: `pillar-ai/src/models_generated.rs` is generated, never hand-edited; the generator lives in `pillar-ai/src/bin/generate-models.rs`. The generated file is committed, and diffs to it are always acceptable in a commit that regenerates it.
