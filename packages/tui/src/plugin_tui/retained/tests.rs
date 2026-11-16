use super::*;
use bcode_plugin_sdk::definition::{PluginFuture, PluginResult, ProductExtensionPort};
use bmux_plugin::component_viewport::portable::PortablePresentation;
use bmux_tui::{
    buffer::Buffer,
    frame::Frame,
    hit::HitRegion,
    image::{
        ImageContribution, ImageFit, ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat,
        ImagePlacement,
    },
    selection::{SelectionFragment, SelectionScope},
    semantic::SemanticRegion,
    style::Style,
};

struct Owner;
impl ProductExtensionPort for Owner {
    fn ensure_active(&self) -> PluginResult<()> {
        Ok(())
    }
    fn invoke(
        &self,
        _contract: String,
        _payload: Vec<u8>,
    ) -> PluginFuture<'_, PluginResult<Vec<u8>>> {
        panic!("retained replay must not invoke plugin code")
    }
}

fn frontend() -> (Arc<RetainedFrontend>, Arc<Owner>) {
    let owner = Arc::new(Owner);
    let selected = ProductExtensions::new(owner.clone(), None);
    let view = RetainedFrontend::from_owner(
        selected,
        "test.frontend",
        7,
        PortablePresentationLimits::default(),
    )
    .unwrap();
    (view, owner)
}

fn publish(view: &RetainedFrontend) -> FrontendResourceId {
    let FrontendPresentationResponse::Created { resource } = view
        .apply(FrontendPresentationRequest::Create { generation: 4 })
        .unwrap()
    else {
        panic!("create")
    };
    let producer = bmux_tui::presentation::PresentationState::new(Rect::new(0, 0, 4, 2));
    let mut prepared = producer
        .prepare(bmux_tui::damage::Damage::Full, |cx| {
            cx.set_cell(0, 1, "界", Style::new());
            cx.set_focus_scope(Some(HitId::new("modal")));
            cx.push_hit(HitRegion::new("field", Rect::new(0, 1, 2, 1)).focusable(true));
            cx.push_semantic(SemanticRegion::new("label", Rect::new(0, 1, 2, 1), "text"));
            cx.push_selection_scope(
                SelectionScope::new("source", Rect::new(0, 0, 4, 2)).revision(1),
            );
            cx.push_selection_fragment(
                SelectionFragment::new("source", "content", Rect::new(0, 1, 2, 1), 0, 0..3)
                    .id("fragment")
                    .revision(1),
            );
            cx.push_image(ImageContribution::Present(ImagePlacement {
                key: ImageKey::new("image"),
                payload: ImagePayload::Pixels {
                    bytes: vec![255, 0, 0, 255],
                    width: 1,
                    height: 1,
                    format: ImagePixelFormat::Rgba8,
                },
                destination: Rect::new(2, 1, 1, 1),
                clip: Rect::new(2, 1, 1, 1),
                allocation: None,
                fit: ImageFit::Contain,
                lifecycle: ImageLifecycle::Frame,
            }));
            cx.set_cursor(bmux_tui::geometry::Point::new(1, 1), true);
        })
        .expect("producer prepared");
    prepared
        .set_selection_text(BTreeMap::from([(
            SelectionContentId::new("content"),
            "界".to_owned(),
        )]))
        .unwrap();
    assert!(prepared.set_modal_scopes(&[HitId::new("modal")]));
    let presentation = PortablePresentation::from_prepared(&prepared);
    view.apply(FrontendPresentationRequest::Publish {
        resource,
        generation: 4,
        revision: 1,
        presentation: Box::new(presentation),
    })
    .unwrap();
    resource
}

fn paint(
    view: &RetainedFrontend,
    id: FrontendResourceId,
) -> (
    HitId,
    bmux_tui::hit::HitMap,
    bmux_tui::selection::SelectionScene,
) {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 6));
    let mut frame = Frame::new(&mut buffer);
    view.paint(
        (id, 4, 1),
        Rect::new(3, 2, 4, 1),
        1,
        &mut PaintCx::new(&mut frame),
    )
    .unwrap();
    let hits = frame.hits().regions();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].area, Rect::new(3, 2, 2, 1));
    assert!(frame.focus_scope().is_some());
    assert_eq!(frame.semantics().regions()[0].area, Rect::new(3, 2, 2, 1));
    assert_eq!(frame.selection().fragments()[0].area, Rect::new(3, 2, 2, 1));
    assert_eq!(
        frame.cursor().unwrap().position,
        bmux_tui::geometry::Point::new(4, 2)
    );
    let ImageContribution::Present(image) = &frame.images()[0] else {
        panic!("image")
    };
    assert_eq!(image.destination, Rect::new(5, 2, 1, 1));
    (
        hits[0].id.clone(),
        frame.hits().clone(),
        frame.selection().clone(),
    )
}

