//! Transcript timeline and semantic item presentation.

use std::borrow::Cow;

use super::theme::{color, radius, space, surface, typeface};
use crate::context::{PresentationAction, PresentationContext};
use bcode_session_view_models::{
    ChatMessageView, PluginVisualView, SessionViewSnapshot, TextFormat, TranscriptViewItemKind,
};
use hyperchad::template::{Containers, container};

use super::adapters::{VISUAL_ADAPTERS, json_panel, render_plugin_visual};
use super::components::{
    MessageArticle, conversation_timeline, disclosure, empty_state, message_article,
    truncation_notice, unsupported_content,
};
use super::permissions::permission_history;
use super::semantic_dom_id;
use super::tools::render_tool_lifecycle_with_context;
use super::usage::usage_transcript_item;

const MAX_INLINE_MESSAGE_CHARS: usize = 100_000;

fn bounded_message_text(text: &str) -> (Cow<'_, str>, bool) {
    let Some((byte_index, _)) = text.char_indices().nth(MAX_INLINE_MESSAGE_CHARS) else {
        return (Cow::Borrowed(text), false);
    };
    (Cow::Owned(text[..byte_index].to_owned()), true)
}

pub(super) fn is_superseded_tool_request(
    items: &[bcode_session_view_models::TranscriptViewItem],
    index: usize,
) -> bool {
    let Some(tool_call_id) = (match &items[index].kind {
        TranscriptViewItemKind::ToolRequest { tool } => Some(tool.tool_call_id.as_str()),
        _ => None,
    }) else {
        return false;
    };
    items[index + 1..].iter().any(|item| {
        matches!(
            &item.kind,
            TranscriptViewItemKind::ToolInvocation { tool }
                if tool.tool_call_id == tool_call_id
        )
    })
}

pub(super) fn transcript_section(
    snapshot: &SessionViewSnapshot,
    context: &impl PresentationContext,
) -> Containers {
    conversation_timeline(&container! {
            @if snapshot.transcript.has_older_history {
                @if let (Some(session_id), Some(anchor_sequence)) = (snapshot.session_id, snapshot.transcript.source_start_sequence) {
                    form hx-post=(context.action_target(PresentationAction::MoveHistoryWindow)) hx-target="#bcode-web-shell" hx-swap=this margin-bottom=((space::MD)) {
                        input type=hidden name="session_id" value=(session_id.to_string());
                        input type=hidden name="direction" value="older";
                        input type=hidden name="anchor_sequence" value=(anchor_sequence.to_string());
                        button type=submit background=(surface::CONTROL) color=(color::INFO) border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) padding="10, 14" { "load older history" }
                    }
                }
            }
            @if snapshot.transcript.items.is_empty() {
                (empty_state(if snapshot.session_id.is_some() {
                    "This session has no conversation entries in the current history view."
                } else {
                    "Attach or create a session to begin."
                }))
            } @else {
                @for (index, item) in snapshot.transcript.items.iter().enumerate() {
                    @if should_render_transcript_item(item, snapshot.thinking.visible)
                        && !is_superseded_tool_request(&snapshot.transcript.items, index)
                        && !is_active_interaction_summary(item, &snapshot.interactions) {
                        (transcript_item_with_context(item, snapshot.session_id, context))
                    }
                }
            }
            @if snapshot.transcript.has_newer_history {
                @if let (Some(session_id), Some(anchor_sequence)) = (snapshot.session_id, snapshot.transcript.source_end_sequence) {
                    form hx-post=(context.action_target(PresentationAction::MoveHistoryWindow)) hx-target="#bcode-web-shell" hx-swap=this margin-top=((space::MD)) {
                        input type=hidden name="session_id" value=(session_id.to_string());
                        input type=hidden name="direction" value="newer";
                        input type=hidden name="anchor_sequence" value=(anchor_sequence.to_string());
                        button type=submit background=(surface::CONTROL) color=(color::INFO) border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) padding="10, 14" { "load newer history" }
                    }
                }
            }
    })
}

pub(super) const fn should_render_transcript_item(
    item: &bcode_session_view_models::TranscriptViewItem,
    reasoning_visible: bool,
) -> bool {
    reasoning_visible || !matches!(&item.kind, TranscriptViewItemKind::ReasoningMessage { .. })
}

pub(super) fn is_active_interaction_summary(
    item: &bcode_session_view_models::TranscriptViewItem,
    active_interactions: &[bcode_session_view_models::InteractionViewSummary],
) -> bool {
    let TranscriptViewItemKind::Interaction { interaction } = &item.kind else {
        return false;
    };
    !interaction.resolved
        && active_interactions
            .iter()
            .any(|active| active.interaction_id == interaction.interaction_id)
}

#[cfg(test)]
pub(super) fn transcript_item(item: &bcode_session_view_models::TranscriptViewItem) -> Containers {
    transcript_item_with_context(item, None, &crate::context::StaticPresentationContext)
}

