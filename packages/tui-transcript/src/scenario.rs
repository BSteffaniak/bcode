//! Deterministic fixtures for scenario drivers (feature `testing`).
//!
//! * [`LayoutWorkCounters`] (spec 9.2, D4): the engine counts producer builds, measurements
//!   and the Markdown work producers report; read and reset them with
//!   [`crate::engine::TranscriptEngine::take_layout_work`]. Scroll-only updates and frames keep
//!   every counter at zero; a prepend counts only the new units.
//! * Fixture producers live in [`crate::layout::producers::testing`].

pub use crate::layout::LayoutWorkCounters;
pub use crate::layout::producers::testing::{FixtureProducer, PlainTextProducer};
