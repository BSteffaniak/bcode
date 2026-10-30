//! Differential fixtures for shared acceptance and clipped retained painting.

use super::*;
use bcode_session_view_models::{
    ChatMessageView, TextFormat, TranscriptViewItem, TranscriptViewItemKind,
};
use bmux_tui::{
    buffer::Buffer,
    component::{Component, Constraints, LayoutCx},
    frame::Frame,
    geometry::Rect,
    paint::PaintCx,
};

pub(super) fn document(format: TextFormat, text: &str) -> TranscriptViewDocument {
    let mut snapshot = bcode_session_view_models::SessionViewSnapshot::empty();
    snapshot.transcript.revision = 1;
    snapshot.transcript.items.push(TranscriptViewItem {
        id: TranscriptViewItemId::new("fixture"),
        revision: 1,
        output_location: None,
        sequence: Some(1),
        timestamp_ms: Some(1),
        streaming: false,
        kind: TranscriptViewItemKind::AssistantMessage {
            message: ChatMessageView {
                text: text.to_owned(),
                display_label: None,
                activity: None,
                submission_status: None,
                submission_context: None,
                format,
            },
        },
    });
    snapshot.transcript
}

#[test]
fn semantic_tool_fixtures_remain_readable_without_an_adapter() {
    let fixtures: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../session-view/models/fixtures/renderer-tool-presentations.json"
    ))
    .unwrap();
    for fixture in fixtures.as_array().unwrap() {
        let mut source = bcode_session_view_models::SessionViewSnapshot::empty().transcript;
        source.revision = 1;
        source
            .items
            .push(serde_json::from_value(fixture["item"].clone()).unwrap());
        let mut accepted = AcceptedTranscript::default();
        accepted.prepare(&source, 80);
        let mut text = String::new();
        for row in accepted.layout.visible_lines_from_top(0, u16::MAX) {
            if let Some(line) = accepted.layout.line(row) {
                text.push_str(&line.plain_text());
                text.push('\n');
            }
        }
        for forbidden in fixture["forbidden"].as_array().into_iter().flatten() {
            let forbidden = forbidden.as_str().unwrap();
            assert!(
                !text.contains(forbidden),
                "{} exposed opaque data {forbidden:?}",
                fixture["name"]
            );
        }
        for expected in fixture["expected"].as_array().unwrap() {
            let expected = expected.as_str().unwrap();
            assert!(
                text.contains(expected),
                "{} fallback missing {expected:?}: {text}",
                fixture["name"]
            );
        }
    }
}

#[test]
fn shared_dispatch_and_native_row_bundle_paint_identically_at_tiny_and_unicode_widths() {
    for width in [0, 1, 4, 22, 80] {
        for format in [TextFormat::PlainText, TextFormat::Markdown] {
            let mut accepted = AcceptedTranscript::default();
            accepted.prepare(
                &document(format, "你好 👩‍💻 e\u{301}\n\n[link](https://example.com)"),
                width,
            );
            let item = accepted.document.get(0).expect("accepted semantic item");
            let expected = semantic_rows(item, width, &accepted.markdown);
            let (TranscriptLayoutRows::Rendered(rows)
            | TranscriptLayoutRows::Selected { rows, .. }
            | TranscriptLayoutRows::Markdown { rows, .. }) = expected
            else {
                unreachable!()
            };
            let area = Rect::new(0, 0, width, 6);
            let child = super::super::child::TranscriptChild {
                id: "fixture",
                layout: &accepted.layout,
                items: accepted.document.items(),
                top: 0,
                selection_scope: "fixture-selection",
                metadata: true,
                decorate: None,
            };
            let measured = child.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
            let mut actual = Buffer::empty(area);
            let mut frame = Frame::new(&mut actual);
            child.paint(&measured, &mut PaintCx::new(&mut frame));
            for hit in frame.hits().regions() {
                assert_eq!(hit.area.intersection(area), hit.area);
            }
            for fragment in frame.selection().fragments() {
                assert_eq!(fragment.area.intersection(area), fragment.area);
            }
            drop(frame);
            let mut expected = Buffer::empty(area);
            let mut expected_frame = Frame::new(&mut expected);
            for (y, line) in rows.iter().take(6).enumerate() {
                PaintCx::new(&mut expected_frame).write_line(
                    bmux_tui::paint::LocalRect::new(0, i64::try_from(y).unwrap(), width, 1),
                    line,
                );
            }
            drop(expected_frame);
            assert_eq!(actual, expected);
        }
    }
}

