//! Canonical artifact image admission as an accepted plugin visual (spec L8, E10).
//!
//! A decoded artifact image becomes the item's accepted visual: [`IMAGE_ROWS`] reserved rows
//! with one image placement over them, painted from the retained rows that show it. Decoding
//! is asynchronous, so the request captures the item's identity, revision, body width and the
//! revision of the visual accepted for it; the decoded pixels are admitted only while every
//! one of them still holds (stale-identity fence).
use bcode_session_view_models::{ToolResultView, TranscriptViewItemId, TranscriptViewItemKind};
use bcode_tui_transcript::engine::TranscriptEngine;
use bmux_tui::{
    geometry::Rect,
    image::{ImageFit, ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat, ImagePlacement},
};

use crate::visuals::{BrowserVisuals, DecodedImage, body_width, resident_item};

/// Rows a decoded artifact image reserves.
pub const IMAGE_ROWS: u16 = 12;
/// Largest encoded artifact image admitted for decoding.
pub const MAX_ENCODED_IMAGE_BYTES: u64 = 128 * 1024;
/// Largest decoded image side in pixels.
pub const MAX_IMAGE_SIDE: u32 = 2048;
/// Largest decoded RGBA image in bytes.
pub const MAX_DECODED_IMAGE_BYTES: usize = 4 * 1024 * 1024;

/// Identity one image decode was requested for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingImage {
    item: TranscriptViewItemId,
    item_revision: u64,
    width: u16,
    visual_revision: Option<u64>,
}

/// Capture the identity of a bounded image reference of `item_id` before decoding it.
#[must_use]
pub fn prepare_image(
    engine: &TranscriptEngine,
    visuals: &BrowserVisuals,
    item_id: &str,
    reference: &str,
) -> Option<PendingImage> {
    let id = TranscriptViewItemId::new(item_id);
    let item = resident_item(engine, &id)?;
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
        .is_none_or(|bytes| bytes == 0 || bytes > MAX_ENCODED_IMAGE_BYTES)
    {
        return None;
    }
    let width = body_width(engine, item);
    if width == 0 {
        return None;
    }
    Some(PendingImage {
        visual_revision: visuals.revision_of(&id),
        item: id,
        item_revision: item.revision,
        width,
    })
}

