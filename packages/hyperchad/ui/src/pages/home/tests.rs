use super::activity::unrepresented_active_invocations;
use super::adapters::{ARTIFACT_ADAPTERS, VISUAL_ADAPTERS, json_panel, render_plugin_visual};
use super::composer::composer;
use super::interactions::interaction_request;
use super::navigation::session_navigation;
use super::permissions::{permission_history, permission_request};
use super::tools::{render_tool_lifecycle, render_tool_result};
use super::transcript::{
    is_superseded_tool_request, item_label, message_content, should_render_transcript_item,
    transcript_item, transcript_item_body,
};
use super::usage::{runtime_usage, usage_transcript_item};
use super::*;
use bcode_session_models::{PluginVisualDescriptor, RuntimeWorkStatus, ToolArtifact, WorkId};
use bcode_session_view_models::{
    ChatMessageView, CompactionView, CompactionViewStatus, InteractionViewSummary,
    PermissionBatchView, PermissionView, PluginVisualView, RuntimeWorkView, SessionViewSnapshot,
    SkillView, SkillViewStatus, TextFormat, ToolArtifactView, ToolInvocationView,
    ToolInvocationViewStatus, ToolResultView, ToolTimingView, TranscriptViewItem,
    TranscriptViewItemId, TranscriptViewItemKind,
};

fn container_text(container: &hyperchad_transformer::Container, text: &mut String) {
    if let hyperchad_transformer::Element::Text { value } = &container.element {
        text.push_str(value);
    }
    for child in &container.children {
        container_text(child, text);
    }
}

fn exact_context_occupancy(tokens: u64) -> bcode_session_models::RequestContextOccupancy {
    bcode_session_models::RequestContextOccupancy {
        context_epoch: 0,
        observation_sequence: 41,
        observation: bcode_session_models::RequestContextObservation {
            request: bcode_session_models::ModelRequestIdentity {
                provider_plugin_id: "provider.example".to_owned(),
                requested_model_id: None,
                effective_model_id: "model-example".to_owned(),
                request_id: "request".to_owned(),
                model_turn_id: "turn-1".to_owned(),
                round: 0,
                request_fingerprint: "fingerprint".to_owned(),
                effective_auth_profile: None,
                context_format_version: None,
                compatibility_key: None,
                context_epoch: 0,
            },
            context_through_sequence: 40,
            context_tokens: bcode_session_models::RequestContextTokenCount::ProviderExact(tokens),
            local_estimate: bcode_session_models::LocalContextEstimate {
                tokens,
                algorithm_version: 1,
            },
        },
    }
}

#[test]
fn usage_and_context_render_compact_summaries_with_secondary_details() {
    let usage = bcode_session_models::SessionTokenUsage {
        input_tokens: Some(100),
        output_tokens: Some(25),
        total_tokens: Some(125),
        cached_input_tokens: Some(40),
        cache_write_input_tokens: Some(10),
        reasoning_tokens: Some(5),
    };
    let runtime = bcode_session_view_models::SessionRuntimeViewState {
        context_occupancy: Some(exact_context_occupancy(8_000)),
        cumulative_metered_tokens: 12_345,
        latest_usage: Some(usage.clone()),
        ..bcode_session_view_models::SessionRuntimeViewState::default()
    };
    let usage = bcode_session_view_models::UsageView {
        turn_id: "turn-1".to_owned(),
        usage,
    };

    let runtime_containers = runtime_usage(&runtime);
    let transcript_containers = usage_transcript_item(&usage);
    let mut runtime_text = String::new();
    let mut transcript_text = String::new();
    for container in &runtime_containers {
        container_text(container, &mut runtime_text);
    }
    for container in &transcript_containers {
        container_text(container, &mut transcript_text);
    }
    assert!(runtime_text.contains("current context"));
    assert!(runtime_text.contains("8000 tokens"));
    assert!(runtime_text.contains("measured"));
    assert!(runtime_text.contains("12345 tokens"));
    assert!(runtime_text.contains("usage details"));
    assert!(transcript_text.contains("Model usage"));
    assert!(transcript_text.contains("125 tokens"));
    assert!(transcript_text.contains("token breakdown"));
    assert!(transcript_text.contains("cached input"));
}

#[test]
fn empty_and_long_messages_render_bounded_explicit_states() {
    let empty = ChatMessageView::plain("");
    let long = ChatMessageView::plain("x".repeat(100_100));
    let empty_containers = message_content(&empty);
    let long_containers = message_content(&long);
    let mut empty_text = String::new();
    let mut long_text = String::new();
    for container in &empty_containers {
        container_text(container, &mut empty_text);
    }
    for container in &long_containers {
        container_text(container, &mut long_text);
    }

    assert_eq!(empty_text, "Empty message");
    assert!(long_text.contains("Message truncated for display."));
    assert_eq!(long_text.matches('x').count(), 100_000);
}

#[test]
fn malformed_markdown_remains_safe_and_readable() {
    let malformed = ChatMessageView::markdown("[broken](<\n\n```rust\nfn unfinished() {");
    let containers = message_content(&malformed);
    let mut text = String::new();
    for container in &containers {
        container_text(container, &mut text);
    }

    assert!(text.contains("broken"));
    assert!(text.contains("fn unfinished()"));
}

#[test]
fn markdown_messages_use_hyperchad_markdown_with_highlighting_and_xss_protection() {
    let options = hyperchad::markdown::MarkdownOptions::default();
    assert!(options.syntax_highlighting);
    assert!(options.xss_protection);

    let message = ChatMessageView {
        text: "# Heading\n\n```rust\nfn main() {}\n```\n\n<script>alert('unsafe')</script>"
            .to_owned(),
        display_label: None,
        format: TextFormat::Markdown,
    };

    let containers = message_content(&message);
    let mut text = String::new();
    for container in &containers {
        container_text(container, &mut text);
    }
    let rendered = format!("{containers:?}");
    assert!(text.contains("Heading"));
    assert!(text.contains("fn main() {}"));
    assert!(rendered.contains("markdown"));
    assert!(!rendered.contains("Element::Raw"));
    assert!(!text.contains("alert('unsafe')"));
}

#[test]
fn plain_text_messages_do_not_parse_markdown() {
    let message = ChatMessageView {
        text: "**literal**".to_owned(),
        display_label: None,
        format: TextFormat::PlainText,
    };

    let rendered = format!("{:?}", message_content(&message));
    assert!(rendered.contains("**literal**"));
    assert!(!rendered.contains("markdown"));
}

#[test]
fn user_and_assistant_messages_have_distinct_semantic_surfaces() {
    let user = TranscriptViewItem {
        id: TranscriptViewItemId::new("user:surface"),
        sequence: Some(1),
        timestamp_ms: None,
        revision: 1,
        streaming: false,
        kind: TranscriptViewItemKind::UserMessage {
            message: ChatMessageView::markdown("User message"),
        },
    };
    let assistant = TranscriptViewItem {
        id: TranscriptViewItemId::new("assistant:surface"),
        sequence: Some(2),
        timestamp_ms: None,
        revision: 1,
        streaming: false,
        kind: TranscriptViewItemKind::AssistantMessage {
            message: ChatMessageView::markdown("Assistant message"),
        },
    };

    let user = format!("{:?}", transcript_item(&user));
    let assistant = format!("{:?}", transcript_item(&assistant));
    assert!(user.contains("transcript-item-user:surface"));
    assert!(assistant.contains("transcript-item-assistant:surface"));
    assert!(user.contains("margin_left: Some(Integer(48))"));
    assert!(assistant.contains("margin_right: Some(Integer(48))"));
    assert_ne!(user, assistant);
}

#[test]
fn streaming_assistant_item_has_stable_identity_live_state_and_developer_metadata() {
    let item = TranscriptViewItem {
        id: TranscriptViewItemId::new("assistant:stream"),
        sequence: Some(41),
        timestamp_ms: Some(42),
        revision: 3,
        streaming: true,
        kind: TranscriptViewItemKind::AssistantMessage {
            message: ChatMessageView::markdown("Streaming **answer**"),
        },
    };

    let containers = transcript_item(&item);
    let mut text = String::new();
    for container in &containers {
        container_text(container, &mut text);
    }
    let rendered = format!("{containers:?}");
    assert!(rendered.contains("transcript-item-assistant:stream"));
    assert!(text.contains("live"));
    assert!(text.contains("developer details"));
    assert!(text.contains("revision 3"));
    assert!(text.contains("event 41"));
}

#[test]
fn reasoning_item_uses_collapsed_semantic_disclosure() {
    let item = TranscriptViewItem {
        id: TranscriptViewItemId::new("reasoning:details"),
        sequence: Some(1),
        timestamp_ms: None,
        revision: 1,
        streaming: false,
        kind: TranscriptViewItemKind::ReasoningMessage {
            message: ChatMessageView::markdown("Because **reasons**"),
        },
    };

    let rendered = format!("{:?}", transcript_item(&item));
    assert!(rendered.contains("Reasoning"));
    assert!(rendered.contains("Details"));
    assert!(!rendered.contains("open: true"));
}

#[test]
fn system_skill_and_compaction_items_render_as_notices() {
    let system = TranscriptViewItemKind::SystemMessage {
        message: ChatMessageView::plain("System notice"),
    };
    let skill = TranscriptViewItemKind::Skill {
        skill: SkillView {
            skill_id: "broken".to_owned(),
            status: SkillViewStatus::Failed,
            text: "Skill failed".to_owned(),
        },
    };
    let compaction = TranscriptViewItemKind::Compaction {
        compaction: CompactionView {
            status: CompactionViewStatus::Local,
            text: "Older context summarized".to_owned(),
            provider_plugin_id: None,
            model_id: None,
        },
    };

    let system = format!("{:?}", transcript_item_body(&system));
    let skill = format!("{:?}", transcript_item_body(&skill));
    let compaction = format!("{:?}", transcript_item_body(&compaction));
    assert!(system.contains("Aside"));
    assert!(skill.contains("Aside"));
    assert!(skill.contains("r: 248"));
    assert!(skill.contains("g: 81"));
    assert!(skill.contains("b: 73"));
    assert!(compaction.contains("Context compacted"));
}

#[test]
fn reasoning_items_follow_shared_visibility() {
    let item = TranscriptViewItem {
        id: TranscriptViewItemId::new("reasoning:test"),
        sequence: Some(1),
        timestamp_ms: None,
        revision: 1,
        streaming: false,
        kind: TranscriptViewItemKind::ReasoningMessage {
            message: ChatMessageView::markdown("hidden"),
        },
    };

    assert!(!should_render_transcript_item(&item, false));
    assert!(should_render_transcript_item(&item, true));
}

