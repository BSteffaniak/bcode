//! Painting for the structure inspector.
//!
//! The inspector paints over the transcript body: docked to the right as a
//! side panel when the body is wide, or as an inset modal when it is narrow.
//! Content is composed from BMUX primitives (a bordered [`Surface`] around a
//! wrapping, scrollable [`TextViewComponent`]), so wrapping, clipping, and
//! measurement stay in BMUX. The panel's hit region is the painted panel
//! rectangle, registered above transcript regions so clicks on the panel never
//! reach units underneath. Painting records the body's terminal rectangle and
//! resolved layout, and scrollbar clicks and drags route through BMUX's text
//! view against exactly that geometry.

use std::cell::Cell;

use bmux_tui::chrome::BorderSet;
use bmux_tui::component::{Component, Constraints, LayoutCx, LayoutId};
use bmux_tui::composition::Surface;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::hit::{HitRegion, HitRole};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::Line;
use bmux_tui_components::scroll_view::{ScrollView, ScrollViewState};
use bmux_tui_components::scrollbar_layout::ScrollbarAxisLayoutMode;
use bmux_tui_components::text_view::{TextViewComponent, TextViewPolicy, TextViewStyles};

use crate::inspector::{INSPECTOR_HIT_ID, PaintedInspectorBody};
use crate::render::TuiTheme;

/// Body width from which the inspector docks as a side panel.
const SIDE_PANEL_MIN_BODY_WIDTH: u16 = 96;
/// Side panel width bounds.
const SIDE_PANEL_MIN_WIDTH: u16 = 40;
const SIDE_PANEL_MAX_WIDTH: u16 = 72;
/// Modal insets from the body edges, applied only when the body has room.
const MODAL_INSET_X: u16 = 2;
const MODAL_INSET_Y: u16 = 1;
/// Smallest panel that can show a border and one content cell.
const MIN_PANEL: u16 = 3;
/// Hit layer above transcript regions.
const INSPECTOR_HIT_LAYER: i16 = 1;

/// How the inspector is placed in the transcript body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectorPlacement {
    /// Docked to the right edge of the body.
    Side(Rect),
    /// Inset over the body.
    Modal(Rect),
}

impl InspectorPlacement {
    /// Painted panel rectangle.
    #[must_use]
    pub const fn area(self) -> Rect {
        match self {
            Self::Side(area) | Self::Modal(area) => area,
        }
    }
}

/// Placement of the inspector in transcript `body`, or `None` when the body
/// cannot hold a bordered panel. The panel never extends outside `body`.
#[must_use]
pub fn inspector_placement(body: Rect) -> Option<InspectorPlacement> {
    if body.width < MIN_PANEL || body.height < MIN_PANEL {
        return None;
    }
    if body.width >= SIDE_PANEL_MIN_BODY_WIDTH {
        let width = (body.width / 5 * 2).clamp(SIDE_PANEL_MIN_WIDTH, SIDE_PANEL_MAX_WIDTH);
        let x = body.x.saturating_add(body.width - width);
        return Some(InspectorPlacement::Side(Rect::new(
            x,
            body.y,
            width,
            body.height,
        )));
    }
    // Insets degrade to nothing before the panel drops below its minimum.
    let inset_x = if body.width >= MIN_PANEL + 2 * MODAL_INSET_X + SIDE_PANEL_MIN_WIDTH / 2 {
        MODAL_INSET_X
    } else {
        0
    };
    let inset_y = if body.height >= MIN_PANEL + 2 * MODAL_INSET_Y + 4 {
        MODAL_INSET_Y
    } else {
        0
    };
    Some(InspectorPlacement::Modal(Rect::new(
        body.x.saturating_add(inset_x),
        body.y.saturating_add(inset_y),
        body.width - 2 * inset_x,
        body.height - 2 * inset_y,
    )))
}

/// Stable identity of the inspector body text view.
const BODY_ID: &str = "transcript.inspector.body";

