//! Generic UI invalidation primitives.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

const MAX_TEMPORAL_SOURCES: usize = 256;
const TEMPORAL_COALESCE_WINDOW: Duration = Duration::from_millis(2);

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

/// Bounded retained deadline index for renderer-local temporal sources.
#[derive(Debug, Default)]
pub struct TemporalRegistry {
    by_key: BTreeMap<InvalidationKey, Instant>,
    by_deadline: BTreeSet<(Instant, InvalidationKey)>,
}

impl TemporalRegistry {
    /// Incrementally reconcile active temporal sources without rebuilding the registry.
    pub fn reconcile(&mut self, requests: impl IntoIterator<Item = InvalidationRequest>) {
        let requests = requests
            .into_iter()
            .take(MAX_TEMPORAL_SOURCES)
            .collect::<Vec<_>>();
        let requested_keys = requests
            .iter()
            .map(|request| request.key.clone())
            .collect::<BTreeSet<_>>();
        let removed = self
            .by_key
            .keys()
            .filter(|key| !requested_keys.contains(*key))
            .cloned()
            .collect::<Vec<_>>();
        for key in removed {
            self.remove(&key);
        }
        for request in requests {
            self.insert(request);
        }
    }

    /// Return the earliest retained deadline.
    #[must_use]
    pub fn next_at(&self) -> Option<Instant> {
        self.by_deadline.first().map(|(at, _)| *at)
    }

    /// Remove and return all sources due within the bounded coalescing window.
    pub fn take_due(&mut self, now: Instant) -> Vec<InvalidationKey> {
        let cutoff = now + TEMPORAL_COALESCE_WINDOW;
        let due = self
            .by_deadline
            .iter()
            .take_while(|(at, _)| *at <= cutoff)
            .cloned()
            .collect::<Vec<_>>();
        for (at, key) in &due {
            self.by_deadline.remove(&(*at, key.clone()));
            self.by_key.remove(key);
        }
        due.into_iter().map(|(_, key)| key).collect()
    }

    fn insert(&mut self, request: InvalidationRequest) {
        if self.by_key.contains_key(&request.key) {
            return;
        }
        self.by_key.insert(request.key.clone(), request.at);
        self.by_deadline.insert((request.at, request.key));
    }

    fn remove(&mut self, key: &InvalidationKey) {
        if let Some(at) = self.by_key.remove(key) {
            self.by_deadline.remove(&(at, key.clone()));
        }
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
    fn retained_temporal_registry_delivers_scheduled_keys_before_reconciliation() {
        let now = Instant::now();
        let first = InvalidationKey::new("activity");
        let second = InvalidationKey::new("tool");
        let mut registry = TemporalRegistry::default();
        registry.reconcile([
            InvalidationRequest::new(first.clone(), now + Duration::from_millis(100)),
            InvalidationRequest::new(second.clone(), now + Duration::from_millis(101)),
        ]);

        assert_eq!(registry.next_at(), Some(now + Duration::from_millis(100)));
        assert_eq!(
            registry.take_due(now + Duration::from_millis(100)),
            [first, second]
        );
        assert_eq!(registry.next_at(), None);
    }

    #[test]
    fn retained_temporal_registry_preserves_pending_deadline_during_reconciliation() {
        let now = Instant::now();
        let key = InvalidationKey::new("animation");
        let mut registry = TemporalRegistry::default();
        registry.reconcile([InvalidationRequest::new(
            key.clone(),
            now + Duration::from_millis(20),
        )]);
        for delay in 21..=100 {
            registry.reconcile([InvalidationRequest::new(
                key.clone(),
                now + Duration::from_millis(delay),
            )]);
        }
        assert_eq!(registry.next_at(), Some(now + Duration::from_millis(20)));
        assert_eq!(registry.take_due(now + Duration::from_millis(20)), [key]);
        assert_eq!(registry.next_at(), None);
    }

    #[test]
    fn retained_temporal_registry_schedules_next_generation_after_delivery() {
        let now = Instant::now();
        let key = InvalidationKey::new("animation");
        let mut registry = TemporalRegistry::default();
        registry.reconcile([InvalidationRequest::new(
            key.clone(),
            now + Duration::from_millis(20),
        )]);
        assert_eq!(
            registry.take_due(now + Duration::from_millis(20)),
            [key.clone()]
        );

        registry.reconcile([InvalidationRequest::new(
            key,
            now + Duration::from_millis(40),
        )]);
        assert_eq!(registry.next_at(), Some(now + Duration::from_millis(40)));
    }

    #[test]
    fn retained_temporal_registry_removes_inactive_sources() {
        let now = Instant::now();
        let mut registry = TemporalRegistry::default();
        registry.reconcile([InvalidationRequest::new(
            InvalidationKey::new("animation"),
            now + Duration::from_millis(20),
        )]);

        registry.reconcile([]);
        assert_eq!(registry.next_at(), None);
    }

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
