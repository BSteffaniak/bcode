//! Keybinding actions and lazygit-compatible defaults.
//!
//! Every action is a [`KeymapAction`] registered in a [`KeymapRegistry`], so users rebind or
//! unbind them under `[tui.keybindings.scopes."git.<scope>"]` exactly like other bcode actions.

use std::collections::BTreeMap;

use bcode_keymap::{KeymapAction, KeymapError, KeymapRegistry, SequenceMatch};
use bmux_keyboard::{KeyCode, KeyStroke};

/// Input scope. Panel scopes fall back to [`Scope::Global`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// Available everywhere outside popups.
    Global,
    /// Shared list navigation for every list-like view.
    List,
    /// Status panel.
    Status,
    /// Files panel.
    Files,
    /// Worktrees tab.
    Worktrees,
    /// Submodules tab.
    Submodules,
    /// Local branches tab.
    Branches,
    /// Remotes tab.
    Remotes,
    /// Remote branches of one remote.
    RemoteBranches,
    /// Tags tab.
    Tags,
    /// Commits tab.
    Commits,
    /// Files of one commit or stash.
    CommitFiles,
    /// Reflog tab.
    Reflog,
    /// Stash panel.
    Stash,
    /// Main view while focused (read-only diff or log).
    Main,
    /// Line-by-line staging view.
    Staging,
    /// Popup menus and pickers.
    Menu,
    /// Text prompts and the commit editor.
    Input,
    /// Yes/no confirmations.
    Confirm,
}

impl Scope {
    /// Configuration scope name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Global => "git.global",
            Self::List => "git.list",
            Self::Status => "git.status",
            Self::Files => "git.files",
            Self::Worktrees => "git.worktrees",
            Self::Submodules => "git.submodules",
            Self::Branches => "git.branches",
            Self::Remotes => "git.remotes",
            Self::RemoteBranches => "git.remote-branches",
            Self::Tags => "git.tags",
            Self::Commits => "git.commits",
            Self::CommitFiles => "git.commit-files",
            Self::Reflog => "git.reflog",
            Self::Stash => "git.stash",
            Self::Main => "git.main",
            Self::Staging => "git.staging",
            Self::Menu => "git.menu",
            Self::Input => "git.input",
            Self::Confirm => "git.confirm",
        }
    }
}

macro_rules! actions {
    ($($variant:ident => $scope:ident, $name:literal, $description:literal, [$($key:literal),*];)*) => {
        /// A bindable command.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum Action {
            $(
                #[doc = $description]
                $variant,
            )*
        }

        impl Action {
            /// Every action, in help order.
            pub const ALL: &'static [Self] = &[$(Self::$variant),*];

            /// Scope the action belongs to.
            #[must_use]
            pub const fn scope(self) -> Scope {
                match self {
                    $(Self::$variant => Scope::$scope,)*
                }
            }

            /// Short name, unique within the scope.
            #[must_use]
            pub const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)*
                }
            }

            /// Help description.
            #[must_use]
            pub const fn description(self) -> &'static str {
                match self {
                    $(Self::$variant => $description,)*
                }
            }

            /// Default strokes.
            #[must_use]
            pub const fn defaults(self) -> &'static [&'static str] {
                match self {
                    $(Self::$variant => &[$($key),*],)*
                }
            }
        }
    };
}

