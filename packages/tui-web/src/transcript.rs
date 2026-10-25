//! Retained native Markdown recipe, composed with BMUX source geometry.
use bcode_markdown_render::MarkdownRenderOptions;
use bcode_session_view_models::{TranscriptViewDocument, TranscriptViewItemKind};
use bcode_tui_components::transcript_markdown::{
    MarkdownBlockStyle, MarkdownBody, TranscriptMarkdown, append_markdown_block,
};
use bmux_tui::{
    component::{Component, Constraints, LayoutCx, LayoutId, LayoutNode, LogicalSize},
    paint::{LocalRect, PaintCx},
    style::Style,
    text::Line,
};
use std::{collections::BTreeMap, ops::Range};

pub struct Entry {
    pub accepted: Option<TranscriptMarkdown>,
    pub lines: Vec<Line>,
    pub start: usize,
    pub body_start: usize,
}

pub struct Transcript {
    pub entries: Vec<Entry>,
    pub rows: usize,
    width: Option<u16>,
    revision: Option<u64>,
}

impl Transcript {
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
            rows: 0,
            width: None,
            revision: None,
        }
    }

    /// Called on semantic/geometry acceptance, never from paint.
    pub fn prepare(&mut self, document: Option<&TranscriptViewDocument>, width: u16) {
        if self.width == Some(width) && self.revision == document.map(|d| d.revision) {
            return;
        }
        let old: BTreeMap<_, _> = std::mem::take(&mut self.entries)
            .into_iter()
            .filter_map(|entry| entry.accepted.map(|a| (a.content_id().to_owned(), a)))
            .collect();
        let same_width = self.width == Some(width);
        self.width = Some(width);
        self.revision = document.map(|d| d.revision);
        self.rows = 0;
        self.push_notice(
            "Browser BMUX · plugin/tool controls, images and history paging unavailable",
        );
        let Some(document) = document else {
            return;
        };
        let mut budget = 64 * 1024;
        for item in &document.items {
            let (title, message) = match &item.kind {
                TranscriptViewItemKind::UserMessage { message } => ("You", message),
                TranscriptViewItemKind::AssistantMessage { message } => ("Assistant", message),
                TranscriptViewItemKind::ReasoningMessage { message } => ("Reasoning", message),
                _ => {
                    self.push_notice(
                        "[semantic item retained; rich plugin/tool presentation unavailable]",
                    );
                    continue;
                }
            };
            // Never parse a truncated source as if it were the whole document.
            if message.text.len() > budget {
                self.push_notice(
                    "[browser projection budget reached; complete semantic source retained]",
                );
                break;
            }
            budget = budget.saturating_sub(message.text.len().max(1));
            let id = item.id.get().to_owned();
            let accepted = old
                .get(&id)
                .filter(|previous| {
                    same_width
                        && previous.revision() == item.revision
                        && previous.source() == message.text
                })
                .cloned()
                .unwrap_or_else(|| {
                    let mut options = MarkdownRenderOptions::new(width.saturating_sub(2));
                    options.document_id = Some(id.clone());
                    options.streaming = item.streaming;
                    TranscriptMarkdown::prepare(id, item.revision, message.text.clone(), &options)
                });
            let mut lines = Vec::new();
            let body_start = append_markdown_block(
                &mut lines,
                title,
                accepted.projection(),
                width,
                MarkdownBlockStyle {
                    heading: Style::new(),
                    muted: Style::new(),
                    prominent: title == "You",
                    body_indent: 2,
                },
            );
            let start = self.rows;
            self.rows = self.rows.saturating_add(lines.len());
            self.entries.push(Entry {
                accepted: Some(accepted),
                lines,
                start,
                body_start,
            });
        }
    }

    fn push_notice(&mut self, text: &str) {
        let block = bmux_tui::composition::TextBlock::new(text);
        let layout = block.layout(
            Constraints::for_width(u64::from(self.width.unwrap_or(0))),
            &mut LayoutCx::new(),
        );
        let lines: Vec<_> = block
            .projection(&layout)
            .into_iter()
            .map(|row| row.line)
            .collect();
        let start = self.rows;
        self.rows += lines.len();
        self.entries.push(Entry {
            accepted: None,
            lines,
            start,
            body_start: 0,
        });
    }

    pub fn anchor(&self, row: usize) -> Option<(String, usize)> {
        for entry in &self.entries {
            let Some(accepted) = entry.accepted.as_ref() else {
                continue;
            };
            let body = entry.start + entry.body_start;
            if row >= body && row < body + accepted.projection().lines.len() {
                let source = accepted
                    .projection()
                    .selection_provenance()
                    .iter()
                    .find(|unit| {
                        unit.rects
                            .iter()
                            .any(|rect| usize::from(rect.y) == row - body)
                    })?;
                return source
                    .source_ranges
                    .first()
                    .map(|range| (accepted.content_id().to_owned(), range.start));
            }
        }
        None
    }

    pub fn resolve_anchor(&self, anchor: &(String, usize)) -> Option<usize> {
        self.entries.iter().find_map(|entry| {
            let accepted = entry.accepted.as_ref()?;
            if accepted.content_id() != anchor.0 {
                return None;
            }
            accepted
                .projection()
                .selection_provenance()
                .iter()
                .find(|unit| {
                    unit.source_ranges
                        .iter()
                        .any(|range| range.contains(&anchor.1))
                })
                .and_then(|unit| unit.rects.first())
                .map(|rect| entry.start + entry.body_start + usize::from(rect.y))
        })
    }

    pub fn copy(&self, selection: &bmux_tui::selection::SelectionSnapshot) -> Option<String> {
        let mut output = String::new();
        for slice in &selection.slices {
            let (accepted, suffix) = self
                .entries
                .iter()
                .filter_map(|entry| entry.accepted.as_ref())
                .find_map(|a| {
                    slice
                        .content_id
                        .as_str()
                        .strip_prefix(a.content_id())
                        .map(|suffix| (a, suffix))
                })?;
            if accepted.revision() != slice.revision {
                return None;
            }
            let ranges: Vec<Range<usize>> = if let Some(suffix) = suffix.strip_prefix('.') {
                bcode_tui_components::markdown_selection::expand_code_selection_range(
                    accepted.source(),
                    accepted.projection().code_block_selections(),
                    suffix,
                    slice.source_range.clone(),
                )?
            } else {
                vec![slice.source_range.clone()]
            };
            for range in ranges {
                output.push_str(accepted.source().get(range)?);
            }
        }
        Some(output)
    }
}

