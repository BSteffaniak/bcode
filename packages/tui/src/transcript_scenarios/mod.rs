//! Engine-neutral transcript scenario suite (TUI V2 phase 0).
//!
//! Scenarios describe user-visible transcript behavior once and run against every engine
//! through a [`driver::TranscriptScenarioDriver`]. Each scenario is a generic function in
//! [`scenarios`]; each engine instantiates the list with [`transcript_scenarios!`], choosing
//! per engine which scenarios are ignored as known bugs. The status of every scenario is tracked
//! in `docs/tui-v2-scenarios.md`.
//!
//! Observations come from committed frames: items are identified by marker tokens rendered in
//! their text (see [`session::Marker`]), so a scenario never reads engine anchor fields.

pub mod driver;
pub mod frame;
pub mod interaction;
pub mod navigation;
pub mod paging;
pub mod pipeline;
pub mod scenarios;
pub mod session;
pub mod startup;
pub mod v1;
pub mod v2;
pub mod v2_headless;

use std::time::Duration;

use driver::{
    HistoryRequestKind, HistoryResponse, PendingHistoryRequest, ScenarioInput, ScenarioIntent,
    ScenarioSetup, TranscriptScenarioDriver, WheelDirection,
};
use frame::ScenarioFrame;
use session::ScenarioSession;

/// One scenario run: an engine driver, the fake daemon's canonical session and committed frames.
pub struct Scenario<D: TranscriptScenarioDriver> {
    driver: D,
    session: ScenarioSession,
    frames: Vec<ScenarioFrame>,
}

impl<D: TranscriptScenarioDriver> Scenario<D> {
    /// Start the engine for `setup`. No frame is presented yet.
    #[must_use]
    pub fn start(setup: ScenarioSetup) -> Self {
        let driver = D::start(&setup);
        Self {
            driver,
            session: setup.session,
            frames: Vec::new(),
        }
    }

    /// Start with the newest `resident` events of `session` attached at `width`×`height`.
    #[must_use]
    pub fn attach_latest(
        session: ScenarioSession,
        resident: usize,
        width: u16,
        height: u16,
    ) -> Self {
        Self::start(ScenarioSetup {
            session,
            resident: driver::ResidentSeed::Latest { events: resident },
            width,
            height,
        })
    }

    /// The canonical session served by the fake daemon.
    #[must_use]
    pub const fn session(&self) -> &ScenarioSession {
        &self.session
    }

    /// Rows one wheel tick scrolls.
    #[must_use]
    pub fn wheel_rows(&self) -> i32 {
        i32::from(self.driver.wheel_rows())
    }

    /// Present and commit one frame and return it.
    pub fn present(&mut self) -> ScenarioFrame {
        let mut frame = self.driver.present();
        frame.index = self.frames.len();
        self.frames.push(frame.clone());
        frame
    }

    /// The last committed frame.
    ///
    /// # Panics
    ///
    /// Panics when no frame was presented.
    #[must_use]
    pub fn last_frame(&self) -> &ScenarioFrame {
        self.frames.last().expect("scenario presented no frame")
    }

    /// Every committed frame in order.
    #[must_use]
    pub fn frames(&self) -> &[ScenarioFrame] {
        &self.frames
    }

    /// Deliver one input without presenting.
    pub fn input(&mut self, input: ScenarioInput) {
        self.driver.input(input);
    }

    /// Deliver a key stroke by BMUX key name without presenting.
    pub fn key(&mut self, name: &str) {
        self.driver.input(ScenarioInput::Key(name.to_owned()));
    }

    /// Deliver `ticks` wheel ticks over the transcript center without presenting.
    pub fn wheel(&mut self, direction: WheelDirection, ticks: usize) {
        let at = self
            .frames
            .last()
            .map_or(driver::CellPoint { x: 1, y: 1 }, |frame| {
                frame.transcript.center()
            });
        for _ in 0..ticks {
            self.driver.input(ScenarioInput::Wheel { direction, at });
        }
    }

    /// Deliver a navigation intent without presenting.
    pub fn navigate(&mut self, intent: ScenarioIntent) {
        self.driver.navigate(intent);
    }

    /// Advance the engine clock.
    pub fn advance(&mut self, by: Duration) {
        self.driver.advance_clock(by);
    }

    /// Append one assistant item to the canonical session and deliver it live.
    pub fn append_live_assistant(&mut self, lines: u32) -> u64 {
        self.append_live_with(|session| session.push_assistant(lines))
    }

