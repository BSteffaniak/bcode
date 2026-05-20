//! BMUX-native TUI backend.

mod app;
mod command_palette;
mod command_palette_render;
mod input;
mod render;
mod session_picker;
mod session_picker_render;

use std::io::{self, Write};
use std::time::Duration;

use bcode_client::BcodeClient;
use bcode_ipc::Event as BcodeEvent;
use bcode_session_models::SessionId;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_text_edit::keyboard::TextKeymap;
use bmux_tui::crossterm::{CrosstermTerminalGuard, poll_event};
use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::geometry::Rect;
use bmux_tui::input::{TextInputEnterBehavior, TextInputKeyHandler, TextInputKeyOutcome};
use bmux_tui::palette::{CommandPalette, CommandPaletteKeyOutcome};
use bmux_tui::terminal::Terminal;
use crossterm::terminal::size;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use self::app::BmuxApp;
use self::command_palette::{BmuxCommandPalette, PaletteCommand};
use super::TuiError;

const EVENT_POLL_TIMEOUT: Duration = Duration::from_millis(50);
const IDLE_REDRAW_INTERVAL: Duration = Duration::from_millis(250);
const INITIAL_HISTORY_EVENT_LIMIT: usize = 500;

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
    let session_id = match session_id {
        Some(session_id) => session_id,
        None => pick_session(terminal, &client).await?,
    };
    let (event_sender, mut event_receiver) = mpsc::unbounded_channel();
    let (attached, event_task) =
        attach_session_event_stream(&client, session_id, event_sender).await?;
    let result = run_with_client(
        terminal,
        &client,
        session_id,
        &attached.history,
        &attached.input_history,
        &mut event_receiver,
    )
    .await;
    event_task.abort();
    result
}

async fn run_with_client<W: Write>(
    terminal: &mut Terminal<&mut W>,
    client: &BcodeClient,
    session_id: SessionId,
    history: &[bcode_session_models::SessionEvent],
    input_history: &[bcode_session_models::SessionInputHistoryEntry],
    event_receiver: &mut mpsc::UnboundedReceiver<BcodeEvent>,
) -> Result<(), TuiError> {
    let mut app = BmuxApp::new_with_history(Some(session_id), history, input_history);
    let mut palette: Option<BmuxCommandPalette> = None;
    let mut needs_redraw = true;

    while !app.should_exit() {
        while let Ok(event) = event_receiver.try_recv() {
            match event {
                BcodeEvent::Session(event) if event.session_id == session_id => {
                    app.absorb_session_event(&event);
                    needs_redraw = true;
                }
                BcodeEvent::Session(_) => {}
            }
        }

        if resize_from_terminal(terminal)? {
            needs_redraw = true;
        }

        if needs_redraw {
            terminal.draw(|frame| {
                render::render(&app, frame);
                if let Some(palette) = &mut palette {
                    command_palette_render::render_palette(palette, frame);
                }
            })?;
            needs_redraw = false;
        }

        if let Some(event) = poll_event(EVENT_POLL_TIMEOUT)? {
            if handle_event(client, &mut app, &mut palette, terminal, event).await? {
                needs_redraw = true;
            }
        } else if app.tick() {
            needs_redraw = true;
        }
    }

    Ok(())
}

