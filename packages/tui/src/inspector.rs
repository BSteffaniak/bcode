//! Structure inspector and global transcript tooling actions.
//!
//! The inspector explains how the focused disclosure target resolved: its
//! facts, every resolved property with the rule and layer that set it, the
//! visibility-floor adjustments, and any manual override. It reuses the
//! policy's own `explain` (through [`StructureState::explain`] and
//! [`StructureState::explain_facts`]) and the shared [`property_rows`]
//! flattening that `bcode theme explain` prints, so there is no second
//! resolver.
//!
//! This module also owns the two global actions that act on resolved
//! disclosure: cycling detail (collapsed → summary → expanded) and making the
//! focused tool's disclosure the default for that tool. Defaults persist to
//! user state through a [`TuiEffect`](crate::effects::TuiEffect), never by file
//! I/O on the interactive path.
//!
//! [`StructureState::explain`]: crate::structure::StructureState::explain
//! [`StructureState::explain_facts`]: crate::structure::StructureState::explain_facts

use std::cell::{Cell, RefCell};
use std::path::PathBuf;

use bcode_presentation_policy::PropertyOrigin;
use bcode_presentation_policy::models::{
    Disclosure, LayerSource, PresentationRule, ResolvedPresentation, UnitFacts, UnitKind,
};
use bmux_tui::component::LayoutNode;
use bmux_tui::event::{Event, MouseEvent};
use bmux_tui::geometry::Rect;
use bmux_tui_components::scroll_view::{ScrollViewOutcome, ScrollViewState};

use crate::app::BmuxApp;
use crate::structure::DisclosureTarget;
use crate::transcript::TranscriptItem;
use crate::transcript_focus::{DisclosureAnchors as _, FocusAction, next_detail};

/// Hit-region id of the painted inspector panel. It sits above transcript
/// hit regions, so clicks on the panel never reach units underneath.
pub const INSPECTOR_HIT_ID: &str = "transcript-inspector";
/// Rows one inspector scroll step moves.
pub const INSPECTOR_SCROLL_ROWS: i64 = 3;
/// Interactive defaults waiting to be persisted, at most.
const MAX_PENDING_DEFAULTS: usize = 32;
/// Bytes of one rendered fact or property value, at most; longer values end
/// with an ellipsis. Values are bounded for work, never measured for layout.
const MAX_VALUE_BYTES: usize = 256;
/// Levels in the detail cycle (collapsed, summary, expanded).
const DETAIL_LEVELS: usize = 3;
/// Fact and property rows in one report, at most each.
const MAX_REPORT_ROWS: usize = 256;

/// Transcript tooling state owned by the application: the inspector, the
/// global detail cycle position, and interactive defaults awaiting
/// persistence.
#[derive(Debug, Clone, Default)]
pub struct TranscriptTools {
    inspector: Option<InspectorState>,
    /// Last disclosure applied to every unit by a global action.
    detail_cycle: Option<Disclosure>,
    /// Default rules waiting for the persistence effect, oldest first; at
    /// most one rule per matcher.
    pending_defaults: Vec<PresentationRule>,
    /// Whether a persistence effect is running; pending rules wait for it.
    defaults_in_flight: bool,
}

impl TranscriptTools {
    /// Record a disclosure applied to every unit, so the next global detail
    /// cycle continues from it.
    pub const fn note_global_detail(&mut self, disclosure: Disclosure) {
        self.detail_cycle = Some(disclosure);
    }

    /// Queue `rule` for persistence, replacing a queued rule with the same
    /// matcher; the oldest rule is dropped beyond the bound.
    fn queue_default(&mut self, rule: PresentationRule) {
        self.pending_defaults
            .retain(|queued| queued.matcher != rule.matcher);
        self.pending_defaults.push(rule);
        let excess = self
            .pending_defaults
            .len()
            .saturating_sub(MAX_PENDING_DEFAULTS);
        self.pending_defaults.drain(..excess);
    }
}

/// Open-inspector state.
///
/// Interior mutability lets painting clamp the scroll offset against the
/// measured layout, reset it when focus moves, and record the painted body
/// geometry pointer events route through.
#[derive(Debug, Clone, Default)]
pub struct InspectorState {
    scroll: Cell<ScrollViewState>,
    /// Target the scroll offset belongs to.
    scrolled: RefCell<Option<DisclosureTarget>>,
    /// Body as last painted; `None` until painted or while it does not fit.
    painted: RefCell<Option<PaintedInspectorBody>>,
}

