//! The stack history on the review screen: the panel (opened with the `stack_history` action),
//! the summary of the last push on the layer strip, and opening a comparison as its own review.

use super::super::history::{self, HistoryOutcome, HistoryPanel, HistoryRequest, HistoryScope};
use super::super::loader::Message;
use super::ReviewSurface;
use super::flows::Modal;
use crate::action::Action;
use crate::context::UiContext;
use bcode_code_review::history::compare::Comparison;
use bcode_code_review_models::StackTimeline;
use bmux_tui_components::status_bar::StatusSeverity;

impl ReviewSurface {
    /// What a history is about: the stack this review last saw, and what was reviewed of it.
    fn history_scope(&self) -> Option<HistoryScope> {
        let news = self.stack_news.as_ref()?;
        let reviewed_heads = self
            .workspace
            .as_ref()
            .map(|workspace| {
                workspace
                    .reviewed_revisions
                    .values()
                    .filter_map(|revision| revision.reviewed_head_oid.clone())
                    .collect()
            })
            .unwrap_or_default();
        Some(HistoryScope {
            repo_path: self.repo_path.clone(),
            provider: news.provider.clone(),
            repository: news.repository.clone(),
            stack: news.stack.clone(),
            reviewed_heads,
        })
    }

    /// Open the history panel: the history as recorded, completed with the forge's push history
    /// (fetching old versions' commits).
    pub(super) fn open_history(&mut self, host: &UiContext) {
        let Some(scope) = self.history_scope() else {
            self.set_status(
                "Stack history needs a stack review (its stack is not known yet)",
                StatusSeverity::Info,
            );
            return;
        };
        let mut panel = HistoryPanel::loading();
        if let Some(timeline) = self.stack_history.clone() {
            panel.set_timeline(timeline);
            panel.set_note("Checking the forge for more history…");
        }
        self.modal = Some(Modal::History(panel));
        let (services, inbox) = (self.services.clone(), self.inbox.clone());
        host.spawn(Box::pin(async move {
            let result = history::load(services, scope, true).await;
            inbox.push(Message::HistoryPanel(result));
        }));
    }

    /// Load the history as recorded (no forge), for the layer strip's summary of the last push.
    pub(super) fn load_history_quietly(&self, host: &UiContext) {
        let Some(scope) = self.history_scope() else {
            return;
        };
        let (services, inbox) = (self.services.clone(), self.inbox.clone());
        host.spawn(Box::pin(async move {
            let result = history::load(services, scope, false).await;
            inbox.push(Message::History(result));
        }));
    }

    /// The quietly loaded history; failures leave the strip without a summary.
    pub(super) fn apply_history(&mut self, result: Result<StackTimeline, String>) {
        if let Ok(timeline) = result {
            self.stack_history = Some(timeline);
        }
    }

    /// The panel's history.
    pub(super) fn apply_history_panel(&mut self, result: Result<StackTimeline, String>) {
        let Some(Modal::History(panel)) = self.modal.as_mut() else {
            if let Ok(timeline) = result {
                self.stack_history = Some(timeline);
            }
            return;
        };
        match result {
            Ok(timeline) => {
                panel.set_timeline(timeline.clone());
                self.stack_history = Some(timeline);
            }
            Err(error) => panel.set_note(format!("Could not read the history: {error}")),
        }
    }

    /// A comparison is ready: open it as its own review (returning here when it closes).
    pub(super) fn apply_history_compared(
        &mut self,
        result: Result<(String, Vec<Comparison>), String>,
    ) {
        let Some(Modal::History(panel)) = self.modal.as_mut() else {
            return;
        };
        match result {
            Ok((_, comparisons)) if comparisons.is_empty() => {
                panel.set_note("Nothing changed there (rebases are left out; i includes them).");
            }
            Ok((title, comparisons)) => {
                let filtered = comparisons
                    .iter()
                    .filter(|comparison| !comparison.rebase_removed)
                    .count();
                if filtered > 0 {
                    self.set_status(
                        format!(
                            "{filtered} comparison{} could not take the rebase out and \
                             include it",
                            if filtered == 1 { "" } else { "s" }
                        ),
                        StatusSeverity::Warning,
                    );
                }
                self.modal = None;
                self.pending_action = Some(Action::OpenReview {
                    title,
                    sources: history::sources_of(&comparisons),
                });
            }
            Err(error) => panel.set_note(format!("Could not compare: {error}")),
        }
    }

    /// Answer what a key in the panel led to.
    pub(super) fn history_outcome(&mut self, outcome: HistoryOutcome, host: &UiContext) -> Action {
        match outcome {
            HistoryOutcome::Close => {
                self.modal = None;
                Action::Redraw
            }
            HistoryOutcome::Redraw => Action::Redraw,
            HistoryOutcome::Ignored => Action::None,
            HistoryOutcome::Compare(request) => {
                self.compare_history(request, host);
                Action::Redraw
            }
        }
    }

    fn compare_history(&self, request: HistoryRequest, host: &UiContext) {
        let Some(Modal::History(panel)) = self.modal.as_ref() else {
            return;
        };
        let Some(timeline) = panel.timeline().cloned() else {
            return;
        };
        let (services, inbox, repo) = (
            self.services.clone(),
            self.inbox.clone(),
            self.repo_path.clone(),
        );
        host.spawn(Box::pin(async move {
            let result = history::comparisons(services, repo, timeline, request).await;
            inbox.push(Message::HistoryCompared(result));
        }));
    }

    /// The layer strip's note about the last push: when, and what it changed.
    pub(super) fn last_push_summary(&self) -> Option<String> {
        let push = self.stack_history.as_ref()?.pushes.last()?;
        Some(format!(
            "last push {}: {}",
            history::ago(push.at_ms, history::now_ms()),
            bcode_code_review::history::push_summary(push)
        ))
    }
}