#[test]
fn skill_transcript_item_renders_semantic_label_and_text() {
    let item = TranscriptViewItem {
        id: TranscriptViewItemId::new("skill:test"),
        sequence: Some(1),
        timestamp_ms: None,
        revision: 1,
        streaming: false,
        kind: TranscriptViewItemKind::Skill {
            skill: SkillView {
                skill_id: "review".to_owned(),
                status: SkillViewStatus::Failed,
                text: "review: boom".to_owned(),
            },
        },
    };

    assert_eq!(item_label(&item.kind), "skill error");
    let rendered = format!("{:?}", transcript_item(&item));
    assert!(rendered.contains("review: boom"));

    let context_item = TranscriptViewItem {
        id: TranscriptViewItemId::new("skill:context"),
        sequence: Some(2),
        timestamp_ms: None,
        revision: 1,
        streaming: false,
        kind: TranscriptViewItemKind::Skill {
            skill: SkillView {
                skill_id: "review".to_owned(),
                status: SkillViewStatus::ContextLoaded,
                text: "loaded review".to_owned(),
            },
        },
    };
    assert_eq!(item_label(&context_item.kind), "skill context");
    let rendered = format!("{:?}", transcript_item(&context_item));
    assert!(rendered.contains("loaded review"));

    let compaction_item = TranscriptViewItem {
        id: TranscriptViewItemId::new("compaction:test"),
        sequence: Some(3),
        timestamp_ms: None,
        revision: 1,
        streaming: false,
        kind: TranscriptViewItemKind::Compaction {
            compaction: CompactionView {
                status: CompactionViewStatus::Local,
                text: "local context compaction: summary".to_owned(),
                provider_plugin_id: None,
                model_id: None,
            },
        },
    };
    assert_eq!(item_label(&compaction_item.kind), "compaction");
    let rendered = format!("{:?}", transcript_item(&compaction_item));
    assert!(rendered.contains("local context compaction: summary"));
}

#[test]
fn hyperchad_shell_renders_all_primary_regions_including_runtime_state() {
    let mut snapshot = SessionViewSnapshot::empty();
    snapshot.session_id = Some(bcode_session_models::SessionId::new());
    snapshot.runtime_work.push(RuntimeWorkView {
        work_id: WorkId::new("work-1"),
        kind: bcode_session_models::RuntimeWorkKind::Tool,
        label: "index workspace".to_owned(),
        status: RuntimeWorkStatus::Running,
        cancellable: true,
        message: Some("indexing".to_owned()),
        completed_units: Some(2),
        total_units: Some(4),
        updated_at_ms: Some(1),
    });

    let rendered = format!("{:?}", home(&snapshot, &[], "secret-token"));
    assert!(rendered.contains("sessions"));
    assert!(rendered.contains("transcript"));
    assert!(rendered.contains("composer"));
    assert!(rendered.contains("daemon connected · session attached"));
    assert!(rendered.contains("runtime work"));
    assert!(rendered.contains("index workspace"));
    assert!(rendered.contains("work-1"));
}

#[test]
fn permission_controls_distinguish_single_batch_and_resolved_history() {
    let session_id = bcode_session_models::SessionId::new();
    let permission = |call_count, resolved, approved| PermissionView {
        permission_id: "permission-1".to_owned(),
        session_id: Some(session_id),
        tool_call_id: "call-1".to_owned(),
        tool_name: "shell.run".to_owned(),
        arguments_json: r#"{"command":"pwd"}"#.to_owned(),
        batch: Some(PermissionBatchView {
            batch_id: "batch-1".to_owned(),
            call_index: 0,
            call_count,
        }),
        agent_id: "agent-1".to_owned(),
        title: Some("Permission requested".to_owned()),
        policy_source: Some("workspace policy".to_owned()),
        detail: Some("Review command execution".to_owned()),
        resolved,
        approved,
        can_remember: true,
    };

    let single_containers = permission_request(
        &permission(1, false, None),
        Some(session_id),
        "secret-token",
    );
    let batch_containers = permission_request(
        &permission(3, false, None),
        Some(session_id),
        "secret-token",
    );
    let resolved_containers = permission_history(&permission(3, true, Some(false)));
    let mut single = String::new();
    let mut batch = String::new();
    let mut resolved = String::new();
    for container in &single_containers {
        container_text(container, &mut single);
    }
    for container in &batch_containers {
        container_text(container, &mut batch);
    }
    for container in &resolved_containers {
        container_text(container, &mut resolved);
    }
    let single_tree = format!("{single_containers:?}");
    let batch_tree = format!("{batch_containers:?}");
    let resolved_tree = format!("{resolved_containers:?}");
    assert!(single.contains("shell.run"));
    assert!(single.contains("agent-1"));
    assert!(single.contains("workspace policy"));
    assert!(single.contains("decision can be remembered"));
    assert!(single.contains("remember"));
    assert!(!single.contains("approve all"));
    assert!(batch.contains("call"));
    assert!(batch.contains("approve all 3"));
    assert!(batch.contains("deny all 3"));
    assert!(batch_tree.contains("/actions/permission-batch"));
    assert!(single_tree.contains("/actions/permission?"));
    assert!(resolved.contains("denied"));
    assert!(!resolved_tree.contains("/actions/permission"));
    assert!(!resolved.contains("approve"));
}

#[test]
fn grouped_permission_renders_per_call_and_apply_to_all_actions() {
    let mut snapshot = SessionViewSnapshot::empty();
    snapshot.session_id = Some(bcode_session_models::SessionId::new());
    snapshot.permissions.push(PermissionView {
        permission_id: "permission-1".to_owned(),
        session_id: snapshot.session_id,
        tool_call_id: "call-1".to_owned(),
        tool_name: "shell.run".to_owned(),
        arguments_json: r#"{"command":"pwd"}"#.to_owned(),
        batch: Some(PermissionBatchView {
            batch_id: "batch-1".to_owned(),
            call_index: 0,
            call_count: 3,
        }),
        agent_id: "agent-1".to_owned(),
        title: Some("Permission requested".to_owned()),
        policy_source: Some("test".to_owned()),
        detail: Some("review".to_owned()),
        resolved: false,
        approved: None,
        can_remember: true,
    });

    let rendered = format!("{:?}", home(&snapshot, &[], "secret-token"));
    assert!(rendered.contains("batch"));
    assert!(rendered.contains('1'));
    assert!(rendered.contains('3'));
    assert!(rendered.contains("approve all"));
    assert!(rendered.contains("deny all"));
    assert!(rendered.contains("/actions/permission-batch"));
    assert!(rendered.contains("/actions/permission?"));
    assert!(rendered.contains("batch-1"));
}

#[test]
fn transcript_history_controls_render_source_anchored_actions() {
    let mut snapshot = SessionViewSnapshot::empty();
    snapshot.session_id = Some(bcode_session_models::SessionId::new());
    snapshot.transcript.source_start_sequence = Some(10);
    snapshot.transcript.source_end_sequence = Some(20);
    snapshot.transcript.has_older_history = true;
    snapshot.transcript.has_newer_history = true;

    let rendered = format!("{:?}", home(&snapshot, &[], "secret-token"));
    assert!(rendered.contains("/actions/history-window?token=secret-token"));
    assert!(rendered.contains("load older history"));
    assert!(rendered.contains("load newer history"));
    assert!(rendered.contains("10"));
    assert!(rendered.contains("20"));
}

#[test]
fn session_navigation_marks_selected_active_and_idle_sessions() {
    let selected = bcode_session_models::SessionSummary {
        id: bcode_session_models::SessionId::new(),
        name: Some("Selected session".to_owned()),
        explicit_name: None,
        derived_title: None,
        title_source: bcode_session_models::SessionTitleSource::Explicit,
        client_count: 2,
        created_at_ms: 1,
        updated_at_ms: 42,
        working_directory: "/tmp/selected".into(),
        import: None,
        fork: None,
    };
    let idle = bcode_session_models::SessionSummary {
        id: bcode_session_models::SessionId::new(),
        name: Some("Idle session".to_owned()),
        client_count: 0,
        working_directory: "/tmp/idle".into(),
        ..selected.clone()
    };

    let containers =
        session_navigation(&[selected.clone(), idle], Some(selected.id), "secret-token");
    let mut text = String::new();
    for container in &containers {
        container_text(container, &mut text);
    }
    assert!(text.contains("Selected session"));
    assert!(text.contains("selected · active"));
    assert!(text.contains("2 connected"));
    assert!(text.contains("Idle session"));
    assert!(text.contains("idle"));
}

#[test]
fn composer_presents_ready_disabled_and_message_placement_states() {
    let mut ready = SessionViewSnapshot::empty();
    ready.composer.can_submit = true;
    ready.composer.draft = "Preserved draft".to_owned();
    let mut disabled = ready.clone();
    disabled.composer.can_submit = false;
    disabled.composer.disabled_reason = Some("Wait for the active operation".to_owned());

    let ready = format!("{:?}", composer(&ready, "secret-token"));
    let disabled = format!("{:?}", composer(&disabled, "secret-token"));
    assert!(ready.contains("Ready to send"));
    assert!(ready.contains("Preserved draft"));
    assert!(ready.contains("Steer the active turn"));
    assert!(ready.contains("Queue as a follow-up"));
    assert!(ready.contains("Send message"));
    assert!(disabled.contains("Wait for the active operation"));
    assert!(disabled.contains("Sending unavailable"));
    assert!(disabled.contains("disabled"));
}

#[test]
fn access_token_is_propagated_to_browser_actions() {
    let rendered = format!(
        "{:?}",
        home(&SessionViewSnapshot::empty(), &[], "secret-token")
    );

    assert!(rendered.contains("/actions/submit-message?token=secret-token"));
}

#[test]
fn session_links_propagate_access_token_and_live_scope() {
    let session = bcode_session_models::SessionSummary {
        id: bcode_session_models::SessionId::new(),
        name: Some("session".to_owned()),
        explicit_name: None,
        derived_title: None,
        title_source: bcode_session_models::SessionTitleSource::Explicit,
        client_count: 0,
        created_at_ms: 1,
        updated_at_ms: 1,
        working_directory: "/tmp/project".into(),
        import: None,
        fork: None,
        execution: None,
    };
    let rendered = format!(
        "{:?}",
        home(
            &SessionViewSnapshot::empty(),
            std::slice::from_ref(&session),
            "secret-token"
        )
    );
    assert!(
        rendered.contains(&format!(
            "token=secret-token&amp;hyperchad-event-scope=secret-token:{}",
            session.id
        )) || rendered.contains(&format!(
            "token=secret-token&hyperchad-event-scope=secret-token:{}",
            session.id
        ))
    );
}

