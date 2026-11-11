//! The review visual system (spec section 6.3): one [`ReviewTheme`] with semantic roles, derived
//! from the host's plugin theme, and the icon set chosen by presentation policy.
//!
//! * Every role is derived from the presentation theme's `ComponentTheme` and diff styles, so
//!   the review matches the rest of Bcode under any theme. Without a host theme the review
//!   derives the same roles from a built-in dark seed palette.
//! * Roles cover file status, `+`/`−` counts and change bars, comment badges, the gutter, hunk
//!   headers, gap rows, sticky headers, the cursor line versus a selected range, pane borders
//!   (focused or not), and muted metadata. BMUX component styles (tree, panels, badges, progress,
//!   key hints, status bar, toasts, the publish button) are derived from the same theme.
//! * [`Icons`] hold every glyph the review paints outside file content, with an ASCII set for
//!   terminals or fonts without the Unicode symbols ([`IconStyle`], a user setting).

use bcode_code_review_models::ReviewFileStatus;
use bcode_plugin_sdk::tui::PluginTuiTheme;
use bmux_color::Rgb;
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui_components::badge::BadgeStyles;
use bmux_tui_components::diff_viewer::DiffViewerStyle;
use bmux_tui_components::key_hint_bar::KeyHintBarStyles;
use bmux_tui_components::panel_group::PanelGroupStyles;
use bmux_tui_components::progress_bar::ProgressBarStyles;
use bmux_tui_components::scrollbar::ScrollbarStyles;
use bmux_tui_components::split_button::SplitButtonStyles;
use bmux_tui_components::status_bar::StatusBarStyles;
use bmux_tui_components::theme::{ComponentTheme, ComponentThemeSeed};
use bmux_tui_components::toast_stack::ToastStackStyles;
use bmux_tui_components::tree_view::TreeViewStyles;
use serde::Deserialize;

/// Which glyph set the review paints.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IconStyle {
    /// Unicode symbols (the default).
    #[default]
    Unicode,
    /// Plain ASCII, for terminals or fonts without the Unicode symbols.
    Ascii,
}

/// Every glyph the review paints outside of file content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Icons {
    /// A viewed file.
    pub viewed: &'static str,
    /// A file not viewed yet.
    pub unviewed: &'static str,
    /// Comment threads.
    pub thread: &'static str,
    /// Draft comments.
    pub draft: &'static str,
    /// AI answers.
    pub ai: &'static str,
    /// Accent bar at the start of a file header.
    pub accent_bar: &'static str,
    /// Separator between metadata parts.
    pub separator: &'static str,
    /// Rename arrow between the old and the new path.
    pub rename: &'static str,
    /// Minus sign of removed-line counts.
    pub minus: &'static str,
    /// Rule drawn around gap labels.
    pub gap_rule: &'static str,
    /// Reveal lines above.
    pub reveal_up: &'static str,
    /// Reveal lines below.
    pub reveal_down: &'static str,
    /// Reveal a small gap entirely.
    pub reveal_all: &'static str,
    /// Elided or loading content.
    pub ellipsis: &'static str,
    /// Filled cell of a change bar.
    pub bar_filled: &'static str,
    /// Empty cell of a change bar.
    pub bar_empty: &'static str,
    /// Filled cell of the viewed progress bar.
    pub progress_filled: &'static str,
    /// Empty cell of the viewed progress bar.
    pub progress_empty: &'static str,
    /// Menu toggle of the publish button.
    pub menu: &'static str,
    /// Prompt of the file filter.
    pub filter: &'static str,
    /// Hide-the-panel action of the file panel.
    pub collapse: &'static str,
    /// A collapsed group (the menu toggle marks an expanded one).
    pub collapsed: &'static str,
}

