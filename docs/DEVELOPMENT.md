# Development guidance

This document preserves the repository guidance formerly kept in `AGENTS.md`. The short `AGENTS.md` is intentionally non-binding; use this document and the documents under `docs/rules/` as optional project guidance when relevant.

## Project

pillar is a Rust port of pi v0.84.3 with a Luau extension runtime on luaur v0.1.8. Upstream revisions are pinned in [docs/rules/06-upstream-sync.md](../docs/rules/06-upstream-sync.md). The rules docs under `docs/rules/` are optional reference material.

## Conversational style

- Keep answers short and concise. Technical prose only, be direct.
- No emojis in commits, issues, PR comments, or code.
- When the user asks a question, answer it first before making edits or running implementation commands.
- Explain non-trivial designs as: problem, concrete example or short trace, then solution. State why the solution is necessary and distinguish it from optional complexity.
- When responding to feedback or analysis, explicitly say whether you agree or disagree before saying what you changed.

## Code quality

- Read files in full before wide-ranging changes, before editing files you have not fully inspected, and when asked to investigate or audit. Do not rely on search snippets for broad changes.
- When porting a pi module, read the whole upstream file first. Porting from snippets is how silent behavior drift starts; see [docs/rules/02-porting-policy.md](../docs/rules/02-porting-policy.md).
- Never remove or downgrade functionality to fix a compile error; fix the cause.
- Do not preserve backward compatibility unless the user asks for it.
- Every extension-visible surface (event names, payload keys, return-dict keys) must match pi exactly. Event names are `snake_case` in Luau (`tool_call`), while payload keys mirror the TypeScript property names (`toolName` stays `tool_name`; the mapping table lives in [docs/rules/04-luau-extensions.md](../docs/rules/04-luau-extensions.md)).
- Check generated upstream code for external API types (`packages/*/src` in the pinned pi checkout); don't guess.

## Extension development (Luau)

Extensions live in `~/.pillar/extensions/` (global) or `.pillar/extensions/` (project-local) as `*.luau` files or `*/index.luau`. They must type-check under `--!strict` with the `@pillar` definitions. When editing extension docs or host API code, the contract is: type-check clean, then run; see [docs/rules/04-luau-extensions.md](../docs/rules/04-luau-extensions.md).

## Upstream reference checkouts

When a task needs reading pi or luaur source, clone them outside the repo (e.g. `/tmp/upstream/pi`, `/tmp/upstream/luaur`) at the pinned revisions. Do not commit upstream checkouts or their contents into this repo. The pinned revisions and how to update them are in [docs/rules/06-upstream-sync.md](../docs/rules/06-upstream-sync.md).

## Upstream sync workflow

Sync procedure and upstream pins are described in [docs/rules/06-upstream-sync.md](../docs/rules/06-upstream-sync.md).
