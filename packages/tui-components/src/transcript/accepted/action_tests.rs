//! Typed action resolution is revision-bound and never executes host work.
use super::super::actions::{TranscriptAction, TranscriptActionRegion};
use super::tests::document;
use super::*;
use bmux_tui::{
    buffer::Buffer,
    component::{Component, Constraints, LayoutCx},
    frame::Frame,
    geometry::Rect,
    paint::PaintCx,
};

pub(super) fn markdown_hits(accepted: &AcceptedTranscript) -> Vec<String> {
    let area = Rect::new(0, 0, 40, 40);
    let mut buffer = Buffer::empty(area);
    let mut frame = Frame::new(&mut buffer);
    let child = accepted.child("transcript", 0, "selection");
    let layout = child.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    child.paint(&layout, &mut PaintCx::new(&mut frame));
    frame
        .hits()
        .regions()
        .iter()
        .filter(|hit| hit.id.as_str().starts_with("markdown:"))
        .map(|hit| hit.id.as_str().to_owned())
        .collect()
}

#[test]
fn producer_actions_are_clipped_and_old_revision_hits_are_inert() {
    let mut source = document(TextFormat::PlainText, "fallback");
    let mut accepted = AcceptedTranscript::default();
    accepted.prepare(&source, 20);
    let visual = AcceptedVisual {
        item_revision: 1,
        width: 20,
        revision: 4,
        rows: TranscriptLayoutRows::Rendered(vec![Line::raw("action")]),
    };
    let action = TranscriptAction::Plugin {
        producer_id: "plugin".into(),
        control_id: "expand".into(),
        payload: serde_json::json!({"expanded":true}),
    };
    assert!(accepted.accept_rich_visual_with_actions(
        TranscriptViewItemId::new("fixture"),
        visual,
        vec![],
        vec![TranscriptActionRegion {
            id: "expand".into(),
            area: Rect::new(0, 0, 6, 1),
            action: action.clone(),
        }]
    ));
    accepted.prepare(&source, 20);
    let area = Rect::new(0, 0, 20, 1);
    let mut buffer = Buffer::empty(area);
    let mut frame = Frame::new(&mut buffer);
    let child = accepted.child("transcript", 0, "selection");
    let layout = child.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    child.paint(&layout, &mut PaintCx::new(&mut frame));
    let hit = frame
        .hits()
        .regions()
        .iter()
        .find(|hit| hit.id.as_str().starts_with("transcript-action:"))
        .unwrap();
    let hit_id = hit.id.as_str().to_owned();
    assert_eq!(hit.area.intersection(area), hit.area);
    assert_eq!(accepted.activate(&hit_id), Some(action));
    source.revision += 1;
    source.items[0].revision += 1;
    accepted.prepare(&source, 20);
    assert!(accepted.activate(&hit_id).is_none());
}

#[test]
fn markdown_links_resolve_from_real_geometry_and_reject_old_width() {
    let source = document(TextFormat::Markdown, "[example](https://example.com)");
    let mut accepted = AcceptedTranscript::default();
    accepted.prepare(&source, 40);
    let area = Rect::new(0, 0, 40, 8);
    let mut buffer = Buffer::empty(area);
    let mut frame = Frame::new(&mut buffer);
    let child = accepted.child("transcript", 0, "selection");
    let layout = child.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    child.paint(&layout, &mut PaintCx::new(&mut frame));
    let hit = frame
        .hits()
        .regions()
        .iter()
        .find(|hit| hit.id.as_str().starts_with("markdown:"))
        .unwrap();
    let hit_id = hit.id.as_str().to_owned();
    assert!(matches!(
        accepted.activate(&hit_id),
        Some(TranscriptAction::Open(
            bcode_markdown_render::MarkdownDestination::Web(_)
        ))
    ));
    accepted.prepare(&source, 20);
    assert!(accepted.activate(&hit_id).is_none());
}
