//! Modal popups: menus, confirmations, prompts and output viewers.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;

use bcode_git::StashOptions;
use bcode_git::models::{CommitMessageSource, RemoteBranch, StashEntry};
use bmux_pty::PtyProcess;
use bmux_text_edit::TextEditBuffer;
use bmux_tui::text::Line;
use bmux_tui_components::selectable_list::SelectableListState;
use bmux_tui_components::terminal_view::TerminalState;
use bmux_tui_components::text_input::TextInputState;

use crate::keymap::Action;
use crate::ops::Op;

/// What happens when a menu item or confirmation is chosen.
#[derive(Debug, Clone)]
pub enum Then {
    /// Run operations in order.
    Ops(Vec<Op>),
    /// Dispatch a keymap action.
    Action(Action),
    /// Dispatch a contributed action id (custom commands).
    ActionId(String),
    /// Open a prompt.
    Prompt(PromptKind),
    /// Ask before running.
    Confirm {
        /// Title.
        title: String,
        /// Message.
        message: String,
        /// Follow-up.
        then: Box<Self>,
    },
    /// Launch an external program with the terminal attached.
    External(External),
    /// Run a shell command, showing its terminal output live in a popup.
    Terminal {
        /// The command line.
        command: String,
        /// Close the popup when the command succeeds (it stays open when it fails).
        close_on_success: bool,
    },
    /// Change the panel layout.
    Layout(crate::panels::LayoutCommand),
    /// Close without doing anything.
    Nothing,
}

impl Then {
    /// Run a single operation.
    #[must_use]
    pub fn op(op: Op) -> Self {
        Self::Ops(vec![op])
    }
}

/// A program run with the terminal handed over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct External {
    /// Shell command line.
    pub command: String,
    /// Working directory.
    pub cwd: PathBuf,
    /// Wait for a key press before returning to the UI.
    pub pause: bool,
}

/// One menu row.
#[derive(Debug, Clone)]
pub struct MenuItem {
    /// Label.
    pub label: String,
    /// Right-aligned detail (keys or description).
    pub detail: String,
    /// Optional direct-selection key.
    pub hotkey: Option<char>,
    /// Effect.
    pub then: Then,
}

impl MenuItem {
    /// Menu item with a hotkey.
    #[must_use]
    pub fn new(hotkey: char, label: impl Into<String>, then: Then) -> Self {
        Self {
            label: label.into(),
            detail: String::new(),
            hotkey: Some(hotkey),
            then,
        }
    }

    /// Menu item without a hotkey.
    #[must_use]
    pub fn plain(label: impl Into<String>, detail: impl Into<String>, then: Then) -> Self {
        Self {
            label: label.into(),
            detail: detail.into(),
            hotkey: None,
            then,
        }
    }
}

/// A list of choices, optionally filtered by typed text (command palette).
#[derive(Debug)]
pub struct Menu {
    /// Title.
    pub title: String,
    /// All items.
    pub items: Vec<MenuItem>,
    /// Filter input, when typing filters the list.
    pub filter: Option<RefCell<TextInputState>>,
    /// Selected index into the visible items.
    pub selected: usize,
    /// List state.
    pub list: Cell<SelectableListState>,
}

impl Menu {
    /// Create a menu.
    #[must_use]
    pub fn new(title: impl Into<String>, items: Vec<MenuItem>) -> Self {
        Self {
            title: title.into(),
            items,
            filter: None,
            selected: 0,
            list: Cell::new(SelectableListState::new(Some(0))),
        }
    }

    /// Create a menu filtered by typing.
    #[must_use]
    pub fn filtered(title: impl Into<String>, items: Vec<MenuItem>) -> Self {
        Self {
            filter: Some(RefCell::new(TextInputState::new(TextEditBuffer::new()))),
            ..Self::new(title, items)
        }
    }

    /// Current filter text.
    #[must_use]
    pub fn query(&self) -> String {
        self.filter
            .as_ref()
            .map(|filter| filter.borrow().buffer().text().to_lowercase())
            .unwrap_or_default()
    }

    /// Indices of items matching the filter (all words, any order).
    #[must_use]
    pub fn visible(&self) -> Vec<usize> {
        let query = self.query();
        let words: Vec<&str> = query.split_whitespace().collect();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                let haystack = format!("{} {}", item.label, item.detail).to_lowercase();
                words.iter().all(|word| haystack.contains(word))
            })
            .map(|(index, _)| index)
            .collect()
    }
}

/// A yes/no question.
#[derive(Debug)]
pub struct Confirm {
    /// Title.
    pub title: String,
    /// Message.
    pub message: String,
    /// Effect when confirmed.
    pub then: Then,
}