actions! {
    // Global
    Quit => Global, "quit", "Quit", ["q", "ctrl+c"];
    Back => Global, "back", "Back / close", ["escape"];
    NextPanel => Global, "next-panel", "Focus next panel", ["tab", "l", "right"];
    PreviousPanel => Global, "previous-panel", "Focus previous panel", ["shift+tab", "h", "left"];
    FocusWindow1 => Global, "focus-window-1", "Focus side window 1", ["1"];
    FocusWindow2 => Global, "focus-window-2", "Focus side window 2", ["2"];
    FocusWindow3 => Global, "focus-window-3", "Focus side window 3", ["3"];
    FocusWindow4 => Global, "focus-window-4", "Focus side window 4", ["4"];
    FocusWindow5 => Global, "focus-window-5", "Focus side window 5", ["5"];
    FocusWindow6 => Global, "focus-window-6", "Focus side window 6", ["6"];
    FocusWindow7 => Global, "focus-window-7", "Focus side window 7", ["7"];
    FocusWindow8 => Global, "focus-window-8", "Focus side window 8", ["8"];
    FocusWindow9 => Global, "focus-window-9", "Focus side window 9", ["9"];
    FocusStatus => Global, "focus-status", "Focus the status view", [];
    FocusFiles => Global, "focus-files", "Focus the files view", [];
    FocusWorktrees => Global, "focus-worktrees", "Focus the worktrees view", [];
    FocusSubmodules => Global, "focus-submodules", "Focus the submodules view", [];
    FocusBranches => Global, "focus-branches", "Focus the local branches view", [];
    FocusRemotes => Global, "focus-remotes", "Focus the remotes view", [];
    FocusTags => Global, "focus-tags", "Focus the tags view", [];
    FocusCommits => Global, "focus-commits", "Focus the commits view", [];
    FocusReflog => Global, "focus-reflog", "Focus the reflog view", [];
    FocusStash => Global, "focus-stash", "Focus the stash view", [];
    FocusMain => Global, "focus-main", "Focus main view", ["0"];
    NextTab => Global, "next-tab", "Next tab", ["]"];
    PreviousTab => Global, "previous-tab", "Previous tab", ["["];
    Help => Global, "help", "Keybindings for this view", ["?"];
    Palette => Global, "palette", "Command palette", ["ctrl+p"];
    Refresh => Global, "refresh", "Refresh", ["shift+r"];
    Push => Global, "push", "Push", ["shift+p"];
    Pull => Global, "pull", "Pull", ["p"];
    Fetch => Global, "fetch", "Fetch all remotes", ["f"];
    OperationMenu => Global, "operation-menu", "Merge / rebase options", ["m"];
    Undo => Global, "undo", "Undo last action", ["z"];
    Redo => Global, "redo", "Redo", ["ctrl+z"];
    ScreenModeNext => Global, "screen-mode-next", "Enlarge focused view", ["+"];
    ScreenModePrevious => Global, "screen-mode-previous", "Shrink focused view", ["_"];
    ScrollMainUp => Global, "scroll-main-up", "Scroll main view up", ["ctrl+u", "shift+k"];
    ScrollMainDown => Global, "scroll-main-down", "Scroll main view down", ["ctrl+d", "shift+j"];
    DiffContextMore => Global, "diff-context-more", "More diff context", ["}"];
    DiffContextLess => Global, "diff-context-less", "Less diff context", ["{"];
    DiffingMenu => Global, "diffing-menu", "Diff against selection", ["shift+w"];
    ShellCommand => Global, "shell-command", "Run shell command", [":"];
    CommandLog => Global, "command-log", "Toggle command log", ["@"];

    // Shared list navigation
    Up => List, "up", "Previous item", ["k", "up"];
    Down => List, "down", "Next item", ["j", "down"];
    PageUp => List, "page-up", "Previous page", [",", "pageup"];
    PageDown => List, "page-down", "Next page", [".", "pagedown"];
    Top => List, "top", "First item", ["<", "home", "g g"];
    Bottom => List, "bottom", "Last item", [">", "end", "shift+g"];
    Filter => List, "filter", "Filter list", ["/"];
    CopyToClipboard => List, "copy", "Copy to clipboard", ["ctrl+o"];

    // Status
    StatusOpenConfig => Status, "open-config", "Edit bcode config", ["e"];

    // Files
    FileToggleStaged => Files, "toggle-staged", "Stage / unstage file", ["space"];
    FileStageAll => Files, "stage-all", "Stage / unstage all", ["a"];
    FileEnter => Files, "enter", "Stage lines / open directory", ["enter"];
    FileCommit => Files, "commit", "Commit", ["c"];
    FileCommitNoVerify => Files, "commit-no-verify", "Commit without hooks", ["w"];
    FileAmend => Files, "amend", "Amend last commit", ["shift+a"];
    FileCommitEditor => Files, "commit-editor", "Commit using $EDITOR", ["shift+c"];
    FileDiscard => Files, "discard", "Discard changes", ["d"];
    FileResetMenu => Files, "reset-menu", "Reset / clean options", ["shift+d"];
    FileEdit => Files, "edit", "Edit file", ["e"];
    FileOpen => Files, "open", "Open file", ["o"];
    FileIgnore => Files, "ignore", "Ignore / exclude file", ["i"];
    FileStash => Files, "stash", "Stash all changes", ["s"];
    FileStashMenu => Files, "stash-menu", "Stash options", ["shift+s"];
    FileToggleTree => Files, "toggle-tree", "Toggle file tree", ["`"];
    FileCollapseAll => Files, "collapse-all", "Collapse all directories", ["-"];
    FileExpandAll => Files, "expand-all", "Expand all directories", ["="];
    FileConflictOurs => Files, "conflict-ours", "Resolve conflict with ours", ["ctrl+o"];
    FileConflictTheirs => Files, "conflict-theirs", "Resolve conflict with theirs", ["ctrl+t"];

    // Worktrees
    WorktreeSwitch => Worktrees, "switch", "Switch to worktree", ["space", "enter"];
    WorktreeNew => Worktrees, "new", "New worktree", ["n"];
    WorktreeRemove => Worktrees, "remove", "Remove worktree", ["d"];

    // Submodules
    SubmoduleEnter => Submodules, "enter", "Open submodule", ["enter"];
    SubmoduleUpdate => Submodules, "update", "Update submodules", ["u"];

    // Branches
    BranchCheckout => Branches, "checkout", "Checkout", ["space"];
    BranchNew => Branches, "new", "New branch", ["n"];
    BranchDelete => Branches, "delete", "Delete branch", ["d"];
    BranchRename => Branches, "rename", "Rename branch", ["shift+r"];
    BranchMerge => Branches, "merge", "Merge into current branch", ["shift+m"];
    BranchRebase => Branches, "rebase", "Rebase current branch onto", ["r"];
    BranchCheckoutByName => Branches, "checkout-by-name", "Checkout by name", ["c"];
    BranchFastForward => Branches, "fast-forward", "Fast-forward to upstream", ["f"];
    BranchUpstream => Branches, "upstream", "Upstream options", ["u"];
    BranchTag => Branches, "tag", "Tag branch tip", ["shift+t"];
    BranchViewCommits => Branches, "view-commits", "View commits", ["enter"];
    BranchCopyName => Branches, "copy-name", "Copy branch name", ["y"];

    // Remotes
    RemoteView => Remotes, "view", "View remote branches", ["enter"];
    RemoteNew => Remotes, "new", "Add remote", ["n"];
    RemoteDelete => Remotes, "delete", "Remove remote", ["d"];
    RemoteEdit => Remotes, "edit", "Edit remote", ["e"];
    RemoteFetch => Remotes, "fetch", "Fetch remote", ["f"];

    // Remote branches
    RemoteBranchCheckout => RemoteBranches, "checkout", "Checkout as local branch", ["space"];
    RemoteBranchDelete => RemoteBranches, "delete", "Delete remote branch", ["d"];
    RemoteBranchMerge => RemoteBranches, "merge", "Merge into current branch", ["shift+m"];
    RemoteBranchRebase => RemoteBranches, "rebase", "Rebase current branch onto", ["r"];
    RemoteBranchUpstream => RemoteBranches, "set-upstream", "Set as upstream of current branch", ["u"];
    RemoteBranchViewCommits => RemoteBranches, "view-commits", "View commits", ["enter"];

    // Tags
    TagCheckout => Tags, "checkout", "Checkout tag", ["space"];
    TagNew => Tags, "new", "New tag", ["n"];
    TagDelete => Tags, "delete", "Delete tag", ["d"];
    TagPush => Tags, "push", "Push tag", ["shift+p"];
    TagViewCommits => Tags, "view-commits", "View commits", ["enter"];

    // Commits
    CommitViewFiles => Commits, "view-files", "View changed files", ["enter"];
    CommitCheckout => Commits, "checkout", "Checkout commit", ["space"];
    CommitSquash => Commits, "squash", "Squash into parent", ["s"];
    CommitFixup => Commits, "fixup", "Fixup into parent", ["f"];
    CommitCreateFixup => Commits, "create-fixup", "Create fixup commit for this", ["shift+f"];
    CommitAutosquash => Commits, "autosquash", "Apply fixups above this commit", ["shift+s"];
    CommitReword => Commits, "reword", "Reword", ["r"];
    CommitDrop => Commits, "drop", "Drop commit", ["d"];
    CommitEdit => Commits, "edit", "Edit (stop rebase here)", ["e"];
    CommitMoveDown => Commits, "move-down", "Move commit down", ["ctrl+j"];
    CommitMoveUp => Commits, "move-up", "Move commit up", ["ctrl+k"];
    CommitAmendInto => Commits, "amend-into", "Amend staged changes into commit", ["shift+a"];
    CommitRevert => Commits, "revert", "Revert commit", ["t"];
    CommitCherryPickCopy => Commits, "cherry-pick-copy", "Copy for cherry-pick", ["shift+c"];
    CommitCherryPickPaste => Commits, "cherry-pick-paste", "Paste (cherry-pick) copied commits", ["shift+v"];
    CommitResetMenu => Commits, "reset-menu", "Reset to this commit", ["g"];
    CommitTag => Commits, "tag", "Tag commit", ["shift+t"];
    CommitNewBranch => Commits, "new-branch", "New branch from commit", ["n"];
    CommitBisect => Commits, "bisect", "Bisect options", ["b"];
    CommitCopySha => Commits, "copy-sha", "Copy commit SHA", ["y"];
    CommitToggleGraph => Commits, "toggle-graph", "Toggle graph", ["ctrl+g"];
    CommitLoadMore => Commits, "load-more", "Load more commits", ["shift+l"];

    // Commit files
    CommitFileView => CommitFiles, "view", "View diff / select lines", ["enter"];
    CommitFileCheckout => CommitFiles, "checkout", "Checkout file from this commit", ["c"];
    CommitFileTogglePatch => CommitFiles, "toggle-patch", "Toggle file in custom patch", ["space"];
    CommitFilePatchMenu => CommitFiles, "patch-menu", "Custom patch options", ["ctrl+p"];

    // Reflog
    ReflogCheckout => Reflog, "checkout", "Checkout entry", ["space"];
    ReflogResetMenu => Reflog, "reset-menu", "Reset to entry", ["g"];
    ReflogViewFiles => Reflog, "view-files", "View changed files", ["enter"];

    // Stash
    StashApply => Stash, "apply", "Apply stash", ["space"];
    StashPop => Stash, "pop", "Pop stash", ["g"];
    StashDrop => Stash, "drop", "Drop stash", ["d"];
    StashRename => Stash, "rename", "Rename stash", ["r"];
    StashBranch => Stash, "branch", "New branch from stash", ["n"];
    StashViewFiles => Stash, "view-files", "View stashed files", ["enter"];

    // Main view
    MainLineUp => Main, "line-up", "Scroll up", ["k", "up"];
    MainLineDown => Main, "line-down", "Scroll down", ["j", "down"];
    MainNextHunk => Main, "next-hunk", "Next hunk", ["n", "]"];
    MainPreviousHunk => Main, "previous-hunk", "Previous hunk", ["shift+n", "["];

    // Staging
    StageLineUp => Staging, "line-up", "Previous line", ["k", "up"];
    StageLineDown => Staging, "line-down", "Next line", ["j", "down"];
    StageNextHunk => Staging, "next-hunk", "Next hunk", ["n", "]"];
    StagePreviousHunk => Staging, "previous-hunk", "Previous hunk", ["shift+n", "["];
    StageToggle => Staging, "toggle", "Stage / unstage selection", ["space"];
    StageRange => Staging, "range", "Toggle range selection", ["v"];
    StageHunkMode => Staging, "hunk-mode", "Toggle hunk selection", ["a"];
    StageSwitchSide => Staging, "switch-side", "Switch staged / unstaged", ["tab"];
    StageDiscard => Staging, "discard", "Discard selection", ["d"];
    StageEdit => Staging, "edit", "Edit file at line", ["e"];
    StageCommit => Staging, "commit", "Commit", ["c"];

    // Menus
    MenuUp => Menu, "up", "Previous option", ["k", "up", "ctrl+p"];
    MenuDown => Menu, "down", "Next option", ["j", "down", "ctrl+n"];
    MenuConfirm => Menu, "confirm", "Choose option", ["enter"];
    MenuClose => Menu, "close", "Close", ["escape", "q"];

    // Input
    InputSubmit => Input, "submit", "Submit", ["enter"];
    InputCancel => Input, "cancel", "Cancel", ["escape"];
    InputToggleAmend => Input, "toggle-amend", "Toggle amend (commit editor)", ["ctrl+a"];
    InputSwitchField => Input, "switch-field", "Switch summary / description (commit editor)", ["tab", "shift+tab"];
    InputSubmitFromAnyField => Input, "submit-any-field", "Submit from any field (commit editor)", ["alt+enter", "ctrl+s"];

    // Confirm
    ConfirmYes => Confirm, "yes", "Confirm", ["enter", "y"];
    ConfirmNo => Confirm, "no", "Cancel", ["escape", "n", "q"];
}