#[test]
fn rich_bundle_is_generation_checked_and_replaced_atomically() {
    let source = document(TextFormat::PlainText, "fallback");
    let mut accepted = AcceptedTranscript::default();
    accepted.prepare(&source, 20);
    let bundle = |item_revision, revision, text: &str| AcceptedVisual {
        item_revision,
        width: 20,
        revision,
        rows: TranscriptLayoutRows::Rendered(vec![Line::raw(text)]),
    };
    assert!(!accepted.accept_visual(TranscriptViewItemId::new("fixture"), bundle(0, 1, "stale")));
    assert!(accepted.accept_visual(
        TranscriptViewItemId::new("fixture"),
        bundle(1, 2, "rich snapshot")
    ));
    assert!(!accepted.accept_visual(TranscriptViewItemId::new("fixture"), bundle(1, 1, "older")));
    accepted.prepare(&source, 20);
    let visible = accepted.layout.line_at_row(0).unwrap();
    assert_eq!(
        accepted.layout.line(visible).unwrap().plain_text(),
        "rich snapshot"
    );
    accepted.prepare(&source, 10);
    assert_ne!(
        accepted
            .layout
            .line(accepted.layout.line_at_row(0).unwrap())
            .unwrap()
            .plain_text(),
        "rich snapshot"
    );
}

#[test]
fn oversized_resident_window_is_rejected_before_projection_not_truncated() {
    let source = document(TextFormat::Markdown, &"x".repeat(300 * 1024));
    let mut accepted = AcceptedTranscript::default();
    accepted.prepare(&source, 40);
    assert!(accepted.window_rejected);
    assert_eq!(accepted.markdown.render_count(), 0);
    assert!(
        accepted
            .document
            .get(0)
            .unwrap()
            .text()
            .contains("smaller history page")
    );
}

#[test]
fn producer_copy_uses_accepted_source_revision_and_survives_semantic_reorder() {
    let mut source = document(TextFormat::PlainText, "not producer source");
    let mut accepted = AcceptedTranscript::default();
    accepted.prepare(&source, 20);
    let row = bcode_plugin_models::tui_selection::PluginTuiSelectionRow {
        identity: "plugin/source".into(),
        byte_start: 0,
        text: "actual source".into(),
        cells: vec![],
        revision: 7,
    };
    assert!(accepted.accept_visual(
        TranscriptViewItemId::new("fixture"),
        AcceptedVisual {
            item_revision: 1,
            width: 20,
            revision: 7,
            rows: TranscriptLayoutRows::Selected {
                rows: vec![Line::raw("display")],
                anchors: vec![],
                selection: BTreeMap::from([(0, row)])
            },
        }
    ));
    assert_eq!(
        accepted
            .producer_source("plugin/source", 7, 0..6)
            .as_deref(),
        Some("actual")
    );
    assert!(accepted.producer_source("plugin/source", 6, 0..6).is_none());
    let mut other = source.items[0].clone();
    other.id = TranscriptViewItemId::new("other");
    source.items.insert(0, other);
    source.revision += 1;
    accepted.prepare(&source, 20);
    assert_eq!(
        accepted
            .producer_source("plugin/source", 7, 0..6)
            .as_deref(),
        Some("actual")
    );
}

#[test]
fn retained_large_markdown_selection_is_indexed_per_visible_row() {
    let mut accepted = AcceptedTranscript::default();
    accepted.prepare(
        &document(TextFormat::Markdown, &"hello world\n\n".repeat(2000)),
        30,
    );
    let row = accepted.layout.line_at_row(2000).unwrap();
    let fragments = accepted
        .layout
        .selection_fragments(row.entry_index, row.row_in_entry);
    assert!(fragments.len() <= 20);
    for fragment in fragments {
        assert!(fragment.source_range.end <= 26000);
    }
}
