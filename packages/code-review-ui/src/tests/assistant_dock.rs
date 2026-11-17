//! The assistant panel's dock: pointer input reaches the panel only while it is painted, and
//! the docked panel is a member of a panel group with the screen whose divider drags like the
//! review's and keeps the dragged proportion when the terminal resizes. The panel grows until
//! the screen keeps its minimum, starts at its configured size, and keeps a dragged size across
//! runs (saved once per drag) until it is reset.

use super::app::settled;
use super::assistant::{Setup, setup, setup_with};
use crate::app::ReviewApp;
use crate::assistant::panel::{
    DOCK_ID, Dock, MIN_HEIGHT, MIN_WIDTH, SCREEN_MIN_HEIGHT, SCREEN_MIN_WIDTH,
};
use crate::assistant::profile::{AssistantConfig, DockLength};
use crate::config::ReviewViewConfig;
use crate::entry::Entry;
use crate::panels::{cells_of, share_of};
use crate::testing::backend::TestBackend;
use crate::testing::harness::{Scenario, Screen};
use bcode_code_review_models::{
    GetReviewAssistantPanelSizeRequest, OP_REVIEW_ASSISTANT_PANEL_SIZE_SAVE,
    ReviewAssistantPanelSize,
};
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
    // The limits win: the screen keeps its columns; on a wide area the share applies uncapped.
    setup.scenario.resize(100, 40);
    setup.settle();
    let (rect, _) = panel(&setup, 100, 40).unwrap();
    assert_eq!(rect.x, SCREEN_MIN_WIDTH);
    assert!(rect.width >= MIN_WIDTH);
    setup.scenario.resize(400, 40);
    setup.settle();
    let wide = panel(&setup, 400, 40).unwrap().0.width;
    assert!(
        wide > 100 && wide.abs_diff(cells_of(share, 400)) <= 1,
        "{wide}"
    );

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

/// A drag of the divider at `from` through each of `through`, released at the last.
fn drag_through(setup: &mut Setup, from: (u16, u16), through: &[(u16, u16)]) {
    let scenario = &mut setup.scenario;
    scenario.mouse(MouseEventKind::Down(MouseButton::Left), from.0, from.1);
    for (x, y) in through {
        scenario.mouse(MouseEventKind::Drag(MouseButton::Left), *x, *y);
    }
    let (x, y) = *through.last().expect("a point");
    scenario.mouse(MouseEventKind::Up(MouseButton::Left), x, y);
    setup.settle();
}

/// The assistant configuration read from `toml` (the `assistant` table's body).
fn config(toml: &str) -> (AssistantConfig, Vec<String>) {
    let table: toml::Table = format!("[assistant]\n{toml}").parse().expect("toml");
    AssistantConfig::from_plugin_table(&table)
}

/// Start the app again on home over the same store, with `config` for the assistant.
fn relaunch(setup: &mut Setup, config: AssistantConfig, width: u16, height: u16) {
    let services = setup.backend.services().with_assistant(config);
    let app = ReviewApp::new(
        services,
        setup.review.repo_root.clone(),
        ReviewViewConfig::default(),
        Entry::Home,
    );
    setup.scenario = Scenario::with_shared_host(app, setup.backend.host(), width, height);
    settled(&mut setup.scenario);
}

fn saved_size(setup: &Setup) -> Option<ReviewAssistantPanelSize> {
    setup
        .backend
        .call_now(GetReviewAssistantPanelSizeRequest {
            repo_path: setup.review.repo_root.clone(),
        })
        .size
}

/// The painted divider agrees with the panel: its handle is the panel's left border, drawn
/// there, and no hit region leaves the frame.
fn assert_divider_agrees(setup: &Setup, screen: &Screen, width: u16, height: u16) {
    let (rect, dock) = panel(setup, width, height).expect("docked");
    assert_eq!(dock, Dock::Right);
    let handle = divider(screen).expect("divider");
    assert_eq!((handle.x, handle.width), (rect.x, 1));
    assert_eq!(screen.cell(handle.x, 5).unwrap().symbol, "│");
    let frame = Rect::new(0, 0, width, height);
    for hit in screen.hits() {
        assert_eq!(frame.intersection(hit.area), hit.area);
    }
}