/// The inspector body as last painted.
///
/// Holds the terminal rectangle its text view occupied and the view's
/// resolved layout. Scrollbar clicks and drags route through exactly this
/// geometry, so they agree with what is on screen.
#[derive(Debug, Clone)]
pub struct PaintedInspectorBody {
    /// Terminal rectangle the body text view painted into.
    pub area: Rect,
    /// The body text view's resolved layout.
    pub layout: LayoutNode,
}

impl InspectorState {
    /// Scroll state for `target`, reset to the top when the target changed
    /// since the last call.
    #[must_use]
    pub fn scroll_for(&self, target: Option<&DisclosureTarget>) -> &Cell<ScrollViewState> {
        let mut scrolled = self.scrolled.borrow_mut();
        if scrolled.as_ref() != target {
            *scrolled = target.cloned();
            self.scroll.set(ScrollViewState::new());
        }
        &self.scroll
    }

    /// Move the scroll offset by `delta` rows; painting clamps it to the
    /// content. Returns whether the stored offset changed.
    pub fn scroll_by(&self, delta: i64) -> bool {
        let mut state = self.scroll.get();
        let old = state.vertical_offset();
        let next = if delta < 0 {
            old.saturating_sub(delta.unsigned_abs())
        } else {
            old.saturating_add(delta.unsigned_abs())
        };
        state.set_vertical_offset(next);
        self.scroll.set(state);
        next != old
    }

    /// Record the body geometry painted this frame, or `None` when the
    /// inspector did not fit; a scrollbar drag cannot outlive its geometry.
    pub fn record_painted(&self, painted: Option<PaintedInspectorBody>) {
        if painted.is_none() {
            self.stop_scrollbar_drag();
        }
        *self.painted.borrow_mut() = painted;
    }

    /// Body geometry recorded by the last paint.
    #[cfg(test)]
    pub fn painted_body_for_test(&self) -> Option<PaintedInspectorBody> {
        self.painted.borrow().clone()
    }

    /// Whether a scrollbar drag that started on the painted body is active.
    #[must_use]
    pub const fn scrollbar_dragging(&self) -> bool {
        self.scroll.get().dragging_scrollbar()
    }

    /// Route one left-button pointer event to the painted body's scrollbar:
    /// a press on the track jumps there and starts a drag, dragging moves
    /// the offset, and release ends the drag. Returns whether the event
    /// belonged to the scrollbar (it scrolled, or a drag started, continued,
    /// or ended).
    pub fn handle_scrollbar_mouse(&self, mouse: MouseEvent) -> bool {
        let painted = self.painted.borrow();
        let Some(painted) = painted.as_ref() else {
            let dragging = self.scrollbar_dragging();
            self.stop_scrollbar_drag();
            return dragging;
        };
        let was_dragging = self.scrollbar_dragging();
        let outcome = crate::inspector_render::body_view(&[], &self.scroll).handle_event(
            painted.area,
            &painted.layout,
            &Event::Mouse(mouse),
        );
        outcome != ScrollViewOutcome::Ignored || was_dragging || self.scrollbar_dragging()
    }

    /// End a scrollbar drag, keeping the offsets; BMUX exposes no drag
    /// setter, so the state is rebuilt from its public parts.
    fn stop_scrollbar_drag(&self) {
        let old = self.scroll.get();
        if !old.dragging_scrollbar() {
            return;
        }
        let mut state = ScrollViewState::new();
        state.interaction = old.interaction;
        state.set_vertical_offset(old.vertical_offset());
        state.set_horizontal_offset(old.horizontal_offset());
        state.set_follow_bottom(old.follows_bottom());
        self.scroll.set(state);
    }
}

/// One resolved property and the rule that set it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectedProperty {
    /// Property path (`disclosure`, `sections.output.visible`, …), matching
    /// [`PropertyOrigin::property`].
    pub property: String,
    /// Resolved value.
    pub value: serde_json::Value,
    /// Layer and rule that set it; `None` for host defaults.
    pub origin: Option<PropertyOrigin>,
}

/// What the inspector shows for one disclosure target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectorReport {
    /// The inspected target.
    pub target: DisclosureTarget,
    /// Facts the owning unit (or group) resolved with.
    pub facts: UnitFacts,
    /// The target's effective disclosure, as painted.
    pub effective: Disclosure,
    /// Manual override recorded for the target, before the floor.
    pub manual: Option<Disclosure>,
    /// Every resolved property with its origin; for a section target the
    /// section's own properties come first.
    pub properties: Vec<InspectedProperty>,
    /// Visibility-floor adjustments and manual-toggle notes.
    pub floor: Vec<String>,
    /// Grouping context (membership, group members).
    pub notes: Vec<String>,
}