async fn pick_session<W: Write>(
    terminal: &mut Terminal<&mut W>,
    client: &BcodeClient,
) -> Result<SessionId, TuiError> {
    let sessions = client.list_sessions().await?;
    let mut picker = session_picker::SessionPickerApp::new(sessions);
    loop {
        terminal.resize(terminal_area()?);
        terminal.draw(|frame| session_picker_render::render_picker(&mut picker, frame))?;
        let Some(event) = poll_event(EVENT_POLL_TIMEOUT)? else {
            continue;
        };
        match event {
            Event::Resize(size) => terminal.resize(Rect::new(0, 0, size.width, size.height)),
            Event::Paste(text) => {
                picker.filter_mut().insert_str(&text);
                picker.refresh_filter();
            }
            Event::Key(stroke) => match handle_picker_key(&mut picker, stroke) {
                PickerKeyOutcome::Continue => {}
                PickerKeyOutcome::Create => return Ok(client.create_session(None).await?.id),
                PickerKeyOutcome::Rename => rename_picker_session(client, &mut picker).await?,
                PickerKeyOutcome::Delete => delete_picker_session(client, &mut picker).await?,
                PickerKeyOutcome::Selected => {
                    if let Some(session_id) = picker.selected_session_id() {
                        return Ok(session_id);
                    }
                    picker.set_status("No session selected; press Ctrl-N to create one".to_owned());
                }
                PickerKeyOutcome::Canceled => return Err(TuiError::Canceled),
            },
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost)
            | Event::Mouse(_)
            | Event::Tick
            | Event::User(_) => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerKeyOutcome {
    Continue,
    Create,
    Rename,
    Delete,
    Selected,
    Canceled,
}

fn handle_picker_key(
    picker: &mut session_picker::SessionPickerApp,
    stroke: KeyStroke,
) -> PickerKeyOutcome {
    match picker.mode() {
        session_picker::SessionPickerMode::Filter => handle_picker_filter_key(picker, stroke),
        session_picker::SessionPickerMode::Rename => handle_picker_rename_key(picker, stroke),
        session_picker::SessionPickerMode::DeleteConfirm => {
            handle_picker_delete_key(picker, stroke)
        }
    }
}

fn handle_picker_filter_key(
    picker: &mut session_picker::SessionPickerApp,
    stroke: KeyStroke,
) -> PickerKeyOutcome {
    if stroke.key == KeyCode::Escape {
        return PickerKeyOutcome::Canceled;
    }
    if matches!(stroke.key, KeyCode::Char('n' | 'N')) && stroke.modifiers.ctrl {
        return PickerKeyOutcome::Create;
    }
    if matches!(stroke.key, KeyCode::Char('r' | 'R')) && stroke.modifiers.ctrl {
        picker.start_rename();
        return PickerKeyOutcome::Continue;
    }
    if matches!(stroke.key, KeyCode::Char('d' | 'D')) && stroke.modifiers.ctrl {
        picker.start_delete_confirmation();
        return PickerKeyOutcome::Continue;
    }
    match stroke.key {
        KeyCode::Enter => PickerKeyOutcome::Selected,
        KeyCode::Up if stroke.modifiers.is_empty() => {
            picker.select_previous();
            PickerKeyOutcome::Continue
        }
        KeyCode::Down if stroke.modifiers.is_empty() => {
            picker.select_next();
            PickerKeyOutcome::Continue
        }
        _ => {
            let outcome = TextInputKeyHandler::new(
                TextKeymap::default(),
                TextInputEnterBehavior::InsertNewline,
            )
            .handle_key(picker.filter_mut(), stroke);
            if outcome == TextInputKeyOutcome::Edited {
                picker.refresh_filter();
            }
            PickerKeyOutcome::Continue
        }
    }
}

fn handle_picker_rename_key(
    picker: &mut session_picker::SessionPickerApp,
    stroke: KeyStroke,
) -> PickerKeyOutcome {
    if stroke.key == KeyCode::Escape {
        picker.cancel_rename();
        return PickerKeyOutcome::Continue;
    }
    if stroke.key == KeyCode::Enter {
        return PickerKeyOutcome::Rename;
    }
    let outcome = TextInputKeyHandler::new(TextKeymap::default(), TextInputEnterBehavior::Submit)
        .handle_key(picker.rename_mut(), stroke);
    if outcome == TextInputKeyOutcome::Submitted {
        PickerKeyOutcome::Rename
    } else {
        PickerKeyOutcome::Continue
    }
}

fn handle_picker_delete_key(
    picker: &mut session_picker::SessionPickerApp,
    stroke: KeyStroke,
) -> PickerKeyOutcome {
    match stroke.key {
        KeyCode::Escape | KeyCode::Char('n' | 'N') => {
            picker.cancel_delete();
            PickerKeyOutcome::Continue
        }
        KeyCode::Char('y' | 'Y') => PickerKeyOutcome::Delete,
        _ => PickerKeyOutcome::Continue,
    }
}

async fn rename_picker_session(
    client: &BcodeClient,
    picker: &mut session_picker::SessionPickerApp,
) -> Result<(), TuiError> {
    let Some(session_id) = picker.selected_session_id() else {
        picker.finish_mutation("No session selected to rename".to_owned());
        return Ok(());
    };
    let name = picker.rename().text().trim();
    let name = (!name.is_empty()).then(|| name.to_owned());
    match client.rename_session(session_id, name).await {
        Ok(_) => {
            picker.replace_sessions(client.list_sessions().await?);
            picker.finish_mutation("Session renamed".to_owned());
        }
        Err(error) => picker.finish_mutation(format!("rename failed: {error}")),
    }
    Ok(())
}

async fn delete_picker_session(
    client: &BcodeClient,
    picker: &mut session_picker::SessionPickerApp,
) -> Result<(), TuiError> {
    let Some(session_id) = picker.selected_session_id() else {
        picker.finish_mutation("No session selected to delete".to_owned());
        return Ok(());
    };
    match client.delete_session(session_id).await {
        Ok(_) => {
            picker.replace_sessions(client.list_sessions().await?);
            picker.finish_mutation("Session deleted".to_owned());
        }
        Err(error) => picker.finish_mutation(format!("delete failed: {error}")),
    }
    Ok(())
}

async fn attach_session_event_stream(
    client: &BcodeClient,
    session_id: SessionId,
    event_sender: mpsc::UnboundedSender<BcodeEvent>,
) -> Result<(bcode_client::AttachedSessionHistory, JoinHandle<()>), TuiError> {
    let mut connection = client.connect("bcode-tui-bmux").await?;
    let attached = connection
        .attach_session_recent_with_input_history(session_id, INITIAL_HISTORY_EVENT_LIMIT)
        .await?;
    let event_task = tokio::spawn(async move {
        loop {
            match connection.recv_event().await {
                Ok(event) => {
                    if event_sender.send(event).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    eprintln!("BMUX TUI event stream ended: {error}");
                    break;
                }
            }
        }
    });
    Ok((attached, event_task))
}

async fn handle_event<W: Write>(
    client: &BcodeClient,
    app: &mut BmuxApp,
    palette: &mut Option<BmuxCommandPalette>,
    terminal: &mut Terminal<&mut W>,
    event: Event,
) -> Result<bool, TuiError> {
    match event {
        Event::Resize(size) => {
            terminal.resize(Rect::new(0, 0, size.width, size.height));
            Ok(true)
        }
        Event::Key(stroke) => {
            if palette.is_some() {
                return handle_palette_key(client, app, palette, terminal, stroke).await;
            }
            if is_palette_open_key(stroke) {
                *palette = Some(BmuxCommandPalette::new());
                return Ok(true);
            }
            let outcome = input::handle_key(app, stroke);
            if outcome.submitted {
                submit_composer(client, app).await?;
            }
            Ok(outcome.redraw)
        }
        Event::Paste(text) => {
            if let Some(palette) = palette {
                palette.state_mut().query.insert_str(&text);
                return Ok(true);
            }
            app.composer_mut().insert_str(&text);
            app.wake_cursor();
            Ok(true)
        }
        Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick => Ok(true),
        Event::Mouse(_) | Event::User(_) => Ok(false),
    }
}

async fn handle_palette_key<W: Write>(
    client: &BcodeClient,
    app: &mut BmuxApp,
    palette: &mut Option<BmuxCommandPalette>,
    terminal: &mut Terminal<&mut W>,
    stroke: KeyStroke,
) -> Result<bool, TuiError> {
    let Some(active_palette) = palette else {
        return Ok(false);
    };
    let items = active_palette.cloned_items();
    let widget = CommandPalette::new(&items);
    let outcome = widget.handle_key(active_palette.state_mut(), terminal.area().height, stroke);
    match outcome {
        CommandPaletteKeyOutcome::Activated(index) => {
            let command = active_palette.command_at(index);
            *palette = None;
            if let Some(command) = command {
                execute_palette_command(client, app, terminal, command).await?;
            }
            Ok(true)
        }
        CommandPaletteKeyOutcome::Canceled => {
            *palette = None;
            Ok(true)
        }
        CommandPaletteKeyOutcome::Ignored => Ok(false),
        CommandPaletteKeyOutcome::QueryEdited | CommandPaletteKeyOutcome::SelectionMoved => {
            Ok(true)
        }
    }
}

async fn execute_palette_command<W: Write>(
    client: &BcodeClient,
    app: &mut BmuxApp,
    terminal: &mut Terminal<&mut W>,
    command: PaletteCommand,
) -> Result<(), TuiError> {
    match command {
        PaletteCommand::NewSession => {
            let session = client.create_session(None).await?;
            app.set_status(format!("created session {}", session.id));
        }
        PaletteCommand::SwitchSession => {
            let session_id = pick_session(terminal, client).await?;
            app.set_status(format!(
                "selected session {session_id}; restart BMUX backend to attach"
            ));
        }
        PaletteCommand::CancelTurn => {
            let Some(session_id) = app.session_id() else {
                app.set_status("No active session".to_owned());
                return Ok(());
            };
            let cancelled = client.cancel_session_turn(session_id).await?;
            app.set_status(if cancelled {
                "cancel requested".to_owned()
            } else {
                "no active turn to cancel".to_owned()
            });
        }
        PaletteCommand::CompactContext => {
            let Some(session_id) = app.session_id() else {
                app.set_status("No active session".to_owned());
                return Ok(());
            };
            let message = client.compact_session(session_id).await?;
            app.set_status(message);
        }
    }
    Ok(())
}

const fn is_palette_open_key(stroke: KeyStroke) -> bool {
    matches!(stroke.key, KeyCode::Char('p' | 'P')) && stroke.modifiers.ctrl
}

async fn submit_composer(client: &BcodeClient, app: &mut BmuxApp) -> Result<(), TuiError> {
    let Some(session_id) = app.session_id() else {
        app.set_status("No active session".to_owned());
        return Ok(());
    };
    let message = app.take_pending_submission();
    if message.trim().is_empty() {
        return Ok(());
    }
    match client.send_user_message(session_id, message).await {
        Ok(acceptance) => {
            if acceptance.queued {
                app.mark_pending_submission_queued(acceptance.queue_position);
                app.set_status(format!(
                    "Message queued{}",
                    acceptance
                        .queue_position
                        .map_or_else(String::new, |position| format!(" at #{position}"))
                ));
            } else {
                app.mark_pending_submission_sent();
                app.set_status("Message sent".to_owned());
            }
            Ok(())
        }
        Err(error) => {
            app.restore_pending_submission();
            app.set_status(format!("send failed: {error}"));
            Ok(())
        }
    }
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
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::Rect;

    use super::{app::BmuxApp, render};

    #[test]
    fn render_includes_status_and_composer() {
        let app = BmuxApp::new_with_history(None, &[], &[]);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 10));
        let cursor = {
            let mut frame = Frame::new(&mut buffer);
            render::render(&app, &mut frame);
            frame.cursor()
        };

        assert!(buffer.row_symbols(0).unwrap().contains("Bcode BMUX TUI"));
        assert!(buffer.row_symbols(3).unwrap().contains("BMUX backend"));
        assert!(buffer.row_symbols(4).unwrap().contains("Composer"));
        assert!(cursor.is_some());
    }
}
