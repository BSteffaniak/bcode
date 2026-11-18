//! The guided review on the review screen (see `docs/code-review-guide.md`).
//!
//! * [`state`]: the review's guides as stored, the version shown, its sections in reading order
//!   with their numbers, the section the reviewer is on, and where guide jumps came from.
//! * [`marks`]: the version's citation ranges indexed by file and side, so diff rows find their
//!   badges and the current section's tint without scanning the guide.
//! * [`card`]: the guide as the assistant's conversation shows it, a live card in place of a
//!   turn's guide tool lines.
//! * [`fanout`]: a guide written by child agents: queueing, budget, resume, and the progress
//!   the panel and the card show.
//! * [`panel`]: the read-only guide panel (outline and current section) and the context peek of
//!   cited code outside the review.
//!
//! The guide is read here, never written: guides are written through the review plugin's
//! `code_review.guide.*` tools.

pub mod card;
pub mod fanout;
pub mod marks;
pub mod panel;
pub mod state;

pub use card::{CardSection, CardSectionState, CardStatus, GuideCard};
pub use marks::{BadgeGlyph, GuideMarks, MarkBadge, RowMarks};
pub use panel::{GuidePanelView, PanelClick, PanelModel, Peek, PeekView, Placement};
pub use state::{
    ActiveGuide, CiteTarget, GuideKeys, GuideLoad, GuideLoaded, GuideState, SectionEntry,
};
