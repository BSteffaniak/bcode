//! Structure inspector and transcript tooling for the V2 engine (spec section 6, FO6).
//!
//! The inspector explains how the focused disclosure target resolved, with the content the
//! V1 inspector shows: facts, every resolved property with the rule and layer that set it,
//! visibility-floor adjustments, the manual override and grouping notes. The explanation comes
//! from the V2 engine's own structure resolution ([`TranscriptEngine::inspect_focused`]);
//! presentation helpers that only format policy types (`fact_rows`, `property_rows`,
//! placement and the bordered panel) are shared with the V1 inspector, so both engines paint
//! the same panel. Nothing here reads or changes V1 transcript state.
//!
//! The inspector paints over the transcript viewport (a side panel on wide viewports, an inset
//! modal on narrow ones) after the transcript, and registers its hit region above transcript
//! regions, so wheel and pointer input over it scroll the inspector rather than the transcript
//! (B7).
//!
//! Tooling actions that need the host shell report through [`ToolNotice`]s (status text) and
//! queued per-tool default rules, which the root model drains after every update: the rules
//! persist to user state through the same effect V1 uses (A3).
//!
//! [`TranscriptEngine::inspect_focused`]: bcode_tui_transcript::engine::TranscriptEngine::inspect_focused

use std::cell::Cell;

use bcode_presentation_policy::models::{Disclosure, PresentationRule};
use bcode_tui_transcript::engine::{DefaultRuleRefusal, DetailCycle};
use bcode_tui_transcript::units::structure::{DisclosureTarget, Inspection};
use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::paint::PaintCx;
use bmux_tui::prelude::{Line, Span};
use bmux_tui::style::Modifier;
use bmux_tui_components::scroll_view::{ScrollViewOutcome, ScrollViewState};

use super::TranscriptV2Host;
use crate::app::BmuxApp;
use crate::inspector::{
    INSPECTOR_HIT_ID, INSPECTOR_SCROLL_ROWS, InspectedProperty, PaintedInspectorBody, bounded_text,
    disclosure_name, fact_rows, origin_label, property_rows, value_text,
};
use crate::inspector_render::{InspectorPlacement, body_view, inspector_placement, paint_panel};
use crate::render::TuiTheme;

/// Fact and property rows in one report, at most each.
const MAX_REPORT_ROWS: usize = 256;
/// Default rules waiting to be handed to the host shell, at most.
const MAX_PENDING_DEFAULTS: usize = 32;

/// A status message from a transcript tooling action, formatted by the host shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolNotice {
    /// The inspector opened; the text names its scroll and close keys.
    InspectorOpened,
    /// The inspector closed.
    InspectorClosed,
    /// Nothing inspectable is focused.
    NothingToInspect,
    /// The visibility floor refused every detail level of the focused target.
    FloorKeepsOpen,
    /// The focused target is not a unit.
    DefaultNeedsUnit,
    /// The focused unit is not a tool unit.
    DefaultNeedsTool,
    /// Every unit (or the focused subtree) changed to this disclosure.
    Disclosure {
        /// Whether only the focused subtree changed.
        focused: bool,
        /// The new disclosure.
        disclosure: Disclosure,
    },
    /// Manual disclosure was reset.
    Reset {
        /// Whether only the focused subtree was reset.
        focused: bool,
    },
    /// The focused shell screen expanded or collapsed inline (C39).
    Shell {
        /// Whether it is now expanded.
        expanded: bool,
    },
}

impl ToolNotice {
    /// Status text, with key hints from the host shell's effective keymap.
    #[must_use]
    pub fn text(&self, app: &BmuxApp) -> String {
        match self {
            Self::InspectorOpened => {
                let scope = super::focus::FOCUS_SCOPE;
                format!(
                    "INSPECTOR · follows focus · {} / {} scroll · {} close",
                    app.shortcut_hint(scope, "tui.transcriptFocus.inspectorScrollDown"),
                    app.shortcut_hint(scope, "tui.transcriptFocus.inspectorScrollUp"),
                    app.shortcut_hint(scope, "tui.transcript.inspect"),
                )
            }
            Self::InspectorClosed => "Inspector closed".to_owned(),
            Self::NothingToInspect => "Focus a transcript unit to inspect it".to_owned(),
            Self::FloorKeepsOpen => "The visibility floor keeps this target open".to_owned(),
            Self::DefaultNeedsUnit => {
                "Focus a tool unit to make its disclosure the default".to_owned()
            }
            Self::DefaultNeedsTool => "Only tool units can set a per-tool default".to_owned(),
            Self::Disclosure {
                focused: true,
                disclosure,
            } => format!(
                "Focused target {} with its contents",
                disclosure_name(*disclosure)
            ),
            Self::Disclosure {
                focused: false,
                disclosure,
            } => format!("All units {}", disclosure_name(*disclosure)),
            Self::Reset { focused: true } => "Focused disclosure reset to defaults".to_owned(),
            Self::Reset { focused: false } => "Tool disclosure reset to defaults".to_owned(),
            Self::Shell { expanded: true } => "Shell screen expanded inline".to_owned(),
            Self::Shell { expanded: false } => "Shell screen collapsed".to_owned(),
        }
    }
}

