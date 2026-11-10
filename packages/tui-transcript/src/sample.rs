//! Standalone layout of a fixed item list through the standard producers (previews and
//! generated galleries).
//!
//! [`SampleLayout`] resolves structure for a small, complete document with an explicit
//! [`StructureConfig`], derives its units, lays every unit out exactly once at one width
//! through the standard [`ProducerRegistry`] with an explicit [`RenderConfig`] (L2, L4), and
//! paints the retained layouts stacked top to bottom. It is what the transcript engine renders
//! for the same items, minus the engine's viewport, navigation, focus, selection and window
//! boundary rows: a sample is not a resident window, so it has no boundary units, and it is
//! never interactive, so it registers no hit regions or focus targets of its own.
//!
//! The layout is pure in its inputs: equal items, configuration and width give equal rows.

use bcode_session_view_models::{TranscriptChanges, TranscriptViewItem};
use bmux_tui::component::LogicalSize;
use bmux_tui::paint::PaintCx;

use crate::layout::cache::{ItemLayoutCache, LayoutInputs, RetainedItem};
use crate::layout::context::{ActivityPhase, RenderConfig};
use crate::layout::producers::ProducerRegistry;
use crate::units::structure::{StructureConfig, StructureResolver};
use crate::units::{UnitKey, Units};
use crate::window::WindowMode;

/// One sample document laid out at one width: every non-boundary unit's retained layout, in
/// document order, with its top row.
pub struct SampleLayout {
    width: u16,
    height: u64,
    /// Unit keys in paint order with their top row in the sample.
    units: Vec<(UnitKey, u64)>,
    layouts: ItemLayoutCache,
}

impl std::fmt::Debug for SampleLayout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SampleLayout")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("units", &self.units)
            .finish_non_exhaustive()
    }
}

impl SampleLayout {
    /// Lay out `items` (a complete document, oldest first) at `width` under `structure` and
    /// `render`.
    ///
    /// Structure is resolved over `items` alone, so turn recency, `last_in_turn` and grouping
    /// apply to the sample exactly as they would to the same resident window. Paint-only state
    /// is at rest: no focus, no selection, spinner frame zero and no elapsed labels (L5).
    #[must_use]
    pub fn new(
        items: &[TranscriptViewItem],
        structure: StructureConfig,
        render: &RenderConfig,
        width: u16,
    ) -> Self {
        let mut resolver = StructureResolver::new(structure);
        let _ = resolver.update(&TranscriptChanges::default(), items);
        let mut units = Units::new();
        let _ = units.apply(
            &TranscriptChanges::default(),
            items,
            WindowMode::Live,
            &resolver,
        );
        let producer = ProducerRegistry::standard();
        let inputs = LayoutInputs {
            units: &units,
            items,
            structure: &resolver,
            render,
            producer: &producer,
        };
        let mut layouts = ItemLayoutCache::new();
        let mut placed = Vec::new();
        let mut top = 0_u64;
        for key in units.keys().iter().filter(|key| !key.is_boundary()) {
            let height = inputs.measure(&mut layouts, key, u64::from(width));
            placed.push((key.clone(), top));
            top = top.saturating_add(height);
        }
        Self {
            width,
            height: top,
            units: placed,
            layouts,
        }
    }

    /// Width the sample was laid out at, in cells.
    #[must_use]
    pub const fn width(&self) -> u16 {
        self.width
    }

    /// Total measured height in rows.
    #[must_use]
    pub const fn height(&self) -> u64 {
        self.height
    }

    /// Units in paint order with their top row.
    #[must_use]
    pub fn units(&self) -> &[(UnitKey, u64)] {
        &self.units
    }

