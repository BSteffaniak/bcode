//! Markdown body behavior: row-count-preserving containment (B10), source mapping from the
//! retained projection (B6, C24), document-scoped `<details>` state and work reporting.

use bmux_tui::buffer::Buffer;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::text_width::display_width;

use super::*;
use crate::layout::context::RenderConfig;
use crate::units::structure::ResolvedUnitPresentation;
use crate::units::{BoundaryEdge, UnitKey};

fn layout(content: &dyn ItemContent, width: u64) -> LayoutNode {
    content.layout(Constraints::for_width(width), &mut LayoutCx::new())
}

fn paint(content: &dyn ItemContent, node: &LayoutNode) -> Vec<String> {
    let width = u16::try_from(node.size.width).expect("width");
    let height = u16::try_from(node.size.height).expect("height");
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
    let mut frame = Frame::new(&mut buffer);
    content.paint(node, &mut PaintCx::new(&mut frame));
    (0..height)
        .map(|row| frame.buffer().row_symbols(row).unwrap_or_default())
        .collect()
}

fn options(config: &RenderConfig, request: &MarkdownRequest<'_>) -> MarkdownRenderOptions {
    let structure = ResolvedUnitPresentation::Boundary;
    let cx = config.context(
        &UnitKey::Boundary(BoundaryEdge::Older),
        request.width,
        &structure,
    );
    render_options(request, &cx)
}

fn project(config: &RenderConfig, request: &MarkdownRequest<'_>) -> (MarkdownBody, ProducerWork) {
    let structure = ResolvedUnitPresentation::Boundary;
    let cx = config.context(
        &UnitKey::Boundary(BoundaryEdge::Older),
        request.width,
        &structure,
    );
    markdown_projection(request, &cx)
}

#[test]
fn b10_container_keeps_projection_rows_and_clips_overflow() {
    let config = RenderConfig::default();
    let source =
        "# Title\n\nsome words that wrap\n\n- item one\n- item two\n\n```rust\nfn main() {}\n```";
    let (body, work) = project(&config, &MarkdownRequest::new(source, 30));
    assert_eq!(work.markdown_parses, 1);
    assert_eq!(work.projections, 1);
    assert!(
        work.highlights >= 1,
        "the fenced code block counts as a highlight"
    );
    let at_width = layout(&body, 30);
    assert_eq!(
        usize::try_from(at_width.size.height).expect("rows"),
        body.rows()
    );
    // Laid out narrower than projected, the body keeps its row count and clips (B10).
    let narrow = layout(&body, 6);
    assert_eq!(narrow.size.height, at_width.size.height);
    for row in paint(&body, &narrow) {
        assert!(display_width(&row) <= 6, "{row:?}");
    }
}

#[test]
fn b6_c24_source_rows_come_from_the_retained_projection() {
    let config = RenderConfig::default();
    let source = "one two three four five six seven eight nine ten\n\nsecond paragraph here";
    let second = source.find("second").expect("offset") as u64;
    for width in [80, 16, 8] {
        let (body, _) = project(&config, &MarkdownRequest::new(source, width));
        let node = layout(&body, width);
        let geometry = body.geometry(&node);
        let rows = paint(&body, &node);
        let row = geometry
            .source_map
            .row_for_source(&SourcePosition::MarkdownByte(second))
            .expect("mapped");
        assert!(
            rows[usize::try_from(row).expect("row")].contains("second"),
            "{rows:?}"
        );
        let bytes: Vec<_> = geometry
            .source_map
            .rows()
            .iter()
            .map(|row| row.position.clone())
            .collect();
        assert!(bytes.windows(2).all(|pair| pair[0] <= pair[1]), "{bytes:?}");
        assert_eq!(geometry.selection_fragments[0].rows, 0..node.size.height);
    }
}

#[test]
fn details_state_is_scoped_to_the_owning_document() {
    let source = "<details><summary>More</summary>\n\nhidden body text\n\n</details>";
    let request = MarkdownRequest::new(source, 40).document("transcript:item:a");
    let mut config = RenderConfig::default();
    let (closed, _) = project(&config, &request);
    let id = render_markdown(source, &options(&config, &request))
        .contributions
        .iter()
        .find(|contribution| matches!(contribution.kind, MarkdownContributionKind::Details { .. }))
        .map(|contribution| contribution.id.clone())
        .expect("details contribution");
    assert!(id.starts_with("transcript:item:a:"), "{id}");
    // Another document's state does not apply.
    config
        .markdown
        .details_open
        .insert("transcript:item:b:x".to_owned(), true);
    let (unrelated, _) = project(&config, &request);
    assert_eq!(unrelated.rows(), closed.rows());
    config.markdown.details_open.insert(id, true);
    let (open, _) = project(&config, &request);
    assert!(
        open.rows() > closed.rows(),
        "opening details adds the body rows"
    );
}

// ---- asynchronous visuals (L8) -------------------------------------------------------------

const NO_IMAGES: [Rect; 0] = [];