/// The inspector body text view: identity and scroll policy only. Painting
/// adds styles; pointer routing uses it as-is against the painted layout, so
/// both see the same scrollbar geometry.
pub fn body_view<'a, 'state>(
    lines: &'a [Line],
    scroll: &'state Cell<ScrollViewState>,
) -> TextViewComponent<'a, 'state> {
    TextViewComponent::new(BODY_ID, lines, scroll).policy(
        TextViewPolicy::scrollable().vertical_scrollbar(ScrollbarAxisLayoutMode::OverlayOnOverflow),
    )
}

/// Paint the panel into `area`; returns the body text view's painted
/// terminal rectangle and layout.
pub fn paint_panel(
    area: Rect,
    lines: &[Line],
    scroll: &Cell<ScrollViewState>,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
    component_theme: &bmux_tui_components::theme::ComponentTheme,
) -> Option<PaintedInspectorBody> {
    let view = body_view(lines, scroll).styles(TextViewStyles {
        text: theme.text,
        empty: theme.muted,
        background: theme.overlay,
        scrollbar: TextViewStyles::default().scrollbar,
    });
    let viewport_id = view.viewport_id();
    let border =
        component_theme
            .chrome
            .border(BorderSet::ROUNDED, theme.border.patch(theme.overlay), false);
    let component = Surface::new(view)
        .id("transcript.inspector")
        .background(theme.overlay)
        .border(border)
        .padding(Insets::new(0, 1, 0, 1));
    let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    // Clamp the stored offset to the measured content before painting.
    if let Some(viewport) = layout.find(&viewport_id) {
        let mut state = scroll.get();
        ScrollView::new().reconcile(viewport, &mut state);
        scroll.set(state);
    }
    let local = LocalRect::new(0, 0, area.width, area.height);
    frame.occlude(LocalRect::terminal(area));
    frame.with_child(i32::from(area.x), i64::from(area.y), local, |cx| {
        component.paint(&layout, cx);
    });
    frame.push_hit(
        HitRegion::new(INSPECTOR_HIT_ID, area)
            .layer(INSPECTOR_HIT_LAYER)
            .role(HitRole::Scroll),
    );
    let body_id = LayoutId::from(BODY_ID);
    let body = layout.find_rect(&body_id)?;
    let body_area = Rect::new(
        area.x.saturating_add(body.x),
        area.y.saturating_add(body.y),
        body.width,
        body.height,
    );
    Some(PaintedInspectorBody {
        area: body_area,
        layout: layout.find(&body_id)?.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placement_docks_wide_bodies_and_insets_narrow_ones_within_the_body() {
        let wide = Rect::new(0, 2, 200, 30);
        let Some(InspectorPlacement::Side(side)) = inspector_placement(wide) else {
            panic!("side panel");
        };
        assert_eq!(side.width, SIDE_PANEL_MAX_WIDTH);
        assert_eq!(side.x + side.width, wide.x + wide.width);
        assert_eq!((side.y, side.height), (wide.y, wide.height));
        let medium = Rect::new(0, 0, SIDE_PANEL_MIN_BODY_WIDTH, 20);
        let Some(InspectorPlacement::Side(side)) = inspector_placement(medium) else {
            panic!("side panel");
        };
        assert_eq!(side.width, SIDE_PANEL_MIN_WIDTH.max(medium.width / 5 * 2));
        for width in 0..SIDE_PANEL_MIN_BODY_WIDTH {
            for height in 0..12 {
                let body = Rect::new(3, 1, width, height);
                match inspector_placement(body) {
                    None => assert!(width < MIN_PANEL || height < MIN_PANEL),
                    Some(InspectorPlacement::Modal(panel)) => {
                        assert_eq!(panel.intersection(body), panel, "{width}x{height}");
                        assert!(panel.width >= MIN_PANEL && panel.height >= MIN_PANEL);
                    }
                    Some(InspectorPlacement::Side(_)) => panic!("narrow bodies use a modal"),
                }
            }
        }
    }
}
