//! TUI tests.

use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use bcode_agent_profile::AgentInfo;
use bcode_client::AttachedSessionHistory;
use bcode_config::{
    TuiAccentTransitionCurve, TuiAccentTransitionMode, TuiConfig, TuiThemeConfig, TuiThinkingConfig,
};
use bcode_session_models::{
    ClientId, LegacyLegacyToolPresentationFieldKind, LegacyToolPluginViewPresentation,
    LegacyToolPresentationEvent, LegacyToolPresentationField, LegacyToolPresentationLevel,
    LegacyToolPresentationSection, LegacyToolPresentationTarget,
    LegacyToolRequestPresentationMetadata, LegacyToolRequestPreviewMetadata,
    LegacyToolStatusPresentation, LiveFileEditPreview, LivePluginViewPreview, LiveQueryPreview,
    LiveShellCommandPreview, LiveToolArgumentPreview, RuntimeWorkId, RuntimeWorkKind, SessionEvent,
    SessionEventKind, SessionId, SessionInputHistoryEntry, SessionProjectionKind, SessionSummary,
    SessionTitleSource, SessionTokenUsage, SessionTraceEvent, SessionTracePayload,
    SessionTracePhase, ShellRunResult, ToolArtifact, ToolArtifactRef, ToolInvocationResult,
    ToolInvocationStreamEvent, ToolOutputStream, build_tool_invocation_projections,
};
use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
use bmux_text_edit::TextMotion;
use bmux_tui::buffer::Buffer;
use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect};

use super::{
    app::{BmuxApp, KeyActivationOutcome},
    input,
    input::KeyRequest,
    keymap::{BmuxAction, BmuxKeyActivation, BmuxKeyBinding, BmuxKeyMap, BmuxScope},
    pending_submissions::PendingSubmissions,
    render, slash_palette, slash_palette_render,
    temporal::next_elapsed_invalidation_capped,
    time_format::{format_duration_nanos, format_millis},
    transcript::{TranscriptItem, TranscriptItemKind, transcript_items_from_events_with_reasoning},
    transcript_document::TranscriptDocument,
};

fn shell_legacy_request_presentation() -> LegacyToolRequestPresentationMetadata {
    LegacyToolRequestPresentationMetadata {
        title: "Shell command".to_owned(),
        fields: vec![
            LegacyToolPresentationField {
                label: "command".to_owned(),
                argument: "command".to_owned(),
                kind: LegacyLegacyToolPresentationFieldKind::Command,
                optional: false,
            },
            LegacyToolPresentationField {
                label: "cwd".to_owned(),
                argument: "cwd".to_owned(),
                kind: LegacyLegacyToolPresentationFieldKind::Path,
                optional: true,
            },
            LegacyToolPresentationField {
                label: "terminal".to_owned(),
                argument: "terminal".to_owned(),
                kind: LegacyLegacyToolPresentationFieldKind::Boolean,
                optional: true,
            },
        ],
        preview: None,
    }
}

fn file_edit_legacy_request_presentation() -> LegacyToolRequestPresentationMetadata {
    LegacyToolRequestPresentationMetadata {
        title: "Edit file".to_owned(),
        fields: vec![
            LegacyToolPresentationField {
                label: "Path".to_owned(),
                argument: "path".to_owned(),
                kind: LegacyLegacyToolPresentationFieldKind::Path,
                optional: false,
            },
            LegacyToolPresentationField {
                label: "Old text".to_owned(),
                argument: "old_text".to_owned(),
                kind: LegacyLegacyToolPresentationFieldKind::Text,
                optional: false,
            },
            LegacyToolPresentationField {
                label: "New text".to_owned(),
                argument: "new_text".to_owned(),
                kind: LegacyLegacyToolPresentationFieldKind::Text,
                optional: false,
            },
        ],
        preview: Some(LegacyToolRequestPreviewMetadata::FileEdit {
            path_fields: vec!["path".to_owned()],
            old_text_fields: vec!["old_text".to_owned()],
            new_text_fields: vec!["new_text".to_owned(), "contents".to_owned()],
        }),
    }
}

fn theme_transition_config(curve: TuiAccentTransitionCurve) -> TuiConfig {
    TuiConfig {
        theme: TuiThemeConfig {
            accent_transition: TuiAccentTransitionMode::Transition,
            accent_transition_ms: 100,
            accent_transition_curve: curve,
        },
        ..TuiConfig::default()
    }
}

fn disable_theme_transition(app: &mut BmuxApp) {
    app.apply_tui_config(TuiConfig {
        theme: TuiThemeConfig {
            accent_transition: TuiAccentTransitionMode::Immediate,
            ..TuiThemeConfig::default()
        },
        ..TuiConfig::default()
    });
}

#[test]
fn theme_transition_curves_shape_midpoint_progress() {
    let target = bmux_tui::style::Color::Rgb(100, 0, 0);
    let started_at = Instant::now();

    let mut linear = BmuxApp::new_with_history(None, &[], &[], false);
    linear.apply_tui_config(theme_transition_config(TuiAccentTransitionCurve::Linear));
    assert_eq!(
        linear.animated_accent(target, started_at),
        bmux_tui::style::Color::Rgb(100, 116, 139)
    );
    assert_eq!(
        linear.animated_accent(target, started_at + Duration::from_millis(50)),
        bmux_tui::style::Color::Rgb(100, 58, 70)
    );

    let mut ease_in = BmuxApp::new_with_history(None, &[], &[], false);
    ease_in.apply_tui_config(theme_transition_config(TuiAccentTransitionCurve::EaseIn));
    assert_eq!(
        ease_in.animated_accent(target, started_at),
        bmux_tui::style::Color::Rgb(100, 116, 139)
    );
    assert_eq!(
        ease_in.animated_accent(target, started_at + Duration::from_millis(50)),
        bmux_tui::style::Color::Rgb(100, 102, 122)
    );

    let mut ease_out = BmuxApp::new_with_history(None, &[], &[], false);
    ease_out.apply_tui_config(theme_transition_config(TuiAccentTransitionCurve::EaseOut));
    assert_eq!(
        ease_out.animated_accent(target, started_at),
        bmux_tui::style::Color::Rgb(100, 116, 139)
    );
    assert_eq!(
        ease_out.animated_accent(target, started_at + Duration::from_millis(50)),
        bmux_tui::style::Color::Rgb(100, 15, 18)
    );
}

#[test]
fn immediate_theme_transition_ignores_curve() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    app.apply_tui_config(TuiConfig {
        theme: TuiThemeConfig {
            accent_transition: TuiAccentTransitionMode::Immediate,
            accent_transition_ms: 100,
            accent_transition_curve: TuiAccentTransitionCurve::EaseIn,
        },
        ..TuiConfig::default()
    });

    assert_eq!(
        app.animated_accent(bmux_tui::style::Color::Rgb(1, 2, 3), Instant::now()),
        bmux_tui::style::Color::Rgb(1, 2, 3)
    );
}

#[test]
fn provider_tool_call_delta_trace_does_not_replace_status() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    app.set_status("thinking".to_owned());
    app.absorb_session_event(&SessionEvent {
        schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        sequence: 1,
        timestamp_ms: 1,
        session_id,
        provenance: None,
        kind: SessionEventKind::TraceEvent {
            trace: Box::new(SessionTraceEvent {
                timestamp_ms: 0,
                turn_id: Some("turn-1".to_owned()),
                phase: SessionTracePhase::ModelProviderEvent,
                payload: SessionTracePayload::ProviderEvent {
                    event_type: "tool_call_delta".to_owned(),
                    detail: None,
                },
            }),
        },
    });

    assert_eq!(app.status(), "thinking");
}

#[test]
fn provider_tool_call_progress_status_formats_bytes() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    app.absorb_session_event(&SessionEvent {
        schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        sequence: 1,
        timestamp_ms: 1,
        session_id,
        provenance: None,
        kind: SessionEventKind::TraceEvent {
            trace: Box::new(SessionTraceEvent {
                timestamp_ms: 0,
                turn_id: Some("turn-1".to_owned()),
                phase: SessionTracePhase::ModelProviderEvent,
                payload: SessionTracePayload::ProviderStreamEvent(
                    bcode_session_models::ProviderStreamEvent::ToolCallProgress {
                        tool_call_id: "call-1".to_owned(),
                        tool_name: "example.write".to_owned(),
                        argument_bytes: 1536,
                    },
                ),
            }),
        },
    });

    assert_eq!(
        app.status(),
        "assembling example.write arguments (1.5 KiB received)"
    );
}

#[test]
fn live_provider_tool_call_progress_updates_status() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    app.absorb_session_live_event(&bcode_session_models::SessionLiveEvent {
        session_id,
        kind: bcode_session_models::SessionLiveEventKind::ProviderStreamProgress {
            turn_id: "turn-1".to_owned(),
            event: bcode_session_models::ProviderStreamEvent::ToolCallProgress {
                tool_call_id: "call-1".to_owned(),
                tool_name: "example.write".to_owned(),
                argument_bytes: 4096,
            },
        },
    });

    assert_eq!(
        app.status(),
        "assembling example.write arguments (4.0 KiB received)"
    );
}

#[test]
fn running_tool_elapsed_invalidations_are_frame_capped() {
    let now = std::time::Instant::now();
    let next = next_elapsed_invalidation_capped(
        0,
        None,
        now,
        SystemTime::UNIX_EPOCH + Duration::from_millis(1_200),
        Duration::from_millis(16),
    )
    .expect("running tool schedules elapsed invalidation");

    assert!(next <= now + Duration::from_millis(16));
}

#[test]
fn transcript_document_mutations_bump_revision() {
    let mut document = TranscriptDocument::default();
    assert_eq!(document.revision(), 0);

    document.push(TranscriptItem::new("System", "one".to_owned()));
    assert_eq!(document.revision(), 1);

    document.replace(vec![TranscriptItem::new("You", "two".to_owned())]);
    assert_eq!(document.revision(), 2);

    let item = document.get_mut(0).expect("item exists");
    item.append_text("!");
    assert_eq!(document.revision(), 3);

    let item = document.get_mut(0).expect("item exists");
    item.finish_streaming();
    assert_eq!(document.revision(), 4);
}

#[test]
fn transcript_document_streaming_helpers_bump_revision() {
    let mut document = TranscriptDocument::default();

    document.push_streaming_item("Assistant", "hello");
    assert_eq!(document.revision(), 1);
    assert_eq!(document.items()[0].text(), "hello");

    document.push_streaming_item("Assistant", " world");
    assert_eq!(document.revision(), 2);
    assert_eq!(document.items()[0].text(), "hello world");

    document.finish_streaming_item("Assistant", "hello world");
    assert_eq!(document.revision(), 3);
    assert!(!document.items()[0].streaming());
}

#[test]
fn reasoning_streaming_starts_new_item_after_interleaved_transcript_item() {
    let mut document = TranscriptDocument::default();

    document.push_streaming_item("Reasoning summary", "first thought");
    document.push(TranscriptItem::new("System", "tool output".to_owned()));
    document.push_streaming_item("Reasoning summary", "second thought");

    assert_eq!(document.len(), 3);
    assert_eq!(document.items()[0].text(), "first thought");
    assert_eq!(document.items()[1].text(), "tool output");
    assert_eq!(document.items()[2].text(), "second thought");
    assert!(document.items()[0].streaming());
    assert!(document.items()[2].streaming());
}

#[test]
fn reasoning_finish_preserves_split_streaming_items() {
    let mut document = TranscriptDocument::default();

    document.push_streaming_item("Reasoning summary", "first thought");
    document.push(TranscriptItem::new("System", "tool output".to_owned()));
    document.push_streaming_item("Reasoning summary", "second thought");
    document.finish_streaming_item(
        "Reasoning summary",
        "first thought second thought final aggregate",
    );

    assert_eq!(document.len(), 3);
    assert_eq!(document.items()[0].text(), "first thought");
    assert_eq!(document.items()[2].text(), "second thought");
    assert!(!document.items()[0].streaming());
    assert!(!document.items()[2].streaming());
}

#[test]
fn pending_submissions_mutations_bump_revision() {
    let mut pending = PendingSubmissions::default();
    assert_eq!(pending.revision(), 0);

    pending.stage("hello".to_owned());
    assert_eq!(pending.revision(), 1);

    pending.mark_first_queued(Some(1));
    assert_eq!(pending.revision(), 2);

    pending.mark_first_sent();
    assert_eq!(pending.revision(), 3);

    pending.remove("missing");
    assert_eq!(pending.revision(), 3);

    pending.remove("hello");
    assert_eq!(pending.revision(), 4);
}

#[test]
fn render_includes_status_and_composer() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 10));
    let cursor = {
        let mut frame = Frame::new(&mut buffer);
        render::render(&mut app, &mut frame);
        frame.cursor()
    };

    let output = rendered_text(&buffer);

    assert!(buffer.row_symbols(0).unwrap().contains("bcode"));
    assert!(!output.contains("TUI is attached"));
    assert!(buffer.row_symbols(7).unwrap().contains("Message"));
    assert!(cursor.is_some());
}

#[test]
fn composer_expands_and_scrolls_when_input_exceeds_max_rows() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    app.replace_composer_with("0\n1\n2\n3\n4\n5\n6\n7\n8\n9");
    let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 20));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);

    assert!(buffer.row_symbols(12).unwrap().contains("Message"));
    assert!(output.contains('9'));
}

#[test]
fn duration_formatting_uses_readable_units() {
    assert_eq!(format_duration_nanos(12), "12ns");
    assert_eq!(format_duration_nanos(1_500), "1.5µs");
    assert_eq!(format_duration_nanos(1_500_000), "1.5ms");
    assert_eq!(format_millis(30_000), "30.0s");
    assert_eq!(format_millis(30_100), "30.1s");
    assert_eq!(format_millis(120_000), "2m");
    assert_eq!(format_millis(90_000), "1m 30s");
}

#[test]
fn multiline_paste_preserves_line_breaks_in_composer() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);

    app.paste_composer_text("first\nsecond\r\nthird\rfourth");

    assert_eq!(app.composer().text(), "first\nsecond\nthird\nfourth");
}

#[test]
fn escape_requires_double_tap_to_interrupt_without_exiting_chat() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    let keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());

    let first = input::handle_key(&mut app, &keymap, key(KeyCode::Escape));
    let second = input::handle_key(&mut app, &keymap, key(KeyCode::Escape));

    assert_eq!(first.request, KeyRequest::None);
    assert!(first.redraw);
    assert_eq!(app.status(), "hit esc twice to cancel");
    assert_eq!(second.request, KeyRequest::Interrupt);
    assert!(!app.should_exit());
}

#[test]
fn multi_tap_key_activation_supports_three_taps() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    let now = Instant::now();
    let binding = BmuxKeyBinding::new(
        key(KeyCode::Escape),
        BmuxAction::AppInterrupt,
        BmuxKeyActivation::MultiTap {
            required_taps: 3,
            window_ms: 1_500,
            prompt: "tap again".to_owned(),
        },
    );

    let first = app.activate_key_binding_for_test(BmuxScope::Chat, &binding, now);
    let second = app.activate_key_binding_for_test(
        BmuxScope::Chat,
        &binding,
        now + Duration::from_millis(100),
    );
    let third = app.activate_key_binding_for_test(
        BmuxScope::Chat,
        &binding,
        now + Duration::from_millis(200),
    );

    assert_eq!(first, KeyActivationOutcome::Pending);
    assert_eq!(second, KeyActivationOutcome::Pending);
    assert_eq!(
        third,
        KeyActivationOutcome::Activated(BmuxAction::AppInterrupt)
    );
}

#[test]
fn multi_tap_key_activation_resets_after_timeout() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    let now = Instant::now();
    let binding = BmuxKeyBinding::new(
        key(KeyCode::Escape),
        BmuxAction::AppInterrupt,
        BmuxKeyActivation::MultiTap {
            required_taps: 2,
            window_ms: 500,
            prompt: "tap again".to_owned(),
        },
    );

    let first = app.activate_key_binding_for_test(BmuxScope::Chat, &binding, now);
    let expired_second = app.activate_key_binding_for_test(
        BmuxScope::Chat,
        &binding,
        now + Duration::from_millis(600),
    );

    assert_eq!(first, KeyActivationOutcome::Pending);
    assert_eq!(expired_second, KeyActivationOutcome::Pending);
}

#[test]
fn other_key_resets_pending_multi_tap_activation() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    let keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());

    let escape = input::handle_key(&mut app, &keymap, key(KeyCode::Escape));
    let tab = input::handle_key(&mut app, &keymap, key(KeyCode::Tab));
    let escape_again = input::handle_key(&mut app, &keymap, key(KeyCode::Escape));

    assert_eq!(escape.request, KeyRequest::None);
    assert_eq!(tab.request, KeyRequest::CycleAgent);
    assert_eq!(escape_again.request, KeyRequest::None);
}

#[test]
fn immediate_key_activation_runs_without_pending_state() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    let binding = BmuxKeyBinding::new(
        key(KeyCode::Tab),
        BmuxAction::AgentCycle,
        BmuxKeyActivation::Immediate,
    );

    let outcome = app.activate_key_binding_for_test(BmuxScope::Chat, &binding, Instant::now());

    assert_eq!(
        outcome,
        KeyActivationOutcome::Activated(BmuxAction::AgentCycle)
    );
}

#[test]
fn configured_interrupt_binding_stays_immediate() {
    let mut config = bcode_config::TuiConfig::default();
    config.keybindings.chat = BTreeMap::from([("ctrl+c".to_owned(), "app.interrupt".to_owned())]);
    let keymap = BmuxKeyMap::from_config(&config);
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);

    let outcome = input::handle_key(&mut app, &keymap, ctrl_key('c'));

    assert_eq!(outcome.request, KeyRequest::Interrupt);
}

#[test]
fn configured_ctrl_enter_submits_while_enter_inserts_newline() {
    let mut config = bcode_config::TuiConfig::default();
    config.keybindings.chat = BTreeMap::from([
        (
            "ctrl+enter".to_owned(),
            "tui.input.submitSteering".to_owned(),
        ),
        ("enter".to_owned(), "tui.input.newLine".to_owned()),
    ]);
    let keymap = BmuxKeyMap::from_config(&config);
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    app.replace_composer_with("draft");

    let enter = input::handle_key(&mut app, &keymap, key(KeyCode::Enter));
    assert!(enter.redraw);
    assert!(!matches!(enter.request, KeyRequest::Submit { .. }));
    assert_eq!(app.composer().text(), "draft\n");

    let ctrl_enter = input::handle_key(&mut app, &keymap, ctrl_key_code(KeyCode::Enter));
    assert_eq!(
        ctrl_enter.request,
        KeyRequest::Submit {
            placement: bcode_ipc::PromptPlacement::Steering,
        }
    );
}

#[test]
fn default_tab_requests_agent_cycle_in_chat_input() {
    let keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    app.replace_composer_with("draft");

    assert_eq!(
        keymap.action_for_key(BmuxScope::Chat, key(KeyCode::Tab)),
        Some(BmuxAction::AgentCycle)
    );

    let outcome = input::handle_key(&mut app, &keymap, key(KeyCode::Tab));

    assert!(outcome.redraw);
    assert_eq!(outcome.request, KeyRequest::CycleAgent);
    assert!(!matches!(outcome.request, KeyRequest::Submit { .. }));
    assert_eq!(app.composer().text(), "draft");
}

#[test]
fn default_shift_tab_requests_thinking_effort_cycle_in_chat_input() {
    let keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    app.replace_composer_with("draft");
    let shift_tab = KeyStroke::with_modifiers(
        KeyCode::Tab,
        Modifiers {
            shift: true,
            ..Modifiers::NONE
        },
    );

    assert_eq!(
        keymap.action_for_key(BmuxScope::Chat, shift_tab),
        Some(BmuxAction::ThinkingEffortCycle)
    );

    let outcome = input::handle_key(&mut app, &keymap, shift_tab);

    assert!(outcome.redraw);
    assert_eq!(outcome.request, KeyRequest::CycleThinkingEffort);
    assert_eq!(app.composer().text(), "draft");
}

#[test]
fn configured_agent_cycle_binding_can_be_changed() {
    let mut config = bcode_config::TuiConfig::default();
    config.keybindings.chat = BTreeMap::from([("ctrl+a".to_owned(), "tui.agent.cycle".to_owned())]);
    let keymap = BmuxKeyMap::from_config(&config);

    assert_eq!(
        keymap.action_for_key(BmuxScope::Chat, key(KeyCode::Tab)),
        None
    );
    assert_eq!(
        keymap.action_for_key(BmuxScope::Chat, ctrl_key('a')),
        Some(BmuxAction::AgentCycle)
    );
}

#[test]
fn agent_catalog_applies_configured_accent() {
    let agents =
        agent_infos_with_accents(&[("plan", false, Some("#6b7280")), ("build", true, None)]);
    let catalog = super::session_flow::AgentCatalog::from_agents(agents);
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);

    catalog.apply_agent_to_app(&mut app, "plan");

    assert_eq!(app.current_agent_id(), "plan");
    assert_eq!(app.current_agent_accent(), Some("#6b7280"));
}

#[test]
fn next_agent_preserves_list_order_and_wraps() {
    let agents = agent_infos(&[("plan", false), ("review", false), ("build", true)]);

    assert_eq!(
        super::session_flow::next_agent(&agents, "plan").map(|agent| agent.id.as_str()),
        Some("review")
    );
    assert_eq!(
        super::session_flow::next_agent(&agents, "review").map(|agent| agent.id.as_str()),
        Some("build")
    );
    assert_eq!(
        super::session_flow::next_agent(&agents, "build").map(|agent| agent.id.as_str()),
        Some("plan")
    );
}

#[test]
fn next_agent_uses_default_or_first_when_current_is_unknown() {
    let agents = agent_infos(&[("plan", false), ("build", true)]);
    let no_default = agent_infos(&[("plan", false), ("build", false)]);

    assert_eq!(
        super::session_flow::next_agent(&agents, "custom").map(|agent| agent.id.as_str()),
        Some("build")
    );
    assert_eq!(
        super::session_flow::next_agent(&no_default, "custom").map(|agent| agent.id.as_str()),
        Some("plan")
    );
    assert!(super::session_flow::next_agent(&[], "custom").is_none());
}

#[test]
fn default_ctrl_v_maps_to_clipboard_image_paste() {
    let keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());

    assert_eq!(
        keymap.action_for_key(BmuxScope::Chat, ctrl_key('v')),
        Some(BmuxAction::ClipboardPasteImage)
    );
}

