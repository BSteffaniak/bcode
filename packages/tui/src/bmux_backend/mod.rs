//! BMUX-native TUI backend.

mod app;
mod chat_loop;
mod command_palette;
mod command_palette_render;
mod composer_flow;
mod filtered_list;
mod history_flow;
mod input;
mod keymap;
mod model_flow;
mod model_picker;
mod model_picker_render;
mod mouse_flow;
mod palette_flow;
mod permission_dialog;
mod permission_dialog_render;
mod permission_flow;
mod picker_mouse;
mod picker_render;
mod provider_picker;
mod provider_picker_render;
mod render;
mod session_flow;
mod session_picker;
mod session_picker_render;
mod skill_flow;
mod skill_picker;
mod skill_picker_render;
mod slash_commands;
mod slash_flow;
mod slash_palette;
mod slash_palette_render;

use std::io::{self, Write};
use std::time::Duration;

use bcode_client::BcodeClient;
use bcode_ipc::Event as BcodeEvent;
use bcode_session_models::SessionId;
use bmux_keyboard::KeyStroke;
use bmux_text_edit::keyboard::TextKeymap;
use bmux_tui::crossterm::CrosstermTerminalGuard;
use bmux_tui::geometry::Rect;
use bmux_tui::input::{TextInputEnterBehavior, TextInputKeyHandler, TextInputKeyOutcome};
use bmux_tui::terminal::Terminal;
use crossterm::terminal::size;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use self::app::BmuxApp;
use self::keymap::BmuxKeyMap;
use super::TuiError;

const EVENT_POLL_TIMEOUT: Duration = Duration::from_millis(50);
const IDLE_REDRAW_INTERVAL: Duration = Duration::from_millis(250);
const INITIAL_HISTORY_EVENT_LIMIT: usize = 500;
const OLDER_HISTORY_EVENT_LIMIT: usize = 500;
const MOUSE_WHEEL_ROWS: usize = 1;

/// Run the BMUX-native TUI backend.
///
/// # Errors
///
/// Returns I/O errors from terminal setup, event polling, drawing, or Bcode
/// client operations.
pub async fn run(session_id: Option<SessionId>) -> Result<(), TuiError> {
    let stdout = io::stdout();
    let mut guard = CrosstermTerminalGuard::enter(stdout)?;
    let result = {
        let mut terminal = Terminal::new(
            guard.writer_mut().expect("guard writer exists"),
            terminal_area()?,
        );
        run_event_loop(&mut terminal, session_id).await
    };

    match result {
        Ok(()) => {
            let _writer = guard.leave()?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

async fn run_event_loop<W: Write>(
    terminal: &mut Terminal<&mut W>,
    session_id: Option<SessionId>,
) -> Result<(), TuiError> {
    let client = BcodeClient::default_endpoint();
    let config = bcode_config::load_config()?;
    let keymap = BmuxKeyMap::from_config(&config.tui);
    let session_id = match session_id {
        Some(session_id) => session_id,
        None => session_flow::pick_session(terminal, &client, &keymap).await?,
    };
    let (event_sender, event_receiver) = mpsc::unbounded_channel();
    let (attached, event_task) =
        history_flow::attach_session_event_stream(&client, session_id, event_sender.clone())
            .await?;
    let app = BmuxApp::new_with_history(
        Some(session_id),
        &attached.history,
        &attached.input_history,
        attached.history.len() >= INITIAL_HISTORY_EVENT_LIMIT,
    );
    let mut chat = ActiveChat {
        app,
        session_id,
        event_sender,
        event_receiver,
        event_task,
    };
    hydrate_status(&client, &mut chat.app).await;
    let result = chat_loop::run_with_client(terminal, &client, &keymap, &mut chat).await;
    chat.event_task.abort();
    result
}

struct ActiveChat {
    app: BmuxApp,
    session_id: SessionId,
    event_sender: mpsc::UnboundedSender<BcodeEvent>,
    event_receiver: mpsc::UnboundedReceiver<BcodeEvent>,
    event_task: JoinHandle<()>,
}

async fn hydrate_status(client: &BcodeClient, app: &mut BmuxApp) {
    let Some(session_id) = app.session_id() else {
        return;
    };
    let model = client.session_model_status(session_id).await.ok();
    let active_skills = client.active_skills(session_id).await.ok();
    let model_text = model.as_ref().map_or_else(
        || "model unknown".to_owned(),
        |status| {
            let provider = status.provider_plugin_id.as_deref().unwrap_or("auto");
            let model = status.model_id.as_deref().unwrap_or("default");
            format!("{provider}/{model}")
        },
    );
    let skill_count = active_skills.as_ref().map_or(0, Vec::len);
    app.set_status(format!("model: {model_text}; active skills: {skill_count}"));
}

fn handle_text_buffer_key(
    buffer: &mut bmux_text_edit::TextEditBuffer,
    keymap: &BmuxKeyMap,
    stroke: KeyStroke,
    enter_behavior: TextInputEnterBehavior,
) -> TextInputKeyOutcome {
    if let Some(command) = keymap.editor_command_for_key(stroke) {
        buffer.apply_command(command);
        return TextInputKeyOutcome::Edited;
    }
    TextInputKeyHandler::new(TextKeymap::default(), enter_behavior).handle_key(buffer, stroke)
}

async fn switch_session(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    next_session_id: SessionId,
) -> Result<(), TuiError> {
    chat.event_task.abort();
    while chat.event_receiver.try_recv().is_ok() {}
    let (attached, next_task) = history_flow::attach_session_event_stream(
        client,
        next_session_id,
        chat.event_sender.clone(),
    )
    .await?;
    chat.event_task = next_task;
    chat.session_id = next_session_id;
    chat.app = BmuxApp::new_with_history(
        Some(next_session_id),
        &attached.history,
        &attached.input_history,
        attached.history.len() >= INITIAL_HISTORY_EVENT_LIMIT,
    );
    hydrate_status(client, &mut chat.app).await;
    Ok(())
}

fn report_client_error(app: &mut BmuxApp, label: &str, error: &TuiError) {
    let message = format!("{label}: {error}");
    app.set_status(message.clone());
    app.push_system_note(message);
}

fn resize_from_terminal<W: Write>(terminal: &mut Terminal<&mut W>) -> io::Result<bool> {
    let area = terminal_area()?;
    let resized = terminal.area() != area;
    terminal.resize(area);
    Ok(resized)
}

fn terminal_area() -> io::Result<Rect> {
    let (width, height) = size()?;
    Ok(Rect::new(0, 0, width, height))
}

#[cfg(test)]
mod tests {
    use bcode_session_models::{ClientId, SessionEvent, SessionEventKind, SessionId};
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

        assert!(buffer.row_symbols(0).unwrap().contains("Bcode BMUX TUI"));
        assert!(buffer.row_symbols(3).unwrap().contains("BMUX backend"));
        assert!(buffer.row_symbols(4).unwrap().contains("Composer"));
        assert!(cursor.is_some());
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

        slash_palette_render::render_palette(&palette, &mut frame);
        let output = rendered_text(&buffer);

        assert!(output.contains("Slash Commands"));
        assert!(output.contains("/plan"));
        assert!(buffer.row_symbols(0).unwrap().trim().is_empty());
        assert!(buffer.row_symbols(10).unwrap().contains("Slash Commands"));
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
}
