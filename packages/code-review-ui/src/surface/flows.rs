//! The review's modal flows (publish and import) and re-review, wired to the surface: what
//! they need from the open review, the work they ask for (run on host tasks), and what their
//! results change.

use super::super::import::{self, ImportContext, ImportFlow, ImportOutcome, ImportRequest};
use super::super::loader::Message;
use super::super::notes::NoteScope;
use super::super::publish::{
    self, ChecklistThread, FlowOutcome, FlowRequest, PrepareContext, PublishFlow, Readiness,
};
use super::super::rereview::{self, PatchComparison, RereviewPlan};
use super::ReviewSurface;
use crate::action::Action;
use crate::context::UiContext;
use bcode_code_review::document::{FileKey, ItemKey};
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
}

impl ReviewSurface {
    // ----------------------------------------------------------------------------------------
    // Publish
    // ----------------------------------------------------------------------------------------

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
        Some(PrepareContext {
            repo_path: self.repo_path.clone(),
            fallback_target: NoteScope::for_workspace(&workspace).target,
            workspace,
            resolved,
            files,
            old_paths,
            threads,
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
            None => Action::None,
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
            None => Action::None,
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

    // ----------------------------------------------------------------------------------------
    // Re-review
    // ----------------------------------------------------------------------------------------

    /// After the first open of a durable review: compare each source's current head with the
    /// last recorded pass.
    pub(super) fn start_rereview(&mut self, host: &UiContext) {
        if self.rereview_checked || self.is_transient() || self.since_last.is_some() {
            return;
        }
        self.rereview_checked = true;
        let recorded = self.workspace_mut().reviewed_revisions.clone();
        let changed: Vec<_> = self
            .pass_revisions
            .iter()
            .filter_map(|(source_id, current)| {
                let reviewed = recorded.get(source_id)?;
                let new_head = current.reviewed_head_oid.clone()?;
                (reviewed.reviewed_head_oid.as_deref() != Some(new_head.as_str()))
                    .then(|| (source_id.clone(), reviewed.clone(), new_head))
            })
            .collect();
        if changed.is_empty() {
            return;
        }
        let services = self.services.clone();
        let repo = self.repo_path.clone();
        let inbox = self.inbox.clone();
        host.spawn(Box::pin(async move {
            let plan = rereview::analyze(services, repo, changed).await;
            inbox.push(Message::Rereview(Box::new(plan)));
        }));
    }

    /// The comparison finished: offer the delta view and carry unchanged patches of rebased
    /// sources over as viewed.
    pub(super) fn apply_rereview(&mut self, plan: RereviewPlan, host: &UiContext) {
        if plan.is_empty() {
            return;
        }
        let key = self
            .action_keys
            .get("since_last_review")
            .cloned()
            .unwrap_or_else(|| "L".to_owned());
        self.toasts.push(
            ToastSeverity::Info,
            plan.summary(),
            Some(format!(
                "{key}: show only the changes since your last review"
            )),
        );
        self.compare_patches(&plan, host);
        self.rereview = Some(plan);
    }

    /// Compare the patches of rebased sources' files with the reviewed ones.
    fn compare_patches(&self, plan: &RereviewPlan, host: &UiContext) {
        let Some(review) = &self.review else {
            return;
        };
        let mut comparisons = Vec::new();
        let mut groups = Vec::new();
        for changed in plan
            .changed
            .iter()
            .filter(|changed| !changed.fast_forward())
        {
            let (Some(old_base), Some(old_head)) = (
                changed.reviewed.reviewed_base_oid.clone(),
                changed.reviewed.reviewed_head_oid.clone(),
            ) else {
                continue;
            };
            if changed.compare.is_some_and(|compare| !compare.old_exists) {
                continue;
            }
            for group in review.doc.groups() {
                if group.source.source_id != changed.source_id || group.source.part.is_some() {
                    continue;
                }
                comparisons.push(PatchComparison {
                    group: groups.len(),
                    old: (old_base.clone(), old_head.clone()),
                    new: (group.source.base.clone(), group.source.head.clone()),
                    paths: review
                        .doc
                        .group_files(group.index)
                        .map(|file| file.path.clone())
                        .collect(),
                });
                groups.push(group.source.source_id.clone());
            }
        }
        if comparisons.is_empty() {
            return;
        }
        let services = self.services.clone();
        let repo = self.repo_path.clone();
        let inbox = self.inbox.clone();
        host.spawn(Box::pin(async move {
            let result = rereview::unchanged_patches(services, repo, comparisons)
                .await
                .map(|unchanged| {
                    unchanged
                        .into_iter()
                        .map(|(group, path, patch)| (groups[group].clone(), path, patch))
                        .collect()
                });
            inbox.push(Message::Carried(result));
        }));
    }

    /// Unchanged patches arrived: mark those files viewed with their new identity and show
    /// them de-emphasized.
    pub(super) fn apply_carried(&mut self, result: Result<Vec<(String, String, String)>, String>) {
        let unchanged = match result {
            Ok(unchanged) => unchanged,
            Err(error) => {
                self.toasts.push(
                    ToastSeverity::Warning,
                    "Could not compare patches with the last review",
                    Some(error),
                );
                return;
            }
        };
        let Some(review) = self.review.as_ref() else {
            return;
        };
        let mut marks = Vec::new();
        for (source_id, path, patch) in &unchanged {
            let key = FileKey::new(source_id.clone(), None, path.clone());
            if let Some((identity, _)) = review.doc.viewed_identity(&key) {
                marks.push((identity, patch.clone()));
            }
            self.carried.insert(key);
        }
        let workspace = self.workspace_mut();
        for (identity, patch) in &marks {
            bcode_code_review::viewed::mark_viewed(workspace, &identity.with_patch(Some(patch)));
        }
        if let Some(review) = self.review.as_mut() {
            review.doc.set_carried_over(&self.carried);
            review.note_splices();
        }
        self.apply_viewed();
        if !unchanged.is_empty() {
            self.toasts.push(
                ToastSeverity::Info,
                format!(
                    "{} file{} unchanged since your last review (marked viewed)",
                    unchanged.len(),
                    if unchanged.len() == 1 { "" } else { "s" }
                ),
                None,
            );
        }
    }

    /// Switch between the whole review and the changes since the last review.
    pub(super) fn toggle_since_last(&mut self, host: &UiContext) {
        if let Some(full) = self.since_last.take() {
            self.sources = full;
            self.set_status("showing the whole review", StatusSeverity::Info);
            self.switching = true;
            self.reopen(host);
            return;
        }
        let Some(plan) = self.rereview.as_ref() else {
            self.set_status(
                if self.is_transient() {
                    "save the review to track review passes"
                } else {
                    "nothing changed since your last review pass"
                },
                StatusSeverity::Info,
            );
            return;
        };
        let delta = plan.delta_sources(&self.sources);
        let full = std::mem::replace(&mut self.sources, delta);
        self.since_last = Some(full);
        self.set_status(
            "showing the changes since your last review",
            StatusSeverity::Info,
        );
        self.switching = true;
        self.reopen(host);
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
        self.rereview = None;
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
