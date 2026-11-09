//! Minimal phase 2 unit producers.
//!
//! A producer turns one unit (and its transcript item, when it has one) into a BMUX component.
//! Heights are always measured by laying that component out with BMUX at the exact width, so
//! measurement and painting share one layout definition (B3) and no text is measured by byte
//! or character count (B2).
//!
//! Phase 2 producers render plain text rows, which is sufficient for exact measurement and for
//! headless navigation tests. Identity-keyed item layout with `ItemRenderContext`, Markdown,
//! tools, diffs and chrome replaces them in phase 3 (`layout::producers`).

use bcode_session_view_models::{TranscriptViewItem, TranscriptViewItemKind};
use bmux_tui::component::{Component, Constraints, Element, LayoutCx};
use bmux_tui::composition::TextBlock;
use bmux_tui::text::{Line, Text, TextWrap};

use super::{BoundaryEdge, UnitEntry, UnitKind};
use crate::window::BoundaryStatus;

/// Everything a producer may read for one unit.
#[derive(Debug, Clone, Copy)]
pub struct UnitContent<'a> {
    /// The derived unit.
    pub unit: &'a UnitEntry,
    /// The unit's transcript item (the first member for groups and markers).
    pub item: Option<&'a TranscriptViewItem>,
    /// Boundary state for boundary units. Paint-only: it never changes the height (U3).
    pub boundary: Option<BoundaryStatus>,
}

/// Builds the component for one unit.
///
/// Any BMUX component may represent a unit (L4); it may borrow the unit's content.
pub trait UnitProducer {
    /// Component for one unit. The same component is used for measurement and painting.
    fn component<'a>(&self, content: UnitContent<'a>) -> Element<'a>;

    /// Exact height of the unit at `width`, measured by laying out [`Self::component`].
    fn measure(&self, content: UnitContent<'_>, width: u64) -> u64 {
        measure_component(&self.component(content), width)
    }
}

/// Lay a component out at `width` and return its exact height.
#[must_use]
pub fn measure_component(component: &dyn Component, width: u64) -> u64 {
    component
        .layout(Constraints::for_width(width), &mut LayoutCx::new())
        .size
        .height
}

/// Boundary row text (U3, W13).
#[must_use]
pub const fn boundary_text(edge: BoundaryEdge, status: BoundaryStatus) -> &'static str {
    match (edge, status) {
        (_, BoundaryStatus::Loading) => "loading…",
        (BoundaryEdge::Older, BoundaryStatus::MoreAvailable) => "earlier messages available",
        (BoundaryEdge::Older, BoundaryStatus::Exhausted) => "start of session",
        (BoundaryEdge::Newer, _) => "newer messages available",
    }
}

/// Boundary rows never wrap, so their height is exactly one row at every width and in every
/// state (U3).
#[must_use]
pub fn boundary_component(edge: BoundaryEdge, status: BoundaryStatus) -> TextBlock {
    TextBlock::new(Text::from_lines(vec![Line::raw(boundary_text(
        edge, status,
    ))]))
    .wrap(TextWrap::None)
}

/// Phase 2 producer: plain text rows, word-wrapped by BMUX.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlainTextProducer;

impl UnitProducer for PlainTextProducer {
    fn component<'a>(&self, content: UnitContent<'a>) -> Element<'a> {
        Element::new(match content.unit.kind {
            UnitKind::Boundary(edge) => boundary_component(
                edge,
                content.boundary.unwrap_or(BoundaryStatus::MoreAvailable),
            ),
            UnitKind::GroupHeader { members, expanded } => {
                let marker = if expanded { "▾" } else { "▸" };
                let label = content.item.map_or("items", item_label);
                single_row(&format!("{marker} {members} {label}"))
            }
            UnitKind::HiddenMarker { count } => single_row(&format!("▸ {count} hidden")),
            UnitKind::Item => {
                let lines = content.item.map_or_else(Vec::new, item_lines);
                TextBlock::new(Text::from_lines(lines)).wrap(TextWrap::Word)
            }
        })
    }
}

fn single_row(text: &str) -> TextBlock {
    TextBlock::new(Text::from_lines(vec![Line::raw(text.to_owned())])).wrap(TextWrap::None)
}

/// Short semantic label of an item kind.
const fn item_label(item: &TranscriptViewItem) -> &'static str {
    match &item.kind {
        TranscriptViewItemKind::UserMessage { .. } => "user",
        TranscriptViewItemKind::AssistantMessage { .. } => "assistant",
        TranscriptViewItemKind::ReasoningMessage { .. }
        | TranscriptViewItemKind::ReasoningActivity { .. } => "reasoning",
        TranscriptViewItemKind::ToolInvocation { .. }
        | TranscriptViewItemKind::ToolRequestDraft { .. }
        | TranscriptViewItemKind::ToolRequest { .. }
        | TranscriptViewItemKind::ToolContribution { .. } => "tool",
        TranscriptViewItemKind::Permission { .. } => "permission",
        TranscriptViewItemKind::Compaction { .. } => "compaction",
        TranscriptViewItemKind::Interaction { .. } => "interaction",
        TranscriptViewItemKind::Skill { .. } => "skill",
        TranscriptViewItemKind::SystemMessage { .. } => "system",
    }
}

