//! `--help` text for the CLI (upstream `cli/args.ts::printHelp`).
//!
//! divergence: upstream colorizes with chalk and prints directly; the port
//! renders plain text and returns it so callers/tests can snapshot it.

use crate::cli::args::{APP_NAME, ENV_AGENT_DIR, ENV_SESSION_DIR};
use crate::core::extensions_runner::ExtensionFlag;
use crate::core::settings_manager::CONFIG_DIR_NAME;

/// Render the `--help` text. `extension_flags` are the extension-registered
/// CLI flags (name + flag metadata), appended as the upstream
/// "Extension CLI Flags" section.
pub fn render_help(extension_flags: &[(String, ExtensionFlag)]) -> String {
    let extension_flags_text = if extension_flags.is_empty() {
        String::new()
    } else {
        let mut text = String::from("\nExtension CLI Flags:\n");
        for (name, flag) in extension_flags {
            let value = if flag.kind == "string" {
                " <value>"
            } else {
                ""
            };
            let left = format!("  --{name}{value}");
            let description = if flag.description.is_empty() {
                "Registered by extension".to_string()
            } else {
                flag.description.clone()
            };
            text.push_str(&format!("{left:<30}{description}\n"));
        }
        text
    };

    HELP_TEMPLATE
        .replace("%E1%", ENV_AGENT_DIR)
        .replace("%E2%", ENV_SESSION_DIR)
        .replace("%C%", CONFIG_DIR_NAME)
        .replace("%A%", APP_NAME)
        .replace("%EXT%", &extension_flags_text)
}

const HELP_TEMPLATE: &str = r#"%A% - AI coding assistant with read, bash, edit, write tools

Usage:
  %A% [options] [--] [@files...] [messages...]

Commands:
  %A% install <source> [-l]     Install extension source and add to settings
  %A% remove <source> [-l]      Remove extension source from settings
  %A% uninstall <source> [-l]   Alias for remove
  %A% update [source|self|pi]   Update %A%, extensions, or model catalogs
  %A% list                      List installed extensions from settings
  %A% config [-l]               Open TUI to enable/disable package resources (Tab switches scope)
  %A% auth <command>            Print credentials or check provider readiness
  %A% <command> --help          Show help for install/remove/uninstall/update/list/config/auth

