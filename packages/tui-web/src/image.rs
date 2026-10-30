//! Canonical artifact image admission with shared async visual fencing.
use crate::transcript::Transcript;
use bcode_session_view_models::{ToolResultView, TranscriptViewItemId, TranscriptViewItemKind};
use bcode_tui_components::transcript::accepted::{AcceptedVisual, VisualPreparation};
use bcode_tui_components::transcript::transcript_layout::TranscriptLayoutRows;
use bmux_tui::{
    geometry::Rect,
    image::{ImageFit, ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat, ImagePlacement},
};

pub struct PendingImage {
    pub token: VisualPreparation,
    pub revision: u64,
    pub width: u16,
    pub rows: u16,
    pub item_revision: u64,
}

impl Transcript {
    pub fn prepare_image(
        &self,
        snapshot: &bcode_session_view_models::SessionViewSnapshot,
        item_id: &str,
        reference: &str,
        width: u16,
    ) -> Option<PendingImage> {
        let item = snapshot
            .transcript
            .items
            .iter()
            .find(|item| item.id.get() == item_id)?;
        let TranscriptViewItemKind::ToolInvocation { tool } = &item.kind else {
            return None;
        };
        let Some(ToolResultView::Artifact { artifact }) = &tool.result else {
            return None;
        };
        let reference = artifact
            .artifact
            .refs
            .iter()
            .find(|value| value.key == reference)?;
        if !matches!(
            reference.content_type.as_deref(),
            Some("image/png" | "image/jpeg" | "image/webp")
        ) || reference
            .byte_len
            .is_none_or(|bytes| bytes == 0 || bytes > 128 * 1024)
        {
            return None;
        }
        let token = self
            .accepted
            .visual_preparation(&TranscriptViewItemId::new(item_id))?;
        if width == 0 {
            return None;
        }
        Some(PendingImage {
            token,
            revision: item.revision,
            item_revision: item.revision,
            width,
            rows: 12,
        })
    }
    pub fn accept_image(
        &mut self,
        pending: &PendingImage,
        bytes: Vec<u8>,
        width: u32,
        height: u32,
    ) -> bool {
        if width == 0
            || height == 0
            || width > 2048
            || height > 2048
            || bytes.len() > 4 * 1024 * 1024
            || u64::from(width) * u64::from(height) * 4 != bytes.len() as u64
        {
            return false;
        }
        let area = Rect::new(0, 0, pending.width, pending.rows);
        let image = ImagePlacement {
            key: ImageKey::new("artifact-image"),
            payload: ImagePayload::Pixels {
                bytes,
                width,
                height,
                format: ImagePixelFormat::Rgba8,
            },
            destination: area,
            allocation: None,
            clip: area,
            fit: ImageFit::Contain,
            lifecycle: ImageLifecycle::Frame,
        };
        let rows = (0..pending.rows)
            .map(|_| bmux_tui::prelude::Line::raw(""))
            .collect();
        let accepted = self.accepted.accept_prepared_visual(
            &pending.token,
            AcceptedVisual {
                item_revision: pending.item_revision,
                width: pending.width,
                revision: pending.revision,
                rows: TranscriptLayoutRows::Rendered(rows),
            },
            vec![image],
            Vec::new(),
        );
        if accepted {
            self.accepted.prepare_visuals(|_, _| None);
            self.rows = self.accepted.layout.total_rows();
        }
        accepted
    }
}
