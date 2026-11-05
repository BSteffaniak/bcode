//! Action dispatch, menus and prompts.

use std::collections::BTreeSet;

use bcode_git::models::{
    Commit, CommitMessageSource, DiffLineKind, HeadState, RebaseAction, RebaseTodoItem,
    RepoOperation, ResetMode,
};
use bcode_git::{ApplyTarget, CommitOptions, ConflictSide, DiffTarget, PushOptions, StashOptions};
use bmux_keyboard::KeyStroke;
use bmux_tui::event::Event;
use bmux_tui::text::{Line, Span};
use bmux_tui_components::text_input::{TextInputControl, TextInputPolicy};
use bmux_tui_runtime::Update;

use crate::app::{App, Msg, Row, ScreenMode, Staging, UpdateExt, View, Window};
use crate::data::{FileNode, SectionKind};
use crate::keymap::{Action, Scope, display_key};
use crate::ops::{DraftCleanup, Op};
use crate::popup::{External, Menu, MenuItem, Popup, Prompt, PromptKind, TextView, Then};

/// Action run by Enter (or a repeated click) in a view.
#[must_use]
pub const fn primary_action(view: View) -> Action {
    match view {
        View::Status => Action::FocusMain,
        View::Files => Action::FileEnter,
        View::Worktrees => Action::WorktreeSwitch,
        View::Submodules => Action::SubmoduleEnter,
        View::Branches => Action::BranchViewCommits,
        View::Remotes => Action::RemoteView,
        View::RemoteBranches => Action::RemoteBranchViewCommits,
        View::Tags => Action::TagViewCommits,
        View::SubCommits | View::Commits => Action::CommitViewFiles,
        View::Reflog => Action::ReflogViewFiles,
        View::Stash => Action::StashViewFiles,
        View::CommitFiles => Action::CommitFileView,
    }
}

fn quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "-_./:@+=,".contains(ch))
        && !value.is_empty()
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn editor(app: &App) -> String {
    app.config
        .editor
        .clone()
        .or_else(|| std::env::var("GIT_EDITOR").ok())
        .or_else(|| std::env::var("VISUAL").ok())
        .or_else(|| std::env::var("EDITOR").ok())
        .unwrap_or_else(|| "vi".into())
}

fn edit_command(app: &App, path: &str, line: Option<u32>) -> External {
    let editor = editor(app);
    let program = editor
        .split_whitespace()
        .next()
        .and_then(|program| std::path::Path::new(program).file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let full = app.repo.root().join(path);
    let target = quote(&full.to_string_lossy());
    let command = match (line, program.as_str()) {
        (
            Some(line),
            "vi" | "vim" | "nvim" | "nano" | "emacs" | "emacsclient" | "hx" | "kak" | "micro"
            | "helix",
        ) => {
            format!("{editor} +{line} {target}")
        }
        (Some(line), "code" | "code-insiders" | "cursor" | "zed") => {
            format!(
                "{editor} --goto {}",
                quote(&format!("{}:{line}", full.to_string_lossy()))
            )
        }
        _ => format!("{editor} {target}"),
    };
    External {
        command,
        cwd: app.repo.root().to_owned(),
        pause: false,
    }
}

fn open_command(app: &App, path: &str) -> External {
    let full = quote(&app.repo.root().join(path).to_string_lossy());
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "start \"\""
    } else {
        "xdg-open"
    };
    External {
        command: format!("{opener} {full}"),
        cwd: app.repo.root().to_owned(),
        pause: false,
    }
}

