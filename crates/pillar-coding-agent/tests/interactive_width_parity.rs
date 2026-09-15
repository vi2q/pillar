//! Regression: every rendered transcript line must fit the terminal width.
//!
//! Upstream `tui-main-screen.ts` aborts with "Rendered line N exceeds
//! terminal width" when a component emits a wider line; the cases that used
//! to trip this port were the write tool preview (a multi-line string stored
//! as a single line) and CJK text (wide graphemes).

use std::sync::Mutex;

use pillar_agent::types::AgentMessage;
use pillar_ai::types::{
    AssistantMessage, Content, Message, StopReason, Usage, UsageCost,
};
use pillar_coding_agent::core::messages::CodingAgentMessage;
use pillar_coding_agent::modes::interactive::transcript::{
    InteractiveTranscript, TranscriptSettings,
};
use pillar_tui::text_utils::{strip_terminal_sequences, visible_width};
use pillar_tui::tui::Component as _;

static THEME_LOCK: Mutex<()> = Mutex::new(());

fn assistant_message(content: Vec<Content>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        content,
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        model: "claude-sonnet-4-5".to_string(),
        response_model: None,
        usage: Usage {
            input: 10,
            output: 10,
            cache_read: 0,
            cache_write: 0,
            cache_write_1h: None,
            reasoning: None,
            total_tokens: 20,
            cost: UsageCost::default(),
        },
        stop_reason,
        deferred: None,
        error_message: None,
        response_id: None,
        diagnostics: Vec::new(),
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 1,
    }
}

/// Every rendered line fits `width` (the renderer's invariant).
fn assert_fits(transcript: &mut InteractiveTranscript, width: usize) {
    let lines = transcript.chat.render(width);
    assert!(!lines.is_empty());
    for (index, line) in lines.iter().enumerate() {
        let plain = strip_terminal_sequences(line);
        let visible = visible_width(&plain);
        assert!(
            visible <= width,
            "line {index} is {visible} wide (limit {width}): {plain:?}"
        );
    }
}

#[test]
fn cjk_and_multiline_tool_output_fit_the_width() {
    let _guard = THEME_LOCK.lock().expect("lock");
    pillar_coding_agent::modes::interactive::theme::init_theme(Some("dark"));
    let mut transcript =
        InteractiveTranscript::new(TranscriptSettings::default(), None, Vec::new(), "/tmp");

    // User message (the one from the real session).
    transcript.add_message_to_chat(
        CodingAgentMessage::Base(Message::User {
            content: pillar_ai::types::UserContent::Text(
                "docsに自己モデルの紹介をmdで三行ぐらいで作成して".to_string(),
            ),
            timestamp: 1,
        }),
        false,
    );
    assert_fits(&mut transcript, 109);
    assert_fits(&mut transcript, 80);

    // Assistant streaming with thinking + text.
    transcript.handle_event(&pillar_coding_agent::core::agent_session_class::AgentSessionEvent::MessageStart {
        message: AgentMessage::Message(Message::Assistant(Box::new(assistant_message(
            Vec::new(),
            StopReason::Pending,
        )))),
    });
    transcript.handle_event(&pillar_coding_agent::core::agent_session_class::AgentSessionEvent::MessageUpdate {
        message: AgentMessage::Message(Message::Assistant(Box::new(assistant_message(
            vec![
                Content::Thinking {
                    thinking: "ユーザーはdocsに自己モデルの紹介をmdで三行ぐらいで作成してほしいと言っている。まずdocsディレクトリを確認しよう。".to_string(),
                    thinking_signature: None,
                    redacted: None,
                },
                Content::text("はい、応答できます！\n\n現在 /Users/vie/Desktop/pillar というディレクトリで作業できる状態です。ファイルの読み書き・編集、コマンド実行などが可能です。\n\n何かお手伝いできることはありますか？"),
                Content::ToolCall {
                    id: "tc-1".to_string(),
                    name: "bash".to_string(),
                    arguments: serde_json::json!({"command": "ls docs/ 2>/dev/null || echo \"docs ディレクトリなし\""}),
                    thought_signature: None,
                    namespace: None,
                },
                Content::ToolCall {
                    id: "tc-2".to_string(),
                    name: "write".to_string(),
                    arguments: serde_json::json!({
                        "file_path": "/Users/vie/Desktop/pillar/docs/自己モデル紹介.md",
                        "content": "# 自己モデル紹介\n\n私は **Omen Alpha** が動作しているコーディングアシスタントです。ファイルの読み書き・編集、コマンド実行などを通じて開発を支援します。\n\n日本語でのやり取りにも対応しています。\n"
                    }),
                    thought_signature: None,
                    namespace: None,
                },
            ],
            StopReason::Pending,
        )))),
        assistant_message_event: Box::new(pillar_ai::types::AssistantMessageEvent::Start {
            partial: assistant_message(Vec::new(), StopReason::Pending),
        }),
    });
    assert_fits(&mut transcript, 109);
    assert_fits(&mut transcript, 80);
    assert_fits(&mut transcript, 40);
}