    /// Append the event `build` adds to the canonical session and deliver it live.
    pub fn append_live_with(
        &mut self,
        build: impl FnOnce(&mut ScenarioSession) -> bcode_session_models::SessionEvent,
    ) -> u64 {
        let event = build(&mut self.session);
        let sequence = event.sequence;
        self.driver.deliver_live(event);
        sequence
    }

    /// Pending history requests.
    #[must_use]
    pub fn pending(&self) -> Vec<PendingHistoryRequest> {
        self.driver.pending_requests()
    }

    /// First pending request of `kind`.
    #[must_use]
    pub fn pending_of(&self, kind: HistoryRequestKind) -> Option<PendingHistoryRequest> {
        self.pending()
            .into_iter()
            .find(|pending| pending.request.kind() == kind)
    }

    /// Answer `pending` from the canonical session and deliver the result without presenting.
    pub fn complete(&mut self, pending: &PendingHistoryRequest) -> HistoryResponse {
        let response = self.session.answer(&pending.request);
        self.driver.complete_request(pending.id, response.clone());
        response
    }

    /// Deliver `response`, answered earlier from the canonical session, as the result of
    /// `pending` without presenting: the daemon read it before later live events existed.
    pub fn complete_with(&mut self, pending: &PendingHistoryRequest, response: HistoryResponse) {
        self.driver.complete_request(pending.id, response);
    }

    /// Answer the first pending request of `kind`, if any.
    pub fn complete_kind(&mut self, kind: HistoryRequestKind) -> Option<HistoryResponse> {
        let pending = self.pending_of(kind)?;
        Some(self.complete(&pending))
    }

    /// Deliver one wheel tick and present, repeating until a request of `kind` is pending.
    ///
    /// Returns the number of ticks delivered, or `None` when `max_ticks` passed without one.
    pub fn wheel_until_request(
        &mut self,
        direction: WheelDirection,
        kind: HistoryRequestKind,
        max_ticks: usize,
    ) -> Option<usize> {
        for tick in 1..=max_ticks {
            self.wheel(direction, 1);
            self.present();
            if self.pending_of(kind).is_some() {
                return Some(tick);
            }
        }
        None
    }

    /// Resident canonical sequence range.
    #[must_use]
    pub fn resident(&self) -> Option<(u64, u64)> {
        self.driver.resident_sequences()
    }

    /// Canonical events resident, when the driver reports it.
    #[must_use]
    pub fn resident_event_count(&self) -> Option<u64> {
        self.driver.resident_event_count()
    }

    /// Resident-window operation work so far, when the driver counts it.
    #[must_use]
    pub fn window_work(&self) -> Option<driver::WindowWork> {
        self.driver.window_work()
    }

    /// Apply one engine-neutral setting without presenting.
    pub fn configure(&mut self, setting: &driver::ScenarioSetting) {
        self.driver.configure(setting);
    }

    /// Fail with `message` and the last two committed frames.
    ///
    /// # Panics
    ///
    /// Always.
    #[track_caller]
    pub fn fail(&self, message: &str) -> ! {
        let mut context = String::new();
        for frame in self.frames().iter().rev().take(2).rev() {
            context.push_str(&frame.text());
        }
        panic!(
            "[{:?}] scenario failed: {message}\npending={:?} resident={:?} selection={:?}\n{context}",
            D::ENGINE,
            self.pending(),
            self.resident(),
            self.driver.selection_text()
        );
    }

    /// Fail unless `condition` holds.
    #[track_caller]
    pub fn check(&self, condition: bool, message: impl FnOnce() -> String) {
        if !condition {
            self.fail(&message());
        }
    }
}