impl App {
    /// Run an action.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn dispatch(&mut self, action: Action) -> Update<Msg> {
        let view = self.current_view();
        let mut update = match action {
            // ----- global -----
            Action::Quit => {
                self.quit = true;
                return Update::exit();
            }
            Action::Back => self.back(),
            Action::NextPanel | Action::PreviousPanel => {
                if self.window == Window::Main
                    && action == Action::NextPanel
                    && self.staging.is_none()
                {
                    return Update::none();
                }
                let count = self.layout.windows.len();
                let next = match (self.window, action) {
                    (Window::Side(index), Action::NextPanel) => Window::Side((index + 1) % count),
                    (Window::Side(index), _) => Window::Side((index + count - 1) % count),
                    (Window::Main, _) => self.last_side,
                };
                self.focus(next);
                Update::redraw()
            }
            Action::FocusWindow1
            | Action::FocusWindow2
            | Action::FocusWindow3
            | Action::FocusWindow4
            | Action::FocusWindow5
            | Action::FocusWindow6
            | Action::FocusWindow7
            | Action::FocusWindow8
            | Action::FocusWindow9 => {
                let index = [
                    Action::FocusWindow1,
                    Action::FocusWindow2,
                    Action::FocusWindow3,
                    Action::FocusWindow4,
                    Action::FocusWindow5,
                    Action::FocusWindow6,
                    Action::FocusWindow7,
                    Action::FocusWindow8,
                    Action::FocusWindow9,
                ]
                .iter()
                .position(|candidate| *candidate == action)
                .unwrap_or(0);
                if index < self.layout.windows.len() {
                    self.focus(Window::Side(index));
                }
                Update::redraw()
            }
            Action::FocusStatus => self.focus_named(View::Status),
            Action::FocusFiles => self.focus_named(View::Files),
            Action::FocusWorktrees => self.focus_named(View::Worktrees),
            Action::FocusSubmodules => self.focus_named(View::Submodules),
            Action::FocusBranches => self.focus_named(View::Branches),
            Action::FocusRemotes => self.focus_named(View::Remotes),
            Action::FocusTags => self.focus_named(View::Tags),
            Action::FocusCommits => self.focus_named(View::Commits),
            Action::FocusReflog => self.focus_named(View::Reflog),
            Action::FocusStash => self.focus_named(View::Stash),
            Action::FocusMain | Action::CommitFileView => {
                self.window = Window::Main;
                Update::redraw()
            }
            Action::NextTab | Action::PreviousTab => {
                let window = if self.window == Window::Main {
                    self.last_side
                } else {
                    self.window
                };
                let tabs = self.tabs(window).len();
                if tabs > 1 {
                    self.remote = None;
                    if self
                        .sub_commits
                        .as_ref()
                        .is_some_and(|sub| sub.window == window)
                    {
                        self.sub_commits = None;
                    }
                    if self
                        .commit_files
                        .as_ref()
                        .is_some_and(|files| files.window == window)
                    {
                        self.commit_files = None;
                    }
                    let current = self.tabs.get(&window).copied().unwrap_or(0);
                    let next = if action == Action::NextTab {
                        (current + 1) % tabs
                    } else {
                        (current + tabs - 1) % tabs
                    };
                    self.tabs.insert(window, next);
                }
                Update::redraw()
            }
            Action::PanelWider | Action::PanelNarrower => self.resize_panel(
                crate::panels::ROOT,
                0,
                if action == Action::PanelWider { 2 } else { -2 },
            ),
            Action::PanelTaller | Action::PanelShorter => {
                let grow = action == Action::PanelTaller;
                self.resize_focused_panel(if grow { 1 } else { -1 })
            }
            Action::PanelCollapse => self.toggle_collapsed(self.last_side),
            Action::PanelClose => self.close_window(self.last_side),
            Action::PanelMaximize => self.toggle_maximized(self.window),
            Action::PanelReset => self.reset_layout(),
            Action::LayoutMenu => self.layout_menu(),
            Action::Help => self.help(),
            Action::Palette => self.palette(),
            Action::Refresh => Update::redraw().maybe(self.refresh()),
            Action::Push => self.push_menu(),
            Action::Pull => self.run_ops(vec![Op::Pull { rebase: None }]),
            Action::Fetch => self.run_ops(vec![Op::Fetch(None)]),
            Action::OperationMenu => self.operation_menu(),
            Action::Undo => self.undo(),
            Action::Redo => self.redo(),
            Action::ScreenModeNext => {
                self.screen_mode = match self.screen_mode {
                    ScreenMode::Normal => ScreenMode::Half,
                    _ => ScreenMode::Full,
                };
                Update::redraw()
            }
            Action::ScreenModePrevious => {
                self.screen_mode = match self.screen_mode {
                    ScreenMode::Full => ScreenMode::Half,
                    _ => ScreenMode::Normal,
                };
                Update::redraw()
            }
            Action::ScrollMainUp | Action::ScrollMainDown => {
                let list = crate::scroll::main_list(self.main_section.min(1));
                let half = (self.list_page(&list) / 2).max(1);
                self.scroll_list(
                    &list,
                    if action == Action::ScrollMainUp {
                        -half
                    } else {
                        half
                    },
                );
                Update::redraw()
            }
            Action::DiffContextMore | Action::DiffContextLess => {
                self.cx.context = if action == Action::DiffContextMore {
                    self.cx.context.saturating_add(1).min(100)
                } else {
                    self.cx.context.saturating_sub(1)
                };
                self.toast(format!("Diff context: {} lines", self.cx.context), false);
                let mut update = Update::redraw();
                if let Some(command) = self.sync_main(true) {
                    update = update.maybe(command);
                }
                update
            }
            Action::DiffingMenu => self.diffing_menu(),
            Action::ShellCommand => {
                self.shell_history.reset();
                self.prompt("Shell command", "", PromptKind::Shell)
            }
            Action::CommandLog => {
                self.command_log = !self.command_log;
                Update::redraw()
            }

            // ----- lists -----
            Action::Up => self.move_cursor(view, -1),
            Action::Down => self.move_cursor(view, 1),
            Action::PageUp => self.move_cursor(view, -self.page()),
            Action::PageDown => self.move_cursor(view, self.page()),
            Action::Top => self.move_cursor(view, isize::MIN / 2),
            Action::Bottom => self.move_cursor(view, isize::MAX / 2),
            Action::Filter => {
                let initial = self.filters.get(&view).cloned().unwrap_or_default();
                self.prompt(
                    format!("Filter {}", view.label()),
                    &initial,
                    PromptKind::Filter,
                )
            }
            Action::CopyToClipboard => self.copy_menu(),

            Action::StatusOpenConfig => {
                // The most specific existing file (the dedicated family outranks bcode.toml);
                // with none, the dedicated user file, ready to be created.
                let paths = crate::config::config_files(
                    self.repo.root(),
                    &crate::config::ConfigOptions::default(),
                );
                let path = paths
                    .git
                    .iter()
                    .rev()
                    .chain(paths.bcode.iter().rev())
                    .find(|path| path.is_file())
                    .cloned()
                    .unwrap_or_else(|| {
                        let path = crate::config::user_git_config_file();
                        if let Some(parent) = path.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        path
                    });
                self.launch(External {
                    command: format!("{} {}", editor(self), quote(&path.to_string_lossy())),
                    cwd: self.repo.root().to_owned(),
                    pause: false,
                })
            }

            // ----- files -----
            Action::FileToggleStaged => self.toggle_staged(),
            Action::FileStageAll => {
                let any_unstaged = self
                    .snapshot
                    .status
                    .entries
                    .iter()
                    .any(bcode_git::models::StatusEntry::has_unstaged);
                self.run_ops(vec![if any_unstaged {
                    Op::StageAll
                } else {
                    Op::UnstageAll
                }])
            }
            Action::FileEnter => self.enter_file(),
            Action::FileCommit | Action::FileCommitNoVerify | Action::StageCommit => {
                // The editor always opens; whether there is anything to commit is checked when
                // the message is submitted, against the status at that moment.
                let entries = &self.snapshot.status.entries;
                let stage_all = !entries
                    .iter()
                    .any(bcode_git::models::StatusEntry::has_staged);
                let prefill = crate::commit_message::find(
                    &self.repo,
                    &self.commit_sources,
                    self.env_message.take(),
                );
                for error in &prefill.errors {
                    self.toast(error.clone(), true);
                }
                let title = self.commit_title(&prefill);
                let initial = prefill
                    .chosen
                    .as_ref()
                    .map(|(_, text)| text.clone())
                    .unwrap_or_default();
                let prefill = prefill.chosen;
                self.prompt(
                    title,
                    &initial,
                    PromptKind::Commit {
                        amend: false,
                        no_verify: action == Action::FileCommitNoVerify,
                        stage_all,
                        prefill,
                    },
                )
            }
            Action::FileAmend => self.confirm_if(
                true,
                "Amend last commit",
                "Add the staged changes to the last commit, keeping its message?".into(),
                Then::op(Op::Commit(
                    CommitOptions {
                        amend: true,
                        no_edit: true,
                        allow_empty: true,
                        ..CommitOptions::default()
                    },
                    DraftCleanup::default(),
                )),
            ),
            Action::FileCommitEditor => self.launch(External {
                command: "git commit".into(),
                cwd: self.repo.root().to_owned(),
                pause: false,
            }),
            Action::FileDiscard => self.discard_menu(),
            Action::FileResetMenu => self.reset_files_menu(),
            Action::FileEdit => {
                let external = self
                    .selected_entry()
                    .map(|entry| edit_command(self, &entry.path, None));
                external.map_or_else(Update::none, |external| self.launch(external))
            }
            Action::FileOpen => {
                let external = self
                    .selected_entry()
                    .map(|entry| open_command(self, &entry.path));
                external.map_or_else(Update::none, |external| self.launch(external))
            }
            Action::FileIgnore => {
                self.selected_files_pattern()
                    .map_or_else(Update::none, |pattern| {
                        self.open(Popup::Menu(Menu::new(
                            "Ignore",
                            vec![
                                MenuItem::new(
                                    'i',
                                    format!("Add {pattern} to .gitignore"),
                                    Then::op(Op::Ignore(pattern.clone())),
                                ),
                                MenuItem::new(
                                    'e',
                                    format!("Add {pattern} to .git/info/exclude"),
                                    Then::op(Op::Exclude(pattern)),
                                ),
                            ],
                        )))
                    })
            }
            Action::FileStash => self.prompt(
                "Stash message",
                "",
                PromptKind::StashMessage(StashOptions {
                    include_untracked: true,
                    ..StashOptions::default()
                }),
            ),
            Action::FileStashMenu => self.stash_menu(),
            Action::FileToggleTree => {
                self.file_tree = !self.file_tree;
                self.rebuild_files();
                Update::redraw()
            }
            Action::FileCollapseAll => {
                for node in &self.file_nodes.clone() {
                    if let FileNode::Dir { path, .. } = node {
                        self.collapsed.insert(path.clone());
                    }
                }
                self.rebuild_files();
                Update::redraw()
            }
            Action::FileExpandAll => {
                self.collapsed.clear();
                self.rebuild_files();
                Update::redraw()
            }
            Action::FileConflictOurs | Action::FileConflictTheirs => match self.selected_entry() {
                Some(entry) if entry.is_conflicted() => {
                    let path = entry.path.clone();
                    self.run_ops(vec![Op::ResolveConflict {
                        path,
                        side: if action == Action::FileConflictOurs {
                            ConflictSide::Ours
                        } else {
                            ConflictSide::Theirs
                        },
                    }])
                }
                _ => {
                    self.toast("Select a conflicted file", true);
                    Update::redraw()
                }
            },

            // ----- worktrees and submodules -----
            Action::WorktreeSwitch => match self.selected(View::Worktrees) {
                Some(Row::Worktree(index)) => {
                    let path = self.snapshot.worktrees[index].path.clone();
                    match bcode_git::Repository::discover(&path, self.repo.runner().log().clone()) {
                        Ok(repo) => {
                            self.repo = repo;
                            self.toast(format!("Switched to {}", path.display()), false);
                            self.main_requested = None;
                            Update::redraw().maybe(self.refresh())
                        }
                        Err(error) => {
                            self.toast(error.message(), true);
                            Update::redraw()
                        }
                    }
                }
                _ => Update::none(),
            },
            Action::WorktreeNew => {
                self.prompt("Branch for new worktree", "", PromptKind::WorktreeBranch)
            }
            Action::WorktreeRemove => match self.selected(View::Worktrees) {
                Some(Row::Worktree(index)) if !self.snapshot.worktrees[index].is_main => {
                    let path = self.snapshot.worktrees[index].path.clone();
                    self.confirm_if(
                        true,
                        "Remove worktree",
                        format!("Remove worktree {}?", path.display()),
                        Then::op(Op::RemoveWorktree { path, force: false }),
                    )
                }
                _ => Update::none(),
            },
            Action::SubmoduleEnter => match self.selected(View::Submodules) {
                Some(Row::Submodule(index)) => {
                    let path = self.repo.root().join(&self.snapshot.submodules[index].path);
                    self.launch(External {
                        command: format!(
                            "{} -C {}",
                            std::env::current_exe().map_or_else(
                                |_| "bcode-git".into(),
                                |exe| quote(&exe.to_string_lossy())
                            ),
                            quote(&path.to_string_lossy())
                        ),
                        cwd: path,
                        pause: false,
                    })
                }
                _ => Update::none(),
            },
            Action::SubmoduleUpdate => self.run_ops(vec![Op::UpdateSubmodules]),

            // ----- branches -----
            Action::BranchCheckout => match self.selected(View::Branches) {
                Some(Row::Branch(index)) => {
                    let name = self.snapshot.branches[index].name.clone();
                    self.run_ops(vec![Op::Checkout(name)])
                }
                _ => Update::none(),
            },
            Action::BranchNew => {
                let start = self.selected_ref();
                self.prompt(
                    format!("New branch from {}", start.as_deref().unwrap_or("HEAD")),
                    "",
                    PromptKind::NewBranch { start },
                )
            }
            Action::BranchDelete => self.delete_branch(),
            Action::BranchRename => match self.selected(View::Branches) {
                Some(Row::Branch(index)) => {
                    let name = self.snapshot.branches[index].name.clone();
                    self.prompt(
                        "Rename branch",
                        &name.clone(),
                        PromptKind::RenameBranch(name),
                    )
                }
                _ => Update::none(),
            },
            Action::BranchMerge | Action::RemoteBranchMerge => self
                .selected_ref()
                .map_or_else(Update::none, |target| self.merge_menu(&target)),
            Action::BranchRebase | Action::RemoteBranchRebase => {
                let current = self.current_branch().unwrap_or_else(|| "HEAD".into());
                self.selected_ref().map_or_else(Update::none, |target| {
                    self.confirm_if(
                        true,
                        "Rebase",
                        format!("Rebase {current} onto {target}?"),
                        Then::op(Op::Rebase(target)),
                    )
                })
            }
            Action::BranchCheckoutByName => {
                self.prompt("Checkout branch", "", PromptKind::CheckoutByName)
            }
            Action::BranchFastForward => match self.selected(View::Branches) {
                Some(Row::Branch(index)) => {
                    let branch = self.snapshot.branches[index].clone();
                    self.run_ops(vec![Op::FastForward(branch)])
                }
                _ => Update::none(),
            },
            Action::BranchUpstream => self.upstream_menu(),
            Action::BranchTag | Action::CommitTag => {
                self.selected_ref().map_or_else(Update::none, |target| {
                    self.prompt(format!("Tag {target}"), "", PromptKind::NewTag(target))
                })
            }
            Action::BranchViewCommits
            | Action::RemoteBranchViewCommits
            | Action::TagViewCommits => self.selected_ref().map_or_else(Update::none, |revision| {
                let command = self.load_sub_commits(revision.clone(), revision);
                Update::redraw().maybe(command)
            }),
            Action::BranchCopyName => self
                .selected_ref()
                .map_or_else(Update::none, |name| self.run_ops(vec![Op::Copy(name)])),

            // ----- remotes -----
            Action::RemoteView => match self.selected(View::Remotes) {
                Some(Row::Remote(index)) => {
                    self.remote = Some(self.snapshot.remotes[index].name.clone());
                    self.cursors.insert(View::RemoteBranches, 0);
                    Update::redraw()
                }
                _ => Update::none(),
            },
            Action::RemoteNew => self.prompt("Remote name", "", PromptKind::RemoteName),
            Action::RemoteDelete => match self.selected(View::Remotes) {
                Some(Row::Remote(index)) => {
                    let name = self.snapshot.remotes[index].name.clone();
                    self.confirm_if(
                        true,
                        "Remove remote",
                        format!("Remove remote {name}?"),
                        Then::op(Op::RemoveRemote(name)),
                    )
                }
                _ => Update::none(),
            },
            Action::RemoteEdit => match self.selected(View::Remotes) {
                Some(Row::Remote(index)) => {
                    let remote = self.snapshot.remotes[index].clone();
                    self.prompt(
                        format!("URL for {}", remote.name),
                        remote.fetch_url.as_deref().unwrap_or_default(),
                        PromptKind::EditRemoteUrl(remote.name),
                    )
                }
                _ => Update::none(),
            },
            Action::RemoteFetch => match self.selected(View::Remotes) {
                Some(Row::Remote(index)) => {
                    let name = self.snapshot.remotes[index].name.clone();
                    self.run_ops(vec![Op::Fetch(Some(name))])
                }
                _ => Update::none(),
            },
            Action::RemoteBranchCheckout => match self.selected(View::RemoteBranches) {
                Some(Row::RemoteBranch(index)) => {
                    let branch = self.snapshot.remote_branches[index].clone();
                    if self
                        .snapshot
                        .branches
                        .iter()
                        .any(|local| local.name == branch.name)
                    {
                        let name = branch.name.clone();
                        self.prompt(
                            format!("{name} exists locally; name for new branch"),
                            &format!("{}-{}", branch.remote, name),
                            PromptKind::RemoteCheckoutName(branch),
                        )
                    } else {
                        self.run_ops(vec![Op::CheckoutRemote(branch)])
                    }
                }
                _ => Update::none(),
            },
            Action::RemoteBranchDelete => match self.selected(View::RemoteBranches) {
                Some(Row::RemoteBranch(index)) => {
                    let branch = self.snapshot.remote_branches[index].clone();
                    self.confirm_if(
                        true,
                        "Delete remote branch",
                        format!("Delete {} on the remote?", branch.full_name()),
                        Then::op(Op::DeleteRemoteBranch {
                            remote: branch.remote,
                            name: branch.name,
                        }),
                    )
                }
                _ => Update::none(),
            },
            Action::RemoteBranchUpstream => {
                match (self.selected(View::RemoteBranches), self.current_branch()) {
                    (Some(Row::RemoteBranch(index)), Some(branch)) => {
                        let upstream = self.snapshot.remote_branches[index].full_name();
                        self.run_ops(vec![Op::SetUpstream {
                            branch,
                            upstream: Some(upstream),
                        }])
                    }
                    _ => Update::none(),
                }
            }

            // ----- tags -----
            Action::TagCheckout => self
                .selected_ref()
                .map_or_else(Update::none, |tag| self.run_ops(vec![Op::Checkout(tag)])),
            Action::TagNew => self.prompt("New tag at HEAD", "", PromptKind::NewTag("HEAD".into())),
            Action::TagDelete => match self.selected(View::Tags) {
                Some(Row::Tag(index)) => {
                    let name = self.snapshot.tags[index].name.clone();
                    self.confirm_if(
                        self.config.confirm.delete,
                        "Delete tag",
                        format!("Delete tag {name}?"),
                        Then::op(Op::DeleteTag(name)),
                    )
                }
                _ => Update::none(),
            },
            Action::TagPush => match self.selected(View::Tags) {
                Some(Row::Tag(index)) => {
                    let name = self.snapshot.tags[index].name.clone();
                    let items = self
                        .snapshot
                        .remotes
                        .iter()
                        .enumerate()
                        .map(|(position, remote)| {
                            MenuItem::new(
                                hotkey(position),
                                format!("Push to {}", remote.name),
                                Then::op(Op::PushTag {
                                    remote: remote.name.clone(),
                                    name: name.clone(),
                                }),
                            )
                        })
                        .collect();
                    self.open(Popup::Menu(Menu::new(format!("Push tag {name}"), items)))
                }
                _ => Update::none(),
            },

            // ----- commits -----
            Action::CommitViewFiles | Action::ReflogViewFiles => {
                let oid = match self.selected(view) {
                    Some(Row::Reflog(index)) => Some(self.snapshot.reflog[index].oid.clone()),
                    _ => self.selected_commit().map(|commit| commit.oid),
                };
                oid.map_or_else(Update::none, |oid| {
                    let title = oid.chars().take(8).collect::<String>();
                    let command =
                        self.load_commit_files(DiffTarget::Commit(oid.clone()), oid, title);
                    Update::redraw().maybe(command)
                })
            }
            Action::CommitCheckout | Action::ReflogCheckout => {
                let oid = match self.selected(view) {
                    Some(Row::Reflog(index)) => Some(self.snapshot.reflog[index].oid.clone()),
                    _ => self.selected_commit().map(|commit| commit.oid),
                };
                oid.map_or_else(Update::none, |oid| {
                    self.confirm_if(
                        true,
                        "Checkout commit",
                        format!("Check out {} (detached HEAD)?", &oid[..oid.len().min(10)]),
                        Then::op(Op::Checkout(oid)),
                    )
                })
            }
            Action::CommitSquash => self.rewrite(RebaseAction::Squash),
            Action::CommitFixup => self.rewrite(RebaseAction::Fixup),
            Action::CommitDrop => self.rewrite(RebaseAction::Drop),
            Action::CommitEdit => self.rewrite(RebaseAction::Edit),
            Action::CommitMoveDown => self.move_commit(1),
            Action::CommitMoveUp => self.move_commit(-1),
            Action::CommitReword => match self.selected_commit() {
                Some(commit) if view == View::Commits => {
                    let message = self
                        .repo
                        .commit_message(&commit.oid)
                        .unwrap_or(commit.summary);
                    self.prompt("Reword commit", &message, PromptKind::Reword(commit.oid))
                }
                _ => Update::none(),
            },
            Action::CommitCreateFixup => match self.selected_commit() {
                Some(commit) => self.run_ops(vec![Op::CreateFixup(commit.oid)]),
                None => Update::none(),
            },
            Action::CommitAutosquash => match self.selected_commit() {
                Some(commit) if view == View::Commits => {
                    let base = if commit.parents.is_empty() {
                        "--root".to_owned()
                    } else {
                        format!("{}^", commit.oid)
                    };
                    self.run_ops(vec![Op::Autosquash(base)])
                }
                _ => Update::none(),
            },
            Action::CommitAmendInto => match self.selected_commit() {
                Some(commit) if view == View::Commits => self.confirm_if(
                    true,
                    "Amend into commit",
                    format!(
                        "Amend the staged changes into {} {}?",
                        commit.short_oid, commit.summary
                    ),
                    Then::op(Op::AmendInto(commit.oid)),
                ),
                _ => Update::none(),
            },
            Action::CommitRevert => match self.selected_commit() {
                Some(commit) => self.confirm_if(
                    true,
                    "Revert commit",
                    format!("Revert {} {}?", commit.short_oid, commit.summary),
                    Then::op(Op::Revert(vec![commit.oid])),
                ),
                None => Update::none(),
            },
            Action::CommitCherryPickCopy => {
                if let Some(commit) = self.selected_commit() {
                    if let Some(position) = self
                        .cherry
                        .iter()
                        .position(|copied| copied.oid == commit.oid)
                    {
                        self.cherry.remove(position);
                    } else {
                        self.cherry.push(commit);
                    }
                    self.toast(
                        format!("{} commit(s) copied for cherry-pick", self.cherry.len()),
                        false,
                    );
                }
                self.move_cursor(view, 1)
            }
            Action::CommitCherryPickPaste => {
                if self.cherry.is_empty() {
                    self.toast("Copy commits with C first", true);
                    return Update::redraw();
                }
                // Apply oldest first.
                let mut oids: Vec<String> = self
                    .cherry
                    .iter()
                    .map(|commit| commit.oid.clone())
                    .collect();
                oids.reverse();
                self.cherry.clear();
                self.confirm_if(
                    true,
                    "Cherry-pick",
                    format!("Cherry-pick {} commit(s) onto HEAD?", oids.len()),
                    Then::op(Op::CherryPick(oids)),
                )
            }
            Action::CommitResetMenu | Action::ReflogResetMenu => {
                let target = match self.selected(view) {
                    Some(Row::Reflog(index)) => Some(self.snapshot.reflog[index].oid.clone()),
                    _ => self.selected_commit().map(|commit| commit.oid),
                };
                target.map_or_else(Update::none, |target| self.reset_menu(&target))
            }
            Action::CommitNewBranch => match self.selected_commit() {
                Some(commit) => self.prompt(
                    format!("New branch at {}", commit.short_oid),
                    "",
                    PromptKind::NewBranch {
                        start: Some(commit.oid),
                    },
                ),
                None => Update::none(),
            },
            Action::CommitBisect => self.bisect_menu(),
            Action::CommitCopySha => match self.selected_commit() {
                Some(commit) => self.run_ops(vec![Op::Copy(commit.oid)]),
                None => Update::none(),
            },
            Action::CommitToggleGraph => {
                self.cx.show_graph = !self.cx.show_graph;
                Update::redraw()
            }
            Action::CommitLoadMore => {
                if self.snapshot.more_commits {
                    Update::redraw().maybe(self.load_more_commits())
                } else {
                    Update::none()
                }
            }

            // ----- commit files -----
            Action::CommitFileCheckout => {
                match (self.selected(View::CommitFiles), &self.commit_files) {
                    (Some(Row::CommitFile(index)), Some(files)) => {
                        let path = files.files[index].path.clone();
                        let revision = files.revision.clone();
                        self.confirm_if(
                        true,
                        "Checkout file",
                        format!("Replace {path} in the worktree and index with its version from {revision}?"),
                        Then::op(Op::CheckoutPath { revision, path }),
                    )
                    }
                    _ => Update::none(),
                }
            }
            Action::CommitFileTogglePatch => {
                if let (Some(Row::CommitFile(index)), Some(files)) =
                    (self.selected(View::CommitFiles), self.commit_files.as_mut())
                {
                    let path = files.files[index].path.clone();
                    if !files.patch.remove(&path) {
                        files.patch.insert(path);
                    }
                }
                self.move_cursor(View::CommitFiles, 1)
            }
            Action::CommitFilePatchMenu => self.patch_menu(),

            // ----- stash -----
            Action::StashApply | Action::StashPop | Action::StashDrop => {
                match self.selected(View::Stash) {
                    Some(Row::Stash(index)) => {
                        let selector = self.snapshot.stashes[index].selector();
                        match action {
                            Action::StashApply => self.run_ops(vec![Op::StashApply {
                                selector,
                                pop: false,
                            }]),
                            Action::StashPop => self.run_ops(vec![Op::StashApply {
                                selector,
                                pop: true,
                            }]),
                            _ => self.confirm_if(
                                self.config.confirm.delete,
                                "Drop stash",
                                format!("Drop {selector}?"),
                                Then::op(Op::StashDrop(selector)),
                            ),
                        }
                    }
                    _ => Update::none(),
                }
            }
            Action::StashRename => match self.selected(View::Stash) {
                Some(Row::Stash(index)) => {
                    let entry = self.snapshot.stashes[index].clone();
                    self.prompt(
                        "Rename stash",
                        &entry.message.clone(),
                        PromptKind::RenameStash(entry),
                    )
                }
                _ => Update::none(),
            },
            Action::StashBranch => match self.selected(View::Stash) {
                Some(Row::Stash(index)) => {
                    let selector = self.snapshot.stashes[index].selector();
                    self.prompt("Branch name", "", PromptKind::StashBranch(selector))
                }
                _ => Update::none(),
            },
            Action::StashViewFiles => match self.selected(View::Stash) {
                Some(Row::Stash(index)) => {
                    let selector = self.snapshot.stashes[index].selector();
                    let command = self.load_commit_files(
                        DiffTarget::Stash(selector.clone()),
                        selector.clone(),
                        selector,
                    );
                    Update::redraw().maybe(command)
                }
                _ => Update::none(),
            },

            // ----- main view: a page without a cursor -----
            Action::MainLineUp | Action::MainLineDown => {
                let list = crate::scroll::main_list(self.main_section.min(1));
                self.scroll_list(&list, if action == Action::MainLineUp { -1 } else { 1 });
                Update::redraw()
            }
            Action::MainPageUp | Action::MainPageDown => {
                let list = crate::scroll::main_list(self.main_section.min(1));
                let page = self.list_page(&list);
                self.scroll_list(
                    &list,
                    if action == Action::MainPageUp {
                        -page
                    } else {
                        page
                    },
                );
                Update::redraw()
            }
            Action::MainTop | Action::MainBottom => {
                let list = crate::scroll::main_list(self.main_section.min(1));
                self.scroll_list_to(
                    &list,
                    if action == Action::MainTop {
                        0
                    } else {
                        u64::MAX
                    },
                );
                Update::redraw()
            }
            Action::MainNextHunk | Action::MainPreviousHunk => {
                self.scroll_to_hunk(
                    self.main_section.min(1),
                    if action == Action::MainNextHunk {
                        1
                    } else {
                        -1
                    },
                );
                Update::redraw()
            }

            // ----- staging: the cursor picks lines -----
            Action::StageLineUp => {
                self.move_main(-1);
                self.snap_to_change(-1);
                Update::redraw()
            }
            Action::StageLineDown => {
                self.move_main(1);
                self.snap_to_change(1);
                Update::redraw()
            }
            Action::StageNextHunk => {
                self.jump_hunk(1);
                Update::redraw()
            }
            Action::StagePreviousHunk => {
                self.jump_hunk(-1);
                Update::redraw()
            }
            Action::StageToggle => self.stage_selection(false),
            Action::StageDiscard => self.stage_selection(true),
            Action::StageRange => {
                if let Some(staging) = &mut self.staging {
                    staging.anchor = match staging.anchor {
                        Some(_) => None,
                        None => Some(self.main_cursor[self.main_section.min(1)]),
                    };
                    staging.hunk_mode = false;
                }
                Update::redraw()
            }
            Action::StageHunkMode => {
                if let Some(staging) = &mut self.staging {
                    staging.hunk_mode = !staging.hunk_mode;
                    staging.anchor = None;
                }
                Update::redraw()
            }
            Action::StageSwitchSide => {
                let sections = self.main.as_ref().map_or(0, |main| main.sections.len());
                if sections > 1 {
                    self.main_section = (self.main_section + 1) % sections.min(2);
                    if let Some(staging) = &mut self.staging {
                        staging.anchor = None;
                    }
                    self.snap_to_change(1);
                }
                Update::redraw()
            }
            Action::StageEdit => {
                let line = self
                    .main
                    .as_ref()
                    .and_then(|main| main.sections.get(self.main_section))
                    .and_then(|section| {
                        section.rows.get(self.main_cursor[self.main_section.min(1)])
                    })
                    .and_then(|row| row.new_line);
                let external = self
                    .selected_entry()
                    .map(|entry| edit_command(self, &entry.path, line));
                external.map_or_else(Update::none, |external| self.launch(external))
            }

            // Popup-only actions are handled by popup routing.
            Action::MenuUp
            | Action::MenuDown
            | Action::MenuConfirm
            | Action::MenuClose
            | Action::InputSubmit
            | Action::InputCancel
            | Action::InputToggleAmend
            | Action::InputSwitchField
            | Action::InputSubmitFromAnyField
            | Action::InputClear
            | Action::ConfirmYes
            | Action::ConfirmNo => Update::none(),
        };
        if let Some(command) = self.sync_main(false) {
            update = update.maybe(command);
        }
        update
    }

    fn page(&self) -> isize {
        isize::try_from(self.size.get().1 / 3).unwrap_or(10).max(1)
    }

    /// Commit editor title: where the text came from, what it replaces, and what will be
    /// committed.
    pub(crate) fn commit_title(&self, prefill: &crate::commit_message::Prefill) -> String {
        let entries = &self.snapshot.status.entries;
        let mut notes: Vec<String> = Vec::new();
        if let Some((source, _)) = &prefill.chosen {
            notes.push(format!("from {}", source.label()));
        }
        if let Some(replaced) = prefill.replaced {
            notes.push(format!("replaces {}", replaced.label()));
        }
        if entries.is_empty() {
            notes.push("no changes yet".into());
        } else if !entries
            .iter()
            .any(bcode_git::models::StatusEntry::has_staged)
        {
            notes.push("nothing staged: commits all changes".into());
        }
        if notes.is_empty() {
            "Commit message".to_owned()
        } else {
            format!("Commit message ({})", notes.join("; "))
        }
    }

    /// Keep an unsent commit message as the draft (or drop the draft when cleared). Untouched
    /// text prefilled from git's own files is not copied into a draft.
    pub(crate) fn save_unsent_commit(&mut self, prompt: &Prompt) {
        let PromptKind::Commit {
            amend: false,
            prefill,
            ..
        } = &prompt.kind
        else {
            return;
        };
        if !self.config.commit.save_draft {
            return;
        }
        let text = prompt.text();
        let untouched = prefill.as_ref().is_some_and(|(source, original)| {
            *original == text && *source != CommitMessageSource::Env
        });
        if untouched {
            return;
        }
        if let Err(error) = self.repo.save_commit_draft(&text) {
            self.toast(
                format!("Could not save the draft: {}", error.message()),
                true,
            );
        }
    }

    /// Grow (positive) or shrink the focused panel along its column.
    fn resize_focused_panel(&mut self, delta: i32) -> Update<Msg> {
        match self.window {
            Window::Side(index) => {
                let Some(position) = self.panels.position(index) else {
                    return Update::none();
                };
                // Move the divider below the window, or above it for the last one.
                if position + 1 < self.panels.visible.len() {
                    self.resize_panel(crate::panels::SIDE, position, delta)
                } else if position > 0 {
                    self.resize_panel(crate::panels::SIDE, position - 1, -delta)
                } else {
                    Update::none()
                }
            }
            Window::Main if self.command_log => self.resize_panel(crate::panels::MAIN, 0, delta),
            Window::Main => Update::none(),
        }
    }

    /// Layout actions, including reopening closed side windows.
    fn layout_menu(&mut self) -> Update<Msg> {
        let window = self.last_side;
        let name = |app: &Self, index: usize| {
            app.layout.windows[index]
                .iter()
                .map(|view| view.label())
                .collect::<Vec<_>>()
                .join(" · ")
        };
        let Window::Side(focused) = window else {
            return Update::none();
        };
        let collapsed = self.panels.collapsed.contains(&focused);
        let mut items = vec![
            MenuItem::new(
                'z',
                format!(
                    "{} {}",
                    if collapsed { "Expand" } else { "Collapse" },
                    name(self, focused)
                ),
                Then::Action(Action::PanelCollapse),
            ),
            MenuItem::new(
                'o',
                if self.screen_mode == ScreenMode::Full {
                    "Restore panels".to_owned()
                } else {
                    "Maximize the focused panel".to_owned()
                },
                Then::Action(Action::PanelMaximize),
            ),
            MenuItem::new(
                'c',
                format!("Close {}", name(self, focused)),
                Then::Action(Action::PanelClose),
            ),
        ];
        for (number, index) in self.panels.closed.clone().into_iter().enumerate() {
            let label = format!("Show {}", name(self, index));
            let then = Then::Layout(crate::panels::LayoutCommand::Reopen(index));
            items.push(
                match char::from_digit(u32::try_from(number + 1).unwrap_or(0), 10) {
                    Some(digit) if number < 9 => MenuItem::new(digit, label, then),
                    _ => MenuItem::plain(label, "", then),
                },
            );
        }
        items.push(MenuItem::new(
            '=',
            "Reset layout",
            Then::Layout(crate::panels::LayoutCommand::Reset),
        ));
        self.open(Popup::Menu(Menu::new("Layout", items)))
    }

    fn focus_named(&mut self, view: View) -> Update<Msg> {
        if !self.focus_view(view) {
            self.toast(
                format!("The {} view is not in the layout", view.label()),
                true,
            );
        }
        Update::redraw()
    }

    fn back(&mut self) -> Update<Msg> {
        if self.window == Window::Main {
            if let Some(staging) = &mut self.staging
                && (staging.anchor.is_some() || staging.hunk_mode)
            {
                staging.anchor = None;
                staging.hunk_mode = false;
                return Update::redraw();
            }
            self.staging = None;
            self.window = self.last_side;
            return Update::redraw();
        }
        let view = self.current_view();
        if self.filters.remove(&view).is_some() {
            return Update::redraw();
        }
        if view == View::CommitFiles {
            self.commit_files = None;
        } else if view == View::SubCommits {
            self.sub_commits = None;
        } else if view == View::RemoteBranches {
            self.remote = None;
        } else if self.diff_base.take().is_some() {
            self.toast("Left diffing mode", false);
        } else if self.screen_mode != self.config.layout.screen_mode {
            self.screen_mode = self.config.layout.screen_mode;
        }
        Update::redraw()
    }

    fn selected_ref(&self) -> Option<String> {
        match self.selected(self.current_view())? {
            Row::Branch(index) => Some(self.snapshot.branches[index].name.clone()),
            Row::RemoteBranch(index) => Some(self.snapshot.remote_branches[index].full_name()),
            Row::Tag(index) => Some(self.snapshot.tags[index].name.clone()),
            Row::Commit(index) => Some(self.snapshot.commits[index].oid.clone()),
            Row::SubCommit(index) => self
                .sub_commits
                .as_ref()
                .map(|sub| sub.commits[index].oid.clone()),
            Row::Reflog(index) => Some(self.snapshot.reflog[index].oid.clone()),
            _ => None,
        }
    }

    fn selected_files_pattern(&self) -> Option<String> {
        match self.selected(View::Files)? {
            Row::File(index) => Some(match &self.file_nodes[index] {
                FileNode::Dir { path, .. } => format!("{path}/"),
                FileNode::File { entry, .. } => self.snapshot.status.entries[*entry].path.clone(),
            }),
            _ => None,
        }
    }

    fn toggle_staged(&mut self) -> Update<Msg> {
        let entries = self.selected_entries();
        if entries.is_empty() {
            return Update::none();
        }
        if entries.iter().any(|entry| entry.is_conflicted()) {
            let paths: Vec<String> = entries.iter().map(|entry| entry.path.clone()).collect();
            return self.confirm_if(
                true,
                "Mark resolved",
                "Stage the conflicted file(s) as resolved?".into(),
                Then::op(Op::Stage(paths)),
            );
        }
        let any_unstaged = entries.iter().any(|entry| entry.has_unstaged());
        let paths: Vec<String> = entries
            .iter()
            .flat_map(|entry| {
                let mut paths = vec![entry.path.clone()];
                if !any_unstaged && let Some(orig) = &entry.orig_path {
                    paths.push(orig.clone());
                }
                paths
            })
            .collect();
        let op = if any_unstaged {
            Op::Stage(paths)
        } else {
            Op::Unstage(paths)
        };
        let update = self.run_ops(vec![op]);
        update.merge(self.move_cursor(View::Files, 1))
    }

    fn enter_file(&mut self) -> Update<Msg> {
        match self.selected(View::Files) {
            Some(Row::File(index)) => match self.file_nodes[index].clone() {
                FileNode::Dir { path, .. } => {
                    if !self.collapsed.remove(&path) {
                        self.collapsed.insert(path);
                    }
                    self.rebuild_files();
                    Update::redraw()
                }
                FileNode::File { .. } => {
                    let stageable = self.main.as_ref().is_some_and(|main| {
                        main.sections
                            .iter()
                            .any(|section| section.kind != SectionKind::Plain)
                    });
                    if !stageable {
                        return Update::none();
                    }
                    self.window = Window::Main;
                    self.staging = Some(Staging {
                        hunk_mode: self.config.staging.hunk_mode,
                        ..Staging::default()
                    });
                    self.main_section = 0;
                    self.snap_to_change(1);
                    Update::redraw()
                }
            },
            _ => Update::none(),
        }
    }

    fn jump_hunk(&mut self, direction: isize) {
        let section = self.main_section.min(1);
        let Some(rows) = self
            .main
            .as_ref()
            .and_then(|main| main.sections.get(section))
            .map(|section| &section.rows)
        else {
            return;
        };
        let cursor = self.main_cursor[section];
        let current = rows.get(cursor).and_then(|row| row.hunk.zip(row.file));
        let found = if direction > 0 {
            (cursor + 1..rows.len()).find(|index| {
                rows[*index].kind == crate::diffview::RowKind::HunkHeader
                    && rows[*index].hunk.zip(rows[*index].file) != current
            })
        } else {
            (0..cursor).rev().find(|index| {
                rows[*index].kind == crate::diffview::RowKind::HunkHeader
                    && rows[*index].hunk.zip(rows[*index].file) != current
            })
        };
        if let Some(index) = found {
            self.main_cursor[section] = index;
            if self.staging.is_some() {
                self.snap_to_change(1);
            }
        }
    }

    fn stage_selection(&mut self, discard: bool) -> Update<Msg> {
        let ids = self.staging_selection();
        if ids.is_empty() {
            return Update::none();
        }
        let Some(section) = self
            .main
            .as_ref()
            .and_then(|main| main.sections.get(self.main_section))
        else {
            return Update::none();
        };
        let Some(file) = section.files.first().cloned() else {
            return Update::none();
        };
        let kind = section.kind;
        if let Some(staging) = &mut self.staging {
            staging.anchor = None;
        }
        let untracked = self
            .selected_entry()
            .filter(|entry| entry.is_untracked())
            .map(|entry| entry.path.clone());
        match (kind, discard) {
            (SectionKind::Unstaged, false) => self.run_ops(vec![Op::StageLines {
                file,
                ids,
                intent_to_add: untracked,
            }]),
            (SectionKind::Staged, false) => self.run_ops(vec![Op::UnstageLines { file, ids }]),
            (SectionKind::Unstaged, true) if untracked.is_none() => {
                let count = ids.len();
                self.confirm_if(
                    self.config.confirm.discard,
                    "Discard lines",
                    format!("Discard {count} selected line(s) from the working tree?"),
                    Then::op(Op::DiscardLines { file, ids }),
                )
            }
            _ => {
                self.toast("Discard works on unstaged lines of tracked files", true);
                Update::redraw()
            }
        }
    }

    fn discard_menu(&mut self) -> Update<Msg> {
        let entries = self.selected_entries();
        if entries.is_empty() {
            return Update::none();
        }
        let tracked: Vec<String> = entries
            .iter()
            .filter(|entry| !entry.is_untracked())
            .flat_map(|entry| std::iter::once(entry.path.clone()).chain(entry.orig_path.clone()))
            .collect();
        let untracked: Vec<String> = entries
            .iter()
            .filter(|entry| entry.is_untracked())
            .map(|entry| entry.path.clone())
            .collect();
        let has_both = entries
            .iter()
            .any(|entry| entry.has_staged() && entry.has_unstaged());
        let label = if entries.len() == 1 {
            entries[0].path.clone()
        } else {
            format!("{} files", entries.len())
        };
        let mut items = vec![MenuItem::new(
            'x',
            "Discard all changes",
            Then::op(Op::Discard {
                tracked: tracked.clone(),
                untracked,
            }),
        )];
        if has_both {
            items.push(MenuItem::new(
                'u',
                "Discard unstaged changes",
                Then::op(Op::DiscardUnstaged(tracked)),
            ));
        }
        if !self.config.confirm.discard && items.len() == 1 {
            return self.apply(items.remove(0).then);
        }
        self.open(Popup::Menu(Menu::new(format!("Discard {label}"), items)))
    }

    fn reset_files_menu(&mut self) -> Update<Msg> {
        let items = vec![
            MenuItem::new('u', "Unstage everything", Then::op(Op::UnstageAll)),
            MenuItem::new(
                'x',
                "Discard all changes (git reset --hard HEAD)",
                Then::Confirm {
                    title: "Discard everything".into(),
                    message: "Reset the index and working tree to HEAD? Untracked files are kept."
                        .into(),
                    then: Box::new(Then::op(Op::Reset {
                        target: "HEAD".into(),
                        mode: ResetMode::Hard,
                    })),
                },
            ),
            MenuItem::new(
                'c',
                "Delete untracked files (git clean -fd)",
                Then::Confirm {
                    title: "Clean".into(),
                    message: "Delete every untracked file and directory?".into(),
                    then: Box::new(Then::op(Op::Shell("git clean -fd".into()))),
                },
            ),
            MenuItem::new(
                's',
                "Stash everything including untracked",
                Then::op(Op::StashPush(StashOptions {
                    include_untracked: true,
                    ..StashOptions::default()
                })),
            ),
        ];
        self.open(Popup::Menu(Menu::new("Reset", items)))
    }

    fn stash_menu(&mut self) -> Update<Msg> {
        let options = |include_untracked: bool, staged: bool, keep_index: bool| StashOptions {
            include_untracked,
            staged,
            keep_index,
            ..StashOptions::default()
        };
        let mut items = vec![
            MenuItem::new(
                'a',
                "Stash all changes",
                Then::Prompt(PromptKind::StashMessage(options(false, false, false))),
            ),
            MenuItem::new(
                'u',
                "Stash all including untracked",
                Then::Prompt(PromptKind::StashMessage(options(true, false, false))),
            ),
            MenuItem::new(
                's',
                "Stash staged changes",
                Then::Prompt(PromptKind::StashMessage(options(false, true, false))),
            ),
            MenuItem::new(
                'k',
                "Stash unstaged changes (keep index)",
                Then::Prompt(PromptKind::StashMessage(options(false, false, true))),
            ),
        ];
        let paths: Vec<String> = self
            .selected_entries()
            .iter()
            .map(|entry| entry.path.clone())
            .collect();
        if !paths.is_empty() {
            items.push(MenuItem::new(
                'f',
                "Stash selected file(s)",
                Then::Prompt(PromptKind::StashMessage(StashOptions {
                    include_untracked: true,
                    paths,
                    ..StashOptions::default()
                })),
            ));
        }
        self.open(Popup::Menu(Menu::new("Stash", items)))
    }

    fn push_menu(&mut self) -> Update<Msg> {
        let status = &self.snapshot.status.branch;
        let Some(branch) = status.head.clone() else {
            self.toast("Cannot push a detached HEAD", true);
            return Update::redraw();
        };
        if status.upstream.is_none() {
            let remotes: Vec<String> = self
                .snapshot
                .remotes
                .iter()
                .map(|remote| remote.name.clone())
                .collect();
            if remotes.is_empty() {
                self.toast("No remotes configured; add one in the Remotes tab", true);
                return Update::redraw();
            }
            let items = remotes
                .into_iter()
                .enumerate()
                .map(|(index, remote)| {
                    MenuItem::new(
                        hotkey(index),
                        format!("Push {branch} to {remote} and set upstream"),
                        Then::op(Op::Push(PushOptions {
                            remote: Some(remote),
                            refspec: Some(branch.clone()),
                            set_upstream: true,
                            ..PushOptions::default()
                        })),
                    )
                })
                .collect();
            return self.open(Popup::Menu(Menu::new("Push", items)));
        }
        if status.behind > 0 {
            let then = Then::op(Op::Push(PushOptions {
                force_with_lease: true,
                ..PushOptions::default()
            }));
            let items = vec![
                MenuItem::new('f', "Force push (with lease)", then),
                MenuItem::new('p', "Pull first", Then::op(Op::Pull { rebase: None })),
            ];
            return self.open(Popup::Menu(Menu::new(
                format!("{branch} is {} behind its upstream", status.behind),
                items,
            )));
        }
        self.run_ops(vec![Op::Push(PushOptions::default())])
    }

    fn operation_menu(&mut self) -> Update<Msg> {
        let operation = self.snapshot.operation;
        if operation == RepoOperation::None {
            let items = vec![
                MenuItem::new(
                    'p',
                    "Pull (merge)",
                    Then::op(Op::Pull {
                        rebase: Some(false),
                    }),
                ),
                MenuItem::new(
                    'r',
                    "Pull (rebase)",
                    Then::op(Op::Pull { rebase: Some(true) }),
                ),
                MenuItem::new('f', "Fetch all", Then::op(Op::Fetch(None))),
            ];
            return self.open(Popup::Menu(Menu::new("Sync", items)));
        }
        let mut items = vec![
            MenuItem::new('c', "Continue", Then::op(Op::Continue)),
            MenuItem::new('a', "Abort", Then::op(Op::Abort)),
        ];
        if matches!(
            operation,
            RepoOperation::Rebasing | RepoOperation::CherryPicking | RepoOperation::Reverting
        ) {
            items.push(MenuItem::new('s', "Skip", Then::op(Op::Skip)));
        }
        self.open(Popup::Menu(Menu::new(
            format!("{} in progress", operation.label()),
            items,
        )))
    }

    fn merge_menu(&mut self, target: &str) -> Update<Msg> {
        let current = self.current_branch().unwrap_or_else(|| "HEAD".into());
        let items = vec![
            MenuItem::new(
                'm',
                "Merge",
                Then::op(Op::Merge {
                    target: target.to_owned(),
                    squash: false,
                    no_ff: false,
                }),
            ),
            MenuItem::new(
                'n',
                "Merge (no fast-forward)",
                Then::op(Op::Merge {
                    target: target.to_owned(),
                    squash: false,
                    no_ff: true,
                }),
            ),
            MenuItem::new(
                's',
                "Squash merge",
                Then::op(Op::Merge {
                    target: target.to_owned(),
                    squash: true,
                    no_ff: false,
                }),
            ),
        ];
        self.open(Popup::Menu(Menu::new(
            format!("Merge {target} into {current}"),
            items,
        )))
    }

    fn upstream_menu(&mut self) -> Update<Msg> {
        let Some(Row::Branch(index)) = self.selected(View::Branches) else {
            return Update::none();
        };
        let branch = self.snapshot.branches[index].clone();
        let mut items: Vec<MenuItem> = self
            .snapshot
            .remotes
            .iter()
            .enumerate()
            .map(|(position, remote)| {
                MenuItem::new(
                    hotkey(position),
                    format!("Track {}/{}", remote.name, branch.name),
                    Then::op(Op::SetUpstream {
                        branch: branch.name.clone(),
                        upstream: Some(format!("{}/{}", remote.name, branch.name)),
                    }),
                )
            })
            .collect();
        if branch.upstream.is_some() {
            items.push(MenuItem::new(
                'u',
                "Unset upstream",
                Then::op(Op::SetUpstream {
                    branch: branch.name.clone(),
                    upstream: None,
                }),
            ));
        }
        self.open(Popup::Menu(Menu::new(
            format!("Upstream of {}", branch.name),
            items,
        )))
    }

    fn delete_branch(&mut self) -> Update<Msg> {
        let Some(Row::Branch(index)) = self.selected(View::Branches) else {
            return Update::none();
        };
        let branch = self.snapshot.branches[index].clone();
        if branch.is_head {
            self.toast("Cannot delete the checked out branch", true);
            return Update::redraw();
        }
        let mut items = vec![
            MenuItem::new(
                'd',
                "Delete local branch",
                Then::op(Op::DeleteBranch {
                    name: branch.name.clone(),
                    force: false,
                }),
            ),
            MenuItem::new(
                'D',
                "Force delete local branch",
                Then::Confirm {
                    title: "Force delete".into(),
                    message: format!("Delete {} even if unmerged?", branch.name),
                    then: Box::new(Then::op(Op::DeleteBranch {
                        name: branch.name.clone(),
                        force: true,
                    })),
                },
            ),
        ];
        if let Some((remote, name)) = branch
            .upstream
            .as_deref()
            .and_then(|upstream| upstream.split_once('/'))
        {
            items.push(MenuItem::new(
                'r',
                format!("Delete remote branch {remote}/{name}"),
                Then::Confirm {
                    title: "Delete remote branch".into(),
                    message: format!("Delete {remote}/{name} on the remote?"),
                    then: Box::new(Then::op(Op::DeleteRemoteBranch {
                        remote: remote.to_owned(),
                        name: name.to_owned(),
                    })),
                },
            ));
        }
        self.open(Popup::Menu(Menu::new(
            format!("Delete {}", branch.name),
            items,
        )))
    }

    fn reset_menu(&mut self, target: &str) -> Update<Msg> {
        let short: String = target.chars().take(10).collect();
        let hard = Then::op(Op::Reset {
            target: target.to_owned(),
            mode: ResetMode::Hard,
        });
        let items = vec![
            MenuItem::new(
                's',
                "Soft reset (keep changes staged)",
                Then::op(Op::Reset {
                    target: target.to_owned(),
                    mode: ResetMode::Soft,
                }),
            ),
            MenuItem::new(
                'm',
                "Mixed reset (keep changes unstaged)",
                Then::op(Op::Reset {
                    target: target.to_owned(),
                    mode: ResetMode::Mixed,
                }),
            ),
            MenuItem::new(
                'h',
                "Hard reset (discard changes)",
                if self.config.confirm.hard_reset {
                    Then::Confirm {
                        title: "Hard reset".into(),
                        message: format!("Discard all changes and reset to {short}?"),
                        then: Box::new(hard),
                    }
                } else {
                    hard
                },
            ),
        ];
        self.open(Popup::Menu(Menu::new(format!("Reset to {short}"), items)))
    }

    fn bisect_menu(&mut self) -> Update<Msg> {
        let Some(commit) = self.selected_commit() else {
            return Update::none();
        };
        let bisecting = self.snapshot.operation == RepoOperation::Bisecting;
        let mark = |term: &str| Op::BisectMark {
            term: term.to_owned(),
            revision: commit.oid.clone(),
        };
        let mut items = Vec::new();
        if bisecting {
            items.push(MenuItem::new(
                'b',
                format!("Mark {} as bad", commit.short_oid),
                Then::op(mark("bad")),
            ));
            items.push(MenuItem::new(
                'g',
                format!("Mark {} as good", commit.short_oid),
                Then::op(mark("good")),
            ));
            items.push(MenuItem::new(
                's',
                format!("Skip {}", commit.short_oid),
                Then::op(mark("skip")),
            ));
            items.push(MenuItem::new(
                'r',
                "Reset bisect",
                Then::op(Op::BisectReset),
            ));
        } else {
            items.push(MenuItem::new(
                'b',
                format!("Start: {} is bad", commit.short_oid),
                Then::Ops(vec![Op::BisectBegin, mark("bad")]),
            ));
            items.push(MenuItem::new(
                'g',
                format!("Start: {} is good", commit.short_oid),
                Then::Ops(vec![Op::BisectBegin, mark("good")]),
            ));
        }
        self.open(Popup::Menu(Menu::new("Bisect", items)))
    }

    fn diffing_menu(&mut self) -> Update<Msg> {
        if self.diff_base.take().is_some() {
            self.toast("Left diffing mode", false);
        } else if let Some(base) = self.selected_ref() {
            let short: String = base.chars().take(12).collect();
            self.diff_base = Some(base);
            self.toast(
                format!("Diffing against {short}: select refs to compare (esc exits)"),
                false,
            );
        } else {
            return Update::none();
        }
        let mut update = Update::redraw();
        if let Some(command) = self.sync_main(true) {
            update = update.maybe(command);
        }
        update
    }

    fn copy_menu(&mut self) -> Update<Msg> {
        let mut items = Vec::new();
        let view = self.current_view();
        match self.selected(view) {
            Some(Row::File(_)) => {
                if let Some(pattern) = self.selected_files_pattern() {
                    items.push(MenuItem::new(
                        'p',
                        format!("Path: {pattern}"),
                        Then::op(Op::Copy(pattern.clone())),
                    ));
                    let absolute = self
                        .repo
                        .root()
                        .join(&pattern)
                        .to_string_lossy()
                        .into_owned();
                    items.push(MenuItem::new(
                        'a',
                        "Absolute path",
                        Then::op(Op::Copy(absolute)),
                    ));
                }
            }
            Some(Row::Commit(_) | Row::SubCommit(_)) => {
                if let Some(commit) = self.selected_commit() {
                    let message = self.repo.commit_message(&commit.oid).unwrap_or_default();
                    items.push(MenuItem::new(
                        's',
                        "Commit SHA",
                        Then::op(Op::Copy(commit.oid.clone())),
                    ));
                    items.push(MenuItem::new(
                        'm',
                        "Commit message",
                        Then::op(Op::Copy(message)),
                    ));
                    items.push(MenuItem::new(
                        'u',
                        "Commit subject",
                        Then::op(Op::Copy(commit.summary)),
                    ));
                }
            }
            _ => {
                if let Some(name) = self.selected_ref() {
                    items.push(MenuItem::new(
                        'n',
                        format!("Name: {name}"),
                        Then::op(Op::Copy(name)),
                    ));
                }
            }
        }
        if items.is_empty() {
            return Update::none();
        }
        self.open(Popup::Menu(Menu::new("Copy to clipboard", items)))
    }

    fn patch_menu(&mut self) -> Update<Msg> {
        let Some(files) = &self.commit_files else {
            return Update::none();
        };
        let mut paths: Vec<String> = files.patch.iter().cloned().collect();
        if paths.is_empty()
            && let Some(Row::CommitFile(index)) = self.selected(View::CommitFiles)
        {
            paths.push(files.files[index].path.clone());
        }
        if paths.is_empty() {
            return Update::none();
        }
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let diff = self.repo.diff(&files.target, &refs, 3).unwrap_or_default();
        let mut patch = String::new();
        for file in &diff {
            let ids: BTreeSet<bcode_git::models::DiffLineId> = file
                .hunks
                .iter()
                .enumerate()
                .flat_map(|(hunk, h)| {
                    h.lines
                        .iter()
                        .enumerate()
                        .filter(|(_, line)| line.kind != DiffLineKind::Context)
                        .map(move |(line, _)| bcode_git::models::DiffLineId { hunk, line })
                })
                .collect();
            if let Ok(Some(text)) =
                bcode_git::patch::build_patch(file, &ids, bcode_git::patch::PatchDirection::Forward)
            {
                patch.push_str(&text);
            }
        }
        if patch.is_empty() {
            self.toast("Nothing to patch (binary or empty files)", true);
            return Update::redraw();
        }
        let op = |target, reverse| {
            Then::op(Op::ApplyPatch {
                patch: patch.clone(),
                target,
                reverse,
            })
        };
        let title = format!("Patch with {} file(s)", paths.len());
        let items = vec![
            MenuItem::new(
                'a',
                "Apply to working tree",
                op(ApplyTarget::Worktree, false),
            ),
            MenuItem::new('i', "Apply to index", op(ApplyTarget::Index, false)),
            MenuItem::new(
                'r',
                "Apply in reverse to working tree",
                op(ApplyTarget::Worktree, true),
            ),
            MenuItem::new('x', "Clear patch selection", Then::Nothing),
        ];
        if let Some(files) = &mut self.commit_files {
            files.patch.clear();
        }
        self.open(Popup::Menu(Menu::new(title, items)))
    }

    /// Rewrite history at the selected commit with a single-instruction change.
    fn rewrite(&mut self, action: RebaseAction) -> Update<Msg> {
        if self.current_view() != View::Commits {
            return Update::none();
        }
        let Some(Row::Commit(index)) = self.selected(View::Commits) else {
            return Update::none();
        };
        let commits = &self.snapshot.commits;
        let commit = commits[index].clone();
        if commit.is_merge() || commits[..=index].iter().any(Commit::is_merge) {
            self.toast("Rewriting across merge commits is not supported", true);
            return Update::redraw();
        }
        if matches!(action, RebaseAction::Squash | RebaseAction::Fixup) && commit.parents.is_empty()
        {
            self.toast("The root commit has no parent to squash into", true);
            return Update::redraw();
        }
        // Squash and fixup fold the selected commit into its parent.
        let base_index = if matches!(action, RebaseAction::Squash | RebaseAction::Fixup) {
            index + 1
        } else {
            index
        };
        let base_commit = commits.get(base_index).cloned();
        if base_commit.as_ref().is_some_and(Commit::is_merge) {
            self.toast("Rewriting across merge commits is not supported", true);
            return Update::redraw();
        }
        let base = base_commit
            .as_ref()
            .and_then(|commit| commit.parents.first().cloned());
        let mut todo: Vec<RebaseTodoItem> = commits[..=base_index.min(commits.len() - 1)]
            .iter()
            .rev()
            .map(|candidate| RebaseTodoItem {
                action: if candidate.oid == commit.oid {
                    action
                } else {
                    RebaseAction::Pick
                },
                oid: candidate.oid.clone(),
                summary: candidate.summary.clone(),
            })
            .collect();
        if base_commit.is_none() {
            todo.retain(|item| item.oid != commit.oid || action != RebaseAction::Drop);
        }
        let verb = match action {
            RebaseAction::Squash => "Squash",
            RebaseAction::Fixup => "Fixup",
            RebaseAction::Drop => "Drop",
            RebaseAction::Edit => "Edit",
            RebaseAction::Reword => "Reword",
            RebaseAction::Pick => "Pick",
        };
        let ask = action == RebaseAction::Drop && self.config.confirm.delete;
        self.confirm_if(
            ask || matches!(action, RebaseAction::Squash | RebaseAction::Fixup),
            &format!("{verb} commit"),
            format!("{verb} {} {}?", commit.short_oid, commit.summary),
            Then::op(Op::RebaseTodo { base, todo }),
        )
    }

    fn move_commit(&mut self, direction: isize) -> Update<Msg> {
        if self.current_view() != View::Commits {
            return Update::none();
        }
        let Some(Row::Commit(index)) = self.selected(View::Commits) else {
            return Update::none();
        };
        let other = match index.checked_add_signed(direction) {
            Some(other) if other < self.snapshot.commits.len() => other,
            _ => return Update::none(),
        };
        let commits = &self.snapshot.commits;
        let deepest = index.max(other);
        if commits[..=deepest].iter().any(Commit::is_merge) {
            self.toast("Moving across merge commits is not supported", true);
            return Update::redraw();
        }
        let base = commits[deepest].parents.first().cloned();
        let mut order: Vec<&Commit> = commits[..=deepest].iter().collect();
        order.swap(index, other);
        let todo = order
            .iter()
            .rev()
            .map(|commit| RebaseTodoItem {
                action: RebaseAction::Pick,
                oid: commit.oid.clone(),
                summary: commit.summary.clone(),
            })
            .collect();
        self.cursors.insert(View::Commits, other);
        self.keys.remove(&View::Commits);
        self.run_ops(vec![Op::RebaseTodo { base, todo }])
    }

    fn undo(&mut self) -> Update<Msg> {
        let Some(entry) = self.undo.last().cloned() else {
            self.toast("Nothing to undo", true);
            return Update::redraw();
        };
        self.confirm_if(
            true,
            "Undo",
            format!(
                "Undo \"{}\"? HEAD returns to {}.",
                entry.label,
                &entry.before.oid[..10.min(entry.before.oid.len())]
            ),
            Then::Ops(vec![Op::Restore {
                position: entry.before,
                journal: crate::ops::JournalMove::Undo,
            }]),
        )
    }

    fn redo(&mut self) -> Update<Msg> {
        let Some(entry) = self.redo.last().cloned() else {
            self.toast("Nothing to redo", true);
            return Update::redraw();
        };
        self.confirm_if(
            true,
            "Redo",
            format!("Redo \"{}\"?", entry.label),
            Then::Ops(vec![Op::Restore {
                position: entry.after,
                journal: crate::ops::JournalMove::Redo,
            }]),
        )
    }

    fn help(&mut self) -> Update<Msg> {
        let scopes = self.help_scopes();
        let items = self.action_items(&scopes);
        self.open(Popup::Menu(Menu::filtered(
            format!("Keybindings · {}", self.current_view().label()),
            items,
        )))
    }

    fn palette(&mut self) -> Update<Msg> {
        let mut scopes = self.help_scopes();
        for view in [View::Files, View::Branches, View::Commits, View::Stash] {
            if !scopes.contains(&view.scope()) {
                scopes.push(view.scope());
            }
        }
        let mut items = self.action_items(&scopes);
        let context = match self.selected(self.current_view()) {
            Some(Row::Commit(_) | Row::SubCommit(_)) => self
                .selected_commit()
                .map(|commit| crate::host::HostContext::Commit(commit.oid)),
            Some(Row::File(_)) => self
                .selected_entry()
                .map(|entry| crate::host::HostContext::File(entry.path.clone())),
            _ => None,
        }
        .unwrap_or(crate::host::HostContext::StagedChanges);
        items.extend(self.host.menu_items(&context));
        self.open(Popup::Menu(Menu::filtered(
            format!("{} · command palette", self.host.name()),
            items,
        )))
    }

    fn help_scopes(&self) -> Vec<Scope> {
        if self.window == Window::Main {
            vec![
                if self.staging.is_some() {
                    Scope::Staging
                } else {
                    Scope::Main
                },
                Scope::Global,
            ]
        } else {
            vec![self.current_view().scope(), Scope::List, Scope::Global]
        }
    }

    fn action_items(&self, scopes: &[Scope]) -> Vec<MenuItem> {
        let mut items: Vec<MenuItem> = Vec::new();
        for scope in scopes {
            for action in Action::ALL.iter().filter(|action| action.scope() == *scope) {
                if matches!(action, Action::Help | Action::Palette) {
                    continue;
                }
                let keys = self
                    .keymap
                    .keys(scope.name(), &action.id())
                    .iter()
                    .map(|key| display_key(key))
                    .collect::<Vec<_>>()
                    .join(" ");
                items.push(MenuItem::plain(
                    action.description(),
                    keys,
                    Then::Action(*action),
                ));
            }
            for (index, command) in self.config.custom_commands.iter().enumerate() {
                if command.scope == scope.name() {
                    let id = format!("bcode.git.custom.{index}");
                    let keys = self.keymap.keys(scope.name(), &id).join(" ");
                    items.push(MenuItem::plain(
                        command
                            .description
                            .clone()
                            .unwrap_or_else(|| command.command.clone()),
                        keys,
                        Then::ActionId(id),
                    ));
                }
            }
        }
        items
    }
}

