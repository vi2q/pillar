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

The target is a complete port of the pinned pi revision: every package, every module, every externally observable behavior. "Done" per module is defined by [05-testing-parity.md](05-testing-parity.md) coverage rules, not by line counts.

## Sync procedure

Run when a task requires behavior from a newer pi or luaur, or on a scheduled cadence (monthly or per upstream minor release, whichever comes first).

1. **Pick the target.** Read upstream CHANGELOGs between the pinned revision and the target. pi: `packages/*/CHANGELOG.md`, sections under `## [Unreleased]` and released versions. Classify every change: strict-layer (observable) or free-layer (internal).
2. **Bump the pin.** Update `UPSTREAM.toml` in its own commit: `chore(sync): bump pi to <version> (<commit>)`. This commit contains only the pin change.
3. **Port behavior diffs.** One commit per crate or subsystem, messages `sync(pi/<pkg>): port <change summary>`. For each change:
   - Strict-layer: port exactly; update or add parity tests.
   - Free-layer: decide port or skip; record decisions in the commit message.
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
