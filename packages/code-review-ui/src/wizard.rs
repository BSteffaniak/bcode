//! The new-review wizard: a BMUX `stepper` over a [`Picker`] per step.
//!
//! 1. **Source type**: working tree, current branch, compare branches, commits, pull request,
//!    stack, or files.
//! 2. **Details**, by type: unstaged / staged / both with counts (`review.repo.state`); the
//!    current branch's base (detected base prefilled, any reference pickable); base and head
//!    references (`review.refs.search`); commits from the paged log (`review.commits.page`) with
//!    `tab` marking commits and `shift+tab` marking the contiguous range from the last mark; the
//!    provider's pull requests (review requested from me first); a pull request's stack (whole
//!    stack, step through every layer, or one layer); repository files (`review.files.search`),
//!    marked with `tab`, with `:START-END` selecting a line range.
//! 3. **Title** (prefilled) and the sources so far; "Add another source" returns to step 1 for
//!    a composite review, and "Create review" finishes.
//!
//! **Editing a review's sources** ([`Wizard::editing`]) opens step 3 with
//! the review's title and sources: sources can be added (step 1 again), removed (`enter` on
//! one), and included or excluded (`tab` on one); "Save changes" returns them with their ids
//! kept, so threads and viewed marks stay attached.
//!
//! Every list loads on host-spawned tasks, paged on demand; stale answers (an older query) are
//! dropped. Without a review-source provider, or when it fails (for example without
//! credentials), the pull request and stack steps say so and every other type still works.

use super::chrome::{self, Status};
use super::entry::{NO_PROVIDER, default_title};
use super::home::model::PullsAnswer;
use super::picker::{Picker, PickerItem, PickerMessage, PickerOutcome};
use super::theme::ReviewTheme;
use crate::context::UiContext;
use crate::services::ReviewServices;
use bcode_code_review_models::{
    PullRequestSummary, ReviewCommitEntry, ReviewCommitsPageRequest, ReviewCommitsPageResponse,
    ReviewFilesSearchRequest, ReviewFilesSearchResponse, ReviewRefEntry, ReviewRefKind,
    ReviewRefsSearchRequest, ReviewRefsSearchResponse, ReviewRepoStateResponse, ReviewSource,
    ReviewSourceKind, ReviewStack,
};
use bmux_keyboard::KeyStroke;
use bmux_tui::event::Event;
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::text::{Line, Span};
use bmux_tui_components::status_bar::StatusSeverity;
use bmux_tui_components::stepper::{StepItem, StepStatus, StepperComponent, StepperPolicy};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Keymap scope of the wizard.
pub const WIZARD_SCOPE: &str = "code-review-wizard";
/// Page size of reference, commit, and file listings.
const PAGE: u32 = 100;

/// Source types of step 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SourceType {
    /// Unstaged, staged, or both.
    WorkingTree,
    /// The current branch against its base.
    CurrentBranch,
    /// A base and a head reference.
    CompareBranches,
    /// Commits from the log.
    Commits,
    /// A provider pull request.
    PullRequest,
    /// A provider stack.
    Stack,
    /// Repository files or line ranges.
    Files,
}

impl SourceType {
    /// Every type, in menu order.
    pub const ALL: [Self; 7] = [
        Self::WorkingTree,
        Self::CurrentBranch,
        Self::CompareBranches,
        Self::Commits,
        Self::PullRequest,
        Self::Stack,
        Self::Files,
    ];

    /// Stable id.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::WorkingTree => "working-tree",
            Self::CurrentBranch => "current-branch",
            Self::CompareBranches => "compare",
            Self::Commits => "commits",
            Self::PullRequest => "pull-request",
            Self::Stack => "stack",
            Self::Files => "files",
        }
    }

    /// Menu label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::WorkingTree => "Working tree",
            Self::CurrentBranch => "Current branch",
            Self::CompareBranches => "Compare branches",
            Self::Commits => "Commits",
            Self::PullRequest => "Pull request",
            Self::Stack => "Stack",
            Self::Files => "Files",
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::WorkingTree => "unstaged, staged, or both",
            Self::CurrentBranch => "everything on this branch since its base",
            Self::CompareBranches => "any base and head reference",
            Self::Commits => "one commit, a range, or a selection",
            Self::PullRequest => "an open pull request",
            Self::Stack => "stacked pull requests: whole, step through, or one layer",
            Self::Files => "whole files or line ranges",
        }
    }

    fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.id() == id)
    }
}

/// What an input on the wizard asks the app to do.
/// Picker id of the stack step's over-limit warning row (`enter` dismisses it).
const STACK_LIMIT_WARNING_ID: &str = "stack-limit-warning";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WizardOutcome {
    /// Nothing happened.
    Ignored,
    /// The wizard changed.
    Redraw,
    /// Leave the wizard (back to home).
    Cancel,
    /// Create a durable review.
    Create {
        /// Title.
        title: String,
        /// Sources, in order.
        sources: Vec<ReviewSourceKind>,
    },
    /// Save the edited sources of the review being edited.
    Update {
        /// The edited workspace: its title and sources changed, everything else kept.
        workspace: Box<bcode_code_review_models::ReviewWorkspace>,
    },
}

/// A source chosen so far: its kind and label, and (when editing) its id and inclusion.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Chosen {
    kind: ReviewSourceKind,
    label: String,
    /// Id of a source the edited review already had.
    id: Option<String>,
    included: bool,
}

/// Step 2 of one source type.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Details {
    WorkingTree,
    Branch,
    Compare { base: Option<String> },
    Commits,
    Pull,
    Stack(StackStage),
    Files,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StackStage {
    Pick,
    Resolving(u64),
    Mode(Box<ReviewStack>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Type,
    Details(Details),
    Confirm,
}

/// Results delivered to the wizard.
#[derive(Debug)]
enum WizardMessage {
    State(Result<ReviewRepoStateResponse, String>),
    Refs {
        seq: u64,
        append: bool,
        result: Result<ReviewRefsSearchResponse, String>,
    },
    Commits {
        seq: u64,
        append: bool,
        result: Result<ReviewCommitsPageResponse, String>,
    },
    Files {
        seq: u64,
        append: bool,
        result: Result<ReviewFilesSearchResponse, String>,
    },
    Pulls(Result<Option<PullsAnswer>, String>),
    Stack {
        number: u64,
        result: Result<ReviewStack, String>,
    },
}

#[derive(Debug, Clone, Default)]
struct Inbox(Arc<Mutex<Vec<WizardMessage>>>);

impl Inbox {
    fn push(&self, message: WizardMessage) {
        if let Ok(mut queue) = self.0.lock() {
            queue.push(message);
        }
    }

    fn drain(&self) -> Vec<WizardMessage> {
        self.0
            .lock()
            .map(|mut queue| std::mem::take(&mut *queue))
            .unwrap_or_default()
    }
}

/// A paged listing behind the step's picker.
#[derive(Debug, Default)]
struct Listing {
    /// Sequence number of the newest request; older answers are dropped.
    seq: u64,
    /// Whether a request is running.
    loading: bool,
    /// The query changed while a request ran (request again when it finishes).
    again: bool,
    /// `enter` was pressed before the listing answered the current query; it runs once the
    /// answer is shown, so it never picks a row of an older query.
    enter_pending: bool,
    /// Cursor of the next page.
    next: Option<String>,
    refs: Vec<ReviewRefEntry>,
    commits: Vec<ReviewCommitEntry>,
    files: Vec<String>,
    error: Option<String>,
}

impl Listing {
    /// Empty the listing for another step. The sequence number keeps counting, so an answer to
    /// a request of the previous step that is still running is dropped instead of being taken
    /// for the new step's answer.
    fn reset(&mut self) {
        *self = Self {
            seq: self.seq,
            ..Self::default()
        };
    }
}

/// The new-review wizard.
pub struct Wizard {
    services: ReviewServices,
    repo: PathBuf,
    inbox: Inbox,
    step: Step,
    picker: Picker,
    state: Option<ReviewRepoStateResponse>,
    /// Why the repository's state could not be read (outside a git repository, for example).
    state_error: Option<String>,
    state_requested: bool,
    pulls: Option<Result<Option<PullsAnswer>, String>>,
    pulls_requested: bool,
    listing: Listing,
    marked: Vec<String>,
    /// Last explicitly marked id (start of a range mark).
    anchor: Option<String>,
    sources: Vec<Chosen>,
    /// The review whose sources are edited.
    editing: Option<Box<bcode_code_review_models::ReviewWorkspace>>,
    /// Title typed before adding another source.
    typed_title: Option<String>,
    /// Source type of the details step last entered.
    chosen_type: Option<SourceType>,
    /// Source type to focus once step 1's items are shown again.
    focus_type: Option<SourceType>,
    status: Option<Status>,
    action_keys: BTreeMap<String, String>,
    pending_items: bool,
    /// The stack (by the pull request it was resolved for) whose over-limit warning was
    /// dismissed.
    stack_limit_dismissed: Option<u64>,
}

impl std::fmt::Debug for Wizard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wizard")
            .field("step", &self.step)
            .field("sources", &self.sources.len())
            .finish_non_exhaustive()
    }
}