#[test]
fn unknown_interactions_keep_bounded_active_controls_and_resolved_history() {
    let session_id = bcode_session_models::SessionId::new();
    let active = InteractionViewSummary {
        interaction_id: "interaction-unknown".to_owned(),
        kind: "future.interaction".to_owned(),
        surface_kind: "future.surface".to_owned(),
        tool_call_id: Some("call-unknown".to_owned()),
        title: Some("Future interaction".to_owned()),
        required: true,
        snapshot: Some(serde_json::json!({"sentinel": "u".repeat(40_000)})),
        resolved: false,
        resolution: None,
    };
    let resolved = InteractionViewSummary {
        resolved: true,
        resolution: Some(serde_json::json!({"status": "future-resolved"})),
        ..active.clone()
    };

    let active = format!(
        "{:?}",
        interaction_request(&active, Some(session_id), "secret-token")
    );
    let resolved = format!(
        "{:?}",
        interaction_request(&resolved, Some(session_id), "secret-token")
    );
    assert!(active.contains("controller snapshot"));
    assert!(active.contains("Structured details truncated for display."));
    assert!(active.contains("generic semantic controls"));
    assert!(active.contains("/actions/interaction?"));
    assert!(resolved.contains("future-resolved"));
    assert!(resolved.contains("resolution"));
    assert!(!resolved.contains("generic semantic controls"));
    assert!(!resolved.contains("/actions/interaction?"));
}

#[test]
fn question_snapshot_renders_polished_controls_and_generic_fallback() {
    let interaction = InteractionViewSummary {
        interaction_id: "interaction-1".to_owned(),
        kind: "bcode.question".to_owned(),
        surface_kind: "bcode.question.inline".to_owned(),
        tool_call_id: Some("call-1".to_owned()),
        title: Some("Choose".to_owned()),
        required: true,
        snapshot: Some(serde_json::json!({
            "request": {
                "questions": [{
                    "header": "Decision",
                    "question": "Proceed?",
                    "options": [{"label": "Yes", "value": "yes", "description": "Continue"}],
                    "control": "radio",
                    "selection_mode": "single",
                    "custom": true,
                    "custom_mode": "additional",
                    "required": true
                }]
            },
            "answers": [{"question_index": 0, "selected": ["yes"], "custom": null}],
            "focus": {"type": "question", "question_index": 0},
            "focused_control_id": "question-0"
        })),
        resolved: false,
        resolution: None,
    };

    let rendered = format!(
        "{:?}",
        interaction_request(
            &interaction,
            Some(bcode_session_models::SessionId::new()),
            "secret-token"
        )
    );
    assert!(rendered.contains("Proceed?"));
    assert!(rendered.contains("Decision"));
    assert!(rendered.contains("Continue"));
    assert!(rendered.contains("Choose one option"));
    assert!(rendered.contains("you may also provide a custom answer"));
    assert!(rendered.contains("◉"));
    assert!(rendered.contains(" *"));
    assert!(rendered.contains("question-0.option-0"));
    assert!(rendered.contains("submit answers"));
    assert!(rendered.contains("generic semantic controls"));
    for label in [
        "activate control",
        "change control value",
        "focus control",
        "blur control",
        "navigate focus",
        "submit interaction",
        "cancel interaction",
    ] {
        assert!(rendered.contains(label));
    }
    assert!(rendered.contains("/actions/interaction?"));
}

#[test]
fn question_snapshot_renders_multiple_checkbox_and_exclusive_custom_semantics() {
    let interaction = InteractionViewSummary {
        interaction_id: "interaction-multiple".to_owned(),
        kind: "bcode.question".to_owned(),
        surface_kind: "bcode.question.inline".to_owned(),
        tool_call_id: Some("call-multiple".to_owned()),
        title: Some("Choose several".to_owned()),
        required: false,
        snapshot: Some(serde_json::json!({
            "request": {
                "questions": [{
                    "header": null,
                    "question": "Which options?",
                    "options": [
                        {"label": "Alpha", "value": "a", "description": null},
                        {"label": "Beta", "value": "b", "description": "Second option"}
                    ],
                    "control": "checkbox",
                    "selection_mode": "multiple",
                    "custom": true,
                    "custom_mode": "exclusive",
                    "required": false
                }]
            },
            "answers": [{"question_index": 0, "selected": ["a"], "custom": "other"}],
            "focus": {"type": "custom", "question_index": 0},
            "focused_control_id": "question-0.custom"
        })),
        resolved: false,
        resolution: None,
    };

    let containers = interaction_request(
        &interaction,
        Some(bcode_session_models::SessionId::new()),
        "secret-token",
    );
    let mut text = String::new();
    for container in &containers {
        container_text(container, &mut text);
    }
    let rendered = format!("{containers:?}");
    assert!(text.contains("Choose one or more options"));
    assert!(text.contains("or provide a custom answer"));
    assert!(text.contains("☑ Alpha"));
    assert!(text.contains("☐ Beta"));
    assert!(text.contains("Second option"));
    assert!(rendered.contains("other"));
}

#[test]
fn filesystem_query_adapters_cover_empty_structured_metadata_and_bounded_snippets() {
    let result = |schema: &str, title: &str, metadata| {
        render_tool_result(&ToolResultView::Artifact {
            artifact: ToolArtifactView::from(ToolArtifact {
                artifact_id: format!("query-{schema}"),
                producer_plugin_id: "bcode.filesystem".to_owned(),
                schema: schema.to_owned(),
                schema_version: 1,
                tool_call_id: Some("call-query".to_owned()),
                title: Some(title.to_owned()),
                metadata,
                refs: Vec::new(),
            }),
        })
    };
    let exists = format!(
        "{:?}",
        result(
            "bcode.filesystem.exists",
            "Path exists",
            serde_json::json!({"path": "/tmp/missing", "exists": false}),
        )
    );
    let list = format!(
        "{:?}",
        result(
            "bcode.filesystem.list",
            "Directory entries",
            serde_json::json!({
                "entries": [], "backend": "rust", "partial": false,
                "timed_out": false, "visited_entries": 0
            }),
        )
    );
    let find = format!(
        "{:?}",
        result(
            "bcode.filesystem.find",
            "Path matches",
            serde_json::json!({
                "paths": [], "backend": "rust", "partial": true,
                "timed_out": false, "visited_entries": 42,
                "message": "result limit reached"
            }),
        )
    );
    let grep = format!(
        "{:?}",
        result(
            "bcode.filesystem.grep",
            "Text matches",
            serde_json::json!({
                "matches": [{
                    "path": "/tmp/example.rs",
                    "line_number": 77,
                    "line": "g".repeat(2_100)
                }],
                "backend": "ripgrep", "partial": true,
                "timed_out": true, "visited_entries": 100
            }),
        )
    );
    let stat = format!(
        "{:?}",
        result(
            "bcode.filesystem.stat",
            "Path metadata",
            serde_json::json!({
                "path": "/tmp/example.rs", "exists": true,
                "kind": "file", "len": 1234
            }),
        )
    );

    assert!(exists.contains("Path does not exist"));
    assert!(exists.contains("/tmp/missing"));
    assert!(list.contains("No directory entries."));
    assert!(list.contains("visited entries"));
    assert!(find.contains("No matching paths."));
    assert!(find.contains("result limit reached"));
    assert!(grep.contains("/tmp/example.rs"));
    assert!(grep.contains("77"));
    assert!(grep.contains('…'));
    assert!(grep.contains("timed out"));
    assert!(stat.contains("Path exists"));
    assert!(stat.contains("file"));
    assert!(stat.contains("1234"));
}

#[test]
fn filesystem_change_adapter_renders_bounded_split_diff_and_line_context() {
    let result = ToolResultView::Artifact {
        artifact: ToolArtifactView::from(ToolArtifact {
            artifact_id: "filesystem-change-edge".to_owned(),
            producer_plugin_id: "bcode.filesystem".to_owned(),
            schema: "bcode.filesystem.change".to_owned(),
            schema_version: 1,
            tool_call_id: Some("call-change".to_owned()),
            title: Some("File change".to_owned()),
            metadata: serde_json::json!({
                "tool_name": "filesystem.edit",
                "summary": "updated configuration",
                "path": "/tmp/config.rs",
                "old_text": format!("old_one{}\nold_two", "o".repeat(17_000)),
                "new_text": format!("new_one{}\nnew_two", "n".repeat(17_000)),
                "old_start_line": 10,
                "new_start_line": 12
            }),
            refs: Vec::new(),
        }),
    };
    let containers = render_tool_result(&result);
    let mut text = String::new();
    for container in &containers {
        container_text(container, &mut text);
    }

    assert!(text.contains("/tmp/config.rs"));
    assert!(text.contains("updated configuration"));
    assert!(text.contains("operation: filesystem.edit"));
    assert!(text.contains("removed · lines 10–11"));
    assert!(text.contains("added · lines 12–13"));
    assert!(text.contains("old_one"));
    assert!(text.contains("new_one"));
    assert!(text.contains("Removed text truncated for display."));
    assert!(text.contains("Added text truncated for display."));
}

#[test]
fn filesystem_read_adapter_renders_path_range_language_truncation_and_continuation() {
    let result = ToolResultView::Artifact {
        artifact: ToolArtifactView::from(ToolArtifact {
            artifact_id: "filesystem-read-edge".to_owned(),
            producer_plugin_id: "bcode.filesystem".to_owned(),
            schema: "bcode.filesystem.read".to_owned(),
            schema_version: 1,
            tool_call_id: Some("call-read".to_owned()),
            title: Some("File contents".to_owned()),
            metadata: serde_json::json!({
                "path": "/tmp/example.rs",
                "start_line": 11,
                "end_line": 20,
                "total_lines": 100,
                "returned_bytes": 33_000,
                "total_bytes": 50_000,
                "truncated": true,
                "contents": format!("fn main() {{}}\n{}", "x".repeat(33_000))
            }),
            refs: vec![bcode_session_models::ToolArtifactRef {
                key: "full-file".to_owned(),
                content_type: Some("text/x-rust".to_owned()),
                storage_uri: Some("file:///tmp/example.rs".to_owned()),
                byte_len: Some(50_000),
                metadata: None,
            }],
        }),
    };
    let containers = render_tool_result(&result);
    let mut text = String::new();
    for container in &containers {
        container_text(container, &mut text);
    }
    let rendered = format!("{containers:?}");

    assert!(text.contains("/tmp/example.rs"));
    assert!(text.contains("lines 11–20 of 100"));
    assert!(text.contains("33000 of 50000 bytes"));
    assert!(text.contains("rust"));
    assert!(text.contains("fn main() {}"));
    assert!(text.contains("File contents truncated for display."));
    assert!(text.contains("More file content is available."));
    assert!(text.contains("Continue at offset 21."));
    assert!(text.contains("artifact references"));
    assert!(text.contains("full-file"));
    assert!(rendered.contains("markdown"));
}