#[test]
fn configured_clipboard_image_paste_binding_can_be_changed() {
    let mut config = bcode_config::TuiConfig::default();
    config.keybindings.chat =
        BTreeMap::from([("alt+v".to_owned(), "app.clipboard.pasteImage".to_owned())]);
    let keymap = BmuxKeyMap::from_config(&config);

    assert_eq!(keymap.action_for_key(BmuxScope::Chat, ctrl_key('v')), None);
    assert_eq!(
        keymap.action_for_key(BmuxScope::Chat, alt_char('v')),
        Some(BmuxAction::ClipboardPasteImage)
    );
}

#[test]
fn configured_bindings_can_keep_multiple_keys_for_same_action() {
    let mut config = bcode_config::TuiConfig::default();
    config.keybindings.chat = BTreeMap::from([
        ("enter".to_owned(), "tui.input.newLine".to_owned()),
        ("shift+enter".to_owned(), "tui.input.newLine".to_owned()),
        (
            "ctrl+enter".to_owned(),
            "tui.input.submitSteering".to_owned(),
        ),
    ]);
    let keymap = BmuxKeyMap::from_config(&config);

    assert_eq!(
        keymap.action_for_key(BmuxScope::Chat, key(KeyCode::Enter)),
        Some(BmuxAction::InputNewLine)
    );
    assert_eq!(
        keymap.action_for_key(BmuxScope::Chat, shift_key(KeyCode::Enter)),
        Some(BmuxAction::InputNewLine)
    );
    assert_eq!(
        keymap.action_for_key(BmuxScope::Chat, ctrl_key_code(KeyCode::Enter)),
        Some(BmuxAction::InputSubmitSteering)
    );
}

#[test]
fn ctrl_d_clears_input_before_exit() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    app.replace_composer_with("draft");
    let keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());

    let first = input::handle_key(&mut app, &keymap, ctrl_key('d'));
    let second = input::handle_key(&mut app, &keymap, ctrl_key('d'));

    assert!(first.redraw);
    assert!(app.composer().is_empty());
    assert!(second.redraw);
    assert!(app.should_exit());
}

#[test]
fn shift_arrows_extend_composer_selection() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    app.replace_composer_with("hello");
    let keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());

    let outcome = input::handle_key(&mut app, &keymap, shift_key(KeyCode::Left));

    assert!(outcome.redraw);
    assert_eq!(app.composer().selected_text(), Some("o".to_owned()));
}

#[test]
fn composer_mouse_drag_extends_selection() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    app.set_composer_content_area(Rect::new(2, 8, 20, 1));
    app.replace_composer_with("hello world");

    let down = app.handle_composer_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 8));
    let drag = app.handle_composer_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 7, 8));
    let up = app.handle_composer_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 7, 8));

    assert!(matches!(
        down,
        bmux_tui_components::text_input::TextInputOutcome::Redraw
    ));
    assert!(matches!(
        drag,
        bmux_tui_components::text_input::TextInputOutcome::Redraw
    ));
    assert!(matches!(
        up,
        bmux_tui_components::text_input::TextInputOutcome::Redraw
    ));
    assert_eq!(app.composer().selected_text(), Some("hello".to_owned()));
}

#[test]
fn composer_drag_beyond_visible_edge_scrolls_selection() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    app.replace_composer_with("0\n1\n2\n3\n4");
    app.composer_mut().move_cursor(TextMotion::Start);
    app.set_composer_content_area(Rect::new(2, 8, 20, 2));

    let _ = app.handle_composer_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 8));
    let outcome = app.handle_composer_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 2, 10));

    assert!(matches!(
        outcome,
        bmux_tui_components::text_input::TextInputOutcome::Redraw
    ));
    assert_eq!(app.composer_scroll_offset_for_render(), 1);
    assert_eq!(app.composer().selected_text(), Some("0\n1\n".to_owned()));
}

#[test]
fn composer_double_click_selects_word_and_triple_click_selects_all() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    app.set_composer_content_area(Rect::new(2, 8, 20, 1));
    app.replace_composer_with("hello world");

    let _ = app.handle_composer_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 3, 8));
    let _ = app.handle_composer_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 3, 8));
    assert_eq!(app.composer().selected_text(), Some("hello".to_owned()));

    let _ = app.handle_composer_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 3, 8));
    assert_eq!(
        app.composer().selected_text(),
        Some("hello world".to_owned())
    );
}

#[test]
fn input_history_updates_status_and_restores_draft() {
    let history = [
        SessionInputHistoryEntry {
            timestamp_ms: 1,
            sequence: 1,
            text: "first prompt".to_owned(),
        },
        SessionInputHistoryEntry {
            timestamp_ms: 1,
            sequence: 2,
            text: "second prompt".to_owned(),
        },
    ];
    let mut app = BmuxApp::new_with_history(None, &[], &history, false);
    app.replace_composer_with("draft prompt");

    assert!(app.previous_input_history());
    assert_eq!(app.composer().text(), "second prompt");
    assert_eq!(app.status(), "input history 2/2");

    assert!(app.previous_input_history());
    assert_eq!(app.composer().text(), "first prompt");
    assert_eq!(app.status(), "input history 1/2");

    assert!(app.next_input_history());
    assert_eq!(app.composer().text(), "second prompt");
    assert_eq!(app.status(), "input history 2/2");

    assert!(app.next_input_history());
    assert_eq!(app.composer().text(), "draft prompt");
    assert_eq!(app.status(), "draft restored");
}

#[test]
fn input_history_empty_and_not_browsing_update_status() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);

    assert!(app.previous_input_history());
    assert_eq!(app.status(), "no input history in this session");

    assert!(app.next_input_history());
    assert_eq!(app.status(), "not browsing input history");
}

#[test]
fn composer_edit_after_history_resets_navigation() {
    let history = [SessionInputHistoryEntry {
        timestamp_ms: 1,
        sequence: 1,
        text: "history prompt".to_owned(),
    }];
    let mut app = BmuxApp::new_with_history(None, &[], &history, false);
    let keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());

    assert!(app.previous_input_history());
    let outcome = input::handle_key(&mut app, &keymap, key(KeyCode::Char('!')));
    assert!(outcome.redraw);
    assert!(app.next_input_history());

    assert_eq!(app.status(), "not browsing input history");
}

#[test]
fn input_history_moves_within_multiline_entry_before_cycling() {
    let history = [
        SessionInputHistoryEntry {
            timestamp_ms: 1,
            sequence: 1,
            text: "older prompt\nolder second line".to_owned(),
        },
        SessionInputHistoryEntry {
            timestamp_ms: 1,
            sequence: 2,
            text: "newest prompt\nnewest second line".to_owned(),
        },
    ];
    let mut app = BmuxApp::new_with_history(None, &[], &history, false);
    app.set_composer_content_area(Rect::new(0, 0, 40, 3));
    let keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());

    assert!(input::handle_key(&mut app, &keymap, key(KeyCode::Up)).redraw);
    assert_eq!(app.composer().text(), "newest prompt\nnewest second line");
    assert_eq!(app.status(), "input history 2/2");

    assert!(input::handle_key(&mut app, &keymap, key(KeyCode::Up)).redraw);
    assert_eq!(app.composer().text(), "newest prompt\nnewest second line");
    assert_eq!(app.status(), "input history 2/2");

    assert!(input::handle_key(&mut app, &keymap, key(KeyCode::Up)).redraw);
    assert_eq!(app.composer().text(), "older prompt\nolder second line");
    assert_eq!(app.status(), "input history 1/2");

    assert!(input::handle_key(&mut app, &keymap, key(KeyCode::Up)).redraw);
    assert_eq!(app.composer().text(), "older prompt\nolder second line");
    assert_eq!(app.status(), "input history 1/2");

    assert!(input::handle_key(&mut app, &keymap, key(KeyCode::Down)).redraw);
    assert_eq!(app.composer().text(), "older prompt\nolder second line");
    assert_eq!(app.status(), "input history 1/2");

    assert!(input::handle_key(&mut app, &keymap, key(KeyCode::Down)).redraw);
    assert_eq!(app.composer().text(), "newest prompt\nnewest second line");
    assert_eq!(app.status(), "input history 2/2");

    assert!(input::handle_key(&mut app, &keymap, key(KeyCode::Down)).redraw);
    assert!(app.composer().is_empty());
    assert_eq!(app.status(), "draft restored");
}

#[test]
fn input_history_restores_empty_draft_from_newest_entry_bottom() {
    let history = [SessionInputHistoryEntry {
        timestamp_ms: 1,
        sequence: 1,
        text: "history prompt".to_owned(),
    }];
    let mut app = BmuxApp::new_with_history(None, &[], &history, false);
    app.set_composer_content_area(Rect::new(0, 0, 40, 3));
    let keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());

    assert!(input::handle_key(&mut app, &keymap, key(KeyCode::Up)).redraw);
    assert_eq!(app.composer().text(), "history prompt");
    assert_eq!(app.status(), "input history 1/1");

    assert!(input::handle_key(&mut app, &keymap, key(KeyCode::Down)).redraw);
    assert!(app.composer().is_empty());
    assert_eq!(app.status(), "draft restored");
}

#[test]
fn live_user_message_does_not_overwrite_saved_history_draft() {
    let session_id = SessionId::new();
    let history = [SessionInputHistoryEntry {
        timestamp_ms: 1,
        sequence: 1,
        text: "older prompt".to_owned(),
    }];
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &history, false);
    let keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());

    assert!(input::handle_key(&mut app, &keymap, key(KeyCode::Up)).redraw);
    assert_eq!(app.composer().text(), "older prompt");
    app.absorb_session_event(&event(
        session_id,
        2,
        SessionEventKind::UserMessage {
            text: "newly committed prompt".to_owned(),
            client_id: ClientId::new(),
        },
    ));
    assert!(input::handle_key(&mut app, &keymap, key(KeyCode::Down)).redraw);

    assert!(app.composer().is_empty());
    assert_eq!(app.status(), "draft restored");
}

#[test]
fn empty_and_slash_submissions_do_not_enter_input_history() {
    let history = [SessionInputHistoryEntry {
        timestamp_ms: 1,
        sequence: 1,
        text: "real prompt".to_owned(),
    }];
    let mut app = BmuxApp::new_with_history(None, &[], &history, false);

    app.stage_submission();
    let empty = app.take_pending_submission();
    app.clear_pending_submission(&empty);
    app.replace_composer_with("/help");
    app.stage_submission();
    let slash = app.take_pending_submission();
    app.clear_pending_submission(&slash);

    assert!(app.previous_input_history());
    assert_eq!(app.composer().text(), "real prompt");
    assert_eq!(app.status(), "input history 1/1");
}

#[test]
fn status_line_includes_scroll_offset_when_scrolled() {
    let session_id = SessionId::new();
    let history = (0..40)
        .map(|sequence| {
            event(
                session_id,
                sequence,
                SessionEventKind::AssistantMessage {
                    text: format!("message {sequence}"),
                },
            )
        })
        .collect::<Vec<_>>();
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 140, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert!(app.scroll_transcript_up(1));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 140, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert_eq!(app.scroll_offset(), 1);
    assert!(rendered_text(&buffer).contains("1 rows from bottom"));
}

#[test]
fn header_uses_attach_summary_title_when_recent_history_lacks_title_events() {
    let session_id = SessionId::new();
    let history = [event(
        session_id,
        42,
        SessionEventKind::AssistantMessage {
            text: "recent response".to_owned(),
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], true);
    app.apply_session_summary(&SessionSummary {
        id: session_id,
        name: Some("Canonical title".to_owned()),
        explicit_name: Some("Canonical title".to_owned()),
        derived_title: None,
        title_source: SessionTitleSource::Explicit,
        client_count: 1,
        created_at_ms: 1,
        updated_at_ms: 2,
        working_directory: "/tmp/bcode-tui-test".into(),
        import: None,
        fork: None,
    });
    let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 10));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut frame);

    assert!(buffer.row_symbols(0).unwrap().contains("Canonical title"));
    assert!(!buffer.row_symbols(0).unwrap().contains("Untitled session"));
}

#[test]
fn header_drops_low_priority_segments_in_narrow_panes() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    app.apply_session_summary(&SessionSummary {
        id: session_id,
        name: Some("A readable session title".to_owned()),
        explicit_name: Some("A readable session title".to_owned()),
        derived_title: None,
        title_source: SessionTitleSource::Explicit,
        client_count: 1,
        created_at_ms: 1,
        updated_at_ms: 2,
        working_directory: "/tmp/bcode-tui-test".into(),
        import: None,
        fork: None,
    });
    app.apply_model_status(bcode_ipc::SessionModelStatus {
        provider_plugin_id: Some("very-long-provider-plugin-id".to_owned()),
        model_id: Some("very-long-model-id".to_owned()),
        context_window: None,
        max_output_tokens: None,
        reasoning: None,
        reasoning_effort: None,
        reasoning_summary: None,
        prompt_cache_mode: None,
        conversation_reuse_mode: None,
        compaction_mode: None,
        cache: None,
        metadata_source: None,
        pricing: None,
    });
    let mut buffer = Buffer::empty(Rect::new(0, 0, 36, 8));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut frame);
    let header = buffer.row_symbols(0).unwrap();

    assert!(header.contains("bcode"));
    assert!(header.contains("build"));
    assert!(!header.contains("provider"));
    assert!(!header.contains("thinking"));
}

#[test]
fn header_shortens_session_id_on_wide_panes() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 240, 8));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut frame);
    let header = buffer.row_symbols(0).unwrap();

    assert!(header.contains(&format!("#{}", &session_id.to_string()[..8])));
    assert!(!header.contains(&session_id.to_string()[9..]));
}

#[test]
fn header_accent_color_tracks_arbitrary_selected_agent() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    disable_theme_transition(&mut app);
    app.set_agent_metadata_hydrated(true);
    app.set_current_agent_id("one");
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 8));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut frame);

    assert_eq!(
        buffer.get(Point::new(0, 0)).and_then(|cell| cell.style.fg),
        Some(bmux_tui::style::Color::Rgb(52, 211, 153))
    );
}

#[test]
fn composer_border_accent_color_tracks_arbitrary_selected_agent() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    disable_theme_transition(&mut app);
    app.set_agent_metadata_hydrated(true);
    app.set_current_agent_id("two");
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 8));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut frame);
    let border_y = app.composer_content_area().y.saturating_sub(1);

    assert_eq!(
        buffer
            .get(Point::new(0, border_y))
            .and_then(|cell| cell.style.fg),
        Some(bmux_tui::style::Color::Cyan)
    );
}

#[test]
fn same_agent_gets_same_accent_across_chrome() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    app.set_agent_metadata_hydrated(true);
    app.set_current_agent_id("custom-agent");
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 8));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut frame);
    let header_accent = buffer.get(Point::new(0, 0)).and_then(|cell| cell.style.fg);
    let border_y = app.composer_content_area().y.saturating_sub(1);
    let composer_accent = buffer
        .get(Point::new(0, border_y))
        .and_then(|cell| cell.style.fg);

    assert_eq!(header_accent, composer_accent);
}

#[test]
fn configured_agent_accent_overrides_fallback_color() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    disable_theme_transition(&mut app);
    app.set_current_agent("quiet-plan", Some("#6b7280".to_owned()));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 8));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut frame);

    assert_eq!(
        buffer.get(Point::new(0, 0)).and_then(|cell| cell.style.fg),
        Some(bmux_tui::style::Color::Rgb(107, 114, 128))
    );
}

#[test]
fn invalid_configured_agent_accent_falls_back_to_agent_color() {
    let mut fallback_app = BmuxApp::new_with_history(None, &[], &[], false);
    fallback_app.set_agent_metadata_hydrated(true);
    fallback_app.set_current_agent_id("quiet-plan");
    let mut fallback_buffer = Buffer::empty(Rect::new(0, 0, 100, 8));
    let mut fallback_frame = Frame::new(&mut fallback_buffer);
    render::render(&mut fallback_app, &mut fallback_frame);
    let fallback_accent = fallback_buffer
        .get(Point::new(0, 0))
        .and_then(|cell| cell.style.fg);

    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    app.set_agent_metadata_hydrated(true);
    app.set_current_agent("quiet-plan", Some("not-a-color".to_owned()));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 8));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert_eq!(
        buffer.get(Point::new(0, 0)).and_then(|cell| cell.style.fg),
        fallback_accent
    );
}

#[test]
fn live_session_rename_overrides_attach_summary_title() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    app.apply_session_summary(&SessionSummary {
        id: session_id,
        name: Some("Old title".to_owned()),
        explicit_name: Some("Old title".to_owned()),
        derived_title: None,
        title_source: SessionTitleSource::Explicit,
        client_count: 1,
        created_at_ms: 1,
        updated_at_ms: 2,
        working_directory: "/tmp/bcode-tui-test".into(),
        import: None,
        fork: None,
    });

    app.absorb_session_event(&event(
        session_id,
        7,
        SessionEventKind::SessionRenamed {
            name: Some("New title".to_owned()),
        },
    ));

    assert_eq!(app.session_title(), Some("New title"));
}

#[test]
fn header_and_footer_include_model_agent_and_token_context() {
    let session_id = SessionId::new();
    let history = [
        event(
            session_id,
            1,
            SessionEventKind::SessionCreated {
                name: Some("Visual parity work".to_owned()),
                working_directory: "/tmp/bcode-tui-test".into(),
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::AgentChanged {
                agent_id: "plan".to_owned(),
            },
        ),
        event(
            session_id,
            3,
            SessionEventKind::ModelUsage {
                turn_id: "turn-1".to_owned(),
                usage: SessionTokenUsage {
                    input_tokens: Some(512),
                    output_tokens: Some(128),
                    total_tokens: Some(640),
                    cached_input_tokens: Some(256),
                    cache_write_input_tokens: Some(128),
                    reasoning_tokens: Some(64),
                },
            },
        ),
    ];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    app.apply_model_status(bcode_ipc::SessionModelStatus {
        provider_plugin_id: Some("provider.example".to_owned()),
        model_id: Some("model-example".to_owned()),
        context_window: Some(1024),
        max_output_tokens: None,
        reasoning: None,
        reasoning_effort: None,
        reasoning_summary: None,
        prompt_cache_mode: None,
        conversation_reuse_mode: None,
        compaction_mode: None,
        cache: None,
        metadata_source: None,
        pricing: None,
    });
    let mut buffer = Buffer::empty(Rect::new(0, 0, 180, 12));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);

    assert!(
        buffer
            .row_symbols(0)
            .unwrap()
            .contains("Visual parity work")
    );
    assert!(buffer.row_symbols(0).unwrap().contains("provider.example"));
    assert!(output.contains("model-example"));
    assert!(output.contains("plan"));
    assert!(!output.contains("agent plan"));
    assert!(output.contains("ctx 512/1.0k 50%"));
    assert!(output.contains("read 256"));
    assert!(output.contains("write 128"));
    assert!(output.contains("spent 640"));
}

#[test]
fn status_line_prioritizes_context_over_spent_tokens() {
    let session_id = SessionId::new();
    let history = [event(
        session_id,
        1,
        SessionEventKind::ModelUsage {
            turn_id: "turn-1".to_owned(),
            usage: SessionTokenUsage {
                input_tokens: Some(512),
                output_tokens: Some(128),
                total_tokens: Some(640),
                cached_input_tokens: None,
                cache_write_input_tokens: None,
                reasoning_tokens: None,
            },
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    app.apply_model_status(bcode_ipc::SessionModelStatus {
        provider_plugin_id: Some("provider.example".to_owned()),
        model_id: Some("model-example".to_owned()),
        context_window: Some(128_000),
        max_output_tokens: None,
        reasoning: None,
        reasoning_effort: None,
        reasoning_summary: None,
        prompt_cache_mode: None,
        conversation_reuse_mode: None,
        compaction_mode: None,
        cache: None,
        metadata_source: None,
        pricing: None,
    });
    let mut buffer = Buffer::empty(Rect::new(0, 0, 68, 8));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);

    assert!(output.contains("ctx 512/128.0k 0%"), "{output}");
    assert!(!output.contains("spent 640"), "{output}");
}

#[test]
fn status_line_includes_unknown_context_before_spent_tokens() {
    let session_id = SessionId::new();
    let history = [event(
        session_id,
        1,
        SessionEventKind::ModelUsage {
            turn_id: "turn-1".to_owned(),
            usage: SessionTokenUsage {
                input_tokens: Some(75_700),
                output_tokens: Some(1_524_300),
                total_tokens: Some(1_600_000),
                cached_input_tokens: Some(75_700),
                cache_write_input_tokens: None,
                reasoning_tokens: None,
            },
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    app.set_status("model: bcode.openai-compatible/gpt-5.5; active skills: 0".to_owned());
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 8));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);

    assert!(output.contains("ctx unknown"), "{output}");
    assert!(!output.contains("spent 1.6m"), "{output}");
}

#[test]
fn status_line_drops_low_priority_segments_in_narrow_panes() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    app.set_daemon_connection(super::app::DaemonConnectionState::Connected);
    app.set_status("important status message".to_owned());
    let mut buffer = Buffer::empty(Rect::new(0, 0, 36, 8));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);

    assert!(output.contains("ready"));
    assert!(output.contains("important status message"));
    assert!(!output.contains("enter send"));
    assert!(!output.contains("spent"));
}

#[test]
fn draft_agent_selection_updates_header() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);

    app.set_current_agent_id("plan");

    let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 10));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert!(buffer.row_symbols(0).unwrap().contains("plan"));
    assert!(!buffer.row_symbols(0).unwrap().contains("agent plan"));
}

#[test]
fn new_draft_preserves_selected_agent() {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut chat = super::session_flow::ActiveChat {
        app: BmuxApp::new_with_history(None, &[], &[], false),
        agents: super::session_flow::AgentCatalog::default(),
        session_id: None,
        event_sender: sender,
        event_receiver: receiver,
        event_task: None,
        opening_session_id: None,
        pending_effects: super::effects::TuiEffectQueue::default(),
    };
    chat.app.set_current_agent_id("plan");

    super::session_flow::switch_to_draft_session(&mut chat);

    assert_eq!(chat.app.current_agent_id(), "plan");
}