impl Icons {
    /// Unicode glyphs.
    pub const UNICODE: Self = Self {
        viewed: "✓",
        unviewed: "○",
        thread: "💬",
        draft: "✎",
        ai: "✦",
        accent_bar: "▍",
        separator: " · ",
        rename: " → ",
        minus: "−",
        gap_rule: "┄",
        reveal_up: "↑",
        reveal_down: "↓",
        reveal_all: "↕",
        ellipsis: "⋯",
        bar_filled: "■",
        bar_empty: "·",
        progress_filled: "▰",
        progress_empty: "▱",
        menu: "▾",
        filter: "⌕",
        collapse: "«",
        collapsed: "▸",
    };

    /// ASCII glyphs.
    pub const ASCII: Self = Self {
        viewed: "v",
        unviewed: "o",
        thread: "#",
        draft: "~",
        ai: "AI",
        accent_bar: "|",
        separator: " | ",
        rename: " -> ",
        minus: "-",
        gap_rule: "-",
        reveal_up: "^",
        reveal_down: "v",
        reveal_all: "<>",
        ellipsis: "...",
        bar_filled: "#",
        bar_empty: ".",
        progress_filled: "#",
        progress_empty: "-",
        menu: "v",
        filter: "/",
        collapse: "<<",
        collapsed: ">",
    };

    /// The glyph set of `style`.
    #[must_use]
    pub const fn for_style(style: IconStyle) -> Self {
        match style {
            IconStyle::Unicode => Self::UNICODE,
            IconStyle::Ascii => Self::ASCII,
        }
    }
}

/// Semantic styles of the review screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewTheme {
    /// Component theme every BMUX component style below is derived from.
    pub component: ComponentTheme,
    /// Diff rows: text, gutter, markers, row tints, and emphasis.
    pub diff: DiffViewerStyle,
    /// Canvas behind the review.
    pub canvas: Style,
    /// Primary text.
    pub text: Style,
    /// Panel titles.
    pub title: Style,
    /// Accent for current items.
    pub accent: Style,
    /// Secondary metadata.
    pub muted: Style,
    /// Failed or unavailable content.
    pub error: Style,
    /// File notices (binary, truncated).
    pub notice: Style,
    /// Added-file status.
    pub status_added: Style,
    /// Modified-file status.
    pub status_modified: Style,
    /// Deleted-file status.
    pub status_deleted: Style,
    /// Renamed or copied file status.
    pub status_renamed: Style,
    /// Binary-file marker.
    pub status_binary: Style,
    /// Unknown status.
    pub status_unknown: Style,
    /// File path of a file not viewed yet.
    pub path: Style,
    /// File path of a viewed file.
    pub path_viewed: Style,
    /// Directory name in the file tree.
    pub directory: Style,
    /// Added line count.
    pub additions: Style,
    /// Removed line count.
    pub deletions: Style,
    /// Added share of a change bar.
    pub bar_added: Style,
    /// Removed share of a change bar.
    pub bar_removed: Style,
    /// Unused cells of a change bar.
    pub bar_empty: Style,
    /// Draft comment badge.
    pub badge_draft: Style,
    /// Open thread badge.
    pub badge_open: Style,
    /// Resolved thread badge.
    pub badge_resolved: Style,
    /// Outdated thread badge.
    pub badge_outdated: Style,
    /// AI answer badge.
    pub badge_ai: Style,
    /// Viewed marker.
    pub viewed: Style,
    /// Line numbers and gutter.
    pub gutter: Style,
    /// Hunk header text.
    pub hunk: Style,
    /// Hunk header row fill (it stays pinned below the file header).
    pub hunk_fill: Style,
    /// File header row fill (it stays pinned at the top).
    pub file_header: Style,
    /// Source and stack-layer header.
    pub source_header: Style,
    /// Gap row text.
    pub gap: Style,
    /// Interactive gap segments.
    pub gap_action: Style,
    /// Thread and agent rows.
    pub thread: Style,
    /// Cursor line (background only, layered over the row).
    pub cursor: Style,
    /// Selected line range (background only), distinct from the cursor line.
    pub selection: Style,
    /// A search match in diff content.
    pub search_match: Style,
    /// The current search match.
    pub search_current: Style,
    /// Pane border.
    pub border: Style,
    /// Border of the focused pane.
    pub border_focused: Style,
    /// Header bar fill and title.
    pub header_bar: Style,
    /// Footer bar fill.
    pub footer: Style,
    /// Filter input text.
    pub input: Style,
    /// Filter placeholder text.
    pub placeholder: Style,
    /// Scrollbar of the review list.
    pub scrollbar: ScrollbarStyles,
    /// Scrollbar mark of a thread.
    pub mark_thread: Style,
    /// Scrollbar mark of the current file.
    pub mark_file: Style,
    /// File tree rows while the file panel has focus.
    pub tree_focused: TreeViewStyles,
    /// File tree rows while another pane has focus.
    pub tree: TreeViewStyles,
    /// Panel dividers.
    pub panels: PanelGroupStyles,
    /// Source chips and badges.
    pub badges: BadgeStyles,
    /// Viewed progress bar.
    pub progress: ProgressBarStyles,
    /// Footer key hints.
    pub hints: KeyHintBarStyles,
    /// Footer status messages.
    pub status: StatusBarStyles,
    /// Toast notifications.
    pub toasts: ToastStackStyles,
    /// Publish button.
    pub publish: SplitButtonStyles,
    /// Glyphs.
    pub icons: Icons,
}

