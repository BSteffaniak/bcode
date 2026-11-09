//! Composer/transcript keyboard focus (spec section 6, rules FO1–FO10).
//!
//! Focus targets are the focus targets of each unit's retained layout (FO7, B12), walked in
//! document order and, inside a unit, in layout order. Traversal is a pure function of units and
//! retained layouts ([`FocusView`]); the engine owns the [`FocusState`] and turns focus moves
//! into `FocusTarget` navigation intents (C32).

use std::ops::Range;

use crate::layout::ItemLayoutCache;
use crate::layout::geometry::ItemFocusTarget;
use crate::units::structure::DisclosureTarget;
use crate::units::{UnitKey, Units};
use crate::viewport::ViewportFrame;

/// Maximum units one traversal step visits (bounded interactive work).
pub const MAX_SCAN_UNITS: usize = 4096;

/// One focus target inside a unit.
///
/// Targets are derived from the unit's retained layout (FO7, B12). `local` is the target's
/// position among the unit's focus targets in layout order; `0` is the unit itself.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FocusTargetId {
    /// Owning unit.
    pub unit: UnitKey,
    /// Target within the unit (`0`: the unit itself).
    pub local: u32,
}

impl FocusTargetId {
    /// The unit-level target of a unit.
    #[must_use]
    pub const fn unit(unit: UnitKey) -> Self {
        Self { unit, local: 0 }
    }
}

/// Where keyboard focus returns when transcript focus ends (FO3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ReturnFocus {
    /// The composer.
    #[default]
    Composer,
}

/// Which surface has keyboard focus (FO1). Ephemeral; never persisted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub enum ChatFocus {
    /// The composer (default).
    #[default]
    Composer,
    /// Transcript unit focus (`alt+u`).
    Transcript {
        /// Focused target.
        target: FocusTargetId,
        /// Where focus returns.
        return_to: ReturnFocus,
    },
}

impl ChatFocus {
    /// The focused transcript target, if the transcript has focus.
    #[must_use]
    pub const fn transcript_target(&self) -> Option<&FocusTargetId> {
        match self {
            Self::Composer => None,
            Self::Transcript { target, .. } => Some(target),
        }
    }
}

/// Direction of one focus step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FocusStep {
    /// The next target in document order.
    Next,
    /// The previous target in document order.
    Previous,
}

/// Which end of the resident window focus moves to once a direct load lands (FO4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FocusEdge {
    /// The first target.
    First,
    /// The last target.
    Last,
}

/// Engine-owned focus state: the [`ChatFocus`] plus the identities that let focus survive
/// re-derivation (FO8).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FocusState {
    chat: ChatFocus,
    /// Disclosure toggled by the focused target, the stable identity across regrouping.
    disclosure: Option<DisclosureTarget>,
    /// Document index of the focused unit when it was last located.
    index_hint: Option<usize>,
    /// Focus moves to this end of the window once the pending direct load lands (FO4).
    pending_edge: Option<FocusEdge>,
}

impl FocusState {
    /// Composer/transcript focus.
    #[must_use]
    pub const fn chat(&self) -> &ChatFocus {
        &self.chat
    }

    /// Focused transcript target.
    #[must_use]
    pub const fn target(&self) -> Option<&FocusTargetId> {
        self.chat.transcript_target()
    }

    /// Disclosure of the focused target, if it toggles one.
    #[must_use]
    pub const fn disclosure(&self) -> Option<&DisclosureTarget> {
        match self.chat {
            ChatFocus::Composer => None,
            ChatFocus::Transcript { .. } => self.disclosure.as_ref(),
        }
    }

    /// Edge focus moves to when the pending direct load lands.
    #[must_use]
    pub const fn pending_edge(&self) -> Option<FocusEdge> {
        self.pending_edge
    }

    /// Request focus at `edge` once the pending load lands.
    pub const fn set_pending_edge(&mut self, edge: Option<FocusEdge>) {
        self.pending_edge = edge;
    }

    /// Focus `target` (entering transcript focus when the composer had it). Returns whether
    /// the focus changed.
    pub fn focus(&mut self, view: &FocusView<'_>, target: FocusTargetId) -> bool {
        if self.chat.transcript_target() == Some(&target) {
            return false;
        }
        self.disclosure = view
            .target(&target)
            .and_then(|found| found.disclosure.clone());
        self.index_hint = view.units.index_of(&target.unit);
        self.chat = ChatFocus::Transcript {
            target,
            return_to: ReturnFocus::Composer,
        };
        true
    }

    /// Return focus to the composer (FO3). Returns whether the transcript had focus.
    pub fn release(&mut self) -> bool {
        let had = matches!(self.chat, ChatFocus::Transcript { .. });
        *self = Self::default();
        had
    }

