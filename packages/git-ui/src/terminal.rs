//! Live terminal output: shell commands run from the UI, and git commands' output in the
//! command log.
//!
//! Shell commands (`:` and custom commands) run on a pseudo-terminal and show in a popup as
//! they run. Git commands run through the repository's runner, which records their terminal
//! output in the command log; the log keeps one parsed terminal per shown command, fed with
//! what the runner recorded since the last sync.

use std::collections::BTreeMap;

use bcode_git::models::GitCommandRecord;
use bmux_pty::{PtyCommand, PtyEvent, PtySize};
use bmux_tui_components::terminal_view::{TerminalState, TerminalStatus};
use bmux_tui_runtime::{Command, Update};

use crate::app::{App, Msg, UpdateExt as _};
use crate::keymap::Action;
use crate::popup::{Popup, TerminalRun};

/// Commands shown in the command log, newest first.
pub const LOG_ENTRIES: usize = 100;
/// History kept per terminal.
const SCROLLBACK: usize = 5_000;
/// Events applied per terminal per update, so a flood of output cannot stall the UI.
const EVENTS_PER_UPDATE: usize = 256;

/// A command-log entry's terminal and how much of the recorded output it has read.
#[derive(Debug)]
pub struct LogOutput {
    /// The parsed output.
    pub terminal: TerminalState,
    read: usize,
}

impl App {
    /// Run a shell command in the repository root on a terminal, shown live in a popup; with
    /// `close_on_success`, the popup closes by itself when the command succeeds.
    pub(crate) fn run_in_terminal(
        &mut self,
        command: String,
        close_on_success: bool,
    ) -> Update<Msg> {
        // A first guess at the popup's size; the view asks for its real size once painted.
        let (width, height) = self.size.get();
        let size = PtySize::new(
            width
                .min(crate::view::TERMINAL_POPUP_WIDTH)
                .saturating_sub(4),
            (height.saturating_mul(4) / 5).saturating_sub(4),
        );
        let (program, args) = crate::ops::shell_invocation(&command);
        let process = PtyCommand::new(program)
            .args(args)
            .current_dir(self.repo.root())
            .spawn(size);
        match process {
            Ok(process) => {
                self.pending.clear();
                self.popups.push(Popup::Terminal(Box::new(TerminalRun {
                    command,
                    terminal: TerminalState::new(size.cols, size.rows, SCROLLBACK),
                    process: Some(process),
                    close_on_success,
                    interrupted: false,
                })));
            }
            Err(error) => self.toast(format!("Could not run {command}: {error}"), true),
        }
        Update::redraw()
    }

    /// A key in the terminal popup: interrupt (`ctrl+c`) interrupts a running command (a
    /// second time kills one that ignores it) and closes a finished one, close keys close it
    /// (stopping the command), up and down scroll.
    pub(crate) fn terminal_key(
        &mut self,
        mut run: Box<TerminalRun>,
        action: Option<Action>,
    ) -> Update<Msg> {
        let interrupt = action == Some(Action::MenuInterrupt);
        if interrupt && let Some(process) = &run.process {
            let _ = if run.interrupted {
                process.kill()
            } else {
                process.interrupt()
            };
            run.interrupted = true;
            self.popups.push(Popup::Terminal(run));
            return Update::redraw();
        }
        if interrupt || matches!(action, Some(Action::MenuClose | Action::MenuConfirm)) {
            // Dropping a running process stops it.
            return Update::redraw().maybe(self.refresh());
        }
        self.popups.push(Popup::Terminal(run));
        let delta = match action {
            Some(Action::MenuUp) => -1,
            Some(Action::MenuDown) => 1,
            _ => return Update::none(),
        };
        self.scroll_list(crate::scroll::TERMINAL_LIST, delta);
        Update::redraw()
    }

    /// Apply output of a running shell command and its view's size. Returns whether anything
    /// changed, and a refresh once the command ends (it may have changed the repository).
    pub(crate) fn pump_terminal(&mut self) -> (bool, Option<Command<Msg>>) {
        let Some(run) = self.popups.iter_mut().find_map(|popup| match popup {
            Popup::Terminal(run) => Some(run),
            _ => None,
        }) else {
            return (false, None);
        };
        let mut changed = if let Some((cols, rows)) = run.terminal.requested_size() {
            run.terminal.resize(cols, rows);
            if let Some(process) = &run.process {
                let _ = process.resize(PtySize::new(cols, rows));
            }
            true
        } else {
            false
        };
        let mut exited = false;
        if let Some(process) = &mut run.process {
            for event in process.ready_events(EVENTS_PER_UPDATE) {
                changed = true;
                match event {
                    PtyEvent::Output(bytes) | PtyEvent::Stdout(bytes) => run.terminal.feed(&bytes),
                    PtyEvent::Exited(exit) => {
                        run.terminal.finish(TerminalStatus::Exited(exit.code()));
                        exited = true;
                    }
                }
            }
        }
        if exited {
            run.process = None;
            if run.close_on_success && run.terminal.status().success() {
                self.popups
                    .retain(|popup| !matches!(popup, Popup::Terminal(run) if run.process.is_none() && run.close_on_success));
            }
            return (true, self.refresh());
        }
        (changed, None)
    }

    /// Bring the command log's entries and their terminals up to date with the runner's
    /// records. Returns whether anything changed.
    pub(crate) fn sync_command_log(&mut self) -> bool {
        let log = self.repo.runner().log();
        let revision = log.revision();
        if self.log_revision == Some(revision) {
            return false;
        }
        self.log_revision = Some(revision);
        let records: Vec<GitCommandRecord> =
            log.records().into_iter().rev().take(LOG_ENTRIES).collect();
        let terminals = &mut self.log_terminals;
        terminals.retain(|id, _| records.iter().any(|record| record.id == *id));
        for record in &records {
            let output = terminals.entry(record.id).or_insert_with(|| LogOutput {
                terminal: TerminalState::new(
                    bcode_git::COMMAND_TERMINAL_COLUMNS,
                    bcode_git::COMMAND_TERMINAL_ROWS,
                    SCROLLBACK,
                ),
                read: 0,
            });
            let bytes = log.output(record.id, output.read);
            if !bytes.is_empty() {
                output.read += bytes.len();
                output.terminal.feed(&bytes);
            }
            if !record.running() && output.terminal.status() == TerminalStatus::Running {
                output
                    .terminal
                    .finish(TerminalStatus::Exited(Some(i32::from(!record.success()))));
            }
        }
        self.log_records = records;
        true
    }
}

/// Terminals of command-log entries by record id.
pub type LogOutputs = BTreeMap<u64, LogOutput>;
