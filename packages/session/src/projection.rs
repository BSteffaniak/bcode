use bcode_session_models::{
    ProjectionSourceRange, ProjectionWindow, ProjectionWindowAnchor, ProjectionWindowDirection,
    ProjectionWindowRequest, SessionEvent, SessionEventKind, SessionProjectionKind,
    ToolInvocationStreamEvent, TranscriptProjectionItem, TranscriptProjectionItemKind,
};
use std::collections::BTreeMap;

pub(crate) fn projection_window_from_index_entries(
    entries: &[crate::index::TranscriptProjectionIndexEntry],
    first_event_sequence: Option<u64>,
    last_event_sequence: Option<u64>,
    request: &ProjectionWindowRequest,
) -> Option<ProjectionWindow> {
    if request.projection != SessionProjectionKind::Transcript
        || request.anchor != ProjectionWindowAnchor::Latest
        || request.direction != ProjectionWindowDirection::Backward
    {
        return None;
    }

    let items = entries
        .iter()
        .map(|entry| TranscriptProjectionItem {
            kind: entry.kind,
            source_range: entry.source_range,
            estimated_rows: estimate_rows(entry.content_bytes, request.target.width_columns),
            content_bytes: entry.content_bytes,
        })
        .collect::<Vec<_>>();
    let selected_items = select_latest_items(&items, request);
    let source_range = source_range_for_items(&selected_items);
    let scanned_events = source_range.map_or(0, |range| {
        usize::try_from(range.end_sequence - range.start_sequence + 1).unwrap_or(usize::MAX)
    });
    let has_older = source_range.is_some_and(|range| {
        first_event_sequence.is_some_and(|first| first < range.start_sequence)
    });
    let has_newer = source_range
        .is_some_and(|range| last_event_sequence.is_some_and(|last| last > range.end_sequence));

    Some(ProjectionWindow {
        projection: request.projection,
        transcript_items: selected_items,
        source_range,
        has_older,
        has_newer,
        scanned_events,
    })
}

/// Select a bounded projection window from chronological session events.
///
/// Returns `None` for unsupported projections, anchors, or directions.
#[must_use]
pub fn projection_window_from_events(
    events: &[SessionEvent],
    request: &ProjectionWindowRequest,
) -> Option<ProjectionWindow> {
    if request.projection != SessionProjectionKind::Transcript
        || request.anchor != ProjectionWindowAnchor::Latest
        || request.direction != ProjectionWindowDirection::Backward
    {
        return None;
    }

    let scan_start = events
        .len()
        .saturating_sub(request.limits.max_events_scanned);
    let scanned_events = events.len().saturating_sub(scan_start);
    let scanned = &events[scan_start..];
    let items = build_transcript_projection(scanned, request.target.width_columns);
    let selected_items = select_latest_items(&items, request);
    let source_range = source_range_for_items(&selected_items);
    let has_older = source_range.is_some_and(|range| {
        events
            .first()
            .is_some_and(|first| first.sequence < range.start_sequence)
    });
    let has_newer = false;

    Some(ProjectionWindow {
        projection: request.projection,
        transcript_items: selected_items,
        source_range,
        has_older,
        has_newer,
        scanned_events,
    })
}

/// Build first-pass transcript projection metadata from chronological session events.
#[must_use]
pub fn build_transcript_projection(
    events: &[SessionEvent],
    width_columns: Option<u16>,
) -> Vec<TranscriptProjectionItem> {
    let mut builder = TranscriptProjectionBuilder::new(width_columns);
    for event in events {
        builder.apply(event);
    }
    builder.finish()
}

