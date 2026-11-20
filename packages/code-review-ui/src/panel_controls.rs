//! The review app's policy for the window controls every panel shares: the file panel, the
//! review document, the threads panel, the guide panel and the review assistant.
//!
//! The controls themselves are BMUX `panel_window`: the title-row minimize, maximize and close
//! controls with their narrow-width drop order, the minimized row and strip, maximize, double
//! clicks on a title row, and the window commands. This module chooses what is the review
//! app's own:
//!
//! * **Panels.** [`PanelKind`] names the panels and says which close: every panel but the
//!   review document, which is what the review is (its close key is refused, and it minimizes
//!   only while another panel in its group stays expanded to take its room).
//! * **Glyphs.** The theme's icons, with their ASCII fallbacks, in one muted cluster joined
//!   to the border: `┤ ▾ ⤢ × ├` ([`controls`]). A hovered control takes the accent colour,
//!   close the error colour.
//! * **Visibility.** The controls show on the focused panel and the panel under the pointer;
//!   a minimized panel always shows its restore control. Keys and double clicks on a title
//!   row work on every panel.
//! * **Keys.** [`ACTIONS`] names the keymap actions that run a [`Command`] on the focused
//!   panel, with the same keys in every panel's scope; grow and shrink move an edge
//!   [`RESIZE_STEP`] cells.
//! * **Hints.** The focused panel's footer lists its panel keys ([`hints`]) after the
//!   panel's own hints and before app-wide ones (palette, help, quit), so a narrow footer,
//!   which keeps the leading hints that fit, drops them before any primary hint. Labels follow
//!   the panel's shape: maximize reads "restore" while maximized, minimize while minimized,
//!   and the size keys show only at the panel's normal size.
//! * **Statuses.** What a refused close or minimize of the document says.

use crate::theme::ReviewTheme;
use bmux_tui::composition::Column;
pub use bmux_tui_components::panel_window::{
    DOUBLE_CLICK, PanelShape as Shape, TitleClicks, TitleTarget, WindowCommand as Command,
    WindowControl as Control,
};
use bmux_tui_components::panel_window::{WindowControls, WindowGlyphs, minimized_content};

/// Cells of a panel's title the title-row controls leave it at least (the whole title when it
/// is shorter).
pub const MIN_TITLE_CELLS: u16 = 16;

/// Columns or rows a grow or shrink key moves a panel's edge.
pub const RESIZE_STEP: i32 = 4;

/// A panel of the review app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PanelKind {
    /// The file panel.
    Files,
    /// The review document.
    Review,
    /// The threads panel.
    Threads,
    /// The guide panel.
    Guide,
    /// The review assistant.
    Assistant,
}

impl PanelKind {
    /// Every panel.
    pub const ALL: [Self; 5] = [
        Self::Files,
        Self::Review,
        Self::Threads,
        Self::Guide,
        Self::Assistant,
    ];

    /// The panel's name, as its minimized strip shows it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Files => "Files",
            Self::Review => "Review",
            Self::Threads => "Threads",
            Self::Guide => "Guide",
            Self::Assistant => "Assistant",
        }
    }

    /// Whether the panel can be closed (every panel but the review document).
    #[must_use]
    pub const fn closable(self) -> bool {
        !matches!(self, Self::Review)
    }
}

/// The window-control glyphs of `theme`: its icons, with caps that join its panels' border
/// line (a double or thick border takes matching tees).
#[must_use]
pub const fn glyphs(theme: &ReviewTheme) -> WindowGlyphs {
    let icons = &theme.icons;
    let (cap_start, cap_end) = match theme.component.chrome.border_set {
        Some(set) if set.horizontal == '═' => ("╡", "╞"),
        Some(set) if set.horizontal == '━' => ("┥", "┝"),
        Some(set) if set.horizontal == '-' => ("|", "|"),
        _ => (icons.panel_cap_start, icons.panel_cap_end),
    };
    WindowGlyphs {
        minimize: icons.panel_minimize,
        restore_minimized: icons.panel_unminimize,
        maximize: icons.panel_maximize,
        restore_maximized: icons.panel_restore,
        close: icons.panel_close,
        cap_start,
        cap_end,
    }
}

/// The controls of `kind` shown as `shape`, in `theme`.
#[must_use]
pub const fn controls(kind: PanelKind, shape: Shape, theme: &ReviewTheme) -> WindowControls {
    WindowControls::new(shape)
        .closable(kind.closable())
        .glyphs(glyphs(theme))
        .style(theme.muted)
        .hover_styles(theme.accent, theme.error)
        .min_title_width(MIN_TITLE_CELLS)
}

/// Width of a minimized strip in `theme`.
#[must_use]
pub fn strip_width(theme: &ReviewTheme) -> u16 {
    controls(PanelKind::Files, Shape::Strip, theme).strip_width()
}

/// Content of `kind` minimized as `shape` (its name down a strip, nothing on a row), or
/// `None` while it is not minimized.
#[must_use]
pub fn minimized(kind: PanelKind, shape: Shape, theme: &ReviewTheme) -> Option<Column<'static>> {
    minimized_content(
        format!("panel.strip.{}", kind.name()),
        shape,
        kind.name(),
        theme.muted,
    )
}