#[test]
fn only_successful_display_admits_owner_scoped_input() {
    let (view, _owner) = frontend();
    let id = publish(&view);
    view.begin_display();
    let (hit, hits, selection) = paint(&view, id);
    assert!(view.local_hit(id, 4, 1, &hit).is_err());
    view.display_succeeded(&hits, &selection).unwrap();
    let content = &selection.fragments()[0].content_id;
    assert_eq!(view.selection_text(content).as_deref(), Some("界"));
    let (local, area, offset) = view.local_hit(id, 4, 1, &hit).unwrap();
    assert_eq!(local, "field");
    assert_eq!(area, Rect::new(3, 2, 4, 1));
    assert_eq!(offset, 1);
    assert!(view.local_hit(id, 5, 1, &hit).is_err());
    assert!(view.local_hit(id, 4, 2, &hit).is_err());
    view.display_failed();
    assert!(view.selection_text(content).is_none());
    assert!(view.local_hit(id, 4, 1, &hit).is_err());
    view.begin_display();
    let (hit, hits, selection) = paint(&view, id);
    view.display_succeeded(&hits, &selection).unwrap();
    view.close();
    assert!(view.local_hit(id, 4, 1, &hit).is_err());
}

#[test]
fn fully_clipped_paint_never_admits_input() {
    let (view, _) = frontend();
    let id = publish(&view);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 2));
    let mut frame = Frame::new(&mut buffer);
    view.begin_display();
    view.paint(
        (id, 4, 1),
        Rect::new(10, 10, 4, 1),
        1,
        &mut PaintCx::new(&mut frame),
    )
    .unwrap();
    view.display_succeeded(frame.hits(), frame.selection())
        .unwrap();
    assert!(view.state.lock().unwrap().displayed.is_empty());
    view.close();
}

#[test]
fn output_occlusion_and_conflicting_placements_fail_closed() {
    let (view, _) = frontend();
    let id = publish(&view);
    view.begin_display();
    let (hit, _, _) = paint(&view, id);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 6));
    let mut frame = Frame::new(&mut buffer);
    assert!(
        view.paint(
            (id, 4, 1),
            Rect::new(4, 2, 4, 1),
            1,
            &mut PaintCx::new(&mut frame)
        )
        .is_err()
    );
    // Successful terminal output without this resource's hit (e.g. covered by a modal)
    // must never route the producer's formerly painted target.
    view.display_succeeded(frame.hits(), frame.selection())
        .unwrap();
    assert!(view.local_hit(id, 4, 1, &hit).is_err());
    view.close();
}

#[test]
fn v2_prepared_capture_merges_modal_sources_and_revokes_old_input_lease() {
    let (view, _) = frontend();
    let id = publish(&view);
    view.begin_display();
    let host = bmux_tui::presentation::PresentationState::new(Rect::new(0, 0, 12, 6));
    let mut prepared = host
        .prepare(bmux_tui::damage::Damage::Full, |cx| {
            view.paint((id, 4, 1), Rect::new(3, 2, 4, 1), 1, cx)
                .unwrap();
        })
        .unwrap();
    let mut text = prepared.selection_text().clone();
    let mut scopes = Vec::new();
    view.merge_prepared(&mut text, &mut scopes).unwrap();
    prepared.set_selection_text(text).unwrap();
    assert!(prepared.set_modal_scopes(&scopes));
    assert_eq!(prepared.modal_scopes().count(), 1);
    assert_eq!(
        prepared
            .selection_text()
            .values()
            .next()
            .map(String::as_str),
        Some("界")
    );
    let capture = PortablePresentation::from_prepared(&prepared);
    assert_eq!(capture.version, 2);
    assert_eq!(capture.modal_scopes.len(), 1);
    view.display_succeeded(prepared.hits(), prepared.selection())
        .unwrap();
    let lease = view
        .state
        .lock()
        .unwrap()
        .resources
        .as_ref()
        .unwrap()
        .committed_for_input(id, 4, 1)
        .unwrap();
    assert!(!lease.selection_text().is_empty());
    let mut newer = capture.clone();
    newer.version = 2;
    view.apply(FrontendPresentationRequest::Publish {
        resource: id,
        generation: 4,
        revision: 2,
        presentation: Box::new(newer),
    })
    .unwrap();
    view.begin_display();
    let mut output = host
        .prepare(bmux_tui::damage::Damage::Full, |cx| {
            view.paint((id, 4, 2), Rect::new(3, 2, 4, 1), 1, cx)
                .unwrap();
        })
        .unwrap();
    let mut sources = output.selection_text().clone();
    let mut modals = Vec::new();
    view.merge_prepared(&mut sources, &mut modals).unwrap();
    output.set_selection_text(sources).unwrap();
    assert!(output.set_modal_scopes(&modals));
    view.display_succeeded(output.hits(), output.selection())
        .unwrap();
    assert!(lease.selection_text().is_empty());
    assert_eq!(lease.modal_scopes(), [] as [HitId; 0]);
    let current = view
        .state
        .lock()
        .unwrap()
        .resources
        .as_ref()
        .unwrap()
        .committed_for_input(id, 4, 2)
        .unwrap();
    assert!(!current.selection_text().is_empty());
    view.display_failed();
    assert!(current.selection_text().is_empty());
    assert!(lease.selection_text().is_empty());
    assert_eq!(lease.modal_scopes(), [] as [HitId; 0]);
    let mut legacy = capture;
    legacy.version = 1;
    assert!(
        view.apply(FrontendPresentationRequest::Publish {
            resource: id,
            generation: 4,
            revision: 2,
            presentation: Box::new(legacy),
        })
        .is_err()
    );
    view.close();
}

