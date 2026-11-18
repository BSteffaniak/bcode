//! The review's modal flows (publish and import) and re-review, wired to the surface: what
//! they need from the open review, the work they ask for (run on host tasks), and what their
//! results change.

use super::super::import::{self, ImportContext, ImportFlow, ImportOutcome, ImportRequest};
use super::super::loader::Message;
use super::super::notes::NoteScope;
use super::super::publish::{
    self, ChecklistThread, FlowOutcome, FlowRequest, PrepareContext, PublishFlow, Readiness,
};
use super::ReviewSurface;
use crate::action::Action;
use crate::context::UiContext;
use bcode_code_review::document::ItemKey;
use bcode_code_review::publish_plan::PublishThread;
use bcode_code_review_models::{
    ResolvedReviewStack, ResolvedSource, ReviewFileSummary, ReviewPullRequestTarget,
    ReviewSourceKind,
};
use bmux_keyboard::KeyStroke;
use bmux_tui::event::Event;
use bmux_tui_components::status_bar::StatusSeverity;
use bmux_tui_components::toast_stack::ToastSeverity;
use std::collections::{BTreeMap, BTreeSet};

/// An open modal flow.
#[derive(Debug)]
pub enum Modal {
    /// Publishing.
    Publish(PublishFlow),
    /// Importing an external review.
    Import(ImportFlow),
    /// The stack's history.
    History(super::super::history::HistoryPanel),
    /// Choosing what pull requests are compared with.
    Compare(crate::compare_menu::CompareMenu),
    /// Choosing two versions of one pull request.
    Versions(crate::compare_menu::VersionPicker),
    /// Choosing a version of the guide to read.
    GuideVersions(crate::picker::Picker),
}

impl ReviewSurface {
    // ----------------------------------------------------------------------------------------
    // Publish
    // ----------------------------------------------------------------------------------------

    /// Open the publish flow for a review of `targets` with `verdict` (straight to the provider's
    /// review options).
    pub(super) fn open_review(
        &mut self,
        verdict: bcode_code_review_models::ReviewVerdict,
        targets: Vec<ReviewPullRequestTarget>,
        host: &UiContext,
    ) {
        if self.review.is_none() || targets.is_empty() {
            return;
        }
        if self.notes.pending() > 0 {
            self.set_status(
                "comments are still saving; try again in a moment",
                StatusSeverity::Warning,
            );
            return;
        }
        let (flow, outcome) = PublishFlow::for_preset(
            self.readiness(),
            self.checklist_threads(),
            self.pull_targets(),
            publish::Preset { verdict, targets },
            &self.theme,
        );
        self.modal = Some(Modal::Publish(flow));
        if let FlowOutcome::Request(request) = outcome {
            self.run_publish_request(*request, host);
        }
    }

    /// Open the publish flow at its checklist.
    pub(super) fn open_publish(&mut self) {
        if self.review.is_none() {
            return;
        }
        if self.notes.pending() > 0 {
            self.set_status(
                "comments are still saving; try again in a moment",
                StatusSeverity::Warning,
            );
            return;
        }
        let flow = PublishFlow::new(
            self.readiness(),
            self.checklist_threads(),
            self.pull_targets(),
            &self.theme,
        );
        self.modal = Some(Modal::Publish(flow));
    }

    fn readiness(&self) -> Readiness {
        let mut readiness = Readiness {
            drafts: self.notes.draft_count(),
            ..Readiness::default()
        };
        if let Some(review) = &self.review {
            let progress = review.doc.viewed_progress();
            readiness.viewed = progress.viewed;
            readiness.total = progress.total;
            readiness.unviewed = review
                .doc
                .file_keys()
                .filter(|file| {
                    !review
                        .doc
                        .viewed_state(file)
                        .is_some_and(bcode_code_review::viewed::ViewedState::is_viewed)
                })
                .take(3)
                .map(|file| file.path.clone())
                .collect();
        }
        for thread in self.publish_threads() {
            let Some(first) = thread.comments.first() else {
                continue;
            };
            if first.resolved_at_ms.is_some() {
                readiness.resolved += 1;
            } else {
                if readiness.open.0.len() < 3 {
                    readiness
                        .open
                        .0
                        .push(super::annotate::location_label(&first.anchor));
                }
                readiness.open.1 += 1;
            }
        }
        readiness
    }