impl InspectorReport {
    /// The property row named `property`, if resolved.
    #[must_use]
    pub fn property(&self, property: &str) -> Option<&InspectedProperty> {
        self.properties.iter().find(|row| row.property == property)
    }
}

/// Flatten a resolved presentation into property rows, attaching origins.
///
/// Sections expand to `sections.<name>.<field>` and plugin properties use
/// their namespaced key, matching [`PropertyOrigin::property`] names. Shared
/// with `bcode theme explain`, so the CLI and the inspector agree row for row.
#[must_use]
pub fn property_rows(
    presentation: &ResolvedPresentation,
    origins: &[PropertyOrigin],
) -> Vec<InspectedProperty> {
    let Ok(serde_json::Value::Object(fields)) = serde_json::to_value(presentation) else {
        return Vec::new();
    };
    let mut values = Vec::new();
    for (name, value) in fields {
        match (name.as_str(), value) {
            ("sections", serde_json::Value::Object(sections)) => {
                for (section, value) in sections {
                    if let serde_json::Value::Object(section_fields) = value {
                        for (field, value) in section_fields {
                            values.push((format!("sections.{section}.{field}"), value));
                        }
                    }
                }
            }
            ("plugin", serde_json::Value::Object(plugin)) => values.extend(plugin),
            (_, value) => values.push((name, value)),
        }
    }
    values
        .into_iter()
        .map(|(property, value)| InspectedProperty {
            origin: origins
                .iter()
                .find(|origin| origin.property == property)
                .cloned(),
            property,
            value,
        })
        .collect()
}

/// Short human label for a layer source (`theme:compact`, `config`, …).
#[must_use]
pub fn layer_label(source: &LayerSource) -> String {
    match source {
        LayerSource::Host => "host".to_owned(),
        LayerSource::Plugin(id) => format!("plugin:{id}"),
        LayerSource::Theme(id) => format!("theme:{id}"),
        LayerSource::Config => "config".to_owned(),
        LayerSource::UserState => "user_state".to_owned(),
    }
}

/// Origin label of a property row: `<layer>#<rule>`, or `default`.
#[must_use]
pub fn origin_label(origin: Option<&PropertyOrigin>) -> String {
    origin.map_or_else(
        || "default".to_owned(),
        |origin| format!("{}#{}", layer_label(&origin.source), origin.rule),
    )
}

/// Facts as `(name, value)` rows, omitting unknown (null) facts; plugin facts
/// use their namespaced key.
#[must_use]
pub fn fact_rows(facts: &UnitFacts) -> Vec<(String, String)> {
    let Ok(serde_json::Value::Object(fields)) = serde_json::to_value(facts) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for (name, value) in fields {
        match (name.as_str(), value) {
            (_, serde_json::Value::Null) => {}
            ("plugin", serde_json::Value::Object(plugin)) => {
                rows.extend(
                    plugin
                        .into_iter()
                        .map(|(key, value)| (key, value_text(&value))),
                );
            }
            (_, value) => rows.push((name, value_text(&value))),
        }
    }
    rows.truncate(MAX_REPORT_ROWS);
    rows
}

/// Display text of a JSON value: strings unquoted, others compact JSON, at
/// most [`MAX_VALUE_BYTES`] bytes (cut on a character boundary).
#[must_use]
pub fn value_text(value: &serde_json::Value) -> String {
    let text = match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    bounded_text(text)
}

/// `text` cut to at most [`MAX_VALUE_BYTES`] bytes on a character boundary,
/// ending with an ellipsis when cut. Bounds work, never measures layout.
#[must_use]
pub fn bounded_text(mut text: String) -> String {
    if text.len() <= MAX_VALUE_BYTES {
        return text;
    }
    let mut end = MAX_VALUE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push('…');
    text
}

/// Human label of a disclosure.
#[must_use]
pub const fn disclosure_name(disclosure: Disclosure) -> &'static str {
    match disclosure {
        Disclosure::Expanded => "expanded",
        Disclosure::Summary => "summary",
        Disclosure::Collapsed => "collapsed",
        Disclosure::Hidden => "hidden",
    }
}

