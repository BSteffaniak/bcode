//! Window controls every panel of the review app shares: the file panel, the review document,
//! the threads panel, the guide panel and the review assistant.
//!
//! This module is the one definition of those controls; each panel consumes it.
//!
//! * **Title-row controls.** [`Controls`] appends the panel's last title-row actions, left to
//!   right: minimize (restore while minimized), maximize (restore while maximized), and close.
//!   They are BMUX pane title actions, so their painting, their hit regions
//!   (`<pane id>.action.minimize`, `.maximize`, `.close`) and [`hit`] all come from the pane's
//!   one title layout. The title keeps [`MIN_TITLE_CELLS`] (or all of a shorter title): when
//!   the row has less room, the pane drops the leftmost actions first: a panel's own actions
//!   (the assistant's), then minimize, then maximize; close goes last. Past that, the title is
//!   clipped. Actions never overlap the title.
//! * **Minimized.** A minimized panel collapses along the axis of the group it sits in: to its
//!   title row when stacked, or to a strip [`strip_width`] columns wide when side by side. A
//!   strip shows only its restore control and the panel's name down the strip
//!   ([`strip_content`]).
//! * **Maximized.** A maximized panel fills the review body (the assistant fills the app),
//!   and the other panels are not laid out until it is restored. Restore it with the same
//!   control, its key, a double click on its title, or the focused panel's back key (`esc`).
//! * **Double click.** Two presses on a title row within [`DOUBLE_CLICK`] toggle maximize
//!   ([`TitleClicks`]).
//! * **Keys.** [`Command`] names the keymap actions that act on the focused panel, with the
//!   same keys in every panel's scope.
//! * **Exception.** The review document has no close control and its close key is refused: it
//!   is what the review is. It minimizes only when another panel in its group stays expanded to
//!   take its room.

use crate::theme::ReviewTheme;
use bmux_tui::composition::{Column, TextBlock};
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::text::{Line, Span, Text, TextWrap};
use bmux_tui_components::pane::{Pane, PaneTitleAction};
use std::time::{Duration, Instant};

/// Two presses on a panel's title row within this time toggle maximize.
pub const DOUBLE_CLICK: Duration = Duration::from_millis(400);

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

/// A title-row control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// Minimize, or restore a minimized panel.
    Minimize,
    /// Maximize, or restore a maximized panel.
    Maximize,
    /// Close (hide) the panel.
    Close,
}

impl Control {
    /// The title action id (the hit region is `<pane id>.action.<id>`).
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Minimize => "minimize",
            Self::Maximize => "maximize",
            Self::Close => "close",
        }
    }

    /// The control with title action id `id`.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        [Self::Minimize, Self::Maximize, Self::Close]
            .into_iter()
            .find(|control| control.id() == id)
    }

    /// The command the control runs.
    #[must_use]
    pub const fn command(self) -> Command {
        match self {
            Self::Minimize => Command::Minimize,
            Self::Maximize => Command::Maximize,
            Self::Close => Command::Close,
        }
    }
}

/// How a panel is shown, for its controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Shape {
    /// At its size.
    #[default]
    Normal,
    /// Minimized to its title row (stacked).
    Row,
    /// Minimized to a strip (side by side).
    Strip,
    /// Maximized.
    Maximized,
}

impl Shape {
    /// Whether the panel is minimized (a row or a strip).
    #[must_use]
    pub const fn minimized(self) -> bool {
        matches!(self, Self::Row | Self::Strip)
    }
}

/// The controls of one panel in one shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Controls {
    /// The panel.
    pub kind: PanelKind,
    /// How it is shown.
    pub shape: Shape,
}

impl Controls {
    /// The controls of `kind` shown as `shape`.
    #[must_use]
    pub const fn new(kind: PanelKind, shape: Shape) -> Self {
        Self { kind, shape }
    }