/// Seed of the built-in palette used without a host theme.
const DEFAULT_SEED: ComponentThemeSeed = ComponentThemeSeed {
    background: Rgb {
        r: 24,
        g: 26,
        b: 33,
    },
    foreground: Rgb {
        r: 214,
        g: 218,
        b: 228,
    },
    accent: Rgb {
        r: 122,
        g: 162,
        b: 247,
    },
    info: Rgb {
        r: 125,
        g: 207,
        b: 255,
    },
    success: Rgb {
        r: 158,
        g: 206,
        b: 106,
    },
    warning: Rgb {
        r: 224,
        g: 175,
        b: 104,
    },
    error: Rgb {
        r: 247,
        g: 118,
        b: 142,
    },
};

impl ReviewTheme {
    /// Theme for the host's `theme` (or the built-in palette without one) with `icons`.
    #[must_use]
    pub fn derive(theme: Option<&PluginTuiTheme>, icons: IconStyle) -> Self {
        let component = theme
            .and_then(PluginTuiTheme::component_theme)
            .unwrap_or_else(|| ComponentTheme::derive(DEFAULT_SEED));
        let diff = theme.map_or_else(
            || DiffViewerStyle {
                hunk: component.info,
                ..DiffViewerStyle::default()
            },
            |theme| DiffViewerStyle {
                text: theme.diff.text,
                muted: theme.diff.muted,
                title: theme.diff.title,
                label: theme.diff.label,
                added: theme.diff.added,
                removed: theme.diff.removed,
                hunk: theme.diff.hunk,
                added_row: theme.diff.added_row,
                removed_row: theme.diff.removed_row,
                added_emphasis: theme.diff.added_emphasis,
                removed_emphasis: theme.diff.removed_emphasis,
            },
        );
        Self::from_parts(component, diff, Icons::for_style(icons))
    }