/// Report for a structural unit `item` targeted by `target`.
fn unit_report(
    app: &BmuxApp,
    item: &TranscriptItem,
    target: &DisclosureTarget,
) -> Option<InspectorReport> {
    let structure = app.structure();
    let snapshot = structure.snapshot();
    let facts = snapshot.facts(item)?;
    let explained = structure.explain(item)?;
    let item_id = item.id().get();
    let mut properties = property_rows(&explained.presentation, &explained.origins);
    properties.truncate(MAX_REPORT_ROWS);
    let mut floor = explained.floor;
    let mut notes = Vec::new();
    let manual = snapshot.target_override(target);
    let effective = match target {
        DisclosureTarget::Section(_, name) => {
            // The section's own rows first; the rest of the unit follows.
            let prefix = format!("sections.{name}.");
            properties.sort_by_key(|row| !row.property.starts_with(&prefix));
            let effective = snapshot.section(item, name).disclosure;
            if let Some(manual) = manual
                && manual != effective
            {
                floor.push(format!(
                    "visibility floor keeps section {name} {} (manual: {})",
                    disclosure_name(effective),
                    disclosure_name(manual)
                ));
            }
            effective
        }
        _ => snapshot
            .presentation(item)
            .map_or(explained.presentation.disclosure, |presentation| {
                presentation.disclosure
            }),
    };
    if let Some(group) = snapshot.group_of(item_id) {
        notes.push(format!(
            "in {}; the group's disclosure decides whether it renders",
            DisclosureTarget::Group(group)
        ));
        if !matches!(target, DisclosureTarget::Section(..))
            && effective != explained.presentation.disclosure
        {
            notes.push(format!(
                "shown {} as a member of its group (resolved {})",
                disclosure_name(effective),
                disclosure_name(explained.presentation.disclosure)
            ));
        }
    }
    Some(InspectorReport {
        target: target.clone(),
        facts,
        effective,
        manual,
        properties,
        floor,
        notes,
    })
}

/// Report for the group whose first member is `group`.
fn group_report(app: &BmuxApp, group: u64) -> Option<InspectorReport> {
    let structure = app.structure();
    let snapshot = structure.snapshot();
    let view = snapshot.group_view(group)?;
    let target = DisclosureTarget::Group(group);
    let explained = structure.explain_facts(&view.facts);
    let effective = snapshot.group_disclosure(group)?;
    let manual = snapshot.target_override(&target);
    let mut properties = property_rows(&explained.presentation, &explained.origins);
    properties.truncate(MAX_REPORT_ROWS);
    let mut floor = explained.floor;
    if let Some(manual) = manual {
        floor.push(format!(
            "manual toggle: disclosure set to {}",
            disclosure_name(manual)
        ));
    }
    if explained.presentation.disclosure == Disclosure::Hidden {
        floor.push("groups are never hidden: shown collapsed".to_owned());
    }
    let tools = view
        .tools
        .iter()
        .map(|(tool, count)| format!("{tool}×{count}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut notes = vec![bounded_text(format!(
        "{} member(s): {tools}",
        view.members.len()
    ))];
    if !view.bridged.is_empty() {
        notes.push(format!("{} bridged unit(s)", view.bridged.len()));
    }
    if !view.attached.is_empty() {
        notes.push(format!("{} attached unit(s)", view.attached.len()));
    }
    Some(InspectorReport {
        target,
        facts: view.facts.clone(),
        effective,
        manual,
        properties,
        floor,
        notes,
    })
}

impl BmuxApp {
    /// Resident transcript item with `item_id`, located through the focused
    /// target's verified entry hint before falling back to a scan.
    fn tooling_item(&self, item_id: u64) -> Option<&TranscriptItem> {
        let index = self.disclosure_anchors().entry_of_item(item_id)?;
        self.transcript().get(index)
    }

    /// Run a focus-mode tooling action (inspector, detail cycle, per-tool
    /// default); returns `Some(changed)` when handled and `None` for other
    /// actions, or when no target is focused (focus mode then refocuses).
    pub fn handle_transcript_tool_action(&mut self, action: FocusAction) -> Option<bool> {
        match action {
            FocusAction::Inspect => Some(self.toggle_inspector()),
            FocusAction::InspectorScrollDown => Some(self.scroll_inspector(INSPECTOR_SCROLL_ROWS)),
            FocusAction::InspectorScrollUp => Some(self.scroll_inspector(-INSPECTOR_SCROLL_ROWS)),
            FocusAction::CycleDetail if self.focused_disclosure_target().is_some() => {
                Some(self.cycle_detail())
            }
            FocusAction::MakeDefault if self.focused_disclosure_target().is_some() => {
                Some(self.make_focused_default())
            }
            _ => None,
        }
    }

