//! Private live generation activity, separate from content-free diagnostic exports.
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Instant,
};

/// Request-local live fact. Text is provisional and may contain source content.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Activity {
    /// A model call started, with an opaque operation-local identity.
    Started { id: u64, label: String },
    /// Public assistant output, not authoritative structured content or hidden reasoning.
    Text { id: u64, text: String },
    /// A call settled. Success does not imply the generation has published.
    Finished { id: u64, succeeded: bool },
    /// An exact completed result was reused without provider execution.
    Reused,
}
/// Explicit consumer for private content-bearing activity. Never added to diagnostics implicitly.
pub trait ActivitySink: Send + Sync + std::fmt::Debug {
    /// Must remain bounded and nonblocking; slow presentation cannot block generation.
    fn emit(&self, activity: Activity);
}
/// A bounded view of recent activity; the full durable transcript is not implied.
#[derive(Clone, Debug)]
pub struct Timeline {
    started: Instant,
    entries: Arc<Mutex<VecDeque<(u64, Activity)>>>,
}
impl Default for Timeline {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            entries: Arc::default(),
        }
    }
}
impl ActivitySink for Timeline {
    fn emit(&self, mut activity: Activity) {
        if let Activity::Text { text, .. } = &mut activity {
            let mut end = text.len().min(2048);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let millis = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if let Activity::Text { id, text } = &activity
            && let Some((
                _,
                Activity::Text {
                    id: last_id,
                    text: last,
                },
            )) = entries.back_mut()
            && id == last_id
        {
            last.push_str(text);
            if last.len() > 4096 {
                let mut cut = last.len() - 4096;
                while !last.is_char_boundary(cut) {
                    cut += 1;
                }
                last.drain(..cut);
            }
            return;
        }
        entries.push_back((millis, activity));
        while entries.len() > 256 {
            entries.pop_front();
        }
        drop(entries);
    }
}
impl Timeline {
    /// Bounded recent activity. Eviction is explicit; this is not a durable resume cursor.
    #[must_use]
    pub fn recent(&self, limit: usize) -> Vec<(u64, Activity)> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries
            .iter()
            .skip(entries.len().saturating_sub(limit.min(256)))
            .cloned()
            .collect()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn streaming_memory_is_bounded_and_unicode_safe() {
        let timeline = Timeline::default();
        for _ in 0..10_000 {
            timeline.emit(Activity::Text {
                id: 1,
                text: "🦀".repeat(1000),
            });
        }
        let entries = timeline.recent(256);
        assert_eq!(entries.len(), 1);
        let Activity::Text { text, .. } = &entries[0].1 else {
            panic!()
        };
        assert!(text.len() <= 4096);
        for id in 0..300 {
            timeline.emit(Activity::Finished {
                id,
                succeeded: true,
            });
        }
        assert_eq!(timeline.recent(1000).len(), 256);
    }
}
