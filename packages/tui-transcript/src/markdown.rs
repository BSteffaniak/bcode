//! Keyboard focus and activation of Markdown contributions (links, GitHub issue references,
//! footnote references and definitions, `<details>` blocks and Mermaid diagrams).
//!
//! Markdown focus is a highlighted contribution, not keyboard focus (FO1): the composer or the
//! transcript keeps keyboard focus while `tui.markdown.focusNext` / `focusPrevious` move the
//! highlight and `tui.markdown.activate` / `copyDestination` act on it, from either focus.
//!
//! Targets are the [`MarkdownTarget`]s of each unit's retained layout (FO7, B12), walked in
//! document order and, inside a unit, in row-then-column order. Traversal is a pure function
//! of units and retained layouts ([`MarkdownView`]) bounded to [`MAX_SCAN_UNITS`] units per
//! step, and wraps at the ends of the resident window; focus never forces paging (FO8). The
//! engine owns the [`MarkdownFocusState`] and reveals a newly focused target through the
//! `FocusTarget` navigation intent (BMUX focus reveal, C32).

use std::collections::BTreeMap;

use bcode_markdown_render::MarkdownDestination;

use crate::focus::{FocusStep, MAX_SCAN_UNITS};
use crate::layout::ItemLayoutCache;
use crate::layout::producers::markdown::{MarkdownAnchor, MarkdownTarget};
use crate::units::{UnitKey, Units};
use crate::viewport::ViewportFrame;

/// Identity of one Markdown target: its unit and owner-qualified contribution id.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MarkdownTargetId {
    /// Owning unit.
    pub unit: UnitKey,
    /// Contribution id (`transcript:<unit>:...`), stable across re-projection of the same
    /// source.
    pub contribution: String,
}

impl MarkdownTargetId {
    /// The target `contribution` of `unit`.
    #[must_use]
    pub fn new(unit: UnitKey, contribution: impl Into<String>) -> Self {
        Self {
            unit,
            contribution: contribution.into(),
        }
    }
}

/// What activating a Markdown target asks of the host, or what it did inside the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownActivation {
    /// Toggle a `<details>` block: the host stores `open` under `key` in the render
    /// configuration's `<details>` state, which re-measures only the owning unit (L3).
    Details {
        /// `<details>` state key (the contribution id).
        key: String,
        /// The new open state.
        open: bool,
    },
    /// Toggle the Mermaid source view of the contribution.
    Mermaid {
        /// Diagram source without the fence.
        source: String,
    },
    /// A footnote reference was activated: its definition was focused and revealed (with a
    /// return target recorded), or it is not resident.
    FootnoteDefinition {
        /// Whether the definition was focused.
        focused: bool,
    },
    /// A footnote definition was activated: focus returned to the reference that jumped to it
    /// (else its first reference), or no reference is resident.
    FootnoteReference {
        /// Whether a reference was focused.
        returned: bool,
    },
    /// A link or GitHub issue reference: the host opens the classified destination (or
    /// navigates fragments through [`crate::engine::TranscriptEngine::navigate_markdown_fragment`]).
    Link(MarkdownDestination),
    /// No such resident target.
    Unavailable,
}

/// Engine-owned Markdown focus: the focused target plus what lets it survive re-derivation
/// (FO8) and the footnote return targets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MarkdownFocusState {
    focused: Option<MarkdownTargetId>,
    /// Document index of the focused unit and the target's position in it, when last located.
    hint: Option<(usize, usize)>,
    /// Reference a jump to a footnote definition came from, by definition.
    returns: BTreeMap<MarkdownTargetId, MarkdownTargetId>,
}

impl MarkdownFocusState {
    /// The focused target.
    #[must_use]
    pub const fn focused(&self) -> Option<&MarkdownTargetId> {
        self.focused.as_ref()
    }

    /// Focus `target`. Returns whether the focus changed.
    pub fn focus(&mut self, view: &MarkdownView<'_>, target: MarkdownTargetId) -> bool {
        self.hint = view.locate(&target);
        if self.focused.as_ref() == Some(&target) {
            return false;
        }
        self.focused = Some(target);
        true
    }

    /// Record that a jump to footnote `definition` came from `reference`.
    pub fn record_return(&mut self, definition: MarkdownTargetId, reference: MarkdownTargetId) {
        self.returns.insert(definition, reference);
    }

    /// Take the recorded return target of `definition`.
    pub fn take_return(&mut self, definition: &MarkdownTargetId) -> Option<MarkdownTargetId> {
        self.returns.remove(definition)
    }

