//! The assistant in a review: a member of the review body's panels (its own column at the far
//! right, or under the document), inside the review's header and footer, on the review's `tab`
//! cycle, maximized over the body, sized with the app (or the review, as configured), and with
//! the divider on its edge the review's.

use super::assistant::{Setup, setup, setup_with};
use crate::assistant::profile::AssistantConfig;
use crate::testing::backend::TestBackend;
use bmux_tui::event::{MouseButton, MouseEventKind};
use bmux_tui::geometry::Rect;

/// Where the assistant was painted in the last frame.
fn panel(setup: &Setup) -> Option<Rect> {
    let size = setup.scenario.size();
    setup
        .app()
        .assistant()
        .split(Rect::new(0, 0, size.width, size.height))
        .panel
        .map(|(rect, _)| rect)
}

fn configured(toml: &str, width: u16, height: u16) -> Setup {
    let (config, problems) =
        AssistantConfig::from_plugin_table(&format!("[assistant]\n{toml}").parse().expect("toml"));
    assert!(problems.is_empty(), "{problems:?}");
    setup_with(
        TestBackend::default().with_assistant_config(config),
        width,
        height,
        true,
    )
}

/// The handle of the review body's divider before the assistant, as painted.
fn divider(setup: &mut Setup) -> Rect {
    let screen = setup.settle();
    let rect = panel(setup).expect("the panel");
    screen
        .hits()
        .iter()
        .filter(|hit| hit.id.as_str().starts_with("review.panels.divider."))
        .map(|hit| hit.area)
        .find(|area| area.x == rect.x)
        .unwrap_or_else(|| panic!("no divider on the panel's edge:\n{}", screen.text()))
}

fn drag(setup: &mut Setup, from: (u16, u16), to: (u16, u16)) {
    let scenario = &mut setup.scenario;
    scenario.mouse(MouseEventKind::Down(MouseButton::Left), from.0, from.1);
    scenario.mouse(MouseEventKind::Drag(MouseButton::Left), to.0, to.1);
    scenario.mouse(MouseEventKind::Up(MouseButton::Left), to.0, to.1);
    setup.settle();
}

#[test]
fn the_assistant_is_a_panel_of_the_review_body_inside_its_header_and_footer() {
    let mut setup = setup(160, 40, true);
    setup.scenario.key("ctrl+k");
    let screen = setup.settle();
    let rect = panel(&setup).expect("shown");
    // The review's header and footer span the whole width, the panel sits between them at the
    // far right, its top row level with the review's other panels.
    assert!(rect.y > 0, "{rect:?}\n{}", screen.text());
    assert_eq!(rect.right(), 160, "{rect:?}");
    assert!(rect.bottom() < 40, "{rect:?}");
    let header = screen.row_text(0);
    assert!(header.contains("Publish"), "{header}");
    assert!(header.trim_end().ends_with("Ask AI"), "{header}");
    let review = setup
        .app()
        .review()
        .and_then(|review| review.body_area(crate::surface::REVIEW_PANE_ID))
        .expect("the document");
    assert_eq!(review.y, rect.y, "{review:?} {rect:?}");
    // Nothing of the review takes pointer input on the panel, except the divider on its edge.
    for hit in screen.hits() {
        let id = hit.id.as_str();
        if !id.starts_with("review.") || id.starts_with("review.panels.divider.") {
            continue;
        }
        assert!(
            hit.area.intersection(rect).is_empty(),
            "{id} at {:?} inside the panel at {rect:?}",
            hit.area
        );
    }

    // Narrower than its column needs, it sits under the document, over the footer.
    setup.scenario.resize(90, 40);
    let screen = setup.settle();
    let rect = panel(&setup).expect("shown");
    assert!(
        rect.x == 0 && rect.width == 90,
        "{rect:?}\n{}",
        screen.text()
    );
    assert!(rect.bottom() < 40, "{rect:?}");
    assert!(screen.row_text(0).trim_end().ends_with("Ask AI"));
}