    /// Keep focus on a resident target after units changed (FO8): the same target if it
    /// survives, the unit now owning the focused disclosure, else the nearest surviving
    /// target at the last known document position. Focus never forces paging; with no target
    /// left focus returns to the composer. Returns whether the focus changed.
    pub fn reconcile(&mut self, view: &FocusView<'_>) -> bool {
        let Some(current) = self.chat.transcript_target().cloned() else {
            return false;
        };
        if view.target(&current).is_some() {
            self.index_hint = view.units.index_of(&current.unit);
            return false;
        }
        let by_disclosure = self
            .disclosure
            .as_ref()
            .and_then(|disclosure| view.target_for_disclosure(disclosure));
        // P4/U2: a member folded into a group follows its group header or hidden marker.
        let by_owner = || {
            let owner = view.units.owner_unit(current.unit.item_id()?)?;
            view.unit_target_of(owner, false)
        };
        let replacement = by_disclosure.or_else(by_owner).or_else(|| {
            let index = self.index_hint.unwrap_or(0);
            view.nearest_from(index)
        });
        match replacement {
            Some(target) => self.focus(view, target),
            None => self.release(),
        }
    }
}

/// Read-only traversal over units and their retained layouts.
#[derive(Debug, Clone, Copy)]
pub struct FocusView<'a> {
    /// Units in document order.
    pub units: &'a Units,
    /// Retained layouts (focus targets, B12).
    pub layouts: &'a ItemLayoutCache,
}

impl<'a> FocusView<'a> {
    /// A view over `units` and `layouts`.
    #[must_use]
    pub const fn new(units: &'a Units, layouts: &'a ItemLayoutCache) -> Self {
        Self { units, layouts }
    }

