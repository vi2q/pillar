//! Run one LMPC turn and print the trace: the artifact's smoke path
//! (`scripts/check.sh`) and a starting point for a game host.

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1).peekable();
    // `--host-model`: the same turn, but the model answers come from a scripted
    // host (`scripts/wasm_host_model.mjs` speaks the same protocol) instead of
    // the crate's own stand-in.
    let host_model = args.peek().map(String::as_str) == Some("--host-model");
    if host_model {
        let _ = args.next();
    }
    let prompt = args.next().unwrap_or_else(|| "remember something".to_string());
    let trace = if host_model {
        pillar_lmpc::host_model_demo_turn(&prompt)?
    } else {
        pillar_lmpc::demo_turn(&prompt)?
    };
    print!("{}", pillar_lmpc::trace_text(&trace));
    Ok(())
}