/// Text collected by a prompt and what it is for.
#[derive(Debug, Clone)]
pub enum PromptKind {
    /// Commit message.
    Commit {
        /// Amend `HEAD`.
        amend: bool,
        /// Skip hooks.
        no_verify: bool,
        /// Stage everything first.
        stage_all: bool,
        /// Where the prefilled text came from, and that text.
        prefill: Option<(CommitMessageSource, String)>,
    },
    /// New message for a commit.
    Reword(String),
    /// New branch name.
    NewBranch {
        /// Start point.
        start: Option<String>,
    },
    /// Rename a branch.
    RenameBranch(String),
    /// Check out by name.
    CheckoutByName,
    /// New tag name at a target.
    NewTag(String),
    /// Annotation for a tag.
    TagMessage {
        /// Tag name.
        name: String,
        /// Target.
        target: String,
    },
    /// New remote name.
    RemoteName,
    /// URL for a new remote.
    RemoteUrl(String),
    /// New URL for an existing remote.
    EditRemoteUrl(String),
    /// Stash message.
    StashMessage(StashOptions),
    /// Rename a stash entry.
    RenameStash(StashEntry),
    /// Branch from a stash.
    StashBranch(String),
    /// Path of a new worktree for a branch.
    WorktreePath {
        /// Branch to check out.
        branch: String,
        /// Create the branch.
        create: bool,
    },
    /// Branch for a new worktree.
    WorktreeBranch,
    /// Shell command.
    Shell,
    /// Live filter for a side view.
    Filter,
    /// Value for a custom command prompt.
    Custom {
        /// Command index.
        command: usize,
        /// Values collected so far.
        values: Vec<(String, String)>,
    },
    /// Local branch name for checking out a remote branch with a different name.
    RemoteCheckoutName(RemoteBranch),
}

/// A text prompt.
///
/// Commit messages use two fields, like lazygit: a one-line summary and an optional
/// free-form description. They are one git message: the summary is its first line and the
/// description follows after a blank line.
#[derive(Debug)]
pub struct Prompt {
    /// Title.
    pub title: String,
    /// Input state; the summary when the prompt has a description field.
    pub input: RefCell<TextInputState>,
    /// Description field of a commit message (boxed: most prompts have none).
    pub body: Option<Box<RefCell<TextInputState>>>,
    /// Whether the description field has focus.
    pub body_focused: bool,
    /// Purpose.
    pub kind: PromptKind,
    /// Allow several lines in the single field.
    pub multiline: bool,
}

impl Prompt {
    /// Create a prompt with initial text. Commit messages are split into summary and
    /// description.
    #[must_use]
    pub fn new(title: impl Into<String>, initial: &str, kind: PromptKind) -> Self {
        let message = matches!(kind, PromptKind::Commit { .. } | PromptKind::Reword(_));
        let mut prompt = Self {
            title: title.into(),
            input: RefCell::new(TextInputState::new(TextEditBuffer::from_text(""))),
            body: message.then(|| {
                Box::new(RefCell::new(TextInputState::new(
                    TextEditBuffer::from_text(""),
                )))
            }),
            body_focused: false,
            multiline: matches!(kind, PromptKind::TagMessage { .. }),
            kind,
        };
        prompt.set_text(initial);
        prompt
    }

    /// Replace the content, splitting a message into summary and description when the
    /// prompt has both fields.
    pub fn set_text(&mut self, text: &str) {
        let (summary, body) = if self.body.is_some() {
            split_message(text)
        } else {
            (text, "")
        };
        *self.input.borrow_mut() = TextInputState::new(TextEditBuffer::from_text(summary));
        if let Some(field) = &self.body {
            *field.borrow_mut() = TextInputState::new(TextEditBuffer::from_text(body));
        }
    }

    /// Entered text: for commit messages the summary, a blank line, then the description.
    #[must_use]
    pub fn text(&self) -> String {
        let summary = self.summary();
        let body = self.description();
        let body = body.trim_end();
        if body.trim().is_empty() {
            summary
        } else {
            format!("{}\n\n{body}", summary.trim_end())
        }
    }

    /// The summary field (or the only field).
    #[must_use]
    pub fn summary(&self) -> String {
        self.input.borrow().buffer().text().to_owned()
    }

    /// The description field; empty when the prompt has none.
    #[must_use]
    pub fn description(&self) -> String {
        self.body
            .as_ref()
            .map(|body| body.borrow().buffer().text().to_owned())
            .unwrap_or_default()
    }

    /// The field that receives typing.
    #[must_use]
    pub fn focused_input(&self) -> &RefCell<TextInputState> {
        match &self.body {
            Some(body) if self.body_focused => body.as_ref(),
            _ => &self.input,
        }
    }
}

/// Split a commit message into its first line and the rest, without the separating blank
/// lines.
#[must_use]
pub fn split_message(text: &str) -> (&str, &str) {
    let (summary, rest) = text.split_once('\n').unwrap_or((text, ""));
    (
        summary.trim_end_matches('\r'),
        rest.trim_start_matches(['\n', '\r']),
    )
}