    /// The controls in title-row order, left to right. A strip has only its restore control.
    #[must_use]
    pub fn list(self) -> Vec<Control> {
        if self.shape == Shape::Strip {
            return vec![Control::Minimize];
        }
        let mut list = vec![Control::Minimize, Control::Maximize];
        if self.kind.closable() {
            list.push(Control::Close);
        }
        list
    }

    /// The glyph of `control` in this shape.
    #[must_use]
    pub const fn glyph(self, control: Control, theme: &ReviewTheme) -> &'static str {
        let icons = &theme.icons;
        match control {
            Control::Minimize if self.shape.minimized() => icons.panel_restore,
            Control::Minimize => icons.panel_minimize,
            Control::Maximize if matches!(self.shape, Shape::Maximized) => icons.panel_restore,
            Control::Maximize => icons.panel_maximize,
            Control::Close => icons.panel_close,
        }
    }

    /// `pane` with the controls appended as its last title-row actions. Unless the panel is a
    /// strip, the title keeps [`MIN_TITLE_CELLS`]: controls that would leave it less drop,
    /// leftmost first.
    #[must_use]
    pub fn apply<'a>(self, mut pane: Pane<'a>, theme: &ReviewTheme) -> Pane<'a> {
        if self.shape != Shape::Strip {
            pane = pane.min_title_width(MIN_TITLE_CELLS);
        }
        for control in self.list() {
            pane = pane.action(PaneTitleAction::new(
                control.id(),
                label(self.glyph(control, theme), theme),
            ));
        }
        pane
    }
}

/// A control's label: its glyph with a cell of padding on each side.
fn label(glyph: &str, theme: &ReviewTheme) -> Line {
    Line::from_spans(vec![Span::styled(format!(" {glyph} "), theme.muted)])
}

/// Width of a minimized strip: its restore control between the pane's two corners.
#[must_use]
pub fn strip_width(theme: &ReviewTheme) -> u16 {
    let restore = label(theme.icons.panel_restore, theme).width();
    u16::try_from(restore).unwrap_or(u16::MAX).saturating_add(2)
}

/// Content of a minimized strip: the panel's name down the strip, a letter a row, clipped to
/// the strip by the layout.
#[must_use]
pub fn strip_content(kind: PanelKind, theme: &ReviewTheme) -> Column<'static> {
    // The names are fixed ASCII words, so each letter is one grapheme.
    let mut column = Column::new().id(format!("panel.strip.{}", kind.name()));
    for letter in kind.name().chars() {
        column = column.child(
            TextBlock::new(Text::from_lines(vec![Line::from_spans(vec![
                Span::styled(format!(" {letter}"), theme.muted),
            ])]))
            .wrap(TextWrap::None),
        );
    }
    column
}

/// What a press on a pane's title row is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleHit {
    /// One of the panel controls.
    Control(Control),
    /// One of the panel's own title actions (index into them).
    Action(usize),
    /// Elsewhere on the title row, inside the corners.
    Title,
}

/// What `position` is on in the title row of `pane` laid out at `area` (absolute).
///
/// It hit tests the pane's own title layout. The pane's title actions are its `own_actions`
/// followed by `controls` ([`Controls::apply`]). `None` off the title row, and on its corners
/// (which a divider may cover).
#[must_use]
pub fn hit(
    pane: &Pane<'_>,
    own_actions: usize,
    controls: Controls,
    area: Rect,
    position: Point,
) -> Option<TitleHit> {
    if area.width < 3
        || area.height == 0
        || position.y != area.y
        || position.x <= area.x
        || position.x >= area.right().saturating_sub(1)
    {
        return None;
    }
    let list = controls.list();
    let layout = pane.title_layout(Rect::new(0, 0, area.width, area.height));
    for (index, rect) in layout.actions {
        let absolute = Rect::new(area.x + rect.x, area.y + rect.y, rect.width, rect.height);
        if absolute.contains(position) {
            return Some(
                index
                    .checked_sub(own_actions)
                    .and_then(|slot| list.get(slot).copied())
                    .map_or(TitleHit::Action(index), TitleHit::Control),
            );
        }
    }
    Some(TitleHit::Title)
}

