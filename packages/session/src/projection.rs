use bcode_session_models::{
    ProjectionSourceRange, SessionEvent, SessionEventKind, TranscriptProjectionItem,
    TranscriptProjectionItemKind,
};

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

struct TranscriptProjectionBuilder {
    items: Vec<TranscriptProjectionItem>,
    width_columns: Option<u16>,
    assistant_stream: Option<PendingStream>,
    reasoning_stream: Option<PendingStream>,
}

impl TranscriptProjectionBuilder {
    const fn new(width_columns: Option<u16>) -> Self {
        Self {
            items: Vec::new(),
            width_columns,
            assistant_stream: None,
            reasoning_stream: None,
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

fn non_streaming_item(event: &SessionEvent) -> Option<(TranscriptProjectionItemKind, usize)> {
    match &event.kind {
        SessionEventKind::UserMessage { text, .. } | SessionEventKind::SystemMessage { text } => {
            Some((TranscriptProjectionItemKind::UserMessage, text.len()))
        }
        SessionEventKind::ToolCallRequested { arguments_json, .. } => Some((
            TranscriptProjectionItemKind::ToolInvocation,
            arguments_json.len(),
        )),
        SessionEventKind::ToolCallFinished { result, .. } => {
            Some((TranscriptProjectionItemKind::ToolInvocation, result.len()))
        }
        SessionEventKind::ToolInvocationStream { event } => Some((
            TranscriptProjectionItemKind::ToolInvocation,
            tool_stream_content_bytes(event),
        )),
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

const fn tool_stream_content_bytes(
    event: &bcode_session_models::ToolInvocationStreamEvent,
) -> usize {
    match event {
        bcode_session_models::ToolInvocationStreamEvent::Started { tool_name, .. } => {
            tool_name.len()
        }
        bcode_session_models::ToolInvocationStreamEvent::OutputDelta { text, .. }
        | bcode_session_models::ToolInvocationStreamEvent::Status { message: text, .. } => {
            text.len()
        }
        bcode_session_models::ToolInvocationStreamEvent::Finished { .. } => 0,
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
        CURRENT_SESSION_EVENT_SCHEMA_VERSION, ClientId, SessionId, ToolInvocationStreamEvent,
        ToolOutputStream,
    };

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
