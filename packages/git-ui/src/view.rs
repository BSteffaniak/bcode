//! Presentation tree assembled from BMUX components.

use std::cell::Cell;

use bcode_git::models::{ChangeKind, CommandStatus, DiffFileStatus, StatusEntry, SubmoduleState};
use bmux_tui::component::{Element, LayoutId};
use bmux_tui::composition::{
    Column, Flex, HitArea, Row as BoxRow, SizeBox, Stack, Surface, TextBlock,
};
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::style::{Modifier, Style};
use bmux_tui::text::{Line, Span, Text, TextWrap};
use bmux_tui_components::modal_frame::{
    ModalFrame, ModalFrameComponent, ModalPlacement, ModalSizing, ModalTheme,
};
use bmux_tui_components::pane::{Pane, PaneComponent, PaneState, PaneStyles, PaneTitleAction};
use bmux_tui_components::panel_group::{PanelGroupAxis, PanelGroupComponent};
use bmux_tui_components::scroll_view::{ScrollSource, ScrollViewComponent};
use bmux_tui_components::scrollbar_layout::ScrollbarAxisLayoutMode;
use bmux_tui_components::selectable_list::{
    SelectableListComponent, SelectableListHighlightPolicy, SelectableListItem,
    SelectableListPolicy, SelectableListState, SelectableListStyles, list_viewport_id,
};
use bmux_tui_components::tab_bar::{
    TabBarComponent, TabBarPolicy, TabBarState, TabBarStyles, TabItem,
};
use bmux_tui_components::terminal_view::{
    TerminalState, TerminalStatus, TerminalViewComponent, TerminalViewMode,
};
use bmux_tui_components::text_input::TextInputPolicy;
use bmux_tui_components::text_input_box::{
    TextInputBoxComponent, TextInputBoxPolicy, TextInputBoxStyles,
};

use crate::actions::head_label;
use crate::activity::{Load, Subject};
use crate::app::{App, Row, ScreenMode, View, Window};
use crate::config::{MainSplit, Portrait};
use crate::data::{FileNode, SectionKind, commit_line, format_time, status_style};
use crate::keymap::{Action, Scope, display_key};
use crate::popup::Popup;

