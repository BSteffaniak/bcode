//! Deterministic state, paint and committed interaction tests; no provider or subprocess.
use super::*;
use bmux_tui::{buffer::Buffer, frame::Frame, geometry::Point, paint::PaintCx};

fn paint(app: &App, width: u16, height: u16) -> (Buffer, bmux_tui::hit::HitMap) {
    app.size.set((width, height));
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
    let mut frame = Frame::new(&mut buffer);
    let tree = app.tree();
    let layout = tree.layout(
        Constraints::tight(Size::new(width, height)),
        &mut LayoutCx::new(),
    );
    tree.paint(&layout, &mut PaintCx::new(&mut frame));
    app.layout.replace(Some(layout));
    let hits = frame.hits().clone();
    drop(frame);
    (buffer, hits)
}

#[test]
fn old_preview_completion_is_rejected_after_navigation() {
    let mut app = App::new(TuiOptions::default());
    app.preview_generation = 2;
    app.update(RuntimeEvent::Message(Message::Preview(
        1,
        Box::new(Err("stale".into())),
    )))
    .unwrap();
    assert!(!app.status.contains("stale"));
    assert!(app.preview.is_none());
}

#[test]
fn text_rows_inherit_opaque_semantic_surfaces() {
    let app = App::new(TuiOptions::default());
    let (buffer, _) = paint(&app, 120, 36);
    for cell in buffer
        .cells()
        .iter()
        .filter(|cell| !cell.symbol.trim().is_empty())
    {
        assert!(
            cell.style.bg.is_some(),
            "visible glyph lost its semantic surface: {:?}",
            cell.symbol
        );
    }
}

#[test]
fn model_failure_guidance_is_actionable_without_inventing_a_provider_cause() {
    let error = application::Error::Engine(bcode_recap::engine::EngineError::Composition(
        bcode_recap::composition::CompositionError::Model(
            bcode_recap::model::ModelError::ExecutionFailed,
        ),
    ));
    let message = operation_error(&error);
    assert!(message.contains("not confirmed"));
    assert!(message.contains("No automatic retry"));
    assert!(message.contains("profile"));
}

#[test]
fn launch_is_local_and_never_admits_a_model() {
    let app = App::new(TuiOptions::default());
    assert!(app.active.is_none());
    assert!(app.document.is_none());
    assert_eq!(app.sequence, 0);
    assert!(app.options.model_config.is_none());
}

#[test]
fn empty_generate_is_validation_not_an_effect() {
    let mut app = App::new(TuiOptions::default());
    let update = app.action("generate");
    assert!(update.commands.is_empty());
    assert!(update.subscriptions.is_empty());
    assert!(app.active.is_none());
    assert!(app.status.contains("source") || app.status.contains("video"));
}

#[test]
fn stale_completion_cannot_replace_reader_or_status() {
    let mut app = App::new(TuiOptions::default());
    let before = app.status.clone();
    app.update(RuntimeEvent::Message(Message::Settled(
        99,
        Box::new(Err("stale failure".into())),
    )))
    .unwrap();
    assert_eq!(app.status, before);
    assert_eq!(app.screen, Screen::Home);
}

#[test]
fn every_hit_is_visible_and_confined_at_tiny_and_unicode_widths() {
    for (width, height) in [
        (0, 0),
        (1, 1),
        (12, 5),
        (33, 12),
        (34, 13),
        (60, 24),
        (90, 32),
        (140, 42),
    ] {
        let app = App::new(TuiOptions {
            source: Some("/video/日本語 👩🏽‍💻 e\u{301}.mp4".into()),
            ..TuiOptions::default()
        });
        let (_, hits) = paint(&app, width, height);
        for region in hits.regions() {
            assert!(region.area.right() <= width);
            assert!(region.area.bottom() <= height);
            assert!(!region.area.is_empty());
        }
    }
}

#[test]
fn home_uses_layered_editorial_surfaces_and_real_source_control() {
    let app = App::new(TuiOptions::default());
    let (buffer, hits) = paint(&app, 120, 34);
    assert!(
        hits.regions()
            .iter()
            .any(|r| r.id.as_str() == "source.input")
    );
    assert!(hits.regions().iter().any(|r| r.id.as_str() == "generate"));
    let backgrounds: std::collections::BTreeSet<_> = buffer
        .cells()
        .iter()
        .map(|c| format!("{:?}", c.style.bg))
        .collect();
    assert!(backgrounds.len() >= 2);
    assert!(buffer.get(Point::new(0, 0)).is_some());
}

#[test]
fn options_and_library_are_reachable_without_inference() {
    let mut app = App::new(TuiOptions::default());
    app.action("options");
    let (_, hits) = paint(&app, 80, 30);
    assert!(
        hits.regions()
            .iter()
            .any(|r| r.id.as_str() == "transcript.input")
    );
    app.action("chapters");
    let (_, hits) = paint(&app, 80, 30);
    assert!(
        !hits
            .regions()
            .iter()
            .any(|r| r.id.as_str() == "source.input")
    );
    assert!(app.active.is_none());
}

#[test]
fn committed_source_and_options_focus_accept_unicode_without_shortcut_collisions() {
    let mut app = App::new(TuiOptions::default());
    let (_, hits) = paint(&app, 120, 36);
    commit_app(&mut app, &hits, &bmux_tui::focus::FocusTrap::new());
    assert_eq!(
        app.router.focused().map(bmux_tui::hit::HitId::as_str),
        Some("source.input")
    );
    for character in "日本語 🌱 café".chars() {
        app.terminal(&Event::Key(bmux_keyboard::KeyStroke::simple(
            KeyCode::Char(character),
        )));
        let (_, hits) = paint(&app, 120, 36);
        commit_app(&mut app, &hits, &bmux_tui::focus::FocusTrap::new());
    }
    assert_eq!(app.text(0), "日本語 🌱 café");
    app.terminal(&Event::Key(bmux_keyboard::KeyStroke::simple(KeyCode::F(4))));
    let (_, hits) = paint(&app, 120, 36);
    commit_app(&mut app, &hits, &bmux_tui::focus::FocusTrap::new());
    for character in "captions.vtt".chars() {
        app.terminal(&Event::Key(bmux_keyboard::KeyStroke::simple(
            KeyCode::Char(character),
        )));
    }
    assert_eq!(app.text(1), "captions.vtt");
    app.terminal(&Event::Key(bmux_keyboard::KeyStroke::simple(KeyCode::Tab)));
    let (_, hits) = paint(&app, 120, 36);
    commit_app(&mut app, &hits, &bmux_tui::focus::FocusTrap::new());
    for character in "Compare motion".chars() {
        app.terminal(&Event::Key(bmux_keyboard::KeyStroke::simple(
            KeyCode::Char(character),
        )));
    }
    assert_eq!(app.text(2), "Compare motion");
}
