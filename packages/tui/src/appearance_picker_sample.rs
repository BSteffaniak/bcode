//! Sample transcript rendered by the appearance picker's preview pane and the
//! generated theme gallery.
//!
//! The sample is ordinary transcript content (`SessionView` items), so it
//! renders through the transcript engine's unit producers
//! ([`bcode_tui_transcript::sample::SampleLayout`]) under an explicit theme and
//! structure configuration, and shows exactly what a palette and profile do to
//! tools and messages.

use std::sync::Arc;

use bcode_presentation_policy::PolicyRegistry;
use bcode_session_models::TurnOutputPosition;
use bcode_session_view_models::{
    ChatMessageView, PermissionView, ToolInvocationView, ToolInvocationViewStatus, ToolResultView,
    ToolTimingView, TranscriptViewItem, TranscriptViewItemId, TranscriptViewItemKind,
    TurnOutputLocation,
};
use bcode_tui_theme::definition::ResolvedThemeDefinition;
use bcode_tui_transcript::layout::RenderConfig;
use bcode_tui_transcript::sample::SampleLayout;
use bcode_tui_transcript::units::structure::{PolicyLayers, StructureConfig};

/// Turn of the sample's earlier exchange.
const EARLIER_TURN: &str = "sample-earlier";
/// Turn of the sample's latest exchange.
const LATEST_TURN: &str = "sample-latest";