#[tokio::test]
async fn async_session_open_preserves_typed_draft() {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let session_id = SessionId::new();
    let mut chat = super::session_flow::ActiveChat {
        app: BmuxApp::new_with_history(Some(session_id), &[], &[], false),
        agents: super::session_flow::AgentCatalog::default(),
        session_id: None,
        event_sender: sender,
        event_receiver: receiver,
        event_task: None,
        opening_session_id: Some(session_id),
        pending_effects: super::effects::TuiEffectQueue::default(),
    };
    chat.app.replace_composer_with("draft while opening");
    let (_event_sender, event_receiver) = tokio::sync::broadcast::channel::<SessionEvent>(1);
    let attached = AttachedSessionHistory {
        session: session_summary(session_id),
        history: vec![event(
            session_id,
            1,
            SessionEventKind::AssistantMessage {
                text: "previous answer".to_owned(),
            },
        )],
        input_history: vec![SessionInputHistoryEntry {
            timestamp_ms: 1,
            sequence: 1,
            text: "previous prompt".to_owned(),
        }],
        import_warnings: Vec::new(),
        draft: None,
        runtime_selection: bcode_ipc::SessionRuntimeSelection::default(),
    };

    super::session_flow::complete_switch_session(
        &mut chat,
        session_id,
        true,
        Ok((
            attached,
            tokio::spawn(async move {
                drop(event_receiver);
            }),
        )),
    );

    assert_eq!(chat.app.composer().text(), "draft while opening");
}

#[tokio::test]
async fn async_session_open_initial_state_preserves_existing_draft() {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let session_id = SessionId::new();
    let mut chat = super::session_flow::ActiveChat {
        app: BmuxApp::new_with_history(None, &[], &[], false),
        agents: super::session_flow::AgentCatalog::default(),
        session_id: None,
        event_sender: sender,
        event_receiver: receiver,
        event_task: None,
        opening_session_id: None,
        pending_effects: super::effects::TuiEffectQueue::default(),
    };
    chat.app.replace_composer_with("draft before opening");

    super::session_flow::start_switch_session(
        &mut chat,
        session_id,
        super::session_flow::initial_transcript_window_request(Rect::new(0, 0, 80, 24)),
    );

    assert_eq!(chat.app.composer().text(), "draft before opening");
}

#[tokio::test]
async fn async_session_open_initial_state_preserves_plugin_host() {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let session_id = SessionId::new();
    let mut chat = super::session_flow::ActiveChat {
        app: BmuxApp::new_with_history(None, &[], &[], false),
        agents: super::session_flow::AgentCatalog::default(),
        session_id: None,
        event_sender: sender,
        event_receiver: receiver,
        event_task: None,
        opening_session_id: None,
        pending_effects: super::effects::TuiEffectQueue::default(),
    };
    chat.app.set_plugin_host(Arc::new(filesystem_plugin_host()));

    super::session_flow::start_switch_session(
        &mut chat,
        session_id,
        super::session_flow::initial_transcript_window_request(Rect::new(0, 0, 80, 24)),
    );

    assert!(chat.app.plugin_host().is_some());
}

#[tokio::test]
async fn async_session_open_completion_preserves_plugin_host() {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let session_id = SessionId::new();
    let mut chat = super::session_flow::ActiveChat {
        app: BmuxApp::new_with_history(Some(session_id), &[], &[], false),
        agents: super::session_flow::AgentCatalog::default(),
        session_id: None,
        event_sender: sender,
        event_receiver: receiver,
        event_task: None,
        opening_session_id: Some(session_id),
        pending_effects: super::effects::TuiEffectQueue::default(),
    };
    chat.app.set_plugin_host(Arc::new(filesystem_plugin_host()));
    let (_event_sender, event_receiver) = tokio::sync::broadcast::channel::<SessionEvent>(1);
    let attached = AttachedSessionHistory {
        session: session_summary(session_id),
        history: Vec::new(),
        input_history: Vec::new(),
        import_warnings: Vec::new(),
        draft: None,
        runtime_selection: bcode_ipc::SessionRuntimeSelection::default(),
    };

    super::session_flow::complete_switch_session(
        &mut chat,
        session_id,
        false,
        Ok((
            attached,
            tokio::spawn(async move {
                drop(event_receiver);
            }),
        )),
    );

    assert!(chat.app.plugin_host().is_some());
}

#[test]
fn switch_to_draft_session_preserves_plugin_host() {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let session_id = SessionId::new();
    let mut chat = super::session_flow::ActiveChat {
        app: BmuxApp::new_with_history(Some(session_id), &[], &[], false),
        agents: super::session_flow::AgentCatalog::default(),
        session_id: Some(session_id),
        event_sender: sender,
        event_receiver: receiver,
        event_task: None,
        opening_session_id: None,
        pending_effects: super::effects::TuiEffectQueue::default(),
    };
    chat.app.set_plugin_host(Arc::new(filesystem_plugin_host()));

    super::session_flow::switch_to_draft_session(&mut chat);

    assert!(chat.app.plugin_host().is_some());
}

#[tokio::test]
async fn session_open_preserved_plugin_host_renders_live_file_preview() {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let session_id = SessionId::new();
    let mut chat = super::session_flow::ActiveChat {
        app: BmuxApp::new_with_history(None, &[], &[], false),
        agents: super::session_flow::AgentCatalog::default(),
        session_id: None,
        event_sender: sender,
        event_receiver: receiver,
        event_task: None,
        opening_session_id: None,
        pending_effects: super::effects::TuiEffectQueue::default(),
    };
    chat.app.set_plugin_host(Arc::new(filesystem_plugin_host()));

    super::session_flow::start_switch_session(
        &mut chat,
        session_id,
        super::session_flow::initial_transcript_window_request(Rect::new(0, 0, 80, 24)),
    );
    chat.app
        .absorb_session_live_event(&bcode_session_models::SessionLiveEvent {
            session_id,
            kind: bcode_session_models::SessionLiveEventKind::ToolArgumentPreview {
                turn_id: "turn-1".to_owned(),
                tool_call_id: "call_write".to_owned(),
                tool_name: "filesystem_write".to_owned(),
                argument_bytes: 36,
                preview: LiveToolArgumentPreview::FileEdit(LiveFileEditPreview {
                    preview_title: Some("Write preview".to_owned()),
                    streaming_status: Some("writing src/lib.rs".to_owned()),
                    path: Some("src/lib.rs".to_owned()),
                    old_text_prefix: None,
                    new_text_prefix: "pub fn demo() {}".to_owned(),
                    old_text_required: false,
                    argument_bytes: 36,
                    truncated: false,
                }),
            },
        });

    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 30));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut chat.app, &mut frame);
    let output = rendered_text(&buffer);
    assert!(output.contains("Write preview"), "{output}");
    assert!(!output.contains("plugin host unavailable"), "{output}");
}

#[test]
fn slash_pending_submission_clears_after_take() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    app.replace_composer_with("/plan");
    app.stage_submission();
    let message = app.take_pending_submission();

    app.clear_pending_submission(&message);

    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 10));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);

    assert!(!output.contains("/plan"));
    assert!(!output.contains("[sending]"));
}

#[test]
fn taken_pending_submission_can_be_restored_after_send_failure() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    app.replace_composer_with("hello");
    app.stage_submission();
    let message = app.take_pending_submission();

    app.restore_pending_submission(&message);

    assert_eq!(app.composer().text(), "hello");
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 10));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);

    assert!(!output.contains("[sending]"));
}

#[test]
fn slash_palette_renders_above_composer() {
    let palette = slash_palette::SlashPalette::from_items(vec![
        ("/plan", "Switch to plan agent"),
        ("/build", "Switch to build agent"),
    ]);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 20));
    let mut frame = Frame::new(&mut buffer);

    slash_palette_render::render_palette(
        &palette,
        Rect::new(2, 18, 76, 1),
        &mut frame,
        render::TuiTheme::for_agent("build", None, true),
    );
    let output = rendered_text(&buffer);

    assert!(output.contains("Slash Commands"));
    assert!(output.contains("/plan"));
    assert!(buffer.row_symbols(0).unwrap().trim().is_empty());
    assert!(buffer.row_symbols(13).unwrap().contains("Slash Commands"));
}

#[test]
fn assistant_final_replaces_stream_when_usage_is_interleaved() {
    let session_id = SessionId::new();
    let history = [
        event(
            session_id,
            1,
            SessionEventKind::AssistantDelta {
                text: "Fixed".to_owned(),
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::ModelUsage {
                turn_id: "turn-1".to_owned(),
                usage: SessionTokenUsage::default(),
            },
        ),
        event(
            session_id,
            3,
            SessionEventKind::AssistantMessage {
                text: "Fixed.".to_owned(),
            },
        ),
    ];
    let app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);

    let assistant_items = app
        .transcript()
        .iter()
        .filter(|item| item.role() == "Assistant")
        .collect::<Vec<_>>();

    assert_eq!(assistant_items.len(), 1);
    assert_eq!(assistant_items[0].text(), "Fixed.");
    assert!(!assistant_items[0].streaming());
    assert!(app.transcript().iter().any(|item| item.role() == "Usage"));
}

#[test]
fn history_rebuild_does_not_duplicate_initial_history() {
    let session_id = SessionId::new();
    let history = [
        event(
            session_id,
            1,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "first".to_owned(),
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::AssistantMessage {
                text: "second".to_owned(),
            },
        ),
    ];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);

    app.apply_thinking_config(TuiThinkingConfig::default());

    let user_items = app
        .transcript()
        .iter()
        .filter(|item| item.role() == "You")
        .collect::<Vec<_>>();
    let assistant_items = app
        .transcript()
        .iter()
        .filter(|item| item.role() == "Assistant")
        .collect::<Vec<_>>();

    assert_eq!(user_items.len(), 1);
    assert_eq!(user_items[0].text(), "first");
    assert_eq!(assistant_items.len(), 1);
    assert_eq!(assistant_items[0].text(), "second");
}

#[test]
fn initial_transcript_window_request_uses_viewport_targets() {
    let request = super::history_flow::initial_transcript_window_request(Rect::new(0, 0, 100, 20));

    assert_eq!(request.projection, SessionProjectionKind::Transcript);
    assert_eq!(request.target.width_columns, Some(100));
    assert_eq!(request.target.min_items, Some(12));
    assert_eq!(request.target.min_estimated_rows, Some(40));
    assert_eq!(request.limits.max_events_scanned, 2_048);
}

#[test]
fn live_event_overlapping_initial_history_is_ignored() {
    let session_id = SessionId::new();
    let history = [
        event(
            session_id,
            1,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "question".to_owned(),
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::AssistantMessage {
                text: "answer".to_owned(),
            },
        ),
    ];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);

    app.absorb_session_event(&history[1]);

    let assistant_items = app
        .transcript()
        .iter()
        .filter(|item| item.role() == "Assistant")
        .collect::<Vec<_>>();
    assert_eq!(assistant_items.len(), 1);
    assert_eq!(assistant_items[0].text(), "answer");
}

#[test]
fn newer_live_event_after_initial_history_is_absorbed() {
    let session_id = SessionId::new();
    let history = [event(
        session_id,
        1,
        SessionEventKind::UserMessage {
            client_id: ClientId::new(),
            text: "question".to_owned(),
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);

    app.absorb_session_event(&event(
        session_id,
        2,
        SessionEventKind::AssistantMessage {
            text: "answer".to_owned(),
        },
    ));

    assert!(
        app.transcript()
            .iter()
            .any(|item| item.role() == "Assistant" && item.text() == "answer")
    );
}

#[test]
fn prepended_history_coalesces_assistant_deltas() {
    let session_id = SessionId::new();
    let newer = [event(
        session_id,
        10,
        SessionEventKind::UserMessage {
            client_id: ClientId::new(),
            text: "newer prompt".to_owned(),
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &newer, &[], true);
    let older = [
        event(
            session_id,
            1,
            SessionEventKind::AssistantDelta {
                text: "hello ".to_owned(),
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::AssistantDelta {
                text: "world".to_owned(),
            },
        ),
        event(
            session_id,
            3,
            SessionEventKind::AssistantMessage {
                text: "hello world".to_owned(),
            },
        ),
    ];

    app.prepend_older_history(&older, false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 14));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);

    assert!(output.contains("Bcode"));
    assert!(output.contains("  hello world"));
    assert!(!output.contains("Bcode …"));
    assert_eq!(
        app.transcript()
            .iter()
            .filter(|item| item.role() == "Assistant")
            .count(),
        1
    );
}

#[test]
fn transcript_renders_tool_blocks_with_structure_and_pretty_arguments() {
    let session_id = SessionId::new();
    let full_call_id = "call_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let history = [
        event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: full_call_id.to_owned(),
                producer_plugin_id: None,
                tool_name: "shell.run".to_owned(),
                arguments_json: r#"{"command":"cargo check","cwd":"/tmp/project"}"#.to_owned(),
                legacy_request_presentation: Some(shell_legacy_request_presentation()),
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::ToolCallFinished {
                tool_call_id: full_call_id.to_owned(),
                result: "ok".to_owned(),
                is_error: false,
                output: None,
                semantic_result: None,
            },
        ),
    ];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 32));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);

    assert!(output.contains("Tool · shell.run"));
    assert!(output.contains("call call_ABCD"));
    assert!(output.contains(full_call_id));
    assert!(output.contains("\"command\": \"cargo check\""), "{output}");
    assert!(output.contains("\"cwd\": \"/tmp/project\""), "{output}");
    assert!(output.contains("Tool result · shell.run · ok"));
    assert!(output.contains("    ok"));
}

#[test]
fn live_file_write_statusline_is_not_duplicated_and_truncates_path() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "call_write".to_owned(),
            producer_plugin_id: None,
            tool_name: "filesystem_write".to_owned(),
            arguments_json: serde_json::json!({
                "path": "/Users/braden/projects/bcode/packages/tui/src/render.rs",
                "contents": "fn main() {}\n",
            })
            .to_string(),
            legacy_request_presentation: Some(file_edit_legacy_request_presentation()),
        },
    ));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 72, 16));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);

    assert!(output.contains("tool filesystem_write"));
    assert!(output.contains("\"path\":"), "{output}");
    assert!(
        output.contains("/Users/braden/projects/bcode/packages/tui/src/render.rs"),
        "{output}"
    );
    assert!(!output.contains("File change preview"));
    assert!(!output.contains("tool filesystem_write · running tool filesystem_write"));
}

#[test]
fn live_file_edit_card_shows_permission_and_applied_phases() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    let args = serde_json::json!({
        "path": "src/lib.rs",
        "old_text": "old\n",
        "new_text": "new\n",
    })
    .to_string();
    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "call_edit".to_owned(),
            producer_plugin_id: None,
            tool_name: "filesystem_edit".to_owned(),
            arguments_json: args.clone(),
            legacy_request_presentation: Some(file_edit_legacy_request_presentation()),
        },
    ));
    app.absorb_session_event(&event(
        session_id,
        2,
        SessionEventKind::PermissionRequested {
            permission_id: "perm_edit".to_owned(),
            tool_call_id: "call_edit".to_owned(),
            producer_plugin_id: None,
            tool_name: "filesystem_edit".to_owned(),
            arguments_json: args,
            legacy_request_presentation: Some(file_edit_legacy_request_presentation()),
            policy_source: None,
            policy_reason: None,
        },
    ));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 40));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);
    assert!(output.contains("Permission required"), "{output}");
    assert!(!output.contains("Editing file"), "{output}");

    app.absorb_session_event(&event(
        session_id,
        3,
        SessionEventKind::ToolInvocationStream {
            event: ToolInvocationStreamEvent::Started {
                tool_call_id: "call_edit".to_owned(),
                tool_name: "filesystem_edit".to_owned(),
                sequence: 0,
                terminal: false,
                columns: None,
                rows: None,
                started_at_ms: None,
            },
        },
    ));
    app.absorb_session_event(&event(
        session_id,
        4,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call_edit".to_owned(),
            result: "edited src/lib.rs".to_owned(),
            is_error: false,
            output: None,
            semantic_result: None,
        },
    ));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 40));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);
    assert!(output.contains("finished"), "{output}");
    assert!(!output.contains("confirmation: edited src/lib.rs"));
}

#[test]
fn denied_file_permission_marks_preview_failed() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    let args = serde_json::json!({
        "path": "src/lib.rs",
        "old_text": "old\n",
        "new_text": "new\n",
    })
    .to_string();
    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "call_edit".to_owned(),
            producer_plugin_id: None,
            tool_name: "filesystem_edit".to_owned(),
            arguments_json: args.clone(),
            legacy_request_presentation: Some(file_edit_legacy_request_presentation()),
        },
    ));
    app.absorb_session_event(&event(
        session_id,
        2,
        SessionEventKind::PermissionRequested {
            permission_id: "perm_edit".to_owned(),
            tool_call_id: "call_edit".to_owned(),
            producer_plugin_id: None,
            tool_name: "filesystem_edit".to_owned(),
            arguments_json: args,
            legacy_request_presentation: Some(file_edit_legacy_request_presentation()),
            policy_source: None,
            policy_reason: None,
        },
    ));
    app.absorb_session_event(&event(
        session_id,
        3,
        SessionEventKind::PermissionResolved {
            permission_id: "perm_edit".to_owned(),
            approved: false,
        },
    ));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 40));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);

    assert!(!output.contains("File change preview · filesystem_edit"));
}

#[test]
fn transcript_renders_filesystem_edit_request_without_core_inline_preview() {
    let session_id = SessionId::new();
    let history = [event(
        session_id,
        1,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "call_edit".to_owned(),
            producer_plugin_id: None,
            tool_name: "example.edit".to_owned(),
            arguments_json: serde_json::json!({
                "path": "src/lib.rs",
                "old_text": "fn answer() -> i32 {\n    41\n}\n",
                "new_text": "fn answer() -> i32 {\n    42\n}\n",
            })
            .to_string(),
            legacy_request_presentation: Some(file_edit_legacy_request_presentation()),
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 18));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);

    assert!(output.contains("example.edit"), "{output}");
    assert!(output.contains("\"path\": \"src/lib.rs\""), "{output}");
    assert!(output.contains("\"old_text\":"), "{output}");
    assert!(output.contains("\"new_text\":"), "{output}");
    assert!(!output.contains("File change preview"), "{output}");
    assert!(!output.contains("replaced 1 line with 1 line"), "{output}");
}

#[test]
fn transcript_renders_shell_output_with_ansi_and_limits() {
    let session_id = SessionId::new();
    let stdout = (0..40)
        .map(|index| format!("\u{1b}[32mline {index}\u{1b}[0m"))
        .collect::<Vec<_>>()
        .join("\n");
    let history = [
        event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call_shell".to_owned(),
                producer_plugin_id: None,
                tool_name: "shell.run".to_owned(),
                arguments_json: serde_json::json!({
                    "command": "cargo test",
                    "cwd": "/tmp/project",
                    "terminal": true,
                })
                .to_string(),
                legacy_request_presentation: Some(file_edit_legacy_request_presentation()),
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::Started {
                    tool_call_id: "call_shell".to_owned(),
                    tool_name: "shell.run".to_owned(),
                    sequence: 0,
                    terminal: true,
                    columns: Some(80),
                    rows: Some(10),
                    started_at_ms: None,
                },
            },
        ),
        event(
            session_id,
            3,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::OutputDelta {
                    tool_call_id: "call_shell".to_owned(),
                    stream: ToolOutputStream::Pty,
                    sequence: 1,
                    text: stdout,
                    byte_len: 0,
                },
            },
        ),
        event(
            session_id,
            4,
            SessionEventKind::ToolCallFinished {
                tool_call_id: "call_shell".to_owned(),
                result: String::new(),
                is_error: false,
                output: None,
                semantic_result: None,
            },
        ),
    ];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 40));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);

    assert!(output.contains("Terminal · shell.run"));
    assert!(output.contains("completed"));
    assert!(!output.contains("line 0"));
    assert!(output.contains("line 39"));
    assert!(!output.contains('\u{1b}'));
}

#[test]
fn transcript_renders_terminal_shell_output_without_unbounded_row_request() {
    let session_id = SessionId::new();
    let output = (0..40)
        .map(|index| format!("\u{1b}[32mline {index}\u{1b}[0m"))
        .collect::<Vec<_>>()
        .join("\r\n");
    let history = [
        event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call_terminal".to_owned(),
                producer_plugin_id: None,
                tool_name: "shell.run".to_owned(),
                arguments_json: serde_json::json!({
                    "command": "git status --short && ls",
                })
                .to_string(),
                legacy_request_presentation: Some(shell_legacy_request_presentation()),
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::Started {
                    tool_call_id: "call_terminal".to_owned(),
                    tool_name: "shell.run".to_owned(),
                    sequence: 0,
                    terminal: true,
                    columns: Some(80),
                    rows: Some(10),
                    started_at_ms: None,
                },
            },
        ),
        event(
            session_id,
            3,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::OutputDelta {
                    tool_call_id: "call_terminal".to_owned(),
                    stream: ToolOutputStream::Pty,
                    sequence: 1,
                    text: output,
                    byte_len: 0,
                },
            },
        ),
        event(
            session_id,
            4,
            SessionEventKind::ToolCallFinished {
                tool_call_id: "call_terminal".to_owned(),
                result: String::new(),
                is_error: false,
                output: None,
                semantic_result: None,
            },
        ),
    ];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 40));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);

    assert!(output.contains("Terminal · shell.run"));
    assert!(output.contains("completed"));
    assert!(!output.contains("terminal: 80x10"));
    assert!(!output.contains("line 0"));
    assert!(output.contains("line 12"));
    assert!(output.contains("line 39"));
    assert!(!output.contains('\u{1b}'));
}

#[test]
fn transcript_renders_terminal_shell_output_without_viewport_padding() {
    let session_id = SessionId::new();
    let history = [event(
        session_id,
        1,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call_terminal".to_owned(),
            result: String::new(),
            is_error: false,
            output: None,
            semantic_result: Some(shell_result_artifact(&ShellRunResult::Terminal {
                exit_code: Some(0),
                timed_out: false,
                cancelled: false,
                duration_ms: None,
                output_tail: "one\r\ntwo".to_owned(),
                output_truncated: false,
                output_bytes: None,
                retained_output_bytes: None,
                columns: 80,
                rows: 10,
            })),
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 18));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);

    assert!(output.contains("Shell run"));
    assert!(output.contains("plugin host unavailable"));
    assert!(!output.contains("test.shell-artifact"));
    assert!(!output.contains("artifact_id"));
    assert!(!output.contains("producer_plugin_id"));
    assert!(!output.contains("metadata"));
}

