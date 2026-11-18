//! The window controls every panel of the review app shares: the file panel, the review
//! document, the threads panel, the guide panel and the assistant all minimize, maximize,
//! close, resize and restore through the same controls and the same keys, driven here through
//! one table of panels.

use super::assistant::{Setup, setup};
use crate::assistant::panel::PANEL_ID;
use crate::panel_controls::{Control, DOUBLE_CLICK, PanelKind, Shape};
use crate::surface::{FILES_PANE_ID, GUIDE_PANE_ID, REVIEW_PANE_ID, THREADS_PANE_ID};
use crate::testing::harness::Screen;
use bmux_tui::geometry::Rect;

const WIDTH: u16 = 200;
/// A named change to the panels.
type Step = fn(&mut Setup);
const HEIGHT: u16 = 50;

/// The pane id of `kind` (its control hit regions are `<pane id>.action.<control>`).
const fn pane_id(kind: PanelKind) -> &'static str {
    match kind {
        PanelKind::Files => FILES_PANE_ID,
        PanelKind::Review => REVIEW_PANE_ID,
        PanelKind::Threads => THREADS_PANE_ID,
        PanelKind::Guide => GUIDE_PANE_ID,
        PanelKind::Assistant => PANEL_ID,
    }
}

/// The key that opens (and focuses, or toggles) `kind` again after it was closed.
const fn open_key(kind: PanelKind) -> &'static str {
    match kind {
        PanelKind::Files => "t",
        PanelKind::Threads => "shift+t",
        PanelKind::Guide => "alt+g",
        PanelKind::Assistant => "ctrl+k",
        PanelKind::Review => "",
    }
}

/// A review with every panel open: the file panel (shown by width), the threads panel, the
/// guide panel and the docked assistant, with the keyboard back on the review.
fn all_open(width: u16, height: u16) -> Setup {
    let mut setup = setup(width, height, true);
    setup.scenario.key("shift+t");
    setup.scenario.key("alt+g");
    setup.scenario.key("ctrl+k");
    setup.settle();
    // Give the keyboard back to the review; the assistant stays docked.
    setup.scenario.key("esc");
    setup.settle();
    for kind in PanelKind::ALL {
        assert_eq!(shape(&setup, kind), Some(Shape::Normal), "{kind:?}");
    }
    setup
}

/// How `kind` is shown, or `None` while it is not.
fn shape(setup: &Setup, kind: PanelKind) -> Option<Shape> {
    if kind == PanelKind::Assistant {
        let (width, height) = size(setup);
        let assistant = setup.app().assistant();
        return assistant
            .split(Rect::new(0, 0, width, height))
            .panel
            .map(|_| assistant.shape());
    }
    setup.app().review()?.panel_shape(kind)
}

fn size(setup: &Setup) -> (u16, u16) {
    let screen = setup.scenario.size();
    (screen.width, screen.height)
}

/// The painted cells of `kind`'s pane.
fn area(setup: &Setup, kind: PanelKind) -> Option<Rect> {
    if kind == PanelKind::Assistant {
        let (width, height) = size(setup);
        return setup
            .app()
            .assistant()
            .split(Rect::new(0, 0, width, height))
            .panel
            .map(|(rect, _)| rect);
    }
    setup.app().review()?.body_area(pane_id(kind))
}

/// The hit region of `kind`'s `control`, as painted.
fn control(screen: &Screen, kind: PanelKind, control: Control) -> Option<Rect> {
    let id = format!("{}.action.{}", pane_id(kind), control.id());
    screen
        .hits()
        .iter()
        .find(|hit| hit.id.as_str() == id)
        .map(|hit| hit.area)
}

fn click_control(setup: &mut Setup, kind: PanelKind, which: Control) -> Screen {
    let screen = setup.settle();
    let at = control(&screen, kind, which)
        .unwrap_or_else(|| panic!("{kind:?} {which:?}\n{}", screen.text()));
    setup.scenario.click(at.x + at.width / 2, at.y);
    // The very next frame already shows the new state on its controls.
    let first = setup.scenario.frame();
    assert_controls_agree(&first, setup, &format!("{kind:?} {which:?}: first frame"));
    setup.settle()
}

/// A cell of `kind`'s title row inside its corners, off its controls.
fn title_cell(setup: &Setup, kind: PanelKind) -> (u16, u16) {
    let area = area(setup, kind).unwrap_or_else(|| panic!("{kind:?} shown"));
    (area.x + 2, area.y)
}

