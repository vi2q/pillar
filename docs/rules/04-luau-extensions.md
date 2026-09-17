# 04 — Luau extensions

Luau拡張のAPI対応と契約の参照資料。pi v0.84.3の `packages/coding-agent/docs/extensions.md` / `core/extensions/types.ts` を参照するが、全APIの実装完了を主張するものではない。必要機能上限・built-in同等の進捗／中断・UI分離・Wasm再利用は [開発方針](../DEVELOPMENT-STRATEGY.md) に従う。API対応表と、実装・配線・検証済み状態を区別する。

## Runtime

- VM: `luaur-rt` (luaur v0.1.8)。現在のmanifestで有効なfeaturesは `serde`, `typecheck`, `send`。
- `ExtensionRuntime`は`Lua`を所有し、loader/bridgeは同じruntime handleを共有する。reloadでは新しい世代を構築する。
- Every extension file is type-checked with `luaur-analysis` against the `@pillar` definitions before it runs. Files that fail type-checking are skipped with a warning listing the diagnostics; they do not abort startup. (pi compiles TS with jiti and fails the same way: per-file load error, other extensions continue.)
- Extensions declare `--!strict` at the top; non-strict files still type-check under the checker's non-strict mode.
- Luauからの外部作用はhost API経由。CLIの信頼判定・Effect Brokerと、VMが提供する言語面は別の境界である。Luau採用だけでOS権限が隔離されるとはみなさない。Wasm構成でも最終的な能力・資源制限は外側のhostが強制する。
- VMはディスクもプロセスも持たない。拡張の*ソース*はhostが`SourceReader`（`pillar_extensions::loader::SourceReader`）で供給し（nativeは`pillar-cli::extension_sources::filesystem_source_reader`）、execは`ExecHost` callbackで注入する。`LoadedExtension`は自分のsourceを保持し、setupの再実行でファイルを読み直さない。discovery（どのファイルを読むか）もhost側（`pillar-coding-agent::core::extensions_luau::discover_luau_paths`）の責務で、VM crateに`std::fs`は無い（`dependency_profiles::the_embedding_core_keeps_os_capabilities_behind_features`が機械検査）。

## Discovery locations

| Location | Scope |
| --- | --- |
| `~/.pillar/agent/extensions/*.luau` | Global |
| `~/.pillar/agent/extensions/*/index.luau` | Global (subdirectory) |
| `.pillar/extensions/*.luau` | Project-local |
| `.pillar/extensions/*/index.luau` | Project-local (subdirectory) |

Load order matches pi: global first, then project-local; within a directory, sorted by file name. Loading a project-local extension evaluates its code with `pillar.exec` / `pillar.fs` available, so the CLI resolves project trust before any project resource is read — the interactive startup prompt, `--approve` / `--no-approve`, or the nearest entry in `~/.pillar/agent/trust.json` — and otherwise skips `.pillar/extensions` with a warning. divergence: the `project_trust` extension event itself is not ported; the decision is host-side and never reaches the VM.

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

### Tool `execute` arguments

`execute(tool_call_id, params, signal, on_update, ctx)`（upstreamと同じ順序）:

| 引数 | 内容 |
| --- | --- |
| `signal` | そのtool callのabort signal。`signal.aborted()`がbooleanを返す。`pillar.exec(..., { signal = signal })`へ渡すと、中断・timeoutで実行中のコマンドがkillされる（killされると結果の`killed = true`）。abortはpump/Host側のスレッドから伝わるため、toolのスレッドがコマンド待ちでも届く |
| `on_update(partial)` | 途中結果の通知。`partial`は最終結果と同じ`{ content = {...}, details = ... }`で、agentの`tool_execution_update`イベントとしてUIへ流れる（呼出は同期。toolの完了後の呼出は無視される） |

