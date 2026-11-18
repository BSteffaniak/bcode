//! Guided review: what the guide tools check, plan, write, and render (see
//! `docs/code-review-guide.md`).
//!
//! * [`coverage`] tells which review files reviewed sections mark viewed, and which sections
//!   un-viewing a file un-reviews.
//! * [`document`] checks a guide document against its bounds and structure before it is
//!   stored, and fingerprints sections for reviewed marks.
//! * [`cite`] checks a requested citation against the revisions a guide is pinned to, through
//!   the small [`cite::CitationReader`] interface, and never drops it.
//! * [`freshness`] tells whether a version's citations still hold at the review's current
//!   revisions.
//! * [`config`] reads the `guide` configuration; [`template`] resolves templates and the
//!   section catalog; [`context`] lists and reads the configured context files.
//! * [`plan`] decides, deterministically, between one agent and a fan-out, and partitions a
//!   fan-out.
//! * [`write`] applies a batch of write operations under the writer's grant; [`lint`] reports
//!   what a writer should fix; [`export`] renders a version as markdown.

pub mod cite;
pub mod config;
pub mod context;
pub mod coverage;
pub mod document;
pub mod export;
pub mod freshness;
pub mod lint;
pub mod plan;
pub mod template;
mod text;
pub mod write;
