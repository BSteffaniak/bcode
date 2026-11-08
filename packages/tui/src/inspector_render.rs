//! Painting for the structure inspector.
//!
//! The inspector paints over the transcript body: docked to the right as a
//! side panel when the body is wide, or as an inset modal when it is narrow.
//! Content is composed from BMUX primitives (a bordered [`Surface`] around a
//! wrapping, scrollable [`TextViewComponent`]), so wrapping, clipping, and
//! measurement stay in BMUX. The panel's hit region is the painted panel
//! rectangle, registered above transcript regions so clicks on the panel never
//! reach units underneath.

use std::cell::Cell;

use bmux_tui::chrome::BorderSet;
use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::composition::Surface;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::hit::{HitRegion, HitRole};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span};
use bmux_tui::style::Modifier;
use bmux_tui_components::scroll_view::ScrollView;
use bmux_tui_components::scrollbar_layout::ScrollbarAxisLayoutMode;
use bmux_tui_components::text_view::{TextViewComponent, TextViewPolicy, TextViewStyles};

use crate::app::BmuxApp;
use crate::inspector::{
    INSPECTOR_HIT_ID, InspectorReport, bounded_text, disclosure_name, fact_rows, origin_label,
    value_text,
};
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

/// Inspector content rows for `report`, or a placeholder when nothing
/// structural is focused. Rows are semantic lines; wrapping is BMUX's.
#[must_use]
pub fn report_lines(report: Option<&InspectorReport>, theme: TuiTheme) -> Vec<Line> {
    let Some(report) = report else {
        return vec![
            Line::from_spans(vec![Span::styled(
                "Inspector",
                theme.focused.add_modifier(Modifier::BOLD),
            )]),
            Line::from_spans(vec![Span::styled(
                "The focused target is not a structural unit.",
                theme.muted,
            )]),
        ];
    };
    let heading = |text: &str| {
        Line::from_spans(vec![Span::styled(
            text.to_owned(),
            theme.focused.add_modifier(Modifier::BOLD),
        )])
    };
    let pair = |name: String, value: String, origin: Option<String>| {
        let mut spans = vec![
            Span::styled(name, theme.muted),
            Span::styled(" ", theme.text),
            Span::styled(value, theme.text),
        ];
        if let Some(origin) = origin {
            spans.push(Span::styled(format!(" · {origin}"), theme.info));
        }
        Line::from_spans(spans)
    };
    let mut rows = vec![
        Line::from_spans(vec![
            Span::styled("Inspector · ", theme.focused.add_modifier(Modifier::BOLD)),
            Span::styled(
                report.target.to_string(),
                theme.text.add_modifier(Modifier::BOLD),
            ),
        ]),
        pair(
            "shown".to_owned(),
            disclosure_name(report.effective).to_owned(),
            report
                .manual
                .map(|manual| format!("manual {}", disclosure_name(manual))),
        ),
        Line::default(),
        heading("Facts"),
    ];
    rows.extend(
        fact_rows(&report.facts)
            .into_iter()
            .map(|(name, value)| pair(name, value, None)),
    );
    rows.push(Line::default());
    rows.push(heading("Properties"));
    rows.extend(report.properties.iter().map(|row| {
        pair(
            row.property.clone(),
            value_text(&row.value),
            Some(origin_label(row.origin.as_ref())),
        )
    }));
    if !report.floor.is_empty() {
        rows.push(Line::default());
        rows.push(heading("Floor and overrides"));
        rows.extend(report.floor.iter().map(|note| {
            Line::from_spans(vec![Span::styled(
                bounded_text(note.clone()),
                theme.warning,
            )])
        }));
    }
    if !report.notes.is_empty() {
        rows.push(Line::default());
        rows.push(heading("Grouping"));
        rows.extend(report.notes.iter().map(|note| {
            Line::from_spans(vec![Span::styled(bounded_text(note.clone()), theme.text)])
        }));
    }
    rows
}

/// Area the open inspector paints (and claims for hits) over transcript
/// `body`, or `None` while it is closed or does not fit.
#[must_use]
pub fn inspector_panel(app: &BmuxApp, body: Rect) -> Option<Rect> {
    app.inspector_state()?;
    inspector_placement(body).map(InspectorPlacement::area)
}

/// Paint the open inspector over transcript `body`; returns the painted
/// panel rectangle (also its hit region), or `None` when nothing painted.
pub fn paint_inspector(
    app: &BmuxApp,
    body: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
) -> Option<Rect> {
    let state = app.inspector_state()?;
    let area = inspector_panel(app, body)?;
    let report = app.inspector_report();
    let lines = report_lines(report.as_ref(), theme);
    let scroll = state.scroll_for(app.focused_disclosure_target());
    let component_theme = app.presented_theme().component_theme();
    paint_panel(area, &lines, scroll, frame, theme, &component_theme);
    Some(area)
}