divergences: piの`AbortSignal`は`aborted`プロパティと`addEventListener`を持ち、signalは`AbortController`で作られてrun全体で共有される。portはtool callごとのsignalをtable（`aborted()`メソッド）として渡し、`on_abort`リスナーは提供しない。signalはそのcallの間だけ有効。

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
| `exec(cmd, args, opts?)` | `pillar.exec(cmd, args, opts?)`。`opts`は`signal`（tool `execute`のsignal）/ `timeout`（ms）/ `cwd`を受け、結果は`{ stdout, stderr, code, killed }`。中断・timeout時はunixでSIGTERM→5秒後SIGKILL（[`pillar_coding_agent::core::exec`](../../crates/pillar-coding-agent/src/core/exec.rs)） |
| `getActiveTools()` / `getAllTools()` / `setActiveTools(names)` | `pillar.get_active_tools()` / `pillar.get_all_tools()` / `pillar.set_active_tools(names)` |
| `getCommands()` | `pillar.get_commands()` |
| `setModel(m)` / `getThinkingLevel()` / `setThinkingLevel(l)` | `pillar.set_model(m)` / `pillar.get_thinking_level()` / `pillar.set_thinking_level(l)` |
| `registerProvider(p)` / `unregisterProvider(name)` | `pillar.register_provider(p)` / `pillar.unregister_provider(name)` |
| `events` (EventBus) | `pillar.events` (emit/on with same semantics) |

### Events

upstreamイベントの参照一覧（全件配線済みという意味ではない。特に`project_trust`は上記のとおりhost側で判断する）:

`project_trust`, `resources_discover`, `session_start`, `session_info_changed`, `session_before_switch`, `session_before_fork`, `session_before_compact`, `session_compact`, `session_compact_failed`, `session_shutdown`, `session_before_tree`, `session_tree`, `context`, `before_provider_request`, `before_provider_headers`, `after_provider_response`, `before_agent_start`, `agent_start`, `agent_end`, `agent_settled`, `ui_prompt_start`, `ui_prompt_end`, `turn_start`, `turn_end`, `message_start`, `message_update`, `message_end`, `tool_execution_start`, `tool_execution_update`, `tool_execution_end`, `model_select`, `thinking_level_select`, `tool_call`, `tool_result`, `user_bash`, `input`.

イベント契約は種別ごとに検証する。`tool_call`の`{ block = true }`はreasonの有無によらず遮断する。一登録一呼出と登録順を維持する。通知・変換連鎖・拒否・回答採用を同じ処理とみなさず、例外を安全判定の許可へ変換しない。

### `ctx`

Every handler receives `(event, ctx)`; a tool's `execute` receives it as its
fifth argument. `ctx` carries the host facts and the UI bridge:

| pi (`ctx.*`) | pillar (`ctx.*`) |
| --- | --- |
| `cwd` | `ctx.cwd` |
| `mode` (`"tui" \| "rpc" \| "json" \| "print"`) | `ctx.mode` |
| `hasUI` | `ctx.hasUI` |
| `ui` | `ctx.ui` (table, see below) |
| `isIdle()` / `sessionManager` | 現在は`ctx.isIdle()`と`ctx.sessionManager.getSessionId()` / `getEntries()`を提供 |
| その他のcontext操作 | 完了状態はruntime・host配線・実経路テストで確認する |

### `ctx.ui`

Without a UI context (print / json modes) every method is a no-op answering
pi's `noOpUIContext` defaults. Requests are queued, not applied inline: an
extension handler runs with the Luau runtime locked, so the port hands the
request to the interactive mode's pump thread. Requests made before the run
loop starts (the usual `session_start` setup) are replayed when it does.

