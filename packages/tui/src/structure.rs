//! Structural presentation for transcript units.
//!
//! The compiled policy comes from theme, configuration, and user-state rule
//! layers; this module adapts terminal transcript items into renderer-neutral
//! [`UnitFacts`] and keeps per-item disclosure overrides as ephemeral renderer
//! state. Overrides are the user's own choices, so they may lower disclosure
//! below the theme floor, except for units awaiting a decision, which always
//! stay expanded.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bcode_presentation_policy::models::{
    Disclosure, LayerSource, ResolvedPresentation, RuleLayer, SizeBucket, TurnRecency, UnitFacts,
    UnitKind, UnitStatus,
};
use bcode_presentation_policy::{CompiledPolicy, PolicyDiagnostic, PolicyRegistry};
use bcode_session_view_models::ToolInvocationViewStatus;
use bcode_tui_components::transcript::item::{TranscriptItem, TranscriptItemKind};

/// Maximum retained per-item overrides; older entries are dropped first.
const MAX_OVERRIDES: usize = 4096;
/// Maximum items scanned from the end when locating the latest turns.
const MAX_TURN_SCAN: usize = 512;

/// Immutable view of structural state used while building transcript rows.
#[derive(Debug, Clone, Default)]
pub struct StructureSnapshot {
    policy: Arc<CompiledPolicy>,
    overrides: Arc<BTreeMap<u64, Disclosure>>,
    turns: TurnIndex,
}

/// The latest two turn identities, for the `turn` recency fact.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnIndex {
    latest: Option<String>,
    previous: Option<String>,
}

