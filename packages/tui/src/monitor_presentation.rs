//! Bounded, terminal-owned child monitor presentation and acceptance-test interfaces.

use std::fmt::Write as _;

use bcode_session_models::{SessionActivityEvidence, SessionActivityState, SessionId};

/// One selectable relationship row; activity facts may coexist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorRow {
    /// Stable canonical identity used for selection and opening.
    pub session_id: SessionId,
    /// User-facing session title.
    pub title: String,
    /// Verified parent identity from the loaded summary.
    pub parent_id: Option<SessionId>,
    /// Missing loaded ancestry or an inconsistent relationship.
    pub detached: bool,
    /// Semantic depth, independent of terminal width.
    pub depth: usize,
    /// Ancestors whose connector continues into later siblings.
    pub ancestor_continuations: Vec<bool>,
    /// Whether this is the final loaded sibling.
    pub last_sibling: bool,
    /// Branch expansion state; an undiscovered branch can still be expanded.
    pub expanded: bool,
    /// Concurrent normalized observations, or unknown when unobserved.
    pub states: Vec<SessionActivityState>,
    /// Own-session evidence, never subtree totals.
    pub evidence: Option<SessionActivityEvidence>,
    /// Observation age label; unobserved is explicit.
    pub age: String,
}

impl MonitorRow {
    /// ASCII-safe bounded tree prefix; BMUX measures this same prefix for chevron hits.
    #[must_use]
    pub fn connector_prefix(&self) -> String {
        let mut prefix = String::new();
        for continues in &self.ancestor_continuations {
            prefix.push_str(if *continues { "| " } else { "  " });
        }
        if self.depth > 4 {
            let _ = write!(prefix, "[{}] ", self.depth);
        }
        prefix.push_str(if self.last_sibling { "`-" } else { "|-" });
        prefix
    }
}

/// Typed bounded panel contents. No fetching or execution authority is implied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorPresentation {
    /// Attached parent title.
    pub parent_title: String,
    /// Coverage and attention summary from the shared observation policy.
    pub overview: String,
    /// Root observation freshness.
    pub freshness: String,
    /// Loaded preview in stable relationship order.
    pub rows: Vec<MonitorRow>,
    /// Index in this preview, not the complete relationship collection.
    pub selected: Option<usize>,
    /// Selected identity and own-session details even when the preview is shortened.
    pub details: Option<MonitorRow>,
    /// Root observation coverage; absent observations remain unknown.
    pub coverage: Option<bcode_session_models::SessionActivityCoverage>,
    /// Latest monitoring failure, without replacing accepted observation evidence.
    pub error: Option<String>,
    /// A real pagination action is currently available.
    pub load_more: bool,
    /// Contextual key hints resolved through the active keymap.
    pub footer: String,
}

/// Theme-adapted semantic status palette for terminal acceptance tests.
#[derive(Debug, Clone, Copy)]
pub struct MonitorStatusStyles {
    /// Running activity (cyan).
    pub running: bmux_tui::style::Style,
    /// Queued activity (blue).
    pub queued: bmux_tui::style::Style,
    /// Pending decisions/input (amber).
    pub attention: bmux_tui::style::Style,
    /// Failed work (red).
    pub failure: bmux_tui::style::Style,
    /// Idle, not completed.
    pub idle: bmux_tui::style::Style,
    /// Unknown observations (dim).
    pub unknown: bmux_tui::style::Style,
}

impl MonitorStatusStyles {
    /// Resolve the style of a normalized observation.
    #[must_use]
    pub const fn for_state(self, state: SessionActivityState) -> bmux_tui::style::Style {
        match state {
            SessionActivityState::Running => self.running,
            SessionActivityState::Queued => self.queued,
            SessionActivityState::PermissionWait | SessionActivityState::InputWait => {
                self.attention
            }
            SessionActivityState::Failed => self.failure,
            SessionActivityState::Interrupted => self.attention,
            SessionActivityState::Idle => self.idle,
            SessionActivityState::Unknown => self.unknown,
        }
    }
}

