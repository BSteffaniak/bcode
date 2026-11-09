//! Renderer-neutral facts of transcript items (U5).
//!
//! [`item_facts`] maps one `TranscriptViewItem` to the presentation policy's [`UnitFacts`]
//! (kind, tool, producer, schema, status, error, decision state, line bucket, target and
//! duration). Neighbour-dependent facts (`turn`, `last_in_turn`) come from [`TurnFacts`];
//! plugin-declared facts come from a host-supplied [`PluginFactSource`]. Everything here is a
//! pure function of explicit inputs.

use std::collections::BTreeMap;

use bcode_presentation_policy::models::{
    DurationBucket, FactValue, SizeBucket, TurnRecency, UnitFacts, UnitKind, UnitStatus,
};
use bcode_session_models::{ReasoningActivityStatus, ToolContributionPlacement};
use bcode_session_view_models::{
    ToolInvocationView, ToolInvocationViewStatus, TranscriptViewItem, TranscriptViewItemId,
    TranscriptViewItemKind,
};

/// Host argument fields read, in order, for the `target` fact.
pub const HOST_TARGET_FIELDS: [&str; 5] = ["path", "command", "pattern", "query", "url"];
/// Largest arguments document scanned for a target; larger ones have none.
const MAX_TARGET_ARGUMENT_BYTES: usize = 1024 * 1024;
/// Largest target fact, in bytes; longer targets are cut on a character boundary.
const MAX_TARGET_BYTES: usize = 256;
/// Most items scanned from the end when locating the latest turns.
pub const MAX_TURN_SCAN: usize = 512;

/// Producer-declared target argument fields: tool name, then declaring plugin id, then
/// ordered argument field names. Declared fields are tried before [`HOST_TARGET_FIELDS`].
pub type DeclaredTargetFields = BTreeMap<String, BTreeMap<String, Vec<String>>>;

/// Plugin-declared facts for the units a plugin produced (supplied by the host).
///
/// Implementations are pure: the same item revision and generation yield the same facts.
pub trait PluginFactSource {
    /// Revision of the declarations; a change re-resolves every item.
    fn generation(&self) -> u64;

    /// Facts `producer` declares for `item` (only facts declared by that producer).
    fn facts(
        &self,
        item: &TranscriptViewItem,
        producer: &str,
        tool: Option<&str>,
    ) -> BTreeMap<String, FactValue>;
}

/// No plugin facts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoPluginFacts;

impl PluginFactSource for NoPluginFacts {
    fn generation(&self) -> u64 {
        0
    }

    fn facts(
        &self,
        _item: &TranscriptViewItem,
        _producer: &str,
        _tool: Option<&str>,
    ) -> BTreeMap<String, FactValue> {
        BTreeMap::new()
    }
}

const fn unit_status(status: ToolInvocationViewStatus) -> UnitStatus {
    match status {
        ToolInvocationViewStatus::Requested => UnitStatus::Requested,
        ToolInvocationViewStatus::Running => UnitStatus::Running,
        ToolInvocationViewStatus::Waiting => UnitStatus::Waiting,
        ToolInvocationViewStatus::Finished => UnitStatus::Succeeded,
        ToolInvocationViewStatus::Failed => UnitStatus::Failed,
        ToolInvocationViewStatus::Cancelled => UnitStatus::Cancelled,
    }
}

const fn lifecycle(streaming: bool) -> UnitStatus {
    if streaming {
        UnitStatus::Running
    } else {
        UnitStatus::Succeeded
    }
}

/// Whether a tool invocation is still active (no terminal status).
const fn tool_active(tool: &ToolInvocationView) -> bool {
    matches!(
        tool.status,
        ToolInvocationViewStatus::Requested
            | ToolInvocationViewStatus::Running
            | ToolInvocationViewStatus::Waiting
    )
}

/// Finished duration of a tool invocation in milliseconds, when known.
#[must_use]
pub fn tool_duration_ms(tool: &ToolInvocationView) -> Option<u64> {
    if tool_active(tool) {
        return None;
    }
    let timing = &tool.timing;
    timing
        .duration_ms
        .or_else(|| Some(timing.finished_at_ms?.saturating_sub(timing.started_at_ms?)))
}

