# AGENTS.md

Operating rules for humans and coding agents working in this repository.

## Project

pillar is a Rust port of pi v0.84.3 with a Luau extension runtime on luaur v0.1.8. Upstream revisions are pinned in [docs/rules/06-upstream-sync.md](docs/rules/06-upstream-sync.md). The rules docs under `docs/rules/` are normative; this file covers day-to-day operation.

## Conversational style

- Keep answers short and concise. Technical prose only, be direct.
- No emojis in commits, issues, PR comments, or code.
- When the user asks a question, answer it first before making edits or running implementation commands.
- Explain non-trivial designs as: problem, concrete example or short trace, then solution. State why the solution is necessary and distinguish it from optional complexity.
- When responding to feedback or analysis, explicitly say whether you agree or disagree before saying what you changed.

## Code quality

- Read files in full before wide-ranging changes, before editing files you have not fully inspected, and when asked to investigate or audit. Do not rely on search snippets for broad changes.
- When porting a pi module, read the whole upstream file first. Porting from snippets is how silent behavior drift starts; see [docs/rules/02-porting-policy.md](docs/rules/02-porting-policy.md).
- No `unwrap()`/`expect()` outside tests and `#[cfg(test)]` modules. Error types are defined per crate, not per call site; see [docs/rules/03-rust-conventions.md](docs/rules/03-rust-conventions.md).
- No `unsafe` in workspace crates. If a dependency forces an unsafe boundary, contain it in one module and document the invariant; see the unsafe policy in [docs/rules/03-rust-conventions.md](docs/rules/03-rust-conventions.md).
- Never remove or downgrade functionality to fix a compile error; fix the cause.
- Do not preserve backward compatibility unless the user asks for it.
- Every extension-visible surface (event names, payload keys, return-dict keys) must match pi exactly. Event names are `snake_case` in Luau (`tool_call`), while payload keys mirror the TypeScript property names (`toolName` stays `tool_name`; the mapping table lives in [docs/rules/04-luau-extensions.md](docs/rules/04-luau-extensions.md)).
- Check generated upstream code for external API types (`packages/*/src` in the pinned pi checkout); don't guess.
- Always ask before removing functionality or code that appears intentional.

## Commands

- After code changes (not docs): `cargo check --workspace --all-targets` and `cargo clippy --workspace --all-targets -- -D warnings`. Fix all errors and warnings before committing.
- Format check: `cargo fmt --all --check`. After docs changes: `markdownlint docs/ *.md` if configured.
- Never run `cargo test` on the full workspace unless requested; it includes differential tests that need the pinned upstream checkout. Per-crate: `cargo test -p <crate>`.
- Luau extension tests run through the VM harness: `cargo test -p pillar-extensions`.
- If you create or modify a test file, run it and iterate until it passes.
- For ad-hoc scripts, write them to a temp file (e.g. `/tmp`), run, edit if needed, remove when done. Don't embed multi-line scripts in bash commands.
- Never commit unless the user asks.

## Extension development (Luau)

Extensions live in `~/.pillar/extensions/` (global) or `.pillar/extensions/` (project-local) as `*.luau` files or `*/index.luau`. They must type-check under `--!strict` with the `@pillar` definitions. When editing extension docs or host API code, the contract is: type-check clean, then run; see [docs/rules/04-luau-extensions.md](docs/rules/04-luau-extensions.md).

## Upstream reference checkouts

When a task needs reading pi or luaur source, clone them outside the repo (e.g. `/tmp/upstream/pi`, `/tmp/upstream/luaur`) at the pinned revisions. Do not commit upstream checkouts or their contents into this repo. The pinned revisions and how to update them are in [docs/rules/06-upstream-sync.md](docs/rules/06-upstream-sync.md).

## Git

- Only commit files YOU changed in this session. Stage explicit paths (`git add <path1> <path2>`); never `git add -A` / `git add .`.
- Before committing, run `git status` and verify you are only staging your files.
- Message format: `{feat,fix,docs,refactor}[(<crate>)]: <message>`. Crate scopes: `ai`, `agent`, `coding-agent`, `extensions`, `tui`, `protocol`. Message is informative and concise.
- Never run: `git reset --hard`, `git checkout .`, `git clean -fd`, `git stash`, `git add -A`, `git commit --no-verify`.
- Never force push. If rebase conflicts occur, resolve only in files you modified; if a conflict is in a file you did not modify, abort and ask the user.

## Upstream sync workflow

Code changes must not be mixed with a sync commit. Syncs follow the procedure in [docs/rules/06-upstream-sync.md](docs/rules/06-upstream-sync.md): bump pins, port behavior diffs, run the parity suite, then port behavior-specific changes in separate `{feat|fix}(<crate>): ...` commits.
