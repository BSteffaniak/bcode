//! V2 transcript input: focus transitions (X9: FO2, FO3, FO10), Home/End per focus (X5, FO4),
//! scroll keys from either focus (FO5), wheel and clicks that never move focus (FO9, X1).

use std::time::{Duration, Instant};

use bcode_tui_transcript::navigation::NavMode;
use bmux_keyboard::parse_key_stroke;
use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::Point;

use super::TranscriptInputOutcome;
use crate::keymap::BmuxKeyMap;
use crate::transcript_v2_host::TranscriptV2Host;
use crate::transcript_v2_host::test_fixtures::{COMPOSER, REGION, attached, paint};

fn keymap() -> BmuxKeyMap {
    BmuxKeyMap::from_config_for_engine(
        &bcode_config::TuiConfig::default(),
        bcode_config::TuiEngine::V2,
    )
}

fn press(host: &mut TranscriptV2Host, key: &str, at: Instant) -> TranscriptInputOutcome {
    host.handle_key(&keymap(), parse_key_stroke(key).expect("key"), at)
}

fn mouse(
    host: &mut TranscriptV2Host,
    kind: MouseEventKind,
    x: u16,
    y: u16,
) -> TranscriptInputOutcome {
    host.handle_mouse(
        MouseEvent::new(kind, Point::new(x, y)),
        3,
        None,
        Instant::now(),
    )
}

fn focused(host: &TranscriptV2Host) -> Option<bcode_tui_transcript::focus::FocusTargetId> {
    host.focus().transcript_target().cloned()
}

#[test]
fn outcomes_map_to_shell_fallthrough_and_repaint() {
    assert_eq!(TranscriptInputOutcome::Ignored.consumed_redraw(), None);
    assert_eq!(
        TranscriptInputOutcome::Consumed.consumed_redraw(),
        Some(false)
    );
    assert_eq!(TranscriptInputOutcome::Redraw.consumed_redraw(), Some(true));
}

#[test]
fn x9_alt_u_enters_and_leaves_and_escape_returns_to_the_composer() {
    let at = Instant::now();
    let mut host = attached(3, at);
    assert!(!host.transcript_focus_active(), "FO1: composer by default");
    assert_eq!(
        press(&mut host, "alt+u", at),
        TranscriptInputOutcome::Redraw
    );
    assert!(host.transcript_focus_active());
    assert_eq!(
        press(&mut host, "alt+u", at),
        TranscriptInputOutcome::Redraw
    );
    assert!(!host.transcript_focus_active());

    let _ = press(&mut host, "alt+u", at);
    assert_eq!(
        press(&mut host, "escape", at),
        TranscriptInputOutcome::Redraw
    );
    assert!(!host.transcript_focus_active());
    // FO10: with the composer focused Escape is not a transcript key; it reaches the shell
    // (and its double-Escape sequence) untouched.
    assert_eq!(
        press(&mut host, "escape", at),
        TranscriptInputOutcome::Ignored
    );
}

#[test]
fn x9_unbound_keys_and_paste_return_focus_and_reach_the_composer() {
    let at = Instant::now();
    let mut host = attached(3, at);
    let keymap = keymap();
    let _ = press(&mut host, "alt+u", at);
    let bound = Event::Key(parse_key_stroke("j").expect("key"));
    assert!(!host.release_focus_for_event(&keymap, &bound));
    assert!(host.transcript_focus_active());
    let unbound = Event::Key(parse_key_stroke("x").expect("key"));
    assert!(host.release_focus_for_event(&keymap, &unbound));
    assert!(!host.transcript_focus_active());

    let _ = press(&mut host, "alt+u", at);
    assert!(host.release_focus_for_event(&keymap, &Event::Paste("text".to_owned())));
    assert!(!host.transcript_focus_active());

    // Reaching `handle_key` directly, an unbound key still releases focus and is left to the
    // composer.
    let _ = press(&mut host, "alt+u", at);
    assert_eq!(press(&mut host, "x", at), TranscriptInputOutcome::Ignored);
    assert!(!host.transcript_focus_active());
}

