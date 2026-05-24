//! TUI tests.

use std::collections::BTreeMap;

use bcode_session_models::{
    ClientId, SessionEvent, SessionEventKind, SessionId, SessionInputHistoryEntry, SessionSummary,
    SessionTokenUsage, ToolInvocationStreamEvent, ToolOutputStream,
};
use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
use bmux_text_edit::TextMotion;
use bmux_tui::buffer::Buffer;
use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect};

use super::{
    app::BmuxApp,
    input,
    keymap::{BmuxAction, BmuxKeyMap, BmuxScope},
    render, slash_palette, slash_palette_render,
    transcript::{TranscriptItemKind, transcript_items_from_events_with_reasoning},
};

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
fn multiline_paste_preserves_line_breaks_in_composer() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);

    app.paste_composer_text("first\nsecond\r\nthird\rfourth");

    assert_eq!(app.composer().text(), "first\nsecond\nthird\nfourth");
}

#[test]
fn escape_interrupt_does_not_exit_chat() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    let keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());

    let outcome = input::handle_key(&mut app, &keymap, key(KeyCode::Escape));

    assert!(outcome.interrupted);
    assert!(!app.should_exit());
}

#[test]
fn configured_ctrl_enter_submits_while_enter_inserts_newline() {
    let mut config = bcode_config::TuiConfig::default();
    config.keybindings.chat = BTreeMap::from([
        ("ctrl+enter".to_owned(), "tui.input.submit".to_owned()),
        ("enter".to_owned(), "tui.input.newLine".to_owned()),
    ]);
    let keymap = BmuxKeyMap::from_config(&config);
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    app.replace_composer_with("draft");

    let enter = input::handle_key(&mut app, &keymap, key(KeyCode::Enter));
    assert!(enter.redraw);
    assert!(!enter.submitted);
    assert_eq!(app.composer().text(), "draft\n");

    let ctrl_enter = input::handle_key(&mut app, &keymap, ctrl_key_code(KeyCode::Enter));
    assert!(ctrl_enter.submitted);
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
        ("ctrl+enter".to_owned(), "tui.input.submit".to_owned()),
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
        Some(BmuxAction::InputSubmit)
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
            sequence: 1,
            text: "first prompt".to_owned(),
        },
        SessionInputHistoryEntry {
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
            sequence: 1,
            text: "older prompt\nolder second line".to_owned(),
        },
        SessionInputHistoryEntry {
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
        client_count: 1,
        created_at_ms: 1,
        updated_at_ms: 2,
        working_directory: "/tmp/bcode-tui-test".into(),
    });
    let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 10));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut frame);

    assert!(buffer.row_symbols(0).unwrap().contains("Canonical title"));
    assert!(!buffer.row_symbols(0).unwrap().contains("Untitled session"));
}

#[test]
fn live_session_rename_overrides_attach_summary_title() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
    app.apply_session_summary(&SessionSummary {
        id: session_id,
        name: Some("Old title".to_owned()),
        client_count: 1,
        created_at_ms: 1,
        updated_at_ms: 2,
        working_directory: "/tmp/bcode-tui-test".into(),
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
    assert!(buffer.row_symbols(0).unwrap().contains("agent: plan"));
    assert!(output.contains("ctx 512/1.0k 50%"));
    assert!(output.contains("cached 256 tok"));
    assert!(output.contains("cache write 128 tok"));
    assert!(output.contains("spent 640 tok"));
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

    slash_palette_render::render_palette(&palette, Rect::new(2, 18, 76, 1), &mut frame);
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
                tool_name: "shell.run".to_owned(),
                arguments_json: r#"{"command":"cargo check","cwd":"/tmp/project"}"#.to_owned(),
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::ToolCallFinished {
                tool_call_id: full_call_id.to_owned(),
                result: "ok".to_owned(),
                is_error: false,
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
    assert!(output.contains("command: cargo check"));
    assert!(output.contains("cwd: /tmp/project"));
    assert!(output.contains("Tool result · shell.run · ok"));
    assert!(output.contains("    ok"));
}

#[test]
fn transcript_renders_filesystem_edit_inline_diff_preview() {
    let session_id = SessionId::new();
    let history = [event(
        session_id,
        1,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "call_edit".to_owned(),
            tool_name: "filesystem.edit".to_owned(),
            arguments_json: serde_json::json!({
                "path": "src/lib.rs",
                "old_text": "fn answer() -> i32 {\n    41\n}\n",
                "new_text": "fn answer() -> i32 {\n    42\n}\n",
            })
            .to_string(),
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 18));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);

    assert!(output.contains("Tool · filesystem.edit"));
    assert!(output.contains("src/lib.rs  +1 -1"));
    assert!(output.contains("replaced 1 line with 1 line"));
    assert!(output.contains("showing"));
    assert!(output.contains("-   2 │     41"));
    assert!(output.contains("+   2 │     42"));
    assert!(!output.contains("\"old_text\""));
    assert_eq!(
        buffer
            .get(Point::new(
                2,
                output_line_y(&buffer, "-   2 │     41").unwrap()
            ))
            .map(|cell| cell.style.fg),
        Some(Some(bmux_tui::style::Color::BrightRed))
    );
    assert_eq!(
        buffer
            .get(Point::new(
                2,
                output_line_y(&buffer, "+   2 │     42").unwrap()
            ))
            .map(|cell| cell.style.fg),
        Some(Some(bmux_tui::style::Color::BrightGreen))
    );
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
                tool_name: "shell.run".to_owned(),
                arguments_json: serde_json::json!({
                    "command": "cargo test",
                    "cwd": "/tmp/project",
                })
                .to_string(),
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::ToolCallFinished {
                tool_call_id: "call_shell".to_owned(),
                result: format!(
                    "exit_code: 0\ntimed_out: false\nstdout:\n{stdout}\nstderr:\n\u{1b}[31mwarning\u{1b}[0m"
                ),
                is_error: false,
            },
        ),
    ];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 40));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);

    assert!(output.contains("Tool result · shell.run · ok"));
    assert!(output.contains("terminal: yes"));
    assert!(output.contains("exit 0"));
    assert!(!output.contains("line 0"));
    assert!(output.contains("line 39"));
    assert!(output.contains("stdout rows hidden"));
    assert!(!output.contains('\u{1b}'));
    assert_eq!(
        buffer
            .get(Point::new(4, output_line_y(&buffer, "line 39").unwrap()))
            .map(|cell| cell.style.fg),
        Some(Some(bmux_tui::style::Color::Green))
    );
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
                tool_name: "shell.run".to_owned(),
                arguments_json: serde_json::json!({
                    "command": "git status --short && ls",
                })
                .to_string(),
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::ToolCallFinished {
                tool_call_id: "call_terminal".to_owned(),
                result: serde_json::json!({
                    "mode": "terminal",
                    "exit_code": 0,
                    "timed_out": false,
                    "output": output,
                    "columns": 80,
                    "rows": 10,
                })
                .to_string(),
                is_error: false,
            },
        ),
    ];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 40));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);

    assert!(output.contains("Tool result · shell.run · ok"));
    assert!(output.contains("terminal: 80x10"));
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
            result: serde_json::json!({
                "mode": "terminal",
                "exit_code": 0,
                "timed_out": false,
                "output": "one\r\ntwo",
                "columns": 80,
                "rows": 10,
            })
            .to_string(),
            is_error: false,
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 18));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);

    assert!(output.contains("one"));
    assert!(output.contains("two"));
    assert!(output.contains("Message"));
}

