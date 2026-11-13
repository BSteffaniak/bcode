//! The review visual system: one [`ReviewTheme`] with semantic roles, derived from the host's
//! component styles, and the icon set chosen by presentation policy.
//!
//! * Every role is derived from a few semantic [`Tiers`] (surfaces, text levels, accents),
//!   which come from the host theme's component styles, so the review matches the rest of
//!   Bcode under any theme. Without a host theme the review derives the same tiers from a
//!   built-in dark seed palette.
//! * Warning color means "needs you" (a pending decision, unsaved comments, a failure);
//!   status roles never use it: a modified file takes the accent, a renamed one the second
//!   accent.
//! * Panels are rounded unless the theme chooses border glyphs; a focused panel's border and
//!   title take the accent, an unfocused title is muted.
//! * Roles cover file status, `+`/`−` counts and change bars, comment badges, the gutter, hunk
//!   headers, gap rows, sticky headers, the cursor line versus a selected range, pane borders
//!   (focused or not), and muted metadata. BMUX component styles (tree, panels, badges, progress,
//!   key hints, status bar, toasts, the publish button) are derived from the same theme.
//! * [`Icons`] hold every glyph the review paints outside file content, with an ASCII set for
//!   terminals or fonts without the Unicode symbols ([`IconStyle`], a user setting).

mod load;
mod tiers;

pub use load::{DEFAULT_THEME, HostTheme};
pub use tiers::Tiers;