fn tool_facts(tool: &ToolInvocationView) -> UnitFacts {
    let failed = tool.status == ToolInvocationViewStatus::Failed || tool.is_error == Some(true);
    UnitFacts {
        kind: Some(UnitKind::Tool),
        tool: tool.tool_name.clone(),
        producer: tool.producer_plugin_id.clone(),
        status: Some(if tool.timing.timed_out == Some(true) {
            UnitStatus::TimedOut
        } else if failed && !tool_active(tool) {
            UnitStatus::Failed
        } else {
            unit_status(tool.status)
        }),
        is_error: failed,
        duration: tool_duration_ms(tool).map(DurationBucket::of_millis),
        ..UnitFacts::default()
    }
}

/// Primary text of an item, used for the line bucket.
fn primary_text(item: &TranscriptViewItem) -> Option<std::borrow::Cow<'_, str>> {
    use std::borrow::Cow;
    match &item.kind {
        TranscriptViewItemKind::UserMessage { message }
        | TranscriptViewItemKind::AssistantMessage { message }
        | TranscriptViewItemKind::ReasoningMessage { message }
        | TranscriptViewItemKind::SystemMessage { message } => Some(Cow::Borrowed(&message.text)),
        TranscriptViewItemKind::ReasoningActivity { activity } => Some(Cow::Owned(activity.text())),
        TranscriptViewItemKind::ToolInvocation { tool }
        | TranscriptViewItemKind::ToolRequest { tool } => {
            tool.result_text.as_deref().map(Cow::Borrowed)
        }
        TranscriptViewItemKind::ToolRequestDraft { draft } => Some(Cow::Borrowed(&draft.preview)),
        TranscriptViewItemKind::Compaction { compaction } => Some(Cow::Borrowed(&compaction.text)),
        TranscriptViewItemKind::Skill { skill } => Some(Cow::Borrowed(&skill.text)),
        TranscriptViewItemKind::Permission { permission } => {
            permission.detail.as_deref().map(Cow::Borrowed)
        }
        TranscriptViewItemKind::Interaction { .. }
        | TranscriptViewItemKind::ToolContribution { .. } => None,
    }
}

/// Tool arguments of an item, when it has any.
#[must_use]
pub fn tool_arguments(item: &TranscriptViewItem) -> Option<&str> {
    match &item.kind {
        TranscriptViewItemKind::ToolInvocation { tool }
        | TranscriptViewItemKind::ToolRequest { tool }
        | TranscriptViewItemKind::ToolContribution {
            invocation: Some(tool),
            ..
        } => tool.arguments_json.as_deref(),
        TranscriptViewItemKind::Permission { permission } => Some(&permission.arguments_json),
        _ => None,
    }
}

/// Tool invocation an item is or belongs to, when known (grouping identity of a call).
#[must_use]
pub fn invocation_id(item: &TranscriptViewItem) -> Option<&str> {
    match &item.kind {
        TranscriptViewItemKind::ToolInvocation { tool }
        | TranscriptViewItemKind::ToolRequest { tool } => Some(&tool.tool_call_id),
        TranscriptViewItemKind::Permission { permission } => Some(&permission.tool_call_id),
        TranscriptViewItemKind::ToolContribution { contribution, .. } => {
            Some(&contribution.invocation_id)
        }
        TranscriptViewItemKind::ToolRequestDraft { draft } => Some(&draft.tool_call_id),
        _ => None,
    }
}

/// Whether an item is a tool contribution attached to an invocation rather than the
/// presentation of the call itself (supplemental and hidden placements).
#[must_use]
pub const fn is_attached_contribution(item: &TranscriptViewItem) -> bool {
    matches!(
        item.kind,
        TranscriptViewItemKind::ToolContribution {
            placement: ToolContributionPlacement::Supplemental | ToolContributionPlacement::Hidden,
            ..
        }
    )
}

/// Whether an item is a historical tool request (superseded when a later item represents its
/// invocation).
#[must_use]
pub const fn is_historical_request(item: &TranscriptViewItem) -> bool {
    matches!(item.kind, TranscriptViewItemKind::ToolRequest { .. })
}

