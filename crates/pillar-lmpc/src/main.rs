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
    // `--stream`: the scripted host streams its answer in deltas first.
    let stream = args.peek().map(String::as_str) == Some("--stream");
    if stream {
        let _ = args.next();
    }
    // `--resume`: store the conversation after the first turn and continue it in
    // a new session (the host-side persistence round trip).
    let resume = args.peek().map(String::as_str) == Some("--resume");
    if resume {
        let _ = args.next();
    }
    // `--host-tools`: the guest's tool calls run in the host (engine actions).
    let host_tools = args.peek().map(String::as_str) == Some("--host-tools");
    if host_tools {
        let _ = args.next();
    }
    let prompts: Vec<String> = args.collect();
    let prompts = if prompts.is_empty() {
        vec!["remember something".to_string()]
    } else {
        prompts
    };
    let trace = if host_tools {
        pillar_lmpc::host_tools_demo_turn(&prompts[0])?
    } else if host_model && resume {
        pillar_lmpc::host_model_resume_demo(&prompts)?
    } else if host_model {
        // Several prompts mean several turns on one session (the NPC keeps its
        // state), which is what the Wasm host does too.
        pillar_lmpc::host_model_demo_turns_with(&prompts, stream)?
    } else {
        pillar_lmpc::demo_turn(&prompts[0])?
    };
    print!("{}", pillar_lmpc::trace_text(&trace));
    Ok(())
}