const IMAGE_DOC: &str = "Before\n\n![a cat](https://example.com/cat.png)\n\nAfter";

fn image_config() -> RenderConfig {
    let mut config = RenderConfig::default();
    config.markdown.images = true;
    config
}

fn image_request() -> MarkdownRequest<'static> {
    MarkdownRequest::new(IMAGE_DOC, 30).document("transcript:item:a")
}

fn only_slot(body: &MarkdownBody, width: u64) -> VisualSlot {
    let node = layout(body, width);
    let geometry = body.geometry(&node);
    let [slot] = geometry.visual_slots.as_slice() else {
        panic!("one slot, got {:?}", geometry.visual_slots);
    };
    slot.clone()
}

fn pixels() -> Arc<ImagePayload> {
    Arc::new(ImagePayload::Pixels {
        bytes: vec![0; 4],
        width: 1,
        height: 1,
        format: bmux_tui::image::ImagePixelFormat::Rgba8,
    })
}

fn accepted(slot: &VisualSlot, revision: u64, visual: MarkdownVisual) -> MarkdownVisualState {
    let mut state = MarkdownVisualState::new();
    state.accept(
        slot.contribution_id.clone(),
        AcceptedMarkdownVisual {
            source_key: slot.request.source_key(),
            revision,
            visual,
        },
    );
    state
}

/// Images painted by `body` at `node` in a frame of `height` rows, with the body placed at row
/// `y` (negative: scrolled above the frame).
fn painted_images(body: &MarkdownBody, node: &LayoutNode, y: i64, height: u16) -> Vec<Rect> {
    let width = u16::try_from(node.size.width).expect("width");
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
    let mut frame = Frame::new(&mut buffer);
    {
        let mut cx = PaintCx::new(&mut frame);
        let size = bmux_tui::paint::LocalRect::new(
            0,
            0,
            width,
            u16::try_from(node.size.height).expect("height"),
        );
        cx.with_child(0, y, size, |cx| body.paint(node, cx));
    }
    frame
        .images()
        .iter()
        .filter_map(|image| match image {
            ImageContribution::Present(placement) => Some(placement.destination),
            ImageContribution::Remove(_) => None,
        })
        .collect()
}

#[test]
fn l8_image_reserves_a_slot_with_the_worker_request() {
    let config = image_config();
    let (body, _) = project(&config, &image_request());
    let slot = only_slot(&body, 30);
    assert!(slot.contribution_id.starts_with("transcript:item:a:"));
    assert_eq!(slot.rect.height, u64::from(IMAGE_RESERVED_ROWS));
    assert_eq!(
        slot.rect.x + slot.rect.width,
        30,
        "the slot spans to the body edge"
    );
    let VisualSlotRequest::Image { alt, source, .. } = &slot.request else {
        panic!("image request");
    };
    assert_eq!(alt, "a cat");
    assert!(matches!(source, MarkdownDestination::Web(_)));
    // Without an accepted visual, nothing is placed and the placeholder text shows.
    let node = layout(&body, 30);
    assert_eq!(painted_images(&body, &node, 0, 20), NO_IMAGES);
    assert!(paint(&body, &node).iter().any(|row| row.contains("a cat")));
}

#[test]
fn l8_accepted_image_keeps_rows_blanks_the_slot_and_places_one_image() {
    let mut config = image_config();
    let (placeholder, _) = project(&config, &image_request());
    let slot = only_slot(&placeholder, 30);
    config.markdown.visuals = accepted(&slot, 1, MarkdownVisual::Image(pixels()));
    let (body, _) = project(&config, &image_request());
    assert_eq!(
        body.rows(),
        placeholder.rows(),
        "acceptance never changes rows"
    );
    let node = layout(&body, 30);
    let rows = paint(&body, &node);
    let start = usize::try_from(slot.rect.y).expect("row");
    for row in &rows[start..start + usize::from(IMAGE_RESERVED_ROWS)] {
        assert!(
            row.trim().is_empty(),
            "slot rows are blank under the image: {row:?}"
        );
    }
    assert!(rows.iter().any(|row| row.contains("After")));
    let expected = Rect::new(
        u16::try_from(slot.rect.x).expect("x"),
        u16::try_from(slot.rect.y).expect("y"),
        u16::try_from(slot.rect.width).expect("width"),
        IMAGE_RESERVED_ROWS,
    );
    assert_eq!(painted_images(&body, &node, 0, 20), vec![expected]);
    // The rows of a placed image are rendered rows: no source anchor inside the slot.
    let geometry = body.geometry(&node);
    assert!(
        geometry
            .source_map
            .rows()
            .iter()
            .all(|row| !(slot.rect.y..slot.rect.y + slot.rect.height).contains(&row.row))
    );
}

