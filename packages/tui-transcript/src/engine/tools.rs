//! Transcript tooling entry points (spec section 6, FO6; C39; D10): bulk disclosure (expand,
//! collapse, reset, detail cycling of every unit), the structure inspector, per-tool defaults,
//! inline shell expansion and the inline interaction reservation.
//!
//! With transcript focus the bulk actions apply to the focused target's subtree, as in V1;
//! without it they apply to every resident unit and group. Every change goes through the
//! structure resolver and one content update, so anchors keep the reading position (P1, U2).

use std::time::Instant;

use bcode_presentation_policy::models::{Disclosure, PresentationRule, UnitKind};
use bcode_session_view_models::TranscriptChanges;

use crate::frame::FrameSnapshot;
use crate::units::UnitKey;
use crate::units::structure::disclosure::next_detail;
use crate::units::structure::{DisclosureTarget, InlineInteraction, Inspection};

use super::{EngineUpdate, TranscriptEngine};

/// Levels in the detail cycle (collapsed, summary, expanded).
const DETAIL_LEVELS: usize = 3;

/// Result of a detail cycle step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DetailCycle {
    /// Detail moved to this disclosure.
    Changed(Disclosure),
    /// The visibility floor refused every level for the focused target (A16).
    Refused,
    /// Nothing to cycle (no target, or nothing resident changes).
    Unchanged,
}

/// Why no per-tool default rule could be made from the focused target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DefaultRuleRefusal {
    /// No unit is focused (a section, group or nothing has focus).
    NotAUnit,
    /// The focused unit is not a tool unit with a tool name.
    NotATool,
}

/// Where the inline interaction surface sits in a prepared frame (D10), in viewport rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InlineInteractionPlacement {
    /// Viewport row of the reserved unit's first row (negative when it starts above).
    pub top: i64,
    /// Rows the unit reserves.
    pub height: u64,
}

impl TranscriptEngine {
    /// The disclosure the focused target toggles, if any.
    fn focused_disclosure_target(&self) -> Option<DisclosureTarget> {
        self.focus.disclosure().cloned()
    }

    /// The focused target and everything under it: a group's member units, and, when
    /// `with_sections`, the focused unit's nested section targets.
    fn focused_subtree(&self, with_sections: bool) -> Vec<DisclosureTarget> {
        let Some(root) = self.focused_disclosure_target() else {
            return Vec::new();
        };
        let mut subtree = vec![root.clone()];
        if let DisclosureTarget::Group(leader) = &root {
            subtree.extend(
                self.structure
                    .group_members(leader)
                    .iter()
                    .cloned()
                    .map(DisclosureTarget::Unit),
            );
        }
        if with_sections && let Some(target) = self.focus.target() {
            for nested in self.focus_view().subtree_disclosures(target) {
                if !subtree.contains(&nested) {
                    subtree.push(nested);
                }
            }
        }
        subtree
    }

    /// One content update after structure tooling changed (U2, P1).
    fn structure_changed(&mut self, at: Instant) -> EngineUpdate {
        self.content(TranscriptChanges::default(), at)
    }

    /// Expand or collapse everything (`tools.expandAll` / `tools.collapseAll`): with
    /// transcript focus the focused target and its subtree (sections follow only when
    /// expanding, so collapsing keeps which sections were open), otherwise every resident unit
    /// and group. The visibility floor still applies (A16). Returns the update and whether any
    /// disclosure changed.
    pub fn set_all_disclosure(
        &mut self,
        disclosure: Disclosure,
        at: Instant,
    ) -> (EngineUpdate, bool) {
        self.observe_time(at);
        let changed = if self.focus.target().is_some() {
            let mut changed = false;
            for target in self.focused_subtree(disclosure == Disclosure::Expanded) {
                if self.structure.effective_disclosure(&target) != Some(disclosure) {
                    changed |= self.structure.set_disclosure(&target, disclosure);
                }
            }
            changed
        } else {
            let changed = self.structure.set_all(disclosure);
            if changed {
                self.structure.note_detail_cycle(disclosure);
            }
            changed
        };
        if !changed {
            return (EngineUpdate::default(), false);
        }
        (self.structure_changed(at), true)
    }

    /// Drop manual disclosure overrides (`tools.resetDisclosure`): with transcript focus only
    /// those of the focused target's subtree, otherwise all. Returns the update and whether any
    /// override was dropped.
    pub fn reset_disclosure(&mut self, at: Instant) -> (EngineUpdate, bool) {
        self.observe_time(at);
        let changed = if self.focus.target().is_some() {
            let subtree = self.focused_subtree(false);
            if subtree.is_empty() {
                return (EngineUpdate::default(), false);
            }
            self.structure
                .clear_matching(|candidate| subtree.iter().any(|root| root.contains(candidate)))
        } else {
            self.structure.clear_overrides()
        };
        if !changed {
            return (EngineUpdate::default(), false);
        }
        (self.structure_changed(at), true)
    }