    /// Whether the structure inspector is open.
    #[must_use]
    pub const fn inspector_open(&self) -> bool {
        self.transcript_tools.inspector.is_some()
    }

    /// Open inspector state, when open.
    #[must_use]
    pub const fn inspector_state(&self) -> Option<&InspectorState> {
        self.transcript_tools.inspector.as_ref()
    }

    /// Explain `target`, when it is a resident structural unit, section, or
    /// group.
    #[must_use]
    pub fn inspect_target(&self, target: &DisclosureTarget) -> Option<InspectorReport> {
        match target {
            DisclosureTarget::Group(group) => group_report(self, *group),
            DisclosureTarget::Unit(id) | DisclosureTarget::Section(id, _) => {
                unit_report(self, self.tooling_item(*id)?, target)
            }
        }
    }

    /// Report for the focused target while the inspector is open.
    #[must_use]
    pub fn inspector_report(&self) -> Option<InspectorReport> {
        if !self.inspector_open() {
            return None;
        }
        self.inspect_target(self.focused_disclosure_target()?)
    }

    /// Open the inspector on the focused target, or close it; returns
    /// whether the UI changed.
    pub fn toggle_inspector(&mut self) -> bool {
        if self.close_inspector() {
            return true;
        }
        if self.focused_disclosure_target().is_none() {
            self.set_status("Focus a transcript unit to inspect it".to_owned());
            return true;
        }
        self.transcript_tools.inspector = Some(InspectorState::default());
        let scope = crate::transcript_focus::FOCUS_SCOPE;
        let action = crate::transcript_focus::FocusAction::Inspect.id();
        let down = crate::transcript_focus::FocusAction::InspectorScrollDown.id();
        let up = crate::transcript_focus::FocusAction::InspectorScrollUp.id();
        self.set_status(format!(
            "INSPECTOR · follows focus · {} / {} scroll · {} close",
            self.shortcut_hint(scope, down),
            self.shortcut_hint(scope, up),
            self.shortcut_hint(scope, action),
        ));
        true
    }

    /// Close the inspector; returns whether it was open.
    pub fn close_inspector(&mut self) -> bool {
        if self.transcript_tools.inspector.take().is_none() {
            return false;
        }
        self.set_status("Inspector closed".to_owned());
        true
    }

    /// Scroll the open inspector by `delta` rows; returns whether its offset
    /// changed. The offset lives in the inspector's interior-mutable scroll
    /// state, which painting clamps.
    pub fn scroll_inspector(&self, delta: i64) -> bool {
        self.transcript_tools
            .inspector
            .as_ref()
            .is_some_and(|inspector| inspector.scroll_by(delta))
    }

    /// Whether a drag on the open inspector's scrollbar is active.
    #[must_use]
    pub fn inspector_scrollbar_dragging(&self) -> bool {
        self.transcript_tools
            .inspector
            .as_ref()
            .is_some_and(InspectorState::scrollbar_dragging)
    }

    /// Route a pointer event to the open inspector's painted scrollbar;
    /// returns whether the scrollbar took it (see
    /// [`InspectorState::handle_scrollbar_mouse`]).
    pub fn inspector_scrollbar_mouse(&self, mouse: MouseEvent) -> bool {
        self.transcript_tools
            .inspector
            .as_ref()
            .is_some_and(|inspector| inspector.handle_scrollbar_mouse(mouse))
    }

    /// Run a transcript tooling command-palette route (`transcript.inspect`,
    /// `transcript.cycleDetail`, `transcript.makeDefault`,
    /// `transcript.focusUnits`, `tools.expandAll`, `tools.collapseAll`,
    /// `tools.resetDisclosure`). Each runs the same handler as its keymap
    /// action, so bulk actions in focus mode apply to the focused subtree.
    /// Returns `None` for other routes, else whether the UI changed.
    pub fn run_transcript_tool_route(&mut self, route: &str) -> Option<bool> {
        Some(match route {
            "transcript.inspect" => self.open_inspector_from_palette(),
            "transcript.cycleDetail" => self.cycle_detail(),
            "transcript.makeDefault" => self.make_focused_default(),
            "transcript.focusUnits" => {
                if self.transcript_focus_active() {
                    self.exit_transcript_focus()
                } else {
                    self.enter_transcript_focus()
                }
            }
            "tools.expandAll" => self.set_all_disclosure(Disclosure::Expanded),
            "tools.collapseAll" => self.set_all_disclosure(Disclosure::Collapsed),
            "tools.resetDisclosure" => self.reset_disclosure(),
            _ => return None,
        })
    }