#[test]
fn the_right_dock_grows_past_a_hundred_columns_until_the_screen_keeps_its_minimum() {
    let mut setup = setup(300, 40, false);
    setup.scenario.key("ctrl+k");
    let screen = setup.settle();
    let (rect, _) = panel(&setup, 300, 40).expect("docked");
    assert_eq!(rect.width, 120, "40% by default");
    let handle = divider(&screen).expect("divider");
    drag(&mut setup, (handle.x, 10), (100, 10));
    let screen = setup.settle();
    let (rect, _) = panel(&setup, 300, 40).unwrap();
    assert_eq!(
        (rect.x, rect.width),
        (100, 200),
        "the drag sticks past 100 columns"
    );
    assert_divider_agrees(&setup, &screen, 300, 40);

    // Dragged as far as it goes, the screen keeps its minimum.
    drag(&mut setup, (rect.x, 10), (5, 10));
    let screen = setup.settle();
    let (rect, _) = panel(&setup, 300, 40).unwrap();
    assert_eq!(
        (rect.x, rect.width),
        (SCREEN_MIN_WIDTH, 300 - SCREEN_MIN_WIDTH)
    );
    assert_divider_agrees(&setup, &screen, 300, 40);
    // Narrower areas: the screen still keeps its minimum, the panel its own.
    for width in [250_u16, 160, 100] {
        setup.scenario.resize(width, 40);
        let screen = setup.settle();
        let (rect, _) = panel(&setup, width, 40).unwrap();
        assert_eq!(rect.x, SCREEN_MIN_WIDTH, "{width}");
        assert!(rect.width >= MIN_WIDTH, "{width}");
        assert_divider_agrees(&setup, &screen, width, 40);
    }
    // Wider again: the dragged share, not a fixed cap.
    setup.scenario.resize(300, 40);
    setup.settle();
    assert_eq!(panel(&setup, 300, 40).unwrap().0.x, SCREEN_MIN_WIDTH);

    // The bottom dock has no cap either: it grows until the screen keeps its rows.
    setup.scenario.resize(99, 60);
    setup.settle();
    let (rect, dock) = panel(&setup, 99, 60).unwrap();
    assert_eq!(dock, Dock::Bottom);
    drag(&mut setup, (3, rect.y), (3, 0));
    let (rect, _) = panel(&setup, 99, 60).unwrap();
    assert_eq!(
        (rect.y, rect.height),
        (SCREEN_MIN_HEIGHT, 60 - SCREEN_MIN_HEIGHT)
    );
}

#[test]
fn configured_sizes_apply_as_columns_rows_or_percentages() {
    let cases: [(&str, u16, u16); 5] = [
        // (configuration, right dock width at 300x40, bottom dock height at 99x40)
        ("width = 120\nheight = 12", 120, 12),
        ("width = \"70%\"\nheight = \"50%\"", 210, 20),
        ("width = \" 25 % \"", 75, 16),
        // Beyond the limits: the screen keeps its minimum, the panel its own.
        (
            "width = 500\nheight = 100",
            300 - SCREEN_MIN_WIDTH,
            40 - SCREEN_MIN_HEIGHT,
        ),
        ("width = 1\nheight = \"1%\"", MIN_WIDTH, MIN_HEIGHT),
    ];
    for (toml, width, height) in cases {
        let (config, problems) = config(toml);
        assert_eq!(problems, Vec::<String>::new(), "{toml}");
        let mut setup = setup_with(
            TestBackend::default().with_assistant_config(config),
            300,
            40,
            false,
        );
        setup.scenario.key("ctrl+k");
        setup.settle();
        assert_eq!(panel(&setup, 300, 40).unwrap().0.width, width, "{toml}");
        setup.scenario.resize(99, 40);
        setup.settle();
        let (rect, dock) = panel(&setup, 99, 40).unwrap();
        assert_eq!((dock, rect.height), (Dock::Bottom, height), "{toml}");
    }
}

#[test]
fn invalid_configured_sizes_fall_back_to_forty_percent_with_a_warning() {
    for toml in [
        "width = 0\nheight = -3",
        "width = \"0%\"\nheight = \"150%\"",
        "width = \"60\"\nheight = \"wide\"",
        "width = 1.5\nheight = true",
        "width = 70000\nheight = \"%\"",
    ] {
        let (config, problems) = config(toml);
        assert_eq!(config.width, DockLength::Percent(40), "{toml}");
        assert_eq!(config.height, DockLength::Percent(40), "{toml}");
        assert_eq!(problems.len(), 2, "{toml}: {problems:?}");
        assert!(
            problems[0].contains("assistant.width must be a number of columns or a percentage")
                && problems[0].contains("using \"40%\""),
            "{problems:?}"
        );
        assert!(
            problems[1].contains("assistant.height must be a number of rows"),
            "{problems:?}"
        );
        let mut setup = setup_with(
            TestBackend::default().with_assistant_config(config),
            300,
            40,
            false,
        );
        setup.scenario.key("ctrl+k");
        setup.settle();
        assert_eq!(panel(&setup, 300, 40).unwrap().0.width, 120, "{toml}");
    }
}