/// Instantiate scenarios from [`scenarios`] as tests for one engine driver.
///
/// The `in <module>;` form instantiates scenarios defined in another scenario module of this
/// harness (for example [`navigation`]) instead of [`scenarios`].
///
/// ```ignore
/// mod v1_engine {
///     crate::transcript_scenarios::transcript_scenarios! { crate::transcript_scenarios::v1::V1Driver;
///         page_then_frame_preserves_reader,
///         #[ignore = "V1 bug BUG-A1: wheel tick before the next frame loses the anchor"]
///         bug_a1_wheel_between_older_page_and_frame,
///     }
/// }
/// ```
macro_rules! transcript_scenarios {
    ($driver:ty; in $module:ident; $($(#[$meta:meta])* $name:ident),* $(,)?) => {
        $(
            $(#[$meta])*
            #[tokio::test]
            async fn $name() {
                $crate::transcript_scenarios::$module::$name::<$driver>();
            }
        )*
    };
    ($driver:ty; $($(#[$meta:meta])* $name:ident),* $(,)?) => {
        $(
            $(#[$meta])*
            #[tokio::test]
            async fn $name() {
                $crate::transcript_scenarios::scenarios::$name::<$driver>();
            }
        )*
    };
}
pub(crate) use transcript_scenarios;

/// Scenario instantiation for the V1 engine. Ignored entries fail on V1 (confirmed with
/// `--ignored`); `docs/tui-v2-scenarios.md` records the observed failure of each.
mod v1_engine {
    super::transcript_scenarios! { super::v1::V1Driver;
        t1_live_append_while_following_shows_latest,
        t23_live_append_while_reading_keeps_position,
        c29_older_page_then_frame_preserves_reader,
        #[ignore = "V1 bug BUG-A1: wheel tick before the next frame maps the anchor to a stale index and leaps a page"]
        bug_a1_wheel_between_older_page_and_frame,
        #[ignore = "V1 bug BUG-A2: no prefetch; the view stalls at the boundary while a page loads"]
        bug_a2_continuous_wheel_moves_without_stall_or_leap,
        bug_a3_eviction_after_prepend_keeps_visible_content,
        #[ignore = "V1 bug BUG-A4: the history banner disappears at exhaustion and shifts content by a row"]
        bug_a4_session_start_does_not_shift_content,
        #[ignore = "V1 bug BUG-B1: the first newer page after a timeline jump evicts about 500 older events"]
        bug_b1_timeline_window_first_newer_page_keeps_window,
        #[ignore = "V1 bug BUG-B2: newer page plus eviction then a wheel tick before the frame flings into blank space"]
        bug_b2_wheel_between_newer_page_with_eviction_and_frame,
        #[ignore = "V1 bug BUG-B3: jump-to-latest issues no tail-window request and only scrolls to the bottom of the historical window"]
        bug_b3_jump_to_latest_after_deep_jump_returns_live,
        bug_b4_resident_jump_while_detached_lands_and_stays,
        #[ignore = "V1 deviation C11: an overflowing streaming response bottom-follows instead of holding its top"]
        c11_streaming_response_reveals_top_and_holds,
        #[ignore = "V1 deviation C11: no reveal motion runs for an overflowing streaming response (depends on C11)"]
        c17_reflow_during_reveal_motion_keeps_target,
        c17_height_change_during_latest_motion_lands_on_latest,
        #[ignore = "V1 deviation C17: a live item during the jump-to-latest motion keeps a motion running indefinitely"]
        c17_live_item_during_latest_motion_settles,
    }
}

/// V1 instantiation of the kept-behavior navigation scenarios in [`navigation`]. Ignored entries
/// fail on V1 (confirmed with `--ignored`); `docs/tui-v2-scenarios.md` records each observation.
mod v1_navigation {
    super::transcript_scenarios! { super::v1::V1Driver; in navigation;
        c1_attach_follows_latest_bottom,
        #[ignore = "V1 deviation C1: wheeling back down to the content bottom stays anchored, so later live content is not followed"]
        c1_wheel_up_reads_and_wheel_down_resumes_following,
        c2_wheel_down_at_bottom_enters_tail_space,
        c2_tail_space_is_bounded_by_the_viewport,
        c3_tail_space_position_is_stable_across_frames,
        #[ignore = "V1 deviation C4: after content fills the tail space a motion keeps running and following is never reported (content is correct)"]
        c4_new_content_fills_tail_space_then_follows,
        c5_reflow_round_trip_does_not_drift,
        c6_rows_inserted_above_do_not_consume_tail_space,
        c7_wheel_up_inside_tail_space_keeps_tail_space,
        #[ignore = "V1 deviation C8: a staged submission while following moves the view to the transcript top"]
        c8_staged_submission_does_not_move_view,
        #[ignore = "V1 deviation C8: after the staged submission moves to the top, acceptance never reveals the message and a motion keeps running"]
        c8_accepted_submission_reveals_message_at_top,
        c8_cleared_local_command_submission_does_not_anchor,
        #[ignore = "V1 deviation C8: depends on the accepted-submission reveal, which V1 does not perform here"]
        c9_tool_activity_after_submission_resumes_following,
        #[ignore = "V1 deviation C11: a new assistant segment below history follows the bottom instead of being revealed at the top"]
        c10_later_stream_updates_do_not_move_revealed_response,
        #[ignore = "V1 deviation C11: a new assistant segment below history follows the bottom instead of being revealed at the top"]
        c11_assistant_hold_survives_completion,
        #[ignore = "V1 deviation C11: a new assistant segment below history follows the bottom instead of being revealed at the top"]
        c11_non_assistant_item_releases_hold_into_overflow_follow,
        c12_revealed_item_is_not_readmitted,
        #[ignore = "V1 deviation C11: an overflowing streaming response follows the bottom instead of holding its top"]
        c13_sticky_reveal_hands_off_after_segment_end,
        c14_manual_scroll_materializes_running_motion,
        #[ignore = "V1 deviation C14: manual scroll does not cancel a pending non-resident timeline jump; its result replaces the view"]
        c14_manual_scroll_cancels_pending_jump,
        c15_grace_suppresses_stream_top_anchor,
        c16_stream_does_not_anchor_when_scrolled_up,
        c17_latest_bar_motion_lands_on_latest_bottom,
        c17_pending_jump_never_falls_back_to_tail,
        c22_reduced_motion_jump_is_instant,
        #[ignore = "V1 deviation C22: enabling reduce_motion during a motion lets the motion keep running"]
        c22_enabling_reduced_motion_mid_motion_settles_at_target,
        c23_reader_survives_page_and_live_append_before_frame,
        c24_width_change_keeps_source_line_at_top,
        c25_structural_insertion_keeps_reading_position,
        c25_collapse_above_keeps_reading_position,
        c26_runtime_work_events_do_not_move_final_response,
        c27_repeated_presentation_is_idempotent,
        #[ignore = "driver: needs presentation failure injection"]
        c27_failed_presentation_does_not_commit_navigation,
        c28_movement_kept_while_page_loads_is_spent_once,
        c29_direction_reversal_cancels_pending_movement,
        c30_growth_while_following_is_not_a_prepend,
        c31_window_bottom_is_not_session_tail,
        c32_focus_up_puts_target_top_on_top_edge,
        c32_focus_down_puts_target_bottom_on_bottom_edge,
        c32_focus_on_visible_target_does_not_scroll,
        c33_initial_focus_is_last_visible_target,
        c33_initial_focus_falls_back_to_nearest_target_above,
        fo4_composer_home_end_do_not_move_transcript,
        #[ignore = "V1 deviation FO4: Home in transcript focus does not load the session start directly"]
        fo4_transcript_home_loads_session_start_directly,
        #[ignore = "V1 deviation FO4: End in transcript focus does not return to the latest bottom"]
        fo4_transcript_end_returns_to_latest,
        fo5_page_up_scrolls_from_composer_focus,
    }
}

/// V1 instantiation of the kept-behavior interaction scenarios in [`interaction`].
mod v1_interaction {
    super::transcript_scenarios! { super::v1::V1Driver; in interaction;
        c18_latest_bar_ignores_hidden_continuation_of_visible_entry,
        c18_latest_bar_shows_for_distinct_entry_fully_below,
        c18_live_entry_below_reader_shows_bar,
        #[ignore = "V1 deviation C17: jump-to-latest after a live entry keeps a motion running and never resumes following"]
        c18_jump_to_latest_after_live_entry_follows_and_clears_bar,
        c19_latest_bar_never_causes_its_own_visibility,
        c20_scroll_timer_and_reflow_generate_no_activity,
        #[ignore = "V1 deviation C20/C21: the latest-bar glow never settles, also when time advances through the production timed-invalidation path (root cause not diagnosed)"]
        c21_latest_bar_glow_settles_still_after_window,
        c21_reduced_motion_latest_bar_is_still_immediately,
        #[ignore = "V1 deviation C34: a drag that starts inside a response extends into later transcript items"]
        c34_drag_inside_response_stays_within_it,
        c34_drag_from_surrounding_space_spans_items,
        c34_soft_wrapped_line_selects_without_newlines,
        c36_code_selection_preserves_indentation_and_blank_lines,
        c34_selection_survives_scroll,
        #[ignore = "V1 deviation C34: the selection is invalidated once its rows scroll out of view (BMUX 13)"]
        c34_selection_survives_scrolling_out_of_view,
        #[ignore = "V1 deviation C34: copying across a Markdown hard line break glues the two lines together"]
        c34_hard_line_break_copies_as_line_break,
        c35_release_copies_and_clears_after_delay,
        c35_failed_copy_keeps_selection,
        c35_new_gesture_cancels_pending_clear,
        c35_click_without_drag_does_not_copy,
        x1_click_toggles_tool_header_once_per_click,
        x1_click_toggles_section_label,
        x1_click_toggles_group_header,
        x1_drag_from_header_selects_without_toggling,
        x2_focus_mode_keys_walk_and_toggle_targets,
        x2_focus_walk_scrolls_targets_into_view,
        x3_collapsed_group_reveals_failed_member,
        a16_hidden_units_leave_marker_and_failed_stays_visible,
        a16_floor_applies_after_manual_toggles,
        a16_awaiting_decision_stays_expanded_under_rules,
        c38_streaming_markdown_converges_to_final,
        #[ignore = "V1 bug D-16: every wheel tick marks full damage"]
        d6_wheel_scroll_repaints_regionally,
        #[ignore = "V1 bug D-16: composer input marks full damage"]
        d6_composer_typing_repaints_regionally,
        #[ignore = "driver: needs per-frame layout work counters"]
        d4_scrolling_does_no_layout_work,
        #[ignore = "driver: needs per-frame layout work counters"]
        d4_prepend_measures_only_new_units,
        #[ignore = "driver: needs plugin interaction surfaces"]
        c37_interaction_placement_and_focus_restore,
        #[ignore = "driver: needs positioned shell captures"]
        c39_inline_shell_expansion,
    }
}

/// Scenario instantiation for the V2 engine in the application (phase 4): the root model with the
/// V2 host selected, driven through `update` and the root presenter ([`v2::V2Driver`]).
/// Ignored entries need a later phase's deliverable or a driver capability and fail when run
/// (confirmed with `--ignored`); `docs/tui-v2-scenarios.md` records each status.
mod v2_engine {
    super::transcript_scenarios! { super::v2::V2Driver;
        t1_live_append_while_following_shows_latest,
        t23_live_append_while_reading_keeps_position,
        c29_older_page_then_frame_preserves_reader,
        bug_a1_wheel_between_older_page_and_frame,
        bug_a2_continuous_wheel_moves_without_stall_or_leap,
        bug_a3_eviction_after_prepend_keeps_visible_content,
        bug_a4_session_start_does_not_shift_content,
        bug_b1_timeline_window_first_newer_page_keeps_window,
        bug_b2_wheel_between_newer_page_with_eviction_and_frame,
        bug_b3_jump_to_latest_after_deep_jump_returns_live,
        bug_b4_resident_jump_while_detached_lands_and_stays,
        c11_streaming_response_reveals_top_and_holds,
        c17_reflow_during_reveal_motion_keeps_target,
        c17_height_change_during_latest_motion_lands_on_latest,
        c17_live_item_during_latest_motion_settles,
    }
}

/// Engine-level instantiation of the [`scenarios`] list against the V2 engine façade without
/// the host ([`v2_headless::V2HeadlessDriver`]). These scenarios need no composer, focus mode or
/// latest bar, so a failure here isolates the engine from the host. The two `c17_…_latest_motion_…`
/// scenarios start their motion from a latest-bar click, which only the host paints, so they run
/// in [`v2_engine`] only.
mod v2_headless_engine {
    super::transcript_scenarios! { super::v2_headless::V2HeadlessDriver;
        t1_live_append_while_following_shows_latest,
        t23_live_append_while_reading_keeps_position,
        c29_older_page_then_frame_preserves_reader,
        bug_a1_wheel_between_older_page_and_frame,
        bug_a2_continuous_wheel_moves_without_stall_or_leap,
        bug_a3_eviction_after_prepend_keeps_visible_content,
        bug_a4_session_start_does_not_shift_content,
        bug_b1_timeline_window_first_newer_page_keeps_window,
        bug_b2_wheel_between_newer_page_with_eviction_and_frame,
        bug_b3_jump_to_latest_after_deep_jump_returns_live,
        bug_b4_resident_jump_while_detached_lands_and_stays,
        c11_streaming_response_reveals_top_and_holds,
        c17_reflow_during_reveal_motion_keeps_target,
    }
}

/// Resident window and paging scenarios in [`paging`] (phase 5) for the V2 engine in the
/// application. V1 is not instantiated: the scenarios check V2's window-operation counters and
/// resident ceiling, which V1 does not report (`docs/tui-v2-scenarios.md`).
mod v2_paging {
    super::transcript_scenarios! { super::v2::V2Driver; in paging;
        x12_wheel_up_through_long_history_is_continuous,
        x12_wheel_bursts_with_live_events_stay_continuous,
        x12_wheel_down_from_session_start_returns_live,
        phase5_paging_performs_no_full_rebuild,
        phase5_tool_heavy_paging_after_a_jump_is_incremental,
        w5_prefetch_requests_one_page_before_the_boundary,
        bug_b5_timeline_window_is_bounded_and_turn_aligned,
        w11_newer_page_reaching_tail_applies_buffered_live_events,
        w11_live_buffer_overflow_rehydrates_live_state,
        bug_b3_end_in_transcript_focus_after_deep_jump_returns_live,
        x6_scrollbar_thumb_is_placed_within_the_whole_session,
    }
}

/// Engine-level instantiation of the [`paging`] scenarios that need no host (no focus mode and
/// no scrollbar, which the host paints and hit-tests).
mod v2_headless_paging {
    super::transcript_scenarios! { super::v2_headless::V2HeadlessDriver; in paging;
        x12_wheel_up_through_long_history_is_continuous,
        x12_wheel_bursts_with_live_events_stay_continuous,
        x12_wheel_down_from_session_start_returns_live,
        phase5_paging_performs_no_full_rebuild,
        phase5_tool_heavy_paging_after_a_jump_is_incremental,
        w5_prefetch_requests_one_page_before_the_boundary,
        bug_b5_timeline_window_is_bounded_and_turn_aligned,
        w11_newer_page_reaching_tail_applies_buffered_live_events,
        w11_live_buffer_overflow_rehydrates_live_state,
    }
}

/// V2 instantiation of the kept-behavior navigation scenarios in [`navigation`].
mod v2_navigation {
    super::transcript_scenarios! { super::v2::V2Driver; in navigation;
        c1_attach_follows_latest_bottom,
        c1_wheel_up_reads_and_wheel_down_resumes_following,
        c2_wheel_down_at_bottom_enters_tail_space,
        c2_tail_space_is_bounded_by_the_viewport,
        c3_tail_space_position_is_stable_across_frames,
        c4_new_content_fills_tail_space_then_follows,
        c5_reflow_round_trip_does_not_drift,
        c6_rows_inserted_above_do_not_consume_tail_space,
        c7_wheel_up_inside_tail_space_keeps_tail_space,
        c8_staged_submission_does_not_move_view,
        c8_accepted_submission_reveals_message_at_top,
        c8_cleared_local_command_submission_does_not_anchor,
        c9_tool_activity_after_submission_resumes_following,
        c10_later_stream_updates_do_not_move_revealed_response,
        c11_assistant_hold_survives_completion,
        c11_non_assistant_item_releases_hold_into_overflow_follow,
        c12_revealed_item_is_not_readmitted,
        c13_sticky_reveal_hands_off_after_segment_end,
        c14_manual_scroll_materializes_running_motion,
        c14_manual_scroll_cancels_pending_jump,
        c15_grace_suppresses_stream_top_anchor,
        c16_stream_does_not_anchor_when_scrolled_up,
        c17_latest_bar_motion_lands_on_latest_bottom,
        c17_pending_jump_never_falls_back_to_tail,
        c22_reduced_motion_jump_is_instant,
        c22_enabling_reduced_motion_mid_motion_settles_at_target,
        c23_reader_survives_page_and_live_append_before_frame,
        c24_width_change_keeps_source_line_at_top,
        c25_structural_insertion_keeps_reading_position,
        c25_collapse_above_keeps_reading_position,
        c26_runtime_work_events_do_not_move_final_response,
        c27_repeated_presentation_is_idempotent,
        #[ignore = "driver: needs presentation failure injection"]
        c27_failed_presentation_does_not_commit_navigation,
        c28_movement_kept_while_page_loads_is_spent_once,
        c29_direction_reversal_cancels_pending_movement,
        c30_growth_while_following_is_not_a_prepend,
        c31_window_bottom_is_not_session_tail,
        c32_focus_up_puts_target_top_on_top_edge,
        c32_focus_down_puts_target_bottom_on_bottom_edge,
        c32_focus_on_visible_target_does_not_scroll,
        c33_initial_focus_is_last_visible_target,
        c33_initial_focus_falls_back_to_nearest_target_above,
        fo4_composer_home_end_do_not_move_transcript,
        fo4_transcript_home_loads_session_start_directly,
        fo4_transcript_end_returns_to_latest,
        fo5_page_up_scrolls_from_composer_focus,
    }
}

/// V2 instantiation of the kept-behavior interaction scenarios in [`interaction`]. The C37 and
/// C39 driver stubs are not instantiated: the V2 driver opens interaction surfaces and expands
/// shell screens, and [`pipeline`] covers both criteria for V2 (`v2_pipeline`).
mod v2_interaction {
    super::transcript_scenarios! { super::v2::V2Driver; in interaction;
        c18_latest_bar_ignores_hidden_continuation_of_visible_entry,
        c18_latest_bar_shows_for_distinct_entry_fully_below,
        c18_live_entry_below_reader_shows_bar,
        c18_jump_to_latest_after_live_entry_follows_and_clears_bar,
        c19_latest_bar_never_causes_its_own_visibility,
        c20_scroll_timer_and_reflow_generate_no_activity,
        c21_latest_bar_glow_settles_still_after_window,
        c21_reduced_motion_latest_bar_is_still_immediately,
        c34_drag_inside_response_stays_within_it,
        c34_drag_from_surrounding_space_spans_items,
        c34_soft_wrapped_line_selects_without_newlines,
        c36_code_selection_preserves_indentation_and_blank_lines,
        c34_selection_survives_scroll,
        c34_selection_survives_scrolling_out_of_view,
        c34_hard_line_break_copies_as_line_break,
        c35_release_copies_and_clears_after_delay,
        c35_failed_copy_keeps_selection,
        c35_new_gesture_cancels_pending_clear,
        c35_click_without_drag_does_not_copy,
        x1_click_toggles_tool_header_once_per_click,
        x1_click_toggles_section_label,
        x1_click_toggles_group_header,
        x1_drag_from_header_selects_without_toggling,
        x2_focus_mode_keys_walk_and_toggle_targets,
        x2_focus_walk_scrolls_targets_into_view,
        x3_collapsed_group_reveals_failed_member,
        a16_hidden_units_leave_marker_and_failed_stays_visible,
        a16_floor_applies_after_manual_toggles,
        a16_awaiting_decision_stays_expanded_under_rules,
        c38_streaming_markdown_converges_to_final,
        d6_wheel_scroll_repaints_regionally,
        d6_composer_typing_repaints_regionally,
        d4_scrolling_does_no_layout_work,
        d4_prepend_measures_only_new_units,
    }
}

/// Frame pipeline, damage and transcript tooling scenarios in [`pipeline`] (phase 6) for the V2
/// engine in the application. V1 is not instantiated: the scenarios read the terminal's committed
/// damage, the host's command routes and inspector, and the V2 inline interaction and shell
/// tooling (`docs/tui-v2-scenarios.md`).
mod v2_pipeline {
    super::transcript_scenarios! { super::v2::V2Driver; in pipeline;
        f1_cells_hits_and_focus_come_from_one_snapshot,
        f2_f3_presentation_is_pure_and_reads_no_clock,
        d7_activity_labels_advance_on_accepted_frames,
        d6_wheel_tick_is_scroll_damage_of_the_viewport,
        d6_transcript_changes_repaint_the_transcript_region,
        d4_focus_and_selection_do_no_layout_work,
        fo6_inspector_follows_focus_scrolls_and_closes_first,
        fo6_make_default_applies_the_focused_disclosure_to_its_tool,
        fo6_detail_cycle_applies_to_every_unit,
        fo6_bulk_disclosure_routes_apply_to_units_or_the_focused_subtree,
        visuals_image_notice_replaces_the_placeholder,
        visuals_mermaid_without_images_keeps_its_source,
        c37_inline_interaction_scrolls_with_the_transcript_and_restores,
        c39_shell_expansion_opens_up_to_128_rows,
        #[ignore = "V2 pending phase 7: local horizontal scroll inside the inline shell screen (C39) is not implemented"]
        c39_shell_screen_scrolls_horizontally,
    }
}
