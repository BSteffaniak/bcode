//! Markdown visuals of the V2 host: D2 lane bounds, exact-match installation, the result
//! cache, policy notices and image placements registered only for painted, correctly
//! positioned content.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bcode_session_models::{SessionEvent, SessionEventKind, SessionId};
use bcode_tui_transcript::engine::HostRequest;
use bcode_tui_transcript::layout::producers::markdown::MarkdownVisual;
use bcode_tui_transcript::window::HistoryWindowResult;
use bmux_tui::buffer::Buffer;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::image::{ImageContribution, ImagePayload, ImagePixelFormat};
use bmux_tui::paint::PaintCx;

use super::{VisualCompletion, VisualLane, VisualPolicy, VisualWorkers};
use crate::effects::{TuiEffect, TuiEffectResult};
use crate::markdown_image::MarkdownImageLoader;
use crate::transcript_v2_host::TranscriptV2Host;
use crate::transcript_v2_host::effects::HistoryLoaded;
use crate::transcript_v2_host::test_fixtures::{COMPOSER, REGION, event, style};

const POLICY: VisualPolicy = VisualPolicy {
    terminal_images: true,
    network_images: true,
};

fn assistant(session: SessionId, sequence: u64, text: &str) -> SessionEvent {
    event(
        session,
        sequence,
        SessionEventKind::AssistantMessage {
            text: text.to_owned(),
        },
    )
}

/// A host attached to a session of `texts` assistant messages, all resident, with image
/// workers installed.
fn attached(texts: &[&str], at: Instant) -> TranscriptV2Host {
    let session = SessionId::new();
    let mut host = TranscriptV2Host::new();
    host.sync_geometry(REGION, at);
    host.set_composer_area(COMPOSER);
    host.sync_session(Some(session), at);
    let effects = host.drain_effects();
    let [TuiEffect::LoadTranscriptV2History { load }] = effects.as_slice() else {
        panic!("one tail request, got {} effects", effects.len());
    };
    let HostRequest::Window(issued) = load.request else {
        panic!("window request");
    };
    let events = texts
        .iter()
        .zip(1..)
        .map(|(text, sequence)| assistant(session, sequence, text))
        .collect::<Vec<_>>();
    let last = events.last().map(|event| event.sequence);
    let result = TuiEffectResult::TranscriptV2HistoryLoaded {
        load: *load,
        result: Ok(HistoryLoaded::Window(HistoryWindowResult {
            generation: issued.generation,
            target: issued.request,
            events,
            has_older: false,
            has_newer: false,
            first_available_sequence: Some(1),
            last_available_sequence: last,
        })),
    };
    assert!(host.apply_effect_result(result, at));
    host.set_visual_workers(|| VisualWorkers {
        images: Some(MarkdownImageLoader::new().expect("loader")),
        mermaid: None,
    });
    host
}

/// Paint and commit one frame; return the image placements it registered.
fn paint(host: &mut TranscriptV2Host, at: Instant) -> Vec<Rect> {
    host.sync_geometry(REGION, at);
    let area = Rect::new(0, 0, REGION.width, REGION.bottom() + COMPOSER.height + 1);
    let mut buffer = Buffer::empty(area);
    let images = {
        let mut frame = Frame::new(&mut buffer);
        frame.set_clock(Some(bmux_tui::clock::PresentationClock::new(at)));
        host.paint(
            REGION,
            &mut PaintCx::new(&mut frame),
            &style(Duration::ZERO),
            at,
        );
        frame
            .images()
            .iter()
            .filter_map(|image| match image {
                ImageContribution::Present(placement) => Some(placement.destination),
                ImageContribution::Remove(_) => None,
            })
            .collect()
    };
    host.commit();
    images
}

fn pixels() -> Arc<ImagePayload> {
    Arc::new(ImagePayload::Pixels {
        bytes: vec![255; 4],
        width: 1,
        height: 1,
        format: ImagePixelFormat::Rgba8,
    })
}

fn completion(
    host: &mut TranscriptV2Host,
    result: Result<Arc<ImagePayload>, Arc<str>>,
) -> VisualCompletion {
    let work = host.drain_visual_work();
    let [work] = work.as_slice() else {
        panic!("one started request, got {}", work.len());
    };
    VisualCompletion {
        lane: work.lane(),
        request: work.request().clone(),
        result,
    }
}