/// Resolve theme roles while preserving the monitor's explicit activity colors.
#[must_use]
pub const fn status_styles(
    text: bmux_tui::style::Style,
    info: bmux_tui::style::Style,
    warning: bmux_tui::style::Style,
    error: bmux_tui::style::Style,
    muted: bmux_tui::style::Style,
) -> MonitorStatusStyles {
    use bmux_tui::style::{Color, Modifier};
    MonitorStatusStyles {
        running: info.fg(Color::Cyan),
        queued: info.fg(Color::Blue),
        attention: warning,
        failure: error,
        idle: text,
        unknown: muted.add_modifier(Modifier::DIM),
    }
}

/// Text/icon status policy; idle never claims completion.
#[must_use]
pub const fn status_label(state: SessionActivityState) -> (&'static str, &'static str) {
    match state {
        SessionActivityState::Running => (">", "running"),
        SessionActivityState::Queued => ("~", "queued"),
        SessionActivityState::PermissionWait => ("!", "permission"),
        SessionActivityState::InputWait => ("?", "input"),
        SessionActivityState::Failed => ("x", "failed"),
        SessionActivityState::Interrupted => ("!", "interrupted"),
        SessionActivityState::Idle => ("-", "idle"),
        SessionActivityState::Unknown => ("?", "unknown"),
    }
}

/// Resolve a distinct raised surface at the terminal boundary without changing configuration.
/// Terminal-default backgrounds use the theme foreground to select a contrasting fallback.
#[must_use]
pub fn raised_surface(
    raised: bmux_tui::style::Style,
    canvas: bmux_tui::style::Style,
    text: bmux_tui::style::Style,
) -> bmux_tui::style::Style {
    use bmux_tui::style::Color;
    let distinct = match (raised.bg, canvas.bg) {
        (Some(Color::Default) | None, Some(Color::Default) | None) => false,
        (Some(a), Some(b)) => a != b,
        (Some(_), None) | (None, Some(_)) => true,
    };
    if distinct {
        return raised;
    }
    let light_canvas = match canvas.bg {
        Some(Color::White | Color::BrightWhite) => true,
        Some(Color::Rgb(r, g, b)) => u16::from(r) + u16::from(g) + u16::from(b) > 384,
        _ => matches!(text.fg, Some(Color::Black | Color::BrightBlack)),
    };
    raised.bg(if light_canvas {
        Color::Rgb(224, 228, 234)
    } else {
        Color::Rgb(30, 35, 43)
    })
}

/// Resolved terminal palette; acceptance tests may supply their own theme.
#[derive(Debug, Clone, Copy)]
pub struct MonitorStyles {
    /// Ordinary content.
    pub text: bmux_tui::style::Style,
    /// Secondary content and unknown observations.
    pub muted: bmux_tui::style::Style,
    /// Unfocused panel border.
    pub border: bmux_tui::style::Style,
    /// Focused panel accent.
    pub focused: bmux_tui::style::Style,
    /// Raised background, separate from transcript canvas.
    pub raised: bmux_tui::style::Style,
    /// Selected-row background.
    pub selection: bmux_tui::style::Style,
    /// Informational content.
    pub info: bmux_tui::style::Style,
    /// Attention-needed content.
    pub warning: bmux_tui::style::Style,
    /// Failed content.
    pub error: bmux_tui::style::Style,
}

/// A monitor action, independent of display strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorAction {
    /// Select a row without opening it.
    Select(SessionId),
    /// Select a row and toggle its branch.
    ToggleExpansion(SessionId),
    /// Fetch the selected branch/root continuation.
    LoadMore,
}

/// An actually painted visible action rectangle from BMUX's resolved layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorHit {
    /// Typed action.
    pub action: MonitorAction,
    /// Visible terminal-space rectangle.
    pub rect: bmux_tui::geometry::Rect,
}

impl MonitorHit {
    /// Stable semantic key used by committed terminal hit routing.
    #[must_use]
    pub fn key(self) -> String {
        match self.action {
            MonitorAction::Select(id) => format!("child-monitor:{id}"),
            MonitorAction::ToggleExpansion(id) => format!("child-monitor-expand:{id}"),
            MonitorAction::LoadMore => "child-monitor-more".to_owned(),
        }
    }
}

