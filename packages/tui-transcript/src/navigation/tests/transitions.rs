//! One test (or more) per transition row T1–T27 and per related rule (N6, N8, N12, P4, CK).

use super::{Fixture, OLDER, has_page, unit};
use crate::focus::FocusTargetId;
use crate::navigation::{
    ApproachEdge, Correspondence, JumpGoal, ListPosition, MotionConfig, NavEffect, NavInput,
    NavIntent, NavMode, NavTimer, PendingJump, PendingMovement, Placement, ReadingAnchor,
    ReadingPosition, RevealRequest, ScrollTarget, UnitRole, VerticalDirection, presented_position,
};
use crate::units::UnitKey;
use crate::window::{PageDirection, PageRequest, WindowMode, WindowRequest};

/// 10-row viewport over boundary + ten 5-row units: 51 rows, content top 41, tail top 50.
fn fixture() -> Fixture {
    Fixture::uniform(10, 10, 5)
}

fn holding(item: &str, sticky: bool) -> NavMode {
    NavMode::Holding {
        item: unit(item),
        sticky,
    }
}

fn reading(anchor: ReadingAnchor) -> NavMode {
    NavMode::Reading { anchor }
}

fn accept(f: &mut Fixture, item: &str, ms: u64) -> Vec<NavEffect> {
    let at = f.at(ms);
    f.reduce(NavInput::SubmissionAccepted {
        item: unit(item),
        at,
    })
}

fn page_result(f: &mut Fixture, inserted_rows: u64, exhausted: bool) -> Vec<NavEffect> {
    let generation = f.window.generation;
    f.reduce(NavInput::PageResult {
        generation,
        direction: PageDirection::Older,
        inserted_rows,
        exhausted,
    })
}

fn window_result(f: &mut Fixture, target: WindowRequest) -> Vec<NavEffect> {
    let generation = f.window.generation;
    f.reduce(NavInput::WindowResult { generation, target })
}

/// A held assistant segment `a` (sticky) below the admitted content.
fn holding_assistant() -> Fixture {
    let mut f = fixture();
    f.admit_all();
    f.role("a", UnitRole::AssistantSegment);
    f.append("a", 3);
    let _ = f.content(1_000);
    assert_eq!(f.state.mode, holding("a", true));
    f
}

#[test]
fn t01_following_stays_following_on_growth() {
    let mut f = fixture();
    f.admit_all();
    f.append("n", 7);
    assert!(!f.content(0).iter().any(|e| matches!(e, NavEffect::Page(_))));
    assert_eq!(f.state.mode, NavMode::Following);
    assert_eq!(f.top(), f.total() - 10);
}

#[test]
fn t02_accepted_submission_is_held_at_its_top_and_clears_earlier_reveals() {
    let mut f = fixture().animated();
    f.admit_all();
    let _ = accept(&mut f, "earlier", 0);
    assert!(f.state.pending_reveals.contains_key(&unit("earlier")));
    f.role("u", UnitRole::UserMessage);
    f.append("u", 2);
    let effects = accept(&mut f, "u", 10);
    assert_eq!(f.state.mode, holding("u", false));
    assert!(f.state.pending_reveals.is_empty());
    let motion = f.state.motion.clone().expect("reveal animates");
    assert_eq!(motion.target, ScrollTarget::ItemTop(unit("u")));
    assert!(effects.iter().any(|e| matches!(
        e,
        NavEffect::ScheduleTimer {
            key: NavTimer::MotionFrame,
            ..
        }
    )));
    // Blank rows below the held unit are allowed (tail space).
    assert_eq!(f.top(), f.offset("u"));
    assert!(f.resolved().blank_rows_below() > 0);
}

#[test]
fn t02_queued_submission_reveals_when_its_unit_becomes_visible() {
    let mut f = fixture();
    f.admit_all();
    let _ = f.scroll(-20, 0); // reading: T2 applies from any mode
    let _ = accept(&mut f, "u", 10);
    assert!(f.state.pending_reveals.contains_key(&unit("u")));
    assert!(matches!(f.state.mode, NavMode::Reading { .. }));
    f.append("u", 2);
    let _ = f.content(20);
    assert_eq!(f.state.mode, holding("u", false));
    assert_eq!(f.top(), f.offset("u"));
}

#[test]
fn t02_cleared_submission_never_anchors() {
    let mut f = fixture();
    f.admit_all();
    let _ = accept(&mut f, "u", 0);
    let _ = f.content_with(
        vec![bcode_session_view_models::TranscriptChange::Removed {
            ids: vec![bcode_session_view_models::TranscriptViewItemId::new("u")],
        }],
        10,
    );
    assert!(f.state.pending_reveals.is_empty());
    f.append("u", 2);
    let _ = f.content(20);
    assert_eq!(f.state.mode, NavMode::Following);
}

