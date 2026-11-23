//! The stack history on the review screen: the matrix (opened with the `stack_history`
//! action), whose comparisons apply in place, and the summary of the last push.

use super::super::history::{self, HistoryOutcome, HistoryPanel, HistoryRequest, HistoryScope};
use super::super::loader::Message;
use super::ReviewSurface;
use super::flows::Modal;
use crate::action::Action;
use crate::context::UiContext;
use bcode_code_review::history::version_at;
use bcode_code_review_models::{ReviewCompareBaseline, StackTimeline};
use bmux_tui_components::status_bar::StatusSeverity;
use std::collections::BTreeMap;

impl ReviewSurface {
    /// Standalone sources (including PR layers) participate in a workspace pass regardless
    /// of focus. A commit series advances only the member(s) or aggregate being reviewed.
    /// Record the original revisions, not the temporary sides of a comparison.
    pub(super) fn active_pass_revisions(
        &self,
    ) -> BTreeMap<String, bcode_code_review_models::ReviewedRevision> {
        let Some(review) = &self.review else {
            return BTreeMap::default();
        };
        review
            .doc
            .groups()
            .filter(|group| {
                !group.loading
                    && (matches!(
                        group.key,
                        bcode_code_review_models::ReviewGroupKey::Standalone { .. }
                    ) || (!group.hidden && !group.alternate))
            })
            .filter_map(|group| {
                let key = group.key.selector();
                Some((key.clone(), self.pass_revisions.get(&key)?.clone()))
            })
            .collect()
    }

    /// The review's recorded passes once the pass `revisions` (from
    /// [`Self::active_pass_revisions`]) is recorded: the pass replaces every entry it covers
    /// and every standalone source's; a series group the pass leaves out (a member not shown,
    /// say) keeps its entry while its source is in the review; entries of sources the review
    /// no longer has are dropped.
    pub(super) fn passes_with(
        &self,
        revisions: BTreeMap<String, bcode_code_review_models::ReviewedRevision>,
    ) -> BTreeMap<String, bcode_code_review_models::ReviewedRevision> {
        let mut merged: BTreeMap<_, _> = self
            .workspace
            .iter()
            .flat_map(|workspace| &workspace.reviewed_revisions)
            .filter(|(key, _)| {
                let owner = bcode_code_review_models::ReviewGroupKey::source_id_of_selector(key);
                owner != key.as_str() && self.sources.iter().any(|source| source.id == owner)
            })
            .map(|(key, revision)| (key.clone(), revision.clone()))
            .collect();
        merged.extend(revisions);
        merged
    }

    /// What a history is about: the stack this review last saw, and the heads the reviewer
    /// engaged with (their looks, and the passes of this review before looks were listed).
    fn history_scope(&self) -> Option<HistoryScope> {
        let news = self.stack_news.as_ref()?;
        let mut reviewed_heads = self.comparing.engaged_heads();
        if let Some(workspace) = self.workspace.as_ref() {
            reviewed_heads.extend(
                self.sources
                    .iter()
                    .filter(|source| {
                        matches!(
                            &source.kind,
                            bcode_code_review_models::ReviewSourceKind::PullRequest { .. }
                                | bcode_code_review_models::ReviewSourceKind::StackLayer { .. }
                                | bcode_code_review_models::ReviewSourceKind::Stack { .. }
                        )
                    })
                    .filter_map(|source| workspace.reviewed_revisions.get(&source.id))
                    .filter_map(|revision| revision.reviewed_head_oid.clone()),
            );
        }
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
        panel.set_reviews(self.review_marks());
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

    /// Each layer's approvals and requests for changes, from its conversation.
    fn review_marks(&self) -> std::collections::BTreeMap<u64, Vec<history::ReviewMark>> {
        use bcode_code_review_models::ReviewState;
        self.talk
            .data
            .links
            .iter()
            .filter_map(|link| {
                let activity = self.talk.data.activity(&link.pull)?;
                let marks = activity
                    .latest_reviews()
                    .into_iter()
                    .filter(|review| {
                        matches!(
                            review.state,
                            ReviewState::Approved | ReviewState::ChangesRequested
                        )
                    })
                    .map(|review| history::ReviewMark {
                        login: review.author.login.clone(),
                        approved: review.state == ReviewState::Approved,
                        commit: review.commit_oid.clone(),
                    })
                    .collect();
                Some((link.pull.number, marks))
            })
            .collect()
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
                self.modal = None;
                self.compare_history(request, host);
                Action::Redraw
            }
            HistoryOutcome::OnlyChanged => {
                self.toggle_only_changed();
                Action::Redraw
            }
        }
    }

    /// Apply a comparison the matrix asked for to the open review.
    fn compare_history(&mut self, request: HistoryRequest, host: &UiContext) {
        match request {
            HistoryRequest::Layer {
                number,
                from_ms,
                to_ms,
            } => {
                let indices = self.stack_history.as_ref().and_then(|history| {
                    let layer = history.layers.iter().find(|layer| layer.number == number)?;
                    Some((version_at(layer, from_ms)?, version_at(layer, to_ms)?))
                });
                match indices {
                    Some((from, to)) if from != to => self.pick_versions(number, from, to, host),
                    _ => self.set_status(
                        format!("#{number} did not change between those pushes"),
                        bmux_tui_components::status_bar::StatusSeverity::Info,
                    ),
                }
            }
            HistoryRequest::Between { from_ms, to_ms } => {
                self.set_compare_baseline(ReviewCompareBaseline::Between { from_ms, to_ms }, host);
            }
            HistoryRequest::StackAt { at_ms } => self.compare_stack_at(at_ms, host),
            HistoryRequest::SinceLooked => {
                self.set_compare_baseline(ReviewCompareBaseline::LastLooked, host);
            }
        }
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