/// First non-empty value of `fields` (in field order) that is a string in the arguments
/// object, bounded to one line of at most 256 bytes.
#[must_use]
pub fn extract_target<'f>(
    arguments: &str,
    fields: impl IntoIterator<Item = &'f str>,
) -> Option<String> {
    if arguments.len() > MAX_TARGET_ARGUMENT_BYTES {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(arguments).ok()?;
    let object = value.as_object()?;
    fields.into_iter().find_map(|field| {
        let line = object.get(field)?.as_str()?.lines().next()?.trim();
        if line.is_empty() {
            return None;
        }
        let mut end = line.len().min(MAX_TARGET_BYTES);
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        Some(line[..end].to_owned())
    })
}

/// Facts of one item that depend on the item alone: kind, tool, producer, schema, status,
/// error and decision state, line bucket, host target and duration.
///
/// System messages labelled `Plugin` are plugin status notes; system messages that carry
/// activity are activity units.
#[must_use]
#[allow(clippy::too_many_lines, reason = "one exhaustive arm per item kind")]
pub fn item_facts(item: &TranscriptViewItem) -> UnitFacts {
    let mut facts = match &item.kind {
        TranscriptViewItemKind::UserMessage { .. } => UnitFacts {
            kind: Some(UnitKind::UserMessage),
            ..UnitFacts::default()
        },
        TranscriptViewItemKind::AssistantMessage { .. } => UnitFacts {
            kind: Some(UnitKind::AssistantMessage),
            status: Some(lifecycle(item.streaming)),
            ..UnitFacts::default()
        },
        TranscriptViewItemKind::ReasoningMessage { .. } => UnitFacts {
            kind: Some(UnitKind::Reasoning),
            status: Some(lifecycle(item.streaming)),
            ..UnitFacts::default()
        },
        TranscriptViewItemKind::ReasoningActivity { activity } => UnitFacts {
            kind: Some(UnitKind::Reasoning),
            status: Some(match activity.status {
                ReasoningActivityStatus::Failed => UnitStatus::Failed,
                ReasoningActivityStatus::Interrupted => UnitStatus::Cancelled,
                ReasoningActivityStatus::Completed if activity.finished => UnitStatus::Succeeded,
                ReasoningActivityStatus::Completed => UnitStatus::Running,
            }),
            ..UnitFacts::default()
        },
        TranscriptViewItemKind::ToolInvocation { tool }
        | TranscriptViewItemKind::ToolRequest { tool } => tool_facts(tool),
        TranscriptViewItemKind::ToolRequestDraft { draft } => UnitFacts {
            kind: Some(UnitKind::Tool),
            tool: Some(draft.tool_name.clone()),
            producer: draft.producer_plugin_id.clone(),
            schema: Some(format!("{}/{}", draft.schema, draft.schema_version)),
            status: Some(UnitStatus::Requested),
            ..UnitFacts::default()
        },
        TranscriptViewItemKind::ToolContribution {
            contribution,
            invocation,
            ..
        } => {
            let invocation = invocation.as_deref();
            UnitFacts {
                kind: Some(UnitKind::Tool),
                tool: invocation.and_then(|tool| tool.tool_name.clone()),
                producer: invocation
                    .and_then(|tool| tool.producer_plugin_id.clone())
                    .or_else(|| Some(contribution.producer_id.clone())),
                schema: Some(format!(
                    "{}/{}",
                    contribution.schema, contribution.schema_version
                )),
                status: invocation.map(|tool| unit_status(tool.status)),
                is_error: invocation.and_then(|tool| tool.is_error).unwrap_or(false),
                ..UnitFacts::default()
            }
        }
        TranscriptViewItemKind::Permission { permission } => UnitFacts {
            kind: Some(UnitKind::Permission),
            tool: Some(permission.tool_name.clone()).filter(|name| !name.is_empty()),
            producer: permission.producer_plugin_id.clone(),
            awaiting_decision: !permission.resolved,
            status: Some(match (permission.resolved, permission.approved) {
                (false, _) => UnitStatus::Waiting,
                (true, Some(false)) => UnitStatus::Cancelled,
                (true, _) => UnitStatus::Succeeded,
            }),
            ..UnitFacts::default()
        },
        TranscriptViewItemKind::Compaction { .. } => UnitFacts {
            kind: Some(UnitKind::Compaction),
            ..UnitFacts::default()
        },
        TranscriptViewItemKind::Interaction { interaction } => UnitFacts {
            kind: Some(UnitKind::Interaction),
            producer: interaction.producer_id.clone(),
            schema: interaction.exchange_schema.as_ref().map(|schema| {
                interaction
                    .exchange_schema_version
                    .map_or_else(|| schema.clone(), |version| format!("{schema}/{version}"))
            }),
            awaiting_decision: !interaction.resolved,
            status: Some(if interaction.resolved {
                UnitStatus::Succeeded
            } else {
                UnitStatus::Waiting
            }),
            ..UnitFacts::default()
        },
        TranscriptViewItemKind::Skill { skill } => {
            let failed = skill.status == bcode_session_view_models::SkillViewStatus::Failed;
            UnitFacts {
                kind: Some(UnitKind::Skill),
                status: failed.then_some(UnitStatus::Failed),
                is_error: failed,
                ..UnitFacts::default()
            }
        }
        TranscriptViewItemKind::SystemMessage { message } => UnitFacts {
            kind: Some(if message.activity.is_some() {
                UnitKind::Activity
            } else if message.display_label.as_deref() == Some(PLUGIN_LABEL) {
                UnitKind::PluginStatus
            } else {
                UnitKind::System
            }),
            ..UnitFacts::default()
        },
    };
    facts.lines = SizeBucket::of(primary_text(item).map_or(0, |text| text.lines().count()));
    facts.target =
        tool_arguments(item).and_then(|arguments| extract_target(arguments, HOST_TARGET_FIELDS));
    facts
}

/// Display label of plugin status system messages.
const PLUGIN_LABEL: &str = "Plugin";

/// The latest two turns, for the `turn` recency fact, and which recent items end their turn,
/// for the `last_in_turn` fact.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnFacts {
    latest: Option<String>,
    previous: Option<String>,
    last_in_turn: BTreeMap<TranscriptViewItemId, bool>,
}

