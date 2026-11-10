//! Process-local rows (spec section 3.3, LR1–LR7): pending submissions and notices the host
//! owns, reduced through the same update path as content changes.

use web_time::Instant;

use bcode_session_view_models::TranscriptChanges;

use crate::navigation::NavInput;
use crate::units::local::LocalRow;

use super::{EngineUpdate, TranscriptEngine, transcript_items};

impl TranscriptEngine {
    /// Replace the host's process-local rows (LR1–LR4) in one update.
    ///
    /// Rows equal to the current ones are a no-op. Otherwise the units are re-derived, new or
    /// changed rows are measured and spliced like any membership change, and the change is
    /// reduced as content (N11): a following reader stays on the latest bottom, a reading
    /// position stays where it is, and a row below the viewport is latest-bar activity (LR7).
    /// Local rows never trigger automatic navigation (LR6, C8).
    pub fn set_local_rows(&mut self, rows: Vec<LocalRow>, at: Instant) -> EngineUpdate {
        self.observe_time(at);
        let items = transcript_items(&self.session);
        let unit_changes = self.units.set_local_rows(rows, items, &self.structure);
        if unit_changes.is_empty() {
            return EngineUpdate::default();
        }
        self.splice_units(&unit_changes);
        let effects = self.reduce_input(
            NavInput::Content {
                changes: TranscriptChanges::default(),
                at,
            },
            &unit_changes.correspondence,
        );
        let mut update = self.run_effects(effects);
        update.invalidated = true;
        self.record_hidden_activity(&unit_changes, at);
        update.merge(self.settle(at));
        update
    }
}