    /// Theme from a component theme and diff styles.
    #[must_use]
    pub fn from_parts(component: ComponentTheme, diff: DiffViewerStyle, icons: Icons) -> Self {
        let raised = with_fallback_bg(component.surfaces.raised, Color::Rgb(30, 33, 41));
        let overlay = with_fallback_bg(component.surfaces.overlay, Color::Rgb(38, 42, 52));
        let cursor_bg = component
            .selected_inactive
            .bg
            .or(component.selected.bg)
            .unwrap_or(Color::Rgb(44, 48, 60));
        let selection_bg = component
            .selected
            .bg
            .filter(|selected| *selected != cursor_bg)
            .unwrap_or(Color::Rgb(54, 70, 110));
        let bold = |style: Style| style.add_modifier(Modifier::BOLD);
        // Counts and change bars use the diff's own added/removed colors, so the tree, the
        // headers, and the rows agree.
        let added = fg_only(diff.added).unwrap_or(component.success);
        let removed = fg_only(diff.removed).unwrap_or(component.error);
        let gutter = diff.muted;
        let components = components(&component, raised, overlay, cursor_bg);
        Self {
            component,
            diff: DiffViewerStyle {
                muted: gutter,
                ..diff
            },
            canvas: component.canvas,
            text: component.text,
            title: component.title,
            accent: component.accent,
            muted: component.muted,
            error: component.error,
            notice: component.warning,
            status_added: bold(added),
            status_modified: bold(component.warning),
            status_deleted: bold(removed),
            status_renamed: bold(component.info),
            status_binary: component.muted.add_modifier(Modifier::ITALIC),
            status_unknown: component.muted,
            path: bold(component.text),
            path_viewed: component.muted,
            directory: bold(component.accent),
            additions: added,
            deletions: removed,
            bar_added: added,
            bar_removed: removed,
            bar_empty: component.border,
            badge_draft: bold(component.warning),
            badge_open: bold(component.info),
            badge_resolved: bold(component.success),
            badge_outdated: component.muted.add_modifier(Modifier::ITALIC),
            badge_ai: bold(component.accent).add_modifier(Modifier::ITALIC),
            viewed: bold(component.success),
            gutter,
            hunk: diff.hunk,
            hunk_fill: overlay.patch(diff.hunk),
            file_header: raised.patch(component.text),
            source_header: bold(component.title),
            gap: component.muted,
            gap_action: bold(component.link),
            thread: component.info,
            cursor: Style::new().bg(cursor_bg),
            selection: Style::new().bg(selection_bg),
            search_match: Style::new()
                .fg(Color::Rgb(20, 20, 20))
                .bg(component.warning.fg.unwrap_or(Color::Yellow)),
            search_current: Style::new()
                .fg(Color::Rgb(20, 20, 20))
                .bg(component.accent.fg.unwrap_or(Color::Cyan))
                .add_modifier(Modifier::BOLD),
            border: component.border,
            border_focused: bold(component.border_focused),
            header_bar: raised.patch(component.text),
            footer: raised.patch(component.muted),
            input: component.text,
            placeholder: component.muted.add_modifier(Modifier::ITALIC),
            scrollbar: components.scrollbar,
            mark_thread: component.info,
            mark_file: component.accent,
            tree_focused: components.tree_focused,
            tree: components.tree,
            panels: components.panels,
            badges: components.badges,
            progress: components.progress,
            hints: components.hints,
            status: components.status,
            toasts: components.toasts,
            publish: components.publish,
            icons,
        }
    }

    /// Style of a file status.
    #[must_use]
    pub const fn status(&self, status: ReviewFileStatus) -> Style {
        match status {
            ReviewFileStatus::Added => self.status_added,
            ReviewFileStatus::Modified => self.status_modified,
            ReviewFileStatus::Deleted => self.status_deleted,
            ReviewFileStatus::Renamed | ReviewFileStatus::Copied => self.status_renamed,
            ReviewFileStatus::Unknown => self.status_unknown,
        }
    }
}

/// BMUX component styles of the review, from the same component theme as its roles.
struct Components {
    scrollbar: ScrollbarStyles,
    tree_focused: TreeViewStyles,
    tree: TreeViewStyles,
    panels: PanelGroupStyles,
    badges: BadgeStyles,
    progress: ProgressBarStyles,
    hints: KeyHintBarStyles,
    status: StatusBarStyles,
    toasts: ToastStackStyles,
    publish: SplitButtonStyles,
}