impl TurnFacts {
    /// Scan at most [`MAX_TURN_SCAN`] items from the end. Only items with an output location
    /// take part; an item is last in its turn when the next positioned item belongs to
    /// another turn.
    #[must_use]
    pub fn from_items(items: &[TranscriptViewItem]) -> Self {
        let mut facts = Self::default();
        let mut next_turn: Option<&String> = None;
        for item in items.iter().rev().take(MAX_TURN_SCAN) {
            let Some(turn) = item
                .output_location
                .as_ref()
                .map(|location| &location.turn_id)
            else {
                continue;
            };
            facts
                .last_in_turn
                .insert(item.id.clone(), next_turn != Some(turn));
            next_turn = Some(turn);
            match &facts.latest {
                None => facts.latest = Some(turn.clone()),
                Some(latest) if latest != turn && facts.previous.is_none() => {
                    facts.previous = Some(turn.clone());
                }
                Some(_) => {}
            }
        }
        facts
    }

    /// Recency of an item's turn; items without a turn count as latest.
    #[must_use]
    pub fn recency(&self, item: &TranscriptViewItem) -> TurnRecency {
        let Some(turn) = item
            .output_location
            .as_ref()
            .map(|location| &location.turn_id)
        else {
            return TurnRecency::Latest;
        };
        if self.latest.as_ref() == Some(turn) {
            TurnRecency::Latest
        } else if self.previous.as_ref() == Some(turn) {
            TurnRecency::Previous
        } else {
            TurnRecency::Older
        }
    }

    /// `last_in_turn` for an item, when known.
    #[must_use]
    pub fn last_in_turn(&self, item: &TranscriptViewItem) -> Option<bool> {
        self.last_in_turn.get(&item.id).copied()
    }

    /// Whether the turn recency (what the `turn` fact observes) equals `other`'s.
    #[must_use]
    pub fn same_recency(&self, other: &Self) -> bool {
        self.latest == other.latest && self.previous == other.previous
    }

