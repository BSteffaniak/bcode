# TUI V2 transcript scenario status

This manifest lists every engine-neutral transcript scenario. For each one it gives the
requirement or bug it covers and its status on each engine. The scenarios assert the behavior
required by [`tui-v2-architecture.md`](tui-v2-architecture.md), not what V1 currently does.

## Where the scenarios live

* Harness: `packages/tui/src/transcript_scenarios/`. It is a `cfg(test)` module of
  `bcode_tui`.
  * `driver.rs` defines the engine-neutral `TranscriptScenarioDriver` contract, plus the
    semantic history requests and inputs.
  * `session.rs` is the fake daemon: canonical history and per-line marker tokens
    (`S00042L003` means sequence 42, line 3).
  * `frame.rs` holds observations taken from the committed frame: visible items and their rows,
    the top marker, content shift, latest bar, following state, motion and focus.
  * `mod.rs` contains the `Scenario<D>` runner, the `transcript_scenarios!` instantiation macro
    and the per-engine instantiation lists.
  * `scenarios.rs` holds the scenario definitions, each a generic `fn name<D: TranscriptScenarioDriver>()`.
  * `v1.rs` drives the real `BcodeRuntimeModel` through `Program::update` and the production
    root presenter.
  * `v2.rs` drives the real `BcodeRuntimeModel` with the V2 engine selected through
    `Program::update` and the production root presenter (phase 4; see "V2 driver" below).
  * `v2_headless.rs` drives the `bcode_tui_transcript` engine façade without the host, for
    engine-level checks (`v2_headless_engine` in `mod.rs`).
  * `startup.rs` checks engine selection at startup through the root model (X7, X8) and that
    both engines attach to the same scripted session.
* Commands:
  * Run the active suite: `cargo test -p bcode_tui --lib transcript_scenarios`
  * Run one engine: `cargo test -p bcode_tui --lib transcript_scenarios::v2` (or `::v1`;
    `::v2_headless` for the engine façade only)
  * Check known failures: `cargo test -p bcode_tui --lib transcript_scenarios -- --ignored`

## Status values

| Value | Meaning |
|---|---|
| pass | Runs as an active test and passes. |
| fail-ignored | Instantiated with `#[ignore = "<engine> bug <ID>: …"]`. It fails when run with `--ignored`; the observed failure is recorded below. |
| pending-ignored | Instantiated with `#[ignore = "V2 pending phase <N>: <capability>"]`: the scenario needs a later phase's deliverable. It fails when run with `--ignored`; the V2 column names the phase and capability. |
| stub-ignored | Instantiated with `#[ignore = "driver: needs <capability>"]`: no driver can express the scenario yet. |

When a fix lands, remove the engine's `#[ignore]` and change the status in the same change.

## Scenarios