fn components(
    theme: &ComponentTheme,
    raised: Style,
    overlay: Style,
    cursor_bg: Color,
) -> Components {
    let component = *theme;
    let bold = |style: Style| style.add_modifier(Modifier::BOLD);
    let tree = component.tree_view_styles();
    Components {
        scrollbar: ScrollbarStyles::from(component),
        // The selected file stands out while the panel has focus and stays marked (as the
        // cursor line is) otherwise.
        tree_focused: TreeViewStyles {
            selected: bold(component.selected),
            ..tree
        },
        tree: TreeViewStyles {
            selected: Style::new().bg(cursor_bg),
            ..tree
        },
        panels: PanelGroupStyles {
            divider: Style::new(),
            hovered_divider: component.border_focused,
            active_divider: bold(component.border_focused),
        },
        badges: BadgeStyles::from(component),
        progress: ProgressBarStyles {
            filled: component.success,
            empty: component.border,
            label: bold(component.text),
            complete: bold(component.success),
            indeterminate: component.info,
            background: Style::new(),
        },
        hints: KeyHintBarStyles {
            key: bold(component.accent),
            label: component.muted,
            separator: component.border,
            disabled: component.disabled,
            background: raised,
        },
        status: StatusBarStyles {
            background: raised,
            ..component.status_bar_styles()
        },
        toasts: ToastStackStyles {
            surface: overlay,
            ..component.toast_stack_styles()
        },
        publish: SplitButtonStyles {
            button: component.into(),
            menu: component.into(),
            border: component.border,
        },
    }
}

/// `style` with `fallback` as its background when it has none.
const fn with_fallback_bg(style: Style, fallback: Color) -> Style {
    if style.bg.is_some() {
        style
    } else {
        style.bg(fallback)
    }
}

/// The foreground of `style` as a style of its own, when it has one.
fn fg_only(style: Style) -> Option<Style> {
    style.fg.map(|color| Style::new().fg(color))
}

/// One-letter status label.
#[must_use]
pub const fn status_letter(status: ReviewFileStatus) -> &'static str {
    match status {
        ReviewFileStatus::Added => "A",
        ReviewFileStatus::Modified => "M",
        ReviewFileStatus::Deleted => "D",
        ReviewFileStatus::Renamed => "R",
        ReviewFileStatus::Copied => "C",
        ReviewFileStatus::Unknown => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_are_distinct_where_the_screen_relies_on_them() {
        for theme in [
            ReviewTheme::derive(None, IconStyle::Unicode),
            ReviewTheme::from_parts(
                ComponentTheme::opaque_dark(),
                DiffViewerStyle::default(),
                Icons::UNICODE,
            ),
        ] {
            let statuses = [
                ReviewFileStatus::Added,
                ReviewFileStatus::Modified,
                ReviewFileStatus::Deleted,
                ReviewFileStatus::Renamed,
            ]
            .map(|status| theme.status(status));
            for (index, style) in statuses.iter().enumerate() {
                assert!(statuses[index + 1..].iter().all(|other| other != style));
            }
            assert_ne!(theme.additions, theme.deletions);
            assert_ne!(theme.cursor, theme.selection);
            assert_ne!(theme.border, theme.border_focused);
            assert_ne!(theme.path, theme.path_viewed);
            let badges = [
                theme.badge_draft,
                theme.badge_open,
                theme.badge_resolved,
                theme.badge_outdated,
                theme.badge_ai,
            ];
            for (index, style) in badges.iter().enumerate() {
                assert!(badges[index + 1..].iter().all(|other| other != style));
            }
            assert_ne!(theme.tree.selected, theme.tree_focused.selected);
            // Line numbers are the diff layout's gutter style.
            assert_eq!(theme.gutter, theme.diff.muted);
        }
    }

    #[test]
    fn ascii_icons_are_ascii() {
        let icons = Icons::ASCII;
        for glyph in [
            icons.viewed,
            icons.unviewed,
            icons.thread,
            icons.draft,
            icons.ai,
            icons.accent_bar,
            icons.separator,
            icons.rename,
            icons.minus,
            icons.gap_rule,
            icons.reveal_up,
            icons.reveal_down,
            icons.reveal_all,
            icons.ellipsis,
            icons.bar_filled,
            icons.bar_empty,
            icons.progress_filled,
            icons.progress_empty,
            icons.menu,
            icons.filter,
            icons.collapse,
        ] {
            assert!(glyph.is_ascii(), "{glyph:?}");
        }
    }
}