impl Action {
    /// Stable action identity.
    #[must_use]
    pub fn id(self) -> String {
        format!(
            "bcode.git.{}.{}",
            self.scope().name().trim_start_matches("git."),
            self.name()
        )
    }

    /// Registry declaration.
    #[must_use]
    pub fn definition(self) -> KeymapAction {
        KeymapAction {
            id: self.id(),
            scope: self.scope().name().to_owned(),
            description: self.description().to_owned(),
            defaults: self
                .defaults()
                .iter()
                .map(|key| (*key).to_owned())
                .collect(),
        }
    }

    /// Resolve an identity produced by [`Self::id`].
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|action| action.id() == id)
    }
}

/// Resolved bindings for the git UI.
#[derive(Debug, Clone)]
pub struct Keymap {
    registry: KeymapRegistry,
    extra: Vec<KeymapAction>,
}

/// Outcome of resolving input in a stack of scopes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// A registered action, possibly contributed by configuration (custom commands).
    Action(String),
    /// More strokes are required.
    Pending,
    /// No binding matched.
    Unmatched,
}

impl Keymap {
    /// Build the effective keymap from defaults, extra contributed actions and overrides.
    ///
    /// # Errors
    /// * [`KeymapError`] for malformed or conflicting bindings.
    pub fn new(
        extra: Vec<KeymapAction>,
        overrides: BTreeMap<String, BTreeMap<String, String>>,
    ) -> Result<Self, KeymapError> {
        let registry = bcode_keymap::resolve_registry(
            Action::ALL
                .iter()
                .map(|action| action.definition())
                .chain(extra.iter().cloned()),
            overrides,
        )?;
        Ok(Self { registry, extra })
    }