use bcode_code_review_models::ReviewFileStatus;
use bcode_tui_theme::components::ComponentStyles;
use bmux_color::Rgb;
use bmux_tui::capabilities::TerminalColorDepth;
use bmux_tui::chrome::BorderSet;
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui::text::{Line, Span};
use bmux_tui_components::badge::BadgeStyles;
use bmux_tui_components::button::ButtonStyles;
use bmux_tui_components::diff_viewer::DiffViewerStyle;
use bmux_tui_components::key_hint_bar::KeyHintBarStyles;
use bmux_tui_components::menu::MenuPolicy;
use bmux_tui_components::panel_group::PanelGroupStyles;
use bmux_tui_components::progress_bar::ProgressBarStyles;
use bmux_tui_components::scrollbar::ScrollbarStyles;
use bmux_tui_components::selectable_list::SelectableListHighlightPolicy;
use bmux_tui_components::split_button::SplitButtonStyles;
use bmux_tui_components::status_bar::StatusBarStyles;
use bmux_tui_components::theme::{
    ComponentChrome, ComponentSurfaces, ComponentTheme, ComponentThemeSeed,
};
use bmux_tui_components::toast_stack::ToastStackStyles;
use bmux_tui_components::tree_view::{TreeViewSelectionBar, TreeViewStyles};
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
    /// Rule of the banner that starts each stack layer.
    pub banner_rule: &'static str,
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
    /// Bar marking the selected row of a list or tree.
    pub selection_bar: &'static str,
    /// Prompt of the file filter.
    pub filter: &'static str,
    /// Hide-the-panel action of the file panel.
    pub collapse: &'static str,
    /// A collapsed group (the menu toggle marks an expanded one).
    pub collapsed: &'static str,
    /// A running tool call.
    pub running: &'static str,
    /// A failed tool call.
    pub failed: &'static str,
    /// Rule before a line of tool output.
    pub output: &'static str,
    /// An error.
    pub alert: &'static str,
    /// An approval.
    pub approved: &'static str,
    /// A request for changes.
    pub changes: &'static str,
    /// A review still needed.
    pub awaiting: &'static str,
    /// Checks.
    pub checks: &'static str,
    /// Opens a web page.
    pub external: &'static str,
    /// Something new since last looked at.
    pub new: &'static str,
    /// Cells a page's bottom edge is torn with (the page hangs into the row).
    pub tear_bottom: [&'static str; 3],
    /// Cells a page's top edge is torn with (the page rises into the row).
    pub tear_top: [&'static str; 3],
    /// A plain rule between files.
    pub file_rule: &'static str,
    /// Name of the BMUX spinner preset of busy items (`braille` or `ascii`).
    pub spinner: &'static str,
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
        banner_rule: "━",
        reveal_up: "↑",
        reveal_down: "↓",
        reveal_all: "↕",
        ellipsis: "⋯",
        bar_filled: "■",
        bar_empty: "·",
        progress_filled: "▰",
        progress_empty: "▱",
        menu: "▾",
        selection_bar: "▌",
        filter: "⌕",
        collapse: "«",
        collapsed: "▸",
        running: "◌",
        failed: "✗",
        output: "│",
        alert: "⚠",
        approved: "✓",
        changes: "✕",
        awaiting: "◌",
        checks: "●",
        external: "↗",
        new: "•",
        tear_bottom: ["▀", "▔", " "],
        tear_top: ["▄", "▁", " "],
        file_rule: "┄",
        spinner: "braille",
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
        banner_rule: "=",
        reveal_up: "^",
        reveal_down: "v",
        reveal_all: "<>",
        ellipsis: "...",
        bar_filled: "#",
        bar_empty: ".",
        progress_filled: "#",
        progress_empty: "-",
        menu: "v",
        selection_bar: ">",
        filter: "/",
        collapse: "<<",
        collapsed: ">",
        running: "*",
        failed: "x",
        output: "|",
        alert: "!",
        approved: "+",
        changes: "x",
        awaiting: "o",
        checks: "*",
        external: "->",
        new: "*",
        tear_bottom: ["~", "^", " "],
        tear_top: ["~", "_", " "],
        file_rule: "-",
        spinner: "ascii",
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
    /// Semantic tiers every role below is derived from.
    pub tiers: Tiers,
    /// Component theme every BMUX component style below is derived from.
    pub component: ComponentTheme,
    /// Component theme of content inside overlays (pickers, the palette, menus), whose canvas
    /// is the overlay.
    pub overlay_component: ComponentTheme,
    /// Diff rows: text, gutter, markers, row tints, and emphasis.
    pub diff: DiffViewerStyle,
    /// Canvas behind the review.
    pub canvas: Style,
    /// Primary text.
    pub text: Style,
    /// Headings.
    pub title: Style,
    /// Title of an unfocused pane.
    pub pane_title: Style,
    /// Title of the focused pane.
    pub pane_title_focused: Style,
    /// A neutral pill (a chip of metadata).
    pub pill: Style,
    /// A pill that stands out: the current mode or a count that matters.
    pub pill_accent: Style,
    /// A pill for something that needs the reviewer.
    pub pill_warning: Style,
    /// Bar marking the selected row of a list or tree.
    pub selection_bar: Style,
    /// Accent for current items.
    pub accent: Style,
    /// Secondary metadata.
    pub muted: Style,
    /// Failed or unavailable content.
    pub error: Style,
    /// Something that needs the reviewer (unsaved work, new commits, a stale review).
    pub notice: Style,
    /// File notices (binary, truncated): information, not a request.
    pub file_notice: Style,
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
    /// File headers, the breaks between files, and the pinned headers' edge.
    pub files: FileStyles,
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

/// Share of the accent in the background of a selected line range.
const RANGE_ACCENT_SHARE: f64 = 0.30;

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
    /// Theme for the host's component `styles` (or the built-in palette without them) with
    /// `icons`.
    #[must_use]
    pub fn derive(styles: Option<&ComponentStyles>, icons: IconStyle) -> Self {
        let Some(styles) = styles else {
            let component = ComponentTheme::derive(DEFAULT_SEED);
            return Self::from_parts(
                component,
                DiffViewerStyle {
                    hunk: component.info,
                    ..DiffViewerStyle::default()
                },
                Icons::for_style(icons),
            );
        };
        let component = styles.component;
        // Second hues, most preferred first: syntax keyword and type colors read as distinct
        // categories in most palettes; info and link are the fallbacks every theme has.
        let alternates: Vec<Color> = [
            Some(styles.syntax.keyword.to_tui()),
            Some(styles.syntax.type_name.to_tui()),
            component.info.fg,
            component.link.fg,
        ]
        .into_iter()
        .flatten()
        .filter(|color| *color != Color::Default)
        .collect();
        let tiers = Tiers::derive(&component, styles.control_focused.fg, &alternates);
        Self::from_tiers(tiers, component, styles.diff, Icons::for_style(icons))
    }

    /// Theme from a component theme and diff styles, with tiers derived from the component
    /// theme alone.
    #[must_use]
    pub fn from_parts(component: ComponentTheme, diff: DiffViewerStyle, icons: Icons) -> Self {
        let alternates: Vec<Color> = [component.info.fg, component.link.fg]
            .into_iter()
            .flatten()
            .collect();
        let tiers = Tiers::derive(&component, None, &alternates);
        Self::from_tiers(tiers, component, diff, icons)
    }

    /// Theme from `tiers`, the component theme they came from, and diff styles.
    #[must_use]
    pub fn from_tiers(
        tiers: Tiers,
        component: ComponentTheme,
        diff: DiffViewerStyle,
        icons: Icons,
    ) -> Self {
        let overlay_component = overlay_component(&component, &tiers);
        let component = review_component(&component, &tiers);
        let bold = |style: Style| style.add_modifier(Modifier::BOLD);
        let accent_bg = |style: Style| style.fg.map_or(style, |fg| Style::new().bg(fg));
        // The cursor line is the theme's (inactive) selection; a selected range is the accent
        // mixed into the canvas, so the two never coincide.
        let cursor_bg = component
            .selected_inactive
            .bg
            .or(component.selected.bg)
            .or(tiers.surface_alt.bg)
            .unwrap_or(Color::Rgb(44, 48, 60));
        let selection_bg = mixed(tiers.accent.fg, tiers.canvas.bg, RANGE_ACCENT_SHARE)
            .or(component.selected.bg)
            .filter(|selection| *selection != cursor_bg)
            .unwrap_or(Color::Rgb(54, 70, 110));
        // Counts and change bars use the diff's own added/removed colors, so the tree, the
        // headers, and the rows agree.
        // A theme whose diff roles carry only modifiers (no colors) keeps those modifiers.
        let added = fg_only(diff.added).unwrap_or_else(|| component.success.patch(diff.added));
        let removed = fg_only(diff.removed).unwrap_or_else(|| component.error.patch(diff.removed));
        let gutter = diff.muted;
        let components = components(&component, &tiers, cursor_bg);
        Self {
            tiers,
            component,
            overlay_component,
            diff: DiffViewerStyle {
                muted: gutter,
                ..diff
            },
            canvas: tiers.canvas,
            text: tiers.text,
            title: bold(tiers.accent),
            pane_title: bold(tiers.muted),
            pane_title_focused: bold(tiers.accent),
            pill: pill(&tiers),
            pill_accent: tiers.on_accent,
            pill_warning: bold(component.warning.fg.map_or(component.warning, |fg| {
                Style::new().bg(fg).fg(on(fg, &tiers))
            })),
            selection_bar: bold(tiers.accent),
            accent: tiers.accent,
            muted: tiers.muted,
            error: component.error,
            notice: component.warning,
            file_notice: tiers.muted.add_modifier(Modifier::ITALIC),
            status_added: bold(added),
            status_modified: modified_status(&tiers, [component.warning, added, removed]),
            status_deleted: bold(removed),
            status_renamed: bold(tiers.accent_alt),
            status_binary: tiers.muted.add_modifier(Modifier::ITALIC),
            status_unknown: tiers.muted,
            path: tiers.strong,
            path_viewed: tiers.muted,
            directory: tiers.strong,
            additions: added,
            deletions: removed,
            bar_added: added,
            bar_removed: removed,
            bar_empty: tiers.dim,
            badge_draft: bold(tiers.accent_alt),
            badge_open: bold(component.info),
            badge_resolved: bold(component.success),
            badge_outdated: tiers.muted.add_modifier(Modifier::ITALIC),
            badge_ai: bold(tiers.accent).add_modifier(Modifier::ITALIC),
            viewed: bold(component.success),
            gutter,
            hunk: diff.hunk,
            hunk_fill: tiers.canvas.patch(diff.hunk),
            files: FileStyles::of(&tiers),
            source_header: bold(tiers.accent),
            gap: tiers.dim,
            gap_action: bold(component.link),
            thread: component.info,
            cursor: Style::new().bg(cursor_bg),
            selection: Style::new().bg(selection_bg),
            search_match: Style::new()
                .fg(Color::Rgb(20, 20, 20))
                .bg(component.warning.fg.unwrap_or(Color::Yellow)),
            search_current: accent_bg(tiers.accent)
                .patch(tiers.on_accent)
                .add_modifier(Modifier::BOLD),
            border: component.border,
            border_focused: tiers.accent,
            header_bar: tiers.surface_alt,
            footer: tiers.surface_alt.patch(tiers.muted),
            input: tiers.text,
            placeholder: tiers.dim.add_modifier(Modifier::ITALIC),
            scrollbar: components.scrollbar,
            mark_thread: component.info,
            mark_file: tiers.accent,
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

    /// This theme for a terminal showing `depth` colors.
    ///
    /// Without colors, the cues that relied on a background alone get a modifier: the cursor
    /// line and the selected range are reversed, search matches underlined, filled pills and
    /// the publish button reversed, and selected rows keep their selection bar.
    #[must_use]
    pub fn for_color_depth(mut self, depth: TerminalColorDepth) -> Self {
        if depth == TerminalColorDepth::Monochrome {
            self.cursor = self.cursor.add_modifier(Modifier::REVERSED);
            self.selection = self
                .selection
                .add_modifier(Modifier::REVERSED | Modifier::UNDERLINE);
            self.search_match = self.search_match.add_modifier(Modifier::UNDERLINE);
            self.search_current = self
                .search_current
                .add_modifier(Modifier::UNDERLINE | Modifier::REVERSED);
            self.tree_focused.selected = self.tree_focused.selected.add_modifier(Modifier::BOLD);
            self.pill_accent = self.pill_accent.add_modifier(Modifier::REVERSED);
            self.publish.button.normal =
                self.publish.button.normal.add_modifier(Modifier::REVERSED);
        }
        self
    }

    /// `policy` with the selection bar as its highlight symbol.
    #[must_use]
    pub const fn menu_policy(&self, mut policy: MenuPolicy) -> MenuPolicy {
        policy.list.highlight = SelectableListHighlightPolicy::new(self.icons.selection_bar, true)
            .styled(self.selection_bar);
        policy
    }

    /// The selection bar of trees and lists.
    #[must_use]
    pub const fn tree_selection_bar(&self) -> TreeViewSelectionBar {
        TreeViewSelectionBar::new(self.icons.selection_bar, self.selection_bar)
    }

    /// A pane title, ` title `: accent when the pane has focus, muted otherwise.
    #[must_use]
    pub fn pane_title(&self, title: &str, focused: bool) -> Line {
        let style = if focused {
            self.pane_title_focused
        } else {
            self.pane_title
        };
        Line::from_spans(vec![Span::styled(format!(" {title} "), style)])
    }

    /// A pane's right title, ` status `, in muted text; nothing when the status is empty.
    #[must_use]
    pub fn pane_status(&self, status: &str) -> Line {
        if status.is_empty() {
            return Line::default();
        }
        Line::from_spans(vec![Span::styled(format!(" {status} "), self.muted)])
    }

    /// Whether the theme has a desk color (torn pages need one).
    #[must_use]
    pub const fn has_desk(&self) -> bool {
        self.files.desk.bg.is_some()
    }

    /// The status letter as a pill: the status color as fill.
    #[must_use]
    pub fn status_pill(&self, status: ReviewFileStatus) -> Style {
        let style = self.status(status);
        style.fg.map_or_else(
            || style.add_modifier(Modifier::REVERSED),
            |fill| {
                Style::new()
                    .bg(fill)
                    .fg(on(fill, &self.tiers))
                    .add_modifier(Modifier::BOLD)
            },
        )
    }

    /// Style of a status.
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

fn components(theme: &ComponentTheme, tiers: &Tiers, cursor_bg: Color) -> Components {
    let component = *theme;
    let bold = |style: Style| style.add_modifier(Modifier::BOLD);
    let tree = component.tree_view_styles();
    let button = tiers.on_accent;
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
            hovered_divider: tiers.accent,
            active_divider: bold(tiers.accent),
        },
        badges: BadgeStyles {
            default: pill(tiers),
            info: pill(tiers),
            success: pill(tiers).patch(fg_only(component.success).unwrap_or_default()),
            warning: pill(tiers).patch(fg_only(component.warning).unwrap_or_default()),
            error: pill(tiers).patch(fg_only(component.error).unwrap_or_default()),
            muted: pill(tiers).patch(tiers.muted),
        },
        progress: ProgressBarStyles {
            filled: component.success,
            empty: tiers.dim,
            label: tiers.strong,
            complete: bold(component.success),
            indeterminate: component.info,
            background: Style::new(),
        },
        hints: KeyHintBarStyles {
            key: bold(tiers.accent),
            label: tiers.muted,
            separator: tiers.dim,
            disabled: component.disabled,
            background: tiers.surface_alt,
        },
        status: StatusBarStyles {
            background: tiers.surface_alt,
            ..component.status_bar_styles()
        },
        toasts: ToastStackStyles {
            surface: tiers.overlay,
            ..component.toast_stack_styles()
        },
        // Publish is a filled button; its menu toggle shares the fill.
        publish: SplitButtonStyles {
            button: ButtonStyles {
                normal: button,
                focused: button.add_modifier(Modifier::UNDERLINE),
                hovered: button.add_modifier(Modifier::UNDERLINE),
                pressed: button.add_modifier(Modifier::REVERSED),
                disabled: tiers.surface_alt.patch(component.disabled),
            },
            menu: overlay_component(&component, tiers).into(),
            border: tiers.accent,
        },
    }
}

/// The component theme of the review's own surfaces: the tiers' canvas and overlay, rounded
/// panels unless the theme chooses glyphs, and accent focus.
fn review_component(component: &ComponentTheme, tiers: &Tiers) -> ComponentTheme {
    ComponentTheme {
        canvas: tiers.canvas,
        surfaces: ComponentSurfaces {
            normal: tiers.canvas,
            // BMUX pickers and toasts sit on the raised surface; the review shows them as
            // overlays.
            raised: tiers.overlay,
            overlay: tiers.overlay,
            scrim: component.surfaces.scrim,
        },
        accent: tiers.accent,
        title: tiers.accent.add_modifier(Modifier::BOLD),
        border_focused: tiers.accent,
        chrome: ComponentChrome {
            border_set: Some(component.chrome.border_set.unwrap_or(BorderSet::ROUNDED)),
            ..component.chrome
        },
        ..*component
    }
}

/// The component theme of content inside an overlay (lists in pickers, the palette, menus):
/// the overlay is their canvas.
fn overlay_component(component: &ComponentTheme, tiers: &Tiers) -> ComponentTheme {
    let review = review_component(component, tiers);
    ComponentTheme {
        canvas: tiers.overlay,
        surfaces: ComponentSurfaces {
            normal: tiers.overlay,
            ..review.surfaces
        },
        ..review
    }
}

/// The modified-file status: the accent, or neutral strong text when the accent is one of
/// the `taken` colors (warning, which means "needs you", or the added and removed colors).
fn modified_status(tiers: &Tiers, taken: [Style; 3]) -> Style {
    let accent = tiers.accent.fg;
    if accent.is_some() && taken.iter().any(|style| style.fg == accent) {
        tiers.strong
    } else {
        tiers.accent.add_modifier(Modifier::BOLD)
    }
}

/// Share of the accent mixed into the canvas for file headers.
const FILE_HEADER_TINT: f64 = 0.16;
/// Share of black mixed into a canvas for the desk between files.
const DESK_SHADE: f64 = 0.38;
/// Share of black mixed into a light canvas for the desk.
const DESK_SHADE_LIGHT: f64 = 0.10;
/// Share of the text color mixed into a canvas too dark to shade.
const DESK_LIFT: f64 = 0.07;

/// Styles of files' chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStyles {
    /// File header row fill (it stays pinned at the top): the accent mixed into the canvas, so
    /// it differs from the header and footer bars and from content.
    pub header: Style,
    /// The desk between files: what shows where a page is torn off. No fill when the theme
    /// has no RGB canvas to derive one from (see [`ReviewTheme::has_desk`]).
    pub desk: Style,
    /// A page's torn edge drawn over the desk: the canvas color as foreground.
    pub tear: Style,
    /// The edge under the pinned headers.
    pub pinned_edge: Style,
}

impl FileStyles {
    fn of(tiers: &Tiers) -> Self {
        Self {
            header: file_header_fill(tiers),
            desk: desk_fill(tiers),
            tear: tear_style(tiers),
            pinned_edge: Style::new().add_modifier(Modifier::UNDERLINE),
        }
    }
}

/// The file header fill: the accent mixed into the canvas, else the raised surface.
fn file_header_fill(tiers: &Tiers) -> Style {
    mixed(tiers.accent.fg, tiers.canvas.bg, FILE_HEADER_TINT)
        .map_or(tiers.surface_alt, |bg| tiers.text.bg(bg))
}

/// The desk between files: the canvas shaded (or, when it is nearly black, lifted).
fn desk_fill(tiers: &Tiers) -> Style {
    let Some(Color::Rgb(r, g, b)) = tiers.canvas.bg else {
        return Style::new();
    };
    let canvas = Rgb::new(r, g, b);
    let black = Some(Color::Rgb(0, 0, 0));
    let shade = if canvas.contrast_ratio(Rgb::BLACK) < 1.25 {
        mixed(tiers.text.fg, tiers.canvas.bg, DESK_LIFT)
    } else if canvas.contrast_ratio(Rgb::BLACK) > 10.0 {
        mixed(black, tiers.canvas.bg, DESK_SHADE_LIGHT)
    } else {
        mixed(black, tiers.canvas.bg, DESK_SHADE)
    };
    shade.map_or_else(Style::new, |bg| Style::new().bg(bg))
}

/// A torn edge: the page (canvas) color drawn over the desk.
fn tear_style(tiers: &Tiers) -> Style {
    let desk = desk_fill(tiers);
    match (tiers.canvas.bg, desk.bg) {
        (Some(page), Some(_)) => desk.fg(page),
        _ => tiers.dim,
    }
}

/// A neutral pill: text on the overlay fill, which stands apart from the header and footer
/// bars (`surface_alt`) it sits on.
const fn pill(tiers: &Tiers) -> Style {
    tiers.overlay.patch(tiers.text)
}

/// `a` mixed into `b` by `share`, when both are RGB colors.
fn mixed(a: Option<Color>, b: Option<Color>, share: f64) -> Option<Color> {
    match (a?, b?) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => Some(Color::from(bmux_color::mix(
            Rgb::new(ar, ag, ab),
            Rgb::new(br, bg, bb),
            share,
        ))),
        _ => None,
    }
}