| pi (`ctx.ui.*`) | pillar (`ctx.ui.*`) |
| --- | --- |
| `notify(message, type?)` | `ctx.ui.notify(message, type?)` (`"info" \| "warning" \| "error"`) |
| `setStatus(key, text?)` | `ctx.ui.set_status(key, text?)` |
| `setTitle(t)` | `ctx.ui.set_title(t)` |
| `setWorkingMessage(m?)` / `setWorkingVisible(v)` / `setWorkingIndicator(o?)` | same names snake_cased |
| `setHiddenThinkingLabel(l?)` | `ctx.ui.set_hidden_thinking_label(l?)` |
| `setEditorText(t)` / `pasteToEditor(t)` | `ctx.ui.set_editor_text(t)` / `ctx.ui.paste_to_editor(t)` |
| `getToolsExpanded()` / `setToolsExpanded(v)` | `ctx.ui.set_tools_expanded(v)` (`get_` not ported yet) |
| `getAllThemes()` | `ctx.ui.get_all_themes()` → `{ { name = string, path = string? } }` |
| `theme` (property) | `ctx.ui.theme` (table: `name`, `mode`, `fg`, `bg`, `bold`, `italic`, `underline`, `strikethrough`, `inverse`) |
| `confirm` / `select` / `input` | ported: a dialog in the editor slot, answered through a per-request id with a timeout that cancels it (`ctx.ui.confirm` → boolean, `select` → the option or `null`, `input` → the text or `null`) |
| `editor` | ported: the multi-line editor seeded with `initialText`; `tui.input.submit` submits, `tui.input.newLine` adds a line, Escape answers `null` |
| `custom(factory, options?)` | ported: the factory receives `(tui, theme, keybindings, done)` and returns a component table (`render(width)` → lines, `handle_input(data)`, `dispose`); `done(result)` answers the call. divergence: the port runs the component on the extension's thread, so `tui` exposes only `requestRender` (a no-op) and `keybindings.matches` asks the host, which resolves the user's `keybindings.json` (edits to that file apply on the next start); `overlay` / `overlayOptions` / `onHandle` are not ported (the component fills the editor slot), and a component awaited before the interactive run loop starts cancels after the same 600 s bound the dialogs use |
| `setWidget` / `setFooter` / `setHeader` / `setEditorComponent` | not ported yet (TASKS 2f) |
| `getEditorText()` / `getTheme(name)` / `setTheme(name)` / `onTerminalInput` / `addAutocompleteProvider` | not ported yet (TASKS 2d-b/2d-c) |

`ctx.ui.theme` mirrors pi's `Theme`: `theme.fg(name, text)` / `theme.bg(name,
text)` colour with a theme colour name (an unknown name renders plain), and
the attribute helpers wrap text in the corresponding ANSI codes. `theme.name`
/ `theme.mode` are `nil` / empty before a theme is loaded.

divergence: upstream reads the live TUI theme; the port asks the host through
`pillar_extensions_contract::ThemeProvider`, which the app installs on
`HostApi` (`pillar-cli` wires
`pillar_coding_agent::modes::interactive::theme::contract_provider()`). The VM
therefore neither names the theme module nor lists theme directories itself;
without a provider every colour name renders plain and `getAllThemes()` is
empty.

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
| absent optional field | omitted key (`nil` in Luau; the host drops `null` keys it would otherwise send) |
| JSON `null` nested inside extension data | empty table (`{}` — luaur's `null` representation, not `nil`) |
| `bool` | `boolean` |
| `i64`/`f64` | `number` (Luau numbers are f64; integer precision beyond 2^53 is not preserved — pi payloads never contain such integers) |
| `String` | `string` |
| `Vec<T>` | array table (1-indexed, `#`-countable) |
| `struct`/`Map` | table |
| `serde_json::Value` | table or scalar per shape |
| function/callback handles | `function` (host wraps the Rust side) |

Return directions follow the same table. Handler return values that pi types as `X \| undefined` accept `nil` in Luau. Malformed returns (wrong shape) raise a catchable error with a message naming the expected shape.

## 実行・非同期の現在地と目標

現在のhost bridgeには同期callbackがある。停止の保証は次の3段で成り立っている。

- **VM 実行予算**: `ExtensionRuntime::set_vm_budget`（既定 `VmBudget::DEFAULT_STEPS`）が interrupt hook を入れ、safepoint ごとに step 数・deadline・**実行中 tool call の abort signal** を見る。host に一切戻らない `while true do end` もここで止まる。
- **待ちの分割**: `ctx.ui.custom` のループはイベント待ちを 25 ms スライスに区切り、各スライスで abort / deadline を確認する（全体の上限は 600 秒だが、abort は 1 スライス以内で待ちを終える）。
- **同期 host 呼出**: `pillar.exec` は同期だが、abort / timeout で子プロセスを SIGTERM→SIGKILL できる。`pillar.fs` は短い処理として扱う。

保持量は **VM メモリ**（`VmBudget.memory_bytes`、既定 256 MiB、safepoint に到達しない単一の巨大確保も `MemoryError` で止まる）と **host 側 registry の登録数**（`HostRegistry::max_registrations`、既定 10,000）で有界。`pillar.exec` の出力 buffer も 4 MiB/stream で切る（`ExecResult.truncated`）。