/// Every panel command with its keymap action name (without the `bcode.code_review.` prefix).
pub const ACTIONS: [(&str, Command); 6] = [
    ("panel_maximize", Command::Maximize),
    ("panel_minimize", Command::Minimize),
    ("panel_close", Command::Close),
    ("panel_grow", Command::Grow),
    ("panel_shrink", Command::Shrink),
    ("panel_reset_size", Command::ResetSize),
];

/// Footer hints `(key, label)` for the panel keys of `kind` shown as `shape`.
///
/// `key` gives an action's key (action names without the `bcode.code_review.` prefix), and an
/// action without one is left out. Grow and shrink share one hint.
pub fn hints(
    kind: PanelKind,
    shape: Shape,
    key: impl Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    let minimized = matches!(shape, Shape::Row | Shape::Strip);
    let mut hints = Vec::new();
    let mut push = |key: Option<String>, label: &str| {
        if let Some(key) = key.filter(|key| !key.is_empty()) {
            hints.push((key, label.to_owned()));
        }
    };
    push(
        key("panel_maximize"),
        if shape == Shape::Maximized {
            "restore"
        } else {
            "maximize"
        },
    );
    push(
        key("panel_minimize"),
        if minimized { "restore" } else { "minimize" },
    );
    if kind.closable() {
        push(key("panel_close"), "close panel");
    }
    if shape == Shape::Normal {
        match (key("panel_grow"), key("panel_shrink")) {
            (Some(grow), Some(shrink)) => push(Some(format!("{grow}/{shrink}")), "size"),
            (grow, shrink) => {
                push(grow, "grow");
                push(shrink, "shrink");
            }
        }
        push(key("panel_reset_size"), "reset size");
    }
    hints
}

/// The command of keymap action `action` (without its prefix).
#[must_use]
pub fn command(action: &str) -> Option<Command> {
    ACTIONS
        .iter()
        .find(|(name, _)| *name == action)
        .map(|(_, command)| *command)
}

/// The status shown when the review document is asked to close.
pub const REVIEW_CLOSE_REFUSED: &str = "the review document stays open; minimize or maximize it";

/// The status shown when the review document is asked to minimize with no expanded panel to
/// take its room.
pub const REVIEW_MINIMIZE_REFUSED: &str = "no open panel to take the document's room";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::IconStyle;

    #[test]
    fn hints_follow_the_panel_shape_and_skip_unbound_keys() {
        let keys = |action: &str| {
            let key = match action {
                "panel_maximize" => "alt+z",
                "panel_minimize" => "alt+c",
                "panel_close" => "alt+w",
                "panel_grow" => "alt+=",
                "panel_shrink" => "alt+-",
                "panel_reset_size" => "alt+0",
                _ => return None,
            };
            Some(key.to_owned())
        };
        let labels = |kind, shape| {
            hints(kind, shape, keys)
                .into_iter()
                .map(|(key, label)| format!("{key} {label}"))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            labels(PanelKind::Files, Shape::Normal),
            [
                "alt+z maximize",
                "alt+c minimize",
                "alt+w close panel",
                "alt+=/alt+- size",
                "alt+0 reset size"
            ]
        );
        assert_eq!(
            labels(PanelKind::Review, Shape::Maximized),
            ["alt+z restore", "alt+c minimize"]
        );
        assert_eq!(
            labels(PanelKind::Threads, Shape::Strip),
            ["alt+z maximize", "alt+c restore", "alt+w close panel"]
        );
        let only_grow = hints(PanelKind::Guide, Shape::Normal, |action: &str| {
            (action == "panel_grow").then(|| "alt+=".to_owned())
        });
        assert_eq!(only_grow, [("alt+=".to_owned(), "grow".to_owned())]);
    }

    #[test]
    fn every_panel_but_the_document_closes() {
        let theme = ReviewTheme::derive(None, IconStyle::default());
        for kind in PanelKind::ALL {
            let list = controls(kind, Shape::Normal, &theme).list();
            assert_eq!(list.contains(&Control::Close), kind != PanelKind::Review);
        }
    }

    #[test]
    fn controls_use_the_theme_glyphs_and_their_ascii_fallbacks() {
        for style in [IconStyle::Unicode, IconStyle::Ascii] {
            let theme = ReviewTheme::derive(None, style);
            let icons = theme.icons;
            let maximized = controls(PanelKind::Files, Shape::Maximized, &theme);
            assert_eq!(maximized.glyph(Control::Maximize), icons.panel_restore);
            assert_eq!(maximized.glyph(Control::Close), icons.panel_close);
            let normal = controls(PanelKind::Files, Shape::Normal, &theme);
            assert_eq!(normal.glyph(Control::Minimize), icons.panel_minimize);
            assert_eq!(normal.glyph(Control::Maximize), icons.panel_maximize);
        }
    }

    #[test]
    fn commands_round_trip_their_action_names() {
        for (name, command) in ACTIONS {
            assert_eq!(super::command(name), Some(command));
        }
        assert_eq!(super::command("files_toggle"), None);
    }
}