/// Open-inspector state.
#[derive(Debug, Default)]
struct InspectorPanel {
    /// Body scroll offset; the BMUX text view takes it as a cell.
    scroll: Cell<ScrollViewState>,
    /// Target the scroll offset belongs to; a focus move resets it to the top.
    scrolled: Option<DisclosureTarget>,
    /// The focused target's inspection, refreshed in the update path after every engine
    /// update ([`TranscriptV2Host::refresh_inspector`]); painting only formats it (F3).
    report: Option<Inspection>,
    /// Body geometry as last painted; scrollbar input routes through exactly this geometry.
    painted: Option<PaintedInspectorBody>,
}

impl InspectorPanel {
    fn scroll_by(&self, delta: i64) -> bool {
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

    const fn dragging(&self) -> bool {
        self.scroll.get().dragging_scrollbar()
    }

    /// End a scrollbar drag, keeping the offsets (BMUX exposes no drag setter).
    fn stop_drag(&self) {
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

/// Host-side transcript tooling state: the inspector panel, pending notices and per-tool
/// default rules waiting for the host shell.
#[derive(Debug, Default)]
pub struct HostTools {
    inspector: Option<InspectorPanel>,
    notice: Option<ToolNotice>,
    default_rules: Vec<PresentationRule>,
}

impl TranscriptV2Host {
    /// Whether the structure inspector is open.
    #[must_use]
    pub const fn inspector_open(&self) -> bool {
        self.tools.inspector.is_some()
    }

    /// The status notice of the last tooling action, once.
    pub const fn take_notice(&mut self) -> Option<ToolNotice> {
        self.tools.notice.take()
    }

    /// Per-tool default rules queued by make-default (`m`), once. The host shell persists them
    /// to user state (A3).
    pub fn take_default_rules(&mut self) -> Vec<PresentationRule> {
        std::mem::take(&mut self.tools.default_rules)
    }

    pub(super) const fn notify(&mut self, notice: ToolNotice) {
        self.tools.notice = Some(notice);
    }

    /// The inspection the open inspector shows: the focused target's.
    #[must_use]
    pub fn inspector_report(&self) -> Option<Inspection> {
        self.tools.inspector.as_ref()?.report.clone()
    }

    /// Follow the focused target in the update path (F3): a focus move resets the panel's
    /// scroll to the top, and the inspection is rebuilt from the engine's current structure.
    pub(super) fn refresh_inspector(&mut self) {
        let Some(panel) = self.tools.inspector.as_mut() else {
            return;
        };
        let target = self.engine.focus_state().disclosure().cloned();
        if panel.scrolled != target {
            panel.scrolled = target;
            panel.scroll.set(ScrollViewState::new());
        }
        let report = self.engine.inspect_focused();
        if panel.report != report {
            panel.report = report;
            self.invalidated = true;
        }
    }

    /// The open inspector's body scroll offset.
    #[cfg(test)]
    pub(super) fn inspector_scroll_offset(&self) -> Option<u64> {
        self.tools
            .inspector
            .as_ref()
            .map(|panel| panel.scroll.get().vertical_offset())
    }

    /// Open the inspector on the focused target, or close it (`i`). Returns whether the UI
    /// changed; without a focused disclosure the inspector stays closed and a notice says why.
    pub(super) fn toggle_inspector(&mut self) -> bool {
        if self.close_inspector() {
            return true;
        }
        if self.engine.focus_state().disclosure().is_none() {
            self.notify(ToolNotice::NothingToInspect);
            return true;
        }
        self.tools.inspector = Some(InspectorPanel::default());
        self.refresh_inspector();
        self.notify(ToolNotice::InspectorOpened);
        self.invalidated = true;
        true
    }

    /// Drop an open inspector without a notice (focus left the transcript).
    pub(super) fn discard_inspector(&mut self) {
        if self.tools.inspector.take().is_some() {
            self.invalidated = true;
        }
    }

    /// Close the inspector; returns whether it was open.
    pub(super) fn close_inspector(&mut self) -> bool {
        if self.tools.inspector.take().is_none() {
            return false;
        }
        self.notify(ToolNotice::InspectorClosed);
        self.invalidated = true;
        true
    }

    /// Scroll the open inspector by `delta` rows (shift+Up/Down, the wheel over the panel);
    /// painting clamps the offset. Returns whether the offset changed.
    pub(super) fn scroll_inspector(&self, delta: i64) -> bool {
        self.tools
            .inspector
            .as_ref()
            .is_some_and(|panel| panel.scroll_by(delta))
    }

    /// Route pointer input over the open inspector panel (B7): the wheel scrolls it, and a
    /// primary press, drag and release go to its scrollbar. `None` when the inspector is closed
    /// or the event is neither over the panel nor part of a scrollbar drag that started there.
    pub(super) fn inspector_mouse(&self, mouse: MouseEvent, top_hit: Option<&str>) -> Option<bool> {
        let panel = self.tools.inspector.as_ref()?;
        let over = top_hit == Some(INSPECTOR_HIT_ID);
        let dragging = panel.dragging();
        match mouse.kind {
            MouseEventKind::ScrollUp if over => Some(panel.scroll_by(-INSPECTOR_SCROLL_ROWS)),
            MouseEventKind::ScrollDown if over => Some(panel.scroll_by(INSPECTOR_SCROLL_ROWS)),
            MouseEventKind::Down(MouseButton::Left) if over => Some(panel_scrollbar(panel, mouse)),
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
                if dragging =>
            {
                Some(panel_scrollbar(panel, mouse))
            }
            _ if over => Some(false),
            _ => None,
        }
    }

    /// Paint the open inspector over the transcript viewport of the last painted frame, after
    /// the transcript (FO6). The panel occludes what it covers and registers its hit region
    /// above transcript regions; the body geometry is recorded for scrollbar input. The
    /// inspection itself is prepared in the update path ([`Self::refresh_inspector`]).
    pub fn paint_inspector(
        &mut self,
        cx: &mut PaintCx<'_, '_>,
        theme: TuiTheme,
        component_theme: &bmux_tui_components::theme::ComponentTheme,
    ) {
        let placement = self
            .regions
            .viewport
            .and_then(inspector_placement)
            .map(InspectorPlacement::area);
        let Some(panel) = self.tools.inspector.as_mut() else {
            return;
        };
        // Painting reads the inspection the update path prepared and only records the painted
        // body geometry that scrollbar input routes through (the ScrollView contract).
        let Some(area) = placement else {
            panel.stop_drag();
            panel.painted = None;
            return;
        };
        let lines = report_lines(panel.report.as_ref(), theme);
        panel.painted = paint_panel(area, &lines, &panel.scroll, cx, theme, component_theme);
        if panel.painted.is_none() {
            panel.stop_drag();
        }
    }

    /// Make the focused tool unit's current disclosure the default for its tool (`m`). The
    /// rule is queued for the host shell, which persists it to user state; refusals become
    /// notices. Returns whether the UI changed.
    pub(super) fn make_focused_default(&mut self) -> bool {
        match self.engine.focused_default_rule() {
            Ok(rule) => {
                self.tools
                    .default_rules
                    .retain(|queued| queued.matcher != rule.matcher);
                self.tools.default_rules.push(rule);
                let excess = self
                    .tools
                    .default_rules
                    .len()
                    .saturating_sub(MAX_PENDING_DEFAULTS);
                self.tools.default_rules.drain(..excess);
            }
            Err(DefaultRuleRefusal::NotAUnit) => self.notify(ToolNotice::DefaultNeedsUnit),
            Err(DefaultRuleRefusal::NotATool) => self.notify(ToolNotice::DefaultNeedsTool),
        }
        true
    }

    /// Cycle detail (`d`, `alt+d`, `transcript.cycleDetail`): the focused target with
    /// transcript focus, every unit otherwise. Returns whether the UI changed.
    pub(super) fn cycle_detail(&mut self, at: std::time::Instant) -> bool {
        let (update, cycled) = self.engine.cycle_detail(at);
        self.absorb(update);
        match cycled {
            DetailCycle::Changed(disclosure) => {
                if !self.transcript_focus_active() {
                    self.notify(ToolNotice::Disclosure {
                        focused: false,
                        disclosure,
                    });
                }
                true
            }
            DetailCycle::Refused => {
                self.notify(ToolNotice::FloorKeepsOpen);
                true
            }
            DetailCycle::Unchanged => false,
        }
    }

    /// Expand or collapse everything, or the focused subtree (`tools.expandAll`,
    /// `tools.collapseAll`). Returns whether anything changed.
    pub(super) fn set_all_disclosure(
        &mut self,
        disclosure: Disclosure,
        at: std::time::Instant,
    ) -> bool {
        let focused = self.transcript_focus_active();
        let (update, changed) = self.engine.set_all_disclosure(disclosure, at);
        self.absorb(update);
        if changed {
            self.notify(ToolNotice::Disclosure {
                focused,
                disclosure,
            });
        }
        changed
    }

    /// Drop manual disclosure, or the focused subtree's (`tools.resetDisclosure`). Returns
    /// whether anything changed.
    pub(super) fn reset_disclosure(&mut self, at: std::time::Instant) -> bool {
        let focused = self.transcript_focus_active();
        let (update, changed) = self.engine.reset_disclosure(at);
        self.absorb(update);
        if changed {
            self.notify(ToolNotice::Reset { focused });
        }
        changed
    }

    /// `e` on a focused shell screen toggles its inline expansion (C39); opening it also
    /// expands the focused subtree. Returns `None` when the focused unit has no shell screen.
    pub(super) fn toggle_focused_shell(&mut self, at: std::time::Instant) -> Option<bool> {
        let (update, expanded) = self.engine.toggle_focused_shell(at);
        self.absorb(update);
        let expanded = expanded?;
        self.notify(ToolNotice::Shell { expanded });
        Some(true)
    }
}

/// Route a primary-button event to the panel's scrollbar through the painted body geometry.
fn panel_scrollbar(panel: &InspectorPanel, mouse: MouseEvent) -> bool {
    let Some(painted) = panel.painted.as_ref() else {
        let dragging = panel.dragging();
        panel.stop_drag();
        return dragging;
    };
    let was_dragging = panel.dragging();
    let outcome = body_view(&[], &panel.scroll).handle_event(
        painted.area,
        &painted.layout,
        &Event::Mouse(mouse),
    );
    outcome != ScrollViewOutcome::Ignored || was_dragging || panel.dragging()
}

/// Property rows of `inspection`, the section's own rows first for a section target.
#[must_use]
pub fn inspection_properties(inspection: &Inspection) -> Vec<InspectedProperty> {
    let mut properties = property_rows(
        &inspection.explained.presentation,
        &inspection.explained.origins,
    );
    properties.truncate(MAX_REPORT_ROWS);
    if let DisclosureTarget::Section(_, name) = &inspection.target {
        let prefix = format!("sections.{name}.");
        properties.sort_by_key(|row| !row.property.starts_with(&prefix));
    }
    properties
}

/// Inspector content rows for `report` (the rows the V1 inspector shows), or a placeholder
/// when nothing structural is focused. Rows are semantic lines; wrapping is BMUX's.
#[must_use]
pub fn report_lines(report: Option<&Inspection>, theme: TuiTheme) -> Vec<Line> {
    let heading = |text: &str| {
        Line::from_spans(vec![Span::styled(
            text.to_owned(),
            theme.focused.add_modifier(Modifier::BOLD),
        )])
    };
    let Some(report) = report else {
        return vec![
            heading("Inspector"),
            Line::from_spans(vec![Span::styled(
                "The focused target is not a structural unit.",
                theme.muted,
            )]),
        ];
    };
    let pair = |name: String, value: String, origin: Option<String>| {
        let mut spans = vec![
            Span::styled(name, theme.muted),
            Span::styled(" ", theme.text),
            Span::styled(value, theme.text),
        ];
        if let Some(origin) = origin {
            spans.push(Span::styled(format!(" · {origin}"), theme.info));
        }
        Line::from_spans(spans)
    };
    let mut rows = vec![
        Line::from_spans(vec![
            Span::styled("Inspector · ", theme.focused.add_modifier(Modifier::BOLD)),
            Span::styled(
                bounded_text(report.target.to_string()),
                theme.text.add_modifier(Modifier::BOLD),
            ),
        ]),
        pair(
            "shown".to_owned(),
            disclosure_name(report.effective).to_owned(),
            report
                .manual
                .map(|manual| format!("manual {}", disclosure_name(manual))),
        ),
        Line::default(),
        heading("Facts"),
    ];
    rows.extend(
        fact_rows(&report.facts)
            .into_iter()
            .map(|(name, value)| pair(name, value, None)),
    );
    rows.push(Line::default());
    rows.push(heading("Properties"));
    rows.extend(inspection_properties(report).iter().map(|row| {
        pair(
            row.property.clone(),
            value_text(&row.value),
            Some(origin_label(row.origin.as_ref())),
        )
    }));
    let note_rows = |notes: &[String], style| {
        notes
            .iter()
            .take(MAX_REPORT_ROWS)
            .map(move |note| {
                Line::from_spans(vec![Span::styled(bounded_text(note.clone()), style)])
            })
            .collect::<Vec<_>>()
    };
    if !report.explained.floor.is_empty() {
        rows.push(Line::default());
        rows.push(heading("Floor and overrides"));
        rows.extend(note_rows(&report.explained.floor, theme.warning));
    }
    if !report.notes.is_empty() {
        rows.push(Line::default());
        rows.push(heading("Grouping"));
        rows.extend(note_rows(&report.notes, theme.text));
    }
    rows
}

#[cfg(test)]
mod tests;