/// Give `kind` the keyboard with a press on its title row, far enough from the last press
/// not to make a double click.
fn focus(setup: &mut Setup, kind: PanelKind) {
    std::thread::sleep(DOUBLE_CLICK + std::time::Duration::from_millis(50));
    let (x, y) = title_cell(setup, kind);
    setup.scenario.click(x, y);
    setup.settle();
}

/// Every control painted on `screen` shows the glyph of its state on its own cells, inside the
/// frame, and only the panels in `shown` have regions.
fn assert_controls_agree(screen: &Screen, setup: &Setup, context: &str) {
    let (width, height) = size(setup);
    let frame = Rect::new(0, 0, width, height);
    let theme = crate::theme::ReviewTheme::derive(None, crate::theme::IconStyle::default());
    let icons = theme.icons;
    for hit in screen.hits() {
        assert!(
            hit.area.width > 0 && hit.area.height > 0,
            "{context}: {hit:?}"
        );
        assert_eq!(frame.intersection(hit.area), hit.area, "{context}: {hit:?}");
        let id = hit.id.as_str();
        for kind in PanelKind::ALL {
            let Some(which) = id
                .strip_prefix(&format!("{}.action.", pane_id(kind)))
                .and_then(Control::from_id)
            else {
                continue;
            };
            let state = shape(setup, kind);
            assert!(
                state.is_some(),
                "{context}: a region of a hidden {kind:?}: {hit:?}"
            );
            let glyph = match (which, state) {
                (Control::Minimize, Some(shape)) if shape.minimized() => icons.panel_restore,
                (Control::Minimize, _) => icons.panel_minimize,
                (Control::Maximize, Some(Shape::Maximized)) => icons.panel_restore,
                (Control::Maximize, _) => icons.panel_maximize,
                (Control::Close, _) => icons.panel_close,
            };
            assert_eq!(
                screen.text_in(hit.area, hit.area.y).trim(),
                glyph,
                "{context}: {hit:?}"
            );
        }
    }
}

#[test]
fn every_panel_minimizes_and_restores_by_click_and_by_key() {
    let mut setup = all_open(WIDTH, HEIGHT);
    for kind in PanelKind::ALL {
        let screen = click_control(&mut setup, kind, Control::Minimize);
        let minimized = shape(&setup, kind).expect("still shown");
        assert!(
            minimized.minimized(),
            "{kind:?} {minimized:?}\n{}",
            screen.text()
        );
        assert_controls_agree(&screen, &setup, &format!("{kind:?} minimized"));
        // Collapsed along its group's axis: one row stacked, a strip side by side.
        let rect = area(&setup, kind).unwrap();
        match minimized {
            Shape::Row => assert_eq!(rect.height, 1, "{kind:?}"),
            Shape::Strip => assert_eq!(
                rect.width,
                crate::panel_controls::strip_width(&crate::theme::ReviewTheme::derive(
                    None,
                    crate::theme::IconStyle::default()
                )),
                "{kind:?}"
            ),
            other => panic!("{kind:?} {other:?}"),
        }
        let screen = click_control(&mut setup, kind, Control::Minimize);
        assert_eq!(shape(&setup, kind), Some(Shape::Normal), "{kind:?}");
        assert_controls_agree(&screen, &setup, &format!("{kind:?} restored"));

        focus(&mut setup, kind);
        setup.scenario.key("alt+c");
        setup.settle();
        assert!(shape(&setup, kind).unwrap().minimized(), "{kind:?} by key");
        setup.scenario.key("alt+c");
        setup.settle();
        assert_eq!(shape(&setup, kind), Some(Shape::Normal), "{kind:?} by key");
    }
}