/// Supplemental observation chrome, kept separate for row-constructor compatibility.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MonitorContext {
    /// Attached session marked as the current parent/root.
    pub current_session: Option<SessionId>,
    /// Bounded known-descendant badges; never complete subtree totals.
    pub descendant_badges: std::collections::BTreeMap<SessionId, MonitorDescendantBadge>,
}

/// Activity in a bounded observed descendant window, including collapsed branches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MonitorDescendantBadge {
    /// Unique observed descendants with active evidence.
    pub active: usize,
    /// Unique observed descendants needing attention.
    pub attention: usize,
}

/// Paint a bounded panel and return its real visible row/chevron/action geometry.
/// Empty/tiny rectangles degrade through BMUX without invisible hit targets.
#[must_use]
pub fn paint(
    presentation: &MonitorPresentation,
    area: bmux_tui::geometry::Rect,
    focused: bool,
    theme: MonitorStyles,
    frame: &mut bmux_tui::paint::PaintCx<'_, '_>,
) -> Vec<MonitorHit> {
    paint_with_context(
        presentation,
        &MonitorContext::default(),
        area,
        focused,
        theme,
        frame,
    )
}

/// Paint with attached-root identity and bounded known-descendant activity badges.
/// Uses the same BMUX-resolved rectangles for painting and action reporting.
#[must_use]
pub fn paint_with_context(
    presentation: &MonitorPresentation,
    context: &MonitorContext,
    area: bmux_tui::geometry::Rect,
    focused: bool,
    theme: MonitorStyles,
    frame: &mut bmux_tui::paint::PaintCx<'_, '_>,
) -> Vec<MonitorHit> {
    use bmux_tui::composition::{Surface, TextBlock};
    use bmux_tui::geometry::Rect;
    use bmux_tui::paint::LocalRect;
    use bmux_tui::prelude::{Border, BorderSet, Component, Constraints, LayoutCx, Line};
    use bmux_tui_components::selectable_list::{
        SelectableList, SelectableListState, SelectableListStyles,
    };
    if area.is_empty() {
        return Vec::new();
    }
    let border = if focused { theme.focused } else { theme.border };
    let shell = Surface::new(TextBlock::new(""))
        .border(Border::new(BorderSet::ASCII, border.patch(theme.raised)))
        .background(theme.raised)
        .content_style(theme.text);
    let shell_layout = shell.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    frame.with_child(
        i32::from(area.x),
        i64::from(area.y),
        LocalRect::new(0, 0, area.width, area.height),
        |cx| shell.paint(&shell_layout, cx),
    );
    let Some(child) = shell_layout.children.first() else {
        return Vec::new();
    };
    let inner = Rect::new(
        area.x
            .saturating_add(u16::try_from(child.x).unwrap_or(u16::MAX)),
        area.y
            .saturating_add(u16::try_from(child.y).unwrap_or(u16::MAX)),
        u16::try_from(child.node.size.width).unwrap_or(u16::MAX),
        u16::try_from(child.node.size.height).unwrap_or(u16::MAX),
    );
    if inner.is_empty() {
        return Vec::new();
    }
    let (tree_area, detail_area) = split_areas(inner, presentation.details.is_some());
    let wide = detail_area.is_some();
    let (items, selected) = prepare_items(presentation, context, tree_area, wide, theme);
    if let Some(detail_area) = detail_area
        && let Some(row) = &presentation.details
    {
        paint_details(row, context, detail_area, theme, frame);
    }
    let mut state = SelectableListState::new(selected);
    if !focused {
        state.set_focused(None);
    }
    let selection = bmux_tui::style::Style {
        fg: None,
        ..theme.selection
    };
    let policy = bmux_tui_components::selectable_list::SelectableListPolicy {
        highlight: bmux_tui_components::selectable_list::SelectableListHighlightPolicy::new(
            "", false,
        ),
        ..Default::default()
    };
    let list = SelectableList::new(&items)
        .policy(policy)
        .styles(SelectableListStyles {
            normal: theme.text,
            focused: selection,
            selected: selection,
            hovered: theme.focused,
            pressed: theme.selection,
            disabled: theme.muted,
            background: theme.raised,
            scrollbar: bmux_tui_components::scrollbar::ScrollbarStyles::default(),
        });
    list.paint(tree_area, &state, theme.text.patch(theme.raised), frame);
    let mut hits = Vec::new();
    for region in list.visible_semantic_regions(tree_area, &state) {
        if let Some(id) = region.key.strip_prefix("child-monitor:") {
            if let Some(row) = presentation
                .rows
                .iter()
                .find(|row| row.session_id.to_string() == id)
            {
                hits.push(MonitorHit {
                    action: MonitorAction::Select(row.session_id),
                    rect: region.rect,
                });
                let offset = u16::try_from(Line::from(row_prefix(row, tree_area.width)).width())
                    .unwrap_or(u16::MAX);
                if offset < region.rect.width {
                    hits.push(MonitorHit {
                        action: MonitorAction::ToggleExpansion(row.session_id),
                        rect: Rect::new(region.rect.x.saturating_add(offset), region.rect.y, 1, 1),
                    });
                }
            }
        } else if region.key == "child-monitor-more" {
            hits.push(MonitorHit {
                action: MonitorAction::LoadMore,
                rect: region.rect,
            });
        }
    }
    hits
}

