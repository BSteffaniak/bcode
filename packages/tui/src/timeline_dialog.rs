//! Timeline dialog state for browsing user-sent messages.

use bmux_keyboard::KeyStroke;

/// Outcome from one timeline-dialog keyboard update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineDialogOutcome {
    /// Selection changed and the dialog remains open.
    Handled,
    /// Activate the selected timeline entry and close the dialog.
    Activate,
    /// Close the dialog without activating an entry.
    Cancel,
    /// Read canonical milestone annotations.
    Milestones,
    /// Return to resident user prompts.
    Prompts,
    /// Load the next bounded milestone page.
    NextMilestonePage,
    /// The key is not owned by the timeline dialog.
    Ignored,
}

/// One selectable user-message timeline row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineEntry {
    sequence: u64,
    timestamp_ms: u64,
    text: String,
}

impl TimelineEntry {
    /// Adapt a canonical milestone into a source-navigation entry, never a rewind target.
    #[must_use]
    pub fn from_milestone(entry: &bcode_session_models::SessionMilestoneEntry) -> Self {
        Self::new(
            entry.milestone.source.anchor_sequence,
            entry.timestamp_ms,
            format!(
                "{} — {}",
                entry.milestone.header, entry.milestone.description
            ),
        )
    }
    /// Create a timeline entry identified by its source event sequence.
    #[must_use]
    pub fn new(sequence: u64, timestamp_ms: u64, text: impl Into<String>) -> Self {
        Self {
            sequence,
            timestamp_ms,
            text: text.into(),
        }
    }

    /// Return the source event sequence.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Return the event timestamp in Unix milliseconds.
    #[must_use]
    pub const fn timestamp_ms(&self) -> u64 {
        self.timestamp_ms
    }

    /// Return the message text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// Ephemeral identity of one terminal milestone page request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MilestonePageRequest {
    /// Session owning the canonical page.
    pub session_id: bcode_session_models::SessionId,
    /// Monotonic local request generation.
    pub generation: u64,
    /// Exclusive publication cursor.
    pub after_sequence: Option<u64>,
}

/// Terminal-only page request correlation; not durable session state.
#[derive(Debug, Default)]
pub struct MilestonePageRequests {
    generation: u64,
    pending: Option<MilestonePageRequest>,
}

impl MilestonePageRequests {
    /// Start a new request and remove stale navigable entries.
    pub fn begin(
        &mut self,
        session_id: bcode_session_models::SessionId,
        after_sequence: Option<u64>,
        dialog: &mut TimelineDialogState,
    ) -> Option<MilestonePageRequest> {
        self.pending = None;
        self.generation = self.generation.checked_add(1)?;
        let request = MilestonePageRequest {
            session_id,
            generation: self.generation,
            after_sequence,
        };
        self.pending = Some(request);
        *dialog = TimelineDialogState::new(Vec::new());
        dialog.loading = true;
        Some(request)
    }

    /// Invalidate pending work when closing or returning to prompts.
    pub const fn cancel(&mut self) {
        self.pending = None;
    }

    /// Whether a page is pending.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Accept exactly the current response, including failures, for the viewed session.
    pub fn accept(
        &mut self,
        request: MilestonePageRequest,
        viewed: Option<bcode_session_models::SessionId>,
        dialog: Option<&mut TimelineDialogState>,
    ) -> bool {
        if self.pending != Some(request) {
            return false;
        }
        if viewed != Some(request.session_id) {
            self.pending = None;
            return false;
        }
        let Some(dialog) = dialog else {
            return false;
        };
        self.pending = None;
        dialog.loading = false;
        true
    }
}

/// Timeline modal state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineDialogState {
    entries: Vec<TimelineEntry>,
    next_after_sequence: Option<u64>,
    loading: bool,
    selected: usize,
    scroll: usize,
}

impl TimelineDialogState {
    /// Create a timeline dialog from selectable entries.
    #[must_use]
    pub const fn new(entries: Vec<TimelineEntry>) -> Self {
        let selected = entries.len().saturating_sub(1);
        Self {
            entries,
            next_after_sequence: None,
            loading: false,
            selected,
            scroll: 0,
        }
    }

