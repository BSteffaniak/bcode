//! The assistant panel's dock: pointer input reaches the panel only while it is painted, and
//! the docked panel is a member of a panel group with the screen whose divider drags like the
//! review's and keeps the dragged proportion when the terminal resizes.

use super::assistant::{Setup, setup};
use crate::assistant::panel::{
    DOCK_ID, Dock, MAX_WIDTH, MIN_HEIGHT, MIN_WIDTH, SCREEN_MIN_HEIGHT, SCREEN_MIN_WIDTH,
};
use crate::panels::{cells_of, share_of};
use crate::testing::harness::Screen;
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

/// The dock divider's painted handle.
fn divider(screen: &Screen) -> Option<Rect> {
    let id = format!("{DOCK_ID}.divider.0");
    screen
        .hits()
        .iter()
        .find(|hit| hit.id.as_str() == id)
        .map(|hit| hit.area)
}

fn drag(setup: &mut Setup, (x, y): (u16, u16), (to_x, to_y): (u16, u16)) {
    let scenario = &mut setup.scenario;
    scenario.mouse(MouseEventKind::Down(MouseButton::Left), x, y);
    scenario.mouse(MouseEventKind::Drag(MouseButton::Left), to_x, to_y);
    scenario.mouse(MouseEventKind::Up(MouseButton::Left), to_x, to_y);
    setup.settle();
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
    let screen = setup.settle();
    let (rect, _) = panel(&setup, 160, 40).expect("docked");
    let handle = divider(&screen).expect("divider");
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

#[test]
fn the_docked_panel_keeps_its_dragged_proportion_when_the_terminal_resizes() {
    let mut setup = setup(160, 40, false);
    setup.scenario.key("ctrl+k");
    let screen = setup.settle();
    let (rect, dock) = panel(&setup, 160, 40).expect("docked");
    assert_eq!(dock, Dock::Right);
    // The handle is the panel's left border.
    let handle = divider(&screen).expect("divider");
    assert_eq!((handle.x, handle.width), (rect.x, 1));
    assert_eq!(screen.cell(handle.x, 5).unwrap().symbol, "│");
    drag(&mut setup, (handle.x, 10), (handle.x - 10, 10));
    let (dragged, _) = panel(&setup, 160, 40).unwrap();
    assert_eq!(dragged.width, rect.width + 10);
    let right_share = share_of(dragged.width, 160);
    let share = right_share;
    for width in [200_u16, 130, 160] {
        setup.scenario.resize(width, 40);
        let screen = setup.settle();
        let (rect, dock) = panel(&setup, width, 40).unwrap();
        assert_eq!(dock, Dock::Right);
        assert!(
            rect.width.abs_diff(cells_of(share, width)) <= 1,
            "{width}: {rect:?}"
        );
        assert_eq!(divider(&screen).map(|handle| handle.x), Some(rect.x));
    }
    // The limits win: the screen keeps its columns, the panel its maximum.
    setup.scenario.resize(100, 40);
    setup.settle();
    let (rect, _) = panel(&setup, 100, 40).unwrap();
    assert_eq!(rect.x, SCREEN_MIN_WIDTH);
    assert!(rect.width >= MIN_WIDTH);
    setup.scenario.resize(400, 40);
    setup.settle();
    assert_eq!(panel(&setup, 400, 40).unwrap().0.width, MAX_WIDTH);

    // Under the screen the handle is the panel's top row, and the height scales.
    setup.scenario.resize(99, 40);
    let screen = setup.settle();
    let (rect, dock) = panel(&setup, 99, 40).unwrap();
    assert_eq!(dock, Dock::Bottom);
    let handle = divider(&screen).expect("divider");
    assert_eq!((handle.y, handle.height), (rect.y, 1));
    drag(&mut setup, (3, rect.y), (3, rect.y - 4));
    let (dragged, _) = panel(&setup, 99, 40).unwrap();
    assert_eq!(dragged.height, rect.height + 4);
    let share = share_of(dragged.height, 40);
    for height in [60_u16, 30, 40] {
        setup.scenario.resize(99, height);
        let screen = setup.settle();
        let (rect, _) = panel(&setup, 99, height).unwrap();
        assert!(
            rect.height.abs_diff(cells_of(share, height)) <= 1,
            "{height}: {rect:?}"
        );
        assert_eq!(divider(&screen).map(|handle| handle.y), Some(rect.y));
    }
    setup.scenario.resize(99, 16);
    setup.settle();
    let (rect, _) = panel(&setup, 99, 16).unwrap();
    assert_eq!(rect.y, SCREEN_MIN_HEIGHT);
    assert_eq!(rect.height, MIN_HEIGHT);

    // Back on the right: the width it was dragged to there.
    setup.scenario.resize(160, 40);
    setup.settle();
    assert_eq!(
        panel(&setup, 160, 40).unwrap().0.width,
        cells_of(right_share, 160)
    );

    // Tiny areas: no handle outside the frame, none while zoomed.
    for (width, height) in [(99_u16, 15_u16), (40, 5), (11, 3), (1, 1)] {
        setup.scenario.resize(width, height);
        let screen = setup.settle();
        assert_eq!(divider(&screen), None, "{width}x{height}");
        let frame = Rect::new(0, 0, width, height);
        for hit in screen.hits() {
            assert_eq!(frame.intersection(hit.area), hit.area, "{width}x{height}");
        }
    }
}
