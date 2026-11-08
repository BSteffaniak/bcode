//! Interactive interface-profile picker state.
//!
//! Moving the selection applies the profile live, so the transcript behind the
//! picker and the picker's own sample transcript both re-render through the
//! normal structural presentation pipeline. Cancelling restores the profile
//! that was active when the picker opened.

use bcode_session_view_models::ToolInvocationViewStatus;
use bcode_tui_components::transcript::item::{
    ToolTiming, TranscriptItem, TranscriptItemKind, permission_request_item,
};
use bmux_keyboard::KeyStroke;
use bmux_tui_components::selectable_list::SelectableListState as ListState;

use super::filtered_list::FilteredListState;
use super::theme::ProfileCatalogEntry;

/// Profile picker outcome for one input event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfilePickerOutcome {
    /// Selection moved; apply the profile without persisting it.
    Preview(Option<String>),
    /// Persist the selected profile (`None` clears it).
    Apply(Option<String>),
    /// Close and restore the profile active when the picker opened.
    Cancel,
    /// Event was ignored.
    Ignored,
}

/// One picker row; `id` is `None` for the theme's own presentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfilePickerEntry {
    /// Profile id, or `None` for no profile overlay.
    pub id: Option<String>,
    /// Display name.
    pub display_name: String,
    /// One-line description.
    pub description: Option<String>,
}

/// Bounded interactive profile picker state.
#[derive(Debug, Clone)]
pub struct ProfilePickerState {
    entries: Vec<ProfilePickerEntry>,
    original: Option<String>,
    sample: Vec<TranscriptItem>,
    list: FilteredListState,
}

impl ProfilePickerState {
    /// Create a picker from catalog rows; `original` is the active profile.
    #[must_use]
    pub fn new(catalog: Vec<ProfileCatalogEntry>, original: Option<String>) -> Self {
        let entries = std::iter::once(ProfilePickerEntry {
            id: None,
            display_name: "Theme default".to_owned(),
            description: Some("No profile overlay: the theme's own chrome and structure.".into()),
        })
        .chain(catalog.into_iter().map(|entry| ProfilePickerEntry {
            id: Some(entry.id),
            display_name: entry.display_name,
            description: entry.description,
        }))
        .collect::<Vec<_>>();
        let selected = entries
            .iter()
            .position(|entry| entry.id == original)
            .unwrap_or(0);
        let mut list = FilteredListState::new(entries.len());
        let _selected = list.select_visible(selected);
        Self {
            entries,
            original,
            sample: sample_transcript(),
            list,
        }
    }

    /// Return picker rows.
    #[must_use]
    pub fn entries(&self) -> &[ProfilePickerEntry] {
        &self.entries
    }

    /// Return the profile active when the picker opened.
    #[must_use]
    pub fn original(&self) -> Option<&str> {
        self.original.as_deref()
    }

    /// Return the sample transcript rendered in the preview pane.
    #[must_use]
    pub fn sample(&self) -> &[TranscriptItem] {
        &self.sample
    }

    /// Synchronize list visibility before rendering and return its render state.
    pub fn list_render_state(&mut self, viewport_height: u16) -> &mut ListState {
        self.list.render_state(viewport_height)
    }

    /// Return the current list offset for hit testing.
    #[must_use]
    pub fn list_offset(&self) -> usize {
        self.list.offset()
    }

    /// Return the selected row.
    #[must_use]
    pub fn selected_entry(&self) -> Option<&ProfilePickerEntry> {
        self.list
            .selected_source_index()
            .and_then(|selected| self.entries.get(selected))
    }

    /// Select one absolute row and preview it.
    pub fn select_row(&mut self, row: usize) -> ProfilePickerOutcome {
        let Some(entry) = self.entries.get(row) else {
            return ProfilePickerOutcome::Ignored;
        };
        let id = entry.id.clone();
        let _selected = self.list.select_visible(row);
        ProfilePickerOutcome::Preview(id)
    }

    /// Activate one absolute row and request persistence.
    pub fn activate_row(&mut self, row: usize) -> ProfilePickerOutcome {
        let Some(entry) = self.entries.get(row) else {
            return ProfilePickerOutcome::Ignored;
        };
        let id = entry.id.clone();
        let _selected = self.list.select_visible(row);
        ProfilePickerOutcome::Apply(id)
    }