/// Previously submitted inputs, browsed with up and down like a shell's history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputRecall {
    entries: Vec<String>,
    /// Entry being shown while browsing (index into `entries`).
    index: Option<usize>,
    /// What was typed before browsing started, restored when browsing past the newest entry.
    draft: Option<String>,
}

impl InputRecall {
    /// Most entries kept.
    pub const CAPACITY: usize = 200;

    /// History holding `entries`, oldest first.
    #[must_use]
    pub fn from_entries(entries: Vec<String>) -> Self {
        let mut recall = Self::default();
        for entry in entries {
            recall.push(&entry);
        }
        recall
    }

    /// Entries, oldest first.
    #[must_use]
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Record a submitted input (blank or repeated inputs are not added) and stop browsing.
    pub fn push(&mut self, text: &str) {
        self.reset();
        let text = text.trim_end();
        if text.trim().is_empty() || self.entries.last().is_some_and(|last| last == text) {
            return;
        }
        self.entries.push(text.to_owned());
        if self.entries.len() > Self::CAPACITY {
            self.entries.remove(0);
        }
    }

    /// The entry before the one shown (or the newest, remembering `current` as the draft).
    pub fn older(&mut self, current: &str) -> Option<&str> {
        let index = match self.index {
            Some(0) => return None,
            Some(index) => index - 1,
            None => {
                let newest = self.entries.len().checked_sub(1)?;
                self.draft = Some(current.to_owned());
                newest
            }
        };
        self.index = Some(index);
        self.entries.get(index).map(String::as_str)
    }

    /// The entry after the one shown, or the draft after the newest. `None` when not browsing.
    pub fn newer(&mut self) -> Option<String> {
        let index = self.index?;
        if index + 1 < self.entries.len() {
            self.index = Some(index + 1);
            return self.entries.get(index + 1).cloned();
        }
        self.index = None;
        Some(self.draft.take().unwrap_or_default())
    }

    /// Stop browsing.
    pub fn reset(&mut self) {
        self.index = None;
        self.draft = None;
    }
}

/// Scrollable read-only text.
#[derive(Debug)]
pub struct TextView {
    /// Title.
    pub title: String,
    /// Content lines.
    pub lines: Vec<Line>,
    /// Scroll position.
    pub list: Cell<SelectableListState>,
}

impl TextView {
    /// Create a text viewer.
    #[must_use]
    pub fn new(title: impl Into<String>, lines: Vec<Line>) -> Self {
        Self {
            title: title.into(),
            lines,
            list: Cell::new(SelectableListState::new(None)),
        }
    }
}

/// A shell command running (or finished) on a terminal, shown live.
#[derive(Debug)]
pub struct TerminalRun {
    /// The command line.
    pub command: String,
    /// What it wrote to its terminal.
    pub terminal: TerminalState,
    /// The process while it runs.
    pub process: Option<PtyProcess>,
    /// Close when the command succeeds.
    pub close_on_success: bool,
    /// Whether it was interrupted; interrupting again kills it.
    pub interrupted: bool,
}

/// A modal popup.
#[derive(Debug)]
pub enum Popup {
    /// Choices.
    Menu(Menu),
    /// Yes/no.
    Confirm(Confirm),
    /// Text input.
    Prompt(Prompt),
    /// Read-only text.
    Text(TextView),
    /// A command's live terminal (boxed: it holds a terminal emulator).
    Terminal(Box<TerminalRun>),
}

#[cfg(test)]
mod recall_tests {
    use super::InputRecall;

    #[test]
    fn browses_back_and_forth_and_restores_the_draft() {
        let mut recall = InputRecall::from_entries(vec!["ls".into(), "git gc".into()]);
        assert_eq!(recall.older("ech"), Some("git gc"));
        assert_eq!(recall.older("git gc"), Some("ls"));
        assert_eq!(recall.older("ls"), None);
        assert_eq!(recall.newer().as_deref(), Some("git gc"));
        assert_eq!(recall.newer().as_deref(), Some("ech"));
        assert_eq!(recall.newer(), None);
    }

    #[test]
    fn skips_blank_and_repeated_entries_and_stays_bounded() {
        let mut recall = InputRecall::default();
        recall.push("make");
        recall.push("make");
        recall.push("   ");
        assert_eq!(recall.entries(), ["make"]);
        for index in 0..InputRecall::CAPACITY + 5 {
            recall.push(&index.to_string());
        }
        assert_eq!(recall.entries().len(), InputRecall::CAPACITY);
        assert_eq!(recall.entries().last().map(String::as_str), Some("204"));
        assert_eq!(InputRecall::default().older(""), None);
    }
}