#[test]
fn fo6_focus_keys_move_focus_only_with_transcript_focus() {
    let at = Instant::now();
    let mut host = attached(3, at);
    assert_eq!(press(&mut host, "k", at), TranscriptInputOutcome::Ignored);
    let _ = press(&mut host, "alt+u", at);
    let first = focused(&host).expect("focus");
    assert_eq!(press(&mut host, "k", at), TranscriptInputOutcome::Redraw);
    let previous = focused(&host).expect("focus");
    assert_ne!(previous, first);
    let _ = press(&mut host, "down", at);
    assert_eq!(focused(&host), Some(first));
}

#[test]
fn x5_home_and_end_belong_to_the_composer_unless_the_transcript_has_focus() {
    let at = Instant::now();
    let mut host = attached(6, at);
    assert_eq!(
        press(&mut host, "home", at),
        TranscriptInputOutcome::Ignored
    );
    assert_eq!(press(&mut host, "end", at), TranscriptInputOutcome::Ignored);
    assert_eq!(host.engine().nav().mode, NavMode::Following);

    let _ = press(&mut host, "alt+u", at);
    let last = focused(&host).expect("focus");
    assert_eq!(press(&mut host, "home", at), TranscriptInputOutcome::Redraw);
    let start = focused(&host).expect("focus");
    assert_ne!(start, last, "focus moved to the first target");
    assert_eq!(
        Some(&start),
        host.engine()
            .focus_view()
            .edge(bcode_tui_transcript::focus::FocusEdge::First)
            .as_ref()
    );
    assert_ne!(host.engine().nav().mode, NavMode::Following);
    let _ = press(&mut host, "end", at);
    assert_eq!(focused(&host), Some(last));
    assert!(host.transcript_focus_active());
}

#[test]
fn fo5_page_and_jump_keys_scroll_from_either_focus_without_changing_it() {
    let at = Instant::now();
    let mut host = attached(6, at);
    assert_eq!(
        press(&mut host, "pageUp", at),
        TranscriptInputOutcome::Redraw
    );
    assert!(!host.transcript_focus_active());
    assert_ne!(host.engine().nav().mode, NavMode::Following);
    let _ = press(&mut host, "ctrl+end", at);
    assert_eq!(host.engine().nav().mode, NavMode::Following);

    let _ = press(&mut host, "alt+u", at);
    let target = focused(&host);
    let _ = press(&mut host, "ctrl+up", at);
    assert_ne!(host.engine().nav().mode, NavMode::Following);
    assert!(host.transcript_focus_active());
    let _ = press(&mut host, "pageDown", at);
    assert_eq!(focused(&host), target, "scrolling never moves focus");
    assert!(host.transcript_focus_active());
}

#[test]
fn wheel_scrolls_inside_the_transcript_region_only() {
    let at = Instant::now();
    let mut host = attached(6, at);
    let _ = paint(&mut host, at, Duration::ZERO);
    let outside = mouse(&mut host, MouseEventKind::ScrollUp, 1, COMPOSER.y + 1);
    assert_eq!(outside, TranscriptInputOutcome::Ignored);
    assert_eq!(host.engine().nav().mode, NavMode::Following);
    let inside = mouse(&mut host, MouseEventKind::ScrollUp, 1, REGION.y + 2);
    assert_eq!(inside, TranscriptInputOutcome::Redraw);
    assert_ne!(host.engine().nav().mode, NavMode::Following);
}

