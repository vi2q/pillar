# 04 — Luau extensions

The extension runtime contract. Extensions are Luau scripts executed by an embedded luaur VM; every event, payload, and API method mirrors pi's TypeScript extension system as defined in `packages/coding-agent/docs/extensions.md` and `packages/coding-agent/src/core/extensions/types.ts` (pi v0.84.3).

## Runtime

- VM: `luaur-rt` (luaur v0.1.8) with features `serde`, `async`, `typecheck`.
- One `Lua` instance per pi process for global extensions; each extension file loads as a module returning a setup function, mirroring pi's default-export factory.
- Every extension file is type-checked with `luaur-analysis` against the `@pillar` definitions before it runs. Files that fail type-checking are skipped with a warning listing the diagnostics; they do not abort startup. (pi compiles TS with jiti and fails the same way: per-file load error, other extensions continue.)
- Extensions declare `--!strict` at the top; non-strict files still type-check under the checker's non-strict mode.
- The VM is sandboxed by capability: an extension only reaches what the host injects. There is no `io`, `os.execute`, or `require` of arbitrary paths. `require("@pillar")` and `require("@pillar.tui")` are the only roots. Note the placement rules: extension scripts run with the user's full permissions via host calls (`pillar.exec`, tool registration) exactly like pi extensions; the sandbox limits the script language surface, not the host authority.

## Discovery locations

| Location | Scope |
| --- | --- |
| `~/.pillar/extensions/*.luau` | Global |
| `~/.pillar/extensions/*/index.luau` | Global (subdirectory) |
| `.pillar/extensions/*.luau` | Project-local |
| `.pillar/extensions/*/index.luau` | Project-local (subdirectory) |

Load order matches pi: global first, then project-local; within a directory, sorted by file name. Project-local extensions load only after project trust is granted (`project_trust` event fires before project-local loads).

`pillar -e ./ext.luau` loads an extra extension for one run, same as pi's `--extension`.

## Naming conventions

Luau has no camelCase tradition for library APIs; pillar's host API uses `snake_case` for methods and fields. Event names are already `snake_case` in pi (`tool_call`, `session_start`) and are unchanged. Payload keys convert camelCase → `snake_case` mechanically:

| TypeScript (pi) | Luau (pillar) |
| --- | --- |
| `event.toolName` | `event.tool_name` |
| `event.input.command` | `event.input.command` (unchanged; tool inputs keep their schema keys verbatim) |
| `ctx.ui.setWidget(key, lines)` | `ctx.ui.set_widget(key, lines)` |
| `pi.registerTool(def)` | `pillar.register_tool(def)` |
| `pi.registerCommand(name, opts)` | `pillar.register_command(name, opts)` |
| `pi.appendEntry(type, data)` | `pillar.append_entry(type, data)` |

Tool input keys and tool result content blocks are **not** converted: they cross the LLM boundary and must match pi exactly (`{ type = "text", text = "..." }`). The conversion applies only at the pillar-API surface.

## The `@pillar` module

```luau
-- ~/.pillar/extensions/greet.luau
--!strict
local pillar = require("@pillar")

pillar.on("tool_call", function(event, ctx)
  if event.tool_name == "bash" and event.input.command:find("rm -rf", 1, true) then
    local ok = ctx.ui.confirm("Dangerous!", "Allow rm -rf?")
    if not ok then
      return { block = true, reason = "Blocked by user" }
    end
  end
end)

pillar.register_tool({
  name = "greet",
  label = "Greet",
  description = "Greet someone by name",
  parameters = pillar.schema.object({
    name = pillar.schema.string({ description = "Name to greet" }),
  }),
  execute = function(tool_call_id, params, signal, on_update, ctx)
    return {
      content = { { type = "text", text = "Hello, " .. params.name .. "!" } },
      details = {},
    }
  end,
})

pillar.register_command("hello", {
  description = "Say hello",
  handler = function(args, ctx)
    ctx.ui.notify("Hello " .. (args or "world") .. "!", "info")
  end,
})
```

### Registration methods

