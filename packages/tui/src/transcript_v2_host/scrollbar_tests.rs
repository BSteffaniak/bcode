//! The optional V2 transcript scrollbar (VP4, W12, X6): presentation modes, the thumb within the
//! whole session, hit-region agreement and track presses that jump through `JumpToFraction`.

use std::time::{Duration, Instant};

use bcode_session_models::SessionId;
use bcode_tui_transcript::engine::HostRequest;
use bcode_tui_transcript::navigation::NavMode;
use bcode_tui_transcript::viewport::ScrollbarMode;
use bcode_tui_transcript::window::{HistoryWindowResult, WindowRequest};
use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::{Point, Rect};
use bmux_tui_components::scrollbar::ScrollbarPolicy;

use super::TranscriptV2Host;
use super::effects::HistoryLoaded;
use super::paint::ITEM_HIT_PREFIX;
use super::test_fixtures::{COMPOSER, Painted, REGION, conversation, paint};
use crate::effects::{TuiEffect, TuiEffectResult};

/// Turns of the long session; three canonical events each.
const TURNS: u64 = 1_000;
/// Turns resident in the attach tail window.
const RESIDENT_TURNS: u64 = 30;

/// A host with scrollbar `mode`, attached to a long session whose tail window holds only the
/// last [`RESIDENT_TURNS`] turns, with the session bounds the host's history executor reports.
fn long_session(mode: ScrollbarMode, at: Instant) -> TranscriptV2Host {
    let session = SessionId::new();
    let mut host = TranscriptV2Host::new();
    host.sync_geometry(REGION, at);
    host.set_composer_area(COMPOSER);
    let _ = host.set_scrollbar(mode, at);
    host.sync_session(Some(session), at);
    let effects = host.drain_effects();
    let [TuiEffect::LoadTranscriptV2History { load }] = effects.as_slice() else {
        panic!("one tail request, got {} effects", effects.len());
    };
    let HostRequest::Window(issued) = load.request else {
        panic!("window request");
    };
    let all = conversation(session, TURNS);
    let resident = all[all.len() - usize::try_from(RESIDENT_TURNS * 3).expect("small")..].to_vec();
    let result = TuiEffectResult::TranscriptV2HistoryLoaded {
        load: *load,
        result: Ok(HistoryLoaded::Window(HistoryWindowResult {
            generation: issued.generation,
            target: issued.request,
            events: resident,
            has_older: true,
            has_newer: false,
            first_available_sequence: Some(1),
            last_available_sequence: Some(TURNS * 3),
        })),
    };
    assert!(host.apply_effect_result(result, at));
    host
}

/// Terminal rows of column `x` inside the region that paint the scrollbar thumb.
fn thumb_rows(painted: &Painted, x: u16) -> Vec<u16> {
    let thumb = ScrollbarPolicy::vertical().thumb;
    (REGION.y..REGION.bottom())
        .filter(|y| {
            painted
                .buffer
                .get(Point::new(x, *y))
                .is_some_and(|cell| cell.symbol == thumb)
        })
        .collect()
}

fn item_hits(painted: &Painted) -> impl Iterator<Item = Rect> + '_ {
    painted
        .hits
        .iter()
        .filter(|hit| hit.id.as_str().starts_with(ITEM_HIT_PREFIX))
        .map(|hit| hit.area)
}

fn mouse(host: &mut TranscriptV2Host, kind: MouseEventKind, at_cell: Point, at: Instant) {
    let _ = host.handle_mouse(MouseEvent::new(kind, at_cell), 3, None, at);
}

fn around_requests(host: &mut TranscriptV2Host) -> Vec<u64> {
    host.drain_effects()
        .into_iter()
        .filter_map(|effect| match effect {
            TuiEffect::LoadTranscriptV2History { load } => match load.request {
                HostRequest::Window(issued) => match issued.request {
                    WindowRequest::Around { sequence } => Some(sequence),
                    _ => None,
                },
                HostRequest::Page(_) => None,
            },
            _ => None,
        })
        .collect()
}