    /// Replace, rather than accumulate, a bounded canonical milestone page.
    pub fn replace_milestone_page(&mut self, page: &bcode_session_models::SessionMilestonePage) {
        *self = Self::new(
            page.milestones
                .iter()
                .map(TimelineEntry::from_milestone)
                .collect(),
        );
        self.next_after_sequence = page.next_after_sequence;
    }

    /// Exclusive publication cursor for the next milestone page.
    #[must_use]
    pub const fn next_after_sequence(&self) -> Option<u64> {
        self.next_after_sequence
    }

    /// Whether navigation is disabled while canonical data is loading.
    #[must_use]
    pub const fn is_loading(&self) -> bool {
        self.loading
    }

    /// Return timeline entries.
    #[must_use]
    pub fn entries(&self) -> &[TimelineEntry] {
        &self.entries
    }

    /// Return the selected entry index.
    #[must_use]
    pub const fn selected(&self) -> usize {
        self.selected
    }

    /// Return the first rendered entry index.
    #[must_use]
    pub const fn scroll(&self) -> usize {
        self.scroll
    }

    /// Return the selected entry.
    #[must_use]
    pub fn selected_entry(&self) -> Option<&TimelineEntry> {
        self.entries.get(self.selected)
    }

    /// Move selection up by one entry.
    pub const fn select_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move selection down by one entry.
    pub const fn select_next(&mut self) {
        if self.selected + 1 < self.entries.len() {
            self.selected += 1;
        }
    }

    /// Move selection up by a page.
    pub fn page_previous(&mut self, rows: usize) {
        self.selected = self.selected.saturating_sub(rows.max(1));
    }

    /// Move selection down by a page.
    pub fn page_next(&mut self, rows: usize) {
        if self.entries.is_empty() {
            return;
        }
        self.selected = self
            .selected
            .saturating_add(rows.max(1))
            .min(self.entries.len().saturating_sub(1));
    }

    /// Select the first entry.
    pub const fn select_first(&mut self) {
        self.selected = 0;
    }

    /// Select the last entry.
    pub const fn select_last(&mut self) {
        self.selected = self.entries.len().saturating_sub(1);
    }

    /// Handle one keyboard input through the dialog's navigation policy.
    #[cfg(test)]
    pub fn handle_key(&mut self, stroke: KeyStroke) -> TimelineDialogOutcome {
        self.handle_key_with_keymap(
            stroke,
            &super::keymap::BmuxKeyMap::from_config(&bcode_config::TuiConfig::default()),
        )
    }

    /// Handle input through the effective registry.
    pub fn handle_key_with_keymap(
        &mut self,
        stroke: KeyStroke,
        keymap: &super::keymap::BmuxKeyMap,
    ) -> TimelineDialogOutcome {
        match keymap.action("timeline", stroke) {
            Some("tui.timeline.milestones") => TimelineDialogOutcome::Milestones,
            Some("tui.timeline.prompts") => TimelineDialogOutcome::Prompts,
            Some("tui.timeline.nextMilestonePage") => TimelineDialogOutcome::NextMilestonePage,
            Some("tui.control.up") => {
                self.select_previous();
                TimelineDialogOutcome::Handled
            }
            Some("tui.control.down") => {
                self.select_next();
                TimelineDialogOutcome::Handled
            }
            Some("tui.control.page_up") => {
                self.page_previous(10);
                TimelineDialogOutcome::Handled
            }
            Some("tui.control.page_down") => {
                self.page_next(10);
                TimelineDialogOutcome::Handled
            }
            Some("tui.control.home") => {
                self.select_first();
                TimelineDialogOutcome::Handled
            }
            Some("tui.control.end") => {
                self.select_last();
                TimelineDialogOutcome::Handled
            }
            Some("tui.control.submit") if self.loading => TimelineDialogOutcome::Handled,
            Some("tui.control.submit") => TimelineDialogOutcome::Activate,
            Some("tui.control.cancel") => TimelineDialogOutcome::Cancel,
            _ => TimelineDialogOutcome::Ignored,
        }
    }

