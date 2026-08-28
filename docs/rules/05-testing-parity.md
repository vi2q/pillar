# 05 — Testing parity

How tests prove that pillar behaves like pi, and that Luau extensions behave like TypeScript extensions.

## Test layers

| Layer | What it proves | Where |
| --- | --- | --- |
| Unit | A ported module matches upstream unit behavior | `crates/*/tests/` |
| Parity (differential) | Output byte-equality against upstream on fixtures | `crates/*/tests/parity/` |
| Contract | Cross-crate surfaces (session format, CBOR, tool schemas) | `tests/contract/` (workspace) |
| Extension | Luau extension semantics against pi's extension test suite | `crates/pillar-extensions/tests/` |
| Conformance | luaur conformance scripts still pass inside pillar's VM setup | `crates/pillar-extensions/tests/conformance/` |

Upstream reference checkouts at the pinned revisions (see [06-upstream-sync.md](06-upstream-sync.md)) are inputs to parity tests, not committed fixtures.

## Unit tests

Port upstream tests when they exist:

- `packages/*/test/*.test.ts` → `crates/<pillar-crate>/tests/<module>_parity.rs`, one `#[test]` per upstream `it()`/`test()`, keeping the upstream test name in a comment so gaps are greppable.
- Not every upstream test ports cleanly (DOM/timing/node-API dependent). Skip with an explicit marker: `// skipped upstream test: <name> — <reason>`. A silent skip is a bug.

## Parity tests

Differential tests compare pillar's output against pi's recorded output on fixed inputs:

1. **Session format**: run pi and pillar against the same scripted interaction (fixture prompts, faux provider), byte-diff the resulting JSONL after normalizing timestamps and uuids. Fixtures live in `tests/fixtures/sessions/`.
2. **CLI surface**: golden files for `--help`, `--version`, error messages, and `--print` mode output.
3. **Tool results**: same tool inputs → same `content` blocks and `details` JSON.
4. **Provider requests**: capture pi's request bodies against the faux provider (`packages/ai/src/providers/faux.ts`) and replay through pillar; byte-diff after normalizing auth headers.

Parity tests run offline with recorded fixtures; no real provider API calls.

## Extension tests

pi's extension behavior is exercised through its own example extensions. Port them as Luau and assert identical outcomes:

- Port the deterministic subset of `packages/coding-agent/examples/extensions/` (e.g. `confirm-destructive`, `custom-compaction` semantics, `input-transform`, `git-checkpoint` logic). Each ported example pairs a `.luau` file with a test asserting the same observable result as the upstream TypeScript test/harness where one exists.
- Event-ordering tests assert the exact sequence from [04-luau-extensions.md](04-luau-extensions.md) for a scripted session.
- Block/modify semantics: `tool_call` returning `{ block = true, reason = "..." }` produces the same tool result as pi.
- Type-check gates: every corpus `.luau` must type-check clean under `--!strict`; a failing diagnostic is a test failure.

## Conformance

luaur's own test suite validates the VM; pillar's job is to validate the *host bridge*:

- Run a representative subset of luaur's conformance scripts (`crates/luaur-conformance`) inside pillar's VM configuration to confirm the embedded VM behaves like stock luaur (no host pollution of globals, `require` resolution per [04](04-luau-extensions.md)).
- Any luaur feature flag pillar enables must be covered here.

## Running

```bash
cargo test -p pillar-extensions           # extension suite (offline)
cargo test --workspace --exclude pillar-extensions  # everything else
cargo test -p pillar-ai parity            # one layer
```

Full-suite runs that need the upstream checkouts set `PILLAR_UPSTREAM_PI` and `PILLAR_UPSTREAM_LUAUR` to the checkout paths; unset, those tests skip (with a printed skip count, never silently).

## Coverage expectations

- Every strict-layer item in [02-porting-policy.md](02-porting-policy.md) has at least one parity test.
- Every event in [04-luau-extensions.md](04-luau-extensions.md) has an ordering or payload test.
- Every `// divergence:` comment in the code has a test documenting the divergent behavior.
- New provider: request-body parity test plus streaming event order test, before the provider is considered ported.
- New CLI flag: golden file for `--help` plus one behavior test.

## CI

CI runs, on every PR: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --locked` (offline layers), and the extension corpus type-check. Parity layers that need upstream checkouts run in a scheduled workflow against the pinned revisions.
