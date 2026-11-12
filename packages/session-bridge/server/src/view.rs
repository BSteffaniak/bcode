//! The bridge's stable projection of a daemon session view.
//!
//! [`project`] maps the daemon's renderer-neutral [`SessionViewSnapshot`] into the bridge's
//! versioned [`SessionView`]. Every item keeps display text, so a kind the bridge protocol has
//! no name for still reads as [`ViewItemKind::Other`], and every text is bounded.

use bcode_session_bridge_models::{
    LastTurn, MAX_ITEM_TEXT_BYTES, MAX_TOOL_TEXT_BYTES, PendingPermission, SessionView,
    ToolCallStatus, ToolCallView, TurnOutcome, Usage, ViewItem, ViewItemKind, bounded_text,
};
use bcode_session_models::ModelTurnOutcome;
use bcode_session_view_models::{
    PermissionView, SessionConnectionViewStatus, SessionViewSnapshot, ToolInvocationView,
    ToolInvocationViewStatus, TranscriptViewItem, TranscriptViewItemKind,
};

/// Bytes kept of the reasoning text streaming right now.
const MAX_THINKING_BYTES: usize = 8 * 1024;

/// Project `snapshot` with at most `max_items` of its newest transcript items.
#[must_use]
pub fn project(snapshot: &SessionViewSnapshot, max_items: usize) -> SessionView {
    let items = &snapshot.transcript.items;
    let skipped = items.len().saturating_sub(max_items);
    let runtime = &snapshot.runtime;
    SessionView {
        session_id: snapshot
            .session_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
        title: snapshot.title.clone(),
        items: items[skipped..].iter().filter_map(project_item).collect(),
        has_older: skipped > 0 || snapshot.transcript.has_older_history,
        pending_permissions: snapshot
            .permissions
            .iter()
            .map(pending_permission)
            .collect(),
        turn_active: runtime.active_turn_id.is_some(),
        cancelling: runtime.cancelling,
        thinking: snapshot
            .thinking
            .active_text
            .as_deref()
            .filter(|_| snapshot.thinking.streaming)
            .map(|text| bounded_text(text, MAX_THINKING_BYTES)),
        agent: runtime.agent_id.clone(),
        model: runtime
            .effective_model_id
            .clone()
            .or_else(|| runtime.requested_model_id.clone()),
        client_count: snapshot
            .session_summary
            .as_ref()
            .map_or(0, |summary| summary.client_count),
        usage: Usage {
            tokens: runtime.cumulative_metered_tokens,
            cost_usd: (runtime.cost.unavailable_usage_count == 0)
                .then(|| runtime.cost.totals_micros.get("USD"))
                .flatten()
                .map(|micros| micros_to_dollars(*micros)),
        },
        error: match &snapshot.connection_status {
            SessionConnectionViewStatus::Error(message) => Some(message.clone()),
            _ => None,
        },
        last_turn: runtime.last_turn_outcome.map(|outcome| LastTurn {
            outcome: turn_outcome(outcome),
            message: runtime
                .last_turn_message
                .as_deref()
                .map(|text| bounded_text(text, MAX_TOOL_TEXT_BYTES)),
        }),
    }
}

const fn turn_outcome(outcome: ModelTurnOutcome) -> TurnOutcome {
    match outcome {
        ModelTurnOutcome::Completed => TurnOutcome::Completed,
        ModelTurnOutcome::Cancelled => TurnOutcome::Cancelled,
        ModelTurnOutcome::Error => TurnOutcome::Failed,
        ModelTurnOutcome::IdleTimeout => TurnOutcome::TimedOut,
        ModelTurnOutcome::ToolRoundLimitReached => TurnOutcome::ToolRoundLimit,
        ModelTurnOutcome::ProviderUnavailable => TurnOutcome::ProviderUnavailable,
    }
}

#[allow(clippy::cast_precision_loss)]
fn micros_to_dollars(micros: u64) -> f64 {
    micros as f64 / 1_000_000.0
}

fn item(source: &TranscriptViewItem, kind: ViewItemKind, text: &str) -> ViewItem {
    ViewItem {
        id: item_id(source),
        kind,
        text: bounded_text(text, MAX_ITEM_TEXT_BYTES),
        sequence: source.sequence,
        streaming: source.streaming,
        tool: None,
        request_id: None,
    }
}