#[test]
fn prepared_capture_keeps_persistent_images_and_host_occlusion() {
    let mut producer = bmux_tui::presentation::PresentationState::new(Rect::new(0, 0, 4, 2));
    producer
        .present(
            bmux_tui::damage::Damage::Full,
            |cx| {
                cx.push_image(ImageContribution::Present(ImagePlacement {
                    key: ImageKey::new("persistent"),
                    payload: ImagePayload::Pixels {
                        bytes: vec![255, 0, 0, 255],
                        width: 1,
                        height: 1,
                        format: ImagePixelFormat::Rgba8,
                    },
                    destination: Rect::new(0, 0, 1, 1),
                    clip: Rect::new(0, 0, 1, 1),
                    allocation: None,
                    fit: ImageFit::Contain,
                    lifecycle: ImageLifecycle::Persistent,
                }));
            },
            |_, _| Ok::<(), ()>(()),
        )
        .unwrap();
    let prepared = producer
        .prepare(bmux_tui::damage::Damage::Full, |cx| {
            cx.occlude(bmux_tui::paint::LocalRect::new(2, 0, 1, 1));
        })
        .unwrap();
    let capture = PortablePresentation::from_prepared(&prepared);
    assert_eq!(capture.images.len(), 1);
    assert_eq!(capture.occlusions, vec![(0, Rect::new(2, 0, 1, 1))]);
    capture
        .validate(PortablePresentationLimits::default())
        .unwrap();
}

#[test]
fn retained_image_frame_uses_authoritative_configure_path() {
    let (view, _) = frontend();
    let resource = publish(&view);
    let presentation = crate::plugin_tui::PluginTuiPresentation::new(
        bcode_plugin::PluginHost::load_static_plugins(&[]).unwrap(),
    );
    presentation.retain_frontend(Arc::clone(&view)).unwrap();
    presentation.begin_retained_display();
    let host = bmux_tui::presentation::PresentationState::new(Rect::new(0, 0, 12, 6));
    let mut prepared = host
        .prepare(bmux_tui::damage::Damage::Full, |cx| {
            view.paint((resource, 4, 1), Rect::new(3, 2, 4, 1), 1, cx)
                .unwrap();
        })
        .unwrap();
    presentation
        .configure_retained_prepared(&mut prepared)
        .unwrap();
    assert_eq!(prepared.selection_text().len(), 1);
    assert_eq!(prepared.modal_scopes().count(), 1);
    assert_eq!(prepared.image_scene().placements().count(), 1);
    let captured = PortablePresentation::from_prepared(&prepared);
    captured
        .validate(PortablePresentationLimits::default())
        .unwrap();
    assert_eq!(captured.text.len(), 1);
    assert_eq!(captured.images.len(), 1);
    assert!(
        view.selection_text(&prepared.selection().fragments()[0].content_id)
            .is_none()
    );
    presentation.commit_retained_display(prepared.hits(), prepared.selection());
    assert!(
        presentation
            .retained_selection_text(&prepared.selection().fragments()[0].content_id)
            .is_some()
    );
    presentation.fail_retained_display();
    assert!(
        presentation
            .retained_selection_text(&prepared.selection().fragments()[0].content_id)
            .is_none()
    );
    drop(presentation);
    drop(view);
}

