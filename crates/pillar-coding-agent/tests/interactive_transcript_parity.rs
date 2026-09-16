//! Parity tests for the transcript half of interactive mode (pi v0.84.3
//! `interactive-mode.ts`): the chat assembly, the streaming event subset and
//! the populate path.

use std::sync::Mutex;

use pillar_agent::types::AgentMessage;
use pillar_ai::types::{
    AssistantMessage, Content, Message, StopReason, Usage, UsageCost, UserContent,
};
use pillar_coding_agent::core::agent_session_class::AgentSessionEvent;
use pillar_coding_agent::core::messages::CodingAgentMessage;
use pillar_coding_agent::core::session_entries::{
    SessionEntry, SessionEntryBase, SessionMessageEntry,
};
use pillar_coding_agent::modes::interactive::transcript::{
    InteractiveTranscript, RenderSessionItem, TranscriptSettings, get_user_message_text,
    tool_execution_result_from_json,
};
use pillar_tui::tui::Component as _;

static THEME_LOCK: Mutex<()> = Mutex::new(());

fn install_dark() {
    pillar_coding_agent::modes::interactive::theme::init_theme(Some("dark"));
}

fn strip_ansi(text: &str) -> String {
    pillar_tui::text_utils::strip_terminal_sequences(text)
}

