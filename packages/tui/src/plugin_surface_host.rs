//! Host adapter for native plugin-owned TUI surfaces.

use std::io::Write;

use bcode_plugin_sdk::tui::{PluginTuiAction, PluginTuiSurface, TokioPluginTuiHost};
use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::geometry::Rect;
use bmux_tui::terminal::Terminal;
use tokio::sync::mpsc;

use super::terminal_events::TuiInput;
use super::{TuiError, helpers};

/// Run one plugin-owned native TUI surface in the Bcode TUI host.
///
/// # Errors
///
/// Returns an error when terminal I/O or terminal input fails.
#[allow(clippy::future_not_send)]
pub async fn run_plugin_surface<W: Write>(
    terminal: &mut Terminal<&mut W>,
    surface: &mut dyn PluginTuiSurface,
) -> Result<(), TuiError> {
    let mut input = TuiInput::start();
    let (redraw_sender, mut redraw_receiver) = mpsc::unbounded_channel();
    let host = TokioPluginTuiHost::current(redraw_sender);
    let mut needs_redraw = true;
    let mut should_exit = false;

    while !should_exit {
        if helpers::resize_from_terminal(terminal)? {
            needs_redraw = true;
        }
        if surface.poll(&host).requests_redraw() {
            needs_redraw = true;
        }
        if needs_redraw {
            terminal.draw(|frame| {
                let area = frame.area();
                surface.render(area, frame);
            })?;
            needs_redraw = false;
        }

        tokio::select! {
            event = input.recv() => {
                let Some(event) = event? else {
                    continue;
                };
                if handle_host_event(terminal, &event) {
                    needs_redraw = true;
                }
                match surface.handle_event(&event, &host) {
                    PluginTuiAction::None => {}
                    PluginTuiAction::Redraw => needs_redraw = true,
                    PluginTuiAction::Close => should_exit = true,
                    PluginTuiAction::OpenSurface { .. } | PluginTuiAction::RunCommand { .. } => {
                        needs_redraw = true;
                    }
                }
            }
            redraw = redraw_receiver.recv() => {
                if redraw.is_some() {
                    needs_redraw = true;
                }
            }
        }
    }

    Ok(())
}

fn handle_host_event<W: Write>(terminal: &mut Terminal<&mut W>, event: &Event) -> bool {
    match event {
        Event::Resize(size) => {
            terminal.resize(Rect::new(0, 0, size.width, size.height));
            true
        }
        Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick => true,
        Event::Key(_) | Event::Mouse(_) | Event::Paste(_) | Event::User(_) => false,
    }
}
