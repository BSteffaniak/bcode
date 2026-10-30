//! Bounded selected-source preview decoded outside terminal painting.
use bcode_recap::{
    contracts::{BlockContent, RecapDocument},
    storage::DocumentStore,
};
use bmux_tui::{
    component::{Component, Constraints, LayoutCx, LayoutNode, LogicalSize},
    geometry::Rect,
    image::{
        ImageContribution, ImageFit, ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat,
        ImagePlacement,
    },
    paint::PaintCx,
};
use std::io::Cursor;

#[derive(Clone, Debug)]
pub(super) struct PreviewImage {
    pub(super) key: String,
    pub(super) payload: ImagePayload,
}

pub(super) fn load_preview(
    store: &DocumentStore,
    document: &RecapDocument,
    chapter: usize,
) -> Result<Option<PreviewImage>, String> {
    let Some(section) = document.sections.get(chapter) else {
        return Ok(None);
    };
    let asset_id = section
        .blocks
        .iter()
        .find_map(|block| match &block.content {
            BlockContent::Image { asset_id, .. } => Some(asset_id.as_str()),
            BlockContent::Comparison { items } => items.first().map(|item| item.asset_id.as_str()),
            BlockContent::Text { .. }
            | BlockContent::Clip { .. }
            | BlockContent::Heading { .. }
            | BlockContent::List { .. }
            | BlockContent::Diagram { .. } => None,
        });
    let Some(asset_id) = asset_id else {
        return Ok(None);
    };
    let asset = document
        .assets
        .iter()
        .find(|asset| asset.id == asset_id)
        .ok_or("Preview asset is not registered")?;
    let bytes = store
        .read_asset(&document.id, asset, 8 * 1024 * 1024)
        .map_err(|_| "Source image unavailable or exceeds preview budget")?;
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| "Unsupported source image")?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let preview = reader
        .decode()
        .map_err(|_| "Source image could not be decoded within preview limits")?
        .thumbnail(960, 540)
        .into_rgba8();
    let (width, height) = preview.dimensions();
    Ok(Some(PreviewImage {
        key: format!("recap-{}-{}-{asset_id}", document.id, document.revision),
        payload: ImagePayload::Pixels {
            bytes: preview.into_raw(),
            width,
            height,
            format: ImagePixelFormat::Rgba8,
        },
    }))
}
impl Component for PreviewImage {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        LayoutNode::leaf(
            "source-preview".into(),
            constraints.constrain(LogicalSize::new(constraints.max_width(), 8)),
        )
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let width = u16::try_from(layout.size.width).unwrap_or(u16::MAX);
        let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        if width == 0 || height == 0 {
            return;
        }
        let area = Rect::new(0, 0, width, height);
        cx.push_image(ImageContribution::Present(ImagePlacement {
            key: ImageKey::new(self.key.clone()),
            payload: self.payload.clone(),
            destination: area,
            fit: ImageFit::Contain,
            allocation: None,
            clip: area,
            lifecycle: ImageLifecycle::Frame,
        }));
    }
}