#[test]
fn l8_image_placements_follow_the_painted_position_and_clip() {
    let mut config = image_config();
    let (placeholder, _) = project(&config, &image_request());
    let slot = only_slot(&placeholder, 30);
    config.markdown.visuals = accepted(&slot, 1, MarkdownVisual::Image(pixels()));
    let (body, _) = project(&config, &image_request());
    let node = layout(&body, 30);
    let slot_y = u16::try_from(slot.rect.y).expect("y");
    // Scrolled so the slot starts one row above the frame: the placement is clipped.
    let above = -i64::from(slot_y) - 1;
    let clipped = painted_images(&body, &node, above, 20);
    assert_eq!(clipped.len(), 1);
    assert_eq!(clipped[0].y, 0);
    assert_eq!(clipped[0].height, IMAGE_RESERVED_ROWS - 1);
    // Entirely outside the frame: no placement at all.
    assert_eq!(painted_images(&body, &node, 40, 20), NO_IMAGES);
    assert_eq!(painted_images(&body, &node, -40, 20), NO_IMAGES);
}

#[test]
fn l8_visual_applies_only_on_exact_source_match() {
    let mut config = image_config();
    let (placeholder, _) = project(&config, &image_request());
    let slot = only_slot(&placeholder, 30);
    let mut state = MarkdownVisualState::new();
    state.accept(
        slot.contribution_id,
        AcceptedMarkdownVisual {
            source_key: "image:https://example.com/dog.png".to_owned(),
            revision: 1,
            visual: MarkdownVisual::Image(pixels()),
        },
    );
    config.markdown.visuals = state;
    let (body, _) = project(&config, &image_request());
    let node = layout(&body, 30);
    assert_eq!(painted_images(&body, &node, 0, 20), NO_IMAGES);
    assert_eq!(
        paint(&body, &node),
        paint(&placeholder, &layout(&placeholder, 30))
    );
}

#[test]
fn l8_notice_replaces_slot_rows_and_clips_at_the_width() {
    let mut config = image_config();
    let (placeholder, _) = project(&config, &image_request());
    let slot = only_slot(&placeholder, 30);
    let notice = "[a cat — image failed: a very long diagnostic that overflows]\nsecond";
    config.markdown.visuals = accepted(&slot, 1, MarkdownVisual::Notice(Arc::from(notice)));
    let (body, _) = project(&config, &image_request());
    let node = layout(&body, 30);
    let rows = paint(&body, &node);
    let start = usize::try_from(slot.rect.y).expect("row");
    assert!(rows[start].starts_with("[a cat — image failed"));
    assert!(display_width(rows[start].trim_end()) <= 30);
    assert_eq!(rows[start + 1].trim_end(), "second");
    assert_eq!(rows[start + 2].trim(), "");
    assert_eq!(painted_images(&body, &node, 0, 20), NO_IMAGES);
    assert_eq!(body.rows(), placeholder.rows());
}

#[test]
fn l8_mermaid_slots_exist_only_with_mermaid_contributions() {
    let source = "```mermaid\ngraph TD; A-->B\n```";
    let request = MarkdownRequest::new(source, 40).document("transcript:item:m");
    let (plain, _) = project(&RenderConfig::default(), &request);
    assert_eq!(plain.visual_slots(), [] as [VisualSlot; 0]);
    let mut config = RenderConfig::default();
    config.markdown.mermaid = true;
    let (body, _) = project(&config, &request);
    let slot = only_slot(&body, 40);
    assert!(matches!(slot.request, VisualSlotRequest::Mermaid { .. }));
    assert!(slot.rect.height >= u64::from(MERMAID_RESERVED_ROWS));
    assert!(slot.request.source_key().starts_with("mermaid:"));
}

#[test]
fn l3_accepting_a_visual_changes_only_the_owning_units_fingerprint() {
    let structure = ResolvedUnitPresentation::Boundary;
    let owner = UnitKey::Item(bcode_session_view_models::TranscriptViewItemId::new("a"));
    let other = UnitKey::Item(bcode_session_view_models::TranscriptViewItemId::new("b"));
    let base = RenderConfig::default();
    let fingerprint =
        |config: &RenderConfig, unit: &UnitKey| config.context(unit, 40, &structure).fingerprint();
    let mut config = base.clone();
    let id = format!(
        "{}:image:0",
        crate::layout::context::markdown_document_id(&owner)
    );
    config.markdown.visuals.accept(
        id.clone(),
        AcceptedMarkdownVisual {
            source_key: "image:x".to_owned(),
            revision: 1,
            visual: MarkdownVisual::Notice(Arc::from("n")),
        },
    );
    assert_ne!(fingerprint(&config, &owner), fingerprint(&base, &owner));
    assert_eq!(fingerprint(&config, &other), fingerprint(&base, &other));
    let mut replaced = config.clone();
    replaced.markdown.visuals.accept(
        id,
        AcceptedMarkdownVisual {
            source_key: "image:x".to_owned(),
            revision: 2,
            visual: MarkdownVisual::Notice(Arc::from("n")),
        },
    );
    assert_ne!(fingerprint(&replaced, &owner), fingerprint(&config, &owner));
}
