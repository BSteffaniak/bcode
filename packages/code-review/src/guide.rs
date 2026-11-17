//! Guided review: document checks, citation checks, and freshness (see
//! `docs/code-review-guide.md`).
//!
//! * [`document`] checks a guide document against its bounds and structure before it is
//!   stored, and fingerprints sections for reviewed marks.
//! * [`cite`] checks a requested citation against the revisions a guide is pinned to, through
//!   the small [`cite::CitationReader`] interface, and never drops it.
//! * [`freshness`] tells whether a version's citations still hold at the review's current
//!   revisions.

pub mod cite;
pub mod document;
pub mod freshness;
mod text;
