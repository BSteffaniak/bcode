//! BMUX-native TUI backend.

mod app;
mod input;
mod render;

use std::io::{self, Write};
use std::time::Duration;

use bcode_client::BcodeClient;
use bcode_session_models::SessionId;
use bmux_tui::crossterm::{CrosstermTerminalGuard, poll_event};
use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::geometry::Rect;
use bmux_tui::terminal::Terminal;
use crossterm::terminal::size;

use self::app::BmuxApp;
use super::TuiError;

const EVENT_POLL_TIMEOUT: Duration = Duration::from_millis(50);
const IDLE_REDRAW_INTERVAL: Duration = Duration::from_millis(250);

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
        None => client.create_session(None).await?.id,
    };
    run_with_client(terminal, &client, session_id).await
}

async fn run_with_client<W: Write>(
    terminal: &mut Terminal<&mut W>,
    client: &BcodeClient,
    session_id: SessionId,
) -> Result<(), TuiError> {
    let mut app = BmuxApp::new(Some(session_id));
    let mut needs_redraw = true;

    while !app.should_exit() {
        if resize_from_terminal(terminal)? {
            needs_redraw = true;
        }

        if needs_redraw {
            terminal.draw(|frame| render::render(&app, frame))?;
            needs_redraw = false;
        }

        if let Some(event) = poll_event(EVENT_POLL_TIMEOUT)? {
            if handle_event(&client, &mut app, terminal, event).await? {
                needs_redraw = true;
            }
        } else if app.tick() {
            needs_redraw = true;
        }
    }

    Ok(())
}

async fn handle_event<W: Write>(
    client: &BcodeClient,
    app: &mut BmuxApp,
    terminal: &mut Terminal<&mut W>,
    event: Event,
) -> Result<bool, TuiError> {
    match event {
        Event::Resize(size) => {
            terminal.resize(Rect::new(0, 0, size.width, size.height));
            Ok(true)
        }
        Event::Key(stroke) => {
            let outcome = input::handle_key(app, stroke);
            if outcome.submitted {
                submit_composer(client, app).await?;
            }
            Ok(outcome.redraw)
        }
        Event::Paste(text) => {
            app.composer_mut().insert_str(&text);
            app.wake_cursor();
            Ok(true)
        }
        Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick => Ok(true),
        Event::Mouse(_) | Event::User(_) => Ok(false),
    }
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
                app.set_status(format!(
                    "Message queued{}",
                    acceptance
                        .queue_position
                        .map_or_else(String::new, |position| format!(" at #{position}"))
                ));
            } else {
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
        let app = BmuxApp::new(None);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 10));
        let mut frame = Frame::new(&mut buffer);

        render::render(&app, &mut frame);

        let cursor = frame.cursor();
        drop(frame);

        assert!(buffer.row_symbols(0).unwrap().contains("Bcode BMUX TUI"));
        assert!(buffer.row_symbols(3).unwrap().contains("BMUX backend"));
        assert!(buffer.row_symbols(4).unwrap().contains("Composer"));
        assert!(cursor.is_some());
    }
}