/// Items and per-frame state owned for one layout and paint pass.
pub struct Frame {
    side: Vec<(Window, View, Vec<SelectableListItem>)>,
    main: Vec<Vec<SelectableListItem>>,
    main_gutters: Vec<Vec<Option<usize>>>,
    main_compact: Vec<Vec<Option<Line>>>,
    popup: Vec<SelectableListItem>,
    side_panes: Vec<Cell<PaneState>>,
    side_tabs: Vec<Vec<TabItem<'static>>>,
    tab_states: Vec<std::cell::RefCell<TabBarState>>,
    main_panes: [Cell<PaneState>; 2],
    log_pane: Cell<PaneState>,
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
        let pane_state = || Cell::new(PaneState::new(Rect::default()));
        let main = (0..2).map(|section| main_items(app, section)).collect();
        let main_gutters = (0..2)
            .map(|section| {
                app.main
                    .as_ref()
                    .and_then(|main| main.sections.get(section))
                    .map(|section| {
                        section
                            .rows
                            .iter()
                            .map(|row| row.wrap.then_some(row.gutter_spans))
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect();
        let popup = popup_items(app);
        let main_compact = (0..2)
            .map(|section| {
                app.main
                    .as_ref()
                    .and_then(|main| main.sections.get(section))
                    .map(|section| section.rows.iter().map(|row| row.compact.clone()).collect())
                    .unwrap_or_default()
            })
            .collect();
        Self {
            main,
            main_gutters,
            main_compact,
            popup,
            side_panes: side.iter().map(|_| pane_state()).collect(),
            side_tabs: side_tab_items(app, &side),
            tab_states: side
                .iter()
                .map(|_| std::cell::RefCell::new(TabBarState::new(None)))
                .collect(),
            main_panes: [pane_state(), pane_state()],
            log_pane: pane_state(),
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
                worktree.branch.as_ref().map_or_else(
                    || Span::styled("(detached)", theme.warning),
                    |branch| Span::styled(branch.clone(), app.cx.branches.local(branch)),
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
            // The name's color says what state the branch is in (or what a rule says).
            let style = app.cx.branches.local(&branch.name);
            let mut spans = vec![
                Span::styled(
                    format!("{:>3} ", format_time(branch.commit_time, true)),
                    theme.date,
                ),
                Span::styled(if branch.is_head { "✱ " } else { "  " }, style),
                Span::styled(branch.name.clone(), style),
            ];
            spans.extend(
                app.activity
                    .row_spans(&Subject::Branch(branch.name.clone()), theme),
            );
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
            if let Some(pull) = app.cx.branches.pull(&branch.name) {
                spans.extend(crate::pulls::marker(pull, theme));
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
            let mut spans = vec![
                Span::styled("☁ ", theme.branch_remote),
                Span::styled(
                    remote.name.clone(),
                    theme.branch_remote.add_modifier(Modifier::BOLD),
                ),
            ];
            spans.extend(
                app.activity
                    .row_spans(&Subject::Remote(remote.name.clone()), theme),
            );
            spans.push(Span::styled(format!("  {count} branches"), theme.muted));
            spans.push(Span::styled(
                format!("  {}", remote.fetch_url.clone().unwrap_or_default()),
                theme.muted,
            ));
            Line::from_spans(spans)
        }
        Row::RemoteBranch(index) => {
            let branch = &snapshot.remote_branches[index];
            let (_, style) = app.cx.branches.remote(&branch.name);
            let mut spans = vec![
                Span::styled(
                    format!("{:>3} ", format_time(branch.commit_time, true)),
                    theme.date,
                ),
                Span::styled(branch.name.clone(), style),
            ];
            spans.extend(app.activity.row_spans(
                &Subject::RemoteBranch {
                    remote: branch.remote.clone(),
                    name: branch.name.clone(),
                },
                theme,
            ));
            spans.push(Span::styled(format!("  {}", branch.summary), theme.muted));
            Line::from_spans(spans)
        }
        Row::Tag(index) => {
            let tag = &snapshot.tags[index];
            let mut spans = vec![
                Span::styled(if tag.annotated { "◆ " } else { "◇ " }, theme.tag),
                Span::styled(tag.name.clone(), theme.tag),
            ];
            spans.extend(
                app.activity
                    .row_spans(&Subject::Tag(tag.name.clone()), theme),
            );
            spans.push(Span::styled(format!("  {}", tag.summary), theme.muted));
            Line::from_spans(spans)
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
                &app.cx.branches,
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
                &app.cx.branches,
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

/// Wraps a list so a cursor that moved since the last frame is revealed against the measured
/// viewport before painting. Otherwise the list keeps its own scroll position, which the
/// wheel and the scrollbar change without moving the cursor.
struct Revealed<'a, 'state>(SelectableListComponent<'a, 'state>, bool);

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
        if self.1 {
            self.0.reveal_focus(layout);
        }
        self.0.paint(layout, cx);
    }

    fn revision(&self) -> bmux_tui::component::ComponentRevision {
        self.0.revision()
    }
}

/// A main-view list whose rows wrap to the width layout gives it. Each source row stays one
/// selectable item (so staging selections and hit regions keep their row identity); its gutter
/// spans start the first visual row and become blank padding on the continuation rows. Rows
/// without a gutter entry are fixed and clipped instead.
struct WrappedList<'a> {
    id: String,
    source: &'a [SelectableListItem],
    gutters: &'a [Option<usize>],
    compact: &'a [Option<Line>],
    state: &'a Cell<SelectableListState>,
    policy: SelectableListPolicy,
    styles: SelectableListStyles,
    reveal: bool,
    wrapped: std::cell::RefCell<Option<(u64, Vec<SelectableListItem>)>>,
}

impl WrappedList<'_> {
    /// Rows wrapped for `width`, computed once per width.
    fn with_items<T>(&self, width: u64, use_items: impl FnOnce(&[SelectableListItem]) -> T) -> T {
        let mut cache = self.wrapped.borrow_mut();
        if cache.as_ref().is_none_or(|(cached, _)| *cached != width) {
            let columns = usize::try_from(width).unwrap_or(usize::MAX);
            let items = self
                .source
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    self.gutters.get(index).copied().flatten().map_or_else(
                        || fit_fixed(item, self.compact.get(index).cloned().flatten(), columns),
                        |gutter| wrap_item(item, gutter, columns),
                    )
                })
                .collect();
            *cache = Some((width, items));
        }
        use_items(cache.as_ref().map_or(&[], |(_, items)| items.as_slice()))
    }

    fn component<'b>(&'b self, items: &'b [SelectableListItem]) -> SelectableListComponent<'b, 'b> {
        SelectableListComponent::new(self.id.clone(), items, self.state)
            .policy(self.policy)
            .styles(self.styles)
    }
}

impl bmux_tui::component::Component for WrappedList<'_> {
    fn layout(
        &self,
        constraints: bmux_tui::component::Constraints,
        cx: &mut bmux_tui::component::LayoutCx,
    ) -> bmux_tui::component::LayoutNode {
        self.with_items(constraints.max_width(), |items| {
            self.component(items).layout(constraints, cx)
        })
    }