#[test]
fn transcript_renders_truncated_terminal_shell_output_as_terminal() {
    let session_id = SessionId::new();
    let history = [event(
        session_id,
        1,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call_terminal".to_owned(),
            result: String::new(),
            is_error: false,
            output: None,
            semantic_result: Some(shell_result_artifact(&ShellRunResult::Terminal {
                exit_code: Some(0),
                timed_out: false,
                cancelled: false,
                duration_ms: None,
                output_tail: "one\r\ntwo".to_owned(),
                output_truncated: true,
                output_bytes: Some(70000),
                retained_output_bytes: Some(65536),
                columns: 80,
                rows: 10,
            })),
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 20));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);

    assert!(output.contains("Shell run"));
    assert!(output.contains("plugin host unavailable"));
    assert!(!output.contains("test.shell-artifact"));
    assert!(!output.contains("artifact_id"));
    assert!(!output.contains("producer_plugin_id"));
    assert!(!output.contains("metadata"));
}

#[test]
fn streamed_terminal_output_renders_running_until_final_result() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);

    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "call-running".to_owned(),
            producer_plugin_id: None,
            tool_name: "shell.run".to_owned(),
            arguments_json: "{}".to_owned(),
            legacy_request_presentation: Some(shell_legacy_request_presentation()),
        },
    ));
    app.absorb_session_event(&event(
        session_id,
        2,
        SessionEventKind::ToolInvocationStream {
            event: ToolInvocationStreamEvent::Started {
                tool_call_id: "call-running".to_owned(),
                tool_name: "shell.run".to_owned(),
                sequence: 0,
                terminal: true,
                columns: Some(80),
                rows: Some(24),
                started_at_ms: Some(1_000),
            },
        },
    ));
    app.absorb_session_event(&event(
        session_id,
        3,
        SessionEventKind::ToolInvocationStream {
            event: ToolInvocationStreamEvent::OutputDelta {
                tool_call_id: "call-running".to_owned(),
                stream: ToolOutputStream::Pty,
                sequence: 1,
                text: "still running\n".to_owned(),
                byte_len: "still running\n".len(),
            },
        },
    ));

    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 20));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);

    assert!(output.contains("Terminal · shell.run · running"));
    assert!(output.contains("running"));
    assert!(!output.contains(" · terminal"));
    assert!(!output.contains("exit 0"));
}

#[test]
fn streamed_terminal_output_preserves_ansi_color() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);

    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "call-color".to_owned(),
            producer_plugin_id: None,
            tool_name: "shell.run".to_owned(),
            arguments_json: "{}".to_owned(),
            legacy_request_presentation: Some(shell_legacy_request_presentation()),
        },
    ));
    app.absorb_session_event(&event(
        session_id,
        2,
        SessionEventKind::ToolInvocationStream {
            event: ToolInvocationStreamEvent::Started {
                tool_call_id: "call-color".to_owned(),
                tool_name: "shell.run".to_owned(),
                sequence: 0,
                terminal: true,
                columns: Some(80),
                rows: Some(24),
                started_at_ms: None,
            },
        },
    ));
    app.absorb_session_event(&event(
        session_id,
        3,
        SessionEventKind::ToolInvocationStream {
            event: ToolInvocationStreamEvent::OutputDelta {
                tool_call_id: "call-color".to_owned(),
                stream: ToolOutputStream::Pty,
                sequence: 1,
                text: "\u{1b}[32mgreen\u{1b}[0m\n".to_owned(),
                byte_len: "\u{1b}[32mgreen\u{1b}[0m\n".len(),
            },
        },
    ));

    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 20));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert_eq!(
        buffer
            .get(Point::new(4, output_line_y(&buffer, "green").unwrap()))
            .map(|cell| cell.style.fg),
        Some(Some(bmux_tui::style::Color::Green))
    );
}

#[test]
fn streamed_terminal_output_updates_header_after_final_result() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);

    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "call-final".to_owned(),
            producer_plugin_id: None,
            tool_name: "shell.run".to_owned(),
            arguments_json: "{}".to_owned(),
            legacy_request_presentation: Some(shell_legacy_request_presentation()),
        },
    ));
    app.absorb_session_event(&event(
        session_id,
        2,
        SessionEventKind::ToolInvocationStream {
            event: ToolInvocationStreamEvent::Started {
                tool_call_id: "call-final".to_owned(),
                tool_name: "shell.run".to_owned(),
                sequence: 0,
                terminal: true,
                columns: Some(80),
                rows: Some(24),
                started_at_ms: None,
            },
        },
    ));
    app.absorb_session_event(&event(
        session_id,
        3,
        SessionEventKind::ToolInvocationStream {
            event: ToolInvocationStreamEvent::OutputDelta {
                tool_call_id: "call-final".to_owned(),
                stream: ToolOutputStream::Pty,
                sequence: 1,
                text: "done\n".to_owned(),
                byte_len: "done\n".len(),
            },
        },
    ));
    app.absorb_session_event(&event(
        session_id,
        4,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call-final".to_owned(),
            result: "done\n".to_string(),
            is_error: true,
            output: None,
            semantic_result: Some(shell_result_artifact(&ShellRunResult::Terminal {
                exit_code: Some(2),
                timed_out: false,
                cancelled: false,
                duration_ms: None,
                output_tail: "done\n".to_owned(),
                output_truncated: false,
                output_bytes: Some(5),
                retained_output_bytes: Some(5),
                columns: 80,
                rows: 24,
            })),
        },
    ));

    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 20));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);

    assert!(output.contains("Terminal · shell.run · signal"));
    assert!(output.contains("signal"));
    assert!(!output.contains("Terminal · shell.run · running"));
}

#[test]
fn scroll_up_requests_older_history_only_after_top() {
    let session_id = SessionId::new();
    let history = (10..60)
        .map(|sequence| {
            event(
                session_id,
                sequence,
                SessionEventKind::UserMessage {
                    client_id: ClientId::new(),
                    text: format!("prompt {sequence}"),
                },
            )
        })
        .collect::<Vec<_>>();
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], true);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 20));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert!(app.scroll_transcript_up(1));
    assert!(!app.should_load_older_history());

    assert!(app.scroll_transcript_up(usize::MAX / 2));
    assert!(app.should_load_older_history());
}

#[test]
fn latest_bar_ignores_hidden_continuation_of_visible_message() {
    let session_id = SessionId::new();
    let long_message = (0..30)
        .map(|line| format!("line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let history = [event(
        session_id,
        0,
        SessionEventKind::AssistantMessage { text: long_message },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert!(app.scroll_transcript_up(usize::MAX / 2));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert!(!app.newer_transcript_content_below());
    assert!(!rendered_text(&buffer).contains("New messages below"));
}

#[test]
fn latest_bar_shows_for_distinct_hidden_entry_below_visible_message() {
    let session_id = SessionId::new();
    let history = (0..20)
        .map(|sequence| {
            event(
                session_id,
                sequence,
                SessionEventKind::AssistantMessage {
                    text: format!("message {sequence}"),
                },
            )
        })
        .collect::<Vec<_>>();
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert!(app.scroll_transcript_up(4));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert!(app.newer_transcript_content_below());
    assert!(rendered_text(&buffer).contains("New messages below"));
}

#[test]
fn scroll_down_at_bottom_enters_virtual_space() {
    let session_id = SessionId::new();
    let history = (0..20)
        .map(|sequence| {
            event(
                session_id,
                sequence,
                SessionEventKind::AssistantMessage {
                    text: format!("message {sequence}"),
                },
            )
        })
        .collect::<Vec<_>>();
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert_eq!(app.scroll_offset(), 0);
    assert!(app.scroll_transcript_down(4));
    assert_eq!(app.bottom_overscroll(), 4);

    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert!(rendered_text(&buffer).contains("4 rows below latest"));
    assert!(output_line_y(&buffer, "message 19").is_some_and(|y| y < 9));
}

#[test]
fn appended_rows_consume_virtual_space_until_following_catches_up() {
    let session_id = SessionId::new();
    let history = [event(
        session_id,
        0,
        SessionEventKind::AssistantMessage {
            text: "message".to_owned(),
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert!(app.scroll_transcript_down(4));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    assert_eq!(app.bottom_overscroll(), 4);
    app.expire_manual_transcript_scroll_for_test();

    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::AssistantMessage {
            text: "new one".to_owned(),
        },
    ));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    assert!(app.bottom_overscroll() < 4);

    for sequence in 2..8 {
        app.absorb_session_event(&event(
            session_id,
            sequence,
            SessionEventKind::AssistantMessage {
                text: format!("new {sequence}"),
            },
        ));
    }
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert_eq!(app.bottom_overscroll(), 0);
    assert_eq!(app.scroll_offset(), 0);
}

#[test]
fn streaming_delta_fills_virtual_space_instead_of_top_anchoring() {
    let session_id = SessionId::new();
    let history = [event(
        session_id,
        0,
        SessionEventKind::UserMessage {
            client_id: ClientId::new(),
            text: "prompt".to_owned(),
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert!(app.scroll_transcript_down(4));
    app.expire_manual_transcript_scroll_for_test();
    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::AssistantDelta {
            text: "first line".to_owned(),
        },
    ));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert!(app.bottom_overscroll() < 4);
    assert_ne!(output_line_y(&buffer, "Bcode …"), Some(1));
}

#[test]
fn manual_scroll_grace_prevents_virtual_space_catch_up() {
    let session_id = SessionId::new();
    let history = [event(
        session_id,
        0,
        SessionEventKind::AssistantMessage {
            text: "message".to_owned(),
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert!(app.scroll_transcript_down(4));
    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::AssistantMessage {
            text: "new one".to_owned(),
        },
    ));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert_eq!(app.bottom_overscroll(), 4);
}

#[test]
fn manual_scroll_grace_prevents_stream_top_anchor() {
    let session_id = SessionId::new();
    let history = [event(
        session_id,
        0,
        SessionEventKind::UserMessage {
            client_id: ClientId::new(),
            text: "prompt".to_owned(),
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert!(app.scroll_transcript_down(4));
    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::AssistantDelta {
            text: "first line".to_owned(),
        },
    ));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert_eq!(app.bottom_overscroll(), 4);
}

#[test]
fn submitted_user_message_anchors_at_top() {
    let session_id = SessionId::new();
    let history = (0..12)
        .map(|sequence| {
            event(
                session_id,
                sequence,
                SessionEventKind::AssistantMessage {
                    text: format!("message {sequence}"),
                },
            )
        })
        .collect::<Vec<_>>();
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    app.replace_composer_with("new prompt");
    app.stage_submission();
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    std::thread::sleep(Duration::from_millis(220));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert_eq!(output_line_y(&buffer, "You · sending"), Some(1));
}

#[test]
fn accepted_submission_preserves_submitted_user_message_transition() {
    let session_id = SessionId::new();
    let history = (0..12)
        .map(|sequence| {
            event(
                session_id,
                sequence,
                SessionEventKind::AssistantMessage {
                    text: format!("message {sequence}"),
                },
            )
        })
        .collect::<Vec<_>>();
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    app.replace_composer_with("new prompt");
    app.stage_submission();
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    std::thread::sleep(Duration::from_millis(220));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    assert_eq!(output_line_y(&buffer, "You · sending"), Some(1));

    app.mark_pending_submission_sent();
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    std::thread::sleep(Duration::from_millis(220));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert_ne!(output_line_y(&buffer, "message 11"), Some(1));
}

#[test]
fn cleared_submission_does_not_anchor() {
    let session_id = SessionId::new();
    let history = (0..12)
        .map(|sequence| {
            event(
                session_id,
                sequence,
                SessionEventKind::AssistantMessage {
                    text: format!("message {sequence}"),
                },
            )
        })
        .collect::<Vec<_>>();
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    app.replace_composer_with("/help");
    app.stage_submission();
    let slash = app.take_pending_submission();
    app.clear_pending_submission(&slash);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert_ne!(output_line_y(&buffer, "message 11"), Some(1));
}

#[test]
fn tool_activity_after_submitted_user_message_resumes_following_latest_rows() {
    let session_id = SessionId::new();
    let history = (0..12)
        .map(|sequence| {
            event(
                session_id,
                sequence,
                SessionEventKind::AssistantMessage {
                    text: format!("message {sequence}"),
                },
            )
        })
        .collect::<Vec<_>>();
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 20));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    app.replace_composer_with("new prompt");
    app.stage_submission();
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 20));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    std::thread::sleep(Duration::from_millis(220));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 20));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    assert_eq!(output_line_y(&buffer, "You · sending"), Some(1));

    app.absorb_session_event(&event(
        session_id,
        12,
        SessionEventKind::UserMessage {
            client_id: ClientId::new(),
            text: "new prompt".to_owned(),
        },
    ));
    app.absorb_session_event(&event(
        session_id,
        13,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "tool-1".to_owned(),
            producer_plugin_id: None,
            tool_name: "shell.run".to_owned(),
            arguments_json: r#"{"command":"echo hi"}"#.to_owned(),
            legacy_request_presentation: Some(shell_legacy_request_presentation()),
        },
    ));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 20));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert!(rendered_text(&buffer).contains("shell.run"));
    assert_eq!(output_line_y(&buffer, "You"), Some(1));
}

#[test]
fn streaming_assistant_response_anchors_at_top_when_following() {
    let session_id = SessionId::new();
    let history = [event(
        session_id,
        0,
        SessionEventKind::UserMessage {
            client_id: ClientId::new(),
            text: "prompt".to_owned(),
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::AssistantDelta {
            text: "first line".to_owned(),
        },
    ));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    let initial_y = output_line_y(&buffer, "Bcode …").expect("streaming heading is visible");

    app.absorb_session_event(&event(
        session_id,
        2,
        SessionEventKind::AssistantDelta {
            text: "\nsecond line\nthird line\nfourth line".to_owned(),
        },
    ));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert_eq!(output_line_y(&buffer, "Bcode …"), Some(initial_y));
}

#[test]
fn manual_scroll_from_stream_anchor_preserves_visual_position() {
    let session_id = SessionId::new();
    let history = [event(
        session_id,
        0,
        SessionEventKind::UserMessage {
            client_id: ClientId::new(),
            text: "prompt".to_owned(),
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::AssistantDelta {
            text: "first
second
third
fourth
fifth
sixth
seventh
eighth
ninth"
                .to_owned(),
        },
    ));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    std::thread::sleep(Duration::from_millis(220));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    let initial_y = output_line_y(&buffer, "Bcode …").expect("streaming heading is visible");
    assert!(app.scroll_transcript_up(3));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert_eq!(output_line_y(&buffer, "Bcode …"), initial_y.checked_add(3));
}

#[test]
fn streaming_assistant_response_does_not_anchor_when_scrolled_up() {
    let session_id = SessionId::new();
    let history = (0..20)
        .map(|sequence| {
            event(
                session_id,
                sequence,
                SessionEventKind::AssistantMessage {
                    text: format!("message {sequence}"),
                },
            )
        })
        .collect::<Vec<_>>();
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    assert!(app.scroll_transcript_up(3));

    app.absorb_session_event(&event(
        session_id,
        21,
        SessionEventKind::AssistantDelta {
            text: "new stream".to_owned(),
        },
    ));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert!(app.scroll_offset() > 0);
    assert_eq!(output_line_y(&buffer, "Bcode …"), None);
}

#[test]
fn tool_activity_after_assistant_preamble_resumes_following_latest_rows() {
    let session_id = SessionId::new();
    let history = [event(
        session_id,
        0,
        SessionEventKind::UserMessage {
            client_id: ClientId::new(),
            text: "prompt".to_owned(),
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::AssistantDelta {
            text: "I'll inspect that first.".to_owned(),
        },
    ));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    std::thread::sleep(Duration::from_millis(220));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    assert_eq!(output_line_y(&buffer, "Bcode …"), Some(1));

    app.absorb_session_event(&event(
        session_id,
        2,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "tool-1".to_owned(),
            producer_plugin_id: None,
            tool_name: "shell.run".to_owned(),
            arguments_json: r#"{"command":"echo hi"}"#.to_owned(),
            legacy_request_presentation: None,
        },
    ));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert!(rendered_text(&buffer).contains("shell.run"));
    assert_ne!(output_line_y(&buffer, "Bcode …"), Some(1));
}

#[test]
fn manual_scroll_cancels_stream_anchor_for_remaining_deltas() {
    let session_id = SessionId::new();
    let history = [event(
        session_id,
        0,
        SessionEventKind::UserMessage {
            client_id: ClientId::new(),
            text: "prompt".to_owned(),
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::AssistantDelta {
            text: "first\nsecond\nthird\nfourth\nfifth\nsixth".to_owned(),
        },
    ));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    std::thread::sleep(Duration::from_millis(220));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    assert!(app.scroll_transcript_down(3));
    app.expire_manual_transcript_scroll_for_test();

    app.absorb_session_event(&event(
        session_id,
        2,
        SessionEventKind::AssistantDelta {
            text: "\nseventh\neighth".to_owned(),
        },
    ));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert!(app.bottom_overscroll() > 0);
}

#[test]
fn assistant_response_after_tool_loop_transitions_to_message_top() {
    let session_id = SessionId::new();
    let history = [event(
        session_id,
        0,
        SessionEventKind::UserMessage {
            client_id: ClientId::new(),
            text: "prompt".to_owned(),
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "tool-1".to_owned(),
            producer_plugin_id: None,
            tool_name: "shell.run".to_owned(),
            arguments_json: r#"{"command":"echo hi"}"#.to_owned(),
            legacy_request_presentation: Some(shell_legacy_request_presentation()),
        },
    ));
    app.absorb_session_event(&event(
        session_id,
        2,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "tool-1".to_owned(),
            result: "done".to_owned(),
            is_error: false,
            output: None,
            semantic_result: None,
        },
    ));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    app.absorb_session_event(&event(
        session_id,
        3,
        SessionEventKind::AssistantDelta {
            text: "final response".to_owned(),
        },
    ));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    std::thread::sleep(Duration::from_millis(220));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert_eq!(output_line_y(&buffer, "Bcode …"), Some(1));
}

#[test]
fn runtime_work_events_do_not_pull_final_response_to_bottom() {
    let session_id = SessionId::new();
    let history = [event(
        session_id,
        0,
        SessionEventKind::UserMessage {
            client_id: ClientId::new(),
            text: "prompt".to_owned(),
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::AssistantDelta {
            text: "final answer\nline 2\nline 3\nline 4".to_owned(),
        },
    ));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    std::thread::sleep(Duration::from_millis(220));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    app.absorb_session_event(&event(
        session_id,
        2,
        SessionEventKind::AssistantMessage {
            text: "final answer\nline 2\nline 3\nline 4".to_owned(),
        },
    ));
    app.absorb_session_event(&event(
        session_id,
        3,
        SessionEventKind::RuntimeWorkStarted {
            work_id: RuntimeWorkId::new("work-1"),
            kind: RuntimeWorkKind::ModelTurn,
            label: "model turn".to_owned(),
            tool_call_id: None,
            plugin_id: None,
            service_interface: None,
            operation: None,
            parent_work_id: None,
            started_at_ms: None,
            cancellable: false,
        },
    ));
    app.absorb_session_event(&event(
        session_id,
        4,
        SessionEventKind::ModelUsage {
            turn_id: "turn-1".to_owned(),
            usage: SessionTokenUsage::default(),
        },
    ));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert_eq!(output_line_y(&buffer, "Bcode"), Some(1));
}

#[test]
fn committed_user_echo_does_not_restart_submitted_message_anchor() {
    let session_id = SessionId::new();
    let history = (0..12)
        .map(|sequence| {
            event(
                session_id,
                sequence,
                SessionEventKind::AssistantMessage {
                    text: format!("message {sequence}"),
                },
            )
        })
        .collect::<Vec<_>>();
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    app.replace_composer_with("new prompt");
    app.stage_submission();
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    std::thread::sleep(Duration::from_millis(220));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    assert_eq!(output_line_y(&buffer, "You · sending"), Some(1));

    app.absorb_session_event(&event(
        session_id,
        12,
        SessionEventKind::UserMessage {
            client_id: ClientId::new(),
            text: "new prompt".to_owned(),
        },
    ));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);

    assert_eq!(output_line_y(&buffer, "You"), Some(1));
}

#[test]
fn streamed_tool_output_is_not_duplicated_by_final_result() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);

    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "call-1".to_owned(),
            producer_plugin_id: None,
            tool_name: "filesystem.shell.run".to_owned(),
            arguments_json: "{}".to_owned(),
            legacy_request_presentation: Some(file_edit_legacy_request_presentation()),
        },
    ));
    app.absorb_session_event(&event(
        session_id,
        2,
        SessionEventKind::ToolInvocationStream {
            event: ToolInvocationStreamEvent::Started {
                tool_call_id: "call-1".to_owned(),
                tool_name: "filesystem.shell.run".to_owned(),
                sequence: 0,
                terminal: true,
                columns: Some(80),
                rows: Some(24),
                started_at_ms: None,
            },
        },
    ));
    app.absorb_session_event(&event(
        session_id,
        3,
        SessionEventKind::ToolInvocationStream {
            event: ToolInvocationStreamEvent::OutputDelta {
                tool_call_id: "call-1".to_owned(),
                stream: ToolOutputStream::Pty,
                sequence: 0,
                text: "first\n".to_owned(),
                byte_len: "first\n".len(),
            },
        },
    ));
    app.absorb_session_event(&event(
        session_id,
        4,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call-1".to_owned(),
            result: "first\n".to_owned(),
            is_error: false,
            output: None,
            semantic_result: None,
        },
    ));

    let tool_results = app
        .transcript()
        .iter()
        .filter(|item| item.text() == "first\n")
        .collect::<Vec<_>>();
    assert_eq!(tool_results.len(), 1);
    assert_eq!(tool_results[0].text(), "first\n");
    assert!(!tool_results[0].streaming());
}

#[test]
fn streamed_terminal_history_suppresses_final_tool_result_tail() {
    let session_id = SessionId::new();
    let events = streamed_terminal_tool_events(session_id);

    let transcript = transcript_items_from_events_with_reasoning(&events, true);
    let terminal_items = transcript
        .iter()
        .filter(|item| matches!(item.kind(), TranscriptItemKind::TerminalOutput { .. }))
        .collect::<Vec<_>>();
    assert!(!transcript.iter().any(|item| {
        matches!(item.kind(), TranscriptItemKind::ToolResult { .. })
            && item.text().contains("final duplicate tail")
    }));

    assert_eq!(terminal_items.len(), 1);
    assert_eq!(terminal_items[0].text(), "live output\n");
    assert!(!terminal_items[0].streaming());
    let TranscriptItemKind::TerminalOutput {
        exit_code,
        timed_out,
        ..
    } = terminal_items[0].kind()
    else {
        panic!("expected terminal output");
    };
    assert_eq!(*exit_code, None);
    assert_eq!(*timed_out, Some(false));
}