| pi (`ExtensionAPI`) | pillar (`@pillar`) |
| --- | --- |
| `on(event, handler)` | `pillar.on(event, handler)` |
| `registerTool(def)` | `pillar.register_tool(def)` |
| `registerCommand(name, opts)` | `pillar.register_command(name, opts)` |
| `registerShortcut(key, opts)` | `pillar.register_shortcut(key, opts)` |
| `registerFlag(name, opts)` | `pillar.register_flag(name, opts)` |
| `getFlag(name)` | `pillar.get_flag(name)` |
| `registerMessageRenderer(type, fn)` | `pillar.register_message_renderer(type, fn)` |
| `registerEntryRenderer(type, fn)` | `pillar.register_entry_renderer(type, fn)` |
| `registerMarkdownTransformer(fn)` | `pillar.register_markdown_transformer(fn)` |
| `sendMessage(msg)` | `pillar.send_message(msg)` |
| `sendUserMessage(msg)` | `pillar.send_user_message(msg)` |
| `appendEntry(type, data?)` | `pillar.append_entry(type, data?)` |
| `setSessionName(name)` / `getSessionName()` | `pillar.set_session_name(name)` / `pillar.get_session_name()` |
| `setLabel(entryId, label?)` | `pillar.set_label(entry_id, label?)` |
| `exec(cmd, args, opts?)` | `pillar.exec(cmd, args, opts?)` (async; yields) |
| `getActiveTools()` / `getAllTools()` / `setActiveTools(names)` | `pillar.get_active_tools()` / `pillar.get_all_tools()` / `pillar.set_active_tools(names)` |
| `getCommands()` | `pillar.get_commands()` |
| `setModel(m)` / `getThinkingLevel()` / `setThinkingLevel(l)` | `pillar.set_model(m)` / `pillar.get_thinking_level()` / `pillar.set_thinking_level(l)` |
| `registerProvider(p)` / `unregisterProvider(name)` | `pillar.register_provider(p)` / `pillar.unregister_provider(name)` |
| `events` (EventBus) | `pillar.events` (emit/on with same semantics) |

### Events

All 36 pi events, same names, same firing order, same payloads (keys snake_cased per the table above):

`project_trust`, `resources_discover`, `session_start`, `session_info_changed`, `session_before_switch`, `session_before_fork`, `session_before_compact`, `session_compact`, `session_compact_failed`, `session_shutdown`, `session_before_tree`, `session_tree`, `context`, `before_provider_request`, `before_provider_headers`, `after_provider_response`, `before_agent_start`, `agent_start`, `agent_end`, `agent_settled`, `ui_prompt_start`, `ui_prompt_end`, `turn_start`, `turn_end`, `message_start`, `message_update`, `message_end`, `tool_execution_start`, `tool_execution_update`, `tool_execution_end`, `model_select`, `thinking_level_select`, `tool_call`, `tool_result`, `user_bash`, `input`.

Handler semantics preserved exactly:

- A handler returning `{ block = true, reason = "..." }` from `tool_call` blocks the tool call, matching pi's `ToolCallEventResult`.
- Handlers run in registration order; a handler's returned modifications feed the next handler.
- An error thrown from a handler is caught by the runtime, reported like pi's extension error path, and does not prevent other handlers from running.

### `ctx.ui`

| pi (`ctx.ui.*`) | pillar (`ctx.ui.*`) |
| --- | --- |
| `select(title, options, opts?)` | `ctx.ui.select(title, options, opts?)` → `string?` |
| `confirm(title, message, opts?)` | `ctx.ui.confirm(title, message, opts?)` → `boolean` |
| `input(title, placeholder?, opts?)` | `ctx.ui.input(title, placeholder?, opts?)` → `string?` |
| `editor(title, prefill?)` | `ctx.ui.editor(title, prefill?)` → `string?` |
| `notify(message, type?)` | `ctx.ui.notify(message, type?)` (`"info" \| "warning" \| "error"`) |
| `custom(...)` | `ctx.ui.custom(...)` (component factory receives the TUI bridge) |
| `setStatus(key, text?)` | `ctx.ui.set_status(key, text?)` |
| `setWidget(key, lines?, opts?)` | `ctx.ui.set_widget(key, lines?, opts?)` |
| `setFooter(...)` / `setHeader(...)` | `ctx.ui.set_footer(...)` / `ctx.ui.set_header(...)` |
| `setTitle(t)` | `ctx.ui.set_title(t)` |
| `setTheme`/`getTheme`/`getAllThemes` | `ctx.ui.set_theme` / `ctx.ui.get_theme` / `ctx.ui.get_all_themes` |
| `setEditorText`/`getEditorText`/`pasteToEditor` | `ctx.ui.set_editor_text` / `ctx.ui.get_editor_text` / `ctx.ui.paste_to_editor` |
| `setWorkingIndicator`/`setWorkingMessage`/`setWorkingVisible` | same names snake_cased |
| `onTerminalInput(handler)` | `ctx.ui.on_terminal_input(handler)` |
| `theme` (property) | `ctx.ui.theme` (table) |