/// Hint actions of the wizard scope.
const HINT_ACTIONS: &[&str] = &["wizard_mark", "wizard_mark_range", "wizard_back"];

fn type_picker() -> Picker {
    let mut picker = Picker::new("wizard.type", "What do you want to review?", None);
    picker.set_footer("enter choose · esc back to reviews");
    picker
}

impl Wizard {
    /// A wizard for the repository at `repo`.
    #[must_use]
    pub fn new(services: ReviewServices, repo: PathBuf) -> Self {
        Self {
            services,
            repo,
            inbox: Inbox::default(),
            step: Step::Type,
            picker: type_picker(),
            state: None,
            state_error: None,
            state_requested: false,
            pulls: None,
            pulls_requested: false,
            listing: Listing::default(),
            marked: Vec::new(),
            anchor: None,
            sources: Vec::new(),
            editing: None,
            typed_title: None,
            chosen_type: None,
            focus_type: None,
            status: None,
            action_keys: BTreeMap::new(),
            pending_items: true,
            stack_limit_dismissed: None,
        }
    }

    /// A wizard editing `workspace`'s title and sources (opens at step 3).
    #[must_use]
    pub fn editing(
        services: ReviewServices,
        repo: PathBuf,
        workspace: bcode_code_review_models::ReviewWorkspace,
    ) -> Self {
        let mut wizard = Self::new(services, repo);
        wizard.sources = workspace
            .sources
            .iter()
            .map(|source| Chosen {
                kind: source.kind.clone(),
                label: source.label.clone(),
                id: Some(source.id.clone()),
                included: source.included,
            })
            .collect();
        wizard.typed_title = Some(workspace.title.clone());
        wizard.editing = Some(Box::new(workspace));
        wizard.enter_confirm();
        wizard
    }

    /// Whether the wizard edits an existing review's sources.
    #[must_use]
    pub const fn is_editing(&self) -> bool {
        self.editing.is_some()
    }

    /// Whether each chosen source is included, in order.
    #[must_use]
    pub fn inclusion(&self) -> Vec<bool> {
        self.sources.iter().map(|source| source.included).collect()
    }

    /// Remember the effective keys of the wizard's actions.
    pub fn prepare_keymap(&mut self, host: &UiContext) {
        self.action_keys = HINT_ACTIONS
            .iter()
            .filter_map(|action| {
                host.keymap_keys(WIZARD_SCOPE, &format!("bcode.code_review.{action}"))
                    .into_iter()
                    .next()
                    .map(|key| ((*action).to_owned(), key))
            })
            .collect();
    }

    /// The step shown: 1 (type), 2 (details), or 3 (title).
    #[must_use]
    pub const fn step_number(&self) -> usize {
        match self.step {
            Step::Type => 1,
            Step::Details(_) => 2,
            Step::Confirm => 3,
        }
    }

    /// The step's picker.
    #[must_use]
    pub const fn picker(&self) -> &Picker {
        &self.picker
    }

    /// Labels of the sources added so far.
    #[must_use]
    pub fn source_labels(&self) -> Vec<&str> {
        self.sources
            .iter()
            .map(|source| source.label.as_str())
            .collect()
    }

    /// Ids marked in the current listing (commit ids or paths).
    #[must_use]
    pub fn marked(&self) -> &[String] {
        &self.marked
    }

    /// Whether any listing or provider call is running.
    #[must_use]
    pub const fn loading(&self) -> bool {
        self.listing.loading || (self.pulls_requested && self.pulls.is_none())
    }

    /// Status line text.
    #[must_use]
    pub fn status(&self) -> Option<&str> {
        self.status.as_ref().map(|status| status.text.as_str())
    }

    // ----------------------------------------------------------------------------------------
    // Loading
    // ----------------------------------------------------------------------------------------

    fn spawn(
        &self,
        host: &UiContext,
        task: impl std::future::Future<Output = WizardMessage> + Send + 'static,
    ) {
        let inbox = self.inbox.clone();
        host.spawn(Box::pin(async move {
            inbox.push(task.await);
        }));
    }

    fn request_state(&mut self, host: &UiContext) {
        if self.state_requested {
            return;
        }
        self.state_requested = true;
        let (services, repo) = (self.services.clone(), self.repo.clone());
        self.spawn(host, async move {
            WizardMessage::State(super::home::load::state(services, repo).await)
        });
    }

    fn request_pulls(&mut self, host: &UiContext) {
        if self.pulls_requested {
            return;
        }
        self.pulls_requested = true;
        let (services, repo) = (self.services.clone(), self.repo.clone());
        self.spawn(host, async move {
            WizardMessage::Pulls(super::home::load::pulls(services, repo).await)
        });
    }

    /// Request the first page (or, with `append`, the next page) of the step's listing.
    fn request_listing(&mut self, host: &UiContext, append: bool) {
        let Step::Details(details) = &self.step else {
            return;
        };
        if self.listing.loading {
            if !append {
                self.listing.again = true;
            }
            return;
        }
        let cursor = if append {
            match self.listing.next.clone() {
                Some(cursor) => Some(cursor),
                None => return,
            }
        } else {
            None
        };
        self.listing.seq += 1;
        self.listing.loading = true;
        let seq = self.listing.seq;
        let (services, repo) = (self.services.clone(), self.repo.clone());
        let query = self.picker.query();
        match details {
            Details::Branch | Details::Compare { .. } => {
                let request = ReviewRefsSearchRequest {
                    repo_path: repo,
                    query,
                    kinds: vec![
                        ReviewRefKind::Branch,
                        ReviewRefKind::RemoteBranch,
                        ReviewRefKind::Tag,
                    ],
                    cursor,
                    limit: Some(PAGE),
                };
                self.spawn(host, async move {
                    WizardMessage::Refs {
                        seq,
                        append,
                        result: services.call(request).await.map_err(|error| error.message),
                    }
                });
            }
            Details::Commits => {
                let request = ReviewCommitsPageRequest {
                    repo_path: repo,
                    rev: None,
                    query: (!query.trim().is_empty()).then(|| query.trim().to_owned()),
                    cursor,
                    limit: Some(PAGE),
                };
                self.spawn(host, async move {
                    WizardMessage::Commits {
                        seq,
                        append,
                        result: services.call(request).await.map_err(|error| error.message),
                    }
                });
            }
            Details::Files => {
                let (query, _) = split_range(&query);
                let request = ReviewFilesSearchRequest {
                    repo_path: repo,
                    query: query.trim().to_owned(),
                    revision: None,
                    cursor,
                    limit: Some(PAGE),
                };
                self.spawn(host, async move {
                    WizardMessage::Files {
                        seq,
                        append,
                        result: services.call(request).await.map_err(|error| error.message),
                    }
                });
            }
            Details::WorkingTree | Details::Pull | Details::Stack(_) => {
                self.listing.loading = false;
            }
        }
    }

