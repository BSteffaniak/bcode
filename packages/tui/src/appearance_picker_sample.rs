//! Sample transcript rendered by the appearance picker's preview pane and the
//! generated theme gallery.
//!
//! The sample is ordinary transcript content, so it renders through the real
//! transcript row pipeline under the live structural policy and shows exactly
//! what a palette and profile do to tools and messages.

use bcode_session_view_models::ToolInvocationViewStatus;
use bcode_tui_components::transcript::item::{
    ToolTiming, TranscriptItem, TranscriptItemKind, permission_request_item,
};

fn numbered(prefix: &str, count: usize) -> String {
    (1..=count)
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn finished_tool(
    call: &str,
    tool: &str,
    arguments: &str,
    result: String,
    is_error: bool,
    duration_ms: u64,
) -> TranscriptItem {
    TranscriptItem::with_kind(
        if is_error { "Tool error" } else { "Tool" },
        result.clone(),
        false,
        TranscriptItemKind::ToolResult {
            tool_call_id: call.to_owned(),
            tool_name: Some(tool.to_owned()),
            arguments_json: Some(arguments.to_owned()),
            working_directory: None,
            result,
            artifact: None,
            is_error,
            timing: ToolTiming {
                duration_ms: Some(duration_ms),
                ..ToolTiming::default()
            },
        },
    )
}

/// Turn of the sample's earlier exchange.
const EARLIER_TURN: &str = "sample-earlier";
/// Turn of the sample's latest exchange.
const LATEST_TURN: &str = "sample-latest";

/// An item of `kind` positioned in `turn`, as the session view projects it,
/// so turn-recency rules (`turn`, `last_in_turn`) apply to the sample.
fn positioned(
    turn: &str,
    position: u64,
    kind: bcode_session_view_models::TranscriptViewItemKind,
) -> TranscriptItem {
    bcode_tui_components::transcript::item::terminal_item_from_shared(
        &bcode_session_view_models::TranscriptViewItem {
            id: bcode_session_view_models::TranscriptViewItemId::new(format!(
                "profile-sample:{turn}:{position}"
            )),
            revision: 1,
            sequence: None,
            timestamp_ms: None,
            output_location: Some(bcode_session_view_models::TurnOutputLocation {
                turn_id: turn.to_owned(),
                position: bcode_session_models::TurnOutputPosition::new(position),
            }),
            streaming: false,
            kind,
        },
    )
}

/// An earlier exchange (a question, a finished lookup, and the answer) so
/// profiles that fold previous turns show the difference.
fn earlier_turn() -> Vec<TranscriptItem> {
    use bcode_session_view_models::{ChatMessageView, ToolInvocationView, TranscriptViewItemKind};
    vec![
        TranscriptItem::new("You", "Where is the picker drawn?".to_owned()),
        positioned(
            EARLIER_TURN,
            0,
            TranscriptViewItemKind::ToolInvocation {
                tool: Box::new(ToolInvocationView {
                    tool_call_id: "sample-earlier-glob".to_owned(),
                    producer_plugin_id: None,
                    tool_name: Some("glob".to_owned()),
                    arguments_json: Some(r#"{"pattern":"src/**/picker*.rs"}"#.to_owned()),
                    working_directory: None,
                    request_draft: None,
                    status: ToolInvocationViewStatus::Finished,
                    result_text: Some("src/picker.rs\nsrc/picker_render.rs".to_owned()),
                    is_error: Some(false),
                    result: None,
                    presentation: None,
                    timing: bcode_session_view_models::ToolTimingView {
                        duration_ms: Some(9),
                        ..bcode_session_view_models::ToolTimingView::default()
                    },
                }),
            },
        ),
        positioned(
            EARLIER_TURN,
            1,
            TranscriptViewItemKind::AssistantMessage {
                message: ChatMessageView::markdown("In `src/picker_render.rs`."),
            },
        ),
    ]
}

/// A short, representative conversation: an earlier exchange, then a latest
/// turn with messages plus tools in every state a profile may treat
/// differently (small and large succeeded, failed, running, and awaiting a
/// decision). Consecutive lookups (two `read`s then a `grep`) let grouping
/// profiles show both same-tool and consecutive groups.
#[must_use]
pub fn sample_transcript() -> Vec<TranscriptItem> {
    let mut items = earlier_turn();
    items.extend(latest_turn());
    items
}

fn latest_turn() -> Vec<TranscriptItem> {
    vec![
        TranscriptItem::new(
            "You",
            "Why does the picker lose its selection after a resize?".to_owned(),
        ),
        finished_tool(
            "sample-read",
            "read",
            r#"{"path":"src/picker.rs"}"#,
            numbered("  let row = ", 14),
            false,
            42,
        ),
        finished_tool(
            "sample-read-render",
            "read",
            r#"{"path":"src/render.rs"}"#,
            numbered("  let offset = ", 3),
            false,
            27,
        ),
        finished_tool(
            "sample-grep",
            "grep",
            r#"{"pattern":"list_offset"}"#,
            "src/picker.rs:88\nsrc/render.rs:412\nsrc/mouse.rs:57".to_owned(),
            false,
            18,
        ),
        positioned(
            LATEST_TURN,
            0,
            bcode_session_view_models::TranscriptViewItemKind::AssistantMessage {
                message: bcode_session_view_models::ChatMessageView::markdown(
                    "The offset is recomputed before layout, so hit rows lag one frame. \
                     I'll move it after measurement.",
                ),
            },
        ),
        finished_tool(
            "sample-check",
            "bash",
            r#"{"command":"cargo check"}"#,
            "error[E0599]: no method named `offset` found\n  --> src/picker.rs:91:18\nerror: could not compile"
                .to_owned(),
            true,
            2_310,
        ),
        TranscriptItem::with_kind(
            "Tool",
            r#"{"command":"cargo test picker"}"#.to_owned(),
            false,
            TranscriptItemKind::ToolRequest {
                tool_call_id: "sample-test".to_owned(),
                producer_plugin_id: None,
                tool_name: "bash".to_owned(),
                working_directory: None,
                active: true,
                status: Some(ToolInvocationViewStatus::Running),
                timing: ToolTiming::default(),
            },
        ),
        permission_request_item(
            "sample-permission",
            "sample-edit",
            "edit",
            r#"{"path":"src/picker.rs","old":"offset()","new":"list_offset()"}"#,
            None,
            None,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_lookups_group_under_grouping_profiles() {
        use bcode_presentation_policy::models::{LayerSource, PresentationSection, RuleLayer};
        let sample = sample_transcript();
        let items: Vec<&TranscriptItem> = sample.iter().collect();
        for (by, members) in [("same_tool", 2), ("consecutive", 3)] {
            let mut state = super::super::structure::StructureState::default();
            state.set_layers(&[RuleLayer {
                source: LayerSource::Theme("test".into()),
                rules: toml::from_str::<PresentationSection>(&format!(
                    "[[rules]]\nmatch = {{ kind = \"tool\", status = \"succeeded\" }}\nset = {{ group = {{ by = \"{by}\" }} }}"
                ))
                .unwrap()
                .rules,
            }]);
            let snapshot = state.snapshot().with_groups_for(&items);
            let first_read = sample
                .iter()
                .find(|item| {
                    super::super::structure::unit_facts(item)
                        .is_some_and(|facts| facts.tool.as_deref() == Some("read"))
                })
                .unwrap();
            let view = snapshot
                .group_view(first_read.id().get())
                .unwrap_or_else(|| panic!("{by} groups the sample lookups"));
            assert_eq!(view.members.len(), members, "{by}");
        }
    }

    #[test]
    fn sample_spans_an_earlier_turn() {
        use bcode_presentation_policy::models::{TurnRecency, UnitKind};
        let sample = sample_transcript();
        let snapshot = super::super::structure::StructureSnapshot::default().with_turns(
            super::super::structure::TurnIndex::from_items(sample.iter()),
        );
        let facts: Vec<_> = sample
            .iter()
            .map(|item| snapshot.facts(item).unwrap())
            .collect();
        let earlier_tool = facts
            .iter()
            .find(|facts| facts.tool.as_deref() == Some("glob"))
            .unwrap();
        assert_eq!(earlier_tool.kind, Some(UnitKind::Tool));
        assert_eq!(earlier_tool.turn, TurnRecency::Previous);
        assert_eq!(earlier_tool.target.as_deref(), Some("src/**/picker*.rs"));
        assert!(
            facts
                .iter()
                .any(|facts| facts.kind == Some(UnitKind::AssistantMessage)
                    && facts.turn == TurnRecency::Previous
                    && facts.last_in_turn == Some(true))
        );
        assert!(facts.iter().any(
            |facts| facts.tool.as_deref() == Some("read") && facts.turn == TurnRecency::Latest
        ));
    }

    #[test]
    fn sample_covers_every_structural_state() {
        use bcode_presentation_policy::models::UnitStatus;
        let statuses = sample_transcript()
            .iter()
            .filter_map(|item| super::super::structure::unit_facts(item)?.status)
            .collect::<std::collections::BTreeSet<_>>();
        for status in [
            UnitStatus::Succeeded,
            UnitStatus::Failed,
            UnitStatus::Running,
            UnitStatus::Waiting,
        ] {
            assert!(
                statuses.contains(&status),
                "{status:?} missing: {statuses:?}"
            );
        }
    }
}