#[test]
fn t03_new_assistant_segment_is_held_once_and_supersedes() {
    let mut f = holding_assistant();
    assert_eq!(f.top(), f.offset("a"));
    // A revision of the same segment never re-admits or moves (N10).
    f.set_height("a", 4);
    let effects = f.content(1_100);
    assert_eq!(f.state.mode, holding("a", true));
    assert_eq!(effects, Vec::new());
    // A newer segment supersedes the hold exactly once.
    f.role("b", UnitRole::AssistantSegment);
    f.append("b", 2);
    let _ = f.content(1_200);
    assert_eq!(f.state.mode, holding("b", true));
    assert_eq!(f.top(), f.offset("b"));
}

#[test]
fn t03_reading_reader_is_not_pulled_by_a_new_segment() {
    let mut f = fixture();
    f.admit_all();
    let _ = f.scroll(-10, 0);
    let before = f.state.mode.clone();
    f.role("a", UnitRole::AssistantSegment);
    f.append("a", 3);
    let _ = f.content(1_000);
    assert_eq!(f.state.mode, before);
    // Admitted silently: returning to latest later never reveals it (C16, N10).
    assert!(f.state.admitted.contains(&unit("a")));
}

#[test]
fn t04_non_sticky_hold_hands_off_to_following_on_overflow() {
    let mut f = fixture();
    f.admit_all();
    f.append("u", 2);
    let _ = accept(&mut f, "u", 0);
    f.append("t", 3);
    let _ = f.content(10);
    assert_eq!(
        f.state.mode,
        holding("u", false),
        "fitting content keeps the hold"
    );
    f.append("t2", 20);
    let _ = f.content(20);
    assert_eq!(f.state.mode, NavMode::Following);
    assert_eq!(f.top(), f.total() - 10);
}

#[test]
fn t04_overflow_handoff_waits_for_grace() {
    let mut f = fixture();
    f.admit_all();
    let _ = f.scroll(-1, 0);
    f.append("u", 2);
    let _ = accept(&mut f, "u", 10); // T2 is never suppressed by grace
    assert_eq!(f.state.mode, holding("u", false));
    f.append("t", 20);
    let _ = f.content(20);
    assert_eq!(f.state.mode, holding("u", false));
    let _ = f.timer(NavTimer::GraceExpired, 450);
    assert_eq!(f.state.mode, NavMode::Following);
}

#[test]
fn t05_sticky_hold_survives_overflow() {
    let mut f = holding_assistant();
    f.set_height("a", 30);
    let _ = f.content(1_100);
    assert_eq!(f.state.mode, holding("a", true));
    assert_eq!(f.top(), f.offset("a"));
}

#[test]
fn t06_non_assistant_arrival_unsticks_without_moving_fitting_content() {
    let mut f = holding_assistant();
    let top = f.top();
    f.append("tool", 2);
    let _ = f.content(1_100);
    assert_eq!(f.state.mode, holding("a", false));
    assert_eq!(f.top(), top);
    // The overflow part then hands off (T4).
    f.append("tool2", 20);
    let _ = f.content(1_200);
    assert_eq!(f.state.mode, NavMode::Following);
}

#[test]
fn t07_hold_survives_segment_completion() {
    let mut f = holding_assistant();
    f.set_height("a", 5);
    let _ = f.content(1_100);
    let _ = f.content(1_200);
    assert_eq!(f.state.mode, holding("a", true));
}

#[test]
fn t08_manual_scroll_detaches_cancels_pending_work_and_sets_grace() {
    let mut f = fixture();
    f.state.pending_jump = Some(PendingJump {
        generation: 0,
        request: WindowRequest::Tail,
        goal: JumpGoal::Latest,
    });
    f.state.pending_movement = Some(PendingMovement {
        direction: VerticalDirection::Down,
        rows: 3,
    });
    f.state
        .pending_reveals
        .insert(unit("u"), RevealRequest { requested: f.at(0) });
    let effects = f.scroll(-3, 0);
    assert!(matches!(f.state.mode, NavMode::Reading { .. }));
    assert_eq!(f.top(), 38);
    assert_eq!(f.state.pending_jump, None);
    assert_eq!(f.state.pending_movement, None);
    assert!(f.state.pending_reveals.is_empty());
    assert_eq!(f.state.grace_until, Some(f.at(450)));
    assert!(effects.contains(&NavEffect::ScheduleTimer {
        key: NavTimer::GraceExpired,
        at: f.at(450),
    }));
}