/// Text color readable on a `fill` background: the tiers' `on_accent` ink when the fill is
/// RGB, else the canvas color.
fn on(fill: Color, tiers: &Tiers) -> Color {
    match fill {
        Color::Rgb(r, g, b) => Color::from(tiers::readable_extreme(Rgb::new(r, g, b))),
        _ => tiers.canvas.bg.unwrap_or(Color::Black),
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
                assert!(
                    statuses[index + 1..].iter().all(|other| other != style),
                    "{statuses:?}"
                );
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

    /// Every bundled bcode theme, dark and light variants alike.
    fn bundled_themes() -> Vec<(String, ReviewTheme)> {
        use bcode_tui_theme::definition::{
            ResolvedThemeVariant, ThemeCatalog, ThemeKind, ThemeSelection,
        };
        let catalog = ThemeCatalog::bundled().expect("bundled themes");
        // Selectable themes on their own; profiles (chrome without colors) over the default.
        let selections: Vec<ThemeSelection> = catalog
            .definitions()
            .filter_map(|definition| match definition.kind() {
                ThemeKind::Theme => Some(ThemeSelection::new(definition.id())),
                ThemeKind::Profile => Some(
                    ThemeSelection::new(DEFAULT_THEME).overlays(vec![definition.id().to_owned()]),
                ),
                ThemeKind::Base => None,
            })
            .collect();
        let mut themes = Vec::new();
        for selection in selections {
            for variant in [ResolvedThemeVariant::Dark, ResolvedThemeVariant::Light] {
                let Ok(resolved) = catalog.resolve(&selection.clone().variant(variant)) else {
                    continue;
                };
                let id = format!("{selection:?}");
                let host = HostTheme::from_definition(Some(&resolved));
                themes.push((
                    format!("{id} ({variant:?})"),
                    ReviewTheme::derive(Some(&host.styles), IconStyle::Unicode),
                ));
            }
        }
        assert!(themes.len() > 20, "{}", themes.len());
        themes
    }

    #[test]
    fn every_bundled_theme_yields_a_complete_review_theme() {
        for (name, theme) in bundled_themes() {
            let tiers = theme.tiers;
            // Status roles of colored themes are distinct (themes without colors tell statuses
            // apart by their letters), and none of them is the warning color, which means
            // "needs you".
            let statuses = [
                ReviewFileStatus::Added,
                ReviewFileStatus::Modified,
                ReviewFileStatus::Deleted,
                ReviewFileStatus::Renamed,
            ]
            .map(|status| theme.status(status));
            if statuses.iter().all(|style| style.fg.is_some()) {
                for (index, style) in statuses.iter().enumerate() {
                    assert!(
                        statuses[index + 1..].iter().all(|other| other != style),
                        "{name}: {statuses:?}"
                    );
                }
            }
            for style in [theme.status_modified, theme.status_renamed] {
                assert!(
                    style.fg.is_none() || style.fg != theme.notice.fg,
                    "{name}: {style:?}"
                );
            }
            assert_ne!(tiers.accent, tiers.accent_alt, "{name}");
            assert_ne!(theme.cursor, theme.selection, "{name}");
            // RGB canvases get layered surfaces and text that reads on the accent fill.
            if let Some(Color::Rgb(..)) = tiers.canvas.bg {
                assert_ne!(tiers.surface_alt.bg, tiers.canvas.bg, "{name}");
                assert_ne!(tiers.overlay.bg, tiers.canvas.bg, "{name}");
                if let (Some(Color::Rgb(fr, fg, fb)), Some(Color::Rgb(br, bg, bb))) =
                    (tiers.on_accent.fg, tiers.on_accent.bg)
                {
                    let contrast = Rgb::new(fr, fg, fb).contrast_ratio(Rgb::new(br, bg, bb));
                    assert!(contrast >= 4.5, "{name}: {contrast}");
                }
            }
            // Panels are rounded unless the theme chose glyphs.
            assert!(theme.component.chrome.border_set.is_some(), "{name}");
        }
    }

    #[test]
    fn without_colors_background_only_cues_get_modifiers() {
        let theme = ReviewTheme::derive(None, IconStyle::Unicode);
        let plain = theme.for_color_depth(TerminalColorDepth::Monochrome);
        assert!(plain.cursor.modifiers.contains(Modifier::REVERSED));
        assert!(plain.selection.modifiers.contains(Modifier::UNDERLINE));
        assert_ne!(plain.cursor, plain.selection);
        assert!(plain.search_match.modifiers.contains(Modifier::UNDERLINE));
        assert!(
            plain
                .publish
                .button
                .normal
                .modifiers
                .contains(Modifier::REVERSED)
        );
        // Other depths keep the theme as it is.
        for depth in [
            TerminalColorDepth::Ansi16,
            TerminalColorDepth::Ansi256,
            TerminalColorDepth::TrueColor,
        ] {
            assert_eq!(theme.for_color_depth(depth), theme);
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
            icons.running,
            icons.failed,
            icons.output,
            icons.alert,
        ] {
            assert!(glyph.is_ascii(), "{glyph:?}");
        }
    }
}
