#![allow(clippy::module_name_repetitions)]

//! Shared reducer for live and replayed tool invocation presentation state.

use bcode_session_models::ToolInvocationPresentation;

use crate::transcript::{
    TranscriptItem, file_change_presentation_item, streaming_terminal_output_item,
};
use crate::transcript_document::TranscriptDocument;

/// Known tool request metadata needed to render presentation context.
#[derive(Debug, Clone, Copy)]
pub struct ToolInvocationRequestContext<'a> {
    /// Tool name.
    pub tool_name: &'a str,
}

/// Existing live/replay terminal transcript item metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalInvocationItemContext {
    /// Transcript item index if one has already been created.
    pub index: Option<usize>,
}

/// Mutable transcript operations needed by the tool invocation reducer.
pub trait ToolInvocationTranscript {
    /// Return a mutable transcript item by index.
    fn item_mut(&mut self, index: usize) -> Option<&mut TranscriptItem>;

    /// Push a transcript item and return its index.
    fn push_item(&mut self, item: TranscriptItem) -> usize;
}

impl ToolInvocationTranscript for Vec<TranscriptItem> {
    fn item_mut(&mut self, index: usize) -> Option<&mut TranscriptItem> {
        self.get_mut(index)
    }

    fn push_item(&mut self, item: TranscriptItem) -> usize {
        self.push(item);
        self.len().saturating_sub(1)
    }
}

impl ToolInvocationTranscript for TranscriptDocument {
    fn item_mut(&mut self, index: usize) -> Option<&mut TranscriptItem> {
        self.get_mut(index)
    }

    fn push_item(&mut self, item: TranscriptItem) -> usize {
        let index = self.len();
        self.push(item);
        index
    }
}

/// Shared tool presentation reducer input.
#[derive(Debug, Clone, Copy)]
pub struct ToolInvocationPresentationInput<'a> {
    /// Provider tool call identifier.
    pub tool_call_id: &'a str,
    /// Presentation start timestamp.
    pub started_at_ms: Option<u64>,
    /// Presentation finish timestamp.
    pub finished_at_ms: Option<u64>,
    /// Whether the tool failed.
    pub is_error: bool,
    /// Durable typed presentation.
    pub presentation: &'a ToolInvocationPresentation,
    /// Request context if known.
    pub request_context: Option<ToolInvocationRequestContext<'a>>,
    /// Existing terminal item context if known.
    pub terminal_context: Option<TerminalInvocationItemContext>,
}

/// Side effects produced by applying a tool presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolInvocationPresentationEffects {
    /// Tool call has durable presentation and generic final result should be suppressed.
    pub suppress_final_result: bool,
    /// Terminal transcript item index created or updated.
    pub terminal_index: Option<usize>,
    /// Whether terminal output is known after reducing this presentation.
    pub terminal_saw_output: bool,
}

/// Apply a typed tool presentation to transcript state.
#[must_use]
pub fn apply_tool_invocation_presentation<T>(
    transcript: &mut T,
    input: ToolInvocationPresentationInput<'_>,
) -> ToolInvocationPresentationEffects
where
    T: ToolInvocationTranscript,
{
    match input.presentation {
        ToolInvocationPresentation::Terminal {
            exit_code,
            timed_out,
            output,
            columns,
            rows,
            ..
        } => {
            let index = apply_terminal_presentation(
                transcript,
                TerminalPresentationInput {
                    tool_call_id: input.tool_call_id,
                    started_at_ms: input.started_at_ms,
                    finished_at_ms: input.finished_at_ms,
                    is_error: input.is_error,
                    exit_code: *exit_code,
                    timed_out: *timed_out,
                    output,
                    columns: (*columns).max(1),
                    rows: (*rows).max(1),
                    request_context: input.request_context,
                    terminal_context: input.terminal_context,
                },
            );
            ToolInvocationPresentationEffects {
                suppress_final_result: true,
                terminal_index: Some(index),
                terminal_saw_output: true,
            }
        }
        ToolInvocationPresentation::FileChange {
            tool_name,
            summary,
            path,
        } => {
            if input.request_context.is_none() {
                transcript.push_item(file_change_presentation_item(
                    input.tool_call_id,
                    tool_name,
                    summary,
                    path.as_deref(),
                    input.is_error,
                ));
            }
            ToolInvocationPresentationEffects {
                suppress_final_result: true,
                terminal_index: None,
                terminal_saw_output: false,
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct TerminalPresentationInput<'a> {
    tool_call_id: &'a str,
    started_at_ms: Option<u64>,
    finished_at_ms: Option<u64>,
    is_error: bool,
    exit_code: Option<i32>,
    timed_out: bool,
    output: &'a str,
    columns: u16,
    rows: u16,
    request_context: Option<ToolInvocationRequestContext<'a>>,
    terminal_context: Option<TerminalInvocationItemContext>,
}

fn apply_terminal_presentation<T>(transcript: &mut T, input: TerminalPresentationInput<'_>) -> usize
where
    T: ToolInvocationTranscript,
{
    if let Some(index) = input.terminal_context.and_then(|context| context.index)
        && let Some(item) = transcript.item_mut(index)
    {
        item.apply_terminal_presentation(
            input.output.to_owned(),
            input.exit_code,
            input.timed_out,
            input.is_error,
            input.finished_at_ms,
        );
        return index;
    }
    let index = transcript.push_item(streaming_terminal_output_item(
        input.tool_call_id,
        input.request_context.map(|context| context.tool_name),
        input.output,
        input.columns,
        input.rows,
        input.started_at_ms,
    ));
    if let Some(item) = transcript.item_mut(index) {
        item.apply_terminal_presentation(
            input.output.to_owned(),
            input.exit_code,
            input.timed_out,
            input.is_error,
            input.finished_at_ms,
        );
    }
    index
}