#[test]
fn dynamic_wire_channel_publishes_same_rich_projection_without_commit_authority() {
    use bmux_plugin::component_viewport::portable::PORTABLE_PRESENTATION_CONTRACT_ID;
    let (view, _) = frontend();
    let channel = view.producer();
    let created = channel
        .extension(
            PORTABLE_PRESENTATION_CONTRACT_ID,
            &serde_json::to_vec(&FrontendPresentationRequest::Create { generation: 9 }).unwrap(),
        )
        .unwrap();
    let FrontendPresentationResponse::Created { resource } = created else {
        panic!("resource")
    };
    let producer = bmux_tui::presentation::PresentationState::new(Rect::new(0, 0, 4, 1));
    let mut scene = producer
        .prepare(bmux_tui::damage::Damage::Full, |cx| {
            cx.set_cell(0, 0, "界", Style::new());
            cx.push_selection_scope(SelectionScope::new("source", Rect::new(0, 0, 4, 1)));
            cx.push_selection_fragment(SelectionFragment::new(
                "source",
                "source",
                Rect::new(0, 0, 2, 1),
                0,
                0..3,
            ));
            cx.push_hit(HitRegion::new("edit", Rect::new(0, 0, 2, 1)).focusable(true));
        })
        .unwrap();
    scene
        .set_selection_text(BTreeMap::from([(
            SelectionContentId::new("source"),
            "界".to_owned(),
        )]))
        .unwrap();
    let snapshot = PortablePresentation::from_prepared(&scene);
    let publish = FrontendPresentationRequest::Publish {
        resource,
        generation: 9,
        revision: 1,
        presentation: Box::new(snapshot),
    };
    assert_eq!(
        channel
            .extension(
                PORTABLE_PRESENTATION_CONTRACT_ID,
                &serde_json::to_vec(&publish).unwrap()
            )
            .unwrap(),
        FrontendPresentationResponse::Published
    );
    view.begin_display();
    let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
    let mut frame = Frame::new(&mut buffer);
    view.paint(
        (resource, 9, 1),
        Rect::new(0, 0, 4, 1),
        0,
        &mut PaintCx::new(&mut frame),
    )
    .unwrap();
    let hit = frame.hits().regions()[0].id.clone();
    assert!(view.local_hit(resource, 9, 1, &hit).is_err());
    view.display_succeeded(frame.hits(), frame.selection())
        .unwrap();
    assert_eq!(view.local_hit(resource, 9, 1, &hit).unwrap().0, "edit");
    assert!(
        channel
            .extension("bmux.presentation.retained.v1", b"{}")
            .is_err()
    );
    view.close();
    assert!(
        channel
            .extension(
                PORTABLE_PRESENTATION_CONTRACT_ID,
                &serde_json::to_vec(&publish).unwrap()
            )
            .is_err()
    );
}

#[test]
fn producer_channel_retirement_is_not_extended_by_cloned_observers() {
    let (view, _) = frontend();
    let producer = view.producer();
    let observer = producer.clone();
    assert!(
        producer
            .apply(FrontendPresentationRequest::Create { generation: 1 })
            .is_ok()
    );
    view.close();
    assert!(
        producer
            .apply(FrontendPresentationRequest::Create { generation: 1 })
            .is_err()
    );
    drop(view);
    assert!(
        observer
            .apply(FrontendPresentationRequest::Create { generation: 1 })
            .is_err()
    );
}

#[test]
fn hidden_destroyed_and_other_instance_hits_never_gain_authority() {
    let (first, _) = frontend();
    let (second, _) = frontend();
    let id = publish(&first);
    let second_id = publish(&second);
    first.begin_display();
    let (hit, hits, selection) = paint(&first, id);
    first.display_succeeded(&hits, &selection).unwrap();
    // No display receipt in the second frontend despite identical resource counters.
    assert!(second.local_hit(second_id, 4, 1, &hit).is_err());
    second.begin_display();
    let (second_hit, second_hits, second_selection) = paint(&second, second_id);
    second
        .display_succeeded(&second_hits, &second_selection)
        .unwrap();
    assert_ne!(hit, second_hit);
    assert!(second.local_hit(second_id, 4, 1, &hit).is_err());
    first.begin_display();
    first.display_succeeded(&hits, &selection).unwrap();
    assert!(first.local_hit(id, 4, 1, &hit).is_err());
    first.begin_display();
    let (hit, hits, selection) = paint(&first, id);
    first.display_succeeded(&hits, &selection).unwrap();
    first
        .apply(FrontendPresentationRequest::Destroy { resource: id })
        .unwrap();
    assert!(first.local_hit(id, 4, 1, &hit).is_err());
    first.close();
    assert!(
        first
            .apply(FrontendPresentationRequest::Create { generation: 4 })
            .is_err()
    );
}