#[test]
fn document_and_ocr_results_bound_text_and_show_metadata_and_references() {
    let reference = bcode_session_models::ToolArtifactRef {
        key: "text-sidecar".to_owned(),
        content_type: Some("text/plain".to_owned()),
        storage_uri: Some("file:///tmp/extracted.txt".to_owned()),
        byte_len: Some(40_000),
        metadata: None,
    };
    let document = ToolResultView::Artifact {
        artifact: ToolArtifactView::from(ToolArtifact {
            artifact_id: "document-edge".to_owned(),
            producer_plugin_id: "bcode.document".to_owned(),
            schema: "bcode.document.extract_result".to_owned(),
            schema_version: 1,
            tool_call_id: Some("call-document".to_owned()),
            title: Some("Document extraction".to_owned()),
            metadata: serde_json::json!({
                "source": "file:///tmp/source.pdf",
                "content_type": "application/pdf",
                "extractor": "pdftotext",
                "document_path": "/tmp/source.pdf",
                "text_path": "/tmp/extracted.txt",
                "truncated": true,
                "text": "d".repeat(33_000)
            }),
            refs: vec![reference.clone()],
        }),
    };
    let ocr = ToolResultView::Artifact {
        artifact: ToolArtifactView::from(ToolArtifact {
            artifact_id: "ocr-edge".to_owned(),
            producer_plugin_id: "bcode.ocr".to_owned(),
            schema: "bcode.ocr.extract_result".to_owned(),
            schema_version: 1,
            tool_call_id: Some("call-ocr".to_owned()),
            title: Some("OCR extraction".to_owned()),
            metadata: serde_json::json!({
                "source": {"path": "/tmp/image.png"},
                "engine": "tesseract",
                "language": "eng",
                "text_bytes": 33_000,
                "truncated": true,
                "text": "o".repeat(33_000)
            }),
            refs: vec![reference],
        }),
    };

    let document = format!("{:?}", render_tool_result(&document));
    let ocr = format!("{:?}", render_tool_result(&ocr));
    for rendered in [&document, &ocr] {
        assert!(rendered.contains("Source extraction was truncated."));
        assert!(rendered.contains("Extracted text truncated for display."));
        assert!(rendered.contains("artifact references"));
        assert!(rendered.contains("text-sidecar"));
        assert!(rendered.contains("file:///tmp/extracted.txt"));
    }
    assert!(document.contains("application/pdf"));
    assert!(document.contains("pdftotext"));
    assert!(ocr.contains("tesseract"));
    assert!(ocr.contains("eng"));
}

#[test]
fn bundled_visual_registry_covers_actual_high_value_request_schemas() {
    for schema in [
        "bcode.filesystem.request",
        "bcode.filesystem.change",
        "bcode.filesystem.read",
        "bcode.filesystem.image",
        "bcode.filesystem.exists",
        "bcode.filesystem.list",
        "bcode.filesystem.find",
        "bcode.filesystem.grep",
        "bcode.filesystem.stat",
        "bcode.filesystem.artifact.metadata",
        "bcode.filesystem.artifact.read",
        "bcode.filesystem.artifact.grep",
        "bcode.document.request",
        "bcode.ocr.request",
        "bcode.web-search.search_request",
        "bcode.web-search.fetch_request",
        "bcode.web-search.status_request",
        "bcode.web-search.inspect_request",
        "bcode.git.clone_request",
        "bcode.worktree.request",
        "bcode.vim-edit.request.preview",
        "bcode.vim-edit.request.apply",
        "bcode.vim-edit.live",
        "bcode.vim-edit.playback",
    ] {
        assert!(
            VISUAL_ADAPTERS.contains_key(&(schema, 1)),
            "missing {schema}"
        );
    }
}

#[test]
fn structured_request_adapter_renders_meaningful_fields_and_fallback() {
    let visual = PluginVisualView::from(PluginVisualDescriptor {
        visual_id: Some("filesystem-1".to_owned()),
        producer_plugin_id: Some("bcode.filesystem".to_owned()),
        schema: "bcode.filesystem.request".to_owned(),
        schema_version: 1,
        title: Some("Read file".to_owned()),
        subtitle: None,
        payload: serde_json::json!({"operation": "read", "path": "/tmp/sentinel.txt"}),
    });

    let rendered = format!("{:?}", render_plugin_visual("request visual", &visual));
    assert!(rendered.contains("Read file"));
    assert!(rendered.contains("/tmp/sentinel.txt"));
    assert!(rendered.contains("request visual"));
}

#[test]
fn shell_visual_adapter_is_versioned_and_keeps_generic_fallback() {
    let visual = PluginVisualView::from(PluginVisualDescriptor {
        visual_id: Some("shell-1".to_owned()),
        producer_plugin_id: Some("bcode.shell".to_owned()),
        schema: "bcode.tool.request.shell.run".to_owned(),
        schema_version: 1,
        title: Some("Shell command".to_owned()),
        subtitle: None,
        payload: serde_json::json!({
            "arguments": {"command": "printf sentinel", "cwd": "/tmp"},
            "_bcode_runtime": {"output": "sentinel output"}
        }),
    });

    let rendered = format!("{:?}", render_plugin_visual("plugin visual", &visual));
    assert!(rendered.contains("printf sentinel"));
    assert!(rendered.contains("sentinel output"));
    assert!(rendered.contains("plugin visual"));
}

#[test]
fn unknown_visual_schema_uses_generic_fallback() {
    let visual = PluginVisualView::from(PluginVisualDescriptor {
        visual_id: None,
        producer_plugin_id: Some("fixture".to_owned()),
        schema: "fixture.unknown".to_owned(),
        schema_version: 99,
        title: None,
        subtitle: None,
        payload: serde_json::json!({"sentinel": "generic-only"}),
    });

    let rendered = format!("{:?}", render_plugin_visual("plugin visual", &visual));
    assert!(rendered.contains("generic-only"));
}

#[test]
fn generic_plugin_visual_keeps_schema_payload_in_render_tree() {
    let kind = TranscriptViewItemKind::PluginVisual {
        visual: PluginVisualView::from(PluginVisualDescriptor {
            visual_id: Some("visual-1".to_owned()),
            producer_plugin_id: Some("fixture-plugin".to_owned()),
            schema: "fixture.visual".to_owned(),
            schema_version: 1,
            title: Some("Fixture visual".to_owned()),
            subtitle: None,
            payload: serde_json::json!({"sentinel": "visual-payload"}),
        }),
    };

    let rendered = format!("{:?}", transcript_item_body(&kind));
    assert!(rendered.contains("fixture.visual"));
    assert!(rendered.contains("visual-payload"));
}

#[test]
fn unknown_contribution_has_no_raw_hyperchad_fallback() {
    let kind = TranscriptViewItemKind::ToolContribution {
        placement: bcode_session_models::ToolContributionPlacement::Request,
        contribution: bcode_session_models::ToolContributionEvent {
            invocation_id: "call".to_owned(),
            contribution_id: "surface".to_owned(),
            sequence: 9,
            producer_id: "future.producer".to_owned(),
            schema: "future.unknown/schema".to_owned(),
            schema_version: 77,
            operation: bcode_session_models::ToolContributionOperation::Append,
            persistence: bcode_session_models::ToolContributionPersistence::Durable,
            artifact: None,
            payload: serde_json::json!({"sentinel": "opaque-web"}),
        },
    };
    let rendered = format!("{:?}", transcript_item_body(&kind));
    assert!(!rendered.contains("future.unknown/schema"));
    assert!(!rendered.contains("opaque-web"));
    assert!(!rendered.contains("append"));
}

#[test]
fn git_contribution_renders_through_schema_adapter_without_fallback() {
    let kind = TranscriptViewItemKind::ToolContribution {
        placement: bcode_session_models::ToolContributionPlacement::Request,
        contribution: bcode_session_models::ToolContributionEvent {
            invocation_id: "git-call".to_owned(),
            contribution_id: "clone-request".to_owned(),
            sequence: 1,
            producer_id: "bcode.git".to_owned(),
            schema: "bcode.git.clone_request".to_owned(),
            schema_version: 1,
            operation: bcode_session_models::ToolContributionOperation::Upsert,
            persistence: bcode_session_models::ToolContributionPersistence::Durable,
            artifact: None,
            payload: serde_json::json!({
                "url": "https://github.com/bmorphism/bcode",
                "ref": "main"
            }),
        },
    };

    let rendered = format!("{:?}", transcript_item_body(&kind));
    assert!(rendered.contains("github.com/bmorphism/bcode"));
    assert!(rendered.contains("main"));
    assert!(!rendered.contains("bcode.git.clone_request"));
}

#[test]
fn unsupported_shell_contribution_has_no_raw_hyperchad_fallback() {
    let kind = TranscriptViewItemKind::ToolContribution {
        placement: bcode_session_models::ToolContributionPlacement::Request,
        contribution: bcode_session_models::ToolContributionEvent {
            invocation_id: "shell-call".to_owned(),
            contribution_id: "shell-run-summary".to_owned(),
            sequence: 1,
            producer_id: "bcode.shell".to_owned(),
            schema: "bcode.shell.run.summary".to_owned(),
            schema_version: 1,
            operation: bcode_session_models::ToolContributionOperation::Upsert,
            persistence: bcode_session_models::ToolContributionPersistence::Durable,
            artifact: None,
            payload: serde_json::json!({"output": "shell-render-sentinel"}),
        },
    };
    let rendered = format!("{:?}", transcript_item_body(&kind));
    assert!(!rendered.contains("bcode.shell.run.summary"));
    assert!(!rendered.contains("shell-render-sentinel"));
}

#[test]
fn visual_adapters_are_schema_version_specific_and_keep_fallbacks() {
    let supported = PluginVisualView::from(PluginVisualDescriptor {
        visual_id: Some("filesystem-version-1".to_owned()),
        producer_plugin_id: Some("bcode.filesystem".to_owned()),
        schema: "bcode.filesystem.request".to_owned(),
        schema_version: 1,
        title: Some("Filesystem read".to_owned()),
        subtitle: None,
        payload: serde_json::json!({"operation": "read", "path": "/tmp/versioned"}),
    });
    let supported_rendered = format!("{:?}", render_plugin_visual("plugin visual", &supported));
    assert!(supported_rendered.contains("Filesystem read"));
    assert!(supported_rendered.contains("/tmp/versioned"));
    assert!(supported_rendered.contains("bcode.filesystem.request"));

    let unsupported_version = PluginVisualView::from(PluginVisualDescriptor {
        schema_version: 2,
        ..supported.descriptor
    });
    let unsupported_rendered = format!(
        "{:?}",
        render_plugin_visual("plugin visual", &unsupported_version)
    );
    assert!(
        !VISUAL_ADAPTERS.contains_key(&("bcode.filesystem.request", 2)),
        "unexpected rich adapter for unsupported schema version"
    );
    assert!(unsupported_rendered.contains("bcode.filesystem.request"));
    assert!(unsupported_rendered.contains("/tmp/versioned"));
}