fn paint_panel(
    area: Rect,
    lines: &[Line],
    scroll: &Cell<bmux_tui_components::scroll_view::ScrollViewState>,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
    component_theme: &bmux_tui_components::theme::ComponentTheme,
) {
    let view = TextViewComponent::new("transcript.inspector.body", lines, scroll)
        .policy(
            TextViewPolicy::scrollable()
                .vertical_scrollbar(ScrollbarAxisLayoutMode::OverlayOnOverflow),
        )
        .styles(TextViewStyles {
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
}

#[cfg(test)]
mod tests {
    use bcode_presentation_policy::models::{Disclosure, UnitFacts, UnitKind};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;

    use super::*;
    use crate::inspector::InspectorState;
    use crate::structure::DisclosureTarget;

    fn report(target_text: &str) -> InspectorReport {
        InspectorReport {
            target: DisclosureTarget::Unit(7),
            facts: UnitFacts {
                kind: Some(UnitKind::Tool),
                tool: Some("read".to_owned()),
                target: Some(target_text.to_owned()),
                ..UnitFacts::default()
            },
            effective: Disclosure::Collapsed,
            manual: Some(Disclosure::Expanded),
            properties: Vec::new(),
            floor: vec!["failed units are never below summary".to_owned()],
            notes: vec!["in group:3".to_owned()],
        }
    }

    fn paint(
        area: Rect,
        body: Rect,
        lines: &[Line],
        state: &InspectorState,
    ) -> (Buffer, Vec<HitRegion>) {
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        let mut cx = PaintCx::new(&mut frame);
        // A transcript region under the panel, as the transcript registers.
        cx.push_hit(HitRegion::new("disclosure:unit:7", body));
        if let Some(placement) = inspector_placement(body) {
            let theme = TuiTheme::for_theme_id("terminal-native");
            let component_theme = bmux_tui_components::theme::ComponentTheme::default();
            let scroll = state.scroll_for(Some(&DisclosureTarget::Unit(7)));
            paint_panel(
                placement.area(),
                lines,
                scroll,
                &mut cx,
                theme,
                &component_theme,
            );
        }
        let hits = frame.hits().regions().to_vec();
        (buffer, hits)
    }

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

    #[test]
    fn panel_hit_region_matches_the_painted_area_and_shadows_the_transcript() {
        let theme = TuiTheme::for_theme_id("terminal-native");
        let lines = report_lines(Some(&report("src/lib.rs")), theme);
        let state = InspectorState::default();
        for (width, height) in [(0, 0), (1, 1), (2, 5), (3, 3), (8, 4), (40, 12), (120, 20)] {
            let area = Rect::new(0, 0, width, height);
            let (buffer, hits) = paint(area, area, &lines, &state);
            let panel = inspector_placement(area).map(InspectorPlacement::area);
            let inspector: Vec<&HitRegion> = hits
                .iter()
                .filter(|region| region.id.as_str() == INSPECTOR_HIT_ID)
                .collect();
            match panel {
                None => assert!(inspector.is_empty(), "{width}x{height}"),
                Some(panel) => {
                    assert_eq!(inspector.len(), 1);
                    assert_eq!(inspector[0].area, panel);
                    let map = bmux_tui::hit::HitMap::default()
                        .with_region(HitRegion::new("disclosure:unit:7", area))
                        .with_region(inspector[0].clone());
                    let inside = bmux_tui::geometry::Point::new(panel.x, panel.y);
                    assert_eq!(
                        map.hit_test(inside).map(|hit| hit.id().as_str().to_owned()),
                        Some(INSPECTOR_HIT_ID.to_owned())
                    );
                    // Border glyphs land inside the panel only.
                    if panel.x > 0 {
                        assert_eq!(
                            buffer
                                .get(bmux_tui::geometry::Point::new(0, panel.y))
                                .map(|cell| cell.symbol.clone()),
                            Some(" ".to_owned())
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn wide_graphemes_wrap_inside_the_panel_and_scrolling_clamps_to_content() {
        let theme = TuiTheme::for_theme_id("terminal-native");
        let target = "src/日本語/🚀👩‍💻/e\u{301}.rs".repeat(4);
        let lines = report_lines(Some(&report(&target)), theme);
        let state = InspectorState::default();
        let tall = Rect::new(0, 0, 30, 60);
        let (buffer, _) = paint(tall, tall, &lines, &state);
        let rows: Vec<String> = (0..tall.height)
            .filter_map(|row| buffer.row_symbols(row))
            .collect();
        let text = rows.join("\n");
        assert!(text.contains("Inspector"));
        assert!(text.contains("unit:7"));
        assert!(text.contains("日本"), "{text}");
        // The right border stays in the panel's last column on every row,
        // so wide graphemes never push it out of place.
        let panel = inspector_placement(tall).expect("panel").area();
        let right = bmux_tui::geometry::Point::new(panel.x + panel.width - 1, panel.y + 5);
        assert_eq!(
            buffer.get(right).map(|cell| cell.symbol.clone()),
            Some("│".to_owned())
        );
        let area = Rect::new(0, 0, 30, 10);
        // Scrolling far past the end clamps to the last page on paint.
        assert!(state.scroll_by(10_000));
        let (_, _) = paint(area, area, &lines, &state);
        let clamped = state
            .scroll_for(Some(&DisclosureTarget::Unit(7)))
            .get()
            .vertical_offset();
        assert!(clamped > 0 && clamped < 10_000, "{clamped}");
        let (scrolled, _) = paint(area, area, &lines, &state);
        let text = (0..area.height)
            .filter_map(|row| scrolled.row_symbols(row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("in group:3"), "{text}");
    }

    #[test]
    fn placeholder_explains_unstructured_focus() {
        let theme = TuiTheme::for_theme_id("terminal-native");
        let lines = report_lines(None, theme);
        assert_eq!(lines.len(), 2);
    }
}
