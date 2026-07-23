//! Unified tool lifecycle presentation.

use std::borrow::Cow;

use bcode_plugin_sdk::path::display_from_current_dir;
use bcode_session_view_models::{ToolInvocationView, ToolInvocationViewStatus, ToolResultView};
use hyperchad::template::{Containers, container};

use super::adapters::{ARTIFACT_ADAPTERS, json_panel, render_plugin_visual};

const MAX_INLINE_ARGUMENT_CHARS: usize = 8_000;
const MAX_INLINE_OUTPUT_CHARS: usize = 32_000;

fn bounded_preview(text: &str, max_chars: usize) -> (Cow<'_, str>, bool) {
    let Some((byte_index, _)) = text.char_indices().nth(max_chars) else {
        return (Cow::Borrowed(text), false);
    };
    (Cow::Owned(text[..byte_index].to_owned()), true)
}

fn lifecycle_status(tool: &ToolInvocationView) -> (&'static str, &'static str) {
    if tool.timing.timed_out == Some(true) {
        ("timed out", "#f85149")
    } else if tool.is_error == Some(true) {
        ("failed", "#f85149")
    } else {
        match tool.status {
            ToolInvocationViewStatus::Requested => ("requested", "#8b949e"),
            ToolInvocationViewStatus::Running => ("running", "#7ee787"),
            ToolInvocationViewStatus::Finished => ("finished", "#58a6ff"),
            ToolInvocationViewStatus::Cancelled => ("cancelled", "#f2cc60"),
            ToolInvocationViewStatus::Failed => ("failed", "#f85149"),
        }
    }
}

fn timing_summary(tool: &ToolInvocationView) -> Option<String> {
    if let Some(duration_ms) = tool.timing.duration_ms {
        return Some(format!(
            "{}.{:03}s",
            duration_ms / 1_000,
            duration_ms % 1_000
        ));
    }
    match (tool.timing.started_at_ms, tool.timing.finished_at_ms) {
        (Some(started), Some(finished)) => {
            let duration_ms = finished.saturating_sub(started);
            Some(format!(
                "{}.{:03}s",
                duration_ms / 1_000,
                duration_ms % 1_000
            ))
        }
        _ => None,
    }
}

pub(super) fn render_tool_lifecycle(tool: &ToolInvocationView) -> Containers {
    let (status, status_color) = lifecycle_status(tool);
    let timing = timing_summary(tool);
    let arguments = tool
        .arguments_json
        .as_deref()
        .map(|arguments| bounded_preview(arguments, MAX_INLINE_ARGUMENT_CHARS));
    let output = tool
        .output
        .as_ref()
        .map(|output| bounded_preview(&output.text, MAX_INLINE_OUTPUT_CHARS));
    let result_text = tool
        .result_text
        .as_deref()
        .map(|result| bounded_preview(result, MAX_INLINE_OUTPUT_CHARS));
    container! {
        section background="#0d1117" border="1, #30363d" border-radius=8 padding=12 {
            div justify-content=space-between gap=12 margin-bottom=8 {
                div {
                    div color="#f0f6fc" { (tool.tool_name.as_deref().unwrap_or("Unknown tool")) }
                    @if let Some(working_directory) = &tool.working_directory {
                        div color="#8b949e" font-size=11 margin-top=3 { (display_from_current_dir(working_directory).to_string()) }
                    }
                }
                div color=(status_color) font-size=12 {
                    (status)
                    @if let Some(timing) = timing { " · " (timing) }
                }
            }
            @if let Some((arguments, truncated)) = arguments {
                details margin-bottom=8 {
                    summary color="#8b949e" font-size=11 { "developer arguments" }
                    div white-space="preserve-wrap" background="#010409" border="1, #30363d" border-radius=6 padding=8 margin-top=6 color="#c9d1d9" { (arguments) }
                    @if truncated {
                        div color="#f2cc60" font-size=11 margin-top=6 { "Arguments truncated for display." }
                    }
                }
            }
            @if let Some(visual) = &tool.request_visual {
                (render_plugin_visual("request", visual))
            }
            @if let Some((output, truncated)) = output {
                details open=true margin-top=8 {
                    summary color="#8b949e" font-size=11 { "live output" }
                    div white-space="preserve-wrap" background="#010409" border="1, #30363d" border-radius=6 padding=8 margin-top=6 color="#c9d1d9" { (output) }
                    @if truncated {
                        div color="#f2cc60" font-size=11 margin-top=6 { "Output truncated for display; the underlying result remains available through its artifact when provided." }
                    }
                }
            }
            @if let Some(result) = &tool.result {
                div margin-top=8 { (render_tool_result(result)) }
            } @else if let Some((result_text, truncated)) = result_text {
                details open=true margin-top=8 {
                    summary color=(status_color) font-size=11 { "result" }
                    div white-space="preserve-wrap" background="#010409" border="1, #30363d" border-radius=6 padding=8 margin-top=6 color="#c9d1d9" { (result_text) }
                    @if truncated {
                        div color="#f2cc60" font-size=11 margin-top=6 { "Result truncated for display." }
                    }
                }
            }
        }
    }
}

pub(super) fn render_tool_result(result: &ToolResultView) -> Containers {
    match result {
        ToolResultView::Text { text } => container! {
            div white-space="preserve-wrap" background="#010409" border="1, #30363d" border-radius=6 padding=10 color="#c9d1d9" { (text) }
        },
        ToolResultView::Json { value } => serde_json::from_str(value).map_or_else(
            |_| container! {
                div white-space="preserve-wrap" background="#010409" border="1, #30363d" border-radius=6 padding=10 color="#c9d1d9" { (value) }
            },
            |value| json_panel("result details", &value),
        ),
        ToolResultView::Artifact { artifact } => ARTIFACT_ADAPTERS
            .get(&(
                artifact.artifact.schema.as_str(),
                artifact.artifact.schema_version,
            ))
            .and_then(|adapter| adapter(artifact))
            .unwrap_or_else(|| json_panel("artifact details", &artifact.generic_payload)),
    }
}
