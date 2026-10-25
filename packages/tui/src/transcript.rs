//! Native compatibility exports for the shared transcript acceptance boundary.

pub use bcode_tui_components::transcript::item::*;

#[cfg(test)]
use bcode_session_models::SessionEvent;
#[cfg(test)]
use bcode_session_view::SessionView;
#[cfg(test)]
use bcode_session_view_models::TranscriptViewItemKind;

/// Project session events through the shared semantic view into terminal transcript items,
/// optionally hiding reasoning items.
#[cfg(test)]
#[must_use]
pub fn transcript_items_from_events_with_reasoning(
    events: &[SessionEvent],
    include_reasoning: bool,
) -> Vec<TranscriptItem> {
    let mut view = SessionView::new();
    for event in events {
        view.apply_event(event);
    }
    view.snapshot()
        .transcript
        .items
        .iter()
        .filter(|item| {
            include_reasoning
                || !matches!(item.kind, TranscriptViewItemKind::ReasoningMessage { .. })
        })
        .map(terminal_item_from_shared)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_view_models::InteractionViewSummary;

    #[test]
    fn active_interaction_layout_height_changes_item_signature() {
        let item = terminal_interaction_item_from_shared(&InteractionViewSummary {
            producer_id: Some("example.plugin".to_owned()),
            exchange_schema: Some("example.request".to_owned()),
            exchange_schema_version: Some(1),
            interaction_id: "question-1".to_owned(),
            kind: "example.interaction".to_owned(),
            tool_call_id: None,
            title: Some("Question".to_owned()),
            required: true,
            snapshot: None,
            state: bcode_session_view_models::InteractionViewState::Pending,
            status_detail: None,
            resolved: false,
            resolution: None,
        });
        let fallback = crate::transcript_projection::test_layout_signature(&item, 80, None);
        let first = crate::transcript_projection::test_layout_signature_with_interaction_height(
            &item,
            80,
            "question-1",
            5,
        );
        let resized = crate::transcript_projection::test_layout_signature_with_interaction_height(
            &item,
            80,
            "question-1",
            12,
        );

        assert_ne!(fallback, first);
        assert_ne!(first, resized);
    }
}