#[test]
fn d2_one_active_and_one_pending_request_per_lane() {
    let at = Instant::now();
    let mut host = attached(
        &[
            "![one](https://example.com/1.png)\n\n![two](https://example.com/2.png)\n\n![three](https://example.com/3.png)",
        ],
        at,
    );
    assert!(
        paint(&mut host, at).is_empty(),
        "placeholders place nothing"
    );
    assert!(host.schedule_markdown_visuals(POLICY, at));
    let visuals = host.markdown_visuals();
    let active = visuals.active(VisualLane::Image).expect("active").clone();
    let pending = visuals.pending(VisualLane::Image).expect("pending").clone();
    assert!(active.source_key.ends_with("1.png"), "{active:?}");
    assert!(pending.source_key.ends_with("2.png"), "{pending:?}");
    let first = completion(&mut host, Ok(pixels()));
    // Scheduling again starts nothing while the lane is busy and keeps one pending request.
    assert!(!host.schedule_markdown_visuals(POLICY, at));
    assert!(host.drain_visual_work().is_empty());
    assert_eq!(
        host.markdown_visuals()
            .pending(VisualLane::Image)
            .map(|p| p.id),
        Some(pending.id)
    );
    // Completion installs the first image and starts the pending request.
    assert!(host.complete_markdown_visual(first, at));
    assert_eq!(host.markdown_visuals().accepted().len(), 1);
    assert_eq!(
        host.markdown_visuals()
            .active(VisualLane::Image)
            .map(|a| a.id),
        Some(pending.id)
    );
    assert_eq!(host.drain_visual_work().len(), 1);
}

#[test]
fn accepted_image_is_placed_over_its_visible_slot_only() {
    let at = Instant::now();
    let mut host = attached(
        &["Look:\n\n![cat](https://example.com/cat.png)\n\nafter"],
        at,
    );
    let _ = paint(&mut host, at);
    assert!(host.schedule_markdown_visuals(POLICY, at));
    let done = completion(&mut host, Ok(pixels()));
    let contribution = done.request.contribution_id.clone();
    assert!(host.complete_markdown_visual(done, at));
    assert!(host.take_invalidated(), "acceptance is a unit revision");
    let images = paint(&mut host, at);
    let [placement] = images.as_slice() else {
        panic!("one placement, got {images:?}");
    };
    assert!(
        REGION.intersection(*placement) == *placement,
        "placed inside the transcript region: {placement:?}"
    );
    // The placement covers exactly the slot of the painted layout, at the unit's position.
    let frame = host.last_frame().expect("frame").clone();
    let viewport = frame.viewport.as_ref().expect("viewport");
    let (unit, slot) = viewport
        .units
        .iter()
        .find_map(|unit| {
            let layout = host.engine().layouts().current(&unit.key)?;
            let slot = layout
                .geometry
                .visual_slots
                .iter()
                .find(|slot| slot.contribution_id == contribution)?;
            Some((unit.clone(), slot.clone()))
        })
        .expect("visible slot");
    let top = i64::from(REGION.y) + unit.top + i64::try_from(slot.rect.y).expect("row");
    assert_eq!(i64::from(placement.y), top);
    assert_eq!(u64::from(placement.x), u64::from(REGION.x) + slot.rect.x);
    assert!(matches!(
        host.markdown_visuals()
            .accepted()
            .accepted(&contribution)
            .map(|accepted| &accepted.visual),
        Some(MarkdownVisual::Image(_))
    ));
    // A frame painted into an empty region registers no placement.
    let area = Rect::new(0, 0, REGION.width, REGION.bottom() + COMPOSER.height + 1);
    let mut buffer = Buffer::empty(area);
    let mut frame = Frame::new(&mut buffer);
    host.paint(
        Rect::new(0, 1, REGION.width, 0),
        &mut PaintCx::new(&mut frame),
        &style(Duration::ZERO),
        at,
    );
    assert_eq!(frame.images(), [] as [ImageContribution; 0]);
}

#[test]
fn stale_or_mismatched_completions_are_not_installed() {
    let at = Instant::now();
    let mut host = attached(&["![cat](https://example.com/cat.png)"], at);
    let _ = paint(&mut host, at);
    assert!(host.schedule_markdown_visuals(POLICY, at));
    let mut done = completion(&mut host, Ok(pixels()));
    // Not the active request: discarded without touching the lane.
    let mut stale = done.clone();
    stale.request.id += 100;
    assert!(!host.complete_markdown_visual(stale, at));
    assert!(host.markdown_visuals().active(VisualLane::Image).is_some());
    // The active request, but rendered for another layout revision: not installed, cached.
    done.request.revision += 1;
    assert!(!host.complete_markdown_visual(done, at));
    assert!(host.markdown_visuals().accepted().is_empty());
    assert!(host.markdown_visuals().active(VisualLane::Image).is_none());
    // The next schedule installs the cached result without starting work.
    assert!(!host.schedule_markdown_visuals(POLICY, at));
    assert!(host.drain_visual_work().is_empty());
    assert_eq!(host.markdown_visuals().accepted().len(), 1);
}