fn prepare_items(
    presentation: &MonitorPresentation,
    context: &MonitorContext,
    area: bmux_tui::geometry::Rect,
    wide: bool,
    theme: MonitorStyles,
) -> (
    Vec<bmux_tui_components::selectable_list::SelectableListItem>,
    Option<usize>,
) {
    use bmux_tui::prelude::{Line, Span};
    use bmux_tui_components::selectable_list::SelectableListItem;
    let chrome = |text: String, style| {
        SelectableListItem::rich(
            "monitor-chrome",
            Line::from_spans(vec![Span::styled(text, style)]),
        )
    };
    let height = area.height;
    let mut items = vec![chrome(
        format!(
            "Session monitor · {} · {}",
            presentation.freshness, presentation.parent_title
        ),
        theme.focused,
    )];
    if height == 1 {
        return (items, None);
    }
    let show_root = height >= 6;
    let show_overview = height >= 5;
    items.extend(context_items(
        presentation,
        context,
        show_root,
        show_overview,
        theme,
    ));
    let show_more = presentation.load_more && height >= 4;
    let detail_rows = usize::from(!wide && height >= 10 && presentation.details.is_some()) * 4;
    let budget = usize::from(height).saturating_sub(
        2 + usize::from(show_root)
            + usize::from(show_overview)
            + detail_rows
            + usize::from(show_more),
    );
    let badge_rows = presentation
        .rows
        .iter()
        .any(|row| !row.expanded && context.descendant_badges.contains_key(&row.session_id));
    let budget = if badge_rows { budget / 2 } else { budget };
    let start = presentation
        .selected
        .unwrap_or(0)
        .saturating_sub(budget / 2)
        .min(presentation.rows.len().saturating_sub(budget));
    let mut selected = None;
    for (index, row) in presentation
        .rows
        .iter()
        .enumerate()
        .skip(start)
        .take(budget)
    {
        if Some(index) == presentation.selected {
            selected = Some(items.len());
        }
        let line = row_line(row, area.width, theme);
        let mut lines = vec![line];
        if !row.expanded
            && let Some(badge) = context.descendant_badges.get(&row.session_id)
        {
            lines.push(Line::from_spans(vec![Span::styled(
                format!(
                    "  {} active / {} attention · known descendants, partial",
                    badge.active, badge.attention
                ),
                if badge.attention > 0 {
                    theme.warning
                } else {
                    theme.muted
                },
            )]));
        }
        items.push(SelectableListItem::multiline(
            format!("child-monitor:{}", row.session_id),
            lines,
        ));
    }
    if budget > 0 && presentation.rows.is_empty() {
        items.push(chrome(
            "No loaded children · discovery may be partial".to_owned(),
            theme.muted,
        ));
    }
    if detail_rows > 0
        && let Some(row) = &presentation.details
    {
        items.extend(detail_items(row, context, theme));
    }
    if show_more {
        items.push(SelectableListItem::rich(
            "child-monitor-more",
            Line::from_spans(vec![Span::styled("[ Load more children ]", theme.info)]),
        ));
    }
    items.push(chrome(presentation.footer.clone(), theme.muted));
    (items, selected)
}

