//! Deterministic fixtures for scenario drivers (feature `testing`): layout work counters and a
//! counting producer that observes them.

use std::cell::Cell;
use std::rc::Rc;

use bmux_tui::component::Element;

use crate::units::producers::{UnitContent, UnitProducer};

/// Layout work observed per frame (spec section 9.2, D4).
///
/// Scroll-only frames keep every counter at zero; a prepend counts only the new units.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct LayoutWorkCounters {
    /// Markdown source parses.
    pub markdown_parses: u64,
    /// Syntax highlighting runs.
    pub highlights: u64,
    /// Markdown projections built.
    pub projections: u64,
    /// Unit measurements (exact heights computed).
    pub measurements: u64,
    /// Producer component builds outside measurement.
    pub producer_builds: u64,
}

/// Shared handle to counters a [`CountingProducer`] updates.
#[derive(Debug, Clone, Default)]
pub struct LayoutWorkProbe {
    counters: Rc<Cell<LayoutWorkCounters>>,
}

impl LayoutWorkProbe {
    /// A probe with every counter at zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Current counters.
    #[must_use]
    pub fn counters(&self) -> LayoutWorkCounters {
        self.counters.get()
    }

    /// Return the current counters and reset them to zero.
    #[must_use]
    pub fn take(&self) -> LayoutWorkCounters {
        self.counters.take()
    }

    fn record(&self, update: impl FnOnce(&mut LayoutWorkCounters)) {
        let mut counters = self.counters.get();
        update(&mut counters);
        self.counters.set(counters);
    }
}

/// Wraps a producer and counts its measurements and component builds into a probe.
///
/// Phase 2 producers render plain text, so only `measurements` and `producer_builds` move;
/// the Markdown counters belong to the phase 3 producers.
#[derive(Debug)]
pub struct CountingProducer<P> {
    inner: P,
    probe: LayoutWorkProbe,
}

impl<P> CountingProducer<P> {
    /// Count `inner`'s work into `probe`.
    #[must_use]
    pub const fn new(inner: P, probe: LayoutWorkProbe) -> Self {
        Self { inner, probe }
    }
}

impl<P: UnitProducer> UnitProducer for CountingProducer<P> {
    fn component<'a>(&self, content: UnitContent<'a>) -> Element<'a> {
        self.probe.record(|counters| counters.producer_builds += 1);
        self.inner.component(content)
    }

    fn measure(&self, content: UnitContent<'_>, width: u64) -> u64 {
        self.probe.record(|counters| counters.measurements += 1);
        self.inner.measure(content, width)
    }
}
