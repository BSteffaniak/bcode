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
        let revision = self.accepted.replacement_visual_revision(&token)?;
        if width == 0 {
            return None;
        }
        Some(PendingImage {
            token,
            revision,
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

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_view_models::SessionViewSnapshot;
    use bmux_tui::{
        buffer::Buffer,
        component::{Component, Constraints, LayoutCx},
        frame::Frame,
        paint::PaintCx,
    };

    fn fixture() -> (Transcript, SessionViewSnapshot) {
        let mut snapshot = SessionViewSnapshot::empty();
        snapshot.transcript.revision = 1;
        snapshot.transcript.items.push(serde_json::from_value(serde_json::json!({
            "id":"image","revision":1,"sequence":null,"timestamp_ms":null,"streaming":false,
            "kind":{"type":"tool_invocation","tool":{
                "tool_call_id":"image-call","producer_plugin_id":"bcode.filesystem","tool_name":"read","arguments_json":"{}","working_directory":null,
                "status":"finished","result_text":null,"is_error":false,"timing":{},
                "result":{"type":"artifact","artifact":{
                    "artifact":{"artifact_id":"image-artifact","producer_plugin_id":"bcode.filesystem","schema":"bcode.filesystem.image","schema_version":1,"metadata":{},"refs":[{"key":"inline-image","content_type":"image/png","byte_len":4}]},
                    "generic_payload":{}
                }}
            }}
        })).unwrap());
        let mut transcript = Transcript::new();
        transcript.prepare_snapshot(&snapshot, 40);
        (transcript, snapshot)
    }

    fn card(transcript: &mut Transcript, revision: u64) {
        assert!(transcript.accepted.accept_visual(
            TranscriptViewItemId::new("image"),
            AcceptedVisual {
                item_revision: 1,
                width: 40,
                revision,
                rows: TranscriptLayoutRows::Rendered(vec![bmux_tui::prelude::Line::raw(
                    "producer image card"
                )]),
            }
        ));
    }

    #[test]
    fn decoded_image_replaces_higher_card_revision_and_paints_in_retained_scene() {
        let (mut transcript, snapshot) = fixture();
        card(&mut transcript, 500);
        let pending = transcript
            .prepare_image(&snapshot, "image", "inline-image", 40)
            .unwrap();
        assert_eq!(pending.revision, 501);
        let pixels = vec![255, 0, 0, 255];
        assert!(transcript.accept_image(&pending, pixels.clone(), 1, 1));
        let area = Rect::new(0, 0, 40, 30);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        let child = transcript.accepted.child("image-test", 0, "selection");
        let layout = child.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
        child.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(frame.images().len(), 1);
        let bmux_tui::image::ImageContribution::Present(image) = &frame.images()[0] else {
            panic!("decoded image must be composed as a retained placement");
        };
        assert_eq!(
            image.payload,
            ImagePayload::Pixels {
                bytes: pixels,
                width: 1,
                height: 1,
                format: ImagePixelFormat::Rgba8
            }
        );
        assert_eq!(image.destination.width, 40);
        assert_eq!(image.destination.height, 12);
        assert!(!transcript.accept_image(&pending, vec![255, 0, 0, 255], 1, 1));
    }

    #[test]
    fn decoded_image_cannot_replace_newer_output_or_exhausted_revision() {
        let (mut transcript, snapshot) = fixture();
        card(&mut transcript, 500);
        let pending = transcript
            .prepare_image(&snapshot, "image", "inline-image", 40)
            .unwrap();
        card(&mut transcript, 501);
        assert!(!transcript.accept_image(&pending, vec![255, 0, 0, 255], 1, 1));
        card(&mut transcript, u64::MAX);
        assert!(
            transcript
                .prepare_image(&snapshot, "image", "inline-image", 40)
                .is_none()
        );
    }

    #[test]
    fn decoded_image_preserves_geometry_and_item_revision_fences() {
        let (mut transcript, mut snapshot) = fixture();
        let pending = transcript
            .prepare_image(&snapshot, "image", "inline-image", 40)
            .unwrap();
        transcript.prepare_snapshot(&snapshot, 39);
        assert!(!transcript.accept_image(&pending, vec![255, 0, 0, 255], 1, 1));
        let pending = transcript
            .prepare_image(&snapshot, "image", "inline-image", 39)
            .unwrap();
        snapshot.transcript.revision += 1;
        snapshot.transcript.items[0].revision += 1;
        transcript.prepare_snapshot(&snapshot, 39);
        assert!(!transcript.accept_image(&pending, vec![255, 0, 0, 255], 1, 1));
    }
}