#[test]
fn streamed_terminal_live_suppresses_final_tool_result_tail() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    for event in streamed_terminal_tool_events(session_id) {
        app.absorb_session_event(&event);
    }

    let terminal_items = app
        .transcript()
        .iter()
        .filter(|item| matches!(item.kind(), TranscriptItemKind::TerminalOutput { .. }))
        .collect::<Vec<_>>();
    assert!(!app.transcript().iter().any(|item| {
        matches!(item.kind(), TranscriptItemKind::ToolResult { .. })
            && item.text().contains("final duplicate tail")
    }));

    assert_eq!(terminal_items.len(), 1);
    assert_eq!(terminal_items[0].text(), "live output\n");
    assert!(!terminal_items[0].streaming());
    let TranscriptItemKind::TerminalOutput {
        exit_code,
        timed_out,
        ..
    } = terminal_items[0].kind()
    else {
        panic!("expected terminal output");
    };
    assert_eq!(*exit_code, None);
    assert_eq!(*timed_out, Some(false));
}

#[test]
fn file_change_artifact_history_renders_generic_tool_result_without_request() {
    let session_id = SessionId::new();
    let events = file_change_semantic_result_events(session_id, false);

    let transcript = transcript_items_from_events_with_reasoning(&events, true);

    assert!(transcript.iter().any(|item| {
        matches!(item.kind(), TranscriptItemKind::ToolResult { .. })
            && item.text().contains("File change")
            && item.text().contains("wrote 2 bytes")
            && item.text().contains("file.txt")
    }));
    assert!(!transcript.iter().any(|item| {
        matches!(item.kind(), TranscriptItemKind::ToolResult { .. })
            && item.text().contains("duplicate write result")
    }));
}

#[test]
fn file_change_artifact_history_uses_request_preview_when_present() {
    let session_id = SessionId::new();
    let events = file_change_semantic_result_events(session_id, true);

    let transcript = transcript_items_from_events_with_reasoning(&events, true);

    assert!(transcript.iter().any(|item| matches!(
        item.kind(),
        TranscriptItemKind::ToolRequest { tool_call_id, .. } if tool_call_id == "call-file"
    )));
    assert!(!transcript.iter().any(|item| {
        matches!(item.kind(), TranscriptItemKind::ToolResult { .. })
            && item.text().contains("duplicate write result")
    }));
}

#[test]
fn file_change_artifact_live_renders_generic_tool_result() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    for event in file_change_semantic_result_events(session_id, false) {
        app.absorb_session_event(&event);
    }

    assert!(app.transcript().iter().any(|item| {
        matches!(item.kind(), TranscriptItemKind::ToolResult { .. })
            && item.text().contains("File change")
            && item.text().contains("wrote 2 bytes")
            && item.text().contains("file.txt")
    }));
    assert!(!app.transcript().iter().any(|item| {
        matches!(item.kind(), TranscriptItemKind::ToolResult { .. })
            && item.text().contains("duplicate write result")
    }));
}

fn file_change_semantic_result_events(
    session_id: SessionId,
    include_request: bool,
) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    if include_request {
        events.push(event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-file".to_owned(),
                producer_plugin_id: None,
                tool_name: "example.write".to_owned(),
                arguments_json: r#"{"path":"file.txt","contents":"hi"}"#.to_owned(),
                legacy_request_presentation: Some(file_edit_legacy_request_presentation()),
            },
        ));
    }
    events.push(event(
        session_id,
        2,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call-file".to_owned(),
            result: "wrote 2 bytes".to_owned(),
            is_error: false,
            output: None,
            semantic_result: Some(file_change_artifact(
                &bcode_session_models::FileChangeResult {
                    tool_name: "example.write".to_owned(),
                    summary: "wrote 2 bytes".to_owned(),
                    path: Some("file.txt".to_owned()),
                },
            )),
        },
    ));
    events
}

#[test]
fn streamed_tool_without_output_renders_final_result() {
    let session_id = SessionId::new();
    let events = vec![
        event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-empty".to_owned(),
                producer_plugin_id: None,
                tool_name: "shell.run".to_owned(),
                arguments_json: "{}".to_owned(),
                legacy_request_presentation: Some(shell_legacy_request_presentation()),
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::Started {
                    tool_call_id: "call-empty".to_owned(),
                    tool_name: "shell.run".to_owned(),
                    sequence: 0,
                    terminal: true,
                    columns: Some(120),
                    rows: Some(40),
                    started_at_ms: None,
                },
            },
        ),
        event(
            session_id,
            3,
            SessionEventKind::ToolCallFinished {
                tool_call_id: "call-empty".to_owned(),
                result: "final result".to_owned(),
                is_error: false,
                output: None,
                semantic_result: None,
            },
        ),
    ];

    let transcript = transcript_items_from_events_with_reasoning(&events, true);

    assert!(transcript.iter().any(|item| item.text() == "final result"));
}

#[test]
fn streamed_terminal_output_renders_finished_elapsed_duration() {
    let session_id = SessionId::new();
    let events = streamed_terminal_tool_events(session_id);
    let mut app = BmuxApp::new_with_history(Some(session_id), &events, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 30));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);

    assert!(
        output.contains("Terminal · shell.run · completed · duration 1.5s"),
        "{output}"
    );
    assert!(output.contains("completed · duration 1.5s"), "{output}");
    assert!(!output.contains("terminal: 120x40"), "{output}");
}

fn streamed_terminal_tool_events(session_id: SessionId) -> Vec<SessionEvent> {
    vec![
        event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-stream".to_owned(),
                producer_plugin_id: None,
                tool_name: "shell.run".to_owned(),
                arguments_json: "{}".to_owned(),
                legacy_request_presentation: Some(shell_legacy_request_presentation()),
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::Started {
                    tool_call_id: "call-stream".to_owned(),
                    tool_name: "shell.run".to_owned(),
                    sequence: 0,
                    terminal: true,
                    columns: Some(120),
                    rows: Some(40),
                    started_at_ms: Some(1_000),
                },
            },
        ),
        event(
            session_id,
            3,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::OutputDelta {
                    tool_call_id: "call-stream".to_owned(),
                    stream: ToolOutputStream::Pty,
                    sequence: 1,
                    text: "live output\n".to_owned(),
                    byte_len: "live output\n".len(),
                },
            },
        ),
        event(
            session_id,
            4,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::Finished {
                    tool_call_id: "call-stream".to_owned(),
                    sequence: 2,
                    is_error: false,
                    finished_at_ms: Some(2_500),
                },
            },
        ),
        event(
            session_id,
            5,
            SessionEventKind::ToolCallFinished {
                tool_call_id: "call-stream".to_owned(),
                result: "final duplicate tail".to_owned(),
                is_error: false,
                output: None,
                semantic_result: Some(shell_result_artifact(&ShellRunResult::Terminal {
                    exit_code: Some(7),
                    timed_out: true,
                    cancelled: false,
                    duration_ms: None,
                    output_tail: "final duplicate tail".to_owned(),
                    output_truncated: false,
                    output_bytes: Some("final duplicate tail".len() as u64),
                    retained_output_bytes: Some("final duplicate tail".len() as u64),
                    columns: 120,
                    rows: 40,
                })),
            },
        ),
    ]
}

#[test]
fn semantic_terminal_result_without_live_delta_renders_terminal_history() {
    let session_id = SessionId::new();
    let events = vec![
        event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-no-live".to_owned(),
                producer_plugin_id: None,
                tool_name: "shell.run".to_owned(),
                arguments_json: "{}".to_owned(),
                legacy_request_presentation: Some(shell_legacy_request_presentation()),
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::Started {
                    tool_call_id: "call-no-live".to_owned(),
                    tool_name: "shell.run".to_owned(),
                    sequence: 0,
                    terminal: true,
                    columns: Some(80),
                    rows: Some(24),
                    started_at_ms: Some(1_000),
                },
            },
        ),
        event(
            session_id,
            3,
            SessionEventKind::ToolCallFinished {
                tool_call_id: "call-no-live".to_owned(),
                result: String::new(),
                is_error: false,
                output: None,
                semantic_result: Some(shell_result_artifact(&ShellRunResult::Terminal {
                    exit_code: Some(0),
                    timed_out: false,
                    cancelled: false,
                    duration_ms: None,
                    output_tail: String::new(),
                    output_truncated: false,
                    output_bytes: Some(0),
                    retained_output_bytes: Some(0),
                    columns: 80,
                    rows: 24,
                })),
            },
        ),
    ];

    let app = BmuxApp::new_with_history(Some(session_id), &events, &[], false);
    let terminal_count = app
        .transcript()
        .iter()
        .filter(|item| matches!(item.kind(), TranscriptItemKind::TerminalOutput { .. }))
        .count();
    let tool_result_count = app
        .transcript()
        .iter()
        .filter(|item| matches!(item.kind(), TranscriptItemKind::ToolResult { .. }))
        .count();

    assert_eq!(terminal_count, 0);
    assert_eq!(tool_result_count, 1);
}

#[test]
fn live_shell_result_preserves_request_block() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    let events = vec![
        event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-live-shell".to_owned(),
                producer_plugin_id: None,
                tool_name: "shell.run".to_owned(),
                arguments_json: serde_json::json!({
                    "command": "echo hello",
                    "terminal": false,
                })
                .to_string(),
                legacy_request_presentation: Some(shell_legacy_request_presentation()),
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::ToolCallFinished {
                tool_call_id: "call-live-shell".to_owned(),
                result: String::new(),
                is_error: false,
                output: None,
                semantic_result: Some(shell_result_artifact(&ShellRunResult::Captured {
                    exit_code: Some(0),
                    timed_out: false,
                    cancelled: false,
                    duration_ms: None,
                    stdout: "hello\n".to_owned(),
                    stderr: String::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                    stdout_bytes: Some(6),
                    stderr_bytes: Some(0),
                })),
            },
        ),
    ];

    for event in events {
        app.absorb_session_event(&event);
    }
    let transcript = app.transcript();

    assert!(transcript.iter().any(|item| {
        matches!(item.kind(), TranscriptItemKind::ToolRequest { .. })
            && item.text().contains("echo hello")
    }));
    assert!(transcript.iter().any(|item| {
        matches!(item.kind(), TranscriptItemKind::ToolResult { .. })
            && item.text().contains("Shell run")
    }));
}

#[test]
fn live_streamed_shell_result_preserves_request_and_suppresses_artifact_duplicate() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    let events = vec![
        event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-live-stream".to_owned(),
                producer_plugin_id: None,
                tool_name: "shell.run".to_owned(),
                arguments_json: serde_json::json!({
                    "command": "cargo test",
                    "terminal": true,
                })
                .to_string(),
                legacy_request_presentation: Some(shell_legacy_request_presentation()),
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::Started {
                    tool_call_id: "call-live-stream".to_owned(),
                    tool_name: "shell.run".to_owned(),
                    sequence: 0,
                    terminal: true,
                    columns: Some(80),
                    rows: Some(24),
                    started_at_ms: Some(10),
                },
            },
        ),
        event(
            session_id,
            3,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::OutputDelta {
                    tool_call_id: "call-live-stream".to_owned(),
                    stream: ToolOutputStream::Pty,
                    sequence: 1,
                    text: "running\n".to_owned(),
                    byte_len: 8,
                },
            },
        ),
        event(
            session_id,
            4,
            SessionEventKind::ToolCallFinished {
                tool_call_id: "call-live-stream".to_owned(),
                result: String::new(),
                is_error: false,
                output: None,
                semantic_result: Some(shell_result_artifact(&ShellRunResult::Terminal {
                    exit_code: Some(0),
                    timed_out: false,
                    cancelled: false,
                    duration_ms: None,
                    output_tail: "final duplicate tail\n".to_owned(),
                    output_truncated: false,
                    output_bytes: Some(21),
                    retained_output_bytes: Some(21),
                    columns: 80,
                    rows: 24,
                })),
            },
        ),
    ];

    for event in events {
        app.absorb_session_event(&event);
    }
    let transcript = app.transcript();

    assert!(transcript.iter().any(|item| {
        matches!(item.kind(), TranscriptItemKind::ToolRequest { .. })
            && item.text().contains("cargo test")
    }));
    assert!(transcript.iter().any(|item| {
        matches!(item.kind(), TranscriptItemKind::TerminalOutput { .. })
            && item.text().contains("running")
    }));
    assert!(!transcript.iter().any(|item| {
        matches!(item.kind(), TranscriptItemKind::ToolResult { .. })
            && item.text().contains("Shell run")
    }));
}

#[test]
fn live_shell_preview_with_streamed_output_preserves_preview_and_suppresses_artifact_duplicate() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    app.absorb_session_live_event(&live_shell_preview_stream_preview(session_id));
    for event in live_shell_preview_stream_events(session_id) {
        app.absorb_session_event(&event);
    }
    let transcript = app.transcript();

    assert!(
        transcript
            .iter()
            .any(|item| item.is_live_preview_anchor_for("call-live-preview-stream"))
    );
    assert!(
        !transcript
            .iter()
            .any(|item| matches!(item.kind(), TranscriptItemKind::ToolRequest { .. }))
    );
    assert!(transcript.iter().any(|item| {
        matches!(item.kind(), TranscriptItemKind::TerminalOutput { .. })
            && item.text().contains("running")
    }));
    assert!(!transcript.iter().any(|item| {
        matches!(item.kind(), TranscriptItemKind::ToolResult { .. })
            && item.text().contains("Shell run")
    }));

    let mut buffer = Buffer::empty(Rect::new(0, 0, 90, 30));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);
    assert!(output.contains("Shell command"), "{output}");
    assert!(output.contains("command: cargo test"), "{output}");
    assert!(output.contains("running"), "{output}");
    assert!(!output.contains("final duplicate tail"), "{output}");
}

fn live_shell_preview_stream_preview(
    session_id: SessionId,
) -> bcode_session_models::SessionLiveEvent {
    bcode_session_models::SessionLiveEvent {
        session_id,
        kind: bcode_session_models::SessionLiveEventKind::ToolArgumentPreview {
            turn_id: "turn-1".to_owned(),
            tool_call_id: "call-live-preview-stream".to_owned(),
            tool_name: "shell.run".to_owned(),
            argument_bytes: 28,
            preview: LiveToolArgumentPreview::ShellCommand(LiveShellCommandPreview {
                preview_title: None,
                streaming_status: None,
                command_prefix: "cargo test".to_owned(),
                cwd: Some("/repo".to_owned()),
                argument_bytes: 28,
                truncated: false,
            }),
        },
    }
}

fn live_shell_preview_stream_events(session_id: SessionId) -> Vec<SessionEvent> {
    vec![
        live_shell_preview_stream_request(session_id),
        live_shell_preview_stream_started(session_id),
        live_shell_preview_stream_output(session_id),
        live_shell_preview_stream_finished(session_id),
    ]
}

fn live_shell_preview_stream_request(session_id: SessionId) -> SessionEvent {
    event(
        session_id,
        1,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "call-live-preview-stream".to_owned(),
            producer_plugin_id: None,
            tool_name: "shell.run".to_owned(),
            arguments_json: serde_json::json!({
                "command": "cargo test",
                "terminal": true,
            })
            .to_string(),
            legacy_request_presentation: Some(shell_legacy_request_presentation()),
        },
    )
}

fn live_shell_preview_stream_started(session_id: SessionId) -> SessionEvent {
    event(
        session_id,
        2,
        SessionEventKind::ToolInvocationStream {
            event: ToolInvocationStreamEvent::Started {
                tool_call_id: "call-live-preview-stream".to_owned(),
                tool_name: "shell.run".to_owned(),
                sequence: 0,
                terminal: true,
                columns: Some(80),
                rows: Some(24),
                started_at_ms: Some(10),
            },
        },
    )
}

fn live_shell_preview_stream_output(session_id: SessionId) -> SessionEvent {
    event(
        session_id,
        3,
        SessionEventKind::ToolInvocationStream {
            event: ToolInvocationStreamEvent::OutputDelta {
                tool_call_id: "call-live-preview-stream".to_owned(),
                stream: ToolOutputStream::Pty,
                sequence: 1,
                text: "running\n".to_owned(),
                byte_len: 8,
            },
        },
    )
}

fn live_shell_preview_stream_finished(session_id: SessionId) -> SessionEvent {
    event(
        session_id,
        4,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call-live-preview-stream".to_owned(),
            result: String::new(),
            is_error: false,
            output: None,
            semantic_result: Some(shell_result_artifact(&ShellRunResult::Terminal {
                exit_code: Some(0),
                timed_out: false,
                cancelled: false,
                duration_ms: None,
                output_tail: "final duplicate tail\n".to_owned(),
                output_truncated: false,
                output_bytes: Some(21),
                retained_output_bytes: Some(21),
                columns: 80,
                rows: 24,
            })),
        },
    )
}

#[test]
fn semantic_terminal_result_without_stream_renders_generic_artifact() {
    let session_id = SessionId::new();
    let events = vec![event(
        session_id,
        1,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call-terminal".to_owned(),
            result: String::new(),
            is_error: false,
            output: None,
            semantic_result: Some(shell_result_artifact(&ShellRunResult::Terminal {
                exit_code: Some(0),
                timed_out: false,
                cancelled: false,
                duration_ms: None,
                output_tail: "ansi tail\n".to_owned(),
                output_truncated: false,
                output_bytes: Some(10),
                retained_output_bytes: Some(10),
                columns: 80,
                rows: 24,
            })),
        },
    )];

    let transcript = transcript_items_from_events_with_reasoning(&events, true);

    assert!(
        !transcript
            .iter()
            .any(|item| matches!(item.kind(), TranscriptItemKind::TerminalOutput { .. }))
    );
    assert!(transcript.iter().any(|item| {
        matches!(item.kind(), TranscriptItemKind::ToolResult { .. })
            && item.text().contains("Shell run")
            && item.text().contains("test.shell-artifact")
    }));
}

#[test]
fn semantic_terminal_result_suppresses_existing_stream_item_duplicate_result() {
    let session_id = SessionId::new();
    let events = vec![
        event(
            session_id,
            1,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::Started {
                    tool_call_id: "call-stream-semantic".to_owned(),
                    tool_name: "shell.run".to_owned(),
                    sequence: 0,
                    terminal: true,
                    columns: Some(80),
                    rows: Some(24),
                    started_at_ms: Some(10),
                },
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::OutputDelta {
                    tool_call_id: "call-stream-semantic".to_owned(),
                    stream: ToolOutputStream::Pty,
                    sequence: 1,
                    text: "live\n".to_owned(),
                    byte_len: 5,
                },
            },
        ),
        event(
            session_id,
            3,
            SessionEventKind::ToolCallFinished {
                tool_call_id: "call-stream-semantic".to_owned(),
                result: r#"{"mode":"terminal"}"#.to_owned(),
                is_error: true,
                output: None,
                semantic_result: Some(shell_result_artifact(&ShellRunResult::Terminal {
                    exit_code: Some(2),
                    timed_out: true,
                    cancelled: false,
                    duration_ms: None,
                    output_tail: "final tail\n".to_owned(),
                    output_truncated: false,
                    output_bytes: Some(11),
                    retained_output_bytes: Some(11),
                    columns: 80,
                    rows: 24,
                })),
            },
        ),
    ];

    let transcript = transcript_items_from_events_with_reasoning(&events, true);
    let terminal_items = transcript
        .iter()
        .filter(|item| matches!(item.kind(), TranscriptItemKind::TerminalOutput { .. }))
        .collect::<Vec<_>>();

    assert_eq!(terminal_items.len(), 1);
    assert_eq!(terminal_items[0].text(), "live\n");
    assert!(terminal_items[0].streaming());
    let TranscriptItemKind::TerminalOutput {
        exit_code,
        timed_out,
        is_error,
        ..
    } = terminal_items[0].kind()
    else {
        panic!("expected terminal output");
    };
    assert_eq!(*exit_code, None);
    assert_eq!(*timed_out, None);
    assert!(!*is_error);
    assert!(!transcript.iter().any(|item| {
        matches!(item.kind(), TranscriptItemKind::ToolResult { .. })
            && item.text().contains(r#""mode":"terminal""#)
    }));
}

#[test]
fn semantic_captured_shell_result_renders_generic_artifact() {
    let session_id = SessionId::new();
    let events = vec![event(
        session_id,
        1,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call-captured".to_owned(),
            result: "legacy output should not be used".to_owned(),
            is_error: false,
            output: None,
            semantic_result: Some(shell_result_artifact(&ShellRunResult::Captured {
                exit_code: Some(0),
                timed_out: false,
                cancelled: false,
                duration_ms: None,
                stdout: "captured stdout\n".to_owned(),
                stderr: "captured stderr\n".to_owned(),
                stdout_truncated: false,
                stderr_truncated: true,
                stdout_bytes: Some(16),
                stderr_bytes: Some(16),
            })),
        },
    )];

    let transcript = transcript_items_from_events_with_reasoning(&events, true);

    assert!(
        !transcript
            .iter()
            .any(|item| matches!(item.kind(), TranscriptItemKind::TerminalOutput { .. }))
    );
    assert!(transcript.iter().any(|item| {
        matches!(item.kind(), TranscriptItemKind::ToolResult { .. })
            && item.text().contains("Shell run")
            && item.text().contains("test.shell-artifact")
    }));
    assert!(
        !transcript
            .iter()
            .any(|item| item.text().contains("legacy output should not be used"))
    );
}