#[test]
fn every_registered_visual_adapter_has_a_fixture() {
    for ((schema, schema_version), adapter) in VISUAL_ADAPTERS.iter() {
        let visual = PluginVisualView::from(PluginVisualDescriptor {
            visual_id: Some(format!("fixture:{schema}:{schema_version}")),
            producer_plugin_id: Some("fixture-plugin".to_owned()),
            schema: (*schema).to_owned(),
            schema_version: *schema_version,
            title: Some(format!("Fixture {schema}")),
            subtitle: None,
            payload: visual_adapter_fixture_payload(schema),
        });
        assert!(
            adapter(&visual).is_some(),
            "adapter fixture did not render {schema}@{schema_version}"
        );
        let rendered = format!("{:?}", render_plugin_visual("plugin visual", &visual));
        assert!(rendered.contains(schema));
        assert!(rendered.contains("fixture"));
    }
}

#[test]
fn every_registered_artifact_adapter_has_a_fixture() {
    for ((schema, schema_version), adapter) in ARTIFACT_ADAPTERS.iter() {
        let artifact = ToolArtifactView::from(ToolArtifact {
            artifact_id: format!("fixture:{schema}:{schema_version}"),
            producer_plugin_id: "fixture-plugin".to_owned(),
            schema: (*schema).to_owned(),
            schema_version: *schema_version,
            tool_call_id: Some("fixture-call".to_owned()),
            title: Some(format!("Fixture {schema}")),
            metadata: artifact_adapter_fixture_metadata(schema),
            refs: Vec::new(),
        });
        let _rich = adapter(&artifact).unwrap_or_else(|| {
            panic!("artifact adapter fixture did not render {schema}@{schema_version}")
        });
        let rendered = format!(
            "{:?}",
            render_tool_result(&ToolResultView::Artifact { artifact })
        );
        assert!(!rendered.contains("artifact details"));
    }
}

fn artifact_adapter_fixture_metadata(schema: &str) -> serde_json::Value {
    document_artifact_fixture(schema)
        .or_else(|| filesystem_artifact_fixture(schema))
        .or_else(|| ocr_artifact_fixture(schema))
        .or_else(|| web_and_worktree_artifact_fixture(schema))
        .unwrap_or_else(|| serde_json::json!({"fixture": true}))
}

fn document_artifact_fixture(schema: &str) -> Option<serde_json::Value> {
    match schema {
        "bcode.document.extract_result" => Some(serde_json::json!({
            "source": "file:///tmp/fixture.pdf",
            "content_type": "application/pdf",
            "artifact_kind": "document",
            "artifact_scope": "session",
            "document_path": "/tmp/fixture.pdf",
            "text_path": "/tmp/fixture.txt",
            "text": "fixture document text",
            "truncated": false,
            "extractor": "native"
        })),
        "bcode.document.status" => Some(serde_json::json!({
            "extract": {
                "available": true,
                "extractors": [{
                    "name": "fixture-extractor",
                    "available": true,
                    "quality": "fixture-quality"
                }],
                "configured_order": ["fixture-extractor"]
            }
        })),
        _ => None,
    }
}

fn filesystem_artifact_fixture(schema: &str) -> Option<serde_json::Value> {
    match schema {
        "bcode.filesystem.read" => Some(serde_json::json!({
            "path": "/tmp/fixture.rs",
            "start_line": 1,
            "end_line": 1,
            "total_lines": 1,
            "returned_bytes": 21,
            "total_bytes": 21,
            "truncated": false,
            "contents": "fixture file contents"
        })),
        "bcode.filesystem.image" => Some(serde_json::json!({
            "path": "/tmp/fixture.png",
            "mime_type": "image/png",
            "width": 640,
            "height": 480,
            "byte_len": 1024
        })),
        "bcode.filesystem.change" => Some(serde_json::json!({
            "tool_name": "filesystem.edit",
            "summary": "fixture change",
            "path": "/tmp/fixture.txt",
            "old_text": "old fixture",
            "new_text": "new fixture",
            "start_line": 1
        })),
        "bcode.filesystem.exists" => Some(serde_json::json!({
            "exists": true
        })),
        "bcode.filesystem.list" => Some(serde_json::json!({
            "entries": [{"path": "/tmp/fixture.txt", "kind": "file"}],
            "backend": "fixture-backend",
            "timed_out": false,
            "partial": false,
            "visited_entries": 1,
            "message": "fixture message"
        })),
        "bcode.filesystem.find" => Some(serde_json::json!({
            "paths": ["/tmp/fixture.txt"],
            "backend": "fixture-backend",
            "timed_out": false,
            "partial": false,
            "visited_entries": 1,
            "message": "fixture message"
        })),
        "bcode.filesystem.grep" => Some(serde_json::json!({
            "matches": [{"path": "/tmp/fixture.txt", "line_number": 1, "line": "fixture match"}],
            "backend": "fixture-backend",
            "timed_out": false,
            "partial": false,
            "visited_entries": 1,
            "message": "fixture message"
        })),
        "bcode.filesystem.stat" => Some(serde_json::json!({
            "exists": true,
            "kind": "file",
            "len": 128
        })),
        _ => filesystem_artifact_file_fixture(schema),
    }
}

fn filesystem_artifact_file_fixture(schema: &str) -> Option<serde_json::Value> {
    match schema {
        "bcode.filesystem.artifact.metadata" => Some(serde_json::json!({
            "path": "/tmp/fixture-artifact.json",
            "exists": true,
            "kind": "file",
            "byte_len": 128,
            "content_type": "application/json",
            "complete": true,
            "message": "fixture message"
        })),
        "bcode.filesystem.artifact.read" => Some(serde_json::json!({
            "path": "/tmp/fixture-artifact.json",
            "offset_bytes": 0,
            "returned_bytes": 16,
            "total_bytes": 16,
            "from_end": false,
            "truncated": false,
            "contents": "fixture artifact"
        })),
        "bcode.filesystem.artifact.grep" => Some(serde_json::json!({
            "path": "/tmp/fixture-artifact.json",
            "matches": [{"path": "/tmp/fixture-artifact.json", "line_number": 1, "line": "fixture artifact match"}],
            "total_bytes": 128,
            "partial": false,
            "message": "fixture message"
        })),
        _ => None,
    }
}

fn ocr_artifact_fixture(schema: &str) -> Option<serde_json::Value> {
    match schema {
        "bcode.ocr.extract_result" => Some(serde_json::json!({
            "text": "fixture OCR text",
            "source": {
                "path": "/tmp/fixture.png",
                "url": null
            },
            "engine": "tesseract",
            "language": "eng",
            "truncated": false,
            "text_bytes": 16,
            "full_text_bytes": 16
        })),
        "bcode.ocr.status" => Some(serde_json::json!({
            "extract": {
                "available": true,
                "default_engine": "tesseract",
                "engines": [{
                    "name": "tesseract",
                    "available": true,
                    "version": "fixture-version",
                    "quality": "fixture-quality"
                }]
            }
        })),
        _ => None,
    }
}

fn web_and_worktree_artifact_fixture(schema: &str) -> Option<serde_json::Value> {
    match schema {
        "bcode.shell.run" => Some(serde_json::json!({
            "mode": "terminal",
            "exit_code": 0,
            "timed_out": false,
            "cancelled": false,
            "duration_ms": 125,
            "output_tail": "fixture shell output",
            "output_truncated": false,
            "output_bytes": 20,
            "retained_output_bytes": 20,
            "columns": 80,
            "rows": 24,
            "format_commands": true
        })),
        "bcode.git.clone_result" => Some(serde_json::json!({
            "host": "github.com",
            "owner": "fixture-owner",
            "repo": "fixture-repo",
            "clone_url": "https://github.com/fixture-owner/fixture-repo.git",
            "path": "/tmp/fixture-repo",
            "already_exists": false
        })),
        "bcode.web-search.search_results" => Some(serde_json::json!({
            "query": "fixture search",
            "provider": "fixture-provider",
            "results": [{
                "title": "fixture result",
                "url": "https://example.com/fixture",
                "snippet": "fixture snippet"
            }],
            "partial": false,
            "message": "fixture message"
        })),
        "bcode.web-search.fetch_result" => Some(serde_json::json!({
            "url": "https://example.com/fixture",
            "final_url": "https://example.com/fixture-final",
            "status": 200,
            "title": "fixture page",
            "content_type": "text/html",
            "content_format": "markdown",
            "rendered": true,
            "truncated": false,
            "markdown": "fixture body"
        })),
        "bcode.question.outcome" => Some(serde_json::json!({
            "status": "answered",
            "questions": [{
                "question_index": 0,
                "header": "Choice",
                "question": "Choose one",
                "status": "answered",
                "selected": [{"label": "Fixture", "value": "fixture"}],
                "custom": null,
                "required": true
            }]
        })),
        "bcode.web-search.status" => Some(serde_json::json!({
            "search": {"available": true, "provider": "fixture", "quality": "native"},
            "fetch": {"available": true, "rendered_fetch": true, "max_bytes": 1024}
        })),
        "bcode.web-search.inspect_result" => Some(serde_json::json!({
            "url": "https://example.com",
            "kind": "web_page",
            "recommended_tool": "web_fetch",
            "recommended_action": "Fetch the page",
            "notes": ["Fixture note"]
        })),
        _ => worktree_artifact_fixture(schema),
    }
}

fn worktree_artifact_fixture(schema: &str) -> Option<serde_json::Value> {
    match schema {
        "bcode.worktree.list" => Some(serde_json::json!({
            "main_root": "/tmp/fixture-repo",
            "worktrees": [{
                "path": "/tmp/fixture-worktree",
                "is_main": false,
                "branch": "fixture-branch",
                "commit": "abc1234"
            }]
        })),
        "bcode.worktree.create_result" => Some(serde_json::json!({
            "repo_root": "/tmp/fixture-repo",
            "path": "/tmp/fixture-worktree",
            "branch": "fixture-branch",
            "created_branch": true,
            "setup_applied": false
        })),
        "bcode.worktree.remove_result" => Some(serde_json::json!({
            "path": "/tmp/fixture-worktree"
        })),
        _ => None,
    }
}

