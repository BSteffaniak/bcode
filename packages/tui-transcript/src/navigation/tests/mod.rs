//! Reducer tests: a measured-index fixture plus one test module per concern.

mod properties;
mod transitions;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use bcode_session_view_models::{TranscriptChange, TranscriptChanges, TranscriptViewItemId};
use bmux_tui_components::virtual_list::{
    MeasuredListIndex, MeasuredListItem, NoSourceMap, ResolvedPosition,
};

use super::{
    Correspondence, MotionConfig, NavEffect, NavInput, NavIntent, NavState, NavView, RemovedUnits,
    UnitFacts, UnitRole, presented_position, reduce,
};
use crate::units::{BoundaryEdge, UnitKey};
use crate::viewport::ViewportGeometry;
use crate::window::ResidentWindow;

/// The older resident-window boundary unit.
const OLDER: UnitKey = UnitKey::Boundary(BoundaryEdge::Older);
/// Fixture width (heights are given directly).
const WIDTH: u64 = 80;

fn unit(name: &str) -> UnitKey {
    UnitKey::Item(TranscriptViewItemId::new(name))
}

struct Facts<'a> {
    roles: &'a BTreeMap<UnitKey, UnitRole>,
    sequences: &'a BTreeMap<u64, UnitKey>,
    removed: &'a RemovedUnits,
}

impl UnitFacts for Facts<'_> {
    fn role(&self, unit: &UnitKey) -> UnitRole {
        self.roles.get(unit).copied().unwrap_or_default()
    }

    fn unit_for_sequence(&self, sequence: u64) -> Option<UnitKey> {
        self.sequences.get(&sequence).cloned()
    }

    fn correspondence(&self, removed: &UnitKey) -> Option<Correspondence> {
        self.removed.get(removed).cloned()
    }
}

/// Units with exact heights, a resident window and the navigation state under test.
struct Fixture {
    units: Vec<(UnitKey, u64)>,
    roles: BTreeMap<UnitKey, UnitRole>,
    sequences: BTreeMap<u64, UnitKey>,
    removed: RemovedUnits,
    window: ResidentWindow,
    viewport: ViewportGeometry,
    motion: MotionConfig,
    state: NavState,
    t0: Instant,
}

impl Fixture {
    /// Viewport of `height` rows over the older boundary row plus `units`; instant motion.
    fn new(height: u64, units: &[(&str, u64)]) -> Self {
        let mut all = vec![(OLDER, 1)];
        all.extend(units.iter().map(|(name, rows)| (unit(name), *rows)));
        let mut window = ResidentWindow::new();
        window.first_sequence = Some(1_000);
        window.last_sequence = Some(2_000);
        window.event_count = 100;
        Self {
            units: all,
            roles: BTreeMap::new(),
            sequences: BTreeMap::new(),
            removed: RemovedUnits::new(),
            window,
            viewport: ViewportGeometry::new(WIDTH, height),
            motion: MotionConfig::INSTANT,
            state: NavState::new(),
            t0: Instant::now(),
        }
    }

    /// `count` units `u0..` of `rows` rows each.
    fn uniform(height: u64, count: usize, rows: u64) -> Self {
        let names: Vec<String> = (0..count).map(|n| format!("u{n}")).collect();
        let units: Vec<(&str, u64)> = names.iter().map(|name| (name.as_str(), rows)).collect();
        Self::new(height, &units)
    }

    fn animated(mut self) -> Self {
        self.motion = MotionConfig {
            duration: Duration::from_millis(100),
            easing: bmux_tui::style::Easing::Linear,
            frame_interval: Duration::from_millis(10),
        };
        self
    }

    fn index(&self) -> MeasuredListIndex<UnitKey> {
        MeasuredListIndex::from_items(
            0,
            self.units
                .iter()
                .map(|(key, rows)| MeasuredListItem::new(key.clone(), 0, WIDTH, 0, *rows)),
        )
        .expect("fixture keys are unique")
    }

