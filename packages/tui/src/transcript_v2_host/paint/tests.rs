//! V2 transcript frames: geometry, the latest bar (F6, C18–C21), the focus highlight and hit
//! regions from the painted snapshot (B3, B7, B12).

use std::time::{Duration, Instant};

use bcode_tui_transcript::navigation::NavMode;
use bmux_keyboard::parse_key_stroke;
use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::{Point, Rect};

use super::{LATEST_BAR_HIT, TranscriptV2Host};
use crate::keymap::BmuxKeyMap;
use crate::transcript_v2_host::test_fixtures::{COMPOSER, REGION, attached, paint, style};

fn press(host: &mut TranscriptV2Host, key: &str, at: Instant) {
    let keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());
    let _ = host.handle_key(&keymap, parse_key_stroke(key).expect("key"), at);
}

#[test]
fn geometry_follows_the_transcript_region() {
    let mut host = TranscriptV2Host::new();
    host.sync_geometry(Rect::new(2, 1, 40, 12), Instant::now());
    let geometry = host.engine().geometry();
    assert_eq!((geometry.width, geometry.height), (40, 12));
}

#[test]
fn f6_latest_bar_reserves_its_row_paints_and_jumps_to_latest() {
    let at = Instant::now();
    let mut host = attached(8, at);
    let painted = paint(&mut host, at, Duration::ZERO);
    assert!(!host.latest_bar().visible, "following: no bar");
    assert!(
        !painted
            .hits
            .iter()
            .any(|hit| hit.id.as_str() == LATEST_BAR_HIT)
    );

    press(&mut host, "pageUp", at);
    press(&mut host, "pageUp", at);
    let painted = paint(&mut host, at, Duration::ZERO);
    assert!(host.latest_bar().visible);
    assert_eq!(
        host.engine().geometry().height,
        u64::from(REGION.height) - 1,
        "the bar row is reserved"
    );
    let bar_row = REGION.bottom() - 1;
    assert!(
        painted
            .row(bar_row)
            .contains("New messages below · ctrl+end to jump"),
        "{}",
        painted.row(bar_row)
    );
    let bar = painted
        .hits
        .iter()
        .find(|hit| hit.id.as_str() == LATEST_BAR_HIT)
        .expect("latest-bar hit");
    assert_eq!(bar.area, Rect::new(REGION.x, bar_row, REGION.width, 1));
    assert_eq!(host.regions().latest_bar, Some(bar.area));
    assert!(
        painted
            .hits
            .iter()
            .filter(|hit| hit.id.as_str() != LATEST_BAR_HIT)
            .all(|hit| hit.area.bottom() <= bar_row),
        "B7: no transcript region under the bar"
    );

    let click = |kind| MouseEvent::new(kind, Point::new(3, bar_row));
    let _ = host.handle_mouse(click(MouseEventKind::Down(MouseButton::Left)), 3, None, at);
    let _ = host.handle_mouse(click(MouseEventKind::Up(MouseButton::Left)), 3, None, at);
    assert_eq!(host.engine().nav().mode, NavMode::Following);
    let _ = paint(&mut host, at, Duration::ZERO);
    assert!(!host.latest_bar().visible);
    assert_eq!(host.engine().geometry().height, u64::from(REGION.height));
}

#[test]
fn c21_glow_schedules_frames_within_the_window_and_is_still_under_reduced_motion() {
    let at = Instant::now();
    let mut host = attached(8, at);
    press(&mut host, "pageUp", at);
    press(&mut host, "pageUp", at);
    let _ = paint(&mut host, at, Duration::ZERO);
    assert!(host.latest_bar().visible);
    let activity = at + Duration::from_millis(10);
    let changed = host.ingest_stream_update(
        &crate::history_flow::SessionStreamUpdate::Event(Box::new(bcode_ipc::Event::Session(
            crate::transcript_v2_host::test_fixtures::event(
                host.session().expect("session"),
                25,
                bcode_session_models::SessionEventKind::AssistantMessage {
                    text: "hidden".to_owned(),
                },
            ),
        ))),
        false,
        activity,
    );
    assert!(changed);
    let window = Duration::from_millis(420);
    let _ = paint(&mut host, activity, window);
    let deadline = host.presentation_deadline().expect("glow frame scheduled");
    assert!(deadline > activity && deadline <= activity + window);
    assert!(!host.advance_presentation(activity));
    assert!(host.advance_presentation(deadline));
    assert!(host.is_invalidated());
    // After the window: one still frame and no further deadline.
    let _ = paint(&mut host, activity + window, window);
    assert_eq!(host.presentation_deadline(), None);

    // Reduced motion: never glows.
    let _ = paint(&mut host, activity, Duration::ZERO);
    assert_eq!(host.presentation_deadline(), None);
}

#[test]
fn fo7_focus_highlight_covers_the_focused_target_rows_only() {
    let at = Instant::now();
    let mut host = attached(2, at);
    let painted = paint(&mut host, at, Duration::ZERO);
    assert!(
        (REGION.y..REGION.bottom()).all(|row| !painted.highlighted(0, row)),
        "no highlight without transcript focus"
    );
    press(&mut host, "alt+u", at);
    let painted = paint(&mut host, at, Duration::ZERO);
    let highlighted = (REGION.y..REGION.bottom())
        .filter(|row| painted.highlighted(0, *row))
        .count();
    assert!(highlighted > 0);
    assert!(highlighted < usize::from(REGION.height));
}