fn visual_adapter_fixture_payload(schema: &str) -> serde_json::Value {
    match schema {
        "bcode.tool.request.shell.run" => {
            serde_json::json!({"command": "echo fixture", "cwd": "/tmp"})
        }
        "bcode.web-search.search_request" => serde_json::json!({
            "arguments": {
                "query": "fixture query",
                "provider": "fixture-provider",
                "site": "example.com"
            }
        }),
        "bcode.web-search.fetch_request" => serde_json::json!({
            "arguments": {
                "url": "https://example.com/fixture",
                "provider": "fixture-provider",
                "render": true
            }
        }),
        "bcode.git.clone_request" => serde_json::json!({
            "arguments": {
                "url": "https://github.com/fixture-owner/fixture-repo.git",
                "ref": "main",
                "destination": "/tmp/fixture-repo"
            }
        }),
        "bcode.worktree.request" => serde_json::json!({
            "arguments": {
                "operation": "create",
                "path": "/tmp/fixture-worktree",
                "branch": "fixture-branch",
                "base_ref": "head"
            }
        }),
        "bcode.vim-edit.live" => serde_json::json!({
            "phase": "running",
            "path": "/tmp/fixture.rs",
            "file_index": 0,
            "file_total": 1,
            "step_index": 0,
            "step_total": 1,
            "cursor": {"line": 1, "column": 1},
            "changed": true,
            "message": "fixture live edit",
            "error": null
        }),
        "bcode.vim-edit.playback" => serde_json::json!({
            "success": true,
            "error": null,
            "tool_mode": "preview",
            "summary": "fixture playback",
            "path": "/tmp/fixture.rs",
            "changed": true,
            "diff": "-old\n+fixture",
            "diff_truncated": false,
            "frame_count": 1,
            "frames_truncated": false
        }),
        "bcode.vim-edit.request.preview" | "bcode.vim-edit.request.apply" => {
            serde_json::json!({
                "arguments": {
                    "path": "/tmp/fixture.txt",
                    "steps": [{"keys": "ifixture<Esc>"}],
                    "sandbox": "default",
                    "timeout_ms": 1000
                }
            })
        }
        _ => serde_json::json!({"operation": "fixture", "path": "/tmp/fixture"}),
    }
}

#[test]
fn web_search_request_adapter_renders_query_options_and_fallback() {
    let visual = PluginVisualView::from(PluginVisualDescriptor {
        visual_id: Some("web-search-1".to_owned()),
        producer_plugin_id: Some("bcode.web-search".to_owned()),
        schema: "bcode.web-search.search_request".to_owned(),
        schema_version: 1,
        title: Some("Web search".to_owned()),
        subtitle: None,
        payload: serde_json::json!({
            "arguments": {
                "query": "renderer neutral app",
                "provider": "brave",
                "site": "example.com",
                "freshness": "week",
                "region": "us",
                "safe_search": "moderate",
                "max_results": 5
            }
        }),
    });

    let rendered = format!("{:?}", render_plugin_visual("plugin visual", &visual));
    assert!(rendered.contains("renderer neutral app"));
    assert!(rendered.contains("brave"));
    assert!(rendered.contains("example.com"));
    assert!(rendered.contains("max results"));
    assert!(rendered.contains("bcode.web-search.search_request"));
}

#[test]
fn web_fetch_request_adapter_renders_url_options_and_fallback() {
    let visual = PluginVisualView::from(PluginVisualDescriptor {
        visual_id: Some("web-fetch-1".to_owned()),
        producer_plugin_id: Some("bcode.web-search".to_owned()),
        schema: "bcode.web-search.fetch_request".to_owned(),
        schema_version: 1,
        title: Some("Fetch page".to_owned()),
        subtitle: None,
        payload: serde_json::json!({
            "arguments": {
                "url": "https://example.com/page",
                "provider": "rendered",
                "render": true,
                "max_bytes": 4096,
                "timeout_ms": 1000,
                "prompt": "extract summary"
            }
        }),
    });

    let rendered = format!("{:?}", render_plugin_visual("plugin visual", &visual));
    assert!(rendered.contains("https://example.com/page"));
    assert!(rendered.contains("rendered"));
    assert!(rendered.contains("max bytes"));
    assert!(rendered.contains("extract summary"));
    assert!(rendered.contains("bcode.web-search.fetch_request"));
}

#[test]
fn terminal_tool_item_supersedes_its_request_only_slot_in_web_timeline() {
    let tool = |status| ToolInvocationView {
        tool_call_id: "call-one-card".to_owned(),
        producer_plugin_id: None,
        tool_name: Some("one.card".to_owned()),
        arguments_json: Some("{}".to_owned()),
        working_directory: None,
        request_visual: None,
        status,
        result_text: None,
        is_error: None,
        result: None,
        output: None,
        timing: ToolTimingView::default(),
    };
    let items = vec![
        TranscriptViewItem {
            id: TranscriptViewItemId::new("tool-request:call-one-card"),
            sequence: Some(1),
            timestamp_ms: None,
            revision: 2,
            streaming: false,
            kind: TranscriptViewItemKind::ToolRequest {
                tool: Box::new(tool(ToolInvocationViewStatus::Running)),
            },
        },
        TranscriptViewItem {
            id: TranscriptViewItemId::new("tool:call-one-card"),
            sequence: Some(2),
            timestamp_ms: None,
            revision: 1,
            streaming: false,
            kind: TranscriptViewItemKind::ToolInvocation {
                tool: Box::new(tool(ToolInvocationViewStatus::Cancelled)),
            },
        },
    ];

    assert!(is_superseded_tool_request(&items, 0));
    assert!(!is_superseded_tool_request(&items, 1));
}

#[test]
fn active_invocations_only_include_operations_missing_from_transcript() {
    let lifecycle = |id: &str| bcode_session_models::ToolInvocationLifecycleEvent {
        invocation_id: id.to_owned(),
        sequence: 1,
        stage: bcode_session_models::ToolInvocationLifecycleStage::Progress,
        message: Some(format!("running {id}")),
        metadata: serde_json::Value::Null,
    };
    let tool = ToolInvocationView {
        tool_call_id: "represented".to_owned(),
        producer_plugin_id: None,
        tool_name: Some("represented.tool".to_owned()),
        arguments_json: None,
        working_directory: None,
        request_visual: None,
        status: ToolInvocationViewStatus::Running,
        result_text: None,
        is_error: None,
        result: None,
        output: None,
        timing: ToolTimingView::default(),
    };
    let mut snapshot = SessionViewSnapshot::empty();
    snapshot
        .active_invocations
        .insert("represented".to_owned(), lifecycle("represented"));
    snapshot
        .active_invocations
        .insert("orphan".to_owned(), lifecycle("orphan"));
    snapshot.transcript.items.push(TranscriptViewItem {
        id: TranscriptViewItemId::new("tool:represented"),
        sequence: Some(1),
        timestamp_ms: None,
        revision: 1,
        streaming: true,
        kind: TranscriptViewItemKind::ToolInvocation {
            tool: Box::new(tool),
        },
    });

    let active = unrepresented_active_invocations(&snapshot);
    assert!(!active.contains_key("represented"));
    assert!(active.contains_key("orphan"));
}

#[test]
fn empty_terminal_tool_result_remains_an_explicit_finished_card() {
    let containers = render_tool_lifecycle(&ToolInvocationView {
        tool_call_id: "call-empty".to_owned(),
        producer_plugin_id: None,
        tool_name: Some("empty.result".to_owned()),
        arguments_json: None,
        working_directory: None,
        request_visual: None,
        status: ToolInvocationViewStatus::Finished,
        result_text: Some(String::new()),
        is_error: Some(false),
        result: None,
        output: None,
        timing: ToolTimingView::default(),
    });
    let mut text = String::new();
    for container in &containers {
        container_text(container, &mut text);
    }

    assert!(text.contains("empty.result"));
    assert!(text.contains("finished"));
    assert!(text.contains("result"));
}

#[test]
fn tool_lifecycle_bounds_raw_arguments_output_and_result_text() {
    let long_arguments = "a".repeat(8_100);
    let long_output = "o".repeat(32_100);
    let rendered = format!(
        "{:?}",
        render_tool_lifecycle(&ToolInvocationView {
            tool_call_id: "call-bounded".to_owned(),
            producer_plugin_id: None,
            tool_name: Some("bounded.tool".to_owned()),
            arguments_json: Some(long_arguments.clone()),
            working_directory: None,
            request_visual: None,
            status: ToolInvocationViewStatus::Running,
            result_text: Some(long_output.clone()),
            is_error: None,
            result: None,
            output: Some(bcode_session_view_models::ToolOutputView {
                text: long_output.clone(),
                columns: None,
                rows: None,
            }),
            timing: ToolTimingView::default(),
        })
    );

    assert!(rendered.contains("developer arguments"));
    assert!(rendered.contains("Arguments truncated for display."));
    assert!(rendered.contains("Output truncated for display"));
    assert!(rendered.contains("Result truncated for display."));
    assert!(!rendered.contains(&long_arguments));
    assert!(!rendered.contains(&long_output));
}

#[test]
fn tool_lifecycle_card_covers_request_running_success_failure_and_timeout() {
    let base = ToolInvocationView {
        tool_call_id: "call-lifecycle".to_owned(),
        producer_plugin_id: Some("example.plugin".to_owned()),
        tool_name: Some("example.tool".to_owned()),
        arguments_json: Some("{\"path\":\"/tmp/example\"}".to_owned()),
        working_directory: Some(std::path::PathBuf::from("/tmp/project")),
        request_visual: None,
        status: ToolInvocationViewStatus::Requested,
        result_text: None,
        is_error: None,
        result: None,
        output: None,
        timing: ToolTimingView::default(),
    };
    let requested = format!("{:?}", render_tool_lifecycle(&base));
    let running = format!(
        "{:?}",
        render_tool_lifecycle(&ToolInvocationView {
            status: ToolInvocationViewStatus::Running,
            output: Some(bcode_session_view_models::ToolOutputView {
                text: "live output".to_owned(),
                columns: Some(80),
                rows: Some(24),
            }),
            ..base.clone()
        })
    );
    let finished = format!(
        "{:?}",
        render_tool_lifecycle(&ToolInvocationView {
            status: ToolInvocationViewStatus::Finished,
            result_text: Some("done".to_owned()),
            timing: ToolTimingView {
                duration_ms: Some(1_234),
                ..ToolTimingView::default()
            },
            ..base.clone()
        })
    );
    let cancelled = format!(
        "{:?}",
        render_tool_lifecycle(&ToolInvocationView {
            status: ToolInvocationViewStatus::Cancelled,
            result_text: Some("cancelled by user".to_owned()),
            ..base.clone()
        })
    );
    let failed = format!(
        "{:?}",
        render_tool_lifecycle(&ToolInvocationView {
            status: ToolInvocationViewStatus::Failed,
            result_text: Some("failed result".to_owned()),
            is_error: Some(true),
            ..base.clone()
        })
    );
    let timed_out = format!(
        "{:?}",
        render_tool_lifecycle(&ToolInvocationView {
            status: ToolInvocationViewStatus::Finished,
            timing: ToolTimingView {
                timed_out: Some(true),
                ..ToolTimingView::default()
            },
            ..base
        })
    );

    assert!(requested.contains("requested"));
    assert!(requested.contains("arguments"));
    assert!(requested.contains("example.tool"));
    assert!(running.contains("running"));
    assert!(running.contains("live output"));
    assert!(finished.contains("finished"));
    assert!(finished.contains("1.234s"));
    assert!(finished.contains("done"));
    assert!(cancelled.contains("cancelled"));
    assert!(cancelled.contains("cancelled by user"));
    assert!(failed.contains("failed"));
    assert!(timed_out.contains("timed out"));
}