    /// Focus targets of `unit`'s retained layout, in layout order.
    #[must_use]
    pub fn targets(&self, unit: &UnitKey) -> &'a [ItemFocusTarget] {
        if unit.is_boundary() {
            return &[];
        }
        let layouts: &'a ItemLayoutCache = self.layouts;
        layouts
            .current(unit)
            .map_or(&[], |layout| layout.geometry.focus_targets.as_slice())
    }

    /// The retained target for `id`, if it still exists.
    #[must_use]
    pub fn target(&self, id: &FocusTargetId) -> Option<&'a ItemFocusTarget> {
        if !self.units.contains(&id.unit) {
            return None;
        }
        self.targets(&id.unit)
            .iter()
            .find(|target| target.local == id.local)
    }

    /// Item-local rows of a target.
    #[must_use]
    pub fn rows(&self, id: &FocusTargetId) -> Option<Range<u64>> {
        self.target(id).map(|target| target.rows.clone())
    }

    /// Item-local rows a target governs (C32, T19): from its first row to the first row of
    /// the next target in layout order that is not nested under it, or to the item's last row.
    /// An item's own target therefore spans the item and a section target spans its section,
    /// derived from the same retained rows as the target itself (B12).
    #[must_use]
    pub fn extent(&self, id: &FocusTargetId) -> Option<Range<u64>> {
        let target = self.target(id)?;
        let height = self.layouts.current(&id.unit)?.height;
        let targets = self.targets(&id.unit);
        let nested_under = |candidate: &ItemFocusTarget| {
            let mut parent = candidate.parent;
            // Parents precede their children in layout order, so the chain is bounded.
            for _ in 0..targets.len() {
                match parent {
                    Some(local) if local == id.local => return true,
                    Some(local) => {
                        parent = targets
                            .iter()
                            .find(|target| target.local == local)
                            .and_then(|target| target.parent);
                    }
                    None => return false,
                }
            }
            false
        };
        let end = targets
            .iter()
            .skip_while(|candidate| candidate.local != id.local)
            .skip(1)
            .find(|candidate| !nested_under(candidate))
            .map_or(height, |next| next.rows.start);
        Some(target.rows.start..end.max(target.rows.end))
    }

    /// The target that toggles `disclosure`.
    #[must_use]
    pub fn target_for_disclosure(&self, disclosure: &DisclosureTarget) -> Option<FocusTargetId> {
        self.units.keys().iter().find_map(|unit| {
            self.targets(unit)
                .iter()
                .find(|target| target.disclosure.as_ref() == Some(disclosure))
                .map(|target| FocusTargetId {
                    unit: unit.clone(),
                    local: target.local,
                })
        })
    }

    /// First target of the unit at `index` in layout order, or its last for `last`.
    fn unit_target(&self, index: usize, last: bool) -> Option<FocusTargetId> {
        self.unit_target_of(self.units.keys().get(index)?, last)
    }

    /// First target of `unit` in layout order, or its last for `last`.
    fn unit_target_of(&self, unit: &UnitKey, last: bool) -> Option<FocusTargetId> {
        let targets = self.targets(unit);
        let target = if last {
            targets.last()
        } else {
            targets.first()
        }?;
        Some(FocusTargetId {
            unit: unit.clone(),
            local: target.local,
        })
    }

    /// The target `step` moves to from `current`, bounded by [`MAX_SCAN_UNITS`]. `None` at
    /// either end of the resident window (focus never forces paging, FO8).
    #[must_use]
    pub fn step(&self, current: &FocusTargetId, step: FocusStep) -> Option<FocusTargetId> {
        let index = self.units.index_of(&current.unit)?;
        let targets = self.targets(&current.unit);
        let position = targets
            .iter()
            .position(|target| target.local == current.local)?;
        let within = match step {
            FocusStep::Next => targets.get(position + 1),
            FocusStep::Previous => position.checked_sub(1).and_then(|at| targets.get(at)),
        };
        if let Some(target) = within {
            return Some(FocusTargetId {
                unit: current.unit.clone(),
                local: target.local,
            });
        }
        match step {
            FocusStep::Next => (index + 1..self.units.len())
                .take(MAX_SCAN_UNITS)
                .find_map(|at| self.unit_target(at, false)),
            FocusStep::Previous => (0..index)
                .rev()
                .take(MAX_SCAN_UNITS)
                .find_map(|at| self.unit_target(at, true)),
        }
    }

    /// The first or last target of the resident window.
    #[must_use]
    pub fn edge(&self, edge: FocusEdge) -> Option<FocusTargetId> {
        match edge {
            FocusEdge::First => (0..self.units.len())
                .take(MAX_SCAN_UNITS)
                .find_map(|at| self.unit_target(at, false)),
            FocusEdge::Last => (0..self.units.len())
                .rev()
                .take(MAX_SCAN_UNITS)
                .find_map(|at| self.unit_target(at, true)),
        }
    }

    /// The nearest target to document position `index`: the first at or after it, else the
    /// last before it.
    #[must_use]
    pub fn nearest_from(&self, index: usize) -> Option<FocusTargetId> {
        let len = self.units.len();
        let start = index.min(len);
        (start..len)
            .take(MAX_SCAN_UNITS)
            .find_map(|at| self.unit_target(at, false))
            .or_else(|| {
                (0..start)
                    .rev()
                    .take(MAX_SCAN_UNITS)
                    .find_map(|at| self.unit_target(at, true))
            })
    }

    /// Initial transcript focus (C33): the last target whose first row is visible in `frame`,
    /// else the nearest target above the viewport, else the first target below it.
    #[must_use]
    pub fn initial(&self, frame: Option<&ViewportFrame>) -> Option<FocusTargetId> {
        let Some(frame) = frame else {
            return self.edge(FocusEdge::Last);
        };
        let height = i64::try_from(frame.resolved.viewport_height).unwrap_or(i64::MAX);
        let visible = frame.units.iter().rev().find_map(|unit| {
            self.targets(&unit.key)
                .iter()
                .rev()
                .find(|target| {
                    let row = unit
                        .top
                        .saturating_add(i64::try_from(target.rows.start).unwrap_or(i64::MAX));
                    (0..height).contains(&row)
                })
                .map(|target| FocusTargetId {
                    unit: unit.key.clone(),
                    local: target.local,
                })
        });
        if visible.is_some() {
            return visible;
        }
        let first = frame.units.first()?;
        // Targets of the first visible unit that start above the viewport come before every
        // earlier unit's targets.
        let above_in_first = self
            .targets(&first.key)
            .iter()
            .rev()
            .find(|target| {
                first
                    .top
                    .saturating_add(i64::try_from(target.rows.start).unwrap_or(i64::MAX))
                    < 0
            })
            .map(|target| FocusTargetId {
                unit: first.key.clone(),
                local: target.local,
            });
        above_in_first
            .or_else(|| {
                (0..first.index)
                    .rev()
                    .take(MAX_SCAN_UNITS)
                    .find_map(|at| self.unit_target(at, true))
            })
            .or_else(|| self.nearest_from(first.index))
    }

    /// The enclosing target of `current` inside its unit, if nested.
    #[must_use]
    pub fn parent(&self, current: &FocusTargetId) -> Option<FocusTargetId> {
        let parent = self.target(current)?.parent?;
        self.targets(&current.unit)
            .iter()
            .any(|target| target.local == parent)
            .then(|| FocusTargetId {
                unit: current.unit.clone(),
                local: parent,
            })
    }

    /// Disclosures of `current` and every target nested under it in its unit, outermost first.
    #[must_use]
    pub fn subtree_disclosures(&self, current: &FocusTargetId) -> Vec<DisclosureTarget> {
        let targets = self.targets(&current.unit);
        let descends = |mut local: u32| {
            for _ in 0..targets.len() {
                if local == current.local {
                    return true;
                }
                let Some(parent) = targets
                    .iter()
                    .find(|target| target.local == local)
                    .and_then(|target| target.parent)
                else {
                    return false;
                };
                local = parent;
            }
            false
        };
        targets
            .iter()
            .filter(|target| descends(target.local))
            .filter_map(|target| target.disclosure.clone())
            .collect()
    }
}