    fn paint(
        &self,
        layout: &bmux_tui::component::LayoutNode,
        cx: &mut bmux_tui::paint::PaintCx<'_, '_>,
    ) {
        self.with_items(layout.size.width, |items| {
            let list = self.component(items);
            if self.reveal {
                list.reveal_focus(layout);
            }
            list.paint(layout, cx);
        });
    }

    fn revision(&self) -> bmux_tui::component::ComponentRevision {
        self.component(self.source).revision()
    }
}

/// A fixed row as is when it fits `width`, else its compact rendering (clipped if needed).
fn fit_fixed(item: &SelectableListItem, compact: Option<Line>, width: usize) -> SelectableListItem {
    use bmux_tui::text_width::display_width;

    let fits = item.lines.iter().all(|line| {
        line.spans
            .iter()
            .map(|span| display_width(&span.content))
            .sum::<usize>()
            <= width
    });
    match compact {
        Some(compact) if !fits => SelectableListItem::rich(item.id.clone(), compact),
        _ => item.clone(),
    }
}

/// Wrap one row's content to `width` columns, keeping its first `gutter` spans as a prefix.
fn wrap_item(item: &SelectableListItem, gutter: usize, width: usize) -> SelectableListItem {
    use bmux_tui::text::{TextWrap, TextWrapGeometry, wrap_line_with_geometry};
    use bmux_tui::text_width::display_width;

    let Some(line) = item.lines.first() else {
        return item.clone();
    };
    let split = gutter.min(line.spans.len());
    let (prefix, body) = line.spans.split_at(split);
    let prefix_width: usize = prefix.iter().map(|span| display_width(&span.content)).sum();
    // Too narrow for any content beside the gutter: leave the row to be clipped.
    let Some(content_width) = width.checked_sub(prefix_width).filter(|width| *width > 0) else {
        return item.clone();
    };
    let rows = wrap_line_with_geometry(
        &Line::from_spans(body.to_vec()),
        TextWrapGeometry::uniform(content_width),
        TextWrap::Word,
    );
    if rows.len() <= 1 {
        return item.clone();
    }
    let padding: Vec<Span> = prefix
        .iter()
        .map(|span| Span::styled(" ".repeat(display_width(&span.content)), span.style))
        .collect();
    let lines = rows
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            let mut spans = if index == 0 {
                prefix.to_vec()
            } else {
                padding.clone()
            };
            spans.extend(row.spans);
            Line::from_spans(spans)
        })
        .collect::<Vec<_>>();
    SelectableListItem::multiline(item.id.clone(), lines)
}

/// Scrollbar of popup lists: inside the popup's frame, shown only while the list overflows.
/// (Panes carry theirs on the border instead.)
pub const POPUP_SCROLLBAR: ScrollbarAxisLayoutMode = ScrollbarAxisLayoutMode::OverlayOnOverflow;