#[test]
fn a_dragged_size_is_saved_once_per_drag_and_restored_over_the_configuration_until_reset() {
    let mut setup = setup(250, 40, false);
    setup.scenario.key("ctrl+k");
    let screen = setup.settle();
    let handle = divider(&screen).expect("divider");
    assert_eq!(saved_size(&setup), None);
    // Many drag events, one save when the button is released.
    drag_through(
        &mut setup,
        (handle.x, 10),
        &[(140, 10), (120, 10), (100, 12), (90, 10)],
    );
    assert_eq!(setup.backend.calls(OP_REVIEW_ASSISTANT_PANEL_SIZE_SAVE), 1);
    let (rect, _) = panel(&setup, 250, 40).unwrap();
    assert_eq!(rect.width, 160);
    let saved = saved_size(&setup).expect("saved");
    assert_eq!(saved.right_share, Some(share_of(160, 250)));
    assert_eq!(saved.bottom_share, None);
    // A press on the divider without a move saves nothing.
    setup
        .scenario
        .mouse(MouseEventKind::Down(MouseButton::Left), rect.x, 10);
    setup
        .scenario
        .mouse(MouseEventKind::Up(MouseButton::Left), rect.x, 10);
    setup.settle();
    assert_eq!(setup.backend.calls(OP_REVIEW_ASSISTANT_PANEL_SIZE_SAVE), 1);
    // A drag cut short by closing the panel keeps the size it reached, saved once.
    setup
        .scenario
        .mouse(MouseEventKind::Down(MouseButton::Left), rect.x, 10);
    setup
        .scenario
        .mouse(MouseEventKind::Drag(MouseButton::Left), rect.x + 10, 10);
    setup.scenario.key("ctrl+k");
    setup.settle();
    assert!(!setup.app().assistant().is_open());
    assert_eq!(setup.backend.calls(OP_REVIEW_ASSISTANT_PANEL_SIZE_SAVE), 2);
    assert_eq!(
        saved_size(&setup).and_then(|size| size.right_share),
        Some(share_of(150, 250))
    );

    // A new run restores it, over a configured width.
    let (configured, _) = config("width = 50");
    relaunch(&mut setup, configured.clone(), 250, 40);
    setup.scenario.key("ctrl+k");
    let screen = setup.settle();
    assert_eq!(panel(&setup, 250, 40).unwrap().0.width, 150);
    assert_divider_agrees(&setup, &screen, 250, 40);
    assert_eq!(
        setup.backend.calls(OP_REVIEW_ASSISTANT_PANEL_SIZE_SAVE),
        2,
        "restoring saves nothing"
    );

    // Reset from the palette: the configured width, and the saved size is cleared.
    setup.scenario.key("esc");
    setup.settle();
    setup.scenario.key(":");
    setup.settle();
    setup.scenario.paste("reset the panel size");
    setup.settle();
    setup.scenario.key("enter");
    let screen = setup.settle();
    assert_eq!(panel(&setup, 250, 40).unwrap().0.width, 50);
    assert!(
        screen.contains("panel size reset to the default"),
        "{}",
        screen.text()
    );
    assert_eq!(
        saved_size(&setup).map(|size| (size.right_share, size.bottom_share)),
        Some((None, None))
    );
    relaunch(&mut setup, configured, 250, 40);
    setup.scenario.key("ctrl+k");
    setup.settle();
    assert_eq!(panel(&setup, 250, 40).unwrap().0.width, 50);
}

#[test]
fn a_saved_bottom_size_is_restored_and_a_failed_save_says_so() {
    let mut setup = setup(99, 40, false);
    setup.scenario.key("ctrl+k");
    setup.settle();
    let (rect, dock) = panel(&setup, 99, 40).unwrap();
    assert_eq!(dock, Dock::Bottom);
    drag(&mut setup, (3, rect.y), (3, rect.y - 6));
    let height = panel(&setup, 99, 40).unwrap().0.height;
    assert_eq!(height, rect.height + 6);
    relaunch(&mut setup, AssistantConfig::default(), 99, 40);
    setup.scenario.key("ctrl+k");
    setup.settle();
    assert_eq!(panel(&setup, 99, 40).unwrap().0.height, height);

    setup
        .backend
        .set_failing(OP_REVIEW_ASSISTANT_PANEL_SIZE_SAVE, true);
    let (rect, _) = panel(&setup, 99, 40).unwrap();
    drag(&mut setup, (3, rect.y), (3, rect.y + 3));
    let screen = setup.settle();
    assert!(
        screen.contains("could not save the panel size"),
        "{}",
        screen.text()
    );
    // The size still applies for the run.
    assert_eq!(panel(&setup, 99, 40).unwrap().0.height, height - 3);
}