    fn request_stack(&mut self, host: &UiContext, number: u64) {
        let Some(Ok(Some(pulls))) = &self.pulls else {
            return;
        };
        let (services, route, repo, repository) = (
            self.services.clone(),
            pulls.route.clone(),
            self.repo.clone(),
            pulls.repository.clone(),
        );
        self.step = Step::Details(Details::Stack(StackStage::Resolving(number)));
        self.pending_items = true;
        self.spawn(host, async move {
            WizardMessage::Stack {
                number,
                result: super::home::load::stack(services, route, repo, repository, number).await,
            }
        });
    }

    /// Apply finished work and start what is due. Returns whether the wizard changed.
    pub fn poll(&mut self, host: &UiContext, theme: &ReviewTheme) -> bool {
        self.request_state(host);
        let mut changed = false;
        for message in self.inbox.drain() {
            changed = true;
            self.apply(message, host);
        }
        if self.pending_items {
            self.pending_items = false;
            self.refresh_items(theme);
            changed = true;
        }
        if self.listing.enter_pending && !self.listing_outdated() {
            // Picking in a details step only moves the wizard on (creating needs step 3).
            self.listing.enter_pending = false;
            let _ = self.key(
                KeyStroke::simple(bmux_keyboard::KeyCode::Enter),
                host,
                theme,
            );
            changed = true;
        }
        changed
    }

    /// Whether the step's listing does not answer the current query yet.
    const fn listing_outdated(&self) -> bool {
        matches!(
            self.step,
            Step::Details(
                Details::Branch | Details::Compare { .. } | Details::Commits | Details::Files
            )
        ) && (self.listing.loading || self.listing.again)
    }

    fn apply(&mut self, message: WizardMessage, host: &UiContext) {
        match message {
            WizardMessage::State(result) => {
                match result {
                    Ok(state) => {
                        let prefill = self.step == Step::Details(Details::Branch)
                            && self.picker.query().is_empty();
                        if prefill && let Some(base) = &state.base {
                            self.picker.set_query(base);
                            self.request_listing(host, false);
                        }
                        self.state = Some(state);
                    }
                    Err(error) => self.state_error = Some(error),
                }
                // Counts shown in the step's rows depend on the state.
                self.pending_items = true;
            }
            WizardMessage::Refs {
                seq,
                append,
                result,
            } => {
                if self.finish_listing(seq, host) {
                    match result {
                        Ok(response) => {
                            if !append {
                                self.listing.refs.clear();
                            }
                            self.listing.refs.extend(response.refs);
                            self.listing.next = response.next_cursor;
                            self.listing.error = None;
                        }
                        Err(error) => self.listing.error = Some(error),
                    }
                }
            }
            WizardMessage::Commits {
                seq,
                append,
                result,
            } => {
                if self.finish_listing(seq, host) {
                    match result {
                        Ok(response) => {
                            if !append {
                                self.listing.commits.clear();
                            }
                            self.listing.commits.extend(response.commits);
                            self.listing.next = response.next_cursor;
                            self.listing.error = None;
                        }
                        Err(error) => self.listing.error = Some(error),
                    }
                }
            }
            WizardMessage::Files {
                seq,
                append,
                result,
            } => {
                if self.finish_listing(seq, host) {
                    match result {
                        Ok(response) => {
                            if !append {
                                self.listing.files.clear();
                            }
                            self.listing
                                .files
                                .extend(response.files.into_iter().map(|file| file.path));
                            self.listing.next = response.next_cursor;
                            self.listing.error = None;
                        }
                        Err(error) => self.listing.error = Some(error),
                    }
                }
            }
            WizardMessage::Pulls(result) => self.pulls = Some(result),
            WizardMessage::Stack { number, result } => {
                if self.step == Step::Details(Details::Stack(StackStage::Resolving(number))) {
                    match result {
                        Ok(stack) => {
                            self.step =
                                Step::Details(Details::Stack(StackStage::Mode(Box::new(stack))));
                            self.replace_picker(
                                Picker::new(
                                    "wizard.stack.mode",
                                    "How do you want to review it?",
                                    None,
                                ),
                                "enter choose · esc back",
                            );
                        }
                        Err(error) => {
                            self.step = Step::Details(Details::Stack(StackStage::Pick));
                            self.set_status(error, StatusSeverity::Error);
                        }
                    }
                }
            }
        }
        self.pending_items = true;
    }

    /// Note a finished listing request; returns whether its answer is current.
    fn finish_listing(&mut self, seq: u64, host: &UiContext) -> bool {
        if seq != self.listing.seq {
            return false;
        }
        self.listing.loading = false;
        if std::mem::take(&mut self.listing.again) {
            self.request_listing(host, false);
            return false;
        }
        true
    }

    fn set_status(&mut self, text: impl Into<String>, severity: StatusSeverity) {
        self.status = Some(Status {
            text: text.into(),
            severity,
        });
    }

    // ----------------------------------------------------------------------------------------
    // Steps
    // ----------------------------------------------------------------------------------------

    fn replace_picker(&mut self, picker: Picker, footer: &str) {
        self.picker = picker;
        self.picker.set_footer(footer);
        self.pending_items = true;
    }

    fn enter_details(&mut self, kind: SourceType, host: &UiContext) {
        self.chosen_type = Some(kind);
        self.listing.reset();
        self.marked.clear();
        self.anchor = None;
        let mark = self
            .action_keys
            .get("wizard_mark")
            .cloned()
            .unwrap_or_default();
        let range = self
            .action_keys
            .get("wizard_mark_range")
            .cloned()
            .unwrap_or_default();
        let (details, picker, footer) = match kind {
            SourceType::WorkingTree => (
                Details::WorkingTree,
                Picker::new("wizard.worktree", "Which changes?", None),
                "enter choose · esc back".to_owned(),
            ),
            SourceType::CurrentBranch => {
                let mut picker = Picker::new(
                    "wizard.branch",
                    "Base of the current branch (merge-base)",
                    Some("base reference"),
                );
                if let Some(base) = self.state.as_ref().and_then(|state| state.base.clone()) {
                    picker.set_query(&base);
                }
                (
                    Details::Branch,
                    picker,
                    "enter choose · type to search references · esc back".to_owned(),
                )
            }
            SourceType::CompareBranches => (
                Details::Compare { base: None },
                Picker::new(
                    "wizard.compare",
                    "Base reference",
                    Some("search references"),
                ),
                "enter choose · esc back".to_owned(),
            ),
            SourceType::Commits => (
                Details::Commits,
                Picker::new("wizard.commits", "Commits", Some("search commit messages")),
                format!("enter review · {mark} mark · {range} mark range · esc back"),
            ),
            SourceType::PullRequest => {
                self.request_pulls(host);
                (
                    Details::Pull,
                    Picker::new("wizard.pulls", "Pull request", Some("filter pull requests")),
                    "enter choose · esc back".to_owned(),
                )
            }
            SourceType::Stack => {
                self.request_pulls(host);
                (
                    Details::Stack(StackStage::Pick),
                    Picker::new(
                        "wizard.stack",
                        "A pull request in the stack",
                        Some("filter pull requests"),
                    ),
                    "enter resolve its stack · esc back".to_owned(),
                )
            }
            SourceType::Files => (
                Details::Files,
                Picker::new(
                    "wizard.files",
                    "Files",
                    Some("search files · add :START-END for a line range"),
                ),
                format!("enter review · {mark} mark · esc back"),
            ),
        };
        self.step = Step::Details(details);
        self.replace_picker(picker, &footer);
        self.request_listing(host, false);
    }