fn transcript_item_with_context(
    item: &bcode_session_view_models::TranscriptViewItem,
    session_id: Option<bcode_session_models::SessionId>,
    context: &impl PresentationContext,
) -> Containers {
    let (background, accent, margin_left, margin_right) = match &item.kind {
        TranscriptViewItemKind::UserMessage { .. } => (surface::USER_MESSAGE, color::INFO, 48, 0),
        TranscriptViewItemKind::AssistantMessage { .. } => (surface::APP, color::SUCCESS, 0, 48),
        TranscriptViewItemKind::ReasoningMessage { .. } => {
            (surface::PANEL, color::REASONING, 24, 24)
        }
        TranscriptViewItemKind::SystemMessage { .. }
        | TranscriptViewItemKind::Compaction { .. }
        | TranscriptViewItemKind::Skill { .. } => (surface::PANEL, color::MUTED, 24, 24),
        _ => (surface::APP, surface::DISABLED, 0, 0),
    };
    let item_id = semantic_dom_id("transcript-item", item.id.get());
    let developer_detail = disclosure(
        "developer details",
        &container! {
            div color=(color::MUTED) font-size=((typeface::DETAIL)) {
                "item " (item.id.get().to_string()) " · revision " (item.revision.to_string())
                @if let Some(sequence) = item.sequence {
                    " · event " (sequence.to_string())
                }
                @if let Some(timestamp_ms) = item.timestamp_ms {
                    " · timestamp " (timestamp_ms.to_string())
                }
            }
        },
    );
    message_article(&MessageArticle {
        id: &item_id,
        label: item_label(&item.kind),
        streaming: item.streaming,
        background,
        accent,
        margins: (margin_left, margin_right),
        content: &transcript_item_body_with_context(&item.kind, session_id, context),
        developer_detail: &developer_detail,
    })
}

fn compact_unsupported_contribution(
    placement: bcode_session_models::ToolContributionPlacement,
) -> Containers {
    let label = match placement {
        bcode_session_models::ToolContributionPlacement::Request => "tool request",
        bcode_session_models::ToolContributionPlacement::Progress => "tool progress",
        bcode_session_models::ToolContributionPlacement::Result => "tool result",
        bcode_session_models::ToolContributionPlacement::Supplemental
        | bcode_session_models::ToolContributionPlacement::Hidden => return Containers::default(),
    };
    unsupported_content(&format!(
        "No rich presentation is available for this {label}; bounded developer details remain available."
    ))
}

pub(super) fn message_content(message: &ChatMessageView) -> Containers {
    if message.text.is_empty() {
        return container! {
            div color=(color::MUTED) { "Empty message" }
        };
    }
    let (text, truncated) = bounded_message_text(&message.text);
    let content = match message.format {
        TextFormat::Markdown => vec![hyperchad::markdown::markdown_to_container(&text)],
        TextFormat::PlainText | TextFormat::Json => container! {
            div white-space="preserve-wrap" margin=0 color=(color::TEXT) { (text) }
        },
    };
    container! {
        div {
            (content)
            @if truncated {
                (truncation_notice("Message truncated for display."))
            }
        }
    }
}

fn compaction_notice(compaction: &bcode_session_view_models::CompactionView) -> Containers {
    container! {
        aside color=(color::MUTED) {
            div font-size=((typeface::DETAIL)) margin-bottom=((space::XS)) {
                (match compaction.status {
                    bcode_session_view_models::CompactionViewStatus::Local => "Local context compacted",
                    bcode_session_view_models::CompactionViewStatus::Provider => "Provider context compacted",
                })
            }
            div white-space="preserve-wrap" margin=0 { (compaction.text) }
            @if let Some(provider) = &compaction.provider_plugin_id { div font-size=((typeface::DETAIL)) margin-top=((space::XS)) { "provider: " (provider) } }
            @if let Some(model) = &compaction.model_id { div font-size=((typeface::DETAIL)) margin-top=((space::S2)) { "model: " (model) } }
        }
    }
}

fn interaction_notice(
    interaction: &bcode_session_view_models::InteractionViewSummary,
) -> Containers {
    let status = if interaction.resolved {
        "resolved"
    } else {
        "pending"
    };
    container! {
        aside {
            div justify-content=space-between gap=((space::SM)) {
                div color=(color::STRONG) { (interaction.title.as_deref().unwrap_or("Interactive request")) }
                div color=(if interaction.resolved { color::MUTED } else { color::WARNING }) font-size=((typeface::DETAIL)) { (status) }
            }
            div color=(color::MUTED) font-size=((typeface::LABEL)) { (interaction.kind) @if interaction.required { " · required" } }
            @if let Some(resolution) = &interaction.resolution {
                (json_panel("resolution", resolution))
            } @else if let Some(snapshot) = &interaction.snapshot {
                (json_panel("snapshot", snapshot))
            }
        }
    }
}

