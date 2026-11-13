//! Renderer-neutral review document.
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
//! **Portability.** The document uses no terminal types. It lives in the data layer, which
//! depends on no renderer, so any front end (terminal or otherwise) can drive it; it reads only
//! `bcode_code_review_models`, the viewed-mark matcher, `regex`, and the standard library.

mod anchors;
mod build;
mod fenwick;
mod gaps;
mod interval;
mod items;
mod keys;
mod navigation;
mod review;
mod upsert;

#[cfg(test)]
pub(crate) mod tests;

pub use anchors::{AnchorLines, AnchorTarget, Annotations, DocAnchor, target_of};
pub use build::estimate_rows;
pub use interval::IntervalSet;
pub use items::{
    AgentThreadItem, AnchorPlacement, ComposerItem, FileDetailsItem, FileHeaderItem, FileLoad,
    FileNoticeItem, GapItem, GapState, HunkHeaderItem, LayerNumber, LayoutMode, LineContent,
    LineItem, LineKind, LinePairItem, OutdatedReason, PairSide, PlaceholderItem, ReviewItem,
    SourceHeaderItem, StackLayerLabel, ThreadItem,
};
pub use keys::{FileKey, FileNoticeKind, FileRef, ItemKey};
pub use navigation::{
    Direction, NavKind, NavTarget, SearchError, SearchMatch, SearchQuery, SearchResults,
};
pub use review::{
    DEFAULT_RESIDENCY_BUDGET_BYTES, DocumentConfig, DocumentNeed, FileChrome, GroupInfo,
    REVIEW_LEVEL_PATH, ReviewDocument, Sections, SourceInput, SourceStack, Splice, Ticket,
    ViewedProgress,
};
