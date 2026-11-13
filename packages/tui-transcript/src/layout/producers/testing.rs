//! Deterministic producers for headless tests (feature `testing`).

use std::collections::BTreeMap;

use bmux_tui::composition::TextBlock;
use bmux_tui::text::{Line, Text, TextWrap};

use super::{ItemRenderContext, ProducedItem, UnitProducer, UnitView, common};
use crate::units::{UnitKey, UnitKind};
use crate::window::BoundaryStatus;

/// Plain text rows for every unit (the plain fixture rendering): word-wrapped message lines, kind
/// labels, one-row boundaries, headers and markers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlainTextProducer;

impl UnitProducer for PlainTextProducer {
    fn produce(&self, unit: UnitView<'_>, _cx: &ItemRenderContext<'_>) -> ProducedItem {
        if let UnitKind::Boundary(edge) = unit.unit.kind {
            return ProducedItem::new(common::boundary_row(edge, BoundaryStatus::MoreAvailable));
        }
        ProducedItem::new(common::plain_unit(unit))
    }
}

/// Every non-boundary unit renders exactly the configured number of unwrapped rows at every
/// width.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FixtureProducer {
    /// Height per unit key; units not listed use `default_height`.
    pub heights: BTreeMap<UnitKey, u64>,
    /// Height of unlisted non-boundary units.
    pub default_height: u64,
}

impl FixtureProducer {
    /// A fixture where every non-boundary unit has `default_height` rows.
    #[must_use]
    pub const fn uniform(default_height: u64) -> Self {
        Self {
            heights: BTreeMap::new(),
            default_height,
        }
    }

    /// Return the fixture with one unit's height overridden.
    #[must_use]
    pub fn with_height(mut self, key: UnitKey, height: u64) -> Self {
        self.heights.insert(key, height);
        self
    }
}

impl UnitProducer for FixtureProducer {
    fn produce(&self, unit: UnitView<'_>, _cx: &ItemRenderContext<'_>) -> ProducedItem {
        if let UnitKind::Boundary(edge) = unit.unit.kind {
            return ProducedItem::new(common::boundary_row(edge, BoundaryStatus::MoreAvailable));
        }
        let height = self
            .heights
            .get(&unit.unit.key)
            .copied()
            .unwrap_or(self.default_height);
        let lines = (0..height)
            .map(|row| Line::raw(format!("{} row {row}", unit.unit.key)))
            .collect::<Vec<_>>();
        ProducedItem::new(TextBlock::new(Text::from_lines(lines)).wrap(TextWrap::None))
    }
}
