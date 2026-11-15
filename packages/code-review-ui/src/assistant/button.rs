//! The Ask AI header button: the assistant's visible entry point on every screen.
//!
//! * **Place.** The right end of the screen's header row. The app plans the button from the
//!   screen's width, tells the screen how many cells to leave free at the end of its header
//!   ([`ButtonPlan::width`]), paints the button there and registers its hit region from the same
//!   plan, so the painted cells and the clickable cells are one rectangle.
//! * **Degradation.** `✦ Ask AI`, then `✦ AI`, then `✦` as the header narrows; below
//!   [`MIN_HEADER`] cells no button is shown (`ctrl+k` and the palette still open the panel).
//! * **State.** A filled pill while the panel is open; a badge (a spinner frame while the
//!   conversation works, `?` while it waits on the user) while the panel is closed or zoomed
//!   away.

use crate::theme::ReviewTheme;
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::text::{Line, Span};

/// Hit region id of the button.
pub const BUTTON_ID: &str = "app.assistant.button";
/// Header width from which the full label shows.
pub const FULL_HEADER: u16 = 48;
/// Header width from which the short label shows.
pub const SHORT_HEADER: u16 = 24;
/// Narrowest header that shows the button (the icon alone).
pub const MIN_HEADER: u16 = 8;

/// Header cells always left to the screen's title.
const TITLE_ROOM: u16 = 4;

/// What the button shows next to its label.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Badge {
    /// Nothing.
    #[default]
    None,
    /// The conversation works (a spinner frame).
    Working(String),
    /// The conversation waits on the user (a permission request).
    NeedsYou,
}

/// The planned button: its label and width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ButtonPlan {
    /// The painted label, padded.
    pub label: Line,
    /// Cells it takes, measured from the label.
    pub width: u16,
}

/// Plan the button for a header `header_width` cells wide; `None` when it does not fit.
#[must_use]
pub fn plan(
    header_width: u16,
    open: bool,
    badge: &Badge,
    theme: &ReviewTheme,
) -> Option<ButtonPlan> {
    if header_width < MIN_HEADER {
        return None;
    }
    let icon = theme.icons.assistant;
    let text = if header_width >= FULL_HEADER {
        format!("{icon} Ask AI")
    } else if header_width >= SHORT_HEADER {
        format!("{icon} AI")
    } else {
        icon.to_owned()
    };
    let badge = match badge {
        Badge::None => String::new(),
        Badge::Working(frame) => format!(" {frame}"),
        Badge::NeedsYou => " ?".to_owned(),
    };
    let style = if open {
        theme.pill_accent
    } else {
        theme.accent
    };
    let label = Line::from_spans(vec![Span::styled(format!(" {text}{badge} "), style)]);
    let width = u16::try_from(label.width()).unwrap_or(u16::MAX);
    // The screen's title keeps a few cells.
    (width.saturating_add(TITLE_ROOM) <= header_width).then_some(ButtonPlan { label, width })
}

impl ButtonPlan {
    /// Where the button sits on the header row `header`.
    #[must_use]
    pub fn area(&self, header: Rect) -> Rect {
        let width = self.width.min(header.width);
        Rect::new(header.right().saturating_sub(width), header.y, width, 1)
    }

    /// Paint the button at the end of `header` and register its hit region.
    pub fn paint(&self, header: Rect, theme: &ReviewTheme, cx: &mut PaintCx<'_, '_>) -> Rect {
        let area = self.area(header);
        cx.fill(
            LocalRect::new(area.x.into(), area.y.into(), area.width, 1),
            " ",
            theme.header_bar,
        );
        cx.write_line(
            LocalRect::new(area.x.into(), area.y.into(), area.width, 1),
            &self.label,
        );
        cx.push_hit(bmux_tui::hit::HitRegion::new(BUTTON_ID, area).layer(2));
        area
    }
}