    fn with_view<R>(&self, f: impl FnOnce(&NavView<'_>) -> R) -> R {
        let index = self.index();
        let facts = Facts {
            roles: &self.roles,
            sequences: &self.sequences,
            removed: &self.removed,
        };
        let view = NavView {
            index: &index,
            sources: &NoSourceMap,
            facts: &facts,
            viewport: self.viewport,
            window: &self.window,
            motion: self.motion,
        };
        f(&view)
    }

    fn reduce(&mut self, input: NavInput) -> Vec<NavEffect> {
        let index = self.index();
        let facts = Facts {
            roles: &self.roles,
            sequences: &self.sequences,
            removed: &self.removed,
        };
        let view = NavView {
            index: &index,
            sources: &NoSourceMap,
            facts: &facts,
            viewport: self.viewport,
            window: &self.window,
            motion: self.motion,
        };
        reduce(&mut self.state, input, &view)
    }

    fn at(&self, ms: u64) -> Instant {
        self.t0 + Duration::from_millis(ms)
    }

    fn intent(&mut self, intent: NavIntent, ms: u64) -> Vec<NavEffect> {
        let at = self.at(ms);
        self.reduce(NavInput::Intent { intent, at })
    }

    fn scroll(&mut self, rows: i64, ms: u64) -> Vec<NavEffect> {
        self.intent(
            NavIntent::ScrollBy {
                rows,
                source: super::InputSource::Wheel,
            },
            ms,
        )
    }

    fn content(&mut self, ms: u64) -> Vec<NavEffect> {
        self.content_with(Vec::new(), ms)
    }

    fn content_with(&mut self, changes: Vec<TranscriptChange>, ms: u64) -> Vec<NavEffect> {
        let at = self.at(ms);
        self.reduce(NavInput::Content {
            changes: TranscriptChanges {
                changes,
                ..TranscriptChanges::default()
            },
            at,
        })
    }

    fn timer(&mut self, key: super::NavTimer, ms: u64) -> Vec<NavEffect> {
        let at = self.at(ms);
        self.reduce(NavInput::Timer { key, at })
    }

    /// Resolved resting position.
    fn resolved(&self) -> ResolvedPosition {
        self.with_view(|view| view.resolve(&self.state.resting_position()))
            .expect("resting anchor is resident")
    }

    /// Top row of the resting position.
    fn top(&self) -> u64 {
        self.resolved().top_offset
    }

    /// Top row presented at `ms` (motion sampled).
    fn top_at(&self, ms: u64) -> u64 {
        let at = self.at(ms);
        self.with_view(|view| {
            view.resolve(&presented_position(&self.state, view, at))
                .expect("presented anchor is resident")
                .top_offset
        })
    }

    fn offset(&self, name: &str) -> u64 {
        self.with_view(|view| view.unit_offset(&unit(name)))
            .expect("unit is resident")
    }

    /// Screen row of a unit's first row under the resting position.
    fn row_of(&self, name: &str) -> i64 {
        i64::try_from(self.offset(name)).unwrap() - i64::try_from(self.top()).unwrap()
    }

    fn total(&self) -> u64 {
        self.units.iter().map(|(_, rows)| rows).sum()
    }

    fn append(&mut self, name: &str, rows: u64) {
        self.units.push((unit(name), rows));
    }

    /// Insert units right after the older boundary; returns the inserted rows.
    fn prepend(&mut self, units: &[(&str, u64)]) -> u64 {
        let at = usize::from(self.units.first().is_some_and(|(key, _)| key == &OLDER));
        for (offset, (name, rows)) in units.iter().enumerate() {
            self.units.insert(at + offset, (unit(name), *rows));
        }
        units.iter().map(|(_, rows)| rows).sum()
    }

    fn set_height(&mut self, name: &str, rows: u64) {
        let key = unit(name);
        if let Some(entry) = self.units.iter_mut().find(|(unit, _)| unit == &key) {
            entry.1 = rows;
        }
    }

    /// Remove units and record their P4 correspondence.
    fn remove(&mut self, names: &[&str]) {
        let before: Vec<UnitKey> = self.units.iter().map(|(key, _)| key.clone()).collect();
        let gone: Vec<UnitKey> = names.iter().map(|name| unit(name)).collect();
        self.units.retain(|(key, _)| !gone.contains(key));
        self.removed = RemovedUnits::between(&before, |key| !gone.contains(key), |_| None);
    }

    fn role(&mut self, name: &str, role: UnitRole) {
        self.roles.insert(unit(name), role);
    }

    /// Admit every visible resident unit, as an attached engine would have.
    fn admit_all(&mut self) {
        for (key, rows) in &self.units {
            if !key.is_boundary() && *rows > 0 {
                self.state.admitted.insert(key.clone());
            }
        }
    }
}

fn has_page(effects: &[NavEffect]) -> bool {
    effects
        .iter()
        .any(|effect| matches!(effect, NavEffect::Page(_)))
}
