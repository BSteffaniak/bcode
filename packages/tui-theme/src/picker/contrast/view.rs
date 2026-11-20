//! The contrast rows as a BMUX component below a picker's body.
//!
//! [`ContrastRowsComponent`] lays its body out above the two rows and paints the rows from
//! [`ContrastControls`]: the contrast factor as a line gauge over the accepted range labelled
//! with its value, and the minimum-contrast choices with the current one marked. The rows
//! degrade explicitly: they drop their labels when the width cannot hold a label beside a
//! usable control, and they are omitted when the body would keep fewer than its minimum
//! height. Each painted row registers a hit region (its id from [`row_hit_id`]), and pointer
//! events over a row are reported through [`ContrastRowsComponent::pointer`]; layout, paint,
//! hit regions and pointer routing all use the same resolved layout.

use std::cell::Cell;
use std::hash::{Hash, Hasher};

use bmux_tui::component::{
    ChildLayout, Component, ComponentRevision, Constraints, Element, EventCx, LayoutCx, LayoutId,
    LayoutNode, LogicalSize,
};
use bmux_tui::composition::{Fill, Flex, HitArea, Row, SizeBox, TextBlock};
use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEventKind};
use bmux_tui::geometry::Point;
use bmux_tui::paint::PaintCx;
use bmux_tui::style::{Modifier, Style};
use bmux_tui::text::{Line, Span, Text};
use bmux_tui::text_width::display_width;
use bmux_tui_components::progress_bar::{
    ProgressBarComponent, ProgressBarPolicy, ProgressBarStyles, ProgressBarValue,
};

use super::{ContrastControls, ContrastRow, factor_position, minimum_choices};

/// Narrowest control that keeps its row label beside it.
const CONTROL_MIN_WIDTH: u64 = 12;
/// Gap between a row's label and its control.
const LABEL_GAP: u64 = 1;
/// Marker before the focused row's label (and blank space before the others).
const FOCUS_MARKER: &str = "› ";
const BLUR_MARKER: &str = "  ";
/// Rows the contrast rows take.
const ROWS: u64 = 2;

/// Styles of the contrast rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContrastRowsStyles {
    /// Labels of unfocused rows.
    pub label: Style,
    /// The focused row's label and slider.
    pub focused: Style,
    /// An unfocused slider and the current minimum of an unfocused row.
    pub value: Style,
    /// The slider's empty part and the other minimum choices.
    pub muted: Style,
    /// The current minimum while its row is focused.
    pub selected: Style,
    /// Row background.
    pub background: Style,
}

/// A pointer event over a painted contrast row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContrastPointer {
    /// A press: focus the row.
    Focus(ContrastRow),
    /// A wheel notch: step the row (`true` raises; scrolling up raises).
    Step(ContrastRow, bool),
}

/// The hit-region id of contrast `row` under a component with id `id`.
#[must_use]
pub fn row_hit_id(id: &str, row: ContrastRow) -> String {
    match row {
        ContrastRow::Factor => format!("{id}.contrast"),
        ContrastRow::Minimum => format!("{id}.minimum"),
    }
}

/// The contrast row whose hit region (under a component with id `id`) is `hit`.
#[must_use]
pub fn hit_row(id: &str, hit: &str) -> Option<ContrastRow> {
    ContrastRow::ALL
        .into_iter()
        .find(|row| row_hit_id(id, *row) == hit)
}

/// What a pointer event over a painted row means, if anything.
#[must_use]
pub const fn pointer_action(kind: MouseEventKind, row: ContrastRow) -> Option<ContrastPointer> {
    match kind {
        MouseEventKind::Down(MouseButton::Left) => Some(ContrastPointer::Focus(row)),
        MouseEventKind::ScrollUp => Some(ContrastPointer::Step(row, true)),
        MouseEventKind::ScrollDown => Some(ContrastPointer::Step(row, false)),
        _ => None,
    }
}

/// A picker body with the contrast rows below it.
pub struct ContrastRowsComponent<'a> {
    id: String,
    body: Element<'a>,
    controls: &'a ContrastControls,
    styles: ContrastRowsStyles,
    min_body_height: u64,
    pointer: Option<&'a Cell<Option<ContrastPointer>>>,
    factor_label: String,
    choices: Vec<String>,
}