    /// Cycle detail collapsed → summary → expanded (`d`, `tui.transcript.cycleDetail`): the
    /// focused target with transcript focus, every unit and group otherwise. Levels the
    /// visibility floor refuses are skipped.
    pub fn cycle_detail(&mut self, at: Instant) -> (EngineUpdate, DetailCycle) {
        self.observe_time(at);
        if self.focus.target().is_some() {
            let Some(target) = self.focused_disclosure_target() else {
                return (EngineUpdate::default(), DetailCycle::Unchanged);
            };
            let Some(current) = self.structure.effective_disclosure(&target) else {
                return (EngineUpdate::default(), DetailCycle::Unchanged);
            };
            let mut next = next_detail(current);
            let mut update = EngineUpdate::default();
            for _ in 0..DETAIL_LEVELS {
                if next != current && self.structure.set_disclosure(&target, next) {
                    // The floor may keep the old level although the override was recorded;
                    // the next level is tried then.
                    update.merge(self.structure_changed(at));
                    if let Some(effective) = self.structure.effective_disclosure(&target)
                        && effective != current
                    {
                        return (update, DetailCycle::Changed(effective));
                    }
                }
                next = next_detail(next);
            }
            return (update, DetailCycle::Refused);
        }
        let start = self
            .structure
            .tools()
            .detail_cycle()
            .unwrap_or(Disclosure::Expanded);
        let mut next = next_detail(start);
        for _ in 0..DETAIL_LEVELS {
            if next != start && self.structure.set_all(next) {
                self.structure.note_detail_cycle(next);
                return (self.structure_changed(at), DetailCycle::Changed(next));
            }
            next = next_detail(next);
        }
        (EngineUpdate::default(), DetailCycle::Unchanged)
    }

    /// Structure inspection of `target` (FO6): how its presentation resolved.
    #[must_use]
    pub fn inspect(&self, target: &DisclosureTarget) -> Option<Inspection> {
        self.structure.inspect(target)
    }

    /// Structure inspection of the focused target, when it toggles a disclosure.
    #[must_use]
    pub fn inspect_focused(&self) -> Option<Inspection> {
        self.inspect(self.focus.disclosure()?)
    }

    /// The per-tool default rule `m` saves for the focused tool unit (FO6): kind `tool`, the
    /// unit's tool, and its current effective disclosure. The engine never persists it; the
    /// host writes it to user state (A3).
    ///
    /// # Errors
    ///
    /// * [`DefaultRuleRefusal::NotAUnit`] when the focused target is not a unit (or nothing is
    ///   focused).
    /// * [`DefaultRuleRefusal::NotATool`] when the focused unit is not a named tool unit.
    pub fn focused_default_rule(&self) -> Result<PresentationRule, DefaultRuleRefusal> {
        let Some(DisclosureTarget::Unit(id)) = self.focused_disclosure_target() else {
            return Err(DefaultRuleRefusal::NotAUnit);
        };
        let facts = self
            .structure
            .item_facts(&id)
            .ok_or(DefaultRuleRefusal::NotAUnit)?;
        let tool = facts
            .tool
            .clone()
            .filter(|_| facts.kind == Some(UnitKind::Tool))
            .ok_or(DefaultRuleRefusal::NotATool)?;
        let disclosure = self
            .structure
            .effective_disclosure(&DisclosureTarget::Unit(id))
            .ok_or(DefaultRuleRefusal::NotAUnit)?;
        let mut rule = PresentationRule::default();
        rule.matcher.kind.0.push(UnitKind::Tool);
        rule.matcher.tool.0.push(tool);
        rule.set.disclosure = Some(disclosure);
        Ok(rule)
    }

    /// Toggle the inline shell screen of the focused unit (C39): it opens expanded with up to
    /// 128 output rows, inside the transcript, and normal scrolling exposes the rows. Returns
    /// the update and the new state, or `None` when the focused unit has no shell screen.
    pub fn toggle_focused_shell(&mut self, at: Instant) -> (EngineUpdate, Option<bool>) {
        self.observe_time(at);
        let Some(DisclosureTarget::Unit(id) | DisclosureTarget::Section(id, _)) =
            self.focused_disclosure_target()
        else {
            return (EngineUpdate::default(), None);
        };
        if !self.structure.is_shell(&id) {
            return (EngineUpdate::default(), None);
        }
        let expanded = !self.structure.tools().shell_expanded(&id);
        if !self.structure.set_shell_expanded(&id, expanded) {
            return (EngineUpdate::default(), None);
        }
        (self.structure_changed(at), Some(expanded))
    }

    /// Reserve rows for the inline interaction surface in its unresolved interaction unit, or
    /// drop the reservation (`interactions.placement = transcript`, D10). The unit then
    /// measures `rows` blank rows the host paints the surface over; anchors keep the reading
    /// position (P1).
    pub fn set_inline_interaction(
        &mut self,
        interaction: Option<InlineInteraction>,
        at: Instant,
    ) -> EngineUpdate {
        self.observe_time(at);
        if !self.structure.set_inline_interaction(interaction) {
            return EngineUpdate::default();
        }
        self.structure_changed(at)
    }

    /// Where the reserved inline interaction unit sits in `snapshot`, while it is visible and
    /// its painted layout is current (B7: no placement for clipped-away or stale content).
    #[must_use]
    pub fn inline_interaction_placement(
        &self,
        snapshot: &FrameSnapshot,
    ) -> Option<InlineInteractionPlacement> {
        let item = self.structure.inline_interaction_item()?;
        let key = UnitKey::Item(item.clone());
        let unit = snapshot
            .viewport
            .as_ref()?
            .units
            .iter()
            .find(|unit| unit.key == key)?;
        self.painted_layout(snapshot, unit)?;
        Some(InlineInteractionPlacement {
            top: unit.top,
            height: unit.height,
        })
    }
}

#[cfg(test)]
mod tests;