/// The item's stable id as text (the id serializes as a plain string).
fn item_id(source: &TranscriptViewItem) -> String {
    serde_json::to_value(&source.id)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// One transcript item; live-only request drafts are left out.
fn project_item(source: &TranscriptViewItem) -> Option<ViewItem> {
    Some(match &source.kind {
        TranscriptViewItemKind::UserMessage { message } => {
            item(source, ViewItemKind::User, &message.text)
        }
        TranscriptViewItemKind::AssistantMessage { message } => {
            item(source, ViewItemKind::Assistant, &message.text)
        }
        TranscriptViewItemKind::ReasoningMessage { message } => {
            item(source, ViewItemKind::Reasoning, &message.text)
        }
        TranscriptViewItemKind::ReasoningActivity { activity } => {
            let text = activity
                .parts
                .iter()
                .map(|part| part.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            item(source, ViewItemKind::Reasoning, &text)
        }
        TranscriptViewItemKind::ToolInvocation { tool }
        | TranscriptViewItemKind::ToolRequest { tool } => tool_item(source, tool),
        TranscriptViewItemKind::ToolRequestDraft { .. } => return None,
        TranscriptViewItemKind::Permission { permission } => ViewItem {
            request_id: Some(permission.permission_id.clone()),
            ..item(
                source,
                ViewItemKind::Permission,
                &format!("permission requested for {}", permission.tool_name),
            )
        },
        TranscriptViewItemKind::Interaction { interaction } => ViewItem {
            request_id: Some(interaction.interaction_id.clone()),
            ..item(
                source,
                ViewItemKind::Interaction,
                interaction.title.as_deref().unwrap_or(&interaction.kind),
            )
        },
        TranscriptViewItemKind::Compaction { compaction } => {
            item(source, ViewItemKind::Notice, &compaction.text)
        }
        TranscriptViewItemKind::Skill { skill } => item(source, ViewItemKind::Notice, &skill.text),
        TranscriptViewItemKind::SystemMessage { message } => {
            item(source, ViewItemKind::Notice, &message.text)
        }
        // Tool contributions are producer-owned presentation the bridge protocol does not model.
        TranscriptViewItemKind::ToolContribution { .. } => {
            item(source, ViewItemKind::Other, "tool output")
        }
    })
}

fn tool_item(source: &TranscriptViewItem, tool: &ToolInvocationView) -> ViewItem {
    let name = tool.tool_name.clone().unwrap_or_default();
    let status = match tool.status {
        ToolInvocationViewStatus::Requested => ToolCallStatus::Requested,
        ToolInvocationViewStatus::Running => ToolCallStatus::Running,
        ToolInvocationViewStatus::Waiting => ToolCallStatus::Waiting,
        ToolInvocationViewStatus::Finished if tool.is_error == Some(true) => ToolCallStatus::Failed,
        ToolInvocationViewStatus::Finished => ToolCallStatus::Finished,
        ToolInvocationViewStatus::Cancelled => ToolCallStatus::Cancelled,
        ToolInvocationViewStatus::Failed => ToolCallStatus::Failed,
    };
    ViewItem {
        tool: Some(ToolCallView {
            tool_call_id: tool.tool_call_id.clone(),
            name: name.clone(),
            status,
            arguments: tool
                .arguments_json
                .as_deref()
                .map(|arguments| bounded_text(arguments, MAX_TOOL_TEXT_BYTES)),
            result: tool
                .result_text
                .as_deref()
                .map(|result| bounded_text(result, MAX_TOOL_TEXT_BYTES)),
            is_error: tool.is_error.unwrap_or(false),
        }),
        ..item(source, ViewItemKind::Tool, &name)
    }
}

/// A pending permission as the bridge shows it.
#[must_use]
pub fn pending_permission(permission: &PermissionView) -> PendingPermission {
    PendingPermission {
        permission_id: permission.permission_id.clone(),
        session_id: permission.session_id.map(|id| id.to_string()),
        tool_name: permission.tool_name.clone(),
        arguments: bounded_text(&permission.arguments_json, MAX_TOOL_TEXT_BYTES),
        title: permission.title.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_view_models::ChatMessageView;

    fn transcript_item(id: u64, kind: TranscriptViewItemKind) -> TranscriptViewItem {
        TranscriptViewItem {
            id: bcode_session_view_models::TranscriptViewItemId::event(id),
            revision: 1,
            sequence: Some(id),
            timestamp_ms: None,
            output_location: None,
            streaming: false,
            kind,
        }
    }

    #[test]
    fn items_are_bounded_newest_first_and_keep_display_text() {
        let mut snapshot = SessionViewSnapshot::empty();
        snapshot.transcript.items = (1..=5)
            .map(|id| {
                transcript_item(
                    id,
                    TranscriptViewItemKind::AssistantMessage {
                        message: ChatMessageView::markdown("x".repeat(MAX_ITEM_TEXT_BYTES + 10)),
                    },
                )
            })
            .collect();
        let view = project(&snapshot, 2);
        assert_eq!(view.items.len(), 2);
        assert!(view.has_older);
        assert_eq!(view.items[0].sequence, Some(4));
        assert!(view.items[0].text.len() <= MAX_ITEM_TEXT_BYTES);
    }
}
