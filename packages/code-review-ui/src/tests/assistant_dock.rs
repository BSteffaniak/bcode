//! The assistant panel's dock: pointer input reaches the panel only while it is painted.

use super::assistant::{Setup, setup};
use crate::assistant::panel::Dock;
use bmux_tui::event::{MouseButton, MouseEventKind};
use bmux_tui::geometry::Rect;

/// The panel's cells and dock at the scenario's size, as the app splits it.
fn panel(setup: &Setup, width: u16, height: u16) -> Option<(Rect, Dock)> {
    setup
        .app()
        .assistant()
        .split(Rect::new(0, 0, width, height))
        .panel
}

fn scroll_offset(setup: &Setup) -> u64 {
    setup
        .app()
        .review()
        .and_then(crate::ReviewSurface::scroll_offset)
        .expect("a review on screen")
}

#[test]
fn a_closed_panel_leaves_its_old_cells_to_the_review() {
    let mut setup = setup(160, 20, true);
    setup.scenario.key("ctrl+k");
    setup.settle();
    let (rect, _) = panel(&setup, 160, 20).expect("docked");
    let (x, y) = (rect.x + rect.width / 2, rect.y + rect.height / 2);
    // Closed from the keyboard, then a frame without it.
    setup.scenario.key("ctrl+k");
    setup.settle();
    assert!(!setup.app().assistant().is_open());
    assert!(
        !setup
            .app()
            .assistant()
            .contains(bmux_tui::geometry::Point::new(x, y))
    );

    let before = scroll_offset(&setup);
    setup.scenario.mouse(MouseEventKind::ScrollDown, x, y);
    setup.settle();
    assert!(
        scroll_offset(&setup) > before,
        "the review scrolls under the old panel cells"
    );
    setup.scenario.click(x, y);
    let screen = setup.settle();
    assert!(!setup.app().assistant().is_open(), "{}", screen.text());
    assert!(
        !screen.contains("Assistant"),
        "a click does not reopen it: {}",
        screen.text()
    );
}

#[test]
fn a_panel_closed_before_the_next_frame_takes_no_pointer_input() {
    let mut setup = setup(160, 20, true);
    setup.scenario.key("ctrl+k");
    setup.settle();
    let (rect, _) = panel(&setup, 160, 20).expect("docked");
    let (x, y) = (rect.x + rect.width / 2, rect.y + rect.height / 2);
    // Closed and clicked with no frame painted in between.
    setup.scenario.key("ctrl+k");
    setup.scenario.click(x, y);
    setup.settle();
    assert!(!setup.app().assistant().is_open());
}

#[test]
fn an_open_panel_hidden_on_a_small_area_takes_no_pointer_input() {
    let mut setup = setup(99, 15, true);
    setup.scenario.key("ctrl+k");
    let screen = setup.settle();
    assert_eq!(
        panel(&setup, 99, 15).map(|(_, dock)| dock),
        Some(Dock::Zoomed)
    );
    assert!(screen.contains("Assistant"));
    // The screen takes the keyboard: the zoomed panel hides but stays open.
    setup.scenario.key("esc");
    setup.settle();
    assert!(setup.app().assistant().is_open());
    assert_eq!(panel(&setup, 99, 15), None);

    let before = scroll_offset(&setup);
    setup.scenario.mouse(MouseEventKind::ScrollDown, 50, 8);
    setup.settle();
    assert!(scroll_offset(&setup) > before, "the review scrolls");
    setup.scenario.click(50, 8);
    let screen = setup.settle();
    assert!(
        !setup.app().assistant().is_focused(),
        "a click on the review does not bring the panel back: {}",
        screen.text()
    );
    assert!(setup.app().assistant().is_open());
}

#[test]
fn a_divider_drag_ends_when_the_panel_closes() {
    let mut setup = setup(160, 40, true);
    setup.scenario.key("ctrl+k");
    setup.settle();
    let (rect, _) = panel(&setup, 160, 40).expect("docked");
    // The divider is the panel's left edge.
    let handle = rect;
    setup
        .scenario
        .mouse(MouseEventKind::Down(MouseButton::Left), handle.x, 10);
    setup.scenario.key("ctrl+k");
    setup.settle();
    assert!(!setup.app().assistant().is_open());
    setup
        .scenario
        .mouse(MouseEventKind::Drag(MouseButton::Left), handle.x - 20, 10);
    setup
        .scenario
        .mouse(MouseEventKind::Up(MouseButton::Left), handle.x - 20, 10);
    setup.settle();
    assert!(!setup.app().assistant().is_open());
    // Reopened, the panel has the width it had: the drag did not go on while it was closed.
    setup.scenario.key("ctrl+k");
    setup.settle();
    assert_eq!(panel(&setup, 160, 40).map(|(rect, _)| rect), Some(rect));
}
