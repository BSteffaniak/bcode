use super::action_tests::markdown_hits;
use super::tests::document;
use super::*;

#[test]
fn disclosure_reprepares_and_fragments_use_retained_bmux_anchors() {
    let source = document(
        TextFormat::Markdown,
        "[go](#target)\n\n# Target\n\n<details>\n<summary>More</summary>\n\nsecret 界🙂\n\n</details>",
    );
    let mut accepted = AcceptedTranscript::default();
    accepted.prepare(&source, 40);
    let hits = markdown_hits(&accepted);
    let fragment = hits
        .iter()
        .find(|hit| {
            matches!(
                accepted.activate(hit),
                Some(super::super::actions::TranscriptAction::Open(
                    bcode_markdown_render::MarkdownDestination::Fragment(_)
                ))
            )
        })
        .unwrap();
    let (projection, body_start) = accepted.layout.markdown(0).unwrap();
    let expected = body_start
        + usize::from(
            projection
                .anchors
                .iter()
                .find(|a| a.fragment == "target")
                .unwrap()
                .row,
        );
    assert!(
        matches!(accepted.activate_local(fragment), Some(TranscriptLocalAction::ScrollTo { row, .. }) if row == expected)
    );
    let disclosure = hits.iter().find(|hit| matches!(accepted.activate(hit), Some(super::super::actions::TranscriptAction::Presentation { kind, .. }) if matches!(kind.as_ref(), bcode_markdown_render::MarkdownContributionKind::Details { .. }))).unwrap();
    let old_height = accepted.layout.total_rows();
    let revision = accepted.document.revision();
    assert_eq!(
        accepted.activate_local(disclosure),
        Some(TranscriptLocalAction::Reprepare)
    );
    assert!(accepted.activate_local(disclosure).is_none());
    accepted.prepare(&source, 40);
    assert_eq!(accepted.document.revision(), revision);
    assert!(accepted.layout.total_rows() > old_height);
    accepted.prepare(&source, 20);
    assert!(accepted.activate_local(fragment).is_none());
}

#[test]
fn rich_actions_share_clipped_hit_and_semantic_geometry() {
    use bmux_tui::{
        buffer::Buffer,
        component::{Component, Constraints, LayoutCx},
        frame::Frame,
        geometry::Rect,
        paint::PaintCx,
    };
    let source = document(
        TextFormat::Markdown,
        "[Acceptance link](https://example.invalid)\n\n```rust\nlet browser = true;\n```\n\n<details><summary>Expand detail</summary>\n\nHidden body\n\n</details>",
    );
    let mut accepted = AcceptedTranscript::default();
    for width in [40, 12, 1, 0] {
        accepted.prepare(&source, width);
        for top in [0, 4, accepted.layout.total_rows()] {
            let area = Rect::new(0, 0, width, 30);
            let mut buffer = Buffer::empty(area);
            let mut frame = Frame::new(&mut buffer);
            let child = accepted.child("transcript", top, "selection");
            let layout = child.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
            child.paint(&layout, &mut PaintCx::new(&mut frame));
            let actions = frame
                .hits()
                .regions()
                .iter()
                .filter(|hit| {
                    hit.id.as_str().starts_with("markdown:")
                        || hit.id.as_str().starts_with("transcript-copy:")
                })
                .collect::<Vec<_>>();
            for hit in &actions {
                let semantic = frame
                    .semantics()
                    .regions()
                    .iter()
                    .find(|semantic| semantic.id == hit.id.as_str() && semantic.area == hit.area)
                    .expect("every retained action hit has matching clipped semantics");
                let accessibility = semantic.accessibility.as_ref().unwrap();
                assert!(
                    accessibility
                        .actions
                        .contains(&bmux_tui::semantic::SemanticAction::Activate)
                );
                assert!(accepted.activate(hit.id.as_str()).is_some());
            }
            for semantic in frame.semantics().regions().iter().filter(|semantic| {
                semantic.id.starts_with("markdown:") || semantic.id.starts_with("transcript-copy:")
            }) {
                assert!(
                    actions
                        .iter()
                        .any(|hit| hit.id.as_str() == semantic.id && hit.area == semantic.area)
                );
            }
            if width == 40 && top == 0 {
                assert!(
                    frame
                        .semantics()
                        .regions()
                        .iter()
                        .any(|region| region.role == "link")
                );
                assert!(frame.semantics().regions().iter().any(|region| {
                    region
                        .accessibility
                        .as_ref()
                        .and_then(|a| a.label.as_deref())
                        .is_some_and(|label| label.contains("Expand detail"))
                }));
                assert!(
                    actions
                        .iter()
                        .any(|hit| hit.id.as_str().starts_with("transcript-copy:"))
                );
            }
        }
    }
}
