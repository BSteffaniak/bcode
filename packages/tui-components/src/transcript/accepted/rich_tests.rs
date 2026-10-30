//! Atomic rich image/source lifetime and clipped composition regressions.
use super::tests::document;
use super::*;
use bmux_tui::image::{
    ImageFit, ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat, ImagePlacement,
};
use bmux_tui::{
    buffer::Buffer,
    component::{Component, Constraints, LayoutCx},
    frame::Frame,
    geometry::Rect,
    paint::PaintCx,
};

fn visual(revision: u64) -> AcceptedVisual {
    AcceptedVisual {
        item_revision: revision,
        width: 20,
        revision: 1,
        rows: TranscriptLayoutRows::Rendered(vec![Line::raw(""); 6]),
    }
}
fn image() -> ImagePlacement {
    ImagePlacement {
        key: ImageKey::new("image"),
        payload: ImagePayload::Pixels {
            bytes: vec![255; 16],
            width: 2,
            height: 2,
            format: ImagePixelFormat::Rgba8,
        },
        destination: Rect::new(1, 1, 8, 4),
        fit: ImageFit::Contain,
        allocation: None,
        clip: Rect::new(1, 1, 8, 4),
        lifecycle: ImageLifecycle::Frame,
    }
}

#[test]
fn rich_images_follow_item_revision_width_removal_and_viewport_clipping() {
    let mut source = document(TextFormat::PlainText, "fallback");
    let mut accepted = AcceptedTranscript::default();
    accepted.prepare(&source, 20);
    assert!(accepted.accept_rich_visual(
        TranscriptViewItemId::new("fixture"),
        visual(1),
        vec![image()]
    ));
    accepted.prepare(&source, 20);
    for top in [0, 2, 5, 100] {
        let area = Rect::new(0, 0, 20, 2);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        let child = accepted.child("transcript", top, "selection");
        let layout = child.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
        child.paint(&layout, &mut PaintCx::new(&mut frame));
        if top < 5 {
            assert_eq!(frame.images().len(), 1);
            let bmux_tui::image::ImageContribution::Present(image) = &frame.images()[0] else {
                panic!("present image")
            };
            assert_eq!(image.clip.intersection(area), image.clip);
            assert_eq!(image.destination.intersection(area), image.destination);
            assert_eq!(image.allocation.unwrap().height, 4);
        } else {
            assert!(frame.images().is_empty());
        }
    }
    source.revision += 1;
    source.items[0].revision += 1;
    accepted.prepare(&source, 20);
    assert!(accepted.images.is_empty());
    assert!(!accepted.accept_rich_visual(
        TranscriptViewItemId::new("fixture"),
        visual(1),
        vec![image()]
    ));
    assert!(accepted.accept_rich_visual(
        TranscriptViewItemId::new("fixture"),
        visual(2),
        vec![image()]
    ));
    accepted.prepare(&source, 10);
    assert!(accepted.images.is_empty());
    assert!(!accepted.accept_rich_visual(
        TranscriptViewItemId::new("fixture"),
        visual(2),
        vec![image()]
    ));
    accepted.prepare(&source, 20);
    assert!(accepted.accept_rich_visual(
        TranscriptViewItemId::new("fixture"),
        visual(2),
        vec![image()]
    ));
    assert!(accepted.remove_visual(&TranscriptViewItemId::new("fixture")));
    assert!(accepted.images.is_empty());
}

#[test]
fn plain_projection_preserves_unicode_and_crlf_source_offsets() {
    let text = "你好 e\u{301} 👩‍💻 abcdef\r\nsecond\r\n\r\nend";
    for width in [0, 1, 4, 20] {
        let mut accepted = AcceptedTranscript::default();
        accepted.prepare(&document(TextFormat::PlainText, text), width);
        let item = accepted.document.get(0).unwrap();
        let rows = semantic_rows(item, width, &accepted.markdown);
        if width == 0 {
            assert!(rows.is_empty());
            continue;
        }
        let TranscriptLayoutRows::Selected { selection, .. } = rows else {
            panic!("source-mapped plain rows")
        };
        for source in selection.values() {
            assert!(source.is_valid());
            for cell in &source.cells {
                let expected = text.get(cell.bytes.clone()).unwrap();
                let local =
                    cell.bytes.start - source.byte_start..cell.bytes.end - source.byte_start;
                assert_eq!(Some(expected), source.text.get(local));
                assert!(cell.column + cell.width <= width);
            }
        }
        if width == 20 {
            let second = selection
                .values()
                .find(|source| source.text == "second")
                .unwrap();
            assert_eq!(second.byte_start, text.find("second").unwrap());
        }
    }
}