    fn back(&mut self) -> WizardOutcome {
        match &self.step {
            Step::Type => {
                if self.sources.is_empty() {
                    return WizardOutcome::Cancel;
                }
                self.enter_confirm();
            }
            Step::Details(Details::Compare { base: Some(_) }) => {
                self.step = Step::Details(Details::Compare { base: None });
                let mut picker = Picker::new(
                    "wizard.compare",
                    "Base reference",
                    Some("search references"),
                );
                picker.set_query("");
                self.replace_picker(picker, "enter choose · esc back");
                self.listing.reset();
            }
            Step::Details(Details::Stack(StackStage::Mode(_) | StackStage::Resolving(_))) => {
                self.step = Step::Details(Details::Stack(StackStage::Pick));
                self.replace_picker(
                    Picker::new(
                        "wizard.stack",
                        "A pull request in the stack",
                        Some("filter pull requests"),
                    ),
                    "enter resolve its stack · esc back",
                );
            }
            Step::Details(_) => {
                self.step = Step::Type;
                self.replace_picker(type_picker(), "enter choose · esc back to reviews");
                // Back on the type that was being configured.
                self.focus_type = self.chosen_type;
            }
            Step::Confirm if self.editing.is_some() => return WizardOutcome::Cancel,
            Step::Confirm => {
                self.step = Step::Type;
                self.replace_picker(type_picker(), "enter choose · esc back");
            }
        }
        WizardOutcome::Redraw
    }

    /// Add sources and go to step 3. A source already chosen is not added twice.
    fn add_sources(&mut self, sources: Vec<(ReviewSourceKind, String)>) {
        let mut duplicates = 0_usize;
        for (kind, label) in sources {
            if self.sources.iter().any(|source| source.kind == kind) {
                duplicates += 1;
                continue;
            }
            self.sources.push(Chosen {
                kind,
                label,
                id: None,
                included: true,
            });
        }
        self.enter_confirm();
        if duplicates > 0 {
            self.set_status(
                if duplicates == 1 {
                    "that source is already included".to_owned()
                } else {
                    format!("{duplicates} sources are already included")
                },
                StatusSeverity::Info,
            );
        }
    }

    fn enter_confirm(&mut self) {
        let title = self
            .typed_title
            .clone()
            .unwrap_or_else(|| self.default_title());
        self.step = Step::Confirm;
        let mut picker = Picker::new("wizard.confirm", "Title", Some("review title"));
        picker.set_query(&title);
        let footer = if self.editing.is_some() {
            "enter choose · tab include or exclude · esc back to the review"
        } else {
            "enter choose · esc add or change sources"
        };
        self.replace_picker(picker, footer);
    }

    fn picker_title_or_default(&self) -> String {
        let text = self.picker.query();
        if text.trim().is_empty() {
            self.default_title()
        } else {
            text
        }
    }

    fn default_title(&self) -> String {
        if let [only] = self.sources.as_slice() {
            only.label.clone()
        } else {
            let sources: Vec<ReviewSource> = self
                .sources
                .iter()
                .map(|source| ReviewSource {
                    id: String::new(),
                    kind: source.kind.clone(),
                    label: source.label.clone(),
                    included: true,
                })
                .collect();
            default_title(&sources)
        }
    }

    // ----------------------------------------------------------------------------------------
    // Items
    // ----------------------------------------------------------------------------------------

    fn refresh_items(&mut self, theme: &ReviewTheme) {
        let (items, message) = self.items(theme);
        self.picker.set_items(items);
        if let Some(kind) = self.focus_type.take() {
            self.picker.focus_id(kind.id());
        }
        self.picker.set_message(message, theme);
    }