fn numbered(prefix: &str, count: usize) -> String {
    (1..=count)
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Lifecycle of one sample tool.
enum ToolState {
    /// Finished with the text after the given milliseconds.
    Succeeded(String, u64),
    /// Failed with the text after the given milliseconds.
    Failed(String, u64),
    /// Still running.
    Running,
}

/// Builds the sample document in order, assigning sequences and turn
/// positions the way the session view projects them: user messages start a
/// turn and carry no output location; every other item is the next output of
/// the current turn.
struct SampleBuilder {
    items: Vec<TranscriptViewItem>,
    turn: &'static str,
    position: u64,
}

impl SampleBuilder {
    const fn new() -> Self {
        Self {
            items: Vec::new(),
            turn: EARLIER_TURN,
            position: 0,
        }
    }

    fn push(&mut self, name: &str, kind: TranscriptViewItemKind) {
        let output_location = if matches!(kind, TranscriptViewItemKind::UserMessage { .. }) {
            None
        } else {
            let location = TurnOutputLocation {
                turn_id: self.turn.to_owned(),
                position: TurnOutputPosition::new(self.position),
            };
            self.position += 1;
            Some(location)
        };
        let sequence = u64::try_from(self.items.len()).unwrap_or(u64::MAX);
        self.items.push(TranscriptViewItem {
            id: TranscriptViewItemId::new(format!("profile-sample:{name}")),
            revision: 1,
            sequence: Some(sequence),
            timestamp_ms: None,
            output_location,
            streaming: false,
            kind,
        });
    }

    fn user(&mut self, turn: &'static str, text: &str) {
        self.turn = turn;
        self.position = 0;
        self.push(
            &format!("{turn}:user"),
            TranscriptViewItemKind::UserMessage {
                message: ChatMessageView::markdown(text),
            },
        );
    }

    fn assistant(&mut self, name: &str, text: &str) {
        self.push(
            name,
            TranscriptViewItemKind::AssistantMessage {
                message: ChatMessageView::markdown(text),
            },
        );
    }

    fn tool(&mut self, call: &str, tool: &str, arguments: &str, state: ToolState) {
        let (status, finished) = match state {
            ToolState::Succeeded(text, ms) => {
                (ToolInvocationViewStatus::Finished, Some((text, false, ms)))
            }
            ToolState::Failed(text, ms) => {
                (ToolInvocationViewStatus::Failed, Some((text, true, ms)))
            }
            ToolState::Running => (ToolInvocationViewStatus::Running, None),
        };
        let (result, is_error, duration_ms) = match finished {
            Some((text, is_error, ms)) => (
                Some(ToolResultView::Text { text }),
                Some(is_error),
                Some(ms),
            ),
            None => (None, None, None),
        };
        self.push(
            call,
            TranscriptViewItemKind::ToolInvocation {
                tool: Box::new(ToolInvocationView {
                    tool_call_id: call.to_owned(),
                    producer_plugin_id: None,
                    tool_name: Some(tool.to_owned()),
                    arguments_json: Some(arguments.to_owned()),
                    working_directory: None,
                    request_draft: None,
                    status,
                    result_text: None,
                    is_error,
                    result,
                    presentation: None,
                    timing: ToolTimingView {
                        duration_ms,
                        ..ToolTimingView::default()
                    },
                }),
            },
        );
    }

    fn permission(&mut self, call: &str, tool: &str, arguments: &str) {
        self.push(
            &format!("{call}:permission"),
            TranscriptViewItemKind::Permission {
                permission: PermissionView {
                    permission_id: format!("{call}-permission"),
                    session_id: None,
                    tool_call_id: call.to_owned(),
                    tool_name: tool.to_owned(),
                    producer_plugin_id: None,
                    arguments_json: arguments.to_owned(),
                    batch: None,
                    agent_id: String::new(),
                    title: None,
                    policy_source: None,
                    detail: None,
                    resolved: false,
                    approved: None,
                    can_remember: false,
                },
            },
        );
    }
}

/// A short, representative conversation: an earlier exchange (a question, a
/// finished lookup and the answer) so profiles that fold previous turns show
/// the difference, then a latest turn with messages plus tools in every state
/// a profile may treat differently (small and large succeeded, failed,
/// running, and awaiting a decision). Consecutive lookups (two `read`s then a
/// `grep`) let grouping profiles show both same-tool and consecutive groups.
#[must_use]
pub fn sample_transcript() -> Vec<TranscriptViewItem> {
    let mut sample = SampleBuilder::new();
    sample.user(EARLIER_TURN, "Where is the picker drawn?");
    sample.tool(
        "sample-earlier-glob",
        "glob",
        r#"{"pattern":"src/**/picker*.rs"}"#,
        ToolState::Succeeded("src/picker.rs\nsrc/picker_render.rs".to_owned(), 9),
    );
    sample.assistant("sample-earlier-answer", "In `src/picker_render.rs`.");

    sample.user(
        LATEST_TURN,
        "Why does the picker lose its selection after a resize?",
    );
    sample.tool(
        "sample-read",
        "read",
        r#"{"path":"src/picker.rs"}"#,
        ToolState::Succeeded(numbered("  let row = ", 14), 42),
    );
    sample.tool(
        "sample-read-render",
        "read",
        r#"{"path":"src/render.rs"}"#,
        ToolState::Succeeded(numbered("  let offset = ", 3), 27),
    );
    sample.tool(
        "sample-grep",
        "grep",
        r#"{"pattern":"list_offset"}"#,
        ToolState::Succeeded(
            "src/picker.rs:88\nsrc/render.rs:412\nsrc/mouse.rs:57".to_owned(),
            18,
        ),
    );
    sample.assistant(
        "sample-latest-answer",
        "The offset is recomputed before layout, so hit rows lag one frame. \
         I'll move it after measurement.",
    );
    sample.tool(
        "sample-check",
        "bash",
        r#"{"command":"cargo check"}"#,
        ToolState::Failed(
            "error[E0599]: no method named `offset` found\n  --> src/picker.rs:91:18\nerror: could not compile"
                .to_owned(),
            2_310,
        ),
    );
    sample.tool(
        "sample-test",
        "bash",
        r#"{"command":"cargo test picker"}"#,
        ToolState::Running,
    );
    sample.permission(
        "sample-edit",
        "edit",
        r#"{"path":"src/picker.rs","old":"offset()","new":"list_offset()"}"#,
    );
    sample.items
}

/// Structure configuration of a theme on its own: its chrome density and
/// `[presentation]` layers over the host defaults, with no configuration,
/// user-state or plugin layers (the generated gallery).
#[must_use]
pub fn theme_structure_config(theme: &ResolvedThemeDefinition) -> StructureConfig {
    PolicyLayers {
        theme: Some(theme),
        ..PolicyLayers::default()
    }
    .compile(&PolicyRegistry::with_host_parts())
    .0
}

/// Lay the sample out at `width` under `theme` and `structure`.
#[must_use]
pub fn sample_layout(
    theme: Arc<ResolvedThemeDefinition>,
    structure: StructureConfig,
    width: u16,
) -> SampleLayout {
    SampleLayout::new(
        &sample_transcript(),
        structure,
        &RenderConfig::for_theme(theme),
        width,
    )
}

/// Preview inputs: the theme the preview renders with and its structure.
#[derive(Clone)]
struct PreviewInputs {
    theme: Arc<ResolvedThemeDefinition>,
    structure: StructureConfig,
    /// Identity of the inputs: theme id and fingerprint plus the structure key.
    key: (String, u64, u64),
}

/// The appearance picker's preview: the sample laid out under the previewed
/// palette and live profile, retained per width.
///
/// Inputs are refreshed outside drawing whenever the picker's selection may
/// have changed ([`Self::refresh`]); the layout is computed once per inputs
/// and width and reused by every frame.
#[derive(Clone, Default)]
pub struct SamplePreview {
    inputs: Option<PreviewInputs>,
    layout: Option<Arc<SampleLayout>>,
}

impl std::fmt::Debug for SamplePreview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SamplePreview")
            .field("inputs", &self.inputs.as_ref().map(|inputs| &inputs.key))
            .field("layout", &self.layout)
            .finish()
    }
}

impl SamplePreview {
    /// Use `theme` and `structure`; returns whether the inputs changed (a
    /// changed input drops the retained layout).
    pub fn set_inputs(
        &mut self,
        theme: Arc<ResolvedThemeDefinition>,
        structure: StructureConfig,
    ) -> bool {
        let key = (theme.id.clone(), theme.fingerprint, structure.key());
        if self.inputs.as_ref().is_some_and(|inputs| inputs.key == key) {
            return false;
        }
        self.inputs = Some(PreviewInputs {
            theme,
            structure,
            key,
        });
        self.layout = None;
        true
    }