#[test]
fn t08_manual_input_materializes_a_running_motion_n6() {
    let mut f = fixture().animated();
    let _ = f.scroll(-41, 0);
    assert_eq!(f.top(), 0);
    let _ = f.intent(NavIntent::JumpToLatest, 1_000);
    assert!(f.state.motion.is_some());
    let drawn = f.top_at(1_050);
    assert!(drawn > 0 && drawn < 41, "mid-motion sample, got {drawn}");
    let _ = f.scroll(-2, 1_050);
    assert_eq!(f.state.motion, None);
    assert_eq!(f.top(), drawn - 2);
    assert!(matches!(f.state.mode, NavMode::Reading { .. }));
}

#[test]
fn t08_manual_scroll_into_space_below_content_is_tail_space() {
    let mut f = fixture();
    let _ = f.scroll(-5, 0);
    let _ = f.scroll(8, 10);
    assert!(matches!(f.state.mode, NavMode::TailSpace { .. }));
    assert_eq!(f.resolved().blank_rows_below(), 3);
}

#[test]
fn t09_scrolling_down_onto_the_content_bottom_follows() {
    let mut f = fixture();
    let _ = f.scroll(-5, 0);
    assert!(matches!(f.state.mode, NavMode::Reading { .. }));
    let _ = f.scroll(5, 10);
    assert_eq!(f.state.mode, NavMode::Following);
}

#[test]
fn t10_scrolling_down_at_the_bottom_enters_bounded_tail_space() {
    let mut f = fixture();
    let _ = f.scroll(3, 0);
    assert!(matches!(f.state.mode, NavMode::TailSpace { .. }));
    assert_eq!(f.resolved().blank_rows_below(), 3);
    let _ = f.scroll(100, 10);
    assert_eq!(f.resolved().blank_rows_below(), 9, "viewport_height - 1");
}

#[test]
fn t10_detached_bottom_is_not_the_session_tail() {
    let mut f = fixture();
    f.window.mode = WindowMode::Detached;
    f.window.has_newer = true;
    let effects = f.scroll(3, 0);
    assert!(matches!(f.state.mode, NavMode::Reading { .. }));
    assert_eq!(f.resolved().blank_rows_below(), 0);
    assert_eq!(
        f.state.pending_movement,
        Some(PendingMovement {
            direction: VerticalDirection::Down,
            rows: 3,
        })
    );
    assert!(effects.contains(&NavEffect::Page(PageRequest::Newer {
        after_sequence: 2_000
    })));
}

#[test]
fn t11_content_filling_tail_space_resumes_following_even_in_grace() {
    let mut f = fixture();
    f.admit_all();
    let _ = f.scroll(3, 0);
    f.append("n", 2);
    let _ = f.content(10);
    assert!(matches!(f.state.mode, NavMode::TailSpace { .. }));
    assert_eq!(f.resolved().blank_rows_below(), 1);
    f.append("m", 1);
    let _ = f.content(20);
    assert!(f.state.in_grace(f.at(20)));
    assert_eq!(f.state.mode, NavMode::Following);
}

#[test]
fn t12_scrolling_up_in_tail_space_keeps_it_until_content_bottom_leaves() {
    let mut f = fixture();
    let _ = f.scroll(5, 0);
    let _ = f.scroll(-2, 10);
    assert!(matches!(f.state.mode, NavMode::TailSpace { .. }));
    assert_eq!(f.resolved().blank_rows_below(), 3);
    let _ = f.scroll(-5, 20);
    assert!(matches!(f.state.mode, NavMode::Reading { .. }));
    assert_eq!(f.top(), 39);
}

#[test]
fn t13_jump_to_latest_in_a_live_window_follows_via_motion() {
    let mut f = fixture().animated();
    let _ = f.scroll(-20, 0);
    let _ = f.intent(NavIntent::JumpToLatest, 1_000);
    assert_eq!(f.state.mode, NavMode::Following);
    assert_eq!(
        f.state.motion.as_ref().map(|motion| motion.target.clone()),
        Some(ScrollTarget::LatestBottom)
    );
    let _ = f.timer(NavTimer::MotionFrame, 1_100);
    assert_eq!(f.state.motion, None);
    assert_eq!(f.top(), 41);
}

#[test]
fn t14_jump_to_latest_from_a_detached_window_loads_the_tail_directly() {
    let mut f = fixture();
    f.window.mode = WindowMode::Detached;
    let _ = f.scroll(-5, 0);
    let effects = f.intent(NavIntent::Follow, 10);
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(effect, NavEffect::Window(WindowRequest::Tail)))
            .count(),
        1
    );
    assert!(matches!(f.state.mode, NavMode::Reading { .. }));
    f.window.mode = WindowMode::Live;
    f.units.truncate(3);
    let _ = window_result(&mut f, WindowRequest::Tail);
    assert_eq!(f.state.mode, NavMode::Following);
    assert_eq!(f.state.pending_jump, None);
}