未完成として残るのは **coroutine 再開による完全な非同期化**（待ちの間 runtime lock を保持しない）。方式は 2 つの probe で確認済み。**採用するのは luaur の `async` feature**（mlua 互換の `create_async_function` / `Function::call_async`）で、host の実行器（CLI は tokio、engine はそのフレームループ）が待つ。エラーの制約: async future が `Err` を返すと luaur 0.1.8 は coroutine を park したままにするので、**結果テーブルにエラーを載せて Lua wrapper が `error(...)` を上げる**。

host 側の駆動（実装済み）: bridge は tool の execute future で **step ごとに runtime lock を取り**、待つ host 呼出は `HostCallRunner`（`bridge_to_agent_tools_with` の第2引数）に渡す。runner は lock を保持せずに自分の executor で実行する（CLI は `EffectBroker` の exec を `spawn_blocking`）。runner を渡さない既定は lock 下で inline 実行（state machine 以前と同じ挙動）。

採用した協調プロトコル（実装済み）: tool call は**専用 coroutine**で走り、`pillar.exec` は coroutine 内で `coroutine.yield(request)` する。host は `ExtensionRuntime::start_tool_call` が返す `ToolStep::HostCall` を見て（runtime lock を解放したまま）作業し、`step_tool_call(answer)` で再開する。同期 API の `call_tool` は同じ状態機械を inline で回すだけなので、既存の呼び出し側は変わらない。main state（setup / handler）では yield できないため `pillar.exec` は inline 実行になる。

参考: 手書きプロトコルの probe（`vm_resume_probe.rs`）:

1. 待つ呼出の Luau wrapper が `coroutine.yield(request)` する（request は Lua table）。
2. host が coroutine を resume して request を受け取り、**何も保持せずに**処理する（別スレッドでコマンドを走らせてもよい）。
3. host が結果で resume すると、それが `coroutine.yield` の戻り値になり wrapper が呼び出し元へ返す。
4. abort は `resume_error` で待ちをエラーとして畳む。

制約: **yield は coroutine の中でしか起きない**ため、tool `execute` / event handler を main state ではなく `Thread` で走らせる必要がある（現状は main state で直接 call している）。VM 予算は resume をまたいで残り step を持ち越す。

**host 関数を coroutine で呼ぶときの必須条件**: 値を組み立てる host 関数は **呼び出し元の state** で作らなければならない。`Function::wrap` の closure は state を受け取れず captured した `Lua`（main state）を使うため、coroutine 内では別スタックに push され **VM が引数を返してしまう**（probe: `crates/pillar-extensions/tests/vm_quirk_probe.rs`）。`Lua::create_function` は closure に呼び出し元の `&Lua` を渡すので coroutine でも正しく返る。`pillar.fs.*` も `pillar.exec` と同じ協調プロトコルで yield する（`{kind="fs",op,path,content}`）。存在しないパスは JSON `null` が Lua の `nil` にならない（luaur の null sentinel）ため、答えに `missing` を明示して wrapper が `nil` を返す。**luaur は fork の pin が必須**: エラーで終わった coroutine の register window を丸ごと親 state へ移す `luaur-rt::Thread::finish_resume` のエラー分岐が `lua_xmove` の `api_check` で SIGTRAP するため（fork コミット `2bb4838d` で修正、`UPSTREAM.toml` に pin）。

値を組み立てる host 関数は `create_function` へ移設済み（`pillar.exec` の同期版 / `pillar.fs.read|list|stat` / `sessionManager.getEntries` / `ctx.ui.confirm|select|input|editor` / `custom.next` / `pillar.get_flag` / `get_commands|get_active_tools|get_all_tools` / `pillar.schema.*`）。値を作らない関数（`signal.aborted()` など）は `Function::wrap` のままで問題ない。`spawn_blocking` だけで VM の永久ループを強制停止できるとはみなさない（interrupt hook が実際の停止点）。

## Session persistence & custom entries

- `pillar.append_entry(custom_type, data)` writes the same `CustomEntry` shape pi writes (`customMessage` role `custom`, `customType`, `data` serialized through the value bridge).
- Custom entries render via `register_message_renderer`/`register_entry_renderer` registrations; renderers receive the entry data as a table.

## Custom rendering

`pillar.register_message_renderer(custom_type, renderer)` and
`pillar.register_entry_renderer(custom_type, renderer)` receive the payload
plus options, and answer a **declarative component description** (pi returns a
live `Component` object; Luau cannot express one):