#[test]
fn revision_zero_visual_removal_restores_semantic_rows() {
    let source = document(TextFormat::PlainText, "fallback");
    let mut accepted = AcceptedTranscript::default();
    accepted.prepare(&source, 20);
    assert!(accepted.accept_visual(
        TranscriptViewItemId::new("fixture"),
        AcceptedVisual {
            revision: 0,
            rows: TranscriptLayoutRows::Rendered(vec![Line::raw("visual")]),
            ..visual(1)
        }
    ));
    accepted.prepare(&source, 20);
    assert_eq!(
        accepted
            .layout
            .line(accepted.layout.line_at_row(0).unwrap())
            .unwrap()
            .plain_text(),
        "visual"
    );
    assert!(accepted.remove_visual(&TranscriptViewItemId::new("fixture")));
    accepted.prepare(&source, 20);
    assert_ne!(
        accepted
            .layout
            .line(accepted.layout.line_at_row(0).unwrap())
            .unwrap()
            .plain_text(),
        "visual"
    );
}

#[test]
fn theme_reload_changes_rows_without_changing_item_identity() {
    let source = document(TextFormat::Markdown, "**styled**");
    let mut accepted = AcceptedTranscript::default();
    accepted.prepare(&source, 20);
    let id = accepted.document.get(0).unwrap().id();
    let before = accepted
        .layout
        .line(accepted.layout.line_at_row(0).unwrap())
        .unwrap()
        .clone();
    accepted.set_presentation(TranscriptPresentation {
        heading: Style::new().fg(bmux_tui::style::Color::Red),
        ..Default::default()
    });
    let after = accepted
        .layout
        .line(accepted.layout.line_at_row(0).unwrap())
        .unwrap();
    assert_eq!(before.plain_text(), after.plain_text());
    assert_ne!(&before, after);
    assert_eq!(accepted.document.get(0).unwrap().id(), id);
}

#[test]
fn producer_sources_are_unavailable_after_semantic_revision_or_width_change() {
    let mut source = document(TextFormat::PlainText, "fallback");
    let mut accepted = AcceptedTranscript::default();
    accepted.prepare(&source, 20);
    let bundle = || AcceptedVisual {
        rows: TranscriptLayoutRows::Selected {
            rows: vec![Line::raw("display")],
            anchors: vec![],
            selection: BTreeMap::from([(
                0,
                bcode_plugin_models::tui_selection::PluginTuiSelectionRow {
                    identity: "producer".into(),
                    byte_start: 0,
                    text: "source".into(),
                    cells: vec![],
                    revision: 8,
                },
            )]),
        },
        ..visual(1)
    };
    assert!(accepted.accept_visual(TranscriptViewItemId::new("fixture"), bundle()));
    accepted.prepare(&source, 20);
    assert_eq!(
        accepted.producer_source("producer", 8, 0..6).as_deref(),
        Some("source")
    );
    accepted.prepare(&source, 10);
    assert!(accepted.producer_source("producer", 8, 0..6).is_none());
    accepted.prepare(&source, 20);
    assert!(accepted.accept_visual(TranscriptViewItemId::new("fixture"), bundle()));
    source.revision += 1;
    source.items[0].revision += 1;
    accepted.prepare(&source, 20);
    assert!(accepted.producer_source("producer", 8, 0..6).is_none());
    let mut projected = source.clone();
    let preparation = accepted
        .visual_preparation(&TranscriptViewItemId::new("fixture"))
        .unwrap();
    accepted.set_presentation(TranscriptPresentation {
        heading: Style::new().fg(bmux_tui::style::Color::Blue),
        ..Default::default()
    });
    assert!(!accepted.accept_prepared_visual(&preparation, visual(2), vec![image()], vec![]));
    projected.revision += 1;
    projected.items[0].revision += 1;
    accepted.prepare(&projected, 20);
    assert!(!accepted.accept_prepared_visual(&preparation, visual(2), vec![image()], vec![]));
}

#[test]
fn invalid_images_do_not_replace_accepted_rows() {
    let source = document(TextFormat::PlainText, "fallback");
    let mut accepted = AcceptedTranscript::default();
    accepted.prepare(&source, 20);
    for invalid in [
        ImagePlacement {
            lifecycle: ImageLifecycle::Persistent,
            ..image()
        },
        ImagePlacement {
            destination: Rect::new(19, 1, 8, 4),
            ..image()
        },
        ImagePlacement {
            payload: ImagePayload::Pixels {
                bytes: vec![0],
                width: 2,
                height: 2,
                format: ImagePixelFormat::Rgba8,
            },
            ..image()
        },
    ] {
        assert!(!accepted.accept_rich_visual(
            TranscriptViewItemId::new("fixture"),
            visual(1),
            vec![invalid]
        ));
        assert!(accepted.visuals.is_empty());
    }
    assert!(!accepted.accept_rich_visual(
        TranscriptViewItemId::new("fixture"),
        visual(1),
        vec![image(), image()]
    ));
}