/// Behaviour shared by every list, with the given integrated scrollbar.
pub const fn list_base_policy(scrollbar: ScrollbarAxisLayoutMode) -> SelectableListPolicy {
    SelectableListPolicy {
        highlight: SelectableListHighlightPolicy::new("", false),
        ..SelectableListPolicy::interactive()
    }
    .scrollbar(scrollbar)
}

/// Selection, focus and styling shared by every list. `cursor` is the highlighted row, if
/// any; lists read as pages have none. Also returns whether the cursor moved since the last
/// frame, in which case it is scrolled into view.
fn list_policy(
    app: &App,
    state: &Cell<SelectableListState>,
    cursor: Option<usize>,
    focused: bool,
    scrollbar: ScrollbarAxisLayoutMode,
) -> (SelectableListPolicy, SelectableListStyles, bool) {
    let mut current = state.get();
    let moved = cursor.is_some() && current.focused() != cursor;
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
    (list_base_policy(scrollbar), styles, moved)
}

fn list<'a>(
    app: &'a App,
    id: String,
    items: &'a [SelectableListItem],
    state: &'a Cell<SelectableListState>,
    cursor: Option<usize>,
    focused: bool,
    scrollbar: ScrollbarAxisLayoutMode,
) -> Revealed<'a, 'a> {
    let (policy, styles, moved) = list_policy(app, state, cursor, focused, scrollbar);
    Revealed(
        SelectableListComponent::new(id, items, state)
            .policy(policy)
            .styles(styles),
        moved,
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

/// A pane whose title row is clickable and carries buttons.
/// A pane title: text, or a component such as a row holding a tab bar.
enum PaneTitle<'a> {
    Text(Line),
    Content(Element<'a>),
}

impl<'a> PaneTitle<'a> {
    /// The tabbed title of a side window when it has one, else `text`.
    fn side(app: &'a App, frame: &'a Frame, position: usize, text: Line) -> Self {
        tabbed_title(app, frame, position).map_or(Self::Text(text), Self::Content)
    }
}

/// What a pane scrolls through its border scrollbar: the scrolled viewport's layout id and
/// the state holding its position.
type Scrolled<'a> = Option<(LayoutId, &'a dyn ScrollSource)>;

#[allow(clippy::too_many_arguments)]
fn pane_with<'a>(
    app: &'a App,
    id: impl Into<String>,
    title: PaneTitle<'a>,
    focused: bool,
    state: &'a Cell<PaneState>,
    actions: &[(&str, &str)],
    scrolled: Scrolled<'a>,
    child: impl bmux_tui::component::Component + 'a,
) -> PaneComponent<'a, 'a> {
    let mut current = state.get();
    current.interaction.focused = focused;
    state.set(current);
    let (text, title_content) = match title {
        PaneTitle::Text(line) => (Some(line), None),
        PaneTitle::Content(content) => (None, Some(content)),
    };
    let mut chrome = Pane::new().title_hit(true);
    if let Some(text) = text {
        chrome = chrome.title(text);
    }
    chrome = chrome.styles(PaneStyles {
        background: None,
        border: app.theme.border,
        focused_border: app.theme.border_focused,
    });
    for (id, label) in actions {
        chrome = chrome.action(PaneTitleAction::new(
            *id,
            Line::from_spans(vec![Span::styled((*label).to_owned(), app.theme.muted)]),
        ));
    }
    let mut component = PaneComponent::new(id.into(), chrome, state, child);
    if let Some((viewport, source)) = scrolled {
        component = component.border_scrollbar(viewport, source);
    }
    match title_content {
        Some(title) => component.title_content(title),
        None => component,
    }
}

/// Whether side windows stack above the main view.
pub const fn portrait(app: &App) -> bool {
    let layout = &app.config.layout;
    match layout.portrait {
        Portrait::Auto => app.size.get().0 < layout.portrait_below_width,
        Portrait::Always => true,
        Portrait::Never => false,
    }
}

