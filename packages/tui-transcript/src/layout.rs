//! Identity-keyed item layout (spec section 3.3, rules L1–L9).
//!
//! Phase 2 stub: this module currently defines only the shared types other layers compile
//! against ([`SourcePosition`], [`LayoutFingerprint`], [`ItemLayout`]). The identity-keyed
//! cache, `ItemRenderContext` and producers land with the layout layer (phases 2–3).

use std::collections::BTreeMap;

use crate::units::UnitKey;

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
}

/// Hash of every layout-affecting input of an item except width (L3).
///
/// Never contains pointer addresses, wall-clock time, transcript length, history-loading state,
/// focus, selection, clock phase or elapsed time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LayoutFingerprint(pub u64);

/// Retained exact layout of one unit at one width.
///
/// Phase 2 carries only the exact height; retained rows, source map, focus targets, hit regions
/// and selection fragments are added by the layout layer (L6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ItemLayout {
    /// Exact height in rows at the layout width.
    pub height: u64,
}

/// Cache key of one retained layout (L1).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemLayoutKey {
    /// Unit identity.
    pub unit: UnitKey,
    /// Unit layout revision.
    pub revision: u64,
    /// Layout-affecting inputs other than width.
    pub fingerprint: LayoutFingerprint,
    /// Layout width in cells.
    pub width: u64,
}

/// Identity-keyed cache of retained unit layouts; the only source of unit heights (VP6).
#[derive(Debug, Clone, Default)]
pub struct ItemLayoutCache {
    entries: BTreeMap<ItemLayoutKey, ItemLayout>,
}

impl ItemLayoutCache {
    /// Create an empty cache.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Retained layout for an exact key, if cached.
    #[must_use]
    pub fn get(&self, key: &ItemLayoutKey) -> Option<&ItemLayout> {
        self.entries.get(key)
    }

    /// Store a retained layout, replacing any previous one for the same key.
    pub fn insert(&mut self, key: ItemLayoutKey, layout: ItemLayout) {
        self.entries.insert(key, layout);
    }

    /// Drop every retained layout of a unit (it left the resident window).
    pub fn remove_unit(&mut self, unit: &UnitKey) {
        self.entries.retain(|key, _| &key.unit != unit);
    }

    /// Number of retained layouts.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