#[test]
fn every_panel_maximizes_by_click_key_and_double_click_and_restores() {
    let mut setup = all_open(WIDTH, HEIGHT);
    let only = |setup: &Setup, screen: &Screen, kind: PanelKind, context: &str| {
        assert_eq!(shape(setup, kind), Some(Shape::Maximized), "{context}");
        // The other panels are not laid out: none of their regions is left behind. A review
        // panel fills the review body (the assistant docked beside the screen stays); the
        // assistant fills the app.
        for other in PanelKind::ALL.into_iter().filter(|other| {
            *other != kind && (kind == PanelKind::Assistant || *other != PanelKind::Assistant)
        }) {
            let prefix = format!("{}.", pane_id(other));
            assert!(
                !screen
                    .hits()
                    .iter()
                    .any(|hit| hit.id.as_str().starts_with(&prefix)),
                "{context}: {other:?} left regions\n{}",
                screen.text()
            );
        }
        assert_controls_agree(screen, setup, context);
    };
    for kind in PanelKind::ALL {
        let screen = click_control(&mut setup, kind, Control::Maximize);
        only(&setup, &screen, kind, &format!("{kind:?} by click"));
        let screen = click_control(&mut setup, kind, Control::Maximize);
        assert_eq!(shape(&setup, kind), Some(Shape::Normal), "{kind:?}");
        assert_controls_agree(&screen, &setup, &format!("{kind:?} restored"));

        focus(&mut setup, kind);
        setup.scenario.key("alt+z");
        let screen = setup.settle();
        only(&setup, &screen, kind, &format!("{kind:?} by key"));
        // The panel's back key restores it, keeping the focus there.
        setup.scenario.key("esc");
        setup.settle();
        assert_eq!(shape(&setup, kind), Some(Shape::Normal), "{kind:?} esc");
        setup.scenario.key("alt+z");
        setup.settle();
        assert_eq!(shape(&setup, kind), Some(Shape::Maximized), "{kind:?}");
        setup.scenario.key("alt+z");
        setup.settle();
        assert_eq!(shape(&setup, kind), Some(Shape::Normal), "{kind:?}");

        std::thread::sleep(DOUBLE_CLICK + std::time::Duration::from_millis(50));
        let (x, y) = title_cell(&setup, kind);
        setup.scenario.click(x, y);
        setup.scenario.click(x, y);
        let screen = setup.settle();
        only(&setup, &screen, kind, &format!("{kind:?} by double click"));
        let (x, y) = title_cell(&setup, kind);
        setup.scenario.click(x, y);
        setup.scenario.click(x, y);
        setup.settle();
        assert_eq!(shape(&setup, kind), Some(Shape::Normal), "{kind:?}");
    }
}

#[test]
fn every_panel_but_the_document_closes_by_click_and_by_key() {
    let mut setup = all_open(WIDTH, HEIGHT);
    for kind in PanelKind::ALL {
        if kind == PanelKind::Review {
            let screen = setup.settle();
            assert!(control(&screen, kind, Control::Close).is_none());
            focus(&mut setup, kind);
            setup.scenario.key("alt+w");
            let screen = setup.settle();
            assert_eq!(shape(&setup, kind), Some(Shape::Normal));
            assert!(
                screen.contains(crate::panel_controls::REVIEW_CLOSE_REFUSED),
                "{}",
                screen.text()
            );
            continue;
        }
        let screen = click_control(&mut setup, kind, Control::Close);
        assert_eq!(shape(&setup, kind), None, "{kind:?} closed by click");
        assert_controls_agree(&screen, &setup, &format!("{kind:?} closed"));
        setup.scenario.key(open_key(kind));
        setup.settle();
        assert_eq!(
            shape(&setup, kind),
            Some(Shape::Normal),
            "{kind:?} reopened"
        );

        focus(&mut setup, kind);
        setup.scenario.key("alt+w");
        setup.settle();
        assert_eq!(shape(&setup, kind), None, "{kind:?} closed by key");
        setup.scenario.key(open_key(kind));
        setup.settle();
        if kind == PanelKind::Assistant {
            setup.scenario.key("esc");
        }
        setup.settle();
        assert_eq!(
            shape(&setup, kind),
            Some(Shape::Normal),
            "{kind:?} reopened"
        );
    }
}

#[test]
fn every_panel_grows_shrinks_and_resets_by_key() {
    let mut setup = all_open(WIDTH, HEIGHT);
    for kind in PanelKind::ALL {
        focus(&mut setup, kind);
        let before = area(&setup, kind).unwrap();
        let cells = |rect: Rect| u32::from(rect.width) * u32::from(rect.height);
        // Shrink first: a panel at its default size has room to give back.
        setup.scenario.key("alt+-");
        setup.settle();
        let shrunk = area(&setup, kind).unwrap();
        assert!(
            cells(shrunk) < cells(before),
            "{kind:?} {before:?} -> {shrunk:?}"
        );
        setup.scenario.key("alt+=");
        setup.settle();
        assert_eq!(area(&setup, kind).unwrap(), before, "{kind:?} grown back");
        setup.scenario.key("alt+-");
        setup.scenario.key("alt+-");
        setup.settle();
        setup.scenario.key("alt+0");
        setup.settle();
        assert_eq!(area(&setup, kind).unwrap(), before, "{kind:?} reset");
    }
    // The old keys still resize the file panel from anywhere.
    focus(&mut setup, PanelKind::Review);
    let before = area(&setup, PanelKind::Files).unwrap();
    setup.scenario.key("<");
    setup.settle();
    assert!(area(&setup, PanelKind::Files).unwrap().width < before.width);
    setup.scenario.key(">");
    setup.settle();
    assert_eq!(area(&setup, PanelKind::Files).unwrap(), before);
}