#[test]
fn legacy_terminal_result_renders_plain_tool_result() {
    let session_id = SessionId::new();
    let events = vec![
        event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-old-order".to_owned(),
                producer_plugin_id: None,
                tool_name: "shell.run".to_owned(),
                arguments_json: "{}".to_owned(),
                legacy_request_presentation: Some(shell_legacy_request_presentation()),
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::ToolCallFinished {
                tool_call_id: "call-old-order".to_owned(),
                result: serde_json::json!({
                    "mode": "terminal",
                    "exit_code": 0,
                    "timed_out": false,
                    "output": "legacy tail\n",
                    "columns": 80,
                    "rows": 24,
                })
                .to_string(),
                is_error: false,
                output: None,
                semantic_result: None,
            },
        ),
    ];

    let transcript = transcript_items_from_events_with_reasoning(&events, true);
    let terminal_count = transcript
        .iter()
        .filter(|item| matches!(item.kind(), TranscriptItemKind::TerminalOutput { .. }))
        .count();
    let raw_json_count = transcript
        .iter()
        .filter(|item| item.text().contains(r#""mode":"terminal""#))
        .count();

    assert_eq!(terminal_count, 0);
    assert_eq!(raw_json_count, 1);
}

#[test]
fn presentation_events_replay_ignores_legacy_presentation_events() {
    let session_id = SessionId::new();
    let events = vec![
        event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-present".to_owned(),
                producer_plugin_id: None,
                tool_name: "third.party".to_owned(),
                arguments_json: "{}".to_owned(),
                legacy_request_presentation: None,
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::LegacyPresentation {
                    tool_call_id: "call-present".to_owned(),
                    sequence: 1,
                    presentation: LegacyToolPresentationEvent::Status(
                        LegacyToolStatusPresentation {
                            target: LegacyToolPresentationTarget::Result,
                            text: "custom status".to_owned(),
                            level: LegacyToolPresentationLevel::Success,
                        },
                    ),
                },
            },
        ),
    ];

    let transcript = transcript_items_from_events_with_reasoning(&events, true);

    assert!(transcript.iter().any(|item| matches!(
        item.kind(),
        TranscriptItemKind::ToolRequest { tool_call_id, .. } if tool_call_id == "call-present"
    )));
    assert!(
        !transcript
            .iter()
            .any(|item| item.text().contains("custom status"))
    );
}

#[test]
fn legacy_presentation_card_does_not_replace_tool_request_surface() {
    let session_id = SessionId::new();
    let events = vec![
        event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-present".to_owned(),
                producer_plugin_id: None,
                tool_name: "third.party".to_owned(),
                arguments_json: serde_json::json!({"raw": "arguments"}).to_string(),
                legacy_request_presentation: None,
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::LegacyPresentation {
                    tool_call_id: "call-present".to_owned(),
                    sequence: 1,
                    presentation: LegacyToolPresentationEvent::Card(
                        bcode_session_models::LegacyToolCardPresentation {
                            target: LegacyToolPresentationTarget::Preview,
                            title: "Plugin preview".to_owned(),
                            subtitle: None,
                            sections: Vec::new(),
                        },
                    ),
                },
            },
        ),
    ];

    let transcript = transcript_items_from_events_with_reasoning(&events, true);

    assert!(transcript.iter().any(|item| matches!(
        item.kind(),
        TranscriptItemKind::ToolRequest { tool_call_id, .. } if tool_call_id == "call-present"
    )));
    assert!(
        transcript
            .iter()
            .any(|item| item.text().contains("arguments"))
    );
    assert!(
        !transcript
            .iter()
            .any(|item| item.text().contains("Plugin preview"))
    );
}

#[test]
fn legacy_result_presentation_does_not_suppress_generic_tool_result() {
    let session_id = SessionId::new();
    let events = vec![
        event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-present".to_owned(),
                producer_plugin_id: None,
                tool_name: "third.party".to_owned(),
                arguments_json: serde_json::json!({"raw": "arguments"}).to_string(),
                legacy_request_presentation: None,
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::LegacyPresentation {
                    tool_call_id: "call-present".to_owned(),
                    sequence: 1,
                    presentation: LegacyToolPresentationEvent::Card(
                        bcode_session_models::LegacyToolCardPresentation {
                            target: LegacyToolPresentationTarget::Result,
                            title: "Plugin result".to_owned(),
                            subtitle: None,
                            sections: Vec::new(),
                        },
                    ),
                },
            },
        ),
        event(
            session_id,
            3,
            SessionEventKind::ToolCallFinished {
                tool_call_id: "call-present".to_owned(),
                result: "raw result".to_owned(),
                is_error: false,
                output: None,
                semantic_result: None,
            },
        ),
    ];

    let transcript = transcript_items_from_events_with_reasoning(&events, true);

    assert!(
        !transcript
            .iter()
            .any(|item| item.text().contains("Plugin result"))
    );
    assert!(transcript.iter().any(|item| matches!(
        item.kind(),
        TranscriptItemKind::ToolResult { result, .. } if result == "raw result"
    )));
}

#[test]
fn presentation_clear_removes_replayed_transcript_card() {
    let session_id = SessionId::new();
    let events = vec![
        event(
            session_id,
            1,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::LegacyPresentation {
                    tool_call_id: "call-present".to_owned(),
                    sequence: 1,
                    presentation: LegacyToolPresentationEvent::Status(
                        LegacyToolStatusPresentation {
                            target: LegacyToolPresentationTarget::Result,
                            text: "custom status".to_owned(),
                            level: LegacyToolPresentationLevel::Success,
                        },
                    ),
                },
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::LegacyPresentation {
                    tool_call_id: "call-present".to_owned(),
                    sequence: 2,
                    presentation: LegacyToolPresentationEvent::Clear {
                        target: LegacyToolPresentationTarget::Result,
                    },
                },
            },
        ),
    ];

    let transcript = transcript_items_from_events_with_reasoning(&events, true);

    assert!(
        !transcript
            .iter()
            .any(|item| item.text().contains("custom status"))
    );
}

#[test]
fn legacy_live_presentation_card_is_ignored_by_normal_transcript() {
    let session_id = SessionId::new();
    let events = vec![
        event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-present".to_owned(),
                producer_plugin_id: None,
                tool_name: "third.party".to_owned(),
                arguments_json: serde_json::json!({"raw": "arguments"}).to_string(),
                legacy_request_presentation: None,
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::LegacyPresentation {
                    tool_call_id: "call-present".to_owned(),
                    sequence: 1,
                    presentation: LegacyToolPresentationEvent::Card(
                        bcode_session_models::LegacyToolCardPresentation {
                            target: LegacyToolPresentationTarget::Result,
                            title: "Custom result".to_owned(),
                            subtitle: Some("plugin-owned".to_owned()),
                            sections: vec![LegacyToolPresentationSection::Text {
                                label: Some("Summary".to_owned()),
                                text: "Rendered from plugin presentation".to_owned(),
                            }],
                        },
                    ),
                },
            },
        ),
    ];

    let transcript = transcript_items_from_events_with_reasoning(&events, true);

    assert!(transcript.iter().any(|item| matches!(
        item.kind(),
        TranscriptItemKind::ToolRequest { tool_call_id, .. } if tool_call_id == "call-present"
    )));
    assert!(
        !transcript
            .iter()
            .any(|item| item.text().contains("Custom result"))
    );
}

#[test]
fn semantic_text_result_renders_generic_tool_result() {
    let session_id = SessionId::new();
    let events = vec![event(
        session_id,
        1,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call-text".to_owned(),
            result: "legacy text".to_owned(),
            is_error: false,
            output: None,
            semantic_result: Some(ToolInvocationResult::Text {
                text: "semantic text".to_owned(),
            }),
        },
    )];

    let transcript = transcript_items_from_events_with_reasoning(&events, true);

    assert!(transcript.iter().any(|item| matches!(
        item.kind(),
        TranscriptItemKind::ToolResult { result, .. } if result == "semantic text"
    )));
    assert!(
        !transcript
            .iter()
            .any(|item| item.text().contains("legacy text"))
    );
}

#[test]
fn live_semantic_terminal_result_finishes_stream_with_semantic_status_not_legacy_json() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);

    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::ToolInvocationStream {
            event: ToolInvocationStreamEvent::Started {
                tool_call_id: "call-live-semantic".to_owned(),
                tool_name: "shell.run".to_owned(),
                sequence: 0,
                terminal: true,
                columns: Some(80),
                rows: Some(24),
                started_at_ms: Some(10),
            },
        },
    ));
    app.absorb_session_event(&event(
        session_id,
        2,
        SessionEventKind::ToolInvocationStream {
            event: ToolInvocationStreamEvent::OutputDelta {
                tool_call_id: "call-live-semantic".to_owned(),
                stream: ToolOutputStream::Pty,
                sequence: 1,
                text: "live\n".to_owned(),
                byte_len: 5,
            },
        },
    ));
    app.absorb_session_event(&event(
        session_id,
        3,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call-live-semantic".to_owned(),
            result: r#"{"mode":"terminal","exit_code":0,"timed_out":false}"#.to_owned(),
            is_error: true,
            output: None,
            semantic_result: Some(shell_result_artifact(&ShellRunResult::Terminal {
                exit_code: Some(7),
                timed_out: true,
                cancelled: false,
                duration_ms: None,
                output_tail: "semantic final tail\n".to_owned(),
                output_truncated: false,
                output_bytes: Some(20),
                retained_output_bytes: Some(20),
                columns: 80,
                rows: 24,
            })),
        },
    ));

    let terminal_items = app
        .transcript()
        .iter()
        .filter(|item| matches!(item.kind(), TranscriptItemKind::TerminalOutput { .. }))
        .collect::<Vec<_>>();

    assert_eq!(terminal_items.len(), 1);
    assert_eq!(terminal_items[0].text(), "live\n");
    let TranscriptItemKind::TerminalOutput {
        exit_code,
        timed_out,
        is_error,
        ..
    } = terminal_items[0].kind()
    else {
        panic!("expected terminal output");
    };
    assert_eq!(*exit_code, None);
    assert_eq!(*timed_out, Some(false));
    assert!(*is_error);
}

fn session_summary(session_id: SessionId) -> SessionSummary {
    SessionSummary {
        id: session_id,
        name: Some("Opened session".to_owned()),
        explicit_name: Some("Opened session".to_owned()),
        derived_title: None,
        title_source: SessionTitleSource::Explicit,
        client_count: 1,
        created_at_ms: 1,
        updated_at_ms: 2,
        working_directory: "/tmp/bcode-tui-test".into(),
        import: None,
        fork: None,
    }
}

#[test]
fn transcript_resident_window_trims_live_bottom_following_turns() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);

    for turn in 0..600_u64 {
        let user_sequence = turn.saturating_mul(2).saturating_add(1);
        app.absorb_session_event(&event(
            session_id,
            user_sequence,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: format!("user {turn}"),
            },
        ));
        app.absorb_session_event(&event(
            session_id,
            user_sequence.saturating_add(1),
            SessionEventKind::AssistantMessage {
                text: format!("assistant {turn}"),
            },
        ));
    }

    assert!(app.resident_transcript_event_count() <= 700);
    assert!(
        app.resident_transcript_oldest_sequence()
            .is_some_and(|sequence| sequence > 1)
    );
    assert!(app.has_older_history());
    let cursor = app.older_history_cursor().expect("dropped history cursor");
    assert_eq!(
        cursor.sequence,
        app.resident_transcript_oldest_sequence()
            .expect("oldest resident sequence")
            .saturating_sub(1)
    );
    assert!(
        app.transcript()
            .iter()
            .any(|item| item.text().contains("assistant 599"))
    );
}

#[test]
fn transcript_resident_window_does_not_trim_with_active_tool() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);

    for turn in 0..500_u64 {
        let user_sequence = turn.saturating_mul(2).saturating_add(1);
        app.absorb_session_event(&event(
            session_id,
            user_sequence,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: format!("user {turn}"),
            },
        ));
        app.absorb_session_event(&event(
            session_id,
            user_sequence.saturating_add(1),
            SessionEventKind::AssistantMessage {
                text: format!("assistant {turn}"),
            },
        ));
    }
    app.absorb_session_event(&event(
        session_id,
        1_001,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "active-tool".to_owned(),
            producer_plugin_id: None,
            tool_name: "shell.run".to_owned(),
            arguments_json: "{}".to_owned(),
            legacy_request_presentation: Some(shell_legacy_request_presentation()),
        },
    ));
    for index in 0..50_u64 {
        app.absorb_session_event(&event(
            session_id,
            1_002_u64.saturating_add(index),
            SessionEventKind::AssistantDelta {
                text: format!("still active {index}"),
            },
        ));
    }

    assert!(app.resident_transcript_event_count() > 1_024);
}

#[test]
fn transcript_resident_window_prunes_old_tool_state_after_trim() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);

    for turn in 0..360_u64 {
        let base = turn.saturating_mul(3).saturating_add(1);
        let tool_call_id = format!("tool-{turn}");
        app.absorb_session_event(&event(
            session_id,
            base,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: format!("user {turn}"),
            },
        ));
        app.absorb_session_event(&event(
            session_id,
            base.saturating_add(1),
            SessionEventKind::ToolCallRequested {
                tool_call_id: tool_call_id.clone(),
                producer_plugin_id: None,
                tool_name: "shell.run".to_owned(),
                arguments_json: "{}".to_owned(),
                legacy_request_presentation: Some(shell_legacy_request_presentation()),
            },
        ));
        app.absorb_session_event(&event(
            session_id,
            base.saturating_add(2),
            SessionEventKind::ToolCallFinished {
                tool_call_id,
                result: "ok".to_owned(),
                is_error: false,
                output: None,
                semantic_result: None,
            },
        ));
    }

    assert!(app.resident_transcript_event_count() <= 600);
    assert!(app.resident_tool_call_context_count() < 360);
    assert_eq!(app.resident_streamed_tool_result_count(), 0);
}

fn filesystem_change_artifact() -> bcode_session_models::ToolArtifact {
    bcode_session_models::ToolArtifact {
        artifact_id: "call-file-filesystem-change".to_owned(),
        producer_plugin_id: "bcode.filesystem".to_owned(),
        schema: "bcode.filesystem.change".to_owned(),
        schema_version: 1,
        tool_call_id: Some("call-file".to_owned()),
        title: Some("File change".to_owned()),
        metadata: serde_json::json!({
            "summary": "edited file",
            "path": "/tmp/hello.txt",
            "old_text": "before\n",
            "new_text": "after\n"
        }),
        refs: Vec::new(),
    }
}

fn filesystem_plugin_host() -> bcode_plugin::PluginHost {
    let bundled = [bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/filesystem-plugin/bcode-plugin.toml"),
        bcode_filesystem_plugin::static_plugin(),
    )];
    let selected = bcode_plugin::filter_selected_static_plugins(
        &bundled,
        &bcode_plugin::PluginSelection::all_enabled(),
    )
    .expect("static filesystem plugin manifest should parse");
    bcode_plugin::PluginHost::load_static_plugins(&selected)
        .expect("static filesystem plugin should load")
}

fn shell_run_artifact() -> bcode_session_models::ToolArtifact {
    bcode_session_models::ToolArtifact {
        artifact_id: "call-shell-shell-run".to_owned(),
        producer_plugin_id: "bcode.shell".to_owned(),
        schema: "bcode.shell.run".to_owned(),
        schema_version: 1,
        tool_call_id: Some("call-shell".to_owned()),
        title: Some("Shell run".to_owned()),
        metadata: serde_json::json!({
            "mode": "terminal",
            "exit_code": 0,
            "timed_out": false,
            "cancelled": false,
            "duration_ms": 12,
            "output_tail": "shell raw output\n",
            "output_truncated": false,
            "output_bytes": 17,
            "retained_output_bytes": 17,
            "columns": 80,
            "rows": 24
        }),
        refs: Vec::new(),
    }
}

fn shell_plugin_host() -> bcode_plugin::PluginHost {
    let bundled = [bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/shell-plugin/bcode-plugin.toml"),
        bcode_shell_plugin::static_plugin(),
    )];
    let selected = bcode_plugin::filter_selected_static_plugins(
        &bundled,
        &bcode_plugin::PluginSelection::all_enabled(),
    )
    .expect("static shell plugin manifest should parse");
    bcode_plugin::PluginHost::load_static_plugins(&selected)
        .expect("static shell plugin should load")
}

fn question_outcome_artifact() -> bcode_session_models::ToolArtifact {
    bcode_session_models::ToolArtifact {
        artifact_id: "question-outcome-call-question".to_owned(),
        producer_plugin_id: "bcode.question".to_owned(),
        schema: "bcode.question.outcome".to_owned(),
        schema_version: 1,
        tool_call_id: Some("call-question".to_owned()),
        title: Some("Question outcome".to_owned()),
        metadata: serde_json::json!({
            "status": "answered",
            "questions": [{
                "question_index": 0,
                "header": "Decision",
                "question": "Proceed?",
                "status": "answered",
                "selected": [{"label": "Yes", "value": "yes"}],
                "custom": null,
                "required": true
            }]
        }),
        refs: Vec::new(),
    }
}

fn question_plugin_host() -> bcode_plugin::PluginHost {
    let bundled = [bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/question-plugin/bcode-plugin.toml"),
        bcode_question_plugin::static_plugin(),
    )];
    let selected = bcode_plugin::filter_selected_static_plugins(
        &bundled,
        &bcode_plugin::PluginSelection::all_enabled(),
    )
    .expect("static question plugin manifest should parse");
    bcode_plugin::PluginHost::load_static_plugins(&selected)
        .expect("static question plugin should load")
}

fn render_app_text(app: &mut BmuxApp) -> String {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 40));
    let mut frame = Frame::new(&mut buffer);
    render::render(app, &mut frame);
    rendered_text(&buffer)
}

fn rendered_tool_body(rendered: &str) -> Vec<String> {
    rendered
        .lines()
        .skip(1)
        .take_while(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with('⠴')
                && !trimmed.starts_with('⠸')
                && !trimmed.starts_with('⠼')
                && !trimmed.starts_with('┌')
        })
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

#[test]
fn live_question_artifact_renders_outcome_from_raw_metadata() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    app.set_plugin_host(Arc::new(question_plugin_host()));
    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call-question".to_owned(),
            result: "question answered".to_owned(),
            is_error: false,
            output: None,
            semantic_result: Some(ToolInvocationResult::Artifact {
                artifact: Box::new(question_outcome_artifact()),
            }),
        },
    ));

    let rendered = render_app_text(&mut app);

    assert!(rendered.contains("Question outcome"), "{rendered}");
    assert!(rendered.contains("Proceed?"), "{rendered}");
    assert!(rendered.contains("✓ Yes"), "{rendered}");
    assert!(!rendered.contains("bcode.question.outcome"), "{rendered}");
}

#[test]
fn replayed_question_artifact_renders_outcome_from_raw_metadata() {
    let session_id = SessionId::new();
    let events = vec![event(
        session_id,
        1,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call-question".to_owned(),
            result: "question answered".to_owned(),
            is_error: false,
            output: None,
            semantic_result: Some(ToolInvocationResult::Artifact {
                artifact: Box::new(question_outcome_artifact()),
            }),
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &events, &[], false);
    app.set_plugin_host(Arc::new(question_plugin_host()));

    let rendered = render_app_text(&mut app);

    assert!(rendered.contains("Question outcome"), "{rendered}");
    assert!(rendered.contains("Proceed?"), "{rendered}");
    assert!(rendered.contains("✓ Yes"), "{rendered}");
    assert!(!rendered.contains("bcode.question.outcome"), "{rendered}");
}

#[test]
fn live_shell_artifact_renders_terminal_output_from_raw_run_metadata() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    app.set_plugin_host(Arc::new(shell_plugin_host()));
    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call-shell".to_owned(),
            result: "shell completed".to_owned(),
            is_error: false,
            output: None,
            semantic_result: Some(ToolInvocationResult::Artifact {
                artifact: Box::new(shell_run_artifact()),
            }),
        },
    ));

    let rendered = render_app_text(&mut app);

    assert!(rendered.contains("Terminal · shell.run"), "{rendered}");
    assert!(rendered.contains("completed"), "{rendered}");
    assert!(rendered.contains("shell raw output"), "{rendered}");
    assert!(!rendered.contains("bcode.shell.run"), "{rendered}");
}

#[test]
fn replayed_shell_artifact_renders_terminal_output_from_raw_run_metadata() {
    let session_id = SessionId::new();
    let events = vec![event(
        session_id,
        1,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call-shell".to_owned(),
            result: "shell completed".to_owned(),
            is_error: false,
            output: None,
            semantic_result: Some(ToolInvocationResult::Artifact {
                artifact: Box::new(shell_run_artifact()),
            }),
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &events, &[], false);
    app.set_plugin_host(Arc::new(shell_plugin_host()));

    let rendered = render_app_text(&mut app);

    assert!(rendered.contains("Terminal · shell.run"), "{rendered}");
    assert!(rendered.contains("completed"), "{rendered}");
    assert!(rendered.contains("shell raw output"), "{rendered}");
    assert!(!rendered.contains("bcode.shell.run"), "{rendered}");
}

#[test]
fn replayed_shell_artifact_renders_terminal_replay_ref_through_terminal_grid() {
    let session_id = SessionId::new();
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let pty_path = temp_dir.path().join("raw-pty.txt");
    std::fs::write(&pty_path, "first\rsecond\n").expect("write pty artifact");
    let mut artifact = shell_run_artifact();
    artifact.metadata["output_tail"] =
        serde_json::Value::String("artifact raw fallback\n".to_owned());
    artifact.refs.push(ToolArtifactRef {
        key: "terminal_pty_stream".to_owned(),
        content_type: Some("application/x-bcode-terminal-pty-stream; charset=utf-8".to_owned()),
        storage_uri: Some(format!("file://{}", pty_path.to_string_lossy())),
        byte_len: Some(13),
        metadata: Some(serde_json::json!({
            "stream": "pty",
            "columns": 80,
            "rows": 24,
            "retained_tail_bytes": 13,
            "tail_truncated": false,
        })),
    });
    let events = vec![event(
        session_id,
        1,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call-shell".to_owned(),
            result: "shell completed".to_owned(),
            is_error: false,
            output: None,
            semantic_result: Some(ToolInvocationResult::Artifact {
                artifact: Box::new(artifact),
            }),
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &events, &[], false);
    app.set_plugin_host(Arc::new(shell_plugin_host()));

    let rendered = render_app_text(&mut app);

    assert!(rendered.contains("second"), "{rendered}");
    assert!(!rendered.contains("first"), "{rendered}");
    assert!(!rendered.contains("artifact raw fallback"), "{rendered}");
}

#[test]
fn replayed_shell_run_prefers_raw_pty_stream_over_shell_artifact_metadata() {
    let session_id = SessionId::new();
    let mut artifact = shell_run_artifact();
    artifact.metadata["output_tail"] =
        serde_json::Value::String("artifact raw fallback\n".to_owned());
    let events = vec![
        event(
            session_id,
            0,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-shell".to_owned(),
                producer_plugin_id: Some("bcode.shell".to_owned()),
                tool_name: "shell.run".to_owned(),
                arguments_json: r#"{"command":"printf"}"#.to_owned(),
                legacy_request_presentation: None,
            },
        ),
        event(
            session_id,
            1,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::Started {
                    tool_call_id: "call-shell".to_owned(),
                    tool_name: "shell.run".to_owned(),
                    sequence: 0,
                    terminal: true,
                    columns: Some(80),
                    rows: Some(24),
                    started_at_ms: Some(1),
                },
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::OutputDelta {
                    tool_call_id: "call-shell".to_owned(),
                    stream: ToolOutputStream::Pty,
                    sequence: 1,
                    text: "first\rsecond\n".to_owned(),
                    byte_len: 13,
                },
            },
        ),
        event(
            session_id,
            3,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::Finished {
                    tool_call_id: "call-shell".to_owned(),
                    sequence: 2,
                    is_error: false,
                    finished_at_ms: Some(3),
                },
            },
        ),
        event(
            session_id,
            4,
            SessionEventKind::ToolCallFinished {
                tool_call_id: "call-shell".to_owned(),
                result: "shell completed".to_owned(),
                is_error: false,
                output: None,
                semantic_result: Some(ToolInvocationResult::Artifact {
                    artifact: Box::new(artifact),
                }),
            },
        ),
    ];
    let mut app = BmuxApp::new_with_history(Some(session_id), &events, &[], false);
    app.set_plugin_host(Arc::new(shell_plugin_host()));

    let rendered = render_app_text(&mut app);

    assert!(rendered.contains("second"), "{rendered}");
    assert!(!rendered.contains("first"), "{rendered}");
    assert!(!rendered.contains("artifact raw fallback"), "{rendered}");
}