#[test]
fn tab_moves_through_the_review_panels_to_the_assistant_and_back() {
    let mut setup = setup(220, 40, true);
    setup.scenario.key("shift+t");
    setup.scenario.key("ctrl+k");
    setup.settle();
    setup.scenario.key("esc");
    setup.settle();
    assert!(!setup.app().assistant().is_focused());
    // The document, the file panel, the threads panel, then the assistant.
    for _ in 0..2 {
        setup.scenario.key("tab");
        setup.settle();
        assert!(!setup.app().assistant().is_focused());
    }
    setup.scenario.key("tab");
    let screen = setup.settle();
    assert!(setup.app().assistant().is_focused(), "{}", screen.text());
    setup.scenario.key("tab");
    setup.settle();
    assert!(!setup.app().assistant().is_focused());
    // `tab` moved the keyboard on; it typed nothing.
    let typed = setup
        .app()
        .assistant()
        .input()
        .borrow()
        .buffer()
        .text()
        .to_string();
    assert_eq!(typed, "");
    assert!(setup.app().assistant().is_open());
    // Minimized, it is skipped.
    setup.scenario.key("ctrl+k");
    setup.scenario.key("alt+c");
    setup.settle();
    assert!(setup.app().assistant().minimized());
    for _ in 0..4 {
        setup.scenario.key("tab");
        setup.settle();
        assert!(!setup.app().assistant().is_focused());
    }
}

#[test]
fn maximized_the_assistant_takes_the_review_body_and_keeps_its_header() {
    let mut setup = setup(160, 40, true);
    setup.scenario.key("ctrl+k");
    let screen = setup.settle();
    let docked = panel(&setup).expect("shown");
    let files = setup
        .app()
        .review()
        .and_then(|review| review.body_area(crate::surface::FILES_PANE_ID));
    assert!(files.is_some(), "{}", screen.text());
    setup.scenario.key("alt+z");
    let screen = setup.settle();
    let rect = panel(&setup).expect("shown");
    assert_eq!(rect.y, docked.y, "{rect:?}");
    assert_eq!((rect.x, rect.width), (0, 160), "{rect:?}");
    assert!(screen.row_text(0).contains("Publish"), "{}", screen.text());
    // No other panel is laid out meanwhile.
    let review = setup.app().review().expect("review");
    assert!(review.body_area(crate::surface::FILES_PANE_ID).is_none());
    assert!(review.body_area(crate::surface::REVIEW_PANE_ID).is_none());
    // Back to the screen, it is restored in place.
    setup.scenario.key("esc");
    setup.settle();
    assert_eq!(panel(&setup), Some(docked));
}

#[test]
fn dragging_its_edge_sizes_the_assistant_for_the_app_by_default() {
    let mut setup = setup(200, 40, true);
    setup.scenario.key("ctrl+k");
    setup.settle();
    let before = panel(&setup).expect("shown");
    let handle = divider(&mut setup);
    drag(
        &mut setup,
        (handle.x, handle.y + 5),
        (handle.x - 10, handle.y + 5),
    );
    let after = panel(&setup).expect("shown");
    assert_eq!(after.width, before.width + 10, "{before:?} -> {after:?}");
    assert_eq!(after.right(), 200);
    // The app keeps it: the review saves no size for it, and home docks it as wide.
    let state = setup.app().review().expect("review").presentation();
    assert_eq!(state.panels.assistant_right_share, None);
    setup.scenario.key("esc");
    setup.scenario.key("q");
    let screen = setup.settle();
    let home = panel(&setup).expect("open on home");
    assert_eq!(home.width, after.width, "{}", screen.text());
}

#[test]
fn configured_per_review_the_assistant_keeps_its_size_with_the_review() {
    let mut setup = configured("size_scope = \"review\"", 200, 40);
    setup.scenario.key("ctrl+k");
    setup.settle();
    let before = panel(&setup).expect("shown");
    let handle = divider(&mut setup);
    drag(
        &mut setup,
        (handle.x, handle.y + 5),
        (handle.x - 10, handle.y + 5),
    );
    let after = panel(&setup).expect("shown");
    assert_eq!(after.width, before.width + 10, "{before:?} -> {after:?}");
    let state = setup.app().review().expect("review").presentation();
    assert_eq!(
        state.panels.assistant_right_share,
        Some(crate::panels::share_of(after.width, 200))
    );
    // The app's own size is unchanged: home docks it at its configured width.
    setup.scenario.key("esc");
    setup.scenario.key("q");
    setup.settle();
    let home = panel(&setup).expect("open on home");
    assert_eq!(home.width, before.width);
}

#[test]
fn an_unknown_size_scope_is_reported_and_the_apps_size_used() {
    let (config, problems) =
        AssistantConfig::from_plugin_table(&"[assistant]\nsize_scope = \"panel\"".parse().unwrap());
    assert!(!config.size_per_review);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("size_scope"), "{problems:?}");
}