| Scenario | Covers | V1 | V2 | V1 observation |
|---|---|---|---|---|
| `t1_live_append_while_following_shows_latest` | T1, C10 | pass | pass | — |
| `t23_live_append_while_reading_keeps_position` | T23, C16 | pass | pass | — |
| `c29_older_page_then_frame_preserves_reader` | C29, C28 | pass | pass | When the page lands before the next frame with no input in between, the reader is preserved. |
| `bug_a1_wheel_between_older_page_and_frame` | BUG-A1, C23 | fail-ignored | pass | A wheel tick between the page arriving and the frame makes content leap by about the page height (the reference markers leave the viewport). |
| `bug_a2_continuous_wheel_moves_without_stall_or_leap` | BUG-A2, X12, C28 | fail-ignored | pass | Stalls at step 49: the frame does not move while the boundary page loads (V1 does not prefetch). |
| `bug_a3_eviction_after_prepend_keeps_visible_content` | BUG-A3, W7 | pass | pass | The latent stale-index trim was not reproduced. Visible content is preserved and the window returns under the cap. |
| `bug_a4_session_start_does_not_shift_content` | BUG-A4, W13 | fail-ignored | pass | An exhausting page with no rows removes the history banner, and content shifts up by 1 row. |
| `bug_b1_timeline_window_first_newer_page_keeps_window` | BUG-B1, W8 | fail-ignored | pass | The first newer page after a timeline jump evicts older content: the first resident sequence goes from 489 to 1013. |
| `bug_b2_wheel_between_newer_page_with_eviction_and_frame` | BUG-B2, C23 | fail-ignored | pass | A newer page with eviction, then a wheel tick before the frame, commits a blank transcript viewport. |
| `bug_b3_jump_to_latest_after_deep_jump_returns_live` | BUG-B3, X11, W10 | fail-ignored | pass | Fails at the request check: jump-to-latest issues no request (the scenario requires exactly one tail-window request and no further non-prefetch requests). It stops at the bottom of the historical window (sequence 1012 of 2000). |
| `bug_b4_resident_jump_while_detached_lands_and_stays` | BUG-B4, T17 | pass | pass | Not reproduced. Variants tried: detached reader, detached timeline window, a live event before the next frame, and a live event after landing. |
| `c11_streaming_response_reveals_top_and_holds` | C11, C13, T3, T5 | fail-ignored | pass | V1 deviation, root cause not diagnosed: a streaming response that grows past the viewport follows the bottom instead of holding its top. |
| `c17_reflow_during_reveal_motion_keeps_target` | C17, C23, N7 | fail-ignored | pass | Depends on C11: V1 starts no reveal motion for this input. |
| `c17_height_change_during_latest_motion_lands_on_latest` | C17, C5, T13 | pass | pass | Reflow during the latest-bar motion settles following the latest bottom. |
| `c17_live_item_during_latest_motion_settles` | C17, N7, C12 | fail-ignored | pass | V1 deviation, root cause not diagnosed: a live assistant item that arrives during the jump-to-latest motion keeps a motion running through 6 s of scenario time. The content is correct. |

Height changes during a motion are produced by reflow (a narrower terminal), not by an
asynchronous visual, because the fake session has no asynchronous visual producer. Add a
true async-visual variant once an engine exposes an injectable visual producer.

BUG-B1, BUG-B2 and BUG-B3 are phase-5 exit criteria but already pass on V2: the window manager
aligns timeline windows to turns (W4, W8), loads `Head`/`Tail` windows directly (W10) and returns
to live from a tail window. Phase 5 replaces the document rebuild behind them with incremental
window operations.

## Navigation scenarios (kept behavior)

Defined in `packages/tui/src/transcript_scenarios/navigation.rs` and instantiated for V1 in
`mod v1_navigation` with `transcript_scenarios! { Driver; in navigation; … }`. They cover the
kept navigation behavior C1–C17 and C22–C33, plus the focus model's Home/End and page-key
rules (FO4, FO5). Run them alone with
`cargo test -p bcode_tui --lib transcript_scenarios::v1_navigation`.

Notes:

* Assistant streaming is driven by canonical `AssistantDelta` events. The production daemon
  streams through live `AssistantTextStreamUpdated` events, which the driver does not deliver
  yet. The C11 deviation rows (`c10_…`, `c11_…`, `c13_…`) may depend on that path, so confirm
  them against the real app before treating them as V1 bugs.
* Submission scenarios type into the composer and press Enter. The driver drops the submit
  effect, so the daemon stays "connecting". Acceptance arrives as a live `UserMessage` event
  with the same text. The C8 deviation was observed under these conditions.
* Focus-edge scenarios read the focused target's rows through the driver's `focused_extent`
  observation, which is relative to the transcript viewport top.