    /// Resolve `palette` under `app`'s live profile overlay and structure
    /// configuration and use them. Keeps the previous inputs (and returns
    /// `false`) when the palette does not resolve.
    pub fn refresh(&mut self, app: &mut crate::app::BmuxApp, palette: &str) -> bool {
        let Some(theme) = app.resolve_theme_selection_cached(palette) else {
            return false;
        };
        let structure = crate::transcript_v2_host::render_config::app_structure_config(app, &theme);
        self.set_inputs(Arc::new(theme), structure)
    }

    /// The sample laid out at `width`, laying it out when the width or the
    /// inputs changed; `None` before any inputs were set.
    pub fn layout(&mut self, width: u16) -> Option<&SampleLayout> {
        let inputs = self.inputs.as_ref()?;
        if self
            .layout
            .as_ref()
            .is_none_or(|layout| layout.width() != width)
        {
            self.layout = Some(Arc::new(sample_layout(
                Arc::clone(&inputs.theme),
                inputs.structure.clone(),
                width,
            )));
        }
        self.layout.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use bcode_presentation_policy::CompiledPolicy;
    use bcode_presentation_policy::models::{
        LayerSource, PresentationSection, RuleLayer, TurnRecency, UnitKind, UnitStatus,
    };
    use bcode_session_view_models::TranscriptChanges;
    use bcode_tui_transcript::units::UnitKey;
    use bcode_tui_transcript::units::structure::{ResolvedUnitPresentation, StructureResolver};

    use super::*;

    fn resolved(config: StructureConfig) -> (Vec<TranscriptViewItem>, StructureResolver) {
        let sample = sample_transcript();
        let mut resolver = StructureResolver::new(config);
        let _ = resolver.update(&TranscriptChanges::default(), &sample);
        (sample, resolver)
    }

    fn first_tool(
        sample: &[TranscriptViewItem],
        resolver: &StructureResolver,
        tool: &str,
    ) -> TranscriptViewItemId {
        sample
            .iter()
            .find(|item| {
                resolver
                    .item_facts(&item.id)
                    .is_some_and(|facts| facts.tool.as_deref() == Some(tool))
            })
            .map(|item| item.id.clone())
            .unwrap()
    }

    #[test]
    fn sample_lookups_group_under_grouping_profiles() {
        for (by, members) in [("same_tool", 2), ("consecutive", 3)] {
            let rules = toml::from_str::<PresentationSection>(&format!(
                "[[rules]]\nmatch = {{ kind = \"tool\", status = \"succeeded\" }}\nset = {{ group = {{ by = \"{by}\" }} }}"
            ))
            .unwrap()
            .rules;
            let (policy, diagnostics) = CompiledPolicy::compile(
                &[RuleLayer {
                    source: LayerSource::Theme("test".into()),
                    rules,
                }],
                &PolicyRegistry::with_host_parts(),
            );
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
            let (sample, resolver) = resolved(StructureConfig {
                policy: Arc::new(policy),
                ..StructureConfig::default()
            });
            let first_read = first_tool(&sample, &resolver, "read");
            let ResolvedUnitPresentation::Group(group) =
                resolver.unit_presentation(&UnitKey::GroupHeader(first_read))
            else {
                panic!("{by} groups the sample lookups");
            };
            assert_eq!(group.members.len(), members, "{by}");
        }
    }

    #[test]
    fn sample_spans_an_earlier_turn() {
        let (sample, resolver) = resolved(StructureConfig::default());
        let facts: Vec<_> = sample
            .iter()
            .map(|item| resolver.item_facts(&item.id).unwrap())
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
        let (sample, resolver) = resolved(StructureConfig::default());
        let statuses = sample
            .iter()
            .filter_map(|item| resolver.item_facts(&item.id)?.status)
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

    #[test]
    fn preview_lays_out_once_per_inputs_and_width() {
        let theme = Arc::new(ResolvedThemeDefinition::empty("plain", "Plain"));
        let mut preview = SamplePreview::default();
        assert!(preview.layout(40).is_none(), "no inputs yet");
        assert!(preview.set_inputs(Arc::clone(&theme), StructureConfig::default()));
        let first = preview.layout(40).map(std::ptr::from_ref);
        assert_eq!(
            preview.layout(40).map(std::ptr::from_ref),
            first,
            "the same width reuses the retained layout"
        );
        assert!(
            !preview.set_inputs(Arc::clone(&theme), StructureConfig::default()),
            "equal inputs keep the layout"
        );
        assert_eq!(preview.layout(40).map(std::ptr::from_ref), first);
        let expected = sample_layout(theme, StructureConfig::default(), 20).height();
        assert_eq!(preview.layout(20).map(SampleLayout::height), Some(expected));
        assert_eq!(preview.layout(20).map(SampleLayout::width), Some(20));
    }
}