fn select_latest_items(
    items: &[TranscriptProjectionItem],
    request: &ProjectionWindowRequest,
) -> Vec<TranscriptProjectionItem> {
    let mut selected = Vec::new();
    let mut selected_rows = 0usize;
    let mut selected_bytes = 0usize;

    for item in items.iter().rev() {
        if selected.len() >= request.limits.max_items {
            break;
        }
        if selected_bytes.saturating_add(item.content_bytes) > request.limits.max_bytes
            && !selected.is_empty()
        {
            break;
        }

        selected_rows = selected_rows.saturating_add(item.estimated_rows.unwrap_or(1));
        selected_bytes = selected_bytes.saturating_add(item.content_bytes);
        selected.push(item.clone());

        if target_satisfied(
            selected.len(),
            selected_rows,
            selected_bytes,
            request.target.min_items,
            request.target.min_estimated_rows,
            request.target.min_bytes,
        ) {
            break;
        }
    }

    selected.reverse();
    selected
}

fn target_satisfied(
    selected_items: usize,
    selected_rows: usize,
    selected_bytes: usize,
    min_items: Option<usize>,
    min_estimated_rows: Option<usize>,
    min_bytes: Option<usize>,
) -> bool {
    min_items.is_none_or(|minimum| selected_items >= minimum)
        && min_estimated_rows.is_none_or(|minimum| selected_rows >= minimum)
        && min_bytes.is_none_or(|minimum| selected_bytes >= minimum)
}

fn source_range_for_items(items: &[TranscriptProjectionItem]) -> Option<ProjectionSourceRange> {
    let first = items.first()?;
    let last = items.last()?;
    Some(ProjectionSourceRange {
        start_sequence: first.source_range.start_sequence,
        end_sequence: last.source_range.end_sequence,
    })
}

struct TranscriptProjectionBuilder {
    items: Vec<TranscriptProjectionItem>,
    width_columns: Option<u16>,
    assistant_stream: Option<PendingStream>,
    reasoning_stream: Option<PendingStream>,
    tool_invocations: BTreeMap<String, PendingToolInvocation>,
}

impl TranscriptProjectionBuilder {
    const fn new(width_columns: Option<u16>) -> Self {
        Self {
            items: Vec::new(),
            width_columns,
            assistant_stream: None,
            reasoning_stream: None,
            tool_invocations: BTreeMap::new(),
        }
    }

    fn apply(&mut self, event: &SessionEvent) {
        match &event.kind {
            SessionEventKind::AssistantDelta { text } => {
                self.push_stream_delta(TranscriptProjectionItemKind::AssistantMessage, event, text);
            }
            SessionEventKind::AssistantMessage { text } => {
                self.finish_stream(TranscriptProjectionItemKind::AssistantMessage, event, text);
            }
            SessionEventKind::AssistantReasoningDelta { text } => {
                self.push_stream_delta(TranscriptProjectionItemKind::Reasoning, event, text);
            }
            SessionEventKind::AssistantReasoningMessage { text } => {
                self.finish_stream(TranscriptProjectionItemKind::Reasoning, event, text);
            }
            SessionEventKind::ToolCallRequested {
                tool_call_id,
                arguments_json,
                ..
            } => {
                self.flush_streams();
                self.start_tool_invocation(tool_call_id, event.sequence, arguments_json.len());
            }
            SessionEventKind::ToolInvocationStream { event: stream } => {
                self.flush_streams();
                self.apply_tool_stream(event.sequence, stream);
            }
            SessionEventKind::ToolCallFinished {
                tool_call_id,
                result,
                ..
            } => {
                self.flush_streams();
                self.finish_tool_invocation(tool_call_id, event.sequence, result.len());
            }
            _ => {
                self.flush_streams();
                if let Some((kind, bytes)) = non_streaming_item(event) {
                    self.push_item(kind, event.sequence, event.sequence, bytes);
                }
            }
        }
    }

    fn finish(mut self) -> Vec<TranscriptProjectionItem> {
        self.flush_streams();
        self.flush_tool_invocations();
        self.items
    }

    fn push_stream_delta(
        &mut self,
        kind: TranscriptProjectionItemKind,
        event: &SessionEvent,
        text: &str,
    ) {
        let stream = self.pending_stream_mut(kind);
        if let Some(stream) = stream {
            stream.end_sequence = event.sequence;
            stream.content_bytes += text.len();
        } else {
            *self.pending_stream_slot(kind) = Some(PendingStream {
                kind,
                start_sequence: event.sequence,
                end_sequence: event.sequence,
                content_bytes: text.len(),
            });
        }
    }