#[test]
fn generic_json_fallback_is_bounded_and_marks_truncation() {
    let sentinel = "j".repeat(40_000);
    let containers = json_panel(
        "developer payload",
        &serde_json::json!({"sentinel": sentinel}),
    );
    let mut text = String::new();
    for container in &containers {
        container_text(container, &mut text);
    }

    assert!(text.contains("developer payload"));
    assert!(text.contains("Structured details truncated for display."));
    assert!(text.matches('j').count() <= 32_000);
}

#[test]
fn unknown_artifact_and_text_json_results_keep_readable_fallbacks() {
    let unknown = ToolResultView::Artifact {
        artifact: ToolArtifactView::from(ToolArtifact {
            artifact_id: "unknown-artifact".to_owned(),
            producer_plugin_id: "example.plugin".to_owned(),
            schema: "example.unknown".to_owned(),
            schema_version: 9,
            tool_call_id: Some("call-unknown".to_owned()),
            title: Some("Unknown result".to_owned()),
            metadata: serde_json::json!({"sentinel": "preserved"}),
            refs: Vec::new(),
        }),
    };
    let text = ToolResultView::Text {
        text: "plain output".to_owned(),
    };
    let json = ToolResultView::Json {
        value: serde_json::json!({"answer": 42}).to_string(),
    };
    let malformed = ToolResultView::Json {
        value: "{malformed".to_owned(),
    };

    let unknown = format!("{:?}", render_tool_result(&unknown));
    let text = format!("{:?}", render_tool_result(&text));
    let json = format!("{:?}", render_tool_result(&json));
    let malformed = format!("{:?}", render_tool_result(&malformed));
    assert!(unknown.contains("artifact details"));
    assert!(unknown.contains("preserved"));
    assert!(text.contains("plain output"));
    assert!(!text.contains("semantic result"));
    assert!(json.contains("result details"));
    assert!(json.contains("answer"));
    assert!(malformed.contains("{malformed"));
}

fn git_worktree_result(schema: &str, title: &str, metadata: serde_json::Value) -> ToolResultView {
    ToolResultView::Artifact {
        artifact: ToolArtifactView::from(ToolArtifact {
            artifact_id: format!("fixture-{schema}"),
            producer_plugin_id: if schema.starts_with("bcode.git") {
                "bcode.git".to_owned()
            } else {
                "bcode.worktree".to_owned()
            },
            schema: schema.to_owned(),
            schema_version: 1,
            tool_call_id: Some("call-git-worktree".to_owned()),
            title: Some(title.to_owned()),
            metadata,
            refs: Vec::new(),
        }),
    }
}

#[test]
fn vim_edit_adapters_cover_request_live_preview_apply_result_and_failure() {
    let visual = |schema: &str, payload| {
        PluginVisualView::from(PluginVisualDescriptor {
            visual_id: Some(format!("fixture-{schema}")),
            producer_plugin_id: Some("bcode.vim-edit".to_owned()),
            schema: schema.to_owned(),
            schema_version: 1,
            title: Some("Vim edit".to_owned()),
            subtitle: None,
            payload,
        })
    };
    let request = visual(
        "bcode.vim-edit.request.preview",
        serde_json::json!({
            "path": "/tmp/example.rs",
            "steps": [{"keys": "gg"}, {"insert": "hello"}],
            "sandbox": "default",
            "timeout_ms": 1000
        }),
    );
    let live = visual(
        "bcode.vim-edit.live",
        serde_json::json!({
            "phase": "running",
            "path": "/tmp/example.rs",
            "file_index": 0,
            "file_total": 2,
            "step_index": 1,
            "step_total": 3,
            "cursor": {"line": 4, "column": 2},
            "changed": true,
            "message": "applying step",
            "error": null
        }),
    );
    let playback = visual(
        "bcode.vim-edit.playback",
        serde_json::json!({
            "success": true,
            "error": null,
            "tool_mode": "preview",
            "summary": "vim edit changed file",
            "path": "/tmp/example.rs",
            "changed": true,
            "diff": format!("-old\n+new\n{}", "d".repeat(33_000)),
            "diff_truncated": true,
            "frame_count": 20,
            "frames_truncated": true
        }),
    );
    let failure = visual(
        "bcode.vim-edit.playback",
        serde_json::json!({
            "success": false,
            "error": "Neovim command failed",
            "tool_mode": "apply",
            "summary": "vim edit failed",
            "path": "/tmp/example.rs",
            "changed": false,
            "diff": "",
            "diff_truncated": false,
            "frame_count": 0,
            "frames_truncated": false
        }),
    );

    let request = render_plugin_visual("request", &request);
    let live = render_plugin_visual("live", &live);
    let playback = render_plugin_visual("result", &playback);
    let failure = render_plugin_visual("result", &failure);
    let text = |containers: &hyperchad::template::Containers| {
        let mut text = String::new();
        for container in containers {
            container_text(container, &mut text);
        }
        text
    };
    let request = text(&request);
    let live = text(&live);
    let playback = text(&playback);
    let failure = text(&failure);
    assert!(request.contains("/tmp/example.rs"));
    assert!(request.contains("steps: 2"));
    assert!(request.contains("sandbox: default"));
    assert!(live.contains("running"));
    assert!(live.contains("file 1 of 2"));
    assert!(live.contains("step 2 of 3"));
    assert!(live.contains("cursor 4:2"));
    assert!(playback.contains("preview"));
    assert!(playback.contains("changed"));
    assert!(playback.contains("20 playback frames"));
    assert!(playback.contains("Diff was truncated by the producer."));
    assert!(playback.contains("Diff truncated for display."));
    assert!(playback.contains("Playback frames were truncated."));
    assert!(failure.contains("failed"));
    assert!(failure.contains("Neovim command failed"));
    assert!(failure.contains("apply"));
}

#[test]
fn git_clone_adapter_covers_request_result_ref_path_and_status() {
    let request = PluginVisualView::from(PluginVisualDescriptor {
        visual_id: Some("git-request".to_owned()),
        producer_plugin_id: Some("bcode.git".to_owned()),
        schema: "bcode.git.clone_request".to_owned(),
        schema_version: 1,
        title: Some("Clone".to_owned()),
        subtitle: None,
        payload: serde_json::json!({
            "url": "https://github.com/example/repo.git",
            "ref": "feature/ref",
            "destination": "/tmp/repo"
        }),
    });
    let result = git_worktree_result(
        "bcode.git.clone_result",
        "Repository cloned",
        serde_json::json!({
            "host": "github.com",
            "owner": "example",
            "repo": "repo",
            "clone_url": "https://github.com/example/repo.git",
            "git_ref": "feature/ref",
            "path": "/tmp/repo",
            "already_exists": false
        }),
    );
    let request = format!("{:?}", render_plugin_visual("request", &request));
    let result = format!("{:?}", render_tool_result(&result));

    for expected in [
        "https://github.com/example/repo.git",
        "feature/ref",
        "/tmp/repo",
    ] {
        assert!(request.contains(expected));
        assert!(result.contains(expected));
    }
    assert!(result.contains("repository cloned"));
}

#[test]
fn worktree_adapters_cover_request_results_empty_state_and_error_context() {
    let request = PluginVisualView::from(PluginVisualDescriptor {
        visual_id: Some("worktree-request".to_owned()),
        producer_plugin_id: Some("bcode.worktree".to_owned()),
        schema: "bcode.worktree.request".to_owned(),
        schema_version: 1,
        title: Some("Create worktree".to_owned()),
        subtitle: None,
        payload: serde_json::json!({
            "operation": "create",
            "name": "renderer",
            "cwd": "/tmp/repo",
            "new_branch": "feature/renderer",
            "base_ref": "head",
            "force": true
        }),
    });
    let empty_list = git_worktree_result(
        "bcode.worktree.list",
        "Worktrees",
        serde_json::json!({
            "repo_root": "/tmp/repo",
            "current_worktree": "/tmp/repo",
            "worktrees": []
        }),
    );
    let create_result = git_worktree_result(
        "bcode.worktree.create_result",
        "Worktree created",
        serde_json::json!({
            "repo_root": "/tmp/repo",
            "path": "/tmp/worktrees/renderer",
            "branch": "feature/renderer",
            "created_branch": true,
            "setup_applied": true,
            "session": {"name": "Renderer session"}
        }),
    );
    let remove_result = git_worktree_result(
        "bcode.worktree.remove_result",
        "Worktree removed",
        serde_json::json!({"path": "/tmp/worktrees/renderer"}),
    );
    let failed = ToolInvocationView {
        tool_call_id: "call-worktree-failed".to_owned(),
        producer_plugin_id: Some("bcode.worktree".to_owned()),
        tool_name: Some("worktree.create".to_owned()),
        arguments_json: None,
        working_directory: Some("/tmp/repo".into()),
        request_visual: Some(request.clone()),
        status: ToolInvocationViewStatus::Failed,
        result_text: Some("git worktree add failed".to_owned()),
        is_error: Some(true),
        result: None,
        output: None,
        timing: ToolTimingView::default(),
    };
    let request = format!("{:?}", render_plugin_visual("request", &request));
    let empty_list = format!("{:?}", render_tool_result(&empty_list));
    let create_result = format!("{:?}", render_tool_result(&create_result));
    let remove_result = format!("{:?}", render_tool_result(&remove_result));
    let failed = format!("{:?}", render_tool_lifecycle(&failed));

    for expected in ["create", "renderer", "feature/renderer", "base ref", "head"] {
        assert!(request.contains(expected));
    }
    for expected in ["repository: ", "current: ", "No worktrees found."] {
        assert!(empty_list.contains(expected));
    }
    assert!(create_result.contains("worktree created"));
    assert!(create_result.contains("Renderer session"));
    assert!(remove_result.contains("worktree removed"));
    assert!(failed.contains("failed"));
    assert!(failed.contains("git worktree add failed"));
}

