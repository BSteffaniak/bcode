//! Transcript selection (spec C34–C36).
//!
//! Phase 2 stub; selection over retained source mappings lands in phase 6.

/// Transcript selection state; paint-only input (L5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct SelectionState {
    /// A selection gesture or selection is active (defers eviction, W7).
    pub active: bool,
}
