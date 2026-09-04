//! Host API surface tests (upstream agent-session.test.ts).
use pillar_protocol::ProtocolError;
use pillar_agent::Agent;
use pillar_ai::Provider;
use pillar_tui::TuiError;
use pillar_tui::stack_layout::StackLayout;

#[tokio::test]
async fn agent_end_to_end() {
    let agent = Agent::new();
    let provider = Provider::openai_compatible("model", "base_url");
    let stack_layout = StackLayout::new();
    let result = agent.run();
    assert!(result.is_ok());
}