#[test]
fn t15_session_start_when_resident_moves_to_the_older_boundary() {
    let mut f = fixture();
    let effects = f.intent(NavIntent::JumpToSessionStart, 0);
    assert!(!effects.iter().any(|e| matches!(e, NavEffect::Window(_))));
    assert_eq!(f.state.mode, reading(ReadingAnchor::top(OLDER)));
    assert_eq!(f.top(), 0);
}

#[test]
fn t16_session_start_when_not_resident_loads_the_head_window() {
    let mut f = fixture();
    f.window.has_older = true;
    let effects = f.intent(NavIntent::JumpToSessionStart, 0);
    assert!(effects.contains(&NavEffect::Window(WindowRequest::Head)));
    assert_eq!(f.state.mode, NavMode::Following);
    f.window.has_older = false;
    let _ = window_result(&mut f, WindowRequest::Head);
    assert_eq!(f.state.mode, reading(ReadingAnchor::top(OLDER)));
}

#[test]
fn t17_resident_jumps_place_the_target_and_cancel_pending_work() {
    let mut f = fixture();
    f.state.pending_movement = Some(PendingMovement {
        direction: VerticalDirection::Up,
        rows: 4,
    });
    let _ = f.intent(
        NavIntent::JumpToItem {
            item: unit("u2"),
            placement: Placement::ItemTopAtTop,
        },
        0,
    );
    assert_eq!(f.state.mode, reading(ReadingAnchor::top(unit("u2"))));
    assert_eq!(f.top(), f.offset("u2"));
    assert_eq!(f.state.pending_movement, None);

    f.sequences.insert(1_500, unit("u5"));
    let effects = f.intent(
        NavIntent::JumpToSequence {
            sequence: 1_500,
            placement: Placement::ItemBottomAtBottom,
        },
        10,
    );
    assert!(!effects.iter().any(|e| matches!(e, NavEffect::Window(_))));
    assert_eq!(f.top(), f.offset("u5") + 5 - 10);
}

#[test]
fn t18_non_resident_sequence_and_fraction_load_an_around_window() {
    let mut f = fixture();
    let effects = f.intent(
        NavIntent::JumpToSequence {
            sequence: 5_000,
            placement: Placement::ItemTopAtTop,
        },
        0,
    );
    assert!(effects.contains(&NavEffect::Window(WindowRequest::Around {
        sequence: 5_000
    })));
    f.units = vec![(OLDER, 1), (unit("x"), 4), (unit("z"), 30), (unit("y"), 30)];
    f.sequences.insert(5_000, unit("z"));
    let _ = window_result(&mut f, WindowRequest::Around { sequence: 5_000 });
    assert_eq!(f.state.mode, reading(ReadingAnchor::top(unit("z"))));
    assert_eq!(f.top(), f.offset("z"));
}

#[test]
fn t18_fraction_maps_to_resident_rows_or_a_sequence_estimate() {
    let mut f = fixture();
    f.window.first_available_sequence = Some(0);
    f.window.last_available_sequence = Some(3_000);
    // before 1000, resident 51, after 1000 estimated rows; max top 2041.
    let effects = f.intent(NavIntent::JumpToFraction { fraction: 0.0 }, 0);
    assert!(effects.contains(&NavEffect::Window(WindowRequest::Around { sequence: 0 })));
    let effects = f.intent(
        NavIntent::JumpToFraction {
            fraction: 1_020.0 / 2_041.0,
        },
        10,
    );
    assert!(!effects.iter().any(|e| matches!(e, NavEffect::Window(_))));
    assert_eq!(f.top(), 20);
    assert_eq!(f.state.pending_jump, None);
}

#[test]
fn t19_focus_target_moves_only_when_not_fully_visible() {
    let mut f = fixture();
    let before = f.state.clone();
    let effects = f.intent(
        NavIntent::FocusTarget {
            target: FocusTargetId::unit(unit("u9")),
            edge: ApproachEdge::Bottom,
        },
        0,
    );
    assert_eq!(effects, Vec::new());
    assert_eq!(f.state, before);
    let _ = f.intent(
        NavIntent::FocusTarget {
            target: FocusTargetId::unit(unit("u0")),
            edge: ApproachEdge::Top,
        },
        10,
    );
    assert_eq!(f.top(), f.offset("u0"), "moving up: first row on top");
    let _ = f.intent(
        NavIntent::FocusTarget {
            target: FocusTargetId::unit(unit("u2")),
            edge: ApproachEdge::Bottom,
        },
        20,
    );
    assert_eq!(
        f.top(),
        f.offset("u2") + 5 - 10,
        "moving down: last row on bottom"
    );
}