#[test]
fn x1_fo9_clicks_toggle_disclosures_drags_do_not_and_focus_never_moves() {
    let at = Instant::now();
    let mut host = attached(2, at);
    let painted = paint(&mut host, at, Duration::ZERO);
    let item = painted
        .hits
        .iter()
        .find(|hit| {
            hit.id
                .as_str()
                .starts_with(super::super::paint::ITEM_HIT_PREFIX)
        })
        .expect("an interactive region is registered");
    let (x, y) = (item.area.x, item.area.y);
    let rows_before = (REGION.y..REGION.bottom())
        .map(|row| painted.row(row))
        .collect::<Vec<_>>();

    // Drag: no toggle.
    assert_eq!(
        mouse(&mut host, MouseEventKind::Down(MouseButton::Left), x, y),
        TranscriptInputOutcome::Consumed
    );
    let _ = mouse(&mut host, MouseEventKind::Drag(MouseButton::Left), x + 1, y);
    assert_eq!(
        mouse(&mut host, MouseEventKind::Up(MouseButton::Left), x + 1, y),
        TranscriptInputOutcome::Consumed
    );
    let painted = paint(&mut host, at, Duration::ZERO);
    let rows_dragged = (REGION.y..REGION.bottom())
        .map(|row| painted.row(row))
        .collect::<Vec<_>>();
    assert_eq!(rows_before, rows_dragged);

    // Click: the region toggles and keyboard focus stays with the composer.
    let _ = mouse(&mut host, MouseEventKind::Down(MouseButton::Left), x, y);
    assert_eq!(
        mouse(&mut host, MouseEventKind::Up(MouseButton::Left), x, y),
        TranscriptInputOutcome::Redraw
    );
    assert!(!host.transcript_focus_active());
    let painted = paint(&mut host, at, Duration::ZERO);
    let rows_clicked = (REGION.y..REGION.bottom())
        .map(|row| painted.row(row))
        .collect::<Vec<_>>();
    assert_ne!(
        rows_before, rows_clicked,
        "the click changed the disclosure"
    );

    // With transcript focus a click does not move it either.
    let _ = host.handle_key(&keymap(), parse_key_stroke("alt+u").expect("key"), at);
    let target = focused(&host);
    let _ = mouse(&mut host, MouseEventKind::Down(MouseButton::Left), x, y);
    let _ = mouse(&mut host, MouseEventKind::Up(MouseButton::Left), x, y);
    assert_eq!(focused(&host), target);
}

#[test]
fn fo3_a_press_in_the_composer_returns_focus_and_is_left_to_the_shell() {
    let at = Instant::now();
    let mut host = attached(2, at);
    let _ = paint(&mut host, at, Duration::ZERO);
    let _ = press(&mut host, "alt+u", at);
    let outcome = mouse(
        &mut host,
        MouseEventKind::Down(MouseButton::Left),
        COMPOSER.x + 2,
        COMPOSER.y + 1,
    );
    assert_eq!(outcome, TranscriptInputOutcome::Ignored);
    assert!(!host.transcript_focus_active());
}

/// B7: a press or release over a hit region another painter registered above the transcript
/// (an overlay) is not a transcript click; the occluded disclosure does not toggle.
#[test]
fn clicks_resolve_against_the_presented_hit_regions() {
    let at = Instant::now();
    let mut host = attached(2, at);
    let painted = paint(&mut host, at, Duration::ZERO);
    let item = painted
        .hits
        .iter()
        .find(|hit| {
            hit.id
                .as_str()
                .starts_with(super::super::paint::ITEM_HIT_PREFIX)
        })
        .expect("an interactive region is registered");
    let (id, point) = (
        item.id.as_str().to_owned(),
        Point::new(item.area.x, item.area.y),
    );
    let event = |kind| MouseEvent::new(kind, point);

    let overlay = Some("overlay.button");
    assert_eq!(
        host.handle_mouse(
            event(MouseEventKind::Down(MouseButton::Left)),
            3,
            overlay,
            at
        ),
        TranscriptInputOutcome::Ignored,
        "the overlay's press is left to the shell"
    );
    let _ = host.handle_mouse(event(MouseEventKind::Up(MouseButton::Left)), 3, overlay, at);
    let unchanged = paint(&mut host, at, Duration::ZERO);
    assert_eq!(
        (REGION.y..REGION.bottom())
            .map(|row| unchanged.row(row))
            .collect::<Vec<_>>(),
        (REGION.y..REGION.bottom())
            .map(|row| painted.row(row))
            .collect::<Vec<_>>()
    );

    let _ = host.handle_mouse(
        event(MouseEventKind::Down(MouseButton::Left)),
        3,
        Some(&id),
        at,
    );
    assert_eq!(
        host.handle_mouse(
            event(MouseEventKind::Up(MouseButton::Left)),
            3,
            Some(&id),
            at
        ),
        TranscriptInputOutcome::Redraw,
        "the transcript's own presented region toggles"
    );
}