    fn finish_stream(
        &mut self,
        kind: TranscriptProjectionItemKind,
        event: &SessionEvent,
        text: &str,
    ) {
        let start_sequence = self
            .pending_stream_slot(kind)
            .take()
            .map_or(event.sequence, |stream| stream.start_sequence);
        self.push_item(kind, start_sequence, event.sequence, text.len());
    }

    fn flush_streams(&mut self) {
        let assistant_stream = self.assistant_stream.take();
        let reasoning_stream = self.reasoning_stream.take();
        if let Some(stream) = assistant_stream {
            self.push_stream_item(stream);
        }
        if let Some(stream) = reasoning_stream {
            self.push_stream_item(stream);
        }
    }

    fn push_stream_item(&mut self, stream: PendingStream) {
        self.push_item(
            stream.kind,
            stream.start_sequence,
            stream.end_sequence,
            stream.content_bytes,
        );
    }

    fn start_tool_invocation(&mut self, tool_call_id: &str, sequence: u64, content_bytes: usize) {
        self.tool_invocations.insert(
            tool_call_id.to_owned(),
            PendingToolInvocation {
                start_sequence: sequence,
                end_sequence: sequence,
                content_bytes,
                saw_stream_output: false,
            },
        );
    }

    fn apply_tool_stream(&mut self, sequence: u64, event: &ToolInvocationStreamEvent) {
        let tool_call_id = tool_stream_tool_call_id(event);
        let invocation = self
            .tool_invocations
            .entry(tool_call_id.to_owned())
            .or_insert(PendingToolInvocation {
                start_sequence: sequence,
                end_sequence: sequence,
                content_bytes: 0,
                saw_stream_output: false,
            });
        invocation.end_sequence = sequence;
        invocation.content_bytes = invocation
            .content_bytes
            .saturating_add(tool_stream_content_bytes(event));
        if matches!(event, ToolInvocationStreamEvent::OutputDelta { .. }) {
            invocation.saw_stream_output = true;
        }
    }

    fn finish_tool_invocation(&mut self, tool_call_id: &str, sequence: u64, result_bytes: usize) {
        if let Some(mut invocation) = self.tool_invocations.remove(tool_call_id) {
            invocation.end_sequence = sequence;
            if !invocation.saw_stream_output {
                invocation.content_bytes = invocation.content_bytes.saturating_add(result_bytes);
            }
            self.push_tool_invocation_item(invocation);
            return;
        }
        self.push_item(
            TranscriptProjectionItemKind::ToolInvocation,
            sequence,
            sequence,
            result_bytes,
        );
    }

    fn flush_tool_invocations(&mut self) {
        let invocations = std::mem::take(&mut self.tool_invocations);
        for invocation in invocations.into_values() {
            self.push_tool_invocation_item(invocation);
        }
    }

    fn push_tool_invocation_item(&mut self, invocation: PendingToolInvocation) {
        self.push_item(
            TranscriptProjectionItemKind::ToolInvocation,
            invocation.start_sequence,
            invocation.end_sequence,
            invocation.content_bytes,
        );
    }

    fn push_item(
        &mut self,
        kind: TranscriptProjectionItemKind,
        start_sequence: u64,
        end_sequence: u64,
        content_bytes: usize,
    ) {
        self.items.push(TranscriptProjectionItem {
            kind,
            source_range: ProjectionSourceRange {
                start_sequence,
                end_sequence,
            },
            estimated_rows: estimate_rows(content_bytes, self.width_columns),
            content_bytes,
        });
    }

    fn pending_stream_mut(
        &mut self,
        kind: TranscriptProjectionItemKind,
    ) -> Option<&mut PendingStream> {
        self.pending_stream_slot(kind).as_mut()
    }

