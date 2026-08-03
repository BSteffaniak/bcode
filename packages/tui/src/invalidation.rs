//! Generic UI invalidation primitives.

use std::time::Instant;

/// Opaque key identifying a coalescable UI invalidation source.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InvalidationKey(String);

impl InvalidationKey {
    /// Create an invalidation key.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Return the key text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A requested future invalidation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidationRequest {
    /// Opaque invalidation source key.
    pub key: InvalidationKey,
    /// Time at which the invalidation should be delivered.
    pub at: Instant,
}

impl InvalidationRequest {
    /// Create an invalidation request.
    #[must_use]
    pub const fn new(key: InvalidationKey, at: Instant) -> Self {
        Self { key, at }
    }
}

/// Rendering invalidation severity and semantic damage level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum UiInvalidation {
    /// No render is required.
    None,
    /// Existing layout can be repainted.
    Paint,
    /// One or more stable transcript items changed.
    Items,
    /// Item ordering or membership changed.
    Structural,
    /// Full terminal state changed, such as resize or reset.
    Full,
}

impl UiInvalidation {
    /// Return whether a terminal draw is needed.
    #[cfg(test)]
    #[must_use]
    pub const fn needs_render(self) -> bool {
        !matches!(self, Self::None)
    }

    /// Return the stronger of two invalidations.
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        self.max(other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_damage_merges_by_severity_and_item_identity() {
        let mut damage = UiInvalidation::Paint;
        damage = damage.merge(UiInvalidation::Items);
        assert_eq!(damage, UiInvalidation::Items);
        damage = damage.merge(UiInvalidation::Structural);
        damage = damage.merge(UiInvalidation::Paint);
        assert_eq!(damage, UiInvalidation::Structural);
        damage = damage.merge(UiInvalidation::Full);
        assert_eq!(damage, UiInvalidation::Full);
    }
}