fn hotkey(index: usize) -> char {
    char::from_digit(u32::try_from(index + 1).unwrap_or(0) % 10, 10).unwrap_or('0')
}

/// Handle a key in a menu popup (already removed from the stack).
pub fn menu_key(
    app: &mut App,
    mut menu: Menu,
    action: Option<Action>,
    stroke: KeyStroke,
    event: &Event,
) -> Update<Msg> {
    let visible = menu.visible();
    match action {
        Some(Action::MenuClose) => return Update::redraw(),
        Some(Action::MenuUp) => menu.selected = menu.selected.saturating_sub(1),
        Some(Action::MenuDown) => {
            menu.selected = (menu.selected + 1).min(visible.len().saturating_sub(1));
        }
        Some(Action::MenuConfirm) => {
            if let Some(index) = visible.get(menu.selected) {
                let then = menu.items[*index].then.clone();
                return app.apply(then);
            }
            return Update::redraw();
        }
        _ => {
            if let Some(filter) = &menu.filter {
                let policy = TextInputPolicy::chat_composer();
                TextInputControl::new(&policy).handle_event(&mut filter.borrow_mut(), event);
                menu.selected = 0;
            } else if let Some(ch) =
                bcode_keymap::controls::LiteralText::from_stroke(stroke).map(|text| text.0)
                && let Some(item) = menu.items.iter().find(|item| item.hotkey == Some(ch))
            {
                let then = item.then.clone();
                return app.apply(then);
            }
        }
    }
    let mut state = menu.list.get();
    state.set_focused(Some(menu.selected));
    state.set_selected(Some(menu.selected));
    menu.list.set(state);
    app.popups.push(Popup::Menu(menu));
    Update::redraw()
}