fn detail_items(
    row: &MonitorRow,
    context: &MonitorContext,
    theme: MonitorStyles,
) -> Vec<bmux_tui_components::selectable_list::SelectableListItem> {
    use bmux_tui::prelude::{Line, Span};
    use bmux_tui_components::selectable_list::SelectableListItem;
    let chrome = |text: String, style| {
        SelectableListItem::rich(
            "monitor-detail",
            Line::from_spans(vec![Span::styled(text, style)]),
        )
    };
    let mut items = Vec::new();
    let operation = row
        .evidence
        .as_ref()
        .and_then(|e| e.current_operation.as_deref())
        .unwrap_or("operation unknown");
    items.push(chrome(
        format!("Selected · {} · {}", row.session_id, row.age),
        theme.muted,
    ));
    let parent = row.parent_id.map_or_else(
        || "unknown".to_owned(),
        |id| {
            if Some(id) == context.current_session {
                format!("{id} (You are here)")
            } else {
                id.to_string()
            }
        },
    );
    items.push(chrome(
        format!(
            "Parent · {parent}{}",
            if row.detached {
                " · not loaded/unverified ancestry"
            } else {
                ""
            }
        ),
        theme.muted,
    ));
    items.push(chrome(format!("Operation · {operation}"), theme.text));
    let counts = row.evidence.as_ref().map_or_else(
        || "Own counts unknown".to_owned(),
        |e| {
            format!(
                "Own counts{} · {} running · {} queued · {} permission · {} input",
                if e.counts_complete { "" } else { " (partial)" },
                e.running,
                e.queued,
                e.permission_waits,
                e.input_waits
            )
        },
    );
    items.push(chrome(counts, theme.muted));
    items
}

// Responsive presentation policy, not a text measurement/clipping engine. On narrow
// rows, remove ancestor chrome entirely before sacrificing status/title content.
fn row_prefix(row: &MonitorRow, width: u16) -> String {
    if width < 48 {
        String::new()
    } else {
        row.connector_prefix()
    }
}

fn paint_details(
    row: &MonitorRow,
    context: &MonitorContext,
    area: bmux_tui::geometry::Rect,
    theme: MonitorStyles,
    frame: &mut bmux_tui::paint::PaintCx<'_, '_>,
) {
    use bmux_tui::component::{Component, Constraints, LayoutCx};
    use bmux_tui::composition::{Surface, TextBlock};
    use bmux_tui::paint::LocalRect;
    use bmux_tui::prelude::{Border, Line, Span, Text, TextWrap};
    let mut lines = vec![Line::from_spans(vec![Span::styled(
        format!("Selected · {}", row.title),
        theme.focused,
    )])];
    let mut states = Vec::new();
    for state in &row.states {
        let (icon, label) = status_label(*state);
        states.push(Span::styled(
            format!("{icon} {label} "),
            status_styles(
                theme.text,
                theme.info,
                theme.warning,
                theme.error,
                theme.muted,
            )
            .for_state(*state),
        ));
    }
    lines.push(Line::from_spans(states));
    lines.extend(
        detail_items(row, context, theme)
            .into_iter()
            .flat_map(|item| item.lines),
    );
    if let Some(badge) = context.descendant_badges.get(&row.session_id) {
        lines.push(Line::from_spans(vec![Span::styled(
            format!(
                "Descendants · {} active / {} attention (known, partial)",
                badge.active, badge.attention
            ),
            theme.muted,
        )]));
    }
    let detail = Surface::new(TextBlock::new(Text::from_lines(lines)).wrap(TextWrap::Word))
        .border(Border::ascii().style(theme.border))
        .background(theme.raised)
        .content_style(theme.text);
    let layout = detail.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    frame.with_child(
        i32::from(area.x),
        i64::from(area.y),
        LocalRect::new(0, 0, area.width, area.height),
        |cx| detail.paint(&layout, cx),
    );
}

