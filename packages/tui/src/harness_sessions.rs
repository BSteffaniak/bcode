//! Terminal adaptation of shared managed-session semantics. BMUX owns all cell geometry.

use bcode_session_view::external::external_fallback_blocks;
use bcode_session_view_models::external::ExternalSessionView;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::text::Line;

/// Paint a bounded literal fallback.
///
/// This intentionally clips rather than independently wrapping:
/// transcript selection/scroll integration can use the same stable fallback block identities.
/// No tool execution, implicit response, auth input, or invisible hit region is registered here.
/// Returns false for an invalid/oversized page and leaves the paint context unchanged.
pub fn paint_external_session(
    cx: &mut PaintCx<'_, '_>,
    area: LocalRect,
    view: &ExternalSessionView,
    first_line: usize,
) -> bool {
    let Ok(blocks) = external_fallback_blocks(view) else {
        return false;
    };
    if area.width == 0 || area.height == 0 {
        return true;
    }
    let lines = blocks.iter().flat_map(|block| block.text.lines());
    for (row, text) in lines
        .skip(first_line)
        .take(usize::from(area.height))
        .enumerate()
    {
        let row = i64::try_from(row).unwrap_or(i64::MAX);
        let line = Line::raw(text);
        cx.write_line(
            LocalRect::new(area.x, area.y.saturating_add(row), area.width, 1),
            &line,
        );
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{ExternalSessionView, LocalRect, PaintCx, paint_external_session};
    use bmux_tui::text::Line;

    #[test]
    fn canonical_native_tool_is_visible_through_the_normal_terminal_adapter() {
        use crate::app::BmuxApp;
        use bcode_harness_models::{
            HarnessCompleteness, HarnessObservation, HarnessObservationKind,
        };
        use bcode_session_models::external_backend::{
            ExternalBackendEnvelope, ExternalBackendRecord,
        };
        use bcode_session_models::{CURRENT_SESSION_EVENT_SCHEMA_VERSION, SessionEvent, SessionId};
        let session_id = SessionId::new();
        let event = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            session_id,
            sequence: 1,
            timestamp_ms: 0,
            provenance: None,
            kind: ExternalBackendEnvelope {
                version: 1,
                record: ExternalBackendRecord::Observation {
                    observation: HarnessObservation {
                        observation_id: "native-tool-observed".into(),
                        session_id: session_id.to_string(),
                        generation: 3,
                        native_correlations: vec![],
                        completeness: HarnessCompleteness::Partial,
                        kind: HarnessObservationKind::Tool {
                            input_json: None,
                            native_tool_id: "native-tool".into(),
                            name: "native-shell-visible-marker".into(),
                            status: "running".into(),
                        },
                    },
                },
            }
            .into_event_kind()
            .unwrap(),
        };
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        app.absorb_session_event(&event);
        assert_eq!(app.transcript().len(), 1);
        // An observed native tool is an ordinary generic tool card owned by the observing
        // harness, never a Bcode tool invocation.
        assert!(app.transcript().iter().any(|item| matches!(
            item.kind(),
            crate::transcript::TranscriptItemKind::ToolRequest {
                tool_name,
                tool_call_id,
                ..
            } if tool_name == "native-shell-visible-marker"
                && tool_call_id.starts_with("external:3:")
        )));
        assert!(app.session_view_snapshot().tools.is_empty());
        drop(app);
    }

    #[test]
    fn terminal_reply_keeps_background_and_late_continuation_visible_without_controls() {
        use bcode_harness_models::{
            HarnessAttemptIdentity, HarnessCompleteness, HarnessExecutionEvent,
            HarnessNotificationState, HarnessObservation, HarnessObservationKind,
            HarnessTerminalDisposition, HarnessWorkKind, HarnessWorkState,
        };
        use bcode_session_models::{
            CURRENT_SESSION_EVENT_SCHEMA_VERSION, SessionEvent, SessionEventKind, SessionId,
        };
        let session_id = SessionId::new();
        let origin = HarnessAttemptIdentity {
            submission_id: "submit".into(),
            attempt_id: "attempt".into(),
        };
        let mut app = crate::app::BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let terminal = HarnessExecutionEvent::SubmissionTerminal {
            attempt: origin.clone(),
            native_turn_id: None,
            native_frame_id: Some("result".into()),
            disposition: HarnessTerminalDisposition::Completed,
            output: None,
        };
        let work = |id: &str| HarnessExecutionEvent::BackgroundWork {
            work_id: id.into(),
            native_task_id: None,
            origin: Some(origin.clone()),
            parent_work_id: None,
            work_kind: HarnessWorkKind::Subagent,
            label: "界 task".into(),
            state: HarnessWorkState::Running,
            notification: HarnessNotificationState::Pending,
        };
        let events = [
            terminal,
            work("one"),
            work("two"),
            HarnessExecutionEvent::Continuation {
                continuation_id: "later".into(),
                origin: Some(origin.clone()),
                source_work_id: Some("one".into()),
                native_turn_id: None,
                state: HarnessWorkState::Pending,
                output: None,
            },
        ];
        for (index, execution) in events.into_iter().enumerate() {
            let sequence = u64::try_from(index).unwrap() + 1;
            app.absorb_session_event(&SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                session_id,
                sequence,
                timestamp_ms: sequence,
                provenance: None,
                kind: SessionEventKind::ExternalObservationRecorded {
                    version: 1,
                    observation: Box::new(HarnessObservation {
                        observation_id: format!("execution-{index}"),
                        session_id: session_id.to_string(),
                        generation: 3,
                        native_correlations: vec![],
                        completeness: HarnessCompleteness::Complete,
                        kind: HarnessObservationKind::Execution { event: execution },
                    }),
                },
            });
        }
        assert!(
            app.session_view_snapshot()
                .execution_lifecycle
                .summary()
                .contains("Reply complete + 2 background")
        );
        // Lifecycle evidence adds no transcript rows; the status line keeps it visible.
        assert!(app.status().contains("1 continuation pending"));
        app.absorb_session_event(&coverage_loss_event(session_id));
        assert!(
            app.session_view_snapshot()
                .execution_lifecycle
                .summary()
                .contains("coverage unknown")
        );
        assert!(
            app.session_view_snapshot()
                .execution_lifecycle
                .summary()
                .contains("Reply complete")
        );
        assert!(app.session_view_snapshot().tools.is_empty());
        assert_eq!(
            app.session_view_snapshot().permissions,
            [] as [bcode_session_view_models::PermissionView; 0]
        );
        assert!(
            app.session_view_snapshot().execution_lifecycle.submissions[0]
                .native_id
                .is_none()
        );
        drop(app);
    }

    fn coverage_loss_event(
        session_id: bcode_session_models::SessionId,
    ) -> bcode_session_models::SessionEvent {
        use bcode_harness_models::{
            HarnessCompleteness, HarnessObservation, HarnessObservationKind,
        };
        use bcode_session_models::{
            CURRENT_SESSION_EVENT_SCHEMA_VERSION, SessionEvent, SessionEventKind,
        };
        SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            session_id,
            sequence: 5,
            timestamp_ms: 5,
            provenance: None,
            kind: SessionEventKind::ExternalObservationRecorded {
                version: 1,
                observation: Box::new(HarnessObservation {
                    observation_id: "gap".into(),
                    session_id: session_id.to_string(),
                    generation: 3,
                    native_correlations: vec![],
                    completeness: HarnessCompleteness::Gap,
                    kind: HarnessObservationKind::Text {
                        segment_id: None,
                        text: "coverage lost".into(),
                    },
                }),
            },
        }
    }

    #[test]
    fn painting_external_history_is_bounded_and_installs_no_tool_hits() {
        use bcode_session_view_models::external::{
            ExternalActivityView, ExternalProvenanceView, ExternalSessionMetadataView,
        };
        use bmux_tui::{buffer::Buffer, frame::Frame, geometry::Rect};
        let view = ExternalSessionView {
            execution_lifecycle:
                bcode_session_view_models::execution::ExecutionLifecycleView::default(),
            provenance: ExternalProvenanceView {
                session_id: "session".into(),
                instance_id: "界".into(),
                plugin_id: "👩‍💻".into(),
                native_session_id: "e\u{301}".into(),
                generation: 2,
            },
            metadata: ExternalSessionMetadataView {
                executable: "native".into(),
                native_version: "1".into(),
                config_namespace: "isolated".into(),
            },
            activity: ExternalActivityView {
                connectivity: "offline".into(),
                execution: "unknown".into(),
                attention: "unknown".into(),
                continuation: "unavailable".into(),
                completeness: "gap".into(),
            },
            transcript: vec![],
            interactions: vec![],
            actions: vec![],
            next_after_sequence: None,
            has_more: false,
            truncated: false,
        };
        for width in [0, 1, 2, 8, 80] {
            let mut buffer = Buffer::empty(Rect::new(0, 0, width, 3));
            let mut frame = Frame::new(&mut buffer);
            assert!(paint_external_session(
                &mut PaintCx::new(&mut frame),
                LocalRect::new(0, 0, width, 3),
                &view,
                0
            ));
            assert_eq!(frame.hits().regions(), []);
        }
    }

    #[test]
    fn native_text_uses_bmux_cell_clipping_including_tiny_widths() {
        for text in ["界界", "👩‍💻", "e\u{301}", "oversized native prefix 界"] {
            for width in [0, 1, 2, 3, 8] {
                let line = Line::raw(text).truncate(width);
                assert!(line.width() <= width);
            }
        }
    }
}
