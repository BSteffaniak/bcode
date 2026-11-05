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
        assert!(app.transcript().iter().any(|item| {
            item.text().contains("native-shell-visible-marker")
                && item.text().contains("not executed by Bcode")
        }));
        assert!(app.session_view_snapshot().tools.is_empty());
        drop(app);
    }

    #[test]
    fn painting_external_history_is_bounded_and_installs_no_tool_hits() {
        use bcode_session_view_models::external::{
            ExternalActivityView, ExternalProvenanceView, ExternalSessionMetadataView,
        };
        use bmux_tui::{buffer::Buffer, frame::Frame, geometry::Rect};
        let view = ExternalSessionView {
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
