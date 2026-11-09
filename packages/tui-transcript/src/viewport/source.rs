//! Lazy BMUX item source over transcript units and their retained layouts (VP6, BMUX 6).
//!
//! The source never lays anything out: [`VirtualItemSource::retained_layout`] hands BMUX the
//! node the [`ItemLayoutCache`] measured, and [`VirtualItemSource::build`] wraps the retained
//! component for painting and event routing. Boundary state and paint slots are paint
//! revisions, never layout revisions (U3, L5).

use bmux_tui::component::{Element, LayoutEnvironment, LayoutNode};
use bmux_tui_components::virtual_list::VirtualItemSource;

use crate::layout::producers::common::{StatusRow, boundary_row};
use crate::layout::{ActivityPhase, ItemLayout, ItemLayoutCache, RetainedItem};
use crate::units::{UnitKey, UnitKind, Units};
use crate::window::{BoundaryStatus, ResidentWindow};

/// `VirtualItemSource` over the current units and the layout cache.
pub struct TranscriptItemSource<'a> {
    units: &'a Units,
    window: &'a ResidentWindow,
    layouts: &'a ItemLayoutCache,
    activity: ActivityPhase,
}

impl std::fmt::Debug for TranscriptItemSource<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TranscriptItemSource")
            .field("units", &self.units.len())
            .field("layouts", &self.layouts.len())
            .finish_non_exhaustive()
    }
}

impl<'a> TranscriptItemSource<'a> {
    /// Create a source over `units` painted from `layouts`.
    #[must_use]
    pub const fn new(
        units: &'a Units,
        window: &'a ResidentWindow,
        layouts: &'a ItemLayoutCache,
        activity: ActivityPhase,
    ) -> Self {
        Self {
            units,
            window,
            layouts,
            activity,
        }
    }

    /// The retained layout of a unit if it is current for the unit's revision and `width`.
    #[must_use]
    pub fn layout(&self, key: &UnitKey, width: u64) -> Option<&'a ItemLayout> {
        let layouts: &'a ItemLayoutCache = self.layouts;
        layouts.current(key).filter(|layout| {
            layout.key.width == width && Some(layout.key.revision) == self.units.revision(key)
        })
    }
}

const fn status_revision(status: BoundaryStatus) -> u64 {
    match status {
        BoundaryStatus::MoreAvailable => 1,
        BoundaryStatus::Loading => 2,
        BoundaryStatus::Exhausted => 3,
    }
}

impl VirtualItemSource<UnitKey> for TranscriptItemSource<'_> {
    fn len(&self) -> usize {
        self.units.len()
    }

    fn key(&self, index: usize) -> UnitKey {
        self.units.keys()[index].clone()
    }

    fn layout_revision(&self, index: usize) -> u64 {
        self.units.revision(&self.units.keys()[index]).unwrap_or(0)
    }

    fn paint_revision(&self, index: usize) -> u64 {
        let key = &self.units.keys()[index];
        match self.units.entry(key).map(|unit| unit.kind) {
            Some(UnitKind::Boundary(edge)) => status_revision(self.window.boundary_status(edge)),
            _ => self
                .layouts
                .current(key)
                .filter(|layout| !layout.geometry.paint_slots.is_empty())
                .map_or(0, |_| {
                    self.activity
                        .spinner_frame
                        .wrapping_add(self.activity.now_ms.unwrap_or(0))
                }),
        }
    }

    fn build(&self, index: usize) -> Element<'_> {
        let key = &self.units.keys()[index];
        if let Some(UnitKind::Boundary(edge)) = self.units.entry(key).map(|unit| unit.kind) {
            // Paint-only status over the retained one-row layout (U3).
            return Element::new(boundary_row(edge, self.window.boundary_status(edge)));
        }
        self.layouts.current(key).map_or_else(
            // Never reached for resident units: every unit is measured in the update that made
            // it resident (L9). An empty row keeps an inconsistent frame paintable.
            || Element::new(StatusRow::new(bmux_tui::text::Line::raw(""))),
            |layout| Element::new(RetainedItem::new(layout, self.activity)),
        )
    }

    fn retained_layout(
        &self,
        index: usize,
        width: u64,
        _environment: LayoutEnvironment,
    ) -> Option<&LayoutNode> {
        self.layout(&self.units.keys()[index], width)
            .map(|layout| &layout.node)
    }
}