#[test]
fn t20_release_hold_stops_where_the_content_is() {
    let mut f = fixture();
    f.admit_all();
    f.append("u", 2);
    let _ = accept(&mut f, "u", 0);
    let top = f.top();
    let _ = f.intent(NavIntent::ReleaseHold, 10);
    assert!(matches!(f.state.mode, NavMode::TailSpace { .. }));
    assert_eq!(f.top(), top);

    let mut f = fixture();
    let _ = f.intent(NavIntent::ReleaseHold, 0);
    assert!(matches!(f.state.mode, NavMode::Reading { .. }));
    assert_eq!(f.top(), 41);
}

#[test]
fn t21_restore_position_resident_latest_and_not_resident() {
    let mut f = fixture();
    let anchor = ReadingAnchor::row(unit("u3"), 2);
    let _ = f.intent(
        NavIntent::RestorePosition {
            position: ReadingPosition::At {
                sequence: 1_300,
                anchor: anchor.clone(),
            },
        },
        0,
    );
    assert_eq!(f.state.mode, reading(anchor));
    let _ = f.intent(
        NavIntent::RestorePosition {
            position: ReadingPosition::Latest,
        },
        10,
    );
    assert_eq!(f.state.mode, NavMode::Following);

    let gone = ReadingAnchor::top(unit("far"));
    let effects = f.intent(
        NavIntent::RestorePosition {
            position: ReadingPosition::At {
                sequence: 9_000,
                anchor: gone.clone(),
            },
        },
        20,
    );
    assert!(effects.contains(&NavEffect::Window(WindowRequest::Around {
        sequence: 9_000
    })));
    f.units = vec![(OLDER, 1), (unit("far"), 20), (unit("after"), 20)];
    let _ = window_result(&mut f, WindowRequest::Around { sequence: 9_000 });
    assert_eq!(f.state.mode, reading(gone));
}

#[test]
fn t22_manual_intent_cancels_a_pending_jump_and_drops_its_result() {
    let mut f = fixture();
    f.window.mode = WindowMode::Detached;
    let _ = f.intent(NavIntent::JumpToLatest, 0);
    assert!(f.state.pending_jump.is_some());
    let _ = f.scroll(-1, 10);
    assert_eq!(f.state.pending_jump, None);
    let mode = f.state.mode.clone();
    let _ = window_result(&mut f, WindowRequest::Tail);
    assert_eq!(f.state.mode, mode);
}

#[test]
fn t23_reading_anchor_is_unchanged_by_any_content() {
    let mut f = fixture();
    f.admit_all();
    let _ = f.scroll(-30, 0);
    let row = f.row_of("u3");
    let mode = f.state.mode.clone();
    let _ = f.prepend(&[("p0", 7)]);
    let _ = f.content(1_000);
    f.append("n", 9);
    f.set_height("u0", 2);
    f.set_height("u9", 1);
    let _ = f.content(1_100);
    assert_eq!(f.state.mode, mode);
    assert_eq!(f.row_of("u3"), row);
}

#[test]
fn t24_grace_expiry_reveals_only_the_newest_deferred_segment() {
    let mut f = fixture();
    f.admit_all();
    let _ = f.scroll(-1, 0);
    let _ = f.scroll(1, 10);
    assert_eq!(f.state.mode, NavMode::Following);
    f.role("a", UnitRole::AssistantSegment);
    f.role("b", UnitRole::AssistantSegment);
    f.append("a", 2);
    let _ = f.content(20);
    f.append("tool", 1);
    f.append("b", 2);
    let _ = f.content(30);
    assert_eq!(
        f.state.mode,
        NavMode::Following,
        "T3 suppressed during grace"
    );
    // A stale timer from an earlier tick is ignored.
    let _ = f.timer(NavTimer::GraceExpired, 100);
    assert_eq!(f.state.mode, NavMode::Following);
    let _ = f.timer(NavTimer::GraceExpired, 460);
    assert_eq!(f.state.grace_until, None);
    assert_eq!(f.state.mode, holding("b", true));
    assert!(f.state.admitted.contains(&unit("a")));
}

#[test]
fn t24_deferred_segment_is_admitted_once_the_reader_detaches() {
    let mut f = fixture();
    f.admit_all();
    let _ = f.scroll(-1, 0);
    let _ = f.scroll(1, 10);
    f.role("a", UnitRole::AssistantSegment);
    f.append("a", 2);
    let _ = f.content(20);
    assert!(f.state.deferred.contains(&unit("a")));
    assert!(!f.state.admitted.contains(&unit("a")));
    // The reader scrolls away during grace; a newer segment is admitted silently (C16) and
    // the deferred one with it, so returning to latest never reveals the older segment.
    let _ = f.scroll(-5, 30);
    f.role("b", UnitRole::AssistantSegment);
    f.append("b", 2);
    let _ = f.content(40);
    assert!(f.state.deferred.is_empty());
    assert!(f.state.admitted.contains(&unit("a")));
    assert!(f.state.admitted.contains(&unit("b")));
    let _ = f.intent(NavIntent::JumpToLatest, 50);
    let _ = f.timer(NavTimer::GraceExpired, 500);
    assert_eq!(f.state.mode, NavMode::Following);
}

