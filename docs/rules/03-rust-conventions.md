# 03 — Rust conventions

Coding conventions for all workspace crates. Applies to hand-written code; generated files are exempt.

## Errors

- One error enum per crate, named `<Crate>Error` (`AiError`, `AgentError`, `ExtensionsError`, …), `pub` and re-exported at the crate root.
- `thiserror` for definition. Convert at boundaries with `#[from]`; never `map_err` chains across modules.
- `anyhow` is allowed only in the binary target (`pillar-coding-agent/src/bin`), never in library crates.
- `unwrap()`/`expect()` only inside `#[cfg(test)]`. In non-test code, prefer `?` with context; where a value is provably `Some`/`Ok`, use `expect` with an invariant comment — allowed only with the comment.
- Panics must not escape: extension callbacks, tool handlers, and provider streaming callbacks catch panics (`std::panic::catch_unwind` or luaur's error bridge) and turn them into catchable errors, matching pi's behavior where an extension error aborts the current operation without killing the process.
- Error strings that reach a session log, RPC frame, or provider payload are part of the strict layer ([02](02-porting-policy.md)); keep them identical to pi's.

## Async

- Library crates are runtime-agnostic: no `tokio::` references in `pillar-ai`, `pillar-agent`, `pillar-protocol`, `pillar-extensions` public APIs beyond trait bounds.
- The binary owns the runtime: single multi-thread tokio runtime created in `main`.
- No `async fn` in traits without `#[async_trait]` or (preferred) `impl Trait` in return position where stable-compatible.
- Cancellation mirrors pi: abort signals become `tokio_util::sync::CancellationToken`; every spawned task must have a shutdown path.
- Blocking work (file IO on large trees, process spawning) goes through `spawn_blocking`; never block an async thread.

## Serde & data

- All session, RPC, and config types derive `Serialize` + `Deserialize` + `Debug` + `Clone` + `PartialEq` where meaningful.
- Field names in serialized output must match pi exactly (camelCase where pi uses camelCase): use `#[serde(rename_all = "camelCase")]` per struct, not a global cargo feature.
- Newtype wrappers over `String`/`Uuid` for ids (`SessionId`, `EntryId`, `ToolCallId`); serialize transparently (`#[serde(transparent)]`).
- Timestamps are RFC 3339 strings (`jiff` or `time` crate), matching pi's `timestamp` fields byte-for-byte after parse.
- `serde_json::Value` is confined to trust boundaries (extension `data` fields, provider `usage` extras, settings passthrough). Typed structs everywhere else.

## Naming

- Files: `snake_case.rs`. One upstream `.ts` module maps to one `.rs` module where practical ([01-architecture.md](01-architecture.md)).
- Types: `UpperCamelCase`. Keep upstream concept names so the mapping stays greppable (`SessionManager`, `AgentLoop`, `CompactionEntry`).
- Luau-facing identifiers (event names, payload keys) are the only exception — see [04-luau-extensions.md](04-luau-extensions.md).
- Acronyms in type names are capitalized (`Llm`, `Api`) per `rustfmt` defaults; do not fight the formatter.

## Visibility & API surface

- `pub` is a commitment. Anything `pub` in a library crate needs a doc comment.
- Prefer `pub(crate)` by default; widen deliberately.
- No `pub` fields on types that carry invariants; provide constructors that validate.

## Dependencies

- Direct dependencies are pinned to exact versions (`cargo` semver `=x.y.z` for anything outside the workspace).
- No dependency additions without a stated reason in the PR/commit description.
- Prefer stdlib + existing workspace deps. Justify any new async, HTTP, or schema crate; the incumbent stack is tokio, reqwest (or hyper where streaming control is needed), serde, schemars, jiff, uuid, thiserror.
- Keep `Cargo.lock` in sync; builds use `--locked` in CI.

## Formatting & lints

- Workspace `Cargo.toml` sets: `edition = "2024"`, `resolver = "3"`, `rust-version = "1.85"` (the minimum supported Rust version), `lto = "fat"`, and `codegen-units = 1` for release (mirroring luaur's release profile).

## Comments & docs

- Module headers state what upstream module it ports and any divergence:

  ```rust
  //! Port of packages/agent/src/agent-loop.ts (pi v0.84.3).
  //! divergence: uses CancellationToken instead of AbortSignal.
  ```

- `// divergence:` comments are load-bearing; sync tooling greps for them ([06-upstream-sync.md](06-upstream-sync.md)).
- Doc comments on public items: one-line summary, then details. No restating signatures in prose.