fn row_line(row: &MonitorRow, width: u16, theme: MonitorStyles) -> bmux_tui::text::Line {
    use bmux_tui::prelude::{Line, Span};
    let prefix = row_prefix(row, width);
    let mut spans = vec![
        Span::styled(prefix.clone(), theme.muted),
        Span::styled(if row.expanded { "v " } else { "> " }, theme.focused),
    ];
    if width < 48 {
        // Let BMUX bound arbitrary Unicode. Reserve a visible share for both title
        // and primary status; the details retain all concurrent observation facts.
        let available = usize::from(width).saturating_sub(Line::from(prefix).width() + 2);
        let title_width = available / 2;
        let title = bmux_tui::text_width::truncate_to_display_width(&row.title, title_width);
        spans.push(Span::styled(title, theme.text));
        let state = row
            .states
            .first()
            .copied()
            .unwrap_or(SessionActivityState::Unknown);
        let (icon, label) = status_label(state);
        let status = bmux_tui::text_width::truncate_to_display_width(
            &format!(" {icon} {label}"),
            available.saturating_sub(title_width),
        );
        spans.push(Span::styled(
            status,
            status_styles(
                theme.text,
                theme.info,
                theme.warning,
                theme.error,
                theme.muted,
            )
            .for_state(state),
        ));
    } else {
        for state in &row.states {
            let (icon, label) = status_label(*state);
            spans.push(Span::styled(
                format!("{icon} {label} "),
                status_styles(
                    theme.text,
                    theme.info,
                    theme.warning,
                    theme.error,
                    theme.muted,
                )
                .for_state(*state),
            ));
        }
        spans.push(Span::styled(format!("· {}", row.title), theme.text));
    }
    Line::from_spans(spans)
}

fn split_areas(
    inner: bmux_tui::geometry::Rect,
    has_details: bool,
) -> (bmux_tui::geometry::Rect, Option<bmux_tui::geometry::Rect>) {
    use bmux_tui::component::{Component, Constraints, LayoutCx};
    use bmux_tui::composition::{Row, TextBlock, VerticalAlignment};
    use bmux_tui::geometry::Rect;
    let wide = inner.width >= 100 && inner.height >= 8 && has_details;
    let (tree_area, detail_area) = if wide {
        let columns = Row::new()
            .alignment(VerticalAlignment::Stretch)
            .gap(1)
            .flex_child(1, TextBlock::new(""))
            .flex_child(1, TextBlock::new(""));
        let columns_layout = columns.layout(Constraints::tight(inner.size()), &mut LayoutCx::new());
        let rect = |index: usize| {
            let child = &columns_layout.children[index];
            Rect::new(
                inner
                    .x
                    .saturating_add(u16::try_from(child.x).unwrap_or(u16::MAX)),
                inner
                    .y
                    .saturating_add(u16::try_from(child.y).unwrap_or(u16::MAX)),
                u16::try_from(child.node.size.width).unwrap_or(u16::MAX),
                u16::try_from(child.node.size.height).unwrap_or(u16::MAX),
            )
        };
        (rect(0), Some(rect(1)))
    } else {
        (inner, None)
    };
    (tree_area, detail_area)
}

fn context_items(
    presentation: &MonitorPresentation,
    context: &MonitorContext,
    show_root: bool,
    show_overview: bool,
    theme: MonitorStyles,
) -> Vec<bmux_tui_components::selectable_list::SelectableListItem> {
    use bmux_tui::prelude::{Line, Span};
    use bmux_tui_components::selectable_list::SelectableListItem;
    let chrome = |text: String, style| {
        SelectableListItem::rich(
            "monitor-context",
            Line::from_spans(vec![Span::styled(text, style)]),
        )
    };
    let mut items = Vec::new();
    if show_root {
        let parent = context.current_session.map_or_else(
            || presentation.parent_title.clone(),
            |id| format!("{} · {id}", presentation.parent_title),
        );
        items.push(chrome(format!("You are here · {parent}"), theme.focused));
    }
    if show_overview {
        let overview = presentation.error.as_ref().map_or_else(
            || presentation.overview.clone(),
            |error| format!("stale/degraded · {error} · {}", presentation.overview),
        );
        items.push(chrome(overview, theme.muted));
    }
    items
}