    /// Handle one picker key.
    #[cfg(test)]
    pub fn handle_key(&mut self, stroke: KeyStroke) -> ProfilePickerOutcome {
        self.handle_key_with_keymap(
            stroke,
            &super::keymap::BmuxKeyMap::from_config(&bcode_config::TuiConfig::default()),
        )
    }

    /// Handle input using the frontend's effective registry.
    pub fn handle_key_with_keymap(
        &mut self,
        stroke: KeyStroke,
        keymap: &super::keymap::BmuxKeyMap,
    ) -> ProfilePickerOutcome {
        match keymap.action("profile_picker", stroke) {
            Some("tui.control.cancel") => ProfilePickerOutcome::Cancel,
            Some("tui.control.submit") => self
                .selected_entry()
                .map_or(ProfilePickerOutcome::Ignored, |entry| {
                    ProfilePickerOutcome::Apply(entry.id.clone())
                }),
            Some("tui.control.up") => self.move_selection(false),
            Some("tui.control.down") => self.move_selection(true),
            _ => ProfilePickerOutcome::Ignored,
        }
    }

    fn move_selection(&mut self, forward: bool) -> ProfilePickerOutcome {
        if forward {
            self.list.select_next();
        } else {
            self.list.select_previous();
        }
        self.selected_entry()
            .map_or(ProfilePickerOutcome::Ignored, |entry| {
                ProfilePickerOutcome::Preview(entry.id.clone())
            })
    }
}

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
fn sample_transcript() -> Vec<TranscriptItem> {
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
    use bmux_keyboard::KeyCode;

    fn catalog() -> Vec<ProfileCatalogEntry> {
        ["bare", "zen"]
            .into_iter()
            .map(|id| ProfileCatalogEntry {
                id: id.to_owned(),
                display_name: id.to_owned(),
                description: None,
                selected: false,
            })
            .collect()
    }

    #[test]
    fn opens_on_the_active_profile_and_previews_while_moving() {
        let mut picker = ProfilePickerState::new(catalog(), Some("bare".to_owned()));
        assert_eq!(
            picker
                .selected_entry()
                .and_then(|entry| entry.id.as_deref()),
            Some("bare")
        );
        assert_eq!(
            picker.handle_key(KeyStroke::simple(KeyCode::Down)),
            ProfilePickerOutcome::Preview(Some("zen".to_owned()))
        );
        assert_eq!(
            picker.handle_key(KeyStroke::simple(KeyCode::Char('k'))),
            ProfilePickerOutcome::Preview(Some("bare".to_owned()))
        );
        assert_eq!(
            picker.handle_key(KeyStroke::simple(KeyCode::Up)),
            ProfilePickerOutcome::Preview(None)
        );
        assert_eq!(
            picker.handle_key(KeyStroke::simple(KeyCode::Enter)),
            ProfilePickerOutcome::Apply(None)
        );
        assert_eq!(
            picker.handle_key(KeyStroke::simple(KeyCode::Escape)),
            ProfilePickerOutcome::Cancel
        );
        assert_eq!(picker.original(), Some("bare"));
    }

    #[test]
    fn no_active_profile_selects_theme_default() {
        let picker = ProfilePickerState::new(catalog(), None);
        assert_eq!(
            picker.selected_entry().map(|entry| entry.id.clone()),
            Some(None)
        );
        assert_eq!(picker.entries().len(), 3);
    }

    #[test]
    fn mouse_rows_preview_then_activate() {
        let mut picker = ProfilePickerState::new(catalog(), None);
        assert_eq!(
            picker.select_row(2),
            ProfilePickerOutcome::Preview(Some("zen".to_owned()))
        );
        assert_eq!(
            picker.activate_row(2),
            ProfilePickerOutcome::Apply(Some("zen".to_owned()))
        );
        assert_eq!(picker.select_row(9), ProfilePickerOutcome::Ignored);
    }

    #[test]
    fn sample_lookups_group_under_grouping_profiles() {
        use bcode_presentation_policy::models::{LayerSource, PresentationSection, RuleLayer};
        let sample = ProfilePickerState::new(Vec::new(), None).sample;
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
        let sample = ProfilePickerState::new(Vec::new(), None).sample;
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
        let statuses = ProfilePickerState::new(Vec::new(), None)
            .sample()
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
