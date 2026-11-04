//! Modal popups: menus, confirmations, prompts and output viewers.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;

use bcode_git::StashOptions;
use bcode_git::models::{RemoteBranch, StashEntry};
use bmux_text_edit::TextEditBuffer;
use bmux_tui::text::Line;
use bmux_tui_components::selectable_list::SelectableListState;
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
#[derive(Debug)]
pub struct Prompt {
    /// Title.
    pub title: String,
    /// Input state.
    pub input: RefCell<TextInputState>,
    /// Purpose.
    pub kind: PromptKind,
    /// Allow several lines.
    pub multiline: bool,
}

impl Prompt {
    /// Create a prompt with initial text.
    #[must_use]
    pub fn new(title: impl Into<String>, initial: &str, kind: PromptKind) -> Self {
        let multiline = matches!(
            kind,
            PromptKind::Commit { .. } | PromptKind::Reword(_) | PromptKind::TagMessage { .. }
        );
        Self {
            title: title.into(),
            input: RefCell::new(TextInputState::new(TextEditBuffer::from_text(initial))),
            kind,
            multiline,
        }
    }

    /// Entered text.
    #[must_use]
    pub fn text(&self) -> String {
        self.input.borrow().buffer().text().to_owned()
    }
}

/// Scrollable read-only text.
#[derive(Debug)]
pub struct TextView {
    /// Title.
    pub title: String,
    /// Content lines.
    pub lines: Vec<Line>,
    /// Scroll position (first visible line).
    pub selected: usize,
    /// List state.
    pub list: Cell<SelectableListState>,
}

impl TextView {
    /// Create a text viewer.
    #[must_use]
    pub fn new(title: impl Into<String>, lines: Vec<Line>) -> Self {
        Self {
            title: title.into(),
            lines,
            selected: 0,
            list: Cell::new(SelectableListState::new(Some(0))),
        }
    }
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
}
