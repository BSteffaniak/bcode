//! Native transcript viewport adapter and history integration tests.

pub use bcode_tui_components::transcript_viewport::TranscriptViewport;

#[cfg(test)]
mod tests {
    use super::super::older_history::OlderHistoryState;

    use super::*;

    fn older_history() -> OlderHistoryState {
        OlderHistoryState::new(&[], false)
    }

    #[test]
    fn zero_distance_animation_preserves_reveal_and_overflow_policy() {
        for sticky in [false, true] {
            let mut viewport = TranscriptViewport::default();
            let mut history = older_history();
            viewport.sync_max(20, 9, 30, 10, false, &mut history);
            viewport.reveal(sticky);
            let top = viewport.top_row(30, 10);
            assert_eq!(viewport.start_follow_anchor_animation(top), None);
            assert!(viewport.allows_reveal());
            assert_eq!(viewport.allows_overflow(), !sticky);
            viewport.sync_with_anchor((30, 9, 40, 10), None, 0, 0, &mut history);
            let allowed = viewport.allows_overflow();
            viewport.reconcile_overflow(30, allowed, &mut history);
            assert_eq!(viewport.follows_bottom(), !sticky);
        }
    }

    #[test]
    fn exhausted_tail_space_reenables_reveal_policy() {
        let mut viewport = TranscriptViewport::default();
        let mut history = older_history();
        viewport.sync_max(20, 9, 30, 10, false, &mut history);
        viewport.scroll_down(4, &mut history);
        assert!(!viewport.allows_reveal());
        viewport.sync_with_anchor((25, 9, 35, 10), None, 10, 0, &mut history);
        assert!(viewport.follows_bottom());
        assert!(viewport.allows_reveal());
    }

    #[test]
    fn manual_input_removes_reveal_policy() {
        let mut viewport = TranscriptViewport::default();
        let mut history = older_history();
        viewport.sync_max(20, 9, 30, 10, false, &mut history);
        viewport.reveal(true);
        viewport.materialize_top_row(12);
        assert!(viewport.allows_reveal());
        assert!(!viewport.allows_overflow());
        viewport.release_sticky_reveal();
        assert!(viewport.allows_overflow());
        viewport.scroll_up(1, &mut history);
        assert!(!viewport.allows_reveal());
        assert!(!viewport.allows_overflow());
    }

    #[test]
    fn correspondence_precedes_tail_exhaustion() {
        let mut viewport = TranscriptViewport::default();
        let mut history = older_history();
        viewport.sync_max(20, 9, 30, 10, false, &mut history);
        viewport.scroll_down(4, &mut history);
        // Ten rows inserted above the reading position do not consume tail space.
        viewport.sync_with_anchor((30, 9, 40, 10), Some(34), 10, 0, &mut history);
        assert!(!viewport.follows_bottom());
        assert_eq!(viewport.bottom_overscroll(), 4);
        assert_eq!(viewport.top_row(40, 10), 34);
    }

    #[test]
    fn tail_space_restore_preserves_intent_and_derives_blank_rows() {
        let mut viewport = TranscriptViewport::default();
        let mut older = older_history();
        viewport.sync_max(20, 9, 30, 10, false, &mut older);
        viewport.scroll_down(4, &mut older);
        viewport.sync_max(21, 9, 31, 10, false, &mut older);
        viewport.restore_anchor(25);
        assert_eq!(viewport.bottom_overscroll(), 4);
        assert!(!viewport.follows_bottom());
        assert_eq!(viewport.top_row(31, 10), 25);
    }

    #[test]
    fn tail_space_does_not_ratchet_when_existing_geometry_oscillates() {
        let mut viewport = TranscriptViewport::default();
        let mut older = older_history();
        viewport.sync_max(20, 9, 30, 10, false, &mut older);
        viewport.scroll_down(4, &mut older);
        let top = viewport.top_row(30, 10);
        for _ in 0..20 {
            viewport.sync_max(21, 9, 31, 10, false, &mut older);
            assert_eq!(viewport.top_row(31, 10), top);
            viewport.sync_max(20, 9, 30, 10, false, &mut older);
            assert_eq!(viewport.top_row(30, 10), top);
            assert_eq!(viewport.bottom_overscroll(), 4);
        }
    }