    /// Enter focus mode if needed and open the inspector (the palette entry
    /// point); returns whether the UI changed.
    pub fn open_inspector_from_palette(&mut self) -> bool {
        let entered = !self.transcript_focus_active() && self.enter_transcript_focus();
        if self.inspector_open() {
            return entered;
        }
        self.toggle_inspector() || entered
    }

    /// Cycle detail collapsed → summary → expanded: the focused target in
    /// focus mode, every unit and group otherwise. Levels the visibility
    /// floor refuses are skipped. Returns whether the UI changed.
    pub fn cycle_detail(&mut self) -> bool {
        if let Some(target) = self.focused_disclosure_target().cloned() {
            let Some((current, _)) = self.target_disclosure(&target) else {
                return false;
            };
            let mut next = next_detail(current);
            for _ in 0..DETAIL_LEVELS {
                if next != current && self.set_target_disclosure(&target, next) {
                    return true;
                }
                next = next_detail(next);
            }
            self.set_status("The visibility floor keeps this target open".to_owned());
            return true;
        }
        let start = self
            .transcript_tools
            .detail_cycle
            .unwrap_or(Disclosure::Expanded);
        let mut next = next_detail(start);
        for _ in 0..DETAIL_LEVELS {
            if next != start && self.set_all_disclosure(next) {
                return true;
            }
            next = next_detail(next);
        }
        false
    }

    /// Queue the focused tool unit's current disclosure as the default for
    /// its tool; the rule persists to user state through the effect queue and
    /// is applied live once stored. Returns whether the UI changed.
    pub fn make_focused_default(&mut self) -> bool {
        let Some(DisclosureTarget::Unit(id)) = self.focused_disclosure_target().cloned() else {
            self.set_status("Focus a tool unit to make its disclosure the default".to_owned());
            return true;
        };
        let is_tool = self
            .target_disclosure(&DisclosureTarget::Unit(id))
            .is_some_and(|(_, facts)| facts.kind == Some(UnitKind::Tool));
        let Some(rule) = self.default_disclosure_rule(id).filter(|_| is_tool) else {
            self.set_status("Only tool units can set a per-tool default".to_owned());
            return true;
        };
        self.set_status(format!("Saving {}…", default_label(&rule)));
        self.transcript_tools.queue_default(rule);
        true
    }

    /// Interactive default rules to persist now, unless a persistence effect
    /// is already running (they then wait for it to finish).
    pub fn take_pending_default_rules(&mut self) -> Option<Vec<PresentationRule>> {
        let tools = &mut self.transcript_tools;
        if tools.defaults_in_flight || tools.pending_defaults.is_empty() {
            return None;
        }
        tools.defaults_in_flight = true;
        Some(std::mem::take(&mut tools.pending_defaults))
    }

    /// Apply the outcome of persisting `saved`: on success the stored user
    /// state rules recompile live. Returns whether the UI changed.
    pub fn finish_default_rules(
        &mut self,
        saved: &[PresentationRule],
        result: Result<(PathBuf, Vec<PresentationRule>), String>,
    ) -> bool {
        self.transcript_tools.defaults_in_flight = false;
        let labels = saved
            .iter()
            .map(default_label)
            .collect::<Vec<_>>()
            .join(", ");
        match result {
            Ok((path, rules)) => {
                self.apply_user_structure_rules(rules);
                self.set_status(format!(
                    "Saved {labels} to user state at {}",
                    bcode_plugin_sdk::path::display_from_current_dir(&path)
                ));
            }
            Err(error) => self.set_status(format!("Could not save {labels}: {error}")),
        }
        true
    }
}

/// `default for <tool>: <disclosure>` for a per-tool default rule.
fn default_label(rule: &PresentationRule) -> String {
    let tools = rule.matcher.tool.0.join(", ");
    let disclosure = rule.set.disclosure.map_or("unchanged", disclosure_name);
    format!("default for {tools}: {disclosure}")
}

#[cfg(test)]
mod tests {
    use bcode_presentation_policy::models::{FactMatcher, OneOrMany, PropertyPatch, RuleLayer};

    use super::*;
    use crate::app::disclosure_focus_tests::focus_app;

