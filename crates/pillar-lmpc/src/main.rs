//! Run one LMPC turn and print the trace: the artifact's smoke path
//! (`scripts/check.sh`) and a starting point for a game host.

fn main() -> Result<(), String> {
    let prompt = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "remember something".to_string());
    let trace = pillar_lmpc::demo_turn(&prompt)?;
    print!("{}", pillar_lmpc::trace_text(&trace));
    Ok(())
}
