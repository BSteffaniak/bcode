//! Renderer-neutral review document (spec section 5, residency from section 4.3, navigation
//! targets from section 6.2).
//!
//! The document is the only owner of review structure: one keyed sequence of items over every
//! file of every source, with lazily loaded per-file content, interval-set context expansion,
//! anchored annotations, viewed state, and navigation. The TUI adapts it to BMUX components
//! and issues the service requests it asks for; the document itself performs no I/O.
//!
//! * The file index is fully resident. A file whose content is not resident is a file header
//!   plus one placeholder item carrying an estimated height.
//! * [`ReviewDocument::set_viewport`] and [`ReviewDocument::take_needs`] drive loading for the
//!   viewport and a prefetch window. Results come back through `accept_*` or `fail` with the
//!   ticket of their need; stale results are ignored. Content is evicted least recently used
//!   beyond a byte budget, never inside the protected window around the viewport.
//! * Every change to the sequence is reported as a [`Splice`] with a monotonically increasing
//!   revision, for incremental virtual-list measurement.
//!
//! **Portability.** This module must not use BMUX, crossterm, or any terminal type; it depends
//! only on `bcode_code_review_models`, the V2 viewed-mark matcher, `regex`, and the standard
//! library. Within one crate this is enforced by review, not by the compiler (spec section 3:
//! the module can be extracted to a domain crate if mechanical enforcement is wanted).

mod anchors;
mod build;
#[allow(clippy::module_inception)]
mod document;
mod fenwick;
mod gaps;
mod interval;
mod items;
mod keys;
mod navigation;

#[cfg(test)]
pub(crate) mod tests;

pub use anchors::{AnchorLines, AnchorTarget, Annotations, DocAnchor, target_of};
pub use build::estimate_rows;
pub use document::{
    DEFAULT_RESIDENCY_BUDGET_BYTES, DocumentConfig, DocumentNeed, REVIEW_LEVEL_PATH,
    ReviewDocument, Sections, SourceInput, SourceStack, Splice, Ticket, ViewedProgress,
};
pub use interval::IntervalSet;
pub use items::{
    AgentThreadItem, AnchorPlacement, ComposerItem, FileHeaderItem, FileLoad, FileNoticeItem,
    GapItem, GapState, HunkHeaderItem, LayerNumber, LayoutMode, LineContent, LineItem, LineKind,
    LinePairItem, OutdatedReason, PairSide, PlaceholderItem, ReviewItem, SourceHeaderItem,
    StackLayerLabel, ThreadItem,
};
pub use keys::{FileKey, FileNoticeKind, FileRef, ItemKey};
pub use navigation::{
    Direction, NavKind, NavTarget, SearchError, SearchMatch, SearchQuery, SearchResults,
};