impl<'a> ContrastRowsComponent<'a> {
    /// `body` with the rows of `controls` below it; `id` names the component and prefixes the
    /// rows' hit regions ([`row_hit_id`]).
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        body: impl Component + 'a,
        controls: &'a ContrastControls,
        styles: ContrastRowsStyles,
    ) -> Self {
        let shown = controls.shown();
        let mut choices: Vec<String> = minimum_choices().map(|choice| choice.to_string()).collect();
        if !minimum_choices().any(|choice| choice == shown.minimum) {
            // A configured floor between the presets is shown as it is.
            choices.push(shown.minimum.to_string());
        }
        Self {
            id: id.into(),
            body: Element::new(body),
            controls,
            styles,
            min_body_height: 1,
            pointer: None,
            factor_label: format!("{}×", shown.factor),
            choices,
        }
    }

    /// Keep at least `rows` rows for the body; with less room the contrast rows are omitted
    /// (the keys still adjust them).
    #[must_use]
    pub const fn min_body_height(mut self, rows: u64) -> Self {
        self.min_body_height = rows;
        self
    }

    /// Report pointer events over the painted rows into `pointer`.
    #[must_use]
    pub const fn pointer(mut self, pointer: &'a Cell<Option<ContrastPointer>>) -> Self {
        self.pointer = Some(pointer);
        self
    }

    /// Cells the widest label takes with its focus marker.
    fn label_width() -> u64 {
        ContrastRow::ALL
            .iter()
            .map(|row| display_width(FOCUS_MARKER) + display_width(row.label()))
            .max()
            .map_or(0, |width| u64::try_from(width).unwrap_or(u64::MAX))
    }

    /// Whether a row `width` cells wide keeps its label.
    fn labelled(width: u64) -> bool {
        width >= Self::label_width() + LABEL_GAP + CONTROL_MIN_WIDTH
    }

    fn row_element(&self, row: ContrastRow, labelled: bool) -> Element<'_> {
        let focused = self.controls.focus() == Some(row);
        let styles = self.styles;
        let control: Element<'_> = match row {
            ContrastRow::Factor => {
                let filled = if focused {
                    styles.focused
                } else {
                    styles.value
                };
                Element::new(
                    ProgressBarComponent::new(
                        format!("{}.gauge", self.id),
                        ProgressBarValue::determinate(
                            factor_position(self.controls.shown().factor),
                            100,
                        ),
                    )
                    .label(&self.factor_label)
                    .policy(ProgressBarPolicy::compact().line_gauge())
                    .styles(ProgressBarStyles {
                        filled,
                        empty: styles.muted,
                        label: filled,
                        complete: filled,
                        indeterminate: filled,
                        background: styles.background,
                    }),
                )
            }
            ContrastRow::Minimum => {
                let current = self.controls.shown().minimum.to_string();
                let mut spans = Vec::new();
                for (index, choice) in self.choices.iter().enumerate() {
                    if index > 0 {
                        spans.push(Span::styled(" ", styles.background));
                    }
                    let style = match (*choice == current, focused) {
                        (true, true) => styles.selected,
                        (true, false) => styles
                            .value
                            .add_modifier(Modifier::BOLD | Modifier::UNDERLINE),
                        (false, _) => styles.muted,
                    };
                    spans.push(Span::styled(format!(" {choice} "), style));
                }
                Element::new(TextBlock::new(Text::from_lines(vec![Line::from_spans(
                    spans,
                )])))
            }
        };
        let content: Element<'_> = if labelled {
            let (marker, style) = if focused {
                (FOCUS_MARKER, styles.focused.add_modifier(Modifier::BOLD))
            } else {
                (BLUR_MARKER, styles.label)
            };
            let label = TextBlock::new(Text::from_lines(vec![Line::from_spans(vec![
                Span::styled(format!("{marker}{}", row.label()), style),
            ])]));
            Element::new(
                Row::new()
                    .gap(LABEL_GAP)
                    .child(SizeBox::new(label).width(Self::label_width()))
                    .flex(Flex::from_element(1, control)),
            )
        } else {
            control
        };
        Element::new(HitArea::new(
            row_hit_id(&self.id, row),
            Fill::from_element(styles.background, content),
        ))
    }

    const fn row_constraints(width: u64) -> Constraints {
        Constraints::new(width, width, 1, Some(1))
    }
}