/// Axis of the split between the side column and the main column.
pub const fn root_axis(app: &App) -> PanelGroupAxis {
    if portrait(app) {
        PanelGroupAxis::Vertical
    } else {
        PanelGroupAxis::Horizontal
    }
}

/// Axis of the staged/unstaged split.
pub const fn sections_axis(app: &App) -> PanelGroupAxis {
    let layout = &app.config.layout;
    let beside = match layout.main_split {
        MainSplit::SideBySide => true,
        MainSplit::Stacked => false,
        MainSplit::Flexible => app.size.get().0 >= layout.main_split_min_width,
    };
    if beside {
        PanelGroupAxis::Horizontal
    } else {
        PanelGroupAxis::Vertical
    }
}

/// Position, filter and trailing padding after a side window's name or tabs.
fn title_suffix(app: &App, view: View) -> Vec<Span> {
    let theme = &app.theme;
    let mut spans = Vec::new();
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
    // What is loading into this view.
    let load = match view {
        View::Status => Some((Load::Repository, "")),
        View::Branches => Some((Load::PullRequests, " PRs")),
        View::Commits => Some((Load::MoreCommits, " more")),
        View::CommitFiles => Some((Load::CommitFiles, "")),
        View::SubCommits => Some((Load::SubCommits, "")),
        _ => None,
    };
    if let Some((_, label)) = load.filter(|(load, _)| app.activity.loading(*load)) {
        spans.push(Span::raw(" "));
        spans.push(app.activity.spinner_span(theme.activity));
        spans.push(Span::styled(label, theme.muted));
    }
    spans.push(Span::raw(" "));
    spans
}

/// Tab items of every side window, in frame order; ids are the views' configuration names, so a
/// tab's hit region is `tabs-<window number>.<view name>`.
fn side_tab_items(
    app: &App,
    side: &[(Window, View, Vec<SelectableListItem>)],
) -> Vec<Vec<TabItem<'static>>> {
    side.iter()
        .map(|(window, _, _)| {
            app.tabs(*window)
                .iter()
                .map(|view| TabItem::new(crate::layout::view_name(*view), view.label()))
                .collect()
        })
        .collect()
}

/// A clickable title for a side window with several tabs showing one of them: its number,
/// a tab bar, then its position and filter. Other windows keep a text title.
fn tabbed_title<'a>(app: &'a App, frame: &'a Frame, position: usize) -> Option<Element<'a>> {
    let (window, view, _) = &frame.side[position];
    let tabs = app.tabs(*window);
    let selected = tabs.iter().position(|tab| tab == view)?;
    if tabs.len() < 2 {
        return None;
    }
    let theme = &app.theme;
    let title_style = if app.window == *window {
        theme.title_focused
    } else {
        theme.title
    };
    frame.tab_states[position].replace(TabBarState::new(Some(selected)));
    let bar = TabBarComponent::new(
        format!("tabs-{}", window.number()),
        &frame.side_tabs[position],
        &frame.tab_states[position],
    )
    .policy(TabBarPolicy::interactive().padding("", "").separator(" · "))
    .styles(TabBarStyles {
        normal: theme.muted,
        selected: title_style,
        focused: Style::new(),
        hovered: theme.text,
        pressed: title_style,
        disabled: theme.muted,
        separator: theme.muted,
    });
    let text = |spans: Vec<Span>| {
        TextBlock::new(Text::from_lines(vec![Line::from_spans(spans)])).wrap(TextWrap::None)
    };
    Some(Element::new(
        BoxRow::new()
            .child(text(vec![Span::styled(
                format!("[{}] ", window.number()),
                theme.key,
            )]))
            .child(bar)
            .child(text(title_suffix(app, *view))),
    ))
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
            spans.push(Span::styled("Commits · ", title_style));
            // A branch's commits: its name in its branch color.
            if let Some(sub) = &app.sub_commits {
                for (text, style) in app.cx.branches.reference(&sub.title) {
                    spans.push(Span::styled(text, style.add_modifier(Modifier::BOLD)));
                }
            }
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
    spans.extend(title_suffix(app, view));
    Line::from_spans(spans)
}