    #[allow(clippy::too_many_lines)]
    fn items(&self, theme: &ReviewTheme) -> (Vec<PickerItem>, Option<PickerMessage>) {
        let row = |title: String, detail: String| {
            Line::from_spans(vec![
                Span::styled(title, theme.text),
                Span::styled(format!("  {detail}"), theme.muted),
            ])
        };
        let message = |title: &str, body: String, error: bool| {
            Some(PickerMessage {
                title: title.to_owned(),
                body,
                error,
            })
        };
        let loading = || message("Loading…", String::new(), false);
        let listing_message = |empty: &str| {
            self.listing.error.as_ref().map_or_else(
                || {
                    if self.listing.loading {
                        loading()
                    } else {
                        message(empty, "Try another search.".to_owned(), false)
                    }
                },
                |error| message("Could not search", error.clone(), true),
            )
        };
        match &self.step {
            Step::Type => (
                SourceType::ALL
                    .iter()
                    .map(|kind| {
                        PickerItem::new(
                            kind.id(),
                            row(kind.label().to_owned(), kind.description().to_owned()),
                        )
                    })
                    .collect(),
                None,
            ),
            Step::Details(Details::WorkingTree) => {
                let pending = if self.state_error.is_some() {
                    "counts unavailable"
                } else {
                    "counting…"
                };
                let counts = |count: Option<bcode_code_review_models::ReviewChangeCount>| {
                    count.map_or_else(
                        || pending.to_owned(),
                        |count| {
                            if count.is_empty() {
                                "no changes".to_owned()
                            } else {
                                format!(
                                    "{} file{} · +{} {}{}",
                                    count.files,
                                    if count.files == 1 { "" } else { "s" },
                                    count.additions,
                                    theme.icons.minus,
                                    count.deletions
                                )
                            }
                        },
                    )
                };
                let state = self.state.as_ref();
                let unstaged = state.map(|state| state.unstaged);
                let staged = state.map(|state| state.staged);
                let both = state.map(|state| bcode_code_review_models::ReviewChangeCount {
                    files: state.unstaged.files + state.staged.files,
                    additions: state.unstaged.additions + state.staged.additions,
                    deletions: state.unstaged.deletions + state.staged.deletions,
                });
                (
                    vec![
                        PickerItem::new(
                            "unstaged",
                            row("Unstaged changes".into(), counts(unstaged)),
                        ),
                        PickerItem::new("staged", row("Staged changes".into(), counts(staged))),
                        PickerItem::new("both", row("Unstaged and staged".into(), counts(both))),
                    ],
                    None,
                )
            }
            Step::Details(Details::Branch | Details::Compare { .. }) => {
                let items: Vec<PickerItem> = self
                    .listing
                    .refs
                    .iter()
                    .map(|entry| {
                        let mut spans = vec![Span::styled(
                            if entry.is_head { "● " } else { "  " },
                            theme.accent,
                        )];
                        spans.push(Span::styled(entry.name.clone(), theme.text));
                        let kind = match entry.kind {
                            ReviewRefKind::Branch => "",
                            ReviewRefKind::RemoteBranch => "remote ",
                            ReviewRefKind::Tag => "tag ",
                        };
                        spans.push(Span::styled(
                            format!("  {kind}{}", entry.subject),
                            theme.muted,
                        ));
                        PickerItem::new(entry.name.clone(), Line::from_spans(spans))
                    })
                    .collect();
                let message = if items.is_empty() {
                    listing_message("No matching references")
                } else {
                    None
                };
                (items, message)
            }
            Step::Details(Details::Commits) => {
                let marked: BTreeSet<&str> = self.marked.iter().map(String::as_str).collect();
                let items: Vec<PickerItem> = self
                    .listing
                    .commits
                    .iter()
                    .map(|commit| {
                        let mark = marked.contains(commit.oid.as_str());
                        let mut spans = vec![
                            Span::styled(
                                if mark {
                                    format!("{} ", theme.icons.viewed)
                                } else {
                                    "  ".to_owned()
                                },
                                theme.viewed,
                            ),
                            Span::styled(format!("{} ", commit.short_oid), theme.accent),
                            Span::styled(commit.subject.clone(), theme.text),
                            Span::styled(format!("  {}", commit.author_name), theme.muted),
                        ];
                        if !commit.refs.is_empty() {
                            spans.push(Span::styled(
                                format!("  ({})", commit.refs.join(", ")),
                                theme.status_renamed,
                            ));
                        }
                        PickerItem::new(commit.oid.clone(), Line::from_spans(spans))
                    })
                    .collect();
                let message = if items.is_empty() {
                    listing_message("No matching commits")
                } else {
                    None
                };
                (items, message)
            }
            Step::Details(Details::Files) => {
                let marked: BTreeSet<&str> = self.marked.iter().map(String::as_str).collect();
                let items: Vec<PickerItem> = self
                    .listing
                    .files
                    .iter()
                    .map(|path| {
                        let mark = marked.contains(path.as_str());
                        PickerItem::new(
                            path.clone(),
                            Line::from_spans(vec![
                                Span::styled(
                                    if mark {
                                        format!("{} ", theme.icons.viewed)
                                    } else {
                                        "  ".to_owned()
                                    },
                                    theme.viewed,
                                ),
                                Span::styled(path.clone(), theme.path),
                            ]),
                        )
                    })
                    .collect();
                let message = if items.is_empty() {
                    listing_message("No matching files")
                } else {
                    None
                };
                (items, message)
            }
            Step::Details(Details::Pull | Details::Stack(StackStage::Pick)) => {
                self.pull_items(theme, matches!(self.step, Step::Details(Details::Stack(_))))
            }
            Step::Details(Details::Stack(StackStage::Resolving(number))) => (
                Vec::new(),
                message(
                    &format!("Resolving the stack of #{number}…"),
                    String::new(),
                    false,
                ),
            ),
            Step::Details(Details::Stack(StackStage::Mode(stack))) => {
                let limit = self.services.stack_limit();
                let mut items = Vec::new();
                if self.stack_limit_dismissed != Some(stack.requested)
                    && limit.warns_for(stack.layers.len())
                {
                    items.push(PickerItem::new(
                        STACK_LIMIT_WARNING_ID,
                        Line::from_spans(vec![
                            Span::styled(
                                format!(
                                    "{} layers, over the {}-layer limit; every layer is loaded",
                                    stack.layers.len(),
                                    limit.max_layers
                                ),
                                theme.notice,
                            ),
                            Span::styled("  enter to dismiss", theme.muted),
                        ]),
                    ));
                }
                items.extend([
                    PickerItem::new(
                        "whole",
                        row(
                            "Whole stack".into(),
                            format!(
                                "one review from {} to #{}, labeled by layer",
                                stack.trunk,
                                stack.layers.last().map_or(0, |layer| layer.pull.number)
                            ),
                        ),
                    ),
                    PickerItem::new(
                        "step",
                        row(
                            "Step through every layer".into(),
                            format!("{} layers, each against its parent", stack.layers.len()),
                        ),
                    ),
                    PickerItem::section(
                        "layers",
                        Line::from_spans(vec![Span::styled("One layer", theme.title)]),
                    ),
                ]);
                items.extend(stack.layers.iter().map(|layer| {
                    PickerItem::new(
                        format!("layer-{}", layer.pull.number),
                        row(
                            format!("#{} {}", layer.pull.number, layer.pull.title),
                            format!("{} ← {}", layer.pull.base_branch, layer.pull.head_branch),
                        ),
                    )
                }));
                for warning in &stack.warnings {
                    items.push(PickerItem::section(
                        format!("warning-{}", items.len()),
                        Line::from_spans(vec![Span::styled(warning.clone(), theme.notice)]),
                    ));
                }
                (items, None)
            }
            Step::Confirm => {
                let mut items = vec![
                    PickerItem::new(
                        "create",
                        Line::from_spans(vec![Span::styled(
                            if self.editing.is_some() {
                                "Save changes"
                            } else {
                                "Create review"
                            },
                            theme.accent.patch(theme.title),
                        )]),
                    ),
                    PickerItem::new(
                        "add",
                        row("Add another source…".into(), "composite review".into()),
                    ),
                    PickerItem::section(
                        "sources",
                        Line::from_spans(vec![Span::styled(
                            format!("Sources ({})", self.sources.len()),
                            theme.title,
                        )]),
                    ),
                ];
                let editing = self.editing.is_some();
                items.extend(self.sources.iter().enumerate().map(|(index, source)| {
                    let detail = match (editing, source.included) {
                        (false, _) => "enter removes".to_owned(),
                        (true, true) => "included · tab excludes · enter removes".to_owned(),
                        (true, false) => "excluded · tab includes · enter removes".to_owned(),
                    };
                    PickerItem::new(format!("source-{index}"), row(source.kind.label(), detail))
                }));
                (items, None)
            }
        }
    }

    /// The provider's pull requests, or the message shown instead of them.
    fn pulls_answer(&self) -> Result<&PullsAnswer, PickerMessage> {
        let message = |title: &str, body: String, error: bool| PickerMessage {
            title: title.to_owned(),
            body,
            error,
        };
        match &self.pulls {
            None => Err(message("Loading pull requests…", String::new(), false)),
            Some(Err(error)) => Err(message("Could not list pull requests", error.clone(), true)),
            Some(Ok(None)) => Err(message(
                "No pull request provider",
                NO_PROVIDER.to_owned(),
                false,
            )),
            Some(Ok(Some(answer))) => Ok(answer),
        }
    }