    /// Whether the `last_in_turn` facts equal `other`'s.
    #[must_use]
    pub fn same_last_in_turn(&self, other: &Self) -> bool {
        self.last_in_turn == other.last_in_turn
    }
}

#[cfg(test)]
mod tests {
    use bcode_session_view_models::{ChatMessageView, PermissionView, ToolTimingView};

    use super::*;

    fn item(kind: TranscriptViewItemKind) -> TranscriptViewItem {
        TranscriptViewItem {
            id: TranscriptViewItemId::new("x"),
            revision: 1,
            sequence: Some(1),
            timestamp_ms: None,
            output_location: None,
            streaming: false,
            kind,
        }
    }

    fn tool(status: ToolInvocationViewStatus, arguments: &str) -> ToolInvocationView {
        ToolInvocationView {
            tool_call_id: "call".to_owned(),
            producer_plugin_id: Some("plugin".to_owned()),
            tool_name: Some("read".to_owned()),
            arguments_json: Some(arguments.to_owned()),
            working_directory: None,
            request_draft: None,
            status,
            result_text: Some("one\ntwo\nthree".to_owned()),
            is_error: None,
            result: None,
            presentation: None,
            timing: ToolTimingView {
                started_at_ms: Some(1_000),
                finished_at_ms: Some(3_500),
                timeout_ms: None,
                timed_out: None,
                duration_ms: None,
            },
        }
    }

    #[test]
    fn tool_facts_carry_tool_status_target_lines_and_duration() {
        let facts = item_facts(&item(TranscriptViewItemKind::ToolInvocation {
            tool: Box::new(tool(
                ToolInvocationViewStatus::Finished,
                r#"{"other": 1, "path": "src/lib.rs\nignored"}"#,
            )),
        }));
        assert_eq!(facts.kind, Some(UnitKind::Tool));
        assert_eq!(facts.tool.as_deref(), Some("read"));
        assert_eq!(facts.producer.as_deref(), Some("plugin"));
        assert_eq!(facts.status, Some(UnitStatus::Succeeded));
        assert_eq!(facts.target.as_deref(), Some("src/lib.rs"));
        assert_eq!(facts.lines, SizeBucket::Few);
        assert_eq!(facts.duration, Some(DurationBucket::OneToTenSeconds));
        assert!(!facts.is_error);
    }

    #[test]
    fn a16_failed_tools_and_pending_permissions_are_flagged() {
        let failed = item_facts(&item(TranscriptViewItemKind::ToolInvocation {
            tool: Box::new(tool(ToolInvocationViewStatus::Failed, "{}")),
        }));
        assert!(failed.is_error);
        assert_eq!(failed.status, Some(UnitStatus::Failed));

        let permission = PermissionView {
            permission_id: "p".to_owned(),
            session_id: None,
            tool_call_id: "call".to_owned(),
            tool_name: "bash".to_owned(),
            producer_plugin_id: None,
            arguments_json: r#"{"command":"ls"}"#.to_owned(),
            batch: None,
            agent_id: String::new(),
            title: None,
            policy_source: None,
            detail: None,
            resolved: false,
            approved: None,
            can_remember: false,
        };
        let pending = item_facts(&item(TranscriptViewItemKind::Permission { permission }));
        assert!(pending.awaiting_decision);
        assert_eq!(pending.target.as_deref(), Some("ls"));
    }

    #[test]
    fn streaming_messages_are_running() {
        let mut message = item(TranscriptViewItemKind::AssistantMessage {
            message: ChatMessageView::markdown("a\nb"),
        });
        message.streaming = true;
        let facts = item_facts(&message);
        assert_eq!(facts.status, Some(UnitStatus::Running));
        assert_eq!(facts.lines, SizeBucket::Few);
    }

    #[test]
    fn oversized_or_invalid_arguments_have_no_target() {
        assert_eq!(extract_target("not json", HOST_TARGET_FIELDS), None);
        assert_eq!(extract_target(r#"{"path": ""}"#, HOST_TARGET_FIELDS), None);
        let long = format!(r#"{{"path": "{}"}}"#, "漢".repeat(200));
        let target = extract_target(&long, HOST_TARGET_FIELDS).expect("target");
        assert!(target.len() <= MAX_TARGET_BYTES);
    }
}