/// Handle a key in a text viewer popup, which reads as a page: keys scroll it.
pub fn text_key(app: &mut App, view: TextView, action: Option<Action>) -> Update<Msg> {
    if matches!(action, Some(Action::MenuClose | Action::MenuConfirm)) {
        return Update::redraw();
    }
    app.popups.push(Popup::Text(view));
    match action {
        Some(Action::MenuUp) => app.scroll_list(crate::scroll::TEXT_LIST, -1),
        Some(Action::MenuDown) => app.scroll_list(crate::scroll::TEXT_LIST, 1),
        _ => false,
    };
    Update::redraw()
}

/// Initial title and text for a prompt opened from a menu.
#[must_use]
pub fn prompt_defaults(_app: &App, kind: &PromptKind) -> (String, String) {
    let title = match kind {
        PromptKind::StashMessage(_) => "Stash message",
        PromptKind::NewBranch { .. } => "New branch name",
        PromptKind::RemoteName => "Remote name",
        _ => "Input",
    };
    (title.to_owned(), String::new())
}

/// Act on a submitted prompt.
#[allow(clippy::too_many_lines)]
pub fn submit_prompt(app: &mut App, prompt: Prompt) -> Update<Msg> {
    let text = prompt.text();
    let value = text.trim().to_owned();
    let required = |app: &mut App, prompt: Prompt| {
        app.toast("A value is required", true);
        app.popups.push(Popup::Prompt(prompt));
        Update::redraw()
    };
    let summary_required = |app: &mut App, mut prompt: Prompt| {
        app.toast("A summary is required", true);
        prompt.body_focused = false;
        app.popups.push(Popup::Prompt(prompt));
        Update::redraw()
    };
    match prompt.kind.clone() {
        PromptKind::Filter => Update::redraw(),
        PromptKind::Commit {
            amend,
            no_verify,
            stage_all,
            prefill,
        } => {
            if prompt.summary().trim().is_empty() {
                return summary_required(app, prompt);
            }
            let entries = &app.snapshot.status.entries;
            if !amend && entries.is_empty() {
                app.toast("Nothing to commit: there are no changes", true);
                app.popups.push(Popup::Prompt(prompt));
                return Update::redraw();
            }
            // Re-evaluated at submit: staging may have changed while the editor was open.
            let stage_all = stage_all
                && !entries
                    .iter()
                    .any(bcode_git::models::StatusEntry::has_staged);
            // Amending rewrites HEAD's own message and leaves drafts alone. Otherwise the message
            // is saved first, so a commit that fails (a rejecting hook, say) keeps it as the
            // draft, and the commit removes the drafts it consumed only once it lands.
            let save_draft = app.config.commit.save_draft;
            let drafts = if amend {
                DraftCleanup::default()
            } else {
                if save_draft && let Err(error) = app.repo.save_commit_draft(&text) {
                    app.toast(
                        format!("Could not save the draft: {}", error.message()),
                        true,
                    );
                }
                let source = prefill.as_ref().map(|(source, _)| *source);
                DraftCleanup {
                    bcode: save_draft || source == Some(CommitMessageSource::BcodeDraft),
                    lazygit: source == Some(CommitMessageSource::LazygitDraft),
                }
            };
            // A configured prefix such as "WIP" skips hooks, as in lazygit.
            let prefix = app.config.commit.skip_hook_prefix.trim();
            let no_verify = no_verify
                || (!prefix.is_empty() && prompt.summary().trim_start().starts_with(prefix));
            let commit = Op::Commit(
                CommitOptions {
                    message: crate::commit_message::wrap_description(
                        &text,
                        app.config.commit.wrap_width,
                    ),
                    amend,
                    no_verify,
                    allow_empty: amend,
                    no_edit: false,
                },
                drafts,
            );
            app.run_ops(if stage_all {
                vec![Op::StageAll, commit]
            } else {
                vec![commit]
            })
        }
        PromptKind::Reword(oid) => {
            if prompt.summary().trim().is_empty() {
                return summary_required(app, prompt);
            }
            app.run_ops(vec![Op::Reword { oid, message: text }])
        }
        _ if value.is_empty()
            && !matches!(
                prompt.kind,
                PromptKind::StashMessage(_)
                    | PromptKind::Custom { .. }
                    | PromptKind::TagMessage { .. }
            ) =>
        {
            required(app, prompt)
        }
        PromptKind::NewBranch { start } => app.run_ops(vec![Op::CreateBranch {
            name: value,
            start,
            checkout: true,
        }]),
        PromptKind::RenameBranch(old) => app.run_ops(vec![Op::RenameBranch { old, new: value }]),
        PromptKind::CheckoutByName => app.run_ops(vec![Op::Checkout(value)]),
        PromptKind::NewTag(target) => app.prompt(
            format!("Message for tag {value} (empty for lightweight)"),
            "",
            PromptKind::TagMessage {
                name: value,
                target,
            },
        ),
        PromptKind::TagMessage { name, target } => app.run_ops(vec![Op::CreateTag {
            name,
            target,
            message: (!value.is_empty()).then_some(text),
        }]),
        PromptKind::RemoteName => {
            app.prompt(format!("URL for {value}"), "", PromptKind::RemoteUrl(value))
        }
        PromptKind::RemoteUrl(name) => app.run_ops(vec![
            Op::AddRemote {
                name: name.clone(),
                url: value,
            },
            Op::Fetch(Some(name)),
        ]),
        PromptKind::EditRemoteUrl(name) => app.run_ops(vec![Op::SetRemoteUrl { name, url: value }]),
        PromptKind::StashMessage(mut options) => {
            options.message = (!value.is_empty()).then_some(value);
            app.run_ops(vec![Op::StashPush(options)])
        }
        PromptKind::RenameStash(entry) => app.run_ops(vec![Op::StashRename {
            entry,
            message: value,
        }]),
        PromptKind::StashBranch(selector) => app.run_ops(vec![Op::StashBranch {
            name: value,
            selector,
        }]),
        PromptKind::WorktreeBranch => {
            let create = !app
                .snapshot
                .branches
                .iter()
                .any(|branch| branch.name == value);
            let parent = app
                .repo
                .root()
                .parent()
                .map_or_else(|| app.repo.root().to_owned(), std::path::Path::to_path_buf);
            let name = app
                .repo
                .root()
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let suggestion = parent.join(format!("{name}-{}", value.replace('/', "-")));
            app.prompt(
                "Worktree path",
                &suggestion.to_string_lossy(),
                PromptKind::WorktreePath {
                    branch: value,
                    create,
                },
            )
        }
        PromptKind::WorktreePath { branch, create } => app.run_ops(vec![Op::AddWorktree {
            path: std::path::PathBuf::from(value),
            branch,
            create,
        }]),
        PromptKind::Shell => {
            app.shell_history.push(&value);
            app.run_ops_with_output(vec![Op::Shell(value)], true)
        }
        PromptKind::RemoteCheckoutName(branch) => {
            let start = branch.full_name();
            app.run_ops(vec![
                Op::CreateBranch {
                    name: value.clone(),
                    start: Some(start.clone()),
                    checkout: true,
                },
                Op::SetUpstream {
                    branch: value,
                    upstream: Some(start),
                },
            ])
        }
        PromptKind::Custom {
            command,
            mut values,
        } => {
            let name = app
                .config
                .custom_commands
                .get(command)
                .and_then(|custom| custom.prompts.get(values.len()))
                .map(|prompt| prompt.name.clone())
                .unwrap_or_default();
            values.push((name, text));
            custom_command(app, command, values)
        }
    }
}