    fn pull_items(
        &self,
        theme: &ReviewTheme,
        stack: bool,
    ) -> (Vec<PickerItem>, Option<PickerMessage>) {
        let answer = match self.pulls_answer() {
            Ok(answer) => answer,
            Err(message) => return (Vec::new(), Some(message)),
        };
        if stack && !answer.route.manifest.capabilities.stack_resolve {
            return (
                Vec::new(),
                Some(PickerMessage {
                    title: "Stacks are not supported".to_owned(),
                    body: format!("{} cannot resolve stacks.", answer.route.manifest.label),
                    error: false,
                }),
            );
        }
        let words: Vec<String> = self
            .picker
            .query()
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let matches = |pull: &PullRequestSummary| {
            let haystack = format!(
                "#{} {} {} {} {}",
                pull.number,
                pull.title,
                pull.author.as_deref().unwrap_or_default(),
                pull.head_branch,
                pull.base_branch
            )
            .to_lowercase();
            words.iter().all(|word| haystack.contains(word.as_str()))
        };
        let item = |pull: &PullRequestSummary| pull_item(theme, pull);
        let branch = self.state.as_ref().and_then(|state| state.branch.clone());
        let mut sections: Vec<(&str, Vec<&PullRequestSummary>)> = Vec::new();
        let own: Vec<&PullRequestSummary> = answer
            .pulls
            .iter()
            .filter(|pull| stack && branch.as_deref() == Some(pull.head_branch.as_str()))
            .collect();
        let requested: Vec<&PullRequestSummary> = answer
            .pulls
            .iter()
            .filter(|pull| pull.review_requested_from_me && !own.contains(pull))
            .collect();
        let other: Vec<&PullRequestSummary> = answer
            .pulls
            .iter()
            .filter(|pull| !pull.review_requested_from_me && !own.contains(pull))
            .collect();
        sections.push(("Your branch", own));
        sections.push(("Review requested", requested));
        sections.push(("Open pull requests", other));
        let mut items = Vec::new();
        for (title, pulls) in sections {
            let pulls: Vec<&PullRequestSummary> =
                pulls.into_iter().filter(|pull| matches(pull)).collect();
            if pulls.is_empty() {
                continue;
            }
            items.push(PickerItem::section(
                title,
                Line::from_spans(vec![Span::styled(title, theme.title)]),
            ));
            items.extend(pulls.into_iter().map(item));
        }
        let message = items.is_empty().then(|| PickerMessage {
            title: "No matching pull requests".to_owned(),
            body: format!(
                "{} lists only open pull requests.",
                answer.route.manifest.label
            ),
            error: false,
        });
        (items, message)
    }

    // ----------------------------------------------------------------------------------------
    // Input
    // ----------------------------------------------------------------------------------------

    /// Handle a key.
    pub fn key(
        &mut self,
        stroke: KeyStroke,
        host: &UiContext,
        theme: &ReviewTheme,
    ) -> WizardOutcome {
        let before = self.status.clone();
        if stroke.key != bmux_keyboard::KeyCode::Enter {
            // Any other key replaces a deferred `enter`.
            self.listing.enter_pending = false;
        }
        let action = host
            .keymap_action(WIZARD_SCOPE, stroke)
            .map(|action| action.trim_start_matches("bcode.code_review.").to_owned());
        let outcome = match action.as_deref() {
            Some("wizard_back") => self.back(),
            Some("wizard_mark") if self.markable() => self.mark(false),
            Some("wizard_mark_range") if self.markable() => self.mark(true),
            _ if stroke.key == bmux_keyboard::KeyCode::Enter
                && stroke.modifiers.is_empty()
                && self.listing_outdated() =>
            {
                self.listing.enter_pending = true;
                WizardOutcome::Redraw
            }
            _ => {
                let outcome = self.picker.key(stroke);
                let typed = self.picker.query().trim().to_owned();
                let references = matches!(
                    self.step,
                    Step::Details(Details::Branch | Details::Compare { .. })
                );
                if outcome == PickerOutcome::Ignored
                    && stroke.key == bmux_keyboard::KeyCode::Enter
                    && references
                    && !typed.is_empty()
                {
                    // No listed reference matches: use the typed revision as it is.
                    self.picked(&typed, host)
                } else {
                    self.picker_outcome(outcome, host)
                }
            }
        };
        if before.is_some() && self.status == before && outcome != WizardOutcome::Ignored {
            self.status = None;
        }
        if self.pending_items {
            self.pending_items = false;
            self.refresh_items(theme);
        }
        outcome
    }

    /// Insert pasted text into the step's input.
    pub fn paste(&mut self, text: &str, host: &UiContext, theme: &ReviewTheme) -> WizardOutcome {
        let outcome = self.picker.paste(text);
        let outcome = self.picker_outcome(outcome, host);
        if self.pending_items {
            self.pending_items = false;
            self.refresh_items(theme);
        }
        outcome
    }

    /// Route a pointer event through the painted picker.
    pub fn mouse(&mut self, event: &Event, host: &UiContext, theme: &ReviewTheme) -> WizardOutcome {
        let outcome = self.picker.mouse(event, theme);
        let outcome = self.picker_outcome(outcome, host);
        if self.pending_items {
            self.pending_items = false;
            self.refresh_items(theme);
        }
        outcome
    }

    const fn markable(&self) -> bool {
        matches!(self.step, Step::Details(Details::Commits | Details::Files))
            || (matches!(self.step, Step::Confirm) && self.editing.is_some())
    }

    /// The edited sources: kept ones keep their ids; new ones get ids unused so far.
    fn edited_sources(&self, previous: &[ReviewSource]) -> Vec<ReviewSource> {
        let mut used: BTreeSet<String> = previous.iter().map(|source| source.id.clone()).collect();
        let mut next = 1_usize;
        self.sources
            .iter()
            .map(|source| {
                let id = source.id.clone().unwrap_or_else(|| {
                    while used.contains(&format!("source-{next}")) {
                        next += 1;
                    }
                    let id = format!("source-{next}");
                    used.insert(id.clone());
                    id
                });
                ReviewSource {
                    id,
                    kind: source.kind.clone(),
                    label: source.label.clone(),
                    included: source.included,
                }
            })
            .collect()
    }

    /// Toggle the focused item's mark, or (`range`) mark every item from the last mark to it.
    /// On step 3 of an edit, include or exclude the focused source.
    fn mark(&mut self, range: bool) -> WizardOutcome {
        let Some(focused) = self.picker.focused().map(str::to_owned) else {
            return WizardOutcome::Ignored;
        };
        if matches!(self.step, Step::Confirm) {
            if let Some(source) = focused
                .strip_prefix("source-")
                .and_then(|index| index.parse::<usize>().ok())
                .and_then(|index| self.sources.get_mut(index))
            {
                source.included = !source.included;
                self.pending_items = true;
                return WizardOutcome::Redraw;
            }
            return WizardOutcome::Ignored;
        }
        let order: Vec<&str> = self
            .picker
            .items()
            .iter()
            .filter(|item| !item.section)
            .map(|item| item.id.as_str())
            .collect();
        let anchor = self
            .anchor
            .as_deref()
            .and_then(|anchor| order.iter().position(|id| *id == anchor));
        match (range, anchor, order.iter().position(|id| *id == focused)) {
            (true, Some(start), Some(end)) => {
                let (low, high) = (start.min(end), start.max(end));
                for id in &order[low..=high] {
                    if !self.marked.iter().any(|marked| marked == id) {
                        self.marked.push((*id).to_owned());
                    }
                }
            }
            _ => {
                if let Some(position) = self.marked.iter().position(|id| *id == focused) {
                    self.marked.remove(position);
                } else {
                    self.marked.push(focused.clone());
                }
            }
        }
        self.anchor = Some(focused);
        self.pending_items = true;
        WizardOutcome::Redraw
    }

