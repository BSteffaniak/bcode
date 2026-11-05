//! Presentation tree assembled from BMUX components.

use std::cell::Cell;

use bcode_git::models::{ChangeKind, DiffFileStatus, StatusEntry, SubmoduleState};
use bmux_tui::component::Element;
use bmux_tui::composition::{Column, Flex, Row as BoxRow, SizeBox, Stack, Surface, TextBlock};
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::style::{Modifier, Style};
use bmux_tui::text::{Line, Span, Text, TextWrap};
use bmux_tui_components::modal_frame::{
    ModalFrame, ModalFrameComponent, ModalPlacement, ModalSizing, ModalTheme,
};
use bmux_tui_components::pane::{Pane, PaneComponent, PaneState, PaneStyles};
use bmux_tui_components::scrollbar_layout::ScrollbarAxisLayoutMode;
use bmux_tui_components::selectable_list::{
    SelectableListComponent, SelectableListHighlightPolicy, SelectableListItem,
    SelectableListPolicy, SelectableListState, SelectableListStyles,
};
use bmux_tui_components::text_input::TextInputPolicy;
use bmux_tui_components::text_input_box::{
    TextInputBoxComponent, TextInputBoxPolicy, TextInputBoxStyles,
};

use crate::actions::head_label;
use crate::app::{App, Row, ScreenMode, View, Window};
use crate::config::{CollapsedWindows, MainSplit, Portrait};
use crate::data::{FileNode, SectionKind, commit_line, format_time, status_style};
use crate::keymap::{Action, Scope, display_key};
use crate::popup::Popup;

/// Items and per-frame state owned for one layout and paint pass.
pub struct Frame {
    side: Vec<(Window, View, Vec<SelectableListItem>)>,
    main: Vec<Vec<SelectableListItem>>,
    log: Vec<SelectableListItem>,
    popup: Vec<SelectableListItem>,
    side_panes: Vec<Cell<PaneState>>,
    main_panes: [Cell<PaneState>; 2],
    log_pane: Cell<PaneState>,
    log_state: Cell<SelectableListState>,
    collapsed: Vec<(Vec<SelectableListItem>, Cell<SelectableListState>)>,
}

const MIN_WIDTH: u16 = 30;
/// Visible rows of a commit description.
const DESCRIPTION_ROWS: u16 = 6;
const MIN_HEIGHT: u16 = 10;

impl Frame {
    /// Build items for the current state.
    #[must_use]
    pub fn new(app: &App) -> Self {
        let side: Vec<_> = app
            .side_windows()
            .map(|window| {
                let view = app.view(window);
                (window, view, side_items(app, view))
            })
            .collect();
        let collapsed = side
            .iter()
            .map(|(window, view, _)| {
                (
                    vec![SelectableListItem::rich(
                        "title",
                        window_title(app, *window, *view),
                    )],
                    Cell::new(SelectableListState::new(None)),
                )
            })
            .collect();
        let pane_state = || Cell::new(PaneState::new(Rect::default()));
        let main = (0..2).map(|section| main_items(app, section)).collect();
        let records = app.repo.command_log();
        let log: Vec<SelectableListItem> = records
            .iter()
            .enumerate()
            .rev()
            .take(100)
            .map(|(index, record)| {
                SelectableListItem::rich(
                    index.to_string(),
                    Line::from_spans(vec![
                        Span::styled(
                            if record.success { "✓ " } else { "✗ " },
                            if record.success {
                                app.theme.success
                            } else {
                                app.theme.error
                            },
                        ),
                        Span::styled(record.display(), app.theme.text),
                        Span::styled(format!("  {}ms", record.duration_ms), app.theme.muted),
                    ]),
                )
            })
            .collect();
        let popup = popup_items(app);
        Self {
            main,
            log,
            popup,
            side_panes: side.iter().map(|_| pane_state()).collect(),
            main_panes: [pane_state(), pane_state()],
            log_pane: pane_state(),
            log_state: Cell::new(SelectableListState::new(None)),
            collapsed,
            side,
        }
    }
}

fn side_items(app: &App, view: View) -> Vec<SelectableListItem> {
    let theme = &app.theme;
    let rows = app.rows(view);
    if rows.is_empty() {
        let message = match view {
            View::Files => "  ✨ Working tree clean",
            View::Stash => "  No stashes",
            View::Commits | View::SubCommits => "  No commits",
            View::Tags => "  No tags",
            View::Remotes => "  No remotes",
            View::Worktrees => "  No worktrees",
            View::Submodules => "  No submodules",
            _ => "  Nothing here",
        };
        return vec![
            SelectableListItem::rich(
                "empty",
                Line::from_spans(vec![Span::styled(message, theme.muted)]),
            )
            .disabled(true),
        ];
    }
    rows.iter()
        .enumerate()
        .map(|(index, row)| SelectableListItem::rich(index.to_string(), row_line(app, *row)))
        .collect()
}

fn entry_spans(entry: &StatusEntry, app: &App) -> Vec<Span> {
    let theme = &app.theme;
    let (index_style, worktree_style) = status_style(entry, theme);
    let code = entry.short_code();
    let mut chars = code.chars();
    let first = chars.next().unwrap_or(' ').to_string();
    let second = chars.next().unwrap_or(' ').to_string();
    vec![
        Span::styled(first, index_style.add_modifier(Modifier::BOLD)),
        Span::styled(second, worktree_style.add_modifier(Modifier::BOLD)),
        Span::raw(" "),
    ]
}

