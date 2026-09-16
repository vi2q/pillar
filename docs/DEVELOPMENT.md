# Development guidance

This document preserves the repository guidance formerly kept in `AGENTS.md`. The short `AGENTS.md` is intentionally non-binding; use this document and the documents under `docs/rules/` as optional project guidance when relevant.

開発の目的・優先順位・完了条件・性能と疎結合の設計は [DEVELOPMENT-STRATEGY.md](DEVELOPMENT-STRATEGY.md) を参照する。ゲーム開発支援の必要機能上限（Luau拡張を含む）への到達を主線とし、高性能化とazparam / LMPC向け再利用の境界を並行して育てる。

[HANDOFF.md](HANDOFF.md) は調査時点の引き継ぎ記録。最新状態は実装・検証結果と [TASKS.md](TASKS.md) で確認し、過去の残件を現在の未実装とみなさない。

## Verification

`scripts/check.sh` is the gate: a locked build, clippy across the workspace with
`-D warnings`, the locked test sweep (which includes the dependency-direction
check), and a sandboxed smoke run of the binary with an isolated `HOME`/cwd,
`PILLAR_OFFLINE=1` and a dead proxy. `--quick` skips the test sweep for a local
loop. Wire it into CI as-is; the turn-level rule is one targeted test per change
(docs/RULES.md).

## Project

pillar は pi を参照元とする Rust 製ゲーム開発サポートエージェントで、luaur 上の Luau 拡張を備える。grep 等の独自強化、azparam への Wasm 埋め込み、将来の LMPC ランタイムへの選択的再利用を目的に含む。全 upstream 機能の再現は目的ではない。参照 revision は [06-upstream-sync.md](rules/06-upstream-sync.md) を参照。

`docs/rules/` は採用済み契約・現在の構造の参照資料。機能採否は開発方針で判断し、既存の有用な能力を縮小する口実として使わない。

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
- 採用した拡張互換面のイベント名・payload・返却値は契約どおりに保つ。Luauでのキー変換（`toolName` → `tool_name`、tool inputのschema keyは変換しない）は [04-luau-extensions.md](rules/04-luau-extensions.md) を参照。独自強化は差分とテストを明示し、全upstream APIの実装義務とは区別する。
- Check generated upstream code for external API types (`packages/*/src` in the pinned pi checkout); don't guess.

## Extension development (Luau)

Extensions live in `~/.pillar/agent/extensions/` (global; `$PILLAR_CODING_AGENT_DIR` overrides the agent directory) or `.pillar/extensions/` (project-local, only after project trust is granted) as `*.luau` files or `*/index.luau`. They must type-check under `--!strict` with the `@pillar` definitions. When editing extension docs or host API code, the contract is: type-check clean, then run; see [docs/rules/04-luau-extensions.md](../docs/rules/04-luau-extensions.md).

## Upstream reference checkouts

When a task needs reading pi or luaur source, clone them outside the repo (e.g. `/tmp/upstream/pi`, `/tmp/upstream/luaur`) at the pinned revisions. Do not commit upstream checkouts or their contents into this repo. The pinned revisions and how to update them are in [docs/rules/06-upstream-sync.md](../docs/rules/06-upstream-sync.md).

## Upstream sync workflow

Sync procedure and upstream pins are described in [docs/rules/06-upstream-sync.md](../docs/rules/06-upstream-sync.md).