    /// Underlying registry.
    #[must_use]
    pub const fn registry(&self) -> &KeymapRegistry {
        &self.registry
    }

    /// Additional contributed actions (custom commands).
    #[must_use]
    pub fn extra(&self) -> &[KeymapAction] {
        &self.extra
    }

    /// Resolve strokes typed so far against scopes in priority order.
    #[must_use]
    pub fn resolve(&self, scopes: &[Scope], strokes: &[KeyStroke]) -> Resolution {
        let strokes: Vec<KeyStroke> = strokes.iter().copied().map(terminal_stroke).collect();
        let mut pending = false;
        for scope in scopes {
            match self.registry.resolve_sequence(scope.name(), &strokes) {
                SequenceMatch::Complete(action) => return Resolution::Action(action.to_owned()),
                SequenceMatch::Pending => pending = true,
                SequenceMatch::NoMatch => {}
            }
        }
        if pending {
            Resolution::Pending
        } else {
            Resolution::Unmatched
        }
    }

    /// First effective key for an action, for hints.
    #[must_use]
    pub fn key(&self, action: Action) -> Option<&str> {
        self.registry
            .keys_for_action(action.scope().name(), &action.id())
            .first()
            .copied()
    }

    /// All effective keys for an action id in a scope.
    #[must_use]
    pub fn keys(&self, scope: &str, action: &str) -> Vec<&str> {
        self.registry.keys_for_action(scope, action)
    }