#[test]
fn filesystem_write_request_preview_renders_from_raw_arguments_without_metadata() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    app.set_plugin_host(Arc::new(filesystem_plugin_host()));
    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "call-write".to_owned(),
            producer_plugin_id: Some("bcode.filesystem".to_owned()),
            tool_name: "filesystem.write".to_owned(),
            arguments_json: serde_json::json!({
                "path": "/tmp/raw-preview.txt",
                "contents": "created from raw args\n"
            })
            .to_string(),
            legacy_request_presentation: None,
        },
    ));

    let rendered = render_app_text(&mut app);

    assert!(rendered.contains("/tmp/raw-preview.txt"), "{rendered}");
    assert!(rendered.contains("created from raw args"), "{rendered}");
    assert!(!rendered.contains("arguments"), "{rendered}");
}

#[test]
fn filesystem_edit_request_preview_renders_from_raw_arguments_without_metadata() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    app.set_plugin_host(Arc::new(filesystem_plugin_host()));
    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "call-edit".to_owned(),
            producer_plugin_id: Some("bcode.filesystem".to_owned()),
            tool_name: "filesystem.edit".to_owned(),
            arguments_json: serde_json::json!({
                "path": "/tmp/raw-edit.txt",
                "old_text": "old raw args\n",
                "new_text": "new raw args\n"
            })
            .to_string(),
            legacy_request_presentation: None,
        },
    ));

    let rendered = render_app_text(&mut app);

    assert!(rendered.contains("/tmp/raw-edit.txt"), "{rendered}");
    assert!(rendered.contains("old raw args"), "{rendered}");
    assert!(rendered.contains("new raw args"), "{rendered}");
    assert!(!rendered.contains("arguments"), "{rendered}");
}

#[test]
fn same_raw_filesystem_events_render_same_live_and_replayed_tool_ui() {
    let session_id = SessionId::new();
    let events = vec![
        event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-file".to_owned(),
                producer_plugin_id: Some("bcode.filesystem".to_owned()),
                tool_name: "filesystem.edit".to_owned(),
                arguments_json: serde_json::json!({
                    "path": "/tmp/hello.txt",
                    "old_text": "before\n",
                    "new_text": "after\n"
                })
                .to_string(),
                legacy_request_presentation: None,
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::ToolCallFinished {
                tool_call_id: "call-file".to_owned(),
                result: "edited file".to_owned(),
                is_error: false,
                output: None,
                semantic_result: Some(ToolInvocationResult::Artifact {
                    artifact: Box::new(filesystem_change_artifact()),
                }),
            },
        ),
    ];

    let plugin_host = Arc::new(filesystem_plugin_host());
    let mut live_app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    live_app.set_plugin_host(Arc::clone(&plugin_host));
    for event in &events {
        live_app.absorb_session_event(event);
    }
    let mut replayed_app = BmuxApp::new_with_history(Some(session_id), &events, &[], false);
    replayed_app.set_plugin_host(plugin_host);

    let live_rendered = render_app_text(&mut live_app);
    let replayed_rendered = render_app_text(&mut replayed_app);

    assert_eq!(
        rendered_tool_body(&live_rendered),
        rendered_tool_body(&replayed_rendered),
        "live:\n{live_rendered}\n\nreplayed:\n{replayed_rendered}"
    );
}

#[test]
fn live_filesystem_artifact_renders_rich_diff_from_raw_change_metadata() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    app.set_plugin_host(Arc::new(filesystem_plugin_host()));
    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "call-file".to_owned(),
            producer_plugin_id: Some("bcode.filesystem".to_owned()),
            tool_name: "filesystem.edit".to_owned(),
            arguments_json: serde_json::json!({
                "path": "/tmp/hello.txt",
                "old_text": "before\n",
                "new_text": "after\n"
            })
            .to_string(),
            legacy_request_presentation: None,
        },
    ));
    app.absorb_session_event(&event(
        session_id,
        2,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call-file".to_owned(),
            result: "edited file".to_owned(),
            is_error: false,
            output: None,
            semantic_result: Some(ToolInvocationResult::Artifact {
                artifact: Box::new(filesystem_change_artifact()),
            }),
        },
    ));

    let rendered = render_app_text(&mut app);

    assert!(rendered.contains("/tmp/hello.txt"), "{rendered}");
    assert!(rendered.contains("before"), "{rendered}");
    assert!(rendered.contains("after"), "{rendered}");
    assert!(!rendered.contains("bcode.filesystem.change"), "{rendered}");
}

#[test]
fn final_filesystem_artifact_supersedes_matching_live_plugin_preview() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    app.set_plugin_host(Arc::new(filesystem_plugin_host()));
    app.absorb_session_live_event(&bcode_session_models::SessionLiveEvent {
        session_id,
        kind: bcode_session_models::SessionLiveEventKind::ToolArgumentPreview {
            turn_id: "turn-1".to_owned(),
            tool_call_id: "call-file".to_owned(),
            tool_name: "filesystem.write".to_owned(),
            argument_bytes: 42,
            preview: LiveToolArgumentPreview::PluginView(LivePluginViewPreview {
                producer_plugin_id: "bcode.filesystem".to_owned(),
                schema: "bcode.filesystem.change".to_owned(),
                schema_version: 1,
                title: Some("Writing file".to_owned()),
                subtitle: Some("Writing file".to_owned()),
                payload: serde_json::json!({
                    "subtitle": "Writing file",
                    "path": "/tmp/hello.txt",
                    "old_text": "before\n",
                    "new_text": "after\n",
                    "truncated": false
                }),
            }),
        },
    });
    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call-file".to_owned(),
            result: "edited file".to_owned(),
            is_error: false,
            output: None,
            semantic_result: Some(ToolInvocationResult::Artifact {
                artifact: Box::new(filesystem_change_artifact()),
            }),
        },
    ));

    let rendered = render_app_text(&mut app);

    assert!(rendered.contains("/tmp/hello.txt"), "{rendered}");
    assert!(rendered.contains("after"), "{rendered}");
    assert!(!rendered.contains("Streaming preview"), "{rendered}");
    assert!(rendered.contains("File change"), "{rendered}");
}

#[test]
fn streamed_filesystem_json_is_replaced_by_final_rich_artifact() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    app.set_plugin_host(Arc::new(filesystem_plugin_host()));
    app.absorb_session_live_event(&bcode_session_models::SessionLiveEvent {
        session_id,
        kind: bcode_session_models::SessionLiveEventKind::ToolArgumentPreview {
            turn_id: "turn-1".to_owned(),
            tool_call_id: "call-file".to_owned(),
            tool_name: "filesystem.write".to_owned(),
            argument_bytes: 42,
            preview: LiveToolArgumentPreview::PluginView(LivePluginViewPreview {
                producer_plugin_id: "bcode.filesystem".to_owned(),
                schema: "bcode.filesystem.change".to_owned(),
                schema_version: 1,
                title: Some("Writing file".to_owned()),
                subtitle: Some("Writing file".to_owned()),
                payload: serde_json::json!({
                    "subtitle": "Writing file",
                    "path": "/tmp/hello.txt",
                    "old_text": "before\n",
                    "new_text": "after\n",
                    "truncated": false
                }),
            }),
        },
    });
    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::ToolInvocationStream {
            event: ToolInvocationStreamEvent::OutputDelta {
                tool_call_id: "call-file".to_owned(),
                stream: ToolOutputStream::Stdout,
                sequence: 1,
                text: r#"{"path":"/tmp/hello.txt","new_text":"after"}"#.to_owned(),
                byte_len: 44,
            },
        },
    ));
    app.absorb_session_event(&event(
        session_id,
        2,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call-file".to_owned(),
            result: "edited file".to_owned(),
            is_error: false,
            output: None,
            semantic_result: Some(ToolInvocationResult::Artifact {
                artifact: Box::new(filesystem_change_artifact()),
            }),
        },
    ));

    let rendered = render_app_text(&mut app);

    assert!(rendered.contains("/tmp/hello.txt"), "{rendered}");
    assert!(rendered.contains("after"), "{rendered}");
    assert!(rendered.contains("File change"), "{rendered}");
    assert!(!rendered.contains(r#"{"path":"#), "{rendered}");
    assert!(!rendered.contains("Streaming preview"), "{rendered}");
}

#[test]
fn shell_live_preview_is_not_superseded_by_terminal_result() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    app.set_plugin_host(Arc::new(shell_plugin_host()));
    app.absorb_session_live_event(&bcode_session_models::SessionLiveEvent {
        session_id,
        kind: bcode_session_models::SessionLiveEventKind::ToolArgumentPreview {
            turn_id: "turn-1".to_owned(),
            tool_call_id: "call-shell".to_owned(),
            tool_name: "shell.run".to_owned(),
            argument_bytes: 24,
            preview: LiveToolArgumentPreview::ShellCommand(LiveShellCommandPreview {
                preview_title: None,
                streaming_status: None,
                command_prefix: "cargo test".to_owned(),
                cwd: Some("/tmp/project".to_owned()),
                argument_bytes: 24,
                truncated: false,
            }),
        },
    });
    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call-shell".to_owned(),
            result: "finished".to_owned(),
            is_error: false,
            output: None,
            semantic_result: Some(ToolInvocationResult::Artifact {
                artifact: Box::new(shell_run_artifact()),
            }),
        },
    ));

    let rendered = render_app_text(&mut app);

    assert!(rendered.contains("Shell command"), "{rendered}");
    assert!(rendered.contains("command: cargo test"), "{rendered}");
    assert!(rendered.contains("Terminal · shell.run"), "{rendered}");
    assert!(rendered.contains("completed"), "{rendered}");
    assert!(rendered.contains("shell raw output"), "{rendered}");
}

#[test]
fn query_live_preview_is_not_superseded_by_unmatched_result() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    let mut fields = BTreeMap::new();
    fields.insert("query".to_owned(), "rust borrow checker".to_owned());
    app.absorb_session_live_event(&bcode_session_models::SessionLiveEvent {
        session_id,
        kind: bcode_session_models::SessionLiveEventKind::ToolArgumentPreview {
            turn_id: "turn-1".to_owned(),
            tool_call_id: "call-query".to_owned(),
            tool_name: "web.search".to_owned(),
            argument_bytes: 32,
            preview: LiveToolArgumentPreview::Query(LiveQueryPreview {
                preview_title: Some("Search query".to_owned()),
                streaming_status: None,
                fields,
                argument_bytes: 32,
                truncated: false,
            }),
        },
    });
    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call-query".to_owned(),
            result: "search completed".to_owned(),
            is_error: false,
            output: None,
            semantic_result: None,
        },
    ));

    let rendered = render_app_text(&mut app);

    assert!(rendered.contains("Search query"), "{rendered}");
    assert!(
        rendered.contains("query: rust borrow checker"),
        "{rendered}"
    );
    assert!(rendered.contains("Tool result"), "{rendered}");
    assert!(rendered.contains("search completed"), "{rendered}");
}

#[test]
fn replayed_filesystem_artifact_renders_rich_diff_from_raw_change_metadata() {
    let session_id = SessionId::new();
    let events = vec![event(
        session_id,
        1,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call-file".to_owned(),
            result: "edited file".to_owned(),
            is_error: false,
            output: None,
            semantic_result: Some(ToolInvocationResult::Artifact {
                artifact: Box::new(filesystem_change_artifact()),
            }),
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &events, &[], false);
    app.set_plugin_host(Arc::new(filesystem_plugin_host()));

    let rendered = render_app_text(&mut app);

    assert!(rendered.contains("/tmp/hello.txt"), "{rendered}");
    assert!(rendered.contains("before"), "{rendered}");
    assert!(rendered.contains("after"), "{rendered}");
    assert!(!rendered.contains("bcode.filesystem.change"), "{rendered}");
}

#[test]
fn disabled_filesystem_renderer_falls_back_generically_and_reenabled_renderer_restores_rich_display()
 {
    let session_id = SessionId::new();
    let events = vec![event(
        session_id,
        1,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call-file".to_owned(),
            result: "edited file".to_owned(),
            is_error: false,
            output: None,
            semantic_result: Some(ToolInvocationResult::Artifact {
                artifact: Box::new(filesystem_change_artifact()),
            }),
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &events, &[], false);
    let before = app.tool_invocation_projections().clone();

    let fallback_rendered = render_app_text(&mut app);

    assert!(
        fallback_rendered.contains("File change"),
        "{fallback_rendered}"
    );
    assert!(
        fallback_rendered.contains("plugin host unavailable"),
        "{fallback_rendered}"
    );
    assert!(
        fallback_rendered.contains("edited file"),
        "{fallback_rendered}"
    );
    assert!(!fallback_rendered.contains("before"), "{fallback_rendered}");
    assert!(!fallback_rendered.contains("after"), "{fallback_rendered}");
    assert_eq!(&before, app.tool_invocation_projections());

    app.set_plugin_host(Arc::new(filesystem_plugin_host()));
    let rich_rendered = render_app_text(&mut app);

    assert!(rich_rendered.contains("/tmp/hello.txt"), "{rich_rendered}");
    assert!(rich_rendered.contains("before"), "{rich_rendered}");
    assert!(rich_rendered.contains("after"), "{rich_rendered}");
    assert!(
        !rich_rendered.contains("bcode.filesystem.change"),
        "{rich_rendered}"
    );
    assert_eq!(&before, app.tool_invocation_projections());
}

#[test]
fn legacy_serialized_artifact_result_does_not_render_raw_json() {
    let session_id = SessionId::new();
    let artifact = bcode_session_models::ToolArtifact {
        artifact_id: "call-file-filesystem-change".to_string(),
        producer_plugin_id: "bcode.filesystem".to_string(),
        schema: "bcode.filesystem.change".to_string(),
        schema_version: 1,
        tool_call_id: Some("call-file".to_string()),
        title: Some("File change".to_string()),
        metadata: serde_json::json!({
            "summary": "wrote 12 bytes",
            "path": "/tmp/hello.txt",
            "old_text": "",
            "new_text": "hello world\n"
        }),
        refs: Vec::new(),
    };
    let events = vec![event(
        session_id,
        1,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call-file".to_owned(),
            result: serde_json::to_string(&artifact).expect("artifact should serialize"),
            is_error: false,
            output: None,
            semantic_result: None,
        },
    )];

    let transcript = transcript_items_from_events_with_reasoning(&events, true);
    let item = transcript
        .iter()
        .find(|item| matches!(item.kind(), TranscriptItemKind::ToolResult { .. }))
        .expect("tool result should render");

    assert!(item.text().contains("File change"));
    assert!(item.text().contains("wrote 12 bytes"));
    assert!(item.text().contains("/tmp/hello.txt"));
    assert!(!item.text().contains("artifact_id"));
    assert!(!item.text().contains("producer_plugin_id"));
    assert!(!item.text().contains("metadata"));
}

#[test]
fn legacy_serialized_semantic_artifact_result_does_not_render_raw_json() {
    let session_id = SessionId::new();
    let result = ToolInvocationResult::Artifact {
        artifact: Box::new(bcode_session_models::ToolArtifact {
            artifact_id: "call-file-filesystem-change".to_string(),
            producer_plugin_id: "bcode.filesystem".to_string(),
            schema: "bcode.filesystem.change".to_string(),
            schema_version: 1,
            tool_call_id: Some("call-file".to_string()),
            title: Some("File change".to_string()),
            metadata: serde_json::json!({
                "summary": "edited file",
                "path": "/tmp/hello.txt",
                "old_text": "hello",
                "new_text": "hello world"
            }),
            refs: Vec::new(),
        }),
    };
    let events = vec![event(
        session_id,
        1,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call-file".to_owned(),
            result: serde_json::to_string(&result).expect("semantic result should serialize"),
            is_error: false,
            output: None,
            semantic_result: None,
        },
    )];

    let transcript = transcript_items_from_events_with_reasoning(&events, true);
    let item = transcript
        .iter()
        .find(|item| matches!(item.kind(), TranscriptItemKind::ToolResult { .. }))
        .expect("tool result should render");

    assert!(item.text().contains("File change"));
    assert!(item.text().contains("edited file"));
    assert!(item.text().contains("/tmp/hello.txt"));
    assert!(!item.text().contains("artifact_id"));
    assert!(!item.text().contains("producer_plugin_id"));
    assert!(!item.text().contains("metadata"));
}

#[test]
fn generic_artifact_fallback_projection_is_repeatable_and_non_mutating() {
    let session_id = SessionId::new();
    let events = vec![event(
        session_id,
        1,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call-artifact".to_owned(),
            result: "fallback".to_owned(),
            is_error: false,
            output: None,
            semantic_result: Some(ToolInvocationResult::Artifact {
                artifact: Box::new(bcode_session_models::ToolArtifact {
                    artifact_id: "artifact-1".to_owned(),
                    producer_plugin_id: "test.plugin".to_owned(),
                    schema: "test.schema".to_owned(),
                    schema_version: 1,
                    tool_call_id: Some("call-artifact".to_owned()),
                    title: Some("Test artifact".to_owned()),
                    metadata: serde_json::json!({"path": "/tmp/example.txt"}),
                    refs: Vec::new(),
                }),
            }),
        },
    )];
    let original_events = events.clone();

    let first = transcript_items_from_events_with_reasoning(&events, true);
    let second = transcript_items_from_events_with_reasoning(&events, true);

    assert_eq!(events, original_events);
    assert_eq!(first.len(), second.len());
    assert_eq!(first[0].text(), second[0].text());
    assert_eq!(first[0].kind(), second[0].kind());
    assert!(first.iter().any(|item| matches!(
        item.kind(),
        TranscriptItemKind::ToolResult { result, .. }
            if result.contains("Test artifact") && result.contains("test.schema")
    )));
}

#[test]
fn live_tool_invocation_projection_matches_replayed_projection() {
    let session_id = SessionId::new();
    let events = vec![
        event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-1".to_owned(),
                producer_plugin_id: Some("plugin.shell".to_owned()),
                tool_name: "shell.run".to_owned(),
                arguments_json: r#"{"command":"echo hi"}"#.to_owned(),
                legacy_request_presentation: None,
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::Started {
                    tool_call_id: "call-1".to_owned(),
                    tool_name: "shell.run".to_owned(),
                    sequence: 0,
                    terminal: true,
                    columns: Some(120),
                    rows: Some(30),
                    started_at_ms: Some(10),
                },
            },
        ),
        event(
            session_id,
            3,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::OutputDelta {
                    tool_call_id: "call-1".to_owned(),
                    stream: ToolOutputStream::Stdout,
                    sequence: 1,
                    text: "hi\n".to_owned(),
                    byte_len: 3,
                },
            },
        ),
        event(
            session_id,
            4,
            SessionEventKind::ToolCallFinished {
                tool_call_id: "call-1".to_owned(),
                result: "final text".to_owned(),
                is_error: false,
                output: None,
                semantic_result: Some(ToolInvocationResult::Text {
                    text: "semantic text".to_owned(),
                }),
            },
        ),
    ];
    let replay_projection = build_tool_invocation_projections(&events);
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);

    for event in &events {
        app.absorb_session_event(event);
    }
    let live_projection = app
        .tool_invocation_projections()
        .values()
        .cloned()
        .collect::<Vec<_>>();

    assert_eq!(live_projection, replay_projection);
}

fn shell_result_artifact(result: &ShellRunResult) -> ToolInvocationResult {
    ToolInvocationResult::Artifact {
        artifact: Box::new(bcode_session_models::ToolArtifact {
            artifact_id: "test-shell-run".to_string(),
            producer_plugin_id: "test.shell".to_string(),
            schema: "test.shell-artifact".to_string(),
            schema_version: 1,
            tool_call_id: None,
            title: Some("Shell run".to_string()),
            metadata: serde_json::to_value(result).expect("shell result should serialize"),
            refs: Vec::new(),
        }),
    }
}

fn file_change_artifact(result: &bcode_session_models::FileChangeResult) -> ToolInvocationResult {
    ToolInvocationResult::Artifact {
        artifact: Box::new(bcode_session_models::ToolArtifact {
            artifact_id: "test-file-change".to_string(),
            producer_plugin_id: "test.filesystem".to_string(),
            schema: "test.file-change-artifact".to_string(),
            schema_version: 1,
            tool_call_id: None,
            title: Some("File change".to_string()),
            metadata: serde_json::to_value(result).expect("file change should serialize"),
            refs: Vec::new(),
        }),
    }
}
fn event(session_id: SessionId, sequence: u64, kind: SessionEventKind) -> SessionEvent {
    SessionEvent {
        schema_version: 1,
        sequence,
        timestamp_ms: 1,
        session_id,
        provenance: None,
        kind,
    }
}

fn key(key: KeyCode) -> KeyStroke {
    KeyStroke {
        key,
        modifiers: Modifiers::NONE,
    }
}