#[test]
fn t24_content_after_grace_end_re_evaluates_once() {
    let mut f = fixture();
    f.admit_all();
    let _ = f.scroll(-1, 0);
    let _ = f.scroll(1, 10);
    f.role("a", UnitRole::AssistantSegment);
    f.append("a", 2);
    let _ = f.content(20);
    f.append("tool", 1);
    let _ = f.content(500); // grace ended before the timer was delivered
    assert_eq!(
        f.state.mode,
        holding("a", false),
        "T3, then T6 for the tool"
    );
}

#[test]
fn t25_motion_frames_invalidate_and_completion_resolves_the_keyed_target() {
    let mut f = fixture().animated();
    let _ = f.intent(
        NavIntent::JumpToItem {
            item: unit("u0"),
            placement: Placement::ItemTopAtTop,
        },
        0,
    );
    let effects = f.timer(NavTimer::MotionFrame, 10);
    assert!(effects.contains(&NavEffect::Invalidate(
        crate::navigation::Region::Transcript
    )));
    assert!(effects.contains(&NavEffect::ScheduleTimer {
        key: NavTimer::MotionFrame,
        at: f.at(20),
    }));
    assert!(f.state.motion.is_some());
    // Content arrives above the target during the motion.
    let _ = f.prepend(&[("p0", 7)]);
    let _ = f.content(50);
    let _ = f.timer(NavTimer::MotionFrame, 100);
    assert_eq!(f.state.motion, None);
    assert_eq!(f.state.mode, reading(ReadingAnchor::top(unit("u0"))));
    assert_eq!(f.top(), f.offset("u0"));
}

#[test]
fn t25_reduced_motion_mid_animation_settles_at_the_target() {
    let mut f = fixture().animated();
    let _ = f.scroll(-41, 0);
    let _ = f.intent(NavIntent::JumpToLatest, 1_000);
    assert!(f.state.motion.is_some());
    f.motion = MotionConfig::INSTANT;
    assert_eq!(f.top_at(1_010), 41, "presentation settles immediately");
    let _ = f.timer(NavTimer::MotionFrame, 1_010);
    assert_eq!(f.state.motion, None);
    assert_eq!(f.state.mode, NavMode::Following);
}

/// Reader scrolled 4 rows past the top of a window with older history.
fn pending_at_top() -> (Fixture, Vec<NavEffect>) {
    let mut f = fixture();
    f.window.has_older = true;
    let effects = f.scroll(-45, 0);
    assert_eq!(f.top(), 0);
    (f, effects)
}

#[test]
fn t26_boundary_input_traverses_rows_first_then_waits_for_a_page() {
    let (f, effects) = pending_at_top();
    assert_eq!(
        f.state.pending_movement,
        Some(PendingMovement {
            direction: VerticalDirection::Up,
            rows: 4,
        })
    );
    assert!(effects.contains(&NavEffect::Page(PageRequest::Older {
        before_sequence: 1_000
    })));
}

#[test]
fn t26_page_rows_are_spent_once_against_newly_inserted_rows() {
    let (mut f, _) = pending_at_top();
    assert_eq!(f.row_of("u0"), 1);
    let inserted = f.prepend(&[("p0", 5), ("p1", 5), ("p2", 5), ("p3", 5)]);
    let _ = page_result(&mut f, inserted, false);
    assert_eq!(f.row_of("u0"), 5, "moved up exactly the 4 pending rows");
    assert_eq!(f.state.pending_movement, None);
    assert!(matches!(f.state.mode, NavMode::Reading { .. }));
}

#[test]
fn t26_zero_row_page_keeps_the_movement_and_requests_the_next_page() {
    let (mut f, _) = pending_at_top();
    let effects = page_result(&mut f, 0, false);
    assert_eq!(f.state.pending_movement.map(|p| p.rows), Some(4));
    assert!(has_page(&effects));
}

#[test]
fn t26_short_page_keeps_the_remainder_and_exhaustion_drops_it() {
    let (mut f, _) = pending_at_top();
    let inserted = f.prepend(&[("p0", 1)]);
    let effects = page_result(&mut f, inserted, false);
    assert_eq!(f.state.pending_movement.map(|p| p.rows), Some(3));
    assert!(has_page(&effects));
    assert_eq!(f.top(), 0);
    let inserted = f.prepend(&[("p1", 2)]);
    let _ = page_result(&mut f, inserted, true);
    assert_eq!(f.state.pending_movement, None);
    assert_eq!(f.top(), 0);
}