fn plain(transcript: &mut InteractiveTranscript, width: usize) -> String {
    let lines = transcript.chat.render(width);
    lines
        .iter()
        .map(|line| strip_ansi(line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64, cost: f64) -> Usage {
    Usage {
        input,
        output,
        cache_read,
        cache_write,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: input + output + cache_read + cache_write,
        cost: UsageCost {
            total: cost,
            ..UsageCost::default()
        },
    }
}

fn assistant_message(
    content: Vec<Content>,
    stop_reason: StopReason,
    error_message: Option<&str>,
) -> AssistantMessage {
    AssistantMessage {
        content,
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        model: "claude-sonnet-4-5".to_string(),
        response_model: None,
        usage: usage(10, 10, 0, 0, 0.0),
        stop_reason,
        deferred: None,
        error_message: error_message.map(str::to_string),
        response_id: None,
        diagnostics: Vec::new(),
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 1,
    }
}

fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> Content {
    Content::ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments,
        thought_signature: None,
        namespace: None,
    }
}

fn tool_result(tool_call_id: &str, text: &str) -> CodingAgentMessage {
    CodingAgentMessage::Base(Message::ToolResult(Box::new(
        pillar_ai::types::ToolResultMessage {
            tool_call_id: tool_call_id.to_string(),
            tool_name: "read".to_string(),
            content: vec![Content::text(text)],
            details: None,
            usage: None,
            added_tool_names: Some(Vec::new()),
            is_error: false,
            timestamp: 1,
        },
    )))
}

fn message_entry(message: CodingAgentMessage) -> SessionEntry {
    SessionEntry::Message(SessionMessageEntry {
        base: SessionEntryBase {
            id: format!("e{}", rand_suffix()),
            parent_id: None,
            timestamp: 1,
        },
        message,
    })
}

fn rand_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn make_transcript() -> InteractiveTranscript {
    // The caller holds THEME_LOCK and installed the dark theme.
    InteractiveTranscript::new(TranscriptSettings::default(), None, Vec::new(), "/tmp")
}

// --- streaming lifecycle ------------------------------------------------------------------

#[test]
fn transcript_streams_an_assistant_message_and_tracks_tool_calls() {
    let _guard = THEME_LOCK.lock().expect("lock");
    install_dark();
    let mut transcript = make_transcript();
    // Expanded so the read result body renders (upstream `formatReadResult`
    // hides collapsed read output).
    transcript.set_tool_output_expanded(true);

    transcript.handle_event(&AgentSessionEvent::MessageStart {
        message: AgentMessage::Message(Message::Assistant(Box::new(assistant_message(
            Vec::new(),
            StopReason::Pending,
            None,
        )))),
    });
    assert!(transcript.streaming().is_some());
    assert_eq!(transcript.chat.len(), 1);

    // A streaming tool call creates a pending tool component.
    transcript.handle_event(&AgentSessionEvent::MessageUpdate {
        message: AgentMessage::Message(Message::Assistant(Box::new(assistant_message(
            vec![tool_call(
                "tc-1",
                "read",
                serde_json::json!({"file_path": "/tmp/a.rs"}),
            )],
            StopReason::Pending,
            None,
        )))),
        assistant_message_event: Box::new(pillar_ai::types::AssistantMessageEvent::Start {
            partial: assistant_message(Vec::new(), StopReason::Pending, None),
        }),
    });
    assert!(transcript.pending_tools().contains_key("tc-1"));
    assert_eq!(transcript.chat.len(), 2, "streaming + tool call");
    let body = plain(&mut transcript, 60);
    assert!(body.contains("read /tmp/a.rs"), "{body:?}");

    // A second update with new args updates the same component (no
    // duplicate).
    transcript.handle_event(&AgentSessionEvent::MessageUpdate {
        message: AgentMessage::Message(Message::Assistant(Box::new(assistant_message(
            vec![tool_call(
                "tc-1",
                "read",
                serde_json::json!({"file_path": "/tmp/b.rs"}),
            )],
            StopReason::Pending,
            None,
        )))),
        assistant_message_event: Box::new(pillar_ai::types::AssistantMessageEvent::Start {
            partial: assistant_message(Vec::new(), StopReason::Pending, None),
        }),
    });
    assert_eq!(transcript.pending_tools().len(), 1);
    assert_eq!(transcript.chat.len(), 2, "{:?}", transcript.chat.len());

    // Streaming partial results update the tool component.
    transcript.handle_event(&AgentSessionEvent::ToolExecutionUpdate {
        tool_call_id: "tc-1".to_string(),
        tool_name: "read".to_string(),
        args: serde_json::json!({}),
        partial_result: serde_json::json!({"content": [{"type": "text", "text": "partial!"}]}),
    });
    let body = plain(&mut transcript, 60);
    assert!(body.contains("partial!"), "{body:?}");

    // The end delivers the final result and drops the pending entry.
    transcript.handle_event(&AgentSessionEvent::ToolExecutionEnd {
        tool_call_id: "tc-1".to_string(),
        tool_name: "read".to_string(),
        result: serde_json::json!({"content": [{"type": "text", "text": "final!"}]}),
        is_error: false,
    });
    assert!(transcript.pending_tools().get("tc-1").is_none());
    let body = plain(&mut transcript, 60);
    assert!(body.contains("final!"), "{body:?}");
    assert!(!body.contains("partial!"), "{body:?}");

    // message_end keeps the transcript (the component is still a child);
    // agent_end removes the streaming component.
    transcript.handle_event(&AgentSessionEvent::MessageEnd {
        message: AgentMessage::Message(Message::Assistant(Box::new(assistant_message(
            vec![tool_call(
                "tc-1",
                "read",
                serde_json::json!({"file_path": "/tmp/a.rs"}),
            )],
            StopReason::ToolUse,
            None,
        )))),
    });
    assert!(transcript.streaming().is_none());
    assert_eq!(transcript.chat.len(), 2, "{:?}", transcript.chat.len());
    transcript.handle_event(&AgentSessionEvent::AgentEnd {
        messages: Vec::new(),
        will_retry: false,
    });
    // Upstream clears `streamingComponent` at message_end, so agent_end's
    // removeChild is a no-op here: the component stays in the chat.
    assert_eq!(transcript.chat.len(), 2, "{:?}", transcript.chat.len());
}

#[test]
fn transcript_propagates_abort_errors_to_pending_tools() {
    let _guard = THEME_LOCK.lock().expect("lock");
    install_dark();
    let mut transcript = make_transcript();
    transcript.retry_attempt = 2;

    transcript.handle_event(&AgentSessionEvent::MessageStart {
        message: AgentMessage::Message(Message::Assistant(Box::new(assistant_message(
            Vec::new(),
            StopReason::Pending,
            None,
        )))),
    });
    transcript.handle_event(&AgentSessionEvent::MessageUpdate {
        message: AgentMessage::Message(Message::Assistant(Box::new(assistant_message(
            vec![tool_call(
                "tc-1",
                "bash",
                serde_json::json!({"command": "ls"}),
            )],
            StopReason::Pending,
            None,
        )))),
        assistant_message_event: Box::new(pillar_ai::types::AssistantMessageEvent::Start {
            partial: assistant_message(Vec::new(), StopReason::Pending, None),
        }),
    });

    transcript.handle_event(&AgentSessionEvent::MessageEnd {
        message: AgentMessage::Message(Message::Assistant(Box::new(assistant_message(
            vec![tool_call("tc-1", "bash", serde_json::json!({}))],
            StopReason::Aborted,
            None,
        )))),
    });
    // Every pending tool shows the retry-aware abort message.
    let body = plain(&mut transcript, 80);
    assert!(body.contains("Aborted after 2 retry attempts"), "{body:?}");
    assert!(transcript.pending_tools().is_empty());

    // The streaming assistant message carries the abort notice.
    let body = plain(&mut transcript, 80);
    assert!(body.contains("Aborted after 2 retry attempts"), "{body:?}");
}

// --- populate path ------------------------------------------------------------------------

#[test]
fn transcript_rebuilds_tool_calls_and_matches_results() {
    let _guard = THEME_LOCK.lock().expect("lock");
    install_dark();
    let mut transcript = make_transcript();

    let entries = vec![
        message_entry(CodingAgentMessage::Base(Message::User {
            content: UserContent::Text("run the thing".to_string()),
            timestamp: 1,
        })),
        SessionEntry::Message(SessionMessageEntry {
            base: SessionEntryBase {
                id: "a1".to_string(),
                parent_id: None,
                timestamp: 2,
            },
            message: CodingAgentMessage::Base(Message::Assistant(Box::new(assistant_message(
                vec![
                    Content::text("working"),
                    tool_call(
                        "tc-1",
                        "read",
                        serde_json::json!({"file_path": "/tmp/a.rs"}),
                    ),
                ],
                StopReason::ToolUse,
                None,
            )))),
        }),
        SessionEntry::Message(SessionMessageEntry {
            base: SessionEntryBase {
                id: "a2".to_string(),
                parent_id: None,
                timestamp: 3,
            },
            message: tool_result("tc-1", "the file body"),
        }),
        // A trailing tool call without a result stays pending.
        SessionEntry::Message(SessionMessageEntry {
            base: SessionEntryBase {
                id: "a3".to_string(),
                parent_id: None,
                timestamp: 4,
            },
            message: CodingAgentMessage::Base(Message::Assistant(Box::new(assistant_message(
                vec![tool_call(
                    "tc-2",
                    "bash",
                    serde_json::json!({"command": "ls"}),
                )],
                StopReason::ToolUse,
                None,
            )))),
        }),
    ];

    // Expanded so the read result renders its body (collapsed read
    // results render nothing, upstream `formatReadResult`).
    transcript.set_tool_output_expanded(true);
    transcript.render_session_entries(&entries, None);
    let body = plain(&mut transcript, 70);
    assert!(body.contains("run the thing"), "{body:?}");
    assert!(body.contains("working"), "{body:?}");
    assert!(body.contains("the file body"), "{body:?}");
    assert!(body.contains("ls"), "{body:?}");
    // The un-matched trailing tool call stays pending (upstream moves the
    // rendered pending tools into `pendingTools`).
    assert_eq!(
        transcript.pending_tools().len(),
        1,
        "{:?}",
        transcript
            .pending_tools()
            .keys()
            .cloned()
            .collect::<Vec<_>>()
    );
}

#[test]
fn transcript_renders_skill_block_and_user_message() {
    let _guard = THEME_LOCK.lock().expect("lock");
    install_dark();
    let mut transcript = make_transcript();
    let skill_text = "<skill name=\"commit\" location=\"/skills/commit.md\">\ncommit it\n</skill>\n\nplease commit";
    transcript.add_message_to_chat(
        CodingAgentMessage::Base(Message::User {
            content: UserContent::Text(skill_text.to_string()),
            timestamp: 1,
        }),
        false,
    );
    let body = plain(&mut transcript, 80);
    assert!(body.contains("commit"), "{body:?}");
    assert!(body.contains("please commit"), "{body:?}");
}

// --- custom entries and notices -----------------------------------------------------------

#[test]
fn transcript_splices_custom_entries_before_the_streaming_component() {
    let _guard = THEME_LOCK.lock().expect("lock");
    install_dark();
    let mut transcript = make_transcript();

    // Start streaming first so the custom entry must splice before it.
    transcript.handle_event(&AgentSessionEvent::MessageStart {
        message: AgentMessage::Message(Message::Assistant(Box::new(assistant_message(
            Vec::new(),
            StopReason::Pending,
            None,
        )))),
    });

    let custom_entry = pillar_coding_agent::core::session_entries::CustomEntry {
        base: SessionEntryBase {
            id: "c1".to_string(),
            parent_id: None,
            timestamp: 5,
        },
        custom_type: "my-widget".to_string(),
        data: Some(serde_json::json!({"text": "widget body"})),
    };
    // Without a renderer lookup nothing is added.
    transcript.add_custom_entry_to_chat(&custom_entry);
    assert_eq!(transcript.chat.len(), 1, "{:?}", transcript.chat.len());

    // The lookup is None here, so the entry was skipped. Configure a
    // lookup on a fresh transcript instead.
    let mut transcript = make_transcript();
    transcript.set_entry_renderer_lookup(Some(Box::new(|custom_type: &str| {
        if custom_type == "my-widget" {
            // Upstream `EntryRenderer`: the entry plus render options and
            // the theme; the stub answers a plain text component.
            Some(std::sync::Arc::new(
                |entry: &pillar_coding_agent::core::session_entries::CustomEntry,
                 _options: &pillar_coding_agent::core::extensions_types::EntryRenderOptions,
                 _theme: &pillar_coding_agent::modes::interactive::theme::Theme| {
                    let text = entry
                        .data
                        .as_ref()
                        .and_then(|data| data.get("text"))
                        .and_then(|text| text.as_str())
                        .unwrap_or("");
                    if text.is_empty() {
                        return None;
                    }
                    Some(Box::new(pillar_tui::components::Text::new(
                        &format!("widget body {}", entry.custom_type),
                        0,
                        0,
                    ))
                        as Box<dyn pillar_tui::tui::Component>)
                },
            ))
        } else {
            None
        }
    })));
    transcript.handle_event(&AgentSessionEvent::MessageStart {
        message: AgentMessage::Message(Message::Assistant(Box::new(assistant_message(
            vec![Content::text("streaming body")],
            StopReason::Pending,
            None,
        )))),
    });
    transcript.handle_event(&AgentSessionEvent::EntryAppended {
        entry: SessionEntry::Custom(custom_entry),
    });
    assert_eq!(transcript.chat.len(), 2, "{:?}", transcript.chat.len());
    let lines = transcript.chat.render(60);
    let widget_at = lines
        .iter()
        .position(|line| strip_ansi(line).contains("widget body"))
        .expect("widget rendered");
    let streaming_at = lines
        .iter()
        .position(|line| strip_ansi(line).contains("streaming body"))
        .expect("streaming rendered");
    assert!(
        widget_at < streaming_at,
        "widget must render before the streaming component: {lines:?}"
    );
}

#[test]
fn transcript_tool_result_json_shaping() {
    // Content blocks parse from the wire JSON; unknown blocks are dropped.
    let result = tool_execution_result_from_json(
        &serde_json::json!({
            "content": [
                {"type": "text", "text": "body"},
                {"type": "unknown"},
            ],
            "details": {"a": 1},
        }),
        true,
    );
    assert_eq!(result.content.len(), 1, "{result:?}");
    assert!(result.is_error);
    assert_eq!(result.details, serde_json::json!({"a": 1}));

    // Missing content yields the default (empty) shape.
    let result = tool_execution_result_from_json(&serde_json::json!({}), false);
    assert!(result.content.is_empty());
    assert!(result.details.is_null());
}

// --- user message text --------------------------------------------------------------------

#[test]
fn user_message_text_joins_text_blocks() {
    let message = Message::User {
        content: UserContent::Text("hello".to_string()),
        timestamp: 1,
    };
    assert_eq!(get_user_message_text(&message), "hello");

    let message = Message::User {
        content: UserContent::Blocks(vec![
            Content::text("a"),
            Content::Image {
                data: "z".to_string(),
                mime_type: "image/png".to_string(),
            },
            Content::text("b"),
        ]),
        timestamp: 1,
    };
    assert_eq!(get_user_message_text(&message), "ab");

    let message = Message::Assistant(Box::new(assistant_message(
        Vec::new(),
        StopReason::Stop,
        None,
    )));
    assert_eq!(get_user_message_text(&message), "");
}

#[test]
fn compaction_cost_notice_renders_only_when_notices_are_on() {
    let _guard = THEME_LOCK.lock().expect("lock");
    install_dark();

    let mut transcript = make_transcript();
    transcript.render_session_items(
        vec![RenderSessionItem::CompactionCost {
            kind:
                pillar_coding_agent::modes::interactive::transcript::CompactionCostKind::Compaction,
            usage: usage(20000, 100, 0, 0, 0.02),
        }],
        None,
    );
    assert_eq!(transcript.chat.len(), 0, "notices off by default");

    transcript.settings_mut().show_cache_miss_notices = true;
    transcript.render_session_items(
        vec![RenderSessionItem::CompactionCost {
            kind:
                pillar_coding_agent::modes::interactive::transcript::CompactionCostKind::Compaction,
            usage: usage(20000, 100, 0, 0, 0.02),
        }],
        None,
    );
    let body = plain(&mut transcript, 80);
    assert!(body.contains("Compaction:"), "{body:?}");
    assert!(body.contains("20k tokens billed (~$0.02)"), "{body:?}");
}