    /// Keep selected entry visible in the provided viewport height.
    pub fn sync_scroll(&mut self, visible_rows: usize) {
        let visible_rows = visible_rows.max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll.saturating_add(visible_rows) {
            self.scroll = self.selected.saturating_add(1).saturating_sub(visible_rows);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TimelineDialogOutcome, TimelineDialogState, TimelineEntry};
    use bmux_keyboard::{KeyCode, KeyStroke};

    fn state() -> TimelineDialogState {
        TimelineDialogState::new(vec![
            TimelineEntry::new(1, 10, "first"),
            TimelineEntry::new(2, 20, "second"),
        ])
    }

    #[test]
    fn milestone_navigation_uses_source_not_publication_sequence() {
        let entry = bcode_session_models::SessionMilestoneEntry {
            event_sequence: 99,
            timestamp_ms: 1234,
            milestone: bcode_session_models::SessionMilestone {
                evaluation_id: "evaluation".to_owned(),
                source: bcode_session_models::SessionMilestoneSource {
                    start_sequence: 4,
                    end_sequence: 8,
                    anchor_sequence: 6,
                },
                header: "子 👩‍💻".to_owned(),
                description: "e\u{301} settled work".to_owned(),
                provenance: bcode_session_models::SessionMilestoneProvenance {
                    provider_id: "provider".to_owned(),
                    model_id: "model".to_owned(),
                    input_truncated: true,
                },
            },
        };
        let mut dialog = TimelineDialogState::new(vec![TimelineEntry::from_milestone(&entry)]);
        dialog.replace_milestone_page(&bcode_session_models::SessionMilestonePage {
            milestones: vec![entry.clone(); 200],
            next_after_sequence: Some(99),
        });
        assert_eq!(dialog.entries().len(), 200);
        assert_eq!(dialog.next_after_sequence(), Some(99));
        dialog.replace_milestone_page(&bcode_session_models::SessionMilestonePage {
            milestones: vec![entry],
            next_after_sequence: None,
        });
        assert_eq!(dialog.entries().len(), 1);
        assert_eq!(dialog.next_after_sequence(), None);
        dialog.sync_scroll(0);
        assert_eq!(dialog.selected_entry().unwrap().sequence(), 6);
        assert_eq!(dialog.selected_entry().unwrap().timestamp_ms(), 1234);
        assert!(dialog.selected_entry().unwrap().text().contains("子 👩‍💻"));
        assert_eq!(
            dialog.handle_key(KeyStroke::simple(KeyCode::Enter)),
            TimelineDialogOutcome::Activate
        );
    }

    #[test]
    fn milestone_controls_use_effective_keymap_and_pages_replace() {
        let mut config = bcode_config::TuiConfig::default();
        config.keybindings.scopes.insert(
            "timeline".to_owned(),
            std::collections::BTreeMap::from([
                ("m".to_owned(), String::new()),
                ("ctrl+m".to_owned(), "tui.timeline.milestones".to_owned()),
            ]),
        );
        let keymap = super::super::keymap::BmuxKeyMap::try_from_config(&config).unwrap();
        let mut dialog = state();
        assert_eq!(
            dialog.handle_key_with_keymap(KeyStroke::simple(KeyCode::Char('m')), &keymap),
            TimelineDialogOutcome::Ignored
        );
        assert_eq!(
            dialog.handle_key_with_keymap(
                bmux_keyboard::parse_key_stroke("ctrl+m").unwrap(),
                &keymap
            ),
            TimelineDialogOutcome::Milestones
        );
        assert_eq!(
            dialog.handle_key(KeyStroke::simple(KeyCode::Char('n'))),
            TimelineDialogOutcome::NextMilestonePage
        );
        assert_eq!(
            dialog.handle_key(KeyStroke::simple(KeyCode::Char('p'))),
            TimelineDialogOutcome::Prompts
        );
        dialog.replace_milestone_page(&bcode_session_models::SessionMilestonePage {
            milestones: Vec::new(),
            next_after_sequence: Some(200),
        });
        assert!(dialog.entries().is_empty());
        assert_eq!(dialog.next_after_sequence(), Some(200));
        dialog.replace_milestone_page(&bcode_session_models::SessionMilestonePage {
            milestones: Vec::new(),
            next_after_sequence: None,
        });
        assert_eq!(dialog.next_after_sequence(), None);
        assert_eq!(dialog.selected(), 0);
    }

    #[test]
    fn milestone_loading_and_out_of_order_pages_are_fenced() {
        let session = bcode_session_models::SessionId::new();
        let other = bcode_session_models::SessionId::new();
        let mut requests = super::MilestonePageRequests::default();
        let mut dialog = state();
        let first = requests.begin(session, None, &mut dialog).unwrap();
        assert!(dialog.entries().is_empty());
        assert!(dialog.is_loading());
        assert_eq!(
            dialog.handle_key(KeyStroke::simple(KeyCode::Enter)),
            TimelineDialogOutcome::Handled
        );
        let reset = requests.begin(session, None, &mut dialog).unwrap();
        assert!(reset.generation > first.generation);
        assert!(!requests.accept(first, Some(session), Some(&mut dialog)));
        assert!(dialog.is_loading());
        assert!(!requests.accept(
            super::MilestonePageRequest {
                after_sequence: Some(99),
                ..reset
            },
            Some(session),
            Some(&mut dialog)
        ));
        assert!(requests.accept(reset, Some(session), Some(&mut dialog)));
        dialog.replace_milestone_page(&bcode_session_models::SessionMilestonePage {
            milestones: Vec::new(),
            next_after_sequence: Some(200),
        });
        let next = requests
            .begin(session, dialog.next_after_sequence(), &mut dialog)
            .unwrap();
        assert_eq!(next.after_sequence, Some(200));
        let newer_reset = requests.begin(session, None, &mut dialog).unwrap();
        assert!(!requests.accept(next, Some(session), Some(&mut dialog))); // stale success or failure
        assert!(requests.accept(newer_reset, Some(session), Some(&mut dialog)));
        assert!(!requests.accept(newer_reset, Some(session), Some(&mut dialog))); // duplicate
        let closed = requests.begin(session, None, &mut dialog).unwrap();
        requests.cancel();
        assert!(!requests.accept(closed, Some(session), None));
        let reopened = requests.begin(session, None, &mut dialog).unwrap();
        assert!(!requests.accept(closed, Some(session), Some(&mut dialog)));
        assert!(requests.accept(reopened, Some(session), Some(&mut dialog)));
        let switched = requests.begin(other, None, &mut dialog).unwrap();
        assert!(!requests.accept(reopened, Some(other), Some(&mut dialog)));
        assert!(requests.accept(switched, Some(other), Some(&mut dialog)));
        let departed = requests.begin(session, None, &mut dialog).unwrap();
        assert!(!requests.accept(departed, Some(other), Some(&mut dialog)));
        assert!(!requests.is_pending());
        assert!(!requests.accept(departed, Some(session), Some(&mut dialog)));
    }

    #[test]
    fn keyboard_policy_owns_navigation_activation_and_cancel() {
        let mut dialog = state();

        assert_eq!(
            dialog.handle_key(KeyStroke::simple(KeyCode::Up)),
            TimelineDialogOutcome::Handled
        );
        assert_eq!(dialog.selected(), 0);
        assert_eq!(
            dialog.handle_key(KeyStroke::simple(KeyCode::Enter)),
            TimelineDialogOutcome::Activate
        );
        assert_eq!(
            dialog.handle_key(KeyStroke::simple(KeyCode::Escape)),
            TimelineDialogOutcome::Cancel
        );
        assert_eq!(
            dialog.handle_key(KeyStroke::simple(KeyCode::Char('x'))),
            TimelineDialogOutcome::Ignored
        );
    }
}