    fn pending_stream_slot(
        &mut self,
        kind: TranscriptProjectionItemKind,
    ) -> &mut Option<PendingStream> {
        match kind {
            TranscriptProjectionItemKind::AssistantMessage => &mut self.assistant_stream,
            TranscriptProjectionItemKind::Reasoning => &mut self.reasoning_stream,
            _ => unreachable!("only streaming transcript item kinds have pending slots"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PendingStream {
    kind: TranscriptProjectionItemKind,
    start_sequence: u64,
    end_sequence: u64,
    content_bytes: usize,
}

#[derive(Debug, Clone, Copy)]
struct PendingToolInvocation {
    start_sequence: u64,
    end_sequence: u64,
    content_bytes: usize,
    saw_stream_output: bool,
}

fn non_streaming_item(event: &SessionEvent) -> Option<(TranscriptProjectionItemKind, usize)> {
    match &event.kind {
        SessionEventKind::UserMessage { text, .. } | SessionEventKind::SystemMessage { text } => {
            Some((TranscriptProjectionItemKind::UserMessage, text.len()))
        }
        SessionEventKind::PermissionRequested { arguments_json, .. } => Some((
            TranscriptProjectionItemKind::Permission,
            arguments_json.len(),
        )),
        SessionEventKind::PermissionResolved { .. } => {
            Some((TranscriptProjectionItemKind::Permission, 0))
        }
        SessionEventKind::ContextCompacted { summary, .. } => Some((
            TranscriptProjectionItemKind::ContextCompaction,
            summary.len(),
        )),
        SessionEventKind::WorkingDirectoryChanged {
            old_working_directory,
            new_working_directory,
        } => Some((
            TranscriptProjectionItemKind::WorkingDirectoryChange,
            old_working_directory.as_os_str().len() + new_working_directory.as_os_str().len(),
        )),
        SessionEventKind::SkillInvoked { arguments, .. } => {
            Some((TranscriptProjectionItemKind::Other, arguments.len()))
        }
        SessionEventKind::SkillInvocationFailed { error, .. } => {
            Some((TranscriptProjectionItemKind::Other, error.len()))
        }
        SessionEventKind::ModelUsage { .. } => Some((TranscriptProjectionItemKind::Other, 0)),
        _ => None,
    }
}

fn tool_stream_tool_call_id(event: &ToolInvocationStreamEvent) -> &str {
    match event {
        ToolInvocationStreamEvent::Started { tool_call_id, .. }
        | ToolInvocationStreamEvent::OutputDelta { tool_call_id, .. }
        | ToolInvocationStreamEvent::Status { tool_call_id, .. }
        | ToolInvocationStreamEvent::Finished { tool_call_id, .. } => tool_call_id,
    }
}

const fn tool_stream_content_bytes(event: &ToolInvocationStreamEvent) -> usize {
    match event {
        ToolInvocationStreamEvent::Started { tool_name, .. } => tool_name.len(),
        ToolInvocationStreamEvent::OutputDelta { text, .. }
        | ToolInvocationStreamEvent::Status { message: text, .. } => text.len(),
        ToolInvocationStreamEvent::Finished { .. } => 0,
    }
}

fn estimate_rows(content_bytes: usize, width_columns: Option<u16>) -> Option<usize> {
    let width = usize::from(width_columns?);
    if width == 0 {
        return Some(1);
    }
    Some((content_bytes / width).saturating_add(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::{
        CURRENT_SESSION_EVENT_SCHEMA_VERSION, ClientId, ProjectionWindowLimits,
        ProjectionWindowRequest, ProjectionWindowTarget, SessionId, ToolInvocationStreamEvent,
        ToolOutputStream,
    };

    #[test]
    fn latest_projection_window_from_index_entries_satisfies_targets() {
        let entries = vec![
            index_entry(1, TranscriptProjectionItemKind::UserMessage, 5),
            index_entry(2, TranscriptProjectionItemKind::AssistantMessage, 5),
            index_entry(3, TranscriptProjectionItemKind::UserMessage, 5),
            index_entry(4, TranscriptProjectionItemKind::AssistantMessage, 5),
        ];

        let window = projection_window_from_index_entries(
            &entries,
            Some(1),
            Some(4),
            &ProjectionWindowRequest {
                projection: SessionProjectionKind::Transcript,
                anchor: ProjectionWindowAnchor::Latest,
                direction: ProjectionWindowDirection::Backward,
                target: ProjectionWindowTarget {
                    min_items: Some(3),
                    min_estimated_rows: None,
                    min_bytes: None,
                    width_columns: Some(80),
                },
                limits: ProjectionWindowLimits {
                    max_items: 8,
                    max_events_scanned: 64,
                    max_bytes: 4096,
                },
            },
        )
        .expect("index-backed latest transcript windows are supported");

        assert_eq!(window.transcript_items.len(), 3);
        assert_eq!(window.source_range.expect("source range").start_sequence, 2);
        assert!(window.has_older);
        assert!(!window.has_newer);
    }

    #[test]
    fn latest_projection_window_from_index_entries_estimates_rows_from_request_width() {
        let entries = vec![index_entry(
            1,
            TranscriptProjectionItemKind::AssistantMessage,
            100,
        )];

        let narrow = projection_window_from_index_entries(
            &entries,
            Some(1),
            Some(1),
            &ProjectionWindowRequest {
                projection: SessionProjectionKind::Transcript,
                anchor: ProjectionWindowAnchor::Latest,
                direction: ProjectionWindowDirection::Backward,
                target: ProjectionWindowTarget {
                    min_items: Some(1),
                    min_estimated_rows: None,
                    min_bytes: None,
                    width_columns: Some(20),
                },
                limits: ProjectionWindowLimits {
                    max_items: 8,
                    max_events_scanned: 64,
                    max_bytes: 4096,
                },
            },
        )
        .expect("index-backed latest transcript windows are supported");
        let wide = projection_window_from_index_entries(
            &entries,
            Some(1),
            Some(1),
            &ProjectionWindowRequest {
                projection: SessionProjectionKind::Transcript,
                anchor: ProjectionWindowAnchor::Latest,
                direction: ProjectionWindowDirection::Backward,
                target: ProjectionWindowTarget {
                    min_items: Some(1),
                    min_estimated_rows: None,
                    min_bytes: None,
                    width_columns: Some(100),
                },
                limits: ProjectionWindowLimits {
                    max_items: 8,
                    max_events_scanned: 64,
                    max_bytes: 4096,
                },
            },
        )
        .expect("index-backed latest transcript windows are supported");

        assert_eq!(narrow.transcript_items[0].estimated_rows, Some(6));
        assert_eq!(wide.transcript_items[0].estimated_rows, Some(2));
    }

    #[test]
    fn latest_projection_window_from_index_entries_respects_byte_cap() {
        let entries = vec![
            index_entry(1, TranscriptProjectionItemKind::UserMessage, 5),
            index_entry(2, TranscriptProjectionItemKind::AssistantMessage, 5),
        ];

        let window = projection_window_from_index_entries(
            &entries,
            Some(1),
            Some(2),
            &ProjectionWindowRequest {
                projection: SessionProjectionKind::Transcript,
                anchor: ProjectionWindowAnchor::Latest,
                direction: ProjectionWindowDirection::Backward,
                target: ProjectionWindowTarget {
                    min_items: Some(2),
                    min_estimated_rows: None,
                    min_bytes: None,
                    width_columns: Some(80),
                },
                limits: ProjectionWindowLimits {
                    max_items: 8,
                    max_events_scanned: 64,
                    max_bytes: 5,
                },
            },
        )
        .expect("index-backed latest transcript windows are supported");

        assert_eq!(window.transcript_items.len(), 1);
        assert_eq!(window.source_range.expect("source range").start_sequence, 2);
    }

    #[test]
    fn assistant_deltas_finish_as_one_projection_item() {
        let session_id = SessionId::new();
        let events = vec![
            event(
                session_id,
                1,
                SessionEventKind::AssistantDelta {
                    text: "hel".to_owned(),
                },
            ),
            event(
                session_id,
                2,
                SessionEventKind::AssistantDelta {
                    text: "lo".to_owned(),
                },
            ),
            event(
                session_id,
                3,
                SessionEventKind::AssistantMessage {
                    text: "hello".to_owned(),
                },
            ),
        ];

        let items = build_transcript_projection(&events, Some(80));

        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].kind,
            TranscriptProjectionItemKind::AssistantMessage
        );
        assert_eq!(items[0].source_range.start_sequence, 1);
        assert_eq!(items[0].source_range.end_sequence, 3);
        assert_eq!(items[0].content_bytes, 5);
    }

    #[test]
    fn unterminated_assistant_deltas_are_retained() {
        let session_id = SessionId::new();
        let events = vec![
            event(
                session_id,
                1,
                SessionEventKind::AssistantDelta {
                    text: "hel".to_owned(),
                },
            ),
            event(
                session_id,
                2,
                SessionEventKind::AssistantDelta {
                    text: "lo".to_owned(),
                },
            ),
        ];

        let items = build_transcript_projection(&events, Some(80));

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].source_range.start_sequence, 1);
        assert_eq!(items[0].source_range.end_sequence, 2);
        assert_eq!(items[0].content_bytes, 5);
    }

    #[test]
    fn streamed_tool_invocation_groups_source_range_and_avoids_final_double_count() {
        let session_id = SessionId::new();
        let events = vec![
            event(
                session_id,
                1,
                SessionEventKind::ToolCallRequested {
                    tool_call_id: "tool".to_owned(),
                    tool_name: "shell".to_owned(),
                    arguments_json: "{}".to_owned(),
                },
            ),
            event(
                session_id,
                2,
                SessionEventKind::ToolInvocationStream {
                    event: ToolInvocationStreamEvent::Started {
                        tool_call_id: "tool".to_owned(),
                        tool_name: "shell".to_owned(),
                        terminal: false,
                        columns: None,
                        rows: None,
                        started_at_ms: None,
                    },
                },
            ),
            event(
                session_id,
                3,
                SessionEventKind::ToolInvocationStream {
                    event: ToolInvocationStreamEvent::OutputDelta {
                        tool_call_id: "tool".to_owned(),
                        stream: ToolOutputStream::Stdout,
                        sequence: 1,
                        text: "output".to_owned(),
                        byte_len: 6,
                    },
                },
            ),
            event(
                session_id,
                4,
                SessionEventKind::ToolCallFinished {
                    tool_call_id: "tool".to_owned(),
                    result: "final result".to_owned(),
                    is_error: false,
                    output: None,
                },
            ),
        ];

        let items = build_transcript_projection(&events, Some(80));

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, TranscriptProjectionItemKind::ToolInvocation);
        assert_eq!(items[0].source_range.start_sequence, 1);
        assert_eq!(items[0].source_range.end_sequence, 4);
        assert_eq!(items[0].content_bytes, 13);
    }

    #[test]
    fn non_streamed_tool_invocation_groups_request_and_result() {
        let session_id = SessionId::new();
        let events = vec![
            event(
                session_id,
                1,
                SessionEventKind::ToolCallRequested {
                    tool_call_id: "tool".to_owned(),
                    tool_name: "read".to_owned(),
                    arguments_json: "{}".to_owned(),
                },
            ),
            event(
                session_id,
                2,
                SessionEventKind::ToolCallFinished {
                    tool_call_id: "tool".to_owned(),
                    result: "result".to_owned(),
                    is_error: false,
                    output: None,
                },
            ),
        ];

        let items = build_transcript_projection(&events, Some(80));

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, TranscriptProjectionItemKind::ToolInvocation);
        assert_eq!(items[0].source_range.start_sequence, 1);
        assert_eq!(items[0].source_range.end_sequence, 2);
        assert_eq!(items[0].content_bytes, 8);
    }

