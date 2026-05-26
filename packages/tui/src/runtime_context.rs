//! Shared TUI runtime context types.

use std::io::Write;

use bcode_client::BcodeClient;
use bmux_tui::terminal::Terminal;

use super::keymap::BmuxKeyMap;
use super::terminal_events::TuiInput;

/// Mutable terminal I/O resources shared by TUI flows.
pub struct TuiIo<'a, 'b, W: Write> {
    pub terminal: &'a mut Terminal<&'b mut W>,
    pub input: &'a mut TuiInput,
}

/// Immutable service dependencies shared by TUI flows.
#[derive(Clone, Copy)]
pub struct TuiServices<'a> {
    pub client: &'a BcodeClient,
    pub keymap: &'a BmuxKeyMap,
}