pub struct TranscriptComponent<'a> {
    pub transcript: &'a Transcript,
    pub top: usize,
}
impl Component for TranscriptComponent<'_> {
    fn layout(&self, constraints: Constraints, _: &mut LayoutCx) -> LayoutNode {
        LayoutNode::leaf(
            LayoutId::new("bcode-web/transcript"),
            constraints.constrain(LogicalSize::new(
                constraints.max_width(),
                constraints.max_height().unwrap_or(0),
            )),
        )
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        let width = u16::try_from(layout.size.width).unwrap_or(u16::MAX);
        for entry in &self.transcript.entries {
            if entry.start >= self.top.saturating_add(usize::from(height))
                || entry.start + entry.lines.len() <= self.top
            {
                continue;
            }
            for (index, line) in entry.lines.iter().enumerate() {
                let row = entry.start + index;
                let is_body = entry.accepted.as_ref().is_some_and(|accepted| {
                    index >= entry.body_start
                        && index < entry.body_start + accepted.projection().lines.len()
                });
                if row >= self.top && row - self.top < usize::from(height) && !is_body {
                    cx.write_line(
                        LocalRect::new(
                            0,
                            i64::try_from(row - self.top).unwrap_or(i64::MAX),
                            width,
                            1,
                        ),
                        line,
                    );
                }
            }
            if let Some(accepted) = &entry.accepted {
                let body = entry.start + entry.body_start;
                let skip = self.top.saturating_sub(body);
                let y = body.saturating_sub(self.top);
                let available = usize::from(height)
                    .saturating_sub(y)
                    .min(accepted.projection().lines.len().saturating_sub(skip));
                if available == 0 {
                    continue;
                }
                let component = MarkdownBody::new(
                    format!("markdown/{}", accepted.content_id()),
                    "bcode-web/transcript-selection".into(),
                    accepted,
                    u64::try_from(entry.start).unwrap_or(u64::MAX),
                )
                .top_row(skip);
                let size = bmux_tui::geometry::Size::new(
                    width.saturating_sub(2),
                    u16::try_from(available).unwrap_or(height),
                );
                let measured = component.layout(Constraints::tight(size), &mut LayoutCx::new());
                cx.with_child(
                    2,
                    i64::try_from(y).unwrap_or(i64::MAX),
                    LocalRect::new(0, 0, size.width, size.height),
                    |cx| component.paint(&measured, cx),
                );
            }
        }
    }
}