Options:
  --provider <name>              Provider name (default: google)
  --model <pattern>              Model pattern or ID (supports "provider/id" and optional ":<thinking>")
  --api-key <key>                API key (defaults to env vars)
  --system-prompt <text>         System prompt (default: coding assistant prompt)
  --append-system-prompt <text>  Append text or file contents to the system prompt (can be used multiple times)
  --mode <mode>                  Output mode: text (default), json, or rpc
  --print, -p                    Non-interactive mode: process prompt and exit
  --continue, -c                 Continue previous session
  --resume, -r                   Select a session to resume
  --session <path|id>            Use specific session file or partial UUID
  --session-id <id>              Use exact project session ID, creating it if missing
  --fork <path|id>               Fork specific session file or partial UUID into a new session
  --session-dir <dir>            Directory for session storage and lookup
  --no-session                   Don't save session (ephemeral)
  --name, -n <name>              Set session display name
  --models <patterns>            Comma-separated model patterns for Ctrl+P cycling
                                 Supports globs (anthropic/*, *sonnet*) and fuzzy matching
  --no-tools, -nt                Disable all tools by default (built-in and extension)
  --no-builtin-tools, -nbt       Disable built-in tools by default but keep extension/custom tools enabled
  --tools, -t <tools>            Comma-separated allowlist of tool names to enable
                                 Applies to built-in, extension, and custom tools
  --exclude-tools, -xt <tools>   Comma-separated denylist of tool names to disable
                                 Applies to built-in, extension, and custom tools
  --thinking <level>             Set thinking level: off, minimal, low, medium, high, xhigh, max
  --extension, -e <path>         Load an extension file (can be used multiple times)
  --no-extensions, -ne           Disable extension discovery (explicit -e paths still work)
  --skill <path>                 Load a skill file or directory (can be used multiple times)
  --no-skills, -ns               Disable skills discovery and loading
  --prompt-template <path>       Load a prompt template file or directory (can be used multiple times)
  --no-prompt-templates, -np     Disable prompt template discovery and loading
  --theme <path>                 Load a theme file or directory (can be used multiple times)
  --use-theme <name[/name]>      Set the initial interactive theme for this run
  --no-themes                    Disable theme discovery and loading
  --no-context-files, -nc        Disable AGENTS.md and CLAUDE.md discovery and loading
  --export <file>                Export session file to HTML and exit
  --list-models [search]         List available models (with optional fuzzy search)
  --verbose                      Force verbose startup (overrides quietStartup setting)
  --tui-mode <mode>              TUI mode: regular (default) or fullscreen
  --approve, -a                  Trust project-local files for this run
  --no-approve, -na              Ignore project-local files for this run
  --offline                      Disable startup network operations (same as PI_OFFLINE=1)
  --                             End option parsing; treat remaining arguments as messages/files
  --help, -h                     Show this help
  --version, -v                  Show version number

Extensions can register additional flags (e.g., --plan from plan-mode extension).%EXT%

Examples:
  # Print a provider API key for an external client
  %A% auth print-api-key --provider openai

  # Print an OAuth bearer token for an external client (refreshes if expired)
  %A% auth print-bearer-token --provider openai-codex

  # Interactive mode
  %A%

  # Interactive mode with initial prompt
  %A% "List all .ts files in src/"

  # Include files in initial message
  %A% @prompt.md @image.png "What color is the sky?"

  # Non-interactive mode (process and exit)
  %A% -p "List all .ts files in src/"

  # Prompt beginning with a dash
  %A% -p -- "- Summarize these points"

  # Multiple messages (interactive)
  %A% "Read package.json" "What dependencies do we have?"

  # Continue previous session
  %A% --continue "What did we discuss?"

  # Start a named session
  %A% --name "Refactor auth module"

  # Use different model
  %A% --provider openai --model gpt-4o-mini "Help me refactor this code"

  # Use model with provider prefix (no --provider needed)
  %A% --model openai/gpt-4o "Help me refactor this code"

  # Use model with thinking level shorthand
  %A% --model sonnet:high "Solve this complex problem"

  # Limit model cycling to specific models
  %A% --models claude-sonnet,claude-haiku,gpt-4o

  # Limit to a specific provider with glob pattern
  %A% --models "github-copilot/*"

  # Cycle models with fixed thinking levels
  %A% --models sonnet:high,haiku:low

  # Start with a specific thinking level
  %A% --thinking high "Solve this complex problem"

  # Read-only mode (no file modifications possible)
  %A% --tools read,grep,find,ls -p "Review the code in src/"

  # Disable one tool while keeping the rest available
  %A% --exclude-tools ask_question

  # Export a session file to HTML
  %A% --export ~/%C%/agent/sessions/--path--/session.jsonl
  %A% --export session.jsonl output.html

Environment Variables:
  ANTHROPIC_AUTH_TOKEN             - Anthropic bearer auth token
  ANTHROPIC_API_KEY                - Anthropic Claude API key
  ANTHROPIC_OAUTH_TOKEN            - Anthropic OAuth token (alternative to API key)
  ANT_LING_API_KEY                 - Ant Ling API key
  OPENAI_API_KEY                   - OpenAI GPT API key
  AZURE_OPENAI_API_KEY             - Azure OpenAI API key
  AZURE_OPENAI_BASE_URL            - Azure OpenAI/Cognitive Services base URL (e.g. https://{resource}.openai.azure.com)
  AZURE_OPENAI_RESOURCE_NAME       - Azure OpenAI resource name (alternative to base URL)
  AZURE_OPENAI_API_VERSION         - Azure OpenAI API version (default: v1)
  AZURE_OPENAI_DEPLOYMENT_NAME_MAP - Azure OpenAI model=deployment map (comma-separated)
  DEEPSEEK_API_KEY                 - DeepSeek API key
  NVIDIA_API_KEY                   - NVIDIA NIM API key
  GEMINI_API_KEY                   - Google Gemini API key
  GROQ_API_KEY                     - Groq API key
  CEREBRAS_API_KEY                 - Cerebras API key
  XAI_API_KEY                      - xAI Grok API key
  FIREWORKS_API_KEY                - Fireworks API key
  TOGETHER_API_KEY                 - Together AI API key
  BASETEN_API_KEY                  - Baseten API key
  OPENROUTER_API_KEY               - OpenRouter API key
  AI_GATEWAY_API_KEY               - Vercel AI Gateway API key
  ZAI_API_KEY                      - ZAI Coding Plan API key (Global)
  ZAI_CODING_CN_API_KEY            - ZAI Coding Plan API key (China)
  MISTRAL_API_KEY                  - Mistral API key
  MINIMAX_API_KEY                  - MiniMax API key
  MOONSHOT_API_KEY                 - Moonshot AI API key
  OPENCODE_API_KEY                 - OpenCode Zen/OpenCode Go API key
  KIMI_API_KEY                     - Kimi For Coding API key
  CLOUDFLARE_API_KEY               - Cloudflare API token (Workers AI and AI Gateway)
  CLOUDFLARE_ACCOUNT_ID            - Cloudflare account id (required for both)
  CLOUDFLARE_GATEWAY_ID            - Cloudflare AI Gateway slug (required for AI Gateway)
  QWEN_TOKEN_PLAN_API_KEY          - Qwen Token Plan API key (international region)
  QWEN_TOKEN_PLAN_CN_API_KEY       - Qwen Token Plan API key (China region)
  XIAOMI_API_KEY                   - Xiaomi MiMo API key (api.xiaomimimo.com billing)
  XIAOMI_TOKEN_PLAN_CN_API_KEY     - Xiaomi MiMo Token Plan API key (China region)
  XIAOMI_TOKEN_PLAN_AMS_API_KEY    - Xiaomi MiMo Token Plan API key (Amsterdam region)
  XIAOMI_TOKEN_PLAN_SGP_API_KEY    - Xiaomi MiMo Token Plan API key (Singapore region)
  AWS_PROFILE                      - AWS profile for Amazon Bedrock
  AWS_ACCESS_KEY_ID                - AWS access key for Amazon Bedrock
  AWS_SECRET_ACCESS_KEY            - AWS secret key for Amazon Bedrock
  AWS_BEARER_TOKEN_BEDROCK         - Bedrock API key (bearer token)
  AWS_REGION                       - AWS region for Amazon Bedrock (e.g., us-east-1)
  %E1% - Config directory (default: ~/%C%/agent)
  %E2% - Session storage directory (overridden by --session-dir)
  PI_PACKAGE_DIR                   - Override package directory (for Nix/Guix store paths)
  PI_OFFLINE                       - Disable startup network operations when set to 1/true/yes
  PI_TELEMETRY                     - Override install telemetry when set to 1/true/yes or 0/false/no
  PI_SHARE_VIEWER_URL              - Base URL for /share command (default: https://pi.dev/session/)

Built-in Tool Names:
  read       - Read file contents
  bash       - Execute bash commands
  powershell - Execute PowerShell commands on Windows
  edit       - Edit files with find/replace
  write      - Write files (creates/overwrites)
  grep       - Search file contents (read-only, off by default)
  find       - Find files by glob pattern (read-only, off by default)
  ls         - List directory contents (read-only, off by default)
"#;
