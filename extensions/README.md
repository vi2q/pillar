# Bundled Luau extensions

Real extensions that ship with pillar (the port of pi's own extension
ecosystem). They are plain Luau files, loaded exactly like any other
extension.

## `instructions.luau`

The Luau port of [`pi-instructions-ext`](https://github.com/vi2q/pi-instructions-ext):
it keeps `docs/TASKS.md` as the standing record of work instructions.

What it does:

- `session_start` — injects a one-time pointer (a custom message) telling the
  agent to read `docs/RULES.md` and use `tasks_init` when the files are
  missing; sends the session owner tag.
- `before_agent_start` — rides a short per-turn reminder next to the prompt
  (identical text every turn so provider caches stay valid) and switches to a
  staleness warning when `docs/TASKS.md` / `docs/RULES.md` changed on disk
  since the agent last touched them.
- `turn_end` — refreshes the staleness baseline.
- `session_compact` — re-injects the pointer after compaction.
- `tasks_init` tool — generates `docs/TASKS.md` and `docs/RULES.md` when they
  are missing (never overwrites).
- `tasks_tidy` tool — deterministic formatting: checkbox syntax, 2-space
  nesting, `Confirm (user):` prefixes. Order-preserving, non-destructive.

Not ported yet (needs host API that is still missing, see `docs/TASKS.md`):
the `/tasks-*` commands (the host cannot invoke an extension command handler
yet), `/tasks-blocked` / `/tasks-completed` (need `ctx.ui.custom` and
`ctx.ui.get_editor_text`), `/tasks-archive` / `/tasks-clear`.

### Loading it

```sh
# one-off
pillar --mode interactive -e extensions/instructions.luau

# or install it for every session
mkdir -p ~/.pillar/agent/extensions
ln -sf "$PWD/extensions/instructions.luau" ~/.pillar/agent/extensions/
```

Project-local extensions live in `<project>/.pillar/extensions/`.

### Tests

`crates/pillar-extensions/tests/instructions_extension.rs` drives the
extension through the real runtime: the templates, the tidy normalization,
the session pointer and its dedup, the per-turn tag with the staleness
variants, and the post-compact pointer.