## Schema system (`pillar.schema`)

pi uses typebox for tool parameter schemas; typebox types are plain JSON-Schema-generating objects, so pillar uses JSON Schema directly with builder helpers:

```luau
pillar.schema.string({ description = "..." })
pillar.schema.number({ minimum = 0 })
pillar.schema.boolean()
pillar.schema.enum({ "low", "medium", "high" })
pillar.schema.array(item)
pillar.schema.object({ name = pillar.schema.string() }, { required = { "name" } })
```

Builders return plain tables with a hidden metatable tagged `__pillar_schema`; the host converts them to JSON Schema before tool registration. Fields must serialize identically to the typebox output for the same shape (same `type`, `description`, `required` ordering semantics).

## Host↔Lua value bridge

Conversion rules for every value crossing the bridge (both directions):

| Rust | Luau |
| --- | --- |
| `None`/`null` | `nil` |
| `bool` | `boolean` |
| `i64`/`f64` | `number` (Luau numbers are f64; integer precision beyond 2^53 is not preserved — pi payloads never contain such integers) |
| `String` | `string` |
| `Vec<T>` | array table (1-indexed, `#`-countable) |
| `struct`/`Map` | table |
| `serde_json::Value` | table or scalar per shape |
| function/callback handles | `function` (host wraps the Rust side) |

Return directions follow the same table. Handler return values that pi types as `X \| undefined` accept `nil` in Luau. Malformed returns (wrong shape) raise a catchable error with a message naming the expected shape.

## Async semantics

- Handler functions may be synchronous or coroutines (`coroutine`-based). The host drives coroutines to completion, awaiting host calls (`pillar.exec`, `ctx.ui.confirm`) that yield.
- Rust futures exposed to Luau (from `luaur-rt`'s `async` feature) are bridged: an async host call returns a promise-like handle that a handler can `coroutine.yield` on; the runtime resumes the coroutine on completion. Extension-facing behavior matches pi's `await`: sequential, error-propagating.
- Event delivery order is identical to pi: synchronous dispatch on the agent thread for non-async handlers; async handlers do not delay subsequent events beyond what pi's own async handlers allow.

## Session persistence & custom entries

- `pillar.append_entry(custom_type, data)` writes the same `CustomEntry` shape pi writes (`customMessage` role `custom`, `customType`, `data` serialized through the value bridge).
- Custom entries render via `register_message_renderer`/`register_entry_renderer` registrations; renderers receive the entry data as a table.

## Type-checking gates

- `luaur-analysis` runs on every extension file at load with the `@pillar`/`@pillar.tui` definition files (`.d.luau` shipped inside `pillar-extensions/src/definitions/`).
- CI runs the definition files against luaur's own conformance suite plus pillar's extension test corpus (see [05-testing-parity.md](05-testing-parity.md)).
- A definition-file change that breaks type-checking of any corpus file is a breaking change: bump the definitions version and document it.

## Feature parity ledger

Extensions features intentionally NOT ported (with replacement):

| pi feature | pillar replacement |
| --- | --- |
| jiti TypeScript loading, npm imports in extensions | Luau modules; no npm. Sharing via git packages works the same (`packages` in settings) |
| Direct access to `@earendil-works/pi-tui` component classes from extensions | `ctx.ui.custom` receives a Luau-facing TUI bridge covering the same component surface; parity tracked per component in the ledger |
| Node.js builtins (`node:fs`, …) inside extensions | `pillar.fs` module (bounded API: read/write/list/stat), same trust model as tool calls |
| Dynamic `import()` of other extension files | `require("@ext/<name>")` for files in the same discovery root |

Everything else in `docs/extensions.md`'s "Key capabilities" list is ported: custom tools, event interception, user interaction, custom UI components, custom commands, session persistence, custom rendering.