    /// Keep focus on a resident target after units or layouts changed (FO8): the same target
    /// if it survives, else the target at the same position of the same unit, else the
    /// nearest target from the last known document position; with none left the focus is
    /// cleared. Return targets whose ends are gone are dropped. Returns whether the focus
    /// changed.
    pub fn reconcile(&mut self, view: &MarkdownView<'_>) -> bool {
        self.returns.retain(|definition, reference| {
            view.target(definition).is_some() && view.target(reference).is_some()
        });
        let Some(current) = self.focused.clone() else {
            return false;
        };
        if let Some(found) = view.locate(&current) {
            self.hint = Some(found);
            return false;
        }
        let (index, position) = self.hint.unwrap_or_default();
        let replacement = view
            .units
            .index_of(&current.unit)
            .and_then(|_| view.at_position(&current.unit, position))
            .or_else(|| view.nearest_from(index));
        self.hint = replacement.as_ref().and_then(|target| view.locate(target));
        self.focused = replacement;
        true
    }
}

/// Read-only Markdown traversal over units and their retained layouts.
#[derive(Debug, Clone, Copy)]
pub struct MarkdownView<'a> {
    /// Units in document order.
    pub units: &'a Units,
    /// Retained layouts (Markdown targets, B12).
    pub layouts: &'a ItemLayoutCache,
}

impl<'a> MarkdownView<'a> {
    /// A view over `units` and `layouts`.
    #[must_use]
    pub const fn new(units: &'a Units, layouts: &'a ItemLayoutCache) -> Self {
        Self { units, layouts }
    }