    fn tool_rule(tool: &str, disclosure: Disclosure) -> PresentationRule {
        PresentationRule {
            matcher: FactMatcher {
                kind: OneOrMany(vec![UnitKind::Tool]),
                tool: OneOrMany(vec![tool.to_owned()]),
                ..FactMatcher::default()
            },
            set: PropertyPatch {
                disclosure: Some(disclosure),
                ..PropertyPatch::default()
            },
        }
    }

    fn focused(app: &BmuxApp) -> DisclosureTarget {
        app.focused_disclosure_target().cloned().expect("focus")
    }

    #[test]
    fn inspector_reports_facts_origins_manual_overrides_and_follows_focus() {
        let (mut app, ids) = focus_app(3);
        app.apply_configured_structure_rules(vec![tool_rule("read", Disclosure::Summary)]);
        // Without a focused target the inspector does not open.
        assert!(app.toggle_inspector());
        assert!(!app.inspector_open());
        assert!(app.enter_transcript_focus());
        assert!(app.handle_transcript_focus_action(FocusAction::Inspect));
        assert!(app.inspector_open());
        let report = app.inspector_report().expect("report");
        assert_eq!(report.target, DisclosureTarget::Unit(ids[2]));
        assert!(
            fact_rows(&report.facts)
                .iter()
                .any(|(name, value)| name == "tool" && value == "read")
        );
        let disclosure = report.property("disclosure").expect("disclosure row");
        assert_eq!(disclosure.value, serde_json::json!("summary"));
        assert_eq!(
            disclosure.origin.as_ref().map(|origin| &origin.source),
            Some(&LayerSource::Config)
        );
        assert_eq!(
            origin_label(
                report
                    .property("indent")
                    .and_then(|row| row.origin.as_ref())
            ),
            "default"
        );
        assert_eq!(report.manual, None);

        // A manual toggle is reported over the rule.
        assert!(app.handle_transcript_focus_action(FocusAction::Toggle));
        let report = app.inspector_report().expect("report");
        assert_eq!(report.manual, Some(Disclosure::Expanded));
        assert_eq!(report.effective, Disclosure::Expanded);
        assert!(
            report
                .floor
                .iter()
                .any(|note| note.contains("manual toggle"))
        );

        // The inspector follows focus, including onto sections.
        assert!(app.handle_transcript_focus_action(FocusAction::Previous));
        let report = app.inspector_report().expect("section report");
        assert_eq!(
            report.target,
            DisclosureTarget::Section(ids[1], "output".to_owned())
        );
        // Escape closes the inspector before leaving focus mode.
        assert!(app.handle_transcript_focus_action(FocusAction::Exit));
        assert!(!app.inspector_open());
        assert!(app.transcript_focus_active());
        assert!(app.handle_transcript_focus_action(FocusAction::Exit));
        assert!(!app.transcript_focus_active());
    }

    #[test]
    fn floor_adjustments_appear_for_units_awaiting_a_decision() {
        let facts = UnitFacts {
            kind: Some(UnitKind::Permission),
            awaiting_decision: true,
            ..UnitFacts::default()
        };
        let mut state = crate::structure::StructureState::default();
        state.set_layers(&[RuleLayer {
            source: LayerSource::Theme("quiet".to_owned()),
            rules: vec![PresentationRule {
                matcher: FactMatcher::default(),
                set: PropertyPatch {
                    disclosure: Some(Disclosure::Collapsed),
                    ..PropertyPatch::default()
                },
            }],
        }]);
        let explained = state.explain_facts(&facts);
        assert_eq!(explained.presentation.disclosure, Disclosure::Expanded);
        assert_ne!(explained.floor, Vec::<String>::new());
        let rows = property_rows(&explained.presentation, &explained.origins);
        let disclosure = rows
            .iter()
            .find(|row| row.property == "disclosure")
            .expect("row");
        assert_eq!(disclosure.value, serde_json::json!("expanded"));
        assert_eq!(
            disclosure.origin.as_ref().map(|origin| &origin.source),
            Some(&LayerSource::Theme("quiet".to_owned()))
        );
    }