/// Run (or continue collecting prompts for) a custom command.
pub fn custom_command(app: &mut App, index: usize, values: Vec<(String, String)>) -> Update<Msg> {
    let Some(command) = app.config.custom_commands.get(index).cloned() else {
        return Update::none();
    };
    if let Some(next) = command.prompts.get(values.len()) {
        return app.prompt(
            next.title.clone(),
            &next.initial,
            PromptKind::Custom {
                command: index,
                values,
            },
        );
    }
    let mut text = command.command.clone();
    let mut replace = |placeholder: &str, value: Option<String>| {
        if text.contains(placeholder) {
            text = text.replace(placeholder, &quote(&value.unwrap_or_default()));
        }
    };
    replace(
        "{file}",
        app.selected_entry()
            .map(|entry| entry.path.clone())
            .or_else(
                || match (app.selected(View::CommitFiles), &app.commit_files) {
                    (Some(Row::CommitFile(index)), Some(files)) => {
                        Some(files.files[index].path.clone())
                    }
                    _ => None,
                },
            ),
    );
    replace(
        "{branch}",
        match app.selected(app.current_view()) {
            Some(Row::Branch(index)) => Some(app.snapshot.branches[index].name.clone()),
            Some(Row::RemoteBranch(index)) => Some(app.snapshot.remote_branches[index].full_name()),
            _ => app.current_branch(),
        },
    );
    replace("{commit}", app.selected_commit().map(|commit| commit.oid));
    replace(
        "{remote}",
        match app.selected(View::Remotes) {
            Some(Row::Remote(index)) => Some(app.snapshot.remotes[index].name.clone()),
            _ => app.remote.clone(),
        },
    );
    replace(
        "{tag}",
        match app.selected(View::Tags) {
            Some(Row::Tag(index)) => Some(app.snapshot.tags[index].name.clone()),
            _ => None,
        },
    );
    replace(
        "{stash}",
        match app.selected(View::Stash) {
            Some(Row::Stash(index)) => Some(app.snapshot.stashes[index].selector()),
            _ => None,
        },
    );
    replace("{current_branch}", app.current_branch());
    replace(
        "{repo}",
        Some(app.repo.root().to_string_lossy().into_owned()),
    );
    for (name, value) in &values {
        replace(&format!("{{prompt.{name}}}"), Some(value.clone()));
    }
    let then = if command.interactive {
        Then::External(External {
            command: text.clone(),
            cwd: app.repo.root().to_owned(),
            pause: true,
        })
    } else {
        Then::Ops(vec![Op::Shell(text.clone())])
    };
    if command.confirm {
        return app.apply(Then::Confirm {
            title: command.description.unwrap_or_else(|| "Run command".into()),
            message: text,
            then: Box::new(then),
        });
    }
    match then {
        Then::Ops(ops) => app.run_ops_with_output(ops, command.show_output),
        other => app.apply(other),
    }
}