#[test]
fn t26_reversal_cancels_the_movement() {
    let (mut f, _) = pending_at_top();
    assert!(f.state.pending_movement.is_some());
    let _ = f.scroll(1, 10);
    assert_eq!(f.state.pending_movement, None, "reversal cancels");
}

#[test]
fn t26_page_after_a_generation_bump_still_spends_the_movement() {
    // An abandoned replacement or an eviction that cancelled the page bumps the generation
    // after the movement was stored; the page requested afterwards must still satisfy it.
    let (mut f, _) = pending_at_top();
    f.window.generation += 2;
    let inserted = f.prepend(&[("p0", 10)]);
    let _ = page_result(&mut f, inserted, false);
    assert_eq!(f.row_of("u0"), 5, "moved up exactly the 4 pending rows");
    assert_eq!(f.state.pending_movement, None);
    // Nothing is left to keep requesting history on later content.
    assert!(!has_page(&f.content(20)));
}

#[test]
fn t26_window_replacement_drops_the_movement() {
    let (mut f, _) = pending_at_top();
    f.units = vec![(OLDER, 1), (unit("x"), 30)];
    let _ = window_result(&mut f, WindowRequest::Around { sequence: 7 });
    assert_eq!(f.state.pending_movement, None);
    assert!(!has_page(&f.content(20)));
}

#[test]
fn t26_ceiling_defers_page_requests_until_room_exists_w7a() {
    let mut f = fixture();
    f.window.has_older = true;
    f.window.event_count = 1_100;
    let effects = f.scroll(-45, 0);
    assert!(!has_page(&effects));
    assert!(f.state.pending_movement.is_some());
    f.window.event_count = 500;
    let effects = f.content(10);
    assert!(effects.contains(&NavEffect::Page(PageRequest::Older {
        before_sequence: 1_000
    })));
}

#[test]
fn t27_detached_submission_loads_the_tail_then_reveals() {
    let mut f = fixture();
    f.admit_all();
    f.window.mode = WindowMode::Detached;
    let _ = f.scroll(-5, 0);
    let mode = f.state.mode.clone();
    let effects = accept(&mut f, "u", 10);
    assert!(effects.contains(&NavEffect::Window(WindowRequest::Tail)));
    assert_eq!(f.state.mode, mode);
    assert!(matches!(
        f.state.pending_jump,
        Some(PendingJump {
            goal: JumpGoal::Submission { .. },
            ..
        })
    ));
    f.window.mode = WindowMode::Live;
    f.append("u", 2);
    let _ = window_result(&mut f, WindowRequest::Tail);
    assert_eq!(f.state.mode, holding("u", false));
    assert!(f.state.pending_reveals.is_empty());
}

#[test]
fn t27_manual_intent_before_the_tail_result_keeps_the_reader_detached() {
    let mut f = fixture();
    f.window.mode = WindowMode::Detached;
    let _ = accept(&mut f, "u", 0);
    let _ = f.scroll(-2, 10);
    assert_eq!(f.state.pending_jump, None);
    assert!(f.state.pending_reveals.is_empty());
    let mode = f.state.mode.clone();
    f.append("u", 2);
    let _ = window_result(&mut f, WindowRequest::Tail);
    assert_eq!(f.state.mode, mode);
}

#[test]
fn p4_removed_anchor_moves_to_the_next_survivor_or_the_previous_last_row() {
    let mut f = fixture();
    let _ = f.intent(
        NavIntent::JumpToItem {
            item: unit("u3"),
            placement: Placement::ItemTopAtTop,
        },
        0,
    );
    f.remove(&["u3"]);
    let _ = f.content(10);
    assert_eq!(f.state.mode, reading(ReadingAnchor::top(unit("u4"))));

    let mut f = fixture();
    let _ = f.scroll(3, 0); // tail space at the last unit
    f.remove(&["u8", "u9"]);
    let _ = f.content(10);
    match &f.state.mode {
        NavMode::Reading { anchor } | NavMode::TailSpace { anchor } => {
            assert_eq!(anchor, &ReadingAnchor::row(unit("u7"), 4));
        }
        mode => panic!("unexpected {mode:?}"),
    }
}

#[test]
fn p4_collapsed_member_keeps_its_row_in_the_header() {
    let mut f = fixture();
    let _ = f.intent(
        NavIntent::JumpToItem {
            item: unit("u3"),
            placement: Placement::ItemTopAtTop,
        },
        0,
    );
    let _ = f.scroll(2, 10);
    let header = UnitKey::GroupHeader(bcode_session_view_models::TranscriptViewItemId::new("u3"));
    let position = f
        .units
        .iter()
        .position(|(key, _)| key == &unit("u3"))
        .unwrap();
    f.units[position] = (header.clone(), 3);
    f.removed
        .insert(unit("u3"), Correspondence::Collapsed(header.clone()));
    let _ = f.content(20);
    assert_eq!(f.state.mode, reading(ReadingAnchor::row(header, 2)));
}