#[test]
fn zero_sized_regions_paint_nothing_and_hit_nothing() {
    let at = Instant::now();
    let mut host = attached(2, at);
    host.sync_geometry(Rect::new(0, 1, 0, 0), at);
    let mut buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, 10, 4));
    let mut frame = bmux_tui::frame::Frame::new(&mut buffer);
    host.paint(
        Rect::new(0, 1, 0, 0),
        &mut bmux_tui::paint::PaintCx::new(&mut frame),
        &crate::transcript_v2_host::test_fixtures::style(Duration::ZERO),
        at,
    );
    assert_eq!(frame.hits().regions(), &[]);
    assert_eq!(host.regions().viewport, None);
}

/// Wheel up by one tick at `at` and paint the settled frame one second later.
fn wheel_up_and_settle(host: &mut TranscriptV2Host, at: Instant) -> Instant {
    let _ = host.handle_mouse(
        MouseEvent::new(MouseEventKind::ScrollUp, Point::new(1, REGION.y + 2)),
        3,
        None,
        at,
    );
    at + Duration::from_secs(1)
}

#[test]
fn f5_d6_scrolling_is_transcript_damage_and_a_pure_scroll_becomes_scroll_damage() {
    use super::TranscriptDamage;
    use bcode_tui_transcript::frame::FrameClock;
    use bmux_tui::damage::{Damage, DamagePolicy};

    let at = Instant::now();
    let mut host = attached(12, at);
    let _ = paint(&mut host, at, Duration::ZERO);
    assert_eq!(host.take_damage(), TranscriptDamage::None);
    // The first ticks detach and show the latest bar; the last one scrolls only.
    let mut at = at;
    for _ in 0..2 {
        at = wheel_up_and_settle(&mut host, at);
        let _ = paint(&mut host, at, Duration::ZERO);
    }
    assert!(host.latest_bar().visible);
    let at = wheel_up_and_settle(&mut host, at);
    assert_eq!(host.take_damage(), TranscriptDamage::Transcript);

    let area = Rect::new(0, 0, REGION.width, COMPOSER.bottom());
    let policy = DamagePolicy {
        max_regions: 64,
        max_area_percent: 100,
    };
    let status = Rect::new(0, REGION.bottom(), REGION.width, 1);
    host.sync_geometry(REGION, at);
    host.prepare(REGION, FrameClock { now: at }, &style(Duration::ZERO));
    let damage = host.frame_damage(Damage::regions([REGION, status], area, policy), area);
    let viewport = host.regions().viewport.expect("viewport");
    assert_eq!(damage.scroll_shift(), Some((viewport, -3)), "{damage:?}");
    assert!(
        damage
            .retained_regions()
            .iter()
            .any(|region| region.contains(Point::new(0, status.y))),
        "other damaged regions are still repainted: {damage:?}"
    );
    // Full damage and damage without the transcript region are left alone.
    assert!(host.frame_damage(Damage::Full, area).is_full());
    let composer = Damage::regions([COMPOSER], area, policy);
    assert_eq!(host.frame_damage(composer.clone(), area), composer);

    // Without a scroll (the same position as committed) nothing is a scroll.
    let _ = paint(&mut host, at, Duration::ZERO);
    host.prepare(REGION, FrameClock { now: at }, &style(Duration::ZERO));
    let regional = Damage::regions([REGION], area, policy);
    assert_eq!(host.frame_damage(regional.clone(), area), regional);
}

#[test]
fn f5_focus_kind_changes_need_full_damage() {
    use super::TranscriptDamage;

    let at = Instant::now();
    let mut host = attached(3, at);
    let _ = paint(&mut host, at, Duration::ZERO);
    press(&mut host, "alt+u", at);
    assert_eq!(host.take_damage(), TranscriptDamage::Full);
    // Focus traversal inside the transcript repaints the transcript alone.
    let _ = paint(&mut host, at, Duration::ZERO);
    press(&mut host, "k", at);
    assert_eq!(host.take_damage(), TranscriptDamage::Transcript);
}

#[test]
fn f7_d7_activity_advances_only_on_accepted_frames() {
    let at = Instant::now();
    let mut host = attached(2, at);
    let _ = paint(&mut host, at, Duration::ZERO);
    host.sync_activity(4);
    assert!(host.is_invalidated(), "a new accepted frame repaints");
    let _ = paint(&mut host, at, Duration::ZERO);
    let phase = host.last_frame().expect("frame").activity;
    assert_eq!(phase.spinner_frame, 4);
    // Repeating the same frame (no accepted invalidation) changes nothing, and painting later
    // never advances the phase on wall time.
    host.sync_activity(4);
    assert!(!host.is_invalidated());
    let _ = paint(&mut host, at + Duration::from_secs(5), Duration::ZERO);
    assert_eq!(host.last_frame().expect("frame").activity, phase);
}