#[test]
fn transcript_renders_truncated_terminal_shell_output_as_terminal() {
    let session_id = SessionId::new();
    let history = [event(
        session_id,
        1,
        SessionEventKind::ToolCallFinished {
            tool_call_id: "call_terminal".to_owned(),
            result: serde_json::json!({
                "mode": "terminal",
                "exit_code": 0,
                "timed_out": false,
                "output": "one\r\ntwo",
                "output_truncated": true,
                "output_bytes": 70000,
                "retained_output_bytes": 65536,
                "columns": 80,
                "rows": 10,
            })
            .to_string(),
            is_error: false,
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 20));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut frame);
    let output = rendered_text(&buffer);

    assert!(output.contains("output truncated"));
    assert!(output.contains("showing 65536 of 70000 bytes"));
    assert!(output.contains("one"));
    assert!(output.contains("two"));
    assert!(!output.contains("{\"mode\":\"terminal\""));
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
fn streamed_tool_output_is_not_duplicated_by_final_result() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);

    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "call-1".to_owned(),
            tool_name: "filesystem.shell.run".to_owned(),
            arguments_json: "{}".to_owned(),
        },
    ));
    app.absorb_session_event(&event(
        session_id,
        2,
        SessionEventKind::ToolInvocationStream {
            event: ToolInvocationStreamEvent::Started {
                tool_call_id: "call-1".to_owned(),
                tool_name: "filesystem.shell.run".to_owned(),
                terminal: true,
                columns: Some(80),
                rows: Some(24),
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
    assert!(
        !transcript
            .iter()
            .any(|item| item.text().contains("final duplicate tail"))
    );

    assert_eq!(terminal_items.len(), 1);
    assert_eq!(terminal_items[0].text(), "live output\n");
    assert!(!terminal_items[0].streaming());
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
    assert!(
        !app.transcript()
            .iter()
            .any(|item| item.text().contains("final duplicate tail"))
    );

    assert_eq!(terminal_items.len(), 1);
    assert_eq!(terminal_items[0].text(), "live output\n");
    assert!(!terminal_items[0].streaming());
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
                tool_name: "shell.run".to_owned(),
                arguments_json: "{}".to_owned(),
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::Started {
                    tool_call_id: "call-empty".to_owned(),
                    tool_name: "shell.run".to_owned(),
                    terminal: true,
                    columns: Some(120),
                    rows: Some(40),
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
            },
        ),
    ];

    let transcript = transcript_items_from_events_with_reasoning(&events, true);

    assert!(transcript.iter().any(|item| item.text() == "final result"));
}

fn streamed_terminal_tool_events(session_id: SessionId) -> Vec<SessionEvent> {
    vec![
        event(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-stream".to_owned(),
                tool_name: "shell.run".to_owned(),
                arguments_json: "{}".to_owned(),
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::Started {
                    tool_call_id: "call-stream".to_owned(),
                    tool_name: "shell.run".to_owned(),
                    terminal: true,
                    columns: Some(120),
                    rows: Some(40),
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
            },
        ),
    ]
}

fn event(session_id: SessionId, sequence: u64, kind: SessionEventKind) -> SessionEvent {
    SessionEvent {
        schema_version: 1,
        sequence,
        session_id,
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