    fn picker_outcome(&mut self, outcome: PickerOutcome, host: &UiContext) -> WizardOutcome {
        match outcome {
            PickerOutcome::Ignored => WizardOutcome::Ignored,
            PickerOutcome::Redraw => WizardOutcome::Redraw,
            PickerOutcome::Cancel => self.back(),
            PickerOutcome::NearEnd => {
                if self.listing.next.is_some() {
                    self.request_listing(host, true);
                }
                WizardOutcome::Redraw
            }
            PickerOutcome::Edited => {
                if let Step::Details(
                    Details::Branch | Details::Compare { .. } | Details::Commits | Details::Files,
                ) = &self.step
                {
                    self.request_listing(host, false);
                }
                self.pending_items = true;
                WizardOutcome::Redraw
            }
            PickerOutcome::Picked(id) => self.picked(&id, host),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn picked(&mut self, id: &str, host: &UiContext) -> WizardOutcome {
        match self.step.clone() {
            Step::Type => {
                if let Some(kind) = SourceType::from_id(id) {
                    self.enter_details(kind, host);
                }
            }
            Step::Details(Details::WorkingTree) => {
                let (kind, title) = match id {
                    "unstaged" => (ReviewSourceKind::WorkingTreeUnstaged, "Unstaged changes"),
                    "staged" => (ReviewSourceKind::IndexStaged, "Staged changes"),
                    _ => (ReviewSourceKind::WorkingTreeAndIndex, "Uncommitted changes"),
                };
                self.add_sources(vec![(kind, title.to_owned())]);
            }
            Step::Details(Details::Branch) => {
                let detected = self.state.as_ref().and_then(|state| state.base.clone());
                let base = (Some(id.to_owned()) != detected).then(|| id.to_owned());
                let title = self
                    .state
                    .as_ref()
                    .and_then(|state| state.branch.clone())
                    .unwrap_or_else(|| "Current branch".to_owned());
                self.add_sources(vec![(ReviewSourceKind::CurrentBranch { base }, title)]);
            }
            Step::Details(Details::Compare { base: None }) => {
                self.step = Step::Details(Details::Compare {
                    base: Some(id.to_owned()),
                });
                self.listing.reset();
                let title = format!("Head reference (base {id})");
                self.replace_picker(
                    Picker::new("wizard.compare.head", title, Some("search references")),
                    "enter choose · esc change the base",
                );
                self.request_listing(host, false);
            }
            Step::Details(Details::Compare { base: Some(base) }) => {
                let title = format!("{base}...{id}");
                self.add_sources(vec![(
                    ReviewSourceKind::BranchCompare {
                        base_branch: base,
                        head_branch: id.to_owned(),
                        merge_base: true,
                    },
                    title,
                )]);
            }
            Step::Details(Details::Commits) => {
                let marked: BTreeSet<&str> = self.marked.iter().map(String::as_str).collect();
                // Oldest first, as the log lists them newest first.
                let mut revs: Vec<&ReviewCommitEntry> = self
                    .listing
                    .commits
                    .iter()
                    .filter(|commit| {
                        if marked.is_empty() {
                            commit.oid == id
                        } else {
                            marked.contains(commit.oid.as_str())
                        }
                    })
                    .collect();
                revs.reverse();
                let source = match revs.as_slice() {
                    [] => return WizardOutcome::Ignored,
                    [only] => (
                        ReviewSourceKind::Commit {
                            rev: only.oid.clone(),
                        },
                        format!("{} {}", only.short_oid, only.subject),
                    ),
                    [first, .., last] => (
                        ReviewSourceKind::Commits {
                            revs: revs.iter().map(|commit| commit.oid.clone()).collect(),
                        },
                        format!(
                            "{} commits {}..{}",
                            revs.len(),
                            first.short_oid,
                            last.short_oid
                        ),
                    ),
                };
                self.add_sources(vec![source]);
            }
            Step::Details(Details::Files) => {
                let (_, range) = split_range(&self.picker.query());
                let sources: Vec<(ReviewSourceKind, String)> = if self.marked.is_empty() {
                    vec![match range {
                        Some((start, end)) => (
                            ReviewSourceKind::FileRange {
                                path: id.to_owned(),
                                start,
                                end,
                            },
                            format!("{id}:{start}-{end}"),
                        ),
                        None => (
                            ReviewSourceKind::File {
                                path: id.to_owned(),
                            },
                            id.to_owned(),
                        ),
                    }]
                } else {
                    self.marked
                        .iter()
                        .map(|path| (ReviewSourceKind::File { path: path.clone() }, path.clone()))
                        .collect()
                };
                self.add_sources(sources);
            }
            Step::Details(Details::Pull) => {
                let Some(Ok(Some(answer))) = &self.pulls else {
                    return WizardOutcome::Ignored;
                };
                let Ok(number) = id.parse::<u64>() else {
                    return WizardOutcome::Ignored;
                };
                let title = answer
                    .pulls
                    .iter()
                    .find(|pull| pull.number == number)
                    .map_or_else(
                        || format!("#{number}"),
                        |pull| format!("#{number} {}", pull.title),
                    );
                let kind = ReviewSourceKind::PullRequest {
                    provider: answer.route.manifest.id.clone(),
                    repository: answer.repository.clone(),
                    number,
                };
                self.add_sources(vec![(kind, title)]);
            }
            Step::Details(Details::Stack(StackStage::Pick)) => {
                if let Ok(number) = id.parse::<u64>() {
                    self.request_stack(host, number);
                }
            }
            Step::Details(Details::Stack(StackStage::Resolving(_))) => {}
            Step::Details(Details::Stack(StackStage::Mode(stack))) => {
                if id == STACK_LIMIT_WARNING_ID {
                    self.stack_limit_dismissed = Some(stack.requested);
                    self.pending_items = true;
                    return WizardOutcome::Redraw;
                }
                let Some(Ok(Some(answer))) = &self.pulls else {
                    return WizardOutcome::Ignored;
                };
                let (provider, repository) =
                    (answer.route.manifest.id.clone(), answer.repository.clone());
                let layer = |number: u64, title: &str| {
                    (
                        ReviewSourceKind::StackLayer {
                            provider: provider.clone(),
                            repository: repository.clone(),
                            number,
                        },
                        format!("#{number} {title}"),
                    )
                };
                let sources = match id {
                    "whole" => {
                        let Some(top) = stack.layers.last() else {
                            return WizardOutcome::Ignored;
                        };
                        vec![(
                            ReviewSourceKind::Stack {
                                provider: provider.clone(),
                                repository: repository.clone(),
                                top: top.pull.number,
                            },
                            format!(
                                "Stack #{} · {} ({} layers)",
                                top.pull.number,
                                top.pull.title,
                                stack.layers.len()
                            ),
                        )]
                    }
                    "step" => stack
                        .layers
                        .iter()
                        .map(|entry| layer(entry.pull.number, &entry.pull.title))
                        .collect(),
                    other => {
                        let Some(entry) = other
                            .strip_prefix("layer-")
                            .and_then(|number| number.parse::<u64>().ok())
                            .and_then(|number| stack.layer(number))
                        else {
                            return WizardOutcome::Ignored;
                        };
                        vec![layer(entry.pull.number, &entry.pull.title)]
                    }
                };
                self.add_sources(sources);
            }
            Step::Confirm => match id {
                "create" => {
                    let title = self.picker_title_or_default().trim().to_owned();
                    if let Some(editing) = &self.editing {
                        if !self.sources.iter().any(|source| source.included) {
                            self.set_status("include at least one source", StatusSeverity::Warning);
                            return WizardOutcome::Redraw;
                        }
                        let mut workspace = editing.clone();
                        workspace.title = title;
                        workspace.sources = self.edited_sources(&workspace.sources);
                        return WizardOutcome::Update { workspace };
                    }
                    return WizardOutcome::Create {
                        title,
                        sources: self
                            .sources
                            .iter()
                            .map(|source| source.kind.clone())
                            .collect(),
                    };
                }
                "add" => {
                    // Keep a typed title for when the step returns.
                    let query = self.picker.query();
                    if query.trim() != self.default_title() && !query.trim().is_empty() {
                        self.typed_title = Some(query);
                    }
                    self.step = Step::Type;
                    self.replace_picker(type_picker(), "enter choose · esc back");
                }
                other => {
                    if let Some(index) = other
                        .strip_prefix("source-")
                        .and_then(|index| index.parse::<usize>().ok())
                        .filter(|index| *index < self.sources.len())
                    {
                        self.sources.remove(index);
                        if self.sources.is_empty() {
                            self.step = Step::Type;
                            self.replace_picker(
                                type_picker(),
                                "enter choose · esc back to reviews",
                            );
                        } else {
                            self.enter_confirm();
                        }
                    }
                }
            },
        }
        WizardOutcome::Redraw
    }

    // ----------------------------------------------------------------------------------------
    // Rendering
    // ----------------------------------------------------------------------------------------

    /// Footer hints of the current step.
    fn hints(&self) -> Vec<(String, String)> {
        let mut hints = vec![
            ("enter".to_owned(), "choose".to_owned()),
            ("↑↓".to_owned(), "move".to_owned()),
        ];
        if matches!(self.step, Step::Confirm) && self.markable() {
            if let Some(key) = self.action_keys.get("wizard_mark") {
                hints.push((key.clone(), "include or exclude".to_owned()));
            }
        } else if self.markable() {
            for (action, label) in [("wizard_mark", "mark"), ("wizard_mark_range", "mark range")] {
                if let Some(key) = self.action_keys.get(action) {
                    hints.push((key.clone(), label.to_owned()));
                }
            }
        }
        if let Some(key) = self.action_keys.get("wizard_back") {
            hints.push((key.clone(), "back".to_owned()));
        }
        hints
    }

    /// Paint the wizard over `area`.
    pub fn render(&mut self, area: Rect, cx: &mut PaintCx<'_, '_>, theme: &ReviewTheme) {
        cx.fill(
            LocalRect::new(area.x.into(), area.y.into(), area.width, area.height),
            " ",
            theme.canvas,
        );
        if area.width == 0 || area.height == 0 {
            return;
        }
        if self.pending_items {
            self.pending_items = false;
            self.refresh_items(theme);
        }
        let header = area.height >= 6;
        let footer = area.height >= 2;
        if header {
            let row = Rect::new(area.x, area.y, area.width, 1);
            cx.fill(
                LocalRect::new(row.x.into(), row.y.into(), row.width, 1),
                " ",
                theme.header_bar,
            );
            let title = Line::from_spans(vec![
                Span::raw(" "),
                Span::styled(
                    if self.editing.is_some() {
                        "Edit sources"
                    } else {
                        "New review"
                    },
                    theme.title,
                ),
                Span::styled(
                    if self.sources.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "{}{} source{} so far",
                            theme.icons.separator,
                            self.sources.len(),
                            if self.sources.len() == 1 { "" } else { "s" }
                        )
                    },
                    theme.muted,
                ),
            ])
            .truncate(usize::from(area.width));
            cx.with_style(theme.header_bar, |cx| {
                cx.write_line(
                    LocalRect::new(row.x.into(), row.y.into(), row.width, 1),
                    &title,
                );
            });
            let status = |step: usize| match step.cmp(&self.step_number()) {
                std::cmp::Ordering::Less => StepStatus::Complete,
                std::cmp::Ordering::Equal => StepStatus::Current,
                std::cmp::Ordering::Greater => StepStatus::Pending,
            };
            let steps = [
                StepItem::new("type", "Source type").status(status(1)),
                StepItem::new("details", "Details").status(status(2)),
                StepItem::new(
                    "title",
                    if self.editing.is_some() {
                        "Title & save"
                    } else {
                        "Title & create"
                    },
                )
                .status(status(3)),
            ];
            let stepper = StepperComponent::new("wizard.steps", &steps)
                .policy(StepperPolicy::horizontal())
                .styles(theme.component.stepper_styles());
            cx.paint_component(
                &stepper,
                Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), 1),
            );
        }
        let top = area.y + if header { 3 } else { 0 };
        let bottom = area.bottom() - u16::from(footer);
        let body = Rect::new(area.x, top, area.width, bottom.saturating_sub(top));
        self.picker.paint(theme, body, cx);
        if footer {
            let hints = self.hints();
            chrome::paint_footer(
                &hints,
                self.status.as_ref(),
                theme,
                Rect::new(area.x, area.bottom() - 1, area.width, 1),
                cx,
            );
        }
    }
}

