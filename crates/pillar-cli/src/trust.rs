//! Startup project-trust resolution.
//!
//! The decision must exist before any project-local resource is read:
//! `.pillar/extensions` is evaluated Luau with `pi.exec` / `pillar.fs`
//! available and `.pillar/settings.json` can change agent behavior, so an
//! untrusted checkout must never reach either
//! (docs/ARCHITECTURE-REVIEW-s05c0.md A).

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use pillar_coding_agent::core::trust_manager::{
    ProjectTrustStore, get_project_trust_options, has_trust_requiring_project_resources,
};

/// The trust decision for `cwd`: `--approve` / `--no-approve` wins, then the
/// persisted decision, else deny. Never prompts.
pub fn stored_project_trust(cwd: &str, agent_dir: &str, override_decision: Option<bool>) -> bool {
    override_decision.unwrap_or_else(|| {
        ProjectTrustStore::new(Path::new(agent_dir))
            .get(cwd)
            .unwrap_or(false)
    })
}

/// Resolve the trust decision for the startup project, asking the user on the
/// terminal when the project has trust-requiring resources and no decision is
/// stored (upstream the CLI trust prompt). A non-terminal stdin never blocks
/// on a prompt; anything unreadable or out of range is treated as "not trusted
/// for this session".
pub fn resolve_project_trust(
    cwd: &str,
    agent_dir: &str,
    override_decision: Option<bool>,
    interactive: bool,
) -> bool {
    if let Some(decision) = override_decision {
        return decision;
    }
    let store = ProjectTrustStore::new(Path::new(agent_dir));
    if let Some(decision) = store.get(cwd) {
        return decision;
    }
    if !interactive
        || !std::io::stdin().is_terminal()
        || !has_trust_requiring_project_resources(cwd)
    {
        return false;
    }
    prompt_and_store(cwd, &store)
}

/// The `cwd`'s project-local extension directory, when the project may load
/// it.
pub fn project_extension_dir(cwd: &str, trusted: bool) -> Option<std::path::PathBuf> {
    let dir = Path::new(cwd).join(".pillar").join("extensions");
    (trusted && dir.is_dir()).then_some(dir)
}

fn prompt_and_store(cwd: &str, store: &ProjectTrustStore) -> bool {
    let options = get_project_trust_options(cwd, true);
    println!();
    println!("Project resources in {cwd} can run code or change agent behavior.");
    for (index, option) in options.iter().enumerate() {
        println!("  {}) {}", index + 1, option.label);
    }
    print!("Choose an option [1-{}]: ", options.len());
    let _ = std::io::stdout().flush();

    let mut line = String::new();
    let chosen = if std::io::stdin().lock().read_line(&mut line).is_ok() {
        line.trim().parse::<usize>().ok()
    } else {
        None
    };
    let option = chosen
        .and_then(|number| number.checked_sub(1))
        .and_then(|index| options.get(index));
    let Some(option) = option else {
        println!("Not trusted for this session.");
        return false;
    };
    if option.saved_path.is_some()
        && let Err(error) = store.set_many(option.updates.clone())
    {
        eprintln!("Warning: failed to save the trust decision: {error}");
    }
    option.trusted
}
