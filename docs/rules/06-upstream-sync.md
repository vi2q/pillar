# 06 — Upstream sync

pillar tracks two upstream projects. This document pins the revisions and defines the sync procedure.

## Pinned revisions

| Project | Role | Revision | Source |
| --- | --- | --- | --- |
| pi | Behavioral ground truth (all crates except `pillar-extensions` semantics source) | v0.84.3, commit `56700d42ed65a94a80af7376adb19a9298065164` | <https://github.com/earendil-works/pi> |
| luaur | Extension runtime engine (VM, type checker, safe API) | v0.1.8, commit `c2ed1091fbe9b1fcf07cbbb5a252a875ba37f796` | <https://github.com/pjankiewicz/luaur> |

The pin lives in `UPSTREAM.toml` at the repo root:

```toml
[pi]
version = "0.84.3"
commit = "56700d42ed65a94a80af7376adb19a9298065164"
repository = "https://github.com/earendil-works/pi"

[luaur]
version = "0.1.8"
commit = "c2ed1091fbe9b1fcf07cbbb5a252a875ba37f796"
repository = "https://github.com/pjankiewicz/luaur"
```

`UPSTREAM.toml` is the single source of truth; CI reads it, the sync tooling reads it, and this document's table must match it. The version column here is descriptive; the commit is the pin.

## Reference checkouts

Sync and parity tooling clones upstream at the pinned commit into a cache directory (`~/.cache/pillar/upstream/` or a path from `PILLAR_UPSTREAM_CACHE`). Checkouts are inputs, never committed: no upstream source, fixtures, or derived files enter the repo except recorded fixture outputs produced by running upstream (which are normal test fixtures).

## Goal

目標は [開発方針](../DEVELOPMENT-STRATEGY.md) に定めるゲーム開発支援の必要機能上限、高性能化、azparam / LMPC向け再利用であり、pinned piの全package・全module・全挙動の完全移植ではない。

upstream更新は、採用面への影響・不具合修正・必要な新能力を分類して選択的に取り込む。非採用面の変更は参照記録に留める。機能の完了は制作工程・実拡張・契約の検証で判断し、行数や移植率で判断しない。

## Sync procedure

Run when a task requires behavior from a newer pi or luaur, or on a scheduled cadence (monthly or per upstream minor release, whichever comes first).

1. **Pick the target.** Read upstream CHANGELOGs between the pinned revision and the target. pi: `packages/*/CHANGELOG.md`, sections under `## [Unreleased]` and released versions. Classify every change: strict-layer (observable) or free-layer (internal).
2. **Bump the pin.** Update `UPSTREAM.toml` in its own commit: `chore(sync): bump pi to <version> (<commit>)`. This commit contains only the pin change.
3. **Port behavior diffs.** One commit per crate or subsystem, messages `sync(pi/<pkg>): port <change summary>`. For each change:
   - 採用した互換面: 取り込む挙動を契約と照合し、parity/contract testを更新する。pillarの意図的差分を機械的に上書きしない。
   - 新しい外部機能: 開発方針の採用条件に照らして採否を決める。採用しない変更を未完了の移植作業として積まない。
   - 内部実装: pillarの境界・性能に必要か判断して採用／見送りを記録する。
   - Update `// divergence:` comments if upstream removed the reason for one.
4. **Luaur bumps** additionally require: feature-flag review (`luaur-rt` features pillar enables), definition-file re-check against the extension corpus, and conformance rerun.
5. Verify the relevant parity layers described in [05-testing-parity.md](05-testing-parity.md), including parity layers against the new pinned checkout.
6. **Update docs.** If an upstream doc change alters an extension contract (events, payloads, UI methods), update [04-luau-extensions.md](04-luau-extensions.md) in the same sync, in a commit `docs(extensions): track pi <change summary>`.

## Divergence ledger

Every intentional divergence from upstream carries a `// divergence:` comment in code ([03-rust-conventions.md](03-rust-conventions.md)) and an entry in the crate CHANGELOG under `### Changed` or `### Removed`. The known standing divergences:

| Divergence | Reason | Tracked in |
| --- | --- | --- |
| Extension runtime: TypeScript/jiti → Luau on luaur | Project goal | [04-luau-extensions.md](04-luau-extensions.md) |
| Extension API method names snake_cased | Luau convention; event names and LLM-boundary keys unchanged | [04-luau-extensions.md](04-luau-extensions.md) |
| Identity and paths are `pillar`, not `pi`: `APP_NAME`, `--version`, the system prompt, `~/.pillar/agent`, `CONFIG_DIR_NAME = ".pillar"`, extension discovery, and `PILLAR_*` environment variables | Project goal (user decision 2026-09-15): the agent must not present itself as pi. Session/config *formats* stay pi-compatible, but the old `~/.pi` / `PI_*` locations are not read — migrate by copying `~/.pi/agent` to `~/.pillar/agent` | [02-porting-policy.md](02-porting-policy.md) |
| Provider protocol values (`User-Agent`, `x-opencode-client`, OpenRouter/NVIDIA/Cloudflare attribution) identify as `pillar` and follow `PILLAR_CLIENT_NAME` / `PILLAR_REFERER_URL` (no referer by default) | Project goal; the env override reverts to the upstream-verified `pi` values (`PILLAR_CLIENT_NAME=pi PILLAR_REFERER_URL=https://pi.dev`) if a provider rejects the pillar client name | This file |
| No npm package sharing for extensions | Luau modules have no npm graph; git packages still supported | [04-luau-extensions.md](04-luau-extensions.md) |

New divergences require a row here and a test ([05-testing-parity.md](05-testing-parity.md), "Every `// divergence:` comment has a test").

## Deprecation handling

If upstream removes a strict-layer surface (a CLI flag, an entry type, an event), pillar deprecates it in the same sync: keep reading/writing it, emit a warning, document the removal target. pillar never breaks the session *format* ahead of upstream (the storage location moved to `.pillar`, see the ledger).