#[test]
fn off_by_default_reserves_nothing_and_registers_no_track() {
    let at = Instant::now();
    let mut host = long_session(ScrollbarMode::Off, at);
    let painted = paint(&mut host, at, Duration::ZERO);
    assert_eq!(host.engine().scrollbar(), ScrollbarMode::Off);
    assert_eq!(host.engine().geometry().width, u64::from(REGION.width));
    assert_eq!(host.regions().scrollbar, None);
    assert_eq!(thumb_rows(&painted, REGION.right() - 1), Vec::<u16>::new());
}

#[test]
fn gutter_reserves_a_column_and_places_the_thumb_within_the_whole_session() {
    let at = Instant::now();
    let mut host = long_session(ScrollbarMode::Gutter, at);
    let painted = paint(&mut host, at, Duration::ZERO);
    let gutter = REGION.right() - 1;
    assert_eq!(
        host.engine().geometry().width,
        u64::from(REGION.width) - 1,
        "units are laid out beside the gutter"
    );
    assert_eq!(
        host.regions().scrollbar,
        Some(Rect::new(gutter, REGION.y, 1, REGION.height))
    );
    // Following the latest content of a long session: the estimate puts thousands of rows
    // before the window, so the thumb is short and at the bottom of the track.
    let estimate = host.engine().position_estimate();
    assert!(estimate.before > estimate.resident * 10, "{estimate:?}");
    let rows = thumb_rows(&painted, gutter);
    assert!(!rows.is_empty(), "the thumb is painted");
    assert!(rows.len() <= 2, "{rows:?}");
    assert_eq!(rows.last().copied(), Some(REGION.bottom() - 1), "{rows:?}");
    assert!(
        item_hits(&painted).all(|area| area.right() <= gutter),
        "no item hit region in the gutter"
    );
}

#[test]
fn overlay_keeps_the_content_width_and_item_hits_stop_short_of_the_track() {
    let at = Instant::now();
    let mut host = long_session(ScrollbarMode::Overlay, at);
    let painted = paint(&mut host, at, Duration::ZERO);
    let track = REGION.right() - 1;
    assert_eq!(host.engine().geometry().width, u64::from(REGION.width));
    assert_eq!(
        host.regions().scrollbar.map(|track| track.x),
        Some(track),
        "the overlay track lies over the last column"
    );
    assert_ne!(thumb_rows(&painted, track), Vec::<u16>::new());
    assert!(item_hits(&painted).all(|area| area.right() <= track));
}

#[test]
fn a_track_press_outside_the_resident_rows_jumps_with_one_around_window() {
    let at = Instant::now();
    let mut host = long_session(ScrollbarMode::Gutter, at);
    let _ = paint(&mut host, at, Duration::ZERO);
    let _ = host.drain_effects();
    let gutter = REGION.right() - 1;
    let top = Point::new(gutter, REGION.y);
    mouse(&mut host, MouseEventKind::Down(MouseButton::Left), top, at);
    let around = around_requests(&mut host);
    assert_eq!(around.len(), 1, "{around:?}");
    assert!(
        around[0] < TURNS,
        "the top of the track maps near the session start: {around:?}"
    );
    // The release ends the gesture: it is no item click and later presses elsewhere are
    // ordinary transcript input.
    mouse(&mut host, MouseEventKind::Up(MouseButton::Left), top, at);
    assert!(!host.scrollbar_drag);
    assert!(host.pending_click.is_none());
}

#[test]
fn dragging_the_thumb_into_the_resident_rows_scrolls_without_a_request() {
    let at = Instant::now();
    let mut host = long_session(ScrollbarMode::Gutter, at);
    let painted = paint(&mut host, at, Duration::ZERO);
    let _ = host.drain_effects();
    let gutter = REGION.right() - 1;
    let thumb = *thumb_rows(&painted, gutter).last().expect("thumb");
    mouse(
        &mut host,
        MouseEventKind::Down(MouseButton::Left),
        Point::new(gutter, thumb),
        at,
    );
    assert!(host.scrollbar_drag, "a thumb press starts a drag");
    // Dragging back to the thumb row after leaving the gutter stays the scrollbar's gesture.
    mouse(
        &mut host,
        MouseEventKind::Drag(MouseButton::Left),
        Point::new(3, thumb),
        at,
    );
    mouse(
        &mut host,
        MouseEventKind::Up(MouseButton::Left),
        Point::new(3, thumb),
        at,
    );
    assert!(!host.scrollbar_drag);
    assert_eq!(around_requests(&mut host), Vec::<u64>::new());
    assert_eq!(host.engine().nav().mode, NavMode::Following);
}