    /// Markdown targets of `unit`'s retained layout, in document order.
    #[must_use]
    pub fn targets(&self, unit: &UnitKey) -> &'a [MarkdownTarget] {
        if unit.is_boundary() {
            return &[];
        }
        let layouts: &'a ItemLayoutCache = self.layouts;
        layouts
            .current(unit)
            .map_or(&[], |layout| layout.geometry.markdown.targets.as_slice())
    }

    /// Fragment anchors of `unit`'s retained layout.
    #[must_use]
    pub fn anchors(&self, unit: &UnitKey) -> &'a [MarkdownAnchor] {
        if unit.is_boundary() {
            return &[];
        }
        let layouts: &'a ItemLayoutCache = self.layouts;
        layouts
            .current(unit)
            .map_or(&[], |layout| layout.geometry.markdown.anchors.as_slice())
    }

    /// The retained target of `id`, if its unit is resident and still has it.
    #[must_use]
    pub fn target(&self, id: &MarkdownTargetId) -> Option<&'a MarkdownTarget> {
        if !self.units.contains(&id.unit) {
            return None;
        }
        self.targets(&id.unit)
            .iter()
            .find(|target| target.contribution_id == id.contribution)
    }

    /// Document index of `id`'s unit and the target's position in it.
    fn locate(&self, id: &MarkdownTargetId) -> Option<(usize, usize)> {
        let index = self.units.index_of(&id.unit)?;
        let position = self
            .targets(&id.unit)
            .iter()
            .position(|target| target.contribution_id == id.contribution)?;
        Some((index, position))
    }

    /// The target at `position` of `unit` (clamped to its last), if it has any.
    fn at_position(&self, unit: &UnitKey, position: usize) -> Option<MarkdownTargetId> {
        let targets = self.targets(unit);
        let target = targets.get(position).or_else(|| targets.last())?;
        Some(MarkdownTargetId::new(
            unit.clone(),
            target.contribution_id.clone(),
        ))
    }

    /// First target of the unit at `index`, or its last for `last`.
    fn unit_target(&self, index: usize, last: bool) -> Option<MarkdownTargetId> {
        let unit = self.units.keys().get(index)?;
        let targets = self.targets(unit);
        let target = if last {
            targets.last()
        } else {
            targets.first()
        }?;
        Some(MarkdownTargetId::new(
            unit.clone(),
            target.contribution_id.clone(),
        ))
    }

    /// First target at or after unit `start`, bounded by [`MAX_SCAN_UNITS`].
    fn first_from(&self, start: usize) -> Option<MarkdownTargetId> {
        (start..self.units.len())
            .take(MAX_SCAN_UNITS)
            .find_map(|at| self.unit_target(at, false))
    }

    /// Last target at or before unit `end`, bounded by [`MAX_SCAN_UNITS`].
    fn last_until(&self, end: usize) -> Option<MarkdownTargetId> {
        (0..=end.min(self.units.len().checked_sub(1)?))
            .rev()
            .take(MAX_SCAN_UNITS)
            .find_map(|at| self.unit_target(at, true))
    }

    /// The nearest target to document position `index`: the first at or after it, else the
    /// last before it.
    #[must_use]
    pub fn nearest_from(&self, index: usize) -> Option<MarkdownTargetId> {
        self.first_from(index)
            .or_else(|| index.checked_sub(1).and_then(|end| self.last_until(end)))
    }

    /// The target `step` moves to from `current`, wrapping at either end of the resident
    /// window. Without a current target (or when it is gone) the walk starts in `frame`:
    /// [`FocusStep::Next`] takes the first target that starts on or below the viewport top,
    /// [`FocusStep::Previous`] the last one that starts above the viewport bottom. `None`
    /// when the resident window has no target.
    #[must_use]
    pub fn step(
        &self,
        current: Option<&MarkdownTargetId>,
        step: FocusStep,
        frame: Option<&ViewportFrame>,
    ) -> Option<MarkdownTargetId> {
        let located = current.and_then(|current| Some((current, self.locate(current)?)));
        let Some((current, (index, position))) = located else {
            return self.start_in_frame(step, frame).or_else(|| self.wrap(step));
        };
        let targets = self.targets(&current.unit);
        let within = match step {
            FocusStep::Next => targets.get(position + 1),
            FocusStep::Previous => position.checked_sub(1).and_then(|at| targets.get(at)),
        };
        if let Some(target) = within {
            return Some(MarkdownTargetId::new(
                current.unit.clone(),
                target.contribution_id.clone(),
            ));
        }
        let beyond = match step {
            FocusStep::Next => self.first_from(index + 1),
            FocusStep::Previous => index.checked_sub(1).and_then(|end| self.last_until(end)),
        };
        beyond.or_else(|| self.wrap(step))
    }

    /// The first (or, moving back, last) target of the resident window.
    fn wrap(&self, step: FocusStep) -> Option<MarkdownTargetId> {
        match step {
            FocusStep::Next => self.first_from(0),
            FocusStep::Previous => self.last_until(self.units.len().checked_sub(1)?),
        }
    }

    /// The first target starting at or below the viewport top, or the last one starting above
    /// its bottom, in `frame`.
    fn start_in_frame(
        &self,
        step: FocusStep,
        frame: Option<&ViewportFrame>,
    ) -> Option<MarkdownTargetId> {
        let frame = frame?;
        let height = i64::try_from(frame.resolved.viewport_height).unwrap_or(i64::MAX);
        let starts = |top: i64, target: &MarkdownTarget| {
            top.saturating_add(i64::try_from(target.rows().start).unwrap_or(i64::MAX))
        };
        match step {
            FocusStep::Next => {
                let first = frame.units.first()?;
                self.targets(&first.key)
                    .iter()
                    .find(|target| starts(first.top, target) >= 0)
                    .map(|target| {
                        MarkdownTargetId::new(first.key.clone(), target.contribution_id.clone())
                    })
                    .or_else(|| self.first_from(first.index + 1))
            }
            FocusStep::Previous => {
                let last = frame.units.last()?;
                self.targets(&last.key)
                    .iter()
                    .rev()
                    .find(|target| starts(last.top, target) < height)
                    .map(|target| {
                        MarkdownTargetId::new(last.key.clone(), target.contribution_id.clone())
                    })
                    .or_else(|| {
                        last.index
                            .checked_sub(1)
                            .and_then(|end| self.last_until(end))
                    })
            }
        }
    }

    /// The unit and item-local row of fragment anchor `fragment`: in `preferred` first (the
    /// unit of the activated link), else the first resident unit in document order that has
    /// it, bounded by [`MAX_SCAN_UNITS`].
    #[must_use]
    pub fn anchor(&self, fragment: &str, preferred: Option<&UnitKey>) -> Option<(UnitKey, u64)> {
        let find = |unit: &UnitKey| {
            self.anchors(unit)
                .iter()
                .find(|anchor| anchor.fragment == fragment)
                .map(|anchor| (unit.clone(), anchor.row))
        };
        preferred
            .filter(|unit| self.units.contains(unit))
            .and_then(find)
            .or_else(|| self.units.keys().iter().take(MAX_SCAN_UNITS).find_map(find))
    }
}
