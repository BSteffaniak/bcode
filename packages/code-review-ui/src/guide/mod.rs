//! The guided review on the review screen (see `docs/code-review-guide.md`).
//!
//! * [`state`]: the review's guides as stored, the version shown, its sections in reading order
//!   with their numbers, the section the reviewer is on, and where guide jumps came from.
//! * [`marks`]: the version's citation ranges indexed by file and side, so diff rows find their
//!   badges and the current section's tint without scanning the guide.
//! * [`panel`]: the read-only guide panel (outline and current section) and the context peek of
//!   cited code outside the review.
//!
//! The guide is read here, never written: guides are written through the review plugin's
//! `code_review.guide.*` tools.

pub mod marks;
pub mod panel;
pub mod state;

pub use marks::{BadgeGlyph, GuideMarks, MarkBadge, RowMarks};
pub use panel::{GuidePanelView, PanelClick, PanelModel, Peek, PeekView, Placement};
pub use state::{ActiveGuide, CiteTarget, GuideLoad, GuideLoaded, GuideState, SectionEntry};