/// One pull request row: number, title, and author, branches, and draft state.
fn pull_item(theme: &ReviewTheme, pull: &PullRequestSummary) -> PickerItem {
    let mut spans = vec![
        Span::styled(format!("#{} ", pull.number), theme.accent),
        Span::styled(pull.title.clone(), theme.text),
    ];
    let mut detail = Vec::new();
    if let Some(author) = &pull.author {
        detail.push(author.clone());
    }
    detail.push(format!("{} ← {}", pull.base_branch, pull.head_branch));
    if pull.draft {
        detail.push("draft".to_owned());
    }
    spans.push(Span::styled(
        format!("  {}", detail.join(theme.icons.separator)),
        theme.muted,
    ));
    PickerItem::new(pull.number.to_string(), Line::from_spans(spans))
}

/// Split a trailing `:START-END` line range off a file query.
fn split_range(query: &str) -> (&str, Option<(u32, u32)>) {
    let Some((path, range)) = query.rsplit_once(':') else {
        return (query, None);
    };
    let Some((start, end)) = range.split_once('-') else {
        return (query, None);
    };
    match (start.trim().parse::<u32>(), end.trim().parse::<u32>()) {
        (Ok(start), Ok(end)) if start > 0 && end >= start => (path, Some((start, end))),
        _ => (query, None),
    }
}

#[cfg(test)]
mod tests {
    use super::{SourceType, Wizard, split_range};
    use crate::testing::backend::TestBackend;
    use crate::testing::fixture::{FixtureSpec, GitFixture};
    use crate::testing::harness::ScriptedHost;
    use std::sync::Arc;

    #[test]
    fn answers_to_an_earlier_steps_listing_are_dropped() {
        let fixture = GitFixture::generate(FixtureSpec::small());
        let scripted = Arc::new(ScriptedHost::new());
        let host = scripted.context();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let mut wizard = Wizard::new(
            TestBackend::default().services(),
            fixture.root().to_path_buf(),
        );
        // The commit log is requested, then the reviewer moves on to files before it answers.
        wizard.enter_details(SourceType::Commits, &host);
        wizard.enter_details(SourceType::Files, &host);
        scripted.run_until_idle(&runtime);
        let theme = crate::theme::ReviewTheme::derive(None, crate::theme::IconStyle::default());
        assert!(wizard.poll(&host, &theme));
        assert!(
            wizard.listing.commits.is_empty(),
            "the commit log answer belongs to the previous step"
        );
        assert_ne!(wizard.listing.files.len(), 0);
        assert!(!wizard.listing.loading);
    }

    #[test]
    fn file_queries_split_off_line_ranges() {
        assert_eq!(
            split_range("src/lib.rs:10-20"),
            ("src/lib.rs", Some((10, 20)))
        );
        assert_eq!(split_range("lib:3-3"), ("lib", Some((3, 3))));
        assert_eq!(split_range("lib:9-3"), ("lib:9-3", None));
        assert_eq!(split_range("lib:0-3"), ("lib:0-3", None));
        assert_eq!(split_range("lib.rs"), ("lib.rs", None));
        assert_eq!(split_range("a:b-c"), ("a:b-c", None));
    }
}