/// Plain text rows of an item: message text split into source lines (a Markdown hard-break
/// backslash at a line end is dropped), or a one-row kind label for other items.
fn item_lines(item: &TranscriptViewItem) -> Vec<Line> {
    let text = match &item.kind {
        TranscriptViewItemKind::UserMessage { message }
        | TranscriptViewItemKind::AssistantMessage { message }
        | TranscriptViewItemKind::ReasoningMessage { message }
        | TranscriptViewItemKind::SystemMessage { message } => Some(message.text.as_str()),
        _ => None,
    };
    text.map_or_else(
        || vec![Line::raw(format!("[{}]", item_label(item)))],
        |text| {
            text.split('\n')
                .map(|line| Line::raw(line.strip_suffix('\\').unwrap_or(line).to_owned()))
                .collect()
        },
    )
}

/// Deterministic fixture producer for headless tests: every unit renders exactly the
/// configured number of unwrapped rows at every width.
#[cfg(any(test, feature = "testing"))]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FixtureProducer {
    /// Height per unit key; units not listed use `default_height`.
    pub heights: std::collections::BTreeMap<super::UnitKey, u64>,
    /// Height of unlisted non-boundary units.
    pub default_height: u64,
}

#[cfg(any(test, feature = "testing"))]
impl FixtureProducer {
    /// A fixture where every non-boundary unit has `default_height` rows.
    #[must_use]
    pub const fn uniform(default_height: u64) -> Self {
        Self {
            heights: std::collections::BTreeMap::new(),
            default_height,
        }
    }

    /// Return the fixture with one unit's height overridden.
    #[must_use]
    pub fn with_height(mut self, key: super::UnitKey, height: u64) -> Self {
        self.heights.insert(key, height);
        self
    }
}

#[cfg(any(test, feature = "testing"))]
impl UnitProducer for FixtureProducer {
    fn component<'a>(&self, content: UnitContent<'a>) -> Element<'a> {
        if let UnitKind::Boundary(edge) = content.unit.kind {
            return Element::new(boundary_component(
                edge,
                content.boundary.unwrap_or(BoundaryStatus::MoreAvailable),
            ));
        }
        let height = self
            .heights
            .get(&content.unit.key)
            .copied()
            .unwrap_or(self.default_height);
        let lines = (0..height)
            .map(|row| Line::raw(format!("{} row {row}", content.unit.key)))
            .collect::<Vec<_>>();
        Element::new(TextBlock::new(Text::from_lines(lines)).wrap(TextWrap::None))
    }
}

#[cfg(test)]
mod tests {
    use bcode_session_view_models::{ChatMessageView, TranscriptViewItemId};

    use super::*;
    use crate::units::{UnitKey, Units};
    use crate::window::WindowMode;

    fn units_for(items: &[TranscriptViewItem]) -> Units {
        let mut units = Units::new();
        let _ = units.apply(
            &bcode_session_view_models::TranscriptChanges::default(),
            items,
            WindowMode::Detached,
            &crate::units::structure::FlatStructure,
        );
        units
    }

    fn assistant(id: &str, text: &str) -> TranscriptViewItem {
        TranscriptViewItem {
            id: TranscriptViewItemId::new(id),
            revision: 1,
            sequence: Some(1),
            timestamp_ms: None,
            output_location: None,
            streaming: false,
            kind: TranscriptViewItemKind::AssistantMessage {
                message: ChatMessageView::markdown(text),
            },
        }
    }

    #[test]
    fn u3_boundary_rows_are_one_row_in_every_state_and_width() {
        let units = units_for(&[]);
        for edge in [BoundaryEdge::Older, BoundaryEdge::Newer] {
            let unit = units.entry(&UnitKey::Boundary(edge)).expect("boundary");
            for status in [
                BoundaryStatus::MoreAvailable,
                BoundaryStatus::Loading,
                BoundaryStatus::Exhausted,
            ] {
                for width in [0, 1, 3, 80] {
                    let content = UnitContent {
                        unit,
                        item: None,
                        boundary: Some(status),
                    };
                    assert_eq!(PlainTextProducer.measure(content, width), 1);
                }
            }
        }
    }

    #[test]
    fn b2_item_height_is_measured_in_cells_by_bmux() {
        // Wide CJK graphemes take two cells each: 4 graphemes need 8 cells.
        let item = assistant("a", "漢字漢字");
        let units = units_for(std::slice::from_ref(&item));
        let unit = units
            .entry(&UnitKey::Item(item.id.clone()))
            .expect("item unit");
        let content = UnitContent {
            unit,
            item: Some(&item),
            boundary: None,
        };
        assert_eq!(PlainTextProducer.measure(content, 8), 1);
        assert_eq!(PlainTextProducer.measure(content, 4), 2);
    }

    #[test]
    fn hard_breaks_become_rows() {
        let item = assistant("a", "one\\\ntwo\\\nthree");
        let units = units_for(std::slice::from_ref(&item));
        let unit = units.entry(&UnitKey::Item(item.id.clone())).expect("unit");
        let content = UnitContent {
            unit,
            item: Some(&item),
            boundary: None,
        };
        assert_eq!(PlainTextProducer.measure(content, 40), 3);
    }

    #[test]
    fn fixture_heights_are_exact_and_width_independent() {
        let item = assistant("a", "ignored");
        let units = units_for(std::slice::from_ref(&item));
        let key = UnitKey::Item(item.id.clone());
        let unit = units.entry(&key).expect("unit");
        let producer = FixtureProducer::uniform(2).with_height(key, 5);
        let content = UnitContent {
            unit,
            item: Some(&item),
            boundary: None,
        };
        assert_eq!(producer.measure(content, 1), 5);
        assert_eq!(producer.measure(content, 200), 5);
    }
}
