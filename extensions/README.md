# Bundled Luau extensions

Real extensions that ship with pillar (the port of pi's own extension
ecosystem). They are plain Luau files, loaded exactly like any other
extension.

## `instructions.luau`

The complete Luau port of
[`pi-instructions-ext`](https://github.com/vi2q/pi-instructions-ext): it keeps
`docs/TASKS.md` as the standing record of work instructions.

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
- `tasks_tidy` tool — deterministic formatting: checkbox syntax, flattened
  checklist items, `Confirm (user):` prefixes. Order-preserving,
  non-destructive.

Commands:

- `/tasks-init` — generate the templates (never overwrites).
- `/tasks-tidy` — normalize the format behind a confirmation dialog.
- `/tasks-archive` — move every checked item (with its notes) to a dated file
  under `docs/archives/`, behind a confirmation dialog.
- `/tasks-clear` — clear the file and regenerate the skeleton with a short
  tombstone line, behind a confirmation dialog.
- `/tasks-blocked` — two-column picker (categories ←/→, items ↑/↓) over
  unfinished / pending-confirmation / needs-fix items; Enter inserts the
  item's text into the editor.
- `/tasks-completed` — the same picker over checked items, for re-check
  requests.
- `/tasks-verify` — send the user message that starts the per-item
  confirmation walkthrough.
- `/tasks-info` — the `/tasks-*` cheat sheet.

divergences from pi's version (documented in the file itself): change
detection uses `size:mtime_ms` instead of a content hash, the owner tag is the
session id's tail instead of a SHA-1 prefix, and the picker measures display
width with a small East-Asian-width approximation.

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
extension through the real runtime: the templates, the tidy normalization
(including flattening), the archive and clear commands, the session pointer
and its dedup, the per-turn tag with the staleness variants, and the
post-compact pointer. The `ctx.ui.get_editor_text` host op the pickers use is
covered by `crates/pillar-coding-agent/tests/interactive_mode_parity.rs`.