| Scenario | Covers | V1 | V2 | V1 observation |
|---|---|---|---|---|
| `c1_attach_follows_latest_bottom` | C1 | pass | pass | — |
| `c1_wheel_up_reads_and_wheel_down_resumes_following` | C1, T8, T9 | fail-ignored | pass | Wheeling back down to the content bottom leaves V1 anchored. A live item delivered afterwards is not followed, and the latest bar appears. |
| `c2_wheel_down_at_bottom_enters_tail_space` | C2, T10 | pass | pass | — |
| `c2_tail_space_is_bounded_by_the_viewport` | C2 | pass | pass | V1 measures the bound from the content bottom, which includes the trailing separator row, so the newest line sits just above the viewport at the bound. |
| `c3_tail_space_position_is_stable_across_frames` | C3 | pass | pass | — |
| `c4_new_content_fills_tail_space_then_follows` | C4, T11 | fail-ignored | pass | Content fills the tail space without moving and ends at the latest bottom, but a motion is still running and following is not reported after 4 s of scenario time. |
| `c5_reflow_round_trip_does_not_drift` | C5 | pass | pass | — |
| `c6_rows_inserted_above_do_not_consume_tail_space` | C6 | pass | pass | — |
| `c7_wheel_up_inside_tail_space_keeps_tail_space` | C7, T12 | pass | pass | — |
| `c8_staged_submission_does_not_move_view` | C8, N9 | fail-ignored | pass | Pressing Enter while following moves the view to the transcript top ("69 rows from bottom") before acceptance. Root cause not diagnosed. |
| `c8_accepted_submission_reveals_message_at_top` | C8, T2 | fail-ignored | pass | Follows from the staged jump: after acceptance, the view stays at the transcript top with a motion running, and the accepted message is not visible. |
| `c8_cleared_local_command_submission_does_not_anchor` | C8, N9 | pass | pass | — |
| `c9_tool_activity_after_submission_resumes_following` | C9, T4 | fail-ignored | pass | Depends on the accepted-submission reveal (see `c8_accepted_…`). |
| `c10_later_stream_updates_do_not_move_revealed_response` | C10, T3 | fail-ignored | pass | With history above, a new assistant segment follows the bottom instead of being revealed at the top, and later deltas push it up (row 13 to row 10). Same deviation as `c11_streaming_response_reveals_top_and_holds`. |
| `c11_assistant_hold_survives_completion` | C11, T7 | fail-ignored | pass | Same C11 deviation: the response is never held at the top. |
| `c11_non_assistant_item_releases_hold_into_overflow_follow` | C11, T6 | fail-ignored | pass | Same C11 deviation: a fitting tool item moves content up by 2 rows because V1 is following, not holding. |
| `c12_revealed_item_is_not_readmitted` | C12, N10 | pass | pass | Passes on V1, but V1 never reveals the item in the first place (C11 deviation), so this mainly guards V2. |
| `c13_sticky_reveal_hands_off_after_segment_end` | C13, T5, T6 | fail-ignored | pass | Same C11 deviation: an overflowing response follows the bottom instead of holding its top. |
| `c14_manual_scroll_materializes_running_motion` | C14, N6, T8 | pass | pass | — |
| `c14_manual_scroll_cancels_pending_jump` | C14, T22 | fail-ignored | pass | After manual scrolling, the pending non-resident timeline jump result still replaces the window and moves the reader. |
| `c15_grace_suppresses_stream_top_anchor` | C15, N5 | pass | pass | — |
| `c16_stream_does_not_anchor_when_scrolled_up` | C16, T23 | pass | pass | — |
| `c17_latest_bar_motion_lands_on_latest_bottom` | C17, T13 | pass | pass | — |
| `c17_pending_jump_never_falls_back_to_tail` | C17, T18 | pass | pass | — |
| `c22_reduced_motion_jump_is_instant` | C22, N7 | pass | pass | — |
| `c22_enabling_reduced_motion_mid_motion_settles_at_target` | C22, N7 | fail-ignored | pass | Enabling `reduce_motion` through the configuration reload path while a motion runs lets the motion continue; the next frame is still mid-motion. |
| `c23_reader_survives_page_and_live_append_before_frame` | C23, N11, C28 | pass | pass | — |
| `c24_width_change_keeps_source_line_at_top` | C24 | pass | pass (see note) | — |
| `c25_structural_insertion_keeps_reading_position` | C25 | pass | pass | — |
| `c25_collapse_above_keeps_reading_position` | C25 | pass | pass | Collapse is applied through structural rules (`kind = "tool"` collapsed). The `alt+d` cycle would collapse the reading item too. |
| `c26_runtime_work_events_do_not_move_final_response` | C26, T23 | pass | pass | — |
| `c27_repeated_presentation_is_idempotent` | C27, CK1 | pass | pass | — |
| `c27_failed_presentation_does_not_commit_navigation` | C27, CK2, CK3 | fail-ignored | stub-ignored | Stub. The driver cannot inject a presentation failure (`driver: needs presentation failure injection`). |
| `c28_movement_kept_while_page_loads_is_spent_once` | C28, N8 | pass | pass | — |
| `c29_direction_reversal_cancels_pending_movement` | C29, N8 | pass | pass | Checks that the reversal moves by its own tick only, that the older page is still incorporated (first resident sequence decreases), and that neither the page nor later frames spend the cancelled movement. V1 passes because it keeps no movement while a page loads. |
| `c30_growth_while_following_is_not_a_prepend` | C30, N12 | pass | pass | — |
| `c31_window_bottom_is_not_session_tail` | C31, W10 | pass | pass | — |
| `c32_focus_up_puts_target_top_on_top_edge` | C32, T19 | pass | pass | — |
| `c32_focus_down_puts_target_bottom_on_bottom_edge` | C32, T19 | pass | pass | — |
| `c32_focus_on_visible_target_does_not_scroll` | C32, T19 | pass | pass | — |
| `c33_initial_focus_is_last_visible_target` | C33, FO7 | pass | pass | — |
| `c33_initial_focus_falls_back_to_nearest_target_above` | C33, FO7 | pass | pass | V1 scrolls the tall target's top into view when focus is entered; the scenario allows that (T19). |
| `fo4_composer_home_end_do_not_move_transcript` | FO4, X5 | pass | pass | — |
| `fo4_transcript_home_loads_session_start_directly` | FO4, T16, X5 | fail-ignored | pass | V1 transcript focus (`alt+u`) does not load the session start on Home (the FO4 decision is new in V2). |
| `fo4_transcript_end_returns_to_latest` | FO4, T13, X5 | fail-ignored | pass | As above, for End. |
| `fo5_page_up_scrolls_from_composer_focus` | FO5 | pass | pass (see note) | — |