```lua
pillar.register_message_renderer("my-card", function(message, options)
  return {
    lines = {
      { { text = "CARD ", style = "accent" }, { text = message.details.title } },
      "expanded: " .. tostring(options.expanded),
    },
  }
end)
```

| answer | meaning |
| --- | --- |
| `nil` | fall back to the default rendering (messages) / skip the entry (entries) |
| a string | one plain line |
| `{ text = "…", style = "…" }` | one styled line |
| `{ lines = { line, … } }` | one line per entry; a line is a string or a list of `{ text, style }` segments |
| `{ lines = {} }` | nothing to show (skipped) |

`style` is a theme foreground colour name (`text`, `dim`, `accent`, `muted`,
`success`, `error`, `customMessageText`, …); an unknown name renders unstyled.

divergence (Rust contract): upstream's renderer returns a live `Component`;
the port's `MessageRenderer` / `EntryRenderer` answer **themed lines**
(`RenderedLines = Vec<String>`) and the presentation adapter wraps them in its
own component. They also take neutral inputs: `{ CustomMessagePayload,
MessageRenderOptions, &dyn ThemeStyle }` (and the entry equivalent), so the
contract names neither the message/session models nor the theme type. The
transcript builds the payload (`core::extensions_types::message_render_payload`
/ `entry_render_payload`) and the theme lookup (`theme_style_fn`); the VM
forwards them to Lua. That keeps the extension contract free of TUI types, so
`pillar-extensions` does not depend on the terminal layer (the dependency
table in [01-architecture.md](01-architecture.md) is asserted by
`tests/dependency_direction.rs`).

Payloads: a message renderer gets `{ customType, content, display, details,
timestamp }` and `{ expanded, outputPad }`; an entry renderer gets
`{ customType, id, data }` and `{ expanded }`. A renderer that raises shows
pi's `[type] renderer failed: …` notice and never breaks the transcript.

`pillar.register_markdown_transformer(fn)` receives `(markdown, context)` with
`context = { messageType = "user" | "assistant" | "assistant-thinking",
isStreaming, availableWidth }` and answers the rewritten Markdown (or `nil` to
keep it). pi keeps one transformer per extension; a second registration
replaces the first. Transformers run for user and assistant Markdown before
rendering, in extension order.

## Type-checking gates

- 現在のloaderは`runtime.rs::PILLAR_DEFINITIONS`を使ってload前にtype-checkする。定義に`any`があるため、type-check成功だけで全context契約が正しい証拠にはならない。
- 実拡張のstrict corpus、必要なluaur conformance、host配線検証をCIへ接続する計画は [05-testing-parity.md](05-testing-parity.md) と [開発方針](../DEVELOPMENT-STRATEGY.md) を参照。CI実行済みとは主張しない。
- 型契約の変更で既存corpusを壊す場合は互換性への影響を記録し、実行契約と同時に更新する。

## Feature parity ledger

Extensions features intentionally NOT ported (with replacement):

| pi feature | pillar replacement |
| --- | --- |
| jiti TypeScript loading, npm imports in extensions | Luau modules; no npm. Sharing via git packages works the same (`packages` in settings) |
| Direct access to `@earendil-works/pi-tui` component classes from extensions | Luau-facing `ctx.ui.custom`。現在の対応範囲・制限は上のUI表を参照し、全component面の互換を仮定しない |
| Node.js builtins (`node:fs`, …) inside extensions | `pillar.fs` module (bounded API: read/write/list/stat), same trust model as tool calls |
| Dynamic `import()` of other extension files | `require("@ext/<name>")` for files in the same discovery root |

機能の完了は、宣言・実装・host配線・型チェック・成功／失敗／中断の検証が揃った能力ごとに判定する。upstream一覧から「それ以外は全て移植済み」と推定しない。

## VM budget

An extension's Lua runs under a budget (`ExtensionRuntime::set_vm_budget`, default
`VmBudget::DEFAULT_STEPS`): the VM's interrupt callback fires at safepoints (loop
back-edges, calls/returns) and stops the operation when the step budget is spent,
when the host's wall-clock deadline passed, or when the running tool call's abort
signal is set. Each tool call, event dispatch and extension load gets a fresh
budget.

Why it exists: a pure-Lua loop never calls back into the host, so no host timer
or abort can stop it from the outside — without the guard a `while true do end`
in an extension occupies the runtime's VM forever. A host that trusts its
extensions can pass `VmBudget::unlimited()`.