#[test]
fn n12_c30_empty_document_follows_and_growth_from_empty_infers_nothing() {
    let mut f = Fixture::new(10, &[]);
    f.units.clear();
    let _ = f.scroll(-3, 0);
    assert_eq!(f.state.mode, NavMode::Following);
    f.role("a", UnitRole::AssistantSegment);
    f.units = vec![(OLDER, 1), (unit("a"), 30)];
    let _ = f.content(1_000);
    assert_eq!(f.state.mode, NavMode::Following);
    assert_eq!(f.top(), 21);
}

#[test]
fn ck_commit_and_presentation_never_change_navigation() {
    let mut f = fixture().animated();
    let _ = f.scroll(-30, 0);
    let _ = f.intent(NavIntent::JumpToLatest, 1_000);
    let before = f.state.clone();
    let at = f.at(1_040);
    let first = f.with_view(|view| presented_position(&f.state, view, at));
    let second = f.with_view(|view| presented_position(&f.state, view, at));
    assert_eq!(first, second);
    assert!(matches!(first, ListPosition::Anchored { .. }));
    let effects = f.reduce(NavInput::PresentationCommitted {
        frame: crate::frame::FrameId(3),
    });
    assert_eq!(effects, Vec::new());
    assert_eq!(f.state, before);
}

#[test]
fn bug_a1_wheel_tick_after_page_arrival_moves_only_by_the_tick() {
    let mut f = fixture();
    f.window.has_older = true;
    let _ = f.scroll(-41, 0); // exactly at the top: anchored on the boundary row
    assert_eq!(f.state.pending_movement, None);
    let row = f.row_of("u0");
    let inserted = f.prepend(&[("p0", 30), ("p1", 30)]);
    let _ = page_result(&mut f, inserted, false);
    assert_eq!(f.row_of("u0"), row);
    let _ = f.scroll(-1, 10);
    assert_eq!(f.row_of("u0"), row + 1);
}

#[test]
fn bug_b2_page_with_eviction_in_the_same_update_keeps_the_reader() {
    let mut f = Fixture::uniform(10, 40, 3);
    f.window.has_older = true;
    let _ = f.intent(
        NavIntent::JumpToItem {
            item: unit("u30"),
            placement: Placement::ItemTopAtTop,
        },
        0,
    );
    let _ = f.scroll(-2, 10);
    let row = f.row_of("u30");
    let inserted = f.prepend(&[("p0", 12), ("p1", 12)]);
    let evicted: Vec<String> = (0..20).map(|n| format!("u{n}")).collect();
    let evicted: Vec<&str> = evicted.iter().map(String::as_str).collect();
    f.remove(&evicted);
    let _ = page_result(&mut f, inserted, false);
    let _ = f.content(20);
    assert_eq!(f.row_of("u30"), row);
    let _ = f.scroll(-1, 30);
    assert_eq!(f.row_of("u30"), row + 1);
}

/// Counts role lookups, the per-unit question the automatic policy asks.
struct CountingFacts {
    lookups: std::cell::Cell<u64>,
}

impl crate::navigation::UnitFacts for CountingFacts {
    fn role(&self, _: &UnitKey) -> UnitRole {
        self.lookups.set(self.lookups.get() + 1);
        UnitRole::Other
    }

    fn unit_for_sequence(&self, _: u64) -> Option<UnitKey> {
        None
    }

    fn correspondence(&self, _: &UnitKey) -> Option<Correspondence> {
        None
    }
}

#[test]
fn d11_streaming_revision_does_not_inspect_every_resident_unit() {
    let mut f = Fixture::uniform(10, 1_000, 2);
    f.admit_all();
    f.set_height("u999", 3);
    let index = f.index();
    let facts = CountingFacts {
        lookups: std::cell::Cell::new(0),
    };
    let view = crate::navigation::NavView {
        index: &index,
        sources: &bmux_tui_components::virtual_list::NoSourceMap,
        facts: &facts,
        viewport: f.viewport,
        window: &f.window,
        motion: f.motion,
    };
    let input = NavInput::Content {
        changes: bcode_session_view_models::TranscriptChanges::default(),
        at: f.at(0),
    };
    let _ = crate::navigation::reduce(&mut f.state, input, &view);
    assert!(
        facts.lookups.get() <= 4,
        "{} role lookups for one revision",
        facts.lookups.get()
    );
}