/// The notice accepted for the first slot of the last frame's visible units, if any.
fn notice(host: &TranscriptV2Host) -> Option<String> {
    let frame = host.last_frame()?;
    let slot = frame.viewport.as_ref()?.units.iter().find_map(|unit| {
        host.engine()
            .layouts()
            .current(&unit.key)?
            .geometry
            .visual_slots
            .first()
            .cloned()
    })?;
    match &host
        .markdown_visuals()
        .accepted()
        .accepted(&slot.contribution_id)?
        .visual
    {
        MarkdownVisual::Notice(text) => Some(text.to_string()),
        MarkdownVisual::Image(_) => None,
    }
}

#[test]
fn failures_and_policy_become_notices_without_work() {
    let at = Instant::now();
    let mut host = attached(&["![cat](https://example.com/cat.png)"], at);
    let _ = paint(&mut host, at);
    let offline = VisualPolicy {
        terminal_images: true,
        network_images: false,
    };
    assert!(!host.schedule_markdown_visuals(offline, at));
    assert!(host.drain_visual_work().is_empty());
    let _ = paint(&mut host, at);
    assert!(
        notice(&host).is_some_and(|text| text.contains("network disabled")),
        "{:?}",
        notice(&host)
    );
    // A failed load becomes the failure notice.
    assert!(host.schedule_markdown_visuals(POLICY, at));
    let failed = completion(&mut host, Err(Arc::from("HTTP 404")));
    assert!(host.complete_markdown_visual(failed, at));
    let _ = paint(&mut host, at);
    assert!(
        notice(&host).is_some_and(|text| text.contains("image failed: HTTP 404")),
        "{:?}",
        notice(&host)
    );
    // Without terminal image support nothing loads and no image is placed.
    let unsupported = VisualPolicy {
        terminal_images: false,
        network_images: true,
    };
    assert!(!host.schedule_markdown_visuals(unsupported, at));
    assert_eq!(paint(&mut host, at), [] as [Rect; 0]);
    assert!(notice(&host).is_some_and(|text| text.contains("unsupported")));
}

#[tokio::test]
async fn image_work_runs_the_bounded_loader_outside_drawing() {
    use image::ImageEncoder as _;

    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("image.png");
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&[0x80; 2 * 3 * 4], 2, 3, image::ExtendedColorType::Rgba8)
        .expect("png");
    std::fs::write(&path, bytes).expect("write");
    let request = super::VisualRequest {
        id: 1,
        unit: bcode_tui_transcript::units::UnitKey::Item(
            bcode_session_view_models::TranscriptViewItemId::new("a"),
        ),
        revision: 0,
        contribution_id: "transcript:item:a:image".to_owned(),
        request: bcode_tui_transcript::layout::producers::markdown::VisualSlotRequest::Image {
            alt: "local".to_owned(),
            source: bcode_markdown_render::MarkdownDestination::LocalPath(path),
            linked_destination: None,
        },
        source_key: String::new(),
    };
    let work = super::VisualWork {
        request,
        job: super::Job::Image {
            loader: MarkdownImageLoader::new().expect("loader"),
            cancellation: crate::markdown_image::MarkdownImageCancellationToken::default(),
        },
    };
    assert_eq!(work.lane(), VisualLane::Image);
    let done = work.run().await;
    let payload = done.result.expect("decoded");
    assert!(matches!(
        *payload,
        ImagePayload::Pixels {
            width: 2,
            height: 3,
            ..
        }
    ));
}

#[test]
fn accepted_visuals_of_units_that_left_the_window_are_dropped() {
    let at = Instant::now();
    let mut host = attached(&["![cat](https://example.com/cat.png)"], at);
    let _ = paint(&mut host, at);
    assert!(host.schedule_markdown_visuals(POLICY, at));
    let done = completion(&mut host, Ok(pixels()));
    assert!(host.complete_markdown_visual(done, at));
    assert_eq!(host.markdown_visuals().accepted().len(), 1);
    // Another session replaces every resident unit.
    host.sync_session(Some(SessionId::new()), at);
    let _ = host.drain_effects();
    let _ = host.schedule_markdown_visuals(POLICY, at);
    assert!(host.markdown_visuals().accepted().is_empty());
    assert!(
        host.engine().render_config().markdown.visuals.is_empty(),
        "the engine no longer holds the pixels"
    );
}
