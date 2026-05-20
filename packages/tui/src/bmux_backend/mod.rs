//! BMUX-native TUI backend.

mod app;
mod input;
mod render;

use std::io::{self, Write};
use std::time::Duration;

use bcode_session_models::SessionId;
use bmux_tui::crossterm::{CrosstermTerminalGuard, poll_event};
use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::geometry::Rect;
use bmux_tui::terminal::Terminal;
use crossterm::terminal::size;

use self::app::BmuxApp;
use super::TuiError;

const EVENT_POLL_TIMEOUT: Duration = Duration::from_millis(50);

/// Run the BMUX-native TUI backend.
///
/// # Errors
///
/// Returns I/O errors from terminal setup, event polling, or drawing.
pub fn run(session_id: Option<SessionId>) -> Result<(), TuiError> {
    let stdout = io::stdout();
    let mut guard = CrosstermTerminalGuard::enter(stdout)?;
    let mut terminal = Terminal::new(
        guard.writer_mut().expect("guard writer exists"),
        terminal_area()?,
    );
    let mut app = BmuxApp::new(session_id);

    while !app.should_exit() {
        terminal.resize(terminal_area()?);
        terminal.draw(|frame| render::render(&app, frame))?;
        if let Some(event) = poll_event(EVENT_POLL_TIMEOUT)? {
            handle_event(&mut app, &mut terminal, event);
        }
    }

    drop(terminal);
    let _stdout = guard.leave()?;
    Ok(())
}

fn handle_event<W: Write>(app: &mut BmuxApp, terminal: &mut Terminal<W>, event: Event) {
    match event {
        Event::Resize(size) => terminal.resize(Rect::new(0, 0, size.width, size.height)),
        Event::Key(stroke) => input::handle_key(app, stroke),
        Event::Paste(text) => app.composer_mut().insert_str(&text),
        Event::Focus(FocusEvent::Gained | FocusEvent::Lost)
        | Event::Mouse(_)
        | Event::Tick
        | Event::User(_) => {}
    }
}

fn terminal_area() -> io::Result<Rect> {
    let (width, height) = size()?;
    Ok(Rect::new(0, 0, width, height))
}
