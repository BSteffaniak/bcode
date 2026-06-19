//! Shared context types for threading TUI flow dependencies.
//!
//! These structs keep interactive flow signatures focused without introducing
//! global mutable state: `TuiIo` carries the mutable terminal/input handles, and
//! `TuiServices` carries immutable service dependencies.

use std::io::Write;

use bcode_client::BcodeClient;
use bmux_tui::terminal::Terminal;

use super::keymap::BmuxKeyMap;
use super::render::TuiTheme;
use super::terminal_events::TuiInput;

/// Mutable terminal I/O resources shared by TUI flows.
pub struct TuiIo<'a, 'b, W: Write> {
    pub terminal: &'a mut Terminal<&'b mut W>,
    pub input: &'a mut TuiInput,
}

/// Immutable service dependencies shared by TUI flows.
#[derive(Clone, Copy)]
pub struct TuiServices<'a> {
    /// Interactive client used for explicit user actions; may start the daemon.
    pub client: &'a BcodeClient,
    /// Passive client used for background hydration/polling; never starts the daemon.
    pub passive_client: &'a BcodeClient,
    pub keymap: &'a BmuxKeyMap,
    pub theme: TuiTheme,
}