    /// Whether configuration owns a stroke in a scope (so it must not become text).
    #[must_use]
    pub fn is_overridden(&self, scope: Scope, stroke: KeyStroke) -> bool {
        self.registry
            .is_overridden_key(scope.name(), terminal_stroke(stroke))
    }
}

/// Terminals report the space bar as the character `' '`, while key spellings such as `space`
/// parse to [`KeyCode::Space`]. Treat both as the same binding.
#[must_use]
pub const fn terminal_stroke(mut stroke: KeyStroke) -> KeyStroke {
    if matches!(stroke.key, KeyCode::Char(' ')) {
        stroke.key = KeyCode::Space;
    }
    stroke
}

/// Pretty-print a configured key spelling for hints (`shift+a` → `A`).
#[must_use]
pub fn display_key(key: &str) -> String {
    key.split_whitespace()
        .map(|stroke| {
            if let Some(letter) = stroke.strip_prefix("shift+")
                && letter.len() == 1
                && letter.chars().all(|ch| ch.is_ascii_lowercase())
            {
                return letter.to_ascii_uppercase();
            }
            match stroke {
                "escape" => "esc".to_owned(),
                "space" => "␣".to_owned(),
                "enter" => "⏎".to_owned(),
                "up" => "↑".to_owned(),
                "down" => "↓".to_owned(),
                "left" => "←".to_owned(),
                "right" => "→".to_owned(),
                other => other.replace("ctrl+", "^"),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stroke(key: &str) -> KeyStroke {
        bmux_keyboard::parse_key_stroke(key).unwrap()
    }

    #[test]
    fn defaults_register_without_conflicts_and_ids_round_trip() {
        let keymap = Keymap::new(Vec::new(), BTreeMap::new()).unwrap();
        for action in Action::ALL {
            assert_eq!(Action::from_id(&action.id()), Some(*action));
        }
        assert_eq!(keymap.key(Action::FileCommit), Some("c"));
    }

    #[test]
    fn panel_scope_wins_over_global_and_sequences_resolve() {
        let keymap = Keymap::new(Vec::new(), BTreeMap::new()).unwrap();
        let scopes = [Scope::Branches, Scope::List, Scope::Global];
        assert_eq!(
            keymap.resolve(&scopes, &[stroke("f")]),
            Resolution::Action(Action::BranchFastForward.id())
        );
        assert_eq!(
            keymap.resolve(&[Scope::Files, Scope::List, Scope::Global], &[stroke("p")]),
            Resolution::Action(Action::Pull.id())
        );
        assert_eq!(keymap.resolve(&scopes, &[stroke("g")]), Resolution::Pending);
        assert_eq!(
            keymap.resolve(&scopes, &[stroke("g"), stroke("g")]),
            Resolution::Action(Action::Top.id())
        );
    }

    #[test]
    fn terminal_space_character_matches_space_bindings() {
        let keymap = Keymap::new(Vec::new(), BTreeMap::new()).unwrap();
        assert_eq!(
            keymap.resolve(
                &[Scope::Files, Scope::List, Scope::Global],
                &[KeyStroke::simple(KeyCode::Char(' '))]
            ),
            Resolution::Action(Action::FileToggleStaged.id())
        );
    }

    #[test]
    fn overrides_rebind_and_unbind() {
        let keymap = Keymap::new(
            Vec::new(),
            BTreeMap::from([(
                "git.files".to_owned(),
                BTreeMap::from([
                    ("x".to_owned(), Action::FileCommit.id()),
                    ("d".to_owned(), String::new()),
                ]),
            )]),
        )
        .unwrap();
        let scopes = [Scope::Files, Scope::List, Scope::Global];
        assert_eq!(
            keymap.resolve(&scopes, &[stroke("x")]),
            Resolution::Action(Action::FileCommit.id())
        );
        assert_eq!(
            keymap.resolve(&scopes, &[stroke("c")]),
            Resolution::Unmatched
        );
        assert_eq!(
            keymap.resolve(&scopes, &[stroke("d")]),
            Resolution::Unmatched
        );
    }

    #[test]
    fn display_keys_are_compact() {
        assert_eq!(display_key("shift+a"), "A");
        assert_eq!(display_key("ctrl+j"), "^j");
        assert_eq!(display_key("g g"), "g g");
    }
}
