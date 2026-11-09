//! Lazy BMUX item source over transcript units (VP6, BMUX 6).
//!
//! The source exposes units in document order with their layout revisions; it builds a unit's
//! component only when BMUX asks (measurement of a new or stale unit, or painting a visible
//! one). Boundary state is a paint revision, never a layout revision (U3, L5).

use bcode_session_view_models::TranscriptViewItem;
use bmux_tui::component::Element;
use bmux_tui_components::virtual_list::VirtualItemSource;

use crate::units::producers::{UnitContent, UnitProducer};
use crate::units::{UnitKey, UnitKind, Units};
use crate::window::{BoundaryStatus, ResidentWindow};

/// `VirtualItemSource` over the current units.
pub struct TranscriptItemSource<'a> {
    units: &'a Units,
    items: &'a [TranscriptViewItem],
    window: &'a ResidentWindow,
    producer: &'a dyn UnitProducer,
}

impl std::fmt::Debug for TranscriptItemSource<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TranscriptItemSource")
            .field("units", &self.units.len())
            .field("items", &self.items.len())
            .finish_non_exhaustive()
    }
}

impl<'a> TranscriptItemSource<'a> {
    /// Create a source over units derived from `items`.
    #[must_use]
    pub const fn new(
        units: &'a Units,
        items: &'a [TranscriptViewItem],
        window: &'a ResidentWindow,
        producer: &'a dyn UnitProducer,
    ) -> Self {
        Self {
            units,
            items,
            window,
            producer,
        }
    }

    /// Everything the producer reads for one unit, if the unit exists.
    #[must_use]
    pub fn content(&self, key: &UnitKey) -> Option<UnitContent<'a>> {
        let units: &'a Units = self.units;
        let unit = units.entry(key)?;
        // Logarithmic per unit: building a visible unit never touches every resident item
        // (spec 9.2, no O(resident units) work in prepare or paint).
        let item = key.item_id().and_then(|id| units.item(self.items, id));
        let boundary = match unit.kind {
            UnitKind::Boundary(edge) => Some(self.window.boundary_status(edge)),
            _ => None,
        };
        Some(UnitContent {
            unit,
            item,
            boundary,
        })
    }

    /// Exact height of a unit at `width` (0 for an unknown unit).
    #[must_use]
    pub fn measure(&self, key: &UnitKey, width: u64) -> u64 {
        self.content(key)
            .map_or(0, |content| self.producer.measure(content, width))
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
        match self
            .units
            .entry(&self.units.keys()[index])
            .map(|unit| unit.kind)
        {
            Some(UnitKind::Boundary(edge)) => status_revision(self.window.boundary_status(edge)),
            _ => 0,
        }
    }

    fn build(&self, index: usize) -> Element<'_> {
        let key = &self.units.keys()[index];
        let content = self
            .content(key)
            .expect("source index names a current unit");
        self.producer.component(content)
    }
}
