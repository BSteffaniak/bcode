//! Process-local transcript notices owned by the host shell.
//!
//! Notices are not canonical session history: errors, slash-command output and plugin
//! activity that this TUI process shows in the transcript. Each records the canonical
//! chronology boundary it was raised at (the newest canonical transcript item and event
//! sequence), so the transcript engine places it after that item (LR1–LR7).

use bcode_session_view_models::{TextFormat, TranscriptViewItemId};

/// Source of process-local notices raised by the host shell itself.
pub const HOST_NOTICE_SOURCE: &str = "bcode.tui";

/// Source of finished-turn summaries.
pub const TURN_SUMMARY_SOURCE: &str = "bcode.tui.turn-summary";

/// Display role of finished-turn summaries.
pub const TURN_SUMMARY_ROLE: &str = "Summary";

/// Source prefix of plugin activity notices, followed by `<plugin id>:<note id>`.
pub const PLUGIN_ACTIVITY_SOURCE_PREFIX: &str = "bcode.plugin-activity:";

/// Stable process-unique identity of one notice (kept when notices are copied between apps).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NoticeId(u64);

impl NoticeId {
    /// Process-unique numeric value of this identity.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    fn next() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self(NEXT_ID.fetch_add(1, Ordering::Relaxed))
    }
}

/// The canonical chronology boundary a notice follows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NoticeBoundary {
    /// Newest canonical transcript item when the notice was raised.
    pub after: Option<TranscriptViewItemId>,
    /// Newest canonical event sequence when the notice was raised.
    pub after_sequence: Option<u64>,
}

/// One process-local notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EphemeralNotice {
    id: NoticeId,
    source: String,
    role: &'static str,
    text: String,
    format: TextFormat,
    boundary: NoticeBoundary,
}

impl EphemeralNotice {
    /// Stable notice identity.
    #[must_use]
    pub const fn id(&self) -> NoticeId {
        self.id
    }

    /// Component or workflow that produced the notice.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Display role (`System` for host notices, `Activity` for plugin activity).
    #[must_use]
    pub const fn role(&self) -> &'static str {
        self.role
    }

    /// Notice text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Text format of [`Self::text`].
    #[must_use]
    pub const fn format(&self) -> TextFormat {
        self.format
    }

    /// Canonical transcript item the notice follows.
    #[must_use]
    pub const fn after(&self) -> Option<&TranscriptViewItemId> {
        self.boundary.after.as_ref()
    }

    /// Newest canonical event sequence before the notice.
    #[must_use]
    pub const fn after_sequence(&self) -> Option<u64> {
        self.boundary.after_sequence
    }
}

/// Ordered process-local notices of one app, in the order they were raised.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EphemeralNotices {
    entries: Vec<EphemeralNotice>,
}

impl EphemeralNotices {
    /// Notices in presentation order.
    pub fn iter(&self) -> impl Iterator<Item = &EphemeralNotice> + '_ {
        self.entries.iter()
    }

    /// Append a host notice at `boundary`.
    pub fn push(&mut self, text: String, format: TextFormat, boundary: NoticeBoundary) {
        self.entries.push(EphemeralNotice {
            id: NoticeId::next(),
            source: HOST_NOTICE_SOURCE.to_owned(),
            role: "System",
            text,
            format,
            boundary,
        });
    }

    /// Append a finished-turn summary at `boundary`.
    pub fn push_turn_summary(&mut self, text: String, boundary: NoticeBoundary) {
        self.entries.push(EphemeralNotice {
            id: NoticeId::next(),
            source: TURN_SUMMARY_SOURCE.to_owned(),
            role: TURN_SUMMARY_ROLE,
            text,
            format: TextFormat::PlainText,
            boundary,
        });
    }

    /// Replace transient plugin activity: `(source, text)` pairs, each keeping its identity and
    /// chronology while its source stays active. New activity follows `boundary`. Returns
    /// whether any notice changed.
    pub fn set_plugin_activity(
        &mut self,
        activity: &[(String, String)],
        boundary: &NoticeBoundary,
    ) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| {
            !entry.source.starts_with(PLUGIN_ACTIVITY_SOURCE_PREFIX)
                || activity.iter().any(|(source, _)| *source == entry.source)
        });
        let mut changed = self.entries.len() != before;
        for (source, text) in activity {
            if let Some(entry) = self
                .entries
                .iter_mut()
                .find(|entry| entry.source == *source)
            {
                if entry.text != *text {
                    entry.text.clone_from(text);
                    changed = true;
                }
            } else {
                self.entries.push(EphemeralNotice {
                    id: NoticeId::next(),
                    source: source.clone(),
                    role: "Activity",
                    text: text.clone(),
                    format: TextFormat::PlainText,
                    boundary: boundary.clone(),
                });
                changed = true;
            }
        }
        changed
    }

    /// Copy the notices of `source` this list does not hold yet, keeping their identity.
    pub fn copy_from(&mut self, source: &Self) {
        for entry in &source.entries {
            if !self.entries.iter().any(|existing| existing.id == entry.id) {
                self.entries.push(entry.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EphemeralNotices, NoticeBoundary};
    use bcode_session_view_models::{TextFormat, TranscriptViewItemId};

    fn boundary(item: &str, sequence: u64) -> NoticeBoundary {
        NoticeBoundary {
            after: Some(TranscriptViewItemId::new(item)),
            after_sequence: Some(sequence),
        }
    }

    #[test]
    fn notices_keep_their_order_identity_and_boundary() {
        let mut notices = EphemeralNotices::default();
        notices.push("first".into(), TextFormat::PlainText, boundary("a", 1));
        notices.push("second".into(), TextFormat::Markdown, boundary("b", 2));
        let shown = notices.iter().collect::<Vec<_>>();
        assert_eq!(shown.len(), 2);
        assert_eq!(shown[0].text(), "first");
        assert_eq!(shown[0].role(), "System");
        assert_eq!(shown[0].after_sequence(), Some(1));
        assert_eq!(shown[1].format(), TextFormat::Markdown);
        assert_eq!(shown[1].after(), Some(&TranscriptViewItemId::new("b")));
        assert_ne!(shown[0].id(), shown[1].id());
    }

    #[test]
    fn plugin_activity_is_replaced_in_place_and_removed_when_inactive() {
        let mut notices = EphemeralNotices::default();
        let source = "bcode.plugin-activity:p:n".to_owned();
        assert!(
            notices.set_plugin_activity(&[(source.clone(), "working".into())], &boundary("a", 1))
        );
        let id = notices.iter().next().map(super::EphemeralNotice::id);
        assert!(
            notices.set_plugin_activity(&[(source.clone(), "almost".into())], &boundary("b", 9))
        );
        let entry = notices.iter().next().expect("activity");
        assert_eq!(Some(entry.id()), id);
        assert_eq!(entry.text(), "almost");
        assert_eq!(entry.after_sequence(), Some(1));
        assert!(!notices.set_plugin_activity(&[(source, "almost".into())], &boundary("c", 3)));
        assert!(notices.set_plugin_activity(&[], &boundary("c", 3)));
        assert_eq!(notices.iter().count(), 0);
    }

    #[test]
    fn copying_keeps_identity_and_skips_held_notices() {
        let mut source = EphemeralNotices::default();
        source.push("local".into(), TextFormat::PlainText, boundary("a", 1));
        let mut target = EphemeralNotices::default();
        target.copy_from(&source);
        target.copy_from(&source);
        assert_eq!(target.iter().count(), 1);
        assert_eq!(
            target.iter().next().map(super::EphemeralNotice::id),
            source.iter().next().map(super::EphemeralNotice::id)
        );
    }
}