    #[test]
    fn non_streaming_items_preserve_chronological_ranges() {
        let session_id = SessionId::new();
        let events = vec![
            event(
                session_id,
                1,
                SessionEventKind::UserMessage {
                    client_id: ClientId::new(),
                    text: "question".to_owned(),
                },
            ),
            event(
                session_id,
                2,
                SessionEventKind::ToolInvocationStream {
                    event: ToolInvocationStreamEvent::OutputDelta {
                        tool_call_id: "tool".to_owned(),
                        stream: ToolOutputStream::Stdout,
                        sequence: 1,
                        text: "output".to_owned(),
                        byte_len: 6,
                    },
                },
            ),
        ];

        let items = build_transcript_projection(&events, Some(4));

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].kind, TranscriptProjectionItemKind::UserMessage);
        assert_eq!(items[0].source_range.start_sequence, 1);
        assert_eq!(items[1].kind, TranscriptProjectionItemKind::ToolInvocation);
        assert_eq!(items[1].source_range.start_sequence, 2);
        assert_eq!(items[0].estimated_rows, Some(3));
        assert_eq!(items[1].estimated_rows, Some(2));
    }

    #[test]
    fn latest_projection_window_satisfies_item_target_after_compaction() {
        let session_id = SessionId::new();
        let mut events = vec![
            event(
                session_id,
                1,
                SessionEventKind::UserMessage {
                    client_id: ClientId::new(),
                    text: "older".to_owned(),
                },
            ),
            event(
                session_id,
                2,
                SessionEventKind::AssistantMessage {
                    text: "reply".to_owned(),
                },
            ),
            event(
                session_id,
                3,
                SessionEventKind::UserMessage {
                    client_id: ClientId::new(),
                    text: "newer".to_owned(),
                },
            ),
        ];
        events.extend((4..20).map(|sequence| {
            event(
                session_id,
                sequence,
                SessionEventKind::AssistantDelta {
                    text: "x".to_owned(),
                },
            )
        }));
        events.push(event(
            session_id,
            20,
            SessionEventKind::AssistantMessage {
                text: "streamed final".to_owned(),
            },
        ));

        let window = projection_window_from_events(
            &events,
            &ProjectionWindowRequest {
                projection: SessionProjectionKind::Transcript,
                anchor: ProjectionWindowAnchor::Latest,
                direction: ProjectionWindowDirection::Backward,
                target: ProjectionWindowTarget {
                    min_items: Some(4),
                    min_estimated_rows: None,
                    min_bytes: None,
                    width_columns: Some(80),
                },
                limits: ProjectionWindowLimits {
                    max_items: 8,
                    max_events_scanned: 64,
                    max_bytes: 4096,
                },
            },
        )
        .expect("latest transcript windows are supported");

        assert_eq!(window.transcript_items.len(), 4);
        assert_eq!(
            window
                .transcript_items
                .iter()
                .map(|item| item.kind)
                .collect::<Vec<_>>(),
            vec![
                TranscriptProjectionItemKind::UserMessage,
                TranscriptProjectionItemKind::AssistantMessage,
                TranscriptProjectionItemKind::UserMessage,
                TranscriptProjectionItemKind::AssistantMessage,
            ]
        );
        assert_eq!(window.source_range.expect("source range").start_sequence, 1);
    }

    #[test]
    fn min_estimated_rows_pulls_short_older_messages() {
        let session_id = SessionId::new();
        let events = vec![
            event(
                session_id,
                1,
                SessionEventKind::UserMessage {
                    client_id: ClientId::new(),
                    text: "a".to_owned(),
                },
            ),
            event(
                session_id,
                2,
                SessionEventKind::AssistantMessage {
                    text: "b".to_owned(),
                },
            ),
            event(
                session_id,
                3,
                SessionEventKind::UserMessage {
                    client_id: ClientId::new(),
                    text: "c".to_owned(),
                },
            ),
            event(
                session_id,
                4,
                SessionEventKind::AssistantMessage {
                    text: "d".to_owned(),
                },
            ),
        ];

        let window = projection_window_from_events(
            &events,
            &ProjectionWindowRequest {
                projection: SessionProjectionKind::Transcript,
                anchor: ProjectionWindowAnchor::Latest,
                direction: ProjectionWindowDirection::Backward,
                target: ProjectionWindowTarget {
                    min_items: None,
                    min_estimated_rows: Some(3),
                    min_bytes: None,
                    width_columns: Some(80),
                },
                limits: ProjectionWindowLimits {
                    max_items: 8,
                    max_events_scanned: 64,
                    max_bytes: 4096,
                },
            },
        )
        .expect("latest transcript windows are supported");

        assert_eq!(window.transcript_items.len(), 3);
        assert_eq!(window.source_range.expect("source range").start_sequence, 2);
    }

    #[test]
    fn latest_projection_window_respects_byte_cap() {
        let session_id = SessionId::new();
        let events = vec![
            event(
                session_id,
                1,
                SessionEventKind::UserMessage {
                    client_id: ClientId::new(),
                    text: "older".to_owned(),
                },
            ),
            event(
                session_id,
                2,
                SessionEventKind::AssistantMessage {
                    text: "newer".to_owned(),
                },
            ),
        ];

        let window = projection_window_from_events(
            &events,
            &ProjectionWindowRequest {
                projection: SessionProjectionKind::Transcript,
                anchor: ProjectionWindowAnchor::Latest,
                direction: ProjectionWindowDirection::Backward,
                target: ProjectionWindowTarget {
                    min_items: Some(2),
                    min_estimated_rows: None,
                    min_bytes: None,
                    width_columns: Some(80),
                },
                limits: ProjectionWindowLimits {
                    max_items: 8,
                    max_events_scanned: 64,
                    max_bytes: 5,
                },
            },
        )
        .expect("latest transcript windows are supported");

        assert_eq!(window.transcript_items.len(), 1);
        assert_eq!(window.source_range.expect("source range").start_sequence, 2);
    }

    #[test]
    fn latest_projection_window_respects_scan_cap() {
        let session_id = SessionId::new();
        let events = vec![
            event(
                session_id,
                1,
                SessionEventKind::UserMessage {
                    client_id: ClientId::new(),
                    text: "older".to_owned(),
                },
            ),
            event(
                session_id,
                2,
                SessionEventKind::AssistantMessage {
                    text: "newer".to_owned(),
                },
            ),
        ];

        let window = projection_window_from_events(
            &events,
            &ProjectionWindowRequest {
                projection: SessionProjectionKind::Transcript,
                anchor: ProjectionWindowAnchor::Latest,
                direction: ProjectionWindowDirection::Backward,
                target: ProjectionWindowTarget {
                    min_items: Some(2),
                    min_estimated_rows: None,
                    min_bytes: None,
                    width_columns: Some(80),
                },
                limits: ProjectionWindowLimits {
                    max_items: 8,
                    max_events_scanned: 1,
                    max_bytes: 4096,
                },
            },
        )
        .expect("latest transcript windows are supported");

        assert_eq!(window.scanned_events, 1);
        assert_eq!(window.transcript_items.len(), 1);
        assert!(window.has_older);
    }

    fn index_entry(
        sequence: u64,
        kind: TranscriptProjectionItemKind,
        content_bytes: usize,
    ) -> crate::index::TranscriptProjectionIndexEntry {
        crate::index::TranscriptProjectionIndexEntry {
            projection_item_id: format!("transcript:{sequence}:{sequence}"),
            kind,
            source_range: ProjectionSourceRange {
                start_sequence: sequence,
                end_sequence: sequence,
            },
            content_bytes,
        }
    }

    fn event(session_id: SessionId, sequence: u64, kind: SessionEventKind) -> SessionEvent {
        SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            session_id,
            provenance: None,
            kind,
        }
    }
}
