//! Entry point of the `pillar` binary (upstream `cli.ts` + the `main.ts`
//! bootstrap). The argument/help/version surface and the mode resolution are
//! ported; the runtime bootstrap (settings/session/services/AgentSession) and
//! the interactive/rpc/print modes are pending (docs/TASKS.md).

use std::io::IsTerminal;
use std::process::ExitCode;

use pillar_coding_agent::cli::args::{DiagnosticKind, Mode, VERSION, parse_args};
use pillar_coding_agent::cli::help::render_help;
use pillar_coding_agent::cli::main::{AppMode, resolve_app_mode};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let parsed = parse_args(&args);

    let mut has_error = false;
    for diagnostic in &parsed.diagnostics {
        match diagnostic.kind {
            DiagnosticKind::Error => {
                eprintln!("Error: {}", diagnostic.message);
                has_error = true;
            }
            DiagnosticKind::Warning => eprintln!("Warning: {}", diagnostic.message),
        }
    }
    if has_error {
        return ExitCode::from(1);
    }

    if parsed.version == Some(true) {
        println!("{VERSION}");
        return ExitCode::SUCCESS;
    }

    if parsed.help == Some(true) {
        print!("{}", render_help(&[]));
        return ExitCode::SUCCESS;
    }

    if parsed.mode == Some(Mode::Rpc) && !parsed.file_args.is_empty() {
        eprintln!("Error: @file arguments are not supported in RPC mode");
        return ExitCode::from(1);
    }

    let app_mode = resolve_app_mode(
        &parsed,
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    );
    let _ = AppMode::Interactive;
    eprintln!(
        "Error: `{}` mode is not ported yet (only --version/--help are available)",
        app_mode.as_str()
    );
    ExitCode::from(1)
}