    /// Paint the sample with its first row at the top of `cx`, clipped to `cx`'s area. Rows
    /// below the area are not painted; cells the producers leave unstyled keep the style
    /// already in the buffer.
    pub fn paint(&self, cx: &mut PaintCx<'_, '_>) {
        let area = cx.area();
        let bottom = u64::try_from(area.y.saturating_add(i64::from(area.height))).unwrap_or(0);
        for (key, top) in &self.units {
            if *top >= bottom {
                break;
            }
            let Some(layout) = self.layouts.current(key) else {
                continue;
            };
            let retained = RetainedItem::new(layout, ActivityPhase::default());
            let Ok(offset) = i64::try_from(*top) else {
                break;
            };
            cx.with_child_size(
                0,
                offset,
                LogicalSize::new(u64::from(self.width), layout.height),
                |cx| {
                    use bmux_tui::component::Component as _;
                    retained.paint(&layout.node, cx);
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use bcode_session_view_models::{
        ChatMessageView, ToolInvocationView, ToolInvocationViewStatus, ToolResultView,
        TranscriptViewItemId, TranscriptViewItemKind,
    };
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::Rect;
    use bmux_tui::text_width::display_width;

    use super::*;

    fn item(id: &str, kind: TranscriptViewItemKind) -> TranscriptViewItem {
        TranscriptViewItem {
            id: TranscriptViewItemId::new(id),
            revision: 1,
            sequence: None,
            timestamp_ms: None,
            output_location: None,
            streaming: false,
            kind,
        }
    }

    fn sample() -> Vec<TranscriptViewItem> {
        vec![
            item(
                "user",
                TranscriptViewItemKind::UserMessage {
                    message: ChatMessageView::markdown("Where is the 漢字 picker? 👋"),
                },
            ),
            item(
                "tool",
                TranscriptViewItemKind::ToolInvocation {
                    tool: Box::new(ToolInvocationView {
                        tool_call_id: "call".to_owned(),
                        producer_plugin_id: None,
                        tool_name: Some("read".to_owned()),
                        arguments_json: Some(r#"{"path":"src/picker.rs"}"#.to_owned()),
                        working_directory: None,
                        request_draft: None,
                        status: ToolInvocationViewStatus::Finished,
                        result_text: None,
                        is_error: Some(false),
                        result: Some(ToolResultView::Text {
                            text: "fn picker() {}".to_owned(),
                        }),
                        presentation: None,
                        timing: bcode_session_view_models::ToolTimingView::default(),
                    }),
                },
            ),
            item(
                "answer",
                TranscriptViewItemKind::AssistantMessage {
                    message: ChatMessageView::markdown("In `src/picker.rs`, e\u{301}."),
                },
            ),
        ]
    }

    fn paint(layout: &SampleLayout, height: u16) -> Vec<String> {
        let mut buffer = Buffer::empty(Rect::new(0, 0, layout.width(), height));
        let mut frame = Frame::new(&mut buffer);
        layout.paint(&mut PaintCx::new(&mut frame));
        (0..height)
            .map(|row| {
                frame
                    .buffer()
                    .row_symbols(row)
                    .unwrap_or_default()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn units_stack_in_document_order_without_boundary_rows() {
        let layout = SampleLayout::new(
            &sample(),
            StructureConfig::default(),
            &RenderConfig::default(),
            40,
        );
        let keys: Vec<_> = layout.units().iter().map(|(key, _)| key.clone()).collect();
        assert_eq!(
            keys,
            ["user", "tool", "answer"]
                .map(|id| UnitKey::Item(TranscriptViewItemId::new(id)))
                .to_vec()
        );
        let tops: Vec<_> = layout.units().iter().map(|(_, top)| *top).collect();
        assert!(tops.windows(2).all(|pair| pair[0] < pair[1]), "{tops:?}");
        assert!(layout.height() > *tops.last().unwrap());
        let rows = paint(&layout, u16::try_from(layout.height()).unwrap());
        let text = rows.join("\n");
        let user = text.find("漢字").expect("user message painted");
        let answer = text
            .find("src/picker.rs`")
            .or_else(|| text.rfind("src/picker.rs"));
        assert!(answer.is_some_and(|answer| answer > user), "{text}");
    }

    #[test]
    fn paint_is_bounded_at_every_size_and_deterministic() {
        for (width, height) in [(0, 0), (1, 1), (3, 2), (12, 5), (40, 30)] {
            let first = SampleLayout::new(
                &sample(),
                StructureConfig::default(),
                &RenderConfig::default(),
                width,
            );
            let second = SampleLayout::new(
                &sample(),
                StructureConfig::default(),
                &RenderConfig::default(),
                width,
            );
            let rows = paint(&first, height);
            assert_eq!(rows, paint(&second, height));
            assert_eq!(rows.len(), usize::from(height));
            for row in &rows {
                assert!(display_width(row) <= usize::from(width), "{width}: {row:?}");
            }
        }
    }

    #[test]
    fn clipped_paint_agrees_with_the_full_paint() {
        let layout = SampleLayout::new(
            &sample(),
            StructureConfig::default(),
            &RenderConfig::default(),
            30,
        );
        let full = paint(&layout, u16::try_from(layout.height()).unwrap());
        let clipped = paint(&layout, 3);
        assert_eq!(clipped, full[..3]);
    }
}
