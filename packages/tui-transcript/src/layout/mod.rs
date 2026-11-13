//! Identity-keyed item layout.
//!
//! * [`context`]: explicit render inputs ([`ItemRenderContext`], [`PaintInputs`]) and the
//!   layout fingerprint (L2, L3).
//! * [`producers`]: the producer contract and dispatch (L4).
//! * [`geometry`]: per-item geometry derived from the retained layout (B3, L6).
//! * [`cache`]: the identity-keyed [`ItemLayoutCache`], the only source of unit heights and
//!   retained layouts (L1, VP6), with layout work counters (9.2).

pub mod cache;
pub mod context;
pub mod geometry;
pub mod producers;

pub use cache::{
    ItemLayout, ItemLayoutCache, ItemLayoutKey, LayoutInputs, LayoutWorkCounters, RetainedItem,
};
pub use context::{
    ActivityPhase, DisclosureState, ItemRenderContext, LayoutFingerprint, PaintInputs, RenderConfig,
};
pub use geometry::{ItemGeometry, PaintSlot, PaintSlotKind, SourceRow, SourceRows};

/// Source position inside a unit, used by source-backed anchors (P3) and selection.
///
/// BMUX sees this only as the opaque source-position type parameter of its list mechanics and
/// never interprets it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SourcePosition {
    /// Byte offset into a Markdown source.
    MarkdownByte(u64),
    /// Zero-based line of a diff.
    DiffLine(u64),
    /// Byte range start inside shell output.
    ShellByte(u64),
    /// Producer-namespaced plugin content key.
    PluginContent(String),
    /// Position inside a plugin visual's adapter-owned source (E3): a producer-scoped identity
    /// (`<plugin>:<identity>`, a version 4 source range identity or a version 3 content key)
    /// and an offset inside it (the range start, or rows below a keyed row). Positions resolve
    /// only within their own identity, by containment.
    PluginSource {
        /// Producer-scoped source identity.
        identity: String,
        /// Offset inside the identity.
        offset: u64,
    },
}

impl SourcePosition {
    /// Whether `self` and `other` index the same source: the same kind and, for plugin
    /// sources, the same identity.
    #[must_use]
    pub fn same_source(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::PluginSource { identity, .. },
                Self::PluginSource {
                    identity: other, ..
                },
            ) => identity == other,
            _ => std::mem::discriminant(self) == std::mem::discriminant(other),
        }
    }
}
