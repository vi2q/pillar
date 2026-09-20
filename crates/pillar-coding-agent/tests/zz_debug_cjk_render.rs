//! Temporary reproduction: render an assistant message with CJK + inline code
//! through the real theme and the real transcript path.

use std::sync::Mutex;

use pillar_ai::types::{AssistantMessage, Content, Message, StopReason, Usage, UsageCost};
use pillar_coding_agent::core::messages::CodingAgentMessage;
use pillar_coding_agent::modes::interactive::transcript::{
    InteractiveTranscript, TranscriptSettings,
};
use pillar_tui::tui::Component as _;

static THEME_LOCK: Mutex<()> = Mutex::new(());

fn usage() -> Usage {
    Usage {
        input: 10,
        output: 10,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 20,
        cost: UsageCost::default(),
    }
}

fn assistant(text: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![Content::Text {
            text: text.to_string(),
            text_signature: None,
        }],
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        model: "claude-sonnet-4-5".to_string(),
        response_model: None,
        usage: usage(),
        stop_reason: StopReason::Stop,
        deferred: None,
        error_message: None,
        response_id: None,
        diagnostics: Vec::new(),
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 1,
    }
}

#[test]
fn assistant_inline_code_with_cjk_survives_real_theme() {
    let _guard = THEME_LOCK.lock().expect("lock");
    pillar_coding_agent::modes::interactive::theme::init_theme(Some("dark"));

    let text = "\u{5909}\u{66f4}\u{304c}\u{7121}\u{3044}/\u{672b}\u{5c3e}\u{3060}\u{3051}\u{5909}\u{5316}\u{306e}\u{3068}\u{304d} chat `Container` \u{304c}\u{30ad}\u{30e3}\u{30c3}\u{30b7}\u{30e5}\u{3092}\u{8fd4}\u{3059}\u{ff08}clone \u{3092} `Arc<[String]>` \u{5316}\u{3057}\u{3066}\u{6d88}\u{3059}\u{ff09}\u{3002}";

    for width in 20..80 {
        let mut transcript =
            InteractiveTranscript::new(TranscriptSettings::default(), None, Vec::new(), "/tmp");
        transcript.add_message_to_chat(
            CodingAgentMessage::Base(Message::Assistant(Box::new(assistant(text)))),
            false,
        );
        let lines = transcript.chat.render(width);
        let joined: String = lines
            .iter()
            .map(|l| pillar_tui::text_utils::strip_terminal_sequences(l))
            .collect::<Vec<_>>()
            .join("");
        let joined = joined.replace(' ', "");
        assert!(
            joined.contains("Container"),
            "width={width}: `Container` lost; got {joined:?}"
        );
        assert!(
            joined.contains("Arc<[String]>"),
            "width={width}: `Arc` lost; got {joined:?}"
        );
    }
}