    #[test]
    fn cycle_detail_walks_collapsed_summary_expanded_for_focus_and_everything() {
        let (mut app, ids) = focus_app(3);
        let unit = |index: usize| DisclosureTarget::Unit(ids[index]);
        let disclosure = |app: &BmuxApp, target: &DisclosureTarget| {
            app.target_disclosure(target)
                .map(|(disclosure, _)| disclosure)
        };
        // Outside focus mode every unit cycles together.
        for expected in [
            Disclosure::Collapsed,
            Disclosure::Summary,
            Disclosure::Expanded,
            Disclosure::Collapsed,
        ] {
            assert!(app.cycle_detail());
            for index in 0..3 {
                assert_eq!(disclosure(&app, &unit(index)), Some(expected));
            }
        }
        // In focus mode only the focused target cycles.
        assert!(app.enter_transcript_focus());
        assert_eq!(focused(&app), unit(2));
        assert!(app.handle_transcript_focus_action(FocusAction::CycleDetail));
        assert_eq!(disclosure(&app, &unit(2)), Some(Disclosure::Summary));
        assert_eq!(disclosure(&app, &unit(0)), Some(Disclosure::Collapsed));
        assert!(app.handle_transcript_focus_action(FocusAction::CycleDetail));
        assert_eq!(disclosure(&app, &unit(2)), Some(Disclosure::Expanded));
    }

    #[test]
    fn make_default_queues_one_rule_per_tool_and_applies_stored_rules_live() {
        let (mut app, ids) = focus_app(2);
        assert!(app.make_focused_default());
        assert_eq!(app.take_pending_default_rules(), None);
        assert!(app.enter_transcript_focus());
        // Focus starts on the last unit's `output` section; climb to the unit.
        assert!(app.handle_transcript_focus_action(FocusAction::Previous));
        assert_eq!(focused(&app), DisclosureTarget::Unit(ids[1]));
        assert!(app.handle_transcript_focus_action(FocusAction::Toggle));
        assert!(app.handle_transcript_focus_action(FocusAction::MakeDefault));
        // Pressing again replaces the queued rule instead of duplicating it.
        assert!(app.handle_transcript_focus_action(FocusAction::MakeDefault));
        let saved = app.take_pending_default_rules().expect("queued rule");
        assert_eq!(saved, [tool_rule("read", Disclosure::Collapsed)]);
        // While the effect runs, new defaults wait for it.
        assert!(app.handle_transcript_focus_action(FocusAction::MakeDefault));
        assert_eq!(app.take_pending_default_rules(), None);

        // The stored user-state rules recompile live; explain reports the
        // user-state layer for the other unit, which has no manual toggle.
        assert!(app.finish_default_rules(&saved, Ok((PathBuf::from("tui.toml"), saved.clone()))));
        assert!(app.status().contains("default for read: collapsed"));
        let report = app
            .inspect_target(&DisclosureTarget::Unit(ids[0]))
            .expect("report");
        assert_eq!(report.effective, Disclosure::Collapsed);
        assert_eq!(
            report
                .property("disclosure")
                .and_then(|row| row.origin.as_ref())
                .map(|origin| &origin.source),
            Some(&LayerSource::UserState)
        );
        // The rule queued during the effect persists next.
        assert_eq!(
            app.take_pending_default_rules().map(|rules| rules.len()),
            Some(1)
        );
        assert!(app.finish_default_rules(&[], Err("disk full".to_owned())));
        assert!(app.status().contains("disk full"));
    }

    #[test]
    fn make_default_refuses_sections_and_non_tool_targets() {
        let (mut app, _) = focus_app(2);
        assert!(app.enter_transcript_focus());
        assert!(matches!(focused(&app), DisclosureTarget::Section(..)));
        assert!(app.make_focused_default());
        assert!(app.status().contains("Focus a tool unit"));
        assert_eq!(app.take_pending_default_rules(), None);
    }

    #[test]
    fn values_are_bounded_on_character_boundaries() {
        let long = "日本語🚀".repeat(100);
        let text = value_text(&serde_json::Value::String(long));
        assert!(text.len() <= MAX_VALUE_BYTES + '…'.len_utf8());
        assert!(text.ends_with('…'));
        assert_eq!(value_text(&serde_json::json!(3)), "3");
        assert_eq!(value_text(&serde_json::json!("e\u{301}")), "e\u{301}");
    }

    #[test]
    fn scroll_resets_when_the_inspected_target_changes() {
        let state = InspectorState::default();
        let first = DisclosureTarget::Unit(1);
        let _ = state.scroll_for(Some(&first));
        assert!(state.scroll_by(5));
        assert_eq!(state.scroll_for(Some(&first)).get().vertical_offset(), 5);
        assert!(state.scroll_by(-10));
        assert_eq!(state.scroll_for(Some(&first)).get().vertical_offset(), 0);
        assert!(!state.scroll_by(-1));
        assert!(state.scroll_by(4));
        let second = DisclosureTarget::Unit(2);
        assert_eq!(state.scroll_for(Some(&second)).get().vertical_offset(), 0);
    }
}