    #[test]
    fn tail_space_exhaustion_is_independent_of_manual_scroll_timing() {
        for manual_scroll_active in [false, true] {
            let mut viewport = TranscriptViewport::default();
            let mut older = older_history();
            viewport.sync_max(20, 9, 30, 10, false, &mut older);
            viewport.scroll_down(4, &mut older);
            assert!(!viewport.follows_bottom());
            viewport.sync_max(22, 9, 32, 10, manual_scroll_active, &mut older);
            assert_eq!(viewport.top_row(32, 10), 24);
            assert_eq!(viewport.bottom_overscroll(), 2);
            viewport.sync_max(25, 9, 35, 10, manual_scroll_active, &mut older);
            assert!(viewport.follows_bottom());
            assert_eq!(viewport.top_row(35, 10), 25);
        }
    }

    #[test]
    fn tail_space_repeated_measurement_is_idempotent() {
        let mut viewport = TranscriptViewport::default();
        let mut older = older_history();
        viewport.sync_max(20, 9, 30, 10, false, &mut older);
        viewport.scroll_down(4, &mut older);
        viewport.sync_max(21, 9, 31, 10, false, &mut older);
        let resolved = viewport;
        viewport.sync_max(21, 9, 31, 10, false, &mut older);
        assert_eq!(viewport, resolved);
    }

    #[test]
    fn anchored_history_top_row_does_not_move_when_rows_append() {
        let mut viewport = TranscriptViewport::default();
        let mut older = older_history();
        viewport.sync_max(20, 0, 30, 10, false, &mut older);
        viewport.scroll_up(8, &mut older);
        let top_row = viewport.top_row(30, 10);

        viewport.sync_max(25, 0, 35, 10, false, &mut older);

        assert_eq!(viewport.top_row(35, 10), top_row);
    }

    #[test]
    fn anchored_history_top_row_survives_async_row_height_growth() {
        let mut viewport = TranscriptViewport::default();
        let mut older = older_history();
        viewport.sync_max(30, 0, 40, 10, false, &mut older);
        viewport.scroll_up(12, &mut older);
        let top_row = viewport.top_row(40, 10);

        viewport.sync_max(37, 0, 47, 10, false, &mut older);

        assert_eq!(viewport.top_row(47, 10), top_row);
        assert_eq!(viewport.offset(), 47 - top_row - 10);
    }

    #[test]
    fn following_bottom_tracks_async_row_height_growth() {
        let mut viewport = TranscriptViewport::default();
        let mut older = older_history();
        viewport.sync_max(30, 0, 40, 10, false, &mut older);

        viewport.sync_max(37, 0, 47, 10, false, &mut older);

        assert_eq!(viewport.top_row(47, 10), 37);
        assert!(viewport.at_bottom_threshold());
    }

    #[test]
    fn following_bottom_tracks_appended_rows() {
        let mut viewport = TranscriptViewport::default();
        let mut older = older_history();
        viewport.sync_max(20, 0, 30, 10, false, &mut older);
        assert_eq!(viewport.top_row(30, 10), 20);

        viewport.sync_max(25, 0, 35, 10, false, &mut older);

        assert_eq!(viewport.top_row(35, 10), 25);
        assert_eq!(viewport.offset(), 0);
        assert!(viewport.at_bottom_threshold());
    }

    #[test]
    fn anchored_history_top_row_does_not_move_when_viewport_shrinks() {
        let mut viewport = TranscriptViewport::default();
        let mut older = older_history();
        viewport.sync_max(20, 0, 30, 10, false, &mut older);
        viewport.scroll_up(8, &mut older);
        let top_row = viewport.top_row(30, 10);

        viewport.sync_max(21, 0, 30, 9, false, &mut older);

        assert_eq!(viewport.top_row(30, 9), top_row);
    }