/// Lines for the head summary in the status panel.
#[must_use]
pub fn head_label(app: &App) -> Line {
    let theme = &app.theme;
    let name = app
        .repo
        .root()
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut spans = vec![
        Span::styled(
            name,
            theme.text.add_modifier(bmux_tui::style::Modifier::BOLD),
        ),
        Span::styled(" → ", theme.muted),
    ];
    match &app.snapshot.head {
        Some(HeadState::Branch { name } | HeadState::Unborn { name }) => {
            spans.push(Span::styled(name.clone(), theme.branch_head));
        }
        Some(HeadState::Detached { oid }) => {
            spans.push(Span::styled(
                format!("detached {}", &oid[..oid.len().min(8)]),
                theme.warning,
            ));
        }
        None => spans.push(Span::styled("…", theme.muted)),
    }
    let branch = &app.snapshot.status.branch;
    if branch.upstream.is_some() {
        if branch.ahead == 0 && branch.behind == 0 {
            spans.push(Span::styled(" ✓", theme.success));
        } else {
            spans.push(Span::styled(format!(" ↑{}", branch.ahead), theme.success));
            spans.push(Span::styled(format!("↓{}", branch.behind), theme.warning));
        }
    }
    if app.snapshot.operation != RepoOperation::None {
        spans.push(Span::styled(
            format!("  {}", app.snapshot.operation.label().to_uppercase()),
            theme.warning.add_modifier(bmux_tui::style::Modifier::BOLD),
        ));
    }
    if app.diff_base.is_some() {
        spans.push(Span::styled(
            "  DIFFING",
            theme.info.add_modifier(bmux_tui::style::Modifier::BOLD),
        ));
    }
    Line::from_spans(spans)
}
