//! BMUX backend tests.

use std::collections::BTreeSet;

use bcode_session_models::{
    ClientId, SessionEvent, SessionEventKind, SessionId, SessionTokenUsage,
};
use bmux_tui::buffer::Buffer;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;

use super::{app::BmuxApp, render, slash_palette, slash_palette_render};

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
    assert!(output.contains("BMUX backend"));
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
fn header_and_footer_include_model_agent_and_token_context() {
    let session_id = SessionId::new();
    let history = [
        event(
            session_id,
            1,
            SessionEventKind::SessionCreated {
                name: Some("Visual parity work".to_owned()),
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
        model: Some(bcode_model::ModelInfo {
            model_id: "model-example".to_owned(),
            display_name: "Model Example".to_owned(),
            is_default: false,
            context_window: Some(1024),
            max_output_tokens: None,
            capabilities: BTreeSet::new(),
        }),
    });
    let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 12));
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
    assert!(buffer.row_symbols(0).unwrap().contains("model-example"));
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

    assert!(output.contains("Assistant: hello world"));
    assert!(!output.contains("Assistant …: hello"));
    assert_eq!(output.matches("Assistant").count(), 1);
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

fn event(session_id: SessionId, sequence: u64, kind: SessionEventKind) -> SessionEvent {
    SessionEvent {
        schema_version: 1,
        sequence,
        session_id,
        kind,
    }
}

fn rendered_text(buffer: &Buffer) -> String {
    (0..buffer.area().height)
        .filter_map(|row| buffer.row_symbols(row))
        .collect::<Vec<_>>()
        .join("\n")
}
