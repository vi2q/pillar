//! Port of packages/agent/test/harness/agent-harness-scaffold.test.ts
//! (pi v0.84.3): record-free session gate, defensive-copy configuration,
//! explicit rejection of unfinished operations, and closed-harness
//! reporting.

#![cfg(feature = "harness-tools")]

use pillar_agent::harness::agent_harness::{
    AgentHarness, AgentHarnessOptions, HARNESS_CLOSED_NAME, HARNESS_NOT_IMPLEMENTED_NAME,
    HarnessError, Resources,
};
use pillar_agent::harness::session::memory::{
    InMemorySessionRepo, InMemorySessionStorage, ProvisionedRecord, Session, SessionCreateOptions,
};
use pillar_agent::harness::session::types::{
    OperationIntent, RecordPayload, RecordQuery, SessionMetadata,
};
use pillar_agent::types::AgentMessage;
use pillar_agent::types::thinking::AgentThinkingLevel;
use pillar_ai::types::{Content, Model, UserContent};

fn harness_model(provider: &str, id: &str) -> Model {
    Model {
        id: id.to_owned(),
        name: id.to_owned(),
        provider: provider.to_owned(),
        api: "openai-responses".to_owned(),
        base_url: String::new(),
        reasoning: false,
        thinking_level_map: None,
        input: Vec::new(),
        cost: pillar_ai::types::ModelCost {
            rates: pillar_ai::types::ModelCostRates::default(),
            tiers: None,
        },
        context_window: 1,
        max_tokens: 1,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn create_session(id: &str) -> Session {
    let repo = InMemorySessionRepo::default();
    repo.create(Some(SessionCreateOptions {
        id: Some(id.to_owned()),
        parent_session_id: None,
    }))
    .expect("session create")
}

fn create_session_direct(id: &str) -> Session {
    Session::new(Box::new(InMemorySessionStorage::new(SessionMetadata {
        id: id.to_owned(),
        created_at: 1,
        parent_session_id: None,
    })))
}

fn harness_options(model: Model) -> AgentHarnessOptions {
    AgentHarnessOptions {
        models: None,
        model,
        thinking_level: None,
        active_tool_names: Vec::new(),
        tools: Vec::new(),
        resources: Resources::default(),
        stream_options: pillar_ai::types::SimpleStreamOptionsLike::default(),
        retry: None,
        compaction: None,
        steering_mode: None,
        follow_up_mode: None,
        tool_execution: None,
    }
}

fn create_harness() -> AgentHarness {
    let session = create_session("session");
    let (harness, suspended) = AgentHarness::create(
        harness_options(harness_model("google", "gemini-2.5-flash")),
        session,
    )
    .expect("create");
    assert!(suspended.is_empty());
    harness
}

fn user_message(text: &str) -> AgentMessage {
    AgentMessage::Message(pillar_ai::types::Message::User {
        content: UserContent::Blocks(vec![Content::text(text)]),
        timestamp: 1,
    })
}

fn error_name(error: &HarnessError) -> &'static str {
    match error {
        HarnessError::NotImplemented(_) => HARNESS_NOT_IMPLEMENTED_NAME,
        HarnessError::Closed(_) => HARNESS_CLOSED_NAME,
        HarnessError::Fault(_) => "HarnessFault",
    }
}

/// conformance: "opens only record-free sessions before restore is
/// implemented".
#[test]
fn opens_only_record_free_sessions_before_restore_is_implemented() {
    let session = create_session("session");
    let (mut harness, suspended) = AgentHarness::create(
        harness_options(harness_model("google", "gemini-2.5-flash")),
        session,
    )
    .expect("create");

    assert!(suspended.is_empty());
    assert_eq!(harness.name, "main");
    assert_eq!(harness.get_leaf_id().expect("leaf"), None);

    harness.close();
    assert!(harness.is_closed());

    let recorded = create_session("recorded");
    recorded
        .append_record(ProvisionedRecord {
            id: "run".to_owned(),
            lane: "main".to_owned(),
            payload: RecordPayload::OperationStarted {
                source_leaf_id: None,
                intent: OperationIntent::Run {
                    original_prompt: Vec::new(),
                    initial_messages: Vec::new(),
                    system_prompt_override: None,
                    resume_data: None,
                },
            },
        })
        .expect("append record");
    let error = match AgentHarness::create(
        harness_options(harness_model("google", "gemini-2.5-flash")),
        recorded,
    ) {
        Err(error) => error,
        Ok(_) => panic!("recorded session must be rejected"),
    };
    match &error {
        HarnessError::NotImplemented(not_implemented) => {
            assert_eq!(not_implemented.operation, "create.restore");
        }
        other => panic!("expected HarnessNotImplemented(create.restore), got {other:?}"),
    }
    assert_eq!(error_name(&error), HARNESS_NOT_IMPLEMENTED_NAME);
    let _ = create_session_direct("unused");
}

/// conformance: "keeps scaffold-safe configuration as defensive copies".
#[test]
fn keeps_scaffold_safe_configuration_as_defensive_copies() {
    let mut harness = create_harness();

    let model = harness_model("anthropic", "claude-sonnet-4-5");
    harness.set_model(model.clone());
    assert_eq!(harness.get_model(), model);

    harness.set_thinking_level(AgentThinkingLevel::High);
    assert_eq!(harness.get_thinking_level(), AgentThinkingLevel::High);

    harness.set_active_tools(vec!["one".to_owned()]);
    // The stored copy is unaffected by caller-side mutation semantics: the
    // port moves ownership, the getter clones.
    assert_eq!(harness.get_active_tools(), vec!["one".to_owned()]);
    let read_active_tools = harness.get_active_tools();
    let mut caller_copy = read_active_tools.clone();
    caller_copy.push("mutated".to_owned());
    assert_eq!(harness.get_active_tools(), vec!["one".to_owned()]);

    let tools = vec![pillar_agent::harness::agent_harness::HarnessTool {
        tool: pillar_ai::types::Tool {
            name: "tool".to_owned(),
            description: "Tool".to_owned(),
            parameters: serde_json::json!({}),
            constrained_sampling: None,
        },
        replay: None,
    }];
    harness.set_tools(tools, None);
    assert_eq!(
        harness
            .get_tools()
            .iter()
            .map(|item| item.tool.name.clone())
            .collect::<Vec<_>>(),
        vec!["tool".to_owned()]
    );

    harness.set_resources(Resources {
        skills: vec![pillar_agent::harness::types::Skill {
            name: "skill".to_owned(),
            description: "desc".to_owned(),
            content: "body".to_owned(),
            file_path: "/tmp/SKILL.md".to_owned(),
            disable_model_invocation: false,
        }],
        prompt_templates: vec![pillar_agent::harness::types::PromptTemplate {
            name: "template".to_owned(),
            description: String::new(),
            content: "body".to_owned(),
        }],
    });
    assert_eq!(
        harness
            .get_resources()
            .skills
            .iter()
            .map(|skill| skill.name.clone())
            .collect::<Vec<_>>(),
        vec!["skill".to_owned()]
    );

    harness.set_stream_options(pillar_ai::types::SimpleStreamOptionsLike {
        max_tokens: Some(10),
        ..Default::default()
    });
    assert_eq!(harness.get_stream_options().max_tokens, Some(10));

    harness.set_retry_policy(pillar_ai::retry::RetryPolicy {
        enabled: true,
        max_retries: 2,
        base_delay_ms: 10,
    });
    let policy = harness.get_retry_policy();
    assert!(policy.enabled);
    assert_eq!(policy.max_retries, 2);
    assert_eq!(policy.base_delay_ms, 10);

    harness.set_compaction_settings(
        pillar_agent::harness::compaction::compaction::CompactionSettings {
            enabled: false,
            reserve_tokens: 1,
            keep_recent_tokens: 2,
        },
    );
    let settings = harness.get_compaction_settings();
    assert!(!settings.enabled);
    assert_eq!(settings.reserve_tokens, 1);
    assert_eq!(settings.keep_recent_tokens, 2);

    harness.set_steering_mode(pillar_agent::types::QueueMode::All);
    assert_eq!(
        harness.get_steering_mode(),
        pillar_agent::types::QueueMode::All
    );
    harness.set_follow_up_mode(pillar_agent::types::QueueMode::All);
    assert_eq!(
        harness.get_follow_up_mode(),
        pillar_agent::types::QueueMode::All
    );
}

type UnfinishedOp<'a> = (&'static str, Box<dyn Fn() -> Result<(), HarnessError> + 'a>);

/// conformance: "rejects every unfinished public operation explicitly".
#[test]
fn rejects_every_unfinished_public_operation_explicitly() {
    let harness = create_harness();

    let unfinished: Vec<UnfinishedOp<'_>> = vec![
        ("prompt", Box::new(|| harness.prompt(user_message("hello")))),
        ("skill", Box::new(|| harness.skill("skill", None))),
        (
            "promptFromTemplate",
            Box::new(|| harness.prompt_from_template("template", None)),
        ),
        ("compact", Box::new(|| harness.compact(None))),
        (
            "navigateTree",
            Box::new(|| harness.navigate_tree(None, None)),
        ),
        ("resume", Box::new(|| harness.resume())),
        ("abort", Box::new(|| harness.abort())),
        ("steer", Box::new(|| harness.steer(user_message("steer")))),
        (
            "followUp",
            Box::new(|| harness.follow_up(user_message("follow"))),
        ),
        (
            "nextRun",
            Box::new(|| harness.next_run(user_message("next"))),
        ),
        ("cancelQueued", Box::new(|| harness.cancel_queued("queued"))),
        (
            "recordUsage",
            Box::new(|| harness.record_usage(pillar_ai::types::Usage::default(), None)),
        ),
        ("waitForIdle", Box::new(|| harness.wait_for_idle())),
        (
            "runWhenIdle",
            Box::new(|| harness.run_when_idle(Box::new(|| {}))),
        ),
        ("peekAction", Box::new(|| harness.peek_action().map(|_| ()))),
        (
            "executeAction",
            Box::new(|| harness.execute_action().map(|_| ())),
        ),
        ("runToCompletion", Box::new(|| harness.run_to_completion())),
        ("watch", Box::new(|| harness.watch().map(|_| ()))),
        ("lane", Box::new(|| harness.lane("main").map(|_| ()))),
        (
            "createLane",
            Box::new(|| harness.create_lane("thread", None)),
        ),
        ("lanes", Box::new(|| harness.lanes().map(|_| ()))),
        (
            "watchSession",
            Box::new(|| harness.watch_session().map(|_| ())),
        ),
    ];

    for (operation, invoke) in unfinished {
        let error = invoke().expect_err(&format!("{operation} must be rejected"));
        match &error {
            HarnessError::NotImplemented(not_implemented) => {
                assert_eq!(not_implemented.operation, operation);
            }
            other => panic!("{operation}: expected HarnessNotImplemented, got {other:?}"),
        }
        assert_eq!(error_name(&error), HARNESS_NOT_IMPLEMENTED_NAME);
    }

    // hooks/events registration on the scaffold also reports
    // HarnessNotImplemented with the registry's operation name.
    for (registry, operation) in [(harness.hooks, "hooks.on"), (harness.events, "events.on")] {
        let error = registry.on(false).expect_err("registry must reject");
        match &error {
            HarnessError::NotImplemented(not_implemented) => {
                assert_eq!(not_implemented.operation, operation);
            }
            other => panic!("{operation}: expected HarnessNotImplemented, got {other:?}"),
        }
    }
}

/// conformance: "reports HarnessClosed for unfinished operations after
/// close".
#[test]
fn reports_harness_closed_for_unfinished_operations_after_close() {
    let mut harness = create_harness();
    harness.close();

    let error = harness.prompt(user_message("hello")).expect_err("closed");
    assert_eq!(error_name(&error), HARNESS_CLOSED_NAME);
    let error = harness.wait_for_idle().expect_err("closed");
    assert_eq!(error_name(&error), HARNESS_CLOSED_NAME);
    let error = harness.hooks.on(true).expect_err("closed");
    assert_eq!(error_name(&error), HARNESS_CLOSED_NAME);
    let error = harness.events.on(true).expect_err("closed");
    assert_eq!(error_name(&error), HARNESS_CLOSED_NAME);
    let _ = RecordQuery::default();
}