impl Component for ContrastRowsComponent<'_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.hash(&mut layout);
        self.min_body_height.hash(&mut layout);
        let mut paint = std::collections::hash_map::DefaultHasher::new();
        self.controls.focus().hash(&mut paint);
        self.factor_label.hash(&mut paint);
        self.controls.shown().minimum.to_string().hash(&mut paint);
        self.choices.hash(&mut paint);
        self.styles.hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish()).combine(self.body.revision())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let width = constraints.max_width();
        let shown = width > 0
            && constraints
                .max_height()
                .is_none_or(|height| height >= ROWS.saturating_add(self.min_body_height));
        let rows = if shown { ROWS } else { 0 };
        let body = self.body.layout(
            Constraints::new(
                constraints.min_width(),
                width,
                constraints.min_height().saturating_sub(rows),
                constraints
                    .max_height()
                    .map(|height| height.saturating_sub(rows)),
            ),
            cx,
        );
        if !shown {
            let size = constraints.constrain(body.size);
            return LayoutNode::with_children(
                LayoutId::new(self.id.clone()),
                size,
                vec![ChildLayout::new(0, 0, body)],
            );
        }
        let top = body.size.height;
        let labelled = Self::labelled(width);
        let mut children = vec![ChildLayout::new(0, 0, body)];
        for (index, row) in (0u64..).zip(ContrastRow::ALL) {
            let node = self
                .row_element(row, labelled)
                .layout(Self::row_constraints(width), cx);
            children.push(ChildLayout::new(0, top + index, node));
        }
        let size = constraints.constrain(LogicalSize::new(width, top + ROWS));
        LayoutNode::with_children(LayoutId::new(self.id.clone()), size, children)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let mut children = layout.children.iter();
        if let Some(body) = children.next() {
            cx.with_child_size(
                i32::try_from(body.x).unwrap_or(i32::MAX),
                i64::try_from(body.y).unwrap_or(i64::MAX),
                body.node.size,
                |cx| self.body.paint(&body.node, cx),
            );
        }
        let labelled = Self::labelled(layout.size.width);
        for (child, row) in children.zip(ContrastRow::ALL) {
            let element = self.row_element(row, labelled);
            cx.with_child_size(
                i32::try_from(child.x).unwrap_or(i32::MAX),
                i64::try_from(child.y).unwrap_or(i64::MAX),
                child.node.size,
                |cx| element.paint(&child.node, cx),
            );
        }
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        if let Event::Mouse(mouse) = event {
            let point = Point::new(mouse.position.x, mouse.position.y);
            let over = ContrastRow::ALL.into_iter().find(|row| {
                cx.find_rect(&LayoutId::new(row_hit_id(&self.id, *row)))
                    .is_some_and(|area| area.contains(point))
            });
            if let Some(row) = over {
                return match (pointer_action(mouse.kind, row), self.pointer) {
                    (Some(action), Some(pointer)) => {
                        pointer.set(Some(action));
                        EventOutcome::Redraw
                    }
                    _ => EventOutcome::Ignored,
                };
            }
        }
        let Some(body) = layout.children.first() else {
            return EventOutcome::Ignored;
        };
        cx.with_child(body, |cx| self.body.event(event, &body.node, cx))
    }
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::Rect;
    use bmux_tui::hit::HitMap;
    use bmux_tui::paint::LocalRect;

    use super::*;
    use crate::adjustment::ContrastFactor;

    fn styles() -> ContrastRowsStyles {
        ContrastRowsStyles {
            label: Style::new(),
            focused: Style::new().add_modifier(Modifier::BOLD),
            value: Style::new(),
            muted: Style::new().add_modifier(Modifier::DIM),
            selected: Style::new().add_modifier(Modifier::REVERSED),
            background: Style::new(),
        }
    }

    /// Paint over `area`: the painted rows as text and the registered hit regions.
    fn paint(controls: &ContrastControls, area: Rect) -> (Vec<String>, HitMap, LayoutNode) {
        let body = TextBlock::new(Text::from_lines(vec![Line::from("body")]));
        let component =
            ContrastRowsComponent::new("picker", body, controls, styles()).min_body_height(3);
        let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(area);
        let hits = {
            let mut frame = Frame::new(&mut buffer);
            PaintCx::new(&mut frame).with_child(
                0,
                0,
                LocalRect::new(0, 0, area.width, area.height),
                |cx| component.paint(&layout, cx),
            );
            frame.hits().clone()
        };
        let rows = (0..area.height)
            .map(|y| buffer.row_symbols(y).unwrap_or_default())
            .collect();
        (rows, hits, layout)
    }

    #[test]
    fn rows_paint_their_values_and_degrade_with_the_area() {
        let mut controls = ContrastControls::default();
        controls.show(crate::adjustment::ContrastSettings {
            factor: ContrastFactor::new(1.4).expect("factor"),
            ..crate::adjustment::ContrastSettings::EXACT
        });
        controls.set_focus(Some(ContrastRow::Minimum));
        let (rows, hits, _) = paint(&controls, Rect::new(0, 0, 50, 6));
        assert!(
            rows[4].contains("Contrast") && rows[4].contains("1.40×"),
            "{rows:?}"
        );
        assert!(rows[5].starts_with("› Minimum"), "{rows:?}");
        assert!(rows[5].contains("off   3.0   4.5   7.0"), "{rows:?}");
        for (y, row) in [(4, ContrastRow::Factor), (5, ContrastRow::Minimum)] {
            for x in [0, 49] {
                let hit = hits.hit_test(Point::new(x, y)).expect("hit");
                assert_eq!(hit_row("picker", hit.region.id.as_str()), Some(row));
            }
        }

        // Narrow: the labels go, the controls stay inside the area.
        let (rows, hits, _) = paint(&controls, Rect::new(0, 0, 14, 6));
        assert!(
            !rows[4].contains("Contrast") && rows[4].contains("1.40×"),
            "{rows:?}"
        );
        assert!(rows[5].starts_with(" off "), "{rows:?}");
        assert!(hits.hit_test(Point::new(13, 5)).is_some());

        // Short: the body keeps its rows and the contrast rows are omitted, without hits.
        let (rows, hits, layout) = paint(&controls, Rect::new(0, 0, 50, 4));
        assert_eq!(layout.children.len(), 1);
        assert!(rows.iter().all(|row| !row.contains("Minimum")), "{rows:?}");
        assert!((0..4).all(|y| hits.hit_test(Point::new(0, y)).is_none()));

        // Nothing at all.
        let (_, hits, _) = paint(&controls, Rect::new(0, 0, 0, 0));
        assert!(hits.hit_test(Point::new(0, 0)).is_none());
    }
}