fn side_column<'a>(app: &'a App, frame: &'a Frame) -> Element<'a> {
    let mut group = PanelGroupComponent::new(
        crate::panels::SIDE,
        crate::panels::group(PanelGroupAxis::Vertical, &app.theme),
        &app.panels.side,
    );
    for index in &app.panels.visible {
        if let Some(position) = frame
            .side
            .iter()
            .position(|(window, _, _)| *window == Window::Side(*index))
        {
            group = group.child(side_pane(app, frame, position));
        }
    }
    Element::new(group)
}

/// One side window: its list, or only its title row while collapsed.
fn side_pane<'a>(app: &'a App, frame: &'a Frame, position: usize) -> Element<'a> {
    let (window, view, items) = &frame.side[position];
    let Window::Side(index) = *window else {
        return Element::new(TextBlock::new(""));
    };
    let focused = app.window == *window;
    let collapsed = app.panels.is_collapsed(index);
    let actions = [
        ("collapse", if collapsed { "▸" } else { "▾" }),
        ("maximize", "□"),
        ("close", "✕"),
    ];
    let title = window_title(app, *window, *view);
    let id = format!("pane-{}", window.number());
    if collapsed {
        return Element::new(pane_with(
            app,
            id,
            PaneTitle::side(app, frame, position, title),
            focused,
            &frame.side_panes[position],
            &actions,
            None,
            TextBlock::new(""),
        ));
    }
    let cursor = (!app.rows(*view).is_empty()).then(|| app.cursor(*view));
    let list_id = crate::scroll::side_list(*window);
    let content = list(
        app,
        list_id.clone(),
        items,
        &app.lists[view],
        cursor,
        focused,
        ScrollbarAxisLayoutMode::Hidden,
    );
    Element::new(pane_with(
        app,
        id,
        PaneTitle::side(app, frame, position, title),
        focused,
        &frame.side_panes[position],
        &actions,
        Some((list_viewport_id(&LayoutId::new(list_id)), &app.lists[view])),
        content,
    ))
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
        let spans = if app.loaded_once && !app.activity.loading(Load::Main) {
            Vec::new()
        } else {
            vec![
                Span::raw("  "),
                app.activity.spinner_span(theme.activity),
                Span::styled(
                    if app.loaded_once {
                        " Loading…"
                    } else {
                        " Loading repository…"
                    },
                    theme.muted,
                ),
            ]
        };
        let loading = TextBlock::new(Text::from_lines(vec![Line::from_spans(spans)]));
        column = column.flex(Flex::new(
            1,
            pane(
                app,
                "pane-main",
                Line::new(),
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
        // Untitled sections (messages) leave the border unbroken instead of padding nothing.
        let mut title = Vec::new();
        if !data.title.is_empty() {
            title.push(Span::styled(
                format!(" {} ", data.title),
                if focused {
                    theme.title_focused
                } else {
                    theme.title
                },
            ));
        }
        if let Some(crate::data::MainRequest::File(entry)) =
            app.main.as_ref().map(|main| &main.request)
        {
            title.push(Span::styled(format!("{} ", entry.path), theme.text));
        }
        // The old content stays while new content loads.
        if section == 0 && app.activity.loading(Load::Main) {
            title.push(app.activity.spinner_span(theme.activity));
            title.push(Span::raw(" "));
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
        // The main view reads as a page; only staging shows a cursor, in its section.
        let cursor = (focused && app.staging.is_some()).then(|| app.main_cursor[section]);
        let list_id = crate::scroll::main_list(section);
        let content = if app.config.diff.wrap_lines {
            let (policy, styles, reveal) = list_policy(
                app,
                &app.main_lists[section],
                cursor,
                focused,
                ScrollbarAxisLayoutMode::Hidden,
            );
            Element::new(WrappedList {
                id: list_id.clone(),
                source: &frame.main[section],
                gutters: &frame.main_gutters[section],
                compact: &frame.main_compact[section],
                state: &app.main_lists[section],
                policy,
                styles,
                reveal,
                wrapped: std::cell::RefCell::new(None),
            })
        } else {
            Element::new(list(
                app,
                list_id.clone(),
                &frame.main[section],
                &app.main_lists[section],
                cursor,
                focused,
                ScrollbarAxisLayoutMode::Hidden,
            ))
        };
        section_panes.push(pane_with(
            app,
            format!("pane-main-{section}"),
            PaneTitle::Text(Line::from_spans(title)),
            focused,
            &frame.main_panes[section],
            &[("maximize", "□")],
            Some((
                list_viewport_id(&LayoutId::new(list_id.clone())),
                &app.main_lists[section],
            )),
            content,
        ));
    }
    let mut panes = section_panes.into_iter();
    let content: Option<Element<'a>> = match (panes.next(), panes.next()) {
        (Some(first), Some(second)) => Some(Element::new(
            PanelGroupComponent::new(
                crate::panels::SECTIONS,
                crate::panels::group(sections_axis(app), theme),
                &app.panels.sections,
            )
            .child(first)
            .child(second),
        )),
        (Some(only), None) => Some(Element::new(only)),
        _ => None,
    };
    let mut group = PanelGroupComponent::new(
        crate::panels::MAIN,
        crate::panels::group(PanelGroupAxis::Vertical, theme),
        &app.panels.main,
    );
    group = match content {
        Some(content) => group.child(content),
        None => group.child(column),
    };
    if app.command_log {
        group = group.child(pane_with(
            app,
            "pane-log",
            PaneTitle::Text(Line::from_spans(vec![Span::styled(
                " Command log ",
                theme.title,
            )])),
            false,
            &frame.log_pane,
            &[("close", "✕")],
            Some((LayoutId::new(crate::scroll::LOG_LIST), &app.log_scroll)),
            command_log(app),
        ));
    }
    Element::new(group)
}

/// Rows of output shown under each command-log entry (its end, when longer).
const LOG_OUTPUT_ROWS: u64 = 12;

/// The command log, newest first: each command, its status and duration, and the end of its
/// terminal output (live while it runs). The whole log scrolls.
fn command_log(app: &App) -> Element<'_> {
    let theme = &app.theme;
    let text = |spans: Vec<Span>| {
        TextBlock::new(Text::from_lines(vec![Line::from_spans(spans)])).wrap(TextWrap::None)
    };
    let mut entries = Column::new();
    for record in &app.log_records {
        let mark = match record.status {
            CommandStatus::Running => app.activity.spinner_span(theme.activity),
            CommandStatus::Succeeded => Span::styled("✓", theme.success),
            CommandStatus::Failed => Span::styled("✗", theme.error),
        };
        let mut header = vec![
            mark,
            Span::raw(" "),
            Span::styled(record.display(), theme.text),
        ];
        if !record.running() {
            header.push(Span::styled(
                format!("  {}ms", record.duration_ms),
                theme.muted,
            ));
        }
        entries = entries.child(text(header));
        let output = app
            .log_terminals
            .get(&record.id)
            .filter(|output| !output.terminal.is_empty());
        if let Some(output) = output {
            entries = entries.child(
                Surface::new(
                    TerminalViewComponent::new(format!("log-{}", record.id), &output.terminal)
                        .mode(TerminalViewMode::Reflow)
                        .max_rows(LOG_OUTPUT_ROWS)
                        .cursor(false),
                )
                .padding(Insets::new(0, 0, 0, 2)),
            );
            if record.output_truncated {
                entries = entries.child(text(vec![Span::styled(
                    "  … later output not kept",
                    theme.muted,
                )]));
            }
        } else if let Some(error) = &record.error {
            // Without recorded output (it ran on pipes), the failure's reason.
            for line in error.lines() {
                entries = entries.child(text(vec![Span::styled(format!("  {line}"), theme.error)]));
            }
        }
    }
    Element::new(
        ScrollViewComponent::new(
            crate::scroll::LOG_LIST,
            bmux_tui::component::LogicalSize::new(0, 0),
            app.log_scroll.get(),
            entries,
        )
        .fill()
        .retain_state(&app.log_scroll),
    )
}

/// Widest a terminal popup gets.
pub const TERMINAL_POPUP_WIDTH: u16 = 140;

/// The view of a terminal popup's terminal (shared with scroll capture, so both agree on its
/// scrollbar).
pub fn terminal_popup_view(terminal: &TerminalState) -> TerminalViewComponent<'_> {
    TerminalViewComponent::new(crate::scroll::TERMINAL_LIST, terminal).scrollbar(POPUP_SCROLLBAR)
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
    // Running work shows over messages; clicking it opens the command log, which shows its
    // live output.
    let activity = app.activity.status_spans(theme);
    if activity.is_none()
        && let Some(toast) = &app.toast
    {
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
    let text = |spans| TextBlock::new(Text::from_lines(vec![Line::from_spans(spans)]));
    let mut row = BoxRow::new()
        .flex_child(
            1,
            TextBlock::new(Text::from_lines(vec![hints(app)])).wrap(TextWrap::None),
        )
        .child(text(right).wrap(TextWrap::None));
    if let Some(activity) = activity {
        row = row.child(HitArea::new(
            STATUS_ACTIVITY,
            text(activity).wrap(TextWrap::None),
        ));
    }
    Element::new(row)
}

/// Hit region of the running work in the status bar.
pub const STATUS_ACTIVITY: &str = "status-activity";

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
    let full = app.screen_mode == ScreenMode::Full;
    let body: Element<'a> = if full && app.window == Window::Main {
        main_column(app, frame)
    } else if full {
        // The focused side window alone.
        frame
            .side
            .iter()
            .position(|(window, _, _)| *window == app.last_side)
            .map_or_else(
                || side_column(app, frame),
                |position| side_pane(app, frame, position),
            )
    } else {
        Element::new(
            PanelGroupComponent::new(
                crate::panels::ROOT,
                crate::panels::group(root_axis(app), theme),
                &app.panels.root,
            )
            .child(side_column(app, frame))
            .child(main_column(app, frame)),
        )
    };
    let mut screen = Column::new().flex(Flex::new(1, body));
    // A hidden status bar still appears while work runs or a message is showing.
    if layout.status_bar || app.activity.running() || app.toast.is_some() || !app.pending.is_empty()
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
            let notes = u16::try_from(menu.notes.len()).unwrap_or(u16::MAX);
            let height = u16::try_from(visible)
                .unwrap_or(u16::MAX)
                .saturating_add(if menu.filter.is_some() { 7 } else { 4 })
                .saturating_add(notes)
                .min(30u16.saturating_add(notes));
            let items = list(
                app,
                crate::scroll::MENU_LIST.into(),
                &frame.popup,
                &menu.list,
                (visible > 0).then_some(menu.selected.min(visible.saturating_sub(1))),
                true,
                POPUP_SCROLLBAR,
            );
            let mut column = Column::new();
            if !menu.notes.is_empty() {
                column = column.child(
                    TextBlock::new(Text::from_lines(menu.notes.clone())).wrap(TextWrap::None),
                );
            }
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
            // Read as a page: no cursor row.
            let items = list(
                app,
                crate::scroll::TEXT_LIST.into(),
                &frame.popup,
                &view.list,
                None,
                false,
                POPUP_SCROLLBAR,
            );
            Some(modal(app, &view.title, 110, height, items))
        }
        Popup::Terminal(run) => {
            let status = match run.terminal.status() {
                TerminalStatus::Running => {
                    format!("running · {} stops", key(app, Action::MenuInterrupt))
                }
                TerminalStatus::Exited(Some(0)) => "done".to_owned(),
                TerminalStatus::Exited(Some(code)) => format!("exit {code}"),
                TerminalStatus::Exited(None) => "stopped".to_owned(),
            };
            let height = app.size.get().1.saturating_mul(4) / 5;
            Some(modal(
                app,
                &format!("$ {} · {status}", run.command),
                TERMINAL_POPUP_WIDTH,
                height,
                terminal_popup_view(&run.terminal),
            ))
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