#[test]
fn an_overlay_track_press_over_the_lists_own_item_region_is_the_scrollbars() {
    // The BMUX list registers each visible item's hit region across the full list width, so in
    // overlay mode the topmost region at the track is the list's item region, not an overlay.
    let at = Instant::now();
    let mut host = long_session(ScrollbarMode::Overlay, at);
    let _ = paint(&mut host, at, Duration::ZERO);
    let _ = host.drain_effects();
    let track = Point::new(REGION.right() - 1, REGION.y);
    let item_region = format!("{}.item.event:7", bcode_tui_transcript::engine::LIST_ID);
    let _ = host.handle_mouse(
        MouseEvent::new(MouseEventKind::Down(MouseButton::Left), track),
        3,
        Some(&item_region),
        at,
    );
    assert!(host.scrollbar_drag);
    assert_eq!(around_requests(&mut host).len(), 1);
}

#[test]
fn a_press_under_another_painter_is_not_the_scrollbar() {
    let at = Instant::now();
    let mut host = long_session(ScrollbarMode::Gutter, at);
    let _ = paint(&mut host, at, Duration::ZERO);
    let _ = host.drain_effects();
    let top = Point::new(REGION.right() - 1, REGION.y);
    let _ = host.handle_mouse(
        MouseEvent::new(MouseEventKind::Down(MouseButton::Left), top),
        3,
        Some("overlay.dialog"),
        at,
    );
    assert!(!host.scrollbar_drag);
    assert_eq!(around_requests(&mut host), Vec::<u64>::new());
}

#[test]
fn narrow_regions_present_no_scrollbar_and_switching_modes_remeasures() {
    let at = Instant::now();
    let mut host = long_session(ScrollbarMode::Gutter, at);
    let narrow = Rect::new(0, 1, 1, 12);
    host.sync_geometry(narrow, at);
    assert_eq!(host.engine().presented_scrollbar(), ScrollbarMode::Off);
    assert_eq!(
        host.engine().geometry().width,
        1,
        "the only column stays content"
    );

    host.sync_geometry(REGION, at);
    assert_eq!(host.engine().geometry().width, u64::from(REGION.width) - 1);
    assert!(host.set_scrollbar(ScrollbarMode::Off, at));
    assert_eq!(host.engine().geometry().width, u64::from(REGION.width));
    assert!(!host.set_scrollbar(ScrollbarMode::Off, at), "unchanged");
}

#[test]
fn a_track_press_inside_a_fully_resident_session_reads_there_without_a_request() {
    let at = Instant::now();
    let mut host = super::test_fixtures::attached(60, at);
    assert!(host.set_scrollbar(ScrollbarMode::Gutter, at));
    let _ = paint(&mut host, at, Duration::ZERO);
    let _ = host.drain_effects();
    let estimate = host.engine().position_estimate();
    assert_eq!((estimate.before, estimate.after), (0, 0), "{estimate:?}");
    let middle = Point::new(REGION.right() - 1, REGION.y + REGION.height / 2);
    mouse(
        &mut host,
        MouseEventKind::Down(MouseButton::Left),
        middle,
        at,
    );
    mouse(&mut host, MouseEventKind::Up(MouseButton::Left), middle, at);
    assert_eq!(around_requests(&mut host), Vec::<u64>::new());
    assert!(
        matches!(host.engine().nav().mode, NavMode::Reading { .. }),
        "{:?}",
        host.engine().nav().mode
    );
}
