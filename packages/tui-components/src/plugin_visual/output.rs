//! Complete plugin visual output: rows with their source correspondence, item-local images
//! and action regions, accepted atomically by the host.

use std::collections::{BTreeMap, BTreeSet};

use bcode_plugin_models::tui_selection::PluginTuiSelectionRow;
use bcode_plugin_models::tui_visual::TuiVisualAnchor;
use bmux_tui::image::ImagePlacement;
use bmux_tui::prelude::Line;

use super::action::{VisualAction, VisualActionRegion};

/// Most rows one plugin visual may retain.
pub const MAX_VISUAL_ROWS: usize = 8192;
/// Most text bytes of all rows (and, separately, of all selection rows) of one visual.
pub const MAX_VISUAL_TEXT_BYTES: usize = 256 * 1024;
/// Most images one plugin visual may place.
pub const MAX_VISUAL_IMAGES: usize = 32;
/// Most encoded bytes of all images of one plugin visual.
pub const MAX_VISUAL_IMAGE_BYTES: usize = 8 * 1024 * 1024;

/// Adapter rows and the correspondence captured with this exact projection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VisualRows {
    /// Styled rows, one terminal row each; hosts clip them and never reflow them.
    pub rows: Vec<Line>,
    /// Adapter-owned content correspondence (content keys and source ranges) by row.
    pub anchors: Vec<TuiVisualAnchor>,
    /// Source text and cell geometry of rows that copy their source, by row.
    pub selection: BTreeMap<usize, PluginTuiSelectionRow>,
}

impl VisualRows {
    /// Rows without correspondence.
    #[must_use]
    pub const fn rendered(rows: Vec<Line>) -> Self {
        Self {
            rows,
            anchors: Vec::new(),
            selection: BTreeMap::new(),
        }
    }

    /// Rows with adapter anchors.
    #[must_use]
    pub const fn anchored(rows: Vec<Line>, anchors: Vec<TuiVisualAnchor>) -> Self {
        Self {
            rows,
            anchors,
            selection: BTreeMap::new(),
        }
    }

    /// Number of rows.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether there are no rows.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Whether the rows, anchors and selection rows are bounded and consistent.
    #[must_use]
    pub fn supported(&self) -> bool {
        if self.rows.len() > MAX_VISUAL_ROWS
            || bcode_plugin_models::tui_visual::validate_visual_anchors(
                &self.anchors,
                self.rows.len(),
            )
            .is_err()
        {
            return false;
        }
        let bytes = self
            .rows
            .iter()
            .flat_map(|line| &line.spans)
            .try_fold(0usize, |sum, span| sum.checked_add(span.content.len()));
        if bytes.is_none_or(|bytes| bytes > MAX_VISUAL_TEXT_BYTES) {
            return false;
        }
        self.selection.keys().all(|row| *row < self.rows.len())
            && self.selection.values().all(PluginTuiSelectionRow::is_valid)
            && self
                .selection
                .values()
                .map(|row| row.text.len())
                .sum::<usize>()
                <= MAX_VISUAL_TEXT_BYTES
    }
}

impl From<Vec<Line>> for VisualRows {
    fn from(rows: Vec<Line>) -> Self {
        Self::rendered(rows)
    }
}

/// Whether `images` are bounded, frame-scoped, uniquely keyed placements that lie inside a
/// `width` by `rows` visual and are not pre-clipped.
#[must_use]
pub fn images_supported(images: &[ImagePlacement], width: u16, rows: usize) -> bool {
    use bmux_tui::image::{ImageLifecycle, ImagePayload, ImagePixelFormat};
    if images.len() > MAX_VISUAL_IMAGES {
        return false;
    }
    let mut keys = BTreeSet::new();
    let mut bytes = 0usize;
    images.iter().all(|image| {
        let (payload, w, h, valid) = match &image.payload {
            ImagePayload::Png {
                bytes,
                width,
                height,
            } => (bytes, *width, *height, true),
            ImagePayload::Pixels {
                bytes,
                width,
                height,
                format,
            } => {
                let channels = match format {
                    ImagePixelFormat::Rgb8 => 3usize,
                    ImagePixelFormat::Rgba8 => 4usize,
                };
                let expected = usize::try_from(*width)
                    .ok()
                    .and_then(|w| usize::try_from(*height).ok().and_then(|h| w.checked_mul(h)))
                    .and_then(|n| n.checked_mul(channels));
                (bytes, *width, *height, expected == Some(bytes.len()))
            }
        };
        bytes = bytes.saturating_add(payload.len());
        valid
            && w > 0
            && h > 0
            && w <= 4096
            && h <= 4096
            && !payload.is_empty()
            && bytes <= MAX_VISUAL_IMAGE_BYTES
            && !image.key.as_str().is_empty()
            && image.key.as_str().len() <= 512
            && keys.insert(image.key.clone())
            && image.lifecycle == ImageLifecycle::Frame
            && image.allocation.is_none()
            && !image.destination.is_empty()
            && image.destination.right() <= width
            && usize::from(image.destination.bottom()) <= rows
            && image.clip == image.destination
    })
}

/// Complete producer output, accepted atomically with source geometry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortableVisualOutput {
    /// Rows and their original source mappings/anchors, never server-made cells.
    pub rows: VisualRows,
    /// Adapter-local image geometry captured with the rows.
    pub images: Vec<ImagePlacement>,
    /// Narrow intents only; registering an adapter does not authorize execution.
    pub actions: Vec<VisualActionRegion>,
}

impl PortableVisualOutput {
    /// Whether the rows, images and actions are bounded and lie inside a `width`-cell visual.
    #[must_use]
    pub fn supported(&self, width: u16) -> bool {
        self.rows.supported()
            && images_supported(&self.images, width, self.rows.len())
            && super::action::supported(&self.actions, width, self.rows.len())
    }

    /// Attach bounded copy intents from the exact retained source-cell geometry.
    /// Raster text is never used as copy authority. Unmapped decoration is inert.
    #[must_use]
    pub fn with_source_copy_actions(mut self) -> Self {
        for (row, source) in &self.rows.selection {
            let Ok(y) = u16::try_from(*row) else { continue };
            if !source.is_valid() {
                continue;
            }
            for (index, cell) in source.cells.iter().enumerate() {
                if self.actions.len() >= 1024 {
                    return self;
                }
                let start = cell.bytes.start - source.byte_start;
                let end = cell.bytes.end - source.byte_start;
                let Some(text) = source.text.get(start..end).filter(|text| !text.is_empty()) else {
                    continue;
                };
                self.actions.push(VisualActionRegion {
                    id: format!("copy:{row}:{index}"),
                    area: bmux_tui::geometry::Rect::new(cell.column, y, cell.width, 1),
                    action: VisualAction::Copy(text.to_owned()),
                });
            }
        }
        self
    }
}
