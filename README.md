# pillar

A terminal coding agent written in Rust. It runs as an interactive TUI (or a
one-shot / JSON / RPC process), reads and edits files, runs shell commands and
searches a codebase, and can be extended with Luau scripts.

pillar is a Rust port of [pi](https://github.com/earendil-works/pi). It keeps
pi's behavior where compatibility matters and adds native search, an embedded
Luau runtime and an embeddable agent runtime.

## Features

**Coding tools**

- `read`, `write`, `edit` (unified-diff patches) and `ls`.
- `grep` and `find` implemented natively — no external `rg`/`find` required.
- `bash` with live streaming, output truncation and full-output spillover to a file.

**Agent loop**

- Streaming responses with per-token rendering and reasoning/thinking blocks.
- Automatic and manual context compaction, plus branch summarization.
- Tool-call execution with diff previews, image output and expand/collapse of long results.

**Terminal UI**

- Multi-line editor with history, file / `@`-attachment / `/`-command completion and paste markers.
- Markdown rendering with syntax highlighting, LaTeX, tables and Kitty-protocol images.
- Themes, a model picker, a session picker, a settings UI and keybinding hints, over a differential renderer with synchronized output.

**Sessions**

- JSONL session storage with resume, fork, clone and a tree view.
- Export a session to a single self-contained HTML file.

**Providers**

- Anthropic, OpenAI (Chat Completions and Responses), Google Gemini and Vertex AI, Mistral, AWS Bedrock, Azure OpenAI and GitHub Copilot.
- Server-sent-event streaming with each provider's tool-call and reasoning format.

**Extensions and customization**

- Luau extensions run in an embedded VM ([luaur](https://github.com/pjankiewicz/luaur)) and are type-checked before they run; they can add tools, slash commands, UI and lifecycle hooks.
- Skills, prompt templates, custom themes and `AGENTS.md` / `CLAUDE.md` context files.

**Modes and embedding**

- Interactive TUI, one-shot print (`-p`), JSON event stream and RPC.
- Client/server protocol crates for driving the agent from another host.
- A minimal runtime (`pillar-lmpc`) that also builds for `wasm32-unknown-unknown`.

## Installation

Rust 1.85 or newer (edition 2024) is required; CI pins 1.91.1.

From git:

```sh
cargo install --git https://github.com/vi2q/pillar pillar-cli
```

From a clone:

```sh
git clone https://github.com/vi2q/pillar
cd pillar
cargo install --path crates/pillar-cli
```

The Luau runtime (a pinned [`vi2q/luaur`](https://github.com/vi2q/luaur) fork)
resolves automatically. To build without it:

```sh
cargo install --path crates/pillar-cli --no-default-features
```

## Usage

```sh
pillar                          # interactive TUI
pillar -p "explain this code"   # one-shot print
pillar --mode json "..."        # JSON event stream
pillar auth --help              # provider credentials
```

Inside the TUI, type `/` for commands such as `/model`, `/resume`, `/compact`,
`/export` and `/settings`.

## Extensions

```sh
pillar install <source>   # install and enable an extension
pillar list               # list installed extensions
pillar config             # enable or disable package resources
```

Bundled example extensions live in `extensions/`. Project-local extensions are
read from `.pillar/extensions/`.

## License

MIT