#[test]
fn shell_result_adapter_covers_terminal_captured_error_and_truncation() {
    let render = |metadata| {
        render_tool_result(&ToolResultView::Artifact {
            artifact: ToolArtifactView::from(ToolArtifact {
                artifact_id: "shell-edge".to_owned(),
                producer_plugin_id: "bcode.shell".to_owned(),
                schema: "bcode.shell.run".to_owned(),
                schema_version: 1,
                tool_call_id: Some("call-shell".to_owned()),
                title: Some("Shell run".to_owned()),
                metadata,
                refs: Vec::new(),
            }),
        })
    };
    let terminal = render(serde_json::json!({
        "mode": "terminal",
        "exit_code": 0,
        "timed_out": false,
        "cancelled": false,
        "duration_ms": 1234,
        "output_tail": "terminal output",
        "output_truncated": false,
        "output_bytes": 15,
        "retained_output_bytes": 15,
        "columns": 100,
        "rows": 30,
        "format_commands": true
    }));
    let captured = render(serde_json::json!({
        "mode": "captured",
        "exit_code": 2,
        "timed_out": false,
        "cancelled": false,
        "duration_ms": 50,
        "stdout": "captured stdout",
        "stderr": "e".repeat(33_000),
        "stdout_truncated": false,
        "stderr_truncated": true,
        "stdout_bytes": 15,
        "stderr_bytes": 33_000
    }));
    let timed_out = render(serde_json::json!({
        "mode": "terminal",
        "exit_code": null,
        "timed_out": true,
        "cancelled": false,
        "output_tail": "timeout output",
        "output_truncated": true,
        "columns": 80,
        "rows": 24
    }));
    let text = |containers: &hyperchad::template::Containers| {
        let mut text = String::new();
        for container in containers {
            container_text(container, &mut text);
        }
        text
    };
    let terminal = text(&terminal);
    let captured = text(&captured);
    let timed_out = text(&timed_out);

    assert!(terminal.contains("exit 0"));
    assert!(terminal.contains("1234 ms"));
    assert!(terminal.contains("100x30"));
    assert!(terminal.contains("terminal output"));
    assert!(captured.contains("exit 2"));
    assert!(captured.contains("captured stdout"));
    assert!(captured.contains("stderr"));
    assert!(captured.contains("Shell output was truncated by the producer."));
    assert!(captured.contains("Shell output truncated for display."));
    assert!(timed_out.contains("timed out"));
    assert!(timed_out.contains("timeout output"));
}

#[test]
fn web_search_result_adapter_handles_empty_partial_and_long_results() {
    let render = |metadata| {
        render_tool_result(&ToolResultView::Artifact {
            artifact: ToolArtifactView::from(ToolArtifact {
                artifact_id: "web-search-edge".to_owned(),
                producer_plugin_id: "bcode.web-search".to_owned(),
                schema: "bcode.web-search.search_results".to_owned(),
                schema_version: 1,
                tool_call_id: Some("call-web-search-edge".to_owned()),
                title: Some("Search results".to_owned()),
                metadata,
                refs: Vec::new(),
            }),
        })
    };
    let empty = format!(
        "{:?}",
        render(serde_json::json!({
            "query": "nothing",
            "provider": "fixture-provider",
            "results": [],
            "partial": false
        }))
    );
    let partial = format!(
        "{:?}",
        render(serde_json::json!({
            "query": "partial",
            "provider": "fixture-provider",
            "results": [{
                "title": "Long result",
                "url": "https://example.com/long",
                "snippet": "s".repeat(2_100)
            }],
            "partial": true,
            "message": "provider returned a partial page"
        }))
    );

    assert!(empty.contains("No search results."));
    assert!(empty.contains("fixture-provider"));
    assert!(partial.contains("Long result"));
    assert!(partial.contains("https://example.com/long"));
    assert!(partial.contains("partial results"));
    assert!(partial.contains("provider returned a partial page"));
    assert!(!partial.contains(&"s".repeat(2_100)));
    assert!(partial.contains('…'));
}

#[test]
fn web_search_result_adapter_renders_results_without_redundant_fallback() {
    let artifact = ToolArtifactView::from(ToolArtifact {
        artifact_id: "web-search-result".to_owned(),
        producer_plugin_id: "bcode.web-search".to_owned(),
        schema: "bcode.web-search.search_results".to_owned(),
        schema_version: 1,
        tool_call_id: Some("call-web-search".to_owned()),
        title: Some("Search results".to_owned()),
        metadata: serde_json::json!({
            "query": "rust tui web renderer",
            "provider": "brave",
            "results": [{
                "title": "Renderer Neutral",
                "url": "https://example.com/renderer",
                "snippet": "A renderer-neutral search result"
            }],
            "partial": false,
            "message": "ok"
        }),
        refs: Vec::new(),
    });
    let result = ToolResultView::Artifact { artifact };

    let rendered = format!("{:?}", render_tool_result(&result));
    assert!(rendered.contains("rust tui web renderer"));
    assert!(rendered.contains("Renderer Neutral"));
    assert!(rendered.contains("https://example.com/renderer"));
    assert!(!rendered.contains("semantic result"));
    assert!(!rendered.contains("artifact details"));
}

#[test]
fn web_fetch_result_adapter_bounds_markdown_and_handles_empty_unsafe_sources() {
    let render = |metadata| {
        render_tool_result(&ToolResultView::Artifact {
            artifact: ToolArtifactView::from(ToolArtifact {
                artifact_id: "web-fetch-edge".to_owned(),
                producer_plugin_id: "bcode.web-search".to_owned(),
                schema: "bcode.web-search.fetch_result".to_owned(),
                schema_version: 1,
                tool_call_id: Some("call-web-fetch-edge".to_owned()),
                title: Some("Fetched page".to_owned()),
                metadata,
                refs: Vec::new(),
            }),
        })
    };
    let bounded = format!(
        "{:?}",
        render(serde_json::json!({
            "url": "https://example.com/source",
            "final_url": "https://example.com/final",
            "status": 200,
            "content_type": "text/html",
            "content_format": "markdown",
            "rendered": true,
            "truncated": true,
            "markdown": format!("# Extracted heading\n\n{}", "m".repeat(33_000))
        }))
    );
    let empty = format!(
        "{:?}",
        render(serde_json::json!({
            "url": "javascript:alert(1)",
            "status": 204,
            "content_format": "text",
            "truncated": false
        }))
    );

    assert!(bounded.contains("Extracted heading"));
    assert!(bounded.contains("https://example.com/final"));
    assert!(bounded.contains("source: "));
    assert!(bounded.contains("https://example.com/source"));
    assert!(bounded.contains("Source content was truncated."));
    assert!(bounded.contains("Fetched content truncated for display."));
    assert!(!bounded.contains(&format!("# Extracted heading\n\n{}", "m".repeat(33_000))));
    assert!(empty.contains("javascript:alert(1)"));
    assert!(!empty.contains("href: Some(\"javascript:alert(1)\")"));
    assert!(empty.contains("No extracted content was returned."));
}

#[test]
fn web_fetch_result_adapter_renders_metadata_preview_without_redundant_fallback() {
    let artifact = ToolArtifactView::from(ToolArtifact {
        artifact_id: "web-fetch-result".to_owned(),
        producer_plugin_id: "bcode.web-search".to_owned(),
        schema: "bcode.web-search.fetch_result".to_owned(),
        schema_version: 1,
        tool_call_id: Some("call-web-fetch".to_owned()),
        title: Some("Fetched page".to_owned()),
        metadata: serde_json::json!({
            "url": "https://example.com/original",
            "final_url": "https://example.com/final",
            "status": 200,
            "title": "Example page",
            "content_type": "text/html",
            "content_format": "markdown",
            "rendered": true,
            "truncated": false,
            "markdown": "# Sentinel preview"
        }),
        refs: Vec::new(),
    });
    let result = ToolResultView::Artifact { artifact };

    let rendered = format!("{:?}", render_tool_result(&result));
    assert!(rendered.contains("Example page"));
    assert!(rendered.contains("https://example.com/final"));
    assert!(rendered.contains("Sentinel preview"));
    assert!(!rendered.contains("semantic result"));
    assert!(!rendered.contains("artifact details"));
}

#[test]
fn filesystem_change_adapter_renders_path_and_diff_fields_with_fallback() {
    let visual = PluginVisualView::from(PluginVisualDescriptor {
        visual_id: Some("change-1".to_owned()),
        producer_plugin_id: Some("bcode.filesystem".to_owned()),
        schema: "bcode.filesystem.change".to_owned(),
        schema_version: 1,
        title: Some("Edit file".to_owned()),
        subtitle: None,
        payload: serde_json::json!({
            "path": "/tmp/example.rs",
            "old_text": "old();",
            "new_text": "new();"
        }),
    });

    let rendered = format!("{:?}", render_plugin_visual("plugin visual", &visual));
    assert!(rendered.contains("/tmp/example.rs"));
    assert!(rendered.contains("old();"));
    assert!(rendered.contains("new();"));
    assert!(rendered.contains("bcode.filesystem.change"));
}

#[test]
fn generic_tool_artifact_keeps_schema_metadata_in_render_tree() {
    let artifact = ToolArtifactView::from(ToolArtifact {
        artifact_id: "artifact-1".to_owned(),
        producer_plugin_id: "fixture-plugin".to_owned(),
        schema: "fixture.artifact".to_owned(),
        schema_version: 1,
        tool_call_id: Some("call-1".to_owned()),
        title: Some("Fixture artifact".to_owned()),
        metadata: serde_json::json!({"sentinel": "artifact-metadata"}),
        refs: Vec::new(),
    });
    let kind = TranscriptViewItemKind::ToolInvocation {
        tool: Box::new(ToolInvocationView {
            tool_call_id: "call-1".to_owned(),
            producer_plugin_id: Some("fixture-plugin".to_owned()),
            tool_name: Some("fixture".to_owned()),
            arguments_json: None,
            working_directory: None,
            request_visual: None,
            status: ToolInvocationViewStatus::Finished,
            result_text: None,
            is_error: Some(false),
            result: Some(ToolResultView::Artifact { artifact }),
            output: None,
            timing: ToolTimingView::default(),
        }),
    };

    let rendered = format!("{:?}", transcript_item_body(&kind));
    assert!(rendered.contains("fixture.artifact"));
    assert!(rendered.contains("artifact-metadata"));
}