/// Double-click detection on panel title rows.
#[derive(Debug, Default, Clone, Copy)]
pub struct TitleClicks {
    last: Option<(PanelKind, Instant)>,
}

impl TitleClicks {
    /// A press on `kind`'s title row: whether it completes a double click (and then starts
    /// over).
    pub fn press(&mut self, kind: PanelKind) -> bool {
        let now = Instant::now();
        let double = self
            .last
            .is_some_and(|(last, at)| last == kind && now.duration_since(at) <= DOUBLE_CLICK);
        self.last = (!double).then_some((kind, now));
        double
    }

    /// Forget the last press (the title row moved or went away).
    pub const fn reset(&mut self) {
        self.last = None;
    }
}

/// A panel action of the keymap, acting on the focused panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Maximize, or restore a maximized panel.
    Maximize,
    /// Minimize, or restore a minimized panel.
    Minimize,
    /// Close (hide) the panel.
    Close,
    /// Grow the panel by [`RESIZE_STEP`] cells.
    Grow,
    /// Shrink the panel by [`RESIZE_STEP`] cells.
    Shrink,
    /// Forget the panel's resized size.
    ResetSize,
}

impl Command {
    /// Every command with its keymap action name (without the `bcode.code_review.` prefix).
    pub const ACTIONS: [(&'static str, Self); 6] = [
        ("panel_maximize", Self::Maximize),
        ("panel_minimize", Self::Minimize),
        ("panel_close", Self::Close),
        ("panel_grow", Self::Grow),
        ("panel_shrink", Self::Shrink),
        ("panel_reset_size", Self::ResetSize),
    ];

    /// The command of keymap action `action` (without its prefix).
    #[must_use]
    pub fn from_action(action: &str) -> Option<Self> {
        Self::ACTIONS
            .iter()
            .find(|(name, _)| *name == action)
            .map(|(_, command)| *command)
    }

    /// Signed cells a grow or shrink moves the panel's edge, `count` times; zero otherwise.
    #[must_use]
    pub fn delta(self, count: Option<u32>) -> i32 {
        let step = RESIZE_STEP.saturating_mul(i32::try_from(count.unwrap_or(1)).unwrap_or(1));
        match self {
            Self::Grow => step,
            Self::Shrink => -step,
            _ => 0,
        }
    }
}

/// The status shown when the review document is asked to close.
pub const REVIEW_CLOSE_REFUSED: &str = "the review document stays open; minimize or maximize it";

/// The status shown when the review document is asked to minimize with no expanded panel to
/// take its room.
pub const REVIEW_MINIMIZE_REFUSED: &str = "no open panel to take the document's room";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_panel_but_the_document_closes_and_strips_only_restore() {
        for kind in PanelKind::ALL {
            let list = Controls::new(kind, Shape::Normal).list();
            assert_eq!(list[..2], [Control::Minimize, Control::Maximize]);
            assert_eq!(list.contains(&Control::Close), kind != PanelKind::Review);
            assert_eq!(
                Controls::new(kind, Shape::Strip).list(),
                [Control::Minimize]
            );
        }
    }

    #[test]
    fn commands_round_trip_their_action_names() {
        for (name, command) in Command::ACTIONS {
            assert_eq!(Command::from_action(name), Some(command));
        }
        assert_eq!(Command::from_action("files_toggle"), None);
        assert_eq!(Command::Grow.delta(Some(2)), 2 * RESIZE_STEP);
        assert_eq!(Command::Shrink.delta(None), -RESIZE_STEP);
    }

    #[test]
    fn a_double_click_needs_two_presses_on_the_same_title() {
        let mut clicks = TitleClicks::default();
        assert!(!clicks.press(PanelKind::Files));
        assert!(!clicks.press(PanelKind::Guide), "another panel starts over");
        assert!(clicks.press(PanelKind::Guide));
        assert!(!clicks.press(PanelKind::Guide), "a third press starts over");
    }
}