#[test]
fn minimized_panels_persist_across_restarts_and_maximize_does_not() {
    let mut setup = all_open(WIDTH, HEIGHT);
    for kind in [PanelKind::Files, PanelKind::Threads, PanelKind::Guide] {
        click_control(&mut setup, kind, Control::Minimize);
    }
    click_control(&mut setup, PanelKind::Review, Control::Maximize);
    assert_eq!(shape(&setup, PanelKind::Review), Some(Shape::Maximized));
    // Reopen the review as stored.
    let stored: bcode_code_review_models::GetReviewWorkspaceResponse =
        setup
            .backend
            .call_now(bcode_code_review_models::GetReviewWorkspaceRequest {
                repo_path: setup.review.repo_root.clone(),
                workspace_id: setup.review.id.clone(),
            });
    setup.review = stored.workspace.expect("stored");
    setup.reopen(WIDTH, HEIGHT);
    for kind in [PanelKind::Files, PanelKind::Threads, PanelKind::Guide] {
        assert!(
            shape(&setup, kind).is_some_and(Shape::minimized),
            "{kind:?} {:?}",
            shape(&setup, kind)
        );
    }
    assert_eq!(shape(&setup, PanelKind::Review), Some(Shape::Normal));
    // Focused from the keyboard, a minimized panel is restored.
    setup.scenario.key("alt+g");
    setup.settle();
    assert_eq!(shape(&setup, PanelKind::Guide), Some(Shape::Normal));
    // Closing a panel restores it: it opens at its size next time.
    setup.scenario.key("alt+c");
    setup.settle();
    assert!(shape(&setup, PanelKind::Guide).is_some_and(Shape::minimized));
    setup.scenario.key("alt+w");
    setup.settle();
    assert_eq!(shape(&setup, PanelKind::Guide), None);
    setup.scenario.key("alt+g");
    setup.settle();
    assert_eq!(shape(&setup, PanelKind::Guide), Some(Shape::Normal));
}

#[test]
fn the_document_minimizes_only_with_an_expanded_panel_to_take_its_room() {
    let mut setup = all_open(WIDTH, HEIGHT);
    // Under it, the threads panel takes its rows.
    click_control(&mut setup, PanelKind::Review, Control::Minimize);
    assert_eq!(shape(&setup, PanelKind::Review), Some(Shape::Row));
    click_control(&mut setup, PanelKind::Review, Control::Minimize);
    // With the threads panel minimized too, nothing under it takes its rows.
    click_control(&mut setup, PanelKind::Threads, Control::Minimize);
    focus(&mut setup, PanelKind::Review);
    setup.scenario.key("alt+c");
    let screen = setup.settle();
    assert_eq!(shape(&setup, PanelKind::Review), Some(Shape::Normal));
    assert!(
        screen.contains(crate::panel_controls::REVIEW_MINIMIZE_REFUSED),
        "{}",
        screen.text()
    );
}

#[test]
fn control_regions_agree_with_painting_at_tiny_and_resized_sizes() {
    let mut setup = all_open(WIDTH, HEIGHT);
    let sizes = [
        (200, 50),
        (120, 30),
        (100, 20),
        (80, 16),
        (60, 12),
        (40, 8),
        (24, 5),
        (12, 4),
        (3, 2),
        (1, 1),
        (160, 40),
    ];
    let states: [(&str, Step); 3] = [
        ("normal", |_| {}),
        ("minimized", |setup| {
            for kind in [PanelKind::Files, PanelKind::Guide, PanelKind::Assistant] {
                if shape(setup, kind).is_some_and(|shape| !shape.minimized()) {
                    click_control(setup, kind, Control::Minimize);
                }
            }
        }),
        ("maximized", |setup| {
            if shape(setup, PanelKind::Threads).is_some() {
                click_control(setup, PanelKind::Threads, Control::Maximize);
            }
        }),
    ];
    for (name, apply) in states {
        setup.scenario.resize(WIDTH, HEIGHT);
        setup.settle();
        apply(&mut setup);
        for (width, height) in sizes {
            setup.scenario.resize(width, height);
            let screen = setup.settle();
            assert_controls_agree(&screen, &setup, &format!("{name} at {width}x{height}"));
        }
    }
}