## Interaction scenarios (kept behavior)

Definitions live in `interaction.rs` and are instantiated for V1 in `mod.rs` as
`v1_interaction`. They cover the latest bar, selection and copy, disclosure clicks, keyboard
focus mode, the visibility floor, streaming Markdown and repaint scope. Targets are found by
unique text in the committed frame (tool output tokens such as `ALPHAOUT1`, header text such as
`Tool result`), so no scenario reads hit regions.

These scenarios use five additive driver capabilities from `driver.rs`:

* Press, drag and release inputs: `ScenarioInput::Press`, `DragTo` and `Release`.
* `configure(&ScenarioSetting)`: presentation rules, the `[tui.selection]` copy policy,
  `tui.reduce_motion`, and a scripted clipboard failure.
* `clipboard_writes()`: the text written to the clipboard so far.
* `last_repaint()`: whether the last frame painted the whole surface or only damaged regions.
* `layout_work()`: item layout work since the previous call (spec 9.2 `LayoutWorkCounters`);
  `None` when the engine does not count it.

See "V1 driver and time" below for how the V1 driver keeps wall-clock V1 state on scenario
time.

Status values also include `stub-ignored`: the scenario is ignored with
`#[ignore = "driver: needs <capability>"]` because no driver can express it yet.

| Scenario | Covers | V1 | V2 | V1 observation |
|---|---|---|---|---|
| `c18_latest_bar_ignores_hidden_continuation_of_visible_entry` | C18 (ambiguity 2) | pass | pass | — |
| `c18_latest_bar_shows_for_distinct_entry_fully_below` | C18 | pass | pass | — |
| `c18_live_entry_below_reader_shows_bar` | C18, T23 | pass | pass | — |
| `c18_jump_to_latest_after_live_entry_follows_and_clears_bar` | C18, X11, C17 | fail-ignored | pass | V1 deviation C17. After the jump, the live entry is visible and the bar is gone, but a motion is still running after 2 s and following never resumes. This matches `c17_live_item_during_latest_motion_settles`. |
| `c19_latest_bar_never_causes_its_own_visibility` | C19, F6 | pass | pass | — |
| `c20_scroll_timer_and_reflow_generate_no_activity` | C20 | pass | pass | — |
| `c21_latest_bar_glow_settles_still_after_window` | C21, C20 | fail-ignored | pass | V1 deviation C20/C21, root cause not diagnosed. The glow still shows after the glow window. It reproduces with or without the driver's Markdown projection step, and also when clock advances reach V1 through the production timed-invalidation timer (paint-level, temporal path) instead of a forced structural redraw, so it is not explained by full preparation alone. Confirm in the real app. |
| `c21_reduced_motion_latest_bar_is_still_immediately` | C21, C22 | pass | pass | — |
| `c34_drag_inside_response_stays_within_it` | C34 | fail-ignored | pending-ignored (phase 6: transcript selection (BMUX 13)) | V1 deviation C34. A drag that starts inside a response keeps selecting into the next user item and the next response. The response-local clamp tested at unit level does not apply in the presented transcript. |
| `c34_drag_from_surrounding_space_spans_items` | C34 | pass | pending-ignored (phase 6: transcript selection (BMUX 13)) | — |
| `c34_soft_wrapped_line_selects_without_newlines` | C34, C36 | pass | pending-ignored (phase 6: transcript selection (BMUX 13)) | — |
| `c36_code_selection_preserves_indentation_and_blank_lines` | C36 | pass | pending-ignored (phase 6: transcript selection (BMUX 13)) | — |
| `c34_selection_survives_scroll` | C34 | pass | pending-ignored (phase 6: transcript selection and copy policy) | — |
| `c34_selection_survives_scrolling_out_of_view` | C34, BMUX 13 | fail-ignored | pending-ignored (phase 6: transcript selection and copy policy) | V1 deviation C34. The selection is cleared as soon as its rows scroll out of view, so it is gone when they scroll back. |
| `c34_hard_line_break_copies_as_line_break` | C34 | fail-ignored | pending-ignored (phase 6: transcript selection (BMUX 13)) | V1 deviation C34. Copying across a Markdown hard line break (`\` at the end of a line) joins the two lines with no separator (`…line 0S00004L001…`). |
| `c35_release_copies_and_clears_after_delay` | C35 | pass | pending-ignored (phase 6: transcript selection and copy) | — |
| `c35_failed_copy_keeps_selection` | C35 | pass | pending-ignored (phase 6: transcript selection and copy policy) | — |
| `c35_new_gesture_cancels_pending_clear` | C35 | pass | pending-ignored (phase 6: transcript selection and copy) | — |
| `c35_click_without_drag_does_not_copy` | C35, X1 | pass | pass (see note) | — |
| `x1_click_toggles_tool_header_once_per_click` | X1 | pass | pass (see note) | — |
| `x1_click_toggles_section_label` | X1 | pass | pass (see note) | — |
| `x1_click_toggles_group_header` | X1, X3 | pass | pass (see note) | — |
| `x1_drag_from_header_selects_without_toggling` | X1, C35 | pass | pending-ignored (phase 6: transcript selection (BMUX 13) and gesture routing (BMUX 14)) | — |
| `x2_focus_mode_keys_walk_and_toggle_targets` | X2, FO7 | pass | pending-ignored (phase 6: structure inspector on the V2 path (FO6 `i`); see note) | Covers `alt+u`, `j`, `k`, `h`, `l`, Enter, Space, `d`, `e`, `i` and Escape. `m` is not covered because its user-state write is an effect the driver drops. |
| `x2_focus_walk_scrolls_targets_into_view` | X2, C32 | pass | pass | — |
| `x3_collapsed_group_reveals_failed_member` | X3, A16 | pass | pass | — |
| `a16_hidden_units_leave_marker_and_failed_stays_visible` | A16, X3 | pass | pass | — |
| `a16_floor_applies_after_manual_toggles` | A16 | pass | pass (see note) | — |
| `a16_awaiting_decision_stays_expanded_under_rules` | A16 | pass | pass | — |
| `c38_streaming_markdown_converges_to_final` | C38 | pass | pass | — |
| `d6_wheel_scroll_repaints_regionally` | D6, F5 | fail-ignored | pending-ignored (phase 6: regional damage observation (BMUX 15b)) | V1 bug D-16. A wheel tick marks full damage. |
| `d6_composer_typing_repaints_regionally` | D6, F5 | fail-ignored | pending-ignored (phase 6: regional damage (D6, F5); see note) | V1 bug D-16. A printable key in the composer marks full damage. |
| `d4_scrolling_does_no_layout_work` | D4, 9.2 | stub-ignored | pass | The V1 driver has no layout-work observation (`driver: needs per-frame layout work counters`). V1 drains its layout sync statistics inside `draw_chat_frame`. |
| `d4_prepend_measures_only_new_units` | D4, 9.2 | stub-ignored | pass | As above. |
| `c37_interaction_placement_and_focus_restore` | C37 | stub-ignored | stub-ignored | The driver cannot open plugin interaction surfaces. V1 builds them from tool exchanges only with the bundled-plugin features enabled. |
| `c39_inline_shell_expansion` | C39 | stub-ignored | stub-ignored | The driver cannot produce positioned shell captures. |

## V2 driver

`v2.rs` (`V2Driver`) runs the V2 engine in the application, as phase 4 specifies. It builds the
same root model as the V1 driver (`v1::scenario_model`: the scripted session attached, the
production keymap contributors, no daemon) with `TuiEngine::V2` selected at startup, then:

* Delivers every input as a terminal event through `Program::update`. Keys therefore take the
  application's routing: the keymap, the composer, the transcript focus model (`alt+u`, FO2–FO10)
  and the V2 host seams in the root model. Wheel ticks, clicks, presses, drags and releases are
  BMUX mouse events; resizes resize the terminal and deliver `Event::Resize`.
* Delivers live events as `SessionStream` messages, so the host's ingest path sees them as in
  the application, including `SubmissionAccepted` for durable user messages that accept this
  client's own pending submission (T2, C8, C10).
* Collects effects after every update and commit. `LoadTranscriptV2History` effects become
  pending scenario requests (a new load supersedes the pending load of its lane, as the effect
  queue does). Each scripted response is executed by the host's production history executor
  (`transcript_v2_host::effects::load_history`: bounded reads, the `Around` split, page
  validation, session bounds) against a daemon double holding exactly that response, and
  delivered as a `TranscriptV2HistoryLoaded` effect result through `update`. The attach `Tail` window is
  answered with the setup's resident seed. Every other effect, including V1 history loads for
  the hidden host-shell transcript, is dropped. Pages ask for the per-request caps (16 items,
  512 events, W6), `Around` asks for 256 events on each side (W8), and `Head`/`Tail` ask for
  512 events (W10). Requests the engine no longer awaits are not reported.
* Keeps the host clock on scenario time (`TranscriptV2Host::set_scripted_now`).
  `advance_clock` advances it and delivers the production V2 timer (`bcode.transcript_v2`)
  through `update`. Timeline jumps go through the root model's timeline-jump path, and
  jump-to-latest is the `ctrl+end` binding the latest bar names.
* Applies configuration (presentation rules, `tui.reduce_motion`, selection policy) to both
  configuration owners, as a configuration reload does; the host picks it up at the end of the
  next update.
* Presents through the production root presenter and commits through
  `Program::presentation_committed`. Observations come from the committed terminal buffer
  (marker tokens per row) and from the regions the host painted (viewport and latest bar).
  Following, motion and focus come from the snapshot that frame painted.
* `selection_text()` is always `None`: transcript selection is phase 6.

`v2_headless.rs` (`V2HeadlessDriver`) keeps the phase 2–3 driver for engine-level checks. It
drives the engine façade without the host, with the standard producers and the bundled default
theme (`terminal-native`). The transcript region is the whole terminal, and only the keys bound
in every focus (FO5) are translated into navigation intents. Inputs that need the host panic.
`v2_headless_engine` in `mod.rs` instantiates the core navigation scenarios with it, except the
two `c17_…_latest_motion_…` scenarios, which start from a latest-bar click that only the host
paints.

## Engine selection at startup

`startup.rs` follows the resolved engine into the root model:

* `both_engines_attach_to_the_same_scripted_session`: both engines attach to one scripted
  session, present its latest marker and hold its tail (phase 4: both engines selectable side
  by side).
* `startup_engine_follows_precedence_and_rejects_invalid_environment` (X7): the engine the root
  model runs follows EN1 precedence (`--tui-engine` > `BCODE_TUI_ENGINE` > `[tui] engine` > V1).
  An invalid environment value is an error that names the variable and the valid values, even
  when the flag wins (EN2).
* `configuration_reload_never_switches_the_engine` (X8): a configuration reload that names the
  other engine is applied, but the engine started with stays in place (EN3).

The resolution rules are unit-tested in `bcode_tui::engine_selection`, and the CLI flag is
tested in `bcode_cli`.

Notes on V2 passes and ignores:

* `c24_width_change_keeps_source_line_at_top`: at the narrow width the Markdown marker lines wrap
  (two rows each), so the reading line is kept through the source-backed anchors the Markdown
  producer's source map provides (P3, C24), not through row offsets.
* `fo5_page_up_scrolls_from_composer_focus` passes through the host keymap: Page Up scrolls the
  transcript while the composer has focus.
* The X1 click scenarios and `a16_floor_applies_after_manual_toggles` pass through the host's
  click routing onto the engine's retained hit regions. Their "a click neither selects nor
  copies" halves, and `c35_click_without_drag_does_not_copy` as a whole, are vacuous until
  phase 6 adds selection and copy.
* `d4_scrolling_does_no_layout_work` and `d4_prepend_measures_only_new_units` are the phase-3
  `LayoutWorkCounters` exit criteria (9.2): scroll-only updates and frames do no parse,
  highlight, projection, build or measurement work, and an older page measures at most its new
  units.
* `x2_focus_mode_keys_walk_and_toggle_targets`: alt+u, the walk keys and the disclosure keys
  pass. The scenario fails at `i`, because the structure inspector is not on the V2 path yet:
  the V1 inspector builds its reports from V1 transcript items, so the V2 host consumes `i`,
  shift+Up/Down and `m` without effect. FO6 lists these keys, so this is the one focus-model
  item left open after phase 4. It is recorded against phase 6, which splits the host-side
  objects on the V2 path.
* `d6_composer_typing_repaints_regionally`: a printable composer key marks full damage in the
  shared host shell's composer path (D-16, the same as on V1), not in V2. Regional damage is a
  phase-6 exit criterion (D6, F5).
* `x2_focus_walk_scrolls_targets_into_view` advances scenario time between key presses, so the
  animated scroll toward each focused target settles before the next press. This is a fix to the
  shared scenario: V1 freezes its scroll animation, so V1 never needed the wait.

## V1 driver and time

V1 reads several deadlines and timestamps from wall time. The V1 driver keeps them on scenario
time so real time spent running a test never changes an outcome:

* Deadlines V1 compares with `Instant::now()` (the 450 ms manual-scroll grace and the post-copy
  selection clear) are observed after every update and presentation. When V1 arms one, the
  driver records its due time in scenario time and pins V1's copy an hour ahead. `advance_clock`
  ends it once scenario time reaches the due time; the selection clear is then delivered as its
  timer through `update`.
* Latest-bar activity timestamps (activity, glow start, next glow frame) are moved forward by the
  wall time spent between driver calls and backward by each `advance_clock` step, so glow phases
  follow scenario time only.
* `advance_clock` then delivers the production timed-invalidation timer through `update`, so time
  reaches V1 through the same path and damage selection as in the application. Only when that
  requests no repaint (state the driver advanced directly, such as the frozen scroll-motion clock,
  may have no due invalidation in wall time) does the driver request a paint-level redraw. It
  never forces a structural preparation.

All V1 deviations recorded above still fail with this time model.

## Adding a scenario

1. Write `pub fn <id>_<behavior><D: TranscriptScenarioDriver>()` in `scenarios.rs`. Start the
   name with the requirement or bug ID it covers.
2. Build the session with `ScenarioSession`: `with_turns`, `push_user`, `push_assistant`,
   `push_assistant_delta` or `push_silent`. Start the engine with `Scenario::attach_latest` or
   `Scenario::start`.
3. Deliver inputs, live events, intents and history results through the runner. Several
   deliveries may happen between frames. Call `present()` to commit a frame and observe it.
4. Assert on frame observations: markers, `shift_from`, `visible_items`, `following_bottom`,
   `latest_bar`, `motion_running` and `focus`. Use `scenario.check` / `scenario.fail` so failures
   print the last two committed frames.
5. Add the name to each engine's `transcript_scenarios!` list in `mod.rs`. Ignore it for an
   engine only after confirming the failure with `--ignored`, and use the reason format
   `"<engine> bug <ID>: <one line>"`.
6. Add a row to the table above.
