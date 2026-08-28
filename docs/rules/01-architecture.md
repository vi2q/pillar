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
└── pillar-extensions/   # NEW (no pi counterpart): Luau extension runtime on luaur-rt
```

One binary ships: `pillar-coding-agent` produces `pillar` (pi's `pi`). `pillar-server`, `pillar-client`, and `pillar-protocol` are compiled but behind the same feature gates as upstream. pi's `evals` package is private upstream tooling and is not ported.

## Dependency direction

```
pillar-telemetry   (leaf)
pillar-protocol    (leaf; CBOR codec, no deps on sibling crates)
pillar-ai         → telemetry
pillar-agent      → ai
pillar-extensions → agent, luaur-rt, luaur-analysis, luaur-config
pillar-tui        → (leaf among pillars; mirrors pi-tui standalone)
pillar-coding-agent → agent, ai, tui, extensions, client, protocol, telemetry, session-store
pillar-server     → protocol, client
pillar-client     → protocol
pillar-session-store → (leaf; rusqlite, loaded behind a feature by coding-agent)
```

Rules:

- Dependencies point the same way as upstream package deps. If pi's `packages/agent` does not import `packages/tui`, `pillar-agent` must not depend on `pillar-tui`.
- `pillar-extensions` depends on `luaur-*` crates and on `pillar-agent` types, never on `pillar-coding-agent`. The CLI wires the extension runtime into the agent; the runtime itself stays CLI-agnostic.
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

`pillar-extensions` is the deliberate divergence: pi's extension system is TypeScript-in-TS-runtime (loader.ts, runner.ts, wrapper.ts); pillar replaces the loader and runner with a Luau VM while keeping every event, payload shape, and API method documented in [04-luau-extensions.md](04-luau-extensions.md).

## Upstream checkouts

The pinned pi and luaur revisions live outside the repo (see [06-upstream-sync.md](06-upstream-sync.md)). Never vendor upstream source into the workspace, even as reference copies.

## Generated code

pi generates `models.generated.ts` from live provider catalogs. pillar mirrors this: `pillar-ai/src/models_generated.rs` is generated, never hand-edited; the generator lives in `pillar-ai/src/bin/generate-models.rs`. The generated file is committed, and diffs to it are always acceptable in a commit that regenerates it.