fn skill_notice(skill: &bcode_session_view_models::SkillView) -> Containers {
    container! {
        aside color=(if matches!(skill.status, bcode_session_view_models::SkillViewStatus::Failed) { color::ERROR } else { color::MUTED }) {
            div font-size=((typeface::DETAIL)) margin-bottom=((space::XS)) { (item_label(&TranscriptViewItemKind::Skill { skill: skill.clone() })) }
            div white-space="preserve-wrap" margin=0 { (skill.text) }
        }
    }
}

#[cfg(test)]
pub(super) fn transcript_item_body(kind: &TranscriptViewItemKind) -> Containers {
    transcript_item_body_with_context(kind, None, &crate::context::StaticPresentationContext)
}

fn transcript_item_body_with_context(
    kind: &TranscriptViewItemKind,
    session_id: Option<bcode_session_models::SessionId>,
    context: &impl PresentationContext,
) -> Containers {
    match kind {
        TranscriptViewItemKind::UserMessage { message }
        | TranscriptViewItemKind::AssistantMessage { message } => message_content(message),
        TranscriptViewItemKind::ReasoningMessage { message } => container! {
            details {
                summary color=(color::REASONING) { "Reasoning" }
                div margin-top=((space::SM)) { (message_content(message)) }
            }
        },
        TranscriptViewItemKind::SystemMessage { message } => container! {
            aside color=(color::MUTED) { (message_content(message)) }
        },
        TranscriptViewItemKind::Compaction { compaction } => compaction_notice(compaction),
        TranscriptViewItemKind::Skill { skill } => skill_notice(skill),
        TranscriptViewItemKind::ToolRequest { tool }
        | TranscriptViewItemKind::ToolInvocation { tool } => {
            render_tool_lifecycle_with_context(tool, session_id, context)
        }
        TranscriptViewItemKind::Permission { permission } => permission_history(permission),
        TranscriptViewItemKind::Usage { usage } => usage_transcript_item(usage),
        TranscriptViewItemKind::RuntimeWork { work } => container! {
            div {
                div color=(color::STRONG) { (work.message.as_deref().unwrap_or("Runtime work")) }
                div color=(color::MUTED) font-size=((typeface::LABEL)) { (format!("{:?}", work.status)) }
            }
        },
        TranscriptViewItemKind::Interaction { interaction } => interaction_notice(interaction),
        TranscriptViewItemKind::PluginVisual { visual } => {
            render_plugin_visual("plugin visual", visual)
        }
        TranscriptViewItemKind::ToolContribution {
            contribution,
            placement,
        } => {
            let visual = PluginVisualView::from(bcode_session_models::PluginVisualDescriptor {
                visual_id: Some(format!(
                    "{}-{}",
                    contribution.invocation_id, contribution.contribution_id
                )),
                producer_plugin_id: Some(contribution.producer_id.clone()),
                schema: contribution.schema.clone(),
                schema_version: contribution.schema_version,
                title: Some("Tool contribution".to_owned()),
                subtitle: None,
                payload: contribution.payload.clone(),
            });
            VISUAL_ADAPTERS
                .get(&(contribution.schema.as_str(), contribution.schema_version))
                .and_then(|adapter| adapter(&visual))
                .unwrap_or_else(|| compact_unsupported_contribution(*placement))
        }
    }
}

pub(super) const fn item_label(kind: &TranscriptViewItemKind) -> &'static str {
    match kind {
        TranscriptViewItemKind::UserMessage { .. } => "user",
        TranscriptViewItemKind::AssistantMessage { .. } => "assistant",
        TranscriptViewItemKind::ReasoningMessage { .. } => "reasoning",
        TranscriptViewItemKind::ToolInvocation { .. } => "tool",
        TranscriptViewItemKind::ToolRequest { .. } => "tool request",
        TranscriptViewItemKind::Permission { .. } => "permission",
        TranscriptViewItemKind::RuntimeWork { .. } => "runtime work",
        TranscriptViewItemKind::Usage { .. } => "usage",
        TranscriptViewItemKind::Compaction { .. } => "compaction",
        TranscriptViewItemKind::Interaction { .. } => "interaction",
        TranscriptViewItemKind::Skill { skill } => match skill.status {
            bcode_session_view_models::SkillViewStatus::ContextLoaded => "skill context",
            bcode_session_view_models::SkillViewStatus::Failed => "skill error",
            bcode_session_view_models::SkillViewStatus::Invoked
            | bcode_session_view_models::SkillViewStatus::Suggested => "skill",
        },
        TranscriptViewItemKind::SystemMessage { .. } => "system",
        TranscriptViewItemKind::PluginVisual { .. } => "plugin visual",
        TranscriptViewItemKind::ToolContribution { .. } => "tool contribution",
    }
}
