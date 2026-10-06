//! Ephemeral request fencing for bounded child discovery and observations.
use bcode_session_models::SessionId;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(1);

/// Latest explicitly scheduled request per operation and session.
#[derive(Debug, Clone, Default)]
pub struct ChildRequestFence(BTreeMap<(bool, SessionId), u64>);
impl ChildRequestFence {
    pub fn begin(&mut self, activity: bool, session_id: SessionId) -> u64 {
        let generation = NEXT.fetch_add(1, Ordering::Relaxed);
        self.0.insert((activity, session_id), generation);
        generation
    }
    #[must_use]
    pub fn accepts(&self, activity: bool, session_id: SessionId, generation: u64) -> bool {
        self.0.get(&(activity, session_id)) == Some(&generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn later_request_and_new_navigation_reject_stale_same_session() {
        let id = SessionId::new();
        let mut fence = ChildRequestFence::default();
        let first = fence.begin(false, id);
        let second = fence.begin(false, id);
        assert!(!fence.accepts(false, id, first));
        assert!(fence.accepts(false, id, second));
        assert!(!ChildRequestFence::default().accepts(false, id, second));
        assert!(!fence.accepts(true, id, second));
    }
}