    /// Persisted draft threads, oldest comment first.
    fn publish_threads(&self) -> Vec<PublishThread> {
        let mut order = Vec::new();
        let mut seen = BTreeSet::new();
        for draft in self.notes.drafts() {
            if super::super::notes::is_local(&draft.comment_id)
                || !seen.insert(draft.thread_id.clone())
            {
                continue;
            }
            order.push(PublishThread {
                thread_id: draft.thread_id.clone(),
                comments: self
                    .notes
                    .thread(&draft.thread_id)
                    .into_iter()
                    .cloned()
                    .collect(),
            });
        }
        order
    }

    fn checklist_threads(&self) -> Vec<ChecklistThread> {
        self.publish_threads()
            .into_iter()
            .filter_map(|thread| {
                let first = thread.comments.first()?;
                let mut label = super::annotate::location_label(&first.anchor);
                if first.resolved_at_ms.is_some() {
                    label.push_str(" (resolved)");
                }
                if self.notes.is_published(&thread.thread_id) {
                    label.push_str(" (published)");
                }
                label.push_str(" — ");
                label.push_str(first.body.lines().next().unwrap_or_default());
                Some(ChecklistThread {
                    thread_id: thread.thread_id,
                    label,
                    included: true,
                })
            })
            .collect()
    }

    /// Pull requests the review's sources publish to, by provider.
    fn pull_targets(&self) -> BTreeMap<String, Vec<ReviewPullRequestTarget>> {
        let mut targets: BTreeMap<String, Vec<ReviewPullRequestTarget>> = BTreeMap::new();
        let resolved = self.resolved_sources();
        for source in self.sources.iter().filter(|source| source.included) {
            let mut push = |provider: &str, repository: &str, number: u64| {
                let list = targets.entry(provider.to_owned()).or_default();
                let target = ReviewPullRequestTarget {
                    provider: provider.to_owned(),
                    repository: repository.to_owned(),
                    number,
                };
                if !list.contains(&target) {
                    list.push(target);
                }
            };
            match &source.kind {
                ReviewSourceKind::PullRequest {
                    provider,
                    repository,
                    number,
                }
                | ReviewSourceKind::StackLayer {
                    provider,
                    repository,
                    number,
                } => push(provider, repository, *number),
                ReviewSourceKind::Stack {
                    provider,
                    repository,
                    ..
                } => {
                    if let Some((_, Some(stack))) = resolved.get(&source.id) {
                        for layer in &stack.layers {
                            push(provider, repository, layer.number);
                        }
                    }
                }
                _ => {}
            }
        }
        targets
    }

    /// The first resolved part of each source and its stack.
    fn resolved_sources(&self) -> BTreeMap<String, (ResolvedSource, Option<ResolvedReviewStack>)> {
        let mut resolved = BTreeMap::new();
        if let Some(review) = &self.review {
            for group in review.doc.groups() {
                resolved
                    .entry(group.source.source_id.clone())
                    .or_insert_with(|| {
                        (
                            group.source.clone(),
                            group.stack.map(|stack| stack.stack.clone()),
                        )
                    });
            }
        }
        resolved
    }