fn shift_key(key: KeyCode) -> KeyStroke {
    KeyStroke {
        key,
        modifiers: Modifiers {
            shift: true,
            ..Modifiers::NONE
        },
    }
}

fn ctrl_key(ch: char) -> KeyStroke {
    ctrl_key_code(KeyCode::Char(ch))
}

fn alt_char(ch: char) -> KeyStroke {
    KeyStroke {
        key: KeyCode::Char(ch),
        modifiers: Modifiers {
            alt: true,
            ..Modifiers::NONE
        },
    }
}

fn ctrl_key_code(key: KeyCode) -> KeyStroke {
    KeyStroke {
        key,
        modifiers: Modifiers {
            ctrl: true,
            ..Modifiers::NONE
        },
    }
}

fn mouse(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
    MouseEvent::new(kind, Point::new(x, y))
}

fn output_line_y(buffer: &Buffer, needle: &str) -> Option<u16> {
    (0..buffer.area().height).find(|row| {
        buffer
            .row_symbols(*row)
            .is_some_and(|line| line.contains(needle))
    })
}

fn rendered_text(buffer: &Buffer) -> String {
    (0..buffer.area().height)
        .filter_map(|row| buffer.row_symbols(row))
        .collect::<Vec<_>>()
        .join("\n")
}

fn agent_infos(items: &[(&str, bool)]) -> Vec<AgentInfo> {
    agent_infos_with_accents(
        &items
            .iter()
            .map(|(id, is_default)| (*id, *is_default, None))
            .collect::<Vec<_>>(),
    )
}

fn agent_infos_with_accents(items: &[(&str, bool, Option<&str>)]) -> Vec<AgentInfo> {
    items
        .iter()
        .map(|(id, is_default, accent)| AgentInfo {
            id: (*id).to_owned(),
            name: (*id).to_owned(),
            description: String::new(),
            badge: None,
            accent: accent.map(ToOwned::to_owned),
            aliases: Vec::new(),
            is_default: *is_default,
        })
        .collect()
}

#[test]
fn thinking_label_uses_effective_values() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    app.apply_model_status(bcode_ipc::SessionModelStatus {
        provider_plugin_id: None,
        model_id: None,
        context_window: None,
        max_output_tokens: None,
        reasoning: Some(bcode_model::ModelReasoningInfo {
            effort_values: vec!["low".to_owned(), "medium".to_owned(), "high".to_owned()],
            default_effort: Some("medium".to_owned()),
            visible_summary_supported: true,
            summary_values: vec!["auto".to_owned(), "detailed".to_owned()],
            default_summary: Some("auto".to_owned()),
            raw_reasoning_supported: false,
            source: bcode_model::ModelReasoningCapabilitySource::KnownModelTable,
        }),
        reasoning_effort: None,
        reasoning_summary: Some("detailed".to_owned()),
        prompt_cache_mode: None,
        conversation_reuse_mode: None,
        compaction_mode: None,
        cache: None,
        metadata_source: None,
        pricing: None,
    });
    app.set_reasoning_visible(true);

    assert_eq!(
        app.thinking_label(),
        "reasoning output shown · effort: medium · visible summary: detailed"
    );
    assert_eq!(app.model_header_label(), "default [medium]");
}

#[test]
fn thinking_dialog_cycles_supported_values() {
    let status = bcode_ipc::SessionModelStatus {
        provider_plugin_id: None,
        model_id: None,
        context_window: None,
        max_output_tokens: None,
        reasoning: Some(bcode_model::ModelReasoningInfo {
            effort_values: vec!["low".to_owned(), "medium".to_owned()],
            default_effort: Some("low".to_owned()),
            visible_summary_supported: true,
            summary_values: vec!["auto".to_owned(), "detailed".to_owned()],
            default_summary: Some("auto".to_owned()),
            raw_reasoning_supported: false,
            source: bcode_model::ModelReasoningCapabilitySource::KnownModelTable,
        }),
        reasoning_effort: Some("low".to_owned()),
        reasoning_summary: Some("auto".to_owned()),
        prompt_cache_mode: None,
        conversation_reuse_mode: None,
        compaction_mode: None,
        cache: None,
        metadata_source: None,
        pricing: None,
    };
    let mut dialog = super::thinking_dialog::ThinkingDialogState::new(false, &status);

    dialog.cycle_focused();
    assert!(dialog.visible());
    dialog.focus_next();
    dialog.cycle_focused();
    assert_eq!(dialog.effort(), Some("medium"));
    dialog.focus_next();
    dialog.cycle_focused();
    assert_eq!(dialog.summary(), Some("detailed"));
}

#[test]
fn thinking_dialog_can_start_focused_on_effort_or_summary() {
    let status = bcode_ipc::SessionModelStatus {
        provider_plugin_id: None,
        model_id: None,
        context_window: None,
        max_output_tokens: None,
        reasoning: Some(bcode_model::ModelReasoningInfo {
            effort_values: vec!["low".to_owned(), "medium".to_owned()],
            default_effort: Some("low".to_owned()),
            visible_summary_supported: true,
            summary_values: vec!["auto".to_owned(), "detailed".to_owned()],
            default_summary: Some("auto".to_owned()),
            raw_reasoning_supported: false,
            source: bcode_model::ModelReasoningCapabilitySource::KnownModelTable,
        }),
        reasoning_effort: Some("low".to_owned()),
        reasoning_summary: Some("auto".to_owned()),
        prompt_cache_mode: None,
        conversation_reuse_mode: None,
        compaction_mode: None,
        cache: None,
        metadata_source: None,
        pricing: None,
    };

    let effort = super::thinking_dialog::ThinkingDialogState::new_focused(
        false,
        &status,
        super::thinking_dialog::ThinkingDialogFocus::Effort,
    );
    let summary = super::thinking_dialog::ThinkingDialogState::new_focused(
        false,
        &status,
        super::thinking_dialog::ThinkingDialogFocus::Summary,
    );

    assert_eq!(effort.focused_row(), 1);
    assert_eq!(summary.focused_row(), 2);
}

#[test]
fn thinking_dialog_does_not_cycle_when_reasoning_is_unsupported() {
    let status = bcode_ipc::SessionModelStatus {
        provider_plugin_id: None,
        model_id: None,
        context_window: None,
        max_output_tokens: None,
        reasoning: None,
        reasoning_effort: None,
        reasoning_summary: None,
        prompt_cache_mode: None,
        conversation_reuse_mode: None,
        compaction_mode: None,
        cache: None,
        metadata_source: None,
        pricing: None,
    };
    let mut dialog = super::thinking_dialog::ThinkingDialogState::new(false, &status);

    dialog.focus_next();
    dialog.cycle_focused();
    assert_eq!(dialog.effort(), None);
    assert!(dialog.effort_values().is_empty());
    dialog.focus_next();
    dialog.cycle_focused();
    assert_eq!(dialog.summary(), None);
    assert!(dialog.summary_values().is_empty());
}

#[test]
fn plugin_view_result_presentation_is_ignored_in_normal_live_path() {
    let session_id = SessionId::new();
    let events = vec![event(
        session_id,
        1,
        SessionEventKind::ToolInvocationStream {
            event: ToolInvocationStreamEvent::LegacyPresentation {
                tool_call_id: "call-view".to_owned(),
                sequence: 1,
                presentation: LegacyToolPresentationEvent::PluginView(
                    LegacyToolPluginViewPresentation {
                        target: LegacyToolPresentationTarget::Result,
                        producer_plugin_id: "plugin".to_owned(),
                        schema: "legacy.view".to_owned(),
                        schema_version: 1,
                        title: Some("Legacy view".to_owned()),
                        subtitle: None,
                        payload: serde_json::json!({"raw": true}),
                    },
                ),
            },
        },
    )];
    let transcript = transcript_items_from_events_with_reasoning(&events, true);

    assert!(
        !transcript
            .iter()
            .any(|item| item.text().contains("Legacy view"))
    );
}

#[test]
fn replayed_plugin_view_result_does_not_suppress_artifact_json_fallback() {
    let session_id = SessionId::new();
    let events = vec![
        event(
            session_id,
            1,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::LegacyPresentation {
                    tool_call_id: "call-view".to_owned(),
                    sequence: 1,
                    presentation: LegacyToolPresentationEvent::PluginView(
                        LegacyToolPluginViewPresentation {
                            target: LegacyToolPresentationTarget::Result,
                            producer_plugin_id: "plugin".to_owned(),
                            schema: "legacy.view".to_owned(),
                            schema_version: 1,
                            title: Some("Legacy view".to_owned()),
                            subtitle: None,
                            payload: serde_json::json!({"raw": true}),
                        },
                    ),
                },
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::ToolCallFinished {
                tool_call_id: "call-view".to_owned(),
                result: "fallback".to_owned(),
                is_error: false,
                output: None,
                semantic_result: Some(ToolInvocationResult::Artifact {
                    artifact: Box::new(ToolArtifact {
                        artifact_id: "artifact-1".to_owned(),
                        producer_plugin_id: "filesystem".to_owned(),
                        schema: "bcode.filesystem.change".to_owned(),
                        schema_version: 1,
                        tool_call_id: Some("call-view".to_owned()),
                        title: Some("Applied file change".to_owned()),
                        metadata: serde_json::json!({"path": "/tmp/main.rs"}),
                        refs: Vec::new(),
                    }),
                }),
            },
        ),
    ];

    let transcript = transcript_items_from_events_with_reasoning(&events, true);

    assert!(
        !transcript
            .iter()
            .any(|item| item.text().contains("Legacy view"))
    );
    assert!(transcript.iter().any(|item| matches!(
        item.kind(),
        TranscriptItemKind::ToolResult { result, .. } if result.contains("bcode.filesystem.change")
    )));
}

#[test]
fn plugin_visual_degrades_without_rendering_raw_payload() {
    let artifact = ToolArtifact {
        artifact_id: "artifact-1".to_owned(),
        producer_plugin_id: "bcode.filesystem".to_owned(),
        schema: "bcode.filesystem.change".to_owned(),
        schema_version: 1,
        tool_call_id: Some("call-1".to_owned()),
        title: Some("File change".to_owned()),
        metadata: serde_json::json!({
            "path": "src/main.rs",
            "old_text": "fn main() {}",
            "new_text": "fn main() { println!(\"hi\"); }"
        }),
        refs: Vec::new(),
    };
    let item = super::transcript::semantic_tool_result_item_from_raw(
        "call-1",
        None,
        None,
        &ToolInvocationResult::Artifact {
            artifact: Box::new(artifact),
        },
        false,
    );

    let rows = render::transcript_item_rows(&[item], &BTreeMap::new(), 0, 90, None);
    let output = format!("{rows:?}");

    assert!(output.contains("File change"), "{output}");
    assert!(output.contains("plugin host unavailable"), "{output}");
    assert!(!output.contains("old_text"), "{output}");
    assert!(!output.contains("new_text"), "{output}");
    assert!(!output.contains("fn main() {}"), "{output}");
}

#[test]
fn live_shell_command_preview_streams_before_final_request_and_is_preserved() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    app.absorb_session_live_event(&bcode_session_models::SessionLiveEvent {
        session_id,
        kind: bcode_session_models::SessionLiveEventKind::ToolArgumentPreview {
            turn_id: "turn-1".to_owned(),
            tool_call_id: "call_shell".to_owned(),
            tool_name: "shell_run".to_owned(),
            argument_bytes: 36,
            preview: LiveToolArgumentPreview::ShellCommand(LiveShellCommandPreview {
                preview_title: None,
                streaming_status: None,
                command_prefix: "cargo test -p bcode_tui".to_owned(),
                cwd: Some("/repo".to_owned()),
                argument_bytes: 36,
                truncated: false,
            }),
        },
    });

    let mut buffer = Buffer::empty(Rect::new(0, 0, 90, 20));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);
    assert!(output.contains("Shell command"), "{output}");
    assert!(
        output.contains("command: cargo test -p bcode_tui"),
        "{output}"
    );
    assert!(output.contains("cwd: /repo"), "{output}");

    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "call_shell".to_owned(),
            producer_plugin_id: None,
            tool_name: "shell_run".to_owned(),
            arguments_json: serde_json::json!({
                "command": "cargo test -p bcode_tui",
                "cwd": "/repo",
            })
            .to_string(),
            legacy_request_presentation: Some(shell_legacy_request_presentation()),
        },
    ));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 90, 20));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);
    assert!(output.contains("Shell command"), "{output}");
    assert!(!output.contains("Tool · shell_run"), "{output}");
    assert!(!output.contains("streaming preview"), "{output}");
    assert!(
        output.contains("command: cargo test -p bcode_tui"),
        "{output}"
    );

    app.absorb_session_event(&event(
        session_id,
        2,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call_shell".to_owned(),
            result: "finished".to_owned(),
            is_error: false,
            output: None,
            semantic_result: None,
        },
    ));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 90, 30));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);
    assert!(output.contains("Shell command"), "{output}");
    assert!(!output.contains("Tool · shell_run"), "{output}");
    assert!(
        output.contains("command: cargo test -p bcode_tui"),
        "{output}"
    );
    assert!(output.contains("finished"), "{output}");
}

#[test]
fn replayed_shell_request_uses_shell_plugin_request_renderer_without_legacy_metadata() {
    let session_id = SessionId::new();
    let history = vec![event(
        session_id,
        1,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "call_shell".to_owned(),
            producer_plugin_id: Some("bcode.shell".to_owned()),
            tool_name: "shell.run".to_owned(),
            arguments_json: serde_json::json!({
                "command": "cargo check --workspace",
                "cwd": "/Users/braden/GitHub/bcode",
                "columns": 120,
                "rows": 30,
                "timeout_ms": 120_000,
            })
            .to_string(),
            legacy_request_presentation: None,
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    app.set_plugin_host(Arc::new(shell_plugin_host()));

    let rendered = render_app_text(&mut app);

    assert!(rendered.contains("Shell command"), "{rendered}");
    assert!(
        rendered.contains("command: cargo check --workspace"),
        "{rendered}"
    );
    assert!(
        rendered.contains("cwd: /Users/braden/GitHub/bcode"),
        "{rendered}"
    );
    assert!(!rendered.contains("Tool · shell.run"), "{rendered}");
    assert!(!rendered.contains("arguments"), "{rendered}");
}

#[test]
fn live_file_preview_updates_without_duplicates_and_final_replaces_it() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    app.set_plugin_host(Arc::new(filesystem_plugin_host()));
    for contents in ["fn main", "fn main() {\n    println!(\"hi\");\n}"] {
        app.absorb_session_live_event(&bcode_session_models::SessionLiveEvent {
            session_id,
            kind: bcode_session_models::SessionLiveEventKind::ToolArgumentPreview {
                turn_id: "turn-1".to_owned(),
                tool_call_id: "call_write".to_owned(),
                tool_name: "filesystem_write".to_owned(),
                argument_bytes: contents.len(),
                preview: LiveToolArgumentPreview::FileEdit(LiveFileEditPreview {
                    preview_title: None,
                    streaming_status: None,
                    path: Some("src/main.rs".to_owned()),
                    old_text_prefix: None,
                    new_text_prefix: contents.to_owned(),
                    old_text_required: false,
                    argument_bytes: contents.len(),
                    truncated: false,
                }),
            },
        });
    }

    let mut buffer = Buffer::empty(Rect::new(0, 0, 90, 30));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);
    assert_eq!(output.matches("File change preview").count(), 1, "{output}");
    assert!(output.contains("received:"), "{output}");
    assert!(output.contains("src/main.rs"), "{output}");
    assert!(!output.contains("Ready to apply"), "{output}");

    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "call_write".to_owned(),
            producer_plugin_id: None,
            tool_name: "filesystem_write".to_owned(),
            arguments_json: serde_json::json!({
                "path": "src/main.rs",
                "contents": "fn main() {\n    println!(\"hi\");\n}",
            })
            .to_string(),
            legacy_request_presentation: Some(file_edit_legacy_request_presentation()),
        },
    ));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 90, 30));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);
    assert!(output.contains("tool filesystem_write"), "{output}");
    assert!(!output.contains("streaming preview"), "{output}");
}

#[test]
fn live_file_preview_is_removed_when_final_filesystem_artifact_arrives() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    app.set_plugin_host(Arc::new(filesystem_plugin_host()));
    app.absorb_session_live_event(&bcode_session_models::SessionLiveEvent {
        session_id,
        kind: bcode_session_models::SessionLiveEventKind::ToolArgumentPreview {
            turn_id: "turn-1".to_owned(),
            tool_call_id: "call_write".to_owned(),
            tool_name: "filesystem.write".to_owned(),
            argument_bytes: 18,
            preview: LiveToolArgumentPreview::FileEdit(LiveFileEditPreview {
                preview_title: Some("Write preview".to_owned()),
                streaming_status: Some("writing src/main.rs".to_owned()),
                path: Some("src/main.rs".to_owned()),
                old_text_prefix: None,
                new_text_prefix: "fn main() {}".to_owned(),
                old_text_required: false,
                argument_bytes: 18,
                truncated: false,
            }),
        },
    });
    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "call_write".to_owned(),
            producer_plugin_id: None,
            tool_name: "filesystem.write".to_owned(),
            arguments_json: serde_json::json!({
                "path": "src/main.rs",
                "contents": "fn main() {}",
            })
            .to_string(),
            legacy_request_presentation: Some(file_edit_legacy_request_presentation()),
        },
    ));
    app.absorb_session_event(&event(
        session_id,
        2,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call_write".to_owned(),
            result: "wrote 12 bytes".to_owned(),
            is_error: false,
            output: None,
            semantic_result: Some(ToolInvocationResult::Artifact {
                artifact: Box::new(ToolArtifact {
                    artifact_id: "call_write-filesystem-change".to_owned(),
                    producer_plugin_id: "bcode.filesystem".to_owned(),
                    schema: "bcode.filesystem.change".to_owned(),
                    schema_version: 1,
                    tool_call_id: Some("call_write".to_owned()),
                    title: Some("File change".to_owned()),
                    metadata: serde_json::json!({
                        "tool_name": "filesystem.write",
                        "summary": "wrote 12 bytes",
                        "path": "src/main.rs",
                        "old_text": "",
                        "new_text": "fn main() {}",
                    }),
                    refs: Vec::new(),
                }),
            }),
        },
    ));

    assert!(
        !app.transcript()
            .iter()
            .any(|item| item.is_live_preview_anchor_for("call_write"))
    );
    assert_eq!(
        app.transcript()
            .iter()
            .filter(|item| matches!(item.kind(), TranscriptItemKind::ToolResult { .. }))
            .count(),
        1
    );
    let mut buffer = Buffer::empty(Rect::new(0, 0, 90, 30));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);
    assert!(output.contains("File change"), "{output}");
    assert!(!output.contains("Write preview"), "{output}");
    assert!(!output.contains("received:"), "{output}");
}

#[test]
fn live_file_preview_renders_available_new_text_before_original_text() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    app.set_plugin_host(Arc::new(filesystem_plugin_host()));
    app.absorb_session_live_event(&bcode_session_models::SessionLiveEvent {
        session_id,
        kind: bcode_session_models::SessionLiveEventKind::ToolArgumentPreview {
            turn_id: "turn-1".to_owned(),
            tool_call_id: "call_edit".to_owned(),
            tool_name: "filesystem_edit".to_owned(),
            argument_bytes: 40,
            preview: LiveToolArgumentPreview::FileEdit(LiveFileEditPreview {
                preview_title: None,
                streaming_status: None,
                path: Some("src/lib.rs".to_owned()),
                old_text_prefix: None,
                new_text_prefix: "pub fn demo() {\n    println!(\"hi\");\n}".to_owned(),
                old_text_required: true,
                argument_bytes: 40,
                truncated: false,
            }),
        },
    });

    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 30));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);

    assert!(!output.contains("waiting for original text"), "{output}");
    assert!(output.contains("File change preview"), "{output}");
    assert!(output.contains("src/lib.rs"), "{output}");
    assert!(
        output.contains("original text pending; showing available new text"),
        "{output}"
    );
    assert!(!output.contains("old_text"), "{output}");
    assert!(!output.contains("new_text"), "{output}");
}

#[test]
fn live_file_preview_renders_truncation_note_and_received_bytes() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    app.set_plugin_host(Arc::new(filesystem_plugin_host()));
    app.absorb_session_live_event(&bcode_session_models::SessionLiveEvent {
        session_id,
        kind: bcode_session_models::SessionLiveEventKind::ToolArgumentPreview {
            turn_id: "turn-1".to_owned(),
            tool_call_id: "call_write".to_owned(),
            tool_name: "filesystem_write".to_owned(),
            argument_bytes: 2048,
            preview: LiveToolArgumentPreview::FileEdit(LiveFileEditPreview {
                preview_title: None,
                streaming_status: None,
                path: Some("src/lib.rs".to_owned()),
                old_text_prefix: None,
                new_text_prefix: "pub fn demo() {}".to_owned(),
                old_text_required: false,
                argument_bytes: 2048,
                truncated: true,
            }),
        },
    });

    let mut buffer = Buffer::empty(Rect::new(0, 0, 90, 24));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);
    assert!(output.contains("received: 2.0 KiB"), "{output}");
    assert!(
        output.contains("preview truncated; showing available diff rows"),
        "{output}"
    );
}

#[test]
fn live_file_preview_uses_plugin_owned_renderer() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    app.set_plugin_host(Arc::new(filesystem_plugin_host()));
    app.absorb_session_live_event(&bcode_session_models::SessionLiveEvent {
        session_id,
        kind: bcode_session_models::SessionLiveEventKind::ToolArgumentPreview {
            turn_id: "turn-1".to_owned(),
            tool_call_id: "call_write".to_owned(),
            tool_name: "filesystem_write".to_owned(),
            argument_bytes: 36,
            preview: LiveToolArgumentPreview::FileEdit(LiveFileEditPreview {
                preview_title: None,
                streaming_status: None,
                path: Some("src/lib.rs".to_owned()),
                old_text_prefix: None,
                new_text_prefix: "pub fn demo() {\n    println!(\"hi\");\n}".to_owned(),
                old_text_required: false,
                argument_bytes: 36,
                truncated: false,
            }),
        },
    });

    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 30));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);
    assert!(output.contains("File change preview"), "{output}");
    assert!(output.contains("src/lib.rs"), "{output}");
    assert!(!output.contains("old_text"), "{output}");
    assert!(!output.contains("new_text"), "{output}");
}