impl TurnIndex {
    /// Find the latest two distinct turns by scanning at most
    /// [`MAX_TURN_SCAN`] `items` from the end.
    #[must_use]
    pub fn from_items<'a>(items: impl DoubleEndedIterator<Item = &'a TranscriptItem>) -> Self {
        let mut index = Self::default();
        for item in items.rev().take(MAX_TURN_SCAN) {
            let Some(turn) = item.output_location().map(|location| &location.turn_id) else {
                continue;
            };
            match &index.latest {
                None => index.latest = Some(turn.clone()),
                Some(latest) if latest != turn => {
                    index.previous = Some(turn.clone());
                    break;
                }
                Some(_) => {}
            }
        }
        index
    }

    fn recency(&self, item: &TranscriptItem) -> TurnRecency {
        let Some(turn) = item.output_location().map(|location| &location.turn_id) else {
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

    /// Stable key for layout fingerprints.
    #[must_use]
    pub fn key(&self) -> String {
        format!("{:?}:{:?}", self.latest, self.previous)
    }
}

impl StructureSnapshot {
    /// Effective presentation for `item`, or `None` for non-structural items.
    #[must_use]
    pub fn presentation(&self, item: &TranscriptItem) -> Option<ResolvedPresentation> {
        let mut facts = unit_facts(item)?;
        facts.turn = self.turns.recency(item);
        let mut presentation = self.policy.resolve(&facts);
        if let Some(disclosure) = self.overrides.get(&item.id().get())
            && !facts.awaiting_decision
        {
            presentation.disclosure = *disclosure;
        }
        Some(presentation)
    }

    /// Stable fragment for layout signatures of `item`.
    #[must_use]
    pub fn signature(&self, item: &TranscriptItem) -> String {
        self.presentation(item)
            .map_or_else(String::new, |presentation| {
                format!(
                    "{:?}:{:?}:{}:{:?}:{:?}",
                    presentation.disclosure,
                    presentation.detail,
                    presentation.preview.lines,
                    presentation.preview.keep,
                    presentation.parts
                )
            })
    }
}

/// Mutable structural state owned by the application.
#[derive(Debug, Clone, Default)]
pub struct StructureState {
    snapshot: StructureSnapshot,
    diagnostics: Vec<PolicyDiagnostic>,
    revision: u64,
}

impl StructureState {
    /// Recompile from `layers`; returns whether the effective policy changed.
    pub fn set_layers(&mut self, layers: &[RuleLayer]) -> bool {
        let (policy, diagnostics) =
            CompiledPolicy::compile(layers, &PolicyRegistry::with_host_parts());
        if policy.fingerprint() == self.snapshot.policy.fingerprint() {
            return false;
        }
        for diagnostic in &diagnostics {
            tracing::warn!(
                source = ?diagnostic.source,
                rule = ?diagnostic.rule,
                severity = ?diagnostic.severity,
                message = %diagnostic.message,
                "presentation rule diagnostic"
            );
        }
        self.snapshot.policy = Arc::new(policy);
        self.diagnostics = diagnostics;
        self.revision = self.revision.wrapping_add(1);
        true
    }

    /// Snapshot for row building.
    #[must_use]
    pub fn snapshot(&self) -> StructureSnapshot {
        self.snapshot.clone()
    }

    /// Update turn recency; returns whether it changed in a way the policy observes.
    pub fn set_turns(&mut self, turns: TurnIndex) -> bool {
        if turns == self.snapshot.turns {
            return false;
        }
        self.snapshot.turns = turns;
        self.snapshot.policy.uses_turn_facts()
    }

    /// Layout key for turn recency, empty when the policy ignores turns.
    #[must_use]
    pub fn turn_key(&self) -> String {
        if self.snapshot.policy.uses_turn_facts() {
            self.snapshot.turns.key()
        } else {
            String::new()
        }
    }

    /// Diagnostics from the last compilation.
    #[must_use]
    pub fn diagnostics(&self) -> &[PolicyDiagnostic] {
        &self.diagnostics
    }

    /// Policy fingerprint, for structural layout cache keys.
    #[must_use]
    pub fn policy_fingerprint(&self) -> u64 {
        self.snapshot.policy.fingerprint()
    }

    /// Revision bumped by every override or policy change.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Explain how `item` resolved, when it is a structural unit.
    #[must_use]
    pub fn explain(
        &self,
        item: &TranscriptItem,
    ) -> Option<bcode_presentation_policy::ExplainedPresentation> {
        let facts = unit_facts(item)?;
        let mut explained = self.snapshot.policy.explain(&facts);
        if let Some(disclosure) = self.snapshot.overrides.get(&item.id().get())
            && !facts.awaiting_decision
        {
            explained.presentation.disclosure = *disclosure;
            explained
                .floor
                .push(format!("manual toggle: disclosure set to {disclosure:?}"));
        }
        Some(explained)
    }

    /// Toggle `item` between expanded and collapsed; returns the new disclosure.
    ///
    /// Summarized or hidden items expand first. Units awaiting a decision
    /// cannot be collapsed.
    pub fn toggle(&mut self, item: &TranscriptItem) -> Option<Disclosure> {
        let facts = unit_facts(item)?;
        if facts.awaiting_decision {
            return None;
        }
        let current = self.snapshot.presentation(item)?.disclosure;
        let next = if current == Disclosure::Expanded {
            Disclosure::Collapsed
        } else {
            Disclosure::Expanded
        };
        self.set_override(item.id().get(), next);
        Some(next)
    }

    /// Set every structural item in `items` to `disclosure`; returns changed item indexes.
    pub fn set_all<'a>(
        &mut self,
        items: impl IntoIterator<Item = (usize, &'a TranscriptItem)>,
        disclosure: Disclosure,
    ) -> Vec<usize> {
        let mut changed = Vec::new();
        for (index, item) in items {
            let Some(facts) = unit_facts(item) else {
                continue;
            };
            if facts.awaiting_decision {
                continue;
            }
            if self
                .snapshot
                .presentation(item)
                .is_some_and(|presentation| presentation.disclosure != disclosure)
            {
                self.set_override(item.id().get(), disclosure);
                changed.push(index);
            }
        }
        changed
    }

    /// Drop all manual overrides; returns whether any existed.
    pub fn clear_overrides(&mut self) -> bool {
        if self.snapshot.overrides.is_empty() {
            return false;
        }
        self.snapshot.overrides = Arc::new(BTreeMap::new());
        self.revision = self.revision.wrapping_add(1);
        true
    }

    /// Forget overrides for items no longer resident.
    pub fn reconcile(&mut self, resident: &BTreeSet<u64>) {
        if self
            .snapshot
            .overrides
            .keys()
            .any(|id| !resident.contains(id))
        {
            let overrides = Arc::make_mut(&mut self.snapshot.overrides);
            overrides.retain(|id, _| resident.contains(id));
        }
    }

    fn set_override(&mut self, id: u64, disclosure: Disclosure) {
        let overrides = Arc::make_mut(&mut self.snapshot.overrides);
        overrides.insert(id, disclosure);
        while overrides.len() > MAX_OVERRIDES {
            overrides.pop_first();
        }
        self.revision = self.revision.wrapping_add(1);
    }
}

/// Rule layer persisted from interactive "make this the default" actions.
#[must_use]
pub const fn user_state_layer(
    rules: Vec<bcode_presentation_policy::models::PresentationRule>,
) -> RuleLayer {
    RuleLayer {
        source: LayerSource::UserState,
        rules,
    }
}

/// Renderer-neutral facts for a transcript item, or `None` for items without
/// structural presentation (plain messages and metadata).
#[must_use]
pub fn unit_facts(item: &TranscriptItem) -> Option<UnitFacts> {
    let lines = SizeBucket::of(item.text().lines().count());
    let base = UnitFacts {
        lines,
        ..UnitFacts::default()
    };
    match item.kind() {
        TranscriptItemKind::ToolRequest {
            producer_plugin_id,
            tool_name,
            active,
            status,
            ..
        } => Some(UnitFacts {
            kind: Some(UnitKind::Tool),
            tool: Some(tool_name.clone()),
            producer: producer_plugin_id.clone(),
            status: Some(status.map_or(
                if *active {
                    UnitStatus::Running
                } else {
                    UnitStatus::Requested
                },
                unit_status,
            )),
            is_error: matches!(status, Some(ToolInvocationViewStatus::Failed)),
            ..base
        }),
        TranscriptItemKind::ToolResult {
            tool_name,
            is_error,
            timing,
            ..
        } => Some(UnitFacts {
            kind: Some(UnitKind::Tool),
            tool: tool_name.clone(),
            status: Some(if *is_error {
                UnitStatus::Failed
            } else if timing.timed_out == Some(true) {
                UnitStatus::TimedOut
            } else {
                UnitStatus::Succeeded
            }),
            is_error: *is_error,
            ..base
        }),
        TranscriptItemKind::ToolContribution {
            contribution,
            invocation,
            ..
        } => {
            let invocation = invocation.as_deref();
            Some(UnitFacts {
                kind: Some(UnitKind::Tool),
                tool: invocation.and_then(|invocation| invocation.tool_name.clone()),
                producer: invocation
                    .and_then(|invocation| invocation.producer_plugin_id.clone())
                    .or_else(|| Some(contribution.producer_id.clone())),
                schema: Some(format!(
                    "{}/{}",
                    contribution.schema, contribution.schema_version
                )),
                status: invocation.map(|invocation| unit_status(invocation.status)),
                is_error: invocation
                    .and_then(|invocation| invocation.is_error)
                    .unwrap_or(false),
                ..base
            })
        }
        TranscriptItemKind::PermissionRequest { tool_name, .. } => Some(UnitFacts {
            kind: Some(UnitKind::Permission),
            tool: Some(tool_name.clone()),
            awaiting_decision: true,
            status: Some(UnitStatus::Waiting),
            ..base
        }),
        TranscriptItemKind::ReasoningMessage => Some(UnitFacts {
            kind: Some(UnitKind::Reasoning),
            ..base
        }),
        _ => None,
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

#[cfg(test)]
mod tests {
    use bcode_presentation_policy::models::PresentationSection;
    use bcode_tui_components::transcript::item::{permission_request_item, tool_result_item};

    use super::*;

    fn theme_layer(rules: &str) -> RuleLayer {
        RuleLayer {
            source: LayerSource::Theme("test".into()),
            rules: toml::from_str::<PresentationSection>(rules).unwrap().rules,
        }
    }

    fn state(rules: &str) -> StructureState {
        let mut state = StructureState::default();
        state.set_layers(&[theme_layer(rules)]);
        state
    }

    #[test]
    fn tool_results_resolve_through_theme_rules() {
        let state = state(
            "[[rules]]\nmatch = { kind = \"tool\", tool = \"read\" }\nset = { disclosure = \"collapsed\" }",
        );
        let read = tool_result_item("call-1", Some("read"), None, "contents", false);
        let bash = tool_result_item("call-2", Some("bash"), None, "ok", false);
        let snapshot = state.snapshot();
        assert_eq!(
            snapshot.presentation(&read).map(|p| p.disclosure),
            Some(Disclosure::Collapsed)
        );
        assert_eq!(
            snapshot.presentation(&bash).map(|p| p.disclosure),
            Some(Disclosure::Expanded)
        );
        assert_ne!(snapshot.signature(&read), snapshot.signature(&bash));
    }

    #[test]
    fn toggles_override_theme_defaults_and_bump_revision() {
        let mut state = state("[[rules]]\nset = { disclosure = \"collapsed\" }");
        let read = tool_result_item("call-1", Some("read"), None, "contents", false);
        let before = state.revision();
        assert_eq!(state.toggle(&read), Some(Disclosure::Expanded));
        assert!(state.revision() > before);
        assert_eq!(
            state.snapshot().presentation(&read).map(|p| p.disclosure),
            Some(Disclosure::Expanded)
        );
        assert_eq!(state.toggle(&read), Some(Disclosure::Collapsed));
        assert!(state.clear_overrides());
        assert!(!state.clear_overrides());
    }

    #[test]
    fn decisions_cannot_be_collapsed_by_rules_or_toggles() {
        let mut state = state("[[rules]]\nset = { disclosure = \"hidden\" }");
        let permission = permission_request_item("perm-1", "call-1", "bash", "{}", None, None);
        assert_eq!(
            state
                .snapshot()
                .presentation(&permission)
                .map(|p| p.disclosure),
            Some(Disclosure::Expanded)
        );
        assert_eq!(state.toggle(&permission), None);
        assert_eq!(
            state.set_all([(0, &permission)], Disclosure::Collapsed),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn failed_tools_stay_visible_under_theme_rules_but_users_may_collapse_them() {
        let mut state = state("[[rules]]\nset = { disclosure = \"hidden\" }");
        let failed = tool_result_item("call-1", Some("bash"), None, "boom", true);
        assert_eq!(
            state.snapshot().presentation(&failed).map(|p| p.disclosure),
            Some(Disclosure::Summary)
        );
        assert_eq!(state.toggle(&failed), Some(Disclosure::Expanded));
        assert_eq!(state.toggle(&failed), Some(Disclosure::Collapsed));
    }

    #[test]
    fn set_all_reports_changed_indexes_and_reconcile_drops_stale_overrides() {
        let mut state = StructureState::default();
        state.set_layers(&[]);
        let a = tool_result_item("a", Some("read"), None, "x", false);
        let b = tool_result_item("b", Some("read"), None, "y", false);
        assert_eq!(
            state.set_all([(3, &a), (7, &b)], Disclosure::Collapsed),
            [3, 7]
        );
        assert_eq!(
            state.set_all([(3, &a), (7, &b)], Disclosure::Collapsed),
            Vec::<usize>::new()
        );
        state.reconcile(&BTreeSet::from([a.id().get()]));
        assert_eq!(
            state.snapshot().presentation(&b).map(|p| p.disclosure),
            Some(Disclosure::Expanded)
        );
    }

    #[test]
    fn plain_messages_have_no_structural_presentation() {
        let message = TranscriptItem::new("assistant", "hello".to_owned());
        assert!(unit_facts(&message).is_none());
    }
}