    fn prepare_context(&mut self) -> Option<PrepareContext> {
        let resolved = self.resolved_sources();
        let threads = self.publish_threads();
        let workspace = self.workspace_mut().clone();
        let review = self.review.as_ref()?;
        let mut files: BTreeMap<String, Vec<ReviewFileSummary>> = BTreeMap::new();
        let mut old_paths = BTreeMap::new();
        for file in review.doc.file_keys() {
            let Some(entry) = review.doc.entry(file) else {
                continue;
            };
            if let Some(old) = &entry.old_path {
                old_paths.insert((file.source_id.clone(), file.path.clone()), old.clone());
            }
            files
                .entry(file.source_id.clone())
                .or_default()
                .push(ReviewFileSummary {
                    path: entry.path.clone(),
                    status: entry.status,
                    additions: entry.additions,
                    deletions: entry.deletions,
                    hunks: 0,
                    is_binary: entry.has(bcode_code_review_models::ReviewIndexFlag::Binary),
                });
        }
        let target_of = |key: &crate::conversation::PullKey| ReviewPullRequestTarget {
            provider: key.provider.clone(),
            repository: key.repository.clone(),
            number: key.number,
        };
        let held = self
            .notes
            .held_replies()
            .into_iter()
            .filter(|draft| !super::super::notes::is_local(&draft.comment_id))
            .filter_map(|draft| {
                let replied =
                    super::super::notes::replied_thread(draft.anchor.reply_to.as_deref()?);
                let info = self.notes.remote(&replied)?;
                let key = self
                    .talk
                    .data
                    .links
                    .iter()
                    .find(|link| link.pull.id() == info.pull)?
                    .pull
                    .clone();
                Some(publish::HeldReplyDraft {
                    target: target_of(&key),
                    thread_id: info.remote_id.clone(),
                    body: draft.body.clone(),
                    comment_id: draft.comment_id.clone(),
                })
            })
            .collect();
        let heads = self
            .talk
            .data
            .links
            .iter()
            .map(|link| (target_of(&link.pull), link.head_oid.clone()))
            .collect();
        let preset_targets = match &self.modal {
            Some(Modal::Publish(flow)) => flow
                .preset()
                .map(|preset| preset.targets.clone())
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        Some(PrepareContext {
            repo_path: self.repo_path.clone(),
            fallback_target: NoteScope::for_workspace(&workspace).target,
            workspace,
            resolved,
            files,
            old_paths,
            threads,
            held,
            heads,
            preset_targets,
        })
    }

    fn run_publish_request(&mut self, request: FlowRequest, host: &UiContext) {
        let services = self.services.clone();
        let inbox = self.inbox.clone();
        match request {
            FlowRequest::Publishers => host.spawn(Box::pin(async move {
                let result = publish::discover(services).await;
                inbox.push(Message::Publish(publish::FlowResult::Publishers(result)));
            })),
            FlowRequest::Prepare {
                publisher,
                options,
                threads,
            } => {
                let Some(context) = self.prepare_context() else {
                    return;
                };
                host.spawn(Box::pin(async move {
                    let result =
                        publish::prepare(services, context, publisher, options, threads).await;
                    inbox.push(Message::Publish(publish::FlowResult::Prepared(result)));
                }));
            }
            FlowRequest::Submit {
                publisher,
                options,
                bundles,
                skip,
            } => {
                let workspace = (!self.is_transient()).then(|| self.workspace_mut().clone());
                host.spawn(Box::pin(async move {
                    let report =
                        publish::submit(services, workspace, publisher, options, bundles, skip)
                            .await;
                    inbox.push(Message::Publish(publish::FlowResult::Submitted(report)));
                }));
            }
        }
    }

    /// Apply publish work: advance the flow, and after a submit record the pass, reload the
    /// publish records, and report.
    pub(super) fn apply_publish(&mut self, result: publish::FlowResult) {
        let theme = self.theme;
        let report = match self.modal.as_mut() {
            Some(Modal::Publish(flow)) => flow.apply(result, &theme),
            _ => match result {
                publish::FlowResult::Submitted(report) => Some(report),
                _ => None,
            },
        };
        let Some(report) = report else {
            return;
        };
        // Replies sent with a review are drafts no more; what was reviewed is read again.
        for comment_id in &report.sent_replies {
            let _ = self.notes.delete_draft(comment_id);
        }
        if !report.reviewed.is_empty() {
            let keys = report
                .reviewed
                .iter()
                .map(|target| crate::conversation::PullKey {
                    provider: target.provider.clone(),
                    repository: target.repository.clone(),
                    number: target.number,
                })
                .collect();
            self.talk.signals.extend::<Vec<_>>(keys);
        }
        if !report.done.is_empty() {
            let labels: Vec<String> = report
                .done
                .iter()
                .map(|(_, response)| response.message.clone())
                .collect();
            self.toasts.push(
                ToastSeverity::Success,
                "Review published",
                Some(labels.join("\n")),
            );
            // Publishing completes the pass; published markers come from the records.
            self.record_pass();
            self.notes_requested = false;
        }
        if let Some((_, error)) = &report.failed {
            self.toasts.push(
                ToastSeverity::Error,
                "Publishing failed",
                Some(error.clone()),
            );
        }
        for error in report.record_errors {
            self.toasts.push(
                ToastSeverity::Warning,
                "Could not save the publish record",
                Some(error),
            );
        }
        if matches!(&self.modal, Some(Modal::Publish(flow)) if flow.finished()) {
            self.modal = None;
        }
    }

    // ----------------------------------------------------------------------------------------
    // Import
    // ----------------------------------------------------------------------------------------

    /// The review's single pull request (with its source id), when it is one pull request or
    /// stack layer.
    fn single_pull(&self) -> Option<(String, ReviewPullRequestTarget)> {
        let mut pulls = self
            .sources
            .iter()
            .filter(|source| source.included)
            .filter_map(|source| match &source.kind {
                ReviewSourceKind::PullRequest {
                    provider,
                    repository,
                    number,
                }
                | ReviewSourceKind::StackLayer {
                    provider,
                    repository,
                    number,
                } => Some((
                    source.id.clone(),
                    ReviewPullRequestTarget {
                        provider: provider.clone(),
                        repository: repository.clone(),
                        number: *number,
                    },
                )),
                _ => None,
            });
        let first = pulls.next()?;
        pulls.next().is_none().then_some(first)
    }

    /// Open the import flow (it starts by discovering importers).
    pub(super) fn open_import(&mut self, host: &UiContext) {
        if self.review.is_none() {
            return;
        }
        let (flow, request) = ImportFlow::new(self.single_pull().map(|(_, target)| target));
        self.modal = Some(Modal::Import(flow));
        self.run_import_request(request, host);
    }

    fn run_import_request(&mut self, request: ImportRequest, host: &UiContext) {
        let services = self.services.clone();
        let inbox = self.inbox.clone();
        match request {
            ImportRequest::Importers => host.spawn(Box::pin(async move {
                let importers = import::discover(services).await;
                inbox.push(Message::Import(import::ImportResult::Importers(importers)));
            })),
            ImportRequest::Import { importer, options } => {
                let target = self.single_pull();
                let workspace_id = (!self.is_transient()).then(|| self.workspace_mut().id.clone());
                let Some(review) = self.review.as_ref() else {
                    return;
                };
                let files = review
                    .doc
                    .groups()
                    .filter(|group| {
                        target
                            .as_ref()
                            .is_none_or(|(source_id, _)| &group.source.source_id == source_id)
                    })
                    .flat_map(|group| {
                        let source = group.source.clone();
                        review
                            .doc
                            .group_files(group.index)
                            .filter_map(|file| {
                                Some((source.clone(), review.doc.entry(file)?.clone()))
                            })
                            .collect::<Vec<_>>()
                    })
                    .take(import::MAX_IMPORT_FILES)
                    .collect();
                let context = ImportContext {
                    repo_path: self.repo_path.clone(),
                    workspace_id,
                    target,
                    files,
                };
                host.spawn(Box::pin(async move {
                    let result = import::import(services, context, importer, options).await;
                    inbox.push(Message::Import(import::ImportResult::Imported(result)));
                }));
            }
        }
    }

    /// Apply import work: advance the flow; a finished import shows its threads.
    pub(super) fn apply_import(&mut self, result: import::ImportResult) {
        let theme = self.theme;
        let Some(Modal::Import(flow)) = self.modal.as_mut() else {
            return;
        };
        let Some(record) = flow.apply(result, &theme) else {
            return;
        };
        self.modal = None;
        let count = record.threads.len();
        let warnings = record.warnings.clone();
        self.notes.put_external(record);
        self.sync_notes();
        self.toasts.push(
            ToastSeverity::Success,
            format!(
                "Imported {count} external review thread{}",
                if count == 1 { "" } else { "s" }
            ),
            (!warnings.is_empty()).then(|| warnings.join("\n")),
        );
    }

    // ----------------------------------------------------------------------------------------
    // Input routing
    // ----------------------------------------------------------------------------------------

    /// Keys while a modal flow is open (it captures every key).
    pub(super) fn modal_key(&mut self, stroke: KeyStroke, host: &UiContext) -> Action {
        let theme = self.theme;
        match self.modal.as_mut() {
            Some(Modal::Publish(flow)) => {
                let outcome = flow.key(stroke, &theme);
                self.publish_outcome(outcome, host)
            }
            Some(Modal::Import(flow)) => {
                let outcome = flow.key(stroke);
                self.import_outcome(outcome, host)
            }
            Some(Modal::History(panel)) => {
                let outcome = panel.key(stroke);
                self.history_outcome(outcome, host)
            }
            Some(Modal::Compare(menu)) => {
                let outcome = menu.key(stroke);
                self.compare_menu_outcome(outcome, host)
            }
            Some(Modal::Versions(picker)) => {
                let number = picker.number();
                let outcome = picker.key(stroke, &theme);
                self.version_outcome(outcome, number, host)
            }
            Some(Modal::GuideVersions(_)) => self.guide_versions_key(stroke),
            None => Action::None,
        }
    }

    /// Paste while a modal flow is open.
    pub(super) fn modal_paste(&mut self, text: &str, host: &UiContext) -> Action {
        match self.modal.as_mut() {
            Some(Modal::Publish(flow)) => {
                let outcome = flow.paste(text);
                self.publish_outcome(outcome, host)
            }
            Some(Modal::Import(flow)) => {
                let outcome = flow.paste(text);
                self.import_outcome(outcome, host)
            }
            Some(
                Modal::History(_)
                | Modal::Compare(_)
                | Modal::Versions(_)
                | Modal::GuideVersions(_),
            )
            | None => Action::None,
        }
    }

    /// Pointer events while a modal flow is open.
    pub(super) fn modal_mouse(&mut self, event: &Event, host: &UiContext) -> Action {
        let theme = self.theme;
        match self.modal.as_mut() {
            Some(Modal::Publish(flow)) => {
                let outcome = flow.mouse(event, &theme);
                self.publish_outcome(outcome, host)
            }
            Some(Modal::Import(flow)) => {
                let outcome = flow.mouse(event, &theme);
                self.import_outcome(outcome, host)
            }
            Some(Modal::Compare(menu)) => {
                let outcome = menu.mouse(event, &theme);
                self.compare_menu_outcome(outcome, host)
            }
            Some(Modal::Versions(picker)) => {
                let number = picker.number();
                let outcome = picker.mouse(event, &theme);
                self.version_outcome(outcome, number, host)
            }
            Some(Modal::GuideVersions(_)) => self.guide_versions_mouse(event),
            Some(Modal::History(_)) | None => Action::None,
        }
    }

    fn publish_outcome(&mut self, outcome: FlowOutcome, host: &UiContext) -> Action {
        match outcome {
            FlowOutcome::Close => {
                self.modal = None;
                Action::Redraw
            }
            FlowOutcome::Redraw => Action::Redraw,
            FlowOutcome::Ignored => Action::None,
            FlowOutcome::Request(request) => {
                self.run_publish_request(*request, host);
                Action::Redraw
            }
        }
    }

    fn import_outcome(&mut self, outcome: ImportOutcome, host: &UiContext) -> Action {
        match outcome {
            ImportOutcome::Close => {
                self.modal = None;
                Action::Redraw
            }
            ImportOutcome::Redraw => Action::Redraw,
            ImportOutcome::Ignored => Action::None,
            ImportOutcome::Request(request) => {
                self.run_import_request(request, host);
                Action::Redraw
            }
        }
    }

    /// Mark the review pass complete: record the reviewed revisions now.
    pub(super) fn complete_pass(&mut self) {
        if self.is_transient() {
            self.set_status(
                "save the review (ctrl+s) to track review passes",
                StatusSeverity::Warning,
            );
            return;
        }
        self.record_pass();
        self.toasts
            .push(ToastSeverity::Success, "Review pass recorded", None);
    }

    /// Whether the cursor is on an imported (read-only) thread; says so in the status line.
    pub(super) fn refuse_external_thread(&mut self) -> bool {
        let external = self.review.as_ref().is_some_and(|review| {
            matches!(
                &review.cursor,
                Some(ItemKey::Thread { thread_id }) if self.notes.external(thread_id).is_some()
            )
        });
        if external {
            self.set_status(
                "imported threads are read-only; reply on the provider",
                StatusSeverity::Info,
            );
        }
        external
    }
}