    #[test]
    fn successive_older_pages_reveal_only_outstanding_boundary_movement() {
        let mut viewport = TranscriptViewport::default();
        let mut history = older_history();
        history.mark_dropped_history_before(30);
        viewport.sync_max(20, 0, 30, 10, false, &mut history);
        viewport.follow_anchor(0);
        viewport.scroll_up(9, &mut history);
        viewport.scroll_up(4, &mut history);
        assert_eq!(history.reveal_request(), Some(13));
        history.set_loading(true);
        let session_id = bcode_session_models::SessionId::new();
        let event = |sequence| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: "older".to_owned(),
            },
        };
        history.update_cursor(&[event(20)], true);
        history.set_loading(false);
        viewport.sync_with_anchor((25, 0, 35, 10), Some(5), 10, 0, &mut history);
        assert_eq!(viewport.top_row(35, 10), 0);
        assert_eq!(history.reveal_request(), Some(8));
        // Re-preparing the same accepted page cannot spend the movement twice.
        viewport.sync_with_anchor((25, 0, 35, 10), Some(5), 10, 0, &mut history);
        assert_eq!(history.reveal_request(), Some(8));
        assert!(history.should_load());
        history.set_loading(true);
        history.update_cursor(&[], false);
        history.set_loading(false);
        viewport.sync_with_anchor((38, 0, 48, 10), Some(18), 10, 0, &mut history);
        assert_eq!(viewport.top_row(48, 10), 10);
        assert_eq!(history.reveal_request(), None);
        viewport.sync_with_anchor((38, 0, 48, 10), Some(10), 10, 0, &mut history);
        assert_eq!(viewport.top_row(48, 10), 10);
    }

    #[test]
    fn scrolling_to_centered_newer_boundary_does_not_request_an_extra_row() {
        let mut viewport = TranscriptViewport::default();
        let mut history = older_history();
        let session_id = bcode_session_models::SessionId::new();
        let event = bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 10,
            timestamp_ms: 10,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: "resident".to_owned(),
            },
        };
        history.replace_centered(std::slice::from_ref(&event), false, true, 10);
        viewport.sync_max(20, 0, 30, 10, false, &mut history);
        viewport.follow_anchor(15);
        assert!(viewport.scroll_down(5, &mut history));
        assert_eq!(viewport.top_row(30, 10), 20);
        assert_eq!(history.newer_reveal_request(), None);
        assert!(!history.should_load_newer());
        assert!(viewport.scroll_down(1, &mut history));
        assert_eq!(history.newer_reveal_request(), Some(1));
        assert!(history.should_load_newer());
    }

    #[test]
    fn exhausted_newer_page_does_not_spend_pending_movement_on_unrelated_growth() {
        let mut viewport = TranscriptViewport::default();
        let mut history = older_history();
        let session_id = bcode_session_models::SessionId::new();
        let event = bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 10,
            timestamp_ms: 10,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: "resident".to_owned(),
            },
        };
        history.replace_centered(std::slice::from_ref(&event), false, true, 10);
        viewport.sync_max(20, 0, 30, 10, false, &mut history);
        viewport.follow_anchor(15);
        viewport.scroll_down(8, &mut history);
        assert_eq!(history.newer_reveal_request(), Some(3));
        history.set_loading_newer(true);
        history.update_newer_cursor(&[], false);
        history.set_loading_newer(false);
        // Existing content reflows below the reader at the same time as
        // exhaustion. No page rows were added; only correspondence is restored.
        viewport.sync_with_anchor((30, 0, 40, 10), Some(20), 10, 0, &mut history);
        assert_eq!(viewport.top_row(40, 10), 20);
        assert_eq!(history.newer_reveal_request(), None);
        viewport.sync_with_anchor((30, 0, 40, 10), Some(20), 10, 0, &mut history);
        assert_eq!(viewport.top_row(40, 10), 20);
    }

    #[test]
    fn terminal_newer_page_can_reveal_its_own_rows_once() {
        let mut viewport = TranscriptViewport::default();
        let mut history = older_history();
        let session_id = bcode_session_models::SessionId::new();
        let event = bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 10,
            timestamp_ms: 10,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: "resident".to_owned(),
            },
        };
        history.replace_centered(std::slice::from_ref(&event), false, true, 10);
        viewport.sync_max(20, 0, 30, 10, false, &mut history);
        viewport.follow_anchor(15);
        viewport.scroll_down(8, &mut history);
        assert_eq!(history.newer_reveal_request(), Some(3));
        history.set_loading_newer(true);
        history.update_newer_cursor(
            &[bcode_session_models::SessionEvent {
                sequence: 11,
                ..event
            }],
            false,
        );
        history.set_loading_newer(false);
        viewport.sync_with_anchor((30, 0, 40, 10), Some(20), 0, 10, &mut history);
        assert_eq!(viewport.top_row(40, 10), 23);
        assert_eq!(history.newer_reveal_request(), None);
        viewport.sync_with_anchor((30, 0, 40, 10), Some(23), 10, 0, &mut history);
        assert_eq!(viewport.top_row(40, 10), 23);
    }

    #[test]
    fn newer_page_only_spends_measured_page_rows_during_concurrent_reflow() {
        let mut viewport = TranscriptViewport::default();
        let mut history = older_history();
        let session_id = bcode_session_models::SessionId::new();
        let event = bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 10,
            timestamp_ms: 10,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: "resident".to_owned(),
            },
        };
        history.replace_centered(std::slice::from_ref(&event), false, true, 10);
        viewport.sync_max(20, 0, 30, 10, false, &mut history);
        viewport.follow_anchor(15);
        viewport.scroll_down(8, &mut history);
        assert_eq!(history.newer_reveal_request(), Some(3));
        history.set_loading_newer(true);
        history.update_newer_cursor(
            &[bcode_session_models::SessionEvent {
                sequence: 11,
                ..event
            }],
            true,
        );
        history.set_loading_newer(false);
        // Most of the new geometry came from a width change to existing rows.
        // Only two of those rows belong to the accepted source page.
        viewport.sync_with_anchor((35, 0, 45, 10), Some(25), 0, 2, &mut history);
        assert_eq!(viewport.top_row(45, 10), 27);
        assert_eq!(history.newer_reveal_request(), Some(1));
        viewport.sync_with_anchor((35, 0, 45, 10), Some(27), 0, 2, &mut history);
        assert_eq!(viewport.top_row(45, 10), 27);
        assert_eq!(history.newer_reveal_request(), Some(1));
    }

    #[test]
    fn newer_page_without_new_visible_rows_keeps_requested_movement_pending() {
        let mut viewport = TranscriptViewport::default();
        let mut history = older_history();
        let session_id = bcode_session_models::SessionId::new();
        let event = bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 10,
            timestamp_ms: 10,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: "resident".to_owned(),
            },
        };
        history.replace_centered(std::slice::from_ref(&event), false, true, 10);
        viewport.sync_max(20, 0, 30, 10, false, &mut history);
        viewport.follow_anchor(15);
        viewport.scroll_down(6, &mut history);
        assert_eq!(history.newer_reveal_request(), Some(1));
        history.set_loading_newer(true);
        history.update_newer_cursor(
            &[bcode_session_models::SessionEvent {
                sequence: 11,
                ..event
            }],
            true,
        );
        history.set_loading_newer(false);
        // The accepted page contains only non-presenting events. Existing rows
        // below the reader must not satisfy movement requested at the boundary.
        viewport.sync_with_anchor((20, 0, 30, 10), Some(20), 10, 0, &mut history);
        assert_eq!(viewport.top_row(30, 10), 20);
        assert_eq!(history.newer_reveal_request(), Some(1));
        viewport.sync_with_anchor((20, 0, 30, 10), Some(20), 10, 0, &mut history);
        assert_eq!(viewport.top_row(30, 10), 20);
        assert_eq!(history.newer_reveal_request(), Some(1));
    }

    #[test]
    fn older_page_without_new_visible_rows_does_not_reveal_existing_content() {
        let mut viewport = TranscriptViewport::default();
        let mut history = older_history();
        history.mark_dropped_history_before(10);
        viewport.sync_max(20, 0, 30, 10, false, &mut history);
        viewport.follow_anchor(0);
        viewport.scroll_up(3, &mut history);
        history.set_loading(true);
        // A page of non-presenting events replaces the loading banner, but contributes
        // no navigable rows above the reader.
        history.update_cursor(&[], true);
        history.mark_dropped_history_before(9);
        history.set_loading(false);
        viewport.sync_with_anchor((20, 0, 30, 10), Some(0), 10, 0, &mut history);
        assert_eq!(viewport.top_row(30, 10), 0);
        assert_eq!(history.reveal_request(), Some(3));
    }

    #[test]
    fn older_page_does_not_spend_reflow_above_reader_as_page_rows() {
        let mut viewport = TranscriptViewport::default();
        let mut history = older_history();
        history.mark_dropped_history_before(10);
        viewport.sync_max(20, 0, 30, 10, false, &mut history);
        viewport.follow_anchor(5);
        viewport.scroll_up(9, &mut history);
        assert_eq!(history.reveal_request(), Some(4));
        history.update_cursor(&[], true);
        history.mark_dropped_history_before(9);
        // Existing rows above the anchor gained eight rows during reflow, but
        // the accepted page introduced only two source-backed rows.
        viewport.sync_with_anchor((30, 0, 40, 10), Some(10), 2, 0, &mut history);
        assert_eq!(viewport.top_row(40, 10), 8);
        assert_eq!(history.reveal_request(), Some(2));
        viewport.sync_with_anchor((30, 0, 40, 10), Some(8), 0, 0, &mut history);
        assert_eq!(history.reveal_request(), Some(2));
    }

    #[test]
    fn older_page_does_not_count_rows_added_below_reader() {
        let mut viewport = TranscriptViewport::default();
        let mut history = older_history();
        history.mark_dropped_history_before(10);
        viewport.sync_max(20, 0, 30, 10, false, &mut history);
        viewport.follow_anchor(0);
        viewport.scroll_up(3, &mut history);
        history.set_loading(true);
        history.update_cursor(&[], true);
        history.mark_dropped_history_before(9);
        history.set_loading(false);
        // Only two rows were added above the preserved anchor; the other 18
        // were caused by content below it changing before the frame was accepted.
        viewport.sync_with_anchor((40, 0, 50, 10), Some(2), 10, 0, &mut history);
        assert_eq!(viewport.top_row(50, 10), 0);
        assert_eq!(history.reveal_request(), Some(1));
    }

    #[test]
    fn missing_item_anchor_does_not_treat_growth_below_as_prepend() {
        let mut viewport = TranscriptViewport::default();
        let mut history = older_history();
        history.mark_dropped_history_before(10);
        viewport.sync_max(20, 0, 30, 10, false, &mut history);
        viewport.follow_anchor(0); // The loading banner has no item anchor.
        viewport.scroll_up(3, &mut history);
        history.set_loading(true);
        history.update_cursor(&[], true);
        history.mark_dropped_history_before(9);
        history.set_loading(false);
        // Two rows above the old viewport, eighteen rows below it.
        viewport.sync_with_anchor((40, 0, 50, 10), None, 10, 0, &mut history);
        assert_eq!(viewport.top_row(50, 10), 0);
        // Without a surviving item anchor we cannot prove how much was prepended,
        // so keep the movement pending instead of guessing from total growth.
        assert_eq!(history.reveal_request(), Some(3));
    }

    #[test]
    fn boundary_scroll_applies_available_rows_and_retains_loading_movement() {
        let mut viewport = TranscriptViewport::default();
        let mut history = older_history();
        history.mark_dropped_history_before(10);
        viewport.sync_max(20, 9, 30, 10, false, &mut history);
        viewport.follow_anchor(2);
        assert!(viewport.scroll_up(3, &mut history));
        assert_eq!(viewport.top_row(30, 10), 0);
        assert_eq!(history.reveal_request(), Some(1));
        history.set_loading(true);
        viewport.scroll_up(2, &mut history);
        viewport.sync_with_anchor((20, 9, 30, 10), Some(0), 10, 0, &mut history);
        assert_eq!(history.reveal_request(), Some(3));
        history.update_cursor(&[], false);
        history.set_loading(false);
        viewport.sync_with_anchor((30, 9, 40, 10), Some(10), 10, 0, &mut history);
        assert_eq!(viewport.top_row(40, 10), 7);
        assert_eq!(history.reveal_request(), None);
    }

    #[test]
    fn direction_reversal_cancels_pending_history_movement() {
        let mut viewport = TranscriptViewport::default();
        let mut history = older_history();
        history.mark_dropped_history_before(10);
        viewport.sync_max(20, 9, 30, 10, false, &mut history);
        viewport.follow_anchor(0);
        viewport.scroll_up(3, &mut history);
        viewport.scroll_down(2, &mut history);
        history.update_cursor(&[], false);
        viewport.sync_with_anchor((30, 9, 40, 10), Some(12), 10, 0, &mut history);
        assert_eq!(viewport.top_row(40, 10), 12);
    }

    #[test]
    fn older_history_reveal_keeps_same_content_visible_after_prepend() {
        let mut viewport = TranscriptViewport::default();
        let mut older = older_history();
        viewport.sync_max(20, 0, 30, 10, false, &mut older);
        viewport.scroll_up(8, &mut older);
        // Unsolicited prepend preserves the reader; it is not a request to move.
        older.update_cursor(&[], false);
        viewport.sync_with_anchor((24, 0, 34, 10), Some(16), 10, 0, &mut older);

        assert_eq!(viewport.top_row(34, 10), 16);
    }
}
