//! Unified tool lifecycle presentation.

use std::borrow::Cow;

use super::theme::{color, space, typeface};
use crate::context::PresentationContext;
use bcode_plugin_sdk::path::display_from_current_dir;
use bcode_session_view_models::{ToolInvocationView, ToolInvocationViewStatus, ToolResultView};
use hyperchad::template::{Containers, container};

use super::adapters::{ARTIFACT_ADAPTERS, json_panel, render_plugin_visual};
use super::components::{StatusTone, code_output, disclosure, tool_card};

const MAX_INLINE_ARGUMENT_CHARS: usize = 8_000;
const MAX_INLINE_OUTPUT_CHARS: usize = 32_000;

fn supported_inline_image_content_type(content_type: &str) -> bool {
    matches!(
        content_type.split(';').next().map(str::trim),
        Some("image/png" | "image/jpeg" | "image/gif" | "image/webp")
    )
}

fn bounded_preview(text: &str, max_chars: usize) -> (Cow<'_, str>, bool) {
    let Some((byte_index, _)) = text.char_indices().nth(max_chars) else {
        return (Cow::Borrowed(text), false);
    };
    (Cow::Owned(text[..byte_index].to_owned()), true)
}

fn lifecycle_status(tool: &ToolInvocationView) -> (&'static str, &'static str) {
    if tool.timing.timed_out == Some(true) {
        ("timed out", color::ERROR)
    } else if tool.is_error == Some(true) {
        ("failed", color::ERROR)
    } else {
        match tool.status {
            ToolInvocationViewStatus::Requested => ("requested", color::MUTED),
            ToolInvocationViewStatus::Running => ("running", color::SUCCESS),
            ToolInvocationViewStatus::Finished => ("finished", color::INFO),
            ToolInvocationViewStatus::Cancelled => ("cancelled", color::WARNING),
            ToolInvocationViewStatus::Failed => ("failed", color::ERROR),
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

#[cfg(test)]
pub(super) fn render_tool_lifecycle(tool: &ToolInvocationView) -> Containers {
    render_tool_lifecycle_with_context(tool, None, &crate::context::StaticPresentationContext)
}

pub(super) fn render_tool_lifecycle_with_context(
    tool: &ToolInvocationView,
    session_id: Option<bcode_session_models::SessionId>,
    presentation: &impl PresentationContext,
) -> Containers {
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
    let status_tone = if status_color == color::ERROR {
        StatusTone::Error
    } else if status_color == color::WARNING {
        StatusTone::Warning
    } else if status_color == color::SUCCESS {
        StatusTone::Success
    } else if status_color == color::INFO {
        StatusTone::Info
    } else {
        StatusTone::Neutral
    };
    let status_label = timing.map_or_else(
        || status.to_owned(),
        |timing| format!("{status} · {timing}"),
    );
    let subtitle = tool
        .working_directory
        .as_ref()
        .map(|directory| display_from_current_dir(directory).to_string());
    let content = container! {
        @if let Some((arguments, truncated)) = arguments {
            (disclosure("developer arguments", &container! {
                (code_output(&arguments, StatusTone::Neutral))
                @if truncated {
                    div color=(color::WARNING) font-size=((typeface::DETAIL)) margin-top=((space::S6)) { "Arguments truncated for display." }
                }
            }))
        }
        @if let Some(visual) = &tool.request_visual {
            (render_plugin_visual("request", visual))
        }
        @if let Some((output, truncated)) = output {
            (disclosure("live output", &container! {
                (code_output(&output, StatusTone::Neutral))
                @if truncated {
                    div color=(color::WARNING) font-size=((typeface::DETAIL)) margin-top=((space::S6)) { "Output truncated for display; the underlying result remains available through its artifact when provided." }
                }
            }))
        }
        @if let Some(result) = &tool.result {
            div margin-top=((space::SM)) { (render_tool_result_with_context(result, session_id, presentation)) }
        } @else if let Some((result_text, truncated)) = result_text {
            (disclosure("result", &container! {
                (code_output(&result_text, if tool.is_error == Some(true) { StatusTone::Error } else { StatusTone::Neutral }))
                @if truncated {
                    div color=(color::WARNING) font-size=((typeface::DETAIL)) margin-top=((space::S6)) { "Result truncated for display." }
                }
            }))
        }
    };
    tool_card(
        tool.tool_name.as_deref().unwrap_or("Unknown tool"),
        subtitle.as_deref(),
        &status_label,
        status_tone,
        &content,
    )
}

#[cfg(test)]
pub(super) fn render_tool_result(result: &ToolResultView) -> Containers {
    render_tool_result_with_context(result, None, &crate::context::StaticPresentationContext)
}

pub(super) fn render_tool_result_with_context(
    result: &ToolResultView,
    session_id: Option<bcode_session_models::SessionId>,
    context: &impl PresentationContext,
) -> Containers {
    match result {
        ToolResultView::Text { text } => {
            let (text, truncated) = bounded_preview(text, MAX_INLINE_OUTPUT_CHARS);
            container! {
                (code_output(&text, StatusTone::Neutral))
                @if truncated { div color=(color::WARNING) font-size=((typeface::DETAIL)) margin-top=((space::S6)) { "Text result truncated for display." } }
            }
        }
        ToolResultView::Json { value } => serde_json::from_str(value).map_or_else(
            |_| {
                let (value, truncated) = bounded_preview(value, MAX_INLINE_OUTPUT_CHARS);
                container! {
                    (code_output(&value, StatusTone::Warning))
                    @if truncated { div color=(color::WARNING) font-size=((typeface::DETAIL)) margin-top=((space::S6)) { "Malformed JSON result truncated for display." } }
                }
            },
            |value| json_panel("result details", &value),
        ),
        ToolResultView::Artifact { artifact } => {
            let target = session_id
                .filter(|_| !artifact.artifact.artifact_id.is_empty())
                .and_then(|session_id| {
                    artifact.artifact.refs.iter().find_map(|reference| {
                        (!reference.key.is_empty()
                            && reference
                                .content_type
                                .as_deref()
                                .is_some_and(supported_inline_image_content_type))
                        .then(|| {
                            context.artifact_target(
                                session_id,
                                &artifact.artifact.artifact_id,
                                &reference.key,
                            )
                        })
                        .flatten()
                    })
                });
            if artifact.artifact.schema == "bcode.filesystem.image"
                && artifact.artifact.schema_version == 1
            {
                return super::adapters::render_filesystem_image_result(
                    artifact,
                    target.as_deref(),
                )
                .unwrap_or_else(|| json_panel("artifact details", &artifact.generic_payload));
            }
            ARTIFACT_ADAPTERS
                .get(&(
                    artifact.artifact.schema.as_str(),
                    artifact.artifact.schema_version,
                ))
                .and_then(|adapter| adapter(artifact))
                .unwrap_or_else(|| json_panel("artifact details", &artifact.generic_payload))
        }
    }
}