fn entry_name_style(entry: &StatusEntry, app: &App) -> Style {
    let theme = &app.theme;
    if entry.is_conflicted() {
        theme.conflict
    } else if entry.is_untracked() {
        theme.untracked
    } else if entry.has_staged() && !entry.has_unstaged() {
        theme.staged
    } else if entry.worktree == ChangeKind::Deleted || entry.index == ChangeKind::Deleted {
        theme.removed
    } else {
        theme.text
    }
}

#[allow(clippy::too_many_lines)]
fn row_line(app: &App, row: Row) -> Line {
    let theme = &app.theme;
    let snapshot = &app.snapshot;
    match row {
        Row::Status => head_label(app),
        Row::File(index) => match &app.file_nodes[index] {
            FileNode::Dir {
                label,
                depth,
                collapsed,
                entries,
                ..
            } => {
                let staged = entries
                    .iter()
                    .all(|entry| snapshot.status.entries[*entry].has_staged());
                let unstaged = entries
                    .iter()
                    .any(|entry| snapshot.status.entries[*entry].has_unstaged());
                let marker = match (staged, unstaged) {
                    (true, false) => Span::styled("● ", theme.staged),
                    (true, true) => Span::styled("◐ ", theme.warning),
                    _ => Span::styled("○ ", theme.unstaged),
                };
                Line::from_spans(vec![
                    Span::raw("  ".repeat(*depth)),
                    marker,
                    Span::styled(if *collapsed { "▸ " } else { "▾ " }, theme.directory),
                    Span::styled(format!("{label}/"), theme.directory),
                ])
            }
            FileNode::File {
                entry,
                label,
                depth,
            } => {
                let entry = &snapshot.status.entries[*entry];
                let mut spans = vec![Span::raw("  ".repeat(*depth))];
                spans.extend(entry_spans(entry, app));
                spans.push(Span::styled(label.clone(), entry_name_style(entry, app)));
                if let Some(orig) = &entry.orig_path {
                    spans.push(Span::styled(format!(" ← {orig}"), theme.muted));
                }
                if entry.submodule {
                    spans.push(Span::styled(" ⧉", theme.info));
                }
                Line::from_spans(spans)
            }
        },
        Row::Worktree(index) => {
            let worktree = &snapshot.worktrees[index];
            let current = worktree.path == app.repo.root();
            Line::from_spans(vec![
                Span::styled(if current { "● " } else { "  " }, theme.success),
                Span::styled(
                    worktree
                        .branch
                        .clone()
                        .unwrap_or_else(|| "(detached)".into()),
                    if current {
                        theme.branch_head
                    } else {
                        theme.branch_local
                    },
                ),
                Span::styled(format!("  {}", worktree.path.display()), theme.muted),
                Span::styled(if worktree.locked { "  locked" } else { "" }, theme.warning),
            ])
        }
        Row::Submodule(index) => {
            let submodule = &snapshot.submodules[index];
            let (glyph, style) = match submodule.state {
                SubmoduleState::Current => ("✓ ", theme.success),
                SubmoduleState::Uninitialized => ("○ ", theme.muted),
                SubmoduleState::Modified => ("± ", theme.warning),
                SubmoduleState::Conflicted => ("✗ ", theme.conflict),
            };
            Line::from_spans(vec![
                Span::styled(glyph, style),
                Span::styled(submodule.path.clone(), theme.text),
                Span::styled(
                    submodule
                        .describe
                        .as_ref()
                        .map(|describe| format!("  {describe}"))
                        .unwrap_or_default(),
                    theme.muted,
                ),
            ])
        }
        Row::Branch(index) => {
            let branch = &snapshot.branches[index];
            let mut spans = vec![
                Span::styled(
                    format!("{:>3} ", format_time(branch.commit_time, true)),
                    theme.date,
                ),
                Span::styled(if branch.is_head { "✱ " } else { "  " }, theme.branch_head),
                Span::styled(
                    branch.name.clone(),
                    if branch.is_head {
                        theme.branch_head.add_modifier(Modifier::BOLD)
                    } else {
                        theme.hashed(&branch.name)
                    },
                ),
            ];
            if branch.upstream_gone {
                spans.push(Span::styled(" ⊘ gone", theme.error));
            } else if branch.upstream.is_some() {
                if branch.ahead == 0 && branch.behind == 0 {
                    spans.push(Span::styled(" ✓", theme.success));
                } else {
                    if branch.ahead > 0 {
                        spans.push(Span::styled(format!(" ↑{}", branch.ahead), theme.success));
                    }
                    if branch.behind > 0 {
                        spans.push(Span::styled(format!(" ↓{}", branch.behind), theme.warning));
                    }
                }
            }
            if branch.worktree.is_some() {
                spans.push(Span::styled(" (worktree)", theme.info));
            }
            spans.push(Span::styled(format!("  {}", branch.summary), theme.muted));
            Line::from_spans(spans)
        }
        Row::Remote(index) => {
            let remote = &snapshot.remotes[index];
            let count = snapshot
                .remote_branches
                .iter()
                .filter(|branch| branch.remote == remote.name)
                .count();
            Line::from_spans(vec![
                Span::styled("☁ ", theme.branch_remote),
                Span::styled(
                    remote.name.clone(),
                    theme.branch_remote.add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {count} branches"), theme.muted),
                Span::styled(
                    format!("  {}", remote.fetch_url.clone().unwrap_or_default()),
                    theme.muted,
                ),
            ])
        }
        Row::RemoteBranch(index) => {
            let branch = &snapshot.remote_branches[index];
            Line::from_spans(vec![
                Span::styled(
                    format!("{:>3} ", format_time(branch.commit_time, true)),
                    theme.date,
                ),
                Span::styled(branch.name.clone(), theme.hashed(&branch.name)),
                Span::styled(format!("  {}", branch.summary), theme.muted),
            ])
        }
        Row::Tag(index) => {
            let tag = &snapshot.tags[index];
            Line::from_spans(vec![
                Span::styled(if tag.annotated { "◆ " } else { "◇ " }, theme.tag),
                Span::styled(tag.name.clone(), theme.tag),
                Span::styled(format!("  {}", tag.summary), theme.muted),
            ])
        }
        Row::SubCommit(index) => app.sub_commits.as_ref().map_or_else(Line::new, |sub| {
            commit_line(
                &sub.commits[index],
                app.cx
                    .show_graph
                    .then(|| sub.graph.get(index).map(Vec::as_slice))
                    .flatten(),
                false,
                theme,
                app.cx.relative_dates,
            )
        }),
        Row::Commit(index) => {
            let commit = &snapshot.commits[index];
            let mut line = commit_line(
                commit,
                app.cx
                    .show_graph
                    .then(|| app.commit_graph.get(index).map(Vec::as_slice))
                    .flatten(),
                snapshot.unpushed.contains(&commit.oid),
                theme,
                app.cx.relative_dates,
            );
            if app.cherry.iter().any(|copied| copied.oid == commit.oid) {
                line.spans.insert(
                    0,
                    Span::styled("⧉ ", theme.info.add_modifier(Modifier::BOLD)),
                );
            }
            line
        }
        Row::Reflog(index) => {
            let entry = &snapshot.reflog[index];
            Line::from_spans(vec![
                Span::styled(format!("{} ", entry.short_oid), theme.sha),
                Span::styled(entry.message.clone(), theme.text),
                Span::styled(
                    format!("  {}", format_time(entry.time, app.cx.relative_dates)),
                    theme.date,
                ),
            ])
        }
        Row::Stash(index) => {
            let entry = &snapshot.stashes[index];
            Line::from_spans(vec![
                Span::styled(format!("{} ", entry.index), theme.muted),
                Span::styled(entry.message.clone(), theme.text),
                Span::styled(
                    format!("  {}", format_time(entry.time, app.cx.relative_dates)),
                    theme.date,
                ),
            ])
        }
        Row::CommitFile(index) => app.commit_files.as_ref().map_or_else(Line::new, |files| {
            let file = &files.files[index];
            let (badge, style) = match file.status {
                DiffFileStatus::Added => ("A", theme.added),
                DiffFileStatus::Deleted => ("D", theme.removed),
                DiffFileStatus::Renamed => ("R", theme.info),
                DiffFileStatus::Copied => ("C", theme.info),
                DiffFileStatus::Modified => ("M", theme.warning),
            };
            let mut spans = vec![
                Span::styled(
                    if files.patch.contains(&file.path) {
                        "● "
                    } else {
                        "  "
                    },
                    theme.accent,
                ),
                Span::styled(format!("{badge} "), style.add_modifier(Modifier::BOLD)),
                Span::styled(file.path.clone(), theme.text),
            ];
            if let Some(old) = &file.old_path {
                spans.push(Span::styled(format!(" ← {old}"), theme.muted));
            }
            Line::from_spans(spans)
        }),
    }
}

fn main_items(app: &App, section: usize) -> Vec<SelectableListItem> {
    let Some(rows) = app
        .main
        .as_ref()
        .and_then(|main| main.sections.get(section))
        .map(|section| &section.rows)
    else {
        return Vec::new();
    };
    let selection = if section == app.main_section {
        app.staging_selection()
    } else {
        std::collections::BTreeSet::default()
    };
    let marking = app
        .staging
        .is_some_and(|staging| staging.anchor.is_some() || staging.hunk_mode);
    rows.iter()
        .enumerate()
        .map(|(index, row)| {
            let line = if marking && row.id.is_some_and(|id| selection.contains(&id)) {
                let mut line = row.line.clone();
                if let Some(first) = line.spans.get_mut(0) {
                    first.style = first.style.patch(app.theme.range);
                }
                line
            } else {
                row.line.clone()
            };
            SelectableListItem::rich(index.to_string(), line)
        })
        .collect()
}

fn popup_items(app: &App) -> Vec<SelectableListItem> {
    let theme = &app.theme;
    match app.popups.last() {
        Some(Popup::Menu(menu)) => menu
            .visible()
            .into_iter()
            .map(|index| {
                let item = &menu.items[index];
                let mut spans = Vec::new();
                if let Some(hotkey) = item.hotkey {
                    spans.push(Span::styled(format!("{hotkey}  "), theme.key));
                }
                spans.push(Span::styled(item.label.clone(), theme.text));
                if !item.detail.is_empty() {
                    spans.push(Span::styled(format!("   {}", item.detail), theme.key));
                }
                SelectableListItem::rich(index.to_string(), Line::from_spans(spans))
            })
            .collect(),
        Some(Popup::Text(view)) => view
            .lines
            .iter()
            .enumerate()
            .map(|(index, line)| SelectableListItem::rich(index.to_string(), line.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

/// Wraps a list so the focused row is revealed against the measured viewport before painting.
struct Revealed<'a, 'state>(SelectableListComponent<'a, 'state>);

impl bmux_tui::component::Component for Revealed<'_, '_> {
    fn layout(
        &self,
        constraints: bmux_tui::component::Constraints,
        cx: &mut bmux_tui::component::LayoutCx,
    ) -> bmux_tui::component::LayoutNode {
        self.0.layout(constraints, cx)
    }

    fn paint(
        &self,
        layout: &bmux_tui::component::LayoutNode,
        cx: &mut bmux_tui::paint::PaintCx<'_, '_>,
    ) {
        self.0.reveal_focus(layout);
        self.0.paint(layout, cx);
    }

    fn revision(&self) -> bmux_tui::component::ComponentRevision {
        self.0.revision()
    }
}

fn list<'a>(
    app: &'a App,
    id: String,
    items: &'a [SelectableListItem],
    state: &'a Cell<SelectableListState>,
    cursor: Option<usize>,
    focused: bool,
) -> Revealed<'a, 'a> {
    let mut current = state.get();
    current.set_focused(cursor);
    current.set_selected(cursor);
    current.interaction.focused = focused;
    state.set(current);
    let theme = &app.theme;
    let cursor_style = if focused {
        theme.cursor
    } else {
        theme.cursor_inactive
    };
    let styles = SelectableListStyles {
        background: Style::new(),
        normal: Style::new(),
        focused: cursor_style,
        selected: cursor_style,
        hovered: Style::new(),
        pressed: cursor_style,
        disabled: Style::new(),
        ..SelectableListStyles::default()
    };
    let policy = SelectableListPolicy {
        highlight: SelectableListHighlightPolicy::new("", false),
        ..SelectableListPolicy::interactive()
    }
    .scrollbar(ScrollbarAxisLayoutMode::Hidden);
    Revealed(
        SelectableListComponent::new(id, items, state)
            .policy(policy)
            .styles(styles),
    )
}

fn pane<'a>(
    app: &'a App,
    id: impl Into<String>,
    title: Line,
    focused: bool,
    state: &'a Cell<PaneState>,
    child: impl bmux_tui::component::Component + 'a,
) -> PaneComponent<'a, 'a> {
    let mut current = state.get();
    current.interaction.focused = focused;
    state.set(current);
    PaneComponent::new(
        id.into(),
        Pane::new().title(title).styles(PaneStyles {
            background: None,
            border: app.theme.border,
            focused_border: app.theme.border_focused,
        }),
        state,
        child,
    )
}

fn window_title(app: &App, window: Window, view: View) -> Line {
    let theme = &app.theme;
    let focused = app.window == window;
    let title_style = if focused {
        theme.title_focused
    } else {
        theme.title
    };
    let mut spans = vec![Span::styled(format!("[{}] ", window.number()), theme.key)];
    let tabs = app.tabs(window);
    match view {
        View::RemoteBranches => {
            spans.push(Span::styled(
                format!("Remote · {}", app.remote.clone().unwrap_or_default()),
                title_style,
            ));
        }
        View::SubCommits => {
            spans.push(Span::styled(
                format!(
                    "Commits · {}",
                    app.sub_commits
                        .as_ref()
                        .map(|sub| sub.title.clone())
                        .unwrap_or_default()
                ),
                title_style,
            ));
        }
        View::CommitFiles => {
            spans.push(Span::styled(
                format!(
                    "Files · {}",
                    app.commit_files
                        .as_ref()
                        .map(|files| files.title.clone())
                        .unwrap_or_default()
                ),
                title_style,
            ));
        }
        _ => {
            for (index, tab) in tabs.iter().enumerate() {
                if index > 0 {
                    spans.push(Span::styled(" · ", theme.muted));
                }
                spans.push(Span::styled(
                    tab.label(),
                    if *tab == view {
                        title_style
                    } else {
                        theme.muted
                    },
                ));
            }
        }
    }
    let count = app.rows(view).len();
    if view != View::Status && count > 0 {
        spans.push(Span::styled(
            format!(" {}/{count}", (app.cursor(view) + 1).min(count)),
            theme.muted,
        ));
    }
    if let Some(filter) = app.filters.get(&view).filter(|filter| !filter.is_empty()) {
        spans.push(Span::styled(format!(" /{filter}"), theme.warning));
    }
    spans.push(Span::raw(" "));
    Line::from_spans(spans)
}

fn side_column<'a>(app: &'a App, frame: &'a Frame) -> Element<'a> {
    let layout = &app.config.layout;
    let mut column = Column::new();
    let focus_window = if app.window == Window::Main {
        app.last_side
    } else {
        app.window
    };
    let half = app.screen_mode == ScreenMode::Half;
    for (position, (window, view, items)) in frame.side.iter().enumerate() {
        let focused = app.window == *window;
        if half && *window != focus_window {
            // Other windows shrink to a clickable title row, or disappear.
            if layout.collapsed_windows == CollapsedWindows::Titles {
                let (title, state) = &frame.collapsed[position];
                column = column.child(
                    SizeBox::new(
                        SelectableListComponent::new(
                            format!("side-{}", window.number()),
                            title,
                            state,
                        )
                        .policy(SelectableListPolicy {
                            highlight: SelectableListHighlightPolicy::new("", false),
                            ..SelectableListPolicy::interactive()
                        })
                        .styles(SelectableListStyles {
                            focused: Style::new(),
                            selected: Style::new(),
                            ..SelectableListStyles::default()
                        }),
                    )
                    .height(1),
                );
            }
            continue;
        }
        let cursor = (!app.rows(*view).is_empty()).then(|| app.cursor(*view));
        let content = list(
            app,
            format!("side-{}", window.number()),
            items,
            &app.lists[view],
            cursor,
            focused,
        );
        let panel = pane(
            app,
            format!("pane-{}", window.number()),
            window_title(app, *window, *view),
            focused,
            &frame.side_panes[position],
            content,
        );
        if !half && app.tabs(*window) == [View::Status] {
            column = column.child(SizeBox::new(panel).height(3));
        } else {
            let weight = if *window == focus_window && layout.expand_focused {
                layout.expanded_weight.max(1)
            } else {
                1
            };
            column = column.flex(Flex::new(weight, panel));
        }
    }
    Element::new(column)
}

#[allow(clippy::too_many_lines)]
fn main_column<'a>(app: &'a App, frame: &'a Frame) -> Element<'a> {
    let theme = &app.theme;
    let mut column = Column::new();
    let sections = app
        .main
        .as_ref()
        .map_or(0, |main| main.sections.len())
        .min(2);
    if sections == 0 {
        let loading = TextBlock::new(Text::from_lines(vec![Line::from_spans(vec![
            Span::styled(
                if app.loaded_once {
                    ""
                } else {
                    "  Loading repository…"
                },
                theme.muted,
            ),
        ])]));
        column = column.flex(Flex::new(
            1,
            pane(
                app,
                "pane-main",
                Line::raw(" "),
                false,
                &frame.main_panes[0],
                loading,
            ),
        ));
    }
    let mut section_panes = Vec::with_capacity(sections);
    for section in 0..sections {
        let Some(data) = app
            .main
            .as_ref()
            .and_then(|main| main.sections.get(section))
        else {
            continue;
        };
        let focused = app.window == Window::Main && app.main_section == section;
        let mut title = vec![Span::styled(
            format!(" {} ", data.title),
            if focused {
                theme.title_focused
            } else {
                theme.title
            },
        )];
        if let Some(crate::data::MainRequest::File(entry)) =
            app.main.as_ref().map(|main| &main.request)
        {
            title.push(Span::styled(format!("{} ", entry.path), theme.text));
        }
        if focused && app.staging.is_some() {
            let mode = match app.staging {
                Some(staging) if staging.anchor.is_some() => "RANGE",
                Some(staging) if staging.hunk_mode => "HUNK",
                _ => "LINE",
            };
            title.push(Span::styled(
                format!("[{mode}] "),
                theme.warning.add_modifier(Modifier::BOLD),
            ));
            let verb = if data.kind == SectionKind::Staged {
                "unstage"
            } else {
                "stage"
            };
            title.push(Span::styled(
                format!(
                    "{} {verb} · {} range · {} hunk · {} switch ",
                    key(app, Action::StageToggle),
                    key(app, Action::StageRange),
                    key(app, Action::StageHunkMode),
                    key(app, Action::StageSwitchSide)
                ),
                theme.muted,
            ));
        }
        let cursor = Some(app.main_cursor[section]);
        let content = list(
            app,
            format!("main-{section}"),
            &frame.main[section],
            &app.main_lists[section],
            cursor,
            focused,
        );
        section_panes.push(pane(
            app,
            format!("pane-main-{section}"),
            Line::from_spans(title),
            focused,
            &frame.main_panes[section],
            content,
        ));
    }
    let layout = &app.config.layout;
    let beside = section_panes.len() == 2
        && match layout.main_split {
            MainSplit::SideBySide => true,
            MainSplit::Stacked => false,
            MainSplit::Flexible => app.size.get().0 >= layout.main_split_min_width,
        };
    if beside {
        let mut row = BoxRow::new();
        for panel in section_panes {
            row = row.flex(Flex::new(1, panel));
        }
        column = column.flex(Flex::new(1, row));
    } else {
        for panel in section_panes {
            column = column.flex(Flex::new(1, panel));
        }
    }
    if app.command_log {
        let content = SelectableListComponent::new("command-log", &frame.log, &frame.log_state)
            .policy(SelectableListPolicy {
                highlight: SelectableListHighlightPolicy::new("", false),
                ..SelectableListPolicy::interactive()
            })
            .styles(SelectableListStyles {
                focused: Style::new(),
                selected: Style::new(),
                ..SelectableListStyles::default()
            });
        column = column.child(
            SizeBox::new(pane(
                app,
                "pane-log",
                Line::from_spans(vec![Span::styled(" Command log ", theme.title)]),
                false,
                &frame.log_pane,
                content,
            ))
            .height(u64::from(layout.command_log.height.max(3))),
        );
    }
    Element::new(column)
}

fn key(app: &App, action: Action) -> String {
    app.keymap.key(action).map(display_key).unwrap_or_default()
}

fn hints(app: &App) -> Line {
    let theme = &app.theme;
    let actions: &[Action] = if app.window == Window::Main {
        if app.staging.is_some() {
            &[
                Action::StageToggle,
                Action::StageRange,
                Action::StageHunkMode,
                Action::StageSwitchSide,
                Action::StageDiscard,
                Action::StageCommit,
                Action::Back,
            ]
        } else {
            &[Action::MainNextHunk, Action::MainPreviousHunk, Action::Back]
        }
    } else {
        match app.current_view() {
            View::Files => &[
                Action::FileToggleStaged,
                Action::FileEnter,
                Action::FileCommit,
                Action::FileDiscard,
                Action::FileStash,
                Action::FileStageAll,
            ],
            View::Branches => &[
                Action::BranchCheckout,
                Action::BranchNew,
                Action::BranchMerge,
                Action::BranchRebase,
                Action::BranchDelete,
            ],
            View::Commits => &[
                Action::CommitViewFiles,
                Action::CommitSquash,
                Action::CommitFixup,
                Action::CommitReword,
                Action::CommitDrop,
                Action::CommitCherryPickCopy,
            ],
            View::Stash => &[Action::StashApply, Action::StashPop, Action::StashDrop],
            View::CommitFiles => &[
                Action::CommitFileView,
                Action::CommitFileTogglePatch,
                Action::CommitFilePatchMenu,
            ],
            View::RemoteBranches => &[Action::RemoteBranchCheckout, Action::RemoteBranchMerge],
            View::Remotes => &[Action::RemoteView, Action::RemoteFetch, Action::RemoteNew],
            View::Tags => &[Action::TagCheckout, Action::TagNew, Action::TagPush],
            _ => &[],
        }
    };
    let mut spans = Vec::new();
    for action in actions
        .iter()
        .chain(&[Action::Push, Action::Pull, Action::Help, Action::Quit])
    {
        let key = key(app, *action);
        if key.is_empty() {
            continue;
        }
        spans.push(Span::styled(key, theme.key));
        spans.push(Span::styled(
            format!(
                " {}  ",
                action
                    .description()
                    .split(['/', '('])
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_lowercase()
            ),
            theme.muted,
        ));
    }
    Line::from_spans(spans)
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn status_bar(app: &App) -> Element<'_> {
    let theme = &app.theme;
    let mut right = Vec::new();
    if !app.pending.is_empty() {
        right.push(Span::styled(
            format!(
                "{} … ",
                app.pending
                    .iter()
                    .map(|stroke| format!("{stroke:?}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            theme.warning,
        ));
    }
    if let Some(running) = &app.running {
        right.push(Span::styled(
            format!("{} {running} ", SPINNER[app.spinner % SPINNER.len()]),
            theme.accent.add_modifier(Modifier::BOLD),
        ));
    } else if let Some(toast) = &app.toast {
        right.push(Span::styled(
            format!(
                "{} {} ",
                if toast.error { "✗" } else { "✓" },
                toast.text.lines().next().unwrap_or_default()
            ),
            if toast.error {
                theme.error.add_modifier(Modifier::BOLD)
            } else {
                theme.success.add_modifier(Modifier::BOLD)
            },
        ));
    }
    Element::new(
        BoxRow::new()
            .flex_child(
                1,
                TextBlock::new(Text::from_lines(vec![hints(app)])).wrap(TextWrap::None),
            )
            .child(
                TextBlock::new(Text::from_lines(vec![Line::from_spans(right)]))
                    .wrap(TextWrap::None),
            ),
    )
}

/// Build the whole screen.
#[must_use]
pub fn tree<'a>(app: &'a App, frame: &'a Frame) -> Element<'a> {
    let theme = &app.theme;
    let (width, height) = app.size.get();
    if width < MIN_WIDTH || height < MIN_HEIGHT {
        return Element::new(
            Surface::new(TextBlock::new(Text::from_lines(vec![
                Line::from_spans(vec![Span::styled(
                    "bcode git",
                    theme.accent.add_modifier(Modifier::BOLD),
                )]),
                Line::from_spans(vec![Span::styled(
                    format!("Make the terminal at least {MIN_WIDTH}×{MIN_HEIGHT}."),
                    theme.muted,
                )]),
            ])))
            .background(theme.canvas)
            .content_style(theme.text),
        );
    }
    let layout = &app.config.layout;
    let portrait = match layout.portrait {
        Portrait::Auto => width < layout.portrait_below_width,
        Portrait::Always => true,
        Portrait::Never => false,
    };
    let full = app.screen_mode == ScreenMode::Full;
    let body: Element<'a> = if full && app.window == Window::Main {
        main_column(app, frame)
    } else if full {
        side_column(app, frame)
    } else if portrait {
        Element::new(
            Column::new()
                .flex(Flex::new(1, side_column(app, frame)))
                .flex(Flex::new(1, main_column(app, frame))),
        )
    } else {
        let side_percent = if app.screen_mode == ScreenMode::Half {
            layout.half_side_panel_width
        } else {
            layout.side_panel_width
        }
        .clamp(10, 90);
        Element::new(
            BoxRow::new()
                .flex(Flex::new(side_percent, side_column(app, frame)))
                .flex(Flex::new(100 - side_percent, main_column(app, frame))),
        )
    };
    let mut screen = Column::new().flex(Flex::new(1, body));
    // A hidden status bar still appears while work runs or a message is showing.
    if layout.status_bar || app.running.is_some() || app.toast.is_some() || !app.pending.is_empty()
    {
        screen = screen.child(SizeBox::new(status_bar(app)).height(1));
    }
    let screen = Surface::new(screen)
        .background(theme.canvas)
        .content_style(theme.text);
    // Popups float over the screen, which stays visible around them, as in lazygit. The modal
    // frame paints only its own panel, so it is stacked rather than placed in an `Overlay`,
    // which would blank everything behind the frame's full-screen placement area.
    match popup(app, frame).or_else(|| which_key(app)) {
        Some(foreground) => Element::new(Stack::new().child(screen).child(foreground)),
        None => Element::new(screen),
    }
}

const fn modal_theme(app: &App) -> ModalTheme {
    let theme = &app.theme;
    let background = theme.overlay;
    // Every modal style carries the panel background: content inherits the text style, and a
    // child that fills its area (labels, input boxes) would otherwise reset cells to the
    // terminal default, punching holes in the panel.
    ModalTheme::new(
        background,
        Style {
            modifiers: Modifier::EMPTY,
            ..on_panel(theme.border_focused, background)
        },
        on_panel(theme.title_focused, background),
        on_panel(theme.text, background),
        on_panel(theme.muted, background),
        on_panel(theme.accent, background),
    )
}

/// `style` drawn on `panel`'s background.
const fn on_panel(style: Style, panel: Style) -> Style {
    Style {
        bg: panel.bg,
        ..style
    }
}

fn modal<'a>(
    app: &'a App,
    title: &str,
    width: u16,
    height: u16,
    child: impl bmux_tui::component::Component + 'a,
) -> Element<'a> {
    let frame = ModalFrame::new(
        ModalSizing::new(Size::new(20, 5), Size::new(width, height), Insets::all(1)),
        modal_theme(app),
    )
    .title(Line::from_spans(vec![Span::styled(
        format!(" {title} "),
        app.theme.title_focused,
    )]))
    .placement(ModalPlacement::Centered);
    Element::new(ModalFrameComponent::new("popup", frame, child))
}

#[allow(clippy::too_many_lines)]
fn popup<'a>(app: &'a App, frame: &'a Frame) -> Option<Element<'a>> {
    let theme = &app.theme;
    match app.popups.last()? {
        Popup::Menu(menu) => {
            let visible = frame.popup.len();
            let height = u16::try_from(visible)
                .unwrap_or(u16::MAX)
                .saturating_add(if menu.filter.is_some() { 7 } else { 4 })
                .min(30);
            let items = list(
                app,
                "popup-list".into(),
                &frame.popup,
                &menu.list,
                (visible > 0).then_some(menu.selected.min(visible.saturating_sub(1))),
                true,
            );
            let mut column = Column::new();
            if let Some(filter) = &menu.filter {
                column = column.child(
                    TextInputBoxComponent::new(
                        "popup-filter",
                        TextInputPolicy::chat_composer(),
                        filter,
                    )
                    .placeholder("Type to filter…")
                    .policy(TextInputBoxPolicy::field().focused(true).rows(1, Some(1)))
                    .styles(input_styles(app)),
                );
            }
            column = column.flex(Flex::new(1, items));
            Some(modal(app, &menu.title, 80, height, column))
        }
        Popup::Text(view) => {
            let height = u16::try_from(view.lines.len())
                .unwrap_or(u16::MAX)
                .saturating_add(4)
                .min(40);
            let items = list(
                app,
                "popup-text".into(),
                &frame.popup,
                &view.list,
                Some(view.selected),
                false,
            );
            Some(modal(app, &view.title, 110, height, items))
        }
        Popup::Confirm(confirm) => Some(modal(
            app,
            &confirm.title,
            70,
            8,
            Column::new()
                .child(TextBlock::new(Text::from_lines(vec![Line::from_spans(
                    vec![Span::styled(confirm.message.clone(), theme.text)],
                )])))
                .child(TextBlock::new(Text::from_lines(vec![
                    Line::new(),
                    Line::from_spans(vec![
                        Span::styled(key(app, Action::ConfirmYes), theme.key),
                        Span::styled(" confirm   ", theme.muted),
                        Span::styled(key(app, Action::ConfirmNo), theme.key),
                        Span::styled(" cancel", theme.muted),
                    ]),
                ]))),
        )),
        Popup::Prompt(prompt) => {
            let rows = if prompt.multiline { 8 } else { 1 };
            let field = |id: &'static str,
                         state: &'a std::cell::RefCell<_>,
                         rows: u16,
                         focused: bool,
                         placeholder: &'static str| {
                TextInputBoxComponent::new(id, TextInputPolicy::chat_composer(), state)
                    .placeholder(placeholder)
                    .policy(
                        TextInputBoxPolicy::field()
                            .focused(focused)
                            .rows(rows, Some(rows)),
                    )
                    .styles(input_styles(app))
            };
            let label = |text: &'static str, focused: bool| {
                TextBlock::new(Text::from_lines(vec![Line::from_spans(vec![
                    Span::styled(
                        text,
                        if focused {
                            theme.title_focused
                        } else {
                            theme.muted
                        },
                    ),
                ])]))
                .wrap(TextWrap::None)
            };
            let mut column = Column::new();
            let field_rows = if let Some(body) = &prompt.body {
                column = column
                    .child(label("Summary", !prompt.body_focused))
                    .child(field(
                        "popup-input",
                        &prompt.input,
                        1,
                        !prompt.body_focused,
                        "First line of the message",
                    ))
                    .child(label("Description", prompt.body_focused))
                    .child(field(
                        "popup-body",
                        body.as_ref(),
                        DESCRIPTION_ROWS,
                        prompt.body_focused,
                        "Optional details",
                    ));
                // Labels plus a bordered one-line summary and the bordered description.
                1 + 3 + 1 + DESCRIPTION_ROWS + 2
            } else {
                column = column.child(field("popup-input", &prompt.input, rows, true, ""));
                rows + 2
            };
            let submit_label = if prompt.body.is_some() && prompt.body_focused {
                format!(
                    "{} newline  {} submit  ",
                    key(app, Action::InputSubmit),
                    key(app, Action::InputSubmitFromAnyField)
                )
            } else {
                format!("{} submit  ", key(app, Action::InputSubmit))
            };
            let mut hint = vec![
                Span::styled(submit_label, theme.key),
                Span::styled(
                    format!("{} cancel", key(app, Action::InputCancel)),
                    theme.muted,
                ),
            ];
            if prompt.body.is_some() {
                hint.push(Span::styled(
                    format!(
                        "  {} {}",
                        key(app, Action::InputSwitchField),
                        if prompt.body_focused {
                            "summary"
                        } else {
                            "description"
                        }
                    ),
                    theme.muted,
                ));
            }
            if prompt.multiline {
                hint.push(Span::styled("  shift+⏎ newline", theme.muted));
            }
            if let crate::popup::PromptKind::Commit { amend, .. } = &prompt.kind {
                let length = prompt.summary().chars().count();
                hint.push(Span::styled("  ", theme.muted));
                hint.push(Span::styled(
                    format!("{length}/50"),
                    if length > 72 {
                        theme.error
                    } else if length > 50 {
                        theme.warning
                    } else {
                        theme.success
                    },
                ));
                hint.push(Span::styled(
                    format!("  {} ", key(app, Action::InputToggleAmend)),
                    theme.key,
                ));
                hint.push(Span::styled(
                    if *amend { "amend: on" } else { "amend: off" },
                    if *amend { theme.warning } else { theme.muted },
                ));
            }
            column = column.child(
                TextBlock::new(Text::from_lines(vec![Line::from_spans(hint)])).wrap(TextWrap::None),
            );
            // The status bar is covered while a popup is open, so errors show in the popup.
            let error = app.toast.as_ref().filter(|toast| toast.error);
            if let Some(toast) = error {
                column = column.child(
                    TextBlock::new(Text::from_lines(vec![Line::from_spans(vec![
                        Span::styled(
                            format!("✗ {}", toast.text.lines().next().unwrap_or_default()),
                            theme.error,
                        ),
                    ])]))
                    .wrap(TextWrap::None),
                );
            }
            // Fields, the hint row, an optional error row, and the frame.
            let height = field_rows + 1 + u16::from(error.is_some()) + 4;
            Some(modal(app, &prompt.title, 90, height, column))
        }
    }
}

const fn input_styles(app: &App) -> TextInputBoxStyles {
    let theme = &app.theme;
    TextInputBoxStyles {
        text: theme.text,
        focused_text: theme.text,
        disabled_text: theme.muted,
        placeholder: theme.muted,
        selection: theme.range,
        border: theme.border,
        focused_border: theme.border_focused,
        background: theme.overlay,
        focused_background: theme.overlay,
        disabled_background: theme.overlay,
    }
}

fn which_key(app: &App) -> Option<Element<'_>> {
    if app.pending.is_empty() {
        return None;
    }
    let theme = &app.theme;
    let scopes: Vec<Scope> = if app.window == Window::Main {
        vec![
            if app.staging.is_some() {
                Scope::Staging
            } else {
                Scope::Main
            },
            Scope::Global,
        ]
    } else {
        vec![app.current_view().scope(), Scope::List, Scope::Global]
    };
    let mut lines = Vec::new();
    for scope in scopes {
        for binding in app
            .keymap
            .registry()
            .continuations(scope.name(), &app.pending)
        {
            let description = Action::from_id(&binding.action).map_or_else(
                || binding.action.clone(),
                |action| action.description().to_owned(),
            );
            lines.push(Line::from_spans(vec![
                Span::styled(format!("{:<8}", display_key(&binding.key)), theme.key),
                Span::styled(description, theme.text),
            ]));
        }
    }
    if lines.is_empty() {
        return None;
    }
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(4);
    Some(modal(
        app,
        "Continue with…",
        50,
        height,
        TextBlock::new(Text::from_lines(lines)),
    ))
}