/// Admit decoded RGBA pixels for `pending` while its captured identity still holds. Returns
/// whether the image was accepted; it is shown from the next visual sync.
pub fn accept_image(
    engine: &TranscriptEngine,
    visuals: &mut BrowserVisuals,
    pending: &PendingImage,
    bytes: Vec<u8>,
    width: u32,
    height: u32,
) -> bool {
    if width == 0
        || height == 0
        || width > MAX_IMAGE_SIDE
        || height > MAX_IMAGE_SIDE
        || bytes.len() > MAX_DECODED_IMAGE_BYTES
        || u64::from(width) * u64::from(height) * 4 != bytes.len() as u64
    {
        return false;
    }
    let Some(item) = resident_item(engine, &pending.item) else {
        return false;
    };
    if item.revision != pending.item_revision
        || body_width(engine, item) != pending.width
        || visuals.revision_of(&pending.item) != pending.visual_revision
    {
        return false;
    }
    let area = Rect::new(0, 0, pending.width, IMAGE_ROWS);
    visuals.accept_image(
        pending.item.clone(),
        DecodedImage {
            item_revision: pending.item_revision,
            width: pending.width,
            rows: IMAGE_ROWS,
            image: ImagePlacement {
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
            },
        },
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::BrowserTranscript;
    use bcode_tui_transcript::frame::FrameClock;
    use bmux_tui::{buffer::Buffer, frame::Frame, paint::PaintCx};
    use web_time::Instant;

    const SESSION: &str = "00000000-0000-4000-8000-000000000001";

    fn event(sequence: u64, kind: &serde_json::Value) -> serde_json::Value {
        serde_json::json!({"schema_version": 1, "sequence": sequence, "timestamp_ms": sequence,
            "session_id": SESSION, "kind": kind})
    }

    /// A transcript attached to a session holding one finished image artifact call.
    fn fixture(width: u16) -> BrowserTranscript {
        let at = Instant::now();
        let mut transcript = BrowserTranscript::new(SESSION.parse().unwrap());
        let _ = transcript.sync_geometry(width, 30, at);
        let start = serde_json::json!({"schema_version":1,"continuity":"start",
            "cursor":{"epoch":SESSION,"offset":0},"events":[],"more":false});
        transcript
            .accept_live(start.to_string().as_bytes(), at)
            .unwrap();
        let events = [
            event(
                1,
                &serde_json::json!({"tool_call_requested": {"tool_call_id": "image-call",
                "producer_plugin_id": "bcode.filesystem", "tool_name": "read", "arguments_json": "{}"}}),
            ),
            event(
                2,
                &serde_json::json!({"tool_invocation_result_recorded": {"record": {
                "invocation_id": "image-call", "model_output": "", "is_error": false,
                "result": {"type": "artifact", "artifact": {"artifact_id": "image-artifact",
                    "producer_plugin_id": "bcode.filesystem", "schema": "bcode.filesystem.image",
                    "schema_version": 1, "refs": [{"key": "inline-image",
                        "content_type": "image/png", "byte_len": 4}]}}}}}),
            ),
        ];
        let page = serde_json::json!({"schema_version": 1, "page": {
            "session_id": SESSION, "events": events, "has_more": false}})
        .to_string();
        while let Some(read) = transcript.next_history_read(at).unwrap() {
            transcript
                .accept_history_page(read.claim, Some(page.as_bytes()), at)
                .unwrap();
        }
        let _ = transcript.sync_render(None, 0, at);
        transcript
    }

    fn item() -> &'static str {
        "tool:image-call"
    }

    fn accept(transcript: &mut BrowserTranscript, pending: &PendingImage) -> bool {
        let (engine, visuals) = transcript.engine_and_visuals();
        accept_image(engine, visuals, pending, vec![255, 0, 0, 255], 1, 1)
    }

    fn images(transcript: &mut BrowserTranscript) -> Vec<bmux_tui::image::ImageContribution> {
        let _ = transcript.sync_render(None, 0, Instant::now());
        let snapshot = transcript
            .prepare_frame(FrameClock {
                now: Instant::now(),
            })
            .clone();
        let area = Rect::new(0, 0, 40, 30);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        transcript
            .engine()
            .paint_frame(&snapshot, &mut PaintCx::new(&mut frame));
        frame.images().to_vec()
    }

    #[test]
    fn a_decoded_image_becomes_the_items_visual_and_paints_one_placement() {
        let mut transcript = fixture(40);
        assert_eq!(images(&mut transcript).len(), 0);
        let pending = prepare_image(
            transcript.engine(),
            &transcript.visuals,
            item(),
            "inline-image",
        )
        .expect("bounded image reference");
        assert!(accept(&mut transcript, &pending));
        let painted = images(&mut transcript);
        assert_eq!(painted.len(), 1);
        let bmux_tui::image::ImageContribution::Present(image) = &painted[0] else {
            panic!("a present placement");
        };
        assert_eq!(
            image.payload,
            ImagePayload::Pixels {
                bytes: vec![255, 0, 0, 255],
                width: 1,
                height: 1,
                format: ImagePixelFormat::Rgba8
            }
        );
        assert_eq!(image.destination.height, IMAGE_ROWS);
    }

    #[test]
    fn a_decode_cannot_replace_newer_output_or_outlive_its_width() {
        let mut transcript = fixture(40);
        let pending = prepare_image(
            transcript.engine(),
            &transcript.visuals,
            item(),
            "inline-image",
        )
        .unwrap();
        // A newer visual was accepted for the item meanwhile.
        let first = prepare_image(
            transcript.engine(),
            &transcript.visuals,
            item(),
            "inline-image",
        )
        .unwrap();
        assert!(accept(&mut transcript, &first));
        let _ = transcript.sync_render(None, 0, Instant::now());
        assert!(!accept(&mut transcript, &pending), "newer output");
        // The body width the decode was measured at changed.
        let pending = prepare_image(
            transcript.engine(),
            &transcript.visuals,
            item(),
            "inline-image",
        )
        .unwrap();
        let _ = transcript.sync_geometry(39, 30, Instant::now());
        assert!(!accept(&mut transcript, &pending), "stale width");
        assert!(
            prepare_image(transcript.engine(), &transcript.visuals, item(), "missing").is_none()
        );
    }

    #[test]
    fn malformed_or_oversized_pixels_are_rejected() {
        let mut transcript = fixture(40);
        let pending = prepare_image(
            transcript.engine(),
            &transcript.visuals,
            item(),
            "inline-image",
        )
        .unwrap();
        let (engine, visuals) = transcript.engine_and_visuals();
        assert!(!accept_image(engine, visuals, &pending, vec![255], 1, 1));
        assert!(!accept_image(engine, visuals, &pending, Vec::new(), 0, 0));
        assert!(!accept_image(
            engine,
            visuals,
            &pending,
            vec![0; 4 * 4096],
            4096,
            1
        ));
    }
}
