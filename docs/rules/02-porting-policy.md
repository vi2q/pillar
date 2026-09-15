# 02 — Porting policy

pillar is a hybrid port: the external boundary of pi is reproduced exactly, the internals are idiomatic Rust. This document defines which is which, and the TypeScript→Rust conversion rules.

## The two layers

### Strict-compatibility layer (behavior pinned to pi)

These surfaces must be byte- or schema-identical to pi v0.84.3. Breaking them breaks sessions, tools, and extensions written against pi.

1. **CLI**: every flag (`--model`, `--print`, `--extension`, `--resume`, `--continue`, `--mode`, `--no-session`, …), flag aliases, argument order semantics, exit codes, and the version string format.
2. **Session files**: JSONL at `~/.pillar/agent/sessions/--<path>--/<timestamp>_<uuid>.jsonl`, version-3 tree structure (`id`/`parentId` linking), every entry `type` (`message`, `model_change`, `thinking_level_change`, `compaction`, `branch_summary`, `custom`, `label`, `session_info`, `custom_message`), the `SessionHeader` shape, and v1/v2→v3 migration on load. pillar must read pi sessions and pi must read pillar sessions.
3. **Agent↔tool contract**: built-in tool names (`bash`, `edit`, `find`, `grep`, `ls`, `read`, `write`, `powershell` on Windows), their input schemas, result `content` blocks, and `details` shapes.
4. **LLM request/response semantics**: provider request bodies, streaming event order, usage accounting, retry/backoff behavior per provider. A provider implementation is correct when pi's own provider tests pass against it.
5. **CBOR protocol** (`pillar-protocol`): framing, codec, and schema bytes must interoperate with pi's `pi-server`/`pi-client`.
6. **Settings/config files**: `~/.pillar/agent/settings.json` keys and precedence, `AGENTS.md` discovery, skill/theme/prompt-template discovery paths.
7. **Extension semantics**: event names, firing order, payload shapes, result handling (block/modify), and the full `ExtensionAPI` method surface. The Luau mapping is defined in [04-luau-extensions.md](04-luau-extensions.md) and is itself part of this layer.
8. **Slash commands and keybindings**: default names and behavior.

### Free layer (idiomatic Rust allowed)

- Internal module decomposition (subject to [01-architecture.md](01-architecture.md)).
- Data structures, caching, and allocation strategies.
- Error type hierarchy and error message strings, except where an error reaches the session log, RPC, or a provider payload.
- Concurrency model (tokio tasks, channels) as long as event ordering matches.
- Anything reachable only from Rust code, not from sessions/CLI/network.

When in doubt: if a user can observe it (terminal output, session file, network bytes, extension script), it is strict. If only a Rust caller sees it, it is free.

## TypeScript → Rust conversion table

| TypeScript construct | Rust replacement |
| --- | --- |
| `interface`/`type` object shapes | `struct` + `serde::Deserialize`/`Serialize`; schema-validated shapes derive `JsonSchema` |
| tagged unions (`type: "..."`) | `#[serde(tag = "type")] enum` |
| discriminated event payloads | one enum per event family, serde-tagged |
| `Promise<T>` / `async` | `async fn` (tokio runtime at the binary edge; libraries stay runtime-agnostic) |
| exceptions / `try/catch` | `Result<T, E>`; see [03-rust-conventions.md](03-rust-conventions.md) |
| `any` / loose objects | `serde_json::Value` only at trust boundaries (extension payloads, provider extras); typed structs everywhere else |
| `Map`/`Set` | `std::collections::HashMap`/`HashSet` (or `BTreeMap` where key order is observable, e.g. serialized output) |
| `string \| undefined` | `Option<String>` |
| classes with inheritance | structs + traits; upstream inheritance is shallow and maps to composition |
| jiti TS module loading | removed (replaced by Luau VM; see [04](04-luau-extensions.md)) |
| `EventEmitter` | typed event bus with `tokio::sync::mpsc`/`broadcast`, preserving handler order |
| getter properties | methods (Rust has no property syntax; call sites may differ) |
| `readonly` fields | plain fields; no `&mut` escape if upstream treats it as immutable |
| numeric string enums (`"low" \| "medium" \| "high"`) | `#[derive(Serialize, Deserialize)] enum` with `#[serde(rename_all = "lowercase")]` |

## Porting procedure per module

1. Read the entire upstream file (AGENTS.md rule). Note every externally observable behavior: output strings, file paths, ordering, defaults.
2. Write the Rust module skeleton with types first, then logic.
3. Port the upstream tests for that module (see [05-testing-parity.md](05-testing-parity.md)).
4. Diff behavior, not code: run the parity tests; byte-diff session outputs on fixture sessions.
5. Record any intentional divergence in the module header comment (`// divergence: <reason>`) and in the crate's CHANGELOG.

## Known hard cases

- **`agent-loop.ts` streaming**: upstream interleaves async iterators and abort signals. Keep the event sequence identical even if the scheduling differs.
- **`jiti` TypeScript loading**: intentionally not ported. The replacement contract is [04-luau-extensions.md](04-luau-extensions.md).
- **TUI differential rendering** (`pi-tui`): layout algorithms must produce identical output grids; internal node representation is free.
- **provider OAuth flows**: token refresh timing affects credentials storage; port the storage format exactly, the refresh scheduling is free.
- **`models.generated.ts`**: regenerate through the generator, never hand-port; see [01-architecture.md](01-architecture.md).
