//! Code Review V2.
//!
//! The review UI and data layer described in `docs/code-review-v2-architecture.md`. V2 began as a
//! parallel rewrite of the first review interface and replaced it (phase 7); it is the only
//! implementation.
//!
//! * [`document`] is the renderer-neutral review document (phase 2).
//! * [`service`] is the lazy, revision-pinned data layer (phase 1).
//! * [`provider`] routes pull-request and stack sources to review-source provider plugins from
//!   the client.
//! * [`viewed`] matches identity-pinned viewed marks.
//! * [`publish`] plans which pull request each thread publishes to and builds bundle threads.
//! * [`tui`] is the review surface over the document (phase 3).

pub mod document;
pub mod provider;
pub mod publish;
pub mod service;
#[cfg(test)]
pub mod testing;
pub mod tui;
pub mod viewed;
